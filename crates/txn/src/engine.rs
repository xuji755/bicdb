//! **事务引擎**（REQ-ENG-002）：执行器（SQL / 图 / 检索 / 资产登记）看到的
//! **事务、快照、锁、提交**门面。
//!
//! ```text
//! begin        工作区上下文 → 事务句柄（不跨工作区，REQ-TXN-019）
//! snapshot     事务句柄 → 快照句柄（语句级 RC：语句开始取、结束释放）
//! commit       事务句柄 → 提交序号（提交记录先入日志流 → 标记可见 → 唤醒等待者）
//! rollback     事务句柄 → —（反向应用 undo、释放锁与快照）
//! 语句回滚点   事务句柄 → 回滚点句柄（死锁牺牲者 = 语句级回滚，其余保留）
//! lock         事务句柄、行标识 → 获得 / 等待 / 牺牲
//! ```
//!
//! **句柄不透明**（不变量 12）：`TxnHandle` 不暴露 undo 指针、事务表槽号、
//! ITL 槽——执行器只能经本模块的操作用它。
//!
//! **并发形态**：共享资源（日志写口、撤销链）各在 `Mutex` 后——V1.0
//! **每工作区 1 个 undo 段**（§4.6.3），同一工作区的全部会话共用一条撤销链与
//! 一个日志写口；`&Engine` 可跨线程共享。**挂起期间不持这两把锁**（等待-重试
//! 的每次尝试自取——否则持锁者进不来，等待即死锁）。
//!
//! **未接**：`查询终态`（REQ-API-014 的落点）——按提交序号 / 请求标签判定
//! 终态的持久化侧随 #63 细则与协议层对账落地；`commit` 已返回**提交序号**
//! （终态查询的输入）。

use std::sync::{Arc, Mutex};

use bicdb_common::seq::CommitSeq;
use bicdb_storage::buffer::{BufferKey, BufferPool};
use bicdb_storage::rowid::RowId;
use bicdb_storage::undo::{TxnId, TxnState, UndoChain, UndoError};
use bicdb_wal::group::GroupWriter;

use crate::lock::{Deadlock, TicketState, WaitGate, WaitTicket};
use crate::snapshot::{SnapshotHandle, SnapshotRegistry};
use crate::write::{self, StatementContext, StatementMark, Txn, TxnError, WaitPolicy};

/// **事务句柄**（不透明）：执行器只把它递回本模块的操作。
#[derive(Debug)]
pub struct TxnHandle {
    txn: Txn,
}

impl TxnHandle {
    /// 事务标识（**仅供诊断与日志关联**——不暴露槽号等存储内部构造）。
    #[must_use]
    pub fn id(&self) -> TxnId {
        self.txn.txn_id
    }

    /// 句柄是否仍活动（诊断）。
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.txn.state == TxnState::Active
    }
}

/// Result of a nonblocking row-owner wait probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowWaitStatus {
    /// No lock is transferred; rescan the original statement and predicate.
    Ready,
    /// Retain the request outside the SQL execution pool.
    Pending,
    /// Explicitly cancelled or superseded by another wait generation.
    Cancelled,
}

/// Owned enq registration. Contains no page pin, catalog cursor, undo/WAL lock
/// or OS thread. Dropping a disconnected request cancels only this generation.
#[derive(Debug)]
pub struct RowOwnerWait {
    gate: Arc<WaitGate>,
    ticket: WaitTicket,
    holder: TxnId,
    waiter: TxnId,
    row: RowId,
    next_check: std::time::Instant,
    deadline: Option<std::time::Instant>,
    period: std::time::Duration,
    needs_header: bool,
    terminal: Option<RowWaitStatus>,
}
impl RowOwnerWait {
    /// FIFO order within this engine's enq registry; not a global transaction ID.
    pub fn registration_order(&self) -> u64 {
        self.ticket.registration_order()
    }
    /// Suggested timer delay; owner completion can wake the request earlier.
    pub fn retry_after(&self) -> std::time::Duration {
        self.next_check
            .saturating_duration_since(std::time::Instant::now())
    }
    /// Header loading belongs to a worker, never to the control/lock poll loop.
    pub fn needs_header(&self) -> bool {
        self.needs_header
    }
    /// Cancel the registration without rolling back the transaction or its locks.
    pub fn cancel(&mut self) {
        self.gate.cancel_ticket(self.ticket);
        self.terminal = Some(RowWaitStatus::Cancelled);
    }
    fn finish(&mut self, outcome: RowWaitStatus) -> RowWaitStatus {
        self.gate.cancel_ticket(self.ticket);
        self.terminal = Some(outcome);
        outcome
    }
}
impl Drop for RowOwnerWait {
    fn drop(&mut self) {
        self.gate.cancel_ticket(self.ticket);
    }
}

/// **语句回滚点句柄**（不透明）。
#[derive(Debug, Clone, Copy)]
pub struct StatementHandle {
    mark: StatementMark,
}

/// **行锁结果**（REQ-ENG-002 的"获得 / 等待 / 牺牲"）。
///
/// "等待"在 [`Engine::lock_row`] 内部完成（登记 → 挂起 → 从头重试）；返回
/// `Ok` 即**已获得**。牺牲者经 [`TxnError::DeadlockVictim`] 上抛——**语句级
/// 回滚已由驱动完成**（事务其余部分与已持有的锁保留）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockStatus {
    /// 是否重入（本事务本就锁着这一行）。
    pub reentrant: bool,
}

struct GraphAuthorityReceipt {
    ws: [u8; 8],
    graph: u32,
    owner: Option<TxnId>,
    epoch: u64,
    header: Vec<u8>,
}

/// **事务引擎**（一个工作区一个实例；共享资源在内部串行化）。
pub struct Engine<'a, 'b, 'io, 'f> {
    pool: &'a BufferPool<'b>,
    /// WAL 写口（单写者纪律：会话经本锁串行取用）。
    wal: Mutex<GroupWriter<'io, 'f>>,
    /// 撤销链（V1.0 每工作区 1 段 ⇒ 全部会话共用）。
    chain: Mutex<UndoChain<'io, 'f>>,
    /// 快照注册表（最老快照封顶 undo 保留，§12.7）。
    snapshots: Mutex<SnapshotRegistry>,
    /// 行锁的等待门（§5.4.2）。
    gate: Arc<WaitGate>,
    undo_header: BufferKey,
    /// 当前提交序号（**已发布**的最大值；`begin` 的语句快照取它）。
    current_seq: Mutex<CommitSeq>,
    /// At most 128 complete graph validation receipts for this process/epoch.
    /// Any commit invalidates receipts from older epochs; owner scopes isolate
    /// uncommitted overlays. Cold/restarted processes always validate fully.
    graph_authorities: Mutex<Vec<GraphAuthorityReceipt>>,
    /// **下一个可用提交序号**（预约与提交共用这一个号源——
    /// 预约取走的号，提交时不再另取）。
    next_seq: Mutex<CommitSeq>,
    /// 等锁策略（挂起时长 / 死锁阈值 / 等待上限）。
    policy: WaitPolicy,
}

impl std::fmt::Debug for Engine<'_, '_, '_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field(
                "current_seq",
                &*self.current_seq.lock().unwrap_or_else(|e| e.into_inner()),
            )
            .field(
                "snapshots",
                &self
                    .snapshots
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .len(),
            )
            .finish()
    }
}

impl<'a, 'b, 'io, 'f> Engine<'a, 'b, 'io, 'f> {
    /// 建引擎：`initial_seq` = 启动时的提交序号水位（来自控制文件 / 恢复）。
    pub fn new(
        pool: &'a BufferPool<'b>,
        mut wal: GroupWriter<'io, 'f>,
        chain: UndoChain<'io, 'f>,
        initial_seq: CommitSeq,
    ) -> Self {
        wal.seed_commit_seq(initial_seq);
        let segment = chain.segment();
        let undo_header = BufferKey::new(
            segment.workspace_ref(),
            bicdb_storage::rowid::Rdba::from_parts(
                segment.file_id(),
                segment.logical_block(0).expect("undo header block"),
            )
            .expect("undo header address"),
        );
        Self {
            pool,
            wal: Mutex::new(wal),
            chain: Mutex::new(chain),
            snapshots: Mutex::new(SnapshotRegistry::new()),
            gate: Arc::new(WaitGate::new()),
            undo_header,
            current_seq: Mutex::new(initial_seq),
            graph_authorities: Mutex::new(Vec::new()),
            next_seq: Mutex::new(initial_seq),
            policy: WaitPolicy::default(),
        }
    }

