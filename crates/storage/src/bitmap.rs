//! 位图页（`page_type = 8`）与区分配图（LMT）（存储架构 §3.2、§5.11）。
//!
//! # 页体（两种用途、同一页体）
//!
//! ```text
//! 偏移 68   页体头（8B）
//!              kind        1B   0 = 区分配图（LMT，1 位/区）/ 1 = 段内空闲级别（2 位/页）
//!              flags       1B
//!              own_index   2B   本位图页在本位图块中的序号（读盘自校验）
//!              reserved    4B
//! 偏移 76   位数组（连续；**低位到高位对应递增的单元号**）
//! ```
//!
//! **容量换算**（页 16 KiB、区 8 页 = 128 KB；数字是算出来的，不是估的）：
//!
//! ```text
//! 位图页的位数组 = 16384 − 68（页头）− 4（页尾校验）− 8（页体头）= 16304 B
//!                = 130432 位
//! 一个位图页  → 管 130432 个区 ≈ 15.9 GB
//! 一个位图区  → 8 页位图 = 1043456 个区 ≈ 127 GB
//! 4 TiB 文件  → ceil(33554432 / 1043456) = 33 个位图区
//! ```
//!
//! **`own_index` 的作用**：位图内容不作地址标记——"第 i 位属于哪个区"由
//! "这是第几个位图页"推算；页放错位置会**静默管错对象**。`own_index`
//! 让这件事可检出（代价 2 字节；[`verify_own_index`]）。
//!
//! # 文件布局（§3.2）
//!
//! ```text
//! 块 0        文件头页（类型 11；内含"位图空间头"）
//! 块 1 起     数据区（区的块从 1 号块开始）
//! 文件尾部    位图区（每个 = 1 个区 = 8 页）——自尾部**往前**分配
//! ```
//!
//! 数据区与位图区**相向而行**；文件增长策略（自动扩展/新增/到顶，§3.3）
//! 与段级分配的接入见后续切片。

use crate::page::{Page, PageType, PAGE_SIZE};

/// 页体头偏移（页头 68B 之后）。
pub const BODY_OFFSET: usize = 68;
/// 页体头长度。
pub const BODY_LEN: usize = 8;
/// 位数组偏移。
pub const BITS_OFFSET: usize = BODY_OFFSET + BODY_LEN;
/// 位数组字节数（用尽页体，不取整到 2 的幂）。
pub const BITS_BYTES: usize = PAGE_SIZE - BODY_OFFSET - BODY_LEN - 4; // 扣页尾校验 4B
/// 一个位图页的位数。
pub const BITS_PER_BITMAP_PAGE: usize = BITS_BYTES * 8; // = 130432

/// 区 = 8 页 = 128 KB。
pub const EXTENT_BLOCKS: u32 = 8;
/// 位图区 = 1 个区 = 8 个位图页。
pub const BITMAP_PAGES_PER_RUN: usize = EXTENT_BLOCKS as usize;
/// 一个位图区管理的区数。
pub const BITS_PER_RUN: usize = BITS_PER_BITMAP_PAGE * BITMAP_PAGES_PER_RUN;
/// 位图区数上限（4 TiB 上限下需 33；留余量到 40）。
pub const MAX_BITMAP_RUNS: usize = 40;
/// 数据区第一个块（块 0 是文件头页）。
pub const DATA_AREA_FIRST_BLOCK: u32 = 1;

/// 位图种类（页体头 `kind`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitmapKind {
    /// 0 = 区分配图（LMT；1 位/区）。
    ExtentMap = 0,
    /// 1 = 段内空闲级别（2 位/页）。
    FreeLevel = 1,
}

/// 段内空闲级别（2 位；§4.5）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FreeLevel {
    /// 0 = FULL（无插入空间）。
    Full = 0,
    /// 1 = 低。
    Low = 1,
    /// 2 = 中。
    Medium = 2,
    /// 3 = 高。
    High = 3,
}

/// 位图操作错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitmapError {
    /// 页类型不是位图页。
    NotABitmapPage,
    /// 页体 `kind` 与操作不符。
    WrongKind,
    /// 位号越出本位图页的位数组。
    BitOutOfRange,
    /// `own_index` 与预期不符（页被放错位置/读错来源）。
    BadOwnIndex,
}

impl std::fmt::Display for BitmapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            BitmapError::NotABitmapPage => "页类型不是位图页",
            BitmapError::WrongKind => "位图 kind 与操作不符",
            BitmapError::BitOutOfRange => "位号越出位数组",
            BitmapError::BadOwnIndex => "位图页 own_index 不符（可能放错位置）",
        })
    }
}

