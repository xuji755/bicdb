//! **聚合族**（切片 4）：`ScalarAgg` / `HashAgg` / `SortedAgg`（设计 §2.4）。
//!
//! - **三段式**（`TYP` 口径）：初值 → 转移（逐行） → 归并/收尾；
//! - **`DISTINCT` 变体是修饰、不是新算子**：每组携带"已见值集"（哈希形态），
//!   命中即跳过转移（设计 §2.4——`COUNT(DISTINCT x)` 等）；
//! - **空输入语义**（§4.5）：无 `GROUP BY` ⇒ **恒出一行**（`COUNT(*)=0`、
//!   `SUM/AVG/MIN/MAX` = NULL）；有 `GROUP BY` ⇒ 零行；
//! - `NULL` 不参与聚合（`COUNT(x)` 计非空、`SUM/AVG/MIN/MAX` 跳过 NULL）；
//! - **HAVING 是聚合之上的 `Filter`**（本模块不含——计划侧叠加）。

use std::collections::{HashMap, HashSet};

use bicdb_types::Number;

use crate::context::{ExecContext, WorkAreaOutcome};
use crate::error::ExecError;
use crate::expr::{self, Expr};
use crate::operator::Operator;
use crate::sort::SortKey;
use crate::value::{Row, Value};
use crate::wmm::{AreaClaim, WorkArea};

/// 聚合函数（SQL 面清单：`COUNT(*)`/`COUNT(x)`/`SUM`/`AVG`/`MIN`/`MAX`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggKind {
    /// `COUNT(*)`（含 NULL 行）。
    CountStar,
    /// `COUNT(x)`（计非空）。
    Count,
    /// `SUM(x)`。
    Sum,
    /// `AVG(x)`。
    Avg,
    /// `MIN(x)`。
    Min,
    /// `MAX(x)`。
    Max,
}

/// 一个聚合项（函数 + 参数 + `DISTINCT` 修饰）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AggSpec {
    /// 函数。
    pub kind: AggKind,
    /// 参数（`COUNT(*)` 为 `None`）。
    pub arg: Option<Expr>,
    /// `DISTINCT` 修饰（设计 §2.4：修饰不新增算子）。
    pub distinct: bool,
}

/// 聚合的转移状态（三段式的中段）。
#[derive(Debug, Clone)]
enum AggState {
    /// `COUNT` 族。
    Count(u64),
    /// `SUM`（零元 = `None` ⇒ 收尾给 NULL）。
    Sum(Option<Number>),
    /// `AVG = sum / count`。
    Avg {
        /// 和。
        sum: Number,
        /// 非空行数。
        count: u64,
    },
    /// `MIN`/`MAX` 当前最优值。
    Best(Option<Value>),
}

/// 一个聚合项的运行时（状态 + `DISTINCT` 的已见值集）。
#[derive(Debug)]
pub struct AggAcc {
    spec: AggSpec,
    state: AggState,
    seen: Option<HashSet<Value>>,
}

impl AggAcc {
    /// 由规格初始化（三段式的初值段）。
    #[must_use]
    pub fn new(spec: AggSpec) -> Self {
        let state = match spec.kind {
            AggKind::CountStar | AggKind::Count => AggState::Count(0),
            AggKind::Sum => AggState::Sum(None),
            AggKind::Avg => AggState::Avg {
                sum: Number::zero(),
                count: 0,
            },
            AggKind::Min | AggKind::Max => AggState::Best(None),
        };
        let seen = if spec.distinct {
            Some(HashSet::new())
        } else {
            None
        };
        Self { spec, state, seen }
    }

