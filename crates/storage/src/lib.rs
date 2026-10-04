//! # bicdb-storage
//!
//! 页格式、槽位堆表、跨页行片段、BufferPool、PageGuard
//!
//! - 设计依据：§7 Oracle风格存储与跨页记录（存储架构 §5.3–§5.9）
//! - 对应阶段：**P2**（已启动；本片 = `页格式基础`）
//! - 当前状态：**v0.7**——[`page`]（页格式与两层完整性检出）、[`rowid`]、
//!   [`row`]（行格式与片段链）、[`heap`]（堆表操作与内存堆表）、
//!   [`fragment`]（跨页行片段链）、[`pagefile`]（页文件定址读写）、
//!   [`bitmap`]（位图页与**区分配图 LMT**：区 = 128 KB、位图区 = 8 页、
//!   容量换算与 `own_index` 自校验）、[`controlfile`]（P3：**工作区控制文件**
//!   ——20 页 × 16 KiB 字节布局、双副本、单区间更新协议与崩溃自愈）。
//!   **段与区管理（§4）、BufferPool 随后。**
//!
//! 三条纪律：
//! 1. **字节序定死小端**（REQ-PRT-003）——磁盘格式不随主机变化；
//! 2. **能推导的不存**（§5.2）：`free_start = 固定头末尾 + slot_count×2`、
//!    可用空间 = 两指针之差，页头不存冗余量；
//! 3. **每次修改必须 `seal`**（写页尾副本 → 重算校验和）——头、尾、
//!    校验和三者在一次落盘前一致。

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod bitmap;
pub mod controlfile;
pub mod fragment;
pub mod heap;
pub mod page;
pub mod pagefile;
pub mod row;
pub mod rowid;
