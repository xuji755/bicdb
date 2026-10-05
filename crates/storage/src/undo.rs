//! Undo 段：**事务表**、段控制与**撤销记录**（§4.6.2 / §4.6.3）。
//!
//! 事务表放在 **undo 段头页（类型 7）的类型扩展区**——紧跟其后是段控制
//! （`KTUXC` 的对应物），再往后才是区映射条目（§5.11）：
//!
//! ```text
//! 偏移 128   事务表：256 槽 × 24B = 6144B
//! 偏移 6272  段控制：24B（reclaim_seq │ commit_head │ commit_tail │ slot_count │ free_head）
//! 偏移 6296  区映射条目（7B × ≤1440）
//! ```
//!
//! `txn_id` 是**三段式**（6B）：`usn 8 │ slot 8 │ wrap 32`——拿到它就能
//! O(1) 定位事务表槽；`wrap` 是槽重用代次，让**陈旧引用被判定为"不是自己"**
//! （防的是 undo 链误入，不是行锁）。槽号 0 起、`0xFFFF` = 链尾。
//!
//! 撤销记录是 undo 页里的"一行"（长度由槽位目录给出）：`prev_undo 6B │
//! op 1B │ flags 1B │ rowid 6B │ 载荷（按 op）`；超长时沿 §6.3 的片段链
//! 跨页（首片带"多片头"标志、尾片带"多片尾"，`prev_undo` 恒指首片）。

use bicdb_common::seq::CommitSeq;
use bicdb_workspace::io::{FileHandle, FileIo};

use crate::page::{Page, PageType, ITL_ENTRY_LEN};
use crate::rowid::{Rdba, RowId};
use crate::segment::{
    read_header, write_header, SegType, Segment, SegmentSpaceError, BITMAP_PAGE_COVERAGE,
    SEG_EXTENSION_OFFSET,
};

/// 事务表槽数（§14 第 28 项：V1.0 单段、N = 256）。
pub const TXN_SLOTS: usize = 256;
/// 单个事务表槽的字节数（23B 字段 + 1B 保留，对齐 24B）。
pub const TXN_SLOT_LEN: usize = 24;
/// 事务表字节数。
pub const TXN_TABLE_LEN: usize = TXN_SLOTS * TXN_SLOT_LEN;
/// 段控制字节数（`reclaim_seq 6B │ commit_head 2B │ commit_tail 2B │
/// slot_count 2B │ free_head 2B │ 保留 10B`）。
pub const UNDO_CONTROL_LEN: usize = 24;
/// undo 段类型扩展区总长（事务表 + 段控制）。
pub const UNDO_EXTENSION_LEN: usize = TXN_TABLE_LEN + UNDO_CONTROL_LEN;
/// 段控制在页内的偏移（紧跟事务表）。
pub const UNDO_CONTROL_OFFSET: usize = SEG_EXTENSION_OFFSET + TXN_TABLE_LEN;
/// 空闲链表尾哨兵。
pub const NO_SLOT: u16 = 0xFFFF;

/// Undo 结构错误。
#[derive(Debug, PartialEq, Eq)]
pub enum UndoError {
    /// 不是段头页。
    NotSegmentHeader,
    /// 槽号越界。
    SlotOutOfRange(u16),
    /// 空闲链表损坏（环/越界）。
    FreeListCorrupt,
    /// 撤销记录载荷非法（长度与 op 不符、列列表越界……）。
    MalformedRecord,
    /// 撤销记录操作码未知。
    UnknownOp(u8),
    /// 段类型不是 Undo。
    WrongSegType,
    /// 页不是 undo 页（put/get 记录用）。
    NotUndoPage,
    /// 页内放不下（需新开 undo 页）。
    PageFull,
}

impl std::fmt::Display for UndoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UndoError::NotSegmentHeader => f.write_str("不是段头页"),
            UndoError::SlotOutOfRange(s) => write!(f, "事务表槽号 {s} 越界"),
            UndoError::FreeListCorrupt => f.write_str("事务表空闲链表损坏"),
            UndoError::MalformedRecord => f.write_str("撤销记录载荷非法"),
            UndoError::UnknownOp(op) => write!(f, "撤销记录操作码 {op} 未知"),
            UndoError::WrongSegType => f.write_str("段类型不是 Undo"),
            UndoError::NotUndoPage => f.write_str("不是 undo 页"),
            UndoError::PageFull => f.write_str("undo 页已满"),
        }
    }
}

impl std::error::Error for UndoError {}

/// 事务表槽状态（1B）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnState {
    /// 0：空闲（在空闲链表上）。
    Free = 0,
    /// 1：活动。
    Active = 1,
    /// 2：已提交（提交序号已落）。
    Committed = 2,
    /// 3：待回滚。
    PendingRollback = 3,
}

impl TxnState {
    /// 由字节解码。
    #[must_use]
    pub const fn from_u8(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(Self::Free),
            1 => Some(Self::Active),
            2 => Some(Self::Committed),
            3 => Some(Self::PendingRollback),
            _ => None,
        }
    }

    /// 字节值。
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// 事务标识（**三段式**，6B）：`usn 8 │ slot 8 │ wrap 32`。
///
/// `txn_id` 的职责是**定位**（不是排序）：拿到它即可直接算出槽。
/// `commit_seq` 才是排序量——两者不得混用（它们甚至不同时分配）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TxnId(u64);

impl TxnId {
    /// 由三段构造。
    #[must_use]
    pub const fn from_parts(usn: u8, slot: u8, wrap: u32) -> Self {
        Self(((usn as u64) << 40) | ((slot as u64) << 32) | (wrap as u64))
    }

    /// undo 段号（V1.0 恒 0）。
    #[must_use]
    pub const fn usn(self) -> u8 {
        (self.0 >> 40) as u8
    }

    /// 原始 48 位值（redo 记录头的 `txn_id` 字段与此同一编码）。
    #[must_use]
    pub const fn as_raw(self) -> u64 {
        self.0
    }

    /// 事务表槽号。
    #[must_use]
    pub const fn slot(self) -> u8 {
        ((self.0 >> 32) & 0xFF) as u8
    }

    /// 槽重用代次。
    #[must_use]
    pub const fn wrap(self) -> u32 {
        (self.0 & 0xFFFF_FFFF) as u32
    }

    /// 6 字节小端编码。
    #[must_use]
    pub fn to_bytes(self) -> [u8; 6] {
        self.0.to_le_bytes()[..6].try_into().expect("6 字节")
    }

    /// 由 6 字节小端解码。
    #[must_use]
    pub fn from_bytes(bytes: &[u8; 6]) -> Self {
        let mut b = [0u8; 8];
        b[..6].copy_from_slice(bytes);
        Self(u64::from_le_bytes(b))
    }
}

/// 一个事务表槽（24B）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxnSlot {
    /// 状态。
    pub state: TxnState,
    /// 提交序号（提交时落）。
    pub commit_seq: CommitSeq,
    /// **undo 链链头**（该事务最新一条撤销记录的位置；`None` = 尚无）。
    pub undo_current: Option<RowId>,
    /// 链上记录数（容量与回收判断用）。
    pub rec_count: u32,
    /// 空闲链表后继（状态无关字段；仅 `Free` 时有效）。
    pub next_free: u16,
    /// 槽重用代次。
    pub wrap: u32,
}

impl TxnSlot {
    /// 空闲槽（`next_free` 由初始化/释放路径填写）。
    #[must_use]
    pub fn free(next_free: u16, wrap: u32) -> Self {
        Self {
            state: TxnState::Free,
            commit_seq: CommitSeq::from_raw(0).expect("0 在 48 位域内"),
            undo_current: None,
            rec_count: 0,
            next_free,
            wrap,
        }
    }

    /// 编码到 24B。
    pub fn encode(&self, out: &mut [u8]) {
        debug_assert_eq!(out.len(), TXN_SLOT_LEN);
        out.fill(0);
        out[0] = self.state.as_u8();
        out[1..7].copy_from_slice(&self.commit_seq.as_raw().to_le_bytes()[..6]);
        if let Some(undo) = self.undo_current {
            out[7..13].copy_from_slice(&undo.to_bytes());
        }
        out[13..17].copy_from_slice(&self.rec_count.to_le_bytes());
        out[17..19].copy_from_slice(&self.next_free.to_le_bytes());
        out[19..23].copy_from_slice(&self.wrap.to_le_bytes());
    }

    /// 由 24B 解码。
    pub fn decode(b: &[u8]) -> Result<Self, UndoError> {
        debug_assert_eq!(b.len(), TXN_SLOT_LEN);
        let state = TxnState::from_u8(b[0]).ok_or(UndoError::MalformedRecord)?;
        let mut seq = [0u8; 8];
        seq[..6].copy_from_slice(&b[1..7]);
        let mut rid = [0u8; 6];
        rid.copy_from_slice(&b[7..13]);
        let undo_raw = u64::from_le_bytes({
            let mut raw = [0u8; 8];
            raw[..6].copy_from_slice(&rid);
            raw
        });
        let undo = RowId::from_raw(undo_raw).ok_or(UndoError::MalformedRecord)?;
        Ok(Self {
            state,
            commit_seq: CommitSeq::from_raw(u64::from_le_bytes(seq))
                .ok_or(UndoError::MalformedRecord)?,
            undo_current: (undo_raw != 0).then_some(undo),
            rec_count: u32::from_le_bytes(b[13..17].try_into().expect("4 字节")),
            next_free: u16::from_le_bytes(b[17..19].try_into().expect("2 字节")),
            wrap: u32::from_le_bytes(b[19..23].try_into().expect("4 字节")),
        })
    }
}

/// 段控制（`KTUXC` 的对应物；24B）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UndoControl {
    /// **已回收到的提交序号水位**——低于它的 undo 已被复用。
    pub reclaim_seq: CommitSeq,
    /// 提交链表头（最老，回收从这里开始）。
    pub commit_head: u16,
    /// 提交链表尾。
    pub commit_tail: u16,
    /// 事务表槽数。
    pub slot_count: u16,
    /// 空闲槽链表头。
    pub free_head: u16,
}

impl Default for UndoControl {
    fn default() -> Self {
        Self {
            reclaim_seq: CommitSeq::from_raw(0).expect("0 在 48 位域内"),
            commit_head: NO_SLOT,
            commit_tail: NO_SLOT,
            slot_count: TXN_SLOTS as u16,
            free_head: NO_SLOT,
        }
    }
}

impl UndoControl {
    /// 编码到 24B。
    pub fn encode(&self, out: &mut [u8]) {
        debug_assert_eq!(out.len(), UNDO_CONTROL_LEN);
        out.fill(0);
        out[0..6].copy_from_slice(&self.reclaim_seq.as_raw().to_le_bytes()[..6]);
        out[6..8].copy_from_slice(&self.commit_head.to_le_bytes());
        out[8..10].copy_from_slice(&self.commit_tail.to_le_bytes());
        out[10..12].copy_from_slice(&self.slot_count.to_le_bytes());
        out[12..14].copy_from_slice(&self.free_head.to_le_bytes());
    }

    /// 由 24B 解码。
    pub fn decode(b: &[u8]) -> Result<Self, UndoError> {
        debug_assert_eq!(b.len(), UNDO_CONTROL_LEN);
        let mut seq = [0u8; 8];
        seq[..6].copy_from_slice(&b[0..6]);
        Ok(Self {
            reclaim_seq: CommitSeq::from_raw(u64::from_le_bytes(seq))
                .ok_or(UndoError::MalformedRecord)?,
            commit_head: u16::from_le_bytes(b[6..8].try_into().expect("2 字节")),
            commit_tail: u16::from_le_bytes(b[8..10].try_into().expect("2 字节")),
            slot_count: u16::from_le_bytes(b[10..12].try_into().expect("2 字节")),
            free_head: u16::from_le_bytes(b[12..14].try_into().expect("2 字节")),
        })
    }
}

fn check_seg_header(page: &Page) -> Result<(), UndoError> {
    match page.header() {
        Some(h) if h.page_type == PageType::SegmentHeader => Ok(()),
        _ => Err(UndoError::NotSegmentHeader),
    }
}

