//! 一致性读（CR）：**在快照下重建块的可见版本**（§12.2 的逐段回溯）。
//!
//! 读者不加锁、不等待（REQ-TXN-005）：拿到当前块后，凡是"快照看不见"的
//! 事务（活动、待回滚、或提交序号 > 快照），就**沿其 undo 链把该块的部分
//! 撤销回去**——块级自包含，不需要任何全局事务状态表：
//!
//! ```text
//! 读当前块 → ITL[i] 属于 T2：快照看不见 → 撤销 T2 对本块的修改
//!     ├─ 行前像：被 T2 改过的行按 undo 恢复       （apply_undo_to_page）
//!     └─ ITL 覆盖：ITL[i] 还原成 T2 占用它之前的内容（§4.6.2 的块级动作）
//!          → 取回 T1 与其 undo_ptr
//!   再判 T1：仍看不见 → 继续撤销 …… 直到全部可见
//! ```
//!
//! # 判定（§11.1.1 的"读一个没清除过的块"）
//!
//! | ITL 状态 | 判定 |
//! | --- | --- |
//! | `Committed`（已清除） | 页内 `commit_seq` 与快照直接比较——**不必查事务表**；**序号字段为空（0）时按"不知道"处理，回查事务表**（见 `itl::decode`） |
//! | `Active` / `PendingRollback`（未清除） | 由 `txn_id` 三段式**直接定位**事务表槽：槽 `Committed` 取准确序号比较；槽 `Active` ⇒ 未提交 |
//! | `RolledBack` / `Free` | 可见（回滚已把块改回去） |
//! | **槽查不到**（`wrap` 不匹配 = 槽已复用） | **可见**——复用条件（§4.6.3）保证旧事务 `commit_seq < 最老快照 ≤ S` |
//!
//! # 回溯起点与终止（为什么能收敛）
//!
//! 起点取**事务表槽的 `undo_current`**（该事务最新一条撤销记录）——它是
//! 权威的当前位置；ITL 条目的 `undo_ptr` 只是登记时的快照性提示，可能
//! 落后于后续追加。（与 Oracle 取 `ktuxc` 当前撤销指针、而非只信 ITL 的
//! `uba` 同理。）
//!
//! 每撤销一个条目，必然走到并应用**本块的"ITL 覆盖"记录**——写路径在
//! *占用* ITL 槽时就记下它（旧值 `None` = 原为空闲），它是该事务对本块
//! 修改的前一刻状态。因此一轮回溯 = 一个条目的状态严格回退到更旧的值；
//! 链上无环时轮数有界。三条防线：
//!
//! 1. **终止符缺失** ⇒ 数据不一致（有本块数据记录却无占用记录），报
//!    [`CrError::NoItlUndo`]，不复用；
//! 2. **`prev` 环** ⇒ 步数超过该事务的 `rec_count`，报 [`CrError::ChainCycle`]；
//! 3. **条目反复**（恢复出的前像仍不可见）⇒ 轮数上限，报 [`CrError::TooManyRounds`]。
//!
//! **重建在内存副本上进行**（源页一个字节也不动）。
//!
//! # 读己所写（`own`）
//!
//! 判定是"事务对快照的可见性"，**不含"我自己的事务"这一维**：一个事务对
//! 自己未提交的改动必须可见（Oracle/PG 同款：`BEGIN; INSERT; SELECT` 看得
//! 到刚插的行）。所以视角是 [`ReadView`]——快照 **+ 本会话自己的活动事务**：
//! `own` 命中的 ITL 条目不撤销，本会话的未提交改动因此**留在重建结果里**。
//!
//! 判据取 `txn_id`（ITL 里登记的所有者）**而不是**"状态看起来像我"：
//! 只有这一个事务是自己的，`wrap` 三段式保证不会认错别人（槽复用后
//! `txn_id` 必不同）。

use bicdb_common::seq::CommitSeq;

use crate::itl::{self, ItlState};
use crate::page::{Page, PageType};
use crate::rowid::RowId;
use crate::undo::{
    apply_undo_to_page, RollbackError, TxnId, TxnState, UndoChain, UndoChainError, UndoOp,
};

