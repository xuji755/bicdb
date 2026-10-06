//! **文件一致性核对：`file_scn` ↔ 控制文件检查点**（`目录详设` §2.5；C2c）。
//!
//! ```text
//! 控制文件：CheckpointProgress.checkpoint_lsn   ← 低水位（该 LSN 之前全部已落盘）
//! 文件头  ：file_scn（LSN 原值）                ← 本文件最新持久化位点
//! ```
//!
//! 两者是磁盘上**独立存在**的事实（纪律 7 的对偶：不设落盘 `file$`），
//! **比对即得判定**——本模块就是那张判定表的可执行形态：
//!
//! | `file_scn` vs 检查点 | 判定 | 动作 |
//! | --- | --- | --- |
//! | 相等 | [`FileVerdict::Consistent`] | 正常打开（该文件无需重放） |
//! | 落后、且在日志链内 | [`FileVerdict::Replay`] | 自 `file_scn` 起重放 redo **+ 告警** |
//! | 落后、越出日志链 | [`FileVerdict::MediaRecovery`] | 备份 + 归档恢复 **+ 告警** |
//! | 超前 | [`FileVerdict::Ahead`] | **拒绝打开**（配错文件/换过控制文件） |
//! | 头读不出 | [`FileVerdict::HeadUnreadable`] | 先走 §2.3 自愈/重建，**再回到本表** |
//! | `role = temp`（file 2） | [`FileVerdict::SkippedTemp`] | 不参与（打开即整体重置，`arch/03` §4.8） |
//!
//! **"日志链起点"从哪来**：在线组的最小 `start_lsn`（[`bicdb_wal`] 侧的
//! `OnlineGroup::start_lsn`）——重放只能覆盖日志链内；越出链条（组被覆盖/
//! 归档缺失）就只能走介质恢复。本模块**不自己找日志**，由调用方把
//! `chain_start` 传进来（保持本模块是**纯判定**，可离线体检复用）。
//!
//! **告警不静默**（设计原话）：凡非"相等"的判定都记一条 [`Finding`]，
//! 由调用方决定呈现（启动日志 / `db_check` 报告）。

use bicdb_common::seq::Lsn;
use bicdb_storage::datafile::{DataFile, TEMP_FILE_ROLE};

/// 一条发现（告警/拒绝理由；稳定代码供脚本断言）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// 稳定代码（`FILE_SCN_*`；回归与运维脚本按它断言）。
    pub code: &'static str,
    /// 文件号（0 = file 0）。
    pub file_id: u16,
    /// 说明（含两个位点的实值——"控制文件说 X、文件头说 Y"）。
    pub detail: String,
}

impl std::fmt::Display for Finding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] file {} — {}", self.code, self.file_id, self.detail)
    }
}

/// **一个文件的位点事实**（核对的最小输入——离线体检时可脱离 `DataFile` 构造）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilePoint {
    /// 头可读且自洽：`role` 与 `file_scn` 从文件头来。
    Readable {
        /// 文件号。
        file_id: u16,
        /// 文件角色（§2.2）。
        role: u8,
        /// 文件头里的持久化位点（LSN 原值）。
        file_scn: u64,
    },
    /// 头**读不出/校验失败**——按 §2.5 第 4 行，先走 §2.3 自愈/重建。
    HeadUnreadable {
        /// 文件号。
        file_id: u16,
        /// 原因（诊断文本）。
        why: String,
    },
}

impl FilePoint {
    /// 从已打开的数据文件取事实。
    #[must_use]
    pub fn of(file: &DataFile<'_>) -> Self {
        Self::Readable {
            file_id: file.file_id(),
            role: file.role(),
            file_scn: file.file_scn(),
        }
    }

    /// 文件号。
    #[must_use]
    pub fn file_id(&self) -> u16 {
        match self {
            Self::Readable { file_id, .. } | Self::HeadUnreadable { file_id, .. } => *file_id,
        }
    }
}