    /// 等锁策略（会话级参数；默认见 [`WaitPolicy`]）。
    pub fn set_policy(&mut self, policy: WaitPolicy) {
        self.policy = policy;
    }

    /// 等待门（会话层取消 / 诊断用）。
    #[must_use]
    pub fn gate(&self) -> &WaitGate {
        &self.gate
    }

    /// **开始事务**（语句快照 = 当前提交序号水位）。
    ///
    /// **事务表满 ⇒ 先回收再试**（§4.6.5）：撤销段的事务表是定长槽表
    /// （256 槽），已提交的事务靠 [`write::reclaim`] 出链回收——本方法在
    /// "槽满"这一条路径上按**最老活跃快照**水位回收一次后重试。
    /// （长跑不回收 ⇒ 第 257 个事务起再也开不出来，这是 2026-10-06 审计
    /// 实测到的墙；回收口本身早已实现并有测试，缺的就是这条接线。）
    pub fn begin(&self) -> Result<TxnHandle, TxnError> {
        let snapshot = *self.current_seq.lock().unwrap_or_else(|e| e.into_inner());
        let mut wal = self.wal.lock().unwrap_or_else(|e| e.into_inner());
        let mut chain = self.chain.lock().unwrap_or_else(|e| e.into_inner());
        let txn = match write::begin(self.pool, &mut wal, &mut chain, snapshot) {
            Ok(t) => t,
            Err(TxnError::Undo(UndoError::NoFreeSlot)) => {
                let watermark = self.reclaim_watermark(snapshot);
                let _ = write::reclaim(self.pool, &mut wal, &mut chain, watermark)?;
                write::begin(self.pool, &mut wal, &mut chain, snapshot)?
            }
            Err(e) => return Err(e),
        };
        Ok(TxnHandle { txn })
    }

    /// **回收水位**：最老活跃快照（无活跃快照 ⇒ 调用方给的水位）。
    ///
    /// 语句快照的生命周期是"语句内"（`snapshot`/`release_snapshot`），
    /// V1.0 的会话在扫描期外不注册快照 ⇒ 绝大多数时候这里给出的是
    /// "全部已提交事务都可回收"。
    fn reclaim_watermark(&self, fallback: CommitSeq) -> Option<CommitSeq> {
        self.oldest_snapshot().or(Some(fallback))
    }

    /// **语句快照**（REQ-TXN-001：语句开始取、结束释放）——句柄进注册表，
    /// 它决定 undo 保留的水位（§12.7 最老快照）。
    pub fn snapshot(&self, txn: &TxnHandle) -> SnapshotHandle {
        let mut reg = self.snapshots.lock().unwrap_or_else(|e| e.into_inner());
        reg.register(txn.txn.snapshot)
    }

    /// 释放语句快照（语句结束时）。
    pub fn release_snapshot(&self, handle: SnapshotHandle) -> bool {
        let mut reg = self.snapshots.lock().unwrap_or_else(|e| e.into_inner());
        reg.release(handle)
    }

    /// **当前提交序号**（已发布的水位；会话的快照起点/诊断）。
    #[must_use]
    pub fn current_seq(&self) -> u64 {
        self.current_seq
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_raw()
    }

    /// Kernel-only receipt of a completely validated snapshot or an atomic
    /// publication derived from one. This is not an externally supplied flag.
    pub fn remember_graph_authority(
        &self,
        ws: [u8; 8],
        graph: u32,
        owner: Option<TxnId>,
        epoch: u64,
        header: &[u8],
    ) {
        if epoch != self.current_seq() || header.len() > 4096 {
            return;
        }
        let mut receipts = self
            .graph_authorities
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        receipts.retain(|r| !(r.ws == ws && r.graph == graph && r.owner == owner));
        if receipts.len() == 128 {
            receipts.remove(0);
        }
        receipts.push(GraphAuthorityReceipt {
            ws,
            graph,
            owner,
            epoch,
            header: header.to_vec(),
        });
    }
    /// Match a validated source only in this exact committed epoch and owner.
    pub fn graph_authority_validated(
        &self,
        ws: [u8; 8],
        graph: u32,
        owner: Option<TxnId>,
        epoch: u64,
        header: &[u8],
    ) -> bool {
        epoch == self.current_seq()
            && self
                .graph_authorities
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .any(|r| {
                    r.ws == ws
                        && r.graph == graph
                        && r.owner == owner
                        && r.epoch == epoch
                        && r.header == header
                })
    }
    /// **完全检查点**（§11.7 的"关闭工作区前"形态）：脏页按序全部写回 →
    /// 低水位一次推到当前日志位置 → 发布（控制文件 + 检查点记录）。
    ///
    /// **单进程关闭路径**：调用后实例即可干净退出（下次打开的重做范围为 0）。
    /// 与运行期增量检查点的区别只在"先刷页"这一步——发布次序两者相同。
    pub fn checkpoint_full(
        &self,
        workspace: [u8; 8],
    ) -> Result<bicdb_wal::checkpoint::CheckpointReport, bicdb_wal::checkpoint::CheckpointError>
    {
        self.checkpoint_full_impl(workspace, false)
    }

    /// Final shutdown checkpoint rejects any unresolved undo transaction slot.
    pub fn checkpoint_shutdown(
        &self,
        workspace: [u8; 8],
    ) -> Result<bicdb_wal::checkpoint::CheckpointReport, bicdb_wal::checkpoint::CheckpointError>
    {
        self.checkpoint_full_impl(workspace, true)
    }

    fn checkpoint_full_impl(
        &self,
        workspace: [u8; 8],
        shutdown: bool,
    ) -> Result<bicdb_wal::checkpoint::CheckpointReport, bicdb_wal::checkpoint::CheckpointError>
    {
        self.pool.ensure_workspace_writable(workspace)?;
        let oldest = self.oldest_snapshot();
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let mut wal = self.wal.lock().unwrap_or_else(|e| e.into_inner());
        let chain = self.chain.lock().unwrap_or_else(|e| e.into_inner());
        if workspace != chain.segment().workspace_ref() {
            return Err(bicdb_wal::checkpoint::CheckpointError::InvalidUndo(
                "workspace mismatch",
            ));
        }
        let current = wal.commit_watermark();
        let checkpoint = if shutdown {
            bicdb_wal::checkpoint::shutdown_checkpoint
        } else {
            bicdb_wal::checkpoint::transaction_checkpoint
        };
        checkpoint(
            &mut wal,
            self.pool,
            &chain,
            oldest.unwrap_or(current),
            timestamp,
        )
    }

