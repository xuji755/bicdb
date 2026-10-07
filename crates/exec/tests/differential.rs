//! **切片 1/2b/2c 验收**：真表（真段 + 真缓冲池 + 真撤销链）上
//! **算子执行器 ↔ 直译执行器逐行差分**（设计 §6；REQ-SQL-005 的验收形态）。
//!
//! 两路共享同一份语义查询（[`SelectQuery`]）：直译路径一个循环走完、
//! 算子路径先 `to_plan()` 再建树——结果必须逐行一致。
//! 另钉：LIMIT 的**真短路**（扫描行数可观测）、取消/截止、空表/空结果、
//! 表达式（算术/CASE/CAST/BETWEEN/COALESCE/NULLIF）与 `ORDER BY` 的差分。

mod common;

use std::sync::atomic::{AtomicBool, Ordering};

use bicdb_exec::{
    build, collect, execute_direct, ArithOp, CmpOp, ColKind, ExecContext, ExecEnv, ExecError, Expr,
    Row, RowCursor, SelectQuery, SortKey, Value,
};
use bicdb_storage::scan::HeapScanner;

use common::{fixture, mem_io, num, row, run_both, shape, DATA_FID};

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
        groups: Vec::new(),
        aggs: Vec::new(),
        having: None,
        projection: vec![col_expr(0), col_expr(1)],
        order_by: Vec::new(),
        limit: None,
        offset: 0,
    }
}

#[test]
fn operators_and_direct_interpreter_agree_row_by_row() {
    let io = mem_io();
    let rows: Vec<Row> = (1..=20).map(|i| row(i, &format!("t{i:02}"))).collect();
    let fx = fixture(io, &rows);
    assert_eq!(fx.blocks.len(), 3, "三张表页");

    // ① 全表（无谓词、全投影）。
    let out = run_both(&fx, &full_query());
    assert_eq!(out.plan_rows, rows, "算子路径 = 插入序");
    assert_eq!(out.direct_rows, rows, "直译路径 = 插入序");
    assert_eq!(out.scanned, 20, "全表扫描行数");

    // ② 谓词 + 投影 + LIMIT/OFFSET。
    let q = SelectQuery {
        predicate: Some(Expr::Compare {
            op: CmpOp::Gt,
            left: Box::new(col_expr(0)),
            right: Box::new(literal(num("15"))),
        }),
        projection: vec![col_expr(1)],
        limit: Some(2),
        offset: 1,
        ..full_query()
    };
    let out = run_both(&fx, &q);
    assert_eq!(out.plan_rows.len(), 2);
    assert_eq!(out.plan_rows, out.direct_rows, "差分一致");
    assert_eq!(
        out.plan_rows[0],
        Row::new(vec![Value::Bytes(b"t17".to_vec())]),
        "id>15 的第 2 条（offset=1）"
    );
    // 短路可观测：谓词放行 16..20，Limit 跳 1 行、产 2 行后不再调子 ⇒
    // SeqScan 恰好产出到 id=18（18 行），**不耗尽全表**（20 行）。
    assert_eq!(out.scanned, 18, "LIMIT 在谓词之后收口");

    // ③ 谓词跳过 + 反向比较。
    let q = SelectQuery {
        predicate: Some(Expr::Compare {
            op: CmpOp::Ne,
            left: Box::new(literal(Value::Bytes(b"t05".to_vec()))),
            right: Box::new(col_expr(1)),
        }),
        projection: vec![col_expr(0)],
        ..full_query()
    };
    let out = run_both(&fx, &q);
    assert_eq!(out.plan_rows, out.direct_rows);
    assert_eq!(out.plan_rows.len(), 19, "排除 t05");

    // ④ IS NULL（本表无 NULL 行 ⇒ 零行；两侧都零行）。
    let q = SelectQuery {
        predicate: Some(Expr::IsNull {
            expr: Box::new(col_expr(0)),
            negated: false,
        }),
        ..full_query()
    };
    let out = run_both(&fx, &q);
    assert!(out.plan_rows.is_empty() && out.direct_rows.is_empty());

    // ⑤ 空结果：谓词不可能满足。
    let q = SelectQuery {
        predicate: Some(Expr::Compare {
            op: CmpOp::Lt,
            left: Box::new(col_expr(0)),
            right: Box::new(literal(num("0"))),
        }),
        ..full_query()
    };
    let out = run_both(&fx, &q);
    assert!(out.plan_rows.is_empty() && out.direct_rows.is_empty());
}

#[test]
fn limit_short_circuits_the_scan() {
    let io = mem_io();
    let rows: Vec<Row> = (1..=20).map(|i| row(i, "x")).collect();
    let fx = fixture(io, &rows);

    let q = SelectQuery {
        limit: Some(3),
        ..full_query()
    };
    let out = run_both(&fx, &q);
    assert_eq!(out.plan_rows.len(), 3);
    assert_eq!(out.plan_rows, out.direct_rows);
    // **真短路**：Limit 产出第 3 行后不再调子 ⇒ 扫描恰 3 行（不是 20）。
    assert_eq!(out.scanned, 3, "LIMIT 短路：扫描行数 = LIMIT");
}

#[test]
fn empty_table_scans_to_nothing() {
    let io = mem_io();
    let fx = fixture(io, &[]);
    let out = run_both(&fx, &full_query());
    assert!(out.plan_rows.is_empty() && out.direct_rows.is_empty());
    assert_eq!(out.scanned, 0, "空表：零行产出");
}

