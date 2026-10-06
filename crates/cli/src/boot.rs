//! **实例建区 / 打开 / 关闭**（`目录详设` §5.1 的文件面 + 控制文件/日志装配）。
//!
//! ```text
//! <dir>/file0.dat   工作区字典（role = 0，带式排布）
//! <dir>/undo.dat    撤销段（V1.0 单段）
//! <dir>/wal/        日志组目录（2 组 × 1 成员）
//! <dir>/cf_a, cf_b  控制文件双副本
//! ```
//!
//! **三条纪律**（都在本模块落实，缺一不可）：
//!
//! 1. **WAL 规则 2**：池的 [`WalGuard`] 就是 `GroupWriter::shared()`——
//!    页写回前先把 redo 刷到该页的 `page_lsn`（不是"假称已耐久"）；
//! 2. **打开即恢复**：`open` 先跑三阶段恢复（分析 → 重做 → 输家回滚），
//!    **再**建池（顺序不可换：池会在读页时缓存，恢复要写的是文件）；
//! 3. **关闭即完全检查点**：脏页按序写回 + 发布低水位（§11.7），
//!    下次打开的重做范围从一个干净的锚点开始。
//!
//! **生命周期记档**：常驻件（I/O、池、引擎、控制文件）`Box::leak` 成 `'static`
//! ——对**单进程 CLI** 而言"进程生命周期 = 实例生命周期"，这不是取巧而是语义
//! 本身（引擎的类型参数要求实例级借用）。daemon 化时改为实例结构体持有。

use std::path::{Path, PathBuf};

use bicdb_catalog::{create_dictionary, ddl, Catalog};
use bicdb_common::seq::{CommitSeq, Lsn};
use bicdb_storage::bitmap::{ExtentNo, FileLayout, META_ROLE};
use bicdb_storage::buffer::{BufferPool, CacheConfig, SystemClock};
use bicdb_storage::controlfile::{
    ArchiveMode, ArchiveRecord, ControlFile, RedoEntries, WorkspaceEntry,
};
use bicdb_storage::datafile::DataFile;
use bicdb_storage::rowid::Rdba;
use bicdb_storage::segment::Segment;
use bicdb_storage::undo::{create_undo_segment, UndoChain};
use bicdb_txn::engine::Engine;
use bicdb_wal::group::{online_groups, GroupSpec, GroupWriter};
use bicdb_wal::recovery::recover;
use bicdb_workspace::io::{FileHandle, FileIo, OsFileIo};

use crate::lock::{self, InstanceLock, LockError, LockMode};
use bicdb_workspace::WorkspaceId;

/// 工作区标识（V1.0 单工作区 CLI：常量；多工作区随 daemon/DCL）。
pub const WS: [u8; 8] = *b"bicdb001";

/// file 0 的物理文件号（池的定位表按它分派）。
const FILE0_ID: u16 = 0;
/// 撤销段的物理文件号。
const UNDO_ID: u16 = 1;

/// 字典文件名（也是"这是个 bicdb 实例"的判据）。
pub const FILE0: &str = "file0.dat";
/// 撤销文件名。
pub const UNDO: &str = "undo.dat";
/// 日志组目录名。
pub const WAL_DIR: &str = "wal";
const CF_A: &str = "cf_a";
const CF_B: &str = "cf_b";

/// 建区时 file 0 的初始块数（`FileLayout::meta().min_file_blocks()` + 余量；
/// 段扩展会按需长大，这个数字只是"免去建区后立刻扩文件"）。
const FILE0_BLOCKS: u64 = 4096;

/// **缓冲区帧数**（16 KiB/帧 ⇒ 4 MiB）。
const POOL_FRAMES: usize = 256;

/// 建区/打开错误。
#[derive(Debug)]
pub enum BootError {
    /// I/O（含各层的 I/O 类错误）。
    Io(std::io::Error),
    /// 目录（字典）层。
    Catalog(String),
    /// **实例被别的进程占着**（单写者纪律；见 `lock.rs`）。
    Occupied(String),
}

