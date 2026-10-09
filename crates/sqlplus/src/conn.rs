//! **连接面**：直连（进程内打开实例）或经服务（控制套接字）。
//!
//! ```text
//! 实例没被别人占 ⇒ 直连：本进程打开实例（与 `bicdb shell` 同一条路径）
//! 服务在跑       ⇒ 经套接字：连接 = 会话（事务跨语句保持）
//! 别的进程直连中 ⇒ 拒绝（单写者纪律；不许两个写者各写各的池）
//! ```
//!
//! **两条路的语义差异（记档）**：
//! - 直连：`Session` 在客户端进程里，语句直接落到实例；显式事务的句柄由
//!   **连接**保管（`Instance::txn`），语句执行时借给会话——两条路的事务
//!   语义因此一致（`BEGIN … COMMIT` 跨语句成立）；
//! - 经服务：语句送到守护进程执行，结果按 `bicdb-net` 的本机协议回来；
//!   **事务语义由服务端"一个连接一个会话"保证**——`BEGIN … COMMIT` 跨语句成立。
//!
//! **`DESCRIBE` 只在直连可用**（要走目录的列定义）——经服务时先用直连打开
//! 会被锁挡住，所以经服务形态下 `DESC` 明确报"需直连"（不静默给空）。

use std::path::{Path, PathBuf};

use bicdb_catalog::dict::{namespace, ColTypeCode};
use bicdb_cli::boot::{self, Instance};
use bicdb_cli::lock::LockMode;
use bicdb_cli::proto;
use bicdb_cli::service::{self, ServiceState};
use bicdb_common::seq::CommitSeq;
use bicdb_net::Client;
use bicdb_sql::session::{QueryResult, Session};

/// 连接错误。
#[derive(Debug)]
pub enum ConnError {
    /// 打开/建区层。
    Boot(String),
    /// 套接字/协议层。
    Wire(String),
    /// 会话层（语句）。
    Sql(String),
    /// 状态（别人占着 / DESC 需直连 …）。
    State(String),
}

impl std::fmt::Display for ConnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnError::Boot(w) | ConnError::Wire(w) | ConnError::Sql(w) | ConnError::State(w) => {
                f.write_str(w)
            }
        }
    }
}

impl std::error::Error for ConnError {}

/// 一列的形态（`DESCRIBE` 用）。
#[derive(Debug, Clone)]
pub struct ColumnInfo {
    /// 列名。
    pub name: String,
    /// 是否可空。
    pub nullable: bool,
    /// 类型名（SQL 形态：`NUMBER` / `VARCHAR2(32)` …）。
    pub type_name: String,
}

/// **连接**：直连或经服务。
pub enum Conn {
    /// 直连（本进程打开实例）。
    Local(Box<Instance>),
    /// 经服务（持久连接 = 服务端一个会话）。
    Remote {
        /// 持久连接（事务跨语句保持）。
        client: Client,
        /// 套接字路径（诊断）。
        socket: PathBuf,
        /// 实例目录（诊断）。
        dir: PathBuf,
    },
}

impl Conn {
    /// **按实例状态选路**：服务在跑 ⇒ 经服务；否则直连（被占用则报错）。
    pub fn open(
        params: &bicdb_cli::config::InstanceParams,
        force_direct: bool,
    ) -> Result<Self, ConnError> {
        Self::open_with(params, force_direct, None)
    }

