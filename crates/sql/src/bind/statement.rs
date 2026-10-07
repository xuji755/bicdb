//! **语句绑定**（`SQL前端设计` §4；S3 的语句部分）。
//!
//! ```text
//! Raw AST ──绑定──▶ BoundStatement
//!   SELECT   ：单表 FROM + WHERE + ORDER BY + LIMIT/OFFSET（投影列带类型）
//!   INSERT   ：目标表解析 + 列清单 + VALUES 行（每列带目标形态）
//!   DDL      ：CREATE TABLE / CREATE [UNIQUE] INDEX / DROP TABLE / DROP INDEX
//!   BEGIN/COMMIT/ROLLBACK：会话层直通（绑定只做透传）
//! ```
//!
//! **写目标检查**（§4.4）：写目标不得是固定表 / `asset$` / `ref$` / `audit` /
//! 自举对象 / `public` 工作区（写侧只有标准接口）。固定表在第 ① 格就查不到，
//! 其余在 [`check_writable`] 里具名拒绝。
//!
//! **本切片不做**（S3 首版的边界，逐条记档）：多表/连接/子查询、聚合（`GROUP BY`
//! /聚合函数）、集合运算、`DISTINCT`、表达式索引、`WITH` 表选项全集、显式
//! `CASE/COALESCE`、`INSERT … SELECT`——全部**绑定期具名拒绝**（不静默）。

use bicdb_catalog::ddl::{ColumnSpec, IndexSpec, TableOptions, TableSpec};
use bicdb_catalog::ColTypeCode;
use bicdb_exec::{ColKind, Expr as PlanExpr, RowShape};

use super::expr::{bind_expr, kind_name, BindScope, BoundColumn, BoundParams};
use super::{BindError, CatalogObject, CatalogView, NameResolver, NameSpace, ResolvedName};
use crate::ast::{
    self, DefElemArg, Expr, FromItem, InsertStmt, ObjectType, SelectStmt, SortByDir, Stmt,
};

/// 一条绑定后的语句。
#[derive(Debug, Clone, PartialEq)]
pub enum BoundStatement {
    /// `SELECT`（单表；投影 + 过滤 + 排序 + 限行）。
    Select(BoundSelect),
    /// `INSERT … VALUES`。
    Insert(BoundInsert),
    /// DDL（目录写侧直通）。
    Ddl(BoundDdl),
    /// **集合运算**（两侧都是 `SELECT`）。
    SetOp(BoundSetOp),
    /// `UPDATE`（单表；SET + WHERE）。
    Update(BoundUpdate),
    /// `DELETE`（单表；WHERE）。
    Delete(BoundDelete),
    /// 事务控制（会话层直通）。
    Transaction(ast::TransactionStmtKind),
    /// **DCL 管理语句**（`doc/DCL语句设计_v0.1.md` v0.2：F/W/U/T 四组）。
    ///
    /// **绑定只做搬运**：DCL 不吃名字解析、不吃列/类型——对象查找、资格检查
    /// 与语义校验都在**执行层**（`crate::dcl_exec`），那里才拿得到目录写侧、
    /// 实例注册表（全局控制文件）与文件系统。
    Dcl(Box<ast::Stmt>),
}

/// 绑定后的 `SELECT`。
#[derive(Debug, Clone, PartialEq)]
pub struct BoundSelect {
    /// FROM 面的表（1 或 2 张）。
    pub tables: Vec<BoundTable>,
    /// 连接（单表时 `None`）。
    pub join: Option<BoundJoin>,
    /// 输出列（名 + 形态）。
    pub columns: Vec<BoundColumn>,
    /// 投影表达式（对表行求值；顺序 = 输出列序）。
    pub projection: Vec<PlanExpr>,
    /// `WHERE`（None = 全放行）。
    pub filter: Option<PlanExpr>,
    /// 排序键（见 [`BoundSortKey`]：输出列**或**输入表达式）。
    pub sort: Vec<BoundSortKey>,
    /// `LIMIT`。
    pub limit: Option<u64>,
    /// `OFFSET`。
    pub offset: u64,
    /// `DISTINCT`。
    pub distinct: bool,
    /// **`GROUP BY` 键**（空 = 没有分组；非空 ⇒ 走 `HashAgg`）。
    pub groups: Vec<PlanExpr>,
    /// **聚合项**（空 = 不是聚合查询）。
    ///
    /// **提取规则**（`extract_aggregates`）：SELECT 列表与 HAVING 里的
    /// `COUNT/SUM/AVG/MIN/MAX` 各抽成一个聚合项，原位置换成对**聚合输出行**
    /// 的列引用；投影与 HAVING 因此都变成"对聚合输出行求值"的表达式。
    pub aggs: Vec<bicdb_exec::AggSpec>,
    /// `HAVING`（**对聚合输出行**求值：`groups ++ aggs` 的列序）。
    pub having: Option<PlanExpr>,
    /// 参数清单（顺序即执行期参数序）。
    pub params: BoundParams,
}

/// 集合运算种类（`UNION`/`INTERSECT`/`EXCEPT`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundSetKind {
    /// `UNION [ALL]`。
    Union,
    /// `INTERSECT [ALL]`。
    Intersect,
    /// `EXCEPT [ALL]`。
    Except,
}

/// 绑定后的**集合运算**（`UNION`/`INTERSECT`/`EXCEPT`；左深嵌套）。
#[derive(Debug, Clone, PartialEq)]
pub struct BoundSetOp {
    /// 种类（**自有枚举**：UNION 走 `Append`（+/`Unique`），INTERSECT/EXCEPT 走
    /// exec 的 `SetOp`——那个只有后两者）。
    pub kind: BoundSetKind,
    /// `ALL`。
    pub all: bool,
    /// 左、右。
    pub left: Box<BoundStatement>,
    /// 右侧。
    pub right: Box<BoundStatement>,
    /// 输出列数（两侧必须一致）。
    pub width: usize,
}

/// 绑定后的 `INSERT`。
#[derive(Debug, Clone, PartialEq)]
pub struct BoundInsert {
    /// 目标表。
    pub table: CatalogObject,
    /// 表列数（`encode_row` 的形状宽度）。
    pub table_shape: RowShape,
    /// 目标列号（0 起；按 `INSERT` 的列清单序）。
    pub target_cols: Vec<usize>,
    /// 行（每行 = 与 `target_cols` 等长的表达式）。
    ///
    /// **`INSERT … SELECT` 时为空**：来源是 [`BoundInsert::source_select`]，
    /// 执行期把它的结果行变成字面量再走同一条插入路径。
    pub rows: Vec<Vec<PlanExpr>>,
    /// `INSERT … SELECT` 的来源（**本版：物化后插入**——先跑一遍 SELECT 收齐行，
    /// 再当字面量插入；复用唯一性预检与索引维护的整条路）。
    pub source_select: Option<Box<BoundStatement>>,
    /// 参数清单。
    pub params: BoundParams,
}

/// 绑定的表（FROM 的一项）。
#[derive(Debug, Clone, PartialEq)]
pub struct BoundTable {
    /// 表名（限定名用）。
    pub name: String,
    /// 别名（限定名优先用它）。
    pub alias: Option<String>,
    /// 目录对象（**固定表**这里是合成形态：`obj = 0`、不进字典、无段）。
    pub obj: CatalogObject,
    /// **固定表名**（`None` = 普通表）：行在查询期由引擎即时产生，**没有段**。
    pub fixed: Option<&'static str>,
    /// 列（`source` = 别名或表名）。
    pub columns: Vec<BoundColumn>,
    /// 行形状。
    pub shape: RowShape,
}

/// 连接（**本版两表**：`INNER`/`LEFT`；逗号连接 = 无 `ON` 的内连接）。
#[derive(Debug, Clone, PartialEq)]
pub struct BoundJoin {
    /// 连接类型。
    pub kind: bicdb_exec::JoinKind,
    /// `ON` 条件（对合并行求值；`None` = 笛卡尔）。
    pub qual: Option<PlanExpr>,
}

/// 绑定后的 FROM。
#[derive(Debug, Clone, PartialEq)]
pub struct BoundFrom {
    /// 参与的表（1 = 单表；2 = 两表连接）。
    pub tables: Vec<BoundTable>,
    /// 连接（单表时为 `None`）。
    pub join: Option<BoundJoin>,
    /// `ON` 的 **AST**（还没绑——它要在"合并作用域"里绑，调用方才有）。
    pub on_clause: Option<ast::Expr>,
}

