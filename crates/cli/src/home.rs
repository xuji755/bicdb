//! **`BICDB_HOME`**：一次部署 = 一个根（`doc/安装布局_v0.1.md`）。
//!
//! ```text
//! <BICDB_HOME>/
//! ├── app/           程序（只读、随版本走；升级 = 换这个目录）
//! │   ├── bin/       bicdb · bicdbcli · db_check · page_dump
//! │   ├── share/     doc/ · examples/
//! │   └── VERSION
//! ├── public/        **PUBLIC 工作区**（数据 → control/ · wal/ · data/）
//! ├── log/           数据库日志（按工作区命名：public.log）
//! └── backup/        默认的备份包落地处
//! ```
//!
//! **三条原则**：一个根；**不藏东西**（程序/数据/日志/备份各有唯一去处，
//! 运维不用到处找）；不成第二事实源（默认值在二进制里，`bicdb init` 现场生成）。
//!
//! **确定顺序**：环境变量 `BICDB_HOME` > **二进制位置推断**
//! （`<home>/app/bin/bicdb`——路径形状自证，开发树 `target/release/` 不满足）
//! > 未安装（报错指路，**不猜**）。

use std::path::{Path, PathBuf};

/// 环境变量名。
pub const ENV_HOME: &str = "BICDB_HOME";

/// 默认工作区名（V1.0 单工作区；`crates/workspace` 的 `PUBLIC_WORKSPACE` 同名）。
pub const PUBLIC: &str = "public";

/// 程序目录名。
pub const APP_DIR: &str = "app";

/// 日志目录名。
pub const LOG_DIR: &str = "log";

/// 备份目录名。
pub const BACKUP_DIR: &str = "backup";

/// **全局控制文件目录名**（实例级注册表；`doc/全局控制文件设计_v0.1.md` §3）。
///
/// **为什么在 home 根、在任何工作区之外**：读 `public` 要先知道 `public` 在哪
/// ——清单放进工作区就形成循环依赖；而且 `public` 坏了不该让整个实例失联。
pub const CONTROL_DIR: &str = "control";

/// 全局控制文件双副本名。
pub const GLOBAL_CTL_NAMES: [&str; 2] = ["control01.ctl", "control02.ctl"];

/// home 是怎么定出来的（`bicdb home` 要如实说出来）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeSource {
    /// 环境变量 `BICDB_HOME`。
    Env,
    /// 二进制位置推断（`<home>/app/bin/bicdb`）。
    Binary,
}

impl HomeSource {
    /// 展示名。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            HomeSource::Env => "环境变量 BICDB_HOME",
            HomeSource::Binary => "二进制位置（<home>/app/bin/bicdb）",
        }
    }
}

/// 解析失败（**只在这一层报**：调用方决定"没有 home 能不能继续"）。
#[derive(Debug)]
pub enum HomeError {
    /// 环境变量指到的路径不存在/不是目录。
    BadEnv {
        /// 变量里的值。
        path: PathBuf,
    },
    /// 未安装（无环境变量，二进制也不在 `<home>/app/bin/` 里）。
    NotInstalled {
        /// 当前可执行文件路径（诊断）。
        exe: PathBuf,
    },
}

impl std::fmt::Display for HomeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HomeError::BadEnv { path } => {
                write!(
                    f,
                    "{ENV_HOME} 指到 {}，但它不存在或不是目录",
                    path.display()
                )
            }
            HomeError::NotInstalled { exe } => write!(
                f,
                "未安装：没有 {ENV_HOME}，且 {} 不在安装形态里（<exe>/.. 不是 app/bin）\
                 ——装法见 scripts/install.sh；或显式给 `-p <参数文件|工作区目录>`",
                exe.display()
            ),
        }
    }
}

impl std::error::Error for HomeError {}

/// **一个部署根**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Home {
    /// 根目录。
    pub root: PathBuf,
    /// 怎么定出来的。
    pub source: HomeSource,
}

