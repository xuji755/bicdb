//! **引导页（页类型 12）与 file 0 的副本带**（`目录详设` §2.1/§2.2/§2.3）。
//!
//! ```text
//! file 0 排布（带式）：
//!   块 0    文件头页（类型 11，主）           ┐ 核心元数据带 [0, 4 MiB)
//!   块 1    引导页（类型 12，主）             ┘（其余块预留、不写入）
//!   块 256  引导页**副本**                    ┐ 副本带 [4 MiB, 8 MiB)
//!   块 257  文件头**副本**                    ┘（与主相隔 4 MiB——同一片损坏不可能同时命中）
//!   块 512+ 位图区；块 832+ 数据区（字典段）
//!
//! 引导页页体（**格式永久固定**）：
//!   偏移 68  页体头 8B：entry_count 2B │ format_version 1B │ 保留 5B
//!   偏移 76  条目区：entry[N] × 24B
//!              dataobj# 4B │ seg_type 1B │ 保留 1B │ seg_header 6B │ iniexts 4B │ 保留 8B
//!   （未用条目槽**写零**——页内容确定，便于校验与比对）
//! ```
//!
//! **三层读取协议**（自愈 → 重建）：主 → 副本带（**就地自愈**）→ 扫段头页**重建**；
//! 引导页**不产生 redo**（它本身是恢复锚点），耐久性靠创建路径的显式 fsync。

use crate::bitmap::FileLayout;
use crate::datafile::{DataFile, DataFileError};
use crate::page::{Page, PageType, PAGE_SIZE};
use crate::rowid::RowId;
use crate::segment::{self, SegmentError};

/// 引导页**主**的位置（file 0 块 1）。
pub const BOOTSTRAP_MAIN_BLOCK: u32 = 1;
/// 引导页**副本**的位置（副本带首块；与主相隔 4 MiB）。
pub const BOOTSTRAP_REPLICA_BLOCK: u32 = crate::bitmap::META_REPLICA_BAND_FIRST_BLOCK;
/// **文件头副本**的位置（副本带第 2 块）。
pub const FILE_HEADER_REPLICA_BLOCK: u32 = BOOTSTRAP_REPLICA_BLOCK + 1;
/// 引导页体内布局的版本（**自举条目布局**；不认识即拒绝）。
pub const BOOTSTRAP_FORMAT_VERSION: u8 = 1;

/// 页体头偏移（页头 68B 之后）。
const BODY_OFFSET: usize = 68;
/// 页体头长度。
const BODY_LEN: usize = 8;
/// 条目长度（24B；尾 8B 保留供将来扩展，**不改格式即可加字段**）。
const ENTRY_LEN: usize = 24;
/// 页尾校验副本长度（正文不得压上去）。
const TAIL_LEN: usize = 4;
/// 条目数上限（`MAX_BOOTSTRAP_ENTRIES = (16384 − 68 − 8 − 4) / 24 = 679`）。
pub const MAX_BOOTSTRAP_ENTRIES: usize =
    (PAGE_SIZE - BODY_OFFSET - BODY_LEN - TAIL_LEN) / ENTRY_LEN;

/// 一个自举条目（`arch/03` §3.1.1 的四要素）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootstrapEntry {
    /// 数据对象号。
    pub dataobj: u32,
    /// 段类型（`SegType` 的编号）——读引导页时字典表还不可用，故必须随条目存。
    pub seg_type: u8,
    /// 段头页 ROWID（6B）。
    pub seg_header: RowId,
    /// 初始区数。
    pub iniexts: u32,
}

/// 引导页的读取来源（诊断/审计用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapSource {
    /// 主副本有效（正常路径）。
    Main,
    /// 主副本损坏，**从副本带自愈**（已把主位置重写回去）。
    ReplicaHealed,
}

/// 引导页错误（**明确判定**，不静默）。
#[derive(Debug)]
pub enum BootstrapError {
    /// 不是元数据文件（role ≠ 0）——引导页只在 file 0 上。
    NotMetaFile,
    /// 页类型不是引导页。
    NotABootstrapPage,
    /// 页内容非法（条目数越界/版本不认识/条目重叠）。
    Malformed(String),
    /// 条目数超过单页容量。
    TooManyEntries {
        /// 实际条目数。
        count: usize,
        /// 上限。
        max: usize,
    },
    /// **主副皆坏**——必须走重建（扫段头页）。
    BothCopiesBad,
    /// 数据文件层错误。
    DataFile(DataFileError),
    /// 段层错误（重建路径读段头）。
    Segment(SegmentError),
}

