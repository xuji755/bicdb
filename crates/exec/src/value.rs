//! **执行器的值域**（切片 1 子集：已落地编码的类型）。
//!
//! 设计依据：`doc/执行算子设计_v0.1.md` §1.3——行 = 解码后的值序列；
//! 算子之间传值、不传页。值域的完整形态随 `TYP` 内核（表达式与类型切片）
//! 扩展；本切片只收已落地编码：`NUMBER`（[`bicdb_types::Number`]）、
//! `BOOLEAN`、以及**未解码字节串**（比较按字节序，`CONV` §1.3）。
//!
//! **行布局约定（切片 1）**：全部列走变长区（`fixed_len = 0`）——定长列的
//! 表达随行格式/TYP 切片扩展；[`ColKind`] 只给"怎么解码"，不给存储布局。

use bicdb_storage::row::RowView;
use bicdb_types::Number;

use crate::error::ExecError;

/// 一列的值形态（解码依据；由**列定义**给出——行内不自描述，§6.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColKind {
    /// `NUMBER` 变长编码（base-100 保序）。
    Number,
    /// `BOOLEAN`（1 字节）。
    Bool,
    /// 未解码字节串（文本/JSON/等；比较按字节序）。
    Bytes,
}

/// **值**（三值逻辑：NULL 是一等值，不是"缺省"）。
/// `Hash` 供聚合分组键与 `DISTINCT` 已见值集用（`NULL` **同类即同键**——
/// 分组与去重的相等比 `WHERE` 的三值比较更宽，见设计 §2.7 的等价类口径）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Value {
    /// SQL NULL。
    Null,
    /// 布尔。
    Bool(bool),
    /// 数值（Oracle 兼容编码的内在形态）。
    Number(Number),
    /// 字节串（未解码；文本/二进制同形）。
    Bytes(Vec<u8>),
}

impl Value {
    /// 是否 NULL（谓词求值的常用判定）。
    #[must_use]
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// 类型名（错误文案用）。
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "NULL",
            Value::Bool(_) => "BOOLEAN",
            Value::Number(_) => "NUMBER",
            Value::Bytes(_) => "BYTES",
        }
    }
}

/// **行形状**（列数 + 各列形态；由绑定期给出，执行器不猜）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowShape {
    /// 各列形态。
    pub cols: Vec<ColKind>,
}

impl RowShape {
    /// 构造。
    #[must_use]
    pub fn new(cols: Vec<ColKind>) -> Self {
        Self { cols }
    }

    /// 列数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.cols.len()
    }

    /// 零列（防御性）。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cols.is_empty()
    }
}

/// **一行**（解码后的值序列）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// 各列的值。
    pub values: Vec<Value>,
}

impl Row {
    /// 构造。
    #[must_use]
    pub fn new(values: Vec<Value>) -> Self {
        Self { values }
    }

    /// 取列值（越界 ⇒ 具名错误）。
    pub fn get(&self, col: usize) -> Result<&Value, ExecError> {
        self.values
            .get(col)
            .ok_or(ExecError::RowShapeMismatch { col })
    }
}

/// **解码一行**（存储行字节 → 值序列；NULL 位图优先）。
///
/// 列值取自行内**变长区**（`fixed_len = 0` 布局约定，见模块文档）；
/// 解码严格：`NUMBER`/`BOOLEAN` 的字节不合规范即报错（不静默兜底）。
pub fn decode_row(bytes: &[u8], shape: &RowShape) -> Result<Row, ExecError> {
    let view = RowView::new(bytes).map_err(|e| ExecError::BadStoredRow(e.to_string()))?;
    let mut values = Vec::with_capacity(shape.len());
    for (i, kind) in shape.cols.iter().enumerate() {
        if view.is_null(i as u16) {
            values.push(Value::Null);
            continue;
        }
        let raw = view
            .var_column(i, 0)
            .ok_or(ExecError::RowShapeMismatch { col: i })?;
        let value = match kind {
            ColKind::Number => Value::Number(
                Number::decode(raw).map_err(|e| ExecError::BadStoredRow(e.to_string()))?,
            ),
            ColKind::Bool => Value::Bool(
                bicdb_types::decode_boolean(raw)
                    .map_err(|e| ExecError::BadStoredRow(e.to_string()))?,
            ),
            ColKind::Bytes => Value::Bytes(raw.to_vec()),
        };
        values.push(value);
    }
    Ok(Row::new(values))
}

