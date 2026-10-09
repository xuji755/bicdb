//! **索引扫描**（切片 3）：索引定位 + **批量回表**（设计 §2.1/§9.4）。
//!
//! ```text
//! ① 索引定位：Tree::range(low, high, limit)（闭区间；唯一扫描 = 退化形态）
//! ② 攒批：默认 256 条（§9.4 的批大小）
//! ③ 按 (file_id, block_id, slot) 排序 → 按块分组
//!    → 每块**一次区读 + 一次 CR 块重建** → 逐行提取（回表）
//! ④ 覆盖索引（`covered`）：不回表——只把**键**解码成行（§9.4 的 index-only）
//! ```
//!
//! # 两条正确性边界（证据包 `doc/evidence/index-access-20261007/`）
//!
//! 1. **索引只用来"缩小候选"，不承担正确性**：索引项**不携带可见性**（PG 同款——
//!    "索引条目本身不携带这些可见性字段"，所以仅索引扫描仍须回表校验），
//!    而本仓的索引项**只插不删**（`arch/09` §9.1.2 的移除要 undo）⇒ 索引是
//!    **活行的超集**：取到条目后**一律回表**，谓词由上层 `Filter` 复核。
//!    **因此不许在索引层提前限行**：某个键的**第一个条目可能是陈旧项**
//!    （回表落空），真正的活行排在后面——`limit: 1` 会静默漏行。
//! 3. **同一条活行只出一行**（按**解析转发之后**的物理 ROWID 去重）：改键列
//!    **只追加**新项、不移动旧项，于是范围覆盖新旧两个键时同一条行会被两条项
//!    各带出一次。`covered`（不回表）**拿不到物理 ROWID ⇒ 去不了重**——这也是它
//!    不接线的又一条理由（见上一条）。
//!
//! 2. **键 = 复合编码的单列形态**（`storage::key::encode(&[Some(保序编码)])`）——
//!    真实索引条目就是这个形态（`catalog::row::key_from_row`）。多列键的接线
//!    （等值钉满全部键列 / 前缀范围）是后续切片，届时高界要用 NULL 分量补满
//!    （`key::MARK_NULL` 是分量里的最大值）。
//!
//! **切片边界**：索引条目在 `open` 一次收齐（受 `limit` 界——SQL 面的扫描
//! 都带界或小表）；**流式游标**（池页闩耦合的增量取条目）随执行器/会话切片。
//! `covered`（仅索引、不回表）**本仓不接线**：免回表的前提是"页级可见性位图"
//! （PG 的 visibility map），我们没有——见证据包 §3。

use std::collections::VecDeque;

use bicdb_index::{PageStore, ReadOnlyStore, Tree};
use bicdb_storage::buffer::BufferPool;
use bicdb_storage::rowid::RowId;
use bicdb_storage::scan;
use bicdb_storage::undo::UndoChain;
use bicdb_types::Number;

use crate::context::ExecContext;
use crate::error::ExecError;
use crate::expr::{self, Expr};
use crate::operator::Operator;
use crate::value::{decode_row, ColKind, Row, RowShape, Value};

/// 回表攒批的默认大小（§9.4：256 条）。
pub const DEFAULT_FETCH_BATCH: usize = 256;

