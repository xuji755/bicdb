//! **排序族**（切片 2c）：`Sort`（内存快排）与 `TopN`（有界堆）。
//!
//! 设计依据：`doc/执行算子设计_v0.1.md` §2.3/§4.1——`ORDER BY` 的 **NULL 位置**
//! 照 Oracle 默认（**升序在最后、降序在最前**）；`TopN = ORDER BY + LIMIT`
//! 的合并形态，**输入仍须全读**（证据 `2065263`：STOPKEY 只限排序内存，
//! 匹配行照扫）——真短路只发生在无 `ORDER BY` 的 `Limit` 上。
//!
//! **切片边界（2c→6b-2b）**：外部归并（有序 run + k 路归并）已接入；
//! WMM 两种额度形态都可用——**AUTO**（上下文接了共享池 ⇒ `Sort` 声明内存区、
//! 边读边量改档，额度随池内重平衡变动）与**固定值**（会话 MANUAL / 测试直给）。
//! 有溢出空间时超额度 ⇒ one-pass 落 run（`optimal`/`one-pass` 计数 + 额外
//! 字节记账）；无溢出空间仍报具名 [`ExecError::WorkMemoryExceeded`]（防线）。

use std::cmp::Ordering;

use crate::context::{ExecContext, WorkAreaOutcome};
use crate::error::ExecError;
use crate::expr::{self, Expr};
use crate::operator::Operator;
use crate::value::{row_bytes, Row, Value};
use crate::wmm::{AreaClaim, WorkArea};

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
///
/// **溢出（切片 6b）**：给了 [`crate::spill::SpillSpace`] 且超工作内存预算 ⇒
/// 分批排序、逐批写**有序 run**（落 temp 段），收尾做 **k 路归并**
/// （一次只持各 run 一行 + 一页）；未溢出 ⇒ 全内存排序（`optimal`）。
pub struct Sort<'a, 's, 'io> {
    input: Box<dyn Operator + 'a>,
    keys: Vec<SortKey>,
    spill: Option<&'s crate::spill::SpillSpace<'io>>,
    /// 内存区句柄（池形态；`None` = 固定预算/不限）。
    area: Option<WorkArea>,
    /// 已申报的 ideal（边读边量、倍增即改档；设计 §4.2.1 ①）。
    declared: u64,
    /// 溢出空间进入时的字节读数（收尾取差 ⇒ extra bytes）。
    bytes0: (u64, u64),
    runs: Vec<usize>,
    items: Vec<SortItem>,
    at: usize,
    loaded: bool,
    slot: usize,
    opened: bool,
}

impl<'a, 's, 'io> Sort<'a, 's, 'io> {
    /// 构造（无溢出空间——超预算即报错）。
    #[must_use]
    pub fn new(input: Box<dyn Operator + 'a>, keys: Vec<SortKey>) -> Self {
        Self {
            input,
            keys,
            spill: None,
            area: None,
            declared: 0,
            bytes0: (0, 0),
            runs: Vec::new(),
            items: Vec::new(),
            at: 0,
            loaded: false,
            slot: 0,
            opened: false,
        }
    }

    /// 带溢出空间（超预算 ⇒ 外部归并）。
    #[must_use]
    pub fn with_spill(mut self, spill: &'s crate::spill::SpillSpace<'io>) -> Self {
        self.spill = Some(spill);
        self
    }

    /// 把当前批排好序后写成一个 **有序 run**（溢出行 = 键 ++ 原行）。
    fn spill_batch(&mut self, cx: &mut ExecContext<'_>) -> Result<(), ExecError> {
        let space = self.spill.expect("调用方保证有溢出空间");
        sort_items(&mut self.items, &self.keys)?;
        let mut rows = Vec::with_capacity(self.items.len());
        for item in self.items.drain(..) {
            let mut values = item.keys;
            values.extend(item.row.values);
            rows.push(Row::new(values));
        }
        let run = space.write_run(&rows)?;
        self.runs.push(run);
        let _ = cx;
        Ok(())
    }

