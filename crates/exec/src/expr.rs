//! **表达式与三值逻辑**（切片 1 子集）。
//!
//! 设计依据：`doc/执行算子设计_v0.1.md` §2.5——谓词求值走三值逻辑：
//! `NULL` 参与的比较 ⇒ `UNKNOWN`；`WHERE`/`HAVING` **只放行 TRUE**。
//! 表达式树与求值内核的完整形态随 `TYP` 切片扩展（算术、CAST、CASE、IN、
//! BETWEEN、COALESCE 等在切片 2）；本切片收：列引用、字面量、参数、
//! 比较、`IS [NOT] NULL`、`AND`/`OR`/`NOT`。

use crate::error::ExecError;
use crate::value::{Row, Value};

/// 比较算子（`= <> < <= > >=`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
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
}

/// **谓词/表达式的三值结果**（`AND`/`OR` 的折叠域）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Truth {
    /// 真。
    True,
    /// 假。
    False,
    /// 未知（NULL）。
    Unknown,
}

impl Truth {
    /// 由值取真值：`Bool` ⇒ 真/假；`NULL` ⇒ 未知；其他类型 ⇒ 类型错误。
    fn of(value: &Value) -> Result<Truth, ExecError> {
        match value {
            Value::Null => Ok(Truth::Unknown),
            Value::Bool(true) => Ok(Truth::True),
            Value::Bool(false) => Ok(Truth::False),
            other => Err(ExecError::TypeMismatch {
                expected: "BOOLEAN",
                got: other.type_name(),
            }),
        }
    }

    /// 只放行 TRUE（`WHERE`/`HAVING`/连接谓词的判定口径）。
    #[must_use]
    pub fn passes(self) -> bool {
        matches!(self, Truth::True)
    }
}

/// 表达式（切片 1 子集）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    /// 列引用（对本行）。
    Column(usize),
    /// 字面量。
    Literal(Value),
    /// 参数（`:name` 的执行期形态——绑定后按序编号）。
    Param(usize),
    /// 比较（两侧任一为 NULL ⇒ `Unknown`）。
    Compare {
        /// 比较算子。
        op: CmpOp,
        /// 左操作数。
        left: Box<Expr>,
        /// 右操作数。
        right: Box<Expr>,
    },
    /// `IS [NOT] NULL`（**永不**产生 Unknown——这正是它的用处）。
    IsNull {
        /// 操作数。
        expr: Box<Expr>,
        /// 是否 `NOT`。
        negated: bool,
    },
    /// `NOT`（三值取反：`NOT Unknown = Unknown`）。
    Not(Box<Expr>),
    /// `AND`（折叠：任一 FALSE ⇒ FALSE；否则任一 UNKNOWN ⇒ UNKNOWN；否则 TRUE）。
    And(Vec<Expr>),
    /// `OR`（折叠：任一 TRUE ⇒ TRUE；否则任一 UNKNOWN ⇒ UNKNOWN；否则 FALSE）。
    Or(Vec<Expr>),
}

/// 求值（返回**值**——用于投影；谓词请用 [`eval_truth`]）。
pub fn eval(expr: &Expr, row: &Row, params: &[Value]) -> Result<Value, ExecError> {
    match expr {
        Expr::Column(i) => Ok(row.get(*i)?.clone()),
        Expr::Literal(v) => Ok(v.clone()),
        Expr::Param(i) => params.get(*i).cloned().ok_or(ExecError::ParamOutOfRange {
            index: *i,
            count: params.len(),
        }),
        Expr::IsNull { expr, negated } => {
            let v = eval(expr, row, params)?;
            Ok(Value::Bool(v.is_null() != *negated))
        }
        Expr::Not(inner) => match eval_truth(inner, row, params)? {
            Truth::True => Ok(Value::Bool(false)),
            Truth::False => Ok(Value::Bool(true)),
            Truth::Unknown => Ok(Value::Null),
        },
        Expr::And(list) => {
            let mut unknown = false;
            for e in list {
                match eval_truth(e, row, params)? {
                    Truth::False => return Ok(Value::Bool(false)),
                    Truth::Unknown => unknown = true,
                    Truth::True => {}
                }
            }
            Ok(if unknown {
                Value::Null
            } else {
                Value::Bool(true)
            })
        }
        Expr::Or(list) => {
            let mut unknown = false;
            for e in list {
                match eval_truth(e, row, params)? {
                    Truth::True => return Ok(Value::Bool(true)),
                    Truth::Unknown => unknown = true,
                    Truth::False => {}
                }
            }
            Ok(if unknown {
                Value::Null
            } else {
                Value::Bool(false)
            })
        }
        Expr::Compare { op, left, right } => {
            let a = eval(left, row, params)?;
            let b = eval(right, row, params)?;
            match compare_values(&a, &b)? {
                None => Ok(Value::Null), // NULL 参与 ⇒ Unknown（以 NULL 值表示）
                Some(ord) => Ok(Value::Bool(apply_cmp(*op, ord))),
            }
        }
    }
}

/// 求**真值**（谓词口径；`AND`/`OR`/`NOT` 需要三态而非值）。
pub fn eval_truth(expr: &Expr, row: &Row, params: &[Value]) -> Result<Truth, ExecError> {
    match expr {
        Expr::Compare { .. } | Expr::IsNull { .. } => Truth::of(&eval(expr, row, params)?),
        Expr::Not(_) | Expr::And(_) | Expr::Or(_) => Truth::of(&eval(expr, row, params)?),
        // 裸真值表达式（`WHERE flag`）：列/字面量/参数按 BOOLEAN 判。
        Expr::Column(_) | Expr::Literal(_) | Expr::Param(_) => Truth::of(&eval(expr, row, params)?),
    }
}

/// 谓词求值：**只放行 TRUE**。
pub fn eval_where(expr: &Expr, row: &Row, params: &[Value]) -> Result<bool, ExecError> {
    Ok(eval_truth(expr, row, params)?.passes())
}

/// **值比较**（`None` = 有一侧是 NULL ⇒ 未知）。
///
/// 同类型才可比：`NUMBER` 按数值序、`BOOLEAN` 假 < 真、`BYTES` 按**字节序**
/// （`CONV` §1.3：不加引号标识符与文本比较都按字节序）。
pub fn compare_values(a: &Value, b: &Value) -> Result<Option<std::cmp::Ordering>, ExecError> {
    match (a, b) {
        (Value::Null, _) | (_, Value::Null) => Ok(None),
        (Value::Number(x), Value::Number(y)) => Ok(Some(x.cmp(y))),
        (Value::Bool(x), Value::Bool(y)) => Ok(Some(x.cmp(y))),
        (Value::Bytes(x), Value::Bytes(y)) => Ok(Some(x.as_slice().cmp(y.as_slice()))),
        (x, y) => Err(ExecError::TypeMismatch {
            expected: x.type_name(),
            got: y.type_name(),
        }),
    }
}

fn apply_cmp(op: CmpOp, ord: std::cmp::Ordering) -> bool {
    use std::cmp::Ordering;
    match op {
        CmpOp::Eq => ord == Ordering::Equal,
        CmpOp::Ne => ord != Ordering::Equal,
        CmpOp::Lt => ord == Ordering::Less,
        CmpOp::Le => ord != Ordering::Greater,
        CmpOp::Gt => ord == Ordering::Greater,
        CmpOp::Ge => ord != Ordering::Less,
    }
}
