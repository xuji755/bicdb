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
use bicdb_cli::config::InstanceParams;
use bicdb_exec::Value;
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
    // **固定表的内容源**（`file$` ← 控制文件）——与真装配同一份接线。
    session.set_fixed_table_source(Some(bicdb_cli::fixed::CliFixedTables::new_static(
        &inst.dir, inst.io,
    )));
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

/// 建区/打开用的参数（**就是 `bicdb init` 那一步做的事**：默认 + 根区目录；
/// `create_instance` 会把参数文件写到 `<db_root>/bicdb.ini`）。
fn params_for(dir: &std::path::Path) -> InstanceParams {
    InstanceParams::for_init(dir, None, &[]).expect("参数")
}

/// 结果集的行（`SELECT id, name FROM t ORDER BY id` 形态；值的**显示文本**——
/// 断言写起来最直观，形态（`ColKind`）另有断言）。
fn rows(r: &[QueryResult]) -> Vec<Vec<String>> {
    match r.last() {
        Some(QueryResult::Rows { rows, .. }) => rows
            .iter()
            .map(|row| row.iter().map(bicdb_sql::session::format_value).collect())
            .collect(),
        other => panic!("期待结果集，得到 {other:?}"),
    }
}

#[test]
fn create_ddl_dml_index_txn_and_reopen() {
    let dir = TempDir::new("main");
    // ① 建区 + DDL（`stat$`/`seq$` 在建区收尾里建）。
    let mut inst = create_instance(&params_for(dir.path()), None).expect("建区");
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
    let mut inst = open_instance(&params_for(dir.path())).expect("重开");
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
    let mut inst = create_instance(&params_for(dir.path()), None).expect("建区");
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
    let mut inst = create_instance(&params_for(dir.path()), None).expect("建区");
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
    let mut inst = create_instance(&params_for(dir.path()), None).expect("建区");
    ok(&mut inst, "CREATE TABLE c (id NUMBER NOT NULL)");
    ok(&mut inst, "INSERT INTO c VALUES (1)");
    inst.shutdown().expect("关闭"); // 推进 file_scn 到检查点位点
    drop(inst);

    // 备份"当前"控制文件，再走一次写入/关闭让位点前进，然后换回旧的。
    let cur_a = std::fs::read(dir.path().join("control/control01.ctl")).expect("读 cf_a");
    let cur_b = std::fs::read(dir.path().join("control/control02.ctl")).expect("读 cf_b");
    let mut inst = open_instance(&params_for(dir.path())).expect("重开");
    ok(&mut inst, "INSERT INTO c VALUES (2)");
    inst.shutdown().expect("关闭");
    drop(inst);
    std::fs::write(dir.path().join("control/control01.ctl"), &cur_a).expect("写回旧 cf_a");
    std::fs::write(dir.path().join("control/control02.ctl"), &cur_b).expect("写回旧 cf_b");

    match open_instance(&params_for(dir.path())) {
        Ok(_) => panic!("旧控制文件 + 新文件 ⇒ 应拒绝打开"),
        Err(e) => {
            let msg = e.to_string();
            assert!(msg.contains("超前"), "错误应指认超前：{msg}");
        }
    }

    // 复原（好控制文件 = 刚才那份新的）——把备份放回去，实例仍可打开。
    std::fs::write(dir.path().join("control/control01.ctl"), &cur_a).ok();
    std::fs::write(dir.path().join("control/control02.ctl"), &cur_b).ok();
}

/// **崩溃语义**：不调 `shutdown`（脏页留在池里、WAL 是唯一耐久源）⇒
/// 重开时恢复把已提交的行重做出来。
#[test]
fn crash_without_shutdown_recovers_committed_rows() {
    let dir = TempDir::new("crash");
    {
        let mut inst = create_instance(&params_for(dir.path()), None).expect("建区");
        ok(
            &mut inst,
            "CREATE TABLE c (id NUMBER NOT NULL, tag VARCHAR2(16))",
        );
        for i in 1..=50 {
            ok(&mut inst, &format!("INSERT INTO c VALUES ({i}, 'r{i}')"));
        }
        // **不 `shutdown`**：模拟进程直接消失（日志已耐久、页未回写）。
        // 真崩溃时进程也没了 ⇒ 实例锁是**陈旧的**（pid 文件在、进程不在）——
        // `mem::forget` 做不到"进程消失"，这里把 pid 文件删掉补上这一半。
        std::mem::forget(inst);
        let _ = std::fs::remove_file(dir.path().join("bicdb.pid"));
    }
    let mut inst = open_instance(&params_for(dir.path())).expect("崩溃后重开");
    let r = inst.recovery.expect("应有一次恢复");
    assert_eq!(
        rows(&ok(&mut inst, "SELECT id FROM c")).len(),
        50,
        "恢复后行数一致"
    );
    inst.shutdown().expect("关闭");
    let _ = r;
}

/// **读己所写**：一个会话要看得见**自己**未提交的改动（Oracle/PG 同款）。
///
/// 钉的是一条真炸过的路：CR（一致性读）只按"快照可见性"撤销，把自己事务的
/// ITL 条目也撤销了——`BEGIN; INSERT; SELECT` 于是查不到刚插的行；
/// 服务模式的会话是**长期存在**的，这个洞在那儿才现形。
///
/// **形态很重要**：必须**一个会话贯穿全程**（服务模式正是这个形态）。
/// 每条语句新开一个 `Session` 的形态测不到事务——语句收尾时 `Drop` 会把
/// 显式事务回滚掉（见 [`Session`] 的 `Drop`）。
///
/// **"别人看不见"那一侧不在本用例里**：`Session` 借 `&mut Catalog`，
/// 同实例同时只存在一个会话（单写者纪律的一种体现）——跨视角的隔离钉在
/// 更低的层：`bicdb-storage` 的 `cr::tests::own_txn_sees_its_uncommitted_row_others_do_not`。
#[test]
fn a_session_sees_its_own_uncommitted_rows() {
    let dir = TempDir::new("own-writes");
    let mut inst = create_instance(&params_for(dir.path()), None).expect("建区");
    ok(
        &mut inst,
        "CREATE TABLE t (id NUMBER NOT NULL, v VARCHAR2(8))",
    );
    ok(&mut inst, "INSERT INTO t VALUES (1, 'a')");

    let seq = inst.seq();
    let mut s = Session::new(inst.pool, inst.engine, &mut inst.catalog, seq);

    // ① 本会话：BEGIN 之后看得见自己的行。
    s.execute("BEGIN").expect("begin");
    s.execute("INSERT INTO t VALUES (2, 'b')").expect("插入");
    let mine = rows(&s.execute("SELECT id FROM t ORDER BY id").expect("查"));
    assert_eq!(mine, vec![vec!["1"], vec!["2"]], "本会话要看得见未提交的行");

    // ② 同一事务里再插一条，前一条也还在（读己所写对整条链成立）。
    s.execute("INSERT INTO t VALUES (3, 'c')").expect("插入2");
    let mine = rows(&s.execute("SELECT id FROM t ORDER BY id").expect("查"));
    assert_eq!(mine.len(), 3, "事务里应看得见两条未提交的行");

    // ③ 回滚后自己也看不见了。
    s.execute("ROLLBACK").expect("回滚");
    let after = rows(&s.execute("SELECT id FROM t ORDER BY id").expect("查"));
    assert_eq!(after, vec![vec!["1"]], "回滚后不该有第 2、3 行");

    // ④ 提交后**任何**快照够新的会话都看得见（换一个会话查）。
    s.execute("BEGIN").expect("begin");
    s.execute("INSERT INTO t VALUES (4, 'd')").expect("插入");
    s.execute("COMMIT").expect("提交");
    drop(s);
    let after = rows(&ok(&mut inst, "SELECT id FROM t ORDER BY id"));
    assert_eq!(after, vec![vec!["1"], vec!["4"]], "提交后要看得见");

    // ⑤ 唯一键在**事务内**也拦得住（读己所写让"同事务重复插入"看得见）。
    let seq = inst.seq();
    let mut s = Session::new(inst.pool, inst.engine, &mut inst.catalog, seq);
    s.execute("CREATE UNIQUE INDEX u ON t (id)")
        .expect("建索引");
    s.execute("BEGIN").expect("begin");
    s.execute("INSERT INTO t VALUES (9, 'x')").expect("插入");
    let e = s
        .execute("INSERT INTO t VALUES (9, 'y')")
        .expect_err("应拦住重复键");
    assert!(
        e.to_string().contains("唯一") || e.to_string().contains("重复"),
        "应拦住重复键：{e}"
    );
    s.execute("ROLLBACK").expect("回滚");
    drop(s);
    inst.shutdown().expect("收尾");
}

