//! 索引**接入存储**的端到端用例（§9.1.5 第 8 步的树头 + §5.10 的池存取口）。
//!
//! 真件：`DataFile` + `SegType::BTree` 段 + `BufferPool`；写口用测试实现
//! （经池改页 + 标脏 + 记录 redo 式前后像）。断言：
//!
//! 1. 插入全程经池（页驻留、脏、按写列表刷回盘）；
//! 2. **树头写回段头页的 B+Tree 扩展区**（根页 ROWID 6B）；
//! 3. 关池重开（**新池从盘读**）后，`Tree::open` 按页类型推出高度、
//!    全部键仍可搜索、`validate` 通过；
//! 4. 分裂记录形态：新页的**完整正文**进过 redo 镜像（§9.1.5 第 5 步——
//!    盘上不存在的页也能被重放重建）。

use bicdb_common::seq::Lsn;
use bicdb_index::{Entry, IndexIo, IndexPage, PageStore, PoolStore, Tree};
use bicdb_storage::buffer::{
    BufferError, BufferKey, BufferPool, CacheConfig, SystemClock, WalGuard,
};
use bicdb_storage::datafile::DataFile;
use bicdb_storage::page::{Page, PageType};
use bicdb_storage::rowid::{Rdba, RowId};
use bicdb_storage::segment::{read_tree_head, write_tree_head, SegType, Segment};
use bicdb_workspace::io::{FileHandle, FileIo, MemFileIo};
use std::path::Path;

const WS: [u8; 8] = [9u8; 8];
const F: u16 = 5;
const DATA_F: &str = "/mem/idx.dat";

/// 假 WAL（池的写回不等 redo；本用例只考页与树头）。
struct NoWal;
impl WalGuard for NoWal {
    fn durable_lsn(&self) -> Lsn {
        Lsn::from_raw(u64::MAX >> 16).expect("域内")
    }
    fn ensure_durable(&self, _t: Lsn) -> std::io::Result<()> {
        Ok(())
    }
}

/// 测试写口：分配走段、写页走池（**新页直接装入**），并记录 redo 镜像。
struct TestIo<'a, 'b, 'c> {
    pool: &'a BufferPool<'b>,
    segment: &'a mut Segment<'c, 'c>,
    redo: &'a mut Vec<(u32, Vec<u8>)>,
    lsn: u64,
}

impl IndexIo for TestIo<'_, '_, '_> {
    fn allocate_page(&mut self) -> Result<u32, bicdb_index::IndexError> {
        let logical = self
            .segment
            .allocate_append_page()
            .map_err(|e| bicdb_index::IndexError::Io(e.to_string()))?;
        self.segment
            .logical_block(logical)
            .ok_or(bicdb_index::IndexError::Malformed("逻辑页无物理块"))
    }

    fn apply_page(&mut self, block: u32, after: &Page) -> Result<(), bicdb_index::IndexError> {
        let key = BufferKey::new(WS, Rdba::from_parts(F, block).expect("块号在域内"));
        let mut guard = match self.pool.pin(key) {
            Ok(g) => g,
            // 盘上尚无该新页（或内容不可读）⇒ 以权威镜像**直接装入**
            // （分裂记录的正文随 redo ⇒ 重放可重建不存在的新页，§9.1.5 第 5 步）。
            Err(BufferError::Unresolved { .. }) => {
                return Err(bicdb_index::IndexError::BlockNotFound { block })
            }
            Err(_) => self
                .pool
                .insert_new(key, Page::from_bytes(Box::new(*after.as_bytes())))
                .map_err(|e| bicdb_index::IndexError::Io(e.to_string()))?,
        };
        guard.as_bytes_mut().copy_from_slice(after.as_bytes());
        self.lsn += 1;
        let lsn = Lsn::from_raw(self.lsn).expect("域内");
        let mut header = guard.header().expect("页头");
        header.page_lsn = lsn;
        header.mod_seq = header.mod_seq.wrapping_add(1);
        guard.write_header(&header);
        guard.mark_dirty(lsn);
        self.redo.push((block, after.as_bytes().to_vec()));
        Ok(())
    }

    /// 本段索引页枚举（FFS 用）：逻辑页 1..append_pos，排除段内位图页。
    fn allocated_blocks(&mut self) -> Result<Vec<u32>, bicdb_index::IndexError> {
        let end = self
            .segment
            .append_position()
            .map_err(|e| bicdb_index::IndexError::Io(e.to_string()))?;
        let mut out = Vec::new();
        for logical in 1..end {
            if self.segment.is_bitmap_page(logical) {
                continue;
            }
            if let Some(block) = self.segment.logical_block(logical) {
                out.push(block);
            }
        }
        out.sort_unstable();
        Ok(out)
    }
}

