//! 恢复的**分析阶段**（§11.2 第 1 步；§4.6.6 ⑤ 的"与恢复的接口"）。
//!
//! 扫一遍从检查点起的日志流，把每个**出现过的事务**判一个结局：
//!
//! | 流里有记录 | 结局 | 恢复动作 |
//! | --- | --- | --- |
//! | `0x30 提交`（带 `commit_seq`） | **胜者** | 事务表槽**前滚补标记**（§4.6.6 ⑤） |
//! | `0x31 回滚完成` | 已回滚 | 无（撤销阶段不必再碰） |
//! | 只有页修改、无终局记录 | **输家** | 撤销阶段回滚 |
//!
//! **系统记录**（`txn_id = 0`：日志切换 / 检查点）不参与判定。
//!
//! # 为什么"补标记"是恢复的必须步骤
//!
//! 本设计采用**延迟块清除**（§11.1.1）：提交只写流、不碰数据块。于是一个
//! **已提交但未清除**的块，其 ITL 条目看起来仍是"活动"，可见性判定要靠
//! `txn_id` 回查事务表槽（§12.2 ②′）。若崩溃把**事务表槽的提交标记**
//! 也丢了（提交记录已入流、槽未落盘），不补标记，读者会把已提交事务
//! **当成未提交去撤销**——一致性读直接给出错误的历史。**所以"提交记录在则
//! 前滚补标记"不是优化，是正确性步骤**：它让 §12.2 的判定在恢复后成立。
//! （重做阶段把 undo 段头页的旧字节重放回来之后，本阶段再按日志里的
//! 提交记录把标记补齐——顺序不可颠倒。）
//!
//! # 输家不止"日志里出现的"
//!
//! 一个事务可能**在检查点之前就已活动**、此后一条记录都没写——它不会
//! 出现在本阶段的扫描结果里。因此**事务表槽扫描是输家集合的另一半**
//! （`Active` / `PendingRollback` 的槽，见 `bicdb_storage::undo` 的
//! `repair_committed_slots`）：两半取并集才是完整的输家集合。
//! 这也正是 Oracle 从 undo 段读事务表、而不是只信 redo 的原因。
//!
//! # 重做起点
//!
//! 本阶段不改"重做从哪开始"——V1.0 没有 BufferPool，重做起点一律取
//! 控制文件的检查点 LSN（低水位）；将来有了脏页表，起点可推进到
//! "最老脏页的 `page_lsn`"，那时本报告多带一张脏页表即可（纯增量）。

use std::collections::BTreeMap;

use bicdb_common::seq::Lsn;
use bicdb_workspace::io::FileIo;

use crate::file::scan_log;
use crate::group::OnlineGroup;
use crate::record::RecordOp;
use crate::recovery::RecoveryError;

/// 一个事务在日志流里的结局。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnOutcome {
    /// 提交记录在流中——"前滚补标记"的输入。
    Committed {
        /// 流中提交记录携带的提交序号。
        commit_seq: u64,
    },
    /// 回滚完成记录在流中——无需处置。
    RolledBack,
    /// 有活动痕迹、无终局记录——**输家**。
    Loser,
}

/// 分析阶段的执行报告。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalysisReport {
    /// 被扫描的组数（整组落在起点之前的组不计）。
    pub groups_scanned: usize,
    /// 被扫描的记录数（起点之前的记录不计）。
    pub records_scanned: u64,
    /// 日志末端（在线组已写前缀的尽头）= 重做终点 = **续写位置**。
    pub log_end: Lsn,
    /// 每个出现过的事务的结局（按 `txn_id` 排序）。
    pub txns: BTreeMap<u64, TxnOutcome>,
    /// 流中出现的最大提交序号——恢复后**当前提交序号**的起点。
    pub highest_commit_seq: u64,
}

impl Default for AnalysisReport {
    fn default() -> Self {
        Self {
            groups_scanned: 0,
            records_scanned: 0,
            log_end: Lsn::from_raw(0).expect("0 在 48 位域内"),
            txns: BTreeMap::new(),
            highest_commit_seq: 0,
        }
    }
}

