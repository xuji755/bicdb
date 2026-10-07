//! **线上的值模型**（传输层；**不认识引擎的值类型**）。
//!
//! ```text
//! 一个单元： <标记> <载荷字节数> \n <载荷>
//!   标记： - NULL │ n 数值（文本，十进制） │ o 布尔（"0"/"1"） │ b 字节串（**十六进制**）
//! ```
//!
//! **为什么这样定**：
//! - **无损**：字节串走十六进制（ASCII 安全），不像"统一转字符串"那样
//!   把非 UTF-8 的字节悄悄毁掉（驱动拿到的是原字节）；
//! - **不过度设计**：数值以十进制文本过线（SQL 的 `NUMBER` 本来任意精度，
//!   文本是唯一不丢精度又不引依赖的形态）；驱动的"原生类型"转换是**驱动侧**的事
//!   （Python `Decimal`、Rust `i64/f64`）；
//! - **无依赖**：本 crate 不带引擎，也不带序列化库。

/// 线上的一个值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// 空。
    Null,
    /// 数值（十进制文本，保持精度）。
    Number(String),
    /// 布尔。
    Bool(bool),
    /// 字节串（原字节）。
    Bytes(Vec<u8>),
}

impl Value {
    /// 是不是空。
    #[must_use]
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// 数值文本（非数值 ⇒ `None`）。
    #[must_use]
    pub fn as_number_text(&self) -> Option<&str> {
        match self {
            Value::Number(t) => Some(t),
            _ => None,
        }
    }

    /// 取 `i64`（数值且能整出）。
    #[must_use]
    pub fn as_i64(&self) -> Option<i64> {
        self.as_number_text()?.parse().ok()
    }

    /// 取 `f64`。
    #[must_use]
    pub fn as_f64(&self) -> Option<f64> {
        self.as_number_text()?.parse().ok()
    }

    /// 取布尔。
    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// 取字节串。
    #[must_use]
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }

    /// 按 UTF-8 解释字节串（非字节串或非 UTF-8 ⇒ `None`）。
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        std::str::from_utf8(self.as_bytes()?).ok()
    }

    /// 追加到编码缓冲（一个单元）。
    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Value::Null => {
                out.extend_from_slice(b"-0\n");
            }
            Value::Number(t) => {
                out.extend_from_slice(format!("n{}\n", t.len()).as_bytes());
                out.extend_from_slice(t.as_bytes());
            }
            Value::Bool(b) => {
                let s = if *b { "o1\n1" } else { "o1\n0" };
                out.extend_from_slice(s.as_bytes());
            }
            Value::Bytes(b) => {
                let hex = hex_encode(b);
                out.extend_from_slice(format!("b{}\n", hex.len()).as_bytes());
                out.extend_from_slice(hex.as_bytes());
            }
        }
    }

    /// 从单元输入解一个值（`it` 是**行**切分后的片段游标）。
    #[must_use]
    pub fn decode(tag: u8, text: &str) -> Value {
        match tag {
            b'-' => Value::Null,
            b'n' => Value::Number(text.to_owned()),
            b'o' => Value::Bool(text == "1"),
            b'b' => Value::Bytes(hex_decode(text)),
            _ => Value::Bytes(text.as_bytes().to_vec()),
        }
    }
}

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

/// 十六进制编码（小写）。
#[must_use]
pub fn hex_encode(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

/// 十六进制解码（非法字符按 0 处理——**只在自家编码上使用**）。
#[must_use]
pub fn hex_decode(s: &str) -> Vec<u8> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut i = 0;
    while i + 1 < bytes.len() {
        let hi = (bytes[i] as char).to_digit(16).unwrap_or(0) as u8;
        let lo = (bytes[i + 1] as char).to_digit(16).unwrap_or(0) as u8;
        out.push((hi << 4) | lo);
        i += 2;
    }
    out
}
