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

use common::{
    build_env, build_index, create_table, index_key_number, mem_io, num, row, shape, Env, DATA_FID,
};

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
fn table_with_index(env: &mut Env, io: &'static bicdb_workspace::io::MemFileIo) -> (Vec<u32>, u32) {
    let rows = self_join_rows();
    let table = create_table(env, io, &rows);
    let entries: Vec<(Vec<u8>, RowId)> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let id = match &r.values[0] {
                Value::Number(n) => index_key_number(n),
                _ => unreachable!(),
            };
            (id, table.rowids[i])
        })
        .collect();
    let seg0 = build_index(env, &entries);
    (table.blocks, seg0)
}

fn index_scan_plan(
    seg_page0: u32,
    low: Option<Expr>,
    high: Option<Expr>,
    covered: bool,
    batch: Option<usize>,
) -> PlanNode {
    index_scan_bounds(seg_page0, low, false, high, false, covered, batch)
}

/// 带**开闭**的形态（`>`/`<` 用）。
#[allow(clippy::too_many_arguments)]
fn index_scan_bounds(
    seg_page0: u32,
    low: Option<Expr>,
    low_exclusive: bool,
    high: Option<Expr>,
    high_exclusive: bool,
    covered: bool,
    batch: Option<usize>,
) -> PlanNode {
    PlanNode::IndexScan {
        file_id: DATA_FID,
        seg_page0,
        key_kind: ColKind::Number,
        shape: shape(),
        low,
        low_exclusive,
        high,
        high_exclusive,
        points: Vec::new(),
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
        chain: Some(&env.chain),
        spill: None,
        writer: None,
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
    let (blocks, seg0) = table_with_index(&mut env, io);

    // 索引范围 [2, 4]（闭区间：命中 2,2,4 三行，按键序）。
    let plan = index_scan_plan(seg0, Some(lit(num("2"))), Some(lit(num("4"))), false, None);
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
        groups: Vec::new(),
        aggs: Vec::new(),
        having: None,
        projection: vec![col(0), col(1)],
        order_by: Vec::new(),
        limit: None,
        offset: 0,
    };
    let mut cursor = HeapScanner::new(
        env.pool,
        &env.chain,
        bicdb_storage::cr::ReadView::new(env.snapshot),
        DATA_FID,
        blocks,
    );
    let mut cx = ExecContext::new(env.snapshot);
    let direct = execute_direct(&query, &mut cursor, &mut cx).unwrap();
    assert_eq!(indexed, direct, "索引路径 == 直译路径");
}

/// **开区间的端点**：`Tree::range` 只有闭区间，`>`/`<` 的端点要在收集时剔掉——
/// 剔错（少剔）会让 `WHERE k > 2` 多出一条 `k = 2` 的行，**静默错**。
#[test]
fn exclusive_bounds_drop_the_endpoint_keys() {
    let io = mem_io();
    let mut env = build_env(io);
    let (_, seg0) = table_with_index(&mut env, io);
    // 表里是 {1, 2, 2, 4}；`k > 2` ⇒ 只有 4；`k >= 2` ⇒ 2,2,4；`k < 4` ⇒ 1,2,2。
    /// 一组开闭对照：`(下界, 下界开?, 上界, 上界开?, 期望的 id)`。
    type Case = (Option<Expr>, bool, Option<Expr>, bool, Vec<i64>);
    let cases: Vec<Case> = vec![
        (Some(lit(num("2"))), true, None, false, vec![4]),
        (Some(lit(num("2"))), false, None, false, vec![2, 2, 4]),
        (None, false, Some(lit(num("4"))), true, vec![1, 2, 2]),
        (None, false, Some(lit(num("4"))), false, vec![1, 2, 2, 4]),
        // 区间两端都开：`2 < k < 4` ⇒ 空（表里没有 3）。
        (Some(lit(num("2"))), true, Some(lit(num("4"))), true, vec![]),
        // 端点落在**重复键**上：`2 < k` 要把两条 2 都剔掉。
        (
            Some(lit(num("2"))),
            true,
            Some(lit(num("2"))),
            false,
            vec![],
        ),
    ];
    for (low, le, high, he, want) in cases {
        let plan = index_scan_bounds(seg0, low, le, high, he, false, None);
        let (got, _) = run_plan_only(&env, &plan).unwrap();
        let ids: Vec<String> = got
            .iter()
            .map(|r| match &r.values[0] {
                Value::Number(n) => n.to_string(),
                _ => unreachable!(),
            })
            .collect();
        let want: Vec<String> = want.iter().map(|v| v.to_string()).collect();
        assert_eq!(ids, want, "开闭组合 {plan:?}");
    }
}

