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
use std::sync::mpsc;
use std::time::{Duration, Instant};

use bicdb_sql::session::{FulltextScheduler, Session, SessionState};

use bicdb_net::client::{call_once, ClientError};
use bicdb_net::{frame, AuthOk, AuthRequest, Hello, SqlRequest, WIRE_VERSION};

use crate::boot::open_unlocked_with;
use crate::lock::{self, InstanceLock, LockError, LockInfo, LockMode};
use crate::proto;

enum ServiceEvent {
    Request {
        id: u64,
        verb: String,
        payload: Vec<u8>,
        reply: mpsc::SyncSender<WireReply>,
    },
    Closed(u64),
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
    state: SessionState,
    sql_served: bool,
    authed: Option<String>,
    auth_failed: bool,
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
        let Ok(response) = receive.recv() else {
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
            let root = self
                .private_root
                .canonicalize()
                .map_err(|e| e.to_string())?;
            let entry = registry
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
                .ok_or("认证失败：工作区未注册")?;
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
    let mut inst = open_unlocked_with(params, Some(lock))?;
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
    let mut served: u64 = 0;
    let mut sql_elapsed_us: u128 = 0;
    let started = Instant::now();
    let mut fulltext = FulltextScheduler::new(
        params.run.fulltext_interval_ms,
        params.run.fulltext_batch_rows,
    )
    .map_err(|e| ServiceError::State(e.to_string()))?;
    let (event_tx, event_rx) = mpsc::channel();
    let mut connections = BTreeMap::<u64, ConnectionSession>::new();
    let mut next_connection = 1u64;
    let stop_after = 'service: loop {
        loop {
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
                    state: SessionState::new(inst.seq()),
                    sql_served: false,
                    authed: None,
                    auth_failed: false,
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
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if !connections
                    .values()
                    .any(|connection| connection.state.in_transaction())
                {
                    let seq = inst.seq();
                    let mut session = Session::new(inst.pool, inst.engine, &mut inst.catalog, seq);
                    session
                        .set_fulltext_defaults(
                            params.run.fulltext_interval_ms,
                            params.run.fulltext_batch_rows,
                        )
                        .map_err(|e| ServiceError::State(e.to_string()))?;
                    session
                        .set_graph_limits(params.run.graph_limits())
                        .map_err(|e| ServiceError::State(e.to_string()))?;
                    maintain_fulltext(&mut session, &mut fulltext, &mut log);
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(ServiceError::State("服务连接调度器意外关闭".into()));
            }
        };
        match event {
            ServiceEvent::Closed(id) => {
                if let Some(mut connection) = connections.remove(&id) {
                    let seq = inst.seq();
                    let mut session = Session::new(inst.pool, inst.engine, &mut inst.catalog, seq);
                    session.resume_state(&mut connection.state);
                    drop(session); // rollback this connection's uncommitted transaction
                }
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
                let seq = inst.seq();
                connection.state.refresh_committed(seq);
                let mut session = Session::new(inst.pool, inst.engine, &mut inst.catalog, seq);
                session.resume_state(&mut connection.state);
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
                session.set_fixed_table_source(Some(crate::fixed::CliFixedTables::new_static(
                    &opts.dir, inst.io,
                )));

                let mut requested_stop = None;
                let response = match verb.as_str() {
                    "HELLO" => WireReply::ok(
                        Hello {
                            wire: WIRE_VERSION,
                            version: env!("CARGO_PKG_VERSION").to_owned(),
                            instance: opts.dir.display().to_string(),
                        }
                        .encode()
                        .into_bytes(),
                    ),
                    "STATUS" => {
                        let cache = inst.pool.stats();
                        WireReply::ok(format!(
                            "instance={}\nversion={}\npid={}\nuptime_s={}\nserved={served}\nseq={}\nmode=service\nwire={}\nconnections={connection_count}\nmax_connections={}\nworkspace_kind={}\nidentity={}\ncache_frames={}\ncache_bytes={}\ncache_resident={}\ncache_hits={}\ncache_misses={}\ncache_evictions={}\ncache_writes={}\ncache_dirty_pages={}\ncache_free_buffer_waits={}\ncache_wal_syncs={}\ncache_run_reads={}\ncache_run_pages={}\ngraph_detach_edge_limit={}\ngraph_max_nodes={}\ngraph_max_edges={}\ngraph_max_rows={}\ngraph_max_expansions={}\ngraph_max_edge_expansions={}\ngraph_max_elapsed_ms={}\ngraph_max_depth={}\ngraph_max_text_bytes={}\nsql_elapsed_us={sql_elapsed_us}\n",
                            opts.dir.display(), env!("CARGO_PKG_VERSION"), std::process::id(),
                            started.elapsed().as_secs(), session.seq(), WIRE_VERSION,
                            params.run.max_connections,
                            if session.on_public_workspace() { "public" } else { "private" },
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
                            params.run.graph_max_text_bytes,
                        ).into_bytes())
                    }
                    "AUTH" => {
                        if connection.sql_served {
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
                    "SQL" => {
                        if connection.auth_failed {
                            WireReply::error(
                                "认证失败的连接不能执行语句；请重新认证或重连"
                                    .as_bytes()
                                    .to_vec(),
                            )
                        } else {
                            served += 1;
                            connection.sql_served = true;
                            match SqlRequest::decode(&payload) {
                                Err(error) => {
                                    log.line(&format!("请求载荷非法：{error}"));
                                    WireReply::error(format!("请求载荷非法：{error}").into_bytes())
                                }
                                Ok(request) => {
                                    let named = proto::engine_params(&request.params);
                                    let named_ref: Vec<(&str, bicdb_exec::Value)> = named
                                        .iter()
                                        .map(|(name, value)| (name.as_str(), value.clone()))
                                        .collect();
                                    let executing = Instant::now();
                                    let result =
                                        session.execute_with_params(&request.sql, &named_ref);
                                    sql_elapsed_us += executing.elapsed().as_micros();
                                    match result {
                                        Ok(results) => {
                                            WireReply::ok(bicdb_net::message::encode_statements(
                                                &proto::statements(&results),
                                            ))
                                        }
                                        Err(error) => {
                                            log.line(&format!("语句失败：{error}"));
                                            WireReply::error(error.to_string().into_bytes())
                                        }
                                    }
                                }
                            }
                        }
                    }
                    "DESCRIBE" => {
                        if connection.auth_failed {
                            WireReply::error(
                                "认证失败的连接不能执行语句；请重新认证或重连"
                                    .as_bytes()
                                    .to_vec(),
                            )
                        } else {
                            connection.sql_served = true;
                            let name = String::from_utf8_lossy(&payload).trim().to_owned();
                            match session.describe_columns(&name) {
                                Ok(columns) => {
                                    let rows: Vec<bicdb_net::Column> = columns
                                        .into_iter()
                                        .map(|(name, nullable, type_code, length)| {
                                            bicdb_net::Column {
                                                name,
                                                kind: proto::kind_char(bicdb_sql::plan::kind_of(
                                                    type_code,
                                                )),
                                                nullable,
                                                type_code,
                                                length,
                                                type_name: type_name(type_code, length),
                                            }
                                        })
                                        .collect();
                                    WireReply::ok(bicdb_net::message::encode_columns(&rows))
                                }
                                Err(error) => WireReply::error(error.to_string().into_bytes()),
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
                session.suspend_state(&mut connection.state);
                drop(session);
                let _ = reply.send(response);
                if let Some(mode) = requested_stop {
                    break 'service mode;
                }
                // Poll by elapsed time after every request as well as during
                // idle periods. A client polling SHOW FULLTEXT every 40 ms
                // must not starve a 100 ms maintenance interval forever.
                // Any suspended explicit transaction pauses maintenance for
                // the whole workspace, preserving the single-session rule.
                if !connections
                    .values()
                    .any(|connection| connection.state.in_transaction())
                {
                    let seq = inst.seq();
                    let mut maintenance =
                        Session::new(inst.pool, inst.engine, &mut inst.catalog, seq);
                    maintenance
                        .set_fulltext_defaults(
                            params.run.fulltext_interval_ms,
                            params.run.fulltext_batch_rows,
                        )
                        .map_err(|e| ServiceError::State(e.to_string()))?;
                    maintenance
                        .set_graph_limits(params.run.graph_limits())
                        .map_err(|e| ServiceError::State(e.to_string()))?;
                    maintain_fulltext(&mut maintenance, &mut fulltext, &mut log);
                }
            }
        }
    };

    // Every suspended explicit transaction belongs to a connection. Roll all
    // of them back before checkpointing so shutdown never publishes client work.
    for (_, mut connection) in connections {
        let seq = inst.seq();
        let mut session = Session::new(inst.pool, inst.engine, &mut inst.catalog, seq);
        session.resume_state(&mut connection.state);
        drop(session);
    }

    // **收尾**：fast 走完全检查点；immediate 直接退出（下次打开做恢复）。
    match stop_after {
        StopMode::Immediate => log.line("immediate 停止：不做完全检查点（下次打开走崩溃恢复）"),
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
        .args(
            opts.overrides
                .iter()
                .flat_map(|(k, v)| ["-c".to_owned(), format!("{k}={v}")]),
        )
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
