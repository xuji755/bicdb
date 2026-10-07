//! **客户端连接**（协议的使用面：`bicdb-net` 的 `Client`）。
//!
//! ```text
//! Client::connect(socket) ──▶ HELLO（版本核对）──▶ SQL / DESCRIBE / STATUS
//!                          └▶ AUTH（可选：主体名 + 口令 —— 连上之后、SQL 之前）
//! ```
//!
//! **一次连接 = 一个会话**（服务端口径）：`BEGIN … COMMIT` 跨该连接的语句成立。
//!
//! **trace**（可选；照 PG `PQTRACE`/`PQsetTraceFlags` 的做法）：把每一帧的
//! 动词与载荷长度打到 stderr——排障时不用抓包；由 `BICDB_TRACE=1` 或
//! [`Client::set_trace`] 打开。

use std::os::unix::net::UnixStream;
use std::path::Path;

use crate::frame::{read_frame, FrameError};
use crate::message::{
    decode_columns, decode_statements, AuthOk, AuthRequest, Column, Hello, SqlRequest, Statement,
    WIRE_VERSION,
};
use crate::value::Value;

/// 客户端错误。
#[derive(Debug)]
pub enum ClientError {
    /// 传输层。
    Frame(FrameError),
    /// **服务端报错**（语句失败/未知动词——原文透传）。
    Server(String),
    /// 协议版本不兼容。
    Version {
        /// 服务端版本。
        server: u8,
        /// 本客户端版本。
        client: u8,
    },
    /// 载荷解码失败。
    Codec(String),
    /// **实例在忙**：服务一次只服务一条连接（V1.0 单写者），握手套不着。
    Busy,
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Frame(e) => write!(f, "{e}"),
            ClientError::Server(m) => f.write_str(m),
            ClientError::Version { server, client } => write!(
                f,
                "协议版本不兼容：服务端 {server}，客户端 {client}——升级较旧的一端"
            ),
            ClientError::Codec(m) => write!(f, "载荷解码失败：{m}"),
            ClientError::Busy => f.write_str(
                "实例正忙：服务一次只服务一条连接（V1.0 单写者）——等另一条连接收尾后重试",
            ),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<FrameError> for ClientError {
    fn from(e: FrameError) -> Self {
        ClientError::Frame(e)
    }
}

impl From<std::io::Error> for ClientError {
    fn from(e: std::io::Error) -> Self {
        ClientError::Frame(FrameError::from(e))
    }
}

/// **一条连接**（持有会话）。
pub struct Client {
    stream: UnixStream,
    /// 服务端握手信息（连上时取）。
    hello: Hello,
    /// trace 开关。
    trace: bool,
}

impl Client {
    /// **握手超时**（默认 5 秒）：服务**一次只服务一条连接**（V1.0 单写者），
    /// 第二条连接会被内核排进 backlog 干等——没有超时就是**无声挂住**。
    /// 超时后报 [`ClientError::Busy`]（可重试），不是"连接坏了"。
    pub const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

    /// **连上并握手**（版本不符即拒绝——不"尽力解释"）。
    pub fn connect(socket: &Path) -> Result<Self, ClientError> {
        Self::connect_with_timeout(socket, Self::HANDSHAKE_TIMEOUT)
    }

    /// 握手超时可调（`None` = 不限；长事务/慢实例的场合）。
    pub fn connect_with_timeout(
        socket: &Path,
        timeout: std::time::Duration,
    ) -> Result<Self, ClientError> {
        let stream = UnixStream::connect(socket).map_err(FrameError::from)?;
        let trace = std::env::var_os("BICDB_TRACE").is_some();
        stream
            .set_read_timeout(Some(timeout))
            .map_err(FrameError::from)?;
        stream
            .set_write_timeout(Some(timeout))
            .map_err(FrameError::from)?;
        let mut c = Self {
            stream,
            hello: Hello {
                wire: 0,
                version: String::new(),
                instance: String::new(),
            },
            trace,
        };
        let text = c.call("HELLO", "").map_err(|e| match e {
            // 等不到握手 ⇒ **实例在忙**（另一个连接正被服务）——可重试，
            // 与"对端坏了"分开报。
            ClientError::Frame(FrameError::Io(io))
                if matches!(
                    io.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                ClientError::Busy
            }
            other => other,
        })?;
        // 握手之后不再限时（长查询是正常的；要限时用 `set_timeout`）。
        let _ = c.stream.set_read_timeout(None);
        let _ = c.stream.set_write_timeout(None);
        let hello = Hello::decode(&text);
        if hello.wire != WIRE_VERSION {
            return Err(ClientError::Version {
                server: hello.wire,
                client: WIRE_VERSION,
            });
        }
        c.hello = hello;
        Ok(c)
    }

    /// 握手的服务端信息（版本/实例）。
    #[must_use]
    pub fn hello(&self) -> &Hello {
        &self.hello
    }

    /// 开关 trace。
    pub fn set_trace(&mut self, on: bool) {
        self.trace = on;
    }

