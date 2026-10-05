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
    /// Undo 段 = **事务表 6144B + 段控制 24B**（§4.6.3/§4.6.4；控制块宽度
    /// 为 P3 undo 切片所钉）；B+Tree = 树头 6B；ANN 的扩展区布局随 P8 定案
    /// （当前明确拒绝，避免猜 size）。
    #[must_use]
    pub const fn extension_len(self) -> Option<usize> {
        match self {
            SegType::Heap | SegType::Adjacency | SegType::Temporary => Some(0),
            SegType::BTree => Some(6),
            SegType::Undo => Some(crate::undo::UNDO_EXTENSION_LEN),
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
/// 把新区间项插进区映射并**相邻合并**。
///
/// **隐含前提**：映射项按 `(file_id, block_id)` 升序 ⇔ 逻辑页序——
/// 即"后分配的区物理块号更大"。当前的分配只有"尾部增长"一种来源，
/// 前提成立；**区回收/复用落地时必须重审**（低块号回填会让
/// [`Segment::logical_of_block`] 的按序累加错乱）。
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

/// 计划中的段扩展（[`Segment::plan_extend`]）：要写的页镜像 + 新区号。
pub struct PlannedExtend {
    /// 分配的区号。
    pub extent: crate::bitmap::ExtentNo,
    /// 要写的页：（rdba, 前像, 后像）。
    pub images: Vec<(Rdba, Page, Page)>,
}

/// **计划推进过位图页**（append_pos 落在窗口首位时）：要写的页分两类——
/// `images`（既有页：扩展的文件位图/段头页、窗口内其余位图页）经池写 redo；
/// `fresh`（**全新位图页自身**：`before` 为零页）必须先**格式化落盘 + fsync**
/// 再让别处引用它（物理增量无法重建一个不存在的页——与"新撤销页"同规）。
pub struct PlannedAdvance {
    /// 要写的既有页：（rdba, 前像, 后像）。
    pub images: Vec<(Rdba, Page, Page)>,
    /// 全新的位图页：（rdba, 初始化后的页）。
    pub fresh: Vec<(Rdba, Page)>,
    /// 段头页的**后像**（`append_pos` 已推进、`bitmap_pages` 已 +1）——
    /// 调用方在其上继续做本次的槽更新（否则会把这两个字段写回旧值）。
    pub header_after: Page,
}

/// 第 `index` 个段内位图页覆盖的逻辑页区间 `[起, 止)`（**诊断/测试用**；
/// 生产路径用 [`Segment::bitmap_slot`]）。`index × coverage` 以 u64 计算后
/// 收窄——大 index 不会回绕。
#[must_use]
pub const fn bitmap_page_range(index: u32) -> (u32, u32) {
    let start = (index as u64 * BITMAP_PAGE_COVERAGE as u64) as u32;
    (start, start + BITMAP_PAGE_COVERAGE)
}

/// 覆盖 `pages` 个逻辑页所需的段内位图页数。
#[must_use]
pub const fn bitmap_pages_for(pages: u32) -> u32 {
    pages.div_ceil(BITMAP_PAGE_COVERAGE)
}

// ---------------------------------------------------------------------------
// 段：创建、扩展与页访问（在数据文件之上；§4.2/§4.5）
// ---------------------------------------------------------------------------

/// 区映射格式版本（独立于页格式，§5.11）。
pub const SEG_MAP_FORMAT: u8 = 1;

/// 段的空间操作错误（格式错误 + 文件/分配错误）。
#[derive(Debug)]
pub enum SegmentSpaceError {
    /// 段头格式错误。
    Format(SegmentError),
    /// 数据文件错误（含**文件满**）。
    File(crate::datafile::DataFileError),
    /// 底层 I/O。
    Io(std::io::Error),
    /// 段内位图页覆盖范围不足（多页位图随后切片）。
    BitmapCoverage,
    /// 段内位图操作错误（kind/own_index/位号）。
    Bitmap(crate::bitmap::BitmapError),
}

impl std::fmt::Display for SegmentSpaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SegmentSpaceError::Format(e) => write!(f, "{e}"),
            SegmentSpaceError::File(e) => write!(f, "{e}"),
            SegmentSpaceError::Io(e) => write!(f, "段 I/O：{e}"),
            SegmentSpaceError::BitmapCoverage => f.write_str("段内位图页覆盖不足（多页位图随后）"),
            SegmentSpaceError::Bitmap(e) => write!(f, "段内位图：{e}"),
        }
    }
}

impl std::error::Error for SegmentSpaceError {}

impl From<SegmentError> for SegmentSpaceError {
    fn from(e: SegmentError) -> Self {
        SegmentSpaceError::Format(e)
    }
}

impl From<crate::datafile::DataFileError> for SegmentSpaceError {
    fn from(e: crate::datafile::DataFileError) -> Self {
        SegmentSpaceError::File(e)
    }
}

