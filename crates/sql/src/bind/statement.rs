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
use super::{BindError, CatalogObject, CatalogView, NameResolver, ResolvedName};
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
    /// 事务控制（会话层直通）。
    Transaction(ast::TransactionStmtKind),
    /// `CREATE WORKSPACE`（DCL；会话/管理面直通）。
    CreateWorkspace {
        /// 主体。
        subject: String,
        /// 名字。
        name: Option<Vec<u8>>,
    },
}

/// 绑定后的 `SELECT`。
#[derive(Debug, Clone, PartialEq)]
pub struct BoundSelect {
    /// 表对象（已解析）。
    pub table: CatalogObject,
    /// 表的行形状（扫描/回表用）。
    pub table_shape: RowShape,
    /// 表列名（诊断）。
    pub table_column_names: Vec<String>,
    /// 输出列（名 + 形态）。
    pub columns: Vec<BoundColumn>,
    /// 投影表达式（对表行求值；顺序 = 输出列序）。
    pub projection: Vec<PlanExpr>,
    /// `WHERE`（None = 全放行）。
    pub filter: Option<PlanExpr>,
    /// 排序键（**对投影后的行求值**——`ORDER BY` 按输出列/序号）。
    pub sort: Vec<(usize, bool)>,
    /// `LIMIT`。
    pub limit: Option<u64>,
    /// `OFFSET`。
    pub offset: u64,
    /// 参数清单（顺序即执行期参数序）。
    pub params: BoundParams,
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
    pub rows: Vec<Vec<PlanExpr>>,
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

/// **绑定一条语句**（入口；`snapshot` = 语句快照，随语句定）。
pub fn bind_statement<V: CatalogView>(
    resolver: &mut NameResolver<'_, V>,
    stmt: &Stmt,
) -> Result<BoundStatement, BindError> {
    match stmt {
        Stmt::Select(s) => Ok(BoundStatement::Select(bind_select(resolver, s)?)),
        Stmt::Insert(i) => Ok(BoundStatement::Insert(bind_insert(resolver, i)?)),
        Stmt::CreateTable(c) => Ok(BoundStatement::Ddl(BoundDdl::CreateTable(
            bind_create_table(c)?,
        ))),
        Stmt::Index(ix) => Ok(BoundStatement::Ddl(BoundDdl::CreateIndex(
            bind_create_index(resolver, ix)?,
        ))),
        Stmt::Drop(d) => bind_drop(d),
        Stmt::Transaction(t) => Ok(BoundStatement::Transaction(t.kind)),
        Stmt::CreateWorkspace(cw) => Ok(BoundStatement::CreateWorkspace {
            subject: cw.subject.clone(),
            name: cw.name.clone(),
        }),
        Stmt::Update(_) | Stmt::Delete(_) => Err(BindError::Unsupported(
            "UPDATE / DELETE（S3 首版的语句面：SELECT / INSERT / CREATE / DROP）".to_owned(),
        )),
        Stmt::CreateGraph(_)
        | Stmt::AlterWorkspace(_)
        | Stmt::VariableSet(_)
        | Stmt::AlterSystem(_)
        | Stmt::AlterDatabase(_) => Err(BindError::Unsupported(
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
    if s.op.is_some() || s.larg.is_some() || s.rarg.is_some() {
        return Err(BindError::Unsupported(
            "集合运算（UNION/INTERSECT/EXCEPT）".to_owned(),
        ));
    }
    if s.distinct {
        return Err(BindError::Unsupported("SELECT DISTINCT".to_owned()));
    }
    if !s.group_clause.is_empty() || s.having_clause.is_some() {
        return Err(BindError::Unsupported(
            "GROUP BY / HAVING（聚合随 S4 之后）".to_owned(),
        ));
    }
    // 单表 FROM。
    let table_name = match s.from_clause.as_slice() {
        [FromItem::RangeVar(rv)] => {
            if rv.alias.is_some() {
                return Err(BindError::Unsupported("表别名".to_owned()));
            }
            rv.relname.clone()
        }
        [] => return Err(BindError::Unsupported("无 FROM 的 SELECT".to_owned())),
        _ => {
            return Err(BindError::Unsupported(
                "多表 FROM / JOIN（随 S4 之后）".to_owned(),
            ))
        }
    };
    // 三格解析（表源：固定表本轮不支持列枚举 ⇒ 具名拒绝）。
    let table = match resolver.resolve_table(&table_name)? {
        ResolvedName::Object(o) => o,
        ResolvedName::FixedTable(n) => {
            return Err(BindError::Unsupported(format!(
                "固定表 `{n}` 的扫描（随 OPS/会话切片）"
            )))
        }
    };
    if table.type_code != bicdb_catalog::obj_kind::TABLE {
        return Err(BindError::Unsupported(format!(
            "`{table_name}` 不是表（type# {}）",
            table.type_code
        )));
    }
    // 列枚举（类型/可空来自 col$；TYP 内核转换随类型面扩展）。
    let cols = resolver.view().columns(table.obj)?;
    let table_columns: Vec<BoundColumn> = cols
        .iter()
        .map(|c| BoundColumn {
            name: c.name.clone(),
            kind: col_kind(c.type_code).unwrap_or(ColKind::Bytes),
            nullable: c.nullable,
            table_col: Some(c.col),
        })
        .collect();
    let table_shape = RowShape::new(table_columns.iter().map(|c| c.kind).collect());
    let table_column_names = table_columns.iter().map(|c| c.name.clone()).collect();

    // 投影（`*` 展开为全部表列）。
    let mut params = BoundParams::default();
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
        let scope = BindScope {
            table_columns: &table_columns,
            output_names: &Default::default(),
        };
        let (e, kind) = bind_expr(&t.val, &scope, &mut params, None)?;
        let name = t.name.clone().unwrap_or_else(|| output_name(&t.val));
        out_columns.push(BoundColumn {
            name,
            kind,
            nullable: true,
            table_col: None,
        });
        projection.push(e);
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

    // ORDER BY（**对投影后的行求值**：序号 or 输出名；MVP 的收窄口径）。
    let mut sort = Vec::new();
    for sb in &s.sort_clause {
        let idx = match &sb.node {
            Expr::AConst(c) => match c.value.as_ref() {
                Some(crate::ast::ConstValue::Int(text)) => {
                    let n: usize = text
                        .parse()
                        .map_err(|_| BindError::Unsupported("ORDER BY 序号".to_owned()))?;
                    if n == 0 || n > out_columns.len() {
                        return Err(BindError::UnknownColumn(text.clone()));
                    }
                    n - 1
                }
                _ => return Err(BindError::Unsupported("ORDER BY 常量".to_owned())),
            },
            Expr::ColumnRef(cr) => {
                let name = match cr.fields.as_slice() {
                    [ast::ColumnRefField::Name(n)] => n.clone(),
                    _ => return Err(BindError::Unsupported("ORDER BY 多段引用".to_owned())),
                };
                *output_names
                    .get(&name)
                    .ok_or_else(|| BindError::UnknownColumn(name.clone()))?
            }
            _ => {
                return Err(BindError::Unsupported(
                    "ORDER BY 表达式（首版只收序号/输出名）".to_owned(),
                ))
            }
        };
        sort.push((idx, sb.sortby_dir == SortByDir::Desc));
    }

    // LIMIT / OFFSET（只收整数字面量）。
    let limit = match &s.limit_count {
        None => None,
        Some(e) => Some(int_literal(e, "LIMIT")?),
    };
    let offset = match &s.limit_offset {
        None => 0,
        Some(e) => int_literal(e, "OFFSET")?,
    };

    Ok(BoundSelect {
        table,
        table_shape,
        table_column_names,
        columns: out_columns,
        projection,
        filter,
        sort,
        limit,
        offset,
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
            _ => "?column?".to_owned(),
        },
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
    if src.op.is_some() {
        return Err(BindError::Unsupported("INSERT … SELECT".to_owned()));
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
        "timestamp" => (ColTypeCode::Timestamp, 0),
        other => return Err(BindError::Unsupported(format!("列类型 `{other}`"))),
    };
    Ok(out)
}

/// `WITH (…)` 表选项 → 内核选项（**键闭集**；未知键具名拒绝）。
fn bind_table_options(opts: &[ast::DefElem]) -> Result<TableOptions, BindError> {
    let mut o = TableOptions::default();
    for d in opts {
        match d.defname.as_str() {
            // `table_type = memory|…`：表类型（V1.0 的选项面）——映射到更新模式/清理策略。
            "table_type" => {
                let v = ident_value(&d.arg)?;
                match v.as_str() {
                    "transactional" | "heap" => {}
                    "memory" => o.update_mode = bicdb_catalog::table_opt::UPDATE_IN_PLACE,
                    "append_only" => o.update_mode = bicdb_catalog::table_opt::UPDATE_APPEND_ONLY,
                    other => return Err(BindError::Unsupported(format!("table_type = {other}"))),
                }
            }
            "pctfree" => o.pctfree = int_value(&d.arg)? as u8,
            "itl_max" => o.itl_max = int_value(&d.arg)? as u8,
            "retention" => o.retention = int_value(&d.arg)?,
            "logging" => {
                let v = ident_value(&d.arg)?;
                o.logging = match v.as_str() {
                    "full" => bicdb_catalog::table_opt::LOGGING_FULL,
                    "redo_only" => bicdb_catalog::table_opt::LOGGING_REDO_ONLY,
                    "none" => bicdb_catalog::table_opt::LOGGING_NONE,
                    other => return Err(BindError::Unsupported(format!("logging = {other}"))),
                };
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
    if d.objects.len() != 1 {
        return Err(BindError::Unsupported("一次 DROP 多个对象".to_owned()));
    }
    let name = d.objects[0].relname.clone();
    match d.remove_type {
        ObjectType::Table => Ok(BoundStatement::Ddl(BoundDdl::DropTable(name))),
        ObjectType::Index => Ok(BoundStatement::Ddl(BoundDdl::DropIndex(name))),
        ObjectType::Graph | ObjectType::Workspace => Err(BindError::Unsupported(
            "DROP GRAPH / DROP WORKSPACE".to_owned(),
        )),
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
