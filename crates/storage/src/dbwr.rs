//! **DBWR 线程**（§11.7 写侧五角色的写线程；池侧的"真线程化"）。
//!
//! ```text
//! 前台（Make Free）：容量压力下的内联写回——低延迟路径，不变
//! DBWR 线程（本模块）：工作区间轮转、工作区内按首次变脏顺序处理有界批次
//!                     + 前台可 `wake()` 提前触发
//! ```
//!
//! 与 WAL 的唯一交点是**规则 2**（页写前 redo 必须耐久）——池在写回时经
//! `WalGuard` 完成，而 `WalGuard` 现在可以是 [`bicdb_wal::group::WalShared`]
//! 的 `Arc`（**可共享**），所以后台线程刷盘是安全的：
//! 同一个 `redo_io` 闩锁 + 同一个 `synced_lsn` 水位，与前台提交共享组提交。
//!
//! pin、在途写回和未耐久 redo 进入临时跳过清单；硬错误单独报告并停止本写者
//! 对受影响工作区的写回。清单不持有 pin，所有未写出的页保留恢复水位。

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::buffer::{BufferKey, BufferPool, WritebackDeferred, WritebackOutcome};

/// 周期兜底的默认间隔（与 LGWR 的 3 秒同源；DBWR 的"空闲写"档）。
pub const DEFAULT_TICK: Duration = Duration::from_secs(3);

/// DBWR 的运行统计（诊断；失败不静默）。
#[derive(Debug, Default)]
pub struct DbwrStats {
    /// 完成的写回轮次。
    pub passes: u64,
    /// 累计写回的页数。
    pub pages_written: u64,
    /// 硬错误次数（页仍在写列表，本写者停止处理该工作区）。
    pub failures: u64,
    /// Pages temporarily skipped, without pins held by the queue.
    pub deferred_pages: u64,
    /// Current skipped pages by reason (diagnostic gauges).
    pub pinned_pages: u64,
    /// Skipped pages with another writeback already in flight.
    pub in_flight_pages: u64,
    /// Skipped pages waiting for redo durability in their own log stream.
    pub redo_pending_pages: u64,
    /// 最近一次失败的原因。
    pub last_error: Option<String>,
}

/// **DBWR 线程**：周期/按需把脏页按写列表顺序写回。
pub struct Dbwr {
    signal: Arc<(Mutex<bool>, Condvar)>,
    stop: Arc<AtomicBool>,
    stats: Arc<StatsInner>,
    handle: Option<std::thread::JoinHandle<()>>,
}

#[derive(Default)]
struct StatsInner {
    passes: AtomicU64,
    pages_written: AtomicU64,
    failures: AtomicU64,
    last_error: Mutex<Option<String>>,
    deferred_pages: AtomicU64,
    pinned_pages: AtomicU64,
    in_flight_pages: AtomicU64,
    redo_pending_pages: AtomicU64,
}

impl std::fmt::Debug for Dbwr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dbwr")
            .field("stats", &self.stats())
            .finish()
    }
}

impl Dbwr {
    /// **启动**（要求池可 `'static` 共享：生产上池随实例存在）。
    ///
    /// **全局**形态：一条线程扫**所有**分区的脏工作区（N=1 的常规形态；
    /// N>1 用 [`DbwrGroup`]——一个分区一条线程）。
    pub fn start(pool: Arc<BufferPool<'static>>, tick: Duration) -> Self {
        Self::start_scoped(pool, None, tick, |_| {})
    }

