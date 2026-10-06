//! # bicdb-wal
//!
//! WAL 记录、重做与撤销、检查点、恢复重入
//!
//! - 设计依据：§8 崩溃恢复契约（存储架构 §11.1–§11.5，均已定案）
//! - 对应阶段：**P3**（已启动）
//! - 当前状态：**v0.15**——[`record`]（20B 记录头 / 块段 / 主段 / rdba 5B）、
//!   [`logpage`]（512B redo 页 + 12B 分片头 + 跨页重组 + 截断丢弃）、
//!   [`buffer`]（latch 串行化追加 + **组提交**刷盘 + **环形页池**：
//!   容量判据/1/3 触发/页复用——§11.5.5）、[`file`]
//!   （redo 文件的落盘映射与扫描：LSN ↔ 物理偏移互为校验、
//!   **记录不得跨文件**、末尾残缺整条丢弃）、[`group`]
//!   （**日志组与切换**：多组轮换 / 序列号推进 / 切换记录随流自描述 /
//!   控制文件发布 / 检查点降级与归档轴的发布口；成员镜像随后）。
//!   [`apply`]（**重做应用**：块引用定位 → 页内字节写入，`page_lsn` 幂等，
//!   跨块原子性由幂等重放保证）。
//!   [`recovery`]（**重做阶段**：在线组按序列号升序扫描、从检查点 LSN
//!   起逐条 apply、报告日志末端与跳过统计）。
//!   [`analysis`]（**分析阶段**：判定流中每个事务的结局——提交（带序号）/
//!   回滚完成 / **输家**；`highest_commit_seq` 作恢复后提交序号起点；
//!   系统记录不参与）。
//!   [`undo_phase`]（**撤销阶段**：输家逐槽回滚——整链逆操作、**补偿生成
//!   redo 并刷盘后才写页**（WAL 次序）、0x31 回滚完成标记、槽释放；
//!   五类补偿幂等 ⇒ 崩溃中断重走整链即可）。
//!   [`checkpoint`]（**检查点**：CKPT 角色只发布位置——检查点记录入流并
//!   刷盘 + CF 进度与组降级同区间发布；低水位来自缓冲池脏链；单调性守卫；
//!   完全检查点先按序写回全部脏页）。
//!   （续）**三阶段恢复驱动**——`recover`：分析 → 重做 → 补标记 + 输家回滚，
//!   次序钉死、可重跑（三阶段各自幂等）。
//!   （续）**PITR 目标点**——`pitr_stop` + `ReplayBound` + `recover_to_target`：
//!   提交序号为权威、重做/分析同界、补偿在"更旧"页上为空操作。
//!   （续）**成员镜像与 `STALE`**（CF 保留字节位、扇出降级、前缀重建）；
//!   **墙钟目标点**（采样环 + 线性内插 + 窗口边界报错）。
//!   （续，v0.15，P3 审核修复）**组复用与成员降级的四处收口**——复用组
//!   **先清空**（旧周期残页不再被判"中部坏页"）；`flush` **只写健康成员**、
//!   组已写页数取**未失败成员最大值**（成员 0 一次瞬时写失败不再令 WAL
//!   永久写不出去）；成员扫描**坏成员跳过**（健康镜像顶替）、组起点取自
//!   选中的成员；`rebuild_member` 复制后截断目标尾部；（续）刷盘摘页时
//!   **冻结页分配位置**（I/O 窗口内的追加不再复用已刷出页的起点/LSN）。
//!   **P4 多写者随后。**
//!
//! 三条纪律（§11.5）：
//! 1. **只存 after-image**——前像在 undo 里；
//! 2. **能推导的不存**——主段长度、其余分片位置、块数都由结构给出；
//! 3. **`op` 只是标签**——apply 路径唯一（块引用定位 → 页内字节写入，
//!    幂等性由 `page_lsn` 判定）。

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod analysis;
pub mod apply;
pub mod buffer;
pub mod checkpoint;
pub mod file;
pub mod group;
pub mod logpage;
pub mod record;
pub mod recovery;
pub mod thread;
pub mod undo_phase;

pub use apply::{apply_record, ApplyError, ApplyReport, BlockResolver};
pub use buffer::{decode_sink_pages, LogBuffer, LogSink, VecLogSink, WalError};
pub use file::{scan_log, FileLogSink, LogFileError, ScanResult};
pub use group::{
    member_file_name, online_groups, GroupError, GroupSpec, GroupWriter, OnlineGroup,
    SwitchBlocked, MAX_RECORD_FOOTPRINT,
};
pub use logpage::{
    decode_records, plan_fragments, simulate_append, write_record, Fragment, LogPage, LogPageError,
    TailState, FRAGMENT_HEADER_LEN, LOG_PAGE_SIZE,
};
pub use record::{BlockRef, Change, Rdba, RecordError, RecordOp, RedoRecord, RECORD_HEADER_LEN};
pub use recovery::{redo_from, RecoveryError, RedoReport};
