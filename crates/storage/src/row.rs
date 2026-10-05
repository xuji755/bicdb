//! 行格式：行头、NULL 位图、变长列偏移数组、片段链与转发指针
//! （存储架构 §6.1–§6.3）。
//!
//! ```text
//! 完整行（普通行；碎片行重组后同形）：
//!   偏移 0  1B  row_flags      位 0 已删除 / 位 1 已迁移 / 位 2 片段 / 3–7 保留
//!   偏移 1  1B  itl_slot       指向本页 ITL 槽索引；0xFF = 无
//!   偏移 2  2B  row_len        含行头（**每条片段记录 = 片段自身长度**）
//!   偏移 4  2B  col_count
//!   偏移 6  2B  null_bitmap_len
//!   偏移 8  2B  var_col_count
//!   偏移 10 nB  null_bitmap    位 i = 第 i 列是 NULL（LSB 在前）
//!         2mB  var_offsets     各变长列起始偏移（相对**变长数据区**起点），单调不减
//!            列数据            定长列在前、变长列在后
//!
//! 头片段：完整行头 + 下一片段 ROWID 6B + 数据第 1 段
//! 中/尾片段：短行头 10B（row_flags │ itl_slot │ row_len │ 下一片段 ROWID）+ 数据段
//! 转发指针：槽位状态 = 2，行体**只有 6 字节**新 ROWID（无行头）
//! ```
//!
//! # 三条纪律（§6.0/§6.1）
//!
//! 1. **不设每列长度前缀**——变长列长度由 `var_offsets` 相邻项之差得出、
//!    末列由行尾界定（随机访问一列不必扫过前文）；
//! 2. **行头不含事务信息**——只有 `itl_slot` 这个**索引**（"是不是我锁的"
//!    与 ITL 里的 `txn_id` 比对，不随行数膨胀）；
//! 3. **规范形式唯一**——位图长度取整字节、偏移数组首项为 0、末列到行尾。
//!
//! # 两个必须知道的边界
//!
//! - **定长区宽度不在行内**：它由列定义（字典）给出；因此访问定长/变长
//!   数据的接口都要传 `fixed_len`。行内自描述的是"变长部分各自的长度"。
//! - **每条片段都不例外地是"页里的一条行"**（§6.3："链上每个片段都是页里
//!   的一'行'，与普通行同规"）：其 `row_len` 是**片段自身长度**；整行长度
//!   **不在任何一处存**——由链求和推导（"能推导的不存"）。头片段多出的
//!   只是：行头之后有 6 字节"下一片段 ROWID"，且列元数据（位图与偏移数组）
//!   描述的是整行——但它描述的对象是**重组后的整行**，不是片段自身。

use crate::rowid::{RowId, ROWID_LEN};

/// 行头固定部分长度。
pub const ROW_HEADER_FIXED_LEN: usize = 10;

/// 片段短行头长度（含下一片段 ROWID）。
pub const FRAGMENT_HEADER_LEN: usize = 10;

/// 转发指针长度（行体只有新 ROWID）。
pub const FORWARDING_LEN: usize = ROWID_LEN;

/// `itl_slot` 的"无"取值。
pub const ITL_SLOT_NONE: u8 = 0xFF;

/// `row_flags` 位定义（§6.1）。
pub mod row_flags {
    /// 位 0：已删除。
    pub const DELETED: u8 = 1 << 0;
    /// 位 1：已迁移（本槽位是转发指针）。
    pub const MIGRATED: u8 = 1 << 1;
    /// 位 2：跨页行片段（链上任一片段都置）。
    pub const FRAGMENT: u8 = 1 << 2;
    /// 位 3：**片段链首**（启用自"保留"区；与 `FRAGMENT` 同置）。
    ///
    /// 头片段 = 完整行头 + 6B 下一片段 + 数据段、中/尾 = 短行头——
    /// 两者仅凭字节**无法可靠区分**（中片段的 next 指针字节可伪装成完整
    /// 行头）。用一个保留位显式标记链首，扫描/检查器无需猜。
    /// 该取值口径已记入待复核清单（保留位的启用在评审后可换别处表达）。
    pub const FRAGMENT_HEAD: u8 = 1 << 3;
}

