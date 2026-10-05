//! **闩锁形态 A/B：Oracle 形态（不排队）vs LWLock 形态（FIFO + handoff）**。
//!
//! 参照文档《Oracle Latch 与 PostgreSQL LWLock 算法对照》§0 的"赌注"：
//! 两种形态的胜负取决于**临界区长度分布**。本用例把两种形态在**同一台机器**上
//! 对拍，度量三条曲线（吞吐 / 每操作延迟 / 放弃自旋的代价）：
//!
//! - `Latch<T>`（`bicdb-common`）：immediate → 分段自旋 → futex 睡眠（Oracle 形态）；
//! - `FifoLock`（本文件）：快路径 CAS，慢路径 **FIFO 排队 + 释放者 handoff**
//!   （LWLock 形态：`Mutex + Condvar + VecDeque` 的 safe-Rust 等价物）。
//!
//! **跑法**（默认忽略，手动跑）：
//! ```text
//! cargo test -p bicdb-common --release --test lock_contention -- --ignored --nocapture
//! ```
//! 机器：Neoverse-N1 **128 核单 socket**（arm64）——高核数是"自旋者挤同一
//! cache line"的最坏场景，也是队列形态最有利的场景。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use bicdb_common::latch::Latch;

// ---------------------------------------------------------------------------
// 对照组：FIFO + handoff（LWLock 形态；safe Rust 等价物）
// ---------------------------------------------------------------------------

/// LWLock 形态的**形状基线**：`std::sync::Mutex`——Linux 上是 futex：
/// **排队 + 唤醒一个**（无惊群；唤醒后由内核完成所有权转移，等价于 handoff 的
/// 效果）+ 内建短暂自适应自旋。对照 PG 的 LWLock（futex 队列 + 释放者点名 +
/// `RELEASE_OK` 握手）取其**形状**：**竞争时排队，而不是大家重抢**。
///
/// 说明：第一版对照曾手写"原子快路径 + Condvar 队列 + handoff"，在 128 线程下
/// 真死锁（`swap` 之后、入队之前被释放者的 `store(false)` 覆盖）——正是文档
/// §3.6 `RELEASE_OK` 握手要消除的竞态；唤醒用 `notify_all` 又制造了文档明确
/// 规避的惊群。**这两点本身就是对照结论的一部分**：LWLock 形态的收益真实，
/// 但把它写对写快有门槛；`std::Mutex` 已替我们付了这笔实现成本。
struct QueueFirst {
    inner: std::sync::Mutex<u64>,
}

impl QueueFirst {
    fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(0),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, u64> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

// ---------------------------------------------------------------------------
// 度量
// ---------------------------------------------------------------------------

/// 每条曲线：`(线程数, 临界区忙等时长) → 吞吐（次/秒）`。
fn bench_oracle_latch(threads: usize, busy: Duration, secs: f64) -> f64 {
    let latch = Latch::new("bench", 0u64);
    let ops = AtomicU64::new(0);
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        for _ in 0..threads {
            let ops = &ops;
            let stop = &stop;
            let latch = &latch;
            s.spawn(move || {
                let mut n = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    {
                        let mut g = latch.lock();
                        *g += 1;
                        if !busy.is_zero() {
                            std::thread::sleep(busy); // 模拟"临界区内做事"（长临界区）
                        }
                    }
                    n += 1;
                }
                ops.fetch_add(n, Ordering::Relaxed);
            });
        }
        std::thread::sleep(Duration::from_secs_f64(secs));
        stop.store(true, Ordering::Relaxed);
    });
    ops.load(Ordering::Relaxed) as f64 / secs
}

fn bench_queue_first_long(threads: usize, busy: Duration, secs: f64) -> f64 {
    let lock = QueueFirst::new();
    let ops = AtomicU64::new(0);
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        for _ in 0..threads {
            let ops = &ops;
            let stop = &stop;
            let lock = &lock;
            s.spawn(move || {
                let mut n = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    {
                        let mut g = lock.lock();
                        *g += 1;
                        if !busy.is_zero() {
                            std::thread::sleep(busy);
                        }
                    }
                    n += 1;
                }
                ops.fetch_add(n, Ordering::Relaxed);
            });
        }
        std::thread::sleep(Duration::from_secs_f64(secs));
        stop.store(true, Ordering::Relaxed);
    });
    ops.load(Ordering::Relaxed) as f64 / secs
}