/// **索引扫描算子**。
pub struct IndexScan<'a, 'b, 'io, 'f> {
    pool: &'a BufferPool<'b>,
    chain: &'a UndoChain<'io, 'f>,
    /// 索引段所在文件号。
    pub(crate) file_id: u16,
    /// **索引段的段头块**（`seg$.block_id`）——树头（根页 ROWID）在**段头页的
    /// 扩展区**里，执行期现读（`取的时候再读`：计划期捕获的根页可能已随分裂变化）。
    pub(crate) seg_page0: u32,
    /// 键的列形态（解码键用；覆盖扫描的整行 = 一个键列）。
    pub(crate) key_kind: ColKind,
    /// 回表行的形状（`covered` 时忽略）。
    pub(crate) shape: RowShape,
    /// 范围下界（`None` = 无界——求值发生在 open/rescan）。
    pub(crate) low: Option<Expr>,
    /// 下界是否**排除**（`>` 形态；`>=`/`=` 为闭）。
    pub(crate) low_exclusive: bool,
    /// 范围上界（`None` = 无界）。
    pub(crate) high: Option<Expr>,
    /// **多点探测**（非空 = `IN (值表)` 形态：逐点各做一次闭区间点查）。
    ///
    /// 为什么在**一个算子**里做而不是串 N 个算子（`Append` 各一支）：去重集
    /// （[`IndexScan::seen_rows`]）是**按算子**一份的——串起来的话，一条活行经
    /// 两条索引项（改键列留下的旧项 + 新项）从**两个分支**各出一次，谁也没重复
    /// 可判（实测抓到过：`k IN (1,3)` 在 `UPDATE k=1→3` 之后把那一行出了两次）。
    pub(crate) points: Vec<Expr>,
    /// 上界是否**排除**（`<` 形态；`<=`/`=` 为闭）。
    pub(crate) high_exclusive: bool,
    /// 覆盖扫描（不回表——查询只碰索引列）。
    pub(crate) covered: bool,
    /// 条目上限（`None` = 不限）。
    pub(crate) limit: Option<u64>,
    batch: usize,
    /// 待回表的条目（键, ROWID）。
    entries: VecDeque<(Vec<u8>, RowId)>,
    /// **本趟已出过的物理行**（去重用）：同一条活行可能有多条索引项——改键列
    /// **只追加**新项、不移动旧项（索引写没有 undo）⇒ 范围覆盖新旧两个键时，
    /// 同一条行会被两条项各带出一次。去重键是**解析转发之后**的物理 ROWID。
    seen_rows: std::collections::HashSet<RowId>,
    /// 已取回、待吐出的行。
    pending: VecDeque<Row>,
    done: bool,
    /// 待定位（惰性：`open`/`rescan` 置位，首次 `next` 才收条目——
    /// `NestedLoop` 的参数在 `rescan` 之后才装入，不能提前求值边界）。
    pending_collect: bool,
    slot: usize,
    opened: bool,
    /// 诊断：回表批次数。
    pub fetch_batches: u64,
}

