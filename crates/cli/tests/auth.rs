//! **认证与会话身份（D6）端到端**（`doc/evidence/auth-20261007/evidence.md`）。
//!
//! 钉住四条（都是证据包里的成熟产品口径）：
//! 1. **口令校验先、状态检查后**；"主体不存在"与"口令不对"**同一条**文案（防枚举）；
//! 2. `PAUSE` ⇒ **拒绝新会话**；`EXPIRE` ⇒ **受限会话**（只许本人改密）；
//! 3. 具名主体在 `public` 上**只读**；管理面语句要**管理面身份**；
//! 4. 身份只从 [`Session::authenticate`] 进——语句载荷里没有"用户号"这个字段
//!    （`REQ-ISO-002` 的结构保证；本文件用"以 alice 的会话去动 bob"来钉它）。
//!
//! 会话级（不走套接字）：协议层的准入另有 `crates/client/tests/live.rs` 的实机用例。

use std::path::Path;

use bicdb_cli::boot::{create_instance, Instance};
use bicdb_cli::config::InstanceParams;
use bicdb_cli::home::{Home, HomeSource};
use bicdb_sql::session::{QueryResult, Session};

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "bicdb-auth-{}-{tag}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        Self(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn home_of(root: &Path) -> Home {
    Home {
        root: root.to_path_buf(),
        source: HomeSource::Env,
    }
}

/// **建一个会话**（与 `bicdb sql` 一次的形态一致：管理面上下文都接上）。
fn session<'i>(inst: &'i mut Instance, home: &Home) -> Session<'i, 'static, 'static, 'static> {
    let seq = inst.seq();
    let mut s = Session::new(inst.pool, inst.engine, &mut inst.catalog, seq);
    s.set_dcl_context(Some(home.root.clone()), Some(inst.io));
    s.set_workspace_provisioner(Some(bicdb_cli::provision::CliProvisioner::new_static()));
    // 测试用 1000 轮（生产默认 210_000；迭代数写进散列串，故可调）。
    s.set_pbkdf2_iterations(1000);
    s
}

/// 管理面跑一条（新建会话——每次都是一次新的 `bicdb sql`）。
fn run(inst: &mut Instance, home: &Home, sql: &str) -> Result<String, String> {
    let mut s = session(inst, home);
    match s.execute(sql) {
        Ok(r) => Ok(format!("{:?}", r.last())),
        Err(e) => Err(e.to_string()),
    }
}

/// 建好 public + 一个盘 + 一个工作区 + 主体 `alice`（口令 `pw-one`）。
fn setup(tag: &str) -> (TempDir, Home, Instance) {
    let holder = TempDir::new(tag);
    let home = home_of(holder.path());
    let params = InstanceParams::for_init(&home.public_dir(), None, &[]).expect("参数");
    let mut inst = create_instance(&params, Some(&home)).expect("建 public");
    let d1 = holder.path().join("data1");
    std::fs::create_dir_all(&d1).unwrap();
    run(
        &mut inst,
        &home,
        &format!("CREATE FILESYSTEM data1 USING '{}'", d1.display()),
    )
    .expect("建盘");
    run(&mut inst, &home, "CREATE WORKSPACE w1").expect("建区");
    run(
        &mut inst,
        &home,
        "CREATE USER alice IDENTIFIED BY 'pw-one' USING WORKSPACE w1",
    )
    .expect("建主体");
    run(
        &mut inst,
        &home,
        "CREATE USER bob IDENTIFIED BY 'pw-bob' USING WORKSPACE w2",
    )
    .unwrap_err(); // w2 还没有——顺手钉一句"工作区不存在"
    run(&mut inst, &home, "CREATE WORKSPACE w2").expect("建区 2");
    run(
        &mut inst,
        &home,
        "CREATE USER bob IDENTIFIED BY 'pw-bob' USING WORKSPACE w2",
    )
    .expect("建主体 2");
    (holder, home, inst)
}

