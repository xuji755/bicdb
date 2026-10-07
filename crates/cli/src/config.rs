//! **实例参数文件** `bicdb.ini`（Oracle pfile/spfile 分工的**文本那一半**）。
//!
//! ```text
//! <db_root>/bicdb.ini      ← 根区目录里的这份，是**实例的唯一入口**
//!
//! [instance]
//! db_root = /data/bicdb    ← 根区目录**注册在这里**（实例的全部文件都在它下面）
//! [init]                   ← 建区期参数（`bicdb init` 读它；建区后只作记录）
//! [buffer] [storage] [service] [lock]   ← 运行期参数（改完重启生效）
//! ```
//!
//! **寻址模型（照 Oracle，不照"启动指向某个目录"）**：
//! - `init` 是**唯一**接受根区目录的命令——它就是"指向文件系统"那一步，
//!   并在根区里**生成默认参数文件**；
//! - 此后 `start`/`stop`/`status`/`params`/`sql`/`shell`/`bicdbcli` 一律
//!   **按参数文件寻址**：`-p <路径>` > 环境变量 `BICDB_INI` > `./bicdb.ini`；
//!   `-p` 给目录时等价于该目录下的 `bicdb.ini`（便利形态）；
//! - `db_root` 由参数文件**注册**（权威）——参数文件放哪儿都行，实例在哪儿由它说。
//!
//! **四条纪律**：
//! 1. **闭集**：未知节/未知键**具名拒绝**（收下不生效就是空壳）；
//! 2. **关键参数不硬编码**：建区期（`[init]`）与运行期（其余节）都由文件给，
//!    程序里只留"没有文件时"的默认值；
//! 3. **建区期参数建区后不可改**：控制文件是权威，`start` 逐项核对，
//!    不符即**拒绝启动**并说明"改需重建"；
//! 4. **三来源**：命令行 `-c 键=值` > 参数文件 > 内置默认（`bicdb params` 显示来源）。

use std::path::{Path, PathBuf};

/// 参数文件名（**根区目录**下）。
pub const FILE_NAME: &str = "bicdb.ini";

/// 环境变量：不给 `-p` 时用它找参数文件（Oracle `ORACLE_SID` / PG `PGDATA` 的位置）。
pub const ENV_INI: &str = "BICDB_INI";

/// 参数错误的来源（诊断要能指到"哪个文件、哪一行、哪一节"）。
#[derive(Debug)]
pub enum ConfigError {
    /// 读文件失败。
    Io {
        /// 路径。
        path: PathBuf,
        /// 底层错误。
        why: std::io::Error,
    },
    /// **找不到参数文件**（三个来源都没有）。
    NotFound {
        /// 试过哪些位置（诊断）。
        tried: Vec<PathBuf>,
    },
    /// **未知节**。
    UnknownSection {
        /// 节名。
        section: String,
        /// 行号（1 起）。
        line: usize,
    },
    /// **未知键**。
    UnknownKey {
        /// 节名。
        section: String,
        /// 键名。
        key: String,
        /// 行号。
        line: usize,
    },
    /// 取值非法。
    BadValue {
        /// 键名（`节.键`）。
        key: String,
        /// 取值文本。
        value: String,
        /// 为什么不行。
        why: String,
    },
    /// 行形态非法（缺 `=`、键出现在任何节之前…）。
    Malformed {
        /// 行号。
        line: usize,
        /// 原文。
        text: String,
    },
    /// **必填项缺失**（`db_root`）。
    Missing {
        /// 键名。
        key: String,
    },
    /// **建区期参数与既有的库不符**（控制文件权威；改需重建）。
    CreationMismatch {
        /// 逐项不符清单。
        diffs: Vec<String>,
    },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Io { path, why } => write!(f, "读 {}：{why}", path.display()),
            ConfigError::NotFound { tried } => {
                write!(f, "找不到参数文件 {FILE_NAME}——试过：")?;
                for t in tried {
                    write!(f, "{}；", t.display())?;
                }
                write!(
                    f,
                    "（用 `-p <参数文件|根区目录>` 指定，或设环境变量 {ENV_INI}）"
                )
            }
            ConfigError::UnknownSection { section, line } => write!(
                f,
                "{FILE_NAME} 第 {line} 行：未知节 `[{section}]`（`bicdb params` 看全部可调项）"
            ),
            ConfigError::UnknownKey { section, key, line } => write!(
                f,
                "{FILE_NAME} 第 {line} 行：`[{section}]` 里没有参数 `{key}`（闭集）"
            ),
            ConfigError::BadValue { key, value, why } => {
                write!(f, "{FILE_NAME}：`{key} = {value}` 不合法——{why}")
            }
            ConfigError::Malformed { line, text } => write!(
                f,
                "{FILE_NAME} 第 {line} 行形态非法（要 `[节]` 或 `键 = 值`）：`{text}`"
            ),
            ConfigError::Missing { key } => {
                write!(
                    f,
                    "{FILE_NAME} 缺必填项 `{key}`（`[instance] db_root = …`）"
                )
            }
            ConfigError::CreationMismatch { diffs } => {
                write!(f, "建区期参数与既有的库不符（**改需重建**）：")?;
                for d in diffs {
                    write!(f, "\n  {d}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ConfigError {}

/// 参数的最终来源（`bicdb params` 显示用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// 内置默认。
    Default,
    /// 参数文件。
    File,
    /// 命令行 `-c`。
    Cli,
}

impl Source {
    /// 显示名。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Default => "默认",
            Source::File => "文件",
            Source::Cli => "命令行",
        }
    }
}

/// **建区期参数**（`[init]`；`bicdb init` 读它造库，之后控制文件是权威）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitParams {
    /// file 0（字典/数据）初始块数。
    pub file0_initial_blocks: u64,
    /// 撤销文件初始块数。
    pub undo_initial_blocks: u64,
    /// 日志组数（2–8）。
    pub wal_groups: u8,
    /// 每组成员数（1–8）。
    pub wal_members: u8,
    /// 每组成员页数（下限 34 页 = 一条记录的最大 footprint）。
    pub wal_group_pages: u32,
}

impl Default for InitParams {
    fn default() -> Self {
        Self {
            file0_initial_blocks: crate::boot::DEFAULT_FILE0_BLOCKS,
            undo_initial_blocks: crate::boot::DEFAULT_UNDO_BLOCKS,
            wal_groups: 2,
            wal_members: 1,
            wal_group_pages: crate::boot::DEFAULT_WAL_GROUP_PAGES,
        }
    }
}

