//! **物理计划**（`SQL前端设计` §6；**S5 首版：直映射**）。
//!
//! ```text
//! BoundStatement ──▶ bicdb_exec::PlanNode（+ 行源清单）
//! ```
//!
//! **首版的范围记档**：设计 §5 的**逻辑表示 + 白名单变换**在首版里是**空变换**
//! （REQ-SQL-007 的闭集里，没有一条规则被启用）⇒ 绑定产出的形状**即**物理形状，
//! 本模块只做三件事：
//!
//! 1. **形状补全**（`INSERT` 的行补齐到全列宽——缺列写 NULL）；
//! 2. **算子选择**（`ORDER BY`+`LIMIT` ⇒ `TopN`；否则 `Sort` + `Limit`）；
//! 3. **行源装配**（表 → 段头块 + 数据页清单，交执行器开扫描）；
//! 4. **访问路径选择（规则式）**：单表 + 等值谓词且该列是某索引的**唯一键列**
//!    ⇒ `IndexScan`（唯一索引优先），否则 `SeqScan`——规则照 Oracle RBO 的
//!    15 级排名（唯一键单行访问 4 > 单列索引 9 > 全表扫描 15）；**没有统计
//!    信息就不做代价式选择**（证据包 `doc/evidence/index-access-20261007/`）。
//!
//! **索引扫描只"缩小候选"**：谓词仍由 `Filter` 复核、`IndexScan` 恒回表——
//! 索引项**只插不删**（陈旧项）且不携带可见性，正确性不能挂在它身上。
//!
//! 逻辑 IR 的引入随"第一条变换规则"落地（**触发条件 = REQ-SQL-007 的规则启用**）
//! ——现在引入只会多一层同构的树。

use bicdb_exec::{CmpOp, ColKind, Expr as PlanExpr, PlanNode, RowShape, SortKey, SourceId, Value};

use crate::bind::statement::{col_kind, BoundSortKey, SortSpec};
use crate::bind::{
    BindError, BoundDdl, BoundDelete, BoundInsert, BoundSelect, BoundSetKind, BoundSetOp,
    BoundStatement, BoundTable, BoundUpdate, CatalogIndex,
};

/// 一个**行源**（执行器按需开扫描：表段 + 数据页清单）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourcePlan {
    /// 行源标识（计划里的 `SourceId`）。
    pub id: SourceId,
    /// 表对象号（诊断/CR 用；固定表为 0——它不在字典里）。
    pub table_obj: u32,
    /// 数据对象号。
    pub dataobj: u32,
    /// 段头块（执行器开段用；固定表为 0——**没有段**）。
    pub seg_block: u32,
    /// **固定表名**（`None` = 普通表）：行由引擎即时产生，开扫描走内存游标。
    pub fixed: Option<&'static str>,
    /// 行形状。
    pub shape: RowShape,
}

/// 计划种类（会话层据此选执行通道：读/写/DDL/事务）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanKind {
    /// 读（`SELECT`）。
    Select,
    /// 写（`INSERT`）。
    Insert,
    /// 写（`UPDATE`：源 = 带 ROWID 的扫描）。
    Update,
    /// 写（`DELETE`：源 = 带 ROWID 的扫描）。
    Delete,
}

/// 一条语句的物理计划。
#[derive(Debug, Clone, PartialEq)]
pub struct PhysicalPlan {
    /// 计划树。
    pub node: PlanNode,
    /// **`INSERT … SELECT` 的来源计划**（`None` = 普通 `INSERT … VALUES`）。
    ///
    /// 它的 `node` 里行源 id 已**平移**（目标表占 0，来源从 1 起），
    /// `sources` 合并在**外层计划**的 `sources` 里——执行期一次装好、
    /// 两边共用一份定位表。
    pub insert_source: Option<Box<PhysicalPlan>>,
    /// 输出形状（结果集列形态）。
    pub output: RowShape,
    /// 结果列名（协议层元数据）。
    pub output_names: Vec<String>,
    /// 行源清单（执行器开扫描用）。
    pub sources: Vec<SourcePlan>,
    /// 种类。
    pub kind: PlanKind,
}

/// **计划期问目录的三件事**（本模块**不持目录**——与 [`CatalogView`] 同一分工）。
///
/// 为什么是端口而不是直接拿 `Catalog`：计划层是纯函数式的（同输入同输出），
/// 目录访问只在装配时发生；端口也让用例能给假件（`crates/sql/tests/plan_access.rs`）。
pub trait PlanCatalog {
    /// 表的段头块（`seg$.block_id`）。
    fn segment_block(&mut self, obj: u32) -> Result<u32, BindError>;
    /// **索引段的（文件号, 段头块）**——索引扫描的落点（`seg$` 两列）。
    fn index_segment(&mut self, obj: u32) -> Result<(u16, u32), BindError>;
    /// 一张表的索引清单（含键列与状态；`Move` 后失效的不在内）。
    fn indexes_of(&mut self, obj: u32) -> Result<Vec<CatalogIndex>, BindError>;
}

/// **由绑定结果造物理计划**（目录访问走 [`PlanCatalog`]——避免本模块持目录）。
pub fn plan_statement(
    bound: &BoundStatement,
    seg_block_of: &mut dyn PlanCatalog,
) -> Result<Option<PhysicalPlan>, BindError> {
    match bound {
        BoundStatement::Select(s) => Ok(Some(plan_select(s, seg_block_of)?)),
        BoundStatement::Insert(i) => Ok(Some(plan_insert(i, seg_block_of)?)),
        BoundStatement::SetOp(o) => Ok(Some(plan_setop(o, seg_block_of)?)),
        BoundStatement::Update(u) => Ok(Some(plan_update(u, seg_block_of)?)),
        BoundStatement::Delete(d) => Ok(Some(plan_delete(d, seg_block_of)?)),
        BoundStatement::Ddl(_) | BoundStatement::Transaction(_) | BoundStatement::Dcl(_) => {
            Ok(None) // 不走算子通道
        }
    }
}

