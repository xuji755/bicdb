//! 监督器：工作进程的入口对象（总体方案 §14/§15、§17 的 P1 清单）。
//!
//! 职责（按设计原文）：
//!
//! 1. **启动自检**（P1 验收"启动恢复"）：逐工作区校验根目录与布局、
//!    以句柄方式确认**可打开**（`ISO` REQ-ISO-008"以句柄为准"）；
//!    任一失败 → **fail closed**（整体不启动）。按工作区隔离降级
//!    （一个工作区故障不拖累他人）归 `NFR` REL-005，P11 落实，此处不做半开。
//! 2. **生成 `WorkspaceContext`**：§14 明文"由认证和监督器生成，业务不可修改"
//!    ——身份来自 [`AuthenticatedSubject`]，路由是身份的函数（REQ-ISO-002）。
//! 3. **准入与资源拒绝**（`NFR` REQ-RES-001/004）：活跃工作区数受实例额度
//!    （默认 8 / 上限 32）；每工作区任务额度（`max_task_queue`）与实例池
//!    队列**各自有界**，超限返回**可判定的拒绝**（"哪个额度、当前值、上限"——
//!    `CONV` §4.3 资源类错误的字段形态在此先行）。
//! 4. **专用维护通道**：维护任务走 [`Maintenance`] 自己的线程，
//!    与执行池分离（REQ-RES-003：不被普通请求饿死）。

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use bicdb_workspace::io::{FileIo as _, OsFileIo};
use bicdb_workspace::registry::{Routed, RoutingError, WorkspaceRegistry};
use bicdb_workspace::{AuthenticatedSubject, WorkspaceContext, WorkspaceId};

use crate::limits::{InstanceLimits, LimitsError};
use crate::maintenance::Maintenance;
use crate::numa::{NumaBinder, NumaConfig, NumaStatus};
use crate::pool::{ExecPool, PoolError};

/// 监督器配置（`Default` = P0 冻结的上限取值；NUMA 默认**关闭**）。
#[derive(Debug, Clone, Default)]
pub struct SupervisorConfig {
    /// 实例级上限（[`InstanceLimits::default`] = P0 冻结值）。
    pub limits: InstanceLimits,
    /// **NUMA 第二级绑定**（默认关闭；详设 `doc/numa绑定设计_v0.1.md`）。
    /// 启用后：工作区任务的执行线程在运行前被放进其节点的 cpuset——
    /// **本地性是优化不是正确性**：任何失败都只记录、不改变行为。
    pub numa: NumaConfig,
}

/// 启动失败（fail closed：不带着校验不过的状态启动）。
#[derive(Debug)]
pub enum BootError {
    /// 上限配置不合法。
    Limits(LimitsError),
    /// 某工作区的根目录或布局校验失败（含符号链接篡改）。
    WorkspaceNotOpenable {
        /// 哪个工作区。
        workspace: WorkspaceId,
        /// 底层错误。
        source: io::Error,
    },
    /// 保留工作区 `public` 的根目录校验失败。
    PublicNotOpenable {
        /// 底层错误。
        source: io::Error,
    },
}

/// 激活失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivateError {
    /// 该主体名下不存在这个名字（与他人工作区**不可区分**，`ISO` REQ-ISO-006）。
    NotFound,
    /// 活跃工作区数已达实例额度（资源拒绝；"调额 / 等待后重试"）。
    ResourceLimit {
        /// 触顶的额度名。
        quota: &'static str,
        /// 当前值。
        current: usize,
        /// 上限。
        limit: usize,
    },
    /// `public` 不参与活跃工作区准入——它是**共享只读**的常驻工作区
    /// （`ISO` REQ-ISO-004），不占私有工作区额度。
    PublicNotActivated,
}

/// 任务提交被拒（P1 的资源拒绝面；`CONV` §4.3 资源类的字段形态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskRejected {
    /// 该工作区未激活（没有它的 `WorkspaceContext` 可用——上下文由监督器生成）。
    NotActive(WorkspaceId),
    /// 该工作区队列额度已满。
    WorkspaceQueueFull {
        /// 哪个工作区。
        workspace: WorkspaceId,
        /// 上限（`max_task_queue`）。
        limit: usize,
    },
    /// 实例执行池队列已满。
    InstanceQueueFull {
        /// 实例池容量。
        capacity: usize,
    },
    /// 停机中。
    Closed,
}

