//! **DCL 执行层**（`doc/DCL语句设计_v0.1.md` v0.2 §3 的"执行落点"）。
//!
//! ```text
//! 解析（D1 已落地）→ 绑定（只搬运）→ **本模块**（语义 + 两处写）
//!                                         ├── 属性：public 的字典行（crate::dcl / catalog::dcl）
//!                                         └── 位置：实例注册表（storage::globalctl）
//! ```
//!
//! # 三条执行纪律
//!
//! 1. **只在 `public` 上执行**（管理面：`user$`/`ws$`/`fs$`/`wq$` 在那里）；
//!    **资格检查先于对象查找**（REQ-SQL-005）——先判"是不是管理面会话"，
//!    再看对象存不存在；
//! 2. **一个动作 = 一个 DDL 事务**（活动事务中发 DCL ⇒ 拒绝，与 DDL 同规）；
//! 3. **写序固定：属性先、可见性最后**（`doc/全局控制文件设计_v0.1.md` §2）——
//!    控制文件里有没有它，才是"它存在"的判据。
//!
//! # 现在落地了哪几组
//!
//! | 组 | 语句 | 状态 |
//! | --- | --- | --- |
//! | **F**（文件系统池） | `CREATE/ALTER/DROP FILESYSTEM` | ✅ 本切片 |
//! | W / U / T | 工作区 / 用户 / 模板 | ⏳ 后续切片（**具名拒绝**，点明缺什么） |

use std::path::{Path, PathBuf};

use bicdb_catalog::dcl::{self as catdcl, fs_status, ws_status, FsEntry, WsEntry};
use bicdb_storage::globalctl::GlobalControlFile;
use bicdb_workspace::id::WorkspaceId;
use bicdb_workspace::io::FileIo;

use crate::ast::{
    AlterFilesystemStmt, CreateFilesystemStmt, DropFilesystemStmt, FsQuota, FsRef, QuotaAmount,
    Stmt, WorkRef,
};
use crate::session::{QueryResult, Session, SessionError};

/// **工作区文件面的供给方**（`CREATE/DROP WORKSPACE` 用）。
///
/// **为什么是一个端口**：建一个工作区要 ①建文件（file 0/1 + 控制文件 + 日志组 +
/// 字典种子）②在 `public.ws$` 登记 ③写实例注册表——第 ① 步的装配在**实例层**
/// （`bicdb-cli` 的 `boot`，它才拿得到参数文件与日志布局），而 DCL 执行在 `bicdb-sql`。
/// 端口由 CLI 实现（同 `CatalogView` 的既有范式：sql 定义口、cli 给真件）。
pub trait WorkspaceProvisioner {
    /// **建一个工作区的文件面**（幂等：目标已存在即报错）。
    ///
    /// 成功返回后：该目录下应能独立 `open` 起一个实例（字典 + 撤销段 + 控制文件 +
    /// 日志组齐备，`stat$`/`seq$` 已建）。
    fn provision(&self, req: &ProvisionRequest) -> Result<(), String>;

    /// Publish an immutable empty-schema template from an idle workspace.
    fn add_schema_template(&self, _home: &Path, _source: &Path, _name: &str) -> Result<(), String> {
        Err("初始化模板供给方未实现".to_owned())
    }
    /// Publish an explicit graph-data snapshot from an idle unowned workspace.
    fn add_graph_template(&self, _home: &Path, _source: &Path, _name: &str) -> Result<(), String> {
        Err("带数据图模板供给方未实现".to_owned())
    }

    /// Remove a template; existing copies have no dependency on it.
    fn drop_schema_template(&self, _home: &Path, _name: &str) -> Result<(), String> {
        Err("初始化模板供给方未实现".to_owned())
    }

    /// **删之前的预检**（能不能删）。
    ///
    /// **要在动可见性之前调**：可预见的失败（比如目录被活实例占着）不该把
    /// 工作区切成"不可见但目录还在"的半死状态——先问清楚，再走三步。
    fn check_deprovisionable(&self, root: &Path) -> Result<(), String>;

    /// **删一个工作区的文件面**（整棵目录）。
    ///
    /// 调用方**已经先断了可见性**（注册表 + `ws$` 墓碑）——所以这里只管文件。
    /// 目录被活实例占着（`bicdb.pid` 活着）⇒ 拒绝。
    fn deprovision(&self, root: &Path) -> Result<(), String>;
}

/// Live daemon state transition used by `ALTER WORKSPACE ... OPEN`.
/// Implementations must acknowledge an effective runtime change; updating a
/// catalog flag alone is not a successful open.
pub trait WorkspaceStateController {
    /// Apply one access mode to a registered workspace.
    fn open_workspace(
        &self,
        workspace_id: u64,
        name: &str,
        root: &Path,
        mode: crate::ast::WorkspaceOpenMode,
    ) -> Result<String, String>;

    /// Verify one isolated recovery scope against durable media, append the
    /// durable verification record, and only then release that exact scope.
    fn verify_recovery(
        &self,
        workspace_id: u64,
        name: &str,
        root: &Path,
        scope: crate::ast::RecoveryVerifyScope,
    ) -> Result<String, String>;
}

/// 建工作区的文件面需要的事实（**不含参数**——参数由供给方按实例口径给）。
#[derive(Debug, Clone)]
pub struct ProvisionRequest {
    /// 工作区根目录（注册表里要记的就是它）。
    pub root: PathBuf,
    /// 工作区号（48 位；注册表分配）。
    pub workspace_id: u64,
    /// 工作区名（日志文件名与诊断用）。
    pub name: String,
    /// 参数文件种子（通常 = 实例的 `<home>/public/bicdb.ini`）。
    pub seed_ini: PathBuf,
    /// 日志落点（`<home>/log/<name>.log`——运维只记一个地方）。
    pub log_path: PathBuf,
    /// Schema or explicit graph-data template, applied before workspace registration.
    pub from_template: Option<String>,
}

/// **管理面的实例侧上下文**：`BICDB_HOME` + 一条 I/O + 工作区供给方。
#[derive(Clone)]
pub struct DclContext<'a> {
    /// 实例根（`<BICDB_HOME>`）——注册表在 `<root>/control/`。
    pub home_root: PathBuf,
    /// 文件 I/O（全局控制文件走它）。
    pub io: &'a dyn FileIo,
    /// 工作区文件面供给方（`None` ⇒ `CREATE/DROP WORKSPACE` 具名拒绝）。
    pub provisioner: Option<&'a dyn WorkspaceProvisioner>,
    /// Live instance controller (`None` in offline/direct sessions).
    pub controller: Option<&'a dyn WorkspaceStateController>,
    /// 口令散列迭代数（写进存储串；来自会话参数）。
    pub pbkdf2_iterations: u32,
}

impl std::fmt::Debug for DclContext<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DclContext")
            .field("home_root", &self.home_root)
            .finish_non_exhaustive()
    }
}

impl<'a> DclContext<'a> {
    /// 建一个（不带供给方的）上下文（测试/只读场景）。
    #[must_use]
    pub fn new(home_root: PathBuf, io: &'a dyn FileIo) -> Self {
        Self {
            home_root,
            io,
            provisioner: None,
            controller: None,
            pbkdf2_iterations: bicdb_common::pbkdf2::DEFAULT_ITERATIONS,
        }
    }

    /// 带上供给方。
    #[must_use]
    pub fn with_provisioner(mut self, p: &'a dyn WorkspaceProvisioner) -> Self {
        self.provisioner = Some(p);
        self
    }

    /// Attach the live instance controller.
    #[must_use]
    pub fn with_controller(mut self, controller: &'a dyn WorkspaceStateController) -> Self {
        self.controller = Some(controller);
        self
    }

    /// 注册表双副本路径（`<home>/control/control01.ctl`、`control02.ctl`）。
    #[must_use]
    pub fn global_ctl_paths(&self) -> [PathBuf; 2] {
        let dir = self.home_root.join(bicdb_cli_home_control_dir());
        [dir.join("control01.ctl"), dir.join("control02.ctl")]
    }
}

