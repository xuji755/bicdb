//! 日志组与切换：**多组轮换、序列号推进、切换记录与状态发布**
//! （存储架构 §11.9 / §11.5.1）。
//!
//! # 一条流、多个文件
//!
//! LSN 是**日志流的字节位置**——跨组连续。组与组在**页边界**相接：切换时
//! 旧组就地收尾（保留最后一个部分页），新组从下一张页的起点开始。
//! 每组的第一个字节位置写**切换记录**（`0x01`，组号 1B │ 新序列号 4B）——
//! 组序与序列号因此在日志流内**自描述**（恢复扫描即可重建）。
//!
//! # 切换协议（§11.9 步骤 1–4）
//!
//! 1. **触发**：当前组放不下下一条记录时**提前收尾**（记录不得跨组——
//!    用 [`LogBuffer::end_lsn_if_appended`] 精确预检）；或**强制切换**
//!    （管理操作：备份 / PITR / 克隆前、优雅关闭）。
//! 2. **选组**：从当前组的下一组起轮转，找**可复用**组——
//!    （`INACTIVE` ∨ `UNUSED`）∧（非归档模式 ∨ 归档 `DONE`）。
//!    无可复用组 ⇒ 按缺的条件分类暂停（[`SwitchBlocked`]）。
//! 3. 刷尽旧组 → 在**新组第一条位置**写切换记录并 **sync** → 发布控制文件
//!    （新组 `CURRENT`；旧组 `ACTIVE`，归档模式下归档轴 → `NEEDED`）。
//!    次序不能反：**切换记录先于 `CURRENT` 发布**——否则崩溃后控制文件
//!    指认的当前组却没有自述序列号（链断）。
//! 4. **降级**（[`GroupWriter::publish_checkpoint`]）：把"组结尾 LSN ≤
//!    检查点低水位"的 `ACTIVE` 组降为 `INACTIVE`，**与低水位同一次控制
//!    文件更新**发布（§11.9 的发布纪律：降级不得先于低水位独立落盘）。
//!
//! # 重开与续写
//!
//! [`GroupWriter::open`] 扫描**已用过的组**（含当前组）：当前组的写位置 =
//! 已刷前缀的下一页边界——**已刷出的页不再改写**（重开会丢弃最后一个
//! 部分页的尾部空位，这是"页序列永远是文件前缀"的代价与保证）。
//!
//! # 多成员镜像（已实现；`member_count ≥ 1`）
//!
//! 扇出写全部成员 → **写失败的成员标 `STALE`**（`member_stale` 位）→
//! 刷盘跳过 STALE 成员（全坏才报 `Damaged`）→ [`GroupWriter::rebuild_member`]
//! 从健康成员复制已用前缀并清位。位记在**控制文件的 Reo 条目**里（redo 文件
//! 是纯页流、无文件头页——§11.9 原写"记在成员文件头页"，落点按本实现修正）。
//!
//! **发布纪律**：标脏可能发生在**没有控制文件的线程**（LGWR/池的 `WalGuard`
//! 走 [`WalShared`]）——位先记在共享态并置 `stale_dirty`，由**下一次前台
//! 发布**（`flush` / `publish_checkpoint`）取走并落控制文件（`take_stale_dirty`）。

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use bicdb_common::latch::{Latch, LatchGuard};
use bicdb_common::seq::{CommitSeq, Lsn};
use bicdb_storage::controlfile::{
    ArchiveMode, CheckpointProgress, ControlFile, ControlFileError, LogArchiveState, LogRunState,
    RedoEntries, RedoGroup, MAX_REDO_GROUPS, NO_CURRENT_GROUP,
};
use bicdb_workspace::io::{FileHandle, FileIo, OpenOptions};

use bicdb_storage::buffer::WalGuard;

use crate::buffer::{LogBuffer, LogSink, WalError};
use crate::file::{scan_log, FileLogSink, LogFileError};
use crate::logpage::{LogPage, LOG_PAGE_SIZE};
use crate::record::RedoRecord;

/// 单条记录的最坏占用（17 KiB：16417 B 内容 → 34 页；§11.5.2）。
pub const MAX_RECORD_FOOTPRINT: u64 = 34 * LOG_PAGE_SIZE as u64;

/// 组集规格。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupSpec {
    /// 组数（2..=8；**至少 2 组轮换**，§2.4）。
    pub group_count: u8,
    /// 成员数（本切片仅支持 1；2 份镜像随后）。
    pub member_count: u8,
    /// 每组成员大小（页数）。**容量下限 = [`MAX_RECORD_FOOTPRINT`]（34 页）**；
    /// 更小的组仍可运行，但放不下的记录会被明确拒绝
    /// （[`GroupError::RecordTooLarge`]）。
    pub group_pages: u32,
}

impl GroupSpec {
    /// 构造并校验。
    pub fn new(group_count: u8, member_count: u8, group_pages: u32) -> Result<Self, GroupError> {
        if !(2..=MAX_REDO_GROUPS as u8).contains(&group_count) {
            return Err(GroupError::Spec("组数应在 2..=8（至少 2 组轮换）"));
        }
        if member_count == 0 || member_count > 8 {
            return Err(GroupError::Spec("成员数应在 1..=8"));
        }
        if group_pages == 0 {
            return Err(GroupError::Spec("每组至少 1 页"));
        }
        Ok(Self {
            group_count,
            member_count,
            group_pages,
        })
    }

    /// 组字节大小。
    #[must_use]
    pub const fn group_bytes(&self) -> u64 {
        self.group_pages as u64 * LOG_PAGE_SIZE as u64
    }
}

/// 组文件命名（§2.1 的 `wal/` 目录）：`redo_g<组号>_m<成员号>`（均从 1 起）。
#[must_use]
pub fn member_file_name(group: u8, member: u8) -> String {
    format!("redo_g{}_m{}", group + 1, member + 1)
}

/// **成员镜像的写扇出**：一页写给全部成员；某成员失败即标记并跳过它，
/// **全坏才整体失败**（§11.9：镜像掉一个降级继续，掉光才是故障）。
struct TeeSink<'io> {
    sinks: Vec<FileLogSink<'io>>,
    failed: Vec<bool>,
}

impl LogSink for TeeSink<'_> {
    fn append_page(&mut self, page: &LogPage) -> std::io::Result<()> {
        let mut ok = 0;
        let mut last = None;
        for (i, sink) in self.sinks.iter_mut().enumerate() {
            if self.failed[i] {
                continue;
            }
            match sink.append_page(page) {
                Ok(()) => ok += 1,
                Err(e) => {
                    self.failed[i] = true;
                    last = Some(e);
                }
            }
        }
        if ok == 0 {
            Err(last.unwrap_or_else(|| std::io::Error::other("全部成员写入失败")))
        } else {
            Ok(())
        }
    }

    fn sync(&mut self) -> std::io::Result<()> {
        let mut ok = 0;
        let mut last = None;
        for (i, sink) in self.sinks.iter_mut().enumerate() {
            if self.failed[i] {
                continue;
            }
            match sink.sync() {
                Ok(()) => ok += 1,
                Err(e) => {
                    self.failed[i] = true;
                    last = Some(e);
                }
            }
        }
        if ok == 0 {
            Err(last.unwrap_or_else(|| std::io::Error::other("全部成员 sync 失败")))
        } else {
            Ok(())
        }
    }
}

/// 切换受阻（写者暂停的两类原因，§11.9 的"等待与告警"）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchBlocked {
    /// 组已写满且**未归档**——对应 `log file switch (archiving needed)`。
    AwaitingArchive,
    /// 已归档（或非归档模式）但**检查点未越过**——对应
    /// `log file switch (checkpoint incomplete)`。
    AwaitingCheckpoint,
}

impl std::fmt::Display for SwitchBlocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SwitchBlocked::AwaitingArchive => f.write_str("日志切换等待归档（无可复用组：未归档）"),
            SwitchBlocked::AwaitingCheckpoint => {
                f.write_str("日志切换等待检查点（无可复用组：检查点未越过）")
            }
        }
    }
}

/// 日志组错误。
#[derive(Debug)]
pub enum GroupError {
    /// 底层 I/O。
    Io(io::Error),
    /// 控制文件错误。
    ControlFile(ControlFileError),
    /// 日志缓冲/分页错误。
    Buffer(WalError),
    /// 日志文件错误。
    File(LogFileError),
    /// 规格不合法，或与控制文件里的组集不符。
    Spec(&'static str),
    /// 控制文件没有当前组（尚未首次激活）。
    NotActivated,
    /// 组内容损坏（中部坏页 / 当前组无内容等）。
    Damaged {
        /// 组号（0 起）。
        group: u8,
        /// 原因。
        reason: &'static str,
    },
    /// 无可复用组——写者按 [`SwitchBlocked`] 分类暂停。
    Blocked(SwitchBlocked),
    /// **单条记录超过一个整组**（组容量太小；下限见 §11.5.2）。
    RecordTooLarge {
        /// 记录编码长度。
        encoded_len: usize,
    },
}

impl std::fmt::Display for GroupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GroupError::Io(e) => write!(f, "日志组 I/O：{e}"),
            GroupError::ControlFile(e) => write!(f, "日志组控制文件：{e}"),
            GroupError::Buffer(e) => write!(f, "日志组缓冲：{e}"),
            GroupError::File(e) => write!(f, "日志组文件：{e}"),
            GroupError::Spec(s) => write!(f, "日志组规格不符：{s}"),
            GroupError::NotActivated => f.write_str("控制文件无当前组（尚未首次激活）"),
            GroupError::Damaged { group, reason } => {
                write!(f, "日志组 {} 损坏：{reason}", group + 1)
            }
            GroupError::Blocked(b) => write!(f, "{b}"),
            GroupError::RecordTooLarge { encoded_len } => write!(
                f,
                "记录 {encoded_len} B 超过一个整组（组容量下限 = {MAX_RECORD_FOOTPRINT} B）"
            ),
        }
    }
}

impl std::error::Error for GroupError {}

impl From<io::Error> for GroupError {
    fn from(e: io::Error) -> Self {
        GroupError::Io(e)
    }
}

impl From<ControlFileError> for GroupError {
    fn from(e: ControlFileError) -> Self {
        GroupError::ControlFile(e)
    }
}

impl From<WalError> for GroupError {
    fn from(e: WalError) -> Self {
        GroupError::Buffer(e)
    }
}

impl From<LogFileError> for GroupError {
    fn from(e: LogFileError) -> Self {
        GroupError::File(e)
    }
}

/// **可共享的 WAL 刷盘核心**（§11.7 的 LGWR 角色；P4 线程化）。
///
/// 与 [`GroupWriter`] 的分工：**控制文件的发布（组切换、检查点降级、采样）
/// 仍是单写者**（`cf: &mut ControlFile`），留在 `GroupWriter`；而
/// **追加与刷盘**只需要"缓冲区 + 组文件 + 写盘账"——那三样在这里，经 `Arc`
/// 共享：
///
/// - 前台提交：`flush_to(目标 LSN)`（组提交：`synced_lsn ≥ target` 即返回）；
/// - 后台 DBWR 的 WAL 规则 2：同一个口（`impl WalGuard`）——池把 `Arc` 克隆
///   交给后台线程，于是"页写前 redo 必须耐久"在**任意线程**都能成立；
/// - 周期 LGWR（3 秒兜底）：同口。
///
/// 同步由内部两把闩锁完成：`buffer` 自己的 `redo_buffer`/`redo_io`，
/// 以及这里的 `state`（文件侧写盘账：当前组、每组成员已刷页数、组尾 LSN、
/// 成员健康位）——**刷盘可来自任意线程，切换/发布仍由持有控制文件的一方做**。
pub struct WalShared<'io> {
    io: &'io dyn FileIo,
    spec: GroupSpec,
    /// `files[组][成员]`（建立后不变）。
    files: Vec<Vec<FileHandle>>,
    buffer: LogBuffer,
    /// 文件侧写盘账（闩锁："redo_write" 的宿主）。
    state: Latch<WalState>,
    /// 当前组的起始 LSN（原子量：刷盘快路径与追加预检都要读）。
    current_start: AtomicU64,
    /// **成员 `STALE` 位有待发布到控制文件**（后台 LGWR/池写失败时置；
    /// 前台发布时取走）。见 [`WalShared::take_stale_dirty`]。
    stale_dirty: AtomicBool,
}

