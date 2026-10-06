//! **去重与集合运算族**（切片 5；设计 §2.7 的**排序归并路线**）。
//!
//! ```text
//! Append（UNION ALL）：按序拉各输入——流水，无阻塞
//! Unique（相邻去重）：输入按去重键有序 ⇒ 流式（SELECT DISTINCT / UNION 去重）
//! SetOp（INTERSECT [ALL] / EXCEPT [ALL]）：两侧**已排序** ⇒ 归并计数
//! ```
//!
//! **两条实现要点（设计 §2.7 写死）**：
//! 1. **集合去重是等价类语义**——`NULL` 在集合运算里彼此相等（PG 证据
//!    `622847`）⇒ 比较用 [`crate::sort::compare_keys`]（`NULL` 与 `NULL` 判等、
//!    总序一致），**不是** `WHERE` 的三值比较；两侧必须按**同一总序**排序，
//!    否则"相等即相邻"不成立。
//! 2. `ALL` 变体按**重数**（多重集）：`INTERSECT ALL` 出 `min(c_l, c_r)` 份、
//!    `EXCEPT ALL` 出 `max(0, c_l − c_r)` 份。

use std::collections::VecDeque;

use crate::context::ExecContext;
use crate::error::ExecError;
use crate::expr::Expr;
use crate::operator::Operator;
use crate::sort::{compare_keys, SortKey};
use crate::value::{Row, Value};

/// **`Append`**（`UNION ALL`）：按序拉各子输入；**流水**。
pub struct Append<'a> {
    inputs: Vec<Box<dyn Operator + 'a>>,
    current: usize,
    slot: usize,
    opened: bool,
}

impl<'a> Append<'a> {
    /// 构造（输入顺序 = 输出顺序）。
    #[must_use]
    pub fn new(inputs: Vec<Box<dyn Operator + 'a>>) -> Self {
        Self {
            inputs,
            current: 0,
            slot: 0,
            opened: false,
        }
    }
}

impl Operator for Append<'_> {
    fn open(&mut self, cx: &mut ExecContext<'_>) -> Result<(), ExecError> {
        if !self.opened {
            self.slot = cx.register_op("Append");
            self.opened = true;
        }
        for input in &mut self.inputs {
            input.open(cx)?;
        }
        Ok(())
    }

    fn next(&mut self, cx: &mut ExecContext<'_>) -> Result<Option<Row>, ExecError> {
        while self.current < self.inputs.len() {
            cx.check()?;
            match self.inputs[self.current].next(cx)? {
                Some(row) => {
                    cx.note_row(self.slot);
                    return Ok(Some(row));
                }
                None => self.current += 1,
            }
        }
        Ok(None)
    }

    fn close(&mut self, cx: &mut ExecContext<'_>) {
        for input in &mut self.inputs {
            input.close(cx);
        }
    }
}

/// **`Unique`**（相邻去重）：输入按 `keys` 有序 ⇒ 与上一行等价者跳过。
pub struct Unique<'a> {
    input: Box<dyn Operator + 'a>,
    keys: Vec<SortKey>,
    last: Option<Vec<Value>>,
    slot: usize,
    opened: bool,
}

impl<'a> Unique<'a> {
    /// 构造（`keys` = 去重键；`SELECT DISTINCT` 用全部输出列）。
    #[must_use]
    pub fn new(input: Box<dyn Operator + 'a>, keys: Vec<SortKey>) -> Self {
        Self {
            input,
            keys,
            last: None,
            slot: 0,
            opened: false,
        }
    }
}

