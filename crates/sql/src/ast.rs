//! **Raw AST**（设计 `doc/SQL前端设计_v0.1.md` §3.3；REQ-SQL-002）。
//!
//! **正面清单纪律**（代码审查据此，验收原文"AST 模块不 import 目录接口"）：
//!
//! | 有 | 没有 |
//! | --- | --- |
//! | 语法节点（语句 / 子句 / 表达式） | 任何对象号 / 对象版本 |
//! | 标识符的**文本**与大小写 | 任何解析结果（表不存在？列是否存在？） |
//! | 字面量的原文本与位置 | 任何类型判定（`CAST` 的类型名也只是文本） |
//! | 源码位置（字节区间） | 任何权限判定 |
//!
//! ⇒ 三项收益随之成立：可单测（零目录状态）、**可跨工作区复用**（实例级
//! AST 缓存安全）、名字探测不可能发生在解析期。

use crate::lexer::Span;

/// 一条语句（Raw AST 根）。
///
/// 变体尺寸差异大（`Select` 最大）——本项目按值传递语句对象的场景（解析后
/// 立即消费/单测比对）不值得引入 `Box`；抑制该 lint 是**有意**的。
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    /// `SELECT …`（含集合运算链）。
    Select(SelectStmt),
    /// `INSERT INTO … VALUES …`。
    Insert(InsertStmt),
    /// `UPDATE … SET …`。
    Update(UpdateStmt),
    /// `DELETE FROM …`。
    Delete(DeleteStmt),
    /// `CREATE TABLE …`。
    CreateTable(CreateTableStmt),
    /// `DROP TABLE …`。
    DropTable(DropTableStmt),
    /// `CREATE [UNIQUE] INDEX …`（表 / 图 / 表达式键）。
    CreateIndex(CreateIndexStmt),
    /// `DROP INDEX …`。
    DropIndex(DropIndexStmt),
    /// `CREATE GRAPH …`。
    CreateGraph(CreateGraphStmt),
    /// `DROP GRAPH …`。
    DropGraph(DropGraphStmt),
    /// `CREATE WORKSPACE …`（仅 admin）。
    CreateWorkspace(CreateWorkspaceStmt),
    /// `ALTER WORKSPACE …`（仅 admin）。
    AlterWorkspace(AlterWorkspaceStmt),
    /// `DROP WORKSPACE …`（仅 admin）。
    DropWorkspace(DropWorkspaceStmt),
    /// 事务控制（`BEGIN` / `COMMIT` / `ROLLBACK`）。
    Txn {
        /// 哪一种。
        kind: TxnStmt,
        /// 位置。
        span: Span,
    },
}

/// 语句整体区间（错误定位 / 缓存文本比对的辅助）。
#[must_use]
pub fn stmt_span(stmt: &Stmt) -> Span {
    match stmt {
        Stmt::Select(s) => s.span,
        Stmt::Insert(s) => s.span,
        Stmt::Update(s) => s.span,
        Stmt::Delete(s) => s.span,
        Stmt::CreateTable(s) => s.span,
        Stmt::DropTable(s) => s.span,
        Stmt::CreateIndex(s) => s.span,
        Stmt::DropIndex(s) => s.span,
        Stmt::CreateGraph(s) => s.span,
        Stmt::DropGraph(s) => s.span,
        Stmt::CreateWorkspace(s) => s.span,
        Stmt::AlterWorkspace(s) => s.span,
        Stmt::DropWorkspace(s) => s.span,
        Stmt::Txn { span, .. } => *span,
    }
}

// ─────────────────────────── 表达式 ───────────────────────────

/// 二元操作符。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    /// `OR`
    Or,
    /// `AND`
    And,
    /// `=`
    Eq,
    /// `<>`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`
    Div,
    /// `<->`（向量 L2；RET 接口）
    VecL2,
    /// `<=>`（余弦）
    VecCosine,
    /// `<#>`（负内积）
    VecNegInner,
}

/// 一元操作符。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    /// `NOT`
    Not,
    /// 一元 `-`
    Neg,
    /// 一元 `+`
    PosAffirm,
}

/// 字面量（**原文本 + 种类**；值的解析在 ② 走 TYP 内核）。
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    /// 数字原文本。
    Number(String),
    /// 字符串（已解转义）。
    Str(Vec<u8>),
    /// `TRUE` / `FALSE`。
    Bool(bool),
    /// `NULL`。
    Null,
}

