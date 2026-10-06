//! **打开链与建区种子**（C2b；`目录详设` §4.1 的第 ②–⑥ 步）。
//!
//! ```text
//! open：  file 0 → 引导页（主 → 副本自愈）→ 按**自举计划**逐条开段自证
//!         → Catalog（按名取段 / 扫表 / 按索引点查 / 按 ROWID 取行）
//! seed：  建区⑤的"最小自洽字典"——自举集里每张表与每个索引的**自身字典行**
//!         （obj$/tab$/col$/ind$/icol$/seg$）+ 各索引的条目（直接写，见下）
//! ```
//!
//! **自举计划的身份**：引导页只给 `dataobj#`，**哪条是谁**由**内核常量的顺序**
//! 决定（[`crate::dict::bootstrap_plan`]）——建区与打开用**同一份计划**，
//! 顺序即身份（改顺序 = 改格式）。
//!
//! **种子行为什么可以直写、无 redo**：与建区期同一条理由（`create.rs` 的模块
//! 文档）——建区是"全有或全无"：中途崩溃 ⇒ 未登记 `ws$` ⇒ 整体清除。

use std::collections::BTreeMap;
use std::path::Path;

use bicdb_index::{IndexError, SegmentStore, Tree};
use bicdb_storage::bootstrap::{self, BootstrapEntry};
use bicdb_storage::datafile::{DataFile, DataFileError};
use bicdb_storage::heap::{self, InsertPolicy};
use bicdb_storage::key;
use bicdb_storage::page::{Page, PageType};
use bicdb_storage::row::RowView;
use bicdb_storage::rowid::RowId;
use bicdb_storage::segment::{self, Segment, SegmentError, SegmentSpaceError};
use bicdb_workspace::io::FileIo;

use crate::create::BuiltDictionary;
use crate::dict::{self, DictTable, KeyDef};
use crate::row::{self, DictValue, RowCodecError};

/// 打开/种子错误。
#[derive(Debug)]
pub enum OpenError {
    /// 数据文件层错误。
    DataFile(DataFileError),
    /// 引导页层错误（含 `BothCopiesBad`——需重建）。
    Bootstrap(bootstrap::BootstrapError),
    /// 段层错误。
    Segment(SegmentSpaceError),
    /// 段头自证错误。
    SegmentHead(SegmentError),
    /// 索引层错误。
    Index(IndexError),
    /// 行编解码错误。
    Row(RowCodecError),
    /// 不是元数据文件（role ≠ 0）。
    NotMetaFile,
    /// 引导页条目数与自举计划对不上。
    EntryCount {
        /// 实际条目数。
        got: usize,
        /// 普通工作区应有的数目。
        normal: usize,
        /// `public` 应有的数目。
        public: usize,
    },
    /// 引导页条目与自举计划不自洽（段类型/段头自证不符）。
    PlanMismatch(String),
    /// 表/索引不在自举集里。
    NoSuchObject(String),
    /// 页不是堆表页（字典表段的页）。
    NotAHeapPage {
        /// 块号。
        block: u32,
    },
    /// 行不存在（按 ROWID 取行落空——损坏或并发删除）。
    RowMissing(RowId),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::DataFile(e) => write!(f, "目录数据文件：{e}"),
            OpenError::Bootstrap(e) => write!(f, "目录引导页：{e}"),
            OpenError::Segment(e) => write!(f, "目录段：{e}"),
            OpenError::SegmentHead(e) => write!(f, "目录段头自证：{e}"),
            OpenError::Index(e) => write!(f, "目录索引：{e}"),
            OpenError::Row(e) => write!(f, "目录行：{e}"),
            OpenError::NotMetaFile => f.write_str("目录只在 file 0（role = 0）上"),
            OpenError::EntryCount {
                got,
                normal,
                public,
            } => write!(
                f,
                "引导页条目数 {got} 与自举计划不符（普通 {normal} / public {public}）"
            ),
            OpenError::PlanMismatch(why) => write!(f, "引导页与自举计划不自洽：{why}"),
            OpenError::NoSuchObject(name) => write!(f, "自举集里没有对象 {name}"),
            OpenError::NotAHeapPage { block } => write!(f, "块 {block} 不是堆表页"),
            OpenError::RowMissing(rid) => write!(f, "ROWID {rid} 的行不存在"),
        }
    }
}

