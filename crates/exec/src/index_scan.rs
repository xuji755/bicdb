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
//! **切片边界**：索引条目在 `open` 一次收齐（受 `limit` 界——SQL 面的扫描
//! 都带界或小表）；**流式游标**（池页闩耦合的增量取条目）随执行器/会话切片。

use std::collections::VecDeque;

use bicdb_index::{ReadOnlyStore, Tree};
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
    /// 树头（根页 ROWID——执行器从段头扩展区读得）。
    pub(crate) root: RowId,
    /// 键的列形态（解码键用；覆盖扫描的整行 = 一个键列）。
    pub(crate) key_kind: ColKind,
    /// 回表行的形状（`covered` 时忽略）。
    pub(crate) shape: RowShape,
    /// 范围下界（闭区间；`None` = 无界——求值发生在 open/rescan）。
    pub(crate) low: Option<Expr>,
    /// 范围上界（闭区间；`None` = 无界）。
    pub(crate) high: Option<Expr>,
    /// 覆盖扫描（不回表——查询只碰索引列）。
    pub(crate) covered: bool,
    /// 条目上限（`None` = 不限）。
    pub(crate) limit: Option<u64>,
    batch: usize,
    /// 待回表的条目（键, ROWID）。
    entries: VecDeque<(Vec<u8>, RowId)>,
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
    /// 构造（`pool`/`chain` 是存储服务口；`file_id`/`root` 来自段与树头）。
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        pool: &'a BufferPool<'b>,
        chain: &'a UndoChain<'io, 'f>,
        file_id: u16,
        root: RowId,
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
            root,
            key_kind,
            shape,
            low,
            high,
            covered,
            limit,
            batch: DEFAULT_FETCH_BATCH,
            entries: VecDeque::new(),
            pending: VecDeque::new(),
            done: false,
            pending_collect: true,
            slot: 0,
            opened: false,
            fetch_batches: 0,
        }
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
        let mut tree = Tree::open(&mut store, self.file_id, self.root)?;
        let low = match &self.low {
            Some(e) => Some(expr::eval(e, &Row::new(Vec::new()), cx.params())?),
            None => None,
        };
        let high = match &self.high {
            Some(e) => Some(expr::eval(e, &Row::new(Vec::new()), cx.params())?),
            None => None,
        };
        let low_bytes = key_bytes(low.as_ref())?;
        let high_bytes = key_bytes(high.as_ref())?;
        let limit = match self.limit {
            Some(n) => n as usize,
            None => usize::MAX,
        };
        let found = tree.range(low_bytes.as_deref(), high_bytes.as_deref(), limit)?;
        for (key, rid) in found {
            self.entries.push_back((key, rid));
        }
        Ok(())
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
        let mut sorted = rids.clone();
        scan::sort_rowids(&mut sorted);
        let rows = scan::fetch_rows(self.pool, self.chain, cx.snapshot(), &sorted)?;
        // 回表结果与请求同序（sorted）；再映射回**键序**输出。
        let mut by_rid: std::collections::HashMap<RowId, usize> = std::collections::HashMap::new();
        for (i, rid) in sorted.iter().enumerate() {
            by_rid.insert(*rid, i);
        }
        for (key, rid) in keys.into_iter().zip(rids) {
            let _ = key;
            let idx = *by_rid.get(&rid).expect("请求的 ROWID 必在结果里");
            if let Some(bytes) = &rows[idx] {
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
        self.pending.clear();
        self.done = false;
        self.pending_collect = true; // 参数已由外层换好——下次 `next` 定位
        Ok(())
    }
}

/// 键字节 → 值（按 `key_kind` 解码）。
fn decode_key(key: &[u8], kind: ColKind) -> Result<Value, ExecError> {
    Ok(match kind {
        ColKind::Number => {
            Value::Number(Number::decode(key).map_err(|e| ExecError::BadStoredRow(e.to_string()))?)
        }
        ColKind::Bool => Value::Bool(
            bicdb_types::decode_boolean(key).map_err(|e| ExecError::BadStoredRow(e.to_string()))?,
        ),
        ColKind::Bytes => Value::Bytes(key.to_vec()),
    })
}

/// 界表达式求值结果 → 索引键字节（`NULL` 界 = 无界）。
fn key_bytes(v: Option<&Value>) -> Result<Option<Vec<u8>>, ExecError> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => Ok(Some(n.encode())),
        Some(Value::Bool(b)) => Ok(Some(bicdb_types::encode_boolean(*b).to_vec())),
        Some(Value::Bytes(b)) => Ok(Some(b.clone())),
    }
}
