//! **行锁的等待结构与死锁检测**（§5.4.2）。
//!
//! 锁本身住在数据页的 **ITL** 里（§5.4：`txn_id` / `lock_cnt`）——本模块只管
//! **"等不到时谁在等谁"** 与 **"等出环了怎么办"**，是 §5.4.2 ②/④ 的落点：
//!
//! ```text
//! 获取（快路径，在写路径里）：空闲槽/已提交槽 ⇒ 占用（lock_cnt++）；
//!                             槽属他人且**活动** ⇒ 返回持锁者，转入等待
//! 等待（本模块）：按**持锁者**挂等待者表 —— (等待者, 持锁者, 资源, 起始时刻)
//!                 "我在等谁" + "谁在等我" 两半合起来 = 等待图的一条边
//! 唤醒：持锁事务结束 → 唤醒其**全部**等待者（**不做锁移交**，§5.4.2）
//!       醒来后从头重试（不假设行还在原地/槽没换人/行还存在）
//! 死锁：等待超阈值（默认 3 s，D-03）触发环检测；发现环 → 牺牲者 =
//!       **已修改行数最少者** → 语句级回滚（环即解开：牺牲者不再等待）
//! ```
//!
//! **与 latch 的分工**（§5.4.2 表）：latch 护内存结构、持有以页访问为界、
//! **持有 latch 时不得等待业务锁**；lock 护业务数据、持有到事务结束、**参与
//! 死锁图**——所以本模块的等待**只在 latch 之外**发生（写路径先放 latch，
//! 再由会话层登记等待）。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Duration;

use bicdb_common::seq::CommitSeq;
use bicdb_storage::rowid::RowId;
use bicdb_storage::undo::{TxnId, TxnSlot, TxnState, UndoChain};

/// 死锁检测的**触发阈值**（等待超时它才触发，§5.4.2 ④；Oracle 的默认口径）。
pub const DEADLOCK_THRESHOLD_MS: u64 = 3_000;

/// 一个等待者（挂在**持锁者**名下的记录）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Waiter {
    /// 等待者的事务标识。
    pub waiter: TxnId,
    /// 它等的那一行（诊断：谁堵住了谁）。
    pub row: RowId,
    /// 开始等待的时刻（单调毫秒）。
    pub since_ms: u64,
}

/// **等待结构**（内存；按持锁者组织——`txn_id` 能直接寻址到事务表槽，
/// 所以持锁者的两半指针天然对齐，§5.4.2 ②）。
#[derive(Debug, Default)]
pub struct WaitRegistry {
    waiting: BTreeMap<TxnId, Vec<Waiter>>,
}

impl WaitRegistry {
    /// 空表。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// **登记等待**：`waiter` 等 `holder` 释放 `row`。
    pub fn register(&mut self, waiter: TxnId, holder: TxnId, row: RowId, now_ms: u64) {
        let list = self.waiting.entry(holder).or_default();
        if !list.iter().any(|w| w.waiter == waiter) {
            list.push(Waiter {
                waiter,
                row,
                since_ms: now_ms,
            });
        }
    }

    /// **取消等待**（REQ-TXN-002）：只退出等待，**不动已持有的锁**。
    /// 返回是否确有一个等待被取消。
    pub fn cancel(&mut self, waiter: TxnId) -> bool {
        let mut found = false;
        for list in self.waiting.values_mut() {
            let before = list.len();
            list.retain(|w| w.waiter != waiter);
            found |= list.len() != before;
        }
        self.waiting.retain(|_, list| !list.is_empty());
        found
    }

    /// **唤醒**（持锁事务结束）：把它名下的等待者全部叫醒——**不做锁移交**
    /// （被唤醒者自行重试；低争用画像下惊群代价可忽略，§5.4.2）。
    pub fn wake(&mut self, holder: TxnId) -> Vec<TxnId> {
        self.waiting
            .remove(&holder)
            .map(|list| list.into_iter().map(|w| w.waiter).collect())
            .unwrap_or_default()
    }

