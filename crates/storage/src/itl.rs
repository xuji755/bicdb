//! ITL（事务槽）：页内的事务条目——**行锁的承载**与**可见性判定的自包含**（§5.4）。
//!
//! ```text
//! 页头偏移 40 起，每条 24B：
//!   txn_id     6B   三段式（usn 8 │ slot 8 │ wrap 32）——**直接寻址**事务表槽
//!   undo_ptr   6B   与 ROWID 同构（file_id + block_id + 页内槽号）——该事务在本块
//!                  最后一次修改的 undo 链入口
//!   commit_seq 6B   提交序号（未提交为 0/None）
//!   lock_cnt   2B   本事务在本块锁定的行数——槽回收条件
//!   flags      1B   空闲 / 活动 / 已提交 / 已回滚
//!   保留       3B
//! ```
//!
//! **行锁就是 ITL 槽占用**：判断"这行被锁了吗"＝读其 `itl_slot` 所指条目的
//! `txn_id`；"是不是我自己锁的"＝比对 `txn_id`（含 `wrap`）。**读路径不加锁**：
//! 只读 ITL 条目判可见性，不获取、不等待（REQ-TXN-005）。
//!
//! **槽的可复用**（§5.4.1）：`flags != Active` 且 `lock_cnt = 0`——覆盖一个槽
//! 的动作本身受 undo 保护（§4.6.2 的 `ITL 覆盖`），故**不必等旧查询结束**；
//! 但**必须先被清除**（`flags` 不再显示活动），否则那个事务的提交序号取不到。
//! 无空槽时按 `itl_max`（**段级属性**）动态扩展；达上限仍无槽 ⇒ 明确报错由
//! 调用方等待（**不静默失败**）。

use bicdb_common::seq::CommitSeq;

use crate::page::{itl_entry_offset, Page, ITL_ENTRY_LEN, SLOT_ENTRY_LEN};
use crate::rowid::RowId;
use crate::undo::TxnId;

/// ITL 槽上限（§5.4.1：页头预算——32 槽 = 812B ≈ 页的 5%）。
pub const ITL_MAX_LIMIT: u16 = 32;

/// ITL 状态（`flags` 1B）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItlState {
    /// 0：空闲。
    Free = 0,
    /// 1：活动（**同时涵盖"已提交但未清除"**——见 §11.1.1 的延迟块清除）。
    Active = 1,
    /// 2：已提交（`commit_seq` 已准确落位）。
    Committed = 2,
    /// 3：已回滚。
    RolledBack = 3,
}

impl ItlState {
    /// 由字节解码。
    #[must_use]
    pub const fn from_u8(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(Self::Free),
            1 => Some(Self::Active),
            2 => Some(Self::Committed),
            3 => Some(Self::RolledBack),
            _ => None,
        }
    }

    /// 字节值。
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// 一条 ITL 条目的值形态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ItlEntry {
    /// 占用者。**是否"空"由 `state` 表达**——`txn_id = 0`（usn 0/slot 0/wrap 0）
    /// 是合法身份（第一个事务），不能当哨兵用。
    pub txn_id: TxnId,
    /// undo 链入口（`None` = 本事务尚未在本块留下 undo）。
    pub undo_ptr: Option<RowId>,
    /// 提交序号（`None` = 未提交）。
    pub commit_seq: Option<CommitSeq>,
    /// 本事务在本块锁定的行数。
    pub lock_cnt: u16,
    /// 状态。
    pub state: ItlState,
}

impl ItlEntry {
    /// 空闲条目。
    pub const FREE: ItlEntry = ItlEntry {
        txn_id: TxnId::from_parts(0, 0, 0),
        undo_ptr: None,
        commit_seq: None,
        lock_cnt: 0,
        state: ItlState::Free,
    };

