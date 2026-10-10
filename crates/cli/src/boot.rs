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

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use bicdb_catalog::{create_dictionary, ddl, Catalog};
use bicdb_common::seq::{CommitSeq, Lsn};
use bicdb_storage::bitmap::{ExtentNo, FileLayout, META_ROLE};
use bicdb_storage::buffer::{BufferPool, SystemClock};
use bicdb_storage::controlfile::{
    ArchiveMode, ArchiveRecord, ControlFile, DataFileRecord, RedoEntries, WorkspaceEntry,
};
use bicdb_storage::datafile::DataFile;
use bicdb_storage::globalctl::{
    generate_library_id, GlobalControlFile, LibraryEntry, WorkspaceRecord,
};
use bicdb_storage::rowid::Rdba;
use bicdb_storage::segment::Segment;
use bicdb_storage::undo::{create_undo_segment, UndoChain};
use bicdb_txn::engine::Engine;
use bicdb_wal::group::{online_groups, GroupSpec, GroupWriter};
use bicdb_wal::recovery::recover;
use bicdb_workspace::io::{FileHandle, FileIo, OsFileIo};

use crate::config::{self, InstanceParams};
use crate::lock::{InstanceLock, LockError, LockMode};
use bicdb_workspace::WorkspaceId;

/// 工作区标识的**文本形态**（数据文件名前缀）：`workspace_ref` 的十六进制。
///
/// `workspace_ref = SHA-256(workspace_id)` 前 8 字节（`crates/workspace/src/id.rs`；
/// `doc/workspace_ref设计_v0.1.md`），文件头里也带着它——**数据文件名与文件头同源**。
#[must_use]
pub fn ws_name(ws_ref: [u8; 8]) -> String {
    ws_ref.iter().map(|b| format!("{b:02x}")).collect()
}

/// **实例的"身份文件"**（file 0）：`data/*_meta`——判"这是不是 bicdb 实例"用它。
///
/// 扫描而不是拼名：文件名前缀是 `workspace_ref`，而打开时才知道工作区号
/// （控制文件在固定路径上，先读它拿到号，再算名字）。
#[must_use]
pub fn find_meta_file(dir: &Path) -> Option<PathBuf> {
    let data = dir.join(DATA_DIR);
    let entries = std::fs::read_dir(&data).ok()?;
    let mut hits: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with("_meta"))
        })
        .collect();
    hits.sort();
    hits.into_iter().next()
}

/// file 0 的物理文件号（池的定位表按它分派）。
const FILE0_ID: u16 = 0;
/// 撤销段的物理文件号。
const UNDO_ID: u16 = 1;

/// 字典文件名（也是"这是个 bicdb 实例"的判据）。
/// **文件面**（`docs/storage/02-工作区存储布局.md` §2.1 的布局，V1.0 单工作区）：
///
/// ```text
/// <db_root>/                      根区目录（参数文件注册它）
/// ├── bicdb.ini                   实例参数文件
/// ├── control/                    控制文件双副本（control01.ctl / control02.ctl）
/// ├── wal/                        日志组（redo_g<组>_m<成员>）
/// └── data/                       数据文件：<ws>_meta（file 0）/ <ws>_undo（file 1）
/// ```
///
/// **为什么按 `<ws>_` 前缀**：设计里数据文件按工作区命名（`<ws>_meta`/`<ws>_undo`/
/// `<ws>_data_NN`）——单工作区 CLI 也照这个名字，多工作区/数据文件扩展时命名规则不变。
pub const CONTROL_DIR: &str = "control";
/// 数据文件目录。
pub const DATA_DIR: &str = "data";
/// 控制文件副本名（设计与 `ControlFile::format` 的取法：01/02）。
pub const CF_A: &str = "control/control01.ctl";
/// 控制文件副本名（第二份）。
pub const CF_B: &str = "control/control02.ctl";

/// 实例的文件面：数据文件名（`<workspace_ref 十六进制>_<角色>`）。
#[must_use]
pub fn data_file_name(ws_ref: [u8; 8], kind: &str) -> String {
    format!("{}_{kind}", ws_name(ws_ref))
}
/// 日志组目录名。
pub const WAL_DIR: &str = "wal";

/// 建区时 file 0 初始块数的**默认值**（参数文件 `[init] file0_initial_blocks` 可改）。
pub const DEFAULT_FILE0_BLOCKS: u64 = 4096;
/// 撤销文件初始块数的**默认值**（参数文件 `[init] undo_initial_blocks` 可改）。
pub const DEFAULT_UNDO_BLOCKS: u64 = 512;
/// 临时文件初始块数（file 2；no-cache/no-redo，启动与干净停机都重置）。
pub const DEFAULT_TEMP_BLOCKS: u64 = 512;
/// 日志每组成员页数的**默认值**（参数文件 `[init] wal_group_pages` 可改）。
pub const DEFAULT_WAL_GROUP_PAGES: u32 = 8192;

/// **自动扩展增量的默认值**（块；与 `storage::segment` 同源）。
pub const DEFAULT_FILE_EXTEND_BLOCKS: u64 = bicdb_storage::segment::DEFAULT_FILE_EXTEND_BLOCKS;

/// 建区/打开错误。
#[derive(Debug)]
pub enum BootError {
    /// I/O（含各层的 I/O 类错误）。
    Io(std::io::Error),
    /// 目录（字典）层。
    Catalog(String),
    /// **实例被别的进程占着**（单写者纪律；见 `lock.rs`）。
    Occupied(String),
    /// **参数文件**（未知键/取值非法；见 `config.rs`）。
    Config(String),
}

impl std::fmt::Display for BootError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BootError::Io(e) => write!(f, "I/O：{e}"),
            BootError::Catalog(w) => write!(f, "目录：{w}"),
            BootError::Occupied(w) => f.write_str(w),
            BootError::Config(w) => write!(f, "参数：{w}"),
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

/// 撤销文件（file 1）的**文件角色**（`§2.2` 的角色表：0 元数据 / 1 Undo / 2 临时）。
pub const UNDO_FILE_ROLE: u8 = 1;

/// **登记工作区的数据文件到控制文件**（`file$` 的内容；`arch/03` §3.1.4）。
///
/// **为什么在装配层写**：文件清单的权威是**控制文件**（`目录详设` 纪律 7），
/// 而控制文件是**单写者**（组切换/检查点/采样都归 `GroupWriter`）——这一处落在
/// `GroupWriter` 接手**之前**的装配期，走的是同一套双副本区间更新协议。
///
/// **幂等**：同一文件号只覆盖同一条记录；`creation_blocks` 取**当时的大小**
/// （建区时 = 建区参数；补登记时 = 当前大小——创建时的值已不可考，如实记当前）。
fn publish_data_files(
    io: &dyn bicdb_workspace::io::FileIo,
    cf: &mut ControlFile<'_>,
    files: &[(u16, u8, &PathBuf)],
) -> Result<(), BootError> {
    let now = now_ms();
    let mut wrote = false;
    for (file_id, role, path) in files {
        // 大小取**文件自述**（`blocks()` 读文件头；不是 `stat` 长度——两者含义不同）。
        let file = DataFile::open(io, path)?;
        let blocks = file.blocks();
        file.close()?;
        let path_bytes = std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str()).to_vec();
        // **已登记且对得上就跑**（幂等的意义就在这里：常态打开**不写**控制文件）。
        if let Ok(existing) = cf.data_file_record(usize::from(*file_id)) {
            if existing.status != 0 && existing.role == *role && existing.path() == path_bytes {
                continue;
            }
        }
        let mut rec = DataFileRecord::new(*file_id, *role);
        rec.status = 1;
        rec.creation_blocks = blocks;
        rec.created_at = now;
        rec.set_path(&path_bytes)
            .map_err(|e| BootError::Catalog(format!("数据文件路径登记：{e}")))?;
        cf.write_data_file_record(usize::from(*file_id), &rec)
            .map_err(|e| BootError::Catalog(format!("登记数据文件记录：{e}")))?;
        wrote = true;
    }
    if wrote {
        cf.sync()
            .map_err(|e| BootError::Catalog(format!("控制文件落盘：{e}")))?;
    }
    Ok(())
}