impl std::error::Error for BitmapError {}

/// 全局区号（= 位图区号 × 每区位数 + 区内位号）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExtentNo(u32);

impl ExtentNo {
    /// 由全局区号构造。
    #[must_use]
    pub fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// 全局区号。
    #[must_use]
    pub fn as_raw(self) -> u32 {
        self.0
    }

    /// 该区在数据区的首个块号（块 = 1 + 区号 × 8）。
    #[must_use]
    pub fn first_block(self) -> u32 {
        DATA_AREA_FIRST_BLOCK + self.0 * EXTENT_BLOCKS
    }

    /// 该区覆盖的块数。
    #[must_use]
    pub fn blocks(self) -> u32 {
        EXTENT_BLOCKS
    }
}

fn bitmap_kind(page: &Page) -> Result<BitmapKind, BitmapError> {
    match page.header() {
        Some(h) if h.page_type == PageType::Bitmap => {}
        _ => return Err(BitmapError::NotABitmapPage),
    }
    match page.as_bytes()[BODY_OFFSET] {
        0 => Ok(BitmapKind::ExtentMap),
        1 => Ok(BitmapKind::FreeLevel),
        _ => Err(BitmapError::WrongKind),
    }
}

/// 初始化一张位图页（清空位数组、写页体头、封页）。
pub fn init(page: &mut Page, kind: BitmapKind, own_index: u16) -> Result<(), BitmapError> {
    match page.header() {
        Some(h) if h.page_type == PageType::Bitmap => {}
        _ => return Err(BitmapError::NotABitmapPage),
    }
    {
        let bytes = page.as_bytes_mut();
        bytes[BODY_OFFSET] = kind as u8;
        bytes[BODY_OFFSET + 1] = 0; // flags 保留
        bytes[BODY_OFFSET + 2..BODY_OFFSET + 4].copy_from_slice(&own_index.to_le_bytes());
        bytes[BODY_OFFSET + 4..BODY_OFFSET + BODY_LEN].fill(0); // reserved
        bytes[BITS_OFFSET..BITS_OFFSET + BITS_BYTES].fill(0);
    }
    page.seal();
    Ok(())
}

/// 页体 `kind`。
pub fn kind(page: &Page) -> Result<BitmapKind, BitmapError> {
    bitmap_kind(page)
}

/// `own_index`。
pub fn own_index(page: &Page) -> Result<u16, BitmapError> {
    bitmap_kind(page)?;
    Ok(u16::from_le_bytes(
        page.as_bytes()[BODY_OFFSET + 2..BODY_OFFSET + 4]
            .try_into()
            .expect("2 字节"),
    ))
}

/// 读盘自校验：`own_index` 必须等于预期序号。
pub fn verify_own_index(page: &Page, expected: u16) -> Result<(), BitmapError> {
    if own_index(page)? == expected {
        Ok(())
    } else {
        Err(BitmapError::BadOwnIndex)
    }
}

fn require_extent_map(page: &Page) -> Result<(), BitmapError> {
    match bitmap_kind(page)? {
        BitmapKind::ExtentMap => Ok(()),
        BitmapKind::FreeLevel => Err(BitmapError::WrongKind),
    }
}

fn bit_index(bit: u32) -> Result<(usize, u8), BitmapError> {
    let idx = bit as usize;
    if idx >= BITS_PER_BITMAP_PAGE {
        return Err(BitmapError::BitOutOfRange);
    }
    Ok((idx / 8, (idx % 8) as u8))
}

/// 读一位（区分配图：是否已分配）。
pub fn is_allocated(page: &Page, bit: u32) -> Result<bool, BitmapError> {
    require_extent_map(page)?;
    let (byte, shift) = bit_index(bit)?;
    Ok(page.as_bytes()[BITS_OFFSET + byte] & (1 << shift) != 0)
}

/// 置位（分配）。
pub fn set_allocated(page: &mut Page, bit: u32) -> Result<(), BitmapError> {
    require_extent_map(page)?;
    let (byte, shift) = bit_index(bit)?;
    page.as_bytes_mut()[BITS_OFFSET + byte] |= 1 << shift;
    Ok(())
}

/// 清位（回收）。
pub fn clear_allocated(page: &mut Page, bit: u32) -> Result<(), BitmapError> {
    require_extent_map(page)?;
    let (byte, shift) = bit_index(bit)?;
    page.as_bytes_mut()[BITS_OFFSET + byte] &= !(1 << shift);
    Ok(())
}

