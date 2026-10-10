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
use std::time::{Duration, Instant};

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

/// `-p <根区目录>`：按参数文件寻址（照 Oracle 的口径）。
fn p_arg(dir: &Path) -> String {
    dir.display().to_string()
}

fn start(dir: &Path) -> (i32, String, String) {
    let d = p_arg(dir);
    run(&["start", "-p", &d, "-w", "30"])
}

fn stop(dir: &Path, mode: &str) -> (i32, String, String) {
    let d = p_arg(dir);
    run(&["stop", "-p", &d, "-m", mode])
}

#[test]
fn lifecycle_start_status_stop_restart() {
    let dir = TempDir::new("lifecycle");
    init(dir.path());
    let d = dir.path().display().to_string();

    // 未运行。
    let (code, out, _) = run(&["status", "-p", &d]);
    assert_eq!(code, 0);
    assert!(out.contains("未运行"), "{out}");

    // start → 就绪。
    let (code, out, err) = start(dir.path());
    assert_eq!(code, 0, "start：{out}{err}");
    assert!(out.contains("服务已启动"), "{out}");

    // status：在跑 + 服务模式。
    let (_, out, _) = run(&["status", "-p", &d]);
    assert!(out.contains("运行中"), "{out}");
    assert!(out.contains("服务模式"), "{out}");
    assert!(out.contains("DB Cache 2048 MiB"), "{out}");

    // 第二个 start 被拒（实例锁）。
    let (code, _out, err) = start(dir.path());
    assert_ne!(code, 0, "第二个 start 应被拒");
    assert!(err.contains("占用"), "{err}");

    // 经服务执行 SQL（`bicdb sql` 自动选路）。
    let (code, out, err) = run(&[
        "sql",
        "-p",
        &d,
        "CREATE TABLE t (id NUMBER NOT NULL, v VARCHAR2(8))",
    ]);
    assert_eq!(code, 0, "建表：{out}{err}");
    let (code, out, _) = run(&["sql", "-p", &d, "INSERT INTO t VALUES (1,'a')"]);
    assert_eq!(code, 0, "插入：{out}");
    let (_, out, _) = run(&["sql", "-p", &d, "SELECT id, v FROM t"]);
    assert!(out.contains('a'), "经服务查询：{out}");

    // restart：停（fast）+ 起。
    let (code, out, err) = run(&["restart", "-p", &d, "-w", "30"]);
    assert_eq!(code, 0, "restart：{out}{err}");
    let (_, out, _) = run(&["status", "-p", &d]);
    assert!(out.contains("运行中"), "restart 后应在跑：{out}");

    // stop（fast）：完全检查点。
    let (code, out, err) = stop(dir.path(), "fast");
    assert_eq!(code, 0, "stop：{out}{err}");
    assert!(out.contains("服务已停止"), "{out}");
    let (_, out, _) = run(&["status", "-p", &d]);
    assert!(out.contains("未运行"), "停后状态：{out}");

    // 停后直连：数据在（完全检查点 + 重开）。
    let (code, out, err) = run(&["sql", "-p", &d, "SELECT id, v FROM t"]);
    assert_eq!(code, 0, "直连：{out}{err}");
    assert!(out.contains('a'), "数据应在：{out}");
}

#[test]
fn blocked_sql_does_not_block_control_plane_or_other_workers() {
    let dir = TempDir::new("concurrent-service");
    init(dir.path());
    let (code, out, err) = start(dir.path());
    assert_eq!(code, 0, "start: {out}{err}");
    let d = dir.path().display().to_string();
    assert_eq!(
        run(&[
            "sql",
            "-p",
            &d,
            "CREATE TABLE queue_probe (id NUMBER NOT NULL, value NUMBER NOT NULL); INSERT INTO queue_probe VALUES (1,0)",
        ])
        .0,
        0
    );

    let socket = dir.path().join("bicdb.sock");
    let mut holder = bicdb_net::Client::connect(&socket).unwrap();
    holder.sql("BEGIN", &[]).unwrap();
    holder
        .sql("UPDATE queue_probe SET value=1 WHERE id=1", &[])
        .unwrap();

    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let waiter_socket = socket.clone();
        let waiter = scope.spawn(move || {
            let mut client = bicdb_net::Client::connect(&waiter_socket).unwrap();
            entered_tx.send(()).unwrap();
            client
                .sql("UPDATE queue_probe SET value=2 WHERE id=1", &[])
                .unwrap();
        });
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        std::thread::sleep(Duration::from_millis(100));

        let began = Instant::now();
        let (code, status, err) = run(&["status", "-p", &d]);
        assert_eq!(code, 0, "status: {status}{err}");
        assert!(began.elapsed() < Duration::from_secs(2), "{status}");
        assert!(status.contains("运行中"), "{status}");

        let began = Instant::now();
        let (code, selected, err) = run(&[
            "sql",
            "-p",
            &d,
            "SELECT id, value FROM queue_probe WHERE id=1",
        ]);
        assert_eq!(code, 0, "select: {selected}{err}");
        assert!(began.elapsed() < Duration::from_secs(2), "{selected}");

        holder.sql("COMMIT", &[]).unwrap();
        waiter.join().unwrap();
    });
    let _ = stop(dir.path(), "fast");
}

