//! **会话侧装配：compile → execute**（`SQL前端设计` §2 `src/session.rs`；ENG REQ-ENG-005）。
//!
//! ```text
//! SQL 文本 ── parse ── bind ── plan ── execute ──▶ 结果集 / 影响行数
//!               ①       ②      ③④       ⑤
//! ```
//!
//! **事务边界**（既定纪律：语句 = 一个事务，REQ-TXN-016 的 DDL 独占）：
//! - **自动提交**（默认）：每条语句自带事务（写语句 commit、出错 rollback）；
//! - **显式事务**（`BEGIN`）：持一个事务句柄，`COMMIT`/`ROLLBACK` 收尾——
//!   期间 DDL 一律拒绝（"活动事务中发 DDL ⇒ 拒绝"）。
//!
//! **本切片的会话面**：`SELECT` / `INSERT … VALUES` / `CREATE TABLE` /
//! `CREATE [UNIQUE] INDEX` / `DROP TABLE|INDEX` / `BEGIN|COMMIT|ROLLBACK`。
//! 清单外语句在**绑定期**具名拒绝（绝不静默）。
//!
//! **身份（D6）**：会话带一个**身份**——`None` = **管理面身份**（本机/OS：
//! 控制套接字的文件权限允许谁连），`Some(Identity)` = 经 `AUTH` 认证的**具名主体**。
//! 身份只从 [`Session::authenticate`] 进（`REQ-ISO-002` 的结构保证：语句载荷里
//! 没有"用户号"这个字段）；它对语句面的作用是三条**具名拒绝**：
//! ① 具名主体在 `public` 上**只读**（`public` 是共享内容，对主体只读）；
//! ② 管理面语句（DCL）要**管理面身份**，主体只能对自己用
//!    `ALTER USER … IDENTIFIED BY … REPLACE '<旧口令>'`（Oracle 的"本人改密"口径）；
//! ③ `EXPIRE` 的**受限会话**：除本人改密之外一律拒绝（MySQL 的同位形态）。
//! 资格检查**先于**绑定与对象查找（REQ-SQL-005 口径）。

use bicdb_catalog::ddl::{self, create_index, create_table, drop_index, drop_table};
use bicdb_common::seq::CommitSeq;
use bicdb_exec::{
    build, collect, ColKind, ExecContext, ExecEnv, Row, RowCursor, RowShape, TableAccessWriter,
    TableWriter, Value,
};
use bicdb_storage::buffer::BufferPool;
use bicdb_storage::scan::HeapScanner;
use bicdb_storage::segment::Segment;
use bicdb_txn::engine::{Engine, TxnHandle};

pub use bicdb_graph::{Limits as GraphLimits, DEFAULT_DETACH_EDGE_LIMIT, MAX_DETACH_EDGE_LIMIT};

use crate::auth::{self, Identity};
use crate::bind::{bind_statement, BindError, CatalogView, CatalogViewImpl, NameResolver};
use crate::parser::parse_many as parse;
use crate::plan::{ddl_summary, plan_statement, PhysicalPlan, PlanKind};

/// 会话错误。
#[derive(Debug)]
pub enum SessionError {
    /// 词法/语法。
    Parse(crate::parser::ParseError),
    /// 绑定。
    Bind(BindError),
    /// 目录写侧（DDL）。
    Ddl(ddl::DdlError),
    /// 执行器。
    Exec(bicdb_exec::ExecError),
    /// 事务引擎。
    Txn(bicdb_txn::write::TxnError),
    /// 段层。
    Segment(bicdb_storage::segment::SegmentSpaceError),
    /// 会话状态非法（DDL 落在显式事务里 / 提交时无事务 …）。
    State(String),
    /// **参数面**（缺值 / 多给 / 形态不符）——语句声明了参数却没配对。
    Params(String),
    /// **管理面（DCL）**：资格/上下文/对象/路径——各条都有具名文案。
    Dcl(String),
    /// **身份/认证（D6）**：认证不通过与身份资格（各条都有具名文案）。
    Auth(String),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::Parse(e) => write!(f, "{e}"),
            SessionError::Bind(e) => write!(f, "{e}"),
            SessionError::Ddl(e) => write!(f, "{e}"),
            SessionError::Exec(e) => write!(f, "执行：{e}"),
            SessionError::Txn(e) => write!(f, "事务：{e}"),
            SessionError::Segment(e) => write!(f, "段：{e}"),
            SessionError::State(why) => f.write_str(why),
            SessionError::Params(why) => write!(f, "参数：{why}"),
            SessionError::Dcl(why) => write!(f, "{why}"),
            SessionError::Auth(why) => write!(f, "{why}"),
        }
    }
}

impl std::error::Error for SessionError {}

macro_rules! from_err {
    ($($v:ident <- $t:ty),* $(,)?) => {
        $(impl From<$t> for SessionError { fn from(e: $t) -> Self { Self::$v(e) } })*
    };
}
from_err!(
    Parse <- crate::parser::ParseError,
    Bind <- BindError,
    Ddl <- ddl::DdlError,
    Exec <- bicdb_exec::ExecError,
    Txn <- bicdb_txn::write::TxnError,
    Segment <- bicdb_storage::segment::SegmentSpaceError,
);

/// **固定表的内容源**（`file$` 的行 = **控制文件的内存映像**）。
///
/// 为什么是端口：目录层（`bicdb-catalog`）**不持有控制文件**——它的生命周期在
/// 实例/工作区打开链（`catalog::api::fixed_table` 的两参形态就是这个理由）；
/// 所以"谁来读控制文件"由外层决定（CLI/服务用 `<工作区>/control/`）。
///
/// `None` = 这张固定表不认识（或没有内容源 ⇒ 查询具名拒绝，不静默空集）。
pub trait FixedTableSource {
    /// 按名取固定表（行 + 列形状）。
    fn fixed_table(&self, name: &str) -> Option<bicdb_catalog::fixed::FixedTable>;
}

/// 结果集的一列：名 + **形态**（驱动据此把文本/字节转成原生类型；CLI 据此对齐）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnMeta {
    /// 列名。
    pub name: String,
    /// 形态（数值/字节串/布尔）。
    pub kind: ColKind,
}

/// 一条语句的结果（协议层/CLI/驱动共用的形状）。
#[derive(Debug, Clone, PartialEq)]
pub enum QueryResult {
    /// 结果集（**值保持原样**——呈现/转换交给调用方：CLI 格式化、驱动转原生类型）。
    Rows {
        /// 列（名 + 形态）。
        columns: Vec<ColumnMeta>,
        /// 行（`bicdb-exec` 的值形态；`NULL` 是一等值）。
        rows: Vec<Vec<Value>>,
    },
    /// 影响行数（DML）。
    Affected(u64),
    /// DDL 回执。
    Ddl(String),
    /// 事务回执。
    Txn(String),
}

/// Workspace-local timer state for automatic full-text maintenance.
pub use crate::graph_sql::{FulltextScheduler, GraphSnapshot};

impl QueryResult {
    /// 结果集的显示宽（诊断/CLI 对齐）。
    #[must_use]
    pub fn row_count(&self) -> usize {
        match self {
            QueryResult::Rows { rows, .. } => rows.len(),
            _ => 0,
        }
    }
}

/// **会话**：一个工作区的编译-执行通道。
pub struct Session<'a, 'b, 'io, 'f> {
    pub(crate) pool: &'a BufferPool<'b>,
    pub(crate) engine: &'a Engine<'io, 'f, 'io, 'f>,
    pub(crate) catalog: &'a mut bicdb_catalog::Catalog<'io>,
    pub(crate) ws: [u8; 8],
    /// 显式事务（`BEGIN` 后持有）。
    pub(crate) txn: Option<TxnHandle>,
    /// 当前提交序号（新语句的快照水位）。
    pub(crate) seq: u64,
    /// **本语句的参数值**（按绑定期给出的**出现序**摆好；空 = 无参数）。
    pub(crate) exec_params: Vec<Value>,
    /// **管理面（DCL）的实例根**（`BICDB_HOME`；`None` = 没配 ⇒ DCL 具名拒绝）。
    dcl_home: Option<std::path::PathBuf>,
    /// **管理面（DCL）的 I/O**（全局控制文件走它；默认 OS 文件，测试注入内存）。
    dcl_io: Option<&'a dyn bicdb_workspace::io::FileIo>,
    /// **工作区文件面的供给方**（`CREATE/DROP WORKSPACE` 用；CLI 实现）。
    dcl_provisioner: Option<&'a dyn crate::dcl_exec::WorkspaceProvisioner>,
    /// **口令散列的迭代数**（`[auth] pbkdf2_iterations`；默认见
    /// `bicdb_common::pbkdf2::DEFAULT_ITERATIONS`）。写进存储串，故可调。
    dcl_pbkdf2_iterations: u32,
    /// **本会话的身份**（D6）：`None` = 管理面身份（本机/OS），`Some` = 认证过的主体。
    /// **私有 + 唯一的写口是 [`Session::authenticate`]**——身份不可能来自语句载荷。
    identity: Option<Identity>,
    private_auth: Option<&'a dyn auth::PrivateAuthProvider>,
    /// **因日志压力推过的完全检查点次数**（诊断/用例观测：CKPT 触发 ② 是否生效）。
    log_checkpoints: u64,
    /// **固定表的内容源**（`file$`；`None` ⇒ 查询具名拒绝——不静默给空集）。
    pub(crate) graph_internal: bool,
    /// One immutable, workspace-local full-text generation; manifest digests invalidate it.
    pub(crate) fulltext_cache: Option<(
        u32,
        [u8; 32],
        std::sync::Arc<bicdb_graph::fulltext::Generation>,
    )>,
    pub(crate) fulltext_interval_ms: u64,
    pub(crate) fulltext_batch_rows: usize,
    pub(crate) fulltext_maintenance_documents_loaded: usize,
    pub(crate) graph_limits: GraphLimits,
    pub(crate) graph_deadline: Option<bicdb_graph::Deadline>,
    fixed_source: Option<&'a dyn FixedTableSource>,
}

