//! **写入侧的后台线程**（§11.7 的五个角色里的 LGWR；DBWR 在缓冲池侧）。
//!
//! ```text
//! 前台（提交路径）：追加记录 → flush 到自己的目标（组提交：共享同一次 fsync）
//! LGWR 线程（本模块）：**周期兜底**（≈3 s，§11.5.5 的写盘触发 ③）
//!                     + 按需唤醒（`wake()`——前台也可把刷盘"外包"给它）
//! ```
//!
//! 与前台的关系：两者都走 [`WalShared`] 的同一把 `redo_io` 闩锁与同一个
//! `synced_lsn` 水位——**组提交（组内一次 fsync）与"刷盘失败不前进水位"
//! 的纪律对两条路径同时成立**，不存在"两个写者把日志写乱"的可能。
//!
//! **降级纪律**：后台刷盘失败**不 panic、不重试风暴**——记进统计、等下一轮
//! （周期兜底的意义正是"下一轮会再来"）；真正的持久性判定仍由前台的
//! `flush_to` 返回错误来承担（提交路径不会因为后台失败而误报成功）。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::group::WalShared;

/// 周期兜底的默认间隔（§11.5.5 写盘触发 ③：约 3 秒）。
pub const DEFAULT_TICK: Duration = Duration::from_secs(3);

/// LGWR 的运行统计（诊断；失败不静默）。
#[derive(Debug, Default)]
pub struct LgwrStats {
    /// 成功刷盘次数。
    pub flushes: u64,
    /// 失败次数（下一轮重试；错误文本见 `last_error`）。
    pub failures: u64,
    /// 最近一次失败的原因。
    pub last_error: Option<String>,
}

/// **LGWR 线程**：周期 + 按需把日志缓冲刷到盘上。
///
/// `shutdown()` 会唤醒并 join（**退出前做最后一次刷盘**——把手上的日志交干）。
pub struct Lgwr {
    /// `(待刷标志, 条件变量)`：`wake()` 置位并通知。
    signal: Arc<(Mutex<bool>, Condvar)>,
    stop: Arc<AtomicBool>,
    stats: Arc<StatsInner>,
    handle: Option<std::thread::JoinHandle<()>>,
}

struct StatsInner {
    flushes: AtomicU64,
    failures: AtomicU64,
    last_error: Mutex<Option<String>>,
}

impl std::fmt::Debug for Lgwr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lgwr")
            .field("stats", &self.stats())
            .finish()
    }
}

impl Lgwr {
    /// **启动**（`tick` = 周期兜底间隔；`wake()` 可随时提前触发）。
    pub fn start(wal: Arc<WalShared<'static>>, tick: Duration) -> Self {
        Self::start_scoped(wal, tick, || {})
    }