    /// **启动（作用域形态）**：`partition = Some(p)` ⇒ 本线程**只**写分区 `p`
    /// 的脏工作区（§5.10："一个分区只由一个写线程负责"——分区之间零争用，
    /// 写列表/链/闩锁都按分区）。
    ///
    /// `prestart(partition)` 在**线程体内、任何写回之前**执行一次——它是
    /// **NUMA 绑定的注入点**（详设 §5 阶段 B："线程创建时即入组"：创建方在
    /// 闭包里把本线程绑进该工作集的节点组；绑定失败只诊断，不阻止写回）。
    pub fn start_scoped(
        pool: impl std::ops::Deref<Target = BufferPool<'static>> + Send + Sync + 'static,
        partition: Option<usize>,
        tick: Duration,
        prestart: impl FnOnce(Option<usize>) + Send + 'static,
    ) -> Self {
        assert!(!tick.is_zero(), "DBWR sleep interval must be positive");
        let signal = Arc::new((Mutex::new(false), Condvar::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(StatsInner {
            passes: AtomicU64::new(0),
            pages_written: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            last_error: Mutex::new(None),
            deferred_pages: AtomicU64::new(0),
            pinned_pages: AtomicU64::new(0),
            in_flight_pages: AtomicU64::new(0),
            redo_pending_pages: AtomicU64::new(0),
        });
        let (sig, st, stp, pool2) = (
            Arc::clone(&signal),
            Arc::clone(&stats),
            Arc::clone(&stop),
            pool,
        );
        let name = match partition {
            Some(p) => format!("bicdb-dbwr-{p}"),
            None => "bicdb-dbwr".to_string(),
        };
        let handle = std::thread::Builder::new()
            .name(name)
            .spawn(move || {
                prestart(partition);
                let mut deferred = DeferredWrites::default();
                let mut next_scan = Instant::now() + tick;
                loop {
                    let notified = {
                        let (lock, cv) = &*sig;
                        let mut pending = lock.lock().unwrap_or_else(|e| e.into_inner());
                        if !*pending {
                            let (guard, _) = cv
                                .wait_timeout_while(
                                    pending,
                                    deferred.next_delay(
                                        next_scan.saturating_duration_since(Instant::now()),
                                    ),
                                    |pending| !*pending && !stp.load(Ordering::SeqCst),
                                )
                                .unwrap_or_else(|e| e.into_inner());
                            pending = guard;
                        }
                        let notified = *pending;
                        *pending = false;
                        notified
                    };
                    if stp.load(Ordering::SeqCst) {
                        break;
                    }
                    let scan = notified || Instant::now() >= next_scan;
                    if scan {
                        next_scan = Instant::now() + tick;
                    }
                    write_back_once(&pool2, &st, partition, &mut deferred, scan);
                }
                // 最后一轮仍不能绕过 pin/WAL 门；完全停机由实例协调者排空。
                write_back_once(&pool2, &st, partition, &mut deferred, true);
            })
            .expect("创建 DBWR 线程");
        Self {
            signal,
            stop,
            stats,
            handle: Some(handle),
        }
    }

    /// **叫醒 DBWR**（新的脏页到达、前台希望尽早落盘时）。
    pub fn wake(&self) {
        let (lock, cv) = &*self.signal;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
        cv.notify_one();
    }

    /// 统计快照。
    #[must_use]
    pub fn stats(&self) -> DbwrStats {
        DbwrStats {
            deferred_pages: self.stats.deferred_pages.load(Ordering::Relaxed),
            pinned_pages: self.stats.pinned_pages.load(Ordering::Relaxed),
            in_flight_pages: self.stats.in_flight_pages.load(Ordering::Relaxed),
            redo_pending_pages: self.stats.redo_pending_pages.load(Ordering::Relaxed),
            passes: self.stats.passes.load(Ordering::Relaxed),
            pages_written: self.stats.pages_written.load(Ordering::Relaxed),
            failures: self.stats.failures.load(Ordering::Relaxed),
            last_error: self
                .stats
                .last_error
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        }
    }

    /// **停止并 join**（含最后一轮收尾写回）。
    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.wake();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Dbwr {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.wake();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// **按分区的一组 DBWR**（§5.10 层级："一个分区只由一个写线程负责"）：
/// 每个分区一条线程，各自只写本分区的脏页；`prestart(partition)` 在每条
/// 线程体内先于写回执行（**NUMA 绑定的注入点**，详设 §5 阶段 B）。
///
/// 与全局 [`Dbwr`] 的关系：N=1 时二者等价（一条线程）；N>1 时分区的
/// 写列表/链/闩锁本就互不相干——分线程后**写回在分区之间并行**，而
/// 每一条写列表仍只有**一个**写者（协议不变：按（首次变脏 LSN, rdba）升序）。
pub struct DbwrGroup {
    writers: Vec<Dbwr>,
}

impl std::fmt::Debug for DbwrGroup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbwrGroup")
            .field("writers", &self.writers.len())
            .field("stats", &self.stats())
            .finish()
    }
}

impl DbwrGroup {
    /// 启动：线程数 = 池的**分区数**；`prestart` 被每条线程各调用一次
    /// （入参 = 该线程负责的分区号）。闭包需要 `Send + Sync`——它被各线程
    /// 共享（NUMA 绑定器本身是 `Arc<NumaBinder>`，满足）。
    pub fn start(
        pool: Arc<BufferPool<'static>>,
        tick: Duration,
        prestart: impl Fn(usize) + Send + Sync + 'static,
    ) -> Self {
        let partitions = pool.partition_count();
        let prestart = Arc::new(prestart);
        let writers = (0..partitions)
            .map(|p| {
                let pre = Arc::clone(&prestart);
                Dbwr::start_scoped(Arc::clone(&pool), Some(p), tick, move |_| pre(p))
            })
            .collect();
        Self { writers }
    }

    /// Start writers for a process-lifetime pool without creating another cache.
    pub fn start_borrowed(pool: &'static BufferPool<'static>, tick: Duration) -> Self {
        Self {
            writers: (0..pool.partition_count())
                .map(|p| Dbwr::start_scoped(pool, Some(p), tick, |_| {}))
                .collect(),
        }
    }

    /// 分区数（= 写线程数）。
    #[must_use]
    pub fn writers(&self) -> usize {
        self.writers.len()
    }

    /// 叫醒**全部**写线程（写线程各自只扫自己的分区，叫醒不会跨分区做无用功
    /// ——真正无侧效的按分区叫醒留待"分区写列表到达"的钩子接入时细化）。
    pub fn wake(&self) {
        for w in &self.writers {
            w.wake();
        }
    }

    /// 聚合统计（各写线程之和；`last_error` 取任一非空）。
    #[must_use]
    pub fn stats(&self) -> DbwrStats {
        let mut out = DbwrStats::default();
        for w in &self.writers {
            let s = w.stats();
            out.passes += s.passes;
            out.pages_written += s.pages_written;
            out.failures += s.failures;
            out.deferred_pages += s.deferred_pages;
            out.pinned_pages += s.pinned_pages;
            out.in_flight_pages += s.in_flight_pages;
            out.redo_pending_pages += s.redo_pending_pages;
            if out.last_error.is_none() {
                out.last_error = s.last_error;
            }
        }
        out
    }

    /// 停止并 join 全部写线程（各自含最后一轮收尾写回）。
    pub fn shutdown(self) {
        for w in self.writers {
            w.shutdown();
        }
    }
}

struct DeferredEntry {
    attempts: u32,
    retry_at: Instant,
    reason: WritebackDeferred,
}

#[derive(Default)]
struct DeferredWrites {
    // Keys are re-resolved every time: no pins, indexes or stale frame pointers.
    entries: BTreeMap<BufferKey, DeferredEntry>,
    fresh: VecDeque<BufferKey>,
    failed: BTreeSet<[u8; 8]>,
    fresh_workspace: Option<[u8; 8]>,
    retry_workspace: Option<[u8; 8]>,
}

impl DeferredWrites {
    fn next_delay(&self, tick: Duration) -> Duration {
        if !self.fresh.is_empty() {
            return Duration::ZERO;
        }
        self.entries
            .values()
            .map(|entry| entry.retry_at.saturating_duration_since(Instant::now()))
            .min()
            .map_or(tick, |delay| delay.min(tick))
    }
}

/// Cursor survives removal of the previously written page. Each workspace gets
/// one turn at a time; the order of pages within each stream is preserved.
fn fair_batch(
    keys: impl IntoIterator<Item = BufferKey>,
    cursor: &mut Option<[u8; 8]>,
    limit: usize,
) -> Vec<BufferKey> {
    let mut groups: BTreeMap<_, VecDeque<_>> = BTreeMap::new();
    for key in keys {
        groups.entry(key.workspace).or_default().push_back(key);
    }
    let mut workspaces: Vec<_> = groups.keys().copied().collect();
    if let Some(last) = *cursor {
        let start = workspaces
            .iter()
            .position(|workspace| *workspace > last)
            .unwrap_or(0);
        workspaces.rotate_left(start);
    }
    let mut order: VecDeque<_> = workspaces.into();
    let mut batch = Vec::with_capacity(limit);
    while batch.len() < limit {
        let Some(workspace) = order.pop_front() else {
            break;
        };
        let pages = groups.get_mut(&workspace).expect("workspace queue");
        if let Some(key) = pages.pop_front() {
            batch.push(key);
            *cursor = Some(workspace);
        }
        if !pages.is_empty() {
            order.push_back(workspace);
        }
    }
    batch
}
fn attempt(
    pool: &BufferPool<'static>,
    stats: &StatsInner,
    deferred: &mut DeferredWrites,
    key: BufferKey,
) {
    if deferred.failed.contains(&key.workspace) || pool.workspace_fault(key.workspace).is_some() {
        deferred.failed.insert(key.workspace);
        // Do not burn consecutive zero-delay batches on a quarantined stream.
        deferred
            .fresh
            .retain(|candidate| candidate.workspace != key.workspace);
        deferred
            .entries
            .retain(|candidate, _| candidate.workspace != key.workspace);
        return;
    }
    match pool.try_flush(key) {
        Ok(WritebackOutcome::Written) => {
            stats.pages_written.fetch_add(1, Ordering::Relaxed);
            deferred.entries.remove(&key);
        }
        Ok(WritebackOutcome::Clean) => {
            deferred.entries.remove(&key);
        }
        Ok(WritebackOutcome::Deferred(reason)) => {
            let attempts = deferred
                .entries
                .get(&key)
                .map_or(0, |entry| entry.attempts)
                .saturating_add(1);
            let delay = Duration::from_millis((5u64 << attempts.min(7)).min(1000));
            deferred.entries.insert(
                key,
                DeferredEntry {
                    attempts,
                    retry_at: Instant::now() + delay,
                    reason,
                },
            );
        }
        Err(error) => {
            // Hard I/O failures are not temporary skips. Preserve the dirty page
            // and isolate this writer's affected workspace until repair/restart.
            deferred.entries.remove(&key);
            pool.quarantine_workspace(key.workspace, format!("DBWR: {error}"));
            deferred.failed.insert(key.workspace);
            stats.failures.fetch_add(1, Ordering::Relaxed);
            *stats.last_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(error.to_string());
        }
    }
}
fn write_back_once(
    pool: &BufferPool<'static>,
    stats: &StatsInner,
    partition: Option<usize>,
    deferred: &mut DeferredWrites,
    scan: bool,
) {
    const BATCH: usize = 256;
    if scan {
        let mut candidates = Vec::new();
        for p in 0..pool.partition_count() {
            if partition.map_or(true, |selected| selected == p) {
                candidates.extend(pool.dirty_keys_in(p));
            }
        }
        // Snapshot only on notification/scan deadline, not every short retry.
        // Prune keys cleaned/evicted by CKPT or foreground writes.
        let resident: BTreeSet<_> = candidates.iter().copied().collect();
        deferred.entries.retain(|key, _| resident.contains(key));
        let limit = candidates.len();
        deferred.fresh = fair_batch(
            candidates.into_iter().filter(|key| {
                !deferred.failed.contains(&key.workspace) && !deferred.entries.contains_key(key)
            }),
            &mut deferred.fresh_workspace,
            limit,
        )
        .into();
    }
    let now = Instant::now();
    for _ in 0..BATCH {
        let Some(key) = deferred.fresh.pop_front() else {
            break;
        };
        attempt(pool, stats, deferred, key);
    }
    // Revisit after the current writable batch; future passes provide the
    // periodic fallback when a pin release or LGWR completion has no signal.
    // Oldest retry deadline first within a workspace. Repeatedly skipped low
    // keys move behind other due pages, including when the periodic tick is
    // longer than all backoff delays.
    let mut due: Vec<_> = deferred
        .entries
        .iter()
        .filter(|(_, entry)| entry.retry_at <= now)
        .map(|(key, entry)| (entry.retry_at, *key))
        .collect();
    due.sort_unstable();
    let revisit = fair_batch(
        due.into_iter().map(|(_, key)| key),
        &mut deferred.retry_workspace,
        BATCH,
    );
    for key in revisit {
        attempt(pool, stats, deferred, key);
    }
    let mut log_targets = BTreeMap::new();
    for (key, entry) in &deferred.entries {
        if let WritebackDeferred::RedoPending(target) = entry.reason {
            let required = log_targets.entry(key.workspace).or_insert(target);
            *required = (*required).max(target);
        }
    }
    for (workspace, target) in log_targets {
        if deferred.failed.contains(&workspace) || pool.workspace_fault(workspace).is_some() {
            continue;
        }
        if let Err(error) = pool.request_redo_flush(workspace, target) {
            pool.quarantine_workspace(workspace, format!("LGWR request: {error}"));
            deferred.failed.insert(workspace);
            deferred.entries.retain(|key, _| key.workspace != workspace);
            stats.failures.fetch_add(1, Ordering::Relaxed);
            *stats.last_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(error.to_string());
        }
    }
    stats
        .deferred_pages
        .store(deferred.entries.len() as u64, Ordering::Relaxed);
    let (mut pinned, mut in_flight, mut redo_pending) = (0, 0, 0);
    for entry in deferred.entries.values() {
        match entry.reason {
            WritebackDeferred::Pinned => pinned += 1,
            WritebackDeferred::InFlight => in_flight += 1,
            WritebackDeferred::RedoPending(_) => redo_pending += 1,
        }
    }
    stats.pinned_pages.store(pinned, Ordering::Relaxed);
    stats.in_flight_pages.store(in_flight, Ordering::Relaxed);
    stats
        .redo_pending_pages
        .store(redo_pending, Ordering::Relaxed);
    stats.passes.fetch_add(1, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::{BufferKey, BufferPool, CacheConfig, SystemClock, WalGuard};
    use crate::page::{Page, PageType};
    use crate::pagefile;
    use crate::rowid::Rdba;
    use bicdb_common::seq::Lsn;
    use bicdb_workspace::io::{FileHandle, FileIo, MemFileIo, OpenOptions};
    use std::path::Path;
    use std::sync::Mutex;

    const WS: [u8; 8] = [9u8; 8];
    const DATA_F: &str = "/mem/data.dat";

    /// 记录**次序**的共用日志（WAL 事件与 I/O 事件按发生顺序追加）。
    type Log = Arc<Mutex<Vec<String>>>;

    struct OrderWal {
        durable: AtomicU64,
        log: Log,
    }
    impl WalGuard for OrderWal {
        fn durable_lsn(&self) -> Lsn {
            Lsn::from_raw(self.durable.load(Ordering::SeqCst)).unwrap()
        }
        fn ensure_durable(&self, target: Lsn) -> std::io::Result<()> {
            self.log
                .lock()
                .unwrap()
                .push(format!("wal:ensure:{}", target.as_raw()));
            self.durable.fetch_max(target.as_raw(), Ordering::SeqCst);
            Ok(())
        }
    }

    struct OrderIo {
        inner: MemFileIo,
        log: Log,
    }
    impl FileIo for OrderIo {
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

    /// **测试专用**：内存 I/O 泄成 `'static`（线程要求；生产上 I/O 口随
    /// 工作区/实例存在，由守护进程持有，不需要泄）。
    fn leaked_io(log: Log) -> &'static OrderIo {
        let inner = MemFileIo::new();
        inner.add_dir("/mem");
        Box::leak(Box::new(OrderIo { inner, log }))
    }

    #[test]
    fn hard_failure_quarantines_workspace_for_other_writers_but_not_healthy_workspace() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let io = leaked_io(Arc::clone(&log));
        let other = [10; 8];
        let handles: Vec<_> = [(WS, "/mem/bad"), (other, "/mem/good")]
            .into_iter()
            .map(|(ws, path)| {
                let file =
                    crate::datafile::DataFile::create(io, Path::new(path), 3, 3, ws, 512).unwrap();
                let handle = file.handle();
                pagefile::write_page(io, handle, 1, &mut Page::new(PageType::HeapTable, ws, 3, 1))
                    .unwrap();
                Box::leak(Box::new(file));
                handle
            })
            .collect();
        let pool = BufferPool::new(
            io,
            4,
            move |ws, r| Some((handles[usize::from(*ws == other)], r.block_id())),
            OrderWal {
                durable: AtomicU64::new(100),
                log,
            },
        )
        .unwrap();
        let bad = BufferKey::new(WS, Rdba::from_parts(3, 1).unwrap());
        let good = BufferKey::new(other, Rdba::from_parts(3, 1).unwrap());
        {
            let mut guard = pool.pin(bad).unwrap();
            let mut header = guard.header().unwrap();
            header.file_id = 99;
            guard.write_header(&header);
            guard.mark_dirty(Lsn::from_raw(1).unwrap());
        }
        {
            let mut guard = pool.pin(good).unwrap();
            guard.mark_dirty(Lsn::from_raw(1).unwrap());
        }
        let stats = StatsInner::default();
        let mut first_writer = DeferredWrites::default();
        attempt(&pool, &stats, &mut first_writer, bad);
        assert!(pool.workspace_fault(WS).is_some());
        assert_eq!(stats.failures.load(Ordering::Relaxed), 1);
        let mut other_writer = DeferredWrites::default();
        attempt(&pool, &stats, &mut other_writer, bad);
        assert_eq!(
            stats.failures.load(Ordering::Relaxed),
            1,
            "another writer must observe shared quarantine"
        );
        assert!(other_writer.failed.contains(&WS));
        attempt(&pool, &stats, &mut other_writer, good);
        assert_eq!(pool.dirty_len(WS), 1);
        assert_eq!(pool.dirty_len(other), 0);
        assert!(pool.workspace_fault(other).is_none());
    }

    #[test]
    fn batch_is_fair_even_when_previous_page_disappears() {
        let key = |ws, block| BufferKey::new([ws; 8], Rdba::from_parts(3, block).unwrap());
        let mut cursor = None;
        let first = fair_batch([key(1, 1), key(2, 1), key(3, 1)], &mut cursor, 2);
        assert_eq!(first, vec![key(1, 1), key(2, 1)]);
        // Last written key no longer exists; workspace 1 has a large fresh load.
        let candidates = (2..600).map(|block| key(1, block)).chain([key(3, 1)]);
        let next = fair_batch(candidates, &mut cursor, 2);
        assert_eq!(next, vec![key(3, 1), key(1, 2)]);
    }

    #[test]
    fn pinned_head_does_not_block_tail_and_retry_retains_recovery_watermark() {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let io = leaked_io(Arc::clone(&log));
        let file = Box::leak(Box::new(
            crate::datafile::DataFile::create(io, Path::new(DATA_F), 3, 3, WS, 512).unwrap(),
        ));
        let handle = file.handle();
        for block in 1..=2 {
            pagefile::write_page(
                io,
                handle,
                block,
                &mut Page::new(PageType::HeapTable, WS, 3, block),
            )
            .unwrap();
        }
        let pool = BufferPool::new(
            io,
            4,
            move |ws, rdba| (*ws == WS && rdba.file_id() == 3).then_some((handle, rdba.block_id())),
            OrderWal {
                durable: AtomicU64::new(8),
                log: Arc::clone(&log),
            },
        )
        .unwrap();
        let keys: Vec<_> = (1..=2)
            .map(|block| BufferKey::new(WS, Rdba::from_parts(3, block).unwrap()))
            .collect();
        for (key, first) in keys.iter().zip([1, 2]) {
            let mut guard = pool.pin(*key).unwrap();
            let mut header = guard.header().unwrap();
            header.page_lsn = Lsn::from_raw(8).unwrap();
            guard.write_header(&header);
            guard.mark_dirty(Lsn::from_raw(first).unwrap());
        }
        log.lock().unwrap().clear();
        let held = pool.pin(keys[0]).unwrap();
        let stats = StatsInner::default();
        let mut deferred = DeferredWrites::default();
        write_back_once(&pool, &stats, None, &mut deferred, true);
        assert_eq!(stats.pages_written.load(Ordering::Relaxed), 1);
        assert_eq!(stats.pinned_pages.load(Ordering::Relaxed), 1);
        assert_eq!(pool.low_water(WS), Some(Lsn::from_raw(1).unwrap()));
        assert_eq!(pool.dirty_len(WS), 1);
        assert!(matches!(
            deferred.entries[&keys[0]].reason,
            WritebackDeferred::Pinned
        ));
        drop(held);
        deferred.entries.get_mut(&keys[0]).unwrap().retry_at = Instant::now();
        write_back_once(&pool, &stats, None, &mut deferred, false);
        assert_eq!(pool.dirty_len(WS), 0);
        assert_eq!(pool.low_water(WS), None);
        assert!(deferred.entries.is_empty());
        assert_eq!(stats.pages_written.load(Ordering::Relaxed), 2);

        // A foreground writer/CKPT may clean a deferred page before its retry.
        let mut held = pool.pin(keys[0]).unwrap();
        held.mark_dirty(Lsn::from_raw(8).unwrap());
        write_back_once(&pool, &stats, None, &mut deferred, true);
        assert_eq!(deferred.entries.len(), 1);
        drop(held);
        pool.flush(keys[0]).unwrap();
        write_back_once(&pool, &stats, None, &mut deferred, true);
        assert!(deferred.entries.is_empty(), "过期清单必须随脏页快照清理");
        assert_eq!(stats.deferred_pages.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn redo_requests_coalesce_to_maximum_lsn_within_workspace() {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let io = leaked_io(Arc::clone(&log));
        let file = Box::leak(Box::new(
            crate::datafile::DataFile::create(io, Path::new(DATA_F), 3, 3, WS, 512).unwrap(),
        ));
        let handle = file.handle();
        for block in 1..=2 {
            pagefile::write_page(
                io,
                handle,
                block,
                &mut Page::new(PageType::HeapTable, WS, 3, block),
            )
            .unwrap();
        }
        let router = Arc::new(crate::wal_router::WorkspaceWalRouter::default());
        router
            .register(
                WS,
                Arc::new(OrderWal {
                    durable: AtomicU64::new(0),
                    log: Arc::clone(&log),
                }),
            )
            .unwrap();
        let (send, receive) = std::sync::mpsc::channel();
        router
            .set_flush_request_handler(Arc::new(move |ws, target| {
                send.send((ws, target)).unwrap();
            }))
            .unwrap();
        let pool = BufferPool::new(
            io,
            4,
            move |ws, rdba| (*ws == WS && rdba.file_id() == 3).then_some((handle, rdba.block_id())),
            router,
        )
        .unwrap();
        for block in 1..=2 {
            let key = BufferKey::new(WS, Rdba::from_parts(3, block).unwrap());
            let mut guard = pool.pin(key).unwrap();
            let mut header = guard.header().unwrap();
            header.page_lsn = Lsn::from_raw(u64::from(block) * 4).unwrap();
            guard.write_header(&header);
            guard.mark_dirty(header.page_lsn);
        }
        log.lock().unwrap().clear();
        let stats = StatsInner::default();
        let mut deferred = DeferredWrites::default();
        write_back_once(&pool, &stats, None, &mut deferred, true);
        assert_eq!(receive.try_recv().unwrap(), (WS, Lsn::from_raw(8).unwrap()));
        assert!(
            receive.try_recv().is_err(),
            "同一日志流每轮只提交合并的目标"
        );
        assert!(
            log.lock().unwrap().is_empty(),
            "扫描阶段不能执行日志或数据 I/O"
        );
        assert_eq!(pool.dirty_len(WS), 2);
        assert_eq!(pool.low_water(WS), Some(Lsn::from_raw(4).unwrap()));
    }

    #[test]
    fn snapshot_backlog_drains_without_rescanning_or_waiting_for_tick() {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let io = leaked_io(Arc::clone(&log));
        let file = Box::leak(Box::new(
            crate::datafile::DataFile::create(io, Path::new(DATA_F), 3, 3, WS, 512).unwrap(),
        ));
        let handle = file.handle();
        for block in 1..=300 {
            pagefile::write_page(
                io,
                handle,
                block,
                &mut Page::new(PageType::HeapTable, WS, 3, block),
            )
            .unwrap();
        }
        let pool = BufferPool::new(
            io,
            304,
            move |ws, rdba| (*ws == WS && rdba.file_id() == 3).then_some((handle, rdba.block_id())),
            OrderWal {
                durable: AtomicU64::new(8),
                log: Arc::clone(&log),
            },
        )
        .unwrap();
        for block in 1..=300 {
            let key = BufferKey::new(WS, Rdba::from_parts(3, block).unwrap());
            let mut guard = pool.pin(key).unwrap();
            guard.mark_dirty(Lsn::from_raw(8).unwrap());
        }
        let stats = StatsInner::default();
        let mut deferred = DeferredWrites::default();
        write_back_once(&pool, &stats, None, &mut deferred, true);
        assert_eq!(stats.pages_written.load(Ordering::Relaxed), 256);
        assert_eq!(deferred.fresh.len(), 44);
        assert_eq!(deferred.next_delay(Duration::from_secs(60)), Duration::ZERO);
        write_back_once(&pool, &stats, None, &mut deferred, false);
        assert_eq!(pool.dirty_len(WS), 0);
        assert_eq!(stats.pages_written.load(Ordering::Relaxed), 300);
        assert!(deferred.fresh.is_empty());
        assert!(deferred.entries.is_empty());
    }

    #[test]
    fn idle_dbwr_ignores_notifications_without_work() {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let io = leaked_io(Arc::clone(&log));
        let pool = Arc::new(
            BufferPool::new(
                io,
                2,
                |_, _| None,
                OrderWal {
                    durable: AtomicU64::new(0),
                    log,
                },
            )
            .unwrap(),
        );
        let (ready, started) = std::sync::mpsc::channel();
        let dbwr = Dbwr::start_scoped(pool, None, Duration::from_secs(5), move |_| {
            ready.send(()).unwrap();
        });
        started.recv_timeout(Duration::from_secs(1)).unwrap();
        for _ in 0..10 {
            let _guard = dbwr.signal.0.lock().unwrap();
            dbwr.signal.1.notify_one();
            drop(_guard);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(dbwr.stats().passes, 0);
        let start = Instant::now();
        dbwr.shutdown();
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn oldest_due_retry_has_priority_over_repeatedly_skipped_low_keys() {
        let key = |block| BufferKey::new(WS, Rdba::from_parts(3, block).unwrap());
        let now = Instant::now();
        let mut deferred = DeferredWrites::default();
        for block in 1..=600 {
            deferred.entries.insert(
                key(block),
                DeferredEntry {
                    attempts: 1,
                    retry_at: now,
                    reason: WritebackDeferred::Pinned,
                },
            );
        }
        // A previous batch retried the lowest keys; they must go to the tail.
        for block in 1..=256 {
            deferred.entries.get_mut(&key(block)).unwrap().retry_at =
                now + Duration::from_millis(1);
        }
        let mut due: Vec<_> = deferred
            .entries
            .iter()
            .map(|(key, entry)| (entry.retry_at, *key))
            .collect();
        due.sort_unstable();
        let next = fair_batch(
            due.into_iter().map(|(_, key)| key),
            &mut deferred.retry_workspace,
            256,
        );
        assert_eq!(next.first(), Some(&key(257)));
        assert_eq!(next.last(), Some(&key(512)));
    }

    #[test]
    fn dbwr_skips_pending_redo_then_writes_after_lgwr() {
        // DBWR must not perform a blocking WAL flush during its scan. An
        // independent LGWR advances durability before the page can be written.
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let io = leaked_io(Arc::clone(&log));
        let data_handle = {
            let file = Box::leak(Box::new(
                crate::datafile::DataFile::create(io, Path::new(DATA_F), 3, 3, WS, 512).unwrap(),
            ));
            let h = file.handle();
            let mut page = Page::new(PageType::HeapTable, WS, 3, 1);
            pagefile::write_page(io, h, 1, &mut page).unwrap();
            h
        };
        let wal = Arc::new(OrderWal {
            durable: AtomicU64::new(0),
            log: Arc::clone(&log),
        });
        let pool = Arc::new(
            BufferPool::with_config(
                io,
                8,
                move |_ws, r| {
                    if r.file_id() == 3 {
                        Some((data_handle, r.block_id()))
                    } else {
                        None
                    }
                },
                Arc::clone(&wal),
                SystemClock,
                CacheConfig::for_capacity(8),
            )
            .unwrap(),
        );
        let page_lsn = Lsn::from_raw(4_096).unwrap();
        let key = BufferKey::new(WS, Rdba::from_parts(3, 1).unwrap());
        {
            let mut g = pool.pin(key).unwrap();
            g.as_bytes_mut()[4096] = 0xAB;
            let mut header = g.header().unwrap();
            header.page_lsn = page_lsn;
            g.write_header(&header);
            g.mark_dirty(page_lsn);
        }
        log.lock().unwrap().clear();

        // Long periodic tick: the deferred queue supplies its own short retry.
        let dbwr = Dbwr::start(Arc::clone(&pool), Duration::from_secs(60));
        dbwr.wake();
        let deadline = Instant::now() + Duration::from_secs(5);
        while dbwr.stats().redo_pending_pages == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(dbwr.stats().redo_pending_pages, 1);
        assert_eq!(pool.dirty_len(WS), 1);
        assert!(
            log.lock().unwrap().is_empty(),
            "未耐久 redo 不允许页 I/O，也不由 DBWR 同步催刷"
        );
        std::thread::spawn(move || wal.ensure_durable(page_lsn).unwrap())
            .join()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while pool.dirty_len(WS) > 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(pool.dirty_len(WS), 0, "后台把脏页写回");
        let stats = dbwr.stats();
        dbwr.shutdown();

        // **次序**：WAL 事件先于页 I/O 事件。
        let events = log.lock().unwrap().clone();
        let wal_at = events.iter().position(|e| e == "wal:ensure:4096");
        let io_at = events.iter().position(|e| e.starts_with("io:write:"));
        assert!(wal_at.is_some(), "写页前催刷 redo：{events:?}");
        assert!(io_at.is_some(), "页写出：{events:?}");
        assert!(wal_at < io_at, "次序必须是先 WAL 后页：{events:?}");
        assert!(stats.pages_written >= 1);
        assert_eq!(stats.failures, 0);
    }

    #[test]
    fn one_workspace_uses_multiple_writers_without_losing_pinned_low_water() {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let io = leaked_io(Arc::clone(&log));
        let file = Box::leak(Box::new(
            crate::datafile::DataFile::create(io, Path::new(DATA_F), 3, 3, WS, 512).unwrap(),
        ));
        let handle = file.handle();
        for block in 1..=16 {
            pagefile::write_page(
                io,
                handle,
                block,
                &mut Page::new(PageType::HeapTable, WS, 3, block),
            )
            .unwrap();
        }
        let pool = Arc::new(
            BufferPool::with_partitions(
                io,
                2,
                8,
                move |ws, rdba| {
                    (*ws == WS && rdba.file_id() == 3).then_some((handle, rdba.block_id()))
                },
                OrderWal {
                    durable: AtomicU64::new(8),
                    log: Arc::clone(&log),
                },
                SystemClock,
                CacheConfig::for_capacity(8),
            )
            .unwrap(),
        );
        let keys: Vec<_> = (1..=16)
            .map(|block| BufferKey::new(WS, Rdba::from_parts(3, block).unwrap()))
            .collect();
        let left = *keys
            .iter()
            .find(|key| pool.partition_for(**key) == 0)
            .unwrap();
        let right = *keys
            .iter()
            .find(|key| pool.partition_for(**key) == 1)
            .unwrap();
        for (key, first) in [(left, 1), (right, 2)] {
            let mut guard = pool.pin(key).unwrap();
            let mut header = guard.header().unwrap();
            header.page_lsn = Lsn::from_raw(8).unwrap();
            guard.write_header(&header);
            guard.as_bytes_mut()[4096] = 0x5A;
            guard.mark_dirty(Lsn::from_raw(first).unwrap());
        }
        let held = pool.pin(left).unwrap();
        let group = DbwrGroup::start(Arc::clone(&pool), Duration::from_secs(60), |_| {});
        group.wake();
        let deadline = Instant::now() + Duration::from_secs(5);
        while pool.dirty_len(WS) == 2 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(pool.dirty_len_in(WS, 0), 1);
        assert_eq!(
            pool.dirty_len_in(WS, 1),
            0,
            "另一分区的 DBWR 不被占用页阻塞"
        );
        assert_eq!(pool.low_water(WS), Some(Lsn::from_raw(1).unwrap()));
        drop(held);
        group.wake();
        let deadline = Instant::now() + Duration::from_secs(5);
        while pool.dirty_len(WS) != 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(pool.dirty_len(WS), 0);
        assert_eq!(pool.low_water(WS), None);
        group.shutdown();
        for key in [left, right] {
            let page = pagefile::read_page_verified(io, handle, key.rdba.block_id()).unwrap();
            assert_eq!(page.as_bytes()[4096], 0x5A);
        }
    }

    #[test]
    fn partitioned_writers_flush_their_own_partition_and_run_the_binding_hook() {
        // §5.10："一个分区只由一个写线程负责"——每分区一条线程，各自只扫
        // 本分区的脏工作区；`prestart` 钩子在线程体内、写回之前执行
        // （NUMA 绑定的注入点，入参 = 该线程负责的分区号）。
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let io = leaked_io(Arc::clone(&log));
        // 两个工作区、两个文件（file 3 = WS_A、file 4 = WS_B），各一页。
        const WS_B: [u8; 8] = [4u8; 8];
        let (ha, hb) = {
            let fa = Box::leak(Box::new(
                crate::datafile::DataFile::create(io, Path::new("/mem/pa.dat"), 3, 3, WS, 512)
                    .unwrap(),
            ));
            let fb = Box::leak(Box::new(
                crate::datafile::DataFile::create(io, Path::new("/mem/pb.dat"), 4, 3, WS_B, 512)
                    .unwrap(),
            ));
            for (h, ws, fid) in [(fa.handle(), WS, 3u16), (fb.handle(), WS_B, 4)] {
                let mut page = Page::new(PageType::HeapTable, ws, fid, 1);
                pagefile::write_page(io, h, 1, &mut page).unwrap();
            }
            (fa.handle(), fb.handle())
        };
        let pool = Arc::new(
            BufferPool::with_partitions(
                io,
                2,
                4,
                move |ws, r| match (*ws, r.file_id()) {
                    (w, 3) if w == WS => Some((ha, r.block_id())),
                    (w, 4) if w == WS_B => Some((hb, r.block_id())),
                    _ => None,
                },
                OrderWal {
                    durable: AtomicU64::new(8),
                    log: Arc::clone(&log),
                },
                SystemClock,
                CacheConfig::for_capacity(4),
            )
            .unwrap(),
        );
        let (pa, pb) = (
            pool.partition_for(BufferKey::new(WS, Rdba::from_parts(3, 1).unwrap())),
            pool.partition_for(BufferKey::new(WS_B, Rdba::from_parts(4, 1).unwrap())),
        );
        assert_ne!(pa, pb, "夹具必须落两个不同分区");
        for (ws, fid, lsn_v) in [(WS, 3u16, 8u64), (WS_B, 4, 4)] {
            let key = BufferKey::new(ws, Rdba::from_parts(fid, 1).unwrap());
            let mut g = pool.pin(key).unwrap();
            g.as_bytes_mut()[4096] = 0x5A;
            let mut header = g.header().unwrap();
            header.page_lsn = Lsn::from_raw(lsn_v).unwrap();
            g.write_header(&header);
            g.mark_dirty(Lsn::from_raw(lsn_v).unwrap());
        }
        assert_eq!(pool.dirty_workspaces_in(pa), vec![WS]);
        assert_eq!(pool.dirty_workspaces_in(pb), vec![WS_B]);

        // 钩子记录"哪个分区在哪个线程里启动"——线程体先于写回。
        let seen: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::new()));
        let seen2 = Arc::clone(&seen);
        let group = DbwrGroup::start(Arc::clone(&pool), Duration::from_secs(60), move |p| {
            seen2.lock().unwrap().push(p)
        });
        assert_eq!(group.writers(), 2, "每分区一条写线程");
        group.wake();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while (pool.dirty_len(WS) > 0 || pool.dirty_len(WS_B) > 0)
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(pool.dirty_len(WS), 0, "分区 0 的脏页写回");
        assert_eq!(pool.dirty_len(WS_B), 0, "分区 1 的脏页写回");
        let stats = group.stats();
        group.shutdown();
        assert_eq!(stats.pages_written, 2, "两页都写出（聚合统计）");
        assert_eq!(stats.failures, 0);

        let mut seen = seen.lock().unwrap().clone();
        seen.sort_unstable();
        assert_eq!(
            seen,
            vec![pa.min(pb), pa.max(pb)],
            "每条线程各跑一次钩子（自己的分区号）"
        );
    }
}