fn slot_offset(index: u16) -> usize {
    SEG_EXTENSION_OFFSET + usize::from(index) * TXN_SLOT_LEN
}

/// 读一个事务表槽。
pub fn read_slot(page: &Page, index: u16) -> Result<TxnSlot, UndoError> {
    check_seg_header(page)?;
    if usize::from(index) >= TXN_SLOTS {
        return Err(UndoError::SlotOutOfRange(index));
    }
    let at = slot_offset(index);
    TxnSlot::decode(&page.as_bytes()[at..at + TXN_SLOT_LEN])
}

/// 写一个事务表槽。
pub fn write_slot(page: &mut Page, index: u16, slot: &TxnSlot) -> Result<(), UndoError> {
    check_seg_header(page)?;
    if usize::from(index) >= TXN_SLOTS {
        return Err(UndoError::SlotOutOfRange(index));
    }
    let at = slot_offset(index);
    slot.encode(&mut page.as_bytes_mut()[at..at + TXN_SLOT_LEN]);
    Ok(())
}

/// 读段控制。
pub fn read_control(page: &Page) -> Result<UndoControl, UndoError> {
    check_seg_header(page)?;
    UndoControl::decode(
        &page.as_bytes()[UNDO_CONTROL_OFFSET..UNDO_CONTROL_OFFSET + UNDO_CONTROL_LEN],
    )
}

/// 写段控制。
pub fn write_control(page: &mut Page, control: &UndoControl) -> Result<(), UndoError> {
    check_seg_header(page)?;
    control.encode(
        &mut page.as_bytes_mut()[UNDO_CONTROL_OFFSET..UNDO_CONTROL_OFFSET + UNDO_CONTROL_LEN],
    );
    Ok(())
}

/// **初始化 undo 段头页的扩展区**：事务表全空闲（0 → 1 → … → 0xFFFF 链）、
/// 段控制就位。**只动扩展区字节**（段头公共部分与区映射条目不起）。
pub fn init_extension(page: &mut Page) -> Result<(), UndoError> {
    check_undo_segment(page)?;
    for i in 0..TXN_SLOTS as u16 {
        let next = if usize::from(i) + 1 < TXN_SLOTS {
            i + 1
        } else {
            NO_SLOT
        };
        write_slot(page, i, &TxnSlot::free(next, 0))?;
    }
    write_control(
        page,
        &UndoControl {
            reclaim_seq: CommitSeq::from_raw(0).expect("0 在 48 位域内"),
            commit_head: NO_SLOT,
            commit_tail: NO_SLOT,
            slot_count: TXN_SLOTS as u16,
            free_head: 0,
        },
    )?;
    Ok(())
}

/// 读段头并核对是 undo 段。
fn read_header_checked(page: &Page) -> Result<SegType, UndoError> {
    check_seg_header(page)?;
    crate::segment::read_header(page)
        .map(|h| h.seg_type)
        .map_err(|_| UndoError::NotSegmentHeader)
}

/// 校验是 undo 段头页。
fn check_undo_segment(page: &Page) -> Result<(), UndoError> {
    match read_header_checked(page)? {
        SegType::Undo => Ok(()),
        _ => Err(UndoError::WrongSegType),
    }
}

/// **分配一个事务表槽**（弹空闲链 → `Active`；`wrap` 不变——它在**释放**时推进）。
pub fn allocate_slot(page: &mut Page) -> Result<(u16, TxnSlot), UndoError> {
    let mut control = read_control(page)?;
    if control.free_head == NO_SLOT {
        return Err(UndoError::FreeListCorrupt); // 无空闲槽（256 并发写事务已满）
    }
    let index = control.free_head;
    let mut slot = read_slot(page, index)?;
    if slot.state != TxnState::Free {
        return Err(UndoError::FreeListCorrupt);
    }
    control.free_head = slot.next_free;
    slot.state = TxnState::Active;
    slot.commit_seq = CommitSeq::from_raw(0).expect("0 在 48 位域内");
    slot.undo_current = None;
    slot.rec_count = 0;
    slot.next_free = NO_SLOT;
    write_slot(page, index, &slot)?;
    write_control(page, &control)?;
    Ok((index, slot))
}

/// **释放一个事务表槽**（槽重用：`wrap + 1`，压回空闲链头）。
pub fn free_slot(page: &mut Page, index: u16) -> Result<(), UndoError> {
    let mut control = read_control(page)?;
    let mut slot = read_slot(page, index)?;
    slot.state = TxnState::Free;
    slot.wrap = slot.wrap.wrapping_add(1);
    slot.next_free = control.free_head;
    slot.undo_current = None;
    slot.rec_count = 0;
    control.free_head = index;
    write_slot(page, index, &slot)?;
    write_control(page, &control)?;
    Ok(())
}

/// **由 `txn_id` 直接定位槽**：槽号 + `wrap` 必须同时相符——
/// 不符即"**不是自己**"（槽已被复用，陈旧引用在此被判定）。
pub fn find_slot(page: &Page, txn_id: TxnId) -> Result<Option<TxnSlot>, UndoError> {
    if txn_id.usn() != 0 {
        return Ok(None); // V1.0 单 undo 段
    }
    let slot = read_slot(page, u16::from(txn_id.slot()))?;
    if slot.wrap == txn_id.wrap() && slot.state != TxnState::Free {
        Ok(Some(slot))
    } else {
        Ok(None)
    }
}

/// 由槽号与槽构造 `txn_id`（三段式打包）。
#[must_use]
pub fn txn_id_of(index: u16, slot: &TxnSlot) -> TxnId {
    TxnId::from_parts(0, index as u8, slot.wrap)
}

/// **分析阶段的事务表修复**（§4.6.6 ⑤"前滚补标记"）：
///
/// 1. 对日志里**已提交**的事务（`committed` = `txn_id → commit_seq` 对），
///    把事务表槽补成 `Committed` + 准确序号——提交记录已入流，槽标记即使
///    在崩溃中丢了也在此恢复；
/// 2. 返回**待撤销的槽列表**（`Active` / `PendingRollback`）——它包含
///    "检查点前就活动、日志里没有任何记录"的当事务，是输家集合的另一半
///    （日志侧的另一半由 `bicdb_wal::analysis` 给出）。
///
/// **为什么补标记是正确性步骤**：延迟块清除（§11.1.1）下，已提交但未清除
/// 的块其 ITL 条目仍是"活动"外观，可见性判定靠 `txn_id` 回查本槽
/// （§12.2 ②′）——槽标记不补，读者会把已提交事务当成未提交去撤销。
///
/// 幂等：已有 `Committed` 标记的槽不动；重复调用结果相同。
pub fn repair_committed_slots(
    page: &mut Page,
    committed: &[(TxnId, CommitSeq)],
) -> Result<Vec<u16>, UndoError> {
    check_undo_segment(page)?;
    for (txn_id, seq) in committed {
        let Some(mut slot) = find_slot(page, *txn_id)? else {
            continue; // 槽已回收（提交后释放过）：无需补标记
        };
        if slot.state != TxnState::Committed {
            slot.state = TxnState::Committed;
            slot.commit_seq = *seq;
            write_slot(page, u16::from(txn_id.slot()), &slot)?;
        }
    }
    let mut losers = Vec::new();
    for i in 0..TXN_SLOTS as u16 {
        let slot = read_slot(page, i)?;
        if matches!(slot.state, TxnState::Active | TxnState::PendingRollback) {
            losers.push(i);
        }
    }
    Ok(losers)
}

// ---------------------------------------------------------------------------
// 撤销记录（§4.6.2）
// ---------------------------------------------------------------------------

/// `flags` 位：多片撤销记录的**头片**（Oracle8+ 的 0x01）。
pub const UNDO_FLAG_MULTI_HEAD: u8 = 0x01;
/// `flags` 位：多片撤销记录的**尾片**（Oracle8+ 的 0x02）。
pub const UNDO_FLAG_MULTI_TAIL: u8 = 0x02;

/// 撤销记录固定头长度：`prev_undo 6B │ op 1B │ flags 1B │ rowid 6B`。
pub const UNDO_RECORD_HEADER_LEN: usize = 14;

/// 撤销记录操作码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UndoOp {
    /// 0：插入（无载荷——撤销动作 = 删除该行）。
    Insert = 0,
    /// 1：删除（载荷 = 整行旧值，含行头）。
    Delete = 1,
    /// 2：更新（载荷 = 旧 `itl_slot` + 列前像列表）。
    Update = 2,
    /// 3：转发指针更新（载荷 = 旧的转发目标 6B）。
    Forward = 3,
    /// 4：ITL 覆盖（载荷 = 被覆盖的旧 ITL 24B，或"原为空闲"）。
    ItlOverwrite = 4,
}

impl UndoOp {
    /// 由字节解码。
    #[must_use]
    pub const fn from_u8(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(Self::Insert),
            1 => Some(Self::Delete),
            2 => Some(Self::Update),
            3 => Some(Self::Forward),
            4 => Some(Self::ItlOverwrite),
            _ => None,
        }
    }

    /// 字节值。
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// 撤销记录载荷（按 `op` 解释）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoPayload {
    /// 插入：无。
    None,
    /// 删除：整行旧值（**含行头**）。
    FullRow(Vec<u8>),
    /// 更新：旧 `itl_slot` 1B + 每个变更段的（**行内偏移** 2B │ 旧值长度 2B │ 旧值）。
    ///
    /// **"列号"改为"行内偏移"**（编码期细化，2026-10-05）：补偿落点要能
    /// **不依赖列元数据**地写回（`apply_undo_to_page` 手上只有页），而行内
    /// 偏移在"等长就地更新"下是稳定的——2B 换来自包含的补偿。
    Update {
        /// 行头里原来的 `itl_slot` 字节。
        old_itl_slot: u8,
        /// 变更段的前像列表：（行内偏移, 旧值）。
        patches: Vec<(u16, Vec<u8>)>,
    },
    /// 转发指针更新：旧的转发目标。
    Forward(RowId),
    /// ITL 覆盖：**被覆盖的 ITL 槽号** + 旧内容（`None` = 原为空闲）。
    ///
    /// 槽号是回滚的落点（§4.6.2 的"ITL 覆盖"是**块级**动作——不针对某行，
    /// 记录里的 `rowid` 只借它的 file/block 定位块）。
    ItlOverwrite {
        /// 被覆盖的 ITL 槽号。
        itl_slot: u8,
        /// 旧内容（`None` = 原为空闲）。
        old: Option<[u8; ITL_ENTRY_LEN]>,
    },
}

/// 一条撤销记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoRecord {
    /// 前一条记录的位置（`None` = 链首）。
    pub prev: Option<RowId>,
    /// 操作码。
    pub op: UndoOp,
    /// 标志位（多片头/尾）。
    pub flags: u8,
    /// **被修改的行**（链是全局的，一条记录可能改的是别的行）。
    pub rowid: RowId,
    /// 载荷。
    pub payload: UndoPayload,
}