/// Durable state owned by one network connection while the service lends the
/// single catalog executor to other connections. It is deliberately opaque:
/// identity can only enter through [`Session::authenticate`], and transaction
/// handles can only enter through a live `Session`.
#[derive(Default)]
pub struct SessionState {
    txn: Option<TxnHandle>,
    seq: u64,
    identity: Option<Identity>,
    log_checkpoints: u64,
    fulltext_cache: Option<(
        u32,
        [u8; 32],
        std::sync::Arc<bicdb_graph::fulltext::Generation>,
    )>,
}

impl SessionState {
    /// Start one connection at the instance's current committed sequence.
    #[must_use]
    pub fn new(seq: u64) -> Self {
        Self {
            seq,
            ..Self::default()
        }
    }

    /// Whether this suspended connection owns an explicit transaction.
    #[must_use]
    pub fn in_transaction(&self) -> bool {
        self.txn.is_some()
    }

    /// Advance an idle connection to the latest committed sequence before its
    /// next request. Explicit transactions keep the sequence captured at
    /// `BEGIN`, providing repeatable visibility until COMMIT/ROLLBACK.
    pub fn refresh_committed(&mut self, seq: u64) {
        if self.txn.is_none() && self.seq != seq {
            self.seq = seq;
            self.fulltext_cache = None;
        }
    }
}

impl<'a, 'b, 'io, 'f> Session<'a, 'b, 'io, 'f> {
    /// 建会话。
    #[must_use]
    pub fn new(
        pool: &'a BufferPool<'b>,
        engine: &'a Engine<'io, 'f, 'io, 'f>,
        catalog: &'a mut bicdb_catalog::Catalog<'io>,
        seq: u64,
    ) -> Self {
        let ws = catalog.ws();
        Self {
            pool,
            engine,
            catalog,
            ws,
            txn: None,
            seq,
            exec_params: Vec::new(),
            dcl_home: None,
            dcl_io: None,
            dcl_provisioner: None,
            dcl_pbkdf2_iterations: bicdb_common::pbkdf2::DEFAULT_ITERATIONS,
            identity: None,
            private_auth: None,
            log_checkpoints: 0,
            graph_internal: false,
            fulltext_cache: None,
            fulltext_interval_ms: 5000,
            fulltext_batch_rows: 256,
            fulltext_maintenance_documents_loaded: 0,
            graph_limits: GraphLimits::default(),
            graph_deadline: None,
            fixed_source: None,
        }
    }

    /// Resume the opaque state of the same trusted service connection.
    ///
    /// # Panics
    /// Panics if the temporary session already owns a transaction or identity;
    /// mixing two connection states would violate isolation.
    pub fn resume_state(&mut self, state: &mut SessionState) {
        assert!(self.txn.is_none() && self.identity.is_none());
        self.txn = state.txn.take();
        self.seq = state.seq;
        self.identity = state.identity.take();
        self.log_checkpoints = state.log_checkpoints;
        self.fulltext_cache = state.fulltext_cache.take();
    }

    /// Suspend this session without rolling back its explicit transaction.
    /// The caller must later resume or close the state through a `Session`.
    pub fn suspend_state(&mut self, state: &mut SessionState) {
        assert!(state.txn.is_none() && state.identity.is_none());
        state.txn = self.txn.take();
        state.seq = self.seq;
        state.identity = self.identity.take();
        state.log_checkpoints = self.log_checkpoints;
        state.fulltext_cache = self.fulltext_cache.take();
    }

    /// Set a trusted instance-wide statement DETACH budget. Zero permits only
    /// isolated-node detachment; direct relationship DELETE remains available.
    ///
    /// # Errors
    /// Rejects values above the native graph edge ceiling, without changing it.
    pub fn set_graph_detach_edge_limit(&mut self, limit: usize) -> Result<(), SessionError> {
        if limit > MAX_DETACH_EDGE_LIMIT {
            return Err(SessionError::State(format!(
                "graph.detach_edge_limit must be 0..={MAX_DETACH_EDGE_LIMIT}"
            )));
        }
        self.graph_limits.max_detach_edges = limit;
        Ok(())
    }

    /// Set validated workspace-wide graph ceilings. Requests may only tighten
    /// them. Invalid configuration preserves all previously installed values.
    pub fn set_graph_limits(&mut self, limits: GraphLimits) -> Result<(), SessionError> {
        limits
            .validate_workspace()
            .map_err(|e| SessionError::State(e.to_string()))?;
        self.graph_limits = limits;
        self.fulltext_cache = None;
        Ok(())
    }

    /// Set instance defaults used by manual SYNC and maintenance diagnostics.
    ///
    /// # Errors
    /// Uses the same bounds as [`FulltextScheduler::new`].
    pub fn set_fulltext_defaults(
        &mut self,
        interval_ms: u64,
        batch_rows: usize,
    ) -> Result<(), SessionError> {
        FulltextScheduler::new(interval_ms, batch_rows)?;
        self.fulltext_interval_ms = interval_ms;
        self.fulltext_batch_rows = batch_rows;
        Ok(())
    }

    /// Publish at most one due full-text maintenance batch between client requests.
    /// This trusted service hook never executes inside a user transaction. Its
    /// internal management scope is restored before returning, including errors.
    ///
    /// # Errors
    /// Metadata, analysis or publication errors leave the failed batch pending.
    pub fn maintain_fulltext(
        &mut self,
        scheduler: &mut FulltextScheduler,
    ) -> Result<Option<String>, SessionError> {
        if self.in_transaction() {
            return Ok(None);
        }
        let identity = self.identity.take();
        let params = std::mem::take(&mut self.exec_params);
        let internal = std::mem::replace(&mut self.graph_internal, true);
        let result = self.poll_fulltext_scheduler(scheduler);
        self.graph_internal = internal;
        self.exec_params = params;
        self.identity = identity;
        result
    }

    /// **配管理面上下文**（DCL 的执行落点：实例根 + 全局控制文件的 I/O）。
    ///
    /// 不配也能跑——只是 DCL 语句会以"要一个部署根"具名拒绝（不猜位置）。
    pub fn set_dcl_context(
        &mut self,
        home_root: Option<std::path::PathBuf>,
        io: Option<&'a dyn bicdb_workspace::io::FileIo>,
    ) {
        self.dcl_home = home_root;
        self.dcl_io = io;
    }

    /// **给会话配固定表的内容源**（`file$`：控制文件的内存映像）。
    ///
    /// 不配也能跑——只是 `SELECT … FROM file$` 会以"没有内容源"具名拒绝
    /// （不猜位置：控制文件在哪是实例装配层的事）。
    pub fn set_fixed_table_source(&mut self, s: Option<&'a dyn FixedTableSource>) {
        self.fixed_source = s;
    }

    /// **给会话配工作区供给方**（`CREATE/DROP WORKSPACE` 的文件面）。
    pub fn set_workspace_provisioner(
        &mut self,
        p: Option<&'a dyn crate::dcl_exec::WorkspaceProvisioner>,
    ) {
        self.dcl_provisioner = p;
    }