/// 控制目录名（与 `bicdb-cli` 的 `home::CONTROL_DIR` 同值；sql 不依赖 cli，
/// 故在这里落一份常量并**用测试钉住两者一致**）。
const fn bicdb_cli_home_control_dir() -> &'static str {
    "control"
}

/// DCL 执行错误（会话层友好文案）。
#[derive(Debug)]
pub enum DclExecError {
    /// 没有管理面上下文（没配 `BICDB_HOME`）。
    NoHome,
    /// 不是管理面会话（不在 `public` 上）。
    NotPublic,
    /// 活动事务中发 DCL。
    InTransaction,
    /// 实例注册表（全局控制文件）。
    Registry(String),
    /// 目录写侧（`catalog::dcl`）。
    Catalog(catdcl::DclError),
    /// 路径校验/文件系统。
    Path(String),
    /// 本切片未落地的组（具名拒绝，点明缺什么）。
    Pending(String),
}

impl std::fmt::Display for DclExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DclExecError::NoHome => write!(
                f,
                "管理面语句需要一个已部署的实例根（`BICDB_HOME`）——\
                 注册表在 `<BICDB_HOME>/control/`；装法见 scripts/install.sh"
            ),
            DclExecError::NotPublic => write!(
                f,
                "管理面语句只在 PUBLIC 工作区上执行（`<BICDB_HOME>/public`）——\
                 当前连接的不是它"
            ),
            DclExecError::InTransaction => {
                write!(f, "活动事务中发 DCL ⇒ 拒绝（与 DDL 同规，REQ-TXN-016）")
            }
            DclExecError::Registry(w) => write!(f, "实例注册表：{w}"),
            DclExecError::Catalog(e) => write!(f, "{e}"),
            DclExecError::Path(w) => write!(f, "{w}"),
            DclExecError::Pending(w) => write!(f, "{w}"),
        }
    }
}

impl std::error::Error for DclExecError {}

impl From<catdcl::DclError> for DclExecError {
    fn from(e: catdcl::DclError) -> Self {
        DclExecError::Catalog(e)
    }
}

/// 时间戳（墙钟毫秒；取不到给 0——它不是关键路径）。
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// 字节串 → 文本（DCL 的字面量都是 UTF-8 口径）。
fn utf8(b: &[u8], what: &str) -> Result<String, DclExecError> {
    String::from_utf8(b.to_vec()).map_err(|_| DclExecError::Path(format!("{what}不是有效的 UTF-8")))
}

/// 这条语句是不是"**本人**改密"（`ALTER USER <自己> … IDENTIFIED BY '<新>' REPLACE '<旧>'`）。
///
/// **两处共用同一份判据**：会话层的身份资格（`session::check_identity`）与
/// 本层的 DCL 资格——两处各写一份必然漂移。
///
/// 形态取自 Oracle：`REPLACE '<旧口令>'` 是**本人**改密的写法，
/// "特权用户 `ALTER USER` 其他用户时无需旧密码"（证据包 `auth-20261007` §3）。
#[must_use]
pub fn is_self_password_change(stmt: &Stmt, subject: &str) -> bool {
    match stmt {
        Stmt::AlterUser(u) => {
            u.name == subject
                && matches!(
                    u.action,
                    crate::ast::AlterUserAction::ReplacePassword { .. }
                )
        }
        _ => false,
    }
}

/// **一条语句是不是管理面语句**（会话层用它决定"要不要 public"）。
#[must_use]
pub fn is_management(stmt: &Stmt) -> bool {
    matches!(
        stmt,
        Stmt::CreateFilesystem(_)
            | Stmt::AlterFilesystem(_)
            | Stmt::DropFilesystem(_)
            | Stmt::CreateWorkspace(_)
            | Stmt::AlterWorkspace(_)
            | Stmt::CreateUser(_)
            | Stmt::AlterUser(_)
            | Stmt::DropUser(_)
            | Stmt::AlterDatabase(_)
    )
}

impl<'a, 'b, 'io, 'f> Session<'a, 'b, 'io, 'f> {
    /// **执行一条 DCL**（`BoundStatement::Dcl` 的出口）。
    ///
    /// # Errors
    /// 见 [`DclExecError`]（各条都有具名文案）。
    pub fn execute_dcl(&mut self, stmt: &Stmt) -> Result<QueryResult, SessionError> {
        // ① 资格检查**先于**对象查找：管理面只在 `public` 上。
        if !self.on_public_workspace() {
            return Err(self.dcl_err(DclExecError::NotPublic));
        }
        // ②**身份**资格：具名主体只放行"本人改密"这一条（D6）。
        //   （会话层在绑定前已拦一次；这里是执行层自己的闸门——直接调本函数的
        //    调用方也越不过去。）
        if let Some(id) = self.identity() {
            if !is_self_password_change(stmt, id.name()) {
                return Err(self.dcl_err(DclExecError::Path(format!(
                    "管理面语句需要**管理面身份**（本机 = 控制套接字的文件权限）——\
                     主体 `{}` 只能对自己用 `ALTER USER … IDENTIFIED BY … REPLACE …`",
                    id.name()
                ))));
            }
        }
        if self.in_transaction() {
            return Err(self.dcl_err(DclExecError::InTransaction));
        }
        match stmt {
            Stmt::CreateFilesystem(s) => self.dcl_create_filesystem(s),
            Stmt::AlterFilesystem(s) => self.dcl_alter_filesystem(s),
            Stmt::DropFilesystem(s) => self.dcl_drop_filesystem(s),
            Stmt::CreateWorkspace(s) => self.dcl_create_workspace(s),
            Stmt::AlterWorkspace(s) => self.dcl_alter_workspace(s),
            Stmt::Drop(d)
                if d.remove_type == crate::ast::ObjectType::Workspace
                    && !d.workspaces.is_empty() =>
            {
                self.dcl_drop_workspace(d)
            }
            Stmt::CreateUser(s) => self.dcl_create_user(s),
            Stmt::AlterUser(s) => self.dcl_alter_user(s),
            Stmt::DropUser(s) => self.dcl_drop_user(s),
            Stmt::AlterDatabase(s) => self.dcl_schema_template(s),
            other => Err(self.dcl_err(DclExecError::Pending(format!(
                "`{}` 不是管理面语句",
                stmt_label(other)
            )))),
        }
    }

    fn dcl_err(&self, e: DclExecError) -> SessionError {
        let _ = self;
        SessionError::Dcl(e.to_string())
    }

