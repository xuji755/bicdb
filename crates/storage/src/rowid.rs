//! `ROWID`：48 位 = **6 字节**（存储架构 §7.1）。
//!
//! ```text
//! ROWID = 48 位
//!   file_id  10 位 → 1024 文件（表空间内相对编号）
//!   block_id 28 位 → 268,435,456 块 × 16 KiB = 4 TiB/文件
//!   row_id   10 位 → 行号 1..=1023（0 保留为"无"；与页内槽位上限耦合）
//! ```
//!
//! 作用域：**表空间内相对地址**——不同工作区会出现相同 ROWID。
//! **不含代次**（§6.4）：文件重建时分配新 `file_id`、永不复用，陈旧引用由
//! `file_id` 不匹配检出。**用户不可见**：不得作为对外持久引用（槽位复用，
//! §6.4）；行锁与转发指针用它定位（行迁移经转发指针保持稳定）。

use std::fmt;

/// `ROWID` 编码长度。
pub const ROWID_LEN: usize = 6;

/// `file_id` 有效位宽（10 位）。
pub const FILE_ID_BITS: u32 = 10;
/// `block_id` 有效位宽（28 位）。
pub const BLOCK_ID_BITS: u32 = 28;
/// `row_id` 有效位宽（10 位）。
pub const ROW_ID_BITS: u32 = 10;

/// 行号上限（1..=1023；0 保留为"无"）。
pub const ROW_ID_MAX: u16 = 1023;

/// `ROWID` 字段越界。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowIdRangeError;

impl fmt::Display for RowIdRangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ROWID 字段越界（file_id 10 位 / block_id 28 位 / row_id 1..=1023）")
    }
}

impl std::error::Error for RowIdRangeError {}

/// 记录的表空间内相对地址（48 位）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RowId(u64);

impl RowId {
    /// **页地址形态**：`(file_id, block_id, row_id = 0)`——索引的枝/根条目、
    /// 叶链指针、树头都用它（§9.1.3"子页 ROWID 的 `row_id` 置 0，宽度统一"）。
    ///
    /// 与"行 ROWID"是两个取值域：行号 1..=1023（0 保留为「无」），而页地址
    /// **恒 0**。给页地址一个显式的构造口，免得用 1 冒充当"无行号"的语义。
    pub fn page_address(file_id: u16, block_id: u32) -> Result<Self, RowIdRangeError> {
        if file_id >= (1 << FILE_ID_BITS) || block_id >= (1 << BLOCK_ID_BITS) {
            return Err(RowIdRangeError);
        }
        Ok(Self(
            (u64::from(file_id) << (BLOCK_ID_BITS + ROW_ID_BITS))
                | (u64::from(block_id) << ROW_ID_BITS),
        ))
    }

    /// 由三段字段拼装；各段越界（含 `row_id == 0`）即拒绝。
    pub fn from_parts(file_id: u16, block_id: u32, row_id: u16) -> Result<Self, RowIdRangeError> {
        if file_id >= (1 << FILE_ID_BITS)
            || block_id >= (1 << BLOCK_ID_BITS)
            || row_id == 0
            || row_id > ROW_ID_MAX
        {
            return Err(RowIdRangeError);
        }
        Ok(Self(
            (u64::from(file_id) << (BLOCK_ID_BITS + ROW_ID_BITS))
                | (u64::from(block_id) << ROW_ID_BITS)
                | u64::from(row_id),
        ))
    }

    /// 由原始 48 位值构造（越 48 位域即拒绝；三段字段不做额外校验）。
    #[must_use]
    pub fn from_raw(raw: u64) -> Option<Self> {
        if raw < (1u64 << 48) {
            Some(Self(raw))
        } else {
            None
        }
    }

    /// 原始 48 位值。
    #[must_use]
    pub fn as_raw(self) -> u64 {
        self.0
    }

    /// 文件号（10 位）。
    #[must_use]
    pub fn file_id(self) -> u16 {
        (self.0 >> (BLOCK_ID_BITS + ROW_ID_BITS)) as u16
    }

    /// 块号（28 位）。
    #[must_use]
    pub fn block_id(self) -> u32 {
        ((self.0 >> ROW_ID_BITS) & ((1 << BLOCK_ID_BITS) - 1)) as u32
    }

    /// 行号（10 位，1..=1023）。
    #[must_use]
    pub fn row_id(self) -> u16 {
        (self.0 & ((1 << ROW_ID_BITS) - 1)) as u16
    }

    /// 6 字节小端编码。
    #[must_use]
    pub fn to_bytes(self) -> [u8; ROWID_LEN] {
        let b = self.0.to_le_bytes();
        [b[0], b[1], b[2], b[3], b[4], b[5]]
    }

