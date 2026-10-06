//! **表达式绑定与类型推导**（`SQL前端设计` §4.3；S3 的表达式部分）。
//!
//! ```text
//! 绑定 = 名字/列 → 列号 + 类型；字面量 → TYP 内核解析；参数 → 用法推导类型
//! 产出 = bicdb_exec::Expr（执行期形态）+ 该表达式的 ColKind（类型检查用）
//! ```
//!
//! **设计口径**（§4.3）：
//! - **全部走 TYP 内核**——字面量 `NUMBER` 文本经 `bicdb_types::Number::parse`，
//!   四则运算用内核的十进制实现（执行期）；前端**不实现任何独立的类型规则**；
//! - **参数 `:name` 的类型由用法推导**：`col = :p` ⇒ 取列类型；推导不出
//!   （如 `SELECT :p`）⇒ **拒绝**（"参数无类型上下文"）；同一参数多处使用必须
//!   推出**同一类型**，否则绑定期错误。
//!
//! **本切片的表达式面**（`执行算子设计` §2.2 的算子输入面）：列引用、字面量、
//! 参数、算术（`+ - * /` 与一元负号）、比较（`= <> < <= > >=`）、`NOT/AND/OR`、
//! `IS [NOT] NULL`、`IN`、`BETWEEN`、`CAST`（`NUMBER`/`BYTES`）。清单外的构件
//! （函数调用、`LIKE`、子查询等）在**绑定期具名拒绝**——解析器已挡掉大部分，
//! 这里兜住"解析通过但绑定不支持"的残余。

use std::collections::BTreeMap;

use bicdb_exec::{ArithOp, CmpOp, Expr as PlanExpr, Value};
use bicdb_types::Number;

use super::{BindError, CatalogColumn};
use crate::ast::{self, AConst, AExprKind, BoolExprType, ConstValue, Expr, NullTestType};

/// 绑定期的一列（表列或输出列的形态）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundColumn {
    /// 列名（输出名/表列名）。
    pub name: String,
    /// 值形态。
    pub kind: bicdb_exec::ColKind,
    /// 可空（表列来自 `col$`；输出列 = 表达式可空性，MVP 取保守值）。
    pub nullable: bool,
    /// **表列号**（1 起；纯输出表达式 ⇒ `None`）。
    pub table_col: Option<u32>,
}

/// **参数表**（`:name` → 推导出的形态；**顺序即执行期参数序**）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BoundParams {
    order: Vec<String>,
    kinds: BTreeMap<String, bicdb_exec::ColKind>,
}

impl BoundParams {
    /// 记一个参数的形态（同参数多处使用必须同类型）。
    pub fn note(&mut self, name: &str, kind: bicdb_exec::ColKind) -> Result<(), BindError> {
        match self.kinds.get(name) {
            Some(k) if *k != kind => Err(BindError::ParamTypeConflict {
                name: name.to_owned(),
                first: kind_name(*k),
                again: kind_name(kind),
            }),
            Some(_) => Ok(()),
            None => {
                self.order.push(name.to_owned());
                self.kinds.insert(name.to_owned(), kind);
                Ok(())
            }
        }
    }

    /// 参数清单（名 + 形态；顺序即执行期参数序）。
    #[must_use]
    pub fn list(&self) -> Vec<(&str, bicdb_exec::ColKind)> {
        self.order
            .iter()
            .map(|n| (n.as_str(), self.kinds[n]))
            .collect()
    }

    /// 个数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.order.len()
    }

    /// 空否。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }
}

/// 形态名（错误文案/诊断）。
#[must_use]
pub fn kind_name(k: bicdb_exec::ColKind) -> &'static str {
    match k {
        bicdb_exec::ColKind::Number => "NUMBER",
        bicdb_exec::ColKind::Bool => "BOOLEAN",
        bicdb_exec::ColKind::Bytes => "BYTES",
    }
}

