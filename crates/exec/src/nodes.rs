//! **切片 1 的算子实现**：`SeqScan` / `Filter` / `Project` / `Limit`。
//!
//! 设计依据：`doc/执行算子设计_v0.1.md` §2.1/§2.5——`SeqScan` 的行来源是
//! 存储服务的行游标（**可见性由服务负责**）；`Filter`/`Project`/`Limit`
//! 是流水算子（短路自然）。

use crate::context::ExecContext;
use crate::error::ExecError;
use crate::expr::{self, Expr};
use crate::operator::{Operator, RowCursor};
use crate::value::{decode_row, Row, RowShape};

/// **顺序扫描**（全表扫描）：拉存储服务的行游标，逐行解码成值。
///
/// 扫描边界（HWM / append 位）与区读批量都由存储侧的游标承担（§4.3.1/§5.12）；
/// 本算子只做"拉一行、解一行"。
pub struct SeqScan<'a> {
    cursor: Box<dyn RowCursor + 'a>,
    shape: RowShape,
    slot: usize,
    opened: bool,
}

impl<'a> SeqScan<'a> {
    /// 构造（形状 = 表定义给出的列形态）。
    #[must_use]
    pub fn new(cursor: Box<dyn RowCursor + 'a>, shape: RowShape) -> Self {
        Self {
            cursor,
            shape,
            slot: 0,
            opened: false,
        }
    }
}

impl Operator for SeqScan<'_> {
    fn open(&mut self, cx: &mut ExecContext<'_>) -> Result<(), ExecError> {
        if self.opened {
            return Ok(());
        }
        self.slot = cx.register_op("SeqScan");
        self.opened = true;
        Ok(())
    }

    fn next(&mut self, cx: &mut ExecContext<'_>) -> Result<Option<Row>, ExecError> {
        cx.check()?;
        match self.cursor.next_row()? {
            None => Ok(None),
            Some((_rid, bytes)) => {
                let row = decode_row(&bytes, &self.shape)?;
                cx.note_row(self.slot);
                Ok(Some(row))
            }
        }
    }

    fn rescan(&mut self, _cx: &mut ExecContext<'_>) -> Result<(), ExecError> {
        // 复位存储游标（同快照、同边界）；不可复位的来源报具名错误。
        self.cursor.rewind()
    }
}

/// **过滤**（谓词只放行 TRUE；三值逻辑）。
pub struct Filter<'a> {
    input: Box<dyn Operator + 'a>,
    predicate: Expr,
    slot: usize,
    opened: bool,
}

impl<'a> Filter<'a> {
    /// 构造。
    #[must_use]
    pub fn new(input: Box<dyn Operator + 'a>, predicate: Expr) -> Self {
        Self {
            input,
            predicate,
            slot: 0,
            opened: false,
        }
    }
}

impl Operator for Filter<'_> {
    fn open(&mut self, cx: &mut ExecContext<'_>) -> Result<(), ExecError> {
        if !self.opened {
            self.slot = cx.register_op("Filter");
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
            if expr::eval_where(&self.predicate, &row, cx.params())? {
                cx.note_row(self.slot);
                return Ok(Some(row));
            }
        }
    }

    fn rescan(&mut self, cx: &mut ExecContext<'_>) -> Result<(), ExecError> {
        self.input.rescan(cx)
    }

    fn close(&mut self, cx: &mut ExecContext<'_>) {
        self.input.close(cx);
    }
}

/// **投影**（表达式求值；输出形状由构造方给出）。
pub struct Project<'a> {
    input: Box<dyn Operator + 'a>,
    exprs: Vec<Expr>,
    slot: usize,
    opened: bool,
}

impl<'a> Project<'a> {
    /// 构造（`exprs` 的顺序 = 输出列序）。
    #[must_use]
    pub fn new(input: Box<dyn Operator + 'a>, exprs: Vec<Expr>) -> Self {
        Self {
            input,
            exprs,
            slot: 0,
            opened: false,
        }
    }
}

impl Operator for Project<'_> {
    fn open(&mut self, cx: &mut ExecContext<'_>) -> Result<(), ExecError> {
        if !self.opened {
            self.slot = cx.register_op("Project");
            self.opened = true;
        }
        self.input.open(cx)
    }

    fn next(&mut self, cx: &mut ExecContext<'_>) -> Result<Option<Row>, ExecError> {
        cx.check()?;
        let Some(row) = self.input.next(cx)? else {
            return Ok(None);
        };
        let mut values = Vec::with_capacity(self.exprs.len());
        for e in &self.exprs {
            values.push(expr::eval(e, &row, cx.params())?);
        }
        cx.note_row(self.slot);
        Ok(Some(Row::new(values)))
    }

    fn rescan(&mut self, cx: &mut ExecContext<'_>) -> Result<(), ExecError> {
        self.input.rescan(cx)
    }

    fn close(&mut self, cx: &mut ExecContext<'_>) {
        self.input.close(cx);
    }
}

/// **限行**（`LIMIT n OFFSET m`）：拉到足够即**不再调子**——无 ORDER BY 时
/// 这是真正的短路（设计 §4.1：有 ORDER BY 时排序已耗尽输入，短路只省下游）。
pub struct Limit<'a> {
    input: Box<dyn Operator + 'a>,
    limit: u64,
    offset: u64,
    produced: u64,
    skipped: u64,
    slot: usize,
    opened: bool,
}

impl<'a> Limit<'a> {
    /// 构造。
    #[must_use]
    pub fn new(input: Box<dyn Operator + 'a>, limit: u64, offset: u64) -> Self {
        Self {
            input,
            limit,
            offset,
            produced: 0,
            skipped: 0,
            slot: 0,
            opened: false,
        }
    }
}

impl Operator for Limit<'_> {
    fn open(&mut self, cx: &mut ExecContext<'_>) -> Result<(), ExecError> {
        if !self.opened {
            self.slot = cx.register_op("Limit");
            self.opened = true;
        }
        self.input.open(cx)
    }

    fn next(&mut self, cx: &mut ExecContext<'_>) -> Result<Option<Row>, ExecError> {
        if self.produced >= self.limit {
            return Ok(None); // 短路：不再调子
        }
        cx.check()?;
        while self.skipped < self.offset {
            match self.input.next(cx)? {
                None => return Ok(None),
                Some(_) => self.skipped += 1,
            }
        }
        match self.input.next(cx)? {
            None => Ok(None),
            Some(row) => {
                self.produced += 1;
                cx.note_row(self.slot);
                Ok(Some(row))
            }
        }
    }

    fn rescan(&mut self, cx: &mut ExecContext<'_>) -> Result<(), ExecError> {
        self.produced = 0;
        self.skipped = 0;
        self.input.rescan(cx)
    }

    fn close(&mut self, cx: &mut ExecContext<'_>) {
        self.input.close(cx);
    }
}