    /// **转移**：喂一行（对 `arg` 求值后调用）。
    pub fn advance(&mut self, value: Option<&Value>, params: &[Value]) -> Result<(), ExecError> {
        let _ = params;
        let v = match value {
            None => {
                // `COUNT(*)`：不看值（含 NULL 行）。
                if let AggState::Count(c) = &mut self.state {
                    *c += 1;
                }
                return Ok(());
            }
            Some(v) if v.is_null() => return Ok(()), // NULL 不参与聚合
            Some(v) => v,
        };
        if let Some(seen) = &mut self.seen {
            if !seen.insert(v.clone()) {
                return Ok(()); // DISTINCT：已见过 ⇒ 跳过转移
            }
        }
        match &mut self.state {
            AggState::Count(c) => *c += 1,
            AggState::Sum(sum) => {
                let n = as_number(v)?;
                *sum = Some(match sum.take() {
                    Some(s) => s.add(&n).map_err(number_err)?,
                    None => n,
                });
            }
            AggState::Avg { sum, count } => {
                let n = as_number(v)?;
                *sum = sum.add(&n).map_err(number_err)?;
                *count += 1;
            }
            AggState::Best(best) => {
                let take = match best {
                    None => true,
                    Some(b) => {
                        let ord = expr::compare_values(v, b)?.expect("两侧都非 NULL");
                        match self.spec.kind {
                            AggKind::Min => ord == std::cmp::Ordering::Less,
                            // MAX（以及防御：其他 kind 不进 Best）
                            _ => ord == std::cmp::Ordering::Greater,
                        }
                    }
                };
                if take {
                    *best = Some(v.clone());
                }
            }
        }
        Ok(())
    }

    /// **转移 + 内存记账**（`HashAgg` 溢出判定用）：与 [`AggAcc::advance`]
    /// 语义相同，另返回本次新增占用字节（`DISTINCT` 已见值集增长——
    /// 非 DISTINCT 的累加器大小不随行数增长，返回 0）。
    pub fn advance_accounted(
        &mut self,
        value: Option<&Value>,
        params: &[Value],
    ) -> Result<u64, ExecError> {
        let before = self.seen.as_ref().map_or(0, HashSet::len);
        self.advance(value, params)?;
        let after = self.seen.as_ref().map_or(0, HashSet::len);
        if after == before {
            return Ok(0);
        }
        // 新增了一个已见值（值本身 + 集合项开销）。
        Ok(match value {
            Some(v) => (crate::value::value_bytes(v) + 32) as u64,
            None => 0,
        })
    }

    /// **收尾**（三段式的末段）：聚合结果值。
    pub fn finalize(&self) -> Result<Value, ExecError> {
        Ok(match &self.state {
            AggState::Count(c) => Value::Number(count_number(*c)),
            AggState::Sum(s) => match s {
                Some(n) => Value::Number(n.clone()),
                None => Value::Null,
            },
            AggState::Avg { sum, count } => {
                if *count == 0 {
                    Value::Null
                } else {
                    Value::Number(sum.div(&count_number(*count)).map_err(number_err)?)
                }
            }
            AggState::Best(b) => b.clone().unwrap_or(Value::Null),
        })
    }
}

fn as_number(v: &Value) -> Result<Number, ExecError> {
    match v {
        Value::Number(n) => Ok(n.clone()),
        other => Err(ExecError::TypeMismatch {
            expected: "NUMBER",
            got: other.type_name(),
        }),
    }
}

fn number_err(e: bicdb_types::NumberError) -> ExecError {
    match e {
        bicdb_types::NumberError::DivisionByZero => ExecError::DivisionByZero,
        _ => ExecError::NumericOverflow,
    }
}

fn count_number(c: u64) -> Number {
    Number::parse(&c.to_string()).expect("计数可解析")
}

/// 组键（值向量——`Value` 可哈希，`NULL` 在分组里视为**同一组**）。
type GroupKey = Vec<Value>;

/// 求一行的分组键（无 `GROUP BY` ⇒ 空键 = 单组）。
fn group_key(groups: &[Expr], row: &Row, params: &[Value]) -> Result<GroupKey, ExecError> {
    groups.iter().map(|e| expr::eval(e, row, params)).collect()
}