impl UndoRecord {
    /// 编码（无长度字段——总长由承载它的 undo 页槽位目录给出）。
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(UNDO_RECORD_HEADER_LEN + 32);
        out.extend_from_slice(&self.prev.map_or([0u8; 6], RowId::to_bytes));
        out.push(self.op.as_u8());
        out.push(self.flags);
        out.extend_from_slice(&self.rowid.to_bytes());
        match &self.payload {
            UndoPayload::None => {}
            UndoPayload::FullRow(bytes) => out.extend_from_slice(bytes),
            UndoPayload::Update {
                old_itl_slot,
                patches,
            } => {
                out.push(*old_itl_slot);
                for (off, old) in patches {
                    out.extend_from_slice(&off.to_le_bytes());
                    out.extend_from_slice(&(old.len() as u16).to_le_bytes());
                    out.extend_from_slice(old);
                }
            }
            UndoPayload::Forward(target) => out.extend_from_slice(&target.to_bytes()),
            UndoPayload::ItlOverwrite { itl_slot, old } => {
                out.push(*itl_slot);
                match old {
                    Some(entry) => out.extend_from_slice(entry),
                    None => out.extend_from_slice(&[0u8; ITL_ENTRY_LEN]), // 全零 = 原为空闲
                }
            }
        }
        out
    }

    /// 解码（输入 = 承载槽位给出的整条字节）。
    pub fn decode(bytes: &[u8]) -> Result<Self, UndoError> {
        if bytes.len() < UNDO_RECORD_HEADER_LEN {
            return Err(UndoError::MalformedRecord);
        }
        let mut prev = [0u8; 6];
        prev.copy_from_slice(&bytes[0..6]);
        let prev_raw = u64::from_le_bytes({
            let mut raw = [0u8; 8];
            raw[..6].copy_from_slice(&prev);
            raw
        });
        let prev = (prev_raw != 0).then(|| RowId::from_raw(prev_raw).expect("6 字节在界内"));
        let op = UndoOp::from_u8(bytes[6]).ok_or(UndoError::UnknownOp(bytes[6]))?;
        let flags = bytes[7];
        let mut rid = [0u8; 6];
        rid.copy_from_slice(&bytes[8..14]);
        let rowid = RowId::from_bytes(&rid);
        let body = &bytes[UNDO_RECORD_HEADER_LEN..];

        let payload = match op {
            UndoOp::Insert => {
                if !body.is_empty() {
                    return Err(UndoError::MalformedRecord);
                }
                UndoPayload::None
            }
            UndoOp::Delete => UndoPayload::FullRow(body.to_vec()),
            UndoOp::Update => {
                if body.is_empty() {
                    return Err(UndoError::MalformedRecord);
                }
                let old_itl_slot = body[0];
                let mut patches = Vec::new();
                let mut at = 1usize;
                while at < body.len() {
                    if at + 4 > body.len() {
                        return Err(UndoError::MalformedRecord);
                    }
                    let off = u16::from_le_bytes([body[at], body[at + 1]]);
                    let len = usize::from(u16::from_le_bytes([body[at + 2], body[at + 3]]));
                    at += 4;
                    if at + len > body.len() {
                        return Err(UndoError::MalformedRecord);
                    }
                    patches.push((off, body[at..at + len].to_vec()));
                    at += len;
                }
                UndoPayload::Update {
                    old_itl_slot,
                    patches,
                }
            }
            UndoOp::Forward => {
                if body.len() != 6 {
                    return Err(UndoError::MalformedRecord);
                }
                UndoPayload::Forward(RowId::from_bytes(body.try_into().expect("6 字节")))
            }
            UndoOp::ItlOverwrite => {
                if body.len() != 1 + ITL_ENTRY_LEN {
                    return Err(UndoError::MalformedRecord);
                }
                let itl_slot = body[0];
                let entry: [u8; ITL_ENTRY_LEN] = body[1..].try_into().expect("24 字节");
                UndoPayload::ItlOverwrite {
                    itl_slot,
                    old: (entry != [0u8; ITL_ENTRY_LEN]).then_some(entry),
                }
            }
        };
        Ok(Self {
            prev,
            op,
            flags,
            rowid,
            payload,
        })
    }
}

/// undo 段创建错误。
#[derive(Debug)]
pub enum UndoSegmentError {
    /// 段空间操作错误。
    Space(SegmentSpaceError),
    /// 事务表/段控制初始化错误。
    Undo(UndoError),
}

impl std::fmt::Display for UndoSegmentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UndoSegmentError::Space(e) => write!(f, "{e}"),
            UndoSegmentError::Undo(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for UndoSegmentError {}

impl From<SegmentSpaceError> for UndoSegmentError {
    fn from(e: SegmentSpaceError) -> Self {
        UndoSegmentError::Space(e)
    }
}

impl From<UndoError> for UndoSegmentError {
    fn from(e: UndoError) -> Self {
        UndoSegmentError::Undo(e)
    }
}

/// **创建 undo 段**（file 1：`seg_type = Undo` + 事务表与段控制就位）。
pub fn create_undo_segment<'io, 'f>(
    file: &'f mut crate::datafile::DataFile<'io>,
    obj: u32,
    dataobj: u32,
    itl_max: u8,
) -> Result<Segment<'io, 'f>, UndoSegmentError> {
    let segment = Segment::create(file, SegType::Undo, obj, dataobj, itl_max, 0, 0)?;
    let mut page = segment.read_page(0)?;
    init_extension(&mut page)?;
    segment.write_page(0, &mut page)?;
    Ok(segment)
}

// ---------------------------------------------------------------------------
// Undo 页写入与链（§4.6.1 / §4.6.2）
// ---------------------------------------------------------------------------

fn require_undo_page(page: &Page) -> Result<(), UndoError> {
    match page.header() {
        Some(h) if h.page_type == PageType::Undo => Ok(()),
        _ => Err(UndoError::NotUndoPage),
    }
}

/// undo 页初始的 `free_end`（第一条记录的上界；页尾 4B）。
fn initial_free_end() -> usize {
    crate::page::PAGE_SIZE - PageType::Undo.tail_len()
}

/// 页内还能放下 `len` 字节记录吗（含可能新增的 2B 槽位目录）。
fn fits(page: &Page, len: usize) -> bool {
    page.free_end() >= page.free_start() + len + crate::page::SLOT_ENTRY_LEN
}

/// **追加一条撤销记录**进 undo 页（槽号递增、记录向下生长；**只追加、不复用槽**）。
///
/// 记载长度**不存**（§4.6.2）：追加严格向下生长，故第 k 条的长度 =
/// 上一条的偏移 − 本条偏移；第 1 条的上界 = 页初始 `free_end`。
pub fn put_record(page: &mut Page, record: &UndoRecord) -> Result<u16, UndoError> {
    require_undo_page(page)?;
    let bytes = record.encode();
    if !fits(page, bytes.len()) {
        return Err(UndoError::PageFull);
    }
    let slot_count = page.slot_count() as usize;
    let offset = page.free_end() - bytes.len();
    page.as_bytes_mut()[offset..offset + bytes.len()].copy_from_slice(&bytes);
    page.set_free_end(offset);
    page.set_slot_count(u16::try_from(slot_count + 1).map_err(|_| UndoError::PageFull)?)
        .map_err(|_| UndoError::PageFull)?;
    let entry = crate::page::SlotEntry::new(offset as u16, crate::page::SlotStatus::Normal)
        .ok_or(UndoError::MalformedRecord)?;
    page.set_slot(slot_count, entry);
    Ok(slot_count as u16 + 1) // 槽号从 1 起（D-06）
}

/// 读一条撤销记录（槽号从 1 起）。
pub fn get_record(page: &Page, slot: u16) -> Result<UndoRecord, UndoError> {
    require_undo_page(page)?;
    let index = crate::heap::slot_index(slot).ok_or(UndoError::SlotOutOfRange(slot))?;
    let entry = page.slot(index).ok_or(UndoError::SlotOutOfRange(slot))?;
    if entry.status() == crate::page::SlotStatus::Free {
        return Err(UndoError::SlotOutOfRange(slot));
    }
    let top = if index == 0 {
        initial_free_end()
    } else {
        page.slot(index - 1)
            .ok_or(UndoError::MalformedRecord)?
            .offset() as usize
    };
    let offset = entry.offset() as usize;
    if top < offset || top > crate::page::PAGE_SIZE {
        return Err(UndoError::MalformedRecord);
    }
    UndoRecord::decode(&page.as_bytes()[offset..top])
}

/// Undo 链错误。
#[derive(Debug)]
pub enum UndoChainError {
    /// 事务表/记录格式错误。
    Undo(UndoError),
    /// 段空间错误。
    Space(SegmentSpaceError),
    /// 该槽空闲（事务尚未开始）。
    SlotNotActive(u16),
    /// 段内位图覆盖不足（多页位图随后）。
    BitmapCoverage,
    /// 段内位图操作错误。
    Bitmap(crate::bitmap::BitmapError),
    /// 页内 ITL 操作错误。
    Itl(crate::itl::ItlError),
}

impl std::fmt::Display for UndoChainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UndoChainError::Undo(e) => write!(f, "{e}"),
            UndoChainError::Space(e) => write!(f, "{e}"),
            UndoChainError::SlotNotActive(s) => write!(f, "事务表槽 {s} 空闲——事务尚未开始"),
            UndoChainError::BitmapCoverage => f.write_str("段内位图覆盖不足（多页位图随后）"),
            UndoChainError::Bitmap(e) => write!(f, "段内位图：{e}"),
            UndoChainError::Itl(e) => write!(f, "ITL：{e}"),
        }
    }
}

impl std::error::Error for UndoChainError {}

impl From<UndoError> for UndoChainError {
    fn from(e: UndoError) -> Self {
        UndoChainError::Undo(e)
    }
}

impl From<SegmentSpaceError> for UndoChainError {
    fn from(e: SegmentSpaceError) -> Self {
        UndoChainError::Space(e)
    }
}

impl From<crate::bitmap::BitmapError> for UndoChainError {
    fn from(e: crate::bitmap::BitmapError) -> Self {
        UndoChainError::Bitmap(e)
    }
}

impl From<crate::itl::ItlError> for UndoChainError {
    fn from(e: crate::itl::ItlError) -> Self {
        UndoChainError::Itl(e)
    }
}

impl From<crate::segment::SegmentError> for UndoChainError {
    fn from(e: crate::segment::SegmentError) -> Self {
        UndoChainError::Space(SegmentSpaceError::Format(e))
    }
}

/// **Undo 页写入器与链**：绑定 undo 段，把撤销记录追加进 undo 页并维护
/// 事务表槽的链头。**一个 undo 页同一时刻只服务一个事务**（§4.6.1）——
/// 换事务即换新页（旧事务的记录仍在该页上，沿链可达）。
/// 一次链追加的**页级计划**（[`UndoChain::plan_append`] 的产物）。
pub struct UndoAppend {
    /// 撤销页的逻辑页号。
    pub logical: u32,
    /// 是否新开了撤销页。
    pub opened: bool,
    /// 本页归属的事务（状态更新用）。
    pub txn_id: TxnId,
    /// 撤销页的（追加前、追加后）镜像。
    pub undo_page: (Page, Page),
    /// 段内位图页的（前、后）镜像（仅新开页时存在）。
    pub bitmap: Option<(Page, Page)>,
    /// 段头页的（前、后）镜像（槽的 `undo_current`/`rec_count` 已更新）。
    pub header: (Page, Page),
    /// **新链头**（本记录的地址）。
    pub head: RowId,
}

/// undo 段上的链写入器（当前页与归属事务的写作状态）。
pub struct UndoChain<'io, 'f> {
    segment: Segment<'io, 'f>,
    current_logical: Option<u32>,
    current_txn: Option<TxnId>,
}

impl std::fmt::Debug for UndoChain<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UndoChain")
            .field("current_logical", &self.current_logical)
            .field("current_txn", &self.current_txn)
            .finish_non_exhaustive()
    }
}

impl<'io, 'f> UndoChain<'io, 'f> {
    /// 打开 undo 段上的链写入器（当前页留空——首次 `append` 时分配）。
    pub fn open(segment: Segment<'io, 'f>) -> Self {
        Self {
            segment,
            current_logical: None,
            current_txn: None,
        }
    }

    /// 只读段视图（诊断/测试）。
    #[must_use]
    pub fn segment(&self) -> &Segment<'io, 'f> {
        &self.segment
    }

    /// **分配一个事务表槽**（事务开始；持久化段头页）。
    pub fn allocate_slot(&mut self) -> Result<u16, UndoChainError> {
        let mut page = self.segment.read_page(0)?;
        let (index, _) = allocate_slot(&mut page)?;
        self.segment.write_page(0, &mut page)?;
        Ok(index)
    }

