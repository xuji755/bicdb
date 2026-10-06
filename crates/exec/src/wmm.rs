//! **工作内存管理器**（WMM；设计 §4.2 / §4.2.1）——实例级**共享池** + 分配算法。
//!
//! ```text
//! WorkMemoryPool（实例一个，Arc 共享；算子无参数、只有申报）
//!   申报 AreaClaim { ideal, one_pass, benefit }   ← 未知输入时先 (0,0,0) 起步
//!   ① 保底  grant = min(one_pass, 公平份额 = target / 活动区数)
//!   ② 加码  剩余按 收益密度 = benefit / max(1, ideal − one_pass) 降序补到 ideal
//!   ③ 上限  grant ≤ target / 20（5% 单区——内部策略，不是参数）
//!   ④ 缩容  任何申报/释放**全量重算** ⇒ 确定、可复现；算子每检查点重读 grant
//! ```
//!
//! **一条纪律**：本模块只管"给多少"；**怎么用/怎么溢出不在这里**
//! （各算子的溢出形态见 `spill` 与设计 §4.2.1 ③）。
//! `WorkArea` 句柄持池的 `Arc`——**Drop 即注销**（错误路径也不泄漏池配额）。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::context::WorkAreaOutcome;

/// 单内存区上限 = 池目标的 1/20（Oracle 5% 规则，KB `3257077`；
/// **内部策略、非配置参数**——Oracle 同样不暴露它）。
const SINGLE_AREA_CAP_DIVISOR: u64 = 20;

/// **一份内存区申报**（设计 §4.2.1 ①；`(0,0,0)` = 未申报 ⇒ 按公平份额起步）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AreaClaim {
    /// 全内存完成所需字节（Oracle `ideal`/`optimal`）。
    pub ideal: u64,
    /// 一趟外存所需字节（Oracle `one-pass`）。
    pub one_pass: u64,
    /// **收益**（字节）：拿到 `ideal` 相对 `one_pass` 省下的 temp 读写流量——
    /// 量纲统一为"每字节内存省下的外存字节"（Sort ≈ 输入 ×2、Hash 族 ≈ 输入）。
    pub benefit: u64,
}

/// 池的**累计监测计数**（Oracle `V$PGASTAT`/`V$SYSSTAT` 口径，设计 §4.2 监测四件套）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PoolStats {
    /// `optimal` 次数（全内存完成）。
    pub optimal: u64,
    /// `one-pass` 次数。
    pub one_pass: u64,
    /// `multi-pass` 次数（要避免、要告警）。
    pub multi_pass: u64,
    /// 溢出写出的额外字节（temp 写）。
    pub extra_bytes_written: u64,
    /// 溢出读回的额外字节（temp 读）。
    pub extra_bytes_read: u64,
}

/// 一个内存区的对外快照（诊断 / 用例断言）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AreaInfo {
    /// 登记号（分配序，也是同密度时的排序键）。
    pub id: u64,
    /// 算子名（诊断）。
    pub name: &'static str,
    /// 当前申报。
    pub claim: AreaClaim,
    /// 当前配额（grant）。
    pub granted: u64,
}

#[derive(Debug)]
struct AreaEntry {
    id: u64,
    name: &'static str,
    claim: AreaClaim,
    granted: Arc<AtomicU64>,
}

#[derive(Debug, Default)]
struct PoolInner {
    next_id: u64,
    areas: Vec<AreaEntry>,
}

/// **实例级共享池**（`work_memory_target`；所有租户/会话/算子内存区共用）。
///
/// 用 `Arc<WorkMemoryPool>` 共享；[`WorkMemoryPool::claim`] 取一个
/// [`WorkArea`] 句柄，句柄 Drop 即注销。
#[derive(Debug)]
pub struct WorkMemoryPool {
    target: u64,
    inner: Mutex<PoolInner>,
    /// 累计计数（`Relaxed` 即可——诊断面，不参与同步）。
    stat: PoolCounters,
}

#[derive(Debug, Default)]
struct PoolCounters {
    optimal: AtomicU64,
    one_pass: AtomicU64,
    multi_pass: AtomicU64,
    extra_written: AtomicU64,
    extra_read: AtomicU64,
}

/// **一个算子内存区的句柄**（配额读取无锁；Drop 注销）。
#[derive(Debug)]
pub struct WorkArea {
    pool: Arc<WorkMemoryPool>,
    id: u64,
    name: &'static str,
    granted: Arc<AtomicU64>,
}