/// **同一条活行只出一行**：改键列只**追加**索引项（不移动旧项）⇒ 同一条行可能
/// 有两条项（新旧键各一条）。范围覆盖新旧两个键时，不去重就会把同一条行吐两次
/// （实测抓到过：`UPDATE k=1→3` 之后 `BETWEEN 1 AND 3` 出两行）。
#[test]
fn a_row_reachable_through_two_entries_is_returned_once() {
    let io = mem_io();
    let mut env = build_env(io);
    let rows = self_join_rows(); // {1, 2, 2, 4}
    let table = create_table(&mut env, io, &rows);
    // 手工造"改键列"之后的索引：第 0 行（id=1）同时有键 1 与键 3 两条项，
    // 指向**同一个 RID**——正是 `after_update` 的产物形态。
    let mut entries: Vec<(Vec<u8>, RowId)> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let Value::Number(n) = &r.values[0] else {
                unreachable!()
            };
            (index_key_number(n), table.rowids[i])
        })
        .collect();
    entries.push((
        index_key_number(&Number::parse("3").unwrap()),
        table.rowids[0],
    ));
    let seg0 = build_index(&mut env, &entries);

    // 范围 [1, 3] 同时覆盖两条项 ⇒ 行 0 只应出一次。
    let plan = index_scan_plan(seg0, Some(lit(num("1"))), Some(lit(num("3"))), false, None);
    let (got, _) = run_plan_only(&env, &plan).unwrap();
    let ids: Vec<String> = got
        .iter()
        .map(|r| match &r.values[0] {
            Value::Number(n) => n.to_string(),
            _ => unreachable!(),
        })
        .collect();
    // 条目按 (键, ROWID) 序：1→r0、2→r1、2→r2、3→r0（重复）；去重后 r0 只出一次。
    // 不去重的话会是 `["1","2","2","1"]`（同一条行吐两次）。
    assert_eq!(
        ids,
        vec!["1", "2", "2"],
        "行 0 有两条项（键 1 与键 3）但只出一行：{ids:?}"
    );
}

