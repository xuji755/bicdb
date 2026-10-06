//! **建区期：把自举集建出来**（`目录详设` §5.1 的第 ②③④ 步）。
//!
//! ```text
//! ① file 0（role = 0，带式排布）—— 调用方建好文件与文件头页
//! ② 7 张自举表：堆段（表）——段头自描述 dataobj#/obj#
//!    8 个自举索引：BTree 段 + **空 B+Tree**（树头写段头扩展区）
//! ③ 引导页三件套：主（块 1）+ 副本（块 256）+ **文件头副本**（块 257）→ fsync
//! ```
//!
//! **为什么这一段可以"直写、无 redo"**：引导页**本身不产生 redo**（它是恢复
//! 锚点），自举段是**首次创建**——建区中途崩溃 ⇒ 该工作区未登记 `ws$`，
//! 按"未完成创建"整体清除（§5.1 的可见性分界）。**此后的字典写入（种子行、
//! `stat$`/`seq$`、用户 DDL）一律走事务 + redo**（C4）。
//!
//! **编号口径（本模块冻结）**：自举对象 `obj# = dataobj#`，自 **1** 起按
//! "**表先、索引后**"的固定顺序连号（`obj# ≤ 99` 是自举区间）；顺序即
//! [`dict::DICT_TABLES`] 的顺序。用户对象由 `seq$` 分配、**≥ 100**。

use bicdb_index::{IndexError, Tree};
use bicdb_storage::bootstrap::{self, BootstrapEntry};
use bicdb_storage::datafile::DataFile;
use bicdb_storage::page::{Page, PageType};
use bicdb_storage::rowid::RowId;
use bicdb_storage::segment::{self, SegType, Segment, SegmentSpaceError};

use crate::dict::{self, ColTypeCode, DictTable};

/// 建区期错误。
#[derive(Debug)]
pub enum CreateError {
    /// 不是元数据文件（role ≠ 0）——自举集只建在 file 0。
    NotMetaFile,
    /// 段层错误。
    Segment(SegmentSpaceError),
    /// 索引层错误。
    Index(IndexError),
    /// 树头写段头扩展区的错误。
    TreeHead(segment::SegmentError),
    /// 引导页层错误。
    Bootstrap(bootstrap::BootstrapError),
    /// 字典表常量自检失败（**建区前**跑一遍——常量坏了不能建出坏字典）。
    ConstantsBroken(String),
    /// 自举对象号越出保留区间（`obj# ≤ 99`）。
    ObjNumberOverflow {
        /// 越出的对象号。
        obj: u32,
    },
    /// 段头页编不出 ROWID（块号/文件号越域——建区参数错）。
    BadSegHeader {
        /// 段头页块号。
        block: u32,
    },
}

impl std::fmt::Display for CreateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CreateError::NotMetaFile => f.write_str("自举集只建在 file 0（role = 0）上"),
            CreateError::Segment(e) => write!(f, "建区段层：{e}"),
            CreateError::Index(e) => write!(f, "建区索引层：{e}"),
            CreateError::TreeHead(e) => write!(f, "建区写树头：{e}"),
            CreateError::Bootstrap(e) => write!(f, "建区引导页：{e}"),
            CreateError::ConstantsBroken(why) => write!(f, "字典常量自检失败：{why}"),
            CreateError::ObjNumberOverflow { obj } => {
                write!(f, "自举对象号 {obj} 越出保留区间（≤ 99）")
            }
            CreateError::BadSegHeader { block } => {
                write!(f, "段头页块 {block} 编不出 ROWID（越域）")
            }
        }
    }
}

impl std::error::Error for CreateError {}

