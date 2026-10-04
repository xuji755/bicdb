//! # bicdb-workspace
//!
//! WorkspaceContext、身份与路由、根目录句柄、配额与公平调度
//!
//! - 设计依据：§3 隔离契约、§4 工作区目录与资源
//! - 对应阶段：**P1**（已启动）
//! - 当前状态：**v0.1 基础层已实现**——ID 类型（48 位工作区 / UUID 主体）、
//!   根目录名（服务端生成 + 规范形态校验）、目录布局（十个固定子目录，`0700`）、
//!   四条配额、工作区上下文。**身份路由、FileIO 替换接口与监督器随后**。
//!
//! 隔离性的两条结构性事实在本 crate 的边界上成立（`ISO` REQ-ISO-008）：
//! 1. **根目录名没有"从任意字符串构造"的公开入口**——用户输入进不了路径拼接；
//! 2. 目录创建**拒绝符号链接**，权限显式收紧为 `0700`。
//!    "打开不跟随符号链接、以句柄为准"的 TOCTOU 硬化在 P2 文件打开层落实。

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod context;
pub mod id;
pub mod layout;
pub mod quota;
pub mod root;

pub use context::WorkspaceContext;
pub use id::{UserId, WorkspaceId, WORKSPACE_ID_MAX};
pub use layout::WorkspaceDir;
pub use quota::Quota;
pub use root::{RootName, WorkspaceRoot};