impl WorkArea {
    /// 当前配额（字节）。**每次检查点都应重读**——池内重平衡可能下调
    /// （设计 §4.2.1 ②"缩容即时生效"）。
    #[must_use]
    pub fn budget(&self) -> u64 {
        self.granted.load(Ordering::Relaxed)
    }

    /// 登记号。
    #[must_use]
    pub fn id(&self) -> u64 {
        self.id
    }

    /// 算子名。
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// **重新申报**（设计 §4.2.1 ①：边读边量、实际增长即改档；触发池内重平衡）。
    pub fn regrade(&self, claim: AreaClaim) {
        self.pool.regrade(self, claim);
    }
}

impl Drop for WorkArea {
    fn drop(&mut self) {
        let mut inner = self.pool.inner.lock().expect("池锁");
        inner.areas.retain(|a| a.id != self.id);
        self.pool.rebalance(&mut inner);
    }
}

impl WorkMemoryPool {
    /// 新建（`target` = `work_memory_target`；P0 冻结 = 4 GiB）。
    #[must_use]
    pub fn new(target: u64) -> Self {
        Self {
            target,
            inner: Mutex::new(PoolInner::default()),
            stat: PoolCounters::default(),
        }
    }

    /// 池目标（字节）。
    #[must_use]
    pub fn target(&self) -> u64 {
        self.target
    }

    /// 单内存区上限（5% 规则）。
    #[must_use]
    pub fn area_cap(&self) -> u64 {
        (self.target / SINGLE_AREA_CAP_DIVISOR).max(1)
    }

    /// **登记一个内存区**（`(0,0,0)` = 未申报 ⇒ 按公平份额起步）。
    pub fn claim(self: &Arc<Self>, name: &'static str) -> WorkArea {
        self.claim_with(name, AreaClaim::default())
    }

    /// 登记并带初始申报。
    pub fn claim_with(self: &Arc<Self>, name: &'static str, claim: AreaClaim) -> WorkArea {
        let granted = Arc::new(AtomicU64::new(0));
        let id = {
            let mut inner = self.inner.lock().expect("池锁");
            let id = inner.next_id;
            inner.next_id += 1;
            inner.areas.push(AreaEntry {
                id,
                name,
                claim,
                granted: Arc::clone(&granted),
            });
            self.rebalance(&mut inner);
            id
        };
        WorkArea {
            pool: Arc::clone(self),
            id,
            name,
            granted,
        }
    }

    /// 改档（重申报 + 重平衡）。
    pub fn regrade(&self, area: &WorkArea, claim: AreaClaim) {
        let mut inner = self.inner.lock().expect("池锁");
        if let Some(entry) = inner.areas.iter_mut().find(|a| a.id == area.id) {
            entry.claim = claim;
        }
        self.rebalance(&mut inner);
    }

    /// 当前配额合计（诊断 / 用例断言）。
    #[must_use]
    pub fn granted_total(&self) -> u64 {
        self.inner
            .lock()
            .expect("池锁")
            .areas
            .iter()
            .map(|a| a.granted.load(Ordering::Relaxed))
            .sum()
    }

    /// 活动内存区数。
    #[must_use]
    pub fn area_count(&self) -> usize {
        self.inner.lock().expect("池锁").areas.len()
    }

    /// 各内存区快照（登记序）。
    #[must_use]
    pub fn areas(&self) -> Vec<AreaInfo> {
        self.inner
            .lock()
            .expect("池锁")
            .areas
            .iter()
            .map(|a| AreaInfo {
                id: a.id,
                name: a.name,
                claim: a.claim,
                granted: a.granted.load(Ordering::Relaxed),
            })
            .collect()
    }

    /// **记一次内存区执行结果**（三态计数；池级累计面）。
    pub fn note_outcome(&self, outcome: WorkAreaOutcome) {
        let c = match outcome {
            WorkAreaOutcome::Optimal => &self.stat.optimal,
            WorkAreaOutcome::OnePass => &self.stat.one_pass,
            WorkAreaOutcome::MultiPass => &self.stat.multi_pass,
        };
        c.fetch_add(1, Ordering::Relaxed);
    }

    /// **记额外字节**（temp 读/写；`V$PGASTAT` 同名口径）。
    pub fn note_extra_bytes(&self, written: u64, read: u64) {
        self.stat
            .extra_written
            .fetch_add(written, Ordering::Relaxed);
        self.stat.extra_read.fetch_add(read, Ordering::Relaxed);
    }

