//! **DCL 执行（F 组：文件系统池）端到端**（`doc/DCL语句设计_v0.1.md` v0.2 §1.2）。
//!
//! 钉住：`CREATE FILESYSTEM … USING '<路径>'` → 注册表 + `public.fs$` 两处都落；
//! `ALTER … SET ALLOCATE` / `DROP FILESYSTEM` 改的是同一对；
//! **顺序纪律**（属性先、注册表最后）；路径校验（目录/非符号链接/可写）；
//! 关 → 开之后两处都还在。

use std::path::Path;

use bicdb_cli::boot::{create_instance, open_global_ctl};
use bicdb_cli::config::InstanceParams;
use bicdb_cli::home::{Home, HomeSource};
use bicdb_sql::session::{QueryResult, Session};

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "bicdb-dcl-{}-{tag}-{}",
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

/// 跑一条 SQL（每次新建会话——与 `bicdb sql` 一次的形态一致）。
fn run(inst: &mut bicdb_cli::boot::Instance, home: &Home, sql: &str) -> Result<String, String> {
    let seq = inst.seq();
    let mut session = Session::new(inst.pool, inst.engine, &mut inst.catalog, seq);
    session.set_dcl_context(Some(home.root.clone()), Some(inst.io));
    session.set_workspace_provisioner(Some(bicdb_cli::provision::CliProvisioner::new_static()));
    // 测试用 1000 轮（生产默认 210_000；迭代数写进散列串，故可调）。
    session.set_pbkdf2_iterations(1000);
    match session.execute(sql) {
        Ok(results) => Ok(format!("{:?}", results.last())),
        Err(e) => Err(e.to_string()),
    }
}

#[test]
fn schema_templates_copy_empty_structure_and_refuse_data_or_owned_sources() {
    let holder = TempDir::new("schema-template");
    let home = home_of(holder.path());
    let params = InstanceParams::for_init(&home.public_dir(), None, &[]).unwrap();
    let mut public = create_instance(&params, Some(&home)).unwrap();
    let disk = home.root.join("disk");
    std::fs::create_dir(&disk).unwrap();
    run(
        &mut public,
        &home,
        &format!("CREATE FILESYSTEM disk USING '{}'", disk.display()),
    )
    .unwrap();
    run(&mut public, &home, "CREATE WORKSPACE seed").unwrap();
    let (params, _) =
        bicdb_cli::boot::instance_params(Some(&home.root.join("seed/bicdb.ini")), &[]).unwrap();
    {
        let mut seed = bicdb_cli::boot::open_instance(&params).unwrap();
        let seq = seed.seq();
        let mut session = Session::new(seed.pool, seed.engine, &mut seed.catalog, seq);
        session.execute("CREATE TABLE empty_init (id NUMBER NOT NULL, name VARCHAR2(32), data BYTES(256)); CREATE UNIQUE INDEX empty_id ON empty_init(id); CREATE INDEX empty_name ON empty_init(name)").unwrap();
        drop(session);
        seed.shutdown().unwrap();
    }
    run(
        &mut public,
        &home,
        "ALTER DATABASE ADD TEMPLATE 'empty_template' FROM seed",
    )
    .unwrap();
    assert!(run(
        &mut public,
        &home,
        "ALTER DATABASE ADD TEMPLATE 'empty_template' FROM seed"
    )
    .is_err());
    assert!(run(
        &mut public,
        &home,
        "ALTER DATABASE ADD TEMPLATE '../escape' FROM seed"
    )
    .is_err());
    assert!(run(
        &mut public,
        &home,
        "ALTER DATABASE ADD TEMPLATE 'public_copy' FROM public"
    )
    .is_err());
    run(
        &mut public,
        &home,
        "CREATE WORKSPACE cloned FROM TEMPLATE 'empty_template'",
    )
    .unwrap();
    assert!(home.root.join("cloned/workspace-template.info").is_file());
    {
        let (params, _) =
            bicdb_cli::boot::instance_params(Some(&home.root.join("cloned/bicdb.ini")), &[])
                .unwrap();
        let mut cloned = bicdb_cli::boot::open_instance(&params).unwrap();
        let seq = cloned.seq();
        let mut session = Session::new(cloned.pool, cloned.engine, &mut cloned.catalog, seq);
        assert!(session.execute("SELECT * FROM empty_init").is_ok());
        session
            .execute("INSERT INTO empty_init VALUES (1, 'private-clone', NULL)")
            .unwrap();
        assert!(session
            .execute("INSERT INTO empty_init VALUES (1, 'duplicate', NULL)")
            .is_err());
        drop(session);
        cloned.shutdown().unwrap();
    }
    // A populated source must never be accepted as an initialization template.
    assert!(run(
        &mut public,
        &home,
        "ALTER DATABASE ADD TEMPLATE 'bad_data' FROM cloned"
    )
    .unwrap_err()
    .contains("非空"));
    run(
        &mut public,
        &home,
        "CREATE USER owner IDENTIFIED BY 'test-only-password' USING WORKSPACE cloned",
    )
    .unwrap();
    assert!(run(
        &mut public,
        &home,
        "ALTER DATABASE ADD TEMPLATE 'bad_owner' FROM cloned"
    )
    .unwrap_err()
    .contains("无主"));
    run(
        &mut public,
        &home,
        "CREATE WORKSPACE clone_two FROM TEMPLATE 'empty_template'",
    )
    .unwrap();
    let script = home.root.join("templates/empty_template/schema.sql");
    let original = std::fs::read(&script).unwrap();
    std::fs::write(&script, b"CREATE TABLE corrupted (id NUMBER);").unwrap();
    assert!(run(
        &mut public,
        &home,
        "CREATE WORKSPACE broken FROM TEMPLATE 'empty_template'"
    )
    .is_err());
    assert!(!home.root.join("broken").exists());
    std::fs::write(&script, original).unwrap();
    run(
        &mut public,
        &home,
        "ALTER DATABASE DROP TEMPLATE 'empty_template'",
    )
    .unwrap();
    assert!(!home.root.join("templates/empty_template").exists());
    assert!(home.root.join("clone_two/bicdb.ini").exists());
    public.shutdown().unwrap();
}