fn plan_select(
    s: &BoundSelect,
    seg_block_of: &mut dyn PlanCatalog,
) -> Result<PhysicalPlan, BindError> {
    // **行源**：一张表一个 `SourcePlan`（两表连接就是两个）。
    let mut sources: Vec<SourcePlan> = Vec::with_capacity(s.tables.len());
    for (i, t) in s.tables.iter().enumerate() {
        // **固定表没有段**（行即时产生）⇒ 不问目录要段头块。
        sources.push(SourcePlan {
            id: i as SourceId,
            table_obj: t.obj.obj,
            dataobj: t.obj.dataobj,
            seg_block: match t.fixed {
                Some(_) => 0,
                None => seg_block_of.segment_block(t.obj.obj)?,
            },
            fixed: t.fixed,
            shape: t.shape.clone(),
        });
    }
    // **访问路径**：单表 + 等值谓词命中索引 ⇒ `IndexScan`（见 [`try_index_scan`]）；
    // 两表以上不选（内表探测由 `NestedLoop` 自己带参数，另说），无 `FROM` 走 `SingleRow`。
    let mut node = match (s.tables.first(), s.join.is_none()) {
        (Some(t), true) => match try_index_scan(s, t, seg_block_of)? {
            Some(scan) => scan,
            None => PlanNode::SeqScan {
                source: 0,
                shape: t.shape.clone(),
            },
        },
        (Some(t), false) => PlanNode::SeqScan {
            source: 0,
            shape: t.shape.clone(),
        },
        // 无 `FROM`（`SELECT 1`）：单行零列的源。
        (None, _) => PlanNode::SingleRow,
    };
    // **两表连接**：`NestedLoop`——内表能命中索引就**参数化索引探测**（IndexNL，
    // 排名 2/3 的形态），命中不了才顺序重扫（排名 12 的排序-合并之下、15 之上；
    // 哈希连接随选路切片）。
    if let (Some(join), Some(right)) = (&s.join, s.tables.get(1)) {
        let left = s.tables.first().expect("两表连接必有两表");
        let mut inner_params = Vec::new();
        let inner = match try_index_inner(s, left, right, join.kind, seg_block_of)? {
            Some((scan, params)) => {
                inner_params = params;
                scan
            }
            None => PlanNode::SeqScan {
                source: 1,
                shape: right.shape.clone(),
            },
        };
        node = PlanNode::NestedLoop {
            outer: Box::new(node),
            inner: Box::new(inner),
            inner_params,
            kind: join.kind,
            qual: join.qual.clone(),
            inner_width: right.shape.len(),
        };
    }
    if let Some(pred) = &s.filter {
        node = PlanNode::Filter {
            input: Box::new(node),
            predicate: pred.clone(),
        };
    }
    // **聚合**：`HashAgg`（有 GROUP BY）或 `ScalarAgg`（无分组，空输入也出一行）。
    // 输出行 = `groups ++ aggs`；投影与 HAVING 都已按这个坐标改写（绑定期）。
    if !s.aggs.is_empty() || !s.groups.is_empty() {
        node = if s.groups.is_empty() {
            PlanNode::ScalarAgg {
                input: Box::new(node),
                aggs: s.aggs.clone(),
            }
        } else {
            PlanNode::HashAgg {
                input: Box::new(node),
                groups: s.groups.clone(),
                aggs: s.aggs.clone(),
            }
        };
        if let Some(h) = &s.having {
            node = PlanNode::Filter {
                input: Box::new(node),
                predicate: h.clone(),
            };
        }
    }
    // **排序放在哪**：键全是输出列 ⇒ 排在**投影之上**（最省事）；
    // 只要有一条**输入表达式**键（不投影的列 / 表达式）⇒ 必须排到**投影之下**，
    // 键换算成"投影的输入行"坐标（输出列键用它的投影表达式换）。
    let sort_below = s.sort.iter().any(|k| matches!(k.spec, SortSpec::Input(_)));
    if sort_below {
        let keys: Vec<SortKey> = s.sort.iter().map(|k| sort_key_input(k, s)).collect();
        if !keys.is_empty() {
            node = sort_node(node, keys, s);
        }
    }
    node = PlanNode::Project {
        input: Box::new(node),
        exprs: s.projection.clone(),
    };
    // **`DISTINCT`**：`Sort`（按输出全列）+ `Unique`（相邻去重）。
    // 与 `ORDER BY` 并存时：排序键 = `ORDER BY` 键 ++ 其余输出列（去重必须
    // 按**全列**相邻，否则只按排序键排，重复行不相邻 ⇒ 去不干净）。
    if s.distinct {
        let width = s.columns.len();
        // `DISTINCT` 时绑定期已拒绝"输入表达式键"（`Unique` 按输出全列去重）——
        // 这里只会看到 `Output` 键。
        let mut keys: Vec<SortKey> = s.sort.iter().map(sort_key_output).collect();
        for i in 0..width {
            if !s
                .sort
                .iter()
                .any(|k| matches!(&k.spec, SortSpec::Output(j) if *j == i))
            {
                keys.push(SortKey {
                    expr: PlanExpr::Column(i),
                    desc: false,
                });
            }
        }
        // **`Unique` 自己会前置一次 `Sort`**（算子契约：相邻去重必须输入有序）——
        // 所以把键给它，别再自己排一遍（自己排的那次会被它内部的升序排序覆盖，
        // 实测抓到的正是这条：`DISTINCT … ORDER BY … DESC` 出来是升序）。
        node = PlanNode::Unique {
            input: Box::new(node),
            keys: Some(keys),
            width,
        };
        // `DISTINCT` 已经把输出排好了 —— 后面的 `ORDER BY` 不必再排一次
        // （键是它的前缀）。`LIMIT` 仍然要。
        if let Some(n) = s.limit {
            node = PlanNode::Limit {
                input: Box::new(node),
                limit: n,
                offset: s.offset,
            };
        } else if s.offset > 0 {
            node = PlanNode::Limit {
                input: Box::new(node),
                limit: u64::MAX,
                offset: s.offset,
            };
        }
        let output = RowShape::new(s.columns.iter().map(|c| c.kind).collect());
        return Ok(PhysicalPlan {
            node,
            insert_source: None,
            output,
            output_names: s.columns.iter().map(|c| c.name.clone()).collect(),
            sources,
            kind: PlanKind::Select,
        });
    }
    let output = RowShape::new(s.columns.iter().map(|c| c.kind).collect());
    // 排序：**在投影之上**（键 = 输出列序号）——只在没有输入表达式键时走这条。
    if !sort_below {
        let keys: Vec<SortKey> = s.sort.iter().map(sort_key_output).collect();
        if !keys.is_empty() {
            node = sort_node(node, keys, s);
        }
    }
    if let Some(n) = s.limit {
        node = PlanNode::Limit {
            input: Box::new(node),
            limit: n,
            offset: s.offset,
        };
    } else if s.offset > 0 {
        node = PlanNode::Limit {
            input: Box::new(node),
            limit: u64::MAX,
            offset: s.offset,
        };
    }
    Ok(PhysicalPlan {
        node,
        insert_source: None,
        output,
        output_names: s.columns.iter().map(|c| c.name.clone()).collect(),
        sources,
        kind: PlanKind::Select,
    })
}

