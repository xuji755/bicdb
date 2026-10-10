//! **Raw AST**（设计 `doc/SQL前端设计_v0.1.md` §3.1）——**形状对齐 PostgreSQL**：
//! 节点同名同形（字段名用 Rust 命名风格），**只裁剪与本库面无关的字段**。
//!
//! - 每个节点一一对应 PG 的解析节点（`parsenodes.h` / `primnodes.h`，
//!   REL_16_STABLE；映射表见设计 §3.1，取证 `12-pg-parser-source.txt`）；
//! - **两处记档差异**：① `location` 用**字节区间**（PG 用起始偏移的 `int`——
//!   本库规格 REQ-SQL-002 要"字节偏移区间"）；② `ParamRef` 用 `:name`
//!   （PG 用 `$n`——本库规格 REQ-SQL-005 明定）；
//! - **正面清单纪律**（REQ-SQL-002）：只有语法与位置——**不 import 任何目录
//!   接口**、无对象号、无类型判定。

use crate::lexer::Span;

/// 位置（PG 的 `int location` 扩展为字节区间——设计 §3.1 记档）。
pub type Location = Span;

/// 一条语句（PG 的 `RawStmt`；每个变体自带 `location`）。
#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    /// `SHOW TABLES`: current-workspace metadata, including dictionary names.
    ShowTables(Location),
    /// List current-workspace named graphs.
    ShowGraphs(Location),
    /// Execute bounded Cypher on a native graph with JSON parameters.
    Cypher(CypherStmt),
    /// Graph property-index lifecycle (separate from table column indexes).
    GraphIndex(GraphIndexStmt),
    /// `SELECT`（`SelectStmt`）。
    Select(SelectStmt),
    /// `INSERT`（`InsertStmt`）。
    Insert(InsertStmt),
    /// `UPDATE`（`UpdateStmt`）。
    Update(UpdateStmt),
    /// `DELETE`（`DeleteStmt`）。
    Delete(DeleteStmt),
    /// `CREATE TABLE`（`CreateStmt`）。
    CreateTable(CreateStmt),
    /// `CREATE [UNIQUE] INDEX`（`IndexStmt`）。
    Index(IndexStmt),
    /// `DROP`（`DropStmt`；对象类型区分表/索引/图/工作区）。
    Drop(DropStmt),
    /// 事务控制（`TransactionStmt`）。
    Transaction(TransactionStmt),
    /// `CREATE GRAPH`（本库扩展；形状仿 PG 的 DDL 节点）。
    CreateGraph(CreateGraphStmt),
    /// `CREATE WORKSPACE`（本库扩展，仅 admin）。
    CreateWorkspace(CreateWorkspaceStmt),
    /// `ALTER WORKSPACE`（本库扩展，仅 admin）。
    AlterWorkspace(AlterWorkspaceStmt),
    /// `ALTER SESSION SET/CLEAR`（PG `VariableSetStmt`；白名单在 ② 判）。
    VariableSet(VariableSetStmt),
    /// `CREATE FILESYSTEM <名> USING '<路径>'`（F1）。
    CreateFilesystem(CreateFilesystemStmt),
    /// `ALTER FILESYSTEM <名> SET ALLOCATE = ON | OFF`（F2）。
    AlterFilesystem(AlterFilesystemStmt),
    /// `DROP FILESYSTEM <名>`（F3）。
    DropFilesystem(DropFilesystemStmt),
    /// `CREATE USER … USING WORKSPACE …`（U1）。
    CreateUser(CreateUserStmt),
    /// `ALTER USER …`（U2–U6）。
    AlterUser(AlterUserStmt),
    /// `DROP USER <主体> [CASCADE]`（U7）。
    DropUser(DropUserStmt),
    /// `ALTER DATABASE …`（PG `AlterDatabaseStmt` 的**无库名**形态；T 组）。
    AlterDatabase(AlterDatabaseStmt),
}

impl Stmt {
    /// 语句起始位置。
    #[must_use]
    pub fn location(&self) -> Location {
        match self {
            Stmt::ShowTables(location) | Stmt::ShowGraphs(location) => *location,
            Stmt::Cypher(s) => s.location,
            Stmt::GraphIndex(s) => s.location,
            Stmt::Select(s) => s.location,
            Stmt::Insert(s) => s.location,
            Stmt::Update(s) => s.location,
            Stmt::Delete(s) => s.location,
            Stmt::CreateTable(s) => s.location,
            Stmt::Index(s) => s.location,
            Stmt::Drop(s) => s.location,
            Stmt::Transaction(s) => s.location,
            Stmt::CreateGraph(s) => s.location,
            Stmt::CreateWorkspace(s) => s.location,
            Stmt::AlterWorkspace(s) => s.location,
            Stmt::VariableSet(s) => s.location,
            Stmt::CreateFilesystem(s) => s.location,
            Stmt::AlterFilesystem(s) => s.location,
            Stmt::DropFilesystem(s) => s.location,
            Stmt::CreateUser(s) => s.location,
            Stmt::AlterUser(s) => s.location,
            Stmt::DropUser(s) => s.location,
            Stmt::AlterDatabase(s) => s.location,
        }
    }
}

// ─────────────────────────── 表达式（primnodes.h）───────────────────────────

