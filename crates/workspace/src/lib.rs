//! # bicdb-workspace
//!
//! WorkspaceContext、身份与路由、根目录句柄、配额与公平调度
//!
//! - 设计依据：§3 隔离契约、§4 工作区目录与资源
//! - 对应阶段：**P1**（已启动）
//! - 当前状态：**v0.3**——ID 类型、根目录名与校验、目录布局（`0700`）、
//!   四条配额、工作区上下文、**FileIO 替换接口**（`io`：句柄制 +
//!   `OsFileIo` / `MemFileIo` / `FaultInjecting`），以及**身份路由**
//!   （`identity` + `registry`：`AuthenticatedSubject` 是路由的唯一身份
//!   来源；他人的工作区报"不存在"且与"不存在"不可区分）。
//!   **监督器随后**。
//!
//! 隔离性的四条结构性事实在本 crate 的边界上成立
//! （`ISO` REQ-ISO-002/006/007/008）：
//! 1. **根目录名没有"从任意字符串构造"的公开入口**——用户输入进不了路径拼接；
//! 2. 目录创建**拒绝符号链接**，权限显式收紧为 `0700`；
//! 3. 文件层**只有打开接受路径**（不跟随符号链接、打开后按句柄复核），
//!    之后一切操作基于**不复用**的句柄（"以句柄为准，不以路径为准"）；
//! 4. 路由**没有 user_id 参数**——身份只来自认证结果，跨属主失败不可区分。

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod context;
pub mod id;
pub mod identity;
pub mod io;
pub mod layout;
pub mod quota;
pub mod registry;
pub mod root;

pub use context::WorkspaceContext;
pub use id::{workspace_ref, UserId, WorkspaceId, WORKSPACE_ID_MAX};
pub use identity::AuthenticatedSubject;
pub use layout::WorkspaceDir;
pub use quota::Quota;
pub use registry::{WorkspaceEntry, WorkspaceRegistry};
pub use root::{RootName, WorkspaceRoot};
