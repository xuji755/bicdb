//! **后台服务**：`bicdb start | stop | status | restart`（PG `pg_ctl` 形态）。
//!
//! ```text
//! start   ──▶ 起一个分离的后台进程（__daemon）→ 等它就绪（-w 语义）
//! __daemon──▶ 取实例锁 → 打开实例（三阶段恢复）→ 绑控制套接字 → 接客
//! stop    ──▶ 经套接字请求收尾（fast = 完全检查点；immediate = 直接退）
//! status  ──▶ 锁事实 + 服务自述（版本/实例/运行时长/连接数）
//! restart ──▶ stop + start
//! ```
//!
//! **取法与边界**（对照 PG `pg_ctl` / Oracle `dbstart|dbshut`、`STARTUP|SHUTDOWN`）：
//!
//! | 事 | PG | Oracle | 我们 |
//! | --- | --- | --- | --- |
//! | 分离 | `pg_ctl start` 里 fork + setsid | `dbstart` 走 `sqlplus / as sysdba` | `Command::process_group(0)`（std，零 unsafe；新进程组 ⇒ 不受终端 SIGHUP 影响） |
//! | 就绪等待 | `-w` 轮询连接 | 等 `STARTUP` 返回 | 轮询控制套接字的 `STATUS`（`--timeout`，默认 30 s） |
//! | 停止 | `-m smart|fast|immediate` | `SHUTDOWN [NORMAL|IMMEDIATE|ABORT]` | `stop [-m fast|immediate]`（fast = 完全检查点；immediate = 直接退出，下次打开走崩溃恢复） |
//! | 日志 | `-l logfile` | alert log | 默认 `<dir>/bicdb.log`（`--log` 覆盖） |
//! | 身份 | `postmaster.pid` | 实例锁 | `<dir>/bicdb.pid`（见 `lock.rs`） |
//!
//! **不发信号**：`#![forbid(unsafe_code)]`（`kill(2)` 用不了），也不需要——
//! 停止走套接字请求，服务自己收尾。被 `kill -9` 打死属于"崩溃"，
//! 下次打开由三阶段恢复兜底（这条路径有 e2e 用例）。

use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bicdb_sql::session::Session;

use crate::boot::{open_unlocked, Instance};
use crate::lock::{self, InstanceLock, LockError, LockInfo, LockMode};
use crate::wire::{self, WireError};

/// 服务错误。
#[derive(Debug)]
pub enum ServiceError {
    /// 实例锁（占用/IO）。
    Lock(LockError),
    /// 打开实例。
    Boot(crate::boot::BootError),
    /// 套接字/协议。
    Wire(WireError),
    /// 状态非法（没在跑 / 已经在跑 / 就绪超时）。
    State(String),
    /// I/O。
    Io(std::io::Error),
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServiceError::Lock(e) => write!(f, "{e}"),
            ServiceError::Boot(e) => write!(f, "{e}"),
            ServiceError::Wire(e) => write!(f, "{e}"),
            ServiceError::State(w) => f.write_str(w),
            ServiceError::Io(e) => write!(f, "I/O：{e}"),
        }
    }
}

impl std::error::Error for ServiceError {}

macro_rules! from_err {
    ($($v:ident <- $t:ty),* $(,)?) => { $(impl From<$t> for ServiceError {
        fn from(e: $t) -> Self { Self::$v(e) }
    })* };
}
from_err!(
    Lock <- LockError,
    Boot <- crate::boot::BootError,
    Wire <- WireError,
    Io <- std::io::Error,
);

/// 停止形态（`pg_ctl -m` / Oracle `SHUTDOWN` 的两档简化）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopMode {
    /// **fast**（默认）：服务先做**完全检查点**再退出（下次打开无需重做）。
    Fast,
    /// **immediate**：直接退出，不做检查点——下次打开走崩溃恢复（数据不丢：
    /// 提交记录在日志里）。
    Immediate,
}

impl StopMode {
    /// 协议词。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            StopMode::Fast => "fast",
            StopMode::Immediate => "immediate",
        }
    }

    /// 解析（`fast`/`immediate`）。
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "fast" | "normal" => Some(StopMode::Fast),
            "immediate" | "abort" => Some(StopMode::Immediate),
            _ => None,
        }
    }
}

