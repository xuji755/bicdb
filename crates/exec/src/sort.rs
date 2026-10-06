//! **排序族**（切片 2c）：`Sort`（内存快排）与 `TopN`（有界堆）。
//!
//! 设计依据：`doc/执行算子设计_v0.1.md` §2.3/§4.1——`ORDER BY` 的 **NULL 位置**
//! 照 Oracle 默认（**升序在最后、降序在最前**）；`TopN = ORDER BY + LIMIT`
//! 的合并形态，**输入仍须全读**（证据 `2065263`：STOPKEY 只限排序内存，
//! 匹配行照扫）——真短路只发生在无 `ORDER BY` 的 `Limit` 上。
//!
//! **切片边界（2c）**：本切片为**内存形态**；超工作内存预算报具名
//! [`ExecError::WorkMemoryExceeded`]——**外部归并（有序 run + k 路归并）随
//! 切片 6 的 WMM + temp 段接入**（届时超预算改走 one-pass 溢出，本错误在
//! 正常路径不可达）。WMM 最小面已在此接入：预算检查 + `optimal` 计数。

use std::cmp::Ordering;

use crate::context::{ExecContext, WorkAreaOutcome};
use crate::error::ExecError;
use crate::expr::{self, Expr};
use crate::operator::Operator;
use crate::value::{row_bytes, Row, Value};

/// 排序键（表达式 + 方向）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SortKey {
    /// 键表达式。
    pub expr: Expr,
    /// 是否降序（`DESC`；缺省 = 升序）。
    pub desc: bool,
}

/// 一行 + 其求值后的键（排序过程只比较键，不重复求值）。
struct SortItem {
    keys: Vec<Value>,
    row: Row,
}

impl SortItem {
    fn bytes(&self) -> u64 {
        row_bytes(&self.row) as u64 + 32
    }
}

