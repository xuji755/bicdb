//! **连接面**：`Connection` / `ResultSet` / `Row` / `Column`。
//!
//! ```text
//! Connection::connect("<根区目录|bicdb.ini>")  ──▶ HELLO（核版本）──▶ query / execute / describe
//! ```
//!
//! **一条连接 = 一个会话**（协议 §8）：`begin()` 之后的语句直到 `commit()`
//! 都在同一个事务里；连接断开 ⇒ 服务端回滚该连接未提交的事务。
//!
//! **不并发**：一条连接上一次只有一个未决请求（协议 §0）。要并发就开多条连接。

use std::path::{Path, PathBuf};

use bicdb_net::client::{Client, ClientError};
use bicdb_net::discover;

use crate::error::Error;
use crate::value::Value;

/// 列形态（协议 §3.3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnKind {
    /// 数值（`NUMBER`）。
    Number,
    /// 字节串（文本/二进制同形）。
    Bytes,
    /// 布尔。
    Bool,
    /// 协议里出现的不认识形态（**不猜**——新形态由新版本协议定义）。
    Unknown(char),
}

impl ColumnKind {
    fn of(tag: char) -> Self {
        match tag {
            'n' => ColumnKind::Number,
            'b' => ColumnKind::Bytes,
            'o' => ColumnKind::Bool,
            other => ColumnKind::Unknown(other),
        }
    }

    /// 协议标记字符。
    #[must_use]
    pub fn tag(self) -> char {
        match self {
            ColumnKind::Number => 'n',
            ColumnKind::Bytes => 'b',
            ColumnKind::Bool => 'o',
            ColumnKind::Unknown(c) => c,
        }
    }

    /// 形态名（错误信息里用）。
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            ColumnKind::Number => "数值",
            ColumnKind::Bytes => "字节串",
            ColumnKind::Bool => "布尔",
            ColumnKind::Unknown(_) => "未知形态",
        }
    }
}

/// 一列。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    /// 列名。
    pub name: String,
    /// 形态。
    pub kind: ColumnKind,
    /// 是否可空（结果集里恒 `true`；`describe` 有真值）。
    pub nullable: bool,
    /// 引擎类型码（`describe` 有）。
    pub type_code: u32,
    /// 声明长度（`describe` 有）。
    pub length: u32,
    /// 类型名（`describe` 有，`VARCHAR2(32)` 形态）。
    pub type_name: String,
}

/// 结果集（列 + 行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultSet {
    columns: Vec<Column>,
    rows: Vec<Vec<Value>>,
}

impl ResultSet {
    /// 列。
    #[must_use]
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    /// 行数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// 没有行吗。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// 按位置取行（从 0 起）。
    #[must_use]
    pub fn row(&self, i: usize) -> Option<Row<'_>> {
        self.rows.get(i).map(|values| Row {
            columns: &self.columns,
            values,
        })
    }

    /// 逐行迭代。
    pub fn iter(&self) -> impl Iterator<Item = Row<'_>> {
        self.rows.iter().map(|values| Row {
            columns: &self.columns,
            values,
        })
    }

    /// 列序号（名字**大小写不敏感**——SQL 里未引号的名字会被折叠）。
    #[must_use]
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
    }

    /// 原始行（不含列信息）——要自己搬走数据时用。
    #[must_use]
    pub fn into_rows(self) -> Vec<Vec<Value>> {
        self.rows
    }

    /// 原始行（借用）。
    #[must_use]
    pub fn rows(&self) -> &[Vec<Value>] {
        &self.rows
    }
}

/// 一行（借结果集）。
#[derive(Debug, Clone, Copy)]
pub struct Row<'a> {
    columns: &'a [Column],
    values: &'a [Value],
}

