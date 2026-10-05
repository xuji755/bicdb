//! **写路径**（§11.1.1 的提交流程、§4.6.6 的事务生命周期）：DML 经缓冲池落地。
//!
//! ```text
//! 一次修改（insert/delete/…，本切片刻 insert）：
//!   ① 取数据页（缓冲池钉住）——**写先入缓存**
//!   ② 占用 ITL 槽：捕获前像 → 记"ITL 覆盖"撤销记录（经池、带 redo）
//!   ③ 改页（插行）→ 记"插入"撤销记录（经池、带 redo）
//!   ④ 数据页差异 → 追加 redo → 推进 page_lsn → **标脏**（写列表排队）
//! 提交：提交记录入流 → **等 LGWR 刷到该记录** → 事务表槽置已提交 → 返回
//! 回滚：沿链从新到旧补偿（每条都经池、带 redo）→ 释放槽
//! ```
//!
//! # 为什么撤销页也走缓冲池（且**立即落盘**）
//!
//! §11.1.2：**undo 自身必须受 redo 保护**。本模块把 [`UndoChain::plan_append`]
//! 的计划页**经缓冲池**写入：每页（撤销页 / 段头页 / 位图页）都按
//! "差异 → redo 记录 → page_lsn → 标脏" 走一遍（WAL 规则 2 由池保证）。
//!
//! 与数据页不同，撤销页**写后立即 `flush`**（仍是经池的写：redo 先落、
//! page_lsn 推进、WAL 规则 2 强制）。理由：链的读取是**直读段文件**的
//! （`UndoChain::read` 供回滚/CR 用），立即落盘让"池里的写"与"文件里的读"
//! 始终一致；数据页保持 no-force（提交路径上没有数据页 I/O 的纪律不变）。
//!
//! # 顺序（不可换）
//!
//! 撤销记录**先于**它保护的数据变更入流（③ 在 ④ 前）——崩溃重放时，
//! 数据变更的 redo 若在日志里，其撤销记录必在更早的位置。

use bicdb_common::seq::{CommitSeq, Lsn};
use bicdb_storage::buffer::{BufferError, BufferKey, BufferPool};
use bicdb_storage::heap::{self, HeapError, InsertPolicy};
use bicdb_storage::itl::{self, ItlEntry, ItlError, ItlState};
use bicdb_storage::page::Page;
use bicdb_storage::rowid::{Rdba, RowId, RowIdRangeError};
use bicdb_storage::undo::{
    apply_undo_to_page, free_slot, read_slot, txn_id_of, write_slot, RollbackError, TxnId,
    TxnState, UndoChain, UndoChainError, UndoError, UndoOp, UndoPayload,
};
use bicdb_wal::group::{GroupError, GroupWriter};
use bicdb_wal::record::{page_diff, BlockRef, RedoRecord};

/// 写路径错误。
#[derive(Debug)]
pub enum TxnError {
    /// 缓冲池。
    Pool(BufferError),
    /// 日志写入。
    Log(GroupError),
    /// undo 链。
    Chain(UndoChainError),
    /// undo 结构（事务表等）。
    Undo(UndoError),
    /// 补偿动作。
    Rollback(RollbackError),
    /// 页操作。
    Heap(HeapError),
    /// ITL。
    Itl(ItlError),
    /// 行号越界。
    RowId(RowIdRangeError),
    /// 缓存中的页与"前像"不符（不该发生——单写者下即失步信号）。
    StaleCache,
    /// 段访问。
    Segment(bicdb_storage::segment::SegmentSpaceError),
    /// **更新非等长**（行迁移/成链留给后续切片）——明确拒绝。
    UpdateNotInPlace {
        /// 旧行长。
        old: usize,
        /// 新行长。
        new: usize,
    },
}

impl std::fmt::Display for TxnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TxnError::Pool(e) => write!(f, "写路径缓冲池：{e}"),
            TxnError::Log(e) => write!(f, "写路径日志：{e}"),
            TxnError::Chain(e) => write!(f, "写路径 undo 链：{e}"),
            TxnError::Undo(e) => write!(f, "写路径 undo：{e}"),
            TxnError::Rollback(e) => write!(f, "写路径回滚：{e}"),
            TxnError::Heap(e) => write!(f, "写路径行操作：{e}"),
            TxnError::Itl(e) => write!(f, "写路径 ITL：{e}"),
            TxnError::RowId(e) => write!(f, "写路径行号：{e}"),
            TxnError::StaleCache => f.write_str("写路径：缓存页与预期前像不符"),
            TxnError::Segment(e) => write!(f, "写路径段访问：{e}"),
            TxnError::UpdateNotInPlace { old, new } => write!(
                f,
                "更新改行长度（{old} → {new}）：等长就地更新之外留给行迁移切片"
            ),
        }
    }
}

impl std::error::Error for TxnError {}

macro_rules! from_impl {
    ($($t:ty => $v:ident),+ $(,)?) => {
        $(impl From<$t> for TxnError {
            fn from(e: $t) -> Self {
                TxnError::$v(e)
            }
        })+
    };
}
from_impl!(
    BufferError => Pool,
    GroupError => Log,
    UndoChainError => Chain,
    UndoError => Undo,
    RollbackError => Rollback,
    HeapError => Heap,
    ItlError => Itl,
    RowIdRangeError => RowId,
    bicdb_storage::segment::SegmentSpaceError => Segment,
);

