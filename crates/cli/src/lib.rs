//! **bicdb 命令行库面**（`bicdb` 二进制与端到端测试共用）。
//!
//! ```text
//! boot：建区 / 打开 / 关闭（文件面 + 控制文件 + 日志 + 池 + 引擎）
//! main：子命令与 REPL（只用本库的 [`boot`]，不重复实现装配）
//! clientauth：客户端侧的认证凭据（`-U` + 口令来源）
//! fixed：固定表的内容源（`file$` ← 控制文件的内存映像）
//! ```

#![forbid(unsafe_code)]

pub mod boot;
pub mod clientauth;
pub mod config;
pub mod fixed;
pub mod home;
pub mod lock;
pub mod proto;
pub mod provision;
pub mod service;