fn plan_insert(
    i: &BoundInsert,
    seg_block_of: &mut dyn PlanCatalog,
) -> Result<PhysicalPlan, BindError> {
    let seg_block = seg_block_of.segment_block(i.table.obj)?;
    let width = i.table_shape.len();
    // 行补齐到全列宽（缺列 = NULL）。
    let mut rows = Vec::with_capacity(i.rows.len());
    for row in &i.rows {
        let mut full: Vec<PlanExpr> = vec![PlanExpr::Literal(Value::Null); width];
        for (e, col) in row.iter().zip(&i.target_cols) {
            full[*col] = e.clone();
        }
        rows.push(full);
    }
    // **`INSERT … SELECT`**：来源计划带上（行源 id 平移 +1——目标表占 0）。
    let insert_source = match &i.source_select {
        None => None,
        Some(src) => {
            let sub = plan_statement(src, seg_block_of)?.ok_or_else(|| {
                BindError::Unsupported("INSERT … SELECT 的来源没有计划".to_owned())
            })?;
            let mut shifted = sub.clone();
            shifted.node = shift_sources(&sub.node, 1);
            for s in shifted.sources.iter_mut() {
                s.id += 1;
            }
            Some(Box::new(shifted))
        }
    };
    let mut sources = vec![SourcePlan {
        id: 0,
        table_obj: i.table.obj,
        dataobj: i.table.dataobj,
        seg_block,
        fixed: None, // 写目标恒是普通表（固定表没有写入口）
        shape: i.table_shape.clone(),
    }];
    if let Some(sub) = &insert_source {
        sources.extend(sub.sources.iter().cloned());
    }
    let node = PlanNode::Insert {
        shape: i.table_shape.clone(),
        rows,
    };
    Ok(PhysicalPlan {
        node,
        insert_source,
        output: RowShape::new(vec![]),
        output_names: vec![],
        sources,
        kind: PlanKind::Insert,
    })
}

/// DDL 的展示串（会话层回执用）。
#[must_use]
pub fn ddl_summary(d: &BoundDdl) -> String {
    match d {
        BoundDdl::CreateTable(t) => format!("CREATE TABLE {}（{} 列）", t.name, t.columns.len()),
        BoundDdl::CreateIndex(i) => format!(
            "CREATE {}INDEX {} ON {}（{} 列）",
            if i.unique { "UNIQUE " } else { "" },
            i.name,
            i.table,
            i.columns.len()
        ),
        BoundDdl::DropTable(n) => format!("DROP TABLE {n}"),
        BoundDdl::DropIndex(n) => format!("DROP INDEX {n}"),
    }
}

/// **排序键（输出列形态）**：键就是对输出行的列引用。
fn sort_key_output(k: &BoundSortKey) -> SortKey {
    match &k.spec {
        SortSpec::Output(i) => SortKey {
            expr: PlanExpr::Column(*i),
            desc: k.desc,
        },
        // `DISTINCT` 之外的两条路已在绑定期拒掉（`Plan` 层不该见到）。
        SortSpec::Input(_) => SortKey {
            expr: PlanExpr::Literal(Value::Null),
            desc: k.desc,
        },
    }
}

/// **排序键（输入行形态）**：`Output(i)` 用它的**投影表达式**换（投影是输入行上的
/// 表达式 ⇒ 同一套坐标），`Input(e)` 本来就是输入行坐标。
fn sort_key_input(k: &BoundSortKey, s: &BoundSelect) -> SortKey {
    let expr = match &k.spec {
        SortSpec::Input(e) => e.clone(),
        SortSpec::Output(i) => s
            .projection
            .get(*i)
            .cloned()
            .unwrap_or(PlanExpr::Literal(Value::Null)),
    };
    SortKey { expr, desc: k.desc }
}

/// **按 `LIMIT` 选 `TopN` 还是全 `Sort`**（`ORDER BY … LIMIT n` ⇒ 有界排序；
/// 输入仍全读——§4.1）。
fn sort_node(input: PlanNode, keys: Vec<SortKey>, s: &BoundSelect) -> PlanNode {
    match s.limit {
        Some(n) => PlanNode::TopN {
            input: Box::new(input),
            keys,
            keep: n.saturating_add(s.offset),
        },
        None => PlanNode::Sort {
            input: Box::new(input),
            keys,
        },
    }
}

/// 值形态 → 显示名（结果集列头）。
#[must_use]
pub fn kind_of(code: u32) -> ColKind {
    col_kind(code).unwrap_or(ColKind::Bytes)
}