impl std::error::Error for OpenError {}

macro_rules! from_err {
    ($($v:ident <- $t:ty),* $(,)?) => {
        $(impl From<$t> for OpenError { fn from(e: $t) -> Self { Self::$v(e) } })*
    };
}
from_err!(
    DataFile <- DataFileError,
    Bootstrap <- bootstrap::BootstrapError,
    Segment <- SegmentSpaceError,
    SegmentHead <- SegmentError,
    Index <- IndexError,
    Row <- RowCodecError,
);

fn mismatch(why: impl Into<String>) -> OpenError {
    OpenError::PlanMismatch(why.into())
}

/// **一个索引条目**：键分量（每分量 = 该列的保序编码；`None` = NULL）+ 行地址。
///
/// 分量**已解码、未拼接**——回表比较/诊断用；索引比较路径走
/// [`bicdb_storage::key::encode`] 后的整键（不解码）。
pub type IndexEntry = (Vec<Option<Vec<u8>>>, RowId);

/// **打开的自举字典**（一个工作区的 file 0）。
pub struct Catalog<'io> {
    file: DataFile<'io>,
    ws: [u8; 8],
    is_public: bool,
    entries: Vec<BootstrapEntry>,
    /// 表名 → （段头块、表定义）。
    tables: BTreeMap<&'static str, (u32, &'static DictTable)>,
    /// 索引名 → （段头块、键定义、所属表）。
    indexes: BTreeMap<&'static str, (u32, KeyDef, &'static str)>,
    /// **字典行缓存**（§4.2；每工作区一份——不跨工作区）。
    pub(crate) cache: crate::cache::RowCache,
    /// **当前的提交序号**（装载戳；打开时 = 恢复后的最新已提交序号）。
    pub(crate) current_seq: u64,
}

impl<'io> Catalog<'io> {
    /// **打开链**（第 ②③④ 步；`目录详设` §4.1）。
    pub fn open(io: &'io dyn FileIo, path: &Path) -> Result<Self, OpenError> {
        let mut file = DataFile::open(io, path)?;
        if file.layout() != bicdb_storage::bitmap::FileLayout::meta() {
            return Err(OpenError::NotMetaFile);
        }
        let (entries, _source) = bootstrap::read_with_heal(&mut file)?;
        Self::from_entries(file, entries)
    }

    /// 由既有文件 + 引导页条目装配（`open` 的后半；重建路径也可复用）。
    pub fn from_entries(
        mut file: DataFile<'io>,
        entries: Vec<BootstrapEntry>,
    ) -> Result<Self, OpenError> {
        let normal = dict::bootstrap_plan(false).len();
        let public = dict::bootstrap_plan(true).len();
        let is_public = if entries.len() == normal {
            false
        } else if entries.len() == public {
            true
        } else {
            return Err(OpenError::EntryCount {
                got: entries.len(),
                normal,
                public,
            });
        };
        let plan = dict::bootstrap_plan(is_public);
        let ws = file.workspace_ref();
        let mut tables = BTreeMap::new();
        let mut indexes = BTreeMap::new();
        for (i, (entry, (table, key))) in entries.iter().zip(plan.iter()).enumerate() {
            // ① 段类型与计划一致。
            let expect = if key.is_none() {
                segment::SegType::Heap as u8
            } else {
                segment::SegType::BTree as u8
            };
            if entry.seg_type != expect {
                return Err(mismatch(format!(
                    "第 {i} 条 {} 的段类型 {} ≠ 计划 {expect}",
                    table.name, entry.seg_type
                )));
            }
            // ② 段头自证（dataobj / seg_type）。
            let block = entry.seg_header.block_id();
            let seg = Segment::open(&mut file, block)?;
            if seg.header().dataobj != entry.dataobj
                || seg.header().seg_type as u8 != entry.seg_type
            {
                return Err(mismatch(format!(
                    "第 {i} 条 {}：段头自证不符（dataobj {} / type {}）",
                    table.name,
                    seg.header().dataobj,
                    seg.header().seg_type as u8
                )));
            }
            drop(seg);
            match key {
                None => {
                    tables.insert(table.name, (block, *table));
                }
                Some(k) => {
                    indexes.insert(k.name, (block, *k, table.name));
                }
            }
        }
        Ok(Self {
            file,
            ws,
            is_public,
            entries,
            tables,
            indexes,
            cache: crate::cache::RowCache::new(0),
            current_seq: 0,
        })
    }