/// 一条 `RangeVar` ⇒ 绑定的表（含别名与列枚举）。
fn bind_range_var<V: CatalogView>(
    resolver: &mut NameResolver<'_, V>,
    rv: &ast::RangeVar,
) -> Result<BoundTable, BindError> {
    let table_name = rv.relname.clone();
    let mut fixed: Option<&'static str> = None;
    let table = match resolver.resolve_table(&table_name)? {
        ResolvedName::Object(o) => o,
        // **固定表**（第 ② 格）：只读、无段、无版本——列从清单取。
        ResolvedName::FixedTable(n) => {
            fixed = Some(n);
            CatalogObject {
                obj: 0,
                name: n.to_owned(),
                namespace: NameSpace::Table,
                type_code: bicdb_catalog::obj_kind::TABLE,
                dataobj: 0,
                status: 1,
                mtime: 0,
            }
        }
    };
    if table.type_code != bicdb_catalog::obj_kind::TABLE {
        return Err(BindError::Unsupported(format!(
            "`{table_name}` 不是表（type# {}）",
            table.type_code
        )));
    }
    let qual = rv
        .alias
        .as_ref()
        .map_or_else(|| table_name.clone(), |a| a.aliasname.clone());
    // 固定表的列从**清单**取（不查字典——它压根不在字典里）。
    let cols = match fixed {
        Some(n) => resolver
            .view()
            .fixed_columns(n)?
            .ok_or_else(|| BindError::NotFound {
                name: table_name.clone(),
                ns: NameSpace::Table,
            })?,
        None => resolver.view().columns(table.obj)?,
    };
    let columns: Vec<BoundColumn> = cols
        .iter()
        .map(|c| BoundColumn {
            name: c.name.clone(),
            kind: col_kind(c.type_code).unwrap_or(ColKind::Bytes),
            nullable: c.nullable,
            table_col: Some(c.col),
            source: Some(qual.clone()),
        })
        .collect();
    let shape = RowShape::new(columns.iter().map(|c| c.kind).collect());
    Ok(BoundTable {
        name: table_name,
        alias: rv.alias.as_ref().map(|a| a.aliasname.clone()),
        obj: table,
        fixed,
        columns,
        shape,
    })
}

/// **FROM 解析**（本版：单表 / 两表连接 / 逗号连接）。
///
/// 三表及以上 ⇒ 具名拒绝（连接树要递归绑定与计划，随 JOIN 的后续切片）。
fn resolve_from<V: CatalogView>(
    resolver: &mut NameResolver<'_, V>,
    from: &[FromItem],
) -> Result<BoundFrom, BindError> {
    match from {
        [FromItem::RangeVar(rv)] => Ok(BoundFrom {
            tables: vec![bind_range_var(resolver, rv)?],
            join: None,
            on_clause: None,
        }),
        [FromItem::Join(je)] => match (&je.larg, &je.rarg) {
            (FromItem::RangeVar(l), FromItem::RangeVar(r)) => {
                let lt = bind_range_var(resolver, l)?;
                let rt = bind_range_var(resolver, r)?;
                // **限定名不许重**（两表同名/别名相撞 ⇒ 引用无从消歧）。
                let lq = lt.alias.clone().unwrap_or_else(|| lt.name.clone());
                let rq = rt.alias.clone().unwrap_or_else(|| rt.name.clone());
                if lq == rq {
                    return Err(BindError::Unsupported(format!(
                        "两张表的限定名都是 `{lq}`——给其中一个起别名"
                    )));
                }
                let kind = match je.jointype {
                    ast::JoinType::Inner => bicdb_exec::JoinKind::Inner,
                    ast::JoinType::Left => bicdb_exec::JoinKind::Left,
                };
                Ok(BoundFrom {
                    tables: vec![lt, rt],
                    join: Some(BoundJoin { kind, qual: None }),
                    on_clause: je.quals.as_deref().cloned(),
                })
            }
            _ => Err(BindError::Unsupported(
                "嵌套连接（`JOIN` 的任一侧又是 `JOIN`）：本版只做两表".to_owned(),
            )),
        },
        // **无 `FROM`**：`SELECT 1` 这类（单行源；投影里不能有列引用——
        // 作用域空 ⇒ 引用列自然报"列不存在"）。
        [] => Ok(BoundFrom {
            tables: Vec::new(),
            join: None,
            on_clause: None,
        }),
        [FromItem::RangeVar(a), FromItem::RangeVar(b)] => {
            let lt = bind_range_var(resolver, a)?;
            let rt = bind_range_var(resolver, b)?;
            Ok(BoundFrom {
                tables: vec![lt, rt],
                join: Some(BoundJoin {
                    kind: bicdb_exec::JoinKind::Inner,
                    qual: None,
                }),
                on_clause: None,
            })
        }
        _ => Err(BindError::Unsupported(
            "三张表以上 / 复杂 FROM：本版只做两表连接".to_owned(),
        )),
    }
}

/// 绑定后的 `UPDATE`（**单表**；SET 表达式对**原行**求值）。
#[derive(Debug, Clone, PartialEq)]
pub struct BoundUpdate {
    /// 目标表。
    pub table: CatalogObject,
    /// 表的行形状。
    pub table_shape: RowShape,
    /// `SET 列 = 表达式`（列号 0 起；同一列写两次 ⇒ 绑定期拒绝）。
    pub sets: Vec<(usize, PlanExpr)>,
    /// `WHERE`（对**表行**求值；`None` = 全表）。
    pub filter: Option<PlanExpr>,
    /// 参数清单。
    pub params: BoundParams,
}

/// 绑定后的 `DELETE`（**单表**）。
#[derive(Debug, Clone, PartialEq)]
pub struct BoundDelete {
    /// 目标表。
    pub table: CatalogObject,
    /// 表的行形状。
    pub table_shape: RowShape,
    /// `WHERE`（对**表行**求值；`None` = 全表）。
    pub filter: Option<PlanExpr>,
    /// 参数清单。
    pub params: BoundParams,
}

/// 绑定后的 DDL（目录写侧直通的规格）。
#[derive(Debug, Clone, PartialEq)]
pub enum BoundDdl {
    /// `CREATE TABLE`。
    CreateTable(TableSpec),
    /// `CREATE [UNIQUE] INDEX`。
    CreateIndex(IndexSpec),
    /// `DROP TABLE`。
    DropTable(String),
    /// `DROP INDEX`。
    DropIndex(String),
}

/// **聚合占位列的列名前缀**（不可能来自词法：含控制字符——用户打不出来，
/// 解析器也不会产生）。
const AGG_PLACEHOLDER_PREFIX: char = '\u{1}';

/// 占位列名。
fn agg_placeholder(k: usize) -> String {
    format!("{AGG_PLACEHOLDER_PREFIX}agg{k}")
}

/// **聚合提取的上下文**（提取器 ↔ 作用域之间的桥）。
struct AggCtx<'a> {
    /// 表的列（聚合实参按它绑）。
    table_columns: &'a [BoundColumn],
    /// 参数清单（聚合实参里的 `:name` 也占位）。
    params: &'a mut BoundParams,
    /// 聚合项（按出现序）。
    aggs: &'a mut Vec<bicdb_exec::AggSpec>,
    /// 占位列（追加进作用域；`table_col` 用不到，占位名唯一即可）。
    placeholders: &'a mut Vec<BoundColumn>,
}

