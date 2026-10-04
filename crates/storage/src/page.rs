//! 页格式基础：16 KiB 页的描述区 / 事务区 / 空间区、槽位目录与页尾副本。
//!
//! 设计依据：存储架构 §5.3–§5.9（页头一致、**页体按 `page_type` 各自定义**；
//! 本片实现的是**所有页类型共用的头部与页尾**，行区/槽位目录的**内容**由
//! 各页体的实现（堆表页、Undo 页、索引页……）在后续切片中填充）。
//!
//! 字节序：**小端**（REQ-PRT-003：磁盘格式定死字节序）。
//!
//! # 两层完整性机制（互补，不是替代——§5.8）
//!
//! | 机制 | 职责 | 成本 |
//! | --- | --- | --- |
//! | **页尾副本** | 检出**断裂块**（只写了一半）：页尾 4 字节存 `page_type` + `mod_seq` + `page_lsn` 低位 2B 的副本，读页时头尾对比 | 一次 4 字节比较 |
//! | **`checksum`** | 完整**数据完整性**：CRC32C 覆盖全页（字段置零后计算） | 全页计算 |
//!
//! 因此 [`Page::verify`] 先比页尾（廉价）再算校验和；任何字段修改后必须
//! 调用 [`Page::seal`]（写页尾副本 → 重算校验和）。
//!
//! 参考实现是 Oracle 的 `tailchk`（块尾保存 SCN Base 低位、块类型、SCN Seq 的副本）。

pub use bicdb_common::checksum::PAGE_SIZE;
use bicdb_common::checksum::{
    page_checksum, verify_page_checksum, PAGE_CHECKSUM_LEN, PAGE_CHECKSUM_OFFSET,
};
use bicdb_common::seq::Lsn;

// ---------------------------------------------------------------------------
// 偏移表（§5.3–§5.5；固定头 = 描述区 32 + 事务区 32 + 空间区 4 = 68 字节）
// ---------------------------------------------------------------------------

/// 描述区起点。
pub const DESCRIBE_OFFSET: usize = 0;
/// 描述区长度。
pub const DESCRIBE_LEN: usize = 32;
/// 格式版本字段偏移（从 1 起；当前 = [`FORMAT_VERSION`]）。
pub const FORMAT_VERSION_OFFSET: usize = 4;
/// `page_type` 字段偏移。
pub const PAGE_TYPE_OFFSET: usize = 5;
/// `flags` 字段偏移。
pub const FLAGS_OFFSET: usize = 6;
/// `mod_seq` 字段偏移。
pub const MOD_SEQ_OFFSET: usize = 7;
/// `workspace_ref` 字段偏移（受校验的工作区标识；哈希只在打开工作区时算一次）。
pub const WORKSPACE_REF_OFFSET: usize = 8;
/// `workspace_ref` 字段宽度。
pub const WORKSPACE_REF_LEN: usize = 8;
/// `file_id` 字段偏移（有效 10 位）。
pub const FILE_ID_OFFSET: usize = 16;
/// `block_id` 字段偏移（有效 28 位，与 ROWID 一致）。
pub const BLOCK_ID_OFFSET: usize = 18;
/// `page_lsn` 字段偏移（48 位、6 字节）。
pub const PAGE_LSN_OFFSET: usize = 22;
/// `page_lsn` 字段宽度。
pub const PAGE_LSN_LEN: usize = 6;

/// 事务区起点。
pub const TRANSACTION_OFFSET: usize = 32;
/// `itl_count` 字段偏移。
pub const ITL_COUNT_OFFSET: usize = 32;
/// 第一个 ITL 条目的偏移（`itl_max` 是段级属性，不在页头）。
pub const ITL_ENTRY_OFFSET: usize = 40;
/// 单个 ITL 条目长度（其内部字段由事务域切片实现）。
pub const ITL_ENTRY_LEN: usize = 24;

/// 第 `index` 个 ITL 条目的页内偏移。
///
/// **布局不是等距的**：ITL[0] 在 40..64（事务区头 8 + 一条 = 32B 事务区），
/// 空间区紧随其后（64..68）；**扩展出来的 ITL[i]（i ≥ 1）在固定头之后**
/// ——`68 + (i−1)×24`（"每多一个槽页头 +24 字节"，§5.4.1）。空间区的
/// `slot_count`/`free_end` 因此始终在固定偏移 64/66。
#[must_use]
pub const fn itl_entry_offset(index: u16) -> usize {
    if index == 0 {
        ITL_ENTRY_OFFSET
    } else {
        FIXED_HEADER_LEN + (index as usize - 1) * ITL_ENTRY_LEN
    }
}