/// 表达式（PG 是 `Node*`；我们用枚举收窄——**记档**）。
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// 列引用 / `*`（`ColumnRef`）。
    ColumnRef(ColumnRef),
    /// 常量（`A_Const`）。
    AConst(AConst),
    /// 参数（`ParamRef`；本库用 `:name`）。
    ParamRef(ParamRef),
    /// 带名操作符表达式（`A_Expr`；含 `IN`/`BETWEEN`/`NULLIF`）。
    AExpr(AExpr),
    /// 布尔表达式（`BoolExpr`；`NOT` 是一元形态）。
    BoolExpr(BoolExpr),
    /// `IS [NOT] NULL`（`NullTest`）。
    NullTest(NullTest),
    /// 函数调用（`FuncCall`）。
    FuncCall(FuncCall),
    /// 转型（`TypeCast`；`CAST(x AS t)` 与 `x::t` 同节点）。
    TypeCast(TypeCast),
    /// `CASE`（`CaseExpr`）。
    CaseExpr(CaseExpr),
    /// `COALESCE`（`CoalesceExpr`）。
    CoalesceExpr(CoalesceExpr),
}

impl Expr {
    /// 位置。
    #[must_use]
    pub fn location(&self) -> Location {
        match self {
            Expr::ColumnRef(n) => n.location,
            Expr::AConst(n) => n.location,
            Expr::ParamRef(n) => n.location,
            Expr::AExpr(n) => n.location,
            Expr::BoolExpr(n) => n.location,
            Expr::NullTest(n) => n.location,
            Expr::FuncCall(n) => n.location,
            Expr::TypeCast(n) => n.location,
            Expr::CaseExpr(n) => n.location,
            Expr::CoalesceExpr(n) => n.location,
        }
    }
}

/// `ColumnRef` 的一个字段（PG：`String` 或 `A_Star`）。
#[derive(Debug, Clone, PartialEq)]
pub enum ColumnRefField {
    /// 名字（已按 PG 规则折叠：未引号 ⇒ 小写；引号 ⇒ 原样）。
    Name(String),
    /// `*`
    AStar,
}

/// 列引用（PG `ColumnRef`）。`a.b` = 两个 `Name`；`*` / `t.*` 含 `AStar`。
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnRef {
    /// 字段链（≥ 1）。
    pub fields: Vec<ColumnRefField>,
    /// 位置。
    pub location: Location,
}

/// 常量值（PG `A_Const.val` 的 `ValUnion` 子集；数字**留原文本**）。
#[derive(Debug, Clone, PartialEq)]
pub enum ConstValue {
    /// 整数形态（原文本）。
    Int(String),
    /// 浮点形态（原文本；含指数写法）。
    Float(String),
    /// 字符串（已解转义）。
    Str(Vec<u8>),
    /// 布尔。
    Bool(bool),
}

/// 常量（PG `A_Const`）。`value = None` ≡ PG 的 `isnull = true`（NULL）。
#[derive(Debug, Clone, PartialEq)]
pub struct AConst {
    /// 值（`None` = `NULL`）。
    pub value: Option<ConstValue>,
    /// 位置。
    pub location: Location,
}

impl AConst {
    /// 是不是 `NULL`。
    #[must_use]
    pub fn is_null(&self) -> bool {
        self.value.is_none()
    }
}

/// 参数（PG `ParamRef`；本库：`:name`，类型绑定期推导）。
#[derive(Debug, Clone, PartialEq)]
pub struct ParamRef {
    /// 参数名（已折叠）。
    pub name: String,
    /// 位置。
    pub location: Location,
}

/// `A_Expr_Kind` 的子集（PG 的完整枚举见 `parsenodes.h`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AExprKind {
    /// 普通操作符（`= <> < <= > >= + - * /` 与向量 `<-> <=> <#>`）。
    Op,
    /// `IN`（`name = "="`，值列表在 `rexpr_list`——照 PG 语义）。
    In,
    /// `NOT IN`
    NotIn,
    /// `BETWEEN`（下界/上界在 `rexpr_list`——照 PG 的二元列表）。
    Between,
    /// `NOT BETWEEN`
    NotBetween,
    /// `NULLIF`（PG 用 `A_Expr`，无独立节点）。
    NullIf,
}

/// 带名操作符表达式（PG `A_Expr`）。
///
/// **记档差异**：PG 的 `lexpr`/`rexpr` 都是 `Node*`，`BETWEEN`/`IN` 把
/// 值列表塞进 `rexpr`；Rust 无此形态 ⇒ 拆为 `rexpr`（单）与 `rexpr_list`（多）。
#[derive(Debug, Clone, PartialEq)]
pub struct AExpr {
    /// 种类。
    pub kind: AExprKind,
    /// 操作符名（`=`、`+`、`<->`、`BETWEEN`…——照 PG 用字符串）。
    pub name: String,
    /// 左操作数（`None` = 一元，如 `+x`）。
    pub lexpr: Option<Box<Expr>>,
    /// 单右操作数（`Op` / `NullIf`）。
    pub rexpr: Option<Box<Expr>>,
    /// 多右操作数（`In`/`NotIn` 的值表、`Between`/`NotBetween` 的上下界）。
    pub rexpr_list: Vec<Expr>,
    /// 位置。
    pub location: Location,
}