/// **`UPDATE` 的物理计划**：`WithRowId` 之上是 `Update`。
///
/// **WHERE 在会话侧判**（`dcl_exec` 之外的 DML 口）：DML 的源是**物化**的
/// （先按快照把命中行收齐、再写——避免"边扫边改"在同一个游标里看见自己的改动），
/// 所以过滤在物化时就做了，计划里不再挂 `Filter`
/// （与 SELECT 的 `Filter` 语义完全相同：只放行 TRUE）。
fn plan_update(
    u: &BoundUpdate,
    seg_block_of: &mut dyn PlanCatalog,
) -> Result<PhysicalPlan, BindError> {
    let seg_block = seg_block_of.segment_block(u.table.obj)?;
    // SET 表达式对**原行**求值（表列坐标，无 ROWID 偏移）。
    let sets = u
        .sets
        .iter()
        .map(|(col, e)| (*col, e.clone()))
        .collect::<Vec<_>>();
    let node = PlanNode::Update {
        input: Box::new(PlanNode::WithRowId {
            source: 0,
            shape: u.table_shape.clone(),
        }),
        sets,
        shape: u.table_shape.clone(),
    };
    Ok(PhysicalPlan {
        node,
        insert_source: None,
        output: RowShape::new(vec![]),
        output_names: vec![],
        sources: vec![SourcePlan {
            id: 0,
            table_obj: u.table.obj,
            dataobj: u.table.dataobj,
            seg_block,
            fixed: None, // 写目标恒是普通表（固定表没有写入口）
            shape: u.table_shape.clone(),
        }],
        kind: PlanKind::Update,
    })
}

/// **`DELETE` 的物理计划**：`WithRowId` 之上是 `Delete`（WHERE 同样在物化时判）。
fn plan_delete(
    d: &BoundDelete,
    seg_block_of: &mut dyn PlanCatalog,
) -> Result<PhysicalPlan, BindError> {
    let seg_block = seg_block_of.segment_block(d.table.obj)?;
    let node = PlanNode::Delete {
        input: Box::new(PlanNode::WithRowId {
            source: 0,
            shape: d.table_shape.clone(),
        }),
        shape: d.table_shape.clone(),
    };
    Ok(PhysicalPlan {
        node,
        insert_source: None,
        output: RowShape::new(vec![]),
        output_names: vec![],
        sources: vec![SourcePlan {
            id: 0,
            table_obj: d.table.obj,
            dataobj: d.table.dataobj,
            seg_block,
            fixed: None, // 写目标恒是普通表（固定表没有写入口）
            shape: d.table_shape.clone(),
        }],
        kind: PlanKind::Delete,
    })
}

/// **集合运算的物理计划**。
///
/// - `UNION ALL` ⇒ `Append`；`UNION` ⇒ `Append` + `Unique`（全列相邻去重）；
/// - `INTERSECT`/`EXCEPT` ⇒ exec 的 `SetOp`（两侧自动按全列排序、归并计数）。
fn plan_setop(
    o: &BoundSetOp,
    seg_block_of: &mut dyn PlanCatalog,
) -> Result<PhysicalPlan, BindError> {
    let left = plan_statement(&o.left, seg_block_of)?
        .ok_or_else(|| BindError::Unsupported("集合运算左侧没有计划".to_owned()))?;
    let right = plan_statement(&o.right, seg_block_of)?
        .ok_or_else(|| BindError::Unsupported("集合运算右侧没有计划".to_owned()))?;
    let mut sources = left.sources.clone();
    let right_base = sources.len() as u32;
    // 右侧计划里的行源 id 要**平移**（两棵子树的 id 都从 0 起）。
    let right_node = shift_sources(&right.node, right_base);
    for s in &right.sources {
        sources.push(SourcePlan {
            id: s.id + right_base,
            ..s.clone()
        });
    }
    let node = match o.kind {
        BoundSetKind::Union => {
            let append = PlanNode::Append {
                inputs: vec![left.node.clone(), right_node],
            };
            if o.all {
                append
            } else {
                let width = o.width;
                let keys = crate::plan::all_columns_sort_keys(width);
                PlanNode::Unique {
                    input: Box::new(append),
                    keys: Some(keys),
                    width,
                }
            }
        }
        BoundSetKind::Intersect | BoundSetKind::Except => PlanNode::SetOp {
            left: Box::new(left.node.clone()),
            right: Box::new(right_node),
            kind: if o.kind == BoundSetKind::Intersect {
                bicdb_exec::SetOpKind::Intersect
            } else {
                bicdb_exec::SetOpKind::Except
            },
            all: o.all,
            width: o.width,
        },
    };
    Ok(PhysicalPlan {
        node,
        insert_source: None,
        output: left.output.clone(),
        output_names: left.output_names.clone(),
        sources,
        kind: PlanKind::Select,
    })
}

