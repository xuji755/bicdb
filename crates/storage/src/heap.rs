//! 堆表页操作与内存堆表：插入 / 读取 / 删除 / 整理（defrag）
//! （存储架构 §5.7 槽位目录与空闲管理、§5.9 PCTFREE、§8.4 表选项）。
//!
//! # 页内模型
//!
//! ```text
//! [固定头][槽位目录 ↓]      [空闲区]      [行数据 ↑ 向低地址][页尾]
//!   free_start 可推导        两指针之差        free_end 为行区上界
//! ```
//!
//! - **插入**：行放在 `free_end − 行长` 处、`free_end` 下移；槽位取
//!   **最低的空闲槽**（`in_place` 策略——§6.4"槽位可复用"），否则追加新槽；
//! - **删除**：槽位置"空闲"（**空间不立即回收**——碎片留给 defrag）；
//! - **defrag**：把活动记录按槽位下标升序重排到页底，合并空闲区
//!   （§5.7"块内重组"；受 WAL 保护——写入路径由事务域接管）；
//! - **PCTFREE**（§5.9）：`可用空间 − 行长 ≥ PCTFREE% × 页大小` 才允许插入；
//!   **是下限不是上限**（删除/变短不让页变满，只阻止插入）。
//!
//! # 表选项
//!
//! `update_mode = append_only` 的表**共用同一页格式**（§8.4），只是策略不同：
//! 不复用空闲槽、无 PCTFREE 预留——由 [`InsertPolicy`] 表达。
//!
//! # 本切片的边界
//!
//! 内存堆表（`Heap`）用最简单的顺序分配页号（`file_id = 1`、`block_id = 1+i`）；
//! **段与区的分配（§4）、页分配位图（§3.2）、空闲级别（§4.5）**在后续切片接入；
//! 行的可见性/事务（ITL）与转发指针的跳转属 P3。

use crate::page::{Page, PageType, SlotEntry, SlotStatus, MAX_SLOTS, PAGE_SIZE};
use crate::row::{RowHeader, ROW_HEADER_FIXED_LEN};
use crate::rowid::RowId;

/// 插入策略（来自表选项：`update_mode` 与 `PCTFREE`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InsertPolicy {
    /// 是否复用空闲槽（`in_place` = 是；`append_only` = 否）。
    pub reuse_free_slots: bool,
    /// `PCTFREE` 百分比（0–99；`append_only` 无更新、取 0）。
    pub pctfree: u8,
}

impl InsertPolicy {
    /// `in_place`（普通关系表）：复用空闲槽 + PCTFREE 预留（默认 10）。
    #[must_use]
    pub fn in_place(pctfree: u8) -> Self {
        Self {
            reuse_free_slots: true,
            pctfree: pctfree.min(99),
        }
    }

    /// `append_only`（配置/会话/记忆表）：只追加，无预留。
    #[must_use]
    pub fn append_only() -> Self {
        Self {
            reuse_free_slots: false,
            pctfree: 0,
        }
    }

    /// PCTFREE 预留字节数（`ceil(pctfree% × 页大小)`）。
    #[must_use]
    pub fn reserved_bytes(&self) -> usize {
        (PAGE_SIZE * self.pctfree as usize).div_ceil(100)
    }
}

impl Default for InsertPolicy {
    fn default() -> Self {
        Self::in_place(10)
    }
}

/// 堆表页操作错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeapError {
    /// 页类型不是堆表页（堆表 / 临时 / Undo 之外不得当数据页使用）。
    NotADataPage,
    /// 页未完成初始化（`flags` 位 0 未置——未完成的页不得使用）。
    NotInitialized,
    /// 本页放不下（含 PCTFREE 预留判定）；换页或新页。
    PageFull,
    /// 槽位到顶（1023；与 `ROWID` 的 `row_id` 10 位耦合）。
    SlotLimit,
    /// 行号不存在（或该槽位空闲）。
    NoSuchRow,
    /// 行字节非法（不足行头 / `row_len` 与给定长度不符）。
    BadRow,
}

