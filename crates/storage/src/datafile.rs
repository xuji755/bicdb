//! 数据文件与文件头页（§3.2 / §5.6 / §5.11）：文件头、位图空间头、
//! **预留式增长**、区分配。
//!
//! ```text
//! 块 0                     文件头页（类型 11）——内含"位图空间头"
//! 块 1..=1+320             位图区（**全量预留**：40 区 × 8 页 = 320 页，
//!                          块 1 起连续；一次建好、此后**永不搬移**）
//! 块 321 起                数据区（段的区；块 = 321 + 区号 × 8）
//!
//! 文件头页页体：偏移 68  file_id 2B │ role 1B │ format_version 1B │ flags 2B │
//!                        当前大小 6B（块数）│ workspace_ref 8B
//!                偏移 88  位图空间头：run_count 1B │ 保留 3B │
//!                        runs[] 4B×40（各位图区的起始块号）│ 保留
//! ```
//!
//! **预留式增长**（2026-10-05 定案）：位图区按**该文件能长到的最大规模**
//! 一次预留够（40 区覆盖 ≈ 5 TB > 4 TiB 的文件上限），于是——
//! **增长 = 纯尾部追加数据块**（[`DataFile::extend`]）：不搬位图、不改格式、
//! 不与数据区"相向而行"。代价是每个文件固定多占 5 MiB（40 × 8 × 16 KiB），
//! 换来增长路径零复制——按"性能最好"取舍。
//!
//! **"当前大小"记在文件头**（§2.6 的对偶）：控制文件只存创建时大小，
//! 可变量以文件自身为准——扩展因此不必碰控制文件的双副本。

use std::io;
use std::path::Path;

use bicdb_workspace::io::{FileHandle, FileIo, OpenOptions};

use crate::bitmap::{self, BitmapError, BitmapKind, ExtentMap, ExtentNo, BITMAP_PAGES_PER_RUN};
use crate::page::{Page, PageType, PAGE_SIZE};
use crate::pagefile;

/// 文件头页页体内的字段起点。
pub const FILE_HEADER_BODY_OFFSET: usize = 68;
/// 位图空间头在页体内的偏移（file_id 2 + role 1 + version 1 + flags 2 +
/// 当前大小 6 + workspace_ref 8 = 68..88）。
pub const BITMAP_SPACE_HEAD_OFFSET: usize = 88;
/// 位图区起始块号数组的偏移（run_count 1B + 保留 3B）。
pub const BITMAP_RUNS_OFFSET: usize = 92;
/// 位图区上限（§5.11：4 TiB 上限下 ≤ 33，留余量到 40）。
pub const MAX_BITMAP_RUNS: usize = 40;
/// 建文件的最小块数：块 0 文件头 + **全量预留的位图区**（320 页）+ 至少一个数据区。
pub const MIN_FILE_BLOCKS: u64 = crate::bitmap::DATA_AREA_FIRST_BLOCK as u64 + 8;

/// 计划中的区分配（[`DataFile::plan_allocate_extent`]）。
pub struct PlannedExtent {
    /// 分配的区号。
    pub extent: ExtentNo,
    /// 位图区在文件内的起始块。
    pub run_start: u32,
    /// 受影响页：（区内页号, 前像, 后像）。
    pub images: Vec<(u8, Page, Page)>,
}

fn clone_page(p: &Page) -> Page {
    Page::from_bytes(Box::new(*p.as_bytes()))
}

