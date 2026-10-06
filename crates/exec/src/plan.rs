//! **物理计划节点**（切片 1 子集）与构建（计划 → 算子树）。
//!
//! 计划**不可变、无值**（REQ-SQL-001：物理计划里只有编译期决策）；
//! 每次执行由本模块**构建一棵新算子树**（REQ-SQL-004：执行状态单次新造）。

use crate::error::ExecError;
use crate::expr::Expr;
use crate::nodes::{Filter, Limit, Project, SeqScan};
use crate::operator::{Operator, RowCursor};
use crate::value::RowShape;

/// 行源标识（切片 1：单表；编号由计划给出，执行期映射到存储服务的表）。
pub type SourceId = u32;

/// 物理计划节点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanNode {
    /// 顺序扫描一个行源。
    SeqScan {
        /// 行源标识。
        source: SourceId,
        /// 表的行形状（列定义）。
        shape: RowShape,
    },
    /// 过滤（谓词只放行 TRUE）。
    Filter {
        /// 输入子树。
        input: Box<PlanNode>,
        /// 谓词。
        predicate: Expr,
    },
    /// 投影。
    Project {
        /// 输入子树。
        input: Box<PlanNode>,
        /// 投影表达式（顺序 = 输出列序）。
        exprs: Vec<Expr>,
    },
    /// 限行（`LIMIT n OFFSET m`）。
    Limit {
        /// 输入子树。
        input: Box<PlanNode>,
        /// `LIMIT n`。
        limit: u64,
        /// `OFFSET m`。
        offset: u64,
    },
}

/// **构建算子树**：`open_cursor` 按行源标识开一个**新**行游标
/// （每次调用返回新游标——树里每个 `SeqScan` 各开一个）。
pub fn build<'a>(
    node: &PlanNode,
    open_cursor: &mut dyn FnMut(SourceId) -> Result<Box<dyn RowCursor + 'a>, ExecError>,
) -> Result<Box<dyn Operator + 'a>, ExecError> {
    Ok(match node {
        PlanNode::SeqScan { source, shape } => {
            Box::new(SeqScan::new(open_cursor(*source)?, shape.clone()))
        }
        PlanNode::Filter { input, predicate } => {
            Box::new(Filter::new(build(input, open_cursor)?, predicate.clone()))
        }
        PlanNode::Project { input, exprs } => {
            Box::new(Project::new(build(input, open_cursor)?, exprs.clone()))
        }
        PlanNode::Limit {
            input,
            limit,
            offset,
        } => Box::new(Limit::new(build(input, open_cursor)?, *limit, *offset)),
    })
}