/// `BoolExprType`（PG 同名枚举的子集）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoolExprType {
    /// `AND`
    And,
    /// `OR`
    Or,
    /// `NOT`（一元；`args` 长 1）
    Not,
}

/// 布尔表达式（PG `BoolExpr`）。
#[derive(Debug, Clone, PartialEq)]
pub struct BoolExpr {
    /// 种类。
    pub boolop: BoolExprType,
    /// 操作数。
    pub args: Vec<Expr>,
    /// 位置。
    pub location: Location,
}

/// `NullTestType`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NullTestType {
    /// `IS NULL`
    IsNull,
    /// `IS NOT NULL`
    IsNotNull,
}

/// `IS [NOT] NULL`（PG `NullTest`）。
#[derive(Debug, Clone, PartialEq)]
pub struct NullTest {
    /// 操作数。
    pub arg: Box<Expr>,
    /// 种类。
    pub nulltesttype: NullTestType,
    /// 位置。
    pub location: Location,
}

/// 函数调用（PG `FuncCall`）。
#[derive(Debug, Clone, PartialEq)]
pub struct FuncCall {
    /// 函数名（已折叠）。
    pub funcname: String,
    /// 实参（`agg_star` 时为空——`COUNT(*)`）。
    pub args: Vec<Expr>,
    /// 实参是 `*`（PG `agg_star`）。
    pub agg_star: bool,
    /// `DISTINCT` 修饰（PG `agg_distinct`）。
    pub agg_distinct: bool,
    /// 位置。
    pub location: Location,
}

/// 转型（PG `TypeCast`；`CAST(x AS t)` 与 `x::t` 同节点）。
#[derive(Debug, Clone, PartialEq)]
pub struct TypeCast {
    /// 操作数。
    pub arg: Box<Expr>,
    /// 目标类型。
    pub type_name: TypeName,
    /// 位置。
    pub location: Location,
}

/// `CASE`（PG `CaseExpr`；去 `casetype`/`casecollid`）。
#[derive(Debug, Clone, PartialEq)]
pub struct CaseExpr {
    /// 简单 `CASE` 的操作数（`None` = 搜索 `CASE`）。
    pub arg: Option<Box<Expr>>,
    /// `WHEN` 列表。
    pub args: Vec<CaseWhen>,
    /// `ELSE`（`None` = 无）。
    pub defresult: Option<Box<Expr>>,
    /// 位置。
    pub location: Location,
}

/// 一个 `WHEN … THEN …`（PG `CaseWhen`）。
#[derive(Debug, Clone, PartialEq)]
pub struct CaseWhen {
    /// 条件（或简单 `CASE` 的比较值）。
    pub expr: Expr,
    /// 结果。
    pub result: Expr,
    /// 位置。
    pub location: Location,
}

/// `COALESCE`（PG `CoalesceExpr`）。
#[derive(Debug, Clone, PartialEq)]
pub struct CoalesceExpr {
    /// 实参。
    pub args: Vec<Expr>,
    /// 位置。
    pub location: Location,
}

/// 类型名（PG `TypeName` 的裁剪：`names` 列表 ⇒ 单名；无 OID/数组/`%TYPE`）。
#[derive(Debug, Clone, PartialEq)]
pub struct TypeName {
    /// 类型名（已折叠）。
    pub name: String,
    /// 类型参数（`NUMBER(10,2)` 的 `10, 2`——**原文本**）。
    pub typmods: Vec<String>,
    /// 位置。
    pub location: Location,
}

// ─────────────────────────── SELECT 家族 ───────────────────────────

/// 集合运算（PG `SetOperation`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOperation {
    /// `UNION`
    Union,
    /// `INTERSECT`
    Intersect,
    /// `EXCEPT`
    Except,
}

/// `SELECT`（PG `SelectStmt` 的裁剪）。
///
/// **集合运算照 PG 用 `op/all/larg/rarg` 左深嵌套**（不设"链"——
/// PG 的形状更通用；`ORDER BY`/`LIMIT` 在**最外层**节点上）。
#[derive(Debug, Clone, PartialEq)]
pub struct SelectStmt {
    /// `DISTINCT`（PG 的 `distinctClause` 列表 ⇒ `bool`——**无 `DISTINCT ON`**）。
    pub distinct: bool,
    /// 投影列表（`ResTarget`）。
    pub target_list: Vec<ResTarget>,
    /// `FROM`（PG 的 `List<Node>`：`RangeVar` 或 `JoinExpr`，
    /// **逗号分隔的项仍是列表里的独立元素**——照 PG 的原始树）。
    pub from_clause: Vec<FromItem>,
    /// `WHERE`
    pub where_clause: Option<Box<Expr>>,
    /// `GROUP BY`
    pub group_clause: Vec<Expr>,
    /// `HAVING`
    pub having_clause: Option<Box<Expr>>,
    /// `VALUES` 行（PG `valuesLists`；`INSERT … VALUES` 走它）。
    pub values_lists: Option<Vec<Vec<Expr>>>,
    /// `ORDER BY`（`SortBy` 列表）。
    pub sort_clause: Vec<SortBy>,
    /// `OFFSET`
    pub limit_offset: Option<Box<Expr>>,
    /// `LIMIT`
    pub limit_count: Option<Box<Expr>>,
    /// 集合运算种类（`None` = 普通 `SELECT`）。
    pub op: Option<SetOperation>,
    /// 集合运算的 `ALL`。
    pub all: bool,
    /// 左操作数（集合运算）。
    pub larg: Option<Box<SelectStmt>>,
    /// 右操作数（集合运算）。
    pub rarg: Option<Box<SelectStmt>>,
    /// 位置。
    pub location: Location,
}