/// 空间区起点。
pub const SPACE_OFFSET: usize = 64;
/// `slot_count` 字段偏移。
pub const SLOT_COUNT_OFFSET: usize = 64;
/// `free_end` 字段偏移（行区上界；`free_start` 可推导、不存）。
pub const FREE_END_OFFSET: usize = 66;

/// 固定头长度（`itl_count == 1` 时；每多一个 ITL 槽页头 +24 字节）。
pub const FIXED_HEADER_LEN: usize = 68;

/// 槽位目录条目长度。
pub const SLOT_ENTRY_LEN: usize = 2;

/// 槽位上限（D-06：槽位从 1 起、0 保留为"无" ⇒ 行号 1..=1023）。
///
/// **与 ROWID 的 `row_id` 10 位域耦合，两者须同时修改**（§5.7）。
pub const MAX_SLOTS: usize = 1023;

/// `file_id` 有效位宽（10 位 ⇒ 1024 文件）。
pub const FILE_ID_MAX: u16 = 1023;
/// `block_id` 有效位宽（28 位）。
pub const BLOCK_ID_MAX: u32 = (1 << 28) - 1;

/// 当前格式版本（**未冻结，冻结后不可变**；§5.6 的编号说明同样适用）。
pub const FORMAT_VERSION: u8 = 1;

/// 一般页的页尾长度（仅页尾副本）。
pub const TAIL_LEN: usize = 4;
/// 索引叶页的页尾长度（`link_prev` 6B + `link_next` 6B + 页尾副本 4B）。
pub const INDEX_LEAF_TAIL_LEN: usize = 16;

/// 页中 `checksum` 字段偏移（即描述区首字段）。
pub const CHECKSUM_OFFSET: usize = PAGE_CHECKSUM_OFFSET;
/// `checksum` 字段宽度。
pub const CHECKSUM_LEN: usize = PAGE_CHECKSUM_LEN;

// ---------------------------------------------------------------------------
// 页类型与标志
// ---------------------------------------------------------------------------

/// 页类型（§5.6 全表；编号在格式冻结前仍可调整，冻结后不可变）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PageType {
    /// 堆表页（槽位目录 + 行区）。
    HeapTable = 1,
    /// 索引叶页（有序条目数组 + 页尾双链）。
    IndexLeaf = 2,
    /// 索引枝页。
    IndexBranch = 3,
    /// 索引根页（体同枝页；分列类型只为诊断与恢复分派）。
    IndexRoot = 4,
    /// 邻接页（按 `(src, edge_id)` 聚簇的边）。
    Adjacency = 5,
    /// ANN 索引页。
    AnnIndex = 6,
    /// 段头页（undo 段头额外含事务表）。
    SegmentHeader = 7,
    /// 位图页（区分配位图，位于文件头部）。
    Bitmap = 8,
    /// Undo 页。
    Undo = 9,
    /// 临时页（体同堆表页）。
    Temporary = 10,
    /// 文件头页（每文件块 0）。
    FileHeader = 11,
    /// 引导页（格式永久固定）。
    Bootstrap = 12,
    /// REDO 页（自有页头 + 记录流；记录可跨页）。
    Redo = 13,
    /// 控制文件页（自有页头 + 自有 undo）。
    ControlFile = 14,
}

impl PageType {
    /// 全部页类型（§5.6 的表；顺序即编号顺序）。
    pub const ALL: [PageType; 14] = [
        PageType::HeapTable,
        PageType::IndexLeaf,
        PageType::IndexBranch,
        PageType::IndexRoot,
        PageType::Adjacency,
        PageType::AnnIndex,
        PageType::SegmentHeader,
        PageType::Bitmap,
        PageType::Undo,
        PageType::Temporary,
        PageType::FileHeader,
        PageType::Bootstrap,
        PageType::Redo,
        PageType::ControlFile,
    ];

    /// 由编号解码；未知编号返回 `None`。
    #[must_use]
    pub const fn from_u8(raw: u8) -> Option<Self> {
        match raw {
            1 => Some(PageType::HeapTable),
            2 => Some(PageType::IndexLeaf),
            3 => Some(PageType::IndexBranch),
            4 => Some(PageType::IndexRoot),
            5 => Some(PageType::Adjacency),
            6 => Some(PageType::AnnIndex),
            7 => Some(PageType::SegmentHeader),
            8 => Some(PageType::Bitmap),
            9 => Some(PageType::Undo),
            10 => Some(PageType::Temporary),
            11 => Some(PageType::FileHeader),
            12 => Some(PageType::Bootstrap),
            13 => Some(PageType::Redo),
            14 => Some(PageType::ControlFile),
            _ => None,
        }
    }