/// 文件侧写盘账（[`WalShared`] 的可变部分）。
#[derive(Debug)]
pub struct WalState {
    /// 当前组（0 起）。
    current: u8,
    /// 每组成员已刷出的页数（成员各自记账；组长取未失败成员的最大值）。
    written_pages: Vec<u64>,
    /// 各组的结尾 LSN（最后一张已写页的终点；未用过的组为 `None`）。
    group_ends: [Option<Lsn>; MAX_REDO_GROUPS],
    /// 每组的成员 `STALE` 位（与控制文件里的 Redo 条目同步——由持有控制文件
    /// 的一方在发布时回写；后台刷盘标脏后由下一次前台发布带上）。
    member_stale: [u8; MAX_REDO_GROUPS],
}

/// 一次刷盘的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlushOutcome {
    /// 已持久化的位置。
    pub synced: Lsn,
    /// 本次刷盘是否**新标了成员 `STALE`**（调用方持有控制文件时需发布）。
    pub stale_changed: bool,
}

impl std::fmt::Debug for WalShared<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WalShared")
            .field("current", &self.state.lock().current)
            .field("appended_lsn", &self.buffer.appended_lsn())
            .finish_non_exhaustive()
    }
}

impl<'io> WalShared<'io> {
    /// 追加位置（下一字节 LSN）。
    #[must_use]
    pub fn appended_lsn(&self) -> Lsn {
        self.buffer.appended_lsn()
    }

    /// 已持久化位置。
    #[must_use]
    pub fn synced_lsn(&self) -> Lsn {
        self.buffer.synced_lsn()
    }

    /// 当前组的起始 LSN。
    #[must_use]
    pub fn current_start(&self) -> Lsn {
        Lsn::from_raw(self.current_start.load(Ordering::SeqCst)).expect("48 位域内")
    }

    /// 当前组的容量边界（= 起始 + 组字节数；**判满用**）。
    #[must_use]
    pub fn group_end(&self) -> Lsn {
        lsn_add(self.current_start(), self.spec().group_bytes()).expect("48 位域内")
    }

    /// 某组的结尾 LSN（诊断/检查点降级用）。
    #[must_use]
    pub fn group_end_lsn(&self, group: u8) -> Option<Lsn> {
        self.state.lock().group_ends[usize::from(group)]
    }

    /// 当前组号。
    #[must_use]
    pub fn current(&self) -> u8 {
        self.state.lock().current
    }

    /// 规格。
    #[must_use]
    pub fn spec(&self) -> GroupSpec {
        self.spec
    }

    /// 写盘账的闩锁（组切换/发布路径用；**不跨 I/O 持有**）。
    #[must_use]
    pub fn state(&self) -> LatchGuard<'_, WalState> {
        self.state.lock()
    }

    /// **某组的成员 `STALE` 位**（共享态 = 权威值；控制文件里的是上次发布的副本）。
    #[must_use]
    pub fn member_stale(&self, group: u8) -> u8 {
        self.state.lock().member_stale[usize::from(group)]
    }

    /// **取走"成员位有待发布"标志**（前台发布路径用；取走即清）。
    #[must_use]
    pub fn take_stale_dirty(&self) -> bool {
        self.stale_dirty.swap(false, Ordering::SeqCst)
    }

    /// **刷盘到 `target`**（组提交语义：已覆盖即直接返回；失败 ⇒ `synced_lsn`
    /// 不前进）。**这是 LGWR 的全部工作**——可从任意线程调用。
    pub fn flush_to(&self, target: Lsn) -> Result<FlushOutcome, GroupError> {
        if self.buffer.synced_lsn() >= target {
            return Ok(FlushOutcome {
                synced: self.buffer.synced_lsn(),
                stale_changed: false,
            });
        }
        let mut st = self.state.lock();
        let g = usize::from(st.current);
        let pages = st.written_pages[g];
        // **只写健康成员**：已置 `STALE` 的成员在重建前不再接收写入——否则
        // 落后成员会写出空洞、被位置校验拒绝，进而把健康成员也拖垮。
        let stale = st.member_stale[g];
        let start = self.current_start();
        let group_pages = self.spec().group_pages as u64;
        let member_count = usize::from(self.spec().member_count);
        let active: Vec<usize> = (0..member_count)
            .filter(|m| stale & (1 << m) == 0)
            .collect();
        if active.is_empty() {
            return Err(GroupError::Damaged {
                group: st.current,
                reason: "全部成员已置 STALE——先重建成员镜像",
            });
        }
        let mut tee = TeeSink {
            sinks: active
                .iter()
                .map(|&m| FileLogSink::resume(self.io, self.files[g][m], start, group_pages, pages))
                .collect(),
            failed: vec![false; active.len()],
        };
        let synced = self.buffer.flush_to(target, &mut tee)?;
        // 组已写页数 = **未失败成员**的最大值（成员 0 一次瞬时失败会把统一基准
        // 拖回落后值 ⇒ WAL 永久写不出去——实测复现，P3 审核修复）。
        st.written_pages[g] = tee
            .sinks
            .iter()
            .enumerate()
            .filter(|(i, _)| !tee.failed[*i])
            .map(|(_, s)| s.written_pages())
            .max()
            .unwrap_or(pages);
        st.group_ends[g] = Some(lsn_add(start, st.written_pages[g] * LOG_PAGE_SIZE as u64)?);
        // 成员失败 ⇒ 标 STALE（记在共享账里；持有控制文件的一方随后发布）。
        let mut changed = false;
        for (i, bad) in tee.failed.iter().enumerate() {
            if *bad {
                st.member_stale[g] |= 1 << active[i];
                changed = true;
            }
        }
        if changed {
            // **待发布标志**：本条路径可能来自后台（LGWR/池的 WalGuard），那里
            // 没有控制文件可写；由下一次前台"发布"（`GroupWriter::flush` /
            // `publish_checkpoint`）取走并落控制文件——否则后台发现的成员损坏
            // 永远到不了控制文件，`rebuild_member` 会以"没坏"静默拒绝重建。
            self.stale_dirty.store(true, Ordering::SeqCst);
        }
        Ok(FlushOutcome {
            synced,
            stale_changed: changed,
        })
    }
}

/// **WAL 规则 2 的共享口**（§5.10/§11.7）：池把 `Arc<WalShared>` 当
/// `WalGuard` 用——**后台线程也能"页写前先把 redo 刷到 page_lsn"**。
impl WalGuard for WalShared<'_> {
    fn durable_lsn(&self) -> Lsn {
        self.buffer.synced_lsn()
    }

    fn ensure_durable(&self, target: Lsn) -> std::io::Result<()> {
        self.flush_to(target)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        // **到没到位的核对**：`flush_to` 只保证"把已有的写出去"——若 `target`
        // 超过追加位（页的 `page_lsn` 不该超过它，除非调用方用错了），
        // 这里必须**报错而不是放行**：页的 WAL 规则 2 靠这条兜底。
        let synced = self.buffer.synced_lsn();
        if synced < target {
            return Err(std::io::Error::other(format!(
                "WAL 未持久化到目标：synced={} target={}（追加位 {}）",
                synced.as_raw(),
                target.as_raw(),
                self.buffer.appended_lsn().as_raw()
            )));
        }
        Ok(())
    }
}

/// 日志组写者。
pub struct GroupWriter<'io, 'cf> {
    cf: &'cf mut ControlFile<'io>,
    dir: PathBuf,
    /// 控制文件 Redo 条目的**内存镜像**（单写者；每次发布后更新）。
    entries: RedoEntries,
    archive_mode: ArchiveMode,
    /// **可共享的刷盘核心**（追加/刷盘/文件账都在里面；见 [`WalShared`]）。
    wal: Arc<WalShared<'io>>,
    /// 本组自激活以来追加的记录数（0 ⇒ 强制切换无需刷盘）。
    records_in_group: u64,
    /// **最近一次提交记录**的提交序号（切换点采样的"当前提交序号"来源——
    /// 写线程看不到事务层，但它写过的提交记录自带序号，无需外部喂；
    /// §11.10、待讨论清单第 35 条）。
    last_commit_seq: Option<CommitSeq>,
    /// Committed watermark, independent from whether a new commit was sampled.
    commit_watermark: CommitSeq,
    /// Commit outcomes since the last transaction-aware checkpoint. One entry
    /// per physical undo slot; wrap checks prevent repairing a reused slot.
    checkpoint_record_end: Option<Lsn>,
    checkpoint_commits: BTreeMap<(u8, u8), (u64, CommitSeq, Lsn)>,
    /// 切换点采样的墙钟源（毫秒；默认系统时钟，测试可换固定函数。
    /// 返回 0 表示"无时间语义"——该次不采样，与检查点路径的约定一致）。
    clock_ms: fn() -> u64,
}

/// 切换点采样的默认墙钟源（Unix 毫秒；取不到返回 0 = 不采样）。
fn system_clock_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl std::fmt::Debug for GroupWriter<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GroupWriter")
            .field("spec", &self.wal.spec())
            .field("current", &self.wal.current())
            .field("current_start", &self.wal.current_start())
            .field("appended_lsn", &self.wal.appended_lsn())
            .finish_non_exhaustive()
    }
}

impl<'io, 'cf> GroupWriter<'io, 'cf> {
    /// **新建组集**：创建全部组文件（预置长度），并把首组激活为 `CURRENT`
    /// （序列号 1——在首组第一条位置写切换记录并落盘，然后发布控制文件）。
    ///
    /// 要求控制文件里已有一份**尚未激活**的 Redo 条目（组数/成员数与
    /// `spec` 一致，`current_group = `[`NO_CURRENT_GROUP`]）。
    pub fn create(
        io: &'io dyn FileIo,
        cf: &'cf mut ControlFile<'io>,
        dir: &Path,
        spec: GroupSpec,
        start_lsn: Lsn,
    ) -> Result<Self, GroupError> {
        let entries = cf.redo_entries()?;
        if entries.group_count != spec.group_count || entries.member_count != spec.member_count {
            return Err(GroupError::Spec("控制文件中的组数与规格不一致"));
        }
        if entries.current_group != NO_CURRENT_GROUP {
            return Err(GroupError::Spec("控制文件已有当前组——应使用 open"));
        }
        let mut files = Vec::new();
        for g in 0..spec.group_count {
            let mut members = Vec::new();
            for m in 0..spec.member_count {
                let path = dir.join(member_file_name(g, m));
                let handle = io.open(
                    &path,
                    OpenOptions::new().read(true).write(true).create_new(true),
                )?;
                io.set_len(handle, spec.group_bytes())?;
                members.push(handle);
            }
            files.push(members);
        }
        let archive_mode = cf.archive_record()?.mode;
        let member_stale = core::array::from_fn(|g| entries.groups[g].member_stale);
        let wal = Arc::new(WalShared {
            io,
            spec,
            files,
            buffer: LogBuffer::new(start_lsn),
            state: Latch::new(
                "redo_write",
                WalState {
                    current: 0,
                    written_pages: vec![0; spec.group_count as usize],
                    group_ends: [None; MAX_REDO_GROUPS],
                    member_stale,
                },
            ),
            current_start: AtomicU64::new(start_lsn.as_raw()),
            stale_dirty: AtomicBool::new(false),
        });
        let initial_seq = cf.checkpoint_progress()?.current_commit_seq;
        let mut writer = Self {
            cf,
            dir: dir.to_path_buf(),
            entries,
            archive_mode,
            wal,
            records_in_group: 0,
            last_commit_seq: None,
            commit_watermark: initial_seq,
            checkpoint_record_end: None,
            checkpoint_commits: BTreeMap::new(),
            clock_ms: system_clock_ms,
        };
        writer.activate(0)?; // 首组：序列号 = 1
        Ok(writer)
    }