impl std::fmt::Display for HeapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            HeapError::NotADataPage => "页类型不是堆表页",
            HeapError::NotInitialized => "页未初始化完成",
            HeapError::PageFull => "本页放不下（含 PCTFREE 预留）",
            HeapError::SlotLimit => "槽位到顶（1023）",
            HeapError::NoSuchRow => "行号不存在",
            HeapError::BadRow => "行字节非法",
        })
    }
}

impl std::error::Error for HeapError {}

/// 该页上的行号 `row_no`（1 起）对应的槽位下标。
#[must_use]
pub fn slot_index(row_no: u16) -> Option<usize> {
    if row_no == 0 {
        None
    } else {
        Some(usize::from(row_no) - 1)
    }
}

fn require_data_page(page: &Page) -> Result<PageType, HeapError> {
    let header = page.header().ok_or(HeapError::NotADataPage)?;
    if !header.page_type.uses_slot_directory() {
        return Err(HeapError::NotADataPage);
    }
    if !page.is_initialized() {
        return Err(HeapError::NotInitialized);
    }
    Ok(header.page_type)
}

/// 可用空间（`free_end − free_start`；无冗余字段）。
#[must_use]
pub fn free_space(page: &Page) -> usize {
    page.free_space()
}

/// 本页能否容下 `row_len` 字节的行（含 PCTFREE 与**槽位目录**的记账）。
///
/// 空间账 = 行长 + PCTFREE 预留 + **新增槽位的 2 字节**（仅当需要新槽时；
/// 复用空闲槽不占目录空间）。
#[must_use]
pub fn can_insert(page: &Page, row_len: usize, policy: &InsertPolicy) -> bool {
    require_data_page(page).is_ok()
        && page.free_space() >= row_len + policy.reserved_bytes() + slot_cost(page, policy)
        && has_slot_room(page, policy)
}

/// 该页还能容纳的**行字节数**（含槽位目录与 PCTFREE 记账，与
/// [`can_insert`] 同一套账；片段链据此实现"满装"）。
#[must_use]
pub fn capacity_for_row(page: &Page, policy: &InsertPolicy) -> usize {
    if require_data_page(page).is_err() {
        return 0;
    }
    page.free_space()
        .saturating_sub(policy.reserved_bytes() + slot_cost(page, policy))
}

/// 本次插入将占用的槽位目录字节（复用空闲槽 = 0；新增槽 = 2）。
fn slot_cost(page: &Page, policy: &InsertPolicy) -> usize {
    if policy.reuse_free_slots && find_free_slot(page).is_some() {
        0
    } else {
        crate::page::SLOT_ENTRY_LEN
    }
}

fn has_slot_room(page: &Page, policy: &InsertPolicy) -> bool {
    let slots = page.slot_count() as usize;
    if slots < MAX_SLOTS {
        return true;
    }
    policy.reuse_free_slots && find_free_slot(page).is_some()
}

fn find_free_slot(page: &Page) -> Option<usize> {
    (0..page.slot_count() as usize).find(|&i| {
        page.slot(i)
            .map(|s| s.status() == SlotStatus::Free)
            .unwrap_or(false)
    })
}

/// 插入一条行字节（**已编码的完整行**，但不含任何页内包装）。
///
/// 返回行号（1 起；= 槽位下标 + 1）。页内字段已改（含 `free_end` 与槽位），
/// **调用方负责 `seal()`**（写回路径统一收尾）。
pub fn insert_row(page: &mut Page, row: &[u8], policy: &InsertPolicy) -> Result<u16, HeapError> {
    // **整行结构校验**（位图规范、头部区在行内）——只核 `row_len` 会放进
    // "写得进、读不回"的行（`RowView` 拒绝、`heap::row` 返回 None）。
    // 片段（`insert_record` 的其他状态）不走这里：它们不是完整逻辑行。
    crate::row::RowView::new(row).map_err(|_| HeapError::BadRow)?;
    insert_record(page, row, policy, SlotStatus::Normal)
}

