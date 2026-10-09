//! 检查点（§11.7）：**CKPT 角色——只把位置写进控制文件，不写数据块**。
//!
//! ```text
//! 写线程（DBWR）           检查点（CKPT）
//!   按序写脏页   ──────▶   低水位前移（缓冲池的脏链头，§5.10）
//!                          ① 检查点记录入流（0x02，主段 24B）并刷盘
//!                          ② 发布：CF 检查点进度 + ACTIVE→INACTIVE 降级
//!                             （**同一区间**，§11.9 的发布纪律）
//! ```
//!
//! # 两件事分开（§11.7）
//!
//! - **写数据块**是写线程的事（本模块不写页；`full_checkpoint` 里的写回是
//!   显式调用缓冲池的 `flush_workspace`，语义上仍属 DBWR 角色）；
//! - **检查点只发布结论**——低水位经缓冲池的脏链计算：**有脏页 ⇒ 最老的
//!   首次变脏 LSN；无脏页 ⇒ 当前日志位置**（该点之前的修改已全部落盘）。
//!
//! # 发布纪律（§11.9）
//!
//! 降级**绝不能先于低水位独立落盘**——否则崩溃后"可复用声明"会早于
//! "恢复起点"，被复用的组会剪断日志链。所以进度与组状态走
//! `write_checkpoint_and_groups` 的**同一连续区间**。
//!
//! # 单调性
//!
//! 低水位**不得回退**：发布比 CF 现值更小的 `checkpoint_lsn` 直接报错
//! （`Regression`）——它是恢复起点，倒退意味着"声称已落盘的又变回未落盘"。
//!
//! # 增量 vs 完全
//!
//! - **增量**（运行期常态）：只发布当前位置，页由写线程慢慢写；
//! - **完全**（关闭工作区/克隆前）：`full_checkpoint` 先把该工作区脏页
//!   **按序全部写回**，再发布——低水位一次推到当前日志位置。

use bicdb_common::seq::{CommitSeq, Lsn};
use bicdb_storage::buffer::{BufferKey, BufferPool};
use bicdb_storage::controlfile::{CheckpointProgress, ControlFileError};
use bicdb_storage::rowid::Rdba;
use bicdb_storage::undo::{repair_committed_slots, UndoChain, UndoChainError, UndoError};

use crate::group::{GroupError, GroupWriter};
use crate::record::RedoRecord;

/// 一次检查点的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointReport {
    /// 已发布的进度。
    pub progress: CheckpointProgress,
    /// 检查点记录的 LSN（= 发布时点的日志位置）。
    pub record_lsn: Lsn,
    /// 本次发布降级的组数（`ACTIVE` → `INACTIVE`）。
    pub groups_demoted: usize,
    /// 本次写回的脏页数（仅 `full_checkpoint`；增量检查点为 0）。
    pub pages_written: u64,
}

/// 检查点错误。
#[derive(Debug)]
pub enum CheckpointError {
    /// 日志写入/发布错误。
    Group(GroupError),
    /// 控制文件访问错误。
    ControlFile(ControlFileError),
    /// 缓冲池错误（写回失败）。
    Pool(bicdb_storage::buffer::BufferError),
    /// Undo chain or transaction slot repair.
    Chain(UndoChainError),
    /// Transaction slot structure.
    Undo(UndoError),
    /// Checkpoint requires the live undo chain bound to this pool.
    InvalidUndo(&'static str),
    /// **低水位回退**——违反单调性，拒绝发布。
    Regression {
        /// 控制文件里的现值。
        old: Lsn,
        /// 请求发布的值。
        new: Lsn,
    },
}

impl std::fmt::Display for CheckpointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CheckpointError::Group(e) => write!(f, "检查点日志：{e}"),
            CheckpointError::ControlFile(e) => write!(f, "检查点控制文件：{e}"),
            CheckpointError::Pool(e) => write!(f, "检查点写回：{e}"),
            CheckpointError::Chain(e) => write!(f, "检查点 undo 链：{e}"),
            CheckpointError::Undo(e) => write!(f, "检查点事务表：{e}"),
            CheckpointError::InvalidUndo(e) => write!(f, "检查点 undo：{e}"),
            CheckpointError::Regression { old, new } => write!(
                f,
                "低水位不得回退：现值 {}，请求 {}",
                old.as_raw(),
                new.as_raw()
            ),
        }
    }
}

impl std::error::Error for CheckpointError {}

