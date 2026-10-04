//! 数据文件与文件头页（§3.2 / §5.6 / §5.11）：文件头、位图空间头、
//! LMT 位图区的**尾部增长**、区分配。
//!
//! ```text
//! 块 0            文件头页（类型 11）——内含"位图空间头"
//! 块 1 起         数据区（段的区；块 = 1 + 区号 × 8）
//! 文件尾部区      位图区（每个 = 1 个区 = 8 页；从尾部往前分配）
//!
//! 文件头页页体：偏移 68  file_id 2B │ role 1B │ format_version 1B │ flags 2B │
//!                        当前大小 6B（块数）│ workspace_ref 8B
//!                偏移 88  位图空间头：run_count 1B │ 保留 3B │
//!                        runs[] 4B×40（各位图区的起始块号）│ 保留
//! ```
//!
//! **数据区与位图区相向而行**（§5.11）：数据从块 1 往后、位图区从尾部往前；
//! 两者相遇即"文件满"（明确报错——**增长协议**（把位图区搬到新尾部再追加
//! 数据块）随后切片，见待讨论清单）。
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
/// 建文件的最小块数：块 0 文件头 + 至少一个数据区 + 至少一个位图区。
pub const MIN_FILE_BLOCKS: u64 = 1 + 8 + 8;

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
    /// **数据区与位图区相遇**——文件满（增长协议随后切片）。
    FileFull,
    /// 位图区数到顶。
    TooManyRuns,
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
            DataFileError::FileFull => {
                f.write_str("数据区与位图区相遇——文件满（增长协议随后切片）")
            }
            DataFileError::TooManyRuns => {
                write!(f, "位图区数已达上限 {MAX_BITMAP_RUNS}")
            }
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
        return Err(DataFileError::TooManyRuns);
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
        let mut header = Page::new(PageType::FileHeader, workspace_ref, file_id, 0);
        write_file_head(&mut header, &file.head)?;
        write_bitmap_runs(&mut header, &[])?;
        file.write_page(0, &mut header)?;
        file.allocate_bitmap_run()?;
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
        for &run in &runs {
            if u64::from(run) + BITMAP_PAGES_PER_RUN as u64 > head.blocks {
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

    /// **数据区上限**（块号，开区间）：最靠内的位图区起点；无位图区时为文件尾。
    #[must_use]
    pub fn data_limit(&self) -> u32 {
        self.runs
            .iter()
            .copied()
            .min()
            .unwrap_or(self.head.blocks as u32)
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

    /// **在尾部建立一个位图区**（数据区与位图区相向而行；相遇即文件满）。
    ///
    /// 返回位图区号（= 顺序号，决定其管理的区号段与 `own_index` 基）。
    pub fn allocate_bitmap_run(&mut self) -> Result<u8, DataFileError> {
        let index = self.runs.len();
        if index >= MAX_BITMAP_RUNS {
            return Err(DataFileError::TooManyRuns);
        }
        let inner = self
            .runs
            .iter()
            .copied()
            .min()
            .unwrap_or(self.head.blocks as u32);
        if u64::from(inner) < 1 + BITMAP_PAGES_PER_RUN as u64 {
            return Err(DataFileError::FileFull);
        }
        let start = inner as u64 - BITMAP_PAGES_PER_RUN as u64;
        let data_end = u64::from(self.data_end_block()?);
        if start < data_end {
            return Err(DataFileError::FileFull); // 数据区已到——相遇
        }

        let base = index as u16 * BITMAP_PAGES_PER_RUN as u16;
        for i in 0..BITMAP_PAGES_PER_RUN {
            let block = (start + i as u64) as u32;
            let mut page = Page::new(
                PageType::Bitmap,
                self.head.workspace_ref,
                self.head.file_id,
                block,
            );
            bitmap::init(&mut page, BitmapKind::ExtentMap, base + i as u16)?;
            self.write_page(block, &mut page)?;
        }
        self.runs.push(start as u32);

        // 更新文件头页的位图空间头。
        let mut header = self.read_page(0)?;
        write_bitmap_runs(&mut header, &self.runs)?;
        self.write_page(0, &mut header)?;
        Ok(index as u8)
    }

    /// **数据区水位**（已分配区的末块；无分配则为块 1）。
    fn data_end_block(&self) -> Result<u32, DataFileError> {
        let mut end = 1u32;
        for idx in 0..self.runs.len() {
            let map = self.load_run(idx)?;
            if let Some(e) = map.highest_allocated() {
                end = end.max(e.first_block() + e.blocks());
            }
        }
        Ok(end)
    }

    /// **分配一个区**：按位图区顺序取最低空闲区；候选越出数据区上限
    /// （撞上位图区）即回收该位并报 [`DataFileError::FileFull`]。
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
    use bicdb_workspace::io::MemFileIo;

    use super::*;

    const F: &str = "/mem/data1.dat";
    const WS: [u8; 8] = [9, 9, 9, 9, 9, 9, 9, 9];

    fn mem() -> MemFileIo {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        io
    }

    #[test]
    fn create_lays_out_header_and_tail_run() {
        let io = mem();
        let mut file = DataFile::create(&io, Path::new(F), 3, 3, WS, 64).unwrap();
        assert_eq!(file.file_id(), 3);
        assert_eq!(file.blocks(), 64);
        assert_eq!(file.runs(), &[56], "位图区在尾部（64−8）");
        assert_eq!(file.data_limit(), 56);

        let header = file.read_page(0).unwrap();
        let head = read_file_head(&header).unwrap();
        assert_eq!(head.file_id, 3);
        assert_eq!(head.role, 3);
        assert_eq!(head.blocks, 64);
        assert_eq!(head.workspace_ref, WS);
        assert_eq!(read_bitmap_runs(&header).unwrap(), vec![56]);

        // 位图页：kind = 区分配图、own_index = 0..8。
        let p = file.read_page(56).unwrap();
        assert_eq!(bitmap::kind(&p).unwrap(), BitmapKind::ExtentMap);
        assert_eq!(bitmap::own_index(&p).unwrap(), 0);

        // 第一个区 = 块 1..9。
        let e = file.allocate_extent().unwrap();
        assert_eq!((e.as_raw(), e.first_block()), (0, 1));
        file.sync().unwrap();
    }

    #[test]
    fn allocation_stops_at_data_limit_and_does_not_leak_bits() {
        let io = mem();
        // 17 块：仅容一个区（块 1..9）+ 位图区（块 9..17）。
        let mut file = DataFile::create(&io, Path::new(F), 3, 3, WS, 17).unwrap();
        assert_eq!(file.data_limit(), 9);
        let e = file.allocate_extent().unwrap();
        assert_eq!(e.first_block(), 1);
        // 第二个区会越出上限（9+8 = 17 > 9）⇒ 文件满，且**位不泄漏**。
        assert!(matches!(
            file.allocate_extent(),
            Err(DataFileError::FileFull)
        ));
        let map = file.load_run(0).unwrap();
        assert_eq!(map.allocated(), 1, "失败的分配不留下已分配位");
    }

    #[test]
    fn reopen_preserves_head_and_runs() {
        let io = mem();
        {
            let file = DataFile::create(&io, Path::new(F), 5, 1, WS, 32).unwrap();
            file.sync().unwrap();
            file.close().unwrap();
        }
        let mut file = DataFile::open(&io, Path::new(F)).unwrap();
        assert_eq!(file.file_id(), 5);
        assert_eq!(file.blocks(), 32);
        assert_eq!(file.runs(), &[24]);
        // 重开后分配照常。
        assert_eq!(file.allocate_extent().unwrap().first_block(), 1);
        assert_eq!(file.allocate_extent().unwrap().first_block(), 9);
    }

    #[test]
    fn two_runs_meet_data_area() {
        let io = mem();
        // 33 块：位图区 0 = 25..33；再建位图区 1 = 17..25。
        let mut file = DataFile::create(&io, Path::new(F), 3, 3, WS, 33).unwrap();
        assert_eq!(file.runs(), &[25]);
        let idx = file.allocate_bitmap_run().unwrap();
        assert_eq!(idx, 1);
        assert_eq!(file.runs(), &[25, 17]);
        assert_eq!(file.data_limit(), 17);
        // 区 0、1（块 1..17）可分配；再往下撞位图区 ⇒ 满。
        assert_eq!(file.allocate_extent().unwrap().first_block(), 1);
        assert_eq!(file.allocate_extent().unwrap().first_block(), 9);
        assert!(matches!(
            file.allocate_extent(),
            Err(DataFileError::FileFull)
        ));
    }

    #[test]
    fn small_file_is_rejected() {
        let io = mem();
        assert!(matches!(
            DataFile::create(&io, Path::new(F), 3, 3, WS, 16),
            Err(DataFileError::SmallFile { .. })
        ));
    }
}