    /// **追加一条撤销记录**：链头来自事务表槽的 `undo_current`；
    /// 页归属不符或放不下则新开 undo 页；最后更新槽的链头与计数。
    /// **计划一次链追加**（**不写盘**）：返回要写的页镜像与前后像——
    /// 写路径经缓冲池落盘（为每页生成 redo 并标脏，§11.1.2"undo 自身受
    /// redo 保护"的落点）；恢复/测试走 [`UndoChain::append`] 的直写形态。
    ///
    /// 若需要新开撤销页且段内无空闲逻辑页，会**直写**扩展段（段头页与位图
    /// 页——`Segment::extend` 的既有行为）；其余页一律只返回镜像。
    pub fn plan_append(
        &mut self,
        slot_index: u16,
        op: UndoOp,
        flags: u8,
        rowid: RowId,
        payload: UndoPayload,
    ) -> Result<UndoAppend, UndoChainError> {
        let mut header_before = self.segment.read_page(0)?;
        let mut txn_slot = read_slot(&header_before, slot_index)?;
        if txn_slot.state == TxnState::Free {
            return Err(UndoChainError::SlotNotActive(slot_index));
        }
        let txn_id = txn_id_of(slot_index, &txn_slot);
        let record = UndoRecord {
            prev: txn_slot.undo_current,
            op,
            flags,
            rowid,
            payload,
        };
        let len = record.encode().len();

        // 复用当前页（限于同一事务）或新开一页。
        let reuse = match (self.current_logical, self.current_txn) {
            (Some(logical), Some(txn)) if txn == txn_id => {
                let page = self.segment.read_page(logical)?;
                fits(&page, len)
            }
            _ => false,
        };
        let mut bitmap = None;
        let (logical, undo_before, opened) = if reuse {
            let logical = self.current_logical.expect("已判定存在");
            let page = self.segment.read_page(logical)?;
            (logical, page, false)
        } else {
            // 新页：逻辑页号 = 段头 `追加位置`；必要时（直写）扩展段。
            let mut seg_header = read_header(&header_before)?;
            let logical = seg_header.append_pos;
            if logical >= BITMAP_PAGE_COVERAGE {
                return Err(UndoChainError::BitmapCoverage);
            }
            if self.segment.logical_block(logical).is_none() {
                self.segment.extend()?;
                header_before = self.segment.read_page(0)?;
                txn_slot = read_slot(&header_before, slot_index)?;
            }
            let block = self
                .segment
                .logical_block(logical)
                .ok_or(SegmentSpaceError::BitmapCoverage)?;
            let mut page = Page::new(
                PageType::Undo,
                self.segment.workspace_ref(),
                self.segment.file_id(),
                block,
            );
            crate::itl::write_itl(
                &mut page,
                0,
                &crate::itl::ItlEntry {
                    txn_id,
                    undo_ptr: None,
                    commit_seq: None,
                    lock_cnt: 0,
                    state: crate::itl::ItlState::Active,
                },
            )?;
            seg_header = read_header(&header_before)?;
            seg_header.append_pos = logical + 1;
            write_header(&mut header_before, &seg_header)?;
            // 段内位图：新页 → High（仍在首个位图页覆盖内）。
            let bmp_before = self.segment.read_page(1)?;
            let mut bmp_after = Page::from_bytes(Box::new(*bmp_before.as_bytes()));
            crate::bitmap::set_free_level(&mut bmp_after, logical, crate::bitmap::FreeLevel::High)?;
            bitmap = Some((bmp_before, bmp_after));
            (logical, page, true)
        };

        let mut undo_after = Page::from_bytes(Box::new(*undo_before.as_bytes()));
        let slot_no = if opened {
            // 新页由上面就地构造——直接在其上追加。
            put_record(&mut undo_after, &record)?
        } else {
            put_record(&mut undo_after, &record)?
        };
        let block = self
            .segment
            .logical_block(logical)
            .ok_or(SegmentSpaceError::BitmapCoverage)?;
        let head = RowId::from_parts(self.segment.file_id(), block, slot_no)
            .map_err(|_| UndoError::MalformedRecord)?;

        let mut header_after = Page::from_bytes(Box::new(*header_before.as_bytes()));
        txn_slot.undo_current = Some(head);
        txn_slot.rec_count = txn_slot.rec_count.saturating_add(1);
        write_slot(&mut header_after, slot_index, &txn_slot)?;

        Ok(UndoAppend {
            logical,
            opened,
            txn_id,
            undo_page: (undo_before, undo_after),
            bitmap,
            header: (header_before, header_after),
            head,
        })
    }

    /// 链写作器状态更新（`plan_append` 之后必须调用）。
    pub fn note_append(&mut self, plan: &UndoAppend) {
        if plan.opened {
            self.current_logical = Some(plan.logical);
            self.current_txn = Some(plan.txn_id);
        }
    }

    /// **追加一条撤销记录并直写落盘**（恢复/测试路径；写路径用
    /// [`UndoChain::plan_append`] 经缓冲池）。
    pub fn append(
        &mut self,
        slot_index: u16,
        op: UndoOp,
        flags: u8,
        rowid: RowId,
        payload: UndoPayload,
    ) -> Result<RowId, UndoChainError> {
        let plan = self.plan_append(slot_index, op, flags, rowid, payload)?;
        if let Some((_, after)) = &plan.bitmap {
            let mut p = Page::from_bytes(Box::new(*after.as_bytes()));
            self.segment.write_page(1, &mut p)?;
        }
        if plan.opened {
            let mut p = Page::from_bytes(Box::new(*plan.undo_page.1.as_bytes()));
            self.segment.write_page(plan.logical, &mut p)?;
        }
        let mut p = Page::from_bytes(Box::new(*plan.undo_page.1.as_bytes()));
        self.segment.write_page(plan.logical, &mut p)?;
        let mut h = Page::from_bytes(Box::new(*plan.header.1.as_bytes()));
        self.segment.write_page(0, &mut h)?;
        self.note_append(&plan);
        Ok(plan.head)
    }

    /// **按 `txn_id` 查事务表槽**（一致性读的可见性判定用）：`None` =
    /// 槽已复用（`wrap` 不符）——在 CR 里即"**必已提交且旧于一切有效快照**"。
    pub fn lookup(&self, txn_id: TxnId) -> Result<Option<TxnSlot>, UndoChainError> {
        let page = self.segment.read_page(0)?;
        Ok(find_slot(&page, txn_id)?)
    }

    /// **按位置读一条撤销记录**（经区映射反查逻辑页）。
    pub fn read(&self, at: RowId) -> Result<UndoRecord, UndoChainError> {
        let logical = self
            .segment
            .logical_of_block(at.block_id())
            .ok_or(UndoChainError::Undo(UndoError::MalformedRecord))?;
        let page = self.segment.read_page(logical)?;
        Ok(get_record(&page, at.row_id())?)
    }
}

// ---------------------------------------------------------------------------
// 回滚：撤销记录的补偿动作与链回放（§4.6.2 的"撤销动作"列）
// ---------------------------------------------------------------------------

/// 数据页定位器：`rdba` →（已打开的页文件句柄，块号）。
pub type PageResolver<'a> = dyn FnMut(Rdba) -> Option<(FileHandle, u32)> + 'a;

/// 回滚错误（含 I/O 与页级错误；与格式错误 [`UndoError`] 分开）。
#[derive(Debug)]
pub enum RollbackError {
    /// 底层 I/O。
    Io(std::io::Error),
    /// 目标页损坏/越界。
    Page(&'static str),
    /// 定位器给不出该块。
    Unresolved(Rdba),
    /// 撤销记录格式错误。
    Undo(UndoError),
    /// 数据页操作错误（行不存在等）。
    Heap(crate::heap::HeapError),
    /// **槽已被复用**（"删除"的原位恢复落不了笔）——单写者语义下不会发生；
    /// 并发写者下由"未提交删除的槽不得复用"规则兜住（随后切片钉住）。
    SlotReused {
        /// 目标行。
        rowid: RowId,
    },
    /// 目标区域已被占用（defrag 之后原位恢复失效）。
    RegionOccupied {
        /// 目标行。
        rowid: RowId,
    },
    /// **更新类撤销需要行布局（定长区宽度）**——由行/更新切片接入。
    UpdateNeedsLayout,
}

impl std::fmt::Display for RollbackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RollbackError::Io(e) => write!(f, "回滚 I/O：{e}"),
            RollbackError::Page(s) => write!(f, "回滚页错误：{s}"),
            RollbackError::Unresolved(r) => {
                write!(
                    f,
                    "回滚块无法定位（文件 {} 块 {}）",
                    r.file_id(),
                    r.block_id()
                )
            }
            RollbackError::Undo(e) => write!(f, "{e}"),
            RollbackError::Heap(e) => write!(f, "回滚行操作：{e}"),
            RollbackError::SlotReused { rowid } => {
                write!(f, "回滚目标行 {rowid} 的槽已被复用")
            }
            RollbackError::RegionOccupied { rowid } => {
                write!(f, "回滚目标行 {rowid} 的区域已被占用（defrag？）")
            }
            RollbackError::UpdateNeedsLayout => {
                f.write_str("更新类撤销需要行布局（定长区宽度）——行/更新切片接入")
            }
        }
    }
}

impl std::error::Error for RollbackError {}

impl From<std::io::Error> for RollbackError {
    fn from(e: std::io::Error) -> Self {
        RollbackError::Io(e)
    }
}

impl From<UndoError> for RollbackError {
    fn from(e: UndoError) -> Self {
        RollbackError::Undo(e)
    }
}

impl From<crate::heap::HeapError> for RollbackError {
    fn from(e: crate::heap::HeapError) -> Self {
        RollbackError::Heap(e)
    }
}

fn page_err(e: crate::pagefile::PageFileError) -> RollbackError {
    match e {
        crate::pagefile::PageFileError::Io(e) => RollbackError::Io(e),
        crate::pagefile::PageFileError::Damaged { .. } => RollbackError::Page("页损坏"),
    }
}

/// **回滚一条撤销记录**：把它的补偿动作应用到 `record.rowid` 指向的数据页。
///
/// | `op` | 补偿动作 |
/// | --- | --- |
/// | 插入 | 删除该行（槽位置空闲） |
/// | 删除 | 整行旧值**原位**写回原槽（槽必须仍空闲、区域未被 defrag 占用） |
/// | 转发指针更新 | 恢复旧目标（槽回 `Forwarding`） |
/// | ITL 覆盖 | 还原被覆盖的 ITL（或恢复为空闲）——**块级**动作 |
/// | 更新 | **需要行布局**——由行/更新切片接入（本切片明确拒绝） |
pub fn rollback_record(
    io: &dyn FileIo,
    record: &UndoRecord,
    resolve: &mut PageResolver<'_>,
) -> Result<(), RollbackError> {
    let rdba = Rdba::from_parts(record.rowid.file_id(), record.rowid.block_id())
        .ok_or(RollbackError::Undo(UndoError::MalformedRecord))?;
    let (handle, block) = resolve(rdba).ok_or(RollbackError::Unresolved(rdba))?;
    let mut page = crate::pagefile::read_page_verified(io, handle, block).map_err(page_err)?;
    apply_undo_to_page(&mut page, record)?;
    crate::pagefile::write_page(io, handle, block, &mut page).map_err(RollbackError::Io)?;
    Ok(())
}

