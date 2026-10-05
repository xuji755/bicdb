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

use std::sync::Mutex;

use bicdb_common::seq::CommitSeq;
use bicdb_storage::buffer::{BufferKey, BufferPool};
use bicdb_storage::undo::{TxnId, TxnState, UndoChain};
use bicdb_wal::group::GroupWriter;

use crate::lock::{Deadlock, WaitGate};
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
    gate: WaitGate,
    /// 当前提交序号（已发布的最大值；`begin` 的语句快照取它）。
    current_seq: Mutex<CommitSeq>,
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
        wal: GroupWriter<'io, 'f>,
        chain: UndoChain<'io, 'f>,
        initial_seq: CommitSeq,
    ) -> Self {
        Self {
            pool,
            wal: Mutex::new(wal),
            chain: Mutex::new(chain),
            snapshots: Mutex::new(SnapshotRegistry::new()),
            gate: WaitGate::new(),
            current_seq: Mutex::new(initial_seq),
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
    pub fn begin(&self) -> Result<TxnHandle, TxnError> {
        let snapshot = *self.current_seq.lock().unwrap_or_else(|e| e.into_inner());
        let mut wal = self.wal.lock().unwrap_or_else(|e| e.into_inner());
        let mut chain = self.chain.lock().unwrap_or_else(|e| e.into_inner());
        let txn = write::begin(self.pool, &mut wal, &mut chain, snapshot)?;
        Ok(TxnHandle { txn })
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

    /// **提交**：提交记录入流 + 等它耐久（提交点）→ 发布新提交序号 →
    /// **唤醒等待者**（§5.4.2 ③ 的后半步）。返回提交序号。
    pub fn commit(&self, txn: &mut TxnHandle) -> Result<CommitSeq, TxnError> {
        let seq = {
            let cur = *self.current_seq.lock().unwrap_or_else(|e| e.into_inner());
            CommitSeq::from_raw(cur.as_raw() + 1).ok_or(TxnError::StaleCache)?
        };
        {
            let mut wal = self.wal.lock().unwrap_or_else(|e| e.into_inner());
            let mut chain = self.chain.lock().unwrap_or_else(|e| e.into_inner());
            write::commit(self.pool, &mut wal, &mut chain, &mut txn.txn, seq)?;
        }
        *self.current_seq.lock().unwrap_or_else(|e| e.into_inner()) = seq;
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
        let reentrant = crate::write::lock_row(
            self.engine.pool,
            &mut wal,
            &mut chain,
            txn,
            self.block,
            self.row_no,
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
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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
        let engine = Box::leak(Box::new(Engine::new(
            pool,
            wal,
            UndoChain::open(segment),
            seq(0),
        )));
        (engine, BufferKey::new(WS, Rdba::from_parts(3, 1).unwrap()))
    }

    use bicdb_storage::heap::InsertPolicy;

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