/// 一个已激活的工作区。
#[derive(Debug)]
struct ActiveWorkspace {
    context: WorkspaceContext,
    /// 该工作区的在途 + 排队任务数（含正在执行的任务）。
    queued: Arc<AtomicUsize>,
}

/// 归还队列额度的守卫：任务执行完（或被拒后闭包被丢弃）时递减。
struct QueueSlot(Arc<AtomicUsize>);

impl Drop for QueueSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// 监督器。
#[derive(Debug)]
pub struct Supervisor {
    limits: InstanceLimits,
    registry: WorkspaceRegistry,
    active: HashMap<WorkspaceId, ActiveWorkspace>,
    pool: ExecPool,
    maintenance: Maintenance,
    /// NUMA 绑定器（`None` = 未启用或降级；原因见 `numa_reason`）。
    numa: Option<Arc<NumaBinder>>,
    /// 降级原因（启用但准备失败时记录；诊断用）。
    numa_reason: Option<String>,
}

impl Supervisor {
    /// 启动：校验上限 → 逐工作区自检（布局 + 以句柄打开）→ 建池与维护线程。
    pub fn boot(config: SupervisorConfig, registry: WorkspaceRegistry) -> Result<Self, BootError> {
        config.limits.validate().map_err(BootError::Limits)?;

        let io = OsFileIo::new();
        for entry in registry.entries() {
            let root = entry.root();
            root.create_layout()
                .map_err(|source| BootError::WorkspaceNotOpenable {
                    workspace: entry.id(),
                    source,
                })?;
            let handle =
                io.open_dir(root.path())
                    .map_err(|source| BootError::WorkspaceNotOpenable {
                        workspace: entry.id(),
                        source,
                    })?;
            io.close(handle)
                .map_err(|source| BootError::WorkspaceNotOpenable {
                    workspace: entry.id(),
                    source,
                })?;
        }
        if let Some(root) = registry.public_root() {
            root.create_layout()
                .map_err(|source| BootError::PublicNotOpenable { source })?;
            let handle = io
                .open_dir(root.path())
                .map_err(|source| BootError::PublicNotOpenable { source })?;
            io.close(handle)
                .map_err(|source| BootError::PublicNotOpenable { source })?;
        }

        // 实例池容量：至少覆盖"全部活跃工作区各自排满"的合计（推导值，
        // P0 未冻结实例队列总量；调整走变更控制）。
        let pool_capacity = config
            .limits
            .max_task_queue_per_workspace
            .saturating_mul(config.limits.max_active_workspaces)
            .max(1);
        let pool = ExecPool::new(config.limits.execution_threads, pool_capacity);
        let maintenance = Maintenance::start(64);

        // NUMA：启用时尽力准备；**失败只降级、不 fail-closed**（它是优化）。
        let (numa, numa_reason) = if config.numa.enabled {
            match NumaBinder::start(&config.numa) {
                Ok(binder) => (Some(Arc::new(binder)), None),
                Err(e) => (None, Some(e.to_string())),
            }
        } else {
            (None, None)
        };

        Ok(Self {
            limits: config.limits,
            registry,
            active: HashMap::new(),
            pool,
            maintenance,
            numa,
            numa_reason,
        })
    }

    /// NUMA 绑定状态（诊断；详设 §9）。
    #[must_use]
    pub fn numa_status(&self) -> NumaStatus {
        match (&self.numa, &self.numa_reason) {
            (Some(binder), _) => binder.status(),
            (None, Some(reason)) => NumaStatus::Degraded {
                reason: reason.clone(),
            },
            (None, None) => NumaStatus::Disabled,
        }
    }

