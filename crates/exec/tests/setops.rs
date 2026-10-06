//! **切片 5 验收**：`Append` / `Unique` / `SetOp`（设计 §2.7 排序归并路线）。
//!
//! 钉住：`UNION ALL` = 合并追加（保重数）；`UNION`/`SELECT DISTINCT` = 去重；
//! `INTERSECT [ALL]` / `EXCEPT [ALL]` 的**重数语义**（`min` / `cl−cr`）；
//! **NULL 等价类**（`NULL` 在集合运算里彼此相等——不是 `WHERE` 的三值比较）；
//! 参考侧 = 用例内手算（朴素计数法）。

mod common;

use std::cmp::Ordering;

use bicdb_exec::{
    all_columns_keys, build, collect, CmpOp, ExecContext, ExecEnv, Expr, PlanNode, Row, RowCursor,
    SetOpKind, Value,
};
use bicdb_storage::scan::HeapScanner;

use common::{fixture, mem_io, null_tag_row, num, row, shape, Fixture, DATA_FID};

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

/// 表：1a 2b 3b 4c 5c 6NULL。
fn rows() -> Vec<Row> {
    vec![
        row(1, "a"),
        row(2, "b"),
        row(3, "b"),
        row(4, "c"),
        row(5, "c"),
        null_tag_row(6),
    ]
}

/// 只投影 tag 列的一侧（制造重复值——测重数）。
fn tag_side(pred: Expr) -> PlanNode {
    PlanNode::Project {
        input: Box::new(PlanNode::Filter {
            input: Box::new(PlanNode::SeqScan {
                source: 0,
                shape: shape(),
            }),
            predicate: pred,
        }),
        exprs: vec![col(1)],
    }
}

fn run(fx: &Fixture, plan: &PlanNode) -> Vec<Row> {
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
        spill: None,
        writer: None,
    };
    let mut op = build(plan, &env, &mut open).unwrap();
    let mut cx = ExecContext::new(fx.snapshot);
    collect(op.as_mut(), &mut cx).unwrap()
}

/// 朴素参考：按等价类（`Value` 的 `PartialEq`——`NULL == NULL`）分组计数。
fn naive(op: SetOpKind, all: bool, left: &[Row], right: &[Row]) -> Vec<Row> {
    fn counts(rows: &[Row]) -> Vec<(Row, u64)> {
        let mut out: Vec<(Row, u64)> = Vec::new();
        for r in rows {
            match out.iter_mut().find(|(k, _)| k == r) {
                Some((_, c)) => *c += 1,
                None => out.push((r.clone(), 1)),
            }
        }
        out
    }
    let (lc, rc) = (counts(left), counts(right));
    let mut result: Vec<(Row, u64)> = Vec::new();
    match op {
        SetOpKind::Intersect => {
            for (k, cl) in &lc {
                if let Some((_, cr)) = rc.iter().find(|(rk, _)| rk == k) {
                    result.push((k.clone(), if all { (*cl).min(*cr) } else { 1 }));
                }
            }
        }
        SetOpKind::Except => {
            for (k, cl) in &lc {
                let cr = rc
                    .iter()
                    .find(|(rk, _)| rk == k)
                    .map(|(_, c)| *c)
                    .unwrap_or(0);
                if *cl > cr {
                    result.push((k.clone(), if all { cl - cr } else { 1 }));
                }
            }
        }
    }
    let mut out = Vec::new();
    for (k, c) in result {
        for _ in 0..c {
            out.push(k.clone());
        }
    }
    // 归一化输出序：按值排序（算子输出为全序归并序，此处对齐比较）。
    out.sort_by(|a, b| compare_rows_lex(a, b).unwrap_or(Ordering::Equal));
    out
}

fn compare_rows_lex(a: &Row, b: &Row) -> Option<Ordering> {
    for (x, y) in a.values.iter().zip(b.values.iter()) {
        let ord = match (x.is_null(), y.is_null()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => bicdb_exec::expr::compare_values(x, y).ok()??,
        };
        if ord != Ordering::Equal {
            return Some(ord);
        }
    }
    Some(Ordering::Equal)
}

fn sorted(mut rows: Vec<Row>) -> Vec<Row> {
    rows.sort_by(|a, b| compare_rows_lex(a, b).unwrap_or(Ordering::Equal));
    rows
}

