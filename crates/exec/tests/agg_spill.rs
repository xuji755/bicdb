//! **切片 6b-2c 验收**：`HashAgg` 的分区溢出（改档重来 + 换种子重分区）。
//!
//! 钉住：**低额度（溢出）与大额度（全内存）结果一致**（无 `ORDER BY` ⇒
//! 按多重集比较）；`one-pass` / `multi-pass` 三态计数；extra bytes 记账；
//! **temp 不进 redo**（真日志写入器在场：溢出全程 LSN 不动）。

mod common;

use std::path::Path;

use bicdb_common::seq::{CommitSeq, Lsn};
use bicdb_exec::{
    build, collect, AggKind, AggSpec, ExecContext, ExecEnv, ExecError, Expr, PlanNode, Row,
    RowCursor,
};
use bicdb_storage::controlfile::{ArchiveRecord, ControlFile, RedoEntries, WorkspaceEntry};
use bicdb_storage::datafile::DataFile;
use bicdb_storage::scan::HeapScanner;
use bicdb_storage::temp::TempKind;
use bicdb_wal::group::{GroupSpec, GroupWriter};
use bicdb_workspace::id::WorkspaceId;

use common::{fixture, mem_io, row, shape, Fixture, DATA_FID, WS};

fn col(i: usize) -> Expr {
    Expr::Column(i)
}

/// 聚合计划：`GROUP BY tag`（列 1）+ `COUNT(*)`、`COUNT(DISTINCT id)`。
fn agg_plan() -> PlanNode {
    PlanNode::HashAgg {
        input: Box::new(PlanNode::SeqScan {
            source: 0,
            shape: shape(),
        }),
        groups: vec![col(1)],
        aggs: vec![
            AggSpec {
                kind: AggKind::CountStar,
                arg: None,
                distinct: false,
            },
            AggSpec {
                kind: AggKind::Count,
                arg: Some(col(0)),
                distinct: true,
            },
        ],
    }
}

/// 跑一次聚合；`budget` 与 `spill` 决定是否走溢出路径。
fn run_agg(
    fx: &Fixture,
    plan: &PlanNode,
    budget: Option<u64>,
    spill: Option<&bicdb_exec::SpillSpace<'_>>,
) -> Result<(Vec<Row>, bicdb_exec::WorkAreaStats), ExecError> {
    let mut open = |_src: u32| {
        Ok(Box::new(HeapScanner::new(
            fx.pool,
            &fx.chain,
            fx.snapshot,
            DATA_FID,
            fx.blocks.clone(),
        )) as Box<dyn RowCursor>)
    };
    let env = ExecEnv {
        pool: fx.pool,
        chain: Some(&fx.chain),
        spill,
        writer: None,
    };
    let mut op = build(plan, &env, &mut open)?;
    let mut cx = ExecContext::new(fx.snapshot).with_work_memory_budget(budget);
    let rows = collect(op.as_mut(), &mut cx)?;
    Ok((rows, cx.work_area_stats()))
}

/// 多重集比较（行序无保证——无 `ORDER BY` 的 SQL 本无顺序）。
fn sorted(mut rows: Vec<Row>) -> Vec<String> {
    let mut out: Vec<String> = rows.drain(..).map(|r| format!("{:?}", r.values)).collect();
    out.sort();
    out
}

/// 造临时数据文件 + 溢出空间（每个用例一份，名字不同避免撞车）。
fn spill_space(
    io: &'static bicdb_workspace::io::MemFileIo,
    path: &str,
) -> &'static bicdb_exec::SpillSpace<'static> {
    let file = Box::leak(Box::new(
        DataFile::open_temp_reset(io, Path::new(path), WS, 4096).expect("临时文件"),
    ));
    Box::leak(Box::new(
        bicdb_exec::SpillSpace::create(file, TempKind::Hash, WS).expect("溢出空间"),
    ))
}

/// 60 组（`tag` 0..59）× 每组 3~4 行；`DISTINCT id` 与 `COUNT(*)` 都有区分度。
fn rows_many_groups() -> Vec<Row> {
    (0..200i64)
        .map(|i| row(i, &format!("g{:02}", i % 60)))
        .collect()
}

