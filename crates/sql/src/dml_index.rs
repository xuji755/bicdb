//! **DML 的索引维护与唯一性预检**（`SQL前端设计` §2 `src/dml_index.rs`）。
//!
//! ```text
//! 写前：check_unique（同键条目 → 取行 → 重算键 → 逐字节比）⇒ DuplicateKey
//! 写中：写侧 insert_row 成功 ⇒ after_insert（每个可用索引一项 + 树头 redo）
//! ```
//!
//! **键字节与行内字节同源**（`catalog::row::key_from_row` 直取行内列字节）——
//! 索引比较即字节比较（`arch/06` §6.0）。
//!
//! ## 唯一性为什么在**写前**判、判据为什么是"重算键"
//!
//! - **不能只看槽位活否**：回滚留下的陈旧索引项（`arch/09` §9.1.2）可能指向
//!   一条**键不同**的新行（槽位复用），只看"活"会把合法插入误判成冲突；
//! - **不能只看索引项**：同一条目本身可能陈旧 ⇒ 必须**取 ROWID 的行、按行字节
//!   重算键**，与待插键逐字节比——这才是"真的有一条同键活行"；
//! - **写前**：预检与写在同一语句内**串行**（单写者），不必在写路径里回查字典
//!   （写路径持着数据文件，回查会与之别名）。
//!
//! **语句内/事务内的重复**由会话侧的**已见键集合**兜住（CR 看不见本事务未提交的
//! 行——`cr::reconstruct` 只认"已提交且 ≤ 快照"）：同一键在一条语句里出现两次、
//! 或一个显式事务里跨语句出现两次，都靠它判。

use std::collections::HashSet;

use bicdb_access::index as acc_index;
use bicdb_catalog::row;
use bicdb_catalog::{Catalog, DmlIndex};
use bicdb_common::seq::CommitSeq;
use bicdb_exec::{ExecError, IndexMaintenance};
use bicdb_storage::buffer::BufferPool;
use bicdb_storage::datafile::DataFile;
use bicdb_storage::rowid::RowId;
use bicdb_storage::scan;
use bicdb_storage::undo::UndoChain;
use bicdb_txn::write::Txn;
use bicdb_wal::group::GroupWriter;

/// **一张表的索引维护清单**（会话在语句开始时从目录取一次）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableIndexes {
    /// 可用索引（`Move` 后失效的不在内）。
    pub indexes: Vec<DmlIndex>,
}

impl TableIndexes {
    /// 建清单。
    #[must_use]
    pub fn new(indexes: Vec<DmlIndex>) -> Self {
        Self { indexes }
    }

    /// 有没有要维护的索引（没有 ⇒ 会话不必装维护口）。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.indexes.is_empty()
    }

    /// 有没有**唯一**索引（没有 ⇒ 不必做写前预检）。
    #[must_use]
    pub fn has_unique(&self) -> bool {
        self.indexes.iter().any(|i| i.unique)
    }
}

impl IndexMaintenance for TableIndexes {
    fn after_insert(
        &mut self,
        pool: &BufferPool<'_>,
        log: &mut GroupWriter<'_, '_>,
        file: &mut DataFile<'_>,
        ws: [u8; 8],
        txn: &Txn,
        rid: RowId,
        row_bytes: &[u8],
    ) -> Result<(), ExecError> {
        if self.indexes.is_empty() {
            return Ok(());
        }
        for idx in &self.indexes {
            let key = row::key_from_row(row_bytes, &idx.cols)
                .map_err(|e| ExecError::BadStoredRow(format!("索引键 {}：{e}", idx.name)))?;
            let new_root =
                acc_index::insert_entry(pool, log, file, ws, idx.seg_page0, txn, &key, rid)
                    .map_err(ExecError::TableAccess)?;
            acc_index::write_tree_head_redo(pool, log, file, ws, idx.seg_page0, txn, new_root)
                .map_err(ExecError::TableAccess)?;
        }
        Ok(())
    }
}

/// **已见键集合**（语句内/事务内的重复检测）——键 = `(索引对象号, 键字节)`。
pub type SeenKeys = HashSet<(u32, Vec<u8>)>;

/// **唯一性预检**（写前）：待写的行里，凡唯一索引的键与**活行**相撞 ⇒ 冲突。
///
/// `[`SeenKeys`]` 里已有的键 ⇒ 语句内/事务内重复（CR 看不见本事务未提交的行）。
#[allow(clippy::too_many_arguments)]
pub fn check_unique(
    cat: &mut Catalog<'_>,
    pool: &BufferPool<'_>,
    chain: &UndoChain<'_, '_>,
    snapshot: CommitSeq,
    indexes: &TableIndexes,
    rows: &[Vec<u8>],
    seen: &mut SeenKeys,
) -> Result<(), ExecError> {
    for row_bytes in rows {
        for idx in indexes.indexes.iter().filter(|i| i.unique) {
            let rk = row::row_key(row_bytes, &idx.cols)
                .map_err(|e| ExecError::BadStoredRow(format!("索引键 {}：{e}", idx.name)))?;
            if rk.has_null {
                continue; // 含 NULL 的键不参与唯一性（MySQL/PG 口径）
            }
            let key = rk.bytes;
            if !seen.insert((idx.obj, key.clone())) {
                return Err(duplicate(&idx.name, &key));
            }
            // 同键条目（叶区间 [key, key]）→ 逐个取行、重算键、逐字节比。
            let entries = cat
                .range_index(&idx.name, Some(&key), Some(&key))
                .map_err(|e| ExecError::BadStoredRow(format!("索引 {}：{e}", idx.name)))?;
            if entries.is_empty() {
                continue;
            }
            let rids: Vec<RowId> = entries.iter().map(|(_, rid)| *rid).collect();
            let found = scan::fetch_rows(pool, chain, snapshot, &rids)
                .map_err(|e| ExecError::BadStoredRow(format!("索引 {} 回表：{e}", idx.name)))?;
            for live in found.iter().flatten() {
                let hit = row::key_from_row(live, &idx.cols)
                    .map_err(|e| ExecError::BadStoredRow(e.to_string()))?;
                if hit == key {
                    return Err(duplicate(&idx.name, &key));
                }
            }
        }
    }
    Ok(())
}

fn duplicate(index: &str, key: &[u8]) -> ExecError {
    let mut hex = String::with_capacity(64);
    for b in key.iter().take(32) {
        hex.push_str(&format!("{b:02x}"));
    }
    ExecError::Index(bicdb_index::IndexError::DuplicateKey {
        key: format!("{index} {hex}"),
    })
}

/// 会话层取清单的薄壳（快照 + 表对象号）。
pub fn table_indexes(
    cat: &mut Catalog<'_>,
    snapshot: CommitSeq,
    table_obj: u32,
) -> Result<TableIndexes, ExecError> {
    let (_, indexes) = cat
        .dml_indexes(snapshot, table_obj)
        .map_err(|e| ExecError::BadStoredRow(format!("索引清单：{e}")))?;
    Ok(TableIndexes::new(indexes))
}