#[test]
fn union_all_appends_and_union_dedups() {
    let io = mem_io();
    let fx = fixture(io, &rows());
    let left = tag_side(bound(0, CmpOp::Le, num("3"))); // tags: a,b,b
    let right = tag_side(bound(0, CmpOp::Ge, num("2"))); // tags: b,b,c,c,NULL

    // UNION ALL：合并且**保重数**（3 + 5 = 8 行）。
    let all = run(
        &fx,
        &PlanNode::Append {
            inputs: vec![left.clone(), right.clone()],
        },
    );
    assert_eq!(all.len(), 8, "UNION ALL 保重数");
    assert_eq!(
        &all[..3],
        &[
            Row::new(vec![Value::Bytes(b"a".to_vec())]),
            Row::new(vec![Value::Bytes(b"b".to_vec())]),
            Row::new(vec![Value::Bytes(b"b".to_vec())]),
        ]
    );

    // UNION：去重后 = a,b,c,NULL 四行。
    let uniq = run(
        &fx,
        &PlanNode::Unique {
            input: Box::new(PlanNode::Append {
                inputs: vec![left.clone(), right.clone()],
            }),
            keys: None,
            width: 1,
        },
    );
    assert_eq!(uniq.len(), 4, "UNION 去重 ⇒ 4 个不同值");
    assert_eq!(
        sorted(uniq),
        sorted(vec![
            Row::new(vec![Value::Bytes(b"a".to_vec())]),
            Row::new(vec![Value::Bytes(b"b".to_vec())]),
            Row::new(vec![Value::Bytes(b"c".to_vec())]),
            Row::new(vec![Value::Null]),
        ]),
        "去重集合 = a / b / c / NULL"
    );

    // SELECT DISTINCT（单侧）：tags b,b,c,c,NULL ⇒ {b,c,NULL}。
    let distinct = run(
        &fx,
        &PlanNode::Unique {
            input: Box::new(right.clone()),
            keys: None,
            width: 1,
        },
    );
    assert_eq!(distinct.len(), 3, "b/c/NULL 三个不同值");
}

#[test]
fn intersect_and_except_follow_multiplicity_semantics() {
    let io = mem_io();
    let fx = fixture(io, &rows());
    let left = tag_side(bound(0, CmpOp::Le, num("3"))); // a,b,b
    let right = tag_side(bound(0, CmpOp::Ge, num("2"))); // b,b,c,c,NULL
    let width = 1;
    let _ = all_columns_keys(width);

    for (kind, all, expect_len) in [
        (SetOpKind::Intersect, false, 1), // {b}
        (SetOpKind::Intersect, true, 2),  // b×min(2,2)
        (SetOpKind::Except, false, 1),    // {a}
        (SetOpKind::Except, true, 1),     // a×(1−0)
    ] {
        let plan = PlanNode::SetOp {
            left: Box::new(left.clone()),
            right: Box::new(right.clone()),
            kind,
            all,
            width,
        };
        let got = run(&fx, &plan);
        assert_eq!(got.len(), expect_len, "{kind:?} all={all} 行数（重数语义）");
        // 与朴素参考一致（集合语义，按值排序后比较）。
        let left_rows = vec![
            Row::new(vec![Value::Bytes(b"a".to_vec())]),
            Row::new(vec![Value::Bytes(b"b".to_vec())]),
            Row::new(vec![Value::Bytes(b"b".to_vec())]),
        ];
        let right_rows = vec![
            Row::new(vec![Value::Bytes(b"b".to_vec())]),
            Row::new(vec![Value::Bytes(b"b".to_vec())]),
            Row::new(vec![Value::Bytes(b"c".to_vec())]),
            Row::new(vec![Value::Bytes(b"c".to_vec())]),
            Row::new(vec![Value::Null]),
        ];
        assert_eq!(
            sorted(got),
            sorted(naive(kind, all, &left_rows, &right_rows)),
            "{kind:?} all={all} 与朴素参考一致"
        );
    }

    // 反向 EXCEPT：右侧 − 左侧 = c,c,NULL（c 的重数 2、NULL 在左不存在）。
    let plan = PlanNode::SetOp {
        left: Box::new(right.clone()),
        right: Box::new(left.clone()),
        kind: SetOpKind::Except,
        all: true,
        width,
    };
    let got = run(&fx, &plan);
    assert_eq!(got.len(), 3, "c 两份 + NULL 一份");
    assert!(got.iter().any(|r| r.values[0].is_null()), "NULL 作为值保留");
}

#[test]
fn null_rows_are_equivalent_in_set_operations() {
    // NULL 等价类：两行 NULL 在 INTERSECT 里算**同一个值**（不是 UNKNOWN）。
    let io = mem_io();
    let fx = fixture(io, &rows());
    // 左：所有 tag（含 1 个 NULL）；右：所有 tag（含 1 个 NULL）——即同一集合。
    let side = tag_side(Expr::Compare {
        op: CmpOp::Ge,
        left: Box::new(col(0)),
        right: Box::new(lit(num("0"))),
    });
    let plan = PlanNode::SetOp {
        left: Box::new(side.clone()),
        right: Box::new(side),
        kind: SetOpKind::Intersect,
        all: true,
        width: 1,
    };
    let got = run(&fx, &plan);
    assert_eq!(
        got.len(),
        6,
        "自我交集 = 原多重集（a×1 + b×2 + c×2 + NULL×1）"
    );
    assert!(
        got.iter().filter(|r| r.values[0].is_null()).count() == 1,
        "NULL 与自身相等（等价类语义——若用三值比较会丢行）"
    );
}