/// `FROM` 的一项（PG 的 `List<Node>` 元素）。
#[derive(Debug, Clone, PartialEq)]
pub enum FromItem {
    /// 表引用。
    RangeVar(RangeVar),
    /// Read-only table-valued function (currently attachment_grep).
    RangeFunction(RangeFunction),
    /// Typed, read-only rows from a workspace-local Cypher query.
    GraphTable(Box<GraphTable>),
    /// 连接（`JoinExpr`；盒装以允许递归）。
    Join(Box<JoinExpr>),
}

impl FromItem {
    /// 位置。
    #[must_use]
    pub fn location(&self) -> Location {
        match self {
            FromItem::RangeVar(r) => r.location,
            FromItem::RangeFunction(r) => r.location,
            FromItem::GraphTable(r) => r.location,
            FromItem::Join(j) => j.location,
        }
    }
}

/// Workspace-local graph query with an explicit, positional scalar schema.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphTable {
    /// Named graph (no workspace/schema qualification).
    pub graph: String,
    /// UTF-8 Cypher literal or named SQL parameter.
    pub query: Expr,
    /// JSON object literal or named SQL parameter; omitted means `{}`.
    pub parameters: Option<Expr>,
    /// Optional JSON request budgets, literal or named SQL parameter.
    pub budgets: Option<Expr>,
    /// SQL column aliases and types, in Cypher RETURN order.
    pub columns: Vec<ColumnDef>,
    /// Optional relation alias.
    pub alias: Option<Alias>,
    /// Source location.
    pub location: Location,
}

/// A non-lateral, read-only table-valued function in FROM.
#[derive(Debug, Clone, PartialEq)]
pub struct RangeFunction {
    /// Function name.
    pub name: String,
    /// Literal or named input arguments.
    pub args: Vec<Expr>,
    /// Optional relation alias.
    pub alias: Option<Alias>,
    /// Source location.
    pub location: Location,
}

/// 表引用（PG `RangeVar` 的裁剪：无 catalog/schema/inh/relpersistence）。
#[derive(Debug, Clone, PartialEq)]
pub struct RangeVar {
    /// 关系名（已折叠）。
    pub relname: String,
    /// 别名。
    pub alias: Option<Alias>,
    /// 位置。
    pub location: Location,
}

/// 别名（PG `Alias` 的裁剪：无列别名清单）。
#[derive(Debug, Clone, PartialEq)]
pub struct Alias {
    /// 别名（已折叠）。
    pub aliasname: String,
}

/// 连接类型（PG `JoinType` 的子集）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinType {
    /// `INNER JOIN`（逗号连接**不**产生 `JoinExpr`——照 PG 的原始树）
    Inner,
    /// `LEFT [OUTER] JOIN`
    Left,
}

/// 连接（PG `JoinExpr` 的裁剪：只留 `jointype/larg/rarg/quals`）。
#[derive(Debug, Clone, PartialEq)]
pub struct JoinExpr {
    /// 连接类型。
    pub jointype: JoinType,
    /// 左。
    pub larg: FromItem,
    /// 右。
    pub rarg: FromItem,
    /// `ON` 条件。
    pub quals: Option<Box<Expr>>,
    /// 位置。
    pub location: Location,
}

/// 投影/赋值目标（PG `ResTarget` 的裁剪：无 `indirection`）。
///
/// `SELECT *` 的值是 `ColumnRef{fields:[AStar]}`（**照 PG**——`*` 不是特例节点）。
#[derive(Debug, Clone, PartialEq)]
pub struct ResTarget {
    /// 输出名（`AS 名` 或裸名）。
    pub name: Option<String>,
    /// 值表达式。
    pub val: Expr,
    /// 位置。
    pub location: Location,
}

/// 排序方向（PG `SortByDir` 的子集；`USING` 在清单外）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortByDir {
    /// 未写（缺省升序）
    Default,
    /// `ASC`
    Asc,
    /// `DESC`
    Desc,
}

/// NULL 位置（PG `SortByNulls`；**语法规格暂不开 `NULLS FIRST/LAST`**，
/// 字段保留使将来加入零成本——设计 §3.1 记档）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortByNulls {
    /// 未写（用本库固定规则：升序在最后、降序在最前）
    Default,
    /// `NULLS FIRST`
    First,
    /// `NULLS LAST`
    Last,
}

/// 排序项（PG `SortBy`）。
#[derive(Debug, Clone, PartialEq)]
pub struct SortBy {
    /// 排序表达式。
    pub node: Expr,
    /// 方向。
    pub sortby_dir: SortByDir,
    /// NULL 位置。
    pub sortby_nulls: SortByNulls,
    /// 位置。
    pub location: Location,
}

