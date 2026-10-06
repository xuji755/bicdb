//! **直译执行器**（差分验收的参考模型；设计 §6）。
//!
//! REQ-SQL-005 的验收原文："参考模型 = 引擎内建的**直译执行器**
//! （逐行按语义求值、不做优化）"。它与算子执行器**共享 Binder 与求值内核、
//! 不共享执行**——同一份语义查询两路执行，结果必须逐行一致。
//!
//! 直译路径的特征：**没有算子树**（一个循环走完）、**不做任何下推**
//! （谓词逐行判、投影逐行算）、**不用索引**（行源给什么就拉什么）。

use crate::context::ExecContext;
use crate::error::ExecError;
use crate::expr::{self, Expr};
use crate::operator::RowCursor;
use crate::plan::{PlanNode, SourceId};
use crate::value::{decode_row, Row, RowShape};

/// **语义查询**（切片 1：单表 SELECT 子集）。
///
/// 这是"绑定后的语义树"的最小形态——两条执行路径共用它：
/// 直译执行器直接消费它；算子执行器先 [`SelectQuery::to_plan`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectQuery {
    /// 行源标识（单表）。
    pub source: SourceId,
    /// 表的行形状。
    pub shape: RowShape,
    /// 谓词（`WHERE`；`None` = 全放行）。
    pub predicate: Option<Expr>,
    /// 投影（顺序 = 输出列序）。
    pub projection: Vec<Expr>,
    /// `LIMIT n`。
    pub limit: Option<u64>,
    /// `OFFSET m`。
    pub offset: u64,
}

impl SelectQuery {
    /// 转物理计划（算子执行器的输入；**同一份语义**）。
    #[must_use]
    pub fn to_plan(&self) -> PlanNode {
        let mut node = PlanNode::SeqScan {
            source: self.source,
            shape: self.shape.clone(),
        };
        if let Some(pred) = &self.predicate {
            node = PlanNode::Filter {
                input: Box::new(node),
                predicate: pred.clone(),
            };
        }
        node = PlanNode::Project {
            input: Box::new(node),
            exprs: self.projection.clone(),
        };
        if let Some(limit) = self.limit {
            node = PlanNode::Limit {
                input: Box::new(node),
                limit,
                offset: self.offset,
            };
        }
        node
    }
}

/// **直译执行**：逐行按语义求值（不建哈希、不下推、不用索引）。
pub fn execute_direct(
    query: &SelectQuery,
    cursor: &mut dyn RowCursor,
    cx: &mut ExecContext<'_>,
) -> Result<Vec<Row>, ExecError> {
    let mut out = Vec::new();
    let mut skipped = 0u64;
    loop {
        cx.check()?;
        let Some((_rid, bytes)) = cursor.next_row()? else {
            break;
        };
        let row = decode_row(&bytes, &query.shape)?;
        if let Some(pred) = &query.predicate {
            if !expr::eval_where(pred, &row, cx.params())? {
                continue;
            }
        }
        if skipped < query.offset {
            skipped += 1;
            continue;
        }
        let mut values = Vec::with_capacity(query.projection.len());
        for e in &query.projection {
            values.push(expr::eval(e, &row, cx.params())?);
        }
        out.push(Row::new(values));
        if let Some(limit) = query.limit {
            if out.len() as u64 >= limit {
                break;
            }
        }
    }
    Ok(out)
}