/// **PITR 目标点 → 重放上界**（§11.10 的终止条件）：
/// 返回"**最后一条 `commit_seq ≤ target` 的提交记录**"的 LSN（含该条）——
/// 其后不再重放。范围内没有这样的提交 ⇒ `None`（不重放任何记录）。
pub fn pitr_stop(
    io: &dyn FileIo,
    groups: &[OnlineGroup],
    start_lsn: Lsn,
    target: u64,
) -> Result<Option<Lsn>, RecoveryError> {
    let mut stop = None;
    for g in groups {
        if g.end_lsn <= start_lsn {
            continue;
        }
        let scan = scan_log(io, g.handle, g.start_lsn, u64::from(g.file_pages))?;
        for record in &scan.records {
            if record.lsn < start_lsn {
                continue;
            }
            if RecordOp::from_u8(record.op) != Some(RecordOp::Commit) {
                continue;
            }
            let seq = record
                .commit_seq()
                .ok_or(RecoveryError::Malformed("提交记录缺 commit_seq 主段"))?;
            if seq <= target {
                // 提交序号随流单增——后面的只会更大，但取"最后一条"更防流损坏。
                stop = Some(record.lsn);
            }
        }
    }
    Ok(stop)
}

/// 墙钟目标点的解析错误（§11.10：超出采样窗口**明确报错**）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WallClockError {
    /// 采样对为空（还没打过检查点）。
    Empty,
    /// 目标早于采样窗口下界。
    BeforeWindow {
        /// 窗口下界（毫秒）。
        earliest: u64,
    },
    /// 目标晚于采样窗口上界。
    AfterWindow {
        /// 窗口上界（毫秒）。
        latest: u64,
    },
}

impl std::fmt::Display for WallClockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WallClockError::Empty => f.write_str("墙钟目标点：采样对为空（先取一次检查点）"),
            WallClockError::BeforeWindow { earliest } => {
                write!(f, "墙钟目标点早于采样窗口（下界 {earliest} ms）")
            }
            WallClockError::AfterWindow { latest } => {
                write!(f, "墙钟目标点晚于采样窗口（上界 {latest} ms）")
            }
        }
    }
}

impl std::error::Error for WallClockError {}

/// **墙钟 → 提交序号**（§11.10 的"采样对线性内插"）：`pairs` 为
/// （提交序号, 时刻毫秒）升序；落在窗口外**明确报错**。
pub fn resolve_wall_clock(
    pairs: &[(bicdb_common::seq::CommitSeq, u64)],
    target_ms: u64,
) -> Result<bicdb_common::seq::CommitSeq, WallClockError> {
    let Some(&(first_seq, first_ms)) = pairs.first() else {
        return Err(WallClockError::Empty);
    };
    if target_ms < first_ms {
        return Err(WallClockError::BeforeWindow { earliest: first_ms });
    }
    let (last_seq, last_ms) = *pairs.last().expect("非空");
    if target_ms > last_ms {
        return Err(WallClockError::AfterWindow { latest: last_ms });
    }
    for w in pairs.windows(2) {
        let (s0, t0) = w[0];
        let (s1, t1) = w[1];
        if target_ms >= t0 && target_ms <= t1 {
            if t1 <= t0 {
                return Ok(s1);
            }
            let span = u128::from(t1 - t0);
            let frac = u128::from(target_ms - t0);
            let delta = u128::from(s1.as_raw().saturating_sub(s0.as_raw()));
            let seq = u128::from(s0.as_raw()) + delta * frac / span;
            return Ok(bicdb_common::seq::CommitSeq::from_raw(seq as u64).unwrap_or(first_seq));
        }
    }
    Ok(last_seq)
}

impl AnalysisReport {
    /// 日志流里出现的输家（**不含**"检查点前就活动、之后无记录"的当事务——
    /// 那要由事务表槽扫描补齐，见模块文档）。
    #[must_use]
    pub fn losers(&self) -> Vec<u64> {
        self.txns
            .iter()
            .filter(|(_, o)| matches!(o, TxnOutcome::Loser))
            .map(|(id, _)| *id)
            .collect()
    }