/// **页级补偿动作**（无 I/O）：把一条撤销记录的补偿直接作用在给定页上——
/// 回滚（写盘路径）与**一致性读的 CR 重建**（内存副本）共用这一处实现。
///
/// **幂等**（§4.6.6 ③ 的两条结论依赖它：回滚中断 = 重走整链、恢复的撤销
/// 阶段就是普通回滚的重放）：补偿都是"把某处置回旧值"，重复应用不产生
/// 额外效果——已"确保不存在"的行再删 = 无操作；已原位恢复的行再写 = 无操作。
pub fn apply_undo_to_page(page: &mut Page, record: &UndoRecord) -> Result<(), RollbackError> {
    let row_no = record.rowid.row_id();
    let index = crate::heap::slot_index(row_no)
        .ok_or(RollbackError::Undo(UndoError::SlotOutOfRange(row_no)))?;

    match record.op {
        UndoOp::Insert => {
            // "确保不存在"：槽已空闲（或越界）说明这条撤销已生效过——幂等返回。
            match crate::heap::slot_status(page, row_no) {
                None | Some(crate::page::SlotStatus::Free) => {}
                Some(_) => crate::heap::delete_row(page, row_no)?,
            }
        }
        UndoOp::Delete => {
            let bytes = match &record.payload {
                UndoPayload::FullRow(bytes) if !bytes.is_empty() => bytes,
                _ => return Err(RollbackError::Undo(UndoError::MalformedRecord)),
            };
            let Some(entry) = page.slot(index) else {
                // 槽不在本页（`slot_count` 之外）：这个页状态**早于本记录**
                // （PITR 的有界重放会让页停在目标点，而链覆盖到目标点之后），
                // 行本来就不在 ⇒ 已在前像，空操作。
                return Ok(());
            };
            if entry.status() != crate::page::SlotStatus::Free {
                // 槽被占着：**行字节与要恢复的旧值逐字节相同** ⇒ 是本补偿
                // 已生效过（幂等）；否则是**槽已复用**——拒绝，不猜。
                if crate::heap::row(page, row_no) == Some(bytes.as_slice()) {
                    return Ok(());
                }
                return Err(RollbackError::SlotReused {
                    rowid: record.rowid,
                });
            }
            let offset = entry.offset() as usize;
            if offset + bytes.len() > crate::page::PAGE_SIZE {
                return Err(RollbackError::Undo(UndoError::MalformedRecord));
            }
            if offset < page.free_end() {
                // 行区只向下生长；原位恢复要求该区域仍在空闲侧（defrag 会破坏）。
                return Err(RollbackError::RegionOccupied {
                    rowid: record.rowid,
                });
            }
            page.as_bytes_mut()[offset..offset + bytes.len()].copy_from_slice(bytes);
            let restored =
                crate::page::SlotEntry::new(offset as u16, crate::page::SlotStatus::Normal)
                    .ok_or(RollbackError::Undo(UndoError::MalformedRecord))?;
            page.set_slot(index, restored);
        }
        UndoOp::Forward => {
            let target = match &record.payload {
                UndoPayload::Forward(t) => *t,
                _ => return Err(RollbackError::Undo(UndoError::MalformedRecord)),
            };
            let Some(entry) = page.slot(index) else {
                return Ok(()); // 页状态早于本记录（同"删除"的说明）：空操作
            };
            let offset = entry.offset() as usize;
            if offset + crate::rowid::ROWID_LEN > crate::page::PAGE_SIZE {
                return Err(RollbackError::Undo(UndoError::MalformedRecord));
            }
            page.as_bytes_mut()[offset..offset + crate::rowid::ROWID_LEN]
                .copy_from_slice(&target.to_bytes());
            let restored =
                crate::page::SlotEntry::new(offset as u16, crate::page::SlotStatus::Forwarding)
                    .ok_or(RollbackError::Undo(UndoError::MalformedRecord))?;
            page.set_slot(index, restored);
        }
        UndoOp::ItlOverwrite => {
            let (itl_slot, old) = match &record.payload {
                UndoPayload::ItlOverwrite { itl_slot, old } => (*itl_slot, old),
                _ => return Err(RollbackError::Undo(UndoError::MalformedRecord)),
            };
            // 该 ITL 槽不在本页（`itl_count` 之外）：页状态早于"占用记录"
            // ——占用（连同可能的槽扩展）没有被重放到这个页 ⇒ 已在前像，空操作。
            let count = crate::itl::itl_count(page).map_err(|e| {
                RollbackError::Page(match e {
                    crate::itl::ItlError::SlotOutOfRange(_) => "ITL 槽越界",
                    _ => "ITL 字段越界",
                })
            })?;
            if u16::from(itl_slot) >= count {
                return Ok(());
            }
            match old {
                Some(bytes) => {
                    crate::itl::restore(page, u16::from(itl_slot), bytes).map_err(|e| {
                        RollbackError::Page(match e {
                            crate::itl::ItlError::SlotOutOfRange(_) => "ITL 槽越界",
                            _ => "ITL 字段越界",
                        })
                    })?
                }
                None => {
                    crate::itl::write_itl(page, u16::from(itl_slot), &crate::itl::ItlEntry::FREE)
                        .map_err(|e| {
                            RollbackError::Page(match e {
                                crate::itl::ItlError::SlotOutOfRange(_) => "ITL 槽越界",
                                _ => "ITL 字段越界",
                            })
                        })?
                }
            }
        }
        UndoOp::Update => {
            let (old_itl_slot, patches) = match &record.payload {
                UndoPayload::Update {
                    old_itl_slot,
                    patches,
                } => (*old_itl_slot, patches),
                _ => return Err(RollbackError::Undo(UndoError::MalformedRecord)),
            };
            let Some(entry) = page.slot(index) else {
                return Ok(()); // 页状态早于本记录（PITR 有界重放）：空操作
            };
            if entry.status() == crate::page::SlotStatus::Free {
                // 行不在（已删/未重放）——前像即"没有这一行"，空操作。
                return Ok(());
            }
            let base = usize::from(entry.offset());
            for (off, old) in patches {
                let at = base + usize::from(*off);
                if at + old.len() > crate::page::PAGE_SIZE || at < base {
                    return Err(RollbackError::Undo(UndoError::MalformedRecord));
                }
                page.as_bytes_mut()[at..at + old.len()].copy_from_slice(old);
            }
            // 行头 `itl_slot`（偏移 1）也要还原（§4.6.2："行头字节也进 undo"）。
            if base + 1 >= crate::page::PAGE_SIZE {
                return Err(RollbackError::Undo(UndoError::MalformedRecord));
            }
            page.as_bytes_mut()[base + 1] = old_itl_slot;
        }
    }
    Ok(())
}

/// **沿链回滚**：从 `head` 起沿 `prev_undo` 走到底，逐条执行补偿动作。
///
/// 返回回放的记录数。
pub fn rollback_chain(
    io: &dyn FileIo,
    chain: &UndoChain<'_, '_>,
    head: Option<RowId>,
    resolve: &mut PageResolver<'_>,
) -> Result<u64, RollbackError> {
    let mut at = head;
    let mut count = 0u64;
    while let Some(pos) = at {
        let record = chain.read(pos).map_err(|e| {
            RollbackError::Page(match e {
                UndoChainError::Undo(e) => {
                    let _ = e;
                    "撤销记录读取失败"
                }
                _ => "撤销记录读取失败",
            })
        })?;
        rollback_record(io, &record, resolve)?;
        at = record.prev;
        count += 1;
    }
    Ok(count)
}

/// **回滚一个事务**：取事务表槽的链头 → 沿链回放 → 槽置空闲（`wrap + 1`）。
pub fn rollback_transaction(
    io: &dyn FileIo,
    chain: &UndoChain<'_, '_>,
    slot_index: u16,
    resolve: &mut PageResolver<'_>,
) -> Result<u64, RollbackError> {
    let page = chain
        .segment()
        .read_page(0)
        .map_err(|_| RollbackError::Page("undo 段头页不可读"))?;
    let head = read_slot(&page, slot_index)
        .map_err(RollbackError::Undo)?
        .undo_current;
    let count = rollback_chain(io, chain, head, resolve)?;

    // 事务表槽收尾：置空闲（释放时推进 wrap）。
    let mut page = chain
        .segment()
        .read_page(0)
        .map_err(|_| RollbackError::Page("undo 段头页不可读"))?;
    free_slot(&mut page, slot_index).map_err(RollbackError::Undo)?;
    chain
        .segment()
        .write_page(0, &mut page)
        .map_err(|_| RollbackError::Page("undo 段头页不可写"))?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use bicdb_workspace::io::MemFileIo;

    use super::*;
    use crate::datafile::DataFile;
    use crate::segment::entries_offset;

    const F: &str = "/mem/undo1.dat";
    const WS: [u8; 8] = [1u8; 8];

    fn mem() -> MemFileIo {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        io
    }

    fn rid(block: u32, slot: u16) -> RowId {
        RowId::from_parts(1, block, slot).unwrap()
    }

    #[test]
    fn txn_id_three_segment_roundtrip() {
        let id = TxnId::from_parts(7, 200, 0xDEAD_BEEF);
        assert_eq!(id.usn(), 7);
        assert_eq!(id.slot(), 200);
        assert_eq!(id.wrap(), 0xDEAD_BEEF);
        assert_eq!(TxnId::from_bytes(&id.to_bytes()), id);
        let max = TxnId::from_parts(255, 255, u32::MAX);
        assert_eq!(TxnId::from_bytes(&max.to_bytes()), max);
        assert_eq!(TxnId::from_parts(0, 0, 0).to_bytes(), [0u8; 6]);
    }

    #[test]
    fn undo_segment_init_allocate_and_free() {
        let io = mem();
        let mut file = DataFile::create(&io, Path::new(F), 1, 1, WS, 512).unwrap();
        let seg = create_undo_segment(&mut file, 2, 3, 4).unwrap();

        // 事务表与段控制就位。
        let page = seg.read_page(0).unwrap();
        let control = read_control(&page).unwrap();
        assert_eq!(control.slot_count, TXN_SLOTS as u16);
        assert_eq!(control.free_head, 0, "空闲链从槽 0 起");
        assert_eq!(control.commit_head, NO_SLOT);
        assert_eq!(control.reclaim_seq.as_raw(), 0);
        let s0 = read_slot(&page, 0).unwrap();
        assert_eq!(s0.state, TxnState::Free);
        assert_eq!(s0.next_free, 1);
        assert_eq!(s0.wrap, 0);
        let s255 = read_slot(&page, 255).unwrap();
        assert_eq!(s255.next_free, NO_SLOT);

        // 分配两个槽 → Active、空闲链前移。
        let mut page = seg.read_page(0).unwrap();
        let (i0, slot0) = allocate_slot(&mut page).unwrap();
        assert_eq!(i0, 0);
        assert_eq!(slot0.state, TxnState::Active);
        let (i1, _) = allocate_slot(&mut page).unwrap();
        assert_eq!(i1, 1);
        assert_eq!(read_control(&page).unwrap().free_head, 2);
        seg.write_page(0, &mut page).unwrap();

        // 由 txn_id 直接定位；wrap 不符 ⇒ 不是自己。
        let page = seg.read_page(0).unwrap();
        let id0 = txn_id_of(0, &slot0);
        assert_eq!(
            find_slot(&page, id0).unwrap().unwrap().state,
            TxnState::Active
        );
        let stale = TxnId::from_parts(0, 0, 99);
        assert!(find_slot(&page, stale).unwrap().is_none(), "陈旧代次被判定");
        assert!(
            find_slot(&page, TxnId::from_parts(1, 0, 0))
                .unwrap()
                .is_none(),
            "V1.0 单段"
        );

        // 释放槽 0：wrap +1、压回链头；老 txn_id 从此找不到。
        let mut page = seg.read_page(0).unwrap();
        free_slot(&mut page, 0).unwrap();
        let s0 = read_slot(&page, 0).unwrap();
        assert_eq!((s0.state, s0.wrap, s0.next_free), (TxnState::Free, 1, 2));
        assert_eq!(read_control(&page).unwrap().free_head, 0);
        assert!(find_slot(&page, id0).unwrap().is_none(), "槽已换人");
        let (i2, slot2) = allocate_slot(&mut page).unwrap();
        assert_eq!((i2, slot2.wrap), (0, 1), "再分配取回槽 0、wrap 已推进");
    }

    #[test]
    fn undo_layout_offsets_are_pinned() {
        assert_eq!(TXN_TABLE_LEN, 6144);
        assert_eq!(UNDO_EXTENSION_LEN, 6168);
        assert_eq!(UNDO_CONTROL_OFFSET, 128 + 6144);
        assert_eq!(
            entries_offset(crate::segment::SegType::Undo).unwrap(),
            6296,
            "区映射条目在事务表 + 段控制之后"
        );
        assert_eq!(entries_capacity_undo(), (16380 - 6296) / 7);

        fn entries_capacity_undo() -> usize {
            (crate::page::PAGE_SIZE - 4 - 6296) / crate::segment::EXTENT_ENTRY_LEN
        }
    }

    #[test]
    fn undo_record_roundtrip_all_ops() {
        let insert = UndoRecord {
            prev: None,
            op: UndoOp::Insert,
            flags: 0,
            rowid: rid(9, 3),
            payload: UndoPayload::None,
        };
        assert_eq!(UndoRecord::decode(&insert.encode()).unwrap(), insert);
        assert_eq!(insert.encode().len(), UNDO_RECORD_HEADER_LEN);

        let delete = UndoRecord {
            prev: Some(rid(9, 2)),
            op: UndoOp::Delete,
            flags: 0,
            rowid: rid(9, 3),
            payload: UndoPayload::FullRow(vec![0xAA; 40]),
        };
        assert_eq!(UndoRecord::decode(&delete.encode()).unwrap(), delete);

        let update = UndoRecord {
            prev: Some(rid(9, 1)),
            op: UndoOp::Update,
            flags: 0,
            rowid: rid(9, 3),
            payload: UndoPayload::Update {
                old_itl_slot: 2,
                patches: vec![(1, b"old-a".to_vec()), (7, vec![])],
            },
        };
        assert_eq!(UndoRecord::decode(&update.encode()).unwrap(), update);

        let forward = UndoRecord {
            prev: None,
            op: UndoOp::Forward,
            flags: UNDO_FLAG_MULTI_HEAD | UNDO_FLAG_MULTI_TAIL,
            rowid: rid(9, 3),
            payload: UndoPayload::Forward(rid(10, 1)),
        };
        let decoded = UndoRecord::decode(&forward.encode()).unwrap();
        assert_eq!(decoded, forward);
        assert_eq!(decoded.flags, 0x03);

        let itl = UndoRecord {
            prev: None,
            op: UndoOp::ItlOverwrite,
            flags: 0,
            rowid: rid(9, 3),
            payload: UndoPayload::ItlOverwrite {
                itl_slot: 3,
                old: Some([7u8; ITL_ENTRY_LEN]),
            },
        };
        assert_eq!(UndoRecord::decode(&itl.encode()).unwrap(), itl);
        let itl_free = UndoRecord {
            payload: UndoPayload::ItlOverwrite {
                itl_slot: 0,
                old: None,
            },
            ..itl.clone()
        };
        assert_eq!(UndoRecord::decode(&itl_free.encode()).unwrap(), itl_free);
    }

    #[test]
    fn malformed_undo_records_are_rejected() {
        // 长度不足。
        assert_eq!(
            UndoRecord::decode(&[0u8; 5]),
            Err(UndoError::MalformedRecord)
        );
        // 未知 op。
        let mut bytes = UndoRecord {
            prev: None,
            op: UndoOp::Insert,
            flags: 0,
            rowid: rid(9, 1),
            payload: UndoPayload::None,
        }
        .encode();
        bytes[6] = 9;
        assert_eq!(UndoRecord::decode(&bytes), Err(UndoError::UnknownOp(9)));
        // 更新列列表截断。
        let mut bytes = UndoRecord {
            prev: None,
            op: UndoOp::Update,
            flags: 0,
            rowid: rid(9, 1),
            payload: UndoPayload::Update {
                old_itl_slot: 0,
                patches: vec![(1, vec![9, 9, 9])],
            },
        }
        .encode();
        bytes.truncate(bytes.len() - 1);
        assert_eq!(UndoRecord::decode(&bytes), Err(UndoError::MalformedRecord));
        // 插入记录不得带载荷。
        let mut bytes = UndoRecord {
            prev: None,
            op: UndoOp::Insert,
            flags: 0,
            rowid: rid(9, 1),
            payload: UndoPayload::None,
        }
        .encode();
        bytes.push(0);
        assert_eq!(UndoRecord::decode(&bytes), Err(UndoError::MalformedRecord));
    }

    #[test]
    fn non_undo_segment_is_rejected() {
        let mut page = crate::page::Page::new(crate::page::PageType::SegmentHeader, WS, 1, 1);
        let header = crate::segment::SegmentHeader {
            seg_type: crate::segment::SegType::Heap,
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
        };
        crate::segment::write_header(&mut page, &header).unwrap();
        assert_eq!(init_extension(&mut page), Err(UndoError::WrongSegType));
    }
}

