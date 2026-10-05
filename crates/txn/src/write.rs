//! **写路径**（§11.1.1 的提交流程、§4.6.6 的事务生命周期）：DML 经缓冲池落地。
//!
//! ```text
//! 一次修改（insert/delete/update）：
//!   ⓪ **可失败预检**（行头/空间/等长）——先于任何链上追加，失败不留幽灵记录
//!   ① 取数据页（缓冲池钉住）——**写先入缓存**
//!   ② 占用 ITL 条目（`occupy_itl`）：
//!        延迟块清除（已提交旧条目 → `Committed` + 准确序号、锁清零）
//!        → 本事务已有条目则复用（每块至多一个）→ 否则新占用：
//!          记"ITL 覆盖"撤销记录（带 `txn_id`，CR 的终止符）→ 写 `Active` 条目
//!   ③ 改页（插/删/改，行头 `itl_slot` 回填）→ 记对应撤销记录（经池、带 redo）
//!   ④ 数据页差异 → 追加 redo → 推进 page_lsn → **标脏**（写列表排队）
//! 提交：提交记录入流 → **等 LGWR 刷到该记录**（提交点）→ 事务表槽置已提交
//!       （此步失败不回滚——恢复的前滚补标记兜底）
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
    apply_undo_to_page, free_slot, read_slot, reclaim_undo, txn_id_of, write_slot, ReclaimReport,
    RollbackError, TxnId, TxnState, UndoChain, UndoChainError, UndoError, UndoOp, UndoPayload,
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
    /// **新行超过单页可容纳**（就地转链路径——片段链的池实现随后续切片，
    /// 明确报错，不给半套）。
    UpdateTooLong {
        /// 新行长度。
        len: usize,
    },
    /// **迁移需要一个放得下的目标页，但分配口给不出**（段满/无可用页）。
    NoMigrationTarget {
        /// 需要的字节数。
        need: usize,
    },
    /// **行被其他事务锁住**（§5.4.2 ① 的等待分支）：调用方登记等待
    /// （`lock::WaitRegistry`/`WaitGate`）、放 latch、重试；等久了由死锁
    /// 检测（④）处置。[`execute_with_wait`] 是这条路径的现成驱动。
    RowLocked {
        /// 持锁者。
        holder: bicdb_storage::undo::TxnId,
        /// 被等的那一行（等待表诊断"谁堵住了谁"用）。
        row: RowId,
    },
    /// **死锁牺牲者**（§5.4.2 ④）：本事务在环上且修改量最少——驱动器已做
    /// **语句级回滚**（到语句回滚点；**不释锁、不回滚此前语句**）并取消等待，
    /// 环即解开。会话层据此重启该语句（或把错误上抛给应用重试）。
    DeadlockVictim {
        /// 环上事务（诊断/日志；从环的顺序给出）。
        cycle: Vec<bicdb_storage::undo::TxnId>,
    },
    /// **等待次数超限**（会话级参数；默认不设限 = 等到底，死锁检测兜底）。
    LockTimeout {
        /// 持锁者。
        holder: bicdb_storage::undo::TxnId,
        /// 被等的那一行。
        row: RowId,
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
            TxnError::UpdateTooLong { len } => write!(
                f,
                "更新后行长 {len} 超过单页可容纳（就地转链路径随后续切片）"
            ),
            TxnError::NoMigrationTarget { need } => {
                write!(f, "行迁移需要 {need} 字节的可用页，分配口给不出")
            }
            TxnError::RowLocked { holder, row } => {
                write!(f, "行 {row:?} 被事务 {holder:?} 锁住：转入等待（§5.4.2）")
            }
            TxnError::DeadlockVictim { cycle } => {
                write!(
                    f,
                    "死锁牺牲者：环 {cycle:?} 中本事务修改量最少，已语句级回滚（§5.4.2 ④）"
                )
            }
            TxnError::LockTimeout { holder, row } => {
                write!(f, "等行 {row:?} 超过等待次数上限（持锁者 {holder:?}）")
            }
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

    // ① 预检（**先于任何链上追加**）：在丢弃副本上走一遍插入——行头、空间、
    //    槽位任何一项不满足都发生在"记记录"之前，不留幽灵撤消记录。
    {
        let mut probe = Page::from_bytes(Box::new(*local.as_bytes()));
        heap::insert_row(&mut probe, row, policy)?;
    }

    // ② 占用 ITL 槽（复用/清除/新占用都在 `occupy_itl` 里，§5.4.1）——
    //    新行随插入即被本事务锁住。
    let (slot, _) = occupy_itl(pool, log, chain, txn, &mut local, block)?;

    // ③ 插行（快照上，**回填行头的 `itl_slot`**）+ 记"插入"撤销记录。
    let mut patched = row.to_vec();
    patched[1] = slot as u8;
    let n = heap::insert_row(&mut local, &patched, policy)?;
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
    // 占用 ITL 条目（含清除/复用/新占用的记录与条目写入）——行锁与可见性
    // 的落点，**不能省**（本切片修复：此前 delete/update 在空槽上什么都不写，
    // 未提交删除/更新对所有快照可见）。
    // **加锁 + 占用 ITL 条目**（清除/复用/新占用 + `ITL 覆盖` 记录）——
    // 行锁与可见性的落点；行被他人活动事务锁住 ⇒ `RowLocked`（调用方登记
    // 等待后重试，§5.4.2 ①），**不得静默失败**。
    lock_and_occupy(pool, log, chain, txn, &mut local, block, row_no)?;
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

/// **更新一行的结果**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdateOutcome {
    /// **稳定入口**（原地/迁移后都不变——索引与外键引用仍指它，§6.2）。
    pub rowid: RowId,
    /// 是否发生了**行迁移**（改长且原页放不下 ⇒ 新位置 + 原槽位转发指针）。
    pub migrated: bool,
}

/// **更新一行**（§6.2/§6.8）：
///
/// - **不增（等长或收缩）** ⇒ **就地重写**：行内差异（含收缩时的尾部旧字节）
///   进 undo，补偿 = 按偏移写回旧值；
/// - **增长** ⇒ **行迁移**：新行写入目的页（同页放得下则同页，否则由分配口
///   `alloc` 给一页），原槽位改为**转发指针**（6B 新 ROWID）；**原 ROWID 不变**
///   （索引不动）。记录与设计一致：**迁移 = "删除旧位置" + "插入新位置"**
///   （§12.4——回滚先删新行、再把转发指针还原成原行）；
/// - **新行超过单页可容纳** ⇒ 就地转链（片段链）随"片段链的池实现"切片——
///   [`TxnError::UpdateTooLong`]，不给半套。
///
/// `alloc`：迁移目标页的分配口（执行器/段层提供——段内找一个放得下的页或
/// 新开一页；返回与源同页也合法，函数本身已先试同页）。
#[allow(clippy::too_many_arguments)]
pub fn update_row(
    pool: &BufferPool<'_>,
    log: &mut GroupWriter<'_, '_>,
    chain: &mut UndoChain<'_, '_>,
    txn: &mut Txn,
    block: BufferKey,
    row_no: u16,
    new_row: &[u8],
    policy: &InsertPolicy,
    alloc: &mut dyn FnMut(usize) -> Result<BufferKey, TxnError>,
) -> Result<UpdateOutcome, TxnError> {
    let (src_before, mut src_local) = {
        let g = pool.pin(block)?;
        (*g.as_bytes(), Page::from_bytes(Box::new(*g.as_bytes())))
    };
    let old_row = heap::row(&src_local, row_no)
        .ok_or(TxnError::Heap(HeapError::NoSuchRow))?
        .to_vec();
    // 新行必须是**结构合法**的完整行——否则写进读不回。
    let header = bicdb_storage::row::RowHeader::read_from(new_row)
        .map_err(|_| TxnError::Heap(HeapError::BadRow))?;
    if header.row_len as usize != new_row.len() {
        return Err(TxnError::Heap(HeapError::BadRow));
    }
    let src_rid = RowId::from_parts(block.rdba.file_id(), block.rdba.block_id(), row_no)?;

    if new_row.len() <= old_row.len() {
        // 不增：就地重写（等长到收缩同径——收缩时尾部旧字节一并进补丁，
        // 撤销按偏移写回即恢复原长；行区留下的洞由 defrag 处理，§6.8）。
        // 加锁 + 占用 ITL（行被他人锁住 ⇒ `RowLocked`，同 delete）。
        let slot = lock_and_occupy(pool, log, chain, txn, &mut src_local, block, row_no)?;
        let patches = row_patches(&old_row, new_row);
        append_undo_via_pool(
            pool,
            log,
            chain,
            txn,
            UndoOp::Update,
            src_rid,
            UndoPayload::Update {
                old_itl_slot: old_row[1],
                patches,
            },
        )?;
        let offset = src_local
            .slot(heap::slot_index(row_no).ok_or(TxnError::Heap(HeapError::NoSuchRow))?)
            .ok_or(TxnError::Heap(HeapError::NoSuchRow))?
            .offset() as usize;
        let mut patched = new_row.to_vec();
        patched[1] = slot as u8;
        src_local.as_bytes_mut()[offset..offset + patched.len()].copy_from_slice(&patched);
        write_page_change(
            pool,
            log,
            txn.raw(),
            block,
            &src_before,
            src_local.as_bytes(),
            false,
        )?;
        return Ok(UpdateOutcome {
            rowid: src_rid,
            migrated: false,
        });
    }

    // 增长：先看"单页放得下吗"（放不下 ⇒ 转链路径，未接）。
    let page_type = src_local
        .header()
        .map(|h| h.page_type)
        .ok_or(TxnError::StaleCache)?;
    let fresh = Page::new(page_type, block.workspace, block.rdba.file_id(), 0);
    if new_row.len() > heap::capacity_for_row(&fresh, policy) {
        return Err(TxnError::UpdateTooLong { len: new_row.len() });
    }

    // 目的页：同页放得下 ⇒ 同页（省一次随机 I/O）；否则向分配口要一页。
    let dest_same = heap::can_migrate_in_page(&src_local, row_no, new_row.len(), policy);
    let dest_key = if dest_same {
        block
    } else {
        alloc(new_row.len())?
    };
    let mut dest_local = if dest_same {
        Page::from_bytes(Box::new(src_before))
    } else {
        let g = pool.pin(dest_key)?;
        Page::from_bytes(Box::new(*g.as_bytes()))
    };
    let dest_before = *dest_local.as_bytes();

    // ① 源页：占用 ITL + "删除"记录（= 迁移的"旧位置"半边）。
    let src_slot = lock_and_occupy(pool, log, chain, txn, &mut src_local, block, row_no)?;
    append_undo_via_pool(
        pool,
        log,
        chain,
        txn,
        UndoOp::Delete,
        src_rid,
        UndoPayload::FullRow(old_row),
    )?;
    // ② 目的页：占用 ITL（换页时是另一页的 ITL 条目）+ 插入新行 + "插入"记录。
    let dest_slot = if dest_same {
        src_slot
    } else {
        // 目的页上的**新行**：随插入即被本事务锁住（无既有锁可争）。
        occupy_itl(pool, log, chain, txn, &mut dest_local, dest_key)?.0
    };
    let mut patched = new_row.to_vec();
    patched[1] = dest_slot as u8;
    let insert_into = if dest_same {
        &mut src_local
    } else {
        &mut dest_local
    };
    let new_no = heap::insert_row(insert_into, &patched, policy)?;
    let new_rid = RowId::from_parts(dest_key.rdba.file_id(), dest_key.rdba.block_id(), new_no)?;
    append_undo_via_pool(
        pool,
        log,
        chain,
        txn,
        UndoOp::Insert,
        new_rid,
        UndoPayload::None,
    )?;
    // ③ 源槽位 → 转发指针（ROWID 稳定入口不变）。同页时 `src_local` 已是
    //    含 ITL/新行/指针的权威镜像，一次写页即可。
    heap::migrate_row(&mut src_local, row_no, new_rid)?;
    write_page_change(
        pool,
        log,
        txn.raw(),
        block,
        &src_before,
        src_local.as_bytes(),
        false,
    )?;
    if !dest_same {
        write_page_change(
            pool,
            log,
            txn.raw(),
            dest_key,
            &dest_before,
            dest_local.as_bytes(),
            false,
        )?;
    }
    Ok(UpdateOutcome {
        rowid: src_rid,
        migrated: true,
    })
}

/// 行内连续差异段（（行内偏移, 旧值）列表）。
fn row_patches(old: &[u8], new: &[u8]) -> Vec<(u32, Vec<u8>)> {
    let mut patches = Vec::new();
    let mut start: Option<usize> = None;
    for i in 0..old.len().min(new.len()) {
        if old[i] != new[i] {
            if start.is_none() {
                start = Some(i);
            }
        } else if let Some(s) = start.take() {
            patches.push((s as u32, old[s..i].to_vec()));
        }
    }
    if let Some(s) = start {
        patches.push((s as u32, old[s..old.len()].to_vec()));
    }
    patches
}

