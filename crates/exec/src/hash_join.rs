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
//! **切片边界（6a）**：构建侧**全内存**——超工作内存预算报具名
//! [`ExecError::WorkMemoryExceeded`]；**分批溢出（构建侧落 temp、逐批重读）
//! 随切片 6b 的 temp 接入**（设计 §4.2：SORT 与 HASH 共用同一套 temp 段）。

use std::collections::{HashMap, VecDeque};

use crate::context::ExecContext;
use crate::error::ExecError;
use crate::expr::{self, Expr};
use crate::join::JoinKind;
use crate::operator::Operator;
use crate::value::{row_bytes, Row, Value};

/// 键 → 构建行列表（保留插入序 ⇒ 同键多行输出稳定）。
type BuildTable = HashMap<Vec<Value>, Vec<Row>>;

/// **哈希连接**（INNER / LEFT）。
pub struct HashJoin<'a> {
    build: Box<dyn Operator + 'a>,
    probe: Box<dyn Operator + 'a>,
    build_keys: Vec<Expr>,
    probe_keys: Vec<Expr>,
    kind: JoinKind,
    /// 连接条件（组合行 = 探测行 ++ 构建行 上求值；`None` = 键等值即匹配）。
    qual: Option<Expr>,
    /// 构建侧列数（LEFT 补 NULL 用）。
    build_width: usize,
    table: BuildTable,
    order: Vec<Vec<Value>>,
    built: bool,
    probe_row: Option<Row>,
    /// 当前探测行已产出的匹配数（LEFT 判定用）。
    matched: bool,
    out: VecDeque<Row>,
    used: u64,
    slot: usize,
    opened: bool,
}

impl<'a> HashJoin<'a> {
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
            table: HashMap::new(),
            order: Vec::new(),
            built: false,
            probe_row: None,
            matched: false,
            out: VecDeque::new(),
            used: 0,
            slot: 0,
            opened: false,
        }
    }

    /// 构建阶段：耗尽构建侧。
    fn build_phase(&mut self, cx: &mut ExecContext<'_>) -> Result<(), ExecError> {
        while let Some(row) = self.build.next(cx)? {
            let keys: Vec<Value> = self
                .build_keys
                .iter()
                .map(|e| expr::eval(e, &row, cx.params()))
                .collect::<Result<_, _>>()?;
            self.used += (row_bytes(&row) + 32) as u64;
            if let Some(budget) = cx.work_memory_budget() {
                if self.used > budget {
                    return Err(ExecError::WorkMemoryExceeded {
                        used: self.used,
                        budget,
                    });
                }
            }
            match self.table.get_mut(&keys) {
                Some(rows) => rows.push(row),
                None => {
                    self.order.push(keys.clone());
                    self.table.insert(keys, vec![row]);
                }
            }
        }
        self.built = true;
        Ok(())
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
        let keys: Vec<Value> = self
            .probe_keys
            .iter()
            .map(|e| expr::eval(e, &probe, cx.params()))
            .collect::<Result<_, _>>()?;
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

impl Operator for HashJoin<'_> {
    fn open(&mut self, cx: &mut ExecContext<'_>) -> Result<(), ExecError> {
        if !self.opened {
            self.slot = cx.register_op("HashJoin");
            self.opened = true;
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
            if !self.built {
                self.build_phase(cx)?;
            }
            // 当前探测行还没探查完 ⇒ 先探查。
            if self.probe_row.is_none() {
                self.probe_row = self.probe.next(cx)?;
                if self.probe_row.is_none() {
                    return Ok(None);
                }
            }
            self.probe_one(cx)?;
            self.probe_row = None; // 本行已探查（结果都在 out 里）
        }
    }

    fn rescan(&mut self, cx: &mut ExecContext<'_>) -> Result<(), ExecError> {
        self.out.clear();
        self.probe_row = None;
        self.built = false;
        self.table.clear();
        self.order.clear();
        self.used = 0;
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