impl std::fmt::Display for BootError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BootError::Io(e) => write!(f, "I/O：{e}"),
            BootError::Catalog(w) => write!(f, "目录：{w}"),
            BootError::Occupied(w) => f.write_str(w),
        }
    }
}

impl std::error::Error for BootError {}

impl From<std::io::Error> for BootError {
    fn from(e: std::io::Error) -> Self {
        BootError::Io(e)
    }
}

/// 各层的结构化错误 → `BootError`：**保留原文**（诊断要能追到层）。
macro_rules! from_io {
    ($($t:ty),* $(,)?) => { $(impl From<$t> for BootError {
        fn from(e: $t) -> Self { BootError::Io(std::io::Error::other(e.to_string())) }
    })* };
}

impl From<LockError> for BootError {
    fn from(e: LockError) -> Self {
        match e {
            LockError::Occupied { pid, mode } => BootError::Occupied(format!(
                "实例被 pid {pid} 占用（{}）——服务在跑时请用 `bicdbcli`/`bicdb stop` 连它",
                match mode {
                    LockMode::Service => "服务模式",
                    LockMode::Direct => "直连模式",
                }
            )),
            other => BootError::Io(std::io::Error::other(other.to_string())),
        }
    }
}

from_io!(
    bicdb_storage::datafile::DataFileError,
    bicdb_storage::controlfile::ControlFileError,
    bicdb_storage::undo::UndoSegmentError,
    bicdb_storage::segment::SegmentSpaceError,
    bicdb_storage::pagefile::PageFileError,
    bicdb_storage::buffer::BufferError,
    bicdb_wal::group::GroupError,
    bicdb_wal::file::LogFileError,
    bicdb_wal::recovery::RecoveryError,
    bicdb_wal::checkpoint::CheckpointError,
);

fn p(dir: &Path, name: &str) -> PathBuf {
    dir.join(name)
}

fn seq(v: u64) -> CommitSeq {
    CommitSeq::from_raw(v).expect("48 位域内")
}

fn lsn(v: u64) -> Lsn {
    Lsn::from_raw(v).expect("48 位域内")
}

/// 撤销段头块（V1.0 单段 ⇒ 标准布局首区）。
fn undo_page0() -> u32 {
    FileLayout::standard().first_block_of(ExtentNo::from_raw(0).expect("域内"))
}

/// **日志组规格**（V1.0 固定：2 组 × 1 成员；与控制文件条目一致）。
#[must_use]
pub fn group_spec() -> GroupSpec {
    GroupSpec::new(2, 1, 8192).expect("日志组参数为常量")
}

fn workspace_entry() -> WorkspaceEntry {
    WorkspaceEntry {
        workspace_id: WorkspaceId::from_raw(1).expect("域内"),
        created_at: 0,
        derived_from: None,
        derived_at_seq: seq(0),
    }
}

/// **块定位**（池与恢复共用）：file 0 = 字典，file 1 = 撤销段。
fn resolve(file0: FileHandle, undo: FileHandle, r: Rdba) -> Option<(FileHandle, u32)> {
    match r.file_id() {
        FILE0_ID => Some((file0, r.block_id())),
        UNDO_ID => Some((undo, r.block_id())),
        _ => None,
    }
}

/// **一个已打开的实例**（CLI 的常驻件）。
pub struct Instance {
    /// 实例目录。
    pub dir: PathBuf,
    /// 文件 I/O（关闭路径要再开一次撤销文件推进 `file_scn`）。
    pub io: &'static OsFileIo,
    /// 撤销文件路径。
    pub undo_path: PathBuf,
    /// 缓冲池。
    pub pool: &'static BufferPool<'static>,
    /// 事务引擎。
    pub engine: &'static Engine<'static, 'static, 'static, 'static>,
    /// 工作区字典（file 0；已接池 = 活系统读法）。
    pub catalog: Catalog<'static>,
    /// 打开期的恢复回执（诊断；`None` = 建区当次）。
    pub recovery: Option<RecoverySummary>,
    /// **实例锁**（单写者纪律；Drop 即释放）。服务模式由守护进程持有，
    /// 直连模式由本进程持有——同一时刻只允许一个写者。
    /// 读它的地方：`Instance::lock_holder`（诊断）与服务退出前的显式释放。
    lock: Option<InstanceLock>,
}

