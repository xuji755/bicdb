//! 扫描 I/O 原语：**区读（多块读）**与**按块批量回表**（§5.12 / §9.4）。
//!
//! ```text
//! 索引扫描产出 ROWID 流
//!   → sort_rowids（按 (file, block, slot) 排序）
//!   → fetch_rows：同块的多行共享**一次区读 + 一次 CR 块重建**
//! ```
//!
//! # 两条纪律
//!
//! 1. **先排序再批量**：`fetch_rows` 不做排序（它按请求顺序回填结果，分组按
//!    首次出现序）——**排序是调用方的责任**（`sort_rowids` 提供），排过序的
//!    请求才让同块多行相邻、让 CR 只做一次/块；未排序时最坏退化为"每行一次
//!    重建"（仍然正确，只是慢）。
//! 2. **批量不改变语义**：CR 快照在整个扫描期固定（§12.1）；批内批间一致。
//!    返回 `None` 表示"该行在快照下不存在"（已删/未提交/槽复用）——与
//!    点查的语义完全相同。

use crate::buffer::{BufferError, BufferPool};
use crate::cr::{self, CrError, ReadView};
use crate::heap;
use crate::page::Page;
use crate::rowid::{Rdba, RowId};
use crate::undo::UndoChain;

/// 顺序扫描的**区读上限**（§5.12：一次 `pread` ≤ 8 页 = 128 KiB）——默认值。
///
/// 实际取值见 [`scan_run_pages`]：实例参数 `storage.multiblock_read_pages`
/// （Oracle `db_file_multiblock_read_count` 的同位物）在实例打开时设定一次。
pub const SCAN_RUN_PAGES: u32 = 8;

/// 进程级当前值（**实例打开时设定一次**；与 `segment::set_file_extend_blocks`
/// 同一范式：扫描层没有实例上下文）。
static RUN_PAGES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(SCAN_RUN_PAGES);

/// 当前的区读页数（索引的 FFS 与堆扫描共用，见 `set_scan_run_pages`）。
#[must_use]
pub fn scan_run_pages() -> u32 {
    RUN_PAGES.load(std::sync::atomic::Ordering::Relaxed)
}

/// **设定区读页数**（实例参数 `storage.multiblock_read_pages`；1..=64 页）。
///
/// 两个调用方必须是同一个值：堆扫描（本模块）与索引的全扫（`bicdb_index`）——
/// 此前是**两个各写 8 的常量**（`SCAN_RUN_PAGES` / `FFS_RUN_PAGES`），
/// 改一处会静默分叉。
///
/// # Errors
/// 越出 1..=64。
pub fn set_scan_run_pages(pages: u32) -> Result<(), &'static str> {
    if !(1..=64).contains(&pages) {
        return Err("multiblock_read_pages 要落在 1–64 页");
    }
    RUN_PAGES.store(pages, std::sync::atomic::Ordering::Relaxed);
    Ok(())
}

/// 扫描原语错误。
#[derive(Debug)]
pub enum ScanError {
    /// 缓冲池。
    Pool(BufferError),
    /// CR 重建。
    Cr(CrError),
    /// ROWID 编不出块地址（越域）。
    BadRowId {
        /// 原始 ROWID。
        rowid: RowId,
    },
    /// 区读返回空（`count = 0`；正常路径不可达——防御性具名）。
    EmptyRun {
        /// 请求的块地址。
        rdba: Rdba,
    },
    /// 区读返回的页数与请求不符（**响亮失败**——绝不静默少读）。
    ShortRun {
        /// 请求的页数。
        expected: u32,
        /// 实际拿到的页数。
        got: usize,
    },
    /// 扫描块号编不出 ROWID（越域；段页表给错）。
    BadBlock {
        /// 块号。
        block: u32,
    },
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScanError::Pool(e) => write!(f, "扫描读池：{e}"),
            ScanError::Cr(e) => write!(f, "扫描 CR：{e}"),
            ScanError::BadRowId { rowid } => write!(f, "扫描：ROWID {rowid} 越出块地址域"),
            ScanError::EmptyRun { rdba } => write!(
                f,
                "扫描：区读返回空（文件 {} 块 {}）",
                rdba.file_id(),
                rdba.block_id()
            ),
            ScanError::ShortRun { expected, got } => {
                write!(f, "扫描：区读请求 {expected} 页、只拿到 {got} 页")
            }
            ScanError::BadBlock { block } => write!(f, "扫描：块号 {block} 编不出 ROWID"),
        }
    }
}