/// **运行期参数**（改完重启生效）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunParams {
    /// 缓冲池帧数（16 KiB/帧）。
    pub pool_frames: usize,
    /// 段增长撞文件尾时的固定增量（块）。
    pub file_extend_blocks: u64,
    /// 控制套接字（相对根区目录；也接受绝对路径）。
    pub socket: String,
    /// 服务日志（相对根区目录；也接受绝对路径）。
    pub log: String,
    /// 等锁单次挂起时长（毫秒）。
    pub park_ms: u64,
    /// 死锁检测阈值（毫秒）。
    pub deadlock_threshold_ms: u64,
    /// 等锁重试次数上限（0 = 不限）。
    pub wait_max_rounds: u64,
    // ── buffer ──
    /// 哈希桶数（0 = 自动）。
    pub hash_buckets: usize,
    /// 桶闩锁数（0 = 自动）。
    pub bucket_latches: usize,
    /// 热段上限 = 容量/该值。
    pub hot_fraction: usize,
    /// 触摸计数的最小递增间隔（毫秒）。
    pub touch_interval_ms: u64,
    /// 冷却值。
    pub cool_count: u32,
    /// 驻留值。
    pub stay_count: u32,
    /// 热判据。
    pub hot_criteria: u32,
    /// 找空帧的前台扫描上限 = 容量/该值。
    pub max_scan_fraction: usize,
    /// 每轮写回批大小 = 分区帧数/该值。
    pub make_free_batch_divisor: usize,
    // ── storage ──
    /// 一次区读的连续页数。
    pub multiblock_read_pages: u32,
    /// 一致性读重建的回溯轮数上限。
    pub cr_max_rounds: u32,
    // ── wal ──
    /// 日志缓冲区页数。
    pub log_buffer_pages: usize,
    /// 未刷出占比达此千分数即建议刷盘。
    pub flush_trigger_permille: u64,
    // ── catalog ──
    /// 字典行缓存：每型行数上限。
    pub row_cache_rows: usize,
    /// 字典行缓存：估算字节上限。
    pub row_cache_bytes: u64,
    /// 行迁移转发链最大跳数。
    pub rid_forward_max_hops: usize,
    // ── index ──
    /// 建索引（批量灌树）叶页填充率（%）。
    pub bulk_fill_percent: u8,
    // ── service（服务生命周期） ──
    /// `start` 等就绪的默认上限（秒）。
    pub start_wait_s: u64,
    /// `stop` 等退出的默认上限（秒）。
    pub stop_wait_s: u64,
    /// 等就绪的轮询间隔（毫秒）。
    pub ready_poll_ms: u64,
    // ── auth（认证与口令） ──
    /// **口令散列（PBKDF2-HMAC-SHA512）的迭代数**。
    ///
    /// 写进散列串（`pbkdf2-sha512$<迭代数>$…`）⇒ **调大不破旧行**（旧行按行里的数校验）。
    /// 默认照 OWASP 2023 对 PBKDF2-HMAC-SHA512 的建议量级。
    pub pbkdf2_iterations: u32,
    /// 等退出的轮询间隔（毫秒）。
    pub stop_poll_ms: u64,
    /// 每轮就绪探测的单次读写超时（毫秒）。
    pub probe_timeout_ms: u64,
    /// `status` 问服务自述的超时（毫秒）。
    pub status_timeout_ms: u64,
    /// 启动失败时回显的日志尾行数。
    pub log_tail_lines: u64,
    // ── client ──
    /// 客户端握手超时（毫秒）。
    pub handshake_timeout_ms: u64,
    /// 客户端请求超时（毫秒；0 = 不限）。
    pub request_timeout_ms: u64,
}

impl Default for RunParams {
    fn default() -> Self {
        // 全部取自 [`SPECS`] 的声明（**默认值只有一个来源**：改声明即改默认）。
        let d = |k: &str| spec_default(k);
        Self {
            pool_frames: d("pool_frames").parse().expect("16 位以上"),
            file_extend_blocks: bicdb_storage::segment::DEFAULT_FILE_EXTEND_BLOCKS,
            socket: crate::lock::SOCKET_FILE.to_owned(),
            log: "bicdb.log".to_owned(),
            park_ms: bicdb_txn::write::WaitPolicy::default()
                .park_timeout
                .as_millis() as u64,
            // **与库里的常量同源**：此前这里写 1000、`txn::lock` 写 3000，
            // 走参数文件的库与直接调库的库行为不同（同一个参数两个默认值）。
            deadlock_threshold_ms: bicdb_txn::write::WaitPolicy::default().deadlock_threshold_ms,
            wait_max_rounds: json_u64(d("wait_max_rounds")),
            hash_buckets: json_u64(d("hash_buckets")) as usize,
            bucket_latches: json_u64(d("bucket_latches")) as usize,
            hot_fraction: json_u64(d("hot_fraction")) as usize,
            touch_interval_ms: json_u64(d("touch_interval_ms")),
            cool_count: json_u64(d("cool_count")) as u32,
            stay_count: json_u64(d("stay_count")) as u32,
            hot_criteria: json_u64(d("hot_criteria")) as u32,
            max_scan_fraction: json_u64(d("max_scan_fraction")) as usize,
            make_free_batch_divisor: json_u64(d("make_free_batch_divisor")) as usize,
            multiblock_read_pages: json_u64(d("multiblock_read_pages")) as u32,
            cr_max_rounds: json_u64(d("cr_max_rounds")) as u32,
            log_buffer_pages: json_u64(d("log_buffer_pages")) as usize,
            flush_trigger_permille: json_u64(d("flush_trigger_permille")),
            row_cache_rows: json_u64(d("row_cache_rows")) as usize,
            row_cache_bytes: json_u64(d("row_cache_bytes")),
            rid_forward_max_hops: json_u64(d("rid_forward_max_hops")) as usize,
            bulk_fill_percent: json_u64(d("bulk_fill_percent")) as u8,
            start_wait_s: json_u64(d("start_wait_s")),
            stop_wait_s: json_u64(d("stop_wait_s")),
            ready_poll_ms: json_u64(d("ready_poll_ms")),
            pbkdf2_iterations: json_u64(d("pbkdf2_iterations")) as u32,
            stop_poll_ms: json_u64(d("stop_poll_ms")),
            probe_timeout_ms: json_u64(d("probe_timeout_ms")),
            status_timeout_ms: json_u64(d("status_timeout_ms")),
            log_tail_lines: json_u64(d("log_tail_lines")),
            handshake_timeout_ms: json_u64(d("handshake_timeout_ms")),
            request_timeout_ms: json_u64(d("request_timeout_ms")),
        }
    }
}

/// [`SPECS`] 里的默认文本（找不到即 panic——表里少一行是编码错误）。
fn spec_default(key: &str) -> &'static str {
    SPECS
        .iter()
        .find(|s| s.key == key)
        .map(|s| s.default)
        .unwrap_or_else(|| panic!("SPECS 里没有键 `{key}`"))
}