    /// 由 6 字节小端解码（6 字节必在 48 位域内，不会失败）。
    #[must_use]
    pub fn from_bytes(bytes: &[u8; ROWID_LEN]) -> Self {
        let mut b = [0u8; 8];
        b[..ROWID_LEN].copy_from_slice(bytes);
        Self(u64::from_le_bytes(b))
    }
}

impl fmt::Display for RowId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// 数据块地址（**rdba**，5 字节）：`file_id` 10 位 │ `block_id` 28 位。
///
/// 与 [`RowId`] 的前两段**同口径**（§7.1）——redo 块引用（§11.5）与
/// 段头区映射条目（§5.11）共用这一编码；比 PG 的
/// `RelFileLocator`(12B) + `BlockNumber`(4B) 省 11 字节。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Rdba(u64);

impl Rdba {
    /// 由两段构造；越界拒绝。
    pub fn from_parts(file_id: u16, block_id: u32) -> Option<Self> {
        if file_id > 1023 || block_id > (1 << 28) - 1 {
            return None;
        }
        Some(Self((u64::from(file_id) << 28) | u64::from(block_id)))
    }

    /// 文件号（10 位）。
    #[must_use]
    pub fn file_id(self) -> u16 {
        (self.0 >> 28) as u16
    }

    /// 块号（28 位）。
    #[must_use]
    pub fn block_id(self) -> u32 {
        (self.0 & ((1 << 28) - 1)) as u32
    }

    /// 5 字节小端编码。
    #[must_use]
    pub fn to_bytes(self) -> [u8; 5] {
        let b = self.0.to_le_bytes();
        [b[0], b[1], b[2], b[3], b[4]]
    }

    /// 由 5 字节小端解码。
    #[must_use]
    pub fn from_bytes(bytes: &[u8; 5]) -> Self {
        let mut b = [0u8; 8];
        b[..5].copy_from_slice(bytes);
        Self(u64::from_le_bytes(b))
    }
}

#[cfg(test)]
mod rdba_tests {
    use super::*;

    #[test]
    fn rdba_roundtrip_and_bounds() {
        let r = Rdba::from_parts(1023, (1 << 28) - 1).unwrap();
        assert_eq!(r.file_id(), 1023);
        assert_eq!(r.block_id(), (1 << 28) - 1);
        assert_eq!(Rdba::from_bytes(&r.to_bytes()), r);
        assert!(Rdba::from_parts(1024, 0).is_none());
        assert!(Rdba::from_parts(0, 1 << 28).is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parts_roundtrip_and_bounds() {
        let r = RowId::from_parts(3, 7, 1).unwrap();
        assert_eq!(r.file_id(), 3);
        assert_eq!(r.block_id(), 7);
        assert_eq!(r.row_id(), 1);

        // 边界值。
        let max = RowId::from_parts(1023, (1 << 28) - 1, 1023).unwrap();
        assert_eq!(max.file_id(), 1023);
        assert_eq!(max.block_id(), (1 << 28) - 1);
        assert_eq!(max.row_id(), 1023);
        assert!(max.as_raw() < (1 << 48));

        // 越界。
        assert!(RowId::from_parts(1024, 0, 1).is_err(), "file_id 超 10 位");
        assert!(
            RowId::from_parts(0, 1 << 28, 1).is_err(),
            "block_id 超 28 位"
        );
        assert!(RowId::from_parts(0, 0, 0).is_err(), "行号 0 保留为『无』");
        assert!(RowId::from_parts(0, 0, 1024).is_err(), "行号超 10 位");
        assert!(RowId::from_raw(1 << 48).is_none(), "原始值超 48 位");
    }

    #[test]
    fn six_byte_encoding_roundtrip() {
        for (f, b, r) in [(0u16, 0u32, 1u16), (1, 2, 3), (1023, 0x0FFF_FFFF, 1023)] {
            let id = RowId::from_parts(f, b, r).unwrap();
            let bytes = id.to_bytes();
            assert_eq!(bytes.len(), ROWID_LEN);
            assert_eq!(RowId::from_bytes(&bytes), id);
        }
    }

    #[test]
    fn sequential_rowids_are_ordered_within_a_block() {
        let a = RowId::from_parts(1, 5, 1).unwrap();
        let b = RowId::from_parts(1, 5, 2).unwrap();
        let c = RowId::from_parts(1, 6, 1).unwrap();
        let d = RowId::from_parts(2, 0, 1).unwrap();
        assert!(a < b && b < c && c < d);
    }
}
