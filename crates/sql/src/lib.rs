//! # bicdb-sql — SQL 前端
//!
//! 设计依据：[`doc/SQL前端设计_v0.1.md`]（草案 v0.1）与 `doc/spec/SQL.md`
//! （REQ-SQL-001…011）。对应阶段：**P5**。
//!
//! **五阶段**（REQ-SQL-001）：SQL 文本 → ① Raw AST → ② Bound Query →
//! ③ 逻辑表示 → ④ 物理计划 → ⑤ 执行。**①②③④ 是编译，⑤ 是执行**。
//!
//! **本切片（S1）已落地**：
//! - [`lexer`]：词法——token 流 + **字节区间位置**；关键字闭集；字面量只记
//!   原文本（值的解析在 ② 走 TYP 内核）；
//! - [`ast`]：**Raw AST**——只有语法与位置，**不 import 任何目录接口**
//!   （REQ-SQL-002 的验收原文；本文件不引用 catalog 即依赖检查通过）；
//! - [`parser`]：手写递归下降 + 显式优先级——语句闭集（REQ-SQL-005 正面清单），
//!   清单外构造**没有产生式**（REQ-SQL-006 闭集纪律）。
//!
//! **未落地**：Binder（②）、逻辑表示与变换（③）、物理计划（④）、计划缓存、
//! 执行接口——按设计 §10 的切片 S2–S8 推进；`GRAPH_TABLE` 随图域接入。

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod ast;
pub mod lexer;
pub mod parser;

pub use ast::{
    AlterAction, AlterWorkspaceStmt, BinaryOp, ColumnDef, CreateGraphStmt, CreateIndexStmt,
    CreateTableStmt, CreateWorkspaceStmt, DeleteStmt, DropGraphStmt, DropIndexStmt, DropTableStmt,
    DropWorkspaceStmt, Expr, FromClause, IndexTarget, InsertStmt, Join, JoinKind, Literal,
    OptionItem, OptionValue, OrderItem, SelectCore, SelectItem, SelectStmt, SetOpKind, SetOpTail,
    Stmt, TableRef, TxnStmt, TypeName, UnaryOp, UpdateStmt,
};
pub use lexer::{tokenize, Keyword, LexError, Punct, Span, Token, TokenKind};
pub use parser::{parse, parse_many, ParseError};