    /// 工作区供给方（`dcl_exec` 用）。
    #[must_use]
    pub(crate) fn dcl_provisioner(&self) -> Option<&'a dyn crate::dcl_exec::WorkspaceProvisioner> {
        self.dcl_provisioner
    }

    /// **配口令散列的迭代数**（实例参数 `[auth] pbkdf2_iterations`）。
    pub fn set_pbkdf2_iterations(&mut self, iterations: u32) {
        if iterations > 0 {
            self.dcl_pbkdf2_iterations = iterations;
        }
    }

    /// 口令散列迭代数（`dcl_exec` 用）。
    #[must_use]
    pub(crate) fn pbkdf2_iterations(&self) -> u32 {
        self.dcl_pbkdf2_iterations
    }

    /// 管理面实例根（诊断用）。
    #[must_use]
    pub fn dcl_home_root(&self) -> Option<&std::path::Path> {
        self.dcl_home.as_deref()
    }

    /// 管理面 I/O。
    #[must_use]
    pub(crate) fn dcl_io(&self) -> Option<&'a dyn bicdb_workspace::io::FileIo> {
        self.dcl_io
    }

    /// 事务引擎（DCL 的字典写侧要它开 DDL 事务）。
    #[must_use]
    pub(crate) fn engine_ref(&self) -> &'a Engine<'io, 'f, 'io, 'f> {
        self.engine
    }

    /// 目录（DCL 的字典写侧要它；**本工作区的**那个）。
    #[must_use]
    pub(crate) fn catalog_mut(&mut self) -> &mut bicdb_catalog::Catalog<'io> {
        self.catalog
    }

    /// 本会话是否含 PUBLIC 管理字典；这是工作区事实，不是认证身份。
    #[must_use]
    pub fn on_public_workspace(&self) -> bool {
        self.catalog.is_public()
    }

    // ───────────────────────── 身份（D6） ─────────────────────────

    /// **认证**（`AUTH` 的落点）：主体名 + 口令 ⇒ 身份。
    ///
    /// 三条前提都是**具名拒绝**：
    /// - 只在 `PUBLIC` 工作区上提供（`user$` 在那里；普通工作区实例按
    ///   控制套接字的文件权限判身份——本机 = OS 身份）；
    /// - 一条连接只认证一次（重认证请重连——身份是会话的属性，不是请求的属性）；
    /// - 校验与状态检查的次序、失败文案、时序口径见 [`crate::auth`]。
    ///
    /// # Errors
    /// 认证不通过（[`crate::auth::AuthError`]）或前提不满足（[`SessionError::Auth`]）。
    pub fn authenticate(&mut self, name: &str, password: &str) -> Result<Identity, SessionError> {
        if !self.on_public_workspace() && self.private_auth.is_none() {
            return Err(SessionError::Auth(
                "本实例不是 PUBLIC 工作区 ⇒ 不做口令认证（`user$` 在 PUBLIC 里）——\
                 普通工作区的本机访问按控制套接字的文件权限（OS 身份）判定"
                    .to_owned(),
            ));
        }
        if let Some(id) = &self.identity {
            return Err(SessionError::Auth(format!(
                "本连接已认证为{}——重认证请重连（身份是连接的属性，不是请求的）",
                id.describe()
            )));
        }
        let id = if self.on_public_workspace() {
            auth::authenticate(self.catalog, name, password, self.dcl_pbkdf2_iterations)
                .map_err(|e| SessionError::Auth(e.to_string()))?
        } else {
            auth::authenticate_private(self.private_auth.expect("checked"), name, password, self.ws)
                .map_err(SessionError::Auth)?
        };
        self.identity = Some(id.clone());
        Ok(id)
    }

    /// Install the native service's trusted PUBLIC authentication provider.
    pub fn set_private_authenticator(
        &mut self,
        provider: Option<&'a dyn auth::PrivateAuthProvider>,
    ) {
        self.private_auth = provider;
    }

    /// Resolve only the authenticated principal's active private workspaces.
    pub fn route_owned_workspace(
        &mut self,
        selection: Option<&str>,
    ) -> Result<(u64, u64, String, String), SessionError> {
        if !self.on_public_workspace() {
            return Err(SessionError::Auth("ROUTE 仅由 PUBLIC 服务提供".into()));
        }
        let id = self
            .identity
            .as_ref()
            .ok_or_else(|| SessionError::Auth("ROUTE 必须先认证".into()))?;
        if id.is_expired() {
            return Err(SessionError::Auth(
                "口令已过期，须先在 PUBLIC 修改口令".into(),
            ));
        }
        let user_id = id.user_id();
        let mut owned = bicdb_catalog::dcl::workspaces_of_user(self.catalog, user_id)
            .map_err(|e| SessionError::Auth(e.to_string()))?;
        owned.retain(|w| w.status == bicdb_catalog::dcl::ws_status::ACTIVE);
        if let Some(selection) = selection {
            owned.retain(|w| w.name == selection || w.workspace_id.to_string() == selection);
        }
        if owned.len() != 1 {
            return Err(SessionError::Auth(
                "工作区选择失败：必须选择本人唯一的有效私有工作区；有多个时请指定 dbworkspace"
                    .into(),
            ));
        }
        let ws = owned.remove(0);
        let workspace = bicdb_workspace::WorkspaceId::from_raw(ws.workspace_id)
            .ok_or_else(|| SessionError::Auth("工作区登记无效".into()))?;
        let home = self
            .dcl_home
            .as_ref()
            .ok_or_else(|| SessionError::Auth("缺少 PUBLIC 注册表".into()))?;
        let io = self
            .dcl_io
            .ok_or_else(|| SessionError::Auth("缺少注册表 IO".into()))?;
        let paths = crate::dcl_exec::DclContext::new(home.clone(), io).global_ctl_paths();
        let registry = bicdb_storage::globalctl::GlobalControlFile::open(io, &paths[0], &paths[1])
            .map_err(|e| SessionError::Auth(e.to_string()))?;
        let entry = registry
            .workspace_by_id(workspace)
            .map_err(|e| SessionError::Auth(e.to_string()))?
            .filter(|w| w.status == bicdb_storage::globalctl::WS_ACTIVE)
            .ok_or_else(|| SessionError::Auth("工作区注册表不可用".into()))?;
        let root = String::from_utf8(entry.root)
            .map_err(|_| SessionError::Auth("工作区路径非 UTF-8".into()))?;
        Ok((user_id, ws.workspace_id, ws.name, root))
    }

    /// **日志要切换而下一组未降级 ⇒ 推一次完全检查点**（CKPT 的"组满被迫"）。
    ///
    /// 为什么要在这里做：写者只有**真正写满当前组**时才撞上 `Blocked`——那时
    /// 语句已失败回滚（报"日志切换等待检查点（无可复用组）"）。会话在**执行之前**
    /// 问一句"再写下去会不会撞墙"（[`Engine::log_switch_blocked`]，只读探测），
    /// 是挡就地推一次检查点：组降级 ⇒ 写者继续。Oracle 的"日志切换触发检查点"
    /// 与 PG 的 `max_wal_size` 触发检查点都是这条（证据包 `checkpoint-on-demand-*`）。
    ///
    /// 显式事务也可在完整语句之间推进检查点：同时保存数据、undo 和事务槽，
    /// 并补齐已提交槽标记，崩溃恢复从持久化事务表识别跨检查点的输家。
    ///
    /// # Errors
    /// 检查点失败（写回/控制文件/日志）——**如实报出**，不吞。
    fn checkpoint_on_log_pressure(&mut self) -> Result<(), SessionError> {
        if !self.engine.log_switch_blocked() {
            return Ok(());
        }
        self.engine
            .checkpoint_full(self.ws)
            .map_err(|e| SessionError::State(format!("日志压力触发的检查点失败：{e}")))?;
        self.log_checkpoints += 1;
        Ok(())
    }

    /// 本会话因日志压力推过的完全检查点次数（诊断/用例）。
    #[must_use]
    pub fn log_checkpoints(&self) -> u64 {
        self.log_checkpoints
    }

    /// 本会话的身份（`None` = 管理面身份）。
    #[must_use]
    pub fn identity(&self) -> Option<&Identity> {
        self.identity.as_ref()
    }

    /// 是不是**管理面身份**（本机/OS；DCL 的资格）。
    #[must_use]
    pub fn is_management_identity(&self) -> bool {
        self.identity.is_none()
    }

    /// **身份资格**（在绑定与对象查找**之前**判；`REQ-SQL-005` 口径）。
    ///
    /// 管理面身份不受限；具名主体按 [`crate::auth`] 与模块头的三条规则判。
    fn check_identity(&self, stmt: &crate::ast::Stmt) -> Result<(), SessionError> {
        use crate::ast::Stmt as S;
        let Some(id) = &self.identity else {
            return Ok(()); // 管理面身份：不设限（本机 = OS 身份）
        };
        // ① `EXPIRE` 的受限会话：只留"本人改密"这一条路。
        if id.is_expired() {
            return if crate::dcl_exec::is_self_password_change(stmt, id.name()) {
                Ok(())
            } else {
                Err(SessionError::Auth(format!(
                    "口令已过期（`EXPIRE`）⇒ **受限会话**：只允许本人改密——\
                     ALTER USER {} IDENTIFIED BY '<新口令>' REPLACE '<旧口令>'\
                     （改完即解除限制）",
                    id.name()
                )))
            };
        }
        // ② 管理面语句：主体只能对自己改密（Oracle 的"本人改密"口径）。
        if crate::dcl_exec::is_management(stmt)
            || matches!(stmt, S::Drop(d) if d.remove_type == crate::ast::ObjectType::Workspace)
        {
            return if crate::dcl_exec::is_self_password_change(stmt, id.name()) {
                Ok(())
            } else {
                Err(SessionError::Auth(format!(
                    "管理面语句需要**管理面身份**（本机 = 控制套接字的文件权限）——\
                     主体 `{}` 只能对自己用 `ALTER USER … IDENTIFIED BY … REPLACE …`",
                    id.name()
                )))
            };
        }
        // A private identity has been checked by PUBLIC against this exact ws.
        if !self.on_public_workspace() {
            return Ok(());
        }
        // ③ 其余语句按类判：`public` 是共享内容，**对主体只读**（SELECT/事务随便）。
        match stmt {
            S::ShowTables(_) | S::ShowGraphs(_) | S::Select(_) | S::Transaction(_) => Ok(()),
            S::GraphIndex(g)
                if matches!(
                    g.action,
                    crate::ast::GraphIndexAction::Show
                        | crate::ast::GraphIndexAction::FulltextShow
                        | crate::ast::GraphIndexAction::FulltextSearch { .. }
                ) =>
            {
                Ok(())
            }
            S::Cypher(c) => {
                if bicdb_graph::parse(&c.query)
                    .map_err(|e| SessionError::State(e.to_string()))?
                    .is_read_only()
                {
                    Ok(())
                } else {
                    Err(SessionError::Auth("PUBLIC 图数据对主体只读".into()))
                }
            }
            S::VariableSet(_) => Ok(()), // 会话变量（白名单在绑定期判）
            S::Insert(_) | S::Update(_) | S::Delete(_) => Err(SessionError::Auth(format!(
                "PUBLIC 工作区对所有主体**只读**——主体 `{}` 不能写它的数据\
                 （写自己的数据要在自己的会话/工作区里；见 `doc/用户与配额管理设计_v0.1.md` §4）",
                id.name()
            ))),
            // `DROP WORKSPACE` 是管理面动作（`is_management` 不收 `Drop`——
            // 它同时管表/索引/图；按对象类型分派，与 `execute_dcl` 同一判据）。
            S::Drop(d) if d.remove_type == crate::ast::ObjectType::Workspace => {
                Err(SessionError::Auth(format!(
                    "管理面语句需要**管理面身份**——主体 `{}` 不能执行",
                    id.name()
                )))
            }
            // 建表/建索引/删表删索引/建图：都是写（含改字典）。
            S::CreateTable(_) | S::Index(_) | S::Drop(_) | S::CreateGraph(_) | S::GraphIndex(_) => {
                Err(SessionError::Auth(format!(
                    "PUBLIC 工作区对所有主体**只读**——主体 `{}` 不能改它的结构",
                    id.name()
                )))
            }
            // 其余管理面语句（上面 `is_management` 已拦）。**穷尽匹配**：新语句
            // 必须在这里表态，不能"默认放行"。
            S::CreateWorkspace(_)
            | S::AlterWorkspace(_)
            | S::CreateFilesystem(_)
            | S::AlterFilesystem(_)
            | S::DropFilesystem(_)
            | S::CreateUser(_)
            | S::AlterUser(_)
            | S::DropUser(_)
            | S::AlterDatabase(_) => Err(SessionError::Auth(format!(
                "管理面语句需要**管理面身份**——主体 `{}` 不能执行",
                id.name()
            ))),
        }
    }

    /// **解除本会话的过期限制**（本人改密成功后：`user$` 的状态已转 `ACTIVE`）。
    ///
    /// 为什么会话里也要解：受限是**会话**的属性——字典改了而会话还拦着，
    /// 用户就得"改完密再重连一次"，那不是 MySQL 的形态（它改完即放行）。
    pub(crate) fn clear_identity_expiry(&mut self) {
        if let Some(id) = self.identity.as_mut() {
            id.clear_expired();
        }
    }

    /// 当前提交序号。
    #[must_use]
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// **列定义**（`DESCRIBE` 用）：`(列名, 可空, 类型码, 声明长度)`。
    ///
    /// 走会话自己的目录借用——**不动事务状态**（`DESCRIBE` 在显式事务里也该能用）。
    pub fn describe_columns(
        &mut self,
        name: &str,
    ) -> Result<Vec<(String, bool, u32, u32)>, SessionError> {
        if let Some(id) = &self.identity {
            if id.is_expired() {
                return Err(SessionError::Auth(
                    "口令已过期（EXPIRE）⇒ 受限会话：只允许本人改密".to_owned(),
                ));
            }
        }
        let snapshot = self.snapshot();
        let is_admin = self.is_management_identity();
        let mut view =
            CatalogViewImpl::new(self.catalog, snapshot).with_graph_access(self.graph_internal);
        let mut resolver =
            NameResolver::with_policy(&mut view, crate::bind::ResolvePolicy { is_admin });
        let cols = match resolver.resolve_table(name)? {
            crate::bind::ResolvedName::Object(obj) => resolver.view().columns(obj.obj)?,
            crate::bind::ResolvedName::FixedTable(name) => resolver
                .view()
                .fixed_columns(name)?
                .ok_or_else(|| SessionError::State(format!("固定表 `{name}` 缺列定义")))?,
        };
        Ok(cols
            .into_iter()
            .map(|c| (c.name, c.nullable, c.type_code, c.length))
            .collect())
    }

    /// **接管一个既有的显式事务**（直连模式：连接 = 进程，事务要跨语句）。
    ///
    /// 为什么需要：`Session` 借住实例（池/引擎/目录都是借来的），
    /// 长连接的**事务状态**因此不能住在 `Session` 里——`bicdbcli --direct`
    /// 每执行一句就新建一个会话的话，`BEGIN` 建的事务会在语句收尾的
    /// `Drop` 里被回滚，`COMMIT` 只回一句"没有活动事务"（**静默降级**）。
    /// 做法：事务句柄由**连接**保管，语句执行时借给会话
    /// （[`Session::adopt_txn`]），执行完交回（[`Session::release_txn`]）。
    #[must_use]
    pub fn adopt_txn(
        &mut self,
        txn: Option<bicdb_txn::engine::TxnHandle>,
    ) -> Option<bicdb_txn::engine::TxnHandle> {
        std::mem::replace(&mut self.txn, txn)
    }

    /// **交回显式事务**（语句结束；`None` = 该语句把事务收尾了）。
    #[must_use]
    pub fn release_txn(&mut self) -> Option<bicdb_txn::engine::TxnHandle> {
        self.txn.take()
    }

    /// 有没有未收尾的显式事务。
    #[must_use]
    pub fn in_transaction(&self) -> bool {
        self.txn.is_some()
    }

    /// **跑一条语句**（`sql` 可以是多条以 `;` 分隔；无参数）。
    ///
    /// 语句声明了参数（`:name`）而没有给值 ⇒ [`SessionError::Params`]——
    /// **不**让执行器抛"参数下标越界"（那是实现细节，不是用户看到的错）。
    pub fn execute(&mut self, sql: &str) -> Result<Vec<QueryResult>, SessionError> {
        self.execute_with_params(sql, &[])
    }

    /// **带参数跑一条语句**：`named` = 调用方按名给的值（语句里 `:name`）。
    ///
    /// 摆位规则 = **绑定期记下的出现序**（`BoundParams::list()`）——同一参数在
    /// 语句里出现多次只占一个位（绑定期已按名归并），因此这里按名查值即可。
    pub fn execute_with_params(
        &mut self,
        sql: &str,
        named: &[(&str, Value)],
    ) -> Result<Vec<QueryResult>, SessionError> {
        let stmts = parse(sql)?;
        // 多给/重复命名参数必须在任何写入之前拒绝，不能提交后才返回参数错误。
        if !named.is_empty() {
            let tokens =
                crate::lexer::tokenize(sql).map_err(|e| SessionError::Params(e.to_string()))?;
            let declared: std::collections::HashSet<&str> = tokens
                .iter()
                .filter_map(|t| match &t.kind {
                    crate::lexer::TokenKind::Param(n) => Some(n.as_str()),
                    _ => None,
                })
                .collect();
            let mut supplied = std::collections::HashSet::new();
            for (name, _) in named {
                if !supplied.insert(*name) {
                    return Err(SessionError::Params(format!("参数 `:{name}` 给了多次")));
                }
            }
            // 缺参优先于多给，沿 SQL 出现序报告，保持驱动已有的错误契约。
            for token in &tokens {
                if let crate::lexer::TokenKind::Param(name) = &token.kind {
                    if !supplied.contains(name.as_str()) {
                        return Err(SessionError::Params(format!("缺参数值 `:{name}`")));
                    }
                }
            }
            for (name, _) in named {
                if !declared.contains(name) {
                    return Err(SessionError::Params(format!(
                        "整批语句都没用到参数 `:{name}`"
                    )));
                }
            }
        }
        let mut out = Vec::with_capacity(stmts.len());
        // **参数是"整批共用"的**：一条语句只用其中几个是正常的
        // （`BEGIN; INSERT … :p; COMMIT`）——"多给"只在**整批**都没用到时才报。
        let mut used: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for stmt in &stmts {
            out.push(self.execute_one(stmt, named, &mut used)?);
        }
        for (name, _) in named {
            if !used.contains(name) {
                return Err(SessionError::Params(format!(
                    "整批语句都没用到参数 `:{name}`"
                )));
            }
        }
        Ok(out)
    }

    pub(crate) fn snapshot(&self) -> CommitSeq {
        CommitSeq::from_raw(self.seq + 1).expect("48 位域内")
    }

    fn execute_one<'c>(
        &mut self,
        stmt: &crate::ast::Stmt,
        named: &'c [(&'c str, Value)],
        used: &mut std::collections::HashSet<&'c str>,
    ) -> Result<QueryResult, SessionError> {
        // ⓪ **身份资格先于绑定与对象查找**（REQ-SQL-005 口径）：
        //    具名主体的只读/自助规则、过期会话的受限规则都在这里拦。
        self.check_identity(stmt)?;
        // ⓪.5 **日志要被挡就先推一次检查点**（§11.7 的 CKPT 触发 ②"组满被迫"；
        //      触发 ①"周期发布"随后台角色切片——`crates/daemon` 尚未接电）。
        self.checkpoint_on_log_pressure()?;
        if let Some(result) = self.execute_graph_statement(stmt) {
            return result;
        }
        // ① 解析已完成（调用方）；② 绑定。
        let snapshot = self.snapshot();
        // **解析策略随会话身份**（先取出来：下面借 `self.catalog` 时不能再借 `self`）：
        // 管理面身份（本机/OS）= admin——`public` 的管理元数据 `file$` 只有它看得见
        // （`spec/SQL.md` 待冻结项 47）。
        let is_admin = self.is_management_identity();
        let bound = {
            let mut view =
                CatalogViewImpl::new(self.catalog, snapshot).with_graph_access(self.graph_internal);
            let mut resolver =
                NameResolver::with_policy(&mut view, crate::bind::ResolvePolicy { is_admin });
            bind_statement(&mut resolver, stmt)?
        };
        // **参数摆位**（绑定期的清单 + 调用方按名给的值）。
        self.exec_params = place_params(&bound, named, used)?;

        match bound {
            crate::bind::BoundStatement::ShowTables => self.show_tables(),
            crate::bind::BoundStatement::Transaction(kind) => self.transaction(kind),
            // **DCL**：语义全在执行层（`dcl_exec`：资格 → 对象 → 两处写）。
            crate::bind::BoundStatement::Dcl(stmt) => self.execute_dcl(&stmt),
            crate::bind::BoundStatement::Ddl(d) => {
                if self.in_transaction() {
                    return Err(SessionError::State(
                        "活动事务中发 DDL ⇒ 拒绝（REQ-TXN-016）".to_owned(),
                    ));
                }
                let summary = ddl_summary(&d);
                match d {
                    crate::bind::BoundDdl::CreateTable(spec) => {
                        create_table(self.catalog, self.engine, &spec)?;
                    }
                    crate::bind::BoundDdl::CreateIndex(spec) => {
                        create_index(self.catalog, self.engine, &spec)?;
                    }
                    crate::bind::BoundDdl::DropTable(name) => {
                        drop_table(self.catalog, self.engine, &name)?;
                    }
                    crate::bind::BoundDdl::DropIndex(name) => {
                        drop_index(self.catalog, self.engine, &name)?;
                    }
                }
                // DDL 之后装载戳前移（目录已 advance_commit）。
                self.seq = self.catalog.current_seq();
                Ok(QueryResult::Ddl(summary))
            }
            crate::bind::BoundStatement::Select(_)
            | crate::bind::BoundStatement::SetOp(_)
            | crate::bind::BoundStatement::Insert(_) => {
                let plan = {
                    let mut view = CatalogViewImpl::new(self.catalog, snapshot)
                        .with_graph_access(self.graph_internal);
                    plan_statement(&bound, &mut view)?
                };
                let Some(plan) = plan else {
                    return Err(SessionError::State("该语句无物理计划".to_owned()));
                };
                self.execute_plan(&plan, snapshot)
            }
            // **DML（UPDATE/DELETE）**：谓词按**表行坐标**给 `run_dml`（物化时判——
            // 计划里不挂 `Filter`，见 `run_dml` 的模块注释；**值不能忘**：
            // 忘掉谓词就是"WHERE 不生效"，那是静默错）。
            crate::bind::BoundStatement::Update(ref u) => {
                let filter = u.filter.clone();
                let plan = {
                    let mut view = CatalogViewImpl::new(self.catalog, snapshot)
                        .with_graph_access(self.graph_internal);
                    plan_statement(&bound, &mut view)?
                };
                let Some(plan) = plan else {
                    return Err(SessionError::State("该语句无物理计划".to_owned()));
                };
                self.run_dml(&plan, snapshot, DmlKind::Update, filter)
            }
            crate::bind::BoundStatement::Delete(ref d) => {
                let filter = d.filter.clone();
                let plan = {
                    let mut view = CatalogViewImpl::new(self.catalog, snapshot)
                        .with_graph_access(self.graph_internal);
                    plan_statement(&bound, &mut view)?
                };
                let Some(plan) = plan else {
                    return Err(SessionError::State("该语句无物理计划".to_owned()));
                };
                self.run_dml(&plan, snapshot, DmlKind::Delete, filter)
            }
        }
    }

    fn show_tables(&mut self) -> Result<QueryResult, SessionError> {
        let tables = self
            .catalog
            .tables()
            .map_err(|e| SessionError::State(format!("表目录：{e}")))?;
        let attachment_virtual = self.identity.is_some()
            && !self.on_public_workspace()
            && [
                "ag_schema_version",
                "ag_attachment_schema",
                "ag_attachment",
                "ag_attachment_chunk",
                "ag_session",
            ]
            .iter()
            .all(|name| tables.iter().any(|table| table.name == *name));
        let mut rows: Vec<Vec<Value>> = tables
            .into_iter()
            .map(|t| {
                let kind = if bicdb_catalog::dict::DICT_TABLES
                    .iter()
                    .any(|d| d.name == t.name)
                {
                    "DICTIONARY"
                } else {
                    "USER"
                };
                vec![
                    Value::Bytes(t.name.into_bytes()),
                    Value::Bytes(kind.as_bytes().to_vec()),
                    dict_to_value(&bicdb_catalog::row::DictValue::Num(u64::from(t.obj))),
                ]
            })
            .collect();
        if attachment_virtual {
            rows.push(vec![
                Value::Bytes(b"attachment$".to_vec()),
                Value::Bytes(b"VIRTUAL".to_vec()),
                Value::Null,
            ]);
        }
        // Fixed tables have no obj$ row: only list a live provider, never invent
        // unimplemented asset/graph tables from design documents.
        if self
            .fixed_source
            .and_then(|s| s.fixed_table("file$"))
            .is_some()
        {
            rows.push(vec![
                Value::Bytes(b"file$".to_vec()),
                Value::Bytes(b"FIXED".to_vec()),
                Value::Null,
            ]);
        }
        rows.sort_by(|a, b| match (&a[0], &b[0]) {
            (Value::Bytes(a), Value::Bytes(b)) => a.cmp(b),
            _ => std::cmp::Ordering::Equal,
        });
        Ok(QueryResult::Rows {
            columns: vec![
                ColumnMeta {
                    name: "table_name".into(),
                    kind: ColKind::Bytes,
                },
                ColumnMeta {
                    name: "table_kind".into(),
                    kind: ColKind::Bytes,
                },
                ColumnMeta {
                    name: "object_id".into(),
                    kind: ColKind::Number,
                },
            ],
            rows,
        })
    }

    // ───────────────────────── 事务控制 ─────────────────────────

    pub(crate) fn transaction(
        &mut self,
        kind: crate::ast::TransactionStmtKind,
    ) -> Result<QueryResult, SessionError> {
        use crate::ast::TransactionStmtKind as K;
        match kind {
            K::Begin => {
                if self.in_transaction() {
                    return Err(SessionError::State("事务已开（嵌套 BEGIN）".to_owned()));
                }
                let txn = self.engine.begin()?;
                self.txn = Some(txn);
                Ok(QueryResult::Txn("BEGIN".to_owned()))
            }
            K::Commit => {
                let mut txn = self
                    .txn
                    .take()
                    .ok_or_else(|| SessionError::State("没有活动事务".to_owned()))?;
                let seq = self.engine.commit(&mut txn)?;
                self.seq = seq.as_raw();
                Ok(QueryResult::Txn(format!("COMMIT（提交序号 {seq}）")))
            }
            K::Rollback => {
                let mut txn = self
                    .txn
                    .take()
                    .ok_or_else(|| SessionError::State("没有活动事务".to_owned()))?;
                let n = self.engine.rollback(&mut txn)?;
                Ok(QueryResult::Txn(format!("ROLLBACK（撤销 {n} 条）")))
            }
        }
    }

    // ───────────────────────── 计划执行 ─────────────────────────

    /// Materialize sanitized metadata before execution borrows the engine.
    /// All references to a dictionary in this statement share the same image.
    fn readonly_cursors(
        &mut self,
        plan: &PhysicalPlan,
        snapshot: CommitSeq,
    ) -> Result<Vec<Option<MemoryCursor>>, SessionError> {
        if plan.sources.iter().any(|s| s.fixed == Some("graph_table")) {
            self.with_graph_deadline(self.graph_limits.max_elapsed_ms, |s| {
                s.readonly_cursors_scoped(plan, snapshot)
            })
        } else {
            self.readonly_cursors_scoped(plan, snapshot)
        }
    }
    fn readonly_cursors_scoped(
        &mut self,
        plan: &PhysicalPlan,
        snapshot: CommitSeq,
    ) -> Result<Vec<Option<MemoryCursor>>, SessionError> {
        let mut images = std::collections::BTreeMap::new();
        let mut cursors = Vec::with_capacity(plan.sources.len());
        // Shared bound for all GRAPH_TABLE sources, including set-operation branches.
        let mut graph_bytes = 0usize;
        let mut remaining_edges = self.graph_limits.max_edge_expansions;
        let mut remaining_work = self.graph_limits.max_expansions;
        for source in &plan.sources {
            let Some(name) = source.fixed else {
                cursors.push(None);
                continue;
            };
            if name == "graph_table" {
                let source_query = source
                    .graph_table
                    .as_ref()
                    .ok_or_else(|| SessionError::State("missing GRAPH_TABLE plan".into()))?;
                let rows =
                    self.graph_table_rows(source_query, &mut remaining_edges, &mut remaining_work)?;
                let zero = bicdb_storage::rowid::RowId::from_bytes(&[0u8; 6]);
                let mut encoded = Vec::with_capacity(rows.len());
                for row in rows {
                    self.check_graph_deadline("GRAPH_TABLE materialization")?;
                    let bytes = bicdb_exec::encode_row(&Row::new(row), &source.shape)?;
                    graph_bytes = graph_bytes.saturating_add(bytes.len()).saturating_add(32);
                    if graph_bytes > self.graph_limits.max_text_bytes {
                        return Err(SessionError::State(format!(
                            "GRAPH_TABLE statement materialization exceeds {} bytes",
                            self.graph_limits.max_text_bytes
                        )));
                    }
                    encoded.push((zero, bytes));
                }
                self.check_graph_deadline("after GRAPH_TABLE materialization")?;
                cursors.push(Some(MemoryCursor::new(encoded)));
                continue;
            }
            if name == "attachment$" || name == "attachment_grep" {
                let rows =
                    self.attachment_virtual_rows(name, source.function_args.as_deref(), snapshot)?;
                cursors.push(Some(values_cursor(&rows, &source.shape)?));
                continue;
            }
            if !images.contains_key(name) {
                let cursor = if crate::bind::dictionary_columns(name).is_some() {
                    let rows = self
                        .catalog
                        .sql_dictionary_rows(snapshot, name)
                        .map_err(|error| BindError::Catalog(error.to_string()))?;
                    dictionary_cursor(&rows, &source.shape)?
                } else {
                    fixed_cursor(self.fixed_source, name, &source.shape)?
                };
                images.insert(name, cursor);
            }
            cursors.push(images.get(name).cloned());
        }
        Ok(cursors)
    }

    fn execute_plan(
        &mut self,
        plan: &PhysicalPlan,
        snapshot: CommitSeq,
    ) -> Result<QueryResult, SessionError> {
        match plan.kind {
            PlanKind::Select => self.run_select(plan, snapshot),
            PlanKind::Insert => self.run_insert(plan, snapshot),
            // UPDATE/DELETE 不走这条（`execute_one` 直接调 `run_dml` 带谓词）；
            // 真走到这里说明有人新开了口子——具名拒绝，不给"WHERE 丢了"的机会。
            PlanKind::Update | PlanKind::Delete => Err(SessionError::State(
                "DML 计划必须经 `run_dml`（带谓词）执行——`execute_plan` 不处理它".to_owned(),
            )),
        }
    }

    fn run_select(
        &mut self,
        plan: &PhysicalPlan,
        snapshot: CommitSeq,
    ) -> Result<QueryResult, SessionError> {
        // **空 sources 是合法的**（`SELECT 1` 走 `SingleRow`——没有表要扫；
        // `open_cursor` 只在有 `SeqScan` 的树里被调，越界即具名错误）。
        // **每个行源各一份扫描边界**（段头经池 + 数据页清单）——两表连接就是两份；
        // `open_cursor` 按 `SourceId` 分派（此前忽略 id、任何源都开第一个）。
        // **固定表没有段**（行即时产生）⇒ 它的那一格是 `None`，开游标时走内存路。
        let mut per_source: Vec<Option<(u16, Vec<u32>)>> = Vec::with_capacity(plan.sources.len());
        for src in &plan.sources {
            if src.fixed.is_some() {
                per_source.push(None);
                continue;
            }
            let (file_id, blocks) = {
                let seg = Segment::open_pooled(
                    self.pool,
                    self.catalog.file_mut(),
                    src.seg_block,
                    self.ws,
                )?;
                let fid = seg.file_id();
                let hwm = seg.hwm();
                (fid, seg.data_blocks(hwm))
            };
            per_source.push(Some((file_id, blocks)));
        }
        let node = plan.node.clone();
        // 列形态来自计划输出（`RowShape`），列名来自计划元数据。
        let columns: Vec<ColumnMeta> = plan
            .output_names
            .iter()
            .enumerate()
            .map(|(i, name)| ColumnMeta {
                name: name.clone(),
                kind: plan.output.cols.get(i).copied().unwrap_or(ColKind::Bytes),
            })
            .collect();
        let params_in = self.exec_params.clone();
        let pool = self.pool;
        // **读己所写**：本会话若有活动事务，它的未提交改动要看得见
        // （`BEGIN; INSERT; SELECT` 看得到刚插的行）——视角里带上它。
        let own = self.txn.as_ref().map(TxnHandle::id);
        let view = bicdb_storage::cr::ReadView::new(snapshot).with_own(own);
        // **固定表的内容源**（借用拷出来——闭包里再借 `self` 会与下面的 `self.engine` 冲突）。
        let readonly = self.readonly_cursors(plan, snapshot)?;
        // 扫描期与 CR 共持撤销链（**读上下文**；语句内完成，不做长事务）。
        let rows = self.engine.with_read_context(|pool_ref, chain| {
            let mut open = |src: bicdb_exec::SourceId| {
                let idx = src as usize;
                let entry = per_source.get(idx).ok_or_else(|| {
                    bicdb_exec::ExecError::Spill(format!("行源 {src} 不在计划里"))
                })?;
                // **固定表**：行由引擎即时产生（内存游标，没有段、没有 RID）。
                if let Some(cursor) = readonly.get(idx).and_then(Option::as_ref) {
                    return Ok(Box::new(cursor.clone()) as Box<dyn RowCursor>);
                }
                let (file_id, blocks) = entry
                    .as_ref()
                    .ok_or_else(|| bicdb_exec::ExecError::Spill(format!("行源 {src} 没有段")))?;
                Ok(Box::new(HeapScanner::new(
                    pool_ref,
                    chain,
                    view,
                    *file_id,
                    blocks.clone(),
                )) as Box<dyn RowCursor>)
            };
            let envx = ExecEnv {
                pool: pool_ref,
                // **索引扫描要用撤销链**（回表 + CR 重建）——从前只有 `SeqScan`
                // 时才经 `open` 闭包拿链，故这里是 `None`；接了 `IndexScan` 之后
                // 计划里可能根本没有行源，链必须从环境走。
                chain: Some(chain),
                spill: None,
                writer: None,
            };
            let mut op = build(&node, &envx, &mut open)?;
            let mut cx = ExecContext::new(snapshot)
                .with_own(own)
                .with_params(&params_in);
            collect(op.as_mut(), &mut cx)
        })?;
        let _ = pool;
        Ok(QueryResult::Rows {
            columns,
            rows: rows.into_iter().map(|r| r.values).collect(),
        })
    }

    /// 会话侧：**跑一棵只读子树并收齐行**（`INSERT … SELECT` 的来源；本版物化）。
    ///
    /// 行源定位表用**外层计划**的 `sources`（来源的 id 已在计划期平移）。
    fn materialize(
        &mut self,
        node: &bicdb_exec::PlanNode,
        plan: &PhysicalPlan,
        snapshot: CommitSeq,
    ) -> Result<Vec<Vec<Value>>, SessionError> {
        // **固定表没有段**（行即时产生）⇒ 它的那一格是 `None`，开游标时走内存路。
        let mut per_source: Vec<Option<(u16, Vec<u32>)>> = Vec::with_capacity(plan.sources.len());
        for src in &plan.sources {
            if src.fixed.is_some() {
                per_source.push(None);
                continue;
            }
            let (file_id, blocks) = {
                let seg = Segment::open_pooled(
                    self.pool,
                    self.catalog.file_mut(),
                    src.seg_block,
                    self.ws,
                )?;
                let fid = seg.file_id();
                let hwm = seg.hwm();
                (fid, seg.data_blocks(hwm))
            };
            per_source.push(Some((file_id, blocks)));
        }
        let params_in = self.exec_params.clone();
        let own = self.txn.as_ref().map(TxnHandle::id);
        let view = bicdb_storage::cr::ReadView::new(snapshot).with_own(own);
        let readonly = self.readonly_cursors(plan, snapshot)?;
        let node = node.clone();
        let rows = self.engine.with_read_context(|pool_ref, chain| {
            let mut open = |src: bicdb_exec::SourceId| {
                let idx = src as usize;
                let entry = per_source.get(idx).ok_or_else(|| {
                    bicdb_exec::ExecError::Spill(format!("行源 {src} 不在计划里"))
                })?;
                // **固定表**：行由引擎即时产生（内存游标，没有段、没有 RID）。
                if let Some(cursor) = readonly.get(idx).and_then(Option::as_ref) {
                    return Ok(Box::new(cursor.clone()) as Box<dyn RowCursor>);
                }
                let (file_id, blocks) = entry
                    .as_ref()
                    .ok_or_else(|| bicdb_exec::ExecError::Spill(format!("行源 {src} 没有段")))?;
                Ok(Box::new(HeapScanner::new(
                    pool_ref,
                    chain,
                    view,
                    *file_id,
                    blocks.clone(),
                )) as Box<dyn RowCursor>)
            };
            let envx = ExecEnv {
                pool: pool_ref,
                // **索引扫描要用撤销链**（回表 + CR 重建）——从前只有 `SeqScan`
                // 时才经 `open` 闭包拿链，故这里是 `None`；接了 `IndexScan` 之后
                // 计划里可能根本没有行源，链必须从环境走。
                chain: Some(chain),
                spill: None,
                writer: None,
            };
            let mut op = build(&node, &envx, &mut open)?;
            let mut cx = ExecContext::new(snapshot)
                .with_own(own)
                .with_params(&params_in);
            collect(op.as_mut(), &mut cx)
        })?;
        Ok(rows.into_iter().map(|r| r.values).collect())
    }

    /// **`UPDATE` / `DELETE` 的执行**（DML 的写路径；与 INSERT 同一套收尾）。
    ///
    /// **为什么先物化再写**（`dml_exec` 的 `materialize`）：边扫边改会让同一个
    /// 游标看见自己刚写的行——行迁移到尚未扫到的页就**可能被改第二次**。
    /// 物化（按语句快照把命中行收齐）把"读什么"与"写什么"分开，语义确定；
    /// 代价是命中行集驻留内存，V1 直说这条边界（`V1` 的表都不大）。
    fn run_dml(
        &mut self,
        plan: &PhysicalPlan,
        snapshot: CommitSeq,
        kind: DmlKind,
        filter: Option<bicdb_exec::Expr>,
    ) -> Result<QueryResult, SessionError> {
        let source = plan
            .sources
            .first()
            .ok_or_else(|| SessionError::State("DML 缺目标表".to_owned()))?;
        let node = plan.node.clone();
        let params_in = self.exec_params.clone();
        let seg_block = source.seg_block;
        let table_obj = source.table_obj;
        let own = self.txn.as_ref().map(TxnHandle::id);
        // **物化目标行**（按语句快照 + 本会话未提交改动；命中行 = WHERE 放行）。
        let view = bicdb_storage::cr::ReadView::new(snapshot).with_own(own);
        let mut indexes = crate::dml_index::table_indexes(self.catalog, snapshot, table_obj)?;
        let mut candidates = if self.graph_internal {
            crate::dml_index::graph_storage_candidates(
                self.catalog,
                &indexes,
                filter.as_ref(),
                &params_in,
            )?
        } else {
            None
        };
        if let Some(rids) = &mut candidates {
            // Stable tree entries may point at forwarding slots after growth.
            // Writers need the physical row ID, as the heap-scan fallback does.
            for rid in rids.iter_mut() {
                *rid = crate::dml_index::resolve_physical_rid(
                    self.pool,
                    self.catalog.file_mut(),
                    self.ws,
                    *rid,
                )?;
            }
            rids.sort_unstable();
            rids.dedup();
        }
        let (file_id, blocks) = if candidates.is_none() {
            let seg = Segment::open_pooled(self.pool, self.catalog.file_mut(), seg_block, self.ws)?;
            let fid = seg.file_id();
            let hwm = seg.hwm();
            (fid, seg.data_blocks(hwm))
        } else {
            (0, vec![])
        };
        let targets: Vec<(bicdb_storage::rowid::RowId, Vec<u8>)> =
            self.engine.with_read_context(|pool_ref, chain| {
                if let Some(rids) = &candidates {
                    let fetched = bicdb_storage::scan::fetch_rows(pool_ref, chain, view, rids)
                        .map_err(|e| SessionError::State(format!("图索引回表失败：{e}")))?;
                    return Ok(rids
                        .iter()
                        .copied()
                        .zip(fetched)
                        .filter_map(|(rid, bytes)| bytes.map(|bytes| (rid, bytes)))
                        .collect());
                }
                let mut scanner = HeapScanner::new(pool_ref, chain, view, file_id, blocks.clone());
                let mut out = Vec::new();
                loop {
                    match scanner.next_row() {
                        Ok(Some((rid, bytes))) => out.push((rid, bytes)),
                        Ok(None) => break,
                        // 扫描出错：**不吞**——包成会话错误返回（物化阶段没有写，
                        // 此时中止是干净的）。
                        Err(e) => {
                            return Err(SessionError::State(format!("DML 物化扫描失败：{e}")))
                        }
                    }
                }
                Ok::<_, SessionError>(out)
            })?;
        // WHERE 在物化之后、写之前判（谓词按**表行**坐标：无 ROWID 偏移）。
        let shape_of = |n: &bicdb_exec::PlanNode| match n {
            bicdb_exec::PlanNode::Update { shape, .. }
            | bicdb_exec::PlanNode::Delete { shape, .. } => Some(shape.clone()),
            _ => None,
        };
        let shape =
            shape_of(&node).ok_or_else(|| SessionError::State("DML 计划形态不符".to_owned()))?;
        let mut rows: Vec<(bicdb_storage::rowid::RowId, Vec<u8>)> =
            Vec::with_capacity(targets.len());
        {
            let cx_params = params_in.clone();
            let mut cx = ExecContext::new(snapshot)
                .with_own(own)
                .with_params(&cx_params);
            for (rid, bytes) in targets {
                if let Some(pred) = &filter {
                    let row = bicdb_exec::value::decode_row(&bytes, &shape)?;
                    match bicdb_exec::expr::eval(pred, &row, cx.params())? {
                        Value::Bool(true) => {}
                        _ => continue,
                    }
                }
                rows.push((rid, bytes));
            }
            let _ = &mut cx;
        }
        // 索引清单（写前一次）。
        let ws = self.ws;
        let has_indexes = !indexes.is_empty();
        let opts = self
            .catalog
            .table_options(snapshot, table_obj)
            .map_err(|e| SessionError::State(format!("读表选项：{e}")))?;
        // **唯一键被改** ⇒ 本版具名拒绝（唯一性预检是"写前看旧行"的形态，
        // UPDATE 的新键要逐一与"别处已有的键"比——那套随 S4 的后续切片）。
        if kind == DmlKind::Update && indexes.has_unique() {
            if let bicdb_exec::PlanNode::Update { sets, .. } = &node {
                for idx in &indexes.indexes {
                    if !idx.unique {
                        continue;
                    }
                    if sets.iter().any(|(col, _)| idx.cols.contains(col)) {
                        return Err(SessionError::State(format!(
                            "UPDATE 改唯一索引 `{}` 的键列：本版未实现（写前预检只覆盖 INSERT）——\
                             改用 `DELETE` + `INSERT`，或先 `DROP INDEX`",
                            idx.name
                        )));
                    }
                }
            }
        }
        let constraints: Vec<bicdb_exec::writer::ColumnConstraint> = self
            .catalog
            .columns(snapshot, table_obj)
            .map_err(|e| SessionError::State(format!("读列约束：{e}")))?
            .into_iter()
            .map(|c| bicdb_exec::writer::ColumnConstraint {
                name: c.name,
                nullable: c.nullable,
                max_bytes: (crate::plan::kind_of(c.type_code) == ColKind::Bytes)
                    .then_some(c.length as usize),
            })
            .collect();
        let own_txn = self.txn.is_none();
        let mut txn = match self.txn.take() {
            Some(t) => t,
            None => self.engine.begin()?,
        };
        let mark = self.engine.statement_mark(&txn)?;
        // 物化后喂给算子：**内存游标**（行 = (RID, 存储行字节)）。
        let cursor_rows = rows.clone();
        let outcome = self
            .engine
            .with_write_context(&mut txn, |pool, log, chain, t| {
                let mut writer = TableAccessWriter::with_txn(
                    pool,
                    chain,
                    log,
                    self.catalog.file_mut(),
                    seg_block,
                    ws,
                    t,
                );
                writer.set_table_options(opts.pctfree as u8, opts.itl_max as u16);
                writer.set_column_constraints(constraints);
                if has_indexes {
                    writer.set_indexes(&mut indexes);
                }
                let cell = std::cell::RefCell::new(&mut writer as &mut dyn TableWriter);
                let mut open = |_src: bicdb_exec::SourceId| {
                    Ok(Box::new(MemoryCursor::new(cursor_rows.clone()))
                        as Box<dyn bicdb_exec::RowCursor>)
                };
                let envx = ExecEnv {
                    pool,
                    // 写上下文里链是**可变借用**（在 `TableAccessWriter` 手里）；
                    // 本路径的源只有 `SeqScan`（`INSERT … SELECT` 的来源在
                    // `materialize` 里用读上下文跑），故不给链。
                    chain: None,
                    spill: None,
                    writer: Some(&cell),
                };
                let mut op = build(&node, &envx, &mut open)?;
                let mut cx = ExecContext::new(snapshot)
                    .with_own(own)
                    .with_params(&params_in);
                collect(op.as_mut(), &mut cx)?;
                let what = match kind {
                    DmlKind::Update => "Update",
                    DmlKind::Delete => "Delete",
                };
                Ok::<u64, bicdb_exec::ExecError>(cx.rows_affected_of(what))
            });
        match outcome {
            Ok(affected) => {
                if own_txn {
                    let committed = self.engine.commit(&mut txn)?;
                    self.seq = committed.as_raw();
                } else {
                    self.txn = Some(txn);
                }
                Ok(QueryResult::Affected(affected))
            }
            Err(e) => {
                let rolled = if own_txn {
                    self.engine.rollback(&mut txn).map(|_| ())
                } else {
                    let r = self.engine.rollback_statement(&mut txn, mark).map(|_| ());
                    self.txn = Some(txn);
                    r
                };
                match rolled {
                    Ok(()) => Err(SessionError::Exec(e)),
                    Err(rb) => Err(SessionError::Exec(bicdb_exec::ExecError::RollbackFailed {
                        main: e.to_string(),
                        rollback: rb.to_string(),
                    })),
                }
            }
        }
    }

    /// **INSERT … VALUES**（写路径）。
    ///
    /// **事务归属**：会话层持事务（写侧 `owns_txn = false`）——
    /// 自动提交形态由本方法收尾，显式事务里留给 `COMMIT`。
    /// **索引维护**：语句开始时从目录取一次清单（活索引 + 键列 + 段头块），
    /// 装进写侧的口（`catalog` 只在写前读，写中不再回查字典）。
    fn run_insert(
        &mut self,
        plan: &PhysicalPlan,
        snapshot: CommitSeq,
    ) -> Result<QueryResult, SessionError> {
        let source = plan
            .sources
            .first()
            .ok_or_else(|| SessionError::State("INSERT 缺目标表".to_owned()))?;
        let mut node = plan.node.clone();
        let params_in = self.exec_params.clone();
        let seg_block = source.seg_block;
        let table_obj = source.table_obj;
        // **`INSERT … SELECT`**：先跑一趟来源（物化），把结果行变成字面量再插入——
        // 复用唯一性预检与索引维护的整条路（来源是只读的，先跑不影响正确性）。
        if let Some(sub) = &plan.insert_source {
            let rows = self.materialize(&sub.node, plan, snapshot)?;
            node = insert_node_with_rows(&node, &rows)?;
        }
        // 索引清单（写前一次；空清单 = 不装口，写侧零开销）。
        let ws = self.ws;
        let mut indexes = crate::dml_index::table_indexes(self.catalog, snapshot, table_obj)?;
        // **表选项**（`tab$`）：`pctfree` 管页内预留、`itl_max` 管 ITL 扩展上限——
        // 两者此前只在字典/段头里躺着，写路径恒用缺省值（本次接线修掉）。
        let opts = self
            .catalog
            .table_options(snapshot, table_obj)
            .map_err(|e| SessionError::State(format!("读表选项：{e}")))?;
        // **唯一性预检**（写前；同键活行 ⇒ 冲突，语句整体不写）。
        if indexes.has_unique() {
            // **用物化后的节点**（`INSERT … SELECT` 的行在 `node` 里，不在 `plan.node` 里
            // ——读错一个变量，唯一性预检就会看到"零行"而放行重复键。实测抓到的正是这条）。
            let rows = plan_row_bytes(&node, &self.exec_params)?;
            let mut seen = crate::dml_index::SeenKeys::new();
            let view = bicdb_storage::cr::ReadView::new(snapshot)
                .with_own(self.txn.as_ref().map(TxnHandle::id));
            let checked = self.engine.with_read_context(|pool, chain| {
                crate::dml_index::check_unique(
                    self.catalog,
                    pool,
                    chain,
                    view,
                    &indexes,
                    &rows,
                    &mut seen,
                )
            });
            checked?;
        }
        let has_indexes = !indexes.is_empty();
        let constraints: Vec<bicdb_exec::writer::ColumnConstraint> = self
            .catalog
            .columns(snapshot, table_obj)
            .map_err(|e| SessionError::State(format!("读列约束：{e}")))?
            .into_iter()
            .map(|c| bicdb_exec::writer::ColumnConstraint {
                name: c.name,
                nullable: c.nullable,
                max_bytes: (crate::plan::kind_of(c.type_code) == ColKind::Bytes)
                    .then_some(c.length as usize),
            })
            .collect();
        let own_txn = self.txn.is_none();
        // 视角与读路径同一份（语句里若有求值读，也照"读己所写"）。
        let own = self.txn.as_ref().map(TxnHandle::id);
        let mut txn = match self.txn.take() {
            Some(t) => t,
            None => self.engine.begin()?,
        };
        // **语句回滚点**（出错时按形态用）：显式事务里语句失败只回滚**本语句**
        // （Oracle 口径；`rollback_statement` 不释锁、不动此前语句）——整事务
        // 回滚会把用户前面已成功的语句一起毁掉，还把手里的锁全丢了。
        // 无改动时它是幂等空操作，取一次的成本可以忽略。
        let mark = self.engine.statement_mark(&txn)?;
        let outcome = self
            .engine
            .with_write_context(&mut txn, |pool, log, chain, t| {
                let mut writer = TableAccessWriter::with_txn(
                    pool,
                    chain,
                    log,
                    self.catalog.file_mut(),
                    seg_block,
                    ws,
                    t,
                );
                writer.set_table_options(opts.pctfree as u8, opts.itl_max as u16);
                writer.set_column_constraints(constraints);
                if has_indexes {
                    writer.set_indexes(&mut indexes);
                }
                let cell = std::cell::RefCell::new(&mut writer as &mut dyn TableWriter);
                let mut open =
                    |_src: bicdb_exec::SourceId| -> Result<Box<dyn RowCursor>, bicdb_exec::ExecError> {
                        unreachable!("INSERT … VALUES 不走行源")
                    };
                let envx = ExecEnv {
                    pool,
                    // 写上下文里链是**可变借用**（在 `TableAccessWriter` 手里）；
                    // 本路径的源只有 `SeqScan`（`INSERT … SELECT` 的来源在
                    // `materialize` 里用读上下文跑），故不给链。
                    chain: None,
                    spill: None,
                    writer: Some(&cell),
                };
                let mut op = build(&node, &envx, &mut open)?;
                let mut cx = ExecContext::new(snapshot)
                .with_own(own)
                .with_params(&params_in);
                collect(op.as_mut(), &mut cx)?;
                Ok::<u64, bicdb_exec::ExecError>(cx.rows_affected_of("Insert"))
            });
        match outcome {
            Ok(affected) => {
                if own_txn {
                    let committed = self.engine.commit(&mut txn)?;
                    self.seq = committed.as_raw();
                } else {
                    // 显式事务：语句不提交（写侧 owns_txn = false 已挡住算子提交）。
                    self.txn = Some(txn);
                }
                Ok(QueryResult::Affected(affected))
            }
            Err(e) => {
                // **回滚失败不吞**（与 DDL/DML 同一口径）：行锁/undo 可能没清，
                // 后续语句会撞上——与主错一并报出。
                //
                // 两种形态：
                // - 自动提交（`own_txn`）：整回滚——这个事务只含本语句；
                // - **显式事务：语句级回滚**，事务与锁留着（`COMMIT` 照旧可用，
                //   此前已成功的语句照旧生效）。整事务回滚是错的（会连坐）。
                let rolled = if own_txn {
                    self.engine.rollback(&mut txn).map(|_| ())
                } else {
                    let r = self.engine.rollback_statement(&mut txn, mark).map(|_| ());
                    self.txn = Some(txn); // 事务放回：锁与前面语句的工作都还在
                    r
                };
                match rolled {
                    Ok(()) => Err(SessionError::Exec(e)),
                    Err(rb) => Err(SessionError::Exec(bicdb_exec::ExecError::RollbackFailed {
                        main: e.to_string(),
                        rollback: rb.to_string(),
                    })),
                }
            }
        }
    }
}