    fn dcl_ctx(&self) -> Result<DclContext<'a>, SessionError> {
        let home = self
            .dcl_home_root()
            .ok_or_else(|| SessionError::Dcl(DclExecError::NoHome.to_string()))?;
        let io = self
            .dcl_io()
            .ok_or_else(|| SessionError::Dcl(DclExecError::NoHome.to_string()))?;
        Ok(DclContext {
            home_root: home.to_path_buf(),
            io,
            provisioner: self.dcl_provisioner(),
            controller: self.dcl_controller(),
            pbkdf2_iterations: self.pbkdf2_iterations(),
        })
    }

    /// **打开实例注册表**（不存在就现场建立——第一次在空 home 上用 DCL）。
    fn dcl_open_registry(
        &self,
        ctx: &DclContext<'a>,
    ) -> Result<GlobalControlFile<'a>, SessionError> {
        let [a, b] = ctx.global_ctl_paths();
        if !a.exists() && !b.exists() {
            if let Some(dir) = a.parent() {
                std::fs::create_dir_all(dir)
                    .map_err(|e| SessionError::Dcl(format!("建 {} 失败：{e}", dir.display())))?;
            }
            let lib = bicdb_storage::globalctl::LibraryEntry::new(
                bicdb_storage::globalctl::generate_library_id(),
                now_ms(),
            );
            return GlobalControlFile::format(ctx.io, &a, &b, &lib)
                .map_err(|e| SessionError::Dcl(DclExecError::Registry(e.to_string()).to_string()));
        }
        GlobalControlFile::open(ctx.io, &a, &b)
            .map_err(|e| SessionError::Dcl(DclExecError::Registry(e.to_string()).to_string()))
    }

    // ───────────────────────── W 组：工作区生命周期 ─────────────────────────

    /// `CREATE WORKSPACE <名> [DEFAULT FILESYSTEM …] [QUOTA …]`（W1）。
    ///
    /// **三步协议**（`arch/02` §2.11）：① 文件面 ② `public.ws$`（属性）
    /// ③ 实例注册表（**可见性最后**）——三步无法原子，靠"固定顺序 + 每步幂等 +
    /// 可见性最后"保证崩在中间留下的只是孤儿文件。
    fn dcl_create_workspace(
        &mut self,
        s: &crate::ast::CreateWorkspaceStmt,
    ) -> Result<QueryResult, SessionError> {
        let ctx = self.dcl_ctx()?;
        let provisioner = ctx.provisioner.ok_or_else(|| {
            self.dcl_err(DclExecError::Pending(
                "本会话没有工作区供给方（`CREATE WORKSPACE` 要建文件面）——\
                 经服务/CLI 连的会话才有"
                    .to_owned(),
            ))
        })?;
        let from_template = s
            .from_template
            .as_ref()
            .map(|name| String::from_utf8(name.clone()))
            .transpose()
            .map_err(|_| self.dcl_err(DclExecError::Path("模板名必须是 UTF-8".into())))?;
        let mut gcf = self.dcl_open_registry(&ctx)?;

        // **资格检查先于对象查找**：池非空（依赖顺序 FS → WORKSPACE）。
        let pool: Vec<_> = gcf
            .fs_members()
            .map_err(|e| self.dcl_err(DclExecError::Registry(e.to_string())))?
            .into_iter()
            .filter(|m| m.allocate)
            .collect();
        if pool.is_empty() {
            return Err(self.dcl_err(DclExecError::Path(
                "文件系统池为空（没有 `ALLOCATE = ON` 的成员）——先 `CREATE FILESYSTEM`\
                 （依赖顺序：FS → WORKSPACE → USER）"
                    .to_owned(),
            )));
        }
        let default_slot = match &s.default_fs {
            Some(r) => resolve_fs(&mut gcf, r).map_err(|e| self.dcl_err(e))?.0,
            None => pool[0].slot,
        };
        if catdcl::ws_by_name(self.catalog_mut(), &s.name)
            .map_err(|e| self.dcl_err(DclExecError::Catalog(e)))?
            .is_some()
        {
            return Err(
                self.dcl_err(DclExecError::Catalog(catdcl::DclError::Duplicate {
                    what: format!("工作区名 `{}` 已在册", s.name),
                })),
            );
        }
        let id = gcf
            .allocate_workspace_id()
            .map_err(|e| self.dcl_err(DclExecError::Registry(e.to_string())))?;
        let root = ctx.home_root.join(&s.name);
        if root.exists() {
            return Err(self.dcl_err(DclExecError::Path(format!(
                "{} 已存在——换一个名字，或先把那个目录收拾掉",
                root.display()
            ))));
        }

        // ① 文件面。
        let req = ProvisionRequest {
            root: root.clone(),
            workspace_id: id.as_raw(),
            name: s.name.clone(),
            seed_ini: ctx.home_root.join("public").join("bicdb.ini"),
            log_path: ctx.home_root.join("log").join(format!("{}.log", s.name)),
            from_template,
        };
        if let Err(error) = provisioner.provision(&req) {
            gcf.close()
                .map_err(|error| self.dcl_err(DclExecError::Registry(error.to_string())))?;
            return Err(self.dcl_err(DclExecError::Path(format!("建工作区文件面失败：{error}"))));
        }

        // ② 属性：盘级配额（`wq$`）+ `ws$` 一行（**无主容器**）。
        for q in &s.quotas {
            let slot = resolve_fs(&mut gcf, &q.fs).map_err(|e| self.dcl_err(e))?.0;
            let bytes = match q.amount {
                QuotaAmount::Bytes(b) => b,
                QuotaAmount::Unlimited => u64::MAX,
            };
            let eng = self.engine_ref();
            catdcl::put_wq(
                self.catalog_mut(),
                eng,
                &catdcl::WqEntry {
                    workspace_id: id.as_raw(),
                    fs_slot: slot,
                    quota_bytes: bytes,
                },
            )
            .map_err(|e| self.dcl_err(DclExecError::Catalog(e)))?;
        }
        let ws = WsEntry {
            workspace_id: id.as_raw(),
            user_id: None,
            name: s.name.clone(),
            status: ws_status::ACTIVE,
            ctime_ms: now_ms(),
            quota: [0; 4],
            default_fs: Some(default_slot),
        };
        let eng = self.engine_ref();
        catdcl::insert_ws(self.catalog_mut(), eng, &ws)
            .map_err(|e| self.dcl_err(DclExecError::Catalog(e)))?;

        // ③ 注册表（**可见性最后**）。
        let rec = bicdb_storage::globalctl::WorkspaceRecord::new(
            id,
            root.display().to_string(),
            now_ms(),
        )
        .map_err(|e| self.dcl_err(DclExecError::Registry(e.to_string())))?;
        gcf.insert_workspace(&rec)
            .map_err(|e| self.dcl_err(DclExecError::Registry(e.to_string())))?;
        gcf.sync()
            .map_err(|e| self.dcl_err(DclExecError::Registry(e.to_string())))?;
        gcf.close()
            .map_err(|e| self.dcl_err(DclExecError::Registry(e.to_string())))?;

        Ok(QueryResult::Ddl(format!(
            "CREATE WORKSPACE：`{}`（工作区号 {}，根 {}，默认盘槽位 {}）——**无主容器**，\
             用 `CREATE USER … USING WORKSPACE '{}'` 绑定属主",
            s.name,
            id.as_raw(),
            root.display(),
            default_slot,
            s.name
        )))
    }

    /// `ALTER WORKSPACE …`（W3–W6；W7 随模板切片）。
    fn dcl_alter_workspace(
        &mut self,
        s: &crate::ast::AlterWorkspaceStmt,
    ) -> Result<QueryResult, SessionError> {
        let ctx = self.dcl_ctx()?;
        let mut gcf = self.dcl_open_registry(&ctx)?;
        let (id, name) = resolve_workspace(self.catalog_mut(), &mut gcf, &s.workspace)
            .map_err(|e| self.dcl_err(e))?;

        match &s.action {
            crate::ast::AlterWorkspaceAction::Open(mode) => {
                let controller = ctx.controller.ok_or_else(|| {
                    self.dcl_err(DclExecError::Pending(
                        "OPEN 必须经正在运行的 daemon 状态控制器执行".into(),
                    ))
                })?;
                let wid = WorkspaceId::from_raw(id).expect("已解析工作区号");
                let rec = gcf
                    .workspace_by_id(wid)
                    .map_err(|error| self.dcl_err(DclExecError::Registry(error.to_string())))?
                    .ok_or_else(|| {
                        self.dcl_err(DclExecError::Registry(format!("工作区 {id} 不在册")))
                    })?;
                let root = PathBuf::from(String::from_utf8_lossy(&rec.root).into_owned());
                let result = controller
                    .open_workspace(id, &name, &root, *mode)
                    .map_err(|error| self.dcl_err(DclExecError::Path(error)))?;
                Ok(QueryResult::Ddl(result))
            }
            crate::ast::AlterWorkspaceAction::VerifyRecovery(scope) => {
                let controller = ctx.controller.ok_or_else(|| {
                    self.dcl_err(DclExecError::Pending(
                        "VERIFY RECOVERY 必须经正在运行的 daemon 状态控制器执行".into(),
                    ))
                })?;
                let wid = WorkspaceId::from_raw(id).expect("已解析工作区号");
                let rec = gcf
                    .workspace_by_id(wid)
                    .map_err(|error| self.dcl_err(DclExecError::Registry(error.to_string())))?
                    .ok_or_else(|| {
                        self.dcl_err(DclExecError::Registry(format!("工作区 {id} 不在册")))
                    })?;
                let root = PathBuf::from(String::from_utf8_lossy(&rec.root).into_owned());
                let result = controller
                    .verify_recovery(id, &name, &root, *scope)
                    .map_err(|error| self.dcl_err(DclExecError::Path(error)))?;
                Ok(QueryResult::Ddl(result))
            }
            crate::ast::AlterWorkspaceAction::AddFilesystem { fs, quota } => {
                let slot = resolve_fs(&mut gcf, fs).map_err(|e| self.dcl_err(e))?.0;
                let bytes = quota.as_ref().map_or(u64::MAX, |q| match q.amount {
                    QuotaAmount::Bytes(b) => b,
                    QuotaAmount::Unlimited => u64::MAX,
                });
                let eng = self.engine_ref();
                catdcl::put_wq(
                    self.catalog_mut(),
                    eng,
                    &catdcl::WqEntry {
                        workspace_id: id,
                        fs_slot: slot,
                        quota_bytes: bytes,
                    },
                )
                .map_err(|e| self.dcl_err(DclExecError::Catalog(e)))?;
                Ok(QueryResult::Ddl(format!(
                    "ALTER WORKSPACE：`{name}` 可在槽位 {slot} 上分配（上限 {}）",
                    if bytes == u64::MAX {
                        "UNLIMITED".to_owned()
                    } else {
                        format!("{bytes} 字节")
                    }
                )))
            }
            crate::ast::AlterWorkspaceAction::SetDefaultFilesystem { fs } => {
                let slot = resolve_fs(&mut gcf, fs).map_err(|e| self.dcl_err(e))?.0;
                let eng = self.engine_ref();
                catdcl::update_ws_row(self.catalog_mut(), eng, id, |w| w.default_fs = Some(slot))
                    .map_err(|e| self.dcl_err(DclExecError::Catalog(e)))?;
                Ok(QueryResult::Ddl(format!(
                    "ALTER WORKSPACE：`{name}` 的默认盘改为槽位 {slot}（只影响之后的分配）"
                )))
            }
            crate::ast::AlterWorkspaceAction::SetName(new) => {
                let new = String::from_utf8_lossy(new).into_owned();
                if catdcl::ws_by_name(self.catalog_mut(), &new)
                    .map_err(|e| self.dcl_err(DclExecError::Catalog(e)))?
                    .is_some()
                {
                    return Err(
                        self.dcl_err(DclExecError::Catalog(catdcl::DclError::Duplicate {
                            what: format!("工作区名 `{new}` 已在册"),
                        })),
                    );
                }
                let eng = self.engine_ref();
                // **名字是唯一键** ⇒ 删旧行 + 插新行（同一事务），不是就地改。
                catdcl::rename_ws(self.catalog_mut(), eng, id, &new)
                    .map_err(|e| self.dcl_err(DclExecError::Catalog(e)))?;
                Ok(QueryResult::Ddl(format!(
                    "ALTER WORKSPACE：`{name}` 改名为 `{new}`（**目录名不动**——根位置是注册表的事）"
                )))
            }
            crate::ast::AlterWorkspaceAction::SetQuota(items) => {
                let mut new_quota: [Option<u64>; 4] = [None; 4];
                for item in items {
                    let idx = match item.defname.as_str() {
                        "data" => 0,
                        "undo" => 1,
                        "temp" => 2,
                        "asset" => 3,
                        other => {
                            return Err(self.dcl_err(DclExecError::Catalog(
                                catdcl::DclError::OutOfRange {
                                    what: format!("配额键 `{other}` 不在闭集内"),
                                },
                            )))
                        }
                    };
                    let bytes = match &item.arg {
                        crate::ast::DefElemArg::Const(c) => match &c.value {
                            Some(crate::ast::ConstValue::Int(t))
                            | Some(crate::ast::ConstValue::Float(t)) => {
                                t.trim().parse::<u64>().map_err(|_| {
                                    self.dcl_err(DclExecError::Catalog(
                                        catdcl::DclError::OutOfRange {
                                            what: format!("配额值不是整数字节数：`{t}`"),
                                        },
                                    ))
                                })?
                            }
                            Some(crate::ast::ConstValue::Str(b)) => String::from_utf8_lossy(b)
                                .trim()
                                .parse::<u64>()
                                .map_err(|_| {
                                    self.dcl_err(DclExecError::Catalog(
                                        catdcl::DclError::OutOfRange {
                                            what: "配额值必须是整数字节数".to_owned(),
                                        },
                                    ))
                                })?,
                            other => {
                                return Err(self.dcl_err(DclExecError::Catalog(
                                    catdcl::DclError::OutOfRange {
                                        what: format!("配额值形态非法：{other:?}"),
                                    },
                                )))
                            }
                        },
                        crate::ast::DefElemArg::Ident(t) => {
                            t.trim().parse::<u64>().map_err(|_| {
                                self.dcl_err(DclExecError::Catalog(catdcl::DclError::OutOfRange {
                                    what: format!("配额值不是整数字节数：`{t}`"),
                                }))
                            })?
                        }
                    };
                    new_quota[idx] = Some(bytes);
                }
                let eng = self.engine_ref();
                catdcl::update_ws_row(self.catalog_mut(), eng, id, |w| {
                    for (i, v) in new_quota.iter().enumerate() {
                        if let Some(b) = v {
                            w.quota[i] = *b;
                        }
                    }
                })
                .map_err(|e| self.dcl_err(DclExecError::Catalog(e)))?;
                Ok(QueryResult::Ddl(format!(
                    "ALTER WORKSPACE：`{name}` 的角色级配额已更新（data/undo/temp/asset）"
                )))
            }
            crate::ast::AlterWorkspaceAction::ToTemplate { .. } => {
                Err(self.dcl_err(DclExecError::Pending(
                    "`TO TEMPLATE`（W7）随模板切片（D5：reflink 快照）".to_owned(),
                )))
            }
        }
    }

    /// `DROP WORKSPACE <引用>`（W8；**反向三步**：可见性先断 → 属性 → 文件）。
    fn dcl_drop_workspace(
        &mut self,
        d: &crate::ast::DropStmt,
    ) -> Result<QueryResult, SessionError> {
        let Some(first) = d.workspaces.first() else {
            return Err(self.dcl_err(DclExecError::Registry("DROP WORKSPACE 缺目标".to_owned())));
        };
        if d.workspaces.len() != 1 {
            return Err(self.dcl_err(DclExecError::Pending(
                "一次 DROP 多个工作区：本版只做单目标".to_owned(),
            )));
        }
        let ctx = self.dcl_ctx()?;
        let provisioner: &'a dyn WorkspaceProvisioner = ctx.provisioner.ok_or_else(|| {
            self.dcl_err(DclExecError::Pending(
                "本会话没有工作区供给方（`DROP WORKSPACE` 要删文件面）".to_owned(),
            ))
        })?;
        let mut gcf = self.dcl_open_registry(&ctx)?;
        let (id, name) =
            resolve_workspace(self.catalog_mut(), &mut gcf, first).map_err(|e| self.dcl_err(e))?;
        gcf.close()
            .map_err(|e| self.dcl_err(DclExecError::Registry(e.to_string())))?;
        // 在册判定 + 保留区 + 预检 + 反向三步，都在共用核心里。
        self.drop_workspace_by_id(&ctx, provisioner, id, &name, false)
            .map_err(|e| self.dcl_err(e))?;
        Ok(QueryResult::Ddl(format!(
            "DROP WORKSPACE：`{name}`（工作区号 {id}）已删——注册表与 `ws$` 留墓碑，\
             目录已清空（**不可逆**）"
        )))
    }

    // ───────────────────────── U 组：用户生命周期 ─────────────────────────

    fn dcl_schema_template(
        &mut self,
        stmt: &crate::ast::AlterDatabaseStmt,
    ) -> Result<QueryResult, SessionError> {
        let ctx = self.dcl_ctx()?;
        let provider = ctx
            .provisioner
            .ok_or_else(|| self.dcl_err(DclExecError::Pending("本会话没有模板供给方".into())))?;
        let decode = |name: &[u8]| {
            String::from_utf8(name.to_vec())
                .map_err(|_| SessionError::Dcl("模板名必须是 UTF-8".into()))
        };
        match &stmt.action {
            crate::ast::AlterDatabaseAction::AddTemplate {
                name,
                from,
                graph_data,
            } => {
                let name = decode(name)?;
                let mut registry = self.dcl_open_registry(&ctx)?;
                let root = (|| {
                    let (id, _) = resolve_workspace(self.catalog_mut(), &mut registry, from)
                        .map_err(|error| self.dcl_err(error))?;
                    let entry = catdcl::ws_by_id(self.catalog_mut(), id)
                        .map_err(|error| self.dcl_err(DclExecError::Catalog(error)))?
                        .ok_or_else(|| SessionError::Dcl("源工作区不存在".into()))?;
                    if entry.user_id.is_some()
                        || entry.status != ws_status::ACTIVE
                        || entry.name == "public"
                    {
                        return Err(SessionError::Dcl(
                        "初始化模板要求无主、ACTIVE、非 PUBLIC 的源工作区；不能复制用户所属工作区"
                            .into(),
                    ));
                    }
                    let root = registry
                        .workspaces()
                        .map_err(|error| SessionError::Dcl(error.to_string()))?
                        .into_iter()
                        .find(|workspace| workspace.workspace_id.as_raw() == id)
                        .ok_or_else(|| SessionError::Dcl("源工作区注册根不存在".into()))?
                        .root;
                    Ok(root)
                })();
                registry
                    .close()
                    .map_err(|error| SessionError::Dcl(error.to_string()))?;
                let root = root?;
                let root = std::str::from_utf8(&root)
                    .map_err(|_| SessionError::Dcl("源根目录不是 UTF-8".into()))?;
                if *graph_data {
                    provider.add_graph_template(&ctx.home_root, Path::new(root), &name)
                } else {
                    provider.add_schema_template(&ctx.home_root, Path::new(root), &name)
                }
                .map_err(SessionError::Dcl)?;
                Ok(QueryResult::Ddl(format!(
                    "ADD TEMPLATE `{name}`：{}",
                    if *graph_data {
                        "带图数据初始化模板（业务表零行）"
                    } else {
                        "空白初始化结构模板（不含业务行）"
                    }
                )))
            }
            crate::ast::AlterDatabaseAction::DropTemplate { name } => {
                let name = decode(name)?;
                provider
                    .drop_schema_template(&ctx.home_root, &name)
                    .map_err(SessionError::Dcl)?;
                Ok(QueryResult::Ddl(format!(
                    "DROP TEMPLATE `{name}`：已删除；既有工作区不受影响"
                )))
            }
        }
    }

    /// `CREATE USER <主体> IDENTIFIED BY '<口令>' USING WORKSPACE <引用>`（U1）。
    ///
    /// **写序**：① 主体行（`user$`）② 绑定（`ws$.user_id`）。
    /// 崩在中间 ⇒ "有主体、没工作区"——可用 `ALTER USER … USING WORKSPACE` 补绑；
    /// 反过来（先绑后插主体）会留下**指向不存在主体的区**，那更难收拾。
    fn dcl_create_user(
        &mut self,
        s: &crate::ast::CreateUserStmt,
    ) -> Result<QueryResult, SessionError> {
        let ctx = self.dcl_ctx()?;
        let mut gcf = self.dcl_open_registry(&ctx)?;
        let (ws_id, ws_name) = resolve_workspace(self.catalog_mut(), &mut gcf, &s.using_workspace)
            .map_err(|e| self.dcl_err(e))?;
        gcf.close()
            .map_err(|e| self.dcl_err(DclExecError::Registry(e.to_string())))?;

        if catdcl::user_by_name(self.catalog_mut(), &s.name)
            .map_err(|e| self.dcl_err(DclExecError::Catalog(e)))?
            .is_some()
        {
            return Err(
                self.dcl_err(DclExecError::Catalog(catdcl::DclError::Duplicate {
                    what: format!("主体 `{}` 已存在", s.name),
                })),
            );
        }
        // 工作区必须**无主**（或已属本人——本人还没建出来，故只可能是无主）。
        let ws = catdcl::ws_by_id(self.catalog_mut(), ws_id)
            .map_err(|e| self.dcl_err(DclExecError::Catalog(e)))?
            .ok_or_else(|| {
                self.dcl_err(DclExecError::Registry(format!("工作区 {ws_id} 不在册")))
            })?;
        if let Some(owner) = ws.user_id {
            let owner_name = catdcl::user_by_id(self.catalog_mut(), owner)
                .map_err(|e| self.dcl_err(DclExecError::Catalog(e)))?
                .map_or_else(|| format!("主体 {owner}"), |u| format!("`{}`", u.name));
            return Err(self.dcl_err(DclExecError::Path(format!(
                "工作区 `{ws_name}` 已属于 {owner_name}——**别人的工作区没有入口**\
                 （一个工作区一个属主）"
            ))));
        }
        let password = String::from_utf8_lossy(&s.password).into_owned();
        let hash = bicdb_common::pbkdf2::hash_password(&password, ctx.pbkdf2_iterations)
            .map_err(|e| self.dcl_err(DclExecError::Path(e.to_string())))?;
        let user_id = catdcl::allocate_user_id(self.catalog_mut())
            .map_err(|e| self.dcl_err(DclExecError::Catalog(e)))?;
        let eng = self.engine_ref();
        catdcl::insert_user(
            self.catalog_mut(),
            eng,
            &catdcl::UserEntry {
                user_id,
                name: s.name.clone(),
                status: catdcl::user_status::ACTIVE,
                ctime_ms: now_ms(),
            },
            &hash,
        )
        .map_err(|e| self.dcl_err(DclExecError::Catalog(e)))?;
        // ② 绑定（`ws$.user_id` 从 NULL 落到该主体）。
        let eng = self.engine_ref();
        catdcl::update_ws_row(self.catalog_mut(), eng, ws_id, |w| {
            w.user_id = Some(user_id)
        })
        .map_err(|e| self.dcl_err(DclExecError::Catalog(e)))?;

        Ok(QueryResult::Ddl(format!(
            "CREATE USER：`{}`（主体号 {}）——工作区 `{ws_name}` 已绑定；\
             **口令散列不可逆**（遗忘只能重置，读不回原文）",
            s.name, user_id
        )))
    }

    /// `ALTER USER …`（U2–U6）。
    fn dcl_alter_user(
        &mut self,
        s: &crate::ast::AlterUserStmt,
    ) -> Result<QueryResult, SessionError> {
        let ctx = self.dcl_ctx()?;
        let user = catdcl::user_by_name(self.catalog_mut(), &s.name)
            .map_err(|e| self.dcl_err(DclExecError::Catalog(e)))?
            .ok_or_else(|| {
                self.dcl_err(DclExecError::Catalog(catdcl::DclError::NotFound {
                    what: format!("主体 `{}`", s.name),
                }))
            })?;

        match &s.action {
            // U2：admin 重置口令（**不需要旧口令**；`EXPIRE` = 下次登录必须改密）。
            crate::ast::AlterUserAction::SetPassword { new, expire } => {
                let new = String::from_utf8_lossy(new).into_owned();
                let hash = bicdb_common::pbkdf2::hash_password(&new, ctx.pbkdf2_iterations)
                    .map_err(|e| self.dcl_err(DclExecError::Path(e.to_string())))?;
                let eng = self.engine_ref();
                catdcl::set_user_password(self.catalog_mut(), eng, user.user_id, &hash)
                    .map_err(|e| self.dcl_err(DclExecError::Catalog(e)))?;
                if *expire {
                    let eng = self.engine_ref();
                    catdcl::set_user_status(
                        self.catalog_mut(),
                        eng,
                        user.user_id,
                        catdcl::user_status::EXPIRED,
                    )
                    .map_err(|e| self.dcl_err(DclExecError::Catalog(e)))?;
                }
                Ok(QueryResult::Ddl(format!(
                    "ALTER USER：`{}` 的口令已重置{}——**旧口令即时失效，且读不回原文**\
                     （审计留痕：重置动作本身）",
                    s.name,
                    if *expire {
                        "（`EXPIRE`：下次登录必须改密）"
                    } else {
                        ""
                    }
                )))
            }
            // U3：本人改密（要求旧口令）。**本版没有会话身份**，但旧口令**可核验**：
            //     策略是"知道旧口令才能改"——比"拒绝"更接近语义，且不放松任何东西。
            crate::ast::AlterUserAction::ReplacePassword { new, old } => {
                let old = String::from_utf8_lossy(old).into_owned();
                let stored = catdcl::password_hash(self.catalog_mut(), user.user_id)
                    .map_err(|e| self.dcl_err(DclExecError::Catalog(e)))?;
                if !bicdb_common::pbkdf2::verify_password(&old, &stored) {
                    return Err(self.dcl_err(DclExecError::Path(
                        "旧口令不符——`REPLACE '<旧口令>'` 要的是**当前口令**；\
                         遗忘请让 admin 用 `IDENTIFIED BY '<新口令>'` 重置"
                            .to_owned(),
                    )));
                }
                let new = String::from_utf8_lossy(new).into_owned();
                let hash = bicdb_common::pbkdf2::hash_password(&new, ctx.pbkdf2_iterations)
                    .map_err(|e| self.dcl_err(DclExecError::Path(e.to_string())))?;
                let eng = self.engine_ref();
                catdcl::set_user_password(self.catalog_mut(), eng, user.user_id, &hash)
                    .map_err(|e| self.dcl_err(DclExecError::Catalog(e)))?;
                let eng = self.engine_ref();
                catdcl::set_user_status(
                    self.catalog_mut(),
                    eng,
                    user.user_id,
                    catdcl::user_status::ACTIVE,
                )
                .map_err(|e| self.dcl_err(DclExecError::Catalog(e)))?;
                // **受限会话就地解除**（本人改密成功）：`user$` 已转 `ACTIVE`，
                // 会话还拦着就是"改完密还得重连"——那是自找的。
                let was_expired = self.identity().is_some_and(|i| i.is_expired());
                self.clear_identity_expiry();
                Ok(QueryResult::Ddl(format!(
                    "ALTER USER：`{}` 已换新口令（旧口令核验通过）{}",
                    s.name,
                    if was_expired {
                        "——本会话的过期限制已解除"
                    } else {
                        ""
                    }
                )))
            }
            // U4：PAUSE / RESUME（≈ Oracle `ACCOUNT LOCK|UNLOCK`）。
            crate::ast::AlterUserAction::SetPaused(paused) => {
                let status = if *paused {
                    catdcl::user_status::PAUSED
                } else {
                    catdcl::user_status::ACTIVE
                };
                let eng = self.engine_ref();
                catdcl::set_user_status(self.catalog_mut(), eng, user.user_id, status)
                    .map_err(|e| self.dcl_err(DclExecError::Catalog(e)))?;
                Ok(QueryResult::Ddl(format!(
                    "ALTER USER：`{}` 已{}（{}）",
                    s.name,
                    if *paused { "暂停" } else { "恢复" },
                    if *paused {
                        "拒绝新会话；**已开会话不掐断**——要掐断用会话管理面"
                    } else {
                        "可再开会话"
                    }
                )))
            }
            // U5：加绑（1 用户 : N 工作区）。
            crate::ast::AlterUserAction::UsingWorkspace(r) => {
                let mut gcf = self.dcl_open_registry(&ctx)?;
                let (ws_id, ws_name) = resolve_workspace(self.catalog_mut(), &mut gcf, r)
                    .map_err(|e| self.dcl_err(e))?;
                gcf.close()
                    .map_err(|e| self.dcl_err(DclExecError::Registry(e.to_string())))?;
                let ws = catdcl::ws_by_id(self.catalog_mut(), ws_id)
                    .map_err(|e| self.dcl_err(DclExecError::Catalog(e)))?
                    .ok_or_else(|| {
                        self.dcl_err(DclExecError::Registry(format!("工作区 {ws_id} 不在册")))
                    })?;
                match ws.user_id {
                    Some(owner) if owner == user.user_id => {
                        return Err(self.dcl_err(DclExecError::Path(format!(
                            "工作区 `{ws_name}` 本来就属于 `{}`",
                            s.name
                        ))))
                    }
                    Some(_) => {
                        return Err(self.dcl_err(DclExecError::Path(format!(
                            "工作区 `{ws_name}` 已属于别的用户——**别人的工作区没有入口**"
                        ))))
                    }
                    None => {}
                }
                let eng = self.engine_ref();
                catdcl::update_ws_row(self.catalog_mut(), eng, ws_id, |w| {
                    w.user_id = Some(user.user_id)
                })
                .map_err(|e| self.dcl_err(DclExecError::Catalog(e)))?;
                Ok(QueryResult::Ddl(format!(
                    "ALTER USER：`{}` 加绑工作区 `{ws_name}`",
                    s.name
                )))
            }
            // U6：解绑（区变无主；**最后一个不允许解**）。
            crate::ast::AlterUserAction::DropWorkspace(r) => {
                let mut gcf = self.dcl_open_registry(&ctx)?;
                let (ws_id, ws_name) = resolve_workspace(self.catalog_mut(), &mut gcf, r)
                    .map_err(|e| self.dcl_err(e))?;
                gcf.close()
                    .map_err(|e| self.dcl_err(DclExecError::Registry(e.to_string())))?;
                let owned = catdcl::workspaces_of_user(self.catalog_mut(), user.user_id)
                    .map_err(|e| self.dcl_err(DclExecError::Catalog(e)))?;
                let ws = owned
                    .iter()
                    .find(|w| w.workspace_id == ws_id)
                    .ok_or_else(|| {
                        self.dcl_err(DclExecError::Path(format!(
                            "工作区 `{ws_name}` 不在 `{}` 名下",
                            s.name
                        )))
                    })?;
                let _ = ws;
                if owned.len() <= 1 {
                    return Err(self.dcl_err(DclExecError::Path(format!(
                        "`{}` 只剩这一个工作区——解绑后他一个区都没有（拒绝；要删用 \
                         `DROP WORKSPACE` 或 `DROP USER … CASCADE`）",
                        s.name
                    ))));
                }
                let eng = self.engine_ref();
                catdcl::update_ws_row(self.catalog_mut(), eng, ws_id, |w| w.user_id = None)
                    .map_err(|e| self.dcl_err(DclExecError::Catalog(e)))?;
                Ok(QueryResult::Ddl(format!(
                    "ALTER USER：`{}` 解绑工作区 `{ws_name}`——该区变**无主**（不可打开）",
                    s.name
                )))
            }
        }
    }

    /// `DROP USER <主体> [CASCADE]`（U7）。
    fn dcl_drop_user(&mut self, s: &crate::ast::DropUserStmt) -> Result<QueryResult, SessionError> {
        let ctx = self.dcl_ctx()?;
        let user = catdcl::user_by_name(self.catalog_mut(), &s.name)
            .map_err(|e| self.dcl_err(DclExecError::Catalog(e)))?
            .ok_or_else(|| {
                self.dcl_err(DclExecError::Catalog(catdcl::DclError::NotFound {
                    what: format!("主体 `{}`", s.name),
                }))
            })?;
        let owned = catdcl::workspaces_of_user(self.catalog_mut(), user.user_id)
            .map_err(|e| self.dcl_err(DclExecError::Catalog(e)))?;
        let mut dropped = Vec::new();
        if !owned.is_empty() {
            if !s.cascade {
                let names: Vec<String> = owned.iter().map(|w| format!("`{}`", w.name)).collect();
                return Err(self.dcl_err(DclExecError::Path(format!(
                    "`{}` 名下还有 {} 个工作区（{}）——**删除要么明说，要么不做**：\
                     加 `CASCADE` 连它们一起删（**不可逆**），或先 `ALTER USER … DROP WORKSPACE` 解绑",
                    s.name,
                    owned.len(),
                    names.join("、")
                ))));
            }
            // CASCADE：逐个走 **DROP WORKSPACE 的反向三步**。
            let provisioner = ctx.provisioner.ok_or_else(|| {
                self.dcl_err(DclExecError::Pending(
                    "`DROP USER … CASCADE` 要删工作区文件面——本会话没有供给方".to_owned(),
                ))
            })?;
            for w in &owned {
                self.drop_workspace_by_id(&ctx, provisioner, w.workspace_id, &w.name, true)
                    .map_err(|e| self.dcl_err(e))?;
                dropped.push(w.name.clone());
            }
        }
        let eng = self.engine_ref();
        catdcl::delete_user(self.catalog_mut(), eng, user.user_id)
            .map_err(|e| self.dcl_err(DclExecError::Catalog(e)))?;
        Ok(QueryResult::Ddl(format!(
            "DROP USER：`{}`（主体号 {}）已删{}{}",
            s.name,
            user.user_id,
            if dropped.is_empty() {
                String::new()
            } else {
                format!(
                    "，连同 {} 个工作区（{}）",
                    dropped.len(),
                    dropped.join("、")
                )
            },
            if s.cascade { "（CASCADE）" } else { "" }
        )))
    }

    /// **DROP WORKSPACE 的核心**（U7 的 `CASCADE` 与 W8 共用）。
    fn drop_workspace_by_id(
        &mut self,
        ctx: &DclContext<'a>,
        provisioner: &'a dyn WorkspaceProvisioner,
        id: u64,
        name: &str,
        allow_owned: bool,
    ) -> Result<(), DclExecError> {
        if name == "public" {
            return Err(DclExecError::Path(
                "`public` 是保留工作区——不能删".to_owned(),
            ));
        }
        let _ = allow_owned; // CASCADE 已由调用方判过；留参数备将来细分
        let mut gcf = GlobalControlFile::open(
            ctx.io,
            &ctx.global_ctl_paths()[0],
            &ctx.global_ctl_paths()[1],
        )
        .map_err(|e| DclExecError::Registry(e.to_string()))?;
        let wid = WorkspaceId::from_raw(id)
            .ok_or_else(|| DclExecError::Registry(format!("工作区号 {id} 越界")))?;
        let rec = gcf
            .workspace_by_id(wid)
            .map_err(|e| DclExecError::Registry(e.to_string()))?
            .ok_or_else(|| DclExecError::Registry(format!("工作区 {id} 不在册")))?;
        let root = PathBuf::from(String::from_utf8_lossy(&rec.root).into_owned());
        provisioner
            .check_deprovisionable(&root)
            .map_err(DclExecError::Path)?;
        gcf.mark_workspace_dropped(wid, now_ms())
            .map_err(|e| DclExecError::Registry(e.to_string()))?;
        gcf.sync()
            .map_err(|e| DclExecError::Registry(e.to_string()))?;
        gcf.close()
            .map_err(|e| DclExecError::Registry(e.to_string()))?;
        let eng = self.engine_ref();
        catdcl::update_ws_row(self.catalog_mut(), eng, id, |w| {
            w.status = ws_status::DROPPED
        })
        .map_err(DclExecError::Catalog)?;
        provisioner.deprovision(&root).map_err(DclExecError::Path)?;
        Ok(())
    }

    // ───────────────────────── F 组：文件系统池 ─────────────────────────

    /// `CREATE FILESYSTEM <名> USING '<路径>'`（F1）。
    fn dcl_create_filesystem(
        &mut self,
        s: &CreateFilesystemStmt,
    ) -> Result<QueryResult, SessionError> {
        let name = s.name.clone();
        let path = utf8(&s.path, "路径").map_err(|e| self.dcl_err(e))?;
        let path_abs = absolute(&path)?;
        check_directory(&path_abs).map_err(|e| self.dcl_err(e))?;

        // ① 注册表（**读**槽位——写留到最后）。
        let ctx = self.dcl_ctx()?;
        let mut gcf = self.dcl_open_registry(&ctx)?;
        let slot = gcf
            .next_fs_slot()
            .map_err(|e| SessionError::Dcl(DclExecError::Registry(e.to_string()).to_string()))?;

        // ② 属性：`public.fs$` 一行（DDL 事务）。
        let (total, free) = fs_sizes(&path_abs);
        let entry = FsEntry {
            slot,
            name: name.clone(),
            path: path_abs.display().to_string(),
            status: fs_status::IN_POOL,
            total_bytes: total,
            free_bytes: free,
            allocate: true,
        };
        let eng = self.engine_ref();
        catdcl::insert_fs(self.catalog_mut(), eng, &entry)
            .map_err(|e| self.dcl_err(DclExecError::Catalog(e)))?;

        // ③ 注册表（**可见性最后**）。
        gcf.insert_fs(
            name.as_bytes(),
            path_abs.display().to_string().as_bytes(),
            now_ms(),
        )
        .map_err(|e| SessionError::Dcl(DclExecError::Registry(e.to_string()).to_string()))?;
        gcf.sync()
            .map_err(|e| SessionError::Dcl(DclExecError::Registry(e.to_string()).to_string()))?;
        // 收尾要**真关句柄**（`drop` 不够：`GlobalControlFile` 自己不实现 `Drop`）。
        gcf.close()
            .map_err(|e| SessionError::Dcl(DclExecError::Registry(e.to_string()).to_string()))?;

        Ok(QueryResult::Ddl(format!(
            "CREATE FILESYSTEM：`{name}` 已入池（槽位 {slot}，路径 {}）",
            path_abs.display()
        )))
    }

    /// `ALTER FILESYSTEM <引用> SET ALLOCATE = ON|OFF`（F2）。
    fn dcl_alter_filesystem(
        &mut self,
        s: &AlterFilesystemStmt,
    ) -> Result<QueryResult, SessionError> {
        let ctx = self.dcl_ctx()?;
        let mut gcf = self.dcl_open_registry(&ctx)?;
        let (slot, name) = resolve_fs(&mut gcf, &s.fs).map_err(|e| self.dcl_err(e))?;
        gcf.set_fs_allocate(slot, s.allocate)
            .map_err(|e| SessionError::Dcl(DclExecError::Registry(e.to_string()).to_string()))?;
        gcf.sync()
            .map_err(|e| SessionError::Dcl(DclExecError::Registry(e.to_string()).to_string()))?;
        // 收尾要**真关句柄**（`drop` 不够：`GlobalControlFile` 自己不实现 `Drop`）。
        gcf.close()
            .map_err(|e| SessionError::Dcl(DclExecError::Registry(e.to_string()).to_string()))?;
        let eng = self.engine_ref();
        catdcl::update_fs_row(self.catalog_mut(), eng, slot, |e| e.allocate = s.allocate)
            .map_err(|e| self.dcl_err(DclExecError::Catalog(e)))?;
        Ok(QueryResult::Ddl(format!(
            "ALTER FILESYSTEM：`{name}` ALLOCATE = {}",
            if s.allocate { "ON" } else { "OFF" }
        )))
    }

    /// `DROP FILESYSTEM <引用>`（F3；**硬前置**：该路径下没有数据文件）。
    fn dcl_drop_filesystem(&mut self, s: &DropFilesystemStmt) -> Result<QueryResult, SessionError> {
        let ctx = self.dcl_ctx()?;
        let mut gcf = self.dcl_open_registry(&ctx)?;
        let (slot, name) = resolve_fs(&mut gcf, &s.fs).map_err(|e| self.dcl_err(e))?;
        let member = gcf
            .fs_by_slot(slot)
            .map_err(|e| SessionError::Dcl(DclExecError::Registry(e.to_string()).to_string()))?
            .ok_or_else(|| self.dcl_err(DclExecError::Registry(format!("池槽位 {slot} 不见了"))))?;
        let path = PathBuf::from(String::from_utf8_lossy(&member.path).into_owned());

        // **硬前置**：任何工作区（在册的）的根目录落在该路径下 ⇒ 拒绝。
        let mut blockers: Vec<String> = Vec::new();
        for w in gcf
            .workspaces()
            .map_err(|e| SessionError::Dcl(DclExecError::Registry(e.to_string()).to_string()))?
        {
            let root = String::from_utf8_lossy(&w.root).into_owned();
            if Path::new(&root).starts_with(&path) {
                blockers.push(root);
            }
        }
        if !blockers.is_empty() {
            return Err(self.dcl_err(DclExecError::Path(format!(
                "该路径下还有 {} 个工作区（{}）——先 `DROP WORKSPACE` 或搬走它们，\
                 再 `DROP FILESYSTEM`（F3 的硬前置）",
                blockers.len(),
                blockers.join("、")
            ))));
        }

        gcf.mark_fs_removed(slot)
            .map_err(|e| SessionError::Dcl(DclExecError::Registry(e.to_string()).to_string()))?;
        gcf.sync()
            .map_err(|e| SessionError::Dcl(DclExecError::Registry(e.to_string()).to_string()))?;
        // 收尾要**真关句柄**（`drop` 不够：`GlobalControlFile` 自己不实现 `Drop`）。
        gcf.close()
            .map_err(|e| SessionError::Dcl(DclExecError::Registry(e.to_string()).to_string()))?;
        let eng = self.engine_ref();
        catdcl::update_fs_row(self.catalog_mut(), eng, slot, |e| {
            e.status = fs_status::REMOVED;
            e.allocate = false;
        })
        .map_err(|e| self.dcl_err(DclExecError::Catalog(e)))?;
        Ok(QueryResult::Ddl(format!(
            "DROP FILESYSTEM：`{name}` 已移出池（槽位 {slot} 保留为墓碑；目录本身不动）"
        )))
    }
}

