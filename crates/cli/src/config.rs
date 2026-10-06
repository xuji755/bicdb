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
}

impl Default for RunParams {
    fn default() -> Self {
        Self {
            pool_frames: 256,
            file_extend_blocks: bicdb_storage::segment::DEFAULT_FILE_EXTEND_BLOCKS,
            socket: crate::lock::SOCKET_FILE.to_owned(),
            log: "bicdb.log".to_owned(),
            park_ms: 50,
            deadlock_threshold_ms: 1000,
        }
    }
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

/// 合法的节名（闭集）。
pub const SECTIONS: [&str; 6] = ["instance", "init", "buffer", "storage", "service", "lock"];

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
                if !SECTIONS.contains(&section.as_str()) {
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
            ("lock", "park_ms") => self.run.park_ms = num(value, 1, 60_000)?,
            ("lock", "deadlock_threshold_ms") => {
                self.run.deadlock_threshold_ms = num(value, 10, 600_000)?
            }
            _ => {
                if !SECTIONS.contains(&section) {
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
        format!(
            "\
# ============================================================
# bicdb 实例参数文件（`bicdb init` 生成）
#
# 形态：`[节]` + `键 = 值`；`#` 或 `;` 起注释；**未知节/未知键拒绝**（闭集）。
# 优先级：命令行 `-c 键=值` > 本文件 > 内置默认。
# 寻址：`-p <本文件|根区目录>` > 环境变量 {ENV_INI} > 当前目录的 {FILE_NAME}
#
# 两类参数：
#   [init]    建区期——`bicdb init` 读取；**建区后不可改**（控制文件权威，
#             改了 `bicdb start` 会逐项核对并拒绝启动，需重建实例）
#   其余节    运行期——改完**重启**生效（`bicdb restart`）
# ============================================================

[instance]
# 根区目录：实例的全部文件（字典/撤销/日志/控制文件）都在它下面。
# 本文件就在 <db_root>/{FILE_NAME}；实例在哪儿由本项**注册**（权威）。
db_root = {}

[init]
# 建区期参数（`bicdb init` 读；建区后只作记录，见上）
file0_initial_blocks = {}   # file 0（字典/数据）初始块数（16 KiB/块）
undo_initial_blocks  = {}   # 撤销文件初始块数
wal_groups           = {}   # 日志组数（2–8，至少 2 组轮转）
wal_members          = {}   # 每组成员数（1–8）
wal_group_pages      = {}   # 每组成员页数（512 B/页）

[buffer]
pool_frames = {}   # 缓冲池帧数（16 KiB/帧）

[storage]
file_extend_blocks = {}   # 段增长撞文件尾时的固定增量（块；512 = 8 MiB）

[service]
socket = {}   # 控制套接字（相对根区目录；也可给绝对路径）
log    = {}   # 服务日志（同上）

[lock]
park_ms               = {}   # 等锁单次挂起时长（毫秒）
deadlock_threshold_ms = {}   # 死锁检测阈值（毫秒）
",
            self.db_root.display(),
            self.init.file0_initial_blocks,
            self.init.undo_initial_blocks,
            self.init.wal_groups,
            self.init.wal_members,
            self.init.wal_group_pages,
            self.run.pool_frames,
            self.run.file_extend_blocks,
            self.run.socket,
            self.run.log,
            self.run.park_ms,
            self.run.deadlock_threshold_ms
        )
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
        vec![
            ("instance", "db_root"),
            ("init", "file0_initial_blocks"),
            ("init", "undo_initial_blocks"),
            ("init", "wal_groups"),
            ("init", "wal_members"),
            ("init", "wal_group_pages"),
            ("buffer", "pool_frames"),
            ("storage", "file_extend_blocks"),
            ("service", "socket"),
            ("service", "log"),
            ("lock", "park_ms"),
            ("lock", "deadlock_threshold_ms"),
        ]
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