/// CR 重建错误。
#[derive(Debug)]
pub enum CrError {
    /// 页/ITL 字段错误。
    Itl(itl::ItlError),
    /// undo 链读取错误。
    Chain(UndoChainError),
    /// 补偿动作错误（含"更新类暂缓"）。
    Rollback(RollbackError),
    /// 不是数据页。
    NotDataPage,
    /// **终止符缺失**：条目所有者在本块有数据记录，链上却没有本块的
    /// "ITL 覆盖"记录——数据不一致。
    NoItlUndo {
        /// 页内 ITL 槽号。
        itl_slot: u16,
    },
    /// undo 链 `prev` 成环（步数超过事务表槽记录的记录数）。
    ChainCycle,
    /// 回溯轮数超限（条目反复恢复成同样的"不可见"前像）。
    TooManyRounds,
}

impl std::fmt::Display for CrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CrError::Itl(e) => write!(f, "CR：{e}"),
            CrError::Chain(e) => write!(f, "CR：{e}"),
            CrError::Rollback(e) => write!(f, "CR：{e}"),
            CrError::NotDataPage => f.write_str("CR：不是数据页"),
            CrError::NoItlUndo { itl_slot } => {
                write!(
                    f,
                    "CR：ITL 槽 {itl_slot} 的所有者在本块有数据记录，却无本块的 ITL 覆盖记录"
                )
            }
            CrError::ChainCycle => f.write_str("CR：undo 链 prev 成环"),
            CrError::TooManyRounds => f.write_str("CR：回溯轮数超限（条目反复恢复）"),
        }
    }
}

impl std::error::Error for CrError {}

impl From<itl::ItlError> for CrError {
    fn from(e: itl::ItlError) -> Self {
        CrError::Itl(e)
    }
}

impl From<UndoChainError> for CrError {
    fn from(e: UndoChainError) -> Self {
        CrError::Chain(e)
    }
}

impl From<RollbackError> for CrError {
    fn from(e: RollbackError) -> Self {
        CrError::Rollback(e)
    }
}

/// **一致性读的视角**：快照 + 本会话自己的活动事务。
///
/// - `snapshot`：提交序号（**它之前的提交可见**）；
/// - `own`：本会话自己的活动事务（`Some` ⇒ **读己所写**：它的未提交改动
///   对自己可见）。无活动事务（自动提交/只读会话）时是 `None`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadView {
    /// 快照（提交序号）。
    pub snapshot: CommitSeq,
    /// 本会话自己的活动事务。
    pub own: Option<TxnId>,
}

impl ReadView {
    /// 只给快照（无自己的事务——自动提交/只读形态）。
    #[must_use]
    pub fn new(snapshot: CommitSeq) -> Self {
        Self {
            snapshot,
            own: None,
        }
    }

    /// 带上自己的活动事务（**读己所写**）。
    #[must_use]
    pub fn with_own(mut self, own: Option<TxnId>) -> Self {
        self.own = own;
        self
    }
}

/// **回溯轮数上限**（默认）——防"条目反复恢复成同样的不可见前像"的死循环。
pub const DEFAULT_MAX_ROUNDS: u32 = 64;

/// 进程级当前值（实例打开时设定一次；实例参数 `storage.cr_max_rounds`）。
static MAX_ROUNDS: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(DEFAULT_MAX_ROUNDS);

/// 当前的回溯轮数上限。
#[must_use]
pub fn cr_max_rounds() -> u32 {
    MAX_ROUNDS.load(std::sync::atomic::Ordering::Relaxed)
}

/// **设定回溯轮数上限**（实例参数 `storage.cr_max_rounds`；下限 16——比一轮
/// 并发写事务还少的预算会把合法场景判成「回溯轮数超限」）。
///
/// # Errors
/// 越出 16..=4096。
pub fn set_cr_max_rounds(rounds: u32) -> Result<(), &'static str> {
    if !(16..=4096).contains(&rounds) {
        return Err("cr_max_rounds 要落在 16–4096");
    }
    MAX_ROUNDS.store(rounds, std::sync::atomic::Ordering::Relaxed);
    Ok(())
}