    /// **等待图**的边（`等待者 → 持锁者`，两半指针合成一条边；按序输出，
    /// 检测可复现）。
    #[must_use]
    pub fn edges(&self) -> Vec<(TxnId, TxnId)> {
        let mut out = Vec::new();
        for (holder, list) in &self.waiting {
            for w in list {
                out.push((w.waiter, *holder));
            }
        }
        out
    }

    /// 某人名下的等待者（诊断/测试）。
    #[must_use]
    pub fn waiters_of(&self, holder: TxnId) -> Vec<Waiter> {
        self.waiting.get(&holder).cloned().unwrap_or_default()
    }

    /// 在册等待者总数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.waiting.values().map(Vec::len).sum()
    }

    /// 是否无人等待。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// **最老的等待已持续多少毫秒**（阈值触发用；无人等待 ⇒ `None`）。
    #[must_use]
    pub fn oldest_wait_ms(&self, now_ms: u64) -> Option<u64> {
        self.waiting
            .values()
            .flat_map(|list| list.iter())
            .map(|w| now_ms.saturating_sub(w.since_ms))
            .max()
    }
}

/// 一次死锁判定（§5.4.2 ④）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deadlock {
    /// 环上事务（从环的顺序给出，诊断/日志）。
    pub cycle: Vec<TxnId>,
    /// **牺牲者**：环上"已修改行数最少者"（D-03；`rec_count` 作代理，
    /// 与行数同阶）。它做**语句级回滚**即解环——不再等待，其余部分保留。
    pub victim: TxnId,
    /// 牺牲者的修改量（诊断）。
    pub victim_work: u32,
}

/// **死锁检测**：等待超 `threshold_ms` 才触发（§5.4.2 ④）；读等待结构建图、
/// 找环、选牺牲者。
///
/// 返回 `None` ⇒ 未到阈值 / 无环。环检测按事务标识有序遍历（可复现）。
pub fn detect_deadlock(
    registry: &WaitRegistry,
    chain: &UndoChain<'_, '_>,
    now_ms: u64,
    threshold_ms: u64,
) -> Result<Option<Deadlock>, bicdb_storage::undo::UndoChainError> {
    let Some(oldest) = registry.oldest_wait_ms(now_ms) else {
        return Ok(None);
    };
    if oldest < threshold_ms {
        return Ok(None);
    }
    // 邻接表（等待者 → 持锁者）。
    let mut edges: BTreeMap<TxnId, Vec<TxnId>> = BTreeMap::new();
    for (waiter, holder) in registry.edges() {
        edges.entry(waiter).or_default().push(holder);
        edges.entry(holder).or_default();
    }
    if let Some(cycle) = find_cycle(&edges) {
        let victim = pick_victim(&cycle, chain)?;
        return Ok(Some(Deadlock {
            cycle,
            victim: victim.0,
            victim_work: victim.1,
        }));
    }
    Ok(None)
}

/// 有向图找环（DFS 三色；节点按序入栈，返回环上节点，起点为环中最小者）。
fn find_cycle(edges: &BTreeMap<TxnId, Vec<TxnId>>) -> Option<Vec<TxnId>> {
    #[derive(Clone, Copy, PartialEq)]
    enum Color {
        White,
        Gray,
        Black,
    }
    let mut color: BTreeMap<TxnId, Color> = edges.keys().map(|k| (*k, Color::White)).collect();
    let mut stack: Vec<TxnId> = Vec::new();

    fn dfs(
        at: TxnId,
        edges: &BTreeMap<TxnId, Vec<TxnId>>,
        color: &mut BTreeMap<TxnId, Color>,
        stack: &mut Vec<TxnId>,
    ) -> Option<Vec<TxnId>> {
        color.insert(at, Color::Gray);
        stack.push(at);
        for next in edges.get(&at).into_iter().flatten() {
            match color.get(next).copied().unwrap_or(Color::White) {
                Color::Gray => {
                    // 回边 ⇒ 环：从栈里 next 的位置截取。
                    let at_pos = stack.iter().position(|t| t == next).expect("在栈上");
                    return Some(stack[at_pos..].to_vec());
                }
                Color::White => {
                    if let Some(cycle) = dfs(*next, edges, color, stack) {
                        return Some(cycle);
                    }
                }
                Color::Black => {}
            }
        }
        stack.pop();
        color.insert(at, Color::Black);
        None
    }

    for start in edges.keys() {
        if color.get(start).copied() == Some(Color::White) {
            if let Some(cycle) = dfs(*start, edges, &mut color, &mut stack) {
                return Some(cycle);
            }
        }
    }
    None
}

