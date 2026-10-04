//! 段头页（类型 7）与区映射（存储架构 §4.2 / §5.11）。
//!
//! **段 = 段头 + 区映射 + 段内位图 + 数据页**；只有段头是锚点（段的第一页）。
//! 本模块是段管理的**字节格式与映射运算**：段头公共部分的读写、区映射的
//! 解析/追加/**相邻合并**/**逻辑页号 → 物理块**的换算、段内位图页覆盖范围的
//! 推算。段的**创建与扩展**（经 LMT 分配区）在后续切片接入。
//!
//! # 段头页的页体（§5.11）
//!
//! ```text
//! 偏移 68    页体头（8B）   seg_type 1B │ map_format 1B │ flags 1B │ 保留 1B │ dataobj# 4B
//! 偏移 76    段标识（16B）  obj# 4B │ pages_per_extent 1B │ itl_max 1B │ pctfree 1B │
//!                          保留 1B │ table_opts 2B │ 保留 6B
//! 偏移 92    空间状态（24B）HWM 4B │ 追加位置 4B │ 起始位图页 4B │ 插入提示 4B │
//!                          已分配区数 2B │ 位图页数 2B │ 保留 4B
//! 偏移 116   区映射头（12B）区间数 2B │ next_map_page 6B（0 = 无溢出）│ 保留 4B
//! 偏移 128   类型扩展区（长度由 seg_type 决定；多数段为 0）
//! 偏移 128+扩展  区间项 6B×n（起始 RDBA 4B │ 区数 2B，相邻者合并）
//! ```
//!
//! **逻辑页号是所有段内位置的口径**（HWM / 追加位置 / 起始位图页 / 插入提示
//! 均为逻辑页号）；物理位置只在区映射里出现一处——段的访问与文件的物理
//! 布局因此解耦。

use bicdb_common::checksum::PAGE_SIZE;

use crate::bitmap::EXTENT_BLOCKS;
use crate::page::{Page, PageType};
use crate::rowid::Rdba;

/// 段头页页体起点（固定头之后）。
pub const SEG_BODY_OFFSET: usize = 68;
/// 段头公共部分长度（页体头 8 + 段标识 16 + 空间状态 24 + 区映射头 12）。
pub const SEG_COMMON_LEN: usize = 60;
/// 类型扩展区起点（固定偏移）。
pub const SEG_EXTENSION_OFFSET: usize = SEG_BODY_OFFSET + SEG_COMMON_LEN;
/// 区间项长度：起始 RDBA 5B │ 区数 2B（RDBA 与 redo 块引用同一编码——10 位
/// 文件号 + **28 位块号**；Oracle 的 4B DBA 是它自己的 22 位块号宽度，
/// 照抄会截断高 6 位，§5.11 已修正）。
pub const EXTENT_ENTRY_LEN: usize = 7;
/// 区间项数上限（无扩展区的段；16252 ÷ 7 = 2321 项）。
pub const MAX_EXTENT_ENTRIES: usize = (PAGE_SIZE - 4 - SEG_EXTENSION_OFFSET) / EXTENT_ENTRY_LEN;
/// 一个**段内位图页**覆盖的逻辑页数（2 位/页 → 130432/2，§4.5）。
pub const BITMAP_PAGE_COVERAGE: u32 = 65216;
/// 区映射续页指针宽度（6B 逻辑页号；0 = 无溢出）。
pub const MAP_POINTER_LEN: usize = 6;

/// 段的物理种类（`seg_type`；§5.11 的枚举——与 `obj$.type#` 是两条轴）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SegType {
    /// 1：堆表段（数据页类型 1）。
    Heap = 1,
    /// 2：B+Tree 索引段（数据页类型 2/3/4；扩展区 = 树头 6B）。
    BTree = 2,
    /// 3：邻接段（图的边；数据页类型 5）。
    Adjacency = 3,
    /// 4：ANN 索引段（数据页类型 6）。
    Ann = 4,
    /// 5：Undo 段（数据页类型 9；扩展区 = 事务表）。
    Undo = 5,
    /// 6：临时段（数据页类型 10，体同堆表页）。
    Temporary = 6,
}