fn key(i: u32) -> Vec<u8> {
    let mut k = format!("k{i:06}").into_bytes();
    k.extend(std::iter::repeat(b'x').take(120));
    k
}

fn rid(block: u32, row: u16) -> RowId {
    RowId::from_parts(F, block, row).expect("域内")
}

/// 建池：文件 5 的块 → 段所在文件的句柄。
fn pool_of<'a>(io: &'a dyn FileIo, handle: FileHandle) -> BufferPool<'a> {
    BufferPool::with_config(
        io,
        16,
        move |_ws, r| {
            if r.file_id() == F {
                Some((handle, r.block_id()))
            } else {
                None
            }
        },
        NoWal,
        SystemClock,
        CacheConfig::for_capacity(16),
    )
    .expect("建池")
}

#[test]
fn tree_lives_in_a_btree_segment_and_survives_reopen_through_the_pool() {
    let io = MemFileIo::new();
    io.add_dir("/mem");
    // 段所在的数据文件（file_id = 5，角色 3 = 数据文件）。
    let mut file = DataFile::create(&io, Path::new(DATA_F), F, 3, WS, 512).expect("建数据文件");
    let handle = file.handle();
    let mut segment =
        Segment::create(&mut file, SegType::BTree, 1, 1, 8, 0, 0).expect("建 B+Tree 段");

    // ① 建树：初始空叶页 + **树头写回段头页扩展区**。
    let tree_root;
    let mut redo_log: Vec<(u32, Vec<u8>)> = Vec::new();
    {
        let pool = pool_of(&io, handle);
        {
            let mut store_io = TestIo {
                pool: &pool,
                segment: &mut segment,
                redo: &mut redo_log,
                lsn: 0,
            };
            let mut store = PoolStore::new(&pool, &mut store_io, F, WS);
            let mut tree = Tree::create(&mut store, F, WS).expect("建树");
            for i in 0..300u32 {
                tree.insert(&key(i), rid(1, (i % 1000 + 1) as u16))
                    .expect("插入");
            }
            for i in 0..300u32 {
                assert!(tree.lookup(&key(i)).expect("查").is_some(), "键 {i} 应可查");
            }
            tree.validate().expect("结构自检");
            // 根页 ROWID 是**随根分裂变化**的：执行器在 `InsertOutcome.grew`
            // 为真时重写树头（§9.1.5 第 7 步：根分裂那条原子记录含树头更新）。
            tree_root = tree.root();
        }
        // ② 树头写回段头页（这里用直写形态；执行器接段头页时同样经池 + redo）。
        let mut header_page = segment.read_page(0).expect("读段头页（逻辑页 0）");
        assert_eq!(
            read_tree_head(&header_page).expect("读树头"),
            RowId::from_bytes(&[0u8; 6]),
            "初始为空头"
        );
        write_tree_head(&mut header_page, tree_root).expect("写树头");
        segment.write_page(0, &mut header_page).expect("落段头页");
        // ③ 池把脏索引页刷回盘。
        pool.flush_workspace(WS).expect("刷索引页");
    }

    // ④ 关池重开：树头从段头页读、高度由页类型推、全部可查。
    let pool2 = pool_of(&io, handle);
    let header_page = segment.read_page(0).expect("读段头页（逻辑页 0）");
    let head = read_tree_head(&header_page).expect("读树头");
    assert_eq!(head, tree_root, "树头 = 建树时写的根页 ROWID");
    let mut reopen_redo: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut stored_io = TestIo {
        pool: &pool2,
        segment: &mut segment,
        redo: &mut reopen_redo,
        lsn: 100,
    };
    let mut store = PoolStore::new(&pool2, &mut stored_io, F, WS);
    let mut tree = Tree::open(&mut store, F, head).expect("按树头打开");
    assert!(tree.height() >= 1, "多页树（height = {}）", tree.height());
    for i in 0..300u32 {
        assert!(
            tree.lookup(&key(i)).expect("查").is_some(),
            "重开后键 {i} 仍可查"
        );
    }
    tree.validate().expect("重开后结构自检");

    // ⑤ 叶链顺序仍是键序（IFS 的可达性）。
    let all = tree.full_scan(10_000).expect("全扫描");
    assert_eq!(all.len(), 300, "全扫描见到全部键");
    let mut prev: Option<Vec<u8>> = None;
    for (k, _) in &all {
        if let Some(p) = &prev {
            assert!(*p < *k, "扫描顺序非单调");
        }
        prev = Some(k.clone());
    }
    // ⑤′ FFS（§9.1.6）：按物理区序区读全段——同一组条目，不依赖叶链。
    let ffs = tree.fast_full_scan(10_000).expect("快速全扫描");
    assert_eq!(ffs.len(), 300, "FFS 见到全部键");
    let mut a = ffs.clone();
    let mut b = all.clone();
    a.sort();
    b.sort();
    assert_eq!(a, b, "FFS 与 IFS 是同一组条目");
    // ⑤″ 统计：entries 与扫描对账；CF 落在 [块数, 行数] 里（本用例行同块）。
    let st = tree.statistics().expect("统计");
    assert_eq!(st.entries, 300);
    assert_eq!(st.blevel, tree.height());
    assert!(st.clustering_factor <= st.entries, "CF ≤ 行数");
    // ⑥ redo 形态：插入期每次改页都留下**完整正文**镜像（分裂的新页也能被
    //    重放重建，§9.1.5 第 5 步）；镜像的页头与键自洽。
    assert!(!redo_log.is_empty(), "插入产生 redo 镜像");
    assert!(
        redo_log.iter().all(|(b, _)| *b != segment.page0_block()),
        "镜像不含段头页"
    );
    let (block, body) = &redo_log[0];
    let mut arr = [0u8; bicdb_storage::page::PAGE_SIZE];
    arr.copy_from_slice(body);
    let header = Page::from_bytes(Box::new(arr)).header().expect("成型的页");
    assert_eq!(header.file_id, F);
    assert_eq!(header.block_id, *block, "镜像的页头自述块号与键一致");
    assert!(reopen_redo.is_empty(), "重开后的只读路径不产生写");
}