impl std::error::Error for ScanError {}

impl From<BufferError> for ScanError {
    fn from(e: BufferError) -> Self {
        ScanError::Pool(e)
    }
}

impl From<CrError> for ScanError {
    fn from(e: CrError) -> Self {
        ScanError::Cr(e)
    }
}

/// 按 `(file_id, block_id, slot)` **三级全序**排序 ROWID（回表批量的第一步；
/// §9.4）。排序后同块的多行相邻，`fetch_rows` 对每块只做一次 CR。
pub fn sort_rowids(ids: &mut [RowId]) {
    ids.sort_unstable_by_key(|r| (r.file_id(), r.block_id(), r.row_id()));
}

/// **把一个 ROWID 解析到当前落点**（沿行迁移的转发链；只读、有限跳）。
///
/// 读不到页/槽不是 Normal ⇒ 原样返回（由调用方的 `None`/判活逻辑说话——
/// 这里**不制造错误**：解析失败与"行不在"对回表是同一种结果）。
fn resolve_forwarding(
    pool: &BufferPool<'_>,
    workspace: [u8; 8],
    rid: RowId,
) -> Result<RowId, ScanError> {
    let mut cur = rid;
    for _ in 0..crate::scan::FORWARD_MAX_HOPS {
        let rdba = match Rdba::from_parts(cur.file_id(), cur.block_id()) {
            Some(r) => r,
            None => return Ok(cur),
        };
        let mut pages = pool.read_run(workspace, rdba, 1)?;
        let Some(page) = pages.pop() else {
            return Ok(cur);
        };
        match heap::slot_status(&page, cur.row_id()) {
            Some(crate::page::SlotStatus::Forwarding) => {
                match heap::forwarding_target(&page, cur.row_id()) {
                    Some(next) => cur = next,
                    None => return Ok(cur),
                }
            }
            _ => return Ok(cur),
        }
    }
    Ok(cur)
}

/// 转发链跳数上限（与 `catalog` 的 `forward_max_hops` 同源口径）。
pub const FORWARD_MAX_HOPS: usize = 16;

/// **批量回表**：取出这批 ROWID 在 `view` 下的行（§9.4）。
///
/// - 返回与请求**同序**的 `Vec<Option<Vec<u8>>>`；`None` = 该行在视角下不
///   存在（已删/未提交/槽已复用）——与点查语义一致；
/// - 同块的多行共享**一次区读（`count = 1`）**与**一次 CR 块重建**——
///   未按块排序时结果仍正确，但每块的重建次数会退化（见模块文档）。
///
/// **批量回表**（索引回表的主路径；`rows` = 索引项里记的 ROWID）。
///
/// **跟随转发指针**（见 [`resolve_forwarding`]）——索引项存的是稳定入口，
/// 行迁移后要靠它落到当前页；不解析的话迁移过的行会回表落空。
///
/// # Errors
/// 页读失败（I/O）、ROWID 形态非法。
pub fn fetch_rows(
    pool: &BufferPool<'_>,
    chain: &UndoChain<'_, '_>,
    view: ReadView,
    rows: &[RowId],
) -> Result<Vec<Option<Vec<u8>>>, ScanError> {
    Ok(fetch_rows_resolved(pool, chain, view, rows)?
        .into_iter()
        .map(|o| o.map(|(_, bytes)| bytes))
        .collect())
}

/// 一行 + 它**解析转发之后**的物理位置（[`fetch_rows_resolved`] 的返回元素）。
pub type ResolvedRow = Option<(RowId, Vec<u8>)>;

