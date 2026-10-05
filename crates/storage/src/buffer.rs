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

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Mutex, MutexGuard};

use bicdb_common::seq::Lsn;
use bicdb_workspace::io::{FileHandle, FileIo};

use crate::page::Page;
use crate::pagefile::{self, PageFileError};
use crate::rowid::Rdba;

/// 块定位器：`(工作区标识, rdba) →（页文件句柄、块号）`。
///
/// 带上工作区是因为**池是实例级共享**的：不同工作区各有自己的文件句柄。
pub type PoolResolver<'io> = dyn FnMut(&[u8; 8], Rdba) -> Option<(FileHandle, u32)> + 'io;

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
    fn ensure_durable(&mut self, target: Lsn) -> std::io::Result<()>;
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

/// 池内状态（单闩锁；分区 latch 在 P4 接入）。
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
    wal: Box<dyn WalGuard + 'io>,
    clock: Box<dyn Clock + 'io>,
    cfg: CacheConfig,
    stats: BufferStats,
}

/// **DB Cache**（§5.10；本切片 N = 1 分区）。
pub struct BufferPool<'io> {
    io: &'io dyn FileIo,
    capacity: usize,
    inner: Mutex<Inner<'io>>,
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
        resolve: impl FnMut(&[u8; 8], Rdba) -> Option<(FileHandle, u32)> + 'io,
        wal: impl WalGuard + 'io,
    ) -> Result<Self, BufferError> {
        Self::with_clock(io, capacity, resolve, wal, SystemClock)
    }

    /// 建池（指定时钟——测试用手动时钟控制三秒规则）。
    pub fn with_clock(
        io: &'io dyn FileIo,
        capacity: usize,
        resolve: impl FnMut(&[u8; 8], Rdba) -> Option<(FileHandle, u32)> + 'io,
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
        resolve: impl FnMut(&[u8; 8], Rdba) -> Option<(FileHandle, u32)> + 'io,
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
            inner: Mutex::new(Inner {
                frames: (0..capacity).map(|_| Frame::empty()).collect(),
                virgin: (0..capacity).rev().collect(),
                buckets: vec![Vec::new(); cfg.buckets],
                hot: VecDeque::new(),
                cold: VecDeque::new(),
                aux: VecDeque::new(),
                write_list: BTreeMap::new(),
                resolve: Box::new(resolve),
                wal: Box::new(wal),
                clock: Box::new(clock),
                cfg,
                stats: BufferStats::default(),
            }),
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

    /// **钉住一页**（命中 → touch count；未命中 → 找空闲帧 → 读入 → 冷段头）。
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

        // 未命中：找可复用帧（AUX → 冷段尾；脏帧跳过——它们已在写列表）。
        let victim = match inner.find_reusable() {
            Some(v) => v,
            None => {
                // Make Free：内联 DBWR 批处理（写列表头按序写）后重试一次。
                inner.make_free(self.io)?;
                match inner.find_reusable() {
                    Some(v) => v,
                    None => {
                        inner.stats.fb_wait += 1;
                        return Err(BufferError::FreeBufferWait);
                    }
                }
            }
        };

        // 读入 + 身份核对（串页防线）。
        let (handle, block) = (inner.resolve)(&key.workspace, key.rdba)
            .ok_or(BufferError::Unresolved { rdba: key.rdba })?;
        let page = pagefile::read_page_verified(self.io, handle, block).map_err(|e| match e {
            PageFileError::Damaged { .. } => BufferError::Damaged { rdba: key.rdba },
            PageFileError::Io(e) => BufferError::Io(e),
        })?;
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

        inner.attach(victim, key, page);
        Ok(PageGuard { inner, idx: victim })
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
        let victim = match inner.find_reusable() {
            Some(v) => v,
            None => {
                inner.make_free(self.io)?;
                match inner.find_reusable() {
                    Some(v) => v,
                    None => {
                        inner.stats.fb_wait += 1;
                        return Err(BufferError::FreeBufferWait);
                    }
                }
            }
        };
        inner.attach(victim, key, page);
        Ok(PageGuard { inner, idx: victim })
    }

    /// 写回某一页（若脏）。返回是否真的写了。
    pub fn flush(&self, key: BufferKey) -> Result<bool, BufferError> {
        let mut inner = self.lock();
        let Some(idx) = inner.find_frame(key) else {
            return Ok(false);
        };
        if !inner.frames[idx].dirty {
            return Ok(false);
        }
        inner.write_back(self.io, idx)?;
        Ok(true)
    }

    /// **按序写回一个工作区的全部脏页**（写列表头 → 尾）。
    pub fn flush_workspace(&self, workspace: [u8; 8]) -> Result<FlushReport, BufferError> {
        let mut inner = self.lock();
        let mut report = FlushReport::default();
        loop {
            let next = inner
                .write_list
                .get(&workspace)
                .and_then(|s| s.iter().next().copied());
            let Some((first_dirty, rdba)) = next else {
                break;
            };
            let key = BufferKey::new(workspace, rdba);
            let Some(idx) = inner.find_frame(key) else {
                // 写列表与驻留表失步（不应发生）——防呆清理后继续。
                if let Some(chain) = inner.write_list.get_mut(&workspace) {
                    chain.remove(&(first_dirty, rdba));
                    if chain.is_empty() {
                        inner.write_list.remove(&workspace);
                    }
                }
                continue;
            };
            inner.write_back(self.io, idx)?;
            report.pages_written += 1;
        }
        Ok(report)
    }

    fn lock(&self) -> MutexGuard<'_, Inner<'io>> {
        self.inner.lock().expect("缓冲池闩锁中毒")
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
    fn find_reusable(&mut self) -> Option<usize> {
        // 0) 从未用过的帧最便宜。
        if let Some(idx) = self.virgin.pop() {
            return Some(idx);
        }
        // 1) AUX：干净、未钉住者直接取用（**复用**——计入 evictions）。
        if let Some(p) = self.aux.iter().position(|&i| self.frames[i].pins == 0) {
            let idx = self.aux.remove(p).expect("位置在界内");
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
            let p = self.cold.len() - 1 - k;
            let idx = self.cold.remove(p).expect("位置在界内");
            self.stats.evictions += 1;
            return Some(idx);
        }
        None
    }

    /// **Make Free**：内联 DBWR 批处理——写列表（每工作区）**头**按序写，
    /// 直到凑够一批或没有可写的。写完的干净未钉住帧进 AUX（可复用）。
    fn make_free(&mut self, io: &dyn FileIo) -> Result<(), BufferError> {
        let batch = (self.frames.len() / 64).max(1);
        let mut written = 0usize;
        while written < batch {
            // 取所有工作区里"最老的首次变脏 LSN"最小的那条头。
            let mut pick: Option<(Lsn, [u8; 8], Rdba)> = None;
            for (ws, chain) in &self.write_list {
                if let Some((lsn, rdba)) = chain.iter().next().copied() {
                    if pick.map_or(true, |(l, _, _)| lsn < l) {
                        pick = Some((lsn, *ws, rdba));
                    }
                }
            }
            let Some((_, ws, rdba)) = pick else {
                break;
            };
            let key = BufferKey::new(ws, rdba);
            let Some(idx) = self.find_frame(key) else {
                if let Some(chain) = self.write_list.get_mut(&ws) {
                    chain.remove(&(Lsn::from_raw(0).expect("0 合法"), rdba));
                    if chain.is_empty() {
                        self.write_list.remove(&ws);
                    }
                }
                continue;
            };
            self.write_back(io, idx)?;
            written += 1;
        }
        Ok(())
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

    /// **写回一帧**：WAL 规则 2 → 页文件写 → 清脏、出写列表；干净未钉住 ⇒ 入 AUX。
    fn write_back(&mut self, io: &dyn FileIo, idx: usize) -> Result<(), BufferError> {
        if !self.frames[idx].dirty {
            return Ok(());
        }
        let Some(key) = self.frames[idx].key else {
            self.frames[idx].dirty = false;
            self.frames[idx].first_dirty = None;
            return Ok(());
        };
        let page_lsn = self.frames[idx]
            .page
            .header()
            .map_or(Lsn::from_raw(0).expect("0 合法"), |h| h.page_lsn);

        // WAL 规则 2：redo 先持久化到该页的 `page_lsn`（Oracle KCBB_REDO：推迟写）。
        if page_lsn > self.wal.durable_lsn() {
            self.wal
                .ensure_durable(page_lsn)
                .map_err(BufferError::WalFlush)?;
            self.stats.wal_syncs += 1;
        }

        let (handle, block) = (self.resolve)(&key.workspace, key.rdba)
            .ok_or(BufferError::Unresolved { rdba: key.rdba })?;
        pagefile::write_page(io, handle, block, &mut self.frames[idx].page)
            .map_err(BufferError::Io)?;

        self.stats.writes += 1;
        if let Some(lsn) = self.frames[idx].first_dirty.take() {
            if let Some(chain) = self.write_list.get_mut(&key.workspace) {
                chain.remove(&(lsn, key.rdba));
                if chain.is_empty() {
                    self.write_list.remove(&key.workspace);
                }
            }
        }
        self.frames[idx].dirty = false;
        // 写完的干净未钉住帧 ⇒ AUX（可重用候选；§5.10 的 AUX_MOV）。
        if self.frames[idx].pins == 0 && !self.aux.contains(&idx) {
            let p_hot = self.hot.iter().position(|&i| i == idx);
            let p_cold = self.cold.iter().position(|&i| i == idx);
            if let Some(p) = p_hot {
                self.hot.remove(p);
            }
            if let Some(p) = p_cold {
                self.cold.remove(p);
            }
            self.aux.push_front(idx);
            self.stats.aux_moved += 1;
        }
        Ok(())
    }
}

/// **页卫兵**（`PageGuard`）：钉住一帧，`Drop` = unpin。
pub struct PageGuard<'a, 'io> {
    inner: MutexGuard<'a, Inner<'io>>,
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

    /// 假 WAL 协调口。
    struct FakeWal {
        durable: Lsn,
        fail: bool,
        log: Arc<Mutex<Vec<String>>>,
    }

    impl WalGuard for FakeWal {
        fn durable_lsn(&self) -> Lsn {
            self.durable
        }
        fn ensure_durable(&mut self, target: Lsn) -> std::io::Result<()> {
            if self.fail {
                return Err(std::io::Error::other("假刷盘失败"));
            }
            self.log
                .lock()
                .unwrap()
                .push(format!("wal:ensure:{}", target.as_raw()));
            if target > self.durable {
                self.durable = target;
            }
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
                durable: lsn(0),
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
}