/// **连接的索引内表（IndexNL）**：内表的连接列是某**单列索引**的键列 ⇒ 内表用
/// **参数化索引探测**取代顺序重扫（`NestedLoop` 的 `inner_params` 本就是为它留的：
/// "外层取一行 → 对参数求值 → 装进参数表 → rescan 内表"）。
///
/// # 返回
/// `Some((索引扫描节点, 内表参数))`；没有可用的等值/索引 ⇒ `None`（顺序重扫）。
///
/// # 规则（照 Oracle RBO 的 15 级排名；证据包 `doc/evidence/index-access-20261007/`）
///
/// 内表探测 = "通过唯一/主键的单行访问"（排名 2/3）×外层行数；顺序重扫 =
/// 每外层行一次全表扫。**唯一索引优先**（同 [`try_index_scan`]）。
///
/// # 谓词取哪一条
///
/// - **`ON`**（`join.qual`）里的 `内列 = 外列`：内连接与左外连接都可用
///   （左外的语义不受影响：`matched` 只在"满足 ON 的候选"里算，而 narrowing
///   用的那条等值式本来就是 ON 的合取项之一）；
/// - **`WHERE`**（`s.filter`）里的 `内列 = 外列`：**只给内连接用**。
///   左外 + WHERE 的组合虽然也安全（被补 NULL 的那一行必然被这条 WHERE 否掉：
///   `NULL = x` 是 UNKNOWN），但那要"补 NULL 行必被同一合取项否掉"这条论证撑着，
///   第一版不摊这个（记档）。
///
/// # 正确性
///
/// 与 [`try_index_scan`] 同源：索引**只缩小候选**、连接条件**仍逐对复核**
/// （`qual` 挂在 `NestedLoop` 上照旧求值），陈旧索引项由行否掉。
fn try_index_inner(
    s: &BoundSelect,
    left: &BoundTable,
    right: &BoundTable,
    kind: bicdb_exec::JoinKind,
    cat: &mut dyn PlanCatalog,
) -> Result<Option<(PlanNode, Vec<PlanExpr>)>, BindError> {
    // 内表是固定表 ⇒ 没有索引可探测（外层是固定表不影响探测内表）。
    if right.fixed.is_some() {
        return Ok(None);
    }
    let w_left = left.shape.len();
    let w_right = right.shape.len();
    // 组合行的列号约定（与 Binder 同）：**外层列在前、内层列在后**。
    let in_outer = |i: usize| i < w_left;
    let in_inner = |i: usize| i >= w_left && i < w_left + w_right;
    // 候选等值式：ON 恒可；WHERE 只给内连接。
    let mut pairs: Vec<(usize, usize)> = s
        .join
        .as_ref()
        .and_then(|j| j.qual.as_ref())
        .map(column_equalities)
        .unwrap_or_default();
    if kind == bicdb_exec::JoinKind::Inner {
        if let Some(f) = &s.filter {
            pairs.extend(column_equalities(f));
        }
    }
    // 表上的索引（单列、有效）——一趟取一次。
    let indexes: Vec<CatalogIndex> = cat
        .indexes_of(right.obj.obj)?
        .into_iter()
        .filter(|i| i.status != 0 && i.cols.len() == 1)
        .collect();
    if indexes.is_empty() {
        return Ok(None);
    }
    let mut best: Option<(&CatalogIndex, usize, usize)> = None; // (索引, 内列, 外列)
    let mut rank = u32::MAX;
    for (a, b) in pairs {
        // 一侧在外、一侧在内（两种书写顺序都收）。
        let (outer_col, inner_col) = if in_outer(a) && in_inner(b) {
            (a, b - w_left)
        } else if in_outer(b) && in_inner(a) {
            (b, a - w_left)
        } else {
            continue;
        };
        for idx in &indexes {
            if idx.cols[0] as usize != inner_col + 1 {
                continue;
            }
            let this = u32::from(!idx.unique) << 31 | (idx.obj & 0x7fff_ffff);
            if this < rank {
                rank = this;
                best = Some((idx, inner_col, outer_col));
            }
        }
    }
    let Some((idx, inner_col, outer_col)) = best else {
        return Ok(None);
    };
    let Some(key_kind) = right.shape.cols.get(inner_col).copied() else {
        return Ok(None);
    };
    let (file_id, seg_page0) = cat.index_segment(idx.obj)?;
    // 内表探测：**界 = 内表参数**（`NestedLoop` 每取一个外层行装一次）。
    Ok(Some((
        PlanNode::IndexScan {
            file_id,
            seg_page0,
            key_kind,
            shape: right.shape.clone(),
            low: Some(PlanExpr::Param(0)),
            low_exclusive: false,
            high: Some(PlanExpr::Param(0)),
            high_exclusive: false,
            points: Vec::new(),
            covered: false,
            limit: None,
            batch: None,
        },
        vec![PlanExpr::Column(outer_col)],
    )))
}

/// 从一个谓词里收齐"**两列相等**"（合取式里的 `colA = colB`）。
fn column_equalities(pred: &PlanExpr) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    collect_column_equalities(pred, &mut out);
    out
}

fn collect_column_equalities(pred: &PlanExpr, out: &mut Vec<(usize, usize)>) {
    match pred {
        PlanExpr::And(list) => {
            for e in list {
                collect_column_equalities(e, out);
            }
        }
        PlanExpr::Compare {
            op: CmpOp::Eq,
            left,
            right,
        } => {
            if let (PlanExpr::Column(a), PlanExpr::Column(b)) = (left.as_ref(), right.as_ref()) {
                out.push((*a, *b));
            }
        }
        _ => {}
    }
}

