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
//! | 就绪等待 | `-w` 轮询连接 | 等 `STARTUP` 返回 | 轮询控制套接字的 `STATUS`（`-w|--wait`，默认 30 s） |
//! | 停止 | `-m smart|fast|immediate` | `SHUTDOWN [NORMAL|IMMEDIATE|ABORT]` | `stop [-m fast|immediate]`（fast = 完全检查点；immediate = 直接退出，下次打开走崩溃恢复） |
//! | 日志 | `-l logfile` | alert log | 默认 `<dir>/bicdb.log`（`--log` 覆盖） |
//! | 身份 | `postmaster.pid` | 实例锁 | `<dir>/bicdb.pid`（见 `lock.rs`） |
//!
//! **不发信号**：`#![forbid(unsafe_code)]`（`kill(2)` 用不了），也不需要——
//! 停止走套接字请求，服务自己收尾。被 `kill -9` 打死属于"崩溃"，
//! 下次打开由三阶段恢复兜底（这条路径有 e2e 用例）。

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use bicdb_sql::session::{
    FulltextScheduler, Session, SessionState, SqlRequest as ExecutableSql, SqlStep,
};
use bicdb_txn::engine::{RowOwnerWait, RowWaitStatus};

use bicdb_net::client::{call_once, ClientError};
use bicdb_net::{frame, AuthOk, AuthRequest, Hello, SqlRequest, WIRE_VERSION};

use crate::boot::{open_shared_force_private, open_shared_with};
use crate::lock::{self, InstanceLock, LockError, LockInfo, LockMode};
use crate::proto;

const STARTER_PID_ENV: &str = "BICDB_INTERNAL_STARTER_PID";

/// `__daemon` is an implementation detail of `bicdb start`.  Require the
/// immediate parent to be this same executable running the `start`
/// command, so users and service managers cannot bypass lifecycle checks by
/// invoking the hidden subcommand directly.
fn guard_daemon_launch() -> Result<(), ServiceError> {
    let expected: u32 = std::env::var(STARTER_PID_ENV)
        .ok()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| ServiceError::State("`__daemon` 是内部入口；请使用 `bicdb start`".into()))?;
    let status = std::fs::read_to_string("/proc/self/status")?;
    let parent: u32 = status
        .lines()
        .find_map(|line| line.strip_prefix("PPid:").map(str::trim))
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| ServiceError::State("无法核验 daemon 启动父进程".into()))?;
    if parent != expected {
        return Err(ServiceError::State(
            "`__daemon` 启动父进程不合法；请使用 `bicdb start`".into(),
        ));
    }
    let parent_exe = std::fs::canonicalize(format!("/proc/{parent}/exe"))?;
    let current_exe = std::fs::canonicalize(std::env::current_exe()?)?;
    let cmdline = std::fs::read(format!("/proc/{parent}/cmdline"))?;
    let args: Vec<&[u8]> = cmdline
        .split(|byte| *byte == 0)
        .filter(|v| !v.is_empty())
        .collect();
    let lifecycle_entry = matches!(args.get(1).copied(), Some(b"start") | Some(b"restart"));
    if parent_exe != current_exe || !lifecycle_entry {
        return Err(ServiceError::State(
            "`__daemon` 只能由 `bicdb start` 启动".into(),
        ));
    }
    Ok(())
}

enum ServiceEvent {
    WorkspaceOpen {
        workspace_id: u64,
        name: String,
        root: PathBuf,
        mode: bicdb_sql::WorkspaceOpenMode,
        reply: mpsc::SyncSender<Result<String, String>>,
    },
    WorkspaceVerify {
        workspace_id: u64,
        name: String,
        root: PathBuf,
        scope: bicdb_sql::RecoveryVerifyScope,
        reply: mpsc::SyncSender<Result<String, String>>,
    },
    Request {
        id: u64,
        verb: String,
        payload: Vec<u8>,
        reply: mpsc::SyncSender<WireReply>,
    },
    Closed(u64),
    Suspended {
        request: WorkerRequest,
        wait: RowOwnerWait,
        elapsed_us: u128,
    },
    Completed {
        id: u64,
        workspace: [u8; 8],
        state: SessionState,
        elapsed_us: u128,
        error: Option<String>,
    },
}

struct DaemonWorkspaceController {
    events: mpsc::Sender<ServiceEvent>,
}

impl bicdb_sql::dcl_exec::WorkspaceStateController for DaemonWorkspaceController {
    fn open_workspace(
        &self,
        workspace_id: u64,
        name: &str,
        root: &Path,
        mode: bicdb_sql::WorkspaceOpenMode,
    ) -> Result<String, String> {
        let (reply, receive) = mpsc::sync_channel(0);
        self.events
            .send(ServiceEvent::WorkspaceOpen {
                workspace_id,
                name: name.to_owned(),
                root: root.to_path_buf(),
                mode,
                reply,
            })
            .map_err(|_| "daemon 工作区状态控制器已停止".to_owned())?;
        receive
            .recv()
            .map_err(|_| "daemon 工作区状态控制器未回复".to_owned())?
    }

    fn verify_recovery(
        &self,
        workspace_id: u64,
        name: &str,
        root: &Path,
        scope: bicdb_sql::RecoveryVerifyScope,
    ) -> Result<String, String> {
        let (reply, receive) = mpsc::sync_channel(0);
        self.events
            .send(ServiceEvent::WorkspaceVerify {
                workspace_id,
                name: name.to_owned(),
                root: root.to_path_buf(),
                scope,
                reply,
            })
            .map_err(|_| "daemon 工作区恢复验证控制器已停止".to_owned())?;
        receive
            .recv()
            .map_err(|_| "daemon 工作区恢复验证控制器未回复".to_owned())?
    }
}

fn workspace_open_uses_narrow_force(mode: bicdb_sql::WorkspaceOpenMode) -> bool {
    matches!(
        mode,
        bicdb_sql::WorkspaceOpenMode::ReadOnly | bicdb_sql::WorkspaceOpenMode::ReadWriteForce
    )
}

fn validate_forced_write_transition(
    forced_recovery: bool,
    mode: bicdb_sql::WorkspaceOpenMode,
) -> Result<(), &'static str> {
    if forced_recovery && mode == bicdb_sql::WorkspaceOpenMode::ReadWrite {
        Err("带不一致范围打开的只读工作区恢复写入必须显式使用 FORCE")
    } else {
        Ok(())
    }
}

fn bound_workspace_capacity(
    active_private: usize,
    maximum: usize,
    already_open: bool,
) -> Result<(), String> {
    if already_open || 1usize.saturating_add(active_private) < maximum {
        Ok(())
    } else {
        Err(format!(
            "实例已达到工作区绑定上限 {maximum}（包含 PUBLIC）；请提高 service.max_bound_workspaces 后重启"
        ))
    }
}

fn remember_recovery_failure(
    registry: &mut RecoveryRequiredRegistry,
    workspace: [u8; 8],
    error: &crate::boot::BootError,
) {
    if matches!(
        error,
        crate::boot::BootError::Io(_) | crate::boot::BootError::Catalog(_)
    ) {
        registry.record(workspace, error.to_string());
    }
}

struct WireReply {
    status: &'static str,
    payload: Vec<u8>,
    close: bool,
}

impl WireReply {
    fn ok(payload: impl Into<Vec<u8>>) -> Self {
        Self {
            status: "OK",
            payload: payload.into(),
            close: false,
        }
    }
    fn error(payload: impl Into<Vec<u8>>) -> Self {
        Self {
            status: "ERR",
            payload: payload.into(),
            close: false,
        }
    }
}

struct ConnectionSession {
    cancelled: Arc<AtomicBool>,
    state: Option<SessionState>,
    sql_served: bool,
    authed: Option<String>,
    auth_failed: bool,
    workspace: Option<[u8; 8]>,
}

fn workspace_has_open_transaction(
    connections: &BTreeMap<u64, ConnectionSession>,
    public_workspace: [u8; 8],
    workspace: [u8; 8],
) -> bool {
    connections.values().any(|connection| {
        connection.workspace.unwrap_or(public_workspace) == workspace
            && connection
                .state
                .as_ref()
                .is_some_and(SessionState::in_transaction)
    })
}

enum WorkerAction {
    Sql {
        sql: String,
        params: Vec<(String, bicdb_exec::Value)>,
    },
    Parsed {
        sql: ExecutableSql,
        wait: Option<RowOwnerWait>,
        cancel: Option<String>,
    },
    Describe(String),
}

struct WorkerRequest {
    cancelled: Arc<AtomicBool>,
    id: u64,
    workspace: [u8; 8],
    runtime: WorkspaceExecution,
    state: SessionState,
    action: WorkerAction,
    reply: mpsc::SyncSender<WireReply>,
}

#[derive(Clone)]
struct WorkspaceExecution {
    pool: &'static bicdb_storage::buffer::BufferPool<'static>,
    engine: &'static bicdb_txn::engine::Engine<'static, 'static, 'static, 'static>,
    io: &'static dyn bicdb_workspace::io::FileIo,
    dir: PathBuf,
    workspace: [u8; 8],
}

#[derive(Default)]
struct RecoveryRequiredRegistry {
    causes: BTreeMap<[u8; 8], String>,
}
impl RecoveryRequiredRegistry {
    fn record(&mut self, workspace: [u8; 8], reason: String) -> &str {
        self.causes.entry(workspace).or_insert(reason).as_str()
    }
    fn cause(&self, workspace: &[u8; 8]) -> Option<&str> {
        self.causes.get(workspace).map(String::as_str)
    }
    fn len(&self) -> usize {
        self.causes.len()
    }
    fn clear(&mut self, workspace: &[u8; 8]) {
        self.causes.remove(workspace);
    }
}
impl WorkspaceExecution {
    fn from_instance(instance: &crate::boot::Instance) -> Self {
        Self {
            pool: instance.pool,
            engine: instance.engine,
            io: instance.io,
            dir: instance.dir.clone(),
            workspace: instance.ws_ref,
        }
    }
    fn catalog(&self) -> Result<bicdb_catalog::Catalog<'static>, ServiceError> {
        let path = self
            .dir
            .join("data")
            .join(crate::boot::data_file_name(self.workspace, "meta"));
        let mut catalog = bicdb_catalog::Catalog::open(self.io, &path)
            .map_err(|e| ServiceError::State(e.to_string()))?;
        catalog.attach_pool(self.pool);
        Ok(catalog)
    }
}
fn selected_instance<'a>(
    public: &'a mut crate::boot::Instance,
    workspaces: &'a mut BTreeMap<[u8; 8], crate::boot::Instance>,
    workspace: Option<[u8; 8]>,
) -> &'a mut crate::boot::Instance {
    match workspace {
        Some(ws) if ws != public.ws_ref => workspaces.get_mut(&ws).expect("bound workspace exists"),
        _ => public,
    }
}