/// **显式事务里的语句级回滚**：一条语句失败只回滚**它自己**——此前已成功的
/// 语句照旧、事务与锁照旧、`COMMIT` 照旧可用（Oracle 口径）。
///
/// 原先的错误路径**无条件整事务回滚**并丢掉事务句柄：`BEGIN; INSERT ok;
/// INSERT 失败; COMMIT;` 会把第一条一起毁掉，`COMMIT` 只回一句"没有活动事务"
/// ——用户的半个事务凭空消失，而且没有任何提示。
#[test]
fn a_failed_statement_does_not_roll_back_the_whole_transaction() {
    let dir = TempDir::new("stmt-rollback");
    let mut inst = create_instance(&params_for(dir.path()), None).expect("建区");
    ok(
        &mut inst,
        "CREATE TABLE t (id NUMBER NOT NULL, v VARCHAR2(8))",
    );
    ok(&mut inst, "CREATE UNIQUE INDEX tu ON t (id)");

    let seq = inst.seq();
    let mut s = Session::new(inst.pool, inst.engine, &mut inst.catalog, seq);
    s.execute("BEGIN").expect("begin");
    s.execute("INSERT INTO t VALUES (1, 'a')").expect("插入");
    // 第二条撞唯一键：**只有它**该被回滚。
    let e = s
        .execute("INSERT INTO t VALUES (1, 'dup')")
        .expect_err("应撞唯一约束");
    assert!(
        e.to_string().contains("唯一") || e.to_string().contains("重复"),
        "{e}"
    );
    // 事务还活着：再插一条成功的，然后提交。
    s.execute("INSERT INTO t VALUES (2, 'b')").expect("插入2");
    let rows_now = rows(&s.execute("SELECT id FROM t ORDER BY id").expect("查"));
    assert_eq!(
        rows_now,
        vec![vec!["1"], vec!["2"]],
        "失败语句不该带走前一条"
    );
    s.execute("COMMIT").expect("提交应照旧可用");
    drop(s);

    // 提交后另开会话可见（真的提交了，不是"没报错但没生效"）。
    let after = rows(&ok(&mut inst, "SELECT id FROM t ORDER BY id"));
    assert_eq!(after, vec![vec!["1"], vec!["2"]]);
    inst.shutdown().expect("收尾");
}

/// **`UPDATE` / `DELETE`**：WHERE 生效、事务里可回滚、**索引与行不脱节**。
///
/// **索引模型（本版）**：索引项**只插不删**（索引页写只有 redo、没有 undo ⇒
/// 删了事务回滚就找不回），"这一项还作不作数"由**行**说话——
/// 读侧一律"取该 ROWID 的行、重算键、逐字节比"（与 PG 同模型；`arch/09` §9.1.2
/// 的"删除即移除"要等索引项写的 undo，记档在 `dml_index`）。
/// 所以本用例钉的是**语义**（找得到活行、不产生假冲突、回滚后项还在），不是条目计数。
#[test]
fn update_and_delete_with_where_and_index_maintenance() {
    let dir = TempDir::new("upd");
    let mut inst = create_instance(&params_for(dir.path()), None).expect("建区");
    ok(
        &mut inst,
        "CREATE TABLE t (id NUMBER NOT NULL, name VARCHAR2(32))",
    );
    ok(&mut inst, "CREATE UNIQUE INDEX t_pk ON t (id)");
    ok(&mut inst, "CREATE INDEX t_name ON t (name)");
    ok(
        &mut inst,
        "INSERT INTO t VALUES (1,'a'); INSERT INTO t VALUES (2,'b'); INSERT INTO t VALUES (3,'c')",
    );

    // **WHERE 真生效**。
    assert_eq!(
        affected(&ok(&mut inst, "UPDATE t SET name = 'X' WHERE id = 1")),
        1
    );
    assert_eq!(
        rows(&ok(&mut inst, "SELECT id, name FROM t ORDER BY id")),
        vec![
            vec!["1".to_owned(), "X".to_owned()],
            vec!["2".to_owned(), "b".to_owned()],
            vec!["3".to_owned(), "c".to_owned()],
        ]
    );
    // 无 WHERE ⇒ 全表。
    assert_eq!(affected(&ok(&mut inst, "UPDATE t SET name = 'ALL'")), 3);
    assert_eq!(affected(&ok(&mut inst, "DELETE FROM t WHERE id >= 2")), 2);
    assert_eq!(
        rows(&ok(&mut inst, "SELECT id, name FROM t")),
        vec![vec!["1".to_owned(), "ALL".to_owned()]]
    );

    // **唯一性由行说话**：删掉的行再插回来不被陈旧索引项挡（也不能漏判——
    // 下一段的"回滚后重插"才是漏判那一侧）。
    ok(&mut inst, "INSERT INTO t VALUES (2, 'b2')");
    assert_eq!(affected(&ok(&mut inst, "DELETE FROM t WHERE id = 2")), 1);
    ok(&mut inst, "INSERT INTO t VALUES (2, 'b3')");
    // 真冲突照旧拒绝。
    let e = err(&mut inst, "INSERT INTO t VALUES (1, 'dup')");
    assert!(
        e.contains("唯一") || e.contains("冲突") || e.contains("Duplicate"),
        "{e}"
    );

    // **改值之后不产生假冲突**（陈旧索引项不得挡住合法插入）：
    // 把 2 号的名字改成 'B3'（`t_name` 上的旧键 'b3' 条目作废、新键 'B3' 生效），
    // 再插一行叫 'b3' —— 必须成功。
    assert_eq!(
        affected(&ok(&mut inst, "UPDATE t SET name = 'B3' WHERE id = 2")),
        1
    );
    ok(&mut inst, "INSERT INTO t VALUES (4, 'b3')");
    // 而当前的键仍然受唯一性保护（`t_pk` 上）。
    let e = err(&mut inst, "INSERT INTO t VALUES (4, 'again')");
    assert!(
        e.contains("唯一") || e.contains("冲突") || e.contains("Duplicate"),
        "{e}"
    );
    assert_eq!(affected(&ok(&mut inst, "DELETE FROM t WHERE id = 4")), 1);

    // **事务里回滚**：数据回来，唯一性照旧（这一条抓过真缺陷：删项的版本下，
    // 回滚后索引里少一条活行的项 ⇒ 同键能重复插进来）。
    let rs = ok(
        &mut inst,
        "BEGIN; DELETE FROM t WHERE id = 1; INSERT INTO t VALUES (7,'seven'); \
         UPDATE t SET name='SEVEN' WHERE id = 7; SELECT name FROM t WHERE id = 7;",
    );
    assert_eq!(rows(&rs), vec![vec!["SEVEN".to_owned()]], "事务内读己所写");
    ok(&mut inst, "BEGIN; DELETE FROM t WHERE id = 7; ROLLBACK");
    let e = err(&mut inst, "INSERT INTO t VALUES (1, 'again')");
    assert!(
        e.contains("唯一") || e.contains("冲突") || e.contains("Duplicate"),
        "回滚恢复的行必须仍受唯一性保护：{e}"
    );

    // **改唯一键 ⇒ 具名拒绝**（写前预检只覆盖 INSERT，本版不假装支持）。
    let e = err(&mut inst, "UPDATE t SET id = 9 WHERE id = 1");
    assert!(e.contains("唯一索引"), "{e}");
    // 改非键列照常。
    assert_eq!(
        affected(&ok(&mut inst, "UPDATE t SET name = 'Z' WHERE id = 1")),
        1
    );

    // **重开**：改过的数据在（完全检查点 + 恢复）。
    inst.shutdown().expect("关");
    drop(inst);
    let mut inst = open_instance(&params_for(dir.path())).expect("开");
    let got = rows(&ok(&mut inst, "SELECT id, name FROM t ORDER BY id"));
    assert_eq!(got.len(), 2, "id = 1 与 2 两行");
    assert_eq!(got[0], vec!["1".to_owned(), "Z".to_owned()]);
    inst.shutdown().expect("关");
}