impl<'a, 'b, 'io, 'f> IndexScan<'a, 'b, 'io, 'f> {
    /// 构造（`pool`/`chain` 是存储服务口；`file_id`/`seg_page0` 来自段）。
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        pool: &'a BufferPool<'b>,
        chain: &'a UndoChain<'io, 'f>,
        file_id: u16,
        seg_page0: u32,
        key_kind: ColKind,
        shape: RowShape,
        low: Option<Expr>,
        high: Option<Expr>,
        covered: bool,
        limit: Option<u64>,
    ) -> Self {
        Self {
            pool,
            chain,
            file_id,
            seg_page0,
            key_kind,
            shape,
            low,
            low_exclusive: false,
            high,
            high_exclusive: false,
            points: Vec::new(),
            covered,
            limit,
            batch: DEFAULT_FETCH_BATCH,
            entries: VecDeque::new(),
            seen_rows: std::collections::HashSet::new(),
            pending: VecDeque::new(),
            done: false,
            pending_collect: true,
            slot: 0,
            opened: false,
            fetch_batches: 0,
        }
    }

    /// **多点探测**（`IN (值表)` 形态：逐点一次点查，**共用一份去重集**）。
    #[must_use]
    pub fn with_points(mut self, points: Vec<Expr>) -> Self {
        self.points = points;
        self
    }

    /// **下界排除**（`col > k` 形态；默认闭区间 `>=`）。
    #[must_use]
    pub fn low_exclusive(mut self, on: bool) -> Self {
        self.low_exclusive = on;
        self
    }

    /// **上界排除**（`col < k` 形态；默认闭区间 `<=`）。
    #[must_use]
    pub fn high_exclusive(mut self, on: bool) -> Self {
        self.high_exclusive = on;
        self
    }

    /// 覆盖扫描（只碰索引列）——`SELECT count(*)` / 只选索引列时的形态。
    #[must_use]
    pub fn covered(mut self) -> Self {
        self.covered = true;
        self
    }

    /// 回表批量大小（默认 256——§9.4；测试用来观测分批）。
    #[must_use]
    pub fn with_batch(mut self, batch: usize) -> Self {
        self.batch = batch.max(1);
        self
    }

    /// 定位并收齐条目（open 段；见模块文档的切片边界）。
    fn collect_entries(&mut self, cx: &mut ExecContext<'_>) -> Result<(), ExecError> {
        let ws = self.chain.segment().workspace_ref();
        let mut store = ReadOnlyStore::new(self.pool, self.file_id, ws);
        // **树头现读**（段头页扩展区）：段头是权威——计划期捕获的根页可能已被
        // 分裂改过（`Tree::open` 拿到的根页地址一直从这里取）。
        let head = store.read(self.seg_page0)?;
        let root = bicdb_storage::segment::read_tree_head(&head)
            .map_err(|e| ExecError::BadStoredRow(format!("索引段头里的树头：{e}")))?;
        let mut tree = Tree::open(&mut store, self.file_id, root)?;
        // **多点探测**（`IN`）：逐点各一次闭区间点查，条目进**同一个**队列——
        // 去重（`seen_rows`）因此跨点生效。点求值为 NULL ⇒ 跳过（`= NULL` 恒不成立）。
        if !self.points.is_empty() {
            let limit = match self.limit {
                Some(n) => n as usize,
                None => usize::MAX,
            };
            for e in &self.points.clone() {
                match expr::eval(e, &Row::new(Vec::new()), cx.params())? {
                    Value::Null => continue,
                    v => {
                        let key = bound_key(&v)?;
                        for (k, rid) in tree.range(Some(&key), Some(&key), limit)? {
                            self.entries.push_back((k, rid));
                        }
                    }
                }
            }
            return Ok(());
        }
        // 界的三种情形：**没给 = 无界**；**给了且算出来是 NULL ⇒ 零行**；
        // 给了且是值 ⇒ 有界（见 [`Bound`]）。
        let low = self.bound(cx, self.low.as_ref())?;
        let high = self.bound(cx, self.high.as_ref())?;
        if low.is_empty() || high.is_empty() {
            // `= NULL` 恒不成立（三值逻辑）：直接零行。把它当"无界"会让
            // `WHERE k = :p`（p 传 NULL）静默退化成全索引扫描。
            return Ok(());
        }
        let low_bytes = low.bytes()?;
        let high_bytes = high.bytes()?;
        let limit = match self.limit {
            Some(n) => n as usize,
            None => usize::MAX,
        };
        let found = tree.range(low_bytes.as_deref(), high_bytes.as_deref(), limit)?;
        for (key, rid) in found {
            // **开区间的端点**：`Tree::range` 只有闭区间，端点相等的那几条在这里
            // 剔掉（键是字节串，相等就是相等——不做 successor/前驱的字节算术）。
            // 代价 = 端点同键的条目数（索引里同键条目本就成组）。
            if self.low_exclusive && Some(key.as_slice()) == low_bytes.as_deref() {
                continue;
            }
            if self.high_exclusive && Some(key.as_slice()) == high_bytes.as_deref() {
                continue;
            }
            self.entries.push_back((key, rid));
        }
        Ok(())
    }

    /// 求一侧的界（对**空行**求值——界表达式里不该有列引用）。
    fn bound(&self, cx: &mut ExecContext<'_>, e: Option<&Expr>) -> Result<Bound, ExecError> {
        match e {
            None => Ok(Bound::Unbounded),
            Some(e) => match expr::eval(e, &Row::new(Vec::new()), cx.params())? {
                Value::Null => Ok(Bound::Empty),
                v => Ok(Bound::Key(bound_key(&v)?)),
            },
        }
    }

    /// 从待回表条目取一批（`batch` 条）→ 回表 → 解码进 `pending`。
    fn fetch_batch(&mut self, cx: &mut ExecContext<'_>) -> Result<(), ExecError> {
        let mut keys = Vec::new();
        let mut rids = Vec::new();
        while rids.len() < self.batch {
            match self.entries.pop_front() {
                Some((k, r)) => {
                    keys.push(k);
                    rids.push(r);
                }
                None => break,
            }
        }
        if rids.is_empty() {
            return Ok(());
        }
        self.fetch_batches += 1;
        if self.covered {
            // 覆盖扫描：不回表——键解码即结果行。
            for key in keys {
                let v = decode_key(&key, self.key_kind)?;
                self.pending.push_back(Row::new(vec![v]));
            }
            return Ok(());
        }
        // **批量回表**（§9.4）：排序 → 按块分组 → 每块一次区读 + 一次 CR。
        // 带**解析后的物理 ROWID**（去重要用它：行迁移过的话，旧项记的入口与
        // 新项记的物理位置 RID 不同，却指向同一行）。
        let mut sorted = rids.clone();
        scan::sort_rowids(&mut sorted);
        let rows = scan::fetch_rows_resolved(self.pool, self.chain, cx.read_view(), &sorted)?;
        // 回表结果与请求同序（sorted）；再映射回**键序**输出。
        let mut by_rid: std::collections::HashMap<RowId, usize> = std::collections::HashMap::new();
        for (i, rid) in sorted.iter().enumerate() {
            by_rid.insert(*rid, i);
        }
        for (key, rid) in keys.into_iter().zip(rids) {
            let _ = key;
            let idx = *by_rid.get(&rid).expect("请求的 ROWID 必在结果里");
            if let Some((phys, bytes)) = &rows[idx] {
                // **同一条活行只出一行**：两条索引项指向同一物理行时（改键列留下的
                // 旧项 + 新项），只认先到的那条——否则范围扫描会把同一条行吐两次
                // （实测抓到的正是这条：`UPDATE k=1→3` 之后 `BETWEEN 1 AND 3` 出两行）。
                if !self.seen_rows.insert(*phys) {
                    continue;
                }
                self.pending.push_back(decode_row(bytes, &self.shape)?);
            }
            // `None` = 该行在快照下不存在（并发删除后索引项成死行）——跳过。
        }
        Ok(())
    }
}

