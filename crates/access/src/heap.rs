//! **堆表访问（写侧）**：页选址 + 表增长 + 行写转发（REQ-ENG-003）。
//!
//! ```text
//! insert：选址（提示页 → 扫描 → **增长**）→ txn::write::insert_row
//! update：按 ROWID 定位 → txn::write::update_row（迁移目标 = 既有页或**增长**）
//! delete：按 ROWID 定位 → txn::write::delete_row
//! ```
//!
//! **调用形状：按次开段**。`Segment<'io,'f>` 独占 `&mut DataFile` ⇒ 同一文件上
//! 的表段与索引段**不能同时持有**；本服务持 `(pool, file, ws)`，每次调用用
//! [`Segment::open_pooled`] 现开现弃（开段 = 一次**池内**页读——段头页 no-force，
//! 直读文件会拿到旧 `append_pos`，所以必须走池）。
//!
//! **选址策略（V1.0，记档）**：按 `pctfree` 判"放得下"（[`bicdb_storage::heap::can_insert`]），
//! 顺序：① 上次成功的页（提示）；② 段内数据页顺序扫；③ 增长。**不做空闲级别
//! 位图选页**——那是 Oracle 式空间复用优化，随"批量装载/回收"切片（`arch/04` §4.5）；
//! 目录表与 V1.0 用户表的规模下顺序扫足够（**代价记档**：大规模随机插入）。
//!
//! **增长**：新页 = 段追加位（跳过/物化段内位图页、必要时扩展段，全部经池 + redo）
//! → **物理格式化 → 先落盘 + fsync → 全页 redo → 池 + 标脏**。次序与撤销页/索引页
//! 共用 [`bicdb_txn::write::fresh_page_with_redo`] 一个实现。

use bicdb_storage::buffer::{BufferError, BufferKey, BufferPool};
use bicdb_storage::datafile::{DataFile, DataFileError};
use bicdb_storage::heap::{self, InsertPolicy};
use bicdb_storage::page::{Page, PageType};
use bicdb_storage::rowid::{Rdba, RowId};
use bicdb_storage::segment::{self, Segment, SegmentError, SegmentSpaceError};
use bicdb_storage::undo::UndoChain;
use bicdb_txn::write::{self, Txn, TxnError};
use bicdb_wal::group::GroupWriter;

/// 表访问错误。
#[derive(Debug)]
pub enum TableAccessError {
    /// 事务引擎（行写、redo、undo）。
    Txn(TxnError),
    /// 段空间/文件。
    Segment(SegmentSpaceError),
    /// 段头格式。
    SegmentHead(SegmentError),
    /// 数据文件。
    DataFile(DataFileError),
    /// 缓冲池。
    Pool(BufferError),
    /// 行在任何页都放不下（含新页——行超过单页容量）。
    RowTooLong {
        /// 行字节数。
        row_len: usize,
        /// 单页可容纳的上限（诊断）。
        limit: usize,
    },
    /// 行不存在（ROWID 指向空槽）。
    RowMissing(RowId),
}

impl std::fmt::Display for TableAccessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TableAccessError::Txn(e) => write!(f, "表访问·事务：{e}"),
            TableAccessError::Segment(e) => write!(f, "表访问·段：{e}"),
            TableAccessError::SegmentHead(e) => write!(f, "表访问·段头：{e}"),
            TableAccessError::DataFile(e) => write!(f, "表访问·文件：{e}"),
            TableAccessError::Pool(e) => write!(f, "表访问·池：{e}"),
            TableAccessError::RowTooLong { row_len, limit } => {
                write!(f, "行 {row_len} 字节超过单页上限 {limit}")
            }
            TableAccessError::RowMissing(rid) => write!(f, "ROWID {rid} 的行不存在"),
        }
    }
}

impl std::error::Error for TableAccessError {}

macro_rules! from_err {
    ($($v:ident <- $t:ty),* $(,)?) => {
        $(impl From<$t> for TableAccessError { fn from(e: $t) -> Self { Self::$v(e) } })*
    };
}
from_err!(
    Txn <- TxnError,
    Segment <- SegmentSpaceError,
    SegmentHead <- SegmentError,
    DataFile <- DataFileError,
    Pool <- BufferError,
);

/// **一张堆表的写侧访问口**（**按次开段、按次借文件**；提示页随口保存）。
///
/// 为什么文件也按次借：`Segment<'io,'f>` 独占 `&mut DataFile`，而同一语句里
/// 表段与索引段要在**同一个文件**上轮流开（见模块文档）——把 `&mut DataFile`
/// 存进本结构会把整条语句的其余借用全挡住。
pub struct TableAccess<'a, 'b> {
    pool: &'a BufferPool<'b>,
    ws: [u8; 8],
    /// 上次插入成功的块（顺序扫描的起点）。
    hint: Option<u32>,
}