/// 行格式错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowError {
    /// `row_len` 与实际字节数不符。
    LengthMismatch,
    /// 行头不足 10 字节 / 位图与偏移数组越出行尾。
    Truncated,
    /// NULL 位图长度不是 0 或 `ceil(col_count/8)`（非规范）。
    BadNullBitmapLen,
    /// 变长列偏移数组非规范（首项非 0 / 递减 / 越过数据区）。
    BadVarOffsets,
    /// 片段链指针不允许出现在该形态上（或缺失）。
    BadFragmentChain,
    /// **行超过行长上限**（`row_len` 为 2B、变长偏移亦为 2B——格式上限 65535B）。
    /// 超长值的设计出口是 `ASSET_REF`（§6.6），不是更长的行。
    TooLong {
        /// 实际长度（字节）。
        len: usize,
    },
}

impl std::fmt::Display for RowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            RowError::LengthMismatch => "行长度与 row_len 不符",
            RowError::Truncated => "行头/位图/偏移数组越界",
            RowError::BadNullBitmapLen => "NULL 位图长度非规范",
            RowError::BadVarOffsets => "变长列偏移数组非规范",
            RowError::BadFragmentChain => "片段链指针缺失或非法",
            RowError::TooLong { .. } => "行超过 64 KiB 上限（row_len 为 2B）",
        })
    }
}

impl std::error::Error for RowError {}

/// 完整行的行头（固定部分 + 两个长度字段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowHeader {
    /// `row_flags`。
    pub flags: u8,
    /// `itl_slot`（[`ITL_SLOT_NONE`] = 无）。
    pub itl_slot: u8,
    /// 行长（含行头；碎片行 = 整行长度）。
    pub row_len: u16,
    /// 列数。
    pub col_count: u16,
    /// NULL 位图字节数。
    pub null_bitmap_len: u16,
    /// 变长列数。
    pub var_col_count: u16,
}

impl RowHeader {
    /// 列数为 `col_count` 时的规范 NULL 位图长度（`ceil(col_count/8)`）。
    #[must_use]
    pub fn null_bitmap_len_for(col_count: u16) -> u16 {
        col_count.div_ceil(8)
    }

    /// `var_offsets` 数组起点。
    #[must_use]
    pub fn var_offsets_start(&self) -> usize {
        ROW_HEADER_FIXED_LEN + self.null_bitmap_len as usize
    }

    /// 列数据区起点（定长列在前）。
    #[must_use]
    pub fn data_start(&self) -> usize {
        self.var_offsets_start() + 2 * self.var_col_count as usize
    }

    /// 写入行头（前 10 字节）。
    pub fn write_into(&self, out: &mut [u8]) {
        out[0] = self.flags;
        out[1] = self.itl_slot;
        out[2..4].copy_from_slice(&self.row_len.to_le_bytes());
        out[4..6].copy_from_slice(&self.col_count.to_le_bytes());
        out[6..8].copy_from_slice(&self.null_bitmap_len.to_le_bytes());
        out[8..10].copy_from_slice(&self.var_col_count.to_le_bytes());
    }

    /// 读出前 10 字节。
    pub fn read_from(bytes: &[u8]) -> Result<Self, RowError> {
        if bytes.len() < ROW_HEADER_FIXED_LEN {
            return Err(RowError::Truncated);
        }
        Ok(Self {
            flags: bytes[0],
            itl_slot: bytes[1],
            row_len: u16::from_le_bytes(bytes[2..4].try_into().expect("2 字节")),
            col_count: u16::from_le_bytes(bytes[4..6].try_into().expect("2 字节")),
            null_bitmap_len: u16::from_le_bytes(bytes[6..8].try_into().expect("2 字节")),
            var_col_count: u16::from_le_bytes(bytes[8..10].try_into().expect("2 字节")),
        })
    }
}

/// 完整行的只读视图（**普通行**，或**重组后的碎片行**）。
#[derive(Debug, Clone, Copy)]
pub struct RowView<'a> {
    bytes: &'a [u8],
    header: RowHeader,
}

impl<'a> RowView<'a> {
    /// 解析完整行（结构校验：`row_len == 字节数`、位图规范、头部区在行内）。
    ///
    /// 变长列偏移的**取值**校验需要定长区宽度（列定义给出），
    /// 见 [`RowView::validate_var_offsets`]。
    pub fn new(bytes: &'a [u8]) -> Result<Self, RowError> {
        let header = RowHeader::read_from(bytes)?;
        if usize::from(header.row_len) != bytes.len() {
            return Err(RowError::LengthMismatch);
        }
        if header.null_bitmap_len != 0
            && header.null_bitmap_len != RowHeader::null_bitmap_len_for(header.col_count)
        {
            return Err(RowError::BadNullBitmapLen);
        }
        if header.data_start() > bytes.len() {
            return Err(RowError::Truncated);
        }
        Ok(Self { bytes, header })
    }

