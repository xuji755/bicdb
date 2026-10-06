//! **索引维护（写侧）**：索引项的插入/删除 + 树头持久化（REQ-ENG-003）。
//!
//! ```text
//! insert_entry / delete_entry：开树（带 redo 写口）→ 树算法 → 新根回写树头
//! write_tree_head_redo      ：树头（段头页扩展区 6B）**经页差异 redo** 落盘
//! ```
//!
//! **为什么树头也要 redo**：引导页/树头是"锚点"，但**B+Tree 的树头不是恢复锚点**
//! ——分裂后新根必须与索引页一起原子（同一日志流）落地，否则崩溃后树头指向
//! 旧根而旧根已分裂（丢一半条目）。所以树头写入走与索引页同一条 redo 流
//! （页差异覆盖段头扩展区的 6B）。
//!
//! **索引项不做独立撤销**（`arch/09` §9.1.2）：删除/插入索引项随**行**的事务
//! 一起走日志；回滚由行侧撤销 + 索引页重放共同保证（V1.0 的索引项与行同事务，
//! 崩溃恢复把两者一起前滚/回滚）。

use bicdb_index::IndexError;
use bicdb_storage::buffer::{BufferKey, BufferPool};
use bicdb_storage::datafile::DataFile;
use bicdb_storage::rowid::RowId;
use bicdb_storage::segment::{self, Segment, SegmentSpaceError};
use bicdb_txn::index_io::TxnIndexIo;
use bicdb_txn::write::{self, Txn};
use bicdb_wal::group::GroupWriter;

use crate::heap::TableAccessError;

impl From<IndexError> for TableAccessError {
    fn from(e: IndexError) -> Self {
        Self::Segment(SegmentSpaceError::Io(std::io::Error::other(e.to_string())))
    }
}

/// **插入一个索引项**；返回**新树头**（根分裂时变化，否则原值）。
///
/// 调用方负责把返回的树头写回（[`write_tree_head_redo`]）——**同一事务内**。
#[allow(clippy::too_many_arguments)]
pub fn insert_entry(
    pool: &BufferPool<'_>,
    log: &mut GroupWriter<'_, '_>,
    file: &mut DataFile<'_>,
    ws: [u8; 8],
    seg_page0: u32,
    txn: &Txn,
    key: &[u8],
    rid: RowId,
) -> Result<RowId, TableAccessError> {
    let root = read_tree_head_live(pool, file, ws, seg_page0)?;
    let file_id = file.file_id();
    let mut seg = open_seg(pool, file, seg_page0, ws)?;
    let new_root = {
        let mut io = TxnIndexIo::new(pool, log, &mut seg, txn);
        let mut store = bicdb_index::PoolStore::new(pool, &mut io, file_id, ws);
        let mut tree = bicdb_index::Tree::open(&mut store, file_id, root)?;
        tree.insert(key, rid)?;
        tree.root()
    };
    Ok(new_root)
}

/// **删除一个索引项**；返回新树头（空页留树——删除不移除页，`arch/09` §9.1.5）。
#[allow(clippy::too_many_arguments)]
pub fn delete_entry(
    pool: &BufferPool<'_>,
    log: &mut GroupWriter<'_, '_>,
    file: &mut DataFile<'_>,
    ws: [u8; 8],
    seg_page0: u32,
    txn: &Txn,
    key: &[u8],
    rid: RowId,
) -> Result<RowId, TableAccessError> {
    let root = read_tree_head_live(pool, file, ws, seg_page0)?;
    let file_id = file.file_id();
    let mut seg = open_seg(pool, file, seg_page0, ws)?;
    let new_root = {
        let mut io = TxnIndexIo::new(pool, log, &mut seg, txn);
        let mut store = bicdb_index::PoolStore::new(pool, &mut io, file_id, ws);
        let mut tree = bicdb_index::Tree::open(&mut store, file_id, root)?;
        tree.delete(key, rid)?;
        tree.root()
    };
    Ok(new_root)
}