/// 数据文件错误（**明确判定**）。
#[derive(Debug)]
pub enum DataFileError {
    /// 底层 I/O。
    Io(io::Error),
    /// 块 0 不是文件头页。
    NotAFileHeader,
    /// 位图空间头越界/格式非法。
    Malformed,
    /// 位图页错误（own_index 不符等）。
    Bitmap(BitmapError),
    /// 文件太小（低于 [`MIN_FILE_BLOCKS`]）。
    SmallFile {
        /// 请求的块数。
        blocks: u64,
        /// 最小块数。
        min: u64,
    },
    /// 数据区已到上限（文件大小 / 预留覆盖）——文件满；增长走 [`DataFile::extend`]。
    FileFull,
    /// `extend` 的目标不大于当前大小（含缩小）——拒绝。
    NotGrowing {
        /// 当前块数。
        blocks: u64,
        /// 请求的块数。
        requested: u64,
    },
    /// 越过预留位图区的覆盖上限（文件能长到的最大规模）。
    BeyondCoverage {
        /// 请求的块数。
        requested: u64,
        /// 覆盖上限（块）。
        limit: u64,
    },
    /// 本页不是该文件应有的块（块号越界）。
    BlockOutOfRange {
        /// 块号。
        block: u32,
    },
}

impl std::fmt::Display for DataFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DataFileError::Io(e) => write!(f, "数据文件 I/O：{e}"),
            DataFileError::NotAFileHeader => f.write_str("块 0 不是文件头页"),
            DataFileError::Malformed => f.write_str("文件头页/位图空间头字段越界"),
            DataFileError::Bitmap(e) => write!(f, "位图：{e}"),
            DataFileError::SmallFile { blocks, min } => {
                write!(f, "文件 {blocks} 块低于最小 {min} 块")
            }
            DataFileError::FileFull => f.write_str("数据区已到上限——文件满（增长走 extend）"),
            DataFileError::NotGrowing { blocks, requested } => write!(
                f,
                "文件增长要求更大的尺寸：当前 {blocks} 块，请求 {requested} 块"
            ),
            DataFileError::BeyondCoverage { requested, limit } => write!(
                f,
                "文件尺寸越过预留位图区覆盖上限：请求 {requested} 块，上限 {limit} 块"
            ),
            DataFileError::BlockOutOfRange { block } => write!(f, "块 {block} 越出文件"),
        }
    }
}

impl std::error::Error for DataFileError {}

impl From<io::Error> for DataFileError {
    fn from(e: io::Error) -> Self {
        DataFileError::Io(e)
    }
}

impl From<BitmapError> for DataFileError {
    fn from(e: BitmapError) -> Self {
        DataFileError::Bitmap(e)
    }
}

/// 文件头（页体值形态；§5.6）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileHead {
    /// 文件号（与 ROWID 的 `file_id` 一致）。
    pub file_id: u16,
    /// 角色（0 元数据 / 1 Undo / 2 Temp / 3+ 数据，§2.2）。
    pub role: u8,
    /// 格式版本。
    pub format_version: u8,
    /// 标志位。
    pub flags: u16,
    /// **当前大小**（块数；6B——权威在这里，不在控制文件）。
    pub blocks: u64,
    /// 工作区受校验标识（与页头同源；"打开错了文件"的快速否决）。
    pub workspace_ref: [u8; 8],
}

/// 读文件头。
pub fn read_file_head(page: &Page) -> Result<FileHead, DataFileError> {
    if page.header().map(|h| h.page_type) != Some(PageType::FileHeader) {
        return Err(DataFileError::NotAFileHeader);
    }
    let b = page.as_bytes();
    let mut blocks = [0u8; 8];
    blocks[..6].copy_from_slice(&b[FILE_HEADER_BODY_OFFSET + 6..FILE_HEADER_BODY_OFFSET + 12]);
    let mut workspace_ref = [0u8; 8];
    workspace_ref.copy_from_slice(&b[FILE_HEADER_BODY_OFFSET + 12..FILE_HEADER_BODY_OFFSET + 20]);
    Ok(FileHead {
        file_id: u16::from_le_bytes([b[FILE_HEADER_BODY_OFFSET], b[FILE_HEADER_BODY_OFFSET + 1]]),
        role: b[FILE_HEADER_BODY_OFFSET + 2],
        format_version: b[FILE_HEADER_BODY_OFFSET + 3],
        flags: u16::from_le_bytes([
            b[FILE_HEADER_BODY_OFFSET + 4],
            b[FILE_HEADER_BODY_OFFSET + 5],
        ]),
        blocks: u64::from_le_bytes(blocks),
        workspace_ref,
    })
}