fn require_public_entry(opts: &StartOptions) -> Result<(), ServiceError> {
    let path = crate::boot::find_meta_file(&opts.dir)
        .ok_or_else(|| ServiceError::State("实例目录缺少字典文件".into()))?;
    let io = bicdb_workspace::io::OsFileIo::new();
    let catalog =
        bicdb_catalog::Catalog::open(&io, &path).map_err(|e| ServiceError::State(e.to_string()))?;
    let is_public = catalog.is_public();
    catalog
        .close()
        .map_err(|e| ServiceError::State(format!("关闭启动检查字典文件失败：{e}")))?;
    if !is_public {
        // A registered deployment has exactly one service entry: <home>/public.
        // Keep explicit standalone databases usable for tests and embedded
        // installations that have no BICDB_HOME/workspace registry.
        if let Ok(home) = crate::home::Home::locate() {
            let dir = std::fs::canonicalize(&opts.dir).unwrap_or_else(|_| opts.dir.clone());
            let root = std::fs::canonicalize(&home.root).unwrap_or(home.root);
            if dir.starts_with(root) {
                return Err(ServiceError::State(
                    "不能为逻辑工作区启动独立 daemon；请连接标准参数文件指定的 PUBLIC 实例".into(),
                ));
            }
        }
    }
    Ok(())
}

fn serve_connection(
    id: u64,
    mut stream: std::os::unix::net::UnixStream,
    events: mpsc::Sender<ServiceEvent>,
) {
    while let Ok((verb, payload)) = frame::read_frame_bytes(&mut stream) {
        let (reply, receive) = mpsc::sync_channel(0);
        if events
            .send(ServiceEvent::Request {
                id,
                verb,
                payload,
                reply,
            })
            .is_err()
        {
            break;
        }
        let response = loop {
            match receive.recv_timeout(Duration::from_millis(100)) {
                Ok(response) => break Some(response),
                Err(mpsc::RecvTimeoutError::Disconnected) => break None,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if peer_disconnected(&stream) {
                        break None;
                    }
                }
            }
        };
        let Some(response) = response else {
            break;
        };
        if frame::write_frame_bytes(&mut stream, response.status, &response.payload).is_err()
            || response.close
        {
            break;
        }
    }
    let _ = events.send(ServiceEvent::Closed(id));
}

/// Peek without consuming a pipelined request or changing socket blocking mode.
fn peer_disconnected(stream: &std::os::unix::net::UnixStream) -> bool {
    let mut byte = [0u8; 1];
    match rustix::net::recv(
        stream,
        &mut byte[..],
        rustix::net::RecvFlags::PEEK | rustix::net::RecvFlags::DONTWAIT,
    ) {
        Ok((_, length)) => length == 0,
        Err(error) => error != rustix::io::Errno::AGAIN && error != rustix::io::Errno::INTR,
    }
}

struct PublicAuthenticator {
    public_root: PathBuf,
    private_root: PathBuf,
    home: PathBuf,
    io: &'static dyn bicdb_workspace::io::FileIo,
}
impl bicdb_sql::auth::PrivateAuthProvider for PublicAuthenticator {
    fn authenticate_private(
        &self,
        name: &str,
        password: &str,
        workspace: [u8; 8],
    ) -> Result<bicdb_sql::auth::PrivateLogin, String> {
        let socket = bicdb_net::socket_for(Some(&self.public_root)).map_err(|e| e.to_string())?;
        let mut last = String::new();
        for attempt in 0..50 {
            let mut public =
                match bicdb_net::Client::connect_with_timeout(&socket, Duration::from_secs(5)) {
                    Ok(client) => client,
                    Err(ClientError::Busy) if attempt < 49 => {
                        std::thread::sleep(Duration::from_millis(100));
                        continue;
                    }
                    Err(e) => {
                        last = e.to_string();
                        break;
                    }
                };
            public
                .set_timeout(Duration::from_secs(30))
                .map_err(|e| e.to_string())?;
            if Path::new(&public.hello().instance)
                .canonicalize()
                .map_err(|e| e.to_string())?
                != self.public_root.canonicalize().map_err(|e| e.to_string())?
            {
                return Err("认证失败：PUBLIC 服务路径不匹配".into());
            }
            let identity = public.auth(name, password).map_err(|e| e.to_string())?;
            if identity.expired {
                return Err("口令已过期，须先在 PUBLIC 修改口令".into());
            }
            let paths =
                bicdb_sql::dcl_exec::DclContext::new(self.home.clone(), self.io).global_ctl_paths();
            let registry =
                bicdb_storage::globalctl::GlobalControlFile::open(self.io, &paths[0], &paths[1])
                    .map_err(|e| e.to_string())?;
            let entry = (|| -> Result<_, String> {
                let root = self
                    .private_root
                    .canonicalize()
                    .map_err(|e| e.to_string())?;
                registry
                    .workspaces()
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .find(|entry| {
                        std::str::from_utf8(&entry.root)
                            .ok()
                            .and_then(|r| Path::new(r).canonicalize().ok())
                            .as_ref()
                            == Some(&root)
                    })
                    .ok_or_else(|| "认证失败：工作区未注册".into())
            })();
            let close_result = registry.close().map_err(|e| e.to_string());
            let entry = entry?;
            close_result?;
            let selection = entry.workspace_id.as_raw().to_string();
            let route = public
                .route_owned(Some(&selection))
                .map_err(|e| e.to_string())?;
            if identity.user_id != route.user_id
                || route.workspace_id.to_string() != selection
                || Path::new(&route.root)
                    .canonicalize()
                    .map_err(|e| e.to_string())?
                    != self
                        .private_root
                        .canonicalize()
                        .map_err(|e| e.to_string())?
            {
                return Err("认证失败：不能连接其他用户的工作区".into());
            }
            return Ok(bicdb_sql::auth::PrivateLogin {
                user_id: identity.user_id,
                name: identity.user,
                workspace,
            });
        }
        Err(format!("PUBLIC 认证服务不可用：{last}"))
    }
}

/// 服务错误。
#[derive(Debug)]
pub enum ServiceError {
    /// 实例锁（占用/IO）。
    Lock(LockError),
    /// 打开实例。
    Boot(crate::boot::BootError),
    /// 套接字/协议。
    Wire(ClientError),
    /// 状态非法（没在跑 / 已经在跑 / 就绪超时）。
    State(String),
    /// I/O。
    Io(std::io::Error),
    /// 参数文件（未知键/取值非法）。
    Config(crate::config::ConfigError),
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServiceError::Lock(e) => write!(f, "{e}"),
            ServiceError::Boot(e) => write!(f, "{e}"),
            ServiceError::Wire(e) => write!(f, "{e}"),
            ServiceError::State(w) => f.write_str(w),
            ServiceError::Io(e) => write!(f, "I/O：{e}"),
            ServiceError::Config(e) => write!(f, "参数：{e}"),
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
    Config <- crate::config::ConfigError,
    Lock <- LockError,
    Boot <- crate::boot::BootError,
    Wire <- ClientError,
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
    /// **实例参数**（按参数文件装载；根区目录由它注册）。
    pub params: crate::config::InstanceParams,
    /// 实例目录（= `params.db_root`；保留为便利字段）。
    pub dir: PathBuf,
    /// 控制套接字路径（默认取参数文件 `socket`，再默认 `<dir>/bicdb.sock`）。
    pub socket: PathBuf,
    /// 日志路径（默认取参数文件 `log`，再默认 `<dir>/bicdb.log`）。
    pub log: PathBuf,
    /// 就绪等待上限。
    pub timeout: Duration,
    /// 命令行 `-c 键=值` 覆盖（优先级最高）。
    pub overrides: Vec<(String, String)>,
    /// Explicit one-start allowance for narrowly classified private-workspace
    /// recovery inconsistencies. PUBLIC is never opened with this mode.
    pub force: bool,
}

impl StartOptions {
    /// **按参数文件寻址装载**（`-p` > `$BICDB_INI` > `./bicdb.ini`）+ 命令行覆盖。
    pub fn load(
        ini: Option<&Path>,
        socket: Option<PathBuf>,
        log: Option<PathBuf>,
        timeout: Duration,
        overrides: Vec<(String, String)>,
    ) -> Result<Self, ServiceError> {
        let (params, _) = crate::config::InstanceParams::load_with_overrides(ini, &overrides)?;
        Ok(Self::from_params(params, socket, log, timeout, overrides))
    }

    /// 同上，但**等待上限的默认值来自参数文件**（`service.start_wait_s` /
    /// `service.stop_wait_s`）——命令行 `-w` 给了就用它（`None` = 没给）。
    ///
    /// 为什么要有这条：`start`/`stop` 的等待上限此前分别写死 30 秒（命令行
    /// 默认）与 300 秒（代码常量），参数文件里没有对应键——**唯一一个只能走
    /// 命令行的运行期量**，与"参数文件是实例的权威"不符。
    pub fn load_with_wait(
        ini: Option<&Path>,
        socket: Option<PathBuf>,
        log: Option<PathBuf>,
        wait_s: Option<u64>,
        stop_side: bool,
        overrides: Vec<(String, String)>,
    ) -> Result<Self, ServiceError> {
        let (params, _) = crate::config::InstanceParams::load_with_overrides(ini, &overrides)?;
        let default_s = if stop_side {
            params.run.stop_wait_s
        } else {
            params.run.start_wait_s
        };
        let timeout = Duration::from_secs(wait_s.unwrap_or(default_s));
        Ok(Self::from_params(params, socket, log, timeout, overrides))
    }

    /// 由**已装载的参数**组装（`init` 之后的路径与 `start` 共用）。
    #[must_use]
    pub fn from_params(
        params: crate::config::InstanceParams,
        socket: Option<PathBuf>,
        log: Option<PathBuf>,
        timeout: Duration,
        overrides: Vec<(String, String)>,
    ) -> Self {
        let dir = params.db_root.clone();
        let socket = socket.unwrap_or_else(|| params.socket_path());
        let log = log.unwrap_or_else(|| params.log_path());
        Self {
            params,
            dir,
            socket,
            log,
            timeout,
            overrides,
            force: false,
        }
    }
}