/// 取"影响行数"（`Affected`）。
fn affected(results: &[QueryResult]) -> u64 {
    match results.last() {
        Some(QueryResult::Affected(n)) => *n,
        other => panic!("期待影响行数，得到 {other:?}"),
    }
}

/// **聚合 / `GROUP BY` / `HAVING` / `DISTINCT` / `CASE`·`COALESCE`**（S2 面）。
#[test]
fn aggregates_group_by_having_distinct_and_case() {
    let dir = TempDir::new("agg");
    let mut inst = create_instance(&params_for(dir.path()), None).expect("建区");
    ok(
        &mut inst,
        "CREATE TABLE s (id NUMBER NOT NULL, grp VARCHAR2(8), v NUMBER)",
    );
    ok(
        &mut inst,
        "INSERT INTO s VALUES (1,'a',10); INSERT INTO s VALUES (2,'a',20); \
         INSERT INTO s VALUES (3,'b',5); INSERT INTO s VALUES (4,'b',NULL)",
    );

    // 无分组聚合：空集语义也要对（下面单独测）。
    assert_eq!(
        rows(&ok(
            &mut inst,
            "SELECT COUNT(*), COUNT(v), SUM(v), AVG(v), MIN(v), MAX(v) FROM s"
        )),
        vec![vec![
            "4".to_owned(),
            "3".to_owned(),
            "35".to_owned(),
            "11.66666666666666666666666666666666666667".to_owned(),
            "5".to_owned(),
            "20".to_owned(),
        ]]
    );
    // GROUP BY + HAVING（按 c 的计数过滤）。
    assert_eq!(
        rows(&ok(
            &mut inst,
            "SELECT grp, COUNT(*) FROM s GROUP BY grp HAVING COUNT(*) > 1 ORDER BY grp"
        )),
        vec![
            vec!["a".to_owned(), "2".to_owned()],
            vec!["b".to_owned(), "2".to_owned()],
        ]
    );
    // `COUNT(DISTINCT x)`。
    assert_eq!(
        rows(&ok(&mut inst, "SELECT COUNT(DISTINCT grp) FROM s")),
        vec![vec!["2".to_owned()]]
    );
    // `WHERE` 先于聚合。
    assert_eq!(
        rows(&ok(&mut inst, "SELECT SUM(v) FROM s WHERE grp = 'a'")),
        vec![vec!["30".to_owned()]]
    );
    // 非分组列 ⇒ 具名拒绝（不是静默给一个值）。
    let e = err(&mut inst, "SELECT id, COUNT(*) FROM s");
    assert!(e.contains("既不在 GROUP BY"), "{e}");

    // `DISTINCT`（含 `ORDER BY` 并存）。
    assert_eq!(
        rows(&ok(
            &mut inst,
            "SELECT DISTINCT grp FROM s ORDER BY grp DESC"
        )),
        vec![vec!["b".to_owned()], vec!["a".to_owned()]]
    );
    // `CASE` / `COALESCE` / `NULLIF`。
    assert_eq!(
        rows(&ok(
            &mut inst,
            "SELECT id, CASE WHEN v IS NULL THEN 'none' WHEN v > 10 THEN 'big' ELSE 'small' END, \
             COALESCE(v, 0), NULLIF(grp, 'a') FROM s ORDER BY id"
        )),
        vec![
            vec![
                "1".to_owned(),
                "small".to_owned(),
                "10".to_owned(),
                "NULL".to_owned()
            ],
            vec![
                "2".to_owned(),
                "big".to_owned(),
                "20".to_owned(),
                "NULL".to_owned()
            ],
            vec![
                "3".to_owned(),
                "small".to_owned(),
                "5".to_owned(),
                "b".to_owned()
            ],
            vec![
                "4".to_owned(),
                "none".to_owned(),
                "0".to_owned(),
                "b".to_owned()
            ],
        ]
    );
    // 未知函数 ⇒ 点名"没有函数目录"。
    let e = err(&mut inst, "SELECT upper(grp) FROM s");
    assert!(e.contains("函数目录"), "{e}");
    // 聚合进 WHERE ⇒ 点明位置不对。
    let e = err(&mut inst, "SELECT COUNT(*) FROM s WHERE SUM(v) > 1");
    assert!(e.contains("只能出现在 SELECT 列表或 HAVING"), "{e}");

    // **空表**：`COUNT(*) = 0`、`SUM` = NULL（一条也聚合出一行）。
    ok(&mut inst, "CREATE TABLE e (v NUMBER)");
    assert_eq!(
        rows(&ok(&mut inst, "SELECT COUNT(*), SUM(v) FROM e")),
        vec![vec!["0".to_owned(), "NULL".to_owned()]]
    );
    // 空表 + GROUP BY ⇒ 零行（没有分组键）。
    assert!(rows(&ok(&mut inst, "SELECT v, COUNT(*) FROM e GROUP BY v")).is_empty());

    inst.shutdown().expect("关");
}

