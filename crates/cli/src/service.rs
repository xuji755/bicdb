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

use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use bicdb_sql::session::Session;

use bicdb_net::client::{call_once, ClientError};
use bicdb_net::{frame, AuthOk, AuthRequest, Hello, SqlRequest, WIRE_VERSION};

use crate::boot::open_unlocked_with;
use crate::lock::{self, InstanceLock, LockError, LockInfo, LockMode};
use crate::proto;

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
        let seq = inst.seq();
        let mut session = Session::new(inst.pool, inst.engine, &mut inst.catalog, seq);
        // **接上管理面**（DCL 的落点：`BICDB_HOME` 的注册表 + 实例 I/O）。
        session.set_dcl_context(
            crate::home::Home::locate().ok().map(|h| h.root),
            Some(inst.io),
        );
        session.set_workspace_provisioner(Some(provisioner_static()));
        session.set_pbkdf2_iterations(inst.params.run.pbkdf2_iterations);
        // **固定表的内容源**（`file$` ← 控制文件的内存映像）。
        session.set_fixed_table_source(Some(crate::fixed::CliFixedTables::new_static(
            &opts.dir, inst.io,
        )));
        // **连接态**：`AUTH` 只认"第一个业务请求"这一条（见下）；
        // `authed` 只用于自述与诊断，**资格判据在会话里**（身份是会话的属性）。
        let mut sql_served = false;
        let mut authed: Option<String> = None;
        // **帧载荷按字节读**（`bicdb-net` 的读满纪律）：SQL 里可以有换行，
        // 结果里可以有非 UTF-8 的字节串——按行读会在第一行就断错。
        while let Ok((verb, payload)) = frame::read_frame_bytes(&mut s) {
            // **正忙时把第二条连接明确挡回去**（尽力而为：只在"对方已经连上
            // 并在等"时能看见它）。本版服务一次只服务一条连接（实例是单写者，
            // 会话又借住实例），不挡的话第二条连接会被内核排进 backlog
            // **无声干等**——那比"具名拒绝"糟得多。
            reject_pending(&listener);
            match verb.as_str() {
                "HELLO" => {
                    let hello = Hello {
                        wire: WIRE_VERSION,
                        version: env!("CARGO_PKG_VERSION").to_owned(),
                        instance: opts.dir.display().to_string(),
                    };
                    let _ = frame::write_frame_bytes(&mut s, "OK", hello.encode().as_bytes());
                }
                "STATUS" => {
                    let body = format!(
                        "instance={}\nversion={}\npid={}\nuptime_s={}\nserved={served}\nseq={}\nmode=service\nwire={}\nidentity={}\n",
                        opts.dir.display(),
                        env!("CARGO_PKG_VERSION"),
                        std::process::id(),
                        started.elapsed().as_secs(),
                        session.seq(),
                        WIRE_VERSION,
                        // 谁在连：管理面（本机/OS）还是某个认证过的主体。
                        authed.as_deref().unwrap_or("管理面（本机/OS 身份）")
                    );
                    let _ = frame::write_frame_bytes(&mut s, "OK", body.as_bytes());
                }
                "AUTH" => {
                    // **AUTH 必须是本连接上的第一个业务请求**：先跑过 SQL/DESCRIBE
                    // 的连接再"变成"某个主体，等于"先以管理面身份做事、再冒名"。
                    if sql_served {
                        let _ = frame::write_frame_bytes(
                            &mut s,
                            "ERR",
                            "AUTH 必须是本连接上的第一个业务请求（本连接已执行过语句）\
                             ——重连再认证"
                                .as_bytes(),
                        );
                        continue;
                    }
                    let req = match AuthRequest::decode(&payload) {
                        Ok(r) => r,
                        // **不回报载荷内容**（它可能含口令）。
                        Err(e) => {
                            log.line(&format!("AUTH 载荷非法：{e}"));
                            let _ = frame::write_frame_bytes(
                                &mut s,
                                "ERR",
                                format!("AUTH 载荷非法：{e}（内容不回报——可能含口令）").as_bytes(),
                            );
                            continue;
                        }
                    };
                    match session.authenticate(&req.user, &req.password) {
                        Ok(id) => {
                            // **口令与散列不进日志**（只记主体与结果）。
                            log.line(&format!("认证成功：{}", id.describe()));
                            authed = Some(id.name().to_owned());
                            let ok = AuthOk {
                                user: id.name().to_owned(),
                                user_id: id.user_id(),
                                expired: id.is_expired(),
                            };
                            let _ = frame::write_frame_bytes(&mut s, "OK", ok.encode().as_bytes());
                        }
                        Err(e) => {
                            // 失败只记**主体名与结果**（诊断用）；口令不记录。
                            log.line(&format!("认证失败：主体 `{}`（口令不记录）——{e}", req.user));
                            let _ =
                                frame::write_frame_bytes(&mut s, "ERR", e.to_string().as_bytes());
                        }
                    }
                }
                "SQL" => {
                    served += 1;
                    sql_served = true;
                    let req = match SqlRequest::decode(&payload) {
                        Ok(r) => r,
                        Err(e) => {
                            log.line(&format!("请求载荷非法：{e}"));
                            let _ = frame::write_frame_bytes(
                                &mut s,
                                "ERR",
                                format!("请求载荷非法：{e}").as_bytes(),
                            );
                            continue;
                        }
                    };
                    let named = proto::engine_params(&req.params);
                    let named_ref: Vec<(&str, bicdb_exec::Value)> =
                        named.iter().map(|(n, v)| (n.as_str(), v.clone())).collect();
                    match session.execute_with_params(&req.sql, &named_ref) {
                        Ok(results) => {
                            let body =
                                bicdb_net::message::encode_statements(&proto::statements(&results));
                            let _ = frame::write_frame_bytes(&mut s, "OK", &body);
                        }
                        Err(e) => {
                            log.line(&format!("语句失败：{e}"));
                            let _ =
                                frame::write_frame_bytes(&mut s, "ERR", e.to_string().as_bytes());
                        }
                    }
                }
                "DESCRIBE" => {
                    sql_served = true;
                    // **走会话**（它的目录借用）：不动事务状态——`DESC` 在
                    // 显式事务里也该能用，不能为此把会话丢了（那会回滚事务）。
                    let name = String::from_utf8_lossy(&payload).trim().to_owned();
                    match session.describe_columns(&name) {
                        Ok(cols) => {
                            let rows: Vec<bicdb_net::Column> = cols
                                .into_iter()
                                .map(|(n, nul, code, len)| bicdb_net::Column {
                                    name: n,
                                    // 形态由类型码定（与结果集同一份口径：
                                    // 驱动拿到 `desc` 就知道该把列转成什么）。
                                    kind: proto::kind_char(bicdb_sql::plan::kind_of(code)),
                                    nullable: nul,
                                    type_code: code,
                                    length: len,
                                    type_name: type_name(code, len),
                                })
                                .collect();
                            let body = bicdb_net::message::encode_columns(&rows);
                            let _ = frame::write_frame_bytes(&mut s, "OK", &body);
                        }
                        Err(e) => {
                            let _ =
                                frame::write_frame_bytes(&mut s, "ERR", e.to_string().as_bytes());
                        }
                    }
                }
                "SHUTDOWN" => {
                    let text = String::from_utf8_lossy(&payload);
                    let mode = StopMode::parse(text.trim()).unwrap_or(StopMode::Fast);
                    let _ = frame::write_frame_bytes(
                        &mut s,
                        "OK",
                        format!("shutting down（{}）", mode.as_str()).as_bytes(),
                    );
                    log.line(&format!("收到停止请求（{}）", mode.as_str()));
                    stop_after = Some(mode);
                    break 'accept;
                }
                other => {
                    let _ = frame::write_frame_bytes(
                        &mut s,
                        "ERR",
                        format!("未知动词 `{other}`").as_bytes(),
                    );
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

/// **挡回一条已在等的连接**（尽力而为；没有等待者就什么都不做）。
///
/// 监听套接字临时切成非阻塞：有等待者就收下、回一帧 `ERR` 再关——客户端
/// 因此拿到"实例正忙"的**具名错误**，而不是挂在那里等第一个连接结束。
fn reject_pending(listener: &std::os::unix::net::UnixListener) {
    if listener.set_nonblocking(true).is_err() {
        return;
    }
    if let Ok((mut extra, _)) = listener.accept() {
        let _ = extra.set_nonblocking(false);
        let _ = frame::write_frame_bytes(
            &mut extra,
            "ERR",
            "实例正忙：服务一次只服务一条连接（V1.0 单写者）——稍后重试".as_bytes(),
        );
    }
    let _ = listener.set_nonblocking(false);
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
