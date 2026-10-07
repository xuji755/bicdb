//! **计划形状**：① 访问路径选择（规则式——等值/有界范围命中**单列索引** ⇒
//! `IndexScan`，否则 `SeqScan`）；② **排序的落点**（输出列键排在投影之上、
//! 输入表达式键排在投影之下）。
//!
//! 规则照 Oracle RBO 的 15 级排名（`doc/evidence/index-access-20261007/` §1）：
//! 唯一键单行访问（4） > 单列索引（9） > 全表扫描（15）。**没有统计信息就不做
//! 代价式选择**——本文件钉的是"选得对"与"没选错"，运行期正确性另由
//! `crates/cli/tests/e2e.rs` 与 `crates/exec/tests/index_join.rs` 钉。
//!
//! 真件：真目录（file 0）+ 真 DDL 建表建索引 → 真绑定 → 真计划。

use std::path::Path;

use bicdb_catalog::ddl::{self, ColumnSpec, IndexSpec, TableOptions, TableSpec};
use bicdb_catalog::Catalog;
use bicdb_common::seq::{CommitSeq, Lsn};
use bicdb_exec::{CmpOp, Expr, PlanNode};
use bicdb_sql::bind::{bind_statement, CatalogViewImpl, NameResolver};
use bicdb_sql::parser::parse_many;
use bicdb_sql::plan::{plan_statement, PhysicalPlan};
use bicdb_storage::buffer::{BufferPool, CacheConfig, SystemClock, WalGuard};
use bicdb_storage::controlfile::{
    ArchiveMode, ArchiveRecord, ControlFile, RedoEntries, WorkspaceEntry,
};
use bicdb_storage::datafile::DataFile;
use bicdb_storage::undo::{create_undo_segment, UndoChain};
use bicdb_txn::engine::Engine;
use bicdb_wal::group::{GroupSpec, GroupWriter};
use bicdb_workspace::io::MemFileIo;
use bicdb_workspace::WorkspaceId;

const WS: [u8; 8] = [23u8; 8];

fn seq(v: u64) -> CommitSeq {
    CommitSeq::from_raw(v).unwrap()
}
fn lsn(v: u64) -> Lsn {
    Lsn::from_raw(v).unwrap()
}

struct FakeWal;
impl WalGuard for FakeWal {
    fn durable_lsn(&self) -> Lsn {
        Lsn::from_raw(u64::MAX >> 16).unwrap()
    }
    fn ensure_durable(&self, _t: Lsn) -> std::io::Result<()> {
        Ok(())
    }
}

/// 一个工作区（file 0 + 池 + 日志 + 引擎）。
struct Ws {
    pool: &'static BufferPool<'static>,
    engine: &'static Engine<'static, 'static, 'static, 'static>,
    path: String,
}