/// **`ScalarAgg`**（无 `GROUP BY`）：单组流式折叠；**空输入恒出一行**。
pub struct ScalarAgg<'a> {
    input: Box<dyn Operator + 'a>,
    aggs: Vec<AggAcc>,
    done: bool,
    slot: usize,
    opened: bool,
}

impl<'a> ScalarAgg<'a> {
    /// 构造。
    #[must_use]
    pub fn new(input: Box<dyn Operator + 'a>, specs: Vec<AggSpec>) -> Self {
        Self {
            input,
            aggs: specs.into_iter().map(AggAcc::new).collect(),
            done: false,
            slot: 0,
            opened: false,
        }
    }
}

impl Operator for ScalarAgg<'_> {
    fn open(&mut self, cx: &mut ExecContext<'_>) -> Result<(), ExecError> {
        if !self.opened {
            self.slot = cx.register_op("ScalarAgg");
            self.opened = true;
        }
        self.input.open(cx)
    }

    fn next(&mut self, cx: &mut ExecContext<'_>) -> Result<Option<Row>, ExecError> {
        if self.done {
            return Ok(None);
        }
        // 阻塞段：耗尽输入。
        while let Some(row) = self.input.next(cx)? {
            for acc in &mut self.aggs {
                let v = match &acc.spec.arg {
                    Some(e) => Some(expr::eval(e, &row, cx.params())?),
                    None => None,
                };
                acc.advance(v.as_ref(), cx.params())?;
            }
        }
        self.done = true;
        let mut values = Vec::with_capacity(self.aggs.len());
        for acc in &self.aggs {
            values.push(acc.finalize()?);
        }
        cx.note_row(self.slot);
        Ok(Some(Row::new(values))) // 空输入也出一行（COUNT(*)=0 等）
    }

    fn rescan(&mut self, cx: &mut ExecContext<'_>) -> Result<(), ExecError> {
        self.done = false;
        self.aggs = self
            .aggs
            .iter()
            .map(|a| AggAcc::new(a.spec.clone()))
            .collect();
        self.input.rescan(cx)
    }

    fn close(&mut self, cx: &mut ExecContext<'_>) {
        self.input.close(cx);
    }
}

/// **`HashAgg`**（有 `GROUP BY`）：组表 + 逐组状态；输出为**键出现序**
/// （全内存路径；溢出路径 = 分区序 + 分区内出现序——无 `ORDER BY` 的 SQL
/// 本无顺序保证，设计 §4.2.1 ③）。
///
/// **溢出（切片 6b-2c）**：超额度且有溢出空间 ⇒ **改档重来**——清表、重读
/// 输入（聚合状态不可逆，不缝合）、按分组键哈希分区落 temp、逐分区读回聚合；
/// 分区仍超额度 ⇒ 换种子**二次重分区**（计 `multi-pass`），深度上限 3。
pub struct HashAgg<'a, 's, 'io> {
    input: Box<dyn Operator + 'a>,
    groups: Vec<Expr>,
    specs: Vec<AggSpec>,
    spill: Option<&'s crate::spill::SpillSpace<'io>>,
    area: Option<WorkArea>,
    declared: u64,
    bytes0: (u64, u64),
    table: HashMap<GroupKey, Vec<AggAcc>>,
    order: Vec<GroupKey>,
    out: Vec<Row>,
    at: usize,
    loaded: bool,
    /// 组表占用估计（新组 + `DISTINCT` 已见值集增量）。
    used: u64,
    /// 发生过二级及以上重分区（`multi-pass` 判定；设计 §4.2.1 ③）。
    multi_pass: bool,
    slot: usize,
    opened: bool,
}

impl<'a, 's, 'io> HashAgg<'a, 's, 'io> {
    /// 构造。
    #[must_use]
    pub fn new(input: Box<dyn Operator + 'a>, groups: Vec<Expr>, specs: Vec<AggSpec>) -> Self {
        Self {
            input,
            groups,
            specs,
            spill: None,
            area: None,
            declared: 0,
            bytes0: (0, 0),
            table: HashMap::new(),
            order: Vec::new(),
            out: Vec::new(),
            at: 0,
            loaded: false,
            used: 0,
            multi_pass: false,
            slot: 0,
            opened: false,
        }
    }