/// **解析工作区引用**（名字或号；W/U/T 组共用）。
///
/// **两处都要对上**：`public.ws$`（属性面）与实例注册表（位置面）——
/// 只有一处有 ⇒ 报"不一致"并指路（崩在三步协议中间的已知形态）。
fn resolve_workspace(
    cat: &mut bicdb_catalog::Catalog<'_>,
    gcf: &mut GlobalControlFile<'_>,
    r: &WorkRef,
) -> Result<(u64, String), DclExecError> {
    // 属性面先查（名字唯一在这一面）。
    let by_attr: Option<WsEntry> = match (r.id, &r.name) {
        (Some(id), _) => catdcl::ws_by_id(cat, id)?,
        (None, Some(name)) => {
            let name = String::from_utf8_lossy(name).into_owned();
            catdcl::ws_by_name(cat, &name)?
        }
        (None, None) => return Err(DclExecError::Registry("工作区引用为空".to_owned())),
    };
    let Some(entry) = by_attr else {
        return Err(DclExecError::Registry(match (&r.name, r.id) {
            (Some(n), _) => format!("工作区 `{}` 不在册", String::from_utf8_lossy(n)),
            (_, Some(id)) => format!("工作区 {id} 不在册"),
            _ => "工作区不在册".to_owned(),
        }));
    };
    if entry.status == bicdb_catalog::dcl::ws_status::DROPPED {
        return Err(DclExecError::Registry(format!(
            "工作区 `{}` 已删（墓碑）",
            entry.name
        )));
    }
    // 位置面（注册表）：在册且根目录对得上。
    let wid = WorkspaceId::from_raw(entry.workspace_id)
        .ok_or_else(|| DclExecError::Registry(format!("工作区号 {} 越界", entry.workspace_id)))?;
    let rec = gcf
        .workspace_by_id(wid)
        .map_err(|e| DclExecError::Registry(e.to_string()))?;
    match rec {
        Some(r) if r.status == bicdb_storage::globalctl::WS_ACTIVE => {
            Ok((entry.workspace_id, entry.name))
        }
        Some(_) => Err(DclExecError::Registry(format!(
            "工作区 `{}` 在 `ws$` 里在册，但注册表里已删——             两处不一致（崩在建/删的中间步骤）；先 `bicdb list` 看一眼再决定清理方向",
            entry.name
        ))),
        None => Err(DclExecError::Registry(format!(
            "工作区 `{}` 在 `ws$` 里在册，但注册表里没有它——             它**不算存在**（可见性以注册表为准）；先 `bicdb list` 看一眼",
            entry.name
        ))),
    }
}