fn workspace(io: &'static MemFileIo, tag: &str) -> Ws {
    let undo_path = format!("/mem/{tag}_undo.dat");
    let data_path = format!("/mem/{tag}_file0.dat");
    let undo_file: &'static mut DataFile<'static> = Box::leak(Box::new(
        DataFile::create(io, Path::new(&undo_path), 1, 1, WS, 512).unwrap(),
    ));
    let undo_handle = undo_file.handle();
    let undo_seg = create_undo_segment(undo_file, 2, 3, 4).unwrap();
    let mut file0 = DataFile::create(
        io,
        Path::new(&data_path),
        0,
        bicdb_storage::bitmap::META_ROLE,
        WS,
        bicdb_storage::bitmap::FileLayout::meta().min_file_blocks() + 512,
    )
    .unwrap();
    let built = bicdb_catalog::create_dictionary(&mut file0, WS, false).unwrap();
    let mut cat = Catalog::from_entries(file0, built.entries.clone()).unwrap();
    cat.seed_own_dictionary(&built).unwrap();
    drop(cat);
    let handle = DataFile::open(io, Path::new(&data_path)).unwrap().handle();
    let pool: &'static BufferPool<'static> = Box::leak(Box::new(
        BufferPool::with_config(
            io,
            64,
            move |_ws, r| match r.file_id() {
                0 => Some((handle, r.block_id())),
                1 => Some((undo_handle, r.block_id())),
                _ => None,
            },
            FakeWal,
            SystemClock,
            CacheConfig::for_capacity(64),
        )
        .unwrap(),
    ));
    let cf_a = format!("/mem/{tag}_cf_a");
    let cf_b = format!("/mem/{tag}_cf_b");
    let wal = format!("/mem/{tag}_wal");
    let cf: &'static mut ControlFile<'static> = Box::leak(Box::new(
        ControlFile::format(
            io,
            Path::new(&cf_a),
            Path::new(&cf_b),
            &WorkspaceEntry {
                workspace_id: WorkspaceId::from_raw(1).unwrap(),
                created_at: 0,
                derived_from: None,
                derived_at_seq: seq(0),
            },
            &RedoEntries::new(2, 1).unwrap(),
            &ArchiveRecord::new(ArchiveMode::NoArchive),
        )
        .unwrap(),
    ));
    let spec = GroupSpec::new(2, 1, 8192).unwrap();
    let writer = GroupWriter::create(io, cf, Path::new(&wal), spec, lsn(0)).unwrap();
    let engine: &'static Engine<'static, 'static, 'static, 'static> =
        Box::leak(Box::new(Engine::new(
            pool,
            writer,
            UndoChain::open(undo_seg).with_pool(pool),
            seq(0),
        )));
    Ws {
        pool,
        engine,
        path: data_path,
    }
}