    /// **打开组集**（重开）：按控制文件找 `CURRENT` 组；扫描**已用过的组**
    /// 重建"结尾 LSN"（降级判据）与当前组的写位置；缓冲从当前组的
    /// **下一页边界**续写（已刷页不改写）。
    pub fn open(
        io: &'io dyn FileIo,
        cf: &'cf mut ControlFile<'io>,
        dir: &Path,
        spec: GroupSpec,
    ) -> Result<Self, GroupError> {
        let entries = cf.redo_entries()?;
        if entries.group_count != spec.group_count || entries.member_count != spec.member_count {
            return Err(GroupError::Spec("控制文件中的组数与规格不一致"));
        }
        let current = entries.current_group;
        if current == NO_CURRENT_GROUP {
            return Err(GroupError::NotActivated);
        }
        if current >= spec.group_count {
            return Err(GroupError::Spec("current_group 超出组数"));
        }
        let archive_mode = cf.archive_record()?.mode;

        let mut files = Vec::new();
        for g in 0..spec.group_count {
            let mut members = Vec::new();
            for m in 0..spec.member_count {
                let path = dir.join(member_file_name(g, m));
                let handle = io.open(&path, OpenOptions::new().read(true).write(true))?;
                members.push(handle);
            }
            files.push(members);
        }

        let mut written_pages = vec![0u64; spec.group_count as usize];
        let mut group_ends: [Option<Lsn>; MAX_REDO_GROUPS] = [None; MAX_REDO_GROUPS];
        let mut current_start_opt: Option<Lsn> = None;
        for g in 0..spec.group_count as usize {
            let used =
                entries.groups[g].run != LogRunState::Unused || entries.groups[g].sequence != 0;
            if !used {
                continue; // 从未用过：文件保持全零
            }
            // 成员挑选（同 `online_groups`）：健康成员里取已写前缀最长者；
            // 单成员损坏/为空由其余镜像顶替（§11.9）；全部为空 = 复用前的
            // 截断态（该组内容不参与恢复），视为"无内容"。
            if let Some((pages, start, _)) = best_member_scan(
                io,
                &files[g],
                g as u8,
                entries.groups[g].member_stale,
                &spec,
            )? {
                written_pages[g] = pages;
                group_ends[g] = Some(lsn_add(start, pages * LOG_PAGE_SIZE as u64)?);
                if g == usize::from(current) {
                    current_start_opt = Some(start);
                }
            }
        }

        let current_start = current_start_opt.ok_or(GroupError::Damaged {
            group: current,
            reason: "当前组无内容（切换记录缺失）",
        })?;
        let resume = group_ends[current as usize].ok_or(GroupError::Damaged {
            group: current,
            reason: "当前组无内容（切换记录缺失）",
        })?;

        let member_stale = core::array::from_fn(|g| entries.groups[g].member_stale);
        let wal = Arc::new(WalShared {
            io,
            spec,
            files,
            buffer: LogBuffer::new(resume),
            state: Latch::new(
                "redo_write",
                WalState {
                    current,
                    written_pages,
                    group_ends,
                    member_stale,
                },
            ),
            current_start: AtomicU64::new(current_start.as_raw()),
            stale_dirty: AtomicBool::new(false),
        });
        let initial_seq = cf.checkpoint_progress()?.current_commit_seq;
        Ok(Self {
            cf,
            dir: dir.to_path_buf(),
            entries,
            archive_mode,
            wal,
            records_in_group: 0,
            last_commit_seq: None,
            commit_watermark: initial_seq,
            checkpoint_record_end: None,
            checkpoint_commits: BTreeMap::new(),
            clock_ms: system_clock_ms,
        })
    }

    /// Seed the committed watermark after startup recovery (monotonic).
    pub fn seed_commit_seq(&mut self, seq: CommitSeq) {
        self.commit_watermark = self.commit_watermark.max(seq);
    }

    /// Remember metadata-only redo generated by the last checkpoint. Idle
    /// ticks must not generate a fresh checkpoint solely to checkpoint itself.
    pub(crate) fn record_checkpoint_end(&mut self) {
        self.checkpoint_record_end = Some(self.appended_lsn());
    }
    pub(crate) fn has_redo_after_checkpoint(&self) -> bool {
        self.checkpoint_record_end != Some(self.appended_lsn())
    }

    /// Highest commit sequence seen by this writer or restored at startup.
    pub fn commit_watermark(&self) -> CommitSeq {
        self.commit_watermark
    }

    /// Commit records whose slot marks must be durable before reclaiming WAL.
    pub(crate) fn checkpoint_commits(&self) -> Vec<(bicdb_storage::undo::TxnId, CommitSeq)> {
        self.checkpoint_commits
            .values()
            .map(|(raw, seq, _)| {
                let bytes = raw.to_le_bytes();
                (
                    bicdb_storage::undo::TxnId::from_bytes(
                        bytes[..6].try_into().expect("six bytes"),
                    ),
                    *seq,
                )
            })
            .collect()
    }

    /// Earliest commit whose undo slot mark is not yet known to be durable.
    pub(crate) fn checkpoint_commit_floor(&self) -> Option<Lsn> {
        self.checkpoint_commits
            .values()
            .map(|(_, _, lsn)| *lsn)
            .min()
    }

    pub(crate) fn clear_checkpoint_commits(&mut self) {
        self.checkpoint_commits.clear();
    }

    // -- 追加与刷盘 ----------------------------------------------------------

    /// **追加一条记录**：latch 内分配 LSN 并分片入页。
    ///
    /// 放不下当前组时**自动切换**（记录不得跨组）后重试——因此 `build`
    /// 在切换路径上会被调用**两次**，必须是**纯构造**（无副作用）。
    pub fn append(&mut self, build: impl Fn(Lsn) -> RedoRecord) -> Result<Lsn, GroupError> {
        // **1/3 触发**（§11.5.5）：占用达阈值先刷盘——单次刷盘体量有界。
        if self.wal.buffer.flush_recommended() {
            let at = self.wal.appended_lsn();
            self.flush(at)?;
        }
        let mut attempt = 0u32;
        loop {
            match self.try_append(&build) {
                // **满则刷 + 重试**（单写者的空间等待形态）。
                Err(GroupError::Buffer(WalError::BufferFull { .. })) if attempt < 2 => {
                    let at = self.wal.appended_lsn();
                    self.flush(at)?;
                    attempt += 1;
                }
                other => return other,
            }
        }
    }

    /// 追加的实际路径（容量不足时返回 [`WalError::BufferFull`]，由
    /// [`GroupWriter::append`] 刷盘后重试）。
    fn try_append(&mut self, build: &impl Fn(Lsn) -> RedoRecord) -> Result<Lsn, GroupError> {
        let probe = self.wal.appended_lsn();
        let record = build(probe);
        let record = if self.wal.buffer.end_lsn_if_appended(record.encoded_len())
            > self.wal.group_end()
        {
            self.switch_group()?;
            let lsn = self.wal.appended_lsn();
            let rebuilt = build(lsn);
            if self.wal.buffer.end_lsn_if_appended(rebuilt.encoded_len()) > self.wal.group_end() {
                return Err(GroupError::RecordTooLarge {
                    encoded_len: rebuilt.encoded_len(),
                });
            }
            rebuilt
        } else {
            record
        };
        // 交给缓冲时以**它分配的 LSN**为准（当前页恰好放满时它会进位到新页；
        // 预检的 probe 可能与之不同）——仅在不一致时重建。
        // 提交记录的序号顺手记下（**落盘成功才生效**）——切换点采样用它。
        let commit_seq = record.commit_seq();
        let txn_raw = record.txn_id;
        let rollback_done = record.op == crate::record::RecordOp::RollbackDone as u8;
        let mut reuse = Some(record);
        let lsn = self.wal.buffer.append(|lsn| match reuse.take() {
            Some(r) if r.lsn == lsn => r,
            _ => build(lsn),
        })?;
        if let Some(raw) = commit_seq {
            if let Some(seq) = CommitSeq::from_raw(raw) {
                self.seed_commit_seq(seq);
                self.last_commit_seq = Some(self.commit_watermark);
                self.checkpoint_commits.insert(
                    ((txn_raw >> 40) as u8, (txn_raw >> 32) as u8),
                    (txn_raw, seq, lsn),
                );
            }
        }
        if rollback_done
            && self
                .checkpoint_commits
                .get(&((txn_raw >> 40) as u8, (txn_raw >> 32) as u8))
                .is_some_and(|(raw, _, _)| *raw == txn_raw)
        {
            self.checkpoint_commits
                .remove(&((txn_raw >> 40) as u8, (txn_raw >> 32) as u8));
        }
        self.records_in_group += 1;
        Ok(lsn)
    }

    /// **刷盘到 `target`**（组提交语义由 [`LogBuffer`] 承担）：把当前组的
    /// 未刷页写入其成员文件并 sync；失败时页被放回（重试可继续）。
    /// **刷盘到 `target`**（组提交语义；LGWR 的全部工作——实现见
    /// [`WalShared::flush_to`]，这里只做"标脏后发布控制文件"的收尾）。
    pub fn flush(&mut self, target: Lsn) -> Result<Lsn, GroupError> {
        let out = self.wal.flush_to(target)?;
        // **后台标脏也在这里发布**：本条路径新标脏（`out.stale_changed`）或
        // 后台/池早已标脏（`take_stale_dirty`）——两者都落控制文件。
        if out.stale_changed || self.wal.take_stale_dirty() {
            // 成员降级要发布进控制文件（下一次 open/诊断看得到）。
            let st = self.wal.state();
            for (g, entry) in self.entries.groups.iter_mut().enumerate() {
                entry.member_stale = st.member_stale[g];
            }
            drop(st);
            self.cf.write_redo_entries(&self.entries)?;
        }
        Ok(out.synced)
    }