impl<'a, 'b> TableAccess<'a, 'b> {
    /// 打开表访问口（`ws` = 工作区标识）。
    #[must_use]
    pub fn new(pool: &'a BufferPool<'b>, ws: [u8; 8]) -> Self {
        Self {
            pool,
            ws,
            hint: None,
        }
    }

    /// 工作区标识。
    #[must_use]
    pub fn workspace(&self) -> [u8; 8] {
        self.ws
    }

    fn key_of(&self, file_id: u16, block: u32) -> Result<BufferKey, TableAccessError> {
        let rdba = Rdba::from_parts(file_id, block)
            .ok_or(TableAccessError::Segment(SegmentSpaceError::BitmapCoverage))?;
        Ok(BufferKey::new(self.ws, rdba))
    }

    /// 一张页的当前镜像（池优先；未命中退化为直读）。
    fn page_image(&self, file: &DataFile<'_>, block: u32) -> Result<Page, TableAccessError> {
        match self.pool.pin(self.key_of(file.file_id(), block)?) {
            Ok(g) => Ok(Page::from_bytes(Box::new(*g.as_bytes()))),
            Err(BufferError::Unresolved { .. }) => Ok(file.read_page(block)?),
            Err(e) => Err(e.into()),
        }
    }

    /// **插入一行**（选址 → 增长 → 事务引擎行写）。
    #[allow(clippy::too_many_arguments)]
    pub fn insert(
        &mut self,
        log: &mut GroupWriter<'_, '_>,
        chain: &mut UndoChain<'_, '_>,
        txn: &mut Txn,
        file: &mut DataFile<'_>,
        seg_page0: u32,
        row: &[u8],
        policy: &InsertPolicy,
    ) -> Result<RowId, TableAccessError> {
        let block = self.space_for_row(log, txn, file, seg_page0, row.len(), policy)?;
        let rid = write::insert_row(
            self.pool,
            log,
            chain,
            txn,
            self.key_of(file.file_id(), block)?,
            row,
            policy,
        )?;
        Ok(rid)
    }

    /// **选址**：返回一张放得下 `row_len` 的块；找不到 ⇒ **增长**。
    ///
    /// 行超过单页容量 ⇒ [`TableAccessError::RowTooLong`]（不增长空页）。
    #[allow(clippy::too_many_arguments)]
    pub fn space_for_row(
        &mut self,
        log: &mut GroupWriter<'_, '_>,
        txn: &Txn,
        file: &mut DataFile<'_>,
        seg_page0: u32,
        row_len: usize,
        policy: &InsertPolicy,
    ) -> Result<u32, TableAccessError> {
        let probe = Page::new(PageType::HeapTable, self.ws, file.file_id(), 0);
        let capacity = heap::capacity_for_row(&probe, policy);
        if row_len > capacity {
            return Err(TableAccessError::RowTooLong {
                row_len,
                limit: capacity,
            });
        }
        // ① 提示页。
        if let Some(b) = self.hint {
            if self.fits(file, b, row_len, policy)? {
                return Ok(b);
            }
        }
        // ② 段内数据页顺序扫（HWM 为界）。
        let blocks = {
            let seg = Segment::open_pooled(self.pool, file, seg_page0, self.ws)?;
            let hwm = seg.hwm();
            seg.data_blocks(hwm)
        };
        for b in blocks {
            if self.fits(file, b, row_len, policy)? {
                self.hint = Some(b);
                return Ok(b);
            }
        }
        // ③ 增长。
        let block = self.allocate_page(log, txn, file, seg_page0)?;
        self.hint = Some(block);
        Ok(block)
    }

    fn fits(
        &self,
        file: &DataFile<'_>,
        block: u32,
        row_len: usize,
        policy: &InsertPolicy,
    ) -> Result<bool, TableAccessError> {
        let page = self.page_image(file, block)?;
        if page.header().map(|h| h.page_type) != Some(PageType::HeapTable) {
            return Ok(false);
        }
        Ok(heap::can_insert(&page, row_len, policy))
    }