/// 十进制文本 → 数（`SPECS` 的默认与参数文件的取值同一条解析路径）。
fn json_u64(text: &str) -> u64 {
    text.parse().unwrap_or_else(|_| panic!("`{text}` 不是数"))
}

/// **一份完整的实例参数**（= 参数文件的全部内容）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceParams {
    /// 根区目录（**注册在参数文件里**；实例的全部文件都在它下面）。
    pub db_root: PathBuf,
    /// 参数文件自身的路径（诊断；`None` = 还没落到某个文件上）。
    pub ini_path: Option<PathBuf>,
    /// 建区期参数。
    pub init: InitParams,
    /// 运行期参数。
    pub run: RunParams,
}

impl Default for InstanceParams {
    fn default() -> Self {
        Self {
            db_root: PathBuf::new(),
            ini_path: None,
            init: InitParams::default(),
            run: RunParams::default(),
        }
    }
}

/// 参数清单（`节.键` → 取值 → 来源）。
pub type ParamTable = Vec<(String, String, Source)>;

/// **参数的生效时机**（照 PG 的 GUC 分层简化而来：`PGC_POSTMASTER` = 要重启、
/// 建库参数 = 我们独有的"建区期"）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// **建区期**：`bicdb init` 读它造库；建区后控制文件是权威（改了会被拒绝启动）。
    Creation,
    /// **重启生效**：改完 `bicdb restart`（服务起来时读一次）。
    Restart,
}

impl Effect {
    /// 显示名（`bicdb params` 的"类别"列）。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Effect::Creation => "建区期",
            Effect::Restart => "运行期",
        }
    }
}

/// **一个参数的声明**（唯一事实源：闭集校验、参数文件渲染、`bicdb params` 的
/// 清单与说明都读它）。
///
/// **加一个参数的纪律**：在这里加一行 + 在 [`RunParams`]/[`InitParams`] 加字段 +
/// 在 `set`/`value_of` 各加一个 match 臂（都在本文件），再去消费点接线。
/// 以前是"五处各写一遍"（节集合 / set / render / all_keys / main 的打印），
/// 加一个键要改五个地方、漏一个就出现"文件里能写但没人读"。
pub struct Spec {
    /// 节（闭集；渲染时按节的顺序成组）。
    pub section: &'static str,
    /// 键。
    pub key: &'static str,
    /// 生效时机。
    pub effect: Effect,
    /// 内置默认（文本形态；渲染与"来源=默认"用）。
    pub default: &'static str,
    /// 一句话说明（渲染成行尾注释、`bicdb params` 里打印）。
    pub doc: &'static str,
}

