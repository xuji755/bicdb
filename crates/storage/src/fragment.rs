//! 跨页行（行片段链）的写入与读取（存储架构 §6.3）。
//!
//! # 规则（§6.3 的三条触发与转换）
//!
//! - **插入**：编码后行长 **> 单页可用空间** → 直接成链（头片段在首个分配的页）；
//!   装得下就**不成链**（本模块自动走普通插入）；
//! - **切分按编码后的行字节流**（不按列对齐）：头片段 = 完整行头 +
//!   下一片段 ROWID 6B + 数据第 1 段；中/尾 = 短行头 10B + 数据段；
//!   **除末片外每片满装**至页可用空间；
//! - **列元数据只在头片段**（`col_count` / NULL 位图 / `var_offsets` 整行一次），
//!   中/尾片段的数据是**不透明字节切片**；
//! - **链上每个片段都是页里的一"行"，与普通行同规**——`row_len` = 片段自身长度；
//!   **整行长度不存、由链求和推导**（重组时回填）；
//! - 片段的**槽位状态 = 3**（§5.7 的"片段头"取值）：本实现让头/中/尾**都用 3**，
//!   "头/中"的区分靠**链图**（未被任何 next 引用的片段即头）——这样槽位扫描
//!   不会把短行头的片段误当完整行解析（该取值口径已记入待复核清单）。
//!
//! # 读取与防线
//!
//! 从头片段沿 `next` 收集数据段直到链尾，拼接后回填整行长度。
//! 三条把守：**环检测**（访问过的 ROWID 集合）、**总长有界**
//! （≤ `MAX_ROW_LEN`，且 ≤ 64 片）、**片段自洽**（每片 `row_len` == 自身字节数）。
//! 更新路径（就地转链 / 收缩回收）属 P3 事务域。

use std::collections::HashSet;

use crate::heap::{self, Heap, HeapError, InsertPolicy};
use crate::row::{
    self, FragmentView, HeadFragment, RowError, RowHeader, RowView, FRAGMENT_HEADER_LEN,
};
use crate::rowid::{RowId, ROWID_LEN};

/// 片段链操作错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FragmentError {
    /// 行字节流非法（行头 / 长度）。
    Row(RowError),
    /// 页操作失败（空间 / 槽位）。
    Heap(HeapError),
    /// 链上出现环（同一 ROWID 被访问两次）。
    LoopDetected,
    /// 链不完整或越界（指针指向不存在的片段 / 总长越界）。
    BrokenChain,
    /// **片段数超过链长上限**（§6.3：64 片）——在写入时报，不是读到一半才发现。
    /// 注意：本 API 无回滚，报错时**已写入的片段成为孤儿**（当前仅测试/工具
    /// 使用该入口；事务层的行迁移切片接入时须与页级 undo 一并处理）。
    TooManyFragments {
        /// 上限（片）。
        limit: usize,
    },
}

impl From<RowError> for FragmentError {
    fn from(e: RowError) -> Self {
        FragmentError::Row(e)
    }
}

impl From<HeapError> for FragmentError {
    fn from(e: HeapError) -> Self {
        FragmentError::Heap(e)
    }
}

impl std::fmt::Display for FragmentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FragmentError::Row(e) => write!(f, "片段行格式错误：{e}"),
            FragmentError::Heap(e) => write!(f, "片段页操作错误：{e}"),
            FragmentError::LoopDetected => f.write_str("片段链存在环"),
            FragmentError::BrokenChain => f.write_str("片段链不完整"),
            FragmentError::TooManyFragments { limit } => {
                write!(f, "片段链超过上限（{limit} 片，§6.3）")
            }
        }
    }
}

impl std::error::Error for FragmentError {}

/// **链长上限**（§6.3：64 个片段 ⇒ 整行上限约 1MB）。超限在写入时报错。
pub const MAX_FRAGMENTS: usize = 64;