    /// **增长一页**：段追加位 → 格式化 → 先落盘 + fsync → 全页 redo → 池。返回块号。
    #[allow(clippy::too_many_arguments)]
    pub fn allocate_page(
        &mut self,
        log: &mut GroupWriter<'_, '_>,
        txn: &Txn,
        file: &mut DataFile<'_>,
        seg_page0: u32,
    ) -> Result<u32, TableAccessError> {
        let pool = self.pool;
        let ws = self.ws;
        let fid = file.file_id();
        let key_of = |block: u32| -> Result<BufferKey, TableAccessError> {
            let rdba = Rdba::from_parts(fid, block)
                .ok_or(TableAccessError::Segment(SegmentSpaceError::BitmapCoverage))?;
            Ok(BufferKey::new(ws, rdba))
        };
        let mut seg = Segment::open_pooled(pool, file, seg_page0, ws)?;
        let logical = next_append_logical(pool, log, &mut seg, txn, ws)?;
        // 推进 append_pos（并抬 hwm）——页差异 redo。
        let (before, after) = {
            let mut current = move |block: u32| -> Option<Page> {
                let rdba = Rdba::from_parts(fid, block)?;
                let g = pool.pin(BufferKey::new(ws, rdba)).ok()?;
                Some(Page::from_bytes(Box::new(*g.as_bytes())))
            };
            seg.plan_advance_append(logical + 1, &mut current)?
        };
        write::write_page_change(
            pool,
            log,
            txn.raw(),
            key_of(seg.page0_block())?,
            before.as_bytes(),
            after.as_bytes(),
            false,
        )?;
        let block = seg
            .logical_block(logical)
            .ok_or(TableAccessError::Segment(SegmentSpaceError::BitmapCoverage))?;
        // 全新页：格式化（空堆表页）→ 先落盘 + fsync → 全页 redo。
        let fresh = Page::new(PageType::HeapTable, ws, fid, block);
        let mut physical = |page: &Page| -> Result<(), TableAccessError> {
            let mut p = Page::from_bytes(Box::new(*page.as_bytes()));
            seg.write_physical_page(block, &mut p)?;
            seg.sync()?;
            Ok(())
        };
        write::fresh_page_with_redo(pool, log, txn.raw(), key_of(block)?, &fresh, &mut physical)?;
        Ok(block)
    }

