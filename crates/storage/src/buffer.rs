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
//! `N = 1` 起步（§5.10"分区是可选能力"的默认值）：单闩锁，**一次只能持有
//! 一个 `PageGuard`**（同线程再 `pin` 会自锁——std 互斥不可重入）。需要跨页
//! 操作时先取副本、放开卫兵再取下一页；分区/工作集与每分区 latch 在 P4 接入。
//! 临界区纪律（**闩锁内不做 I/O**等四条）与闩锁统计口径见 §5.10"闩锁形态与纪律"。

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use bicdb_common::latch::{Latch, LatchGuard, LatchStats};
use bicdb_common::seq::Lsn;
use bicdb_workspace::io::{FileHandle, FileIo};

use crate::page::Page;
use crate::pagefile::{self, PageFileError};
use crate::rowid::Rdba;

/// 块定位器：`(工作区标识, rdba) →（页文件句柄、块号）`。
///
/// 带上工作区是因为**池是实例级共享**的：不同工作区各有自己的文件句柄。
pub type PoolResolver<'io> = dyn FnMut(&[u8; 8], Rdba) -> Option<(FileHandle, u32)> + Send + 'io;

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
pub trait Clock: Send {
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

/// 一次 `flush_workspace` 的报告。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FlushReport {
    /// 写回的页数。
    pub pages_written: u64,
    /// 写回后该工作区的新低水位（写列表已空 ⇒ `None`）。
    pub low_water: Option<Lsn>,
}

/// 一个缓冲帧（内存专有；`kcbbh` 对应物——**不落盘**）。
struct Frame {
    key: Option<BufferKey>,
    page: Page,
    /// 脏标志（同时在写列表里）。
    dirty: bool,
    /// **首次变脏的 LSN**（写列表/检查点队列排序键）。
    first_dirty: Option<Lsn>,
    /// 引用计数（钉住）。
    pins: u32,
    /// 触摸计数与上次递增时刻（三秒规则）。
    touches: u32,
    last_touch_ms: u64,
}

impl Frame {
    fn empty() -> Self {
        Self {
            key: None,
            page: Page::from_bytes(Box::new([0u8; crate::page::PAGE_SIZE])),
            dirty: false,
            first_dirty: None,
            pins: 0,
            touches: 0,
            last_touch_ms: 0,
        }
    }
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

/// 池内状态（单闩锁 `db_cache`；分区/桶分片 latch 随 P4——§5.10"闩锁形态与纪律"）。
struct Inner<'io> {
    frames: Vec<Frame>,
    /// 从未用过的帧（首次装入后帧就长期挂在链上）。
    virgin: Vec<usize>,
    /// 哈希桶：桶号 → 帧号链（找块）。
    buckets: Vec<Vec<usize>>,
    /// 热段（头 = 最热）。
    hot: VecDeque<usize>,
    /// 冷段（头 = 刚重用；尾 = 最先淘汰）。
    cold: VecDeque<usize>,
    /// 可重用候选（干净、未钉住；前台优先扫它）。
    aux: VecDeque<usize>,
    /// 写列表：**每工作区一条**（= 检查点队列），按（首次变脏 LSN, rdba）升序。
    write_list: BTreeMap<[u8; 8], BTreeSet<(Lsn, Rdba)>>,
    resolve: Box<PoolResolver<'io>>,
    clock: Box<dyn Clock + 'io>,
    cfg: CacheConfig,
    stats: BufferStats,
}