    /// 日志流里已提交的事务（`txn_id` → `commit_seq`）。
    #[must_use]
    pub fn committed(&self) -> Vec<(u64, u64)> {
        self.txns
            .iter()
            .filter_map(|(id, o)| match o {
                TxnOutcome::Committed { commit_seq } => Some((*id, *commit_seq)),
                _ => None,
            })
            .collect()
    }
}

/// 记一个**终局**结局：覆盖"活动中"（Loser）或空缺；**已有终局则保留首个**
/// （同一 `txn_id` 在窗口内唯一——`wrap` 推进保证——重复只可能是流损坏）。
fn set_terminal(txns: &mut BTreeMap<u64, TxnOutcome>, txn_id: u64, outcome: TxnOutcome) {
    match txns.get(&txn_id) {
        Some(TxnOutcome::Committed { .. }) | Some(TxnOutcome::RolledBack) => {}
        _ => {
            txns.insert(txn_id, outcome);
        }
    }
}

/// **分析阶段**：扫描从 `start_lsn`（检查点 LSN）起的在线组，判定事务结局。
///
/// `groups` 必须来自 [`crate::group::online_groups`]（已按流顺序排好）。
pub fn analyze_from(
    io: &dyn FileIo,
    groups: &[OnlineGroup],
    start_lsn: Lsn,
) -> Result<AnalysisReport, RecoveryError> {
    analyze_until(io, groups, start_lsn, crate::recovery::ReplayBound::ToEnd)
}

