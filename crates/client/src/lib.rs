//! # bicdb-client —— bicdb 的 Rust 驱动
//!
//! `docs/客户端协议_v0.1.md` 的 Rust 实现（Python 驱动实现**同一份规格**，见
//! `drivers/python/bicdb`）。
//!
//! ```no_run
//! use bicdb_client::{Connection, Value};
//!
//! # fn main() -> Result<(), bicdb_client::Error> {
//! // 按参数文件/根区目录寻址（协议 §1）；服务没在跑就报"连不上"。
//! let mut conn = Connection::connect("/data/bicdb")?;
//! println!("引擎 {}，实例 {}", conn.server_version(), conn.instance());
//!
//! conn.execute("CREATE TABLE t (id NUMBER NOT NULL, name VARCHAR2(32))", &[])?;
//! conn.execute(
//!     "INSERT INTO t VALUES (:id, :name)",
//!     &[("id", Value::from(1_i64)), ("name", Value::from("alpha"))],
//! )?;
//!
//! let rs = conn.query("SELECT id, name FROM t WHERE id = :id", &[("id", 1_i64.into())])?;
//! for row in rs.iter() {
//!     let id = row.i64(0)?;          // 数值列
//!     let name = row.str(1)?;        // 文本列（非 UTF-8 会具名报错，不替换成 �）
//!     println!("{id} {name}");
//! }
//! # Ok(())
//! # }
//! ```
//!
//! ## 边界（记档）
//!
//! | 本驱动有 | 本驱动没有（后续切片） |
//! | --- | --- |
//! | 本机（Unix 套接字）连接、参数化执行、结果集、`describe`、服务自述 | TCP/多机、认证与授权 |
//! | 一条连接 = 一个会话（事务跨语句） | 连接池、异步/流式游标、流水线 |
//! | 协议版本核对（不等即拒连） | 协议全量（封套/幂等/CAS/ACK 对账） |
//!
//! **不重试、不隐藏错误**：语句错原样透出（[`Error::Server`]），
//! 连接层的字节流一旦不可信（半帧/版本不符）就报错并**不再复用**这条连接。

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod conn;
pub mod error;
pub mod value;

pub use conn::{locate_socket, Column, ColumnKind, Connection, Outcome, ResultSet, Row};
pub use error::Error;
pub use value::Value;

/// 驱动名（`STATUS`/日志里标识自己）。
pub const DRIVER_NAME: &str = "bicdb-client-rust";

/// 驱动版本（跟工作区版本走）。
pub const DRIVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 本驱动实现的协议版本。
pub const WIRE_VERSION: u8 = bicdb_net::WIRE_VERSION;