/// DML 种类（`run_dml` 共用一套收尾）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DmlKind {
    /// `UPDATE`。
    Update,
    /// `DELETE`。
    Delete,
}

/// **内存行游标**（物化后的 DML 源；`rewind` 也支持——算子重开不致命）。
#[derive(Clone)]
struct MemoryCursor {
    rows: Vec<(bicdb_storage::rowid::RowId, Vec<u8>)>,
    at: usize,
}

impl MemoryCursor {
    fn new(rows: Vec<(bicdb_storage::rowid::RowId, Vec<u8>)>) -> Self {
        Self { rows, at: 0 }
    }
}

impl bicdb_exec::RowCursor for MemoryCursor {
    fn next_row(
        &mut self,
    ) -> Result<Option<(bicdb_storage::rowid::RowId, Vec<u8>)>, bicdb_exec::ExecError> {
        if self.at >= self.rows.len() {
            return Ok(None);
        }
        let r = self.rows[self.at].clone();
        self.at += 1;
        Ok(Some(r))
    }

    fn rewind(&mut self) -> Result<(), bicdb_exec::ExecError> {
        self.at = 0;
        Ok(())
    }
}

/// **固定表 → 内存游标**（行由引擎即时产生：`file$` 的内容 = 控制文件内存映像）。
///
/// **形态**：字典值 → `bicdb-exec::Value` → 按行形状**编码成行字节**（与堆行同一
/// 编码，下游算子（Filter/Project/Sort/聚合）不必知道来源不同）。**没有 RID**
/// （固定表没有段/槽位）——给一个全零 ROWID（`WithRowId` 只用于 DML，不会碰到它）。
fn fixed_cursor(
    source: Option<&dyn FixedTableSource>,
    name: &str,
    shape: &RowShape,
) -> Result<MemoryCursor, bicdb_exec::ExecError> {
    // 没有内容源 ⇒ **具名拒绝**（不静默给空集）：控制文件在哪是装配层的事。
    let src = source.ok_or(bicdb_exec::ExecError::NoSuchSource { id: 0 })?;
    let table = src.fixed_table(name).ok_or_else(|| {
        bicdb_exec::ExecError::BadStoredRow(format!("固定表 `{name}` 没有内容源"))
    })?;
    dictionary_cursor(&table.rows, shape)
}