/// 数据文件路径（`<db_root>/data/<ws>_<kind>`）。
pub fn data_path(dir: &Path, ws: [u8; 8], kind: &str) -> PathBuf {
    dir.join(DATA_DIR).join(format!("{}_{kind}", ws_name(ws)))
}

/// 实例的"身份文件"：`data/<ws>_meta`（file 0）——判"这是不是 bicdb 实例"用它。
#[must_use]
pub fn data_meta_path(dir: &Path, ws: [u8; 8]) -> PathBuf {
    data_path(dir, ws, "meta")
}

fn seq(v: u64) -> CommitSeq {
    CommitSeq::from_raw(v).expect("48 位域内")
}

fn lsn(v: u64) -> Lsn {
    Lsn::from_raw(v).expect("48 位域内")
}

/// 两个目录是否同一处（先规范化；规范化失败就按原样比）。
fn same_dir(a: &Path, b: &Path) -> bool {
    let ca = std::fs::canonicalize(a).unwrap_or_else(|_| a.to_path_buf());
    let cb = std::fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf());
    ca == cb
}

/// 数据文件按 `workspace_ref` 命名——**旧实例（`bicdb001` 前缀）不兼容**：
/// 报错要说清楚"为什么、怎么办"，不是干巴巴的"文件不存在"。
fn missing_meta_message(dir: &Path, expected: &Path) -> String {
    let legacy = find_meta_file(dir);
    match legacy {
        Some(p) => format!(
            "{} 不存在（本版按 `workspace_ref` 命名，看到的是 {}）\
             ——旧命名的实例（`bicdb001` 前缀）不兼容本版：重建实例，或把该目录作为新实例重新 `bicdb init`",
            expected.display(),
            p.display()
        ),
        None => format!("{} 不存在（先 `bicdb init`）", expected.display()),
    }
}

/// **取/分配本实例的下一个工作区标识**（注册表分配；不在 home 下 ⇒ `None`）。
fn open_global_ctl_cached(
    home: Option<&crate::home::Home>,
    dir: &Path,
) -> Result<Option<WorkspaceId>, BootError> {
    let Some(home) = home else {
        return Ok(None);
    };
    let root = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let home_root = std::fs::canonicalize(&home.root).unwrap_or_else(|_| home.root.clone());
    if !root.starts_with(&home_root) {
        return Ok(None);
    }
    let gcf = open_global_ctl(home)?;
    let id = gcf
        .allocate_workspace_id()
        .map_err(|e| BootError::Catalog(format!("分配工作区标识失败：{e}")))?;
    gcf.close()
        .map_err(|e| BootError::Catalog(format!("关闭全局控制文件失败：{e}")))?;
    Ok(Some(id))
}

/// 撤销段头块（V1.0 单段 ⇒ 标准布局首区）。
fn undo_page0() -> u32 {
    FileLayout::standard().first_block_of(ExtentNo::from_raw(0).expect("域内"))
}

/// **日志组规格**（来自参数文件 `[init]`；与控制文件条目一致）。
#[must_use]
pub fn group_spec(init: &crate::config::InitParams) -> GroupSpec {
    GroupSpec::new(init.wal_groups, init.wal_members, init.wal_group_pages)
        .expect("参数文件已做闭集校验")
}

fn workspace_entry(id: WorkspaceId) -> WorkspaceEntry {
    WorkspaceEntry {
        workspace_id: id,
        created_at: now_ms(),
        derived_from: None,
        derived_at_seq: seq(0),
    }
}

/// 墙钟毫秒（注册表的时间戳；`0` = 取不到——不是致命错误）。
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// **打开（或首次建立）实例的全局控制文件**（`<home>/control/`，双副本）。
///
/// 第一次在这个 home 下建工作区时**现场建立**（随机 `library_id`）；
/// 之后每次打开都走副本校验/自愈（`CfCore::open` 的既有规则）。
///
/// # Errors
/// 目录建不了、两份副本都不可用、登记内容非法。
pub fn open_global_ctl(home: &crate::home::Home) -> Result<GlobalControlFile<'static>, BootError> {
    let io: &'static OsFileIo = Box::leak(Box::new(OsFileIo::new()));
    open_global_ctl_on(io, home)
}

/// 同上，但由调用方给 I/O（测试用内存 I/O）。
pub fn open_global_ctl_on<'a>(
    io: &'a dyn FileIo,
    home: &crate::home::Home,
) -> Result<GlobalControlFile<'a>, BootError> {
    let dir = home.control_dir();
    let [a, b] = home.global_ctl_paths();
    if !a.exists() && !b.exists() {
        std::fs::create_dir_all(&dir)?;
        let library = LibraryEntry::new(generate_library_id(), now_ms());
        let gcf = GlobalControlFile::format(io, &a, &b, &library)
            .map_err(|e| BootError::Catalog(format!("建全局控制文件失败：{e}")))?;
        gcf.sync()
            .map_err(|e| BootError::Catalog(format!("全局控制文件落盘失败：{e}")))?;
        return Ok(gcf);
    }
    GlobalControlFile::open(io, &a, &b)
        .map_err(|e| BootError::Catalog(format!("打开全局控制文件失败：{e}")))
}

/// **把一个工作区登记进实例注册表**（三步协议的 **③ 最后一步**）。
///
/// 顺序纪律见 `doc/全局控制文件设计_v0.1.md` §2：**文件先、控制文件最后**——
/// 这一步一做，"工作区存在"才对外成立。
///
/// `home` 为 `None` 或工作区**不在 home 下** ⇒ 不登记（返回 `Ok(None)`）：
/// 路径式工作区不受 home 约束，`bicdb list` 看不到它（记档于安装布局 §5）。
pub fn register_workspace(
    home: Option<&crate::home::Home>,
    dir: &Path,
    id: WorkspaceId,
) -> Result<Option<u16>, BootError> {
    let Some(home) = home else {
        return Ok(None);
    };
    let root = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let home_root = std::fs::canonicalize(&home.root).unwrap_or_else(|_| home.root.clone());
    if !root.starts_with(&home_root) {
        return Ok(None);
    }
    let mut gcf = open_global_ctl(home)?;
    let root_bytes = root.display().to_string().into_bytes();
    let rec = WorkspaceRecord::new(id, root_bytes, now_ms())
        .map_err(|e| BootError::Catalog(format!("工作区登记内容非法：{e}")))?;
    let slot = gcf
        .insert_workspace(&rec)
        .map_err(|e| BootError::Catalog(format!("工作区登记失败：{e}")))?;
    gcf.sync()
        .map_err(|e| BootError::Catalog(format!("全局控制文件落盘失败：{e}")))?;
    gcf.close()
        .map_err(|e| BootError::Catalog(format!("关闭全局控制文件失败：{e}")))?;
    Ok(Some(slot as u16))
}