#[test]
fn cancel_and_deadline_are_honoured_on_both_paths() {
    let io = mem_io();
    let rows: Vec<Row> = (1..=5).map(|i| row(i, "x")).collect();
    let fx = fixture(io, &rows);

    // 取消：两路都在首个检查点返回 Cancelled。
    let cancel = AtomicBool::new(true);
    let mut cursor = HeapScanner::new(
        fx.pool,
        &fx.chain,
        bicdb_storage::cr::ReadView::new(fx.snapshot),
        DATA_FID,
        fx.blocks.clone(),
    );
    let mut cx = ExecContext::new(fx.snapshot).with_cancel(&cancel);
    let err = execute_direct(&full_query(), &mut cursor, &mut cx).unwrap_err();
    assert!(matches!(err, ExecError::Cancelled), "{err}");

    let plan = full_query().to_plan();
    let mut cursor2 = Some(HeapScanner::new(
        fx.pool,
        &fx.chain,
        bicdb_storage::cr::ReadView::new(fx.snapshot),
        DATA_FID,
        fx.blocks.clone(),
    ));
    let mut open = |_src| Ok(Box::new(cursor2.take().expect("单次扫描")) as Box<dyn RowCursor>);
    let env = ExecEnv {
        pool: fx.pool,
        chain: Some(&fx.chain),
        spill: None,
        writer: None,
    };
    let mut op = build(&plan, &env, &mut open).unwrap();
    let mut cx2 = ExecContext::new(fx.snapshot).with_cancel(&cancel);
    let err = collect(op.as_mut(), &mut cx2).unwrap_err();
    assert!(matches!(err, ExecError::Cancelled), "{err}");

    // 运行中置位取消（检查点在每个 next 开头）。
    let cancel2 = AtomicBool::new(false);
    let mut cursor3 = HeapScanner::new(
        fx.pool,
        &fx.chain,
        bicdb_storage::cr::ReadView::new(fx.snapshot),
        DATA_FID,
        fx.blocks.clone(),
    );
    let mut cx3 = ExecContext::new(fx.snapshot).with_cancel(&cancel2);
    cancel2.store(true, Ordering::Relaxed);
    let err = execute_direct(&full_query(), &mut cursor3, &mut cx3).unwrap_err();
    assert!(matches!(err, ExecError::Cancelled), "{err}");

    // 截止时间：已过 ⇒ Deadline（与取消同路径、判定分开）。
    let deadline = std::time::Instant::now() - std::time::Duration::from_millis(1);
    let mut cursor4 = HeapScanner::new(
        fx.pool,
        &fx.chain,
        bicdb_storage::cr::ReadView::new(fx.snapshot),
        DATA_FID,
        fx.blocks.clone(),
    );
    let mut cx4 = ExecContext::new(fx.snapshot).with_deadline(deadline);
    let err = execute_direct(&full_query(), &mut cursor4, &mut cx4).unwrap_err();
    assert!(matches!(err, ExecError::Deadline), "{err}");
}

#[test]
fn expression_semantics_match_between_paths() {
    // 切片 2b：算术/CASE/BETWEEN/COALESCE/NULLIF 在两条路径上同语义。
    let io = mem_io();
    let rows: Vec<Row> = (1..=10).map(|i| row(i, "x")).collect();
    let fx = fixture(io, &rows);

    let expr = Expr::Case {
        whens: vec![(
            Expr::Between {
                expr: Box::new(col_expr(0)),
                low: Box::new(literal(num("3"))),
                high: Box::new(literal(num("7"))),
            },
            Expr::Arith {
                op: ArithOp::Mul,
                left: Box::new(col_expr(0)),
                right: Box::new(literal(num("10"))),
            },
        )],
        else_: Some(Box::new(Expr::Coalesce(vec![
            Expr::NullIf {
                left: Box::new(col_expr(0)),
                right: Box::new(literal(num("1"))),
            },
            literal(num("-1")),
        ]))),
    };
    let q = SelectQuery {
        projection: vec![col_expr(0), expr],
        order_by: vec![SortKey {
            expr: col_expr(0),
            desc: true,
        }],
        ..full_query()
    };
    let out = run_both(&fx, &q);
    assert_eq!(out.plan_rows, out.direct_rows, "表达式 + 排序差分一致");
    assert_eq!(out.plan_rows.len(), 10);
    // 降序首行 id=10 ⇒ 不在 [3,7] ⇒ COALESCE(NULLIF(10,1)=10, -1) = 10。
    assert_eq!(out.plan_rows[0].values[0], num("10"));
    assert_eq!(out.plan_rows[0].values[1], num("10"));
    // id=1 在末尾：NULLIF(1,1) = NULL ⇒ COALESCE 落到 -1。
    assert_eq!(out.plan_rows[9].values[0], num("1"));
    assert_eq!(out.plan_rows[9].values[1], num("-1"));
    // id=5 ⇒ 5×10 = 50。
    assert_eq!(out.plan_rows[5].values[1], num("50"));
}

#[test]
fn cast_between_number_and_text_through_the_plan() {
    let io = mem_io();
    let rows: Vec<Row> = vec![row(7, "x")];
    let fx = fixture(io, &rows);
    // CAST(id AS BYTES) 再 CAST 回 NUMBER ⇒ 值不变（十进制文本往返）。
    let to_bytes = Expr::Cast {
        expr: Box::new(col_expr(0)),
        to: ColKind::Bytes,
    };
    let back = Expr::Cast {
        expr: Box::new(to_bytes.clone()),
        to: ColKind::Number,
    };
    let q = SelectQuery {
        projection: vec![to_bytes, back],
        ..full_query()
    };
    let out = run_both(&fx, &q);
    assert_eq!(out.plan_rows, out.direct_rows);
    assert_eq!(out.plan_rows[0].values[0], Value::Bytes(b"7".to_vec()));
    assert_eq!(out.plan_rows[0].values[1], num("7"));
}