#[test]
fn authentication_admits_the_subject_and_never_leaks_whether_a_name_exists() {
    let (_h, home, mut inst) = setup("admit");
    let mut s = session(&mut inst, &home);

    // ① 对的口令 ⇒ 身份；主体号从目录来（不是客户端给的）。
    let id = s.authenticate("alice", "pw-one").expect("认证");
    assert_eq!(id.name(), "alice");
    assert_eq!(id.user_id(), 1);
    assert!(!id.is_expired());
    assert!(!s.is_management_identity(), "认证之后不再是管理面身份");
    drop(s);

    // ② **口令错**与**主体不存在**：两条文案**逐字节相同**（防枚举）。
    let mut s = session(&mut inst, &home);
    let wrong = s.authenticate("alice", "nope").unwrap_err().to_string();
    drop(s);
    let mut s = session(&mut inst, &home);
    let ghost = s
        .authenticate("nobody-here", "nope")
        .unwrap_err()
        .to_string();
    assert_eq!(wrong, ghost, "两条原因必须是同一条文案");
    assert!(!wrong.contains("不存在"), "{wrong}");
    // 认证不通过**不留身份**（会话还是管理面身份）。
    assert!(s.is_management_identity());
    drop(s);

    // ③ 一条会话只认证一次（重认证要重连——身份是连接的属性）。
    let mut s = session(&mut inst, &home);
    s.authenticate("alice", "pw-one").expect("认证");
    let again = s.authenticate("alice", "pw-one").unwrap_err().to_string();
    assert!(again.contains("重认证请重连"), "{again}");
    drop(s);

    // ④ `PAUSE` ⇒ **拒绝新会话**（已开会话不掐断）。
    run(&mut inst, &home, "ALTER USER alice PAUSE").expect("暂停");
    let mut s = session(&mut inst, &home);
    let paused = s.authenticate("alice", "pw-one").unwrap_err().to_string();
    assert!(paused.contains("已暂停"), "{paused}");
    // **状态检查在口令校验之后**：口令错时看到的仍是"就不对"，不是"已暂停"
    // （否则状态本身就成了枚举旁路）。
    drop(s);
    let mut s = session(&mut inst, &home);
    let bad = s.authenticate("alice", "nope").unwrap_err().to_string();
    assert_eq!(bad, wrong);
    drop(s);
    run(&mut inst, &home, "ALTER USER alice RESUME").expect("恢复");

    // ⑤ `EXPIRE` ⇒ 登录成功但**受限**（`identity().is_expired()`）。
    run(
        &mut inst,
        &home,
        "ALTER USER alice IDENTIFIED BY 'pw-two' EXPIRE",
    )
    .expect("重置");
    let mut s = session(&mut inst, &home);
    let id = s.authenticate("alice", "pw-two").expect("认证");
    assert!(id.is_expired(), "EXPIRE 后应是受限会话");
    drop(s);
    // 旧口令即时失效（U2 的既有语义，这里复核一次）。
    let mut s = session(&mut inst, &home);
    assert!(s.authenticate("alice", "pw-one").is_err());
}