    /// 工作区标识。
    #[must_use]
    pub fn workspace(&self) -> [u8; 8] {
        self.ws
    }

    /// 是不是 `public` 工作区。
    #[must_use]
    pub fn is_public(&self) -> bool {
        self.is_public
    }

    /// 引导页条目（诊断）。
    #[must_use]
    pub fn entries(&self) -> &[BootstrapEntry] {
        &self.entries
    }

    /// 自举集里的表名（诊断）。
    #[must_use]
    pub fn table_names(&self) -> Vec<&'static str> {
        self.tables.keys().copied().collect()
    }

    /// 表定义（列/键）。
    pub fn table_def(&self, table: &str) -> Result<&'static DictTable, OpenError> {
        Ok(self
            .tables
            .get(table)
            .ok_or_else(|| OpenError::NoSuchObject(table.to_owned()))?
            .1)
    }

    /// 某个键的定义（按索引名）。
    pub fn key_def(&self, index: &str) -> Result<(&'static str, KeyDef), OpenError> {
        let (_, k, t) = self
            .indexes
            .get(index)
            .ok_or_else(|| OpenError::NoSuchObject(index.to_owned()))?;
        Ok((*t, *k))
    }

    /// 按名取表段（作用域内借用——段打开是廉价的：读一张段头页）。
    pub fn segment<'s>(&'s mut self, table: &str) -> Result<Segment<'io, 's>, OpenError> {
        let (block, _) = self
            .tables
            .get(table)
            .ok_or_else(|| OpenError::NoSuchObject(table.to_owned()))?;
        Ok(Segment::open(&mut self.file, *block)?)
    }

    /// **扫全表**（字典表都很小；返回 `(ROWID, 行值)`，按物理序）。
    pub fn scan(&mut self, table: &str) -> Result<Vec<(RowId, Vec<DictValue>)>, OpenError> {
        let def = self.table_def(table)?;
        let fid = self.file.file_id();
        let seg = self.segment(table)?;
        let hwm = seg.hwm();
        let mut out = Vec::new();
        for logical in 0..hwm {
            if logical == 0 || seg.is_bitmap_page(logical) {
                continue;
            }
            let Some(block) = seg.logical_block(logical) else {
                continue;
            };
            let page = seg.read_page(logical)?;
            if page.header().map(|h| h.page_type) != Some(PageType::HeapTable) {
                continue;
            }
            for row_no in 1..=page.slot_count() {
                let Some(bytes) = heap::row(&page, row_no) else {
                    continue;
                };
                let rid =
                    RowId::from_parts(fid, block, row_no).map_err(|_| mismatch("行号越域"))?;
                out.push((rid, row::decode(bytes, def.columns)?));
            }
        }
        Ok(out)
    }

    /// **按 ROWID 取一行**。
    pub fn fetch(&mut self, table: &str, rid: RowId) -> Result<Vec<DictValue>, OpenError> {
        let def = self.table_def(table)?;
        let mut seg = self.segment(table)?;
        let page = read_page_of(&mut seg, rid.block_id())?;
        let bytes = row::read_from_page(&page, rid.row_id()).ok_or(OpenError::RowMissing(rid))?;
        Ok(row::decode(bytes, def.columns)?)
    }

    /// **按索引点查**：`components` 与索引键列一一对应（`None` = NULL），
    /// 字节形态必须与**行内字节**一致（数值列用 [`comp_num`]、文本列用
    /// [`comp_text`]——`arch/06` §6.0 的保序编码口径）。
    pub fn lookup(
        &mut self,
        index: &str,
        components: &[Option<&[u8]>],
    ) -> Result<Option<(RowId, Vec<DictValue>)>, OpenError> {
        let (table, key_def) = self.key_def(index)?;
        if components.len() != key_def.cols.len() {
            return Err(mismatch(format!(
                "键分量数 {} ≠ 索引 {index} 的键列数 {}",
                components.len(),
                key_def.cols.len()
            )));
        }
        let encoded = key::encode(components);
        let (block, _, _) = self
            .indexes
            .get(index)
            .ok_or_else(|| OpenError::NoSuchObject(index.to_owned()))?;
        let block = *block;
        let fid = self.file.file_id();
        let found = {
            let mut seg = Segment::open(&mut self.file, block)?;
            let root = segment::read_tree_head(&seg.read_page(0)?)?;
            let mut store = SegmentStore::new(&mut seg, self.ws);
            let mut tree = Tree::open(&mut store, fid, root)?;
            tree.lookup(&encoded)?
        };
        match found {
            None => Ok(None),
            Some(rid) => {
                let row = self.fetch(table, rid)?;
                Ok(Some((rid, row)))
            }
        }
    }

    /// **一个索引的条目**（`(键分量, ROWID)`，键序；`lo`/`hi` 为闭区间，
    /// `None` = 无界）——全扫（`i_obj_name` 的清单）与前缀范围扫
    /// （`i_col_pk` 取某对象的列组）共用本口。
    pub fn range_index(
        &mut self,
        index: &str,
        lo: Option<&[u8]>,
        hi: Option<&[u8]>,
    ) -> Result<Vec<IndexEntry>, OpenError> {
        let (block, _, _) = self
            .indexes
            .get(index)
            .ok_or_else(|| OpenError::NoSuchObject(index.to_owned()))?;
        let block = *block;
        let fid = self.file.file_id();
        let entries = {
            let mut seg = Segment::open(&mut self.file, block)?;
            let root = segment::read_tree_head(&seg.read_page(0)?)?;
            let mut store = SegmentStore::new(&mut seg, self.ws);
            let mut tree = Tree::open(&mut store, fid, root)?;
            tree.range(lo, hi, usize::MAX)?
        };
        let mut result = Vec::with_capacity(entries.len());
        for (k, rid) in entries {
            let comps = key::decode(&k).map_err(|e| mismatch(e.to_string()))?;
            result.push((comps, rid));
        }
        Ok(result)
    }

    /// **索引全扫**（键序）。
    pub fn scan_index(&mut self, index: &str) -> Result<Vec<IndexEntry>, OpenError> {
        self.range_index(index, None, None)
    }
}