#[test]
fn filesystem_pool_round_trip() {
    let holder = TempDir::new("home_root");
    let home = home_of(holder.path());
    let params = InstanceParams::for_init(&home.public_dir(), None, &[]).expect("参数");
    let mut inst = create_instance(&params, Some(&home)).expect("建 public");

    // 两个"盘"（就是两个普通目录——F1 明确允许）。
    let d1 = holder.path().join("data1");
    let d2 = holder.path().join("data2");
    std::fs::create_dir_all(&d1).unwrap();
    std::fs::create_dir_all(&d2).unwrap();

    let ok = run(
        &mut inst,
        &home,
        &format!("CREATE FILESYSTEM data1 USING '{}'", d1.display()),
    )
    .expect("建 data1");
    assert!(ok.contains("已入池"), "{ok}");

    // ① 注册表（位置）：在池、槽位 1。
    {
        let gcf = open_global_ctl(&home).expect("注册表");
        let members = gcf.fs_members().expect("在池清单");
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].slot, 1);
        assert_eq!(members[0].name, b"data1");
        assert!(members[0].allocate, "默认 ON");
        let canonical = std::fs::canonicalize(&d1).unwrap();
        assert_eq!(
            String::from_utf8_lossy(&members[0].path).as_ref(),
            canonical.display().to_string()
        );
        gcf.close().expect("关");
    }
    // ② 属性（`public.fs$`）：同一槽位、同一路径。
    {
        let listed = bicdb_catalog::dcl::list_fs(&mut inst.catalog).expect("fs$ 清单");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].1.slot, 1);
        assert_eq!(listed[0].1.name, "data1");
    }

    // 槽位单调：第二个拿 2。
    run(
        &mut inst,
        &home,
        &format!("CREATE FILESYSTEM data2 USING '{}'", d2.display()),
    )
    .expect("建 data2");
    {
        let gcf = open_global_ctl(&home).expect("注册表");
        let slots: Vec<u16> = gcf.fs_members().unwrap().iter().map(|r| r.slot).collect();
        assert_eq!(slots, vec![1, 2]);
        gcf.close().unwrap();
    }

    // 排水阀：两处都改。
    run(
        &mut inst,
        &home,
        "ALTER FILESYSTEM data1 SET ALLOCATE = OFF",
    )
    .expect("排水");
    {
        let gcf = open_global_ctl(&home).expect("注册表");
        assert!(!gcf.fs_by_name(b"data1").unwrap().unwrap().allocate);
        gcf.close().unwrap();
    }
    assert!(
        !bicdb_catalog::dcl::fs_by_name(&mut inst.catalog, "data1")
            .unwrap()
            .unwrap()
            .allocate
    );

    // 校验：不是目录 / 符号链接 / 目录不存在——三条都具名拒绝。
    let file = holder.path().join("plain.txt");
    std::fs::write(&file, b"x").unwrap();
    let e = run(
        &mut inst,
        &home,
        &format!("CREATE FILESYSTEM bad USING '{}'", file.display()),
    )
    .unwrap_err();
    assert!(e.contains("不是目录"), "{e}");
    #[cfg(unix)]
    {
        let link = holder.path().join("link");
        std::os::unix::fs::symlink(&d1, &link).unwrap();
        let e = run(
            &mut inst,
            &home,
            &format!("CREATE FILESYSTEM link1 USING '{}'", link.display()),
        )
        .unwrap_err();
        assert!(e.contains("符号链接"), "{e}");
    }
    let e = run(
        &mut inst,
        &home,
        "CREATE FILESYSTEM nope USING '/no/such/dir/here'",
    )
    .unwrap_err();
    assert!(e.contains("不存在"), "{e}");

    // 唯一性：路径重复、名字重复——两处都拒绝（注册表层与 fs$ 层各有一道）。
    let e = run(
        &mut inst,
        &home,
        &format!("CREATE FILESYSTEM other USING '{}'", d1.display()),
    )
    .unwrap_err();
    assert!(e.contains("i_fs_path") || e.contains("已在池"), "{e}");

    // DROP：注册表墓碑 + fs$ 状态；槽位不复用。
    run(&mut inst, &home, "DROP FILESYSTEM data2").expect("移出");
    {
        let gcf = open_global_ctl(&home).expect("注册表");
        assert_eq!(gcf.fs_members().unwrap().len(), 1, "在池只剩 data1");
        assert_eq!(
            gcf.fs_by_slot(2).unwrap().unwrap().status,
            bicdb_storage::globalctl::FS_REMOVED
        );
        gcf.close().unwrap();
    }
    let d3 = holder.path().join("data3");
    std::fs::create_dir_all(&d3).unwrap();
    run(
        &mut inst,
        &home,
        &format!("CREATE FILESYSTEM data3 USING '{}'", d3.display()),
    )
    .expect("建 data3");
    {
        let gcf = open_global_ctl(&home).expect("注册表");
        assert_eq!(
            gcf.fs_by_name(b"data3").unwrap().unwrap().slot,
            3,
            "槽位不复用（2 是 data2 的历史）"
        );
        gcf.close().unwrap();
    }

    // **关 → 开**：两处都还在（注册表在 home 下、fs$ 行在字典里）。
    inst.shutdown().expect("关");
    drop(inst); // 实例锁随 `Instance` 的 Drop 释放——重开前必须先放掉。
    let mut inst = {
        let (params, _) =
            bicdb_cli::boot::instance_params(Some(&home.public_dir().join("bicdb.ini")), &[])
                .expect("参数");
        bicdb_cli::boot::open_instance(&params).expect("开")
    };
    {
        let gcf = open_global_ctl(&home).expect("注册表");
        assert_eq!(gcf.fs_members().unwrap().len(), 2);
        gcf.close().unwrap();
    }
    let listed = bicdb_catalog::dcl::list_fs(&mut inst.catalog).expect("fs$");
    assert_eq!(listed.len(), 3, "三行都在（含墓碑）");
    assert_eq!(
        listed
            .iter()
            .filter(|(_, e)| e.status == bicdb_catalog::dcl::fs_status::IN_POOL)
            .count(),
        2
    );

    // 管理面之外的工作区：DCL 具名拒绝（"资格检查先于对象查找"）。
    let mut inst2 = inst;
    let shop_dir = holder.path().join("shop");
    {
        // 换一个非 public 的实例根跑一句 DCL（用同一把会话 API）。
        let params = InstanceParams::for_init(&shop_dir, None, &[]).expect("参数");
        let mut shop = create_instance(&params, None).expect("建 shop");
        let seq = shop.seq();
        {
            let mut session = Session::new(shop.pool, shop.engine, &mut shop.catalog, seq);
            session.set_dcl_context(Some(home.root.clone()), Some(shop.io));
            let e = session
                .execute(&format!("CREATE FILESYSTEM x USING '{}'", d3.display()))
                .unwrap_err()
                .to_string();
            assert!(e.contains("PUBLIC"), "非管理面要具名拒绝：{e}");
        }
        shop.shutdown().expect("关 shop");
    }
    inst2.shutdown().expect("关");
    let _ = QueryResult::Ddl(String::new());
}