/// 表达式（Raw AST：只有语法与位置）。
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// 字面量。
    Literal {
        /// 字面量本体。
        value: Literal,
        /// 位置。
        span: Span,
    },
    /// 列引用（可带 `表.列` 限定）。
    Column {
        /// 限定名（可空）。
        qualifier: Option<String>,
        /// 列名文本（大小写折叠在 ②）。
        name: String,
        /// 位置。
        span: Span,
    },
    /// 函数调用（`COALESCE`/`NULLIF`/`json_get`/`json_exists`/`json_set`/`id`/…——
    /// **有无此函数、参数个数与类型**都是 ② 的事）。
    Call {
        /// 函数名文本。
        name: String,
        /// `DISTINCT` 修饰（`COUNT(DISTINCT x)`）——**只对聚合函数合法**，
        /// 是不是聚合、参数个数对不对，都是 ② 的事。
        distinct: bool,
        /// 实参。
        args: Vec<Expr>,
        /// 位置。
        span: Span,
    },
    /// 参数 `:name`（类型绑定期推导）。
    Param {
        /// 参数名。
        name: String,
        /// 位置。
        span: Span,
    },
    /// 二元运算。
    Binary {
        /// 操作符。
        op: BinaryOp,
        /// 左。
        left: Box<Expr>,
        /// 右。
        right: Box<Expr>,
        /// 位置。
        span: Span,
    },
    /// 一元运算。
    Unary {
        /// 操作符。
        op: UnaryOp,
        /// 操作数。
        expr: Box<Expr>,
        /// 位置。
        span: Span,
    },
    /// `IS [NOT] NULL`。
    IsNull {
        /// 操作数。
        expr: Box<Expr>,
        /// 是否 `NOT`。
        negated: bool,
        /// 位置。
        span: Span,
    },
    /// `[NOT] IN (列表)`（列表项为表达式——清单只允许字面量，② 判）。
    InList {
        /// 操作数。
        expr: Box<Expr>,
        /// 列表。
        list: Vec<Expr>,
        /// 是否 `NOT`。
        negated: bool,
        /// 位置。
        span: Span,
    },
    /// `[NOT] BETWEEN a AND b`。
    Between {
        /// 操作数。
        expr: Box<Expr>,
        /// 下界。
        low: Box<Expr>,
        /// 上界。
        high: Box<Expr>,
        /// 是否 `NOT`。
        negated: bool,
        /// 位置。
        span: Span,
    },
    /// `CASE [operand] WHEN … THEN … [ELSE …] END`。
    Case {
        /// 简单 CASE 的操作数（`None` = 搜索 CASE）。
        operand: Option<Box<Expr>>,
        /// `(WHEN, THEN)` 列表。
        whens: Vec<(Expr, Expr)>,
        /// `ELSE`。
        otherwise: Option<Box<Expr>>,
        /// 位置。
        span: Span,
    },
    /// **实参位置的 `*`**（`COUNT(*)`）。
    ///
    /// 只在函数调用的实参位置产生；其他位置（投影项之外的运算数等）语法层
    /// 就不可达（投影的 `*` 走 [`SelectItem`] 的空表达式形态）。
    Star {
        /// 位置。
        span: Span,
    },
    /// `CAST(expr AS 类型名)`（**类型名只是文本**——判定在 ②）。
    Cast {
        /// 操作数。
        expr: Box<Expr>,
        /// 类型名与参数（如 `NUMBER(10,2)`）。
        type_name: TypeName,
        /// 位置。
        span: Span,
    },
}

impl Expr {
    /// 位置。
    #[must_use]
    pub fn span(&self) -> Span {
        match self {
            Expr::Literal { span, .. }
            | Expr::Column { span, .. }
            | Expr::Call { span, .. }
            | Expr::Param { span, .. }
            | Expr::Binary { span, .. }
            | Expr::Unary { span, .. }
            | Expr::IsNull { span, .. }
            | Expr::InList { span, .. }
            | Expr::Between { span, .. }
            | Expr::Case { span, .. }
            | Expr::Cast { span, .. }
            | Expr::Star { span } => *span,
        }
    }
}

/// 类型名（`CAST` / 列定义用；**文本 + 位置**，合法性与语义在 ② 判）。
#[derive(Debug, Clone, PartialEq)]
pub struct TypeName {
    /// 类型名文本（如 `NUMBER` / `TIMESTAMP` / `VECTOR`）。
    pub name: String,
    /// 参数（如 `NUMBER(10,2)` 的 `10, 2`；原文本）。
    pub args: Vec<String>,
    /// 位置。
    pub span: Span,
}

// ─────────────────────────── SELECT ───────────────────────────

/// 集合运算种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOpKind {
    /// `UNION`
    Union,
    /// `INTERSECT`
    Intersect,
    /// `EXCEPT`
    Except,
}

/// 投影项。
#[derive(Debug, Clone, PartialEq)]
pub struct SelectItem {
    /// 表达式（`*` 用 [`SelectItem::Wildcard`]）。
    pub expr: Option<Expr>,
    /// 别名（`AS 名` 或裸名）。
    pub alias: Option<String>,
    /// 位置。
    pub span: Span,
}