fn dictionary_cursor(
    values: &[Vec<bicdb_catalog::row::DictValue>],
    shape: &RowShape,
) -> Result<MemoryCursor, bicdb_exec::ExecError> {
    let rows: Vec<Vec<Value>> = values
        .iter()
        .map(|row| row.iter().map(dict_to_value).collect())
        .collect();
    values_cursor(&rows, shape)
}

fn values_cursor(
    values: &[Vec<Value>],
    shape: &RowShape,
) -> Result<MemoryCursor, bicdb_exec::ExecError> {
    let zero = bicdb_storage::rowid::RowId::from_bytes(&[0u8; 6]);
    let mut rows = Vec::with_capacity(values.len());
    for row in values {
        let bytes = bicdb_exec::encode_row(&Row::new(row.clone()), shape)?;
        rows.push((zero, bytes));
    }
    Ok(MemoryCursor::new(rows))
}

/// 字典值 → 执行器值（固定表的行来自目录层）。
fn dict_to_value(v: &bicdb_catalog::row::DictValue) -> Value {
    use bicdb_catalog::row::DictValue as D;
    match v {
        D::Null => Value::Null,
        D::Num(n) => Value::Number(
            bicdb_types::Number::parse(&n.to_string())
                .unwrap_or_else(|_| bicdb_types::Number::parse("0").expect("0 是合法 NUMBER")),
        ),
        D::Text(t) => Value::Bytes(t.as_bytes().to_vec()),
        D::Bytes(b) => Value::Bytes(b.clone()),
        D::Bool(b) => Value::Bool(*b),
    }
}

