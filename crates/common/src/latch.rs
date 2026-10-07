//! **具名闩锁**：先自旋、后睡眠，带 `V$LATCH` 口径的统计
//! （证据包 `doc/evidence/latch-mech-20261005/`）。
//!
//! ```text
//! lock() = gets++
//!   ① try_lock 成功            ⇒ immediate++        （无竞争路径，零系统调用）
//!   ② 自旋 spin 次（try_lock） ⇒ spin_gets++        （Oracle _LATCH_SPIN_COUNT 形态）
//!   ③ 仍失败 ⇒ 睡眠（std futex）⇒ sleeps++、wait_ns += 实际等待
//! ```
//!
//! # 设计依据（证据行见证据包）
//!
//! - Oracle：`_LATCH_SPIN_COUNT`（自旋次数）/`_LATCH_WAIT_POSTING`（自旋失败后
//!   睡眠）——**两段式**；`latch free` 的三个诊断字段是"地址/名字/睡眠次数"；
//!   V$LATCH 给每类闩锁 gets/misses/sleeps ——**闩锁必须具名、可统计**；
//! - PG：`LWLockAttemptLock` 的无竞争路径 = **单次原子 CAS，wait-free**；
//!   竞争才入等待队列——本实现的 ①② 对应其快速路径，③ 交给 std 的 futex；
//! - **自旋是参数化权衡**：自旋过高在无 CAS 平台/高争用下烧 CPU（Note 433631.1）
//!   ——默认取 [`DEFAULT_SPIN`]（**8**；Oracle `_LATCH_SPIN_COUNT` 口径，
//!   实测表见下），`with_spin(0)` 即纯睡眠；先用统计量化，再调参数。
//!   （本节此前写"默认 40"，与代码里的 `DEFAULT_SPIN = 8` 不符——文档漂移。）
//! - **退避形状照 PG**（《Oracle Latch 与 PostgreSQL LWLock 算法对照》§3.1：
//!   `perform_spin_delay` 的"每段自旋次数指数增长"）：自旋预算按 1,2,4,8,… 分段，
//!   **段间 `yield_now()`**——把大量短暂争用吸收在用户态，同时降低 N 个自旋者对
//!   同一 cache line 的连续 RMW 频率。PG 的 **TAS_SPIN（只读轮询）**在 std
//!   `Mutex` 上不可达（没有 unsafe 就观察不到状态字）——以"降频"近似它的一半意图。
//! - **判读口径照 `V$LATCH`**（同文档 §2.7/§5.1）：**`misses()/gets` = 争用强度**
//!   （首次探测失败占比；自旋够用的健康形态是"前者高、后者低"）；
//!   **`sleeps/gets` = 是否真打进内核**。两个比率就是"要不要加深分片（O3）"的度量。
//!   **`sleep_gets ≡ sleeps`**（本实现的睡眠只在**拿到**后才返回——std `Mutex`
//!   不暴露伪唤醒），故不另设计数。
//!
//! # 与 `Mutex` 的关系
//!
//! **不重造睡眠机制**：睡眠路径完全走 `std::sync::Mutex`（futex，成熟且无
//! unsafe）；本类型只加**自旋**与**统计**两层。闩锁语义下不采用"中毒"
//! （panic 后的数据交给下一次使用者去发现，而不是级联恐慌）——`into_inner`
//! 语义，`PoisonError` 直接取回。

use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;

/// `wait_ns` 的**采样间隔**（每 N 次睡眠实测一次、×N 折算成估计值）：把
/// "两次时钟读取 + 一次 RMW"从每次争用降为 1/N 次（实测隔离数据见
/// `doc/evidence/latch-ab-20261005/`）。
pub const WAIT_NS_SAMPLE: u32 = 64;

/// 默认真自旋预算（见模块文档的权衡说明）。
///
/// **实测定的 8**（128 核 Neoverse-N1 单 socket，`tests/lock_contention.rs`
/// 的 A/B，见 `doc/evidence/latch-ab-20261005/`）：
///
/// | 预算 | 2 线程 | 8 | 32 | 128（万次/秒） |
/// | --- | --- | --- | --- | --- |
/// | 0（纯睡眠） | 876 | 453 | 426 | 408 |
/// | **8** | **2248** | **439** | **453** | **433** |
/// | 40 | 2611 | 376 | 235 | 276 |
///
/// 预算 8：低争用拿住"大预算"约 86% 的收益，高争用**不再掉崖**（40 号在
/// 32/128 线程比纯睡眠还慢——每次探测都是 RMW，N 个自旋者把持有者的行搅成负和）。
pub const DEFAULT_SPIN: u32 = 8;