impl<'a> Row<'a> {
    /// 列数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// 空行吗。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// 列信息。
    #[must_use]
    pub fn columns(&self) -> &'a [Column] {
        self.columns
    }

    /// 按位置取值。
    #[must_use]
    pub fn get(&self, i: usize) -> Option<&'a Value> {
        self.values.get(i)
    }

    /// 按位置取值，取不到就报具名错误。
    ///
    /// # Errors
    /// 列序号超出范围。
    pub fn value(&self, i: usize) -> Result<&'a Value, Error> {
        self.get(i).ok_or_else(|| Error::Type {
            column: i.to_string(),
            wanted: "存在的列",
            got: "列序号超出范围",
        })
    }

    /// 按名列序号（大小写不敏感）。
    #[must_use]
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
    }

    /// 按名取值（列名不存在 ⇒ 具名错误）。
    ///
    /// # Errors
    /// 列名不在结果集里。
    pub fn by_name(&self, name: &str) -> Result<&'a Value, Error> {
        let i = self.index_of(name).ok_or_else(|| Error::Type {
            column: name.to_owned(),
            wanted: "结果集里的列",
            got: "不存在的列",
        })?;
        Ok(&self.values[i])
    }

    /// 按位置取 `i64`。
    ///
    /// # Errors
    /// 列不是数值或超出 `i64`。
    pub fn i64(&self, i: usize) -> Result<i64, Error> {
        self.typed(i, "数值(i64)", Value::as_i64)
    }

    /// 按位置取 `f64`（可能丢精度）。
    ///
    /// # Errors
    /// 列不是数值。
    pub fn f64(&self, i: usize) -> Result<f64, Error> {
        self.typed(i, "数值(f64)", Value::as_f64)
    }

    /// 按位置取布尔。
    ///
    /// # Errors
    /// 列不是布尔。
    pub fn bool(&self, i: usize) -> Result<bool, Error> {
        self.typed(i, "布尔", Value::as_bool)
    }

    /// 按位置取字节串。
    ///
    /// # Errors
    /// 列不是字节串（或 `NULL`）。
    pub fn bytes(&self, i: usize) -> Result<&'a [u8], Error> {
        self.typed(i, "字节串", Value::as_bytes)
    }

    /// 按位置取文本（**非 UTF-8 ⇒ 具名错误**，不替换成 `�`）。
    ///
    /// # Errors
    /// 列不是字节串，或字节串不是 UTF-8。
    pub fn str(&self, i: usize) -> Result<&'a str, Error> {
        self.typed(i, "文本(UTF-8)", Value::as_str)
    }

    /// `NULL` 就是 `NULL`（**要区分"缺值"与"值是 NULL"时用它**）。
    ///
    /// # Errors
    /// 列序号超出范围。
    pub fn is_null(&self, i: usize) -> Result<bool, Error> {
        Ok(self.value(i)?.is_null())
    }

    fn typed<T>(
        &self,
        i: usize,
        wanted: &'static str,
        pick: impl Fn(&'a Value) -> Option<T>,
    ) -> Result<T, Error> {
        let v = self.value(i)?;
        pick(v).ok_or_else(|| Error::Type {
            column: self
                .columns
                .get(i)
                .map_or_else(|| i.to_string(), |c| c.name.clone()),
            wanted,
            got: v.kind_name(),
        })
    }
}

/// 一条语句的结果（`sql` 的返回项）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// 结果集。
    Rows(ResultSet),
    /// 影响行数。
    Affected(u64),
    /// DDL 回执。
    Ddl(String),
    /// 事务回执。
    Txn(String),
}

impl Outcome {
    /// 短名（错误信息里用）。
    #[must_use]
    pub fn kind_name(&self) -> &'static str {
        match self {
            Outcome::Rows(_) => "结果集",
            Outcome::Affected(_) => "影响行数",
            Outcome::Ddl(_) => "DDL 回执",
            Outcome::Txn(_) => "事务回执",
        }
    }

    /// 是结果集吗。
    #[must_use]
    pub fn is_rows(&self) -> bool {
        matches!(self, Outcome::Rows(_))
    }
}