/// **两表连接**（`JOIN … ON` / 逗号连接 / 别名 / 限定名 / 歧义）。
#[test]
fn two_table_joins() {
    let dir = TempDir::new("join");
    let mut inst = create_instance(&params_for(dir.path()), None).expect("建区");
    ok(
        &mut inst,
        "CREATE TABLE a (id NUMBER NOT NULL, tag VARCHAR2(8))",
    );
    ok(
        &mut inst,
        "CREATE TABLE b (id NUMBER NOT NULL, note VARCHAR2(8))",
    );
    ok(
        &mut inst,
        "INSERT INTO a VALUES (1,'a1'); INSERT INTO a VALUES (2,'a2'); INSERT INTO a VALUES (3,'a3')",
    );
    ok(
        &mut inst,
        "INSERT INTO b VALUES (2,'b2'); INSERT INTO b VALUES (3,'b3'); INSERT INTO b VALUES (4,'b4')",
    );

    // INNER JOIN + 限定名 + 别名。
    assert_eq!(
        rows(&ok(
            &mut inst,
            "SELECT x.id, y.note FROM a x JOIN b y ON x.id = y.id ORDER BY x.id"
        )),
        vec![
            vec!["2".to_owned(), "b2".to_owned()],
            vec!["3".to_owned(), "b3".to_owned()],
        ]
    );
    // LEFT JOIN：左表全留，右表缺的补 NULL。
    assert_eq!(
        rows(&ok(
            &mut inst,
            "SELECT x.id, y.note FROM a x LEFT JOIN b y ON x.id = y.id ORDER BY x.id"
        )),
        vec![
            vec!["1".to_owned(), "NULL".to_owned()],
            vec!["2".to_owned(), "b2".to_owned()],
            vec!["3".to_owned(), "b3".to_owned()],
        ]
    );
    // 逗号连接 + WHERE（笛卡尔再由 WHERE 收窄）。
    assert_eq!(
        rows(&ok(
            &mut inst,
            "SELECT COUNT(*) FROM a, b WHERE a.id = b.id"
        )),
        vec![vec!["2".to_owned()]]
    );
    // 同名列不加限定 ⇒ 歧义（拒绝，不猜）。
    let e = err(&mut inst, "SELECT id FROM a JOIN b ON a.id = b.id");
    assert!(e.contains("歧义"), "{e}");
    // 别名与表名都能当限定名。
    assert_eq!(
        rows(&ok(
            &mut inst,
            "SELECT b.note FROM a, b WHERE a.id = b.id ORDER BY note"
        )),
        vec![vec!["b2".to_owned()], vec!["b3".to_owned()]]
    );
    // **`ORDER BY` 收表达式/不投影的列**（2026-10-07 起）：`b.id` 没投影出来也能排，
    // 且**限定名按表解析**（不是按输出列名找"id"）。
    assert_eq!(
        rows(&ok(
            &mut inst,
            "SELECT b.note FROM a, b WHERE a.id = b.id ORDER BY b.id DESC"
        )),
        vec![vec!["b3".to_owned()], vec!["b2".to_owned()]]
    );
    // 两处都没有的名字：点明"既不在输出列、也不在 FROM 里"（不误导人去翻表定义）。
    let e = err(
        &mut inst,
        "SELECT b.note FROM a, b WHERE a.id = b.id ORDER BY nope",
    );
    assert!(e.contains("既不在 SELECT 的输出列里"), "{e}");
    // 三表 ⇒ 具名拒绝（本版只做两表）。
    ok(&mut inst, "CREATE TABLE c (id NUMBER NOT NULL)");
    let e = err(&mut inst, "SELECT COUNT(*) FROM a, b, c");
    assert!(e.contains("两表"), "{e}");
    // 两表限定名相撞 ⇒ 拒绝。
    let e = err(&mut inst, "SELECT 1 FROM a b JOIN b b ON a.id = b.id");
    assert!(e.contains("限定名") || e.contains("别名"), "{e}");

    inst.shutdown().expect("关");
}

/// **集合运算 / 无 `FROM` 的 SELECT / `INSERT … SELECT`**（S3 面）。
#[test]
fn setops_no_from_and_insert_select() {
    let dir = TempDir::new("setop");
    let mut inst = create_instance(&params_for(dir.path()), None).expect("建区");
    ok(
        &mut inst,
        "CREATE TABLE u (id NUMBER NOT NULL, tag VARCHAR2(8))",
    );
    ok(
        &mut inst,
        "CREATE TABLE w (id NUMBER NOT NULL, tag VARCHAR2(8))",
    );
    ok(
        &mut inst,
        "INSERT INTO u VALUES (1,'a'); INSERT INTO u VALUES (2,'b'); INSERT INTO u VALUES (3,'c')",
    );
    ok(
        &mut inst,
        "INSERT INTO w VALUES (2,'b'); INSERT INTO w VALUES (3,'x'); INSERT INTO w VALUES (4,'d')",
    );

    // 无 `FROM`：一行、表达式求值。
    assert_eq!(
        rows(&ok(&mut inst, "SELECT 2 + 3 * 4")),
        vec![vec!["14".to_owned()]]
    );
    // 列引用在没有 FROM 时不存在。
    let e = err(&mut inst, "SELECT id");
    assert!(e.contains("列 `id` 不存在"), "{e}");

    // UNION（去重）/ UNION ALL（保重数）。
    assert_eq!(
        rows(&ok(
            &mut inst,
            "SELECT id FROM u UNION SELECT id FROM w ORDER BY id"
        )),
        vec![
            vec!["1".to_owned()],
            vec!["2".to_owned()],
            vec!["3".to_owned()],
            vec!["4".to_owned()]
        ]
    );
    // 子查询仍不支持（语法层拒绝）。
    let e = err(&mut inst, "SELECT COUNT(*) FROM (SELECT 1) x");
    assert!(!e.is_empty());
    // INTERSECT / EXCEPT。
    assert_eq!(
        rows(&ok(
            &mut inst,
            "SELECT id FROM u INTERSECT SELECT id FROM w ORDER BY id"
        )),
        vec![vec!["2".to_owned()], vec!["3".to_owned()]]
    );
    assert_eq!(
        rows(&ok(
            &mut inst,
            "SELECT id FROM u EXCEPT SELECT id FROM w ORDER BY id"
        )),
        vec![vec!["1".to_owned()]]
    );
    // 两侧列数必须一致。
    let e = err(&mut inst, "SELECT id FROM u UNION SELECT id, tag FROM w");
    assert!(e.contains("列数不一致"), "{e}");

    // **`INSERT … SELECT`**：含唯一性预检（重复键整语句拒绝）与聚合来源。
    ok(
        &mut inst,
        "CREATE TABLE t (id NUMBER NOT NULL, tag VARCHAR2(8))",
    );
    ok(&mut inst, "CREATE UNIQUE INDEX t_pk ON t (id)");
    assert_eq!(
        affected(&ok(
            &mut inst,
            "INSERT INTO t (id, tag) SELECT id, tag FROM u"
        )),
        3
    );
    let e = err(&mut inst, "INSERT INTO t (id, tag) SELECT id, tag FROM w");
    assert!(e.contains("唯一约束冲突"), "重复键要整语句拒绝：{e}");
    // 列数不符 ⇒ 绑定期拒绝。
    let e = err(&mut inst, "INSERT INTO t (id) SELECT id, tag FROM u");
    assert!(e.contains("列数必须一致"), "{e}");
    // 聚合来源：写成一行。
    assert_eq!(
        affected(&ok(
            &mut inst,
            "INSERT INTO t (id, tag) SELECT COUNT(*) + 100, MAX(tag) FROM u"
        )),
        1
    );
    assert_eq!(
        rows(&ok(&mut inst, "SELECT id, tag FROM t WHERE id = 103")),
        vec![vec!["103".to_owned(), "c".to_owned()]]
    );
    assert_eq!(
        rows(&ok(&mut inst, "SELECT COUNT(*) FROM t")).len(),
        1,
        "聚合来源只写一行"
    );

    inst.shutdown().expect("关");
}

