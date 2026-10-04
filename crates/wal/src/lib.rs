//! # bicdb-wal
//!
//! WAL 记录、重做与撤销、检查点、恢复重入
//!
//! - 设计依据：§8 崩溃恢复契约（存储架构 §11.1–§11.5，均已定案）
//! - 对应阶段：**P3**（已启动）
//! - 当前状态：**v0.4**——[`record`]（20B 记录头 / 块段 / 主段 / rdba 5B）、
//!   [`logpage`]（512B redo 页 + 12B 分片头 + 跨页重组 + 截断丢弃）、
//!   [`buffer`]（latch 串行化追加 + **组提交**刷盘）、[`file`]
//!   （redo 文件的落盘映射与扫描：LSN ↔ 物理偏移互为校验、
//!   **记录不得跨文件**、末尾残缺整条丢弃）、[`group`]
//!   （**日志组与切换**：多组轮换 / 序列号推进 / 切换记录随流自描述 /
//!   控制文件发布 / 检查点降级与归档轴的发布口；成员镜像随后）。
//!   **检查点、恢复（分析/重做/撤销）随后。**
//!
//! 三条纪律（§11.5）：
//! 1. **只存 after-image**——前像在 undo 里；
//! 2. **能推导的不存**——主段长度、其余分片位置、块数都由结构给出；
//! 3. **`op` 只是标签**——apply 路径唯一（块引用定位 → 页内字节写入，
//!    幂等性由 `page_lsn` 判定）。

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod buffer;
pub mod file;
pub mod group;
pub mod logpage;
pub mod record;

pub use buffer::{decode_sink_pages, LogBuffer, LogSink, VecLogSink, WalError};
pub use file::{scan_log, FileLogSink, LogFileError, ScanResult};
pub use group::{
    member_file_name, GroupError, GroupSpec, GroupWriter, SwitchBlocked, MAX_RECORD_FOOTPRINT,
};
pub use logpage::{
    decode_records, plan_fragments, simulate_append, write_record, Fragment, LogPage, LogPageError,
    TailState, FRAGMENT_HEADER_LEN, LOG_PAGE_SIZE,
};
pub use record::{BlockRef, Change, Rdba, RecordError, RecordOp, RedoRecord, RECORD_HEADER_LEN};
