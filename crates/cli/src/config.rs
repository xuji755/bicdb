//! **实例参数文件**（`<实例目录>/bicdb.conf`；PG `postgresql.conf` 的对应物）。
//!
//! ```text
//! # bicdb 实例参数（`bicdb init` 写出；改完重启服务生效）
//! pool_frames          = 256    # 缓冲池帧数（16 KiB/帧）
//! file_extend_blocks   = 512    # 段增长撞文件尾时的固定增量（块）
//! socket               = bicdb.sock
//! log                  = bicdb.log
//! lock_park_ms         = 50     # 等锁单次挂起时长
//! deadlock_threshold_ms = 1000  # 死锁检测阈值
//! ```
//!
//! **三条纪律**：
//! 1. **闭集**：未知键**拒绝**（与解析期拒绝同一个口径——收下不生效就是空壳，
//!    本项目 2026-10-06 的审计专门清过这一类）；
//! 2. **优先级 = 命令行 > 参数文件 > 内置默认**（`bicdb start -c pool_frames=512`
//!    覆盖文件里的值——PG 的 `postgres -c` 同款）；
//! 3. **建区期参数只读**：日志组数/成员数/组页数/文件初始块数记录在**控制文件**
//!    里（权威），参数文件里写它们**无效**——`bicdb params` 单列一节展示。
//!
//! **取法对照**：Oracle 的 `spfile<SID>.ora`（服务端可写、`ALTER SYSTEM SET` 落它）
//! 与 `pfile`（手工编辑、重启生效）二分；我们 V1.0 只做**文本文件 + 重启生效**这一半，
//! 在线写回（`ALTER SYSTEM SET`）按 `DCL语句设计` §5 的触发条件延后。

use std::path::{Path, PathBuf};

/// 参数文件名（实例目录下）。
pub const FILE_NAME: &str = "bicdb.conf";

/// 参数错误的来源（诊断要能指到"哪个文件的哪一行"）。
#[derive(Debug)]
pub enum ConfigError {
    /// 读文件失败。
    Io {
        /// 路径。
        path: PathBuf,
        /// 底层错误。
        why: std::io::Error,
    },
    /// **未知键**（闭集）。
    Unknown {
        /// 键名。
        key: String,
        /// 行号（1 起）。
        line: usize,
    },
    /// 取值非法。
    BadValue {
        /// 键名。
        key: String,
        /// 取值文本。
        value: String,
        /// 为什么不行。
        why: String,
    },
    /// 行形态非法（如缺 `=`）。
    Malformed {
        /// 行号。
        line: usize,
        /// 原文。
        text: String,
    },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Io { path, why } => write!(f, "读 {}：{why}", path.display()),
            ConfigError::Unknown { key, line } => write!(
                f,
                "{} 第 {line} 行：未知参数 `{key}`（闭集；`bicdb params` 看全部可调项）",
                FILE_NAME
            ),
            ConfigError::BadValue { key, value, why } => {
                write!(f, "{FILE_NAME}：`{key} = {value}` 不合法——{why}")
            }
            ConfigError::Malformed { line, text } => write!(
                f,
                "{FILE_NAME} 第 {line} 行形态非法（要 `键 = 值`）：`{text}`"
            ),
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

/// **运行期可调参数**（每一项都必须有真实落点——审计口径）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceParams {
    /// 缓冲池帧数（16 KiB/帧）。
    pub pool_frames: usize,
    /// 段增长撞文件尾时的固定增量（块）。
    pub file_extend_blocks: u64,
    /// 控制套接字（相对实例目录；也可给绝对路径）。
    pub socket: String,
    /// 服务日志（相对实例目录；也可给绝对路径）。
    pub log: String,
    /// 等锁单次挂起时长（毫秒）。
    pub lock_park_ms: u64,
    /// 死锁检测阈值（毫秒）。
    pub deadlock_threshold_ms: u64,
}

impl Default for InstanceParams {
    fn default() -> Self {
        Self {
            pool_frames: 256,
            file_extend_blocks: crate::boot::DEFAULT_FILE_EXTEND_BLOCKS,
            socket: crate::lock::SOCKET_FILE.to_owned(),
            log: "bicdb.log".to_owned(),
            lock_park_ms: 50,
            deadlock_threshold_ms: 1000,
        }
    }
}