impl From<SegmentSpaceError> for CreateError {
    fn from(e: SegmentSpaceError) -> Self {
        Self::Segment(e)
    }
}
impl From<IndexError> for CreateError {
    fn from(e: IndexError) -> Self {
        Self::Index(e)
    }
}
impl From<segment::SegmentError> for CreateError {
    fn from(e: segment::SegmentError) -> Self {
        Self::TreeHead(e)
    }
}
impl From<bootstrap::BootstrapError> for CreateError {
    fn from(e: bootstrap::BootstrapError) -> Self {
        Self::Bootstrap(e)
    }
}

/// 建出来的一个自举对象（诊断/后续种子行用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapObject {
    /// 对象号（= 数据对象号，见模块口径）。
    pub obj: u32,
    /// 对象名（表名或索引名）。
    pub name: &'static str,
    /// 所属表名（索引才有；表自身为 `None`）。
    pub on_table: Option<&'static str>,
    /// 段头页**物理块号**。
    pub seg_header_block: u32,
}

/// 建区结果：引导页条目 + 自举对象清单（按建区顺序）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltDictionary {
    /// 引导页条目（按建区顺序；表先索引后）。
    pub entries: Vec<BootstrapEntry>,
    /// 自举对象清单（同上顺序）。
    pub objects: Vec<BootstrapObject>,
}

impl BuiltDictionary {
    /// 自举条目数（普通 15 / `public` 23——与 [`dict`] 的定量一致）。
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 是否为空（结构上永不空——口径便利口）。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 按对象名取对象（种子行用）。
    #[must_use]
    pub fn object(&self, name: &str) -> Option<&BootstrapObject> {
        self.objects.iter().find(|o| o.name == name)
    }
}

/// **建自举集**（§5.1 的第 ②③ 步 + 引导页写入）。
///
/// `file` 必须是已建好的 **file 0**（role = 0，带式排布、块 0 文件头已写）；
/// 返回后调用方负责 **fsync** 与后续步骤（种子行、`stat$`/`seq$`、`ws$` 登记）。
pub fn create_dictionary(
    file: &mut DataFile<'_>,
    ws: [u8; 8],
    is_public: bool,
) -> Result<BuiltDictionary, CreateError> {
    if file.layout() != bicdb_storage::bitmap::FileLayout::meta() {
        return Err(CreateError::NotMetaFile);
    }
    // 常量自检先跑（常量坏了不能建出坏字典）。
    dict::self_check().map_err(CreateError::ConstantsBroken)?;

    let plan = dict::bootstrap_plan(is_public);
    let mut entries = Vec::with_capacity(plan.len());
    let mut objects = Vec::with_capacity(plan.len());

    for (obj, (table, key)) in (1u32..).zip(plan) {
        if obj > dict::obj_kind::BOOTSTRAP_MAX {
            return Err(CreateError::ObjNumberOverflow { obj });
        }
        // **口径**：自举对象 obj# = dataobj#（模块文档）。
        let dataobj = obj;
        let (name, seg_type, on_table) = match key {
            None => (table.name, SegType::Heap, None),
            Some(k) => (k.name, SegType::BTree, Some(table.name)),
        };
        let mut segment = Segment::create(file, seg_type, obj, dataobj, 8, 0, 0)?;
        let seg_header_block = segment.page0_block();
        if key.is_some() {
            // **空 B+Tree**：树头写段头页的扩展区（§9.1.5 第 8 步）。
            let file_id = segment.file_id();
            let root = {
                let mut store = bicdb_index::SegmentStore::new(&mut segment, ws);
                let tree = Tree::create(&mut store, file_id, ws)?;
                tree.root()
            };
            // 段头页 = **逻辑页 0**（`read_page` 是逻辑号；`page0_block()` 是物理块）。
            let mut header = segment.read_page(0)?;
            segment::write_tree_head(&mut header, root)?;
            segment.write_page(0, &mut header)?;
        }
        entries.push(BootstrapEntry {
            dataobj,
            seg_type: seg_type as u8,
            seg_header: seg_header_rowid(file.file_id(), seg_header_block).map_err(|_| {
                CreateError::BadSegHeader {
                    block: seg_header_block,
                }
            })?,
            iniexts: 1,
        });
        objects.push(BootstrapObject {
            obj,
            name,
            on_table,
            seg_header_block,
        });
    }

    // 引导页三件套（主 + 副本 + 文件头副本）。
    bootstrap::write_all(file, &entries)?;
    Ok(BuiltDictionary { entries, objects })
}