#[test]
fn pool_store_reads_through_the_cache_and_writes_are_redo_described() {
    // 存取口的最小语义：read 经池（命中拷副本）、write 交给 IndexIo
    // （before/after 都在镜像里）、allocate 走段。
    let io = MemFileIo::new();
    io.add_dir("/mem");
    let mut file = DataFile::create(&io, Path::new(DATA_F), F, 3, WS, 512).expect("建文件");
    let handle = file.handle();
    let mut segment = Segment::create(&mut file, SegType::BTree, 1, 1, 8, 0, 0).expect("建段");
    let pool = pool_of(&io, handle);
    let mut redo_log: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut test_io = TestIo {
        pool: &pool,
        segment: &mut segment,
        redo: &mut redo_log,
        lsn: 0,
    };
    {
        let mut store = PoolStore::new(&pool, &mut test_io, F, WS);
        let b = store.allocate().expect("分配");
        let mut page = Page::new(PageType::IndexLeaf, WS, F, b);
        {
            let mut m = bicdb_index::IndexPageMut::new(&mut page).expect("视图");
            m.init(None, None).expect("空叶");
            m.set_links(bicdb_index::store::no_link(), bicdb_index::store::no_link())
                .expect("链");
        }
        store.write(b, &mut page).expect("写页");
        let back = store.read(b).expect("读回");
        let view = IndexPage::new(&back).expect("视图");
        assert_eq!(view.entry_count(), 1, "读回的就是写下去的那页");
        match view.entry(0).expect("高键") {
            Entry::HighKey { key } => assert!(key.is_none(), "∞ 高键"),
            other => panic!("应为高键：{other:?}"),
        }
    }
    assert_eq!(redo_log.len(), 1, "写产生一条 redo 镜像");
    // 页在池里是脏的（写路径标脏）。
    assert_eq!(pool.dirty_len(WS), 1);
}
