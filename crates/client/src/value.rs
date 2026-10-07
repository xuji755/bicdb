//! **驱动的值模型**（协议 §3 的 API 面：四种形态，不认识引擎类型）。
//!
//! ```text
//! Null │ Number(十进制文本) │ Bool │ Bytes(原字节)
//! ```
//!
//! **为什么数值留文本**：SQL 的 `NUMBER` 任意精度——转 `f64` 会丢精度，
//! 转 `i64` 会溢出。这里**原样保留**，由调用方按需取（[`Value::as_i64`] /
//! [`Value::as_f64`] / [`Value::as_decimal`]）；这个选择与本仓的
//! "不许静默损坏数据"是同一条纪律。
//!
//! **字节串不自动按 UTF-8 解释**：文本列与二进制列在线上同形，
//! 驱动**不替用户猜**——要文本就调 [`Value::as_str`]（非 UTF-8 ⇒ `None`），
//! 要原字节就用 [`Value::as_bytes`]。

/// 一个值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// `NULL`。
    Null,
    /// 数值（十进制文本，**保精度**）。
    Number(String),
    /// 布尔。
    Bool(bool),
    /// 字节串（原字节；文本列也是这个形态）。
    Bytes(Vec<u8>),
}

impl Value {
    /// 是不是 `NULL`。
    #[must_use]
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// 数值的十进制文本。
    #[must_use]
    pub fn as_decimal(&self) -> Option<&str> {
        match self {
            Value::Number(t) => Some(t),
            _ => None,
        }
    }

    /// 取 `i64`（数值且能整出；溢出/带小数 ⇒ `None`）。
    #[must_use]
    pub fn as_i64(&self) -> Option<i64> {
        self.as_decimal()?.parse().ok()
    }

    /// 取 `f64`（**可能丢精度**；要精确值用 [`Value::as_decimal`]）。
    #[must_use]
    pub fn as_f64(&self) -> Option<f64> {
        self.as_decimal()?.parse().ok()
    }

    /// 取布尔。
    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// 取原字节。
    #[must_use]
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }

    /// 按 UTF-8 取文本（**非字节串或非 UTF-8 ⇒ `None`**——不替换成 `�`）。
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        std::str::from_utf8(self.as_bytes()?).ok()
    }

    /// 形态名（错误信息里用）。
    #[must_use]
    pub fn kind_name(&self) -> &'static str {
        match self {
            Value::Null => "NULL",
            Value::Number(_) => "数值",
            Value::Bool(_) => "布尔",
            Value::Bytes(_) => "字节串",
        }
    }
}

// ── 参数书写（`conn.query("… :a", &[("a", 1i64.into())])`） ──

impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::Bytes(v.as_bytes().to_vec())
    }
}

impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::Bytes(v.into_bytes())
    }
}

impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::Number(v.to_string())
    }
}

impl From<i32> for Value {
    fn from(v: i32) -> Self {
        Value::Number(v.to_string())
    }
}

impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Value::Number(v.to_string())
    }
}

impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Bool(v)
    }
}

impl From<Vec<u8>> for Value {
    fn from(v: Vec<u8>) -> Self {
        Value::Bytes(v)
    }
}

impl From<&[u8]> for Value {
    fn from(v: &[u8]) -> Self {
        Value::Bytes(v.to_vec())
    }
}

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Self {
        v.map_or(Value::Null, Into::into)
    }
}

/// 显示形态（与 CLI 的呈现口径一致；**仅供人看**，不要拿它做解析）。
impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::Null => f.write_str("NULL"),
            Value::Number(t) => f.write_str(t),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Bytes(b) => match std::str::from_utf8(b) {
                Ok(s) => f.write_str(s),
                Err(_) => {
                    for x in b {
                        write!(f, "{x:02x}")?;
                    }
                    Ok(())
                }
            },
        }
    }
}