#[test]
fn a_named_subject_is_read_only_and_may_only_change_its_own_password() {
    let (_h, home, mut inst) = setup("readonly");
    let mut s = session(&mut inst, &home);
    s.authenticate("alice", "pw-one").expect("认证");

    // ① 读：随便。
    assert!(s.execute("SELECT 1").is_ok());
    // ② 写：`public` 对主体**只读**（DML 与 DDL 各钉一条）。
    let e = s
        .execute("CREATE TABLE t (id NUMBER)")
        .unwrap_err()
        .to_string();
    assert!(e.contains("只读"), "{e}");
    let e = s
        .execute("INSERT INTO t VALUES (1)")
        .unwrap_err()
        .to_string();
    assert!(e.contains("只读") || e.contains("不存在"), "{e}");
    let e = s.execute("DROP USER bob").unwrap_err().to_string();
    assert!(e.contains("管理面身份"), "{e}");
    // `DROP WORKSPACE` 也走管理面（`Drop` 同时管表/索引/图——按对象类型分派）。
    let e = s.execute("DROP WORKSPACE w2").unwrap_err().to_string();
    assert!(e.contains("管理面身份"), "{e}");
    // ③ **别人的**口令：不是主体的事（A 规则在语句面）。
    let e = s
        .execute("ALTER USER bob IDENTIFIED BY 'x' REPLACE 'pw-bob'")
        .unwrap_err()
        .to_string();
    assert!(e.contains("管理面身份"), "{e}");
    // ④ **自己的**口令：可以（U3；Oracle 的"本人改密"口径）。
    let out = s
        .execute("ALTER USER alice IDENTIFIED BY 'pw-three' REPLACE 'pw-one'")
        .expect("本人改密");
    assert!(format!("{out:?}").contains("已换新口令"), "{out:?}");
    // ⑤ 新口令可登、旧口令不可。
    drop(s);
    let mut s = session(&mut inst, &home);
    assert!(s.authenticate("alice", "pw-one").is_err(), "旧口令应失效");
    drop(s);
    let mut s = session(&mut inst, &home);
    s.authenticate("alice", "pw-three").expect("新口令应可登");
    // ⑥ 管理面身份不受这些限制（本机 = OS 身份）：同一进程里另开一个会话。
    drop(s);
    run(&mut inst, &home, "CREATE TABLE 管理面可以建 (id NUMBER)").expect("管理面可建表");
}

#[test]
fn an_expired_session_is_restricted_to_its_own_password_change() {
    let (_h, home, mut inst) = setup("expired");
    run(
        &mut inst,
        &home,
        "ALTER USER alice IDENTIFIED BY 'pw-e' EXPIRE",
    )
    .expect("重置");

    let mut s = session(&mut inst, &home);
    let id = s.authenticate("alice", "pw-e").expect("认证");
    assert!(id.is_expired());

    // ① 受限：连 `SELECT` 都不放行（MySQL 的受限会话："改密前不能执行普通业务 SQL"）。
    let e = s.execute("SELECT 1").unwrap_err().to_string();
    assert!(e.contains("受限会话"), "{e}");
    // ② 改别人的口令：更不行。
    let e = s
        .execute("ALTER USER bob IDENTIFIED BY 'x' REPLACE 'pw-bob'")
        .unwrap_err()
        .to_string();
    assert!(e.contains("受限会话"), "{e}");
    // ③ 本人改密：**唯一**放行的一条。
    let out = s
        .execute("ALTER USER alice IDENTIFIED BY 'pw-fresh' REPLACE 'pw-e'")
        .expect("受限会话里的本人改密");
    assert!(format!("{out:?}").contains("过期限制已解除"), "{out:?}");
    // ④ 改完即放行（会话就地解除，不用重连——MySQL 的同位形态）。
    assert!(!s.identity().expect("有身份").is_expired());
    let r = s.execute("SELECT 1").expect("改完密就能跑普通语句");
    assert!(matches!(r.last(), Some(QueryResult::Rows { .. })));
    drop(s);
    // ⑤ 新口令可登（且不再受限）。
    let mut s = session(&mut inst, &home);
    let id = s.authenticate("alice", "pw-fresh").expect("认证");
    assert!(!id.is_expired());
}

#[test]
fn authentication_only_exists_on_public() {
    // 普通工作区（`w1`）没有 `user$`：认证在那里**具名拒绝**——
    // 本机访问按控制套接字的文件权限（OS 身份）判定。
    let (holder, _home, inst) = setup("nonpublic");
    drop(inst);
    let root = holder.path().join("w1");
    let (p, _) =
        bicdb_cli::boot::instance_params(Some(&root.join("bicdb.ini")), &[]).expect("参数");
    let mut w1 = bicdb_cli::boot::open_instance(&p).expect("开 w1");
    let seq = w1.seq();
    let mut s = Session::new(w1.pool, w1.engine, &mut w1.catalog, seq);
    let e = s.authenticate("alice", "pw-one").unwrap_err().to_string();
    assert!(e.contains("不做口令认证"), "{e}");
    assert!(s.is_management_identity(), "没认证 ⇒ 仍是管理面身份");
}