impl AggCtx<'_> {
    /// 一个聚合调用 ⇒ 占位列引用（并把聚合项压进清单）。
    fn note_agg(&mut self, f: &ast::FuncCall) -> Result<ast::Expr, BindError> {
        let kind = match f.funcname.as_str() {
            "count" => bicdb_exec::AggKind::Count,
            "sum" => bicdb_exec::AggKind::Sum,
            "avg" => bicdb_exec::AggKind::Avg,
            "min" => bicdb_exec::AggKind::Min,
            "max" => bicdb_exec::AggKind::Max,
            other => {
                return Err(BindError::Unsupported(format!(
                    "聚合 `{other}` 不在闭集内（COUNT/SUM/AVG/MIN/MAX）"
                )))
            }
        };
        // `COUNT(*)` 是**独立形态**（数所有行，含全 NULL 行）——不是"实参缺省"。
        let kind = if f.agg_star {
            if !matches!(kind, bicdb_exec::AggKind::Count) {
                return Err(BindError::Unsupported(format!(
                    "`{}(*)`：只有 COUNT 支持 `*`",
                    f.funcname.to_uppercase()
                )));
            }
            bicdb_exec::AggKind::CountStar
        } else {
            kind
        };
        let mut arg_kind: Option<ColKind> = None;
        let arg = if f.agg_star {
            None
        } else {
            match f.args.as_slice() {
                [a] => {
                    let scope = BindScope {
                        table_columns: self.table_columns,
                        output_names: &Default::default(),
                    };
                    let (e, k) = bind_expr(a, &scope, self.params, None)?;
                    arg_kind = Some(k);
                    // 聚合的实参形态：COUNT 收任意；SUM/AVG 要数值（比较/文本会被
                    // 运行期拒，这里先给**绑定期**的具名拒绝）；MIN/MAX 收可比形态。
                    if matches!(kind, bicdb_exec::AggKind::Sum | bicdb_exec::AggKind::Avg)
                        && k != ColKind::Number
                    {
                        return Err(BindError::TypeMismatch {
                            what: f.funcname.to_uppercase(),
                            want: "NUMBER",
                            got: kind_name(k),
                        });
                    }
                    Some(e)
                }
                [] => {
                    return Err(BindError::Unsupported(format!(
                        "`{}` 缺实参（要写 `{}(*)` 还是 `{}(表达式)`？）",
                        f.funcname.to_uppercase(),
                        f.funcname.to_uppercase(),
                        f.funcname.to_uppercase()
                    )))
                }
                _ => {
                    return Err(BindError::Unsupported(format!(
                        "`{}` 只收一个实参",
                        f.funcname.to_uppercase()
                    )))
                }
            }
        };
        // 结果形态：COUNT/COUNT(*)/SUM/AVG ⇒ NUMBER；MIN/MAX ⇒ **实参的形态**
        // （取最小/最大不改类型：文本按字典序、布尔 false 优先）。
        let out_kind = match kind {
            bicdb_exec::AggKind::Count
            | bicdb_exec::AggKind::CountStar
            | bicdb_exec::AggKind::Sum
            | bicdb_exec::AggKind::Avg => ColKind::Number,
            bicdb_exec::AggKind::Min | bicdb_exec::AggKind::Max => {
                arg_kind.unwrap_or(ColKind::Number)
            }
        };
        let k = self.aggs.len();
        self.aggs.push(bicdb_exec::AggSpec {
            kind,
            arg,
            distinct: f.agg_distinct,
        });
        let name = agg_placeholder(k);
        self.placeholders.push(BoundColumn {
            name: name.clone(),
            kind: out_kind,
            nullable: true,
            table_col: None,
            source: None,
        });
        Ok(ast::Expr::ColumnRef(ast::ColumnRef {
            fields: vec![ast::ColumnRefField::Name(name)],
            location: f.location,
        }))
    }
}

/// 绑定后的尾子句：`(排序键, LIMIT, OFFSET)`。
type BoundTail = (Vec<BoundSortKey>, Option<u64>, u64);

/// **一条排序键**（`spec/SQL.md`：`ORDER BY <表达式> [ASC|DESC]`）。
///
/// **两种目标**（语法上是"表达式"，只是物理落点不同）：
/// - **输出列**（名字或序号）：排在**投影之上**（最省事，键就是输出行坐标）；
/// - **输入行上的表达式**：排在**投影之下**——不投影的列也能作排序键
///   （`SELECT name FROM emp ORDER BY sal DESC` 这种最自然的写法）。
#[derive(Debug, Clone, PartialEq)]
pub struct BoundSortKey {
    /// 排序目标。
    pub spec: SortSpec,
    /// 降序。
    pub desc: bool,
}

/// 排序键的目标（见 [`BoundSortKey`]）。
#[derive(Debug, Clone, PartialEq)]
pub enum SortSpec {
    /// 输出列（投影后行序）：`ORDER BY <输出名|序号>`。
    Output(usize),
    /// **输入行上的表达式**（不投影的列也在这里）——计划期移到投影之下。
    Input(PlanExpr),
}

/// **`ORDER BY` / `LIMIT` / `OFFSET` 绑定**（`SELECT` 尾子句；集合运算外层共用）。
///
/// **口径**：`ORDER BY` 先试**输出列名/序号**（最直观），落空再按**输入行作用域**
/// 绑成表达式。`scope = None` ⇒ 只有输出列可用（集合运算的外层就是这样调的：
/// 合并后的结果没有"输入行"这回事）。
///
/// **错误文案例外**：名字两处都落空时点明"既不在输出列、也不在 FROM 里"——
/// 只说"不在输出列里"会让人去翻表定义（实测被误导过）。
#[allow(clippy::too_many_arguments)]
fn bind_tail(
    sort_clause: &[crate::ast::SortBy],
    limit_count: &Option<Box<Expr>>,
    limit_offset: &Option<Box<Expr>>,
    out_columns: &[BoundColumn],
    output_names: &std::collections::BTreeMap<String, usize>,
    scope: Option<&BindScope<'_>>,
    params: &mut BoundParams,
) -> Result<BoundTail, BindError> {
    // ORDER BY：序号 / 输出名 / **输入行表达式**（后者由计划期移到投影之下）。
    let mut sort = Vec::new();
    for sb in sort_clause {
        let spec = match &sb.node {
            Expr::AConst(c) => match c.value.as_ref() {
                Some(crate::ast::ConstValue::Int(text)) => {
                    let n: usize = text
                        .parse()
                        .map_err(|_| BindError::Unsupported("ORDER BY 序号".to_owned()))?;
                    if n == 0 || n > out_columns.len() {
                        return Err(BindError::Unsupported(format!(
                            "ORDER BY 序号 {n} 越出输出列（共 {} 列）",
                            out_columns.len()
                        )));
                    }
                    SortSpec::Output(n - 1)
                }
                _ => return Err(BindError::Unsupported("ORDER BY 常量".to_owned())),
            },
            Expr::ColumnRef(cr) => {
                // 一段 `名` 或两段 `表.名`（`ORDER BY t.id` 是输出列 `id` 的
                // 来源限定写法；本版按末段名匹配）。
                let name = match cr.fields.as_slice() {
                    [ast::ColumnRefField::Name(n)] => n.clone(),
                    [ast::ColumnRefField::Name(_), ast::ColumnRefField::Name(n)] => n.clone(),
                    _ => return Err(BindError::Unsupported("ORDER BY 三段以上引用".to_owned())),
                };
                // ① 输出列名（最直观）——命中就用输出坐标（排在投影之上）。
                match output_names.get(&name) {
                    Some(i) => SortSpec::Output(*i),
                    // ② 落空 ⇒ 按**输入行作用域**绑（不投影的列也认）。
                    None => match scope {
                        Some(sc) => {
                            let (e, _) = bind_expr(&sb.node, sc, params, None).map_err(|_| {
                                BindError::Unsupported(format!(
                                    "ORDER BY：`{name}` 既不在 SELECT 的输出列里，也不是 FROM 里的列"
                                ))
                            })?;
                            SortSpec::Input(e)
                        }
                        None => {
                            return Err(BindError::Unsupported(format!(
                                "ORDER BY 只收输出列名或序号——`{name}` 不在输出列里"
                            )))
                        }
                    },
                }
            }
            // 一般表达式：按**输入行**绑（`ORDER BY sal * 2`）。
            other => {
                let sc = scope.ok_or_else(|| {
                    BindError::Unsupported(
                        "ORDER BY 表达式（集合运算的外层只收输出列名/序号）".to_owned(),
                    )
                })?;
                let (e, _) = bind_expr(other, sc, params, None)?;
                SortSpec::Input(e)
            }
        };
        sort.push(BoundSortKey {
            spec,
            desc: sb.sortby_dir == SortByDir::Desc,
        });
    }

    // LIMIT / OFFSET（只收整数字面量）。
    let limit = match &limit_count {
        None => None,
        Some(e) => Some(int_literal(e, "LIMIT")?),
    };
    let offset = match &limit_offset {
        None => 0,
        Some(e) => int_literal(e, "OFFSET")?,
    };

    Ok((sort, limit, offset))
}

/// **带聚合提取的表达式绑定**：先把聚合调用换成占位列，再按普通表达式绑定。
///
/// 返回的表达式在"**表行坐标**（表列 ++ 占位列）"下——调用方随后用
/// [`rewrite_to_agg_row`] 把它改写成"聚合输出行坐标"（`groups ++ aggs`）。
fn bind_expr_agg(
    e: &ast::Expr,
    table_columns: &[BoundColumn],
    params: &mut BoundParams,
    aggs: &mut Vec<bicdb_exec::AggSpec>,
    placeholders: &mut Vec<BoundColumn>,
) -> Result<(PlanExpr, ColKind), BindError> {
    let mut ctx = AggCtx {
        table_columns,
        params,
        aggs,
        placeholders,
    };
    let rewritten = extract_aggs_in(e, &mut ctx)?;
    // 作用域 = 表列 ++ 占位列（占位列排在后面 ⇒ 列号 = 表宽 + k）。
    let mut scope_cols: Vec<BoundColumn> = table_columns.to_vec();
    scope_cols.extend(placeholders.iter().cloned());
    let scope = BindScope {
        table_columns: &scope_cols,
        output_names: &Default::default(),
    };
    bind_expr(&rewritten, &scope, params, None)
}