#[cfg(test)]
mod chain_tests {
    use std::path::Path;

    use bicdb_workspace::io::MemFileIo;

    use super::*;
    use crate::datafile::DataFile;
    use crate::itl;
    use crate::page::PageType;

    const F: &str = "/mem/undo1.dat";
    const WS: [u8; 8] = [2u8; 8];

    fn rid(block: u32, slot: u16) -> RowId {
        RowId::from_parts(1, block, slot).unwrap()
    }

    fn insert_rec(prev: Option<RowId>, row: u16, payload: &[u8]) -> (UndoOp, UndoPayload) {
        let _ = prev;
        let _ = row;
        (UndoOp::Delete, UndoPayload::FullRow(payload.to_vec()))
    }

    #[test]
    fn chain_appends_links_and_reads_back() {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let mut file = DataFile::create(&io, Path::new(F), 1, 1, WS, 512).unwrap();
        let segment = create_undo_segment(&mut file, 2, 3, 4).unwrap();
        let mut chain = UndoChain::open(segment);

        let slot = chain.allocate_slot().unwrap();
        assert_eq!(slot, 0);
        let (op, payload) = insert_rec(None, 1, &[0x11; 20]);
        let r1 = chain.append(slot, op, 0, rid(3, 1), payload).unwrap();
        let r2 = chain
            .append(slot, UndoOp::Insert, 0, rid(3, 2), UndoPayload::None)
            .unwrap();
        let (op, payload) = insert_rec(None, 3, &[0x33; 5]);
        let r3 = chain.append(slot, op, 0, rid(3, 3), payload).unwrap();
        assert_ne!(r1, r2);
        assert_ne!(r2, r3);

        // 链：r3 → r2 → r1 → 链首；载荷逐条原样。
        let a = chain.read(r3).unwrap();
        assert_eq!(a.prev, Some(r2));
        assert_eq!(a.payload, UndoPayload::FullRow(vec![0x33; 5]));
        let b = chain.read(r2).unwrap();
        assert_eq!(b.prev, Some(r1));
        assert_eq!(b.op, UndoOp::Insert);
        let c = chain.read(r1).unwrap();
        assert_eq!(c.prev, None);
        assert_eq!(c.payload, UndoPayload::FullRow(vec![0x11; 20]));

        // 事务表槽：链头 = r3、计数 3。
        let page = chain.segment().read_page(0).unwrap();
        let txn = read_slot(&page, slot).unwrap();
        assert_eq!(txn.undo_current, Some(r3));
        assert_eq!(txn.rec_count, 3);
        assert_eq!(txn.wrap, 0, "首用槽：wrap = 0");

        // 记录落在逻辑页 2；页 ITL[0] 归属该事务。
        assert_eq!(chain.segment().logical_of_block(r1.block_id()), Some(2));
        let undo_page = chain.segment().read_page(2).unwrap();
        assert_eq!(undo_page.header().unwrap().page_type, PageType::Undo);
        let e = itl::read_itl(&undo_page, 0).unwrap();
        assert_eq!(e.txn_id, txn_id_of(slot, &txn));
        assert_eq!(e.state, itl::ItlState::Active);
    }

    #[test]
    fn second_transaction_gets_its_own_page() {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let mut file = DataFile::create(&io, Path::new(F), 1, 1, WS, 512).unwrap();
        let segment = create_undo_segment(&mut file, 2, 3, 4).unwrap();
        let mut chain = UndoChain::open(segment);

        let t0 = chain.allocate_slot().unwrap();
        let t1 = chain.allocate_slot().unwrap();
        assert_eq!((t0, t1), (0, 1));
        let a = chain
            .append(t0, UndoOp::Insert, 0, rid(3, 1), UndoPayload::None)
            .unwrap();
        let b = chain
            .append(t1, UndoOp::Insert, 0, rid(4, 1), UndoPayload::None)
            .unwrap();
        assert_eq!(chain.segment().logical_of_block(a.block_id()), Some(2));
        assert_eq!(
            chain.segment().logical_of_block(b.block_id()),
            Some(3),
            "换事务 ⇒ 换新页（一页同一时刻只服务一个事务）"
        );
        // 两条链各自独立、互不干扰。
        assert_eq!(chain.read(a).unwrap().prev, None);
        assert_eq!(chain.read(b).unwrap().prev, None);
        let page = chain.segment().read_page(0).unwrap();
        assert_eq!(read_slot(&page, 0).unwrap().undo_current, Some(a));
        assert_eq!(read_slot(&page, 1).unwrap().undo_current, Some(b));
    }

    #[test]
    fn page_rollover_keeps_chain_continuous() {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let mut file = DataFile::create(&io, Path::new(F), 1, 1, WS, 512).unwrap();
        let segment = create_undo_segment(&mut file, 2, 3, 4).unwrap();
        let mut chain = UndoChain::open(segment);
        let slot = chain.allocate_slot().unwrap();

        // 每条 400B 载荷（编码 ≈ 414B）；页可用 16312B ⇒ 约 39 条翻页。
        let mut rids = Vec::new();
        for i in 0..80u8 {
            let rid_rec = chain
                .append(
                    slot,
                    UndoOp::Delete,
                    0,
                    rid(3, u16::from(i) + 1),
                    UndoPayload::FullRow(vec![i; 400]),
                )
                .unwrap();
            rids.push(rid_rec);
        }
        // 跨了三张页（2、3、4）。
        let pages: Vec<Option<u32>> = rids
            .iter()
            .map(|r| chain.segment().logical_of_block(r.block_id()))
            .collect();
        assert_eq!(pages[0], Some(2));
        assert!(pages.contains(&Some(3)) && pages.contains(&Some(4)), "跨页");

        // 从链头往回走：80 条全在、逆序、载荷原样。
        let page = chain.segment().read_page(0).unwrap();
        let head = read_slot(&page, slot).unwrap().undo_current.unwrap();
        assert_eq!(head, *rids.last().unwrap());
        let mut at = Some(head);
        let mut walked = 0usize;
        while let Some(pos) = at {
            let rec = chain.read(pos).unwrap();
            let expect = 79 - walked as u8;
            assert_eq!(rec.payload, UndoPayload::FullRow(vec![expect; 400]));
            at = rec.prev;
            walked += 1;
        }
        assert_eq!(walked, 80);
    }

    #[test]
    fn append_requires_active_slot() {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let mut file = DataFile::create(&io, Path::new(F), 1, 1, WS, 512).unwrap();
        let segment = create_undo_segment(&mut file, 2, 3, 4).unwrap();
        let mut chain = UndoChain::open(segment);
        let err = chain
            .append(0, UndoOp::Insert, 0, rid(3, 1), UndoPayload::None)
            .unwrap_err();
        assert!(matches!(err, UndoChainError::SlotNotActive(0)), "{err}");
    }

    #[test]
    fn record_length_derivation_is_exact() {
        let mut page = crate::page::Page::new(PageType::Undo, WS, 1, 9);
        let r1 = UndoRecord {
            prev: None,
            op: UndoOp::Forward,
            flags: 0,
            rowid: rid(3, 1),
            payload: UndoPayload::Forward(rid(4, 2)),
        };
        let r2 = UndoRecord {
            prev: Some(rid(9, 1)),
            op: UndoOp::Delete,
            flags: 0,
            rowid: rid(3, 2),
            payload: UndoPayload::FullRow(vec![7u8; 9]),
        };
        let s1 = put_record(&mut page, &r1).unwrap();
        let s2 = put_record(&mut page, &r2).unwrap();
        assert_eq!((s1, s2), (1, 2));
        assert_eq!(
            get_record(&page, 1).unwrap(),
            r1,
            "长度 = 初始 free_end − 偏移"
        );
        assert_eq!(
            get_record(&page, 2).unwrap(),
            r2,
            "长度 = 上一条偏移 − 本条偏移"
        );
        assert_eq!(get_record(&page, 3), Err(UndoError::SlotOutOfRange(3)));
    }
}