impl Operator for Unique<'_> {
    fn open(&mut self, cx: &mut ExecContext<'_>) -> Result<(), ExecError> {
        if !self.opened {
            self.slot = cx.register_op("Unique");
            self.opened = true;
        }
        self.input.open(cx)
    }

    fn next(&mut self, cx: &mut ExecContext<'_>) -> Result<Option<Row>, ExecError> {
        loop {
            cx.check()?;
            let Some(row) = self.input.next(cx)? else {
                return Ok(None);
            };
            let keys: Vec<Value> = self
                .keys
                .iter()
                .map(|k| crate::expr::eval(&k.expr, &row, cx.params()))
                .collect::<Result<_, _>>()?;
            let dup = match &self.last {
                Some(prev) => compare_keys(&self.keys, prev, &keys)? == std::cmp::Ordering::Equal,
                None => false,
            };
            if dup {
                continue; // 相邻同键 ⇒ 跳过
            }
            self.last = Some(keys);
            cx.note_row(self.slot);
            return Ok(Some(row));
        }
    }

    fn close(&mut self, cx: &mut ExecContext<'_>) {
        self.input.close(cx);
    }
}

/// 集合运算种类（`UNION` 族由 `Append` + `Unique` 组合，不出现在这里）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOpKind {
    /// `INTERSECT [ALL]`。
    Intersect,
    /// `EXCEPT [ALL]`。
    Except,
}

/// **`SetOp`**（`INTERSECT [ALL]` / `EXCEPT [ALL]`）：两侧**已排序**、归并计数。
pub struct SetOp<'a> {
    left: Box<dyn Operator + 'a>,
    right: Box<dyn Operator + 'a>,
    kind: SetOpKind,
    all: bool,
    /// 比较键（全列升序——两侧必须按同一总序排序）。
    keys: Vec<SortKey>,
    left_head: Option<Row>,
    right_head: Option<Row>,
    out: VecDeque<Row>,
    slot: usize,
    opened: bool,
}

impl<'a> SetOp<'a> {
    /// 构造（`keys` 必须与两侧 `Sort` 用的键一致——同一个总序）。
    #[must_use]
    pub fn new(
        left: Box<dyn Operator + 'a>,
        right: Box<dyn Operator + 'a>,
        kind: SetOpKind,
        all: bool,
        keys: Vec<SortKey>,
    ) -> Self {
        Self {
            left,
            right,
            kind,
            all,
            keys,
            left_head: None,
            right_head: None,
            out: VecDeque::new(),
            slot: 0,
            opened: false,
        }
    }

    fn key_of(&self, row: &Row, params: &[Value]) -> Result<Vec<Value>, ExecError> {
        self.keys
            .iter()
            .map(|k| crate::expr::eval(&k.expr, row, params))
            .collect()
    }

    fn fill_left(&mut self, cx: &mut ExecContext<'_>) -> Result<(), ExecError> {
        if self.left_head.is_none() {
            self.left_head = self.left.next(cx)?;
        }
        Ok(())
    }

    fn fill_right(&mut self, cx: &mut ExecContext<'_>) -> Result<(), ExecError> {
        if self.right_head.is_none() {
            self.right_head = self.right.next(cx)?;
        }
        Ok(())
    }

    /// 取出左侧当前**整组**（与头部同键的连续行）；返回（重数, 样本行）。
    fn take_left_group(
        &mut self,
        cx: &mut ExecContext<'_>,
    ) -> Result<(u64, Option<Row>), ExecError> {
        let Some(head) = self.left_head.take() else {
            return Ok((0, None));
        };
        let head_keys = self.key_of(&head, cx.params())?;
        let mut count = 1u64;
        loop {
            self.left_head = self.left.next(cx)?;
            let Some(next) = &self.left_head else { break };
            let keys = self.key_of(next, cx.params())?;
            if compare_keys(&self.keys, &head_keys, &keys)? == std::cmp::Ordering::Equal {
                count += 1;
                self.left_head = None; // 已计入本组
            } else {
                break; // 下一组，留在头部
            }
        }
        Ok((count, Some(head)))
    }

