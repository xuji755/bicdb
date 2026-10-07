//! **切片 4 验收**：`ScalarAgg` / `HashAgg` / `SortedAgg`（含 `DISTINCT`）
//! 与 `GROUP BY` / `HAVING`（设计 §2.4/§4.5）。
//!
//! 钉住：**空输入语义**（无分组 ⇒ 恒出一行；有分组 ⇒ 零行）；`NULL` 不参与
//! 聚合、但**在分组里自成一组**；`DISTINCT` 修饰；`HAVING` 在聚合输出行上
//! 过滤；`HashAgg` 与 `SortedAgg` 结果一致；两路差分。

mod common;

use bicdb_exec::{
    build, collect, AggKind, AggSpec, CmpOp, ExecContext, ExecEnv, Expr, PlanNode, Row, RowCursor,
    SelectQuery, SortKey, Value,
};

use common::{fixture, mem_io, null_tag_row, num, row, run_both, shape, DATA_FID};

fn col(i: usize) -> Expr {
    Expr::Column(i)
}

fn count_star() -> AggSpec {
    AggSpec {
        kind: AggKind::CountStar,
        arg: None,
        distinct: false,
    }
}

fn count_col(i: usize) -> AggSpec {
    AggSpec {
        kind: AggKind::Count,
        arg: Some(col(i)),
        distinct: false,
    }
}

fn count_distinct(i: usize) -> AggSpec {
    AggSpec {
        kind: AggKind::Count,
        arg: Some(col(i)),
        distinct: true,
    }
}

fn agg(kind: AggKind, i: usize) -> AggSpec {
    AggSpec {
        kind,
        arg: Some(col(i)),
        distinct: false,
    }
}

fn base_query() -> SelectQuery {
    SelectQuery {
        source: 0,
        shape: shape(),
        predicate: None,
        groups: Vec::new(),
        aggs: Vec::new(),
        having: None,
        projection: vec![col(0), col(1)],
        order_by: Vec::new(),
        limit: None,
        offset: 0,
    }
}

/// `(id, tag)`：a 三行、b 两行、NULL 两行。
fn agg_rows() -> Vec<Row> {
    vec![
        row(1, "a"),
        row(2, "a"),
        row(3, "b"),
        null_tag_row(4),
        row(5, "a"),
        row(6, "b"),
        null_tag_row(7),
    ]
}

#[test]
fn scalar_aggregates_and_empty_input_semantics() {
    let io = mem_io();
    let rows = agg_rows();
    let fx = fixture(io, &rows);

    let q = SelectQuery {
        aggs: vec![
            count_star(),
            count_col(1),
            count_distinct(1),
            agg(AggKind::Sum, 0),
            agg(AggKind::Avg, 0),
            agg(AggKind::Min, 0),
            agg(AggKind::Max, 0),
        ],
        projection: (0..7).map(col).collect(),
        ..base_query()
    };
    let out = run_both(&fx, &q);
    assert_eq!(out.plan_rows, out.direct_rows, "两路差分一致");
    assert_eq!(out.plan_rows.len(), 1, "无 GROUP BY ⇒ 恒一行");
    let r = &out.plan_rows[0];
    assert_eq!(r.values[0], num("7"), "COUNT(*) 含 NULL 行");
    assert_eq!(r.values[1], num("5"), "COUNT(tag) 计非空");
    assert_eq!(r.values[2], num("2"), "COUNT(DISTINCT tag) = a/b");
    assert_eq!(r.values[3], num("28"), "SUM(id)");
    assert_eq!(r.values[4], num("4"), "AVG(id) = 28/7");
    assert_eq!(r.values[5], num("1"), "MIN(id)");
    assert_eq!(r.values[6], num("7"), "MAX(id)");

    // **空输入**：恒出一行（COUNT=0、SUM/AVG/MIN/MAX = NULL）。
    let io2 = mem_io();
    let fx2 = fixture(io2, &[]);
    let q2 = SelectQuery {
        aggs: vec![count_star(), agg(AggKind::Sum, 0), agg(AggKind::Avg, 0)],
        projection: vec![col(0), col(1), col(2)],
        ..base_query()
    };
    let out2 = run_both(&fx2, &q2);
    assert_eq!(out2.plan_rows, out2.direct_rows);
    assert_eq!(out2.plan_rows.len(), 1, "空表也出一行");
    assert_eq!(out2.plan_rows[0].values[0], num("0"), "COUNT(*) = 0");
    assert!(out2.plan_rows[0].values[1].is_null(), "SUM = NULL");
    assert!(out2.plan_rows[0].values[2].is_null(), "AVG = NULL");
}

