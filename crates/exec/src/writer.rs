//! **表访问·写侧的真实现**（`TableWriter` → `bicdb_access::TableAccess`）。
//!
//! ```text
//! 执行器的 DML 算子 ── TableWriter ──▶ 本模块 ──▶ bicdb-access（页选址/增长）
//!                                             ├▶ bicdb-txn（ITL/锁/undo/redo）
//!                                             └▶ IndexMaintenance（会话层：索引项）
//! ```
//!
//! **为什么在 exec 而不在 access**：`TableWriter` 是**执行器侧的端口**（trait 在
//! [`crate::dml]`），实现放执行器侧才有正确的依赖方向（`exec → access`）。
//! 目录 DDL 不经本端口——它直接用 [`bicdb_access`] 的函数（同一份实现）。
//!
//! **两种事务归属**（`TableWriter::owns_txn`）：
//!
//! | 形态 | 构造 | 事务 | 谁提交 |
//! |---|---|---|---|
//! | 自持 | [`TableAccessWriter::new`] | 写侧自己 `begin`/`commit` | 算子（`owns_txn = true`） |
//! | 借用 | [`TableAccessWriter::with_txn`] | 会话层给的 `&mut Txn` | 会话（`owns_txn = false`） |
//!
//! 会话层（SQL 面）用**借用**形态：`BEGIN … COMMIT` 里的语句不得各自提交。
//!
//! **表增长终于可用**：本实现每次插入会**选址 → 必要时增长一页**（此前的执行器
//! 夹具只写"已存在的页"，表满即报错）。

use bicdb_access::TableAccess;
use bicdb_common::seq::CommitSeq;
use bicdb_storage::buffer::BufferPool;
use bicdb_storage::datafile::DataFile;
use bicdb_storage::heap::InsertPolicy;
use bicdb_storage::rowid::RowId;
use bicdb_storage::undo::UndoChain;
use bicdb_txn::write::{self, Txn};
use bicdb_wal::group::GroupWriter;

use crate::dml::{IndexMaintenance, TableWriter};
use crate::error::ExecError;

/// 目录声明的列约束（SQL 装配层注入；与页布局无关）。
#[derive(Debug, Clone)]
pub struct ColumnConstraint {
    /// 列名（错误定位）。
    pub name: String,
    /// 是否允许 NULL。
    pub nullable: bool,
    /// 字节串列的最大字节数；其他形态无长度约束。
    pub max_bytes: Option<usize>,
}

/// **真件表访问写口**（一个语句一个实例）。
pub struct TableAccessWriter<'a, 'b, 'io, 'f> {
    pool: &'a BufferPool<'b>,
    chain: &'a mut UndoChain<'io, 'f>,
    log: &'a mut GroupWriter<'io, 'f>,
    file: &'a mut DataFile<'io>,
    /// 表段头块（`seg$.block_id`）。
    heap_seg: u32,
    /// 工作区标识。
    ws: [u8; 8],
    /// **自持模式**的事务（借用模式为 `None`）。
    txn: Option<Txn>,
    /// **借用模式**的事务（会话层持有；自持模式为 `None`）。
    borrowed: Option<&'a mut Txn>,
    /// 自动提交序号（自持模式的提交序号）。
    seq: u64,
    /// 索引维护口（会话层给；`None` = 该语句表上没有索引维护需求）。
    indexes: Option<&'a mut dyn IndexMaintenance>,
    table: TableAccess<'a, 'b>,
    policy: InsertPolicy,
    constraints: Vec<ColumnConstraint>,
}

impl<'a, 'b, 'io, 'f> TableAccessWriter<'a, 'b, 'io, 'f> {
    /// **自持形态**（`heap_seg` = 表段头块；`seq` = 初始提交序号水位）。
    #[must_use]
    pub fn new(
        pool: &'a BufferPool<'b>,
        chain: &'a mut UndoChain<'io, 'f>,
        log: &'a mut GroupWriter<'io, 'f>,
        file: &'a mut DataFile<'io>,
        heap_seg: u32,
        ws: [u8; 8],
        seq: u64,
    ) -> Self {
        Self {
            pool,
            chain,
            log,
            file,
            heap_seg,
            ws,
            txn: None,
            borrowed: None,
            seq,
            indexes: None,
            table: TableAccess::new(pool, ws),
            policy: InsertPolicy::in_place(0),
            constraints: Vec::new(),
        }
    }