impl From<std::io::Error> for SegmentSpaceError {
    fn from(e: std::io::Error) -> Self {
        SegmentSpaceError::Io(e)
    }
}

impl From<crate::bitmap::BitmapError> for SegmentSpaceError {
    fn from(e: crate::bitmap::BitmapError) -> Self {
        SegmentSpaceError::Bitmap(e)
    }
}

/// 一个段：段头 + 区映射的内存镜像 + 其所在的数据文件。
///
/// 布局约定（每个段的**首区**）：逻辑页 0 = 段头页、逻辑页 1 = **首个段内
/// 位图页**（空闲级别）、逻辑页 2.. 起为数据页；`HWM` / `追加位置` 从 2 起。
/// 区按**到达顺序**占逻辑页段——合并只改区映射的存法，不改逻辑编号。
pub struct Segment<'io, 'f> {
    file: &'f mut crate::datafile::DataFile<'io>,
    header: SegmentHeader,
    page0: u32,
    map: Vec<ExtentEntry>,
    /// 段内位图页的覆盖（位图页 i 管 `[i×coverage, (i+1)×coverage)`；
    /// 生产恒 [`BITMAP_PAGE_COVERAGE`]——**多位图页的落点**：页 i（i≥1）固定
    /// 落在逻辑页 `i×coverage`，i=0 特例在逻辑页 1（窗口首位自指、恒标满）；
    /// 测试可缩小以走到该路径）。
    coverage: u32,
}

impl std::fmt::Debug for Segment<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Segment")
            .field("seg_type", &self.header.seg_type)
            .field("obj", &self.header.obj)
            .field("page0", &self.page0)
            .field("extents", &self.map)
            .field("hwm", &self.header.hwm)
            .finish_non_exhaustive()
    }
}

impl<'io, 'f> Segment<'io, 'f> {
    /// **创建段**：分配首区、写段头页（含首个区映射条目）与段内位图页，
    /// 并把元数据页标 `FULL`、数据页标 `High`。
    pub fn create(
        file: &'f mut crate::datafile::DataFile<'io>,
        seg_type: SegType,
        obj: u32,
        dataobj: u32,
        itl_max: u8,
        pctfree: u8,
        table_opts: u16,
    ) -> Result<Self, SegmentSpaceError> {
        let extent = file.allocate_extent()?;
        let page0 = extent.first_block();
        let header = SegmentHeader {
            seg_type,
            map_format: SEG_MAP_FORMAT,
            flags: 0,
            dataobj,
            obj,
            pages_per_extent: EXTENT_BLOCKS as u8,
            itl_max,
            pctfree,
            table_opts,
            hwm: 2, // 逻辑页 0/1 = 元数据；数据页从 2 起
            append_pos: 2,
            first_bitmap_page: 1,
            insert_hint: 1,
            extent_count: 0, // append_extent 落定首区后为 1
            bitmap_pages: 1,
            next_map_page: 0,
        };
        let rdba = Rdba::from_parts(file.file_id(), page0).expect("块号在 28 位内");
        let mut page = Page::new(
            PageType::SegmentHeader,
            file.workspace_ref(),
            file.file_id(),
            page0,
        );
        write_header(&mut page, &header)?;
        let map = append_extent(&mut page, ExtentEntry::new(rdba, 1))?;
        let header = read_header(&page)?;
        file.write_page(page0, &mut page)?;

        // 段内位图页（类型 8，kind = 空闲级别；own_index = 0）。
        let mut bmp = Page::new(
            PageType::Bitmap,
            file.workspace_ref(),
            file.file_id(),
            page0 + 1,
        );
        crate::bitmap::init(&mut bmp, crate::bitmap::BitmapKind::FreeLevel, 0)?;
        for k in 0..2u32 {
            crate::bitmap::set_free_level(&mut bmp, k, crate::bitmap::FreeLevel::Full)?;
        }
        for k in 2..EXTENT_BLOCKS {
            crate::bitmap::set_free_level(&mut bmp, k, crate::bitmap::FreeLevel::High)?;
        }
        file.write_page(page0 + 1, &mut bmp)?;

        Ok(Self {
            file,
            header,
            page0,
            map,
            coverage: BITMAP_PAGE_COVERAGE,
        })
    }

    /// **打开既有段**（段头物理块已知——来自 `seg$` / 引导页）。
    pub fn open(
        file: &'f mut crate::datafile::DataFile<'io>,
        page0: u32,
    ) -> Result<Self, SegmentSpaceError> {
        let page = file.read_page(page0)?;
        let header = read_header(&page)?;
        let map = read_extents(&page)?;
        Ok(Self {
            file,
            header,
            page0,
            map,
            coverage: BITMAP_PAGE_COVERAGE,
        })
    }