#[test]
fn hash_agg_spill_matches_in_memory_as_multiset() {
    let io = mem_io();
    let fx = fixture(io, &rows_many_groups());
    let plan = agg_plan();

    // ① 大额度、无溢出空间 ⇒ 全内存（optimal）。
    let (in_memory, stats) = run_agg(&fx, &plan, Some(1 << 30), None).unwrap();
    assert_eq!(stats.optimal, 1);
    assert_eq!(in_memory.len(), 60, "60 组");
    assert_eq!(stats.extra_bytes_written, 0);

    // ② 额度 < 全表（约 60×130 B）但 ≥ 单分区表（约 4 组×130 B）⇒ 只做一级
    //    分区：one-pass。
    let space = spill_space(io, "/mem/agg_spill.dat");
    let (spilled, stats2) = run_agg(&fx, &plan, Some(1024), Some(space)).unwrap();
    assert_eq!(
        sorted(spilled.clone()),
        sorted(in_memory.clone()),
        "**低额度（分区溢出）与大额度结果一致**（多重集）"
    );
    assert_eq!(stats2.one_pass, 1, "一级分区 = one-pass（实际 {stats2:?}）");
    assert_eq!(stats2.multi_pass, 0, "分区装得下 ⇒ 无需二次分区");
    assert!(stats2.extra_bytes_written > 0, "temp 写过");
    assert!(stats2.extra_bytes_read > 0, "temp 读过");
    assert!(space.runs() > 1, "分批成多个 run（{}）", space.runs());

    // ③ 额度压到单分区也装不下 ⇒ 换种子重分区：**multi-pass** + 结果仍一致。
    let (tiny, stats3) = run_agg(&fx, &plan, Some(256), Some(space)).unwrap();
    assert_eq!(sorted(tiny), sorted(in_memory), "极小额度也必须一致");
    assert!(
        stats3.multi_pass >= 1,
        "分区装不下 ⇒ 二次重分区（multi-pass；实际 {stats3:?}）"
    );
}

#[test]
fn hash_agg_counts_multi_pass_when_partitions_still_overflow() {
    let io = mem_io();
    // 每组一行（唯一键）⇒ 组数 = 行数；小额度必然逼出二次分区。
    let rows: Vec<Row> = (0..200).map(|i| row(i, &format!("k{i:03}"))).collect();
    let fx = fixture(io, &rows);
    let plan = agg_plan();
    let space = spill_space(io, "/mem/agg_mp.dat");

    let (in_memory, _) = run_agg(&fx, &plan, Some(1 << 30), None).unwrap();
    let (spilled, stats) = run_agg(&fx, &plan, Some(256), Some(space)).unwrap();
    assert_eq!(sorted(spilled), sorted(in_memory), "结果一致");
    assert_eq!(
        stats.multi_pass, 1,
        "分区仍装不下 ⇒ 二次重分区（multi-pass；实际 {stats:?}）"
    );
    assert_eq!(
        stats.one_pass, 0,
        "多趟执行不计 one-pass（口径：以最差为准）"
    );
}

#[test]
fn spill_writes_no_redo_records() {
    // temp 页 **no-redo**（设计 §4.2 落点）：真日志写入器在场，溢出全程 LSN 不动。
    let io = mem_io();
    let fx = fixture(io, &rows_many_groups());
    let plan = agg_plan();

    let cf: &'static mut ControlFile<'static> = Box::leak(Box::new(
        ControlFile::format(
            io,
            Path::new("/mem/agg_c1.ctl"),
            Path::new("/mem/agg_c2.ctl"),
            &WorkspaceEntry {
                workspace_id: WorkspaceId::from_raw(1).unwrap(),
                created_at: 0,
                derived_from: None,
                derived_at_seq: CommitSeq::from_raw(0).unwrap(),
            },
            &RedoEntries::new(2, 1).unwrap(),
            &ArchiveRecord::default(),
        )
        .unwrap(),
    ));
    let log = GroupWriter::create(
        io,
        cf,
        Path::new("/mem/agg_wal"),
        GroupSpec::new(2, 1, 64).unwrap(),
        Lsn::from_raw(0).unwrap(),
    )
    .unwrap();
    let before = log.appended_lsn();

    let space = spill_space(io, "/mem/agg_noredo.dat");
    let (rows_out, stats) = run_agg(&fx, &plan, Some(256), Some(space)).unwrap();
    assert_eq!(rows_out.len(), 60);
    assert!(stats.extra_bytes_written > 0, "确实发生了溢出");
    assert_eq!(log.appended_lsn(), before, "溢出不写日志（temp 不进 redo）");
}
