//! **客户端侧的实例寻址**（找到控制套接字；与服务端的口径一致）。
//!
//! ```text
//! `-p <参数文件|根区目录>` > 环境变量 BICDB_INI > 当前目录的 ./bicdb.ini
//! ```
//!
//! **只读两项**：`[instance] db_root` 与 `[service] socket`（相对项按 db_root 解）。
//! **权威解析在服务端**（`bicdb-cli::config`，含闭集/类型/建区期核对）——本模块是
//! 客户端能自洽的最小读法，客户端**不需要**、也不应该复制服务端的全部校验。

use std::path::{Path, PathBuf};

/// 参数文件名（与 `bicdb-cli::config::FILE_NAME` 同源；改这里要两边一起改）。
pub const FILE_NAME: &str = "bicdb.ini";

/// 环境变量（与服务端一致）。
pub const ENV_INI: &str = "BICDB_INI";

/// 寻址错误。
#[derive(Debug)]
pub enum DiscoverError {
    /// 三个来源都没有参数文件。
    NotFound {
        /// 试过的位置。
        tried: Vec<PathBuf>,
    },
    /// 文件读不了 / 缺 `db_root`。
    Bad {
        /// 路径。
        path: PathBuf,
        /// 原因。
        why: String,
    },
}

impl std::fmt::Display for DiscoverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DiscoverError::NotFound { tried } => {
                write!(f, "找不到参数文件 {FILE_NAME}——试过：")?;
                for t in tried {
                    write!(f, "{}；", t.display())?;
                }
                write!(f, "（用 `-p <参数文件|根区目录>` 或环境变量 {ENV_INI}）")
            }
            DiscoverError::Bad { path, why } => write!(f, "读 {}：{why}", path.display()),
        }
    }
}

impl std::error::Error for DiscoverError {}

/// **找控制套接字**：`explicit` 给文件或目录，否则按环境变量/当前目录。
pub fn socket_for(explicit: Option<&Path>) -> Result<PathBuf, DiscoverError> {
    let (root, socket) = instance_for(explicit)?;
    Ok(resolve(&root, &socket))
}

/// **读实例参数**：返回 `(db_root, socket 项)`。
pub fn instance_for(explicit: Option<&Path>) -> Result<(PathBuf, String), DiscoverError> {
    let mut tried = Vec::new();
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
        if !path.exists() {
            tried.push(path);
            continue;
        }
        let text = std::fs::read_to_string(&path).map_err(|e| DiscoverError::Bad {
            path: path.clone(),
            why: e.to_string(),
        })?;
        return parse(&text, &path);
    }
    Err(DiscoverError::NotFound { tried })
}

/// 解析（**容忍未知节/键**——服务端会严格校验；客户端只挑自己认识的两项）。
fn parse(text: &str, path: &Path) -> Result<(PathBuf, String), DiscoverError> {
    let mut section = String::new();
    let mut root: Option<PathBuf> = None;
    let mut socket = "bicdb.sock".to_owned();
    for raw in text.lines() {
        let no_hash = raw.split_once('#').map_or(raw, |(c, _)| c);
        let code = no_hash.split_once(';').map_or(no_hash, |(c, _)| c).trim();
        if code.is_empty() {
            continue;
        }
        if let Some(rest) = code.strip_prefix('[') {
            section = rest.trim_end_matches(']').trim().to_ascii_lowercase();
            continue;
        }
        let Some((k, v)) = code.split_once('=') else {
            continue;
        };
        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim().trim_matches('"'));
        match (section.as_str(), k.as_str()) {
            ("instance", "db_root") => root = Some(PathBuf::from(v)),
            ("service", "socket") => socket = v.to_owned(),
            _ => {}
        }
    }
    let root = root.ok_or_else(|| DiscoverError::Bad {
        path: path.to_path_buf(),
        why: "缺 `[instance] db_root`".to_owned(),
    })?;
    Ok((root, socket))
}

fn resolve(dir: &Path, name: &str) -> PathBuf {
    let p = Path::new(name);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        dir.join(p)
    }
}
