//! # bicdb-exec — 执行算子
//!
//! 设计依据：[`doc/执行算子设计_v0.1.md`]（本库的算子闭集与取法）。
//! 对应阶段：**P5**（切片 1）。
//!
//! **模型**：PG 拉取（火山）+ Oracle 批量化技巧——对外行式
//! （`open`/`next`/`rescan`/`close`，[`operator::Operator`]），
//! 扫描与回表内部批量（区读 + 批量回表 + 每块一次 CR，落在 `bicdb-storage`）。
//!
//! **本切片（v0.1）**：
//! - [`context`]：执行上下文（语句快照、参数、deadline/取消、统计槽）；
//! - [`operator`]：算子契约 + 行游标口（`RowCursor`，存储服务的拉取面）；
//! - [`value`] / [`expr`]：值域与三值逻辑（切片 1 子集）；
//! - [`nodes`]：`SeqScan`/`Filter`/`Project`/`Limit`；
//! - [`plan`]：物理计划节点（切片 1 子集）与构建；
//! - [`direct`]：**直译执行器**（REQ-SQL-005 的差分参考模型）。
//!
//! **四条纪律**：执行器不碰页（行经存储服务，可见性由服务负责）；读路径
//! 不含锁；deadline/取消贯穿每个 `next()`；计划不可变、执行状态单次新造。

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod agg;
pub mod context;
pub mod direct;
pub mod error;
pub mod expr;
pub mod index_scan;
pub mod join;
pub mod nodes;
pub mod operator;
pub mod plan;
pub mod setops;
pub mod sort;
pub mod value;

pub use agg::{AggKind, AggSpec, HashAgg, ScalarAgg, SortedAgg};
pub use context::{ExecContext, OpStat, WorkAreaOutcome, WorkAreaStats};
pub use direct::{execute_direct, SelectQuery};
pub use error::ExecError;
pub use expr::{ArithOp, CmpOp, Expr, Truth};
pub use index_scan::{IndexScan, DEFAULT_FETCH_BATCH};
pub use join::{JoinKind, NestedLoop};
pub use nodes::{Filter, Limit, Project, SeqScan};
pub use operator::{collect, Operator, RowCursor};
pub use plan::{build, ExecEnv, PlanNode, SourceId};
pub use setops::{all_columns_keys, Append, SetOp, SetOpKind, Unique};
pub use sort::{Sort, SortKey, TopN};
pub use value::{
    cast_value, decode_row, encode_row, kind_name, row_bytes, ColKind, Row, RowShape, Value,
};