/// **段头页的引导页 ROWID**（6B：file + block + **行号恒 1**——页级引用，
/// 与 `bicdb-storage::bootstrap` 的重建路径同一约定）。
pub fn seg_header_rowid(
    file_id: u16,
    block: u32,
) -> Result<RowId, bicdb_storage::rowid::RowIdRangeError> {
    RowId::from_parts(file_id, block, 1)
}

/// 一列种子行的值（建区种子用：`obj$`/`tab$`/`col$`/… 的字典行）。
///
/// **只承载"写行"所需的形态**：字典行的列类型是内核常量（[`dict`]），
/// 值由建区路径按列序给出。
#[derive(Debug, Clone, PartialEq)]
pub enum SeedValue {
    /// 数值（`NUMBER`——字典表里一律以整数形态出现）。
    Num(u64),
    /// 文本（`VARCHAR2`）。
    Text(String),
    /// 布尔。
    Bool(bool),
    /// 字节串（字典内部列，如 `col$.deflt`）。
    Bytes(Vec<u8>),
}

/// **种子行的列值 → 行形状**（按 [`dict::ColDef`] 的列序排好）。
///
/// 返回 `(列类型码, 值)` 序列——调用方（C4 的写路径）据此编码行。
#[must_use]
pub fn row_shape_of(table: &DictTable) -> Vec<ColTypeCode> {
    table.columns.iter().map(|c| c.type_code).collect()
}