// ───────────────────────── 服务端：守护进程 ─────────────────────────

fn run_sql_worker(
    params: crate::config::InstanceParams,
    work: Arc<Mutex<mpsc::Receiver<WorkerRequest>>>,
    events: mpsc::Sender<ServiceEvent>,
) {
    let home = crate::home::Home::locate().ok().map(|home| home.root);
    loop {
        let request = {
            let receiver = work.lock().unwrap_or_else(|error| error.into_inner());
            receiver.recv()
        };
        let Ok(mut request) = request else {
            break;
        };
        let runtime = request.runtime.clone();
        let pool = runtime.pool;
        let engine = runtime.engine;
        let io = runtime.io;
        let mut catalog = match runtime.catalog() {
            Ok(catalog) => catalog,
            Err(error) => {
                let _ = request
                    .reply
                    .send(WireReply::error(error.to_string().into_bytes()));
                let _ = events.send(ServiceEvent::Completed {
                    id: request.id,
                    workspace: request.workspace,
                    state: request.state,
                    elapsed_us: 0,
                    error: Some(error.to_string()),
                });
                continue;
            }
        };
        let fixed = crate::fixed::CliFixedTables::new(&runtime.dir, io);
        let mut state =
            std::mem::replace(&mut request.state, SessionState::new(engine.current_seq()));
        state.refresh_committed(engine.current_seq());
        let workspace_controller = DaemonWorkspaceController {
            events: events.clone(),
        };
        let mut session = Session::new(pool, engine, &mut catalog, engine.current_seq());
        session.resume_state(&mut state);
        let configured = session
            .set_fulltext_defaults(
                params.run.fulltext_interval_ms,
                params.run.fulltext_batch_rows,
            )
            .and_then(|()| session.set_graph_limits(params.run.graph_limits()));
        session.set_dcl_context(home.clone(), Some(io));
        session.set_workspace_provisioner(Some(provisioner_static()));
        session.set_workspace_state_controller(Some(&workspace_controller));
        session.set_pbkdf2_iterations(params.run.pbkdf2_iterations);
        session.set_fixed_table_source(Some(&fixed));
        let started = Instant::now();
        let mut suspended = None;
        let preparation = configured
            .map_err(|error| error.to_string())
            .and_then(|()| {
                if request.cancelled.load(Ordering::Acquire) {
                    session
                        .rollback_uncommitted()
                        .map_err(|error| error.to_string())?;
                    return Err("连接已断开，事务已回滚".into());
                }
                if matches!(&request.action, WorkerAction::Sql { .. }) {
                    let action = std::mem::replace(
                        &mut request.action,
                        WorkerAction::Describe(String::new()),
                    );
                    if let WorkerAction::Sql { sql, params } = action {
                        request.action = WorkerAction::Parsed {
                            sql: ExecutableSql::parse(&sql, params)
                                .map_err(|error| error.to_string())?,
                            wait: None,
                            cancel: None,
                        };
                    }
                }
                Ok(())
            });
        let result: Result<Option<WireReply>, String> =
            preparation.and_then(|()| match &mut request.action {
                WorkerAction::Parsed { sql, wait, cancel } => {
                    if let Some(reason) = cancel.take() {
                        if let Some(mut wait) = wait.take() {
                            wait.cancel();
                        }
                        session
                            .cancel_sql_request(sql)
                            .map_err(|error| error.to_string())?;
                        return Err(reason);
                    }
                    if let Some(mut token) = wait.take() {
                        let probe = (|| {
                            if token.needs_header() {
                                engine.load_row_wait_header(&mut token)?;
                            }
                            engine.poll_row_wait(&mut token)
                        })();
                        match probe {
                            Ok(RowWaitStatus::Pending) => {
                                suspended = Some(token);
                                return Ok(None);
                            }
                            Ok(RowWaitStatus::Ready) => {}
                            outcome => {
                                let reason = match outcome {
                                    Err(error) => error.to_string(),
                                    _ => "行锁等待已取消".into(),
                                };
                                session
                                    .cancel_sql_request(sql)
                                    .map_err(|error| error.to_string())?;
                                return Err(reason);
                            }
                        }
                    }
                    match session
                        .execute_sql_step(sql)
                        .map_err(|error| error.to_string())?
                    {
                        SqlStep::Complete(results) => Ok(Some(WireReply::ok(
                            bicdb_net::message::encode_statements(&proto::statements(&results)),
                        ))),
                        SqlStep::Waiting(wait) => {
                            suspended = Some(wait);
                            Ok(None)
                        }
                    }
                }
                WorkerAction::Describe(name) => {
                    let columns = session
                        .describe_columns(name)
                        .map_err(|error| error.to_string())?;
                    let rows: Vec<bicdb_net::Column> = columns
                        .into_iter()
                        .map(|(name, nullable, type_code, length)| bicdb_net::Column {
                            name,
                            kind: proto::kind_char(bicdb_sql::plan::kind_of(type_code)),
                            nullable,
                            type_code,
                            length,
                            type_name: type_name(type_code, length),
                        })
                        .collect();
                    Ok(Some(WireReply::ok(bicdb_net::message::encode_columns(
                        &rows,
                    ))))
                }
                WorkerAction::Sql { .. } => unreachable!("parsed before SQL execution"),
            });
        let (response, error) = match result {
            Ok(response) => (response, None),
            Err(error) => (
                Some(WireReply::error(error.clone().into_bytes())),
                Some(error),
            ),
        };
        let elapsed_us = started.elapsed().as_micros();
        session.suspend_state(&mut state);
        drop(session);
        if let Err(error) = catalog.close() {
            let error = format!("关闭请求字典文件失败：{error}");
            let _ = request
                .reply
                .send(WireReply::error(error.clone().into_bytes()));
            let _ = events.send(ServiceEvent::Completed {
                id: request.id,
                workspace: request.workspace,
                state,
                elapsed_us,
                error: Some(error),
            });
            continue;
        }
        if let Some(wait) = suspended {
            request.state = state;
            let _ = events.send(ServiceEvent::Suspended {
                request,
                wait,
                elapsed_us,
            });
            continue;
        }
        let _ = request.reply.send(response.expect("completed response"));
        let _ = events.send(ServiceEvent::Completed {
            id: request.id,
            workspace: request.workspace,
            state,
            elapsed_us,
            error,
        });
    }
}

fn is_transaction_end(action: &WorkerAction) -> bool {
    let WorkerAction::Sql { sql, params } = action else {
        return false;
    };
    if !params.is_empty() || sql.len() > 256 {
        return false;
    }
    let Ok(statements) = bicdb_sql::parser::parse_many(sql) else {
        return false;
    };
    matches!(statements.as_slice(), [bicdb_sql::ast::Stmt::Transaction(transaction)]
        if matches!(transaction.kind, bicdb_sql::ast::TransactionStmtKind::Commit
            | bicdb_sql::ast::TransactionStmtKind::Rollback))
}

struct ParkedRequest {
    request: WorkerRequest,
    wait: RowOwnerWait,
    slot: crate::scheduler::WaitSlot<[u8; 8]>,
}

fn mark_request_cancelled(request: &mut WorkerRequest, wait: &mut RowOwnerWait, reason: &str) {
    wait.cancel();
    if let WorkerAction::Parsed { cancel, .. } = &mut request.action {
        *cancel = Some(reason.into());
    }
}

/// No disk I/O, WAL locks, page pins or busy loops in the controller.
fn resume_row_waits(
    parked: &mut BTreeMap<u64, ParkedRequest>,
    scheduler: &mut crate::scheduler::Scheduler<[u8; 8], WorkerRequest>,
    work: &mpsc::Sender<WorkerRequest>,
    stopping: bool,
) -> Result<(), ServiceError> {
    let mut ready = Vec::new();
    for (id, entry) in parked.iter_mut() {
        if stopping {
            mark_request_cancelled(&mut entry.request, &mut entry.wait, "实例正在停止");
            ready.push(*id);
            continue;
        }
        match entry.request.runtime.engine.poll_row_wait(&mut entry.wait) {
            Ok(RowWaitStatus::Ready) => ready.push(*id),
            Ok(RowWaitStatus::Pending) if entry.wait.needs_header() => ready.push(*id),
            Ok(RowWaitStatus::Pending) => {}
            outcome => {
                let reason = match outcome {
                    Err(error) => error.to_string(),
                    _ => "行锁等待已取消".into(),
                };
                mark_request_cancelled(&mut entry.request, &mut entry.wait, &reason);
                ready.push(*id);
            }
        }
    }
    ready.sort_by_key(|id| {
        let entry = &parked[id];
        (entry.request.workspace, entry.wait.registration_order())
    });
    for id in ready {
        let mut entry = parked.remove(&id).expect("selected parked request");
        if let WorkerAction::Parsed { wait, .. } = &mut entry.request.action {
            *wait = Some(entry.wait);
        }
        scheduler
            .resume(entry.slot, entry.request)
            .map_err(|_| ServiceError::State("SQL resume accounting mismatch".into()))?;
    }
    dispatch_ready(scheduler, work)
}

fn dispatch_ready(
    scheduler: &mut crate::scheduler::Scheduler<[u8; 8], WorkerRequest>,
    work: &mpsc::Sender<WorkerRequest>,
) -> Result<(), ServiceError> {
    while let Some(dispatch) = scheduler.dispatch() {
        work.send(dispatch.request)
            .map_err(|_| ServiceError::State("SQL worker pool unexpectedly stopped".into()))?;
    }
    Ok(())
}