/// **牺牲者选择**（D-03）：环中"已修改行数最少者"。
///
/// 修改量取事务表槽的 `rec_count`（撤销记录数）作**代理**——与行数同阶、
/// 槽内即有、无需扫页；查不到（槽已回收）记 0（它已不持有任何锁）。
fn pick_victim(
    cycle: &[TxnId],
    chain: &UndoChain<'_, '_>,
) -> Result<(TxnId, u32), bicdb_storage::undo::UndoChainError> {
    let mut best: Option<(TxnId, u32)> = None;
    for &t in cycle {
        let work = match chain.lookup(t)? {
            Some(slot) => slot_work(&slot),
            None => 0,
        };
        match best {
            Some((_, w)) if w <= work => {}
            _ => best = Some((t, work)),
        }
    }
    Ok(best.expect("环非空"))
}

/// 事务的"修改量"（`rec_count` 作代理）。
fn slot_work(slot: &TxnSlot) -> u32 {
    if slot.state == TxnState::Free {
        0
    } else {
        slot.rec_count
    }
}

/// **等待门**：把 [`WaitRegistry`] 变成**可阻塞的挂起点**（会话层"放 latch →
/// 登记 → 挂起 → 醒来重试"的挂起点，§5.4.2 ②）。
///
/// 为什么要有"门"而不是只有注册表：注册表是**数据**（等待图），挂起是**机制**
/// （`Condvar`）。两者同一把内部锁保护，于是"登记 → 检查已被唤醒 → 挂起"是
/// 原子的——**唤醒不会丢**（经典丢唤醒窗口：唤醒发生在登记之后、挂起之前）。
///
/// **唤醒语义仍是"不接管"**：`wake` 只标记等待者并通知，被唤醒者**自己重试**
/// （不假设行还在原地/槽没换人/行还存在）；**已持有的锁一个都不动**。
pub struct WaitGate {
    inner: Mutex<GateState>,
    cv: Condvar,
}

struct GateState {
    registry: WaitRegistry,
    /// 已唤醒但**尚未被 park 取走**的等待者（唤醒先于挂起时靠它兜底）。
    woken: BTreeSet<TxnId>,
}

impl std::fmt::Debug for WaitGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let g = self.lock();
        f.debug_struct("WaitGate")
            .field("waiters", &g.registry.len())
            .field("woken", &g.woken.len())
            .finish()
    }
}

impl Default for WaitGate {
    fn default() -> Self {
        Self::new()
    }
}

impl WaitGate {
    /// 空门。
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(GateState {
                registry: WaitRegistry::new(),
                woken: BTreeSet::new(),
            }),
            cv: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, GateState> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// **登记等待**：`waiter` 等 `holder` 释放 `row`。
    pub fn register(&self, waiter: TxnId, holder: TxnId, row: RowId, now_ms: u64) {
        let mut g = self.lock();
        g.woken.remove(&waiter); // 新一轮等待：清掉上一轮可能残留的唤醒标记
        g.registry.register(waiter, holder, row, now_ms);
    }