    /// 编码到 24B。
    pub fn encode(&self, out: &mut [u8]) {
        debug_assert_eq!(out.len(), ITL_ENTRY_LEN);
        out.fill(0);
        out[0..6].copy_from_slice(&self.txn_id.to_bytes());
        if let Some(ptr) = self.undo_ptr {
            out[6..12].copy_from_slice(&ptr.to_bytes());
        }
        if let Some(seq) = self.commit_seq {
            out[12..18].copy_from_slice(&seq.as_raw().to_le_bytes()[..6]);
        }
        out[18..20].copy_from_slice(&self.lock_cnt.to_le_bytes());
        out[20] = self.state.as_u8();
    }

    /// 由 24B 解码。
    pub fn decode(b: &[u8]) -> Result<Self, ItlError> {
        debug_assert_eq!(b.len(), ITL_ENTRY_LEN);
        let mut id = [0u8; 6];
        id.copy_from_slice(&b[0..6]);
        let mut ptr = [0u8; 6];
        ptr.copy_from_slice(&b[6..12]);
        let mut seq = [0u8; 8];
        seq[..6].copy_from_slice(&b[12..18]);
        let state = ItlState::from_u8(b[20]).ok_or(ItlError::Malformed)?;
        let raw_seq = u64::from_le_bytes(seq);
        let commit_seq = CommitSeq::from_raw(raw_seq)
            .filter(|_| state == ItlState::Committed)
            .or(if state == ItlState::Committed {
                Some(CommitSeq::from_raw(0).expect("0 在 48 位域内"))
            } else {
                None
            });
        Ok(Self {
            txn_id: TxnId::from_bytes(&id),
            undo_ptr: (ptr != [0u8; 6]).then(|| RowId::from_bytes(&ptr)),
            commit_seq,
            lock_cnt: u16::from_le_bytes(b[18..20].try_into().expect("2 字节")),
            state,
        })
    }

    /// **可复用**（§5.4.1）：非活动且无行锁。
    #[must_use]
    pub fn reusable(&self) -> bool {
        self.state != ItlState::Active && self.lock_cnt == 0
    }
}

/// ITL 操作错误。
#[derive(Debug, PartialEq, Eq)]
pub enum ItlError {
    /// 槽号越出当前 `itl_count`（**扩展必须走 [`grow`]**）。
    SlotOutOfRange(u16),
    /// 页不是可用页（页头缺失/字段越界）。
    Malformed,
    /// 已达 `itl_max` 且无空槽——**该事务等待**（不得静默失败）。
    NoSlotAvailable,
    /// 扩展会把空闲区挤穿（`free_start` 越过 `free_end`）。
    NotEnoughSpace,
}

impl std::fmt::Display for ItlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ItlError::SlotOutOfRange(i) => write!(f, "ITL 槽 {i} 越出当前 itl_count"),
            ItlError::Malformed => f.write_str("页头/ITL 字段越界——按损坏处理"),
            ItlError::NoSlotAvailable => f.write_str("无可用 ITL 槽（已达 itl_max）——应等待"),
            ItlError::NotEnoughSpace => f.write_str("扩展 ITL 会挤穿页内空闲区"),
        }
    }
}

impl std::error::Error for ItlError {}

/// 当前 `itl_count`（页头事务区）。
pub fn itl_count(page: &Page) -> Result<u16, ItlError> {
    let header = page.header().ok_or(ItlError::Malformed)?;
    Ok(header.itl_count.max(1))
}

/// 读一条 ITL（槽号须在当前 `itl_count` 内）。
pub fn read_itl(page: &Page, index: u16) -> Result<ItlEntry, ItlError> {
    if index >= itl_count(page)? {
        return Err(ItlError::SlotOutOfRange(index));
    }
    let at = itl_entry_offset(index);
    ItlEntry::decode(&page.as_bytes()[at..at + ITL_ENTRY_LEN])
}

/// 写一条 ITL（槽号须在当前 `itl_count` 内——**扩展走 [`grow`]**）。
pub fn write_itl(page: &mut Page, index: u16, entry: &ItlEntry) -> Result<(), ItlError> {
    if index >= itl_count(page)? {
        return Err(ItlError::SlotOutOfRange(index));
    }
    let at = itl_entry_offset(index);
    entry.encode(&mut page.as_bytes_mut()[at..at + ITL_ENTRY_LEN]);
    Ok(())
}