fn bench_oracle_latch_spin_short(threads: usize, secs: f64) -> f64 {
    // 极短临界区（一次自增）：自旋能吃满的形态。
    let latch = Latch::new("bench", 0u64);
    let ops = AtomicU64::new(0);
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        for _ in 0..threads {
            let (ops, stop, latch) = (&ops, &stop, &latch);
            s.spawn(move || {
                let mut n = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    *latch.lock() += 1;
                    n += 1;
                }
                ops.fetch_add(n, Ordering::Relaxed);
            });
        }
        std::thread::sleep(Duration::from_secs_f64(secs));
        stop.store(true, Ordering::Relaxed);
    });
    ops.load(Ordering::Relaxed) as f64 / secs
}

fn bench_queue_first_short(threads: usize, secs: f64) -> f64 {
    let lock = QueueFirst::new();
    let ops = AtomicU64::new(0);
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        for _ in 0..threads {
            let (ops, stop, lock) = (&ops, &stop, &lock);
            s.spawn(move || {
                let mut n = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    *lock.lock() += 1;
                    n += 1;
                }
                ops.fetch_add(n, Ordering::Relaxed);
            });
        }
        std::thread::sleep(Duration::from_secs_f64(secs));
        stop.store(true, Ordering::Relaxed);
    });
    ops.load(Ordering::Relaxed) as f64 / secs
}

/// 自旋预算扫描（模块文档"先用统计量化，再调参数"的落点）。
fn bench_spin_sweep(threads: usize, spin: u32, secs: f64) -> f64 {
    let latch = Latch::new("sweep", 0u64).with_spin(spin);
    let ops = AtomicU64::new(0);
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        for _ in 0..threads {
            let (ops, stop, latch) = (&ops, &stop, &latch);
            s.spawn(move || {
                let mut n = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    *latch.lock() += 1;
                    n += 1;
                }
                ops.fetch_add(n, Ordering::Relaxed);
            });
        }
        std::thread::sleep(Duration::from_secs_f64(secs));
        stop.store(true, Ordering::Relaxed);
    });
    ops.load(Ordering::Relaxed) as f64 / secs
}

/// **对照 3：计数器成本隔离**——裸 `Mutex` vs "Mutex + 两个共享计数器 RMW"
/// （模拟 `Latch` 的快路径记账）vs 完整 `Latch`。回答"慢的到底是锁还是统计"。
struct Counted {
    inner: std::sync::Mutex<u64>,
    gets: AtomicU64,
    immediate: AtomicU64,
}

fn bench_counted(threads: usize, secs: f64) -> f64 {
    let l = Counted {
        inner: std::sync::Mutex::new(0),
        gets: AtomicU64::new(0),
        immediate: AtomicU64::new(0),
    };
    let ops = AtomicU64::new(0);
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        for _ in 0..threads {
            let (ops, stop, l) = (&ops, &stop, &l);
            s.spawn(move || {
                let mut n = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    l.gets.fetch_add(1, Ordering::Relaxed);
                    if let Ok(mut g) = l.inner.try_lock() {
                        l.immediate.fetch_add(1, Ordering::Relaxed);
                        *g += 1;
                    } else {
                        let mut g = l.inner.lock().unwrap_or_else(|e| e.into_inner());
                        *g += 1;
                    }
                    n += 1;
                }
                ops.fetch_add(n, Ordering::Relaxed);
            });
        }
        std::thread::sleep(Duration::from_secs_f64(secs));
        stop.store(true, Ordering::Relaxed);
    });
    ops.load(Ordering::Relaxed) as f64 / secs
}

#[test]
#[ignore = "本地基准（跑法见模块文档）；不进 CI"]
fn isolate_counter_cost() {
    let secs = 1.0;
    println!("| 变体 | 线程 | 吞吐（万次/秒） |");
    println!("| --- | --- | --- |");
    for threads in [2usize, 8, 32, 128] {
        let mutex = bench_queue_first_short(threads, secs);
        let counted = bench_counted(threads, secs);
        println!("| 裸 Mutex（队列形态） | {threads} | {:.1} |", mutex / 1e4);
        println!("| Mutex + 2 计数器 | {threads} | {:.1} |", counted / 1e4);
        for spin in [0u32, 4, 8, 16, 40] {
            let t = bench_spin_sweep(threads, spin, secs);
            println!("| `Latch` spin={spin} | {threads} | {:.1} |", t / 1e4);
        }
    }
}