impl std::fmt::Display for BootstrapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BootstrapError::NotMetaFile => f.write_str("引导页只在 file 0（role = 0）上"),
            BootstrapError::NotABootstrapPage => f.write_str("不是引导页"),
            BootstrapError::Malformed(why) => write!(f, "引导页内容非法：{why}"),
            BootstrapError::TooManyEntries { count, max } => {
                write!(f, "自举条目过多：{count} > {max}")
            }
            BootstrapError::BothCopiesBad => {
                f.write_str("引导页主副本与副本带都无效——需按段头页重建")
            }
            BootstrapError::DataFile(e) => write!(f, "引导页数据文件层：{e}"),
            BootstrapError::Segment(e) => write!(f, "引导页重建读段头：{e}"),
        }
    }
}

impl std::error::Error for BootstrapError {}

impl From<DataFileError> for BootstrapError {
    fn from(e: DataFileError) -> Self {
        Self::DataFile(e)
    }
}

impl From<SegmentError> for BootstrapError {
    fn from(e: SegmentError) -> Self {
        Self::Segment(e)
    }
}

/// **编码引导页**（页类型必须已是 `Bootstrap`；未用条目槽写零）。
pub fn encode(page: &mut Page, entries: &[BootstrapEntry]) -> Result<(), BootstrapError> {
    if page.header().map(|h| h.page_type) != Some(PageType::Bootstrap) {
        return Err(BootstrapError::NotABootstrapPage);
    }
    if entries.len() > MAX_BOOTSTRAP_ENTRIES {
        return Err(BootstrapError::TooManyEntries {
            count: entries.len(),
            max: MAX_BOOTSTRAP_ENTRIES,
        });
    }
    let bytes = page.as_bytes_mut();
    let body = &mut bytes[BODY_OFFSET..BODY_OFFSET + BODY_LEN];
    body[..2].copy_from_slice(&(entries.len() as u16).to_le_bytes());
    body[2] = BOOTSTRAP_FORMAT_VERSION;
    body[3..].fill(0);
    // 条目区整体清零（确定性 + 不泄漏旧字节），再写实际条目。
    let area = BODY_OFFSET + BODY_LEN;
    bytes[area..area + MAX_BOOTSTRAP_ENTRIES * ENTRY_LEN].fill(0);
    for (i, e) in entries.iter().enumerate() {
        let at = area + i * ENTRY_LEN;
        bytes[at..at + 4].copy_from_slice(&e.dataobj.to_le_bytes());
        bytes[at + 4] = e.seg_type;
        // at + 5 保留 1B（零）
        bytes[at + 6..at + 12].copy_from_slice(&e.seg_header.to_bytes());
        bytes[at + 12..at + 16].copy_from_slice(&e.iniexts.to_le_bytes());
        // at + 16..24 保留 8B（零）
    }
    Ok(())
}

/// **解码引导页**（严格：类型/版本/条目数越界即拒绝）。
pub fn decode(page: &Page) -> Result<Vec<BootstrapEntry>, BootstrapError> {
    if page.header().map(|h| h.page_type) != Some(PageType::Bootstrap) {
        return Err(BootstrapError::NotABootstrapPage);
    }
    let bytes = page.as_bytes();
    let count = u16::from_le_bytes([bytes[BODY_OFFSET], bytes[BODY_OFFSET + 1]]) as usize;
    let version = bytes[BODY_OFFSET + 2];
    if version != BOOTSTRAP_FORMAT_VERSION {
        return Err(BootstrapError::Malformed(format!(
            "自举条目布局版本 {version} 不认识（当前 {BOOTSTRAP_FORMAT_VERSION}）——拒绝打开"
        )));
    }
    if count > MAX_BOOTSTRAP_ENTRIES {
        return Err(BootstrapError::Malformed(format!(
            "条目数 {count} 超过单页容量 {MAX_BOOTSTRAP_ENTRIES}"
        )));
    }
    let area = BODY_OFFSET + BODY_LEN;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let at = area + i * ENTRY_LEN;
        let dataobj = u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 字节"));
        let seg_type = bytes[at + 4];
        let mut raw = [0u8; 6];
        raw.copy_from_slice(&bytes[at + 6..at + 12]);
        let seg_header = RowId::from_bytes(&raw);
        let iniexts = u32::from_le_bytes(bytes[at + 12..at + 16].try_into().expect("4 字节"));
        out.push(BootstrapEntry {
            dataobj,
            seg_type,
            seg_header,
            iniexts,
        });
    }
    Ok(out)
}