fn open(io: &'static MemFileIo, ws: &Ws) -> Catalog<'static> {
    let mut cat = Catalog::open(io, Path::new(&ws.path)).unwrap();
    cat.attach_pool(ws.pool);
    cat
}

fn table_spec(name: &str) -> TableSpec {
    TableSpec {
        name: name.to_owned(),
        columns: vec![
            ColumnSpec {
                name: "id".to_owned(),
                type_code: bicdb_catalog::ColTypeCode::Number,
                length: 0,
                precision: None,
                scale: None,
                nullable: false,
            },
            ColumnSpec {
                name: "tag".to_owned(),
                type_code: bicdb_catalog::ColTypeCode::Varchar2,
                length: 64,
                precision: None,
                scale: None,
                nullable: true,
            },
            ColumnSpec {
                name: "tag2".to_owned(),
                type_code: bicdb_catalog::ColTypeCode::Varchar2,
                length: 64,
                precision: None,
                scale: None,
                nullable: true,
            },
        ],
        options: TableOptions::default(),
    }
}

/// 建表 + 索引（`indexes` = `(名, 唯一, 列)`）。
fn fixture(tag: &str, indexes: &[(&str, bool, &[&str])]) -> (Catalog<'static>, CommitSeq) {
    let io: &'static MemFileIo = Box::leak(Box::new(MemFileIo::new()));
    io.add_dir("/mem");
    let ws = workspace(io, tag);
    let mut cat = open(io, &ws);
    ddl::init_dictionary_tables(&mut cat, ws.engine).unwrap();
    let t = ddl::create_table(&mut cat, ws.engine, &table_spec("t")).unwrap();
    let mut last = t.commit_seq;
    for (name, unique, cols) in indexes {
        let spec = IndexSpec {
            name: (*name).to_owned(),
            table: "t".to_owned(),
            unique: *unique,
            columns: cols.iter().map(|c| (*c).to_owned()).collect(),
        };
        let out = ddl::create_index(&mut cat, ws.engine, &spec).unwrap();
        last = out.commit_seq;
    }
    (cat, seq(last))
}

/// 建**两张**表（`t`/`u`，同形）+ 各自的索引；返回（目录, 快照）。
fn fixture_two(
    tag: &str,
    idx_t: &[(&str, bool, &[&str])],
    idx_u: &[(&str, bool, &[&str])],
) -> (Catalog<'static>, CommitSeq) {
    let io: &'static MemFileIo = Box::leak(Box::new(MemFileIo::new()));
    io.add_dir("/mem");
    let ws = workspace(io, tag);
    let mut cat = open(io, &ws);
    ddl::init_dictionary_tables(&mut cat, ws.engine).unwrap();
    let mut last = 0;
    for name in ["t", "u"] {
        let out = ddl::create_table(&mut cat, ws.engine, &table_spec(name)).unwrap();
        last = out.commit_seq;
    }
    for (table, list) in [("t", idx_t), ("u", idx_u)] {
        for (name, unique, cols) in list {
            let spec = IndexSpec {
                name: (*name).to_owned(),
                table: (*table).to_owned(),
                unique: *unique,
                columns: cols.iter().map(|c| (*c).to_owned()).collect(),
            };
            let out = ddl::create_index(&mut cat, ws.engine, &spec).unwrap();
            last = out.commit_seq;
        }
    }
    (cat, seq(last))
}

/// 走到 `NestedLoop`（穿过 `Filter`/`Project`）。
fn join_node(plan: &PhysicalPlan) -> &PlanNode {
    fn walk(n: &PlanNode) -> Option<&PlanNode> {
        match n {
            PlanNode::NestedLoop { .. } => Some(n),
            PlanNode::Filter { input, .. } | PlanNode::Project { input, .. } => walk(input),
            _ => None,
        }
    }
    walk(&plan.node).expect("应有 NestedLoop")
}

/// 取 `NestedLoop` 的内表节点。
fn join_inner(plan: &PhysicalPlan) -> PlanNode {
    match join_node(plan) {
        PlanNode::NestedLoop { inner, .. } => inner.as_ref().clone(),
        _ => unreachable!("join_node 已保证"),
    }
}

/// 绑定 + 计划一条 SQL（真目录、真计划）。
fn plan_of(cat: &mut Catalog<'static>, snap: CommitSeq, sql: &str) -> PhysicalPlan {
    let stmts = parse_many(sql).expect("解析");
    let bound = {
        let mut view = CatalogViewImpl::new(cat, snap);
        let mut r = NameResolver::new(&mut view);
        bind_statement(&mut r, &stmts[0]).expect("绑定")
    };
    let mut view = CatalogViewImpl::new(cat, snap);
    plan_statement(&bound, &mut view)
        .expect("计划")
        .expect("有物理计划")
}

/// 取计划树最里层的扫描算子（`Filter`/`Project` 之下）。
fn scan_node(n: &PlanNode) -> &PlanNode {
    match n {
        PlanNode::Filter { input, .. } | PlanNode::Project { input, .. } => scan_node(input),
        other => other,
    }
}

fn assert_index_scan(plan: &PhysicalPlan, want_kind: bicdb_exec::ColKind) {
    match scan_node(&plan.node) {
        PlanNode::IndexScan {
            seg_page0,
            key_kind,
            low,
            high,
            covered,
            limit,
            ..
        } => {
            assert_eq!(*key_kind, want_kind, "键列形态");
            assert!(
                low.is_some() && high.is_some(),
                "等值查询给的是闭区间 [k, k]"
            );
            assert!(!covered, "本仓不做仅索引扫描（无页级可见性位图）");
            assert!(
                limit.is_none(),
                "不给 limit：同键的第一条可能是陈旧项，提前限行会漏行"
            );
            assert!(*seg_page0 > 0, "索引段头块");
        }
        other => panic!("期望 IndexScan，实得 {other:?}"),
    }
}

fn assert_seq_scan(plan: &PhysicalPlan) {
    match scan_node(&plan.node) {
        PlanNode::SeqScan { .. } => {}
        other => panic!("期望 SeqScan，实得 {other:?}"),
    }
}

#[test]
fn a_unique_index_equality_becomes_a_point_lookup() {
    let (mut cat, snap) = fixture("access_unique", &[("i_t_id", true, &["id"])]);
    let plan = plan_of(&mut cat, snap, "SELECT tag FROM t WHERE id = 5");
    assert_index_scan(&plan, bicdb_exec::ColKind::Number);
    // 上下界同一个值 ⇒ 点查（同键集合）。
    if let PlanNode::IndexScan { low, high, .. } = scan_node(&plan.node) {
        assert_eq!(low, high, "等值 = 上下界同值");
        assert!(matches!(low, Some(Expr::Literal(_))), "{low:?}");
    }
}

#[test]
fn a_parameterized_equality_also_uses_the_index() {
    let (mut cat, snap) = fixture("access_param", &[("i_t_id", true, &["id"])]);
    let plan = plan_of(&mut cat, snap, "SELECT tag FROM t WHERE id = :id");
    assert_index_scan(&plan, bicdb_exec::ColKind::Number);
}

#[test]
fn a_non_unique_index_equality_uses_it_too_and_prefers_the_unique_one() {
    // 普通索引（字节串键）⇒ 也可以用（取到的是一组同键行）。
    let (mut cat, snap) = fixture("access_nonunique", &[("i_t_tag", false, &["tag"])]);
    let plan = plan_of(&mut cat, snap, "SELECT id FROM t WHERE tag = 'x'");
    assert_index_scan(&plan, bicdb_exec::ColKind::Bytes);

    // 两个索引都命中同一列 ⇒ **唯一索引优先**（RBO：4 在 9 之前）。
    let (mut cat2, snap2) = fixture(
        "access_prefer",
        &[
            ("i_t_id_plain", false, &["id"]),
            ("i_t_id_uniq", true, &["id"]),
        ],
    );
    let plan2 = plan_of(&mut cat2, snap2, "SELECT tag FROM t WHERE id = 5");
    let PlanNode::IndexScan { seg_page0, .. } = scan_node(&plan2.node) else {
        panic!("期望 IndexScan");
    };
    // 唯一索引的段头块（与普通索引不同）。
    let uniq = bicdb_catalog::ddl::live_segment_location(&mut cat2, 102).unwrap();
    let plain = bicdb_catalog::ddl::live_segment_location(&mut cat2, 101).unwrap();
    assert_ne!(uniq.1, plain.1, "两个索引各有各的段");
    assert_eq!(*seg_page0, uniq.1, "选的应是**唯一**索引");
}

#[test]
fn everything_else_stays_a_full_scan() {
    // 没索引的列。
    let (mut cat, snap) = fixture("access_none", &[("i_t_id", true, &["id"])]);
    assert_seq_scan(&plan_of(&mut cat, snap, "SELECT id FROM t WHERE tag = 'x'"));
    // 非等值（范围）——本切片只做等值（RBO 排名 10/11 留后续）。
    assert_seq_scan(&plan_of(&mut cat, snap, "SELECT id FROM t WHERE id > 5"));
    // 析取（`OR` 里各自等值也不行）。
    assert_seq_scan(&plan_of(
        &mut cat,
        snap,
        "SELECT id FROM t WHERE id = 5 OR id = 6",
    ));
    // `IS NULL` 不是等值。
    assert_seq_scan(&plan_of(
        &mut cat,
        snap,
        "SELECT id FROM t WHERE tag IS NULL",
    ));
    // 列对列比较（值侧有列引用）。
    assert_seq_scan(&plan_of(
        &mut cat,
        snap,
        "SELECT id FROM t WHERE tag = tag2",
    ));
    // 注：**形态不符**（`WHERE id = 'x'`）在**绑定期**就被拒（`TypeMismatch`），
    // 到不了选路——计划层的形态核对因此是第二道防线（防御性，不指望它兜底）。
}

#[test]
fn multi_column_indexes_are_not_chosen_yet() {
    let (mut cat, snap) = fixture("access_multi", &[("i_t_tt", false, &["tag", "tag2"])]);
    // 复合索引的**全键等值**也先不走（多列键编码要按分量拼，随下一片）。
    assert_seq_scan(&plan_of(
        &mut cat,
        snap,
        "SELECT id FROM t WHERE tag = 'a' AND tag2 = 'b'",
    ));
    // 前缀等值更不走。
    assert_seq_scan(&plan_of(&mut cat, snap, "SELECT id FROM t WHERE tag = 'a'"));
}

#[test]
fn an_equality_beside_other_conjuncts_still_selects_the_index() {
    let (mut cat, snap) = fixture("access_conj", &[("i_t_id", true, &["id"])]);
    let plan = plan_of(
        &mut cat,
        snap,
        "SELECT tag FROM t WHERE tag = 'x' AND id = 5",
    );
    assert_index_scan(&plan, bicdb_exec::ColKind::Number);
    // 谓词仍整条挂在 `Filter` 上（索引只缩小候选，`CmpOp::Eq` 由 Filter 复核）。
    let PlanNode::Project { input, .. } = &plan.node else {
        panic!("计划顶上是投影");
    };
    let PlanNode::Filter { predicate, .. } = input.as_ref() else {
        panic!("投影之下应是 Filter");
    };
    assert!(
        matches!(predicate, Expr::And(_)),
        "整条 WHERE 都在 Filter 上：{predicate:?}"
    );
    let _ = CmpOp::Eq;
}

// ─────────────────── 连接的索引内表（IndexNL） ───────────────────

#[test]
fn a_join_probes_the_inner_table_through_its_index() {
    let (mut cat, snap) = fixture_two(
        "nl_on",
        &[("i_t_id", true, &["id"])],
        &[("i_u_id", true, &["id"])],
    );
    let plan = plan_of(&mut cat, snap, "SELECT t.id FROM t JOIN u ON t.id = u.id");
    match join_inner(&plan) {
        PlanNode::IndexScan {
            low,
            high,
            limit,
            covered,
            ..
        } => {
            assert_eq!(low, high, "探测 = 上下界同一个值");
            assert!(
                matches!(low, Some(Expr::Param(0))),
                "界来自内表参数：{low:?}"
            );
            assert!(limit.is_none(), "同 try_index_scan：不给 limit");
            assert!(!covered, "恒回表");
        }
        other => panic!("内表应是 IndexScan，实得 {other:?}"),
    }
    // 内表参数 = 外层列（外层是 `t`，列 0 = id）。
    let PlanNode::NestedLoop { inner_params, .. } = join_node(&plan) else {
        unreachable!("join_node 已保证")
    };
    assert_eq!(inner_params, &vec![Expr::Column(0)], "内表参数 = 外层 id");
}

#[test]
fn a_comma_join_with_a_where_equality_also_probes_the_index() {
    // `FROM t, u WHERE t.id = u.id`：合取式在 WHERE 里（内连接 ⇒ 可用）。
    let (mut cat, snap) = fixture_two(
        "nl_where",
        &[("i_t_id", true, &["id"])],
        &[("i_u_id", true, &["id"])],
    );
    let plan = plan_of(&mut cat, snap, "SELECT t.id FROM t, u WHERE t.id = u.id");
    assert!(
        matches!(join_inner(&plan), PlanNode::IndexScan { .. }),
        "应探测索引"
    );
}

#[test]
fn a_left_join_probes_the_index_only_from_the_on_clause() {
    let (mut cat, snap) = fixture_two(
        "nl_left",
        &[("i_t_id", true, &["id"])],
        &[("i_u_id", true, &["id"])],
    );
    // ON 里的等值 ⇒ 可以（`matched` 只在满足 ON 的候选里算，语义不变）。
    let plan = plan_of(
        &mut cat,
        snap,
        "SELECT t.id FROM t LEFT JOIN u ON t.id = u.id",
    );
    assert!(
        matches!(join_inner(&plan), PlanNode::IndexScan { .. }),
        "ON 里 ⇒ 探测"
    );
    // WHERE 里的等值 + 左外 ⇒ **不**探测（第一版不摊"补 NULL 行必被否掉"的论证）。
    let plan = plan_of(
        &mut cat,
        snap,
        "SELECT t.id FROM t LEFT JOIN u ON t.id = 5 WHERE t.id = u.id",
    );
    assert!(
        matches!(join_inner(&plan), PlanNode::SeqScan { .. }),
        "左外 + WHERE ⇒ 顺序重扫"
    );
}

#[test]
fn a_join_without_an_index_on_the_inner_column_stays_a_rescan() {
    let (mut cat, snap) = fixture_two(
        "nl_none",
        &[("i_t_id", true, &["id"])],
        &[("i_u_tag", false, &["tag"])],
    );
    // 内表 `u.id` 没索引（只有 u.tag）⇒ 顺序重扫。
    let plan = plan_of(&mut cat, snap, "SELECT t.id FROM t JOIN u ON t.id = u.id");
    assert!(matches!(join_inner(&plan), PlanNode::SeqScan { .. }));
    // 内表 `u.tag` 有索引 ⇒ 探测它（键形态 = 字节串）。
    let plan = plan_of(&mut cat, snap, "SELECT t.id FROM t JOIN u ON t.tag = u.tag");
    match join_inner(&plan) {
        PlanNode::IndexScan { key_kind, .. } => assert_eq!(key_kind, bicdb_exec::ColKind::Bytes),
        other => panic!("内表应是 IndexScan，实得 {other:?}"),
    }
}

// ─────────────────── 有界范围（RBO 排名 10） ───────────────────

#[test]
fn bounded_ranges_use_the_index_with_the_right_open_closed_ends() {
    let (mut cat, snap) = fixture("access_range", &[("i_t_id", false, &["id"])]);
    // `BETWEEN` ⇒ 两端闭。
    let plan = plan_of(&mut cat, snap, "SELECT tag FROM t WHERE id BETWEEN 3 AND 7");
    match scan_node(&plan.node) {
        PlanNode::IndexScan {
            low_exclusive,
            high_exclusive,
            ..
        } => {
            assert!(!low_exclusive && !high_exclusive, "BETWEEN 两端都闭");
        }
        other => panic!("期望 IndexScan，实得 {other:?}"),
    }
    // `>= AND <=` ⇒ 两端闭；`> AND <` ⇒ 两端开。
    for (sql, want_low_x, want_high_x) in [
        ("SELECT tag FROM t WHERE id >= 3 AND id <= 7", false, false),
        ("SELECT tag FROM t WHERE id > 3 AND id < 7", true, true),
        ("SELECT tag FROM t WHERE id > 3 AND id <= 7", true, false),
        // **翻向**：`7 > id` 等价于 `id < 7`。
        ("SELECT tag FROM t WHERE 3 <= id AND 7 > id", false, true),
    ] {
        let plan = plan_of(&mut cat, snap, sql);
        match scan_node(&plan.node) {
            PlanNode::IndexScan {
                low_exclusive,
                high_exclusive,
                ..
            } => assert_eq!(
                (*low_exclusive, *high_exclusive),
                (want_low_x, want_high_x),
                "{sql}"
            ),
            other => panic!("{sql}：期望 IndexScan，实得 {other:?}"),
        }
    }
    // **无界范围不接**（条目一次收齐 ⇒ 只给一侧的界会把整棵树搬进内存）。
    assert_seq_scan(&plan_of(&mut cat, snap, "SELECT tag FROM t WHERE id > 3"));
    assert_seq_scan(&plan_of(&mut cat, snap, "SELECT tag FROM t WHERE id <= 7"));
    // 列对列、`<>`、`OR` 都不接（列对列要选**同形态**的两列，否则绑定期就拒了）。
    assert_seq_scan(&plan_of(
        &mut cat,
        snap,
        "SELECT id FROM t WHERE tag > tag2 AND tag < 'z'",
    ));
    assert_seq_scan(&plan_of(&mut cat, snap, "SELECT tag FROM t WHERE id <> 3"));
    assert_seq_scan(&plan_of(
        &mut cat,
        snap,
        "SELECT tag FROM t WHERE id > 3 OR id < 7",
    ));
    // 没索引的列上照样不接。
    assert_seq_scan(&plan_of(
        &mut cat,
        snap,
        "SELECT id FROM t WHERE tag > 'a' AND tag < 'z'",
    ));
}

// ─────────────────── 排序的落点（`ORDER BY <表达式>`） ───────────────────

/// **输出列键 ⇒ 排在投影之上**（现状形态：`Sort`/`TopN` 在全树的顶上之下）。
#[test]
fn output_column_sort_keys_stay_above_the_projection() {
    let (mut cat, snap) = fixture("sort_output", &[("i_t_id", true, &["id"])]);
    let plan = plan_of(&mut cat, snap, "SELECT tag FROM t ORDER BY 1 DESC");
    // 形状：Sort(Project(…))——投影在下、排序在上（键是输出行坐标）。
    let PlanNode::Sort { input, keys } = &plan.node else {
        panic!("顶上应是 Sort，实得 {:?}", plan.node);
    };
    assert_eq!(keys.len(), 1);
    assert!(keys[0].desc);
    assert!(matches!(keys[0].expr, Expr::Column(0)));
    assert!(
        matches!(input.as_ref(), PlanNode::Project { .. }),
        "投影在排序之下"
    );
    // **`ORDER BY <输出名>`** 与序号同一条路。
    let plan = plan_of(&mut cat, snap, "SELECT tag FROM t ORDER BY tag");
    assert!(matches!(plan.node, PlanNode::Sort { .. }));
}

/// **输入表达式键 ⇒ 排到投影之下**（键换算成输入行坐标）。
#[test]
fn input_expression_sort_keys_go_below_the_projection() {
    let (mut cat, snap) = fixture("sort_input", &[("i_t_id", true, &["id"])]);
    // `ORDER BY id`（**不投影** id）：排序必须在投影之下（那时 id 还在）。
    let plan = plan_of(&mut cat, snap, "SELECT tag FROM t ORDER BY id DESC");
    let PlanNode::Project { input, .. } = &plan.node else {
        panic!("顶上应是投影，实得 {:?}", plan.node);
    };
    let PlanNode::Sort { keys, .. } = input.as_ref() else {
        panic!("投影之下应是 Sort，实得 {input:?}");
    };
    assert_eq!(keys.len(), 1);
    assert!(keys[0].desc);
    assert!(
        matches!(keys[0].expr, Expr::Column(0)),
        "键 = 输入行的 id 列（表列 0）：{:?}",
        keys[0].expr
    );
    // **输出列键在混用时要换算**：`ORDER BY id, tag` —— tag 是输出列（投影第 0 项），
    // 换算成它的投影表达式（输入行的 tag 列 = 输入列 1）。
    let plan = plan_of(&mut cat, snap, "SELECT tag FROM t ORDER BY id, tag");
    let PlanNode::Project { input, .. } = &plan.node else {
        panic!("顶上应是投影");
    };
    let PlanNode::Sort { keys, .. } = input.as_ref() else {
        panic!("投影之下应是 Sort");
    };
    assert_eq!(keys.len(), 2);
    assert!(matches!(keys[0].expr, Expr::Column(0)), "id = 输入列 0");
    assert!(
        matches!(keys[1].expr, Expr::Column(1)),
        "tag = 输入列 1（投影的输入）"
    );
    // `ORDER BY <表达式>`：表达式原样落到输入行。
    let plan = plan_of(&mut cat, snap, "SELECT tag FROM t ORDER BY id * 2 + 1");
    let PlanNode::Project { input, .. } = &plan.node else {
        panic!("顶上应是投影");
    };
    let PlanNode::Sort { keys, .. } = input.as_ref() else {
        panic!("投影之下应是 Sort");
    };
    assert!(
        matches!(keys[0].expr, Expr::Arith { .. }),
        "{:?}",
        keys[0].expr
    );
    // `LIMIT` 并存 ⇒ `TopN` 仍在投影之下。
    let plan = plan_of(&mut cat, snap, "SELECT tag FROM t ORDER BY id LIMIT 3");
    let PlanNode::Limit { input, .. } = &plan.node else {
        panic!("顶上应是 Limit");
    };
    let PlanNode::Project { input, .. } = input.as_ref() else {
        panic!("Limit 之下应是投影");
    };
    assert!(
        matches!(input.as_ref(), PlanNode::TopN { .. }),
        "投影之下应是 TopN"
    );
}

// ─────────────────── `IN (值表)` ⇒ N 次点查（`Append`） ───────────────────

/// 走到 `Filter` 之下的那棵树（`Project` → `Filter` → …）。
fn filter_input(plan: &PhysicalPlan) -> &PlanNode {
    let PlanNode::Project { input, .. } = &plan.node else {
        panic!("顶上应是投影：{:?}", plan.node);
    };
    let PlanNode::Filter { input, .. } = input.as_ref() else {
        panic!("投影之下应是 Filter（整条谓词仍要复核）：{input:?}");
    };
    input.as_ref()
}

#[test]
fn an_in_list_over_an_indexed_column_becomes_point_lookups() {
    let (mut cat, snap) = fixture("in_list", &[("i_t_id", true, &["id"])]);
    // `Filter` 之下是**一个** `IndexScan`，带三个点（不是三支算子串起来——
    // 去重集按算子一份，点必须落在同一个算子里）。
    let plan = plan_of(&mut cat, snap, "SELECT tag FROM t WHERE id IN (1, 3, 5)");
    let PlanNode::IndexScan {
        points, low, high, ..
    } = filter_input(&plan)
    else {
        panic!("Filter 之下应是 IndexScan，实得 {:?}", filter_input(&plan));
    };
    assert_eq!(points.len(), 3, "三个值三个点");
    assert!(low.is_none() && high.is_none(), "多点形态不用区间界");
    // **重复字面量去重**（少一次点查；重复值的行级去重另有兜底）。
    let plan = plan_of(&mut cat, snap, "SELECT tag FROM t WHERE id IN (2, 2, 2)");
    let PlanNode::IndexScan { points, .. } = filter_input(&plan) else {
        panic!("应是 IndexScan");
    };
    assert_eq!(points.len(), 1, "三个 2 去重成一个点");
    // **`NULL` 元素丢掉**（永不匹配）；整表 NULL ⇒ 没有候选，退全表扫描。
    let plan = plan_of(&mut cat, snap, "SELECT tag FROM t WHERE id IN (1, NULL)");
    let PlanNode::IndexScan { points, .. } = filter_input(&plan) else {
        panic!("应是 IndexScan");
    };
    assert_eq!(points.len(), 1, "只有 1 是候选");
    assert_seq_scan(&plan_of(
        &mut cat,
        snap,
        "SELECT tag FROM t WHERE id IN (NULL)",
    ));
    // **参数也可以用**（运行期求值 + 按行去重兜底）。
    let plan = plan_of(&mut cat, snap, "SELECT tag FROM t WHERE id IN (1, :p)");
    let PlanNode::IndexScan { points, .. } = filter_input(&plan) else {
        panic!("参数形态也走索引，实得 {:?}", filter_input(&plan));
    };
    assert_eq!(points.len(), 2);
    // 没索引的列 ⇒ 全表。
    assert_seq_scan(&plan_of(
        &mut cat,
        snap,
        "SELECT id FROM t WHERE tag IN ('a', 'b')",
    ));
    // `NOT IN` 是取反，不是成员——不接（`Not(InList)` 不是合取项）。
    assert_seq_scan(&plan_of(
        &mut cat,
        snap,
        "SELECT tag FROM t WHERE id NOT IN (1, 3)",
    ));
}