#[test]
#[ignore = "本地基准（跑法见模块文档）；不进 CI"]
fn ab_oracle_vs_fifo() {
    let secs = 1.0;
    println!("| 形态 | 线程 | 临界区 | 吞吐（万次/秒） |");
    println!("| --- | --- | --- | --- |");
    for threads in [2usize, 8, 32, 128] {
        let a = bench_oracle_latch_spin_short(threads, secs);
        let f = bench_queue_first_short(threads, secs);
        println!(
            "| 自旋形态（`Latch`，spin=40） | {threads} | ~0（纯自增） | {:.1} |",
            a / 1e4
        );
        println!(
            "| 队列形态（std Mutex，futex 队列） | {threads} | ~0（纯自增） | {:.1} |",
            f / 1e4
        );
    }
    for spin in [0u32, 40, 400, 4000] {
        let t = bench_spin_sweep(32, spin, secs);
        println!(
            "| `Latch` spin={spin} | 32 | ~0（纯自增） | {:.1} |",
            t / 1e4
        );
    }
    for threads in [2usize, 8, 32] {
        let busy = Duration::from_micros(20); // 长临界区（I/O 级）
        let a = bench_oracle_latch(threads, busy, secs);
        let f = bench_queue_first_long(threads, busy, secs);
        println!(
            "| 自旋形态（`Latch`，spin=40） | {threads} | 20 µs | {:.1} |",
            a / 1e4
        );
        println!(
            "| 队列形态（std Mutex，futex 队列） | {threads} | 20 µs | {:.1} |",
            f / 1e4
        );
    }
}

/// **隔离 3：包装形态本身的成本**——裸 `Mutex` vs 同形包装（只多一层函数）
/// vs 包装 + **1 个私有分片计数器**（模拟分片后的快路径）。
struct Wrap {
    inner: std::sync::Mutex<u64>,
}
impl Wrap {
    fn lock(&self) -> std::sync::MutexGuard<'_, u64> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[repr(align(64))]
struct Shard {
    c: AtomicU64,
}

struct Wrap1 {
    inner: std::sync::Mutex<u64>,
    shards: [Shard; 8],
}
impl Wrap1 {
    fn lock(&self) -> std::sync::MutexGuard<'_, u64> {
        let x = 0u8;
        let tag = std::ptr::addr_of!(x) as usize;
        let i = (tag >> 6) % 8;
        self.shards[i].c.fetch_add(1, Ordering::Relaxed);
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn bench_wrap<F>(threads: usize, secs: f64, f: &F) -> f64
where
    F: Fn() -> u64 + Sync,
{
    let ops = AtomicU64::new(0);
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        for _ in 0..threads {
            let (ops, stop, f) = (&ops, &stop, f);
            s.spawn(move || {
                let mut n = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    n += f();
                }
                ops.fetch_add(n, Ordering::Relaxed);
            });
        }
        std::thread::sleep(Duration::from_secs_f64(secs));
        stop.store(true, Ordering::Relaxed);
    });
    ops.load(Ordering::Relaxed) as f64 / secs
}

#[test]
#[ignore = "本地基准（跑法见模块文档）；不进 CI"]
fn isolate_wrapper_cost() {
    let secs = 1.0;
    for threads in [2usize, 8, 32, 128] {
        let m = std::sync::Mutex::new(0u64);
        let bare = bench_wrap(threads, secs, &|| {
            let mut g = m.lock().unwrap_or_else(|e| e.into_inner());
            *g += 1;
            1
        });
        let w = Wrap {
            inner: std::sync::Mutex::new(0),
        };
        let wrap = bench_wrap(threads, secs, &|| {
            let mut g = w.lock();
            *g += 1;
            1
        });
        let w1 = Wrap1 {
            inner: std::sync::Mutex::new(0),
            shards: std::array::from_fn(|_| Shard {
                c: AtomicU64::new(0),
            }),
        };
        let wrap1 = bench_wrap(threads, secs, &|| {
            let mut g = w1.lock();
            *g += 1;
            1
        });
        println!("| 裸 Mutex | {threads} | {:.1} |", bare / 1e4);
        println!("| 同形包装（多一层函数） | {threads} | {:.1} |", wrap / 1e4);
        println!(
            "| 包装 + 1 个私有分片计数器 | {threads} | {:.1} |",
            wrap1 / 1e4
        );
    }
}