    /// 缩小段内位图页的覆盖（**仅供测试/诊断**：让"append_pos 走到位图页"
    /// 的路径在几页之内可达）。
    ///
    /// **警告**：生产布局固定为 [`BITMAP_PAGE_COVERAGE`]（65216）——覆盖与
    /// 磁盘上已落定的位图页落点绑定，改变它会让既有段的布局解释错位。
    /// 只允许在"全新段、且只用于内存/测试文件"上调用。
    #[must_use]
    pub fn with_coverage(mut self, coverage: u32) -> Self {
        self.coverage = coverage;
        self
    }

    /// **扩展**：分配一个新区（就近合并进区映射）、持久化段头页，
    /// 并把新区的数据页在段内位图里标 `High`。
    pub fn extend(&mut self) -> Result<crate::bitmap::ExtentNo, SegmentSpaceError> {
        let first_logical = self.header.extent_count as u32 * EXTENT_BLOCKS;
        let extent = self.file.allocate_extent()?;
        let rdba =
            Rdba::from_parts(self.file.file_id(), extent.first_block()).expect("块号在 28 位内");
        let mut page = self.file.read_page(self.page0)?;
        let map = append_extent(&mut page, ExtentEntry::new(rdba, 1))?;
        self.file.write_page(self.page0, &mut page)?;
        self.header = read_header(&page)?;
        self.map = map;

        // 新区数据页 → High（按覆盖自动落到对应位图页；跨窗时先物化位图页 i）。
        // **位图页自身跳过**：它的级别由物化时写（窗口首位自指、恒满）。
        for k in first_logical..first_logical + EXTENT_BLOCKS {
            if self.is_bitmap_page(k) {
                continue;
            }
            self.write_free_level(k, crate::bitmap::FreeLevel::High)?;
        }
        Ok(extent)
    }

    /// **段内位图页的落点**：逻辑页 `logical` 的级别位在
    /// （第 `i` 个位图页，页内位号 `bit`）——位图页 i 位于逻辑页
    /// `i×coverage`（i=0 特例在逻辑页 1）。
    #[must_use]
    pub fn bitmap_slot(&self, logical: u32) -> (u32, u32, u32) {
        let i = logical / self.coverage;
        let bitmap_logical = if i == 0 { 1 } else { i * self.coverage };
        (i, logical - i * self.coverage, bitmap_logical)
    }

    /// 该逻辑页**是不是位图页本身**（i≥1 的窗口首位；i=0 的位图页在逻辑页 1）。
    #[must_use]
    pub fn is_bitmap_page(&self, logical: u32) -> bool {
        logical == 1 || (logical != 0 && logical % self.coverage == 0)
    }

    /// **写一个逻辑页的空闲级别**（自动落到对应位图页；跨窗先物化位图页）。
    pub fn write_free_level(
        &mut self,
        logical: u32,
        level: crate::bitmap::FreeLevel,
    ) -> Result<(), SegmentSpaceError> {
        let (i, bit, bitmap_logical) = self.bitmap_slot(logical);
        self.ensure_bitmap_page(i, bitmap_logical)?;
        let block = self
            .logical_block(bitmap_logical)
            .ok_or(SegmentSpaceError::BitmapCoverage)?;
        let mut bmp = self.file.read_page(block)?;
        crate::bitmap::set_free_level(&mut bmp, bit, level)?;
        self.file.write_page(block, &mut bmp)?;
        Ok(())
    }

    /// **物化第 i 个位图页**（必要时扩展段到该逻辑页所在区；幂等）。
    pub fn ensure_bitmap_page(
        &mut self,
        i: u32,
        bitmap_logical: u32,
    ) -> Result<(), SegmentSpaceError> {
        if u32::from(self.header.bitmap_pages) > i {
            return Ok(()); // 已物化（位图页按序物化）
        }
        // 扩到覆盖该逻辑页（每扩一区 +8 逻辑页）。
        while self.logical_block(bitmap_logical).is_none() {
            self.extend()?;
        }
        // 页体：FreeLevel、own_index = i；**窗口首位自指**——自己的位标满。
        let block = self
            .logical_block(bitmap_logical)
            .ok_or(SegmentSpaceError::BitmapCoverage)?;
        let mut bmp = Page::new(
            PageType::Bitmap,
            self.file.workspace_ref(),
            self.file.file_id(),
            block,
        );
        crate::bitmap::init(&mut bmp, crate::bitmap::BitmapKind::FreeLevel, i as u16)?;
        crate::bitmap::set_free_level(&mut bmp, 0, crate::bitmap::FreeLevel::Full)?;
        self.file.write_page(block, &mut bmp)?;
        // 段头：位图页数 +1（读当前页，避免覆盖 extend 的写入）。
        let mut page = self.file.read_page(self.page0)?;
        let mut h = read_header(&page)?;
        h.bitmap_pages = h.bitmap_pages.saturating_add(1);
        write_header(&mut page, &h)?;
        self.file.write_page(self.page0, &mut page)?;
        self.header = h;
        Ok(())
    }