    /// 行头。
    #[must_use]
    pub fn header(&self) -> &RowHeader {
        &self.header
    }

    /// 是否已删除。
    #[must_use]
    pub fn is_deleted(&self) -> bool {
        self.header.flags & row_flags::DELETED != 0
    }

    /// 是否片段（重组后仍保留该位）。
    #[must_use]
    pub fn is_fragment(&self) -> bool {
        self.header.flags & row_flags::FRAGMENT != 0
    }

    /// `itl_slot`（[`ITL_SLOT_NONE`] = 无）。
    #[must_use]
    pub fn itl_slot(&self) -> u8 {
        self.header.itl_slot
    }

    /// NULL 位图（可能为空——全部非 NULL）。
    #[must_use]
    pub fn null_bitmap(&self) -> &'a [u8] {
        &self.bytes[ROW_HEADER_FIXED_LEN..self.header.var_offsets_start()]
    }

    /// 第 `col` 列（0 起）是否 NULL。
    #[must_use]
    pub fn is_null(&self, col: u16) -> bool {
        if col >= self.header.col_count {
            return false;
        }
        let idx = col as usize;
        let byte = idx / 8;
        if byte >= self.null_bitmap().len() {
            return false;
        }
        self.null_bitmap()[byte] & (1 << (idx % 8)) != 0
    }

    /// 定长列数据区（宽度由列定义给出——`fixed_len`）。
    #[must_use]
    pub fn fixed_area(&self, fixed_len: usize) -> Option<&'a [u8]> {
        let start = self.header.data_start();
        self.bytes.get(start..start + fixed_len)
    }

    /// 变长列数据区（`fixed_len` = 定长列总宽，由列定义给出）。
    #[must_use]
    pub fn var_area(&self, fixed_len: usize) -> Option<&'a [u8]> {
        let start = self.header.data_start() + fixed_len;
        self.bytes.get(start..)
    }

    /// 第 `i` 个变长列（0 起）的字节。
    #[must_use]
    pub fn var_column(&self, i: usize, fixed_len: usize) -> Option<&'a [u8]> {
        if i >= self.header.var_col_count as usize {
            return None;
        }
        let area = self.var_area(fixed_len)?;
        let start = usize::from(self.var_offset(i)?);
        let end = if i + 1 < self.header.var_col_count as usize {
            usize::from(self.var_offset(i + 1)?)
        } else {
            area.len()
        };
        area.get(start..end)
    }

    /// 变长列偏移的取值校验（需要 `fixed_len`；`new` 不做）。
    pub fn validate_var_offsets(&self, fixed_len: usize) -> Result<(), RowError> {
        let m = self.header.var_col_count as usize;
        if m == 0 {
            return Ok(());
        }
        let area = self.var_area(fixed_len).ok_or(RowError::Truncated)?;
        let mut prev = 0u16;
        for i in 0..m {
            let off = self.var_offset(i).ok_or(RowError::Truncated)?;
            if i == 0 && off != 0 {
                return Err(RowError::BadVarOffsets);
            }
            if off < prev || usize::from(off) > area.len() {
                return Err(RowError::BadVarOffsets);
            }
            prev = off;
        }
        Ok(())
    }

    fn var_offset(&self, i: usize) -> Option<u16> {
        let at = self.header.var_offsets_start() + 2 * i;
        let raw = self.bytes.get(at..at + 2)?;
        Some(u16::from_le_bytes(raw.try_into().expect("2 字节")))
    }
}

/// **头片段**视图：完整行头 + 下一片段 ROWID 6B + 数据第 1 段。
///
/// 与 [`RowView`] 同规：`row_len` 是**本片段记录的长度**（§6.3"与普通行同规"）；
/// 整行长度不存——由链求和推导。列元数据（位图与偏移数组）描述整行。
#[derive(Debug, Clone, Copy)]
pub struct HeadFragment<'a> {
    bytes: &'a [u8],
    header: RowHeader,
}