impl SegType {
    /// 由字节解码。
    #[must_use]
    pub const fn from_u8(raw: u8) -> Option<Self> {
        match raw {
            1 => Some(Self::Heap),
            2 => Some(Self::BTree),
            3 => Some(Self::Adjacency),
            4 => Some(Self::Ann),
            5 => Some(Self::Undo),
            6 => Some(Self::Temporary),
            _ => None,
        }
    }

    /// 字节值。
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// 类型扩展区长度。
    ///
    /// Undo 段 = 事务表（256 槽 × 24B，§4.6.3）；B+Tree = 树头 6B；
    /// ANN 的扩展区布局随 P8 定案（当前明确拒绝，避免猜 size）。
    #[must_use]
    pub const fn extension_len(self) -> Option<usize> {
        match self {
            SegType::Heap | SegType::Adjacency | SegType::Temporary => Some(0),
            SegType::BTree => Some(6),
            SegType::Undo => Some(256 * 24),
            SegType::Ann => None, // 布局待 P8 定案
        }
    }
}

/// 段错误（**明确判定**）。
#[derive(Debug, PartialEq, Eq)]
pub enum SegmentError {
    /// 不是段头页（页类型不符）。
    NotSegmentHeader,
    /// `seg_type` 未知。
    UnknownSegType(u8),
    /// 该段类型的扩展区布局尚未定案（ANN）。
    ExtensionNotFrozen(u8),
    /// 页类型扩展区越界（扩展区 + 区间项放不下）。
    ExtensionTooLarge,
    /// 区间项数超过容量（需要溢出到区映射续页——随后切片）。
    MapFull,
    /// 已分配区数越过 2B 字段（段 > 65535 区 = 8 GiB——需段重组）。
    ExtentCountOverflow,
    /// 区间项非法（区数 = 0）。
    EmptyExtent,
    /// 页格式损坏（字段越界）。
    Malformed,
}

impl std::fmt::Display for SegmentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SegmentError::NotSegmentHeader => f.write_str("不是段头页（页类型不符）"),
            SegmentError::UnknownSegType(t) => write!(f, "未知的 seg_type {t}"),
            SegmentError::ExtensionNotFrozen(t) => {
                write!(f, "seg_type {t} 的类型扩展区布局尚未定案")
            }
            SegmentError::ExtensionTooLarge => f.write_str("类型扩展区越出页体"),
            SegmentError::MapFull => f.write_str("区映射已满（需要溢出到续页）"),
            SegmentError::ExtentCountOverflow => {
                f.write_str("已分配区数越过 2B 字段（段 > 65535 区——需段重组）")
            }
            SegmentError::EmptyExtent => f.write_str("区间项区数为 0"),
            SegmentError::Malformed => f.write_str("段头页字段越界——按损坏处理"),
        }
    }
}

impl std::error::Error for SegmentError {}

/// 段头（公共部分的值形态；§5.11）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentHeader {
    /// 段的物理种类。
    pub seg_type: SegType,
    /// 区映射格式版本（独立于页格式）。
    pub map_format: u8,
    /// 标志位。
    pub flags: u8,
    /// 数据对象号（段头自校验的冗余一份）。
    pub dataobj: u32,
    /// 对象号（反向指针；诊断与恢复用）。
    pub obj: u32,
    /// 每区页数（当前恒 8）。
    pub pages_per_extent: u8,
    /// 段级 ITL 上限（对应 `MAXTRANS`）。
    pub itl_max: u8,
    /// 页内预留比例。
    pub pctfree: u8,
    /// 表选项编码（§8.1——打开段即可分派行为）。
    pub table_opts: u16,
    /// 高水位（**逻辑页号**；`append_only` 段不用）。
    pub hwm: u32,
    /// 追加位置（`append_only` 的下一可写逻辑页号）。
    pub append_pos: u32,
    /// 第一个段内位图页的逻辑页号；**0 = 尚无**。
    pub first_bitmap_page: u32,
    /// 插入提示：最后使用的段内位图页。
    pub insert_hint: u32,
    /// 已分配区数。
    pub extent_count: u16,
    /// 位图页数。
    pub bitmap_pages: u16,
    /// 第一个区映射续页（**逻辑页号**；0 = 无溢出）。
    pub next_map_page: u64,
}