    /// **计划扩展**（**不写盘**）：分配新区 + 段头页（区映射/计数）+ 段内
    /// 位图页的（前像、后像）——写路径经缓冲池落盘（redo 保护，
    /// §11.5.3"页/区分配是系统操作"）。决策已在返回镜像里定下。
    pub fn plan_extend(&mut self) -> Result<PlannedExtend, SegmentSpaceError> {
        let planned = self.file.plan_allocate_extent()?;
        let rdba = Rdba::from_parts(self.file.file_id(), planned.extent.first_block())
            .expect("块号在 28 位内");

        // 段头页（前/后）：区映射 + 计数。
        let header_before = self.file.read_page(self.page0)?;
        let mut header_after = Page::from_bytes(Box::new(*header_before.as_bytes()));
        let map = append_extent(&mut header_after, ExtentEntry::new(rdba, 1))?;
        let header_after_state = read_header(&header_after)?;

        // 段内位图页（前/后）：新区数据页 → High（位图页自身跳过）。
        let first_logical = self.header.extent_count as u32 * EXTENT_BLOCKS;
        let mut bmp_images: Vec<(u32, Page, Page)> = Vec::new();
        for k in first_logical..first_logical + EXTENT_BLOCKS {
            if self.is_bitmap_page(k) {
                continue;
            }
            let (_, bit, bmp_logical) = self.bitmap_slot(k);
            if self.logical_block(bmp_logical).is_none() {
                // 该位图页**落在本次新增的区里**（窗口首位 = 位图页自身，
                // 如 `k = 65217` 的位图页在 65216）：不需要逐位标记——
                // 位图页的**初始化**会把本窗口的非位图页统一标 `High`
                // （bit0 自指 `Full`）。物化由调用方随后执行。
                if bmp_logical >= first_logical && bmp_logical < first_logical + EXTENT_BLOCKS {
                    continue;
                }
                // 更远窗口的位图页：不在本次扩展的落点内——仍按覆盖不足拒绝。
                return Err(SegmentSpaceError::BitmapCoverage);
            }
            let idx = bmp_images.iter().position(|(l, _, _)| *l == bmp_logical);
            let (entry_idx, _) = match idx {
                Some(i) => (i, ()),
                None => {
                    let before = self.file.read_page(
                        self.logical_block(bmp_logical)
                            .ok_or(SegmentSpaceError::BitmapCoverage)?,
                    )?;
                    let after = Page::from_bytes(Box::new(*before.as_bytes()));
                    bmp_images.push((bmp_logical, before, after));
                    (bmp_images.len() - 1, ())
                }
            };
            let (_, _, after) = &mut bmp_images[entry_idx];
            crate::bitmap::set_free_level(after, bit, crate::bitmap::FreeLevel::High)?;
        }

        // 文件级位图页的镜像 → rdba。
        let mut images: Vec<(crate::rowid::Rdba, Page, Page)> = Vec::new();
        for (page_in_run, before, after) in planned.images {
            let block = planned.run_start + u32::from(page_in_run);
            let rdba = Rdba::from_parts(self.file.file_id(), block).expect("块号在 28 位内");
            images.push((rdba, before, after));
        }
        images.push((
            Rdba::from_parts(self.file.file_id(), self.page0).expect("块号在 28 位内"),
            header_before,
            header_after,
        ));
        for (logical, before, after) in bmp_images {
            let block = self
                .logical_block(logical)
                .ok_or(SegmentSpaceError::BitmapCoverage)?;
            images.push((
                Rdba::from_parts(self.file.file_id(), block).expect("块号在 28 位内"),
                before,
                after,
            ));
        }

        // 内存状态推进（镜像已定，写盘由调用方完成）。
        self.header = header_after_state;
        self.map = map;
        Ok(PlannedExtend {
            extent: planned.extent,
            images,
        })
    }

