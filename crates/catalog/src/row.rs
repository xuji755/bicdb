//! **字典行的值域与编解码**（C2b）。
//!
//! 字典表是普通表 ⇒ 行格式照 `arch/06` §6.1；本模块只负责**字典行的形态**：
//!
//! ```text
//! DictValue = Null | Num(u64) | Text(String) | Bool(bool) | Bytes(Vec<u8>)
//! 行布局 = 行头 + NULL 位图 + 变长列偏移数组 + 列数据（**全部列按变长处理**）
//! ```
//!
//! **为什么全按变长**：字典表的列域很小（`INTEGER`/`VARCHAR2`/`BOOLEAN`/字节串），
//! 定长优化换不到什么，而"全变长"让编解码只有一条路径（列偏移数组承担
//! 自描述——`arch/06` §6.0 原则③）。
//!
//! **值域的宽度记档**：`Num(u64)` 覆盖字典表的一切数值列（对象号/提交序号/
//! 配额/计数——`NUMBER(38,0)` 域内；负数不出现）。列与类型的对应由
//! [`crate::dict::ColDef`] 给出，本模块**不做类型检查**（那在建区/DDL 的
//! 组装侧；这里只管字节）。

use bicdb_storage::page::Page;
use bicdb_storage::row::{RowHeader, RowView};

use crate::dict::{ColDef, ColTypeCode};

/// 一个字典值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DictValue {
    /// NULL。
    Null,
    /// 数值（字典表的一切数值列）。
    Num(u64),
    /// 文本（`VARCHAR2` 的原始字节按 UTF-8 承载；字典名一律 ASCII 级）。
    Text(String),
    /// 布尔。
    Bool(bool),
    /// 字节串（字典内部列，如 `col$.deflt`、`ind$.expr_src`）。
    Bytes(Vec<u8>),
}

/// 行编解码错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowCodecError {
    /// 值数与列数不符。
    ColumnCount {
        /// 期望（列定义）。
        expected: usize,
        /// 实际（给出的值）。
        got: usize,
    },
    /// 值形态与列类型不符。
    TypeMismatch {
        /// 列号（1 起）。
        col: u16,
        /// 列名。
        name: &'static str,
    },
    /// 行字节非法（行格式层）。
    Format(String),
}

impl std::fmt::Display for RowCodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RowCodecError::ColumnCount { expected, got } => {
                write!(f, "值数 {got} 与列数 {expected} 不符")
            }
            RowCodecError::TypeMismatch { col, name } => {
                write!(f, "第 {col} 列 {name} 的值形态与列类型不符")
            }
            RowCodecError::Format(why) => write!(f, "行格式：{why}"),
        }
    }
}

impl std::error::Error for RowCodecError {}

/// **编码一行**（列数据全按变长；NULL 走 NULL 位图）。
pub fn encode(values: &[DictValue], columns: &[ColDef]) -> Result<Vec<u8>, RowCodecError> {
    if values.len() != columns.len() {
        return Err(RowCodecError::ColumnCount {
            expected: columns.len(),
            got: values.len(),
        });
    }
    let col_count = columns.len() as u16;
    let bitmap_len = RowHeader::null_bitmap_len_for(col_count);
    let mut bitmap = vec![0u8; bitmap_len as usize];
    let mut data: Vec<u8> = Vec::new();
    let mut offsets: Vec<u32> = Vec::with_capacity(values.len());
    for (i, (v, c)) in values.iter().zip(columns).enumerate() {
        if matches!(v, DictValue::Null) {
            bitmap[i / 8] |= 1 << (i % 8);
            offsets.push(data.len() as u32);
            continue;
        }
        offsets.push(data.len() as u32);
        push_value(&mut data, v, c, (i + 1) as u16)?;
    }
    let header = RowHeader {
        flags: 0,
        itl_slot: 0,
        row_len: 0, // 先占位，末尾回填
        col_count,
        null_bitmap_len: bitmap_len,
        var_col_count: col_count,
    };
    let total = header.data_start() + data.len();
    let mut out = vec![0u8; total];
    let header = RowHeader {
        row_len: total as u32,
        ..header
    };
    header.write_into(&mut out);
    // 布局：行头(12B) │ NULL 位图 │ 变长偏移数组（4B×列数）│ 列数据。
    let bitmap_at = bicdb_storage::row::ROW_HEADER_FIXED_LEN;
    out[bitmap_at..bitmap_at + bitmap_len as usize].copy_from_slice(&bitmap);
    for (i, off) in offsets.iter().enumerate() {
        let at = header.var_offsets_start() + i * 4;
        out[at..at + 4].copy_from_slice(&off.to_le_bytes());
    }
    out[header.data_start()..].copy_from_slice(&data);
    Ok(out)
}

/// 追加一个非 NULL 值的字节（**列类型只用于形态校验**）。
fn push_value(
    data: &mut Vec<u8>,
    v: &DictValue,
    c: &ColDef,
    col: u16,
) -> Result<(), RowCodecError> {
    let bad = || RowCodecError::TypeMismatch { col, name: c.name };
    match (c.type_code, v) {
        (ColTypeCode::Number, DictValue::Num(n)) => {
            // **NUMBER 的保序编码**（`arch/06` §6.0 原则①）——字典表的索引键
            // 直接取行里这些列的字节 ⇒ 索引比较即字节比较。
            let num = bicdb_types::Number::parse(&n.to_string()).map_err(|_| bad())?;
            data.extend_from_slice(&num.encode());
        }
        (ColTypeCode::Boolean, DictValue::Bool(b)) => data.push(u8::from(*b)),
        (ColTypeCode::Varchar2, DictValue::Text(t)) => data.extend_from_slice(t.as_bytes()),
        (ColTypeCode::Bytes, DictValue::Bytes(b)) => data.extend_from_slice(b),
        // 字典表的文本列在文本形态之外也接受字节串（名字/路径一律 ASCII 级）。
        (ColTypeCode::Varchar2, DictValue::Bytes(b)) => data.extend_from_slice(b),
        _ => return Err(bad()),
    }
    Ok(())
}

