//! **带 redo 的索引写口**（`bicdb_index::IndexIo` 的生产实现；C4 的基础件）。
//!
//! ```text
//! 树的三个动作 → 本模块的三个落点：
//!   allocate_page()          段空间分配：**计划形态 + 经池 + redo**（§11.5.3 系统操作）
//!   apply_page(block, after) 页内容写入：页差异 redo → 池 → 标脏（数据页同规）
//!   allocated_blocks()       段内数据页的物理序块号（FFS / validate 用）
//! ```
//!
//! # 三条纪律（与撤销页/数据页同规，逐条有据）
//!
//! 1. **系统操作也受 redo 保护**：段头页（`append_pos`/`hwm`/区映射）、段内
//!    位图页、文件级位图页的每一次改动都走 `plan_*` 计划形态 + 页差异 redo——
//!    **不直写**（直写会与池像分叉，崩溃后空间账目错乱）；
//! 2. **全新页：先格式化落盘 + fsync、再让它进 redo**（§11.5.4 实现注记，
//!    与"新撤销页"同规）——物理增量重放**无法重建一个不存在的页**；次序反过来
//!    则安全：崩溃只可能留下"无人引用的已格式化页"（`append_pos` 未推进）；
//! 3. **`page_lsn` 前置到"当前追加位的前一字节"**：块可能**被复用**（段回收后
//!    重分配），上一轮生命周期的 redo 记录 LSN 更小——不前置（= 0）会把旧记录
//!    字节"复活"到新页上。`−1` 的理由与撤销页完全相同（`txn::write` 的注记：
//!    `appended_lsn()` 是**下一条记录的 LSN**，stamp 成它会把本页本轮的第一条
//!    记录一并跳掉）。
//!
//! **一次一个页**（§9.1.5）：每次调用只碰一页；池的卫兵在调用内取放。

use std::collections::BTreeSet;

use bicdb_index::{IndexError, IndexIo};
use bicdb_storage::buffer::{BufferKey, BufferPool};
use bicdb_storage::page::Page;
use bicdb_storage::rowid::Rdba;
use bicdb_storage::segment::{self, Segment};
use bicdb_wal::group::GroupWriter;

use crate::write::{self, Txn};
use bicdb_storage::undo::UndoChain;

/// 索引写口的错误包装（`IndexIo` 的既有约定：字符串化的 I/O 错误）。
fn io_err(what: impl std::fmt::Display) -> IndexError {
    IndexError::Io(what.to_string())
}

fn rdba_of(file_id: u16, block: u32) -> Result<Rdba, IndexError> {
    Rdba::from_parts(file_id, block).ok_or(IndexError::Malformed("块号越出 RDBA 域"))
}

/// **事务化的索引写口**：把 `bicdb_index` 的分配/写页动作落到
/// 段空间管理 + 缓冲池 + WAL 上。
///
/// **生命周期参数**（六个：写口同时借日志与段，两者各自的文件借用相互独立）：
/// - `'a` = 本写口的借用期；`'b` = 池的文件借用；
/// - `'w`/`'wc` = 日志写口的（文件、控制文件）借用；
/// - `'s`/`'sf` = 索引段的（文件、段文件借用）借用。
pub struct TxnIndexIo<'a, 'b, 'w, 'wc, 's, 'sf, 'u, 'uf> {
    pool: &'a BufferPool<'b>,
    log: &'a mut GroupWriter<'w, 'wc>,
    seg: &'a mut Segment<'s, 'sf>,
    /// 产生者事务的原始三段式标识（redo 记录头）。
    txn_raw: u64,
    ws: [u8; 8],
    /// **本次会话分配出的新块**（`apply_page` 据此走"全新页"分支；见模块文档③）。
    fresh: BTreeSet<u32>,
    checkpoint_chain: Option<&'a UndoChain<'u, 'uf>>,
}

impl<'a, 'b, 'w, 'wc, 's, 'sf, 'u, 'uf> TxnIndexIo<'a, 'b, 'w, 'wc, 's, 'sf, 'u, 'uf> {
    /// 打开写口（`txn` = 产生这些索引页修改的事务）。
    pub fn new(
        pool: &'a BufferPool<'b>,
        log: &'a mut GroupWriter<'w, 'wc>,
        seg: &'a mut Segment<'s, 'sf>,
        txn: &Txn,
    ) -> Self {
        let ws = seg.workspace_ref();
        Self {
            pool,
            log,
            seg,
            txn_raw: txn.raw(),
            ws,
            fresh: BTreeSet::new(),
            checkpoint_chain: None,
        }
    }