/// 打开期的一份恢复回执（CLI 启动横幅用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoverySummary {
    /// 恢复起点（控制文件检查点 LSN）。
    pub start_lsn: u64,
    /// 重放的块数。
    pub applied_blocks: u64,
    /// 输家回滚的事务数。
    pub txns_rolled_back: usize,
    /// 日志流里的最大提交序号（恢复后的当前提交序号水位）。
    pub highest_commit_seq: u64,
    /// 恢复后的续写位置。
    pub log_end: u64,
}

impl Instance {
    /// **关闭**：完全检查点（脏页写回 + 发布低水位）——干净退出。
    ///
    /// 崩溃不走这里：此时 WAL 是唯一耐久源，`open` 的重做阶段负责重建。
    pub fn shutdown(&mut self) -> Result<(), BootError> {
        let report = self.engine.checkpoint_full(WS)?;
        // **推进 `file_scn`**（§2.4）：干净关闭后，两个文件的内容确实达到了
        // 检查点位点——头字段是"打开链核对"（`check_files`）的事实来源，
        // 长期不推进的话那道核对永远不可能发现"文件超前"。
        let lsn = report.progress.checkpoint_lsn.as_raw();
        self.catalog.file_mut().set_file_scn(lsn)?;
        self.catalog.file_mut().sync()?;
        let mut undo = DataFile::open(self.io, &self.undo_path)?;
        undo.set_file_scn(lsn)?;
        undo.sync()?;
        undo.close()?;
        Ok(())
    }

    /// 本实例的锁事实（谁占着、什么模式；诊断/`status` 用）。
    #[must_use]
    pub fn lock_holder(&self) -> Option<&crate::lock::LockInfo> {
        self.lock.as_ref().map(InstanceLock::info)
    }

    /// 当前提交序号（会话的快照水位起点）。
    #[must_use]
    pub fn seq(&self) -> u64 {
        self.catalog.current_seq().max(self.engine.current_seq())
    }
}