impl From<GroupError> for CheckpointError {
    fn from(e: GroupError) -> Self {
        CheckpointError::Group(e)
    }
}
impl From<ControlFileError> for CheckpointError {
    fn from(e: ControlFileError) -> Self {
        CheckpointError::ControlFile(e)
    }
}
impl From<bicdb_storage::buffer::BufferError> for CheckpointError {
    fn from(e: bicdb_storage::buffer::BufferError) -> Self {
        CheckpointError::Pool(e)
    }
}

/// **低水位**（§11.7）：有脏页 ⇒ 该工作区最老的首次变脏 LSN；
/// 无脏页 ⇒ 当前日志位置（`log_end`）。
#[must_use]
pub fn low_water(pool: &BufferPool<'_>, workspace: [u8; 8], log_end: Lsn) -> Lsn {
    pool.low_water(workspace).unwrap_or(log_end)
}

/// **发布一次检查点**（CKPT 角色；增量形态）。
///
/// **次序（编码期钉住）**：① 先发布 CF（进度 + 组降级，同一区间）；
/// ② 再把检查点记录（`0x02`，主段 = 检查点提交序号 │ 检查点 LSN │ 当前提交
/// 序号 │ 最老快照提交序号，各 6B）入流并刷盘。
///
/// 为什么**发布在前**：日志可能正卡在"组满 + 下一组未降级"（`AwaitingCheckpoint`）
/// ——此时**记录根本写不进去**；而破除阻塞的正是这次发布（降级靠它）。
/// 检查点记录是诊断 / PITR 的锚，**恢复起点以 CF 为准**，先发布不损正确性：
/// 之间崩溃 ⇒ 恢复从已发布的进度开始；该进度之前的页已耐久。
pub fn publish_checkpoint(
    writer: &mut GroupWriter<'_, '_>,
    progress: CheckpointProgress,
) -> Result<CheckpointReport, CheckpointError> {
    let old = writer.checkpoint_progress()?;
    if progress.checkpoint_lsn < old.checkpoint_lsn {
        return Err(CheckpointError::Regression {
            old: old.checkpoint_lsn,
            new: progress.checkpoint_lsn,
        });
    }
    let groups_demoted = writer.publish_checkpoint(&progress)?;
    // §11.10 的墙钟采样对：随检查点记录一对（时间点目标点的插值用）。
    if progress.timestamp != 0 {
        writer.append_sample_pair(progress.current_commit_seq, progress.timestamp)?;
    }
    let record_lsn = writer.append(|l| {
        RedoRecord::checkpoint(
            l,
            progress.checkpoint_commit_seq.as_raw(),
            progress.checkpoint_lsn.as_raw(),
            progress.current_commit_seq.as_raw(),
            progress.oldest_snapshot_commit_seq.as_raw(),
        )
    })?;
    writer.flush(record_lsn)?;
    Ok(CheckpointReport {
        progress,
        record_lsn,
        groups_demoted,
        pages_written: 0,
    })
}

/// **完全检查点**（关闭工作区/克隆前的形态，§11.7）：
/// 先把该工作区的脏页**按序全部写回**（DBWR 角色，缓冲池的脏链序），
/// 再把低水位一次推到**当前日志位置**并发布。
pub fn full_checkpoint(
    writer: &mut GroupWriter<'_, '_>,
    pool: &BufferPool<'_>,
    workspace: [u8; 8],
    checkpoint_commit_seq: CommitSeq,
    current_commit_seq: CommitSeq,
    oldest_snapshot_commit_seq: CommitSeq,
    timestamp: u64,
) -> Result<CheckpointReport, CheckpointError> {
    let flushed = pool.flush_workspace(workspace)?;
    let log_end = writer.appended_lsn();
    let progress = CheckpointProgress {
        checkpoint_commit_seq,
        checkpoint_lsn: log_end,
        current_commit_seq,
        oldest_snapshot_commit_seq,
        timestamp,
    };
    let mut report = publish_checkpoint(writer, progress)?;
    report.pages_written = flushed.pages_written;
    Ok(report)
}