    /// Open a bulk-build writer that can checkpoint between completed pages.
    pub fn new_checkpointed(
        pool: &'a BufferPool<'b>,
        log: &'a mut GroupWriter<'w, 'wc>,
        chain: &'a UndoChain<'u, 'uf>,
        seg: &'a mut Segment<'s, 'sf>,
        txn: &Txn,
    ) -> Self {
        let mut io = Self::new(pool, log, seg, txn);
        io.checkpoint_chain = Some(chain);
        io
    }

    /// **一张页的当前镜像**（池优先；未命中直读段文件，未初始化 ⇒ `None`）。
    fn current(&self, block: u32) -> Option<Page> {
        let rdba = Rdba::from_parts(self.seg.file_id(), block)?;
        match self.pool.pin(BufferKey::new(self.ws, rdba)) {
            Ok(g) => Some(Page::from_bytes(Box::new(*g.as_bytes()))),
            Err(_) => self.seg.read_physical_page(block),
        }
    }

    /// 页差异 → redo → 池（`write_page_change` 的薄包装）。
    fn write_change(
        &mut self,
        rdba: Rdba,
        before: &Page,
        after: &Page,
        is_new: bool,
    ) -> Result<(), IndexError> {
        let key = BufferKey::new(self.ws, rdba);
        write::write_page_change(
            self.pool,
            self.log,
            self.txn_raw,
            key,
            before.as_bytes(),
            after.as_bytes(),
            is_new,
        )
        .map_err(io_err)?;
        Ok(())
    }

    /// **分配一个可写的数据页**（跳过并物化段内位图页；必要时扩展段）。
    fn allocate(&mut self) -> Result<u32, IndexError> {
        loop {
            // 段头页的当前像（池优先）——`append_pos` 以它为准（no-force 纪律）。
            let header_page = self
                .current(self.seg.page0_block())
                .ok_or_else(|| io_err("索引段头页不可读"))?;
            let h = segment::read_header(&header_page).map_err(io_err)?;
            let logical = h.append_pos;

            if self.seg.is_bitmap_page(logical) {
                // 记录"连续扩展出来的既有页"与"全新位图页"两类镜像。
                let (i, _, _) = self.seg.bitmap_slot(logical);
                let adv = {
                    let pool = self.pool;
                    let ws = self.ws;
                    let file_id = self.seg.file_id();
                    let mut current = move |block: u32| -> Option<Page> {
                        let rdba = Rdba::from_parts(file_id, block)?;
                        let g = pool.pin(BufferKey::new(ws, rdba)).ok()?;
                        Some(Page::from_bytes(Box::new(*g.as_bytes())))
                    };
                    self.seg
                        .plan_materialize_bitmap_page(i, &mut current)
                        .map_err(io_err)?
                };
                // 全新位图页：**先格式化落盘 + fsync**（纪律 2）。
                for (rdba, page) in &adv.fresh {
                    let mut p = Page::from_bytes(Box::new(*page.as_bytes()));
                    self.seg
                        .write_physical_page(rdba.block_id(), &mut p)
                        .map_err(io_err)?;
                }
                self.seg.sync().map_err(io_err)?;
                for (rdba, before, after) in &adv.images {
                    self.write_change(*rdba, before, after, false)?;
                }
                continue;
            }

            if self.seg.logical_block(logical).is_none() {
                // 下一个可写页还没有物理块 ⇒ 先计划扩展（redo 保护）。
                let planned = {
                    let pool = self.pool;
                    let ws = self.ws;
                    let file_id = self.seg.file_id();
                    let mut current = move |block: u32| -> Option<Page> {
                        let rdba = Rdba::from_parts(file_id, block)?;
                        let g = pool.pin(BufferKey::new(ws, rdba)).ok()?;
                        Some(Page::from_bytes(Box::new(*g.as_bytes())))
                    };
                    self.seg.plan_extend(&mut current).map_err(io_err)?
                };
                for (rdba, before, after) in &planned.images {
                    self.write_change(*rdba, before, after, false)?;
                }
                continue;
            }

            // 推进 `append_pos`（并抬 `hwm`）——系统操作，同样走 redo。
            let (before, after) = {
                let pool = self.pool;
                let ws = self.ws;
                let file_id = self.seg.file_id();
                let mut current = move |block: u32| -> Option<Page> {
                    let rdba = Rdba::from_parts(file_id, block)?;
                    let g = pool.pin(BufferKey::new(ws, rdba)).ok()?;
                    Some(Page::from_bytes(Box::new(*g.as_bytes())))
                };
                self.seg
                    .plan_advance_append(logical + 1, &mut current)
                    .map_err(io_err)?
            };
            let page0 = rdba_of(self.seg.file_id(), self.seg.page0_block())?;
            self.write_change(page0, &before, &after, false)?;

            let block = self
                .seg
                .logical_block(logical)
                .ok_or_else(|| io_err("逻辑页无物理块"))?;
            self.fresh.insert(block);
            return Ok(block);
        }
    }