    /// 带溢出空间。
    #[must_use]
    pub fn with_spill(mut self, spill: &'s crate::spill::SpillSpace<'io>) -> Self {
        self.spill = Some(spill);
        self
    }

    /// 额度（池形态每次重读；否则固定预算）。
    fn budget(&self, cx: &ExecContext<'_>) -> Option<u64> {
        cx.budget_for(self.area.as_ref())
    }

    /// 改档（倍增才重申报；`benefit = ideal`——哈希族省下的是"写进 temp
    /// 再读回"的**一趟**流量，入口溢出时数据尚未写盘）。
    fn redeclare(&mut self, used: u64) {
        let Some(area) = &self.area else { return };
        if used > self.declared && (self.declared == 0 || used >= self.declared.saturating_mul(2)) {
            area.regrade(AreaClaim {
                ideal: used,
                one_pass: 0,
                benefit: used,
            });
            self.declared = used;
        }
    }

    /// **喂一行进组表**（含内存记账）。
    fn aggregate_row(&mut self, cx: &ExecContext<'_>, row: &Row) -> Result<(), ExecError> {
        let key = group_key(&self.groups, row, cx.params())?;
        let entry = match self.table.get_mut(&key) {
            Some(accs) => accs,
            None => {
                // 新组：键 + 每个聚合项的累加器开销。
                let key_bytes: usize =
                    key.iter().map(crate::value::value_bytes).sum::<usize>() + 32;
                self.used += (key_bytes + 24 * self.specs.len()) as u64;
                self.order.push(key.clone());
                self.table
                    .entry(key)
                    .or_insert_with(|| self.specs.iter().cloned().map(AggAcc::new).collect())
            }
        };
        for acc in entry.iter_mut() {
            let v = match &acc.spec.arg {
                Some(e) => Some(expr::eval(e, row, cx.params())?),
                None => None,
            };
            self.used += acc.advance_accounted(v.as_ref(), cx.params())?;
        }
        Ok(())
    }

    /// 组表收尾 → 结果行（按键出现序）。
    fn finish_table(&mut self) -> Result<Vec<Row>, ExecError> {
        let mut rows = Vec::with_capacity(self.order.len());
        for key in &self.order {
            let accs = self.table.get(key).expect("已登记");
            let mut values = key.clone();
            for acc in accs {
                values.push(acc.finalize()?);
            }
            rows.push(Row::new(values));
        }
        Ok(rows)
    }

    /// 收尾记账：三态计数 + extra bytes（spill 字节差）。
    fn note_done(&self, cx: &mut ExecContext<'_>, partitioned: bool) {
        let outcome = if self.multi_pass {
            WorkAreaOutcome::MultiPass
        } else if partitioned {
            WorkAreaOutcome::OnePass
        } else {
            WorkAreaOutcome::Optimal
        };
        cx.note_work_area(outcome);
        if let Some(space) = self.spill {
            cx.note_extra_bytes(
                space.bytes_written() - self.bytes0.0,
                space.bytes_read() - self.bytes0.1,
            );
        }
    }