fn read_page_of(seg: &mut Segment<'_, '_>, block: u32) -> Result<Page, OpenError> {
    let hwm = seg.hwm();
    for logical in 0..hwm {
        if seg.logical_block(logical) == Some(block) {
            return Ok(seg.read_page(logical)?);
        }
    }
    Err(OpenError::NotAHeapPage { block })
}

// ─────────────── 键分量的字节形态（与行内字节同源）───────────────

/// 数值分量的字节（字典表的数值列一律 `INTEGER` = `NUMBER(38,0)` 的保序编码）。
#[must_use]
pub fn comp_num(n: u64) -> Vec<u8> {
    bicdb_types::Number::parse(&n.to_string())
        .expect("域内整数可解析")
        .encode()
}

/// 文本分量的字节（`VARCHAR2` 的原始字节）。
#[must_use]
pub fn comp_text(s: &str) -> Vec<u8> {
    s.as_bytes().to_vec()
}

// ─────────────────────────── 建区种子（§5.1 的 ⑤ 的最小自洽面）───────────────────────────

impl<'io> Catalog<'io> {
    /// **写入最小自洽字典**：自举集里每张表/每个索引的**自身字典行**
    /// （`obj$`/`tab$`/`col$`/`ind$`/`icol$`/`seg$`）+ 各索引的条目。
    ///
    /// **直接写、无 redo**：建区期（见模块文档）。`ctime`/`mtime` = 0
    /// （建区尚无提交序号；首个用户 DDL 由 DDL 路径推进——`目录详设` §5.2）。
    /// `stat$`/`seq$` 与预置业务对象**不在此**（走 DDL 路径，C4）。
    pub fn seed_own_dictionary(&mut self, built: &BuiltDictionary) -> Result<usize, OpenError> {
        let mut w = SeedWriter::new(self)?;
        let mut rows = 0usize;
        for obj in &built.objects {
            match obj.on_table {
                None => {
                    // ── 表 ──
                    let t = table_def_of(obj.name);
                    w.insert(
                        "obj$",
                        obj_row(
                            obj.obj,
                            t.name,
                            dict::namespace::TABLE,
                            dict::obj_kind::TABLE,
                        ),
                    )?;
                    let cols = t.columns.len() as u64;
                    w.insert(
                        "tab$",
                        vec![
                            DictValue::Num(u64::from(obj.obj)),
                            DictValue::Num(cols),
                            DictValue::Num(0),     // pctfree
                            DictValue::Num(8),     // itl_max
                            DictValue::Bool(true), // transactional（stat$ 例外——C4 建它时改写）
                            DictValue::Num(u64::from(dict::table_opt::LOGGING_FULL)),
                            DictValue::Num(u64::from(dict::table_opt::UPDATE_IN_PLACE)),
                            DictValue::Num(0), // retention
                            DictValue::Num(0), // version_keep
                            DictValue::Num(u64::from(dict::table_opt::CLEANUP_NONE)),
                            DictValue::Num(u64::from(dict::table_opt::EMBED_NONE)),
                            DictValue::Bool(false), // shared
                        ],
                    )?;
                    rows += 2;
                    for c in t.columns {
                        w.insert(
                            "col$",
                            vec![
                                DictValue::Num(u64::from(obj.obj)),
                                DictValue::Num(u64::from(c.col)),
                                DictValue::Text(c.name.to_owned()),
                                DictValue::Num(u64::from(c.type_code as u8)),
                                DictValue::Num(u64::from(c.length)),
                                DictValue::Null,
                                DictValue::Null,
                                DictValue::Bool(c.nullable),
                                DictValue::Null,
                                DictValue::Num(0),
                            ],
                        )?;
                        rows += 1;
                    }
                }
                Some(base) => {
                    // ── 索引 ──
                    let key = key_def_of(obj.name);
                    let base_obj = built
                        .object(base)
                        .ok_or_else(|| {
                            mismatch(format!("索引 {} 的基表 {base} 不在计划里", obj.name))
                        })?
                        .obj;
                    w.insert(
                        "obj$",
                        obj_row(
                            obj.obj,
                            key.name,
                            dict::namespace::INDEX,
                            dict::obj_kind::INDEX,
                        ),
                    )?;
                    w.insert(
                        "ind$",
                        vec![
                            DictValue::Num(u64::from(obj.obj)),
                            DictValue::Num(u64::from(base_obj)),
                            DictValue::Num(u64::from(dict::index_kind::BTREE)),
                            DictValue::Num(key.cols.len() as u64),
                            DictValue::Bool(key.unique),
                            DictValue::Num(1), // status：有效
                            DictValue::Null,   // expr_src（非表达式索引）
                        ],
                    )?;
                    rows += 2;
                    for (pos, col) in key.cols.iter().enumerate() {
                        w.insert(
                            "icol$",
                            vec![
                                DictValue::Num(u64::from(obj.obj)),
                                DictValue::Num(pos as u64 + 1),
                                DictValue::Num(u64::from(*col)),
                                DictValue::Bool(false),
                            ],
                        )?;
                        rows += 1;
                    }
                }
            }
            // ── 段行（每个对象一行）──
            w.insert(
                "seg$",
                vec![
                    DictValue::Num(u64::from(obj.obj)),
                    DictValue::Num(0), // file_id（file 0）
                    DictValue::Num(u64::from(obj.seg_header_block)),
                    DictValue::Num(1), // iniexts
                    DictValue::Num(0), // ctime
                ],
            )?;
            rows += 1;
        }
        Ok(rows)
    }
}

