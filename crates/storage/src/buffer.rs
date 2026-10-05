//! DB Cache（缓冲池；§5.10 完整设计，结构照 Oracle 8.1 `kcbwds`/`kcbbh`）。
//!
//! ```text
//!         键（工作区标识, RDBA）
//!               │ 哈希
//!               ▼
//!    [ 哈希桶 i ] ──▶ 缓冲块 ──▶ …            ← 哈希链（找块；桶内是帧号链）
//!    [ 热段 ] ◀──▶ [ 冷段 ]                   ← LRU-MAIN（COLD_HD 分界）
//!    [ AUX ]                                  ← 可重用候选（前台优先扫它）
//!    [ 写列表 ]  头 ──▶ … ──▶ 尾              ← LRU-W = 检查点队列（首次变脏 LSN 序）
//! ```
//!
//! # 三条规则（§5.10）
//!
//! | 机制 | 落点 |
//! | --- | --- |
//! | **找块** | 桶 = **`DBA mod 桶数`**（桶数取质数，默认 ≈ 容量/4——Oracle `_DB_BLOCK_HASH_BUCKETS` 口径）；未命中 → 找空闲帧 → 读页 → 身份核对 → 挂桶 |
//! | **腾块** | 前台先扫 **AUX**、再扫**冷段尾**（跳过钉住；脏块已在写列表 ⇒ 跳过）；扫不到 ⇒ **Make Free**（内联 DBWR 批处理：写列表头按序写） |
//! | **写回** | 写列表**头**（= 最老首次变脏）逐块写；**WAL 规则 2**（redo 未持久化则推迟/催刷）；写完 → 清脏 → **入 AUX** |
//!
//! # touch count：三秒规则 + 老化减半（Note 104937.1）
//!
//! 命中且**距上次递增 ≥ 3 秒**（`dbagingtouchtime`）才 `+1`；计数达
//! **热判据** ⇒ 提升到**热段头**、计数置**驻留值**（`_STAY_COUNT` 语义）；
//! 热段超上限（`HBMAX`）⇒ 热段尾**退回冷段头**、计数置**冷却值**
//! （`_COOL_COUNT` 语义）。
//!
//! **淘汰不立即发生**：前台扫冷段尾遇到**计数 > 冷却值**的候选时，把它
//! **减半（aging）后继续扫**——"计数够高的块即使位于列表尾也不被重用"，
//! 减半让它再循环一轮；命中会把计数顶回去，真热块因此永不到达淘汰线。
//! 新读入/重用的帧落在**冷段头**（不是热段），计数 = 冷却值。
//!
//! # 单线程形态（v2 起）
//!
//! 「Make Free → DBWR 批处理」由池**内联执行**（写列表头按序、WAL 规则 2、
//! 写完入 AUX）——与多线程形态**同一协议**，只是不并行。`fb_wait`/`wal_syncs`
//! 等计数口径与 X$KCBWDS 对齐，便于日后接真线程与诊断。
//!
//! # 分区
//!
//! **O2（2026-10-05，§5.10）**：帧是**per-frame 状态对象**——页内容由每帧
//! 自带的 `RwLock` 保护（读共享/写独占）、`pins` 是原子计数，**卫兵不持
//! 分区闩锁**（"一次一个卫兵"纪律退役；同线程可同时持多个卫兵、持卫兵期间
//! 照常调池）。分区闩锁只护**结构面**（元数据/链/桶/写列表/统计）。
//! **闩锁次序**：结构闩锁 →（仅 `try_write`，不阻塞）内容锁；内容锁 →
//! 结构闩锁（`mark_dirty`/写回收尾）；pin 的"定位 + pin 计数"在结构闩锁内、
//! **内容锁在闩锁外取**——两条纪律合起来 ⇒ 无环、无死锁。
//! 临界区纪律（**闩锁内不做 I/O**等四条）与闩锁统计口径见 §5.10"闩锁形态与纪律"。

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use bicdb_common::latch::{Latch, LatchGuard, LatchStats};
use bicdb_common::seq::Lsn;
use bicdb_workspace::io::{FileHandle, FileIo};

use crate::page::Page;
use crate::pagefile::{self, PageFileError};
use crate::rowid::Rdba;

/// 块定位器：`(工作区标识, rdba) →（页文件句柄、块号）`。
///
/// 带上工作区是因为**池是实例级共享**的：不同工作区各有自己的文件句柄。
pub type PoolResolver<'io> =
    dyn Fn(&[u8; 8], Rdba) -> Option<(FileHandle, u32)> + Send + Sync + 'io;

/// **工作区 → 分区**的稳定哈希（§5.10 的 `H`）：FNV-1a 起步 + splitmix64 收尾。
///
/// **收尾是必需的**：工作区标识多是"小的连号整数"（48 位序列的低字节在前），
/// FNV-1a 对这类短输入的低位区分度不足——实测 `[b; 8]`（b = 1..5）在
/// `N ≤ 8` 时**全部落同一个分区**（低位没雪崩）；收尾器把高位差扩散到低位，
/// 分区才是真的"按工作区散开"。
fn hash_workspace(ws: &[u8; 8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in ws {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    // splitmix64 finalizer（雪崩低位）。
    h ^= h >> 30;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^= h >> 31;
    h
}

/// 缓冲池的键：**工作区标识 + RDBA**（§5.10——池实例级共享，键必须带工作区）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BufferKey {
    /// 工作区受校验标识（页头同一字段）。
    pub workspace: [u8; 8],
    /// 块地址。
    pub rdba: Rdba,
}

impl BufferKey {
    /// 由工作区标识与块地址构造。
    #[must_use]
    pub fn new(workspace: [u8; 8], rdba: Rdba) -> Self {
        Self { workspace, rdba }
    }
}

/// 时钟（touch count 的三秒规则要"距上次递增"——测试用**手动时钟**）。
pub trait Clock: Send + Sync {
    /// 单调毫秒。
    fn now_ms(&self) -> u64;
}

/// 系统时钟。
#[derive(Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// 缓存配置（§5.10/证据包 `buffercache-mech-20261005`；证据未展开的取值
/// 标"自定"——全部可调）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheConfig {
    /// 哈希桶数（**质数**；默认 ≈ 容量/4，最小 7——Oracle `_DB_BLOCK_HASH_BUCKETS`
    /// 的默认口径 `db_block_buffers / 4`，取质数）。
    pub buckets: usize,
    /// 热段上限占比的分母（`HBMAX = 容量 / 该值`；Oracle `HBMAX` 语义，取值自定）。
    pub hot_fraction: usize,
    /// 触摸计数的最小递增间隔（毫秒）——**三秒规则**（有证据）。
    pub touch_interval_ms: u64,
    /// 冷却值（`_COOL_COUNT`/`kcbpacc` 语义：新装入与退回冷段时置的计数；
    /// 数值自定——取 0 ⇒"冷却块立即可复用"，被碰过的块获得一轮减半豁免）。
    pub cool_count: u32,
    /// 驻留值（`_STAY_COUNT`/`kcbpasc` 语义：提升到热段头时置的计数；数值自定）。
    pub stay_count: u32,
    /// 热判据（冷段计数达此值 ⇒ 升热段；自定）。
    pub hot_criteria: u32,
    /// 前台扫空闲缓冲的上限 = 容量 / 该值（Oracle `db_block_max_scan_cnt` 默认 缓冲数/4）。
    pub max_scan_fraction: usize,
    /// **桶闩锁数**（O3 桶分片：一组桶一把闩锁——`kcbz.h` 的"桶在 latch 间轮转"）。
    /// 默认 ≈ 桶数/8（夹取 1..=64）；= 1 即退回"单闩锁"形态（对照/测试可用）。
    pub bucket_latches: usize,
}

impl CacheConfig {
    /// 按容量给默认（自定取值见 §5.10）。
    #[must_use]
    pub fn for_capacity(capacity: usize) -> Self {
        Self {
            buckets: prime_at_least((capacity / 4).max(1)),
            hot_fraction: 4,
            touch_interval_ms: 3_000,
            cool_count: 0,
            stay_count: 2,
            hot_criteria: 2,
            max_scan_fraction: 4,
            // **2 的幂**：桶→分片的换算退化为位运算（热路径不背除法）。
            bucket_latches: ((capacity / 4).max(1) / 8).clamp(1, 64).next_power_of_two(),
        }
    }
}

/// ≥ n 的最小质数（桶数取质数，照 Oracle 口径）。
fn prime_at_least(n: usize) -> usize {
    let mut c = n.max(7);
    loop {
        if (2..c).take_while(|d| d * d <= c).all(|d| c % d != 0) {
            return c;
        }
        c += 1;
    }
}

/// WAL 协调口（**WAL 规则 2** 的落点，Oracle `KCBB_REDO` 的对应物）。
pub trait WalGuard: Send {
    /// 当前**已持久化**的 LSN 水位。
    fn durable_lsn(&self) -> Lsn;
    /// 把 redo 持久化到 `target`；**失败即错误**——页不得写出。
    ///
    /// `&self`（而非 `&mut self`）：池把它放在 `redo_write` 闩锁里，**WAL 自己
    /// 是线程安全的**——同一个口可交给后台 DBWR 线程用（P4 线程化）。
    fn ensure_durable(&self, target: Lsn) -> std::io::Result<()>;
}

/// 缓冲池错误。
#[derive(Debug)]
pub enum BufferError {
    /// 底层 I/O。
    Io(std::io::Error),
    /// 目标页损坏（两层校验失败）——按损坏处理，不静默返回数据。
    Damaged {
        /// 块地址。
        rdba: Rdba,
    },
    /// 块无法定位（定位器给不出句柄/块号）。
    Unresolved {
        /// 块地址。
        rdba: Rdba,
    },
    /// 页身份与键不符（串页防线）。
    IdentityMismatch {
        /// 请求的键。
        expected: BufferKey,
        /// 页头自述的工作区标识。
        found_workspace: [u8; 8],
        /// 页头自述的文件号。
        found_file: u16,
        /// 页头自述的块号。
        found_block: u32,
    },
    /// **等空闲缓冲**（`free buffer waits`）：Make Free 批处理之后仍无可用帧。
    FreeBufferWait,
    /// WAL 规则 2 的刷盘失败——页**没有**写出。
    WalFlush(std::io::Error),
    /// 容量为零（非法配置）。
    ZeroCapacity,
    /// 分区数非法（必须是 1 或 2 的幂）。
    BadPartitionCount {
        /// 请求的分区数。
        partitions: usize,
    },
    /// **工作集未排空**（重绑定要求"无在途会话"，详设 §7）：仍有脏帧或钉住帧。
    /// 唯一的净帧以外的帧都意味着 Draining 没做完（或有人正在用）——拒绝，
    /// 不静默丢帧。
    DrainBlocked {
        /// 仍脏的帧数。
        dirty: usize,
        /// 仍被钉住的帧数。
        pinned: usize,
    },
}

impl std::fmt::Display for BufferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BufferError::Io(e) => write!(f, "缓冲池 I/O：{e}"),
            BufferError::Damaged { rdba } => write!(
                f,
                "缓冲池目标页损坏（文件 {} 块 {}）——按损坏处理",
                rdba.file_id(),
                rdba.block_id()
            ),
            BufferError::Unresolved { rdba } => write!(
                f,
                "缓冲池块无法定位（文件 {} 块 {}）",
                rdba.file_id(),
                rdba.block_id()
            ),
            BufferError::IdentityMismatch {
                expected,
                found_workspace,
                found_file,
                found_block,
            } => write!(
                f,
                "页身份与键不符：期望文件 {} 块 {}（工作区 {expected:?}），页头自述文件 {found_file} 块 {found_block}（工作区 {found_workspace:?}）",
                expected.rdba.file_id(),
                expected.rdba.block_id()
            ),
            BufferError::FreeBufferWait => {
                f.write_str("free buffer waits：Make Free 之后仍无可用帧")
            }
            BufferError::WalFlush(e) => write!(f, "WAL 规则 2 前置刷盘失败（页未写出）：{e}"),
            BufferError::ZeroCapacity => f.write_str("缓冲池容量为零"),
            BufferError::BadPartitionCount { partitions } => {
                write!(f, "缓冲池分区数非法（{partitions}）：必须是 1 或 2 的幂")
            }
            BufferError::DrainBlocked { dirty, pinned } => write!(
                f,
                "工作集未排空：仍有脏帧 {dirty}、钉住帧 {pinned}（重绑定要求无在途会话）"
            ),
        }
    }
}

impl std::error::Error for BufferError {}

impl<T: WalGuard + Sync + ?Sized> WalGuard for std::sync::Arc<T> {
    fn durable_lsn(&self) -> Lsn {
        (**self).durable_lsn()
    }
    fn ensure_durable(&self, target: Lsn) -> std::io::Result<()> {
        (**self).ensure_durable(target)
    }
}

impl From<std::io::Error> for BufferError {
    fn from(e: std::io::Error) -> Self {
        BufferError::Io(e)
    }
}

/// 缓冲池统计（口径对齐 X$KCBWDS：FBWAIT、FBINSP/DBINSP/PNINSP、HOTMVS/AUX_MOV、
/// SUM_WRT；`aging_steps` 为 touch-count 老化算法的自有计数）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BufferStats {
    /// 命中次数（缓存层）。
    pub hits: u64,
    /// 未命中（读入）次数。
    pub misses: u64,
    /// 复用（腾出旧帧装新块）次数。
    pub evictions: u64,
    /// 写回次数（脏块落盘）。
    pub writes: u64,
    /// 因 WAL 规则 2 而触发的日志刷盘次数。
    pub wal_syncs: u64,
    /// `free buffer waits`（Make Free 之后仍无可用帧）。
    pub fb_wait: u64,
    /// 寻空闲扫描过的帧数（FBINSP）。
    pub free_inspected: u64,
    /// 寻空闲扫描遇到脏帧的次数（DBINSP）。
    pub dirty_inspected: u64,
    /// 寻空闲扫描遇到钉住帧的次数（PNINSP）。
    pub pinned_inspected: u64,
    /// 冷段高计数帧提升到热段的次数（HOTMVS）。
    pub hot_moved: u64,
    /// **老化减半**（aging）的次数——扫描遇到高计数候选、减半后继续（不走淘汰）。
    pub aging_steps: u64,
    /// **区读（多块读）次数**——一次 `pread` 覆盖一段连续页（§5.12）。
    pub run_reads: u64,
    /// 区读覆盖的页数。
    pub run_pages: u64,
    /// 写完成后放入 AUX 的次数（AUX_MOV）。
    pub aux_moved: u64,
}

impl BufferStats {
    /// 并入另一份计数（多分区的**聚合读取**；`V$KCBWDS` 也是逐工作集计数）。
    pub fn merge(&mut self, other: &BufferStats) {
        self.hits += other.hits;
        self.misses += other.misses;
        self.evictions += other.evictions;
        self.writes += other.writes;
        self.wal_syncs += other.wal_syncs;
        self.fb_wait += other.fb_wait;
        self.free_inspected += other.free_inspected;
        self.dirty_inspected += other.dirty_inspected;
        self.pinned_inspected += other.pinned_inspected;
        self.hot_moved += other.hot_moved;
        self.aging_steps += other.aging_steps;
        self.run_reads += other.run_reads;
        self.run_pages += other.run_pages;
        self.aux_moved += other.aux_moved;
    }
}

/// **统计分片数**（O3；与 `Latch` 计数同法：线程按栈地址选片）。
const STAT_SHARDS: usize = 8;

/// **一片统计计数**（原子 + 一行 cache line）：线程只写自己那片，
/// 读取时逐片求和——**仍然精确**（不是采样）。P0 实测：两个共享计数器
/// 单独吃掉 ~38% 吞吐（证据包 `latch-ab-20261005/`），O3 把它应用到池统计。
#[repr(align(64))]
#[derive(Debug, Default)]
struct StatsShard {
    hits: AtomicU64,
    misses: AtomicU64,
    evictions: AtomicU64,
    writes: AtomicU64,
    wal_syncs: AtomicU64,
    fb_wait: AtomicU64,
    free_inspected: AtomicU64,
    dirty_inspected: AtomicU64,
    pinned_inspected: AtomicU64,
    hot_moved: AtomicU64,
    aging_steps: AtomicU64,
    run_reads: AtomicU64,
    run_pages: AtomicU64,
    aux_moved: AtomicU64,
}

/// **分片统计集**（每分区一份；`BufferPool::stats` 再跨分区聚合）。
#[derive(Debug)]
struct StatsShards {
    shards: Vec<StatsShard>,
}

impl StatsShards {
    fn new() -> Self {
        Self {
            shards: (0..STAT_SHARDS).map(|_| StatsShard::default()).collect(),
        }
    }

    /// 本线程的片（栈地址 → 片号；零成本、零 unsafe——同 `Latch`）。
    fn shard(&self) -> &StatsShard {
        let x = 0u8;
        let tag = std::ptr::addr_of!(x) as usize;
        &self.shards[(tag >> 6) % self.shards.len()]
    }

    /// 记一次（本线程片）。
    fn inc<F: Fn(&StatsShard) -> &AtomicU64>(&self, field: F) {
        field(self.shard()).fetch_add(1, Ordering::Relaxed);
    }

    /// 记 n 次（本线程片）。
    fn add<F: Fn(&StatsShard) -> &AtomicU64>(&self, n: u64, field: F) {
        field(self.shard()).fetch_add(n, Ordering::Relaxed);
    }

    /// **逐片求和**（精确）。
    fn sum(&self) -> BufferStats {
        let mut out = BufferStats::default();
        for sh in &self.shards {
            out.hits += sh.hits.load(Ordering::Relaxed);
            out.misses += sh.misses.load(Ordering::Relaxed);
            out.evictions += sh.evictions.load(Ordering::Relaxed);
            out.writes += sh.writes.load(Ordering::Relaxed);
            out.wal_syncs += sh.wal_syncs.load(Ordering::Relaxed);
            out.fb_wait += sh.fb_wait.load(Ordering::Relaxed);
            out.free_inspected += sh.free_inspected.load(Ordering::Relaxed);
            out.dirty_inspected += sh.dirty_inspected.load(Ordering::Relaxed);
            out.pinned_inspected += sh.pinned_inspected.load(Ordering::Relaxed);
            out.hot_moved += sh.hot_moved.load(Ordering::Relaxed);
            out.aging_steps += sh.aging_steps.load(Ordering::Relaxed);
            out.run_reads += sh.run_reads.load(Ordering::Relaxed);
            out.run_pages += sh.run_pages.load(Ordering::Relaxed);
            out.aux_moved += sh.aux_moved.load(Ordering::Relaxed);
        }
        out
    }
}

