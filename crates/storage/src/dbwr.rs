//! **DBWR 线程**（§11.7 写侧五角色的写线程；池侧的"真线程化"）。
//!
//! ```text
//! 前台（Make Free）：容量压力下的内联写回——低延迟路径，不变
//! DBWR 线程（本模块）：**周期**把有脏页的工作区按写列表**从头按序**写回
//!                     + 前台可 `wake()` 提前触发
//! ```
//!
//! 与 WAL 的唯一交点是**规则 2**（页写前 redo 必须耐久）——池在写回时经
//! `WalGuard` 完成，而 `WalGuard` 现在可以是 [`bicdb_wal::group::WalShared`]
//! 的 `Arc`（**可共享**），所以后台线程刷盘是安全的：
//! 同一个 `redo_io` 闩锁 + 同一个 `synced_lsn` 水位，与前台提交共享组提交。
//!
//! **降级纪律**：后台写回失败**不 panic**——记进统计、等下一轮；正确性由
//! 前台路径与恢复承担（脏页仍在写列表里，什么都没丢）。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::buffer::BufferPool;

/// 周期兜底的默认间隔（与 LGWR 的 3 秒同源；DBWR 的"空闲写"档）。
pub const DEFAULT_TICK: Duration = Duration::from_secs(3);

/// DBWR 的运行统计（诊断；失败不静默）。
#[derive(Debug, Default)]
pub struct DbwrStats {
    /// 完成的写回轮次。
    pub passes: u64,
    /// 累计写回的页数。
    pub pages_written: u64,
    /// 失败次数（页仍在写列表；下一轮重试）。
    pub failures: u64,
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

struct StatsInner {
    passes: AtomicU64,
    pages_written: AtomicU64,
    failures: AtomicU64,
    last_error: Mutex<Option<String>>,
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
    pub fn start(pool: Arc<BufferPool<'static>>, tick: Duration) -> Self {
        let signal = Arc::new((Mutex::new(false), Condvar::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(StatsInner {
            passes: AtomicU64::new(0),
            pages_written: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            last_error: Mutex::new(None),
        });
        let (sig, st, stp, pool2) = (
            Arc::clone(&signal),
            Arc::clone(&stats),
            Arc::clone(&stop),
            Arc::clone(&pool),
        );
        let handle = std::thread::Builder::new()
            .name("bicdb-dbwr".into())
            .spawn(move || {
                loop {
                    {
                        let (lock, cv) = &*sig;
                        let mut pending = lock.lock().unwrap_or_else(|e| e.into_inner());
                        if !*pending {
                            let (guard, _) = cv
                                .wait_timeout(pending, tick)
                                .unwrap_or_else(|e| e.into_inner());
                            pending = guard;
                        }
                        *pending = false;
                    }
                    if stp.load(Ordering::SeqCst) {
                        break;
                    }
                    write_back_once(&pool2, &st);
                }
                // 退出前最后一轮：把脏页交干（尽力而为）。
                write_back_once(&pool2, &st);
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

/// 一轮写回：**每个有脏页的工作区**按写列表从头按序写（`flush_workspace`）。
fn write_back_once(pool: &BufferPool<'static>, stats: &StatsInner) {
    let mut wrote = 0u64;
    let mut failed = false;
    for ws in pool.dirty_workspaces() {
        match pool.flush_workspace(ws) {
            Ok(report) => wrote += report.pages_written,
            Err(e) => {
                failed = true;
                *stats.last_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(e.to_string());
            }
        }
    }
    if wrote > 0 {
        stats.pages_written.fetch_add(wrote, Ordering::Relaxed);
    }
    if failed {
        stats.failures.fetch_add(1, Ordering::Relaxed);
    }
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
    fn dbwr_syncs_wal_before_writing_the_page() {
        // **规则 2 的次序**：页写之前必须先 `ensure_durable(page_lsn)`——
        // 后台线程与前台走同一条路径（`perform_write`），次序可观测。
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
                OrderWal {
                    durable: AtomicU64::new(0),
                    log: Arc::clone(&log),
                },
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

        // 周期很长 ⇒ 只有 `wake()` 会驱动；等到写回完成。
        let dbwr = Dbwr::start(Arc::clone(&pool), Duration::from_secs(60));
        dbwr.wake();
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
}