    /// **写一页内容**（新页 ⇒ 格式化 + 全页 redo；既有页 ⇒ 差异 redo）。
    fn apply(&mut self, block: u32, after: &Page) -> Result<(), IndexError> {
        let file_id = self.seg.file_id();
        let rdba = rdba_of(file_id, block)?;
        let is_fresh = self.fresh.remove(&block) || self.current(block).is_none();
        if is_fresh {
            // **全新页三件套**（先格式化落盘 + fsync、再进 redo）——与撤销页/
            // 堆表页共用 `txn::write::fresh_page_with_redo` 一个实现。
            let mut physical = |page: &Page| -> Result<(), IndexError> {
                let mut p = Page::from_bytes(Box::new(*page.as_bytes()));
                self.seg
                    .write_physical_page(block, &mut p)
                    .map_err(|e| IndexError::Io(e.to_string()))?;
                self.seg.sync().map_err(|e| IndexError::Io(e.to_string()))
            };
            let key = BufferKey::new(self.ws, rdba);
            write::fresh_page_with_redo(
                self.pool,
                self.log,
                self.txn_raw,
                key,
                after,
                &mut physical,
            )
            .map_err(io_err)?;
        } else {
            let before = self
                .current(block)
                .ok_or_else(|| io_err("既有索引页不可读"))?;
            self.write_change(rdba, &before, after, false)?;
        }
        Ok(())
    }
}

impl IndexIo for TxnIndexIo<'_, '_, '_, '_, '_, '_, '_, '_> {
    fn safe_point(&mut self) -> Result<(), IndexError> {
        if let Some(chain) = self.checkpoint_chain {
            write::checkpoint_safe_point(self.pool, self.log, chain).map_err(io_err)?;
        }
        Ok(())
    }
    fn allocate_page(&mut self) -> Result<u32, IndexError> {
        self.allocate()
    }

    fn apply_page(&mut self, block: u32, after: &Page) -> Result<(), IndexError> {
        self.apply(block, after)
    }

    fn allocated_blocks(&mut self) -> Result<Vec<u32>, IndexError> {
        Ok(self.seg.data_blocks(self.seg.hwm()))
    }
}

/// **索引维护的便利口**（写路径四行内联的说明）：
///
/// ```ignore
/// let mut io = TxnIndexIo::new(pool, log, seg, txn);
/// let mut store = PoolStore::new(pool, &mut io, file_id, ws);
/// let mut tree = Tree::open(&mut store, file_id, root)?;
/// tree.insert(&key, rid)?;          // 或 tree.delete(&key)?
/// ```
///
/// **不提供包装函数**：`Tree` 借 `PoolStore`、`PoolStore` 借 `TxnIndexIo`——
/// 三者绑在一个作用域里最省事；包装出去的返回类型只会把生命周期写得更难读。
#[cfg(test)]
mod tests {
    use std::path::Path;

    use bicdb_common::seq::Lsn;
    use bicdb_index::Tree;
    use bicdb_storage::buffer::{BufferPool, CacheConfig, SystemClock, WalGuard};
    use bicdb_storage::controlfile::{
        ArchiveMode, ArchiveRecord, ControlFile, RedoEntries, WorkspaceEntry,
    };
    use bicdb_storage::datafile::DataFile;
    use bicdb_storage::rowid::RowId;
    use bicdb_storage::segment::SegType;
    use bicdb_storage::undo::{create_undo_segment, UndoChain};
    use bicdb_wal::group::GroupSpec;
    use bicdb_wal::recovery::redo_from;
    use bicdb_workspace::io::MemFileIo;
    use bicdb_workspace::WorkspaceId;