/// 单段自旋次数上限（PG `MAX_SPINS_PER_DELAY` 的同位物——段长指数增长到这里封顶）。
pub const MAX_SPINS_PER_DELAY: u32 = 16;

/// 闩锁统计快照（诊断口径：`V$LATCH` 的 gets/immediate/spin/sleeps）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LatchStats {
    /// 具名（闩锁类；`V$LATCHNAME` / PG tranche 的对应物）。
    pub name: &'static str,
    /// 获取尝试总数。
    pub gets: u64,
    /// **无竞争直接成功**（未自旋、未睡眠）。
    pub immediate: u64,
    /// 自旋后成功。
    pub spin_gets: u64,
    /// **睡眠次数**（"latch free" 的睡眠尝试次数）。
    pub sleeps: u64,
    /// 睡眠等待累计纳秒（**采样估计**：每 64 次睡眠实测一次 ×64 折算——
    /// 诊断用途，"睡眠是否昂贵"看它与 `sleeps` 的比值即可）。
    pub wait_ns: u64,
}

impl LatchStats {
    /// **`misses`**（`V$LATCH` 口径：willing-to-wait 的**首次探测失败**次数）。
    ///
    /// 本实现里 `try_lock` 不计数 ⇒ `misses ≡ gets − immediate`——显式化这个
    /// 导出量，免得到处手算（判读见 [`LatchStats::contention`]）。
    #[must_use]
    pub fn misses(&self) -> u64 {
        self.gets.saturating_sub(self.immediate)
    }

    /// **判读三比率**（文档《Oracle vs PG Latch/LWLock》§2.7/§5.1 的口径）：
    /// `（争用强度 = misses/gets，内核路径占比 = sleeps/gets，自旋成功率 = spin_gets/misses）`。
    /// "前者高、后者低" = 自旋够用（健康）；两者都高 = 临界区过长或闩锁过少
    /// ⇒ 加深分片（O3）的信号。
    #[must_use]
    pub fn contention(&self) -> LatchContention {
        let gets = self.gets.max(1);
        let misses = self.misses();
        LatchContention {
            intensity: misses as f64 / gets as f64,
            sleep_ratio: self.sleeps as f64 / gets as f64,
            spin_success: if misses == 0 {
                0.0
            } else {
                self.spin_gets as f64 / misses as f64
            },
        }
    }
}

/// 闩锁争用的三个判读比率（见 [`LatchStats::contention`]）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LatchContention {
    /// `misses / gets`：争用强度。
    pub intensity: f64,
    /// `sleeps / gets`：真打进内核的比例。
    pub sleep_ratio: f64,
    /// `spin_gets / misses`：自旋阶段的成功率。
    pub spin_success: f64,
}

/// **计数器分片数**（2 的幂；线程按首次使用顺序轮转选片，读取时求和）。
///
/// 为什么分片（实测驱动，证据包 `doc/evidence/latch-ab-20261005/`）：一次 `lock`
/// 原先要写**两个全局共享计数器**（gets/immediate）——高核数上每次 RMW 都是一次
/// cache line 转移，合成基准里"统计"一项单独吃掉 **~38%** 吞吐，真实路径基准里
/// 又与"负扩展"直接相关。分片后每个线程只写**自己那片**的行（同片争用者降
/// ~N×），读取时求和——**仍然精确**（不是采样）。
pub const COUNTER_SHARDS: usize = 8;

/// 一片计数器（对齐 cache line：片之间不互相踢行）。
#[repr(align(64))]
#[derive(Debug)]
struct CounterShard {
    /// 无竞争直接成功（快路径，**每操作一次 RMW**）。
    immediate: AtomicU64,
    /// 自旋后成功。
    spin_gets: AtomicU64,
    /// 睡眠次数。
    sleeps: AtomicU64,
    /// 睡眠等待累计纳秒。
    wait_ns: AtomicU64,
}

impl CounterShard {
    fn new() -> Self {
        Self {
            immediate: AtomicU64::new(0),
            spin_gets: AtomicU64::new(0),
            sleeps: AtomicU64::new(0),
            wait_ns: AtomicU64::new(0),
        }
    }
}

/// **线程标识 = 本函数栈帧地址**：每个线程的栈地址不同、且线程存续期内恒定。
///
/// 为什么不走 `thread_local!`：动态 TLS 访问要走 `__tls_get_addr`（每操作
/// 十几纳秒、直逼锁本体），实测分片收益被它吃掉；取一个**局部变量地址**是
/// 零成本、零 unsafe（只取地址、从不解引用），分布由各线程栈地址（不同页、
/// ASLR）保证。
#[inline]
fn thread_tag() -> usize {
    let x = 0u8;
    std::ptr::addr_of!(x) as usize
}