/// **进程级参数落点**（这些层没有实例上下文，故在实例打开时设一次——
/// 照 `storage::segment::set_file_extend_blocks` 的范式）。
///
/// 失败一律**报错**：参数文件已经被 `config` 校验过范围，这里再失败说明
/// 两边的界不一致——那也不该静默（收下不生效比报错更糟）。
fn apply_process_params(params: &InstanceParams) -> Result<(), BootError> {
    let r = &params.run;
    let bad = |what: &str, why: &str| BootError::Config(format!("{what}：{why}"));
    bicdb_storage::segment::set_file_extend_blocks(r.file_extend_blocks)
        .map_err(|e| bad("storage.file_extend_blocks", &e.to_string()))?;
    bicdb_storage::scan::set_scan_run_pages(r.multiblock_read_pages)
        .map_err(|e| bad("storage.multiblock_read_pages", e))?;
    bicdb_storage::cr::set_cr_max_rounds(r.cr_max_rounds)
        .map_err(|e| bad("storage.cr_max_rounds", e))?;
    bicdb_wal::buffer::set_log_buffer_pages(r.log_buffer_pages)
        .map_err(|e| bad("wal.log_buffer_pages", e))?;
    bicdb_wal::buffer::set_flush_trigger_permille(r.flush_trigger_permille)
        .map_err(|e| bad("wal.flush_trigger_permille", e))?;
    bicdb_catalog::open::set_forward_max_hops(r.rid_forward_max_hops)
        .map_err(|e| bad("catalog.rid_forward_max_hops", e))?;
    bicdb_index::set_bulk_fill_percent(r.bulk_fill_percent)
        .map_err(|e| bad("index.bulk_fill_percent", e))?;
    // 字典行缓存上限：先看旧值（`Catalog::open` 会读它），设完把新值装上。
    let _ = bicdb_catalog::cache::set_cache_caps(bicdb_catalog::cache::CacheCaps {
        max_rows: r.row_cache_rows,
        max_bytes: usize::try_from(r.row_cache_bytes).unwrap_or(usize::MAX),
    });
    Ok(())
}

/// 缓存策略（参数文件 → [`CacheConfig`]）。
fn cache_partition_capacity(params: &InstanceParams) -> Result<usize, BootError> {
    let partitions = params.run.kcbwds;
    if partitions == 0
        || !partitions.is_power_of_two()
        || partitions > 64
        || params.run.pool_frames % partitions != 0
    {
        return Err(BootError::Catalog(
            "buffer.kcbwds 必须为 1–64 的 2 的幂，且整除总 pool_frames".into(),
        ));
    }
    Ok(params.run.pool_frames / partitions)
}

fn cache_config(params: &InstanceParams) -> bicdb_storage::buffer::CacheConfig {
    let r = &params.run;
    bicdb_storage::buffer::CacheConfig::for_capacity_tuned(
        r.pool_frames / r.kcbwds.max(1),
        bicdb_storage::buffer::CacheTuning {
            hash_buckets: r.hash_buckets,
            bucket_latches: r.bucket_latches,
            hot_fraction: r.hot_fraction,
            touch_interval_ms: r.touch_interval_ms,
            cool_count: r.cool_count,
            stay_count: r.stay_count,
            hot_criteria: r.hot_criteria,
            max_scan_fraction: r.max_scan_fraction,
            make_free_batch_divisor: r.make_free_batch_divisor,
        },
    )
}

/// 等锁策略（参数文件 → 引擎）。
fn wait_policy(params: &InstanceParams) -> bicdb_txn::write::WaitPolicy {
    bicdb_txn::write::WaitPolicy {
        park_timeout: std::time::Duration::from_millis(params.run.park_ms),
        deadlock_threshold_ms: params.run.deadlock_threshold_ms,
        // 0 = 不限（`lock.wait_max_rounds`；Oracle 的默认也是无限等）。
        max_waits: u32::try_from(params.run.wait_max_rounds)
            .ok()
            .filter(|n| *n > 0),
    }
}

/// 实例参数（`bicdb params` 与诊断用；按参数文件寻址，命令行覆盖可给）。
pub fn instance_params(
    ini: Option<&Path>,
    cli: &[(String, String)],
) -> Result<(InstanceParams, config::ParamTable), BootError> {
    InstanceParams::load_with_overrides(ini, cli).map_err(|e| BootError::Config(e.to_string()))
}