/// **一条连接**（= 一个会话）。
///
/// `Debug` 只打握手信息（**不打印连接内部**——套接字与缓冲不该出现在日志里）。
/// 环境变量：主体名（照 libpq 的 `PGUSER` 口径）——`connect_env` 认它。
pub const ENV_USER: &str = "BICDB_USER";
/// 环境变量：口令（照 libpq 的 `PGPASSWORD` 口径）——`connect_env` 认它。
pub const ENV_PASSWORD: &str = "BICDB_PASSWORD";

/// **一条连接**（一个会话；`BEGIN … COMMIT` 跨该连接成立）。
pub struct Connection {
    client: Client,
    /// **已认证的身份**（`None` = 没认证：本机/OS 身份）。
    ///
    /// 存下来是为了 `user()` 与 `Debug` 能如实说"这条连接是谁"——
    /// **不给任何"改身份"的口**：身份是连接建立时定下的（`REQ-ISO-002`）。
    identity: Option<bicdb_net::AuthOk>,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("instance", &self.client.hello().instance)
            .field("version", &self.client.hello().version)
            .field("wire", &self.client.hello().wire)
            .field("user", &self.user())
            .finish_non_exhaustive()
    }
}

impl Connection {
    /// **连上**：`target` 是**参数文件或根区目录**（协议 §1 的显式来源）。
    ///
    /// # Errors
    /// 参数文件找不到/读不了，或连不上服务。
    pub fn connect(target: impl AsRef<Path>) -> Result<Self, Error> {
        Self::connect_explicit(Some(target.as_ref()))
    }

    /// **连上（自定义握手超时）**：实例在忙时等多久算超时
    /// （默认见 [`bicdb_net::Client::HANDSHAKE_TIMEOUT`]，5 秒）。
    ///
    /// # Errors
    /// 参数文件找不到/读不了，连不上服务，或等不到握手（实例正忙）。
    pub fn connect_with_timeout(
        target: impl AsRef<Path>,
        timeout: std::time::Duration,
    ) -> Result<Self, Error> {
        let socket = discover::socket_for(Some(target.as_ref()))
            .map_err(|e| Error::Discover { why: e.to_string() })?;
        Self::from_socket_with_timeout(&socket, Some(timeout))
    }

    /// Connect using an explicit standard parameter file and verify HELLO identity.
    pub fn connect_configured(ini: &Path, timeout: std::time::Duration) -> Result<Self, Error> {
        if !ini.is_file() {
            return Err(Error::Discover {
                why: "a standard instance parameter file is required".into(),
            });
        }
        let (root, _) = discover::instance_for(Some(ini))
            .map_err(|e| Error::Discover { why: e.to_string() })?;
        let connection = Self::connect_with_timeout(ini, timeout)?;
        let expected = root
            .canonicalize()
            .map_err(|e| Error::Discover { why: e.to_string() })?;
        let actual = Path::new(connection.instance())
            .canonicalize()
            .map_err(|e| Error::Discover { why: e.to_string() })?;
        if actual != expected {
            return Err(Error::Discover {
                why: "configured database instance does not match HELLO".into(),
            });
        }
        Ok(connection)
    }

    /// 连上：不给来源 ⇒ 按 `$BICDB_INI`、再退到当前目录的 `./bicdb.ini`。
    ///
    /// **顺带认环境里的身份**（照 libpq 的 `PGUSER`/`PGPASSWORD` 口径）：
    /// `BICDB_USER` 设了就认证它，口令取 `BICDB_PASSWORD`（缺省空串）。
    /// 没设 `BICDB_USER` ⇒ 不认证（本机/OS 身份），与从前一样。
    ///
    /// # Errors
    /// 参数文件找不到/读不了，连不上服务，或认证不通过。
    pub fn connect_env() -> Result<Self, Error> {
        let mut c = Self::connect_explicit(None)?;
        if let Some(user) = std::env::var_os(ENV_USER) {
            let user = user.to_string_lossy().into_owned();
            let password = std::env::var_os(ENV_PASSWORD)
                .map(|v| v.to_string_lossy().into_owned())
                .unwrap_or_default();
            c.login(&user, &password)?;
        }
        Ok(c)
    }