// ─────────────────────────── DML ───────────────────────────

/// `INSERT`（PG `InsertStmt` 的裁剪）。
#[derive(Debug, Clone, PartialEq)]
pub struct InsertStmt {
    /// 目标表。
    pub relation: RangeVar,
    /// 列清单（`ResTarget` 列表——照 PG 的 `insert_column_list`）。
    pub cols: Vec<ResTarget>,
    /// 来源（`SELECT`/`VALUES`；**`VALUES` 走 `SelectStmt.values_lists`**——照 PG）。
    pub select_stmt: Option<Box<SelectStmt>>,
    /// 位置。
    pub location: Location,
}

/// `UPDATE`（PG `UpdateStmt` 的裁剪）。
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateStmt {
    /// 目标表。
    pub relation: RangeVar,
    /// `SET` 列表（`ResTarget`：`name` = 列名、`val` = 表达式）。
    pub target_list: Vec<ResTarget>,
    /// `WHERE`
    pub where_clause: Option<Box<Expr>>,
    /// 位置。
    pub location: Location,
}

/// `DELETE`（PG `DeleteStmt` 的裁剪）。
#[derive(Debug, Clone, PartialEq)]
pub struct DeleteStmt {
    /// 目标表。
    pub relation: RangeVar,
    /// `WHERE`
    pub where_clause: Option<Box<Expr>>,
    /// 位置。
    pub location: Location,
}

// ─────────────────────────── DDL ───────────────────────────

/// `CREATE TABLE`（PG `CreateStmt` 的裁剪）。
#[derive(Debug, Clone, PartialEq)]
pub struct CreateStmt {
    /// 表名。
    pub relation: RangeVar,
    /// 列定义（PG 的 `tableElts`）。
    pub table_elts: Vec<ColumnDef>,
    /// `WITH (…)` 选项（`DefElem` 列表——照 PG）。
    pub options: Vec<DefElem>,
    /// 位置。
    pub location: Location,
}

/// 列定义（PG `ColumnDef` 的裁剪：`default`/约束/存储/压缩在清单外）。
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDef {
    /// 列名（已折叠）。
    pub colname: String,
    /// 类型。
    pub type_name: TypeName,
    /// `NOT NULL`（PG `is_not_null`）。
    pub is_not_null: bool,
    /// 位置。
    pub location: Location,
}

/// 一个 DDL 选项（PG `DefElem`）。
///
/// **记档差异**：PG 的 `arg` 是任意 `Node`；我们收窄为
/// [`DefElemArg`]（常量或**裸标识符**——`table_type = memory` 是本库写法，
/// PG 的 reloptions 只收字面量）。
#[derive(Debug, Clone, PartialEq)]
pub struct DefElem {
    /// 选项名（已折叠）。
    pub defname: String,
    /// 值。
    pub arg: DefElemArg,
    /// 位置。
    pub location: Location,
}

/// 选项值。
#[derive(Debug, Clone, PartialEq)]
pub enum DefElemArg {
    /// 字面量。
    Const(AConst),
    /// 裸标识符（本库扩展；如 `memory`）。
    Ident(String),
}

/// 索引目标种类（**本库扩展**：图索引的顶点/边；PG 无对应）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexTargetKind {
    /// 表。
    Table,
    /// 图的顶点属性。
    Vertex,
    /// 图的边属性。
    Edge,
}

/// `CREATE [UNIQUE] INDEX`（PG `IndexStmt` 的裁剪）。
#[derive(Debug, Clone, PartialEq)]
pub struct IndexStmt {
    /// 索引名。
    pub idxname: String,
    /// 目标（表或图）。
    pub relation: RangeVar,
    /// 目标种类（**本库扩展**）。
    pub target_kind: IndexTargetKind,
    /// 键（`IndexElem` 列表）。
    pub index_params: Vec<IndexElem>,
    /// 唯一索引。
    pub unique: bool,
    /// 位置。
    pub location: Location,
}

/// 一个索引键（PG `IndexElem` 的裁剪：只留 `name`/`expr`）。
#[derive(Debug, Clone, PartialEq)]
pub struct IndexElem {
    /// 列名（列引用形态）。
    pub name: Option<String>,
    /// 表达式（表达式索引形态，如 `json_get(doc, 'a.b')`）。
    pub expr: Option<Box<Expr>>,
    /// 位置。
    pub location: Location,
}

/// `DROP`（PG `DropStmt` 的裁剪）。
#[derive(Debug, Clone, PartialEq)]
pub struct DropStmt {
    /// 目标对象（`RangeVar` 列表；`remove_type != Workspace` 时用）。
    pub objects: Vec<RangeVar>,
    /// **工作区目标**（`remove_type == Workspace` 时用；`work_ref` 双形态）。
    pub workspaces: Vec<WorkRef>,
    /// 对象类型。
    pub remove_type: ObjectType,
    /// `IF EXISTS`（清单外；字段保留）。
    pub missing_ok: bool,
    /// 位置。
    pub location: Location,
}

/// 可 `DROP` 的对象类型（PG `ObjectType` 的子集 + **本库扩展**）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectType {
    /// 表。
    Table,
    /// 索引。
    Index,
    /// 图（本库扩展）。
    Graph,
    /// 工作区（本库扩展，仅 admin）。
    Workspace,
}