/// **单文件判定**（§2.5 判定表逐行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileVerdict {
    /// 与检查点一致（干净落盘点）——正常打开，无需重放该文件。
    Consistent,
    /// 落后但在**日志链内**：自 `from` 起重放 redo（正常恢复；幂等，重复重放安全）。
    Replay {
        /// 重放起点（= 该文件的 `file_scn`）。
        from: u64,
    },
    /// 落后且**越出日志链**：备份 + 归档的介质恢复路径。
    MediaRecovery {
        /// 文件声称的位点（恢复需要比它更早的日志）。
        from: u64,
    },
    /// **超前**：文件比控制文件新——配错/拷贝错 ⇒ 拒绝打开。
    Ahead {
        /// 文件头位点。
        file_scn: u64,
        /// 检查点位点。
        checkpoint_lsn: u64,
    },
    /// 头读不出/校验失败（`file_scn` 越出 48 位域也算——头已不可信）。
    HeadUnreadable {
        /// 原因。
        why: String,
    },
    /// `role = temp`：不参与核对。
    SkippedTemp,
}

impl FileVerdict {
    /// 是否要求**拒绝打开**（只有"超前"一档）。
    #[must_use]
    pub fn is_refusal(&self) -> bool {
        matches!(self, Self::Ahead { .. })
    }

    /// 该文件需重放的起点（不需要重放则 `None`）。
    #[must_use]
    pub fn replay_from(&self) -> Option<u64> {
        match self {
            Self::Replay { from } | Self::MediaRecovery { from } => Some(*from),
            _ => None,
        }
    }
}

/// 一份文件 + 其判定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileCheck {
    /// 文件号。
    pub file_id: u16,
    /// 判定。
    pub verdict: FileVerdict,
}

/// **核对报告**（整体结论 + 逐文件判定 + 发现清单）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsistencyReport {
    /// 逐文件判定（与输入**同序**）。
    pub files: Vec<FileCheck>,
    /// 告警/拒绝理由（凡非"相等"都有一条；不是"失败"——是"不静默"）。
    pub findings: Vec<Finding>,
    /// 是否**拒绝打开**（任一文件超前）。
    pub refused: bool,
    /// 恢复起点：全部落后文件的**最小 `file_scn`**（无落后 ⇒ `None`）。
    ///
    /// 重放自最落后的那个文件起（redo 靠 `page_lsn` 判重 ⇒ 起点早一点是安全的）。
    pub replay_from: Option<u64>,
    /// 是否有文件需走**介质恢复**（越出日志链）——opening 的正常恢复覆盖不了。
    pub needs_media_recovery: bool,
}

impl ConsistencyReport {
    /// 全部一致（无任何发现）。
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }

    /// 判定按文件号取（诊断用）。
    #[must_use]
    pub fn verdict_of(&self, file_id: u16) -> Option<&FileVerdict> {
        self.files
            .iter()
            .find(|f| f.file_id == file_id)
            .map(|f| &f.verdict)
    }
}