/// 服务的启动参数。
#[derive(Debug, Clone)]
pub struct StartOptions {
    /// 实例目录。
    pub dir: PathBuf,
    /// 控制套接字路径（默认 `<dir>/bicdb.sock`）。
    pub socket: PathBuf,
    /// 日志路径（默认 `<dir>/bicdb.log`）。
    pub log: PathBuf,
    /// 就绪等待上限。
    pub timeout: Duration,
}

impl StartOptions {
    /// 由实例目录与覆盖项组装。
    #[must_use]
    pub fn new(
        dir: &Path,
        socket: Option<PathBuf>,
        log: Option<PathBuf>,
        timeout: Duration,
    ) -> Self {
        Self {
            dir: dir.to_path_buf(),
            socket: socket.unwrap_or_else(|| lock::socket_path(dir)),
            log: log.unwrap_or_else(|| dir.join("bicdb.log")),
            timeout,
        }
    }
}

// ───────────────────────── 服务端：守护进程 ─────────────────────────

/// **守护进程主体**（由 `bicdb start` 以 `__daemon` 子命令拉起）。
///
/// 顺序不能换：**先取锁再打开实例**——反过来会有一段"实例被打开但没上锁"
/// 的窗口，另一个进程能挤进来。
pub fn run_daemon(opts: &StartOptions, foreground: bool) -> Result<(), ServiceError> {
    let mut log = LogFile::open(&opts.log)?;
    log.line(&format!(
        "bicdb {} 服务启动中（实例 {}，套接字 {}）",
        env!("CARGO_PKG_VERSION"),
        opts.dir.display(),
        opts.socket.display()
    ));

    let lock = InstanceLock::acquire(&opts.dir, LockMode::Service, &opts.socket)?;
    log.line(&format!(
        "已取实例锁（pid {}，模式 service）",
        std::process::id()
    ));

    // **打开实例**（三阶段恢复）：可能耗时（重放），先记日志再开工。
    let mut inst = open_unlocked(&opts.dir, Some(lock))?;
    match inst.recovery {
        Some(r) => log.line(&format!(
            "实例已打开：恢复起点 LSN {}，重放 {} 块，回滚 {} 个事务，续写位 {}",
            r.start_lsn, r.applied_blocks, r.txns_rolled_back, r.log_end
        )),
        None => log.line("实例已打开（无恢复回放）"),
    }

    // **绑控制套接字**：陈旧套接字文件（上次进程没清干净）先删。
    if opts.socket.exists() {
        let _ = std::fs::remove_file(&opts.socket);
    }
    let listener = std::os::unix::net::UnixListener::bind(&opts.socket)?;
    log.line(&format!("控制套接字已就绪：{}", opts.socket.display()));
    log.line("READY");

    let mut served: u64 = 0;
    let started = Instant::now();
    let mut stop_after: Option<StopMode> = None;
    'accept: for conn in listener.incoming() {
        let mut s = match conn {
            Ok(s) => s,
            Err(e) => {
                log.line(&format!("接受连接失败：{e}"));
                continue;
            }
        };
        // **一个连接 = 一个会话**：事务（`BEGIN … COMMIT`）跨该连接的语句保持
        // ——SQL*Plus 一句一发，事务语义必须绑在连接上，不能绑在单条语句上。
        // 先服务「不需要会话」的动词（DESCRIBE 只读目录），再建会话——
        // 会话借住 `inst.catalog`，一旦建了就没法再用 `inst`。
        let (verb, payload) = match wire::read_frame(&mut s) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if verb == "DESCRIBE" {
            match describe_object(&mut inst, payload.trim()) {
                Ok(cols) => {
                    let body = wire::encode_columns(&cols);
                    let _ = wire::write_frame(&mut s, "OK", &body);
                }
                Err(e) => {
                    let _ = wire::write_frame(&mut s, "ERR", &e);
                }
            }
            continue;
        }
        let seq = inst.seq();
        let mut session = Session::new(inst.pool, inst.engine, &mut inst.catalog, seq);
        // 第一帧已经读走了：先处理它，再进循环。
        let mut pending = Some((verb, payload));
        loop {
            let (verb, payload) = match pending.take() {
                Some(v) => v,
                None => match wire::read_frame(&mut s) {
                    Ok(v) => v,
                    Err(_) => break,
                },
            };
            match verb.as_str() {
                "HELLO" => {
                    let _ = wire::write_frame(
                        &mut s,
                        "OK",
                        &format!(
                            "wire={} version={}",
                            wire::WIRE_VERSION,
                            env!("CARGO_PKG_VERSION")
                        ),
                    );
                }
                "STATUS" => {
                    let body = format!(
                        "instance={}\nversion={}\npid={}\nuptime_s={}\nserved={served}\nseq={}\nmode=service\nwire={}\n",
                        opts.dir.display(),
                        env!("CARGO_PKG_VERSION"),
                        std::process::id(),
                        started.elapsed().as_secs(),
                        session.seq(),
                        wire::WIRE_VERSION
                    );
                    let _ = wire::write_frame(&mut s, "OK", &body);
                }
                "SQL" => {
                    served += 1;
                    match session.execute(&payload) {
                        Ok(results) => {
                            let body = wire::encode_results(&results);
                            let _ = wire::write_frame(&mut s, "OK", &body);
                        }
                        Err(e) => {
                            log.line(&format!("语句失败：{e}"));
                            let _ = wire::write_frame(&mut s, "ERR", &e.to_string());
                        }
                    }
                }
                "SHUTDOWN" => {
                    let mode = StopMode::parse(&payload).unwrap_or(StopMode::Fast);
                    let _ = wire::write_frame(
                        &mut s,
                        "OK",
                        &format!("shutting down（{}）", mode.as_str()),
                    );
                    log.line(&format!("收到停止请求（{}）", mode.as_str()));
                    stop_after = Some(mode);
                    break 'accept;
                }
                other => {
                    let _ = wire::write_frame(&mut s, "ERR", &format!("未知动词 `{other}`"));
                }
            }
        }
        drop(session); // 会话落前把事务收尾（`Drop` 里回滚未提交的显式事务）
    }

    // **收尾**：fast 走完全检查点；immediate 直接退出（下次打开做恢复）。
    match stop_after {
        Some(StopMode::Immediate) => {
            log.line("immediate 停止：不做完全检查点（下次打开走崩溃恢复）")
        }
        _ => match inst.shutdown() {
            Ok(()) => log.line("完全检查点完成，实例已干净关闭"),
            Err(e) => log.line(&format!("关闭时出错：{e}")),
        },
    }
    drop(listener);
    let _ = std::fs::remove_file(&opts.socket);
    log.line("服务已退出");
    if foreground {
        let _ = std::io::stdout().flush();
    }
    Ok(())
}

