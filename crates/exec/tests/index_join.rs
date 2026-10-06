//! **切片 3 验收**：`IndexScan`（范围/覆盖 + 批量回表）与 `NestedLoop`。
//!
//! 真件：真段 + 真缓冲池 + 真撤销链 + **真 B+Tree 索引**（`bicdb-index`）。
//! 参考侧：`SeqScan` 计划与直译执行器；连接用手算的朴素嵌套循环。
//! 连接用例用**自连接**形态（单表 + 索引，语义与两表等价——避免多段夹具
//! 的控制文件/日志路径纠缠）。

mod common;

use bicdb_exec::{
    build, collect, execute_direct, CmpOp, ColKind, ExecContext, ExecEnv, ExecError, Expr,
    JoinKind, PlanNode, Row, RowCursor, SelectQuery, Value,
};
use bicdb_storage::rowid::RowId;
use bicdb_storage::scan::HeapScanner;
use bicdb_types::Number;

use common::{build_env, build_index, create_table, mem_io, num, row, shape, Env, DATA_FID};

fn col(i: usize) -> Expr {
    Expr::Column(i)
}

fn lit(v: Value) -> Expr {
    Expr::Literal(v)
}

fn id_of(r: &Row) -> Value {
    r.values[0].clone()
}

/// 自连接用的表：id 集合含重复键。
fn self_join_rows() -> Vec<Row> {
    vec![row(1, "b1"), row(2, "b2a"), row(2, "b2b"), row(4, "b4")]
}

/// 建表 + 索引；返回（块表, 根）。
fn table_with_index(
    env: &mut Env,
    io: &'static bicdb_workspace::io::MemFileIo,
) -> (Vec<u32>, RowId) {
    let rows = self_join_rows();
    let table = create_table(env, io, &rows);
    let entries: Vec<(Vec<u8>, RowId)> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let id = match &r.values[0] {
                Value::Number(n) => n.encode(),
                _ => unreachable!(),
            };
            (id, table.rowids[i])
        })
        .collect();
    let root = build_index(env, &entries);
    (table.blocks, root)
}

fn index_scan_plan(
    root: RowId,
    low: Option<Expr>,
    high: Option<Expr>,
    covered: bool,
    batch: Option<usize>,
) -> PlanNode {
    PlanNode::IndexScan {
        file_id: DATA_FID,
        root,
        key_kind: ColKind::Number,
        shape: shape(),
        low,
        high,
        covered,
        limit: None,
        batch,
    }
}

/// 跑一个计划（无 SeqScan 源——索引自足）；返回行与统计。
fn run_plan_only(env: &Env, plan: &PlanNode) -> Result<(Vec<Row>, usize), ExecError> {
    let mut open = |_src| Err::<Box<dyn RowCursor>, ExecError>(ExecError::NoSuchSource { id: 0 });
    let envx = ExecEnv {
        pool: env.pool,
        chain: &env.chain,
    };
    let mut op = build(plan, &envx, &mut open)?;
    let mut cx = ExecContext::new(env.snapshot);
    let rows = collect(op.as_mut(), &mut cx)?;
    let fetches = cx.stats().iter().filter(|s| s.name == "IndexScan").count();
    Ok((rows, fetches))
}