    /// **计划物化第 `i` 个段内位图页**（**不写盘**；`i ≥ 1` 时它位于逻辑页
    /// `i×coverage`，即窗口首位——`append_pos` 恰走到它时需要先物化再前进）。
    ///
    /// 步骤：必要时先**计划扩展**（可多次）直到该逻辑页落入区映射；然后给出
    /// 位图页镜像（`fresh`：初始化 = bit0 自指 `Full`、本窗口其余位 `High`）
    /// 与段头页镜像（`append_pos = 位图页 + 1`、`bitmap_pages += 1`）。
    /// 内存状态随即推进（与 [`Segment::plan_extend`] 同规）。
    pub fn plan_materialize_bitmap_page(
        &mut self,
        i: u32,
    ) -> Result<PlannedAdvance, SegmentSpaceError> {
        let (_, _, bmp_logical) = self.bitmap_slot(i * self.coverage);
        let mut images: Vec<(Rdba, Page, Page)> = Vec::new();
        while self.logical_block(bmp_logical).is_none() {
            let planned = self.plan_extend()?;
            images.extend(planned.images);
        }
        let block = self
            .logical_block(bmp_logical)
            .ok_or(SegmentSpaceError::BitmapCoverage)?;
        let rdba = Rdba::from_parts(self.file.file_id(), block)
            .ok_or(SegmentSpaceError::Format(SegmentError::Malformed))?;

        // 全新位图页：bit0 自指 Full（窗口首位），其余位 High（空白数据页）。
        let mut bmp = Page::new(
            PageType::Bitmap,
            self.file.workspace_ref(),
            self.file.file_id(),
            block,
        );
        crate::bitmap::init(&mut bmp, crate::bitmap::BitmapKind::FreeLevel, i as u16)?;
        crate::bitmap::set_free_level(&mut bmp, 0, crate::bitmap::FreeLevel::Full)?;
        let window_first = i * self.coverage;
        for k in 1..self.coverage {
            if window_first + k == bmp_logical {
                continue; // 不会发生（位图页恒在窗口首位）；防御
            }
            crate::bitmap::set_free_level(&mut bmp, k, crate::bitmap::FreeLevel::High)?;
        }

        // 段头页：append_pos 跳过位图页、bitmap_pages +1。
        let header_before = self.file.read_page(self.page0)?;
        let mut header_after = Page::from_bytes(Box::new(*header_before.as_bytes()));
        let mut h = read_header(&header_after)?;
        h.append_pos = bmp_logical + 1;
        h.bitmap_pages = h.bitmap_pages.saturating_add(1);
        write_header(&mut header_after, &h)?;
        let header_after_state = read_header(&header_after)?;
        let header_after_copy = Page::from_bytes(Box::new(*header_after.as_bytes()));
        images.push((
            Rdba::from_parts(self.file.file_id(), self.page0)
                .ok_or(SegmentSpaceError::Format(SegmentError::Malformed))?,
            header_before,
            header_after,
        ));
        self.header = header_after_state;

        Ok(PlannedAdvance {
            images,
            fresh: vec![(rdba, bmp)],
            header_after: header_after_copy,
        })
    }

    /// **高水位**（内存镜像；§4.3.1）。
    #[must_use]
    pub fn hwm(&self) -> u32 {
        self.header.hwm
    }

    // **扫描上界**由调用方按表类型选（§4.3.1）：`in_place` 取 [`Segment::hwm`]，
    // `append_only` 取 [`Segment::append_position`]——表类型在目录/表选项里，
    // 段层不重复存；扫描方在 `[0, bound)` 上做区读（§5.12）。

    /// **计划推进高水位**（**只增**；§4.3.1）：返回段头页的（前像、后像）——
    /// 调用方经池写 redo（系统操作，§11.5.3）。**降低 HWM 直接拒绝**（只有
    /// TRUNCATE/重组类独占操作可以降，走独立入口）。
    pub fn plan_advance_hwm(&mut self, new_hwm: u32) -> Result<(Page, Page), SegmentSpaceError> {
        if new_hwm < self.header.hwm {
            return Err(SegmentSpaceError::Format(SegmentError::Malformed));
        }
        let header_before = self.file.read_page(self.page0)?;
        let mut header_after = Page::from_bytes(Box::new(*header_before.as_bytes()));
        let mut h = read_header(&header_after)?;
        h.hwm = new_hwm;
        write_header(&mut header_after, &h)?;
        self.header = read_header(&header_after)?;
        Ok((header_before, header_after))
    }

    /// **当前追加位置**（只读：直读段头页的 `append_pos`）。
    pub fn append_position(&self) -> Result<u32, SegmentSpaceError> {
        Ok(read_header(&self.file.read_page(self.page0)?)?.append_pos)
    }

    /// **读一页不校验**（未写过零页的探测；见 [`crate::datafile::DataFile::read_page_unverified`]）。
    pub fn read_page_raw(&self, logical: u32) -> Result<Page, SegmentSpaceError> {
        if self.is_bitmap_page(logical) {
            return Err(SegmentSpaceError::BitmapCoverage);
        }
        let block = self
            .logical_block(logical)
            .ok_or(SegmentSpaceError::BitmapCoverage)?;
        Ok(self.file.read_page_unverified(block)?)
    }

    /// **按物理块号直写一页**（不经区映射/逻辑页——"先格式化落盘"用）。
    pub fn write_physical_page(
        &self,
        block: u32,
        page: &mut Page,
    ) -> Result<(), SegmentSpaceError> {
        self.file.write_page(block, page)?;
        Ok(())
    }