/// **确保撤销段可容纳下一次追加**：下一逻辑页未映射 ⇒ 计划扩展
/// （`plan_extend` 的镜像）→ 每页经池写 redo + 立即 flush。
fn ensure_undo_capacity(
    pool: &BufferPool<'_>,
    log: &mut GroupWriter<'_, '_>,
    chain: &mut UndoChain<'_, '_>,
    txn: &Txn,
) -> Result<(), TxnError> {
    // 以**磁盘上的段头页**为准（plan_append 也直读它）。
    let next = bicdb_storage::segment::read_header(&chain.segment().read_page(0)?)
        .map_err(|e| TxnError::Segment(bicdb_storage::segment::SegmentSpaceError::Format(e)))?
        .append_pos;
    if chain.segment().logical_block(next).is_some() {
        return Ok(());
    }
    let ws = workspace_of(chain);
    let planned = chain.segment_mut().plan_extend()?;
    for (rdba, before, after) in planned.images {
        let key = BufferKey::new(ws, rdba);
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
    Ok(())
}

/// ITL 条目的获取结果——写路径据此决定"要不要记 `ITL 覆盖`"。
#[derive(Debug)]
enum ItlAcquire {
    /// 本事务在该块**已有**活动条目：直接用，不记记录、不写条目。
    Existing(u16),
    /// **新占用**了一个槽：必须记 `ITL 覆盖`（`old` = 前像；`None` = 原为空闲
    /// ——CR 的终止符，§12.3.1）并把条目写成 `Active`。
    Fresh {
        /// 槽号。
        slot: u16,
        /// 被覆盖条目的原始 24B（`None` = 原为空闲）。
        old: Option<[u8; bicdb_storage::page::ITL_ENTRY_LEN]>,
    },
}

/// **延迟块清除**（§11.1.1）：把本块上"事务表已提交"的旧条目清成
/// `Committed` + 准确序号、锁计数归零——已提交的槽从此**可复用**（这是 ITL
/// 槽回收的唯一路径；没有它，同一页 32 个写事务后就是死路）。
///
/// 只清 `Some(Committed)` 的事务表槽：查不到（槽已复用）的条目**不猜**——
/// 宁可让它占着不可复用，也不把可能的活动事务误判成已提交。
/// 清除字节随后续语句的页差异一并入 redo（比"清除不生成 redo"保守，语义等价）。
fn cleanout_committed(page: &mut Page, chain: &UndoChain<'_, '_>) -> Result<(), TxnError> {
    let header = chain.segment().read_page(0)?;
    for i in 0..itl::itl_count(page)? {
        let e = itl::read_itl(page, i)?;
        if e.state != ItlState::Active {
            continue;
        }
        if let Some(slot) = bicdb_storage::undo::find_slot(&header, e.txn_id)? {
            if slot.state == TxnState::Committed {
                itl::cleanout(page, i, slot.commit_seq)?;
            }
        }
    }
    Ok(())
}

/// 保证本事务在该块有一个 ITL 条目（**先清除、再复用、至多一个**）：
/// - 先做延迟块清除，让已提交的旧槽回到可复用集合；
/// - 已有本事务的活动条目 ⇒ [`ItlAcquire::Existing`]（不重复占槽）；
/// - 否则占用空槽/可复用槽 ⇒ [`ItlAcquire::Fresh`]。
fn ensure_itl_entry(
    page: &mut Page,
    chain: &UndoChain<'_, '_>,
    txn_id: TxnId,
) -> Result<ItlAcquire, TxnError> {
    cleanout_committed(page, chain)?;
    for i in 0..itl::itl_count(page)? {
        let e = itl::read_itl(page, i)?;
        if e.state == ItlState::Active && e.txn_id == txn_id {
            return Ok(ItlAcquire::Existing(i));
        }
    }
    acquire_itl_slot(page, txn_id)
}

/// **占用入口**（insert/delete/update 共用）：返回槽号；新占用时先记
/// `ITL 覆盖`（旧值为前像，`None` = 原为空闲）再写 `Active` 条目。
/// 四条纪律都收在这一处：
/// ① 清除先行（可复用集合）；② 一个事务每块**至多一个**条目；
/// ③ 新占用**必记**终止符记录；④ 只改页快照——页回写由调用方在本语句的
///    `write_page_change` 里统一做（含清除字节与本次修改）。
fn occupy_itl(
    pool: &BufferPool<'_>,
    log: &mut GroupWriter<'_, '_>,
    chain: &mut UndoChain<'_, '_>,
    txn: &Txn,
    local: &mut Page,
    block: BufferKey,
) -> Result<(u16, bool), TxnError> {
    match ensure_itl_entry(local, chain, txn.txn_id)? {
        ItlAcquire::Existing(slot) => Ok((slot, false)),
        ItlAcquire::Fresh { slot, old } => {
            // `ITL 覆盖` 是**块级**动作：记录里的行号只借它的 file/block 定位块。
            let block_rowid = RowId::from_parts(block.rdba.file_id(), block.rdba.block_id(), 1)?;
            let head = append_undo_via_pool(
                pool,
                log,
                chain,
                txn,
                UndoOp::ItlOverwrite,
                block_rowid,
                UndoPayload::ItlOverwrite {
                    txn_id: txn.txn_id,
                    itl_slot: slot as u8,
                    old,
                },
            )?;
            itl::write_itl(
                local,
                slot,
                &ItlEntry {
                    txn_id: txn.txn_id,
                    undo_ptr: Some(head),
                    commit_seq: None,
                    lock_cnt: 1, // 第一把行锁随占用计入
                    state: ItlState::Active,
                },
            )?;
            Ok((slot, true))
        }
    }
}

/// **行锁获取结果**（§5.4.2 ①）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockOutcome {
    /// 已持有——`reentrant` = 该行本就指向**本事务**的 ITL 条目（重入，
    /// 不重复计 `lock_cnt`）。
    Acquired {
        /// 是否重入。
        reentrant: bool,
    },
    /// 槽属**他人且其事务仍活动**：调用方转入等待（登记 + 放 latch + 重试）。
    WouldWait {
        /// 持锁者。
        holder: bicdb_storage::undo::TxnId,
    },
}

/// **在一页上判定某行的锁归属**（不做 I/O、不改页——判定与写入分离，
/// 等待发生在 latch 之外，§5.4.2）。
///
/// 判据（§5.4.2 ①）：
/// - 行头 `itl_slot` 无 / 越界 / 条目非活动 / 条目属**已提交或已回收**的事务
///   ⇒ 可加锁（后两类由后续 `occupy_itl` 的"清除先行"复用）；
/// - 条目**活动且是本事务** ⇒ 重入；
/// - 条目**活动且是他人**（事务表里仍 `Active`/`PendingRollback`）⇒ 等待。
pub fn decide_row_lock(
    local: &Page,
    chain: &UndoChain<'_, '_>,
    txn: &Txn,
    block: BufferKey,
    row_no: u16,
) -> Result<LockOutcome, TxnError> {
    let row = heap::row(local, row_no).ok_or(TxnError::Heap(HeapError::NoSuchRow))?;
    let slot_byte = row[1];
    if slot_byte == bicdb_storage::row::ITL_SLOT_NONE {
        return Ok(LockOutcome::Acquired { reentrant: false });
    }
    let index = u16::from(slot_byte);
    if index >= itl::itl_count(local)? {
        // 页状态早于该行（PITR 有界重放的形态）：不算被锁。
        return Ok(LockOutcome::Acquired { reentrant: false });
    }
    let entry = itl::read_itl(local, index)?;
    if entry.state != ItlState::Active {
        return Ok(LockOutcome::Acquired { reentrant: false });
    }
    if entry.txn_id == txn.txn_id {
        // 字面重入；但仍要过**陈旧字节**这关（同下）：本行指向我的槽、我的链上
        // 却没有本行的记录 ⇒ 那是旧占用者留下的槽号，恰好撞上我新占的槽——
        // 本行其实**还没被锁**。
        return Ok(
            if holder_locked_this_row(chain, txn.txn_id, block, row_no)? {
                LockOutcome::Acquired { reentrant: true }
            } else {
                LockOutcome::Acquired { reentrant: false }
            },
        );
    }
    // "活动外观"可能属于**早已提交但未清除**的事务——查事务表定夺
    // （§4.6.3：已提交的槽不算"被锁"；已回收 ⇒ 更不算）。
    match chain.lookup(entry.txn_id)? {
        Some(slot) if matches!(slot.state, TxnState::Active | TxnState::PendingRollback) => {
            // **陈旧字节消歧**：槽**可复用**（§5.4.1：非活动且 `lock_cnt = 0`）
            // ⇒ 旧占用者的槽会被新事务**接管**，而旧占用者未碰过的行其
            // `itl_slot` 字节就停在旧槽号上——那个槽现在写着**新事务**的名字，
            // 行却没被新事务锁过（实测：t1 更新本块另一行后，t2 想改本行被
            // 误判为"等 t1"）。判据：**锁与修改同源**——真的锁了本行的事务，
            // 其撤销链里必有**针对本行**的记录（锁与记录同一步落）；没有 ⇒
            // 字节是陈旧值，行可加锁。
            if holder_locked_this_row(chain, entry.txn_id, block, row_no)? {
                Ok(LockOutcome::WouldWait {
                    holder: entry.txn_id,
                })
            } else {
                Ok(LockOutcome::Acquired { reentrant: false })
            }
        }
        _ => Ok(LockOutcome::Acquired { reentrant: false }),
    }
}