/// **列定义**（服务端的 `DESCRIBE`：只读目录，不建会话）。
fn describe_object(inst: &mut Instance, name: &str) -> Result<Vec<(String, bool, String)>, String> {
    use bicdb_catalog::dict::namespace;
    if name.is_empty() {
        return Err("DESCRIBE 缺对象名".to_owned());
    }
    let snapshot = bicdb_common::seq::CommitSeq::from_raw(inst.seq().max(1)).expect("48 位域内");
    let obj = inst
        .catalog
        .resolve(snapshot, namespace::TABLE, name)
        .map_err(|e| e.to_string())?;
    let cols = inst
        .catalog
        .columns(snapshot, obj.obj)
        .map_err(|e| e.to_string())?;
    Ok(cols
        .into_iter()
        .map(|c| {
            (
                c.name,
                c.nullable,
                crate::service::type_name(c.type_code, c.length),
            )
        })
        .collect())
}

/// 类型码 → SQL 类型名（与 `bicdbcli` 的 `DESCRIBE` 版面同源）。
#[must_use]
pub fn type_name(code: u32, length: u32) -> String {
    use bicdb_catalog::dict::ColTypeCode;
    let Some(t) = ColTypeCode::from_u8(code as u8) else {
        return format!("UNKNOWN({code})");
    };
    match t {
        ColTypeCode::Number => "NUMBER".to_owned(),
        ColTypeCode::Char => format!("CHAR({length})"),
        ColTypeCode::Varchar2 => format!("VARCHAR2({length})"),
        ColTypeCode::Date => "DATE".to_owned(),
        ColTypeCode::Timestamp => "TIMESTAMP".to_owned(),
        ColTypeCode::Boolean => "BOOLEAN".to_owned(),
        ColTypeCode::Uuid => "UUID".to_owned(),
        ColTypeCode::TimestampTz => "TIMESTAMP WITH TIME ZONE".to_owned(),
        ColTypeCode::Json => "JSON".to_owned(),
        ColTypeCode::Vector => "VECTOR".to_owned(),
        ColTypeCode::AssetRef => "ASSET_REF".to_owned(),
        ColTypeCode::Bytes => format!("RAW({length})"),
    }
}

