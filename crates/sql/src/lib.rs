//! # bicdb-sql — SQL 前端
//!
//! 设计依据：[`doc/SQL前端设计_v0.1.md`]（草案 v0.1）与 `doc/spec/SQL.md`
//! （REQ-SQL-001…011）。对应阶段：**P5**。
//!
//! **五阶段**（REQ-SQL-001）：SQL 文本 → ① Raw AST → ② Bound Query →
//! ③ 逻辑表示 → ④ 物理计划 → ⑤ 执行。**①②③④ 是编译，⑤ 是执行**。
//!
//! **本切片（S1）已落地**（**形状对齐 PostgreSQL**——用户口径 2026-10-06）：
//! - [`lexer`]：词法——token 流 + **字节区间位置**；关键字闭集；引号标识符；
//!   字面量只记原文本（值的解析在 ② 走 TYP 内核）；未引号名**照 PG 折叠小写**；
//! - [`ast`]：**Raw AST**——节点**同名同形于 PG**（`parsenodes.h`/`primnodes.h`
//!   REL_16_STABLE，只裁剪无关字段；映射表见设计 §3.1）；只有语法与位置，
//!   **不 import 任何目录接口**（REQ-SQL-002 的验收原文）；
//! - [`parser`]：**照 PG 的产生式删减**——手写递归下降、每 PG 产生式一个函数、
//!   **优先级表照抄 PG**（`gram.y`）；语句闭集（REQ-SQL-005 正面清单），
//!   清单外构造**没有产生式**（REQ-SQL-006 闭集纪律）。
//!
//! **S2 已落地**（2026-10-06）：[`bind`]——**名字解析三格**（① 对象命名空间
//! （自举对象 `obj# ≤ 99` 出局）→ ② 固定表清单 → ③ 其余一律"不存在"）+
//! **保留名清单**（`$` 结尾 + 预置对象名）+ **版本捕获**（`(obj#, mtime)` /
//! `(obj#, mtime, status)` 进 [`bind::BoundRefs`]——计划缓存键的成分）；
//! 目录只读面经 [`bind::CatalogView`] 端口接入（真件 = [`bind::CatalogViewImpl`]）。
//!
//! **D1 已落地**（2026-10-06；`doc/DCL语句设计_v0.1.md` §5——**纯解析，先于 S2**）：
//! DCL 语句面——[`ast::Stmt::VariableSet`]（`ALTER SESSION SET/CLEAR`）、
//! [`ast::Stmt::AlterSystem`]（F 组：文件系统池三动作）、
//! [`ast::Stmt::AlterDatabase`]（W2 克隆双源 + T1–T3 模板四动作）、
//! [`ast::WorkRef`]/[`ast::FsRef`] 双形态（**解析只认形态**——名字查找与唯一性
//! 在 ② 绑定期）；`CREATE WORKSPACE` 去掉 `CLONE OF`（评审点①）。
//!
//! **已落地（2026-10-06）**：② Binder（`bind/`：名字解析三格、类型推导、参数
//! 定型、写目标检查、版本捕获）、④ 物理计划（`plan.rs`，**首版直映射**）、
//! ⑤ 会话（`session.rs`：`parse → bind → plan → execute`、事务边界、DML 索引维护
//! 与唯一性预检 `dml_index.rs`）。
//!
//! **未落地**：③ 逻辑表示与白名单变换（触发条件 = REQ-SQL-007 首条规则启用）、
//! 计划缓存（S6）、`UPDATE`/`DELETE`/聚合/连接/集合运算/`DISTINCT`/`LIKE`
//! （**绑定期具名拒绝**）、`GRAPH_TABLE`（随图域接入）。

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod ast;
pub mod bind;
pub mod dml_index;
pub mod lexer;
pub mod parser;
pub mod plan;
pub mod session;

pub use ast::{
    AConst, AExpr, AExprKind, Alias, AlterDatabaseAction, AlterDatabaseStmt, AlterSystemAction,
    AlterSystemStmt, AlterWorkspaceAction, AlterWorkspaceStmt, BoolExpr, BoolExprType, CaseExpr,
    CaseWhen, CoalesceExpr, ColumnDef, ColumnRef, ColumnRefField, ConstValue, CreateGraphStmt,
    CreateStmt, CreateWorkspaceStmt, DefElem, DefElemArg, DeleteStmt, DropStmt, Expr, FromItem,
    FsRef, FuncCall, IndexElem, IndexStmt, IndexTargetKind, InsertStmt, JoinExpr, JoinType,
    Location, NullTest, NullTestType, ObjectType, ParamRef, RangeVar, ResTarget, SelectStmt,
    SetOperation, SortBy, SortByDir, SortByNulls, Stmt, TransactionStmt, TransactionStmtKind,
    TypeCast, TypeName, UpdateStmt, VariableSetKind, VariableSetStmt, WorkRef, WorkspaceSource,
};
pub use bind::{
    check_new_object_name, is_reserved_name, BindError, BoundRefs, CatalogColumn, CatalogIndex,
    CatalogObject, CatalogView, CatalogViewImpl, NameResolver, NameSpace, ResolvePolicy,
    ResolvedName, FIXED_TABLES, PRESET_OBJECTS,
};
pub use lexer::{tokenize, Keyword, LexError, Punct, Span, Token, TokenKind};
pub use parser::{parse, parse_many, ParseError};
pub use plan::{ddl_summary, plan_statement, PhysicalPlan, PlanKind, SourcePlan};
pub use session::{format_value, QueryResult, Session, SessionError};