/// **守护进程主体**（由 `bicdb start` 以 `__daemon` 子命令拉起）。
///
/// 顺序不能换：**先取锁再打开实例**——反过来会有一段"实例被打开但没上锁"
/// 的窗口，另一个进程能挤进来。
pub fn run_daemon(opts: &StartOptions, foreground: bool) -> Result<(), ServiceError> {
    guard_daemon_launch()?;
    require_public_entry(opts)?;
    let mut log = LogFile::open(&opts.log)?;
    log.line(&format!(
        "bicdb {} 服务启动中（实例 {}，套接字 {}）",
        env!("CARGO_PKG_VERSION"),
        opts.dir.display(),
        opts.socket.display()
    ));

    // **参数文件 + 命令行 `-c`**：先装载（未知键/取值非法在此具名拒绝），
    // 再按它打开实例——与直连路径同一份参数语义。
    let params = &opts.params;
    log.line(&format!(
        "参数：池 {} 帧，自动扩展 {} 块，等锁 {} ms，死锁阈值 {} ms",
        params.run.pool_frames,
        params.run.file_extend_blocks,
        params.run.park_ms,
        params.run.deadlock_threshold_ms
    ));
    let lock = InstanceLock::acquire(&opts.dir, LockMode::Service, &opts.socket)?;
    log.line(&format!(
        "已取实例锁（pid {}，模式 service）",
        std::process::id()
    ));

    // **打开实例**（三阶段恢复）：可能耗时（重放），先记日志再开工。
    let mut inst = open_shared_with(params, Some(lock), None)?;
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
    listener.set_nonblocking(true)?;
    log.line(&format!("控制套接字已就绪：{}", opts.socket.display()));
    log.line("READY");

    let private_auth = crate::home::Home::locate()
        .ok()
        .map(|home| PublicAuthenticator {
            public_root: home.public_dir(),
            private_root: opts.dir.clone(),
            home: home.root,
            io: inst.io,
        });
    let fault_audit = bicdb_storage::fault_audit::FaultAuditWriter::start(inst.io)?;
    let fault_sink = fault_audit.sink();
    fault_sink.register(inst.ws_ref, &inst.dir.join("recovery.audit"))?;
    let delivery = fault_sink.clone();
    inst.pool
        .set_fault_handler(Arc::new(move |workspace, reason| {
            delivery.record(workspace, reason)
        }))?;
    let background = crate::background::Background::start(
        inst.pool,
        inst.shared_cache
            .as_ref()
            .map(|cache| Arc::clone(&cache.wal)),
        params.run.lgwr_threads,
        params.run.checkpoint_threads,
        params.run.undo_threads,
        Duration::from_millis(params.run.undo_interval_ms),
        Duration::from_millis(params.run.lgwr_interval_ms),
        Duration::from_millis(params.run.checkpoint_interval_ms),
        Duration::from_millis(params.run.dbwr_interval_ms),
    )?;
    background.register(inst.ws_ref, inst.engine);
    let mut workspaces = BTreeMap::<[u8; 8], crate::boot::Instance>::new();
    // Failed private recovery is an instance state, not a transient login
    // failure. Keep the first cause so repeated BIND requests do not rerun a
    // potentially destructive/expensive recovery attempt or append duplicate
    // Failed audit records. PUBLIC never enters this map: it is opened before
    // READY and remains the instance hard gate.
    let mut recovery_required = RecoveryRequiredRegistry::default();
    let mut served: u64 = 0;
    let mut sql_elapsed_us: u128 = 0;
    let started = Instant::now();
    let public_fulltext = FulltextScheduler::new(
        params.run.fulltext_interval_ms,
        params.run.fulltext_batch_rows,
    )
    .map_err(|e| ServiceError::State(e.to_string()))?;
    let mut fulltext = BTreeMap::from([(inst.ws_ref, public_fulltext)]);
    let (event_tx, event_rx) = mpsc::channel();
    let (work_tx, work_rx) = mpsc::channel();
    let work_rx = Arc::new(Mutex::new(work_rx));
    let mut worker_handles = Vec::with_capacity(params.run.worker_threads);
    for worker in 0..params.run.worker_threads {
        let receiver = Arc::clone(&work_rx);
        let events = event_tx.clone();
        let worker_params = params.clone();
        let handle = std::thread::Builder::new()
            .name(format!("bicdb-sql-{worker}"))
            .spawn(move || {
                run_sql_worker(worker_params, receiver, events);
            })
            .map_err(ServiceError::Io)?;
        worker_handles.push(handle);
    }
    let mut scheduler = crate::scheduler::Scheduler::new(crate::scheduler::Limits {
        workers: params.run.worker_threads,
        instance_queue: params.run.execution_queue_capacity,
        active_per_workspace: params.run.max_active_per_workspace,
        queue_per_workspace: params.run.workspace_queue_capacity,
    })
    .map_err(|error| ServiceError::State(error.into()))?;
    let mut parked = BTreeMap::<u64, ParkedRequest>::new();
    let mut connections = BTreeMap::<u64, ConnectionSession>::new();
    let mut next_connection = 1u64;
    let mut pending_fast_stop = false;
    let mut fast_stop_started: Option<Instant> = None;
    let mut fast_stop_reported = false;
    let mut next_maintenance = Instant::now();
    let stop_after = 'service: loop {
        resume_row_waits(&mut parked, &mut scheduler, &work_tx, pending_fast_stop)?;
        if !pending_fast_stop && Instant::now() >= next_maintenance {
            if scheduler.idle_in(inst.ws_ref)
                && !workspace_has_open_transaction(&connections, inst.ws_ref, inst.ws_ref)
            {
                maintain_workspace_fulltext(&mut inst, &mut fulltext, params, &mut log)?;
            }
            for (workspace, target) in &mut workspaces {
                if scheduler.idle_in(*workspace)
                    && !workspace_has_open_transaction(&connections, inst.ws_ref, *workspace)
                {
                    maintain_workspace_fulltext(target, &mut fulltext, params, &mut log)?;
                }
            }
            next_maintenance = Instant::now() + Duration::from_millis(100);
        }
        if pending_fast_stop {
            let execution = scheduler.metrics();
            if execution.active == 0 && execution.queued == 0 && execution.parked == 0 {
                break 'service StopMode::Fast;
            }
            if !fast_stop_reported
                && fast_stop_started.is_some_and(|started| {
                    started.elapsed() >= Duration::from_secs(params.run.stop_wait_s)
                })
            {
                log.line(&format!(
                    "fast 停机排空超过 {} 秒：active={} queued={} parked={}；实例继续保留恢复信息并等待，可由管理员另行选择 immediate",
                    params.run.stop_wait_s,
                    execution.active,
                    execution.queued,
                    execution.parked,
                ));
                fast_stop_reported = true;
            }
        }
        // Bound accept work so an incoming connection stream cannot starve
        // queued SQL completions, STATUS, shutdown or maintenance.
        for _ in 0..64 {
            if pending_fast_stop {
                break;
            }
            let stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => {
                    log.line(&format!("接受连接失败：{error}"));
                    break;
                }
            };
            if connections.len() >= params.run.max_connections {
                let mut stream = stream;
                let _ = frame::write_frame_bytes(
                    &mut stream,
                    "ERR",
                    format!(
                        "实例正忙：活动连接已达到服务上限 {}",
                        params.run.max_connections
                    )
                    .as_bytes(),
                );
                continue;
            }
            let id = next_connection;
            next_connection = next_connection
                .checked_add(1)
                .ok_or_else(|| ServiceError::State("服务连接号耗尽".into()))?;
            connections.insert(
                id,
                ConnectionSession {
                    cancelled: Arc::new(AtomicBool::new(false)),
                    state: Some(SessionState::new(inst.seq())),
                    sql_served: false,
                    authed: None,
                    auth_failed: false,
                    workspace: None,
                },
            );
            let events = event_tx.clone();
            std::thread::Builder::new()
                .name(format!("bicdb-client-{id}"))
                .spawn(move || serve_connection(id, stream, events))
                .map_err(ServiceError::Io)?;
        }

        let event = match event_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(event) => event,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(ServiceError::State("服务连接调度器意外关闭".into()));
            }
        };
        match event {
            ServiceEvent::WorkspaceOpen {
                workspace_id,
                name,
                root,
                mode,
                reply,
            } => {
                let result = (|| -> Result<String, String> {
                    let id = bicdb_workspace::WorkspaceId::from_raw(workspace_id)
                        .ok_or_else(|| format!("工作区号 {workspace_id} 无效"))?;
                    let ws = bicdb_workspace::workspace_ref(id);
                    let expected_root = root.canonicalize().map_err(|error| error.to_string())?;
                    let public_root = inst.dir.canonicalize().map_err(|error| error.to_string())?;
                    let is_public = ws == inst.ws_ref;
                    if is_public && expected_root != public_root {
                        return Err("PUBLIC 工作区路径与运行实例不匹配".into());
                    }
                    if is_public && mode == bicdb_sql::WorkspaceOpenMode::ReadWriteForce {
                        return Err("PUBLIC 禁止 FORCE：必须修复一致性后再开放写入".into());
                    }
                    let target_active = scheduler.active_in(ws);
                    let busy = if is_public {
                        target_active > 1 // this OPEN owns one PUBLIC worker
                    } else {
                        target_active != 0
                    };
                    if busy {
                        return Err(format!("工作区 `{name}` 尚有正在执行的 SQL，稍后重试 OPEN"));
                    }

                    if !is_public && !workspaces.contains_key(&ws) {
                        bound_workspace_capacity(
                            workspaces.len(),
                            params.run.max_bound_workspaces,
                            false,
                        )?;
                        let ini = expected_root.join("bicdb.ini");
                        let (mut workspace_params, _) =
                            crate::config::InstanceParams::load_with_overrides(Some(&ini), &[])
                                .map_err(|error| error.to_string())?;
                        if workspace_params
                            .db_root
                            .canonicalize()
                            .map_err(|error| error.to_string())?
                            != expected_root
                        {
                            return Err("工作区参数文件与 PUBLIC 注册表不匹配".into());
                        }
                        let lock = InstanceLock::acquire(
                            &workspace_params.db_root,
                            LockMode::Service,
                            &opts.socket,
                        )
                        .map_err(|error| error.to_string())?;
                        workspace_params.run = params.run.clone();
                        // READ ONLY is the quarantine admission path: it may
                        // bypass only the same narrow, audited private
                        // watermark checks as explicit FORCE, but the write
                        // gate remains closed afterwards.
                        let opened = if workspace_open_uses_narrow_force(mode) {
                            open_shared_force_private(
                                &workspace_params,
                                Some(lock),
                                inst.shared_cache.clone(),
                            )
                        } else {
                            open_shared_with(
                                &workspace_params,
                                Some(lock),
                                inst.shared_cache.clone(),
                            )
                        };
                        let target = opened.map_err(|error| {
                            remember_recovery_failure(&mut recovery_required, ws, &error);
                            format!("工作区打开失败：{error}")
                        })?;
                        if target.ws_ref != ws || target.catalog.is_public() {
                            return Err("实际工作区与 OPEN 目标不匹配".into());
                        }
                        fault_sink
                            .register(ws, &target.dir.join("recovery.audit"))
                            .map_err(|error| error.to_string())?;
                        background.register(ws, target.engine);
                        workspaces.insert(ws, target);
                        recovery_required.clear(&ws);
                    }

                    let target = if is_public {
                        &mut inst
                    } else {
                        workspaces.get_mut(&ws).ok_or("工作区未打开")?
                    };
                    match mode {
                        bicdb_sql::WorkspaceOpenMode::ReadOnly => {
                            let reason = target.forced_recovery.as_ref().map_or_else(
                                || format!("由管理员执行 ALTER WORKSPACE {name} OPEN READ ONLY"),
                                |skipped| {
                                    format!("恢复检查发现不一致，已只读隔离；跳过项：{skipped}")
                                },
                            );
                            target.pool.set_workspace_read_only(ws, reason);
                            let suffix = target
                                .forced_recovery
                                .as_ref()
                                .map_or(String::new(), |skipped| {
                                    format!("；不一致范围已审计：{skipped}")
                                });
                            Ok(format!(
                                "ALTER WORKSPACE：`{name}` 已实际打开为 READ ONLY{suffix}"
                            ))
                        }
                        bicdb_sql::WorkspaceOpenMode::ReadWrite
                        | bicdb_sql::WorkspaceOpenMode::ReadWriteForce => {
                            if let Some(reason) = target.pool.workspace_fault(ws) {
                                return Err(format!(
                                    "工作区 `{name}` 存在耐久性故障，不能在已打开实例上清除：{reason}"
                                ));
                            }
                            validate_forced_write_transition(
                                target.forced_recovery.is_some(),
                                mode,
                            )
                            .map_err(|reason| format!("工作区 `{name}` {reason}"))?;
                            target.pool.clear_workspace_read_only(ws);
                            let state = if target.forced_recovery.is_some() {
                                "READ WRITE FORCED"
                            } else {
                                "READ WRITE"
                            };
                            Ok(format!(
                                "ALTER WORKSPACE：`{name}` 已通过恢复门并实际打开为 {state}"
                            ))
                        }
                    }
                })();
                let _ = reply.send(result);
            }
            ServiceEvent::WorkspaceVerify {
                workspace_id,
                name,
                root,
                scope,
                reply,
            } => {
                let result = (|| -> Result<String, String> {
                    use bicdb_storage::recovery_journal::RecoveryScope;
                    let id = bicdb_workspace::WorkspaceId::from_raw(workspace_id)
                        .ok_or_else(|| format!("工作区号 {workspace_id} 无效"))?;
                    let ws = bicdb_workspace::workspace_ref(id);
                    if ws == inst.ws_ref {
                        return Err(
                            "PUBLIC 的结构化隔离必须在离线修复模式验证，在线命令不得绕过启动一致性门"
                                .into(),
                        );
                    }
                    let expected_root = root.canonicalize().map_err(|error| error.to_string())?;
                    let target = workspaces
                        .get_mut(&ws)
                        .ok_or_else(|| format!("工作区 `{name}` 尚未 OPEN READ ONLY"))?;
                    if target
                        .dir
                        .canonicalize()
                        .map_err(|error| error.to_string())?
                        != expected_root
                    {
                        return Err("恢复验证目标与已打开工作区路径不匹配".into());
                    }
                    if scheduler.active_in(ws) != 0 {
                        return Err(format!("工作区 `{name}` 尚有正在执行的 SQL，稍后重试验证"));
                    }
                    if target.pool.workspace_read_only(ws).is_none() {
                        return Err(format!(
                            "工作区 `{name}` 必须先 OPEN READ ONLY 才能验证恢复"
                        ));
                    }
                    let dirty = target.pool.dirty_len(ws);
                    if dirty != 0 {
                        return Err(format!(
                            "工作区 `{name}` 尚有 {dirty} 个脏页，不能以磁盘像作为修复证据"
                        ));
                    }

                    let (audit_scope, detail) = match scope {
                        bicdb_sql::RecoveryVerifyScope::Page { file_id, block_id } => {
                            let rdba = bicdb_storage::rowid::Rdba::from_parts(file_id, block_id)
                                .ok_or_else(|| format!("页面地址 {file_id}:{block_id} 无效"))?;
                            let key = bicdb_storage::buffer::BufferKey::new(ws, rdba);
                            let previous = target.pool.page_fault(key).ok_or_else(|| {
                                format!("页面 {file_id}:{block_id} 不在恢复隔离清单中")
                            })?;
                            target
                                .pool
                                .verify_quarantined_page(key)
                                .map_err(|error| format!("页面介质验证失败：{error}"))?;
                            let detail = format!(
                                "管理员在线验证页面 {file_id}:{block_id} 的校验和、地址与工作区身份通过；原隔离原因：{previous}"
                            );
                            fault_sink
                                .append_verified(
                                    ws,
                                    0,
                                    RecoveryScope::Page { file_id, block_id },
                                    "bicdb/admin",
                                    &detail,
                                )
                                .map_err(|error| format!("Verified 审计落盘失败：{error}"))?;
                            if !target.pool.clear_verified_page_quarantine(key) {
                                return Err("Verified 已落盘，但运行时页面隔离项意外消失；请重启实例重放审计".into());
                            }
                            (RecoveryScope::Page { file_id, block_id }, detail)
                        }
                        bicdb_sql::RecoveryVerifyScope::Object { object_id } => {
                            let previous = target
                                .pool
                                .object_fault(ws, u64::from(object_id))
                                .ok_or_else(|| format!("对象 {object_id} 不在恢复隔离清单中"))?;
                            let snapshot = bicdb_common::seq::CommitSeq::from_raw(target.seq())
                                .ok_or_else(|| "当前提交序号超出 48 位范围".to_owned())?;
                            let object = target
                                .catalog
                                .resolve_by_obj(snapshot, object_id)
                                .map_err(|error| format!("对象目录验证失败：{error}"))?;
                            if object.status == 0 || object.dataobj == 0 {
                                return Err(format!(
                                    "对象 {object_id} 不是可验证的活动持久段（status={}，dataobj={}）",
                                    object.status, object.dataobj
                                ));
                            }
                            let file_id = target.catalog.file_id();
                            let segment_block = target
                                .catalog
                                .segment_block_by_dataobj(object.dataobj)
                                .map_err(|error| format!("对象段映射验证失败：{error}"))?;
                            let blocks = {
                                let segment = target
                                    .catalog
                                    .segment_at(segment_block)
                                    .map_err(|error| format!("对象段头验证失败：{error}"))?;
                                if segment.header().obj != object.obj
                                    || segment.header().dataobj != object.dataobj
                                {
                                    return Err(format!(
                                        "对象段头身份不符：目录 obj/dataobj={}/{}，段头={}/{}",
                                        object.obj,
                                        object.dataobj,
                                        segment.header().obj,
                                        segment.header().dataobj
                                    ));
                                }
                                let append = segment
                                    .append_position()
                                    .map_err(|error| format!("对象段追加位置验证失败：{error}"))?;
                                segment.data_blocks(segment.hwm().max(append))
                            };
                            for block in
                                std::iter::once(segment_block).chain(blocks.iter().copied())
                            {
                                let rdba = bicdb_storage::rowid::Rdba::from_parts(file_id, block)
                                    .ok_or_else(|| {
                                    format!("对象 {object_id} 含无效块 {block}")
                                })?;
                                let guard = target
                                    .pool
                                    .pin(bicdb_storage::buffer::BufferKey::new(ws, rdba))
                                    .map_err(|error| {
                                        format!("对象 {object_id} 的块 {file_id}:{block} 验证失败：{error}")
                                    })?;
                                drop(guard);
                            }
                            let detail = format!(
                                "管理员在线验证对象 {object_id}（{}）的目录、段头及 {} 个数据块通过；原隔离原因：{previous}",
                                object.name,
                                blocks.len()
                            );
                            fault_sink
                                .append_verified(
                                    ws,
                                    0,
                                    RecoveryScope::Object {
                                        object_id: u64::from(object_id),
                                    },
                                    "bicdb/admin",
                                    &detail,
                                )
                                .map_err(|error| format!("Verified 审计落盘失败：{error}"))?;
                            if !target
                                .pool
                                .clear_verified_object_quarantine(ws, u64::from(object_id))
                            {
                                return Err("Verified 已落盘，但运行时对象隔离项意外消失；请重启实例重放审计".into());
                            }
                            (
                                RecoveryScope::Object {
                                    object_id: u64::from(object_id),
                                },
                                detail,
                            )
                        }
                    };
                    Ok(format!(
                        "ALTER WORKSPACE：`{name}` 已验证并解除 {audit_scope:?} 隔离；{detail}"
                    ))
                })();
                let _ = reply.send(result);
            }
            ServiceEvent::Suspended {
                mut request,
                mut wait,
                elapsed_us,
            } => {
                sql_elapsed_us += elapsed_us;
                if pending_fast_stop || !connections.contains_key(&request.id) {
                    mark_request_cancelled(&mut request, &mut wait, "连接关闭或实例正在停止");
                    // Still owns its active slot; cleanup must run on a worker.
                    work_tx
                        .send(request)
                        .map_err(|_| ServiceError::State("SQL worker pool stopped".into()))?;
                } else {
                    match scheduler.suspend(request.workspace) {
                        Ok(slot) => {
                            if parked
                                .insert(
                                    request.id,
                                    ParkedRequest {
                                        request,
                                        wait,
                                        slot,
                                    },
                                )
                                .is_some()
                            {
                                return Err(ServiceError::State(
                                    "duplicate parked SQL request".into(),
                                ));
                            }
                        }
                        Err(crate::scheduler::SuspendError::Full(_)) => {
                            mark_request_cancelled(
                                &mut request,
                                &mut wait,
                                "行锁等待队列已满，请稍后重试",
                            );
                            work_tx.send(request).map_err(|_| {
                                ServiceError::State("SQL worker pool stopped".into())
                            })?;
                        }
                        Err(_) => {
                            return Err(ServiceError::State(
                                "SQL suspension accounting mismatch".into(),
                            ))
                        }
                    }
                }
                dispatch_ready(&mut scheduler, &work_tx)?;
            }
            ServiceEvent::Closed(id) => {
                if let Some(connection) = connections.get(&id) {
                    connection.cancelled.store(true, Ordering::Release);
                }
                if let Some(mut waiting) = parked.remove(&id) {
                    mark_request_cancelled(&mut waiting.request, &mut waiting.wait, "连接已断开");
                    scheduler
                        .resume(waiting.slot, waiting.request)
                        .map_err(|_| {
                            ServiceError::State("SQL resume accounting mismatch".into())
                        })?;
                    dispatch_ready(&mut scheduler, &work_tx)?;
                }
                if let Some(mut connection) = connections.remove(&id) {
                    connection.cancelled.store(true, Ordering::Release);
                    if let Some(mut state) = connection.state.take() {
                        let target =
                            selected_instance(&mut inst, &mut workspaces, connection.workspace);
                        let seq = target.seq();
                        let mut session =
                            Session::new(target.pool, target.engine, &mut target.catalog, seq);
                        session.resume_state(&mut state);
                        drop(session); // rollback this connection's uncommitted transaction
                    }
                }
            }
            ServiceEvent::Completed {
                id,
                workspace,
                mut state,
                elapsed_us,
                error,
            } => {
                if !scheduler.complete(workspace) {
                    return Err(ServiceError::State(
                        "SQL scheduler completion accounting mismatch".into(),
                    ));
                }
                sql_elapsed_us += elapsed_us;
                if let Some(error) = error {
                    log.line(&format!("语句失败：{error}"));
                }
                if let Some(connection) = connections.get_mut(&id) {
                    if connection.state.replace(state).is_some() {
                        return Err(ServiceError::State(
                            "connection received duplicate SQL completion".into(),
                        ));
                    }
                } else {
                    let target = selected_instance(&mut inst, &mut workspaces, Some(workspace));
                    let seq = target.seq();
                    let mut session =
                        Session::new(target.pool, target.engine, &mut target.catalog, seq);
                    session.resume_state(&mut state);
                    drop(session);
                }
                dispatch_ready(&mut scheduler, &work_tx)?;
            }
            ServiceEvent::Request {
                id,
                verb,
                payload,
                reply,
            } => {
                let connection_count = connections.len();
                let Some(connection) = connections.get_mut(&id) else {
                    let _ = reply.send(WireReply::error(b"connection is closed".to_vec()));
                    continue;
                };
                if verb == "HELLO" {
                    let _ = reply.send(WireReply::ok(
                        Hello {
                            wire: WIRE_VERSION,
                            version: env!("CARGO_PKG_VERSION").to_owned(),
                            instance: opts.dir.display().to_string(),
                        }
                        .encode()
                        .into_bytes(),
                    ));
                    continue;
                }
                if pending_fast_stop && verb != "STATUS" {
                    let _ = reply.send(WireReply::error(
                        "实例正在排空执行队列并停止".as_bytes().to_vec(),
                    ));
                    continue;
                }
                if verb == "STATUS" {
                    let bound_workspaces = workspaces.len() + 1;
                    let inst = selected_instance(&mut inst, &mut workspaces, connection.workspace);
                    let cache = inst.pool.stats();
                    let execution = scheduler.metrics();
                    let _ = reply.send(WireReply::ok({
                        let mut body = format!(
                            "instance={}\nversion={}\npid={}\nuptime_s={}\nserved={served}\nseq={}\nmode=service\nwire={}\nconnections={connection_count}\nfile_handles={}\nrecovery_required_workspaces={}\nshutdown_state={}\nshutdown_elapsed_ms={}\nmax_connections={}\nmax_bound_workspaces={}\nbound_workspaces={}\nworker_threads={}\nexecution_active={}\nexecution_queued={}\nexecution_parked={}\nexecution_ready_workspaces={}\nexecution_queue_capacity={}\nmax_active_per_workspace={}\nworkspace_queue_capacity={}\ncontrol_workers={}\nworkspace_kind={}\nidentity={}\ncache_frames={}\ncache_bytes={}\ncache_resident={}\ncache_hits={}\ncache_misses={}\ncache_evictions={}\ncache_writes={}\ncache_dirty_pages={}\ncache_free_buffer_waits={}\ncache_wal_syncs={}\ncache_run_reads={}\ncache_run_pages={}\ngraph_detach_edge_limit={}\ngraph_max_nodes={}\ngraph_max_edges={}\ngraph_max_rows={}\ngraph_max_expansions={}\ngraph_max_edge_expansions={}\ngraph_max_elapsed_ms={}\ngraph_max_depth={}\ngraph_max_text_bytes={}\nbackground_failures={}\nfault_audit_failures={}\nfault_audit_state={}\nsql_elapsed_us={sql_elapsed_us}\n",
                            opts.dir.display(), env!("CARGO_PKG_VERSION"), std::process::id(),
                            started.elapsed().as_secs(), inst.seq(), WIRE_VERSION,
                            inst.io.open_handle_count().to_string(),
                            recovery_required.len(),
                            if pending_fast_stop { "DRAINING" } else { "RUNNING" },
                            fast_stop_started.map_or(0, |started| started.elapsed().as_millis()),
                            params.run.max_connections,
                            params.run.max_bound_workspaces, bound_workspaces,
                            params.run.worker_threads, execution.active, execution.queued, execution.parked,
                            execution.ready_workspaces, params.run.execution_queue_capacity,
                            params.run.max_active_per_workspace,
                            params.run.workspace_queue_capacity, params.run.control_workers,
                            if inst.catalog.is_public() { "public" } else { "private" },
                            connection.authed.as_deref().unwrap_or("管理面（本机/OS 身份）"),
                            inst.pool.capacity() * inst.pool.partition_count(),
                            inst.pool.capacity() * inst.pool.partition_count() * bicdb_storage::page::PAGE_SIZE,
                            inst.pool.resident(), cache.hits, cache.misses, cache.evictions,
                            cache.writes, inst.pool.dirty_len(inst.ws_ref), cache.fb_wait,
                            cache.wal_syncs, cache.run_reads, cache.run_pages,
                            params.run.graph_detach_edge_limit, params.run.graph_max_nodes,
                            params.run.graph_max_edges, params.run.graph_max_rows,
                            params.run.graph_max_expansions, params.run.graph_max_edge_expansions,
                            params.run.graph_max_elapsed_ms, params.run.graph_max_depth,
                            params.run.graph_max_text_bytes, background.failures(), fault_sink.failures(),
                            format!("{:?}", fault_sink.status(connection.workspace.unwrap_or(inst.ws_ref))).replace('\n', " "),
                        );
                        let writes = background.dbwr_stats();
                        body.push_str(&format!(
                            "cache_partitions={}\ndbwr_threads={}\ndbwr_passes={}\ndbwr_pages_written={}\ndbwr_failures={}\ndbwr_deferred_pages={}\ndbwr_pinned_pages={}\ndbwr_in_flight_pages={}\ndbwr_redo_pending_pages={}\n",
                            inst.pool.partition_count(), background.dbwr_writers(),
                            writes.passes, writes.pages_written, writes.failures,
                            writes.deferred_pages, writes.pinned_pages,
                            writes.in_flight_pages, writes.redo_pending_pages,
                        ));
                        let (lgwr, ckpt, undo) = background.worker_counts();
                        body.push_str(&format!("lgwr_threads={lgwr}\ncheckpoint_threads={ckpt}\nundo_threads={undo}\n"));
                        let fault = inst.pool.workspace_fault(inst.ws_ref);
                        let write_state = if fault.is_some() {
                            "FAULT_READ_ONLY"
                        } else if inst.pool.workspace_read_only(inst.ws_ref).is_some() {
                            "READ_ONLY"
                        } else if inst.forced_recovery.is_some() {
                            "READ_WRITE_FORCED"
                        } else {
                            "READ_WRITE"
                        };
                        body.push_str(&format!("workspace_write_state={write_state}\n"));
                        body.push_str(&format!(
                            "quarantined_pages={}\nquarantined_objects={}\n",
                            inst.pool.quarantined_page_count(inst.ws_ref),
                            inst.pool.quarantined_object_count(inst.ws_ref),
                        ));
                        if let Some(reason) = &inst.forced_recovery {
                            body.push_str(&format!("workspace_forced_reason={}\n", reason.replace(['\n', '\r'], " ")));
                        }
                        if let Some(reason) = inst.pool.workspace_read_only(inst.ws_ref) {
                            body.push_str(&format!("workspace_read_only_reason={}\n", reason.replace(['\n', '\r'], " ")));
                        }
                        if let Some(reason) = fault { body.push_str(&format!("workspace_fault={}\n", reason.replace(['\n', '\r'], " "))); }
                        let checkpoint = background.checkpoint_state(inst.ws_ref);
                        body.push_str(&format!(
                            "checkpoint_attempts={}\ncheckpoint_deferred={}\ncheckpoint_lsn={}\n",
                            checkpoint.attempts, checkpoint.deferred,
                            checkpoint.progress.map_or_else(|| "unknown".into(),
                                |progress| progress.checkpoint_lsn.as_raw().to_string()),
                        ));
                        body.into_bytes()
                    }));
                    continue;
                }

                if verb == "BIND" {
                    let result = (|| -> Result<bicdb_net::message::OwnedWorkspace, String> {
                        if connection.auth_failed || connection.authed.is_none() {
                            return Err("BIND 必须先通过 PUBLIC AUTH".into());
                        }
                        if connection.workspace.is_some() {
                            return Err("本连接已经绑定工作区；切换需新建连接".into());
                        }
                        let selection =
                            std::str::from_utf8(&payload).map_err(|_| "BIND 请求必须是 UTF-8")?;
                        let state = connection.state.as_mut().ok_or("本连接有请求正在执行")?;
                        if state.in_transaction() {
                            return Err("事务期间不能绑定工作区".into());
                        }
                        let mut public = Session::new(
                            inst.pool,
                            inst.engine,
                            &mut inst.catalog,
                            inst.engine.current_seq(),
                        );
                        public.resume_state(state);
                        public.set_dcl_context(
                            crate::home::Home::locate().ok().map(|h| h.root),
                            Some(inst.io),
                        );
                        let route = public.route_owned_workspace(if selection.is_empty() {
                            None
                        } else {
                            Some(selection)
                        });
                        public.suspend_state(state);
                        drop(public);
                        let (user_id, workspace_id, name, root) =
                            route.map_err(|e| e.to_string())?;
                        let ws = bicdb_workspace::workspace_ref(
                            bicdb_workspace::WorkspaceId::from_raw(workspace_id)
                                .ok_or("工作区号无效")?,
                        );
                        if !workspaces.contains_key(&ws) {
                            bound_workspace_capacity(
                                workspaces.len(),
                                params.run.max_bound_workspaces,
                                false,
                            )?;
                            if let Some(reason) = recovery_required.cause(&ws) {
                                return Err(format!(
                                    "工作区处于 RECOVERY REQUIRED，须先修复或由实例管理员执行显式恢复命令：{reason}"
                                ));
                            }
                            let ini = Path::new(&root).join("bicdb.ini");
                            let (mut workspace_params, _) =
                                crate::config::InstanceParams::load_with_overrides(Some(&ini), &[])
                                    .map_err(|e| e.to_string())?;
                            if workspace_params
                                .db_root
                                .canonicalize()
                                .map_err(|e| e.to_string())?
                                != Path::new(&root).canonicalize().map_err(|e| e.to_string())?
                            {
                                return Err("工作区参数文件与 PUBLIC 注册表不匹配".into());
                            }
                            let lock = InstanceLock::acquire(
                                &workspace_params.db_root,
                                LockMode::Service,
                                &opts.socket,
                            )
                            .map_err(|e| e.to_string())?;
                            workspace_params.run = params.run.clone();
                            let opened = if opts.force {
                                open_shared_force_private(
                                    &workspace_params,
                                    Some(lock),
                                    inst.shared_cache.clone(),
                                )
                            } else {
                                open_shared_with(
                                    &workspace_params,
                                    Some(lock),
                                    inst.shared_cache.clone(),
                                )
                            };
                            let target = match opened {
                                Ok(target) => target,
                                Err(error) => {
                                    let reason = error.to_string();
                                    remember_recovery_failure(&mut recovery_required, ws, &error);
                                    return Err(format!("工作区打开失败：{reason}"));
                                }
                            };
                            if target.ws_ref != ws || target.catalog.is_public() {
                                return Err("实际工作区与绑定目标不匹配".into());
                            }
                            fault_sink
                                .register(ws, &target.dir.join("recovery.audit"))
                                .map_err(|error| error.to_string())?;
                            background.register(ws, target.engine);
                            workspaces.insert(ws, target);
                        }
                        connection.workspace = Some(ws);
                        Ok(bicdb_net::message::OwnedWorkspace {
                            user_id,
                            workspace_id,
                            name,
                            root,
                        })
                    })();
                    let response = match result {
                        Ok(route) => WireReply::ok(route.encode().into_bytes()),
                        // BIND errors preserve the existing authenticated identity
                        // and binding; only AUTH may invalidate authentication.
                        Err(error) => WireReply::error(error.into_bytes()),
                    };
                    let _ = reply.send(response);
                    continue;
                }

                if matches!(verb.as_str(), "SQL" | "DESCRIBE") {
                    if connection.auth_failed {
                        let _ = reply.send(WireReply::error(
                            "认证失败的连接不能执行语句；请重新认证或重连"
                                .as_bytes()
                                .to_vec(),
                        ));
                        continue;
                    }
                    let action = if verb == "SQL" {
                        served += 1;
                        connection.sql_served = true;
                        match SqlRequest::decode(&payload) {
                            Ok(request) => WorkerAction::Sql {
                                sql: request.sql,
                                params: proto::engine_params(&request.params),
                            },
                            Err(error) => {
                                log.line(&format!("请求载荷非法：{error}"));
                                let _ = reply.send(WireReply::error(
                                    format!("请求载荷非法：{error}").into_bytes(),
                                ));
                                continue;
                            }
                        }
                    } else {
                        connection.sql_served = true;
                        WorkerAction::Describe(String::from_utf8_lossy(&payload).trim().to_owned())
                    };
                    let Some(state) = connection.state.take() else {
                        let _ = reply.send(WireReply::error(
                            "本连接已有请求正在执行".as_bytes().to_vec(),
                        ));
                        continue;
                    };
                    let target =
                        selected_instance(&mut inst, &mut workspaces, connection.workspace);
                    let request = WorkerRequest {
                        cancelled: Arc::clone(&connection.cancelled),
                        id,
                        workspace: target.ws_ref,
                        runtime: WorkspaceExecution::from_instance(target),
                        state,
                        action,
                        reply,
                    };
                    let transaction_end =
                        request.state.in_transaction() && is_transaction_end(&request.action);
                    let admission = if transaction_end {
                        scheduler.enqueue_transaction_end(
                            target.ws_ref,
                            request,
                            params.run.max_connections,
                        )
                    } else {
                        scheduler.enqueue(target.ws_ref, request)
                    };
                    match admission {
                        Ok(()) => dispatch_ready(&mut scheduler, &work_tx)?,
                        Err((reason, request)) => {
                            connection.state = Some(request.state);
                            let message = match reason {
                                crate::scheduler::EnqueueError::InstanceQueueFull => {
                                    "实例执行队列已满，请稍后重试"
                                }
                                crate::scheduler::EnqueueError::WorkspaceQueueFull => {
                                    "当前工作区执行队列已满，请稍后重试"
                                }
                            };
                            let _ = request
                                .reply
                                .send(WireReply::error(message.as_bytes().to_vec()));
                        }
                    }
                    continue;
                }

                let Some(mut state) = connection.state.take() else {
                    let _ = reply.send(WireReply::error(
                        "本连接已有请求正在执行".as_bytes().to_vec(),
                    ));
                    continue;
                };
                let seq = inst.seq();
                state.refresh_committed(seq);
                // Keep the request-local fixed-table provider alive longer
                // than Session (locals drop in reverse declaration order).
                let fixed = crate::fixed::CliFixedTables::new(&inst.dir, inst.io);
                let mut session = Session::new(inst.pool, inst.engine, &mut inst.catalog, seq);
                session.resume_state(&mut state);
                session
                    .set_fulltext_defaults(
                        params.run.fulltext_interval_ms,
                        params.run.fulltext_batch_rows,
                    )
                    .map_err(|e| ServiceError::State(e.to_string()))?;
                session
                    .set_graph_limits(params.run.graph_limits())
                    .map_err(|e| ServiceError::State(e.to_string()))?;
                session.set_dcl_context(
                    crate::home::Home::locate().ok().map(|h| h.root),
                    Some(inst.io),
                );
                session.set_workspace_provisioner(Some(provisioner_static()));
                session.set_pbkdf2_iterations(inst.params.run.pbkdf2_iterations);
                if !session.on_public_workspace() {
                    session.set_private_authenticator(
                        private_auth
                            .as_ref()
                            .map(|provider| provider as &dyn bicdb_sql::auth::PrivateAuthProvider),
                    );
                }
                session.set_fixed_table_source(Some(&fixed));

                let mut requested_stop = None;
                let response = match verb.as_str() {
                    "AUTH" => {
                        if connection.sql_served || connection.workspace.is_some() {
                            WireReply::error("AUTH 必须是本连接上的第一个业务请求（本连接已执行过语句）——重连再认证".as_bytes().to_vec())
                        } else {
                            match AuthRequest::decode(&payload) {
                                Err(error) => {
                                    connection.auth_failed = true;
                                    log.line(&format!("AUTH 载荷非法：{error}"));
                                    WireReply::error(
                                        format!("AUTH 载荷非法：{error}（内容不回报——可能含口令）")
                                            .into_bytes(),
                                    )
                                }
                                Ok(request) => {
                                    match session.authenticate(&request.user, &request.password) {
                                        Ok(identity) => {
                                            log.line(&format!("认证成功：{}", identity.describe()));
                                            connection.authed = Some(identity.name().to_owned());
                                            connection.auth_failed = false;
                                            WireReply::ok(
                                                AuthOk {
                                                    user: identity.name().to_owned(),
                                                    user_id: identity.user_id(),
                                                    expired: identity.is_expired(),
                                                }
                                                .encode()
                                                .into_bytes(),
                                            )
                                        }
                                        Err(error) => {
                                            connection.auth_failed = true;
                                            log.line(&format!(
                                                "认证失败：主体 `{}`（口令不记录）——{error}",
                                                request.user
                                            ));
                                            WireReply::error(error.to_string().into_bytes())
                                        }
                                    }
                                }
                            }
                        }
                    }
                    "ROUTE" => {
                        if connection.auth_failed {
                            WireReply::error("认证失败的连接不能路由工作区".as_bytes().to_vec())
                        } else {
                            let result = std::str::from_utf8(&payload)
                                .map_err(|_| "ROUTE 请求必须是 UTF-8".to_owned())
                                .and_then(|selection| {
                                    session
                                        .route_owned_workspace(if selection.is_empty() {
                                            None
                                        } else {
                                            Some(selection)
                                        })
                                        .map_err(|error| error.to_string())
                                });
                            match result {
                                Ok((user_id, workspace_id, name, root)) => WireReply::ok(
                                    bicdb_net::message::OwnedWorkspace {
                                        user_id,
                                        workspace_id,
                                        name,
                                        root,
                                    }
                                    .encode()
                                    .into_bytes(),
                                ),
                                Err(error) => WireReply::error(error.into_bytes()),
                            }
                        }
                    }
                    "SHUTDOWN" => {
                        if connection.auth_failed || !session.is_management_identity() {
                            WireReply::error("停止服务需要本机管理面身份".as_bytes().to_vec())
                        } else {
                            let mode = StopMode::parse(String::from_utf8_lossy(&payload).trim())
                                .unwrap_or(StopMode::Fast);
                            requested_stop = Some(mode);
                            log.line(&format!("收到停止请求（{}）", mode.as_str()));
                            let mut response = WireReply::ok(
                                format!("shutting down（{}）", mode.as_str()).into_bytes(),
                            );
                            response.close = true;
                            response
                        }
                    }
                    other => WireReply::error(format!("未知动词 `{other}`").into_bytes()),
                };
                session.suspend_state(&mut state);
                drop(session);
                connection.state = Some(state);
                let _ = reply.send(response);
                if let Some(mode) = requested_stop {
                    if mode == StopMode::Immediate {
                        break 'service mode;
                    }
                    pending_fast_stop = true;
                    fast_stop_started.get_or_insert_with(Instant::now);
                    continue;
                }
            }
        }
    };

    // Stop feeding workers and wait for a fast shutdown's already accepted
    // statements to leave their session states in the completion queue.
    if stop_after == StopMode::Fast {
        drop(work_tx);
        for handle in worker_handles {
            if handle.join().is_err() {
                log.line("SQL worker 异常退出");
            }
        }
    }

    if stop_after == StopMode::Immediate {
        // No rollback, checkpoint, background join or final flush in this path.
        std::mem::forget(background);
        std::mem::forget(fault_audit);
        log.line("immediate 停止：不回滚、不做完全检查点（下次打开走崩溃恢复）");
    } else {
        // All accepted worker requests have completed. Preserve background
        // writers until PUBLIC and every opened workspace are durably closed.
        for (_, mut connection) in connections {
            if let Some(mut state) = connection.state.take() {
                let target = selected_instance(&mut inst, &mut workspaces, connection.workspace);
                let seq = target.seq();
                let mut session =
                    Session::new(target.pool, target.engine, &mut target.catalog, seq);
                session.resume_state(&mut state);
                session.rollback_uncommitted().map_err(|error| {
                    target
                        .pool
                        .quarantine_workspace(target.ws_ref, format!("停机回滚失败：{error}"));
                    ServiceError::State(format!("停机回滚失败：{error}"))
                })?;
            }
        }
        for target in workspaces.values_mut() {
            target.shutdown()?;
        }
        inst.shutdown()?;
        drop(background);
        fault_audit.finish()?;
        log.line("全部工作区完全检查点完成，实例已干净关闭");
    }

    drop(listener);
    let _ = std::fs::remove_file(&opts.socket);
    log.line("服务已退出");
    if foreground {
        let _ = std::io::stdout().flush();
    }
    Ok(())
}

