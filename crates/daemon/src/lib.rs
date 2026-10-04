//! # bicdb-daemon
//!
//! 工作进程入口、有界执行池、维护线程、监督器
//!
//! - 设计依据：§15 多线程与Agent访问协议（P0 冻结第 8 项的额度取值）
//! - 对应阶段：**P1**（已启动）
//! - 当前状态：**v0.1**——[`supervisor`]（启动自检 fail-closed、上下文生成、
//!   活跃工作区准入与两层资源拒绝）、[`pool`]（有界执行池：线程数固定、
//!   队列有界、满即拒绝）、[`maintenance`]（**专用维护线程**，REQ-RES-003
//!   "不被普通请求饿死"）、[`limits`]（P0 冻结额度与校验）。
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
pub mod pool;
pub mod supervisor;

pub use limits::InstanceLimits;
pub use maintenance::Maintenance;
pub use pool::{ExecPool, PoolError};
pub use supervisor::{ActivateError, BootError, Supervisor, SupervisorConfig, TaskRejected};
