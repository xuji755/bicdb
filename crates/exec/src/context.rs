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

/// **执行上下文**（单次执行一造；不属于计划——REQ-SQL-004）。
#[derive(Debug)]
pub struct ExecContext<'a> {
    snapshot: CommitSeq,
    params: &'a [Value],
    deadline: Option<Instant>,
    cancel: Option<&'a AtomicBool>,
    stats: Vec<OpStat>,
}

impl<'a> ExecContext<'a> {
    /// 新建（无参数、无截止、无取消）。
    #[must_use]
    pub fn new(snapshot: CommitSeq) -> Self {
        Self {
            snapshot,
            params: &[],
            deadline: None,
            cancel: None,
            stats: Vec::new(),
        }
    }

    /// 带参数。
    #[must_use]
    pub fn with_params(mut self, params: &'a [Value]) -> Self {
        self.params = params;
        self
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
        self.params
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
