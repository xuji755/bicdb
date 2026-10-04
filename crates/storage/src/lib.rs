//! # bicdb-storage
//!
//! 页格式、槽位堆表、跨页行片段、BufferPool、PageGuard
//!
//! - 设计依据：§7 Oracle风格存储与跨页记录（存储架构 §5.3–§5.9）
//! - 对应阶段：**P2**（已启动；本片 = `页格式基础`）
//! - 当前状态：**v0.3**——[`page`]（页格式与两层完整性检出）、[`rowid`]、
//!   [`row`]（行格式与片段链）、[`heap`]（堆表页操作与内存堆表：
//!   插入/读取/删除/defrag/PCTFREE/表选项策略）。
//!   **段与区分配、BufferPool、页分配位图随后。**
//!
//! 三条纪律：
//! 1. **字节序定死小端**（REQ-PRT-003）——磁盘格式不随主机变化；
//! 2. **能推导的不存**（§5.2）：`free_start = 固定头末尾 + slot_count×2`、
//!    可用空间 = 两指针之差，页头不存冗余量；
//! 3. **每次修改必须 `seal`**（写页尾副本 → 重算校验和）——头、尾、
//!    校验和三者在一次落盘前一致。

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod heap;
pub mod page;
pub mod row;
pub mod rowid;