/// 写文件头。
pub fn write_file_head(page: &mut Page, h: &FileHead) -> Result<(), DataFileError> {
    if page.header().map(|x| x.page_type) != Some(PageType::FileHeader) {
        return Err(DataFileError::NotAFileHeader);
    }
    let b = page.as_bytes_mut();
    b[FILE_HEADER_BODY_OFFSET..FILE_HEADER_BODY_OFFSET + 2]
        .copy_from_slice(&h.file_id.to_le_bytes());
    b[FILE_HEADER_BODY_OFFSET + 2] = h.role;
    b[FILE_HEADER_BODY_OFFSET + 3] = h.format_version;
    b[FILE_HEADER_BODY_OFFSET + 4..FILE_HEADER_BODY_OFFSET + 6]
        .copy_from_slice(&h.flags.to_le_bytes());
    b[FILE_HEADER_BODY_OFFSET + 6..FILE_HEADER_BODY_OFFSET + 12]
        .copy_from_slice(&h.blocks.to_le_bytes()[..6]);
    b[FILE_HEADER_BODY_OFFSET + 12..FILE_HEADER_BODY_OFFSET + 20].copy_from_slice(&h.workspace_ref);
    Ok(())
}

/// 读位图空间头（各位图区的起始块号）。
pub fn read_bitmap_runs(page: &Page) -> Result<Vec<u32>, DataFileError> {
    if page.header().map(|h| h.page_type) != Some(PageType::FileHeader) {
        return Err(DataFileError::NotAFileHeader);
    }
    let b = page.as_bytes();
    let count = usize::from(b[BITMAP_SPACE_HEAD_OFFSET]);
    if count > MAX_BITMAP_RUNS {
        return Err(DataFileError::Malformed);
    }
    let mut runs = Vec::with_capacity(count);
    for i in 0..count {
        let at = BITMAP_RUNS_OFFSET + i * 4;
        runs.push(u32::from_le_bytes(
            b[at..at + 4].try_into().expect("4 字节"),
        ));
    }
    Ok(runs)
}

/// 写位图空间头。
pub fn write_bitmap_runs(page: &mut Page, runs: &[u32]) -> Result<(), DataFileError> {
    if page.header().map(|h| h.page_type) != Some(PageType::FileHeader) {
        return Err(DataFileError::NotAFileHeader);
    }
    if runs.len() > MAX_BITMAP_RUNS {
        return Err(DataFileError::Malformed);
    }
    let b = page.as_bytes_mut();
    b[BITMAP_SPACE_HEAD_OFFSET] = runs.len() as u8;
    b[BITMAP_SPACE_HEAD_OFFSET + 1..BITMAP_SPACE_HEAD_OFFSET + 4].fill(0);
    for (i, run) in runs.iter().enumerate() {
        let at = BITMAP_RUNS_OFFSET + i * 4;
        b[at..at + 4].copy_from_slice(&run.to_le_bytes());
    }
    // 其后清零（缩短时不留残影）。
    for i in runs.len()..MAX_BITMAP_RUNS {
        let at = BITMAP_RUNS_OFFSET + i * 4;
        b[at..at + 4].fill(0);
    }
    Ok(())
}

/// 数据文件（绑定 [`FileIo`] 句柄；文件头与位图空间头的内存镜像）。
pub struct DataFile<'a> {
    io: &'a dyn FileIo,
    handle: FileHandle,
    head: FileHead,
    runs: Vec<u32>,
}

impl std::fmt::Debug for DataFile<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DataFile")
            .field("file_id", &self.head.file_id)
            .field("blocks", &self.head.blocks)
            .field("runs", &self.runs)
            .finish_non_exhaustive()
    }
}