/// 自 `from` 起找第一个空闲位（低位优先）。
#[must_use]
pub fn find_free(page: &Page, from: u32) -> Option<u32> {
    require_extent_map(page).ok()?;
    let bytes = &page.as_bytes()[BITS_OFFSET..BITS_OFFSET + BITS_BYTES];
    let mut bit = from as usize;
    while bit < BITS_PER_BITMAP_PAGE {
        let byte = bit / 8;
        if bytes[byte] != 0xFF {
            let mut shift = (bit % 8) as u8;
            while shift < 8 {
                if bytes[byte] & (1 << shift) == 0 {
                    return Some((byte * 8 + shift as usize) as u32);
                }
                shift += 1;
            }
        }
        bit = (byte + 1) * 8;
    }
    None
}

/// 已分配位数。
#[must_use]
pub fn allocated_count(page: &Page) -> u32 {
    if require_extent_map(page).is_err() {
        return 0;
    }
    page.as_bytes()[BITS_OFFSET..BITS_OFFSET + BITS_BYTES]
        .iter()
        .map(|b| b.count_ones())
        .sum()
}

/// 读 2 位空闲级别（`kind = 1`；每页一个单元）。
pub fn free_level(page: &Page, unit: u32) -> Result<FreeLevel, BitmapError> {
    if bitmap_kind(page)? != BitmapKind::FreeLevel {
        return Err(BitmapError::WrongKind);
    }
    let bit = unit.checked_mul(2).ok_or(BitmapError::BitOutOfRange)?;
    let (byte, shift) = bit_index(bit)?;
    let value = (page.as_bytes()[BITS_OFFSET + byte] >> shift) & 0b11;
    Ok(match value {
        0 => FreeLevel::Full,
        1 => FreeLevel::Low,
        2 => FreeLevel::Medium,
        _ => FreeLevel::High,
    })
}

/// 写 2 位空闲级别。
pub fn set_free_level(page: &mut Page, unit: u32, level: FreeLevel) -> Result<(), BitmapError> {
    if bitmap_kind(page)? != BitmapKind::FreeLevel {
        return Err(BitmapError::WrongKind);
    }
    let bit = unit.checked_mul(2).ok_or(BitmapError::BitOutOfRange)?;
    let (byte, shift) = bit_index(bit)?;
    let mask = 0b11u8 << shift;
    let bytes = page.as_bytes_mut();
    bytes[BITS_OFFSET + byte] = (bytes[BITS_OFFSET + byte] & !mask) | ((level as u8) << shift);
    Ok(())
}

/// 一个**位图区**（8 个位图页）与其管理的区号段。
#[derive(Debug)]
pub struct ExtentMap {
    pages: Vec<Page>,
    run_index: u8,
}

impl ExtentMap {
    /// 新建（`run_index` = 第几个位图区；`own_index` 基 = `run_index × 8`）。
    #[must_use]
    pub fn new(run_index: u8) -> Self {
        let base = u16::from(run_index) * BITMAP_PAGES_PER_RUN as u16;
        let pages = (0..BITMAP_PAGES_PER_RUN)
            .map(|i| {
                let mut p = Page::new(PageType::Bitmap, [0; 8], 1, 1 + i as u32);
                init(&mut p, BitmapKind::ExtentMap, base + i as u16).expect("位图页类型正确");
                p
            })
            .collect();
        Self { pages, run_index }
    }

    /// 位图区号。
    #[must_use]
    pub fn run_index(&self) -> u8 {
        self.run_index
    }

    /// 位图页。
    #[must_use]
    pub fn pages(&self) -> &[Page] {
        &self.pages
    }

    /// 本区管理的区数上限。
    #[must_use]
    pub fn capacity(&self) -> u32 {
        BITS_PER_RUN as u32
    }

    /// 分配一个区（**低位优先**）；满则 `None`。
    pub fn allocate(&mut self) -> Option<ExtentNo> {
        for (i, page) in self.pages.iter_mut().enumerate() {
            if let Some(bit) = find_free(page, 0) {
                set_allocated(page, bit).expect("位号在界内");
                // **不逐次 seal**：分配是批量行为，页写回时由 pagefile 统一
                // seal（校验和与页尾副本在写盘前落定）。
                let global = i as u32 * BITS_PER_BITMAP_PAGE as u32 + bit;
                return Some(ExtentNo::from_raw(
                    u32::from(self.run_index) * BITS_PER_RUN as u32 + global,
                ));
            }
        }
        None
    }