    /// **改档重来**：清表 → 重读输入 → 一级分区落 temp → 逐分区读回聚合。
    fn run_partitioned(&mut self, cx: &mut ExecContext<'_>) -> Result<Vec<Row>, ExecError> {
        self.table.clear();
        self.order.clear();
        self.used = 0;
        self.input.rescan(cx)?;

        // 一级分区（分区数 = 无统计信息下的稳妥默认）。
        let budget = self.budget(cx).unwrap_or(u64::MAX);
        let parts = crate::part::PARTITIONS_LEVEL1;
        let space = self.spill.expect("分区路径必有溢出空间");
        let mut runs: Vec<Vec<usize>> = vec![Vec::new(); parts];
        let mut buf = crate::part::BucketBuffer::new(crate::part::seed_for(0), parts, budget);
        while let Some(row) = self.input.next(cx)? {
            cx.check()?;
            let key = group_key(&self.groups, &row, cx.params())?;
            buf.push(&key, row, space, &mut runs)?;
        }
        buf.flush_all(space, &mut runs)?;

        // 逐分区读回聚合（分区仍超额度 ⇒ 深一层重分区）。
        let mut out = Vec::new();
        for pruns in &runs {
            if pruns.is_empty() {
                continue; // 空分区不产组
            }
            self.aggregate_partition(cx, pruns, 0, &mut out)?;
        }
        Ok(out)
    }

    /// **聚合一个分区**（流式读回；超额度 ⇒ 换种子重分区，深度 ≤ 3）。
    fn aggregate_partition(
        &mut self,
        cx: &mut ExecContext<'_>,
        runs: &[usize],
        depth: u32,
        out: &mut Vec<Row>,
    ) -> Result<(), ExecError> {
        let space = self.spill.expect("分区路径必有溢出空间");
        self.table.clear();
        self.order.clear();
        self.used = 0;

        let mut stream = space.open_runs(runs)?;
        let mut overflow_at: Option<u64> = None;
        while let Some(row) = stream.next_row()? {
            cx.check()?;
            self.aggregate_row(cx, &row)?;
            self.redeclare(self.used);
            if let Some(budget) = self.budget(cx) {
                if self.used > budget {
                    overflow_at = Some(self.used);
                    break;
                }
            }
        }
        if overflow_at.is_none() {
            // 分区装下了：收尾输出（分区内 = 键出现序）。
            let rows = self.finish_table()?;
            out.extend(rows);
            return Ok(());
        }

        // 分区仍超额度 ⇒ 深一层重分区（部分数据多写多读一遍 = multi-pass）。
        self.multi_pass = true;
        if depth >= crate::part::MAX_PARTITION_DEPTH {
            // 极端键偏斜：哈希怎么分都挤一桶——按当前额度继续（记 multi-pass）。
            self.table.clear();
            self.order.clear();
            self.used = 0;
            let mut stream = space.open_runs(runs)?;
            while let Some(row) = stream.next_row()? {
                cx.check()?;
                self.aggregate_row(cx, &row)?;
            }
            let rows = self.finish_table()?;
            out.extend(rows);
            return Ok(());
        }

        let budget = self.budget(cx).unwrap_or(u64::MAX);
        let parts = crate::part::parts_for(overflow_at.unwrap_or(budget), budget);
        let sub_seed = crate::part::seed_for(depth + 1);

        // 读回本分区 → 换种子重分区落 temp。
        let mut sub_runs: Vec<Vec<usize>> = vec![Vec::new(); parts];
        let mut buf = crate::part::BucketBuffer::new(sub_seed, parts, budget);
        let mut stream = space.open_runs(runs)?;
        while let Some(row) = stream.next_row()? {
            cx.check()?;
            let key = group_key(&self.groups, &row, cx.params())?;
            buf.push(&key, row, space, &mut sub_runs)?;
        }
        buf.flush_all(space, &mut sub_runs)?;

        for sruns in &sub_runs {
            if sruns.is_empty() {
                continue;
            }
            self.aggregate_partition(cx, sruns, depth + 1, out)?;
        }
        Ok(())
    }
}