/// **核对建区期参数**（控制文件/日志文件是权威）：不符即拒绝打开。
fn check_creation_facts(
    _dir: &Path,
    params: &InstanceParams,
    entries: &bicdb_storage::controlfile::RedoEntries,
) -> Result<(), BootError> {
    // **只核对控制文件里真有的事实**（组数/成员数）。`wal_group_pages` 不核对：
    // 日志成员文件是**懒增长**的（用到哪写到哪），文件长度不是组容量——
    // 拿它推断会误报（实测：新库 g1 只写了 31 页、g2 还是建时的满长度）。
    let actual = crate::config::ActualCreation {
        wal_groups: entries.group_count,
        wal_members: entries.member_count,
        wal_group_pages: None,
    };
    match params.check_creation(&actual) {
        None => Ok(()),
        Some(e) => Err(BootError::Config(e.to_string())),
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

/// Minimum cache capacity reserved for each activated workspace (2 GiB).
pub const MIN_WORKSPACE_CACHE_FRAMES: usize =
    2 * 1024 * 1024 * 1024 / bicdb_storage::page::PAGE_SIZE;

fn check_cache_admission(total_frames: usize, count: usize) -> Result<(), BootError> {
    let required = count
        .checked_mul(MIN_WORKSPACE_CACHE_FRAMES)
        .ok_or_else(|| BootError::Config("工作区缓存保障容量溢出".into()))?;
    if total_frames < required {
        return Err(BootError::Config(format!("共享 DB Cache 容量不足：{count} 个工作区至少需要 {} GiB，当前 {} 帧；每工作区最低 2 GiB", count * 2, total_frames)));
    }
    Ok(())
}

/// One process-wide cache and file/WAL registry for all logical workspaces.
/// Workspace-local engines retain independent undo and log streams.
pub struct SharedInstanceCache {
    /// All handles belong to the same I/O implementation.
    pub io: &'static OsFileIo,
    /// Single shared database buffer cache.
    pub pool: &'static BufferPool<'static>,
    files: Arc<RwLock<BTreeMap<[u8; 8], (FileHandle, FileHandle)>>>,
    pub(crate) wal: Arc<bicdb_storage::wal_router::WorkspaceWalRouter>,
}
impl SharedInstanceCache {
    fn new(io: &'static OsFileIo, params: &InstanceParams) -> Result<Arc<Self>, BootError> {
        check_cache_admission(params.run.pool_frames, 1)?;
        let files = Arc::new(RwLock::new(
            BTreeMap::<[u8; 8], (FileHandle, FileHandle)>::new(),
        ));
        let resolver = Arc::clone(&files);
        let wal = Arc::new(bicdb_storage::wal_router::WorkspaceWalRouter::default());
        let pool = Box::leak(Box::new(BufferPool::with_partitions(
            io,
            params.run.kcbwds,
            cache_partition_capacity(params)?,
            move |ws, r| {
                let map = resolver.read().ok()?;
                let (meta, undo) = map.get(ws)?;
                resolve(*meta, *undo, r)
            },
            Arc::clone(&wal),
            SystemClock,
            cache_config(params),
        )?));
        Ok(Arc::new(Self {
            io,
            pool,
            files,
            wal,
        }))
    }
    fn register(
        &self,
        ws: [u8; 8],
        meta: FileHandle,
        undo: FileHandle,
        guard: Arc<bicdb_wal::group::WalShared<'static>>,
    ) -> Result<(), BootError> {
        // Admission is serialized by the service control loop. Never hold the
        // file registry while acquiring cache structures: resolution uses the
        // reverse order during page selection.
        let count = {
            let files = self
                .files
                .read()
                .map_err(|_| BootError::Catalog("文件注册表锁损坏".into()))?;
            if files.contains_key(&ws) {
                return Err(BootError::Catalog("工作区已注册到共享缓存".into()));
            }
            files.len() + 1
        };
        check_cache_admission(self.pool.capacity() * self.pool.partition_count(), count)?;
        self.pool
            .reserve_workspace(ws, MIN_WORKSPACE_CACHE_FRAMES)?;
        if let Err(error) = self.wal.register(ws, guard) {
            let _ = self.pool.release_empty_workspace_reservation(ws);
            return Err(BootError::Io(error));
        }
        let inserted = match self.files.write() {
            Ok(mut files) => {
                if files.contains_key(&ws) {
                    Err(BootError::Catalog("工作区已注册到共享缓存".into()))
                } else {
                    files.insert(ws, (meta, undo));
                    Ok(())
                }
            }
            Err(_) => Err(BootError::Catalog("文件注册表锁损坏".into())),
        };
        if let Err(error) = inserted {
            let _ = self.wal.unregister_registration(ws);
            let _ = self.pool.release_empty_workspace_reservation(ws);
            return Err(error);
        }
        Ok(())
    }
}

/// **一个已打开的实例**（CLI 的常驻件）。
pub struct Instance {
    /// Service-wide shared cache; absent only in standalone direct mode.
    pub shared_cache: Option<Arc<SharedInstanceCache>>,
    /// **本实例的有效参数**（参数文件 + 命令行覆盖之后的；诊断与"要参数的功能"用它）。
    pub params: InstanceParams,
    /// 工作区标识（`workspace_ref = SHA-256(workspace_id)` 前 8 字节）——
    /// 检查点、诊断与文件头核对都用它（**不再有"全局常量"**：每个工作区一份）。
    pub ws_ref: [u8; 8],
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
    /// Identified consistency checks bypassed by an explicit private-workspace
    /// FORCE open. `None` means the workspace passed the normal recovery gate.
    pub forced_recovery: Option<String>,
    // Declared before the lifecycle lock: implicit Drop drains the audit writer
    // before releasing ownership to another process. Shared daemon pools use
    // their single instance-wide writer instead.
    fault_audit: Option<bicdb_storage::fault_audit::FaultAuditWriter>,
    /// **实例锁**（单写者纪律；Drop 即释放）。服务模式由守护进程持有，
    /// 直连模式由本进程持有——同一时刻只允许一个写者。
    /// 读它的地方：`Instance::lock_holder`（诊断）与服务退出前的显式释放。
    lock: Option<InstanceLock>,
    /// **直连形态的显式事务**（跨语句保管；`None` = 没有活动事务）。
    ///
    /// 服务形态不用它：那里会话常驻一条连接，事务住在会话里。直连形态
    /// （`bicdbcli --direct`）每个语句新建一个会话，事务只能由连接保管。
    pub txn: Option<bicdb_txn::engine::TxnHandle>,
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
    /// Open an independent workspace-local catalog handle for a service worker.
    ///
    /// Catalog row caches and mutable lookup state are session-local; the
    /// buffer pool and transaction engine remain shared and provide page/WAL/
    /// lock concurrency. Workers must open a fresh handle rather than sharing
    /// `Instance::catalog` behind one global mutex, otherwise one long query
    /// serializes the whole database service.
    pub fn open_worker_catalog(&self) -> Result<Catalog<'static>, BootError> {
        let path = data_path(&self.dir, self.ws_ref, "meta");
        let mut catalog =
            Catalog::open(self.io, &path).map_err(|e| BootError::Catalog(e.to_string()))?;
        catalog.attach_pool(self.pool);
        catalog.set_current_seq(seq(self.seq()));
        Ok(catalog)
    }

    /// **关闭**：完全检查点（脏页写回 + 发布低水位）——干净退出。
    ///
    /// 崩溃不走这里：此时 WAL 是唯一耐久源，`open` 的重做阶段负责重建。
    pub fn shutdown(&mut self) -> Result<(), BootError> {
        let result = self.shutdown_data();
        if let Err(error) = &result {
            self.pool
                .quarantine_workspace(self.ws_ref, format!("停机失败：{error}"));
        }
        let audit = self
            .fault_audit
            .take()
            .map_or(Ok(()), |writer| writer.finish());
        match (result, audit) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) => Err(error),
            (Ok(()), Err(error)) => Err(BootError::Io(error)),
            (Err(error), Err(audit)) => Err(BootError::Io(std::io::Error::other(format!(
                "{error}；停机故障审计失败：{audit}"
            )))),
        }
    }

    fn start_direct_fault_audit(&mut self) -> Result<(), BootError> {
        let writer = bicdb_storage::fault_audit::FaultAuditWriter::start(self.io)?;
        let sink = writer.sink();
        sink.register(self.ws_ref, &self.dir.join("recovery.audit"))?;
        self.pool
            .set_fault_handler(Arc::new(move |workspace, reason| {
                sink.record(workspace, reason)
            }))?;
        self.fault_audit = Some(writer);
        Ok(())
    }

    fn shutdown_data(&mut self) -> Result<(), BootError> {
        // **收尾前先把没结束的事务回滚**：留着不管的话，检查点会把它的
        // 未提交改动原样留在文件里，下次打开要靠恢复回滚——能现在就干净，
        // 就不留给恢复（与"连接断开即回滚"同一口径）。
        if let Some(mut txn) = self.txn.take() {
            if let Err(error) = self.engine.rollback(&mut txn) {
                self.txn = Some(txn);
                self.pool
                    .quarantine_workspace(self.ws_ref, format!("停机回滚失败：{error}"));
                return Err(BootError::Io(std::io::Error::other(format!(
                    "停机回滚失败：{error}"
                ))));
            }
        }
        let report = self.engine.checkpoint_shutdown(self.ws_ref)?;
        let dirty = self.pool.dirty_len(self.ws_ref);
        if dirty != 0 {
            return Err(BootError::Io(std::io::Error::other(format!(
                "完全检查点后仍有 {dirty} 个脏块"
            ))));
        }
        // Engine checkpoint orchestration already published persistent file
        // header metadata after DBWR drain and CF publication.
        let _ = report;

        // Temp is an independent no-cache/no-redo file. It never participates
        // in the checkpoint; reset an existing file only after durable data
        // and redo have reached the clean-shutdown point.
        let temp_path = self
            .dir
            .join(DATA_DIR)
            .join(data_file_name(self.ws_ref, "temp"));
        if temp_path.exists() {
            let temp = DataFile::open_temp_reset(
                self.io,
                &temp_path,
                self.ws_ref,
                bicdb_storage::datafile::MIN_FILE_BLOCKS,
            )?;
            temp.sync()?;
            temp.close()?;
        }
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
///
/// `params` 由 `bicdb init <根区目录>` 装载（建区期参数在 `[init]`；见 `config`），
/// 建区**同时生成参数文件**到 `<db_root>/bicdb.ini`。
pub fn create_instance(
    params: &InstanceParams,
    home: Option<&crate::home::Home>,
) -> Result<Instance, BootError> {
    create_instance_with(params, home, &CreateOptions::default())
}

/// 建区选项（`CREATE WORKSPACE` 的供给方要用）。
#[derive(Debug, Clone, Copy)]
pub struct CreateOptions {
    /// **显式给工作区号**（`None` = 由注册表分配）。
    pub workspace_id: Option<WorkspaceId>,
    /// **要不要顺带登记进注册表**（`false` = 调用方自己按三步协议收尾）。
    pub register: bool,
}

impl Default for CreateOptions {
    fn default() -> Self {
        // `bicdb init` 的形态：自己分配号、自己登记。
        Self {
            workspace_id: None,
            register: true,
        }
    }
}

/// **建区（可指定工作区号与是否登记）**——`create_instance` 的完整形态。
///
/// `register = false` 是给 `CREATE WORKSPACE` 用的：那条路要"①文件 ②`ws$`
/// ③注册表"三步，注册表必须**最后**写（可见性只有一个开关）。
pub fn create_instance_with(
    params: &InstanceParams,
    home: Option<&crate::home::Home>,
    opts: &CreateOptions,
) -> Result<Instance, BootError> {
    let dir = params.db_root.clone();
    let dir = dir.as_path();
    std::fs::create_dir_all(dir)?;
    if let Some(meta) = find_meta_file(dir) {
        return Err(BootError::Catalog(format!(
            "{} 已存在（不是空目录）",
            meta.display()
        )));
    }
    // **工作区标识**：显式给的优先；否则由实例注册表分配（第一个 = 1）；
    // 不在 home 下则退回 1（单机路径式用法；多工作区形态永远走注册表）。
    let ws_id = match opts.workspace_id {
        Some(id) => id,
        None => match open_global_ctl_cached(home, dir) {
            Ok(Some(id)) => id,
            _ => WorkspaceId::from_raw(1).expect("域内"),
        },
    };
    let ws_ref = bicdb_workspace::workspace_ref(ws_id);
    // **PUBLIC 工作区 = `<home>/public`**：只有它带管理面字典表
    // （`user$`/`ws$`/`fs$`——`dict::is_public_only()` 的判据是引导条目 23 vs 15）。
    let is_public = home.is_some_and(|h| same_dir(dir, &h.public_dir()));
    let io: &'static OsFileIo = Box::leak(Box::new(OsFileIo::new()));
    let io_dyn: &'static dyn FileIo = io;
    // 建区也持锁（Direct）：建到一半被别人开起来同样是撕字典。
    let lock = InstanceLock::acquire(dir, LockMode::Direct, &params.socket_path())?;
    // **生成默认参数文件**（`bicdb init` 的产物之一；`db_root` 注册在里面）——
    // 参数是"跑起来的关键"，写出来才看得见、改得动。
    std::fs::write(params.ini_path(), params.render())?;
    apply_process_params(params)?;

    // ① file 0：自举集 + 种子（**建区期直写**，不经池——见 `catalog::create`）。
    let layout = FileLayout::meta();
    std::fs::create_dir_all(dir.join(DATA_DIR))?;
    std::fs::create_dir_all(dir.join(CONTROL_DIR))?;
    let file0_path = data_path(dir, ws_ref, "meta");
    let mut file0 = DataFile::create(
        io_dyn,
        &file0_path,
        FILE0_ID,
        META_ROLE,
        ws_ref,
        layout.min_file_blocks() + params.init.file0_initial_blocks,
    )?;
    let built = create_dictionary(&mut file0, ws_ref, is_public)
        .map_err(|e| BootError::Catalog(e.to_string()))?;
    let mut catalog = Catalog::from_entries(file0, built.entries.clone())
        .map_err(|e| BootError::Catalog(e.to_string()))?;
    catalog
        .seed_own_dictionary(&built)
        .map_err(|e| BootError::Catalog(e.to_string()))?;
    catalog
        .close()
        .map_err(|e| BootError::Catalog(e.to_string()))?;
    // 建区期的直写必须**先落盘再进 redo**（物理增量无法重建不存在的页）。
    let file0_sync = DataFile::open(io_dyn, &file0_path)?;
    file0_sync.sync()?;
    file0_sync.close()?;

    // ② 撤销段（V1.0 单段）。
    let undo_path = data_path(dir, ws_ref, "undo");
    let undo_file: &'static mut DataFile<'static> = Box::leak(Box::new(DataFile::create(
        io_dyn,
        &undo_path,
        UNDO_ID,
        1,
        ws_ref,
        params.init.undo_initial_blocks,
    )?));
    let undo_handle = undo_file.handle();
    let undo_seg = create_undo_segment(undo_file, 2, 3, 4)?;
    debug_assert_eq!(undo_seg.page0_block(), undo_page0());

    // file 2 is deliberately outside DB Cache and WAL. Formatting/resetting it
    // here establishes an empty temp tablespace for the first open.
    let temp_path = data_path(dir, ws_ref, "temp");
    let temp = DataFile::open_temp_reset(io_dyn, &temp_path, ws_ref, DEFAULT_TEMP_BLOCKS)?;
    temp.sync()?;
    temp.close()?;

    // ③ 控制文件双副本 + 日志组。
    let cf: &'static mut ControlFile<'static> = Box::leak(Box::new(ControlFile::format(
        io_dyn,
        &p(dir, CF_A),
        &p(dir, CF_B),
        &workspace_entry(ws_id),
        &RedoEntries::new(params.init.wal_groups, params.init.wal_members).expect("已校验"),
        &ArchiveRecord::new(ArchiveMode::NoArchive),
    )?));
    // ③.5 **登记数据文件到控制文件**（`file$` 的清单；`arch/03` §3.1.4）。
    // 在 `GroupWriter` 接手之前写——控制文件的单写者纪律随后归它。
    publish_data_files(
        io_dyn,
        cf,
        &[
            (FILE0_ID, META_ROLE, &file0_path),
            (UNDO_ID, UNDO_FILE_ROLE, &undo_path),
            (
                bicdb_storage::datafile::TEMP_FILE_ID,
                bicdb_storage::datafile::TEMP_FILE_ROLE,
                &temp_path,
            ),
        ],
    )?;
    let wal_path = p(dir, WAL_DIR);
    std::fs::create_dir_all(&wal_path)?;
    let writer = GroupWriter::create(io_dyn, cf, &wal_path, group_spec(&params.init), lsn(0))?;

    // ④ 池（WAL 守卫 = 日志的刷盘核心）+ 引擎 + 目录。
    let file0_handle = DataFile::open(io_dyn, &file0_path)?.handle();
    let guard = writer.shared();
    let pool: &'static BufferPool<'static> = Box::leak(Box::new(BufferPool::with_partitions(
        io_dyn,
        params.run.kcbwds,
        cache_partition_capacity(params)?,
        move |_ws, r| resolve(file0_handle, undo_handle, r),
        guard,
        SystemClock,
        cache_config(params),
    )?));
    for file_id in [FILE0_ID, UNDO_ID] {
        pool.register_checkpoint_file(bicdb_storage::buffer::BufferKey::new(
            ws_ref,
            bicdb_storage::rowid::Rdba::from_parts(file_id, 0).expect("file header address"),
        ));
    }
    if is_public {
        pool.set_critical_workspace(ws_ref);
    }
    let mut engine = Engine::new(
        pool,
        writer,
        UndoChain::open(undo_seg).with_pool(pool),
        seq(0),
    );
    engine.set_policy(wait_policy(params));
    let engine: &'static Engine<'static, 'static, 'static, 'static> = Box::leak(Box::new(engine));
    let mut catalog =
        Catalog::open(io_dyn, &file0_path).map_err(|e| BootError::Catalog(e.to_string()))?;
    catalog.attach_pool(pool);

    // ⑤ 建区收尾：`stat$`/`seq$` + `object_id` 序列（§5.1 ⑤）。
    ddl::init_dictionary_tables(&mut catalog, engine)
        .map_err(|e| BootError::Catalog(e.to_string()))?;
    catalog.set_current_seq(seq(engine.current_seq()));

    // ⑥ **`public` 的 `ws$` 行**（管理面属性："public 也是一条工作区记录"，
    //    只是它的 `user_id` 永远是 NULL——共享工作区）。
    if is_public {
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "public".to_owned());
        let eng: &Engine<'_, '_, '_, '_> = engine;
        bicdb_catalog::dcl::insert_ws(
            &mut catalog,
            eng,
            &bicdb_catalog::dcl::WsEntry {
                workspace_id: ws_id.as_raw(),
                user_id: None,
                name,
                status: bicdb_catalog::dcl::ws_status::ACTIVE,
                ctime_ms: now_ms(),
                quota: [0; 4],
                default_fs: None,
            },
        )
        .map_err(|e| BootError::Catalog(format!("public 的 ws$ 行建不出来：{e}")))?;
    }

    // ⑦ **登记进实例注册表**（三步协议的 ③——**最后一步**，可见性开关）。
    if opts.register {
        register_workspace(home, dir, ws_id)?;
    }

    let mut instance = Instance {
        params: params.clone(),
        ws_ref,
        dir: dir.to_path_buf(),
        io,
        undo_path: undo_path.clone(),
        pool,
        engine,
        catalog,
        shared_cache: None,
        recovery: None,
        forced_recovery: None,
        fault_audit: None,
        lock: Some(lock),
        txn: None,
    };
    instance.start_direct_fault_audit()?;
    Ok(instance)
}