/// 一个进行中的事务（内存态；持久面在事务表槽与 undo 链上）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Txn {
    /// 三段式标识（定位事务表槽）。
    pub txn_id: TxnId,
    /// 事务表槽号。
    pub slot: u16,
    /// 语句快照（§12.1）。
    pub snapshot: CommitSeq,
    /// 状态（内存镜像；真值在事务表槽）。
    pub state: TxnState,
}

impl Txn {
    /// 三段式原始值（redo 记录头用）。
    #[must_use]
    pub fn raw(&self) -> u64 {
        self.txn_id.as_raw()
    }
}

/// **开始事务**：分配事务表槽（§4.6.6 ①——"开始不需要任何持久化保证"，
/// 但槽位分配本身落在 undo 段头页上，走链的分配口）。
pub fn begin(
    pool: &BufferPool<'_>,
    log: &mut GroupWriter<'_, '_>,
    chain: &mut UndoChain<'_, '_>,
    snapshot: CommitSeq,
) -> Result<Txn, TxnError> {
    let header_before = chain.segment().read_page(0)?;
    let mut header_after = Page::from_bytes(Box::new(*header_before.as_bytes()));
    let (slot, txn_slot) = bicdb_storage::undo::allocate_slot(&mut header_after)?;
    let txn_id = txn_id_of(slot, &txn_slot);
    let txn = Txn {
        txn_id,
        slot,
        snapshot,
        state: TxnState::Active,
    };
    let key = undo_page_key(chain, 0)?;
    write_undo_page_change(
        pool,
        log,
        txn.raw(),
        key,
        header_before.as_bytes(),
        header_after.as_bytes(),
        false,
    )?;
    Ok(txn)
}

/// **插入一行**（经缓冲池；见模块文档的四步）。
///
/// `block` 是目标数据页的键（调用方保证它已可写：段内已分配、页可格式化）；
/// 返回新行的 ROWID。
pub fn insert_row(
    pool: &BufferPool<'_>,
    log: &mut GroupWriter<'_, '_>,
    chain: &mut UndoChain<'_, '_>,
    txn: &mut Txn,
    block: BufferKey,
    row: &[u8],
    policy: &InsertPolicy,
) -> Result<RowId, TxnError> {
    // 单闩锁纪律：**不在持有卫兵时调用池**——先在快照上改，再分步回写。
    let (data_before, mut local) = {
        let g = pool.pin(block)?;
        (*g.as_bytes(), Page::from_bytes(Box::new(*g.as_bytes())))
    };

    // ② 占用 ITL 槽（在快照上，捕获前像）。
    let (slot, old) = acquire_itl_slot(&mut local, txn.txn_id)?;
    let block_rowid = RowId::from_parts(block.rdba.file_id(), block.rdba.block_id(), 1)?;
    let head = append_undo_via_pool(
        pool,
        log,
        chain,
        txn,
        UndoOp::ItlOverwrite,
        block_rowid,
        UndoPayload::ItlOverwrite {
            itl_slot: slot as u8,
            old,
        },
    )?;
    itl::write_itl(
        &mut local,
        slot,
        &ItlEntry {
            txn_id: txn.txn_id,
            undo_ptr: Some(head),
            commit_seq: None,
            lock_cnt: 1,
            state: ItlState::Active,
        },
    )?;

    // ③ 插行（快照上）+ 记"插入"撤销记录。
    let n = heap::insert_row(&mut local, row, policy)?;
    let rid = RowId::from_parts(block.rdba.file_id(), block.rdba.block_id(), n)?;
    append_undo_via_pool(
        pool,
        log,
        chain,
        txn,
        UndoOp::Insert,
        rid,
        UndoPayload::None,
    )?;

    // ④ 数据页差异 → redo → page_lsn → 标脏。
    write_page_change(
        pool,
        log,
        txn.raw(),
        block,
        &data_before,
        local.as_bytes(),
        false,
    )?;
    Ok(rid)
}