    /// **准备下一个可写的追加逻辑页**：跳过（并物化）跨到的位图页本身。
    pub fn prepare_append_page(&mut self) -> Result<u32, SegmentSpaceError> {
        loop {
            let mut h = read_header(&self.file.read_page(self.page0)?)?;
            let logical = h.append_pos;
            if self.is_bitmap_page(logical) {
                let (i, _, bmp_logical) = self.bitmap_slot(logical);
                self.ensure_bitmap_page(i, bmp_logical)?;
                h = read_header(&self.file.read_page(self.page0)?)?;
                h.append_pos = logical + 1;
                let mut page = self.file.read_page(self.page0)?;
                write_header(&mut page, &h)?;
                self.file.write_page(self.page0, &mut page)?;
                self.header = h;
                continue;
            }
            return Ok(logical);
        }
    }

    /// 段头（内存镜像）。
    #[must_use]
    pub fn header(&self) -> &SegmentHeader {
        &self.header
    }

    /// 区映射（内存镜像）。
    #[must_use]
    pub fn extents(&self) -> &[ExtentEntry] {
        &self.map
    }

    /// 段头页的物理块号。
    #[must_use]
    pub fn page0_block(&self) -> u32 {
        self.page0
    }

    /// 段所在文件的文件号。
    #[must_use]
    pub fn file_id(&self) -> u16 {
        self.file.file_id()
    }

    /// 工作区受校验标识。
    #[must_use]
    pub fn workspace_ref(&self) -> [u8; 8] {
        self.file.workspace_ref()
    }

    /// **逻辑页号 → 物理块**（经区映射；`None` = 该逻辑页尚未分配）。
    #[must_use]
    pub fn logical_block(&self, logical: u32) -> Option<u32> {
        logical_to_rdba(&self.map, logical).map(|r| r.block_id())
    }

    /// **物理块 → 逻辑页号**（区映射反查；`None` = 该块不属于本段）。
    ///
    /// 用途：undo 链上存的是**物理 ROWID**（§4.6.2），回读时经此还原
    /// 逻辑页号再走 `read_page`。
    #[must_use]
    pub fn logical_of_block(&self, block: u32) -> Option<u32> {
        let mut base = 0u32;
        for e in &self.map {
            let len = u32::from(e.extents) * EXTENT_BLOCKS;
            if block >= e.start.block_id() && block < e.start.block_id() + len {
                return Some(base + (block - e.start.block_id()));
            }
            base += len;
        }
        None
    }

    /// 段文件的**持久性点**（`fdatasync`）。
    ///
    /// 用途：新开撤销页的"**先落盘、后进 redo**"次序（§11.5.4 的实现注记）
    /// ——物理增量 redo 无法重建一个不存在的页，所以新页必须先持久化。
    pub fn sync(&self) -> Result<(), SegmentSpaceError> {
        self.file.sync()?;
        Ok(())
    }

    /// 读一个逻辑页（两层完整性校验——**未格式化的数据页读会失败**，
    /// 这是"未初始化页不得使用"的落点）。
    pub fn read_page(&self, logical: u32) -> Result<Page, SegmentSpaceError> {
        let block = self
            .logical_block(logical)
            .ok_or(SegmentSpaceError::Format(SegmentError::Malformed))?;
        Ok(self.file.read_page(block)?)
    }

    /// 写一个逻辑页（seal + 定址写；不隐式 fsync）。
    pub fn write_page(&self, logical: u32, page: &mut Page) -> Result<(), SegmentSpaceError> {
        let block = self
            .logical_block(logical)
            .ok_or(SegmentSpaceError::Format(SegmentError::Malformed))?;
        self.file.write_page(block, page)?;
        Ok(())
    }
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
        assert_eq!(
            entries_offset(SegType::Undo).unwrap(),
            128 + 6168,
            "事务表 6144B + 段控制 24B"
        );
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

#[cfg(test)]
mod space_tests {
    use bicdb_workspace::io::MemFileIo;
    use std::path::Path;

    use super::*;
    use crate::bitmap::{self, BitmapKind, FreeLevel};
    use crate::datafile::DataFile;
    use crate::page::WORKSPACE_REF_LEN;

    const F: &str = "/mem/data1.dat";
    const WS: [u8; 8] = [7u8; 8];

    fn mem() -> MemFileIo {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        io
    }