    use super::*;
    use crate::write::{begin, commit};

    const WS: [u8; 8] = [5u8; 8];
    const UNDO_F: &str = "/mem/undo_idx.dat";
    const DATA_F: &str = "/mem/idx.dat";
    const WAL: &str = "/mem/wal_idx";
    const CF_A: &str = "/mem/cf_idx_a";
    const CF_B: &str = "/mem/cf_idx_b";
    const DATA_FID: u16 = 3;
    /// 段的段内位图页覆盖：**压到 4**（测试专用口）——3 张数据页后
    /// `append_pos` 就撞上窗口首位（逻辑页 4 的位图页），物化路径可达。
    const TEST_COVERAGE: u32 = 4;
    /// 插入行数：足够跨"位图页物化 + 段扩展"（每叶页约 600 项；首区 8 块
    /// ⇒ 第 4 张数据页撞位图页、第 6 张之后需要扩展段）。
    const ROWS: u32 = 6_000;
    /// 日志组容量（页）：整棵树的 redo 量在 1 MiB 量级。
    const LOG_PAGES: u32 = 1024;

    fn seq(v: u64) -> bicdb_common::seq::CommitSeq {
        bicdb_common::seq::CommitSeq::from_raw(v).unwrap()
    }

    fn lsn(v: u64) -> Lsn {
        Lsn::from_raw(v).unwrap()
    }

    struct FakeWal;
    impl WalGuard for FakeWal {
        fn durable_lsn(&self) -> Lsn {
            Lsn::from_raw(u64::MAX >> 16).unwrap()
        }
        fn ensure_durable(&self, _t: Lsn) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// 键 = 保序数值编码 + 两字节尾（19B 量级；一叶页约 600 项）。
    fn key_of(i: u32) -> Vec<u8> {
        let n = bicdb_types::Number::parse(&i.to_string()).unwrap().encode();
        [n, vec![0xAB, (i % 251) as u8]].concat()
    }

    fn rid_of(i: u32) -> RowId {
        RowId::from_parts(DATA_FID, 100 + i, 1).unwrap()
    }

