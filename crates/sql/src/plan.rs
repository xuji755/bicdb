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
//! 3. **行源装配**（表 → 段头块 + 数据页清单，交执行器开扫描）。
//!
//! 逻辑 IR 的引入随"第一条变换规则"落地（**触发条件 = REQ-SQL-007 的规则启用**）
//! ——现在引入只会多一层同构的树。

use bicdb_exec::{ColKind, Expr as PlanExpr, PlanNode, RowShape, SortKey, SourceId, Value};

use crate::bind::statement::col_kind;
use crate::bind::{BindError, BoundDdl, BoundInsert, BoundSelect, BoundStatement, CatalogView};

/// 一个**行源**（执行器按需开扫描：表段 + 数据页清单）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourcePlan {
    /// 行源标识（计划里的 `SourceId`）。
    pub id: SourceId,
    /// 表对象号（诊断/CR 用）。
    pub table_obj: u32,
    /// 数据对象号。
    pub dataobj: u32,
    /// 段头块（执行器开段用）。
    pub seg_block: u32,
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
}

/// 一条语句的物理计划。
#[derive(Debug, Clone, PartialEq)]
pub struct PhysicalPlan {
    /// 计划树。
    pub node: PlanNode,
    /// 输出形状（结果集列形态）。
    pub output: RowShape,
    /// 结果列名（协议层元数据）。
    pub output_names: Vec<String>,
    /// 行源清单（执行器开扫描用）。
    pub sources: Vec<SourcePlan>,
    /// 种类。
    pub kind: PlanKind,
}

/// **由绑定结果造物理计划**（`seg_block` 由调用方从目录取——避免本模块持目录）。
pub fn plan_statement(
    bound: &BoundStatement,
    seg_block_of: &mut dyn FnMut(u32) -> Result<u32, BindError>,
) -> Result<Option<PhysicalPlan>, BindError> {
    match bound {
        BoundStatement::Select(s) => Ok(Some(plan_select(s, seg_block_of)?)),
        BoundStatement::Insert(i) => Ok(Some(plan_insert(i, seg_block_of)?)),
        BoundStatement::Ddl(_)
        | BoundStatement::Transaction(_)
        | BoundStatement::CreateWorkspace { .. } => {
            Ok(None) // 不走算子通道
        }
    }
}

fn plan_select(
    s: &BoundSelect,
    seg_block_of: &mut dyn FnMut(u32) -> Result<u32, BindError>,
) -> Result<PhysicalPlan, BindError> {
    let seg_block = seg_block_of(s.table.obj)?;
    let source = SourcePlan {
        id: 0,
        table_obj: s.table.obj,
        dataobj: s.table.dataobj,
        seg_block,
        shape: s.table_shape.clone(),
    };
    let mut node = PlanNode::SeqScan {
        source: 0,
        shape: s.table_shape.clone(),
    };
    if let Some(pred) = &s.filter {
        node = PlanNode::Filter {
            input: Box::new(node),
            predicate: pred.clone(),
        };
    }
    node = PlanNode::Project {
        input: Box::new(node),
        exprs: s.projection.clone(),
    };
    let output = RowShape::new(s.columns.iter().map(|c| c.kind).collect());
    // 排序：**在投影之上**（键 = 输出列序号）——`ORDER BY 序/名` 的口径。
    let keys: Vec<SortKey> = s
        .sort
        .iter()
        .map(|(i, desc)| SortKey {
            expr: PlanExpr::Column(*i),
            desc: *desc,
        })
        .collect();
    if !keys.is_empty() {
        match s.limit {
            // `ORDER BY … LIMIT n` ⇒ 有界排序（输入仍全读——§4.1）。
            Some(n) => {
                node = PlanNode::TopN {
                    input: Box::new(node),
                    keys,
                    keep: n.saturating_add(s.offset),
                };
            }
            None => {
                node = PlanNode::Sort {
                    input: Box::new(node),
                    keys,
                };
            }
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
        output,
        output_names: s.columns.iter().map(|c| c.name.clone()).collect(),
        sources: vec![source],
        kind: PlanKind::Select,
    })
}

fn plan_insert(
    i: &BoundInsert,
    seg_block_of: &mut dyn FnMut(u32) -> Result<u32, BindError>,
) -> Result<PhysicalPlan, BindError> {
    let seg_block = seg_block_of(i.table.obj)?;
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
    let node = PlanNode::Insert {
        shape: i.table_shape.clone(),
        rows,
    };
    Ok(PhysicalPlan {
        node,
        output: RowShape::new(vec![]),
        output_names: vec![],
        sources: vec![SourcePlan {
            id: 0,
            table_obj: i.table.obj,
            dataobj: i.table.dataobj,
            seg_block,
            shape: i.table_shape.clone(),
        }],
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

/// 值形态 → 显示名（结果集列头）。
#[must_use]
pub fn kind_of(code: u32) -> ColKind {
    col_kind(code).unwrap_or(ColKind::Bytes)
}

/// 便利：目录视图的段头块（计划的 `seg_block_of` 闭包常用）。
pub fn seg_block_of<V: CatalogView>(view: &mut V, obj: u32) -> Result<u32, BindError> {
    view.segment_block(obj)
}