/// **读树头（活系统形态）**：段头页**池优先**——页 no-force，直读文件会拿到
/// 旧树头（分裂后的新根还没落盘；实测就是这里踩到"根 = 0 ⇒ 不是索引页"）。
fn read_tree_head_live(
    pool: &BufferPool<'_>,
    file: &DataFile<'_>,
    ws: [u8; 8],
    seg_page0: u32,
) -> Result<RowId, TableAccessError> {
    let rdba = bicdb_storage::rowid::Rdba::from_parts(file.file_id(), seg_page0)
        .ok_or(TableAccessError::Segment(SegmentSpaceError::BitmapCoverage))?;
    let page = match pool.pin(BufferKey::new(ws, rdba)) {
        Ok(g) => bicdb_storage::page::Page::from_bytes(Box::new(*g.as_bytes())),
        Err(_) => file.read_page(seg_page0)?,
    };
    Ok(segment::read_tree_head(&page)?)
}

/// 按次开段（池优先段头读——活系统形态）。
fn open_seg<'io, 'f>(
    pool: &BufferPool<'_>,
    file: &'f mut DataFile<'io>,
    page0: u32,
    ws: [u8; 8],
) -> Result<Segment<'io, 'f>, TableAccessError> {
    Ok(Segment::open_pooled(pool, file, page0, ws)?)
}

/// **建索引（批量灌树；`目录详设` §5.3 ④ 的落点）**。
///
/// ```text
/// 调用方：扫描基表（语句快照）→ 逐行求键 → 排序（唯一索引在此步检出重复）
/// 本函数：bulk_load（自底向上）→ 树头经 redo 落盘 → 报告建了多少页
/// ```
///
/// **为什么批量而不是逐行插入**：`arch/09` §9.1.5 的建索引路径——顺序写每页
/// 一次（无下行、无分裂的页写放大）。**输入必须按键升序**；`unique = true` 时
/// 相邻等键 ⇒ [`bicdb_index::IndexError::DuplicateKey`]（DDL 事务据此整体回滚）。
#[allow(clippy::too_many_arguments)]
pub fn build_index(
    pool: &BufferPool<'_>,
    log: &mut GroupWriter<'_, '_>,
    file: &mut DataFile<'_>,
    ws: [u8; 8],
    seg_page0: u32,
    txn: &Txn,
    entries: &[(Vec<u8>, RowId)],
    unique: bool,
) -> Result<bicdb_index::BulkLoadReport, TableAccessError> {
    let file_id = file.file_id();
    let mut seg = open_seg(pool, file, seg_page0, ws)?;
    let (root, report) = {
        let mut io = TxnIndexIo::new(pool, log, &mut seg, txn);
        let mut store = bicdb_index::PoolStore::new(pool, &mut io, file_id, ws);
        let (tree, report) = bicdb_index::Tree::bulk_load(
            &mut store,
            file_id,
            ws,
            entries,
            unique,
            bicdb_index::DEFAULT_FILL_PERCENT,
        )?;
        (tree.root(), report)
    };
    drop(seg);
    write_tree_head_redo(pool, log, file, ws, seg_page0, txn, root)?;
    Ok(report)
}