    /// 与 [`Conn::open`] 同，但可**显式给控制套接字**（`bicdbcli -s <套接字>`：
    /// 绕过参数文件里的 `[service] socket`，用于运维/多实例排障）。
    pub fn open_with(
        params: &bicdb_cli::config::InstanceParams,
        force_direct: bool,
        socket: Option<&Path>,
    ) -> Result<Self, ConnError> {
        let run = &params.run;
        let dir = params.db_root.clone();
        let dir = dir.as_path();
        if let Some(sock) = socket {
            // 显式套接字 ⇒ 就是"经服务"（直连形态由 `--direct` 选，二者并用报错）。
            if force_direct {
                return Err(ConnError::State(
                    "`-s <套接字>`（经服务）与 `--direct`（直连）不能一起给".to_owned(),
                ));
            }
            let client = Self::connect_configured(sock, run)?;
            return Ok(Conn::Remote {
                client,
                socket: sock.to_path_buf(),
                dir: dir.to_path_buf(),
            });
        }
        if !force_direct {
            match service::state_of(dir) {
                ServiceState::Serving(info) => {
                    let client = Self::connect_configured(&info.socket, run)?;
                    return Ok(Conn::Remote {
                        client,
                        socket: info.socket.clone(),
                        dir: dir.to_path_buf(),
                    });
                }
                ServiceState::Direct(info) => {
                    return Err(ConnError::State(format!(
                        "实例被 pid {} 直连着（不是服务）——等它退出或改用那个会话",
                        info.pid
                    )));
                }
                ServiceState::NotRunning | ServiceState::Stale(_) => {}
            }
        }
        let inst = boot::open_instance(params).map_err(|e| ConnError::Boot(e.to_string()))?;
        Ok(Conn::Local(Box::new(inst)))
    }

    /// **按 `[client]` 参数连接**（握手超时 / 请求超时；0 = 请求不限时）。
    fn connect_configured(
        socket: &Path,
        run: &bicdb_cli::config::RunParams,
    ) -> Result<Client, ConnError> {
        let mut client = Client::connect_with_timeout(
            socket,
            std::time::Duration::from_millis(run.handshake_timeout_ms),
        )
        .map_err(|e| ConnError::Wire(format!("连服务 {}：{e}", socket.display())))?;
        if run.request_timeout_ms > 0 {
            client
                .set_timeout(std::time::Duration::from_millis(run.request_timeout_ms))
                .map_err(|e| ConnError::Wire(e.to_string()))?;
        }
        Ok(client)
    }

    /// **认证**（`-U <主体>`）：经服务的连接上做一次 `AUTH`（连上之后、语句之前）。
    ///
    /// 直连形态**没有这一步**（本机 = OS 身份）——给了 `-U` 就具名拒绝：
    /// 悄悄忽略会让人以为"以 `alice` 的身份"跑了语句。
    ///
    /// # Errors
    /// 服务端拒绝（主体名或口令不对 / 已暂停 / 非 PUBLIC / 已认证）或直连形态。
    pub fn authenticate(
        &mut self,
        c: &bicdb_cli::clientauth::Credentials,
    ) -> Result<bicdb_net::AuthOk, ConnError> {
        match self {
            Conn::Remote { client, .. } => client
                .auth(&c.user, &c.password)
                // 服务端的具名文案**原样透传**（错误文本是契约的一部分）。
                .map_err(|e| ConnError::Wire(e.to_string())),
            Conn::Local(_) => Err(ConnError::State(format!(
                "`-U {}` 只对**经服务**的连接有效——现在是直连形态：\
                 本机访问按控制套接字的文件权限（OS 身份）判定",
                c.user
            ))),
        }
    }

    /// 连接形态的展示名。
    #[must_use]
    pub fn kind(&self) -> String {
        match self {
            Conn::Local(inst) => format!(
                "直连 {}（pid {}）",
                inst.dir.display(),
                inst.lock_holder().map_or(0, |l| l.pid)
            ),
            Conn::Remote { socket, .. } => format!("服务 {}", socket.display()),
        }
    }

    /// 实例目录。
    #[must_use]
    pub fn dir(&self) -> PathBuf {
        match self {
            Conn::Local(inst) => inst.dir.clone(),
            Conn::Remote { dir, .. } => dir.clone(),
        }
    }

    /// 当前提交序号（横幅/诊断）。
    #[must_use]
    pub fn seq(&self) -> u64 {
        match self {
            Conn::Local(inst) => inst.seq(),
            Conn::Remote { .. } => 0, // 经服务：按需查 STATUS（横幅不阻塞）
        }
    }