    /// 取出右侧当前整组；返回重数。
    fn take_right_group(&mut self, cx: &mut ExecContext<'_>) -> Result<u64, ExecError> {
        let Some(head) = self.right_head.take() else {
            return Ok(0);
        };
        let head_keys = self.key_of(&head, cx.params())?;
        let mut count = 1u64;
        loop {
            self.right_head = self.right.next(cx)?;
            let Some(next) = &self.right_head else { break };
            let keys = self.key_of(next, cx.params())?;
            if compare_keys(&self.keys, &head_keys, &keys)? == std::cmp::Ordering::Equal {
                count += 1;
                self.right_head = None;
            } else {
                break;
            }
        }
        Ok(count)
    }

    fn emit(&mut self, sample: &Row, n: u64, cx: &mut ExecContext<'_>) {
        for _ in 0..n {
            self.out.push_back(sample.clone());
        }
        let _ = cx;
    }
}

impl Operator for SetOp<'_> {
    fn open(&mut self, cx: &mut ExecContext<'_>) -> Result<(), ExecError> {
        if !self.opened {
            self.slot = cx.register_op("SetOp");
            self.opened = true;
        }
        self.left.open(cx)?;
        self.right.open(cx)
    }

    fn next(&mut self, cx: &mut ExecContext<'_>) -> Result<Option<Row>, ExecError> {
        loop {
            cx.check()?;
            if let Some(row) = self.out.pop_front() {
                cx.note_row(self.slot);
                return Ok(Some(row));
            }
            self.fill_left(cx)?;
            self.fill_right(cx)?;
            let (has_l, has_r) = (self.left_head.is_some(), self.right_head.is_some());
            match (has_l, has_r) {
                (false, false) => return Ok(None),
                (true, false) => {
                    // 右侧尽：EXCEPT 输出左侧剩余组（去重 = 每组一次；ALL = 按重数）。
                    let (count, sample) = self.take_left_group(cx)?;
                    let Some(sample) = sample else { continue };
                    match self.kind {
                        SetOpKind::Except => {
                            let n = if self.all { count } else { 1 };
                            self.emit(&sample, n, cx);
                        }
                        SetOpKind::Intersect => {} // 无右侧对应 ⇒ 不出
                    }
                }
                (false, true) => {
                    self.take_right_group(cx)?; // 左侧尽：右侧剩余组无贡献
                }
                (true, true) => {
                    let lk = self.key_of(self.left_head.as_ref().expect("有头"), cx.params())?;
                    let rk = self.key_of(self.right_head.as_ref().expect("有头"), cx.params())?;
                    match compare_keys(&self.keys, &lk, &rk)? {
                        std::cmp::Ordering::Equal => {
                            let (cl, sample) = self.take_left_group(cx)?;
                            let cr = self.take_right_group(cx)?;
                            let sample = sample.expect("同键左侧非空");
                            match self.kind {
                                SetOpKind::Intersect => {
                                    let n = if self.all { cl.min(cr) } else { 1 };
                                    self.emit(&sample, n, cx);
                                }
                                SetOpKind::Except => {
                                    if cl > cr {
                                        let n = if self.all { cl - cr } else { 1 };
                                        self.emit(&sample, n, cx);
                                    }
                                }
                            }
                        }
                        std::cmp::Ordering::Less => {
                            // 左侧组比右侧头小 ⇒ 右侧不会有它。
                            let (cl, sample) = self.take_left_group(cx)?;
                            if let (SetOpKind::Except, Some(sample)) = (self.kind, sample) {
                                let n = if self.all { cl } else { 1 };
                                self.emit(&sample, n, cx);
                            }
                        }
                        std::cmp::Ordering::Greater => {
                            self.take_right_group(cx)?; // 右侧头小 ⇒ 无左侧对应
                        }
                    }
                }
            }
        }
    }

    fn close(&mut self, cx: &mut ExecContext<'_>) {
        self.left.close(cx);
        self.right.close(cx);
    }
}

/// 供计划侧：全列升序比较键（两侧 `Sort` 与 `SetOp` 必须同用）。
#[must_use]
pub fn all_columns_keys(width: usize) -> Vec<SortKey> {
    (0..width)
        .map(|i| SortKey {
            expr: Expr::Column(i),
            desc: false,
        })
        .collect()
}
