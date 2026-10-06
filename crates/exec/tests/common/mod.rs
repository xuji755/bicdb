#![allow(dead_code)] // 公共夹具：各测试 crate 各取所需，未用项不算缺陷

//! 执行器集成用例的公共夹具：**真段 + 真缓冲池 + 真撤销链**的小表，
//! 以及"算子树 ↔ 直译执行器"的两路跑法（设计 §6 的差分形态）。

use std::path::Path;

use bicdb_common::seq::{CommitSeq, Lsn};
use bicdb_exec::{
    build, collect, encode_row, execute_direct, ColKind, ExecContext, ExecError, OpStat, Row,
    RowCursor, RowShape, SelectQuery, Value, WorkAreaStats,
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

/// 数据文件号。
pub const DATA_FID: u16 = 3;
/// 工作区标识字节。
pub const WS: [u8; 8] = [8u8; 8];

const UNDO_F: &str = "/mem/exec_undo.dat";
const DATA_F: &str = "/mem/exec_data.dat";
const A: &str = "/mem/exec_c1.ctl";
const B: &str = "/mem/exec_c2.ctl";
const WAL: &str = "/mem/exec_wal";

/// 新建内存 I/O（表名空间独立——每个用例一套）。
pub fn mem_io() -> &'static MemFileIo {
    let io: &'static MemFileIo = Box::leak(Box::new(MemFileIo::new()));
    io.add_dir("/mem");
    io
}

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
pub struct FakeWal;
impl WalGuard for FakeWal {
    fn durable_lsn(&self) -> Lsn {
        lsn(u64::MAX >> 16)
    }
    fn ensure_durable(&self, _t: Lsn) -> std::io::Result<()> {
        Ok(())
    }
}

/// 表形状：`(id NUMBER, tag BYTES)` —— 全变长布局（切片 1 约定）。
pub fn shape() -> RowShape {
    RowShape::new(vec![ColKind::Number, ColKind::Bytes])
}

/// 数值字面量。
pub fn num(text: &str) -> Value {
    Value::Number(Number::parse(text).unwrap())
}

/// 一行 `(id, tag)`。
pub fn row(id: i64, tag: &str) -> Row {
    Row::new(vec![
        num(&id.to_string()),
        Value::Bytes(tag.as_bytes().to_vec()),
    ])
}

/// 一行 `(id, NULL)`（NULL 排序语义用例）。
pub fn null_tag_row(id: i64) -> Row {
    Row::new(vec![num(&id.to_string()), Value::Null])
}

/// 真表夹具（池/链都是 `'static` 件——测试里 `Box::leak`）。
pub struct Fixture {
    /// 缓冲池。
    pub pool: &'static BufferPool<'static>,
    /// 撤销链（CR 读路径）。
    pub chain: UndoChain<'static, 'static>,
    /// 表的数据块（升序）。
    pub blocks: Vec<u32>,
    /// 查询快照（= 插入提交的序号）。
    pub snapshot: CommitSeq,
}

/// 建表并插入 `rows`（每页 8 行，页数按需 ×3 起步）。
#[allow(clippy::too_many_lines)]
pub fn fixture(io: &'static MemFileIo, rows: &[Row]) -> Fixture {
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

    // 已格式化的堆表页（页数按行数准备；经段分配 → 逻辑页 → 物理块）。
    let pages = rows.len().div_ceil(8).max(3) as u32;
    let mut blocks = Vec::new();
    for _ in 0..pages {
        let logical = segment.allocate_append_page().unwrap();
        let block = segment.logical_block(logical).unwrap();
        let mut page = Page::new(PageType::HeapTable, WS, DATA_FID, block);
        segment.write_page(logical, &mut page).unwrap();
        blocks.push(block);
    }

    let pool: &'static BufferPool<'static> = Box::leak(Box::new(
        BufferPool::new(
            io,
            32,
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

/// 两路执行的结果（差分 + 诊断）。
pub struct RunBoth {
    /// 算子路径结果。
    pub plan_rows: Vec<Row>,
    /// 直译路径结果（参考模型）。
    pub direct_rows: Vec<Row>,
    /// 算子路径 `SeqScan` 的产出行数。
    pub scanned: u64,
    /// 算子路径的每算子统计。
    pub op_stats: Vec<OpStat>,
    /// 算子路径的 WMM 三态计数。
    pub work_areas: WorkAreaStats,
}

/// 算子树路径（可带工作内存预算）；错误保真外传。
pub fn run_plan(
    fx: &Fixture,
    query: &SelectQuery,
    budget: Option<u64>,
) -> Result<Vec<Row>, (ExecError, WorkAreaStats)> {
    let plan = query.to_plan();
    let mut cursor = Some(HeapScanner::new(
        fx.pool,
        &fx.chain,
        fx.snapshot,
        DATA_FID,
        fx.blocks.clone(),
    ));
    let mut open =
        |_src| Ok(Box::new(cursor.take().expect("单次扫描：游标恰好开一次")) as Box<dyn RowCursor>);
    let mut op = match build(&plan, &mut open) {
        Ok(op) => op,
        Err(e) => return Err((e, WorkAreaStats::default())),
    };
    let mut cx = ExecContext::new(fx.snapshot).with_work_memory_budget(budget);
    match collect(op.as_mut(), &mut cx) {
        Ok(rows) => Ok(rows),
        Err(e) => Err((e, cx.work_area_stats())),
    }
}

/// 两路执行同一查询（无预算）。
pub fn run_both(fx: &Fixture, query: &SelectQuery) -> RunBoth {
    // 直译路径。
    let mut cursor = HeapScanner::new(fx.pool, &fx.chain, fx.snapshot, DATA_FID, fx.blocks.clone());
    let mut cx = ExecContext::new(fx.snapshot);
    let direct_rows = execute_direct(query, &mut cursor, &mut cx).unwrap();

    // 算子路径（同一语义，先转计划再建树）——需要扫描行数/三态统计，重跑一次。
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
    let plan_rows = collect(op.as_mut(), &mut cx2).unwrap();
    RunBoth {
        plan_rows,
        direct_rows,
        scanned: cx2.rows_out_of("SeqScan"),
        op_stats: cx2.stats().to_vec(),
        work_areas: cx2.work_area_stats(),
    }
}