/// **多点探测（`IN`）在一个算子里共用一份去重集**：同一行被两个点各带出一次时
/// 只出一次行；串成两支算子的话谁也没重复可判（实测抓到过 e2e 里的
/// `UPDATE k=1→3` 之后 `k IN (1,3)` 出两行）。
#[test]
fn multi_point_probe_deduplicates_rows_across_points() {
    let io = mem_io();
    let mut env = build_env(io);
    let rows = self_join_rows(); // {1, 2, 2, 4}
    let table = create_table(&mut env, io, &rows);
    // 手工造"改键列"后的索引：第 0 行同时有键 1 与键 3 两条项（指向同一 RID）。
    let mut entries: Vec<(Vec<u8>, RowId)> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let Value::Number(n) = &r.values[0] else {
                unreachable!()
            };
            (index_key_number(n), table.rowids[i])
        })
        .collect();
    entries.push((
        index_key_number(&Number::parse("3").unwrap()),
        table.rowids[0],
    ));
    let seg0 = build_index(&mut env, &entries);

    // 两个点 {1, 3} 都指向同一物理行 ⇒ 只出一行。
    let plan = PlanNode::IndexScan {
        file_id: DATA_FID,
        seg_page0: seg0,
        key_kind: ColKind::Number,
        shape: shape(),
        low: None,
        low_exclusive: false,
        high: None,
        high_exclusive: false,
        points: vec![lit(num("1")), lit(num("3"))],
        covered: false,
        limit: None,
        batch: None,
    };
    let (got, _) = run_plan_only(&env, &plan).unwrap();
    let ids: Vec<String> = got
        .iter()
        .map(|r| match &r.values[0] {
            Value::Number(n) => n.to_string(),
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(ids, vec!["1"], "两个点指向同一行 ⇒ 一行：{ids:?}");
    // 与重复键共存：点 {2, 2} 把两条**不同**的行各出一次（重复点不合并行）。
    let plan = PlanNode::IndexScan {
        file_id: DATA_FID,
        seg_page0: seg0,
        key_kind: ColKind::Number,
        shape: shape(),
        low: None,
        low_exclusive: false,
        high: None,
        high_exclusive: false,
        points: vec![lit(num("2")), lit(num("2"))],
        covered: false,
        limit: None,
        batch: None,
    };
    let (got, _) = run_plan_only(&env, &plan).unwrap();
    assert_eq!(got.len(), 2, "k=2 的两条行各一次（不同物理行不去重）");
}

/// **R4 回归钉**：索引键必须是**复合形态**（`key::encode(&[Some(载荷)])`）。
///
/// 审计（`doc/全仓审计_20261007.md` R4）记的正是这条：算子原来是拿**裸列编码**
/// （`Number::encode`）去比较树里的键——两者**前缀不同**，比较恒不相等 ⇒
/// 范围扫描**静默返回空集**。这里把"用错编码必然查不到"钉住：
/// 索引按裸编码建（老夹具形态）而查询按复合键 ⇒ **零行**，不是"能查到"。
#[test]
fn a_bare_column_encoding_is_not_a_key() {
    let io = mem_io();
    let mut env = build_env(io);
    let rows = self_join_rows();
    let table = create_table(&mut env, io, &rows);
    let bare: Vec<(Vec<u8>, RowId)> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let Value::Number(n) = &r.values[0] else {
                unreachable!()
            };
            (n.encode(), table.rowids[i])
        })
        .collect();
    let seg0 = build_index(&mut env, &bare);

    // 键值都对着（id=2 真有两行），但**编码形态不同** ⇒ 一条也匹配不上。
    let plan = index_scan_plan(seg0, Some(lit(num("2"))), Some(lit(num("2"))), false, None);
    let (got, _) = run_plan_only(&env, &plan).unwrap();
    assert!(
        got.is_empty(),
        "裸编码建索引 + 复合键查询 ⇒ 空集（R4 的形态）"
    );
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
                Value::Number(n) => index_key_number(n),
                _ => unreachable!(),
            };
            (id, table.rowids[i])
        })
        .collect();
    entries.push((
        index_key_number(&Number::parse("9").unwrap()),
        RowId::from_parts(DATA_FID, table.blocks[0], 999).unwrap(),
    ));
    let seg0 = build_index(&mut env, &entries);

    let plan = index_scan_plan(seg0, None, None, false, Some(2)); // 5 条 ⇒ 3 批
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
    let (_, seg0) = table_with_index(&mut env, io);

    let plan = index_scan_plan(seg0, Some(lit(num("2"))), Some(lit(num("4"))), true, None);
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
    let (blocks, seg0) = table_with_index(&mut env, io);

    // 自连接 INNER：外层全表、内层索引按外层 id 探测、qual = inner.id = outer.id。
    let plan = PlanNode::NestedLoop {
        outer: Box::new(PlanNode::SeqScan {
            source: 0,
            shape: shape(),
        }),
        inner: Box::new(index_scan_plan(
            seg0,
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
            bicdb_storage::cr::ReadView::new(env.snapshot),
            DATA_FID,
            blocks.clone(),
        )) as Box<dyn RowCursor>)
    };
    let envx = ExecEnv {
        pool: env.pool,
        chain: Some(&env.chain),
        spill: None,
        writer: None,
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
    let (blocks, seg0) = table_with_index(&mut env, io);

    // LEFT：内层按 outer.id + 1 探测（id=2 的两行与 id=4 无下一号 ⇒ 补 NULL）。
    let plan = PlanNode::NestedLoop {
        outer: Box::new(PlanNode::SeqScan {
            source: 0,
            shape: shape(),
        }),
        inner: Box::new(index_scan_plan(
            seg0,
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
            bicdb_storage::cr::ReadView::new(env.snapshot),
            DATA_FID,
            blocks.clone(),
        )) as Box<dyn RowCursor>)
    };
    let envx = ExecEnv {
        pool: env.pool,
        chain: Some(&env.chain),
        spill: None,
        writer: None,
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

#[test]
fn nested_loop_over_seq_scan_inner_rewinds_the_scan() {
    // **重扫回到起点**（实测踩过：`SeqScan` 曾用缺省 no-op rescan，
    // 内表每次重扫只读到"尾巴"——结果静默少行）。这里内表 = `SeqScan`
    // 全表，外层 4 行 ⇒ 内外同表自连接 4×4 对（朴素参考）。
    let io = mem_io();
    let mut env = build_env(io);
    let table = create_table(&mut env, io, &self_join_rows());
    let blocks = table.blocks.clone();

    let plan = PlanNode::NestedLoop {
        outer: Box::new(PlanNode::SeqScan {
            source: 0,
            shape: shape(),
        }),
        inner: Box::new(PlanNode::Filter {
            input: Box::new(PlanNode::SeqScan {
                source: 0,
                shape: shape(),
            }),
            predicate: Expr::Compare {
                op: CmpOp::Eq,
                left: Box::new(col(0)),
                right: Box::new(Expr::Param(0)),
            },
        }),
        inner_params: vec![col(0)],
        kind: JoinKind::Inner,
        qual: None,
        inner_width: 2,
    };
    let mut open = |_src| {
        Ok(Box::new(HeapScanner::new(
            env.pool,
            &env.chain,
            bicdb_storage::cr::ReadView::new(env.snapshot),
            DATA_FID,
            blocks.clone(),
        )) as Box<dyn RowCursor>)
    };
    let envx = ExecEnv {
        pool: env.pool,
        chain: Some(&env.chain),
        spill: None,
        writer: None,
    };
    let mut op = build(&plan, &envx, &mut open).unwrap();
    let mut cx = ExecContext::new(env.snapshot);
    let joined = collect(op.as_mut(), &mut cx).unwrap();

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
    assert_eq!(
        joined, expected,
        "内表重扫必须回到起点（4 行 × 同行配对——少一行都是没复位）"
    );
    assert_eq!(joined.len(), 6, "1×1 + 2×2 + 1×1 = 6 对");
}
