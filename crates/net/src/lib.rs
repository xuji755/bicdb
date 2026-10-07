//! # bicdb-net
//!
//! **本机客户端协议**（`docs/客户端协议_v0.1.md` 的实现）：Rust 与 Python 驱动
//! 实现同一份规格，跑同一条线。
//!
//! ```text
//! frame     长度前缀 + 文本载荷（读满纪律：半帧即错，不把后续帧读串）
//! value     线上的值模型（NULL/数值文本/布尔/**十六进制字节串**——无损）
//! message   动词与载荷编解码（HELLO/AUTH/STATUS/SQL/DESCRIBE/SHUTDOWN）
//! client    客户端连接（握手版本核对 + trace）
//! discover  客户端侧寻址（找到控制套接字；只读 db_root 与 socket 两项）
//! ```
//!
//! **边界（记档）**：设计里的 `bicdb-net` 还要覆盖**版本化请求协议的全量**
//! （封套、幂等与 CAS、ACK/对账、游标分页、TCP 承载）——那是独立切片；
//! 本 crate 当前实现的是**本机（Unix 套接字）客户端协议**：驱动、`bicdbcli`、
//! `bicdb start/stop` 三者共用，够 V1.0 的"本机多进程访问"用。
//! 协议版本随**不兼容变更** +1（见 [`WIRE_VERSION`]）。

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod client;
pub mod discover;
pub mod frame;
pub mod message;
pub mod value;

pub use client::{call_once, Client, ClientError};
pub use discover::{socket_for, DiscoverError};
pub use frame::{
    read_frame, read_frame_bytes, write_frame, write_frame_bytes, FrameError, MAX_FRAME,
};
pub use message::{AuthOk, AuthRequest, Column, Hello, SqlRequest, Statement, WIRE_VERSION};
pub use value::Value;