/// **W 组：工作区生命周期**（`CREATE/ALTER/DROP WORKSPACE`）——三步协议端到端。
#[test]
fn workspace_lifecycle_round_trip() {
    let holder = TempDir::new("ws_home");
    let home = home_of(holder.path());
    let params = InstanceParams::for_init(&home.public_dir(), None, &[]).expect("参数");
    let mut inst = create_instance(&params, Some(&home)).expect("建 public");

    // 池非空是一切的前提（依赖顺序 FS → WORKSPACE）。
    let e = run(&mut inst, &home, "CREATE WORKSPACE w1").unwrap_err();
    assert!(e.contains("先 `CREATE FILESYSTEM`"), "{e}");

    let d1 = holder.path().join("data1");
    std::fs::create_dir_all(&d1).unwrap();
    run(
        &mut inst,
        &home,
        &format!("CREATE FILESYSTEM data1 USING '{}'", d1.display()),
    )
    .expect("建盘");

    // 建工作区：①文件 ②ws$（无主）③注册表（最后）。
    let out = run(
        &mut inst,
        &home,
        "CREATE WORKSPACE w1 DEFAULT FILESYSTEM data1 QUOTA 4096 ON FILESYSTEM data1",
    )
    .expect("建工作区");
    assert!(out.contains("无主容器"), "{out}");

    let root = holder.path().join("w1");
    // ① 文件面：能独立起一个实例（字典/撤销/控制文件/日志齐备）。
    assert!(root.join("bicdb.ini").is_file());
    assert!(root.join("control/control01.ctl").is_file());
    let meta = std::fs::read_dir(root.join("data"))
        .unwrap()
        .flatten()
        .any(|e| e.file_name().to_string_lossy().ends_with("_meta"));
    assert!(meta, "data/ 下有 file 0（`*_meta`）");
    // ② 属性：`ws$` 无主 + 配额。
    {
        let w = bicdb_catalog::dcl::ws_by_name(&mut inst.catalog, "w1")
            .unwrap()
            .expect("ws$ 在册");
        assert_eq!(w.user_id, None);
        assert_eq!(w.default_fs, Some(1));
        let wq = bicdb_catalog::dcl::list_wq(&mut inst.catalog).unwrap();
        assert_eq!(wq.len(), 1);
        assert_eq!(wq[0].1.quota_bytes, 4096);
    }
    // ③ 注册表：在册、根目录对得上。
    {
        let gcf = open_global_ctl(&home).expect("注册表");
        let ws = gcf.workspaces().expect("在册清单");
        assert_eq!(ws.len(), 2, "public + w1");
        let w1 = ws
            .iter()
            .find(|r| r.workspace_id.as_raw() == 2)
            .expect("w1 = 2");
        assert_eq!(
            String::from_utf8_lossy(&w1.root).as_ref(),
            std::fs::canonicalize(&root).unwrap().display().to_string()
        );
        gcf.close().unwrap();
    }

    // 独立开起来：建表/写/读（工作区真的是"自己的库"）。
    {
        let (p, _) =
            bicdb_cli::boot::instance_params(Some(&root.join("bicdb.ini")), &[]).expect("参数");
        let mut w1 = bicdb_cli::boot::open_instance(&p).expect("开 w1");
        {
            let seq = w1.seq();
            let mut s = Session::new(w1.pool, w1.engine, &mut w1.catalog, seq);
            s.execute("CREATE TABLE t (id NUMBER NOT NULL)")
                .expect("建表");
            s.execute("INSERT INTO t VALUES (7)").expect("插");
            assert!(s.execute("SELECT id FROM t").is_ok());
        }
        w1.shutdown().expect("关");
    }

    // 名字唯一：同名再建 ⇒ 拒绝。
    let e = run(&mut inst, &home, "CREATE WORKSPACE w1").unwrap_err();
    assert!(e.contains("已在册"), "{e}");

    // ALTER：加盘（wq$ 追加一行）、默认盘、角色配额、改名。
    run(
        &mut inst,
        &home,
        "ALTER WORKSPACE w1 ADD FILESYSTEM data1 QUOTA UNLIMITED ON FILESYSTEM data1",
    )
    .expect("加盘");
    run(
        &mut inst,
        &home,
        "ALTER WORKSPACE w1 SET DEFAULT FILESYSTEM data1",
    )
    .expect("默认盘");
    run(
        &mut inst,
        &home,
        "ALTER WORKSPACE w1 SET QUOTA (data = 1, undo = 2, temp = 3, asset = 4)",
    )
    .expect("角色配额");
    run(&mut inst, &home, "ALTER WORKSPACE w1 SET NAME = 'w1x'").expect("改名");
    {
        let w = bicdb_catalog::dcl::ws_by_name(&mut inst.catalog, "w1x")
            .unwrap()
            .expect("改名后按新名可查");
        assert_eq!(w.quota, [1, 2, 3, 4]);
        assert!(bicdb_catalog::dcl::ws_by_name(&mut inst.catalog, "w1")
            .unwrap()
            .is_none());
        assert!(root.is_dir(), "改名**不动目录名**（根位置是注册表的事）");
    }

    // 保留区：`public` 不能删。
    let e = run(&mut inst, &home, "DROP WORKSPACE public").unwrap_err();
    assert!(e.contains("保留工作区"), "{e}");

    // DROP：反向三步（可见性先断 → ws$ 墓碑 → 目录清空）。
    let out = run(&mut inst, &home, "DROP WORKSPACE w1x").expect("删");
    assert!(out.contains("不可逆"), "{out}");
    assert!(!root.exists(), "目录已清空");
    {
        let gcf = open_global_ctl(&home).expect("注册表");
        assert_eq!(gcf.workspaces().unwrap().len(), 1, "在册只剩 public");
        assert_eq!(
            gcf.workspace_by_id(bicdb_workspace::WorkspaceId::from_raw(2).unwrap())
                .unwrap()
                .unwrap()
                .status,
            bicdb_storage::globalctl::WS_DROPPED
        );
        gcf.close().unwrap();
    }
    assert_eq!(
        bicdb_catalog::dcl::ws_by_name(&mut inst.catalog, "w1x")
            .unwrap()
            .unwrap()
            .status,
        bicdb_catalog::dcl::ws_status::DROPPED,
        "ws$ 留墓碑"
    );

    inst.shutdown().expect("关");
}