/// 页类型断言：字典表的页必须是堆表页（诊断用）。
pub fn is_heap_table_page(page: &Page) -> bool {
    page.header().map(|h| h.page_type) == Some(PageType::HeapTable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bicdb_storage::bitmap::FileLayout;
    use bicdb_storage::bootstrap::{read_with_heal, BootstrapSource};
    use bicdb_storage::datafile::DataFile;
    use bicdb_storage::segment::{read_tree_head, Segment};
    use bicdb_workspace::io::MemFileIo;
    use std::path::Path;

    const WS: [u8; 8] = [7u8; 8];

    fn meta_file<'a>(io: &'a MemFileIo, path: &str) -> DataFile<'a> {
        let layout = FileLayout::meta();
        DataFile::create(
            io,
            Path::new(path),
            0,
            bicdb_storage::bitmap::META_ROLE,
            WS,
            layout.min_file_blocks() + 512,
        )
        .expect("建 file 0")
    }

    #[test]
    fn builds_the_normal_bootstrap_set() {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let mut file = meta_file(&io, "/mem/c1.dat");
        let built = create_dictionary(&mut file, WS, false).expect("建自举集");
        assert_eq!(built.len(), 15, "普通工作区 15 条（7 表 + 8 索引）");
        // 表先索引后；obj$ 的两个键紧随其后。
        let names: Vec<&str> = built.objects.iter().map(|o| o.name).collect();
        assert_eq!(
            &names[..4],
            &["obj$", "i_obj_pk", "i_obj_name", "tab$"],
            "顺序：表 → 其键 → 下一张表"
        );
        assert_eq!(names[14], "i_undo_pk");
        // obj# = dataobj# = 1..15；都在自举区间。
        for (i, o) in built.objects.iter().enumerate() {
            assert_eq!(o.obj, i as u32 + 1);
            assert!(o.obj <= dict::obj_kind::BOOTSTRAP_MAX);
        }
        // 每条的段头都在数据区（file 0 的带式排布：自块 832 起）。
        let d0 = FileLayout::meta().data_area_first_block;
        for e in &built.entries {
            assert!(e.seg_header.block_id() >= d0, "段头在数据区");
            assert_eq!(e.seg_header.row_id(), 1, "页级引用：行号恒 1");
        }
    }

    #[test]
    fn built_segments_open_with_matching_self_description() {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let mut file = meta_file(&io, "/mem/c2.dat");
        let built = create_dictionary(&mut file, WS, false).unwrap();

        // 逐条开段：段头自描述（dataobj/类型）与引导页条目一致。
        for (e, obj) in built.entries.iter().zip(built.objects.iter()) {
            let block = e.seg_header.block_id();
            let segment = Segment::open(&mut file, block).expect("开段");
            assert_eq!(segment.header().dataobj, e.dataobj);
            assert_eq!(segment.header().seg_type as u8, e.seg_type);
            assert_eq!(segment.header().obj, obj.obj);
            if obj.on_table.is_some() {
                // 索引段：树头已写（扩展区里有有效根页），可开树搜索。
                let root = read_tree_head(&segment.read_page(0).unwrap()).expect("树头");
                // 段内布局：逻辑 0 = 段头、逻辑 1 = 段内位图、逻辑 2 起 = 数据页。
                assert_eq!(root.block_id(), block + 2, "空树：一个叶页（区的第 3 页）");
            }
        }
    }

    #[test]
    fn index_trees_are_reopenable_and_empty() {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let mut file = meta_file(&io, "/mem/c3.dat");
        let built = create_dictionary(&mut file, WS, false).unwrap();
        let obj_idx = built.object("i_obj_pk").expect("i_obj_pk");
        let mut segment = Segment::open(&mut file, obj_idx.seg_header_block).unwrap();
        let root = read_tree_head(&segment.read_page(0).unwrap()).unwrap();
        let mut store = bicdb_index::SegmentStore::new(&mut segment, WS);
        let mut tree = bicdb_index::Tree::open(&mut store, 0, root).expect("开空树");
        assert_eq!(tree.root(), root);
        let hit = tree.lookup(&[1u8]).unwrap();
        assert!(hit.is_none(), "空树：查无");
    }

    #[test]
    fn bootstrap_page_round_trips_through_a_reopen() {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let path = Path::new("/mem/c4.dat");
        let built = {
            let mut file = meta_file(&io, "/mem/c4.dat");
            create_dictionary(&mut file, WS, false).unwrap()
        };
        let mut file = DataFile::open(&io, path).unwrap();
        let (entries, src) = read_with_heal(&mut file).unwrap();
        assert_eq!(entries, built.entries, "关盘重开：引导页条目一致");
        assert_eq!(src, BootstrapSource::Main);
    }

    #[test]
    fn public_workspace_builds_23_entries() {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let mut file = meta_file(&io, "/mem/c5.dat");
        let built = create_dictionary(&mut file, WS, true).unwrap();
        assert_eq!(built.len(), 23, "public：15 + 3 表 + 5 索引");
        for name in ["user$", "ws$", "fs$", "i_ws_name", "i_user_name"] {
            assert!(built.object(name).is_some(), "public 应含 {name}");
        }
        // 普通工作区不建 public 独有三张。
        let io2 = MemFileIo::new();
        io2.add_dir("/mem");
        let mut file2 = meta_file(&io2, "/mem/c6.dat");
        let normal = create_dictionary(&mut file2, WS, false).unwrap();
        assert!(normal.object("user$").is_none());
        assert!(normal.object("ws$").is_none());
    }

    #[test]
    fn standard_role_file_is_rejected() {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let mut file = DataFile::create(
            &io,
            Path::new("/mem/c7.dat"),
            3,
            3,
            WS,
            bicdb_storage::datafile::MIN_FILE_BLOCKS,
        )
        .unwrap();
        let err = create_dictionary(&mut file, WS, false).unwrap_err();
        assert!(matches!(err, CreateError::NotMetaFile), "{err}");
    }
}
