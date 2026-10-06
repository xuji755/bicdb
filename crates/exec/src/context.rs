//! **执行上下文**（`ExecContext`；设计 §1.2）。
//!
//! 随请求携带：**语句快照**（语句级 RC，REQ-TXN-001）、类型化参数值、
//! deadline 与取消句柄（ENG 不变量 10）、以及每算子的**统计槽**
//! （行数——LIMIT 短路等行为由此可观测）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use bicdb_common::seq::CommitSeq;

use crate::error::ExecError;
use crate::value::Value;
use crate::wmm::{AreaClaim, WorkArea, WorkMemoryPool};

/// 一个算子的统计槽。
#[derive(Debug, Clone, Default)]
pub struct OpStat {
    /// 算子名（诊断；如 `SeqScan`）。
    pub name: &'static str,
    /// 向上产出的行数。
    pub rows_out: u64,
    /// **影响的写行数**（DML 口径：插入/更新/删除的行数——与"产出"分开）。
    pub rows_affected: u64,
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

/// 三态计数 + 额外字节（诊断口径；设计 §4.2 监测四件套：三态计数与
/// extra bytes 都是设计的一部分，不是可选的观测）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorkAreaStats {
    /// `optimal` 次数。
    pub optimal: u64,
    /// `one-pass` 次数。
    pub one_pass: u64,
    /// `multi-pass` 次数。
    pub multi_pass: u64,
    /// 溢出写出的额外字节（temp 写）。
    pub extra_bytes_written: u64,
    /// 溢出读回的额外字节（temp 读）。
    pub extra_bytes_read: u64,
}

/// **执行上下文**（单次执行一造；不属于计划——REQ-SQL-004）。
#[derive(Debug)]
pub struct ExecContext<'a> {
    snapshot: CommitSeq,
    params: Vec<Value>,
    deadline: Option<Instant>,
    cancel: Option<&'a AtomicBool>,
    stats: Vec<OpStat>,
    /// 工作内存预算（**固定值形态**＝会话 MANUAL 模式：`work_area_size` 或
    /// 测试直给；`None` = 不限。AUTO 模式由 [`Self::claim_area`] 从共享池取）。
    work_memory_budget: Option<u64>,
    /// **实例共享池**（AUTO 模式；会话层注入——执行器不持有实例状态）。
    pool: Option<Arc<WorkMemoryPool>>,
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
            pool: None,
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
        self.stats.push(OpStat {
            name,
            rows_out: 0,
            rows_affected: 0,
        });
        self.stats.len() - 1
    }

    /// 记账：某算子向上产出一行（`slot` 由 [`ExecContext::register_op`] 给）。
    pub fn note_row(&mut self, slot: usize) {
        if let Some(s) = self.stats.get_mut(slot) {
            s.rows_out += 1;
        }
    }

    /// **设固定工作内存预算**（字节；`None` = 不限）——会话 MANUAL 模式
    /// （`work_area_size`）与测试的直给口。
    #[must_use]
    pub fn with_work_memory_budget(mut self, budget: Option<u64>) -> Self {
        self.work_memory_budget = budget;
        self
    }

    /// 固定工作内存预算（未声明内存区时生效；见 [`Self::budget_for`]）。
    #[must_use]
    pub fn work_memory_budget(&self) -> Option<u64> {
        self.work_memory_budget
    }

    /// **接实例共享池**（AUTO 模式；会话层注入——设计 §4.2 三层关系）。
    #[must_use]
    pub fn with_work_memory_pool(mut self, pool: Arc<WorkMemoryPool>) -> Self {
        self.pool = Some(pool);
        self
    }

    /// 实例共享池（若有）。
    #[must_use]
    pub fn work_memory_pool(&self) -> Option<&Arc<WorkMemoryPool>> {
        self.pool.as_ref()
    }

    /// **登记一个算子内存区**（算子 open 时：申报 = 未申报，边读边量再
    /// [`WorkArea::regrade`]；无池 = `None`——算子退回固定预算/不限形态）。
    #[must_use]
    pub fn claim_area(&self, name: &'static str) -> Option<WorkArea> {
        self.pool.as_ref().map(|p| p.claim(name))
    }

    /// 登记并带初始申报（代价模型到位后由计划侧给申报的入口）。
    #[must_use]
    pub fn claim_area_with(&self, name: &'static str, claim: AreaClaim) -> Option<WorkArea> {
        self.pool.as_ref().map(|p| p.claim_with(name, claim))
    }

    /// **本算子的工作内存额度**：声明了内存区 ⇒ 池配额（每次重读——
    /// 池内重平衡可下调）；否则退固定预算（`None` = 不限）。
    #[must_use]
    pub fn budget_for(&self, area: Option<&WorkArea>) -> Option<u64> {
        match area {
            Some(a) => Some(a.budget()),
            None => self.work_memory_budget,
        }
    }

    /// **记额外字节**（temp 读/写；`V$PGASTAT` 同名口径）。
    pub fn note_extra_bytes(&mut self, written: u64, read: u64) {
        self.work_areas.extra_bytes_written += written;
        self.work_areas.extra_bytes_read += read;
        if let Some(p) = &self.pool {
            p.note_extra_bytes(written, read);
        }
    }

    /// **记一次算子内存区的执行结果**（WMM 三态计数；池级累计同步）。
    pub fn note_work_area(&mut self, outcome: WorkAreaOutcome) {
        match outcome {
            WorkAreaOutcome::Optimal => self.work_areas.optimal += 1,
            WorkAreaOutcome::OnePass => self.work_areas.one_pass += 1,
            WorkAreaOutcome::MultiPass => self.work_areas.multi_pass += 1,
        }
        if let Some(p) = &self.pool {
            p.note_outcome(outcome);
        }
    }

    /// 三态计数快照（诊断 / 用例断言）。
    #[must_use]
    pub fn work_area_stats(&self) -> WorkAreaStats {
        self.work_areas
    }

    /// **记账：某 DML 算子影响了 n 行**（`INSERT`/`UPDATE`/`DELETE` 口径）。
    pub fn note_affected(&mut self, slot: usize, n: u64) {
        if let Some(s) = self.stats.get_mut(slot) {
            s.rows_affected += n;
        }
    }

    /// 某算子的影响行数（按名——DML 断言便利口）。
    #[must_use]
    pub fn rows_affected_of(&self, name: &str) -> u64 {
        self.stats
            .iter()
            .filter(|s| s.name == name)
            .map(|s| s.rows_affected)
            .sum()
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