// ─────────────────── 索引访问路径（规则式选路；D6 之后的切片） ───────────────────

/// **等值查询走索引之后，结果必须一条不差**——尤其是有**陈旧索引项**时。
///
/// 索引项**只插不删**（`arch/09` §9.1.2 的移除要 undo）：`UPDATE` 改了键列
/// 只**追加**新项，旧项留着指向同一行。所以"索引 = 活行的超集，不是真相"——
/// 索引只缩小候选，**回表 + 谓词复核**才是正确性的承担者
/// （证据包 `doc/evidence/index-access-20261007/` §3，PG 同款）。
#[test]
fn index_equality_lookups_stay_correct_through_stale_entries() {
    let dir = TempDir::new("idxeq");
    let mut inst = create_instance(&params_for(dir.path()), None).expect("建区");
    ok(
        &mut inst,
        "CREATE TABLE t (id NUMBER NOT NULL, tag VARCHAR2(16))",
    );
    ok(&mut inst, "CREATE UNIQUE INDEX t_pk ON t (id)");
    ok(&mut inst, "CREATE INDEX t_tag ON t (tag)"); // **普通**索引：允许改键列
    ok(
        &mut inst,
        "INSERT INTO t VALUES (1,'a'); INSERT INTO t VALUES (2,'a'); \
         INSERT INTO t VALUES (3,'b'); INSERT INTO t VALUES (4,NULL)",
    );

    // ① 唯一索等值（点查）。
    assert_eq!(
        rows(&ok(&mut inst, "SELECT id FROM t WHERE id = 2")),
        vec![vec!["2".to_owned()]]
    );
    assert!(rows(&ok(&mut inst, "SELECT id FROM t WHERE id = 99")).is_empty());
    // ② 普通索引等值（同键多行）。
    assert_eq!(
        rows(&ok(
            &mut inst,
            "SELECT id FROM t WHERE tag = 'a' ORDER BY id"
        )),
        vec![vec!["1".to_owned()], vec!["2".to_owned()]]
    );
    // ③ 参数形态（执行期才有值）。
    let p = ok_params(
        &mut inst,
        "SELECT id FROM t WHERE id = :id",
        &[("id", num_val(3))],
    );
    assert_eq!(p, vec![vec!["3".to_owned()]]);
    // 参数是 NULL ⇒ `= NULL` 恒不成立 ⇒ **零行**（不是"退化成全索引扫描"）。
    let n = ok_params(
        &mut inst,
        "SELECT id FROM t WHERE id = :id",
        &[("id", Value::Null)],
    );
    assert!(n.is_empty(), "= NULL 必须零行：{n:?}");

    // ④ **陈旧项**：改 `tag`（普通索引的键列）——旧项留着、新项追加。
    assert_eq!(
        affected(&ok(&mut inst, "UPDATE t SET tag = 'z' WHERE id = 2")),
        1
    );
    // 旧键查不到那行（陈旧项回表后被谓词挡下），新键查得到。
    assert_eq!(
        rows(&ok(&mut inst, "SELECT id FROM t WHERE tag = 'a'")),
        vec![vec!["1".to_owned()]],
        "陈旧索引项不许让 `tag = 'a'` 把 id=2 带出来"
    );
    assert_eq!(
        rows(&ok(&mut inst, "SELECT id FROM t WHERE tag = 'z'")),
        vec![vec!["2".to_owned()]]
    );

    // ⑤ 删除之后：行的键没了，索引项还在 ⇒ 同样查不到。
    assert_eq!(affected(&ok(&mut inst, "DELETE FROM t WHERE id = 3")), 1);
    assert!(rows(&ok(&mut inst, "SELECT id FROM t WHERE id = 3")).is_empty());
    assert!(rows(&ok(&mut inst, "SELECT id FROM t WHERE tag = 'b'")).is_empty());

    // ⑥ 重开（完全检查点 → 三阶段恢复）之后照旧。
    inst.shutdown().expect("关");
    drop(inst); // 实例锁随 `Instance` 落下（直连形态）
    let mut inst2 = open_instance(&params_for(dir.path())).expect("重开");
    assert_eq!(
        rows(&ok(&mut inst2, "SELECT id FROM t WHERE tag = 'z'")),
        vec![vec!["2".to_owned()]]
    );
    assert_eq!(
        rows(&ok(&mut inst2, "SELECT id FROM t WHERE tag = 'a'")),
        vec![vec!["1".to_owned()]]
    );
    inst2.shutdown().expect("关");
}

/// 数值参数（测试里手搓 `Value`）。
fn num_val(n: i64) -> Value {
    Value::Number(bicdb_types::Number::parse(&n.to_string()).expect("数值"))
}

/// 带参数跑一条（返回结果集的行）。
fn ok_params(inst: &mut Instance, sql: &str, params: &[(&str, Value)]) -> Vec<Vec<String>> {
    let seq = inst.seq();
    let mut session = Session::new(inst.pool, inst.engine, &mut inst.catalog, seq);
    let out = session
        .execute_with_params(sql, params)
        .unwrap_or_else(|e| panic!("`{sql}` 应成功：{e}"));
    rows(&out)
}

// ─────────────────── 日志压力与检查点（CKPT 触发 ②） ───────────────────

