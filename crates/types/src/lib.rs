//! # bicdb-types
//!
//! Oracle 兼容标量、JSON 文档表示、VECTOR 类型
//!
//! - 设计依据：§5 类型兼容、§6 JSON、§12 向量（存储架构 §6.5/§6.6）
//! - 对应阶段：**P2、P5**（已启动；本片 = `NUMBER` 物理编码）
//! - 当前状态：**v0.1**——[`number`]（变长 base-100 科学计数法：
//!   字节序 = 数值序、规范形式唯一编码、与 Oracle `dump()` 已知向量对齐）。
//!   **DATE/TIMESTAMP、CHAR/VARCHAR2、BOOLEAN/UUID、JSON、VECTOR 随后。**
//!
//! 一条纪律：**`INTEGER`/`FLOAT32`/`FLOAT64` 与 `NUMBER` 共用同一编码**
//! （§6.5）——整数快路径只允许存在于计算路径，不得改变磁盘编码；
//! 因此"保序"一次实现、三处受益。

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod number;

pub use number::{Number, NumberError, MAX_DIGIT_BYTES};