/// **把 `Insert` 计划节点的行换成字面量**（`INSERT … SELECT` 物化之后用）。
///
/// 物化出来的每一行按**目标列序**（`target_cols`）展开成列宽的行——与
/// `INSERT … VALUES` 的计划形态完全一致，因此后面的唯一性预检、写侧、索引维护
/// 一行都不用改。
fn insert_node_with_rows(
    node: &bicdb_exec::PlanNode,
    rows: &[Vec<Value>],
) -> Result<bicdb_exec::PlanNode, SessionError> {
    let bicdb_exec::PlanNode::Insert { shape, .. } = node else {
        return Err(SessionError::State(
            "INSERT 的计划节点不是 Insert".to_owned(),
        ));
    };
    let mut literal_rows = Vec::with_capacity(rows.len());
    for r in rows {
        if r.len() != shape.len() {
            return Err(SessionError::State(format!(
                "INSERT … SELECT 的行有 {} 列、目标表 {} 列",
                r.len(),
                shape.len()
            )));
        }
        literal_rows.push(
            r.iter()
                .map(|v| bicdb_exec::Expr::Literal(v.clone()))
                .collect::<Vec<_>>(),
        );
    }
    Ok(bicdb_exec::PlanNode::Insert {
        shape: shape.clone(),
        rows: literal_rows,
    })
}

