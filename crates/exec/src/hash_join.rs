//! **哈希连接**（切片 6a；设计 §2.2）：构建侧建表 + 探测侧流式。
//!
//! ```text
//! 构建侧（小表）：耗尽 → 哈希表（键 → 行列表）
//! 探测侧（大表）：流式逐行 → 探查 → 命中逐行输出；未命中（LEFT）补 NULL
//! ```
//!
//! **组合行约定**：与 `NestedLoop` 不同——哈希连接的组合行为
//! **探测行 ++ 构建行**（探测侧在前）；`qual` 与投影按此编号。
//! （`NestedLoop` 是外层 ++ 内层；两者由 Binder 按各自形态编号。）
//!
//! **溢出（切片 6b-2d；设计 §4.2.1 ③）**：构建侧溢出发生在**任何输出之前**
//! （构建期阻塞）⇒ 与 `HashAgg` 同式**改档重来**：构建侧重读、**双侧**按
//! 连接键同种子哈希分区落 temp，逐分区"装载构建侧 → 流式探查探测侧"。
//! 某分区构建侧仍装不下 ⇒ **配对重分区**（双侧同种子换层，深度 ≤ 3；
//! 任何二级及以上重分区计 `multi-pass`）。**匹配不跨分区**（同键必同分区）
//! ⇒ INNER/LEFT 语义不变；输出序 = 分区序 + 分区内原序（无 `ORDER BY`
//! 的 SQL 本无顺序保证——差分验收按多重集）。

use std::collections::{HashMap, VecDeque};

use crate::context::{ExecContext, WorkAreaOutcome};
use crate::error::ExecError;
use crate::expr::{self, Expr};
use crate::join::JoinKind;
use crate::operator::Operator;
use crate::spill::{RunStream, SpillSpace};
use crate::value::{row_bytes, Row, Value};
use crate::wmm::{AreaClaim, WorkArea};

/// 键 → 构建行列表（保留插入序 ⇒ 同键多行输出稳定）。
type BuildTable = HashMap<Vec<Value>, Vec<Row>>;

/// 一个**分区对**（构建侧 run 列表 + 探测侧 run 列表，同一层同一分区号）。
struct PartPair {
    build: Vec<usize>,
    probe: Vec<usize>,
    /// 本对的层号（0 = 一级分区；≥1 = 重分区而来）。
    depth: u32,
}

/// 执行相位。
enum Phase {
    /// 未构建。
    Start,
    /// 构建表在全内存（切片 6a 形态）。
    InMemory,
    /// 逐分区处理（溢出形态；`out` 之外不缓冲结果）。
    Partitions,
    /// 已结束（三态记账一次）。
    Done,
}

/// **哈希连接**（INNER / LEFT）。
pub struct HashJoin<'a, 's, 'io> {
    build: Box<dyn Operator + 'a>,
    probe: Box<dyn Operator + 'a>,
    build_keys: Vec<Expr>,
    probe_keys: Vec<Expr>,
    kind: JoinKind,
    /// 连接条件（组合行 = 探测行 ++ 构建行 上求值；`None` = 键等值即匹配）。
    qual: Option<Expr>,
    /// 构建侧列数（LEFT 补 NULL 用）。
    build_width: usize,
    spill: Option<&'s SpillSpace<'io>>,
    area: Option<WorkArea>,
    declared: u64,
    bytes0: (u64, u64),
    table: BuildTable,
    order: Vec<Vec<Value>>,
    phase: Phase,
    /// 分区对（逐分区处理；重分区结果**就地替换**本单位）。
    pairs: Vec<PartPair>,
    at: usize,
    /// 当前分区的探测侧读回流。
    stream: Option<RunStream<'s, 'io>>,
    probe_row: Option<Row>,
    /// 当前探测行已产出的匹配数（LEFT 判定用）。
    matched: bool,
    out: VecDeque<Row>,
    used: u64,
    multi_pass: bool,
    slot: usize,
    opened: bool,
}