/// **删除一行**（经缓冲池）：整行旧值进 undo（"删除"补偿 = 原位写回）。
pub fn delete_row(
    pool: &BufferPool<'_>,
    log: &mut GroupWriter<'_, '_>,
    chain: &mut UndoChain<'_, '_>,
    txn: &mut Txn,
    block: BufferKey,
    row_no: u16,
) -> Result<(), TxnError> {
    let (data_before, mut local) = {
        let g = pool.pin(block)?;
        (*g.as_bytes(), Page::from_bytes(Box::new(*g.as_bytes())))
    };
    let old_row = heap::row(&local, row_no)
        .ok_or(TxnError::Heap(HeapError::NoSuchRow))?
        .to_vec();
    let block_rowid = RowId::from_parts(block.rdba.file_id(), block.rdba.block_id(), 1)?;
    let (slot, old) = ensure_itl_entry(&mut local, txn.txn_id)?;
    if let Some(old) = old {
        let head = append_undo_via_pool(
            pool,
            log,
            chain,
            txn,
            UndoOp::ItlOverwrite,
            block_rowid,
            UndoPayload::ItlOverwrite {
                itl_slot: slot as u8,
                old: Some(old),
            },
        )?;
        itl::write_itl(
            &mut local,
            slot,
            &ItlEntry {
                txn_id: txn.txn_id,
                undo_ptr: Some(head),
                commit_seq: None,
                lock_cnt: 1,
                state: ItlState::Active,
            },
        )?;
    }
    let rid = RowId::from_parts(block.rdba.file_id(), block.rdba.block_id(), row_no)?;
    append_undo_via_pool(
        pool,
        log,
        chain,
        txn,
        UndoOp::Delete,
        rid,
        UndoPayload::FullRow(old_row),
    )?;
    heap::delete_row(&mut local, row_no)?;
    write_page_change(
        pool,
        log,
        txn.raw(),
        block,
        &data_before,
        local.as_bytes(),
        false,
    )?;
    Ok(())
}

/// **更新一行**（v1 限**等长就地**）：行内差异进 undo（补偿 = 按偏移写回旧值）。
///
/// 新行长度 ≠ 旧行长度的更新（变长、行迁移）留给"行迁移"切片——**明确报错**，
/// 不给半套。
pub fn update_row(
    pool: &BufferPool<'_>,
    log: &mut GroupWriter<'_, '_>,
    chain: &mut UndoChain<'_, '_>,
    txn: &mut Txn,
    block: BufferKey,
    row_no: u16,
    new_row: &[u8],
) -> Result<(), TxnError> {
    let (data_before, mut local) = {
        let g = pool.pin(block)?;
        (*g.as_bytes(), Page::from_bytes(Box::new(*g.as_bytes())))
    };
    let old_row = heap::row(&local, row_no)
        .ok_or(TxnError::Heap(HeapError::NoSuchRow))?
        .to_vec();
    if old_row.len() != new_row.len() {
        return Err(TxnError::UpdateNotInPlace {
            old: old_row.len(),
            new: new_row.len(),
        });
    }
    let block_rowid = RowId::from_parts(block.rdba.file_id(), block.rdba.block_id(), 1)?;
    let (slot, old) = ensure_itl_entry(&mut local, txn.txn_id)?;
    if let Some(old) = old {
        let head = append_undo_via_pool(
            pool,
            log,
            chain,
            txn,
            UndoOp::ItlOverwrite,
            block_rowid,
            UndoPayload::ItlOverwrite {
                itl_slot: slot as u8,
                old: Some(old),
            },
        )?;
        itl::write_itl(
            &mut local,
            slot,
            &ItlEntry {
                txn_id: txn.txn_id,
                undo_ptr: Some(head),
                commit_seq: None,
                lock_cnt: 1,
                state: ItlState::Active,
            },
        )?;
    }

    // 行内差异（连续的变更段；旧值随记录）——**先 undo 后变更**。
    let patches = row_patches(&old_row, new_row);
    let rid = RowId::from_parts(block.rdba.file_id(), block.rdba.block_id(), row_no)?;
    append_undo_via_pool(
        pool,
        log,
        chain,
        txn,
        UndoOp::Update,
        rid,
        UndoPayload::Update {
            old_itl_slot: old_row[1],
            patches,
        },
    )?;

    // 就地写新行（等长）：行头 `itl_slot` 指向本事务的槽。
    let offset = local
        .slot(heap::slot_index(row_no).ok_or(TxnError::Heap(HeapError::NoSuchRow))?)
        .ok_or(TxnError::Heap(HeapError::NoSuchRow))?
        .offset() as usize;
    let mut patched = new_row.to_vec();
    patched[1] = slot as u8;
    local.as_bytes_mut()[offset..offset + patched.len()].copy_from_slice(&patched);
    write_page_change(
        pool,
        log,
        txn.raw(),
        block,
        &data_before,
        local.as_bytes(),
        false,
    )?;
    Ok(())
}

/// 行内连续差异段（（行内偏移, 旧值）列表）。
fn row_patches(old: &[u8], new: &[u8]) -> Vec<(u16, Vec<u8>)> {
    let mut patches = Vec::new();
    let mut start: Option<usize> = None;
    for i in 0..old.len().min(new.len()) {
        if old[i] != new[i] {
            if start.is_none() {
                start = Some(i);
            }
        } else if let Some(s) = start.take() {
            patches.push((s as u16, old[s..i].to_vec()));
        }
    }
    if let Some(s) = start {
        patches.push((s as u16, old[s..old.len()].to_vec()));
    }
    patches
}

/// 保证本事务在该块有一个 ITL 条目：
/// - 已有本事务的活动条目 ⇒ `(slot, None)`（不必记"ITL 覆盖"）；
/// - 占用空槽/可复用槽 ⇒ `(slot, Some(前像))`（调用方先记 "ITL 覆盖"）。
fn ensure_itl_entry(page: &mut Page, txn_id: TxnId) -> Result<(u16, Option<[u8; 24]>), TxnError> {
    let count = itl::itl_count(page)?;
    for i in 0..count {
        let e = itl::read_itl(page, i)?;
        if e.state == ItlState::Active && e.txn_id == txn_id {
            return Ok((i, None));
        }
    }
    acquire_itl_slot(page, txn_id)
}