/// **计划里的行字面量 → 行字节**（唯一性预检用；与写侧的编码同一份）。
///
/// `INSERT … VALUES` 的行在计划里恒为**字面量**（绑定期的形态）——非字面量
/// 走到这里即计划形状不符（明确报错，不静默跳过预检）。
fn plan_row_bytes(
    node: &bicdb_exec::PlanNode,
    params: &[Value],
) -> Result<Vec<Vec<u8>>, SessionError> {
    let bicdb_exec::PlanNode::Insert { shape, rows } = node else {
        return Err(SessionError::State(
            "INSERT 的计划节点不是 Insert".to_owned(),
        ));
    };
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let mut values = Vec::with_capacity(row.len());
        for e in row {
            match e {
                bicdb_exec::Expr::Literal(v) => values.push(v.clone()),
                // **参数**：按已摆好的位取值（与执行期同一份序列）。
                bicdb_exec::Expr::Param(i) => values.push(
                    params
                        .get(*i)
                        .cloned()
                        .ok_or_else(|| SessionError::Params(format!("参数位 {i} 无值")))?,
                ),
                other => {
                    return Err(SessionError::State(format!(
                        "唯一性预检只认字面量与参数，行里出现 {other:?}"
                    )))
                }
            }
        }
        out.push(bicdb_exec::encode_row(&Row::new(values), shape).map_err(SessionError::Exec)?);
    }
    Ok(out)
}

