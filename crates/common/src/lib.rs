//! # bicdb-common
//!
//! 统一错误码、LSN/SCN 类型、RAII 与页校验和基础工具
//!
//! - 设计依据：§2 技术基线、§8 事务契约
//! - 对应阶段：**P2**（已启动；本片 = `错误码 + 两个单调量 + 页校验和`）
//! - 当前状态：**v0.1**——[`error`]（`BIC-<五位数字>`、九个 Oracle 对齐码、
//!   四类判定表做成 `advice()`）、[`seq`]（`Lsn` 与 `CommitSeq`：**两种类型，
//!   混用无法通过编译**）、[`checksum`]（CRC32C 增量实现 + 16 KiB 页校验和）。
//!   **RAII 与其余基础工具随后。**
//!
//! 三条纪律：
//! 1. **错误码只增不改、不复用**（`CONV` §4.1）——九个冻结值有测试钉住；
//! 2. **LSN 与提交序号不得混用**（`CONV` §3）——类型层面禁止；
//! 3. **页校验和覆盖全页、`checksum` 字段置零后计算**（存储架构 §5.3）。

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod checksum;
pub mod error;
pub mod latch;
pub mod seq;

pub use checksum::{
    crc32c, page_checksum, set_page_checksum, verify_page_checksum, Crc32c, PAGE_SIZE,
};
pub use error::{BicCode, EngineError, RetryAdvice, TxnStatus};
pub use seq::{CommitSeq, Lsn, SEQ_MAX};