/// **解码一行**（按列定义解释字节）。
pub fn decode(bytes: &[u8], columns: &[ColDef]) -> Result<Vec<DictValue>, RowCodecError> {
    let view = RowView::new(bytes).map_err(|e| RowCodecError::Format(e.to_string()))?;
    let header = view.header();
    if header.col_count as usize != columns.len() {
        return Err(RowCodecError::ColumnCount {
            expected: columns.len(),
            got: header.col_count as usize,
        });
    }
    view.validate_var_offsets(0)
        .map_err(|e| RowCodecError::Format(e.to_string()))?;
    let mut out = Vec::with_capacity(columns.len());
    for (i, c) in columns.iter().enumerate() {
        if view.is_null(i as u16) {
            out.push(DictValue::Null);
            continue;
        }
        let raw = view
            .var_column(i, 0)
            .ok_or_else(|| RowCodecError::Format("缺列字节".to_owned()))?;
        out.push(read_value(raw, c, (i + 1) as u16)?);
    }
    Ok(out)
}

/// 解释一列的非 NULL 字节。
fn read_value(raw: &[u8], c: &ColDef, col: u16) -> Result<DictValue, RowCodecError> {
    let bad = || RowCodecError::TypeMismatch { col, name: c.name };
    Ok(match c.type_code {
        ColTypeCode::Number => {
            let num = bicdb_types::Number::decode(raw).map_err(|_| bad())?;
            DictValue::Num(
                num.to_string()
                    .parse::<u64>()
                    .map_err(|_| RowCodecError::Format("数值列越出字典域".to_owned()))?,
            )
        }
        ColTypeCode::Boolean => match raw {
            [0] => DictValue::Bool(false),
            [1] => DictValue::Bool(true),
            _ => return Err(bad()),
        },
        ColTypeCode::Varchar2 => DictValue::Text(
            String::from_utf8(raw.to_vec())
                .map_err(|_| RowCodecError::Format("非 UTF-8 文本".to_owned()))?,
        ),
        ColTypeCode::Bytes => DictValue::Bytes(raw.to_vec()),
        _ => return Err(bad()),
    })
}

/// **在页里读一行**（`row_no` 从 1 起）——表访问的最小口（字典读取用）。
#[must_use]
pub fn read_from_page(page: &Page, row_no: u16) -> Option<&[u8]> {
    bicdb_storage::heap::row(page, row_no)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dict::{COL_COLS, OBJ_COLS};

    fn obj_row() -> Vec<DictValue> {
        vec![
            DictValue::Num(7),
            DictValue::Text("obj$".to_owned()),
            DictValue::Num(1),
            DictValue::Num(2),
            DictValue::Num(7),
            DictValue::Num(0),
            DictValue::Num(0),
            DictValue::Num(1),
        ]
    }

    #[test]
    fn obj_row_round_trips() {
        let row = obj_row();
        let bytes = encode(&row, OBJ_COLS).unwrap();
        assert_eq!(decode(&bytes, OBJ_COLS).unwrap(), row, "往返一致");
        // 行头自洽：行列数、位图长度、行长度。
        let view = RowView::new(&bytes).unwrap();
        assert_eq!(view.header().col_count, 8);
        assert_eq!(view.header().row_len as usize, bytes.len());
    }

    #[test]
    fn nulls_go_through_the_bitmap() {
        // col$ 的 precision/scale/deflt 可空：NULL 不进数据区。
        let row = vec![
            DictValue::Num(1),
            DictValue::Num(3),
            DictValue::Text("name".to_owned()),
            DictValue::Num(3),
            DictValue::Num(128),
            DictValue::Null,
            DictValue::Null,
            DictValue::Bool(true),
            DictValue::Null,
            DictValue::Num(0),
        ];
        let bytes = encode(&row, COL_COLS).unwrap();
        let back = decode(&bytes, COL_COLS).unwrap();
        assert_eq!(back, row);
        assert_eq!(back[5], DictValue::Null);
        assert_eq!(back[8], DictValue::Null);
        // NULL 列的偏移仍在（偏移数组长度 = 列数）。
        let view = RowView::new(&bytes).unwrap();
        assert_eq!(view.header().var_col_count as usize, COL_COLS.len());
    }

    #[test]
    fn value_shape_must_match_column_type() {
        let mut row = obj_row();
        row[1] = DictValue::Num(9); // name 列给了数值
        let err = encode(&row, OBJ_COLS).unwrap_err();
        assert!(
            matches!(err, RowCodecError::TypeMismatch { col: 2, .. }),
            "{err}"
        );
        let err2 = encode(&row[..3], OBJ_COLS).unwrap_err();
        assert!(matches!(err2, RowCodecError::ColumnCount { .. }), "{err2}");
    }

    #[test]
    fn bytes_column_accepts_byte_strings() {
        // ind$.expr_src 是字节串列：可空 + 承载任意字节。
        use crate::dict::IND_COLS;
        let row = vec![
            DictValue::Num(9),
            DictValue::Num(2),
            DictValue::Num(0),
            DictValue::Num(1),
            DictValue::Bool(true),
            DictValue::Num(1),
            DictValue::Bytes(vec![0x6a, 0x73, 0x6f, 0x6e]), // "json"
        ];
        let bytes = encode(&row, IND_COLS).unwrap();
        assert_eq!(decode(&bytes, IND_COLS).unwrap(), row);
    }
}