/// 与 [`fetch_rows`] 同一条路，额外返回每行**解析转发之后**的物理 `ROWID`。
///
/// 为什么需要它：**同一条活行可能有多条索引项**——改键列只追加新项、不移动
/// 旧项（索引写没有 undo，见 `arch/09` §9.1.2 的取舍）。于是范围覆盖新旧两个键
/// 时，同一条行会被两条项各带出一次；索引扫描据此**按物理行去重**。
/// 去重键必须是**解析后**的物理位置：行迁移过的话，旧项记的入口与
/// 新项记的物理位置**RID 不同**，但指向同一行。
///
/// # Errors
/// 页读失败（I/O）、ROWID 形态非法。
pub fn fetch_rows_resolved(
    pool: &BufferPool<'_>,
    chain: &UndoChain<'_, '_>,
    view: ReadView,
    rows: &[RowId],
) -> Result<Vec<ResolvedRow>, ScanError> {
    let workspace = chain.segment().workspace_ref();
    let mut out: Vec<ResolvedRow> = vec![None; rows.len()];

    // **先把 ROWID 解析到当前落点**（行迁移的转发链）：索引项里存的是**稳定入口**
    // （写入时的位置），迁移后原槽位只剩 6B 转发指针——不回解析的话，回表会拿到
    // `None`（转发槽不是行），于是"这一行明明在、唯一性却漏判"。
    // 实测抓到的正是这条：改长更新（迁移）之后，同键重复插入被放行。
    let resolved: Vec<RowId> = rows
        .iter()
        .map(|r| resolve_forwarding(pool, workspace, *r).unwrap_or(*r))
        .collect();

    // 按块分组（保序：处理顺序 = 首次出现的块序；输出按原下标回填）。
    let mut groups: Vec<(Rdba, Vec<(usize, u16)>)> = Vec::new();
    for (i, r) in resolved.iter().enumerate() {
        let rdba =
            Rdba::from_parts(r.file_id(), r.block_id()).ok_or(ScanError::BadRowId { rowid: *r })?;
        match groups.iter_mut().find(|(k, _)| *k == rdba) {
            Some((_, items)) => items.push((i, r.row_id())),
            None => groups.push((rdba, vec![(i, r.row_id())])),
        }
    }

    for (rdba, items) in groups {
        let mut pages = pool.read_run(workspace, rdba, 1)?;
        let page = pages.pop().ok_or(ScanError::EmptyRun { rdba })?;
        // **一次 CR 块重建**服务本块全部请求行。
        let cr_page = cr::reconstruct(&page, view, chain)?;
        for (idx, row_no) in items {
            let phys = resolved[idx];
            out[idx] = heap::row(&cr_page, row_no).map(|b| (phys, b.to_vec()));
        }
    }
    Ok(out)
}

/// 便捷形态：已经按块排序的 ROWID 流**逐块回表**时，直接给块与行号。
///
/// （供扫描器在"同一块的 ROWID 连续出现"时跳过分组——语义与
/// [`fetch_rows`] 相同。）
pub fn fetch_block_rows(
    pool: &BufferPool<'_>,
    chain: &UndoChain<'_, '_>,
    view: ReadView,
    rdba: Rdba,
    row_nos: &[u16],
) -> Result<Vec<Option<Vec<u8>>>, ScanError> {
    let workspace = chain.segment().workspace_ref();
    let mut pages = pool.read_run(workspace, rdba, 1)?;
    let page = pages.pop().ok_or(ScanError::EmptyRun { rdba })?;
    let cr_page = cr::reconstruct(&page, view, chain)?;
    Ok(row_nos
        .iter()
        .map(|n| heap::row(&cr_page, *n).map(<[u8]>::to_vec))
        .collect())
}

/// **顺序扫描游标**（全表扫描的存储服务口；执行器不碰页）。
///
/// - 按**区**读（连续块成组，≤ [`SCAN_RUN_PAGES`] 页一次 `pread`，§5.12）；
/// - 每块**一次 CR 块重建**（§12.3：整块还原到快照版本），逐槽提取行；
/// - **可见性由本服务负责**（执行器不得推测）：返回的每一行都是"快照下
///   存在"的行——已删 / 未提交 / 槽已复用 ⇒ 不返回（与 [`fetch_rows`]
///   同语义）；行内片段链的重装在行提取之后（`fetch` 语义）。
///
/// 块列表由调用方给（段侧 [`crate::segment::Segment::data_blocks`]，`bound`
/// 按表类型选，§4.3.1）；扫描范围的**上界语义**因此留在调用方。
pub struct HeapScanner<'a, 'b, 'io, 'f> {
    pool: &'a BufferPool<'b>,
    chain: &'a UndoChain<'io, 'f>,
    view: ReadView,
    workspace: [u8; 8],
    file_id: u16,
    /// 待扫物理块（升序）。
    blocks: Vec<u32>,
    /// 下一个待发起的区读下标。
    at: usize,
    /// 当前区读的剩余页（与 `run_blocks` 同序）。
    run: std::collections::VecDeque<Page>,
    /// 当前区读各页对应的块号。
    run_blocks: std::collections::VecDeque<u32>,
    /// 当前页的待发行。
    pending: std::collections::VecDeque<(RowId, Vec<u8>)>,
    /// 诊断：区读次数。
    pub runs: u64,
    /// 诊断：读入页数。
    pub pages_read: u64,
    /// 诊断：CR 重建次数（每块一次）。
    pub cr_rebuilds: u64,
}