#[test]
fn index_range_scan_agrees_with_direct_interpreter() {
    let io = mem_io();
    let mut env = build_env(io);
    let (blocks, root) = table_with_index(&mut env, io);

    // 索引范围 [2, 4]（闭区间：命中 2,2,4 三行，按键序）。
    let plan = index_scan_plan(root, Some(lit(num("2"))), Some(lit(num("4"))), false, None);
    let (indexed, _) = run_plan_only(&env, &plan).unwrap();
    assert_eq!(
        indexed.iter().map(id_of).collect::<Vec<_>>(),
        vec![num("2"), num("2"), num("4")],
        "闭区间、按键序、重复键保留"
    );

    // 直译执行器（SeqScan 源 + 同一谓词）——逐行一致。
    let query = SelectQuery {
        source: 0,
        shape: shape(),
        predicate: Some(Expr::And(vec![
            Expr::Compare {
                op: CmpOp::Ge,
                left: Box::new(col(0)),
                right: Box::new(lit(num("2"))),
            },
            Expr::Compare {
                op: CmpOp::Le,
                left: Box::new(col(0)),
                right: Box::new(lit(num("4"))),
            },
        ])),
        projection: vec![col(0), col(1)],
        order_by: Vec::new(),
        limit: None,
        offset: 0,
    };
    let mut cursor = HeapScanner::new(env.pool, &env.chain, env.snapshot, DATA_FID, blocks);
    let mut cx = ExecContext::new(env.snapshot);
    let direct = execute_direct(&query, &mut cursor, &mut cx).unwrap();
    assert_eq!(indexed, direct, "索引路径 == 直译路径");
}

#[test]
fn index_scan_batches_the_table_fetch_and_skips_dead_rows() {
    let io = mem_io();
    let mut env = build_env(io);
    let rows = self_join_rows();
    let table = create_table(&mut env, io, &rows);
    // 索引里**多一条死行**（指向不存在的槽——并发删除后的死索引项）：
    // 回表取不到 ⇒ 跳过（不报错）。
    let mut entries: Vec<(Vec<u8>, RowId)> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let id = match &r.values[0] {
                Value::Number(n) => n.encode(),
                _ => unreachable!(),
            };
            (id, table.rowids[i])
        })
        .collect();
    entries.push((
        Number::parse("9").unwrap().encode(),
        RowId::from_parts(DATA_FID, table.blocks[0], 999).unwrap(),
    ));
    let root = build_index(&mut env, &entries);

    let plan = index_scan_plan(root, None, None, false, Some(2)); // 5 条 ⇒ 3 批
    let (got, _) = run_plan_only(&env, &plan).unwrap();
    assert_eq!(got.len(), 4, "死行被跳过（5 条条目 → 4 行）");
    assert_eq!(
        got.iter().map(id_of).collect::<Vec<_>>(),
        vec![num("1"), num("2"), num("2"), num("4")],
        "按键序"
    );
}

#[test]
fn covered_index_scan_reads_keys_without_table_access() {
    let io = mem_io();
    let mut env = build_env(io);
    let (_, root) = table_with_index(&mut env, io);

    let plan = index_scan_plan(root, Some(lit(num("2"))), Some(lit(num("4"))), true, None);
    let (got, _) = run_plan_only(&env, &plan).unwrap();
    assert_eq!(got.len(), 3, "覆盖扫描：只出键列");
    assert_eq!(
        got.iter().map(id_of).collect::<Vec<_>>(),
        vec![num("2"), num("2"), num("4")],
        "键序、重复键保留"
    );
    assert!(got.iter().all(|r| r.values.len() == 1), "覆盖扫描单列");
}