    /// **取消等待**（REQ-TXN-002：只退出等待，不动已持有的锁）。
    pub fn cancel(&self, waiter: TxnId) -> bool {
        let mut g = self.lock();
        g.woken.remove(&waiter);
        g.registry.cancel(waiter)
    }

    /// **唤醒持锁事务名下的全部等待者**（事务结束的提交点副作用；
    /// 持锁者结束后调用，§5.4.2 ③）。返回被唤醒的事务（诊断/测试）。
    ///
    /// 唤醒先于挂起也不算丢：标记留在 `woken` 里，`park` 一进门就看到。
    pub fn wake(&self, holder: TxnId) -> Vec<TxnId> {
        let mut g = self.lock();
        let woken = g.registry.wake(holder);
        for t in &woken {
            g.woken.insert(*t);
        }
        self.cv.notify_all();
        woken
    }

    /// **挂起**直到"自己"被唤醒（返回 `true`）或超时（返回 `false`）。
    ///
    /// 超时**不是错误**：调用方去做死锁检测、再决定继续等还是放弃。
    /// 被别人的唤醒通知惊到但自己未被标记 ⇒ 继续等（循环内判断）。
    pub fn park(&self, waiter: TxnId, timeout: Duration) -> bool {
        let mut g = self.lock();
        if g.woken.remove(&waiter) {
            return true; // 唤醒已先行（登记后、挂起前的窗口）
        }
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return false;
            }
            let (mut guard, _) = self
                .cv
                .wait_timeout(g, left)
                .unwrap_or_else(|e| e.into_inner());
            if guard.woken.remove(&waiter) {
                return true;
            }
            g = guard;
        }
    }

    /// 在门锁内执行一段只读逻辑（死锁检测等——等待图与挂起同锁 ⇒ 快照一致）。
    pub fn with_registry<T>(&self, f: impl FnOnce(&WaitRegistry) -> T) -> T {
        f(&self.lock().registry)
    }

    /// 被取消/唤醒后**未取走**的标记数（诊断）。
    #[must_use]
    pub fn pending_wakes(&self) -> usize {
        self.lock().woken.len()
    }

    /// 某持锁者名下的等待者（诊断/测试）。
    #[must_use]
    pub fn waiters_of(&self, holder: TxnId) -> Vec<Waiter> {
        self.lock().registry.waiters_of(holder)
    }
}

/// 等待/死锁相关的提交点副作用：**唤醒**。
///
/// 提交与回滚都会结束事务，两者都调用它（持锁事务结束 ⇒ 唤醒其全部等待者，
/// §5.4.2 ③——"释放锁就是槽状态的改变"，唤醒是它的后半步）。
#[must_use]
pub fn on_txn_end(registry: &mut WaitRegistry, txn: TxnId) -> Vec<TxnId> {
    registry.wake(txn)
}