/// 追加式日志（服务日志 = PG 的 `-l` 日志文件 / Oracle 的 alert log 的简化）。
struct LogFile {
    file: std::fs::File,
}

impl LogFile {
    fn open(path: &Path) -> Result<Self, ServiceError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        Ok(Self { file })
    }

    fn line(&mut self, text: &str) {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(self.file, "[{stamp}] {text}");
        let _ = self.file.flush();
    }
}

// ───────────────────────── 客户端：start / stop / status ─────────────────────────

/// **`bicdb start`**：分离起服务并等它就绪（`-w` 语义）。
pub fn start(opts: &StartOptions) -> Result<(), ServiceError> {
    if !opts.dir.join(crate::boot::FILE0).exists() {
        return Err(ServiceError::State(format!(
            "{} 不是 bicdb 实例（缺 {}）——先 `bicdb init`",
            opts.dir.display(),
            crate::boot::FILE0
        )));
    }
    if let Some(info) = lock::read_lock(&opts.dir) {
        if lock::is_live(&info) {
            return Err(ServiceError::State(format!(
                "实例已被 pid {}（{}）占用",
                info.pid,
                match info.mode {
                    LockMode::Service => "服务模式",
                    LockMode::Direct => "直连模式",
                }
            )));
        }
    }
    let exe = std::env::current_exe()?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&opts.log)?;
    let errlog = log.try_clone()?;
    let child = std::process::Command::new(exe)
        .arg("__daemon")
        .arg("--dir")
        .arg(&opts.dir)
        .arg("--socket")
        .arg(&opts.socket)
        .arg("--log")
        .arg(&opts.log)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(errlog))
        // 新进程组：终端关闭/`Ctrl-C` 不会顺手带走服务（零 unsafe 版的 setsid）。
        .process_group(0)
        .spawn()?;
    let pid = child.id();

    // 等就绪（PG `-w`）：套接字能应答 STATUS 才算起来了。
    let deadline = Instant::now() + opts.timeout;
    loop {
        if let Ok(body) = wire::call_timeout(&opts.socket, "STATUS", "", Duration::from_millis(500))
        {
            println!("服务已启动（pid {pid}）");
            println!("  实例    {}", opts.dir.display());
            println!("  套接字  {}", opts.socket.display());
            println!("  日志    {}", opts.log.display());
            if let Some(v) = body.lines().find_map(|l| l.strip_prefix("version=")) {
                println!("  版本    {v}");
            }
            return Ok(());
        }
        // 子进程死了就别等了。
        if !lock::proc_starttime(pid).is_some_and(|_| true) {
            return Err(ServiceError::State(format!(
                "服务进程 {pid} 已退出——看日志：{}",
                opts.log.display()
            )));
        }
        if Instant::now() >= deadline {
            return Err(ServiceError::State(format!(
                "等待就绪超时（{} 秒）——看日志：{}",
                opts.timeout.as_secs(),
                opts.log.display()
            )));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// **`bicdb stop`**：请服务收尾（经套接字；`-m fast|immediate`），并等它退出。
pub fn stop(dir: &Path, mode: StopMode) -> Result<(), ServiceError> {
    let info = lock::read_lock(dir).ok_or_else(|| {
        ServiceError::State(format!("{} 上没有服务在跑（无 pid 文件）", dir.display()))
    })?;
    if !lock::is_live(&info) {
        let _ = std::fs::remove_file(lock::pid_path(dir));
        return Err(ServiceError::State(
            "pid 文件的持有者已不在（陈旧锁，已清理）".to_owned(),
        ));
    }
    let reply = wire::call_timeout(
        &info.socket,
        "SHUTDOWN",
        mode.as_str(),
        Duration::from_secs(30),
    )
    .map_err(|e| {
        ServiceError::State(format!(
            "连服务失败（{}）：{e}——如果是直连模式占着实例，请到那个进程里收尾",
            info.socket.display()
        ))
    })?;
    // 等进程真的退出（跑完全检查点要时间）。
    let deadline = Instant::now() + Duration::from_secs(300);
    while lock::proc_starttime(info.pid).is_some_and(|st| st == info.starttime) {
        if Instant::now() >= deadline {
            return Err(ServiceError::State(format!(
                "服务（pid {}）在 300 秒内没退出",
                info.pid
            )));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    println!("服务已停止（pid {}）——{}", info.pid, reply);
    if lock::pid_path(dir).exists() {
        let _ = std::fs::remove_file(lock::pid_path(dir));
    }
    Ok(())
}

/// **`bicdb status`**：锁事实 + 服务自述。
pub fn status(dir: &Path) -> Result<(), ServiceError> {
    let Some(info) = lock::read_lock(dir) else {
        println!("未运行：{} 上没有 pid 文件", dir.display());
        return Ok(());
    };
    if !lock::is_live(&info) {
        println!(
            "未运行：pid {} 的进程已不在（陈旧锁）——下次打开会自动接管",
            info.pid
        );
        return Ok(());
    }
    let mode = match info.mode {
        LockMode::Service => "服务模式（可接客）",
        LockMode::Direct => "直连模式（某前台进程开着）",
    };
    println!("运行中：pid {}，{mode}", info.pid);
    println!("  实例    {}", dir.display());
    println!("  套接字  {}", info.socket.display());
    println!("  版本    {}", info.version);
    if info.mode == LockMode::Service {
        match wire::call_timeout(&info.socket, "STATUS", "", Duration::from_secs(5)) {
            Ok(body) => {
                for line in body.lines() {
                    if let Some((k, v)) = line.split_once('=') {
                        match k {
                            "uptime_s" => println!("  运行    {} 秒", v),
                            "served" => println!("  已服务  {v} 次请求"),
                            "seq" => println!("  提交序号 {v}"),
                            _ => {}
                        }
                    }
                }
            }
            Err(e) => println!("  （套接字无应答：{e}）"),
        }
    }
    Ok(())
}

/// **`bicdb restart`**：stop（fast）后 start。
pub fn restart(opts: &StartOptions) -> Result<(), ServiceError> {
    if lock::read_lock(&opts.dir).is_some_and(|i| lock::is_live(&i)) {
        stop(&opts.dir, StopMode::Fast)?;
    }
    start(opts)
}

/// 服务在跑吗（供直连模式判断"是否该走套接字"）。
#[must_use]
pub fn state_of(dir: &Path) -> ServiceState {
    match lock::read_lock(dir) {
        None => ServiceState::NotRunning,
        Some(info) if !lock::is_live(&info) => ServiceState::Stale(info),
        Some(info) => match info.mode {
            LockMode::Service => ServiceState::Serving(info),
            LockMode::Direct => ServiceState::Direct(info),
        },
    }
}

/// 实例当前的占用状态（`bicdbcli` 的连接选择依据）。
#[derive(Debug, Clone)]
pub enum ServiceState {
    /// 没人开着（可直连）。
    NotRunning,
    /// 服务在跑（客户走套接字）。
    Serving(LockInfo),
    /// 某个前台进程直连着（客户应拒绝或等待）。
    Direct(LockInfo),
    /// 陈旧锁（进程已不在）。
    Stale(LockInfo),
}