impl Operator for HashAgg<'_, '_, '_> {
    fn open(&mut self, cx: &mut ExecContext<'_>) -> Result<(), ExecError> {
        if !self.opened {
            self.slot = cx.register_op("HashAgg");
            self.opened = true;
        }
        if self.spill.is_some() {
            self.area = cx.claim_area("HashAgg");
            if let Some(space) = self.spill {
                self.bytes0 = (space.bytes_written(), space.bytes_read());
            }
        }
        self.input.open(cx)
    }

    fn next(&mut self, cx: &mut ExecContext<'_>) -> Result<Option<Row>, ExecError> {
        if !self.loaded {
            // 阻塞段：耗尽输入；超额度 ⇒ 改档重来（有溢出空间时）。
            let mut partitioned = false;
            while let Some(row) = self.input.next(cx)? {
                self.aggregate_row(cx, &row)?;
                self.redeclare(self.used);
                if let Some(budget) = self.budget(cx) {
                    if self.used > budget {
                        if self.spill.is_none() {
                            return Err(ExecError::WorkMemoryExceeded {
                                used: self.used,
                                budget,
                            });
                        }
                        self.out = self.run_partitioned(cx)?;
                        partitioned = true;
                        break;
                    }
                }
            }
            if !partitioned {
                self.out = self.finish_table()?;
            }
            self.note_done(cx, partitioned);
            self.loaded = true;
        }
        match self.out.get(self.at) {
            None => Ok(None),
            Some(r) => {
                self.at += 1;
                cx.note_row(self.slot);
                Ok(Some(r.clone()))
            }
        }
    }

    fn rescan(&mut self, cx: &mut ExecContext<'_>) -> Result<(), ExecError> {
        self.table.clear();
        self.order.clear();
        self.out.clear();
        self.at = 0;
        self.loaded = false;
        self.used = 0;
        self.multi_pass = false;
        self.input.rescan(cx)
    }

    fn close(&mut self, cx: &mut ExecContext<'_>) {
        self.table.clear();
        self.out.clear();
        self.input.close(cx);
    }
}

/// **`SortedAgg`**（输入按分组键有序）：换组即输出上一组（流式首组）。
///
/// `DISTINCT` 变体要求输入**同时按去重键有序**（组内相邻去重）——由计划侧
/// 保证（设计 §2.4 的"有序形态"）；此处按"与上一行同值即跳过"实现。
pub struct SortedAgg<'a> {
    input: Box<dyn Operator + 'a>,
    groups: Vec<Expr>,
    specs: Vec<AggSpec>,
    accs: Option<Vec<AggAcc>>,
    current_key: Option<GroupKey>,
    last_distinct: Vec<Option<Value>>,
    exhausted: bool,
    slot: usize,
    opened: bool,
}

impl<'a> SortedAgg<'a> {
    /// 构造。
    #[must_use]
    pub fn new(input: Box<dyn Operator + 'a>, groups: Vec<Expr>, specs: Vec<AggSpec>) -> Self {
        let n = specs.len();
        Self {
            input,
            groups,
            specs,
            accs: None,
            current_key: None,
            last_distinct: vec![None; n],
            exhausted: false,
            slot: 0,
            opened: false,
        }
    }

    /// 新建一组的状态集。
    fn fresh_accs(&self) -> Vec<AggAcc> {
        self.specs.iter().cloned().map(AggAcc::new).collect()
    }

    /// 把一行喂进活动组（处理 `DISTINCT` 的组内相邻去重）。
    fn acc_row(&mut self, row: &Row, params: &[Value]) -> Result<(), ExecError> {
        let mut accs = self.accs.take().expect("活动组");
        for (i, acc) in accs.iter_mut().enumerate() {
            let v = match &acc.spec.arg {
                Some(e) => Some(expr::eval(e, row, params)?),
                None => None,
            };
            if acc.spec.distinct {
                if self.last_distinct[i] == v {
                    continue; // 相邻同值 ⇒ 跳过（输入按去重键有序）
                }
                self.last_distinct[i] = v.clone();
            }
            acc.advance(v.as_ref(), params)?;
        }
        self.accs = Some(accs);
        Ok(())
    }

    /// 收尾一组：键值 ++ 各聚合结果。
    fn finish_group(&self, key: GroupKey) -> Result<Row, ExecError> {
        let accs = self.accs.as_ref().expect("活动组");
        let mut values = key;
        for acc in accs {
            values.push(acc.finalize()?);
        }
        Ok(Row::new(values))
    }
}

