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

/// 算术算子（`+ - * /`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithOp {
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`
    Div,
}

/// 表达式（切片 2b 面：算术 / `CAST` / `CASE` / `IN` / `BETWEEN` /
/// `COALESCE` / `NULLIF`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    /// 列引用（对本行）。
    Column(usize),
    /// 字面量。
    Literal(Value),
    /// 参数（`:name` 的执行期形态——绑定后按序编号）。
    Param(usize),
    /// 算术（`NUMBER` 十进制运算——`TYP` 内核，不经浮点；任一侧 NULL ⇒ NULL）。
    Arith {
        /// 算子。
        op: ArithOp,
        /// 左操作数。
        left: Box<Expr>,
        /// 右操作数。
        right: Box<Expr>,
    },
    /// 一元负号。
    Neg(Box<Expr>),
    /// 显式转换（结果语义见 [`crate::value::cast_value`]）。
    Cast {
        /// 操作数。
        expr: Box<Expr>,
        /// 目标列形态。
        to: crate::value::ColKind,
    },
    /// 搜索式 `CASE WHEN … THEN … [ELSE …] END`（首个为 TRUE 的分支胜出；
    /// 无 ELSE 且全不中 ⇒ NULL）。
    Case {
        /// `(WHEN 条件, THEN 值)` 列表（按序求值）。
        whens: Vec<(Expr, Expr)>,
        /// `ELSE`（缺省 = NULL）。
        else_: Option<Box<Expr>>,
    },
    /// `expr IN (v1, …, vn)`（三值：任一等 ⇒ TRUE；无 TRUE 但有 UNKNOWN ⇒
    /// UNKNOWN；否则 FALSE；`expr` 为 NULL ⇒ UNKNOWN）。`NOT IN` 经 [`Expr::Not`]。
    InList {
        /// 左侧表达式。
        expr: Box<Expr>,
        /// 值列表（`<字面量列表>`——SQL 面清单）。
        list: Vec<Expr>,
    },
    /// `expr BETWEEN low AND high`（≡ `expr >= low AND expr <= high`，
    /// 三值逻辑同 `AND`）。
    Between {
        /// 被检表达式。
        expr: Box<Expr>,
        /// 下界。
        low: Box<Expr>,
        /// 上界。
        high: Box<Expr>,
    },
    /// `COALESCE(a, b, …)`：首个非 NULL；全 NULL ⇒ NULL。
    Coalesce(Vec<Expr>),
    /// `NULLIF(a, b)`：`a = b` 为真 ⇒ NULL；否则 `a`（比较为 UNKNOWN ⇒ `a`）。
    NullIf {
        /// 左操作数。
        left: Box<Expr>,
        /// 右操作数。
        right: Box<Expr>,
    },
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
        Expr::Arith { op, left, right } => {
            let a = eval(left, row, params)?;
            let b = eval(right, row, params)?;
            match (a, b) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Number(x), Value::Number(y)) => {
                    let r = match op {
                        ArithOp::Add => x.add(&y),
                        ArithOp::Sub => x.sub(&y),
                        ArithOp::Mul => x.mul(&y),
                        ArithOp::Div => x.div(&y),
                    };
                    match r {
                        Ok(n) => Ok(Value::Number(n)),
                        Err(bicdb_types::NumberError::DivisionByZero) => {
                            Err(ExecError::DivisionByZero)
                        }
                        Err(_) => Err(ExecError::NumericOverflow),
                    }
                }
                (v, _) => Err(ExecError::TypeMismatch {
                    expected: "NUMBER",
                    got: v.type_name(),
                }),
            }
        }
        Expr::Neg(inner) => match eval(inner, row, params)? {
            Value::Null => Ok(Value::Null),
            Value::Number(n) => Ok(Value::Number(n.neg())),
            v => Err(ExecError::TypeMismatch {
                expected: "NUMBER",
                got: v.type_name(),
            }),
        },
        Expr::Cast { expr, to } => {
            let v = eval(expr, row, params)?;
            crate::value::cast_value(&v, *to)
        }
        Expr::Case { whens, else_ } => {
            for (cond, then) in whens {
                if eval_truth(cond, row, params)?.passes() {
                    return eval(then, row, params);
                }
            }
            match else_ {
                Some(e) => eval(e, row, params),
                None => Ok(Value::Null),
            }
        }
        Expr::InList { expr, list } => {
            let x = eval(expr, row, params)?;
            if x.is_null() {
                return Ok(Value::Null);
            }
            let mut unknown = false;
            for item in list {
                let v = eval(item, row, params)?;
                match compare_values(&x, &v)? {
                    Some(std::cmp::Ordering::Equal) => return Ok(Value::Bool(true)),
                    None => unknown = true,
                    Some(_) => {}
                }
            }
            Ok(if unknown {
                Value::Null
            } else {
                Value::Bool(false)
            })
        }
        Expr::Between { expr, low, high } => {
            let x = eval(expr, row, params)?;
            let lo = eval(low, row, params)?;
            let hi = eval(high, row, params)?;
            // ≡ (x >= low) AND (x <= high)：三值折叠与 AND 同规。
            let ge = match compare_values(&x, &lo)? {
                None => Truth::Unknown,
                Some(ord) => Truth::of(&Value::Bool(apply_cmp(CmpOp::Ge, ord)))?,
            };
            if ge == Truth::False {
                return Ok(Value::Bool(false));
            }
            let le = match compare_values(&x, &hi)? {
                None => Truth::Unknown,
                Some(ord) => Truth::of(&Value::Bool(apply_cmp(CmpOp::Le, ord)))?,
            };
            Ok(match (ge, le) {
                (_, Truth::False) => Value::Bool(false),
                (Truth::True, Truth::True) => Value::Bool(true),
                _ => Value::Null,
            })
        }
        Expr::Coalesce(list) => {
            for e in list {
                let v = eval(e, row, params)?;
                if !v.is_null() {
                    return Ok(v);
                }
            }
            Ok(Value::Null)
        }
        Expr::NullIf { left, right } => {
            let a = eval(left, row, params)?;
            let b = eval(right, row, params)?;
            match compare_values(&a, &b)? {
                Some(std::cmp::Ordering::Equal) => Ok(Value::Null),
                _ => Ok(a), // 不等或 UNKNOWN ⇒ a（NULLIF 的标准语义）
            }
        }
    }
}

/// 求**真值**（谓词口径；`AND`/`OR`/`NOT` 需要三态而非值）。
pub fn eval_truth(expr: &Expr, row: &Row, params: &[Value]) -> Result<Truth, ExecError> {
    // 一切表达式都按其**值**取真值：Bool ⇒ 真/假、NULL ⇒ 未知、其余类型
    // （如 `WHERE 1+1`）⇒ 类型错误——不做隐式真值化（`TYP` 纪律）。
    Truth::of(&eval(expr, row, params)?)
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
        (Value::GraphElement(x), Value::GraphElement(y)) => Ok(Some(x.cmp(y))),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Row;

    fn n(t: &str) -> Value {
        Value::Number(bicdb_types::Number::parse(t).unwrap())
    }

    fn b(v: &str) -> Value {
        Value::Bytes(v.as_bytes().to_vec())
    }

    fn null_row() -> Row {
        Row::new(vec![Value::Null, Value::Null])
    }

    fn row2(a: Value, b: Value) -> Row {
        Row::new(vec![a, b])
    }

    fn lit(v: Value) -> Expr {
        Expr::Literal(v)
    }

    fn ev(e: &Expr, row: &Row) -> Value {
        eval(e, row, &[]).unwrap()
    }

    fn truth(e: &Expr, row: &Row) -> Truth {
        eval_truth(e, row, &[]).unwrap()
    }

    #[test]
    fn arithmetic_uses_decimal_number_not_float() {
        let e = Expr::Arith {
            op: ArithOp::Add,
            left: Box::new(lit(n("0.1"))),
            right: Box::new(lit(n("0.2"))),
        };
        assert_eq!(ev(&e, &null_row()), n("0.3"), "0.1+0.2 = 0.3 精确");
        // NULL 传染。
        let e2 = Expr::Arith {
            op: ArithOp::Mul,
            left: Box::new(lit(Value::Null)),
            right: Box::new(lit(n("9"))),
        };
        assert!(ev(&e2, &null_row()).is_null());
        // 除零 ⇒ 具名错误。
        let e3 = Expr::Arith {
            op: ArithOp::Div,
            left: Box::new(lit(n("1"))),
            right: Box::new(lit(n("0"))),
        };
        assert!(matches!(
            eval(&e3, &null_row(), &[]).unwrap_err(),
            crate::error::ExecError::DivisionByZero
        ));
        // 非数值 ⇒ 类型错误。
        let e4 = Expr::Arith {
            op: ArithOp::Add,
            left: Box::new(lit(b("x"))),
            right: Box::new(lit(n("1"))),
        };
        assert!(matches!(
            eval(&e4, &null_row(), &[]).unwrap_err(),
            crate::error::ExecError::TypeMismatch { .. }
        ));
        // 一元负号。
        let e5 = Expr::Neg(Box::new(lit(n("3.5"))));
        assert_eq!(ev(&e5, &null_row()), n("-3.5"));
    }

    #[test]
    fn in_list_and_between_follow_three_valued_logic() {
        // 10 IN (1, 10, 20) ⇒ TRUE；10 IN (1, 20) ⇒ FALSE；
        // 10 IN (1, NULL) ⇒ UNKNOWN；NULL IN (1) ⇒ UNKNOWN。
        let inl = |v: Value, list: Vec<Value>| Expr::InList {
            expr: Box::new(lit(v)),
            list: list.into_iter().map(lit).collect(),
        };
        let r = null_row();
        assert_eq!(
            ev(&inl(n("10"), vec![n("1"), n("10"), n("20")]), &r),
            Value::Bool(true)
        );
        assert_eq!(
            ev(&inl(n("10"), vec![n("1"), n("20")]), &r),
            Value::Bool(false)
        );
        assert!(ev(&inl(n("10"), vec![n("1"), Value::Null]), &r).is_null());
        assert!(ev(&inl(Value::Null, vec![n("1")]), &r).is_null());
        // 命中优先于 UNKNOWN：10 IN (10, NULL) ⇒ TRUE。
        assert_eq!(
            ev(&inl(n("10"), vec![n("10"), Value::Null]), &r),
            Value::Bool(true)
        );

        // BETWEEN：闭区间；NULL 任一侧 ⇒ UNKNOWN；越界 ⇒ FALSE。
        let bt = |v: Value| Expr::Between {
            expr: Box::new(lit(v)),
            low: Box::new(lit(n("2"))),
            high: Box::new(lit(n("5"))),
        };
        assert_eq!(ev(&bt(n("3")), &r), Value::Bool(true));
        assert_eq!(ev(&bt(n("5")), &r), Value::Bool(true));
        assert_eq!(ev(&bt(n("6")), &r), Value::Bool(false));
        assert!(ev(&bt(Value::Null), &r).is_null());
    }

    #[test]
    fn case_coalesce_nullif_semantics() {
        let r = null_row();
        // CASE：首个 WHEN 为 TRUE 的分支胜出；全不中且无 ELSE ⇒ NULL。
        let case = Expr::Case {
            whens: vec![
                (lit(Value::Bool(false)), lit(n("1"))),
                (lit(Value::Bool(true)), lit(n("2"))),
                (lit(Value::Bool(true)), lit(n("3"))),
            ],
            else_: Some(Box::new(lit(n("9")))),
        };
        assert_eq!(ev(&case, &r), n("2"));
        let case_no_else = Expr::Case {
            whens: vec![(lit(Value::Bool(false)), lit(n("1")))],
            else_: None,
        };
        assert!(ev(&case_no_else, &r).is_null());
        // WHEN 为 UNKNOWN ⇒ 不中（三值）。
        let case_unknown = Expr::Case {
            whens: vec![(lit(Value::Null), lit(n("1")))],
            else_: Some(Box::new(lit(n("8")))),
        };
        assert_eq!(ev(&case_unknown, &r), n("8"));

        // COALESCE / NULLIF。
        assert_eq!(
            ev(
                &Expr::Coalesce(vec![lit(Value::Null), lit(n("7")), lit(n("8"))]),
                &r
            ),
            n("7")
        );
        assert_eq!(
            Expr::NullIf {
                left: Box::new(lit(n("5"))),
                right: Box::new(lit(n("5"))),
            }
            .pipe(|e| ev(&e, &r)),
            Value::Null
        );
        assert_eq!(
            Expr::NullIf {
                left: Box::new(lit(n("5"))),
                right: Box::new(lit(n("6"))),
            }
            .pipe(|e| ev(&e, &r)),
            n("5")
        );
        // NULLIF(NULL, x) = NULL（左值为 NULL ⇒ 返回左值）。
        assert!(Expr::NullIf {
            left: Box::new(lit(Value::Null)),
            right: Box::new(lit(n("1"))),
        }
        .pipe(|e| ev(&e, &r))
        .is_null());
    }

    #[test]
    fn cast_semantics_and_rejections() {
        use crate::value::{cast_value, ColKind};
        // BYTES → NUMBER（严格解析）；NUMBER → BYTES（规范文本）。
        assert_eq!(
            cast_value(&b("123.45"), ColKind::Number).unwrap(),
            n("123.45")
        );
        assert!(
            cast_value(&b("abc"), ColKind::Number).is_err(),
            "严格：非法文本报错"
        );
        assert_eq!(
            cast_value(&n("0.5"), ColKind::Bytes).unwrap(),
            Value::Bytes(b"0.5".to_vec())
        );
        // NULL 恒转 NULL。
        assert!(cast_value(&Value::Null, ColKind::Number).unwrap().is_null());
        // BOOLEAN 两向未核验 ⇒ 具名拒绝（不凭记忆补语义）。
        assert!(cast_value(&Value::Bool(true), ColKind::Number).is_err());
        assert!(cast_value(&n("1"), ColKind::Bool).is_err());
    }

    #[test]
    fn predicate_truth_never_implicitly_coerces() {
        let r = row2(n("1"), b("x"));
        // 裸数值谓词不做隐式真值化 ⇒ 类型错误。
        assert!(eval_truth(&Expr::Column(0), &r, &[]).is_err());
        // BOOLEAN 列正常。
        let r2 = row2(Value::Bool(true), Value::Null);
        assert_eq!(truth(&Expr::Column(0), &r2), Truth::True);
        assert_eq!(truth(&Expr::Column(1), &r2), Truth::Unknown);
    }

    // 便捷：把表达式喂进闭包（可读性小助手）。
    trait Pipe: Sized {
        fn pipe<R>(self, f: impl FnOnce(Self) -> R) -> R {
            f(self)
        }
    }
    impl<T> Pipe for T {}
}
