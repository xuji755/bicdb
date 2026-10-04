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
//! # 成员（2 份）暂未实现
//!
//! 本切片先做**单成员**（`member_count = 1`）；镜像冗余与 `STALE` 重建
//! 随后——§11.9 把 `STALE` 记在"成员文件头页"，而我们的 redo 文件是
//! **纯页流、无文件头页**，落点需先定案（已记入待讨论清单）。

use std::io;
use std::path::{Path, PathBuf};

use bicdb_common::seq::Lsn;
use bicdb_storage::controlfile::{
    ArchiveMode, CheckpointProgress, ControlFile, ControlFileError, LogArchiveState, LogRunState,
    RedoEntries, RedoGroup, MAX_REDO_GROUPS, NO_CURRENT_GROUP,
};
use bicdb_workspace::io::{FileHandle, FileIo, OpenOptions};

use crate::buffer::{LogBuffer, WalError};
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
        if member_count != 1 {
            return Err(GroupError::Spec("本切片仅支持单成员（成员镜像随后）"));
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

/// 日志组写者。
pub struct GroupWriter<'io, 'cf> {
    io: &'io dyn FileIo,
    cf: &'cf mut ControlFile<'io>,
    spec: GroupSpec,
    dir: PathBuf,
    /// `files[组][成员]`。
    files: Vec<Vec<FileHandle>>,
    /// 控制文件 Redo 条目的**内存镜像**（单写者；每次发布后更新）。
    entries: RedoEntries,
    archive_mode: ArchiveMode,
    buffer: LogBuffer,
    /// 当前组（0 起）。
    current: u8,
    /// 当前组的起始 LSN（= 其首张页的位置）。
    current_start: Lsn,
    /// 每组成员已刷出的页数（单成员先行）。
    written_pages: Vec<u64>,
    /// 各组的结尾 LSN（最后一张已写页的终点；未用过的组为 `None`）。
    group_ends: [Option<Lsn>; MAX_REDO_GROUPS],
    /// 本组自激活以来追加的记录数（0 ⇒ 强制切换无需刷盘）。
    records_in_group: u64,
}

impl std::fmt::Debug for GroupWriter<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GroupWriter")
            .field("spec", &self.spec)
            .field("current", &self.current)
            .field("current_start", &self.current_start)
            .field("appended_lsn", &self.buffer.appended_lsn())
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
        let mut writer = Self {
            io,
            cf,
            spec,
            dir: dir.to_path_buf(),
            files,
            entries,
            archive_mode,
            buffer: LogBuffer::new(start_lsn),
            current: 0,
            current_start: start_lsn,
            written_pages: vec![0; spec.group_count as usize],
            group_ends: [None; MAX_REDO_GROUPS],
            records_in_group: 0,
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
        for g in 0..spec.group_count as usize {
            let used =
                entries.groups[g].run != LogRunState::Unused || entries.groups[g].sequence != 0;
            if !used {
                continue; // 从未用过：文件保持全零
            }
            let (pages, start) = scan_used_group(io, files[g][0], g as u8, spec.group_pages)?;
            written_pages[g] = pages;
            group_ends[g] = Some(lsn_add(start, pages * LOG_PAGE_SIZE as u64)?);
        }

        let current_start =
            read_group_start(io, files[current as usize][0])?.ok_or(GroupError::Damaged {
                group: current,
                reason: "当前组无内容（切换记录缺失）",
            })?;
        let resume = group_ends[current as usize].ok_or(GroupError::Damaged {
            group: current,
            reason: "当前组无内容（切换记录缺失）",
        })?;

        Ok(Self {
            io,
            cf,
            spec,
            dir: dir.to_path_buf(),
            files,
            entries,
            archive_mode,
            buffer: LogBuffer::new(resume),
            current,
            current_start,
            written_pages,
            group_ends,
            records_in_group: 0,
        })
    }

    // -- 追加与刷盘 ----------------------------------------------------------

    /// **追加一条记录**：latch 内分配 LSN 并分片入页。
    ///
    /// 放不下当前组时**自动切换**（记录不得跨组）后重试——因此 `build`
    /// 在切换路径上会被调用**两次**，必须是**纯构造**（无副作用）。
    pub fn append(&mut self, build: impl Fn(Lsn) -> RedoRecord) -> Result<Lsn, GroupError> {
        // **1/3 触发**（§11.5.5）：占用达阈值先刷盘——单次刷盘体量有界。
        if self.buffer.flush_recommended() {
            self.flush(self.buffer.appended_lsn())?;
        }
        let mut attempt = 0u32;
        loop {
            match self.try_append(&build) {
                // **满则刷 + 重试**（单写者的空间等待形态）。
                Err(GroupError::Buffer(WalError::BufferFull { .. })) if attempt < 2 => {
                    self.flush(self.buffer.appended_lsn())?;
                    attempt += 1;
                }
                other => return other,
            }
        }
    }

    /// 追加的实际路径（容量不足时返回 [`WalError::BufferFull`]，由
    /// [`GroupWriter::append`] 刷盘后重试）。
    fn try_append(&mut self, build: &impl Fn(Lsn) -> RedoRecord) -> Result<Lsn, GroupError> {
        let probe = self.buffer.appended_lsn();
        let record = build(probe);
        let record = if self.buffer.end_lsn_if_appended(record.encoded_len()) > self.group_end() {
            self.switch_group()?;
            let lsn = self.buffer.appended_lsn();
            let rebuilt = build(lsn);
            if self.buffer.end_lsn_if_appended(rebuilt.encoded_len()) > self.group_end() {
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
        let mut reuse = Some(record);
        let lsn = self.buffer.append(|lsn| match reuse.take() {
            Some(r) if r.lsn == lsn => r,
            _ => build(lsn),
        })?;
        self.records_in_group += 1;
        Ok(lsn)
    }

    /// **刷盘到 `target`**（组提交语义由 [`LogBuffer`] 承担）：把当前组的
    /// 未刷页写入其成员文件并 sync；失败时页被放回（重试可继续）。
    pub fn flush(&mut self, target: Lsn) -> Result<Lsn, GroupError> {
        let handle = self.files[self.current as usize][0];
        let mut sink = FileLogSink::resume(
            self.io,
            handle,
            self.current_start,
            self.spec.group_pages as u64,
            self.written_pages[self.current as usize],
        );
        let synced = self.buffer.flush_to(target, &mut sink)?;
        self.written_pages[self.current as usize] = sink.written_pages();
        self.group_ends[self.current as usize] = Some(lsn_add(
            self.current_start,
            sink.written_pages() * LOG_PAGE_SIZE as u64,
        )?);
        Ok(synced)
    }

    // -- 切换 ---------------------------------------------------------------

    /// **自动切换**（当前组放不下时由 [`GroupWriter::append`] 调用，
    /// 也可显式调用作**强制切换**）：执行 §11.9 步骤 1–3。
    pub fn switch_group(&mut self) -> Result<u8, GroupError> {
        let next = self.pick_reusable()?;
        self.activate(next)
    }

    /// 从当前组的下一组起轮转，找可复用组；找不到 ⇒ 按缺的条件分类。
    fn pick_reusable(&self) -> Result<u8, GroupError> {
        let mut blocked_archive = false;
        let mut blocked_checkpoint = false;
        for step in 1..=self.spec.group_count {
            let g = (self.current + step) % self.spec.group_count;
            if g == self.current {
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
        let old = self.current;

        // 1) 刷尽旧组（末页的尾部空位留在原处；新组从下一张页起）。
        if !first && self.records_in_group > 0 {
            self.flush(self.buffer.appended_lsn())?;
        }
        let new_start = self.buffer.current_page_start();
        let seq = if first {
            1
        } else {
            self.entries.groups[old as usize].sequence + 1
        };

        // 2) 切换记录写进新组并落盘（**先于** CURRENT 发布——见模块文档）。
        self.current = next;
        self.current_start = new_start;
        self.written_pages[next as usize] = 0;
        self.group_ends[next as usize] = Some(new_start);
        self.records_in_group = 0;
        self.buffer
            .append(|l| RedoRecord::log_switch(l, next, seq))?;
        let end = self.buffer.appended_lsn();
        self.flush(end)?;
        self.records_in_group = 1;

        // 3) 控制文件发布。
        if !first {
            self.group_ends[old as usize] = Some(new_start);
            let old_entry = &mut self.entries.groups[old as usize];
            old_entry.run = LogRunState::Active;
            if self.archive_mode == ArchiveMode::ArchiveLog {
                old_entry.archive = LogArchiveState::Needed;
            }
        }
        self.entries.groups[next as usize] = RedoGroup {
            sequence: seq,
            run: LogRunState::Current,
            archive: LogArchiveState::None,
        };
        self.entries.current_group = next;
        self.cf.write_redo_entries(&self.entries)?;
        Ok(next)
    }

    // -- 检查点与归档（状态迁移算法的发布口）----------------------------------

    /// **检查点发布**（CKPT 角色）：把"**组结尾 LSN ≤ 检查点低水位**"的
    /// `ACTIVE` 组降为 `INACTIVE`，与低水位**同一次控制文件更新**发布
    /// （§11.9 的发布纪律）。
    pub fn publish_checkpoint(&mut self, progress: &CheckpointProgress) -> Result<(), GroupError> {
        for g in 0..self.spec.group_count as usize {
            if self.entries.groups[g].run != LogRunState::Active {
                continue;
            }
            if let Some(end) = self.group_ends[g] {
                if end <= progress.checkpoint_lsn {
                    self.entries.groups[g].run = LogRunState::Inactive;
                }
            }
        }
        self.cf
            .write_checkpoint_and_groups(progress, &self.entries)?;
        Ok(())
    }

    /// **归档完成发布**（ARCn 角色的替身，归档切片接管）：
    /// 组归档状态 `NEEDED` → `DONE`，并推进归档记录的"最后归档序列号"。
    pub fn archive_done(&mut self, group: u8) -> Result<(), GroupError> {
        let idx = usize::from(group);
        if idx >= self.spec.group_count as usize {
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
        self.current
    }

    /// 当前组的日志序列号。
    #[must_use]
    pub fn current_sequence(&self) -> u32 {
        self.entries.groups[self.current as usize].sequence
    }

    /// 当前组的结尾 LSN（= 起始 + 组字节大小）。
    #[must_use]
    pub fn group_end(&self) -> Lsn {
        lsn_add(self.current_start, self.spec.group_bytes()).expect("48 位域内")
    }

    /// 追加位置（下一字节 LSN）。
    #[must_use]
    pub fn appended_lsn(&self) -> Lsn {
        self.buffer.appended_lsn()
    }

    /// 已刷盘位置。
    #[must_use]
    pub fn synced_lsn(&self) -> Lsn {
        self.buffer.synced_lsn()
    }

    /// Redo 条目的内存镜像（诊断/测试）。
    #[must_use]
    pub fn entries(&self) -> &RedoEntries {
        &self.entries
    }

    /// 规格。
    #[must_use]
    pub fn spec(&self) -> GroupSpec {
        self.spec
    }

    /// 组目录。
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 关闭所有成员文件句柄。
    pub fn close(self) -> Result<(), GroupError> {
        for members in &self.files {
            for &h in members {
                self.io.close(h)?;
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
    let mut out = Vec::new();
    for g in 0..spec.group_count as usize {
        let entry = entries.groups[g];
        if entry.run == LogRunState::Unused && entry.sequence == 0 {
            continue;
        }
        let path = dir.join(member_file_name(g as u8, 0));
        let handle = io.open(&path, OpenOptions::new().read(true))?;
        let (pages, start) = scan_used_group(io, handle, g as u8, spec.group_pages)?;
        out.push(OnlineGroup {
            group: g as u8,
            sequence: entry.sequence,
            start_lsn: start,
            end_lsn: lsn_add(start, pages * LOG_PAGE_SIZE as u64)?,
            handle,
            file_pages: spec.group_pages,
        });
    }
    out.sort_by_key(|g| g.sequence);
    for w in out.windows(2) {
        if w[0].sequence == w[1].sequence {
            return Err(GroupError::Spec("在线组序列号重复——控制文件不自洽"));
        }
    }
    Ok(out)
}

/// 扫描一个已用组：返回（已写页数，组起点 LSN）。
///
/// 组的尾部残缺（坏页/空页）与之区分：其后仍有非零页 ⇒ **中部坏页**，按损坏拒绝。
fn scan_used_group(
    io: &dyn FileIo,
    handle: FileHandle,
    group: u8,
    group_pages: u32,
) -> Result<(u64, Lsn), GroupError> {
    let start = read_group_start(io, handle)?.ok_or(GroupError::Damaged {
        group,
        reason: "组状态非 UNUSED 但文件为空",
    })?;
    let scan = scan_log(io, handle, start, group_pages as u64)?;
    if let Some(bad) = scan.first_bad_page {
        if any_nonzero_page_after(io, handle, bad + 1, group_pages as u64)? {
            return Err(GroupError::Damaged {
                group,
                reason: "中部有坏页而其后仍有数据",
            });
        }
    }
    Ok((scan.pages_scanned, start))
}

/// 读组的首张页并将其起始 LSN 作为组起点；整页全零 = 从未写过（`None`）。
fn read_group_start(io: &dyn FileIo, handle: FileHandle) -> Result<Option<Lsn>, GroupError> {
    let mut buf = Box::new([0u8; LOG_PAGE_SIZE]);
    io.read_exact_at(handle, buf.as_mut_slice(), 0)?;
    if buf.iter().all(|&b| b == 0) {
        return Ok(None);
    }
    let page = LogPage::from_bytes(buf);
    page.verify().map_err(|_| GroupError::Damaged {
        group: 0,
        reason: "组首张页校验和不符",
    })?;
    Ok(Some(page.start_lsn()))
}

/// 从页号 `from` 起，是否存在非零页（用于区分"组尾截断"与"中部坏页"）。
fn any_nonzero_page_after(
    io: &dyn FileIo,
    handle: FileHandle,
    from: u64,
    file_pages: u64,
) -> Result<bool, GroupError> {
    let mut buf = Box::new([0u8; LOG_PAGE_SIZE]);
    for page in from..file_pages {
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
        let start = read_group_start(io, h).unwrap().expect("已写过");
        let result = scan_log(io, h, start, pages as u64).unwrap();
        (result.records, result.pages_scanned)
    }

    fn advance_commit(w: &mut GroupWriter, seq: u64) -> Lsn {
        w.append(|l| RedoRecord::commit(l, 1, seq)).unwrap()
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
            &ArchiveRecord::default(), // 归档模式默认开启
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
        assert!(matches!(GroupSpec::new(2, 2, 4), Err(GroupError::Spec(_))));
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
}