/// 表达式里有没有"聚合输出列"（占位列改写后的列号 ≥ `table_len`）。
fn is_aggregated_expr(e: &PlanExpr, table_len: usize) -> bool {
    match e {
        PlanExpr::Column(i) => *i >= table_len,
        PlanExpr::Literal(_) | PlanExpr::Param(_) => false,
        PlanExpr::Arith { left, right, .. } | PlanExpr::Compare { left, right, .. } => {
            is_aggregated_expr(left, table_len) || is_aggregated_expr(right, table_len)
        }
        PlanExpr::NullIf { left, right } => {
            is_aggregated_expr(left, table_len) || is_aggregated_expr(right, table_len)
        }
        PlanExpr::Between { expr, low, high } => {
            is_aggregated_expr(expr, table_len)
                || is_aggregated_expr(low, table_len)
                || is_aggregated_expr(high, table_len)
        }
        PlanExpr::Neg(x) | PlanExpr::Not(x) | PlanExpr::Cast { expr: x, .. } => {
            is_aggregated_expr(x, table_len)
        }
        PlanExpr::IsNull { expr, .. } => is_aggregated_expr(expr, table_len),
        PlanExpr::Case { whens, else_ } => {
            whens
                .iter()
                .any(|(c, v)| is_aggregated_expr(c, table_len) || is_aggregated_expr(v, table_len))
                || else_
                    .as_ref()
                    .is_some_and(|x| is_aggregated_expr(x, table_len))
        }
        PlanExpr::InList { expr, list } => {
            is_aggregated_expr(expr, table_len)
                || list.iter().any(|x| is_aggregated_expr(x, table_len))
        }
        PlanExpr::Coalesce(list) | PlanExpr::And(list) | PlanExpr::Or(list) => {
            list.iter().any(|x| is_aggregated_expr(x, table_len))
        }
    }
}

/// **把 AST 里的聚合调用换成占位列引用**（递归改写；其余节点原样重建）。
fn extract_aggs_in(e: &ast::Expr, ctx: &mut AggCtx<'_>) -> Result<ast::Expr, BindError> {
    Ok(match e {
        Expr::FuncCall(f) if crate::bind::expr::is_aggregate_name(&f.funcname) => {
            ctx.note_agg(f)?
        }
        Expr::FuncCall(f) => ast::Expr::FuncCall(ast::FuncCall {
            args: f
                .args
                .iter()
                .map(|a| extract_aggs_in(a, ctx))
                .collect::<Result<_, _>>()?,
            ..f.clone()
        }),
        Expr::AExpr(a) => ast::Expr::AExpr(ast::AExpr {
            lexpr: a
                .lexpr
                .as_ref()
                .map(|l| extract_aggs_in(l, ctx).map(Box::new))
                .transpose()?,
            rexpr: a
                .rexpr
                .as_ref()
                .map(|r| extract_aggs_in(r, ctx).map(Box::new))
                .transpose()?,
            rexpr_list: a
                .rexpr_list
                .iter()
                .map(|r| extract_aggs_in(r, ctx))
                .collect::<Result<_, _>>()?,
            ..a.clone()
        }),
        Expr::BoolExpr(b) => ast::Expr::BoolExpr(ast::BoolExpr {
            args: b
                .args
                .iter()
                .map(|a| extract_aggs_in(a, ctx))
                .collect::<Result<_, _>>()?,
            ..b.clone()
        }),
        Expr::NullTest(n) => ast::Expr::NullTest(ast::NullTest {
            arg: Box::new(extract_aggs_in(&n.arg, ctx)?),
            ..n.clone()
        }),
        Expr::TypeCast(t) => ast::Expr::TypeCast(ast::TypeCast {
            arg: Box::new(extract_aggs_in(&t.arg, ctx)?),
            ..t.clone()
        }),
        Expr::CaseExpr(c) => ast::Expr::CaseExpr(ast::CaseExpr {
            arg: c
                .arg
                .as_ref()
                .map(|a| extract_aggs_in(a, ctx).map(Box::new))
                .transpose()?,
            args: c
                .args
                .iter()
                .map(|w| {
                    Ok(ast::CaseWhen {
                        expr: extract_aggs_in(&w.expr, ctx)?,
                        result: extract_aggs_in(&w.result, ctx)?,
                        location: w.location,
                    })
                })
                .collect::<Result<_, BindError>>()?,
            defresult: c
                .defresult
                .as_ref()
                .map(|d| extract_aggs_in(d, ctx).map(Box::new))
                .transpose()?,
            ..c.clone()
        }),
        Expr::CoalesceExpr(c) => ast::Expr::CoalesceExpr(ast::CoalesceExpr {
            args: c
                .args
                .iter()
                .map(|a| extract_aggs_in(a, ctx))
                .collect::<Result<_, _>>()?,
            ..c.clone()
        }),
        other => other.clone(),
    })
}

/// **把"表行坐标"的表达式改写成"聚合输出行坐标"**
/// （`groups ++ aggs`；`table_len` = 表列数，聚合占位列紧随其后）。
fn rewrite_to_agg_row(
    e: &PlanExpr,
    groups: &[PlanExpr],
    table_len: usize,
) -> Result<PlanExpr, BindError> {
    use PlanExpr as P;
    Ok(match e {
        P::Column(i) => {
            if *i < table_len {
                // 非聚合位置：**必须在 GROUP BY 里**（否则上面已拒）。
                let at = groups
                    .iter()
                    .position(|g| *g == P::Column(*i))
                    .ok_or_else(|| {
                        BindError::Unsupported(format!(
                            "列 #{} 既不在 GROUP BY 里、也不是聚合",
                            i + 1
                        ))
                    })?;
                P::Column(at)
            } else {
                P::Column(groups.len() + (*i - table_len))
            }
        }
        P::Literal(v) => P::Literal(v.clone()),
        P::Param(i) => P::Param(*i),
        P::Arith { op, left, right } => P::Arith {
            op: *op,
            left: Box::new(rewrite_to_agg_row(left, groups, table_len)?),
            right: Box::new(rewrite_to_agg_row(right, groups, table_len)?),
        },
        P::Neg(x) => P::Neg(Box::new(rewrite_to_agg_row(x, groups, table_len)?)),
        P::Cast { expr, to } => P::Cast {
            expr: Box::new(rewrite_to_agg_row(expr, groups, table_len)?),
            to: *to,
        },
        P::Case { whens, else_ } => P::Case {
            whens: whens
                .iter()
                .map(|(c, v)| {
                    Ok((
                        rewrite_to_agg_row(c, groups, table_len)?,
                        rewrite_to_agg_row(v, groups, table_len)?,
                    ))
                })
                .collect::<Result<_, BindError>>()?,
            else_: match else_ {
                None => None,
                Some(x) => Some(Box::new(rewrite_to_agg_row(x, groups, table_len)?)),
            },
        },
        P::InList { expr, list } => P::InList {
            expr: Box::new(rewrite_to_agg_row(expr, groups, table_len)?),
            list: list
                .iter()
                .map(|x| rewrite_to_agg_row(x, groups, table_len))
                .collect::<Result<_, _>>()?,
        },
        P::Between { expr, low, high } => P::Between {
            expr: Box::new(rewrite_to_agg_row(expr, groups, table_len)?),
            low: Box::new(rewrite_to_agg_row(low, groups, table_len)?),
            high: Box::new(rewrite_to_agg_row(high, groups, table_len)?),
        },
        P::Coalesce(list) => P::Coalesce(
            list.iter()
                .map(|x| rewrite_to_agg_row(x, groups, table_len))
                .collect::<Result<_, _>>()?,
        ),
        P::NullIf { left, right } => P::NullIf {
            left: Box::new(rewrite_to_agg_row(left, groups, table_len)?),
            right: Box::new(rewrite_to_agg_row(right, groups, table_len)?),
        },
        P::Compare { op, left, right } => P::Compare {
            op: *op,
            left: Box::new(rewrite_to_agg_row(left, groups, table_len)?),
            right: Box::new(rewrite_to_agg_row(right, groups, table_len)?),
        },
        P::IsNull { expr, negated } => P::IsNull {
            expr: Box::new(rewrite_to_agg_row(expr, groups, table_len)?),
            negated: *negated,
        },
        P::Not(x) => P::Not(Box::new(rewrite_to_agg_row(x, groups, table_len)?)),
        P::And(list) => P::And(
            list.iter()
                .map(|x| rewrite_to_agg_row(x, groups, table_len))
                .collect::<Result<_, _>>()?,
        ),
        P::Or(list) => P::Or(
            list.iter()
                .map(|x| rewrite_to_agg_row(x, groups, table_len))
                .collect::<Result<_, _>>()?,
        ),
    })
}