/// 事务控制（PG `TransactionStmt` 的裁剪）。
#[derive(Debug, Clone, PartialEq)]
pub struct TransactionStmt {
    /// 种类。
    pub kind: TransactionStmtKind,
    /// 位置。
    pub location: Location,
}

/// 事务控制种类（PG `TransactionStmtKind` 的子集；`SAVEPOINT` 在清单外）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionStmtKind {
    /// `BEGIN`
    Begin,
    /// `COMMIT`
    Commit,
    /// `ROLLBACK`
    Rollback,
}

// ─────────────────────────── 本库扩展的 DDL ───────────────────────────

/// `CREATE GRAPH`（本库扩展；形状仿 PG 的 DDL 节点）。
#[derive(Debug, Clone, PartialEq)]
pub struct CreateGraphStmt {
    /// 图名。
    pub graph: RangeVar,
    /// 位置。
    pub location: Location,
}

/// `CREATE WORKSPACE <名> [DEFAULT FILESYSTEM <fs>] [FROM TEMPLATE '…'] [QUOTA … ON FILESYSTEM …]…`
/// （本库扩展，仅 admin；`DCL语句设计` §1.3 的 W1/W2）。
///
/// **无属主**：工作区在创建时是**无主容器**，属主由 `CREATE USER … USING WORKSPACE` 绑定
/// （依赖顺序 `FS → WORKSPACE → USER`；旧的 `FOR USER` 形式已删除）。
#[derive(Debug, Clone, PartialEq)]
pub struct CreateWorkspaceStmt {
    /// 工作区名（**实例内唯一**——创建时还没有属主，没法用属主限定）。
    pub name: String,
    /// `DEFAULT FILESYSTEM <fs_ref>`（缺省 = 实例默认盘）。
    pub default_fs: Option<FsRef>,
    /// `FROM TEMPLATE '…'`（由模板克隆；W2）。
    pub from_template: Option<Vec<u8>>,
    /// `QUOTA <量>|UNLIMITED ON FILESYSTEM <fs_ref>`（**盘级**配额；可多个）。
    pub quotas: Vec<FsQuota>,
    /// 位置。
    pub location: Location,
}

/// **盘级配额项**：`QUOTA <量>|UNLIMITED ON FILESYSTEM <fs_ref>`。
#[derive(Debug, Clone, PartialEq)]
pub struct FsQuota {
    /// 目标文件系统。
    pub fs: FsRef,
    /// 上限。
    pub amount: QuotaAmount,
    /// 位置。
    pub location: Location,
}

/// 配额的量。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaAmount {
    /// 字节数。
    Bytes(u64),
    /// `UNLIMITED`。
    Unlimited,
}

/// `CREATE FILESYSTEM <名> USING '<路径>'`（F1；本库扩展）。
///
/// **名字是标识**（供 `DEFAULT FILESYSTEM <名>` 引用），**路径只是创建参数**；
/// 路径可以是独立挂载的文件系统，**也可以只是一个目录**。
#[derive(Debug, Clone, PartialEq)]
pub struct CreateFilesystemStmt {
    /// 文件系统名（实例内唯一）。
    pub name: String,
    /// 实际路径。
    pub path: Vec<u8>,
    /// 位置。
    pub location: Location,
}

/// `ALTER FILESYSTEM <名> SET ALLOCATE = ON | OFF`（F2）。
#[derive(Debug, Clone, PartialEq)]
pub struct AlterFilesystemStmt {
    /// 目标文件系统。
    pub fs: FsRef,
    /// 分配开关（退役排水阀）。
    pub allocate: bool,
    /// 位置。
    pub location: Location,
}

/// `DROP FILESYSTEM <名>`（F3）。
#[derive(Debug, Clone, PartialEq)]
pub struct DropFilesystemStmt {
    /// 目标文件系统。
    pub fs: FsRef,
    /// 位置。
    pub location: Location,
}

/// `CREATE USER <主体> IDENTIFIED BY '<口令>' USING WORKSPACE <work_ref>`（U1）。
#[derive(Debug, Clone, PartialEq)]
pub struct CreateUserStmt {
    /// 主体名（实例内唯一）。
    pub name: String,
    /// 口令明文（**只在认证路径使用；不落库、不进日志**——落库的是散列）。
    pub password: Vec<u8>,
    /// 绑定的工作区（**必选**：有了工作区才能建用户）。
    pub using_workspace: WorkRef,
    /// 位置。
    pub location: Location,
}

/// `ALTER USER <主体> …`（U2–U6）。
#[derive(Debug, Clone, PartialEq)]
pub struct AlterUserStmt {
    /// 主体名。
    pub name: String,
    /// 动作。
    pub action: AlterUserAction,
    /// 位置。
    pub location: Location,
}

