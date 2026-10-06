//! **切片 6a 验收**：`HashJoin`（INNER/LEFT，构建侧建表 + 探测侧流式）。
//!
//! 参考侧：手算朴素嵌套循环（同一求值语义）；自连接形态（单表 + 谓词分侧）。
//! 另钉：键等值匹配 + `qual` 过滤的**交互**（LEFT 下 qual 不过 ⇒ 视为未匹配
//! 补 NULL）；重复键的多重匹配；空构建侧（LEFT 全补 NULL）。

mod common;

use bicdb_exec::{
    build, collect, CmpOp, ExecContext, ExecEnv, Expr, JoinKind, PlanNode, Row, RowCursor, Value,
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

/// 表：1a 2b 2c 4d（id=2 重复——测重复键）；外加 5NULL。
fn rows() -> Vec<Row> {
    vec![
        row(1, "a"),
        row(2, "b"),
        row(2, "c"),
        row(4, "d"),
        null_tag_row(5),
    ]
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

/// 朴素参考：自连接（两侧同一谓词）——组合行 = 探测行 ++ 构建行。
fn naive(
    kind: JoinKind,
    probe_pred: &dyn Fn(&Row) -> bool,
    build_pred: &dyn Fn(&Row) -> bool,
    all: &[Row],
) -> Vec<Row> {
    let probe: Vec<&Row> = all.iter().filter(|r| probe_pred(r)).collect();
    let build: Vec<&Row> = all.iter().filter(|r| build_pred(r)).collect();
    let mut out = Vec::new();
    for p in &probe {
        let mut any = false;
        for b in &build {
            if p.values[0] == b.values[0] {
                let mut v = p.values.clone();
                v.extend(b.values.iter().cloned());
                out.push(Row::new(v));
                any = true;
            }
        }
        if !any && kind == JoinKind::Left {
            let mut v = p.values.clone();
            v.extend([Value::Null, Value::Null]);
            out.push(Row::new(v));
        }
    }
    out
}

#[test]
fn hash_join_inner_matches_naive_reference() {
    let io = mem_io();
    let fx = fixture(io, &rows());
    // 构建侧：id ≤ 2（1a、2b、2c）；探测侧：全部。
    let plan = PlanNode::HashJoin {
        build: Box::new(side(bound(0, CmpOp::Le, num("2")))),
        probe: Box::new(scan()),
        build_keys: vec![col(0)],
        probe_keys: vec![col(0)],
        kind: JoinKind::Inner,
        qual: None,
        build_width: 2,
    };
    let got = run(&fx, &plan);
    let all = rows();
    let expected = naive(
        JoinKind::Inner,
        &|_| true,
        &|r| r.values[0] == num("1") || r.values[0] == num("2"),
        &all,
    );
    assert_eq!(got, expected, "HashJoin INNER == 朴素参考");
    // 重复键：探测侧 id=2 两行 × 构建侧 id=2 两行 = 4 对；id=1 一对。
    assert_eq!(got.len(), 5, "1 对 + 2×2 对");
}

#[test]
fn hash_join_left_pads_unmatched_probe_rows() {
    let io = mem_io();
    let fx = fixture(io, &rows());
    // 构建侧：只有 id=2（两行）；探测侧：全部（含 id=1/4/NULL 无匹配）。
    let plan = PlanNode::HashJoin {
        build: Box::new(side(bound(0, CmpOp::Eq, num("2")))),
        probe: Box::new(scan()),
        build_keys: vec![col(0)],
        probe_keys: vec![col(0)],
        kind: JoinKind::Left,
        qual: None,
        build_width: 2,
    };
    let got = run(&fx, &plan);
    // 探测 5 行：id=2 的两行各匹配 2 行（4 对）；id=1/4/NULL 三行补 NULL。
    assert_eq!(got.len(), 4 + 3, "4 对 + 3 补 NULL");
    let padded: Vec<&Row> = got.iter().filter(|r| r.values[2].is_null()).collect();
    assert_eq!(padded.len(), 3);
    assert!(
        padded.iter().all(|r| r.values[3].is_null()),
        "构建侧两列都补 NULL"
    );
    // 补 NULL 的三行 = id 1/4/5（其中 5 行的 tag 为 NULL——非键列的 NULL
    // 不影响匹配判定）。
    let mut ids: Vec<Value> = padded.iter().map(|r| r.values[0].clone()).collect();
    ids.sort_by_key(|v| format!("{v:?}"));
    assert_eq!(ids, vec![num("1"), num("4"), num("5")]);
}

#[test]
fn hash_join_qual_filters_and_left_semantics_interact() {
    let io = mem_io();
    let fx = fixture(io, &rows());
    // 键等值匹配后，qual 要求 tag 相等 ⇒ 只有 (2b,2b)、(2c,2c) 配对；
    // 探测行 2b 与 2c 各匹配 1 次（跨行被 qual 挡掉）。
    let plan = PlanNode::HashJoin {
        build: Box::new(side(bound(0, CmpOp::Eq, num("2")))),
        probe: Box::new(side(bound(0, CmpOp::Eq, num("2")))),
        build_keys: vec![col(0)],
        probe_keys: vec![col(0)],
        kind: JoinKind::Left,
        qual: Some(Expr::Compare {
            op: CmpOp::Eq,
            left: Box::new(col(1)),  // 探测行 tag
            right: Box::new(col(3)), // 构建行 tag
        }),
        build_width: 2,
    };
    let got = run(&fx, &plan);
    assert_eq!(got.len(), 2, "2b 配 2b、2c 配 2c");
    for r in &got {
        assert_eq!(r.values[1], r.values[3], "tag 相等（qual 生效）");
    }

    // qual 永不满足 + LEFT ⇒ 每个探测行都补 NULL（**qual 不过算未匹配**）。
    let plan2 = PlanNode::HashJoin {
        build: Box::new(side(bound(0, CmpOp::Eq, num("2")))),
        probe: Box::new(side(bound(0, CmpOp::Eq, num("2")))),
        build_keys: vec![col(0)],
        probe_keys: vec![col(0)],
        kind: JoinKind::Left,
        qual: Some(lit(Value::Bool(false))),
        build_width: 2,
    };
    let got2 = run(&fx, &plan2);
    assert_eq!(got2.len(), 2, "两个探测行都补 NULL");
    assert!(got2.iter().all(|r| r.values[2].is_null()));
}

#[test]
fn empty_build_side_pads_everything_on_left_join() {
    let io = mem_io();
    let fx = fixture(io, &rows());
    // 构建侧为空（谓词不可能满足）。
    let plan = PlanNode::HashJoin {
        build: Box::new(side(bound(0, CmpOp::Lt, num("0")))),
        probe: Box::new(scan()),
        build_keys: vec![col(0)],
        probe_keys: vec![col(0)],
        kind: JoinKind::Left,
        qual: None,
        build_width: 2,
    };
    let got = run(&fx, &plan);
    assert_eq!(got.len(), 5, "5 个探测行全补 NULL");
    assert!(got
        .iter()
        .all(|r| r.values[2].is_null() && r.values[3].is_null()));

    // INNER + 空构建 ⇒ 零行。
    let plan2 = PlanNode::HashJoin {
        build: Box::new(side(bound(0, CmpOp::Lt, num("0")))),
        probe: Box::new(scan()),
        build_keys: vec![col(0)],
        probe_keys: vec![col(0)],
        kind: JoinKind::Inner,
        qual: None,
        build_width: 2,
    };
    assert!(run(&fx, &plan2).is_empty());
}

#[test]
fn hash_join_respects_work_memory_budget() {
    let io = mem_io();
    let fx = fixture(io, &rows());
    let plan = PlanNode::HashJoin {
        build: Box::new(scan()),
        probe: Box::new(scan()),
        build_keys: vec![col(0)],
        probe_keys: vec![col(0)],
        kind: JoinKind::Inner,
        qual: None,
        build_width: 2,
    };
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
    let mut op = build(&plan, &env, &mut open).unwrap();
    let mut cx = ExecContext::new(fx.snapshot).with_work_memory_budget(Some(64));
    let err = collect(op.as_mut(), &mut cx).unwrap_err();
    assert!(
        matches!(err, bicdb_exec::ExecError::WorkMemoryExceeded { .. }),
        "构建侧超预算具名报错（溢出随切片 6b）：{err}"
    );
}