/// 一次 `flush_workspace` 的报告。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FlushReport {
    /// 写回的页数。
    pub pages_written: u64,
    /// 写回后该工作区的新低水位（写列表已空 ⇒ `None`）。
    pub low_water: Option<Lsn>,
}

/// 工作集**排空**报告（重绑定协议的 Draining，详设 §7）：
/// ① 刷尽脏页（按写列表序）→ ② 丢弃全部净帧（释放页缓冲）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DrainReport {
    /// 写回的页数（Draining ①）。
    pub pages_written: u64,
    /// 丢弃的净帧数（Draining ②；页缓冲已释放，帧回"未分配"态）。
    pub frames_dropped: usize,
}

/// **帧的槽位状态对象**（§5.10 O2）：**内容锁与链闩分离**——帧的**页内容**
/// 不再由分区闩锁保护，而是每帧自带 `RwLock`（读共享 / 写独占）；`pins` 是
/// **原子**计数（**卫兵 ≠ 持锁**：持 [`crate::buffer::PageGuard`] 不再持分区
/// 闩锁）。
///
/// 槽位数组**构造后不移动、不替换**（下标恒稳定）——所以内容锁可以在**不持
/// 分区闩锁**的情况下取用，卫兵的生命周期因此与池（而非闩锁）对齐。
///
/// **不变量（内容锁 ⇒ pin > 0）**：内容锁只在 pin 计数已递增后取——
/// 于是 pins = 0 的帧其内容锁必空闲，`attach`/`replace_in_place` 等**在结构
/// 闩锁内**的装页路径可以用 `try_write()` **不阻塞**地拿内容锁（"持结构闩
/// 不等待内容锁"的纪律由此成立）。
struct FrameSlot {
    /// 原子 pin 计数（卫兵的增减**不经过**分区闩锁）。
    pins: AtomicU32,
    /// **TCH（触摸计数）**（O3：命中路径直接原子更新，不再进结构闩锁）。
    touches: AtomicU32,
    /// 上次计数递增的墙钟毫秒（**三秒规则**；同前——原子）。
    last_touch_ms: AtomicU64,
    /// **页内容**（首次装入时分配；未用过的帧不占 16 KiB）。
    ///
    /// 为什么惰性（§5.10 NUMA 第二级绑定）：帧内存的**首次触碰决定它落在哪个
    /// 节点**——构造时不分配，装页由"已在节点组内的线程"完成，本地性于是
    /// "零额外代码"成立；`drop_clean_frames`（重绑定 Draining ②）释放它，
    /// 下一次装入按新绑定重新落位。
    content: RwLock<Option<Page>>,
}

impl FrameSlot {
    fn empty() -> Self {
        Self {
            pins: AtomicU32::new(0),
            touches: AtomicU32::new(0),
            last_touch_ms: AtomicU64::new(0),
            content: RwLock::new(None),
        }
    }

    /// **三秒规则下的触摸计数**（命中路径；闩外原子更新——O3）。
    /// 返回"已达热判据"——命中路径据此决定**是否值得**去试提升（拿不到结构
    /// 闩锁的 `try` 也是一次 CAS：绝大多数命中不该付它）。
    fn bump_touch(&self, now: u64, cfg: &CacheConfig) -> bool {
        let last = self.last_touch_ms.load(Ordering::Relaxed);
        if now.saturating_sub(last) >= cfg.touch_interval_ms {
            let t = self.touches.fetch_add(1, Ordering::Relaxed) + 1;
            self.last_touch_ms.store(now, Ordering::Relaxed);
            return t >= cfg.hot_criteria;
        }
        self.touches.load(Ordering::Relaxed) >= cfg.hot_criteria
    }

    /// 重置触摸计数（装入/提升/退回 冷段时用）。
    fn reset_touch(&self, value: u32, now: u64) {
        self.touches.store(value, Ordering::Relaxed);
        self.last_touch_ms.store(now, Ordering::Relaxed);
    }
}

/// 帧的**归属与记账**（由分区的结构闩锁保护——临界区短）。
/// **O3 已把命中路径的字段移出**：`touches`/`last_touch_ms` 进了 [`FrameSlot`]
/// （原子）；`hits` 等统计进了 [`StatsShards`]（分片原子）——结构闩锁上只剩
/// "键 + 脏/首次变脏 LSN"（键的变更规则见 `BucketShard` 的文档）。
#[derive(Debug, Clone)]
struct FrameMeta {
    key: Option<BufferKey>,
    /// 脏标志（同时在写列表里）。
    dirty: bool,
    /// **首次变脏的 LSN**（写列表/检查点队列排序键）。
    first_dirty: Option<Lsn>,
}

impl FrameMeta {
    fn empty() -> Self {
        Self {
            key: None,
            dirty: false,
            first_dirty: None,
        }
    }
}

/// **分区（工作集）的结构面**：帧元数据 + 替换链 + 桶 + 写列表 + 统计——
/// 由分区的具名闩锁（`db_cache`）保护。**页内容与 pin 不在其中**（O2）。
struct Structure {
    /// 帧元数据（下标与 [`Partition::slots`] 一一对应）。
    meta: Vec<FrameMeta>,
    /// 从未用过的帧（首次装入后帧就长期挂在链上）。
    virgin: Vec<usize>,
    /// 热段（头 = 最热）。
    hot: VecDeque<usize>,
    /// 冷段（头 = 刚重用；尾 = 最先淘汰）。
    cold: VecDeque<usize>,
    /// 可重用候选（干净、未钉住；前台优先扫它）。
    aux: VecDeque<usize>,
    /// 写列表：**每工作区一条**（= 检查点队列），按（首次变脏 LSN, rdba）升序。
    write_list: BTreeMap<[u8; 8], BTreeSet<(Lsn, Rdba)>>,
    cfg: CacheConfig,
}

/// **桶分片**（O3）：一片闩锁保护的一组桶链。
///
/// **链上条目 `(键, 帧号)`——键随链走**：命中路径只在这一把闩锁下完成
/// "定位 + pin"，**不读**结构闩锁下的帧元数据。
///
/// **键变更规则**（O3 不变量，与"内容锁 ⇒ pin > 0"并列）：
/// ① 帧的键只由**持结构闩锁者**变更（于是 `FrameMeta::key` 的读写都在结构
///    闩下，无需第三把锁）；
/// ② 摘/挂桶链都要持**该链的桶闩锁**。
/// 于是"持桶闩 + 在链上看到 `(K, idx)`" ⇒ 帧 idx 此刻不可能正被换键
/// （换键者必须先摘旧链 = 先持这把闩）——命中路径的读取因此无需再验证。
#[derive(Debug)]
struct BucketShard {
    /// 本片桶链（局部桶号 → 条目链）。
    chains: Vec<Vec<(BufferKey, usize)>>,
}

/// 桶号 = **DBA（rdba）对桶数取模**（Oracle `_DB_BLOCK_HASH_BUCKETS` 原文
/// 口径："hash the required DBA by this number"）；跨工作区的同址块落同桶
/// ——链上再按完整键比对。
fn bucket_of(cfg: &CacheConfig, key: BufferKey) -> usize {
    let dba = (u64::from(key.rdba.file_id()) << 28) | u64::from(key.rdba.block_id());
    (dba % cfg.buckets as u64) as usize
}

/// 桶 → 分片（**相邻桶轮转**到不同闩锁——`kcbz.h` 的"桶在 latch 间轮转"）。
fn shard_of(cfg: &CacheConfig, bucket: usize) -> usize {
    bucket % cfg.bucket_latches.max(1)
}

/// 桶在本片内的局部号。
fn local_bucket(cfg: &CacheConfig, bucket: usize) -> usize {
    bucket / cfg.bucket_latches.max(1)
}

/// 每片的桶数（向上取整）。
fn chains_per_shard(cfg: &CacheConfig) -> usize {
    let n = cfg.bucket_latches.max(1);
    cfg.buckets.div_ceil(n).max(1)
}

/// **一个工作集分区**（§5.10）：帧槽数组（稳定地址、自带同步原语）+
/// 桶分片（O3）+ 结构闩锁 + 分片统计（O3）。`slots` 与其余字段是**不相交
/// 的字段**——同一个方法里可以同时持有"闩锁卫兵"与"帧槽的共享借用"。
struct Partition {
    /// 桶换算所需的配置副本（与 `structure.cfg` 同源）。
    cfg: CacheConfig,
    slots: Box<[FrameSlot]>,
    /// **桶分片闩锁组**（O3）：命中路径只碰这里。
    buckets: Vec<Latch<BucketShard>>,
    structure: Latch<Structure>,
    /// 分片统计（O3；与结构闩锁分离）。
    stats: StatsShards,
}

/// 写回目标（`flush` / 写列表头 / 全局最老头）。
#[derive(Debug, Clone, Copy)]
enum WriteTarget {
    /// 指定页（`flush(key)`）。
    Key(BufferKey),
    /// 某工作区写列表的头。
    WorkspaceHead([u8; 8]),
    /// 所有工作区里"最老首次变脏 LSN"最小的头（Make Free）。
    OldestHead,
}

/// 候选帧"声明"结果（O3：腾帧前必须在旧键桶闩下复核 pins）。
enum Detach {
    /// 已摘链：`Some(旧键)` = 摘到旧桶链条目；`None` = 帧本来无键（virgin）。
    Done(Option<BufferKey>),
    /// 被并发钉住：放弃候选（调用方重选）。
    Pinned,
}

/// 选页结果。
enum Pick {
    /// 无候选。
    None,
    /// 失步条目已清理（有进展、没写页）。
    Stale,
    /// 可写：帧与定位信息。
    Ready {
        idx: usize,
        key: BufferKey,
        handle: FileHandle,
        block: u32,
    },
}

/// 一次写回的冻结快照（闩锁外的 I/O 阶段用它；期间页可被再修改）。
struct WriteJob {
    idx: usize,
    key: BufferKey,
    handle: FileHandle,
    block: u32,
    /// 冻结的页内容（写盘用副本）。
    image: Page,
    /// 快照时的 `mod_seq`（收尾比对：变了 = 期间被再改脏）。
    mod_seq: u8,
    page_lsn: Lsn,
}

/// **DB Cache**（§5.10；本切片 N = 1 分区）。
pub struct BufferPool<'io> {
    io: &'io dyn FileIo,
    /// **每分区容量**（帧数）。
    capacity: usize,
    /// **工作集分区**（§5.10）：每份自带替换链/桶/写列表与**具名闩锁**
    /// （"db_cache"；先自旋后睡眠、V$LATCH 口径——证据包 `latch-mech-20261005/`）。
    /// **闩锁内不做 I/O**：读盘与写回都在闩外完成（两阶段，见 `pin`/`make_free`）。
    /// **O2（2026-10-05）**：帧的内容与 pin 在 `Partition::slots`（不经闩锁），
    /// 闩锁只护结构面。
    partitions: Vec<Partition>,
    /// 共享的块定位器（跨分区同一份；`Fn + Send + Sync`）。
    resolve: Box<PoolResolver<'io>>,
    /// 共享时钟（touch-count 三秒规则）。
    clock: Box<dyn Clock + 'io>,
    /// **redo 写闩锁**：串行化 WAL 刷盘——`ensure_durable` 的 fsync 在闩内
    /// （"一次 fsync"的串行点；Oracle `redo writing latch` 的对应物），
    /// 它是全库唯一允许在闩锁内做 I/O 的地方（且只做这一件）。
    wal: Latch<Box<dyn WalGuard + 'io>>,
}

impl std::fmt::Debug for BufferPool<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BufferPool")
            .field("capacity", &self.capacity)
            .finish_non_exhaustive()
    }
}

impl<'io> BufferPool<'io> {
    /// 建池（系统时钟 + 按容量派的默认配置）。
    pub fn new(
        io: &'io dyn FileIo,
        capacity: usize,
        resolve: impl Fn(&[u8; 8], Rdba) -> Option<(FileHandle, u32)> + Send + Sync + 'io,
        wal: impl WalGuard + 'io,
    ) -> Result<Self, BufferError> {
        Self::with_clock(io, capacity, resolve, wal, SystemClock)
    }

    /// 建池（指定时钟——测试用手动时钟控制三秒规则）。
    pub fn with_clock(
        io: &'io dyn FileIo,
        capacity: usize,
        resolve: impl Fn(&[u8; 8], Rdba) -> Option<(FileHandle, u32)> + Send + Sync + 'io,
        wal: impl WalGuard + 'io,
        clock: impl Clock + 'io,
    ) -> Result<Self, BufferError> {
        Self::with_config(
            io,
            capacity,
            resolve,
            wal,
            clock,
            CacheConfig::for_capacity(capacity),
        )
    }

    /// 建池（全参数）。
    pub fn with_config(
        io: &'io dyn FileIo,
        capacity: usize,
        resolve: impl Fn(&[u8; 8], Rdba) -> Option<(FileHandle, u32)> + Send + Sync + 'io,
        wal: impl WalGuard + 'io,
        clock: impl Clock + 'io,
        cfg: CacheConfig,
    ) -> Result<Self, BufferError> {
        Self::with_partitions(io, 1, capacity, resolve, wal, clock, cfg)
    }

    /// **多工作集分区**（§5.10 的 P4 形态）：`partitions` 个**工作集**，
    /// 每个自带一条替换链（含 AUX）、一套桶、写列表与**自己的闩锁**；
    /// `capacity` 是**每个分区**的帧数。
    ///
    /// - **映射**：`H(工作区标识) mod partitions`（稳定哈希；2 的幂 ⇒ 按位与）
    ///   ——同一个工作区每次都落同一个工作集（否则写列表会在写线程之间搬家）；
    /// - **一个工作区不被拆分**：它的全部缓冲、写列表都在一个分区里
    ///   ⇒ 检查点推进只碰一个闩锁，零跨分区协调；
    /// - `partitions` 必须是 1 或 2 的幂（取模退化为按位与）。
    #[allow(clippy::too_many_arguments)]
    pub fn with_partitions(
        io: &'io dyn FileIo,
        partitions: usize,
        capacity: usize,
        resolve: impl Fn(&[u8; 8], Rdba) -> Option<(FileHandle, u32)> + Send + Sync + 'io,
        wal: impl WalGuard + 'io,
        clock: impl Clock + 'io,
        cfg: CacheConfig,
    ) -> Result<Self, BufferError> {
        if capacity == 0 {
            return Err(BufferError::ZeroCapacity);
        }
        if partitions == 0 || !partitions.is_power_of_two() {
            return Err(BufferError::BadPartitionCount { partitions });
        }
        let mk = |_| Partition {
            cfg,
            slots: (0..capacity).map(|_| FrameSlot::empty()).collect(),
            buckets: (0..cfg.bucket_latches.max(1))
                .map(|_| {
                    Latch::new(
                        "db_bucket",
                        BucketShard {
                            chains: vec![Vec::new(); chains_per_shard(&cfg)],
                        },
                    )
                })
                .collect(),
            structure: Latch::new(
                "db_cache",
                Structure {
                    meta: (0..capacity).map(|_| FrameMeta::empty()).collect(),
                    virgin: (0..capacity).rev().collect(),
                    hot: VecDeque::new(),
                    cold: VecDeque::new(),
                    aux: VecDeque::new(),
                    write_list: BTreeMap::new(),
                    cfg,
                },
            ),
            stats: StatsShards::new(),
        };
        Ok(Self {
            io,
            capacity,
            partitions: (0..partitions).map(mk).collect(),
            resolve: Box::new(resolve),
            clock: Box::new(clock),
            wal: Latch::new("redo_write", Box::new(wal)),
        })
    }

    /// **工作区 → 分区**（§5.10：`H(工作区标识) mod N`，稳定哈希；2 的幂
    /// ⇒ 按位与）。同一个工作区每次都落同一个工作集——写列表不在写线程间搬家。
    #[must_use]
    pub fn partition_of(&self, workspace: &[u8; 8]) -> usize {
        (hash_workspace(workspace) as usize) & (self.partitions.len() - 1)
    }

    /// 分区数（诊断）。
    #[must_use]
    pub fn partition_count(&self) -> usize {
        self.partitions.len()
    }

    /// 某分区的闩锁统计（诊断：逐工作集的 `V$LATCH` 口径）。
    #[must_use]
    pub fn partition_latch_stats(&self) -> Vec<LatchStats> {
        self.partitions
            .iter()
            .flat_map(|p| {
                let mut out = vec![p.structure.stats()];
                // 桶分片（O3）：逐片求和为一个 "db_bucket" 项（V$LATCH 口径）。
                let mut b = LatchStats {
                    name: "db_bucket",
                    gets: 0,
                    immediate: 0,
                    spin_gets: 0,
                    sleeps: 0,
                    wait_ns: 0,
                };
                for shard in &p.buckets {
                    let s = shard.stats();
                    b.gets += s.gets;
                    b.immediate += s.immediate;
                    b.spin_gets += s.spin_gets;
                    b.sleeps += s.sleeps;
                    b.wait_ns += s.wait_ns;
                }
                out.push(b);
                out
            })
            .collect()
    }

    /// 容量（帧数）。
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// 当前驻留帧数（全部分区之和）。
    #[must_use]
    pub fn resident(&self) -> usize {
        self.partitions
            .iter()
            .map(|p| p.structure.lock().capacity_used())
            .sum()
    }

    /// 哈希桶数（每分区同值）。
    #[must_use]
    pub fn bucket_count(&self) -> usize {
        self.partitions[0].cfg.buckets
    }