/// **提交**（§11.1.1 的次序）：提交记录入流 → **等它身持久化** → 事务表槽置
/// 已提交（经池、带 redo）→ 返回提交序号。
pub fn commit(
    pool: &BufferPool<'_>,
    log: &mut GroupWriter<'_, '_>,
    chain: &mut UndoChain<'_, '_>,
    txn: &mut Txn,
    commit_seq: CommitSeq,
) -> Result<(), TxnError> {
    let lsn = log.append(|l| RedoRecord::commit(l, txn.raw(), commit_seq.as_raw()))?;
    log.flush(lsn)?; // 提交在"提交记录耐久"之后才算成功

    let header_before = chain.segment().read_page(0)?;
    let mut header_after = Page::from_bytes(Box::new(*header_before.as_bytes()));
    let mut slot = read_slot(&header_after, txn.slot)?;
    slot.state = TxnState::Committed;
    slot.commit_seq = commit_seq;
    write_slot(&mut header_after, txn.slot, &slot)?;
    let key = undo_page_key(chain, 0)?;
    write_undo_page_change(
        pool,
        log,
        txn.raw(),
        key,
        header_before.as_bytes(),
        header_after.as_bytes(),
        false,
    )?;

    txn.state = TxnState::Committed;
    Ok(())
}

/// **回滚**：沿链从新到旧补偿（每条经池、带 redo），最后释放事务表槽。
pub fn rollback(
    pool: &BufferPool<'_>,
    log: &mut GroupWriter<'_, '_>,
    chain: &mut UndoChain<'_, '_>,
    txn: &mut Txn,
) -> Result<u64, TxnError> {
    let header = chain.segment().read_page(0)?;
    let mut at = read_slot(&header, txn.slot)?.undo_current;
    let mut count = 0u64;
    while let Some(pos) = at {
        let record = chain.read(pos)?;
        at = record.prev;
        let key = BufferKey::new(
            workspace_of(chain),
            Rdba::from_parts(record.rowid.file_id(), record.rowid.block_id())
                .ok_or(TxnError::StaleCache)?,
        );
        let (before, mut local) = {
            let g = pool.pin(key)?;
            (*g.as_bytes(), Page::from_bytes(Box::new(*g.as_bytes())))
        };
        apply_undo_to_page(&mut local, &record)?;
        write_page_change(pool, log, txn.raw(), key, &before, local.as_bytes(), false)?;
        count += 1;
    }
    // 释放槽（经池、带 redo）。
    let header_before = chain.segment().read_page(0)?;
    let mut header_after = Page::from_bytes(Box::new(*header_before.as_bytes()));
    free_slot(&mut header_after, txn.slot)?;
    let key = undo_page_key(chain, 0)?;
    write_undo_page_change(
        pool,
        log,
        txn.raw(),
        key,
        header_before.as_bytes(),
        header_after.as_bytes(),
        false,
    )?;
    txn.state = TxnState::Free;
    Ok(count)
}

// -- 内部 -------------------------------------------------------------------

/// **undo 段的页改动**：经池写入后**立即落盘**（见模块文档）。
#[allow(clippy::too_many_arguments)]
fn write_undo_page_change(
    pool: &BufferPool<'_>,
    log: &mut GroupWriter<'_, '_>,
    txn_raw: u64,
    key: BufferKey,
    before: &[u8; bicdb_storage::page::PAGE_SIZE],
    after: &[u8],
    is_new: bool,
) -> Result<(), TxnError> {
    if write_page_change(pool, log, txn_raw, key, before, after, is_new)?.is_some() {
        pool.flush(key)?;
    }
    Ok(())
}

/// 数据页/undo 段的键（undo 段逻辑页 → 池的键）。
fn undo_page_key(chain: &UndoChain<'_, '_>, logical: u32) -> Result<BufferKey, TxnError> {
    let block = chain
        .segment()
        .logical_block(logical)
        .ok_or(TxnError::StaleCache)?;
    let rdba = Rdba::from_parts(chain.segment().file_id(), block).ok_or(TxnError::StaleCache)?;
    Ok(BufferKey::new(workspace_of(chain), rdba))
}

fn workspace_of(chain: &UndoChain<'_, '_>) -> [u8; 8] {
    chain.segment().workspace_ref()
}

/// 在数据页上占用 ITL 槽：返回（槽号、前像字节；`None` = 原为空闲）。
fn acquire_itl_slot(page: &mut Page, _txn_id: TxnId) -> Result<(u16, Option<[u8; 24]>), TxnError> {
    if let Some(slot) = itl::find_reusable(page)? {
        let old = itl::snapshot(page, slot)?;
        let entry = itl::read_itl(page, slot)?;
        // 前像：原为空闲（flags = Free）⇒ None（"原为空闲"的编码）。
        let old = if entry.state == ItlState::Free {
            None
        } else {
            Some(old)
        };
        return Ok((slot, old));
    }
    // 没有可复用槽 → 扩展（itl_max 由调用方经段/表选项控制；此处用上限 32）。
    let slot = itl::grow(page, itl::ITL_MAX_LIMIT)?;
    Ok((slot, None))
}