    /// **可共享的刷盘核心**（交给池当 `WalGuard`、交给后台 DBWR/LGWR 线程）。
    #[must_use]
    pub fn shared(&self) -> Arc<WalShared<'io>> {
        Arc::clone(&self.wal)
    }

    /// **重建成员镜像**（§11.9 的 `STALE` 恢复）：从健康成员复制已用前缀，
    /// 清除该成员的 `STALE` 位并发布。幂等。
    pub fn rebuild_member(&mut self, group: u8, member: u8) -> Result<(), GroupError> {
        let g = usize::from(group);
        let m = usize::from(member);
        if g >= self.wal.spec().group_count as usize || m >= self.wal.spec().member_count as usize {
            return Err(GroupError::Spec("组号/成员号越界"));
        }
        // 权威 = **共享态**（后台刷盘直接写它）；`entries` 只是上次发布出去的
        // 副本，可能还没带上后台刚标的位——两者不一致时以共享态为准。
        if self.wal.member_stale(group) & (1 << m) == 0 {
            return Ok(()); // 没坏，不用重建
        }
        self.entries.groups[g].member_stale = self.wal.member_stale(group);
        let source = (0..self.wal.spec().member_count as usize)
            .find(|&k| k != m && self.entries.groups[g].member_stale & (1 << k) == 0)
            .ok_or(GroupError::Spec("没有健康成员可作重建源"))?;
        let bytes = self.wal.state().written_pages[g] * LOG_PAGE_SIZE as u64;
        let mut buf = vec![0u8; LOG_PAGE_SIZE];
        let mut copied = 0u64;
        while copied < bytes {
            let n = (bytes - copied).min(LOG_PAGE_SIZE as u64) as usize;
            self.wal
                .io
                .read_exact_at(self.wal.files[g][source], &mut buf[..n], copied)?;
            self.wal
                .io
                .write_at(self.wal.files[g][m], &buf[..n], copied)?;
            copied += n as u64;
        }
        // 目标尾部可能残留上一周期的旧页——截到已复制前缀，避免"重建好的
        // 成员"再被扫描判成"中部坏页"。
        self.wal.io.set_len(self.wal.files[g][m], bytes)?;
        self.wal.io.sync_data(self.wal.files[g][m])?;
        self.entries.groups[g].member_stale &= !(1 << m);
        // **共享账同步**：健康位是刷盘挑成员的依据（`WalShared::flush_to` 读
        // 的是共享副本）——不同步会让重建好的成员被永久跳过（实测：双成员
        // 逐字节一致性用例抓到的正是这一条）。
        self.wal.state().member_stale[g] &= !(1 << m);
        self.cf.write_redo_entries(&self.entries)?;
        Ok(())
    }

    /// 某组的成员镜像位（诊断）。
    #[must_use]
    pub fn member_stale(&self, group: u8) -> u8 {
        self.entries.groups[usize::from(group)].member_stale
    }

    // -- 切换 ---------------------------------------------------------------

    /// **自动切换**（当前组放不下时由 [`GroupWriter::append`] 调用，
    /// 也可显式调用作**强制切换**）：执行 §11.9 步骤 1–3。
    pub fn switch_group(&mut self) -> Result<u8, GroupError> {
        let next = self.pick_reusable()?;
        self.activate(next)
    }

    /// **下一次切换会不会被挡**（`None` = 不会；**只读探测**，不动任何状态）。
    ///
    /// 用途：**CKPT 的"组满被迫"触发条件**（`doc/arch/11-持久化与恢复.md` §11.7：
    /// "① ~3 秒周期发布低水位；② **组满被迫**（日志要切换而下一组未降级）"）。
    /// 写者只有真正写满当前组时才会撞上 `Blocked`（那时语句已经失败回滚了），
    /// 所以调用方要能在**动手之前**问一句"再写下去会不会撞墙"——问完就地
    /// 推一次检查点，写者就能一路写下去（Oracle 的"日志切换触发检查点、
    /// 频繁切换反噬 I/O"与 PG 的 `max_wal_size` 触发检查点，都是这条）。
    #[must_use]
    pub fn switch_blocked(&self) -> Option<SwitchBlocked> {
        match self.pick_reusable() {
            Ok(_) => None,
            Err(GroupError::Blocked(why)) => Some(why),
            Err(_) => None, // 其余错误不是"挡"，由真正切换时报
        }
    }

    /// 从当前组的下一组起轮转，找可复用组；找不到 ⇒ 按缺的条件分类。
    fn pick_reusable(&self) -> Result<u8, GroupError> {
        let mut blocked_archive = false;
        let mut blocked_checkpoint = false;
        let current = self.wal.current();
        for step in 1..=self.wal.spec().group_count {
            let g = (current + step) % self.wal.spec().group_count;
            if g == current {
                continue;
            }
            let e = self.entries.groups[g as usize];
            let run_ok = matches!(e.run, LogRunState::Inactive | LogRunState::Unused);
            if !run_ok {
                if e.run == LogRunState::Active {
                    blocked_checkpoint = true;
                }
                continue;
            }
            let archive_ok = self.archive_mode == ArchiveMode::NoArchive
                || e.run == LogRunState::Unused
                || e.archive == LogArchiveState::Done;
            if !archive_ok {
                blocked_archive = true;
                continue;
            }
            return Ok(g);
        }
        Err(GroupError::Blocked(if blocked_archive {
            SwitchBlocked::AwaitingArchive
        } else {
            let _ = blocked_checkpoint;
            SwitchBlocked::AwaitingCheckpoint
        }))
    }

    /// 激活 `next`：刷尽旧组 → 写切换记录（**先落盘**）→ 发布控制文件。
    fn activate(&mut self, next: u8) -> Result<u8, GroupError> {
        let first = self.entries.current_group == NO_CURRENT_GROUP;
        let old = self.wal.current();

        // 1) 刷尽旧组（末页的尾部空位留在原处；新组从下一张页起）。
        if !first && self.records_in_group > 0 {
            let at = self.wal.appended_lsn();
            self.flush(at)?;
        }
        let new_start = self.wal.buffer.current_page_start();
        let seq = if first {
            1
        } else {
            self.entries.groups[old as usize].sequence + 1
        };

        // 2) 切换记录写进新组并落盘（**先于** CURRENT 发布——见模块文档）。
        //    **复用组先清空**：上一周期的旧页会以"中部坏页"形态被判损坏
        //    （新周期写得比上一轮少时必然踩中）。截断到零，让组内容 = 本周期；
        //    截断后的空文件对 `open/online_groups` 是合法的"无内容"态
        //    （`Inactive` 组的旧内容不参与恢复）——崩溃在切换记录落盘前，
        //    也只是留下一个空组，不产生"损坏"。
        {
            let files = &self.wal.files[next as usize];
            for h in files {
                self.wal.io.set_len(*h, 0)?;
            }
        }
        {
            let mut st = self.wal.state();
            st.current = next;
            st.written_pages[next as usize] = 0;
            st.group_ends[next as usize] = Some(new_start);
            // 新激活的组：镜像视为健康（旧位随序列号换代清零）、成员位同步。
            st.member_stale[next as usize] = 0;
        }
        self.wal
            .current_start
            .store(new_start.as_raw(), Ordering::SeqCst);
        self.records_in_group = 0;
        self.wal
            .buffer
            .append(|l| RedoRecord::log_switch(l, next, seq))?;
        let end = self.wal.appended_lsn();
        self.flush(end)?;
        self.records_in_group = 1;

        // 3) 控制文件发布。
        if !first {
            self.wal.state().group_ends[usize::from(old)] = Some(new_start);
            let old_entry = &mut self.entries.groups[old as usize];
            old_entry.run = LogRunState::Active;
            if self.archive_mode == ArchiveMode::ArchiveLog {
                old_entry.archive = LogArchiveState::Needed;
            }
        }
        self.entries.groups[next as usize] = RedoGroup {
            member_stale: 0, // 新激活：镜像视为健康（旧位随序列号换代清零）
            sequence: seq,
            run: LogRunState::Current,
            archive: LogArchiveState::None,
        };
        self.entries.current_group = next;
        self.cf.write_redo_entries(&self.entries)?;

        // 4) **切换点的墙钟采样**（§11.10；待讨论清单第 35 条）：以"截至切换时
        //    最后一次提交的序号 + 现在"落一对——墙钟目标点的插值在**两次检查点
        //    之间**也有分段依据（切换比检查点频繁得多）。首个组激活时尚无提交，
        //    自然跳过；时钟返回 0 视为「无时间语义」，不采样。
        if let Some(seq) = self.last_commit_seq {
            let ms = (self.clock_ms)();
            if ms != 0 {
                self.cf.append_sample_pair(seq, ms)?;
            }
        }
        Ok(next)
    }

    /// **换时钟**（测试/运维用）：切换点采样的墙钟源。
    pub fn set_clock_ms(&mut self, clock: fn() -> u64) {
        self.clock_ms = clock;
    }

    // -- 检查点与归档（状态迁移算法的发布口）----------------------------------

    /// **检查点发布**（CKPT 角色）：把"**组结尾 LSN ≤ 检查点低水位**"的
    /// `ACTIVE` 组降为 `INACTIVE`，与低水位**同一次控制文件更新**发布
    /// （§11.9 的发布纪律）。
    pub fn publish_checkpoint(
        &mut self,
        progress: &CheckpointProgress,
    ) -> Result<usize, GroupError> {
        let mut demoted = 0usize;
        for g in 0..self.wal.spec().group_count as usize {
            if self.entries.groups[g].run != LogRunState::Active {
                continue;
            }
            if let Some(end) = self.wal.state().group_ends[g] {
                if end <= progress.checkpoint_lsn {
                    self.entries.groups[g].run = LogRunState::Inactive;
                    demoted += 1;
                }
            }
        }
        // **顺带发布"待发布"的成员位**（后台 LGWR 标脏的那批）：检查点本就要
        // 写一次控制文件，带上它们不额外花代价。
        if self.wal.take_stale_dirty() {
            let st = self.wal.state();
            for (g, entry) in self.entries.groups.iter_mut().enumerate() {
                entry.member_stale = st.member_stale[g];
            }
            drop(st);
        }
        self.cf
            .write_checkpoint_and_groups(progress, &self.entries)?;
        Ok(demoted)
    }

    /// **追加墙钟采样对**（§11.10：时间点目标点的插值用）——检查点处由
    /// 调用方喂 `progress.timestamp`；**日志切换处由写线程自动补喂**
    /// （`last_commit_seq` + 墙钟，见 `activate` 第 4 步）。
    pub fn append_sample_pair(
        &mut self,
        seq: bicdb_common::seq::CommitSeq,
        timestamp_ms: u64,
    ) -> Result<(), GroupError> {
        self.cf.append_sample_pair(seq, timestamp_ms)?;
        Ok(())
    }

    /// 控制文件里当前的检查点进度（检查点的单调性守卫用）。
    pub fn checkpoint_progress(&self) -> Result<CheckpointProgress, GroupError> {
        Ok(self.cf.checkpoint_progress()?)
    }

    /// 某组的结尾 LSN（已写前缀的下一页边界；未用过的组为 `None`）。
    #[must_use]
    pub fn group_end_lsn(&self, group: u8) -> Option<Lsn> {
        self.wal.group_end_lsn(group)
    }

    /// **归档完成发布**（ARCn 角色的替身，归档切片接管）：
    /// 组归档状态 `NEEDED` → `DONE`，并推进归档记录的"最后归档序列号"。
    pub fn archive_done(&mut self, group: u8) -> Result<(), GroupError> {
        let idx = usize::from(group);
        if idx >= self.wal.spec().group_count as usize {
            return Err(GroupError::Spec("组号超出组数"));
        }
        if self.entries.groups[idx].archive != LogArchiveState::Needed {
            return Err(GroupError::Spec("该组不在待归档状态"));
        }
        self.entries.groups[idx].archive = LogArchiveState::Done;
        self.cf.write_redo_entries(&self.entries)?;
        let mut record = self.cf.archive_record()?;
        record.last_archived_sequence = record
            .last_archived_sequence
            .max(self.entries.groups[idx].sequence);
        self.cf.write_archive_record(&record)?;
        Ok(())
    }

    // -- 读取 ---------------------------------------------------------------

    /// 当前组号（0 起）。
    #[must_use]
    pub fn current_group(&self) -> u8 {
        self.wal.current()
    }

    /// 当前组的日志序列号。
    #[must_use]
    pub fn current_sequence(&self) -> u32 {
        self.entries.groups[usize::from(self.wal.current())].sequence
    }

    /// 当前组的结尾 LSN（= 起始 + 组字节大小）。
    #[must_use]
    pub fn group_end(&self) -> Lsn {
        self.wal.group_end()
    }

    /// 追加位置（下一字节 LSN）。
    #[must_use]
    pub fn appended_lsn(&self) -> Lsn {
        self.wal.buffer.appended_lsn()
    }

    /// 已刷盘位置。
    #[must_use]
    pub fn synced_lsn(&self) -> Lsn {
        self.wal.buffer.synced_lsn()
    }

    /// Redo 条目的内存镜像（诊断/测试）。
    #[must_use]
    pub fn entries(&self) -> &RedoEntries {
        &self.entries
    }

    /// 规格。
    #[must_use]
    pub fn spec(&self) -> GroupSpec {
        self.wal.spec()
    }

    /// 组目录。
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 关闭所有成员文件句柄。
    pub fn close(self) -> Result<(), GroupError> {
        for members in &self.wal.files {
            for &h in members {
                self.wal.io.close(h)?;
            }
        }
        Ok(())
    }
}

fn lsn_add(base: Lsn, delta: u64) -> Result<Lsn, GroupError> {
    Lsn::from_raw(base.as_raw() + delta).ok_or(GroupError::Spec("LSN 越过 48 位域"))
}

