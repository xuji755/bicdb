//! **嵌套循环连接**（切片 3）：`INNER` / `LEFT`，内表**参数化重扫**。
//!
//! ```text
//! 外层取一行 → 对外层行求值 inner_params → 装进参数表 → rescan 内表
//!           → 内表逐行（连接条件在**组合行** outer ++ inner 上求值）
//!           → 内表耗尽：LEFT 且未匹配 ⇒ 补 NULL 行；还原语句参数、取下一外层行
//! ```
//!
//! **参数表纪律**：`NestedLoop` 是唯一改 `ExecContext` 参数表的算子——
//! 取外层行前**还原语句参数**、装内表参数后**只拉内表**（外层表达式不会
//! 在错误参数下求值；见 `ExecContext::set_params`）。
//!
//! **列编号约定**：连接条件里的列引用按**组合行**编号——外层列在前、
//! 内层列在后（Binder 按此编号；`inner_width` = 内层的列数）。

use crate::context::ExecContext;
use crate::error::ExecError;
use crate::expr::{self, Expr};
use crate::operator::Operator;
use crate::value::{Row, Value};

/// 连接类型（SQL 面：`INNER` / `LEFT`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    /// 内连接：只输出匹配对。
    Inner,
    /// 左外连接：外层无匹配时补 NULL。
    Left,
}

/// **嵌套循环**（内表按参数化重扫——索引探测是常态形态）。
pub struct NestedLoop<'a> {
    outer: Box<dyn Operator + 'a>,
    inner: Box<dyn Operator + 'a>,
    /// 内表参数（对**外层行**求值，按序装进参数表）。
    inner_params: Vec<Expr>,
    kind: JoinKind,
    /// 连接条件（在组合行上求值；`None` = 笛卡尔）。
    qual: Option<Expr>,
    /// 内层列数（LEFT 补 NULL 用）。
    inner_width: usize,
    current_outer: Option<Row>,
    matched: bool,
    base_params: Vec<Value>,
    slot: usize,
    opened: bool,
}

impl<'a> NestedLoop<'a> {
    /// 构造。
    #[must_use]
    pub fn new(
        outer: Box<dyn Operator + 'a>,
        inner: Box<dyn Operator + 'a>,
        inner_params: Vec<Expr>,
        kind: JoinKind,
        qual: Option<Expr>,
        inner_width: usize,
    ) -> Self {
        Self {
            outer,
            inner,
            inner_params,
            kind,
            qual,
            inner_width,
            current_outer: None,
            matched: false,
            base_params: Vec::new(),
            slot: 0,
            opened: false,
        }
    }

    /// 组合行 = 外层值 ++ 内层值。
    fn combine(outer: &Row, inner: &Row) -> Row {
        let mut values = Vec::with_capacity(outer.values.len() + inner.values.len());
        values.extend(outer.values.iter().cloned());
        values.extend(inner.values.iter().cloned());
        Row::new(values)
    }
}

impl Operator for NestedLoop<'_> {
    fn open(&mut self, cx: &mut ExecContext<'_>) -> Result<(), ExecError> {
        if !self.opened {
            self.slot = cx.register_op("NestedLoop");
            self.base_params = cx.params().to_vec();
            self.opened = true;
        }
        self.outer.open(cx)?;
        self.inner.open(cx)
    }

    fn next(&mut self, cx: &mut ExecContext<'_>) -> Result<Option<Row>, ExecError> {
        loop {
            cx.check()?;
            let outer = match self.current_outer.take() {
                Some(o) => o,
                None => {
                    // 取下一外层行前**还原语句参数**（外层表达式依赖它）。
                    cx.set_params(self.base_params.clone());
                    match self.outer.next(cx)? {
                        None => return Ok(None),
                        Some(o) => {
                            // 对外层行求值内表参数并装入参数表。
                            let mut params = self.base_params.clone();
                            for e in &self.inner_params {
                                params.push(expr::eval(e, &o, cx.params())?);
                            }
                            cx.set_params(params);
                            self.matched = false;
                            self.inner.rescan(cx)?;
                            o
                        }
                    }
                }
            };
            match self.inner.next(cx)? {
                Some(inner_row) => {
                    let combined = Self::combine(&outer, &inner_row);
                    // 内表参数追加在语句参数之后；ON 和上层表达式仍用原参数位。
                    if let Some(qual) = &self.qual {
                        if !expr::eval_where(qual, &combined, cx.params())? {
                            self.current_outer = Some(outer);
                            continue;
                        }
                    }
                    self.matched = true;
                    self.current_outer = Some(outer);
                    cx.note_row(self.slot);
                    return Ok(Some(combined));
                }
                None => {
                    if self.kind == JoinKind::Left && !self.matched {
                        let mut values = outer.values.clone();
                        values.extend(std::iter::repeat(Value::Null).take(self.inner_width));
                        cx.note_row(self.slot);
                        return Ok(Some(Row::new(values)));
                    }
                    // 内表耗尽、外层推进（下一轮循环自然处理）。
                }
            }
        }
    }

    fn close(&mut self, cx: &mut ExecContext<'_>) {
        self.outer.close(cx);
        self.inner.close(cx);
        cx.set_params(std::mem::take(&mut self.base_params));
    }
}