/// 插入一条**记录**并指定槽位状态（片段 = [`SlotStatus::FragmentHead`]；
/// 转发指针由 P3 的更新路径使用 [`SlotStatus::Forwarding`]）。
///
/// 记录都必须自带行头（`row_len` 在偏移 2..4）——普通行与片段同规。
pub fn insert_record(
    page: &mut Page,
    row: &[u8],
    policy: &InsertPolicy,
    status: SlotStatus,
) -> Result<u16, HeapError> {
    if status == SlotStatus::Free {
        return Err(HeapError::BadRow);
    }
    require_data_page(page)?;
    if row.len() < ROW_HEADER_FIXED_LEN {
        return Err(HeapError::BadRow);
    }
    let declared = usize::from(
        RowHeader::read_from(row)
            .map_err(|_| HeapError::BadRow)?
            .row_len,
    );
    if declared != row.len() {
        return Err(HeapError::BadRow);
    }
    if !can_insert(page, row.len(), policy) {
        // 区分"槽位到顶"与"空间不足"，便于调用方决策。
        return Err(if !has_slot_room(page, policy) {
            HeapError::SlotLimit
        } else {
            HeapError::PageFull
        });
    }

    let index = if policy.reuse_free_slots {
        find_free_slot(page).unwrap_or(page.slot_count() as usize)
    } else {
        page.slot_count() as usize
    };

    // 行放在行区底部（free_end 下移）。
    let offset = page.free_end() - row.len();
    page.as_bytes_mut()[offset..offset + row.len()].copy_from_slice(row);
    page.set_free_end(offset);

    if index == page.slot_count() as usize {
        page.set_slot_count(page.slot_count() + 1)
            .map_err(|_| HeapError::SlotLimit)?;
    }
    let entry = SlotEntry::new(offset as u16, status).ok_or(HeapError::BadRow)?;
    page.set_slot(index, entry);
    u16::try_from(index + 1).map_err(|_| HeapError::SlotLimit)
}

/// 读取行字节；槽位空闲 / 越界 / 指针非法一律 `None`。
#[must_use]
pub fn row(page: &Page, row_no: u16) -> Option<&[u8]> {
    let index = slot_index(row_no)?;
    let slot = page.slot(index)?;
    if slot.status() == SlotStatus::Free {
        return None;
    }
    let start = usize::from(slot.offset());
    let bytes = page.as_bytes();
    let header = RowHeader::read_from(bytes.get(start..)?).ok()?;
    let len = usize::from(header.row_len);
    let end = start.checked_add(len)?;
    if end > PAGE_SIZE {
        return None;
    }
    bytes.get(start..end)
}

/// 槽位状态（越界 `None`）。
#[must_use]
pub fn slot_status(page: &Page, row_no: u16) -> Option<SlotStatus> {
    page.slot(slot_index(row_no)?).map(|s| s.status())
}

/// 删除：槽位置"空闲"（**空间不立即回收**——碎片留给 defrag）。
pub fn delete_row(page: &mut Page, row_no: u16) -> Result<(), HeapError> {
    require_data_page(page)?;
    let index = slot_index(row_no).ok_or(HeapError::NoSuchRow)?;
    let slot = page.slot(index).ok_or(HeapError::NoSuchRow)?;
    if slot.status() == SlotStatus::Free {
        return Err(HeapError::NoSuchRow);
    }
    // **保留下标偏移**（只翻状态位）：行字节留在原处——回滚"删除"这条 undo
    // 要按原位整行写回（§4.6.2）；偏移归零会让原位恢复无从落笔。
    let entry = SlotEntry::new(slot.offset(), SlotStatus::Free).ok_or(HeapError::BadRow)?;
    page.set_slot(index, entry);
    Ok(())
}