fn maintain_fulltext(
    session: &mut Session<'_, '_, '_, '_>,
    scheduler: &mut FulltextScheduler,
    log: &mut LogFile,
) {
    match session.maintain_fulltext(scheduler) {
        Ok(Some(receipt)) => log.line(&receipt),
        Err(e) => log.line(&format!("全文后台维护失败（保留未处理事件）：{e}")),
        Ok(None) => {}
    }
}

fn maintain_workspace_fulltext(
    instance: &mut crate::boot::Instance,
    schedulers: &mut BTreeMap<[u8; 8], FulltextScheduler>,
    params: &crate::config::InstanceParams,
    log: &mut LogFile,
) -> Result<(), ServiceError> {
    // Read-only and faulted workspaces must not start background write batches.
    if instance.pool.workspace_fault(instance.ws_ref).is_some()
        || instance.pool.workspace_read_only(instance.ws_ref).is_some()
    {
        return Ok(());
    }
    let scheduler = match schedulers.entry(instance.ws_ref) {
        std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
        std::collections::btree_map::Entry::Vacant(entry) => entry.insert(
            FulltextScheduler::new(
                params.run.fulltext_interval_ms,
                params.run.fulltext_batch_rows,
            )
            .map_err(|error| ServiceError::State(error.to_string()))?,
        ),
    };
    // SQL workers publish graph/segment metadata through independent catalogs.
    // A retained control-plane catalog may still cache an old segment HWM.
    let mut catalog = instance.open_worker_catalog()?;
    let seq = instance.seq();
    let mut session = Session::new(instance.pool, instance.engine, &mut catalog, seq);
    session
        .set_fulltext_defaults(
            params.run.fulltext_interval_ms,
            params.run.fulltext_batch_rows,
        )
        .map_err(|error| ServiceError::State(error.to_string()))?;
    session
        .set_graph_limits(params.run.graph_limits())
        .map_err(|error| ServiceError::State(error.to_string()))?;
    maintain_fulltext(&mut session, scheduler, log);
    Ok(())
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

/// 日志尾几行（启动失败的诊断：把死因直接带给用户）。
fn log_tail(path: &Path, n: usize) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return "（日志读不到）".to_owned();
    };
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let from = lines.len().saturating_sub(n);
    lines[from..]
        .iter()
        .map(|l| format!("  {l}"))
        .collect::<Vec<_>>()
        .join("\n")
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