    /// **直接连套接字**（已知套接字路径时用；跳过参数文件）。
    ///
    /// # Errors
    /// 连不上服务。
    pub fn connect_socket(socket: impl AsRef<Path>) -> Result<Self, Error> {
        Self::from_socket(socket.as_ref())
    }

    fn connect_explicit(target: Option<&Path>) -> Result<Self, Error> {
        let socket =
            discover::socket_for(target).map_err(|e| Error::Discover { why: e.to_string() })?;
        Self::from_socket(&socket)
    }

    fn from_socket(socket: &Path) -> Result<Self, Error> {
        Self::from_socket_with_timeout(socket, None)
    }

    fn from_socket_with_timeout(
        socket: &Path,
        timeout: Option<std::time::Duration>,
    ) -> Result<Self, Error> {
        // **先换错再连**（`UnixStream::connect` 对不存在的路径也报
        // `Connection refused` 形态的错——分开报才能指对方向）。
        if !socket.exists() {
            return Err(Error::Connect {
                path: socket.to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "套接字不存在（实例没在跑？）",
                ),
            });
        }
        let connected = match timeout {
            Some(t) => Client::connect_with_timeout(socket, t),
            None => Client::connect(socket),
        };
        let client = connected.map_err(|e| match e {
            ClientError::Busy => Error::Busy {
                path: socket.to_path_buf(),
            },
            other => Error::Connect {
                path: socket.to_path_buf(),
                source: std::io::Error::other(short(&other)),
            },
        })?;
        Ok(Self {
            client,
            identity: None,
        })
    }

    /// **以某个主体连上**（`AUTH`：主体名 + 口令）——连上即认证，
    /// 之后这条连接就是该主体的（`user()` 能取到身份）。
    ///
    /// 口令走**本机套接字**；服务端的失败语义见协议 §4.3：
    /// "主体不存在"与"口令不对"是**同一条**错误（防枚举），
    /// **不要**从错误文本反推"是不是名字写错了"。
    ///
    /// # Errors
    /// 连不上服务，或认证不通过（[`Error::Server`]，服务端原文透传）。
    pub fn connect_as(target: impl AsRef<Path>, user: &str, password: &str) -> Result<Self, Error> {
        let mut c = Self::connect_explicit(Some(target.as_ref()))?;
        c.login(user, password)?;
        Ok(c)
    }

    /// 同上，但**直接连套接字**（跳过参数文件）。
    ///
    /// # Errors
    /// 连不上服务，或认证不通过。
    pub fn connect_socket_as(
        socket: impl AsRef<Path>,
        user: &str,
        password: &str,
    ) -> Result<Self, Error> {
        let mut c = Self::connect_socket(socket)?;
        c.login(user, password)?;
        Ok(c)
    }

    /// **在这条连接上认证**（连上之后、第一条语句之前——服务的准入规则）。
    ///
    /// 典型用法只有 [`Connection::connect_as`]；单独暴露是因为
    /// "先看 `server_version()` 再决定要不要认证"也是合理用法。
    ///
    /// # Errors
    /// 认证不通过或本连接已认证（服务端原文透传）。
    pub fn login(&mut self, user: &str, password: &str) -> Result<(), Error> {
        let id = self.client.auth(user, password).map_err(map_err)?;
        self.identity = Some(id);
        Ok(())
    }

    /// 本连接的身份（主体名）；`None` = 没认证（本机/OS 身份）。
    #[must_use]
    pub fn user(&self) -> Option<&str> {
        self.identity.as_ref().map(|i| i.user.as_str())
    }

    /// 主体号（`None` = 没认证）。
    #[must_use]
    pub fn user_id(&self) -> Option<u64> {
        self.identity.as_ref().map(|i| i.user_id)
    }

    /// Authoritative workspace route returned by PUBLIC after authentication.
    pub fn route_owned(
        &mut self,
        selection: Option<&str>,
    ) -> Result<bicdb_net::message::OwnedWorkspace, Error> {
        self.client.route_owned(selection).map_err(map_err)
    }

    /// Bind subsequent SQL to the authenticated principal's owned workspace.
    pub fn bind_workspace(
        &mut self,
        selection: Option<&str>,
    ) -> Result<bicdb_net::message::OwnedWorkspace, Error> {
        self.client.bind_workspace(selection).map_err(map_err)
    }

    /// 口令是否已过期（受限会话）；`None` = 没认证。
    #[must_use]
    pub fn password_expired(&self) -> Option<bool> {
        self.identity.as_ref().map(|i| i.expired)
    }

    /// 服务/引擎版本（`HELLO` 的 `version`）。
    #[must_use]
    pub fn server_version(&self) -> &str {
        &self.client.hello().version
    }

    /// 实例根区目录（`HELLO` 的 `instance`）。
    #[must_use]
    pub fn instance(&self) -> &str {
        &self.client.hello().instance
    }

    /// 协议版本（`HELLO` 的 `wire`）。
    #[must_use]
    pub fn wire_version(&self) -> u8 {
        self.client.hello().wire
    }

    /// 开关协议 trace（每帧的动词与字节数打到 stderr；也可用 `BICDB_TRACE=1`）。
    pub fn set_trace(&mut self, on: bool) {
        self.client.set_trace(on);
    }

    /// **设置读写超时**（握手后默认不限时——长查询是正常的；
    /// 想给"服务卡住"兜底就设一个）。
    ///
    /// # Errors
    /// 底层的超时设置失败。
    pub fn set_timeout(&mut self, timeout: std::time::Duration) -> Result<(), Error> {
        self.client
            .set_timeout(timeout)
            .map_err(|e| Error::Protocol { why: short(&e) })
    }

    /// **执行 SQL**（可含多条语句；`;` 分隔），逐条取结果。
    ///
    /// # Errors
    /// 语句失败（[`Error::Server`]，**原文透传**）、协议错、连接错。
    pub fn sql(&mut self, sql: &str, params: &[(&str, Value)]) -> Result<Vec<Outcome>, Error> {
        let sent: Vec<(String, bicdb_net::Value)> = params
            .iter()
            .map(|(n, v)| ((*n).to_owned(), to_wire(v)))
            .collect();
        let statements = self.client.sql(sql, &sent).map_err(map_err)?;
        Ok(statements.iter().map(from_statement).collect())
    }

    /// **执行一条查询**：取最后一条语句的结果集（不是结果集 ⇒ 具名错误）。
    ///
    /// # Errors
    /// 语句失败，或最后一条语句不是结果集。
    pub fn query(&mut self, sql: &str, params: &[(&str, Value)]) -> Result<ResultSet, Error> {
        let mut outcomes = self.sql(sql, params)?;
        match outcomes.pop() {
            Some(Outcome::Rows(rs)) => Ok(rs),
            Some(other) => Err(Error::NotResultSet {
                got: other.kind_name(),
            }),
            None => Err(Error::NotResultSet { got: "空结果" }),
        }
    }

    /// **执行一条写入/DDL**：取最后一条语句的影响行数（DDL/事务回执记 0）。
    ///
    /// # Errors
    /// 语句失败。
    pub fn execute(&mut self, sql: &str, params: &[(&str, Value)]) -> Result<u64, Error> {
        let mut outcomes = self.sql(sql, params)?;
        Ok(match outcomes.pop() {
            Some(Outcome::Affected(n)) => n,
            _ => 0,
        })
    }

    /// **列定义**（`DESCRIBE`）。
    ///
    /// # Errors
    /// 对象不存在（[`Error::Server`]）或协议错。
    pub fn describe(&mut self, name: &str) -> Result<Vec<Column>, Error> {
        let cols = self.client.describe(name).map_err(map_err)?;
        Ok(cols.into_iter().map(from_column).collect())
    }

    /// 服务自述（`key=value` 行）。
    ///
    /// # Errors
    /// 协议错。
    pub fn status(&mut self) -> Result<Vec<(String, String)>, Error> {
        let text = self.client.status().map_err(map_err)?;
        Ok(text
            .lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.to_owned(), v.trim().to_owned()))
            .collect())
    }

    /// **开一个显式事务**（`BEGIN`）。
    ///
    /// # Errors
    /// 已经在事务里之类的具名错误。
    pub fn begin(&mut self) -> Result<(), Error> {
        self.sql("BEGIN", &[]).map(|_| ())
    }

    /// **提交**（`COMMIT`）。
    ///
    /// # Errors
    /// 没有事务之类。
    pub fn commit(&mut self) -> Result<(), Error> {
        self.sql("COMMIT", &[]).map(|_| ())
    }

    /// **回滚**（`ROLLBACK`）。
    ///
    /// # Errors
    /// 没有事务之类。
    pub fn rollback(&mut self) -> Result<(), Error> {
        self.sql("ROLLBACK", &[]).map(|_| ())
    }

    /// 断开（服务继续跑；未提交的显式事务由服务端回滚）。
    pub fn close(self) {}
}

