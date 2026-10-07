//! 恢复驱动（§11.2 的**三阶段**）：[`redo_from`] 是重做阶段本身；
//! [`recover`] 把分析 → 重做 → 撤销装成**一个入口**（工作区打开路径调用它）。
//!
//! ```text
//! ① 分析（analyze_from）：事务结局判定；起点 = 控制文件检查点 LSN（低水位）
//! ② 重做（redo_from）：从起点重放；按 page_lsn 判定已包含的修改并跳过（幂等）
//! ③ 撤销（repair_committed_slots + rollback_losers）：
//!      事务表**前滚补标记**（胜者）→ 输家 = 槽扫描 ∪（日志侧由分析给出）
//!      → 整链回滚（补偿生成 redo 并刷盘后才写页）
//! ```
//!
//! **次序不可变**（§11.2）：先重做再撤销——撤销要读 undo 页与事务表，
//! 而它们自己也要先被重做出来；且"补标记"必须在重做把事务表页恢复到
//! 崩溃前状态**之后**做（否则会被重放覆盖）。
//!
//! # 流顺序从哪来
//!
//! 在线组按**序列号升序**即日志流顺序（[`crate::group::online_groups`]——
//! 每组的序列号 = 它最近一次被激活时的序列，各已用组互不相同；组与组在
//! LSN 上连续）。起点之前的记录**跳过不应用**（但不算错）。
//!
//! # 终点
//!
//! [`RedoReport::log_end`] = 在线组已写前缀的尽头——它同时是**撤销阶段的起点**
//! 与**恢复完成后写方的续写位置**（"日志末端不完整就停"即止于此）。

use bicdb_common::seq::Lsn;
use bicdb_workspace::io::FileIo;

use crate::apply::{apply_record, ApplyError, BlockResolver};
use crate::file::{scan_log, LogFileError};
use crate::group::OnlineGroup;

/// 重做阶段的执行报告。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RedoReport {
    /// 被扫描的组数（整组落在起点之前的组不计）。
    pub groups_scanned: usize,
    /// 被应用的记录数（起点之前的记录不计）。
    pub records_applied: usize,
    /// 真正写入的块数（合计）。
    pub applied_blocks: usize,
    /// 因 `page_lsn` 已越过而跳过的块数（重复重放/已落盘）。
    pub skipped_blocks: usize,
    /// 日志末端（在线组已写前缀的尽头）。
    pub log_end: Lsn,
}

impl Default for RedoReport {
    fn default() -> Self {
        Self {
            groups_scanned: 0,
            records_applied: 0,
            applied_blocks: 0,
            skipped_blocks: 0,
            log_end: Lsn::from_raw(0).expect("0 在 48 位域内"),
        }
    }
}

/// 重做阶段错误。
#[derive(Debug)]
pub enum RecoveryError {
    /// 底层 I/O。
    Io(std::io::Error),
    /// 日志文件扫描错误。
    File(LogFileError),
    /// 应用错误（页损坏、块无法定位等）。
    Apply(ApplyError),
    /// 记录结构自洽但语义不完整（如提交记录缺 `commit_seq` 主段）。
    Malformed(&'static str),
    /// 撤销阶段错误。
    UndoPhase(crate::undo_phase::UndoPhaseError),
    /// 事务表修复错误。
    Undo(bicdb_storage::undo::UndoError),
    /// 组发现错误。
    Group(crate::group::GroupError),
    /// undo 段访问错误。
    Segment(bicdb_storage::segment::SegmentSpaceError),
}

impl std::fmt::Display for RecoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecoveryError::Io(e) => write!(f, "恢复 I/O：{e}"),
            RecoveryError::File(e) => write!(f, "恢复扫描：{e}"),
            RecoveryError::Apply(e) => write!(f, "恢复应用：{e}"),
            RecoveryError::Malformed(s) => write!(f, "恢复：记录不完整——{s}"),
            RecoveryError::UndoPhase(e) => write!(f, "{e}"),
            RecoveryError::Undo(e) => write!(f, "恢复的事务表修复：{e}"),
            RecoveryError::Group(e) => write!(f, "恢复的组发现：{e}"),
            RecoveryError::Segment(e) => write!(f, "恢复的 undo 段访问：{e}"),
        }
    }
}

impl std::error::Error for RecoveryError {}

impl From<std::io::Error> for RecoveryError {
    fn from(e: std::io::Error) -> Self {
        RecoveryError::Io(e)
    }
}

impl From<LogFileError> for RecoveryError {
    fn from(e: LogFileError) -> Self {
        RecoveryError::File(e)
    }
}

impl From<ApplyError> for RecoveryError {
    fn from(e: ApplyError) -> Self {
        RecoveryError::Apply(e)
    }
}

impl From<crate::undo_phase::UndoPhaseError> for RecoveryError {
    fn from(e: crate::undo_phase::UndoPhaseError) -> Self {
        RecoveryError::UndoPhase(e)
    }
}

impl From<bicdb_storage::undo::UndoError> for RecoveryError {
    fn from(e: bicdb_storage::undo::UndoError) -> Self {
        RecoveryError::Undo(e)
    }
}

impl From<crate::group::GroupError> for RecoveryError {
    fn from(e: crate::group::GroupError) -> Self {
        RecoveryError::Group(e)
    }
}

impl From<bicdb_storage::segment::SegmentSpaceError> for RecoveryError {
    fn from(e: bicdb_storage::segment::SegmentSpaceError) -> Self {
        RecoveryError::Segment(e)
    }
}