/// 工作区供给方（无状态；`'static` 一份——进程级）。
fn provisioner_static() -> &'static crate::provision::CliProvisioner {
    crate::provision::CliProvisioner::new_static()
}

// ───────────────────────── 客户端：start / stop / status ─────────────────────────

/// **`bicdb start`**：分离起服务并等它就绪（`-w` 语义）。
pub fn start(opts: &StartOptions) -> Result<(), ServiceError> {
    require_public_entry(opts)?;
    // 是不是实例：看**数据文件**在不在（`data/<ws>_meta`，见 `boot` 的文件面）。
    if crate::boot::find_meta_file(&opts.dir).is_none() {
        return Err(ServiceError::State(format!(
            "{} 不是 bicdb 实例（`data/` 下没有 `*_meta`）——先 `bicdb init`",
            opts.dir.display()
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
    let mut child = std::process::Command::new(exe)
        .arg("__daemon")
        .arg("-p")
        .arg(opts.params.ini_path())
        .arg("--socket")
        .arg(&opts.socket)
        .arg("--log")
        .arg(&opts.log)
        .args(opts.force.then_some("--force"))
        .args(
            opts.overrides
                .iter()
                .flat_map(|(k, v)| ["-c".to_owned(), format!("{k}={v}")]),
        )
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(errlog))
        .env(STARTER_PID_ENV, std::process::id().to_string())
        // 新进程组：终端关闭/`Ctrl-C` 不会顺手带走服务（零 unsafe 版的 setsid）。
        .process_group(0)
        .spawn()?;
    let pid = child.id();

    // 等就绪（PG `-w`）：套接字能应答 STATUS 才算起来了。
    let deadline = Instant::now() + opts.timeout;
    loop {
        if let Ok(body) = call_once(
            &opts.socket,
            "STATUS",
            "",
            Duration::from_millis(opts.params.run.probe_timeout_ms),
        ) {
            println!("服务已启动（pid {pid}）");
            println!("  实例    {}", opts.dir.display());
            println!("  套接字  {}", opts.socket.display());
            println!("  日志    {}", opts.log.display());
            if let Some(v) = body.lines().find_map(|l| l.strip_prefix("version=")) {
                println!("  版本    {v}");
            }
            return Ok(());
        }
        // 子进程死了就别等了（`try_wait` 同时**收尸**：僵尸进程在 /proc 里还在，
        // 只看 proc_starttime 会一直"活着"直到超时——实测踩到过）。
        if let Ok(Some(st)) = child.try_wait() {
            // 子进程死因在**日志**里（它的 stderr 也落那儿）——把尾几行带上来，
            // 省得用户还要去翻文件（启动失败是最常见的支持问题）。
            let tail = log_tail(&opts.log, opts.params.run.log_tail_lines as usize);
            return Err(ServiceError::State(format!(
                "服务进程 {pid} 已退出（{st}）——日志 {}：\n{tail}",
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
        std::thread::sleep(Duration::from_millis(opts.params.run.ready_poll_ms));
    }
}

/// **`bicdb stop`**：请服务收尾（经套接字；`-m fast|immediate`），并等它退出。
pub fn stop(
    dir: &Path,
    mode: StopMode,
    wait: Duration,
    params: &crate::config::InstanceParams,
) -> Result<(), ServiceError> {
    let info = lock::read_lock(dir).ok_or_else(|| {
        ServiceError::State(format!("{} 上没有服务在跑（无 pid 文件）", dir.display()))
    })?;
    if !lock::is_live(&info) {
        let _ = std::fs::remove_file(lock::pid_path(dir));
        return Err(ServiceError::State(
            "pid 文件的持有者已不在（陈旧锁，已清理）".to_owned(),
        ));
    }
    let reply = call_once(
        &info.socket,
        "SHUTDOWN",
        mode.as_str(),
        Duration::from_millis(params.run.status_timeout_ms),
    )
    .map_err(|e| {
        ServiceError::State(format!(
            "连服务失败（{}）：{e}——如果是直连模式占着实例，请到那个进程里收尾",
            info.socket.display()
        ))
    })?;
    // 等进程真的退出（跑完全检查点要时间；上限由 `-w` 给——此前写死 300 秒，
    // 而且 `bicdb stop` 解析了 `-w` 却没往下传，用户根本调不动它）。
    let deadline = Instant::now() + wait;
    while lock::proc_starttime(info.pid).is_some_and(|st| st == info.starttime) {
        if Instant::now() >= deadline {
            return Err(ServiceError::State(format!(
                "服务（pid {}）在 {} 秒内没退出（`-w` 可加大：此时它多半正在跑完全检查点）",
                info.pid,
                wait.as_secs()
            )));
        }
        std::thread::sleep(Duration::from_millis(params.run.stop_poll_ms));
    }
    println!("服务已停止（pid {}）——{}", info.pid, reply);
    if lock::pid_path(dir).exists() {
        let _ = std::fs::remove_file(lock::pid_path(dir));
    }
    Ok(())
}

/// **`bicdb status`**：锁事实 + 服务自述。
pub fn status(dir: &Path, params: &crate::config::InstanceParams) -> Result<(), ServiceError> {
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
        match call_once(
            &info.socket,
            "STATUS",
            "",
            Duration::from_millis(params.run.status_timeout_ms),
        ) {
            Ok(body) => {
                for line in body.lines() {
                    if let Some((k, v)) = line.split_once('=') {
                        match k {
                            "uptime_s" => println!("  运行    {} 秒", v),
                            "served" => println!("  已服务  {v} 次请求"),
                            "file_handles" => println!("  文件句柄 {v}"),
                            "recovery_required_workspaces" => {
                                println!("  待恢复工作区 {v}")
                            }
                            "shutdown_state" => println!("  停机状态 {v}"),
                            "shutdown_elapsed_ms" if v != "0" => {
                                println!("  停机排空 {} ms", v)
                            }
                            "quarantined_pages" => println!("  隔离页面 {v}"),
                            "quarantined_objects" => println!("  隔离对象 {v}"),
                            "seq" => println!("  提交序号 {v}"),
                            "cache_bytes" => println!(
                                "  DB Cache {} MiB",
                                v.parse::<u64>().unwrap_or(0) / (1024 * 1024)
                            ),
                            "cache_resident" => println!("  驻留页  {v}"),
                            "cache_hits" => println!("  缓存命中 {v}"),
                            "cache_misses" => println!("  缓存未命中 {v}"),
                            "cache_evictions" => println!("  缓存淘汰 {v}"),
                            "cache_writes" => println!("  数据页写回 {v}"),
                            "cache_dirty_pages" => println!("  待写脏页 {v}"),
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
        stop(&opts.dir, StopMode::Fast, opts.timeout, &opts.params)?;
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

#[cfg(test)]
mod tests;
