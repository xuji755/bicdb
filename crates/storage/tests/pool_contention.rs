//! **缓冲池真实路径的争用基准**（`#[ignore]`，手动跑）：
//!
//! ```text
//! cargo test -p bicdb-storage --release --test pool_contention -- --ignored --nocapture
//! ```
//!
//! 量三条路径（都用**真 `BufferPool`**，不是合成锁）：
//!
//! - **不同块 `pin`**（各自独占）：结构闩锁（哈希查找 + 记账）是唯一共享点——
//!   它是当前 `pin` 的上限，也是 O3 桶分片的判据；
//! - **同块 `pin`**（独占内容锁）：O2 的内容锁串行度（写路径的形态）；
//! - **同块 `pin_shared`**（共享内容锁）：O2 的目标——并发读不串行。
//!
//! 参照对比：合成闩锁基准（`bicdb-common/tests/lock_contention.rs`，证据包
//! `doc/evidence/latch-ab-20261005/`）已给出"自旋 vs 排队"的交叉点；本基准回答
//! **真实临界区长度下，谁是真瓶颈**。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use bicdb_common::seq::Lsn;
use bicdb_storage::buffer::{BufferKey, BufferPool, CacheConfig, SystemClock, WalGuard};
use bicdb_storage::page::{Page, PageType};
use bicdb_storage::pagefile;
use bicdb_storage::rowid::Rdba;
use bicdb_workspace::io::{FileIo, MemFileIo, OpenOptions};
use std::path::Path;

const WS: [u8; 8] = [9u8; 8];
const FILE: &str = "/mem/bench.dat";
const BLOCKS: u32 = 256;

struct NoWal;
impl WalGuard for NoWal {
    fn durable_lsn(&self) -> Lsn {
        Lsn::from_raw(u64::MAX >> 16).expect("域内")
    }
    fn ensure_durable(&self, _t: Lsn) -> std::io::Result<()> {
        Ok(())
    }
}

/// 夹具：文件 7 的 `BLOCKS` 页 + 驻留全部页的池（`partitions` 个分区）。
fn fixture(partitions: usize, threads: usize) -> &'static BufferPool<'static> {
    let io: &'static MemFileIo = Box::leak(Box::new(MemFileIo::new()));
    io.add_dir("/mem");
    let h = io
        .open(
            Path::new(FILE),
            OpenOptions::new().read(true).write(true).create_new(true),
        )
        .expect("建文件");
    io.set_len(h, u64::from(BLOCKS) * bicdb_storage::page::PAGE_SIZE as u64)
        .expect("撑长");
    for block in 0..BLOCKS {
        let mut page = Page::new(PageType::HeapTable, WS, 7, block);
        pagefile::write_page(io, h, block, &mut page).expect("写页");
    }
    let capacity = (BLOCKS as usize).max(threads * 4);
    let pool: &'static BufferPool<'static> = Box::leak(Box::new(
        BufferPool::with_partitions(
            io,
            partitions,
            capacity,
            move |ws, r| {
                if *ws == WS && r.file_id() == 7 {
                    Some((h, r.block_id()))
                } else {
                    None
                }
            },
            NoWal,
            SystemClock,
            CacheConfig::for_capacity(capacity),
        )
        .expect("建池"),
    ));
    // 全部驻留（基准只量内存路径）。
    for block in 0..BLOCKS {
        let key = BufferKey::new(WS, Rdba::from_parts(7, block).expect("域内"));
        let _ = pool.pin(key).expect("驻留");
    }
    pool
}

