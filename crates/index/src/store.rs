//! **索引页的存取口**（B+Tree 与"页从哪来"之间的缝）。
//!
//! 树逻辑只依赖这三个动作：读一页、写一页、**分配一张新页**——段/缓冲池/事务
//! 那一侧（`bicdb-storage` 的 `Segment` + `BufferPool`）由执行器接入时实现本
//! trait；测试用内存实现（[`MemStore`]），于是树的算法可以脱离 I/O 单测。
//!
//! **并发口径**（§9.1.5）：latch 由实现方提供（池的页闩锁/未来每页内容锁）；
//! 本模块的单线程实现以"**一次一个页**"的纪律串行化——右移（叶链）、下行
//! 闩耦合（父→子）都在这条纪律下成立。

use bicdb_storage::buffer::{BufferError, BufferKey, BufferPool};
use bicdb_storage::page::{Page, PageType};
use bicdb_storage::rowid::{Rdba, RowId, ROWID_LEN};
use bicdb_storage::segment::Segment;

use crate::IndexError;

/// 页存取口（块号寻址）。
pub trait PageStore {
    /// 读一页。
    fn read(&mut self, block: u32) -> Result<Page, IndexError>;
    /// 写一页（**写者负责 seal**；实现方可以顺手落盘）。
    fn write(&mut self, block: u32, page: &mut Page) -> Result<(), IndexError>;
    /// **分配一张新页**（返回块号；内容未格式化）。
    fn allocate(&mut self) -> Result<u32, IndexError>;
    /// 页大小（测试/断言用）。
    fn block_count(&self) -> u32;
    /// **本段索引页的物理序枚举**（FFS 用，§9.1.6）：块号**升序** = 区序。
    ///
    /// 实现方只列**索引页**（叶/枝/根）——段头页与段内位图页不属于树，
    /// 由执行器的实现侧排除；FFS 对枚举到的每一页做页类型与块号自证。
    fn blocks(&mut self) -> Result<Vec<u32>, IndexError>;
    /// **区读（多块读）**（§5.12）：一次读取 `[first, first + count)` 的
    /// **连续**块，返回与请求**同序**的页。
    ///
    /// 调用方保证：块号连续、落在同一文件内、`count ≥ 1`（FFS 按物理区分组，
    /// 上限见 [`crate::tree::FFS_RUN_PAGES`]）。池形态下命中页拷副本、缺失页
    /// 一次 `pread` 读入（不是逐页读盘）。
    fn read_run(&mut self, first: u32, count: u32) -> Result<Vec<Page>, IndexError>;
}

/// **ROWID ↔ 块号**：索引树的页地址（file_id + block）——单文件索引段下
/// `row_id` 恒 0（§9.1.3 的"宽度统一"），`file_id` 由 `Tree` 固定。
#[must_use]
pub fn rid_of(file_id: u16, block: u32) -> RowId {
    RowId::page_address(file_id, block).expect("file_id/block 在域内")
}

/// 由页地址 ROWID 取块号。
#[must_use]
pub fn block_of(rid: RowId) -> u32 {
    rid.block_id()
}

/// 空链标记（`link_prev`/`link_next` 全 0 = 无前驱/后继）。
pub fn no_link() -> RowId {
    RowId::from_bytes(&[0u8; ROWID_LEN])
}

/// 便于测试的零值。
#[allow(dead_code)]
const _: [u8; ROWID_LEN] = [0u8; ROWID_LEN];

/// **执行器接入的 I/O 口**（索引页的**分配**与**写**）。
///
/// 树算法只经 [`PageStore`] 说话；这一层把"新页从哪来、改页怎么写"交给
/// 执行器——写路径**必须带 redo**（§9.1.2：索引项随行的删除/插入移除、
/// **不做独立的撤销**，但索引页与数据页一样受 WAL 保护：写前先落日志、
/// 恢复幂等重放；分裂记录的**正文随记录**（§9.1.5 第 5 步）⇒ 盘上不存在的
/// 新页也能被重放重建）。
pub trait IndexIo {
    /// **分配一张新页**（段空间管理；返回块号，内容由调用方随后写入）。
    fn allocate_page(&mut self) -> Result<u32, IndexError>;
    /// **把一页的新内容写下去**：执行器实现为"经池改页 + redo + 标脏"；
    /// 盘上尚无该页（新分配）⇒ 直接以 `after` 装入。
    fn apply_page(&mut self, block: u32, after: &Page) -> Result<(), IndexError>;
    /// **本段索引页的物理序块号**（升序）——[`PageStore::blocks`] 的落点；
    /// 执行器从段空间管理取（排除段头页与段内位图页）。
    fn allocated_blocks(&mut self) -> Result<Vec<u32>, IndexError>;
}

