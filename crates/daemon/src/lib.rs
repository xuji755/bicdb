//! # bicdb-daemon
//!
//! 工作进程入口、有界执行池、维护线程、监督器
//!
//! - 设计依据：§15 多线程与Agent访问协议（P0 冻结第 8 项的额度取值）
//! - 对应阶段：**P1**（已启动）
//! - 当前状态：**v0.2**——[`supervisor`]（启动自检 fail-closed、上下文生成、
//!   活跃工作区准入与两层资源拒绝）、[`pool`]（有界执行池：线程数固定、
//!   队列有界、满即拒绝）、[`maintenance`]（**专用维护线程**，REQ-RES-003
//!   "不被普通请求饿死"）、[`limits`]（P0 冻结额度与校验）、
//!   （v0.2）[`numa`]（**NUMA 第二级绑定**：拓扑/cgroup 探测、cpuset 组
//!   准备、线程绑定落点——零 unsafe 的文件路径；详设
//!   `doc/numa绑定设计_v0.1.md`；本地性是优化不是正确性）；
//!   （v0.3）**重绑定与线程创建时绑定**（详设 §7 Rebinding + §5 阶段 B：
//!   `NumaBinder::{rebind, bind_to_node}`）——`NumaBinder::rebind`
//!   按需准备新节点组、更新映射、**世代号自增**失效全线程的绑定缓存
//!   （缓存键 = 绑定器实例号 + 节点 + 世代）；池侧 Draining 原语在
//!   `bicdb-storage`（`drain_partition`）。
//!
//! 三条结构事实：
//! 1. **身份来自认证结果**——激活只收 [`bicdb_workspace::AuthenticatedSubject`]，
//!    路由到该主体自己的工作区（REQ-ISO-002）；
//! 2. **上下文由监督器生成**（§14），业务不可修改；
//! 3. **维护与执行分离**——维护任务走自己的线程，池打满不影响它（REQ-RES-003）。

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod limits;
pub mod maintenance;
pub mod numa;
pub mod pool;
pub mod supervisor;

pub use limits::InstanceLimits;
pub use maintenance::Maintenance;
pub use numa::{
    BindMode, BindOutcome, CgroupVersion, NumaBinder, NumaConfig, NumaError, NumaStatus, Topology,
};
pub use pool::{ExecPool, PoolError};
pub use supervisor::{ActivateError, BootError, Supervisor, SupervisorConfig, TaskRejected};