/// 插入一条行字节流：**装得下一页就不成链**，否则拆为片段链。
///
/// 返回头片段的 `ROWID`——**稳定入口**（索引与引用只指它；§6.3）。
pub fn insert_row(
    heap: &mut Heap,
    encoded_row: &[u8],
    policy: &InsertPolicy,
) -> Result<RowId, FragmentError> {
    // **整行结构校验**（位图规范、头部区在行内——`data_start` 越过行尾
    // 在这里就被拒绝；只核 `row_len` 会在下面的 `[header_len..]` 越界 panic）。
    RowView::new(encoded_row)?;
    let header = RowHeader::read_from(encoded_row)?;
    if header.row_len as usize != encoded_row.len() {
        return Err(FragmentError::Row(RowError::LengthMismatch));
    }
    if heap.fits_one_page(encoded_row.len(), policy) {
        return Ok(heap.insert(encoded_row)?);
    }

    let header_len = header.data_start();
    let mut data: &[u8] = &encoded_row[header_len..];
    if data.is_empty() {
        return Err(FragmentError::BrokenChain);
    }

    // ---- 头片段：完整行头 + 6B 下一片段（占位）+ 数据第 1 段 ----
    let head_overhead = header_len + ROWID_LEN;
    let (page, cap) = pick_page(heap, head_overhead, policy)?;
    let take = cap.min(data.len());
    let mut head = Vec::with_capacity(head_overhead + take);
    head.extend_from_slice(&encoded_row[..header_len]);
    // §6.3：链上任一片段都置 FRAGMENT；链首另置 FRAGMENT_HEAD（保留位，见 row 模块）。
    head[0] |= row::row_flags::FRAGMENT | row::row_flags::FRAGMENT_HEAD;
    head.extend_from_slice(&[0u8; ROWID_LEN]);
    head.extend_from_slice(&data[..take]);
    data = &data[take..];
    set_own_row_len(&mut head);
    // 片段用槽位状态 3（§5.7；头/中/尾的区分靠链图——见模块文档）。
    let head_id =
        heap.insert_into_with_status(page, &head, policy, crate::page::SlotStatus::FragmentHead)?;

    // ---- 后续片段：中/尾（短行头），逐片写入并回填前一片的 next ----
    let mut prev = head_id;
    let mut prev_next_at = header_len; // 头片段的 next 在完整行头之后
    let mut fragments = 1usize; // 头片段计入链长
    while !data.is_empty() {
        // 链长上限（§6.3：64 片）——**写入时报**，不读到一半才发现。
        if fragments >= MAX_FRAGMENTS {
            return Err(FragmentError::TooManyFragments {
                limit: MAX_FRAGMENTS,
            });
        }
        fragments += 1;
        let (page, cap) = pick_page(heap, FRAGMENT_HEADER_LEN, policy)?;
        let take = cap.min(data.len());
        let mut frag = Vec::with_capacity(FRAGMENT_HEADER_LEN + take);
        frag.push(row::row_flags::FRAGMENT);
        frag.push(row::ITL_SLOT_NONE);
        let len = (FRAGMENT_HEADER_LEN + take) as u32;
        frag.extend_from_slice(&len.to_le_bytes());
        frag.extend_from_slice(&[0u8; ROWID_LEN]); // 下一片段占位
        frag.extend_from_slice(&data[..take]);
        data = &data[take..];
        let id = heap.insert_into_with_status(
            page,
            &frag,
            policy,
            crate::page::SlotStatus::FragmentHead,
        )?;
        heap.patch_record(prev, prev_next_at, &id.to_bytes())?;
        prev = id;
        prev_next_at = FRAGMENT_HEADER_LEN - ROWID_LEN;
    }
    Ok(head_id)
}

/// 读取一条行：头片段 → 沿链收集 → 拼接并**回填整行长度**。
pub fn read_row(heap: &Heap, head: RowId) -> Result<Vec<u8>, FragmentError> {
    let head_bytes = heap.get(head).ok_or(FragmentError::BrokenChain)?;
    if head_bytes[0] & row::row_flags::FRAGMENT_HEAD == 0 {
        return Err(FragmentError::BrokenChain); // 入口必须带链首位
    }
    let head_view = HeadFragment::new(head_bytes)?;
    let mut out = Vec::new();
    out.extend_from_slice(head_view.header_bytes());
    out.extend_from_slice(head_view.data());

    let mut visited = HashSet::new();
    visited.insert(head);
    let mut next = head_view.next();
    while let Some(id) = next {
        if !visited.insert(id) {
            return Err(FragmentError::LoopDetected);
        }
        let bytes = heap.get(id).ok_or(FragmentError::BrokenChain)?;
        let view = FragmentView::new(bytes)?;
        out.extend_from_slice(view.data());
        if out.len() > row::MAX_ROW_LEN {
            return Err(FragmentError::BrokenChain);
        }
        next = view.next();
    }
    let total = out.len() as u32;
    out[2..6].copy_from_slice(&total.to_le_bytes());
    out[0] &= !(row::row_flags::FRAGMENT | row::row_flags::FRAGMENT_HEAD); // 逻辑行不是片段
    Ok(out)
}