    /// **执行一段 SQL**（可含多条语句）。
    pub fn execute(&mut self, sql: &str) -> Result<Vec<QueryResult>, ConnError> {
        match self {
            Conn::Local(inst) => {
                let seq = inst.seq();
                let mut session = Session::new(inst.pool, inst.engine, &mut inst.catalog, seq);
                session
                    .set_fulltext_defaults(
                        inst.params.run.fulltext_interval_ms,
                        inst.params.run.fulltext_batch_rows,
                    )
                    .map_err(|e| ConnError::Sql(e.to_string()))?;
                // **接上管理面**（DCL 的落点：注册表在 `<BICDB_HOME>/control/`）。
                session.set_dcl_context(
                    bicdb_cli::home::Home::locate().ok().map(|h| h.root),
                    Some(inst.io),
                );
                session.set_workspace_provisioner(Some(
                    bicdb_cli::provision::CliProvisioner::new_static(),
                ));
                session.set_pbkdf2_iterations(inst.params.run.pbkdf2_iterations);
                // **固定表的内容源**（`file$` ← 控制文件的内存映像）。
                session.set_fixed_table_source(Some(bicdb_cli::fixed::CliFixedTables::new_static(
                    &inst.dir, inst.io,
                )));
                // **事务跨语句**：直连形态每条语句新建一个会话（会话借住实例，
                // 活不过一次调用），所以显式事务的句柄由**连接**保管——
                // 借给这句，执行完交回。不这样做的话 `BEGIN` 会在语句收尾的
                // `Drop` 里被回滚、`COMMIT` 只回一句"没有活动事务"（静默降级）。
                let _ = session.adopt_txn(inst.txn.take());
                let out = session.execute(sql);
                inst.txn = session.release_txn();
                out.map_err(|e| ConnError::Sql(e.to_string()))
            }
            Conn::Remote { client, .. } => {
                // 参数随请求走（这里没有参数，序列为空）；结果按 `bicdb-net`
                // 的语句形状回来，再译回呈现层认的形状（两条路共用一份打印）。
                let statements = client
                    .sql(sql, &[])
                    .map_err(|e| ConnError::Sql(e.to_string()))?;
                Ok(proto::results(&statements))
            }
        }
    }

    /// **`DESCRIBE`**：列名/可空/类型（直连才可用——要走目录）。
    pub fn describe(&mut self, name: &str) -> Result<Vec<ColumnInfo>, ConnError> {
        if let Conn::Remote { client, .. } = self {
            // 经服务：服务端只读目录回列清单（同一份 `type_name` 口径）。
            let cols = client
                .describe(name)
                .map_err(|e| ConnError::State(e.to_string()))?;
            return Ok(cols
                .into_iter()
                .map(|c| ColumnInfo {
                    name: c.name,
                    nullable: c.nullable,
                    type_name: c.type_name,
                })
                .collect());
        }
        let Conn::Local(inst) = self else {
            unreachable!("上面已处理 Remote");
        };
        let snapshot = CommitSeq::from_raw(inst.seq().max(1)).expect("48 位域内");
        let obj = inst
            .catalog
            .resolve(snapshot, namespace::TABLE, name)
            .map_err(|e| ConnError::State(e.to_string()))?;
        let cols = inst
            .catalog
            .columns(snapshot, obj.obj)
            .map_err(|e| ConnError::State(e.to_string()))?;
        Ok(cols
            .into_iter()
            .map(|c| ColumnInfo {
                name: c.name,
                nullable: c.nullable,
                type_name: type_name(c.type_code, c.length),
            })
            .collect())
    }

    /// 关闭连接（直连 ⇒ 完全检查点；经服务 ⇒ 断开即可，服务继续）。
    pub fn close(&mut self) -> Result<(), ConnError> {
        match self {
            Conn::Local(inst) => inst.shutdown().map_err(|e| ConnError::Boot(e.to_string())),
            Conn::Remote { .. } => Ok(()),
        }
    }

    /// 本连接的锁模式（诊断；服务模式由守护进程持有）。
    #[must_use]
    pub fn lock_mode(&self) -> Option<LockMode> {
        match self {
            Conn::Local(inst) => inst.lock_holder().map(|l| l.mode),
            Conn::Remote { .. } => Some(LockMode::Service),
        }
    }
}

/// **类型码 → SQL 类型名**（`DESCRIBE` 的输出形态；沿 Oracle 的写法）。
#[must_use]
pub fn type_name(code: u32, length: u32) -> String {
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