/// 页内整理：把活动记录按槽位下标升序重排到页底，合并空闲区。
///
/// 返回回收的字节数。转发指针与片段头等其他槽位状态**一并搬运**
/// （它们也是页里的记录）——但**按各自的长度形态**：普通行/片段头是
/// "行头 + 行体"（长度由 `row_len` 给出），**转发指针是裸的 6B 目标**
/// （没有行头——把它当带行头的记录读，会按垃圾 `row_len` 搬运或越界）。
pub fn defrag(page: &mut Page) -> Result<usize, HeapError> {
    require_data_page(page)?;
    let slots = page.slot_count() as usize;
    // 收集活动记录（槽位下标升序；先拷贝，避免覆盖）。
    let mut live: Vec<(usize, Vec<u8>)> = Vec::new();
    for i in 0..slots {
        let slot = page.slot(i).expect("槽位在界内");
        if slot.status() == SlotStatus::Free {
            continue;
        }
        let start = usize::from(slot.offset());
        let bytes = page.as_bytes();
        let len = match slot.status() {
            SlotStatus::Forwarding => crate::rowid::ROWID_LEN,
            _ => usize::from(
                RowHeader::read_from(bytes.get(start..).ok_or(HeapError::BadRow)?)
                    .map_err(|_| HeapError::BadRow)?
                    .row_len,
            ),
        };
        let end = start.checked_add(len).ok_or(HeapError::BadRow)?;
        if end > PAGE_SIZE {
            return Err(HeapError::BadRow);
        }
        live.push((i, bytes[start..end].to_vec()));
    }
    let before = page.free_space();
    let floor = page.row_area_floor();
    let mut cursor = floor;
    for (i, bytes) in &live {
        // 记录总量超出可用区（槽位重叠的损坏页）：拒绝，不越界写。
        cursor = cursor.checked_sub(bytes.len()).ok_or(HeapError::BadRow)?;
        page.as_bytes_mut()[cursor..cursor + bytes.len()].copy_from_slice(bytes);
        let entry = SlotEntry::new(cursor as u16, page.slot(*i).expect("槽位在界内").status())
            .ok_or(HeapError::BadRow)?;
        page.set_slot(*i, entry);
    }
    page.set_free_end(cursor);
    Ok(page.free_space().saturating_sub(before))
}

/// 内存堆表（多页；页号顺序分配——段/区管理在后续切片接入）。
#[derive(Debug)]
pub struct Heap {
    pages: Vec<Page>,
    policy: InsertPolicy,
    workspace_ref: [u8; 8],
}

impl Heap {
    /// 新建（给定工作区标识；策略来自表选项）。
    #[must_use]
    pub fn new(workspace_ref: [u8; 8], policy: InsertPolicy) -> Self {
        Self {
            pages: Vec::new(),
            policy,
            workspace_ref,
        }
    }

    /// 页数。
    #[must_use]
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// 插入：首个可容纳的页，否则新开一页。返回完整 `ROWID`。
    pub fn insert(&mut self, row: &[u8]) -> Result<RowId, HeapError> {
        for (i, page) in self.pages.iter_mut().enumerate() {
            if can_insert(page, row.len(), &self.policy) {
                let row_no = insert_row(page, row, &self.policy)?;
                page.seal();
                return RowId::from_parts(1, (i + 1) as u32, row_no)
                    .map_err(|_| HeapError::SlotLimit);
            }
        }
        let block_id = (self.pages.len() + 1) as u32;
        let mut page = Page::new(PageType::HeapTable, self.workspace_ref, 1, block_id);
        let row_no = insert_row(&mut page, row, &self.policy)?;
        page.seal();
        self.pages.push(page);
        RowId::from_parts(1, block_id, row_no).map_err(|_| HeapError::SlotLimit)
    }

    /// 工作区受校验标识（页头字段）。
    #[must_use]
    pub fn workspace_ref(&self) -> [u8; 8] {
        self.workspace_ref
    }

    /// 遍历页（块号, 页引用）。
    pub fn pages_iter(&self) -> impl Iterator<Item = (u32, &Page)> {
        self.pages
            .iter()
            .enumerate()
            .map(|(i, p)| ((i + 1) as u32, p))
    }