impl Home {
    /// **确定部署根**（顺序见模块文档）。
    ///
    /// # Errors
    /// 环境变量非法，或未安装。
    pub fn locate() -> Result<Self, HomeError> {
        if let Some(raw) = std::env::var_os(ENV_HOME) {
            if !raw.is_empty() {
                let path = PathBuf::from(raw);
                if !path.is_dir() {
                    return Err(HomeError::BadEnv { path });
                }
                return Ok(Self {
                    root: path,
                    source: HomeSource::Env,
                });
            }
        }
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("bicdb"));
        // 形态：`<home>/app/bin/bicdb`——`bin` 的父目录必须叫 `app`。
        if let Some(root) = home_of_exe(&exe) {
            return Ok(Self {
                root,
                source: HomeSource::Binary,
            });
        }
        Err(HomeError::NotInstalled { exe })
    }

    /// **`init` 专用**：与 [`Home::locate`] 同规则，但**环境变量指到不存在的目录时
    /// 现场建出来**——"初始化部署"这条命令要能创建自己的根（否则第一次用
    /// `export BICDB_HOME=<新目录>` 就得先手工 `mkdir`）。
    ///
    /// 其余命令仍走 [`Home::locate`]（指到不存在的目录是**错误**，不是"顺手建"）。
    ///
    /// # Errors
    /// 目录建不出来，或未安装。
    pub fn locate_for_init() -> Result<Self, HomeError> {
        if let Some(raw) = std::env::var_os(ENV_HOME) {
            if !raw.is_empty() {
                let path = PathBuf::from(raw);
                if !path.is_dir() {
                    std::fs::create_dir_all(&path)
                        .map_err(|_| HomeError::BadEnv { path: path.clone() })?;
                }
                return Ok(Self {
                    root: path,
                    source: HomeSource::Env,
                });
            }
        }
        Self::locate()
    }

    /// 程序目录（`<home>/app`）。
    #[must_use]
    pub fn app_dir(&self) -> PathBuf {
        self.root.join(APP_DIR)
    }

    /// 日志目录（`<home>/log`）。
    #[must_use]
    pub fn log_dir(&self) -> PathBuf {
        self.root.join(LOG_DIR)
    }

    /// 备份目录（`<home>/backup`）。
    #[must_use]
    pub fn backup_dir(&self) -> PathBuf {
        self.root.join(BACKUP_DIR)
    }

    /// 全局控制文件目录（`<home>/control`）。
    #[must_use]
    pub fn control_dir(&self) -> PathBuf {
        self.root.join(CONTROL_DIR)
    }

    /// 全局控制文件双副本路径（A、B）。
    #[must_use]
    pub fn global_ctl_paths(&self) -> [PathBuf; 2] {
        let dir = self.control_dir();
        [dir.join(GLOBAL_CTL_NAMES[0]), dir.join(GLOBAL_CTL_NAMES[1])]
    }

    /// 默认工作区根（`<home>/public`）。
    #[must_use]
    pub fn public_dir(&self) -> PathBuf {
        self.root.join(PUBLIC)
    }

    /// 工作区日志文件（`<home>/log/<工作区名>.log`）——**所有日志在一个目录里**。
    #[must_use]
    pub fn workspace_log(&self, name: &str) -> PathBuf {
        self.log_dir().join(format!("{name}.log"))
    }

    /// 工作区根（名字 ⇒ `<home>/<名字>`）。
    ///
    /// **判据**：不含路径分隔符、不是保留目录名（`app`/`log`/`backup`）就是名字；
    /// 含 `/` 的一律当路径，由调用方原样使用——安装形态**不约束**显式路径。
    #[must_use]
    pub fn workspace_dir(&self, name_or_path: &str) -> Option<PathBuf> {
        if name_or_path.is_empty() || name_or_path.contains('/') {
            return None;
        }
        if matches!(name_or_path, APP_DIR | LOG_DIR | BACKUP_DIR | CONTROL_DIR) {
            return None;
        }
        Some(self.root.join(name_or_path))
    }

    /// 安装记录里的版本（`<home>/app/VERSION` 的第一行 `version: `）。
    #[must_use]
    pub fn version(&self) -> Option<String> {
        let text = std::fs::read_to_string(self.app_dir().join("VERSION")).ok()?;
        text.lines()
            .find_map(|l| l.strip_prefix("version: ").map(str::to_owned))
    }

    /// `-p` 的**兜底解析**：既不是文件也不是目录，但是个已存在的工作区名字。
    #[must_use]
    pub fn resolve_workspace_ref(&self, given: &str) -> Option<PathBuf> {
        let dir = self.workspace_dir(given)?;
        dir.join(crate::config::FILE_NAME).is_file().then_some(dir)
    }

    /// **列出根下的工作区**（名字 + 根区目录），按名字排序。
    ///
    /// 判据：一层子目录里有 `bicdb.ini`；`app/`、`log/`、`backup/` 恒不在内。
    ///
    /// # Errors
    /// 根目录读不了。
    pub fn workspaces(&self) -> std::io::Result<Vec<(String, PathBuf)>> {
        let mut out = Vec::new();
        let entries = std::fs::read_dir(&self.root)?;
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if matches!(name.as_str(), APP_DIR | LOG_DIR | BACKUP_DIR | CONTROL_DIR) {
                continue;
            }
            if !path.is_dir() || !path.join(crate::config::FILE_NAME).is_file() {
                continue;
            }
            out.push((name, path));
        }
        out.sort();
        Ok(out)
    }
}