/// `ALTER USER` 的动作（闭集）。
#[derive(Debug, Clone, PartialEq)]
pub enum AlterUserAction {
    /// `IDENTIFIED BY '<新>' [EXPIRE]`——**admin 重置口令**（遗忘找回）。
    SetPassword {
        /// 新口令。
        new: Vec<u8>,
        /// 置"下次登录必须改密"。
        expire: bool,
    },
    /// `IDENTIFIED BY '<新>' REPLACE '<旧>'`——**本人改密**。
    ReplacePassword {
        /// 新口令。
        new: Vec<u8>,
        /// 旧口令。
        old: Vec<u8>,
    },
    /// `PAUSE` / `RESUME`（≈ Oracle `ACCOUNT LOCK|UNLOCK`）。
    SetPaused(bool),
    /// `USING WORKSPACE <work_ref>`——加绑（1 用户 : N 工作区）。
    UsingWorkspace(WorkRef),
    /// `DROP WORKSPACE <work_ref>`——解绑（区变无主）。
    DropWorkspace(WorkRef),
}

/// `DROP USER <主体> [CASCADE]`（U7）。
#[derive(Debug, Clone, PartialEq)]
pub struct DropUserStmt {
    /// 主体名。
    pub name: String,
    /// 连其工作区一起删（否则名下有区即拒绝）。
    pub cascade: bool,
    /// 位置。
    pub location: Location,
}

/// **工作区引用**（`DCL语句设计` §1.1）：`整数 | Str`（**名字实例内唯一**——
/// 工作区在创建时还没有属主，故**没有 `FOR USER` 限定**；名字查找在 **② 绑定期**，
/// REQ-SQL-002：解析不 import 目录）。
#[derive(Debug, Clone, PartialEq)]
pub struct WorkRef {
    /// `workspace_id`（整数形态）。
    pub id: Option<u64>,
    /// 名字（字符串形态）。
    pub name: Option<Vec<u8>>,
    /// 位置。
    pub location: Location,
}

/// **文件系统引用**（`DCL语句设计` §1.1）：`整数（池槽位） | Str（文件系统名）`。
///
/// **路径不是引用位**：`USING '<路径>'` 只在创建时出现，之后一律用名字或槽位。
#[derive(Debug, Clone, PartialEq)]
pub struct FsRef {
    /// 槽位号（整数形态）。
    pub slot: Option<u32>,
    /// 文件系统名（字符串形态）。
    pub name: Option<Vec<u8>>,
    /// 位置。
    pub location: Location,
}

/// `ALTER SESSION SET/CLEAR`（PG `VariableSetStmt`；`ALTER SESSION SET` 与
/// `SET` 同节点——本库**只开** `ALTER SESSION` 拼写，REQ-SQL-006 的会话面）。
#[derive(Debug, Clone, PartialEq)]
pub struct VariableSetStmt {
    /// `SET` / `CLEAR`。
    pub kind: VariableSetKind,
    /// 参数名（未引号折叠小写；**白名单在 ② 判**——解析不判）。
    pub name: String,
    /// `SET` 的值（`CLEAR` 为空；内存量照 PG：`'64MB'` / `'4GiB'` / 纯数字）。
    pub args: Vec<AConst>,
    /// 位置。
    pub location: Location,
}

/// 会话变量动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VariableSetKind {
    /// `SET`。
    Set,
    /// `CLEAR`（回默认）。
    Clear,
}

/// `ALTER DATABASE …`（PG `AlterDatabaseStmt` 的**无库名**形态——本库实例即
/// 一个"库"，`DCL语句设计` §2.2 记档）。
#[derive(Debug, Clone, PartialEq)]
pub struct AlterDatabaseStmt {
    /// 动作。
    pub action: AlterDatabaseAction,
    /// 位置。
    pub location: Location,
}

/// `ALTER DATABASE` 的动作（闭集：只剩模板两条——**克隆移到
/// `CREATE WORKSPACE … FROM TEMPLATE`（W2），原地转模板移到
/// `ALTER WORKSPACE … TO TEMPLATE`（W7）**：一件事不设两个入口）。
#[derive(Debug, Clone, PartialEq)]
pub enum AlterDatabaseAction {
    /// `ADD TEMPLATE '<名>' FROM <work_ref>`（由源区制作模板；源区不动）。
    AddTemplate {
        /// 模板名。
        name: Vec<u8>,
        /// 源工作区。
        from: WorkRef,
        /// Explicitly include committed graph entities; business tables remain empty.
        graph_data: bool,
    },
    /// `DROP TEMPLATE '<名>'`（克隆是复制、不建依赖 ⇒ 无前置）。
    DropTemplate {
        /// 模板名。
        name: Vec<u8>,
    },
}

/// `ALTER WORKSPACE …`（本库扩展，仅 admin）。
#[derive(Debug, Clone, PartialEq)]
pub struct AlterWorkspaceStmt {
    /// 工作区引用（`id` 或名字——§1.1 的三条规则在 ② 判）。
    pub workspace: WorkRef,
    /// 动作。
    pub action: AlterWorkspaceAction,
    /// 位置。
    pub location: Location,
}