/// 区映射条目：起始 RDBA 4B │ 区数 2B（**相邻者合并**，§5.11）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtentEntry {
    /// 区间起始块地址。
    pub start: Rdba,
    /// 区间长度（以区为单位；≥ 1）。
    pub extents: u16,
}

impl ExtentEntry {
    /// 构造（区数 ≥ 1）。
    #[must_use]
    pub const fn new(start: Rdba, extents: u16) -> Self {
        Self { start, extents }
    }

    /// 区间的**逻辑页号起点**（调用方自维护累计；此处不存）。
    #[must_use]
    pub const fn blocks(self) -> u32 {
        self.extents as u32 * 8
    }
}

fn get_u16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(b[off..off + 2].try_into().expect("2 字节"))
}

fn get_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().expect("4 字节"))
}

fn get_u48(b: &[u8], off: usize) -> u64 {
    let mut raw = [0u8; 8];
    raw[..6].copy_from_slice(&b[off..off + 6]);
    u64::from_le_bytes(raw)
}

fn put_u16(b: &mut [u8], off: usize, v: u16) {
    b[off..off + 2].copy_from_slice(&v.to_le_bytes());
}

fn put_u32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

fn put_u48(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 6].copy_from_slice(&v.to_le_bytes()[..6]);
}

/// 读段头（公共部分）。
pub fn read_header(page: &Page) -> Result<SegmentHeader, SegmentError> {
    if page.header().map(|h| h.page_type) != Some(PageType::SegmentHeader) {
        return Err(SegmentError::NotSegmentHeader);
    }
    let b = page.as_bytes();
    let raw_type = b[SEG_BODY_OFFSET];
    let seg_type = SegType::from_u8(raw_type).ok_or(SegmentError::UnknownSegType(raw_type))?;
    Ok(SegmentHeader {
        seg_type,
        map_format: b[SEG_BODY_OFFSET + 1],
        flags: b[SEG_BODY_OFFSET + 2],
        dataobj: get_u32(b, SEG_BODY_OFFSET + 4),
        obj: get_u32(b, SEG_BODY_OFFSET + 8),
        pages_per_extent: b[SEG_BODY_OFFSET + 12],
        itl_max: b[SEG_BODY_OFFSET + 13],
        pctfree: b[SEG_BODY_OFFSET + 14],
        table_opts: get_u16(b, SEG_BODY_OFFSET + 16),
        hwm: get_u32(b, SEG_BODY_OFFSET + 24),
        append_pos: get_u32(b, SEG_BODY_OFFSET + 28),
        first_bitmap_page: get_u32(b, SEG_BODY_OFFSET + 32),
        insert_hint: get_u32(b, SEG_BODY_OFFSET + 36),
        extent_count: get_u16(b, SEG_BODY_OFFSET + 40),
        bitmap_pages: get_u16(b, SEG_BODY_OFFSET + 42),
        next_map_page: get_u48(b, SEG_BODY_OFFSET + 50),
    })
}

/// 写段头（公共部分；**只动 68..128 的字段字节**，扩展区与区间项不动）。
pub fn write_header(page: &mut Page, h: &SegmentHeader) -> Result<(), SegmentError> {
    if page.header().map(|x| x.page_type) != Some(PageType::SegmentHeader) {
        return Err(SegmentError::NotSegmentHeader);
    }
    let b = page.as_bytes_mut();
    b[SEG_BODY_OFFSET] = h.seg_type.as_u8();
    b[SEG_BODY_OFFSET + 1] = h.map_format;
    b[SEG_BODY_OFFSET + 2] = h.flags;
    b[SEG_BODY_OFFSET + 3] = 0;
    put_u32(b, SEG_BODY_OFFSET + 4, h.dataobj);
    put_u32(b, SEG_BODY_OFFSET + 8, h.obj);
    b[SEG_BODY_OFFSET + 12] = h.pages_per_extent;
    b[SEG_BODY_OFFSET + 13] = h.itl_max;
    b[SEG_BODY_OFFSET + 14] = h.pctfree;
    b[SEG_BODY_OFFSET + 15] = 0;
    put_u16(b, SEG_BODY_OFFSET + 16, h.table_opts);
    // 18..24 保留
    put_u32(b, SEG_BODY_OFFSET + 24, h.hwm);
    put_u32(b, SEG_BODY_OFFSET + 28, h.append_pos);
    put_u32(b, SEG_BODY_OFFSET + 32, h.first_bitmap_page);
    put_u32(b, SEG_BODY_OFFSET + 36, h.insert_hint);
    put_u16(b, SEG_BODY_OFFSET + 40, h.extent_count);
    put_u16(b, SEG_BODY_OFFSET + 42, h.bitmap_pages);
    // 44..50 保留
    put_u48(b, SEG_BODY_OFFSET + 50, h.next_map_page);
    // 56..60 保留
    Ok(())
}