/// `obj$` 的一行。
fn obj_row(obj: u32, name: &str, ns: u32, type_: u32) -> Vec<DictValue> {
    vec![
        DictValue::Num(u64::from(obj)),
        DictValue::Text(name.to_owned()),
        DictValue::Num(u64::from(ns)),
        DictValue::Num(u64::from(type_)),
        DictValue::Num(u64::from(obj)), // dataobj# = obj#（自举口径）
        DictValue::Num(0),              // ctime
        DictValue::Num(0),              // mtime
        DictValue::Num(1),              // status：有效
    ]
}

fn table_def_of(name: &str) -> &'static DictTable {
    dict::DICT_TABLES
        .iter()
        .find(|t| t.name == name)
        .expect("自举计划里的表必在常量表里")
}

fn key_def_of(name: &str) -> KeyDef {
    dict::DICT_TABLES
        .iter()
        .flat_map(|t| t.keys.iter())
        .find(|k| k.name == name)
        .copied()
        .expect("自举计划里的键必在常量表里")
}

/// 建区期的字典行写入口（**顺序追加**：建区只插不改；**同步维护该宿主表的
/// 全部索引**——键分量取自**行的字节**，与点查口径同源）。
struct SeedWriter<'a, 'io> {
    cat: &'a mut Catalog<'io>,
    /// 表名 → 当前逻辑页。
    open_pages: BTreeMap<&'static str, u32>,
}