/// 参数清单（键 → 取值 → 来源）。
pub type ParamTable = Vec<(&'static str, String, Source)>;

impl InstanceParams {
    /// **应用一条覆盖**（文件名或命令行同一条路）。
    fn set(&mut self, key: &str, value: &str, line: usize) -> Result<(), ConfigError> {
        let bad = |why: &str| ConfigError::BadValue {
            key: key.to_owned(),
            value: value.to_owned(),
            why: why.to_owned(),
        };
        let num = |v: &str, lo: u64, hi: u64| -> Result<u64, ConfigError> {
            v.parse::<u64>()
                .ok()
                .filter(|n| (lo..=hi).contains(n))
                .ok_or_else(|| bad(&format!("要是 {lo}–{hi} 的整数")))
        };
        match key {
            "pool_frames" => self.pool_frames = num(value, 16, 1_000_000)? as usize,
            "file_extend_blocks" => self.file_extend_blocks = num(value, 8, 1_048_576)?,
            "socket" => {
                if value.trim().is_empty() {
                    return Err(bad("不能为空"));
                }
                self.socket = value.trim().to_owned();
            }
            "log" => {
                if value.trim().is_empty() {
                    return Err(bad("不能为空"));
                }
                self.log = value.trim().to_owned();
            }
            "lock_park_ms" => self.lock_park_ms = num(value, 1, 60_000)?,
            "deadlock_threshold_ms" => self.deadlock_threshold_ms = num(value, 10, 600_000)?,
            other => {
                return Err(ConfigError::Unknown {
                    key: other.to_owned(),
                    line,
                })
            }
        }
        Ok(())
    }

    /// 控制套接字的**绝对路径**（相对项按实例目录解）。
    #[must_use]
    pub fn socket_path(&self, dir: &Path) -> PathBuf {
        resolve(dir, &self.socket)
    }

    /// 服务日志的**绝对路径**。
    #[must_use]
    pub fn log_path(&self, dir: &Path) -> PathBuf {
        resolve(dir, &self.log)
    }

    /// **解析参数文件文本**（`键 = 值`；`#` 起注释）。带行号（诊断要指得到）。
    pub fn parse(text: &str) -> Result<Vec<(String, String, usize)>, ConfigError> {
        let mut out = Vec::new();
        for (i, raw) in text.lines().enumerate() {
            let line = i + 1;
            let code = raw.split_once('#').map_or(raw, |(c, _)| c).trim();
            if code.is_empty() {
                continue;
            }
            let Some((k, v)) = code.split_once('=') else {
                return Err(ConfigError::Malformed {
                    line,
                    text: raw.trim().to_owned(),
                });
            };
            let v = v.trim();
            // 去掉可能包裹的引号（路径里有空格时用得上）。
            let v = v
                .strip_prefix('"')
                .and_then(|t| t.strip_suffix('"'))
                .unwrap_or(v);
            out.push((k.trim().to_ascii_lowercase(), v.to_owned(), line));
        }
        Ok(out)
    }

    /// **装载**：默认 → 文件（若在）→ 命令行覆盖。返回参数表（含来源）。
    pub fn load(dir: &Path, cli: &[(String, String)]) -> Result<Self, ConfigError> {
        let (params, _) = Self::load_with_table(dir, cli)?;
        Ok(params)
    }

    /// 同上，另给一份"键 + 取值 + 来源"的表（`bicdb params` 用）。
    pub fn load_with_table(
        dir: &Path,
        cli: &[(String, String)],
    ) -> Result<(Self, ParamTable), ConfigError> {
        let mut p = Self::default();
        let mut table: ParamTable = Vec::new();
        let mark = |table: &mut ParamTable, key: &str, v: &str, src: Source| {
            table.push((
                KEYS.iter().find(|k| **k == key).copied().unwrap_or("?"),
                v.to_owned(),
                src,
            ));
        };
        for (k, v) in cli {
            table.push((
                KEYS.iter().find(|kk| **kk == k).copied().unwrap_or("?"),
                v.clone(),
                Source::Cli,
            ));
        }
        let path = dir.join(FILE_NAME);
        if path.exists() {
            let text = std::fs::read_to_string(&path).map_err(|why| ConfigError::Io {
                path: path.clone(),
                why,
            })?;
            for (k, v, line) in Self::parse(&text)? {
                p.set(&k, &v, line)?;
                mark(&mut table, &k, &v, Source::File);
            }
        }
        for (k, v) in cli {
            p.set(k, v, 0)?;
        }
        Ok((p, table))
    }

    /// **`bicdb init` 写出的默认参数文件文本**（带注释；改完重启生效）。
    #[must_use]
    pub fn default_file_text() -> String {
        let d = Self::default();
        format!(
            "\
# bicdb 实例参数文件（`bicdb init` 写出）
#
# 形态：`键 = 值`；`#` 起注释；**未知键拒绝**（闭集纪律）。
# 优先级：命令行 `-c 键=值` > 本文件 > 内置默认。
# 生效时机：**改完重启服务**（在线写回 `ALTER SYSTEM SET` 随后续切片）。
#
# 建区期参数（日志组数/成员数/组页数/文件初始块数）由**控制文件**记录，
# 本文件写它们无效——`bicdb params` 会单列一节展示。

pool_frames           = {}   # 缓冲池帧数（16 KiB/帧）
file_extend_blocks    = {}   # 段增长撞文件尾时的固定增量（块；512 块 = 8 MiB）
socket                = {}   # 控制套接字（相对实例目录）
log                   = {}   # 服务日志（相对实例目录）
lock_park_ms          = {}   # 等锁单次挂起时长（毫秒）
deadlock_threshold_ms = {}   # 死锁检测阈值（毫秒）
",
            d.pool_frames,
            d.file_extend_blocks,
            d.socket,
            d.log,
            d.lock_park_ms,
            d.deadlock_threshold_ms
        )
    }

    /// 可调键清单（`-c` 的闭集与 `params` 输出共用）。
    pub const KEYS: [&'static str; 6] = [
        "pool_frames",
        "file_extend_blocks",
        "socket",
        "log",
        "lock_park_ms",
        "deadlock_threshold_ms",
    ];
}

/// `KEYS` 的别名（上面的闭集）。
const KEYS: [&str; 6] = InstanceParams::KEYS;

fn resolve(dir: &Path, name: &str) -> PathBuf {
    let p = Path::new(name);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        dir.join(p)
    }
}

