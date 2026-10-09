//! **Rust 驱动的实机验收**：起一个真服务（`bicdb start`），用它跑一遍驱动面。
//!
//! 钉的是"驱动**真能**说话"：建表/写入/查询/参数/事务/`describe`/错误原文/
//! 非 UTF-8 字节串无损/版本核对/读己所写/多会话隔离。**不用 mock**——协议错了
//! 就得在这儿现形。
//!
//! 每条连接保有独立身份、快照和显式事务；服务在一个实例执行器上公平调度
//! 请求，连接之间不能共享未提交数据或用旧图快照覆盖新提交。

use std::path::{Path, PathBuf};
use std::process::Command;

use bicdb_client::{ColumnKind, Connection, Error, Value};

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("bicdb-drv-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = Command::new(bicdb_bin())
            .args([
                "stop",
                "-p",
                &self.0.display().to_string(),
                "-m",
                "immediate",
            ])
            .output();
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `bicdb` 二进制：测试进程自己就在 `target/<profile>/deps/` 下——
/// 同一目录树的上一层就是它（不需要额外的路径注入）。
fn bicdb_bin() -> PathBuf {
    let exe = std::env::current_exe().expect("取当前测试可执行文件");
    let dir = exe
        .parent()
        .and_then(Path::parent)
        .expect("target/<profile> 目录");
    dir.join("bicdb")
}

fn run(args: &[&str]) {
    let out = Command::new(bicdb_bin())
        .args(args)
        .output()
        .expect("跑 bicdb");
    assert!(
        out.status.success(),
        "bicdb {args:?} 失败：{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// 建区 + 起服务（返回根区目录；`TempDir` 落下时停服务并清目录）。
fn serving(tag: &str) -> TempDir {
    let dir = TempDir::new(tag);
    let d = dir.path().display().to_string();
    run(&["init", &d]);
    run(&["start", "-p", &d, "-w", "30"]);
    dir
}

#[test]
fn driver_talks_to_a_real_service() {
    let dir = serving("live");
    let mut conn = Connection::connect(dir.path()).expect("连上");

    // 握手信息（协议 §4.1）。
    assert_eq!(conn.wire_version(), bicdb_client::WIRE_VERSION);
    assert_eq!(conn.server_version(), env!("CARGO_PKG_VERSION"));
    assert!(
        conn.instance().contains("bicdb-drv-"),
        "实例路径：{}",
        conn.instance()
    );

    // DDL + 写入 + 参数化查询。
    conn.execute(
        "CREATE TABLE t (id NUMBER NOT NULL, name VARCHAR2(32))",
        &[],
    )
    .expect("建表");
    let n = conn
        .execute(
            "INSERT INTO t VALUES (:id, :name)",
            &[("id", 1_i64.into()), ("name", "alpha".into())],
        )
        .expect("插入");
    assert_eq!(n, 1);
    conn.execute("INSERT INTO t VALUES (2, 'beta')", &[])
        .expect("插入2");

    let rs = conn
        .query("SELECT id, name FROM t ORDER BY id", &[])
        .expect("查");
    assert_eq!(rs.len(), 2);
    // 列名照 PG 的口径**折叠小写**（未引号的标识符）。
    assert_eq!(rs.columns()[0].name, "id");
    assert_eq!(rs.columns()[0].kind, ColumnKind::Number);
    assert_eq!(rs.columns()[1].kind, ColumnKind::Bytes);
    let row = rs.row(0).expect("第一行");
    assert_eq!(row.i64(0).expect("id"), 1);
    assert_eq!(row.str(1).expect("name"), "alpha");
    // 按名取**大小写不敏感**（用户写 NAME 也该找到）。
    assert_eq!(row.by_name("NAME").expect("按名取").as_str(), Some("alpha"));
    // 数值也原样保留十进制文本（不丢精度）。
    assert_eq!(row.get(0).and_then(Value::as_decimal), Some("1"));
    // 列名大小写不敏感地找列。
    assert_eq!(rs.column_index("name"), Some(1));

    // 参数摆位错 ⇒ 服务端的具名错误，**原文透传**。
    let err = conn
        .query("SELECT id FROM t WHERE id = :nope", &[("id", 1_i64.into())])
        .expect_err("缺参数应报错");
    assert!(err.is_server(), "应是服务端错误：{err}");
    assert!(
        err.server_message().unwrap_or_default().contains("nope"),
        "错误文本：{err}"
    );

    // 不是结果集的语句走 `query` ⇒ 具名拒绝。
    let err = conn
        .query("INSERT INTO t VALUES (3, 'gamma')", &[])
        .expect_err("不是结果集");
    assert!(matches!(err, Error::NotResultSet { .. }), "{err}");

    // `describe`（带全量元数据）。
    let cols = conn.describe("t").expect("desc");
    assert_eq!(cols.len(), 2);
    assert_eq!(cols[0].name, "id");
    assert_eq!(cols[0].kind, ColumnKind::Number);
    assert!(!cols[0].nullable);
    assert_eq!(cols[1].type_name, "VARCHAR2(32)");
    assert!(cols[1].nullable);

    // 事务：一条连接 = 一个会话，跨请求保持。
    conn.execute("BEGIN", &[]).expect("begin");
    conn.execute("INSERT INTO t VALUES (4, 'delta')", &[])
        .expect("插入");
    // 服务端能看到自己的未提交行（同一会话）。
    let rs = conn.query("SELECT id FROM t ORDER BY id", &[]).expect("查");
    assert_eq!(rs.len(), 4, "会话内应看到未提交的行");
    conn.rollback().expect("回滚");
    let rs = conn.query("SELECT id FROM t ORDER BY id", &[]).expect("查");
    assert_eq!(rs.len(), 3, "回滚后应回到 3 行");

    // 提交之后，同时在线的另一会话在下一条语句刷新到最新提交。
    conn.execute("BEGIN", &[]).expect("begin");
    conn.execute("INSERT INTO t VALUES (5, 'eps')", &[])
        .expect("插入");
    conn.commit().expect("提交");
    let mut other = Connection::connect(dir.path()).expect("第二条连接");
    let rs = other
        .query("SELECT id FROM t ORDER BY id", &[])
        .expect("查");
    assert_eq!(rs.len(), 4);

    // 服务自述（`key=value`）。
    let status = other.status().expect("status");
    let get = |k: &str| {
        status
            .iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.clone())
    };
    assert_eq!(get("mode").as_deref(), Some("service"));
    assert_eq!(get("wire").as_deref(), Some("1"));
    assert_eq!(get("connections").as_deref(), Some("2"));
    assert_eq!(get("workspace_kind").as_deref(), Some("private"));
    assert!(get("pid").is_some());
}

#[test]
fn bytes_are_lossless_and_text_columns_stay_text() {
    let dir = serving("bytes");
    let mut conn = Connection::connect(dir.path()).expect("连上");
    // 字节串列用 `VARCHAR2`（本库的文本/二进制同形：线上都是 `b` 形态；
    // `RAW` 不在本版 SQL 面里——见使用手册 §5 的不支持清单）。
    conn.execute("CREATE TABLE r (k NUMBER NOT NULL, v VARCHAR2(8))", &[])
        .expect("建表");
    // **非 UTF-8 的字节**：写进去、取出来必须一模一样（不是 `�`）。
    let raw = vec![0x00u8, 0xff, 0x0a, 0x80];
    conn.execute(
        "INSERT INTO r VALUES (:k, :v)",
        &[("k", 1_i64.into()), ("v", raw.clone().into())],
    )
    .expect("插入");
    let rs = conn.query("SELECT k, v FROM r", &[]).expect("查");
    let row = rs.row(0).expect("行");
    assert_eq!(row.bytes(1).expect("字节串"), &raw[..]);
    // 按文本取 ⇒ **具名拒绝**（不替换成 `�`）。
    let err = row.str(1).expect_err("非 UTF-8 不该当文本");
    assert!(matches!(err, Error::Type { .. }), "{err}");
    // NULL 与"有值"**分得开**。
    conn.execute("INSERT INTO r VALUES (2, NULL)", &[])
        .expect("插 NULL");
    let rs = conn
        .query("SELECT k, v FROM r WHERE k = 2", &[])
        .expect("查");
    let row = rs.row(0).expect("行");
    assert!(row.is_null(1).expect("取空判定"));
    assert_eq!(row.get(1).and_then(Value::as_bytes), None);
}

#[test]
fn connecting_where_nothing_runs_is_a_named_error() {
    let dir = TempDir::new("nobody");
    let d = dir.path().display().to_string();
    run(&["init", &d]); // 建区但**不起服务**
    let err = Connection::connect(dir.path()).expect_err("没人接客应报错");
    match &err {
        Error::Connect { path, .. } => assert!(path.ends_with("bicdb.sock"), "{path:?}"),
        other => panic!("应是连接错，得到：{other}"),
    }
    // 寻址也能单跑（不连）。
    let sock = bicdb_client::locate_socket(Some(dir.path())).expect("定位");
    assert!(sock.ends_with("bicdb.sock"));
    // 参数文件不存在 ⇒ 寻址错误（不是连接错误）。
    let err = Connection::connect(dir.path().join("nope")).expect_err("应报错");
    assert!(matches!(err, Error::Discover { .. }), "{err}");
}

#[test]
fn two_live_connections_isolate_transactions_and_fence_stale_graph_writes() {
    let dir = serving("multi-session");
    let mut first = Connection::connect(dir.path()).expect("第一条连接");
    let mut second = Connection::connect(dir.path()).expect("第二条连接");
    first
        .execute(
            "CREATE TABLE t (id NUMBER NOT NULL); CREATE UNIQUE INDEX t_pk ON t (id)",
            &[],
        )
        .unwrap();
    first.begin().unwrap();
    first.execute("INSERT INTO t VALUES (1)", &[]).unwrap();
    assert_eq!(
        second
            .query("SELECT count(*) FROM t", &[])
            .unwrap()
            .row(0)
            .unwrap()
            .i64(0)
            .unwrap(),
        0,
        "另一连接不能看到未提交行"
    );
    first.commit().unwrap();
    assert_eq!(
        second
            .query("SELECT count(*) FROM t", &[])
            .unwrap()
            .row(0)
            .unwrap()
            .i64(0)
            .unwrap(),
        1,
        "空闲连接的下一条语句应刷新到最新提交"
    );

    // 断开连接必须由服务端回滚悬挂事务，不能留下锁或幽灵提交。
    let mut abandoned = Connection::connect(dir.path()).expect("第三条连接");
    abandoned.begin().unwrap();
    abandoned.execute("INSERT INTO t VALUES (9)", &[]).unwrap();
    abandoned.close();
    std::thread::sleep(std::time::Duration::from_millis(100));
    second.execute("INSERT INTO t VALUES (9)", &[]).unwrap();
    assert_eq!(
        second
            .query("SELECT count(*) FROM t", &[])
            .unwrap()
            .row(0)
            .unwrap()
            .i64(0)
            .unwrap(),
        2,
        "断线事务必须回滚并释放唯一键写入"
    );

    first.execute("CREATE GRAPH kg", &[]).unwrap();
    first
        .execute(
            "CREATE UNIQUE GRAPH INDEX graph_keys ON kg NODES LABEL \"N\" (key)",
            &[],
        )
        .unwrap();
    first
        .execute("CYPHER kg 'CREATE (:N {key:1})'", &[])
        .unwrap();
    first.begin().unwrap();
    assert_eq!(
        first
            .query("CYPHER kg 'MATCH (n:N) RETURN count(n)'", &[])
            .unwrap()
            .row(0)
            .unwrap()
            .i64(0)
            .unwrap(),
        1
    );
    second
        .execute("CYPHER kg 'CREATE (:N {key:2})'", &[])
        .unwrap();
    let error = first
        .execute("CYPHER kg 'CREATE (:N {key:3})'", &[])
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("changed") || error.contains("proof root"),
        "{error}"
    );
    first.rollback().unwrap();
    second
        .execute("CYPHER kg 'MERGE (:N {key:4})'", &[])
        .unwrap();
    first
        .execute("CYPHER kg 'MERGE (:N {key:4})'", &[])
        .unwrap();
    assert_eq!(
        second
            .query("CYPHER kg 'MATCH (n:N) RETURN count(n)'", &[])
            .unwrap()
            .row(0)
            .unwrap()
            .i64(0)
            .unwrap(),
        3,
        "跨连接 MERGE 不得重复创建唯一节点"
    );
}