impl std::fmt::Debug for HeapScanner<'_, '_, '_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeapScanner")
            .field("remaining_blocks", &(self.blocks.len() - self.at))
            .field("runs", &self.runs)
            .field("cr_rebuilds", &self.cr_rebuilds)
            .finish()
    }
}

impl<'a, 'b, 'io, 'f> HeapScanner<'a, 'b, 'io, 'f> {
    /// 打开游标（块列表 = 待扫数据页，升序；空列表 ⇒ 立即扫完）。
    #[must_use]
    pub fn new(
        pool: &'a BufferPool<'b>,
        chain: &'a UndoChain<'io, 'f>,
        view: ReadView,
        file_id: u16,
        blocks: Vec<u32>,
    ) -> Self {
        let workspace = chain.segment().workspace_ref();
        Self {
            pool,
            chain,
            view,
            workspace,
            file_id,
            blocks,
            at: 0,
            run: std::collections::VecDeque::new(),
            run_blocks: std::collections::VecDeque::new(),
            pending: std::collections::VecDeque::new(),
            runs: 0,
            pages_read: 0,
            cr_rebuilds: 0,
        }
    }

    /// 取下一行（ROWID + 行字节）。`None` = 扫完。
    /// **复位到扫描起点**（重扫：同快照、同块列表；区读/CR 诊断计数
    /// 不清零——它们是本扫描器实例的累计）。
    pub fn rewind(&mut self) {
        self.at = 0;
        self.run.clear();
        self.run_blocks.clear();
        self.pending.clear();
    }

    /// 取下一行（区读 ≤ 8 页 + 每块一次 CR）。
    pub fn next_row(&mut self) -> Result<Option<(RowId, Vec<u8>)>, ScanError> {
        loop {
            if let Some(item) = self.pending.pop_front() {
                return Ok(Some(item));
            }
            let page = match self.run.pop_front() {
                Some(p) => p,
                None => {
                    if !self.start_run()? {
                        return Ok(None);
                    }
                    continue;
                }
            };
            let block = self.run_blocks.pop_front().expect("页与块同序");
            // 块引用自证（串页防线）：页头必须与请求一致。
            match page.header() {
                Some(h) if h.block_id == block && h.file_id == self.file_id => {}
                _ => return Err(ScanError::BadBlock { block }),
            }
            // **一次 CR 块重建**服务本块全部行（§12.3）。
            let cr_page = cr::reconstruct(&page, self.view, self.chain)?;
            self.cr_rebuilds += 1;
            let slots = cr_page.slot_count();
            for row_no in 1..=slots {
                if let Some(bytes) = heap::row(&cr_page, row_no) {
                    let rid = RowId::from_parts(self.file_id, block, row_no)
                        .map_err(|_| ScanError::BadBlock { block })?;
                    self.pending.push_back((rid, bytes.to_vec()));
                }
            }
        }
    }