/// 区间项区在页内的字节偏移（`128 + 类型扩展区长度`）。
pub fn entries_offset(seg_type: SegType) -> Result<usize, SegmentError> {
    let ext = seg_type
        .extension_len()
        .ok_or(SegmentError::ExtensionNotFrozen(seg_type.as_u8()))?;
    let off = SEG_EXTENSION_OFFSET + ext;
    if off > PAGE_SIZE - 4 {
        return Err(SegmentError::ExtensionTooLarge);
    }
    Ok(off)
}

/// 区间项容量（该项区可放的条目数）。
pub fn entries_capacity(seg_type: SegType) -> Result<usize, SegmentError> {
    Ok((PAGE_SIZE - 4 - entries_offset(seg_type)?) / EXTENT_ENTRY_LEN)
}

/// 读区映射（就地存放的区间项；数量由段头的"区间数"给出）。
pub fn read_extents(page: &Page) -> Result<Vec<ExtentEntry>, SegmentError> {
    let header = read_header(page)?;
    let off = entries_offset(header.seg_type)?;
    let count = extent_entry_count(page)?;
    let cap = (PAGE_SIZE - 4 - off) / EXTENT_ENTRY_LEN;
    if count > cap {
        return Err(SegmentError::Malformed);
    }
    let b = page.as_bytes();
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let at = off + i * EXTENT_ENTRY_LEN;
        let start = Rdba::from_bytes(b[at..at + 5].try_into().expect("5 字节"));
        let extents = u16::from_le_bytes([b[at + 5], b[at + 6]]);
        out.push(ExtentEntry { start, extents });
    }
    Ok(out)
}

/// "区间数"（区映射头首字段）。
fn extent_entry_count(page: &Page) -> Result<usize, SegmentError> {
    Ok(usize::from(get_u16(page.as_bytes(), SEG_BODY_OFFSET + 48)))
}

fn set_extent_entry_count(page: &mut Page, count: u16) {
    put_u16(page.as_bytes_mut(), SEG_BODY_OFFSET + 48, count);
}

/// **追加一个区间并合并相邻项**（§5.11：条目数随**碎片度**增长）。
///
/// 返回（合并后仍存的区间项列表）。相邻判定 = 「前项的末块 == 新项的首块」
/// 或「新项的末块 == 后项的首块」；新项恰好填补两邻之间的空隙时合并三者为一条。
///
/// 同时维护段头的"区间数"与"已分配区数"。
pub fn append_extent(
    page: &mut Page,
    entry: ExtentEntry,
) -> Result<Vec<ExtentEntry>, SegmentError> {
    if entry.extents == 0 {
        return Err(SegmentError::EmptyExtent);
    }
    let header = read_header(page)?;
    if header.seg_type.extension_len().is_none() {
        return Err(SegmentError::ExtensionNotFrozen(header.seg_type.as_u8()));
    }
    let mut entries = read_extents(page)?;
    let merged = merge_extent(&mut entries, entry);
    let off = entries_offset(header.seg_type)?;
    let cap = (PAGE_SIZE - 4 - off) / EXTENT_ENTRY_LEN;
    if entries.len() > cap {
        return Err(SegmentError::MapFull);
    }

    // 写回条目区（就地，高地址方向；多余旧条目清零）。
    let b = page.as_bytes_mut();
    for (i, e) in entries.iter().enumerate() {
        let at = off + i * EXTENT_ENTRY_LEN;
        b[at..at + 5].copy_from_slice(&e.start.to_bytes());
        put_u16(b, at + 5, e.extents);
    }
    for i in entries.len()..cap {
        let at = off + i * EXTENT_ENTRY_LEN;
        b[at..at + EXTENT_ENTRY_LEN].fill(0);
    }
    let mut header = header;
    header.extent_count = header
        .extent_count
        .checked_add(entry.extents)
        .ok_or(SegmentError::ExtentCountOverflow)?;
    write_header(page, &header)?;
    set_extent_entry_count(page, entries.len() as u16);
    let _ = merged;
    Ok(entries)
}

