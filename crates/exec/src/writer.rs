//! **表访问·写侧的真实现**（`TableWriter` → `bicdb_access::TableAccess`）。
//!
//! ```text
//! 执行器的 DML 算子 ── TableWriter ──▶ 本模块 ──▶ bicdb-access（页选址/增长）
//!                                              └▶ bicdb-txn（ITL/锁/undo/redo）
//! ```
//!
//! **为什么在 exec 而不在 access**：`TableWriter` 是**执行器侧的端口**（trait 在
//! [`crate::dml`]），实现放执行器侧才有正确的依赖方向（`exec → access`）。
//! 目录 DDL 不经本端口——它直接用 [`bicdb_access`] 的函数（同一份实现）。
//!
//! **语句 = 一个事务**：`begin` 开、`commit`/`rollback` 收（与 [`crate::dml`] 的
//! 既定纪律一致；`owns_txn = false` 的会话层形态随会话切片）。
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

use crate::dml::TableWriter;
use crate::error::ExecError;

/// **真件表访问写口**（一个语句一个实例）。
pub struct TableAccessWriter<'a, 'b, 'io, 'f> {
    pool: &'a BufferPool<'b>,
    chain: &'a mut UndoChain<'io, 'f>,
    log: GroupWriter<'io, 'f>,
    file: &'a mut DataFile<'io>,
    /// 表段头块（`seg$.block_id`）。
    heap_seg: u32,
    /// 自动提交序号（用例/会话层给的提交序号生成器）。
    seq: u64,
    txn: Option<Txn>,
    table: TableAccess<'a, 'b>,
    policy: InsertPolicy,
}

impl<'a, 'b, 'io, 'f> TableAccessWriter<'a, 'b, 'io, 'f> {
    /// 建写口（`heap_seg` = 表段头块；`seq` = 初始提交序号水位）。
    #[must_use]
    pub fn new(
        pool: &'a BufferPool<'b>,
        chain: &'a mut UndoChain<'io, 'f>,
        log: GroupWriter<'io, 'f>,
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
            seq,
            txn: None,
            table: TableAccess::new(pool, ws),
            policy: InsertPolicy::in_place(0),
        }
    }

    /// 表选项里的 `pctfree`（链到 [`bicdb_access`] 的选址判据）。
    pub fn set_pctfree(&mut self, pctfree: u8) {
        self.policy = InsertPolicy::in_place(pctfree);
    }
}

impl TableWriter for TableAccessWriter<'_, '_, '_, '_> {
    fn begin(&mut self) -> Result<(), ExecError> {
        self.seq += 1;
        let snapshot = CommitSeq::from_raw(self.seq).expect("48 位域内");
        let txn = write::begin(self.pool, &mut self.log, self.chain, snapshot)
            .map_err(|e| ExecError::TableAccess(e.into()))?;
        self.txn = Some(txn);
        Ok(())
    }

    fn insert_row(&mut self, row: &[u8]) -> Result<RowId, ExecError> {
        let policy = self.policy;
        let mut txn = self.txn.take().ok_or(ExecError::NoWriter)?;
        let out = self.table.insert(
            &mut self.log,
            self.chain,
            &mut txn,
            self.file,
            self.heap_seg,
            row,
            &policy,
        );
        self.txn = Some(txn);
        out.map_err(ExecError::TableAccess)
    }

    fn update_row(&mut self, rid: RowId, row: &[u8]) -> Result<(), ExecError> {
        let policy = self.policy;
        let mut txn = self.txn.take().ok_or(ExecError::NoWriter)?;
        let out = self.table.update(
            &mut self.log,
            self.chain,
            &mut txn,
            self.file,
            self.heap_seg,
            rid,
            row,
            &policy,
        );
        self.txn = Some(txn);
        out.map_err(ExecError::TableAccess)
    }

    fn delete_row(&mut self, rid: RowId) -> Result<(), ExecError> {
        let mut txn = self.txn.take().ok_or(ExecError::NoWriter)?;
        let out = self
            .table
            .delete(&mut self.log, self.chain, &mut txn, self.file, rid);
        self.txn = Some(txn);
        out.map_err(ExecError::TableAccess)
    }

    fn commit(&mut self) -> Result<(), ExecError> {
        let mut txn = self.txn.take().ok_or(ExecError::NoWriter)?;
        let seq = CommitSeq::from_raw(self.seq).expect("48 位域内");
        write::commit(self.pool, &mut self.log, self.chain, &mut txn, seq)
            .map_err(|e| ExecError::TableAccess(e.into()))?;
        Ok(())
    }

    fn rollback(&mut self) -> Result<(), ExecError> {
        if let Some(mut txn) = self.txn.take() {
            write::rollback(self.pool, &mut self.log, self.chain, &mut txn)
                .map_err(|e| ExecError::TableAccess(e.into()))?;
        }
        Ok(())
    }
}