/// **执行判定表**（§2.5，对每个非 temp 文件逐一）。
///
/// - `checkpoint_lsn`：控制文件 `CheckpointProgress.checkpoint_lsn`（低水位）；
/// - `chain_start`：**日志链起点** = 在线组的最小 `start_lsn`；
///   `None` = 无可用日志（任何落后都只能介质恢复）；
/// - `points`：各文件的位点事实（含头读不出的那些——它们也要出现在报告里）。
///
/// **纯函数**：不碰 IO、不找日志——打开链把事实读好后调它一次。
#[must_use]
pub fn check_files(
    checkpoint_lsn: Lsn,
    chain_start: Option<Lsn>,
    points: &[FilePoint],
) -> ConsistencyReport {
    let mut files = Vec::with_capacity(points.len());
    let mut findings = Vec::new();
    let mut refused = false;
    let mut replay_from: Option<u64> = None;
    let mut needs_media_recovery = false;

    for p in points {
        let file_id = p.file_id();
        let verdict = match p {
            FilePoint::HeadUnreadable { why, .. } => {
                findings.push(Finding {
                    code: "FILE_HEAD_UNREADABLE",
                    file_id,
                    detail: format!("文件头不可信（{why}）——先走副本带自愈/重建，再回到 §2.5 核对"),
                });
                FileVerdict::HeadUnreadable { why: why.clone() }
            }
            FilePoint::Readable { role, file_scn, .. } => {
                if *role == TEMP_FILE_ROLE {
                    FileVerdict::SkippedTemp
                } else {
                    match Lsn::from_raw(*file_scn) {
                        None => {
                            // 头里的位点越出 48 位域 ⇒ 头本身已不可信（腐坏），
                            // 与"读不出头"同一处置：自愈/重建后再核对。
                            let why = format!("file_scn {file_scn} 越出 48 位域（头腐坏）");
                            findings.push(Finding {
                                code: "FILE_SCN_OUT_OF_DOMAIN",
                                file_id,
                                detail: why.clone(),
                            });
                            FileVerdict::HeadUnreadable { why }
                        }
                        Some(scn) => {
                            if scn == checkpoint_lsn {
                                FileVerdict::Consistent
                            } else if scn > checkpoint_lsn {
                                refused = true;
                                findings.push(Finding {
                                    code: "FILE_SCN_AHEAD",
                                    file_id,
                                    detail: format!(
                                        "文件头位点 {} 超前于控制文件检查点 {}——配错文件/控制文件不配对，拒绝打开",
                                        scn.as_raw(),
                                        checkpoint_lsn.as_raw()
                                    ),
                                });
                                FileVerdict::Ahead {
                                    file_scn: scn.as_raw(),
                                    checkpoint_lsn: checkpoint_lsn.as_raw(),
                                }
                            } else {
                                // 落后：链内可重放，越出链条走介质恢复。
                                let replayable = chain_start.is_some_and(|c| scn >= c);
                                if replayable {
                                    findings.push(Finding {
                                        code: "FILE_SCN_LAGGING",
                                        file_id,
                                        detail: format!(
                                            "文件头位点 {} 落后于检查点 {}——自 {} 起重放 redo",
                                            scn.as_raw(),
                                            checkpoint_lsn.as_raw(),
                                            scn.as_raw()
                                        ),
                                    });
                                    replay_from = Some(
                                        replay_from.map_or(scn.as_raw(), |m| m.min(scn.as_raw())),
                                    );
                                    FileVerdict::Replay { from: scn.as_raw() }
                                } else {
                                    needs_media_recovery = true;
                                    findings.push(Finding {
                                        code: "FILE_SCN_OUT_OF_CHAIN",
                                        file_id,
                                        detail: format!(
                                            "文件头位点 {} 越出日志链起点（{}）——走介质恢复（备份 + 归档）",
                                            scn.as_raw(),
                                            chain_start.map_or_else(
                                                || "无可用日志".to_owned(),
                                                |c| c.as_raw().to_string()
                                            )
                                        ),
                                    });
                                    FileVerdict::MediaRecovery { from: scn.as_raw() }
                                }
                            }
                        }
                    }
                }
            }
        };
        files.push(FileCheck { file_id, verdict });
    }

    ConsistencyReport {
        files,
        findings,
        refused,
        replay_from,
        needs_media_recovery,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bicdb_storage::bitmap::{FileLayout, META_ROLE};
    use bicdb_storage::datafile::DataFile;
    use bicdb_workspace::io::MemFileIo;
    use std::path::Path;

    fn lsn(v: u64) -> Lsn {
        Lsn::from_raw(v).unwrap()
    }

    fn readable(file_id: u16, role: u8, scn: u64) -> FilePoint {
        FilePoint::Readable {
            file_id,
            role,
            file_scn: scn,
        }
    }

    #[test]
    fn equal_is_clean_and_needs_no_replay() {
        let r = check_files(
            lsn(8192),
            Some(lsn(0)),
            &[readable(0, META_ROLE, 8192), readable(3, 3, 8192)],
        );
        assert!(r.is_clean(), "{:?}", r.findings);
        assert!(!r.refused && !r.needs_media_recovery && r.replay_from.is_none());
        assert_eq!(r.verdict_of(0), Some(&FileVerdict::Consistent));
        assert_eq!(r.verdict_of(3), Some(&FileVerdict::Consistent));
    }

    #[test]
    fn lagging_inside_the_chain_replays_and_warns() {
        let r = check_files(
            lsn(9000),
            Some(lsn(4096)),
            &[readable(0, META_ROLE, 9000), readable(4, 3, 6144)],
        );
        assert!(!r.is_clean(), "落后必须告警（不静默）");
        assert_eq!(r.verdict_of(4), Some(&FileVerdict::Replay { from: 6144 }));
        assert_eq!(r.replay_from, Some(6144), "恢复起点 = 最落后的位点");
        assert!(!r.refused && !r.needs_media_recovery);
        assert_eq!(r.findings.len(), 1);
        assert_eq!(r.findings[0].code, "FILE_SCN_LAGGING");
        assert!(
            r.findings[0].detail.contains("9000"),
            "{}",
            r.findings[0].detail
        );
    }

    #[test]
    fn replay_start_is_the_minimum_across_lagging_files() {
        let r = check_files(
            lsn(9000),
            Some(lsn(1000)),
            &[
                readable(0, META_ROLE, 8000),
                readable(3, 3, 5000),
                readable(4, 3, 7000),
            ],
        );
        assert_eq!(r.replay_from, Some(5000));
        assert_eq!(r.verdict_of(3), Some(&FileVerdict::Replay { from: 5000 }));
    }

    #[test]
    fn lagging_outside_the_chain_goes_to_media_recovery() {
        // 链起点 4096，文件位点 1000：重放覆盖不到。
        let r = check_files(lsn(9000), Some(lsn(4096)), &[readable(5, 3, 1000)]);
        assert_eq!(
            r.verdict_of(5),
            Some(&FileVerdict::MediaRecovery { from: 1000 })
        );
        assert!(r.needs_media_recovery);
        assert_eq!(r.findings[0].code, "FILE_SCN_OUT_OF_CHAIN");
        // 无可用日志：任何落后都是介质恢复。
        let r2 = check_files(lsn(9000), None, &[readable(5, 3, 1000)]);
        assert!(r2.needs_media_recovery);
        assert!(r2.findings[0].detail.contains("无可用日志"));
    }

    #[test]
    fn ahead_refuses_the_open() {
        let r = check_files(lsn(8192), Some(lsn(0)), &[readable(0, META_ROLE, 9000)]);
        assert!(r.refused, "超前必须拒绝");
        assert_eq!(
            r.verdict_of(0),
            Some(&FileVerdict::Ahead {
                file_scn: 9000,
                checkpoint_lsn: 8192
            })
        );
        assert_eq!(r.findings[0].code, "FILE_SCN_AHEAD");
        assert!(r.replay_from.is_none());
    }

    #[test]
    fn temp_file_is_skipped_even_when_it_looks_ahead() {
        let r = check_files(
            lsn(8192),
            Some(lsn(0)),
            &[readable(2, TEMP_FILE_ROLE, 999_999)],
        );
        assert_eq!(r.verdict_of(2), Some(&FileVerdict::SkippedTemp));
        assert!(r.is_clean() && !r.refused, "temp 不参与核对，绝不致拒绝");
    }

    #[test]
    fn unreadable_head_and_corrupt_scn_route_to_healing() {
        let r = check_files(
            lsn(8192),
            Some(lsn(0)),
            &[
                FilePoint::HeadUnreadable {
                    file_id: 4,
                    why: "两层完整性失败".to_owned(),
                },
                readable(5, 3, 1 << 60), // 越出 48 位域
            ],
        );
        assert!(matches!(
            r.verdict_of(4),
            Some(&FileVerdict::HeadUnreadable { .. })
        ));
        assert!(matches!(
            r.verdict_of(5),
            Some(&FileVerdict::HeadUnreadable { .. })
        ));
        assert!(!r.refused, "头腐坏不是拒绝，是自愈路径");
        assert_eq!(r.findings[0].code, "FILE_HEAD_UNREADABLE");
        assert_eq!(r.findings[1].code, "FILE_SCN_OUT_OF_DOMAIN");
    }

    #[test]
    fn takes_its_facts_from_a_real_file_header() {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let mut file = DataFile::create(
            &io,
            Path::new("/mem/k1.dat"),
            3,
            3,
            [9u8; 8],
            FileLayout::standard().min_file_blocks(),
        )
        .unwrap();
        // 刚建：file_scn = 0（尚未推进）。
        assert_eq!(FilePoint::of(&file), readable(3, 3, 0));
        file.set_file_scn(4096).unwrap();
        let point = FilePoint::of(&file);
        assert_eq!(point, readable(3, 3, 4096));
        // 该文件落后于检查点 8192 ⇒ 可重放。
        let r = check_files(lsn(8192), Some(lsn(0)), &[point]);
        assert_eq!(r.verdict_of(3), Some(&FileVerdict::Replay { from: 4096 }));
        assert_eq!(FilePoint::of(&file).file_id(), 3);
    }
}