#[test]
fn group_by_puts_nulls_in_their_own_group() {
    let io = mem_io();
    let rows = agg_rows();
    let fx = fixture(io, &rows);

    let q = SelectQuery {
        groups: vec![col(1)],
        aggs: vec![count_star(), agg(AggKind::Sum, 0)],
        projection: vec![col(0), col(1), col(2)], // tag, COUNT(*), SUM(id)
        order_by: vec![SortKey {
            expr: col(0),
            desc: false,
        }],
        ..base_query()
    };
    let out = run_both(&fx, &q);
    assert_eq!(out.plan_rows, out.direct_rows, "两路差分一致");
    assert_eq!(out.plan_rows.len(), 3, "a/b/NULL 三组");

    let by_tag = |tag: &Value| -> &Row {
        out.plan_rows
            .iter()
            .find(|r| &r.values[0] == tag)
            .expect("组存在")
    };
    let a = by_tag(&Value::Bytes(b"a".to_vec()));
    assert_eq!(a.values[1], num("3"), "a 组三行");
    assert_eq!(a.values[2], num("8"), "a 组 SUM = 1+2+5");
    let b = by_tag(&Value::Bytes(b"b".to_vec()));
    assert_eq!(b.values[1], num("2"), "b 组两行");
    let n = by_tag(&Value::Null);
    assert_eq!(n.values[1], num("2"), "NULL 自成一组（两行）");
    assert_eq!(n.values[2], num("11"), "NULL 组 SUM = 4+7");

    // 有 GROUP BY 的空输入 ⇒ **零行**（与无分组的"恒一行"对照）。
    let io2 = mem_io();
    let fx2 = fixture(io2, &[]);
    let out2 = run_both(&fx2, &q);
    assert!(out2.plan_rows.is_empty(), "有 GROUP BY ⇒ 空输入零行");
    assert_eq!(out2.plan_rows, out2.direct_rows);
}

#[test]
fn having_filters_aggregate_rows() {
    let io = mem_io();
    let rows = agg_rows();
    let fx = fixture(io, &rows);

    // HAVING COUNT(*) > 2 ⇒ 只剩 a 组。
    let q = SelectQuery {
        groups: vec![col(1)],
        aggs: vec![count_star()],
        having: Some(Expr::Compare {
            op: CmpOp::Gt,
            left: Box::new(col(1)), // 聚合输出行：col1 = COUNT(*)
            right: Box::new(Expr::Literal(num("2"))),
        }),
        projection: vec![col(0), col(1)],
        ..base_query()
    };
    let out = run_both(&fx, &q);
    assert_eq!(out.plan_rows, out.direct_rows);
    assert_eq!(out.plan_rows.len(), 1, "只剩 a 组");
    assert_eq!(out.plan_rows[0].values[0], Value::Bytes(b"a".to_vec()));
    assert_eq!(out.plan_rows[0].values[1], num("3"));
}

#[test]
fn sorted_agg_matches_hash_agg() {
    let io = mem_io();
    let rows = agg_rows();
    let fx = fixture(io, &rows);

    // HashAgg 路径（经 SelectQuery → to_plan）。
    let q = SelectQuery {
        groups: vec![col(1)],
        aggs: vec![count_star(), agg(AggKind::Sum, 0)],
        projection: vec![col(0), col(1), col(2)],
        order_by: vec![SortKey {
            expr: col(0),
            desc: false,
        }],
        ..base_query()
    };
    let hash_out = run_both(&fx, &q);

    // SortedAgg 路径：前置 Sort（按组键）——手工建树（计划形态的对照）。
    let plan = PlanNode::Project {
        input: Box::new(PlanNode::SortedAgg {
            input: Box::new(PlanNode::Sort {
                input: Box::new(PlanNode::SeqScan {
                    source: 0,
                    shape: shape(),
                }),
                keys: vec![SortKey {
                    expr: col(1),
                    desc: false,
                }],
            }),
            groups: vec![col(1)],
            aggs: vec![count_star(), agg(AggKind::Sum, 0)],
        }),
        exprs: vec![col(0), col(1), col(2)],
    };
    let out = run_plan_project(&fx, &plan);
    assert_eq!(out.len(), 3, "三组（NULL 组在升序末尾）");
    // 与 HashAgg 的结果**按组对齐后一致**（HashAgg 输出为键出现序，
    // 这里按组键排序后比较）。
    let mut sorted_hash = hash_out.plan_rows.clone();
    sorted_hash.sort_by(|a, b| format!("{:?}", a.values[0]).cmp(&format!("{:?}", b.values[0])));
    let mut sorted_sorted = out.clone();
    sorted_sorted.sort_by(|a, b| format!("{:?}", a.values[0]).cmp(&format!("{:?}", b.values[0])));
    assert_eq!(
        sorted_hash, sorted_sorted,
        "SortedAgg == HashAgg（按组对齐）"
    );
}

/// 跑一个只含 SeqScan 源的计划（Project 顶层）。
fn run_plan_project(fx: &common::Fixture, plan: &PlanNode) -> Vec<Row> {
    let mut open = |_src: u32| {
        Ok(Box::new(bicdb_storage::scan::HeapScanner::new(
            fx.pool,
            &fx.chain,
            bicdb_storage::cr::ReadView::new(fx.snapshot),
            DATA_FID,
            fx.blocks.clone(),
        )) as Box<dyn RowCursor>)
    };
    let env = ExecEnv {
        pool: fx.pool,
        chain: Some(&fx.chain),
        spill: None,
        writer: None,
    };
    let mut op = build(plan, &env, &mut open).unwrap();
    let mut cx = ExecContext::new(fx.snapshot);
    collect(op.as_mut(), &mut cx).unwrap()
}

#[test]
fn distinct_aggregates_count_each_value_once() {
    let io = mem_io();
    let rows = agg_rows();
    let fx = fixture(io, &rows);

    let q = SelectQuery {
        aggs: vec![count_distinct(1), agg(AggKind::Sum, 0)],
        projection: vec![col(0), col(1)],
        ..base_query()
    };
    let out = run_both(&fx, &q);
    assert_eq!(out.plan_rows, out.direct_rows);
    assert_eq!(out.plan_rows[0].values[0], num("2"), "DISTINCT：a/b 两个值");
    assert_eq!(
        out.plan_rows[0].values[1],
        num("28"),
        "非 DISTINCT 的 SUM 不受影响"
    );
}