impl Operator for SortedAgg<'_> {
    fn open(&mut self, cx: &mut ExecContext<'_>) -> Result<(), ExecError> {
        if !self.opened {
            self.slot = cx.register_op("SortedAgg");
            self.opened = true;
        }
        self.input.open(cx)
    }

    fn next(&mut self, cx: &mut ExecContext<'_>) -> Result<Option<Row>, ExecError> {
        loop {
            cx.check()?;
            if self.exhausted {
                return Ok(None);
            }
            let row = match self.input.next(cx)? {
                Some(r) => r,
                None => {
                    self.exhausted = true;
                    let Some(key) = self.current_key.take() else {
                        return Ok(None);
                    };
                    let out = self.finish_group(key)?;
                    cx.note_row(self.slot);
                    return Ok(Some(out));
                }
            };
            let key = group_key(&self.groups, &row, cx.params())?;
            if self.current_key.as_ref() != Some(&key) {
                // ① 收尾上一组（若有）——用**当前** accs；
                let finished = match self.current_key.take() {
                    Some(prev_key) => Some(self.finish_group(prev_key)?),
                    None => None,
                };
                // ② 开新组、本行立即入新组（不能丢）；
                self.current_key = Some(key);
                self.accs = Some(self.fresh_accs());
                self.last_distinct = vec![None; self.specs.len()];
                self.acc_row(&row, cx.params())?;
                // ③ 有上一组就返回它（本行已安全落进新组状态）。
                if let Some(out) = finished {
                    cx.note_row(self.slot);
                    return Ok(Some(out));
                }
            } else {
                self.acc_row(&row, cx.params())?;
            }
        }
    }

    fn close(&mut self, cx: &mut ExecContext<'_>) {
        self.input.close(cx);
    }
}

/// **直译路径的朴素聚合**（参考模型：线性查组、键出现序输出）。
///
/// 语义与 `HashAgg`/`ScalarAgg` **必须一致**（差分验收的口径）：
/// 无分组键 ⇒ 空输入恒出一行；有分组键 ⇒ 零行。
pub fn direct_aggregate(
    groups: &[Expr],
    specs: &[AggSpec],
    rows: &[Row],
    params: &[Value],
) -> Result<Vec<Row>, ExecError> {
    let mut out: Vec<(GroupKey, Vec<AggAcc>)> = Vec::new();
    for row in rows {
        let key = group_key(groups, row, params)?;
        let idx = match out.iter().position(|(k, _)| *k == key) {
            Some(i) => i,
            None => {
                out.push((key, specs.iter().cloned().map(AggAcc::new).collect()));
                out.len() - 1
            }
        };
        for acc in out[idx].1.iter_mut() {
            let v = match &acc.spec.arg {
                Some(e) => Some(expr::eval(e, row, params)?),
                None => None,
            };
            acc.advance(v.as_ref(), params)?;
        }
    }
    if groups.is_empty() {
        // 单组：无输入也要出一行。
        let accs = match out.into_iter().next() {
            Some((_, accs)) => accs,
            None => specs.iter().cloned().map(AggAcc::new).collect(),
        };
        let mut values = Vec::with_capacity(accs.len());
        for acc in &accs {
            values.push(acc.finalize()?);
        }
        return Ok(vec![Row::new(values)]);
    }
    let mut result = Vec::with_capacity(out.len());
    for (key, accs) in out {
        let mut values = key;
        for acc in &accs {
            values.push(acc.finalize()?);
        }
        result.push(Row::new(values));
    }
    Ok(result)
}

/// 排序键：分组键列（供 `SortedAgg` 的计划侧使用）。
#[must_use]
pub fn group_sort_keys(groups: &[Expr]) -> Vec<SortKey> {
    groups
        .iter()
        .map(|e| SortKey {
            expr: e.clone(),
            desc: false,
        })
        .collect()
}