/// **等值谓词 ⇒ 索引扫描**（规则式选路；证据包 `doc/evidence/index-access-20261007/`）。
///
/// # 规则（照 Oracle RBO 的 15 级排名）
///
/// | 路径 | RBO 排名 | 本切片的判据 |
/// | --- | --- | --- |
/// | 唯一键单行访问 | 4 | 唯一索引，键列被**等值**钉死 |
/// | 单列索引 | 9 | 普通索引，等值（取到的是一组同键行） |
/// | 索引列上的**有界范围搜索** | 10 | 单列索引，键列有**下界与上界各一条**（`BETWEEN` / `>= AND <=` / `> AND <`） |
/// | **`IN (值表)`**（N 次单行访问） | 4/9 的重复形态 | 单列索引 + `col IN (<字面量>…)` ⇒ `Append` 串起 N 次点查 |
/// | 全表扫描 | 15 | 以上都不成立 |
///
/// **无界范围（排名 11）不接**：索引条目在 `open` 时**一次收齐**（算子既定的切片
/// 边界），只给一侧的界会把整棵树搬进内存——`k > 5` 这种先让全表扫描，随
/// "流式游标"切片再开。
///
/// **没有统计信息就不做代价式选择**：本仓没有 `ANALYZE`/直方图，硬上"代价"
/// 只能拍常数——KB 里 CBO 对绑定变量的选择性"只能使用默认值（5% 或 25%）"
/// 正是这个坑。规则式选择是成熟做法（Oracle RBO 走了十几年），代价式随
/// 统计信息切片。
///
/// # 正确性（为什么可以"只管缩小候选"）
///
/// `IndexScan` **恒回表**、谓词仍由上层 `Filter` 复核：索引项只插不删
/// （陈旧项指向别的键）且不携带可见性——PG 同款（"索引上不保存这些可见性
/// 信息"）。所以哪怕这条规则选错（比如小表上其实全表扫描更划算），**结果不变**，
/// 只是可能慢一点。
fn try_index_scan(
    s: &BoundSelect,
    t: &BoundTable,
    cat: &mut dyn PlanCatalog,
) -> Result<Option<PlanNode>, BindError> {
    // 要有一条（或几条）区间合取式；`AND` 已由绑定层折叠成 `Expr::And(…)`。
    // 固定表**没有索引**（行是即时产生的，没有段）。
    if t.fixed.is_some() {
        return Ok(None);
    }
    let Some(filter) = &s.filter else {
        return Ok(None);
    };
    let bounds = column_bounds(filter);
    // 表上的索引清单（`Move` 后失效的不在内）——**一趟取一次**，逐条等值式去配。
    let indexes: Vec<CatalogIndex> = cat
        .indexes_of(t.obj.obj)?
        .into_iter()
        .filter(|i| i.status != 0 && i.cols.len() == 1)
        .collect();
    // **所有的区间式都试**，然后挑最好的（唯一索引优先，再按对象号定序）——
    // 只试第一条会漏：`WHERE tag = 'x' AND id = 5` 里 `tag` 可能没索引，
    // 而 `id` 有（实测抓到的正是这条）。
    let mut best: Option<(&CatalogIndex, ColKind, &ColumnBound<'_>)> = None;
    let mut rank = u32::MAX;
    for bound in &bounds {
        // **两端都要有界**（等值是两端同值的退化形态）；无界范围见函数文档。
        let (Some((low, _)), Some((high, _))) = (bound.low, bound.high) else {
            continue;
        };
        // 两端的形态都必须**与列的形态一致**（键编码按形态走；绑定层也拒形态
        // 不符，这里再挡一道——拿不准就不走索引，退全表扫描）。
        let (Some(kind), Some(hk)) = (
            value_kind(low, s, t, bound.col),
            value_kind(high, s, t, bound.col),
        ) else {
            continue;
        };
        if kind != hk {
            continue;
        }
        for idx in &indexes {
            if idx.cols[0] as usize != bound.col + 1 {
                continue;
            }
            // 先按"唯一位"再按对象号排名（RBO：唯一键单行访问 4 < 单列索引 9；
            // 等值/有界范围共用同一批候选，界的形式只影响开闭）。
            let this = u32::from(!idx.unique) << 31 | (idx.obj & 0x7fff_ffff);
            if this < rank {
                rank = this;
                best = Some((idx, kind, bound));
            }
        }
    }
    let Some((idx, kind, bound)) = best else {
        // 没有等值/范围可用 ⇒ 试 **`IN (值表)`**（等值的 N 次退化形态）。
        return try_in_list_scan(s, t, filter, &indexes, cat);
    };
    let (low, low_x) = bound.low.expect("上面已保证两端都有界");
    let (high, high_x) = bound.high.expect("上面已保证两端都有界");
    let (file_id, seg_page0) = cat.index_segment(idx.obj)?;
    // **等值时 `low == high`**（点查/同键集合）；有界范围时两端各是各的。
    // **不给 `limit`**：某个键的第一条可能是陈旧项（回表落空），活行排在后面
    // ——提前限行会静默漏行。
    Ok(Some(PlanNode::IndexScan {
        file_id,
        seg_page0,
        key_kind: kind,
        shape: t.shape.clone(),
        low: Some(low.clone()),
        low_exclusive: low_x,
        high: Some(high.clone()),
        high_exclusive: high_x,
        points: Vec::new(),
        covered: false,
        limit: None,
        batch: None,
    }))
}

/// **`IN (值表)` ⇒ 逐点一次点查**（RBO 排名 4/9 的重复形态）。
///
/// **一个算子带多点**（`IndexScan::points`）：`IN` 是**集合成员**语义，同一行只许
/// 出一次；而"同一条活行经两条索引项"（改键列留下的旧项 + 新项）要靠**按物理行
/// 去重**——去重集按算子一份，所以点必须落在**同一个算子**里（实测抓到过：
/// 串成 `Append` 之后 `k IN (1,3)` 在 `UPDATE k=1→3` 之后把那行出了两次）。
/// 元素可以是字面量或**参数**（运行期求值；求值为 NULL 的点跳过——`= NULL`
/// 恒不成立），点重复也无妨（按行去重兜住）。
fn try_in_list_scan(
    s: &BoundSelect,
    t: &BoundTable,
    pred: &PlanExpr,
    indexes: &[CatalogIndex],
    cat: &mut dyn PlanCatalog,
) -> Result<Option<PlanNode>, BindError> {
    if indexes.is_empty() {
        return Ok(None);
    }
    for (col, list) in in_list_columns(pred) {
        let Some(col_kind) = t.shape.cols.get(col).copied() else {
            continue;
        };
        // 逐元素核形态（字面量查值、参数查**绑定期的定型**）——拿不准就整条不用。
        let mut values: Vec<PlanExpr> = Vec::with_capacity(list.len());
        let mut usable = true;
        for e in list {
            // 值侧不得含列引用（列对列不是点）。**字面量 NULL 丢掉**（永不匹配）。
            if has_column(e) || matches!(e, PlanExpr::Literal(Value::Null)) {
                if has_column(e) {
                    usable = false;
                    break;
                }
                continue;
            }
            if value_kind(e, s, t, col) != Some(col_kind) {
                usable = false;
                break;
            }
            // **字面量按值去重**（省一次点查；参数运行期才知道，交给按行去重兜）。
            if let PlanExpr::Literal(v) = e {
                if values
                    .iter()
                    .any(|x| matches!(x, PlanExpr::Literal(w) if w == v))
                {
                    continue;
                }
            }
            values.push(e.clone());
        }
        if !usable || values.is_empty() {
            continue;
        }
        // 该列是某个单列索引的键列；唯一索引优先（同 [`try_index_scan`] 的排名）。
        let mut candidates: Vec<&CatalogIndex> = indexes
            .iter()
            .filter(|i| i.cols[0] as usize == col + 1)
            .collect();
        candidates.sort_by_key(|i| (!i.unique, i.obj));
        let Some(idx) = candidates.first() else {
            continue;
        };
        let (file_id, seg_page0) = cat.index_segment(idx.obj)?;
        // **一个算子带多点**（不是串 N 个算子）：去重集按算子一份，串起来的话
        // 一条活行会从两个分支各出一次（见 `IndexScan::points` 的文档）。
        return Ok(Some(PlanNode::IndexScan {
            file_id,
            seg_page0,
            key_kind: col_kind,
            shape: t.shape.clone(),
            low: None,
            low_exclusive: false,
            high: None,
            high_exclusive: false,
            points: values,
            covered: false,
            limit: None,
            batch: None,
        }));
    }
    Ok(None)
}

/// **合取式里的 `col IN (<值表>)`**（左侧是列、列表原样带出——由调用方判可用）。
fn in_list_columns(pred: &PlanExpr) -> Vec<(usize, &Vec<PlanExpr>)> {
    let mut out = Vec::new();
    collect_in_lists(pred, &mut out);
    out
}

fn collect_in_lists<'e>(pred: &'e PlanExpr, out: &mut Vec<(usize, &'e Vec<PlanExpr>)>) {
    match pred {
        PlanExpr::And(list) => {
            for e in list {
                collect_in_lists(e, out);
            }
        }
        PlanExpr::InList { expr, list } => {
            if let PlanExpr::Column(i) = expr.as_ref() {
                out.push((*i, list));
            }
        }
        _ => {}
    }
}

/// **一个列上的一条区间约束**（等值是"两端同值、都闭"的退化形态）。
struct ColumnBound<'e> {
    /// 列号（行内 0 基）。
    col: usize,
    /// 下界 + 是否排除（`(`界表达式, 开?)`）。
    low: Option<(&'e PlanExpr, bool)>,
    /// 上界 + 是否排除。
    high: Option<(&'e PlanExpr, bool)>,
}

/// 从一个谓词里收齐各列的区间约束（合取式；`AND` 已由绑定层折叠）。
///
/// 同一列出现多条时**取先遇到的那条**（不合并）：界再紧一点也只是让 `Filter`
/// 多否几行，**不会漏行**（用的界本身就是合取项之一，结果集必然满足它）。
fn column_bounds(pred: &PlanExpr) -> Vec<ColumnBound<'_>> {
    let mut out: Vec<ColumnBound<'_>> = Vec::new();
    collect_bounds(pred, &mut out);
    out
}

fn collect_bounds<'e>(pred: &'e PlanExpr, out: &mut Vec<ColumnBound<'e>>) {
    let mut note =
        |col: usize, low: Option<(&'e PlanExpr, bool)>, high: Option<(&'e PlanExpr, bool)>| {
            if let Some(b) = out.iter_mut().find(|b| b.col == col) {
                if b.low.is_none() {
                    b.low = low;
                }
                if b.high.is_none() {
                    b.high = high;
                }
            } else {
                out.push(ColumnBound { col, low, high });
            }
        };
    match pred {
        PlanExpr::And(list) => {
            for e in list {
                collect_bounds(e, out);
            }
        }
        // `col OP 值`（`>`/`>=`/`<`/`<=`/`=`；值侧不得含列引用）。
        PlanExpr::Compare { op, left, right } => {
            let (col, e, op) = match (left.as_ref(), right.as_ref()) {
                (PlanExpr::Column(i), other) if !has_column(other) => (*i, other, *op),
                (other, PlanExpr::Column(i)) if !has_column(other) => {
                    // **翻向**：`值 < col` 等价于 `col > 值`。
                    (*i, other, flip(*op))
                }
                _ => return,
            };
            match op {
                CmpOp::Eq => note(col, Some((e, false)), Some((e, false))),
                CmpOp::Gt => note(col, Some((e, true)), None),
                CmpOp::Ge => note(col, Some((e, false)), None),
                CmpOp::Lt => note(col, None, Some((e, true))),
                CmpOp::Le => note(col, None, Some((e, false))),
                // `<>` 是**两个**区间，不是一条——本切片不接（`Filter` 照旧）。
                CmpOp::Ne => {}
            }
        }
        // `col BETWEEN a AND b`（两端都闭）。
        PlanExpr::Between { expr, low, high } => {
            if let PlanExpr::Column(i) = expr.as_ref() {
                if !has_column(low) && !has_column(high) {
                    note(*i, Some((low, false)), Some((high, false)));
                }
            }
        }
        _ => {}
    }
}