impl<'a, 's, 'io> HashJoin<'a, 's, 'io> {
    /// 构造。
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        build: Box<dyn Operator + 'a>,
        probe: Box<dyn Operator + 'a>,
        build_keys: Vec<Expr>,
        probe_keys: Vec<Expr>,
        kind: JoinKind,
        qual: Option<Expr>,
        build_width: usize,
    ) -> Self {
        Self {
            build,
            probe,
            build_keys,
            probe_keys,
            kind,
            qual,
            build_width,
            spill: None,
            area: None,
            declared: 0,
            bytes0: (0, 0),
            table: HashMap::new(),
            order: Vec::new(),
            phase: Phase::Start,
            pairs: Vec::new(),
            at: 0,
            stream: None,
            probe_row: None,
            matched: false,
            out: VecDeque::new(),
            used: 0,
            multi_pass: false,
            slot: 0,
            opened: false,
        }
    }

    /// 带溢出空间（构建侧超额度 ⇒ 双侧分区）。
    #[must_use]
    pub fn with_spill(mut self, spill: &'s SpillSpace<'io>) -> Self {
        self.spill = Some(spill);
        self
    }

    /// 额度（池形态每次重读；否则固定预算）。
    fn budget(&self, cx: &ExecContext<'_>) -> Option<u64> {
        cx.budget_for(self.area.as_ref())
    }

    /// 改档（倍增才重申报；`benefit = ideal`——省下一趟 temp 流量）。
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

    /// 一行的构建键。
    fn build_key_of(&self, row: &Row, params: &[Value]) -> Result<Vec<Value>, ExecError> {
        self.build_keys
            .iter()
            .map(|e| expr::eval(e, row, params))
            .collect()
    }

    /// 一行的探测键。
    fn probe_key_of(&self, row: &Row, params: &[Value]) -> Result<Vec<Value>, ExecError> {
        self.probe_keys
            .iter()
            .map(|e| expr::eval(e, row, params))
            .collect()
    }

    /// 把一行放进构建表（键已求好）。
    fn insert_build(&mut self, keys: Vec<Value>, row: Row) {
        match self.table.get_mut(&keys) {
            Some(rows) => rows.push(row),
            None => {
                self.order.push(keys.clone());
                self.table.insert(keys, vec![row]);
            }
        }
    }

    /// **构建阶段**（全内存形态）：耗尽构建侧；超额度 ⇒ 报错或改档重来。
    fn build_phase(&mut self, cx: &mut ExecContext<'_>) -> Result<(), ExecError> {
        while let Some(row) = self.build.next(cx)? {
            let keys = self.build_key_of(&row, cx.params())?;
            self.used += row_bytes(&row) as u64 + 32;
            self.redeclare(self.used);
            if let Some(budget) = self.budget(cx) {
                if self.used > budget {
                    if self.spill.is_none() {
                        return Err(ExecError::WorkMemoryExceeded {
                            used: self.used,
                            budget,
                        });
                    }
                    self.run_partitioned(cx)?;
                    return Ok(());
                }
            }
            self.insert_build(keys, row);
        }
        self.phase = Phase::InMemory;
        Ok(())
    }

    /// **改档重来**：构建侧重读 → 双侧分区落 temp → 装配分区对。
    fn run_partitioned(&mut self, cx: &mut ExecContext<'_>) -> Result<(), ExecError> {
        self.table.clear();
        self.order.clear();
        self.used = 0;
        self.build.rescan(cx)?;

        let space = self.spill.expect("分区路径必有溢出空间");
        let budget = self.budget(cx).unwrap_or(u64::MAX);
        let parts = crate::part::PARTITIONS_LEVEL1;
        let seed = crate::part::seed_for(0);
        let mut build_runs: Vec<Vec<usize>> = vec![Vec::new(); parts];
        let mut probe_runs: Vec<Vec<usize>> = vec![Vec::new(); parts];

        // 构建侧：重读 + 路由。
        let mut buf = crate::part::BucketBuffer::new(seed, parts, budget);
        while let Some(row) = self.build.next(cx)? {
            cx.check()?;
            let keys = self.build_key_of(&row, cx.params())?;
            buf.push(&keys, row, space, &mut build_runs)?;
        }
        buf.flush_all(space, &mut build_runs)?;

        // 探测侧：耗尽 + 路由（构建期阻塞 ⇒ 此时尚无任何输出）。
        let mut buf = crate::part::BucketBuffer::new(seed, parts, budget);
        while let Some(row) = self.probe.next(cx)? {
            cx.check()?;
            let keys = self.probe_key_of(&row, cx.params())?;
            buf.push(&keys, row, space, &mut probe_runs)?;
        }
        buf.flush_all(space, &mut probe_runs)?;

        // 装配：探测侧为空的分区无输出（INNER/LEFT 都不产）；构建侧为空而
        // 探测侧有行的分区保留（LEFT 要补 NULL）。
        self.pairs = Vec::new();
        for i in 0..parts {
            if probe_runs[i].is_empty() {
                continue;
            }
            self.pairs.push(PartPair {
                build: std::mem::take(&mut build_runs[i]),
                probe: std::mem::take(&mut probe_runs[i]),
                depth: 0,
            });
        }
        self.at = 0;
        self.phase = Phase::Partitions;
        Ok(())
    }

    /// **装载一个分区的构建侧**（流式读回；装不下 ⇒ 配对重分区）。
    ///
    /// 返回 `true` = 本分区对**已被子对就地替换**（调用方**不得**推进游标、
    /// 也不得开探测流——下一个循环会按新的 `pairs[idx]` 重新装载）。
    fn load_partition(&mut self, cx: &mut ExecContext<'_>, idx: usize) -> Result<bool, ExecError> {
        self.table.clear();
        self.order.clear();
        self.used = 0;
        let space = self.spill.expect("分区路径必有溢出空间");
        let (build_runs, depth) = {
            let p = &self.pairs[idx];
            (p.build.clone(), p.depth)
        };
        let mut stream = space.open_runs(&build_runs)?;
        let mut overflow = false;
        while let Some(row) = stream.next_row()? {
            cx.check()?;
            let keys = self.build_key_of(&row, cx.params())?;
            self.used += row_bytes(&row) as u64 + 32;
            self.redeclare(self.used);
            self.insert_build(keys, row);
            if let Some(budget) = self.budget(cx) {
                if self.used > budget {
                    overflow = true;
                    break;
                }
            }
        }
        if !overflow {
            return Ok(false);
        }

        // 装不下：极端偏斜（深度上限）⇒ 按当前额度继续，记 multi-pass。
        self.multi_pass = true;
        if depth >= crate::part::MAX_PARTITION_DEPTH {
            while let Some(row) = stream.next_row()? {
                cx.check()?;
                let keys = self.build_key_of(&row, cx.params())?;
                self.used += row_bytes(&row) as u64 + 32;
                self.insert_build(keys, row);
            }
            return Ok(false);
        }

        // 配对重分区：双侧同种子换层，**就地替换**本分区对。
        let budget = self.budget(cx).unwrap_or(u64::MAX);
        let parts = crate::part::parts_for(self.used, budget);
        let seed = crate::part::seed_for(depth + 1);
        let probe_runs = self.pairs[idx].probe.clone();
        let sub_build = self.route_runs(cx, &build_runs, true, seed, parts, budget)?;
        let sub_probe = self.route_runs(cx, &probe_runs, false, seed, parts, budget)?;

        let subs: Vec<PartPair> = sub_build
            .into_iter()
            .zip(sub_probe)
            .filter(|(_, probe)| !probe.is_empty())
            .map(|(build, probe)| PartPair {
                build,
                probe,
                depth: depth + 1,
            })
            .collect();
        self.pairs.splice(idx..=idx, subs);
        // 本分区对已替换 ⇒ 交回调用方**原地重装载**（`pairs[idx]` 已是第一个子对）。
        self.table.clear();
        self.order.clear();
        self.used = 0;
        Ok(true)
    }

    /// 把一个 run 列表按 `seed` 重分区（`is_build` 选键表达式）。
    fn route_runs(
        &mut self,
        cx: &mut ExecContext<'_>,
        runs: &[usize],
        is_build: bool,
        seed: u64,
        parts: usize,
        budget: u64,
    ) -> Result<Vec<Vec<usize>>, ExecError> {
        let space = self.spill.expect("分区路径必有溢出空间");
        let mut out: Vec<Vec<usize>> = vec![Vec::new(); parts];
        let mut buf = crate::part::BucketBuffer::new(seed, parts, budget);
        let mut stream = space.open_runs(runs)?;
        while let Some(row) = stream.next_row()? {
            cx.check()?;
            let keys = if is_build {
                self.build_key_of(&row, cx.params())?
            } else {
                self.probe_key_of(&row, cx.params())?
            };
            buf.push(&keys, row, space, &mut out)?;
        }
        buf.flush_all(space, &mut out)?;
        Ok(out)
    }

    /// 收尾记账：三态计数 + extra bytes。
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

    /// 组合行 = 探测行 ++ 构建行。
    fn combine(probe: &Row, build: &Row) -> Row {
        let mut values = Vec::with_capacity(probe.values.len() + build.values.len());
        values.extend(probe.values.iter().cloned());
        values.extend(build.values.iter().cloned());
        Row::new(values)
    }

    /// 探查一行：把匹配对（或 LEFT 补 NULL）压进 `out`。
    fn probe_one(&mut self, cx: &mut ExecContext<'_>) -> Result<(), ExecError> {
        let Some(probe) = self.probe_row.clone() else {
            return Ok(());
        };
        let keys = self.probe_key_of(&probe, cx.params())?;
        let mut produced = 0u64;
        if let Some(build_rows) = self.table.get(&keys) {
            for build_row in build_rows {
                let combined = Self::combine(&probe, build_row);
                if let Some(qual) = &self.qual {
                    if !expr::eval_where(qual, &combined, cx.params())? {
                        continue;
                    }
                }
                self.out.push_back(combined);
                produced += 1;
            }
        }
        self.matched = produced > 0;
        if !self.matched && self.kind == JoinKind::Left {
            let mut values = probe.values.clone();
            values.extend(std::iter::repeat(Value::Null).take(self.build_width));
            self.out.push_back(Row::new(values));
        }
        Ok(())
    }
}