/// 定位实例套接字（**不连**；诊断/预检用）。
///
/// # Errors
/// 参数文件找不到/读不了。
pub fn locate_socket(target: Option<&Path>) -> Result<PathBuf, Error> {
    discover::socket_for(target).map_err(|e| Error::Discover { why: e.to_string() })
}

fn to_wire(v: &Value) -> bicdb_net::Value {
    match v {
        Value::Null => bicdb_net::Value::Null,
        Value::Number(t) => bicdb_net::Value::Number(t.clone()),
        Value::Bool(b) => bicdb_net::Value::Bool(*b),
        Value::Bytes(b) => bicdb_net::Value::Bytes(b.clone()),
        Value::GraphElement { graph, kind, id } => bicdb_net::Value::GraphElement {
            graph: *graph,
            kind: *kind,
            id: *id,
        },
    }
}

fn from_wire(v: &bicdb_net::Value) -> Value {
    match v {
        bicdb_net::Value::Null => Value::Null,
        bicdb_net::Value::Number(t) => Value::Number(t.clone()),
        bicdb_net::Value::Bool(b) => Value::Bool(*b),
        bicdb_net::Value::Bytes(b) => Value::Bytes(b.clone()),
        bicdb_net::Value::GraphElement { graph, kind, id } => Value::GraphElement {
            graph: *graph,
            kind: *kind,
            id: *id,
        },
    }
}