    /// 读写超时（`pg_ctl -w` 式的"等就绪"用；服务卡住时调用方要能自己收场）。
    pub fn set_timeout(&mut self, timeout: std::time::Duration) -> Result<(), ClientError> {
        self.stream.set_read_timeout(Some(timeout))?;
        self.stream.set_write_timeout(Some(timeout))?;
        Ok(())
    }

    /// 底层连接（只读诊断用：`getsockname` 之类要走它）。
    #[must_use]
    pub fn stream(&self) -> &UnixStream {
        &self.stream
    }

    /// **认证**（`AUTH`：主体名 + 口令）——连上之后、任何业务请求之前。
    ///
    /// 服务端校验方式见 `doc/evidence/auth-20261007/evidence.md`：口令**不进日志**；
    /// "主体不存在"与"口令不对"是同一条错误（防枚举），所以调用方**不要**从错误文本
    /// 反推"是不是用户建错了"。
    ///
    /// # Errors
    /// 服务端拒绝（`ERR`：主体名或口令不对 / 已暂停 / 本实例不做口令认证 / 已认证）。
    pub fn auth(&mut self, user: &str, password: &str) -> Result<AuthOk, ClientError> {
        let req = AuthRequest {
            user: user.to_owned(),
            password: password.to_owned(),
        };
        let text = self.call_bytes("AUTH", &req.encode())?;
        Ok(AuthOk::decode(&String::from_utf8_lossy(&text)))
    }

    /// **执行 SQL**（参数随请求：直连与服务两条路径行为一致）。
    pub fn sql(
        &mut self,
        sql: &str,
        params: &[(String, Value)],
    ) -> Result<Vec<Statement>, ClientError> {
        let req = SqlRequest {
            sql: sql.to_owned(),
            params: params.to_vec(),
        };
        let body = req.encode();
        let text = self.call_bytes("SQL", &body)?;
        decode_statements(&text).map_err(ClientError::Codec)
    }

    /// **列定义**（`DESCRIBE`）。
    pub fn describe(&mut self, name: &str) -> Result<Vec<Column>, ClientError> {
        let text = self.call("DESCRIBE", name)?;
        decode_columns(text.as_bytes()).map_err(ClientError::Codec)
    }

    /// 服务自述（`key=value` 行）。
    pub fn status(&mut self) -> Result<String, ClientError> {
        self.call("STATUS", "")
    }

    /// 请服务收尾（`fast`/`immediate`）；随后服务退出，连接断开。
    pub fn shutdown(&mut self, mode: &str) -> Result<String, ClientError> {
        self.call("SHUTDOWN", mode)
    }

    /// 断开（服务继续跑）。
    pub fn close(self) {}

    /// 一次请求-应答（文本载荷）。
    pub fn call(&mut self, verb: &str, payload: &str) -> Result<String, ClientError> {
        let bytes = self.call_bytes(verb, payload.as_bytes())?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// 一次请求-应答（字节载荷）。
    pub fn call_bytes(&mut self, verb: &str, payload: &[u8]) -> Result<Vec<u8>, ClientError> {
        if self.trace {
            eprintln!("[bicdb-net] → {verb} ({} 字节)", payload.len());
        }
        write_frame_result(&mut self.stream, verb, payload)?;
        let (head, body) = read_frame(&mut self.stream)?;
        if self.trace {
            eprintln!("[bicdb-net] ← {head} ({} 字节)", body.len());
        }
        match head.as_str() {
            "OK" => Ok(body.into_bytes()),
            "ERR" => Err(ClientError::Server(body)),
            other => Err(ClientError::Codec(format!("未知应答首行 `{other}`"))),
        }
    }
}

/// **一次请求-应答**（不握手；服务管理命令用：`status`/`stop` 只需要一次往返）。
///
/// 与 [`Client`] 的分工：长连接才需要 HELLO 换版本（好让**后续每一句**都有
/// 版本前提）；管理命令问一句就走，多一次往返不值当。
pub fn call_once(
    socket: &Path,
    verb: &str,
    payload: &str,
    timeout: std::time::Duration,
) -> Result<String, ClientError> {
    let mut stream = UnixStream::connect(socket).map_err(FrameError::from)?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(FrameError::from)?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(FrameError::from)?;
    crate::frame::write_frame_bytes(&mut stream, verb, payload.as_bytes())
        .map_err(ClientError::Frame)?;
    let (head, body) = read_frame(&mut stream)?;
    match head.as_str() {
        "OK" => Ok(body),
        "ERR" => Err(ClientError::Server(body)),
        other => Err(ClientError::Codec(format!("未知应答首行 `{other}`"))),
    }
}

/// `write_frame` 的字节载荷版（错误转 [`ClientError`]）。
fn write_frame_result(s: &mut UnixStream, head: &str, payload: &[u8]) -> Result<(), ClientError> {
    crate::frame::write_frame_bytes(s, head, payload).map_err(ClientError::Frame)
}