/// 语句标签（错误文案用）。
fn stmt_label(s: &Stmt) -> &'static str {
    match s {
        Stmt::Select(_) => "SELECT",
        Stmt::Insert(_) => "INSERT",
        Stmt::Update(_) => "UPDATE",
        Stmt::Delete(_) => "DELETE",
        Stmt::CreateTable(_) => "CREATE TABLE",
        Stmt::Index(_) => "CREATE INDEX",
        Stmt::Drop(_) => "DROP",
        Stmt::Transaction(_) => "事务控制",
        Stmt::CreateGraph(_) => "CREATE GRAPH",
        Stmt::Cypher(_) => "CYPHER",
        Stmt::ShowGraphs(_) => "SHOW GRAPHS",
        Stmt::VariableSet(_) => "ALTER SESSION",
        _ => "DCL",
    }
}

/// 绝对化（注册表里存绝对路径——工作区搬走也能找到）。
fn absolute(p: &str) -> Result<PathBuf, SessionError> {
    let path = Path::new(p);
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let cwd =
        std::env::current_dir().map_err(|e| SessionError::Dcl(format!("取当前目录失败：{e}")))?;
    Ok(cwd.join(path))
}

/// **路径校验**（`DCL语句设计` F1）：存在、可写、是目录、**非符号链接**；
/// **不要求是挂载点**（可以只是一个目录）。
fn check_directory(path: &Path) -> Result<(), DclExecError> {
    let meta = std::fs::symlink_metadata(path)
        .map_err(|e| DclExecError::Path(format!("{} 不存在或读不了：{e}", path.display())))?;
    if meta.file_type().is_symlink() {
        return Err(DclExecError::Path(format!(
            "{} 是符号链接——拒绝（`FileIo` 既有纪律：不跟随符号链接）",
            path.display()
        )));
    }
    if !meta.is_dir() {
        return Err(DclExecError::Path(format!(
            "{} 不是目录（文件系统可以是独立挂载点，也可以只是一个目录，但不能是普通文件）",
            path.display()
        )));
    }
    // 可写性：**真写一个探针文件**（权限位在 ACL/只读挂载下会说谎）。
    let probe = path.join(".bicdb-write-probe");
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(e) => Err(DclExecError::Path(format!(
            "{} 不可写（探针文件建不了：{e}）",
            path.display()
        ))),
    }
}

