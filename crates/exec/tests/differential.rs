//! **切片 1 验收**：真表（真段 + 真缓冲池 + 真撤销链）上
//! **算子执行器 ↔ 直译执行器逐行差分**（设计 §6；REQ-SQL-005 的验收形态）。
//!
//! 两路共享同一份语义查询（[`SelectQuery`]）：直译路径一个循环走完、
//! 算子路径先 `to_plan()` 再建树——结果必须逐行一致。
//! 另钉：LIMIT 的**真短路**（扫描行数可观测）、取消路径、空表/空结果。

use std::path::Path;
use std::sync::atomic::AtomicBool;

use bicdb_common::seq::{CommitSeq, Lsn};
use bicdb_exec::{
    build, collect, encode_row, execute_direct, CmpOp, ColKind, ExecContext, ExecError, Expr, Row,
    RowCursor, RowShape, SelectQuery, Value,
};
use bicdb_storage::buffer::{BufferKey, BufferPool, WalGuard};
use bicdb_storage::controlfile::{ArchiveRecord, ControlFile, RedoEntries, WorkspaceEntry};
use bicdb_storage::datafile::DataFile;
use bicdb_storage::heap::InsertPolicy;
use bicdb_storage::page::{Page, PageType};
use bicdb_storage::rowid::Rdba;
use bicdb_storage::scan::HeapScanner;
use bicdb_storage::segment::{SegType, Segment};
use bicdb_storage::undo::{create_undo_segment, UndoChain};
use bicdb_txn::write::{begin, commit, insert_row};
use bicdb_types::Number;
use bicdb_wal::group::{GroupSpec, GroupWriter};
use bicdb_workspace::id::WorkspaceId;
use bicdb_workspace::io::MemFileIo;

const UNDO_F: &str = "/mem/exec_undo.dat";
const DATA_F: &str = "/mem/exec_data.dat";
const A: &str = "/mem/exec_c1.ctl";
const B: &str = "/mem/exec_c2.ctl";
const WAL: &str = "/mem/exec_wal";
const WS: [u8; 8] = [8u8; 8];
const DATA_FID: u16 = 3;

fn seq(v: u64) -> CommitSeq {
    CommitSeq::from_raw(v).unwrap()
}

fn lsn(v: u64) -> Lsn {
    Lsn::from_raw(v).unwrap()
}

fn rdba(block: u32) -> Rdba {
    Rdba::from_parts(DATA_FID, block).unwrap()
}

/// 假 WAL 水位（池不催刷；提交路径单独 flush）。
struct FakeWal;
impl WalGuard for FakeWal {
    fn durable_lsn(&self) -> Lsn {
        lsn(u64::MAX >> 16)
    }
    fn ensure_durable(&self, _t: Lsn) -> std::io::Result<()> {
        Ok(())
    }
}

/// 表形状：`(id NUMBER, tag BYTES)` —— 全变长布局（切片 1 约定）。
fn shape() -> RowShape {
    RowShape::new(vec![ColKind::Number, ColKind::Bytes])
}

fn row(id: i64, tag: &str) -> Row {
    Row::new(vec![
        Value::Number(Number::parse(&id.to_string()).unwrap()),
        Value::Bytes(tag.as_bytes().to_vec()),
    ])
}

/// 建表（3 张已格式化的空页）并插入 `rows`；池/链都是 `'static` 件
/// （测试里 `Box::leak`——与 `bicdb-index` 的端到端用例同法）。
struct Fixture {
    pool: &'static BufferPool<'static>,
    chain: UndoChain<'static, 'static>,
    blocks: Vec<u32>,
    snapshot: CommitSeq,
}