/// **写引导页三件套**（创建时一次）：主（块 1）+ 副本（块 256）+ **文件头副本**（块 257）。
///
/// 只在 file 0（role = 0）上；调用方负责随后的 **fsync**（创建路径的显式持久化）。
pub fn write_all(
    file: &mut DataFile<'_>,
    entries: &[BootstrapEntry],
) -> Result<(), BootstrapError> {
    if file.layout() != FileLayout::meta() {
        return Err(BootstrapError::NotMetaFile);
    }
    if entries.len() > MAX_BOOTSTRAP_ENTRIES {
        return Err(BootstrapError::TooManyEntries {
            count: entries.len(),
            max: MAX_BOOTSTRAP_ENTRIES,
        });
    }
    let ws = file.workspace_ref();
    let file_id = file.file_id();
    for block in [BOOTSTRAP_MAIN_BLOCK, BOOTSTRAP_REPLICA_BLOCK] {
        let mut page = Page::new(PageType::Bootstrap, ws, file_id, block);
        encode(&mut page, entries)?;
        page.seal();
        file.write_page(block, &mut page)?;
    }
    // **文件头副本**：与主（块 0）同内容——头被毁同样致命（file_id/role/
    // 当前大小/位图空间头都在里面）。
    let header = file.read_page(0)?;
    let mut copy = Page::from_bytes(Box::new(*header.as_bytes()));
    // 页头自证字段（block_id）随副本位置改写；其余逐字节一致。
    let mut h = copy.header().expect("头页有页头");
    h.block_id = FILE_HEADER_REPLICA_BLOCK;
    copy.write_header(&h);
    copy.seal();
    file.write_page(FILE_HEADER_REPLICA_BLOCK, &mut copy)?;
    Ok(())
}

/// **读引导页（主 → 副本带自愈）**——设计 §2.3 的第 ①② 步。
///
/// 主位置有效 ⇒ `(entries, Main)`；主位置坏而副本有效 ⇒ **就地写回主位置**
/// 并返回 `(entries, ReplicaHealed)`；两处都坏 ⇒ [`BootstrapError::BothCopiesBad`]
/// （调用方走 [`rebuild_from_segment_headers`]）。
pub fn read_with_heal(
    file: &mut DataFile<'_>,
) -> Result<(Vec<BootstrapEntry>, BootstrapSource), BootstrapError> {
    if file.layout() != FileLayout::meta() {
        return Err(BootstrapError::NotMetaFile);
    }
    if let Ok(page) = file.read_page(BOOTSTRAP_MAIN_BLOCK) {
        if let Ok(entries) = decode(&page) {
            return Ok((entries, BootstrapSource::Main));
        }
    }
    if let Ok(page) = file.read_page(BOOTSTRAP_REPLICA_BLOCK) {
        if let Ok(entries) = decode(&page) {
            // **自愈**：副本有效 ⇒ 就地把主位置重写回去（幂等；下次即走正常路径）。
            let mut heal = Page::from_bytes(Box::new(*page.as_bytes()));
            let mut h = heal.header().expect("引导页有页头");
            h.block_id = BOOTSTRAP_MAIN_BLOCK;
            heal.write_header(&h);
            heal.seal();
            file.write_page(BOOTSTRAP_MAIN_BLOCK, &mut heal)?;
            return Ok((entries, BootstrapSource::ReplicaHealed));
        }
    }
    Err(BootstrapError::BothCopiesBad)
}