/// 取一条 ITL 的**原始 24B**（`ITL 覆盖` undo 的旧值快照，§4.6.2）。
pub fn snapshot(page: &Page, index: u16) -> Result<[u8; ITL_ENTRY_LEN], ItlError> {
    if index >= itl_count(page)? {
        return Err(ItlError::SlotOutOfRange(index));
    }
    let at = itl_entry_offset(index);
    let mut out = [0u8; ITL_ENTRY_LEN];
    out.copy_from_slice(&page.as_bytes()[at..at + ITL_ENTRY_LEN]);
    Ok(out)
}

/// 恢复一条 ITL 的原始 24B（回滚 `ITL 覆盖`）。
pub fn restore(page: &mut Page, index: u16, bytes: &[u8; ITL_ENTRY_LEN]) -> Result<(), ItlError> {
    if index >= itl_count(page)? {
        return Err(ItlError::SlotOutOfRange(index));
    }
    let at = itl_entry_offset(index);
    page.as_bytes_mut()[at..at + ITL_ENTRY_LEN].copy_from_slice(bytes);
    Ok(())
}

/// **动态扩展一个 ITL 槽**（`itl_count + 1`；受 `itl_max` 与页内空闲区约束）。
///
/// 返回新槽的槽号。扩展使页头后移 24B——`free_start = 固定头末尾 +
/// 槽位目录`，故必须保证不挤穿 `free_end`。
pub fn grow(page: &mut Page, itl_max: u16) -> Result<u16, ItlError> {
    let mut header = page.header().ok_or(ItlError::Malformed)?;
    let count = header.itl_count.max(1);
    if count >= itl_max {
        return Err(ItlError::NoSlotAvailable);
    }
    // 新固定头末尾 = 68 + (count+1−1)×24；其后还有槽位目录 2B×slot_count。
    let new_fixed_end = crate::page::FIXED_HEADER_LEN + (usize::from(count)) * ITL_ENTRY_LEN;
    let need = new_fixed_end + usize::from(header.slot_count) * SLOT_ENTRY_LEN;
    if need > usize::from(header.free_end) {
        return Err(ItlError::NotEnoughSpace);
    }
    header.itl_count = count + 1;
    page.write_header(&header);
    // 新槽清零（空闲）。
    let at = itl_entry_offset(count);
    page.as_bytes_mut()[at..at + ITL_ENTRY_LEN].fill(0);
    Ok(count)
}

/// 找一个**可复用**槽（§5.4.1 的判据）；找不到返回 `None`。
pub fn find_reusable(page: &Page) -> Result<Option<u16>, ItlError> {
    for index in 0..itl_count(page)? {
        if read_itl(page, index)?.reusable() {
            return Ok(Some(index));
        }
    }
    Ok(None)
}

/// **占用一个 ITL 槽**：优先复用，其次扩展；都不可 ⇒ [`ItlError::NoSlotAvailable`]
/// （调用方**等待**，不得静默失败）。占用后槽为 `Active`、`txn_id` 落位、计数清零。
pub fn acquire(page: &mut Page, itl_max: u16, txn_id: TxnId) -> Result<u16, ItlError> {
    let index = match find_reusable(page)? {
        Some(i) => i,
        None => grow(page, itl_max)?,
    };
    write_itl(
        page,
        index,
        &ItlEntry {
            txn_id,
            undo_ptr: None,
            commit_seq: None,
            lock_cnt: 0,
            state: ItlState::Active,
        },
    )?;
    Ok(index)
}

/// 设置某槽的 `undo_ptr`（该事务在本块留下 undo 链入口）。
pub fn set_undo_ptr(page: &mut Page, index: u16, ptr: RowId) -> Result<(), ItlError> {
    let mut entry = read_itl(page, index)?;
    entry.undo_ptr = Some(ptr);
    write_itl(page, index, &entry)
}