    /// Incremental checkpoint; busy operation/page boundaries defer instead of
    /// blocking the instance coordinator. No data-page writeback is performed.
    pub fn checkpoint_incremental(
        &self,
        workspace: [u8; 8],
    ) -> Result<
        Option<bicdb_wal::checkpoint::CheckpointReport>,
        bicdb_wal::checkpoint::CheckpointError,
    > {
        if self.pool.workspace_fault(workspace).is_some() {
            return Ok(None);
        }
        use std::sync::TryLockError;
        let mut wal = match self.wal.try_lock() {
            Ok(wal) => wal,
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
        };
        let chain = match self.chain.try_lock() {
            Ok(chain) => chain,
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
        };
        if workspace != chain.segment().workspace_ref() {
            return Err(bicdb_wal::checkpoint::CheckpointError::InvalidUndo(
                "workspace mismatch",
            ));
        }
        let current = wal.commit_watermark();
        let oldest = self.oldest_snapshot().unwrap_or(current);
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0);
        bicdb_wal::checkpoint::transaction_checkpoint_incremental(
            &mut wal, self.pool, &chain, oldest, timestamp,
        )
    }

    /// Nonblocking maintenance for the independent undo worker. No checkpoint
    /// metadata or data-file writes are performed here.
    pub fn repair_undo_slots(
        &self,
        workspace: [u8; 8],
    ) -> Result<Option<bool>, bicdb_wal::checkpoint::CheckpointError> {
        if self.pool.workspace_fault(workspace).is_some() {
            return Ok(None);
        }
        match self.pool.load_checkpoint_headers(workspace) {
            Ok(()) => {}
            Err(bicdb_storage::buffer::BufferError::FreeBufferWait) => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        use std::sync::TryLockError;
        let mut wal = match self.wal.try_lock() {
            Ok(value) => value,
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
        };
        let chain = match self.chain.try_lock() {
            Ok(value) => value,
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
        };
        if workspace != chain.segment().workspace_ref() {
            return Err(bicdb_wal::checkpoint::CheckpointError::InvalidUndo(
                "workspace mismatch",
            ));
        }
        bicdb_wal::checkpoint::repair_transaction_slots(&mut wal, self.pool, &chain)
    }

    /// Shared durability endpoint for instance-level LGWR scheduling.
    pub fn wal_shared(&self) -> std::sync::Arc<bicdb_wal::group::WalShared<'io>> {
        self.wal.lock().unwrap_or_else(|e| e.into_inner()).shared()
    }

    /// **下一次日志切换会不会被挡**（CKPT 的"组满被迫"触发条件，§11.7）。
    ///
    /// 为什么要有这个探测：写者只有**真正写满当前组**时才会撞上 `Blocked`
    /// ——那时语句已经失败回滚了。调用方（会话）在**动手之前**问一句，
    /// 就地推一次检查点，写者就能一路写下去（Oracle 的"日志切换触发检查点"、
    /// PG 的 `max_wal_size` 触发检查点，都是这条）。
    #[must_use]
    pub fn log_switch_blocked(&self) -> bool {
        let wal = self.wal.lock().unwrap_or_else(|e| e.into_inner());
        matches!(
            wal.switch_blocked(),
            Some(bicdb_wal::group::SwitchBlocked::AwaitingCheckpoint)
        )
    }

    /// **最老活跃快照**（undo 回收的唯一输入；`None` = 无活跃快照）。
    pub fn oldest_snapshot(&self) -> Option<CommitSeq> {
        let reg = self.snapshots.lock().unwrap_or_else(|e| e.into_inner());
        if reg.is_empty() {
            None
        } else {
            let cur = *self.current_seq.lock().unwrap_or_else(|e| e.into_inner());
            Some(reg.oldest(cur))
        }
    }

    /// **取语句回滚点**（§4.6.6 ②）。
    pub fn statement_mark(&self, txn: &TxnHandle) -> Result<StatementHandle, TxnError> {
        let mut chain = self.chain.lock().unwrap_or_else(|e| e.into_inner());
        let mark = write::statement_mark(&mut chain, &txn.txn)?;
        Ok(StatementHandle { mark })
    }

    /// **语句级回滚**（死锁牺牲者 / 语句失败）：**不释锁、不回滚此前语句**。
    pub fn rollback_statement(
        &self,
        txn: &mut TxnHandle,
        handle: StatementHandle,
    ) -> Result<u64, TxnError> {
        let mut wal = self.wal.lock().unwrap_or_else(|e| e.into_inner());
        let mut chain = self.chain.lock().unwrap_or_else(|e| e.into_inner());
        write::rollback_to_mark(self.pool, &mut wal, &mut chain, &mut txn.txn, handle.mark)
    }

    /// **取号**（内部）：预约与提交共用——号只发一次。
    fn take_seq(&self) -> Result<CommitSeq, TxnError> {
        let mut next = self.next_seq.lock().unwrap_or_else(|e| e.into_inner());
        let seq = CommitSeq::from_raw(next.as_raw() + 1).ok_or(TxnError::StaleCache)?;
        *next = seq;
        Ok(seq)
    }

    /// **预约提交序号**（`目录详设` §5.2 ⑧ 的引擎侧增量）。
    ///
    /// 用途：`mtime` 必须在提交**前**写进字典行——提交序号在那里就要知道。
    /// 语义三条：
    /// - **原子取号**：从与提交同一个号源取，取走即占用（别的提交/预约**顺延**）；
    /// - **保证**：本事务此后 `commit` **一定**用这个号（回滚则作废，**跳号无害**）；
    /// - **一次一发**：重复预约 ⇒ [`TxnError::AlreadyReserved`]（不静默换号）。
    ///
    /// **使用约束（记档）**：预约号按序使用效果最好——预约与提交的**出现序**
    /// 应一致；倒序（先预约的晚提交）不会让已发布水位回退（发布取 `max`），
    /// 但那段时间内"序号已发布而事务未提交"的窗口里，新快照看见的是
    /// **已提交的那部分**（提交可见性以提交记录为准，与水位无关）。
    /// 单写者语义（DDL 路径）下这一条自然成立。
    pub fn reserve_commit_seq(&self, txn: &mut TxnHandle) -> Result<CommitSeq, TxnError> {
        if let Some(reserved) = txn.txn.reserved_seq {
            return Err(TxnError::AlreadyReserved { reserved });
        }
        let seq = self.take_seq()?;
        txn.txn.reserved_seq = Some(seq);
        Ok(seq)
    }

    /// **借出写上下文**（表访问服务/索引维护的入口）：池 + 日志写口 + 撤销链
    /// + 内层事务句柄，**一次给全**。
    ///
    /// 用途：DDL/表访问的写路径要同时用这四样（行写 `txn::write`、索引维护
    /// `bicdb-index` 的写口、建段/增长经池 + redo），而它们分别藏在引擎的
    /// 私有字段里（各自带锁）。开口而不是开字段：**三把内部锁在 `f` 期间被持有**
    /// （单写者语义下本就是串行段），`f` 内**不得再调引擎方法**（会自锁）。
    ///
    /// `f` 的返回值原样返回；`f` 内的错误由调用方自行处理（本方法不吞错）。
    pub fn with_write_context<R>(
        &self,
        handle: &mut TxnHandle,
        f: impl FnOnce(
            &BufferPool<'b>,
            &mut GroupWriter<'io, 'f>,
            &mut bicdb_storage::undo::UndoChain<'io, 'f>,
            &mut crate::write::Txn,
        ) -> R,
    ) -> R {
        let mut wal = self.wal.lock().unwrap_or_else(|e| e.into_inner());
        let mut chain = self.chain.lock().unwrap_or_else(|e| e.into_inner());
        f(self.pool, &mut wal, &mut chain, &mut handle.txn)
    }

    /// **借出读上下文**（扫描/CR 用）：池 + 撤销链（只读借用）。
    ///
    /// `f` 期间持有撤销链的锁（与写路径同一把）——**读路径不做长事务**：
    /// 扫描应当在 `f` 内完成（会话层的语句执行即此形态）。
    pub fn with_read_context<R>(
        &self,
        f: impl FnOnce(&BufferPool<'b>, &bicdb_storage::undo::UndoChain<'io, 'f>) -> R,
    ) -> R {
        let chain = self.chain.lock().unwrap_or_else(|e| e.into_inner());
        f(self.pool, &chain)
    }

    /// **提交**：提交记录入流 + 等它耐久（提交点）→ 发布新提交序号 →
    /// **唤醒等待者**（§5.4.2 ③ 的后半步）。返回提交序号。
    ///
    /// 序号来源：**预约过就用预约号**（§5.2 ⑧），否则现取（号源同一）。
    pub fn commit(&self, txn: &mut TxnHandle) -> Result<CommitSeq, TxnError> {
        let seq = match txn.txn.reserved_seq.take() {
            Some(reserved) => reserved,
            None => self.take_seq()?,
        };
        {
            let mut wal = self.wal.lock().unwrap_or_else(|e| e.into_inner());
            let mut chain = self.chain.lock().unwrap_or_else(|e| e.into_inner());
            write::commit(self.pool, &mut wal, &mut chain, &mut txn.txn, seq)?;
            // Publish while holding the writer lock: a checkpoint must not
            // discard a durable commit with a stale published watermark.
            let mut cur = self.current_seq.lock().unwrap_or_else(|e| e.into_inner());
            *cur = (*cur).max(seq);
        }
        self.gate.wake(txn.txn.txn_id);
        Ok(seq)
    }

    /// **回滚**：反向应用 undo、释放槽位；**唤醒其等待者**（不做锁移交）。
    pub fn rollback(&self, txn: &mut TxnHandle) -> Result<u64, TxnError> {
        let count = {
            let mut wal = self.wal.lock().unwrap_or_else(|e| e.into_inner());
            let mut chain = self.chain.lock().unwrap_or_else(|e| e.into_inner());
            write::rollback(self.pool, &mut wal, &mut chain, &mut txn.txn)?
        };
        // 预约号**随回滚作废**（§5.2 ⑧：跳号无害；不留给死句柄复用）。
        txn.txn.reserved_seq = None;
        let _ = self.gate.cancel(txn.txn.txn_id);
        self.gate.wake(txn.txn.txn_id);
        Ok(count)
    }

    /// **行锁**（REQ-ENG-002 的 `lock`）：获得 / 等待 / 牺牲。
    pub fn lock_row(
        &self,
        txn: &mut TxnHandle,
        block: BufferKey,
        row_no: u16,
    ) -> Result<LockStatus, TxnError> {
        let mark = self.statement_mark(txn)?;
        let mut ctx = LockCtx {
            engine: self,
            block,
            row_no,
        };
        let policy = self.policy;
        write::drive(
            &mut ctx,
            &mut txn.txn,
            mark.mark,
            &self.gate,
            &policy,
            now_ms,
            |c, t| c.attempt(t),
        )
    }

    /// Wait for the owner reported by a failed row-write attempt to finish.
    ///
    /// The caller must first roll the whole SQL statement back to its mark.
    /// Stable root ROWIDs survive migration. Waiting does not preserve an old
    /// materialized row image or target position: after owner release the
    /// caller must rescan and re-evaluate the original predicate.
    pub fn wait_for_row_owner(
        &self,
        txn: &TxnHandle,
        holder: TxnId,
        row: RowId,
    ) -> Result<(), TxnError> {
        let mut wait = self.enqueue_row_wait(txn, holder, row);
        loop {
            match self.poll_row_wait(&mut wait)? {
                RowWaitStatus::Ready => return Ok(()),
                RowWaitStatus::Cancelled => return Err(TxnError::LockWaitCancelled),
                RowWaitStatus::Pending => {
                    if wait.needs_header() {
                        self.load_row_wait_header(&mut wait)?;
                    }
                    wait.gate.park_ticket(wait.ticket, wait.retry_after());
                }
            }
        }
    }

    /// Register once after statement rollback. Owner release before or after
    /// registration is handled by a notification or the resident slot probe.
    pub fn enqueue_row_wait(&self, txn: &TxnHandle, holder: TxnId, row: RowId) -> RowOwnerWait {
        let now = std::time::Instant::now();
        let period = self
            .policy
            .park_timeout
            .max(std::time::Duration::from_millis(1));
        let deadline = self
            .policy
            .max_waits
            .map(|max| now.checked_add(period.saturating_mul(max)).unwrap_or(now));
        RowOwnerWait {
            gate: Arc::clone(&self.gate),
            ticket: self.gate.enqueue(txn.id(), holder, row, now_ms()),
            holder,
            waiter: txn.id(),
            row,
            next_check: now,
            deadline,
            period,
            needs_header: false,
            terminal: None,
        }
    }

    /// Probe without any WAL/undo mutex, disk I/O or blocking content latch.
    /// A pending result lets the instance scheduler release its SQL worker.
    pub fn poll_row_wait(&self, wait: &mut RowOwnerWait) -> Result<RowWaitStatus, TxnError> {
        if !Arc::ptr_eq(&self.gate, &wait.gate) {
            return Err(TxnError::UnboundUndoChain);
        }
        if let Some(status) = wait.terminal {
            return Ok(status);
        }
        let result = self.poll_row_wait_inner(wait);
        if result.is_err() {
            wait.cancel();
        }
        result
    }

    fn poll_row_wait_inner(&self, wait: &mut RowOwnerWait) -> Result<RowWaitStatus, TxnError> {
        self.pool
            .ensure_workspace_writable(self.undo_header.workspace)
            .map_err(TxnError::Pool)?;
        match self.gate.poll_ticket(wait.ticket) {
            TicketState::Woken => return Ok(wait.finish(RowWaitStatus::Ready)),
            TicketState::Cancelled => return Ok(wait.finish(RowWaitStatus::Cancelled)),
            TicketState::Pending => {}
        }
        let now = std::time::Instant::now();
        if now < wait.next_check {
            return Ok(RowWaitStatus::Pending);
        }
        wait.next_check = now + wait.period;
        let expired = wait.deadline.is_some_and(|deadline| now >= deadline);
        let Some(header) = self.pool.try_pin(self.undo_header) else {
            wait.needs_header = !self.pool.is_resident(self.undo_header);
            if expired {
                return Err(TxnError::LockTimeout {
                    holder: wait.holder,
                    row: wait.row,
                });
            }
            return Ok(RowWaitStatus::Pending);
        };
        wait.needs_header = false;
        let active = bicdb_storage::undo::find_slot(&header, wait.holder)?
            .is_some_and(|slot| matches!(slot.state, TxnState::Active | TxnState::PendingRollback));
        if !active {
            return Ok(wait.finish(RowWaitStatus::Ready));
        }
        let graph = self.gate.snapshot(now_ms());
        let deadlock = crate::lock::detect_deadlock_with_slots(
            &graph,
            self.policy.deadlock_threshold_ms,
            |txn| bicdb_storage::undo::find_slot(&header, txn),
        )?;
        if let Some(deadlock) = deadlock {
            if deadlock.victim == wait.waiter {
                return Err(TxnError::DeadlockVictim {
                    cycle: deadlock.cycle,
                });
            }
        }
        if expired {
            return Err(TxnError::LockTimeout {
                holder: wait.holder,
                row: wait.row,
            });
        }
        Ok(RowWaitStatus::Pending)
    }

    /// Explicit cache-miss preparation by a worker/synchronous adapter.
    pub fn load_row_wait_header(&self, wait: &mut RowOwnerWait) -> Result<(), TxnError> {
        if !Arc::ptr_eq(&self.gate, &wait.gate) {
            return Err(TxnError::UnboundUndoChain);
        }
        if wait.terminal.is_some() {
            return Ok(());
        }
        self.pool
            .ensure_workspace_writable(self.undo_header.workspace)
            .map_err(TxnError::Pool)?;
        drop(self.pool.pin(self.undo_header).map_err(TxnError::Pool)?);
        wait.needs_header = false;
        wait.next_check = std::time::Instant::now();
        Ok(())
    }

    /// 引擎内的 DML 入口（**过渡形态**：正式入口是"存储服务"（REQ-ENG-003），
    /// 本口供本 crate 的用例与最近的执行器切片使用）。
    ///
    /// 返回行号（新插入的行）。
    pub fn insert_row(
        &self,
        txn: &mut TxnHandle,
        block: BufferKey,
        row: &[u8],
        policy: &bicdb_storage::heap::InsertPolicy,
    ) -> Result<bicdb_storage::rowid::RowId, TxnError> {
        let mut wal = self.wal.lock().unwrap_or_else(|e| e.into_inner());
        let mut chain = self.chain.lock().unwrap_or_else(|e| e.into_inner());
        write::insert_row(
            self.pool,
            &mut wal,
            &mut chain,
            &mut txn.txn,
            block,
            row,
            policy,
        )
    }
}

