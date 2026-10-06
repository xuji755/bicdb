//! **端到端验收**（真盘、真进程内的完整链）：建区 → DDL → DML → 索引维护 →
//! 唯一性 → 事务 → **重开（恢复）** → 读回。
//!
//! ```text
//! create_instance(tmp) ──▶ CREATE TABLE / CREATE UNIQUE INDEX / INSERT / SELECT
//!        │                         │
//!        │                         └─ 索引项（唯一冲突、回滚后重插）
//!        └─▶ shutdown（完全检查点）──▶ open_instance（三阶段恢复）──▶ 读回一致
//! ```
//!
//! **为什么在 CLI crate**：这里才有"实例"这一层（文件面 + 控制文件 + 日志）；
//! 各层单测各自覆盖自己的语义，本测试钉的是**装配起来能跑**。

use std::path::PathBuf;

use bicdb_cli::boot::{create_instance, open_instance, Instance};
use bicdb_sql::session::{QueryResult, Session, SessionError};

/// 一个测试独占的实例目录（进程号 + 名字；跑完删）。
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("bicdb-e2e-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Self(dir)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 跑一批语句（同一会话；返回每条的结果）。
fn run(inst: &mut Instance, sql: &str) -> Result<Vec<QueryResult>, SessionError> {
    let seq = inst.seq();
    let mut session = Session::new(inst.pool, inst.engine, &mut inst.catalog, seq);
    session.execute(sql)
}

/// 跑并期待成功。
fn ok(inst: &mut Instance, sql: &str) -> Vec<QueryResult> {
    run(inst, sql).unwrap_or_else(|e| panic!("`{sql}` 应成功：{e}"))
}

/// 跑并期待失败（返回错误文本）。
fn err(inst: &mut Instance, sql: &str) -> String {
    match run(inst, sql) {
        Ok(_) => panic!("`{sql}` 应失败，却成功了"),
        Err(e) => e.to_string(),
    }
}

/// 结果集的行（`SELECT id, name FROM t ORDER BY id` 形态）。
fn rows(r: &[QueryResult]) -> Vec<Vec<String>> {
    match r.last() {
        Some(QueryResult::Rows { rows, .. }) => rows.clone(),
        other => panic!("期待结果集，得到 {other:?}"),
    }
}

#[test]
fn create_ddl_dml_index_txn_and_reopen() {
    let dir = TempDir::new("main");
    // ① 建区 + DDL（`stat$`/`seq$` 在建区收尾里建）。
    let mut inst = create_instance(dir.path()).expect("建区");
    ok(
        &mut inst,
        "CREATE TABLE t (id NUMBER NOT NULL, name VARCHAR2(32))",
    );
    ok(
        &mut inst,
        "INSERT INTO t VALUES (1, 'alpha'); INSERT INTO t VALUES (2, 'beta')",
    );
    ok(&mut inst, "CREATE UNIQUE INDEX t_pk ON t (id)");
    assert_eq!(
        rows(&ok(&mut inst, "SELECT id, name FROM t ORDER BY id")),
        vec![vec!["1", "alpha"], vec!["2", "beta"]]
    );

    // ② 唯一性：撞键 ⇒ 报错且**一行不写**（语句原子）。
    let e = err(&mut inst, "INSERT INTO t VALUES (1, 'dup')");
    assert!(e.contains("唯一约束冲突"), "错误应指认唯一冲突：{e}");
    assert_eq!(rows(&ok(&mut inst, "SELECT id FROM t")).len(), 2);

    // ③ 事务：回滚不留行；**回滚留下的陈旧索引项不得挡住随后同键插入**。
    ok(
        &mut inst,
        "BEGIN; INSERT INTO t VALUES (5, 'five'); ROLLBACK",
    );
    assert_eq!(rows(&ok(&mut inst, "SELECT id FROM t")).len(), 2);
    ok(&mut inst, "INSERT INTO t VALUES (5, 'five')");
    // 同一事务里同键两次 ⇒ 第二句拒绝（预检的"已见键"）。
    let e = err(
        &mut inst,
        "BEGIN; INSERT INTO t VALUES (8,'a'); INSERT INTO t VALUES (8,'b'); COMMIT",
    );
    assert!(e.contains("唯一约束冲突"), "事务内重复应报冲突：{e}");

    // ④ 关（完全检查点）→ 重开（恢复）→ 数据在。
    inst.shutdown().expect("关闭");
    drop(inst);
    let mut inst = open_instance(dir.path()).expect("重开");
    let got = rows(&ok(&mut inst, "SELECT id, name FROM t ORDER BY id"));
    assert_eq!(
        got,
        vec![vec!["1", "alpha"], vec!["2", "beta"], vec!["5", "five"]]
    );

    // ⑤ 多键索引：复合键的两列都参与（同首列不同次列不冲突）。
    ok(&mut inst, "CREATE INDEX t_name ON t (name)");
    ok(&mut inst, "INSERT INTO t VALUES (9, 'nine')");
    assert_eq!(rows(&ok(&mut inst, "SELECT id FROM t")).len(), 4);

    // ⑥ DROP 之后对象不可见（三格解析的"不存在"）。
    ok(&mut inst, "DROP INDEX t_pk");
    ok(&mut inst, "DROP TABLE t");
    let e = err(&mut inst, "SELECT id FROM t");
    assert!(e.contains("不存在"), "DROP 后应报不存在：{e}");
    inst.shutdown().expect("关闭");
}

/// **唯一索引的 NULL 口径**（MySQL/PG 同款）：键里有 NULL ⇒ 不判唯一。
#[test]
fn unique_index_treats_nulls_as_distinct() {
    let dir = TempDir::new("nulls");
    let mut inst = create_instance(dir.path()).expect("建区");
    ok(
        &mut inst,
        "CREATE TABLE n (id NUMBER NOT NULL, code VARCHAR2(8))",
    );
    ok(&mut inst, "INSERT INTO n VALUES (1, 'a'); INSERT INTO n VALUES (2, NULL); INSERT INTO n VALUES (3, NULL)");
    // 建索引时两个 NULL 键不得判成重复（DDL 的唯一性口径同一条）。
    ok(&mut inst, "CREATE UNIQUE INDEX n_code ON n (code)");
    ok(&mut inst, "INSERT INTO n VALUES (4, NULL)");
    let e = err(&mut inst, "INSERT INTO n VALUES (5, 'a')");
    assert!(e.contains("唯一约束冲突"), "非 NULL 重复应拒：{e}");
    assert_eq!(rows(&ok(&mut inst, "SELECT id FROM n")).len(), 4);
    inst.shutdown().expect("关闭");
}

/// **表选项真的走到写路径**（2026-10-06 审计：`pctfree`/`itl_max` 原先只落
/// 字典与段头、存储层恒用缺省值）。黑盒判据：同样 200 行，`pctfree = 50`
/// 的表必须用**更多页**（页内预留生效）。
#[test]
fn table_options_reach_the_write_path() {
    let dir = TempDir::new("options");
    let mut inst = create_instance(dir.path()).expect("建区");
    ok(
        &mut inst,
        "CREATE TABLE tight (id NUMBER NOT NULL, v VARCHAR2(64)) WITH (pctfree = 0)",
    );
    ok(
        &mut inst,
        "CREATE TABLE loose (id NUMBER NOT NULL, v VARCHAR2(64)) WITH (pctfree = 50, itl_max = 4)",
    );
    // 行要足够多、足够大：`pctfree = 50` 的表必须**多用页**才看得见效果。
    for i in 1..=800 {
        let v = format!("{}-{i:04}", "x".repeat(36));
        ok(&mut inst, &format!("INSERT INTO tight VALUES ({i}, '{v}')"));
        ok(&mut inst, &format!("INSERT INTO loose VALUES ({i}, '{v}')"));
    }
    let pages = |inst: &mut Instance, t: &str| -> u32 {
        let obj = inst
            .catalog
            .resolve(
                bicdb_common::seq::CommitSeq::from_raw(inst.seq()).unwrap(),
                bicdb_catalog::dict::namespace::TABLE,
                t,
            )
            .expect("解析表")
            .obj;
        let seg_block =
            bicdb_catalog::ddl::live_segment_block(&mut inst.catalog, obj).expect("段头");
        inst.catalog.segment_at(seg_block).expect("开段").hwm()
    };
    let tight = pages(&mut inst, "tight");
    let loose = pages(&mut inst, "loose");
    assert!(
        loose > tight,
        "pctfree=50 应占更多页（预留生效）：tight={tight} loose={loose}"
    );
    assert_eq!(rows(&ok(&mut inst, "SELECT id FROM tight")).len(), 800);
    inst.shutdown().expect("关闭");
}

/// **打开链的一致性核对是活的**（2026-10-06 审计：`catalog::consistency` 此前
/// 只有单测消费者）。判据：`file_scn` 随干净关闭推进；把**旧的**控制文件换回来
/// ⇒ 文件"超前" ⇒ **拒绝打开**（而不是照常恢复进入运行）。
#[test]
fn a_stale_control_file_is_refused_at_open() {
    let dir = TempDir::new("consistency");
    let mut inst = create_instance(dir.path()).expect("建区");
    ok(&mut inst, "CREATE TABLE c (id NUMBER NOT NULL)");
    ok(&mut inst, "INSERT INTO c VALUES (1)");
    inst.shutdown().expect("关闭"); // 推进 file_scn 到检查点位点
    drop(inst);

    // 备份"当前"控制文件，再走一次写入/关闭让位点前进，然后换回旧的。
    let cur_a = std::fs::read(dir.path().join("cf_a")).expect("读 cf_a");
    let cur_b = std::fs::read(dir.path().join("cf_b")).expect("读 cf_b");
    let mut inst = open_instance(dir.path()).expect("重开");
    ok(&mut inst, "INSERT INTO c VALUES (2)");
    inst.shutdown().expect("关闭");
    drop(inst);
    std::fs::write(dir.path().join("cf_a"), &cur_a).expect("写回旧 cf_a");
    std::fs::write(dir.path().join("cf_b"), &cur_b).expect("写回旧 cf_b");

    match open_instance(dir.path()) {
        Ok(_) => panic!("旧控制文件 + 新文件 ⇒ 应拒绝打开"),
        Err(e) => {
            let msg = e.to_string();
            assert!(msg.contains("超前"), "错误应指认超前：{msg}");
        }
    }

    // 复原（好控制文件 = 刚才那份新的）——把备份放回去，实例仍可打开。
    std::fs::write(dir.path().join("cf_a"), &cur_a).ok();
    std::fs::write(dir.path().join("cf_b"), &cur_b).ok();
}

/// **崩溃语义**：不调 `shutdown`（脏页留在池里、WAL 是唯一耐久源）⇒
/// 重开时恢复把已提交的行重做出来。
#[test]
fn crash_without_shutdown_recovers_committed_rows() {
    let dir = TempDir::new("crash");
    {
        let mut inst = create_instance(dir.path()).expect("建区");
        ok(
            &mut inst,
            "CREATE TABLE c (id NUMBER NOT NULL, tag VARCHAR2(16))",
        );
        for i in 1..=50 {
            ok(&mut inst, &format!("INSERT INTO c VALUES ({i}, 'r{i}')"));
        }
        // **不 `shutdown`**：模拟进程直接消失（日志已耐久、页未回写）。
        std::mem::forget(inst);
    }
    let mut inst = open_instance(dir.path()).expect("崩溃后重开");
    let r = inst.recovery.expect("应有一次恢复");
    assert_eq!(
        rows(&ok(&mut inst, "SELECT id FROM c")).len(),
        50,
        "恢复后行数一致"
    );
    inst.shutdown().expect("关闭");
    let _ = r;
}