#[allow(clippy::too_many_lines)]
fn fixture(io: &'static MemFileIo, rows: &[Row]) -> Fixture {
    let undo_file = Box::leak(Box::new(
        DataFile::create(io, Path::new(UNDO_F), 1, 1, WS, 512).unwrap(),
    ));
    let undo_handle = undo_file.handle();
    let undo_segment = create_undo_segment(undo_file, 2, 3, 4).unwrap();

    let data_file = Box::leak(Box::new(
        DataFile::create(io, Path::new(DATA_F), DATA_FID, 3, WS, 512).unwrap(),
    ));
    let data_handle = data_file.handle();
    let mut segment = Segment::create(data_file, SegType::Heap, 1, 1, 8, 0, 0).unwrap();

    // 三张已格式化的堆表页（经段分配 → 逻辑页 → 物理块）。
    let mut blocks = Vec::new();
    for _ in 0..3 {
        let logical = segment.allocate_append_page().unwrap();
        let block = segment.logical_block(logical).unwrap();
        let mut page = Page::new(PageType::HeapTable, WS, DATA_FID, block);
        segment.write_page(logical, &mut page).unwrap();
        blocks.push(block);
    }

    let pool: &'static BufferPool<'static> = Box::leak(Box::new(
        BufferPool::new(
            io,
            16,
            move |_ws, r| match r.file_id() {
                1 => Some((undo_handle, r.block_id())),
                DATA_FID => Some((data_handle, r.block_id())),
                _ => None,
            },
            FakeWal,
        )
        .unwrap(),
    ));
    let mut chain = UndoChain::open(undo_segment).with_pool(pool);

    let mut cf = ControlFile::format(
        io,
        Path::new(A),
        Path::new(B),
        &WorkspaceEntry {
            workspace_id: WorkspaceId::from_raw(1).unwrap(),
            created_at: 0,
            derived_from: None,
            derived_at_seq: seq(0),
        },
        &RedoEntries::new(2, 1).unwrap(),
        &ArchiveRecord::default(),
    )
    .unwrap();
    let mut log = GroupWriter::create(
        io,
        &mut cf,
        Path::new(WAL),
        GroupSpec::new(2, 1, 64).unwrap(),
        lsn(0),
    )
    .unwrap();

    // 一个事务插完全部行（页 1 → 页 2 → 页 3 顺次填）。
    if !rows.is_empty() {
        let mut txn = begin(pool, &mut log, &mut chain, seq(1)).unwrap();
        for (i, r) in rows.iter().enumerate() {
            let block = blocks[(i / 8).min(blocks.len() - 1)];
            let bytes = encode_row(r, &shape()).unwrap();
            insert_row(
                pool,
                &mut log,
                &mut chain,
                &mut txn,
                BufferKey::new(WS, rdba(block)),
                &bytes,
                &InsertPolicy::in_place(0),
            )
            .unwrap();
        }
        commit(pool, &mut log, &mut chain, &mut txn, seq(1)).unwrap();
    }

    let blocks = segment.data_blocks(segment.hwm());
    Fixture {
        pool,
        chain,
        blocks,
        snapshot: seq(1),
    }
}

/// 两路执行同一查询：返回（算子路径结果, 直译路径结果, 算子路径的扫描行数）。
fn run_both(fx: &Fixture, query: &SelectQuery) -> (Vec<Row>, Vec<Row>, u64) {
    // 直译路径。
    let mut cursor = HeapScanner::new(fx.pool, &fx.chain, fx.snapshot, DATA_FID, fx.blocks.clone());
    let mut cx = ExecContext::new(fx.snapshot);
    let direct = execute_direct(query, &mut cursor, &mut cx).unwrap();

    // 算子路径（同一语义，先转计划再建树）。
    let plan = query.to_plan();
    let mut cursor2 = Some(HeapScanner::new(
        fx.pool,
        &fx.chain,
        fx.snapshot,
        DATA_FID,
        fx.blocks.clone(),
    ));
    let mut open = |_src| {
        Ok(Box::new(cursor2.take().expect("单次扫描：游标恰好开一次")) as Box<dyn RowCursor>)
    };
    let mut op = build(&plan, &mut open).unwrap();
    let mut cx2 = ExecContext::new(fx.snapshot);
    let rows = collect(op.as_mut(), &mut cx2).unwrap();
    let scanned = cx2.rows_out_of("SeqScan");
    (rows, direct, scanned)
}

fn col_expr(i: usize) -> Expr {
    Expr::Column(i)
}

fn literal(v: Value) -> Expr {
    Expr::Literal(v)
}

fn full_query() -> SelectQuery {
    SelectQuery {
        source: 0,
        shape: shape(),
        predicate: None,
        projection: vec![col_expr(0), col_expr(1)],
        limit: None,
        offset: 0,
    }
}

#[test]
fn operators_and_direct_interpreter_agree_row_by_row() {
    let io: &'static MemFileIo = Box::leak(Box::new(MemFileIo::new()));
    io.add_dir("/mem");
    let rows: Vec<Row> = (1..=20).map(|i| row(i, &format!("t{i:02}"))).collect();
    let fx = fixture(io, &rows);
    assert_eq!(fx.blocks.len(), 3, "三张表页");

    // ① 全表（无谓词、全投影）。
    let (plan_rows, direct_rows, scanned) = run_both(&fx, &full_query());
    assert_eq!(plan_rows, rows, "算子路径 = 插入序");
    assert_eq!(direct_rows, rows, "直译路径 = 插入序");
    assert_eq!(scanned, 20, "全表扫描行数");

    // ② 谓词 + 投影 + LIMIT/OFFSET。
    let q = SelectQuery {
        predicate: Some(Expr::Compare {
            op: CmpOp::Gt,
            left: Box::new(col_expr(0)),
            right: Box::new(literal(Value::Number(Number::parse("15").unwrap()))),
        }),
        projection: vec![col_expr(1)],
        limit: Some(2),
        offset: 1,
        ..full_query()
    };
    let (plan_rows, direct_rows, scanned) = run_both(&fx, &q);
    assert_eq!(plan_rows.len(), 2);
    assert_eq!(plan_rows, direct_rows, "差分一致");
    assert_eq!(
        plan_rows[0],
        Row::new(vec![Value::Bytes(b"t17".to_vec())]),
        "id>15 的第 2 条（offset=1）"
    );
    // 短路可观测：谓词放行 16..20，Limit 跳 1 行、产 2 行后不再调子 ⇒
    // SeqScan 恰好产出到 id=18（18 行），**不耗尽全表**（20 行）。
    assert_eq!(scanned, 18, "LIMIT 在谓词之后收口（实测扫描 {scanned} 行）");

    // ③ 谓词跳过 + NULL 三值逻辑：`tag <> 't05'`（t05 命中即不通过）。
    let q = SelectQuery {
        predicate: Some(Expr::Compare {
            op: CmpOp::Ne,
            left: Box::new(literal(Value::Bytes(b"t05".to_vec()))),
            right: Box::new(col_expr(1)),
        }),
        projection: vec![col_expr(0)],
        ..full_query()
    };
    let (plan_rows, direct_rows, _) = run_both(&fx, &q);
    assert_eq!(plan_rows, direct_rows);
    assert_eq!(plan_rows.len(), 19, "排除 t05");

    // ④ IS NULL（切片 1 无 NULL 行 ⇒ 零行；两侧都零行）。
    let q = SelectQuery {
        predicate: Some(Expr::IsNull {
            expr: Box::new(col_expr(0)),
            negated: false,
        }),
        ..full_query()
    };
    let (plan_rows, direct_rows, _) = run_both(&fx, &q);
    assert!(plan_rows.is_empty() && direct_rows.is_empty());

    // ⑤ 空结果：谓词不可能满足。
    let q = SelectQuery {
        predicate: Some(Expr::Compare {
            op: CmpOp::Lt,
            left: Box::new(col_expr(0)),
            right: Box::new(literal(Value::Number(Number::parse("0").unwrap()))),
        }),
        ..full_query()
    };
    let (plan_rows, direct_rows, _) = run_both(&fx, &q);
    assert!(plan_rows.is_empty() && direct_rows.is_empty());
}