/// Full checkpoint at a completed write-operation boundary, including active
/// transactions. The caller holds the workspace writer and undo-chain locks
/// and has no outstanding page shadows or guards. Undo and transaction slots
/// are persisted along with data; recovery finds pre-checkpoint losers by
/// scanning those slots. Commit marks must be repaired before their WAL is reused.
pub fn transaction_checkpoint(
    writer: &mut GroupWriter<'_, '_>,
    pool: &BufferPool<'_>,
    chain: &UndoChain<'_, '_>,
    oldest_snapshot_commit_seq: CommitSeq,
    timestamp: u64,
) -> Result<CheckpointReport, CheckpointError> {
    if chain.bound_pool_addr() != Some(pool as *const _ as usize) {
        return Err(CheckpointError::InvalidUndo(
            "chain must be bound to the checkpoint pool",
        ));
    }
    let end = writer.appended_lsn();
    writer.flush(end)?;
    let mut header = chain.page(0).map_err(CheckpointError::Chain)?;
    let before = *header.as_bytes();
    repair_committed_slots(&mut header, &writer.checkpoint_commits())
        .map_err(CheckpointError::Undo)?;
    if *header.as_bytes() != before {
        let segment = chain.segment();
        let block = segment
            .logical_block(0)
            .ok_or(CheckpointError::InvalidUndo("missing header block"))?;
        let rdba = Rdba::from_parts(segment.file_id(), block)
            .ok_or(CheckpointError::InvalidUndo("invalid header address"))?;
        let mut h = header
            .header()
            .ok_or(CheckpointError::InvalidUndo("invalid header page"))?;
        // end is the NEXT record position; stamping end would skip its redo.
        let stamp = Lsn::from_raw(end.as_raw().saturating_sub(1)).expect("LSN domain");
        h.page_lsn = h.page_lsn.max(stamp);
        header.write_header(&h);
        header.bump_mod_seq();
        let mut guard = pool.pin(BufferKey::new(segment.workspace_ref(), rdba))?;
        guard.as_bytes_mut().copy_from_slice(header.as_bytes());
        guard.mark_dirty(stamp);
    }
    let current = writer.commit_watermark();
    let report = full_checkpoint(
        writer,
        pool,
        chain.segment().workspace_ref(),
        current,
        current,
        oldest_snapshot_commit_seq,
        timestamp,
    )?;
    writer.clear_checkpoint_commits();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use bicdb_storage::buffer::{BufferKey, BufferPool, WalGuard};
    use bicdb_storage::controlfile::{
        ArchiveRecord, ControlFile, LogRunState, RedoEntries, WorkspaceEntry,
    };
    use bicdb_storage::page::{Page, PageType};
    use bicdb_storage::pagefile;
    use bicdb_storage::rowid::Rdba;
    use bicdb_workspace::id::WorkspaceId;
    use bicdb_workspace::io::{FileIo, MemFileIo, OpenOptions};

    use super::*;
    use crate::file::scan_log;
    use crate::group::{online_groups, GroupSpec};
    use crate::record::RecordOp;

    const A: &str = "/mem/control01.ctl";
    const B: &str = "/mem/control02.ctl";
    const WAL: &str = "/mem/wal";
    const WS: [u8; 8] = [3u8; 8];

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

    fn mem() -> MemFileIo {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        io.add_dir(WAL);
        io
    }

    struct FakeWal {
        durable: std::sync::atomic::AtomicU64,
    }
    impl WalGuard for FakeWal {
        fn durable_lsn(&self) -> Lsn {
            Lsn::from_raw(self.durable.load(std::sync::atomic::Ordering::SeqCst)).unwrap()
        }
        fn ensure_durable(&self, target: Lsn) -> std::io::Result<()> {
            let durable =
                Lsn::from_raw(self.durable.load(std::sync::atomic::Ordering::SeqCst)).unwrap();
            if target > durable {
                self.durable
                    .fetch_max(target.as_raw(), std::sync::atomic::Ordering::SeqCst);
            }
            Ok(())
        }
    }

    #[test]
    fn publishes_progress_into_control_file_and_demotes_groups() {
        let io = mem();
        let spec = GroupSpec::new(2, 1, 64).unwrap();
        let mut cf = ControlFile::format(
            &io,
            Path::new(A),
            Path::new(B),
            &ws_entry(),
            &RedoEntries::new(2, 1).unwrap(),
            &ArchiveRecord::default(),
        )
        .unwrap();
        let mut w = GroupWriter::create(&io, &mut cf, Path::new(WAL), spec, lsn(0)).unwrap();
        // 写两条、显式切换一次——组 0 成为 ACTIVE（尾部 = 组 1 的起点）。
        for i in 0..2u64 {
            w.append(|l| {
                RedoRecord::page_modification(
                    l,
                    1,
                    vec![crate::record::BlockRef {
                        flags: 0,
                        rdba: rdba(1, 0),
                        changes: vec![crate::record::Change {
                            offset: 100 + i as u16,
                            after: vec![i as u8; 6],
                        }],
                    }],
                )
            })
            .unwrap();
        }
        assert_eq!(w.switch_group().unwrap(), 1);
        w.flush(w.appended_lsn()).unwrap();

        let log_end = w.appended_lsn();
        let report = publish_checkpoint(
            &mut w,
            CheckpointProgress {
                checkpoint_commit_seq: seq(5),
                checkpoint_lsn: log_end,
                current_commit_seq: seq(9),
                oldest_snapshot_commit_seq: seq(0),
                timestamp: 12345,
            },
        )
        .unwrap();
        assert!(report.record_lsn > log_end, "检查点记录在发布点之后入流");
        assert_eq!(report.groups_demoted, 1, "第一组尾部 ≤ 低水位 ⇒ 降级");

        // 控制文件里的进度与组状态都已发布。
        let cf = ControlFile::open(&io, Path::new(A), Path::new(B)).unwrap();
        let p = cf.checkpoint_progress().unwrap();
        assert_eq!(p.checkpoint_lsn, log_end);
        assert_eq!(p.current_commit_seq, seq(9));
        assert_eq!(p.timestamp, 12345);
        let entries = cf.redo_entries().unwrap();
        let inactive = entries
            .groups
            .iter()
            .filter(|g| g.run == LogRunState::Inactive)
            .count();
        assert_eq!(inactive, 1, "降级与低水位同一区间发布");
        assert_eq!(
            entries
                .groups
                .iter()
                .filter(|g| g.run == LogRunState::Current)
                .count(),
            1,
            "当前组仍是 CURRENT（降级只动 ACTIVE）"
        );
        assert_eq!(
            entries
                .groups
                .iter()
                .filter(|g| g.run == LogRunState::Active)
                .count(),
            0
        );
    }

    #[test]
    fn checkpoint_record_carries_the_progress() {
        let io = mem();
        let spec = GroupSpec::new(2, 1, 64).unwrap();
        let mut cf = ControlFile::format(
            &io,
            Path::new(A),
            Path::new(B),
            &ws_entry(),
            &RedoEntries::new(2, 1).unwrap(),
            &ArchiveRecord::default(),
        )
        .unwrap();
        let mut w = GroupWriter::create(&io, &mut cf, Path::new(WAL), spec, lsn(0)).unwrap();
        let report = publish_checkpoint(
            &mut w,
            CheckpointProgress {
                checkpoint_commit_seq: seq(11),
                checkpoint_lsn: lsn(4096),
                current_commit_seq: seq(12),
                oldest_snapshot_commit_seq: seq(2),
                timestamp: 0,
            },
        )
        .unwrap();
        w.flush(w.appended_lsn()).unwrap();
        w.close().unwrap();

        let cf = ControlFile::open(&io, Path::new(A), Path::new(B)).unwrap();
        let groups = online_groups(&io, &cf, Path::new(WAL), spec).unwrap();
        let scan = scan_log(
            &io,
            groups[0].handle,
            groups[0].start_lsn,
            u64::from(groups[0].file_pages),
        )
        .unwrap();
        let rec = scan
            .records
            .iter()
            .find(|r| r.op == RecordOp::Checkpoint.as_u8())
            .expect("检查点记录在流中");
        assert_eq!(rec.lsn, report.record_lsn);
        assert_eq!(rec.main.len(), 24, "主段 = 四个 6B");
        let decode = |off: usize| {
            let mut b = [0u8; 8];
            b[..6].copy_from_slice(&rec.main[off..off + 6]);
            u64::from_le_bytes(b)
        };
        assert_eq!(decode(0), 11, "检查点提交序号");
        assert_eq!(decode(6), 4096, "检查点 LSN");
        assert_eq!(decode(12), 12, "当前提交序号");
        assert_eq!(decode(18), 2, "最老快照提交序号");
    }

    #[test]
    fn checkpoint_records_a_wall_clock_sample() {
        let io = mem();
        let spec = GroupSpec::new(2, 1, 64).unwrap();
        let mut cf = ControlFile::format(
            &io,
            Path::new(A),
            Path::new(B),
            &ws_entry(),
            &RedoEntries::new(2, 1).unwrap(),
            &ArchiveRecord::default(),
        )
        .unwrap();
        let mut w = GroupWriter::create(&io, &mut cf, Path::new(WAL), spec, lsn(0)).unwrap();
        publish_checkpoint(
            &mut w,
            CheckpointProgress {
                checkpoint_commit_seq: seq(3),
                checkpoint_lsn: lsn(0),
                current_commit_seq: seq(7),
                oldest_snapshot_commit_seq: seq(0),
                timestamp: 12_345,
            },
        )
        .unwrap();
        let cf_ro = ControlFile::open(&io, Path::new(A), Path::new(B)).unwrap();
        let pairs = cf_ro.sample_pairs().unwrap();
        assert_eq!(
            pairs,
            vec![(seq(7), 12_345)],
            "（当前提交序号, 时刻）成对落控制文件"
        );
        // 墙钟目标点可直接解析（配合 pitr 的按目标恢复）。
        assert_eq!(
            crate::analysis::resolve_wall_clock(&pairs, 12_345).unwrap(),
            seq(7)
        );
    }

    #[test]
    fn regression_is_rejected() {
        let io = mem();
        let spec = GroupSpec::new(2, 1, 64).unwrap();
        let mut cf = ControlFile::format(
            &io,
            Path::new(A),
            Path::new(B),
            &ws_entry(),
            &RedoEntries::new(2, 1).unwrap(),
            &ArchiveRecord::default(),
        )
        .unwrap();
        let mut w = GroupWriter::create(&io, &mut cf, Path::new(WAL), spec, lsn(0)).unwrap();
        let make = |v: u64| CheckpointProgress {
            checkpoint_commit_seq: seq(0),
            checkpoint_lsn: lsn(v),
            current_commit_seq: seq(0),
            oldest_snapshot_commit_seq: seq(0),
            timestamp: 0,
        };
        publish_checkpoint(&mut w, make(100)).unwrap();
        let err = publish_checkpoint(&mut w, make(50)).unwrap_err();
        assert!(
            matches!(err, CheckpointError::Regression { old, new } if old == lsn(100) && new == lsn(50)),
            "{err}"
        );
    }

    #[test]
    fn low_water_follows_the_dirty_chain_and_full_checkpoint_drains_it() {
        let io = mem();
        io.add_dir("/mem/data");
        // 数据页（file 9，工作区 WS），一页。
        let data = io
            .open(
                Path::new("/mem/data/d9"),
                OpenOptions::new().read(true).write(true).create_new(true),
            )
            .unwrap();
        io.set_len(data, 2 * bicdb_storage::page::PAGE_SIZE as u64)
            .unwrap();
        for block in 0..2u32 {
            let mut page = Page::new(PageType::HeapTable, WS, 9, block);
            page.as_bytes_mut()[100] = 0xC0 + block as u8;
            pagefile::write_page(&io, data, block, &mut page).unwrap();
        }

        let pool = BufferPool::new(
            &io,
            4,
            move |ws, r| (*ws == WS && r.file_id() == 9).then_some((data, r.block_id())),
            FakeWal {
                durable: std::sync::atomic::AtomicU64::new(0),
            },
        )
        .unwrap();
        {
            let mut g = pool.pin(BufferKey::new(WS, rdba(9, 0))).unwrap();
            let mut h = g.header().unwrap();
            h.page_lsn = lsn(4);
            g.write_header(&h);
            g.mark_dirty(lsn(4));
        }
        assert_eq!(low_water(&pool, WS, lsn(999)), lsn(4), "有脏页 ⇒ 脏链头");

        // 日志侧（一个组就够）。
        let spec = GroupSpec::new(2, 1, 64).unwrap();
        let mut cf = ControlFile::format(
            &io,
            Path::new(A),
            Path::new(B),
            &ws_entry(),
            &RedoEntries::new(2, 1).unwrap(),
            &ArchiveRecord::default(),
        )
        .unwrap();
        let mut w = GroupWriter::create(&io, &mut cf, Path::new(WAL), spec, lsn(0)).unwrap();
        let log_end = w.appended_lsn();
        assert_eq!(low_water(&pool, WS, log_end), lsn(4), "仍是脏页的低水位");

        let report = full_checkpoint(&mut w, &pool, WS, seq(1), seq(2), seq(0), 777).unwrap();
        assert_eq!(report.pages_written, 1, "脏页已写回");
        assert_eq!(pool.dirty_len(WS), 0);
        assert!(
            report.progress.checkpoint_lsn >= log_end,
            "完全检查点把低水位推到当前日志位置"
        );
        assert_eq!(
            low_water(&pool, WS, lsn(9999)),
            lsn(9999),
            "无脏页 ⇒ 日志位置"
        );
    }
}