/// **记一条撤销记录并经池落地**（计划页 → 每页 redo + 标脏）。
#[allow(clippy::too_many_arguments)]
fn append_undo_via_pool(
    pool: &BufferPool<'_>,
    log: &mut GroupWriter<'_, '_>,
    chain: &mut UndoChain<'_, '_>,
    txn: &Txn,
    op: UndoOp,
    rowid: RowId,
    payload: UndoPayload,
) -> Result<RowId, TxnError> {
    let plan = chain.plan_append(txn.slot, op, 0, rowid, payload)?;
    // ① 撤销页（可能新开）。
    let key = undo_page_key(chain, plan.logical)?;
    write_undo_page_change(
        pool,
        log,
        txn.raw(),
        key,
        plan.undo_page.0.as_bytes(),
        plan.undo_page.1.as_bytes(),
        plan.opened,
    )?;
    // ② 段内位图页（仅新开页时有改动）。
    if let Some((before, after)) = &plan.bitmap {
        let key = undo_page_key(chain, 1)?;
        write_undo_page_change(
            pool,
            log,
            txn.raw(),
            key,
            before.as_bytes(),
            after.as_bytes(),
            false,
        )?;
    }
    // ③ 段头页（链头/计数）。
    let key = undo_page_key(chain, 0)?;
    write_undo_page_change(
        pool,
        log,
        txn.raw(),
        key,
        plan.header.0.as_bytes(),
        plan.header.1.as_bytes(),
        false,
    )?;
    let head = plan.head;
    chain.note_append(&plan);
    Ok(head)
}