impl Operator for HashJoin<'_, '_, '_> {
    fn open(&mut self, cx: &mut ExecContext<'_>) -> Result<(), ExecError> {
        if !self.opened {
            self.slot = cx.register_op("HashJoin");
            self.opened = true;
        }
        if self.spill.is_some() {
            self.area = cx.claim_area("HashJoin");
            if let Some(space) = self.spill {
                self.bytes0 = (space.bytes_written(), space.bytes_read());
            }
        }
        self.build.open(cx)?;
        self.probe.open(cx)
    }

    fn next(&mut self, cx: &mut ExecContext<'_>) -> Result<Option<Row>, ExecError> {
        loop {
            cx.check()?;
            if let Some(row) = self.out.pop_front() {
                cx.note_row(self.slot);
                return Ok(Some(row));
            }
            match self.phase {
                Phase::Start => {
                    self.build_phase(cx)?;
                    match self.phase {
                        Phase::InMemory => self.note_done(cx, false),
                        Phase::Partitions => {} // 三态在结束处记（本相位才开始）
                        _ => {}
                    }
                }
                Phase::InMemory => {
                    let Some(row) = self.probe.next(cx)? else {
                        self.phase = Phase::Done;
                        return Ok(None);
                    };
                    self.probe_row = Some(row);
                    self.probe_one(cx)?;
                    self.probe_row = None;
                }
                Phase::Partitions => {
                    if self.stream.is_none() {
                        if self.at >= self.pairs.len() {
                            self.note_done(cx, true);
                            self.phase = Phase::Done;
                            return Ok(None);
                        }
                        if self.load_partition(cx, self.at)? {
                            // 已被子对替换 ⇒ 原地重来（先装载，再开探测流）。
                            continue;
                        }
                        let probe_runs = self.pairs[self.at].probe.clone();
                        self.at += 1;
                        let space = self.spill.expect("分区路径必有溢出空间");
                        self.stream = Some(space.open_runs(&probe_runs)?);
                        continue;
                    }
                    let next = {
                        let s = self.stream.as_mut().expect("上面刚建");
                        s.next_row()?
                    };
                    match next {
                        None => {
                            self.stream = None;
                            self.table.clear();
                            self.order.clear();
                        }
                        Some(row) => {
                            self.probe_row = Some(row);
                            self.probe_one(cx)?; // 结果进 out，下轮取
                            self.probe_row = None;
                        }
                    }
                }
                Phase::Done => return Ok(None),
            }
        }
    }

    fn rescan(&mut self, cx: &mut ExecContext<'_>) -> Result<(), ExecError> {
        self.out.clear();
        self.probe_row = None;
        self.phase = Phase::Start;
        self.table.clear();
        self.order.clear();
        self.used = 0;
        self.multi_pass = false;
        self.pairs.clear();
        self.at = 0;
        self.stream = None;
        self.build.rescan(cx)?;
        self.probe.rescan(cx)
    }

    fn close(&mut self, cx: &mut ExecContext<'_>) {
        self.table.clear();
        self.out.clear();
        self.build.close(cx);
        self.probe.close(cx);
    }
}