impl<'a> HeadFragment<'a> {
    /// 解析（要求：置 `FRAGMENT` 位；`row_len` 与本片段字节数一致；
    /// 头部区与 6 字节指针都在字节流内）。
    pub fn new(bytes: &'a [u8]) -> Result<Self, RowError> {
        let header = RowHeader::read_from(bytes)?;
        if header.flags & row_flags::FRAGMENT == 0 {
            return Err(RowError::BadFragmentChain);
        }
        if usize::from(header.row_len) != bytes.len() {
            return Err(RowError::LengthMismatch);
        }
        if header.data_start() + ROWID_LEN > bytes.len() {
            return Err(RowError::Truncated);
        }
        Ok(Self { bytes, header })
    }

    /// 行头（`row_len` = 本片段记录长度）。
    #[must_use]
    pub fn header(&self) -> &RowHeader {
        &self.header
    }

    /// 下一片段 ROWID（全 0 = 无下一个——单片段链的规范写法）。
    #[must_use]
    pub fn next(&self) -> Option<RowId> {
        let at = self.header.data_start();
        let b: [u8; ROWID_LEN] = self.bytes[at..at + ROWID_LEN].try_into().expect("6 字节");
        if b == [0u8; ROWID_LEN] {
            None
        } else {
            Some(RowId::from_bytes(&b))
        }
    }

    /// 数据第 1 段。
    #[must_use]
    pub fn data(&self) -> &'a [u8] {
        &self.bytes[self.header.data_start() + ROWID_LEN..]
    }

    /// 行头区（位图与偏移数组——**整行的列元数据只在这里**）。
    #[must_use]
    pub fn header_bytes(&self) -> &'a [u8] {
        &self.bytes[..self.header.data_start()]
    }
}

/// 中/尾片段的视图（短行头 10B = flags │ itl_slot │ row_len │ 下一片段 ROWID）。
#[derive(Debug, Clone, Copy)]
pub struct FragmentView<'a> {
    bytes: &'a [u8],
}

impl<'a> FragmentView<'a> {
    /// 解析片段（`row_len` 必须等于片段自身字节数；须置 `FRAGMENT` 位）。
    pub fn new(bytes: &'a [u8]) -> Result<Self, RowError> {
        if bytes.len() < FRAGMENT_HEADER_LEN {
            return Err(RowError::Truncated);
        }
        let row_len = u16::from_le_bytes(bytes[2..4].try_into().expect("2 字节"));
        if usize::from(row_len) != bytes.len() {
            return Err(RowError::LengthMismatch);
        }
        if bytes[0] & row_flags::FRAGMENT == 0 {
            return Err(RowError::BadFragmentChain);
        }
        Ok(Self { bytes })
    }

    /// `row_flags`。
    #[must_use]
    pub fn flags(&self) -> u8 {
        self.bytes[0]
    }

    /// `itl_slot`。
    #[must_use]
    pub fn itl_slot(&self) -> u8 {
        self.bytes[1]
    }

    /// 下一片段 ROWID（**链尾**为 `None`——"下一片段 = NULL"编码为全 0）。
    #[must_use]
    pub fn next(&self) -> Option<RowId> {
        let b: [u8; ROWID_LEN] = self.bytes[FRAGMENT_HEADER_LEN - ROWID_LEN..FRAGMENT_HEADER_LEN]
            .try_into()
            .expect("6 字节");
        if b == [0u8; ROWID_LEN] {
            None
        } else {
            Some(RowId::from_bytes(&b))
        }
    }

    /// 本片段的数据字节。
    #[must_use]
    pub fn data(&self) -> &'a [u8] {
        &self.bytes[FRAGMENT_HEADER_LEN..]
    }
}

/// 转发指针：槽位状态 = 2 时，行体**只有 6 字节**新 ROWID。
pub fn forwarding_pointer(bytes: &[u8]) -> Result<RowId, RowError> {
    if bytes.len() != FORWARDING_LEN {
        return Err(RowError::LengthMismatch);
    }
    let b: [u8; ROWID_LEN] = bytes.try_into().expect("6 字节");
    Ok(RowId::from_bytes(&b))
}

