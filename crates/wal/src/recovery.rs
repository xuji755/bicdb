//! 恢复的**重做阶段**驱动（§11.2 第 2 步）：从检查点 LSN 起，按**日志流顺序**
//! 扫描在线组、逐条应用（[`crate::apply`]）。
//!
//! ```text
//! ① 分析：从最近检查点重建事务状态表与脏页表，确定重做起点
//! ② 重做：从重做起点重放；按 page_lsn 判定已包含的修改并跳过（幂等）   ← 本模块
//! ③ 撤销：回滚未提交事务
//! ```
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
}

impl std::fmt::Display for RecoveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecoveryError::Io(e) => write!(f, "恢复 I/O：{e}"),
            RecoveryError::File(e) => write!(f, "恢复扫描：{e}"),
            RecoveryError::Apply(e) => write!(f, "恢复应用：{e}"),
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
            report.records_applied += 1;
            let outcome = apply_record(io, record, resolver)?;
            report.applied_blocks += outcome.applied;
            report.skipped_blocks += outcome.skipped;
        }
    }
    Ok(report)
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