/// **树头回写（经页差异 redo）**：段头页扩展区的 6B 根页地址。
#[allow(clippy::too_many_arguments)]
pub fn write_tree_head_redo(
    pool: &BufferPool<'_>,
    log: &mut GroupWriter<'_, '_>,
    file: &mut DataFile<'_>,
    ws: [u8; 8],
    seg_page0: u32,
    txn: &Txn,
    root: RowId,
) -> Result<(), TableAccessError> {
    let seg = open_seg(pool, file, seg_page0, ws)?;
    let page0 = seg.page0_block();
    // 前像 = **池优先**的当前镜像（树头可能已被上次维护写过——池像更新）。
    let key = BufferKey::new(
        ws,
        bicdb_storage::rowid::Rdba::from_parts(seg.file_id(), page0)
            .ok_or(TableAccessError::Segment(SegmentSpaceError::BitmapCoverage))?,
    );
    let before = match pool.pin(key) {
        Ok(g) => bicdb_storage::page::Page::from_bytes(Box::new(*g.as_bytes())),
        Err(_) => seg.read_page(0)?,
    };
    let mut after = bicdb_storage::page::Page::from_bytes(Box::new(*before.as_bytes()));
    segment::write_tree_head(&mut after, root)?;
    write::write_page_change(
        pool,
        log,
        txn.raw(),
        key,
        before.as_bytes(),
        after.as_bytes(),
        false,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::TableAccess;
    use bicdb_common::seq::{CommitSeq, Lsn};
    use bicdb_storage::buffer::{BufferPool, CacheConfig, SystemClock, WalGuard};
    use bicdb_storage::controlfile::{
        ArchiveMode, ArchiveRecord, ControlFile, RedoEntries, WorkspaceEntry,
    };
    use bicdb_storage::datafile::DataFile;
    use bicdb_storage::heap::InsertPolicy;
    use bicdb_storage::row::{assemble_row, RowView};
    use bicdb_storage::segment::SegType;
    use bicdb_storage::undo::{create_undo_segment, UndoChain};
    use bicdb_wal::group::GroupSpec;
    use bicdb_wal::recovery::redo_from;
    use bicdb_workspace::io::MemFileIo;
    use bicdb_workspace::WorkspaceId;
    use std::path::Path;

    const WS: [u8; 8] = [11u8; 8];
    const DATA_FID: u16 = 3;
    const UNDO_F: &str = "/mem/acc_undo.dat";
    const DATA_F: &str = "/mem/acc_data.dat";
    const WAL: &str = "/mem/acc_wal";
    const CF_A: &str = "/mem/acc_cf_a";
    const CF_B: &str = "/mem/acc_cf_b";
    const ROWS: u32 = 4_000;

    fn seq(v: u64) -> CommitSeq {
        CommitSeq::from_raw(v).unwrap()
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

    fn row_of(i: u32) -> Vec<u8> {
        let payload = format!("row-{i:06}").into_bytes();
        assemble_row(0, 1, &[false], &[], &[payload.as_slice()]).unwrap()
    }

    fn payload_of(bytes: &[u8]) -> Vec<u8> {
        let view = RowView::new(bytes).unwrap();
        view.var_column(0, 0).unwrap_or_default().to_vec()
    }

    /// 键 = 保序数值编码（与目录同法）。
    fn key_of(i: u32) -> Vec<u8> {
        bicdb_types::Number::parse(&i.to_string()).unwrap().encode()
    }

    /// **建索引（批量灌树）端到端**：空表灌入 → 逐行求键 → 排序 → `build_index`
    /// （自底向上）→ 崩溃 → 仅重放 redo → 索引点查全对。
    ///
    /// 这是 CREATE INDEX 的核心路径（`目录详设` §5.3 ④）：DDL 侧只差"写字典行 +
    /// 事务/锁"的外壳。
    #[test]
    fn build_index_bulk_loads_and_survives_a_crash() {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let spec = GroupSpec::new(2, 1, 8192).unwrap();
        let seg_page0;
        let idx_page0;
        let data_handle;
        {
            let mut undo_file = DataFile::create(&io, Path::new(UNDO_F), 1, 1, WS, 512).unwrap();
            let undo_handle = undo_file.handle();
            let undo_seg = create_undo_segment(&mut undo_file, 2, 3, 4).unwrap();

            let mut data_file =
                DataFile::create(&io, Path::new(DATA_F), DATA_FID, 3, WS, 4096).unwrap();
            data_handle = data_file.handle();
            seg_page0 = {
                let seg = Segment::create(&mut data_file, SegType::Heap, 10, 10, 8, 0, 0).unwrap();
                seg.page0_block()
            };
            idx_page0 = {
                let seg = Segment::create(&mut data_file, SegType::BTree, 11, 11, 8, 0, 0).unwrap();
                seg.page0_block()
            };
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
            let mut txn = write::begin(&pool, &mut log, &mut chain, seq(1)).unwrap();

            // ① 灌表（无索引）。
            let mut rowids = Vec::new();
            {
                let mut table = TableAccess::new(&pool, WS);
                let policy = InsertPolicy::in_place(0);
                for i in 0..ROWS {
                    let rid = table
                        .insert(
                            &mut log,
                            &mut chain,
                            &mut txn,
                            &mut data_file,
                            seg_page0,
                            &row_of(i),
                            &policy,
                        )
                        .unwrap();
                    rowids.push(rid);
                }
            }
            // ② 逐行求键 + 排序（唯一索引：排序后等键即冲突）。
            let mut entries: Vec<(Vec<u8>, RowId)> =
                (0..ROWS).map(|i| (key_of(i), rowids[i as usize])).collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.to_bytes().cmp(&b.1.to_bytes())));
            // ③ 批量建索引（自底向上）。
            let report = build_index(
                &pool,
                &mut log,
                &mut data_file,
                WS,
                idx_page0,
                &txn,
                &entries,
                true,
            )
            .unwrap();
            assert_eq!(report.entries, ROWS as usize);
            assert!(
                report.leaf_blocks > 1 && report.branch_blocks >= 1,
                "多级树：{report:?}"
            );
            write::commit(&pool, &mut log, &mut chain, &mut txn, seq(1)).unwrap();

            log.flush(log.appended_lsn()).unwrap();
            drop(chain);
            drop(pool);
            drop(log);
        }

        // 恢复 → 索引点查全对。
        let cf_ro = ControlFile::open(&io, Path::new(CF_A), Path::new(CF_B)).unwrap();
        let groups = bicdb_wal::group::online_groups(&io, &cf_ro, Path::new(WAL), spec).unwrap();
        let reopened = DataFile::open(&io, Path::new(DATA_F)).unwrap();
        let data_handle2 = reopened.handle();
        let undo_reopened = DataFile::open(&io, Path::new(UNDO_F)).unwrap();
        let undo_handle2 = undo_reopened.handle();
        let mut resolve = |r: bicdb_storage::rowid::Rdba| match r.file_id() {
            1 => Some((undo_handle2, r.block_id())),
            DATA_FID => Some((data_handle2, r.block_id())),
            _ => None,
        };
        redo_from(&io, &groups, lsn(0), &mut resolve).unwrap();

        let mut data_file = DataFile::open(&io, Path::new(DATA_F)).unwrap();
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
        let root = {
            let seg = Segment::open_pooled(&pool2, &mut data_file, idx_page0, WS).unwrap();
            segment::read_tree_head(&seg.read_page(0).unwrap()).unwrap()
        };
        let mut store = bicdb_index::ReadOnlyStore::new(&pool2, DATA_FID, WS);
        let mut tree = bicdb_index::Tree::open(&mut store, DATA_FID, root).unwrap();
        tree.validate().expect("恢复后叶链与根清点相符");
        for i in (0..ROWS).step_by(53) {
            assert!(tree.lookup(&key_of(i)).unwrap().is_some(), "点查命中：{i}");
        }
    }

    /// 端到端（真件）：**空表灌入 + 表增长 + 索引维护 + 崩溃恢复**。
    ///
    /// 覆盖此前空缺的那块——"表增长"（执行器夹具明写"不在本切片"）：
    /// 4 000 行（含多次段扩展 / 段内位图窗口物化 / 索引叶分裂）→ 刷日志 →
    /// 丢缓存 → 仅重放 redo → 行数与索引点查全对。
    #[test]
    fn table_growth_index_maintenance_and_crash_recovery() {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let spec = GroupSpec::new(2, 1, 16384).unwrap();
        let seg_page0;
        let idx_page0;
        let data_handle;
        let idx_root0;
        {
            let mut undo_file = DataFile::create(&io, Path::new(UNDO_F), 1, 1, WS, 512).unwrap();
            let undo_handle = undo_file.handle();
            let undo_seg = create_undo_segment(&mut undo_file, 2, 3, 4).unwrap();

            let mut data_file =
                DataFile::create(&io, Path::new(DATA_F), DATA_FID, 3, WS, 4096).unwrap();
            data_handle = data_file.handle();
            // 堆表段（空表）与索引段（空树）——**建段期**直写，故可短暂并存。
            seg_page0 = {
                let seg = Segment::create(&mut data_file, SegType::Heap, 10, 10, 8, 0, 0).unwrap();
                seg.page0_block()
            };
            idx_page0 = {
                let seg = Segment::create(&mut data_file, SegType::BTree, 11, 11, 8, 0, 0).unwrap();
                seg.page0_block()
            };

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

            let mut txn = write::begin(&pool, &mut log, &mut chain, seq(1)).unwrap();

            // 空树：建初始叶页 + 树头写进索引段头（一切经 redo）。
            idx_root0 = {
                let mut seg = Segment::open_pooled(&pool, &mut data_file, idx_page0, WS).unwrap();
                let file_id = seg.file_id();
                let root = {
                    let mut io_idx = TxnIndexIo::new(&pool, &mut log, &mut seg, &txn);
                    let mut store = bicdb_index::PoolStore::new(&pool, &mut io_idx, file_id, WS);
                    let tree = bicdb_index::Tree::create(&mut store, file_id, WS).unwrap();
                    tree.root()
                };
                root
            };
            write_tree_head_redo(
                &pool,
                &mut log,
                &mut data_file,
                WS,
                idx_page0,
                &txn,
                idx_root0,
            )
            .unwrap();

            // **灌入**（表增长 + 索引维护，同一事务）。
            let mut root = idx_root0;
            {
                let mut table = TableAccess::new(&pool, WS);
                let policy = InsertPolicy::in_place(0);
                for i in 0..ROWS {
                    let rid = table
                        .insert(
                            &mut log,
                            &mut chain,
                            &mut txn,
                            &mut data_file,
                            seg_page0,
                            &row_of(i),
                            &policy,
                        )
                        .unwrap();
                    let new_root = insert_entry(
                        &pool,
                        &mut log,
                        &mut data_file,
                        WS,
                        idx_page0,
                        &txn,
                        &key_of(i),
                        rid,
                    )
                    .unwrap();
                    if new_root != root {
                        write_tree_head_redo(
                            &pool,
                            &mut log,
                            &mut data_file,
                            WS,
                            idx_page0,
                            &txn,
                            new_root,
                        )
                        .unwrap();
                        root = new_root;
                    }
                }
            }
            write::commit(&pool, &mut log, &mut chain, &mut txn, seq(1)).unwrap();

            // 崩溃：日志刷出、缓存丢弃。
            log.flush(log.appended_lsn()).unwrap();
            drop(chain);
            drop(pool);
            drop(log);
        }

        // 恢复：只重放 redo。
        let cf_ro = ControlFile::open(&io, Path::new(CF_A), Path::new(CF_B)).unwrap();
        let groups = bicdb_wal::group::online_groups(&io, &cf_ro, Path::new(WAL), spec).unwrap();
        let reopened = DataFile::open(&io, Path::new(DATA_F)).unwrap();
        let data_handle2 = reopened.handle();
        let undo_reopened = DataFile::open(&io, Path::new(UNDO_F)).unwrap();
        let undo_handle2 = undo_reopened.handle();
        let mut resolve = |r: bicdb_storage::rowid::Rdba| match r.file_id() {
            1 => Some((undo_handle2, r.block_id())),
            DATA_FID => Some((data_handle2, r.block_id())),
            _ => None,
        };
        let report = redo_from(&io, &groups, lsn(0), &mut resolve).unwrap();
        assert!(
            report.records_applied > ROWS as usize,
            "行写与索引维护都进了日志"
        );

        // 读回：行数、hwm、索引结构、索引→行内容。
        let mut data_file = DataFile::open(&io, Path::new(DATA_F)).unwrap();
        let (hwm, seen, rows_read) = {
            let seg2 = Segment::open(&mut data_file, seg_page0).unwrap();
            let hwm = seg2.hwm();
            let mut seen = 0u32;
            let mut rows_read = 0usize;
            for b in seg2.data_blocks(hwm) {
                let page = seg2.read_physical_page(b).expect("页已格式化");
                for slot in 1..=page.slot_count() {
                    if let Some(bytes) = bicdb_storage::heap::row(&page, slot) {
                        if bicdb_storage::heap::slot_status(&page, slot)
                            == Some(bicdb_storage::page::SlotStatus::Normal)
                        {
                            rows_read += 1;
                        }
                        let _ = bytes;
                        seen += 1;
                    }
                }
            }
            (hwm, seen, rows_read)
        };
        assert!(hwm > 2, "表已增长（hwm {hwm} > 初始 2）");
        assert_eq!(seen, ROWS, "恢复后行的总数为 {ROWS}");
        assert_eq!(rows_read, ROWS as usize, "全为非转发行");

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
        let root = {
            let seg2 = Segment::open_pooled(&pool2, &mut data_file, idx_page0, WS).unwrap();
            segment::read_tree_head(&seg2.read_page(0).unwrap()).unwrap()
        };
        assert_ne!(root, idx_root0, "多次分裂后树头已推进（新根）");
        let mut store = bicdb_index::ReadOnlyStore::new(&pool2, DATA_FID, WS);
        let mut tree = bicdb_index::Tree::open(&mut store, DATA_FID, root).unwrap();
        tree.validate().expect("恢复后叶链与根清点相符");
        for i in (0..ROWS).step_by(37) {
            assert!(
                tree.lookup(&key_of(i)).unwrap().is_some(),
                "恢复后索引点查命中：{i}"
            );
        }
        // 抽样核对：索引项 → ROWID → 行内容。
        let rid = tree.lookup(&key_of(1234)).unwrap().unwrap();
        let page = data_file.read_page(rid.block_id()).unwrap();
        assert_eq!(
            payload_of(bicdb_storage::heap::row(&page, rid.row_id()).expect("行在")),
            b"row-001234".to_vec(),
            "索引 → ROWID → 行内容一致"
        );
    }
}
