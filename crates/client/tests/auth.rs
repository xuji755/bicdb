//! **认证的实机验收（Rust 驱动 ↔ 真服务）**：`AUTH` 走真套接字、真目录、真口令散列。
//!
//! 钉住（`doc/evidence/auth-20261007/evidence.md`；会话级另有
//! `crates/cli/tests/auth.rs`）：
//! 1. `connect_as` 带上身份（`user()`/`user_id()`/`password_expired()`）；
//! 2. 具名主体在 `public` 上**只读**、管理面语句要管理面身份；
//! 3. "口令错"与"主体不存在"**同一条**错误（防枚举，走线也要一致）；
//! 4. `AUTH` 必须是连接上的**第一个业务请求**（跑过 SQL 再认证 ⇒ 拒绝）；
//! 5. `PAUSE` ⇒ 拒绝新会话；`EXPIRE` ⇒ 受限会话（本人改密即解除）。

use std::path::{Path, PathBuf};
use std::process::Command;

use bicdb_client::{Connection, Error};

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("bicdb-authdrv-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // 服务（守护进程）在 home/public 上；停它再删目录。
        let _ = Command::new(bicdb_bin())
            .args([
                "stop",
                "-p",
                &self.0.join("public").display().to_string(),
                "-m",
                "immediate",
            ])
            .env("BICDB_HOME", &self.0)
            .output();
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `bicdb` 二进制：测试进程在 `target/<profile>/deps/` 下，上一层就是它。
fn bicdb_bin() -> PathBuf {
    let exe = std::env::current_exe().expect("取当前测试可执行文件");
    let dir = exe
        .parent()
        .and_then(Path::parent)
        .expect("target/<profile> 目录");
    dir.join("bicdb")
}

/// 跑一条 `bicdb` 子命令（**带上 `BICDB_HOME`**——管理面要它）。
fn run(home: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(bicdb_bin())
        .args(args)
        .env("BICDB_HOME", home)
        .output()
        .expect("跑 bicdb");
    (
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

/// 建一台部署（`<home>/public` = PUBLIC 工作区）+ 起服务 + **一个盘 / 一个工作区 /
/// 两个主体**（`alice`、`bob`）。返回 home 根。
fn serving(tag: &str) -> TempDir {
    let home = TempDir::new(tag);
    let root = home.path();
    let public = root.join("public");
    let (ok, out) = run(root, &["init", &public.display().to_string()]);
    assert!(ok, "init：{out}");
    let d1 = root.join("d1");
    std::fs::create_dir_all(&d1).expect("建盘目录");
    // **测试用 1000 轮**（生产默认 210_000；迭代数写进散列串，故可调）。
    let (ok, out) = run(
        root,
        &[
            "start",
            "-p",
            &public.display().to_string(),
            "-w",
            "30",
            "-c",
            "auth.pbkdf2_iterations=1000",
            "-c",
            "buffer.pool_frames=262144",
        ],
    );
    assert!(ok, "start：{out}");
    for sql in [
        format!("CREATE FILESYSTEM d1 USING '{}'", d1.display()),
        "CREATE WORKSPACE w1".to_owned(),
        "CREATE WORKSPACE w2".to_owned(),
        "CREATE USER alice IDENTIFIED BY 'pw-one' USING WORKSPACE w1".to_owned(),
        "CREATE USER bob IDENTIFIED BY 'pw-bob' USING WORKSPACE w2".to_owned(),
    ] {
        let (ok, out) = run(root, &["sql", "-p", &public.display().to_string(), &sql]);
        assert!(ok, "管理面 `{sql}`：{out}");
    }
    home
}

fn public_of(home: &Path) -> PathBuf {
    home.join("public")
}

/// 断言错误是**服务端原文**且含某个片段（驱动不该改写它——协议 §9）。
fn server_err<T: std::fmt::Debug>(r: Result<T, Error>, needle: &str) -> String {
    let e = r.expect_err("应报错").to_string();
    assert!(e.contains(needle), "期望含 `{needle}`：{e}");
    e
}

#[test]
fn a_subject_authenticates_reads_and_may_change_its_own_password() {
    let home = serving("subject");
    let public = public_of(home.path());

    // Put one shared graph in PUBLIC so the wire-level check covers graph reads
    // as well as rejecting every graph mutation class before name binding.
    let mut setup = Connection::connect(&public).expect("management connection");
    setup
        .execute("CREATE GRAPH shared_kg", &[])
        .expect("shared graph");
    drop(setup);

    // 管理面（不认证）能管；**认证的连接**不能管、不能写，只能读。
    let mut conn = Connection::connect_as(&public, "alice", "pw-one").expect("认证并连上");
    assert_eq!(conn.user(), Some("alice"));
    assert_eq!(conn.user_id(), Some(1));
    assert_eq!(conn.password_expired(), Some(false));
    // 读：随便。
    let rs = conn.query("SELECT 2 + 3 * 4 AS n", &[]).expect("读");
    assert_eq!(rs.row(0).expect("行").i64(0).expect("值"), 14);
    conn.query("CYPHER shared_kg 'RETURN 1 AS n'", &[])
        .expect("PUBLIC read-only Cypher is shared with named subjects");
    // 写/管理：**服务端原文透传**（具名拒绝）。
    server_err(conn.execute("CREATE TABLE t (id NUMBER)", &[]), "只读");
    server_err(conn.execute("CREATE GRAPH another_kg", &[]), "只读");
    server_err(
        conn.execute("CYPHER shared_kg 'CREATE (:N {key:1})'", &[]),
        "只读",
    );
    server_err(
        conn.execute(
            "CREATE GRAPH INDEX graph_keys ON shared_kg NODES LABEL \"N\" (key)",
            &[],
        ),
        "只读",
    );
    server_err(
        conn.execute("ALTER FULLTEXT GRAPH INDEX words ON shared_kg SYNC", &[]),
        "只读",
    );
    server_err(conn.execute("DROP USER bob", &[]), "管理面身份");
    server_err(
        conn.execute("ALTER USER bob IDENTIFIED BY 'x' REPLACE 'pw-bob'", &[]),
        "管理面身份",
    );
    // 本人改密：放行。
    conn.execute(
        "ALTER USER alice IDENTIFIED BY 'pw-two' REPLACE 'pw-one'",
        &[],
    )
    .expect("本人改密");
    drop(conn); // 服务一次一条连接：放开再连。

    // 新口令可登、旧口令不可。
    server_err(
        Connection::connect_as(&public, "alice", "pw-one"),
        "主体名或口令不对",
    );
    let conn = Connection::connect_as(&public, "alice", "pw-two").expect("新口令");
    assert_eq!(conn.user(), Some("alice"));
    // 管理面身份仍然全权（同一条服务、另一条连接）。
    drop(conn);
    let mut admin = Connection::connect(&public).expect("管理面连上");
    assert_eq!(admin.user(), None, "没认证 ⇒ 没有身份");
    admin
        .execute("CREATE TABLE 管理面可以建 (id NUMBER)", &[])
        .expect("管理面可建表");
}

#[test]
fn bad_password_and_unknown_subject_are_one_message_over_the_wire() {
    let home = serving("enumerate");
    let public = public_of(home.path());
    let wrong = server_err(
        Connection::connect_as(&public, "alice", "nope"),
        "主体名或口令不对",
    );
    let ghost = server_err(
        Connection::connect_as(&public, "nobody-here", "nope"),
        "主体名或口令不对",
    );
    assert_eq!(wrong, ghost, "两条原因必须是同一条文案（防枚举）");
}

#[test]
fn public_file_dollar_is_invisible_to_a_named_subject() {
    // `spec/SQL.md` 待冻结项 47：`public` 的 `file$` 是**管理元数据**——普通主体
    // 看不到它（"本工作区的 `file$` 对其属主可见"，而管理面的清单只给管理身份）。
    let home = serving("filevisibility");
    let public = public_of(home.path());
    // 管理面（不认证 = 本机/OS 身份）：看得见。
    let mut admin = Connection::connect(&public).expect("管理面连上");
    let rs = admin
        .query("SELECT COUNT(*) FROM file$", &[])
        .expect("管理面可查 file$");
    assert!(rs.row(0).expect("行").i64(0).expect("数") >= 1);
    drop(admin);
    // 具名主体：**名字不存在**（与"从未有过这张表"同一形态，不给暗示）。
    let mut conn = Connection::connect_as(&public, "alice", "pw-one").expect("认证");
    let e = server_err(conn.query("SELECT * FROM file$", &[]), "不存在");
    assert!(
        !e.contains("权限"),
        "不许出现权限字样（名字压根不可见，不是拒绝）：{e}"
    );
}

#[test]
fn auth_must_be_the_first_business_request_on_a_connection() {
    let home = serving("first");
    let public = public_of(home.path());
    let mut conn = Connection::connect(&public).expect("先不认证");
    conn.query("SELECT 1", &[]).expect("先跑一条 SQL");
    // 跑过语句再"变成"某个主体 ⇒ 拒绝（否则就等于"先以管理面身份做事、再冒名"）。
    let e = conn.login("alice", "pw-one").expect_err("应拒绝");
    assert!(e.to_string().contains("第一个业务请求"), "{e}");
}

#[test]
fn pause_refuses_new_sessions_and_expire_restricts_them() {
    let home = serving("pause-expire");
    let root = home.path();
    let public = public_of(root);
    let admin_sql = |sql: &str| {
        let (ok, out) = run(root, &["sql", "-p", &public.display().to_string(), sql]);
        assert!(ok, "管理面 `{sql}`：{out}");
    };

    // `PAUSE` ⇒ 拒绝新会话。
    admin_sql("ALTER USER alice PAUSE");
    server_err(Connection::connect_as(&public, "alice", "pw-one"), "已暂停");
    admin_sql("ALTER USER alice RESUME");
    Connection::connect_as(&public, "alice", "pw-one").expect("恢复后可登");

    // `EXPIRE` ⇒ 受限会话：连上但只许本人改密。
    admin_sql("ALTER USER alice IDENTIFIED BY 'pw-e' EXPIRE");
    let mut conn = Connection::connect_as(&public, "alice", "pw-e").expect("受限会话能连上");
    assert_eq!(conn.password_expired(), Some(true));
    server_err(conn.query("SELECT 1", &[]), "受限会话");
    conn.execute(
        "ALTER USER alice IDENTIFIED BY 'pw-fresh' REPLACE 'pw-e'",
        &[],
    )
    .expect("受限会话里的本人改密");
    // 改完即放行（不用重连——会话就地解除）。
    conn.query("SELECT 1", &[]).expect("改完密就能跑普通语句");
    drop(conn);
    let conn = Connection::connect_as(&public, "alice", "pw-fresh").expect("新口令");
    assert_eq!(conn.password_expired(), Some(false));
}

#[test]
fn administrator_changes_effective_private_workspace_open_mode() {
    let home = serving("workspace-open-mode");
    let public = public_of(home.path());

    let mut admin = Connection::connect(&public).expect("管理面连接");
    admin
        .execute("ALTER WORKSPACE w1 OPEN READ ONLY", &[])
        .expect("打开私有工作区为只读");
    server_err(
        admin.execute("ALTER WORKSPACE public OPEN READ WRITE FORCE", &[]),
        "PUBLIC 禁止 FORCE",
    );
    admin
        .execute("ALTER WORKSPACE public OPEN READ ONLY", &[])
        .expect("PUBLIC 可由管理员显式设为只读");
    server_err(
        admin.execute("CREATE TABLE public_blocked (id NUMBER)", &[]),
        "管理员设为只读",
    );
    admin
        .execute("ALTER WORKSPACE public OPEN READ WRITE", &[])
        .expect("OPEN 管理命令本身可解除 PUBLIC 只读");
    admin
        .execute("CREATE TABLE public_writable (id NUMBER)", &[])
        .expect("PUBLIC 恢复读写");
    drop(admin);

    let mut user = Connection::connect_as(&public, "alice", "pw-one").expect("认证");
    let route = user.bind_workspace(None).expect("绑定本人工作区");
    assert_eq!(route.name, "w1");
    user.query("SELECT 1", &[]).expect("只读工作区可查询");
    server_err(
        user.execute("CREATE TABLE blocked (id NUMBER)", &[]),
        "管理员设为只读",
    );
    let status = user.status().expect("状态");
    assert!(status
        .iter()
        .any(|(key, value)| { key == "workspace_write_state" && value == "READ_ONLY" }));
    assert!(status
        .iter()
        .any(|(key, value)| key == "shutdown_state" && value == "RUNNING"));
    assert!(status
        .iter()
        .any(|(key, value)| key == "quarantined_pages" && value == "0"));
    assert!(status
        .iter()
        .any(|(key, value)| key == "quarantined_objects" && value == "0"));
    drop(user);

    let mut admin = Connection::connect(&public).expect("管理面重连");
    admin
        .execute("ALTER WORKSPACE w1 OPEN READ WRITE", &[])
        .expect("正常恢复门后重新开放写入");
    drop(admin);

    let mut user = Connection::connect_as(&public, "alice", "pw-one").expect("再次认证");
    user.bind_workspace(None).expect("再次绑定");
    user.execute("CREATE TABLE writable (id NUMBER)", &[])
        .expect("恢复读写后应可建表");
    let status = user.status().expect("状态");
    assert!(status
        .iter()
        .any(|(key, value)| { key == "workspace_write_state" && value == "READ_WRITE" }));
}
