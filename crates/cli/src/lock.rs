//! **实例独占锁**（单写者纪律的落点；PG `postmaster.pid` 形态）。
//!
//! ```text
//! <dir>/bicdb.pid   pid │ 进程启动时刻 │ 模式 │ 套接字路径 │ 版本
//! ```
//!
//! **为什么需要它**：V1.0 的写者是**每工作区一个**（缓冲池/undo/日志都是单写者
//! 语义）。服务在后台跑着的时候，另一个 `bicdb sql` 再把同一实例开起来，两边
//! 各写各的池与日志——不是"并发"，是**互相破坏**。所以打开路径必须先过这把锁。
//!
//! **怎么判"还活着"**（PG 的三件套简化版，纯 std、零 unsafe）：
//! 1. 读 pid 文件里的 `pid`；
//! 2. `/proc/<pid>` 不存在 ⇒ 进程没了 ⇒ **陈旧锁**，清掉重取；
//! 3. 存在 ⇒ 再比 `/proc/<pid>/stat` 的 `starttime`（自开机起的时钟滴答）——
//!    与记录值不同即 **pid 被复用**（旧进程死了、号被新进程占了）⇒ 同样算陈旧。
//!
//! **不改内核状态、不发信号**：锁是"文件 + 事实核对"，`stop` 走控制套接字
//! （见 `service.rs`）而不是信号——本仓库 `#![forbid(unsafe_code)]`，`kill(2)`
//! 用不了，也不需要用。

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// pid 文件名（实例目录下）。
pub const PID_FILE: &str = "bicdb.pid";
/// 控制套接字默认文件名（实例目录下）。
pub const SOCKET_FILE: &str = "bicdb.sock";

/// 锁的持有模式（写进 pid 文件，供诊断与 `status` 显示）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockMode {
    /// **服务模式**：后台守护进程持有，客户经控制套接字连。
    Service,
    /// **直连模式**：某个前台进程（`bicdb sql` / `bicdbcli`）自己开着实例。
    Direct,
}

impl LockMode {
    fn as_str(self) -> &'static str {
        match self {
            LockMode::Service => "service",
            LockMode::Direct => "direct",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "service" => Some(LockMode::Service),
            "direct" => Some(LockMode::Direct),
            _ => None,
        }
    }
}

/// pid 文件的内容（= 一把活锁的事实）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockInfo {
    /// 持有者 pid。
    pub pid: u32,
    /// 持有者进程的启动时刻（`/proc/<pid>/stat` 第 22 字段；防 pid 复用）。
    pub starttime: u64,
    /// 模式。
    pub mode: LockMode,
    /// 控制套接字路径（服务模式才有意义）。
    pub socket: PathBuf,
    /// 版本（诊断）。
    pub version: String,
}

/// 取锁失败的原因。
#[derive(Debug)]
pub enum LockError {
    /// **已被占用**（持有者活着）——打开路径据此拒绝。
    Occupied {
        /// 持有者 pid。
        pid: u32,
        /// 持有模式。
        mode: LockMode,
    },
    /// I/O。
    Io(std::io::Error),
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockError::Occupied { pid, mode } => write!(
                f,
                "实例已被占用（pid {pid}，{}）——另一个进程正开着它",
                match mode {
                    LockMode::Service => "服务模式",
                    LockMode::Direct => "直连模式",
                }
            ),
            LockError::Io(e) => write!(f, "实例锁 I/O：{e}"),
        }
    }
}

impl std::error::Error for LockError {}

impl From<std::io::Error> for LockError {
    fn from(e: std::io::Error) -> Self {
        LockError::Io(e)
    }
}

/// pid 文件路径。
#[must_use]
pub fn pid_path(dir: &Path) -> PathBuf {
    dir.join(PID_FILE)
}

/// 控制套接字路径（默认）。
#[must_use]
pub fn socket_path(dir: &Path) -> PathBuf {
    dir.join(SOCKET_FILE)
}

/// 进程启动时刻（`/proc/<pid>/stat` 第 22 字段 = starttime）；查不到 ⇒ `None`。
///
/// **为什么用它**：pid 会被复用——只比 pid 存在与否，会把"旧持有者已死、号被
/// 别人占了"误判成"锁还活着"，实例就再也开不起来了。
#[must_use]
pub fn proc_starttime(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // 格式：pid (comm) state ppid ... ；comm 里可能有空格与括号 ⇒ 从**最后一个 ')'** 之后切。
    let after = stat.rsplit_once(')')?.1;
    // after = " R 1 1 ..."；字段 3 起（state=3），starttime 是第 22 个 ⇒ 下标 19。
    after.split_whitespace().nth(19)?.parse().ok()
}

/// 读 pid 文件（不判断死活；解析失败 ⇒ `None`）。
#[must_use]
pub fn read_lock(dir: &Path) -> Option<LockInfo> {
    let text = fs::read_to_string(pid_path(dir)).ok()?;
    let mut lines = text.lines();
    let pid: u32 = lines.next()?.trim().parse().ok()?;
    let starttime: u64 = lines.next()?.trim().parse().ok()?;
    let mode = LockMode::parse(lines.next()?)?;
    let socket = PathBuf::from(lines.next()?.trim());
    let version = lines.next().unwrap_or("").trim().to_owned();
    Some(LockInfo {
        pid,
        starttime,
        mode,
        socket,
        version,
    })
}

/// **锁是否活着**（持有者进程在，且 pid 未被复用）。
#[must_use]
pub fn is_live(info: &LockInfo) -> bool {
    proc_starttime(info.pid).is_some_and(|st| st == info.starttime)
}

/// **一把已取得的实例锁**（Drop 即释放：删 pid 文件）。
pub struct InstanceLock {
    path: PathBuf,
    info: LockInfo,
}

impl InstanceLock {
    /// **取锁**：陈旧锁自动清理，活锁 ⇒ [`LockError::Occupied`]。
    pub fn acquire(dir: &Path, mode: LockMode, socket: &Path) -> Result<Self, LockError> {
        if let Some(existing) = read_lock(dir) {
            if is_live(&existing) {
                return Err(LockError::Occupied {
                    pid: existing.pid,
                    mode: existing.mode,
                });
            }
            // 陈旧（进程没了 / pid 被复用）：清理后重取。
            let _ = fs::remove_file(pid_path(dir));
        }
        let pid = std::process::id();
        let the_info = LockInfo {
            pid,
            starttime: proc_starttime(pid).unwrap_or(0),
            mode,
            socket: socket.to_path_buf(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
        };
        let text = format!(
            "{}\n{}\n{}\n{}\n{}\n",
            the_info.pid,
            the_info.starttime,
            the_info.mode.as_str(),
            the_info.socket.display(),
            the_info.version
        );
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(pid_path(dir))?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        Ok(Self {
            path: pid_path(dir),
            info: the_info,
        })
    }

    /// 锁的事实。
    #[must_use]
    pub fn info(&self) -> &LockInfo {
        &self.info
    }

    /// **显式释放**（与 Drop 等价；服务退出前调用以便把"已清理"写进日志）。
    pub fn release(self) {}
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