/// `SELECT` 查询表达式（**集合运算链自左向右**；`ORDER BY`/`LIMIT` 作用于
/// **整个查询表达式**——SQL 标准形态，与执行计划里"排序在集合运算之上"一致）。
#[derive(Debug, Clone, PartialEq)]
pub struct SelectStmt {
    /// 第一个 `SELECT` 核心。
    pub first: SelectCore,
    /// 集合运算链（自左向右）。
    pub set_ops: Vec<SetOpTail>,
    /// `ORDER BY`。
    pub order_by: Vec<OrderItem>,
    /// `LIMIT`（原文本；值语义在 ②/④）。
    pub limit: Option<String>,
    /// `OFFSET`。
    pub offset: Option<String>,
    /// 位置。
    pub span: Span,
}

/// 集合运算链的一节：`<op> [ALL] <SELECT 核心>`。
#[derive(Debug, Clone, PartialEq)]
pub struct SetOpTail {
    /// 运算。
    pub op: SetOpKind,
    /// 是否 `ALL`（未写 = 去重，SQL 标准默认）。
    pub all: bool,
    /// 右侧 `SELECT` 核心。
    pub core: SelectCore,
    /// 位置。
    pub span: Span,
}

/// 一个 `SELECT` 核心（投影 + 来源 + 过滤 + 分组）。
#[derive(Debug, Clone, PartialEq)]
pub struct SelectCore {
    /// `DISTINCT`。
    pub distinct: bool,
    /// 投影列表。
    pub projection: Vec<SelectItem>,
    /// `FROM`（`None` = 无来源，如 `SELECT 1`）。
    pub from: Option<FromClause>,
    /// `WHERE`。
    pub filter: Option<Expr>,
    /// `GROUP BY`。
    pub group_by: Vec<Expr>,
    /// `HAVING`。
    pub having: Option<Expr>,
    /// 位置。
    pub span: Span,
}

/// 排序项。
#[derive(Debug, Clone, PartialEq)]
pub struct OrderItem {
    /// 排序表达式。
    pub expr: Expr,
    /// `DESC`（缺省升序）。
    pub desc: bool,
    /// 位置。
    pub span: Span,
}

/// `FROM` 子句：基础表 + 连接链。
#[derive(Debug, Clone, PartialEq)]
pub struct FromClause {
    /// 基础表引用。
    pub base: TableRef,
    /// 连接链（含逗号连接——普通连接）。
    pub joins: Vec<Join>,
    /// 位置。
    pub span: Span,
}

/// 表引用。
#[derive(Debug, Clone, PartialEq)]
pub enum TableRef {
    /// 表名 / 固定表名（**是不是固定表在 ② 判**）。
    Name {
        /// 名字文本。
        name: String,
        /// 别名（`AS 名` 或裸名）。
        alias: Option<String>,
        /// 位置。
        span: Span,
    },
}

/// 一个连接。
#[derive(Debug, Clone, PartialEq)]
pub struct Join {
    /// `INNER` / `LEFT [OUTER]`；逗号连接 = `Inner`（无 `ON`）。
    pub kind: JoinKind,
    /// 右表。
    pub table: TableRef,
    /// `ON` 条件（逗号连接为 `None`）。
    pub on: Option<Expr>,
    /// 位置。
    pub span: Span,
}

/// 连接种类（清单：`INNER` / `LEFT [OUTER]`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    /// `INNER JOIN`（或逗号）。
    Inner,
    /// `LEFT [OUTER] JOIN`。
    Left,
}

// ─────────────────────────── DML ───────────────────────────

/// `INSERT INTO t [(cols)] VALUES (…), (…)`。
#[derive(Debug, Clone, PartialEq)]
pub struct InsertStmt {
    /// 目标表名。
    pub table: String,
    /// 列清单（`None` = 全列按定义序）。
    pub columns: Option<Vec<String>>,
    /// 行值（每行一组表达式）。
    pub rows: Vec<Vec<Expr>>,
    /// 位置。
    pub span: Span,
}

/// `UPDATE t SET c = e, … [WHERE …]`。
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateStmt {
    /// 目标表名。
    pub table: String,
    /// `(列名, 表达式)` 列表。
    pub sets: Vec<(String, Expr)>,
    /// `WHERE`。
    pub filter: Option<Expr>,
    /// 位置。
    pub span: Span,
}

/// `DELETE FROM t [WHERE …]`。
#[derive(Debug, Clone, PartialEq)]
pub struct DeleteStmt {
    /// 目标表名。
    pub table: String,
    /// `WHERE`。
    pub filter: Option<Expr>,
    /// 位置。
    pub span: Span,
}