    /// 某桶的链长（诊断；**全部分区之和**——单分区时即该桶链长）。
    #[must_use]
    pub fn bucket_len(&self, bucket: usize) -> usize {
        self.partitions
            .iter()
            .map(|p| {
                let s = shard_of(&p.cfg, bucket);
                let local = local_bucket(&p.cfg, bucket);
                p.buckets[s].lock().chains.get(local).map_or(0, Vec::len)
            })
            .sum()
    }

    /// 统计快照（**全部分区之和**；口径与 X$KCBWDS 对齐——Oracle 也是逐工作集
    /// 计数、汇总读取）。
    #[must_use]
    pub fn stats(&self) -> BufferStats {
        let mut out = BufferStats::default();
        for p in &self.partitions {
            out.merge(&p.stats.sum()); // 分片原子逐片求和（精确；O3）
        }
        out
    }

    /// 某工作区写列表长度（= 脏块数）。
    #[must_use]
    pub fn dirty_len(&self, workspace: [u8; 8]) -> usize {
        self.lock_of(&workspace)
            .write_list
            .get(&workspace)
            .map_or(0, BTreeSet::len)
    }

    /// **有脏页的工作区**（DBWR 后台线程的入口：按此逐个 `flush_workspace`）。
    #[must_use]
    pub fn dirty_workspaces(&self) -> Vec<[u8; 8]> {
        let mut out = Vec::new();
        for p in &self.partitions {
            out.extend(p.structure.lock().write_list.keys().copied());
        }
        out
    }

    /// **某分区里有脏页的工作区**（按分区写线程的扫描面：一个写线程只看
    /// 自己分区的写列表——§5.10"一个分区只由一个写线程负责"）。
    #[must_use]
    pub fn dirty_workspaces_in(&self, partition: usize) -> Vec<[u8; 8]> {
        self.lock(partition).write_list.keys().copied().collect()
    }

    /// **低水位**：该工作区最老脏块的（首次变脏 LSN）；`None` = 无脏页。
    #[must_use]
    pub fn low_water(&self, workspace: [u8; 8]) -> Option<Lsn> {
        self.lock_of(&workspace)
            .write_list
            .get(&workspace)
            .and_then(|s| s.iter().next().map(|(lsn, _)| *lsn))
    }

    /// 某帧的 **TCH**（touch count——对应 `x$bh` 的 `TCH` 列；热块诊断在
    /// Oracle 侧即"TCH 越高，块被访问越频繁"）。
    #[must_use]
    pub fn touch_count(&self, key: BufferKey) -> Option<u32> {
        let partition = self.partition_of(&key.workspace);
        let idx = {
            let (_s, local, g) = self.lock_bucket(partition, key);
            Self::chain_find(&g, local, key)?
        };
        Some(self.slots(partition)[idx].touches.load(Ordering::Relaxed))
    }