/// **全部参数**（顺序 = 渲染顺序 = `bicdb params` 顺序）。
pub const SPECS: &[Spec] = &[
    Spec {
        section: "instance",
        key: "db_root",
        effect: Effect::Restart,
        default: "",
        doc: "根区目录（权威）：实例的全部文件都在它下面",
    },
    Spec {
        section: "init",
        key: "file0_initial_blocks",
        effect: Effect::Creation,
        default: "4096",
        doc: "file 0（字典/数据）初始块数（16 KiB/块）",
    },
    Spec {
        section: "init",
        key: "undo_initial_blocks",
        effect: Effect::Creation,
        default: "512",
        doc: "撤销文件初始块数",
    },
    Spec {
        section: "init",
        key: "wal_groups",
        effect: Effect::Creation,
        default: "2",
        doc: "日志组数（2–8，至少 2 组轮转）",
    },
    Spec {
        section: "init",
        key: "wal_members",
        effect: Effect::Creation,
        default: "1",
        doc: "每组成员数（1–8）",
    },
    Spec {
        section: "init",
        key: "wal_group_pages",
        effect: Effect::Creation,
        default: "8192",
        doc: "每组成员页数（512 B/页）",
    },
    Spec {
        section: "buffer",
        key: "pool_frames",
        effect: Effect::Restart,
        default: "256",
        doc: "缓冲池帧数（16 KiB/帧）",
    },
    Spec {
        section: "buffer",
        key: "hash_buckets",
        effect: Effect::Restart,
        default: "0",
        doc: "哈希桶数（0 = 自动 ≈ 容量/4 取质数；Oracle _DB_BLOCK_HASH_BUCKETS 口径）",
    },
    Spec {
        section: "buffer",
        key: "bucket_latches",
        effect: Effect::Restart,
        default: "0",
        doc: "桶闩锁数（0 = 自动；须为 2 的幂，≤ 64）",
    },
    Spec {
        section: "buffer",
        key: "hot_fraction",
        effect: Effect::Restart,
        default: "4",
        doc: "热段上限 = 容量/该值（Oracle HBMAX 口径）",
    },
    Spec {
        section: "buffer",
        key: "touch_interval_ms",
        effect: Effect::Restart,
        default: "3000",
        doc: "触摸计数的最小递增间隔（三秒规则）",
    },
    Spec {
        section: "buffer",
        key: "cool_count",
        effect: Effect::Restart,
        default: "0",
        doc: "冷却值：新装入/退回冷段时置的触摸计数（_COOL_COUNT 口径）",
    },
    Spec {
        section: "buffer",
        key: "stay_count",
        effect: Effect::Restart,
        default: "2",
        doc: "驻留值：升热段时置的计数（_STAY_COUNT 口径）",
    },
    Spec {
        section: "buffer",
        key: "hot_criteria",
        effect: Effect::Restart,
        default: "2",
        doc: "热判据：冷段计数达此值才升热段",
    },
    Spec {
        section: "buffer",
        key: "max_scan_fraction",
        effect: Effect::Restart,
        default: "4",
        doc: "找空帧的前台扫描上限 = 容量/该值（db_block_max_scan_cnt 口径）",
    },
    Spec {
        section: "buffer",
        key: "make_free_batch_divisor",
        effect: Effect::Restart,
        default: "64",
        doc: "每轮写回批大小 = 分区帧数/该值",
    },
    Spec {
        section: "storage",
        key: "file_extend_blocks",
        effect: Effect::Restart,
        default: "512",
        doc: "段增长撞文件尾时的固定增量（块；512 = 8 MiB）",
    },
    Spec {
        section: "storage",
        key: "multiblock_read_pages",
        effect: Effect::Restart,
        default: "8",
        doc: "一次区读的连续页数（8 页 = 128 KiB；db_file_multiblock_read_count 口径）",
    },
    Spec {
        section: "storage",
        key: "cr_max_rounds",
        effect: Effect::Restart,
        default: "64",
        doc: "一致性读重建的回溯轮数上限（越限报「回溯轮数超限」）",
    },
    Spec {
        section: "wal",
        key: "log_buffer_pages",
        effect: Effect::Restart,
        default: "256",
        doc: "日志缓冲区页数（512 B/页；Oracle LOG_BUFFER 口径）",
    },
    Spec {
        section: "wal",
        key: "flush_trigger_permille",
        effect: Effect::Restart,
        default: "333",
        doc: "未刷出占比达此千分数即建议刷盘（333 ≈ 1/3）",
    },
    Spec {
        section: "catalog",
        key: "row_cache_rows",
        effect: Effect::Restart,
        default: "4096",
        doc: "字典行缓存：每型行数上限",
    },
    Spec {
        section: "catalog",
        key: "row_cache_bytes",
        effect: Effect::Restart,
        default: "4194304",
        doc: "字典行缓存：估算字节上限（4 MiB）",
    },
    Spec {
        section: "catalog",
        key: "rid_forward_max_hops",
        effect: Effect::Restart,
        default: "16",
        doc: "行迁移转发链的最大跳数（超限报「转发链过长」）",
    },
    Spec {
        section: "index",
        key: "bulk_fill_percent",
        effect: Effect::Restart,
        default: "90",
        doc: "建索引（批量灌树）叶页填充率（%）",
    },
    Spec {
        section: "service",
        key: "socket",
        effect: Effect::Restart,
        default: "bicdb.sock",
        doc: "控制套接字（相对根区目录；也可给绝对路径）",
    },
    Spec {
        section: "service",
        key: "log",
        effect: Effect::Restart,
        default: "bicdb.log",
        doc: "服务日志（同上）",
    },
    Spec {
        section: "service",
        key: "start_wait_s",
        effect: Effect::Restart,
        default: "30",
        doc: "`bicdb start` 等就绪的默认上限（秒；`-w` 覆盖）",
    },
    Spec {
        section: "service",
        key: "stop_wait_s",
        effect: Effect::Restart,
        default: "300",
        doc: "`bicdb stop` 等退出的默认上限（秒；完全检查点可能很慢）",
    },
    Spec {
        section: "service",
        key: "ready_poll_ms",
        effect: Effect::Restart,
        default: "100",
        doc: "等就绪的轮询间隔（毫秒）",
    },
    Spec {
        section: "service",
        key: "stop_poll_ms",
        effect: Effect::Restart,
        default: "50",
        doc: "等退出的轮询间隔（毫秒）",
    },
    Spec {
        section: "service",
        key: "probe_timeout_ms",
        effect: Effect::Restart,
        default: "500",
        doc: "每轮就绪探测的单次读写超时（毫秒）",
    },
    Spec {
        section: "service",
        key: "status_timeout_ms",
        effect: Effect::Restart,
        default: "5000",
        doc: "`bicdb status` 问服务自述的超时（毫秒）",
    },
    Spec {
        section: "service",
        key: "log_tail_lines",
        effect: Effect::Restart,
        default: "3",
        doc: "启动失败时回显的日志尾行数",
    },
    Spec {
        section: "client",
        key: "handshake_timeout_ms",
        effect: Effect::Restart,
        default: "5000",
        doc: "客户端握手超时（毫秒）——实例忙时据此报错而不是挂住",
    },
    Spec {
        section: "client",
        key: "request_timeout_ms",
        effect: Effect::Restart,
        default: "0",
        doc: "客户端请求超时（毫秒；0 = 不限：长查询是正常的）",
    },
    Spec {
        section: "auth",
        key: "pbkdf2_iterations",
        effect: Effect::Restart,
        default: "210000",
        doc: "口令散列的 PBKDF2-HMAC-SHA512 迭代数（写进散列串，调大不破旧行）",
    },
    Spec {
        section: "lock",
        key: "park_ms",
        effect: Effect::Restart,
        default: "50",
        doc: "等锁单次挂起时长（毫秒）",
    },
    Spec {
        section: "lock",
        key: "deadlock_threshold_ms",
        effect: Effect::Restart,
        default: "3000",
        doc: "死锁检测阈值（毫秒；等锁超过它才建图找环）",
    },
    Spec {
        section: "lock",
        key: "wait_max_rounds",
        effect: Effect::Restart,
        default: "0",
        doc: "等锁重试次数上限（0 = 不限；Oracle LOCK_TIMEOUT 的同位物）",
    },
];

impl InstanceParams {
    /// **合法节名**（闭集；由 [`SPECS`] 推出，顺序 = 首次出现序）。
    #[must_use]
    pub fn sections() -> Vec<&'static str> {
        let mut out: Vec<&'static str> = Vec::new();
        for s in SPECS {
            if !out.contains(&s.section) {
                out.push(s.section);
            }
        }
        out
    }

    /// 一个键的声明（`节.键`）。
    #[must_use]
    pub fn spec(section: &str, key: &str) -> Option<&'static Spec> {
        SPECS.iter().find(|s| s.section == section && s.key == key)
    }
}

impl InstanceParams {
    /// **解析 INI 文本**（`[节]` + `键 = 值`；`#`/`;` 起注释）。
    pub fn parse(text: &str) -> Result<Vec<(String, String, String, usize)>, ConfigError> {
        let mut out = Vec::new();
        let mut section = String::new();
        for (i, raw) in text.lines().enumerate() {
            let line = i + 1;
            let no_hash = raw.split_once('#').map_or(raw, |(c, _)| c);
            let code = no_hash.split_once(';').map_or(no_hash, |(c, _)| c).trim();
            if code.is_empty() {
                continue;
            }
            if let Some(rest) = code.strip_prefix('[') {
                let name = rest.strip_suffix(']').ok_or(ConfigError::Malformed {
                    line,
                    text: raw.trim().to_owned(),
                })?;
                section = name.trim().to_ascii_lowercase();
                // **节头那一行就校验**（诊断直接指到 `[nope]`，而不是它下面的键）。
                if !Self::sections().contains(&section.as_str()) {
                    return Err(ConfigError::UnknownSection { section, line });
                }
                continue;
            }
            let Some((k, v)) = code.split_once('=') else {
                return Err(ConfigError::Malformed {
                    line,
                    text: raw.trim().to_owned(),
                });
            };
            if section.is_empty() {
                return Err(ConfigError::Malformed {
                    line,
                    text: format!("`{}` 出现在任何节之前", raw.trim()),
                });
            }
            let v = v.trim();
            let v = v
                .strip_prefix('"')
                .and_then(|t| t.strip_suffix('"'))
                .unwrap_or(v);
            out.push((
                section.clone(),
                k.trim().to_ascii_lowercase(),
                v.to_owned(),
                line,
            ));
        }
        Ok(out)
    }