    /// **启动（带创建时钩子）**：`prestart` 在**线程体内、任何刷盘之前**
    /// 执行一次——NUMA 阶段 B 的"LGWR 创建时入组"注入点（详设 §5；与
    /// `storage::dbwr::Dbwr::start_scoped` 同形）。钩子在 LGWR 线程里跑
    /// ⇒ `NumaBinder::bind_to_node` 绑的就是 LGWR 自己；**本地性是优化
    /// 不是正确性**——钩子失败不得让线程失败（由钩子自己记诊断）。
    pub fn start_scoped(
        wal: Arc<WalShared<'static>>,
        tick: Duration,
        prestart: impl FnOnce() + Send + 'static,
    ) -> Self {
        assert!(!tick.is_zero(), "LGWR sleep interval must be positive");
        let signal = Arc::new((Mutex::new(false), Condvar::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(StatsInner {
            flushes: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            last_error: Mutex::new(None),
        });
        let (sig, st, stp, wal2) = (
            Arc::clone(&signal),
            Arc::clone(&stats),
            Arc::clone(&stop),
            Arc::clone(&wal),
        );
        let handle = std::thread::Builder::new()
            .name("bicdb-lgwr".into())
            .spawn(move || {
                // 创建时钩子：本线程、首次刷盘之前（NUMA 绑定注入点）。
                prestart();
                loop {
                    {
                        let (lock, cv) = &*sig;
                        let mut pending = lock.lock().unwrap_or_else(|e| e.into_inner());
                        if !*pending {
                            let (guard, _) = cv
                                .wait_timeout_while(pending, tick, |pending| {
                                    !*pending && !stp.load(Ordering::SeqCst)
                                })
                                .unwrap_or_else(|e| e.into_inner());
                            pending = guard;
                        }
                        *pending = false;
                    }
                    if stp.load(Ordering::SeqCst) {
                        break;
                    }
                    flush_once(&wal2, &st);
                }
                // 退出前最后一次：把手上的日志交干（尽力而为）。
                flush_once(&wal2, &st);
            })
            .expect("创建 LGWR 线程");
        Self {
            signal,
            stop,
            stats,
            handle: Some(handle),
        }
    }

    /// **叫醒 LGWR**（前台追加后/提交前的"通知 LGWR"；周期兜底之外的手段）。
    pub fn wake(&self) {
        let (lock, cv) = &*self.signal;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
        cv.notify_one();
    }

    /// 统计快照。
    #[must_use]
    pub fn stats(&self) -> LgwrStats {
        LgwrStats {
            flushes: self.stats.flushes.load(Ordering::Relaxed),
            failures: self.stats.failures.load(Ordering::Relaxed),
            last_error: self
                .stats
                .last_error
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        }
    }

    /// **停止并 join**（含一次收尾刷盘）。
    pub fn shutdown(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.wake();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Lgwr {
    fn drop(&mut self) {
        // 忘了显式 shutdown 也要把线程收干净（幂等：handle 取走即不再 join）。
        self.stop.store(true, Ordering::SeqCst);
        self.wake();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn flush_once(wal: &WalShared<'static>, stats: &StatsInner) {
    let target = wal.appended_lsn();
    match wal.flush_to(target) {
        Ok(_) => {
            stats.flushes.fetch_add(1, Ordering::Relaxed);
        }
        Err(e) => {
            stats.failures.fetch_add(1, Ordering::Relaxed);
            *stats.last_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(e.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::group::{GroupSpec, GroupWriter};
    use crate::record::RedoRecord;
    use bicdb_common::seq::Lsn;
    use bicdb_storage::controlfile::{ArchiveRecord, ControlFile, RedoEntries, WorkspaceEntry};
    use bicdb_workspace::id::WorkspaceId;
    use bicdb_workspace::io::MemFileIo;
    use std::path::Path;

    const A: &str = "/mem/control01.ctl";
    const B: &str = "/mem/control02.ctl";
    const WAL: &str = "/mem/wal";

    fn lsn(v: u64) -> Lsn {
        Lsn::from_raw(v).unwrap()
    }

    /// **测试专用**：把内存 I/O 泄成 `'static`（线程要求）。
    /// 生产路径上 I/O 口随工作区生命周期存在（守护进程持有），不需要泄。
    fn leaked_io() -> &'static MemFileIo {
        Box::leak(Box::new(MemFileIo::new()))
    }

    fn setup() -> (Arc<WalShared<'static>>, GroupWriter<'static, 'static>) {
        let io = leaked_io();
        io.add_dir("/mem");
        io.add_dir(WAL);
        let cf: &'static mut ControlFile<'static> = Box::leak(Box::new(
            ControlFile::format(
                io,
                Path::new(A),
                Path::new(B),
                &WorkspaceEntry {
                    workspace_id: WorkspaceId::from_raw(1).unwrap(),
                    created_at: 0,
                    derived_from: None,
                    derived_at_seq: bicdb_common::seq::CommitSeq::from_raw(0).unwrap(),
                },
                &RedoEntries::new(2, 1).unwrap(),
                &ArchiveRecord::default(),
            )
            .unwrap(),
        ));
        let writer = GroupWriter::create(
            io,
            cf,
            Path::new(WAL),
            GroupSpec::new(2, 1, 64).unwrap(),
            lsn(0),
        )
        .unwrap();
        let shared = writer.shared();
        (shared, writer)
    }

    #[test]
    fn idle_lgwr_ignores_notifications_without_work() {
        let (wal, _writer) = setup();
        let (ready, started) = std::sync::mpsc::channel();
        let lgwr = Lgwr::start_scoped(wal, Duration::from_secs(5), move || {
            ready.send(()).unwrap();
        });
        started.recv_timeout(Duration::from_secs(1)).unwrap();
        for _ in 0..10 {
            // Simulate a spurious notification without setting the work predicate.
            let _guard = lgwr.signal.0.lock().unwrap();
            lgwr.signal.1.notify_one();
            drop(_guard);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(lgwr.stats().flushes, 0);
        let start = std::time::Instant::now();
        lgwr.shutdown();
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn lgwr_flushes_on_demand_and_on_shutdown() {
        let (wal, mut writer) = setup();
        let lgwr = Lgwr::start(Arc::clone(&wal), Duration::from_millis(50));

        // 追加两条记录：还没刷盘。
        writer.append(|l| RedoRecord::commit(l, 1, 1)).unwrap();
        writer.append(|l| RedoRecord::commit(l, 1, 2)).unwrap();
        assert!(wal.synced_lsn() < wal.appended_lsn(), "追加不自动刷盘");

        // 按需唤醒：水位追平追加位。
        lgwr.wake();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while wal.synced_lsn() < wal.appended_lsn() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(wal.synced_lsn(), wal.appended_lsn(), "被唤醒后刷到追加位");
        assert!(lgwr.stats().flushes >= 1);
        assert_eq!(lgwr.stats().failures, 0);

        // 再追加：只有**周期兜底**（不唤醒）也会被刷出去。
        let before = lgwr.stats().flushes;
        writer.append(|l| RedoRecord::commit(l, 1, 3)).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while lgwr.stats().flushes == before && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(wal.synced_lsn(), wal.appended_lsn(), "周期兜底追平");
        lgwr.shutdown();
    }

    #[test]
    fn dbwr_and_lgwr_share_the_real_wal_across_threads() {
        // 真 WAL + 独立 LGWR：DBWR 合并日志请求并唤醒 LGWR，不自行等待 fsync。
        use bicdb_storage::buffer::{BufferKey, BufferPool, CacheConfig, SystemClock};
        use bicdb_storage::dbwr::Dbwr;
        use bicdb_storage::page::{Page, PageType};
        use bicdb_storage::pagefile;
        use bicdb_storage::rowid::Rdba;

        let io = leaked_io();
        // 数据文件（leak 句柄；测试专用）。
        let data_handle = {
            let file = Box::leak(Box::new(
                bicdb_storage::datafile::DataFile::create(
                    io,
                    Path::new("/mem/data.dat"),
                    3,
                    3,
                    [9u8; 8],
                    512,
                )
                .unwrap(),
            ));
            let h = file.handle();
            let mut page = Page::new(PageType::HeapTable, [9u8; 8], 3, 1);
            pagefile::write_page(io, h, 1, &mut page).unwrap();
            h
        };
        let (wal, mut writer) = setup();
        let durable_before = wal.synced_lsn();

        // 追加一条 redo，并把页的 `page_lsn` 设到它上面。
        let rec_lsn = writer.append(|l| RedoRecord::commit(l, 7, 1)).unwrap();
        assert!(wal.synced_lsn() < rec_lsn, "尚未刷盘");

        let lgwr = Arc::new(Lgwr::start(Arc::clone(&wal), Duration::from_secs(60)));
        let wake_lgwr = Arc::downgrade(&lgwr);
        let router = Arc::new(bicdb_storage::wal_router::WorkspaceWalRouter::default());
        router
            .register(
                [9u8; 8],
                Arc::clone(&wal) as Arc<dyn bicdb_storage::buffer::WalGuard>,
            )
            .unwrap();
        router
            .set_flush_request_handler(Arc::new(move |_, _| {
                if let Some(lgwr) = wake_lgwr.upgrade() {
                    lgwr.wake();
                }
            }))
            .unwrap();

        let pool = std::sync::Arc::new(
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
                router,
                SystemClock,
                CacheConfig::for_capacity(8),
            )
            .unwrap(),
        );
        let key = BufferKey::new([9u8; 8], Rdba::from_parts(3, 1).unwrap());
        {
            let mut g = pool.pin(key).unwrap();
            g.as_bytes_mut()[4096] = 0xAB;
            let mut header = g.header().unwrap();
            header.page_lsn = rec_lsn;
            g.write_header(&header);
            g.mark_dirty(rec_lsn);
        }
        assert_eq!(pool.dirty_len([9u8; 8]), 1);

        let dbwr = Dbwr::start(std::sync::Arc::clone(&pool), Duration::from_secs(60));
        dbwr.wake();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while pool.dirty_len([9u8; 8]) > 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(pool.dirty_len([9u8; 8]), 0, "后台写回完成");
        assert!(
            wal.synced_lsn() >= rec_lsn,
            "写页前真 WAL 已刷到 page_lsn（跨线程的规则 2）"
        );
        assert!(wal.synced_lsn() > durable_before);
        dbwr.shutdown();
        Arc::try_unwrap(lgwr)
            .expect("测试回调仅保留弱引用")
            .shutdown();

        let page = pagefile::read_page_verified(io, data_handle, 1).unwrap();
        assert_eq!(page.as_bytes()[4096], 0xAB, "页已落盘");
    }

    #[test]
    fn prestart_hook_runs_once_inside_the_lgwr_thread_before_any_flush() {
        // NUMA 阶段 B 的"LGWR 创建时入组"注入点（详设 §5）：钩子在
        // **LGWR 线程体内、任何刷盘之前**跑一次——绑定的是 LGWR 自己。
        fn tid_of(path: &str) -> u32 {
            std::fs::read_link(path)
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .parse()
                .unwrap()
        }
        let (wal, _writer) = setup();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls2 = Arc::clone(&calls);
        let (tx, rx) = std::sync::mpsc::channel();
        let lgwr = Lgwr::start_scoped(Arc::clone(&wal), Duration::from_millis(20), move || {
            calls2.fetch_add(1, Ordering::SeqCst);
            tx.send(tid_of("/proc/thread-self")).expect("回传 tid");
        });
        let hook_tid = rx.recv_timeout(Duration::from_secs(2)).expect("钩子跑过");
        assert_ne!(
            hook_tid,
            tid_of("/proc/thread-self"),
            "钩子必须在 LGWR 线程里（不是调用方线程）"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1, "钩子恰好一次");
        assert_eq!(lgwr.stats().failures, 0);
        lgwr.shutdown();
    }
}
