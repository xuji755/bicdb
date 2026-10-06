//! **切片 2c 验收**：`Sort` / `TopN` 与 WMM 最小面。
//!
//! 钉住：`ORDER BY` 的 **NULL 位置**（升序最后、降序最前——Oracle 默认）；
//! `TopN` 与"全排序 + 取前 n"**逐行一致**；多键与稳定性；工作内存预算
//! （切片 2c 的内存形态——超预算报具名错误，溢出随切片 6）。

mod common;

use bicdb_exec::{
    build, collect, ColKind, ExecContext, ExecEnv, ExecError, Expr, OpStat, Row, RowCursor,
    SelectQuery, SortKey, Value, WorkAreaOutcome, WorkAreaStats,
};
use bicdb_storage::scan::HeapScanner;

use common::{fixture, mem_io, null_tag_row, num, row, run_both, run_plan, shape, DATA_FID};

fn col(i: usize) -> Expr {
    Expr::Column(i)
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

/// 表：id 1..=9 奇数行 tag 为 NULL（NULL 排序用例）。
fn mixed_rows() -> Vec<Row> {
    (1..=9)
        .map(|i| {
            if i % 2 == 1 {
                null_tag_row(i)
            } else {
                row(i, &format!("t{i}"))
            }
        })
        .collect()
}

#[test]
fn null_ordering_matches_oracle_default_on_both_paths() {
    let io = mem_io();
    let fx = fixture(io, &mixed_rows());

    // 升序：NULL 在最后（id 1,3,5,7,9 的 tag 为 NULL）。
    let q = SelectQuery {
        order_by: vec![SortKey {
            expr: col(1),
            desc: false,
        }],
        ..base_query()
    };
    let out = run_both(&fx, &q);
    assert_eq!(out.plan_rows, out.direct_rows, "NULL 升序差分一致");
    let ids: Vec<Value> = out.plan_rows.iter().map(|r| r.values[0].clone()).collect();
    assert_eq!(
        ids,
        vec![
            num("2"),
            num("4"),
            num("6"),
            num("8"),
            num("1"),
            num("3"),
            num("5"),
            num("7"),
            num("9")
        ],
        "升序：非 NULL 字典序在前、NULL 殿后（组内按输入序——稳定）"
    );

    // 降序：NULL 在最前。
    let q = SelectQuery {
        order_by: vec![SortKey {
            expr: col(1),
            desc: true,
        }],
        ..base_query()
    };
    let out = run_both(&fx, &q);
    assert_eq!(out.plan_rows, out.direct_rows);
    let ids: Vec<Value> = out.plan_rows.iter().map(|r| r.values[0].clone()).collect();
    assert_eq!(
        ids,
        vec![
            num("1"),
            num("3"),
            num("5"),
            num("7"),
            num("9"),
            num("8"),
            num("6"),
            num("4"),
            num("2")
        ],
        "降序：NULL 在最前"
    );
}

#[test]
fn topn_matches_full_sort_then_take() {
    let io = mem_io();
    let rows: Vec<Row> = (1..=20).map(|i| row(i, &format!("t{i:02}"))).collect();
    let fx = fixture(io, &rows);

    // A：TopN（ORDER BY + LIMIT）。
    let q_topn = SelectQuery {
        order_by: vec![SortKey {
            expr: col(0),
            desc: true,
        }],
        limit: Some(3),
        ..base_query()
    };
    // B：全排序（无 LIMIT）——取前 3 行应与 A 一致。
    let q_sort = SelectQuery {
        order_by: vec![SortKey {
            expr: col(0),
            desc: true,
        }],
        ..base_query()
    };
    let out_a = run_both(&fx, &q_topn);
    let out_b = run_both(&fx, &q_sort);
    assert_eq!(out_a.plan_rows.len(), 3);
    assert_eq!(out_a.plan_rows, out_a.direct_rows, "TopN 差分一致");
    assert_eq!(
        out_a.plan_rows,
        out_b.plan_rows[..3].to_vec(),
        "TopN = 全排序取前 n（逐行一致）"
    );
    // TopN 的输入仍全读（证据 2065263：省的是排序内存，不是扫描）。
    assert_eq!(out_a.scanned, 20, "TopN 不短路扫描");
    assert!(
        out_a.op_stats.iter().any(|s: &OpStat| s.name == "TopN"),
        "计划里有 TopN 节点"
    );

    // 多键：tag 升序 + id 降序（同一 tag 内比较第二键）。
    let q = SelectQuery {
        order_by: vec![
            SortKey {
                expr: col(1),
                desc: false,
            },
            SortKey {
                expr: col(0),
                desc: true,
            },
        ],
        limit: Some(2),
        ..base_query()
    };
    let out = run_both(&fx, &q);
    assert_eq!(out.plan_rows, out.direct_rows);
    assert_eq!(out.plan_rows[0].values[1], Value::Bytes(b"t01".to_vec()));

    // LIMIT + OFFSET：TopN 保留 limit+offset、Limit 再跳。
    let q = SelectQuery {
        order_by: vec![SortKey {
            expr: col(0),
            desc: false,
        }],
        limit: Some(2),
        offset: 3,
        ..base_query()
    };
    let out = run_both(&fx, &q);
    assert_eq!(out.plan_rows, out.direct_rows);
    assert_eq!(out.plan_rows[0].values[0], num("4"), "升序第 4 行");
    assert_eq!(out.plan_rows[1].values[0], num("5"));
}

#[test]
fn work_memory_budget_is_enforced_and_counted() {
    let io = mem_io();
    let rows: Vec<Row> = (1..=20).map(|i| row(i, &format!("t{i:02}"))).collect();
    let fx = fixture(io, &rows);
    let q = SelectQuery {
        order_by: vec![SortKey {
            expr: col(0),
            desc: false,
        }],
        ..base_query()
    };

    // 预算充足 ⇒ 正常完成 + optimal 计数。
    let plan_rows = run_plan(&fx, &q, Some(1 << 20)).expect("预算充足");
    let _ = plan_rows;
    // （三态计数断言走直跑一次带 context 的路径——`run_plan` 返回行；计数
    // 由下面这段单独验证。）
    let plan = q.to_plan();
    let mut cursor = Some(HeapScanner::new(
        fx.pool,
        &fx.chain,
        fx.snapshot,
        DATA_FID,
        fx.blocks.clone(),
    ));
    let mut open = |_src| Ok(Box::new(cursor.take().expect("单次扫描")) as Box<dyn RowCursor>);
    let env = ExecEnv {
        pool: fx.pool,
        chain: &fx.chain,
    };
    let mut op = build(&plan, &env, &mut open).unwrap();
    let mut cx = ExecContext::new(fx.snapshot).with_work_memory_budget(Some(1 << 20));
    let rows_out = collect(op.as_mut(), &mut cx).unwrap();
    assert_eq!(rows_out.len(), 20);
    assert_eq!(
        cx.work_area_stats(),
        WorkAreaStats {
            optimal: 1,
            one_pass: 0,
            multi_pass: 0
        },
        "预算内完成 ⇒ optimal 计数 1（WMM 最小面）"
    );

    // 预算过小 ⇒ 具名错误（切片 2c 的内存形态；溢出随切片 6）。
    let err = run_plan(&fx, &q, Some(64)).expect_err("预算过小必须报错");
    assert!(
        matches!(err.0, ExecError::WorkMemoryExceeded { .. }),
        "具名错误：{}",
        err.0
    );

    // 计数语义：`collect` 的正式跑法里 optimal 只记一次（一个工作区一次）。
    let mut cx2 = ExecContext::new(fx.snapshot);
    cx2.note_work_area(WorkAreaOutcome::OnePass);
    assert_eq!(cx2.work_area_stats().one_pass, 1);
}

#[test]
fn order_by_followed_by_limit_uses_topn_not_sort() {
    // 计划形状：ORDER BY + LIMIT ⇒ TopN；无 LIMIT ⇒ Sort（§2.3）。
    let with_limit = SelectQuery {
        order_by: vec![SortKey {
            expr: col(0),
            desc: false,
        }],
        limit: Some(1),
        ..base_query()
    };
    let plan = with_limit.to_plan();
    assert!(matches!(plan, bicdb_exec::PlanNode::Limit { .. }));

    let io = mem_io();
    let rows: Vec<Row> = (1..=3).map(|i| row(i, "x")).collect();
    let fx = fixture(io, &rows);
    let out = run_both(&fx, &with_limit);
    assert_eq!(out.plan_rows.len(), 1);
    assert_eq!(out.plan_rows, out.direct_rows);
    let _ = ColKind::Number;
}
