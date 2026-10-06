//! **服务生命周期验收**（`bicdb start|stop|status|restart`）。
//!
//! 钉的是 PG `pg_ctl` 那套纪律：
//! 1. `start` 起分离进程、**等就绪**（不是"起来了就返回"）；
//! 2. 同一实例**不许起第二个**（实例锁；直连也被挡）；
//! 3. `stop` 走控制套接字（`fast` = 完全检查点；`immediate` = 下次打开走恢复）；
//! 4. 陈旧锁（上次被 `kill -9`）能被**自动接管**，不卡住启动；
//! 5. 服务在跑时 `bicdb sql` 自动经套接字（同一条 SQL 路径）。

use std::path::{Path, PathBuf};
use std::process::Command;

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("bicdb-svc-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Self(dir)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = Command::new(bin())
            .arg("stop")
            .arg(&self.0)
            .arg("-m")
            .arg("immediate")
            .output();
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_bicdb"))
}

fn run(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(bin()).args(args).output().expect("跑 bicdb");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn init(dir: &Path) {
    let (code, _, err) = run(&["init", &dir.display().to_string()]);
    assert_eq!(code, 0, "init：{err}");
}

fn start(dir: &Path) -> (i32, String, String) {
    let d = dir.display().to_string();
    run(&["start", &d, "-w", "30"])
}

fn stop(dir: &Path, mode: &str) -> (i32, String, String) {
    let d = dir.display().to_string();
    run(&["stop", &d, "-m", mode])
}

#[test]
fn lifecycle_start_status_stop_restart() {
    let dir = TempDir::new("lifecycle");
    init(dir.path());
    let d = dir.path().display().to_string();

    // 未运行。
    let (code, out, _) = run(&["status", &d]);
    assert_eq!(code, 0);
    assert!(out.contains("未运行"), "{out}");

    // start → 就绪。
    let (code, out, err) = start(dir.path());
    assert_eq!(code, 0, "start：{out}{err}");
    assert!(out.contains("服务已启动"), "{out}");

    // status：在跑 + 服务模式。
    let (_, out, _) = run(&["status", &d]);
    assert!(out.contains("运行中"), "{out}");
    assert!(out.contains("服务模式"), "{out}");

    // 第二个 start 被拒（实例锁）。
    let (code, _out, err) = start(dir.path());
    assert_ne!(code, 0, "第二个 start 应被拒");
    assert!(err.contains("占用"), "{err}");

    // 经服务执行 SQL（`bicdb sql` 自动选路）。
    let (code, out, err) = run(&[
        "sql",
        &d,
        "CREATE TABLE t (id NUMBER NOT NULL, v VARCHAR2(8))",
    ]);
    assert_eq!(code, 0, "建表：{out}{err}");
    let (code, out, _) = run(&["sql", &d, "INSERT INTO t VALUES (1,'a')"]);
    assert_eq!(code, 0, "插入：{out}");
    let (_, out, _) = run(&["sql", &d, "SELECT id, v FROM t"]);
    assert!(out.contains('a'), "经服务查询：{out}");

    // restart：停（fast）+ 起。
    let (code, out, err) = run(&["restart", &d, "-w", "30"]);
    assert_eq!(code, 0, "restart：{out}{err}");
    let (_, out, _) = run(&["status", &d]);
    assert!(out.contains("运行中"), "restart 后应在跑：{out}");

    // stop（fast）：完全检查点。
    let (code, out, err) = stop(dir.path(), "fast");
    assert_eq!(code, 0, "stop：{out}{err}");
    assert!(out.contains("服务已停止"), "{out}");
    let (_, out, _) = run(&["status", &d]);
    assert!(out.contains("未运行"), "停后状态：{out}");

    // 停后直连：数据在（完全检查点 + 重开）。
    let (code, out, err) = run(&["sql", &d, "SELECT id, v FROM t"]);
    assert_eq!(code, 0, "直连：{out}{err}");
    assert!(out.contains('a'), "数据应在：{out}");
}

#[test]
fn immediate_stop_recovers_on_next_open() {
    let dir = TempDir::new("immediate");
    init(dir.path());
    let d = dir.path().display().to_string();
    let (code, out, err) = start(dir.path());
    assert_eq!(code, 0, "start：{out}{err}");
    assert_eq!(
        run(&["sql", &d, "CREATE TABLE i (id NUMBER NOT NULL)"]).0,
        0
    );
    for k in 1..=20 {
        assert_eq!(
            run(&["sql", &d, &format!("INSERT INTO i VALUES ({k})")]).0,
            0
        );
    }
    // immediate：不做完全检查点就退。
    let (code, out, err) = stop(dir.path(), "immediate");
    assert_eq!(code, 0, "immediate stop：{out}{err}");

    // 重开（直连）：三阶段恢复把 20 行重做出来。
    let (code, out, err) = run(&["sql", &d, "SELECT id FROM i"]);
    assert_eq!(code, 0, "重开：{out}{err}");
    assert!(
        out.contains("20 行") || out.contains("（20"),
        "应恢复 20 行：{out}"
    );
}

#[test]
fn a_stale_lock_is_taken_over() {
    let dir = TempDir::new("stale");
    init(dir.path());
    // 手工造一把陈旧锁：pid 用一个**不可能活着**的号（2^31-1）。
    let pid_file = dir.path().join("bicdb.pid");
    std::fs::write(
        &pid_file,
        "2147483647\n1\nservice\n/tmp/nowhere.sock\n0.0.0\n",
    )
    .expect("写陈锁");
    let (code, out, err) = start(dir.path());
    assert_eq!(code, 0, "陈旧锁应被接管：{out}{err}");
    let _ = stop(dir.path(), "fast");
}

#[test]
fn direct_holder_blocks_the_service() {
    let dir = TempDir::new("direct");
    init(dir.path());
    let d = dir.path().display().to_string();
    // 先起服务（拿到 Service 锁），再验证直连被挡（`sql` 会自动走套接字，
    // 所以这里用 `--direct` 走不了的路径：直接开 shell 读 EOF）。
    let (code, out, err) = start(dir.path());
    assert_eq!(code, 0, "start：{out}{err}");
    let out = Command::new(bin())
        .arg("shell")
        .arg(&d)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("跑 shell");
    assert!(!out.status.success(), "服务在跑时直连应被拒");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("占用"), "提示应指认占用：{err}");
    let _ = stop(dir.path(), "fast");
}