/// 一条语句的输出列数（集合运算两侧比它）。
fn select_width(b: &BoundStatement) -> Result<usize, BindError> {
    match b {
        BoundStatement::Select(s) => Ok(s.columns.len()),
        // **集合运算的结果也是一"段 SELECT"**（`A UNION B` 再和 `C` 运算的形态）。
        BoundStatement::SetOp(o) => Ok(o.width),
        _ => Err(BindError::Unsupported(
            "这里要一段 SELECT（或集合运算）".to_owned(),
        )),
    }
}

/// **单表 + 列枚举**（SELECT/UPDATE/DELETE 共用的取数面）。
fn single_table<V: CatalogView>(
    resolver: &mut NameResolver<'_, V>,
    rv: &ast::RangeVar,
    what: &str,
) -> Result<(CatalogObject, Vec<BoundColumn>, RowShape), BindError> {
    let table_name = {
        if rv.alias.is_some() {
            return Err(BindError::Unsupported("表别名".to_owned()));
        }
        rv.relname.clone()
    };
    let _ = what;
    let table = check_writable(resolver, &table_name)?;
    let cols = resolver.view().columns(table.obj)?;
    let table_columns: Vec<BoundColumn> = cols
        .iter()
        .map(|c| BoundColumn {
            name: c.name.clone(),
            kind: col_kind(c.type_code).unwrap_or(ColKind::Bytes),
            nullable: c.nullable,
            table_col: Some(c.col),
            source: Some(table_name.clone()),
        })
        .collect();
    let shape = RowShape::new(table_columns.iter().map(|c| c.kind).collect());
    Ok((table, table_columns, shape))
}

/// **`WHERE` 绑定**（对表行求值；必须是 BOOLEAN 形态）。
fn bind_where<V: CatalogView>(
    resolver: &mut NameResolver<'_, V>,
    where_clause: &Option<Box<ast::Expr>>,
    table_columns: &[BoundColumn],
    params: &mut BoundParams,
) -> Result<Option<PlanExpr>, BindError> {
    let _ = resolver;
    match where_clause {
        None => Ok(None),
        Some(w) => {
            let scope = BindScope {
                table_columns,
                output_names: &Default::default(),
            };
            let (e, k) = bind_expr(w, &scope, params, Some(ColKind::Bool))?;
            if k != ColKind::Bool {
                return Err(BindError::TypeMismatch {
                    what: "WHERE".to_owned(),
                    want: "BOOLEAN",
                    got: kind_name(k),
                });
            }
            Ok(Some(e))
        }
    }
}

/// **`UPDATE <表> SET … WHERE …`**（单表）。
fn bind_update<V: CatalogView>(
    resolver: &mut NameResolver<'_, V>,
    u: &ast::UpdateStmt,
) -> Result<BoundUpdate, BindError> {
    let (table, table_columns, table_shape) = single_table(resolver, &u.relation, "UPDATE")?;
    let mut params = BoundParams::default();
    let mut sets: Vec<(usize, PlanExpr)> = Vec::with_capacity(u.target_list.len());
    if u.target_list.is_empty() {
        return Err(BindError::Unsupported("UPDATE 缺 SET".to_owned()));
    }
    for item in &u.target_list {
        let name = item
            .name
            .clone()
            .ok_or_else(|| BindError::Unsupported("SET 目标不是列名".to_owned()))?;
        let idx = table_columns
            .iter()
            .position(|c| c.name == name)
            .ok_or_else(|| BindError::UnknownColumn(name.clone()))?;
        if sets.iter().any(|(i, _)| *i == idx) {
            return Err(BindError::Unsupported(format!(
                "SET 里列 `{name}` 写了两次（同一列只能有一个新值）"
            )));
        }
        let want = table_columns[idx].kind;
        let scope = BindScope {
            table_columns: &table_columns,
            output_names: &Default::default(),
        };
        let (plan, _k) = bind_expr(&item.val, &scope, &mut params, Some(want))?;
        sets.push((idx, plan));
    }
    let filter = bind_where(resolver, &u.where_clause, &table_columns, &mut params)?;
    Ok(BoundUpdate {
        table,
        table_shape,
        sets,
        filter,
        params,
    })
}

/// **`DELETE FROM <表> WHERE …`**（单表）。
fn bind_delete<V: CatalogView>(
    resolver: &mut NameResolver<'_, V>,
    d: &ast::DeleteStmt,
) -> Result<BoundDelete, BindError> {
    let (table, table_columns, table_shape) = single_table(resolver, &d.relation, "DELETE")?;
    let mut params = BoundParams::default();
    let filter = bind_where(resolver, &d.where_clause, &table_columns, &mut params)?;
    Ok(BoundDelete {
        table,
        table_shape,
        filter,
        params,
    })
}

/// **集合运算绑定**（`UNION`/`INTERSECT`/`EXCEPT`；左深嵌套）。
fn bind_setop<V: CatalogView>(
    resolver: &mut NameResolver<'_, V>,
    s: &SelectStmt,
) -> Result<BoundStatement, BindError> {
    let (op, larg, rarg) = match (s.op, &s.larg, &s.rarg) {
        (Some(op), Some(l), Some(r)) => (op, l, r),
        _ => {
            return Err(BindError::Unsupported(
                "集合运算缺一侧（`op`/`larg`/`rarg` 不齐）".to_owned(),
            ))
        }
    };
    {
        // **集合运算**：两侧各自绑成 `SELECT`，列数必须一致（形态按逐列统一）。
        if s.distinct || !s.group_clause.is_empty() || s.having_clause.is_some() {
            return Err(BindError::Unsupported(
                "集合运算节点上的 DISTINCT / GROUP BY / HAVING（把它们放进两侧的 SELECT 里）"
                    .to_owned(),
            ));
        }
        let left = bind_statement(resolver, &Stmt::Select((**larg).clone()))?;
        let right = bind_statement(resolver, &Stmt::Select((**rarg).clone()))?;
        let (lw, rw) = (select_width(&left)?, select_width(&right)?);
        if lw != rw {
            return Err(BindError::Unsupported(format!(
                "集合运算两侧列数不一致：左 {lw}、右 {rw}"
            )));
        }
        // **外层只能有 `ORDER BY`/`LIMIT`**（PG 口径：`ORDER BY` 对合并结果排序）。
        // 本版：外层 `ORDER BY` 按**输出列名/序号**对合并后的行排序——投影直接用
        // 左侧的输出（`Append` 之后列序即左侧列序）。
        let mut merged = left.clone();
        if let BoundStatement::Select(ls) = &left {
            let mut out = ls.clone();
            // 外层 `ORDER BY`/`LIMIT` 按**合并结果的输出列**绑（列序 = 左侧列序）；
            // 两侧各自的尾子句在集合运算里无意义（标准也不允许），直接取外层。
            let mut output_names = std::collections::BTreeMap::new();
            for (i, c) in ls.columns.iter().enumerate() {
                output_names.entry(c.name.clone()).or_insert(i);
            }
            let (sort, limit, offset) = bind_tail(
                &s.sort_clause,
                &s.limit_count,
                &s.limit_offset,
                &ls.columns,
                &output_names,
                // 集合运算外层：**没有"输入行"** ⇒ 只能按输出列名/序号排。
                // （它也没有参数可绑——表达式那一支在这种形态下直接拒绝。）
                None,
                &mut Default::default(),
            )?;
            out.sort = sort;
            out.limit = limit;
            out.offset = offset;
            merged = BoundStatement::Select(out);
        }
        let kind = match op {
            crate::ast::SetOperation::Union => BoundSetKind::Union,
            crate::ast::SetOperation::Intersect => BoundSetKind::Intersect,
            crate::ast::SetOperation::Except => BoundSetKind::Except,
        };
        Ok(BoundStatement::SetOp(BoundSetOp {
            kind,
            all: s.all,
            left: Box::new(merged),
            right: Box::new(right),
            width: lw,
        }))
    }
}