/// **一列可用的名字解析范围**（表列 + 输出别名）。
pub struct BindScope<'a> {
    /// 表列（按列号序；`table_col` = 序号+1）。
    pub table_columns: &'a [BoundColumn],
    /// 输出别名/输出列名 → 输出列序（`ORDER BY 名` 用）。
    pub output_names: &'a BTreeMap<String, usize>,
}

impl BindScope<'_> {
    fn lookup_table_column(&self, name: &str) -> Option<(usize, &BoundColumn)> {
        self.table_columns
            .iter()
            .enumerate()
            .find(|(_, c)| c.name == name)
    }
}

/// **绑定一个表达式**（`expect` = 期望形态，来自用法：比较的另一侧/插入的目标列）。
pub fn bind_expr(
    raw: &Expr,
    scope: &BindScope<'_>,
    params: &mut BoundParams,
    expect: Option<bicdb_exec::ColKind>,
) -> Result<(PlanExpr, bicdb_exec::ColKind), BindError> {
    match raw {
        Expr::ColumnRef(cr) => {
            let name = column_ref_name(cr)?;
            let (i, col) = scope
                .lookup_table_column(&name)
                .ok_or_else(|| BindError::UnknownColumn(name.clone()))?;
            // 表列（`table_col` 有值）⇒ 序号即表列序；纯输出列在 WHERE 里不可用
            // （MVP：委托给调用方保证 scope 只装可用列）。
            let _ = col;
            Ok((PlanExpr::Column(i), col.kind))
        }
        Expr::AConst(AConst { value, .. }) => {
            let (v, k) = const_value(value.as_ref(), expect)?;
            Ok((PlanExpr::Literal(v), k))
        }
        Expr::ParamRef(p) => {
            let kind = expect.ok_or_else(|| BindError::ParamWithoutContext(p.name.clone()))?;
            params.note(&p.name, kind)?;
            let idx = params
                .list()
                .iter()
                .position(|(n, _)| *n == p.name)
                .expect("刚登记");
            Ok((PlanExpr::Param(idx), kind))
        }
        Expr::AExpr(ae) => bind_aexpr(ae, scope, params, expect),
        Expr::BoolExpr(be) => {
            let mut args = Vec::with_capacity(be.args.len());
            for a in &be.args {
                let (e, k) = bind_expr(a, scope, params, Some(bicdb_exec::ColKind::Bool))?;
                require_kind(k, bicdb_exec::ColKind::Bool, "布尔运算")?;
                args.push(e);
            }
            let plan = match be.boolop {
                BoolExprType::And => PlanExpr::And(args),
                BoolExprType::Or => PlanExpr::Or(args),
                BoolExprType::Not => {
                    if args.len() != 1 {
                        return Err(BindError::Unsupported("NOT 的操作数个数".to_owned()));
                    }
                    PlanExpr::Not(Box::new(args.pop().expect("恰一个")))
                }
            };
            Ok((plan, bicdb_exec::ColKind::Bool))
        }
        Expr::NullTest(nt) => {
            let (e, _k) = bind_expr(&nt.arg, scope, params, None)?;
            let plan = PlanExpr::IsNull {
                expr: Box::new(e),
                negated: matches!(nt.nulltesttype, NullTestType::IsNotNull),
            };
            Ok((plan, bicdb_exec::ColKind::Bool))
        }
        Expr::TypeCast(tc) => {
            let to = cast_target(&tc.type_name.name)?;
            let (e, _k) = bind_expr(&tc.arg, scope, params, Some(to))?;
            Ok((
                PlanExpr::Cast {
                    expr: Box::new(e),
                    to,
                },
                to,
            ))
        }
        Expr::CaseExpr(_) | Expr::CoalesceExpr(_) | Expr::FuncCall(_) => Err(
            BindError::Unsupported("S3 首版的表达式面不含 CASE/COALESCE/函数调用".to_owned()),
        ),
    }
}