/// 线程 → 分片（栈地址右移混入高位——栈 16 字节对齐，低位不够散）。
#[inline]
fn shard_of(n: usize) -> usize {
    (thread_tag() >> 6) % n
}

/// 具名闩锁（先自旋、后睡眠）。
#[derive(Debug)]
pub struct Latch<T> {
    name: &'static str,
    inner: Mutex<T>,
    spin: u32,
    /// 分片计数器（**精确**：读取时求和；写只碰本线程那片）。
    shards: [CounterShard; COUNTER_SHARDS],
}

impl<T> Latch<T> {
    /// 建闩锁（默认自旋次数）。
    #[must_use]
    pub fn new(name: &'static str, value: T) -> Self {
        Self {
            name,
            inner: Mutex::new(value),
            spin: DEFAULT_SPIN,
            shards: std::array::from_fn(|_| CounterShard::new()),
        }
    }

    /// 设定自旋次数（`0` = 纯睡眠；链式，建池时用）。
    #[must_use]
    pub fn with_spin(mut self, spin: u32) -> Self {
        self.spin = spin;
        self
    }

    /// 名字。
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// **获取**：immediate → 自旋 → 睡眠（三段式，统计各自计数）。
    pub fn lock(&self) -> LatchGuard<'_, T> {
        let shard = shard_of(COUNTER_SHARDS);
        if let Ok(guard) = self.inner.try_lock() {
            self.shards[shard].immediate.fetch_add(1, Ordering::Relaxed);
            return LatchGuard { guard };
        }
        // **分段退避**（PG `perform_spin_delay` 形态）：段长 1,2,4,…,16 封顶，
        // 段间 `yield_now()` 让出 CPU——短暂争用被吸收在用户态，且降低同一
        // cache line 上的 RMW 频率（PG 用 TAS_SPIN 只读轮询达成，std Mutex
        // 上不可达，以段间让出近似）。
        let mut budget = self.spin;
        let mut per_delay = 1u32;
        while budget > 0 {
            let mut n = per_delay.min(budget);
            while n > 0 {
                std::hint::spin_loop();
                if let Ok(guard) = self.inner.try_lock() {
                    self.shards[shard].spin_gets.fetch_add(1, Ordering::Relaxed);
                    return LatchGuard { guard };
                }
                n -= 1;
                budget -= 1;
            }
            per_delay = (per_delay * 2).min(MAX_SPINS_PER_DELAY);
            if budget > 0 {
                std::thread::yield_now();
            }
        }
        // **睡眠路径的记账要便宜**（实测隔离，证据包 `latch-ab-20261005/`：
        // 两次 `Instant::now` + 一个 RMW 曾是每次争用的主要额外成本——
        // aarch64 上一次时钟读取 ~20-30ns，逼近锁本体）。`sleeps` 精确
        // （一次 RMW；其返回值顺带给出采样位）；`wait_ns` 改为**采样估计**：
        // 每 `WAIT_NS_SAMPLE` 次睡眠测一次、×N 折算（诊断足够，口径见其文档）。
        let prev = self.shards[shard].sleeps.fetch_add(1, Ordering::Relaxed);
        let t0 = if prev % u64::from(WAIT_NS_SAMPLE) == 0 {
            Some(Instant::now())
        } else {
            None
        };
        // 闩锁语义不采用中毒级联：把数据取回继续用（"下一次使用者发现"）。
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(t0) = t0 {
            let ns = t0.elapsed().as_nanos() as u64;
            self.shards[shard].wait_ns.fetch_add(
                ns.saturating_mul(u64::from(WAIT_NS_SAMPLE)),
                Ordering::Relaxed,
            );
        }
        LatchGuard { guard }
    }

    /// **不计数**的尝试（诊断/测试用；不改变统计）。
    #[must_use]
    pub fn try_lock(&self) -> Option<LatchGuard<'_, T>> {
        self.inner.try_lock().ok().map(|guard| LatchGuard { guard })
    }

    /// 统计快照（**逐片求和——精确**；`gets` 是三个结局之和，不再单设计数器）。
    #[must_use]
    pub fn stats(&self) -> LatchStats {
        let (mut immediate, mut spin_gets, mut sleeps, mut wait_ns) = (0, 0, 0, 0);
        for s in &self.shards {
            immediate += s.immediate.load(Ordering::Relaxed);
            spin_gets += s.spin_gets.load(Ordering::Relaxed);
            sleeps += s.sleeps.load(Ordering::Relaxed);
            wait_ns += s.wait_ns.load(Ordering::Relaxed);
        }
        LatchStats {
            name: self.name,
            gets: immediate + spin_gets + sleeps,
            immediate,
            spin_gets,
            sleeps,
            wait_ns,
        }
    }

    /// 取回内部值（销毁闩锁）。
    pub fn into_inner(self) -> T {
        self.inner.into_inner().unwrap_or_else(|e| e.into_inner())
    }

    /// 可变借用（构建期/独占期用）。
    pub fn get_mut(&mut self) -> &mut T {
        self.inner.get_mut().unwrap_or_else(|e| e.into_inner())
    }
}