/// 由二进制位置推断根：`<home>/app/bin/bicdb`。
fn home_of_exe(exe: &Path) -> Option<PathBuf> {
    let bin = exe.parent()?;
    if bin.file_name()? != "bin" {
        return None;
    }
    let app = bin.parent()?;
    if app.file_name()? != APP_DIR {
        return None;
    }
    let root = app.parent()?;
    // 结构自证：根下确有 app/（挡住"碰巧同名"的路径）。
    root.join(APP_DIR).is_dir().then(|| root.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bicdb-home-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建目录");
        dir
    }

    /// 摆一个安装形态：`<home>/app/bin/bicdb`。
    fn plant(root: &Path) -> PathBuf {
        let bin = root.join(APP_DIR).join("bin");
        std::fs::create_dir_all(&bin).expect("建 bin");
        let exe = bin.join("bicdb");
        std::fs::write(&exe, b"").expect("占位二进制");
        exe
    }

    #[test]
    fn the_app_bin_shape_is_recognized() {
        let root = temp("shape");
        let exe = plant(&root);
        assert_eq!(home_of_exe(&exe), Some(root.clone()));
        let h = Home {
            root: root.clone(),
            source: HomeSource::Binary,
        };
        assert_eq!(h.app_dir(), root.join("app"));
        assert_eq!(h.public_dir(), root.join("public"));
        assert_eq!(h.log_dir(), root.join("log"));
        assert_eq!(h.backup_dir(), root.join("backup"));
        assert_eq!(h.workspace_log("public"), root.join("log/public.log"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_development_tree_is_not_an_install() {
        let root = temp("dev");
        let dev = root.join("target/release");
        std::fs::create_dir_all(&dev).expect("建 target/release");
        let exe = dev.join("bicdb");
        std::fs::write(&exe, b"").expect("占位");
        assert_eq!(home_of_exe(&exe), None);
        // 平铺的 <root>/bin/bicdb 也不是（父目录得叫 app）。
        let flat = root.join("bin");
        std::fs::create_dir_all(&flat).expect("建 bin");
        let f = flat.join("bicdb");
        std::fs::write(&f, b"").expect("占位");
        assert_eq!(home_of_exe(&f), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn reserved_dirs_are_not_workspace_names() {
        let root = temp("reserved");
        let h = Home {
            root: root.clone(),
            source: HomeSource::Env,
        };
        assert_eq!(h.workspace_dir("public"), Some(root.join("public")));
        assert_eq!(h.workspace_dir("shop"), Some(root.join("shop")));
        assert_eq!(h.workspace_dir("app"), None);
        assert_eq!(h.workspace_dir("log"), None);
        assert_eq!(h.workspace_dir("backup"), None);
        assert_eq!(h.workspace_dir("control"), None);
        assert_eq!(h.workspace_dir("/srv/x"), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn listing_skips_reserved_dirs_and_p_falls_back_only_for_real_ones() {
        let root = temp("listing");
        let h = Home {
            root: root.clone(),
            source: HomeSource::Env,
        };
        for d in [APP_DIR, LOG_DIR, BACKUP_DIR] {
            std::fs::create_dir_all(root.join(d)).expect("建保留目录");
        }
        assert_eq!(h.resolve_workspace_ref("public"), None);
        let ws = h.public_dir();
        std::fs::create_dir_all(&ws).expect("建工作区");
        std::fs::write(ws.join(crate::config::FILE_NAME), "x").expect("写参数文件");
        assert_eq!(h.resolve_workspace_ref("public"), Some(ws.clone()));
        assert_eq!(
            h.workspaces().expect("列工作区"),
            vec![("public".to_owned(), ws)]
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