fn bind_aexpr(
    ae: &ast::AExpr,
    scope: &BindScope<'_>,
    params: &mut BoundParams,
    expect: Option<bicdb_exec::ColKind>,
) -> Result<(PlanExpr, bicdb_exec::ColKind), BindError> {
    use bicdb_exec::ColKind;
    let lexpr = ae
        .lexpr
        .as_deref()
        .ok_or_else(|| BindError::Unsupported("缺左操作数".to_owned()))?;
    // `IN (…)`：左侧形态即列表的期望形态。
    if matches!(ae.kind, AExprKind::In | AExprKind::NotIn) {
        let (left, lk) = bind_expr(lexpr, scope, params, None)?;
        let mut items = Vec::new();
        for it in &ae.rexpr_list {
            let (e, k) = bind_expr(it, scope, params, Some(lk))?;
            require_kind(k, lk, "IN 列表")?;
            items.push(e);
        }
        let in_list = PlanExpr::InList {
            expr: Box::new(left),
            list: items,
        };
        return Ok((
            if matches!(ae.kind, AExprKind::NotIn) {
                PlanExpr::Not(Box::new(in_list))
            } else {
                in_list
            },
            ColKind::Bool,
        ));
    }
    // `BETWEEN lo AND hi`：`rexpr_list` 两元素 ⇒ 绑成 `left >= lo AND left <= hi`
    // （**语义等价**，且三值逻辑一致：任一侧 UNKNOWN ⇒ 整体 UNKNOWN）。
    if matches!(ae.kind, AExprKind::Between | AExprKind::NotBetween) {
        if ae.rexpr_list.len() != 2 {
            return Err(BindError::Unsupported("BETWEEN 的界个数".to_owned()));
        }
        let (left, lk) = bind_expr(lexpr, scope, params, expect)?;
        let (lo, lok) = bind_expr(&ae.rexpr_list[0], scope, params, Some(lk))?;
        require_kind(lok, lk, "BETWEEN 下界")?;
        let (hi, hik) = bind_expr(&ae.rexpr_list[1], scope, params, Some(lk))?;
        require_kind(hik, lk, "BETWEEN 上界")?;
        let ge = PlanExpr::Compare {
            op: CmpOp::Ge,
            left: Box::new(left.clone()),
            right: Box::new(lo),
        };
        let le = PlanExpr::Compare {
            op: CmpOp::Le,
            left: Box::new(left),
            right: Box::new(hi),
        };
        let both = PlanExpr::And(vec![ge, le]);
        let plan = if matches!(ae.kind, AExprKind::NotBetween) {
            PlanExpr::Not(Box::new(both))
        } else {
            both
        };
        return Ok((plan, ColKind::Bool));
    }
    let (left, lk) = bind_expr(lexpr, scope, params, expect)?;
    let right = ae
        .rexpr
        .as_deref()
        .ok_or_else(|| BindError::Unsupported("二元表达式缺右侧".to_owned()))?;
    let (right_e, rk) = bind_expr(right, scope, params, Some(lk))?;
    require_kind(rk, lk, "二元运算两侧")?;
    let name = ae.name.clone();
    let plan = match ae.kind {
        AExprKind::Op => match name.as_str() {
            "=" | "<>" | "<" | "<=" | ">" | ">=" => {
                let op = match name.as_str() {
                    "=" => CmpOp::Eq,
                    "<>" => CmpOp::Ne,
                    "<" => CmpOp::Lt,
                    "<=" => CmpOp::Le,
                    ">" => CmpOp::Gt,
                    _ => CmpOp::Ge,
                };
                PlanExpr::Compare {
                    op,
                    left: Box::new(left),
                    right: Box::new(right_e),
                }
            }
            "+" | "-" | "*" | "/" => {
                require_kind(lk, ColKind::Number, "算术")?;
                let op = match name.as_str() {
                    "+" => ArithOp::Add,
                    "-" => ArithOp::Sub,
                    "*" => ArithOp::Mul,
                    _ => ArithOp::Div,
                };
                PlanExpr::Arith {
                    op,
                    left: Box::new(left),
                    right: Box::new(right_e),
                }
            }
            op => return Err(BindError::Unsupported(format!("运算符 `{op}`"))),
        },
        AExprKind::Between | AExprKind::NotBetween => unreachable!("上面已处理"),
        AExprKind::NullIf => return Err(BindError::Unsupported("NULLIF（S3 首版）".to_owned())),
        AExprKind::In | AExprKind::NotIn => unreachable!("上面已处理"),
    };
    let out_kind = if matches!(ae.kind, AExprKind::Op)
        && !matches!(name.as_str(), "=" | "<>" | "<" | "<=" | ">" | ">=")
    {
        ColKind::Number
    } else {
        ColKind::Bool
    };
    Ok((plan, out_kind))
}