    #[test]
    fn create_segment_lays_out_header_bitmap_and_extent() {
        let io = mem();
        let mut file = DataFile::create(&io, Path::new(F), 3, 3, WS, 512).unwrap();
        let seg = Segment::create(&mut file, SegType::Heap, 11, 12, 4, 10, 0x0001).unwrap();

        assert_eq!(
            seg.page0_block(),
            crate::bitmap::DATA_AREA_FIRST_BLOCK,
            "首区首块 = 预留区之后"
        );
        let h = seg.header();
        assert_eq!(h.seg_type, SegType::Heap);
        assert_eq!((h.obj, h.dataobj), (11, 12));
        assert_eq!(h.itl_max, 4);
        assert_eq!(h.pctfree, 10);
        assert_eq!(h.table_opts, 0x0001);
        assert_eq!(h.hwm, 2);
        assert_eq!(h.append_pos, 2);
        assert_eq!(h.first_bitmap_page, 1);
        assert_eq!(h.insert_hint, 1);
        assert_eq!(h.bitmap_pages, 1);
        assert_eq!(h.extent_count, 1);
        assert_eq!(h.map_format, SEG_MAP_FORMAT);
        assert_eq!(
            seg.extents(),
            &[ExtentEntry::new(
                Rdba::from_parts(3, crate::bitmap::DATA_AREA_FIRST_BLOCK).unwrap(),
                1
            )]
        );

        // 逻辑页 0 = 段头页；逻辑页 1 = 空闲级别位图页。
        let d0 = crate::bitmap::DATA_AREA_FIRST_BLOCK;
        assert_eq!(seg.logical_block(0), Some(d0));
        assert_eq!(seg.logical_block(1), Some(d0 + 1));
        assert_eq!(seg.logical_block(7), Some(d0 + 7));
        assert_eq!(seg.logical_block(8), None, "第二个区尚未分配");
        let bmp = seg.read_page(1).unwrap();
        assert_eq!(bitmap::kind(&bmp).unwrap(), BitmapKind::FreeLevel);
        assert_eq!(bitmap::own_index(&bmp).unwrap(), 0);
        assert_eq!(bitmap::free_level(&bmp, 0).unwrap(), FreeLevel::Full);
        assert_eq!(bitmap::free_level(&bmp, 1).unwrap(), FreeLevel::Full);
        assert_eq!(bitmap::free_level(&bmp, 2).unwrap(), FreeLevel::High);
        assert_eq!(bitmap::free_level(&bmp, 7).unwrap(), FreeLevel::High);
    }

    #[test]
    fn extend_merges_and_marks_levels() {
        let io = mem();
        let mut file = DataFile::create(&io, Path::new(F), 3, 3, WS, 512).unwrap();
        let mut seg = Segment::create(&mut file, SegType::Heap, 1, 2, 4, 10, 0).unwrap();
        let e = seg.extend().unwrap();
        assert_eq!(e.first_block(), crate::bitmap::DATA_AREA_FIRST_BLOCK + 8);
        // 相邻 ⇒ 合并为一条（1 区 → 2 区）。
        assert_eq!(
            seg.extents(),
            &[ExtentEntry::new(
                Rdba::from_parts(3, crate::bitmap::DATA_AREA_FIRST_BLOCK).unwrap(),
                2
            )]
        );
        assert_eq!(seg.header().extent_count, 2);
        let d0 = crate::bitmap::DATA_AREA_FIRST_BLOCK;
        assert_eq!(seg.logical_block(8), Some(d0 + 8), "新区首页");
        assert_eq!(seg.logical_block(15), Some(d0 + 15));
        // 新区的数据页在段内位图标 High（逻辑页 8..16）。
        let bmp = seg.read_page(1).unwrap();
        for k in 8..16u32 {
            assert_eq!(bitmap::free_level(&bmp, k).unwrap(), FreeLevel::High);
        }
    }

    #[test]
    fn open_reloads_segment() {
        let io = mem();
        {
            let mut file = DataFile::create(&io, Path::new(F), 3, 3, WS, 512).unwrap();
            let mut seg = Segment::create(&mut file, SegType::Temporary, 5, 6, 2, 0, 0).unwrap();
            seg.extend().unwrap();
            file.sync().unwrap();
            file.close().unwrap();
        }
        let mut file = DataFile::open(&io, Path::new(F)).unwrap();
        let seg = Segment::open(&mut file, crate::bitmap::DATA_AREA_FIRST_BLOCK).unwrap();
        assert_eq!(seg.header().seg_type, SegType::Temporary);
        assert_eq!(seg.header().extent_count, 2);
        assert_eq!(
            seg.logical_block(15),
            Some(crate::bitmap::DATA_AREA_FIRST_BLOCK + 15)
        );
    }