/// **池存取口**（执行器接入的最小形态）：读经**缓冲池**（命中拷副本；
/// 未命中由池的定位器读盘），写与分配经 [`IndexIo`]。
///
/// **一次一个页**（§9.1.5）：树算法每次只读写一页——池的卫兵（内容锁）在
/// 每次 `read`/`write` 内取放，**不跨调用持有**。
pub struct PoolStore<'a, 'b, 'io, I: IndexIo> {
    pool: &'a BufferPool<'b>,
    io: &'io mut I,
    file_id: u16,
    ws: [u8; 8],
}

impl<'a, 'b, 'io, I: IndexIo> PoolStore<'a, 'b, 'io, I> {
    /// 打开存取口（`ws` = 工作区标识；页键 =（工作区, `file_id` + 块号））。
    pub fn new(pool: &'a BufferPool<'b>, io: &'io mut I, file_id: u16, ws: [u8; 8]) -> Self {
        Self {
            pool,
            io,
            file_id,
            ws,
        }
    }

    fn key_of(&self, block: u32) -> Result<BufferKey, IndexError> {
        let rdba = Rdba::from_parts(self.file_id, block)
            .ok_or(IndexError::Malformed("块号越出 ROWID 域"))?;
        Ok(BufferKey::new(self.ws, rdba))
    }
}

impl<I: IndexIo> PageStore for PoolStore<'_, '_, '_, I> {
    fn read(&mut self, block: u32) -> Result<Page, IndexError> {
        let key = self.key_of(block)?;
        match self.pool.pin(key) {
            Ok(guard) => Ok(Page::from_bytes(Box::new(*guard.as_bytes()))),
            Err(BufferError::Unresolved { .. }) => Err(IndexError::BlockNotFound { block }),
            Err(e) => Err(IndexError::Io(e.to_string())),
        }
    }

    fn write(&mut self, block: u32, page: &mut Page) -> Result<(), IndexError> {
        page.seal();
        self.io.apply_page(block, page)
    }

    fn allocate(&mut self) -> Result<u32, IndexError> {
        self.io.allocate_page()
    }

    fn block_count(&self) -> u32 {
        u32::MAX // 池形态没有固定表上限（诊断口；段空间管理负责报满）
    }

    fn blocks(&mut self) -> Result<Vec<u32>, IndexError> {
        self.io.allocated_blocks()
    }

    fn read_run(&mut self, first: u32, count: u32) -> Result<Vec<Page>, IndexError> {
        let rdba = Rdba::from_parts(self.file_id, first)
            .ok_or(IndexError::Malformed("块号越出 ROWID 域"))?;
        self.pool
            .read_run(self.ws, rdba, count)
            .map_err(|e| match e {
                BufferError::Unresolved { rdba } => IndexError::BlockNotFound {
                    block: rdba.block_id(),
                },
                e => IndexError::Io(e.to_string()),
            })
    }
}

/// **只读池存取口**（执行器的**扫描侧**：索引定位只需要读页，不分配、不改页）。
///
/// 与 [`PoolStore`] 同形（读经缓冲池、区读走池的读法），但**写动作一律
/// 响亮失败**——扫描路径不该有写入口（ENG 不变量："读路径不含锁"的姊妹条：
/// 读路径不含写）。
pub struct ReadOnlyStore<'a, 'b> {
    pool: &'a BufferPool<'b>,
    file_id: u16,
    ws: [u8; 8],
}

impl<'a, 'b> ReadOnlyStore<'a, 'b> {
    /// 打开只读存取口（`ws` = 工作区标识；页键 =（工作区, `file_id` + 块号））。
    #[must_use]
    pub fn new(pool: &'a BufferPool<'b>, file_id: u16, ws: [u8; 8]) -> Self {
        Self { pool, file_id, ws }
    }

    fn key_of(&self, block: u32) -> Result<BufferKey, IndexError> {
        let rdba = Rdba::from_parts(self.file_id, block)
            .ok_or(IndexError::Malformed("块号越出 ROWID 域"))?;
        Ok(BufferKey::new(self.ws, rdba))
    }
}

impl PageStore for ReadOnlyStore<'_, '_> {
    fn read(&mut self, block: u32) -> Result<Page, IndexError> {
        let key = self.key_of(block)?;
        match self.pool.pin(key) {
            Ok(guard) => Ok(Page::from_bytes(Box::new(*guard.as_bytes()))),
            Err(BufferError::Unresolved { .. }) => Err(IndexError::BlockNotFound { block }),
            Err(e) => Err(IndexError::Io(e.to_string())),
        }
    }

