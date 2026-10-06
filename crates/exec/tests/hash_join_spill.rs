//! **切片 6b-2d 验收**：`HashJoin` 的构建侧溢出（双侧配对分区）。
//!
//! 钉住：**低额度（分区）与大额度（全内存）结果一致**（多重集）；INNER 与
//! LEFT 语义在分区路径不变（**LEFT 补 NULL 跨分区仍正确**）；`one-pass` /
//! `multi-pass` 计数；额外字节记账。

mod common;

use std::path::Path;

use bicdb_exec::{
    build, collect, CmpOp, ExecContext, ExecEnv, ExecError, Expr, JoinKind, PlanNode, Row,
    RowCursor, SpillSpace, Value, WorkAreaStats,
};
use bicdb_storage::datafile::DataFile;
use bicdb_storage::scan::HeapScanner;
use bicdb_storage::temp::TempKind;

use common::{fixture, mem_io, num, row, shape, Fixture, DATA_FID, WS};

fn col(i: usize) -> Expr {
    Expr::Column(i)
}

fn lit(v: Value) -> Expr {
    Expr::Literal(v)
}

fn bound(i: usize, op: CmpOp, v: Value) -> Expr {
    Expr::Compare {
        op,
        left: Box::new(col(i)),
        right: Box::new(lit(v)),
    }
}

fn scan() -> PlanNode {
    PlanNode::SeqScan {
        source: 0,
        shape: shape(),
    }
}

fn side(pred: Expr) -> PlanNode {
    PlanNode::Filter {
        input: Box::new(scan()),
        predicate: pred,
    }
}

/// 200 行、**id 唯一**（自连接 = 每行配自己；分区正确性一破就少行）。
fn rows_unique() -> Vec<Row> {
    (0..200i64).map(|i| row(i, &format!("t{i:03}"))).collect()
}

fn join_plan(kind: JoinKind, build_pred: Expr) -> PlanNode {
    PlanNode::HashJoin {
        build: Box::new(side(build_pred)),
        probe: Box::new(scan()),
        build_keys: vec![col(0)],
        probe_keys: vec![col(0)],
        kind,
        qual: None,
        build_width: 2,
    }
}

fn run_join(
    fx: &Fixture,
    plan: &PlanNode,
    budget: Option<u64>,
    spill: Option<&SpillSpace<'_>>,
) -> Result<(Vec<Row>, WorkAreaStats), ExecError> {
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

fn sorted(mut rows: Vec<Row>) -> Vec<String> {
    let mut out: Vec<String> = rows.drain(..).map(|r| format!("{:?}", r.values)).collect();
    out.sort();
    out
}

fn spill_space(io: &'static bicdb_workspace::io::MemFileIo) -> &'static SpillSpace<'static> {
    let file = Box::leak(Box::new(
        DataFile::open_temp_reset(io, Path::new("/mem/hj_spill.dat"), WS, 4096).expect("临时文件"),
    ));
    Box::leak(Box::new(
        SpillSpace::create(file, TempKind::Hash, WS).expect("溢出空间"),
    ))
}

#[test]
fn hash_join_spill_matches_in_memory_as_multiset() {
    let io = mem_io();
    let fx = fixture(io, &rows_unique());
    let plan = join_plan(
        JoinKind::Inner,
        Expr::Compare {
            op: CmpOp::Le,
            left: Box::new(col(0)),
            right: Box::new(lit(Value::Number(
                bicdb_types::Number::parse("199").unwrap(),
            ))),
        },
    );

    // ① 大额度、无溢出 ⇒ 全内存：200 行各配自己。
    let (in_memory, stats0) = run_join(&fx, &plan, Some(1 << 30), None).unwrap();
    assert_eq!(in_memory.len(), 200, "自连接每行配自己");
    assert_eq!(stats0.optimal, 1);

    // ② 额度 < 全表但 ≥ 单分区 ⇒ 一级分区（one-pass），结果一致。
    let space = spill_space(io);
    let (spilled, stats1) = run_join(&fx, &plan, Some(1024), Some(space)).unwrap();
    assert_eq!(
        sorted(spilled.clone()),
        sorted(in_memory.clone()),
        "**低额度（双侧分区）与大额度结果一致**（多重集）"
    );
    assert_eq!(stats1.one_pass, 1, "一级分区 = one-pass（实际 {stats1:?}）");
    assert_eq!(stats1.multi_pass, 0);
    assert!(stats1.extra_bytes_written > 0 && stats1.extra_bytes_read > 0);

    // ③ 额度压到单分区也装不下 ⇒ 配对重分区（multi-pass），结果仍一致。
    let (tiny, stats2) = run_join(&fx, &plan, Some(256), Some(space)).unwrap();
    assert_eq!(sorted(tiny), sorted(in_memory), "极小额度也必须一致");
    assert!(
        stats2.multi_pass >= 1,
        "分区装不下 ⇒ 配对重分区（multi-pass；实际 {stats2:?}）"
    );
}

#[test]
fn left_join_pads_nulls_across_partitions() {
    let io = mem_io();
    let fx = fixture(io, &rows_unique());
    // 构建侧只有 id ≤ 99（100 行）；探测侧 200 行 ⇒ 100 匹配 + 100 补 NULL。
    let plan = join_plan(JoinKind::Left, bound(0, CmpOp::Le, num("99")));

    let (in_memory, _) = run_join(&fx, &plan, Some(1 << 30), None).unwrap();
    assert_eq!(in_memory.len(), 200, "100 匹配 + 100 补 NULL");

    let space = spill_space(io);
    let (spilled, stats) = run_join(&fx, &plan, Some(1024), Some(space)).unwrap();
    assert_eq!(
        sorted(spilled.clone()),
        sorted(in_memory),
        "LEFT 语义跨分区不变（多重集一致）"
    );
    // 补 NULL 的 100 行：构建侧两列都为 NULL（**跨分区也必须补**）。
    let padded: Vec<&Row> = spilled
        .iter()
        .filter(|r| r.values[2].is_null() && r.values[3].is_null())
        .collect();
    assert_eq!(padded.len(), 100, "无匹配的探测行补 NULL");
    let mut padded_ids: Vec<String> = padded
        .iter()
        .map(|r| format!("{:?}", r.values[0]))
        .collect();
    padded_ids.sort();
    padded_ids.dedup();
    let matched_ids: Vec<String> = spilled
        .iter()
        .filter(|r| !r.values[2].is_null())
        .map(|r| format!("{:?}", r.values[0]))
        .collect();
    assert_eq!(
        padded_ids.len(),
        100,
        "补 NULL 的是 100 个**互不相同**的探测行 id"
    );
    assert!(
        !padded_ids.iter().any(|id| matched_ids.contains(id)),
        "匹配与补 NULL 不相交"
    );
    assert_eq!(
        matched_ids.len(),
        100,
        "100 个探测行有匹配（构建侧 id ≤ 99）"
    );
    assert!(stats.one_pass + stats.multi_pass >= 1, "走了溢出路径");
}