fn from_column(c: bicdb_net::Column) -> Column {
    Column {
        name: c.name,
        kind: ColumnKind::of(c.kind),
        nullable: c.nullable,
        type_code: c.type_code,
        length: c.length,
        type_name: c.type_name,
    }
}

fn from_statement(s: &bicdb_net::Statement) -> Outcome {
    match s {
        bicdb_net::Statement::Rows { columns, rows } => Outcome::Rows(ResultSet {
            columns: columns.iter().cloned().map(from_column).collect(),
            rows: rows
                .iter()
                .map(|r| r.iter().map(from_wire).collect())
                .collect(),
        }),
        bicdb_net::Statement::Affected(n) => Outcome::Affected(*n),
        bicdb_net::Statement::Ddl(t) => Outcome::Ddl(t.clone()),
        bicdb_net::Statement::Txn(t) => Outcome::Txn(t.clone()),
    }
}

fn map_err(e: ClientError) -> Error {
    match e {
        ClientError::Server(message) => Error::Server { message },
        ClientError::Version { server, client } => Error::Protocol {
            why: format!("协议版本不兼容：服务端 {server}，客户端 {client}——升级较旧的一端"),
        },
        other => Error::Protocol { why: short(&other) },
    }
}

fn short(e: &impl std::fmt::Display) -> String {
    e.to_string()
}