    fn write(&mut self, _block: u32, _page: &mut Page) -> Result<(), IndexError> {
        Err(IndexError::Io("只读存取口：写动作不可用".to_owned()))
    }

    fn allocate(&mut self) -> Result<u32, IndexError> {
        Err(IndexError::Io("只读存取口：分配不可用".to_owned()))
    }

    fn block_count(&self) -> u32 {
        u32::MAX
    }

    fn blocks(&mut self) -> Result<Vec<u32>, IndexError> {
        // 只读扫描不做 FFS（枚举口属执行器的段侧；见 `Segment::data_blocks`）。
        Err(IndexError::Io("只读存取口：块枚举不可用".to_owned()))
    }

    fn read_run(&mut self, first: u32, count: u32) -> Result<Vec<Page>, IndexError> {
        let rdba = Rdba::from_parts(self.file_id, first)
            .ok_or(IndexError::Malformed("块号越出 ROWID 域"))?;
        self.pool
            .read_run(self.ws, rdba, count)
            .map_err(|e| match e {
                BufferError::Unresolved { rdba } => IndexError::BlockNotFound {
                    block: rdba.block_id(),
                },
                e => IndexError::Io(e.to_string()),
            })
    }
}

/// **内存页仓**（测试用）：固定上限的页数组，`allocate` 递增分配。
#[derive(Debug)]
pub struct MemStore {
    pages: Vec<Option<Page>>,
    file_id: u16,
    ws: [u8; 8],
}

impl MemStore {
    /// 空仓（`capacity` 张页的上限）。
    #[must_use]
    pub fn new(capacity: u32, file_id: u16, ws: [u8; 8]) -> Self {
        Self {
            pages: (0..capacity).map(|_| None).collect(),
            file_id,
            ws,
        }
    }

    /// 文件号（树头 ROWID 用）。
    #[must_use]
    pub fn file_id(&self) -> u16 {
        self.file_id
    }

    /// 已分配页数。
    #[must_use]
    pub fn allocated(&self) -> u32 {
        self.pages.iter().filter(|p| p.is_some()).count() as u32
    }

    /// 分配一张**格式化为指定类型**的页（测试便利）。
    pub fn new_page(&mut self, page_type: PageType) -> Result<u32, IndexError> {
        let block = self.allocate()?;
        let page = Page::new(page_type, self.ws, self.file_id, block);
        self.pages[block as usize] = Some(page);
        Ok(block)
    }
}

impl PageStore for MemStore {
    fn read(&mut self, block: u32) -> Result<Page, IndexError> {
        self.pages
            .get(block as usize)
            .and_then(|p| p.as_ref())
            .cloned()
            .ok_or(IndexError::BlockNotFound { block })
    }

    fn write(&mut self, block: u32, page: &mut Page) -> Result<(), IndexError> {
        if block as usize >= self.pages.len() {
            return Err(IndexError::BlockNotFound { block });
        }
        page.seal();
        self.pages[block as usize] = Some(page.clone());
        Ok(())
    }

    fn allocate(&mut self) -> Result<u32, IndexError> {
        for (i, slot) in self.pages.iter_mut().enumerate() {
            if slot.is_none() {
                // 占位（内容由调用方格式化后 write）。
                *slot = Some(Page::new(
                    PageType::IndexLeaf,
                    self.ws,
                    self.file_id,
                    i as u32,
                ));
                return Ok(i as u32);
            }
        }
        Err(IndexError::StoreFull)
    }

    fn block_count(&self) -> u32 {
        self.pages.len() as u32
    }

    fn blocks(&mut self) -> Result<Vec<u32>, IndexError> {
        Ok(self
            .pages
            .iter()
            .enumerate()
            .filter_map(|(i, p)| p.as_ref().map(|_| i as u32))
            .collect())
    }

    fn read_run(&mut self, first: u32, count: u32) -> Result<Vec<Page>, IndexError> {
        // 内存仓没有"一次读盘"的成本差别：逐页取，语义与池的区读相同。
        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count {
            out.push(self.read(first + i)?);
        }
        Ok(out)
    }
}

/// **段直存取口**（**无池、无 redo**）：把一段当作 B+Tree 的页仓——
/// **建区期**（引导页/自举集：引导页本身不产生 redo，段也是首次创建）与
/// 离线工具的存取口。
///
/// **生产 DDL 不走这里**（建表/建索引经缓冲池 + redo，见执行器的 `IndexIo`
/// 适配）；本存取口的存在理由 = 自举阶段**还没有池与日志可用**。
///
/// 页号语义：对外是**物理块号**（与树里的 `rdba` 一致），对内映射到段的
/// **逻辑页号**（`Segment` 的口径）；映射表在分配时建立。
pub struct SegmentStore<'a, 'io, 'f> {
    segment: &'a mut Segment<'io, 'f>,
    file_id: u16,
    ws: [u8; 8],
    /// 物理块 → 逻辑页（分配时记录）。
    map: std::collections::HashMap<u32, u32>,
}