/// 在线组（恢复入口的**只读视图**）：一个已用组的文件句柄与流位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OnlineGroup {
    /// 组号（0 起）。
    pub group: u8,
    /// 该组最近一次的日志序列号。
    pub sequence: u32,
    /// 组起点 LSN（首张页的位置）。
    pub start_lsn: Lsn,
    /// 组结尾 LSN（已写前缀的下一页边界）。
    pub end_lsn: Lsn,
    /// 该组成员的文件句柄（恢复只读扫描用）。
    pub handle: FileHandle,
    /// 组成员容量（页数）。
    pub file_pages: u32,
}

/// 扫描**已用**的在线组，按**序列号升序**返回（= 日志流顺序）。
///
/// 只打开已用组的文件（恢复路径不写、不触碰从未使用的组）。序列号重复
/// ⇒ 控制文件不自洽，明确拒绝。
pub fn online_groups(
    io: &dyn FileIo,
    cf: &ControlFile<'_>,
    dir: &Path,
    spec: GroupSpec,
) -> Result<Vec<OnlineGroup>, GroupError> {
    let entries = cf.redo_entries()?;
    if entries.group_count != spec.group_count || entries.member_count != spec.member_count {
        return Err(GroupError::Spec("控制文件中的组数与规格不一致"));
    }
    let mut out: Vec<OnlineGroup> = Vec::new();
    for g in 0..spec.group_count as usize {
        let entry = entries.groups[g];
        if entry.run == LogRunState::Unused && entry.sequence == 0 {
            continue;
        }
        // **成员挑选**（§11.9）：健康成员里取"已写前缀最长"者——
        // `STALE` 成员不参与；全 `STALE` 时（防御）退回全部候选；
        // 单成员损坏/为空由其余镜像顶替（健康镜像必须救得回来）。
        let mut handles: Vec<FileHandle> = Vec::with_capacity(spec.member_count as usize);
        for m in 0..spec.member_count {
            let path = dir.join(member_file_name(g as u8, m));
            handles.push(io.open(&path, OpenOptions::new().read(true))?);
        }
        let picked = best_member_scan(io, &handles, g as u8, entry.member_stale, &spec);
        let best = match picked {
            Ok(b) => b,
            Err(e) => {
                for h in handles {
                    let _ = io.close(h);
                }
                return Err(e);
            }
        };
        let Some((pages, start, m)) = best else {
            // 全部为空（复用前截断的 `Inactive` 组）：不参与恢复，跳过。
            for h in handles {
                let _ = io.close(h);
            }
            continue;
        };
        // 未选中的句柄关掉（不泄漏 fd）。
        for (i, h) in handles.iter().enumerate() {
            if i != m {
                let _ = io.close(*h);
            }
        }
        let handle = handles[m];
        let end_lsn = match lsn_add(start, pages * LOG_PAGE_SIZE as u64) {
            Ok(end) => end,
            Err(error) => {
                let _ = io.close(handle);
                for group in &out {
                    let _ = io.close(group.handle);
                }
                return Err(error);
            }
        };
        out.push(OnlineGroup {
            group: g as u8,
            sequence: entry.sequence,
            start_lsn: start,
            end_lsn,
            handle,
            file_pages: spec.group_pages,
        });
    }
    out.sort_by_key(|g| g.sequence);
    for w in out.windows(2) {
        if w[0].sequence == w[1].sequence {
            for group in &out {
                let _ = io.close(group.handle);
            }
            return Err(GroupError::Spec("在线组序列号重复——控制文件不自洽"));
        }
    }
    Ok(out)
}

/// 扫描一个已用组：返回（已写页数，组起点 LSN）。
///
/// 组的尾部残缺（坏页/空页）与之区分：其后仍有非零页 ⇒ **中部坏页**，按损坏拒绝。
/// 扫描一个已用组：`Ok(None)` = **文件为空**（从未写过，或复用前已截断）；
/// `Ok(Some(..))` = （已写页数，组起点 LSN）；`Err` = 损坏。
///
/// 组的尾部残缺（坏页/空页）与之区分：其后仍有非零页 ⇒ **中部坏页**，按损坏拒绝。
fn scan_used_group(
    io: &dyn FileIo,
    handle: FileHandle,
    group: u8,
    group_pages: u32,
) -> Result<Option<(u64, Lsn)>, GroupError> {
    let Some(start) = read_group_start(io, handle, group)? else {
        return Ok(None); // 空文件：不是损坏（复用前的截断态）
    };
    let scan = scan_log(io, handle, start, group_pages as u64)?;
    if let Some(bad) = scan.first_bad_page {
        if any_nonzero_page_after(io, handle, bad + 1, group_pages as u64)? {
            return Err(GroupError::Damaged {
                group,
                reason: "中部有坏页而其后仍有数据",
            });
        }
    } else if scan.pages_scanned < group_pages as u64
        && any_nonzero_page_after(io, handle, scan.pages_scanned, group_pages as u64)?
    {
        // **中部空洞**：空页断点之后仍有非零页——不是尾部截断，按损坏拒绝
        // （否则该断点之后的记录被静默丢弃，违反"损坏必须有名有姓"）。
        return Err(GroupError::Damaged {
            group,
            reason: "中部有空页而其后仍有数据",
        });
    }
    Ok(Some((scan.pages_scanned, start)))
}

/// 读组的首张页并将其起始 LSN 作为组起点；整页全零 = 从未写过（`None`）。
fn read_group_start(
    io: &dyn FileIo,
    handle: FileHandle,
    group: u8,
) -> Result<Option<Lsn>, GroupError> {
    let mut buf = Box::new([0u8; LOG_PAGE_SIZE]);
    io.read_exact_at(handle, buf.as_mut_slice(), 0)?;
    if buf.iter().all(|&b| b == 0) {
        return Ok(None);
    }
    let page = LogPage::from_bytes(buf);
    page.verify().map_err(|_| GroupError::Damaged {
        group,
        reason: "组首张页校验和不符",
    })?;
    Ok(Some(page.start_lsn()))
}

/// **跨成员扫描一个"已用过"的组**（`open` 与 `online_groups` 共用）：
/// 健康成员里取**已写前缀最长**者；单个成员损坏或为空 ⇒ **跳过**
/// （其余镜像顶替——§11.9 的降级语义，健康镜像必须救得回来）。
///
/// - `Ok(Some((页数, 起点, 成员号)))`：选中者；
/// - `Ok(None)`：**所有候选都为空文件**（复用前的截断态——`Inactive` 组的旧
///   内容不参与恢复，合法）；
/// - `Err`：没有可用候选，且至少一个候选是**损坏**（不是"为空"）——响亮报错。
fn best_member_scan(
    io: &dyn FileIo,
    files: &[FileHandle],
    group: u8,
    member_stale: u8,
    spec: &GroupSpec,
) -> Result<Option<(u64, Lsn, usize)>, GroupError> {
    let mut order: Vec<usize> = (0..usize::from(spec.member_count))
        .filter(|m| member_stale & (1 << m) == 0)
        .collect();
    if order.is_empty() {
        order = (0..usize::from(spec.member_count)).collect();
    }
    let mut best: Option<(u64, Lsn, usize)> = None;
    let mut first_damage: Option<GroupError> = None;
    for m in order {
        match scan_used_group(io, files[m], group, spec.group_pages) {
            Ok(None) => {} // 空文件：跳过
            Ok(Some((pages, start))) => {
                if best.as_ref().map_or(true, |(p, _, _)| pages > *p) {
                    best = Some((pages, start, m));
                }
            }
            Err(e) => {
                // 坏成员：跳过（健康镜像顶替），但**记住具体原因**——若最终
                // 无可用成员，报它（比"全部损坏"这种笼统文案可诊断得多）。
                if first_damage.is_none() {
                    first_damage = Some(e);
                }
            }
        }
    }
    if best.is_none() {
        if let Some(e) = first_damage {
            return Err(e);
        }
    }
    Ok(best)
}