/// 合并逻辑（纯函数；测试直接钉住）。
fn merge_extent(entries: &mut Vec<ExtentEntry>, entry: ExtentEntry) -> bool {
    // 找插入位置（按逻辑序 = 起始块（file_id, block_id）升序）。
    let key = |e: &ExtentEntry| (e.start.file_id(), e.start.block_id());
    let pos = entries.partition_point(|e| key(e) < key(&entry));
    let mut merged = false;
    let mut new_entry = entry;
    // 与左邻合并：左邻末块 == 新项首块（区数之和必须仍在 2B 内）。
    if pos > 0 {
        let left = entries[pos - 1];
        if left.start.block_id() + left.blocks() == new_entry.start.block_id()
            && left.start.file_id() == new_entry.start.file_id()
        {
            if let Some(sum) = left.extents.checked_add(new_entry.extents) {
                new_entry = ExtentEntry::new(left.start, sum);
                entries.remove(pos - 1);
                merged = true;
            }
        }
    }
    // 与右邻合并：新项末块 == 右邻首块。
    let at = entries.partition_point(|e| key(e) < key(&new_entry));
    if at < entries.len() {
        let right = entries[at];
        if new_entry.start.block_id() + new_entry.blocks() == right.start.block_id()
            && new_entry.start.file_id() == right.start.file_id()
        {
            if let Some(sum) = new_entry.extents.checked_add(right.extents) {
                new_entry = ExtentEntry::new(new_entry.start, sum);
                entries.remove(at);
                merged = true;
            }
        }
    }
    entries.insert(
        entries.partition_point(|e| key(e) < key(&new_entry)),
        new_entry,
    );
    merged
}

/// **逻辑页号 → 物理块地址**（经区映射；`None` = 该逻辑页尚未分配）。
#[must_use]
pub fn logical_to_rdba(extents: &[ExtentEntry], logical: u32) -> Option<Rdba> {
    let extent_index = logical / EXTENT_BLOCKS;
    let in_extent = logical % EXTENT_BLOCKS;
    let mut base = 0u32;
    for e in extents {
        let len = u32::from(e.extents);
        if extent_index < base + len {
            let offset_extents = extent_index - base;
            let block = e.start.block_id() + offset_extents * EXTENT_BLOCKS + in_extent;
            return Rdba::from_parts(e.start.file_id(), block);
        }
        base += len;
    }
    None
}

/// 第 `i` 个段内位图页覆盖的逻辑页范围 `[i×65216, (i+1)×65216)`。
#[must_use]
pub const fn bitmap_page_range(index: u32) -> (u32, u32) {
    let start = index * BITMAP_PAGE_COVERAGE;
    (start, start + BITMAP_PAGE_COVERAGE)
}

/// 覆盖 `pages` 个逻辑页所需的段内位图页数。
#[must_use]
pub const fn bitmap_pages_for(pages: u32) -> u32 {
    pages.div_ceil(BITMAP_PAGE_COVERAGE)
}

#[cfg(test)]
mod tests {
    use bicdb_common::seq::Lsn;

    use super::*;
    use crate::page::flags;