    /// 页引用（块号从 1 起；块 0 留文件头页）。
    #[must_use]
    pub fn page(&self, block_id: u32) -> Option<&Page> {
        if block_id == 0 {
            return None;
        }
        self.pages.get(block_id as usize - 1)
    }

    /// 追加一张空页并返回块号（段/区分配接入前的顺序分配）。
    pub fn push_page(&mut self) -> u32 {
        let block_id = (self.pages.len() + 1) as u32;
        self.pages.push(Page::new(
            PageType::HeapTable,
            self.workspace_ref,
            1,
            block_id,
        ));
        block_id
    }

    /// 指定页插入（调用方已选定目标页；片段链用它实现"满装"策略）。
    pub fn insert_into(
        &mut self,
        block_id: u32,
        row: &[u8],
        policy: &InsertPolicy,
    ) -> Result<RowId, HeapError> {
        self.insert_into_with_status(block_id, row, policy, SlotStatus::Normal)
    }

    /// 指定页插入并指定槽位状态（片段 = [`SlotStatus::FragmentHead`]）。
    pub fn insert_into_with_status(
        &mut self,
        block_id: u32,
        row: &[u8],
        policy: &InsertPolicy,
        status: SlotStatus,
    ) -> Result<RowId, HeapError> {
        let page = self
            .pages
            .get_mut(block_id as usize - 1)
            .ok_or(HeapError::NoSuchRow)?;
        let row_no = insert_record(page, row, policy, status)?;
        page.seal();
        RowId::from_parts(1, block_id, row_no).map_err(|_| HeapError::SlotLimit)
    }

    /// **就地改写某条记录内的一段字节**（片段链回填 next 指针用）。
    ///
    /// `offset` 相对记录起点；越界拒绝。改写后重新 `seal`。
    pub fn patch_record(
        &mut self,
        id: RowId,
        offset: usize,
        bytes: &[u8],
    ) -> Result<(), HeapError> {
        if id.file_id() != 1 {
            return Err(HeapError::NoSuchRow);
        }
        let page = self
            .pages
            .get_mut(id.block_id() as usize - 1)
            .ok_or(HeapError::NoSuchRow)?;
        let record = row(page, id.row_id()).ok_or(HeapError::NoSuchRow)?;
        if offset + bytes.len() > record.len() {
            return Err(HeapError::BadRow);
        }
        let start = usize::from(
            page.slot(slot_index(id.row_id()).unwrap())
                .unwrap()
                .offset(),
        ) + offset;
        page.as_bytes_mut()[start..start + bytes.len()].copy_from_slice(bytes);
        page.seal();
        Ok(())
    }

    /// 行能否**不经片段链**落在某一页（含"新开一页"的情形）。
    #[must_use]
    pub fn fits_one_page(&self, row_len: usize, policy: &InsertPolicy) -> bool {
        if self.pages.iter().any(|p| can_insert(p, row_len, policy)) {
            return true;
        }
        let fresh = Page::new(
            PageType::HeapTable,
            self.workspace_ref,
            1,
            (self.pages.len() + 1) as u32,
        );
        can_insert(&fresh, row_len, policy)
    }

    /// 按 `ROWID` 读取行字节（本切片不跟随转发指针——P3 的更新路径接入）。
    #[must_use]
    pub fn get(&self, id: RowId) -> Option<&[u8]> {
        if id.file_id() != 1 {
            return None;
        }
        let page = self.pages.get(id.block_id() as usize - 1)?;
        row(page, id.row_id())
    }

    /// 删除（槽位标空闲）。
    pub fn delete(&mut self, id: RowId) -> Result<(), HeapError> {
        if id.file_id() != 1 {
            return Err(HeapError::NoSuchRow);
        }
        let page = self
            .pages
            .get_mut(id.block_id() as usize - 1)
            .ok_or(HeapError::NoSuchRow)?;
        delete_row(page, id.row_id())?;
        page.seal();
        Ok(())
    }