    /// **借用形态**（会话层持事务）：语句的提交/回滚由调用方做。
    #[must_use]
    pub fn with_txn(
        pool: &'a BufferPool<'b>,
        chain: &'a mut UndoChain<'io, 'f>,
        log: &'a mut GroupWriter<'io, 'f>,
        file: &'a mut DataFile<'io>,
        heap_seg: u32,
        ws: [u8; 8],
        txn: &'a mut Txn,
    ) -> Self {
        Self {
            pool,
            chain,
            log,
            file,
            heap_seg,
            ws,
            txn: None,
            borrowed: Some(txn),
            seq: 0,
            indexes: None,
            table: TableAccess::new(pool, ws),
            policy: InsertPolicy::in_place(0),
            constraints: Vec::new(),
        }
    }

    /// 装上**索引维护口**（表上有可用索引时由会话层给）。
    pub fn set_indexes(&mut self, indexes: &'a mut dyn IndexMaintenance) {
        self.indexes = Some(indexes);
    }

    /// SQL 写侧必须在开始写入前注入目标表的列约束。
    pub fn set_column_constraints(&mut self, constraints: Vec<ColumnConstraint>) {
        self.constraints = constraints;
    }

    fn validate_row(&self, row: &[u8]) -> Result<(), ExecError> {
        if self.constraints.is_empty() {
            return Ok(());
        }
        let view = bicdb_storage::row::RowView::new(row)
            .map_err(|e| ExecError::BadStoredRow(e.to_string()))?;
        for (i, c) in self.constraints.iter().enumerate() {
            if view.is_null(i as u16) {
                if !c.nullable {
                    return Err(ExecError::ConstraintViolation(format!(
                        "列 `{}` 违反非空约束（NOT NULL）",
                        c.name
                    )));
                }
            } else if let Some(max) = c.max_bytes {
                let bytes = view
                    .var_column(i, 0)
                    .ok_or(ExecError::RowShapeMismatch { col: i })?;
                if bytes.len() > max {
                    return Err(ExecError::ConstraintViolation(format!(
                        "列 `{}` 违反列长度约束：{} 字节超过声明上限 {max}",
                        c.name,
                        bytes.len()
                    )));
                }
            }
        }
        Ok(())
    }

    /// **表选项落到写侧策略**：`pctfree`（页内预留）+ `itl_max`（ITL 上限）。
    ///
    /// 两者来自 `tab$`（会话在语句开始时取），此前写路径恒用缺省值 —— 选项
    /// 只落字典不生效（2026-10-06 审计）。`itl_max = 0` 视为未设（用格式上限）。
    pub fn set_table_options(&mut self, pctfree: u8, itl_max: u16) {
        self.policy = if itl_max == 0 {
            InsertPolicy::in_place(pctfree)
        } else {
            InsertPolicy::in_place(pctfree).with_itl_max(itl_max)
        };
    }

    /// **借出当前事务**（两种模式统一入口）：事务字段**移出**再放回——
    /// 这样 `f` 里可以同时用 `self` 的其余字段（日志/文件/池），不与事务别名。
    fn use_txn<R>(
        &mut self,
        f: impl FnOnce(&mut Self, &mut Txn) -> Result<R, ExecError>,
    ) -> Result<R, ExecError> {
        let mut owned = self.txn.take();
        let mut borrowed = self.borrowed.take();
        let out = match (&mut owned, &mut borrowed) {
            (Some(t), _) => f(self, t),
            (_, Some(t)) => f(self, t),
            _ => Err(ExecError::NoWriter),
        };
        self.txn = owned;
        self.borrowed = borrowed;
        out
    }

    /// **行写后的索引维护**（先堆后索引；键里的 ROWID 来自堆写）。
    ///
    /// 维护口**移出再放回**（同 [`TableAccessWriter::with_txn`] 的理由：
    /// 口里的实现要用日志/文件，不能让它们与自己别名）。
    fn maintain_insert(&mut self, rid: RowId, row: &[u8]) -> Result<(), ExecError> {
        let Some(idx) = self.indexes.take() else {
            return Ok(());
        };
        let out = {
            let txn: &Txn = match (self.txn.as_ref(), self.borrowed.as_deref()) {
                (Some(t), _) => t,
                (_, Some(t)) => t,
                _ => {
                    self.indexes = Some(idx);
                    return Err(ExecError::NoWriter);
                }
            };
            idx.after_insert(self.pool, self.log, self.file, self.ws, txn, rid, row)
        };
        self.indexes = Some(idx);
        out
    }

    /// **行更新后的索引维护**（旧键删、新键插；键没变就什么都不做）。
    fn maintain_update(&mut self, rid: RowId, old: &[u8], new: &[u8]) -> Result<(), ExecError> {
        let Some(idx) = self.indexes.take() else {
            return Ok(());
        };
        let out = {
            let txn: &Txn = match (self.txn.as_ref(), self.borrowed.as_deref()) {
                (Some(t), _) => t,
                (_, Some(t)) => t,
                _ => {
                    self.indexes = Some(idx);
                    return Err(ExecError::NoWriter);
                }
            };
            idx.after_update(self.pool, self.log, self.file, self.ws, txn, rid, old, new)
        };
        self.indexes = Some(idx);
        out
    }