/// 行锁的语句上下文（**每次尝试自取**日志/撤销链两把锁——挂起期间不持锁）。
struct LockCtx<'e, 'a, 'b, 'io, 'f> {
    engine: &'e Engine<'a, 'b, 'io, 'f>,
    block: BufferKey,
    row_no: u16,
}

impl<'a, 'b, 'io, 'f> StatementContext for LockCtx<'_, 'a, 'b, 'io, 'f> {
    type Item = LockStatus;

    fn attempt(&mut self, txn: &mut Txn) -> Result<LockStatus, TxnError> {
        let mut wal = self.engine.wal.lock().unwrap_or_else(|e| e.into_inner());
        let mut chain = self.engine.chain.lock().unwrap_or_else(|e| e.into_inner());
        // 锁路径没有表上下文 ⇒ 用**缺省插入策略**（ITL 上限 = 格式上限）；
        // 表级 `itl_max` 经写路径（insert/update/delete 的 `InsertPolicy`）生效。
        let policy = bicdb_storage::heap::InsertPolicy::in_place(0);
        let reentrant = crate::write::lock_row(
            self.engine.pool,
            &mut wal,
            &mut chain,
            txn,
            self.block,
            self.row_no,
            &policy,
        )?;
        Ok(LockStatus { reentrant })
    }

    fn detect(
        &mut self,
        graph: &crate::lock::WaitGraph,
        threshold_ms: u64,
    ) -> Result<Option<Deadlock>, TxnError> {
        // 图已冻结（门锁已还）；这里只做链查找（可能读盘）——**不持门锁**。
        let chain = self.engine.chain.lock().unwrap_or_else(|e| e.into_inner());
        Ok(crate::lock::detect_deadlock_from(
            graph,
            &chain,
            threshold_ms,
        )?)
    }

    fn rollback_to_mark(&mut self, txn: &mut Txn, mark: StatementMark) -> Result<u64, TxnError> {
        let mut wal = self.engine.wal.lock().unwrap_or_else(|e| e.into_inner());
        let mut chain = self.engine.chain.lock().unwrap_or_else(|e| e.into_inner());
        write::rollback_to_mark(self.engine.pool, &mut wal, &mut chain, txn, mark)
    }
}