/// 一个 ITL 条目的判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// 快照可见——不动。
    Visible,
    /// 快照看不见——需要撤销本块的修改；带回溯起点与步数预算。
    Undo {
        /// 链头（事务表槽的 `undo_current`；`None` = 该事务尚无记录）。
        head: Option<RowId>,
        /// 步数预算 = 槽内 `rec_count` + 1（越过即链有环）。
        budget: u64,
    },
}

/// **重建块在快照 `snapshot` 下的可见版本**（返回内存副本；源页不动）。
pub fn reconstruct(
    page: &Page,
    view: ReadView,
    chain: &UndoChain<'_, '_>,
) -> Result<Page, CrError> {
    let ReadView { snapshot, own } = view;
    let header = page.header().ok_or(CrError::NotDataPage)?;
    if !matches!(
        header.page_type,
        PageType::HeapTable | PageType::Temporary | PageType::Adjacency
    ) {
        // **撤销页不参与 CR**：它的 ITL[0] 是"页归属"（由 `plan_append`
        // 直接写入、链上没有对应的 `ITL 覆盖` 终止符）——走 CR 必被
        // 判成数据不一致。撤销页的内容**直读**（`UndoChain::read`）。
        return Err(CrError::NotDataPage);
    }
    let (file_id, block_id) = (header.file_id, header.block_id);

    let mut cr = Page::from_bytes(Box::new(*page.as_bytes()));
    let mut rounds = 0u32;
    loop {
        let mut undone = false;
        let count = itl::itl_count(&cr)?;
        for index in 0..count {
            let entry = itl::read_itl(&cr, index)?;
            // **读己所写**：自己事务的条目**一条都不撤销**——未提交也要看得见。
            // 放在状态判定之前：`Active` 既涵盖"活动"也涵盖"已提交未清除"，
            // 由 `txn_id` 认自己最直接（见模块文档）。
            if own.is_some_and(|mine| mine == entry.txn_id) {
                continue;
            }
            match entry.state {
                ItlState::Free | ItlState::RolledBack => continue,
                ItlState::Committed => {
                    // 已清除：页内序号即权威（快照够新 ⇒ 可见，不必查事务表）。
                    // 序号为空 ⇒ 落下去查事务表（"不知道"不等于"可见"）。
                    if entry.commit_seq.is_some_and(|s| s <= snapshot) {
                        continue;
                    }
                }
                ItlState::Active => {}
            }
            let Verdict::Undo { head, budget } = classify(entry.txn_id, snapshot, chain)? else {
                continue;
            };
            let Some(head) = head else {
                // 该事务链上一条记录都没有，却出现在本块 ITL——不可能修改过
                // 本块；防御性跳过（不置 `undone`，避免空转）。
                continue;
            };
            if !undo_txn_on_block(&mut cr, head, file_id, block_id, index, chain, budget)? {
                return Err(CrError::NoItlUndo { itl_slot: index });
            }
            undone = true;
        }
        if !undone {
            break;
        }
        rounds += 1;
        if rounds > cr_max_rounds() {
            return Err(CrError::TooManyRounds);
        }
    }
    Ok(cr)
}

/// 事务对快照的可见性判定（见模块文档的表）。
fn classify(
    txn_id: TxnId,
    snapshot: CommitSeq,
    chain: &UndoChain<'_, '_>,
) -> Result<Verdict, CrError> {
    match chain.lookup(txn_id)? {
        // 槽已复用/查无此人 ⇒ 必已提交且旧于一切有效快照（§4.6.3 的复用条件）。
        None => Ok(Verdict::Visible),
        Some(slot) => match slot.state {
            TxnState::Committed => Ok(if slot.commit_seq <= snapshot {
                Verdict::Visible
            } else {
                Verdict::Undo {
                    head: slot.undo_current,
                    budget: u64::from(slot.rec_count) + 1,
                }
            }),
            TxnState::Active | TxnState::PendingRollback => Ok(Verdict::Undo {
                head: slot.undo_current,
                budget: u64::from(slot.rec_count) + 1,
            }),
            TxnState::Free => Ok(Verdict::Visible), // `find_slot` 已滤掉；防御
        },
    }
}