/// 比较算子**翻向**（把 `值 OP col` 写成 `col OP' 值`）。
fn flip(op: CmpOp) -> CmpOp {
    match op {
        CmpOp::Lt => CmpOp::Gt,
        CmpOp::Le => CmpOp::Ge,
        CmpOp::Gt => CmpOp::Lt,
        CmpOp::Ge => CmpOp::Le,
        other => other, // `Eq`/`Ne` 对称
    }
}

/// 表达式里有没有列引用（有 ⇒ 不是"常量"，不能当键值）。
fn has_column(e: &PlanExpr) -> bool {
    match e {
        PlanExpr::Column(_) => true,
        PlanExpr::Literal(_) | PlanExpr::Param(_) => false,
        PlanExpr::Compare { left, right, .. } => has_column(left) || has_column(right),
        PlanExpr::Arith { left, right, .. } => has_column(left) || has_column(right),
        PlanExpr::Neg(x) => has_column(x),
        PlanExpr::Cast { expr, .. } => has_column(expr),
        PlanExpr::Case { whens, else_ } => {
            whens.iter().any(|(w, t)| has_column(w) || has_column(t))
                || else_.as_ref().is_some_and(|e| has_column(e))
        }
        PlanExpr::InList { expr, list } => has_column(expr) || list.iter().any(has_column),
        PlanExpr::Between { expr, low, high } => {
            has_column(expr) || has_column(low) || has_column(high)
        }
        PlanExpr::Coalesce(list) => list.iter().any(has_column),
        PlanExpr::NullIf { left, right } => has_column(left) || has_column(right),
        PlanExpr::IsNull { expr, .. } => has_column(expr),
        PlanExpr::Not(x) => has_column(x),
        PlanExpr::And(list) | PlanExpr::Or(list) => list.iter().any(has_column),
    }
}