/// 单调毫秒时钟（死锁阈值与等待时长；标准库）。
fn now_ms() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use bicdb_storage::buffer::{CacheConfig, SystemClock, WalGuard};
    use bicdb_storage::controlfile::{
        ArchiveMode, ArchiveRecord, ControlFile, RedoEntries, WorkspaceEntry,
    };
    use bicdb_storage::datafile::DataFile;
    use bicdb_storage::page::{Page, PageType};
    use bicdb_storage::pagefile;
    use bicdb_storage::rowid::Rdba;
    use bicdb_storage::undo::create_undo_segment;
    use bicdb_workspace::io::MemFileIo;
    use bicdb_workspace::WorkspaceId;
    use std::path::Path;

    const WS: [u8; 8] = [9u8; 8];
    const UNDO_F: &str = "/mem/undo1.dat";
    const DATA_F: &str = "/mem/data.dat";
    const WAL: &str = "/mem/wal";
    const A: &str = "/mem/cf_a";
    const B: &str = "/mem/cf_b";

    fn seq(v: u64) -> CommitSeq {
        CommitSeq::from_raw(v).unwrap()
    }

    struct FakeWal;
    impl WalGuard for FakeWal {
        fn durable_lsn(&self) -> bicdb_common::seq::Lsn {
            bicdb_common::seq::Lsn::from_raw(u64::MAX >> 16).unwrap()
        }
        fn ensure_durable(&self, _t: bicdb_common::seq::Lsn) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn row_bytes(payload: &[u8]) -> Vec<u8> {
        bicdb_storage::row::assemble_row(0, 1, &[false], &[], &[payload]).unwrap()
    }

    /// 引擎夹具（泄成 `'static`——生产上引擎随实例存在）。
    fn engine() -> (
        &'static Engine<'static, 'static, 'static, 'static>,
        BufferKey,
    ) {
        let (engine, key, _) = engine_with_io();
        (engine, key)
    }

    fn engine_with_io() -> (
        &'static Engine<'static, 'static, 'static, 'static>,
        BufferKey,
        &'static MemFileIo,
    ) {
        engine_with_policy(WaitPolicy::default())
    }

    fn engine_with_policy(
        policy: WaitPolicy,
    ) -> (
        &'static Engine<'static, 'static, 'static, 'static>,
        BufferKey,
        &'static MemFileIo,
    ) {
        let io: &'static MemFileIo = Box::leak(Box::new({
            let io = MemFileIo::new();
            io.add_dir("/mem");
            io.add_dir(WAL);
            io
        }));
        let undo_file: &'static mut DataFile<'static> = Box::leak(Box::new(
            DataFile::create(io, Path::new(UNDO_F), 1, 1, WS, 512).unwrap(),
        ));
        let undo_handle = undo_file.handle();
        let segment = create_undo_segment(undo_file, 2, 3, 4).unwrap();
        let data_handle = {
            let data_file = DataFile::create(io, Path::new(DATA_F), 3, 3, WS, 512).unwrap();
            let h = data_file.handle();
            let mut page = Page::new(PageType::HeapTable, WS, 3, 1);
            pagefile::write_page(io, h, 1, &mut page).unwrap();
            Box::leak(Box::new(data_file));
            h
        };
        let pool: &'static BufferPool<'static> = Box::leak(Box::new(
            BufferPool::with_config(
                io,
                8,
                move |_ws, r| match r.file_id() {
                    1 => Some((undo_handle, r.block_id())),
                    3 => Some((data_handle, r.block_id())),
                    _ => None,
                },
                FakeWal,
                SystemClock,
                CacheConfig::for_capacity(8),
            )
            .unwrap(),
        ));
        let cf: &'static mut ControlFile<'static> = Box::leak(Box::new(
            ControlFile::format(
                io,
                Path::new(A),
                Path::new(B),
                &WorkspaceEntry {
                    workspace_id: WorkspaceId::from_raw(1).unwrap(),
                    created_at: 0,
                    derived_from: None,
                    derived_at_seq: seq(0),
                },
                &RedoEntries::new(2, 1).unwrap(),
                &ArchiveRecord::new(ArchiveMode::NoArchive),
            )
            .unwrap(),
        ));
        let spec = bicdb_wal::group::GroupSpec::new(2, 1, 64).unwrap();
        let wal = GroupWriter::create(
            io,
            cf,
            Path::new(WAL),
            spec,
            bicdb_common::seq::Lsn::from_raw(0).unwrap(),
        )
        .unwrap();
        let mut engine = Engine::new(pool, wal, UndoChain::open(segment).with_pool(pool), seq(0));
        engine.set_policy(policy);
        let engine = Box::leak(Box::new(engine));
        (
            engine,
            BufferKey::new(WS, Rdba::from_parts(3, 1).unwrap()),
            io,
        )
    }

    use bicdb_storage::heap::InsertPolicy;

    #[test]
    fn incremental_checkpoint_recovery_preserves_winner_and_rolls_back_loser() {
        let (engine, key, io) = engine_with_io();
        for file_id in [1, 3] {
            engine.pool.register_checkpoint_file(BufferKey::new(
                WS,
                Rdba::from_parts(file_id, 0).unwrap(),
            ));
        }
        engine.pool.load_checkpoint_headers(WS).unwrap();
        let mut winner = engine.begin().unwrap();
        let kept = engine
            .insert_row(
                &mut winner,
                key,
                &row_bytes(b"kept"),
                &InsertPolicy::in_place(0),
            )
            .unwrap();
        engine.commit(&mut winner).unwrap();
        let mut loser = engine.begin().unwrap();
        let removed = engine
            .insert_row(
                &mut loser,
                key,
                &row_bytes(b"loser"),
                &InsertPolicy::in_place(0),
            )
            .unwrap();
        {
            let mut wal = engine.wal.lock().unwrap();
            let end = wal.appended_lsn();
            wal.flush(end).unwrap();
        }
        let checkpoint = engine.checkpoint_incremental(WS).unwrap().unwrap();
        assert_eq!(checkpoint.pages_written, 0);
        assert!(engine.pool.dirty_len(WS) > 0);
        for file in [UNDO_F, DATA_F] {
            let head = DataFile::open(io, Path::new(file)).unwrap();
            assert_eq!(head.file_scn(), checkpoint.progress.checkpoint_lsn.as_raw());
            assert_eq!(
                head.checkpoint_commit_scn(),
                checkpoint.progress.checkpoint_commit_seq.as_raw()
            );
            head.close().unwrap();
        }
        // Discard all live cache/transaction state; recovery reads only the
        // persisted in-memory filesystem, never the old BufferPool.
        let mut undo = DataFile::open(io, Path::new(UNDO_F)).unwrap();
        let undo_handle = undo.handle();
        let data = DataFile::open(io, Path::new(DATA_F)).unwrap();
        let data_handle = data.handle();
        let mut chain = UndoChain::open(
            bicdb_storage::segment::Segment::open(
                &mut undo,
                engine.chain.lock().unwrap().segment().page0_block(),
            )
            .unwrap(),
        );
        let mut control = ControlFile::open(io, Path::new(A), Path::new(B)).unwrap();
        assert_eq!(control.checkpoint_progress().unwrap(), checkpoint.progress);
        let spec = bicdb_wal::group::GroupSpec::new(2, 1, 64).unwrap();
        let groups = bicdb_wal::group::online_groups(io, &control, Path::new(WAL), spec).unwrap();
        let mut writer = GroupWriter::open(io, &mut control, Path::new(WAL), spec).unwrap();
        let mut resolve = |rdba: Rdba| match rdba.file_id() {
            1 => Some((undo_handle, rdba.block_id())),
            3 => Some((data_handle, rdba.block_id())),
            _ => None,
        };
        let recovered = bicdb_wal::recovery::recover(
            io,
            &groups,
            checkpoint.progress.checkpoint_lsn,
            &mut chain,
            &mut writer,
            &mut resolve,
        )
        .unwrap();
        assert_eq!(recovered.undo.txns_rolled_back, 1);
        let page = pagefile::read_page_verified(io, data_handle, 1).unwrap();
        let bytes = bicdb_storage::heap::row(&page, kept.row_id()).expect("committed row survives");
        assert!(bytes.ends_with(b"kept"));
        assert!(bicdb_storage::heap::row(&page, removed.row_id()).is_none());
    }

    #[test]
    fn cold_wait_probe_never_loads_a_header_or_allocates_cache_frames() {
        let (engine, key) = engine();
        let mut owner = engine.begin().unwrap();
        let mut waiter = engine.begin().unwrap();
        engine.checkpoint_full(WS).unwrap();
        engine.pool.drop_clean_frames(0).unwrap();
        assert_eq!(engine.pool.resident(), 0);
        let row =
            bicdb_storage::rowid::RowId::from_parts(key.rdba.file_id(), key.rdba.block_id(), 1)
                .unwrap();
        let mut ticket = engine.enqueue_row_wait(&waiter, owner.id(), row);
        assert_eq!(
            engine.poll_row_wait(&mut ticket).unwrap(),
            RowWaitStatus::Pending
        );
        assert!(ticket.needs_header());
        assert_eq!(
            engine.pool.resident(),
            0,
            "reactor poll must not read from the file"
        );
        engine.load_row_wait_header(&mut ticket).unwrap();
        assert_eq!(
            engine.poll_row_wait(&mut ticket).unwrap(),
            RowWaitStatus::Pending
        );
        engine.commit(&mut owner).unwrap();
        assert_eq!(
            engine.poll_row_wait(&mut ticket).unwrap(),
            RowWaitStatus::Ready
        );
        engine.rollback(&mut waiter).unwrap();
    }

    #[test]
    fn nonblocking_wait_timeout_and_deadlock_cancel_their_generation() {
        let (engine, key, _) = engine_with_policy(WaitPolicy {
            max_waits: Some(0),
            ..WaitPolicy::default()
        });
        let mut owner = engine.begin().unwrap();
        let mut waiter = engine.begin().unwrap();
        let row =
            bicdb_storage::rowid::RowId::from_parts(key.rdba.file_id(), key.rdba.block_id(), 1)
                .unwrap();
        let mut ticket = engine.enqueue_row_wait(&waiter, owner.id(), row);
        let held = engine.pool.pin(engine.undo_header).unwrap();
        assert!(matches!(
            engine.poll_row_wait(&mut ticket),
            Err(TxnError::LockTimeout { .. })
        ));
        drop(held);
        assert!(engine.gate.waiters_of(owner.id()).is_empty());
        engine.rollback(&mut owner).unwrap();
        engine.rollback(&mut waiter).unwrap();

        let (engine, key, _) = engine_with_policy(WaitPolicy {
            deadlock_threshold_ms: 0,
            ..WaitPolicy::default()
        });
        let mut a = engine.begin().unwrap();
        let mut b = engine.begin().unwrap();
        let row =
            bicdb_storage::rowid::RowId::from_parts(key.rdba.file_id(), key.rdba.block_id(), 1)
                .unwrap();
        let mut a_wait = engine.enqueue_row_wait(&a, b.id(), row);
        let b_wait = engine.enqueue_row_wait(&b, a.id(), row);
        engine.with_read_context(|_, _| {
            assert!(matches!(
                engine.poll_row_wait(&mut a_wait),
                Err(TxnError::DeadlockVictim { .. })
            ));
        });
        assert!(engine.gate.waiters_of(b.id()).is_empty());
        drop(b_wait);
        engine.rollback(&mut a).unwrap();
        engine.rollback(&mut b).unwrap();
    }

    #[test]
    fn row_wait_poll_does_not_take_wal_or_undo_mutexes_and_release_is_not_lost() {
        let (engine, key) = engine();
        let mut holder = engine.begin().unwrap();
        let mut waiter = engine.begin().unwrap();
        let row =
            bicdb_storage::rowid::RowId::from_parts(key.rdba.file_id(), key.rdba.block_id(), 1)
                .unwrap();
        let mut ticket = engine.enqueue_row_wait(&waiter, holder.id(), row);
        engine.with_write_context(&mut holder, |_, _, _, _| {
            assert_eq!(
                engine.poll_row_wait(&mut ticket).unwrap(),
                RowWaitStatus::Pending
            );
        });
        // Owner completion before reactor polling must be remembered.
        engine.commit(&mut holder).unwrap();
        assert_eq!(
            engine.poll_row_wait(&mut ticket).unwrap(),
            RowWaitStatus::Ready
        );
        assert!(engine.gate.waiters_of(holder.id()).is_empty());
        assert_eq!(engine.gate.pending_wakes(), 0);
        engine.rollback(&mut waiter).unwrap();
    }

    #[test]
    fn stale_wait_drop_cannot_cancel_new_owner_and_foreign_engine_cannot_poll() {
        let (other_engine, _) = engine();
        let (engine, key) = engine();
        let mut first = engine.begin().unwrap();
        let mut second = engine.begin().unwrap();
        let mut waiter = engine.begin().unwrap();
        let row =
            bicdb_storage::rowid::RowId::from_parts(key.rdba.file_id(), key.rdba.block_id(), 1)
                .unwrap();
        let old = engine.enqueue_row_wait(&waiter, first.id(), row);
        let mut current = engine.enqueue_row_wait(&waiter, second.id(), row);
        drop(old);
        assert_eq!(engine.gate.waiters_of(second.id()).len(), 1);
        assert!(other_engine.poll_row_wait(&mut current).is_err());
        assert_eq!(
            engine.poll_row_wait(&mut current).unwrap(),
            RowWaitStatus::Pending
        );
        engine.rollback(&mut first).unwrap();
        engine.rollback(&mut second).unwrap();
        assert_eq!(
            engine.poll_row_wait(&mut current).unwrap(),
            RowWaitStatus::Ready
        );
        engine.rollback(&mut waiter).unwrap();
    }

    #[test]
    fn dropped_wait_cancels_registration_without_ending_transaction() {
        let (engine, key) = engine();
        let mut owner = engine.begin().unwrap();
        let mut waiter = engine.begin().unwrap();
        let row =
            bicdb_storage::rowid::RowId::from_parts(key.rdba.file_id(), key.rdba.block_id(), 1)
                .unwrap();
        let ticket = engine.enqueue_row_wait(&waiter, owner.id(), row);
        drop(ticket);
        assert!(engine.gate.waiters_of(owner.id()).is_empty());
        assert!(waiter.is_active());
        engine.rollback(&mut owner).unwrap();
        engine.rollback(&mut waiter).unwrap();
    }

    #[test]
    fn quarantined_workspace_rejects_new_writes_and_commit_but_allows_rollback() {
        let (engine, key) = engine();
        let mut txn = engine.begin().unwrap();
        engine
            .insert_row(
                &mut txn,
                key,
                &row_bytes(b"pending"),
                &InsertPolicy::in_place(0),
            )
            .unwrap();
        engine
            .pool
            .quarantine_workspace(WS, "synthetic data I/O failure".into());
        assert!(engine.begin().is_err());
        assert!(engine
            .insert_row(
                &mut txn,
                key,
                &row_bytes(b"blocked"),
                &InsertPolicy::in_place(0)
            )
            .is_err());
        assert!(engine.commit(&mut txn).is_err());
        assert!(engine.checkpoint_full(WS).is_err());
        assert!(engine.checkpoint_incremental(WS).unwrap().is_none());
        engine.rollback(&mut txn).unwrap();
        assert!(
            engine.begin().is_err(),
            "rollback must not clear quarantine"
        );
    }

    #[test]
    fn idle_incremental_ticks_do_not_generate_checkpoint_redo() {
        let (engine, _) = engine();
        engine.checkpoint_full(WS).unwrap();
        let end = {
            let mut wal = engine.wal.lock().unwrap();
            let end = wal.appended_lsn();
            wal.flush(end).unwrap();
            end
        };
        for _ in 0..10 {
            assert!(engine.checkpoint_incremental(WS).unwrap().is_none());
        }
        assert_eq!(engine.wal.lock().unwrap().appended_lsn(), end);
    }

    #[test]
    fn clean_shutdown_rejects_unfinished_slots_without_publishing_checkpoint() {
        let (engine, key) = engine();
        for file_id in [1, 3] {
            engine.pool.register_checkpoint_file(BufferKey::new(
                WS,
                Rdba::from_parts(file_id, 0).unwrap(),
            ));
        }
        engine.pool.load_checkpoint_headers(WS).unwrap();
        let mut txn = engine.begin().unwrap();
        engine
            .insert_row(
                &mut txn,
                key,
                &row_bytes(b"unfinished"),
                &InsertPolicy::in_place(0),
            )
            .unwrap();
        let before = {
            let wal = engine.wal.lock().unwrap();
            (wal.appended_lsn(), wal.checkpoint_progress().unwrap())
        };
        let slot_no = u16::from(txn.id().slot());
        for state in [
            bicdb_storage::undo::TxnState::Active,
            bicdb_storage::undo::TxnState::PendingRollback,
        ] {
            {
                let mut header = engine.pool.pin(engine.undo_header).unwrap();
                let mut slot = bicdb_storage::undo::read_slot(&header, slot_no).unwrap();
                slot.state = state;
                bicdb_storage::undo::write_slot(&mut header, slot_no, &slot).unwrap();
                header.mark_dirty(bicdb_common::seq::Lsn::from_raw(0).unwrap());
            }
            assert!(matches!(engine.checkpoint_shutdown(WS),
                Err(bicdb_wal::checkpoint::CheckpointError::OutstandingTransaction { slot, state: actual })
                if slot == slot_no && actual == state));
            let wal = engine.wal.lock().unwrap();
            assert_eq!(wal.appended_lsn(), before.0);
            assert_eq!(wal.checkpoint_progress().unwrap(), before.1);
            assert_ne!(engine.pool.dirty_len(WS), 0);
        }
        engine.rollback(&mut txn).unwrap();
        engine.checkpoint_shutdown(WS).unwrap();
        assert_eq!(engine.pool.dirty_len(WS), 0);
    }

    #[test]
    fn clean_shutdown_accepts_durable_known_commit_with_stale_active_slot() {
        let (engine, key) = engine();
        for file_id in [1, 3] {
            engine.pool.register_checkpoint_file(BufferKey::new(
                WS,
                Rdba::from_parts(file_id, 0).unwrap(),
            ));
        }
        engine.pool.load_checkpoint_headers(WS).unwrap();
        let mut txn = engine.begin().unwrap();
        engine
            .insert_row(
                &mut txn,
                key,
                &row_bytes(b"committed"),
                &InsertPolicy::in_place(0),
            )
            .unwrap();
        engine.commit(&mut txn).unwrap();
        let slot_no = u16::from(txn.id().slot());
        {
            let mut header = engine.pool.pin(engine.undo_header).unwrap();
            let mut slot = bicdb_storage::undo::read_slot(&header, slot_no).unwrap();
            slot.state = bicdb_storage::undo::TxnState::Active;
            bicdb_storage::undo::write_slot(&mut header, slot_no, &slot).unwrap();
            header.mark_dirty(bicdb_common::seq::Lsn::from_raw(0).unwrap());
        }
        engine.checkpoint_shutdown(WS).unwrap();
        let header = engine.pool.pin(engine.undo_header).unwrap();
        assert_eq!(
            bicdb_storage::undo::read_slot(&header, slot_no)
                .unwrap()
                .state,
            bicdb_storage::undo::TxnState::Committed
        );
        assert_eq!(engine.pool.dirty_len(WS), 0);
    }

    #[test]
    fn checkpoint_does_not_repair_undo_slots_but_independent_maintenance_does() {
        let (engine, key) = engine();
        let mut txn = engine.begin().unwrap();
        engine
            .insert_row(
                &mut txn,
                key,
                &row_bytes(b"committed"),
                &InsertPolicy::in_place(0),
            )
            .unwrap();
        let id = txn.id();
        engine.commit(&mut txn).unwrap();
        let undo_key = {
            let chain = engine.chain.lock().unwrap();
            BufferKey::new(
                WS,
                Rdba::from_parts(
                    chain.segment().file_id(),
                    chain.segment().logical_block(0).unwrap(),
                )
                .unwrap(),
            )
        };
        let before = {
            let mut guard = engine.pool.pin(undo_key).unwrap();
            let mut slot = bicdb_storage::undo::read_slot(&guard, u16::from(id.slot())).unwrap();
            slot.state = bicdb_storage::undo::TxnState::Active;
            bicdb_storage::undo::write_slot(&mut guard, u16::from(id.slot()), &slot).unwrap();
            guard.mark_dirty(bicdb_common::seq::Lsn::from_raw(0).unwrap());
            *guard.as_bytes()
        };
        {
            let mut wal = engine.wal.lock().unwrap();
            let end = wal.appended_lsn();
            wal.flush(end).unwrap();
        }
        let _ = engine.checkpoint_incremental(WS).unwrap();
        assert_eq!(*engine.pool.pin(undo_key).unwrap().as_bytes(), before);
        assert_eq!(engine.repair_undo_slots(WS).unwrap(), Some(true));
        let slot = bicdb_storage::undo::read_slot(
            &engine.pool.pin(undo_key).unwrap(),
            u16::from(id.slot()),
        )
        .unwrap();
        assert_eq!(slot.state, bicdb_storage::undo::TxnState::Committed);
        assert!(engine.pool.is_dirty(undo_key));
        assert_eq!(engine.repair_undo_slots(WS).unwrap(), Some(false));
        engine.with_write_context(&mut txn, |_, _, _, _| {
            assert_eq!(engine.repair_undo_slots(WS).unwrap(), None);
        });
    }

    #[test]
    fn incremental_checkpoint_defers_busy_operation_boundaries() {
        let (engine, _) = engine();
        let mut txn = engine.begin().unwrap();
        engine.with_write_context(&mut txn, |_, _, _, _| {
            assert!(engine.checkpoint_incremental(WS).unwrap().is_none());
        });
        engine.with_read_context(|_, _| {
            assert!(engine.checkpoint_incremental(WS).unwrap().is_none());
        });
        engine.rollback(&mut txn).unwrap();
    }

    #[test]
    fn incremental_checkpoint_keeps_busy_dirty_pages_and_advances_after_dbwr() {
        let (engine, key) = engine();
        let mut txn = engine.begin().unwrap();
        engine
            .insert_row(
                &mut txn,
                key,
                &row_bytes(b"durable"),
                &InsertPolicy::in_place(0),
            )
            .unwrap();
        engine.commit(&mut txn).unwrap();
        {
            let mut wal = engine.wal.lock().unwrap();
            let end = wal.appended_lsn();
            wal.flush(end).unwrap();
        }
        let held = engine.pool.pin(key).unwrap();
        let dirty = engine.pool.dirty_len(WS);
        let report = engine
            .checkpoint_incremental(WS)
            .unwrap()
            .expect("publish partial progress");
        assert_eq!(report.pages_written, 0);
        assert_eq!(engine.pool.dirty_len(WS), dirty);
        assert!(report.progress.checkpoint_lsn <= engine.pool.low_water(WS).unwrap());
        drop(held);
        engine.pool.flush_workspace(WS).unwrap();
        let next = engine
            .checkpoint_incremental(WS)
            .unwrap()
            .expect("publish DBWR progress");
        assert!(next.progress.checkpoint_lsn > report.progress.checkpoint_lsn);
        assert_eq!(next.progress.checkpoint_commit_seq, seq(1));
        assert_eq!(engine.pool.dirty_len(WS), 0);
    }

    #[test]
    fn reserved_commit_seq_is_honored_and_blocks_later_commits() {
        let (engine, key) = engine();
        // A 预约 1；B（未预约）提交 ⇒ 必须**顺延**到 2（号源同一个）。
        let mut a = engine.begin().unwrap();
        let ra = engine.reserve_commit_seq(&mut a).unwrap();
        assert_eq!(ra, seq(1), "首次预约取 1");
        assert!(
            matches!(
                engine.reserve_commit_seq(&mut a),
                Err(TxnError::AlreadyReserved { reserved }) if reserved == seq(1)
            ),
            "重复预约具名拒绝"
        );
        let mut b = engine.begin().unwrap();
        engine
            .insert_row(&mut b, key, &row_bytes(b"b"), &InsertPolicy::in_place(0))
            .unwrap();
        assert_eq!(engine.commit(&mut b).unwrap(), seq(2), "未预约者顺延");
        // A 用预约号提交（即便晚于 B）。
        engine
            .insert_row(&mut a, key, &row_bytes(b"a"), &InsertPolicy::in_place(0))
            .unwrap();
        assert_eq!(engine.commit(&mut a).unwrap(), seq(1), "预约号被履行");
        // 之后的新事务继续在号源之后取号。
        let mut c = engine.begin().unwrap();
        assert_eq!(engine.commit(&mut c).unwrap(), seq(3));
    }

    #[test]
    fn a_rolled_back_reservation_burns_its_number() {
        let (engine, _key) = engine();
        let mut a = engine.begin().unwrap();
        assert_eq!(engine.reserve_commit_seq(&mut a).unwrap(), seq(1));
        engine.rollback(&mut a).unwrap();
        // 跳号：下一个提交拿 2（**跳号无害**——序号空间的既有口径）。
        let mut b = engine.begin().unwrap();
        assert_eq!(engine.commit(&mut b).unwrap(), seq(2));
        // 回滚后句柄的预约已作废：同号不复用、也不再被 AlreadyReserved 拦。
        let mut c = engine.begin().unwrap();
        assert_eq!(engine.reserve_commit_seq(&mut c).unwrap(), seq(3));
    }

    #[test]
    fn begin_commit_rollback_and_snapshots_through_the_engine() {
        let (engine, key) = engine();
        // 提交：序号推进 + 快照水位跟随。
        let mut t1 = engine.begin().unwrap();
        let rid = engine
            .insert_row(&mut t1, key, &row_bytes(b"one"), &InsertPolicy::in_place(0))
            .unwrap();
        assert!(t1.is_active());
        let seq1 = engine.commit(&mut t1).unwrap();
        assert_eq!(seq1, seq(1));
        assert!(!t1.is_active(), "提交后句柄不再活动");

        // 语句快照：注册/释放 + 最老快照。
        let t2 = engine.begin().unwrap();
        assert_eq!(engine.oldest_snapshot(), None);
        let s1 = engine.snapshot(&t2);
        let s2 = engine.snapshot(&t2);
        assert_eq!(engine.oldest_snapshot(), Some(seq(1)));
        assert!(engine.release_snapshot(s1));
        assert!(engine.release_snapshot(s2));
        assert_eq!(engine.oldest_snapshot(), None);

        // 回滚：撤销插入，行消失。
        let mut t3 = engine.begin().unwrap();
        let rid3 = engine
            .insert_row(
                &mut t3,
                key,
                &row_bytes(b"three"),
                &InsertPolicy::in_place(0),
            )
            .unwrap();
        let undone = engine.rollback(&mut t3).unwrap();
        assert!(undone >= 1, "插入被撤销（ITL 占用 + 插入 = 2 条记录）");
        assert!(!t3.is_active());
        let _ = (rid, rid3);
    }

    #[test]
    fn statement_handles_roll_back_only_the_statement() {
        let (engine, key) = engine();
        let mut t1 = engine.begin().unwrap();
        // 语句 1（保留）。
        let a = engine
            .insert_row(&mut t1, key, &row_bytes(b"AAA"), &InsertPolicy::in_place(0))
            .unwrap();
        // 语句 2：取回滚点 → 插入 → 回滚该语句。
        let mark = engine.statement_mark(&t1).unwrap();
        let b = engine
            .insert_row(&mut t1, key, &row_bytes(b"BBB"), &InsertPolicy::in_place(0))
            .unwrap();
        let undone = engine.rollback_statement(&mut t1, mark).unwrap();
        assert_eq!(undone, 1);
        assert!(t1.is_active(), "语句回滚 ≠ 事务回滚");
        let _ = (a, b);
        engine.rollback(&mut t1).unwrap();
    }

    #[test]
    fn lock_row_conflicts_across_sessions_then_succeeds_after_commit() {
        // 两会话（同引擎、同段）争同一行：第二者等待 → 唤醒 → 获得。
        let (engine, key) = engine();
        let mut t1 = engine.begin().unwrap();
        let rid = engine
            .insert_row(&mut t1, key, &row_bytes(b"row"), &InsertPolicy::in_place(0))
            .unwrap();
        engine.commit(&mut t1).unwrap();

        let mut holder = engine.begin().unwrap();
        let status = engine.lock_row(&mut holder, key, rid.row_id()).unwrap();
        assert!(!status.reentrant);

        let (tx, rx) = std::sync::mpsc::channel::<LockStatus>();
        std::thread::scope(|scope| {
            let waiter = scope.spawn(move || {
                let mut t2 = engine.begin().unwrap();
                let s = engine.lock_row(&mut t2, key, rid.row_id()).unwrap();
                tx.send(s).unwrap();
                engine.commit(&mut t2).unwrap();
            });
            // 等第二会话登记在册，再让持锁者提交（唤醒）。
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while engine.gate().waiters_of(holder.id()).is_empty()
                && std::time::Instant::now() < deadline
            {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            assert!(
                !engine.gate().waiters_of(holder.id()).is_empty(),
                "第二会话在等"
            );
            engine.commit(&mut holder).unwrap();
            let status = rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
            assert!(!status.reentrant, "等到后获得（非重入）");
            waiter.join().unwrap();
        });
    }

    #[test]
    fn reentrant_lock_is_reported_through_the_engine() {
        let (engine, key) = engine();
        let mut t1 = engine.begin().unwrap();
        let rid = engine
            .insert_row(&mut t1, key, &row_bytes(b"row"), &InsertPolicy::in_place(0))
            .unwrap();
        engine.commit(&mut t1).unwrap();
        let mut t2 = engine.begin().unwrap();
        engine.lock_row(&mut t2, key, rid.row_id()).unwrap();
        let again = engine.lock_row(&mut t2, key, rid.row_id()).unwrap();
        assert!(again.reentrant, "同一事务二次加锁 = 重入");
        engine.rollback(&mut t2).unwrap();
    }
}