/// **编码一行**（切片 1 的对称口；测试与直译路径用）。
///
/// 与 [`decode_row`] 同一布局约定（全变长）。NULL 走空列 + NULL 位图。
pub fn encode_row(row: &Row, shape: &RowShape) -> Result<Vec<u8>, ExecError> {
    if row.values.len() != shape.len() {
        return Err(ExecError::RowShapeMismatch {
            col: row.values.len(),
        });
    }
    let mut nulls = Vec::with_capacity(row.values.len());
    let mut var: Vec<Vec<u8>> = Vec::with_capacity(row.values.len());
    for (v, kind) in row.values.iter().zip(shape.cols.iter()) {
        match v {
            Value::Null => {
                nulls.push(true);
                var.push(Vec::new());
            }
            Value::Number(n) => {
                expect_kind(*kind, ColKind::Number)?;
                nulls.push(false);
                var.push(n.encode());
            }
            Value::Bool(b) => {
                expect_kind(*kind, ColKind::Bool)?;
                nulls.push(false);
                var.push(bicdb_types::encode_boolean(*b).to_vec());
            }
            Value::Bytes(bytes) => {
                expect_kind(*kind, ColKind::Bytes)?;
                nulls.push(false);
                var.push(bytes.clone());
            }
        }
    }
    let refs: Vec<&[u8]> = var.iter().map(Vec::as_slice).collect();
    bicdb_storage::row::assemble_row(0, 0, &nulls, &[], &refs)
        .map_err(|e| ExecError::BadStoredRow(e.to_string()))
}

fn expect_kind(actual: ColKind, expected: ColKind) -> Result<(), ExecError> {
    if actual == expected {
        return Ok(());
    }
    Err(ExecError::TypeMismatch {
        expected: kind_name(expected),
        got: kind_name(actual),
    })
}

/// 列形态的类型名（错误文案用）。
#[must_use]
pub fn kind_name(kind: ColKind) -> &'static str {
    match kind {
        ColKind::Number => "NUMBER",
        ColKind::Bool => "BOOLEAN",
        ColKind::Bytes => "BYTES",
    }
}

/// **一行的估计内存**（工作内存记账用；保守估计 + 固定开销）。
#[must_use]
pub fn row_bytes(row: &Row) -> usize {
    let mut n = 16; // 行结构开销
    for v in &row.values {
        n += value_bytes(v);
    }
    n
}

/// **一个值的估计内存**（含容器开销；`HashSet`/`HashMap` 键的内存记账用）。
#[must_use]
pub fn value_bytes(v: &Value) -> usize {
    match v {
        Value::Null => 0,
        Value::Bool(_) => 1,
        Value::Number(num) => num.encoded_len().max(8),
        Value::Bytes(b) => b.len().max(8),
    }
}

/// **显式转换**（`CAST`；切片 2b 的确定面）。
///
/// 转换的**结果**是契约（`TYP` REQ-TYP-004）。已实现且语义明确的：
/// - `→ NUMBER`：`BYTES` 按**严格十进制文本**解析（`Number::parse` 同规；
///   不静默兜底）、`NUMBER` 恒等；
/// - `→ BYTES`：`NUMBER` 取**规范十进制文本**（`.` 为小数点；NLS 面
///   记录为未核验）、`BYTES` 恒等；
/// - `NULL` 恒转 `NULL`；
/// - **其余方向（含 `BOOLEAN` 两向）具名拒绝**——参考环境的转换细则
///   （Oracle SQL 层 BOOLEAN 为 23c 时代特性）未核验，不凭记忆补语义。
pub fn cast_value(value: &Value, to: ColKind) -> Result<Value, ExecError> {
    if value.is_null() {
        return Ok(Value::Null);
    }
    match (value, to) {
        (Value::Number(n), ColKind::Number) => Ok(Value::Number(n.clone())),
        (Value::Bytes(b), ColKind::Number) => {
            let text = std::str::from_utf8(b).map_err(|_| ExecError::TypeMismatch {
                expected: "NUMBER",
                got: "BYTES",
            })?;
            Number::parse(text)
                .map(Value::Number)
                .map_err(|e| ExecError::BadStoredRow(e.to_string()))
        }
        (Value::Number(n), ColKind::Bytes) => Ok(Value::Bytes(n.to_decimal_string().into_bytes())),
        (Value::Bytes(b), ColKind::Bytes) => Ok(Value::Bytes(b.clone())),
        (Value::Bool(b), ColKind::Bool) => Ok(Value::Bool(*b)),
        (v, k) => Err(ExecError::TypeMismatch {
            expected: kind_name(k),
            got: v.type_name(),
        }),
    }
}