/// **重做阶段**：从 `start_lsn`（检查点 LSN = 低水位）起重放在线组。
///
/// `groups` 必须来自 [`crate::group::online_groups`]（已按流顺序排好）；
/// 块定位由调用方经 [`BlockResolver`] 提供。
pub fn redo_from(
    io: &dyn FileIo,
    groups: &[OnlineGroup],
    start_lsn: Lsn,
    resolver: &mut BlockResolver<'_>,
) -> Result<RedoReport, RecoveryError> {
    redo_until(io, groups, start_lsn, ReplayBound::ToEnd, resolver)
}

/// 重放/判定的**区间上界**（PITR 的"目标点之后不再重放"）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayBound {
    /// 到日志末端（崩溃恢复）。
    ToEnd,
    /// 含上界（PITR：最后一条 `commit_seq ≤ S*` 的提交记录的 LSN）。
    Through(Lsn),
    /// **空区间**：不重放/不判定任何记录（目标点之前没有提交记录）。
    Nothing,
}

/// **有界重做**（PITR 用）：只重放到区间上界为止。
pub fn redo_until(
    io: &dyn FileIo,
    groups: &[OnlineGroup],
    start_lsn: Lsn,
    bound: ReplayBound,
    resolver: &mut BlockResolver<'_>,
) -> Result<RedoReport, RecoveryError> {
    let mut report = RedoReport {
        log_end: start_lsn,
        ..RedoReport::default()
    };
    for g in groups {
        if g.end_lsn > report.log_end {
            report.log_end = g.end_lsn;
        }
        if g.end_lsn <= start_lsn {
            continue; // 整组落在起点之前
        }
        let scan = scan_log(io, g.handle, g.start_lsn, u64::from(g.file_pages))?;
        report.groups_scanned += 1;
        for record in &scan.records {
            if record.lsn < start_lsn {
                continue; // 起点之前的记录：不应用
            }
            match bound {
                ReplayBound::ToEnd => {}
                ReplayBound::Through(stop) => {
                    if record.lsn > stop {
                        continue; // 目标点之后的记录：不应用（PITR）
                    }
                }
                ReplayBound::Nothing => continue,
            }
            report.records_applied += 1;
            let outcome = apply_record(io, record, resolver)?;
            report.applied_blocks += outcome.applied;
            report.skipped_blocks += outcome.skipped;
        }
    }
    Ok(report)
}

/// 三阶段恢复的执行报告。
#[derive(Debug)]
pub struct RecoveryReport {
    /// 恢复起点（控制文件检查点 LSN = 低水位）。
    pub start_lsn: Lsn,
    /// ① 分析。
    pub analysis: crate::analysis::AnalysisReport,
    /// ② 重做。
    pub redo: RedoReport,
    /// ③ 日志里**已提交、参与前滚补标记**的事务数（幂等：已标记的槽不再改写）。
    pub committed_txns: usize,
    /// ③ 输家回滚。
    pub undo: crate::undo_phase::UndoReport,
    /// 恢复后的**续写位置**（补偿 redo 之后）。
    pub log_end: Lsn,
}

/// **三阶段恢复**（§11.2）：工作区打开路径的入口。
///
/// ```text
/// ① analyze_from       起点 = 控制文件检查点 LSN（低水位）
/// ② redo_from          重放（幂等：page_lsn 判重）
/// ③ repair_committed_slots + rollback_losers
///      补标记（胜者）→ 输家整链回滚（补偿写 redo）
/// ```
///
/// - `groups` 与 `writer` 来自同一日志目录：`groups` 只读扫描，`writer` 用于
///   撤销阶段的补偿 redo 与恢复后的续写（调用方保证 `writer` 已 `open` 到
///   日志末端）；
/// - `chain` 是 undo 段的链视图（调用方已打开 undo 文件）；
/// - `resolve` 覆盖**全部**涉及的块（数据页与 undo 段头页）。
///
/// **顺序不可换**：补标记在重做**之后**（否则被重放覆盖）；撤销在最后
/// （它要读已重做出来的 undo 页与事务表）。重跑本函数是安全的（三个阶段的
/// 幂等性各自成立）。
pub fn recover(
    io: &dyn FileIo,
    groups: &[OnlineGroup],
    start_lsn: Lsn,
    chain: &bicdb_storage::undo::UndoChain<'_, '_>,
    writer: &mut crate::group::GroupWriter<'_, '_>,
    resolver: &mut BlockResolver<'_>,
) -> Result<RecoveryReport, RecoveryError> {
    // ① 分析（纯日志流判定）。
    let analysis = crate::analysis::analyze_from(io, groups, start_lsn)?;

    // ② 重做。
    let redo = redo_from(io, groups, start_lsn, resolver)?;

    // ③ 事务表修复：胜者补标记 + 输家槽扫描（**在重做之后**——事务表页已被
    //    重放到崩溃前状态；补标记本身是从日志可重导的幂等动作，不需 redo）。
    let committed: Vec<(bicdb_storage::undo::TxnId, bicdb_common::seq::CommitSeq)> = analysis
        .committed()
        .into_iter()
        .map(|(raw, seq)| {
            let mut b = [0u8; 6];
            b.copy_from_slice(&raw.to_le_bytes()[..6]);
            (
                bicdb_storage::undo::TxnId::from_bytes(&b),
                bicdb_common::seq::CommitSeq::from_raw(seq).expect("48 位域内"),
            )
        })
        .collect();
    let mut header = chain.segment().read_page(0)?;
    let before = *header.as_bytes();
    let losers = bicdb_storage::undo::repair_committed_slots(&mut header, &committed)?;
    if *header.as_bytes() != before {
        // 补标记只是**从日志可重导**的幂等动作（重跑恢复会重算），
        // 因此不需要为它生成 redo；页内容变了才落盘。
        chain.segment().write_page(0, &mut header)?;
    }

    // ③ 撤销：输家整链回滚（补偿生成 redo 并刷盘后才写页）。
    let undo = crate::undo_phase::rollback_losers(io, writer, chain, &losers, resolver)?;

    Ok(RecoveryReport {
        start_lsn,
        analysis,
        redo,
        committed_txns: committed.len(),
        undo,
        log_end: writer.appended_lsn(),
    })
}