/// **绑定一条语句**（入口；`snapshot` = 语句快照，随语句定）。
pub fn bind_statement<V: CatalogView>(
    resolver: &mut NameResolver<'_, V>,
    stmt: &Stmt,
) -> Result<BoundStatement, BindError> {
    match stmt {
        Stmt::Select(s) if s.op.is_some() => bind_setop(resolver, s),
        Stmt::Select(s) => Ok(BoundStatement::Select(bind_select(resolver, s)?)),
        Stmt::Insert(i) => Ok(BoundStatement::Insert(bind_insert(resolver, i)?)),
        Stmt::CreateTable(c) => Ok(BoundStatement::Ddl(BoundDdl::CreateTable(
            bind_create_table(c)?,
        ))),
        Stmt::Index(ix) => Ok(BoundStatement::Ddl(BoundDdl::CreateIndex(
            bind_create_index(resolver, ix)?,
        ))),
        Stmt::Drop(d) if d.remove_type == ast::ObjectType::Workspace => {
            Ok(BoundStatement::Dcl(Box::new(stmt.clone())))
        }
        Stmt::Drop(d) => bind_drop(d),
        Stmt::Transaction(t) => Ok(BoundStatement::Transaction(t.kind)),
        Stmt::Update(u) => Ok(BoundStatement::Update(bind_update(resolver, u)?)),
        Stmt::Delete(d) => Ok(BoundStatement::Delete(bind_delete(resolver, d)?)),
        // ── **DCL 族**（`doc/DCL语句设计_v0.1.md` v0.2）──────────────────────
        // **绑定只搬运**：语义在执行层（`crate::dcl_exec`）——那里有目录写侧、
        // 实例注册表与文件系统；这里碰不到，也不该碰（REQ-SQL-002：解析期不 import 目录）。
        Stmt::CreateFilesystem(_)
        | Stmt::AlterFilesystem(_)
        | Stmt::DropFilesystem(_)
        | Stmt::CreateWorkspace(_)
        | Stmt::AlterWorkspace(_)
        | Stmt::CreateUser(_)
        | Stmt::AlterUser(_)
        | Stmt::DropUser(_)
        | Stmt::AlterDatabase(_) => Ok(BoundStatement::Dcl(Box::new(stmt.clone()))),
        Stmt::CreateGraph(_) | Stmt::VariableSet(_) => Err(BindError::Unsupported(
            "S3 首版的语句面：SELECT / INSERT / CREATE TABLE / CREATE INDEX / DROP / 事务控制"
                .to_owned(),
        )),
    }
}

// ───────────────────────────── SELECT ─────────────────────────────

fn bind_select<V: CatalogView>(
    resolver: &mut NameResolver<'_, V>,
    s: &SelectStmt,
) -> Result<BoundSelect, BindError> {
    let distinct = s.distinct;
    if s.values_lists.is_some() {
        return Err(BindError::Unsupported(
            "VALUES 只能作 INSERT 的来源".to_owned(),
        ));
    }
    // **FROM 解析**：单表 / 两表连接（`JOIN … ON`）/ 逗号连接（笛卡尔，条件进 WHERE）。
    let mut params = BoundParams::default();
    let from = resolve_from(resolver, &s.from_clause)?;
    let table_columns: Vec<BoundColumn> =
        from.tables.iter().flat_map(|t| t.columns.clone()).collect();
    // 连接条件（`ON`）：对**合并行**求值，必须是 BOOLEAN。
    let join = match &from.join {
        None => None,
        Some(spec) => {
            let qual = match &from.on_clause {
                None => None,
                Some(on) => {
                    let scope = BindScope {
                        table_columns: &table_columns,
                        output_names: &Default::default(),
                    };
                    let (e, k) = bind_expr(on, &scope, &mut params, Some(ColKind::Bool))?;
                    if k != ColKind::Bool {
                        return Err(BindError::TypeMismatch {
                            what: "ON".to_owned(),
                            want: "BOOLEAN",
                            got: kind_name(k),
                        });
                    }
                    Some(e)
                }
            };
            Some(BoundJoin {
                kind: spec.kind,
                qual,
            })
        }
    };

    // GROUP BY 先绑（投影里的非聚合列要跟它对照）。
    let mut groups = Vec::with_capacity(s.group_clause.len());
    for g in &s.group_clause {
        let scope = BindScope {
            table_columns: &table_columns,
            output_names: &Default::default(),
        };
        let (e, _k) = bind_expr(g, &scope, &mut params, None)?;
        groups.push(e);
    }
    let mut aggs: Vec<bicdb_exec::AggSpec> = Vec::new();
    let mut placeholders: Vec<BoundColumn> = Vec::new();
    let table_len = table_columns.len();
    // 投影（`*` 展开为全部表列；聚合调用抽成聚合项、原位置换成列引用）。
    let mut projection = Vec::new();
    let mut out_columns: Vec<BoundColumn> = Vec::new();
    for t in &s.target_list {
        if is_star(&t.val) {
            for (i, c) in table_columns.iter().enumerate() {
                projection.push(PlanExpr::Column(i));
                out_columns.push(c.clone());
            }
            continue;
        }
        let (e, kind) = bind_expr_agg(
            &t.val,
            &table_columns,
            &mut params,
            &mut aggs,
            &mut placeholders,
        )?;
        let name = t.name.clone().unwrap_or_else(|| output_name(&t.val));
        out_columns.push(BoundColumn {
            name,
            kind,
            nullable: true,
            table_col: None,
            source: None,
        });
        projection.push(e);
    }

    // **聚合查询的两条硬规则**（标准口径，具名拒绝而不是悄悄算错）：
    // 1. 有聚合 ⇒ 非聚合的输出表达式必须**在 GROUP BY 里**（或本身就是聚合项）；
    // 2. 没有聚合但有 GROUP BY ⇒ 也算聚合查询（键就是输出）。
    let has_agg = !aggs.is_empty() || !groups.is_empty();
    if has_agg {
        // 投影整体改写到"聚合输出行"坐标（`groups ++ aggs`）——
        // 非聚合位置必须能在 GROUP BY 里找到同一表达式，否则具名拒绝。
        let mut rewritten = Vec::with_capacity(projection.len());
        for (proj, col) in projection.iter().zip(&out_columns) {
            // 先判"非聚合位置在不在 GROUP BY 里"——**一句人话**，不套两层错误。
            if !is_aggregated_expr(proj, table_len) && !groups.contains(proj) {
                return Err(BindError::Unsupported(format!(
                    "列 `{}` 既不在 GROUP BY 里、也不是聚合——聚合查询里它没有确定的值",
                    col.name
                )));
            }
            rewritten.push(rewrite_to_agg_row(proj, &groups, table_len)?);
        }
        projection = rewritten;
    }
    // 输出名 → 序号（`ORDER BY 名` 用）。
    let mut output_names = std::collections::BTreeMap::new();
    for (i, c) in out_columns.iter().enumerate() {
        output_names.entry(c.name.clone()).or_insert(i);
    }

    // WHERE（对表行求值）。
    let filter = match &s.where_clause {
        None => None,
        Some(w) => {
            let scope = BindScope {
                table_columns: &table_columns,
                output_names: &output_names,
            };
            let (e, k) = bind_expr(w, &scope, &mut params, Some(ColKind::Bool))?;
            if k != ColKind::Bool {
                return Err(BindError::TypeMismatch {
                    what: "WHERE".to_owned(),
                    want: "BOOLEAN",
                    got: kind_name(k),
                });
            }
            Some(e)
        }
    };

    // HAVING（**对聚合输出行求值**：`groups ++ aggs` 的列序）。
    let mut having_placeholders: Vec<BoundColumn> = Vec::new();
    let having = match &s.having_clause {
        None => None,
        Some(h) => {
            let (e, k) = bind_expr_agg(
                h,
                &table_columns,
                &mut params,
                &mut aggs,
                &mut having_placeholders,
            )?;
            let e = rewrite_to_agg_row(&e, &groups, table_len)?;
            if k != ColKind::Bool {
                return Err(BindError::TypeMismatch {
                    what: "HAVING".to_owned(),
                    want: "BOOLEAN",
                    got: kind_name(k),
                });
            }
            Some(e)
        }
    };
    if !has_agg && having.is_some() {
        return Err(BindError::Unsupported(
            "HAVING 只在聚合查询里（没有 GROUP BY 也没有聚合函数）".to_owned(),
        ));
    }

    // ORDER BY / LIMIT / OFFSET（尾子句）。
    //
    // **排序表达式按哪套坐标绑**：计划的排点永远在**投影之下**，所以绑出来的
    // 表达式必须与"投影的输入行"同一套坐标——
    // - 非聚合查询：输入行 = 表行（连接时是组合行）⇒ 直接用表作用域；
    // - 聚合查询：输入行 = **聚合输出行**（`groups ++ aggs`）⇒ 先按表行绑，
    //   再用 `rewrite_to_agg_row` 改写（与投影/HAVING 同一条路；非分组列在那里
    //   会被具名拒绝——"聚合查询里它没有确定的值"）。
    let sort_scope = BindScope {
        table_columns: &table_columns,
        output_names: &Default::default(),
    };
    let (mut sort, limit, offset) = bind_tail(
        &s.sort_clause,
        &s.limit_count,
        &s.limit_offset,
        &out_columns,
        &output_names,
        Some(&sort_scope),
        &mut params,
    )?;
    if has_agg {
        for k in &mut sort {
            if let SortSpec::Input(e) = &k.spec {
                k.spec = SortSpec::Input(rewrite_to_agg_row(e, &groups, table_len)?);
            }
            // **`DISTINCT` + 表达式键**：`DISTINCT` 的排序按输出全列（`Unique`），
            // 输入行的表达式到不了那里 ⇒ 具名拒绝（PG 的口径也是"排序键必须出现在
            // select 列表里"）。
            if s.distinct && matches!(k.spec, SortSpec::Input(_)) {
                return Err(BindError::Unsupported(
                    "`DISTINCT` 与 `ORDER BY <表达式>`：排序键要出现在输出列里（把它加进 SELECT，再用输出列名排序）"
                        .to_owned(),
                ));
            }
        }
    } else if s.distinct {
        for k in &sort {
            if matches!(k.spec, SortSpec::Input(_)) {
                return Err(BindError::Unsupported(
                    "`DISTINCT` 与 `ORDER BY <表达式>`：排序键要出现在输出列里（把它加进 SELECT，再用输出列名排序）"
                        .to_owned(),
                ));
            }
        }
    }

    Ok(BoundSelect {
        tables: from.tables,
        join,
        columns: out_columns,
        projection,
        filter,
        sort,
        limit,
        offset,
        distinct,
        groups,
        aggs,
        having,
        params,
    })
}