/// **标记已提交**（延迟块清除的落点：把准确的 `commit_seq` 写进槽）。
pub fn mark_committed(page: &mut Page, index: u16, seq: CommitSeq) -> Result<(), ItlError> {
    let mut entry = read_itl(page, index)?;
    entry.state = ItlState::Committed;
    entry.commit_seq = Some(seq);
    write_itl(page, index, &entry)
}

/// 标记已回滚。
pub fn mark_rolled_back(page: &mut Page, index: u16) -> Result<(), ItlError> {
    let mut entry = read_itl(page, index)?;
    entry.state = ItlState::RolledBack;
    entry.commit_seq = None;
    write_itl(page, index, &entry)
}

/// 行锁计数 +1（占用该槽后锁定一行）。
pub fn lock(page: &mut Page, index: u16) -> Result<(), ItlError> {
    let mut entry = read_itl(page, index)?;
    entry.lock_cnt = entry.lock_cnt.saturating_add(1);
    write_itl(page, index, &entry)
}

/// 行锁计数 −1（0 以下拒绝）。
pub fn unlock(page: &mut Page, index: u16) -> Result<(), ItlError> {
    let mut entry = read_itl(page, index)?;
    entry.lock_cnt = entry.lock_cnt.checked_sub(1).ok_or(ItlError::Malformed)?;
    write_itl(page, index, &entry)
}

#[cfg(test)]
mod tests {
    use crate::page::{flags, PageType, ITL_COUNT_OFFSET, ITL_ENTRY_OFFSET, WORKSPACE_REF_LEN};

    use super::*;

    fn page() -> Page {
        Page::new(PageType::HeapTable, [0u8; WORKSPACE_REF_LEN], 3, 5)
    }

    fn txn(slot: u8, wrap: u32) -> TxnId {
        TxnId::from_parts(0, slot, wrap)
    }

    #[test]
    fn entry_offsets_are_pinned() {
        assert_eq!(ITL_ENTRY_OFFSET, 40);
        assert_eq!(ITL_ENTRY_LEN, 24);
        assert_eq!(ITL_COUNT_OFFSET, 32);
        let mut p = page();
        write_itl(
            &mut p,
            0,
            &ItlEntry {
                txn_id: txn(3, 7),
                undo_ptr: Some(RowId::from_parts(1, 100, 2).unwrap()),
                commit_seq: Some(CommitSeq::from_raw(0x0A0B).unwrap()),
                lock_cnt: 2,
                state: ItlState::Committed,
            },
        )
        .unwrap();
        let b = p.as_bytes();
        assert_eq!(&b[40..46], &txn(3, 7).to_bytes(), "txn_id 在 40");
        assert_eq!(
            &b[46..52],
            &RowId::from_parts(1, 100, 2).unwrap().to_bytes()
        );
        assert_eq!(b[52], 0x0B, "commit_seq 低字节");
        assert_eq!(u16::from_le_bytes([b[58], b[59]]), 2, "lock_cnt 在 58");
        assert_eq!(b[60], ItlState::Committed.as_u8(), "flags 在 60");
        assert_eq!(read_itl(&p, 0).unwrap().state, ItlState::Committed);
    }

    #[test]
    fn acquire_reuse_commit_and_snapshot() {
        let mut p = page();
        assert_eq!(itl_count(&p).unwrap(), 1, "INITRANS = 1");
        let i = acquire(&mut p, 4, txn(0, 0)).unwrap();
        assert_eq!(i, 0);
        let e = read_itl(&p, 0).unwrap();
        assert_eq!(e.state, ItlState::Active);
        assert_eq!(e.txn_id, txn(0, 0));
        assert!(!e.reusable(), "活动槽不可复用");

        // 锁一行 → 计数 1；提交 → Committed + 准确序号。
        lock(&mut p, 0).unwrap();
        mark_committed(&mut p, 0, CommitSeq::from_raw(9).unwrap()).unwrap();
        let e = read_itl(&p, 0).unwrap();
        assert_eq!((e.state, e.lock_cnt), (ItlState::Committed, 1));
        assert!(!e.reusable(), "还有行锁——不可复用");

        unlock(&mut p, 0).unwrap();
        assert!(read_itl(&p, 0).unwrap().reusable(), "已清除且无锁 ⇒ 可复用");

        // 复用：槽号不变、占用者换人。
        let i = acquire(&mut p, 4, txn(1, 0)).unwrap();
        assert_eq!(i, 0);
        assert_eq!(read_itl(&p, 0).unwrap().txn_id, txn(1, 0));

        // ITL 覆盖的旧值快照与恢复（24B 原样）。
        let snap = snapshot(&p, 0).unwrap();
        mark_rolled_back(&mut p, 0).unwrap();
        assert_ne!(snapshot(&p, 0).unwrap(), snap);
        restore(&mut p, 0, &snap).unwrap();
        assert_eq!(snapshot(&p, 0).unwrap(), snap);
    }