/// **重建引导页**（设计 §2.3 第 ③ 步；E2 的兜底）：扫数据区的**段头页**
/// （类型 7），读出四个自举要素（`dataobj#` / `seg_type` / `seg_header`（该页自身块号）/
/// `iniexts`（段头的 `extent_count`）），按 `dataobj#` 升序返回。
///
/// **只读**：不写盘（调用方决定何时把结果写回主位置与副本带）。
/// 未分配/未写过的块（零页）与损坏页**跳过**——重建是"尽力而为"的最后手段。
pub fn rebuild_from_segment_headers(
    file: &DataFile<'_>,
) -> Result<Vec<BootstrapEntry>, BootstrapError> {
    if file.layout() != FileLayout::meta() {
        return Err(BootstrapError::NotMetaFile);
    }
    let layout = file.layout();
    let mut out = Vec::new();
    for block in layout.data_area_first_block..file.blocks() as u32 {
        let Ok(page) = file.read_page_unverified(block) else {
            continue;
        };
        if page.header().map(|h| h.page_type) != Some(PageType::SegmentHeader) {
            continue;
        }
        let Ok(head) = segment::read_header(&page) else {
            continue;
        };
        let Ok(seg_header) = RowId::from_parts(file.file_id(), block, 1) else {
            continue;
        };
        out.push(BootstrapEntry {
            dataobj: head.dataobj,
            seg_type: head.seg_type as u8,
            seg_header,
            iniexts: u32::from(head.extent_count),
        });
    }
    out.sort_by_key(|e| e.dataobj);
    out.dedup_by_key(|e| e.dataobj);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::SegType;
    use bicdb_workspace::io::MemFileIo;
    use std::path::Path;

    const WS: [u8; 8] = [7u8; 8];

    fn meta_file<'a>(io: &'a MemFileIo, path: &str) -> DataFile<'a> {
        let layout = FileLayout::meta();
        DataFile::create(
            io,
            Path::new(path),
            0,
            crate::bitmap::META_ROLE,
            WS,
            layout.min_file_blocks() + 64,
        )
        .expect("建 file 0")
    }

    fn entries(n: usize) -> Vec<BootstrapEntry> {
        (0..n)
            .map(|i| BootstrapEntry {
                dataobj: 100 + i as u32,
                seg_type: 1 + (i % 3) as u8,
                seg_header: RowId::from_parts(0, 832 + (i as u32) * 8, 1).unwrap(),
                iniexts: i as u32,
            })
            .collect()
    }

    #[test]
    fn encode_decode_round_trip_and_zero_fill() {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let file = meta_file(&io, "/mem/b1.dat");
        let mut page = Page::new(PageType::Bootstrap, WS, 0, BOOTSTRAP_MAIN_BLOCK);
        let es = entries(15);
        encode(&mut page, &es).unwrap();
        // 未用条目槽为**零**（确定性）。
        let bytes = page.as_bytes();
        let area = BODY_OFFSET + BODY_LEN;
        let unused = &bytes[area + 15 * ENTRY_LEN..area + MAX_BOOTSTRAP_ENTRIES * ENTRY_LEN];
        assert!(unused.iter().all(|&b| b == 0), "未用槽写零");
        assert_eq!(decode(&page).unwrap(), es, "往返一致");
        assert_eq!(decode(&page).unwrap().len(), 15);
        drop(file);

        // 上限：679 条可编码；680 条拒绝。
        let mut page2 = Page::new(PageType::Bootstrap, WS, 0, BOOTSTRAP_MAIN_BLOCK);
        assert_eq!(MAX_BOOTSTRAP_ENTRIES, 679);
        let full = entries(MAX_BOOTSTRAP_ENTRIES);
        encode(&mut page2, &full).unwrap();
        assert_eq!(decode(&page2).unwrap(), full);
        let err = encode(&mut page2, &entries(MAX_BOOTSTRAP_ENTRIES + 1)).unwrap_err();
        assert!(
            matches!(err, BootstrapError::TooManyEntries { .. }),
            "{err}"
        );
    }

    #[test]
    fn decode_rejects_wrong_type_and_version() {
        let page = Page::new(PageType::HeapTable, WS, 0, 1);
        assert!(matches!(
            decode(&page),
            Err(BootstrapError::NotABootstrapPage)
        ));
        let mut page2 = Page::new(PageType::Bootstrap, WS, 0, BOOTSTRAP_MAIN_BLOCK);
        encode(&mut page2, &entries(2)).unwrap();
        page2.as_bytes_mut()[BODY_OFFSET + 2] = 99; // 版本不认识
        let err = decode(&page2).unwrap_err();
        assert!(matches!(err, BootstrapError::Malformed(_)), "{err}");
        assert!(err.to_string().contains("版本"), "文案指向版本：{err}");
    }

    #[test]
    fn write_all_then_read_main_then_heal_from_replica() {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let mut file = meta_file(&io, "/mem/b2.dat");
        let es = entries(15);
        write_all(&mut file, &es).unwrap();

        // ① 主有效 ⇒ Main。
        let (got, src) = read_with_heal(&mut file).unwrap();
        assert_eq!(got, es);
        assert_eq!(src, BootstrapSource::Main);

        // 文件头副本（块 257）与主（块 0）：**语义字段全等**（file_id/role/
        // format_version/blocks/workspace_ref/file_scn），页体逐字节一致；
        // 只有"自身块号"与页校验和随位置不同（那是副本的本分）。
        let main = file.read_page(0).unwrap();
        let copy = file.read_page(FILE_HEADER_REPLICA_BLOCK).unwrap();
        assert_eq!(
            crate::datafile::read_file_head(&main).unwrap(),
            crate::datafile::read_file_head(&copy).unwrap(),
            "文件头语义字段全等（含 file_scn）"
        );
        assert_eq!(
            main.as_bytes()[68..],
            copy.as_bytes()[68..],
            "页体逐字节一致（file_scn/位图空间头都在内）"
        );
        assert_eq!(copy.header().unwrap().block_id, FILE_HEADER_REPLICA_BLOCK);

        // ② 主损坏（写一张别类型的页）⇒ 从副本自愈，并把主位置修回。
        let mut junk = Page::new(PageType::HeapTable, WS, 0, BOOTSTRAP_MAIN_BLOCK);
        junk.seal();
        file.write_page(BOOTSTRAP_MAIN_BLOCK, &mut junk).unwrap();
        let (got2, src2) = read_with_heal(&mut file).unwrap();
        assert_eq!(got2, es, "副本内容一致");
        assert_eq!(src2, BootstrapSource::ReplicaHealed);
        let (_, src3) = read_with_heal(&mut file).unwrap();
        assert_eq!(src3, BootstrapSource::Main, "自愈后主位置已修回");
    }

    #[test]
    fn both_copies_bad_is_named_error() {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let mut file = meta_file(&io, "/mem/b3.dat");
        write_all(&mut file, &entries(3)).unwrap();
        for block in [BOOTSTRAP_MAIN_BLOCK, BOOTSTRAP_REPLICA_BLOCK] {
            let mut junk = Page::new(PageType::HeapTable, WS, 0, block);
            junk.seal();
            file.write_page(block, &mut junk).unwrap();
        }
        let err = read_with_heal(&mut file).unwrap_err();
        assert!(matches!(err, BootstrapError::BothCopiesBad), "{err}");
        assert!(err.to_string().contains("重建"), "文案指向重建路径：{err}");
    }

    #[test]
    fn rebuild_reads_segment_headers_from_the_data_area() {
        // 真段（段头页在数据区）⇒ 重建能读出四要素（dataobj/seg_type/上界区数）。
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let mut file = meta_file(&io, "/mem/b4.dat");
        let _s1 = crate::segment::Segment::create(&mut file, SegType::BTree, 11, 101, 8, 0, 0)
            .expect("索引段");
        let _s2 = crate::segment::Segment::create(&mut file, SegType::Heap, 12, 102, 8, 0, 0)
            .expect("表段");
        // 两份自举副本都清掉（模拟全坏）⇒ 走重建。
        for block in [BOOTSTRAP_MAIN_BLOCK, BOOTSTRAP_REPLICA_BLOCK] {
            let mut junk = Page::new(PageType::HeapTable, WS, 0, block);
            junk.seal();
            file.write_page(block, &mut junk).unwrap();
        }
        assert!(read_with_heal(&mut file).is_err());
        let rebuilt = rebuild_from_segment_headers(&file).unwrap();
        assert_eq!(rebuilt.len(), 2, "两个段头页：{rebuilt:?}");
        assert_eq!(rebuilt[0].dataobj, 101);
        assert_eq!(rebuilt[0].seg_type, SegType::BTree as u8);
        assert_eq!(rebuilt[1].dataobj, 102);
        assert_eq!(rebuilt[1].seg_type, SegType::Heap as u8);
        assert_eq!(
            rebuilt[0].seg_header.block_id(),
            FileLayout::meta().data_area_first_block,
            "段头位置 = 数据区首个块"
        );
        // 重建结果可编码成页 ⇒ 往返一致（调用方随后写回主/副本）。
        let mut page = Page::new(PageType::Bootstrap, WS, 0, BOOTSTRAP_MAIN_BLOCK);
        encode(&mut page, &rebuilt).unwrap();
        assert_eq!(decode(&page).unwrap(), rebuilt);
    }

    #[test]
    fn standard_role_file_is_rejected() {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let mut file = DataFile::create(
            &io,
            Path::new("/mem/b5.dat"),
            3,
            3,
            WS,
            crate::datafile::MIN_FILE_BLOCKS,
        )
        .unwrap();
        assert!(matches!(
            write_all(&mut file, &entries(1)),
            Err(BootstrapError::NotMetaFile)
        ));
        assert!(matches!(
            read_with_heal(&mut file),
            Err(BootstrapError::NotMetaFile)
        ));
    }
}