    /// 应用一条 `节.键 = 值`。
    fn set(
        &mut self,
        section: &str,
        key: &str,
        value: &str,
        line: usize,
    ) -> Result<(), ConfigError> {
        let bad = |why: &str| ConfigError::BadValue {
            key: format!("{section}.{key}"),
            value: value.to_owned(),
            why: why.to_owned(),
        };
        let num = |v: &str, lo: u64, hi: u64| -> Result<u64, ConfigError> {
            v.parse::<u64>()
                .ok()
                .filter(|n| (lo..=hi).contains(n))
                .ok_or_else(|| bad(&format!("要是 {lo}–{hi} 的整数")))
        };
        let text = |v: &str| -> Result<String, ConfigError> {
            if v.trim().is_empty() {
                Err(bad("不能为空"))
            } else {
                Ok(v.trim().to_owned())
            }
        };
        match (section, key) {
            ("instance", "db_root") => self.db_root = PathBuf::from(text(value)?),
            ("init", "file0_initial_blocks") => {
                self.init.file0_initial_blocks = num(value, 1024, 1_048_576)?
            }
            ("init", "undo_initial_blocks") => {
                self.init.undo_initial_blocks = num(value, 512, 1_048_576)?
            }
            ("init", "wal_groups") => self.init.wal_groups = num(value, 2, 8)? as u8,
            ("init", "wal_members") => self.init.wal_members = num(value, 1, 8)? as u8,
            ("init", "wal_group_pages") => {
                self.init.wal_group_pages = num(value, 34, 1_048_576)? as u32
            }
            ("buffer", "pool_frames") => self.run.pool_frames = num(value, 16, 1_000_000)? as usize,
            ("storage", "file_extend_blocks") => {
                self.run.file_extend_blocks = num(value, 8, 1_048_576)?
            }
            ("service", "socket") => self.run.socket = text(value)?,
            ("service", "log") => self.run.log = text(value)?,
            ("service", "start_wait_s") => self.run.start_wait_s = num(value, 1, 86_400)?,
            ("service", "stop_wait_s") => self.run.stop_wait_s = num(value, 1, 86_400)?,
            ("service", "ready_poll_ms") => self.run.ready_poll_ms = num(value, 1, 60_000)?,
            ("service", "stop_poll_ms") => self.run.stop_poll_ms = num(value, 1, 60_000)?,
            ("service", "probe_timeout_ms") => self.run.probe_timeout_ms = num(value, 1, 600_000)?,
            ("service", "status_timeout_ms") => {
                self.run.status_timeout_ms = num(value, 1, 600_000)?
            }
            ("service", "log_tail_lines") => self.run.log_tail_lines = num(value, 0, 1000)?,
            ("client", "handshake_timeout_ms") => {
                self.run.handshake_timeout_ms = num(value, 1, 600_000)?
            }
            // 0 = 不限（长查询是正常的；这是"服务卡住"的兜底）。
            ("client", "request_timeout_ms") => {
                self.run.request_timeout_ms = num(value, 0, 86_400_000)?
            }
            ("buffer", "hash_buckets") => {
                self.run.hash_buckets = num(value, 0, 1_000_000)? as usize;
                if self.run.hash_buckets == 1 {
                    return Err(bad("桶数要么 0（自动）要么 ≥ 7（哈希表要装得下）"));
                }
            }
            ("buffer", "bucket_latches") => {
                let n = num(value, 0, 64)? as usize;
                if n != 0 && !n.is_power_of_two() {
                    return Err(bad("桶闩锁数要是 2 的幂（0 = 自动）"));
                }
                self.run.bucket_latches = n;
            }
            ("buffer", "hot_fraction") => self.run.hot_fraction = num(value, 1, 1024)? as usize,
            ("buffer", "touch_interval_ms") => self.run.touch_interval_ms = num(value, 0, 600_000)?,
            ("buffer", "cool_count") => self.run.cool_count = num(value, 0, 1000)? as u32,
            ("buffer", "stay_count") => self.run.stay_count = num(value, 0, 1000)? as u32,
            ("buffer", "hot_criteria") => self.run.hot_criteria = num(value, 1, 1000)? as u32,
            ("buffer", "max_scan_fraction") => {
                self.run.max_scan_fraction = num(value, 1, 1024)? as usize
            }
            ("buffer", "make_free_batch_divisor") => {
                self.run.make_free_batch_divisor = num(value, 1, 1024)? as usize
            }
            ("storage", "multiblock_read_pages") => {
                self.run.multiblock_read_pages = num(value, 1, 64)? as u32
            }
            ("storage", "cr_max_rounds") => self.run.cr_max_rounds = num(value, 16, 4096)? as u32,
            ("wal", "log_buffer_pages") => {
                // 下限 36 页 = 最坏单条记录的 footprint（改了会破"任何一条记录
                // 必能落下"的不变式，见 `wal::buffer::MIN_CAPACITY_PAGES`）。
                self.run.log_buffer_pages = num(value, 36, 1_048_576)? as usize
            }
            ("wal", "flush_trigger_permille") => {
                self.run.flush_trigger_permille = num(value, 1, 1000)?
            }
            ("catalog", "row_cache_rows") => {
                self.run.row_cache_rows = num(value, 16, 1_000_000)? as usize
            }
            ("catalog", "row_cache_bytes") => {
                self.run.row_cache_bytes = num(value, 65_536, 1 << 40)?
            }
            ("catalog", "rid_forward_max_hops") => {
                self.run.rid_forward_max_hops = num(value, 1, 1024)? as usize
            }
            ("index", "bulk_fill_percent") => {
                self.run.bulk_fill_percent = num(value, 10, 100)? as u8
            }
            ("auth", "pbkdf2_iterations") => {
                self.run.pbkdf2_iterations = num(value, 1_000, 100_000_000)? as u32
            }
            ("lock", "park_ms") => self.run.park_ms = num(value, 1, 60_000)?,
            ("lock", "deadlock_threshold_ms") => {
                self.run.deadlock_threshold_ms = num(value, 10, 600_000)?
            }
            // 0 = 不限（Oracle 的默认也是无限等）；非 0 即等锁重试上限。
            ("lock", "wait_max_rounds") => self.run.wait_max_rounds = num(value, 0, 1_000_000)?,
            _ => {
                if !Self::sections().contains(&section) {
                    return Err(ConfigError::UnknownSection {
                        section: section.to_owned(),
                        line,
                    });
                }
                return Err(ConfigError::UnknownKey {
                    section: section.to_owned(),
                    key: key.to_owned(),
                    line,
                });
            }
        }
        Ok(())
    }