/// 字面量 → 值 + 形态（`NULL` 取期望形态；无期望 ⇒ 字节串）。
fn const_value(
    v: Option<&ConstValue>,
    expect: Option<bicdb_exec::ColKind>,
) -> Result<(Value, bicdb_exec::ColKind), BindError> {
    use bicdb_exec::ColKind;
    let Some(v) = v else {
        let k = expect.unwrap_or(ColKind::Bytes);
        return Ok((Value::Null, k));
    };
    match v {
        ConstValue::Int(text) | ConstValue::Float(text) => {
            let n = Number::parse(text).map_err(|e| BindError::BadLiteral {
                text: text.clone(),
                why: e.to_string(),
            })?;
            Ok((Value::Number(n), ColKind::Number))
        }
        ConstValue::Str(bytes) => Ok((Value::Bytes(bytes.clone()), ColKind::Bytes)),
        ConstValue::Bool(b) => Ok((Value::Bool(*b), ColKind::Bool)),
    }
}

/// 比较/运算的两侧形态必须一致（MVP：不做隐式提升——那是 TYP 内核的转换表，
/// 随 `CAST` 的扩展一起接）。
fn require_kind(
    got: bicdb_exec::ColKind,
    want: bicdb_exec::ColKind,
    what: &str,
) -> Result<(), BindError> {
    if got == want {
        return Ok(());
    }
    Err(BindError::TypeMismatch {
        what: what.to_owned(),
        want: kind_name(want),
        got: kind_name(got),
    })
}

fn cast_target(name: &str) -> Result<bicdb_exec::ColKind, BindError> {
    match name {
        "number" | "integer" | "int" => Ok(bicdb_exec::ColKind::Number),
        "bytes" | "varchar2" | "char" | "text" => Ok(bicdb_exec::ColKind::Bytes),
        "boolean" | "bool" => Ok(bicdb_exec::ColKind::Bool),
        other => Err(BindError::Unsupported(format!("CAST 到 `{other}`"))),
    }
}

/// 列引用的单段名字（多段/`*` 在 S3 首版拒绝）。
fn column_ref_name(cr: &ast::ColumnRef) -> Result<String, BindError> {
    match (cr.fields.len(), cr.fields.first()) {
        (1, Some(ast::ColumnRefField::Name(n))) => Ok(n.clone()),
        (1, Some(ast::ColumnRefField::AStar)) => {
            Err(BindError::Unsupported("`*` 在表达式里".to_owned()))
        }
        _ => Err(BindError::Unsupported("多段列引用".to_owned())),
    }
}

/// **可空性**（保守：只要有一侧可空 ⇒ 结果可空；诊断用）。
#[must_use]
pub fn nullable_of(expr: &Expr, cols: &[CatalogColumn]) -> bool {
    match expr {
        Expr::AConst(a) => a.value.is_none(),
        Expr::ColumnRef(cr) => column_ref_name(cr)
            .ok()
            .and_then(|n| cols.iter().find(|c| c.name == n).map(|c| c.nullable))
            .unwrap_or(true),
        Expr::ParamRef(_) => true,
        _ => true,
    }
}