    /// 某帧在哪条链上（`hot` / `cold` / `aux`；诊断与测试）。
    #[must_use]
    pub fn chain_of(&self, key: BufferKey) -> Option<&'static str> {
        let partition = self.partition_of(&key.workspace);
        let st = self.lock(partition);
        let idx = {
            let (_s, local, g) = self.lock_bucket(partition, key);
            Self::chain_find(&g, local, key)?
        };
        Some(if st.hot.contains(&idx) {
            "hot"
        } else if st.cold.contains(&idx) {
            "cold"
        } else if st.aux.contains(&idx) {
            "aux"
        } else {
            "none"
        })
    }

    /// **钉住一页（独占）**（命中 → touch count；未命中 → 读入 → 冷段头）。
    ///
    /// **O2 的闩锁纪律**（§5.10）：结构闩锁只覆盖"定位 + pin 计数 + 记账"；
    /// **页内容锁在闩锁之外取**——不同块的访问因此互不串行，同一块由每帧的
    /// 内容锁串行（卫兵 ≠ 持锁）。未命中时读盘 + 身份核对（串页防线）同样
    /// 在闩外（两阶段；证据包 `latch-mech-20261005/` 结论 1）。
    pub fn pin(&self, key: BufferKey) -> Result<PageGuard<'_>, BufferError> {
        let partition = self.partition_of(&key.workspace);
        let slots = self.slots(partition);
        let stats = self.stats_of(partition);
        // **命中路径（O3）**：只要这把桶闩锁——结构闩锁不再参与。
        let hit = {
            let (_s, local, g) = self.lock_bucket(partition, key);
            match Self::chain_find(&g, local, key) {
                Some(idx) => {
                    slots[idx].pins.fetch_add(1, Ordering::AcqRel);
                    let hot =
                        slots[idx].bump_touch(self.clock.now_ms(), &self.partitions[partition].cfg);
                    Some((idx, hot))
                }
                None => None,
            }
        };
        let idx = match hit {
            Some((idx, hot)) => {
                stats.inc(|s| &s.hits);
                if hot {
                    self.try_promote(partition, idx); // 尽力（持桶闩只 try 结构闩）
                }
                idx
            }
            None => {
                stats.inc(|s| &s.misses);
                let (handle, block) = (self.resolve)(&key.workspace, key.rdba)
                    .ok_or(BufferError::Unresolved { rdba: key.rdba })?;
                // 闩锁外：读盘 + 身份核对。
                let page =
                    pagefile::read_page_verified(self.io, handle, block).map_err(|e| match e {
                        PageFileError::Damaged { .. } => BufferError::Damaged { rdba: key.rdba },
                        PageFileError::Io(e) => BufferError::Io(e),
                    })?;
                self.verify_identity(&page, key)?;
                // 装入（先到者为准；容量不足时闩外 Make Free 后重试一次）。
                self.install(partition, key, page)?
            }
        };
        let slot = &slots[idx];
        let content = content_write(&slot.content);
        Ok(PageGuard {
            content: Some(content),
            pins: &slot.pins,
            structure: self.structure(partition),
            key,
            idx,
        })
    }

    /// **共享钉住（命中即取；不触发读盘）**：`pins` 递增 + 内容**读**锁——
    /// 同一热块的并发读互不串行（§5.10 O2 的目标之一）。未驻留 ⇒ `None`。
    pub fn pin_shared(&self, key: BufferKey) -> Option<PageReadGuard<'_>> {
        let partition = self.partition_of(&key.workspace);
        let slots = self.slots(partition);
        // **命中路径（O3）**：桶闩锁 + 原子（不碰结构闩锁）。
        let (idx, hot) = {
            let (_s, local, g) = self.lock_bucket(partition, key);
            let idx = Self::chain_find(&g, local, key)?;
            slots[idx].pins.fetch_add(1, Ordering::AcqRel);
            let hot = slots[idx].bump_touch(self.clock.now_ms(), &self.partitions[partition].cfg);
            (idx, hot)
        };
        self.stats_of(partition).inc(|s| &s.hits);
        if hot {
            self.try_promote(partition, idx);
        }
        let slot = &slots[idx];
        let content = content_read(&slot.content);
        Some(PageReadGuard {
            content: Some(content),
            pins: &slot.pins,
        })
    }

    /// **装入一页**（返回帧号，**已 pin**）：期间已被他人装入 ⇒ **以先到者为
    /// 准**（本副本丢弃）；否则腾帧 attach；无可用帧 ⇒ 闩外 Make Free 后重试
    /// 一次，仍无 ⇒ `FreeBufferWait`。
    fn install(&self, partition: usize, key: BufferKey, page: Page) -> Result<usize, BufferError> {
        let slots = self.slots(partition);
        let stats = self.stats_of(partition);
        let mut page = Some(page);
        let mut made_free = false;
        let mut claim_misses = 0u32;
        loop {
            let mut st = self.lock(partition);
            // ① 先到者为准（**桶闩下复核**——与命中路径的 pins++ 串行）。
            if let Some(idx) = self.pin_existing(partition, key) {
                drop(st);
                stats.inc(|s| &s.hits);
                self.try_promote(partition, idx);
                return Ok(idx);
            }
            // ② 选候选帧 + **声明**（旧键桶闩下复核 pins==0；被并发钉住则重选）。
            if let Some(victim) = st.find_reusable(slots, stats) {
                match self.detach_for_reuse(partition, &mut st, victim) {
                    Detach::Pinned => {
                        claim_misses += 1;
                        if claim_misses >= 4 {
                            drop(st);
                            self.make_free(partition)?; // 腾干净页给下一轮
                            claim_misses = 0;
                            made_free = true;
                        }
                        continue;
                    }
                    Detach::Done(_old) => {
                        let p = page.take().expect("页只装一次");
                        match self.publish_into(partition, &mut st, victim, key, p) {
                            Ok(idx) => return Ok(idx),
                            // 先到者已在（罕见竞态）：本帧退回未分配（净帧 ⇒ 安全）。
                            Err(_existing) => {
                                self.discard_claimed(partition, &mut st, victim);
                                continue;
                            }
                        }
                    }
                }
            }
            drop(st);
            if made_free {
                stats.inc(|s| &s.fb_wait);
                return Err(BufferError::FreeBufferWait);
            }
            self.make_free(partition)?; // I/O 在闩外
            made_free = true;
        }
    }

    /// **命中即钉住**（桶闩下 `pins++`）；未命中 ⇒ `None`。调用者常已持结构闩
    /// （"结构 → 桶"的正向序——见 [`BucketShard`]）。
    fn pin_existing(&self, partition: usize, key: BufferKey) -> Option<usize> {
        let slots = self.slots(partition);
        let (_s, local, g) = self.lock_bucket(partition, key);
        let idx = Self::chain_find(&g, local, key)?;
        slots[idx].pins.fetch_add(1, Ordering::AcqRel);
        let _ = slots[idx].bump_touch(self.clock.now_ms(), &self.partitions[partition].cfg);
        Some(idx)
    }

    /// **声明一个候选帧供复用**（结构闩下）：
    /// 在**旧键的桶闩**下复核 `pins == 0`（命中路径的 `pins++` 也在这把闩下
    /// ⇒ 两者串行——这是 O3 新增的竞态关），随后摘替换链与旧桶链。
    /// 被并发钉住 ⇒ [`Detach::Pinned`]（放弃候选，调用方重选）。
    fn detach_for_reuse(&self, partition: usize, st: &mut Structure, idx: usize) -> Detach {
        let old = st.meta[idx].key;
        match old {
            // 未用过的帧：不在任何链上（命中路径不可达） ⇒ 无并发窗口。
            None => {
                Structure::detach_from_chains(&mut st.hot, &mut st.cold, &mut st.aux, idx);
                Detach::Done(None)
            }
            Some(k) => {
                let (_s, local, mut g) = self.lock_bucket(partition, k);
                if self.slots(partition)[idx].pins.load(Ordering::Acquire) != 0 {
                    return Detach::Pinned;
                }
                Structure::detach_from_chains(&mut st.hot, &mut st.cold, &mut st.aux, idx);
                Self::chain_remove(&mut g, local, k, idx);
                st.meta[idx].key = None;
                Detach::Done(Some(k))
            }
        }
    }

    /// **发布**一个已声明的帧：**新键桶闩下**查重 → 装内容 → 挂链 → 落冷段头。
    /// 内容先换、链后挂（挂上即可被命中看到，不能给它旧内容）。
    /// 重复键（先到者已在）⇒ `Err(先到者帧号)`。
    fn publish_into(
        &self,
        partition: usize,
        st: &mut Structure,
        idx: usize,
        key: BufferKey,
        page: Page,
    ) -> Result<usize, usize> {
        let cfg = &self.partitions[partition].cfg;
        let slots = self.slots(partition);
        let (_s, local, mut g) = self.lock_bucket(partition, key);
        if let Some(existing) = Self::chain_find(&g, local, key) {
            return Err(existing);
        }
        {
            let mut content = slots[idx]
                .content
                .try_write()
                .expect("pins=0 ⇒ 内容锁必空闲（O2 不变量）");
            *content = Some(page);
        }
        st.meta[idx] = FrameMeta {
            key: Some(key),
            dirty: false,
            first_dirty: None,
        };
        slots[idx].reset_touch(cfg.cool_count, self.clock.now_ms());
        slots[idx].pins.store(1, Ordering::Release); // 装入者持有（`pin` 语义）
        Self::chain_push(&mut g, local, key, idx);
        st.cold.push_front(idx); // 新读入/重用 ⇒ 冷段头（不是热段）
        Ok(idx)
    }

    /// 发布撞到重复键时把已声明的帧**退回未分配态**（净帧 ⇒ 丢弃安全；
    /// 罕见竞态下损失一次缓存，换来"绝无同键双帧"）。
    fn discard_claimed(&self, partition: usize, st: &mut Structure, idx: usize) {
        let slots = self.slots(partition);
        debug_assert_eq!(slots[idx].pins.load(Ordering::Acquire), 0);
        {
            let mut content = slots[idx]
                .content
                .try_write()
                .expect("pins=0 ⇒ 内容锁必空闲（O2 不变量）");
            *content = None;
        }
        st.meta[idx] = FrameMeta::empty();
        st.virgin.push(idx);
    }

    /// **同键原位替换**（权威镜像路径；调用者持结构闩 + 该键的桶闩、已复核
    /// `pins==0`）：帧在链上位置不变，只换内容与记账。
    fn replace_in_place(
        &self,
        partition: usize,
        st: &mut Structure,
        idx: usize,
        key: BufferKey,
        page: Page,
    ) {
        let cfg = &self.partitions[partition].cfg;
        let slots = self.slots(partition);
        debug_assert_eq!(
            slots[idx].pins.load(Ordering::Acquire),
            0,
            "同键重装时不应有在途卫兵（权威镜像路径）"
        );
        if let Some(lsn) = st.meta[idx].first_dirty.take() {
            st.drop_write_entry(key.workspace, lsn, key.rdba);
        }
        {
            let mut content = slots[idx]
                .content
                .try_write()
                .expect("pins=0 ⇒ 内容锁必空闲（O2 不变量）");
            *content = Some(page);
        }
        st.meta[idx] = FrameMeta {
            key: Some(key),
            dirty: false,
            first_dirty: None,
        };
        slots[idx].reset_touch(cfg.cool_count, self.clock.now_ms());
        slots[idx].pins.store(1, Ordering::Release);
    }

    /// **释放一帧**（Draining ② 的丢净帧；调用者持结构闩、且已在桶闩下复核
    /// `pins==0`）：摘链、清桶、释放页缓冲，帧回"未分配"态（virgin）。
    fn release_frame(&self, partition: usize, st: &mut Structure, idx: usize) {
        let slots = self.slots(partition);
        debug_assert!(!st.meta[idx].dirty);
        debug_assert_eq!(slots[idx].pins.load(Ordering::Acquire), 0);
        match self.detach_for_reuse(partition, st, idx) {
            Detach::Done(_) => {}
            Detach::Pinned => return, // 防御：调用方已体检，不应发生
        }
        {
            let mut content = slots[idx]
                .content
                .try_write()
                .expect("pins=0 ⇒ 内容锁必空闲（O2 不变量）");
            *content = None; // 释放 16 KiB（重绑定后按新绑定重新分配）
        }
        st.meta[idx] = FrameMeta::empty();
        st.virgin.push(idx);
    }

    /// 身份核对（页头自述与键逐项相符——串页防线）。
    fn verify_identity(&self, page: &Page, key: BufferKey) -> Result<(), BufferError> {
        let header = page
            .header()
            .ok_or(BufferError::Damaged { rdba: key.rdba })?;
        if header.file_id != key.rdba.file_id()
            || header.block_id != key.rdba.block_id()
            || header.workspace_ref != key.workspace
        {
            return Err(BufferError::IdentityMismatch {
                expected: key,
                found_workspace: header.workspace_ref,
                found_file: header.file_id,
                found_block: header.block_id,
            });
        }
        Ok(())
    }

    /// **装入一张新页**（尚未落盘的分配页——不经 read，不做身份核对）。
    ///
    /// 调用方随后应自行生成 redo（新页的"前像" = 全零页）并 `mark_dirty`。
    /// 返回的卫兵已钉住该帧。
    pub fn insert_new(&self, key: BufferKey, page: Page) -> Result<PageGuard<'_>, BufferError> {
        // **该键仍在池中 ⇒ 原位替换**：页被重置/复用（段回卷、重置复用的撤销页）
        // 时调用方给的镜像就是权威内容——若走 `find_reusable`/`attach`，
        // 桶里会留下**两个同键帧**，`find_frame` 命中的仍是旧的干净帧 ⇒
        // 写回被静默跳过（新内容永远到不了盘上；实测的撤销页丢失即此）。
        let partition = self.partition_of(&key.workspace);
        let idx = self.install_authoritative(partition, key, page)?;
        let slots = self.slots(partition);
        let slot = &slots[idx];
        let content = content_write(&slot.content);
        Ok(PageGuard {
            content: Some(content),
            pins: &slot.pins,
            structure: self.structure(partition),
            key,
            idx,
        })
    }

    /// 装入"尚未落盘的新分配页"（权威镜像）：同键 ⇒ 原位替换（**pins=0** 时）；
    /// 否则腾帧 attach（旧内容被权威镜像取代）；无可用帧 ⇒ Make Free 后重试。
    fn install_authoritative(
        &self,
        partition: usize,
        key: BufferKey,
        page: Page,
    ) -> Result<usize, BufferError> {
        let slots = self.slots(partition);
        let stats = self.stats_of(partition);
        let mut page = Some(page);
        let mut made_free = false;
        loop {
            let mut st = self.lock(partition);
            // 同键在池 ⇒ **桶闩下复核 `pins==0`**（与命中路径串行）后原位替换。
            {
                let (_s, local, g) = self.lock_bucket(partition, key);
                if let Some(idx) = Self::chain_find(&g, local, key) {
                    if slots[idx].pins.load(Ordering::Acquire) == 0 {
                        let p = page.take().expect("页只装一次");
                        self.replace_in_place(partition, &mut st, idx, key, p);
                        return Ok(idx);
                    }
                    drop(g);
                    // 有人钉着：闩外等它（读者退场后重入替换）。
                    drop(st);
                    let content = content_write(&slots[idx].content);
                    drop(content);
                    continue;
                }
            }
            if let Some(victim) = st.find_reusable(slots, stats) {
                if !matches!(
                    self.detach_for_reuse(partition, &mut st, victim),
                    Detach::Done(_)
                ) {
                    continue;
                }
                let p = page.take().expect("页只装一次");
                match self.publish_into(partition, &mut st, victim, key, p) {
                    Ok(idx) => return Ok(idx),
                    Err(_existing) => {
                        self.discard_claimed(partition, &mut st, victim);
                        continue;
                    }
                }
            }
            drop(st);
            if made_free {
                stats.inc(|s| &s.fb_wait);
                return Err(BufferError::FreeBufferWait);
            }
            self.make_free(partition)?; // I/O 在闩外
            made_free = true;
        }
    }

    /// **命中即拷副本**（**不触发读盘**）：扫描批查池用（§5.12）。
    /// 不 touch（扫描语义——不让一次性扫描顶热计数；§5.10 的"一次性扫描
    /// 不该污染热段"由此在 API 上显式化）。
    #[must_use]
    pub fn copy_if_resident(&self, key: BufferKey) -> Option<Page> {
        let partition = self.partition_of(&key.workspace);
        let slots = self.slots(partition);
        let idx = {
            // **pin 必须在（桶）闩锁内递增**：闩外取内容锁的窗口里，帧可能已被
            // 腾出换人（pins=0 ⇒ 可淘汰）——那会读到**别的块**的内容。
            self.pin_existing(partition, key)?
        };
        let slot = &slots[idx];
        let content = content_read(&slot.content);
        let copy = content.as_ref().map(|p| p.clone());
        drop(content);
        slot.pins.fetch_sub(1, Ordering::AcqRel);
        copy
    }

    /// **装入一页"净页"**（从文件读来的盘上内容）：不标脏、不生成 redo；
    /// 帧落**冷段**（新读入语义，§5.10）。已驻留则保留先到者。
    ///
    /// 与 [`BufferPool::insert_new`] 的区别：这里是**净页**（盘上内容的副本），
    /// 那边是"尚未落盘的新分配页"（前像为零页、必须"先 fsync 后进 redo"，
    /// §11.5.4）——两种装入语义不得混用。
    pub fn load_clean(&self, key: BufferKey, page: Page) -> Result<(), BufferError> {
        self.insert_clean(key, page)
    }

    /// **装入净页（两阶段）**：闩锁内 attach；需腾帧则在闩外 Make Free 后重试。
    /// 已驻留 ⇒ 保留先到者（传入副本丢弃）。
    fn insert_clean(&self, key: BufferKey, page: Page) -> Result<(), BufferError> {
        let partition = self.partition_of(&key.workspace);
        // 净页装入 = 空 pin 的通用装入（已驻留 ⇒ 先到者为准，副本丢弃）。
        let idx = self.install(partition, key, page)?;
        self.slots(partition)[idx]
            .pins
            .fetch_sub(1, Ordering::AcqRel); // 净页装入立即放掉
        Ok(())
    }

    /// **扫描区读（多块读）**（§5.12）：对 `[first, first+count)` 的**连续**
    /// 页——命中者直接拷副本（不 touch）；有缺失则**一次 `pread`** 读入整段，
    /// 缺失页 `load_clean` 装入（净页、冷段），返回与请求同序的页副本。
    ///
    /// - 一段区读**不跨文件**（`first + count` 必须落在同一文件内，由调用方
    ///   用扫描边界 §4.3.1 保证）；
    /// - 读入的页可以是**任意版本**——可见性由调用方在副本上做 CR（§12.3）。
    pub fn read_run(
        &self,
        workspace: [u8; 8],
        first: Rdba,
        count: u32,
    ) -> Result<Vec<Page>, BufferError> {
        if count == 0 {
            return Ok(Vec::new());
        }
        let partition = self.partition_of(&workspace);
        let key_at = |i: u32| -> Option<BufferKey> {
            let block = first.block_id().checked_add(i)?;
            let rdba = Rdba::from_parts(first.file_id(), block)?;
            Some(BufferKey::new(workspace, rdba))
        };
        // ① 查池（命中即拷副本——`pin` 防腾帧、内容读锁下拷，均不经过结构闩锁）。
        let mut out: Vec<Option<Page>> = Vec::with_capacity(count as usize);
        let mut any_missing = false;
        let (handle, base) = {
            for i in 0..count {
                let key = key_at(i).ok_or(BufferError::Unresolved { rdba: first })?;
                match self.copy_if_resident(key) {
                    Some(page) => out.push(Some(page)),
                    None => {
                        out.push(None);
                        any_missing = true;
                    }
                }
            }
            if !any_missing {
                return Ok(out.into_iter().flatten().collect());
            }
            (self.resolve)(&workspace, first).ok_or(BufferError::Unresolved { rdba: first })?
        };
        // ② 闩锁外：一次区读。
        let pages = pagefile::read_run(self.io, handle, base, count).map_err(|e| match e {
            PageFileError::Damaged { .. } => BufferError::Damaged { rdba: first },
            PageFileError::Io(e) => BufferError::Io(e),
        })?;
        {
            let stats = self.stats_of(partition);
            stats.inc(|s| &s.run_reads);
            stats.add(u64::from(count), |s| &s.run_pages);
        }
        // ③ 逐缺失页装入（insert_clean 自带两阶段）；副本**以池内为准**
        //    （期间可能已有先到者）。
        for (i, page) in pages.into_iter().enumerate() {
            if out[i].is_none() {
                let key = key_at(i as u32).ok_or(BufferError::Unresolved { rdba: first })?;
                self.insert_clean(key, page)?;
                let copy = match self.copy_if_resident(key) {
                    Some(p) => p,
                    None => {
                        // 刚装入就被淘汰（容量压力）：回退到 pin 拷副本。
                        let guard = self.pin(key)?;
                        (*guard).clone()
                    }
                };
                out[i] = Some(copy);
            }
        }
        Ok(out.into_iter().flatten().collect())
    }

    /// 写回某一页（若脏）。返回是否真的写了。
    pub fn flush(&self, key: BufferKey) -> Result<bool, BufferError> {
        let partition = self.partition_of(&key.workspace);
        Ok(matches!(
            self.write_back_step(partition, WriteTarget::Key(key))?,
            Some(true)
        ))
    }

    /// **按序写回一个工作区的全部脏页**（写列表头 → 尾）。
    pub fn flush_workspace(&self, workspace: [u8; 8]) -> Result<FlushReport, BufferError> {
        let partition = self.partition_of(&workspace);
        let mut report = FlushReport::default();
        loop {
            match self.write_back_step(partition, WriteTarget::WorkspaceHead(workspace))? {
                None => break,
                Some(wrote) => {
                    if wrote {
                        report.pages_written += 1;
                    }
                }
            }
        }
        Ok(report)
    }

    /// **排空一个工作集**（重绑定协议的 Draining，详设 §7 的第 1–2 步）：
    /// ① 按写列表序刷尽该分区**全部**工作区的脏页；② 丢弃全部净帧
    /// （页缓冲释放、帧回"未分配"态）。两步之间有脏帧/钉住帧 ⇒
    /// [`BufferError::DrainBlocked`]（**不静默丢帧**）。
    ///
    /// 之后调用方执行 Rebinding（更新节点组/绑定），本集下次装入的帧内存
    /// **首次触碰落在新节点**——**不搬内存**（缓冲是副本，可重建）。
    pub fn drain_partition(&self, partition: usize) -> Result<DrainReport, BufferError> {
        let mut report = DrainReport::default();
        // ① 刷尽：写列表按（首次变脏 LSN, rdba）升序——循环取最老头即全序。
        loop {
            match self.write_back_step(partition, WriteTarget::OldestHead)? {
                None => break,
                Some(wrote) => {
                    if wrote {
                        report.pages_written += 1;
                    }
                }
            }
        }
        // ② 丢净帧。
        report.frames_dropped = self.drop_clean_frames(partition)?;
        Ok(report)
    }

    /// **丢弃一个工作集的全部净帧**（Draining ②）：干净、未钉住的帧逐个
    /// 释放（摘链、清桶、**释放页缓冲**），帧回到"未分配"态以便重新落位。
    /// 有脏帧或钉住帧 ⇒ [`BufferError::DrainBlocked`]（先 `drain_partition`
    /// 的 ①，且重绑定应在**无在途会话**时进行）。
    ///
    /// **O2 起 `pinned` 判据是真判据**：卫兵不再持分区闩锁 ⇒ 别人可以在有人
    /// 钉着时进来——钉住的帧一律拒绝丢弃（宁拒不丢）。
    pub fn drop_clean_frames(&self, partition: usize) -> Result<usize, BufferError> {
        let slots = self.slots(partition);
        let mut st = self.lock(partition);
        let dirty = st.meta.iter().filter(|f| f.dirty).count();
        let pinned = slots
            .iter()
            .filter(|s| s.pins.load(Ordering::Acquire) > 0)
            .count();
        if dirty > 0 || pinned > 0 {
            return Err(BufferError::DrainBlocked { dirty, pinned });
        }
        let mut dropped = 0usize;
        for idx in 0..st.meta.len() {
            if st.meta[idx].key.is_some() {
                self.release_frame(partition, &mut st, idx);
                dropped += 1;
            }
        }
        Ok(dropped)
    }

    /// 一个分区里**已分配页缓冲**的帧数（诊断；重绑定前后核对
    /// "净帧的内存确实释放了"）。
    #[must_use]
    pub fn allocated_frames(&self, partition: usize) -> usize {
        // 已分配 = 内容槽里有页；被写锁持有时必然已分配（`try_read` 失败按已分配计）。
        self.slots(partition)
            .iter()
            .filter(|s| s.content.try_read().map_or(true, |c| c.is_some()))
            .count()
    }

    /// **Make Free**（§5.10 的 MKFREE 流程内联版）：写列表头按序写回一批；
    /// **闩锁内只选页与收尾，I/O 在闩外**。返回是否实际写过页。
    fn make_free(&self, partition: usize) -> Result<bool, BufferError> {
        let batch = {
            let st = self.lock(partition);
            (st.meta.len() / 64).max(1)
        };
        let mut steps = 0usize;
        let mut wrote_any = false;
        while steps < batch {
            match self.write_back_step(partition, WriteTarget::OldestHead)? {
                None => break,
                Some(wrote) => {
                    wrote_any |= wrote;
                    steps += 1;
                }
            }
        }
        Ok(wrote_any)
    }

    /// **一次写回（两阶段）**：① 闩锁内选页 + 冻结快照（含 WAL 所需信息）；
    /// ② 闩锁外 `ensure_durable` + 写页；③ 闩锁内收尾——**比对 `mod_seq`**：
    /// 期间被再改脏 ⇒ 保持脏与写列表条目（留给下一轮），否则清脏出列表。
    ///
    /// 返回：`None` = 无候选；`Some(false)` = 清理了失步条目（有进展、没写页）；
    /// `Some(true)` = 写了一页。
    fn write_back_step(
        &self,
        partition: usize,
        target: WriteTarget,
    ) -> Result<Option<bool>, BufferError> {
        let slots = self.slots(partition);
        // ① 闩内选页（不取内容锁——"持结构闩不等待内容锁"）。
        let (idx, key, handle, block) = {
            let mut st = self.lock(partition);
            let mut lookup = |k: BufferKey| {
                let (_s, local, g) = self.lock_bucket(partition, k);
                Self::chain_find(&g, local, k)
            };
            match st.pick_for_write(target, &*self.resolve, &mut lookup)? {
                Pick::None => return Ok(None),
                Pick::Stale => return Ok(Some(false)),
                Pick::Ready {
                    idx,
                    key,
                    handle,
                    block,
                } => (idx, key, handle, block),
            }
        };
        // ② 闩外：内容**读**锁下冻结快照。脏帧不会被腾出（腾帧只挑干净未钉住者），
        //    但仍核对页头身份——帧可能在本轮与上一轮之间被原位重装（同键镜像）。
        let job = {
            let content = content_read(&slots[idx].content);
            let Some(page) = content.as_ref() else {
                return Ok(Some(false));
            };
            let Some(header) = page.header() else {
                return Ok(Some(false)); // 页头缺失（不该发生）：不写回，留完整性路径
            };
            if header.file_id != key.rdba.file_id()
                || header.block_id != key.rdba.block_id()
                || header.workspace_ref != key.workspace
            {
                return Ok(Some(false)); // 帧已换内容（本轮与上轮之间被重装）：失步条目
            }
            let page_lsn = header.page_lsn;
            let mod_seq = header.mod_seq;
            WriteJob {
                idx,
                key,
                handle,
                block,
                image: page.clone(),
                mod_seq,
                page_lsn,
            }
        };
        let wal_synced = self.perform_write(&job)?;
        // ③ 闩内收尾：读当前 `mod_seq`（内容读锁——顺序：内容 → 结构）。
        let current_mod_seq = {
            let content = content_read(&slots[idx].content);
            content
                .as_ref()
                .and_then(|p| p.header())
                .map_or(0, |h| h.mod_seq)
        };
        let mut st = self.lock(partition);
        st.finish_write(
            slots,
            &job,
            current_mod_seq,
            wal_synced,
            self.stats_of(partition),
        );
        Ok(Some(true))
    }

    /// 写回作业的闩锁外阶段：**WAL 规则 2 → 页文件写**（§11.1；Oracle
    /// `KCBB_REDO` 的"推迟到日志同步"）。失败 ⇒ 脏状态保留（调用方不收尾）。
    fn perform_write(&self, job: &WriteJob) -> Result<bool, BufferError> {
        let mut wal_synced = false;
        {
            let wal = self.wal.lock();
            if job.page_lsn > wal.durable_lsn() {
                wal.ensure_durable(job.page_lsn)
                    .map_err(BufferError::WalFlush)?;
                wal_synced = true;
            }
        }
        let mut image = job.image.clone();
        pagefile::write_page(self.io, job.handle, job.block, &mut image)
            .map_err(BufferError::Io)?;
        Ok(wal_synced)
    }

    /// 取某分区的**结构闩锁**（`partition` 由 [`BufferPool::partition_of`] 给出）。
    fn lock(&self, partition: usize) -> LatchGuard<'_, Structure> {
        self.partitions[partition].structure.lock()
    }

    /// 按工作区取它所在分区的结构闩锁（一个工作区不被拆分 ⇒ 一次定位）。
    fn lock_of(&self, workspace: &[u8; 8]) -> LatchGuard<'_, Structure> {
        self.lock(self.partition_of(workspace))
    }

    /// 某分区的**帧槽**（内容锁 + 原子 pin + TCH；不经过闩锁）。
    fn slots(&self, partition: usize) -> &[FrameSlot] {
        &self.partitions[partition].slots
    }

    /// 某分区的**分片统计**（注意与公开的 [`BufferPool::stats`] 聚合口区分）。
    fn stats_of(&self, partition: usize) -> &StatsShards {
        &self.partitions[partition].stats
    }

    /// 锁某键所在的**桶分片**：返回（分片号, 局部桶号, 卫兵）。
    ///
    /// **次序**：调用者若持结构闩锁，这里是"结构 → 桶"的正向序；持桶闩时
    /// **不得**再取结构闩锁（只准 `try_lock`）——见 [`BucketShard`] 的规则。
    fn lock_bucket(
        &self,
        partition: usize,
        key: BufferKey,
    ) -> (usize, usize, LatchGuard<'_, BucketShard>) {
        let cfg = &self.partitions[partition].cfg;
        let b = bucket_of(cfg, key);
        let s = shard_of(cfg, b);
        let local = local_bucket(cfg, b);
        (s, local, self.partitions[partition].buckets[s].lock())
    }

    /// 桶链上找帧（**调用者须持对应桶闩**；键随链走 ⇒ 不读结构闩下的元数据）。
    fn chain_find(shard: &BucketShard, local: usize, key: BufferKey) -> Option<usize> {
        shard.chains[local]
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, i)| *i)
    }

    /// 桶链上挂帧（**须持对应桶闩**）。
    fn chain_push(shard: &mut BucketShard, local: usize, key: BufferKey, idx: usize) {
        shard.chains[local].push((key, idx));
    }

    /// **命中路径的尽力提升**（O3）：拿不到结构闩锁就作罢——提升是启发式
    /// （下次命中或扫描者会补），**绝不在此阻塞**（持桶闩时只准 `try_lock`）。
    fn try_promote(&self, partition: usize, idx: usize) {
        if let Some(mut st) = self.partitions[partition].structure.try_lock() {
            st.promote(idx, self.slots(partition), self.stats_of(partition));
        }
    }

    /// 桶链上摘帧（**须持对应桶闩**）；返回是否摘到。
    fn chain_remove(shard: &mut BucketShard, local: usize, key: BufferKey, idx: usize) -> bool {
        let chain = &mut shard.chains[local];
        if let Some(p) = chain.iter().position(|&(k, i)| k == key && i == idx) {
            chain.remove(p);
            true
        } else {
            false
        }
    }

    /// 某分区的结构闩锁（卫兵持有它以便做元数据操作）。
    fn structure(&self, partition: usize) -> &Latch<Structure> {
        &self.partitions[partition].structure
    }

    /// **闩锁统计**（诊断：gets/immediate/spin/sleeps/wait_ns——
    /// `V$LATCH` 口径；证据包 `latch-mech-20261005/`）。
    /// 多分区时是**全部工作集之和**（逐分区的细目见
    /// [`BufferPool::partition_latch_stats`]）。
    #[must_use]
    pub fn latch_stats(&self) -> LatchStats {
        let mut out = LatchStats {
            name: "db_cache",
            gets: 0,
            immediate: 0,
            spin_gets: 0,
            sleeps: 0,
            wait_ns: 0,
        };
        for p in &self.partitions {
            let mut merge = |s: LatchStats| {
                out.gets += s.gets;
                out.immediate += s.immediate;
                out.spin_gets += s.spin_gets;
                out.sleeps += s.sleeps;
                out.wait_ns += s.wait_ns;
            };
            merge(p.structure.stats());
            for shard in &p.buckets {
                merge(shard.stats()); // O3：桶闩锁并入同一判读口径
            }
        }
        out
    }
}