/// **U 组：用户生命周期**（`CREATE/ALTER/DROP USER`）——含 admin 三条硬规则的落点。
#[test]
fn user_lifecycle_round_trip() {
    let holder = TempDir::new("user_home");
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
    run(&mut inst, &home, "CREATE WORKSPACE w2").expect("建区 2");

    // U1：建主体 + 绑区（**一次到位**）。
    let out = run(
        &mut inst,
        &home,
        "CREATE USER alice IDENTIFIED BY 'pw-one' USING WORKSPACE w1",
    )
    .expect("建用户");
    assert!(out.contains("主体号 1"), "{out}");
    {
        let u = bicdb_catalog::dcl::user_by_name(&mut inst.catalog, "alice")
            .unwrap()
            .expect("user$ 在册");
        assert_eq!(u.status, bicdb_catalog::dcl::user_status::ACTIVE);
        // **口令散列只在认证路径可读**，且形态自描述。
        let hash = bicdb_catalog::dcl::password_hash(&mut inst.catalog, u.user_id).unwrap();
        assert!(hash.starts_with("pbkdf2-sha512$1000$"), "{hash}");
        assert!(bicdb_common::pbkdf2::verify_password("pw-one", &hash));
        assert!(!bicdb_common::pbkdf2::verify_password("wrong", &hash));
        // 绑定落到了 ws$。
        let w1 = bicdb_catalog::dcl::ws_by_name(&mut inst.catalog, "w1")
            .unwrap()
            .unwrap();
        assert_eq!(w1.user_id, Some(u.user_id));
    }

    // 别人的工作区没有入口（A 规则的语句面）。
    let e = run(
        &mut inst,
        &home,
        "CREATE USER mallory IDENTIFIED BY 'x' USING WORKSPACE w1",
    )
    .unwrap_err();
    assert!(e.contains("别人的工作区没有入口"), "{e}");

    // U2：admin 重置（含 EXPIRE）——旧口令即时失效。
    run(
        &mut inst,
        &home,
        "ALTER USER alice IDENTIFIED BY 'pw-two' EXPIRE",
    )
    .expect("重置");
    {
        let u = bicdb_catalog::dcl::user_by_name(&mut inst.catalog, "alice")
            .unwrap()
            .unwrap();
        assert_eq!(u.status, bicdb_catalog::dcl::user_status::EXPIRED);
        let hash = bicdb_catalog::dcl::password_hash(&mut inst.catalog, u.user_id).unwrap();
        assert!(bicdb_common::pbkdf2::verify_password("pw-two", &hash));
        assert!(
            !bicdb_common::pbkdf2::verify_password("pw-one", &hash),
            "旧口令失效"
        );
    }

    // U3：本人改密要求旧口令（本版无会话身份，但旧口令**可核验**）。
    let e = run(
        &mut inst,
        &home,
        "ALTER USER alice IDENTIFIED BY 'pw-three' REPLACE 'nope'",
    )
    .unwrap_err();
    assert!(e.contains("旧口令不符"), "{e}");
    run(
        &mut inst,
        &home,
        "ALTER USER alice IDENTIFIED BY 'pw-three' REPLACE 'pw-two'",
    )
    .expect("换口令");
    {
        let u = bicdb_catalog::dcl::user_by_name(&mut inst.catalog, "alice")
            .unwrap()
            .unwrap();
        assert_eq!(
            u.status,
            bicdb_catalog::dcl::user_status::ACTIVE,
            "换口令把 EXPIRE 清掉"
        );
        let hash = bicdb_catalog::dcl::password_hash(&mut inst.catalog, u.user_id).unwrap();
        assert!(bicdb_common::pbkdf2::verify_password("pw-three", &hash));
    }

    // U4：暂停 / 恢复。
    run(&mut inst, &home, "ALTER USER alice PAUSE").expect("暂停");
    assert_eq!(
        bicdb_catalog::dcl::user_by_name(&mut inst.catalog, "alice")
            .unwrap()
            .unwrap()
            .status,
        bicdb_catalog::dcl::user_status::PAUSED
    );
    run(&mut inst, &home, "ALTER USER alice RESUME").expect("恢复");

    // U5/U6：加绑与解绑（最后一个不许解）。
    run(&mut inst, &home, "ALTER USER alice USING WORKSPACE w2").expect("加绑");
    let e = run(&mut inst, &home, "ALTER USER alice USING WORKSPACE w2").unwrap_err();
    assert!(e.contains("本来就属于"), "{e}");
    run(&mut inst, &home, "ALTER USER alice DROP WORKSPACE w2").expect("解绑");
    let e = run(&mut inst, &home, "ALTER USER alice DROP WORKSPACE w1").unwrap_err();
    assert!(e.contains("只剩这一个工作区"), "{e}");

    // U7：删用户（有区无 CASCADE ⇒ 拒绝并列出；CASCADE ⇒ 连区一起删）。
    let e = run(&mut inst, &home, "DROP USER alice").unwrap_err();
    assert!(e.contains("名下还有 1 个工作区"), "{e}");
    let out = run(&mut inst, &home, "DROP USER alice CASCADE").expect("级联删");
    assert!(out.contains("CASCADE"), "{out}");
    assert!(
        bicdb_catalog::dcl::user_by_name(&mut inst.catalog, "alice")
            .unwrap()
            .is_none(),
        "主体行真删（名字可再用）"
    );
    assert!(!holder.path().join("w1").exists(), "CASCADE 连区目录一起删");
    {
        let gcf = open_global_ctl(&home).expect("注册表");
        // CASCADE 只删**他名下的**区：`w2` 早已解绑（无主），仍在册。
        let names: Vec<u64> = gcf
            .workspaces()
            .unwrap()
            .iter()
            .map(|r| r.workspace_id.as_raw())
            .collect();
        assert_eq!(names, vec![1, 3], "public + w2（w1 = 2 已成墓碑）");
        gcf.close().unwrap();
    }

    inst.shutdown().expect("关");
}