/// **逐键比较**（NULL 位置：升序最后、降序最前——Oracle 默认）。
pub fn compare_keys(keys: &[SortKey], a: &[Value], b: &[Value]) -> Result<Ordering, ExecError> {
    for (i, k) in keys.iter().enumerate() {
        let (x, y) = (&a[i], &b[i]);
        let ord = match (x.is_null(), y.is_null()) {
            (true, true) => Ordering::Equal,
            // 升序：NULL 排最后（NULL > 非 NULL）；降序：NULL 排最前。
            (true, false) => {
                if k.desc {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (false, true) => {
                if k.desc {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            (false, false) => {
                let ord = expr::compare_values(x, y)?.expect("两侧都非 NULL ⇒ 有定序");
                if k.desc {
                    ord.reverse()
                } else {
                    ord
                }
            }
        };
        if ord != Ordering::Equal {
            return Ok(ord);
        }
    }
    Ok(Ordering::Equal)
}

/// 求一行的全部键值。
fn keys_of(keys: &[SortKey], row: &Row, params: &[Value]) -> Result<Vec<Value>, ExecError> {
    keys.iter()
        .map(|k| expr::eval(&k.expr, row, params))
        .collect()
}

/// 预算检查（切片 2c 的内存形态；见模块文档）。
fn check_budget(cx: &ExecContext<'_>, used: u64) -> Result<(), ExecError> {
    if let Some(budget) = cx.work_memory_budget() {
        if used > budget {
            return Err(ExecError::WorkMemoryExceeded { used, budget });
        }
    }
    Ok(())
}

/// 对 `items` 按 `keys` 稳定排序（比较错误保真外传——不静默）。
fn sort_items(items: &mut [SortItem], keys: &[SortKey]) -> Result<(), ExecError> {
    let mut err: Option<ExecError> = None;
    items.sort_by(|a, b| match compare_keys(keys, &a.keys, &b.keys) {
        Ok(ord) => ord,
        Err(e) => {
            if err.is_none() {
                err = Some(e);
            }
            Ordering::Equal
        }
    });
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// **`Sort`**（阻塞：首次 `next` 耗尽输入并排序，其后流式吐出）。
pub struct Sort<'a> {
    input: Box<dyn Operator + 'a>,
    keys: Vec<SortKey>,
    items: Vec<SortItem>,
    at: usize,
    loaded: bool,
    slot: usize,
    opened: bool,
}

impl<'a> Sort<'a> {
    /// 构造。
    #[must_use]
    pub fn new(input: Box<dyn Operator + 'a>, keys: Vec<SortKey>) -> Self {
        Self {
            input,
            keys,
            items: Vec::new(),
            at: 0,
            loaded: false,
            slot: 0,
            opened: false,
        }
    }
}

impl Operator for Sort<'_> {
    fn open(&mut self, cx: &mut ExecContext<'_>) -> Result<(), ExecError> {
        if !self.opened {
            self.slot = cx.register_op("Sort");
            self.opened = true;
        }
        self.input.open(cx)
    }

    fn next(&mut self, cx: &mut ExecContext<'_>) -> Result<Option<Row>, ExecError> {
        if !self.loaded {
            // 阻塞段：耗尽输入、随增长记账（超预算早失败）、排序。
            let mut used: u64 = 0;
            while let Some(row) = self.input.next(cx)? {
                let keys = keys_of(&self.keys, &row, cx.params())?;
                let item = SortItem { keys, row };
                used += item.bytes();
                check_budget(cx, used)?;
                self.items.push(item);
            }
            sort_items(&mut self.items, &self.keys)?;
            cx.note_work_area(WorkAreaOutcome::Optimal); // WMM 最小面（切片 6 扩三态）
            self.loaded = true;
        }
        match self.items.get(self.at) {
            None => Ok(None),
            Some(item) => {
                self.at += 1;
                cx.note_row(self.slot);
                Ok(Some(item.row.clone()))
            }
        }
    }

    fn rescan(&mut self, cx: &mut ExecContext<'_>) -> Result<(), ExecError> {
        self.at = 0;
        if self.loaded {
            return Ok(()); // 已装载 ⇒ 直接重放
        }
        self.items.clear();
        self.input.rescan(cx)
    }

    fn close(&mut self, cx: &mut ExecContext<'_>) {
        self.items.clear();
        self.input.close(cx);
    }
}

/// **`TopN`**（有界堆）：只保留输出序前 `limit` 行。**输入仍全读**
/// （证据 `2065263`）——省的是排序内存，不是扫描。
///
/// 堆为**显式二叉堆**（带算子比较器——`BinaryHeap` 要求 `Ord`，而我们的
/// 比较依赖逐算子键与方向，且要能报类型错误）。
pub struct TopN<'a> {
    input: Box<dyn Operator + 'a>,
    keys: Vec<SortKey>,
    limit: u64,
    /// 堆顶 = 输出序里**最靠后**的一行（超限时淘汰它）。
    heap: Vec<SortItem>,
    emitted: Vec<Row>,
    at: usize,
    loaded: bool,
    slot: usize,
    opened: bool,
    used: u64,
}

impl<'a> TopN<'a> {
    /// 构造（`limit` 已含 `OFFSET` 的份额——由计划侧给）。
    #[must_use]
    pub fn new(input: Box<dyn Operator + 'a>, keys: Vec<SortKey>, limit: u64) -> Self {
        Self {
            input,
            keys,
            limit,
            heap: Vec::new(),
            emitted: Vec::new(),
            at: 0,
            loaded: false,
            slot: 0,
            opened: false,
            used: 0,
        }
    }

    fn cmp(&self, a: &SortItem, b: &SortItem) -> Result<Ordering, ExecError> {
        compare_keys(&self.keys, &a.keys, &b.keys)
    }

    /// 入堆（自底上浮；堆序 = 输出序的**大到小**）。
    fn heap_push(&mut self, item: SortItem) -> Result<(), ExecError> {
        self.heap.push(item);
        let mut i = self.heap.len() - 1;
        while i > 0 {
            let parent = (i - 1) / 2;
            if self.cmp(&self.heap[i], &self.heap[parent])? == Ordering::Greater {
                self.heap.swap(i, parent);
                i = parent;
            } else {
                break;
            }
        }
        Ok(())
    }

    /// 弹出堆顶（输出序里最靠后者）。
    fn heap_pop(&mut self) -> Result<Option<SortItem>, ExecError> {
        let Some(last) = self.heap.pop() else {
            return Ok(None);
        };
        if self.heap.is_empty() {
            return Ok(Some(last));
        }
        let top = std::mem::replace(&mut self.heap[0], last);
        let mut i = 0usize;
        loop {
            let (l, r) = (2 * i + 1, 2 * i + 2);
            let mut largest = i;
            if l < self.heap.len()
                && self.cmp(&self.heap[l], &self.heap[largest])? == Ordering::Greater
            {
                largest = l;
            }
            if r < self.heap.len()
                && self.cmp(&self.heap[r], &self.heap[largest])? == Ordering::Greater
            {
                largest = r;
            }
            if largest == i {
                break;
            }
            self.heap.swap(i, largest);
            i = largest;
        }
        Ok(Some(top))
    }
}

impl Operator for TopN<'_> {
    fn open(&mut self, cx: &mut ExecContext<'_>) -> Result<(), ExecError> {
        if !self.opened {
            self.slot = cx.register_op("TopN");
            self.opened = true;
        }
        self.input.open(cx)
    }

    fn next(&mut self, cx: &mut ExecContext<'_>) -> Result<Option<Row>, ExecError> {
        if !self.loaded {
            while let Some(row) = self.input.next(cx)? {
                let keys = keys_of(&self.keys, &row, cx.params())?;
                let item = SortItem { keys, row };
                if (self.heap.len() as u64) < self.limit {
                    self.used += item.bytes();
                    check_budget(cx, self.used)?;
                    self.heap_push(item)?;
                } else if self.limit > 0 {
                    let top = self.heap.first().expect("limit > 0 ⇒ 堆非空");
                    if self.cmp(&item, top)? == Ordering::Less {
                        let removed = self.heap_pop()?.expect("非空堆");
                        self.used = self.used.saturating_sub(removed.bytes());
                        self.used += item.bytes();
                        check_budget(cx, self.used)?;
                        self.heap_push(item)?;
                    }
                }
            }
            // 收尾：堆内容按输出序排好即结果。
            let mut items = std::mem::take(&mut self.heap);
            sort_items(&mut items, &self.keys)?;
            self.emitted = items.into_iter().map(|i| i.row).collect();
            cx.note_work_area(WorkAreaOutcome::Optimal);
            self.loaded = true;
        }
        match self.emitted.get(self.at) {
            None => Ok(None),
            Some(row) => {
                let row = row.clone();
                self.at += 1;
                cx.note_row(self.slot);
                Ok(Some(row))
            }
        }
    }

    fn rescan(&mut self, cx: &mut ExecContext<'_>) -> Result<(), ExecError> {
        self.at = 0;
        if self.loaded {
            return Ok(());
        }
        self.heap.clear();
        self.used = 0;
        self.input.rescan(cx)
    }

    fn close(&mut self, cx: &mut ExecContext<'_>) {
        self.heap.clear();
        self.emitted.clear();
        self.input.close(cx);
    }
}