    /// 回收一个区。
    pub fn free(&mut self, extent: ExtentNo) -> Result<(), BitmapError> {
        let (page_index, bit) = self.locate(extent)?;
        clear_allocated(&mut self.pages[page_index], bit)?;
        Ok(())
    }

    /// 该区是否已分配。
    pub fn is_allocated(&self, extent: ExtentNo) -> Result<bool, BitmapError> {
        let (page_index, bit) = self.locate(extent)?;
        is_allocated(&self.pages[page_index], bit)
    }

    /// 本区已分配数。
    #[must_use]
    pub fn allocated(&self) -> u32 {
        self.pages.iter().map(allocated_count).sum()
    }

    fn locate(&self, extent: ExtentNo) -> Result<(usize, u32), BitmapError> {
        let base = u32::from(self.run_index) * BITS_PER_RUN as u32;
        let raw = extent.as_raw();
        if raw < base || raw >= base + BITS_PER_RUN as u32 {
            return Err(BitmapError::BitOutOfRange);
        }
        let local = raw - base;
        Ok((
            local as usize / BITS_PER_BITMAP_PAGE,
            local % BITS_PER_BITMAP_PAGE as u32,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::PageCheck;

    #[test]
    fn capacity_arithmetic_is_pinned() {
        // 规范里的三个数是算出来的（页头 68 + 页尾 4 + 页体头 8）——这里钉住。
        assert_eq!(BITS_BYTES, 16304);
        assert_eq!(BITS_PER_BITMAP_PAGE, 130_432);
        assert_eq!(BITS_PER_RUN, 1_043_456);
        assert_eq!(EXTENT_BLOCKS * 16 * 1024, 128 * 1024, "区 = 128 KB");
        // 4 TiB 文件需 33 个位图区（上限 40 留余量）。
        let extents_4tib = (4u64 << 40) / (128 * 1024);
        assert_eq!(extents_4tib, 33_554_432);
        assert_eq!(extents_4tib.div_ceil(BITS_PER_RUN as u64), 33);
        const { assert!(MAX_BITMAP_RUNS >= 33) };
    }

    #[test]
    fn init_and_own_index_self_check() {
        let mut page = Page::new(PageType::Bitmap, [1; 8], 1, 3);
        init(&mut page, BitmapKind::ExtentMap, 7).unwrap();
        assert_eq!(kind(&page).unwrap(), BitmapKind::ExtentMap);
        assert_eq!(own_index(&page).unwrap(), 7);
        verify_own_index(&page, 7).unwrap();
        assert_eq!(
            verify_own_index(&page, 8),
            Err(BitmapError::BadOwnIndex),
            "读错来源必须可检出"
        );
        assert_eq!(allocated_count(&page), 0);
        assert_eq!(page.verify(), PageCheck::Ok);
    }

    #[test]
    fn bit_operations_low_to_high() {
        let mut page = Page::new(PageType::Bitmap, [1; 8], 1, 3);
        init(&mut page, BitmapKind::ExtentMap, 0).unwrap();

        // 低位在前：分配 3 次 → 位 0/1/2。
        for expected in [0u32, 1, 2] {
            let bit = find_free(&page, 0).unwrap();
            assert_eq!(bit, expected);
            set_allocated(&mut page, bit).unwrap();
        }
        assert_eq!(allocated_count(&page), 3);
        assert!(is_allocated(&page, 0).unwrap() && is_allocated(&page, 2).unwrap());
        assert!(!is_allocated(&page, 3).unwrap());

        // 回收位 1 → 再次分配取到它。
        clear_allocated(&mut page, 1).unwrap();
        assert_eq!(find_free(&page, 0), Some(1));
        assert_eq!(find_free(&page, 2), Some(3), "from 之后的第一个空闲位");

        // 边界：最后一个位号。
        let last = BITS_PER_BITMAP_PAGE as u32 - 1;
        set_allocated(&mut page, last).unwrap();
        assert!(is_allocated(&page, last).unwrap());
        assert_eq!(
            set_allocated(&mut page, last + 1),
            Err(BitmapError::BitOutOfRange)
        );
    }

    #[test]
    fn wrong_kind_and_wrong_page_type_are_rejected() {
        let mut bitmap = Page::new(PageType::Bitmap, [1; 8], 1, 3);
        init(&mut bitmap, BitmapKind::FreeLevel, 0).unwrap();
        assert_eq!(is_allocated(&bitmap, 0), Err(BitmapError::WrongKind));
        assert_eq!(find_free(&bitmap, 0), None);

        let mut heap = Page::new(PageType::HeapTable, [1; 8], 1, 1);
        assert_eq!(
            init(&mut heap, BitmapKind::ExtentMap, 0),
            Err(BitmapError::NotABitmapPage)
        );
        assert_eq!(own_index(&heap), Err(BitmapError::NotABitmapPage));
    }

    #[test]
    fn free_level_two_bits_per_unit() {
        let mut page = Page::new(PageType::Bitmap, [1; 8], 1, 5);
        init(&mut page, BitmapKind::FreeLevel, 0).unwrap();
        for (unit, level) in [
            (0u32, FreeLevel::High),
            (1, FreeLevel::Full),
            (2, FreeLevel::Low),
            (3, FreeLevel::Medium),
        ] {
            set_free_level(&mut page, unit, level).unwrap();
        }
        assert_eq!(free_level(&page, 0).unwrap(), FreeLevel::High);
        assert_eq!(free_level(&page, 1).unwrap(), FreeLevel::Full);
        assert_eq!(free_level(&page, 2).unwrap(), FreeLevel::Low);
        assert_eq!(free_level(&page, 3).unwrap(), FreeLevel::Medium);
        // 2 位/单元：写第 1 单元不影响第 0 单元。
        set_free_level(&mut page, 1, FreeLevel::High).unwrap();
        assert_eq!(free_level(&page, 0).unwrap(), FreeLevel::High);
        assert_eq!(free_level(&page, 1).unwrap(), FreeLevel::High);
        // 越界（unit × 2 超位数）。
        assert_eq!(
            set_free_level(&mut page, (BITS_PER_BITMAP_PAGE / 2) as u32, FreeLevel::Low),
            Err(BitmapError::BitOutOfRange)
        );
    }

    #[test]
    fn extent_map_allocates_and_reuses() {
        let mut map = ExtentMap::new(0);
        assert_eq!(map.capacity(), 1_043_456);

        let first = map.allocate().unwrap();
        assert_eq!(first, ExtentNo::from_raw(0));
        assert_eq!(first.first_block(), 1, "区 0 从块 1 起");
        assert_eq!(first.blocks(), 8);
        assert_eq!(map.allocate().unwrap().as_raw(), 1, "低位优先");
        assert_eq!(map.allocated(), 2);

        // 回收后复用。
        let mid = ExtentNo::from_raw(0);
        map.free(mid).unwrap();
        assert!(!map.is_allocated(mid).unwrap());
        assert_eq!(map.allocate().unwrap(), mid, "低位优先复用空闲区");

        // 跨位图区的区号拒绝。
        assert_eq!(
            map.free(ExtentNo::from_raw(BITS_PER_RUN as u32)),
            Err(BitmapError::BitOutOfRange)
        );
        // 页自洽（写完由调用方 seal；这里手动收尾）。
        for p in &mut map.pages {
            p.seal();
            assert_eq!(p.verify(), PageCheck::Ok);
        }
    }

    #[test]
    fn extent_map_crosses_bitmap_pages_at_the_boundary() {
        let mut map = ExtentMap::new(0);
        // 直接把第 1 张位图页的位数组填满（构造边界，不做 13 万次分配）。
        {
            let page = &mut map.pages[0];
            page.as_bytes_mut()[BITS_OFFSET..BITS_OFFSET + BITS_BYTES].fill(0xFF);
        }
        // 第 1 页已满 → 分配到第 2 张位图页的第 0 位。
        let crossing = map.allocate().unwrap();
        assert_eq!(crossing.as_raw(), BITS_PER_BITMAP_PAGE as u32);
        assert_eq!(crossing.first_block(), 1 + BITS_PER_BITMAP_PAGE as u32 * 8);

        // 在第 1 页放出一个空位 → 再次分配取它（仍低位优先）。
        {
            let page = &mut map.pages[0];
            clear_allocated(page, 5).unwrap();
        }
        let reused = map.allocate().unwrap();
        assert_eq!(reused.as_raw(), 5);
    }

    #[test]
    fn run_index_offsets_extent_numbers() {
        let mut map = ExtentMap::new(1);
        let e = map.allocate().unwrap();
        assert_eq!(
            e.as_raw(),
            BITS_PER_RUN as u32,
            "第 1 个位图区从全局区号 1043456 起"
        );
        assert_eq!(own_index(&map.pages()[0]).unwrap(), 8, "own_index 基 = 1×8");
        assert!(map.free(ExtentNo::from_raw(0)).is_err(), "不属于本区");
    }
}
