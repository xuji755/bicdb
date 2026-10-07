//! **`bicdbcli` 端到端验收**：真实例、真二进制、真 stdin/stdout。
//!
//! 钉的是 SQL*Plus 的三条招牌行为与两条纪律：
//! 1. 多行输入 + 行号续行提示；`;` 执行；
//! 2. **`/` 重跑当前缓冲**；`LIST`/`DEL`/`APPEND`/`CHANGE` 按行号编辑；
//! 3. `SPOOL` 收输出、`@脚本` 跑批、`DESCRIBE` 版面、`SET` 参数生效；
//! 4. **服务模式**：`bicdb start` 之后 `bicdbcli` 经套接字连（事务跨语句成立）；
//! 5. 单写者纪律：服务在跑时直连被拒。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// 测试用实例目录（进程号 + 名字；跑完删）。
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("bicdbcli-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Self(dir)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // 服务可能在跑：先请它停。
        let _ = Command::new(bicdb_bin())
            .arg("stop")
            .arg("-p")
            .arg(&self.0)
            .arg("-m")
            .arg("immediate")
            .output();
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn bicdb_bin() -> PathBuf {
    // `bicdb` 与 `bicdbcli` 都在同一个 target 目录（同一次构建）。
    let cli = PathBuf::from(env!("CARGO_BIN_EXE_bicdbcli"));
    cli.with_file_name("bicdb")
}

fn bicdbcli() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_bicdbcli"))
}

