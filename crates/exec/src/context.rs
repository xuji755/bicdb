//! **执行上下文**（`ExecContext`；设计 §1.2）。
//!
//! 随请求携带：**语句快照**（语句级 RC，REQ-TXN-001）、类型化参数值、
//! deadline 与取消句柄（ENG 不变量 10）、以及每算子的**统计槽**
//! （行数——LIMIT 短路等行为由此可观测）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use bicdb_common::seq::CommitSeq;

use crate::error::ExecError;
use crate::value::Value;

/// 一个算子的统计槽。
#[derive(Debug, Clone, Default)]
pub struct OpStat {
    /// 算子名（诊断；如 `SeqScan`）。
    pub name: &'static str,
    /// 向上产出的行数。
    pub rows_out: u64,
}

/// **算子内存区的执行结果**（WMM 三态；Oracle `V$SYSSTAT` 口径，
/// 设计 §4.2——`optimal` 全内存 / `one-pass` 一趟外存 / `multi-pass` 多趟）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkAreaOutcome {
    /// 全内存完成。
    Optimal,
    /// 一趟外存（落 temp 一趟读回）。
    OnePass,
    /// 多趟（显著拖慢——要避免、要告警）。
    MultiPass,
}

/// 三态计数（诊断口径；切片 2c 只记 `optimal`——溢出随切片 6）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorkAreaStats {
    /// `optimal` 次数。
    pub optimal: u64,
    /// `one-pass` 次数。
    pub one_pass: u64,
    /// `multi-pass` 次数。
    pub multi_pass: u64,
}

/// **执行上下文**（单次执行一造；不属于计划——REQ-SQL-004）。
#[derive(Debug)]
pub struct ExecContext<'a> {
    snapshot: CommitSeq,
    params: Vec<Value>,
    deadline: Option<Instant>,
    cancel: Option<&'a AtomicBool>,
    stats: Vec<OpStat>,
    /// 工作内存预算（**WMM 最小面**：切片 2c 由计划侧给；`None` = 不限——
    /// 完整 WMM 随切片 6 接入，照 `work_memory_target` / 会话设置的语境填）。
    work_memory_budget: Option<u64>,
    work_areas: WorkAreaStats,
}

impl<'a> ExecContext<'a> {
    /// 新建（无参数、无截止、无取消）。
    #[must_use]
    pub fn new(snapshot: CommitSeq) -> Self {
        Self {
            snapshot,
            params: Vec::new(),
            deadline: None,
            cancel: None,
            stats: Vec::new(),
            work_memory_budget: None,
            work_areas: WorkAreaStats::default(),
        }
    }

    /// 带参数（会话级类型化参数——绑定期已定型）。
    #[must_use]
    pub fn with_params(mut self, params: &[Value]) -> Self {
        self.params = params.to_vec();
        self
    }

    /// **换参数表**（`NestedLoop` 的参数化重扫：装内表参数 / 还原语句参数）。
    pub fn set_params(&mut self, params: Vec<Value>) {
        self.params = params;
    }

    /// 带截止时间（到点即 [`ExecError::Deadline`]）。
    #[must_use]
    pub fn with_deadline(mut self, deadline: Instant) -> Self {
        self.deadline = Some(deadline);
        self
    }

    /// 带取消句柄（会话层 `cancel` 置位即 [`ExecError::Cancelled`]）。
    #[must_use]
    pub fn with_cancel(mut self, cancel: &'a AtomicBool) -> Self {
        self.cancel = Some(cancel);
        self
    }

    /// **语句快照**（读算子把它交给存储服务；可见性由服务负责）。
    #[must_use]
    pub fn snapshot(&self) -> CommitSeq {
        self.snapshot
    }

    /// 参数值。
    #[must_use]
    pub fn params(&self) -> &[Value] {
        &self.params
    }

    /// **取消/截止检查**（每个 `next()` 的开头；长循环另设检查点）。
    pub fn check(&self) -> Result<(), ExecError> {
        if let Some(flag) = self.cancel {
            if flag.load(Ordering::Relaxed) {
                return Err(ExecError::Cancelled);
            }
        }
        if let Some(t) = self.deadline {
            if Instant::now() >= t {
                return Err(ExecError::Deadline);
            }
        }
        Ok(())
    }

    /// 登记一个算子（open 时调用），返回统计槽下标。
    pub fn register_op(&mut self, name: &'static str) -> usize {
        self.stats.push(OpStat { name, rows_out: 0 });
        self.stats.len() - 1
    }

    /// 记账：某算子向上产出一行（`slot` 由 [`ExecContext::register_op`] 给）。
    pub fn note_row(&mut self, slot: usize) {
        if let Some(s) = self.stats.get_mut(slot) {
            s.rows_out += 1;
        }
    }

    /// **设工作内存预算**（字节；`None` = 不限）。切片 2c 的入口——
    /// 完整 WMM（共享池/租户/会话三层 + 分配算法）随切片 6。
    #[must_use]
    pub fn with_work_memory_budget(mut self, budget: Option<u64>) -> Self {
        self.work_memory_budget = budget;
        self
    }

    /// 工作内存预算（排序/哈希工作区在装载时检查；超限在切片 6 前报错）。
    #[must_use]
    pub fn work_memory_budget(&self) -> Option<u64> {
        self.work_memory_budget
    }

    /// **记一次算子内存区的执行结果**（WMM 三态计数）。
    pub fn note_work_area(&mut self, outcome: WorkAreaOutcome) {
        match outcome {
            WorkAreaOutcome::Optimal => self.work_areas.optimal += 1,
            WorkAreaOutcome::OnePass => self.work_areas.one_pass += 1,
            WorkAreaOutcome::MultiPass => self.work_areas.multi_pass += 1,
        }
    }

    /// 三态计数快照（诊断 / 用例断言）。
    #[must_use]
    pub fn work_area_stats(&self) -> WorkAreaStats {
        self.work_areas
    }

    /// 统计快照（诊断 / 用例断言）。
    #[must_use]
    pub fn stats(&self) -> &[OpStat] {
        &self.stats
    }

    /// 某算子的产出行数（按名；诊断便利口）。
    #[must_use]
    pub fn rows_out_of(&self, name: &str) -> u64 {
        self.stats
            .iter()
            .filter(|s| s.name == name)
            .map(|s| s.rows_out)
            .sum()
    }
}