/// **陈旧字节消歧**（见 [`decide_row_lock`] 的说明）：`holder` 的撤销链里有没有
/// **针对本行**的记录。`ItlOverwrite` 是块级记录（其行号是占位 1），不参与判定。
///
/// 代价只落在**争用路径**上（行字节指向他人活动条目时）；步数按 `rec_count + 1`
/// 设预算，受损链不得死循环（同 `rollback_chain` 的防线）。
fn holder_locked_this_row(
    chain: &UndoChain<'_, '_>,
    holder: bicdb_storage::undo::TxnId,
    block: BufferKey,
    row_no: u16,
) -> Result<bool, TxnError> {
    let (file_id, block_id) = (block.rdba.file_id(), block.rdba.block_id());
    let Some(slot) = chain.lookup(holder)? else {
        return Ok(false); // 槽已回收 ⇒ 更不可能持锁
    };
    let mut at = slot.undo_current;
    let mut budget = u64::from(slot.rec_count) + 1;
    while let Some(pos) = at {
        if budget == 0 {
            return Err(TxnError::Chain(UndoChainError::Undo(
                bicdb_storage::undo::UndoError::MalformedRecord,
            )));
        }
        budget -= 1;
        let record = chain.read(pos)?;
        at = record.prev;
        if record.op == UndoOp::ItlOverwrite {
            continue;
        }
        if record.rowid.file_id() == file_id
            && record.rowid.block_id() == block_id
            && record.rowid.row_id() == row_no
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// **加锁并占用 ITL 条目**（写路径的前置：§5.4.2 ① 的完整落点）。
///
/// 返回可用的 ITL 槽号（重入时即该行既有的槽）；`WouldWait` ⇒
/// [`TxnError::RowLocked`]——调用方登记等待后重试，**不得静默失败**。
#[allow(clippy::too_many_arguments)]
fn lock_and_occupy(
    pool: &BufferPool<'_>,
    log: &mut GroupWriter<'_, '_>,
    chain: &mut UndoChain<'_, '_>,
    txn: &Txn,
    local: &mut Page,
    block: BufferKey,
    row_no: u16,
) -> Result<u16, TxnError> {
    match decide_row_lock(local, chain, txn, block, row_no)? {
        LockOutcome::WouldWait { holder } => Err(TxnError::RowLocked {
            holder,
            row: RowId::from_parts(block.rdba.file_id(), block.rdba.block_id(), row_no)?,
        }),
        LockOutcome::Acquired { reentrant: true } => {
            // 同一行的重入：槽已就位，不重复计 `lock_cnt`。
            let row = heap::row(local, row_no).ok_or(TxnError::Heap(HeapError::NoSuchRow))?;
            Ok(u16::from(row[1]))
        }
        LockOutcome::Acquired { reentrant: false } => {
            let (slot, fresh) = occupy_itl(pool, log, chain, txn, local, block)?;
            if fresh {
                // 新占用：条目已按"第一把行锁"计 1（见 `occupy_itl`）。
            } else {
                // 本事务在该块已有条目（锁的是**另一行**）：行数 +1。
                itl::lock(local, slot)?;
            }
            Ok(slot)
        }
    }
}

/// **提交**（§11.1.1 的次序）：提交记录入流 → **等它身持久化**（**提交点**）
/// → 事务表槽置已提交（经池、带 redo）→ 返回。
///
/// **提交记录一旦耐久，事务即已提交**：其后的槽标记是"尽快可见/可回收"的
/// 优化状态——这一步失败**不返回 Err**（调用方若据此回滚，会把日志里已提交
/// 的事务回滚掉 = 已提交数据丢失）；恢复的分析阶段会用**前滚补标记**把槽补成
/// `Committed` + 准确序号（§4.6.6 ⑤），语义不丢。
pub fn commit(
    pool: &BufferPool<'_>,
    log: &mut GroupWriter<'_, '_>,
    chain: &mut UndoChain<'_, '_>,
    txn: &mut Txn,
    commit_seq: CommitSeq,
) -> Result<(), TxnError> {
    let lsn = log.append(|l| RedoRecord::commit(l, txn.raw(), commit_seq.as_raw()))?;
    log.flush(lsn)?; // **提交点**：此后事务已提交（不可再回滚）

    // 槽标记：尽力而为；失败由恢复的前滚补标记兜底（见函数文档）。
    let marked = mark_slot_committed(pool, log, chain, txn, commit_seq);
    txn.state = TxnState::Committed;
    let _ = marked; // 不因槽标记失败把已提交事务变成"可回滚的失败"
    Ok(())
}

/// 事务表槽置"已提交 + 准确提交序号"（经池、带 redo）。
fn mark_slot_committed(
    pool: &BufferPool<'_>,
    log: &mut GroupWriter<'_, '_>,
    chain: &mut UndoChain<'_, '_>,
    txn: &Txn,
    commit_seq: CommitSeq,
) -> Result<(), TxnError> {
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
    )
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

/// **语句回滚点**（§4.6.6 ②）：语句开始时记录的 `undo_current` 快照——
/// **纯内存**（真值在事务表槽里；回滚点只是它的一份拷贝，"建立回滚点"本身
/// 不需要任何持久化动作）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatementMark {
    at: Option<RowId>,
}

impl StatementMark {
    /// 回滚点处的撤销位置（诊断/测试）。
    #[must_use]
    pub fn at(&self) -> Option<RowId> {
        self.at
    }
}

/// **取语句回滚点**（读事务表槽的 `undo_current`）。
pub fn statement_mark(chain: &mut UndoChain<'_, '_>, txn: &Txn) -> Result<StatementMark, TxnError> {
    let header = chain.segment().read_page(0)?;
    let slot = read_slot(&header, txn.slot)?;
    Ok(StatementMark {
        at: slot.undo_current,
    })
}

/// **语句级回滚**（§4.6.6 ②）：沿链从新到旧补偿**到回滚点**（不含回滚点处的
/// 记录）——**不释放锁**（REQ-TXN-003）、**不回滚此前成功语句**；每条逆操作
/// 与事务回滚同路径（经池 + redo，§11.1.2：回滚写回也必须可重放）。
///
/// 走完后把槽的 `undo_current` **置回回滚点**（一次槽头更新，经池 + redo）：
/// 被撤销的这段记录从此**不可达**——链上只剩"最终状态里仍然成立的改动"，
/// 于是后到的事务回滚/恢复重放/CR 重建都不必再对孤链做补偿（那些补偿虽然
/// 幂等，但"槽已复用"判据在跨事务场景下会拒绝）。崩溃落在两次写之间也安全：
/// 恢复从旧链头重走整链，补偿幂等（§4.6.6 ③）。
pub fn rollback_to_mark(
    pool: &BufferPool<'_>,
    log: &mut GroupWriter<'_, '_>,
    chain: &mut UndoChain<'_, '_>,
    txn: &mut Txn,
    mark: StatementMark,
) -> Result<u64, TxnError> {
    let mut at = {
        let header = chain.segment().read_page(0)?;
        read_slot(&header, txn.slot)?.undo_current
    };
    let mut count = 0u64;
    while let Some(pos) = at {
        if Some(pos) == mark.at {
            break;
        }
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
    // 槽头置回回滚点（幂等：没变就不写）。
    let header_before = chain.segment().read_page(0)?;
    let mut header_after = Page::from_bytes(Box::new(*header_before.as_bytes()));
    let mut slot = read_slot(&header_after, txn.slot)?;
    if slot.undo_current != mark.at {
        slot.undo_current = mark.at;
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
    }
    Ok(count)
}

/// **语句执行的等待-重试驱动**（§5.4.2 ①/④ 的会话层落点）。
///
/// 进入时取**语句回滚点**；把 `op` 反复执行：
///
/// ```text
/// op 成功            ⇒ 返回结果
/// op 报 `RowLocked`  ⇒ 登记等待（按持锁者）→ 挂起 → 醒来**从头重试** op
///                       （不假设行还在原地/槽没换人/行还存在，§5.4.2）
/// 死锁环上我是牺牲者 ⇒ **语句级回滚到回滚点**（不释锁、不动此前语句）
///                       + 取消等待（牺牲者不再等待 ⇒ 环即解开）→ 返回
///                       `DeadlockVictim`（会话层重启语句）
/// 其他错误           ⇒ 原样上抛（语句边界由调用方处置：先 `rollback_to_mark`）
/// ```
///
/// **等待只在 latch 之外发生**（§5.4.2：latch 持有以页访问为界，持有 latch
/// 时不得等待业务锁）——`op` 每次调用自行把 latch 取放完；本函数在两次调用
/// 之间挂起。
///
/// `now` 是单调毫秒时钟（死锁阈值与等待时长都用它；测试可注入）。
#[allow(clippy::too_many_arguments)]
pub fn execute_with_wait<T>(
    pool: &BufferPool<'_>,
    log: &mut GroupWriter<'_, '_>,
    chain: &mut UndoChain<'_, '_>,
    txn: &mut Txn,
    gate: &crate::lock::WaitGate,
    policy: &WaitPolicy,
    now: impl Fn() -> u64,
    mut op: impl FnMut(
        &BufferPool<'_>,
        &mut GroupWriter<'_, '_>,
        &mut UndoChain<'_, '_>,
        &mut Txn,
    ) -> Result<T, TxnError>,
) -> Result<T, TxnError> {
    let mark = statement_mark(chain, txn)?;
    let mut waits: u32 = 0;
    loop {
        match op(pool, log, chain, txn) {
            Ok(v) => return Ok(v),
            Err(TxnError::RowLocked { holder, row }) => {
                gate.register(txn.txn_id, holder, row, now());
                let deadlock = gate.with_registry(|r| {
                    crate::lock::detect_deadlock(r, chain, now(), policy.deadlock_threshold_ms)
                })?;
                if let Some(dl) = deadlock {
                    if dl.victim == txn.txn_id {
                        let _ = gate.cancel(txn.txn_id);
                        rollback_to_mark(pool, log, chain, txn, mark)?;
                        return Err(TxnError::DeadlockVictim { cycle: dl.cycle });
                    }
                    // 我不是牺牲者：继续等待（牺牲者回滚后会释放/不再等待）。
                }
                waits = waits.saturating_add(1);
                if let Some(max) = policy.max_waits {
                    if waits > max {
                        let _ = gate.cancel(txn.txn_id);
                        return Err(TxnError::LockTimeout { holder, row });
                    }
                }
                gate.park(txn.txn_id, policy.park_timeout);
            }
            Err(other) => return Err(other),
        }
    }
}

/// 等待-重试驱动的策略（§5.4.2；会话级参数）。
#[derive(Debug, Clone, Copy)]
pub struct WaitPolicy {
    /// 单次挂起的时长（醒来或超时后重试 / 再做死锁检测）。
    pub park_timeout: std::time::Duration,
    /// 死锁检测阈值（等待超过它才建图找环；默认口径 [`crate::lock::DEADLOCK_THRESHOLD_MS`]）。
    pub deadlock_threshold_ms: u64,
    /// 等待次数上限（`None` = 等到底——死锁检测与持锁者结束兜底）。
    pub max_waits: Option<u32>,
}

impl Default for WaitPolicy {
    fn default() -> Self {
        Self {
            park_timeout: std::time::Duration::from_millis(50),
            deadlock_threshold_ms: crate::lock::DEADLOCK_THRESHOLD_MS,
            max_waits: None,
        }
    }
}

/// **回收撤销空间**（§4.6.5）：把"已提交 且 `commit_seq < 最老快照`"的事务
/// 出链、推进回收水位；**槽表全空时段回卷**（`append_pos` 复位，空间交还写者）。
///
/// - `oldest_snapshot`：最老活跃快照的提交序号
///   （[`crate::snapshot::SnapshotRegistry::oldest`]）；`None` = 无活跃快照
///   ⇒ 全部已提交事务可回收（如检查点时的静默窗口）。
///
/// 这是**系统操作**：redo 记录的 `txn_id` = **0**（合法身份，非空槽哨兵——
/// 见待讨论清单第 12 条）；undo 段头页经池写入并**立即落盘**（与其他 undo
/// 页更新同规：链直读的一致性）。
///
/// **幂等**：无可回收对象时不产生任何 I/O 与 redo（字节相同即返回）。
pub fn reclaim(
    pool: &BufferPool<'_>,
    log: &mut GroupWriter<'_, '_>,
    chain: &mut UndoChain<'_, '_>,
    oldest_snapshot: Option<CommitSeq>,
) -> Result<ReclaimReport, TxnError> {
    let header_before = chain.segment().read_page(0)?;
    let mut header_after = Page::from_bytes(Box::new(*header_before.as_bytes()));
    let report = reclaim_undo(&mut header_after, oldest_snapshot, true)?;
    if header_after.as_bytes() == header_before.as_bytes() {
        return Ok(report); // 幂等重入：无变化
    }
    let key = undo_page_key(chain, 0)?;
    write_undo_page_change(
        pool,
        log,
        0,
        key,
        header_before.as_bytes(),
        header_after.as_bytes(),
        false,
    )?;
    if report.rewound {
        chain.note_rewind();
    }
    Ok(report)
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

/// 在数据页上占用一个 **ITL 槽**（复用优先；否则扩展）。
/// 前像：原为空闲（`Free`）⇒ `None`（"原为空闲"的编码）。
fn acquire_itl_slot(page: &mut Page, _txn_id: TxnId) -> Result<ItlAcquire, TxnError> {
    if let Some(slot) = itl::find_reusable(page)? {
        let old = itl::snapshot(page, slot)?;
        let entry = itl::read_itl(page, slot)?;
        // 前像：原为空闲（flags = Free）⇒ None（"原为空闲"的编码）。
        let old = if entry.state == ItlState::Free {
            None
        } else {
            Some(old)
        };
        return Ok(ItlAcquire::Fresh { slot, old });
    }
    // 没有可复用槽 → 扩展（itl_max 由调用方经段/表选项控制；此处用上限 32）。
    let slot = itl::grow(page, itl::ITL_MAX_LIMIT)?;
    Ok(ItlAcquire::Fresh { slot, old: None })
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
    // 撤销段容量：下一个追加页未映射 ⇒ **经池扩展**（redo 保护，镜像先写后读）。
    ensure_undo_capacity(pool, log, chain, txn)?;
    let plan = chain.plan_append(txn.slot, op, 0, rowid, payload)?;
    // ⓪-1 **跨段内位图页的推进**（`append_pos` 落在窗口首位）：全新位图页
    //      先格式化落盘（fsync）——与"新撤销页"同规；扩展出来的既有页
    //      （文件位图/段头）**经池写 redo**（系统操作的 redo 保护）。
    if let Some(adv) = &plan.advance {
        for (rdba, page) in &adv.fresh {
            let mut p = Page::from_bytes(Box::new(*page.as_bytes()));
            chain
                .segment()
                .write_physical_page(rdba.block_id(), &mut p)?;
        }
        chain.segment().sync()?;
        for (rdba, before, after) in &adv.images {
            let key = BufferKey::new(workspace_of(chain), *rdba);
            write_page_change(
                pool,
                log,
                txn.raw(),
                key,
                before.as_bytes(),
                after.as_bytes(),
                false,
            )?;
        }
    }
    // ⓪ **新撤销页：先格式化落盘（fsync）、再让它进 redo**（§11.5.4 实现注记）。
    //    物理增量重放**无法重建一个不存在的页**：若只有 redo 耐久而页从未
    //    落盘，掉电后重放会以"页不存在/校验失败"中止恢复。次序反过来则安全：
    //    页先于 redo 耐久——崩溃只可能留下"无人引用的已格式化页"（append_pos
    //    未推进，下次原样重写）。
    if plan.opened {
        // **新撤销页：先格式化落盘（fsync）、再让它进 redo**（见上），且
        // **`page_lsn` 前置到当前追加位**——该 rdba 可能已有**上一轮生命周期**
        // 的 redo 记录（段回卷 / 重置复用的页、以及文件层复用的块），重放按
        // `page_lsn` 跳过更早的记录；不前置（= 0）会把旧记录字节"复活"到新
        // 内容上（输家回滚读到损坏链）。`before` 与盘上镜像同为这一份字节，
        // 与后续 diff 的基准保持一致（实测：缺此规则时回收页恢复用例失败）。
        let mut formatted = Page::from_bytes(Box::new(*plan.undo_page.0.as_bytes()));
        let lsn_now = log.appended_lsn();
        let mut header = formatted.header().ok_or(TxnError::StaleCache)?;
        header.page_lsn = lsn_now;
        formatted.write_header(&header);
        chain.segment().write_page(plan.logical, &mut formatted)?;
        chain.segment().sync()?;
    }
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
    if let Some((logical, before, after)) = &plan.bitmap {
        let key = undo_page_key(chain, *logical)?;
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
    use bicdb_storage::controlfile::{
        ArchiveMode, ArchiveRecord, CheckpointProgress, ControlFile, RedoEntries, WorkspaceEntry,
    };
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
        assemble_row(0, 1, &[false], &[], &[payload]).unwrap()
    }

    /// 期望的"落盘行"：行头 `itl_slot` 由写路径回填为**实际占用的 ITL 槽号**
    /// （调用方给的字节会被改写——见 `insert_row`）。
    fn stored_row(row: &[u8], itl_slot: u8) -> Vec<u8> {
        let mut r = row.to_vec();
        r[1] = itl_slot;
        r
    }

    /// 假 WAL：水位视为已全落盘（池不催刷；提交路径单独 flush）。
    struct FakeWal;
    impl WalGuard for FakeWal {
        fn durable_lsn(&self) -> Lsn {
            lsn(u64::MAX >> 16)
        }
        fn ensure_durable(&self, _t: Lsn) -> std::io::Result<()> {
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
        assert_eq!(
            heap::row(&page, 1),
            Some(&stored_row(&row, 0)[..]),
            "行已落盘（itl_slot 回填为实际占用的槽 0）"
        );
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

    /// 建一套「undo 段 + 数据页 + 池 + 控制文件」的测试装置（回收用例共用）。
    /// 日志由用例自己 `GroupWriter::create(&io, &mut cf, ..)`（借用控制文件）。
    fn harness(
        io: &MemFileIo,
        archive: ArchiveMode,
    ) -> (
        DataFile<'_>,
        bicdb_workspace::io::FileHandle,
        BufferPool<'_>,
        ControlFile<'_>,
    ) {
        let undo_file = DataFile::create(io, Path::new(UNDO_F), 1, 1, WS, 512).unwrap();
        let undo_handle = undo_file.handle();
        let data_handle = {
            let data_file = DataFile::create(io, Path::new(DATA_F), 3, 3, WS, 512).unwrap();
            let h = data_file.handle();
            let mut page = Page::new(PageType::HeapTable, WS, 3, 1);
            pagefile::write_page(io, h, 1, &mut page).unwrap();
            h
        };
        let pool = BufferPool::new(
            io,
            8,
            move |_ws, r| match r.file_id() {
                1 => Some((undo_handle, r.block_id())),
                3 => Some((data_handle, r.block_id())),
                _ => None,
            },
            FakeWal,
        )
        .unwrap();
        let cf = ControlFile::format(
            io,
            Path::new(A),
            Path::new(B),
            &ws_entry(),
            &RedoEntries::new(2, 1).unwrap(),
            &ArchiveRecord::new(archive),
        )
        .unwrap();
        (undo_file, data_handle, pool, cf)
    }

    #[test]
    fn reclaim_frees_committed_slots_below_the_oldest_snapshot() {
        // #34：判据 = 已提交 且 commit_seq < 最老快照 ——
        // 低于水位的槽释放、可再分配；高处的保留；水位单调、幂等。
        let io = mem();
        let (mut undo_file, _data_handle, pool, mut cf) = harness(&io, ArchiveMode::ArchiveLog);
        let mut chain = UndoChain::open(create_undo_segment(&mut undo_file, 2, 3, 4).unwrap());
        let mut log = GroupWriter::create(&io, &mut cf, Path::new(WAL), spec(), lsn(0)).unwrap();
        let mut t1 = begin(&pool, &mut log, &mut chain, seq(0)).unwrap();
        commit(&pool, &mut log, &mut chain, &mut t1, seq(10)).unwrap();
        let mut t2 = begin(&pool, &mut log, &mut chain, seq(0)).unwrap();
        commit(&pool, &mut log, &mut chain, &mut t2, seq(20)).unwrap();

        // 最老快照 15：序号 10 可回收，20 必须保留。
        let report = reclaim(&pool, &mut log, &mut chain, Some(seq(15))).unwrap();
        assert_eq!(report.slots_freed, 1);
        assert_eq!(report.committed_kept, 1);
        assert_eq!(report.reclaim_seq, seq(10));
        let header = chain.segment().read_page(0).unwrap();
        assert_eq!(read_slot(&header, t1.slot).unwrap().state, TxnState::Free);
        assert_eq!(
            read_slot(&header, t2.slot).unwrap().state,
            TxnState::Committed
        );
        // 幂等：同一水位再跑不释放更多。
        let again = reclaim(&pool, &mut log, &mut chain, Some(seq(15))).unwrap();
        assert_eq!(again.slots_freed, 0);
        assert_eq!(again.reclaim_seq, seq(10));

        // 快照前进到 21：第二个也回收；槽可再分配（wrap 换代）。
        let report = reclaim(&pool, &mut log, &mut chain, Some(seq(21))).unwrap();
        assert_eq!(report.slots_freed, 1);
        assert_eq!(report.reclaim_seq, seq(20));
        let mut t3 = begin(&pool, &mut log, &mut chain, seq(21)).unwrap();
        assert!(t3.slot == t1.slot || t3.slot == t2.slot, "从空闲链取回");
        let header = chain.segment().read_page(0).unwrap(); // 重读（上面的副本已旧）
        assert_eq!(read_slot(&header, t3.slot).unwrap().wrap, 1, "wrap 换代");
        commit(&pool, &mut log, &mut chain, &mut t3, seq(21)).unwrap();
    }

    #[test]
    fn steady_state_reclaim_keeps_the_txn_table_from_exhausting() {
        // 没有回收时，第 257 个事务 begin 必然 `NoFreeSlot`（256 槽）；
        // 稳态回收（每轮把低于当前序号的全部回收）让事务表**永续**。
        let io = mem();
        // 非归档模式：本用例只关心回收与组复用，归档由 group 的专门用例覆盖。
        let (mut undo_file, _data_handle, pool, mut cf) = harness(&io, ArchiveMode::NoArchive);
        let mut chain = UndoChain::open(create_undo_segment(&mut undo_file, 2, 3, 4).unwrap());
        let mut log = GroupWriter::create(&io, &mut cf, Path::new(WAL), spec(), lsn(0)).unwrap();
        let key = BufferKey::new(WS, rdba(3, 1));
        let mut last = seq(0);
        for k in 1..=280u64 {
            let mut txn = begin(&pool, &mut log, &mut chain, last).unwrap();
            // 每次插入（undo 记录 → 打开 undo 页），回收才有页可交还。
            let row = row_bytes(format!("row-{k}").as_bytes());
            insert_row(
                &pool,
                &mut log,
                &mut chain,
                &mut txn,
                key,
                &row,
                &InsertPolicy::in_place(0),
            )
            .unwrap();
            let commit_at = seq(k);
            commit(&pool, &mut log, &mut chain, &mut txn, commit_at).unwrap();
            // 无活跃快照 ⇒ 水位 = 当前提交序号（§12.7 的空集语义）。
            let report = reclaim(&pool, &mut log, &mut chain, Some(commit_at)).unwrap();
            assert!(!report.rewound, "还有最新已提交事务 ⇒ 不整段回卷");
            last = commit_at;
            // CKPT 角色（每轮发布，否则两组用尽后写者被挡——§11.9 的常态闭环：
            // 写入量一小、组切换就要求上一组已降级）。
            log.publish_checkpoint(&CheckpointProgress {
                checkpoint_commit_seq: commit_at,
                checkpoint_lsn: log.appended_lsn(),
                current_commit_seq: commit_at,
                oldest_snapshot_commit_seq: commit_at,
                timestamp: 0,
            })
            .unwrap();
        }
        // **空间回收的判据**：280 个事务后追加位只到 4——每事务开新页的
        // 膨胀被"归属可回收即重置复用"消化（稳态只占 2 张数据页）。
        assert!(
            append_pos(&chain) <= 4,
            "undo 页数有界（append_pos = {}）",
            append_pos(&chain)
        );
        // 进入静默窗口（无活跃快照）⇒ 最新事务也可回收 ⇒ 槽表全空 ⇒ **段回卷**，
        // 空间回到段首（幂等；再跑一次不重复动作）。
        let r = reclaim(&pool, &mut log, &mut chain, None).unwrap();
        assert!(r.rewound, "槽表全空 ⇒ 段回卷");
        assert_eq!(
            append_pos(&chain),
            bicdb_storage::undo::UNDO_FIRST_DATA_PAGE,
            "append_pos 复位到段首"
        );
        assert!(!reclaim(&pool, &mut log, &mut chain, None).unwrap().rewound);
        // 回卷后仍能开新事务并写行（幂等、不破坏段头/undo 页）。
        let mut t = begin(&pool, &mut log, &mut chain, seq(281)).unwrap();
        let row = row_bytes(b"after-rewind");
        insert_row(
            &pool,
            &mut log,
            &mut chain,
            &mut t,
            key,
            &row,
            &InsertPolicy::in_place(0),
        )
        .unwrap();
        commit(&pool, &mut log, &mut chain, &mut t, seq(281)).unwrap();
        // 落盘后行可读（回收/回卷不破坏已有数据路径）。
        pool.flush_workspace(WS).unwrap();
    }

    /// 读 undo 段的当前追加位置（测试辅助）。
    fn append_pos(chain: &UndoChain<'_, '_>) -> u32 {
        chain.segment().append_position().unwrap()
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
            Some(&stored_row(&win_row, 0)[..]),
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
    fn recycled_undo_page_survives_crash_recovery() {
        // 页复用的重放守卫（`page_lsn` 前置）回归：被**重置复用**的撤销页
        // 在其 rdba 上还有上一轮生命周期的 redo 记录——重放必须按 `page_lsn`
        // 跳过它们，否则旧字节会被"复活"到新内容里，输家回滚会读到损坏链。
        let io = mem();
        let (mut undo_file, _data_handle, pool, mut cf) = harness(&io, ArchiveMode::NoArchive);
        let mut chain = UndoChain::open(create_undo_segment(&mut undo_file, 2, 3, 4).unwrap());
        let mut log = GroupWriter::create(&io, &mut cf, Path::new(WAL), spec(), lsn(0)).unwrap();
        let key = BufferKey::new(WS, rdba(3, 1));

        // 第一轮：写入并提交，随后**静默回收**（槽释放 + 水位前移 + 段回卷）——
        // 这张撤销页从此"归属已回收"，可被下一个事务重置复用。
        let r1 = row_bytes(b"first-cycle");
        let mut t1 = begin(&pool, &mut log, &mut chain, seq(0)).unwrap();
        let rid1 = insert_row(
            &pool,
            &mut log,
            &mut chain,
            &mut t1,
            key,
            &r1,
            &InsertPolicy::in_place(0),
        )
        .unwrap();
        commit(&pool, &mut log, &mut chain, &mut t1, seq(1)).unwrap();
        let r = reclaim(&pool, &mut log, &mut chain, None).unwrap();
        assert!(r.rewound, "静默窗口：槽表全空 ⇒ 回卷");
        assert_eq!(
            append_pos(&chain),
            bicdb_storage::undo::UNDO_FIRST_DATA_PAGE
        );

        // 第二轮：**复用同一逻辑页**写入一个不提交的输家事务。
        let r2 = row_bytes(b"second-cycle");
        let mut t2 = begin(&pool, &mut log, &mut chain, seq(1)).unwrap();
        let rid2 = insert_row(
            &pool,
            &mut log,
            &mut chain,
            &mut t2,
            key,
            &r2,
            &InsertPolicy::in_place(0),
        )
        .unwrap();
        // 崩溃：日志耐久；**撤销页只在"先格式化落盘"那一步到过盘**（旧内容
        // 的字节仍在同一块上——正是守卫要处理的局面）。
        log.flush(log.appended_lsn()).unwrap();
        drop(pool);

        // 恢复：分析/重做/撤销。输家 t2 的回滚要**读它自己的撤销链**——
        // 读到损坏的记录即失败，读到上一轮的旧字节则行恢复错值。
        let cf_ro = ControlFile::open(&io, Path::new(A), Path::new(B)).unwrap();
        let groups = online_groups(&io, &cf_ro, Path::new(WAL), spec()).unwrap();
        let data_handle = {
            // 重新打开数据文件（与上面的池同一路径/句柄语义）。
            let h = bicdb_storage::datafile::DataFile::open(&io, Path::new(DATA_F))
                .unwrap()
                .handle();
            h
        };
        let undo_handle = bicdb_storage::datafile::DataFile::open(&io, Path::new(UNDO_F))
            .unwrap()
            .handle();
        let mut resolve = |rd: Rdba| match rd.file_id() {
            1 => Some((undo_handle, rd.block_id())),
            3 => Some((data_handle, rd.block_id())),
            _ => None,
        };
        let report = recover(&io, &groups, lsn(0), &chain, &mut log, &mut resolve).unwrap();
        assert_eq!(report.undo.txns_rolled_back, 1, "输家被回滚");

        // 胜者的行在、输家的行被撤销——且撤销链读的是**本轮**的记录。
        let page = pagefile::read_page_verified(&io, data_handle, 1).unwrap();
        assert_eq!(
            heap::row(&page, rid1.row_id()),
            Some(&stored_row(&r1, 0)[..]),
            "第一轮的行保留"
        );
        assert_eq!(heap::row(&page, rid2.row_id()), None, "输家的行被撤销");
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
            &InsertPolicy::in_place(0),
            &mut no_alloc,
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
            &InsertPolicy::in_place(0),
            &mut no_alloc,
        )
        .unwrap();
        // **改长**更新：走**行迁移**（同页放得下——"bbb"→"ccccc" 只差 2 字节）。
        let row_long = row_bytes(b"ccccc");
        let outcome = update_row(
            &pool,
            &mut log,
            &mut chain,
            &mut t2,
            key,
            rid2.row_id(),
            &row_long,
            &InsertPolicy::in_place(0),
            &mut no_alloc,
        )
        .unwrap();
        assert!(outcome.migrated, "改长 ⇒ 迁移");
        assert_eq!(outcome.rowid, rid2, "ROWID 稳定入口不变");
        // 原槽位成了转发指针，指向新位置。
        let src = page_snapshot(&pool, key);
        assert_eq!(heap::row(&src, rid2.row_id()), None, "原槽位不再直接是行");
        let target = heap::forwarding_target(&src, rid2.row_id()).expect("转发指针");
        // 新行的 `itl_slot` 由写路径回填为**目的页上本事务的 ITL 槽**（与 insert 同规）。
        let owner_slot = (0..bicdb_storage::itl::itl_count(&src).unwrap())
            .find(|&i| itl_of(&src, i).txn_id == t2.txn_id)
            .expect("t2 的 ITL 条目");
        assert_eq!(
            heap::row(&src, target.row_id()),
            Some(&stored_row(&row_long, owner_slot as u8)[..]),
            "新位置的完整行（itl_slot 已回填）"
        );
        commit(&pool, &mut log, &mut chain, &mut t2, seq(2)).unwrap();
        pool.flush_workspace(WS).unwrap();
        let page = pagefile::read_page_verified(&io, data_handle, 1).unwrap();
        // 稳定入口是**转发指针**：读者沿它取到新位置的完整行（§6.2/§12.4）。
        let target = heap::forwarding_target(&page, rid2.row_id()).expect("盘上转发指针");
        let stored = heap::row(&page, target.row_id()).expect("更新后的行（经转发指针）");
        // 行头 itl_slot 指向 t2 的槽（写路径回填）；其余字节 = 改长后的新行。
        assert_eq!(
            usize::from(stored[1]),
            owner_slot.into(),
            "itl_slot 归本事务"
        );
        assert_eq!(&stored[2..], &row_long[2..], "行体 = 改长后的内容");
    }

    #[test]
    fn growing_update_migrates_across_pages_and_rolls_back() {
        // §6.2：增长且原页放不下 ⇒ 迁移（新位置 + 原槽位转发指针）；
        // §12.4：迁移 = "删除旧位置 + 插入新位置"——回滚先删新行、
        // 再把转发指针还原成原行（undo 的"删除"补偿接受 Forwarding 槽）。
        let io = mem();
        let mut undo_file = DataFile::create(&io, Path::new(UNDO_F), 1, 1, WS, 512).unwrap();
        let undo_handle = undo_file.handle();
        let segment = create_undo_segment(&mut undo_file, 2, 3, 4).unwrap();
        let mut chain = UndoChain::open(segment);
        // 两页数据文件：第 2 页作为迁移目标。
        let data_handle = {
            let data_file = DataFile::create(&io, Path::new(DATA_F), 3, 3, WS, 512).unwrap();
            let h = data_file.handle();
            for block in 1..=2u32 {
                let mut page = Page::new(PageType::HeapTable, WS, 3, block);
                pagefile::write_page(&io, h, block, &mut page).unwrap();
            }
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
        let policy = InsertPolicy::in_place(0);
        let key1 = BufferKey::new(WS, rdba(3, 1));
        let key2 = BufferKey::new(WS, rdba(3, 2));

        // 先填满第 1 页（10 × 1500B ≈ 15KB），只留下不足 4KB 空位——
        // 让"同页迁移"不可能，逼出跨页路径（分配口必须被调用）。
        // **目标行由前一个已提交事务放入**：这样"回滚本次更新"才应恢复它
        // （若目标行也是本事务插入的，回滚整条链的净效果是"无此行"——那是
        // 另一个用例覆盖的语义）。
        let old_row = row_bytes(b"aaaa");
        let mut filler = begin(&pool, &mut log, &mut chain, seq(1)).unwrap();
        for k in 0..10u8 {
            let f = row_bytes(&vec![k; 1500]);
            insert_row(&pool, &mut log, &mut chain, &mut filler, key1, &f, &policy).unwrap();
        }
        let rid = insert_row(
            &pool,
            &mut log,
            &mut chain,
            &mut filler,
            key1,
            &old_row,
            &policy,
        )
        .unwrap();
        commit(&pool, &mut log, &mut chain, &mut filler, seq(1)).unwrap();

        let mut t = begin(&pool, &mut log, &mut chain, seq(2)).unwrap();

        // 改长到 4KB：第 1 页放不下 ⇒ 分配口给第 2 页。
        let big = row_bytes(&vec![7u8; 4000]);
        let mut alloc = move |need: usize| -> Result<BufferKey, TxnError> {
            assert!(need <= 4200, "分配口只被要求放得下的页");
            Ok(key2)
        };
        let out = update_row(
            &pool,
            &mut log,
            &mut chain,
            &mut t,
            key1,
            rid.row_id(),
            &big,
            &policy,
            &mut alloc,
        )
        .unwrap();
        assert!(out.migrated);
        assert_eq!(out.rowid, rid, "稳定入口（ROWID）不变");
        let src = page_snapshot(&pool, key1);
        let dst = page_snapshot(&pool, key2);
        assert_eq!(heap::row(&src, rid.row_id()), None, "原槽位 = 转发指针");
        let target = heap::forwarding_target(&src, rid.row_id()).expect("转发指针");
        assert_eq!(target.block_id(), 2, "指向第 2 页");
        assert!(heap::row(&dst, target.row_id()).is_some(), "新位置有完整行");

        // **回滚**：新行删除、原行原位还原、槽位状态回 Normal。
        rollback(&pool, &mut log, &mut chain, &mut t).unwrap();
        let src = page_snapshot(&pool, key1);
        let dst = page_snapshot(&pool, key2);
        assert_eq!(
            heap::row(&src, rid.row_id()),
            Some(&stored_row(&old_row, 0)[..]),
            "原行原位还原（字节 = 落盘形态）"
        );
        assert_eq!(
            heap::forwarding_target(&src, rid.row_id()),
            None,
            "指针已还原"
        );
        assert_eq!(heap::row(&dst, target.row_id()), None, "新行已撤销");
    }

    #[test]
    fn cr_sees_the_pre_migration_row_through_the_chain() {
        // CR（§12.4）：旧快照看原页 —— 迁移被撤销（转发指针还原成原行）、
        // 新页上的插入被撤销；新快照看原页 —— 仍是指针。
        let io = mem();
        let (mut undo_file, data_handle, pool, mut cf) = harness(&io, ArchiveMode::NoArchive);
        // 第 2 页格式化为数据页（迁移目标）。
        {
            let mut page = Page::new(PageType::HeapTable, WS, 3, 2);
            pagefile::write_page(&io, data_handle, 2, &mut page).unwrap();
        }
        let mut chain = UndoChain::open(create_undo_segment(&mut undo_file, 2, 3, 4).unwrap());
        let mut log = GroupWriter::create(&io, &mut cf, Path::new(WAL), spec(), lsn(0)).unwrap();
        let policy = InsertPolicy::in_place(0);
        let key1 = BufferKey::new(WS, rdba(3, 1));
        let key2 = BufferKey::new(WS, rdba(3, 2));

        // 填满第 1 页，逼出跨页迁移（同"回滚"用例的构造）。
        let old_row = row_bytes(b"orig");
        let mut t = begin(&pool, &mut log, &mut chain, seq(5)).unwrap();
        for k in 0..10u8 {
            let f = row_bytes(&vec![k; 1500]);
            insert_row(&pool, &mut log, &mut chain, &mut t, key1, &f, &policy).unwrap();
        }
        let rid = insert_row(&pool, &mut log, &mut chain, &mut t, key1, &old_row, &policy).unwrap();
        commit(&pool, &mut log, &mut chain, &mut t, seq(5)).unwrap();

        let mut t2 = begin(&pool, &mut log, &mut chain, seq(6)).unwrap();
        let big = row_bytes(&vec![9u8; 4000]);
        let mut alloc = move |_need: usize| -> Result<BufferKey, TxnError> { Ok(key2) };
        update_row(
            &pool,
            &mut log,
            &mut chain,
            &mut t2,
            key1,
            rid.row_id(),
            &big,
            &policy,
            &mut alloc,
        )
        .unwrap();
        commit(&pool, &mut log, &mut chain, &mut t2, seq(6)).unwrap();

        let src = page_snapshot(&pool, key1);
        let dst = page_snapshot(&pool, key2);
        // 快照 5（迁移之前）：原页 = 原行；新页 = 无此行。
        let cr_old = bicdb_storage::cr::reconstruct(&src, seq(5), &chain).unwrap();
        assert_eq!(
            heap::row(&cr_old, rid.row_id()),
            Some(&stored_row(&old_row, 0)[..]),
            "旧快照见原行（迁移被撤销；字节 = 落盘形态）"
        );
        assert_eq!(heap::forwarding_target(&cr_old, rid.row_id()), None);
        let dst_old = bicdb_storage::cr::reconstruct(&dst, seq(5), &chain).unwrap();
        let target = heap::forwarding_target(&src, rid.row_id()).expect("物理指针");
        assert_eq!(heap::row(&dst_old, target.row_id()), None, "旧快照不见新行");
        // 快照 6（迁移可见）：原页仍是指针。
        let cr_new = bicdb_storage::cr::reconstruct(&src, seq(6), &chain).unwrap();
        assert_eq!(heap::forwarding_target(&cr_new, rid.row_id()), Some(target));
    }

    #[test]
    fn crash_during_migration_rolls_back_to_the_source_row() {
        // 端到端（§6.2 + §11.2）：一个**未提交的迁移**在崩溃后被撤销——
        // 源槽位恢复为原行（Normal），目的页上的新行消失。
        let io = mem();
        let mut undo_file = DataFile::create(&io, Path::new(UNDO_F), 1, 1, WS, 512).unwrap();
        let undo_handle = undo_file.handle();
        let segment = create_undo_segment(&mut undo_file, 2, 3, 4).unwrap();
        let mut chain = UndoChain::open(segment);
        let data_handle = {
            let data_file = DataFile::create(&io, Path::new(DATA_F), 3, 3, WS, 512).unwrap();
            let h = data_file.handle();
            for block in 1..=2u32 {
                let mut page = Page::new(PageType::HeapTable, WS, 3, block);
                pagefile::write_page(&io, h, block, &mut page).unwrap();
            }
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
        let policy = InsertPolicy::in_place(0);
        let key1 = BufferKey::new(WS, rdba(3, 1));
        let key2 = BufferKey::new(WS, rdba(3, 2));

        // 胜者：放一行（并填页，逼出跨页迁移）后提交。
        let old_row = row_bytes(b"survivor");
        let mut w = begin(&pool, &mut log, &mut chain, seq(0)).unwrap();
        for k in 0..10u8 {
            let f = row_bytes(&vec![k; 1500]);
            insert_row(&pool, &mut log, &mut chain, &mut w, key1, &f, &policy).unwrap();
        }
        let rid = insert_row(&pool, &mut log, &mut chain, &mut w, key1, &old_row, &policy).unwrap();
        commit(&pool, &mut log, &mut chain, &mut w, seq(1)).unwrap();

        // 输家：迁移（改长 4KB，跨页）后**不提交**。
        let big = row_bytes(&vec![7u8; 4000]);
        let mut l = begin(&pool, &mut log, &mut chain, seq(1)).unwrap();
        let mut alloc = move |_need: usize| -> Result<BufferKey, TxnError> { Ok(key2) };
        let out = update_row(
            &pool,
            &mut log,
            &mut chain,
            &mut l,
            key1,
            rid.row_id(),
            &big,
            &policy,
            &mut alloc,
        )
        .unwrap();
        assert!(out.migrated);
        let target = {
            let src = page_snapshot(&pool, key1);
            heap::forwarding_target(&src, rid.row_id()).expect("迁移指针")
        };

        // **崩溃**：日志耐久；数据页/undo 页留在池里（no-force）。
        log.flush(log.appended_lsn()).unwrap();
        drop(pool);

        // 恢复：分析/重做/撤销。
        let cf_ro = ControlFile::open(&io, Path::new(A), Path::new(B)).unwrap();
        let groups = online_groups(&io, &cf_ro, Path::new(WAL), spec()).unwrap();
        let mut resolve = |rd: Rdba| match rd.file_id() {
            1 => Some((undo_handle, rd.block_id())),
            3 => Some((data_handle, rd.block_id())),
            _ => None,
        };
        let report = recover(&io, &groups, lsn(0), &chain, &mut log, &mut resolve).unwrap();
        assert_eq!(report.undo.txns_rolled_back, 1, "迁移事务是输家");

        let src = pagefile::read_page_verified(&io, data_handle, 1).unwrap();
        let dst = pagefile::read_page_verified(&io, data_handle, 2).unwrap();
        assert_eq!(
            heap::row(&src, rid.row_id()),
            Some(&stored_row(&old_row, 0)[..]),
            "源槽位恢复为原行"
        );
        assert_eq!(
            heap::forwarding_target(&src, rid.row_id()),
            None,
            "指针已还原"
        );
        assert_eq!(heap::row(&dst, target.row_id()), None, "新行被撤销");
    }

    #[test]
    fn second_writer_waits_then_succeeds_after_the_holder_ends() {
        // §5.4.2 ①：行锁的"他人持锁 ⇒ 等待"分支；持锁者结束后唤醒重试成功。
        let io = mem();
        let (mut undo_file, _dh, pool, mut cf) = harness(&io, ArchiveMode::NoArchive);
        let mut chain = UndoChain::open(create_undo_segment(&mut undo_file, 2, 3, 4).unwrap());
        let mut log = GroupWriter::create(&io, &mut cf, Path::new(WAL), spec(), lsn(0)).unwrap();
        let policy = InsertPolicy::in_place(0);
        let key = BufferKey::new(WS, rdba(3, 1));

        // 已提交的行（前一个事务）。
        let row = row_bytes(b"lock-target");
        let mut t0 = begin(&pool, &mut log, &mut chain, seq(0)).unwrap();
        let rid = insert_row(&pool, &mut log, &mut chain, &mut t0, key, &row, &policy).unwrap();
        commit(&pool, &mut log, &mut chain, &mut t0, seq(1)).unwrap();

        // t1 锁住它（等长更新——就地），不提交。
        let v1 = row_bytes(b"lock-target"); // 同长
        let mut t1 = begin(&pool, &mut log, &mut chain, seq(1)).unwrap();
        update_row(
            &pool,
            &mut log,
            &mut chain,
            &mut t1,
            key,
            rid.row_id(),
            &v1,
            &policy,
            &mut no_alloc,
        )
        .unwrap();

        // t2 想改同一行 ⇒ `RowLocked`（等待分支），登记等待并唤醒语义由注册表承担。
        let mut t2 = begin(&pool, &mut log, &mut chain, seq(1)).unwrap();
        let mut reg = crate::lock::WaitRegistry::new();
        let err = update_row(
            &pool,
            &mut log,
            &mut chain,
            &mut t2,
            key,
            rid.row_id(),
            &v1,
            &policy,
            &mut no_alloc,
        )
        .unwrap_err();
        match err {
            TxnError::RowLocked { holder, row } => {
                assert_eq!(holder, t1.txn_id, "持锁者是 t1");
                assert_eq!(
                    row,
                    RowId::from_parts(3, 1, rid.row_id()).unwrap(),
                    "等的是那一行"
                );
                reg.register(
                    t2.txn_id,
                    holder,
                    RowId::from_parts(3, 1, rid.row_id()).unwrap(),
                    0,
                );
            }
            other => panic!("应为 RowLocked：{other}"),
        }
        assert_eq!(reg.waiters_of(t1.txn_id).len(), 1);

        // 持锁者结束（回滚）⇒ 唤醒其全部等待者；t2 重试成功。
        rollback(&pool, &mut log, &mut chain, &mut t1).unwrap();
        assert_eq!(
            crate::lock::on_txn_end(&mut reg, t1.txn_id),
            vec![t2.txn_id]
        );
        update_row(
            &pool,
            &mut log,
            &mut chain,
            &mut t2,
            key,
            rid.row_id(),
            &v1,
            &policy,
            &mut no_alloc,
        )
        .unwrap();
        commit(&pool, &mut log, &mut chain, &mut t2, seq(2)).unwrap();
    }

    #[test]
    fn committed_holder_does_not_block_and_reentrant_lock_is_free() {
        // §4.6.3/§5.4.2：**已提交的槽不算"被锁"**（延迟块清除下也如此）；
        // 同事务重复锁同一行 = 重入，不重复计 `lock_cnt`；同块另一行 ⇒ +1。
        let io = mem();
        let (mut undo_file, _dh, pool, mut cf) = harness(&io, ArchiveMode::NoArchive);
        let mut chain = UndoChain::open(create_undo_segment(&mut undo_file, 2, 3, 4).unwrap());
        let mut log = GroupWriter::create(&io, &mut cf, Path::new(WAL), spec(), lsn(0)).unwrap();
        let policy = InsertPolicy::in_place(0);
        let key = BufferKey::new(WS, rdba(3, 1));

        let r1 = row_bytes(b"first-row");
        let mut t0 = begin(&pool, &mut log, &mut chain, seq(0)).unwrap();
        let rid1 = insert_row(&pool, &mut log, &mut chain, &mut t0, key, &r1, &policy).unwrap();
        let rid2 = insert_row(&pool, &mut log, &mut chain, &mut t0, key, &r1, &policy).unwrap();
        commit(&pool, &mut log, &mut chain, &mut t0, seq(1)).unwrap();
        // 加锁（更新 rid1 与 rid2：同块两行 ⇒ lock_cnt = 2）。
        let v = row_bytes(b"first-row");
        let mut t1 = begin(&pool, &mut log, &mut chain, seq(0)).unwrap();
        update_row(
            &pool,
            &mut log,
            &mut chain,
            &mut t1,
            key,
            rid1.row_id(),
            &v,
            &policy,
            &mut no_alloc,
        )
        .unwrap();
        update_row(
            &pool,
            &mut log,
            &mut chain,
            &mut t1,
            key,
            rid2.row_id(),
            &v,
            &policy,
            &mut no_alloc,
        )
        .unwrap();
        let page = page_snapshot(&pool, key);
        let slot = itl_of(&page, 0);
        assert_eq!(slot.txn_id, t1.txn_id);
        assert_eq!(slot.lock_cnt, 2, "同块两行锁 = 2");
        // 重入（再改 rid1）不重复计数。
        update_row(
            &pool,
            &mut log,
            &mut chain,
            &mut t1,
            key,
            rid1.row_id(),
            &v,
            &policy,
            &mut no_alloc,
        )
        .unwrap();
        assert_eq!(
            itl_of(&page_snapshot(&pool, key), 0).lock_cnt,
            2,
            "重入不重复计"
        );

        // t1 提交：其槽变 `Committed` ⇒ 后继事务**不被阻塞**（已提交不算锁）。
        commit(&pool, &mut log, &mut chain, &mut t1, seq(1)).unwrap();
        // 注意：未清除的条目外观仍是 Active——锁判定查事务表定夺。
        let mut t2 = begin(&pool, &mut log, &mut chain, seq(1)).unwrap();
        update_row(
            &pool,
            &mut log,
            &mut chain,
            &mut t2,
            key,
            rid1.row_id(),
            &v,
            &policy,
            &mut no_alloc,
        )
        .unwrap();
        commit(&pool, &mut log, &mut chain, &mut t2, seq(2)).unwrap();
    }

    #[test]
    fn deadlock_detection_finds_the_cycle_and_picks_the_lighter_victim() {
        // §5.4.2 ④：等待超阈值 → 环检测 → 牺牲者 = 已修改行数最少者。
        let io = mem();
        let (mut undo_file, _dh, pool, mut cf) = harness(&io, ArchiveMode::NoArchive);
        let mut chain = UndoChain::open(create_undo_segment(&mut undo_file, 2, 3, 4).unwrap());
        let mut log = GroupWriter::create(&io, &mut cf, Path::new(WAL), spec(), lsn(0)).unwrap();
        let policy = InsertPolicy::in_place(0);
        let key = BufferKey::new(WS, rdba(3, 1));

        let r = row_bytes(b"row-lock");
        let mut t0 = begin(&pool, &mut log, &mut chain, seq(0)).unwrap();
        let rid_a = insert_row(&pool, &mut log, &mut chain, &mut t0, key, &r, &policy).unwrap();
        let rid_b = insert_row(&pool, &mut log, &mut chain, &mut t0, key, &r, &policy).unwrap();
        commit(&pool, &mut log, &mut chain, &mut t0, seq(1)).unwrap();

        // t1 锁 A（1 次更新）；t2 锁 B（2 次更新——修改量更大）。
        let mut t1 = begin(&pool, &mut log, &mut chain, seq(1)).unwrap();
        let v = row_bytes(b"row-lock");
        update_row(
            &pool,
            &mut log,
            &mut chain,
            &mut t1,
            key,
            rid_a.row_id(),
            &v,
            &policy,
            &mut no_alloc,
        )
        .unwrap();
        let mut t2 = begin(&pool, &mut log, &mut chain, seq(1)).unwrap();
        update_row(
            &pool,
            &mut log,
            &mut chain,
            &mut t2,
            key,
            rid_b.row_id(),
            &v,
            &policy,
            &mut no_alloc,
        )
        .unwrap();
        let r2 = row_bytes(b"row-lock2");
        update_row(
            &pool,
            &mut log,
            &mut chain,
            &mut t2,
            key,
            rid_b.row_id(),
            &r2,
            &policy,
            &mut no_alloc,
        )
        .unwrap();

        // 互相等待（把"等不到"登记进等待结构；资源字段仅诊断）。
        let mut reg = crate::lock::WaitRegistry::new();
        let a = RowId::from_parts(3, 1, rid_a.row_id()).unwrap();
        let b = RowId::from_parts(3, 1, rid_b.row_id()).unwrap();
        reg.register(t1.txn_id, t2.txn_id, b, 0);
        reg.register(t2.txn_id, t1.txn_id, a, 0);

        // 阈值未到 ⇒ 不检测。
        assert!(crate::lock::detect_deadlock(&reg, &chain, 1_000, 3_000)
            .unwrap()
            .is_none());
        // 超阈值 ⇒ 检出环，牺牲者 = 修改量少的 t1。
        let dl = crate::lock::detect_deadlock(&reg, &chain, 5_000, 3_000)
            .unwrap()
            .expect("应检出死锁");
        assert_eq!(dl.cycle.len(), 2);
        assert!(dl.cycle.contains(&t1.txn_id) && dl.cycle.contains(&t2.txn_id));
        assert_eq!(dl.victim, t1.txn_id, "牺牲者 = 已修改行数最少者");
        assert!(dl.victim_work < 2 || dl.victim_work <= 2, "修改量代理值");

        // 牺牲者"不再等待" ⇒ 环即解开（会话层的语句级回滚是其后续动作）。
        reg.cancel(t1.txn_id);
        assert!(crate::lock::detect_deadlock(&reg, &chain, 5_000, 3_000)
            .unwrap()
            .is_none());
    }

    /// 迁移分配口：**拒绝换页**（用例里的改长都应同页完成；跨页迁移由专门
    /// 用例给出真实的分配口）。
    fn no_alloc(need: usize) -> Result<BufferKey, TxnError> {
        Err(TxnError::NoMigrationTarget { need })
    }

    /// 页快照（经池钉住后拷贝——测试读 ITL 条目用）。
    fn page_snapshot(pool: &BufferPool<'_>, key: BufferKey) -> Page {
        let g = pool.pin(key).unwrap();
        Page::from_bytes(Box::new(*g.as_bytes()))
    }

    fn itl_of(page: &Page, slot: u16) -> bicdb_storage::itl::ItlEntry {
        bicdb_storage::itl::read_itl(page, slot).unwrap()
    }

    #[test]
    fn uncommitted_delete_is_marked_and_invisible_to_cr() {
        // 审核修复回归（A1）：**第二个事务的删除**必须在页上留下自己的 ITL
        // 条目——没有它，未提交删除对所有快照可见（脏读）。
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
        let row = row_bytes(b"del");
        let mut t0 = begin(&pool, &mut log, &mut chain, seq(1)).unwrap();
        let rid = insert_row(
            &pool,
            &mut log,
            &mut chain,
            &mut t0,
            key,
            &row,
            &InsertPolicy::in_place(0),
        )
        .unwrap();
        commit(&pool, &mut log, &mut chain, &mut t0, seq(1)).unwrap();

        // T1：删除，不提交。
        let mut t1 = begin(&pool, &mut log, &mut chain, seq(2)).unwrap();
        delete_row(&pool, &mut log, &mut chain, &mut t1, key, rid.row_id()).unwrap();

        let page = page_snapshot(&pool, key);
        let count = bicdb_storage::itl::itl_count(&page).unwrap();
        assert!(
            (0..count).any(|i| {
                let e = itl_of(&page, i);
                e.state == bicdb_storage::itl::ItlState::Active && e.txn_id == t1.txn_id
            }),
            "删除者必须在本页有活动 ITL 条目"
        );
        // 能看见 T0 插入（seq 1）、看不见 T1 删除的快照：行仍在。
        let cr = bicdb_storage::cr::reconstruct(&page, seq(1), &chain).unwrap();
        assert_eq!(
            heap::row(&cr, rid.row_id()),
            Some(&stored_row(&row, 0)[..]),
            "未提交删除：看见插入的旧快照必须仍看到该行"
        );
        // 比 T0 还旧的快照：连插入都不可见——行不存在（两段回溯都生效）。
        let cr_ancient = bicdb_storage::cr::reconstruct(&page, seq(0), &chain).unwrap();
        assert_eq!(
            heap::row(&cr_ancient, rid.row_id()),
            None,
            "更旧的快照看不到插入"
        );
        // 提交后：旧快照（seq 1 < 提交序号）仍可见——删除靠 undo 撤销回去。
        commit(&pool, &mut log, &mut chain, &mut t1, seq(2)).unwrap();
        let page = page_snapshot(&pool, key);
        let cr_old = bicdb_storage::cr::reconstruct(&page, seq(1), &chain).unwrap();
        assert_eq!(
            heap::row(&cr_old, rid.row_id()),
            Some(&stored_row(&row, 0)[..]),
            "提交后：旧快照（含等号之下）仍看到删除前的行"
        );
        let cr_new = bicdb_storage::cr::reconstruct(&page, seq(2), &chain).unwrap();
        assert_eq!(heap::row(&cr_new, rid.row_id()), None, "新快照看不到已删行");
    }

    #[test]
    fn one_transaction_uses_one_itl_entry_per_block() {
        // 审核修复回归（A2）：同一事务在同一页插两次 ⇒ **只占一个** ITL 条目，
        // CR 一轮即可回溯（此前两个条目会把 CR 卡进 TooManyRounds）。
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
        let row = row_bytes(b"two");
        let mut txn = begin(&pool, &mut log, &mut chain, seq(1)).unwrap();
        let r1 = insert_row(
            &pool,
            &mut log,
            &mut chain,
            &mut txn,
            key,
            &row,
            &InsertPolicy::in_place(0),
        )
        .unwrap();
        let r2 = insert_row(
            &pool,
            &mut log,
            &mut chain,
            &mut txn,
            key,
            &row,
            &InsertPolicy::in_place(0),
        )
        .unwrap();

        let page = page_snapshot(&pool, key);
        assert_eq!(
            bicdb_storage::itl::itl_count(&page).unwrap(),
            1,
            "一个事务每块只占一个 ITL 条目"
        );
        // 未提交：CR 把两行都抹掉（不再 TooManyRounds）。
        let cr = bicdb_storage::cr::reconstruct(&page, seq(0), &chain).unwrap();
        assert_eq!(heap::row(&cr, r1.row_id()), None);
        assert_eq!(heap::row(&cr, r2.row_id()), None);
        // 提交后可见。
        commit(&pool, &mut log, &mut chain, &mut txn, seq(1)).unwrap();
        let page = page_snapshot(&pool, key);
        let cr = bicdb_storage::cr::reconstruct(&page, seq(1), &chain).unwrap();
        assert!(heap::row(&cr, r1.row_id()).is_some());
        assert!(heap::row(&cr, r2.row_id()).is_some());
    }

    #[test]
    fn committed_itl_entries_are_reused_across_transactions() {
        // 审核修复回归（A6）：延迟块清除让**已提交**的条目回到可复用集合——
        // 同一页连续 40 个写事务不应把 32 个 ITL 槽用尽。
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
        let row = row_bytes(b"x");
        for i in 0..40u64 {
            let mut txn = begin(&pool, &mut log, &mut chain, seq(i + 1)).unwrap();
            insert_row(
                &pool,
                &mut log,
                &mut chain,
                &mut txn,
                key,
                &row,
                &InsertPolicy::in_place(0),
            )
            .unwrap();
            commit(&pool, &mut log, &mut chain, &mut txn, seq(i + 1)).unwrap();
        }
        let page = page_snapshot(&pool, key);
        assert_eq!(
            bicdb_storage::itl::itl_count(&page).unwrap(),
            1,
            "清除 + 复用：40 个事务仍只占 1 个 ITL 槽"
        );
        // 40 行都在（各事务各插一行）。
        assert_eq!(page.slot_count(), 40);
    }

    #[test]
    fn pooled_write_path_advances_over_the_undo_bitmap_page() {
        // 审核修复回归（B2，池路径）：撤销段 `append_pos` 走到**段内位图页**
        // （窗口首位）时——全新位图页先格式化 fsync、扩展页经池写 redo——
        // DML 必须继续成功，位图页落盘可用（此前该点之后写路径断裂）。
        let io = mem();
        let mut undo_file = DataFile::create(&io, Path::new(UNDO_F), 1, 1, WS, 512).unwrap();
        let undo_handle = undo_file.handle();
        let segment = create_undo_segment(&mut undo_file, 2, 3, 4)
            .unwrap()
            .with_coverage(8); // 窗口缩到 8 页：约 200 事务内走到位图页
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
            &RedoEntries::new(4, 1).unwrap(),
            &ArchiveRecord::default(),
        )
        .unwrap();
        // 大日志组：本用例不做检查点，组切满会 Blocked（与位图页无关）。
        let mut log = GroupWriter::create(
            &io,
            &mut cf,
            Path::new(WAL),
            GroupSpec::new(4, 1, 256).unwrap(),
            lsn(0),
        )
        .unwrap();

        let key = BufferKey::new(WS, rdba(3, 1));
        // 用**大行**让撤销页快速填满（每事务 delete+insert ≈ 2KB 撤销记录）。
        let payload = vec![0x5Au8; 2000];
        let row = row_bytes(&payload);
        let mut t0 = begin(&pool, &mut log, &mut chain, seq(1)).unwrap();
        let rid = insert_row(
            &pool,
            &mut log,
            &mut chain,
            &mut t0,
            key,
            &row,
            &InsertPolicy::in_place(0),
        )
        .unwrap();
        commit(&pool, &mut log, &mut chain, &mut t0, seq(1)).unwrap();

        // 每事务做一次"整行都变"的等长更新：Update 补丁 ≈ 2KB ⇒ 撤销页
        // 快速填满（数据页不长大——等长就地）。
        for i in 0..40u64 {
            let mut txn = begin(&pool, &mut log, &mut chain, seq(i + 2)).unwrap();
            let variant = row_bytes(&vec![i as u8; 2000]);
            update_row(
                &pool,
                &mut log,
                &mut chain,
                &mut txn,
                key,
                rid.row_id(),
                &variant,
                &InsertPolicy::in_place(0),
                &mut no_alloc,
            )
            .unwrap_or_else(|e| panic!("第 {i} 个事务 update 失败：{e}"));
            commit(&pool, &mut log, &mut chain, &mut txn, seq(i + 2)).unwrap();
        }
        // 段头：已跨位图页（可能跨了多个窗口——40 次 ≈2KB 的更新）。
        let h =
            bicdb_storage::segment::read_header(&chain.segment().read_page(0).unwrap()).unwrap();
        assert!(h.bitmap_pages >= 2, "段内位图页已物化到 {}", h.bitmap_pages);
        assert!(h.append_pos > 8, "append_pos = {}", h.append_pos);
        // 每个已物化的位图页**都已在磁盘上可用**（先格式化 fsync 的结果）。
        for w in 1..u32::from(h.bitmap_pages) {
            let logical = w * 8; // 本用例的 coverage = 8
            let block = chain.segment().logical_block(logical).unwrap();
            let bmp = pagefile::read_page_verified(&io, undo_handle, block).unwrap();
            assert_eq!(
                bicdb_storage::bitmap::own_index(&bmp).unwrap(),
                w as u16,
                "位图页 i = {w}"
            );
            assert_eq!(
                bicdb_storage::bitmap::free_level(&bmp, 0).unwrap(),
                bicdb_storage::bitmap::FreeLevel::Full,
                "窗口首位自指恒满"
            );
        }
    }

    #[test]
    fn failed_insert_leaves_no_ghost_undo_record() {
        // 审核修复回归（A3）：插入失败的输入/空间检查发生在**记记录之前**——
        // 链头不被推进（幽灵 `ITL 覆盖` 记录会在他处回滚时覆盖他人条目）。
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
        let mut txn = begin(&pool, &mut log, &mut chain, seq(1)).unwrap();
        // ① 行头非法（row_len 与长度不符）⇒ 失败且不留记录。
        let mut bad = row_bytes(b"ghost");
        bad[2] = bad[2].wrapping_add(7); // 改 row_len 低字节
        assert!(insert_row(
            &pool,
            &mut log,
            &mut chain,
            &mut txn,
            key,
            &bad,
            &InsertPolicy::in_place(0)
        )
        .is_err());
        // ② 一次超过整页的行 ⇒ PageFull 且不留记录。
        let big = row_bytes(&vec![b'z'; 20000]);
        let _ = insert_row(
            &pool,
            &mut log,
            &mut chain,
            &mut txn,
            key,
            &big,
            &InsertPolicy::in_place(0),
        );
        let header = chain.segment().read_page(0).unwrap();
        assert_eq!(
            read_slot(&header, txn.slot).unwrap().undo_current,
            None,
            "失败的插入不得在链上留下任何记录"
        );
        // 页也一个字节未动。
        let page = page_snapshot(&pool, key);
        assert_eq!(page.slot_count(), 0);
    }

    #[test]
    fn undo_segment_extension_is_redo_protected() {
        // 把撤销段的追加位置推到首区之外 ⇒ 第一次写入触发**经池扩展**：
        // 段头页/段内位图页/文件位图页的改动都要**进日志**（崩溃可重放）。
        let io = mem();
        let mut undo_file = DataFile::create(&io, Path::new(UNDO_F), 1, 1, WS, 512).unwrap();
        let undo_handle = undo_file.handle();
        let segment = create_undo_segment(&mut undo_file, 2, 3, 4).unwrap();
        let mut chain = UndoChain::open(segment);
        // 推 append_pos 到首区之外（1 区 = 8 逻辑页）。
        {
            let mut page = chain.segment().read_page(0).unwrap();
            let mut h = bicdb_storage::segment::read_header(&page).unwrap();
            h.append_pos = 8;
            bicdb_storage::segment::write_header(&mut page, &h).unwrap();
            chain.segment().write_page(0, &mut page).unwrap();
        }
        assert_eq!(chain.segment().header().extent_count, 1);
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
        let row = row_bytes(b"ext");
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
        commit(&pool, &mut log, &mut chain, &mut txn, seq(1)).unwrap();
        pool.flush_workspace(WS).unwrap();

        // 扩展发生了（第二个区）。
        assert_eq!(chain.segment().header().extent_count, 2, "段已扩展");
        // 日志里有指向**文件级位图页**（file 1 的 runs[0] = 块 1）与
        // **段头页**的页修改记录——扩展的全部改动都受 redo 保护。
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
        let touches = |file: u16, block: u32| {
            scan.records.iter().any(|r| {
                r.blocks
                    .iter()
                    .any(|b| b.rdba.file_id() == file && b.rdba.block_id() == block)
            })
        };
        assert!(touches(1, 1), "文件级位图页（块 1）有 redo");
        let page0_block = chain.segment().logical_block(0).unwrap();
        assert!(touches(1, page0_block), "段头页有 redo");
        // 数据页照常。
        assert!(touches(3, 1), "数据页有 redo");
        let _ = rid;
    }

    #[test]
    fn live_cr_reads_the_chain_directly_after_dml() {
        // 撤销页"经池写 + 立即 flush"的直接回报：**活系统的 CR/回滚直读链**即可
        // 看到刚写下的记录（池里没有脏的 undo 页）。
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
        let row = row_bytes(b"cr");
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

        // 直读链：槽的链头可读、两条记录类别正确（undo 页已 flush，直读可见）。
        let header = chain.segment().read_page(0).unwrap();
        let head = read_slot(&header, txn.slot).unwrap().undo_current.unwrap();
        let rec = chain.read(head).unwrap();
        assert_eq!(rec.op, UndoOp::Insert);
        assert_eq!(rec.rowid, rid);
        let prev = chain.read(rec.prev.unwrap()).unwrap();
        assert_eq!(prev.op, UndoOp::ItlOverwrite);

        // 活系统 CR（直读链）：未提交 ⇒ 行不可见；提交后 ⇒ 可见。
        let snapshot_page = {
            let g = pool.pin(key).unwrap();
            bicdb_storage::page::Page::from_bytes(Box::new(*g.as_bytes()))
        };
        let uncommitted = bicdb_storage::cr::reconstruct(&snapshot_page, seq(0), &chain).unwrap();
        assert_eq!(
            heap::row(&uncommitted, rid.row_id()),
            None,
            "未提交：CR 抹掉"
        );
        commit(&pool, &mut log, &mut chain, &mut txn, seq(1)).unwrap();
        let committed = bicdb_storage::cr::reconstruct(&snapshot_page, seq(1), &chain).unwrap();
        assert_eq!(
            heap::row(&committed, rid.row_id()).map(|b| b.len()),
            Some(row.len()),
            "提交后：CR 可见"
        );
    }

    #[test]
    fn statement_rollback_keeps_earlier_statements_and_locks() {
        // §4.6.6 ②：语句失败 ⇒ 沿链补偿到回滚点——**不释放锁**、
        // **不回滚此前的成功语句**。
        let io = mem();
        let (mut undo_file, _dh, pool, mut cf) = harness(&io, ArchiveMode::NoArchive);
        let mut chain = UndoChain::open(create_undo_segment(&mut undo_file, 2, 3, 4).unwrap());
        let mut log = GroupWriter::create(&io, &mut cf, Path::new(WAL), spec(), lsn(0)).unwrap();
        let policy = InsertPolicy::in_place(0);
        let key = BufferKey::new(WS, rdba(3, 1));

        let mut t1 = begin(&pool, &mut log, &mut chain, seq(0)).unwrap();
        // 语句 1（成功语句：语句回滚不得动它）。
        let a = insert_row(
            &pool,
            &mut log,
            &mut chain,
            &mut t1,
            key,
            &row_bytes(b"stmt-1-a"),
            &policy,
        )
        .unwrap();
        // 语句 2：取回滚点 → 插入 B → "失败" ⇒ 回滚到回滚点。
        let mark = statement_mark(&mut chain, &t1).unwrap();
        let b = insert_row(
            &pool,
            &mut log,
            &mut chain,
            &mut t1,
            key,
            &row_bytes(b"stmt-2-b"),
            &policy,
        )
        .unwrap();
        let undone = rollback_to_mark(&pool, &mut log, &mut chain, &mut t1, mark).unwrap();
        assert_eq!(undone, 1, "语句 2 的这一条插入被补偿");
        {
            let g = pool.pin(key).unwrap();
            assert!(heap::row(&g, a.row_id()).is_some(), "语句 1 的行保留");
            assert!(heap::row(&g, b.row_id()).is_none(), "语句 2 的行已撤销");
        }
        assert_eq!(t1.state, TxnState::Active, "事务仍在（只是语句回滚）");

        // **锁不释放**：另一事务改 A 仍被挡（RowLocked）。
        let mut t2 = begin(&pool, &mut log, &mut chain, seq(0)).unwrap();
        let err = update_row(
            &pool,
            &mut log,
            &mut chain,
            &mut t2,
            key,
            a.row_id(),
            &row_bytes(b"x"),
            &policy,
            &mut no_alloc,
        )
        .unwrap_err();
        assert!(
            matches!(err, TxnError::RowLocked { holder, .. } if holder == t1.txn_id),
            "语句回滚不释锁：{err}"
        );

        // **孤链不可达**：整事务回滚只走剩下的一节链——A 被撤销、B 不会再被
        // "补偿出来"（回滚点让被撤销的记录从此不可达）。
        rollback(&pool, &mut log, &mut chain, &mut t1).unwrap();
        {
            let g = pool.pin(key).unwrap();
            assert!(heap::row(&g, a.row_id()).is_none(), "整事务回滚撤销 A");
            assert!(heap::row(&g, b.row_id()).is_none(), "B 保持已撤销");
        }
    }

    #[test]
    fn statement_mark_without_changes_is_a_noop_and_idempotent() {
        let io = mem();
        let (mut undo_file, _dh, pool, mut cf) = harness(&io, ArchiveMode::NoArchive);
        let mut chain = UndoChain::open(create_undo_segment(&mut undo_file, 2, 3, 4).unwrap());
        let mut log = GroupWriter::create(&io, &mut cf, Path::new(WAL), spec(), lsn(0)).unwrap();
        let policy = InsertPolicy::in_place(0);
        let key = BufferKey::new(WS, rdba(3, 1));

        let mut t0 = begin(&pool, &mut log, &mut chain, seq(0)).unwrap();
        let rid = insert_row(
            &pool,
            &mut log,
            &mut chain,
            &mut t0,
            key,
            &row_bytes(b"row-0"),
            &policy,
        )
        .unwrap();
        commit(&pool, &mut log, &mut chain, &mut t0, seq(1)).unwrap();

        let mut t1 = begin(&pool, &mut log, &mut chain, seq(1)).unwrap();
        let mark = statement_mark(&mut chain, &t1).unwrap();
        // 无改动的语句：回滚点是当前链头 ⇒ 零补偿、幂等。
        assert_eq!(
            rollback_to_mark(&pool, &mut log, &mut chain, &mut t1, mark).unwrap(),
            0
        );
        assert_eq!(
            rollback_to_mark(&pool, &mut log, &mut chain, &mut t1, mark).unwrap(),
            0,
            "重复回滚到同一回滚点 = 无操作"
        );
        // 后续语句照常。
        update_row(
            &pool,
            &mut log,
            &mut chain,
            &mut t1,
            key,
            rid.row_id(),
            &row_bytes(b"row-1"),
            &policy,
            &mut no_alloc,
        )
        .unwrap();
        rollback(&pool, &mut log, &mut chain, &mut t1).unwrap();
        let g = pool.pin(key).unwrap();
        let got = heap::row(&g, rid.row_id()).expect("行在").to_vec();
        let want = row_bytes(b"row-0");
        assert_eq!(got.len(), want.len());
        assert_eq!(
            got[2..],
            want[2..],
            "负载逐字节相同（行头 itl_slot 由写路径回填）"
        );
    }

    #[test]
    fn driver_rolls_back_the_statement_when_it_is_the_deadlock_victim() {
        // §5.4.2 ④ + §4.6.6 ②：环上修改量最少者做**语句级回滚**并返回
        // `DeadlockVictim`——本语句的改动撤销、**此前语句的改动保留**、
        // 事务仍在（不释锁），等待被取消（牺牲者不再等待 ⇒ 环解开）。
        let io = mem();
        let (mut undo_file, _dh, pool, mut cf) = harness(&io, ArchiveMode::NoArchive);
        let mut chain = UndoChain::open(create_undo_segment(&mut undo_file, 2, 3, 4).unwrap());
        let mut log = GroupWriter::create(&io, &mut cf, Path::new(WAL), spec(), lsn(0)).unwrap();
        let policy = InsertPolicy::in_place(0);
        let key = BufferKey::new(WS, rdba(3, 1));

        // 前置行（已提交）。
        let mut t0 = begin(&pool, &mut log, &mut chain, seq(0)).unwrap();
        let r_stmt = insert_row(
            &pool,
            &mut log,
            &mut chain,
            &mut t0,
            key,
            &row_bytes(b"AAA"),
            &policy,
        )
        .unwrap();
        let r_held = insert_row(
            &pool,
            &mut log,
            &mut chain,
            &mut t0,
            key,
            &row_bytes(b"BBB"),
            &policy,
        )
        .unwrap();
        let r_extra = insert_row(
            &pool,
            &mut log,
            &mut chain,
            &mut t0,
            key,
            &row_bytes(b"CCC"),
            &policy,
        )
        .unwrap();
        commit(&pool, &mut log, &mut chain, &mut t0, seq(1)).unwrap();

        // t1：先前语句改一行（保留）；t2：改两行（修改量更大 ⇒ 它不是牺牲者）。
        let mut t1 = begin(&pool, &mut log, &mut chain, seq(1)).unwrap();
        update_row(
            &pool,
            &mut log,
            &mut chain,
            &mut t1,
            key,
            r_stmt.row_id(),
            &row_bytes(b"aaa"),
            &policy,
            &mut no_alloc,
        )
        .unwrap();
        let mut t2 = begin(&pool, &mut log, &mut chain, seq(1)).unwrap();
        update_row(
            &pool,
            &mut log,
            &mut chain,
            &mut t2,
            key,
            r_held.row_id(),
            &row_bytes(b"bbb"),
            &policy,
            &mut no_alloc,
        )
        .unwrap();
        update_row(
            &pool,
            &mut log,
            &mut chain,
            &mut t2,
            key,
            r_extra.row_id(),
            &row_bytes(b"ccc"),
            &policy,
            &mut no_alloc,
        )
        .unwrap();
        update_row(
            &pool,
            &mut log,
            &mut chain,
            &mut t2,
            key,
            r_stmt.row_id(),
            &row_bytes(b"ccc"),
            &policy,
            &mut no_alloc,
        )
        .unwrap_err(); // t1 持有 ⇒ t2 等 t1

        // 等待图：t1 → t2（本测试里 t2 的"等 t1"手动登记，模拟另一会话在挂起）。
        let gate = crate::lock::WaitGate::new();
        gate.register(
            t2.txn_id,
            t1.txn_id,
            RowId::from_parts(3, 1, r_stmt.row_id()).unwrap(),
            0,
        );

        // t1 的语句：先改一行（语句改动，将被回滚），再改 t2 持有的行 ⇒ 转等待。
        let held_by_t2 = r_held.row_id();
        let mut first = true;
        let now = || 10_000u64; // 固定时钟 ⇒ 等待时长恒超阈值
        let policy_wait = WaitPolicy {
            park_timeout: std::time::Duration::from_millis(1),
            deadlock_threshold_ms: 0,
            max_waits: Some(8),
        };
        let result = execute_with_wait(
            &pool,
            &mut log,
            &mut chain,
            &mut t1,
            &gate,
            &policy_wait,
            now,
            |pool, log, chain, txn| {
                if first {
                    first = false;
                    update_row(
                        pool,
                        log,
                        chain,
                        txn,
                        key,
                        r_stmt.row_id(),
                        &row_bytes(b"zzz"),
                        &policy,
                        &mut no_alloc,
                    )?;
                }
                update_row(
                    pool,
                    log,
                    chain,
                    txn,
                    key,
                    held_by_t2,
                    &row_bytes(b"yyy"),
                    &policy,
                    &mut no_alloc,
                )
            },
        );
        match result {
            Err(TxnError::DeadlockVictim { cycle }) => {
                assert!(
                    cycle.contains(&t1.txn_id) && cycle.contains(&t2.txn_id),
                    "环上两方：{cycle:?}"
                );
            }
            other => panic!("应为 DeadlockVictim：{other:?}"),
        }
        assert!(
            gate.waiters_of(t2.txn_id).is_empty(),
            "牺牲者不再等待（环解开）"
        );
        assert_eq!(t1.state, TxnState::Active, "语句回滚 ≠ 事务回滚");
        {
            let g = pool.pin(key).unwrap();
            // 本语句对 r_stmt 的改动已回滚；**此前语句**的改动保留。
            let got = heap::row(&g, r_stmt.row_id()).expect("行在").to_vec();
            let want = row_bytes(b"aaa");
            assert_eq!(got[2..], want[2..], "回滚到语句前（负载 aaa）");
            assert!(
                heap::row(&g, held_by_t2).is_some(),
                "被等的那行未被本事务改动"
            );
        }
    }

    #[test]
    fn second_session_parks_until_the_holder_commits_then_succeeds() {
        // 真跨线程的"等待 → 唤醒 → 重试"（§5.4.2 ①/②）：两个会话共享缓冲池
        // 与**同一个撤销段**（事务表是全段的——持锁者判定要按 `txn_id` 查它，
        // 跨段查不到会被当成"陈旧字节"而误判可锁）；等待在 latch 之外发生、
        // **唤醒不做移交**。
        let io = mem();
        let mut undo_file = DataFile::create(&io, Path::new(UNDO_F), 1, 1, WS, 512).unwrap();
        let undo_handle = undo_file.handle();
        let segment = create_undo_segment(&mut undo_file, 2, 3, 4).unwrap();
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
            &ArchiveRecord::new(ArchiveMode::NoArchive),
        )
        .unwrap();
        let chain = std::sync::Mutex::new(UndoChain::open(segment));
        let log = std::sync::Mutex::new(
            GroupWriter::create(&io, &mut cf, Path::new(WAL), spec(), lsn(0)).unwrap(),
        );
        let policy = InsertPolicy::in_place(0);
        let key = BufferKey::new(WS, rdba(3, 1));

        // 前置行（已提交）。
        let rid = {
            let mut chain = chain.lock().unwrap();
            let mut log = log.lock().unwrap();
            let mut t0 = begin(&pool, &mut log, &mut chain, seq(0)).unwrap();
            let rid = insert_row(
                &pool,
                &mut log,
                &mut chain,
                &mut t0,
                key,
                &row_bytes(b"0123"),
                &policy,
            )
            .unwrap();
            commit(&pool, &mut log, &mut chain, &mut t0, seq(1)).unwrap();
            rid
        };

        let gate = std::sync::Arc::new(crate::lock::WaitGate::new());
        let (locked_tx, locked_rx) = std::sync::mpsc::channel::<()>();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();

        std::thread::scope(|scope| {
            // 会话 1：持锁 → 等会话 2 登记 → 提交 → 唤醒。
            let g1 = std::sync::Arc::clone(&gate);
            let (pool1, chain1, log1) = (&pool, &chain, &log);
            let a = scope.spawn(move || {
                let mut t1;
                {
                    let mut chain = chain1.lock().unwrap();
                    let mut log = log1.lock().unwrap();
                    t1 = begin(pool1, &mut log, &mut chain, seq(1)).unwrap();
                    update_row(
                        pool1,
                        &mut log,
                        &mut chain,
                        &mut t1,
                        key,
                        rid.row_id(),
                        &row_bytes(b"1111"),
                        &policy,
                        &mut no_alloc,
                    )
                    .unwrap();
                }
                locked_tx.send(()).unwrap();
                // 等会话 2 挂上（登记在册）。
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while g1.waiters_of(t1.txn_id).is_empty() && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                {
                    let mut chain = chain1.lock().unwrap();
                    let mut log = log1.lock().unwrap();
                    commit(pool1, &mut log, &mut chain, &mut t1, seq(2)).unwrap();
                }
                let woken = g1.wake(t1.txn_id);
                assert!(!woken.is_empty(), "持锁者结束 ⇒ 唤醒其全部等待者");
            });

            // 会话 2：尝试 → RowLocked ⇒ 登记 + 挂起 → 醒来重试成功。
            let g2 = std::sync::Arc::clone(&gate);
            let (pool2, chain2, log2) = (&pool, &chain, &log);
            let b = scope.spawn(move || {
                locked_rx.recv().unwrap();
                let mut t2 = {
                    let mut chain = chain2.lock().unwrap();
                    let mut log = log2.lock().unwrap();
                    begin(pool2, &mut log, &mut chain, seq(1)).unwrap()
                };
                loop {
                    let attempt = {
                        let mut chain = chain2.lock().unwrap();
                        let mut log = log2.lock().unwrap();
                        update_row(
                            pool2,
                            &mut log,
                            &mut chain,
                            &mut t2,
                            key,
                            rid.row_id(),
                            &row_bytes(b"2222"),
                            &policy,
                            &mut no_alloc,
                        )
                    };
                    match attempt {
                        Ok(_) => break,
                        Err(TxnError::RowLocked { holder, row }) => {
                            g2.register(t2.txn_id, holder, row, 0);
                            assert!(
                                g2.park(t2.txn_id, std::time::Duration::from_secs(5)),
                                "被唤醒"
                            );
                        }
                        Err(other) => panic!("非等待错误：{other}"),
                    }
                }
                {
                    let mut chain = chain2.lock().unwrap();
                    let mut log = log2.lock().unwrap();
                    commit(pool2, &mut log, &mut chain, &mut t2, seq(3)).unwrap();
                }
                done_tx.send(()).unwrap();
            });
            a.join().unwrap();
            b.join().unwrap();
        });
        done_rx.recv().unwrap();
        let g = pool.pin(key).unwrap();
        let got = heap::row(&g, rid.row_id()).expect("行在").to_vec();
        let want = row_bytes(b"2222");
        assert_eq!(got[2..], want[2..], "会话 2 等到了锁并写成功");
    }
}