/// 文件系统的容量（字节）。**取不到就给 0**——权威是文件系统本身，
/// `fs$` 的两列只是缓存（`DCL语句设计` F1）。
fn fs_sizes(path: &Path) -> (u64, Option<u64>) {
    // 用 `statvfs` 需要 libc；这里退一步：目录项扫描给不出容量 ⇒ 留 0/None，
    // 由后续容量切片（OPS）填。**不猜**：写 0 而不是编一个数。
    let _ = path;
    (0, None)
}

/// **解析文件系统引用**（名字或槽位；F 组的对象查找）。
fn resolve_fs(gcf: &mut GlobalControlFile<'_>, r: &FsRef) -> Result<(u16, String), DclExecError> {
    match (r.slot, &r.name) {
        (Some(slot32), _) => {
            let slot = u16::try_from(slot32)
                .map_err(|_| DclExecError::Registry(format!("池槽位号超出 16 位：{slot32}")))?;
            let rec = gcf
                .fs_by_slot(slot)
                .map_err(|e| DclExecError::Registry(e.to_string()))?
                .ok_or_else(|| DclExecError::Registry(format!("池槽位 {slot} 不在池里")))?;
            Ok((slot, String::from_utf8_lossy(&rec.name).into_owned()))
        }
        (None, Some(name)) => {
            let rec = gcf
                .fs_by_name(name)
                .map_err(|e| DclExecError::Registry(e.to_string()))?
                .ok_or_else(|| {
                    DclExecError::Registry(format!(
                        "文件系统 `{}` 不在池里",
                        String::from_utf8_lossy(name)
                    ))
                })?;
            if rec.status != bicdb_storage::globalctl::FS_IN_POOL {
                return Err(DclExecError::Registry(format!(
                    "文件系统 `{}` 已移出池（槽位 {}）",
                    String::from_utf8_lossy(name),
                    rec.slot
                )));
            }
            Ok((rec.slot, String::from_utf8_lossy(&rec.name).into_owned()))
        }
        (None, None) => Err(DclExecError::Registry("文件系统引用为空".to_owned())),
    }
}

/// 未用的引用占位（保持导入完整；W/U 组落地时用）。
#[allow(dead_code)]
fn _unused(_: &WorkRef, _: &FsQuota, _: QuotaAmount) {}