    /// **从文本装载**（`ini_path` 只作记录）。
    pub fn from_text(
        text: &str,
        ini_path: Option<PathBuf>,
    ) -> Result<(Self, ParamTable), ConfigError> {
        let mut p = Self {
            ini_path,
            ..Self::default()
        };
        let mut table: ParamTable = Vec::new();
        for (section, key, value, line) in Self::parse(text)? {
            p.set(&section, &key, &value, line)?;
            table.push((format!("{section}.{key}"), value, Source::File));
        }
        if p.db_root.as_os_str().is_empty() {
            return Err(ConfigError::Missing {
                key: "instance.db_root".to_owned(),
            });
        }
        Ok((p, table))
    }

    /// **按路径装载**（文件或"含 `bicdb.ini` 的目录"）。
    pub fn load_from(ini: &Path) -> Result<(Self, ParamTable), ConfigError> {
        let path = if ini.is_dir() {
            ini.join(FILE_NAME)
        } else {
            ini.to_path_buf()
        };
        let text = std::fs::read_to_string(&path).map_err(|why| ConfigError::Io {
            path: path.clone(),
            why,
        })?;
        Self::from_text(&text, Some(path))
    }

    /// **寻址**：`-p` > `$BICDB_INI` > `./bicdb.ini`；都没有 ⇒ [`ConfigError::NotFound`]。
    pub fn locate(explicit: Option<&Path>) -> Result<(Self, ParamTable), ConfigError> {
        let mut tried: Vec<PathBuf> = Vec::new();
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(p) = explicit {
            // **名字式兜底**：`-p public` 里 `public` 既不是文件也不是目录，但
            // `<BICDB_HOME>/public` 是个工作区 ⇒ 用它（Oracle 的 DB_NAME 直觉）。
            // 只作兜底：`-p` 的语义仍是"参数文件或根区目录"，名字式不改变权威链
            // （`db_root` 仍由参数文件注册）。
            if !p.exists() {
                if let Some(name) = p.to_str() {
                    if let Ok(home) = crate::home::Home::locate() {
                        if let Some(dir) = home.resolve_workspace_ref(name) {
                            candidates.push(dir);
                        }
                    }
                }
            }
            candidates.push(p.to_path_buf());
        }
        if let Ok(env) = std::env::var(ENV_INI) {
            if !env.trim().is_empty() {
                candidates.push(PathBuf::from(env));
            }
        }
        candidates.push(PathBuf::from(FILE_NAME));
        for c in candidates {
            let path = if c.is_dir() {
                c.join(FILE_NAME)
            } else {
                c.clone()
            };
            if path.exists() {
                return Self::load_from(&path);
            }
            tried.push(path);
        }
        Err(ConfigError::NotFound { tried })
    }

    /// **装载 + 命令行覆盖**（`-c 节.键=值`；也接受唯一键名）。
    pub fn load_with_overrides(
        ini: Option<&Path>,
        cli: &[(String, String)],
    ) -> Result<(Self, ParamTable), ConfigError> {
        let (mut p, mut table) = Self::locate(ini)?;
        for (k, v) in cli {
            let (section, key) = split_override(k)?;
            p.set(&section, &key, v, 0)?;
            let full = format!("{section}.{key}");
            table.retain(|(tk, _, _)| tk != &full);
            table.push((full, v.clone(), Source::Cli));
        }
        Ok((p, table))
    }

    /// **`bicdb init` 的参数**：`db_root` 由命令行给（init 的唯一入口），
    /// 建区期/运行期参数可来自**种子参数文件**或 `-c`。
    pub fn for_init(
        db_root: &Path,
        seed: Option<&Path>,
        cli: &[(String, String)],
    ) -> Result<Self, ConfigError> {
        let mut p = Self::default();
        if let Some(seed) = seed {
            let (s, _) = Self::load_from(seed)?;
            p.init = s.init; // 种子文件里的建区期参数生效
            p.run = s.run; // 运行期参数也随种子（并写进新生成的参数文件）
        }
        for (k, v) in cli {
            let (section, key) = split_override(k)?;
            p.set(&section, &key, v, 0)?;
        }
        p.db_root = db_root.to_path_buf(); // 命令行给的根区目录优先（唯一入口）
        Ok(p)
    }

    /// 控制套接字的绝对路径。
    #[must_use]
    pub fn socket_path(&self) -> PathBuf {
        resolve(&self.db_root, &self.run.socket)
    }

    /// 服务日志的绝对路径。
    #[must_use]
    pub fn log_path(&self) -> PathBuf {
        resolve(&self.db_root, &self.run.log)
    }

    /// 参数文件自身的路径。
    #[must_use]
    pub fn ini_path(&self) -> PathBuf {
        self.ini_path
            .clone()
            .unwrap_or_else(|| self.db_root.join(FILE_NAME))
    }