/// **打开既有实例（直连模式）**：先取实例锁（单写者），再打开。
///
/// 服务在跑（或别的进程直连着）⇒ [`BootError::Occupied`]——**不**静默并存：
/// 两个写者各写各的池与日志不是并发，是互相破坏。
pub fn open_instance(params: &InstanceParams) -> Result<Instance, BootError> {
    let lock = InstanceLock::acquire(&params.db_root, LockMode::Direct, &params.socket_path())?;
    open_unlocked_with(params, Some(lock))
}

/// **打开（参数已装载）**：服务路径用它（命令行 `-c` 覆盖在这里生效）。
pub fn open_unlocked_with(
    params: &InstanceParams,
    lock: Option<InstanceLock>,
) -> Result<Instance, BootError> {
    open_unlocked_internal(params, lock, None, false, false)
}

/// Open PUBLIC once, then attach logical workspaces to its shared cache.
/// This does not create a daemon or a second database cache.
pub fn open_shared_with(
    params: &InstanceParams,
    lock: Option<InstanceLock>,
    shared: Option<Arc<SharedInstanceCache>>,
) -> Result<Instance, BootError> {
    open_unlocked_internal(params, lock, shared, true, false)
}

/// Explicitly allow the narrow, audited private-workspace FORCE recovery
/// classifications. The function independently verifies that the target is
/// not PUBLIC; callers cannot turn this into a PUBLIC bypass.
pub fn open_shared_force_private(
    params: &InstanceParams,
    lock: Option<InstanceLock>,
    shared: Option<Arc<SharedInstanceCache>>,
) -> Result<Instance, BootError> {
    open_unlocked_internal(params, lock, shared, true, true)
}