// ─────────────────────────── DDL ───────────────────────────

/// 列定义（`名 类型 [NOT NULL]`；**没有** DEFAULT/PK/FK/CHECK——REQ-SQL-006）。
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDef {
    /// 列名文本。
    pub name: String,
    /// 类型名。
    pub type_name: TypeName,
    /// `NOT NULL`。
    pub not_null: bool,
    /// 位置。
    pub span: Span,
}

/// `WITH (名 = 值, …)` 的一个选项（**合法组合在 ② 判**——表类型决定选项组）。
#[derive(Debug, Clone, PartialEq)]
pub struct OptionItem {
    /// 选项名（小写规范在 ②）。
    pub name: String,
    /// 值（原文本形态）。
    pub value: OptionValue,
    /// 位置。
    pub span: Span,
}

/// 选项值（原文本；语义在 ②）。
#[derive(Debug, Clone, PartialEq)]
pub enum OptionValue {
    /// 标识符形态（`normal` / `config` / …）。
    Ident(String),
    /// 数字形态。
    Number(String),
    /// 字符串形态（如 `'7d'`）。
    Str(Vec<u8>),
}

/// `CREATE TABLE …`。
#[derive(Debug, Clone, PartialEq)]
pub struct CreateTableStmt {
    /// 表名。
    pub name: String,
    /// 列定义。
    pub columns: Vec<ColumnDef>,
    /// `WITH (…)` 选项。
    pub options: Vec<OptionItem>,
    /// 位置。
    pub span: Span,
}

/// `DROP TABLE …`。
#[derive(Debug, Clone, PartialEq)]
pub struct DropTableStmt {
    /// 表名。
    pub name: String,
    /// 位置。
    pub span: Span,
}

/// 索引目标（表 / 图的顶点 / 图的边）。
#[derive(Debug, Clone, PartialEq)]
pub enum IndexTarget {
    /// 表。
    Table(String),
    /// 图的顶点属性。
    Vertex(String),
    /// 图的边属性。
    Edge(String),
}

/// `CREATE [UNIQUE] INDEX 名 ON 目标 (表达式, …)`。
#[derive(Debug, Clone, PartialEq)]
pub struct CreateIndexStmt {
    /// 索引名。
    pub name: String,
    /// 唯一索引。
    pub unique: bool,
    /// 目标。
    pub target: IndexTarget,
    /// 键表达式（列引用 / JSON 路径表达式）。
    pub keys: Vec<Expr>,
    /// 位置。
    pub span: Span,
}

/// `DROP INDEX …`。
#[derive(Debug, Clone, PartialEq)]
pub struct DropIndexStmt {
    /// 索引名。
    pub name: String,
    /// 位置。
    pub span: Span,
}

/// `CREATE GRAPH …`。
#[derive(Debug, Clone, PartialEq)]
pub struct CreateGraphStmt {
    /// 图名。
    pub name: String,
    /// 位置。
    pub span: Span,
}

/// `DROP GRAPH …`。
#[derive(Debug, Clone, PartialEq)]
pub struct DropGraphStmt {
    /// 图名。
    pub name: String,
    /// 位置。
    pub span: Span,
}

/// `CREATE WORKSPACE FOR USER … [NAME '…'] [CLONE OF …]`（仅 admin）。
#[derive(Debug, Clone, PartialEq)]
pub struct CreateWorkspaceStmt {
    /// 主体名。
    pub subject: String,
    /// `NAME '…'`。
    pub name: Option<Vec<u8>>,
    /// `CLONE OF <workspace_id>`。
    pub clone_of: Option<String>,
    /// 位置。
    pub span: Span,
}

/// `ALTER WORKSPACE …`（改名 / 配额）。
#[derive(Debug, Clone, PartialEq)]
pub struct AlterWorkspaceStmt {
    /// 工作区 id 文本。
    pub workspace_id: String,
    /// 动作。
    pub action: AlterAction,
    /// 位置。
    pub span: Span,
}

/// `ALTER WORKSPACE` 的动作。
#[derive(Debug, Clone, PartialEq)]
pub enum AlterAction {
    /// `SET NAME = '…' | NULL`。
    SetName(Option<Vec<u8>>),
    /// `SET QUOTA (data=…, undo=…, temp=…, asset=…)`。
    SetQuota(Vec<OptionItem>),
}

/// `DROP WORKSPACE …`。
#[derive(Debug, Clone, PartialEq)]
pub struct DropWorkspaceStmt {
    /// 工作区 id 文本。
    pub workspace_id: String,
    /// 位置。
    pub span: Span,
}

/// 事务控制语句。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnStmt {
    /// `BEGIN`
    Begin,
    /// `COMMIT`
    Commit,
    /// `ROLLBACK`
    Rollback,
}