    #[test]
    fn formatted_page_roundtrip_through_logical_mapping() {
        let io = mem();
        let mut file = DataFile::create(&io, Path::new(F), 3, 3, WS, 512).unwrap();
        let seg = Segment::create(&mut file, SegType::Heap, 1, 2, 4, 10, 0).unwrap();

        // 未格式化的数据页读取会失败（未初始化页不得使用）。
        assert!(seg.read_page(2).is_err());

        // 插入路径会先"格式化"页（这里是它的替身）：新建页（**块号 = 该区第 3 块**）+ 写。
        let block = crate::bitmap::DATA_AREA_FIRST_BLOCK + 2;
        let mut page = Page::new(PageType::HeapTable, [0u8; WORKSPACE_REF_LEN], 3, block);
        seg.write_page(2, &mut page).unwrap();
        let back = seg.read_page(2).unwrap();
        assert_eq!(back.as_bytes(), page.as_bytes());
        assert_eq!(
            back.header().unwrap().block_id,
            block,
            "逻辑页 2 ↔ 该区第 3 块（预留区之后）"
        );
    }

    #[test]
    fn plan_advance_hwm_is_monotonic_and_planned() {
        // §4.3.1：HWM 推进是**只增**的计划操作（段头前后像交调用方经池写 redo）。
        let io = mem();
        let mut file = DataFile::create(&io, Path::new(F), 3, 3, WS, 512).unwrap();
        let mut seg = Segment::create(&mut file, SegType::Heap, 2, 3, 4, 0, 0).unwrap();
        assert_eq!(seg.hwm(), 2, "创建后 HWM = 2（逻辑页 0/1 是元数据）");
        let (before, after) = seg.plan_advance_hwm(9).unwrap();
        assert_eq!(read_header(&before).unwrap().hwm, 2, "前像 = 旧值");
        assert_eq!(read_header(&after).unwrap().hwm, 9, "后像 = 新值");
        assert_eq!(seg.hwm(), 9, "内存镜像同步推进");
        // **降低被拒绝**（只有 TRUNCATE/重组类独占操作能降）。
        assert!(seg.plan_advance_hwm(8).is_err());
        assert_eq!(seg.hwm(), 9, "拒绝后不变");
    }

    #[test]
    fn multi_page_bitmaps_materialize_on_window_boundary() {
        let io = mem();
        let mut file = DataFile::create(&io, Path::new(F), 3, 3, WS, 512).unwrap();
        // coverage = 8（= 一个区）：位图页 0 管 [0,8)（在逻辑页 1）；
        // 位图页 1 管 [8,16)，落在逻辑页 8（窗口首位、自指）。
        let mut seg = Segment::create(&mut file, SegType::Heap, 1, 2, 4, 10, 0)
            .unwrap()
            .with_coverage(8);
        seg.extend().unwrap(); // 第二区：逻辑页 8..16 就位
        assert!(seg.is_bitmap_page(1) && seg.is_bitmap_page(8));
        assert!(!seg.is_bitmap_page(0), "逻辑页 0 是段头页（不是位图页）");
        assert!(!seg.is_bitmap_page(9));
        assert_eq!(seg.bitmap_slot(0), (0, 0, 1));
        assert_eq!(seg.bitmap_slot(7), (0, 7, 1));
        assert_eq!(seg.bitmap_slot(8), (1, 0, 8), "跨窗：落第 1 个位图页");
        assert_eq!(seg.bitmap_slot(9), (1, 1, 8));

        // 扩到窗口 1 时其位图页随标记**自动物化**（窗口首位自指、恒满）。
        assert_eq!(seg.header().bitmap_pages, 2, "位图页 1 随第二次扩展物化");
        // 写逻辑页 9 的级别 → 落到位图页 1 的位 1。
        seg.write_free_level(9, crate::bitmap::FreeLevel::Medium)
            .unwrap();
        let b1 = seg.read_page(8).unwrap();
        assert_eq!(crate::bitmap::kind(&b1).unwrap(), BitmapKind::FreeLevel);
        assert_eq!(crate::bitmap::own_index(&b1).unwrap(), 1);
        assert_eq!(
            crate::bitmap::free_level(&b1, 0).unwrap(),
            FreeLevel::Full,
            "窗口首位自指：位图页自身标满"
        );
        assert_eq!(
            crate::bitmap::free_level(&b1, 1).unwrap(),
            FreeLevel::Medium,
            "逻辑页 9 的级别落在位图页 1 的位 1"
        );
        // 位图页 0 的位 7（逻辑页 7）不受影响。
        let b0 = seg.read_page(1).unwrap();
        assert_eq!(crate::bitmap::free_level(&b0, 7).unwrap(), FreeLevel::High);

        // 追加位置跨到时：物化并跳过位图页本身。
        seg.write_free_level(8, FreeLevel::Full).unwrap(); // 幂等（已物化）
        let mut page = seg.read_page(0).unwrap();
        let mut h = read_header(&page).unwrap();
        h.append_pos = 8;
        write_header(&mut page, &h).unwrap();
        seg.write_page(0, &mut page).unwrap();
        assert_eq!(seg.prepare_append_page().unwrap(), 9, "跳过位图页后落到 9");
        assert_eq!(seg.header().append_pos, 9);
    }
}