    /// 编号。
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// 该类型的页尾长度（§5.8：只有索引叶页付双链的 12 字节）。
    #[must_use]
    pub const fn tail_len(self) -> usize {
        match self {
            PageType::IndexLeaf => INDEX_LEAF_TAIL_LEN,
            _ => TAIL_LEN,
        }
    }

    /// 是否使用**槽位目录**式页体（数据页 / Undo 页这一类）。
    #[must_use]
    pub const fn uses_slot_directory(self) -> bool {
        matches!(
            self,
            PageType::HeapTable | PageType::Temporary | PageType::Undo
        )
    }
}

/// `flags` 位（只含崩溃后必须存活的状态；§5.3）。
pub mod flags {
    /// 位 0：页初始化完成（**未完成的页不得使用**）。
    pub const INITIALIZED: u8 = 1 << 0;
    /// 位 1：含跨页行片段。
    pub const HAS_CROSS_PAGE_FRAGMENT: u8 = 1 << 1;
}

// ---------------------------------------------------------------------------
// 页头（值形态）与槽位条目
// ---------------------------------------------------------------------------

/// 页头各字段的值形态（读出/写入用；不含 `checksum`——它由 [`Page::seal`] 维护）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// 格式版本。
    pub format_version: u8,
    /// 页类型。
    pub page_type: PageType,
    /// 标志位（见 [`flags`]）。
    pub flags: u8,
    /// 修改序列号（与页尾副本对比）。
    pub mod_seq: u8,
    /// 工作区受校验标识（`H(workspace_id)` 截断 8 字节；由调用方提供）。
    pub workspace_ref: [u8; WORKSPACE_REF_LEN],
    /// 文件号（有效 10 位）。
    pub file_id: u16,
    /// 块号（有效 28 位）。
    pub block_id: u32,
    /// 最近修改的日志位置（仅恢复期用于"该页是否已含某日志的效果"）。
    pub page_lsn: Lsn,
    /// 当前 ITL 槽数（从 `INITRANS` 起，动态扩展）。
    pub itl_count: u16,
    /// 槽位数（含已删槽）。
    pub slot_count: u16,
    /// 行区上界（`free_start` 可推导，不存）。
    pub free_end: u16,
}

/// 槽位状态（§5.7：2 位）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SlotStatus {
    /// 0 空闲。
    Free = 0,
    /// 1 正常行。
    Normal = 1,
    /// 2 转发指针（行迁移的落点）。
    Forwarding = 2,
    /// 3 片段头（跨页行片段链的头）。
    FragmentHead = 3,
}

/// 槽位目录条目（2 字节：14 位行数据起始偏移 + 2 位状态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotEntry(u16);

impl SlotEntry {
    /// 由偏移（14 位域）与状态构造；偏移越界（> 16383）返回 `None`。
    #[must_use]
    pub const fn new(offset: u16, status: SlotStatus) -> Option<Self> {
        if offset > 0x3FFF {
            return None;
        }
        Some(Self((offset << 2) | status as u16))
    }

    /// 由原始 2 字节解码。
    #[must_use]
    pub const fn from_raw(raw: u16) -> Self {
        Self(raw)
    }

    /// 原始 2 字节。
    #[must_use]
    pub const fn as_raw(self) -> u16 {
        self.0
    }

    /// 行数据起始偏移（14 位）。
    #[must_use]
    pub const fn offset(self) -> u16 {
        self.0 >> 2
    }

    /// 状态（2 位）。
    #[must_use]
    pub const fn status(self) -> SlotStatus {
        match self.0 & 0b11 {
            0 => SlotStatus::Free,
            1 => SlotStatus::Normal,
            2 => SlotStatus::Forwarding,
            _ => SlotStatus::FragmentHead,
        }
    }
}

/// 页校验结果（先页尾（廉价）后校验和）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageCheck {
    /// 通过。
    Ok,
    /// **断裂块**：头尾副本不一致（只写了一半）。
    FracturedBlock,
    /// 数据损坏：头尾一致但 CRC32C 不符（也覆盖"头尾一起错"）。
    ChecksumMismatch,
    /// 页类型字段无法解码（格式不认识）。
    UnknownPageType,
}