impl<'a> DataFile<'a> {
    /// **新建数据文件**：写块 0（文件头 + 空位图空间头）、在尾部建立一个
    /// 位图区（8 张区分配图页）。
    pub fn create(
        io: &'a dyn FileIo,
        path: &Path,
        file_id: u16,
        role: u8,
        workspace_ref: [u8; 8],
        blocks: u64,
    ) -> Result<Self, DataFileError> {
        if blocks < MIN_FILE_BLOCKS {
            return Err(DataFileError::SmallFile {
                blocks,
                min: MIN_FILE_BLOCKS,
            });
        }
        // 创建也受硬上限约束（块号 28 位 / 位图覆盖）——否则可建出"块号
        // 编不出来"的文件，其后每次分配区都在 `Rdba` 处失败。
        let hard = crate::bitmap::DATA_AREA_FIRST_BLOCK as u64
            + MAX_BITMAP_RUNS as u64
                * crate::bitmap::BITS_PER_RUN as u64
                * crate::bitmap::EXTENT_BLOCKS as u64;
        let hard = hard.min(1u64 << 28);
        if blocks > hard {
            return Err(DataFileError::BeyondCoverage {
                requested: blocks,
                limit: hard,
            });
        }
        let handle = io.open(
            path,
            OpenOptions::new().read(true).write(true).create_new(true),
        )?;
        io.set_len(handle, blocks * PAGE_SIZE as u64)?;
        let mut file = Self {
            io,
            handle,
            head: FileHead {
                file_id,
                role,
                format_version: crate::page::FORMAT_VERSION,
                flags: 0,
                blocks,
                workspace_ref,
            },
            runs: Vec::new(),
        };
        // **位图区全量预留**：块 1 起连续 40 区 × 8 页，一次建好、永不搬移。
        let runs: Vec<u32> = (0..MAX_BITMAP_RUNS as u32)
            .map(|k| 1 + k * BITMAP_PAGES_PER_RUN as u32)
            .collect();
        let mut header = Page::new(PageType::FileHeader, workspace_ref, file_id, 0);
        write_file_head(&mut header, &file.head)?;
        write_bitmap_runs(&mut header, &runs)?;
        file.write_page(0, &mut header)?;
        file.runs = runs;
        for (k, &start) in file.runs.clone().iter().enumerate() {
            let base = k as u16 * BITMAP_PAGES_PER_RUN as u16;
            for i in 0..BITMAP_PAGES_PER_RUN {
                let block = start + i as u32;
                let mut page = Page::new(PageType::Bitmap, workspace_ref, file_id, block);
                bitmap::init(&mut page, BitmapKind::ExtentMap, base + i as u16)?;
                file.write_page(block, &mut page)?;
            }
        }
        Ok(file)
    }

    /// **打开既有数据文件**（校验块 0 与位图空间头；不做完整性全扫）。
    pub fn open(io: &'a dyn FileIo, path: &Path) -> Result<Self, DataFileError> {
        let handle = io.open(path, OpenOptions::new().read(true).write(true))?;
        let header = pagefile::read_page_verified(io, handle, 0).map_err(|e| match e {
            pagefile::PageFileError::Io(e) => DataFileError::Io(e),
            pagefile::PageFileError::Damaged { .. } => DataFileError::Malformed,
        })?;
        let head = read_file_head(&header)?;
        let runs = read_bitmap_runs(&header)?;
        // 预留式布局的形状校验：**40 区、块 1 起连续**。
        if runs.len() != MAX_BITMAP_RUNS {
            return Err(DataFileError::Malformed);
        }
        for (k, &run) in runs.iter().enumerate() {
            if run != 1 + k as u32 * BITMAP_PAGES_PER_RUN as u32
                || u64::from(run) + BITMAP_PAGES_PER_RUN as u64 > head.blocks
            {
                return Err(DataFileError::Malformed);
            }
        }
        Ok(Self {
            io,
            handle,
            head,
            runs,
        })
    }

    /// 文件句柄（构建 `rdba → (句柄, 块号)` 解析器用）。
    #[must_use]
    pub fn handle(&self) -> FileHandle {
        self.handle
    }

    /// 文件号。
    #[must_use]
    pub fn file_id(&self) -> u16 {
        self.head.file_id
    }

