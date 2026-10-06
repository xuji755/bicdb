//! **bicdbcli 的库面**（客户端内核：缓冲、设置、输出、连接、命令分派）。
//!
//! ```text
//! buffer    当前缓冲区 + 编辑命令（LIST/DEL/APPEND/INPUT/CHANGE）
//! settings  SET/SHOW 参数（只收有落点的）
//! output    结果集版面 + SPOOL
//! conn      直连 / 经服务（控制套接字）
//! shell     一行输入 → 命令或 SQL → 执行 → 输出（`main.rs` 的用户）
//! help      HELP 文本
//! ```
//!
//! 二进制 `bicdbcli` 只是薄壳（参数解析 + 起会话）；语义都在本库，
//! 因此可被单测与集成测试直接驱动。

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod buffer;
pub mod conn;
pub mod help;
pub mod output;
pub mod settings;
pub mod shell;