// ---------------------------------------------------------------------------
// Page
// ---------------------------------------------------------------------------

/// 一页（16 KiB）的字节载体与访问器。
#[derive(Clone)]
pub struct Page {
    bytes: Box<[u8; PAGE_SIZE]>,
}

impl std::fmt::Debug for Page {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 紧凑形态：不打印 16 KiB 字节，只给诊断需要的摘要。
        match self.header() {
            Some(h) => f
                .debug_struct("Page")
                .field("page_type", &h.page_type)
                .field("file_id", &h.file_id)
                .field("block_id", &h.block_id)
                .field("page_lsn", &h.page_lsn)
                .field("mod_seq", &h.mod_seq)
                .field("slot_count", &h.slot_count)
                .field("free_space", &self.free_space())
                .field("verify", &self.verify())
                .finish(),
            None => f
                .debug_struct("Page")
                .field("page_type", &"<未知>")
                .finish(),
        }
    }
}

impl Page {
    /// 初始化新页（`flags` 置"初始化完成"，`mod_seq = 1`，行区为空）。
    ///
    /// `file_id` / `block_id` 超出有效位宽即拒绝（`debug` 断言；
    /// 发布形态下由调用方保证——两者与 ROWID 位宽耦合）。
    #[must_use]
    pub fn new(
        page_type: PageType,
        workspace_ref: [u8; WORKSPACE_REF_LEN],
        file_id: u16,
        block_id: u32,
    ) -> Self {
        debug_assert!(file_id <= FILE_ID_MAX, "file_id 有效 10 位");
        debug_assert!(block_id <= BLOCK_ID_MAX, "block_id 有效 28 位");
        let mut page = Self {
            bytes: Box::new([0u8; PAGE_SIZE]),
        };
        let header = Header {
            format_version: FORMAT_VERSION,
            page_type,
            flags: flags::INITIALIZED,
            mod_seq: 1,
            workspace_ref,
            file_id,
            block_id,
            page_lsn: Lsn::from_raw(0).expect("0 在 48 位域内"),
            itl_count: 1,
            slot_count: 0,
            free_end: (PAGE_SIZE - page_type.tail_len()) as u16,
        };
        page.write_header(&header);
        page.seal();
        page
    }

    /// 由既有字节（如从磁盘读入）构造。
    #[must_use]
    pub fn from_bytes(bytes: Box<[u8; PAGE_SIZE]>) -> Self {
        Self { bytes }
    }