/// **一页的变更落地**：差异 → redo 记录 → 经池覆盖内容 → 推进 `page_lsn`/`mod_seq` → 标脏。
///
/// `is_new` = 该页尚未落盘（新分配的页）：经 [`BufferPool::insert_new`] 装入，
/// 且"前像"必须全零（重放把它重建出来）。
fn write_page_change(
    pool: &BufferPool<'_>,
    log: &mut GroupWriter<'_, '_>,
    txn_raw: u64,
    key: BufferKey,
    before: &[u8; bicdb_storage::page::PAGE_SIZE],
    after: &[u8],
    is_new: bool,
) -> Result<Option<Lsn>, TxnError> {
    let changes = page_diff(before, after);
    if changes.is_empty() {
        return Ok(None);
    }
    let lsn = log.append(|l| {
        RedoRecord::page_modification(
            l,
            txn_raw,
            vec![BlockRef {
                flags: 0,
                rdba: key.rdba,
                changes: changes.clone(), // 闭包是 `Fn`（满则刷+重试）
            }],
        )
    })?;

    let mut guard = if is_new {
        // 新页：不经 read，直接装入（前像 = 该页的初始镜像，恢复重放时
        // 磁盘上就是不存在的零页/未格式化页——重放会把差异叠上去）。
        pool.insert_new(key, Page::from_bytes(Box::new(*before)))?
    } else {
        pool.pin(key)?
    };
    // 调用方给的（前像、后像）是**权威镜像**：池里纵有旧副本也被整体覆盖
    // （单写者纪律；段扩展等直写路径在计划里已重读镜像）。
    guard.as_bytes_mut().copy_from_slice(after);
    let mut header = guard.header().ok_or(TxnError::StaleCache)?;
    header.page_lsn = lsn;
    guard.write_header(&header);
    guard.bump_mod_seq();
    guard.mark_dirty(lsn);
    Ok(Some(lsn))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use bicdb_common::seq::Lsn;
    use bicdb_storage::buffer::WalGuard;
    use bicdb_storage::controlfile::{ArchiveRecord, ControlFile, RedoEntries, WorkspaceEntry};
    use bicdb_storage::datafile::DataFile;
    use bicdb_storage::heap;
    use bicdb_storage::page::PageType;
    use bicdb_storage::pagefile;
    use bicdb_storage::row::assemble_row;
    use bicdb_storage::undo::{create_undo_segment, read_slot};
    use bicdb_workspace::id::WorkspaceId;
    use bicdb_workspace::io::MemFileIo;

    use super::*;
    use bicdb_wal::group::{online_groups, GroupSpec, GroupWriter};
    use bicdb_wal::record::RecordOp;
    use bicdb_wal::recovery::recover;

    const UNDO_F: &str = "/mem/undo1.dat";
    const DATA_F: &str = "/mem/data.dat";
    const A: &str = "/mem/control01.ctl";
    const B: &str = "/mem/control02.ctl";
    const WAL: &str = "/mem/wal";
    const WS: [u8; 8] = [8u8; 8];

    fn mem() -> MemFileIo {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        io.add_dir(WAL);
        io
    }
    fn lsn(v: u64) -> Lsn {
        Lsn::from_raw(v).unwrap()
    }
    fn seq(v: u64) -> CommitSeq {
        CommitSeq::from_raw(v).unwrap()
    }
    fn rdba(file_id: u16, block: u32) -> Rdba {
        Rdba::from_parts(file_id, block).unwrap()
    }
    fn ws_entry() -> WorkspaceEntry {
        WorkspaceEntry {
            workspace_id: WorkspaceId::from_raw(1).unwrap(),
            created_at: 0,
            derived_from: None,
            derived_at_seq: seq(0),
        }
    }
    fn row_bytes(payload: &[u8]) -> Vec<u8> {
        assemble_row(0, 1, &[false], &[], &[payload])
    }

    /// 假 WAL：水位视为已全落盘（池不催刷；提交路径单独 flush）。
    struct FakeWal;
    impl WalGuard for FakeWal {
        fn durable_lsn(&self) -> Lsn {
            lsn(u64::MAX >> 16)
        }
        fn ensure_durable(&mut self, _t: Lsn) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn spec() -> GroupSpec {
        GroupSpec::new(2, 1, 64).unwrap()
    }

    #[test]
    fn insert_and_commit_through_the_cache() {
        let io = mem();
        let mut undo_file = DataFile::create(&io, Path::new(UNDO_F), 1, 1, WS, 512).unwrap();
        let undo_handle = undo_file.handle();
        let segment = create_undo_segment(&mut undo_file, 2, 3, 4).unwrap();
        let mut chain = UndoChain::open(segment);
        let data_handle = {
            let data_file = DataFile::create(&io, Path::new(DATA_F), 3, 3, WS, 512).unwrap();
            let h = data_file.handle();
            let mut page = Page::new(PageType::HeapTable, WS, 3, 1);
            pagefile::write_page(&io, h, 1, &mut page).unwrap();
            h
        };
        let pool = BufferPool::new(
            &io,
            8,
            move |_ws, r| match r.file_id() {
                1 => Some((undo_handle, r.block_id())),
                3 => Some((data_handle, r.block_id())),
                _ => None,
            },
            FakeWal,
        )
        .unwrap();
        let mut cf = ControlFile::format(
            &io,
            Path::new(A),
            Path::new(B),
            &ws_entry(),
            &RedoEntries::new(2, 1).unwrap(),
            &ArchiveRecord::default(),
        )
        .unwrap();
        let mut log = GroupWriter::create(&io, &mut cf, Path::new(WAL), spec(), lsn(0)).unwrap();

        let key = BufferKey::new(WS, rdba(3, 1));
        let row = row_bytes(b"hello");
        let mut txn = begin(&pool, &mut log, &mut chain, seq(1)).unwrap();
        let rid = insert_row(
            &pool,
            &mut log,
            &mut chain,
            &mut txn,
            key,
            &row,
            &InsertPolicy::in_place(0),
        )
        .unwrap();
        assert_eq!(rid.row_id(), 1);

        // 缓存里的页已脏、page_lsn 已推进；文件里还没有（no-force）。
        assert!(pool.dirty_len(WS) >= 1, "数据页/undo 页已进写列表");
        commit(&pool, &mut log, &mut chain, &mut txn, seq(1)).unwrap();

        // 写回工作区（DBWR 角色）→ 文件里能看到行。
        pool.flush_workspace(WS).unwrap();
        let page = pagefile::read_page_verified(&io, data_handle, 1).unwrap();
        assert_eq!(heap::row(&page, 1), Some(&row[..]), "行已落盘");
        // ITL[0] 归该事务（延迟块清除：条目仍是"活动"外观）。
        let entry = bicdb_storage::itl::read_itl(&page, 0).unwrap();
        assert_eq!(entry.txn_id, txn.txn_id);
        assert_eq!(entry.state, bicdb_storage::itl::ItlState::Active);
        // 事务表槽已提交。
        let header = chain.segment().read_page(0).unwrap();
        assert_eq!(
            read_slot(&header, txn.slot).unwrap().state,
            TxnState::Committed
        );
        assert_eq!(read_slot(&header, txn.slot).unwrap().commit_seq, seq(1));

        // 日志：ItlOverwrite + Insert + 提交三类记录都在流里。
        log.flush(log.appended_lsn()).unwrap();
        let cf_ro = ControlFile::open(&io, Path::new(A), Path::new(B)).unwrap();
        let groups = online_groups(&io, &cf_ro, Path::new(WAL), spec()).unwrap();
        let scan = bicdb_wal::file::scan_log(
            &io,
            groups[0].handle,
            groups[0].start_lsn,
            u64::from(groups[0].file_pages),
        )
        .unwrap();
        assert!(scan
            .records
            .iter()
            .any(|r| r.op == RecordOp::Commit.as_u8() && r.txn_id == txn.raw()));
    }

    #[test]
    fn rollback_undoes_the_insert_through_the_cache() {
        let io = mem();
        let mut undo_file = DataFile::create(&io, Path::new(UNDO_F), 1, 1, WS, 512).unwrap();
        let undo_handle = undo_file.handle();
        let segment = create_undo_segment(&mut undo_file, 2, 3, 4).unwrap();
        let mut chain = UndoChain::open(segment);
        let data_handle = {
            let data_file = DataFile::create(&io, Path::new(DATA_F), 3, 3, WS, 512).unwrap();
            let h = data_file.handle();
            let mut page = Page::new(PageType::HeapTable, WS, 3, 1);
            pagefile::write_page(&io, h, 1, &mut page).unwrap();
            h
        };
        let pool = BufferPool::new(
            &io,
            8,
            move |_ws, r| match r.file_id() {
                1 => Some((undo_handle, r.block_id())),
                3 => Some((data_handle, r.block_id())),
                _ => None,
            },
            FakeWal,
        )
        .unwrap();
        let mut cf = ControlFile::format(
            &io,
            Path::new(A),
            Path::new(B),
            &ws_entry(),
            &RedoEntries::new(2, 1).unwrap(),
            &ArchiveRecord::default(),
        )
        .unwrap();
        let mut log = GroupWriter::create(&io, &mut cf, Path::new(WAL), spec(), lsn(0)).unwrap();

        let key = BufferKey::new(WS, rdba(3, 1));
        let row = row_bytes(b"temp");
        let mut txn = begin(&pool, &mut log, &mut chain, seq(0)).unwrap();
        let rid = insert_row(
            &pool,
            &mut log,
            &mut chain,
            &mut txn,
            key,
            &row,
            &InsertPolicy::in_place(0),
        )
        .unwrap();
        let n = rollback(&pool, &mut log, &mut chain, &mut txn).unwrap();
        assert_eq!(n, 2, "回放：ITL 覆盖 + 插入两条");
        pool.flush_workspace(WS).unwrap();
        let page = pagefile::read_page_verified(&io, data_handle, 1).unwrap();
        assert_eq!(heap::row(&page, rid.row_id()), None, "行已被撤销");
        assert_eq!(
            bicdb_storage::itl::read_itl(&page, 0).unwrap().state,
            bicdb_storage::itl::ItlState::Free,
            "ITL 还原为空闲"
        );
        let header = chain.segment().read_page(0).unwrap();
        assert_eq!(read_slot(&header, txn.slot).unwrap().state, TxnState::Free);
    }

    #[test]
    fn crash_after_dml_is_recovered_from_the_log() {
        // **端到端**：DML 只进缓存（页未回写）→ 崩溃 → 恢复（分析/重做/撤销）
        // → 已提交的在、未提交的回滚。
        let io = mem();
        let mut undo_file = DataFile::create(&io, Path::new(UNDO_F), 1, 1, WS, 512).unwrap();
        let undo_handle = undo_file.handle();
        let segment = create_undo_segment(&mut undo_file, 2, 3, 4).unwrap();
        let mut chain = UndoChain::open(segment);
        let data_handle = {
            let data_file = DataFile::create(&io, Path::new(DATA_F), 3, 3, WS, 512).unwrap();
            let h = data_file.handle();
            let mut page = Page::new(PageType::HeapTable, WS, 3, 1);
            pagefile::write_page(&io, h, 1, &mut page).unwrap();
            h
        };
        let pool = BufferPool::new(
            &io,
            8,
            move |_ws, r| match r.file_id() {
                1 => Some((undo_handle, r.block_id())),
                3 => Some((data_handle, r.block_id())),
                _ => None,
            },
            FakeWal,
        )
        .unwrap();
        let mut cf = ControlFile::format(
            &io,
            Path::new(A),
            Path::new(B),
            &ws_entry(),
            &RedoEntries::new(2, 1).unwrap(),
            &ArchiveRecord::default(),
        )
        .unwrap();
        let mut log = GroupWriter::create(&io, &mut cf, Path::new(WAL), spec(), lsn(0)).unwrap();

        let key = BufferKey::new(WS, rdba(3, 1));
        // 胜者：插入并提交。
        let win_row = row_bytes(b"win");
        let mut t1 = begin(&pool, &mut log, &mut chain, seq(1)).unwrap();
        let win_rid = insert_row(
            &pool,
            &mut log,
            &mut chain,
            &mut t1,
            key,
            &win_row,
            &InsertPolicy::in_place(0),
        )
        .unwrap();
        commit(&pool, &mut log, &mut chain, &mut t1, seq(1)).unwrap();
        // 输家：插入但不提交。
        let lose_row = row_bytes(b"lose");
        let mut t2 = begin(&pool, &mut log, &mut chain, seq(2)).unwrap();
        let lose_rid = insert_row(
            &pool,
            &mut log,
            &mut chain,
            &mut t2,
            key,
            &lose_row,
            &InsertPolicy::in_place(0),
        )
        .unwrap();

        // **崩溃**：日志刷出（它才是耐久源；数据页/undo 页始终没回写）。
        log.flush(log.appended_lsn()).unwrap();
        drop(pool); // 丢掉缓存，不 flush —— 页文件停在 DML 前

        // 恢复。
        let cf_ro = ControlFile::open(&io, Path::new(A), Path::new(B)).unwrap();
        let groups = online_groups(&io, &cf_ro, Path::new(WAL), spec()).unwrap();
        let mut resolve = |r: Rdba| match r.file_id() {
            1 => Some((undo_handle, r.block_id())),
            3 => Some((data_handle, r.block_id())),
            _ => None,
        };
        let report = recover(&io, &groups, lsn(0), &chain, &mut log, &mut resolve).unwrap();
        assert_eq!(report.undo.txns_rolled_back, 1, "只有一个输家");

        let page = pagefile::read_page_verified(&io, data_handle, 1).unwrap();
        assert_eq!(
            heap::row(&page, win_rid.row_id()),
            Some(&win_row[..]),
            "胜者的行被重做出来"
        );
        assert_eq!(heap::row(&page, lose_rid.row_id()), None, "输家的行被撤销");
        // 胜者补标记、输家槽释放。
        let header = chain.segment().read_page(0).unwrap();
        assert_eq!(
            read_slot(&header, t1.slot).unwrap().state,
            TxnState::Committed
        );
        assert_eq!(read_slot(&header, t1.slot).unwrap().commit_seq, seq(1));
        assert_eq!(read_slot(&header, t2.slot).unwrap().state, TxnState::Free);
    }

    #[test]
    fn delete_and_update_round_trip_through_the_cache() {
        let io = mem();
        let mut undo_file = DataFile::create(&io, Path::new(UNDO_F), 1, 1, WS, 512).unwrap();
        let undo_handle = undo_file.handle();
        let segment = create_undo_segment(&mut undo_file, 2, 3, 4).unwrap();
        let mut chain = UndoChain::open(segment);
        let data_handle = {
            let data_file = DataFile::create(&io, Path::new(DATA_F), 3, 3, WS, 512).unwrap();
            let h = data_file.handle();
            let mut page = Page::new(PageType::HeapTable, WS, 3, 1);
            pagefile::write_page(&io, h, 1, &mut page).unwrap();
            h
        };
        let pool = BufferPool::new(
            &io,
            8,
            move |_ws, r| match r.file_id() {
                1 => Some((undo_handle, r.block_id())),
                3 => Some((data_handle, r.block_id())),
                _ => None,
            },
            FakeWal,
        )
        .unwrap();
        let mut cf = ControlFile::format(
            &io,
            Path::new(A),
            Path::new(B),
            &ws_entry(),
            &RedoEntries::new(2, 1).unwrap(),
            &ArchiveRecord::default(),
        )
        .unwrap();
        let mut log = GroupWriter::create(&io, &mut cf, Path::new(WAL), spec(), lsn(0)).unwrap();
        let key = BufferKey::new(WS, rdba(3, 1));

        // 事务 1：插入 + 更新（等长）+ 删除，全部经写路径。
        let row_v1 = row_bytes(b"aaa");
        let row_v2 = row_bytes(b"bbb"); // 等长（同一 payload 长度）
        let mut t1 = begin(&pool, &mut log, &mut chain, seq(1)).unwrap();
        let rid = insert_row(
            &pool,
            &mut log,
            &mut chain,
            &mut t1,
            key,
            &row_v1,
            &InsertPolicy::in_place(0),
        )
        .unwrap();
        update_row(
            &pool,
            &mut log,
            &mut chain,
            &mut t1,
            key,
            rid.row_id(),
            &row_v2,
        )
        .unwrap();
        delete_row(&pool, &mut log, &mut chain, &mut t1, key, rid.row_id()).unwrap();
        // 回滚整条链：删除→更新→插入 逐条撤销 ⇒ 回到"没有这一行"。
        rollback(&pool, &mut log, &mut chain, &mut t1).unwrap();
        pool.flush_workspace(WS).unwrap();
        let page = pagefile::read_page_verified(&io, data_handle, 1).unwrap();
        assert_eq!(heap::row(&page, rid.row_id()), None, "净效果 = 无此行");

        // 事务 2：插入 → 更新 → 提交；检查落盘内容 = 更新后的行。
        let mut t2 = begin(&pool, &mut log, &mut chain, seq(2)).unwrap();
        let rid2 = insert_row(
            &pool,
            &mut log,
            &mut chain,
            &mut t2,
            key,
            &row_v1,
            &InsertPolicy::in_place(0),
        )
        .unwrap();
        update_row(
            &pool,
            &mut log,
            &mut chain,
            &mut t2,
            key,
            rid2.row_id(),
            &row_v2,
        )
        .unwrap();
        // 非等长更新：明确拒绝。
        let row_long = row_bytes(b"ccccc");
        assert!(matches!(
            update_row(
                &pool,
                &mut log,
                &mut chain,
                &mut t2,
                key,
                rid2.row_id(),
                &row_long
            ),
            Err(TxnError::UpdateNotInPlace { .. })
        ));
        commit(&pool, &mut log, &mut chain, &mut t2, seq(2)).unwrap();
        pool.flush_workspace(WS).unwrap();
        let page = pagefile::read_page_verified(&io, data_handle, 1).unwrap();
        let stored = heap::row(&page, rid2.row_id()).expect("更新后的行");
        // 行头 itl_slot 指向 t2 的槽（更新时改写）；其余字节 = 新行。
        assert_eq!(stored[1], t2.slot as u8, "itl_slot 归本事务");
        assert_eq!(&stored[2..], &row_v2[2..], "行体 = 更新后的内容");
    }
}