#[test]
fn nested_loop_with_index_inner_matches_naive_reference() {
    let io = mem_io();
    let mut env = build_env(io);
    let (blocks, root) = table_with_index(&mut env, io);

    // 自连接 INNER：外层全表、内层索引按外层 id 探测、qual = inner.id = outer.id。
    let plan = PlanNode::NestedLoop {
        outer: Box::new(PlanNode::SeqScan {
            source: 0,
            shape: shape(),
        }),
        inner: Box::new(index_scan_plan(
            root,
            Some(Expr::Param(0)),
            Some(Expr::Param(0)),
            false,
            None,
        )),
        inner_params: vec![col(0)],
        kind: JoinKind::Inner,
        qual: Some(Expr::Compare {
            op: CmpOp::Eq,
            left: Box::new(col(2)),  // 组合行：inner.id
            right: Box::new(col(0)), // outer.id
        }),
        inner_width: 2,
    };
    let mut open = |_src| {
        Ok(Box::new(HeapScanner::new(
            env.pool,
            &env.chain,
            env.snapshot,
            DATA_FID,
            blocks.clone(),
        )) as Box<dyn RowCursor>)
    };
    let envx = ExecEnv {
        pool: env.pool,
        chain: &env.chain,
    };
    let mut op = build(&plan, &envx, &mut open).unwrap();
    let mut cx = ExecContext::new(env.snapshot);
    let joined = collect(op.as_mut(), &mut cx).unwrap();

    // 朴素参考：自连接 = 每个外层行与**同 id 的每一行**配对（含自身）。
    let rows = self_join_rows();
    let mut expected: Vec<Row> = Vec::new();
    for o in &rows {
        for i in &rows {
            if o.values[0] == i.values[0] {
                let mut v = o.values.clone();
                v.extend(i.values.iter().cloned());
                expected.push(Row::new(v));
            }
        }
    }
    assert_eq!(joined, expected, "NL（索引内表）== 朴素参考");
    assert_eq!(
        joined.len(),
        1 + 2 + 2 + 1,
        "自连接：每外层行 × 同 id 的内层行数"
    );
}

#[test]
fn left_join_pads_unmatched_outer_rows() {
    let io = mem_io();
    let mut env = build_env(io);
    let (blocks, root) = table_with_index(&mut env, io);

    // LEFT：内层按 outer.id + 1 探测（id=2 的两行与 id=4 无下一号 ⇒ 补 NULL）。
    let plan = PlanNode::NestedLoop {
        outer: Box::new(PlanNode::SeqScan {
            source: 0,
            shape: shape(),
        }),
        inner: Box::new(index_scan_plan(
            root,
            Some(Expr::Param(0)),
            Some(Expr::Param(0)),
            false,
            None,
        )),
        inner_params: vec![Expr::Arith {
            op: bicdb_exec::ArithOp::Add,
            left: Box::new(col(0)),
            right: Box::new(lit(num("1"))),
        }],
        kind: JoinKind::Left,
        qual: Some(Expr::Compare {
            op: CmpOp::Eq,
            left: Box::new(col(2)),
            right: Box::new(Expr::Arith {
                op: bicdb_exec::ArithOp::Add,
                left: Box::new(col(0)),
                right: Box::new(lit(num("1"))),
            }),
        }),
        inner_width: 2,
    };
    let mut open = |_src| {
        Ok(Box::new(HeapScanner::new(
            env.pool,
            &env.chain,
            env.snapshot,
            DATA_FID,
            blocks.clone(),
        )) as Box<dyn RowCursor>)
    };
    let envx = ExecEnv {
        pool: env.pool,
        chain: &env.chain,
    };
    let mut op = build(&plan, &envx, &mut open).unwrap();
    let mut cx = ExecContext::new(env.snapshot);
    let joined = collect(op.as_mut(), &mut cx).unwrap();

    // 期望：id=1 ⇒ 与两行 id=2 配对（2 行）；id=2（两行）与 id=4 ⇒ 无匹配补 NULL。
    assert_eq!(joined.len(), 2 + 3, "2 匹配 + 3 补 NULL");
    let padded: Vec<&Row> = joined.iter().filter(|r| r.values[2].is_null()).collect();
    assert_eq!(padded.len(), 3, "三行补 NULL");
    assert!(
        padded.iter().all(|r| r.values[3].is_null()),
        "内层两列都补 NULL"
    );
    let matched: Vec<&Row> = joined.iter().filter(|r| !r.values[2].is_null()).collect();
    assert!(
        matched.iter().all(|r| id_of(r) == num("1")),
        "只有 id=1 有匹配"
    );
    assert!(
        matched.iter().all(|r| r.values[2] == num("2")),
        "匹配到的是 id+1 = 2"
    );
}