    /// 工作区受校验标识。
    #[must_use]
    pub fn workspace_ref(&self) -> [u8; 8] {
        self.head.workspace_ref
    }

    /// 当前大小（块数）。
    #[must_use]
    pub fn blocks(&self) -> u64 {
        self.head.blocks
    }

    /// 位图区起始块号（按建立顺序）。
    #[must_use]
    pub fn runs(&self) -> &[u32] {
        &self.runs
    }

    /// **数据区上限**（块号，开区间）：文件大小与 [`DataFile::block_limit`] 的较小者。
    #[must_use]
    pub fn data_limit(&self) -> u32 {
        self.head.blocks.min(self.block_limit()) as u32
    }

    /// 预留位图区的覆盖上限（块数）——位图能寻址的最大规模。
    #[must_use]
    pub fn coverage_limit(&self) -> u64 {
        crate::bitmap::DATA_AREA_FIRST_BLOCK as u64
            + MAX_BITMAP_RUNS as u64
                * crate::bitmap::BITS_PER_RUN as u64
                * crate::bitmap::EXTENT_BLOCKS as u64
    }

    /// **文件块数硬上限** = min（位图覆盖上限，**块号 28 位上限**）。
    /// 后者是 `Rdba`/`ROWID` 的编码位宽（`1 << 28` 块 = 4 TiB）——越过它，
    /// 块号不再可编址（`Rdba::from_parts` 会失败）。
    #[must_use]
    pub fn block_limit(&self) -> u64 {
        self.coverage_limit().min(1u64 << 28)
    }

    /// **读一页但不做完整性校验**（"这一页写没写过"的探测用：未写过的零页
    /// 过不了校验，但那不是损坏）。I/O 错误照常外传。
    pub fn read_page_unverified(&self, block: u32) -> Result<Page, DataFileError> {
        if u64::from(block) >= self.head.blocks {
            return Err(DataFileError::BlockOutOfRange { block });
        }
        pagefile::read_page(self.io, self.handle, block).map_err(DataFileError::Io)
    }

    /// 读一页（两层完整性校验）。
    pub fn read_page(&self, block: u32) -> Result<Page, DataFileError> {
        if u64::from(block) >= self.head.blocks {
            return Err(DataFileError::BlockOutOfRange { block });
        }
        pagefile::read_page_verified(self.io, self.handle, block).map_err(|e| match e {
            pagefile::PageFileError::Io(e) => DataFileError::Io(e),
            pagefile::PageFileError::Damaged { .. } => DataFileError::Malformed,
        })
    }

    /// 写一页（seal + 定址写；不隐式 fsync）。
    pub fn write_page(&self, block: u32, page: &mut Page) -> Result<(), DataFileError> {
        if u64::from(block) >= self.head.blocks {
            return Err(DataFileError::BlockOutOfRange { block });
        }
        pagefile::write_page(self.io, self.handle, block, page)?;
        Ok(())
    }

    /// 持久性点。
    pub fn sync(&self) -> Result<(), DataFileError> {
        self.io.sync_data(self.handle)?;
        Ok(())
    }

    /// 关闭句柄。
    pub fn close(self) -> Result<(), DataFileError> {
        self.io.close(self.handle)?;
        Ok(())
    }

    /// **文件增长**（§3.3）：尾部追加。位图区**全量预留且在前部**——增长
    /// 不碰上它，因此就是 `set_len` + 更新文件头的"当前大小"，**零搬移**。
    ///
    /// 上限 = 预留位图区的覆盖（≈ 5 TB，覆盖 4 TiB 的文件上限）。
    pub fn extend(&mut self, new_blocks: u64) -> Result<(), DataFileError> {
        if new_blocks <= self.head.blocks {
            return Err(DataFileError::NotGrowing {
                blocks: self.head.blocks,
                requested: new_blocks,
            });
        }
        let limit = self.block_limit();
        if new_blocks > limit {
            return Err(DataFileError::BeyondCoverage {
                requested: new_blocks,
                limit,
            });
        }
        self.io
            .set_len(self.handle, new_blocks * PAGE_SIZE as u64)?;
        // **头页写成功之后**才提交内存尺寸：中途失败时同值重试仍然可行
        // （先改内存会让"磁盘头仍旧值、内存已新值"卡死——`NotGrowing`）。
        let mut header = self.read_page(0)?;
        let mut head = self.head;
        head.blocks = new_blocks;
        write_file_head(&mut header, &head)?;
        self.write_page(0, &mut header)?;
        self.head = head;
        Ok(())
    }