/// 组装一条**完整行**（规范形式）。
///
/// `nulls` 按列序给出（`true` = 该列 NULL，其数据不得出现在
/// `fixed_data` / `var_columns` 中）。
///
/// **长度受格式上限约束**（§6.1）：`row_len` 与变长列偏移都是 2B ⇒ 整行
/// 与变长区都必须 ≤ 65535B；超限返回 [`RowError::TooLong`]（**不截断、
/// 不回绕**——超长值的设计出口是 `ASSET_REF`，§6.6）。
pub fn assemble_row(
    flags: u8,
    itl_slot: u8,
    nulls: &[bool],
    fixed_data: &[u8],
    var_columns: &[&[u8]],
) -> Result<Vec<u8>, RowError> {
    let col_count = nulls.len() as u16;
    let var_col_count = var_columns.len() as u16;
    let bitmap_len = RowHeader::null_bitmap_len_for(col_count);
    let var_area_len: usize = var_columns.iter().map(|c| c.len()).sum();
    let row_len = ROW_HEADER_FIXED_LEN
        + bitmap_len as usize
        + 2 * var_columns.len()
        + fixed_data.len()
        + var_area_len;
    if row_len > usize::from(u16::MAX) {
        return Err(RowError::TooLong { len: row_len });
    }

    let header = RowHeader {
        flags,
        itl_slot,
        row_len: row_len as u16,
        col_count,
        null_bitmap_len: bitmap_len,
        var_col_count,
    };
    let mut out = vec![0u8; row_len];
    header.write_into(&mut out);
    for (i, &is_null) in nulls.iter().enumerate() {
        if is_null {
            out[ROW_HEADER_FIXED_LEN + i / 8] |= 1 << (i % 8);
        }
    }
    let mut acc: usize = 0;
    for (i, col) in var_columns.iter().enumerate() {
        let at = header.var_offsets_start() + 2 * i;
        // 变长列偏移也是 2B：任一项超限即整体拒绝（上界检查已保证累计不越）。
        if acc > usize::from(u16::MAX) {
            return Err(RowError::TooLong {
                len: acc + col.len(),
            });
        }
        out[at..at + 2].copy_from_slice(&(acc as u16).to_le_bytes());
        acc += col.len();
    }
    let mut at = header.data_start();
    out[at..at + fixed_data.len()].copy_from_slice(fixed_data);
    at += fixed_data.len();
    for col in var_columns {
        out[at..at + col.len()].copy_from_slice(col);
        at += col.len();
    }
    Ok(out)
}