    #[test]
    fn grow_extends_within_itl_max_and_space() {
        let mut p = page();
        // 槽 0 保持活动 ⇒ 只能扩展。
        acquire(&mut p, 3, txn(0, 0)).unwrap();
        let i = acquire(&mut p, 3, txn(1, 0)).unwrap();
        assert_eq!(i, 1, "扩展出新槽");
        assert_eq!(itl_count(&p).unwrap(), 2);
        let i = acquire(&mut p, 3, txn(2, 0)).unwrap();
        assert_eq!(i, 2);
        assert_eq!(itl_count(&p).unwrap(), 3);
        // 达 itl_max ⇒ 明确报"无槽可用"（调用方应等待）。
        assert_eq!(
            acquire(&mut p, 3, txn(3, 0)),
            Err(ItlError::NoSlotAvailable)
        );
        // 页头后移了 2×24：固定头末尾 = 68 + 2×24 = 116。
        assert_eq!(p.fixed_header_end(), 68 + 2 * 24);
        assert_eq!(p.header().unwrap().itl_count, 3);
        assert_eq!(p.header().unwrap().flags & flags::INITIALIZED, 1);
        // **布局**：ITL[0] 在 40；扩展槽在固定头之后（68、92）——
        // 空间区（slot_count 64 / free_end 66）不被覆盖。
        assert_eq!(itl_entry_offset(0), 40);
        assert_eq!(itl_entry_offset(1), 68);
        assert_eq!(itl_entry_offset(2), 92);
        let header = p.header().unwrap();
        assert_eq!(header.slot_count, 0, "空间区未被 ITL 覆盖");
        assert_eq!(header.free_end, (crate::page::PAGE_SIZE - 4) as u16);
        assert_eq!(read_itl(&p, 1).unwrap().txn_id, txn(1, 0), "扩展槽内容可读");
    }

    #[test]
    fn grow_refuses_when_free_space_would_be_pierced() {
        let mut p = page();
        // 人为把 free_end 压到刚够 1 槽：固定头末尾 68 + 目录 2×0 = 68。
        let mut header = p.header().unwrap();
        header.free_end = 68;
        p.write_header(&header);
        assert_eq!(grow(&mut p, 2), Err(ItlError::NotEnoughSpace));
        assert_eq!(itl_count(&p).unwrap(), 1);
    }

    #[test]
    fn slot_bounds_and_malformed_flags_are_detected() {
        let mut p = page();
        assert_eq!(read_itl(&p, 1), Err(ItlError::SlotOutOfRange(1)));
        assert_eq!(
            write_itl(&mut p, 1, &ItlEntry::FREE),
            Err(ItlError::SlotOutOfRange(1))
        );
        // 未知 flags。
        let mut b = [0u8; ITL_ENTRY_LEN];
        b[20] = 9;
        assert_eq!(ItlEntry::decode(&b), Err(ItlError::Malformed));
        // 空闲条目往返（"原为空闲"的 ITL 覆盖旧值 = 全零）。
        let mut out = [1u8; ITL_ENTRY_LEN];
        ItlEntry::FREE.encode(&mut out);
        assert_eq!(out, [0u8; ITL_ENTRY_LEN]);
        assert_eq!(ItlEntry::decode(&out).unwrap(), ItlEntry::FREE);
    }
}