/// **日志要被挡就先推一次检查点**（`doc/arch/11-持久化与恢复.md` §11.7 的
/// CKPT 触发条件 ②"组满被迫"）。
///
/// 复现形态：`wal_groups=2` + `wal_group_pages=64`（每组 32 KiB）的小实例上连续
/// 插入——**修之前第 40 行就断**："日志切换等待检查点（无可复用组：检查点未越过）"
/// （写者等一个**永远不来**的检查点：服务里没有后台 CKPT，`crates/daemon` 未接电）。
/// 修之后：会话在语句边界问一句"下一次切换会不会被挡"（只读探测），挡就地推一次
/// 完全检查点 ⇒ 组降级 ⇒ 一路写下去（Oracle 的"日志切换触发检查点"、
/// PG 的 `max_wal_size` 触发检查点，都是这条）。
#[test]
fn log_pressure_triggers_a_checkpoint_so_bulk_loads_keep_going() {
    let dir = TempDir::new("walpress");
    let params = InstanceParams::for_init(
        dir.path(),
        None,
        &[
            ("init.wal_groups".to_owned(), "2".to_owned()),
            ("init.wal_group_pages".to_owned(), "64".to_owned()),
        ],
    )
    .expect("参数");
    let mut inst = create_instance(&params, None).expect("建区");
    let seq = inst.seq();
    let mut session = Session::new(inst.pool, inst.engine, &mut inst.catalog, seq);
    session
        .execute("CREATE TABLE t (id NUMBER NOT NULL)")
        .expect("建表");
    // 300 行：远超两个 32 KiB 组（修之前 ~40 行就失败）。
    let batch: String = (0..300)
        .map(|i| format!("INSERT INTO t VALUES ({i});"))
        .collect();
    session
        .execute(&batch)
        .expect("小日志实例上连续插入不该被挡");
    let got = rows(&session.execute("SELECT COUNT(*) FROM t").expect("数行"));
    assert_eq!(got, vec![vec!["300".to_owned()]], "300 行都在");
    assert!(
        session.log_checkpoints() > 0,
        "应因日志压力推过完全检查点（计数 = {}）",
        session.log_checkpoints()
    );
    // **干净收尾**：完全检查点之后关掉，重开数据照旧。
    drop(session);
    inst.shutdown().expect("关");
    drop(inst);
    let mut again = open_instance(&params).expect("重开");
    let got = rows(&ok(&mut again, "SELECT COUNT(*) FROM t"));
    assert_eq!(got, vec![vec!["300".to_owned()]]);
    again.shutdown().expect("关");
}

/// **索引内表连接（IndexNL）的差分验收**：同一句连接，**有索引**与**没索引**
/// （`DROP INDEX` → 顺序重扫）两条访问路径必须**逐行一致**——含重复键、
/// 陈旧索引项与 `LEFT` 的补 NULL。
///
/// 为什么用差分：连接的两种路径都"跑得通"，逐行对比才抓得住"少了一行/多了一行"
/// 这种静默错（索引只缩小候选、连接条件仍逐对复核，是它们一致的根据）。
#[test]
fn join_with_an_index_inner_matches_the_full_rescan_path() {
    let dir = TempDir::new("indexnl");
    let mut inst = create_instance(&params_for(dir.path()), None).expect("建区");
    ok(
        &mut inst,
        "CREATE TABLE a (k NUMBER NOT NULL, v VARCHAR2(8))",
    );
    ok(
        &mut inst,
        "CREATE TABLE b (k NUMBER NOT NULL, v VARCHAR2(8))",
    );
    // **普通**索引：允许改键列 ⇒ 后面能造陈旧项。
    ok(&mut inst, "CREATE INDEX i_b_k ON b (k)");
    ok(
        &mut inst,
        "INSERT INTO a VALUES (1,'a1'); INSERT INTO a VALUES (2,'a2'); \
         INSERT INTO a VALUES (2,'a2b'); INSERT INTO a VALUES (3,'a3')",
    );
    ok(
        &mut inst,
        "INSERT INTO b VALUES (1,'b1'); INSERT INTO b VALUES (2,'b2'); \
         INSERT INTO b VALUES (2,'b2b'); INSERT INTO b VALUES (4,'b4')",
    );

    const INNER: &str =
        "SELECT a.k, a.v, b.k, b.v FROM a JOIN b ON a.k = b.k ORDER BY a.k, a.v, b.v";
    const LEFT: &str =
        "SELECT a.k, a.v, b.k, b.v FROM a LEFT JOIN b ON a.k = b.k ORDER BY a.k, a.v, b.v";

    // ① 有索引：内表走索引探测。
    let inner_idx = rows(&ok(&mut inst, INNER));
    let left_idx = rows(&ok(&mut inst, LEFT));
    assert_eq!(
        inner_idx.len(),
        5,
        "2 个 a.k=2 × 2 个 b.k=2 ⇒ 5 对：{inner_idx:?}"
    );
    assert_eq!(left_idx.len(), 6, "多一个补 NULL 的 a3：{left_idx:?}");

    // ② 没索引：顺序重扫——结果必须一样。
    ok(&mut inst, "DROP INDEX i_b_k");
    assert_eq!(
        rows(&ok(&mut inst, INNER)),
        inner_idx,
        "INNER：两条路径逐行一致"
    );
    assert_eq!(
        rows(&ok(&mut inst, LEFT)),
        left_idx,
        "LEFT：两条路径逐行一致"
    );

    // ③ 建回索引 + **陈旧项**：改 `b.k`（普通索引键列）⇒ 旧项留在树里。
    ok(&mut inst, "CREATE INDEX i_b_k ON b (k)");
    assert_eq!(
        affected(&ok(&mut inst, "UPDATE b SET k = 9 WHERE v = 'b2'")),
        1
    );
    let inner_after = rows(&ok(&mut inst, INNER));
    let left_after = rows(&ok(&mut inst, LEFT));
    assert_eq!(
        inner_after.len(),
        3,
        "b2 已搬到 k=9：k=2 只剩 b2b（1×2 对）+ k=1 一对：{inner_after:?}"
    );
    ok(&mut inst, "DROP INDEX i_b_k");
    assert_eq!(
        rows(&ok(&mut inst, INNER)),
        inner_after,
        "陈旧项：两条路径一致"
    );
    assert_eq!(
        rows(&ok(&mut inst, LEFT)),
        left_after,
        "陈旧项：两条路径一致"
    );

    // ④ 重开后照旧（索引与行都在）。
    inst.shutdown().expect("关");
    drop(inst);
    let mut again = open_instance(&params_for(dir.path())).expect("重开");
    ok(&mut again, "CREATE INDEX i_b_k ON b (k)");
    assert_eq!(rows(&ok(&mut again, INNER)), inner_after);
    again.shutdown().expect("关");
}