/// **建区**：字典 + 撤销段 + 控制文件 + 日志组 + 池/引擎（§5.1 的 ①–⑤）。
pub fn create_instance(dir: &Path) -> Result<Instance, BootError> {
    std::fs::create_dir_all(dir)?;
    if p(dir, FILE0).exists() {
        return Err(BootError::Catalog(format!(
            "{} 已存在（不是空目录）",
            p(dir, FILE0).display()
        )));
    }
    let io: &'static OsFileIo = Box::leak(Box::new(OsFileIo::new()));
    let io_dyn: &'static dyn FileIo = io;
    // 建区也持锁（Direct）：建到一半被别人开起来同样是撕字典。
    let lock = InstanceLock::acquire(dir, LockMode::Direct, &lock::socket_path(dir))?;

    // ① file 0：自举集 + 种子（**建区期直写**，不经池——见 `catalog::create`）。
    let layout = FileLayout::meta();
    let file0_path = p(dir, FILE0);
    let mut file0 = DataFile::create(
        io_dyn,
        &file0_path,
        FILE0_ID,
        META_ROLE,
        WS,
        layout.min_file_blocks() + FILE0_BLOCKS,
    )?;
    let built =
        create_dictionary(&mut file0, WS, false).map_err(|e| BootError::Catalog(e.to_string()))?;
    let mut catalog = Catalog::from_entries(file0, built.entries.clone())
        .map_err(|e| BootError::Catalog(e.to_string()))?;
    catalog
        .seed_own_dictionary(&built)
        .map_err(|e| BootError::Catalog(e.to_string()))?;
    drop(catalog);
    // 建区期的直写必须**先落盘再进 redo**（物理增量无法重建不存在的页）。
    DataFile::open(io_dyn, &file0_path)?.sync()?;

    // ② 撤销段（V1.0 单段）。
    let undo_path = p(dir, UNDO);
    let undo_file: &'static mut DataFile<'static> = Box::leak(Box::new(DataFile::create(
        io_dyn, &undo_path, UNDO_ID, 1, WS, 512,
    )?));
    let undo_handle = undo_file.handle();
    let undo_seg = create_undo_segment(undo_file, 2, 3, 4)?;
    debug_assert_eq!(undo_seg.page0_block(), undo_page0());

    // ③ 控制文件双副本 + 日志组。
    let cf: &'static mut ControlFile<'static> = Box::leak(Box::new(ControlFile::format(
        io_dyn,
        &p(dir, CF_A),
        &p(dir, CF_B),
        &workspace_entry(),
        &RedoEntries::new(2, 1).expect("常量"),
        &ArchiveRecord::new(ArchiveMode::NoArchive),
    )?));
    let wal_path = p(dir, WAL_DIR);
    std::fs::create_dir_all(&wal_path)?;
    let writer = GroupWriter::create(io_dyn, cf, &wal_path, group_spec(), lsn(0))?;

    // ④ 池（WAL 守卫 = 日志的刷盘核心）+ 引擎 + 目录。
    let file0_handle = DataFile::open(io_dyn, &file0_path)?.handle();
    let guard = writer.shared();
    let pool: &'static BufferPool<'static> = Box::leak(Box::new(BufferPool::with_config(
        io_dyn,
        POOL_FRAMES,
        move |_ws, r| resolve(file0_handle, undo_handle, r),
        guard,
        SystemClock,
        CacheConfig::for_capacity(POOL_FRAMES),
    )?));
    let engine: &'static Engine<'static, 'static, 'static, 'static> =
        Box::leak(Box::new(Engine::new(
            pool,
            writer,
            UndoChain::open(undo_seg).with_pool(pool),
            seq(0),
        )));
    let mut catalog =
        Catalog::open(io_dyn, &file0_path).map_err(|e| BootError::Catalog(e.to_string()))?;
    catalog.attach_pool(pool);

    // ⑤ 建区收尾：`stat$`/`seq$` + `object_id` 序列（§5.1 ⑤）。
    ddl::init_dictionary_tables(&mut catalog, engine)
        .map_err(|e| BootError::Catalog(e.to_string()))?;
    catalog.set_current_seq(seq(engine.current_seq()));

    Ok(Instance {
        dir: dir.to_path_buf(),
        io,
        undo_path: undo_path.clone(),
        pool,
        engine,
        catalog,
        recovery: None,
        lock: Some(lock),
    })
}

/// **打开既有实例（直连模式）**：先取实例锁（单写者），再打开。
///
/// 服务在跑（或别的进程直连着）⇒ [`BootError::Occupied`]——**不**静默并存：
/// 两个写者各写各的池与日志不是并发，是互相破坏。
pub fn open_instance(dir: &Path) -> Result<Instance, BootError> {
    let lock = InstanceLock::acquire(dir, LockMode::Direct, &lock::socket_path(dir))?;
    open_unlocked(dir, Some(lock))
}