    /// 发起下一个区读（连续块成组）。返回是否还有块。
    fn start_run(&mut self) -> Result<bool, ScanError> {
        if self.at >= self.blocks.len() {
            return Ok(false);
        }
        let first = self.blocks[self.at];
        let mut count = 1u32;
        while self.at + (count as usize) < self.blocks.len()
            && self.blocks[self.at + count as usize] == first + count
            && count < scan_run_pages()
        {
            count += 1;
        }
        let rdba =
            Rdba::from_parts(self.file_id, first).ok_or(ScanError::BadBlock { block: first })?;
        let pages = self.pool.read_run(self.workspace, rdba, count)?;
        if pages.len() != count as usize {
            return Err(ScanError::ShortRun {
                expected: count,
                got: pages.len(),
            });
        }
        for i in 0..count {
            self.run_blocks.push_back(first + i);
        }
        self.at += count as usize;
        self.runs += 1;
        self.pages_read += u64::from(count);
        self.run = pages.into();
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use bicdb_common::seq::{CommitSeq, Lsn};
    use bicdb_workspace::io::{FileHandle, FileIo, MemFileIo, OpenOptions};

    use super::*;
    use crate::buffer::{BufferKey, WalGuard};
    use crate::datafile::DataFile;
    use crate::heap::{self, InsertPolicy};
    use crate::itl::{ItlEntry, ItlState};
    use crate::page::{Page, PageType};
    use crate::pagefile;
    use crate::row::assemble_row;
    use crate::undo::{create_undo_segment, read_slot, txn_id_of, UndoOp, UndoPayload};

    const WS: [u8; 8] = [9u8; 8];
    const DATA_F: &str = "/mem/scan.dat";
    const UNDO_F: &str = "/mem/scan_undo.dat";

    fn lsn(v: u64) -> Lsn {
        Lsn::from_raw(v).unwrap()
    }

    fn seq(v: u64) -> CommitSeq {
        CommitSeq::from_raw(v).unwrap()
    }

    fn row(payload: &[u8]) -> Vec<u8> {
        assemble_row(0, 1, &[false], &[], &[payload]).unwrap()
    }

    fn rid(block: u32, slot: u16) -> RowId {
        RowId::from_parts(3, block, slot).unwrap()
    }

    /// 假 WAL：水位视为已全落盘（扫描路径不触发 WAL）。
    struct NoWal;
    impl WalGuard for NoWal {
        fn durable_lsn(&self) -> Lsn {
            lsn(u64::MAX >> 16)
        }
        fn ensure_durable(&self, _t: Lsn) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// 记 `read_exact_at` 次数的 I/O 包装（断言"一次 pread = 多块读"）。
    struct CountingIo {
        inner: MemFileIo,
        reads: AtomicUsize,
    }
    impl CountingIo {
        fn new() -> Self {
            Self {
                inner: MemFileIo::new(),
                reads: AtomicUsize::new(0),
            }
        }
        fn reads(&self) -> usize {
            self.reads.load(Ordering::SeqCst)
        }
    }
    impl FileIo for CountingIo {
        fn open(&self, path: &Path, opts: OpenOptions) -> std::io::Result<FileHandle> {
            self.inner.open(path, opts)
        }
        fn open_dir(&self, path: &Path) -> std::io::Result<FileHandle> {
            self.inner.open_dir(path)
        }
        fn read_at(&self, h: FileHandle, buf: &mut [u8], off: u64) -> std::io::Result<usize> {
            self.inner.read_at(h, buf, off)
        }
        fn read_exact_at(&self, h: FileHandle, buf: &mut [u8], off: u64) -> std::io::Result<()> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            self.inner.read_exact_at(h, buf, off)
        }
        fn write_at(&self, h: FileHandle, buf: &[u8], off: u64) -> std::io::Result<()> {
            self.inner.write_at(h, buf, off)
        }
        fn size(&self, h: FileHandle) -> std::io::Result<u64> {
            self.inner.size(h)
        }
        fn set_len(&self, h: FileHandle, len: u64) -> std::io::Result<()> {
            self.inner.set_len(h, len)
        }
        fn sync_data(&self, h: FileHandle) -> std::io::Result<()> {
            self.inner.sync_data(h)
        }
        fn sync_all(&self, h: FileHandle) -> std::io::Result<()> {
            self.inner.sync_all(h)
        }
        fn sync_dir(&self, h: FileHandle) -> std::io::Result<()> {
            self.inner.sync_dir(h)
        }
        fn close(&self, h: FileHandle) -> std::io::Result<()> {
            self.inner.close(h)
        }
    }

    fn pool_over(io: &CountingIo, data: FileHandle, capacity: usize) -> BufferPool<'_> {
        BufferPool::new(
            io,
            capacity,
            move |ws, r| (*ws == WS && r.file_id() == 3).then_some((data, r.block_id())),
            NoWal,
        )
        .unwrap()
    }

