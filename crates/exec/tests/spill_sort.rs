//! **切片 6b 验收**：`Sort` 的外部归并（temp 段溢出）。
//!
//! 钉住：**低内存预算（溢出路径）与大预算（全内存）结果逐行一致**；
//! 溢出确实发生（run 数 > 1、`one_pass` 计数、temp 文件有页）；
//! 无溢出空间时超预算仍报具名错误（切片 2c 的形态保留为防线）。

mod common;

use bicdb_exec::{
    build, collect, ExecContext, ExecEnv, ExecError, Expr, PlanNode, Row, RowCursor, SortKey,
    SpillSpace, WorkAreaStats,
};
use bicdb_storage::datafile::DataFile;
use bicdb_storage::scan::HeapScanner;
use bicdb_storage::temp::TempKind;
use std::path::Path;

use common::{fixture, mem_io, num, row, shape, Fixture, DATA_FID};

fn col(i: usize) -> Expr {
    Expr::Column(i)
}

/// 200 行（id 逆序插入——排序必须真的改变顺序）。
fn rows_desc() -> Vec<Row> {
    (1..=200)
        .rev()
        .map(|i| row(i, &format!("t{i:03}")))
        .collect()
}

fn sort_plan(desc: bool) -> PlanNode {
    PlanNode::Sort {
        input: Box::new(PlanNode::SeqScan {
            source: 0,
            shape: shape(),
        }),
        keys: vec![SortKey { expr: col(0), desc }],
    }
}

/// 跑一次排序；`budget` 与 `spill` 决定是否走溢出路径。
fn run_sort(
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

#[test]
fn external_merge_matches_in_memory_sort_row_by_row() {
    let io = mem_io();
    let fx = fixture(io, &rows_desc());
    let plan = sort_plan(false);

    // ① 大预算、无溢出 ⇒ 全内存排序（optimal）。
    let (in_memory, stats) = run_sort(&fx, &plan, Some(1 << 30), None).unwrap();
    assert_eq!(
        stats,
        WorkAreaStats {
            optimal: 1,
            one_pass: 0,
            multi_pass: 0
        }
    );
    assert_eq!(in_memory.len(), 200);
    assert_eq!(in_memory[0].values[0], num("1"), "升序首行");
    assert_eq!(in_memory[199].values[0], num("200"));

    // ② 小预算 + 溢出空间 ⇒ 分批落 run + k 路归并。
    let temp = Box::leak(Box::new(
        DataFile::open_temp_reset(io, Path::new("/mem/spill_sort.dat"), [8u8; 8], 512)
            .expect("临时文件"),
    ));
    let space = Box::leak(Box::new(
        SpillSpace::create(temp, TempKind::Sort, [8u8; 8]).expect("溢出空间"),
    ));
    let (spilled, stats2) = run_sort(&fx, &plan, Some(1024), Some(space)).unwrap();
    assert_eq!(spilled, in_memory, "**低预算（溢出）与大预算结果逐行一致**");
    assert!(stats2.one_pass >= 1, "记 one-pass（实际 {:?}）", stats2);
    assert!(space.runs() > 1, "分批成多个 run（实际 {}）", space.runs());
    assert!(space.run_pages(0) > 0, "temp 段确实写了页");

    // ③ 降序同样一致（方向 + NULL 位置规则在归并路径同样成立）。
    let plan_desc = sort_plan(true);
    let (in_mem_desc, _) = run_sort(&fx, &plan_desc, Some(1 << 30), None).unwrap();
    let (spilled_desc, _) = run_sort(&fx, &plan_desc, Some(1024), Some(space)).unwrap();
    assert_eq!(spilled_desc, in_mem_desc, "降序：溢出与全内存一致");
    assert_eq!(spilled_desc[0].values[0], num("200"));

    // ④ 防线：无溢出空间时超预算仍报具名错误。
    let err = run_sort(&fx, &plan, Some(64), None).unwrap_err();
    assert!(
        matches!(err, ExecError::WorkMemoryExceeded { .. }),
        "无溢出空间 ⇒ 具名错误：{err}"
    );
}