    /// 累计监测快照。
    #[must_use]
    pub fn stats(&self) -> PoolStats {
        PoolStats {
            optimal: self.stat.optimal.load(Ordering::Relaxed),
            one_pass: self.stat.one_pass.load(Ordering::Relaxed),
            multi_pass: self.stat.multi_pass.load(Ordering::Relaxed),
            extra_bytes_written: self.stat.extra_written.load(Ordering::Relaxed),
            extra_bytes_read: self.stat.extra_read.load(Ordering::Relaxed),
        }
    }

    /// **全量重算配额**（①保底 → ②收益加码 → ③单区上限；设计 §4.2.1 ②）。
    fn rebalance(&self, inner: &mut PoolInner) {
        let n = inner.areas.len();
        if n == 0 {
            return;
        }
        let target = self.target;
        let cap = self.area_cap();
        let fair = target / n as u64;

        // ① 保底：未申报区 = 公平份额；已申报区 = min(one_pass, 公平份额)。
        let mut grants: Vec<u64> = Vec::with_capacity(n);
        let mut used = 0u64;
        for area in &inner.areas {
            let c = area.claim;
            let g = if c.ideal == 0 && c.one_pass == 0 {
                fair.min(cap) // 未申报：公平份额起步（设计 §4.2.1 ①）
            } else {
                c.one_pass.min(fair).min(cap)
            };
            used += g;
            grants.push(g);
        }

        // ② 加码：按收益密度降序（同密度按登记序），补向 ideal。
        let mut leftover = target.saturating_sub(used);
        if leftover > 0 {
            let mut order: Vec<usize> = (0..n).collect();
            order.sort_by(|&a, &b| {
                density_cmp(&inner.areas[a].claim, &inner.areas[b].claim).then(a.cmp(&b))
            });
            for i in order {
                if leftover == 0 {
                    break;
                }
                let c = inner.areas[i].claim;
                let want = c.ideal.saturating_sub(grants[i]);
                let room = cap.saturating_sub(grants[i]); // ③ 单区上限
                let add = want.min(leftover).min(room);
                grants[i] += add;
                leftover -= add;
            }
        }

        for (i, area) in inner.areas.iter().enumerate() {
            area.granted.store(grants[i], Ordering::Relaxed);
        }
    }
}

