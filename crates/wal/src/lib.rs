//! # bicdb-wal
//!
//! WAL 记录、重做与撤销、检查点、恢复重入
//!
//! - 设计依据：§8 崩溃恢复契约（存储架构 §11.1–§11.5，均已定案）
//! - 对应阶段：**P3**（已启动；本片 = 重做记录与 redo 页的字节格式）
//! - 当前状态：**v0.1**——[`record`]（20B 记录头 / 块段 / 主段 / rdba 5B）、
//!   [`logpage`]（512B redo 页 + 12B 分片头 + 跨页重组 + 截断丢弃）。
//!   **日志缓冲与追加（latch 串行化）、刷盘、恢复、检查点随后。**
//!
//! 三条纪律（§11.5）：
//! 1. **只存 after-image**——前像在 undo 里；
//! 2. **能推导的不存**——主段长度、其余分片位置、块数都由结构给出；
//! 3. **`op` 只是标签**——apply 路径唯一（块引用定位 → 页内字节写入，
//!    幂等性由 `page_lsn` 判定）。

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod logpage;
pub mod record;

pub use logpage::{
    decode_records, write_record, Fragment, LogPage, LogPageError, TailState, FRAGMENT_HEADER_LEN,
    LOG_PAGE_SIZE,
};
pub use record::{BlockRef, Change, Rdba, RecordError, RecordOp, RedoRecord, RECORD_HEADER_LEN};