/// **按目标点恢复（PITR 重放 + 第三阶段）**（§11.10）：
///
/// ```text
/// target（提交序号，权威）
///   → stop = pitr_stop(…)：最后一条 commit_seq ≤ target 的提交记录的 LSN
///        （None ⇒ 范围内没有这样的提交 ⇒ 不重放）
///   → redo_until（含 stop）→ analyze_until（同一上界）
///   → 补标记 + 输家回滚（**目标点之后才提交的不得算胜者**）
/// ```
///
/// 结果 = "目标点时刻已提交的生效、未提交的消失"。归档链完整性
/// （REQ-OPS-013 的缺口指认）与文件还原属于调用方的责任——本函数只覆盖
/// 日志侧的重放与第三阶段。
pub fn recover_to_target(
    io: &dyn FileIo,
    groups: &[OnlineGroup],
    start_lsn: Lsn,
    target: bicdb_common::seq::CommitSeq,
    chain: &bicdb_storage::undo::UndoChain<'_, '_>,
    writer: &mut crate::group::GroupWriter<'_, '_>,
    resolver: &mut BlockResolver<'_>,
) -> Result<RecoveryReport, RecoveryError> {
    let bound = match crate::analysis::pitr_stop(io, groups, start_lsn, target.as_raw())? {
        Some(stop) => ReplayBound::Through(stop),
        None => ReplayBound::Nothing, // 范围内没有 commit_seq ≤ S* ⇒ 不重放
    };

    let analysis = crate::analysis::analyze_until(io, groups, start_lsn, bound)?;
    let redo = redo_until(io, groups, start_lsn, bound, resolver)?;

    let committed: Vec<(bicdb_storage::undo::TxnId, bicdb_common::seq::CommitSeq)> = analysis
        .committed()
        .into_iter()
        .map(|(raw, seq)| {
            let mut b = [0u8; 6];
            b.copy_from_slice(&raw.to_le_bytes()[..6]);
            (
                bicdb_storage::undo::TxnId::from_bytes(&b),
                bicdb_common::seq::CommitSeq::from_raw(seq).expect("48 位域内"),
            )
        })
        .collect();
    let mut header = chain.segment().read_page(0)?;
    let before = *header.as_bytes();
    let losers = bicdb_storage::undo::repair_committed_slots(&mut header, &committed)?;
    if *header.as_bytes() != before {
        chain.segment().write_page(0, &mut header)?;
    }
    let undo = crate::undo_phase::rollback_losers(io, writer, chain, &losers, resolver)?;

    Ok(RecoveryReport {
        start_lsn,
        analysis,
        redo,
        committed_txns: committed.len(),
        undo,
        log_end: writer.appended_lsn(),
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use bicdb_common::seq::CommitSeq;
    use bicdb_storage::controlfile::{ArchiveRecord, ControlFile, RedoEntries, WorkspaceEntry};
    use bicdb_storage::page::{Page, PageType, WORKSPACE_REF_LEN};
    use bicdb_storage::pagefile;
    use bicdb_workspace::id::WorkspaceId;
    use bicdb_workspace::io::{FileHandle, MemFileIo};

    use super::*;
    use crate::group::{online_groups, GroupSpec, GroupWriter};
    use crate::record::{BlockRef, Change, Rdba, RedoRecord};

    const A: &str = "/mem/control01.ctl";
    const B: &str = "/mem/control02.ctl";
    const WAL: &str = "/mem/wal";

    fn lsn(v: u64) -> Lsn {
        Lsn::from_raw(v).unwrap()
    }

    fn rdba(file: u16, block: u32) -> Rdba {
        Rdba::from_parts(file, block).unwrap()
    }

    fn ws_entry() -> WorkspaceEntry {
        WorkspaceEntry {
            workspace_id: WorkspaceId::from_raw(1).unwrap(),
            created_at: 0,
            derived_from: None,
            derived_at_seq: CommitSeq::from_raw(0).unwrap(),
        }
    }

    fn mem() -> MemFileIo {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        io.add_dir(WAL);
        io
    }

    fn make_page_file(io: &dyn FileIo, path: &str, file_id: u16, block_id: u32) -> FileHandle {
        let h = pagefile::create(io, Path::new(path), 1).unwrap();
        let mut page = Page::new(
            PageType::HeapTable,
            [0u8; WORKSPACE_REF_LEN],
            file_id,
            block_id,
        );
        pagefile::write_page(io, h, block_id, &mut page).unwrap();
        h
    }

    fn mod_rec(lsn_v: Lsn, r: Rdba, offset: u16, bytes: &[u8]) -> RedoRecord {
        RedoRecord::page_modification(
            lsn_v,
            1,
            vec![BlockRef {
                flags: 0,
                rdba: r,
                changes: vec![Change {
                    offset,
                    after: bytes.to_vec(),
                }],
            }],
        )
    }

    #[test]
    fn redo_replays_all_online_groups() {
        let io = mem();
        io.add_dir("/mem/data");
        let spec = GroupSpec::new(2, 1, 4).unwrap();
        let h1 = make_page_file(&io, "/mem/data/f1", 1, 0);
        let h2 = make_page_file(&io, "/mem/data/f2", 2, 0);
        let (r1, r2) = (rdba(1, 0), rdba(2, 0));

        {
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
            // 足够多的记录撑满第一组并触发切换（每条 38 B；4 页组 ≈ 39 条切换）。
            for i in 0..50u64 {
                w.append(|l| mod_rec(l, r1, 100 + i as u16, &[(i as u8); 6]))
                    .unwrap();
            }
            w.append(|l| mod_rec(l, r2, 50, &[0x77; 6])).unwrap();
            w.flush(w.appended_lsn()).unwrap();
            assert!(w.current_group() >= 1, "应已发生组切换");
            w.close().unwrap();
        }

        let cf = ControlFile::open(&io, Path::new(A), Path::new(B)).unwrap();
        let groups = online_groups(&io, &cf, Path::new(WAL), spec).unwrap();
        assert_eq!(groups.len(), 2, "两个组都已写");
        assert!(groups[0].sequence < groups[1].sequence, "按序列号升序");

        let mut resolver = |r: Rdba| match r.file_id() {
            1 => Some((h1, 0)),
            _ => Some((h2, 0)),
        };
        let report = redo_from(&io, &groups, lsn(0), &mut resolver).unwrap();
        assert!(report.groups_scanned == 2);
        assert!(report.records_applied >= 51);
        assert!(report.applied_blocks >= 51);
        assert_eq!(report.skipped_blocks, 0, "首次重做无跳过");
        assert_eq!(report.log_end, groups[1].end_lsn, "端点 = 末组的已写尽头");

        // 页上的最终字节：r1 的最后一条（i=49）落在 149..155；r2 落在 50..56。
        let p1 = pagefile::read_page_verified(&io, h1, 0).unwrap();
        assert_eq!(&p1.as_bytes()[149..155], &[49u8; 6]);
        let p2 = pagefile::read_page_verified(&io, h2, 0).unwrap();
        assert_eq!(&p2.as_bytes()[50..56], &[0x77; 6]);
    }

    #[test]
    fn redo_from_mid_stream_skips_earlier_records() {
        let io = mem();
        io.add_dir("/mem/data");
        let spec = GroupSpec::new(2, 1, 4).unwrap();
        let h1 = make_page_file(&io, "/mem/data/f1", 1, 0);
        let r1 = rdba(1, 0);

        let second_lsn;
        {
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
            w.append(|l| mod_rec(l, r1, 100, &[0x11; 6])).unwrap();
            second_lsn = w.append(|l| mod_rec(l, r1, 200, &[0x22; 6])).unwrap();
            w.flush(w.appended_lsn()).unwrap();
            w.close().unwrap();
        }

        let cf = ControlFile::open(&io, Path::new(A), Path::new(B)).unwrap();
        let groups = online_groups(&io, &cf, Path::new(WAL), spec).unwrap();
        let mut resolver = |_: Rdba| Some((h1, 0));
        let report = redo_from(&io, &groups, second_lsn, &mut resolver).unwrap();
        assert_eq!(report.records_applied, 1, "起点之前的记录不应用");

        let page = pagefile::read_page_verified(&io, h1, 0).unwrap();
        assert_eq!(&page.as_bytes()[100..106], &[0; 6], "第一条未应用");
        assert_eq!(&page.as_bytes()[200..206], &[0x22; 6], "第二条已应用");
        assert_eq!(page.header().unwrap().page_lsn, second_lsn);
    }

    #[test]
    fn rerun_is_idempotent() {
        let io = mem();
        io.add_dir("/mem/data");
        let spec = GroupSpec::new(2, 1, 4).unwrap();
        let h1 = make_page_file(&io, "/mem/data/f1", 1, 0);
        let r1 = rdba(1, 0);
        {
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
            for i in 0..5u64 {
                w.append(|l| mod_rec(l, r1, 300 + i as u16, &[(i as u8 + 1); 4]))
                    .unwrap();
            }
            w.flush(w.appended_lsn()).unwrap();
            w.close().unwrap();
        }
        let cf = ControlFile::open(&io, Path::new(A), Path::new(B)).unwrap();
        let groups = online_groups(&io, &cf, Path::new(WAL), spec).unwrap();
        let mut resolver = |_: Rdba| Some((h1, 0));
        let first = redo_from(&io, &groups, lsn(0), &mut resolver).unwrap();
        let before = pagefile::read_page_verified(&io, h1, 0).unwrap();
        let again = redo_from(&io, &groups, lsn(0), &mut resolver).unwrap();
        assert_eq!(first.applied_blocks, 5);
        assert_eq!(again.applied_blocks, 0, "重复重做全部跳过");
        assert_eq!(again.skipped_blocks, first.applied_blocks);
        let after = pagefile::read_page_verified(&io, h1, 0).unwrap();
        assert_eq!(before.as_bytes(), after.as_bytes(), "页逐字节不变");
    }
}

/// **三阶段恢复的端到端用例**：崩溃现场 = 数据页停在旧版本、日志里
/// 有胜者与输家的页修改、undo 段里有两条链。
#[cfg(test)]
mod recover_tests {
    use std::path::Path;

    use bicdb_common::seq::{CommitSeq, Lsn};
    use bicdb_storage::controlfile::{ArchiveRecord, ControlFile, RedoEntries, WorkspaceEntry};
    use bicdb_storage::cr;
    use bicdb_storage::datafile::DataFile;
    use bicdb_storage::heap::{self, InsertPolicy};
    use bicdb_storage::itl::{self, ItlEntry, ItlState};
    use bicdb_storage::page::{Page, PageType};
    use bicdb_storage::pagefile;
    use bicdb_storage::row::assemble_row;
    use bicdb_storage::rowid::RowId;
    use bicdb_storage::undo::{
        create_undo_segment, read_slot, TxnId, TxnState, UndoChain, UndoOp, UndoPayload,
    };
    use bicdb_workspace::id::WorkspaceId;
    use bicdb_workspace::io::MemFileIo;

    use super::*;
    use crate::group::{online_groups, GroupSpec, GroupWriter};
    use crate::record::{page_diff, BlockRef, Change, Rdba, RedoRecord};

    const UNDO_F: &str = "/mem/undo1.dat";
    const DATA_F: &str = "/mem/data.dat";
    const A: &str = "/mem/control01.ctl";
    const B: &str = "/mem/control02.ctl";
    const WAL: &str = "/mem/wal";
    const WS: [u8; 8] = [4u8; 8];

    fn lsn(v: u64) -> Lsn {
        Lsn::from_raw(v).unwrap()
    }

    fn rdba(file_id: u16, block: u32) -> Rdba {
        Rdba::from_parts(file_id, block).unwrap()
    }

    fn ws_entry() -> WorkspaceEntry {
        WorkspaceEntry {
            workspace_id: WorkspaceId::from_raw(1).unwrap(),
            created_at: 0,
            derived_from: None,
            derived_at_seq: CommitSeq::from_raw(0).unwrap(),
        }
    }

    fn mem() -> MemFileIo {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        io.add_dir(WAL);
        io
    }

    /// 一页的每个变更集（before → after 的差异）——真实的写路径也就这么生成。
    fn changes(before: &Page, after: &Page) -> Vec<Change> {
        page_diff(before.as_bytes(), after.as_bytes())
    }

    #[test]
    fn recovers_winner_and_rolls_back_loser_end_to_end() {
        let io = mem();
        let spec = GroupSpec::new(2, 1, 64).unwrap();

        // ---- 夹具：undo 段（两个事务）+ 数据文件（块 0，初始为空页） ----
        let mut undo_file = DataFile::create(&io, Path::new(UNDO_F), 1, 1, WS, 512).unwrap();
        let undo_handle = undo_file.handle();
        let segment = create_undo_segment(&mut undo_file, 2, 3, 4).unwrap();
        let mut chain = UndoChain::open(segment);
        let loser_slot = chain.allocate_slot().unwrap();
        let winner_slot = chain.allocate_slot().unwrap();
        let loser_raw = TxnId::from_parts(0, loser_slot as u8, 0).as_raw();
        let winner_raw = TxnId::from_parts(0, winner_slot as u8, 0).as_raw();

        let data = pagefile::create(&io, Path::new(DATA_F), 1).unwrap();
        let before = Page::new(PageType::HeapTable, WS, 3, 0);
        pagefile::write_page(&io, data, 0, &mut before.clone()).unwrap();

        // 输家：插入一行（ITL[0] = 活动，链上 = 占用 + 插入）。
        let loser_bytes = assemble_row(0, 1, &[false], &[], &[b"loser".as_slice()]).unwrap();
        let mut after1 = before.clone();
        let loser_row =
            heap::insert_row(&mut after1, &loser_bytes, &InsertPolicy::in_place(0)).unwrap();
        let loser_rid = RowId::from_parts(3, 0, loser_row).unwrap();
        chain
            .append(
                loser_slot,
                UndoOp::ItlOverwrite,
                0,
                RowId::from_parts(3, 0, 1).unwrap(),
                UndoPayload::ItlOverwrite {
                    txn_id: TxnId::from_parts(0, loser_slot as u8, 0),
                    itl_slot: 0,
                    old: None,
                },
            )
            .unwrap();
        let loser_head = chain
            .append(loser_slot, UndoOp::Insert, 0, loser_rid, UndoPayload::None)
            .unwrap();
        itl::write_itl(
            &mut after1,
            0,
            &ItlEntry {
                txn_id: TxnId::from_parts(0, loser_slot as u8, 0),
                undo_ptr: Some(loser_head),
                commit_seq: None,
                lock_cnt: 1,
                state: ItlState::Active,
            },
        )
        .unwrap();

        // 胜者：插入一行并提交（ITL[1] 留"活动"外观——延迟块清除）。
        let winner_bytes = assemble_row(0, 2, &[false], &[], &[b"winner".as_slice()]).unwrap();
        let mut after2 = after1.clone();
        let winner_row =
            heap::insert_row(&mut after2, &winner_bytes, &InsertPolicy::in_place(0)).unwrap();
        itl::grow(&mut after2, 8).unwrap(); // ITL 动态扩展：槽 1 就位
        itl::write_itl(
            &mut after2,
            1,
            &ItlEntry {
                txn_id: TxnId::from_parts(0, winner_slot as u8, 0),
                undo_ptr: None,
                commit_seq: None,
                lock_cnt: 1,
                state: ItlState::Active,
            },
        )
        .unwrap();

        // ---- 日志：两条页修改（含页头字段与 ITL 的差异）+ 胜者的提交 ----
        let mut cf = ControlFile::format(
            &io,
            Path::new(A),
            Path::new(B),
            &ws_entry(),
            &RedoEntries::new(2, 1).unwrap(),
            &ArchiveRecord::default(),
        )
        .unwrap();
        let mut writer = GroupWriter::create(&io, &mut cf, Path::new(WAL), spec, lsn(0)).unwrap();
        writer
            .append(|l| {
                RedoRecord::page_modification(
                    l,
                    loser_raw,
                    vec![BlockRef {
                        flags: 0,
                        rdba: rdba(3, 0),
                        changes: changes(&before, &after1),
                    }],
                )
            })
            .unwrap();
        writer
            .append(|l| {
                RedoRecord::page_modification(
                    l,
                    winner_raw,
                    vec![BlockRef {
                        flags: 0,
                        rdba: rdba(3, 0),
                        changes: changes(&after1, &after2),
                    }],
                )
            })
            .unwrap();
        writer
            .append(|l| RedoRecord::commit(l, winner_raw, 7))
            .unwrap();
        writer.flush(writer.appended_lsn()).unwrap();

        // ---- 恢复 ----
        let cf_ro = ControlFile::open(&io, Path::new(A), Path::new(B)).unwrap();
        let groups = online_groups(&io, &cf_ro, Path::new(WAL), spec).unwrap();
        let mut resolve = |r: Rdba| match r.file_id() {
            1 => Some((undo_handle, r.block_id())),
            3 => Some((data, r.block_id())),
            _ => None,
        };
        let report = recover(&io, &groups, lsn(0), &chain, &mut writer, &mut resolve).unwrap();

        // ① 分析：胜者带序号、输家 = 全零 txn（第一个事务）。
        assert_eq!(report.analysis.committed(), vec![(winner_raw, 7)]);
        assert_eq!(report.analysis.losers(), vec![loser_raw]);
        assert_eq!(report.committed_txns, 1);
        // ② 重做：4 条记录（组激活的切换 + 两条页修改 + 提交），
        //    其中 2 个块真正落盘（两条页修改；含输家的——顺序不可颠倒）。
        assert_eq!(report.redo.records_applied, 4);
        assert_eq!(report.redo.applied_blocks, 2);
        // ③ 撤销：只回滚输家。
        assert_eq!(report.undo.txns_rolled_back, 1);

        // ---- 结果 ----
        let page = pagefile::read_page_verified(&io, data, 0).unwrap();
        assert_eq!(heap::row(&page, loser_row), None, "输家的行已回滚");
        assert_eq!(
            heap::row(&page, winner_row),
            Some(&winner_bytes[..]),
            "胜者的行保留"
        );
        assert_eq!(
            itl::read_itl(&page, 0).unwrap().state,
            ItlState::Free,
            "ITL 还原"
        );
        // 胜者：槽已前滚补标记（提交序号准确）；ITL 条目仍"活动"外观。
        let hdr = chain.segment().read_page(0).unwrap();
        let w = read_slot(&hdr, winner_slot).unwrap();
        assert_eq!(w.state, TxnState::Committed);
        assert_eq!(w.commit_seq, CommitSeq::from_raw(7).unwrap());
        let l = read_slot(&hdr, loser_slot).unwrap();
        assert_eq!(l.state, TxnState::Free, "输家槽已释放");
        assert_eq!(l.wrap, 1);

        // 快照 ≥ 7 的一致性读：胜者的行可见（ITL 活动 + 事务表补标记救回来）。
        let snapshot = CommitSeq::from_raw(8).unwrap();
        let cr_page = cr::reconstruct(&page, cr::ReadView::new(snapshot), &chain).unwrap();
        assert_eq!(heap::row(&cr_page, winner_row), Some(&winner_bytes[..]));

        // ---- 重跑恢复：幂等（无输家、重做全跳过） ----
        let groups2 = online_groups(&io, &cf_ro, Path::new(WAL), spec).unwrap();
        let again = recover(&io, &groups2, lsn(0), &chain, &mut writer, &mut resolve).unwrap();
        assert_eq!(again.undo.txns_rolled_back, 0, "已回滚的不再回滚");
        assert_eq!(again.redo.applied_blocks, 0, "重做全跳过");
        let page2 = pagefile::read_page_verified(&io, data, 0).unwrap();
        assert_eq!(page2.as_bytes(), page.as_bytes(), "页逐字节不变");
    }
}

/// **PITR（按目标点恢复）的端到端用例**（§11.10）。
#[cfg(test)]
mod pitr_tests {
    use std::path::Path;

    use bicdb_common::seq::{CommitSeq, Lsn};
    use bicdb_storage::controlfile::{ArchiveRecord, ControlFile, RedoEntries, WorkspaceEntry};
    use bicdb_storage::datafile::DataFile;
    use bicdb_storage::heap::{self, InsertPolicy};
    use bicdb_storage::itl::{self, ItlEntry, ItlState};
    use bicdb_storage::page::{Page, PageType};
    use bicdb_storage::pagefile;
    use bicdb_storage::row::assemble_row;
    use bicdb_storage::rowid::RowId;
    use bicdb_storage::undo::{
        create_undo_segment, read_slot, TxnId, TxnState, UndoChain, UndoOp, UndoPayload,
    };
    use bicdb_workspace::id::WorkspaceId;
    use bicdb_workspace::io::MemFileIo;

    use super::*;
    use crate::analysis::pitr_stop;
    use crate::group::{online_groups, GroupSpec, GroupWriter};
    use crate::record::{page_diff, BlockRef, Change, Rdba, RedoRecord};

    const UNDO_F: &str = "/mem/undo1.dat";
    const DATA_F: &str = "/mem/data.dat";
    const A: &str = "/mem/control01.ctl";
    const B: &str = "/mem/control02.ctl";
    const WAL: &str = "/mem/wal";
    const WS: [u8; 8] = [6u8; 8];

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
    fn changes(before: &Page, after: &Page) -> Vec<Change> {
        page_diff(before.as_bytes(), after.as_bytes())
    }

    #[test]
    fn pitr_stop_is_the_last_commit_at_or_before_target() {
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
        w.append(|l| RedoRecord::commit(l, 1, 3)).unwrap();
        w.append(|l| RedoRecord::commit(l, 2, 9)).unwrap();
        w.flush(w.appended_lsn()).unwrap();
        let cf_ro = ControlFile::open(&io, Path::new(A), Path::new(B)).unwrap();
        let groups = online_groups(&io, &cf_ro, Path::new(WAL), spec).unwrap();

        let stop3 = pitr_stop(&io, &groups, lsn(0), 3).unwrap().unwrap();
        let stop9 = pitr_stop(&io, &groups, lsn(0), 9).unwrap().unwrap();
        assert!(stop3 < stop9);
        assert_eq!(
            pitr_stop(&io, &groups, lsn(0), 100).unwrap(),
            Some(stop9),
            "≥ 最大值 = 链尾"
        );
        assert_eq!(
            pitr_stop(&io, &groups, lsn(0), 1).unwrap(),
            None,
            "之前没有提交"
        );
    }

    /// **PITR 端到端**：目标点（seq 3）之前提交的 W 生效；从未提交的 L 与
    /// **目标点之后才提交**的 W2 都消失。
    #[test]
    fn recover_to_target_keeps_what_was_committed_by_then() {
        let io = mem();
        let spec = GroupSpec::new(2, 1, 64).unwrap();

        // ---- 夹具（undo 三槽 + 数据页初始空） ----
        let mut undo_file = DataFile::create(&io, Path::new(UNDO_F), 1, 1, WS, 512).unwrap();
        let undo_handle = undo_file.handle();
        let segment = create_undo_segment(&mut undo_file, 2, 3, 4).unwrap();
        let mut chain = UndoChain::open(segment);
        let w_slot = chain.allocate_slot().unwrap();
        let l_slot = chain.allocate_slot().unwrap();
        let w2_slot = chain.allocate_slot().unwrap();
        let w = TxnId::from_parts(0, w_slot as u8, 0);
        let l = TxnId::from_parts(0, l_slot as u8, 0);
        let w2 = TxnId::from_parts(0, w2_slot as u8, 0);

        let data = pagefile::create(&io, Path::new(DATA_F), 1).unwrap();
        let before = Page::new(PageType::HeapTable, WS, 3, 0);
        pagefile::write_page(&io, data, 0, &mut before.clone()).unwrap();

        // W：插入 row1，ITL[0]；提交在目标点之内（seq 3）。
        let mut after1 = before.clone();
        let bytes_w = assemble_row(0, 1, &[false], &[], &[b"w".as_slice()]).unwrap();
        let row_w = heap::insert_row(&mut after1, &bytes_w, &InsertPolicy::in_place(0)).unwrap();
        chain
            .append(
                w_slot,
                UndoOp::ItlOverwrite,
                0,
                RowId::from_parts(3, 0, 1).unwrap(),
                UndoPayload::ItlOverwrite {
                    txn_id: TxnId::from_parts(0, w_slot as u8, 0),
                    itl_slot: 0,
                    old: None,
                },
            )
            .unwrap();
        let head_w = chain
            .append(
                w_slot,
                UndoOp::Insert,
                0,
                RowId::from_parts(3, 0, row_w).unwrap(),
                UndoPayload::None,
            )
            .unwrap();
        itl::grow(&mut after1, 8).unwrap();
        itl::write_itl(
            &mut after1,
            0,
            &ItlEntry {
                txn_id: w,
                undo_ptr: Some(head_w),
                commit_seq: None,
                lock_cnt: 1,
                state: ItlState::Active,
            },
        )
        .unwrap();

        // L：插入 row2，ITL[1]；从不提交。
        let mut after2 = after1.clone();
        let bytes_l = assemble_row(0, 2, &[false], &[], &[b"l".as_slice()]).unwrap();
        let row_l = heap::insert_row(&mut after2, &bytes_l, &InsertPolicy::in_place(0)).unwrap();
        chain
            .append(
                l_slot,
                UndoOp::ItlOverwrite,
                0,
                RowId::from_parts(3, 0, 1).unwrap(),
                UndoPayload::ItlOverwrite {
                    txn_id: TxnId::from_parts(0, l_slot as u8, 0),
                    itl_slot: 1,
                    old: None,
                },
            )
            .unwrap();
        let head_l = chain
            .append(
                l_slot,
                UndoOp::Insert,
                0,
                RowId::from_parts(3, 0, row_l).unwrap(),
                UndoPayload::None,
            )
            .unwrap();
        itl::write_itl(
            &mut after2,
            1,
            &ItlEntry {
                txn_id: l,
                undo_ptr: Some(head_l),
                commit_seq: None,
                lock_cnt: 1,
                state: ItlState::Active,
            },
        )
        .unwrap();

        // W2：插入 row3，ITL[2]；**目标点之后**才提交（seq 9）。
        let mut after3 = after2.clone();
        let bytes_w2 = assemble_row(0, 3, &[false], &[], &[b"w2".as_slice()]).unwrap();
        let row_w2 = heap::insert_row(&mut after3, &bytes_w2, &InsertPolicy::in_place(0)).unwrap();
        itl::grow(&mut after3, 8).unwrap(); // ITL[2] 就位
        chain
            .append(
                w2_slot,
                UndoOp::ItlOverwrite,
                0,
                RowId::from_parts(3, 0, 1).unwrap(),
                UndoPayload::ItlOverwrite {
                    txn_id: TxnId::from_parts(0, w2_slot as u8, 0),
                    itl_slot: 2,
                    old: None,
                },
            )
            .unwrap();
        let head_w2 = chain
            .append(
                w2_slot,
                UndoOp::Insert,
                0,
                RowId::from_parts(3, 0, row_w2).unwrap(),
                UndoPayload::None,
            )
            .unwrap();
        itl::write_itl(
            &mut after3,
            2,
            &ItlEntry {
                txn_id: w2,
                undo_ptr: Some(head_w2),
                commit_seq: None,
                lock_cnt: 1,
                state: ItlState::Active,
            },
        )
        .unwrap();

        // 日志：mod(W) → commit(W,3) → mod(L) → mod(W2) → commit(W2,9)。
        let mut cf = ControlFile::format(
            &io,
            Path::new(A),
            Path::new(B),
            &ws_entry(),
            &RedoEntries::new(2, 1).unwrap(),
            &ArchiveRecord::default(),
        )
        .unwrap();
        {
            let mut writer =
                GroupWriter::create(&io, &mut cf, Path::new(WAL), spec, lsn(0)).unwrap();
            writer
                .append(|l| {
                    RedoRecord::page_modification(
                        l,
                        w.as_raw(),
                        vec![BlockRef {
                            flags: 0,
                            rdba: rdba(3, 0),
                            changes: changes(&before, &after1),
                        }],
                    )
                })
                .unwrap();
            writer
                .append(|l| RedoRecord::commit(l, w.as_raw(), 3))
                .unwrap();
            writer
                .append(|l| {
                    RedoRecord::page_modification(
                        l,
                        l.as_raw(),
                        vec![BlockRef {
                            flags: 0,
                            rdba: rdba(3, 0),
                            changes: changes(&after1, &after2),
                        }],
                    )
                })
                .unwrap();
            writer
                .append(|l| {
                    RedoRecord::page_modification(
                        l,
                        w2.as_raw(),
                        vec![BlockRef {
                            flags: 0,
                            rdba: rdba(3, 0),
                            changes: changes(&after2, &after3),
                        }],
                    )
                })
                .unwrap();
            writer
                .append(|l| RedoRecord::commit(l, w2.as_raw(), 9))
                .unwrap();
            writer.flush(writer.appended_lsn()).unwrap();
            writer.close().unwrap();
        }

        let cf_ro = ControlFile::open(&io, Path::new(A), Path::new(B)).unwrap();
        let groups = online_groups(&io, &cf_ro, Path::new(WAL), spec).unwrap();

        // ---- 按目标点恢复（target = seq 3） ----
        let mut cf = ControlFile::open(&io, Path::new(A), Path::new(B)).unwrap();
        let mut writer = GroupWriter::open(&io, &mut cf, Path::new(WAL), spec).unwrap();
        let mut resolve = |r: Rdba| match r.file_id() {
            1 => Some((undo_handle, r.block_id())),
            3 => Some((data, r.block_id())),
            _ => None,
        };
        let report = recover_to_target(
            &io,
            &groups,
            lsn(0),
            seq(3),
            &chain,
            &mut writer,
            &mut resolve,
        )
        .unwrap();

        let page = pagefile::read_page_verified(&io, data, 0).unwrap();
        assert_eq!(
            heap::row(&page, row_w),
            Some(&bytes_w[..]),
            "目标点前提交的保留"
        );
        assert_eq!(heap::row(&page, row_l), None, "从未提交的回滚");
        assert_eq!(heap::row(&page, row_w2), None, "目标点**之后**提交的回滚");

        let hdr = chain.segment().read_page(0).unwrap();
        let sw = read_slot(&hdr, w_slot).unwrap();
        assert_eq!(sw.state, TxnState::Committed, "W 补标记");
        assert_eq!(sw.commit_seq, seq(3));
        assert_eq!(read_slot(&hdr, l_slot).unwrap().state, TxnState::Free);
        assert_eq!(read_slot(&hdr, w2_slot).unwrap().state, TxnState::Free);
        assert_eq!(report.undo.txns_rolled_back, 2);
    }
}