#[test]
fn immediate_stop_recovers_on_next_open() {
    let dir = TempDir::new("immediate");
    init(dir.path());
    let d = dir.path().display().to_string();
    let (code, out, err) = start(dir.path());
    assert_eq!(code, 0, "start：{out}{err}");
    assert_eq!(
        run(&["sql", "-p", &d, "CREATE TABLE i (id NUMBER NOT NULL)"]).0,
        0
    );
    for k in 1..=20 {
        assert_eq!(
            run(&["sql", "-p", &d, &format!("INSERT INTO i VALUES ({k})")]).0,
            0
        );
    }
    // immediate：不做完全检查点就退。
    let (code, out, err) = stop(dir.path(), "immediate");
    assert_eq!(code, 0, "immediate stop：{out}{err}");

    // 重开（直连）：三阶段恢复把 20 行重做出来。
    let (code, out, err) = run(&["sql", "-p", &d, "SELECT id FROM i"]);
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

/// **参数文件是活的**：`init` 写默认；改文件重启生效；`-c` 覆盖文件；
/// 未知键**具名拒绝**（闭集）。
#[test]
fn parameter_file_drives_the_service() {
    let dir = TempDir::new("conf");
    init(dir.path());
    let d = dir.path().display().to_string();
    let conf = dir.path().join("bicdb.ini");
    assert!(conf.exists(), "init 应写出 bicdb.ini（默认参数文件）");

    // 改文件：池 64 帧 + 等锁 7 ms。
    let text = std::fs::read_to_string(&conf).expect("读");
    let text = text
        .lines()
        .map(|line| {
            if line.trim_start().starts_with("pool_frames ") {
                "pool_frames = 131072"
            } else if line.trim_start().starts_with("park_ms ") {
                "park_ms = 7"
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(&conf, text).expect("写");

    // `params` 应报"文件"来源与文件里的值。
    let (code, out, err) = run(&["params", "-p", &d]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(
        out.lines()
            .any(|line| line.contains("buffer.pool_frames") && line.contains("131072")),
        "{out}"
    );
    assert!(out.contains("文件"), "{out}");

    // 命令行覆盖优先。
    let (_, out, _) = run(&["params", "-p", &d, "-c", "pool_frames=262144"]);
    assert!(out.contains("262144") && out.contains("命令行"), "{out}");

    // 按参数起的服务应正常就绪（参数真的被用上，没炸）。
    let (code, out, err) = start(dir.path());
    assert_eq!(code, 0, "start：{out}{err}");
    let _ = stop(dir.path(), "fast");

    // 未知键 ⇒ 具名拒绝（闭集），服务**不启动**。
    let text = std::fs::read_to_string(&conf).expect("读");
    std::fs::write(&conf, format!("{text}\nnope = 1\n")).expect("写");
    let (code, _out, err) = start(dir.path());
    assert_ne!(code, 0, "未知键应拒绝启动");
    // 死因随错误带上（日志尾行）——用户不必自己去翻日志。
    assert!(
        err.contains("没有参数 `nope`") || err.contains("闭集"),
        "{err}"
    );
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
        .arg("-p")
        .arg(&d)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("跑 shell");
    assert!(!out.status.success(), "服务在跑时直连应被拒");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("占用"), "提示应指认占用：{err}");
    let _ = stop(dir.path(), "fast");
}