/// 沿事务链撤销**本块**的修改（链是事务全局的：别的块的记录跳过）。
///
/// 撤销到**本块"该 ITL 槽"的覆盖记录**即止——它把 ITL 还原成该事务占用前的
/// 状态，交外层重新判定；找不到它说明数据不一致，返回 `false`。
/// 终止符按 `payload.itl_slot == expected_slot` 匹配：一个事务理论上每块只占
/// 一个槽，但匹配到**具体槽**才能保证"一轮回溯 = 一个条目严格回退"（否则
/// 遇到更早的别的槽的覆盖记录会空转，§12.3.1 的收敛保证）。
/// 步数超过 `budget`（`rec_count + 1`）说明 `prev` 成环。
fn undo_txn_on_block(
    page: &mut Page,
    head: RowId,
    file_id: u16,
    block_id: u32,
    expected_slot: u16,
    chain: &UndoChain<'_, '_>,
    budget: u64,
) -> Result<bool, CrError> {
    let mut at = Some(head);
    let mut steps = 0u64;
    while let Some(pos) = at {
        steps += 1;
        if steps > budget {
            return Err(CrError::ChainCycle);
        }
        let record = chain.read(pos)?;
        at = record.prev;
        if record.rowid.file_id() != file_id || record.rowid.block_id() != block_id {
            continue; // 别的块的修改：不动，但继续沿链走
        }
        if record.op == UndoOp::ItlOverwrite {
            let target = match &record.payload {
                crate::undo::UndoPayload::ItlOverwrite { itl_slot, .. } => u16::from(*itl_slot),
                _ => {
                    return Err(CrError::Rollback(RollbackError::Undo(
                        crate::undo::UndoError::MalformedRecord,
                    )))
                }
            };
            if target != expected_slot {
                continue; // 别的槽的覆盖记录（更早的占用）：不动，继续沿链走
            }
        }
        apply_undo_to_page(page, &record)?;
        if record.op == UndoOp::ItlOverwrite {
            return Ok(true); // 本槽的 ITL 已还原 → 外层重评估
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use bicdb_workspace::io::MemFileIo;

    use super::*;
    use crate::datafile::DataFile;
    use crate::heap::{self, InsertPolicy};
    use crate::itl::{ItlEntry, ItlState};
    use crate::page::{Page, WORKSPACE_REF_LEN};
    use crate::row::assemble_row;
    use crate::undo::{
        create_undo_segment, free_slot, read_slot, txn_id_of, write_slot, UndoPayload,
    };

    const UNDO_F: &str = "/mem/undo1.dat";
    const WS: [u8; 8] = [5u8; 8];

    fn mem() -> MemFileIo {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        io
    }

    fn row_bytes(itl_slot: u8, payload: &[u8]) -> Vec<u8> {
        assemble_row(0, itl_slot, &[false], &[], &[payload]).unwrap()
    }

    fn seq(v: u64) -> CommitSeq {
        CommitSeq::from_raw(v).unwrap()
    }

    fn txn(slot: u8, wrap: u32) -> TxnId {
        TxnId::from_parts(0, slot, wrap)
    }

    fn active_entry(owner: TxnId, undo_ptr: Option<RowId>) -> ItlEntry {
        ItlEntry {
            txn_id: owner,
            undo_ptr,
            commit_seq: None,
            lock_cnt: 1,
            state: ItlState::Active,
        }
    }

    /// 建一张含一行、ITL[0] 归 `owner` 的堆表页（内存页——CR 只读内存页）。
    fn page_with_row(owner: TxnId, payload: &[u8]) -> (Page, u16, Vec<u8>) {
        let mut page = Page::new(PageType::HeapTable, [0u8; WORKSPACE_REF_LEN], 3, 0);
        let bytes = row_bytes(1, payload);
        let n = heap::insert_row(&mut page, &bytes, &InsertPolicy::in_place(0)).unwrap();
        itl::write_itl(&mut page, 0, &active_entry(owner, None)).unwrap();
        (page, n, bytes)
    }

    /// 记一条"占用 ITL 槽"的撤销记录（写路径在**占用**时记；旧值 `None` =
    /// 原为空闲）。它同时是回溯的**终止符**——撤销到它即还原 ITL 前像。
    fn append_itl_acquire(
        chain: &mut UndoChain<'_, '_>,
        slot: u16,
        itl_slot: u8,
        old: Option<[u8; crate::page::ITL_ENTRY_LEN]>,
    ) -> RowId {
        let header = chain.segment().read_page(0).unwrap();
        let txn_id = txn_id_of(slot, &read_slot(&header, slot).unwrap());
        chain
            .append(
                slot,
                UndoOp::ItlOverwrite,
                0,
                RowId::from_parts(3, 0, 1).unwrap(),
                UndoPayload::ItlOverwrite {
                    txn_id,
                    itl_slot,
                    old,
                },
            )
            .unwrap()
    }

    /// 把事务表槽置为"已提交（序号 `s`）"（块未清除——ITL 仍是 Active）。
    fn commit_slot(chain: &UndoChain<'_, '_>, index: u16, s: u64) {
        let mut hdr = chain.segment().read_page(0).unwrap();
        let mut slot = read_slot(&hdr, index).unwrap();
        slot.state = TxnState::Committed;
        slot.commit_seq = seq(s);
        write_slot(&mut hdr, index, &slot).unwrap();
        chain.segment().write_page(0, &mut hdr).unwrap();
    }

    #[test]
    fn uncommitted_insert_is_invisible() {
        let io = mem();
        let mut file = DataFile::create(&io, Path::new(UNDO_F), 1, 1, WS, 512).unwrap();
        let segment = create_undo_segment(&mut file, 2, 3, 4).unwrap();
        let mut chain = UndoChain::open(segment);
        let slot = chain.allocate_slot().unwrap();

        let owner = txn(slot as u8, 0);
        let (mut page, n, bytes) = page_with_row(owner, b"alpha");
        let rid = RowId::from_parts(3, 0, n).unwrap();
        // 写路径：先记"占用 ITL[0]"（原为空闲），再记插入的行前像。
        append_itl_acquire(&mut chain, slot, 0, None);
        let head = chain
            .append(slot, UndoOp::Insert, 0, rid, UndoPayload::None)
            .unwrap();
        {
            let mut e = itl::read_itl(&page, 0).unwrap();
            e.undo_ptr = Some(head);
            itl::write_itl(&mut page, 0, &e).unwrap();
        }

        let cr = reconstruct(&page, ReadView::new(seq(100)), &chain).unwrap();
        assert_eq!(heap::row(&cr, n), None, "未提交的插入不可见");
        assert_eq!(
            itl::read_itl(&cr, 0).unwrap().state,
            ItlState::Free,
            "ITL 还原为空闲"
        );
        // 源页一个字节不动。
        assert_eq!(heap::row(&page, n), Some(&bytes[..]), "重建不改源页");
    }

    /// **读己所写**：同一个未提交事务，**自己**看得见（`own` 命中），
    /// **别人**看不见（`own` 不是它）——两侧是同一个页、同一条链。
    #[test]
    fn own_txn_sees_its_uncommitted_row_others_do_not() {
        let io = mem();
        let mut file = DataFile::create(&io, Path::new(UNDO_F), 1, 1, WS, 512).unwrap();
        let segment = create_undo_segment(&mut file, 2, 3, 4).unwrap();
        let mut chain = UndoChain::open(segment);
        let slot = chain.allocate_slot().unwrap();

        let owner = txn(slot as u8, 0);
        let (mut page, n, bytes) = page_with_row(owner, b"alpha");
        let rid = RowId::from_parts(3, 0, n).unwrap();
        append_itl_acquire(&mut chain, slot, 0, None);
        let head = chain
            .append(slot, UndoOp::Insert, 0, rid, UndoPayload::None)
            .unwrap();
        {
            let mut e = itl::read_itl(&page, 0).unwrap();
            e.undo_ptr = Some(head);
            itl::write_itl(&mut page, 0, &e).unwrap();
        }

        // ① 自己的视角（own = 本事务）：**看得见**未提交的行。
        let mine =
            reconstruct(&page, ReadView::new(seq(100)).with_own(Some(owner)), &chain).unwrap();
        assert_eq!(
            heap::row(&mine, n),
            Some(&bytes[..]),
            "自己的未提交插入要看得见（读己所写）"
        );
        assert_eq!(
            itl::read_itl(&mine, 0).unwrap(),
            active_entry(owner, Some(head))
        );

        // ② 别人的视角（own = 别的 txn / None）：看不见。
        let other_txn = txn(9, 0);
        for own in [None, Some(other_txn)] {
            let theirs = reconstruct(&page, ReadView::new(seq(100)).with_own(own), &chain).unwrap();
            assert_eq!(
                heap::row(&theirs, n),
                None,
                "别人的未提交插入不该可见（own={own:?}）"
            );
        }
    }

    #[test]
    fn committed_after_snapshot_is_invisible_before() {
        let io = mem();
        let mut file = DataFile::create(&io, Path::new(UNDO_F), 1, 1, WS, 512).unwrap();
        let segment = create_undo_segment(&mut file, 2, 3, 4).unwrap();
        let mut chain = UndoChain::open(segment);
        let slot = chain.allocate_slot().unwrap();
        let owner = txn(slot as u8, 0);
        let (mut page, n, bytes) = page_with_row(owner, b"beta");
        let rid = RowId::from_parts(3, 0, n).unwrap();
        append_itl_acquire(&mut chain, slot, 0, None);
        let head = chain
            .append(slot, UndoOp::Insert, 0, rid, UndoPayload::None)
            .unwrap();
        {
            let mut e = itl::read_itl(&page, 0).unwrap();
            e.undo_ptr = Some(head);
            itl::write_itl(&mut page, 0, &e).unwrap();
        }
        commit_slot(&chain, slot, 10);

        let before = reconstruct(&page, ReadView::new(seq(5)), &chain).unwrap();
        assert_eq!(heap::row(&before, n), None, "快照在提交前 ⇒ 不可见");
        let after = reconstruct(&page, ReadView::new(seq(15)), &chain).unwrap();
        assert_eq!(
            heap::row(&after, n),
            Some(&bytes[..]),
            "快照在提交后 ⇒ 可见"
        );
        assert_eq!(
            after.as_bytes(),
            page.as_bytes(),
            "可见时副本逐字节等于源页"
        );
    }

    #[test]
    fn itl_overwrite_rolls_back_and_exposes_the_previous_txn() {
        let io = mem();
        let mut file = DataFile::create(&io, Path::new(UNDO_F), 1, 1, WS, 512).unwrap();
        let segment = create_undo_segment(&mut file, 2, 3, 4).unwrap();
        let mut chain = UndoChain::open(segment);
        let t1 = chain.allocate_slot().unwrap();
        let t2 = chain.allocate_slot().unwrap();

        // T1 提交（seq 3）：行可见，ITL[0] 已清除（Committed/seq 3）。
        let (mut page, n, bytes) = page_with_row(txn(t1 as u8, 0), b"gamma");
        let rid = RowId::from_parts(3, 0, n).unwrap();
        commit_slot(&chain, t1, 3);
        let t1_entry = ItlEntry {
            txn_id: txn(t1 as u8, 0),
            undo_ptr: None,
            commit_seq: Some(seq(3)),
            lock_cnt: 0,
            state: ItlState::Committed,
        };
        itl::write_itl(&mut page, 0, &t1_entry).unwrap();

        // T2 占用 ITL[0]（记 ItlOverwrite 旧值 = T1 条目的快照）并删除该行。
        let mut old = [0u8; crate::page::ITL_ENTRY_LEN];
        t1_entry.encode(&mut old);
        let head_itl = append_itl_acquire(&mut chain, t2, 0, Some(old));
        itl::write_itl(
            &mut page,
            0,
            &active_entry(txn(t2 as u8, 0), Some(head_itl)),
        )
        .unwrap();
        heap::delete_row(&mut page, n).unwrap();
        let head_del = chain
            .append(
                t2,
                UndoOp::Delete,
                0,
                rid,
                UndoPayload::FullRow(bytes.clone()),
            )
            .unwrap();
        {
            let mut e = itl::read_itl(&page, 0).unwrap();
            e.undo_ptr = Some(head_del);
            itl::write_itl(&mut page, 0, &e).unwrap();
        }

        // S=5：T2 看不见 → 撤销删除与 ITL 覆盖 → 暴露 T1（seq 3 ≤ 5，可见）。
        let cr = reconstruct(&page, ReadView::new(seq(5)), &chain).unwrap();
        assert_eq!(heap::row(&cr, n), Some(&bytes[..]), "行按 T1 版本可见");
        assert_eq!(itl::read_itl(&cr, 0).unwrap(), t1_entry, "ITL 已还原成 T1");
    }

    #[test]
    fn recycled_slot_means_visible() {
        let io = mem();
        let mut file = DataFile::create(&io, Path::new(UNDO_F), 1, 1, WS, 512).unwrap();
        let segment = create_undo_segment(&mut file, 2, 3, 4).unwrap();
        let mut chain = UndoChain::open(segment);
        let slot = chain.allocate_slot().unwrap();
        let owner = txn(slot as u8, 0);
        let (page, n, bytes) = page_with_row(owner, b"delta");

        // 释放槽（wrap + 1）——模拟"槽已换人"。
        {
            let mut hdr = chain.segment().read_page(0).unwrap();
            free_slot(&mut hdr, slot).unwrap();
            chain.segment().write_page(0, &mut hdr).unwrap();
        }
        // ITL 仍显示 Active（陈旧引用），但槽查不到 ⇒ 可见。
        let cr = reconstruct(&page, ReadView::new(seq(1)), &chain).unwrap();
        assert_eq!(heap::row(&cr, n), Some(&bytes[..]));
    }

    #[test]
    fn missing_itl_undo_record_is_reported() {
        let io = mem();
        let mut file = DataFile::create(&io, Path::new(UNDO_F), 1, 1, WS, 512).unwrap();
        let segment = create_undo_segment(&mut file, 2, 3, 4).unwrap();
        let mut chain = UndoChain::open(segment);
        let slot = chain.allocate_slot().unwrap();
        let owner = txn(slot as u8, 0);
        let (mut page, n, _bytes) = page_with_row(owner, b"zeta");
        let rid = RowId::from_parts(3, 0, n).unwrap();

        // **缺终止符**：只有数据记录、没有"占用 ITL"记录（写路径不会这样）。
        chain
            .append(slot, UndoOp::Insert, 0, rid, UndoPayload::None)
            .unwrap();
        {
            let mut e = itl::read_itl(&page, 0).unwrap();
            e.undo_ptr = Some(rid);
            itl::write_itl(&mut page, 0, &e).unwrap();
        }
        assert!(matches!(
            reconstruct(&page, ReadView::new(seq(1)), &chain),
            Err(CrError::NoItlUndo { itl_slot: 0 })
        ));
    }

    #[test]
    fn round_limit_guards_against_non_terminating_undo() {
        let io = mem();
        let mut file = DataFile::create(&io, Path::new(UNDO_F), 1, 1, WS, 512).unwrap();
        let segment = create_undo_segment(&mut file, 2, 3, 4).unwrap();
        let mut chain = UndoChain::open(segment);
        let slot = chain.allocate_slot().unwrap();
        let owner = txn(slot as u8, 0);
        let (mut page, n, _bytes) = page_with_row(owner, b"epsilon");

        // 终止符的旧值 = 与当前**完全相同**的 Active 条目 ⇒ 每轮"还原"回原样，
        // 条目永远需要撤销——轮数上限必须兜住（损坏数据下的环）。
        let entry = itl::read_itl(&page, 0).unwrap();
        let mut old = [0u8; crate::page::ITL_ENTRY_LEN];
        entry.encode(&mut old);
        let rid = RowId::from_parts(3, 0, n).unwrap();
        chain
            .append(
                slot,
                UndoOp::Forward,
                0,
                rid,
                UndoPayload::Forward(RowId::from_parts(3, 0, 7).unwrap()),
            )
            .unwrap();
        append_itl_acquire(&mut chain, slot, 0, Some(old));
        {
            let mut e = entry;
            e.undo_ptr = Some(rid);
            itl::write_itl(&mut page, 0, &e).unwrap();
        }
        assert!(matches!(
            reconstruct(&page, ReadView::new(seq(1)), &chain),
            Err(CrError::TooManyRounds)
        ));
    }

    #[test]
    fn prev_cycle_is_reported() {
        let io = mem();
        let mut file = DataFile::create(&io, Path::new(UNDO_F), 1, 1, WS, 512).unwrap();
        let segment = create_undo_segment(&mut file, 2, 3, 4).unwrap();
        let mut chain = UndoChain::open(segment);
        let slot = chain.allocate_slot().unwrap();
        let owner = txn(slot as u8, 0);
        let (mut page, n, _bytes) = page_with_row(owner, b"eta");

        // 终止符在前、数据记录在后（正常顺序）。
        append_itl_acquire(&mut chain, slot, 0, None);
        let rid = RowId::from_parts(3, 0, n).unwrap();
        let head = chain
            .append(
                slot,
                UndoOp::Forward,
                0,
                rid,
                UndoPayload::Forward(RowId::from_parts(3, 0, 7).unwrap()),
            )
            .unwrap();
        {
            let mut e = itl::read_itl(&page, 0).unwrap();
            e.undo_ptr = Some(head);
            itl::write_itl(&mut page, 0, &e).unwrap();
        }

        // **人为制造 prev 环**：把最新记录的 prev 指回它自己。
        let logical = chain
            .segment()
            .logical_of_block(head.block_id())
            .expect("撤销页在映射内");
        let mut undo_page = chain.segment().read_page(logical).unwrap();
        let index = crate::heap::slot_index(head.row_id()).expect("槽号 1 起");
        let offset = usize::from(undo_page.slot(index).unwrap().offset());
        undo_page.as_bytes_mut()[offset..offset + 6].copy_from_slice(&head.to_bytes());
        chain.segment().write_page(logical, &mut undo_page).unwrap();

        assert!(matches!(
            reconstruct(&page, ReadView::new(seq(1)), &chain),
            Err(CrError::ChainCycle)
        ));
    }

    #[test]
    fn rejects_non_data_pages() {
        let io = mem();
        let mut file = DataFile::create(&io, Path::new(UNDO_F), 1, 1, WS, 512).unwrap();
        let segment = create_undo_segment(&mut file, 2, 3, 4).unwrap();
        let chain = UndoChain::open(segment);
        let page = Page::new(PageType::SegmentHeader, [0u8; WORKSPACE_REF_LEN], 1, 0);
        assert!(matches!(
            reconstruct(&page, ReadView::new(seq(1)), &chain),
            Err(CrError::NotDataPage)
        ));
        // **撤销页不参与 CR**（P3 审核修复）：它的 ITL[0] 是"页归属"、
        // 链上没有对应的 `ITL 覆盖` 终止符——走 CR 必判数据不一致。
        // 撤销页的内容直读（`UndoChain::read`）。
        let undo_page = Page::new(PageType::Undo, [0u8; WORKSPACE_REF_LEN], 1, 5);
        assert!(matches!(
            reconstruct(&undo_page, ReadView::new(seq(1)), &chain),
            Err(CrError::NotDataPage)
        ));
    }
}