    /// **按 ROWID 改一行**（迁移目标：既有页 → 找不到则**增长一页**再试）。
    ///
    /// 两轮策略的**安全性依据**：`update_row` 的迁移口在**任何变更之前**被调用
    /// （`alloc` 在全部 ITL/undo 之前）⇒ `NoMigrationTarget` 失败后重试是安全的。
    #[allow(clippy::too_many_arguments)]
    pub fn update(
        &mut self,
        log: &mut GroupWriter<'_, '_>,
        chain: &mut UndoChain<'_, '_>,
        txn: &mut Txn,
        file: &mut DataFile<'_>,
        seg_page0: u32,
        rid: RowId,
        row: &[u8],
        policy: &InsertPolicy,
    ) -> Result<(), TableAccessError> {
        let fid = file.file_id();
        let pool = self.pool;
        let ws = self.ws;
        let probe = Page::new(PageType::HeapTable, self.ws, fid, 0);
        let capacity = heap::capacity_for_row(&probe, policy);
        let mut grown: Option<u32> = None;
        loop {
            let candidates: Vec<u32> = {
                let seg = Segment::open_pooled(pool, file, seg_page0, self.ws)?;
                let hwm = seg.hwm();
                seg.data_blocks(hwm)
            };
            let candidates: Vec<u32> = candidates
                .into_iter()
                .filter(|b| *b != rid.block_id())
                .filter(|b| self.fits(file, *b, row.len(), policy).unwrap_or(false))
                .collect();
            let growth = grown;
            let mut migrate = |need: usize| -> Result<BufferKey, TxnError> {
                let block = growth.or_else(|| candidates.first().copied());
                match block {
                    Some(b) => {
                        let rdba = Rdba::from_parts(fid, b);
                        match rdba {
                            Some(r) => Ok(BufferKey::new(ws, r)),
                            None => Err(TxnError::NoMigrationTarget { need }),
                        }
                    }
                    None => Err(TxnError::NoMigrationTarget { need }),
                }
            };
            let outcome = write::update_row(
                pool,
                log,
                chain,
                txn,
                self.key_of(fid, rid.block_id())?,
                rid.row_id(),
                row,
                policy,
                &mut migrate,
            );
            match outcome {
                Ok(_) => return Ok(()),
                Err(TxnError::NoMigrationTarget { need })
                    if grown.is_none() && need <= capacity =>
                {
                    grown = Some(self.allocate_page(log, txn, file, seg_page0)?);
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// **按 ROWID 删一行**。
    pub fn delete(
        &mut self,
        log: &mut GroupWriter<'_, '_>,
        chain: &mut UndoChain<'_, '_>,
        txn: &mut Txn,
        file: &DataFile<'_>,
        rid: RowId,
    ) -> Result<(), TableAccessError> {
        write::delete_row(
            self.pool,
            log,
            chain,
            txn,
            self.key_of(file.file_id(), rid.block_id())?,
            rid.row_id(),
        )?;
        Ok(())
    }
}

/// **下一个可写逻辑页**（跳过/物化段内位图页；必要时扩展段）——每一步立即入 redo。
///
/// 与 [`bicdb_txn::index_io`] 的同名逻辑同规：**在飞覆盖层**由 storage 的计划器
/// 内部维护（见 `plan_materialize_bitmap_page` 的注记），这里只负责"计划 → 立即
/// 写 redo"，让下一轮计划的读-改-写基准（池像）看到上一轮结果。
pub fn next_append_logical(
    pool: &BufferPool<'_>,
    log: &mut GroupWriter<'_, '_>,
    seg: &mut Segment<'_, '_>,
    txn: &Txn,
    ws: [u8; 8],
) -> Result<u32, TableAccessError> {
    let file_id = seg.file_id();
    loop {
        let header_page = match pool.pin(BufferKey::new(
            ws,
            Rdba::from_parts(file_id, seg.page0_block())
                .ok_or(TableAccessError::Segment(SegmentSpaceError::BitmapCoverage))?,
        )) {
            Ok(g) => Page::from_bytes(Box::new(*g.as_bytes())),
            Err(_) => seg.read_page(0)?,
        };
        let h = segment::read_header(&header_page)?;
        let logical = h.append_pos;
        if seg.is_bitmap_page(logical) {
            let (i, _, bmp_logical) = seg.bitmap_slot(logical);
            while seg.logical_block(bmp_logical).is_none() {
                let planned = {
                    let mut current = {
                        move |block: u32| -> Option<Page> {
                            let rdba = Rdba::from_parts(file_id, block)?;
                            let g = pool.pin(BufferKey::new(ws, rdba)).ok()?;
                            Some(Page::from_bytes(Box::new(*g.as_bytes())))
                        }
                    };
                    seg.plan_extend(&mut current)?
                };
                apply_images(pool, log, txn, ws, &planned.images)?;
            }
            let adv = {
                let mut current = move |block: u32| -> Option<Page> {
                    let rdba = Rdba::from_parts(file_id, block)?;
                    let g = pool.pin(BufferKey::new(ws, rdba)).ok()?;
                    Some(Page::from_bytes(Box::new(*g.as_bytes())))
                };
                seg.plan_materialize_bitmap_page(i, &mut current)?
            };
            // 全新位图页：物理格式化 + fsync，再进 redo。
            for (rdba, page) in &adv.fresh {
                let mut p = Page::from_bytes(Box::new(*page.as_bytes()));
                seg.write_physical_page(rdba.block_id(), &mut p)?;
            }
            seg.sync()?;
            for (rdba, page) in &adv.fresh {
                let key = BufferKey::new(ws, *rdba);
                let mut physical = |p: &Page| -> Result<(), TableAccessError> {
                    let mut q = Page::from_bytes(Box::new(*p.as_bytes()));
                    seg.write_physical_page(rdba.block_id(), &mut q)?;
                    Ok(())
                };
                write::fresh_page_with_redo(pool, log, txn.raw(), key, page, &mut physical)?;
            }
            apply_images(pool, log, txn, ws, &adv.images)?;
            continue;
        }
        if seg.logical_block(logical).is_none() {
            let planned = {
                let mut current = move |block: u32| -> Option<Page> {
                    let rdba = Rdba::from_parts(file_id, block)?;
                    let g = pool.pin(BufferKey::new(ws, rdba)).ok()?;
                    Some(Page::from_bytes(Box::new(*g.as_bytes())))
                };
                seg.plan_extend(&mut current)?
            };
            apply_images(pool, log, txn, ws, &planned.images)?;
            continue;
        }
        return Ok(logical);
    }
}

/// 一页一页把计划镜像写进 redo（系统操作）。
pub fn apply_images(
    pool: &BufferPool<'_>,
    log: &mut GroupWriter<'_, '_>,
    txn: &Txn,
    ws: [u8; 8],
    images: &[(Rdba, Page, Page)],
) -> Result<(), TableAccessError> {
    for (rdba, before, after) in images {
        write::write_page_change(
            pool,
            log,
            txn.raw(),
            BufferKey::new(ws, *rdba),
            before.as_bytes(),
            after.as_bytes(),
            false,
        )?;
    }
    Ok(())
}