/// **命令行 `-c 键=值`** 的解析（可重复）。
pub fn parse_cli_overrides(args: &[String]) -> Result<Vec<(String, String)>, ConfigError> {
    let mut out = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "-c" || a == "--param" {
            let kv = it.next().ok_or_else(|| ConfigError::Malformed {
                line: 0,
                text: "-c 缺 `键=值`".to_owned(),
            })?;
            let Some((k, v)) = kv.split_once('=') else {
                return Err(ConfigError::Malformed {
                    line: 0,
                    text: format!("-c 要 `键=值`，给的是 `{kv}`"),
                });
            };
            out.push((k.trim().to_ascii_lowercase(), v.trim().to_owned()));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bicdb-conf-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("建目录");
        d
    }

    #[test]
    fn parse_ignores_comments_and_reports_lines() {
        let text = "# 注释\n\npool_frames = 64   # 行尾注释\n";
        let got = InstanceParams::parse(text).expect("解析");
        assert_eq!(got, vec![("pool_frames".to_owned(), "64".to_owned(), 3)]);
        // 缺 `=` 要指到行号。
        let err = InstanceParams::parse("pool_frames 64\n").expect_err("应拒绝");
        assert!(
            matches!(err, ConfigError::Malformed { line: 1, .. }),
            "{err}"
        );
    }

    #[test]
    fn precedence_is_cli_over_file_over_default() {
        let d = dir("prec");
        std::fs::write(d.join(FILE_NAME), "pool_frames = 64\nlock_park_ms = 7\n").expect("写文件");
        // 文件生效。
        let p = InstanceParams::load(&d, &[]).expect("装载");
        assert_eq!(p.pool_frames, 64);
        assert_eq!(p.lock_park_ms, 7);
        // 命令行覆盖文件。
        let p = InstanceParams::load(&d, &[("pool_frames".to_owned(), "128".to_owned())])
            .expect("装载");
        assert_eq!(p.pool_frames, 128);
        assert_eq!(p.lock_park_ms, 7);
        // 文件里没有的键 = 默认。
        assert_eq!(p.deadlock_threshold_ms, 1000);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn unknown_key_and_bad_value_are_named() {
        let d = dir("bad");
        std::fs::write(d.join(FILE_NAME), "没这个键 = 1\n").expect("写");
        let err = InstanceParams::load(&d, &[]).expect_err("应拒绝");
        assert!(matches!(err, ConfigError::Unknown { line: 1, .. }), "{err}");
        std::fs::write(d.join(FILE_NAME), "pool_frames = 1\n").expect("写");
        let err = InstanceParams::load(&d, &[]).expect_err("应拒绝");
        assert!(matches!(err, ConfigError::BadValue { .. }), "{err}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn default_file_text_is_loadable_and_socket_resolves() {
        let d = dir("default");
        std::fs::write(d.join(FILE_NAME), InstanceParams::default_file_text()).expect("写");
        let p = InstanceParams::load(&d, &[]).expect("默认文件应可装载");
        assert_eq!(p, InstanceParams::default());
        assert_eq!(p.socket_path(&d), d.join("bicdb.sock"));
        assert_eq!(p.log_path(&d), d.join("bicdb.log"));
        // 绝对路径原样用。
        let abs = InstanceParams {
            socket: "/tmp/abs.sock".to_owned(),
            ..InstanceParams::default()
        };
        assert_eq!(abs.socket_path(&d), PathBuf::from("/tmp/abs.sock"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn cli_overrides_parse() {
        let args: Vec<String> = [
            "-d",
            "/x",
            "-c",
            "pool_frames=32",
            "-c",
            "socket=/tmp/s.sock",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        let ov = parse_cli_overrides(&args).expect("解析");
        assert_eq!(
            ov,
            vec![
                ("pool_frames".to_owned(), "32".to_owned()),
                ("socket".to_owned(), "/tmp/s.sock".to_owned())
            ]
        );
    }
}