/// 从页号 `from` 起，是否存在非零页（用于区分"组尾截断"与"中部坏页"）。
fn any_nonzero_page_after(
    io: &dyn FileIo,
    handle: FileHandle,
    from: u64,
    file_pages: u64,
) -> Result<bool, GroupError> {
    // 以**实际文件长度**为准（声明容量可能大于当前长度——`scan_log` 同规）；
    // 越过 EOF 去读会把"文件较短"误判成 I/O 损坏。
    let size = io.size(handle)?;
    let usable = (size / LOG_PAGE_SIZE as u64).min(file_pages);
    let mut buf = Box::new([0u8; LOG_PAGE_SIZE]);
    for page in from..usable {
        io.read_exact_at(handle, buf.as_mut_slice(), page * LOG_PAGE_SIZE as u64)?;
        if buf.iter().any(|&b| b != 0) {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use bicdb_common::seq::CommitSeq;
    use bicdb_storage::controlfile::{ArchiveRecord, WorkspaceEntry};
    use bicdb_workspace::id::WorkspaceId;
    use bicdb_workspace::io::MemFileIo;

    use super::*;
    use crate::record::{BlockRef, Change, Rdba, RecordOp};

    const A: &str = "/mem/control01.ctl";
    const B: &str = "/mem/control02.ctl";
    const WAL: &str = "/mem/wal";

    fn lsn(v: u64) -> Lsn {
        Lsn::from_raw(v).unwrap()
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

    /// 扫描某组文件里的记录（按序）。
    fn scan_group(io: &dyn FileIo, group: u8, pages: u32) -> (Vec<RedoRecord>, u64) {
        let path = format!("{WAL}/{}", member_file_name(group, 0));
        let h = io
            .open(Path::new(&path), OpenOptions::new().read(true))
            .unwrap();
        let start = read_group_start(io, h, group).unwrap().expect("已写过");
        let result = scan_log(io, h, start, pages as u64).unwrap();
        (result.records, result.pages_scanned)
    }

    fn advance_commit(w: &mut GroupWriter, seq: u64) -> Lsn {
        w.append(|l| RedoRecord::commit(l, 1, seq)).unwrap()
    }

    #[test]
    fn commit_repairs_are_keyed_by_physical_slot_not_wrap() {
        let seq = |value| CommitSeq::from_raw(value).unwrap();
        let io = mem();
        let mut cf = ControlFile::format(
            &io,
            Path::new(A),
            Path::new(B),
            &ws_entry(),
            &RedoEntries::new(2, 1).unwrap(),
            &ArchiveRecord::default(),
        )
        .unwrap();
        let mut writer = GroupWriter::create(
            &io,
            &mut cf,
            Path::new(WAL),
            GroupSpec::new(2, 1, 64).unwrap(),
            lsn(0),
        )
        .unwrap();
        let a = bicdb_storage::undo::TxnId::from_parts(0, 1, 1);
        let b = bicdb_storage::undo::TxnId::from_parts(0, 2, 1);
        let first = writer
            .append(|l| RedoRecord::commit(l, a.as_raw(), 1))
            .unwrap();
        writer
            .append(|l| RedoRecord::commit(l, b.as_raw(), 2))
            .unwrap();
        assert_eq!(writer.checkpoint_commits(), vec![(a, seq(1)), (b, seq(2))]);
        assert_eq!(writer.checkpoint_commit_floor(), Some(first));
        let newer_a = bicdb_storage::undo::TxnId::from_parts(0, 1, 2);
        writer
            .append(|l| RedoRecord::commit(l, newer_a.as_raw(), 3))
            .unwrap();
        writer
            .append(|l| RedoRecord::rollback_done(l, a.as_raw()))
            .unwrap();
        assert_eq!(
            writer.checkpoint_commits(),
            vec![(newer_a, seq(3)), (b, seq(2))],
            "旧代次不能删除新代次的提交结果，也不能覆盖其他槽"
        );
    }

    #[test]
    fn create_activates_first_group_with_switch_record() {
        let io = mem();
        let mut cf = ControlFile::format(
            &io,
            Path::new(A),
            Path::new(B),
            &ws_entry(),
            &RedoEntries::new(2, 1).unwrap(),
            &ArchiveRecord::default(),
        )
        .unwrap();
        let mut w = GroupWriter::create(
            &io,
            &mut cf,
            Path::new(WAL),
            GroupSpec::new(2, 1, 4).unwrap(),
            lsn(0),
        )
        .unwrap();

        assert_eq!(w.current_group(), 0);
        assert_eq!(w.current_sequence(), 1);
        let e = w.entries();
        assert_eq!(e.current_group, 0);
        assert_eq!(e.groups[0].run, LogRunState::Current);
        assert_eq!(e.groups[0].sequence, 1);
        assert_eq!(e.groups[1].run, LogRunState::Unused);

        // 首组第一条位置 = 切换记录（组号 0、序列号 1）。
        let (records, pages) = scan_group(&io, 0, 4);
        assert_eq!(pages, 1);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0], RedoRecord::log_switch(records[0].lsn, 0, 1));
        assert_eq!(records[0].lsn, lsn(16), "记录起于页头之后");
        assert_eq!(records[0].op, RecordOp::LogSwitch.as_u8());
        let _ = &mut w;
    }

    #[test]
    fn append_fills_then_auto_switches_with_sequence_advance() {
        let io = mem();
        let mut cf = ControlFile::format(
            &io,
            Path::new(A),
            Path::new(B),
            &ws_entry(),
            &RedoEntries::new(2, 1).unwrap(),
            // **显式开归档**：本用例钉的是归档门控下的日志切换（默认是不归档）。
            &ArchiveRecord::new(bicdb_storage::controlfile::ArchiveMode::ArchiveLog),
        )
        .unwrap();
        let mut w = GroupWriter::create(
            &io,
            &mut cf,
            Path::new(WAL),
            GroupSpec::new(2, 1, 2).unwrap(),
            lsn(0),
        )
        .unwrap();

        // 2 页组（1024 B）：**页 0 只有切换记录**（激活即刷盘，已刷页不再追加），
        // 后续记录从页 1 起——一页 496 B 可用、每条 commit 占 38 B ⇒ 13 条。
        let mut seq = 0u64;
        while w.current_group() == 0 {
            advance_commit(&mut w, seq);
            seq += 1;
            assert!(seq < 100, "应在组满前切换");
        }
        w.flush(w.appended_lsn()).unwrap();

        // 组 0：切换记录 + 13 条 commit（页 0 + 页 1）；组 1 首条是切换记录。
        let (g0, p0) = scan_group(&io, 0, 2);
        assert_eq!(p0, 2);
        assert_eq!(g0.len(), 1 + 13);
        assert_eq!(g0[0].op, RecordOp::LogSwitch.as_u8());
        let (g1, _) = scan_group(&io, 1, 2);
        assert_eq!(g1[0], RedoRecord::log_switch(g1[0].lsn, 1, 2));

        // 控制文件：组 0 = ACTIVE + NEEDED（归档模式默认开启）；组 1 = CURRENT。
        let e = w.entries();
        assert_eq!(e.groups[0].run, LogRunState::Active);
        assert_eq!(e.groups[0].archive, LogArchiveState::Needed);
        assert_eq!(e.groups[1].run, LogRunState::Current);
        assert_eq!(e.groups[1].sequence, 2);
        assert_eq!(e.current_group, 1);
    }

    #[test]
    fn preemptive_switch_is_exact() {
        // 2 页组（1024 B）：页 0 = 切换记录（激活即刷盘）；页 1（496 B 可用）
        // 容纳 13 条 commit（13×38 = 494 ≤ 496）；第 14 条要越到页 2 ⇒ 提前切换。
        let io = mem();
        let mut cf = ControlFile::format(
            &io,
            Path::new(A),
            Path::new(B),
            &ws_entry(),
            &RedoEntries::new(2, 1).unwrap(),
            &ArchiveRecord::default(),
        )
        .unwrap();
        let mut w = GroupWriter::create(
            &io,
            &mut cf,
            Path::new(WAL),
            GroupSpec::new(2, 1, 2).unwrap(),
            lsn(0),
        )
        .unwrap();
        for seq in 0..13 {
            advance_commit(&mut w, seq);
            assert_eq!(w.current_group(), 0, "第 {} 条仍应放得下", seq + 1);
        }
        advance_commit(&mut w, 13);
        assert_eq!(w.current_group(), 1, "第 14 条放不下 ⇒ 提前切换");
        w.flush(w.appended_lsn()).unwrap();
        let (g0, p0) = scan_group(&io, 0, 2);
        assert_eq!(p0, 2);
        assert_eq!(g0.len(), 14, "切换记录 + 13 条");
    }

    #[test]
    fn blocked_then_checkpoint_then_archive_unblocks() {
        let io = mem();
        let mut cf = ControlFile::format(
            &io,
            Path::new(A),
            Path::new(B),
            &ws_entry(),
            &RedoEntries::new(2, 1).unwrap(),
            // **显式开归档**：本用例钉的是归档门控下的日志切换（默认是不归档）。
            &ArchiveRecord::new(bicdb_storage::controlfile::ArchiveMode::ArchiveLog), // 归档模式默认开启
        )
        .unwrap();
        let mut w = GroupWriter::create(
            &io,
            &mut cf,
            Path::new(WAL),
            GroupSpec::new(2, 1, 2).unwrap(),
            lsn(0),
        )
        .unwrap();
        // 组 0 装满后切到组 1；组 1 也装满 ⇒ 无组可切（组 0 ACTIVE）。
        let mut seq = 0u64;
        while w.current_group() == 0 {
            advance_commit(&mut w, seq);
            seq += 1;
        }
        let mut blocked = None;
        for _ in 0..40 {
            match w.append(|l| RedoRecord::commit(l, 1, seq)) {
                Err(GroupError::Blocked(b)) => {
                    blocked = Some(b);
                    break;
                }
                Ok(_) => seq += 1,
                Err(e) => panic!("意外错误：{e}"),
            }
        }
        assert_eq!(blocked, Some(SwitchBlocked::AwaitingCheckpoint));

        // 检查点越过组 0 结尾 ⇒ 降级为 INACTIVE，但仍未归档 ⇒ 等待归档。
        let g0_end = {
            let (_, pages) = scan_group(&io, 0, 2);
            lsn(pages * LOG_PAGE_SIZE as u64)
        };
        let progress = CheckpointProgress {
            checkpoint_lsn: g0_end,
            ..CheckpointProgress::default()
        };
        w.publish_checkpoint(&progress).unwrap();
        assert_eq!(w.entries().groups[0].run, LogRunState::Inactive);
        assert_eq!(
            w.append(|l| RedoRecord::commit(l, 1, seq))
                .unwrap_err()
                .to_string(),
            SwitchBlocked::AwaitingArchive.to_string()
        );

        // 归档完成 ⇒ 可复用，切换回组 0（序列号 +1）。
        w.archive_done(0).unwrap();
        let g = w.switch_group().unwrap();
        assert_eq!(g, 0);
        assert_eq!(w.current_sequence(), 3);
        w.flush(w.appended_lsn()).unwrap();
        let (g0_records, _) = scan_group(&io, 0, 1);
        assert_eq!(
            g0_records[0],
            RedoRecord::log_switch(g0_records[0].lsn, 0, 3)
        );
    }

    #[test]
    fn no_archive_mode_skips_archive_wait() {
        let io = mem();
        let mut cf = ControlFile::format(
            &io,
            Path::new(A),
            Path::new(B),
            &ws_entry(),
            &RedoEntries::new(2, 1).unwrap(),
            &ArchiveRecord::new(ArchiveMode::NoArchive),
        )
        .unwrap();
        let mut w = GroupWriter::create(
            &io,
            &mut cf,
            Path::new(WAL),
            GroupSpec::new(2, 1, 2).unwrap(),
            lsn(0),
        )
        .unwrap();
        let mut seq = 0u64;
        while w.current_group() == 0 {
            advance_commit(&mut w, seq);
            seq += 1;
        }
        // 组 1 填满后阻塞：缺检查点。
        let mut blocked = None;
        for _ in 0..40 {
            match w.append(|l| RedoRecord::commit(l, 1, seq)) {
                Err(GroupError::Blocked(b)) => {
                    blocked = Some(b);
                    break;
                }
                Ok(_) => seq += 1,
                Err(e) => panic!("意外错误：{e}"),
            }
        }
        assert_eq!(blocked, Some(SwitchBlocked::AwaitingCheckpoint));
        // 检查点降级后（非归档模式）直接可复用——不需要归档。
        let (_, pages) = scan_group(&io, 0, 2);
        let progress = CheckpointProgress {
            checkpoint_lsn: lsn(pages * LOG_PAGE_SIZE as u64),
            ..CheckpointProgress::default()
        };
        w.publish_checkpoint(&progress).unwrap();
        assert_eq!(w.switch_group().unwrap(), 0);
        assert_eq!(w.entries().groups[0].archive, LogArchiveState::None);
    }

    #[test]
    fn reopen_resumes_at_next_page_boundary() {
        let io = mem();
        let spec = GroupSpec::new(2, 1, 8).unwrap();
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
            for seq in 0..3 {
                advance_commit(&mut w, seq);
            }
            w.flush(w.appended_lsn()).unwrap();
            assert!(w.synced_lsn() > lsn(0));
            w.close().unwrap();
        } // 控制文件与写者都放下（模拟进程结束）

        // 重开：当前组仍是组 0，续写落在**下一页边界**（页 0 的尾部空位不再用）。
        let mut cf = ControlFile::open(&io, Path::new(A), Path::new(B)).unwrap();
        let mut w = GroupWriter::open(&io, &mut cf, Path::new(WAL), spec).unwrap();
        assert_eq!(w.current_group(), 0);
        assert_eq!(w.current_sequence(), 1);
        assert_eq!(
            w.appended_lsn(),
            lsn(2 * LOG_PAGE_SIZE as u64 + 16),
            "续写起点 = 下一页（页 2）页头之后"
        );
        for seq in 3..5 {
            advance_commit(&mut w, seq);
        }
        w.flush(w.appended_lsn()).unwrap();
        let (records, pages) = scan_group(&io, 0, 8);
        assert_eq!(pages, 3, "切换记录页 + 两条记录页");
        assert_eq!(records.len(), 6, "切换记录 + 5 条（无重复、无丢失）");
        assert_eq!(records[0].op, RecordOp::LogSwitch.as_u8());
        for (i, r) in records[1..].iter().enumerate() {
            assert_eq!(r.commit_seq(), Some(i as u64), "第 {i} 条提交记录");
        }
    }

    #[test]
    fn force_switch_advances_to_next_group() {
        let io = mem();
        let mut cf = ControlFile::format(
            &io,
            Path::new(A),
            Path::new(B),
            &ws_entry(),
            &RedoEntries::new(2, 1).unwrap(),
            &ArchiveRecord::default(),
        )
        .unwrap();
        let mut w = GroupWriter::create(
            &io,
            &mut cf,
            Path::new(WAL),
            GroupSpec::new(2, 1, 4).unwrap(),
            lsn(0),
        )
        .unwrap();
        let next = w.switch_group().unwrap();
        assert_eq!(next, 1);
        assert_eq!(w.current_sequence(), 2);
        assert_eq!(w.entries().groups[0].run, LogRunState::Active);
        let (g1, _) = scan_group(&io, 1, 4);
        assert_eq!(g1[0], RedoRecord::log_switch(g1[0].lsn, 1, 2));
    }

    #[test]
    fn switch_point_feeds_a_wall_clock_sample_pair() {
        // 待讨论清单第 35 条：切换点自动补喂采样对——写线程从它写过的
        // **提交记录**里取提交序号，无需外部喂；时钟可注入（固定值）。
        fn fixed_clock() -> u64 {
            424_242
        }
        let io = mem();
        let mut cf = ControlFile::format(
            &io,
            Path::new(A),
            Path::new(B),
            &ws_entry(),
            &RedoEntries::new(3, 1).unwrap(),
            &ArchiveRecord::default(),
        )
        .unwrap();
        let mut w = GroupWriter::create(
            &io,
            &mut cf,
            Path::new(WAL),
            GroupSpec::new(3, 1, 4).unwrap(),
            lsn(0),
        )
        .unwrap();
        w.set_clock_ms(fixed_clock);
        // 首个组激活时无提交 ⇒ 不采样。
        let cf_ro = ControlFile::open(&io, Path::new(A), Path::new(B)).unwrap();
        assert!(cf_ro.sample_pairs().unwrap().is_empty());

        // 写一条提交记录（序号 7），再强制切换 ⇒ 落一对 (7, 424242)。
        w.append(|l| RedoRecord::commit(l, 1, 7)).unwrap();
        w.switch_group().unwrap();
        let cf_ro = ControlFile::open(&io, Path::new(A), Path::new(B)).unwrap();
        let seven = CommitSeq::from_raw(7).unwrap();
        assert_eq!(cf_ro.sample_pairs().unwrap(), vec![(seven, 424_242)]);

        // 再切换一次（期间无新提交）：序号不前进，时间戳更新——插值仍安全。
        w.switch_group().unwrap();
        let cf_ro = ControlFile::open(&io, Path::new(A), Path::new(B)).unwrap();
        assert_eq!(
            cf_ro.sample_pairs().unwrap(),
            vec![(seven, 424_242), (seven, 424_242)]
        );
    }

    #[test]
    fn one_third_trigger_flushes_automatically() {
        let io = mem();
        let mut cf = ControlFile::format(
            &io,
            Path::new(A),
            Path::new(B),
            &ws_entry(),
            &RedoEntries::new(2, 1).unwrap(),
            &ArchiveRecord::default(),
        )
        .unwrap();
        let mut w = GroupWriter::create(
            &io,
            &mut cf,
            Path::new(WAL),
            GroupSpec::new(2, 1, 256).unwrap(),
            lsn(0),
        )
        .unwrap();
        let initial = w.synced_lsn(); // 激活时切换记录已落盘（>0）
        assert!(initial > lsn(0), "首组切换记录已刷盘");
        // 每条 ≈ 1 KiB（记录 ≈ 1032 B ⇒ 3 页）；1/3 默认缓冲 ≈ 43.7 KiB ⇒
        // 约第 43 条触发自动刷盘（未显式调用 flush）。
        let mut appended = 0usize;
        for i in 0..60u64 {
            w.append(|l| {
                RedoRecord::page_modification(
                    l,
                    1,
                    vec![BlockRef {
                        flags: 0,
                        rdba: Rdba::from_parts(1, 2).unwrap(),
                        changes: vec![Change {
                            offset: 0,
                            after: vec![i as u8; 1000],
                        }],
                    }],
                )
            })
            .unwrap();
            appended += 1;
        }
        assert!(appended == 60);
        assert!(
            w.synced_lsn() > initial,
            "1/3 触发应已自动刷盘（synced_lsn 前进）"
        );
        // 全部记录最终都落盘（显式刷尽后计数一致）。
        w.flush(w.appended_lsn()).unwrap();
        let (records, _) = scan_group(&io, 0, 256);
        assert!(records.len() >= 60, "切换记录 + 60 条");
    }

    #[test]
    fn spec_validation() {
        assert!(matches!(GroupSpec::new(1, 1, 4), Err(GroupError::Spec(_))));
        assert!(matches!(GroupSpec::new(9, 1, 4), Err(GroupError::Spec(_))));
        // 多成员（镜像）本切片已支持；越界仍拒绝。
        assert!(GroupSpec::new(2, 2, 4).is_ok(), "双成员镜像合法");
        assert!(matches!(GroupSpec::new(2, 9, 4), Err(GroupError::Spec(_))));
        assert!(matches!(GroupSpec::new(2, 1, 0), Err(GroupError::Spec(_))));
        assert!(GroupSpec::new(2, 1, 34).is_ok());
    }

    #[test]
    fn record_larger_than_group_is_rejected() {
        let io = mem();
        let mut cf = ControlFile::format(
            &io,
            Path::new(A),
            Path::new(B),
            &ws_entry(),
            &RedoEntries::new(2, 1).unwrap(),
            &ArchiveRecord::default(),
        )
        .unwrap();
        let mut w = GroupWriter::create(
            &io,
            &mut cf,
            Path::new(WAL),
            GroupSpec::new(2, 1, 1).unwrap(),
            lsn(0),
        )
        .unwrap();
        // 编码长度 ≈ 20 + 8 + 4 + 600 > 496（一页可用数据）。
        let err = w
            .append(|l| {
                RedoRecord::page_modification(
                    l,
                    1,
                    vec![crate::record::BlockRef {
                        flags: 0,
                        rdba: crate::record::Rdba::from_parts(1, 2).unwrap(),
                        changes: vec![crate::record::Change {
                            offset: 0,
                            after: vec![0u8; 600],
                        }],
                    }],
                )
            })
            .unwrap_err();
        assert!(matches!(err, GroupError::RecordTooLarge { .. }), "{err}");
    }

    /// 可定向失败的 Io 包层（成员 m2 的写失败）。
    struct FlakyIo {
        inner: MemFileIo,
        paths: std::sync::Mutex<std::collections::HashMap<FileHandle, String>>,
        fail_on: std::sync::Mutex<Option<String>>,
    }

    impl FlakyIo {
        fn new() -> Self {
            Self {
                inner: MemFileIo::new(),
                paths: std::sync::Mutex::new(std::collections::HashMap::new()),
                fail_on: std::sync::Mutex::new(None),
            }
        }
        fn arm(&self, needle: &str) {
            *self.fail_on.lock().unwrap() = Some(needle.to_string());
        }
        fn disarm(&self) {
            *self.fail_on.lock().unwrap() = None;
        }
        fn contents(&self, path: &str) -> Option<Vec<u8>> {
            self.inner.contents(Path::new(path))
        }
    }

    impl bicdb_workspace::io::FileIo for FlakyIo {
        fn open(
            &self,
            path: &Path,
            opts: bicdb_workspace::io::OpenOptions,
        ) -> std::io::Result<FileHandle> {
            let h = self.inner.open(path, opts)?;
            self.paths
                .lock()
                .unwrap()
                .insert(h, path.to_string_lossy().to_string());
            Ok(h)
        }
        fn open_dir(&self, path: &Path) -> std::io::Result<FileHandle> {
            self.inner.open_dir(path)
        }
        fn read_at(&self, h: FileHandle, buf: &mut [u8], off: u64) -> std::io::Result<usize> {
            self.inner.read_at(h, buf, off)
        }
        fn write_at(&self, h: FileHandle, buf: &[u8], off: u64) -> std::io::Result<()> {
            if let Some(needle) = self.fail_on.lock().unwrap().as_ref() {
                let paths = self.paths.lock().unwrap();
                if paths.get(&h).is_some_and(|p| p.contains(needle.as_str())) {
                    return Err(std::io::Error::other("注入：目标成员写失败"));
                }
            }
            self.inner.write_at(h, buf, off)
        }
        fn size(&self, h: FileHandle) -> std::io::Result<u64> {
            self.inner.size(h)
        }
        fn set_len(&self, h: FileHandle, len: u64) -> std::io::Result<()> {
            self.inner.set_len(h, len)
        }
        fn sync_data(&self, h: FileHandle) -> std::io::Result<()> {
            self.inner.sync_data(h)
        }
        fn sync_all(&self, h: FileHandle) -> std::io::Result<()> {
            self.inner.sync_all(h)
        }
        fn sync_dir(&self, h: FileHandle) -> std::io::Result<()> {
            self.inner.sync_dir(h)
        }
        fn close(&self, h: FileHandle) -> std::io::Result<()> {
            self.inner.close(h)
        }
    }

    #[test]
    fn members_receive_identical_bytes() {
        let io = mem();
        let mut cf = ControlFile::format(
            &io,
            Path::new(A),
            Path::new(B),
            &ws_entry(),
            &RedoEntries::new(2, 2).unwrap(),
            &ArchiveRecord::default(),
        )
        .unwrap();
        let mut w = GroupWriter::create(
            &io,
            &mut cf,
            Path::new(WAL),
            GroupSpec::new(2, 2, 8).unwrap(),
            lsn(0),
        )
        .unwrap();
        w.append(|l| RedoRecord::commit(l, 7, 1)).unwrap();
        w.append(|l| RedoRecord::commit(l, 8, 2)).unwrap();
        w.flush(w.appended_lsn()).unwrap();

        let m1 = io
            .contents(Path::new(&format!("{WAL}/{}", member_file_name(0, 0))))
            .unwrap();
        let m2 = io
            .contents(Path::new(&format!("{WAL}/{}", member_file_name(0, 1))))
            .unwrap();
        assert_eq!(m1, m2, "两成员逐字节一致");
        assert!(m1.iter().any(|&b| b != 0), "确实写了内容");
        assert_eq!(w.member_stale(0), 0);
    }

    #[test]
    fn member_failure_marks_stale_and_rebuild_restores() {
        let io = FlakyIo::new();
        io.inner.add_dir("/mem");
        io.inner.add_dir(WAL);
        let mut cf = ControlFile::format(
            &io,
            Path::new(A),
            Path::new(B),
            &ws_entry(),
            &RedoEntries::new(2, 2).unwrap(),
            &ArchiveRecord::default(),
        )
        .unwrap();
        let mut w = GroupWriter::create(
            &io,
            &mut cf,
            Path::new(WAL),
            GroupSpec::new(2, 2, 8).unwrap(),
            lsn(0),
        )
        .unwrap();
        w.flush(w.appended_lsn()).unwrap();
        let before = io
            .contents(&format!("{WAL}/{}", member_file_name(0, 1)))
            .unwrap();

        // 成员 2 的写从此失败：继续写入应降级为单成员 + 标 STALE。
        io.arm("_m2");
        w.append(|l| RedoRecord::commit(l, 9, 3)).unwrap();
        w.flush(w.appended_lsn()).unwrap();
        assert_eq!(w.member_stale(0), 0b10, "成员 2（bit1）被标 STALE");
        let good = io
            .contents(&format!("{WAL}/{}", member_file_name(0, 0)))
            .unwrap();
        let bad = io
            .contents(&format!("{WAL}/{}", member_file_name(0, 1)))
            .unwrap();
        assert_ne!(good, bad, "坏成员落后");
        assert_eq!(bad, before, "坏成员自失败起不再改写");

        // 重建：复制已用前缀 → 逐字节一致、位清除。
        io.disarm();
        w.rebuild_member(0, 1).unwrap();
        assert_eq!(w.member_stale(0), 0);
        let rebuilt = io
            .contents(&format!("{WAL}/{}", member_file_name(0, 1)))
            .unwrap();
        assert_eq!(rebuilt, good, "重建后两成员一致");
        // 控制文件里的位也清了。
        let entries = cf.redo_entries().unwrap();
        assert_eq!(entries.groups[0].member_stale, 0);
    }

    /// **后台标脏必须在下次前台发布时进控制文件**（2026-10-06 审计发现的缺陷）：
    /// LGWR/池经 `WalShared` 刷盘时没有控制文件可写；若前台发布只看"本次是否
    /// 新失败"，位一旦置上（该成员此后被跳过、不再产生"新失败"）就再也发布
    /// 不出去 ⇒ 重启后仍以为成员健康、`rebuild_member` 也以"没坏"静默拒绝。
    #[test]
    fn background_stale_is_published_by_the_next_foreground_flush() {
        let io = FlakyIo::new();
        io.inner.add_dir("/mem");
        io.inner.add_dir(WAL);
        let mut cf = ControlFile::format(
            &io,
            Path::new(A),
            Path::new(B),
            &ws_entry(),
            &RedoEntries::new(2, 2).unwrap(),
            &ArchiveRecord::default(),
        )
        .unwrap();
        let mut w = GroupWriter::create(
            &io,
            &mut cf,
            Path::new(WAL),
            GroupSpec::new(2, 2, 8).unwrap(),
            lsn(0),
        )
        .unwrap();
        w.flush(w.appended_lsn()).unwrap();

        w.append(|l| RedoRecord::commit(l, 9, 3)).unwrap();
        io.arm("_m2");
        // **后台路径**（模拟 LGWR）：经共享核心刷盘——它写不了控制文件。
        let shared = w.shared();
        let out = shared.flush_to(shared.appended_lsn()).unwrap();
        assert!(out.stale_changed, "本次刷盘新标脏");
        assert_eq!(shared.member_stale(0), 0b10);
        {
            let cf_ro = ControlFile::open(&io, Path::new(A), Path::new(B)).unwrap();
            assert_eq!(
                cf_ro.redo_entries().unwrap().groups[0].member_stale,
                0,
                "后台没有发布口——此刻控制文件确实还没这个位（这正是要修的）"
            );
        }

        // 下一次前台刷盘：**取走待发布位**并落控制文件。
        w.flush(w.appended_lsn()).unwrap();
        let cf_ro = ControlFile::open(&io, Path::new(A), Path::new(B)).unwrap();
        assert_eq!(
            cf_ro.redo_entries().unwrap().groups[0].member_stale,
            0b10,
            "后台标的位由下一次前台发布带上"
        );
        // 也顺带钉住：`rebuild_member` 认这个位（不会以"没坏"静默返回）。
        io.disarm();
        w.rebuild_member(0, 1).unwrap();
        assert_eq!(w.member_stale(0), 0, "重建后清位");
    }

    #[test]
    fn open_uses_the_longest_member() {
        let io = mem();
        let mut cf = ControlFile::format(
            &io,
            Path::new(A),
            Path::new(B),
            &ws_entry(),
            &RedoEntries::new(2, 2).unwrap(),
            &ArchiveRecord::default(),
        )
        .unwrap();
        let mut w = GroupWriter::create(
            &io,
            &mut cf,
            Path::new(WAL),
            GroupSpec::new(2, 2, 8).unwrap(),
            lsn(0),
        )
        .unwrap();
        for i in 0..5u64 {
            w.append(|l| RedoRecord::commit(l, i + 1, i + 1)).unwrap();
        }
        w.flush(w.appended_lsn()).unwrap();
        let end_before = w.group_end_lsn(0).unwrap();
        w.close().unwrap();

        // 截掉成员 1 的一半（模拟定长文件被截断）：重开应挑更长的成员 2。
        let h = io
            .open(
                Path::new(&format!("{WAL}/{}", member_file_name(0, 0))),
                bicdb_workspace::io::OpenOptions::new()
                    .read(true)
                    .write(true),
            )
            .unwrap();
        io.set_len(h, 512).unwrap();
        io.close(h).unwrap();

        let w = GroupWriter::open(
            &io,
            &mut cf,
            Path::new(WAL),
            GroupSpec::new(2, 2, 8).unwrap(),
        )
        .unwrap();
        assert_eq!(w.group_end_lsn(0), Some(end_before), "以更长成员为准续写");
        assert_eq!(w.current_group(), 0);
    }

    #[test]
    fn member0_failure_does_not_wedge_subsequent_flushes() {
        // 审核修复回归（D2）：**成员 0** 一次瞬时写失败后，组已写页数必须取
        // 未失败成员的最大值——取成员 0 的落后计数会让下一次 flush 位置不符
        // （"页自带 1024，按序应为 512"），WAL 永久写不出去。
        let io = FlakyIo::new();
        io.inner.add_dir("/mem");
        io.inner.add_dir(WAL);
        let mut cf = ControlFile::format(
            &io,
            Path::new(A),
            Path::new(B),
            &ws_entry(),
            &RedoEntries::new(2, 2).unwrap(),
            &ArchiveRecord::default(),
        )
        .unwrap();
        let mut w = GroupWriter::create(
            &io,
            &mut cf,
            Path::new(WAL),
            GroupSpec::new(2, 2, 8).unwrap(),
            lsn(0),
        )
        .unwrap();
        w.flush(w.appended_lsn()).unwrap();

        // 成员 1（`_m1`）的写从此失败：本次降级、成员 2 继续。
        io.arm("_m1");
        w.append(|l| RedoRecord::commit(l, 9, 3)).unwrap();
        w.flush(w.appended_lsn()).unwrap();
        assert_eq!(w.member_stale(0), 0b01, "成员 1（bit0）被标 STALE");

        // **后续 flush 必须还能工作**（旧代码从此永久"位置不符"）。
        io.disarm();
        w.append(|l| RedoRecord::commit(l, 10, 4)).unwrap();
        w.flush(w.appended_lsn()).unwrap();

        // 重建成员 1：复制健康前缀 → 两成员一致、位清除、继续可写。
        w.rebuild_member(0, 0).unwrap();
        assert_eq!(w.member_stale(0), 0);
        w.append(|l| RedoRecord::commit(l, 11, 5)).unwrap();
        w.flush(w.appended_lsn()).unwrap();
        let m1 = io
            .contents(&format!("{WAL}/{}", member_file_name(0, 0)))
            .unwrap();
        let m2 = io
            .contents(&format!("{WAL}/{}", member_file_name(0, 1)))
            .unwrap();
        assert_eq!(m1, m2, "重建后两成员逐字节一致");
    }

    #[test]
    fn mid_log_hole_is_reported_not_silently_truncated() {
        // 审核修复回归：**空页断点之后仍有非零页**（中部空洞）——必须报损坏，
        // 不得当作"尾部截断"把洞之后的记录静默丢掉。
        let io = mem();
        let mut cf = ControlFile::format(
            &io,
            Path::new(A),
            Path::new(B),
            &ws_entry(),
            &RedoEntries::new(2, 1).unwrap(),
            &ArchiveRecord::default(),
        )
        .unwrap();
        let spec = GroupSpec::new(2, 1, 8).unwrap();
        let mut w = GroupWriter::create(&io, &mut cf, Path::new(WAL), spec, lsn(0)).unwrap();
        for i in 0..40u64 {
            w.append(|l| RedoRecord::commit(l, i + 1, i + 1)).unwrap();
        }
        w.flush(w.appended_lsn()).unwrap();
        assert!(scan_group(&io, 0, 8).1 >= 3, "至少 3 页");
        w.close().unwrap();

        // 把第 2 张页清零（洞），其后仍有数据。
        let h = io
            .open(
                Path::new(&format!("{WAL}/{}", member_file_name(0, 0))),
                bicdb_workspace::io::OpenOptions::new()
                    .read(true)
                    .write(true),
            )
            .unwrap();
        io.write_at(h, &[0u8; LOG_PAGE_SIZE], LOG_PAGE_SIZE as u64)
            .unwrap();
        io.close(h).unwrap();

        let mut cf2 = ControlFile::open(&io, Path::new(A), Path::new(B)).unwrap();
        let err = GroupWriter::open(&io, &mut cf2, Path::new(WAL), spec).unwrap_err();
        assert!(
            matches!(err, GroupError::Damaged { reason, .. } if reason.contains("空页")),
            "{err}"
        );
    }

    #[test]
    fn reused_group_is_cleared_so_shorter_cycle_reopens() {
        // 审核修复回归（D1）：复用组必须先清空——新周期写得比上一轮少时，
        // 旧周期的页会以"中部坏页"形态让重开判损坏（崩溃恢复不可用）。
        let io = mem();
        let mut cf = ControlFile::format(
            &io,
            Path::new(A),
            Path::new(B),
            &ws_entry(),
            &RedoEntries::new(2, 1).unwrap(),
            // **显式开归档**：本用例钉的是归档门控下的日志切换（默认是不归档）。
            &ArchiveRecord::new(bicdb_storage::controlfile::ArchiveMode::ArchiveLog),
        )
        .unwrap();
        let spec = GroupSpec::new(2, 1, 4).unwrap();
        let mut w = GroupWriter::create(&io, &mut cf, Path::new(WAL), spec, lsn(0)).unwrap();
        // 组 0 写满（4 页）后自动切到组 1；组 1 再写 1 页。
        let mut seq = 0u64;
        while w.current_group() == 0 {
            advance_commit(&mut w, seq);
            seq += 1;
        }
        advance_commit(&mut w, seq);
        seq += 1;
        w.flush(w.appended_lsn()).unwrap();
        let g0_pages = scan_group(&io, 0, 4).1;
        assert_eq!(g0_pages, 4, "上一周期组 0 写满了 4 页");

        // 组 0 越过检查点 → INACTIVE；归档完成 → 可复用。
        let progress = CheckpointProgress {
            checkpoint_lsn: lsn(g0_pages * LOG_PAGE_SIZE as u64),
            ..CheckpointProgress::default()
        };
        w.publish_checkpoint(&progress).unwrap();
        w.archive_done(0).unwrap();
        assert_eq!(w.switch_group().unwrap(), 0);

        // 组 0 本轮只写 1 页（远少于上一周期的 4 页）。
        advance_commit(&mut w, seq);
        w.flush(w.appended_lsn()).unwrap();
        assert_eq!(scan_group(&io, 0, 4).1, 2, "本轮 = 切换记录 + 1 条");
        w.close().unwrap();

        // **重开**：不得因上一周期残页判损坏（旧代码在此报"中部有坏页"）。
        let mut cf2 = ControlFile::open(&io, Path::new(A), Path::new(B)).unwrap();
        GroupWriter::open(&io, &mut cf2, Path::new(WAL), spec).expect("复用组重开不得判损坏");
        let groups = online_groups(&io, &cf2, Path::new(WAL), spec).unwrap();
        assert!(groups.iter().any(|g| g.group == 0), "组 0 仍在线");
    }

    #[test]
    fn open_falls_back_to_healthy_member_when_one_is_damaged() {
        // 审核修复回归（D3）：单成员**中部损坏**不得让整组判损坏——健康
        // 镜像必须救得回来（§11.9 的降级语义）。
        let io = mem();
        let mut cf = ControlFile::format(
            &io,
            Path::new(A),
            Path::new(B),
            &ws_entry(),
            &RedoEntries::new(2, 2).unwrap(),
            &ArchiveRecord::default(),
        )
        .unwrap();
        let spec = GroupSpec::new(2, 2, 8).unwrap();
        let mut w = GroupWriter::create(&io, &mut cf, Path::new(WAL), spec, lsn(0)).unwrap();
        // 写 ≥3 页，让成员 1 的第 2 页成为"中部"页。
        for i in 0..40u64 {
            w.append(|l| RedoRecord::commit(l, i + 1, i + 1)).unwrap();
        }
        w.flush(w.appended_lsn()).unwrap();
        assert!(scan_group(&io, 0, 8).1 >= 3, "至少 3 页");
        let end_before = w.group_end_lsn(0).unwrap();
        w.close().unwrap();

        // 打坏成员 1 的第 2 页（其后仍有数据 ⇒ 中部坏页，不是尾部截断）。
        let h = io
            .open(
                Path::new(&format!("{WAL}/{}", member_file_name(0, 0))),
                bicdb_workspace::io::OpenOptions::new()
                    .read(true)
                    .write(true),
            )
            .unwrap();
        io.write_at(h, &[0xAB; LOG_PAGE_SIZE], LOG_PAGE_SIZE as u64)
            .unwrap();
        io.close(h).unwrap();

        // 重开：健康成员 2 顶替（旧代码在此报"中部有坏页而其后仍有数据"）。
        let w = GroupWriter::open(&io, &mut cf, Path::new(WAL), spec).expect("健康镜像顶替");
        assert_eq!(w.group_end_lsn(0), Some(end_before), "以健康成员为准");
        let groups = online_groups(&io, &cf, Path::new(WAL), spec).unwrap();
        assert!(groups.iter().any(|g| g.group == 0));
    }
}