#[derive(Debug)]
struct ForceRecovery {
    requested: bool,
    skipped: Vec<String>,
}

fn apply_persisted_recovery_isolation(
    pool: &BufferPool<'_>,
    workspace: [u8; 8],
    is_public: bool,
    records: &[bicdb_storage::recovery_journal::RecoveryRecord],
) -> Result<(), BootError> {
    use bicdb_storage::recovery_journal::{RecoveryScope, RecoveryState};
    let mut pages = std::collections::BTreeMap::new();
    let mut objects = std::collections::BTreeMap::new();
    let mut broad = std::collections::BTreeMap::new();
    for record in records {
        let clear = record.state == RecoveryState::Verified;
        let reason = format!(
            "recovery$ sequence {} {}: {}",
            record.sequence, record.actor, record.detail
        );
        match record.scope {
            RecoveryScope::Workspace => {}
            RecoveryScope::Page { file_id, block_id } => {
                let rdba = Rdba::from_parts(file_id, block_id).ok_or_else(|| {
                    BootError::Catalog(format!(
                        "恢复审计 sequence {} 的页面范围无效",
                        record.sequence
                    ))
                })?;
                if clear {
                    pages.remove(&rdba);
                } else {
                    pages.entry(rdba).or_insert(reason);
                }
            }
            RecoveryScope::Object { object_id } => {
                if object_id > u64::from(u32::MAX) {
                    return Err(BootError::Catalog(format!(
                        "恢复审计 sequence {} 的对象号越界",
                        record.sequence
                    )));
                }
                if clear {
                    objects.remove(&object_id);
                } else {
                    objects.entry(object_id).or_insert(reason);
                }
            }
            RecoveryScope::Transaction { txn_id } => {
                let key = format!("TRANSACTION {txn_id}");
                if clear {
                    broad.remove(&key);
                } else {
                    broad.entry(key).or_insert(reason);
                }
            }
            RecoveryScope::RedoRange { start_lsn, end_lsn } => {
                let key = format!("REDO [{start_lsn},{end_lsn})");
                if clear {
                    broad.remove(&key);
                } else {
                    broad.entry(key).or_insert(reason);
                }
            }
        }
    }
    if is_public && (!pages.is_empty() || !objects.is_empty() || !broad.is_empty()) {
        return Err(BootError::Catalog(
            "PUBLIC 存在未清除的结构化恢复隔离项，必须修复并验证后才能开放".into(),
        ));
    }
    for (rdba, reason) in pages {
        pool.quarantine_page(
            bicdb_storage::buffer::BufferKey::new(workspace, rdba),
            reason,
        );
    }
    for (object_id, reason) in objects {
        pool.quarantine_object(workspace, object_id, reason);
    }
    if !broad.is_empty() {
        pool.set_workspace_read_only(
            workspace,
            broad.into_values().collect::<Vec<_>>().join("；"),
        );
    }
    Ok(())
}
impl ForceRecovery {
    fn new(requested: bool, is_public: bool) -> Result<Self, BootError> {
        if requested && is_public {
            return Err(BootError::Catalog(
                "PUBLIC 禁止 FORCE：必须完成一致性修复后才能开放".into(),
            ));
        }
        Ok(Self {
            requested,
            skipped: Vec::new(),
        })
    }
    fn check(&mut self, inconsistent: bool, detail: String) -> Result<(), BootError> {
        if !inconsistent {
            return Ok(());
        }
        if !self.requested {
            return Err(BootError::Catalog(format!("拒绝打开：{detail}")));
        }
        self.skipped.push(detail);
        Ok(())
    }
    fn detail(self) -> Option<String> {
        (!self.skipped.is_empty()).then(|| self.skipped.join("；"))
    }
}

