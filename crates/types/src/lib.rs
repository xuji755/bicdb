//! # bicdb-types
//!
//! Oracle 兼容标量、JSON 文档表示、VECTOR 类型
//!
//! - 设计依据：§5 类型兼容、§6 JSON、§12 向量（存储架构 §6.5/§6.6）
//! - 对应阶段：**P2、P5**（已启动）
//! - 当前状态：**v0.2**——[`number`]（base-100 保序编码）、[`datetime`]
//!   （`DATE` 7B / `TIMESTAMP` 7B·11B，字节序即时间序）、[`boolean`]（1B）、
//!   [`uuid`]（16B 大端）。**CHAR/VARCHAR2 的填充与偏移数组随行格式落地；
//!   JSON 与 VECTOR 随后。**
//!
//! 一条纪律：**`INTEGER`/`FLOAT32`/`FLOAT64` 与 `NUMBER` 共用同一编码**
//! （§6.5）——整数快路径只允许存在于计算路径，不得改变磁盘编码。

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod boolean;
pub mod datetime;
pub mod number;
pub mod uuid;

pub use boolean::{decode as decode_boolean, encode as encode_boolean, BOOLEAN_LEN};
pub use datetime::{Date, DateTimeError, Precision, Timestamp, DATE_LEN};
pub use number::{Number, NumberError, MAX_DIGIT_BYTES};
pub use uuid::{Uuid, UuidParseError, UUID_LEN};