fn is_star(e: &Expr) -> bool {
    matches!(
        e,
        Expr::ColumnRef(cr) if matches!(cr.fields.as_slice(), [ast::ColumnRefField::AStar])
    )
}

fn output_name(e: &Expr) -> String {
    match e {
        Expr::ColumnRef(cr) => match cr.fields.as_slice() {
            [ast::ColumnRefField::Name(n)] => n.clone(),
            // `表.列` ⇒ 输出名列名用**末段**（照 PG：`SELECT t.id …` 的列名是 `id`）。
            [ast::ColumnRefField::Name(_), ast::ColumnRefField::Name(n)] => n.clone(),
            _ => "?column?".to_owned(),
        },
        // 聚合：列表头写函数名（`count`/`sum`…）——照 PG 的默认列名口径，
        // 比一排 `?column?` 有用得多（多列聚合时能分辨谁是谁）。
        Expr::FuncCall(f) if crate::bind::expr::is_aggregate_name(&f.funcname) => {
            f.funcname.clone()
        }
        _ => "?column?".to_owned(),
    }
}

fn int_literal(e: &Expr, what: &str) -> Result<u64, BindError> {
    match e {
        Expr::AConst(c) => match c.value.as_ref() {
            Some(crate::ast::ConstValue::Int(text)) => text
                .parse()
                .map_err(|_| BindError::Unsupported(format!("{what} 越出 u64"))),
            _ => Err(BindError::Unsupported(format!("{what} 只收整数字面量"))),
        },
        _ => Err(BindError::Unsupported(format!("{what} 只收整数字面量"))),
    }
}

// ───────────────────────────── INSERT ─────────────────────────────

fn bind_insert<V: CatalogView>(
    resolver: &mut NameResolver<'_, V>,
    ins: &InsertStmt,
) -> Result<BoundInsert, BindError> {
    let table_name = ins.relation.relname.clone();
    let table = check_writable(resolver, &table_name)?;
    let cols = resolver.view().columns(table.obj)?;
    let table_columns: Vec<BoundColumn> = cols
        .iter()
        .map(|c| BoundColumn {
            name: c.name.clone(),
            kind: col_kind(c.type_code).unwrap_or(ColKind::Bytes),
            nullable: c.nullable,
            table_col: Some(c.col),
            source: Some(table_name.clone()),
        })
        .collect();
    let table_shape = RowShape::new(table_columns.iter().map(|c| c.kind).collect());

    // 目标列（缺省 = 全部列，按列号序）。
    let target_cols: Vec<usize> = if ins.cols.is_empty() {
        (0..table_columns.len()).collect()
    } else {
        let mut v = Vec::with_capacity(ins.cols.len());
        for c in &ins.cols {
            let name = c
                .name
                .clone()
                .ok_or_else(|| BindError::Unsupported("INSERT 列清单里的表达式".to_owned()))?;
            let idx = table_columns
                .iter()
                .position(|tc| tc.name == name)
                .ok_or_else(|| BindError::UnknownColumn(name.clone()))?;
            v.push(idx);
        }
        v
    };

    // 来源：只收 VALUES（`select_stmt.values_lists`）。
    let src = ins
        .select_stmt
        .as_deref()
        .ok_or_else(|| BindError::Unsupported("INSERT 缺来源".to_owned()))?;
    if src.values_lists.is_none() {
        // **`INSERT … SELECT`**：来源是另一个 `SELECT`（含集合运算）——
        // 判据是"没有 `VALUES` 列表"（普通 SELECT 与集合运算都走这里）。
        let source = bind_statement(resolver, &Stmt::Select(src.clone()))?;
        let width = select_width(&source)?;
        if width != target_cols.len() {
            return Err(BindError::Unsupported(format!(
                "INSERT … SELECT：来源 {width} 列、目标列清单 {} 列——列数必须一致",
                target_cols.len()
            )));
        }
        return Ok(BoundInsert {
            table,
            table_shape,
            target_cols,
            rows: Vec::new(),
            source_select: Some(Box::new(source)),
            // 参数的摆位：来源 `SELECT` 自己带一份（`INSERT … SELECT :p` 的
            // `:p` 在它里面）；本层不再另开清单。
            params: BoundParams::default(),
        });
    }
    let lists = src
        .values_lists
        .as_ref()
        .ok_or_else(|| BindError::Unsupported("INSERT 的来源不是 VALUES".to_owned()))?;
    let mut params = BoundParams::default();
    let mut rows = Vec::with_capacity(lists.len());
    for list in lists {
        if list.len() != target_cols.len() {
            return Err(BindError::Unsupported(format!(
                "VALUES 行有 {} 个值、列清单有 {} 列",
                list.len(),
                target_cols.len()
            )));
        }
        let mut row = Vec::with_capacity(list.len());
        for (e, tc) in list.iter().zip(&target_cols) {
            let want = table_columns[*tc].kind;
            let scope = BindScope {
                table_columns: &[],
                output_names: &Default::default(),
            };
            let (plan, _k) = bind_expr(e, &scope, &mut params, Some(want))?;
            row.push(plan);
        }
        rows.push(row);
    }
    Ok(BoundInsert {
        table,
        table_shape,
        target_cols,
        rows,
        source_select: None,
        params,
    })
}

/// **写目标检查**（§4.4）：固定表已在第 ① 格查不到；这里拒的是"能解析到但不可写"的。
fn check_writable<V: CatalogView>(
    resolver: &mut NameResolver<'_, V>,
    name: &str,
) -> Result<CatalogObject, BindError> {
    const READ_ONLY_PRESETS: &[&str] = &["asset$", "ref$", "audit"];
    let obj = resolver.resolve_write_target(name)?;
    if READ_ONLY_PRESETS.contains(&name) {
        return Err(BindError::NotWritable(name.to_owned()));
    }
    if obj.status != 1 {
        return Err(BindError::NotWritable(format!(
            "{name}（status = {}）",
            obj.status
        )));
    }
    Ok(obj)
}

// ───────────────────────────── DDL ─────────────────────────────

fn bind_create_table(c: &ast::CreateStmt) -> Result<TableSpec, BindError> {
    super::check_new_object_name(&c.relation.relname)?;
    if c.table_elts.is_empty() {
        return Err(BindError::Unsupported("没有列的 CREATE TABLE".to_owned()));
    }
    let mut columns = Vec::with_capacity(c.table_elts.len());
    for cd in &c.table_elts {
        let (type_code, length) = bind_type(&cd.type_name)?;
        columns.push(ColumnSpec {
            name: cd.colname.clone(),
            type_code,
            length,
            precision: None,
            scale: None,
            nullable: !cd.is_not_null,
        });
    }
    let options = bind_table_options(&c.options)?;
    Ok(TableSpec {
        name: c.relation.relname.clone(),
        columns,
        options,
    })
}