fn open_unlocked_internal(
    params: &InstanceParams,
    lock: Option<InstanceLock>,
    shared: Option<Arc<SharedInstanceCache>>,
    use_shared: bool,
    force_private: bool,
) -> Result<Instance, BootError> {
    let dir = params.db_root.clone();
    let dir = dir.as_path();
    if shared.is_none() {
        apply_process_params(params)?;
    }
    let io: &'static OsFileIo = shared
        .as_ref()
        .map(|cache| cache.io)
        .unwrap_or_else(|| Box::leak(Box::new(OsFileIo::new())));
    let io_dyn: &'static dyn FileIo = io;
    let wal_path = p(dir, WAL_DIR);
    let (cf_path_a, cf_path_b) = (p(dir, CF_A), p(dir, CF_B));

    // **先读控制文件拿工作区号**（它在固定路径上），再由它算 `workspace_ref`
    // 与数据文件名——"文件头里的标识"与"文件名里的标识"同源，不会各说各话。
    let ws_id = {
        let cf_ro = ControlFile::open(io_dyn, &cf_path_a, &cf_path_b)?;
        let result = cf_ro.workspace_entry().map(|entry| entry.workspace_id);
        let close = cf_ro.close();
        let ws_id = result?;
        close?;
        ws_id
    };
    let ws_ref = bicdb_workspace::workspace_ref(ws_id);
    use bicdb_storage::recovery_journal::{RecoveryJournal, RecoveryState};
    let mut recovery_audit = RecoveryJournal::open(io_dyn, &dir.join("recovery.audit"), ws_ref)?;
    recovery_audit.append(
        RecoveryState::Recovering,
        0,
        "bicdb/open",
        "恢复开始；起点尚待控制进度校验",
    )?;
    // Keep the instance lock outside the closure until audit publication.
    // No connection or background registration can observe an unaudited open.
    let result = (|| -> Result<Instance, BootError> {
        let file0_path = data_path(dir, ws_ref, "meta");
        if !file0_path.exists() {
            return Err(BootError::Catalog(missing_meta_message(dir, &file0_path)));
        }
        // Determine PUBLIC from the dictionary shape itself, before any FORCE
        // decision. A path comparison alone is insufficient for standalone or
        // relocated deployments. Failure to parse the dictionary is structural
        // damage and is never force-skippable.
        let identity_catalog =
            Catalog::open(io_dyn, &file0_path).map_err(|e| BootError::Catalog(e.to_string()))?;
        let is_public = identity_catalog.is_public();
        identity_catalog
            .close()
            .map_err(|e| BootError::Catalog(e.to_string()))?;
        let mut force = ForceRecovery::new(force_private, is_public)?;
        let undo_path = data_path(dir, ws_ref, "undo");

        let file0_handle = DataFile::open(io_dyn, &file0_path)?.handle();
        let undo_file: &'static mut DataFile<'static> =
            Box::leak(Box::new(DataFile::open(io_dyn, &undo_path)?));
        let undo_handle = undo_file.handle();
        let undo_seg = Segment::open(undo_file, undo_page0())?;
        // Temp never participates in redo/undo recovery. Reset it before any
        // session can be admitted; stale contents from either a clean or crashed
        // previous process are never reusable.
        let temp_path = data_path(dir, ws_ref, "temp");
        let temp = DataFile::open_temp_reset(io_dyn, &temp_path, ws_ref, DEFAULT_TEMP_BLOCKS)?;
        temp.sync()?;
        temp.close()?;

        // 起点 = 控制文件的检查点 LSN（低水位）。
        let spec = group_spec(&params.init);
        let progress = {
            let cf_ro = ControlFile::open(io_dyn, &cf_path_a, &cf_path_b)?;
            let result = (|| {
                // **建区期参数核对**（控制文件权威）：不符 ⇒ 拒绝打开（改需重建）。
                check_creation_facts(dir, params, &cf_ro.redo_entries()?)?;
                Ok::<_, BootError>(cf_ro.checkpoint_progress()?)
            })();
            let close = cf_ro.close();
            let progress = result?;
            close?;
            progress
        };

        // **打开链的事实核对**（`目录详设` §2.5）：各文件头位点 vs 控制文件检查点。
        // 超前（文件比控制文件新：拷错/配错控制文件）⇒ **拒绝打开**；本函数此前
        // 直接进恢复，`catalog::consistency` 整模块只有单测消费者（2026-10-06 审计）。
        let file0_head = DataFile::open(io_dyn, &file0_path)?;
        let undo_head = DataFile::open(io_dyn, &undo_path)?;
        let file0_scn = file0_head.file_scn();
        let undo_scn = undo_head.file_scn();
        let checkpoint_ahead = [
            file0_head.checkpoint_commit_scn(),
            undo_head.checkpoint_commit_scn(),
        ]
        .into_iter()
        .any(|scn| scn > progress.checkpoint_commit_seq.as_raw());
        force.check(
            checkpoint_ahead,
            "数据文件 checkpoint SCN 超前于控制文件".to_owned(),
        )?;
        file0_head.close()?;
        undo_head.close()?;

        // 写口（续写位置在组集内部重建）+ 只读组视图（恢复的扫描面）。
        let cf: &'static mut ControlFile<'static> =
            Box::leak(Box::new(ControlFile::open(io_dyn, &cf_path_a, &cf_path_b)?));
        // **幂等补登记**：文件清单的权威是控制文件（`目录详设` 纪律 7），而本切片
        // 之前的实例从未写过它——不补的话 `file$` 在旧实例上永远是空集。
        // `creation_blocks` 在补登记时取**当前大小**（创建时的值已不可考，如实记当前）。
        publish_data_files(
            io_dyn,
            cf,
            &[
                (FILE0_ID, META_ROLE, &file0_path),
                (UNDO_ID, UNDO_FILE_ROLE, &undo_path),
                (
                    bicdb_storage::datafile::TEMP_FILE_ID,
                    bicdb_storage::datafile::TEMP_FILE_ROLE,
                    &temp_path,
                ),
            ],
        )?;
        let mut writer = GroupWriter::open(io_dyn, cf, &wal_path, spec)?;

        // **恢复**（顺序不可换：分析 → 重做 → 输家回滚；见 `wal::recovery`）。
        let mut chain = UndoChain::open(undo_seg);
        let summary = {
            let cf_ro = ControlFile::open(io_dyn, &cf_path_a, &cf_path_b)?;
            let groups_result = online_groups(io_dyn, &cf_ro, &wal_path, spec);
            let close = cf_ro.close();
            let groups = groups_result?;
            close?;
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
                let findings = report
                    .findings
                    .iter()
                    .map(|f| f.to_string())
                    .collect::<Vec<_>>()
                    .join("；");
                force.check(
                    true,
                    format!("文件超前于控制文件（{findings}）——像是配错了控制文件/拷错文件"),
                )?;
            }
            if !report.is_clean() {
                eprintln!("（一致性核对有发现：{report:?}）");
            }
            let mut resolve = |r: Rdba| resolve(file0_handle, undo_handle, r);
            let recovery = recover(
                io_dyn,
                &groups,
                progress.checkpoint_lsn,
                &mut chain,
                &mut writer,
                &mut resolve,
            );
            let mut close_error = None;
            for group in &groups {
                if let Err(error) = io_dyn.close(group.handle) {
                    close_error.get_or_insert(error);
                }
            }
            let report = recovery?;
            if let Some(error) = close_error {
                return Err(BootError::Io(error));
            }
            // Recovery is an admission gate. No Active/PendingRollback slot may
            // survive the undo phase; exposing SQL in that state would turn an
            // incomplete recovery into normal runtime state.
            let header = chain.page(0).map_err(|error| {
                BootError::Catalog(format!("恢复后读取 undo 状态失败：{error}"))
            })?;
            for slot_no in 0..bicdb_storage::undo::TXN_SLOTS as u16 {
                let slot = bicdb_storage::undo::read_slot(&header, slot_no).map_err(|error| {
                    BootError::Catalog(format!("恢复后校验 undo 状态失败：{error}"))
                })?;
                if matches!(
                    slot.state,
                    bicdb_storage::undo::TxnState::Active
                        | bicdb_storage::undo::TxnState::PendingRollback
                ) {
                    return Err(BootError::Catalog(format!(
                        "恢复未完成：undo 事务槽 {slot_no} 仍为 {:?}",
                        slot.state
                    )));
                }
            }
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
        let shared_cache = if use_shared {
            Some(match shared {
                Some(cache) => cache,
                None => SharedInstanceCache::new(io, params)?,
            })
        } else {
            None
        };
        let pool: &'static BufferPool<'static> = if let Some(cache) = &shared_cache {
            cache.register(ws_ref, file0_handle, undo_handle, guard)?;
            cache.pool
        } else {
            Box::leak(Box::new(BufferPool::with_partitions(
                io_dyn,
                params.run.kcbwds,
                cache_partition_capacity(params)?,
                move |ws, r| {
                    if *ws == ws_ref {
                        resolve(file0_handle, undo_handle, r)
                    } else {
                        None
                    }
                },
                guard,
                SystemClock,
                cache_config(params),
            )?))
        };
        for file_id in [FILE0_ID, UNDO_ID] {
            pool.register_checkpoint_file(bicdb_storage::buffer::BufferKey::new(
                ws_ref,
                bicdb_storage::rowid::Rdba::from_parts(file_id, 0).expect("file header address"),
            ));
        }
        if crate::home::Home::locate().is_ok_and(|home| same_dir(dir, &home.public_dir())) {
            pool.set_critical_workspace(ws_ref);
        }
        apply_persisted_recovery_isolation(pool, ws_ref, is_public, recovery_audit.records())?;
        let mut engine = Engine::new(pool, writer, chain.with_pool(pool), seq(recovered_seq));
        engine.set_policy(wait_policy(params));
        let engine: &'static Engine<'static, 'static, 'static, 'static> =
            Box::leak(Box::new(engine));
        // Make recovery effects, compensation redo and the new checkpoint durable
        // before the caller can bind a socket or publish READY.
        engine.checkpoint_full(ws_ref)?;
        if pool.dirty_len(ws_ref) != 0 {
            return Err(BootError::Catalog(
                "恢复检查点完成后仍存在脏块，拒绝开放实例".into(),
            ));
        }
        let mut catalog =
            Catalog::open(io_dyn, &file0_path).map_err(|e| BootError::Catalog(e.to_string()))?;
        catalog.attach_pool(pool);
        catalog.set_current_seq(seq(recovered_seq));

        Ok(Instance {
            params: params.clone(),
            ws_ref,
            dir: dir.to_path_buf(),
            io,
            undo_path: undo_path.clone(),
            pool,
            engine,
            catalog,
            shared_cache,
            recovery: Some(summary),
            forced_recovery: force.detail(),
            fault_audit: None,
            lock: None,
            txn: None,
        })
    })();
    match result {
        Ok(mut instance) => {
            let summary = instance
                .recovery
                .expect("opened workspace recovery receipt");
            let (audit_state, actor, detail) = if let Some(detail) = &instance.forced_recovery {
                (
                    RecoveryState::Forced,
                    "bicdb/start --force",
                    format!("私有工作区完成恢复后强制开放；跳过项：{detail}"),
                )
            } else {
                (
                    RecoveryState::Verified,
                    "bicdb/open",
                    "redo/undo 校验、loser 回滚及耐久检查点完成".to_owned(),
                )
            };
            if let Err(error) = recovery_audit.append(audit_state, summary.log_end, actor, &detail)
            {
                instance
                    .pool
                    .quarantine_workspace(ws_ref, format!("恢复审计持久化失败：{error}"));
                return Err(BootError::Io(error));
            }
            instance.lock = lock;
            if !use_shared {
                instance.start_direct_fault_audit()?;
            }
            Ok(instance)
        }
        Err(error) => {
            let mut detail = error.to_string();
            if detail.len() > 4096 {
                let mut end = 4096;
                while !detail.is_char_boundary(end) {
                    end -= 1;
                }
                detail.truncate(end);
            }
            match recovery_audit.append(RecoveryState::Failed, 0, "bicdb/open", &detail) {
                Ok(_) => Err(error),
                Err(audit_error) => Err(BootError::Io(std::io::Error::other(format!(
                    "恢复失败：{error}；审计持久化也失败：{audit_error}"
                )))),
            }
        }
    }
}