fn bench<F>(threads: usize, secs: f64, f: &F) -> f64
where
    F: Fn(usize, &AtomicBool) -> u64 + Sync,
{
    let ops = AtomicU64::new(0);
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        for t in 0..threads {
            let (ops, stop, f) = (&ops, &stop, f);
            s.spawn(move || {
                let n = f(t, stop);
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
fn pool_paths_contention() {
    let secs = 1.0;
    println!("| 路径 | 线程 | 吞吐（万次/秒） | `db_cache` 判读（§2.7 口径） |");
    println!("| --- | --- | --- | --- |");
    // E：**写路径**（各自独占块）：pin → 改页 → `mark_dirty` → drop
    //    ——O3 后写侧仍经结构闩锁（写列表），这是它的当前上限。
    for threads in [1usize, 2, 4, 8, 16, 32] {
        let poole = fixture(1, threads);
        // 先预脏（写列表插入是**首次**才做；稳态是"已在写列表"的快路径
        // ——真实负载里两者交替，这里取稳态口径并把首次插入摊薄到测窗外）。
        for t in 0..threads {
            let key = BufferKey::new(
                WS,
                Rdba::from_parts(7, (t % BLOCKS as usize) as u32).expect("域内"),
            );
            let mut g = poole.pin(key).expect("驻留");
            g.mark_dirty(Lsn::from_raw(1).expect("域内"));
        }
        let e = bench(threads, secs, &move |t, stop| {
            let key = BufferKey::new(
                WS,
                Rdba::from_parts(7, (t % BLOCKS as usize) as u32).expect("域内"),
            );
            let mut n = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let mut g = poole.pin(key).expect("命中");
                g.bump_mod_seq();
                g.mark_dirty(Lsn::from_raw(2).expect("域内"));
                std::hint::black_box(&g);
                drop(g);
                n += 1;
            }
            n
        });
        println!(
            "| E `mark_dirty`（写路径，结构闩锁） | {threads} | {:.1} | — |",
            e / 1e4
        );
    }
    for threads in [1usize, 2, 4, 8, 16, 32] {
        let pool = fixture(1, threads);
        // A：不同块 pin（各自独占；共享点 = 结构闩锁）。
        let a = bench(threads, secs, &|t, stop| {
            let key = BufferKey::new(
                WS,
                Rdba::from_parts(7, (t % BLOCKS as usize) as u32).expect("域内"),
            );
            let mut n = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let g = pool.pin(key).expect("命中");
                std::hint::black_box(&g);
                drop(g);
                n += 1;
            }
            n
        });
        // B：同块 pin（独占内容锁串行）。
        let pool2 = fixture(1, threads);
        let key_same = BufferKey::new(WS, Rdba::from_parts(7, 1).expect("域内"));
        let b = bench(threads, secs, &move |_, stop| {
            let mut n = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let g = pool2.pin(key_same).expect("命中");
                std::hint::black_box(&g);
                drop(g);
                n += 1;
            }
            n
        });
        // C：同块 pin_shared（共享内容锁——O2 的目标）。
        let pool3 = fixture(1, threads);
        let c = bench(threads, secs, &move |_, stop| {
            let mut n = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let g = pool3.pin_shared(key_same).expect("命中");
                std::hint::black_box(&g);
                drop(g);
                n += 1;
            }
            n
        });
        let lc = pool.latch_stats().contention();
        println!(
            "| A 不同块 `pin`（结构闩锁） | {threads} | {:.1} | misses/gets={:.2} sleeps/gets={:.3} spin成功率={:.2} |",
            a / 1e4,
            lc.intensity,
            lc.sleep_ratio,
            lc.spin_success
        );
        println!(
            "| B 同块 `pin`（内容锁独占） | {threads} | {:.1} | — |",
            b / 1e4
        );
        println!(
            "| C 同块 `pin_shared`（内容锁共享） | {threads} | {:.1} | — |",
            c / 1e4
        );
    }
    // D：分区数的影响（不同块 pin，4 分区）。
    for threads in [8usize, 32] {
        let pool = fixture(4, threads);
        let d = bench(threads, secs, &move |t, stop| {
            let key = BufferKey::new(
                WS,
                Rdba::from_parts(7, (t % BLOCKS as usize) as u32).expect("域内"),
            );
            let mut n = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let g = pool.pin(key).expect("命中");
                std::hint::black_box(&g);
                drop(g);
                n += 1;
            }
            n
        });
        println!(
            "| D 不同块 `pin`（4 分区同哈希） | {threads} | {:.1} | — |",
            d / 1e4
        );
    }
}