/// **有界范围走索引之后，端点必须按开闭办**——`>` 多带回一条端点行是**静默错**。
///
/// 差分形态与 [`join_with_an_index_inner_matches_the_full_rescan_path`] 同源：
/// **有索引**（范围扫描）与 **`DROP INDEX` 后**（全表扫描 + 谓词复核）逐行一致。
#[test]
fn range_lookups_honour_the_endpoints_and_match_the_full_scan() {
    let dir = TempDir::new("idxrange");
    let mut inst = create_instance(&params_for(dir.path()), None).expect("建区");
    ok(
        &mut inst,
        "CREATE TABLE t (k NUMBER NOT NULL, v VARCHAR2(8))",
    );
    // **普通**索引：允许改键列 ⇒ 后面能造陈旧项（陈旧项在范围里的表现也要对）。
    ok(&mut inst, "CREATE INDEX i_t_k ON t (k)");
    // 重复键（k=2 三条）+ 空缺（没有 5）。
    ok(
        &mut inst,
        "INSERT INTO t VALUES (1,'a'); INSERT INTO t VALUES (2,'b'); \
         INSERT INTO t VALUES (2,'c'); INSERT INTO t VALUES (2,'d'); \
         INSERT INTO t VALUES (4,'e'); INSERT INTO t VALUES (9,'f')",
    );

    let queries = [
        "SELECT k, v FROM t WHERE k BETWEEN 2 AND 4 ORDER BY k, v",
        "SELECT k, v FROM t WHERE k >= 2 AND k <= 4 ORDER BY k, v",
        "SELECT k, v FROM t WHERE k > 2 AND k < 4 ORDER BY k, v", // 端点落在重复键上
        "SELECT k, v FROM t WHERE k > 1 AND k <= 9 ORDER BY k, v",
        "SELECT k, v FROM t WHERE k >= 9 AND k <= 9 ORDER BY k, v",
        "SELECT k, v FROM t WHERE k > 4 AND k < 9 ORDER BY k, v", // 空区间
    ];
    let with_index: Vec<Vec<Vec<String>>> =
        queries.iter().map(|q| rows(&ok(&mut inst, q))).collect();
    // 端点语义：`> 2 AND < 4` 只剩 k=4？不——4 不满足 `< 4` ⇒ **空**。
    assert!(with_index[2].is_empty(), "{:?}", with_index[2]);
    assert_eq!(
        with_index[0].len(),
        4,
        "BETWEEN 2 AND 4 ⇒ 三条 k=2 + 一条 k=4"
    );
    assert_eq!(with_index[3].len(), 5, "1 < k <= 9 ⇒ 除 k=1 外全在");
    assert_eq!(with_index[5].len(), 0, "4 < k < 9 ⇒ 空（没有 5..8）");

    ok(&mut inst, "DROP INDEX i_t_k");
    for (q, want) in queries.iter().zip(&with_index) {
        assert_eq!(&rows(&ok(&mut inst, q)), want, "`{q}`：两条路径逐行一致");
    }

    // **陈旧项**：把 k=1 改成 k=3（普通索引键列）⇒ 索引里同时有 1 与 3 指向同一行。
    ok(&mut inst, "CREATE INDEX i_t_k ON t (k)");
    assert_eq!(
        affected(&ok(&mut inst, "UPDATE t SET k = 3 WHERE k = 1")),
        1
    );
    let stale = rows(&ok(
        &mut inst,
        "SELECT k, v FROM t WHERE k BETWEEN 1 AND 3 ORDER BY k, v",
    ));
    assert_eq!(
        stale.len(),
        4,
        "1 查不到（陈旧项回表后被谓词否掉）、3 查得到：{stale:?}"
    );
    ok(&mut inst, "DROP INDEX i_t_k");
    assert_eq!(
        rows(&ok(
            &mut inst,
            "SELECT k, v FROM t WHERE k BETWEEN 1 AND 3 ORDER BY k, v"
        )),
        stale,
        "陈旧项：两条路径一致"
    );
    inst.shutdown().expect("关");
}

// ─────────────────── 固定表 `file$`（内省：本工作区的文件清单） ───────────────────

/// **`file$` 能查了**（`spec/SQL.md` 待冻结项 47；`目录详设` §6 的固定表落点）。
///
/// 它钉四件事：① 行来自**控制文件**（权威，不是 `data/` 目录扫描）；
/// ② 列形状（`file#`/`role`/`status`/`flags`/`creation_blocks`/`created_at`/`path`）；
/// ③ 它就是个普通行源——谓词/排序/聚合/别名/连接都照常；
/// ④ **只读**：写目标解析里没有固定表这一格（"不是检查，是没有入口"）。
#[test]
fn file_dollar_exposes_the_workspaces_file_list() {
    let dir = TempDir::new("filedollar");
    let mut inst = create_instance(&params_for(dir.path()), None).expect("建区");

    let got = rows(&ok(
        &mut inst,
        "SELECT \"file#\", role, status FROM file$ ORDER BY \"file#\"",
    ));
    assert_eq!(
        got,
        vec![
            vec!["0".to_owned(), "0".to_owned(), "1".to_owned()],
            vec!["1".to_owned(), "1".to_owned(), "1".to_owned()],
        ],
        "file 0（元数据）与 file 1（撤销）：{got:?}"
    );
    // ② 列形状：七列，名字与顺序照设计。
    match ok(&mut inst, "SELECT * FROM file$").last() {
        Some(QueryResult::Rows { columns, .. }) => {
            let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
            assert_eq!(
                names,
                vec![
                    "file#",
                    "role",
                    "status",
                    "flags",
                    "creation_blocks",
                    "created_at",
                    "path"
                ]
            );
        }
        other => panic!("期待结果集，得到 {other:?}"),
    }
    // ③ 普通行源：谓词 / 别名 / 聚合 / 排序都照常。
    let one = rows(&ok(
        &mut inst,
        "SELECT f.path FROM file$ f WHERE f.\"file#\" = 0",
    ));
    assert_eq!(one.len(), 1);
    assert!(
        one[0][0].ends_with("_meta"),
        "路径应指到 file 0：{:?}",
        one[0]
    );
    let agg = rows(&ok(&mut inst, "SELECT COUNT(*) FROM file$"));
    assert_eq!(agg, vec![vec!["2".to_owned()]]);
    // 与大表连接也可以（固定表就是个表源）。
    ok(&mut inst, "CREATE TABLE t (k NUMBER NOT NULL)");
    ok(
        &mut inst,
        "INSERT INTO t VALUES (0); INSERT INTO t VALUES (1)",
    );
    let joined = rows(&ok(
        &mut inst,
        "SELECT t.k, f.role FROM t JOIN file$ f ON t.k = f.\"file#\" ORDER BY t.k",
    ));
    assert_eq!(joined.len(), 2, "两表连接按 file# 对上：{joined:?}");

    // ④ 只读：写目标解析里**没有固定表这一格**（不是检查，是查不到）。
    let e = err(&mut inst, "INSERT INTO file$ VALUES (9,9,9,9,9,9,'x')");
    assert!(e.contains("不存在"), "写固定表应报名字不存在：{e}");
    let e = err(&mut inst, "DELETE FROM file$");
    assert!(e.contains("不存在"), "{e}");
}

/// **清单即事实**：`file$` 的 `path` 列指向 `data/` 下**真实存在**的文件，
/// 且文件名里的工作区标识与文件头同源（`workspace_ref` = SHA-256(工作区号) 前 8 字节）。
#[test]
fn file_dollar_rows_point_at_real_files() {
    let dir = TempDir::new("filecf");
    let mut inst = create_instance(&params_for(dir.path()), None).expect("建区");
    let paths = rows(&ok(
        &mut inst,
        "SELECT \"file#\", path FROM file$ ORDER BY 1",
    ));
    assert_eq!(paths.len(), 2);
    for (i, p) in paths.iter().enumerate() {
        let path = std::path::Path::new(&p[1]);
        assert!(
            path.is_file(),
            "file {i} 的路径应真实存在：{}",
            path.display()
        );
    }
    assert!(
        paths[0][1].contains("7c9fa136d4413fa6_meta"),
        "{:?}",
        paths[0]
    );
    assert!(
        paths[1][1].contains("7c9fa136d4413fa6_undo"),
        "{:?}",
        paths[1]
    );
    inst.shutdown().expect("关");
}

// ─────────────────── `ORDER BY <表达式>`（不投影的列也能排） ───────────────────