#[cfg(test)]
mod rollback_tests {
    use std::path::Path;

    use bicdb_workspace::io::MemFileIo;

    use super::*;
    use crate::datafile::DataFile;
    use crate::heap::{self, InsertPolicy};
    use crate::page::{Page, PageType, SlotEntry, SlotStatus, WORKSPACE_REF_LEN};
    use crate::pagefile;
    use crate::row::assemble_row;

    const UNDO_F: &str = "/mem/undo1.dat";
    const DATA_F: &str = "/mem/data.dat";
    const WS: [u8; 8] = [3u8; 8];

    fn mem() -> MemFileIo {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        io
    }

    fn row_bytes(itl_slot: u8, payload: &[u8]) -> Vec<u8> {
        assemble_row(0, itl_slot, &[false], &[], &[payload])
    }

    /// 建数据页文件（堆表页，file 3 block 0），返回句柄。
    fn data_file(io: &dyn FileIo) -> FileHandle {
        let h = pagefile::create(io, Path::new(DATA_F), 1).unwrap();
        let mut page = Page::new(PageType::HeapTable, [0u8; WORKSPACE_REF_LEN], 3, 0);
        pagefile::write_page(io, h, 0, &mut page).unwrap();
        h
    }

    fn load(io: &dyn FileIo, h: FileHandle) -> Page {
        pagefile::read_page_verified(io, h, 0).unwrap()
    }

    fn store(io: &dyn FileIo, h: FileHandle, page: &mut Page) {
        pagefile::write_page(io, h, 0, page).unwrap();
    }

    fn rec(op: UndoOp, payload: UndoPayload, row_no: u16) -> UndoRecord {
        UndoRecord {
            prev: None,
            op,
            flags: 0,
            rowid: RowId::from_parts(3, 0, row_no).unwrap(),
            payload,
        }
    }

    #[test]
    fn insert_and_delete_rollback_are_inverse() {
        let io = mem();
        let h = data_file(&io);
        let bytes = row_bytes(1, b"alpha");

        // 插入一行 → Insert 的补偿 = 删除它。
        {
            let mut page = load(&io, h);
            let n = heap::insert_row(&mut page, &bytes, &InsertPolicy::in_place(0)).unwrap();
            assert_eq!(n, 1);
            store(&io, h, &mut page);
        }
        let mut resolve = |r: Rdba| (r.file_id() == 3).then_some((h, 0));
        rollback_record(
            &io,
            &rec(UndoOp::Insert, UndoPayload::None, 1),
            &mut resolve,
        )
        .unwrap();
        let page = load(&io, h);
        assert_eq!(
            page.slot(0).unwrap().status(),
            SlotStatus::Free,
            "插入已被撤销"
        );
        assert_eq!(heap::row(&page, 1), None);

        // 删除一行 → Delete 的补偿 = 整行旧值原位写回。
        {
            let mut page = load(&io, h);
            let n = heap::insert_row(&mut page, &bytes, &InsertPolicy::in_place(0)).unwrap();
            heap::delete_row(&mut page, n).unwrap();
            store(&io, h, &mut page);
        }
        let restore = rec(UndoOp::Delete, UndoPayload::FullRow(bytes.clone()), 1);
        rollback_record(&io, &restore, &mut resolve).unwrap();
        let page = load(&io, h);
        assert_eq!(page.slot(0).unwrap().status(), SlotStatus::Normal);
        assert_eq!(heap::row(&page, 1), Some(&bytes[..]), "旧值原位恢复");
    }

    #[test]
    fn delete_rollback_refuses_when_slot_reused() {
        let io = mem();
        let h = data_file(&io);
        {
            let mut page = load(&io, h);
            // 槽里是**别人**的行（字节不同）——原位恢复必须拒绝，不得冒充幂等。
            heap::insert_row(&mut page, &row_bytes(1, b"new"), &InsertPolicy::in_place(0)).unwrap();
            store(&io, h, &mut page);
        }
        let mut resolve = |r: Rdba| (r.file_id() == 3).then_some((h, 0));
        let err = rollback_record(
            &io,
            &rec(
                UndoOp::Delete,
                UndoPayload::FullRow(row_bytes(1, b"old")),
                1,
            ),
            &mut resolve,
        )
        .unwrap_err();
        assert!(matches!(err, RollbackError::SlotReused { .. }), "{err}");
    }

    #[test]
    fn compensations_are_idempotent() {
        let io = mem();
        let h = data_file(&io);
        let bytes = row_bytes(1, b"alpha");

        // 插入的补偿：删两次都成功（第二次是"确保不存在"的幂等空操作）。
        {
            let mut page = load(&io, h);
            heap::insert_row(&mut page, &bytes, &InsertPolicy::in_place(0)).unwrap();
            store(&io, h, &mut page);
        }
        let mut resolve = |r: Rdba| (r.file_id() == 3).then_some((h, 0));
        let insert_undo = rec(UndoOp::Insert, UndoPayload::None, 1);
        rollback_record(&io, &insert_undo, &mut resolve).unwrap();
        rollback_record(&io, &insert_undo, &mut resolve).unwrap(); // 重复应用
        let page = load(&io, h);
        assert_eq!(page.slot(0).unwrap().status(), SlotStatus::Free);

        // 删除的补偿：恢复两次都成功（第二次逐字节相同 ⇒ 幂等空操作）。
        {
            let mut page = load(&io, h);
            let n = heap::insert_row(&mut page, &bytes, &InsertPolicy::in_place(0)).unwrap();
            heap::delete_row(&mut page, n).unwrap();
            store(&io, h, &mut page);
        }
        let delete_undo = rec(UndoOp::Delete, UndoPayload::FullRow(bytes.clone()), 1);
        rollback_record(&io, &delete_undo, &mut resolve).unwrap();
        rollback_record(&io, &delete_undo, &mut resolve).unwrap(); // 重复应用
        let page = load(&io, h);
        assert_eq!(heap::row(&page, 1), Some(&bytes[..]), "仍是恢复后的那份");

        // 转发指针的补偿同样幂等。
        {
            let mut page = load(&io, h);
            let offset = page.free_end() - 6;
            page.as_bytes_mut()[offset..offset + 6].copy_from_slice(&[0u8; 6]);
            page.set_free_end(offset);
            page.set_slot(
                0,
                SlotEntry::new(offset as u16, SlotStatus::Forwarding).unwrap(),
            );
            store(&io, h, &mut page);
        }
        let target = RowId::from_parts(3, 0, 7).unwrap();
        let forward_undo = rec(UndoOp::Forward, UndoPayload::Forward(target), 1);
        rollback_record(&io, &forward_undo, &mut resolve).unwrap();
        rollback_record(&io, &forward_undo, &mut resolve).unwrap();
        let page = load(&io, h);
        assert_eq!(page.slot(0).unwrap().status(), SlotStatus::Forwarding);
        assert_eq!(
            &page.as_bytes()[page.slot(0).unwrap().offset() as usize..][..6],
            &target.to_bytes()
        );
    }

    #[test]
    fn forward_and_itl_overwrite_rollback() {
        let io = mem();
        let h = data_file(&io);
        let target = RowId::from_parts(3, 0, 7).unwrap();

        // 转发指针：槽里放 6B 目标 + Forwarding 状态。
        {
            let mut page = load(&io, h);
            let offset = page.free_end() - 6;
            page.as_bytes_mut()[offset..offset + 6].copy_from_slice(&target.to_bytes());
            page.set_free_end(offset);
            page.set_slot_count(1).unwrap();
            page.set_slot(
                0,
                SlotEntry::new(offset as u16, SlotStatus::Forwarding).unwrap(),
            );
            store(&io, h, &mut page);
        }
        // 更新路径把指针改掉，回滚恢复旧目标。
        {
            let mut page = load(&io, h);
            let entry = page.slot(0).unwrap();
            let at = entry.offset() as usize;
            page.as_bytes_mut()[at..at + 6]
                .copy_from_slice(&RowId::from_parts(4, 1, 2).unwrap().to_bytes());
            store(&io, h, &mut page);
        }
        let mut resolve = |r: Rdba| (r.file_id() == 3).then_some((h, 0));
        rollback_record(
            &io,
            &rec(UndoOp::Forward, UndoPayload::Forward(target), 1),
            &mut resolve,
        )
        .unwrap();
        let page = load(&io, h);
        assert_eq!(page.slot(0).unwrap().status(), SlotStatus::Forwarding);
        let at = page.slot(0).unwrap().offset() as usize;
        assert_eq!(&page.as_bytes()[at..at + 6], &target.to_bytes());

        // ITL 覆盖：被覆盖的旧内容原样还原（含"原为空闲"）。
        {
            let mut page = load(&io, h);
            let old = crate::itl::read_itl(&page, 0).unwrap();
            let mut prev = old;
            prev.state = crate::itl::ItlState::Committed;
            prev.commit_seq = Some(CommitSeq::from_raw(11).unwrap());
            crate::itl::write_itl(&mut page, 0, &prev).unwrap();
            // 新事务覆盖（活动）→ 回滚恢复成 prev。
            crate::itl::write_itl(
                &mut page,
                0,
                &crate::itl::ItlEntry {
                    txn_id: crate::undo::TxnId::from_parts(0, 1, 0),
                    undo_ptr: None,
                    commit_seq: None,
                    lock_cnt: 0,
                    state: crate::itl::ItlState::Active,
                },
            )
            .unwrap();
            store(&io, h, &mut page);
            let mut snap = [0u8; crate::page::ITL_ENTRY_LEN];
            prev.encode(&mut snap);
            rollback_record(
                &io,
                &rec(
                    UndoOp::ItlOverwrite,
                    UndoPayload::ItlOverwrite {
                        itl_slot: 0,
                        old: Some(snap),
                    },
                    1,
                ),
                &mut resolve,
            )
            .unwrap();
            let page = load(&io, h);
            assert_eq!(crate::itl::read_itl(&page, 0).unwrap(), prev, "旧 ITL 还原");
        }
        // "原为空闲"：回滚把槽恢复成空闲。
        {
            let mut page = load(&io, h);
            crate::itl::write_itl(
                &mut page,
                0,
                &crate::itl::ItlEntry {
                    txn_id: crate::undo::TxnId::from_parts(0, 2, 0),
                    undo_ptr: None,
                    commit_seq: None,
                    lock_cnt: 0,
                    state: crate::itl::ItlState::Active,
                },
            )
            .unwrap();
            store(&io, h, &mut page);
            rollback_record(
                &io,
                &rec(
                    UndoOp::ItlOverwrite,
                    UndoPayload::ItlOverwrite {
                        itl_slot: 0,
                        old: None,
                    },
                    1,
                ),
                &mut resolve,
            )
            .unwrap();
            let page = load(&io, h);
            assert_eq!(
                crate::itl::read_itl(&page, 0).unwrap(),
                crate::itl::ItlEntry::FREE
            );
        }
    }

    #[test]
    fn update_rollback_restores_patches_and_itl_slot() {
        // 等长就地更新：补偿 = 按**行内偏移**写回旧值 + 行头 `itl_slot` 还原。
        let io = mem();
        let h = data_file(&io);
        let original = row_bytes(1, b"v1");
        {
            let mut page = load(&io, h);
            heap::insert_row(&mut page, &original, &InsertPolicy::in_place(0)).unwrap();
            store(&io, h, &mut page);
        }
        // "更新"：改 itl_slot 字节（1 → 7）+ 改一个数据字节（行内偏移 11）。
        let mut updated = original.clone();
        updated[1] = 7;
        updated[11] ^= 0xFF;
        {
            let mut page = load(&io, h);
            let offset = page.slot(0).unwrap().offset() as usize;
            page.as_bytes_mut()[offset..offset + updated.len()].copy_from_slice(&updated);
            store(&io, h, &mut page);
        }
        let mut resolve = |r: Rdba| (r.file_id() == 3).then_some((h, 0));
        let r = rec(
            UndoOp::Update,
            UndoPayload::Update {
                old_itl_slot: 1,
                patches: vec![(11, vec![original[11]])],
            },
            1,
        );
        rollback_record(&io, &r, &mut resolve).unwrap();
        let page = load(&io, h);
        assert_eq!(heap::row(&page, 1), Some(&original[..]), "行逐字节还原");

        // 行不在（槽空闲）⇒ 空操作（PITR/幂等家族）。
        {
            let mut page = load(&io, h);
            heap::delete_row(&mut page, 1).unwrap();
            store(&io, h, &mut page);
        }
        rollback_record(&io, &r, &mut resolve).unwrap();
        let page = load(&io, h);
        assert_eq!(page.slot(0).unwrap().status(), SlotStatus::Free);
    }