impl Operator for IndexScan<'_, '_, '_, '_> {
    fn open(&mut self, cx: &mut ExecContext<'_>) -> Result<(), ExecError> {
        if self.opened {
            return Ok(());
        }
        self.slot = cx.register_op("IndexScan");
        // **惰性定位**：见 `pending_collect`（参数由外层在 rescan 时装好）。
        self.pending_collect = true;
        self.opened = true;
        Ok(())
    }

    fn next(&mut self, cx: &mut ExecContext<'_>) -> Result<Option<Row>, ExecError> {
        loop {
            cx.check()?;
            if self.pending_collect {
                self.collect_entries(cx)?;
                self.pending_collect = false;
            }
            if let Some(row) = self.pending.pop_front() {
                cx.note_row(self.slot);
                return Ok(Some(row));
            }
            if self.entries.is_empty() || self.done {
                self.done = true;
                return Ok(None);
            }
            self.fetch_batch(cx)?;
        }
    }

    fn rescan(&mut self, _cx: &mut ExecContext<'_>) -> Result<(), ExecError> {
        self.entries.clear();
        self.seen_rows.clear();
        self.pending.clear();
        self.done = false;
        self.pending_collect = true; // 参数已由外层换好——下次 `next` 定位
        Ok(())
    }
}

/// 键字节 → 值（按 `key_kind` 解码）。
/// **一侧的界**（界表达式的三种情形）。
enum Bound {
    /// 没给界（该侧无界）。
    Unbounded,
    /// 给了界、但求值为 `NULL` ⇒ **零行**（`= NULL` 恒不成立）。
    Empty,
    /// 有界（已按复合键编码）。
    Key(Vec<u8>),
}

impl Bound {
    /// 零行吗。
    fn is_empty(&self) -> bool {
        matches!(self, Bound::Empty)
    }

    /// 键字节（`None` = 无界）。
    fn bytes(&self) -> Result<Option<Vec<u8>>, ExecError> {
        match self {
            Bound::Unbounded => Ok(None),
            Bound::Empty => unreachable!("空界在调用方已拦（is_empty）"),
            Bound::Key(b) => Ok(Some(b.clone())),
        }
    }
}

/// **界值 → 索引键字节**（复合编码的**单列形态**）。
///
/// 真实索引条目是 `key::encode(&[Some(保序编码)])`（`catalog::row::key_from_row`）：
/// `0x01 ‖ 载荷（0x00 转义）‖ 0x00`。这里**必须**同一形态——裸的列编码
/// （`Number::encode`）与树里的键**前缀不同**，比较恒不相等（审计 R4：
/// "接线前必修"；`crates/exec/tests/index_join.rs` 有用例钉住这一点）。
fn bound_key(v: &Value) -> Result<Vec<u8>, ExecError> {
    let payload = match v {
        Value::Null => unreachable!("NULL 由 Bound::Empty 表达"),
        Value::Number(n) => n.encode(),
        Value::Bool(b) => bicdb_types::encode_boolean(*b).to_vec(),
        Value::Bytes(b) => b.clone(),
        Value::GraphElement(v) => crate::value::encode_graph_element(*v).to_vec(),
    };
    Ok(bicdb_storage::key::encode(&[Some(&payload)]))
}

/// **索引键字节 → 值**（复合编码的**单列形态**；只给 `covered` 用）。
fn decode_key(key: &[u8], kind: ColKind) -> Result<Value, ExecError> {
    let comps = bicdb_storage::key::decode(key)
        .map_err(|e| ExecError::BadStoredRow(format!("索引键解码：{e}")))?;
    let [Some(payload)] = comps.as_slice() else {
        return Err(ExecError::BadStoredRow(format!(
            "索引键不是单列复合形态（分量 {} 个）",
            comps.len()
        )));
    };
    Ok(match kind {
        ColKind::Number => Value::Number(
            Number::decode(payload).map_err(|e| ExecError::BadStoredRow(e.to_string()))?,
        ),
        ColKind::Bool => Value::Bool(
            bicdb_types::decode_boolean(payload)
                .map_err(|e| ExecError::BadStoredRow(e.to_string()))?,
        ),
        ColKind::Bytes => Value::Bytes(payload.clone()),
        ColKind::GraphElement => Value::GraphElement(crate::value::decode_graph_element(payload)?),
    })
}
