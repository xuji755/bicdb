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

use crate::page::{Page, PageType, ITL_ENTRY_LEN};
use crate::rowid::RowId;
use crate::segment::{SegType, Segment, SegmentSpaceError, SEG_EXTENSION_OFFSET};

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
    /// 更新：旧 `itl_slot` 1B + 每个变更列的（列号 2B │ 旧值长度 2B │ 旧值）。
    Update {
        /// 行头里原来的 `itl_slot` 字节。
        old_itl_slot: u8,
        /// 变更列的前像列表。
        columns: Vec<(u16, Vec<u8>)>,
    },
    /// 转发指针更新：旧的转发目标。
    Forward(RowId),
    /// ITL 覆盖：被覆盖的旧 ITL 内容（`None` = 原为空闲）。
    ItlOverwrite(Option<[u8; ITL_ENTRY_LEN]>),
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
                columns,
            } => {
                out.push(*old_itl_slot);
                for (col, old) in columns {
                    out.extend_from_slice(&col.to_le_bytes());
                    out.extend_from_slice(&(old.len() as u16).to_le_bytes());
                    out.extend_from_slice(old);
                }
            }
            UndoPayload::Forward(target) => out.extend_from_slice(&target.to_bytes()),
            UndoPayload::ItlOverwrite(old) => match old {
                Some(entry) => out.extend_from_slice(entry),
                None => out.extend_from_slice(&[0u8; ITL_ENTRY_LEN]), // 全零 = 原为空闲
            },
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
                let mut columns = Vec::new();
                let mut at = 1usize;
                while at < body.len() {
                    if at + 4 > body.len() {
                        return Err(UndoError::MalformedRecord);
                    }
                    let col = u16::from_le_bytes([body[at], body[at + 1]]);
                    let len = usize::from(u16::from_le_bytes([body[at + 2], body[at + 3]]));
                    at += 4;
                    if at + len > body.len() {
                        return Err(UndoError::MalformedRecord);
                    }
                    columns.push((col, body[at..at + len].to_vec()));
                    at += len;
                }
                UndoPayload::Update {
                    old_itl_slot,
                    columns,
                }
            }
            UndoOp::Forward => {
                if body.len() != 6 {
                    return Err(UndoError::MalformedRecord);
                }
                UndoPayload::Forward(RowId::from_bytes(body.try_into().expect("6 字节")))
            }
            UndoOp::ItlOverwrite => {
                if body.len() != ITL_ENTRY_LEN {
                    return Err(UndoError::MalformedRecord);
                }
                let entry: [u8; ITL_ENTRY_LEN] = body.try_into().expect("24 字节");
                UndoPayload::ItlOverwrite((entry != [0u8; ITL_ENTRY_LEN]).then_some(entry))
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
        let mut file = DataFile::create(&io, Path::new(F), 1, 1, WS, 64).unwrap();
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
                columns: vec![(1, b"old-a".to_vec()), (7, vec![])],
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
            payload: UndoPayload::ItlOverwrite(Some([7u8; ITL_ENTRY_LEN])),
        };
        assert_eq!(UndoRecord::decode(&itl.encode()).unwrap(), itl);
        let itl_free = UndoRecord {
            payload: UndoPayload::ItlOverwrite(None),
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
                columns: vec![(1, vec![9, 9, 9])],
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