/// `ALTER WORKSPACE` 的动作（闭集；W3–W7）。
#[derive(Debug, Clone, PartialEq)]
pub enum AlterWorkspaceAction {
    /// Change the daemon's effective access state. Success requires an
    /// instance-controller acknowledgement; this is not a catalog-only flag.
    Open(WorkspaceOpenMode),
    /// Verify actual media/catalog state and clear exactly one recovery scope.
    VerifyRecovery(RecoveryVerifyScope),
    /// `ADD FILESYSTEM <fs_ref> [QUOTA …]`——**扩盘**（W3）。
    AddFilesystem {
        /// 目标文件系统。
        fs: FsRef,
        /// 同时给的盘级配额（可选）。
        quota: Option<FsQuota>,
    },
    /// `SET DEFAULT FILESYSTEM <fs_ref>`——新数据文件默认落哪块盘（W4）。
    SetDefaultFilesystem {
        /// 目标文件系统。
        fs: FsRef,
    },
    /// `TO TEMPLATE '<名>'`——原地转模板（W7）。
    ToTemplate {
        /// 模板名。
        name: Vec<u8>,
    },
    /// `SET NAME = '<新名>'`（W5；**无 `NULL` 形态**——v0.2 BNF 只有 `Eq Str`）。
    SetName(Vec<u8>),
    /// `SET QUOTA (…)`（`DefElem` 列表——与 `WITH` 选项同形）。
    SetQuota(Vec<DefElem>),
}

/// Scope accepted by the online recovery verification command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryVerifyScope {
    /// One physical page `(file_id, block_id)`.
    Page {
        /// Data-file number.
        file_id: u16,
        /// Block number within the file.
        block_id: u32,
    },
    /// One workspace-local catalog object.
    Object {
        /// Catalog object number.
        object_id: u32,
    },
}

/// Runtime access state requested by `ALTER WORKSPACE ... OPEN ...`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceOpenMode {
    /// Admit reads while rejecting every business write.
    ReadOnly,
    /// Re-run the normal recovery gate before admitting writes.
    ReadWrite,
    /// Private workspace only: audited, explicitly forced recovery.
    ReadWriteForce,
}

/// CYPHER graph 'query' [PARAMETERS 'JSON object'] [BUDGETS 'JSON object'].
#[derive(Debug, Clone, PartialEq)]
pub struct CypherStmt {
    /// Current-workspace graph name.
    pub graph: String,
    /// PROFILE runs a read-only query and reports actual index access counters.
    pub profile: bool,
    /// Cypher source text, independent of the SQL lexer.
    pub query: String,
    /// JSON object containing typed named Cypher parameters.
    pub parameters: String,
    /// Request budgets may only tighten workspace configuration.
    pub budgets: String,
    /// Source location.
    pub location: Location,
}

/// Fixed source path for a graph full-text field.
#[derive(Debug, Clone, PartialEq)]
pub enum GraphTextPathPart {
    /// Object property name; quoted names retain case.
    Key(String),
    /// Fixed JSON array position.
    Index(usize),
}
/// Graph-owned index lifecycle and full-text search.
#[derive(Debug, Clone, PartialEq)]
pub enum GraphIndexAction {
    /// Create a typed composite or label-only property tree.
    Create {
        /// Nodes or relationships.
        entity: String,
        /// Label/type restriction, or every entity.
        label: Option<String>,
        /// Property paths (JSON object subfields use dotted paths).
        fields: Vec<Vec<String>>,
        /// Enforce unique scalar, non-null tuples.
        unique: bool,
    },
    /// Drop a graph-owned property tree.
    Drop,
    /// Atomically rebuild into a fresh tree.
    Rebuild,
    /// Rebuild the graph's protected node and directional adjacency trees.
    RebuildAccess,
    /// Atomically migrate legacy heap edges to native adjacency storage.
    UpgradeStorage,
    /// Rebuild native physical routes, retaining old snapshot generations.
    RebuildStorage,
    /// List graph-owned property indexes.
    Show,
    /// Create a native full-text index; explicit maintenance policy is validated at execution.
    FulltextCreate {
        /// Nodes or relationships.
        entity: String,
        /// Any matching label/type.
        labels: Vec<String>,
        /// Fixed JSON object/array paths.
        fields: Vec<Vec<GraphTextPathPart>>,
        /// JSON maintenance options.
        options: String,
    },
    /// List graph-owned full-text indexes.
    FulltextShow,
    /// Search a named full-text index with explicit domain and consistency options.
    FulltextSearch {
        /// UTF-8 query text.
        query: String,
        /// JSON query options.
        options: String,
    },
    /// Reconcile changed documents and append postings in a native transaction.
    FulltextSync,
    /// Drive bounded batches toward a frozen source watermark; return explicit status.
    FulltextWait {
        /// Optional target_source_seq and timeout_ms as JSON.
        options: String,
    },
    /// Full reanalysis and fresh native segments, distinct from incremental SYNC.
    FulltextRebuild,
    /// Persistently pause a managed full-text consumer, retaining pending markers.
    FulltextPause,
    /// Resume a paused managed full-text consumer.
    FulltextResume,
    /// Replace only the maintenance policy without rebuilding documents or trees.
    FulltextConfigure {
        /// Complete JSON policy: explicit update mode and optional overrides.
        options: String,
    },
    /// Drop the full-text index and its protected document store.
    FulltextDrop,
}
/// Raw graph-index statement: object resolution and field validation are later.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphIndexStmt {
    /// Current-workspace graph name.
    pub graph: String,
    /// Index name (absent for SHOW).
    pub name: Option<String>,
    /// Requested lifecycle action.
    pub action: GraphIndexAction,
    /// Source location.
    pub location: Location,
}