impl<'a, 'io, 'f> SegmentStore<'a, 'io, 'f> {
    /// 打开（`ws` = 工作区标识；文件号取自段）。
    ///
    /// **建映射**：扫 `逻辑页 0..hwm` 建立 `物理块 → 逻辑页`（既有段因此可
    /// 直接读回；新建段则随分配增量补记）。O(hwm) 一次——本存取口面向
    /// **建区期与离线工具**，不做池化。
    #[must_use]
    pub fn new(segment: &'a mut Segment<'io, 'f>, ws: [u8; 8]) -> Self {
        let file_id = segment.file_id();
        let mut map = std::collections::HashMap::new();
        // 快照先取（避免与 `&mut segment` 的借用冲突）。
        let hwm = segment.hwm();
        for logical in 0..hwm {
            if let Some(block) = segment.logical_block(logical) {
                map.insert(block, logical);
            }
        }
        Self {
            segment,
            file_id,
            ws,
            map,
        }
    }

    fn logical_of(&self, block: u32) -> Result<u32, IndexError> {
        self.map
            .get(&block)
            .copied()
            .ok_or(IndexError::BlockNotFound { block })
    }
}

impl PageStore for SegmentStore<'_, '_, '_> {
    fn read(&mut self, block: u32) -> Result<Page, IndexError> {
        let logical = self.logical_of(block)?;
        self.segment
            .read_page(logical)
            .map_err(|e| IndexError::Io(e.to_string()))
    }

    fn write(&mut self, block: u32, page: &mut Page) -> Result<(), IndexError> {
        let logical = self.logical_of(block)?;
        self.segment
            .write_page(logical, page)
            .map_err(|e| IndexError::Io(e.to_string()))
    }

    fn allocate(&mut self) -> Result<u32, IndexError> {
        IndexIo::allocate_page(self)
    }

    fn block_count(&self) -> u32 {
        self.map.len() as u32
    }

    fn blocks(&mut self) -> Result<Vec<u32>, IndexError> {
        // **只列索引页**（页类型 2/3/4）：段头（逻辑 0）与段内位图页不属于树
        // （`PageStore::blocks` 的口径）。页类型从页头取——顺带做了自证。
        let mut out = Vec::new();
        let mut pairs: Vec<(u32, u32)> = self.map.iter().map(|(&b, &l)| (b, l)).collect();
        pairs.sort_unstable();
        for (block, logical) in pairs {
            let Ok(page) = self.segment.read_page(logical) else {
                continue;
            };
            if matches!(
                page.header().map(|h| h.page_type),
                Some(PageType::IndexLeaf | PageType::IndexBranch | PageType::IndexRoot)
            ) {
                out.push(block);
            }
        }
        Ok(out)
    }

    fn read_run(&mut self, first: u32, count: u32) -> Result<Vec<Page>, IndexError> {
        // 段的区读口（一次读 ≤ N 页）在 `Segment` 层；此处逐页读语义等价
        // （建区期数据量小，不值得另设口）。
        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count {
            out.push(self.read(first + i)?);
        }
        Ok(out)
    }
}

impl IndexIo for SegmentStore<'_, '_, '_> {
    fn allocate_page(&mut self) -> Result<u32, IndexError> {
        let logical = self
            .segment
            .allocate_append_page()
            .map_err(|e| IndexError::Io(e.to_string()))?;
        if self.segment.logical_block(logical).is_none() {
            self.segment
                .extend()
                .map_err(|e| IndexError::Io(e.to_string()))?;
        }
        let block = self
            .segment
            .logical_block(logical)
            .ok_or(IndexError::Malformed("逻辑页无物理块"))?;
        self.map.insert(block, logical);
        Ok(block)
    }

    fn apply_page(&mut self, block: u32, after: &Page) -> Result<(), IndexError> {
        let logical = self.logical_of(block)?;
        let mut page = Page::from_bytes(Box::new(*after.as_bytes()));
        self.segment
            .write_page(logical, &mut page)
            .map_err(|e| IndexError::Io(e.to_string()))
    }

    fn allocated_blocks(&mut self) -> Result<Vec<u32>, IndexError> {
        self.blocks()
    }
}

impl SegmentStore<'_, '_, '_> {
    /// 文件号（诊断）。
    #[must_use]
    pub fn file_id(&self) -> u16 {
        self.file_id
    }

    /// 工作区标识（诊断）。
    #[must_use]
    pub fn workspace(&self) -> [u8; 8] {
        self.ws
    }
}
