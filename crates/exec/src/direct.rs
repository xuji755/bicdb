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
use crate::value::{decode_row, Row, RowShape, Value};

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
    /// `ORDER BY`（空 = 无排序；语义与算子路径共用 [`crate::sort::compare_keys`]）。
    pub order_by: Vec<crate::sort::SortKey>,
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
        if !self.order_by.is_empty() {
            // **排序在投影之下**（`ORDER BY` 可引用未投影的列）；`ORDER BY` +
            // `LIMIT` ⇒ `TopN`（省排序内存，输入仍全读），无 `LIMIT` ⇒ 全排序。
            node = match self.limit {
                Some(limit) => PlanNode::TopN {
                    input: Box::new(node),
                    keys: self.order_by.clone(),
                    keep: limit + self.offset,
                },
                None => PlanNode::Sort {
                    input: Box::new(node),
                    keys: self.order_by.clone(),
                },
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

/// **直译执行**：逐行按语义求值（不建哈希、不下推、不用索引；
/// 排序 = 收全后稳定排序——**参考模型不做 Top-N 优化**，语义与 `TopN` 同规）。
pub fn execute_direct(
    query: &SelectQuery,
    cursor: &mut dyn RowCursor,
    cx: &mut ExecContext<'_>,
) -> Result<Vec<Row>, ExecError> {
    // ① 扫 + 过滤（收集——排序需要全量；无 ORDER BY 时才走下面的短路路径）。
    let mut rows: Vec<Row> = Vec::new();
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
        rows.push(row);
        // 无 ORDER BY：无需收全——本可以是流式，但参考模型从简（见模块文档）。
        if query.order_by.is_empty() {
            if let Some(limit) = query.limit {
                if rows.len() as u64 >= limit + query.offset {
                    break;
                }
            }
        }
    }
    // ② 排序（稳定；键在输入行上求值——`ORDER BY` 可引用未投影列）。
    if !query.order_by.is_empty() {
        let mut keyed: Vec<(Vec<Value>, Row)> = Vec::with_capacity(rows.len());
        for row in rows {
            let keys: Vec<Value> = query
                .order_by
                .iter()
                .map(|k| expr::eval(&k.expr, &row, cx.params()))
                .collect::<Result<_, _>>()?;
            keyed.push((keys, row));
        }
        let mut err: Option<ExecError> = None;
        keyed.sort_by(
            |a, b| match crate::sort::compare_keys(&query.order_by, &a.0, &b.0) {
                Ok(ord) => ord,
                Err(e) => {
                    if err.is_none() {
                        err = Some(e);
                    }
                    std::cmp::Ordering::Equal
                }
            },
        );
        if let Some(e) = err {
            return Err(e);
        }
        rows = keyed.into_iter().map(|(_, r)| r).collect();
    }
    // ③ 投影 + OFFSET/LIMIT。
    let mut out = Vec::new();
    for row in rows.into_iter().skip(query.offset as usize) {
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
        cx.check()?;
    }
    Ok(out)
}