    #[test]
    fn index_pages_carry_redo_and_survive_a_crash() {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let spec = GroupSpec::new(2, 1, LOG_PAGES).unwrap();

        // ── 建文件与段（建段期直写：段头/段内位图初值）──
        let seg_page0;
        let data_handle;
        let root_out: RowId;
        {
            let mut undo_file = DataFile::create(&io, Path::new(UNDO_F), 1, 1, WS, 512).unwrap();
            let undo_handle = undo_file.handle();
            let undo_seg = create_undo_segment(&mut undo_file, 2, 3, 4).unwrap();

            let mut data_file =
                DataFile::create(&io, Path::new(DATA_F), DATA_FID, 3, WS, 2048).unwrap();
            data_handle = data_file.handle();
            let mut idx_seg = bicdb_storage::segment::Segment::create(
                &mut data_file,
                SegType::BTree,
                9,
                9,
                8,
                0,
                0,
            )
            .unwrap();
            idx_seg = idx_seg.with_coverage(TEST_COVERAGE);
            seg_page0 = idx_seg.page0_block();

            let pool = BufferPool::with_config(
                &io,
                64,
                move |_ws, r| match r.file_id() {
                    1 => Some((undo_handle, r.block_id())),
                    DATA_FID => Some((data_handle, r.block_id())),
                    _ => None,
                },
                FakeWal,
                SystemClock,
                CacheConfig::for_capacity(64),
            )
            .unwrap();
            let mut chain = UndoChain::open(undo_seg).with_pool(&pool);
            let mut cf = ControlFile::format(
                &io,
                Path::new(CF_A),
                Path::new(CF_B),
                &WorkspaceEntry {
                    workspace_id: WorkspaceId::from_raw(1).unwrap(),
                    created_at: 0,
                    derived_from: None,
                    derived_at_seq: seq(0),
                },
                &RedoEntries::new(2, 1).unwrap(),
                &ArchiveRecord::new(ArchiveMode::NoArchive),
            )
            .unwrap();
            let mut log = GroupWriter::create(&io, &mut cf, Path::new(WAL), spec, lsn(0)).unwrap();

            // ── 建树 + 灌入 ──
            let mut txn = begin(&pool, &mut log, &mut chain, seq(1)).unwrap();
            let root = {
                let mut ixio = TxnIndexIo::new(&pool, &mut log, &mut idx_seg, &txn);
                let mut store = bicdb_index::PoolStore::new(&pool, &mut ixio, DATA_FID, WS);
                let mut tree = Tree::create(&mut store, DATA_FID, WS).unwrap();
                for i in 0..ROWS {
                    tree.insert(&key_of(i), rid_of(i)).unwrap();
                }
                tree.validate().unwrap();
                for i in (0..ROWS).step_by(97) {
                    assert_eq!(
                        tree.lookup(&key_of(i)).unwrap(),
                        Some(rid_of(i)),
                        "灌入后点查命中：{i}"
                    );
                }
                tree.root()
            };
            root_out = root;
            // Use the same pooled/WAL path as production. Direct file writes
            // behind a dirty cached header would be overwritten by checkpoint.
            {
                let key = BufferKey::new(WS, rdba_of(DATA_FID, seg_page0).unwrap());
                let before = *pool.pin(key).unwrap().as_bytes();
                let mut header = Page::from_bytes(Box::new(before));
                bicdb_storage::segment::write_tree_head(&mut header, root).unwrap();
                write::write_page_change(
                    &pool,
                    &mut log,
                    txn.raw(),
                    key,
                    &before,
                    header.as_bytes(),
                    false,
                )
                .unwrap();
            }
            commit(&pool, &mut log, &mut chain, &mut txn, seq(1)).unwrap();

            // ── 崩溃：日志刷出、缓存丢弃（页不回写）──
            log.flush(log.appended_lsn()).unwrap();
            drop(chain); // 链借了池
            drop(pool);
            drop(log);
        }

        // ── 恢复：只重放日志（已提交 ⇒ 不需要撤销）──
        let cf_ro = ControlFile::open(&io, Path::new(CF_A), Path::new(CF_B)).unwrap();
        let groups = bicdb_wal::group::online_groups(&io, &cf_ro, Path::new(WAL), spec).unwrap();
        let reopened = DataFile::open(&io, Path::new(DATA_F)).unwrap();
        let data_handle2 = reopened.handle();
        // `begin` 在撤销段头页上分配了事务表槽 ⇒ 重放也覆盖撤销文件。
        let undo_reopened = DataFile::open(&io, Path::new(UNDO_F)).unwrap();
        let undo_handle2 = undo_reopened.handle();
        let mut resolve = |r: Rdba| match r.file_id() {
            1 => Some((undo_handle2, r.block_id())),
            DATA_FID => Some((data_handle2, r.block_id())),
            _ => None,
        };
        let report = redo_from(&io, &groups, lsn(0), &mut resolve).unwrap();
        assert!(
            report.records_applied > ROWS as usize,
            "整棵树的改动都进了日志"
        );

        // ── 恢复后开树：结构完整 + 全量点查 ──
        let mut data_file = DataFile::open(&io, Path::new(DATA_F)).unwrap();
        let idx_seg2 = bicdb_storage::segment::Segment::open(&mut data_file, seg_page0).unwrap();
        let root2 =
            bicdb_storage::segment::read_tree_head(&idx_seg2.read_page(0).unwrap()).unwrap();
        assert_eq!(root2, root_out, "树头未被重放破坏（差异不覆盖扩展区）");
        let pool2 = BufferPool::with_config(
            &io,
            64,
            move |_ws, r| match r.file_id() {
                DATA_FID => Some((data_handle2, r.block_id())),
                _ => None,
            },
            FakeWal,
            SystemClock,
            CacheConfig::for_capacity(64),
        )
        .unwrap();
        let mut store = bicdb_index::ReadOnlyStore::new(&pool2, DATA_FID, WS);
        let mut tree = Tree::open(&mut store, DATA_FID, root2).unwrap();
        tree.validate().expect("恢复后：叶链与根清点相符");
        for i in 0..ROWS {
            assert_eq!(
                tree.lookup(&key_of(i)).unwrap(),
                Some(rid_of(i)),
                "恢复后点查命中：{i}"
            );
        }
        let _ = data_handle;
    }
}