    /// **渲染为参数文件文本**（`init` 生成默认参数文件时用）。
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "\
# ============================================================
# bicdb 实例参数文件（`bicdb init` 生成）
#
# 形态：`[节]` + `键 = 值`；`#` 或 `;` 起注释；**未知节/未知键拒绝**（闭集）。
# 优先级：命令行 `-c 键=值` > 本文件 > 内置默认。
# 寻址：`-p <本文件|根区目录>` > 环境变量 {ENV_INI} > 当前目录的 {FILE_NAME}
#
# 两类参数（`bicdb params` 的「类别」列同此）：
#   [init]    建区期——`bicdb init` 读取；**建区后不可改**（控制文件权威，
#             改了 `bicdb start` 会逐项核对并拒绝启动，需重建实例）
#   其余节    运行期——改完**重启**生效（`bicdb restart`）
# ============================================================
"
        ));
        for section in Self::sections() {
            out.push_str(&format!("\n[{section}]\n"));
            for spec in SPECS.iter().filter(|sp| sp.section == section) {
                let value = self.value_of(section, spec.key).unwrap_or_default();
                out.push_str(&format!("{:<22}= {}   # {}\n", spec.key, value, spec.doc));
            }
        }
        out
    }

    /// **建区期参数的当前值**（`bicdb params` 用；它们"当前是什么"由参数文件
    /// 里的记录给出——建区后控制文件才是权威，见 `check_creation`）。
    #[must_use]
    pub fn init_fact(&self, key: &str) -> (String, Source) {
        let v = match key {
            "file0_initial_blocks" => self.init.file0_initial_blocks,
            "undo_initial_blocks" => self.init.undo_initial_blocks,
            "wal_groups" => u64::from(self.init.wal_groups),
            "wal_members" => u64::from(self.init.wal_members),
            "wal_group_pages" => u64::from(self.init.wal_group_pages),
            _ => 0,
        };
        (v.to_string(), Source::File)
    }

    /// **取值文本**（渲染参数文件与 `bicdb params` 共用这一份）。
    ///
    /// 找不到的键 ⇒ `None`（`SPECS` 里声明了却在这里漏了臂，会被渲染测试抓住）。
    #[must_use]
    pub fn value_of(&self, section: &str, key: &str) -> Option<String> {
        let v = match (section, key) {
            ("instance", "db_root") => self.db_root.display().to_string(),
            ("init", "file0_initial_blocks") => self.init.file0_initial_blocks.to_string(),
            ("init", "undo_initial_blocks") => self.init.undo_initial_blocks.to_string(),
            ("init", "wal_groups") => self.init.wal_groups.to_string(),
            ("init", "wal_members") => self.init.wal_members.to_string(),
            ("init", "wal_group_pages") => self.init.wal_group_pages.to_string(),
            ("buffer", "pool_frames") => self.run.pool_frames.to_string(),
            ("buffer", "hash_buckets") => self.run.hash_buckets.to_string(),
            ("buffer", "bucket_latches") => self.run.bucket_latches.to_string(),
            ("buffer", "hot_fraction") => self.run.hot_fraction.to_string(),
            ("buffer", "touch_interval_ms") => self.run.touch_interval_ms.to_string(),
            ("buffer", "cool_count") => self.run.cool_count.to_string(),
            ("buffer", "stay_count") => self.run.stay_count.to_string(),
            ("buffer", "hot_criteria") => self.run.hot_criteria.to_string(),
            ("buffer", "max_scan_fraction") => self.run.max_scan_fraction.to_string(),
            ("buffer", "make_free_batch_divisor") => self.run.make_free_batch_divisor.to_string(),
            ("storage", "file_extend_blocks") => self.run.file_extend_blocks.to_string(),
            ("storage", "multiblock_read_pages") => self.run.multiblock_read_pages.to_string(),
            ("storage", "cr_max_rounds") => self.run.cr_max_rounds.to_string(),
            ("wal", "log_buffer_pages") => self.run.log_buffer_pages.to_string(),
            ("wal", "flush_trigger_permille") => self.run.flush_trigger_permille.to_string(),
            ("catalog", "row_cache_rows") => self.run.row_cache_rows.to_string(),
            ("catalog", "row_cache_bytes") => self.run.row_cache_bytes.to_string(),
            ("catalog", "rid_forward_max_hops") => self.run.rid_forward_max_hops.to_string(),
            ("index", "bulk_fill_percent") => self.run.bulk_fill_percent.to_string(),
            ("service", "socket") => self.run.socket.clone(),
            ("service", "log") => self.run.log.clone(),
            ("service", "start_wait_s") => self.run.start_wait_s.to_string(),
            ("service", "stop_wait_s") => self.run.stop_wait_s.to_string(),
            ("service", "ready_poll_ms") => self.run.ready_poll_ms.to_string(),
            ("service", "stop_poll_ms") => self.run.stop_poll_ms.to_string(),
            ("service", "probe_timeout_ms") => self.run.probe_timeout_ms.to_string(),
            ("service", "status_timeout_ms") => self.run.status_timeout_ms.to_string(),
            ("service", "log_tail_lines") => self.run.log_tail_lines.to_string(),
            ("client", "handshake_timeout_ms") => self.run.handshake_timeout_ms.to_string(),
            ("client", "request_timeout_ms") => self.run.request_timeout_ms.to_string(),
            ("auth", "pbkdf2_iterations") => self.run.pbkdf2_iterations.to_string(),
            ("lock", "park_ms") => self.run.park_ms.to_string(),
            ("lock", "deadlock_threshold_ms") => self.run.deadlock_threshold_ms.to_string(),
            ("lock", "wait_max_rounds") => self.run.wait_max_rounds.to_string(),
            _ => return None,
        };
        Some(v)
    }

    /// **核对建区期参数**（控制文件/文件事实为权威）：返回逐项不符。
    ///
    /// 只核对**库里真有的事实**（组数/成员数/组文件页数）；`*_initial_blocks`
    /// 是"建区时的输入"，之后由自动扩展接管——不核对（文件里注明）。
    #[must_use]
    pub fn check_creation(&self, actual: &ActualCreation) -> Option<ConfigError> {
        let mut diffs = Vec::new();
        if self.init.wal_groups != actual.wal_groups {
            diffs.push(format!(
                "wal_groups：参数文件 {}，控制文件 {}",
                self.init.wal_groups, actual.wal_groups
            ));
        }
        if self.init.wal_members != actual.wal_members {
            diffs.push(format!(
                "wal_members：参数文件 {}，控制文件 {}",
                self.init.wal_members, actual.wal_members
            ));
        }
        if let Some(pages) = actual.wal_group_pages {
            if self.init.wal_group_pages != pages {
                diffs.push(format!(
                    "wal_group_pages：参数文件 {}，日志文件 {}",
                    self.init.wal_group_pages, pages
                ));
            }
        }
        if diffs.is_empty() {
            None
        } else {
            Some(ConfigError::CreationMismatch { diffs })
        }
    }

    /// 全部可调项（`节`, `键`）。
    #[must_use]
    pub fn all_keys() -> Vec<(&'static str, &'static str)> {
        SPECS.iter().map(|s| (s.section, s.key)).collect()
    }

    /// **全部声明**（`bicdb params` 的清单与说明、渲染都读它）。
    #[must_use]
    pub fn specs() -> &'static [Spec] {
        SPECS
    }
}

/// **库里的建区事实**（核对用；打开路径读出来后填）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActualCreation {
    /// 控制文件里的组数。
    pub wal_groups: u8,
    /// 控制文件里的成员数。
    pub wal_members: u8,
    /// 日志成员文件的页数（读不出 ⇒ `None`，该项不核对）。
    pub wal_group_pages: Option<u32>,
}

/// `-c` 的键：`节.键`（全名）或**唯一键名**（如 `pool_frames`）。
fn split_override(k: &str) -> Result<(String, String), ConfigError> {
    if let Some((s, key)) = k.split_once('.') {
        return Ok((
            s.trim().to_ascii_lowercase(),
            key.trim().to_ascii_lowercase(),
        ));
    }
    let k = k.trim().to_ascii_lowercase();
    let hits: Vec<(&str, &str)> = InstanceParams::all_keys()
        .into_iter()
        .filter(|(_, key)| *key == k)
        .collect();
    match hits.as_slice() {
        [(s, key)] => Ok(((*s).to_owned(), (*key).to_owned())),
        [] => Err(ConfigError::UnknownKey {
            section: "?".to_owned(),
            key: k,
            line: 0,
        }),
        _ => Err(ConfigError::BadValue {
            key: k,
            value: String::new(),
            why: "该键名在多个节里都有——请写成 `节.键`".to_owned(),
        }),
    }
}