/// 类型名 → （类型码，声明长度）。**清单外具名拒绝**。
fn bind_type(t: &ast::TypeName) -> Result<(ColTypeCode, u32), BindError> {
    let len = t.typmods.first().and_then(|s| s.parse::<u32>().ok());
    let out = match t.name.as_str() {
        "number" | "integer" | "int" => (ColTypeCode::Number, 0),
        "varchar2" | "varchar" | "text" => (ColTypeCode::Varchar2, len.unwrap_or(255)),
        "char" => (ColTypeCode::Char, len.unwrap_or(1)),
        "bytes" => (ColTypeCode::Bytes, len.unwrap_or(64)),
        "boolean" | "bool" => (ColTypeCode::Boolean, 0),
        // **日期时间类型本版不接**：`crates/types/src/datetime.rs` 的 7 字节
        // DATE / 11 字节 TIMESTAMP 编码已实现，但**没有任何消费者**——此前
        // `timestamp` 被收下并按 `ColKind::Number` 存（等于一个普通数字列：
        // 插 `'2026-01-02 03:04:05'` 报"类型不匹配"，插 `20260102` 却成功）。
        // "收下不生效"比"拒绝"更糟，故与 `date`/`uuid`/`raw`/`json` 同列具名拒绝。
        "date" | "timestamp" | "timestamptz" | "uuid" | "raw" | "json" | "vector" => {
            return Err(BindError::Unsupported(format!(
                "列类型 `{t}`（日期时间/扩展类型随 TYP 切片接线——编码在 \
                 `crates/types/src/datetime.rs`，此前被收下却按普通数值存，故先拒绝）",
                t = t.name
            )))
        }
        other => return Err(BindError::Unsupported(format!("列类型 `{other}`"))),
    };
    Ok(out)
}

/// `WITH (…)` 表选项 → 内核选项（**键闭集**；未知键具名拒绝）。
///
/// **只收「本版真读」的选项**：`pctfree` / `itl_max`（写路径消费，见 `exec::writer`），
/// 以及 `table_type = transactional|heap`（就是本版唯一的表形态，收了不改变行为）。
///
/// **其余一律具名拒绝**（`table_type = memory|append_only`、`logging = …`、
/// `retention = …`）：这些选项**此前被收下、落进 `tab$`、然后没人读**——
/// `CREATE TABLE t (…) WITH (table_type = append_only)` 会**静默得到一张普通表**，
/// 而用户以为自己建了追加型表（表类型与存储选项 §3 的五种表，本版只有第 1 种）。
/// 静默降级比报错更糟：**没实现的语义必须当场说**。
fn bind_table_options(opts: &[ast::DefElem]) -> Result<TableOptions, BindError> {
    let mut o = TableOptions::default();
    for d in opts {
        match d.defname.as_str() {
            // 本版唯一的表形态：普通关系表（transactional + in_place + full logging）。
            // 显式写它 = 写默认值，收下。
            "table_type" => {
                let v = ident_value(&d.arg)?;
                match v.as_str() {
                    "transactional" | "heap" => {}
                    other => {
                        return Err(BindError::Unsupported(format!(
                            "table_type = {other}（本版只有普通关系表；其余表型的语义随『表类型与存储选项 §3』切片——收下不生效比拒绝更糟）"
                        )))
                    }
                }
            }
            // **范围校验，不截断**：`int_value` 给的是 `u32`，`as u8` 会把
            // `pctfree = 300` 静默变成 44、`= 256` 变成 0——用户写的值与库里
            // 落的值不是一回事，还查不出来。越界即具名拒绝。
            "pctfree" => {
                let n = int_value(&d.arg)?;
                if n > 99 {
                    return Err(BindError::Unsupported(format!(
                        "pctfree = {n} 越界（0–99：页内预留百分比）"
                    )));
                }
                o.pctfree = n as u8;
            }
            "itl_max" => {
                let n = int_value(&d.arg)?;
                let max = bicdb_storage::itl::ITL_MAX_LIMIT as u32;
                if n == 0 || n > max {
                    return Err(BindError::Unsupported(format!(
                        "itl_max = {n} 越界（1–{max}：页内事务槽上限）"
                    )));
                }
                o.itl_max = n as u8;
            }
            "retention" => {
                let _ = int_value(&d.arg)?;
                return Err(BindError::Unsupported(
                    "retention（保留窗口）的语义随『表类型与存储选项 §4.5』切片——本版不生效"
                        .to_owned(),
                ));
            }
            "logging" => {
                let v = ident_value(&d.arg)?;
                if v != "full" {
                    return Err(BindError::Unsupported(format!(
                        "logging = {v}（本版恒为 full；redo_only/none 随相应表型切片——不生效）"
                    )));
                }
            }
            other => return Err(BindError::Unsupported(format!("表选项 `{other}`"))),
        }
    }
    Ok(o)
}

fn ident_value(arg: &DefElemArg) -> Result<String, BindError> {
    match arg {
        DefElemArg::Ident(s) => Ok(s.clone()),
        DefElemArg::Const(c) => match c.value.as_ref() {
            Some(crate::ast::ConstValue::Str(b)) => String::from_utf8(b.clone())
                .map_err(|_| BindError::Unsupported("非 UTF-8 选项值".to_owned())),
            _ => Err(BindError::Unsupported("选项值形态".to_owned())),
        },
    }
}

fn int_value(arg: &DefElemArg) -> Result<u32, BindError> {
    match arg {
        DefElemArg::Const(c) => match c.value.as_ref() {
            Some(crate::ast::ConstValue::Int(t)) => t
                .parse()
                .map_err(|_| BindError::Unsupported("选项数值越域".to_owned())),
            _ => Err(BindError::Unsupported("选项值形态".to_owned())),
        },
        DefElemArg::Ident(t) => t
            .parse()
            .map_err(|_| BindError::Unsupported("选项数值越域".to_owned())),
    }
}

fn bind_create_index<V: CatalogView>(
    resolver: &mut NameResolver<'_, V>,
    ix: &ast::IndexStmt,
) -> Result<IndexSpec, BindError> {
    super::check_new_object_name(&ix.idxname)?;
    if ix.target_kind != ast::IndexTargetKind::Table {
        return Err(BindError::Unsupported("图（VERTEX/EDGE）索引".to_owned()));
    }
    if ix.index_params.is_empty() {
        return Err(BindError::Unsupported("没有键列的 CREATE INDEX".to_owned()));
    }
    // 基表必须在（**绑定期早错**；键列的存在性由 DDL 侧对 `col$` 复核）。
    let table = resolver.resolve_table(&ix.relation.relname)?;
    if table.object().is_none() {
        return Err(BindError::Unsupported("对固定表建索引".to_owned()));
    }
    let mut columns = Vec::with_capacity(ix.index_params.len());
    for p in &ix.index_params {
        if p.expr.is_some() {
            return Err(BindError::Unsupported("表达式索引".to_owned()));
        }
        let name = p
            .name
            .clone()
            .ok_or_else(|| BindError::Unsupported("键列形态".to_owned()))?;
        columns.push(name);
    }
    Ok(IndexSpec {
        name: ix.idxname.clone(),
        table: ix.relation.relname.clone(),
        unique: ix.unique,
        columns,
    })
}

fn bind_drop(d: &ast::DropStmt) -> Result<BoundStatement, BindError> {
    if d.missing_ok {
        return Err(BindError::Unsupported("DROP … IF EXISTS".to_owned()));
    }
    // `DROP WORKSPACE` 走**工作区引用**（整数或名字），与表/索引的名字形态不同。
    if d.remove_type == ObjectType::Workspace {
        // 工作区走 `WorkRef`（整数或名字）；语义在执行层（`dcl_exec`）。
        if d.workspaces.len() != 1 {
            return Err(BindError::Unsupported("一次 DROP 多个工作区".to_owned()));
        }
        return Ok(BoundStatement::Dcl(Box::new(ast::Stmt::Drop(d.clone()))));
    }
    if d.objects.len() != 1 {
        return Err(BindError::Unsupported("一次 DROP 多个对象".to_owned()));
    }
    let name = d.objects[0].relname.clone();
    match d.remove_type {
        ObjectType::Table => Ok(BoundStatement::Ddl(BoundDdl::DropTable(name))),
        ObjectType::Index => Ok(BoundStatement::Ddl(BoundDdl::DropIndex(name))),
        ObjectType::Graph => Err(BindError::Unsupported("DROP GRAPH".to_owned())),
        ObjectType::Workspace => unreachable!("上面已返回"),
    }
}

/// 目录类型码 → 执行器形态。
#[must_use]
pub fn col_kind(code: u32) -> Option<ColKind> {
    match ColTypeCode::from_u8(code as u8)? {
        ColTypeCode::Number | ColTypeCode::Timestamp | ColTypeCode::TimestampTz => {
            Some(ColKind::Number)
        }
        ColTypeCode::Boolean => Some(ColKind::Bool),
        ColTypeCode::Char
        | ColTypeCode::Varchar2
        | ColTypeCode::Date
        | ColTypeCode::Bytes
        | ColTypeCode::Uuid
        | ColTypeCode::Json
        | ColTypeCode::Vector
        | ColTypeCode::AssetRef => Some(ColKind::Bytes),
    }
}