/// **`ORDER BY` 收表达式**（`spec/SQL.md` §：`ORDER BY <表达式> [ASC|DESC]`）——
/// 不投影的列、纯表达式、另一张表的列都能作排序键。
///
/// 物理落点：排序在**投影之下**（那时输入行还在）；输出列键换算成它的投影表达式。
#[test]
fn order_by_accepts_expressions_and_unprojected_columns() {
    let dir = TempDir::new("orderby");
    let mut inst = create_instance(&params_for(dir.path()), None).expect("建区");
    ok(
        &mut inst,
        "CREATE TABLE od (g NUMBER NOT NULL, x NUMBER NOT NULL)",
    );
    ok(
        &mut inst,
        "INSERT INTO od VALUES (1,10); INSERT INTO od VALUES (2,20); \
         INSERT INTO od VALUES (2,21); INSERT INTO od VALUES (2,22)",
    );

    // ① 不投影的列（`g`）作排序键 + 输出列（`x`）混用。
    assert_eq!(
        rows(&ok(&mut inst, "SELECT x FROM od ORDER BY g DESC, x DESC")),
        vec![
            vec!["22".to_owned()],
            vec!["21".to_owned()],
            vec!["20".to_owned()],
            vec!["10".to_owned()],
        ]
    );
    // ② 纯表达式。
    assert_eq!(
        rows(&ok(&mut inst, "SELECT x FROM od ORDER BY x * -1")),
        vec![
            vec!["22".to_owned()],
            vec!["21".to_owned()],
            vec!["20".to_owned()],
            vec!["10".to_owned()],
        ]
    );
    // ③ `ORDER BY <表达式> LIMIT`：`TopN` 也在投影之下（取值必须对）。
    assert_eq!(
        rows(&ok(
            &mut inst,
            "SELECT x FROM od ORDER BY g DESC, x LIMIT 2"
        )),
        vec![vec!["20".to_owned()], vec!["21".to_owned()]]
    );
    // ④ 聚合查询：分组列不投影也能排（键写到聚合输出行的坐标）。
    assert_eq!(
        rows(&ok(
            &mut inst,
            "SELECT COUNT(*) AS n FROM od GROUP BY g ORDER BY g DESC"
        )),
        vec![vec!["3".to_owned()], vec!["1".to_owned()]]
    );
    assert_eq!(
        rows(&ok(
            &mut inst,
            "SELECT COUNT(*) AS n FROM od GROUP BY g ORDER BY g"
        )),
        vec![vec!["1".to_owned()], vec!["3".to_owned()]]
    );
    // ⑤ 两表连接：用**另一张表**的列排。
    assert_eq!(
        rows(&ok(
            &mut inst,
            "SELECT a.g FROM od a JOIN od b ON a.x = b.x ORDER BY b.g DESC, a.g DESC"
        )),
        vec![
            vec!["2".to_owned()],
            vec!["2".to_owned()],
            vec!["2".to_owned()],
            vec!["1".to_owned()],
        ]
    );

    // ⑥ 具名拒绝：名字两处都没有；`DISTINCT` 与表达式键；聚合里排非分组列。
    let e = err(&mut inst, "SELECT x FROM od ORDER BY nope");
    assert!(
        e.contains("既不在 SELECT 的输出列里") && e.contains("也不是 FROM 里的列"),
        "两处都要点到：{e}"
    );
    let e = err(&mut inst, "SELECT DISTINCT g FROM od ORDER BY x");
    assert!(e.contains("DISTINCT"), "{e}");
    let e = err(&mut inst, "SELECT COUNT(*) FROM od GROUP BY g ORDER BY x");
    assert!(e.contains("GROUP BY"), "{e}");
    inst.shutdown().expect("关");
}

/// **`IN (值表)` 走索引**（N 次点查）——差分：有索引与 `DROP INDEX` 逐行一致。
///
/// 两条纪律单钉：**重复值去重**（`IN (2,2)` 不许把同一行出两次）与
/// **陈旧索引项**（改键列留下的旧项落在候选里，由 `Filter` 否掉）。
#[test]
fn in_list_lookups_match_the_full_scan() {
    let dir = TempDir::new("inlist");
    let mut inst = create_instance(&params_for(dir.path()), None).expect("建区");
    ok(
        &mut inst,
        "CREATE TABLE t (k NUMBER NOT NULL, v VARCHAR2(8))",
    );
    ok(&mut inst, "CREATE INDEX i_t_k ON t (k)"); // **普通**索引：允许改键列
    ok(
        &mut inst,
        "INSERT INTO t VALUES (1,'a'); INSERT INTO t VALUES (2,'b'); \
         INSERT INTO t VALUES (2,'c'); INSERT INTO t VALUES (4,'d')",
    );

    let queries = [
        "SELECT k, v FROM t WHERE k IN (1, 4) ORDER BY k, v",
        // **重复值**：同一行只许出一次。
        "SELECT k, v FROM t WHERE k IN (2, 2, 2) ORDER BY k, v",
        // 命中的不存在值 + 全是 NULL（永不出行）。
        "SELECT k, v FROM t WHERE k IN (7, 8) ORDER BY k, v",
        "SELECT k, v FROM t WHERE k IN (NULL) ORDER BY k, v",
        // NULL 与非 NULL 混着：只有非 NULL 是候选。
        "SELECT k, v FROM t WHERE k IN (2, NULL) ORDER BY k, v",
        // 列表里混参数（运行期求值；按行去重兜底）。
        "SELECT k, v FROM t WHERE k IN (1, 2) ORDER BY k, v",
    ];
    let with_index: Vec<Vec<Vec<String>>> =
        queries.iter().map(|q| rows(&ok(&mut inst, q))).collect();
    assert_eq!(
        with_index[1].len(),
        2,
        "`IN (2,2,2)` 只出 k=2 的那两行（不许重复）：{:?}",
        with_index[1]
    );
    assert!(with_index[2].is_empty(), "没有命中的值 ⇒ 零行");
    assert!(with_index[3].is_empty(), "`IN (NULL)` ⇒ 零行");

    // 没索引：同样的查询（去掉索引后）逐行一致。
    ok(&mut inst, "DROP INDEX i_t_k");
    for (q, want) in queries.iter().zip(&with_index) {
        assert_eq!(&rows(&ok(&mut inst, q)), want, "`{q}`：两条路径逐行一致");
    }

    // **陈旧项**：k=1 改成 3 ⇒ 索引里 1（旧项）与 3（新项）都在——两个**点**
    // 都会带出那一条行（旧项经转发/同槽落回同一物理行）⇒ 只许出一行。
    // （这条钉的正是"点必须落在同一个算子"：串成 `Append` 时它出过两次。）
    ok(&mut inst, "CREATE INDEX i_t_k ON t (k)");
    assert_eq!(
        affected(&ok(&mut inst, "UPDATE t SET k = 3 WHERE k = 1")),
        1
    );
    let stale = rows(&ok(
        &mut inst,
        "SELECT k, v FROM t WHERE k IN (1, 3) ORDER BY k, v",
    ));
    assert_eq!(
        stale.len(),
        1,
        "只有 k=3 那一行（旧的 1 被谓词否掉）：{stale:?}"
    );
    ok(&mut inst, "DROP INDEX i_t_k");
    assert_eq!(
        rows(&ok(
            &mut inst,
            "SELECT k, v FROM t WHERE k IN (1, 3) ORDER BY k, v"
        )),
        stale
    );
    inst.shutdown().expect("关");
}