    #[test]
    fn read_run_reads_once_then_hits_the_pool() {
        let io = CountingIo::new();
        io.inner.add_dir("/mem");
        let data = pagefile::create(&io, Path::new(DATA_F), 16).unwrap();
        let mut ids = Vec::new();
        for b in 1..=8u32 {
            let mut p = Page::new(PageType::HeapTable, WS, 3, b);
            ids.push(heap::insert_row(&mut p, &row(b"x"), &InsertPolicy::in_place(0)).unwrap());
            pagefile::write_page(&io, data, b, &mut p).unwrap();
        }
        let pool = pool_over(&io, data, 16);
        let first = Rdba::from_parts(3, 1).unwrap();

        let before = io.reads();
        let pages = pool.read_run(WS, first, 8).unwrap();
        assert_eq!(pages.len(), 8, "整段返回");
        assert_eq!(io.reads() - before, 1, "一次 pread 覆盖 8 页（多块读）");
        assert_eq!(
            heap::row(&pages[0], ids[0]),
            Some(&row(b"x")[..]),
            "第一页内容正确"
        );
        assert_eq!(
            heap::row(&pages[7], ids[7]),
            Some(&row(b"x")[..]),
            "最后一页内容正确"
        );
        assert_eq!(pool.stats().run_reads, 1);
        assert_eq!(pool.stats().run_pages, 8);
        assert_eq!(pool.dirty_len(WS), 0, "装入的是净页（不标脏）");

        // 第二次：全部命中 —— 零物理读。
        let before = io.reads();
        let pages = pool.read_run(WS, first, 8).unwrap();
        assert_eq!(io.reads(), before, "第二次零物理读");
        assert_eq!(pages.len(), 8);
        // 装入的帧落**冷段**（扫描不升热）。
        assert_eq!(pool.chain_of(BufferKey::new(WS, first)), Some("cold"));
        assert_eq!(pool.touch_count(BufferKey::new(WS, first)), Some(0));
    }

    #[test]
    fn read_run_uses_pool_copy_for_resident_pages() {
        let io = CountingIo::new();
        io.inner.add_dir("/mem");
        let data = pagefile::create(&io, Path::new(DATA_F), 16).unwrap();
        for b in 1..=8u32 {
            let mut p = Page::new(PageType::HeapTable, WS, 3, b);
            heap::insert_row(&mut p, &row(b"y"), &InsertPolicy::in_place(0)).unwrap();
            pagefile::write_page(&io, data, b, &mut p).unwrap();
        }
        let pool = pool_over(&io, data, 16);
        // 先把第 3 页读进池并**改动池内副本**（不标脏）——区读必须返回池内副本。
        let k3 = BufferKey::new(WS, Rdba::from_parts(3, 3).unwrap());
        {
            let mut g = pool.pin(k3).unwrap();
            g.as_bytes_mut()[5000] = 0x99;
        }
        let before = io.reads();
        let pages = pool
            .read_run(WS, Rdba::from_parts(3, 1).unwrap(), 8)
            .unwrap();
        assert_eq!(io.reads() - before, 1, "缺失页仍一次 pread");
        assert_eq!(pages[2].as_bytes()[5000], 0x99, "驻留页以池内为准");
        assert_eq!(pages[0].as_bytes()[5000], 0x00, "缺失页来自文件");
    }

    #[test]
    fn batch_fetch_preserves_order_and_none_semantics() {
        let io = CountingIo::new();
        io.inner.add_dir("/mem");
        let data = pagefile::create(&io, Path::new(DATA_F), 16).unwrap();
        let r1 = row(b"aaa");
        let r2 = row(b"bbb");
        let ra = row(b"ccc");
        {
            let mut p1 = Page::new(PageType::HeapTable, WS, 3, 1);
            heap::insert_row(&mut p1, &r1, &InsertPolicy::in_place(0)).unwrap();
            heap::insert_row(&mut p1, &r2, &InsertPolicy::in_place(0)).unwrap();
            pagefile::write_page(&io, data, 1, &mut p1).unwrap();
            let mut p2 = Page::new(PageType::HeapTable, WS, 3, 2);
            heap::insert_row(&mut p2, &ra, &InsertPolicy::in_place(0)).unwrap();
            pagefile::write_page(&io, data, 2, &mut p2).unwrap();
        }
        let pool = pool_over(&io, data, 16);
        // 未建任何 undo 记录的空链（CR 无操作）。
        let mut undo_file = DataFile::create(&io, Path::new(UNDO_F), 1, 1, WS, 512).unwrap();
        let segment = create_undo_segment(&mut undo_file, 2, 3, 4).unwrap();
        let chain = UndoChain::open(segment);

        // 乱序请求 + 一个不存在的行号。
        let reqs = vec![rid(2, 1), rid(1, 2), rid(1, 1), rid(1, 99)];
        let out = fetch_rows(&pool, &chain, ReadView::new(seq(0)), &reqs).unwrap();
        assert_eq!(out[0].as_deref(), Some(ra.as_slice()), "块 2 行 1");
        assert_eq!(out[1].as_deref(), Some(r2.as_slice()), "块 1 行 2");
        assert_eq!(out[2].as_deref(), Some(r1.as_slice()), "块 1 行 1");
        assert_eq!(out[3], None, "不存在的行号为 None");

        // 排序 helper：三级全序（块 → 槽）。
        let mut v = reqs.clone();
        sort_rowids(&mut v);
        assert_eq!(
            v.iter()
                .map(|r| (r.block_id(), r.row_id()))
                .collect::<Vec<_>>(),
            vec![(1, 1), (1, 2), (1, 99), (2, 1)],
            "按 (块, 槽) 全序"
        );
    }