    fn seg_page(seg_type: SegType, obj: u32) -> Page {
        let mut page = Page::new(PageType::SegmentHeader, [0u8; 8], 3, 0);
        let header = SegmentHeader {
            seg_type,
            map_format: 1,
            flags: 0,
            dataobj: obj + 1,
            obj,
            pages_per_extent: EXTENT_BLOCKS as u8,
            itl_max: 4,
            pctfree: 10,
            table_opts: 0x0102,
            hwm: 0,
            append_pos: 0,
            first_bitmap_page: 0,
            insert_hint: 0,
            extent_count: 0,
            bitmap_pages: 0,
            next_map_page: 0,
        };
        write_header(&mut page, &header).unwrap();
        page
    }

    #[test]
    fn header_roundtrip_and_offsets_are_pinned() {
        let page = seg_page(SegType::Heap, 42);
        let h = read_header(&page).unwrap();
        assert_eq!(h.seg_type, SegType::Heap);
        assert_eq!(h.obj, 42);
        assert_eq!(h.dataobj, 43);
        assert_eq!(h.itl_max, 4);
        assert_eq!(h.pctfree, 10);
        assert_eq!(h.table_opts, 0x0102);
        assert_eq!(h.pages_per_extent, 8);

        // 字节钉住：分段起点与字段偏移。
        let b = page.as_bytes();
        assert_eq!(SEG_BODY_OFFSET, 68);
        assert_eq!(SEG_EXTENSION_OFFSET, 128);
        assert_eq!(b[68], 1, "seg_type = 堆表段");
        assert_eq!(get_u32(b, 76), 42, "obj# 在段标识首位");
        assert_eq!(get_u16(b, 84), 0x0102, "table_opts");
        assert_eq!(get_u32(b, 92), 0, "HWM");
        assert_eq!(get_u16(b, 116), 0, "区间数");
        assert_eq!(get_u48(b, 118), 0, "next_map_page = 无溢出");
        assert_eq!(MAX_EXTENT_ENTRIES, 2321, "容量 = (16380 − 128)/7");
    }

    #[test]
    fn entries_offset_follows_seg_type() {
        assert_eq!(entries_offset(SegType::Heap).unwrap(), 128);
        assert_eq!(entries_offset(SegType::Temporary).unwrap(), 128);
        assert_eq!(entries_offset(SegType::BTree).unwrap(), 134, "树头 6B");
        assert_eq!(entries_offset(SegType::Undo).unwrap(), 128 + 6144, "事务表");
        assert_eq!(
            entries_offset(SegType::Ann),
            Err(SegmentError::ExtensionNotFrozen(4))
        );
        assert_eq!(entries_capacity(SegType::Heap).unwrap(), 2321);
    }

    #[test]
    fn extent_append_merges_adjacents() {
        let mut page = seg_page(SegType::Heap, 7);
        let rdba = |block: u32| Rdba::from_parts(3, block).unwrap();
        // 两个相邻的 1 区区间 ⇒ 合并成 2 区。
        append_extent(&mut page, ExtentEntry::new(rdba(100), 1)).unwrap();
        let entries = append_extent(&mut page, ExtentEntry::new(rdba(108), 1)).unwrap();
        assert_eq!(entries, vec![ExtentEntry::new(rdba(100), 2)]);
        // 非相邻 ⇒ 两条。
        let entries = append_extent(&mut page, ExtentEntry::new(rdba(200), 2)).unwrap();
        assert_eq!(entries.len(), 2);
        // 左邻相邻、右邻不相邻 ⇒ 只并左（[100,2) + [116,8) → [100,10)）。
        let entries = append_extent(&mut page, ExtentEntry::new(rdba(116), 8)).unwrap();
        assert_eq!(
            entries,
            vec![
                ExtentEntry::new(rdba(100), 10),
                ExtentEntry::new(rdba(200), 2)
            ]
        );
        // 段头维护：区间数 2、已分配区数 1+1+2+8 = 12。
        let h = read_header(&page).unwrap();
        assert_eq!(h.extent_count, 12);
        assert_eq!(extent_entry_count(&page).unwrap(), 2);
        assert_eq!(read_extents(&page).unwrap(), entries);

        // 桥接：新项恰好填满两邻之间的空隙 ⇒ 三者并为一条。
        let mut bridge = seg_page(SegType::Heap, 8);
        append_extent(&mut bridge, ExtentEntry::new(rdba(100), 2)).unwrap(); // 100..116
        append_extent(&mut bridge, ExtentEntry::new(rdba(180), 1)).unwrap(); // 180..188
        let entries = append_extent(&mut bridge, ExtentEntry::new(rdba(116), 8)).unwrap(); // 116..180
        assert_eq!(entries, vec![ExtentEntry::new(rdba(100), 11)]); // 100..188
        assert_eq!(read_header(&bridge).unwrap().extent_count, 11);
    }