    /// 对指定页做整理（回收碎片空间）。
    pub fn defrag_page(&mut self, block_id: u32) -> Result<usize, HeapError> {
        let page = self
            .pages
            .get_mut(block_id as usize - 1)
            .ok_or(HeapError::NoSuchRow)?;
        let reclaimed = defrag(page)?;
        page.seal();
        Ok(reclaimed)
    }

    /// 遍历全部活动行（按页号、行号升序）。
    pub fn iter_rows(&self) -> impl Iterator<Item = (RowId, &[u8])> {
        self.pages.iter().enumerate().flat_map(|(i, page)| {
            (1..=page.slot_count()).filter_map(move |row_no| {
                let bytes = row(page, row_no)?;
                let id = RowId::from_parts(1, (i + 1) as u32, row_no).ok()?;
                Some((id, bytes))
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::{PageCheck, SLOT_ENTRY_LEN};
    use crate::row::assemble_row;

    fn tiny_row(payload: &[u8]) -> Vec<u8> {
        // 1 列（变长）：行头 10B + 位图 1B + 偏移 2B + 数据。
        assemble_row(0, 0, &[false], &[], &[payload]).unwrap()
    }

    #[test]
    fn insert_read_delete_roundtrip() {
        let mut page = Page::new(PageType::HeapTable, [7; 8], 1, 1);
        let r1 = tiny_row(b"alpha");
        let r2 = tiny_row(b"beta");
        let n1 = insert_row(&mut page, &r1, &InsertPolicy::in_place(0)).unwrap();
        let n2 = insert_row(&mut page, &r2, &InsertPolicy::in_place(0)).unwrap();
        assert_eq!((n1, n2), (1, 2), "行号 1 起");
        assert_eq!(row(&page, 1), Some(&r1[..]));
        assert_eq!(row(&page, 2), Some(&r2[..]));
        assert_eq!(row(&page, 3), None);

        // 行区从页底向上：第一行的偏移更高。
        let o1 = page.slot(0).unwrap().offset();
        let o2 = page.slot(1).unwrap().offset();
        assert!(o1 > o2, "后插的行在更低地址");

        // 删除后不可读。
        delete_row(&mut page, 1).unwrap();
        assert_eq!(row(&page, 1), None);
        assert_eq!(slot_status(&page, 1), Some(SlotStatus::Free));
        assert!(delete_row(&mut page, 1).is_err(), "重复删除");

        // 收尾一致性。
        page.seal();
        assert_eq!(page.verify(), PageCheck::Ok);
    }

    #[test]
    fn in_place_reuses_free_slot_but_append_only_does_not() {
        let mut page = Page::new(PageType::HeapTable, [7; 8], 1, 1);
        for p in [b"a".as_slice(), b"b", b"c"] {
            insert_row(&mut page, &tiny_row(p), &InsertPolicy::in_place(0)).unwrap();
        }
        delete_row(&mut page, 2).unwrap();
        // in_place：复用槽 2。
        let n = insert_row(&mut page, &tiny_row(b"d"), &InsertPolicy::in_place(0)).unwrap();
        assert_eq!(n, 2, "空闲槽复用（§6.4：旧 ROWID 指向新行）");
        assert_eq!(page.slot_count(), 3, "不新增槽位");

        // append_only：不复用，追加新槽。
        let mut page2 = Page::new(PageType::HeapTable, [7; 8], 1, 1);
        for p in [b"a".as_slice(), b"b", b"c"] {
            insert_row(&mut page2, &tiny_row(p), &InsertPolicy::append_only()).unwrap();
        }
        delete_row(&mut page2, 2).unwrap();
        let n = insert_row(&mut page2, &tiny_row(b"d"), &InsertPolicy::append_only()).unwrap();
        assert_eq!(n, 4);
        assert_eq!(page2.slot_count(), 4);
        assert_eq!(row(&page2, 2), None, "空闲槽保持空闲");
    }

    #[test]
    fn pctfree_is_a_floor_for_inserts() {
        let mut page = Page::new(PageType::HeapTable, [1; 8], 1, 1);
        let free = page.free_space();
        let reserve = InsertPolicy::in_place(10).reserved_bytes();
        // 行开销 = 行头 10 + 位图 1 + 偏移 2 = 13；槽位另占 2。
        // 取一行使其"无预留下可容纳、10% 预留下放不下"。
        let payload_len = free - reserve - SLOT_ENTRY_LEN - 13 + 1;
        debug_assert!(13 + payload_len + SLOT_ENTRY_LEN + reserve > free);
        let big = tiny_row(&vec![0u8; payload_len]);
        assert_eq!(big.len(), 13 + payload_len);

        assert!(can_insert(&page, big.len(), &InsertPolicy::in_place(0)));
        assert!(!can_insert(&page, big.len(), &InsertPolicy::in_place(10)));
        assert_eq!(
            insert_row(&mut page, &big, &InsertPolicy::in_place(10)).unwrap_err(),
            HeapError::PageFull,
            "PCTFREE 只阻止插入"
        );

        // 无预留下插入成功；空间记账 = 行长 + 槽位目录 2 字节。
        insert_row(&mut page, &big, &InsertPolicy::in_place(0)).unwrap();
        assert_eq!(page.free_space(), free - big.len() - SLOT_ENTRY_LEN);
        // 页还可以变得更空（PCTFREE 是下限不是上限）：删除不改变空闲记账。
        let before_delete = page.free_space();
        delete_row(&mut page, 1).unwrap();
        assert_eq!(page.free_space(), before_delete, "删除的空间留给 defrag");
    }

    #[test]
    fn slot_limit_is_enforced() {
        let mut page = Page::new(PageType::HeapTable, [1; 8], 1, 1);
        let policy = InsertPolicy::append_only();
        let row = assemble_row(0, 0, &[], &[], &[]).unwrap(); // 10B 最小行
        for _ in 0..MAX_SLOTS {
            insert_row(&mut page, &row, &policy).expect("1023 槽内应可插入");
        }
        assert_eq!(page.slot_count() as usize, MAX_SLOTS);
        assert_eq!(
            insert_row(&mut page, &row, &policy).unwrap_err(),
            HeapError::SlotLimit
        );
    }

    #[test]
    fn defrag_compacts_live_rows_and_reclaims_space() {
        let mut page = Page::new(PageType::HeapTable, [1; 8], 1, 1);
        let policy = InsertPolicy::append_only(); // 不复用槽：制造碎片
        let rows: Vec<Vec<u8>> = (0..6u8).map(|i| tiny_row(&[b'a' + i; 64])).collect();
        for r in &rows {
            insert_row(&mut page, r, &policy).unwrap();
        }
        for i in [2u16, 4] {
            delete_row(&mut page, i).unwrap();
        }
        let before = page.free_space();
        let reclaimed = defrag(&mut page).unwrap();
        assert!(reclaimed > 0, "删除产生的碎片应被回收");
        assert_eq!(page.free_space(), before + reclaimed);
        // 活动行仍可读、内容不变。
        for (i, r) in rows.iter().enumerate() {
            let row_no = i as u16 + 1;
            if row_no == 2 || row_no == 4 {
                assert_eq!(row(&page, row_no), None);
            } else {
                assert_eq!(row(&page, row_no), Some(&r[..]), "行 {row_no} 内容不变");
            }
        }
        // 紧凑性：活动记录紧密排列在页底。
        let live_offsets: Vec<u16> = (1..=6)
            .filter_map(|n| page.slot(slot_index(n).unwrap()))
            .filter(|s| s.status() != SlotStatus::Free)
            .map(|s| s.offset())
            .collect();
        let total: usize = 4 * rows[0].len();
        assert_eq!(page.free_end(), page.row_area_floor() - total);
        assert!(live_offsets.iter().all(|&o| o >= page.free_end() as u16));
        page.seal();
        assert_eq!(page.verify(), PageCheck::Ok);
    }

    #[test]
    fn heap_spans_pages_and_addresses_rows() {
        let mut heap = Heap::new([9; 8], InsertPolicy::in_place(0));
        let big = tiny_row(&vec![7u8; PAGE_SIZE / 2]);
        let mut ids = Vec::new();
        for _ in 0..5 {
            ids.push(heap.insert(&big).unwrap());
        }
        assert!(heap.page_count() >= 3, "半页行：每页 1–2 行");
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(heap.get(*id), Some(&big[..]), "第 {i} 行可读");
        }
        assert_eq!(heap.iter_rows().count(), 5);
        // 删除后不可读，其余不受影响。
        heap.delete(ids[2]).unwrap();
        assert_eq!(heap.get(ids[2]), None);
        assert_eq!(heap.get(ids[3]), Some(&big[..]));
        assert_eq!(heap.iter_rows().count(), 4);
        // 页号从 1 起（块 0 留文件头页）。
        assert_eq!(ids[0].block_id(), 1);
        assert_eq!(ids[0].file_id(), 1);
    }

    #[test]
    fn non_data_page_is_rejected() {
        let mut page = Page::new(PageType::Bitmap, [1; 8], 1, 1);
        let row = assemble_row(0, 0, &[], &[], &[]).unwrap();
        assert_eq!(
            insert_row(&mut page, &row, &InsertPolicy::append_only()).unwrap_err(),
            HeapError::NotADataPage
        );
        assert!(!can_insert(&page, row.len(), &InsertPolicy::append_only()));
    }

    #[test]
    fn bad_row_bytes_are_rejected() {
        let mut page = Page::new(PageType::HeapTable, [1; 8], 1, 1);
        assert_eq!(
            insert_row(&mut page, &[0u8; 4], &InsertPolicy::append_only()).unwrap_err(),
            HeapError::BadRow,
            "不足行头"
        );
        let mut row = assemble_row(0, 0, &[], &[], &[]).unwrap();
        row[2..4].copy_from_slice(&99u16.to_le_bytes());
        assert_eq!(
            insert_row(&mut page, &row, &InsertPolicy::append_only()).unwrap_err(),
            HeapError::BadRow,
            "row_len 与字节数不符"
        );
    }

    #[test]
    fn defrag_moves_forwarding_pointers_as_six_bytes() {
        // 审核修复回归（E2）：转发指针是**裸 6B 目标**（没有行头）——defrag
        // 按状态区分长度搬运，不得把它当带行头的记录（会丢指针/越界 panic）。
        let mut page = Page::new(crate::page::PageType::HeapTable, [0u8; 8], 3, 1);
        // 两行 + 一个转发指针。
        let bytes = crate::row::assemble_row(0, 1, &[false], &[], &[b"keep".as_slice()]).unwrap();
        let n1 = insert_row(&mut page, &bytes, &InsertPolicy::in_place(0)).unwrap();
        let n2 = insert_row(&mut page, &bytes, &InsertPolicy::in_place(0)).unwrap();
        let target = crate::rowid::RowId::from_parts(3, 9, 5).unwrap();
        // 槽 3 = 转发指针（手工置入：6B 目标、状态 Forwarding）。
        let offset = page.free_end() - crate::rowid::ROWID_LEN;
        page.as_bytes_mut()[offset..offset + crate::rowid::ROWID_LEN]
            .copy_from_slice(&target.to_bytes());
        page.set_free_end(offset);
        page.set_slot_count(3).unwrap();
        page.set_slot(
            2,
            SlotEntry::new(offset as u16, SlotStatus::Forwarding).unwrap(),
        );

        defrag(&mut page).unwrap();

        // 三者的槽偏移都被重排，但**内容与状态不变**：行可读、指针目标原样。
        assert_eq!(row(&page, n1), Some(&bytes[..]));
        assert_eq!(row(&page, n2), Some(&bytes[..]));
        let slot = page.slot(2).unwrap();
        assert_eq!(slot.status(), SlotStatus::Forwarding);
        let at = usize::from(slot.offset());
        assert_eq!(
            &page.as_bytes()[at..at + crate::rowid::ROWID_LEN],
            &target.to_bytes()
        );
    }
}