    /// 激活一个工作区并生成它的上下文。
    ///
    /// - 路由只认**已认证主体**（REQ-ISO-002）；
    /// - 重复激活是幂等的（返回既有上下文）；
    /// - 活跃数触顶 → [`ActivateError::ResourceLimit`]；
    /// - `public` → [`ActivateError::PublicNotActivated`]（共享只读，不占额度）。
    pub fn activate(
        &mut self,
        subject: &AuthenticatedSubject,
        name: Option<&str>,
    ) -> Result<&WorkspaceContext, ActivateError> {
        let (workspace, root, quota) = match self
            .registry
            .route(subject, name)
            .map_err(|RoutingError::NotFound| ActivateError::NotFound)?
        {
            Routed::Owned(entry) => (entry.id(), entry.root().clone(), entry.quota()),
            Routed::Public(_) => return Err(ActivateError::PublicNotActivated),
        };

        if !self.active.contains_key(&workspace) {
            if self.active.len() >= self.limits.max_active_workspaces {
                return Err(ActivateError::ResourceLimit {
                    quota: "max_active_workspaces",
                    current: self.active.len(),
                    limit: self.limits.max_active_workspaces,
                });
            }
            let context = WorkspaceContext::new(subject.user(), workspace, root, quota);
            self.active.insert(
                workspace,
                ActiveWorkspace {
                    context,
                    queued: Arc::new(AtomicUsize::new(0)),
                },
            );
        }
        Ok(&self.active.get(&workspace).expect("刚插入或已存在").context)
    }

    /// 释放一个工作区（空闲进程可退出；额度归还）。
    pub fn deactivate(&mut self, workspace: WorkspaceId) -> bool {
        self.active.remove(&workspace).is_some()
    }

    /// 提交任务到执行池（按工作区额度准入 + 两层队列各自有界）。
    pub fn submit(
        &self,
        context: &WorkspaceContext,
        task: impl FnOnce() + Send + 'static,
    ) -> Result<(), TaskRejected> {
        let workspace = context.workspace();
        let Some(active) = self.active.get(&workspace) else {
            return Err(TaskRejected::NotActive(workspace));
        };

        let prev = active.queued.fetch_add(1, Ordering::AcqRel) + 1;
        if prev > self.limits.max_task_queue_per_workspace {
            active.queued.fetch_sub(1, Ordering::AcqRel);
            return Err(TaskRejected::WorkspaceQueueFull {
                workspace,
                limit: self.limits.max_task_queue_per_workspace,
            });
        }
        let counter = Arc::clone(&active.queued);
        // 闭包被池接收后持有守卫（Drop 归还额度）。**被拒时闭包不会执行**——
        // 守卫从未构造，必须在此**显式归还**刚加的额度，否则每次
        // `InstanceQueueFull` 都让该工作区的在途计数永久 +1，最终假性"队列满"。
        let queued = Arc::clone(&active.queued);
        // NUMA 绑定点（详设 §5 阶段 A）：运行任务前把**当前线程**放进该
        // 工作区节点的 cpuset；失败只记诊断，**任务照常执行**。
        let numa = self.numa.clone();
        match self.pool.submit(move || {
            let _slot = QueueSlot(counter);
            if let Some(binder) = &numa {
                binder.bind_current_thread(workspace);
            }
            task();
        }) {
            Ok(()) => Ok(()),
            Err(PoolError::QueueFull) => {
                queued.fetch_sub(1, Ordering::AcqRel);
                Err(TaskRejected::InstanceQueueFull {
                    capacity: self.pool.capacity(),
                })
            }
            Err(PoolError::Closed) => {
                queued.fetch_sub(1, Ordering::AcqRel);
                Err(TaskRejected::Closed)
            }
        }
    }

    /// 活跃工作区数。
    #[must_use]
    pub fn active_count(&self) -> usize {
        self.active.len()
    }

    /// 上限（诊断/管理面）。
    #[must_use]
    pub fn limits(&self) -> &InstanceLimits {
        &self.limits
    }

    /// 执行池（诊断）。
    #[must_use]
    pub fn pool(&self) -> &ExecPool {
        &self.pool
    }

    /// 维护通道（专用线程；REQ-RES-003）。
    #[must_use]
    pub fn maintenance(&self) -> &Maintenance {
        &self.maintenance
    }

    /// 该工作区当前在途 + 排队任务数（诊断）。
    #[must_use]
    pub fn in_flight(&self, workspace: WorkspaceId) -> Option<usize> {
        self.active
            .get(&workspace)
            .map(|a| a.queued.load(Ordering::SeqCst))
    }

    /// 停机：关闭执行池与维护线程（队列中已有任务先跑完）。
    pub fn shutdown(self) {
        let Self {
            pool, maintenance, ..
        } = self;
        pool.shutdown();
        maintenance.shutdown();
    }
}