impl<'a, 'io> SeedWriter<'a, 'io> {
    fn new(cat: &'a mut Catalog<'io>) -> Result<Self, OpenError> {
        Ok(Self {
            cat,
            open_pages: BTreeMap::new(),
        })
    }

    /// 追加一行到 `table`，并维护该表**全部键**的索引条目。
    fn insert(&mut self, table: &'static str, values: Vec<DictValue>) -> Result<RowId, OpenError> {
        let def = self.cat.table_def(table)?;
        let bytes = row::encode(&values, def.columns)?;
        let fid = self.cat.file.file_id();
        let ws = self.cat.ws;

        // ① 堆行：当前页能放就放，否则新页（顺序追加）。
        let mut seg = self.cat.segment(table)?;
        let (logical, mut page) = match self.open_pages.get(table).copied() {
            Some(logical) => {
                let page = seg.read_page(logical)?;
                if heap::can_insert(&page, bytes.len(), &InsertPolicy::in_place(0)) {
                    (logical, page)
                } else {
                    let (l, p) = new_dict_page(&mut seg, ws, fid)?;
                    self.open_pages.insert(table, l);
                    (l, p)
                }
            }
            None => {
                let (l, p) = new_dict_page(&mut seg, ws, fid)?;
                self.open_pages.insert(table, l);
                (l, p)
            }
        };
        let row_no = heap::insert_row(&mut page, &bytes, &InsertPolicy::in_place(0))
            .map_err(|e| mismatch(format!("字典行插入：{e}")))?;
        seg.write_page(logical, &mut page)?;
        let block = seg
            .logical_block(logical)
            .ok_or_else(|| mismatch("页无物理块"))?;
        let rid = RowId::from_parts(fid, block, row_no).map_err(|_| mismatch("行号越域"))?;
        drop(seg);

        // ② 索引条目：本表的每个键，取**行内字节**作分量。
        let view = RowView::new(&bytes).map_err(|e| mismatch(e.to_string()))?;
        let keys: Vec<KeyDef> = def.keys.to_vec();
        for k in keys {
            let mut comps: Vec<Option<&[u8]>> = Vec::with_capacity(k.cols.len());
            for &col in k.cols {
                if view.is_null(col - 1) {
                    comps.push(None);
                    continue;
                }
                comps.push(Some(
                    view.var_column(col as usize - 1, 0)
                        .ok_or_else(|| mismatch("键列缺字节"))?,
                ));
            }
            let encoded = key::encode(&comps);
            self.insert_entry(k.name, &encoded, rid)?;
        }
        Ok(rid)
    }

    /// 往索引里插一条（开树 → 插 → 树头写回；建区期量小，逐条开合可接受）。
    fn insert_entry(&mut self, index: &str, encoded: &[u8], rid: RowId) -> Result<(), OpenError> {
        let (block, _, _) = self
            .cat
            .indexes
            .get(index)
            .ok_or_else(|| OpenError::NoSuchObject(index.to_owned()))?;
        let block = *block;
        let fid = self.cat.file.file_id();
        let mut seg = Segment::open(&mut self.cat.file, block)?;
        let ws = self.cat.ws;
        let root0 = segment::read_tree_head(&seg.read_page(0)?)?;
        let root = {
            let mut store = SegmentStore::new(&mut seg, ws);
            let mut tree = Tree::open(&mut store, fid, root0)?;
            tree.insert(encoded, rid)?;
            tree.root()
        };
        let mut header = seg.read_page(0)?;
        segment::write_tree_head(&mut header, root)?;
        seg.write_page(0, &mut header)?;
        Ok(())
    }
}