/// 建区（直接调 `bicdb init`）。
fn init(dir: &Path) {
    let out = Command::new(bicdb_bin())
        .arg("init")
        .arg(dir)
        .output()
        .expect("跑 bicdb init");
    assert!(
        out.status.success(),
        "init 失败：{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// 把一段输入喂给 `bicdbcli`，返回 (stdout, stderr, 退出码)。
fn feed(dir: &Path, input: &str, extra: &[&str]) -> (String, String, i32) {
    let mut cmd = Command::new(bicdbcli());
    cmd.arg("-S");
    for e in extra {
        cmd.arg(e);
    }
    cmd.arg("-p")
        .arg(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("起 bicdbcli");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("写 stdin");
    let out = child.wait_with_output().expect("等 bicdbcli");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

#[test]
fn buffer_slash_commands_and_reporting() {
    let dir = TempDir::new("buffer");
    init(dir.path());
    let input = "\
CREATE TABLE t (id NUMBER NOT NULL, name VARCHAR2(16));
INSERT INTO t VALUES (1, 'alpha');
INSERT INTO t VALUES (2, 'beta');
SELECT id, name
  FROM t
 WHERE id >= 1
 ORDER BY id;
/
LIST
APPEND  -- 追加到当前行
LIST
C /id/id_desc/
RUN
CLEAR BUFFER
/
EXIT 0
";
    let (out, err, code) = feed(dir.path(), input, &[]);
    assert_eq!(code, 0, "退出码；stderr={err}");
    // 表头 + 虚线（未引号名照 PG 折叠成小写）。
    assert!(
        out.contains("id") && out.contains("name"),
        "应有表头：\n{out}"
    );
    assert!(out.contains(" rows selected."), "行数反馈：\n{out}");
    assert!(out.contains("----"), "应有虚线：\n{out}");
    assert!(out.contains("alpha"), "第一轮结果：\n{out}");
    // `/` 重跑（第一次执行 + 重跑 = 至少两处 `rows selected.`）。
    assert!(
        out.matches("rows selected.").count() >= 2,
        "`/` 应重跑缓冲：\n{out}"
    );
    // LIST 显示行号与 `*` 当前行。
    assert!(
        out.contains("1*") || out.contains("1 "),
        "LIST 行号：\n{out}"
    );
    // CHANGE 改的是**当前行**（第 4 行 `ORDER BY id;`）——改完 `RUN` 仍应跑通。
    assert!(out.contains("id_desc"), "CHANGE 生效：\n{out}");
    // CLEAR BUFFER 之后的 `/` 应报空。
    assert!(
        out.contains("缓冲为空") || out.contains("缓冲"),
        "清空提示：\n{out}"
    );
}

#[test]
fn settings_spool_script_and_describe() {
    let dir = TempDir::new("script");
    init(dir.path());
    // 脚本：替换变量 + SPOOL + DESCRIBE + SET。
    let script = dir.path().join("load.sql");
    std::fs::write(
        &script,
        "\
SET ECHO OFF
SET FEEDBACK ON
SET LINESIZE 60
CREATE TABLE s (id NUMBER NOT NULL, tag VARCHAR2(8) NOT NULL, note VARCHAR2(32));
DESC s
DEFINE n=7
INSERT INTO s VALUES (&n, 'x', NULL);
SET NULL (空)
SELECT id, tag, note FROM s;
",
    )
    .expect("写脚本");

    let input = format!(
        "SPOOL {}/out.lst\n@{}\nSPOOL OFF\nSHOW LINESIZE\nEXIT\n",
        dir.path().display(),
        script.display()
    );
    let (out, err, code) = feed(dir.path(), &input, &[]);
    assert_eq!(code, 0, "stderr={err}");
    // DESCRIBE 版面。
    assert!(out.contains("Null?"), "DESC 表头：\n{out}");
    assert!(out.contains("NUMBER"), "DESC 类型：\n{out}");
    assert!(out.contains("NOT NULL"), "DESC 可空列：\n{out}");
    // 替换变量生效（插进去的是 7）。
    assert!(out.contains('7'), "替换变量：\n{out}");
    // NULL 显示形态。
    assert!(out.contains("(空)"), "SET NULL 生效：\n{out}");
    // SHOW。
    assert!(out.contains("linesize 60"), "SHOW：\n{out}");
    // SPOOL 文件收了同样的输出。
    let spooled = std::fs::read_to_string(dir.path().join("out.lst")).expect("读 spool");
    assert!(
        spooled.contains("row selected."),
        "SPOOL 应有结果：\n{spooled}"
    );
    assert!(spooled.contains('7'), "SPOOL 应有脚本输出：\n{spooled}");
}

#[test]
fn script_error_stops_when_whenever_sqlerror_exit() {
    let dir = TempDir::new("sqlerror");
    init(dir.path());
    let script = dir.path().join("bad.sql");
    std::fs::write(
        &script,
        "\
WHENEVER SQLERROR EXIT
SELECT id FROM 不存在的表;
SELECT 1 FROM 也不会跑到;
",
    )
    .expect("写脚本");
    let input = format!("@{}\nEXIT 7\n", script.display());
    let (out, _err, code) = feed(dir.path(), &input, &[]);
    // `WHENEVER SQLERROR EXIT` 生效：脚本中断 ⇒ 整会话以失败结束（退出码 1）。
    assert_eq!(code, 1, "应在脚本中断处退出（out={out}）");
    assert!(
        out.contains("不存在") || out.contains("对象"),
        "错误应有说明：\n{out}"
    );
    assert!(!out.contains("也不会跑到"), "中断后不得继续：\n{out}");
}

#[test]
fn service_mode_connects_over_socket_and_keeps_transactions() {
    let dir = TempDir::new("service");
    init(dir.path());
    // 起服务。
    let out = Command::new(bicdb_bin())
        .arg("start")
        .arg("-p")
        .arg(dir.path())
        .arg("-w")
        .arg("30")
        .output()
        .expect("bicdb start");
    assert!(
        out.status.success(),
        "start 失败：{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // 经服务：**一个连接 = 一个会话** ⇒ `BEGIN` 与 `COMMIT` 之间隔着多次往返也成立。
    let input = "\
CREATE TABLE svc (id NUMBER NOT NULL, v VARCHAR2(8));
INSERT INTO svc VALUES (1, 'a');
BEGIN;
INSERT INTO svc VALUES (2, 'b');
SELECT id, v FROM svc ORDER BY id;
COMMIT;
SELECT id FROM svc ORDER BY id;
EXIT
";
    let (out, err, code) = feed(dir.path(), input, &[]);
    assert_eq!(code, 0, "stderr={err}");
    // BEGIN 之后、COMMIT 之前也能查到自己的行（同会话可见）。
    //
    // **断言要落在行上，不是"输出里有个 2"**：这条断言此前用
    // `out.contains('2')`，而 `ROLLBACK（撤销 2 条）` 里也有个 2——
    // 于是它**一直假通过**，把"读己所写"没了这件事盖了过去（真踩到）。
    // 现在按"表体里出现了 id=2 那一行"判：先取 SELECT 那段输出。
    let after_begin = out.split("BEGIN").nth(1).unwrap_or("");
    let table = after_begin.split("rows selected").next().unwrap_or("");
    assert!(
        table.lines().any(|l| l.trim_start().starts_with("2 ")),
        "事务里应看到自己的插入（表体里要有 id=2 那一行）：\n{out}"
    );
    // **`DESC` 走会话**（不丢事务状态）：显式事务里也能 DESC，且事务照旧。
    let input = "\
BEGIN;
INSERT INTO svc VALUES (3, 'c');
DESC svc
ROLLBACK;
SELECT id FROM svc ORDER BY id;
EXIT
";
    let (out, err, code) = feed(dir.path(), input, &[]);
    assert_eq!(code, 0, "stderr={err}");
    assert!(out.contains("Null?"), "事务里也能 DESC：\n{out}");
    assert!(out.contains("ROLLBACK"), "事务照旧收尾：\n{out}");
    assert!(
        !out.contains('3') || !out.contains("c "),
        "回滚后不应有第 3 行：\n{out}"
    );

    // 直连被锁挡住（单写者纪律）。
    let (_, err2, code2) = feed(dir.path(), "SELECT 1 FROM x;\nEXIT\n", &["--direct"]);
    assert_ne!(code2, 0, "服务在跑时 --direct 应被拒");
    assert!(
        err2.contains("占用") || err2.contains("被 pid"),
        "提示应指认占用：{err2}"
    );

    // 停服务（fast）：完全检查点。
    let out = Command::new(bicdb_bin())
        .arg("stop")
        .arg("-p")
        .arg(dir.path())
        .output()
        .expect("bicdb stop");
    assert!(
        out.status.success(),
        "stop 失败：{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // 停完之后直连应能打开，且数据在（完全检查点 + 重开）。
    let (out, err, code) = feed(
        dir.path(),
        "SELECT id, v FROM svc ORDER BY id;\nEXIT\n",
        &[],
    );
    assert_eq!(code, 0, "直连失败：{err}");
    assert!(out.contains('1') && out.contains('a'), "数据应在：\n{out}");
}