#[cfg(test)]
mod cache_admission_tests {
    use super::*;
    #[test]
    fn two_gib_is_guaranteed_per_activated_workspace() {
        assert!(check_cache_admission(3 * MIN_WORKSPACE_CACHE_FRAMES, 3).is_ok());
        assert!(check_cache_admission(3 * MIN_WORKSPACE_CACHE_FRAMES - 1, 3).is_err());
        assert!(check_cache_admission(MIN_WORKSPACE_CACHE_FRAMES, 2).is_err());
        assert!(check_cache_admission(usize::MAX, usize::MAX).is_err());
    }
    #[test]
    fn cache_frames_are_total_and_divided_between_writers() {
        let mut params = InstanceParams::default();
        params.run.pool_frames = 3 * MIN_WORKSPACE_CACHE_FRAMES;
        params.run.kcbwds = 4;
        assert_eq!(
            cache_partition_capacity(&params).unwrap() * 4,
            params.run.pool_frames
        );
        params.run.pool_frames += 1;
        assert!(cache_partition_capacity(&params).is_err());
        params.run.kcbwds = 3;
        assert!(cache_partition_capacity(&params).is_err());
    }

    #[test]
    fn force_is_private_narrow_and_only_records_actual_skips() {
        assert!(ForceRecovery::new(true, true)
            .unwrap_err()
            .to_string()
            .contains("PUBLIC 禁止 FORCE"));

        let mut normal = ForceRecovery::new(false, false).unwrap();
        assert!(normal
            .check(true, "checkpoint ahead".into())
            .unwrap_err()
            .to_string()
            .contains("拒绝打开"));

        let mut forced = ForceRecovery::new(true, false).unwrap();
        forced.check(false, "unused".into()).unwrap();
        forced.check(true, "checkpoint ahead".into()).unwrap();
        assert_eq!(forced.detail().as_deref(), Some("checkpoint ahead"));

        assert!(ForceRecovery::new(true, false).unwrap().detail().is_none());
    }
}