/// **有界分析**（PITR 用）：只判到 `stop_lsn`（含）为止——目标点之后发生的
/// 提交**不得**被算进来（否则按目标点回滚时会把"当时还没提交的"当成胜者）。
pub fn analyze_until(
    io: &dyn FileIo,
    groups: &[OnlineGroup],
    start_lsn: Lsn,
    bound: crate::recovery::ReplayBound,
) -> Result<AnalysisReport, RecoveryError> {
    let mut report = AnalysisReport {
        log_end: start_lsn,
        ..AnalysisReport::default()
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
                continue; // 起点之前的记录：不判定
            }
            match bound {
                crate::recovery::ReplayBound::ToEnd => {}
                crate::recovery::ReplayBound::Through(stop) => {
                    if record.lsn > stop {
                        continue; // 目标点之后的记录：不判定（PITR）
                    }
                }
                crate::recovery::ReplayBound::Nothing => continue,
            }
            report.records_scanned += 1;
            match RecordOp::from_u8(record.op) {
                // **系统记录由 `op` 识别，不由 `txn_id` 识别**——第一个事务的
                // `txn_id` =（usn 0, slot 0, wrap 0）= 全零（§4.6.3 的合法身份，
                // 与"空槽"哨兵相撞是知名陷阱），若按 `txn_id == 0` 跳过，
                // 输家会**漏判**、其回滚会被跳过。
                Some(RecordOp::LogSwitch | RecordOp::Checkpoint) => {}
                Some(RecordOp::Commit) => {
                    let commit_seq = record
                        .commit_seq()
                        .ok_or(RecoveryError::Malformed("提交记录缺 commit_seq 主段"))?;
                    report.highest_commit_seq = report.highest_commit_seq.max(commit_seq);
                    set_terminal(
                        &mut report.txns,
                        record.txn_id,
                        TxnOutcome::Committed { commit_seq },
                    );
                }
                Some(RecordOp::RollbackDone) => {
                    set_terminal(&mut report.txns, record.txn_id, TxnOutcome::RolledBack);
                }
                // 页修改（与未知 op——原样保留的标签，按"有活动痕迹"处理）
                _ => {
                    report
                        .txns
                        .entry(record.txn_id)
                        .or_insert(TxnOutcome::Loser);
                }
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use bicdb_common::seq::CommitSeq;
    use bicdb_storage::controlfile::{ArchiveRecord, ControlFile, RedoEntries, WorkspaceEntry};
    use bicdb_workspace::id::WorkspaceId;
    use bicdb_workspace::io::MemFileIo;

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

    fn mod_rec(lsn_v: Lsn, txn: u64, r: Rdba, byte: u8) -> RedoRecord {
        RedoRecord::page_modification(
            lsn_v,
            txn,
            vec![BlockRef {
                flags: 0,
                rdba: r,
                changes: vec![Change {
                    offset: 4096,
                    after: vec![byte],
                }],
            }],
        )
    }

    /// 写一条"活动 → 提交 → 回滚完成 → 活动无终局"的流（含组激活的切换记录
    /// 与一条检查点记录），返回组列表与 **txn 11 提交之后**那条记录的 LSN
    /// （用于"起点之前跳过"的用例）。
    fn stream(io: &dyn FileIo) -> (Vec<OnlineGroup>, Lsn) {
        let spec = GroupSpec::new(4, 1, 64).unwrap();
        let mut cf = ControlFile::format(
            io,
            Path::new(A),
            Path::new(B),
            &ws_entry(),
            &RedoEntries::new(4, 1).unwrap(),
            &ArchiveRecord::default(),
        )
        .unwrap();
        let mut w = GroupWriter::create(io, &mut cf, Path::new(WAL), spec, lsn(0)).unwrap();
        let r = rdba(1, 0);
        w.append(|l| mod_rec(l, 11, r, 0xA1)).unwrap();
        w.append(|l| mod_rec(l, 22, r, 0xA2)).unwrap();
        w.append(|l| RedoRecord::commit(l, 11, 7)).unwrap();
        let after_commit = w.append(|l| RedoRecord::rollback_done(l, 22)).unwrap();
        w.append(|l| mod_rec(l, 33, r, 0xA3)).unwrap();
        // 系统记录：不参与判定。
        w.append(|l| RedoRecord::checkpoint(l, 6, 0, 7, 0)).unwrap();
        w.flush(w.appended_lsn()).unwrap();
        w.close().unwrap();

        let cf = ControlFile::open(io, Path::new(A), Path::new(B)).unwrap();
        let groups = online_groups(io, &cf, Path::new(WAL), spec).unwrap();
        (groups, after_commit)
    }

    #[test]
    fn classifies_winners_rollbacks_and_losers() {
        let io = mem();
        let (groups, _after_commit) = stream(&io);
        let report = analyze_from(&io, &groups, lsn(0)).unwrap();

        assert_eq!(report.groups_scanned, groups.len());
        // 7 = 组激活的切换记录 + 6 条（切换记录是系统记录，占一条但不产生条目）。
        assert_eq!(report.records_scanned, 7);
        assert_eq!(report.log_end, groups.last().unwrap().end_lsn);
        assert_eq!(
            report.txns.get(&11),
            Some(&TxnOutcome::Committed { commit_seq: 7 })
        );
        assert_eq!(report.txns.get(&22), Some(&TxnOutcome::RolledBack));
        assert_eq!(report.txns.get(&33), Some(&TxnOutcome::Loser));
        assert_eq!(report.txns.len(), 3, "系统记录不产生事务条目");
        assert_eq!(report.losers(), vec![33]);
        assert_eq!(report.committed(), vec![(11, 7)]);
        assert_eq!(report.highest_commit_seq, 7);
    }

    #[test]
    fn start_lsn_skips_earlier_records() {
        let io = mem();
        let (groups, after_commit) = stream(&io);
        // 从 txn 22 的回滚完成记录起：txn 11 的两条（页修改与提交）都在
        // 起点之前，不再判定；22 的终局记录与 33 的活动记录仍在范围内。
        let report = analyze_from(&io, &groups, after_commit).unwrap();

        assert!(!report.txns.contains_key(&11), "起点之前的记录不判定");
        assert_eq!(report.txns.get(&22), Some(&TxnOutcome::RolledBack));
        assert_eq!(report.txns.get(&33), Some(&TxnOutcome::Loser));
        assert_eq!(report.highest_commit_seq, 0, "提交记录在起点之前");
        // 起点起：回滚完成 + 页修改 + 检查点 = 3 条。
        assert_eq!(report.records_scanned, 3);
    }

    #[test]
    fn zero_txn_id_is_a_real_transaction_not_a_system_record() {
        let io = mem();
        let spec = GroupSpec::new(4, 1, 64).unwrap();
        let mut cf = ControlFile::format(
            &io,
            Path::new(A),
            Path::new(B),
            &ws_entry(),
            &RedoEntries::new(4, 1).unwrap(),
            &ArchiveRecord::default(),
        )
        .unwrap();
        let mut w = GroupWriter::create(&io, &mut cf, Path::new(WAL), spec, lsn(0)).unwrap();
        // 第一个事务：txn_id =（usn 0, slot 0, wrap 0）= 全零。
        w.append(|l| mod_rec(l, 0, rdba(1, 0), 0x01)).unwrap();
        w.flush(w.appended_lsn()).unwrap();
        w.close().unwrap();

        let cf = ControlFile::open(&io, Path::new(A), Path::new(B)).unwrap();
        let groups = online_groups(&io, &cf, Path::new(WAL), spec).unwrap();
        let report = analyze_from(&io, &groups, lsn(0)).unwrap();
        assert_eq!(
            report.txns.get(&0),
            Some(&TxnOutcome::Loser),
            "全零 txn_id 是第一个事务——按 `op` 判系统记录，不按 0 判"
        );
        assert_eq!(report.losers(), vec![0]);
    }

    #[test]
    fn malformed_commit_is_reported() {
        let io = mem();
        let spec = GroupSpec::new(4, 1, 64).unwrap();
        let mut cf = ControlFile::format(
            &io,
            Path::new(A),
            Path::new(B),
            &ws_entry(),
            &RedoEntries::new(4, 1).unwrap(),
            &ArchiveRecord::default(),
        )
        .unwrap();
        let mut w = GroupWriter::create(&io, &mut cf, Path::new(WAL), spec, lsn(0)).unwrap();
        w.append(|l| RedoRecord {
            lsn: l,
            txn_id: 99,
            op: RecordOp::Commit.as_u8(),
            blocks: Vec::new(),
            main: Vec::new(), // 缺 commit_seq 主段
        })
        .unwrap();
        w.flush(w.appended_lsn()).unwrap();
        w.close().unwrap();

        let cf = ControlFile::open(&io, Path::new(A), Path::new(B)).unwrap();
        let groups = online_groups(&io, &cf, Path::new(WAL), spec).unwrap();
        assert!(matches!(
            analyze_from(&io, &groups, lsn(0)),
            Err(RecoveryError::Malformed(_))
        ));
    }

    #[test]
    fn wall_clock_interpolates_and_bounds_the_window() {
        use bicdb_common::seq::CommitSeq;
        let seq = |v: u64| CommitSeq::from_raw(v).unwrap();
        let pairs = vec![(seq(10), 1_000u64), (seq(20), 2_000)];
        assert_eq!(
            resolve_wall_clock(&pairs, 1_000).unwrap(),
            seq(10),
            "下界精确"
        );
        assert_eq!(
            resolve_wall_clock(&pairs, 2_000).unwrap(),
            seq(20),
            "上界精确"
        );
        assert_eq!(
            resolve_wall_clock(&pairs, 1_500).unwrap(),
            seq(15),
            "中点线性内插"
        );
        assert_eq!(resolve_wall_clock(&pairs, 1_250).unwrap(), seq(12));
        assert!(matches!(
            resolve_wall_clock(&pairs, 999),
            Err(WallClockError::BeforeWindow { earliest: 1_000 })
        ));
        assert!(matches!(
            resolve_wall_clock(&pairs, 2_001),
            Err(WallClockError::AfterWindow { latest: 2_000 })
        ));
        assert!(matches!(
            resolve_wall_clock(&[], 1),
            Err(WallClockError::Empty)
        ));
    }
}