impl Structure {
    fn capacity_used(&self) -> usize {
        self.meta.len() - self.virgin.len()
    }

    /// **冷→热提升**（触摸计数达热判据 ⇒ 换链；热段超限 ⇒ 尾部退回冷段）。
    ///
    /// **O3**：计数在 [`FrameSlot::touches`]（原子、命中路径直接更新）——本函数
    /// 只做链的搬迁，可由**持结构闩者**调用，也可由命中路径 `try_lock` 后调用
    /// （拿不到就作罢：提升是启发式，下次命中或扫描者会补）。
    fn promote(&mut self, idx: usize, slots: &[FrameSlot], stats: &StatsShards) {
        if slots[idx].touches.load(Ordering::Relaxed) < self.cfg.hot_criteria {
            return;
        }
        if let Some(p) = self.cold.iter().position(|&i| i == idx) {
            self.cold.remove(p);
            self.hot.push_front(idx);
            slots[idx]
                .touches
                .store(self.cfg.stay_count, Ordering::Relaxed);
            stats.inc(|s| &s.hot_moved);
            let hot_max = (self.meta.len() / self.cfg.hot_fraction).max(1);
            while self.hot.len() > hot_max {
                if let Some(back) = self.hot.pop_back() {
                    slots[back]
                        .touches
                        .store(self.cfg.cool_count, Ordering::Relaxed);
                    self.cold.push_front(back);
                }
            }
        }
    }

    /// 找一个可复用帧：**AUX 优先**（干净候选），再扫**冷段尾**（跳过钉住与脏）。
    ///
    /// **只选不摘**：选中者从链上的移除由 [`Inner::attach`] 完成（成功读入 +
    /// 身份核对之后）。这样"resolve 失败 / 页损坏 / 身份不符"的读入失败不会
    /// 把帧丢在任何链之外——容量不会随失败单调泄漏。（选与摘同处**一个**
    /// 闩锁临界区——读盘的 I/O 在闩外，但"选空闲帧 → attach"不再跨临界区，
    /// 之间没有并发窗口。）
    fn find_reusable(&mut self, slots: &[FrameSlot], stats: &StatsShards) -> Option<usize> {
        // 0) 从未用过的帧最便宜（不在任何链上，attach 时无需摘链）。
        if let Some(idx) = self.virgin.pop() {
            return Some(idx);
        }
        // 1) AUX：干净、未钉住者直接取用（**复用**——计入 evictions）。
        //    **脏帧必须排除**：AUX 的语义是"写完的干净候选"，而 pin 命中与
        //    `mark_dirty` 都不会把帧移出 AUX——漏了这个判据，再次改脏的帧会被
        //    前台无写回直接覆盖（已提交更新静默丢失 + 写列表孤儿）。
        if let Some(&idx) = self
            .aux
            .iter()
            .find(|&&i| slots[i].pins.load(Ordering::Acquire) == 0 && !self.meta[i].dirty)
        {
            stats.inc(|s| &s.free_inspected);
            stats.inc(|s| &s.evictions);
            return Some(idx);
        }
        // 2) 冷段尾：遇到脏帧计数跳过（它们在写列表里排队），上限 = 容量/分数。
        let limit = (self.meta.len() / self.cfg.max_scan_fraction).max(1);
        for k in 0..self.cold.len().min(limit) {
            let idx = self.cold[self.cold.len() - 1 - k];
            stats.inc(|s| &s.free_inspected);
            if slots[idx].pins.load(Ordering::Acquire) > 0 {
                stats.inc(|s| &s.pinned_inspected);
                continue;
            }
            if self.meta[idx].dirty {
                stats.inc(|s| &s.dirty_inspected);
                continue; // 脏帧不直接写回——交 Make Free 按序写
            }
            // **老化减半**（Note 104937.1）：计数高于冷却值 ⇒ 不立即淘汰，
            // 减半后继续扫——"计数够高的块即使位于列表尾也不被重用"。
            let t = slots[idx].touches.load(Ordering::Relaxed);
            if t > self.cfg.cool_count {
                slots[idx].touches.store(t / 2, Ordering::Relaxed);
                stats.inc(|s| &s.aging_steps);
                continue;
            }
            stats.inc(|s| &s.evictions);
            return Some(idx);
        }
        None
    }

    /// **选一个写回候选**（闩锁内；不写盘）：
    /// - `Key`：指定页（`flush` 用；不存在/不脏 ⇒ `None`）；
    /// - `WorkspaceHead`：该工作区写列表头；
    /// - `OldestHead`：所有工作区里"最老首次变脏 LSN"最小的头（Make Free）。
    ///
    /// 失步条目（帧已不在池中/已干净）就地清理并返回 [`Pick::Stale`]——按
    /// **条目自己的 LSN** 删除（用 `Lsn(0)` 当键删不掉 ⇒ 死循环，前台挂起）。
    fn pick_for_write(
        &mut self,
        target: WriteTarget,
        resolve: &PoolResolver<'_>,
        lookup: &mut dyn FnMut(BufferKey) -> Option<usize>,
    ) -> Result<Pick, BufferError> {
        let candidate: Option<(Lsn, [u8; 8], Rdba)> = match target {
            WriteTarget::Key(key) => {
                let Some(idx) = lookup(key) else {
                    return Ok(Pick::None);
                };
                if !self.meta[idx].dirty {
                    return Ok(Pick::None);
                }
                self.meta[idx]
                    .first_dirty
                    .map(|l| (l, key.workspace, key.rdba))
            }
            WriteTarget::WorkspaceHead(ws) => self
                .write_list
                .get(&ws)
                .and_then(|c| c.iter().next().copied())
                .map(|(l, r)| (l, ws, r)),
            WriteTarget::OldestHead => {
                let mut best: Option<(Lsn, [u8; 8], Rdba)> = None;
                for (ws, chain) in &self.write_list {
                    if let Some((lsn, rdba)) = chain.iter().next().copied() {
                        if best.map_or(true, |(l, _, _)| lsn < l) {
                            best = Some((lsn, *ws, rdba));
                        }
                    }
                }
                best
            }
        };
        let Some((lsn, ws, rdba)) = candidate else {
            return Ok(Pick::None);
        };
        let key = BufferKey::new(ws, rdba);
        let Some(idx) = lookup(key) else {
            self.drop_write_entry(ws, lsn, rdba);
            return Ok(Pick::Stale);
        };
        if !self.meta[idx].dirty {
            self.drop_write_entry(ws, lsn, rdba); // 防呆（不应发生）
            return Ok(Pick::Stale);
        }
        let Some(fkey) = self.meta[idx].key else {
            return Ok(Pick::None);
        };
        let (handle, block) = resolve(&fkey.workspace, fkey.rdba)
            .ok_or(BufferError::Unresolved { rdba: fkey.rdba })?;
        Ok(Pick::Ready {
            idx,
            key: fkey,
            handle,
            block,
        })
    }

    fn drop_write_entry(&mut self, ws: [u8; 8], lsn: Lsn, rdba: Rdba) {
        if let Some(chain) = self.write_list.get_mut(&ws) {
            chain.remove(&(lsn, rdba));
            if chain.is_empty() {
                self.write_list.remove(&ws);
            }
        }
    }

    /// **写回收尾**（闩锁内）：`mod_seq` 未变 ⇒ 清脏、出写列表、干净未钉住
    /// 帧入 AUX；**变过（期间被再改脏）⇒ 保持脏与条目**，留给下一轮——
    /// 绝不把"更新过的版本"当"已落盘"（PG `BM_JUST_DIRTIED` 的同款判据）。
    ///
    /// `current_mod_seq` 由调用方**在闩锁外**取（内容读锁下读页头）——
    /// 保持"持结构闩不等待内容锁"的纪律。
    fn finish_write(
        &mut self,
        slots: &[FrameSlot],
        job: &WriteJob,
        current_mod_seq: u8,
        wal_synced: bool,
        stats: &StatsShards,
    ) {
        stats.inc(|s| &s.writes);
        if wal_synced {
            stats.inc(|s| &s.wal_syncs);
        }
        let idx = job.idx;
        if self.meta[idx].key != Some(job.key) {
            return; // 帧已换人（脏帧不可被淘汰；防御）
        }
        if current_mod_seq != job.mod_seq {
            return; // 期间被再改脏：保持脏
        }
        if let Some(lsn) = self.meta[idx].first_dirty.take() {
            self.drop_write_entry(job.key.workspace, lsn, job.key.rdba);
        }
        self.meta[idx].dirty = false;
        if slots[idx].pins.load(Ordering::Acquire) == 0 && !self.aux.contains(&idx) {
            if let Some(p) = self.hot.iter().position(|&i| i == idx) {
                self.hot.remove(p);
            }
            if let Some(p) = self.cold.iter().position(|&i| i == idx) {
                self.cold.remove(p);
            }
            self.aux.push_front(idx);
            stats.inc(|s| &s.aux_moved);
        }
    }

    /// 从三条链里去重移除（顺序：热 → 冷 → AUX）。
    fn detach_from_chains(
        hot: &mut VecDeque<usize>,
        cold: &mut VecDeque<usize>,
        aux: &mut VecDeque<usize>,
        idx: usize,
    ) {
        for chain in [hot, cold, aux] {
            if let Some(p) = chain.iter().position(|&i| i == idx) {
                chain.remove(p);
            }
        }
    }
}

/// 取内容**写**卫兵（中毒不级联——闩锁语义：panic 后的数据交给下一次使用者发现）。
fn content_write(content: &RwLock<Option<Page>>) -> RwLockWriteGuard<'_, Option<Page>> {
    content.write().unwrap_or_else(|e| e.into_inner())
}

/// 取内容**读**卫兵（同上）。
fn content_read(content: &RwLock<Option<Page>>) -> RwLockReadGuard<'_, Option<Page>> {
    content.read().unwrap_or_else(|e| e.into_inner())
}

/// 从内容卫兵取页（"键在位 ⇔ 页在位"不变量）。
fn guard_page(content: &Option<Page>) -> &Page {
    content.as_ref().expect("已装入的帧必有页缓冲")
}

/// **页卫兵（独占）**：钉住一帧 + 持内容**写**锁。
///
/// **O2（§5.10）**：卫兵**不持分区闩锁**——"一次一个卫兵"纪律退役（同线程
/// 可以同时持多个卫兵、持卫兵期间照常调池方法）。元数据操作（`mark_dirty`/
/// `key`/`is_dirty`）各自**短临界区**取结构闩锁。
///
/// `Drop` **先放内容锁、再解 pin**——不得反过来：腾帧者以 `pins = 0` 为前提，
/// 若先解 pin 而我们仍持内容锁，腾帧者会认领一个"取不到内容"的帧。
pub struct PageGuard<'a> {
    /// 内容写卫兵（`Option` 只为 `Drop` 里能显式提前释放）。
    content: Option<RwLockWriteGuard<'a, Option<Page>>>,
    /// 帧的 pin 计数（`Drop` 递减）。
    pins: &'a AtomicU32,
    /// 分区的结构闩锁（元数据操作；**临界区短**，不嵌套内容锁的等待）。
    structure: &'a Latch<Structure>,
    /// 本帧的键（构造时快照——卫兵期间帧不会被换人：pin > 0）。
    key: BufferKey,
    idx: usize,
}

impl std::fmt::Debug for PageGuard<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PageGuard")
            .field("key", &self.key)
            .field("idx", &self.idx)
            .finish()
    }
}

impl PageGuard<'_> {
    /// 本帧的键。
    #[must_use]
    pub fn key(&self) -> BufferKey {
        self.key
    }

    /// 页是否脏。
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.structure.lock().meta[self.idx].dirty
    }

    /// **标脏**（写路径在追加完 redo 后调用）：`first_dirty_lsn` 只在**首次**
    /// 变脏时记入——写列表按它排序，重复标脏不改变排序键。
    pub fn mark_dirty(&mut self, first_dirty_lsn: Lsn) {
        let mut st = self.structure.lock();
        let m = &mut st.meta[self.idx];
        if m.dirty {
            return;
        }
        m.dirty = true;
        m.first_dirty = Some(first_dirty_lsn);
        let key = m.key.expect("钉住的帧必有主");
        st.write_list
            .entry(key.workspace)
            .or_default()
            .insert((first_dirty_lsn, key.rdba));
    }
}

impl std::ops::Deref for PageGuard<'_> {
    type Target = Page;
    fn deref(&self) -> &Page {
        guard_page(self.content.as_ref().expect("内容卫兵在"))
    }
}

impl std::ops::DerefMut for PageGuard<'_> {
    fn deref_mut(&mut self) -> &mut Page {
        self.content
            .as_mut()
            .expect("内容卫兵在")
            .as_mut()
            .expect("已装入的帧必有页缓冲")
    }
}

impl Drop for PageGuard<'_> {
    fn drop(&mut self) {
        self.content.take(); // 先放内容锁
        self.pins.fetch_sub(1, Ordering::AcqRel);
    }
}

/// **页读卫兵（共享）**：钉住一帧 + 持内容**读**锁——同一热块的并发读互不
/// 串行（§5.10 O2 的目标）。`Drop` 次序同 [`PageGuard`]。
pub struct PageReadGuard<'a> {
    content: Option<RwLockReadGuard<'a, Option<Page>>>,
    pins: &'a AtomicU32,
}

impl std::fmt::Debug for PageReadGuard<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PageReadGuard").finish()
    }
}

impl std::ops::Deref for PageReadGuard<'_> {
    type Target = Page;
    fn deref(&self) -> &Page {
        guard_page(self.content.as_ref().expect("内容卫兵在"))
    }
}