    /// **行删除后的索引维护**（删键；键从被删的行算）。
    fn maintain_delete(&mut self, rid: RowId, old: &[u8]) -> Result<(), ExecError> {
        let Some(idx) = self.indexes.take() else {
            return Ok(());
        };
        let out = {
            let txn: &Txn = match (self.txn.as_ref(), self.borrowed.as_deref()) {
                (Some(t), _) => t,
                (_, Some(t)) => t,
                _ => {
                    self.indexes = Some(idx);
                    return Err(ExecError::NoWriter);
                }
            };
            idx.after_delete(self.pool, self.log, self.file, self.ws, txn, rid, old)
        };
        self.indexes = Some(idx);
        out
    }
}

impl TableWriter for TableAccessWriter<'_, '_, '_, '_> {
    fn owns_txn(&self) -> bool {
        self.borrowed.is_none()
    }

    fn begin(&mut self) -> Result<(), ExecError> {
        if self.borrowed.is_some() {
            // 借用形态：事务归会话层——算子不该走到这里（`owns_txn` 已为 false）。
            return Err(ExecError::NoWriter);
        }
        self.seq += 1;
        let snapshot = CommitSeq::from_raw(self.seq).expect("48 位域内");
        let txn = write::begin(self.pool, self.log, self.chain, snapshot)
            .map_err(|e| ExecError::TableAccess(e.into()))?;
        self.txn = Some(txn);
        Ok(())
    }

    fn insert_row(&mut self, row: &[u8]) -> Result<RowId, ExecError> {
        self.validate_row(row)?;
        write::checkpoint_safe_point(self.pool, self.log, self.chain)
            .map_err(|e| ExecError::TableAccess(e.into()))?;
        let policy = self.policy;
        let heap_seg = self.heap_seg;
        let rid = self.use_txn(|s, txn| {
            s.table
                .insert(s.log, s.chain, txn, s.file, heap_seg, row, &policy)
                .map_err(ExecError::TableAccess)
        })?;
        // 先堆后索引（键里的 ROWID = 堆写的返回值）。
        self.maintain_insert(rid, row)?;
        Ok(rid)
    }

    fn update_row(&mut self, rid: RowId, old: &[u8], new: &[u8]) -> Result<(), ExecError> {
        self.validate_row(new)?;
        write::checkpoint_safe_point(self.pool, self.log, self.chain)
            .map_err(|e| ExecError::TableAccess(e.into()))?;
        let policy = self.policy;
        let heap_seg = self.heap_seg;
        // **先堆后索引**（与插入同序）：堆写真成功过才动索引。
        self.use_txn(|s, txn| {
            s.table
                .update(s.log, s.chain, txn, s.file, heap_seg, rid, new, &policy)
                .map_err(ExecError::TableAccess)
        })?;
        self.maintain_update(rid, old, new)
    }

    fn delete_row(&mut self, rid: RowId, old: &[u8]) -> Result<(), ExecError> {
        write::checkpoint_safe_point(self.pool, self.log, self.chain)
            .map_err(|e| ExecError::TableAccess(e.into()))?;
        // 删行按 ROWID 定位（不经段头）——段头由 `TableAccessWriter::with_txn` 持有。
        let policy = self.policy;
        self.use_txn(|s, txn| {
            s.table
                .delete(s.log, s.chain, txn, s.file, rid, &policy)
                .map_err(ExecError::TableAccess)
        })?;
        self.maintain_delete(rid, old)
    }

    fn commit(&mut self) -> Result<(), ExecError> {
        if self.borrowed.is_some() {
            return Err(ExecError::NoWriter);
        }
        let mut txn = self.txn.take().ok_or(ExecError::NoWriter)?;
        let seq = CommitSeq::from_raw(self.seq).expect("48 位域内");
        write::commit(self.pool, self.log, self.chain, &mut txn, seq)
            .map_err(|e| ExecError::TableAccess(e.into()))?;
        Ok(())
    }

    fn rollback(&mut self) -> Result<(), ExecError> {
        if self.borrowed.is_some() {
            return Err(ExecError::NoWriter);
        }
        if let Some(mut txn) = self.txn.take() {
            write::rollback(self.pool, self.log, self.chain, &mut txn)
                .map_err(|e| ExecError::TableAccess(e.into()))?;
        }
        Ok(())
    }
}