    #[test]
    fn batch_fetch_applies_cr_once_per_block() {
        // 页上两行；T1 删除第 1 行且未提交 ⇒ 快照下第 1 行仍可见（CR 生效）。
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let data = pagefile::create(&io, Path::new(DATA_F), 16).unwrap();
        let mut undo_file = DataFile::create(&io, Path::new(UNDO_F), 1, 1, WS, 512).unwrap();
        let segment = create_undo_segment(&mut undo_file, 2, 3, 4).unwrap();
        let mut chain = UndoChain::open(segment);
        let slot = chain.allocate_slot().unwrap();
        let hdr = chain.segment().read_page(0).unwrap();
        let txn_id = txn_id_of(slot, &read_slot(&hdr, slot).unwrap());

        let r1 = row(b"one");
        let r2 = row(b"two");
        {
            let mut p = Page::new(PageType::HeapTable, WS, 3, 1);
            heap::insert_row(&mut p, &r1, &InsertPolicy::in_place(0)).unwrap();
            heap::insert_row(&mut p, &r2, &InsertPolicy::in_place(0)).unwrap();
            crate::itl::write_itl(
                &mut p,
                0,
                &ItlEntry {
                    txn_id,
                    undo_ptr: None,
                    commit_seq: None,
                    lock_cnt: 1,
                    state: ItlState::Active,
                },
            )
            .unwrap();
            pagefile::write_page(&io, data, 1, &mut p).unwrap();
        }
        // 链：ITL 覆盖（原为空闲）+ 删除第 1 行。
        chain
            .append(
                slot,
                UndoOp::ItlOverwrite,
                0,
                RowId::from_parts(3, 1, 1).unwrap(),
                UndoPayload::ItlOverwrite {
                    txn_id,
                    itl_slot: 0,
                    old: None,
                },
            )
            .unwrap();
        chain
            .append(
                slot,
                UndoOp::Delete,
                0,
                RowId::from_parts(3, 1, 1).unwrap(),
                UndoPayload::FullRow(r1.clone()),
            )
            .unwrap();

        let pool = BufferPool::new(
            &io,
            8,
            move |ws, r| (*ws == WS && r.file_id() == 3).then_some((data, r.block_id())),
            NoWal,
        )
        .unwrap();
        // 未提交（快照 0）⇒ 删除被 CR 撤销、两行都可见。
        let out = fetch_rows(
            &pool,
            &chain,
            ReadView::new(seq(0)),
            &[
                RowId::from_parts(3, 1, 1).unwrap(),
                RowId::from_parts(3, 1, 2).unwrap(),
            ],
        )
        .unwrap();
        assert_eq!(out[0].as_deref(), Some(r1.as_slice()), "被删行经 CR 恢复");
        assert_eq!(out[1].as_deref(), Some(r2.as_slice()), "另一行不受影响");

        // fetch_block_rows 便捷形态：同块多行、同语义。
        let out = fetch_block_rows(
            &pool,
            &chain,
            ReadView::new(seq(0)),
            Rdba::from_parts(3, 1).unwrap(),
            &[1, 2],
        )
        .unwrap();
        assert_eq!(out[0].as_deref(), Some(r1.as_slice()));
        assert_eq!(out[1].as_deref(), Some(r2.as_slice()));
    }
}