/// **命令行 `-c 键=值`**（可重复）。
pub fn parse_cli_overrides(args: &[String]) -> Result<Vec<(String, String)>, ConfigError> {
    let mut out = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "-c" || a == "--set" {
            let kv = it.next().ok_or(ConfigError::Malformed {
                line: 0,
                text: "-c 缺 `键=值`".to_owned(),
            })?;
            let Some((k, v)) = kv.split_once('=') else {
                return Err(ConfigError::Malformed {
                    line: 0,
                    text: format!("-c 要 `键=值`，给的是 `{kv}`"),
                });
            };
            out.push((k.trim().to_owned(), v.trim().to_owned()));
        }
    }
    Ok(out)
}

/// **命令行 `-p <参数文件|根区目录>`**。
#[must_use]
pub fn parse_ini_arg(args: &[String]) -> Option<PathBuf> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "-p" || a == "--ini" || a == "--params-file" {
            return it.next().map(PathBuf::from);
        }
    }
    None
}

fn resolve(dir: &Path, name: &str) -> PathBuf {
    let p = Path::new(name);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        dir.join(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bicdb-ini-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("建目录");
        d
    }

    const MIN: &str = "[instance]\ndb_root = /tmp/x\n";

    #[test]
    fn parse_handles_sections_comments_and_lines() {
        let text =
            "# 注释\n[instance]\ndb_root = /data  # 行尾注释\n\n[buffer]\npool_frames = 64\n";
        let got = InstanceParams::parse(text).expect("解析");
        assert_eq!(
            got,
            vec![
                (
                    "instance".to_owned(),
                    "db_root".to_owned(),
                    "/data".to_owned(),
                    3
                ),
                (
                    "buffer".to_owned(),
                    "pool_frames".to_owned(),
                    "64".to_owned(),
                    6
                ),
            ]
        );
        assert!(matches!(
            InstanceParams::parse("db_root = /x\n"),
            Err(ConfigError::Malformed { line: 1, .. })
        ));
        assert!(matches!(
            InstanceParams::parse("[instance]\ndb_root /x\n"),
            Err(ConfigError::Malformed { line: 2, .. })
        ));
    }

    #[test]
    fn unknown_section_and_key_are_named() {
        let e = InstanceParams::from_text("[nope]\ndb_root = /x\n", None).expect_err("应拒绝");
        assert!(
            matches!(e, ConfigError::UnknownSection { line: 1, .. }),
            "{e}"
        );
        let e = InstanceParams::from_text("[instance]\ndb_root = /x\n[buffer]\nnope = 1\n", None)
            .expect_err("应拒绝");
        assert!(matches!(e, ConfigError::UnknownKey { line: 4, .. }), "{e}");
    }

    #[test]
    fn db_root_is_required_and_registered() {
        let e =
            InstanceParams::from_text("[buffer]\npool_frames = 64\n", None).expect_err("应拒绝");
        assert!(matches!(e, ConfigError::Missing { .. }), "{e}");
        let (p, table) = InstanceParams::from_text(MIN, None).expect("装载");
        assert_eq!(p.db_root, PathBuf::from("/tmp/x"));
        assert!(table
            .iter()
            .any(|(k, v, _)| k == "instance.db_root" && v == "/tmp/x"));
    }

    #[test]
    fn render_round_trips_and_resolves_relative_paths() {
        let p = InstanceParams {
            db_root: PathBuf::from("/data/bicdb"),
            ..InstanceParams::default()
        };
        let text = p.render();
        let (back, _) = InstanceParams::from_text(&text, None).expect("回读");
        assert_eq!(back.db_root, p.db_root);
        assert_eq!(back.init, p.init, "建区期参数应往返一致");
        assert_eq!(back.run, p.run, "运行期参数应往返一致");
        assert_eq!(back.socket_path(), PathBuf::from("/data/bicdb/bicdb.sock"));
        assert_eq!(back.log_path(), PathBuf::from("/data/bicdb/bicdb.log"));
    }

    #[test]
    fn cli_overrides_win_and_accept_short_keys() {
        let d = dir("ovr");
        std::fs::write(
            d.join(FILE_NAME),
            format!("{MIN}[buffer]\npool_frames = 64\n"),
        )
        .expect("写");
        let (p, table) = InstanceParams::load_with_overrides(
            Some(&d),
            &[("pool_frames".to_owned(), "128".to_owned())],
        )
        .expect("装载");
        assert_eq!(p.run.pool_frames, 128);
        assert!(table
            .iter()
            .any(|(k, v, s)| k == "buffer.pool_frames" && v == "128" && *s == Source::Cli));
        let e =
            InstanceParams::load_with_overrides(Some(&d), &[("nope".to_owned(), "1".to_owned())])
                .expect_err("应拒绝");
        assert!(matches!(e, ConfigError::UnknownKey { .. }), "{e}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn creation_check_reports_mismatch() {
        let d = dir("chk");
        let p = InstanceParams {
            db_root: d.clone(),
            ..InstanceParams::default()
        };
        let actual = ActualCreation {
            wal_groups: 4,
            wal_members: 2,
            wal_group_pages: None,
        };
        let msg = p.check_creation(&actual).expect("应报不符").to_string();
        assert!(
            msg.contains("wal_groups") && msg.contains("wal_members"),
            "{msg}"
        );
        let ok = ActualCreation {
            wal_groups: p.init.wal_groups,
            wal_members: p.init.wal_members,
            wal_group_pages: None,
        };
        assert!(p.check_creation(&ok).is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn for_init_takes_seed_and_cli() {
        let d = dir("seed");
        let seed = d.join("seed.ini");
        std::fs::write(
            &seed,
            "[instance]\ndb_root = /ignored\n[init]\nwal_groups = 4\n",
        )
        .expect("写种子");
        let p = InstanceParams::for_init(
            &d,
            Some(&seed),
            &[("init.wal_members".to_owned(), "2".to_owned())],
        )
        .expect("装载");
        assert_eq!(
            p.db_root, d,
            "根区目录以命令行为准（种子里的 db_root 忽略）"
        );
        assert_eq!(p.init.wal_groups, 4, "种子里的建区期参数生效");
        assert_eq!(p.init.wal_members, 2, "命令行覆盖种子");
        // 寻址：`-p` 优先（把参数文件写出来，模拟 `init` 的产物）。
        std::fs::write(d.join(FILE_NAME), p.render()).expect("写");
        let (found, _) = InstanceParams::locate(Some(&d)).expect("按目录寻址");
        assert_eq!(found.db_root, d);
        assert_eq!(found.ini_path(), d.join(FILE_NAME));
        let _ = std::fs::remove_dir_all(&d);
    }
}