/// **DB Cache**（§5.10；本切片 N = 1 分区）。
pub struct BufferPool<'io> {
    io: &'io dyn FileIo,
    capacity: usize,
    /// **具名闩锁**（"db_cache"；先自旋后睡眠、V$LATCH 口径统计——
    /// 证据包 `latch-mech-20261005/`）。**闩锁内不做 I/O**：读盘与写回都在
    /// 闩外完成（两阶段，见 `pin`/`make_free`）。
    inner: Latch<Inner<'io>>,
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
        resolve: impl FnMut(&[u8; 8], Rdba) -> Option<(FileHandle, u32)> + Send + 'io,
        wal: impl WalGuard + 'io,
    ) -> Result<Self, BufferError> {
        Self::with_clock(io, capacity, resolve, wal, SystemClock)
    }

    /// 建池（指定时钟——测试用手动时钟控制三秒规则）。
    pub fn with_clock(
        io: &'io dyn FileIo,
        capacity: usize,
        resolve: impl FnMut(&[u8; 8], Rdba) -> Option<(FileHandle, u32)> + Send + 'io,
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
        resolve: impl FnMut(&[u8; 8], Rdba) -> Option<(FileHandle, u32)> + Send + 'io,
        wal: impl WalGuard + 'io,
        clock: impl Clock + 'io,
        cfg: CacheConfig,
    ) -> Result<Self, BufferError> {
        if capacity == 0 {
            return Err(BufferError::ZeroCapacity);
        }
        Ok(Self {
            io,
            capacity,
            inner: Latch::new(
                "db_cache",
                Inner {
                    frames: (0..capacity).map(|_| Frame::empty()).collect(),
                    virgin: (0..capacity).rev().collect(),
                    buckets: vec![Vec::new(); cfg.buckets],
                    hot: VecDeque::new(),
                    cold: VecDeque::new(),
                    aux: VecDeque::new(),
                    write_list: BTreeMap::new(),
                    resolve: Box::new(resolve),
                    clock: Box::new(clock),
                    cfg,
                    stats: BufferStats::default(),
                },
            ),
            wal: Latch::new("redo_write", Box::new(wal)),
        })
    }

    /// 容量（帧数）。
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// 当前驻留帧数。
    #[must_use]
    pub fn resident(&self) -> usize {
        self.lock().capacity_used()
    }

    /// 哈希桶数。
    #[must_use]
    pub fn bucket_count(&self) -> usize {
        self.lock().buckets.len()
    }

    /// 某桶的链长（诊断）。
    #[must_use]
    pub fn bucket_len(&self, bucket: usize) -> usize {
        self.lock().buckets.get(bucket).map_or(0, Vec::len)
    }

    /// 统计快照。
    #[must_use]
    pub fn stats(&self) -> BufferStats {
        self.lock().stats
    }

    /// 某工作区写列表长度（= 脏块数）。
    #[must_use]
    pub fn dirty_len(&self, workspace: [u8; 8]) -> usize {
        self.lock()
            .write_list
            .get(&workspace)
            .map_or(0, BTreeSet::len)
    }

    /// **有脏页的工作区**（DBWR 后台线程的入口：按此逐个 `flush_workspace`）。
    #[must_use]
    pub fn dirty_workspaces(&self) -> Vec<[u8; 8]> {
        self.lock().write_list.keys().copied().collect()
    }

    /// **低水位**：该工作区最老脏块的（首次变脏 LSN）；`None` = 无脏页。
    #[must_use]
    pub fn low_water(&self, workspace: [u8; 8]) -> Option<Lsn> {
        self.lock()
            .write_list
            .get(&workspace)
            .and_then(|s| s.iter().next().map(|(lsn, _)| *lsn))
    }

    /// 某帧的 **TCH**（touch count——对应 `x$bh` 的 `TCH` 列；热块诊断在
    /// Oracle 侧即"TCH 越高，块被访问越频繁"）。
    #[must_use]
    pub fn touch_count(&self, key: BufferKey) -> Option<u32> {
        let inner = self.lock();
        inner.find_frame(key).map(|idx| inner.frames[idx].touches)
    }

    /// 某帧在哪条链上（`hot` / `cold` / `aux`；诊断与测试）。
    #[must_use]
    pub fn chain_of(&self, key: BufferKey) -> Option<&'static str> {
        let inner = self.lock();
        let idx = inner.find_frame(key)?;
        Some(if inner.hot.contains(&idx) {
            "hot"
        } else if inner.cold.contains(&idx) {
            "cold"
        } else if inner.aux.contains(&idx) {
            "aux"
        } else {
            "none"
        })
    }

    /// **钉住一页**（命中 → touch count；未命中 → 读入 → 冷段头）。
    ///
    /// **两阶段（闩锁内不做 I/O；证据包 `latch-mech-20261005/` 结论 1）**：
    /// ① 闩锁内定位与钉住；未命中则**释放闩锁后**读盘并做身份核对（串页
    /// 防线）；② 重新持闩：期间可能已被他人装入（**先到者为准**，丢弃本次
    /// 读到的副本），否则腾帧（Make Free 的 I/O 同样在闩外）后装入。
    pub fn pin(&self, key: BufferKey) -> Result<PageGuard<'_, 'io>, BufferError> {
        let mut inner = self.lock();
        if let Some(idx) = inner.find_frame(key) {
            // 命中：钉住、touch count（三秒规则）、可能的冷→热提升。
            inner.stats.hits += 1;
            inner.frames[idx].pins += 1;
            inner.touch(idx);
            return Ok(PageGuard { inner, idx });
        }
        inner.stats.misses += 1;
        let (handle, block) = (inner.resolve)(&key.workspace, key.rdba)
            .ok_or(BufferError::Unresolved { rdba: key.rdba })?;
        drop(inner);

        // 闩锁外：读盘 + 身份核对。
        let page = pagefile::read_page_verified(self.io, handle, block).map_err(|e| match e {
            PageFileError::Damaged { .. } => BufferError::Damaged { rdba: key.rdba },
            PageFileError::Io(e) => BufferError::Io(e),
        })?;
        self.verify_identity(&page, key)?;

        // 重新持闩：期间已被他人装入 ⇒ 以先到者为准；能腾出帧 ⇒ 装入。
        let mut inner = self.lock();
        if let Some(idx) = inner.find_frame(key) {
            inner.stats.hits += 1;
            inner.frames[idx].pins += 1;
            inner.touch(idx);
            return Ok(PageGuard { inner, idx });
        }
        if let Some(victim) = inner.find_reusable() {
            inner.attach(victim, key, page);
            return Ok(PageGuard { inner, idx: victim });
        }
        // 无可用帧：闩外 Make Free（I/O 在闩外）后重试一次。
        drop(inner);
        self.make_free()?;
        let mut inner = self.lock();
        if let Some(idx) = inner.find_frame(key) {
            inner.stats.hits += 1;
            inner.frames[idx].pins += 1;
            inner.touch(idx);
            return Ok(PageGuard { inner, idx });
        }
        match inner.find_reusable() {
            Some(victim) => {
                inner.attach(victim, key, page);
                Ok(PageGuard { inner, idx: victim })
            }
            None => {
                inner.stats.fb_wait += 1;
                Err(BufferError::FreeBufferWait)
            }
        }
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
    pub fn insert_new(
        &self,
        key: BufferKey,
        page: Page,
    ) -> Result<PageGuard<'_, 'io>, BufferError> {
        let mut inner = self.lock();
        // **该键仍在池中 ⇒ 原位替换**：页被重置/复用（段回卷、重置复用的撤销页）
        // 时调用方给的镜像就是权威内容——若走 `find_reusable`/`attach`，
        // 桶里会留下**两个同键帧**，`find_frame` 命中的仍是旧的干净帧 ⇒
        // 写回被静默跳过（新内容永远到不了盘上；实测的撤销页丢失即此）。
        if let Some(idx) = inner.find_frame(key) {
            inner.replace_in_place(idx, key, page);
            return Ok(PageGuard { inner, idx });
        }
        if let Some(victim) = inner.find_reusable() {
            inner.attach(victim, key, page);
            return Ok(PageGuard { inner, idx: victim });
        }
        drop(inner);
        self.make_free()?; // I/O 在闩外
        let mut inner = self.lock();
        match inner.find_reusable() {
            Some(victim) => {
                inner.attach(victim, key, page);
                Ok(PageGuard { inner, idx: victim })
            }
            None => {
                inner.stats.fb_wait += 1;
                Err(BufferError::FreeBufferWait)
            }
        }
    }

    /// **命中即拷副本**（**不触发读盘**）：扫描批查池用（§5.12）。
    /// 不 touch（扫描语义——不让一次性扫描顶热计数；§5.10 的"一次性扫描
    /// 不该污染热段"由此在 API 上显式化）。
    #[must_use]
    pub fn copy_if_resident(&self, key: BufferKey) -> Option<Page> {
        let inner = self.lock();
        let idx = inner.find_frame(key)?;
        Some(inner.frames[idx].page.clone())
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
        let mut inner = self.lock();
        if inner.find_frame(key).is_some() {
            return Ok(());
        }
        if let Some(v) = inner.find_reusable() {
            inner.attach(v, key, page);
            inner.frames[v].pins = 0; // 净页装入立即放掉
            return Ok(());
        }
        drop(inner);
        self.make_free()?; // I/O 在闩外
        let mut inner = self.lock();
        if inner.find_frame(key).is_some() {
            return Ok(());
        }
        match inner.find_reusable() {
            Some(v) => {
                inner.attach(v, key, page);
                inner.frames[v].pins = 0;
                Ok(())
            }
            None => {
                inner.stats.fb_wait += 1;
                Err(BufferError::FreeBufferWait)
            }
        }
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
        let key_at = |i: u32| -> Option<BufferKey> {
            let block = first.block_id().checked_add(i)?;
            let rdba = Rdba::from_parts(first.file_id(), block)?;
            Some(BufferKey::new(workspace, rdba))
        };
        // ① 闩锁内查池（命中即拷副本）。
        let mut out: Vec<Option<Page>> = Vec::with_capacity(count as usize);
        let mut any_missing = false;
        let (handle, base) = {
            let mut inner = self.lock();
            for i in 0..count {
                let key = key_at(i).ok_or(BufferError::Unresolved { rdba: first })?;
                match inner.find_frame(key) {
                    Some(idx) => out.push(Some(inner.frames[idx].page.clone())),
                    None => {
                        out.push(None);
                        any_missing = true;
                    }
                }
            }
            if !any_missing {
                return Ok(out.into_iter().flatten().collect());
            }
            (inner.resolve)(&workspace, first).ok_or(BufferError::Unresolved { rdba: first })?
        };
        // ② 闩锁外：一次区读。
        let pages = pagefile::read_run(self.io, handle, base, count).map_err(|e| match e {
            PageFileError::Damaged { .. } => BufferError::Damaged { rdba: first },
            PageFileError::Io(e) => BufferError::Io(e),
        })?;
        {
            let mut inner = self.lock();
            inner.stats.run_reads += 1;
            inner.stats.run_pages += u64::from(count);
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
        Ok(matches!(
            self.write_back_step(WriteTarget::Key(key))?,
            Some(true)
        ))
    }

    /// **按序写回一个工作区的全部脏页**（写列表头 → 尾）。
    pub fn flush_workspace(&self, workspace: [u8; 8]) -> Result<FlushReport, BufferError> {
        let mut report = FlushReport::default();
        loop {
            match self.write_back_step(WriteTarget::WorkspaceHead(workspace))? {
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

    /// **Make Free**（§5.10 的 MKFREE 流程内联版）：写列表头按序写回一批；
    /// **闩锁内只选页与收尾，I/O 在闩外**。返回是否实际写过页。
    fn make_free(&self) -> Result<bool, BufferError> {
        let batch = {
            let inner = self.lock();
            (inner.frames.len() / 64).max(1)
        };
        let mut steps = 0usize;
        let mut wrote_any = false;
        while steps < batch {
            match self.write_back_step(WriteTarget::OldestHead)? {
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
    fn write_back_step(&self, target: WriteTarget) -> Result<Option<bool>, BufferError> {
        let job = {
            let mut inner = self.lock();
            match inner.pick_for_write(target)? {
                Pick::None => return Ok(None),
                Pick::Stale => return Ok(Some(false)),
                Pick::Ready {
                    idx,
                    key,
                    handle,
                    block,
                } => inner.begin_write(idx, key, handle, block),
            }
        };
        let wal_synced = self.perform_write(&job)?;
        let mut inner = self.lock();
        inner.finish_write(&job, wal_synced);
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

    fn lock(&self) -> LatchGuard<'_, Inner<'io>> {
        self.inner.lock()
    }

    /// **闩锁统计**（诊断：gets/immediate/spin/sleeps/wait_ns——
    /// `V$LATCH` 口径；证据包 `latch-mech-20261005/`）。
    #[must_use]
    pub fn latch_stats(&self) -> LatchStats {
        self.inner.stats()
    }
}

impl Inner<'_> {
    fn capacity_used(&self) -> usize {
        self.frames.len() - self.virgin.len()
    }

    /// 桶号 = **DBA（rdba）对桶数取模**（Oracle `_DB_BLOCK_HASH_BUCKETS` 的原文
    /// 口径："hash the required DBA by this number"）；跨工作区的同址块落同桶
    /// ——链上再按完整键比对。
    fn bucket_of(&self, key: BufferKey) -> usize {
        let dba = (u64::from(key.rdba.file_id()) << 28) | u64::from(key.rdba.block_id());
        (dba % self.buckets.len() as u64) as usize
    }

    /// 桶内找帧。
    fn find_frame(&self, key: BufferKey) -> Option<usize> {
        let b = self.bucket_of(key);
        self.buckets[b]
            .iter()
            .copied()
            .find(|&i| self.frames[i].key == Some(key))
    }

    /// 命中时的 touch count（三秒规则）与冷→热提升。
    fn touch(&mut self, idx: usize) {
        let now = self.clock.now_ms();
        {
            let f = &mut self.frames[idx];
            if now.saturating_sub(f.last_touch_ms) >= self.cfg.touch_interval_ms {
                f.touches = f.touches.saturating_add(1);
                f.last_touch_ms = now;
            }
        }
        // 冷段中计数达热判据 ⇒ 提升到热段头（计数置驻留值——`_STAY_COUNT` 语义）；
        // 热段超限 ⇒ 热段尾退回冷段头（计数置冷却值——`_COOL_COUNT` 语义）。
        if self.frames[idx].touches >= self.cfg.hot_criteria {
            if let Some(p) = self.cold.iter().position(|&i| i == idx) {
                self.cold.remove(p);
                self.hot.push_front(idx);
                self.frames[idx].touches = self.cfg.stay_count;
                self.stats.hot_moved += 1;
                let hot_max = (self.frames.len() / self.cfg.hot_fraction).max(1);
                while self.hot.len() > hot_max {
                    if let Some(back) = self.hot.pop_back() {
                        self.frames[back].touches = self.cfg.cool_count;
                        self.cold.push_front(back);
                    }
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
    fn find_reusable(&mut self) -> Option<usize> {
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
            .find(|&&i| self.frames[i].pins == 0 && !self.frames[i].dirty)
        {
            self.stats.free_inspected += 1;
            self.stats.evictions += 1;
            return Some(idx);
        }
        // 2) 冷段尾：遇到脏帧计数跳过（它们在写列表里排队），上限 = 容量/分数。
        let limit = (self.frames.len() / self.cfg.max_scan_fraction).max(1);
        for k in 0..self.cold.len().min(limit) {
            let idx = self.cold[self.cold.len() - 1 - k];
            self.stats.free_inspected += 1;
            if self.frames[idx].pins > 0 {
                self.stats.pinned_inspected += 1;
                continue;
            }
            if self.frames[idx].dirty {
                self.stats.dirty_inspected += 1;
                continue; // 脏帧不直接写回——交 Make Free 按序写
            }
            // **老化减半**（Note 104937.1）：计数高于冷却值 ⇒ 不立即淘汰，
            // 减半后继续扫——"计数够高的块即使位于列表尾也不被重用"。
            if self.frames[idx].touches > self.cfg.cool_count {
                self.frames[idx].touches /= 2;
                self.stats.aging_steps += 1;
                continue;
            }
            self.stats.evictions += 1;
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
    fn pick_for_write(&mut self, target: WriteTarget) -> Result<Pick, BufferError> {
        let candidate: Option<(Lsn, [u8; 8], Rdba)> = match target {
            WriteTarget::Key(key) => {
                let Some(idx) = self.find_frame(key) else {
                    return Ok(Pick::None);
                };
                if !self.frames[idx].dirty {
                    return Ok(Pick::None);
                }
                self.frames[idx]
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
        let Some(idx) = self.find_frame(key) else {
            self.drop_write_entry(ws, lsn, rdba);
            return Ok(Pick::Stale);
        };
        if !self.frames[idx].dirty {
            self.drop_write_entry(ws, lsn, rdba); // 防呆（不应发生）
            return Ok(Pick::Stale);
        }
        let Some(fkey) = self.frames[idx].key else {
            return Ok(Pick::None);
        };
        let (handle, block) = (self.resolve)(&fkey.workspace, fkey.rdba)
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

    /// **开始写回**（闩锁内）：冻结页内容与 `mod_seq` 快照（I/O 用副本，
    /// 期间的修改不影响本版写出）。
    fn begin_write(
        &mut self,
        idx: usize,
        key: BufferKey,
        handle: FileHandle,
        block: u32,
    ) -> WriteJob {
        let page = &self.frames[idx].page;
        let page_lsn = page
            .header()
            .map_or(Lsn::from_raw(0).expect("0 合法"), |h| h.page_lsn);
        let mod_seq = page.header().map_or(0, |h| h.mod_seq);
        WriteJob {
            idx,
            key,
            handle,
            block,
            image: page.clone(),
            mod_seq,
            page_lsn,
        }
    }

    /// **写回收尾**（闩锁内）：`mod_seq` 未变 ⇒ 清脏、出写列表、干净未钉住
    /// 帧入 AUX；**变过（期间被再改脏）⇒ 保持脏与条目**，留给下一轮——
    /// 绝不把"更新过的版本"当"已落盘"（PG `BM_JUST_DIRTIED` 的同款判据）。
    fn finish_write(&mut self, job: &WriteJob, wal_synced: bool) {
        self.stats.writes += 1;
        if wal_synced {
            self.stats.wal_syncs += 1;
        }
        let idx = job.idx;
        if self.frames[idx].key != Some(job.key) {
            return; // 帧已换人（脏帧不可被淘汰；防御）
        }
        if self.frames[idx].page.header().map_or(0, |h| h.mod_seq) != job.mod_seq {
            return; // 期间被再改脏：保持脏
        }
        if let Some(lsn) = self.frames[idx].first_dirty.take() {
            self.drop_write_entry(job.key.workspace, lsn, job.key.rdba);
        }
        self.frames[idx].dirty = false;
        if self.frames[idx].pins == 0 && !self.aux.contains(&idx) {
            if let Some(p) = self.hot.iter().position(|&i| i == idx) {
                self.hot.remove(p);
            }
            if let Some(p) = self.cold.iter().position(|&i| i == idx) {
                self.cold.remove(p);
            }
            self.aux.push_front(idx);
            self.stats.aux_moved += 1;
        }
    }

    /// 把一个帧装上新键与内容：出旧链、入桶、落**冷段头**。
    fn attach(&mut self, idx: usize, key: BufferKey, page: Page) {
        Self::detach_from_chains(&mut self.hot, &mut self.cold, &mut self.aux, idx);
        if let Some(old) = self.frames[idx].key.take() {
            let ob = self.bucket_of(old);
            if let Some(p) = self.buckets[ob].iter().position(|&i| i == idx) {
                self.buckets[ob].remove(p);
            }
        }
        self.frames[idx] = Frame {
            key: Some(key),
            page,
            dirty: false,
            first_dirty: None,
            pins: 1,
            touches: self.cfg.cool_count,
            last_touch_ms: self.clock.now_ms(),
        };
        let b = self.bucket_of(key);
        self.buckets[b].push(idx);
        self.cold.push_front(idx); // 新读入/重用 ⇒ 冷段头（不是热段）
    }

    /// **原位替换一个已驻留帧的内容**（同键重新装入）：帧在桶/链上的位置不变
    /// （键未变），只换内容与记账——写列表里的旧条目按旧 `first_dirty` 摘除
    /// （旧内容被权威镜像取代，不再需要写回）。
    fn replace_in_place(&mut self, idx: usize, key: BufferKey, page: Page) {
        debug_assert_eq!(
            self.frames[idx].pins, 0,
            "同键重装时不应有在途卫兵（单写者纪律）"
        );
        if let Some(lsn) = self.frames[idx].first_dirty.take() {
            self.drop_write_entry(key.workspace, lsn, key.rdba);
        }
        self.frames[idx] = Frame {
            key: Some(key),
            page,
            dirty: false,
            first_dirty: None,
            pins: 1,
            touches: self.cfg.cool_count,
            last_touch_ms: self.clock.now_ms(),
        };
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

/// **页卫兵**（`PageGuard`）：钉住一帧，`Drop` = unpin。
pub struct PageGuard<'a, 'io> {
    inner: LatchGuard<'a, Inner<'io>>,
    idx: usize,
}

impl std::fmt::Debug for PageGuard<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PageGuard").field("idx", &self.idx).finish()
    }
}

impl PageGuard<'_, '_> {
    /// 本帧的键。
    #[must_use]
    pub fn key(&self) -> BufferKey {
        self.inner.frames[self.idx].key.expect("钉住的帧必有主")
    }

    /// 页是否脏。
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.inner.frames[self.idx].dirty
    }

    /// **标脏**（写路径在追加完 redo 后调用）：`first_dirty_lsn` 只在**首次**
    /// 变脏时记入——写列表按它排序，重复标脏不改变排序键。
    pub fn mark_dirty(&mut self, first_dirty_lsn: Lsn) {
        let frame = &mut self.inner.frames[self.idx];
        if frame.dirty {
            return;
        }
        frame.dirty = true;
        frame.first_dirty = Some(first_dirty_lsn);
        let key = frame.key.expect("钉住的帧必有主");
        self.inner
            .write_list
            .entry(key.workspace)
            .or_default()
            .insert((first_dirty_lsn, key.rdba));
    }
}

impl std::ops::Deref for PageGuard<'_, '_> {
    type Target = Page;
    fn deref(&self) -> &Page {
        &self.inner.frames[self.idx].page
    }
}

impl std::ops::DerefMut for PageGuard<'_, '_> {
    fn deref_mut(&mut self) -> &mut Page {
        &mut self.inner.frames[self.idx].page
    }
}

impl Drop for PageGuard<'_, '_> {
    fn drop(&mut self) {
        let idx = self.idx;
        self.inner.frames[idx].pins = self.inner.frames[idx].pins.saturating_sub(1);
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
            let mut inner = pool.lock();
            inner
                .write_list
                .entry(WS_A)
                .or_default()
                .insert((lsn(7), rdba(7, 1)));
        }
        pool.make_free().unwrap();
        assert!(
            !pool.lock().write_list.contains_key(&WS_A),
            "失步条目按自身 LSN 清除"
        );
        // 再跑一次也不挂（幂等）。
        pool.make_free().unwrap();
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
}