/// 选页：容量最大者；没有可用页则**新开一页**并取其容量。
/// 容量为 0（页装不下片头开销）→ `PageFull`。
fn pick_page(
    heap: &mut Heap,
    overhead: usize,
    policy: &InsertPolicy,
) -> Result<(u32, usize), FragmentError> {
    let mut best: Option<(u32, usize)> = None;
    for (block_id, page) in heap.pages_iter() {
        let cap = heap::capacity_for_row(page, policy).saturating_sub(overhead);
        if cap == 0 {
            continue;
        }
        if best.map_or(true, |(_, b)| cap > b) {
            best = Some((block_id, cap));
        }
    }
    if let Some((block_id, cap)) = best {
        return Ok((block_id, cap));
    }
    let block_id = heap.push_page();
    let cap = heap::capacity_for_row(heap.page(block_id).expect("刚创建"), policy)
        .saturating_sub(overhead);
    if cap == 0 {
        return Err(FragmentError::Heap(HeapError::PageFull));
    }
    Ok((block_id, cap))
}

/// 把片段记录的 `row_len` 改为它自身长度（头片段写完后调用）。
fn set_own_row_len(fragment: &mut [u8]) {
    let len = fragment.len() as u32;
    fragment[2..6].copy_from_slice(&len.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::{PageCheck, PAGE_SIZE};
    use crate::row::{assemble_row, row_flags};

    fn big_row(payload_len: usize) -> Vec<u8> {
        assemble_row(0, 0, &[false], &[], &[&vec![0x5A; payload_len]]).unwrap()
    }

    #[test]
    fn small_row_does_not_fragment() {
        let mut heap = Heap::new([1; 8], InsertPolicy::append_only());
        let row = big_row(100);
        let id = insert_row(&mut heap, &row, &InsertPolicy::append_only()).unwrap();
        assert_eq!(heap.get(id), Some(&row[..]), "直接以普通行存放");
        assert_eq!(heap.page_count(), 1);
    }

    #[test]
    fn nearly_one_mib_row_roundtrips_across_the_chain() {
        // **#38 定案（扩格式到 1 MiB）**：整行长度与变长偏移升为 4B。
        // 600 KiB 的行（远超旧 64 KiB 上限）经 64 片以内的片段链写入/重组，
        // 逐字节往返一致；行头的 `row_len` 在链上是"片段自身长度"、
        // 重组后回填为整行长度。
        let policy = InsertPolicy::append_only();
        let mut heap = Heap::new([8; 8], policy);
        let payload = big_row(600 * 1024);
        assert!(payload.len() > usize::from(u16::MAX), "超过旧 2B 上限");
        let id = insert_row(&mut heap, &payload, &policy).unwrap();
        let back = read_row(&heap, id).unwrap();
        assert_eq!(back.len(), payload.len());
        assert_eq!(back, payload, "600 KiB 行逐字节往返");
        let view = crate::row::RowView::new(&back).unwrap();
        assert_eq!(view.header().row_len as usize, payload.len());
        assert_eq!(view.var_column(0, 0).unwrap().len(), 600 * 1024);
    }

    #[test]
    fn row_over_the_hard_cap_is_rejected() {
        // 硬上限 `MAX_ROW_LEN` = 1 MiB（行层）——再大即 `TooLong`；
        // 而 1 MiB 以内的行即使超 64 片也只会得到 `TooManyFragments`
        // （两种错误都"写入时报"，不读到一半才发现）。
        assert!(matches!(
            assemble_row(0, 0, &[false], &[], &[&vec![0u8; row::MAX_ROW_LEN][..]]),
            Err(crate::row::RowError::TooLong { .. })
        ));
        let policy = InsertPolicy::append_only();
        let mut heap = Heap::new([9; 8], policy);
        // 约 1 MiB 的载荷：行层允许，但片数超 64 ⇒ 片段层拒绝。
        let payload = big_row(row::MAX_ROW_LEN - row::ROW_HEADER_FIXED_LEN - 8);
        assert!(matches!(
            insert_row(&mut heap, &payload, &policy),
            Err(FragmentError::TooManyFragments { .. })
        ));
    }

    #[test]
    fn cross_page_row_roundtrip() {
        let policy = InsertPolicy::append_only();
        let mut heap = Heap::new([2; 8], policy);
        let payload = big_row(PAGE_SIZE * 3 + 1234);
        let id = insert_row(&mut heap, &payload, &policy).unwrap();
        assert!(heap.page_count() >= 4, "片段跨多页");

        let head = heap.get(id).unwrap();
        let hv = HeadFragment::new(head).unwrap();
        assert_eq!(hv.header().flags & row_flags::FRAGMENT, row_flags::FRAGMENT);
        assert_eq!(
            hv.header().row_len as usize,
            head.len(),
            "片段 row_len = 自身长度"
        );

        let back = read_row(&heap, id).unwrap();
        assert_eq!(back, payload, "重组 = 原始行字节流");

        let mut n = 1;
        let mut cur = hv.next();
        while let Some(next) = cur {
            let bytes = heap.get(next).unwrap();
            let fv = FragmentView::new(bytes).unwrap();
            assert_eq!(fv.flags() & row_flags::FRAGMENT, row_flags::FRAGMENT);
            n += 1;
            cur = fv.next();
        }
        assert!(n >= 4, "片段数 = {n}");
        for (_, page) in heap.pages_iter() {
            assert_eq!(page.verify(), PageCheck::Ok, "写片段后页仍自洽");
        }
    }

    #[test]
    fn fragments_fill_pages_except_the_tail() {
        let policy = InsertPolicy::append_only();
        let mut heap = Heap::new([3; 8], policy);
        // 恰好两页数据：头片满装第 1 页 → 中片满装第 2 页 → 尾片落在第 3 页。
        let payload = big_row(PAGE_SIZE * 2);
        let id = insert_row(&mut heap, &payload, &policy).unwrap();
        assert_eq!(heap.page_count(), 3, "两页数据用三页：头/中/尾各一页");
        assert_eq!(read_row(&heap, id).unwrap(), payload);

        // 非末片所在页"无剩余可再放片段"（满装）。
        let head = heap.get(id).unwrap();
        let first_block = id.block_id();
        let head_page = heap.page(first_block).unwrap();
        let _ = head;
        assert_eq!(
            head_page.free_space(),
            0,
            "头页被头片段满装（剩余空间为 0）"
        );
    }

    #[test]
    fn cycle_is_detected() {
        let policy = InsertPolicy::append_only();
        let mut heap = Heap::new([4; 8], policy);
        let payload = big_row(PAGE_SIZE + 500);
        let id = insert_row(&mut heap, &payload, &policy).unwrap();

        // 人为制造环：把头片段的 next 指回自身。
        let head_len = HeadFragment::new(heap.get(id).unwrap())
            .unwrap()
            .header()
            .data_start();
        heap.patch_record(id, head_len, &id.to_bytes()).unwrap();
        assert_eq!(read_row(&heap, id).err(), Some(FragmentError::LoopDetected));
    }

    #[test]
    fn broken_chain_is_detected() {
        let policy = InsertPolicy::append_only();
        let mut heap = Heap::new([5; 8], policy);
        let payload = big_row(PAGE_SIZE + 500);
        let id = insert_row(&mut heap, &payload, &policy).unwrap();

        let bogus = RowId::from_parts(1, 999, 1).unwrap();
        let head_len = HeadFragment::new(heap.get(id).unwrap())
            .unwrap()
            .header()
            .data_start();
        heap.patch_record(id, head_len, &bogus.to_bytes()).unwrap();
        assert_eq!(read_row(&heap, id).err(), Some(FragmentError::BrokenChain));
    }

    #[test]
    fn malformed_row_is_rejected_before_slicing() {
        // 审核修复回归（E1）：`data_start` 越过行尾的行——旧代码在
        // `&encoded_row[header_len..]` 直接 panic；现按行格式错误拒绝。
        let mut heap = Heap::new([1; 8], InsertPolicy::in_place(0));
        let mut bad = crate::row::assemble_row(0, 1, &[false], &[], &[b"x".as_slice()]).unwrap();
        bad[8..10].copy_from_slice(&10u16.to_le_bytes()); // var_col_count=10 → data_start > 行尾
        let err = insert_row(&mut heap, &bad, &InsertPolicy::in_place(0)).unwrap_err();
        assert!(matches!(err, FragmentError::Row(_)), "{err}");
    }

    #[test]
    fn fragment_chain_length_is_capped_at_64() {
        // 审核修复回归（E3）：§6.3 的链长上限 64 片——**写入时报错**。
        // 用小页容量把大行拆出 64 片以上。
        let mut heap = Heap::new([2; 8], InsertPolicy::in_place(99));
        // pctfree 99 ⇒ 每页只容 ~80B 数据 ⇒ 60000B 的行需要数百片（> 64）。
        let payload = vec![0x5Au8; 60000];
        let big = crate::row::assemble_row(0, 1, &[false], &[], &[payload.as_slice()]).unwrap();
        let err = insert_row(&mut heap, &big, &InsertPolicy::in_place(99)).unwrap_err();
        assert!(
            matches!(err, FragmentError::TooManyFragments { limit: 64 }),
            "{err}"
        );
    }
}