impl Drop for PageReadGuard<'_> {
    fn drop(&mut self) {
        self.content.take();
        self.pins.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use bicdb_workspace::io::{MemFileIo, OpenOptions};

    use super::*;
    use crate::page::PageType;

    fn rdba(file_id: u16, block: u32) -> Rdba {
        Rdba::from_parts(file_id, block).unwrap()
    }

    const WS_A: [u8; 8] = [1u8; 8];
    const WS_B: [u8; 8] = [2u8; 8];
    const F_A: &str = "/mem/a.dat";
    const F_B: &str = "/mem/b.dat";

    fn lsn(v: u64) -> Lsn {
        Lsn::from_raw(v).unwrap()
    }

    /// 记录 I/O 事件到共享日志（与假 WAL 共用一份，断言**次序**）。
    struct RecordingIo {
        inner: MemFileIo,
        log: Arc<Mutex<Vec<String>>>,
    }

    impl FileIo for RecordingIo {
        fn open(&self, path: &Path, opts: OpenOptions) -> std::io::Result<FileHandle> {
            self.inner.open(path, opts)
        }
        fn open_dir(&self, path: &Path) -> std::io::Result<FileHandle> {
            self.inner.open_dir(path)
        }
        fn read_at(&self, h: FileHandle, buf: &mut [u8], off: u64) -> std::io::Result<usize> {
            self.inner.read_at(h, buf, off)
        }
        fn write_at(&self, h: FileHandle, buf: &[u8], off: u64) -> std::io::Result<()> {
            self.log.lock().unwrap().push(format!("io:write:{off}"));
            self.inner.write_at(h, buf, off)
        }
        fn size(&self, h: FileHandle) -> std::io::Result<u64> {
            self.inner.size(h)
        }
        fn set_len(&self, h: FileHandle, len: u64) -> std::io::Result<()> {
            self.inner.set_len(h, len)
        }
        fn sync_data(&self, h: FileHandle) -> std::io::Result<()> {
            self.log.lock().unwrap().push("io:sync".into());
            self.inner.sync_data(h)
        }
        fn sync_all(&self, h: FileHandle) -> std::io::Result<()> {
            self.inner.sync_all(h)
        }
        fn sync_dir(&self, h: FileHandle) -> std::io::Result<()> {
            self.inner.sync_dir(h)
        }
        fn close(&self, h: FileHandle) -> std::io::Result<()> {
            self.inner.close(h)
        }
    }

    /// **闸门式 I/O**：把指定 `(读/写, 偏移)` 的那次 I/O 调用**阻塞**到测试
    /// 放行——把"闩锁外的 I/O 窗口"拉成可观测窗口（O1 并发用例）。
    struct GateIo {
        inner: MemFileIo,
        gate: Mutex<GateState>,
        cv: std::sync::Condvar,
    }

    #[derive(Debug, Clone, Copy)]
    struct GateState {
        /// 拦截点：`(是否写, 字节偏移)`；`None` = 不拦。
        point: Option<(bool, u64)>,
        /// 已进入拦截点（测试据此判定"I/O 在途"）。
        entered: bool,
        /// 放行（放行后不再拦）。
        released: bool,
    }

    impl GateIo {
        fn new(inner: MemFileIo) -> Self {
            Self {
                inner,
                gate: Mutex::new(GateState {
                    point: None,
                    entered: false,
                    released: false,
                }),
                cv: std::sync::Condvar::new(),
            }
        }

        /// 设拦截点（`on_write` 区分写路径；`offset` 是页的字节偏移）。
        fn block(&self, on_write: bool, offset: u64) {
            let mut g = self.gate.lock().unwrap();
            g.point = Some((on_write, offset));
            g.entered = false;
            g.released = false;
        }

        /// 等"已有 I/O 进入拦截点"（带超时，防测试自身挂死）。
        fn wait_entered(&self) -> bool {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let mut g = self.gate.lock().unwrap();
            while !g.entered {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                if left.is_zero() {
                    return false;
                }
                let (guard, _) = self.cv.wait_timeout(g, left).unwrap();
                g = guard;
            }
            true
        }

        /// 放行拦截点。
        fn release(&self) {
            let mut g = self.gate.lock().unwrap();
            g.released = true;
            self.cv.notify_all();
        }

        /// 命中拦截点 ⇒ 阻塞到放行。
        fn pass(&self, on_write: bool, offset: u64) {
            let mut g = self.gate.lock().unwrap();
            if g.point != Some((on_write, offset)) || g.released {
                return;
            }
            g.entered = true;
            self.cv.notify_all();
            while !g.released {
                g = self.cv.wait(g).unwrap();
            }
            g.point = None;
        }
    }

    impl FileIo for GateIo {
        fn open(&self, path: &Path, opts: OpenOptions) -> std::io::Result<FileHandle> {
            self.inner.open(path, opts)
        }
        fn open_dir(&self, path: &Path) -> std::io::Result<FileHandle> {
            self.inner.open_dir(path)
        }
        fn read_at(&self, h: FileHandle, buf: &mut [u8], off: u64) -> std::io::Result<usize> {
            self.pass(false, off);
            self.inner.read_at(h, buf, off)
        }
        fn write_at(&self, h: FileHandle, buf: &[u8], off: u64) -> std::io::Result<()> {
            self.pass(true, off);
            self.inner.write_at(h, buf, off)
        }
        fn size(&self, h: FileHandle) -> std::io::Result<u64> {
            self.inner.size(h)
        }
        fn set_len(&self, h: FileHandle, len: u64) -> std::io::Result<()> {
            self.inner.set_len(h, len)
        }
        fn sync_data(&self, h: FileHandle) -> std::io::Result<()> {
            self.inner.sync_data(h)
        }
        fn sync_all(&self, h: FileHandle) -> std::io::Result<()> {
            self.inner.sync_all(h)
        }
        fn sync_dir(&self, h: FileHandle) -> std::io::Result<()> {
            self.inner.sync_dir(h)
        }
        fn close(&self, h: FileHandle) -> std::io::Result<()> {
            self.inner.close(h)
        }
    }

    /// 两个页文件（A：file_id 7 / 块 0-1；B：file_id 8 / 块 0-1），
    /// 内容字节 = `0xA0+块` / `0xB0+块`——供需要定制 `FileIo` 的用例复用。
    fn open_files(io: &dyn FileIo) -> (FileHandle, FileHandle) {
        let a = io
            .open(
                Path::new(F_A),
                OpenOptions::new().read(true).write(true).create_new(true),
            )
            .unwrap();
        io.set_len(a, 2 * crate::page::PAGE_SIZE as u64).unwrap();
        let b = io
            .open(
                Path::new(F_B),
                OpenOptions::new().read(true).write(true).create_new(true),
            )
            .unwrap();
        io.set_len(b, 2 * crate::page::PAGE_SIZE as u64).unwrap();
        for block in 0..2u32 {
            for (ws, file_id, handle, base) in [(WS_A, 7u16, a, 0xA0u8), (WS_B, 8, b, 0xB0)] {
                let mut page = Page::new(PageType::HeapTable, ws, file_id, block);
                page.as_bytes_mut()[4096] = base + block as u8;
                pagefile::write_page(io, handle, block, &mut page).unwrap();
            }
        }
        (a, b)
    }

    /// 假 WAL 协调口。
    struct FakeWal {
        durable: std::sync::atomic::AtomicU64,
        fail: bool,
        log: Arc<Mutex<Vec<String>>>,
    }

    impl WalGuard for FakeWal {
        fn durable_lsn(&self) -> Lsn {
            Lsn::from_raw(self.durable.load(std::sync::atomic::Ordering::SeqCst)).unwrap()
        }
        fn ensure_durable(&self, target: Lsn) -> std::io::Result<()> {
            if self.fail {
                return Err(std::io::Error::other("假刷盘失败"));
            }
            self.log
                .lock()
                .unwrap()
                .push(format!("wal:ensure:{}", target.as_raw()));
            self.durable
                .fetch_max(target.as_raw(), std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    struct Harness {
        io: RecordingIo,
        log: Arc<Mutex<Vec<String>>>,
        a: FileHandle,
        b: FileHandle,
    }

    /// 双向夹具：文件 A（file_id 7，工作区 A）与文件 B（file_id 8，工作区 B），
    /// 各两页。
    fn harness() -> Harness {
        let mem = MemFileIo::new();
        mem.add_dir("/mem");
        let log = Arc::new(Mutex::new(Vec::new()));
        let io = RecordingIo {
            inner: mem,
            log: Arc::clone(&log),
        };
        let a = io
            .open(
                Path::new(F_A),
                OpenOptions::new().read(true).write(true).create_new(true),
            )
            .unwrap();
        io.set_len(a, 2 * crate::page::PAGE_SIZE as u64).unwrap();
        let b = io
            .open(
                Path::new(F_B),
                OpenOptions::new().read(true).write(true).create_new(true),
            )
            .unwrap();
        io.set_len(b, 2 * crate::page::PAGE_SIZE as u64).unwrap();
        let mut h = Harness { io, log, a, b };
        for block in 0..2u32 {
            h.put_page(WS_A, 7, block, 0xA0 + block as u8);
            h.put_page(WS_B, 8, block, 0xB0 + block as u8);
        }
        h
    }

    impl Harness {
        fn put_page(&mut self, ws: [u8; 8], file_id: u16, block: u32, byte: u8) {
            let handle = if file_id == 7 { self.a } else { self.b };
            let mut page = Page::new(PageType::HeapTable, ws, file_id, block);
            page.as_bytes_mut()[4096] = byte;
            pagefile::write_page(&self.io, handle, block, &mut page).unwrap();
        }
        fn pool<'io>(&'io self, capacity: usize, wal: impl WalGuard + 'io) -> BufferPool<'io> {
            let (a, b) = (self.a, self.b);
            BufferPool::new(
                &self.io,
                capacity,
                move |ws, r| {
                    if *ws == WS_A && r.file_id() == 7 {
                        Some((a, r.block_id()))
                    } else if *ws == WS_B && r.file_id() == 8 {
                        Some((b, r.block_id()))
                    } else {
                        None
                    }
                },
                wal,
            )
            .unwrap()
        }
        fn fake_wal(&self) -> FakeWal {
            FakeWal {
                durable: std::sync::atomic::AtomicU64::new(0),
                fail: false,
                log: Arc::clone(&self.log),
            }
        }
        fn events(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
        fn clear_events(&self) {
            self.log.lock().unwrap().clear();
        }
        fn read_byte(&self, file_id: u16, block: u32) -> u8 {
            let handle = if file_id == 7 { self.a } else { self.b };
            let page = pagefile::read_page_verified(&self.io, handle, block).unwrap();
            page.as_bytes()[4096]
        }
        fn set_page_lsn(&self, page: &mut Page, v: u64) {
            let mut h = page.header().unwrap();
            h.page_lsn = lsn(v);
            page.write_header(&h);
        }
    }

    #[test]
    fn pin_reads_on_miss_then_hits() {
        let h = harness();
        let pool = h.pool(4, h.fake_wal());
        let k = BufferKey::new(WS_A, rdba(7, 0));
        {
            let g = pool.pin(k).unwrap();
            assert_eq!(g.as_bytes()[4096], 0xA0, "内容来自文件");
        }
        {
            let _g = pool.pin(k).unwrap();
        }
        let s = pool.stats();
        assert_eq!((s.misses, s.hits), (1, 1));
        assert_eq!(pool.resident(), 1);
    }

    #[test]
    fn identity_mismatch_is_rejected() {
        let h = harness();
        let pool = h.pool(4, h.fake_wal());
        // 请求 (7,0) 但把页头的工作区改成 B——串页防线必须响亮失败。
        let mut page = {
            let g = pool.pin(BufferKey::new(WS_A, rdba(7, 0))).unwrap();
            Page::from_bytes(Box::new(*g.as_bytes()))
        };
        drop(pool);
        let mut header = page.header().unwrap();
        header.workspace_ref = WS_B;
        page.write_header(&header);
        pagefile::write_page(&h.io, h.a, 0, &mut page).unwrap();

        let pool = h.pool(4, h.fake_wal());
        assert!(matches!(
            pool.pin(BufferKey::new(WS_A, rdba(7, 0))),
            Err(BufferError::IdentityMismatch { .. })
        ));
    }

    #[test]
    fn dirty_eviction_writes_back_after_wal_flush() {
        let h = harness();
        let pool = h.pool(1, h.fake_wal());
        {
            let mut g = pool.pin(BufferKey::new(WS_A, rdba(7, 0))).unwrap();
            g.as_bytes_mut()[4096] = 0xEE;
            h.set_page_lsn(&mut g, 7);
            g.mark_dirty(lsn(7));
        }
        h.clear_events();
        // 容量 1：读第二页必须淘汰第一页（脏 → 先 WAL 规则 2 再写）。
        {
            let _g = pool.pin(BufferKey::new(WS_A, rdba(7, 1))).unwrap();
        }
        assert_eq!(h.events(), vec!["wal:ensure:7", "io:write:0"]);
        assert_eq!(h.read_byte(7, 0), 0xEE, "脏页已写回");
        let s = pool.stats();
        assert_eq!((s.evictions, s.writes, s.wal_syncs), (1, 1, 1));
    }

    #[test]
    fn wal_failure_blocks_write_back() {
        let h = harness();
        let mut wal = h.fake_wal();
        wal.fail = true; // 刷盘必失败——页就不该写出
        let pool = h.pool(1, wal);
        {
            let mut g = pool.pin(BufferKey::new(WS_A, rdba(7, 0))).unwrap();
            g.as_bytes_mut()[4096] = 0xEE;
            h.set_page_lsn(&mut g, 9);
            g.mark_dirty(lsn(9));
        }
        // 容量 1：读第二页要淘汰脏页 → WAL 规则 2 刷盘失败 ⇒ 整体失败。
        let err = pool.pin(BufferKey::new(WS_A, rdba(7, 1))).unwrap_err();
        assert!(matches!(err, BufferError::WalFlush(_)), "{err}");
        assert_eq!(h.read_byte(7, 0), 0xA0, "页**没有**写出");
        assert_eq!(pool.dirty_len(WS_A), 1, "脏状态保留");
        assert_eq!(pool.low_water(WS_A), Some(lsn(9)));
    }

    #[test]
    fn clean_eviction_writes_nothing() {
        let h = harness();
        let pool = h.pool(1, h.fake_wal());
        {
            let _g = pool.pin(BufferKey::new(WS_A, rdba(7, 0))).unwrap();
        }
        h.clear_events();
        {
            let _g = pool.pin(BufferKey::new(WS_A, rdba(7, 1))).unwrap();
        }
        assert!(h.events().is_empty(), "干净页淘汰不产生任何 I/O");
        assert_eq!(pool.stats().writes, 0);
    }

    #[test]
    fn dirty_chain_is_ordered_and_low_water_tracks_oldest() {
        let h = harness();
        let pool = h.pool(4, h.fake_wal());
        {
            let mut g0 = pool.pin(BufferKey::new(WS_A, rdba(7, 0))).unwrap();
            h.set_page_lsn(&mut g0, 9);
            g0.mark_dirty(lsn(9));
        }
        {
            let mut g1 = pool.pin(BufferKey::new(WS_A, rdba(7, 1))).unwrap();
            h.set_page_lsn(&mut g1, 4);
            g1.mark_dirty(lsn(4));
        }
        assert_eq!(pool.dirty_len(WS_A), 2);
        assert_eq!(
            pool.low_water(WS_A),
            Some(lsn(4)),
            "低水位 = 最老的首次变脏 LSN"
        );

        // 写掉较新的一页，低水位不动。
        assert!(pool.flush(BufferKey::new(WS_A, rdba(7, 0))).unwrap());
        assert_eq!(pool.low_water(WS_A), Some(lsn(4)));
        assert_eq!(pool.dirty_len(WS_A), 1);
    }

    #[test]
    fn flush_workspace_writes_in_first_dirty_order() {
        let h = harness();
        let pool = h.pool(4, h.fake_wal());
        {
            let mut g0 = pool.pin(BufferKey::new(WS_A, rdba(7, 0))).unwrap();
            h.set_page_lsn(&mut g0, 9);
            g0.mark_dirty(lsn(9));
        }
        {
            let mut g1 = pool.pin(BufferKey::new(WS_A, rdba(7, 1))).unwrap();
            h.set_page_lsn(&mut g1, 4);
            g1.mark_dirty(lsn(4));
        }
        h.clear_events();
        let report = pool.flush_workspace(WS_A).unwrap();
        assert_eq!(report.pages_written, 2);
        assert_eq!(
            h.events(),
            vec![
                "wal:ensure:4",
                "io:write:16384",
                "wal:ensure:9",
                "io:write:0"
            ],
            "按首次变脏 LSN 升序写；每页写前先满足 WAL 规则 2"
        );
        assert_eq!(pool.dirty_len(WS_A), 0);
        assert_eq!(pool.low_water(WS_A), None);
    }

    #[test]
    fn workspaces_have_separate_frames_and_chains() {
        let h = harness();
        let pool = h.pool(4, h.fake_wal());
        // 单闩锁形态：卫兵要逐个放开（§5.10 的 N = 1 默认）。
        {
            let _a = pool.pin(BufferKey::new(WS_A, rdba(7, 0))).unwrap();
        }
        {
            let mut b = pool.pin(BufferKey::new(WS_B, rdba(8, 0))).unwrap();
            h.set_page_lsn(&mut b, 2);
            b.mark_dirty(lsn(2));
        }
        assert_eq!(pool.resident(), 2, "同块号不同工作区 = 两帧");
        assert_eq!(pool.dirty_len(WS_A), 0);
        assert_eq!(pool.dirty_len(WS_B), 1, "脏链按工作区分");
    }

    /// 手动时钟（三秒规则的可控测试）。
    struct ManualClock(Arc<Mutex<u64>>);
    impl Clock for ManualClock {
        fn now_ms(&self) -> u64 {
            *self.0.lock().unwrap()
        }
    }

    impl Harness {
        fn pool_with(
            &self,
            capacity: usize,
            clock: Arc<Mutex<u64>>,
            cfg: CacheConfig,
        ) -> BufferPool<'_> {
            let (a, b) = (self.a, self.b);
            BufferPool::with_config(
                &self.io,
                capacity,
                move |ws, r| {
                    if *ws == WS_A && r.file_id() == 7 {
                        Some((a, r.block_id()))
                    } else if *ws == WS_B && r.file_id() == 8 {
                        Some((b, r.block_id()))
                    } else {
                        None
                    }
                },
                self.fake_wal(),
                ManualClock(clock),
                cfg,
            )
            .unwrap()
        }
    }

    #[test]
    fn hash_places_blocks_in_buckets() {
        let h = harness();
        let pool = h.pool(4, h.fake_wal());
        assert!(
            pool.bucket_count() >= 7 && pool.bucket_count() % 2 == 1,
            "质数桶数"
        );
        let k0 = BufferKey::new(WS_A, rdba(7, 0));
        let k1 = BufferKey::new(WS_A, rdba(7, 1));
        {
            let _g = pool.pin(k0).unwrap();
        }
        {
            let _g = pool.pin(k1).unwrap();
        }
        let total: usize = (0..pool.bucket_count()).map(|b| pool.bucket_len(b)).sum();
        assert_eq!(total, 2, "两个块各挂一个桶");
        // 桶号 = DBA mod 桶数（Oracle 原文口径）——直接验证放置位置。
        let bucket = (u64::from(7u16) << 28) % pool.bucket_count() as u64;
        assert_eq!(
            pool.bucket_len(bucket as usize),
            1,
            "块 0 落在 DBA 取模的桶"
        );
        assert_eq!(pool.touch_count(k0), Some(0), "TCH 从冷却值 0 起");
        // 命中计数照旧。
        {
            let _g = pool.pin(k0).unwrap();
        }
        assert_eq!(pool.stats().hits, 1);
    }

    #[test]
    fn new_frames_land_in_cold_head_and_written_frames_go_to_aux() {
        let h = harness();
        let pool = h.pool(2, h.fake_wal());
        let k0 = BufferKey::new(WS_A, rdba(7, 0));
        {
            let mut g = pool.pin(k0).unwrap();
            g.as_bytes_mut()[4096] = 0xEE;
            h.set_page_lsn(&mut g, 3);
            g.mark_dirty(lsn(3));
        }
        assert_eq!(pool.chain_of(k0), Some("cold"), "新读入落在冷段头");
        assert!(pool.flush(k0).unwrap());
        assert_eq!(pool.chain_of(k0), Some("aux"), "写完的干净帧进 AUX");
        assert_eq!(pool.stats().aux_moved, 1);
    }

    #[test]
    fn touch_count_follows_the_three_second_rule_and_promotes_to_hot() {
        let h = harness();
        let clock = Arc::new(Mutex::new(0u64));
        let pool = h.pool_with(4, Arc::clone(&clock), CacheConfig::for_capacity(4));
        let k0 = BufferKey::new(WS_A, rdba(7, 0));
        {
            let _g = pool.pin(k0).unwrap(); // 装入：touches = cool_count = 1
        }
        // 1 秒后再命中：不足 3 秒 ⇒ 计数不动、不提升。
        *clock.lock().unwrap() = 1_000;
        {
            let _g = pool.pin(k0).unwrap();
        }
        assert_eq!(pool.chain_of(k0), Some("cold"));
        assert_eq!(pool.stats().hot_moved, 0);
        // 5 秒后再命中：≥3 秒 ⇒ +1（=1），未达热判据（2）⇒ 仍在冷段。
        *clock.lock().unwrap() = 5_000;
        {
            let _g = pool.pin(k0).unwrap();
        }
        assert_eq!(pool.chain_of(k0), Some("cold"), "一次命中还不够热");
        // 10 秒后再命中：+1 达热判据 ⇒ 提升到热段。
        *clock.lock().unwrap() = 10_000;
        {
            let _g = pool.pin(k0).unwrap();
        }
        assert_eq!(pool.chain_of(k0), Some("hot"), "冷段计数达热判据升热段");
        assert_eq!(pool.stats().hot_moved, 1);
    }

    #[test]
    fn hot_overflow_demotes_the_oldest_hot_to_cold_head() {
        let h = harness();
        let clock = Arc::new(Mutex::new(0u64));
        let pool = h.pool_with(8, Arc::clone(&clock), CacheConfig::for_capacity(8));
        let keys = [
            BufferKey::new(WS_A, rdba(7, 0)),
            BufferKey::new(WS_A, rdba(7, 1)),
            BufferKey::new(WS_B, rdba(8, 0)),
        ];
        for k in keys {
            let _g = pool.pin(k).unwrap();
        }
        // 每块两次跨 3 秒命中才会提升（冷却值 0、热判据 2）。
        *clock.lock().unwrap() = 10_000;
        for k in keys {
            let _g = pool.pin(k).unwrap();
        }
        *clock.lock().unwrap() = 20_000;
        for k in keys {
            let _g = pool.pin(k).unwrap();
        }
        // 三块先后提升到热段（热段上限 = 8/4 = 2）⇒ 最早的那块被退回冷段头。
        assert_eq!(pool.chain_of(keys[2]), Some("hot"), "最后提升的在热段头");
        assert_eq!(pool.chain_of(keys[0]), Some("cold"), "最早的被退回冷段头");
        assert_eq!(pool.stats().hot_moved, 3);
    }

    #[test]
    fn make_free_writes_in_write_list_order_not_lru_order() {
        let h = harness();
        let pool = h.pool(2, h.fake_wal());
        let ka = BufferKey::new(WS_A, rdba(7, 0)); // 后变脏（lsn 9）
        let kb = BufferKey::new(WS_A, rdba(7, 1)); // 先变脏（lsn 4）
        {
            let mut g = pool.pin(ka).unwrap();
            h.set_page_lsn(&mut g, 9);
            g.mark_dirty(lsn(9));
        }
        {
            let mut g = pool.pin(kb).unwrap();
            h.set_page_lsn(&mut g, 4);
            g.mark_dirty(lsn(4));
        }
        h.clear_events();
        // 容量 2：读入两个新块，必须两次 Make Free——按**写列表序**（lsn 4 → 9）写。
        for b in 0..2u32 {
            let _g = pool.pin(BufferKey::new(WS_B, rdba(8, b))).unwrap();
        }
        assert_eq!(
            h.events(),
            vec![
                "wal:ensure:4",
                "io:write:16384",
                "wal:ensure:9",
                "io:write:0"
            ],
            "写列表头按序写（与 LRU 顺序无关）"
        );
        assert!(pool.stats().dirty_inspected >= 1);
    }

    #[test]
    fn dirty_only_frames_are_freed_via_make_free() {
        // 容量 1、唯一帧是脏的：前台腾帧必须走 Make Free（写列表头写掉），
        // 而不是"随手写受害者"。`FreeBufferWait` 在单闩锁形态不可达（钉住即持锁）
        // ——它留给多写者/分片形态（写线程持帧时）。
        let h = harness();
        let pool = h.pool(1, h.fake_wal());
        let ka = BufferKey::new(WS_A, rdba(7, 0));
        {
            let mut g = pool.pin(ka).unwrap();
            h.set_page_lsn(&mut g, 5);
            g.mark_dirty(lsn(5));
        }
        h.clear_events();
        {
            let _g = pool.pin(BufferKey::new(WS_B, rdba(8, 0))).unwrap();
        }
        assert_eq!(h.events(), vec!["wal:ensure:5", "io:write:0"]);
        assert!(pool.stats().dirty_inspected >= 1, "扫描先见到脏帧");
        assert_eq!(pool.stats().writes, 1);
        assert_eq!(pool.stats().evictions, 1);
    }

    #[test]
    fn aging_halves_touch_count_instead_of_evicting() {
        // Note 104937.1：扫描遇到计数高于冷却值的候选 ⇒ **减半后继续**，
        // 不立即淘汰；下一次扫描（计数已 ≤ 冷却值）才可复用。
        let h = harness();
        let clock = Arc::new(Mutex::new(0u64));
        let pool = h.pool_with(1, Arc::clone(&clock), CacheConfig::for_capacity(1));
        let ka = BufferKey::new(WS_A, rdba(7, 0));
        {
            let _g = pool.pin(ka).unwrap();
        }
        *clock.lock().unwrap() = 5_000;
        {
            let _g = pool.pin(ka).unwrap(); // 计数 = 1（高于冷却值 0）
        }
        // 读入新块：冷段尾的 A 计数 1 > 0 ⇒ 减半为 0、跳过；重试后才复用。
        {
            let _g = pool.pin(BufferKey::new(WS_A, rdba(7, 1))).unwrap();
        }
        assert_eq!(pool.stats().aging_steps, 1, "发生过一次老化减半");
        assert_eq!(pool.stats().evictions, 1);
        assert_eq!(pool.stats().writes, 0, "干净块不需写回");
    }

    #[test]
    fn failed_pin_does_not_leak_frames() {
        // 审核修复回归（C3）：resolve 失败的读入不得把 victim 帧丢在任何链
        // 之外——否则容量随失败单调泄漏，反复失败最终命中本不可达的
        // `FreeBufferWait`。
        let h = harness();
        let pool = h.pool(1, h.fake_wal());
        let k0 = BufferKey::new(WS_A, rdba(7, 0));
        {
            let _g = pool.pin(k0).unwrap();
        }
        // resolver 给不出的组合（工作区 B 配文件 7）。
        let bad = BufferKey::new(WS_B, rdba(7, 0));
        for _ in 0..8 {
            let err = pool.pin(bad).unwrap_err();
            assert!(
                matches!(err, BufferError::Unresolved { .. }),
                "失败仍是'定位不到'，不是'无空闲缓冲'：{err}"
            );
        }
        assert_eq!(pool.stats().fb_wait, 0, "容量未泄漏");
        // 旧键仍可命中（帧未被丢出链）。
        let g = pool.pin(k0).unwrap();
        assert_eq!(g.as_bytes()[4096], 0xA0);
    }

    #[test]
    fn aux_dirty_frame_is_not_reused_without_write_back() {
        // 审核修复回归（C1）：写回后进 AUX 的帧**再次改脏**时，前台找空闲帧
        // 不得无写回直接覆盖它（pin 命中与 mark_dirty 都不把帧移出 AUX）。
        let h = harness();
        let pool = h.pool(1, h.fake_wal());
        let k0 = BufferKey::new(WS_A, rdba(7, 0));
        let k1 = BufferKey::new(WS_A, rdba(7, 1));

        // 1) 用 k0 → 改 → 标脏 → flush：帧干净、进 AUX。
        {
            let mut g = pool.pin(k0).unwrap();
            g.as_bytes_mut()[4096] = 0x77;
            g.mark_dirty(lsn(1));
        }
        pool.flush(k0).unwrap();
        assert_eq!(h.read_byte(7, 0), 0x77, "第一次写回");

        // 2) 再钉住同一页 → 再改 → 再标脏（帧留在 AUX、且是脏的）。
        {
            let mut g = pool.pin(k0).unwrap();
            g.as_bytes_mut()[4096] = 0x88;
            g.mark_dirty(lsn(2));
        }

        // 3) 容量 1：读 k1 必须先把 k0 写回（Make Free → 写回 → 入 AUX），
        //    而不是把脏的 k0 直接覆盖。
        {
            let g = pool.pin(k1).unwrap();
            assert_eq!(g.as_bytes()[4096], 0xA1, "k1 内容来自文件");
        }
        assert_eq!(
            h.read_byte(7, 0),
            0x88,
            "再次改脏的内容必须写回，不得被丢弃"
        );
        assert_eq!(pool.dirty_len(WS_A), 0, "写列表无孤儿条目");
    }

    #[test]
    fn make_free_terminates_with_stale_write_list_entries() {
        // 审核修复回归（C2）：帧已不在池中的**失步条目**必须按条目自身的
        // LSN 删除——用 `Lsn(0)` 当键删不掉，make_free 每轮重选同一条且
        // 什么都不写 ⇒ 死循环（前台 pin 永久挂起）。
        let h = harness();
        let pool = h.pool(2, h.fake_wal());
        {
            let mut inner = pool.lock(0);
            inner
                .write_list
                .entry(WS_A)
                .or_default()
                .insert((lsn(7), rdba(7, 1)));
        }
        pool.make_free(0).unwrap();
        assert!(
            !pool.lock(0).write_list.contains_key(&WS_A),
            "失步条目按自身 LSN 清除"
        );
        // 再跑一次也不挂（幂等）。
        pool.make_free(0).unwrap();
    }

    /// 分区池（多工作集；§5.10 的 P4 形态）。
    impl Harness {
        fn pool_partitioned<'io>(
            &'io self,
            partitions: usize,
            capacity: usize,
            wal: impl WalGuard + 'io,
        ) -> BufferPool<'io> {
            let (a, b) = (self.a, self.b);
            BufferPool::with_partitions(
                &self.io,
                partitions,
                capacity,
                move |ws, r| {
                    if *ws == WS_A && r.file_id() == 7 {
                        Some((a, r.block_id()))
                    } else if *ws == WS_B && r.file_id() == 8 {
                        Some((b, r.block_id()))
                    } else {
                        None
                    }
                },
                wal,
                SystemClock,
                CacheConfig::for_capacity(capacity),
            )
            .unwrap()
        }
    }

    #[test]
    fn partitions_map_workspaces_by_stable_hash() {
        // §5.10：`H(工作区标识) mod N`——稳定、确定、2 的幂按位与。
        let h = harness();
        let pool = h.pool_partitioned(4, 2, h.fake_wal());
        assert_eq!(pool.partition_count(), 4);
        // 同一个工作区永远落同一个分区（写列表不在写线程间搬家）。
        for _ in 0..3 {
            assert_eq!(pool.partition_of(&WS_A), pool.partition_of(&WS_A));
        }
        // 结构可见：容量是**每分区**的（总帧数 = N × 容量）。
        assert_eq!(pool.capacity(), 2);
        // 非 2 的幂被拒绝。
        let err = BufferPool::with_partitions(
            &h.io,
            3,
            2,
            |_, _| None,
            h.fake_wal(),
            SystemClock,
            CacheConfig::for_capacity(2),
        )
        .unwrap_err();
        assert!(matches!(err, BufferError::BadPartitionCount { .. }));
    }

    #[test]
    fn partitions_have_independent_capacity() {
        // 每分区容量 1、两个**不同分区**的工作区 ⇒ 各驻留一帧、互不驱逐——
        // "分区把跨工作区争用降为零"的最小可观测形态（N=1 时必驱逐）。
        // 夹具就地搭（两个文件、两个工作区；哈希决定分区，选已验证的一对）。
        let mem = MemFileIo::new();
        mem.add_dir("/mem");
        let io = mem;
        const WS_C: [u8; 8] = [3u8; 8]; // [3;8] 与 [1;8] 在 N=4 下不同分区（实测）
        let a = io
            .open(
                Path::new("/mem/pa.dat"),
                OpenOptions::new().read(true).write(true).create_new(true),
            )
            .unwrap();
        let c = io
            .open(
                Path::new("/mem/pc.dat"),
                OpenOptions::new().read(true).write(true).create_new(true),
            )
            .unwrap();
        for (h, ws, fid) in [(a, WS_A, 7u16), (c, WS_C, 9)] {
            io.set_len(h, 2 * crate::page::PAGE_SIZE as u64).unwrap();
            for block in 0..2u32 {
                let mut page = Page::new(PageType::HeapTable, ws, fid, block);
                page.as_bytes_mut()[4096] = 0x5A;
                pagefile::write_page(&io, h, block, &mut page).unwrap();
            }
        }
        let pool = BufferPool::with_partitions(
            &io,
            4,
            1,
            move |ws, r| {
                if *ws == WS_A && r.file_id() == 7 {
                    Some((a, r.block_id()))
                } else if *ws == WS_C && r.file_id() == 9 {
                    Some((c, r.block_id()))
                } else {
                    None
                }
            },
            FakeWal {
                durable: std::sync::atomic::AtomicU64::new(0),
                fail: false,
                log: Arc::new(Mutex::new(Vec::new())),
            },
            SystemClock,
            CacheConfig::for_capacity(1),
        )
        .unwrap();
        let pa = pool.partition_of(&WS_A);
        let pc = pool.partition_of(&WS_C);
        assert_ne!(pa, pc, "夹具的两个工作区必须落不同分区");
        let ka = BufferKey::new(WS_A, rdba(7, 0));
        let kc = BufferKey::new(WS_C, rdba(9, 0));
        {
            let _ga = pool.pin(ka).unwrap();
            let _gc = pool.pin(kc).unwrap(); // 另一分区：不驱逐 ka
        }
        assert_eq!(pool.resident(), 2, "两分区各驻留一帧");
        // 两个键都还是**命中**（容量 1 的 N=1 池此时必然驱逐过一次）。
        let stats = pool.stats();
        assert_eq!(stats.misses, 2, "各未命中一次");
        {
            let _ga = pool.pin(ka).unwrap();
            let _gc = pool.pin(kc).unwrap();
        }
        assert_eq!(pool.stats().hits, 2, "复访仍命中：分区互不驱逐");
    }

    #[test]
    fn a_workspace_is_never_split_across_partitions() {
        // §5.10：**一个工作区不被拆分**——它的全部缓冲与写列表都在同一个
        // 工作集里（检查点推进只碰一个闩锁，零跨分区协调）。
        let h = harness();
        let pool = h.pool_partitioned(8, 2, h.fake_wal());
        let p = pool.partition_of(&WS_A);
        for block in 0..2u32 {
            let mut g = pool.pin(BufferKey::new(WS_A, rdba(7, block))).unwrap();
            g.as_bytes_mut()[4096] = 0x11;
            g.mark_dirty(lsn(1 + u64::from(block)));
        }
        assert_eq!(pool.dirty_len(WS_A), 2);
        // 该工作区的两个帧都在同一个分区里（其它分区没有任何属于它的帧）。
        assert_eq!(
            pool.partitions[p]
                .structure
                .lock()
                .write_list
                .get(&WS_A)
                .map(BTreeSet::len),
            Some(2),
            "写列表整体落在一个分区"
        );
        for (i, part) in pool.partitions.iter().enumerate() {
            if i != p {
                assert!(
                    !part.structure.lock().write_list.contains_key(&WS_A),
                    "分区 {i} 不该有该工作区的写列表"
                );
            }
        }
        // 按分区刷该工作区：一次 flush_workspace 只碰一个分区、按序写两页。
        let report = pool.flush_workspace(WS_A).unwrap();
        assert_eq!(report.pages_written, 2);
    }

    /// **O3 竞态关**：腾帧候选被并发钉住 ⇒ 声明失败（`Detach::Pinned`），
    /// 不得摘链、不得覆盖内容。确定性构造：持着卫兵（pins > 0）再声明。
    #[test]
    fn claiming_a_pinned_frame_is_refused() {
        let h = harness();
        let pool = h.pool(4, h.fake_wal());
        let key = BufferKey::new(WS_A, rdba(7, 0));
        let partition = pool.partition_of(&WS_A);
        let g = pool.pin(key).unwrap(); // pins = 1
        let st = pool.lock(partition);
        let idx = {
            let (_s, local, gb) = pool.lock_bucket(partition, key);
            BufferPool::chain_find(&gb, local, key).expect("已驻留")
        };
        let mut st = st;
        assert!(
            matches!(
                pool.detach_for_reuse(partition, &mut st, idx),
                Detach::Pinned
            ),
            "被钉住的帧不得被声明复用"
        );
        drop(g);
        // 卫兵退场后可以声明（帧仍在池里）。
        assert!(matches!(
            pool.detach_for_reuse(partition, &mut st, idx),
            Detach::Done(Some(_))
        ));
    }

    /// **O3 并发压力回归**（串页防线）：多线程对**超过容量**的键集反复
    /// pin/drop——腾帧（声明 → 发布）与命中路径在桶闩下交错。每个拿到的卫兵
    /// 都必须"页头自述 = 请求的键"（读错帧的经典症状是块号不符）。
    #[test]
    fn concurrent_pin_churn_never_returns_the_wrong_page() {
        let h = harness();
        let pool = h.pool(2, h.fake_wal()); // 容量 2 < 4 键 ⇒ 持续腾帧
                                            // 夹具：WS_A → 文件 7、WS_B → 文件 8，各 2 块（harness）——4 个键；
                                            // **容量取 2**（< 键数）⇒ 持续"装入即腾帧"，声明/发布与命中路径交错。
        let keys: Vec<BufferKey> = [(WS_A, 7u16, 0u32), (WS_A, 7, 1), (WS_B, 8, 0), (WS_B, 8, 1)]
            .into_iter()
            .map(|(ws, f, b)| BufferKey::new(ws, rdba(f, b)))
            .collect();
        std::thread::scope(|scope| {
            for t in 0..4usize {
                let pool = &pool;
                let keys = &keys;
                scope.spawn(move || {
                    for i in 0..3_000usize {
                        let key = keys[(i * 7 + t * 13) % keys.len()];
                        // 容量 2 + 4 线程：全部帧被钉住时**允许** `free buffer
                        // waits`（§5.10 的既有语义，`fb_wait` 计数即此项）——
                        // 只对**成功**的钉住做串页断言。
                        let g = match pool.pin(key) {
                            Ok(g) => g,
                            Err(BufferError::FreeBufferWait) => continue,
                            Err(e) => panic!("钉住 {key:?}: {e}"),
                        };
                        let header = g.header().expect("页头");
                        assert_eq!(header.file_id, key.rdba.file_id(), "串页：文件号不符");
                        assert_eq!(header.block_id, key.rdba.block_id(), "串页：块号不符");
                        assert_eq!(header.workspace_ref, key.workspace, "串页：工作区不符");
                        drop(g);
                    }
                });
            }
        });
        // 结局分布合理：容量 2 < 键数 ⇒ 既有命中也有未命中。
        let stats = pool.stats();
        assert!(stats.hits > 0, "有命中");
        assert!(stats.misses > 0, "有未命中");
    }

    #[test]
    fn partition_counters_aggregate_and_split_by_latch() {
        let h = harness();
        let pool = h.pool_partitioned(4, 2, h.fake_wal());
        let ka = BufferKey::new(WS_A, rdba(7, 0));
        {
            let _g = pool.pin(ka).unwrap();
        }
        {
            // **一次一个卫兵**（§5.10 纪律）：统计读取前先放开卫兵。
            let _g = pool.pin(ka).unwrap(); // 命中
        }
        let stats = pool.stats();
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);
        // 闩锁统计（O3 起每分区两项：`db_cache` 结构闩 + `db_bucket` 桶闩聚合）。
        let per = pool.partition_latch_stats();
        assert_eq!(per.len(), 4 * 2);
        assert_eq!(per.iter().filter(|s| s.name == "db_bucket").count(), 4);
        let sum: u64 = per.iter().map(|s| s.gets).sum();
        let agg = pool.latch_stats();
        assert_eq!(agg.gets, sum, "聚合 = 逐分区（结构 + 桶）之和");
        assert_eq!(agg.name, "db_cache");
        assert!(sum >= 2, "至少两次取闩锁（命中 + 未命中）");
    }

    #[test]
    fn insert_new_on_a_resident_key_replaces_in_place() {
        // 回归：页被**重置复用**（段回卷/撤销页重建）时 `insert_new` 会遇到
        // "键已在池中"——必须**原位替换**。若照常 `find_reusable`/`attach`，
        // 桶里留下**两个同键帧**：`find_frame` 命中旧的干净帧 ⇒ `flush` 判"不脏"
        // 直接返回，新内容永远写不到盘上（撤销页丢失、链在重放后成环的实测根因）。
        let h = harness();
        let pool = h.pool(2, h.fake_wal());
        let k0 = BufferKey::new(WS_A, rdba(7, 0));
        {
            let g = pool.pin(k0).unwrap();
            assert_eq!(g.as_bytes()[4096], 0xA0, "旧内容已驻留");
        }
        assert_eq!(pool.resident(), 1);

        let mut fresh = Page::new(PageType::HeapTable, WS_A, 7, 0);
        fresh.as_bytes_mut()[4096] = 0xEE;
        {
            let mut g = pool.insert_new(k0, fresh).unwrap();
            g.mark_dirty(lsn(9));
        }
        assert_eq!(pool.resident(), 1, "原位替换：不产生第二个同键帧");
        assert!(
            pool.flush(k0).unwrap(),
            "新内容要真的写出（不是被旧帧遮蔽）"
        );
        assert_eq!(h.read_byte(7, 0), 0xEE, "盘上是重置后的新内容");
    }

    #[test]
    fn pin_does_io_outside_the_latch_so_others_proceed() {
        // O1 回归：**闩锁内不做 I/O**（证据包 latch-mech-20261005 结论 1）。
        // pin 未命中在闩外读盘——读盘在途时，另一线程必须能立即取得池闩锁；
        // 旧形态（持闩读盘）下 `pool.stats()` 会一直阻塞到读盘结束。
        let mem = MemFileIo::new();
        mem.add_dir("/mem");
        let io = GateIo::new(mem);
        let (a, _b) = open_files(&io);
        io.block(false, crate::page::PAGE_SIZE as u64); // 拦"读块 1"
        let pool = Arc::new(
            BufferPool::new(
                &io,
                4,
                move |ws, r| {
                    if *ws == WS_A && r.file_id() == 7 {
                        Some((a, r.block_id()))
                    } else {
                        None
                    }
                },
                FakeWal {
                    durable: std::sync::atomic::AtomicU64::new(0),
                    fail: false,
                    log: Arc::new(Mutex::new(Vec::new())),
                },
            )
            .unwrap(),
        );
        let key = BufferKey::new(WS_A, rdba(7, 1));
        std::thread::scope(|scope| {
            let p1 = Arc::clone(&pool);
            let reader = scope.spawn(move || p1.pin(key).map(|g| g.as_bytes()[4096]));
            let entered = io.wait_entered();
            if !entered {
                io.release();
            }
            assert!(entered, "pin 未命中已进入闩锁外的读盘");

            // 读盘在途：另一线程取闩锁（`stats()` 要持闩锁）必须立即成功。
            let (tx, rx) = std::sync::mpsc::channel();
            let p2 = Arc::clone(&pool);
            let prober = scope.spawn(move || {
                let _ = p2.stats();
                tx.send(()).unwrap();
            });
            let got = rx.recv_timeout(std::time::Duration::from_secs(5));
            io.release();
            prober.join().unwrap();
            assert!(
                got.is_ok(),
                "读盘窗口内闩锁可被他人取得（O1：闩锁内不做 I/O）"
            );
            assert_eq!(reader.join().unwrap().unwrap(), 0xA1, "并发 pin 结果不变");
        });
    }

    #[test]
    fn re_dirtied_during_write_window_stays_dirty() {
        // 两阶段写回：写盘窗口内页被**再改脏**（`mod_seq` 推进）⇒ 收尾不得
        // 清脏——绝不把"更新过的版本"当"已落盘"（PG `BM_JUST_DIRTIED` 同款）。
        let mem = MemFileIo::new();
        mem.add_dir("/mem");
        let io = GateIo::new(mem);
        let (a, _b) = open_files(&io);
        let pool = Arc::new(
            BufferPool::new(
                &io,
                2,
                move |ws, r| {
                    if *ws == WS_A && r.file_id() == 7 {
                        Some((a, r.block_id()))
                    } else {
                        None
                    }
                },
                FakeWal {
                    durable: std::sync::atomic::AtomicU64::new(0),
                    fail: false,
                    log: Arc::new(Mutex::new(Vec::new())),
                },
            )
            .unwrap(),
        );
        let key = BufferKey::new(WS_A, rdba(7, 0));
        {
            let mut g = pool.pin(key).unwrap();
            g.as_bytes_mut()[4096] = 0x77;
            g.bump_mod_seq(); // 修改的规范动作（页 API 推进版本号）
            g.mark_dirty(lsn(5));
        }
        io.block(true, 0); // 拦"写块 0"
        std::thread::scope(|scope| {
            let p1 = Arc::clone(&pool);
            let writer = scope.spawn(move || p1.flush(key).unwrap());
            let entered = io.wait_entered();
            if !entered {
                io.release();
            }
            assert!(entered, "写回已进入闩锁外的写盘窗口");

            // 窗口内再改脏（闩锁可用 = O1 的又一体现）。
            let (tx, rx) = std::sync::mpsc::channel();
            let p2 = Arc::clone(&pool);
            let modifier = scope.spawn(move || {
                let mut g = p2.pin(key).unwrap();
                g.as_bytes_mut()[4096] = 0x88;
                g.bump_mod_seq();
                g.mark_dirty(lsn(9));
                tx.send(()).unwrap();
            });
            let modified = rx.recv_timeout(std::time::Duration::from_secs(5));
            io.release();
            modifier.join().unwrap();
            assert!(
                modified.is_ok(),
                "写盘窗口内闩锁可被他人取得（O1：闩锁内不做 I/O）"
            );
            assert!(writer.join().unwrap(), "第一版已写出（写本身成功）");
        });
        assert_eq!(
            pool.dirty_len(WS_A),
            1,
            "期间被再改脏 ⇒ 保持脏（不得标为已落盘）"
        );
        assert!(pool.low_water(WS_A).is_some(), "写列表条目保留");

        // 下一轮把新版本写出，读文件确认。
        pool.flush(key).unwrap();
        assert_eq!(pool.dirty_len(WS_A), 0, "写回后条目出列");
        let page = pagefile::read_page(&io, a, 0).unwrap();
        assert_eq!(page.as_bytes()[4096], 0x88, "新版本落盘");
    }

    // -- NUMA 重绑定支撑：帧惰性分配 / 排空（Draining）----------------------

    #[test]
    fn frame_buffers_allocate_on_first_use_and_drain_frees_them() {
        // §5.10 NUMA：帧内存**首次触碰才分配**（装页的线程决定它落在哪个
        // 节点），Draining ② 释放它——重绑定后下次装入按新绑定重新落位。
        let h = harness();
        let pool = h.pool(4, h.fake_wal());
        let p = pool.partition_of(&WS_A);
        assert_eq!(pool.allocated_frames(p), 0, "空池不分配任何页缓冲");
        {
            let _g = pool.pin(BufferKey::new(WS_A, rdba(7, 0))).unwrap();
        }
        assert_eq!(pool.allocated_frames(p), 1, "装页即分配");
        let report = pool.drain_partition(p).unwrap();
        assert_eq!(report.pages_written, 0, "无脏页：没有写回");
        assert_eq!(report.frames_dropped, 1);
        assert_eq!(pool.allocated_frames(p), 0, "净帧的页缓冲已释放");
        assert_eq!(pool.resident(), 0);
        // 再装入：从文件重新读（不搬内存——缓冲是副本，可重建）。
        let g = pool.pin(BufferKey::new(WS_A, rdba(7, 0))).unwrap();
        assert_eq!(g.as_bytes()[4096], 0xA0, "内容从盘上重建");
    }

    #[test]
    fn drain_partition_flushes_dirty_pages_then_drops_clean_frames() {
        let h = harness();
        let pool = h.pool(4, h.fake_wal());
        let p = pool.partition_of(&WS_A);
        for (block, byte, l) in [(0u32, 0x11u8, 5u64), (1, 0x22, 3)] {
            let mut g = pool.pin(BufferKey::new(WS_A, rdba(7, block))).unwrap();
            g.as_bytes_mut()[4096] = byte;
            h.set_page_lsn(&mut g, l);
            g.mark_dirty(lsn(l));
        }
        let report = pool.drain_partition(p).unwrap();
        assert_eq!(report.pages_written, 2, "两页脏页按写列表序写回");
        assert_eq!(report.frames_dropped, 2);
        assert_eq!(h.read_byte(7, 0), 0x11);
        assert_eq!(h.read_byte(7, 1), 0x22);
        assert_eq!(pool.dirty_len(WS_A), 0, "写列表清空");
        assert_eq!(pool.resident(), 0, "排空后无驻留帧");
    }

    #[test]
    fn drop_clean_frames_refuses_while_frames_are_dirty() {
        // 丢净帧**不静默丢脏帧**（那就是数据丢失）——先 flush，再丢。
        let h = harness();
        let pool = h.pool(4, h.fake_wal());
        let p = pool.partition_of(&WS_A);
        {
            let mut g = pool.pin(BufferKey::new(WS_A, rdba(7, 0))).unwrap();
            g.as_bytes_mut()[4096] = 0x33;
            g.mark_dirty(lsn(1));
        }
        assert!(matches!(
            pool.drop_clean_frames(p),
            Err(BufferError::DrainBlocked {
                dirty: 1,
                pinned: 0
            })
        ));
        assert!(pool.flush(BufferKey::new(WS_A, rdba(7, 0))).unwrap());
        assert_eq!(pool.drop_clean_frames(p).unwrap(), 1);
        assert_eq!(h.read_byte(7, 0), 0x33, "脏页在丢弃之前已经落盘");
    }

    #[test]
    fn draining_one_partition_leaves_the_other_untouched() {
        // **帧区间按集独立**（§5.10 NUMA）：排空一个工作集不动另一个——
        // 重绑定的粒度是工作集（分区），不是整池。
        // 夹具：WS_A（[1;8]）与 WS_D（[4;8]）在 N=2 下不同分区（实测哈希）。
        let mem = MemFileIo::new();
        mem.add_dir("/mem");
        const WS_D: [u8; 8] = [4u8; 8];
        let da = mem
            .open(
                Path::new("/mem/da.dat"),
                OpenOptions::new().read(true).write(true).create_new(true),
            )
            .unwrap();
        let dd = mem
            .open(
                Path::new("/mem/dd.dat"),
                OpenOptions::new().read(true).write(true).create_new(true),
            )
            .unwrap();
        for (h, ws, fid) in [(da, WS_A, 7u16), (dd, WS_D, 9)] {
            mem.set_len(h, crate::page::PAGE_SIZE as u64).unwrap();
            let mut page = Page::new(PageType::HeapTable, ws, fid, 0);
            page.as_bytes_mut()[4096] = 0x5A;
            pagefile::write_page(&mem, h, 0, &mut page).unwrap();
        }
        let pool = BufferPool::with_partitions(
            &mem,
            2,
            2,
            move |ws, r| {
                if *ws == WS_A && r.file_id() == 7 {
                    Some((da, r.block_id()))
                } else if *ws == WS_D && r.file_id() == 9 {
                    Some((dd, r.block_id()))
                } else {
                    None
                }
            },
            FakeWal {
                durable: std::sync::atomic::AtomicU64::new(0),
                fail: false,
                log: Arc::new(Mutex::new(Vec::new())),
            },
            SystemClock,
            CacheConfig::for_capacity(2),
        )
        .unwrap();
        let (pa, pd) = (pool.partition_of(&WS_A), pool.partition_of(&WS_D));
        assert_ne!(pa, pd, "夹具的两个工作区必须落不同分区");
        let ka = BufferKey::new(WS_A, rdba(7, 0));
        let kd = BufferKey::new(WS_D, rdba(9, 0));
        {
            let _ga = pool.pin(ka).unwrap();
            let _gd = pool.pin(kd).unwrap();
        }
        assert_eq!(pool.resident(), 2);
        assert_eq!(pool.drain_partition(pa).unwrap().frames_dropped, 1);
        assert_eq!(pool.allocated_frames(pa), 0, "被排空的分区已释放帧");
        assert_eq!(pool.allocated_frames(pd), 1, "另一分区不受影响");
        assert_eq!(pool.resident(), 1);
        // 受影响分区的键：重新装入（miss）；另一分区：仍是命中。
        let before = pool.stats();
        {
            let _ga = pool.pin(ka).unwrap();
            let _gd = pool.pin(kd).unwrap();
        }
        let after = pool.stats();
        assert_eq!(after.misses - before.misses, 1, "只有被排空的页重新读盘");
        assert_eq!(after.hits - before.hits, 1, "另一分区的帧原样命中");
    }

    // -- O2：per-frame 状态对象（卫兵 ≠ 持锁、内容锁与链闩分离）---------------

    #[test]
    fn guards_no_longer_hold_the_partition_latch() {
        // O2（§5.10）：持卫兵期间不再持分区闩锁——同线程可以**同时持多个
        // 卫兵**，也可以照常调池方法（N=1 下旧形态的"一次一个卫兵"会自锁）。
        let h = harness();
        let pool = h.pool(4, h.fake_wal());
        let ka = BufferKey::new(WS_A, rdba(7, 0));
        let kb = BufferKey::new(WS_A, rdba(7, 1));
        let ga = pool.pin(ka).unwrap();
        let gb = pool.pin(kb).unwrap(); // 旧形态：同线程第二把卫兵自锁
        assert_eq!(ga.as_bytes()[4096], 0xA0);
        assert_eq!(gb.as_bytes()[4096], 0xA1);
        // 持卫兵期间照常调池（旧形态：自锁）。
        assert_eq!(pool.resident(), 2);
        assert_eq!(pool.dirty_len(WS_A), 0);
        drop((ga, gb));
    }

    #[test]
    fn shared_read_guards_are_concurrent_on_the_same_page() {
        // O2 的目标：同一热块的并发**读**互不串行（缓冲 handle 思路）。
        let h = harness();
        let pool = std::sync::Arc::new(h.pool(4, h.fake_wal()));
        let key = BufferKey::new(WS_A, rdba(7, 0));
        {
            let _ = pool.pin(key).unwrap(); // 先驻留（共享钉住不触发读盘）
        }
        let gate = std::sync::Arc::new((std::sync::Mutex::new(0usize), std::sync::Condvar::new()));
        std::thread::scope(|scope| {
            for _ in 0..2 {
                let pool = std::sync::Arc::clone(&pool);
                let gate = std::sync::Arc::clone(&gate);
                scope.spawn(move || {
                    let g = pool.pin_shared(key).expect("驻留后共享命中");
                    assert_eq!(g.as_bytes()[4096], 0xA0);
                    // 两名读者同时在持 —— 到齐才放行。
                    let (lock, cv) = &*gate;
                    let mut n = lock.lock().unwrap();
                    *n += 1;
                    cv.notify_all();
                    while *n < 2 {
                        let (g2, _) = cv
                            .wait_timeout(n, std::time::Duration::from_secs(5))
                            .unwrap();
                        n = g2;
                        if *n >= 2 {
                            break;
                        }
                    }
                });
            }
        });
    }

    #[test]
    fn exclusive_guard_excludes_shared_readers_until_it_drops() {
        // 同一帧上"写独占 / 读共享"的互斥：持独占卫兵时共享读者等待，
        // 但**等待发生在内容锁上**（不是池闩锁）——池的结构面照常可用。
        let h = harness();
        let pool = std::sync::Arc::new(h.pool(4, h.fake_wal()));
        let key = BufferKey::new(WS_A, rdba(7, 0));
        let guard = pool.pin(key).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();

        std::thread::scope(|scope| {
            let pool2 = std::sync::Arc::clone(&pool);
            let reader = scope.spawn(move || {
                let g = pool2.pin_shared(key).expect("命中");
                tx.send(g.as_bytes()[4096]).unwrap();
            });
            // 独占未放行：读者不能完成。
            assert!(
                rx.recv_timeout(std::time::Duration::from_millis(80))
                    .is_err(),
                "读被写卫兵挡住"
            );
            // 而这期间池的结构面照常可用（锁在帧上，不在池上）。
            assert_eq!(pool.resident(), 1);
            drop(guard);
            assert_eq!(
                rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap(),
                0xA0
            );
            reader.join().unwrap();
        });
    }

    #[test]
    fn drain_refuses_while_any_frame_is_pinned() {
        // O2 起 `pinned` 判据是**真判据**（旧形态下持卫兵即持闩锁，别人进不来）。
        let h = harness();
        let pool = h.pool(4, h.fake_wal());
        let p = pool.partition_of(&WS_A);
        let guard = pool.pin(BufferKey::new(WS_A, rdba(7, 0))).unwrap();
        assert!(matches!(
            pool.drain_partition(p),
            Err(BufferError::DrainBlocked { pinned: 1, .. })
        ));
        drop(guard);
        assert_eq!(pool.drain_partition(p).unwrap().frames_dropped, 1);
    }
}