/// 闩锁卫兵（`Deref` 到被保护值；`Drop` = 释放）。
#[derive(Debug)]
pub struct LatchGuard<'a, T> {
    guard: MutexGuard<'a, T>,
}

impl<T> Deref for LatchGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> DerefMut for LatchGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn immediate_path_counts_and_derefs() {
        let latch = Latch::new("test", 7u32);
        {
            let mut g = latch.lock();
            assert_eq!(*g, 7);
            *g = 9;
        }
        assert_eq!(latch.stats().immediate, 1);
        assert_eq!(latch.stats().sleeps, 0);
        assert_eq!(latch.stats().name, "test");
        assert_eq!(latch.into_inner(), 9);
    }

    #[test]
    fn contended_path_sleeps_or_spins_then_succeeds() {
        let latch = Arc::new(Latch::new("contended", 0u32).with_spin(2));
        let held = latch.lock();
        let l2 = Arc::clone(&latch);
        let t = std::thread::spawn(move || {
            let mut g = l2.lock();
            *g += 1;
        });
        // 持锁到子线程进入等待（给它时间走完自旋）。
        std::thread::sleep(std::time::Duration::from_millis(50));
        drop(held);
        t.join().unwrap();
        let s = latch.stats();
        assert_eq!(s.gets, 2);
        assert_eq!(s.immediate, 1, "第一次无竞争");
        assert_eq!(s.spin_gets + s.sleeps, 1, "第二次走了自旋或睡眠");
        if s.sleeps == 1 {
            assert!(s.wait_ns > 0, "睡眠记了等待时长");
        }
        assert_eq!(*latch.lock(), 1);
    }

    #[test]
    fn zero_spin_never_counts_spin_gets() {
        let latch = Arc::new(Latch::new("nospin", ()).with_spin(0));
        let held = latch.lock();
        let l2 = Arc::clone(&latch);
        let t = std::thread::spawn(move || {
            let _ = l2.lock();
        });
        std::thread::sleep(std::time::Duration::from_millis(20));
        drop(held);
        t.join().unwrap();
        let s = latch.stats();
        assert_eq!(s.spin_gets, 0);
        assert_eq!(s.sleeps, 1);
    }

    #[test]
    fn try_lock_does_not_change_stats() {
        let latch = Latch::new("try", 1u8);
        {
            let _g = latch.try_lock().expect("空闲可试锁");
        }
        assert!(latch.try_lock().is_some());
        let s = latch.stats();
        assert_eq!((s.gets, s.immediate, s.spin_gets, s.sleeps), (0, 0, 0, 0));
    }

    #[test]
    fn misses_and_contention_ratios_follow_vlatch() {
        let latch = Latch::new("ratios", 0u32);
        {
            let _g = latch.lock(); // immediate
        }
        let s = latch.stats();
        assert_eq!(s.misses(), 0, "无竞争 ⇒ 零 misses");
        let c = s.contention();
        assert_eq!(c.intensity, 0.0);
        assert_eq!(c.sleep_ratio, 0.0);
        assert_eq!(c.spin_success, 0.0);
    }

    #[test]
    fn staged_backoff_still_finds_the_lock_within_the_spin_budget() {
        // 退避是"形状"不是"语义"：预算内拿到就计 spin_gets，不睡眠。
        let latch = Arc::new(Latch::new("staged", 0u32).with_spin(32));
        let held = latch.lock();
        let l2 = Arc::clone(&latch);
        let t = std::thread::spawn(move || {
            let mut g = l2.lock();
            *g += 1;
        });
        // 自旋预算内释放：子线程应当走 spin_gets（不是 sleeps）。
        std::thread::sleep(std::time::Duration::from_millis(1));
        drop(held);
        t.join().unwrap();
        let s = latch.stats();
        assert_eq!(s.gets, 2);
        assert_eq!(s.immediate, 1);
        assert_eq!(s.spin_gets + s.sleeps, 1, "第二次走自旋或睡眠");
        assert_eq!(s.misses(), 1, "第二次的首次探测失败");
        assert!(s.contention().intensity > 0.0);
    }
}