/// Graph initialization copies definitions and policy; graph/index identities,
/// stale postings, source counters and source event data must be fresh.
#[test]
fn graph_schema_templates_clone_index_policy_and_empty_state_without_source_data() {
    use bicdb_exec::Value;
    fn query(inst: &mut bicdb_cli::boot::Instance, sql: &str) -> Vec<Vec<Value>> {
        let seq = inst.seq();
        let mut session = Session::new(inst.pool, inst.engine, &mut inst.catalog, seq);
        let result = session
            .execute(sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
        let Some(QueryResult::Rows { rows, .. }) = result.last() else {
            panic!("{sql}: rows required")
        };
        rows.clone()
    }
    fn without_identity(rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
        let mut rows: Vec<Vec<Value>> = rows
            .into_iter()
            .map(|r| {
                r.into_iter()
                    .enumerate()
                    .filter_map(|(i, v)| (i != 1).then_some(v))
                    .collect()
            })
            .collect();
        rows.sort_by(|a, b| match (&a[0], &b[0]) {
            (Value::Bytes(a), Value::Bytes(b)) => a.cmp(b),
            _ => panic!("index names must be strings"),
        });
        rows
    }
    let holder = TempDir::new("graph-schema-template");
    let home = home_of(holder.path());
    let params = InstanceParams::for_init(&home.public_dir(), None, &[]).unwrap();
    let mut public = create_instance(&params, Some(&home)).unwrap();
    let disk = home.root.join("disk");
    std::fs::create_dir(&disk).unwrap();
    run(
        &mut public,
        &home,
        &format!("CREATE FILESYSTEM disk USING '{}'", disk.display()),
    )
    .unwrap();
    run(&mut public, &home, "CREATE WORKSPACE graph_seed").unwrap();
    let seed_ini = home.root.join("graph_seed/bicdb.ini");
    let (params, _) = bicdb_cli::boot::instance_params(Some(&seed_ini), &[]).unwrap();
    let mut seed = bicdb_cli::boot::open_instance(&params).unwrap();
    run(&mut seed,&home,r#"CREATE TABLE memo (id NUMBER); CREATE UNIQUE INDEX memo_id ON memo(id);
      CREATE GRAPH "MixedGraph"; CREATE GRAPH g2;
      CREATE UNIQUE GRAPH INDEX node_key ON "MixedGraph" NODES LABEL "Entity" (config."Tenant",name);
      CREATE GRAPH INDEX label_tree ON "MixedGraph" NODES LABEL "Entity" ();
      CREATE GRAPH INDEX link_weight ON "MixedGraph" RELATIONSHIPS TYPE "LINK" (weight);
      CYPHER "MixedGraph" 'CREATE (:Entity {db_type:"d1",name:"DO_NOT_COPY",config:{Tenant:"d1"}})';
      CREATE FULLTEXT GRAPH INDEX node_words ON "MixedGraph" NODES LABEL ("Entity","Fault") (name,config."Alias"[0]."Question","literal.dot") OPTIONS '{"update":"batch","interval_ms":1000,"batch_rows":7}';
      ALTER FULLTEXT GRAPH INDEX node_words ON "MixedGraph" PAUSE;
      CREATE FULLTEXT GRAPH INDEX edge_words ON "MixedGraph" RELATIONSHIPS TYPE ("LINK","Depends") (config."Question"[1],"Purpose") OPTIONS '{"update":"manual","batch_rows":3}';
      CREATE FULLTEXT GRAPH INDEX inherited_words ON g2 NODES (name) OPTIONS '{"update":"batch"}';
      CYPHER "MixedGraph" 'MATCH (n) DELETE n';"#).unwrap();
    let property = without_identity(query(&mut seed, r#"SHOW GRAPH INDEXES ON "MixedGraph""#));
    let source = query(&mut seed, r#"SHOW FULLTEXT GRAPH INDEXES ON "MixedGraph""#);
    assert!(
        source
            .iter()
            .any(|r| r[8] == Value::Number(bicdb_types::Number::parse("1").unwrap())),
        "source contains stale derived document, not live graph data"
    );
    assert!(run(
        &mut public,
        &home,
        "ALTER DATABASE ADD TEMPLATE 'busy_graph' FROM graph_seed"
    )
    .is_err());
    assert!(!home.root.join("templates/busy_graph").exists());
    seed.shutdown().unwrap();
    drop(seed);
    run(
        &mut public,
        &home,
        "ALTER DATABASE ADD TEMPLATE 'graph_init' FROM graph_seed",
    )
    .unwrap();
    let script =
        std::fs::read_to_string(home.root.join("templates/graph_init/schema.sql")).unwrap();
    assert!(script.contains("CREATE GRAPH \"MixedGraph\""));
    assert!(script.contains("CREATE FULLTEXT GRAPH INDEX"));
    assert!(script.contains("PAUSE"));
    assert!(!script.contains("DO_NOT_COPY"));
    assert!(!script.contains("i_graph_"));
    // A correct checksum does not authorize executable data or foreign targets.
    let manifest_path = home.root.join("templates/graph_init/manifest");
    let original_manifest = std::fs::read_to_string(&manifest_path).unwrap();
    let script_path = home.root.join("templates/graph_init/schema.sql");
    for (i, forged) in [
        "CREATE GRAPH kg; CYPHER kg 'CREATE (:Stolen)';",
        "CREATE GRAPH kg; CREATE GRAPH INDEX ix ON foreign_graph NODES (name);",
        "CREATE GRAPH kg; ALTER FULLTEXT GRAPH INDEX foreign_index ON kg PAUSE;",
        "CREATE GRAPH kg; DROP GRAPH kg;",
    ]
    .iter()
    .enumerate()
    {
        let digest = bicdb_common::sha256::digest(forged.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        std::fs::write(&script_path, forged).unwrap();
        std::fs::write(
            &manifest_path,
            format!("BICDB_SCHEMA_TEMPLATE_V1\ngraph_init\n{digest}\n"),
        )
        .unwrap();
        let name = format!("forged_copy_{i}");
        assert!(run(
            &mut public,
            &home,
            &format!("CREATE WORKSPACE {name} FROM TEMPLATE 'graph_init'")
        )
        .is_err());
        assert!(
            !home.root.join(&name).exists(),
            "validation precedes target file creation"
        );
        assert!(query(
            &mut public,
            &format!("SELECT name FROM ws$ WHERE name='{name}'")
        )
        .is_empty());
    }
    std::fs::write(&script_path, &script).unwrap();
    std::fs::write(&manifest_path, original_manifest).unwrap();
    for name in ["copy_a", "copy_b"] {
        run(
            &mut public,
            &home,
            &format!("CREATE WORKSPACE {name} FROM TEMPLATE 'graph_init'"),
        )
        .unwrap();
        let (params, _) =
            bicdb_cli::boot::instance_params(Some(&home.root.join(name).join("bicdb.ini")), &[])
                .unwrap();
        let mut cloned = bicdb_cli::boot::open_instance(&params).unwrap();
        assert_eq!(
            without_identity(query(&mut cloned, r#"SHOW GRAPH INDEXES ON "MixedGraph""#)),
            property
        );
        let ft = query(
            &mut cloned,
            r#"SHOW FULLTEXT GRAPH INDEXES ON "MixedGraph""#,
        );
        assert_eq!(ft.len(), source.len());
        for row in &ft {
            let original = source.iter().find(|r| r[0] == row[0]).unwrap();
            for i in [2, 3, 4, 5, 14, 15, 16, 17] {
                assert_eq!(
                    row[i], original[i],
                    "preserve definition and policy at column {i}"
                );
            }
            for i in [8, 10, 11, 12, 13] {
                assert_eq!(
                    row[i],
                    Value::Number(bicdb_types::Number::parse("0").unwrap()),
                    "fresh graph/index state"
                );
            }
            assert_eq!(row[18], Value::Bytes(b"v4".to_vec()));
            if row[0] == Value::Bytes(b"node_words".to_vec()) {
                assert_eq!(row[9], Value::Bytes(b"PAUSED".to_vec()));
            }
        }
        assert_eq!(query(&mut cloned, "SHOW GRAPHS").len(), 2);
        assert_eq!(
            query(
                &mut cloned,
                r#"CYPHER "MixedGraph" 'MATCH (n) RETURN count(*) AS n'"#
            )[0][0],
            Value::Number(bicdb_types::Number::parse("0").unwrap())
        );
        assert!(query(&mut cloned, "SELECT * FROM memo").is_empty());
        if name == "copy_a" {
            run(&mut cloned,&home,r#"CYPHER "MixedGraph" 'CREATE (:Entity {db_type:"d1",name:"clone_word",config:{Tenant:"d1"}})'; INSERT INTO memo VALUES (1); ALTER FULLTEXT GRAPH INDEX node_words ON "MixedGraph" RESUME; ALTER FULLTEXT GRAPH INDEX node_words ON "MixedGraph" WAIT;"#).unwrap();
            assert_eq!(query(&mut cloned,r#"SEARCH FULLTEXT GRAPH INDEX node_words ON "MixedGraph" FOR 'clone_word' OPTIONS '{"db_type":"d1","consistency":"eventual"}'"#).len(),1);
            assert!(run(&mut cloned,&home,r#"CYPHER "MixedGraph" 'CREATE (:Entity {name:"clone_word",config:{Tenant:"d1"}})'"#).is_err(),"unique property tree remains functional");
        }
        cloned.shutdown().unwrap();
        drop(cloned);
    }
    let mut seed = bicdb_cli::boot::open_instance(&params).unwrap();
    run(
        &mut seed,
        &home,
        r#"CYPHER "MixedGraph" 'CREATE (:Entity {name:"live_source"})'"#,
    )
    .unwrap();
    seed.shutdown().unwrap();
    drop(seed);
    assert!(run(
        &mut public,
        &home,
        "ALTER DATABASE ADD TEMPLATE 'nonempty_graph' FROM graph_seed"
    )
    .unwrap_err()
    .contains("非空"));
    assert!(!home.root.join("templates/nonempty_graph").exists());
    run(
        &mut public,
        &home,
        "ALTER DATABASE DROP TEMPLATE 'graph_init'",
    )
    .unwrap();
    assert!(home.root.join("copy_a/bicdb.ini").exists());
    assert!(home.root.join("copy_b/bicdb.ini").exists());
    // A graph-only schema is a valid initialization template.
    run(&mut public, &home, "CREATE WORKSPACE graph_only").unwrap();
    let (graph_params, _) =
        bicdb_cli::boot::instance_params(Some(&home.root.join("graph_only/bicdb.ini")), &[])
            .unwrap();
    {
        let mut graph_only = bicdb_cli::boot::open_instance(&graph_params).unwrap();
        run(&mut graph_only, &home, "CREATE GRAPH empty_graph").unwrap();
        graph_only.shutdown().unwrap();
    }
    run(
        &mut public,
        &home,
        "ALTER DATABASE ADD TEMPLATE 'graph_only' FROM graph_only",
    )
    .unwrap();
    run(
        &mut public,
        &home,
        "CREATE WORKSPACE graph_only_copy FROM TEMPLATE 'graph_only'",
    )
    .unwrap();
    let (graph_params, _) =
        bicdb_cli::boot::instance_params(Some(&home.root.join("graph_only_copy/bicdb.ini")), &[])
            .unwrap();
    {
        let mut graph_only = bicdb_cli::boot::open_instance(&graph_params).unwrap();
        assert_eq!(query(&mut graph_only, "SHOW GRAPHS").len(), 1);
        assert_eq!(
            query(
                &mut graph_only,
                "CYPHER empty_graph 'MATCH (n) RETURN count(*)'"
            )[0][0],
            Value::Number(bicdb_types::Number::parse("0").unwrap())
        );
        graph_only.shutdown().unwrap();
    }
    public.shutdown().unwrap();
}