/// 键值的形态（`None` = 拿不准 ⇒ **不走索引**，退全表扫描）。
///
/// 与列形态**必须一致**：索引键按列的形态编码（数值保序编码 / 字节串原字节 /
/// 布尔 1 字节），形态不符的比较恒不相等（审计 R4 的教训）。
fn value_kind(value: &PlanExpr, s: &BoundSelect, t: &BoundTable, col: usize) -> Option<ColKind> {
    let col_kind = *t.shape.cols.get(col)?;
    let value_kind = match value {
        PlanExpr::Literal(v) => value_kind_of(v),
        // 参数：形态由绑定期的**参数定型**给出（`s.params` 按位置给形态）。
        PlanExpr::Param(i) => s.params.list().get(*i).map(|(_, k)| *k),
        _ => None,
    }?;
    (value_kind == col_kind).then_some(col_kind)
}

/// 字面值的形态（`NULL` ⇒ `None`：`= NULL` 恒不成立，交给 `Filter` 判）。
fn value_kind_of(v: &Value) -> Option<ColKind> {
    match v {
        Value::Null => None,
        Value::Number(_) => Some(ColKind::Number),
        Value::Bool(_) => Some(ColKind::Bool),
        Value::Bytes(_) => Some(ColKind::Bytes),
    }
}

/// 把一棵子树里的行源 id 平移（集合运算的两侧 / `INSERT … SELECT` 的来源各自从 0 编号）。
///
/// **必须穷尽所有节点**：漏一类（比如聚合）就等于"里层的 `SeqScan` 还指着旧 id"
/// ——实测抓到的正是这条（`INSERT INTO t2 SELECT COUNT(*) FROM big` 数出来的是
/// **目标表**的行数）。
fn shift_sources(n: &PlanNode, base: u32) -> PlanNode {
    let shift = |x: &PlanNode| Box::new(shift_sources(x, base));
    match n {
        PlanNode::SeqScan { source, shape } => PlanNode::SeqScan {
            source: source + base,
            shape: shape.clone(),
        },
        PlanNode::WithRowId { source, shape } => PlanNode::WithRowId {
            source: source + base,
            shape: shape.clone(),
        },
        PlanNode::Filter { input, predicate } => PlanNode::Filter {
            input: shift(input),
            predicate: predicate.clone(),
        },
        PlanNode::Project { input, exprs } => PlanNode::Project {
            input: shift(input),
            exprs: exprs.clone(),
        },
        PlanNode::Limit {
            input,
            limit,
            offset,
        } => PlanNode::Limit {
            input: shift(input),
            limit: *limit,
            offset: *offset,
        },
        PlanNode::Sort { input, keys } => PlanNode::Sort {
            input: shift(input),
            keys: keys.clone(),
        },
        PlanNode::TopN { input, keys, keep } => PlanNode::TopN {
            input: shift(input),
            keys: keys.clone(),
            keep: *keep,
        },
        PlanNode::IndexScan {
            file_id,
            seg_page0,
            key_kind,
            shape,
            low,
            low_exclusive,
            high,
            high_exclusive,
            points,
            covered,
            limit,
            batch,
        } => PlanNode::IndexScan {
            file_id: *file_id,
            seg_page0: *seg_page0,
            key_kind: *key_kind,
            shape: shape.clone(),
            low: low.clone(),
            low_exclusive: *low_exclusive,
            high: high.clone(),
            high_exclusive: *high_exclusive,
            points: points.clone(),
            covered: *covered,
            limit: *limit,
            batch: *batch,
        },
        PlanNode::Unique { input, keys, width } => PlanNode::Unique {
            input: shift(input),
            keys: keys.clone(),
            width: *width,
        },
        PlanNode::ScalarAgg { input, aggs } => PlanNode::ScalarAgg {
            input: shift(input),
            aggs: aggs.clone(),
        },
        PlanNode::HashAgg {
            input,
            groups,
            aggs,
        } => PlanNode::HashAgg {
            input: shift(input),
            groups: groups.clone(),
            aggs: aggs.clone(),
        },
        PlanNode::SortedAgg {
            input,
            groups,
            aggs,
        } => PlanNode::SortedAgg {
            input: shift(input),
            groups: groups.clone(),
            aggs: aggs.clone(),
        },
        PlanNode::HashJoin {
            build,
            probe,
            build_keys,
            probe_keys,
            kind,
            qual,
            build_width,
        } => PlanNode::HashJoin {
            build: shift(build),
            probe: shift(probe),
            build_keys: build_keys.clone(),
            probe_keys: probe_keys.clone(),
            kind: *kind,
            qual: qual.clone(),
            build_width: *build_width,
        },
        PlanNode::NestedLoop {
            outer,
            inner,
            inner_params,
            kind,
            qual,
            inner_width,
        } => PlanNode::NestedLoop {
            outer: shift(outer),
            inner: shift(inner),
            inner_params: inner_params.clone(),
            kind: *kind,
            qual: qual.clone(),
            inner_width: *inner_width,
        },
        PlanNode::Append { inputs } => PlanNode::Append {
            inputs: inputs.iter().map(|x| shift_sources(x, base)).collect(),
        },
        PlanNode::SetOp {
            left,
            right,
            kind,
            all,
            width,
        } => PlanNode::SetOp {
            left: shift(left),
            right: shift(right),
            kind: *kind,
            all: *all,
            width: *width,
        },
        // **无行源的节点**：原样（`SingleRow` 没有源；DML 节点不在只读子树里）。
        PlanNode::SingleRow
        | PlanNode::Insert { .. }
        | PlanNode::Update { .. }
        | PlanNode::Delete { .. } => n.clone(),
    }
}

/// 全列升序排序键（`DISTINCT` / `UNION` 去重用）。
fn all_columns_sort_keys(width: usize) -> Vec<SortKey> {
    (0..width)
        .map(|i| SortKey {
            expr: PlanExpr::Column(i),
            desc: false,
        })
        .collect()
}
