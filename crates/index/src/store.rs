//! **索引页的存取口**（B+Tree 与"页从哪来"之间的缝）。
//!
//! 树逻辑只依赖这三个动作：读一页、写一页、**分配一张新页**——段/缓冲池/事务
//! 那一侧（`bicdb-storage` 的 `Segment` + `BufferPool`）由执行器接入时实现本
//! trait；测试用内存实现（[`MemStore`]），于是树的算法可以脱离 I/O 单测。
//!
//! **并发口径**（§9.1.5）：latch 由实现方提供（池的页闩锁/未来每页内容锁）；
//! 本模块的单线程实现以"**一次一个页**"的纪律串行化——右移（叶链）、下行
//! 闩耦合（父→子）都在这条纪律下成立。

use bicdb_storage::page::{Page, PageType};
use bicdb_storage::rowid::{RowId, ROWID_LEN};

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
}