#[test]
fn limit_short_circuits_the_scan() {
    let io: &'static MemFileIo = Box::leak(Box::new(MemFileIo::new()));
    io.add_dir("/mem");
    let rows: Vec<Row> = (1..=20).map(|i| row(i, "x")).collect();
    let fx = fixture(io, &rows);

    let q = SelectQuery {
        limit: Some(3),
        ..full_query()
    };
    let (plan_rows, direct_rows, scanned) = run_both(&fx, &q);
    assert_eq!(plan_rows.len(), 3);
    assert_eq!(plan_rows, direct_rows);
    // **真短路**：Limit 产出第 3 行后不再调子 ⇒ 扫描恰 3 行（不是 20）。
    assert_eq!(scanned, 3, "LIMIT 短路：扫描行数 = LIMIT");
}

#[test]
fn empty_table_scans_to_nothing() {
    let io: &'static MemFileIo = Box::leak(Box::new(MemFileIo::new()));
    io.add_dir("/mem");
    let fx = fixture(io, &[]);
    let (plan_rows, direct_rows, scanned) = run_both(&fx, &full_query());
    assert!(plan_rows.is_empty() && direct_rows.is_empty());
    assert_eq!(scanned, 0, "空表：零行产出");
}

#[test]
fn cancel_and_deadline_are_honoured_on_both_paths() {
    let io: &'static MemFileIo = Box::leak(Box::new(MemFileIo::new()));
    io.add_dir("/mem");
    let rows: Vec<Row> = (1..=5).map(|i| row(i, "x")).collect();
    let fx = fixture(io, &rows);

    // 取消：两路都在首个检查点返回 Cancelled。
    let cancel = AtomicBool::new(true);
    let mut cursor = HeapScanner::new(fx.pool, &fx.chain, fx.snapshot, DATA_FID, fx.blocks.clone());
    let mut cx = ExecContext::new(fx.snapshot).with_cancel(&cancel);
    let err = execute_direct(&full_query(), &mut cursor, &mut cx).unwrap_err();
    assert!(matches!(err, ExecError::Cancelled), "{err}");

    let plan = full_query().to_plan();
    let mut cursor2 = Some(HeapScanner::new(
        fx.pool,
        &fx.chain,
        fx.snapshot,
        DATA_FID,
        fx.blocks.clone(),
    ));
    let mut open = |_src| {
        Ok(Box::new(cursor2.take().expect("单次扫描：游标恰好开一次")) as Box<dyn RowCursor>)
    };
    let mut op = build(&plan, &mut open).unwrap();
    let mut cx2 = ExecContext::new(fx.snapshot).with_cancel(&cancel);
    let err = collect(op.as_mut(), &mut cx2).unwrap_err();
    assert!(matches!(err, ExecError::Cancelled), "{err}");

    // 截止时间：已过 ⇒ Deadline（与取消同路径、判定分开）。
    let deadline = std::time::Instant::now() - std::time::Duration::from_millis(1);
    let mut cursor3 =
        HeapScanner::new(fx.pool, &fx.chain, fx.snapshot, DATA_FID, fx.blocks.clone());
    let mut cx3 = ExecContext::new(fx.snapshot).with_deadline(deadline);
    let err = execute_direct(&full_query(), &mut cursor3, &mut cx3).unwrap_err();
    assert!(matches!(err, ExecError::Deadline), "{err}");
}
