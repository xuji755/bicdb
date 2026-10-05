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
//!   ——默认 **40**（≈1 µs 量级的 try_lock 轮次），`with_spin(0)` 即纯睡眠；
//!   先用统计量化，再调参数。
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

/// 默认真自旋次数（见模块文档的权衡说明）。
pub const DEFAULT_SPIN: u32 = 40;

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
    /// 睡眠等待累计纳秒。
    pub wait_ns: u64,
}

/// 具名闩锁（先自旋、后睡眠）。
#[derive(Debug)]
pub struct Latch<T> {
    name: &'static str,
    inner: Mutex<T>,
    spin: u32,
    gets: AtomicU64,
    immediate: AtomicU64,
    spin_gets: AtomicU64,
    sleeps: AtomicU64,
    wait_ns: AtomicU64,
}

impl<T> Latch<T> {
    /// 建闩锁（默认自旋次数）。
    #[must_use]
    pub fn new(name: &'static str, value: T) -> Self {
        Self {
            name,
            inner: Mutex::new(value),
            spin: DEFAULT_SPIN,
            gets: AtomicU64::new(0),
            immediate: AtomicU64::new(0),
            spin_gets: AtomicU64::new(0),
            sleeps: AtomicU64::new(0),
            wait_ns: AtomicU64::new(0),
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
        self.gets.fetch_add(1, Ordering::Relaxed);
        if let Ok(guard) = self.inner.try_lock() {
            self.immediate.fetch_add(1, Ordering::Relaxed);
            return LatchGuard { guard };
        }
        for _ in 0..self.spin {
            std::hint::spin_loop();
            if let Ok(guard) = self.inner.try_lock() {
                self.spin_gets.fetch_add(1, Ordering::Relaxed);
                return LatchGuard { guard };
            }
        }
        self.sleeps.fetch_add(1, Ordering::Relaxed);
        let t0 = Instant::now();
        // 闩锁语义不采用中毒级联：把数据取回继续用（"下一次使用者发现"）。
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        self.wait_ns
            .fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
        LatchGuard { guard }
    }

    /// **不计数**的尝试（诊断/测试用；不改变统计）。
    #[must_use]
    pub fn try_lock(&self) -> Option<LatchGuard<'_, T>> {
        self.inner.try_lock().ok().map(|guard| LatchGuard { guard })
    }

    /// 统计快照。
    #[must_use]
    pub fn stats(&self) -> LatchStats {
        LatchStats {
            name: self.name,
            gets: self.gets.load(Ordering::Relaxed),
            immediate: self.immediate.load(Ordering::Relaxed),
            spin_gets: self.spin_gets.load(Ordering::Relaxed),
            sleeps: self.sleeps.load(Ordering::Relaxed),
            wait_ns: self.wait_ns.load(Ordering::Relaxed),
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
}
