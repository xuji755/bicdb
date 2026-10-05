//! **最老快照集合**（§12.7）：undo 回收的唯一输入。
//!
//! 语句开始登记、语句结束注销（只在这两处动它）；**空集时最小值 = 当前提交
//! 序号**（没有活跃快照就没有谁需要旧版本，判据自然给出"可一路回收到最新"）。
//! 每工作区一份、在内存；随检查点把最小值写进控制文件的检查点进度
//! （`oldest_snapshot_commit_seq`，§2.6），供恢复期判断"undo 链能否走到底"。
//!
//! 结构：**按提交序号排序**的 `BTreeMap<提交序号, 句柄集合>`——取最小值
//! `O(log n)`、注销 `O(log n)`（小顶堆的按句柄删除要 O(n)）。同一序号可被
//! 多个语句持有（句柄集合），最小值取第一个键。
//!
//! **并发**：P3 单写者下为普通结构；P4 的读路径并发由"实例级共享点"加闩锁
//! 接入（NFR 点名的唯一共享点就是它）——届时本结构原样包一层 `Latch`。

use std::collections::{BTreeMap, BTreeSet};

use bicdb_common::seq::CommitSeq;

/// 快照句柄（登记时发放；注销与重复注销的凭据）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SnapshotHandle(u64);

impl SnapshotHandle {
    /// 原始值（诊断/日志用）。
    #[must_use]
    pub fn raw(self) -> u64 {
        self.0
    }
}

/// 每工作区一份的**最老快照集合**。
#[derive(Debug, Default)]
pub struct SnapshotRegistry {
    /// 提交序号 → 持有该快照的句柄集合（有序 ⇒ 最小值 = 第一个键）。
    active: BTreeMap<CommitSeq, BTreeSet<SnapshotHandle>>,
    next: u64,
}

impl SnapshotRegistry {
    /// 空集合。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// **登记**一个快照（语句开始）：返回句柄。
    pub fn register(&mut self, snapshot: CommitSeq) -> SnapshotHandle {
        let handle = SnapshotHandle(self.next);
        self.next += 1;
        self.active.entry(snapshot).or_default().insert(handle);
        handle
    }

    /// **注销**（语句结束）。返回该句柄是否在册（重复注销 ⇒ `false`，无副作用）。
    pub fn release(&mut self, handle: SnapshotHandle) -> bool {
        let Some(seq) = self
            .active
            .iter()
            .find(|(_, handles)| handles.contains(&handle))
            .map(|(seq, _)| *seq)
        else {
            return false;
        };
        if let Some(handles) = self.active.get_mut(&seq) {
            handles.remove(&handle);
            if handles.is_empty() {
                self.active.remove(&seq);
            }
        }
        true
    }

    /// **最老快照提交序号**（回收判据的输入）：空集时返回 `current_commit_seq`
    /// （§12.7——"没有活跃快照，可一路回收到最新"）。
    #[must_use]
    pub fn oldest(&self, current_commit_seq: CommitSeq) -> CommitSeq {
        self.active
            .keys()
            .next()
            .copied()
            .unwrap_or(current_commit_seq)
    }

    /// 活跃快照数（诊断）。
    #[must_use]
    pub fn len(&self) -> usize {
        self.active.values().map(BTreeSet::len).sum()
    }

    /// 是否为空集。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.active.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seq(v: u64) -> CommitSeq {
        CommitSeq::from_raw(v).unwrap()
    }

    #[test]
    fn oldest_is_the_minimum_and_empty_falls_back_to_current() {
        let mut reg = SnapshotRegistry::new();
        assert!(reg.is_empty());
        assert_eq!(reg.oldest(seq(100)), seq(100), "空集 ⇒ 当前提交序号");

        let a = reg.register(seq(30));
        let b = reg.register(seq(10));
        let c = reg.register(seq(20));
        assert_eq!(reg.oldest(seq(100)), seq(10), "三者取最小");
        assert_eq!(reg.len(), 3);

        // **乱序注销**：先销最小者 ⇒ 次小者顶上；再销最大者 ⇒ 水位不动。
        assert!(reg.release(b));
        assert_eq!(reg.oldest(seq(100)), seq(20));
        assert!(reg.release(a));
        assert_eq!(reg.oldest(seq(100)), seq(20), "剩下的 c 仍顶住水位");
        assert_eq!(reg.len(), 1);
        // 重复注销：无副作用。
        assert!(!reg.release(a));
        assert_eq!(reg.oldest(seq(100)), seq(20));
        // 全部注销 ⇒ 回到"当前提交序号"。
        assert!(reg.release(c));
        assert!(reg.is_empty());
        assert_eq!(reg.oldest(seq(100)), seq(100));
    }

    #[test]
    fn handles_are_distinct_even_for_equal_snapshots() {
        let mut reg = SnapshotRegistry::new();
        let h1 = reg.register(seq(5));
        let h2 = reg.register(seq(5));
        assert_ne!(h1.raw(), h2.raw());
        assert_eq!(reg.len(), 2);
        reg.release(h1);
        assert_eq!(reg.len(), 1, "同值快照也各自在册");
        assert_eq!(reg.oldest(seq(9)), seq(5));
    }
}