    /// **计划分配一个区**（**不写盘**）：返回区号 + 位图区的起始块 +
    /// 受影响页的（页内序号, 前像, 后像）——写路径经缓冲池落盘（生成 redo，
    /// §11.5.3"页/区分配是系统操作"）。**决策已定**（位已置在后像里），
    /// 调用方写盘后即完成。
    pub fn plan_allocate_extent(&mut self) -> Result<PlannedExtent, DataFileError> {
        for idx in 0..self.runs.len() {
            let mut map = self.load_run(idx)?;
            // **前像先拍**（allocate 会就地把位翻过去）。
            let before: Vec<Page> = map.pages().iter().map(clone_page).collect();
            if let Some(extent) = map.allocate() {
                if u64::from(extent.first_block()) + u64::from(extent.blocks())
                    > u64::from(self.data_limit())
                {
                    map.free(extent)?; // 越出上限：不落盘、不改位图
                    return Err(DataFileError::FileFull);
                }
                let mut images = Vec::new();
                for (i, (b, a)) in before.iter().zip(map.pages()).enumerate() {
                    if b.as_bytes() != a.as_bytes() {
                        images.push((i as u8, clone_page(b), clone_page(a)));
                    }
                }
                return Ok(PlannedExtent {
                    extent,
                    run_start: self.runs[idx],
                    images,
                });
            }
        }
        Err(DataFileError::FileFull)
    }

    /// **分配一个区**：按位图区顺序取最低空闲区；候选越出
    /// [`DataFile::data_limit`]（文件大小 / 预留覆盖的较小者）即回收该位并报
    /// [`DataFileError::FileFull`]——增长走 [`DataFile::extend`]。
    pub fn allocate_extent(&mut self) -> Result<ExtentNo, DataFileError> {
        for idx in 0..self.runs.len() {
            let mut map = self.load_run(idx)?;
            if let Some(extent) = map.allocate() {
                if u64::from(extent.first_block()) + u64::from(extent.blocks())
                    > u64::from(self.data_limit())
                {
                    map.free(extent)?; // 越出上限：不落盘、不改位图
                    return Err(DataFileError::FileFull);
                }
                self.persist_run(idx, &map)?;
                return Ok(extent);
            }
        }
        Err(DataFileError::FileFull)
    }

    /// 装载一个位图区（8 页；`own_index` 自校验）。
    fn load_run(&self, index: usize) -> Result<ExtentMap, DataFileError> {
        let start = self.runs[index];
        let mut pages = Vec::with_capacity(BITMAP_PAGES_PER_RUN);
        for i in 0..BITMAP_PAGES_PER_RUN {
            pages.push(self.read_page(start + i as u32)?);
        }
        Ok(ExtentMap::from_pages(index as u8, pages)?)
    }