    #[test]
    fn rollback_transaction_replays_the_chain_and_frees_slot() {
        let io = mem();
        let h = data_file(&io);
        let bytes = row_bytes(1, b"beta");

        // undo 段 + 链 + 事务槽。
        let mut file = DataFile::create(&io, Path::new(UNDO_F), 1, 1, WS, 512).unwrap();
        let segment = create_undo_segment(&mut file, 2, 3, 4).unwrap();
        let mut chain = UndoChain::open(segment);
        let txn = chain.allocate_slot().unwrap();
        assert_eq!(txn, 0);

        // 事务动作：插入一行（记 Insert），再删除它（记 Delete，含旧值）。
        {
            let mut page = load(&io, h);
            let n = heap::insert_row(&mut page, &bytes, &InsertPolicy::in_place(0)).unwrap();
            store(&io, h, &mut page);
            let rid = RowId::from_parts(3, 0, n).unwrap();
            chain
                .append(txn, UndoOp::Insert, 0, rid, UndoPayload::None)
                .unwrap();
            let mut page = load(&io, h);
            heap::delete_row(&mut page, n).unwrap();
            store(&io, h, &mut page);
            chain
                .append(
                    txn,
                    UndoOp::Delete,
                    0,
                    rid,
                    UndoPayload::FullRow(bytes.clone()),
                )
                .unwrap();
        }

        // 全事务回滚：链上两条逆序补偿；净效果 = 初始状态（无此行）。
        let mut resolve = |r: Rdba| (r.file_id() == 3).then_some((h, 0));
        let n = rollback_transaction(&io, &chain, txn, &mut resolve).unwrap();
        assert_eq!(n, 2, "回放两条");
        let page = load(&io, h);
        assert_eq!(
            page.slot(0).unwrap().status(),
            SlotStatus::Free,
            "净效果 = 无此行"
        );
        assert_eq!(page.slot_count(), 1, "槽目录仍只 1 项");

        // 事务表槽已释放：wrap + 1、状态 Free。
        let header = chain.segment().read_page(0).unwrap();
        let slot = read_slot(&header, txn).unwrap();
        assert_eq!(slot.state, TxnState::Free);
        assert_eq!(slot.wrap, 1);
        assert_eq!(slot.undo_current, None);

        // 单条回放的中途状态也可验证：只回放链头（Delete），行应恢复。
        // （重新构造一遍，避免与上面的净效果混淆。）
        let mut file = DataFile::create(&io, Path::new("/mem/undo2.dat"), 1, 1, WS, 512).unwrap();
        let segment = create_undo_segment(&mut file, 2, 3, 4).unwrap();
        let mut chain2 = UndoChain::open(segment);
        let txn2 = chain2.allocate_slot().unwrap();
        let mut page = load(&io, h);
        let n = heap::insert_row(&mut page, &bytes, &InsertPolicy::in_place(0)).unwrap();
        store(&io, h, &mut page);
        let rid = RowId::from_parts(3, 0, n).unwrap();
        chain2
            .append(txn2, UndoOp::Insert, 0, rid, UndoPayload::None)
            .unwrap();
        let mut page = load(&io, h);
        heap::delete_row(&mut page, n).unwrap();
        store(&io, h, &mut page);
        let head = chain2
            .append(
                txn2,
                UndoOp::Delete,
                0,
                rid,
                UndoPayload::FullRow(bytes.clone()),
            )
            .unwrap();
        // 只回放链头这一条（rollback_chain 会沿链走到底，此处手动取单条）。
        let head_rec = chain2.read(head).unwrap();
        assert_eq!(head_rec.op, UndoOp::Delete);
        rollback_record(&io, &head_rec, &mut resolve).unwrap();
        let page = load(&io, h);
        assert_eq!(heap::row(&page, n), Some(&bytes[..]), "删除被撤销、行恢复");
    }

    #[test]
    fn compensations_are_noops_on_pre_image_page_states() {
        // PITR 的有界重放会让页停在目标点（比链覆盖的范围旧）：槽/ITL 槽
        // 都可能"还不存在"——补偿必须是空操作，而不是报错或凭空造行。
        let io = mem();
        let h = data_file(&io);
        {
            // 页上只有 0 行、1 个 ITL 槽。
            let mut page = load(&io, h);
            let bytes = row_bytes(1, b"x");
            let mut p2 = page.clone();
            heap::insert_row(&mut p2, &bytes, &InsertPolicy::in_place(0)).unwrap();
            let _ = &mut page;
            store(&io, h, &mut p2);
        }

        // 槽 5 不存在：插入/删除/转发补偿都空操作。
        let mut page = load(&io, h);
        let before = *page.as_bytes();
        for op in [UndoOp::Insert, UndoOp::Delete, UndoOp::Forward] {
            let rec = UndoRecord {
                prev: None,
                op,
                flags: 0,
                rowid: RowId::from_parts(3, 0, 6).unwrap(), // 槽 5（1 起）
                payload: match op {
                    UndoOp::Delete => UndoPayload::FullRow(row_bytes(1, b"old")),
                    UndoOp::Forward => UndoPayload::Forward(RowId::from_parts(3, 0, 2).unwrap()),
                    _ => UndoPayload::None,
                },
            };
            apply_undo_to_page(&mut page, &rec).unwrap();
        }
        assert_eq!(page.as_bytes(), &before, "页一字节未动");

        // ITL 槽 3 不存在（itl_count = 1）：ITL 覆盖补偿空操作。
        let rec = UndoRecord {
            prev: None,
            op: UndoOp::ItlOverwrite,
            flags: 0,
            rowid: RowId::from_parts(3, 0, 1).unwrap(),
            payload: UndoPayload::ItlOverwrite {
                itl_slot: 3,
                old: None,
            },
        };
        apply_undo_to_page(&mut page, &rec).unwrap();
        assert_eq!(page.as_bytes(), &before);

        // 但**存在**的槽/ITL 槽仍照常补偿（回归：别把空操作判据放大）。
        let rec = UndoRecord {
            prev: None,
            op: UndoOp::Insert,
            flags: 0,
            rowid: RowId::from_parts(3, 0, 1).unwrap(),
            payload: UndoPayload::None,
        };
        apply_undo_to_page(&mut page, &rec).unwrap();
        assert_eq!(heap::row(&page, 1), None, "存在的行被撤销");
    }
}

/// 分析阶段的事务表修复（前滚补标记 + 输家扫描）。
#[cfg(test)]
mod analysis_tests {
    use std::path::Path;

    use bicdb_common::seq::CommitSeq;
    use bicdb_workspace::io::MemFileIo;

    use super::*;
    use crate::datafile::DataFile;
    use crate::page::Page;

    const F: &str = "/mem/undo_repair.dat";
    const WS: [u8; 8] = [7u8; 8];

    fn seq(v: u64) -> CommitSeq {
        CommitSeq::from_raw(v).unwrap()
    }

    fn mem() -> MemFileIo {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        io
    }

    /// 新建 undo 段头页（真实段 → 页 0；扩展区就位）。测试只看内存页，不写回。
    fn header_page(io: &MemFileIo) -> Page {
        let mut file = DataFile::create(io, Path::new(F), 1, 1, WS, 512).unwrap();
        let segment = create_undo_segment(&mut file, 2, 3, 4).unwrap();
        segment.read_page(0).unwrap()
    }

    #[test]
    fn marks_committed_and_lists_losers() {
        let mut page = header_page(&mem());
        let (s0, _) = allocate_slot(&mut page).unwrap();
        let (s1, _) = allocate_slot(&mut page).unwrap();
        let (s2, _) = allocate_slot(&mut page).unwrap();
        assert_eq!((s0, s1, s2), (0, 1, 2), "空闲链按序弹出");

        let t0 = txn_id_of(s0, &read_slot(&page, s0).unwrap());
        let losers = repair_committed_slots(&mut page, &[(t0, seq(9))]).unwrap();

        let slot0 = read_slot(&page, s0).unwrap();
        assert_eq!(slot0.state, TxnState::Committed, "槽 0 前滚补标记");
        assert_eq!(slot0.commit_seq, seq(9));
        assert_eq!(losers, vec![s1, s2], "活动槽 = 输家（另一半来自日志）");
    }

    #[test]
    fn pending_rollback_is_a_loser() {
        let mut page = header_page(&mem());
        let (s0, _) = allocate_slot(&mut page).unwrap();
        let mut slot = read_slot(&page, s0).unwrap();
        slot.state = TxnState::PendingRollback;
        write_slot(&mut page, s0, &slot).unwrap();

        let losers = repair_committed_slots(&mut page, &[]).unwrap();
        assert_eq!(losers, vec![s0], "待回滚槽要续做到完成");
    }

    #[test]
    fn unknown_or_recycled_txn_is_ignored() {
        let mut page = header_page(&mem());
        let (s0, _) = allocate_slot(&mut page).unwrap();
        let live = txn_id_of(s0, &read_slot(&page, s0).unwrap());

        // ① 槽号越界于事务表的 txn_id（usn 1）② wrap 不匹配（槽已换人）
        let wrong_usn = TxnId::from_parts(1, 0, 0);
        let wrong_wrap = TxnId::from_parts(0, s0 as u8, 99);
        let losers =
            repair_committed_slots(&mut page, &[(wrong_usn, seq(1)), (wrong_wrap, seq(2))])
                .unwrap();

        assert_eq!(
            read_slot(&page, s0).unwrap().state,
            TxnState::Active,
            "未被误标"
        );
        assert_eq!(losers, vec![s0]);
        // 正常引用仍可补标记。
        let losers = repair_committed_slots(&mut page, &[(live, seq(3))]).unwrap();
        assert!(losers.is_empty());
        assert_eq!(read_slot(&page, s0).unwrap().state, TxnState::Committed);
    }

    #[test]
    fn repair_is_idempotent() {
        let mut page = header_page(&mem());
        let (s0, _) = allocate_slot(&mut page).unwrap();
        let t0 = txn_id_of(s0, &read_slot(&page, s0).unwrap());

        let first = repair_committed_slots(&mut page, &[(t0, seq(5))]).unwrap();
        let after_first = read_slot(&page, s0).unwrap();
        let second = repair_committed_slots(&mut page, &[(t0, seq(5))]).unwrap();
        assert_eq!(first, second);
        assert_eq!(read_slot(&page, s0).unwrap(), after_first);

        // 已标记的槽不再被日志序号改写（保留既有值——重复调用不改字节）。
        let bytes_before = *page.as_bytes();
        repair_committed_slots(&mut page, &[(t0, seq(42))]).unwrap();
        assert_eq!(*page.as_bytes(), bytes_before);
    }

    #[test]
    fn free_slot_is_not_touched() {
        let mut page = header_page(&mem());
        let (s0, _) = allocate_slot(&mut page).unwrap();
        let t0 = txn_id_of(s0, &read_slot(&page, s0).unwrap());
        free_slot(&mut page, s0).unwrap(); // wrap + 1、回空闲链

        let losers = repair_committed_slots(&mut page, &[(t0, seq(7))]).unwrap();
        assert!(losers.is_empty());
        assert_eq!(read_slot(&page, s0).unwrap().state, TxnState::Free);
    }
}