/// 收益密度比较（降序：密度大者排前）。整数比较避免浮点：
/// `benefit_a / gap_a` vs `benefit_b / gap_b` ⇔ `benefit_a × gap_b` vs `benefit_b × gap_a`。
fn density_cmp(a: &AreaClaim, b: &AreaClaim) -> std::cmp::Ordering {
    let gap = |c: &AreaClaim| c.ideal.saturating_sub(c.one_pass).max(1) as u128;
    let lhs = u128::from(a.benefit) * gap(b);
    let rhs = u128::from(b.benefit) * gap(a);
    // 降序（大者在前）；同值 = Equal（外层按登记序稳定）。
    rhs.cmp(&lhs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(ideal: u64, one_pass: u64, benefit: u64) -> AreaClaim {
        AreaClaim {
            ideal,
            one_pass,
            benefit,
        }
    }

    #[test]
    fn unclaimed_area_starts_at_fair_share_capped_at_five_percent() {
        let pool = Arc::new(WorkMemoryPool::new(20_000_000));
        // 单区：公平份额 = 全池，但 5% 上限先卡住。
        let a = pool.claim("Sort");
        assert_eq!(a.budget(), 1_000_000, "5% 单区上限（20M/20）");
        // 再登一个未申报区：公平份额 = 10M，仍被 5% 卡住。
        let _b = pool.claim("HashAgg");
        assert_eq!(a.budget(), 1_000_000);
        assert_eq!(pool.area_count(), 2);
        // 释放 ⇒ 注销且配额归还。
        drop(a);
        assert_eq!(pool.area_count(), 1);
        assert_eq!(pool.granted_total(), 1_000_000);
    }

    #[test]
    fn declared_areas_get_one_pass_first_then_top_up_by_density() {
        let pool = Arc::new(WorkMemoryPool::new(100_000_000));
        // 两区申报相同、且都在 5% 上限（5M）之内。
        let a = pool.claim_with("Sort", claim(4_000_000, 1_000_000, 10));
        let b = pool.claim_with("HashAgg", claim(4_000_000, 1_000_000, 10));
        // ① 保底 min(one_pass=1M, 公平份额=50M) = 1M；② 各自补到 ideal=4M。
        assert_eq!(a.budget(), 4_000_000);
        assert_eq!(b.budget(), 4_000_000);

        // 加码序按密度：**池真实稀缺**时才体现（≤20 个区永远到得了 5% 上限，
        // 见下一条用例）；这里 25 个区（Σ上限 = 25M > 目标 20M）争抢。
        let tight = Arc::new(WorkMemoryPool::new(20_000_000));
        let hi = tight.claim_with("HashAgg", claim(2_000_000, 100_000, 1000));
        let mut lo = Vec::new();
        for _ in 0..24 {
            lo.push(tight.claim_with("Sort", claim(2_000_000, 100_000, 1)));
        }
        assert_eq!(hi.budget(), 1_000_000, "高收益密度区吃满 5% 上限");
        assert!(
            lo.iter().any(|a| a.budget() < 1_000_000),
            "池量见底——同密度的低收益区按登记序先到先得、后面的被饿（{:?}）",
            lo.iter().map(|a| a.budget()).collect::<Vec<_>>()
        );
        assert!(
            lo.iter().all(|a| a.budget() >= 100_000),
            "保底 one_pass 不被侵占"
        );
        assert_eq!(tight.granted_total(), 20_000_000, "池被分尽、不超发");
    }

    #[test]
    fn twenty_saturating_areas_fill_the_pool_exactly() {
        // 20 个区、目标 20M ⇒ 5% 上限 1M/区：**Σ上限 = 目标**——"20 个满额
        // 内存区才吃满共享池"（P0 冻结记档的并发口径），无争抢、收益序不参与。
        let pool = Arc::new(WorkMemoryPool::new(20_000_000));
        let mut areas = Vec::new();
        for i in 0..20u64 {
            let benefit = if i == 7 { 1000 } else { 1 };
            areas.push(pool.claim_with("Sort", claim(2_000_000, 100_000, benefit)));
        }
        assert!(
            areas.iter().all(|a| a.budget() == 1_000_000),
            "每个区都到 5% 上限"
        );
        assert_eq!(pool.granted_total(), 20_000_000, "满额区数 = 目标/单区上限");
    }

    #[test]
    fn regrade_grows_and_shrinks_and_release_returns_memory() {
        let pool = Arc::new(WorkMemoryPool::new(100_000_000));
        let a = pool.claim_with("Sort", claim(2_000_000, 1_000_000, 1));
        let b = pool.claim_with("HashJoin", claim(2_000_000, 1_000_000, 1));
        assert_eq!((a.budget(), b.budget()), (2_000_000, 2_000_000));

        // 改档（增长）：a 实际输入远大于申报 ⇒ 重申报后被加码。
        a.regrade(claim(50_000_000, 1_000_000, 100));
        assert!(a.budget() >= 2_000_000, "改档后 a 不降（{}）", a.budget());

        // 改档（缩小）：a 归还多余 ⇒ 立即回到池内。
        a.regrade(claim(2_000_000, 1_000_000, 1));
        assert!(a.budget() <= 5_000_000);

        // 释放 b ⇒ a 的配额可以增大（池内共享、立即可用）。
        let before = a.budget();
        drop(b);
        a.regrade(claim(8_000_000, 1_000_000, 1));
        assert!(a.budget() > before, "释放后池量可供 a 加码");
        assert_eq!(pool.area_count(), 1);
    }

    #[test]
    fn outcome_and_extra_byte_counters_accumulate() {
        let pool = Arc::new(WorkMemoryPool::new(1 << 20));
        pool.note_outcome(WorkAreaOutcome::Optimal);
        pool.note_outcome(WorkAreaOutcome::OnePass);
        pool.note_outcome(WorkAreaOutcome::MultiPass);
        pool.note_outcome(WorkAreaOutcome::MultiPass);
        pool.note_extra_bytes(1000, 1500);
        let s = pool.stats();
        assert_eq!(s.optimal, 1);
        assert_eq!(s.one_pass, 1);
        assert_eq!(s.multi_pass, 2);
        assert_eq!(s.extra_bytes_written, 1000);
        assert_eq!(s.extra_bytes_read, 1500);
    }

    #[test]
    fn allocation_is_deterministic_across_identical_pools() {
        let build = || {
            let pool = Arc::new(WorkMemoryPool::new(20_000_000));
            let mut keep = Vec::new();
            for i in 0..20u64 {
                let benefit = if i % 3 == 0 { 10 } else { 1 };
                keep.push(pool.claim_with("Sort", claim(2_000_000, 100_000, benefit)));
            }
            (pool.areas(), keep)
        };
        let (a, _ka) = build();
        let (b, _kb) = build();
        assert_eq!(a, b, "同申报 ⇒ 同分配（确定、可复现）");
    }
}