    /// 写回一个位图区（8 页；seal 由写路径统一完成）。
    fn persist_run(&self, index: usize, map: &ExtentMap) -> Result<(), DataFileError> {
        let start = self.runs[index];
        for (i, page) in map.pages().iter().enumerate() {
            let mut copy = Page::from_bytes(Box::new(*page.as_bytes()));
            self.write_page(start + i as u32, &mut copy)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use bicdb_workspace::io::MemFileIo;

    use super::*;
    use crate::bitmap::{BITS_PER_RUN, DATA_AREA_FIRST_BLOCK, EXTENT_BLOCKS};

    const F: &str = "/mem/data1.dat";
    const WS: [u8; 8] = [9, 9, 9, 9, 9, 9, 9, 9];
    /// 预留 320 页 + 至少一个数据区 ⇒ 400 块起步（测试统一用）。
    const BLOCKS: u64 = 400;

    fn mem() -> MemFileIo {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        io
    }

    #[test]
    fn create_reserves_the_whole_bitmap_area() {
        let io = mem();
        let file = DataFile::create(&io, Path::new(F), 3, 3, WS, BLOCKS).unwrap();
        assert_eq!(file.file_id(), 3);
        assert_eq!(file.blocks(), BLOCKS);
        // 40 个位图区、块 1 起连续（1, 9, ..., 313）。
        let runs = file.runs();
        assert_eq!(runs.len(), MAX_BITMAP_RUNS);
        assert_eq!(runs[0], 1);
        assert_eq!(runs[MAX_BITMAP_RUNS - 1], 1 + 39 * 8);
        for (k, &run) in runs.iter().enumerate() {
            assert_eq!(run, 1 + k as u32 * BITMAP_PAGES_PER_RUN as u32);
        }
        assert_eq!(
            file.data_limit(),
            BLOCKS as u32,
            "数据区上限 = 文件大小（尾部不再有位图）"
        );

        let header = file.read_page(0).unwrap();
        let head = read_file_head(&header).unwrap();
        assert_eq!(head.blocks, BLOCKS);
        assert_eq!(head.workspace_ref, WS);
        assert_eq!(read_bitmap_runs(&header).unwrap(), runs);

        // 位图页：kind = 区分配图、own_index 从 0 起逐页递增。
        let p = file.read_page(1).unwrap();
        assert_eq!(bitmap::kind(&p).unwrap(), BitmapKind::ExtentMap);
        assert_eq!(bitmap::own_index(&p).unwrap(), 0);
        let p = file.read_page(9).unwrap();
        assert_eq!(
            bitmap::own_index(&p).unwrap(),
            8,
            "第 2 区第一页的 own_index"
        );
    }

    #[test]
    fn first_extent_starts_after_the_reserved_area() {
        let io = mem();
        let mut file = DataFile::create(&io, Path::new(F), 3, 3, WS, BLOCKS).unwrap();
        let e = file.allocate_extent().unwrap();
        assert_eq!(
            (e.as_raw(), e.first_block()),
            (0, 321),
            "区 0 从预留区之后起"
        );
        let e = file.allocate_extent().unwrap();
        assert_eq!(e.first_block(), 329);
        file.sync().unwrap();
    }

    #[test]
    fn allocation_stops_at_data_limit_without_leaking_bits() {
        let io = mem();
        // 预留区 + 2 个数据区（块 321..337）。
        let mut file = DataFile::create(&io, Path::new(F), 3, 3, WS, 337).unwrap();
        assert_eq!(file.data_limit(), 337);
        assert_eq!(file.allocate_extent().unwrap().first_block(), 321);
        assert_eq!(file.allocate_extent().unwrap().first_block(), 329);
        assert!(matches!(
            file.allocate_extent(),
            Err(DataFileError::FileFull)
        ));
        assert_eq!(file.load_run(0).unwrap().allocated(), 2, "失败分配不泄漏位");
    }

    #[test]
    fn extend_grows_at_the_tail_without_moving_bitmaps() {
        let io = mem();
        let mut file = DataFile::create(&io, Path::new(F), 3, 3, WS, 337).unwrap();
        let runs_before = file.runs().to_vec();
        assert!(file.allocate_extent().is_ok());
        assert!(file.allocate_extent().is_ok());
        assert!(matches!(
            file.allocate_extent(),
            Err(DataFileError::FileFull)
        ));

        // 增长：纯尾部追加，位图区一字不动。
        file.extend(353).unwrap();
        assert_eq!(file.blocks(), 353);
        assert_eq!(file.runs(), &runs_before[..], "位图区不搬移");
        assert_eq!(file.data_limit(), 353);
        assert_eq!(
            file.allocate_extent().unwrap().first_block(),
            337,
            "新块可用"
        );

        // 文件头页里的"当前大小"已更新（持久化）。
        let head = read_file_head(&file.read_page(0).unwrap()).unwrap();
        assert_eq!(head.blocks, 353);

        // 拒绝：不增长 / 缩小 / 越覆盖上限。
        assert!(matches!(
            file.extend(353),
            Err(DataFileError::NotGrowing { .. })
        ));
        assert!(matches!(
            file.extend(100),
            Err(DataFileError::NotGrowing { .. })
        ));
        assert!(matches!(
            file.extend(file.coverage_limit() + 1),
            Err(DataFileError::BeyondCoverage { .. })
        ));
    }

    #[test]
    fn reopen_preserves_head_runs_and_allocation() {
        let io = mem();
        {
            let file = DataFile::create(&io, Path::new(F), 5, 1, WS, 400).unwrap();
            file.sync().unwrap();
            file.close().unwrap();
        }
        let mut file = DataFile::open(&io, Path::new(F)).unwrap();
        assert_eq!(file.file_id(), 5);
        assert_eq!(file.blocks(), 400);
        assert_eq!(file.runs().len(), MAX_BITMAP_RUNS);
        assert_eq!(file.allocate_extent().unwrap().first_block(), 321);
        assert_eq!(file.allocate_extent().unwrap().first_block(), 329);
    }

    #[test]
    fn malformed_layout_is_rejected_on_open() {
        let io = mem();
        {
            let file = DataFile::create(&io, Path::new(F), 3, 3, WS, 400).unwrap();
            file.close().unwrap();
        }
        // 把 runs[0] 改坏（不再从块 1 起）→ 打开拒绝。
        {
            let h = io
                .open(
                    Path::new(F),
                    bicdb_workspace::io::OpenOptions::new()
                        .read(true)
                        .write(true),
                )
                .unwrap();
            let mut page = pagefile::read_page_verified(&io, h, 0).unwrap();
            let b = page.as_bytes_mut();
            b[BITMAP_RUNS_OFFSET..BITMAP_RUNS_OFFSET + 4].copy_from_slice(&7u32.to_le_bytes());
            pagefile::write_page(&io, h, 0, &mut page).unwrap();
            io.close(h).unwrap();
        }
        assert!(matches!(
            DataFile::open(&io, Path::new(F)),
            Err(DataFileError::Malformed)
        ));
    }

    #[test]
    fn small_file_is_rejected() {
        let io = mem();
        assert!(matches!(
            DataFile::create(&io, Path::new(F), 3, 3, WS, MIN_FILE_BLOCKS - 1),
            Err(DataFileError::SmallFile { .. })
        ));
        // 覆盖上限的定量：40 区 × 每区位数 × 8 块 + 预留。
        let expected = DATA_AREA_FIRST_BLOCK as u64
            + MAX_BITMAP_RUNS as u64 * BITS_PER_RUN as u64 * EXTENT_BLOCKS as u64;
        assert_eq!(
            DataFile::create(&io, Path::new("/mem/data2.dat"), 3, 3, WS, MIN_FILE_BLOCKS)
                .unwrap()
                .coverage_limit(),
            expected
        );
    }

    #[test]
    fn growth_is_capped_by_block_id_width() {
        // 审核修复回归：块号 28 位（4 TiB）是硬上限——位图覆盖（≈5 TiB）
        // 更大也不能越过去（越过即 `Rdba` 编不出来）。
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let mut file = DataFile::create(&io, Path::new("/mem/f.dat"), 1, 1, WS, 512).unwrap();
        assert_eq!(file.block_limit(), 1u64 << 28);
        assert!(file.coverage_limit() > file.block_limit());
        let err = file.extend((1u64 << 28) + 1).unwrap_err();
        assert!(matches!(err, DataFileError::BeyondCoverage { .. }), "{err}");
        // 失败**不推进内存尺寸**（块号上限的失败路径同样如此）——随后
        // 一次正常增长仍然可行、且头页落盘。
        file.extend(600).unwrap();
        assert_eq!(file.blocks(), 600);
    }
}