/// 已提交序号的便捷构造（测试/诊断用）。
#[must_use]
pub fn seq(v: u64) -> CommitSeq {
    CommitSeq::from_raw(v).expect("48 位域内")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tid(slot: u8) -> TxnId {
        TxnId::from_parts(0, slot, 0)
    }

    fn row(n: u16) -> RowId {
        RowId::from_parts(1, 100, n).unwrap()
    }

    #[test]
    fn register_cancel_and_wake_without_handover() {
        let mut reg = WaitRegistry::new();
        reg.register(tid(1), tid(9), row(1), 1_000);
        reg.register(tid(2), tid(9), row(2), 1_100);
        reg.register(tid(3), tid(8), row(3), 1_200);
        assert_eq!(reg.len(), 3);
        assert_eq!(reg.waiters_of(tid(9)).len(), 2);

        // 取消：只退出等待；别人的等待不受影响。
        assert!(reg.cancel(tid(1)));
        assert!(!reg.cancel(tid(1)), "重复取消无副作用");
        assert_eq!(reg.waiters_of(tid(9)).len(), 1);
        assert_eq!(reg.waiters_of(tid(8)).len(), 1);

        // 唤醒：持锁者结束 ⇒ 全部唤醒（不移交）；醒来后自行重试。
        let woken = reg.wake(tid(9));
        assert_eq!(woken, vec![tid(2)]);
        assert!(reg.waiters_of(tid(9)).is_empty());
        assert_eq!(reg.len(), 1, "tid(8) 的等待者仍在等");
        assert!(reg.wake(tid(9)).is_empty(), "已无人等待");
    }

    #[test]
    fn oldest_wait_drives_the_threshold() {
        let mut reg = WaitRegistry::new();
        assert_eq!(reg.oldest_wait_ms(5_000), None);
        reg.register(tid(1), tid(9), row(1), 4_000);
        reg.register(tid(2), tid(9), row(2), 1_000);
        assert_eq!(reg.oldest_wait_ms(4_500), Some(3_500), "取最老的");
        assert_eq!(reg.oldest_wait_ms(500), Some(0), "未超时不回绕");
    }

    #[test]
    fn cycle_edges_are_the_two_halves_of_the_wait() {
        let mut reg = WaitRegistry::new();
        reg.register(tid(1), tid(2), row(1), 1);
        reg.register(tid(2), tid(3), row(2), 1);
        reg.register(tid(3), tid(1), row(3), 1);
        let mut edges = reg.edges();
        edges.sort();
        assert_eq!(
            edges,
            vec![(tid(1), tid(2)), (tid(2), tid(3)), (tid(3), tid(1))]
        );
    }

    #[test]
    fn gate_wake_before_park_is_not_lost() {
        // 经典丢唤醒窗口：唤醒发生在**登记之后、挂起之前**——标记兜底。
        let gate = WaitGate::new();
        gate.register(tid(1), tid(9), row(1), 0);
        assert_eq!(gate.wake(tid(9)), vec![tid(1)]);
        assert_eq!(gate.pending_wakes(), 1);
        assert!(gate.park(tid(1), Duration::from_millis(50)), "立即返回");
        assert_eq!(gate.pending_wakes(), 0, "标记被取走");

        // 新一轮等待清掉残留标记：不会把上一轮的唤醒算进这一轮。
        gate.register(tid(1), tid(9), row(1), 0);
        assert_eq!(gate.pending_wakes(), 0);
        assert!(
            !gate.park(tid(1), Duration::from_millis(5)),
            "没人唤醒 ⇒ 超时"
        );
    }

    #[test]
    fn gate_parks_a_thread_until_the_holder_ends() {
        let gate = std::sync::Arc::new(WaitGate::new());
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let g2 = std::sync::Arc::clone(&gate);
        let waiter = std::thread::spawn(move || {
            g2.register(tid(1), tid(9), row(1), 0);
            tx.send(()).unwrap();
            g2.park(tid(1), Duration::from_secs(5))
        });
        rx.recv().unwrap();
        // 唤醒前 park 不返回（短探测：不是靠超时"假醒来"）。
        assert!(
            !gate.with_registry(|r| r.waiters_of(tid(9)).is_empty()),
            "等待者在册"
        );
        assert_eq!(gate.wake(tid(9)), vec![tid(1)]);
        assert!(waiter.join().unwrap(), "被持锁者结束的唤醒叫醒");
    }

    #[test]
    fn gate_cancel_only_leaves_the_queue() {
        let gate = WaitGate::new();
        gate.register(tid(1), tid(9), row(1), 0);
        gate.register(tid(2), tid(9), row(2), 0);
        assert!(gate.cancel(tid(1)));
        assert_eq!(gate.waiters_of(tid(9)).len(), 1, "另一个等待者不受影响");
        assert_eq!(gate.wake(tid(9)), vec![tid(2)]);
        // 取消者不再等在册（也就不会"醒来"）。
        assert!(!gate.park(tid(1), Duration::from_millis(5)));
    }
}