/// **打开既有实例（锁已由调用方持有）**：守护进程走这条（它拿的是 Service 锁）。
pub fn open_unlocked(dir: &Path, lock: Option<InstanceLock>) -> Result<Instance, BootError> {
    let io: &'static OsFileIo = Box::leak(Box::new(OsFileIo::new()));
    let io_dyn: &'static dyn FileIo = io;
    let file0_path = p(dir, FILE0);
    if !file0_path.exists() {
        return Err(BootError::Catalog(format!(
            "{} 不存在（先 `bicdb init`）",
            file0_path.display()
        )));
    }
    let undo_path = p(dir, UNDO);
    let wal_path = p(dir, WAL_DIR);
    let (cf_path_a, cf_path_b) = (p(dir, CF_A), p(dir, CF_B));

    let file0_handle = DataFile::open(io_dyn, &file0_path)?.handle();
    let undo_file: &'static mut DataFile<'static> =
        Box::leak(Box::new(DataFile::open(io_dyn, &undo_path)?));
    let undo_handle = undo_file.handle();
    let undo_seg = Segment::open(undo_file, undo_page0())?;

    // 起点 = 控制文件的检查点 LSN（低水位）。
    let spec = group_spec();
    let progress = {
        let cf_ro = ControlFile::open(io_dyn, &cf_path_a, &cf_path_b)?;
        cf_ro.checkpoint_progress()?
    };

    // **打开链的事实核对**（`目录详设` §2.5）：各文件头位点 vs 控制文件检查点。
    // 超前（文件比控制文件新：拷错/配错控制文件）⇒ **拒绝打开**；本函数此前
    // 直接进恢复，`catalog::consistency` 整模块只有单测消费者（2026-10-06 审计）。
    let file0_scn = DataFile::open(io_dyn, &file0_path)?.file_scn();
    let undo_scn = DataFile::open(io_dyn, &undo_path)?.file_scn();

    // 写口（续写位置在组集内部重建）+ 只读组视图（恢复的扫描面）。
    let cf: &'static mut ControlFile<'static> =
        Box::leak(Box::new(ControlFile::open(io_dyn, &cf_path_a, &cf_path_b)?));
    let mut writer = GroupWriter::open(io_dyn, cf, &wal_path, spec)?;

    // **恢复**（顺序不可换：分析 → 重做 → 输家回滚；见 `wal::recovery`）。
    let chain = UndoChain::open(undo_seg);
    let summary = {
        let cf_ro = ControlFile::open(io_dyn, &cf_path_a, &cf_path_b)?;
        let groups = online_groups(io_dyn, &cf_ro, &wal_path, spec)?;
        // 核对：文件位点 vs 检查点（`chain_start` = 在线组的最小起点；无日志 ⇒ None）。
        let chain_start = groups.iter().map(|g| g.start_lsn).min();
        let points = vec![
            bicdb_catalog::FilePoint::Readable {
                file_id: FILE0_ID,
                role: META_ROLE,
                file_scn: file0_scn,
            },
            bicdb_catalog::FilePoint::Readable {
                file_id: UNDO_ID,
                role: 1,
                file_scn: undo_scn,
            },
        ];
        let report = bicdb_catalog::check_files(progress.checkpoint_lsn, chain_start, &points);
        if report.refused {
            return Err(BootError::Catalog(format!(
                "拒绝打开：文件超前于控制文件（{}）——像是配错了控制文件/拷错文件",
                report
                    .findings
                    .iter()
                    .map(|f| f.to_string())
                    .collect::<Vec<_>>()
                    .join("；")
            )));
        }
        if !report.is_clean() {
            eprintln!("（一致性核对有发现：{report:?}）");
        }
        let mut resolve = |r: Rdba| resolve(file0_handle, undo_handle, r);
        let report = recover(
            io_dyn,
            &groups,
            progress.checkpoint_lsn,
            &chain,
            &mut writer,
            &mut resolve,
        )?;
        RecoverySummary {
            start_lsn: report.start_lsn.as_raw(),
            applied_blocks: report.redo.applied_blocks as u64,
            txns_rolled_back: report.undo.txns_rolled_back,
            highest_commit_seq: report.analysis.highest_commit_seq,
            log_end: report.log_end.as_raw(),
        }
    };

    // 当前提交序号 = 控制文件与日志流的**较大者**（恢复后不得回退）。
    let recovered_seq = progress
        .current_commit_seq
        .as_raw()
        .max(summary.highest_commit_seq);
    let guard = writer.shared();
    let pool: &'static BufferPool<'static> = Box::leak(Box::new(BufferPool::with_config(
        io_dyn,
        POOL_FRAMES,
        move |_ws, r| resolve(file0_handle, undo_handle, r),
        guard,
        SystemClock,
        CacheConfig::for_capacity(POOL_FRAMES),
    )?));
    let engine: &'static Engine<'static, 'static, 'static, 'static> = Box::leak(Box::new(
        Engine::new(pool, writer, chain.with_pool(pool), seq(recovered_seq)),
    ));
    let mut catalog =
        Catalog::open(io_dyn, &file0_path).map_err(|e| BootError::Catalog(e.to_string()))?;
    catalog.attach_pool(pool);
    catalog.set_current_seq(seq(recovered_seq));

    Ok(Instance {
        dir: dir.to_path_buf(),
        io,
        undo_path: undo_path.clone(),
        pool,
        engine,
        catalog,
        recovery: Some(summary),
        lock,
    })
}