    #[test]
    fn logical_to_rdba_maps_through_extents() {
        let rdba = |block: u32| Rdba::from_parts(3, block).unwrap();
        let entries = vec![
            ExtentEntry::new(rdba(100), 2), // 逻辑页 0..15 → 块 100..115
            ExtentEntry::new(rdba(200), 1), // 逻辑页 16..23 → 块 200..207
        ];
        assert_eq!(logical_to_rdba(&entries, 0).unwrap().block_id(), 100);
        assert_eq!(logical_to_rdba(&entries, 7).unwrap().block_id(), 107);
        assert_eq!(logical_to_rdba(&entries, 8).unwrap().block_id(), 108);
        assert_eq!(logical_to_rdba(&entries, 15).unwrap().block_id(), 115);
        assert_eq!(logical_to_rdba(&entries, 16).unwrap().block_id(), 200);
        assert_eq!(logical_to_rdba(&entries, 23).unwrap().block_id(), 207);
        assert_eq!(logical_to_rdba(&entries, 24), None);
    }

    #[test]
    fn map_full_is_reported() {
        let mut page = seg_page(SegType::Heap, 7);
        // 用非相邻区间填满容量。
        for i in 0..MAX_EXTENT_ENTRIES as u32 {
            let block = 100 + i * 16; // 每项 1 区、间隔 1 区 ⇒ 不相邻
            append_extent(
                &mut page,
                ExtentEntry::new(Rdba::from_parts(3, block).unwrap(), 1),
            )
            .unwrap();
        }
        assert_eq!(extent_entry_count(&page).unwrap(), MAX_EXTENT_ENTRIES);
        let err = append_extent(
            &mut page,
            ExtentEntry::new(Rdba::from_parts(3, 999_999).unwrap(), 1),
        )
        .unwrap_err();
        assert_eq!(err, SegmentError::MapFull);
    }

    #[test]
    fn bitmap_coverage_math_is_pinned() {
        assert_eq!(BITMAP_PAGE_COVERAGE, 65216, "= 130432 位 ÷ 2 位/页");
        assert_eq!(bitmap_page_range(0), (0, 65216));
        assert_eq!(bitmap_page_range(1), (65216, 130432));
        assert_eq!(bitmap_pages_for(0), 0);
        assert_eq!(bitmap_pages_for(1), 1);
        assert_eq!(bitmap_pages_for(65216), 1);
        assert_eq!(bitmap_pages_for(65217), 2);
    }

    #[test]
    fn wrong_page_type_is_rejected() {
        let mut page = Page::new(PageType::HeapTable, [0u8; 8], 3, 0);
        assert_eq!(read_header(&page), Err(SegmentError::NotSegmentHeader));
        assert_eq!(
            write_header(
                &mut page,
                &SegmentHeader {
                    seg_type: SegType::Heap,
                    map_format: 1,
                    flags: 0,
                    dataobj: 0,
                    obj: 0,
                    pages_per_extent: 8,
                    itl_max: 1,
                    pctfree: 0,
                    table_opts: 0,
                    hwm: 0,
                    append_pos: 0,
                    first_bitmap_page: 0,
                    insert_hint: 0,
                    extent_count: 0,
                    bitmap_pages: 0,
                    next_map_page: 0,
                }
            ),
            Err(SegmentError::NotSegmentHeader)
        );
        // 页初始化标志不受影响（Page::new 已置 INITIALIZED）。
        assert_eq!(page.header().unwrap().flags & flags::INITIALIZED, 1);
        let _ = Lsn::from_raw(0); // 保持 Lsn 导入被使用（页头 page_lsn 类型）
    }
}