    /// 原始字节。
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; PAGE_SIZE] {
        &self.bytes
    }

    /// 原始字节（可变；**修改后必须 [`Page::seal`]**）。
    pub fn as_bytes_mut(&mut self) -> &mut [u8; PAGE_SIZE] {
        &mut self.bytes
    }

    /// 读页头（`page_type` 无法解码时返回 `None`）。
    #[must_use]
    pub fn header(&self) -> Option<Header> {
        let b = &self.bytes;
        let page_type = PageType::from_u8(b[PAGE_TYPE_OFFSET])?;
        let mut workspace_ref = [0u8; WORKSPACE_REF_LEN];
        workspace_ref
            .copy_from_slice(&b[WORKSPACE_REF_OFFSET..WORKSPACE_REF_OFFSET + WORKSPACE_REF_LEN]);
        let mut lsn_bytes = [0u8; 8];
        lsn_bytes[..PAGE_LSN_LEN]
            .copy_from_slice(&b[PAGE_LSN_OFFSET..PAGE_LSN_OFFSET + PAGE_LSN_LEN]);
        Some(Header {
            format_version: b[FORMAT_VERSION_OFFSET],
            page_type,
            flags: b[FLAGS_OFFSET],
            mod_seq: b[MOD_SEQ_OFFSET],
            workspace_ref,
            file_id: u16::from_le_bytes(
                b[FILE_ID_OFFSET..FILE_ID_OFFSET + 2]
                    .try_into()
                    .expect("2 字节"),
            ),
            block_id: u32::from_le_bytes(
                b[BLOCK_ID_OFFSET..BLOCK_ID_OFFSET + 4]
                    .try_into()
                    .expect("4 字节"),
            ),
            page_lsn: Lsn::from_raw(u64::from_le_bytes(lsn_bytes)).expect("48 位域内"),
            itl_count: u16::from_le_bytes(
                b[ITL_COUNT_OFFSET..ITL_COUNT_OFFSET + 2]
                    .try_into()
                    .expect("2 字节"),
            ),
            slot_count: u16::from_le_bytes(
                b[SLOT_COUNT_OFFSET..SLOT_COUNT_OFFSET + 2]
                    .try_into()
                    .expect("2 字节"),
            ),
            free_end: u16::from_le_bytes(
                b[FREE_END_OFFSET..FREE_END_OFFSET + 2]
                    .try_into()
                    .expect("2 字节"),
            ),
        })
    }

    /// 写页头（不含 `checksum`；`page_type` 与页尾副本由 [`Page::seal`] 落定）。
    pub fn write_header(&mut self, header: &Header) {
        let b = &mut self.bytes;
        b[FORMAT_VERSION_OFFSET] = header.format_version;
        b[PAGE_TYPE_OFFSET] = header.page_type.as_u8();
        b[FLAGS_OFFSET] = header.flags;
        b[MOD_SEQ_OFFSET] = header.mod_seq;
        b[WORKSPACE_REF_OFFSET..WORKSPACE_REF_OFFSET + WORKSPACE_REF_LEN]
            .copy_from_slice(&header.workspace_ref);
        b[FILE_ID_OFFSET..FILE_ID_OFFSET + 2].copy_from_slice(&header.file_id.to_le_bytes());
        b[BLOCK_ID_OFFSET..BLOCK_ID_OFFSET + 4].copy_from_slice(&header.block_id.to_le_bytes());
        b[PAGE_LSN_OFFSET..PAGE_LSN_OFFSET + PAGE_LSN_LEN]
            .copy_from_slice(&header.page_lsn.as_raw().to_le_bytes()[..PAGE_LSN_LEN]);
        b[ITL_COUNT_OFFSET..ITL_COUNT_OFFSET + 2].copy_from_slice(&header.itl_count.to_le_bytes());
        b[SLOT_COUNT_OFFSET..SLOT_COUNT_OFFSET + 2]
            .copy_from_slice(&header.slot_count.to_le_bytes());
        b[FREE_END_OFFSET..FREE_END_OFFSET + 2].copy_from_slice(&header.free_end.to_le_bytes());
    }

    /// 推进 `mod_seq`（1 字节回绕无碍：只用于头尾对比）。
    pub fn bump_mod_seq(&mut self) {
        self.bytes[MOD_SEQ_OFFSET] = self.bytes[MOD_SEQ_OFFSET].wrapping_add(1);
    }

    /// 固定头末尾（`itl_count == 1` 为 68；每多一槽 +24）。
    #[must_use]
    pub fn fixed_header_end(&self) -> usize {
        let itl = u16::from_le_bytes(
            self.bytes[ITL_COUNT_OFFSET..ITL_COUNT_OFFSET + 2]
                .try_into()
                .expect("2 字节"),
        );
        FIXED_HEADER_LEN + (itl.max(1) as usize - 1) * ITL_ENTRY_LEN
    }

    /// `free_start`（**可推导、不存**）：固定头末尾 + 槽位目录长度。
    #[must_use]
    pub fn free_start(&self) -> usize {
        self.fixed_header_end() + self.slot_count() as usize * SLOT_ENTRY_LEN
    }

    /// `slot_count`。
    #[must_use]
    pub fn slot_count(&self) -> u16 {
        u16::from_le_bytes(
            self.bytes[SLOT_COUNT_OFFSET..SLOT_COUNT_OFFSET + 2]
                .try_into()
                .expect("2 字节"),
        )
    }

    /// 设置 `slot_count`（越上限拒绝；须与页头其余字段一起 `seal`）。
    pub fn set_slot_count(&mut self, count: u16) -> Result<(), &'static str> {
        if count as usize > MAX_SLOTS {
            return Err("槽位上限 1023（行号 1..=1023，0 保留为「无」）");
        }
        self.bytes[SLOT_COUNT_OFFSET..SLOT_COUNT_OFFSET + 2].copy_from_slice(&count.to_le_bytes());
        Ok(())
    }

    /// `free_end`（行区上界）。
    #[must_use]
    pub fn free_end(&self) -> usize {
        u16::from_le_bytes(
            self.bytes[FREE_END_OFFSET..FREE_END_OFFSET + 2]
                .try_into()
                .expect("2 字节"),
        ) as usize
    }

    /// 设置 `free_end`。
    pub fn set_free_end(&mut self, end: usize) {
        debug_assert!(end <= PAGE_SIZE, "free_end 必须落在页内");
        self.bytes[FREE_END_OFFSET..FREE_END_OFFSET + 2]
            .copy_from_slice(&(end as u16).to_le_bytes());
    }

    /// 可用空间 = `free_end − free_start`（**不存 `free_size`**：两个指针之差）。
    #[must_use]
    pub fn free_space(&self) -> usize {
        self.free_end().saturating_sub(self.free_start())
    }

    /// 行区下界（行数据从页底向上生长；此值为行区**最低**可寻址位置）。
    #[must_use]
    pub fn row_area_floor(&self) -> usize {
        PAGE_SIZE - self.header().map_or(TAIL_LEN, |h| h.page_type.tail_len())
    }

    /// 读槽位条目（`index` 为目录下标，0 起；行号 = 下标 + 1）。
    #[must_use]
    pub fn slot(&self, index: usize) -> Option<SlotEntry> {
        if index >= self.slot_count() as usize {
            return None;
        }
        let at = self.fixed_header_end() + index * SLOT_ENTRY_LEN;
        let raw = u16::from_le_bytes(
            self.bytes[at..at + SLOT_ENTRY_LEN]
                .try_into()
                .expect("2 字节"),
        );
        Some(SlotEntry::from_raw(raw))
    }

    /// 写槽位条目（不改变 `slot_count`；越界拒绝）。
    pub fn set_slot(&mut self, index: usize, entry: SlotEntry) -> bool {
        if index >= self.slot_count() as usize {
            return false;
        }
        let at = self.fixed_header_end() + index * SLOT_ENTRY_LEN;
        self.bytes[at..at + SLOT_ENTRY_LEN].copy_from_slice(&entry.as_raw().to_le_bytes());
        true
    }

    /// 写页尾副本：`page_type` 1B + `mod_seq` 1B + `page_lsn` 低位 2B（最后 4 字节）。
    pub fn update_tail(&mut self) {
        let n = PAGE_SIZE;
        let lsn_low = [self.bytes[PAGE_LSN_OFFSET], self.bytes[PAGE_LSN_OFFSET + 1]];
        self.bytes[n - 4] = self.bytes[PAGE_TYPE_OFFSET];
        self.bytes[n - 3] = self.bytes[MOD_SEQ_OFFSET];
        self.bytes[n - 2] = lsn_low[0];
        self.bytes[n - 1] = lsn_low[1];
    }

    /// 重算并写回 `checksum`（CRC32C 覆盖全页，本字段置零后计算）。
    pub fn update_checksum(&mut self) {
        let sum = page_checksum(&self.bytes[..]);
        self.bytes[CHECKSUM_OFFSET..CHECKSUM_OFFSET + CHECKSUM_LEN]
            .copy_from_slice(&sum.to_le_bytes());
    }

    /// **一次修改的收尾**：写页尾副本 → 重算校验和。任何字段修改后必须调用。
    pub fn seal(&mut self) {
        self.update_tail();
        self.update_checksum();
    }

    /// 校验：先比页尾（廉价），再算校验和（完整）。
    #[must_use]
    pub fn verify(&self) -> PageCheck {
        if self.header().is_none() {
            return PageCheck::UnknownPageType;
        }
        let n = PAGE_SIZE;
        if self.bytes[n - 4] != self.bytes[PAGE_TYPE_OFFSET]
            || self.bytes[n - 3] != self.bytes[MOD_SEQ_OFFSET]
            || self.bytes[n - 2] != self.bytes[PAGE_LSN_OFFSET]
            || self.bytes[n - 1] != self.bytes[PAGE_LSN_OFFSET + 1]
        {
            return PageCheck::FracturedBlock;
        }
        if !verify_page_checksum(&self.bytes[..]) {
            return PageCheck::ChecksumMismatch;
        }
        PageCheck::Ok
    }

    /// 页是否已初始化完成（未完成的页不得使用）。
    #[must_use]
    pub fn is_initialized(&self) -> bool {
        self.bytes[FLAGS_OFFSET] & flags::INITIALIZED != 0
    }

    /// 页是否含跨页行片段。
    #[must_use]
    pub fn has_cross_page_fragment(&self) -> bool {
        self.bytes[FLAGS_OFFSET] & flags::HAS_CROSS_PAGE_FRAGMENT != 0
    }

    /// 读取校验和字段现值。
    #[must_use]
    pub fn stored_checksum(&self) -> u32 {
        u32::from_le_bytes(
            self.bytes[CHECKSUM_OFFSET..CHECKSUM_OFFSET + CHECKSUM_LEN]
                .try_into()
                .expect("4 字节"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ref_of(n: u8) -> [u8; WORKSPACE_REF_LEN] {
        [n; WORKSPACE_REF_LEN]
    }

    #[test]
    fn new_page_has_consistent_header_tail_and_checksum() {
        let page = Page::new(PageType::HeapTable, ref_of(0xAB), 3, 7);
        let h = page.header().expect("页类型可解码");
        assert_eq!(h.format_version, FORMAT_VERSION);
        assert_eq!(h.page_type, PageType::HeapTable);
        assert_eq!(h.flags, flags::INITIALIZED);
        assert!(page.is_initialized());
        assert!(!page.has_cross_page_fragment());
        assert_eq!(h.mod_seq, 1);
        assert_eq!(h.workspace_ref, ref_of(0xAB));
        assert_eq!(h.file_id, 3);
        assert_eq!(h.block_id, 7);
        assert_eq!(h.page_lsn.as_raw(), 0);
        assert_eq!(h.itl_count, 1);
        assert_eq!(h.slot_count, 0);
        assert_eq!(page.fixed_header_end(), FIXED_HEADER_LEN);
        assert_eq!(page.free_start(), FIXED_HEADER_LEN);
        assert_eq!(page.free_end(), PAGE_SIZE - TAIL_LEN);
        assert_eq!(page.free_space(), PAGE_SIZE - TAIL_LEN - FIXED_HEADER_LEN);
        assert_ne!(page.stored_checksum(), 0, "校验和已写入");
        assert_eq!(page.verify(), PageCheck::Ok);
    }

    #[test]
    fn fractured_block_is_detected_before_checksum() {
        let mut page = Page::new(PageType::HeapTable, ref_of(1), 0, 0);
        // 只改页头类型（不动页尾、也不重算校验和）。
        page.as_bytes_mut()[PAGE_TYPE_OFFSET] = PageType::Undo.as_u8();
        assert_eq!(page.verify(), PageCheck::FracturedBlock, "头新尾旧：断裂块");

        // 只改页尾。
        let mut page = Page::new(PageType::HeapTable, ref_of(1), 0, 0);
        let n = PAGE_SIZE;
        page.as_bytes_mut()[n - 3] ^= 0x5A;
        assert_eq!(page.verify(), PageCheck::FracturedBlock);

        // mod_seq 推进后必须密封才会一致。
        let mut page = Page::new(PageType::HeapTable, ref_of(1), 0, 0);
        page.bump_mod_seq();
        assert_eq!(page.verify(), PageCheck::FracturedBlock);
        page.seal();
        assert_eq!(page.verify(), PageCheck::Ok);
        assert_eq!(page.header().unwrap().mod_seq, 2);
    }

    #[test]
    fn header_says_one_thing_tail_says_another_for_lsn() {
        let mut page = Page::new(PageType::HeapTable, ref_of(1), 0, 0);
        // 把 page_lsn 设到低位第三字节之前，尾部副本只覆盖低 2 字节——
        // 低 16 位之外的变化不触发断裂块（由校验和兜底）。
        let lsn = Lsn::from_raw(0x00_0000_0001_0000).unwrap();
        let mut h = page.header().unwrap();
        h.page_lsn = lsn;
        page.write_header(&h);
        page.seal();
        assert_eq!(page.verify(), PageCheck::Ok);
        assert_eq!(page.header().unwrap().page_lsn, lsn);

        // 只改低 16 位之内、但不密封 → 断裂块（尾部副本是旧的）。
        let mut h = page.header().unwrap();
        h.page_lsn = Lsn::from_raw(0x00_0000_0001_0001).unwrap();
        page.write_header(&h);
        assert_eq!(page.verify(), PageCheck::FracturedBlock);
    }

    #[test]
    fn checksum_catches_damage_that_tail_cannot() {
        let mut page = Page::new(PageType::HeapTable, ref_of(1), 0, 0);
        // 改一个"头尾都不覆盖"的字节（保留区 28..32）。
        page.as_bytes_mut()[28] ^= 0xFF;
        assert_eq!(page.verify(), PageCheck::ChecksumMismatch, "不是断裂块");

        // 任意行区字节的翻转同样被校验和抓住。
        let mut page = Page::new(PageType::HeapTable, ref_of(1), 0, 0);
        page.as_bytes_mut()[9000] ^= 0x01;
        assert_eq!(page.verify(), PageCheck::ChecksumMismatch);
    }

    #[test]
    fn unknown_page_type_is_reported() {
        let mut page = Page::new(PageType::HeapTable, ref_of(1), 0, 0);
        page.as_bytes_mut()[PAGE_TYPE_OFFSET] = 99;
        page.update_tail(); // 头尾一致，避免先报断裂块
        page.update_checksum();
        assert_eq!(page.verify(), PageCheck::UnknownPageType);
    }

    #[test]
    fn slot_directory_roundtrip_and_limits() {
        let mut page = Page::new(PageType::HeapTable, ref_of(2), 1, 1);
        assert!(
            !page.set_slot(0, SlotEntry::new(0, SlotStatus::Normal).unwrap()),
            "未设 slot_count 时越界"
        );

        page.set_slot_count(3).unwrap();
        let entries = [
            SlotEntry::new(16000, SlotStatus::Normal).unwrap(),
            SlotEntry::new(15900, SlotStatus::Forwarding).unwrap(),
            SlotEntry::new(15800, SlotStatus::FragmentHead).unwrap(),
        ];
        for (i, e) in entries.iter().enumerate() {
            assert!(page.set_slot(i, *e));
        }
        for (i, e) in entries.iter().enumerate() {
            let got = page.slot(i).unwrap();
            assert_eq!(got, *e);
            assert_eq!(got.offset(), e.offset());
            assert_eq!(got.status(), e.status());
        }
        assert!(page.slot(3).is_none(), "越界槽位");
        // free_start 随 slot_count 推导。
        assert_eq!(page.free_start(), FIXED_HEADER_LEN + 3 * SLOT_ENTRY_LEN);
        // 空闲 = 行区上界 − 目录末尾。
        assert_eq!(
            page.free_space(),
            (PAGE_SIZE - TAIL_LEN) - page.free_start()
        );
        // 上限。
        assert!(page.set_slot_count(MAX_SLOTS as u16).is_ok());
        assert!(page.set_slot_count(MAX_SLOTS as u16 + 1).is_err());
        // 偏移是 14 位域。
        assert!(SlotEntry::new(0x3FFF, SlotStatus::Free).is_some());
        assert!(SlotEntry::new(0x4000, SlotStatus::Free).is_none());
    }

    #[test]
    fn itl_growth_shifts_fixed_header() {
        let mut page = Page::new(PageType::Undo, ref_of(3), 1, 1);
        let mut h = page.header().unwrap();
        h.itl_count = 3;
        page.write_header(&h);
        assert_eq!(
            page.fixed_header_end(),
            FIXED_HEADER_LEN + 2 * ITL_ENTRY_LEN
        );
        assert_eq!(page.free_start(), FIXED_HEADER_LEN + 2 * ITL_ENTRY_LEN);
        page.seal();
        assert_eq!(page.verify(), PageCheck::Ok);
    }

    #[test]
    fn page_type_tail_lengths_and_slot_directory_usage() {
        assert_eq!(PageType::IndexLeaf.tail_len(), INDEX_LEAF_TAIL_LEN);
        assert_eq!(PageType::IndexBranch.tail_len(), TAIL_LEN);
        assert!(PageType::HeapTable.uses_slot_directory());
        assert!(PageType::Undo.uses_slot_directory());
        assert!(!PageType::IndexLeaf.uses_slot_directory());
        for (i, t) in PageType::ALL.iter().enumerate() {
            assert_eq!(t.as_u8() as usize, i + 1);
            assert_eq!(PageType::from_u8(t.as_u8()), Some(*t));
        }
        assert_eq!(PageType::from_u8(0), None);
        assert_eq!(PageType::from_u8(15), None);

        // 索引叶页的行区下界更低 12 字节。
        let leaf = Page::new(PageType::IndexLeaf, ref_of(1), 0, 0);
        assert_eq!(leaf.row_area_floor(), PAGE_SIZE - INDEX_LEAF_TAIL_LEN);
        assert_eq!(leaf.free_end(), PAGE_SIZE - INDEX_LEAF_TAIL_LEN);
        assert_eq!(leaf.verify(), PageCheck::Ok, "16 字节页尾下校验依然通过");
    }

    #[test]
    #[should_panic(expected = "file_id 有效 10 位")]
    fn file_id_beyond_ten_bits_panics() {
        let _ = Page::new(PageType::HeapTable, ref_of(1), FILE_ID_MAX + 1, 0);
    }
}