/// 分配一张新的字典数据页（段内逻辑页；必要时扩段）。
fn new_dict_page(
    seg: &mut Segment<'_, '_>,
    ws: [u8; 8],
    fid: u16,
) -> Result<(u32, Page), OpenError> {
    let logical = seg.allocate_append_page()?;
    if seg.logical_block(logical).is_none() {
        seg.extend()?;
    }
    let block = seg
        .logical_block(logical)
        .ok_or_else(|| mismatch("新页无物理块"))?;
    Ok((logical, Page::new(PageType::HeapTable, ws, fid, block)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bicdb_storage::bitmap::{FileLayout, META_ROLE};
    use bicdb_workspace::io::MemFileIo;

    const WS: [u8; 8] = [7u8; 8];

    /// 建一个工作区的 file 0：建自举集 → 种子自洽字典 → 重开。
    fn seeded<'a>(io: &'a MemFileIo, path: &str, is_public: bool) -> Catalog<'a> {
        let layout = FileLayout::meta();
        let mut file = DataFile::create(
            io,
            Path::new(path),
            0,
            META_ROLE,
            WS,
            layout.min_file_blocks() + 512,
        )
        .unwrap();
        let built = crate::create::create_dictionary(&mut file, WS, is_public).unwrap();
        let mut cat = Catalog::from_entries(file, built.entries.clone()).unwrap();
        let rows = cat.seed_own_dictionary(&built).unwrap();
        assert!(rows > 0);
        drop(cat);
        Catalog::open(io, Path::new(path)).unwrap()
    }

    #[test]
    fn open_chain_maps_every_bootstrap_entry() {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let cat = seeded(&io, "/mem/o1.dat", false);
        assert!(!cat.is_public());
        assert_eq!(cat.entries().len(), 15);
        let names = cat.table_names();
        assert_eq!(names.len(), 7, "7 张自举表");
        assert!(names.contains(&"obj$") && names.contains(&"undo$"));
        // 表/索引定义可查。
        assert_eq!(cat.table_def("obj$").unwrap().keys.len(), 2);
        let (t, k) = cat.key_def("i_obj_name").unwrap();
        assert_eq!(t, "obj$");
        assert_eq!(k.cols, [3, 2]);
    }

    #[test]
    fn seeded_dictionary_describes_itself() {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let mut cat = seeded(&io, "/mem/o2.dat", false);
        // obj$：7 表 + 8 索引 = 15 行，全部 status=1、obj# 1..15。
        let objs = cat.scan("obj$").unwrap();
        assert_eq!(objs.len(), 15, "自举集 15 个对象各一行");
        let mut ids: Vec<u64> = Vec::new();
        for (_, row) in &objs {
            ids.push(match row[0] {
                DictValue::Num(n) => n,
                ref other => panic!("obj# 应为数值：{other:?}"),
            });
            assert_eq!(row[7], DictValue::Num(1), "status 有效");
        }
        ids.sort_unstable();
        assert_eq!(ids, (1..=15).collect::<Vec<u64>>());
        // col$：列定义齐备（每张表的每列一行）。
        let cols = cat.scan("col$").unwrap();
        let expected: usize = dict::bootstrap_plan(false)
            .iter()
            .filter(|(_, k)| k.is_none())
            .map(|(t, _)| t.columns.len())
            .sum();
        assert_eq!(cols.len(), expected, "col$ 行数 = 7 张表的列数合计");
        // seg$：每对象一行，段头块都在数据区。
        let segs = cat.scan("seg$").unwrap();
        assert_eq!(segs.len(), 15);
        let d0 = u64::from(FileLayout::meta().data_area_first_block);
        for (_, row) in &segs {
            match row[2] {
                DictValue::Num(b) => assert!(b >= d0, "段头块 {b} 应在数据区"),
                ref other => panic!("block_id 应为数值：{other:?}"),
            }
        }
        // tab$/ind$/icol$ 也有行。
        assert_eq!(cat.scan("tab$").unwrap().len(), 7);
        assert_eq!(cat.scan("ind$").unwrap().len(), 8);
        let icol_rows: usize = dict::bootstrap_plan(false)
            .iter()
            .filter_map(|(_, k)| k.as_ref())
            .map(|k| k.cols.len())
            .sum();
        assert_eq!(
            cat.scan("icol$").unwrap().len(),
            icol_rows,
            "icol$ 行数 = 8 个键的列数合计（复合键 2 行）"
        );
    }

    #[test]
    fn index_lookup_finds_object_rows() {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let mut cat = seeded(&io, "/mem/o3.dat", false);
        // 按名查 obj$（i_obj_name = (namespace, name)）：表命名空间 + "obj$"。
        let ns = comp_num(u64::from(dict::namespace::TABLE));
        let name = comp_text("obj$");
        let hit = cat
            .lookup("i_obj_name", &[Some(&ns), Some(&name)])
            .unwrap()
            .expect("obj$ 应可查到");
        assert_eq!(hit.1[1], DictValue::Text("obj$".to_owned()));
        assert_eq!(hit.1[3], DictValue::Num(u64::from(dict::obj_kind::TABLE)));
        // 按号查（i_obj_pk）。
        let one = comp_num(1);
        let by_pk = cat.lookup("i_obj_pk", &[Some(&one)]).unwrap().unwrap();
        assert_eq!(by_pk.1[1], DictValue::Text("obj$".to_owned()));
        // 不存在的名字 ⇒ None（不区分"没有"与"别人的"）。
        let missing = comp_text("nope");
        assert!(cat
            .lookup("i_obj_name", &[Some(&ns), Some(&missing)])
            .unwrap()
            .is_none());
        // 索引命名空间下的 i_obj_name 也能查到（同表两个键同装）。
        let ins = comp_num(u64::from(dict::namespace::INDEX));
        assert!(
            cat.lookup("i_obj_name", &[Some(&ins), Some(&name)])
                .unwrap()
                .is_none(),
            "索引命名空间下没有名为 obj$ 的对象"
        );
    }

    #[test]
    fn fetch_by_rowid_round_trips_and_scan_index_lists_keys() {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let mut cat = seeded(&io, "/mem/o4.dat", false);
        let rows = cat.scan("obj$").unwrap();
        let (rid, row) = rows[0].clone();
        assert_eq!(
            cat.fetch("obj$", rid).unwrap(),
            row,
            "按 ROWID 取行 == 扫描行"
        );
        // 索引全扫：i_obj_pk 的 15 条，键升序（1..15 的保序编码）。
        let entries = cat.scan_index("i_obj_pk").unwrap();
        assert_eq!(entries.len(), 15);
        let mut prev: Option<Vec<u8>> = None;
        for (comps, _) in &entries {
            let k = comps[0].clone().expect("键非 NULL");
            if let Some(p) = &prev {
                assert!(p < &k, "键升序（保序编码）");
            }
            prev = Some(k);
        }
    }

    #[test]
    fn public_workspace_seeds_23_entries_and_its_own_tables() {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let mut cat = seeded(&io, "/mem/o5.dat", true);
        assert!(cat.is_public());
        assert_eq!(cat.entries().len(), 23);
        // public 的三张表也在自举集里（空表——尚无主体/工作区/文件系统登记）。
        assert!(cat.table_names().contains(&"user$"));
        assert!(cat.table_names().contains(&"ws$"));
        assert!(cat.table_names().contains(&"fs$"));
        assert!(cat.scan("ws$").unwrap().is_empty());
        // obj$ = 23 个对象。
        assert_eq!(cat.scan("obj$").unwrap().len(), 23);
    }

    #[test]
    fn reopen_after_close_sees_the_seeded_rows() {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let mut before = seeded(&io, "/mem/o6.dat", false);
        let n = before.scan("obj$").unwrap().len();
        drop(before);
        let mut after = Catalog::open(&io, Path::new("/mem/o6.dat")).unwrap();
        assert_eq!(after.scan("obj$").unwrap().len(), n, "关盘重开：行仍在");
        let ns = comp_num(u64::from(dict::namespace::TABLE));
        let name = comp_text("col$");
        assert!(
            after
                .lookup("i_obj_name", &[Some(&ns), Some(&name)])
                .unwrap()
                .is_some(),
            "重开后索引查询仍可用"
        );
    }
}