/// 由头片段与后续片段（中/尾，按链序）重组整行字节流。
///
/// 重组 = 完整行头（含整行列元数据）+ 各数据段顺次拼接；
/// **整行长度 = 各片段之和**（不存、推导），并**回填**到重组结果的
/// `row_len` 字段——于是重组结果是一条规范完整行。
pub fn reassemble_row(head: &[u8], rest: &[&[u8]]) -> Result<Vec<u8>, RowError> {
    let head = HeadFragment::new(head)?;
    let mut out =
        Vec::with_capacity(head.bytes.len() + rest.iter().map(|f| f.len()).sum::<usize>());
    out.extend_from_slice(head.header_bytes());
    out.extend_from_slice(head.data());
    for frag in rest {
        let view = FragmentView::new(frag)?;
        out.extend_from_slice(view.data());
    }
    if out.len() > usize::from(u16::MAX) {
        return Err(RowError::TooLong { len: out.len() });
    }
    // 回填整行长度（头片段的 row_len 是片段自身的），
    // 并清除 `FRAGMENT` 位——该位描述"页内记录是片段"，重组后的逻辑行不是。
    let total = out.len() as u16;
    out[2..4].copy_from_slice(&total.to_le_bytes());
    out[0] &= !(row_flags::FRAGMENT | row_flags::FRAGMENT_HEAD);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXED: usize = 4; // 测试行的定长区宽度（由"列定义"给出）

    #[test]
    fn assemble_and_parse_roundtrip() {
        let nulls = [false, true, false, false];
        let row = assemble_row(
            0,
            2,
            &nulls,
            &[0xAA, 0xBB, 0xCC, 0xDD],
            &[b"hello", b"world!"],
        )
        .unwrap();
        let view = RowView::new(&row).expect("可解析");
        assert_eq!(view.header().col_count, 4);
        assert_eq!(view.header().var_col_count, 2);
        assert_eq!(view.header().null_bitmap_len, 1);
        assert_eq!(view.header().row_len as usize, row.len());
        assert_eq!(view.itl_slot(), 2);
        assert!(!view.is_deleted() && !view.is_fragment());
        assert!(!view.is_null(0) && view.is_null(1) && !view.is_null(3));
        view.validate_var_offsets(FIXED).unwrap();
        assert_eq!(view.fixed_area(FIXED), Some(&[0xAA, 0xBB, 0xCC, 0xDD][..]));
        assert_eq!(view.var_area(FIXED), Some(&b"helloworld!"[..]));
        assert_eq!(view.var_column(0, FIXED), Some(&b"hello"[..]));
        assert_eq!(view.var_column(1, FIXED), Some(&b"world!"[..]));
        assert_eq!(view.var_column(2, FIXED), None);
    }

    #[test]
    fn empty_var_column_is_legal() {
        // 零长度变长列：相邻偏移相等（合法的规范形式）。
        let row = assemble_row(0, ITL_SLOT_NONE, &[false, false], &[], &[b"", b"x"]).unwrap();
        let view = RowView::new(&row).unwrap();
        view.validate_var_offsets(0).unwrap();
        assert_eq!(view.var_column(0, 0), Some(&b""[..]));
        assert_eq!(view.var_column(1, 0), Some(&b"x"[..]));
    }

    #[test]
    fn no_null_bitmap_when_no_columns() {
        let row = assemble_row(0, ITL_SLOT_NONE, &[], &[], &[]).unwrap();
        let view = RowView::new(&row).unwrap();
        assert_eq!(view.header().null_bitmap_len, 0);
        assert!(!view.is_null(0));
        assert_eq!(row.len(), ROW_HEADER_FIXED_LEN);
    }

    #[test]
    fn more_than_eight_columns_use_multi_byte_bitmap() {
        let mut nulls = [false; 11];
        nulls[8] = true;
        let row = assemble_row(0, ITL_SLOT_NONE, &nulls, &[], &[b"x"]).unwrap();
        let view = RowView::new(&row).unwrap();
        assert_eq!(view.header().null_bitmap_len, 2);
        assert!(view.is_null(8) && !view.is_null(7) && !view.is_null(9));
    }

    #[test]
    fn strict_validation_rejects_non_canonical_rows() {
        let row = assemble_row(0, ITL_SLOT_NONE, &[false, false], &[1, 2], &[b"ab"]).unwrap();
        // 长度不符。
        assert_eq!(
            RowView::new(&row[..row.len() - 1]).err(),
            Some(RowError::LengthMismatch)
        );
        // 位图长度非规范。
        let mut bad = row.clone();
        bad[6..8].copy_from_slice(&5u16.to_le_bytes());
        assert!(RowView::new(&bad).is_err());
        // 偏移数组首项非 0。
        let mut bad = row.clone();
        let at = ROW_HEADER_FIXED_LEN + 1;
        bad[at..at + 2].copy_from_slice(&3u16.to_le_bytes());
        assert_eq!(
            RowView::new(&bad)
                .unwrap()
                .validate_var_offsets(FIXED)
                .err(),
            Some(RowError::BadVarOffsets)
        );
        // 偏移越过数据区。
        let row2 = assemble_row(0, 0, &[false], &[], &[b"a", b"bc"]).unwrap();
        let mut bad2 = row2.clone();
        let o1 = ROW_HEADER_FIXED_LEN + 1 + 2;
        bad2[o1..o1 + 2].copy_from_slice(&999u16.to_le_bytes());
        assert_eq!(
            RowView::new(&bad2).unwrap().validate_var_offsets(0).err(),
            Some(RowError::BadVarOffsets)
        );
        // 截断。
        assert_eq!(RowView::new(&row[..5]).err(), Some(RowError::Truncated));
    }

    #[test]
    fn fragment_chain_reassembly() {
        // 整行 = 行头 + 定长 4B + 变长 "0123456789"；拆成头片段（前 4 字节数据）
        // + 中片段（后 6 字节数据，链尾）。每条片段记录 = 自身长度（§6.3）。
        // 原始（未拆分的）行：flags = 0；FRAGMENT 位由拆分方在头片段上置。
        let full =
            assemble_row(0, 3, &[false], &[0xDE, 0xAD, 0xBE, 0xEF], &[b"0123456789"]).unwrap();
        let view = RowView::new(&full).unwrap();
        let data_start = view.header().data_start();
        let (chunk1, chunk2) = full[data_start..].split_at(4);

        let next = RowId::from_parts(1, 4, 2).unwrap();
        let mut head = Vec::new();
        head.extend_from_slice(&full[..data_start]); // 完整行头（整行列元数据）
        head[0] |= row_flags::FRAGMENT; // 拆分方置位
        head.extend_from_slice(&next.to_bytes());
        head.extend_from_slice(chunk1);
        // 头片段的 row_len = 本片段长度。
        let head_len = head.len() as u16;
        head[2..4].copy_from_slice(&head_len.to_le_bytes());

        let mut mid = Vec::new();
        mid.push(row_flags::FRAGMENT);
        mid.push(3u8);
        mid.extend_from_slice(&0u16.to_le_bytes()); // 占位，稍后修正
        mid.extend_from_slice(&[0u8; ROWID_LEN]); // 链尾
        mid.extend_from_slice(chunk2);
        let mid_len = mid.len() as u16;
        mid[2..4].copy_from_slice(&mid_len.to_le_bytes());

        // 头片段：row_len = 自身长度；`RowView` 同样可解析（与普通行同规）。
        let hf = HeadFragment::new(&head).expect("头片段可解析");
        assert_eq!(usize::from(hf.header().row_len), head.len());
        assert_eq!(
            RowView::new(&head).unwrap().header().row_len as usize,
            head.len()
        );
        assert_eq!(hf.next(), Some(next));
        assert_eq!(hf.data(), chunk1);
        // 头片段的行头：除 `row_len`（片段自身长度 ≠ 整行）外与整行头逐字节一致
        // ——位图与偏移数组描述的是**整行**（列元数据只在头片段）。
        let hb = hf.header_bytes();
        assert_eq!(hb.len(), data_start);
        assert_eq!(&hb[4..], &full[4..data_start], "列/位图/偏移元数据一致");
        assert_eq!(hb[0], full[0] | row_flags::FRAGMENT, "仅置片段位");
        assert_eq!(hb[1], full[1]);

        // 中片段。
        let fv = FragmentView::new(&mid).expect("片段可解析");
        assert_eq!(fv.next(), None, "链尾");
        assert_eq!(fv.data(), chunk2);
        assert_eq!(fv.itl_slot(), 3);

        // 重组 = 原整行字节流（整行 row_len 由链求和回填）。
        let rebuilt = reassemble_row(&head, &[&mid]).expect("可重组");
        assert_eq!(rebuilt, full);
        let rv = RowView::new(&rebuilt).unwrap();
        rv.validate_var_offsets(FIXED).unwrap();
        assert_eq!(rv.header().row_len as usize, full.len());
        assert_eq!(rv.var_column(0, FIXED), Some(&b"0123456789"[..]));
        assert_eq!(rv.fixed_area(FIXED), Some(&[0xDE, 0xAD, 0xBE, 0xEF][..]));

        // 转发指针：只有 6 字节。
        let dst = RowId::from_parts(2, 9, 3).unwrap();
        assert_eq!(forwarding_pointer(&dst.to_bytes()), Ok(dst));
        assert_eq!(
            forwarding_pointer(&dst.to_bytes()[..5]).err(),
            Some(RowError::LengthMismatch)
        );
    }

    #[test]
    fn head_fragment_requires_flag_and_fits() {
        let plain = assemble_row(0, ITL_SLOT_NONE, &[], &[], &[]).unwrap();
        assert_eq!(
            HeadFragment::new(&plain).err(),
            Some(RowError::BadFragmentChain),
            "非片段位不得当片段解析"
        );
        let too_short = [row_flags::FRAGMENT, 0, 10, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(
            HeadFragment::new(&too_short).err(),
            Some(RowError::Truncated)
        );
    }

    #[test]
    fn assemble_row_rejects_rows_over_64k() {
        // 审核修复回归（F3）：`row_len` 与变长偏移都是 2B ⇒ 超限**明确报错**
        // （旧行为：row_len 静默截断、偏移累加 debug panic / release 回绕）。
        let big = vec![0u8; 70000];
        assert!(matches!(
            assemble_row(0, 1, &[false], &[], &[big.as_slice()]),
            Err(RowError::TooLong { .. })
        ));
        // 两列各自不大、合计越界：同样拒绝（不是只查单列）。
        let a = vec![0u8; 40000];
        let b = vec![0u8; 40000];
        assert!(matches!(
            assemble_row(0, 1, &[], &[], &[a.as_slice(), b.as_slice()]),
            Err(RowError::TooLong { .. })
        ));
    }
}