    /// **声明/改档**（边读边量；`benefit = ideal × 2`——排序省下的是
    /// "写一次 + 读一次"的 temp 流量）。倍增才重申报（避免逐行抖动）。
    fn redeclare(&mut self, used: u64) {
        let Some(area) = &self.area else { return };
        if used > self.declared && (self.declared == 0 || used >= self.declared.saturating_mul(2)) {
            area.regrade(AreaClaim {
                ideal: used,
                one_pass: 0, // 本实现的归并只持"各 run 一行 + 一页" ⇒ one-pass 内存 ≈ 0
                benefit: used.saturating_mul(2),
            });
            self.declared = used;
        }
    }

    /// 额度（池形态 ⇒ 当前 grant，**每次重读**；否则固定预算）。
    fn budget(&self, cx: &ExecContext<'_>) -> Option<u64> {
        cx.budget_for(self.area.as_ref())
    }

    /// 收尾记账：三态计数 + 额外字节（spill 空间的字节差）。
    fn note_done(&self, cx: &mut ExecContext<'_>, outcome: WorkAreaOutcome) {
        cx.note_work_area(outcome);
        if let Some(space) = self.spill {
            cx.note_extra_bytes(
                space.bytes_written() - self.bytes0.0,
                space.bytes_read() - self.bytes0.1,
            );
        }
    }
}

impl Operator for Sort<'_, '_, '_> {
    fn open(&mut self, cx: &mut ExecContext<'_>) -> Result<(), ExecError> {
        if !self.opened {
            self.slot = cx.register_op("Sort");
            self.opened = true;
        }
        // 池形态：声明内存区（未申报起步——边读边量改档）；固定预算形态不声明。
        if self.spill.is_some() {
            self.area = cx.claim_area("Sort");
            if let Some(space) = self.spill {
                self.bytes0 = (space.bytes_written(), space.bytes_read());
            }
        }
        self.input.open(cx)
    }

    fn next(&mut self, cx: &mut ExecContext<'_>) -> Result<Option<Row>, ExecError> {
        if !self.loaded {
            // 阻塞段：耗尽输入；超额度 ⇒ 分批落 run（有溢出空间时）。
            let mut used: u64 = 0;
            while let Some(row) = self.input.next(cx)? {
                let keys = keys_of(&self.keys, &row, cx.params())?;
                let item = SortItem { keys, row };
                used += item.bytes();
                self.items.push(item);
                self.redeclare(used);
                if let Some(budget) = self.budget(cx) {
                    if used > budget {
                        if self.spill.is_some() {
                            self.spill_batch(cx)?;
                            used = 0;
                        } else {
                            return Err(ExecError::WorkMemoryExceeded { used, budget });
                        }
                    }
                }
            }
            if self.runs.is_empty() {
                sort_items(&mut self.items, &self.keys)?;
                self.note_done(cx, WorkAreaOutcome::Optimal);
            } else {
                // 收尾：剩余批落 run ⇒ k 路归并（键在行前缀——比较无需再求值）。
                if !self.items.is_empty() {
                    self.spill_batch(cx)?;
                }
                let nkeys = self.keys.len();
                let keys = &self.keys;
                let runs = self.runs.clone();
                let space = self.spill.expect("有 run 必有溢出空间");
                let merged = space.merge_runs(&runs, |a, b| {
                    compare_keys(keys, &a.values[..nkeys], &b.values[..nkeys])
                })?;
                self.items = merged
                    .into_iter()
                    .map(|row| SortItem {
                        keys: Vec::new(),
                        row: Row::new(row.values[nkeys..].to_vec()),
                    })
                    .collect();
                self.note_done(cx, WorkAreaOutcome::OnePass);
            }
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
            return Ok(()); // 已装载（含已归并结果）⇒ 直接重放
        }
        self.items.clear();
        self.runs.clear();
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