/// **参数摆位**：绑定期的清单（名 + 形态 + 出现序）× 调用方按名给的值
/// ⇒ 执行期参数序列（位置 = 出现序）。
///
/// 三条判定都是**具名**的：缺值 / 多给（语句里没这个参数）/ 形态不符。
/// `NULL` 对任何形态都放行（它本来就没有类型）。
fn place_params<'n>(
    bound: &crate::bind::BoundStatement,
    named: &'n [(&'n str, Value)],
    used: &mut std::collections::HashSet<&'n str>,
) -> Result<Vec<Value>, SessionError> {
    use crate::bind::BoundStatement as B;
    let declared: Vec<(&str, bicdb_exec::ColKind)> = match bound {
        B::Select(s) => s.params.list(),
        B::Insert(i) => i.params.list(),
        B::Update(u) => u.params.list(),
        B::Delete(d) => d.params.list(),
        B::SetOp(o) => return place_params(&o.left, named, used),
        _ => Vec::new(),
    };
    for (name, _) in named {
        if declared.iter().any(|(d, _)| *d == *name) {
            used.insert(name);
        }
    }
    let mut out = Vec::with_capacity(declared.len());
    for (name, kind) in &declared {
        let (_, v) = named
            .iter()
            .find(|(n, _)| n == name)
            .ok_or_else(|| SessionError::Params(format!("缺参数值 `:{name}`")))?;
        if !matches!(v, Value::Null) {
            let ok = matches!(
                (kind, v),
                (bicdb_exec::ColKind::Number, Value::Number(_))
                    | (bicdb_exec::ColKind::Bytes, Value::Bytes(_))
                    | (bicdb_exec::ColKind::Bool, Value::Bool(_))
            );
            if !ok {
                return Err(SessionError::Params(format!(
                    "参数 `:{name}` 应是{}，给的是{}",
                    crate::bind::kind_name(*kind),
                    value_kind_name(v)
                )));
            }
        }
        out.push(v.clone());
    }
    Ok(out)
}

/// 值的形态名（诊断）。
fn value_kind_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "NULL",
        Value::Number(_) => "数值",
        Value::Bytes(_) => "字节串",
        Value::Bool(_) => "布尔",
        Value::GraphElement(_) => "图元素",
    }
}

/// **会话收尾**：显式事务还没收尾就丢会话 ⇒ 回滚（不留输家给下次恢复）。
impl Drop for Session<'_, '_, '_, '_> {
    fn drop(&mut self) {
        if let Some(mut txn) = self.txn.take() {
            let _ = self.engine.rollback(&mut txn);
        }
    }
}

/// 值 → 显示串。
#[must_use]
pub fn format_value(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_owned(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Bytes(b) => String::from_utf8(b.clone()).unwrap_or_else(|_| format!("0x{}", hex(b))),
        Value::GraphElement(v) => format!(
            "{}:{}:{}",
            v.graph,
            match v.kind {
                bicdb_exec::GraphElementKind::Node => 'n',
                bicdb_exec::GraphElementKind::Edge => 'e',
            },
            v.id
        ),
    }
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}
