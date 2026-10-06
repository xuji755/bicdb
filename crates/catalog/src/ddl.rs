//! **DDL 写侧**（`目录详设` §5.1–§5.4；C4）。
//!
//! ```text
//! 一个 DDL 事务（独立；活动事务中发 DDL ⇒ 拒绝——TXN REQ-TXN-016 由会话层判）：
//! ① 名字检查（保留名 / 已存在——**唯一性的真正闸门是 `i_obj_name` 唯一索引**）
//! ② 分配 obj#（`seq$` 的 object_id 序列；用户对象 ≥ 100）
//! ③ 建段（**经 redo**：`bicdb_access::create_segment`）——段头块即 `seg$.block_id`
//! ④ 写字典行（obj$/tab$/col$/ind$/icol$/seg$）——**经表访问服务**（ITL/锁/
//!    undo/redo），字典表的索引同步维护，并**写穿**行缓存
//! ⑤ **预约提交序号**，所有行的 `mtime = seq`
//! ⑥ 提交（用预约号）→ 登记精确失效 + 世代自增
//! ```
//!
//! **两条纪律**（`目录详设` §1）：
//! 1. **字典表 = 普通表**——读写全部经表引擎 + 事务 + redo，不发明第二套机制；
//! 2. **名字唯一性靠 `i_obj_name` 唯一索引**（不是"先查后插"）——并发同名
//!    CREATE 的正确性由唯一索引的插入冲突兜底（① 的检查只是快速失败）。
//!
//! **对象号口径（本模块冻结，随 C4 记档）**：
//! ```text
//! 1..N       自举集（N = 15 普通 / 23 public；建区期连号，见 create.rs）
//! N+1, N+2   stat$ / seq$（**保留常量**——建它们时 seq$ 还不存在）
//! ≥ 100      用户对象（`seq$` 的 object_id 序列分配；obj_kind::USER_FIRST）
//! ```
//!
//! **崩溃语义**：段先建（"无人引用的空闲段"是可容忍的中间态——区仍标记已分配，
//! 回收随 §5.4 的段回收路径）；字典行与段登记在**同一事务**里提交 ⇒ 崩溃后
//! 要么全在（可解析）、要么全不在（段成垃圾，不破坏已提交数据）。

use bicdb_access::heap::TableAccess;
use bicdb_access::{create_segment, index as acc_index};
use bicdb_common::seq::CommitSeq;
use bicdb_storage::buffer::BufferPool;
use bicdb_storage::heap::InsertPolicy;
use bicdb_storage::rowid::RowId;
use bicdb_storage::segment::SegType;
use bicdb_storage::undo::UndoChain;
use bicdb_txn::engine::Engine;
use bicdb_txn::write::Txn;
use bicdb_wal::group::GroupWriter;

use crate::cache::{ColRow, IcolRow, IndRow, ObjRow, SegRow, TabRow};
use crate::dict::{self, ColTypeCode, DictTable};
use crate::open::{Catalog, OpenError};
use crate::row::{self, DictValue, RowCodecError};

/// DDL 错误。
#[derive(Debug)]
pub enum DdlError {
    /// 目录只读面/打开链。
    Catalog(OpenError),
    /// 事务引擎（内层写路径）。
    Txn(bicdb_txn::write::TxnError),
    /// 表访问/索引维护。
    Access(bicdb_access::TableAccessError),
    /// 行编解码。
    Row(RowCodecError),
    /// 索引层（如唯一冲突）。
    Index(bicdb_index::IndexError),
    /// 名字非法（保留名：`$` 结尾或预置名）。
    ReservedName(String),
    /// 对象已存在（`(namespace, name)` 命中）。
    AlreadyExists(String),
    /// 对象不存在（DROP 的目标）。
    NotFound(String),
    /// 表定义非法（无列/列重名/列名非法）。
    BadTableDef(String),
    /// 索引定义非法（键列不存在/无键列）。
    BadIndexDef(String),
    /// 对象种类不符（对索引执行表的操作，反之亦然）。
    WrongKind(String),
    /// 缓存行形态（字典行与内核常量不符——响亮）。
    Cache(crate::cache::CacheError),
}

impl std::fmt::Display for DdlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DdlError::Catalog(e) => write!(f, "DDL·目录：{e}"),
            DdlError::Txn(e) => write!(f, "DDL·事务：{e}"),
            DdlError::Access(e) => write!(f, "DDL·表访问：{e}"),
            DdlError::Row(e) => write!(f, "DDL·行：{e}"),
            DdlError::Index(e) => write!(f, "DDL·索引：{e}"),
            DdlError::ReservedName(n) => write!(f, "名字 `{n}` 是保留名（`$` 结尾或预置名）"),
            DdlError::AlreadyExists(n) => write!(f, "对象 `{n}` 已存在"),
            DdlError::NotFound(n) => write!(f, "对象 `{n}` 不存在"),
            DdlError::BadTableDef(why) => write!(f, "表定义非法：{why}"),
            DdlError::BadIndexDef(why) => write!(f, "索引定义非法：{why}"),
            DdlError::WrongKind(n) => write!(f, "对象 `{n}` 的种类与操作不符"),
            DdlError::Cache(e) => write!(f, "DDL·缓存行：{e}"),
        }
    }
}

impl std::error::Error for DdlError {}

macro_rules! from_err {
    ($($v:ident <- $t:ty),* $(,)?) => {
        $(impl From<$t> for DdlError { fn from(e: $t) -> Self { Self::$v(e) } })*
    };
}
from_err!(Catalog <- OpenError, Row <- RowCodecError);

impl From<bicdb_access::TableAccessError> for DdlError {
    fn from(e: bicdb_access::TableAccessError) -> Self {
        // **索引层错误保真**（唯一冲突等要原样上抛给调用方/用户）。
        match e {
            bicdb_access::TableAccessError::Index(ie) => Self::Index(ie),
            other => Self::Access(other),
        }
    }
}

/// **一个列的定义**（`CREATE TABLE` 的输入形态；与 `col$` 行一一对应）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnSpec {
    /// 列名（未引号标识符已折叠小写）。
    pub name: String,
    /// 类型码。
    pub type_code: ColTypeCode,
    /// 声明长度（`VARCHAR2`/`BYTES`；其余 0）。
    pub length: u32,
    /// 精度（`NUMBER`）。
    pub precision: Option<u32>,
    /// 标度（`NUMBER`）。
    pub scale: Option<u32>,
    /// 可空。
    pub nullable: bool,
}

/// **表选项**（七项；`arch/03` §3.1.2 的默认值即 `Default`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableOptions {
    /// 页内预留百分比。
    pub pctfree: u8,
    /// ITL 槽上限。
    pub itl_max: u8,
    /// 事务性。
    pub transactional: bool,
    /// 日志模式。
    pub logging: u32,
    /// 更新模式。
    pub update_mode: u32,
    /// 保留策略。
    pub retention: u32,
    /// 版本保留量。
    pub version_keep: u32,
    /// 段清理策略。
    pub cleanup: u32,
    /// 嵌入策略。
    pub embed: u32,
    /// 公共数据表。
    pub shared: bool,
}

impl Default for TableOptions {
    fn default() -> Self {
        Self {
            pctfree: 0,
            itl_max: 8,
            transactional: true,
            logging: dict::table_opt::LOGGING_FULL,
            update_mode: dict::table_opt::UPDATE_IN_PLACE,
            retention: 0,
            version_keep: 0,
            cleanup: dict::table_opt::CLEANUP_NONE,
            embed: dict::table_opt::EMBED_NONE,
            shared: false,
        }
    }
}

/// **建表规格**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableSpec {
    /// 表名。
    pub name: String,
    /// 列（1 起连号，顺序即 `col#`）。
    pub columns: Vec<ColumnSpec>,
    /// 表选项。
    pub options: TableOptions,
}

/// **建索引规格**（V1.0：普通列键；表达式键随 C4+）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexSpec {
    /// 索引名。
    pub name: String,
    /// 基表名。
    pub table: String,
    /// 唯一。
    pub unique: bool,
    /// 键列名（按序）。
    pub columns: Vec<String>,
}

/// 建表结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateTableOutcome {
    /// 对象号。
    pub obj: u32,
    /// 数据对象号。
    pub dataobj: u32,
    /// 段头块。
    pub seg_block: u32,
    /// 提交序号（= 各行的 `mtime`）。
    pub commit_seq: u64,
}

/// 建索引结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateIndexOutcome {
    /// 索引对象号。
    pub obj: u32,
    /// 数据对象号。
    pub dataobj: u32,
    /// 段头块。
    pub seg_block: u32,
    /// 灌入条目数。
    pub entries: usize,
    /// 叶页数。
    pub leaf_blocks: usize,
    /// 枝/根页数。
    pub branch_blocks: usize,
    /// 提交序号。
    pub commit_seq: u64,
}

/// 删除结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropOutcome {
    /// 删除的对象号。
    pub obj: u32,
    /// 级联删除的索引对象号（删表时非空）。
    pub indexes: Vec<u32>,
}

// ───────────────────────────── 对象号 ─────────────────────────────

/// `stat$` 的**保留对象号**（自举集之后第一个；建它时 `seq$` 还不存在）。
#[must_use]
pub fn stat_obj_number(is_public: bool) -> u32 {
    let n = if is_public {
        dict::bootstrap_entries_public()
    } else {
        dict::bootstrap_entries_normal()
    };
    n as u32 + 1
}

/// `seq$` 的**保留对象号**。
#[must_use]
pub fn seq_obj_number(is_public: bool) -> u32 {
    stat_obj_number(is_public) + 1
}

/// `i_stat_pk` 的保留对象号。
#[must_use]
pub fn stat_index_obj_number(is_public: bool) -> u32 {
    stat_obj_number(is_public) + 2
}

/// `i_seq_pk` 的保留对象号。
#[must_use]
pub fn seq_index_obj_number(is_public: bool) -> u32 {
    stat_obj_number(is_public) + 3
}

/// `object_id` 序列在 `seq$` 里的 `seq#`。
pub const OBJECT_ID_SEQ: u64 = 1;

// ───────────────────────────── 字典行写口 ─────────────────────────────

/// **一个 DDL 事务里的字典行写口**（`目录详设` §5.2 的 ③④⑤⑥）。
///
/// 只在 [`with_ddl_txn`] 的闭包里构造——它借的是引擎的三把内部锁 +
/// 内层事务句柄（`TxnEngine::with_write_context`）。
struct DictWriter<'a, 'b, 'io, 'lio, 'lf> {
    cat: &'a mut Catalog<'io>,
    pool: &'a BufferPool<'b>,
    log: &'a mut GroupWriter<'lio, 'lf>,
    chain: &'a mut UndoChain<'lio, 'lf>,
    txn: &'a mut Txn,
    /// 预约的提交序号（写进各行的 `mtime`；也是缓存装载戳）。
    seq: CommitSeq,
    /// 本事务改过的对象（提交后登记精确失效）。
    changed: Vec<u32>,
}

impl<'a, 'b, 'io, 'lio, 'lf> DictWriter<'a, 'b, 'io, 'lio, 'lf> {
    fn ws(&self) -> [u8; 8] {
        self.cat.ws()
    }

    /// 表定义（**内核常量**：字典表才有）+ 段头块（**活路径**：obj$ → seg$）。
    fn table(&mut self, name: &str) -> Result<(&'static DictTable, u32), DdlError> {
        let def = dict::DICT_TABLES
            .iter()
            .find(|t| t.name == name)
            .ok_or_else(|| DdlError::NotFound(name.to_owned()))?;
        let block = live_segment_block_by_name(self.cat, name, dict::namespace::TABLE)?;
        Ok((def, block))
    }

    /// **写一行字典行**（行写经表访问服务 + 该表各索引同步维护 + 缓存写穿）。
    fn insert_row(&mut self, table: &str, values: &[DictValue]) -> Result<RowId, DdlError> {
        let (def, seg_block) = self.table(table)?;
        let bytes = row::encode(values, def.columns)?;
        let policy = InsertPolicy::in_place(0);
        let rid = {
            let mut access = TableAccess::new(self.pool, self.ws());
            access.insert(
                self.log,
                self.chain,
                self.txn,
                self.cat.file_mut(),
                seg_block,
                &bytes,
                &policy,
            )?
        };
        // 该表的每个键：插索引项 + 树头回写（同一事务、同一日志流）。
        let ws = self.ws();
        for key_def in def.keys {
            let comps = key_components(values, def, key_def)?;
            let idx_block = live_index_block(self.cat, key_def.name)?;
            let new_root = acc_index::insert_entry(
                self.pool,
                self.log,
                self.cat.file_mut(),
                ws,
                idx_block,
                self.txn,
                &comps,
                rid,
            )?;
            acc_index::write_tree_head_redo(
                self.pool,
                self.log,
                self.cat.file_mut(),
                ws,
                idx_block,
                self.txn,
                new_root,
            )?;
        }
        Ok(rid)
    }

    /// **写穿缓存**（§4.2 纪律 4：DDL 在同一事务内改字典行时同改缓存）。
    fn write_through_obj(&mut self, values: &[DictValue]) -> Result<(), DdlError> {
        let row = ObjRow::from_values(values)?;
        self.changed.push(row.obj);
        self.cat.row_cache().put_obj(self.seq, row);
        Ok(())
    }

    fn write_through_tab(&mut self, values: &[DictValue]) -> Result<(), DdlError> {
        let row = TabRow::from_values(values)?;
        self.cat.row_cache().put_tab(self.seq, row);
        Ok(())
    }

    fn write_through_seg(&mut self, values: &[DictValue]) -> Result<(), DdlError> {
        let row = SegRow::from_values(values)?;
        self.cat.row_cache().put_seg(self.seq, row);
        Ok(())
    }
}

/// 索引键的分量字节（**与行内字节同源**——`row::component_bytes`）。
fn key_components(
    values: &[DictValue],
    def: &DictTable,
    key: &dict::KeyDef,
) -> Result<Vec<u8>, DdlError> {
    let mut comps: Vec<Option<Vec<u8>>> = Vec::with_capacity(key.cols.len());
    for col_no in key.cols {
        let col_def = def
            .columns
            .iter()
            .find(|c| c.col == *col_no)
            .ok_or_else(|| DdlError::BadIndexDef(format!("键列 {} 不在表里", col_no)))?;
        let v = values
            .get(usize::from(*col_no - 1))
            .ok_or_else(|| DdlError::BadIndexDef(format!("键列 {} 越出行值", col_no)))?;
        comps.push(row::component_bytes(v, col_def)?);
    }
    let refs: Vec<Option<&[u8]>> = comps.iter().map(|c| c.as_deref()).collect();
    Ok(bicdb_storage::key::encode(&refs))
}

// ───────────────────────────── DDL 事务外壳 ─────────────────────────────

/// **DDL 事务的公共外壳**：名字检查 → begin → 预约序号 → 闭包体 → 提交。
///
/// 失败路径：闭包体返回错误 ⇒ 回滚 + **缓存全清**（保守正确：写穿过的行随
/// 事务一起消失）。
fn with_ddl_txn<R>(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    body: impl FnOnce(&mut DictWriter<'_, '_, '_, '_, '_>) -> Result<R, DdlError>,
) -> Result<(R, u64), DdlError> {
    let mut txn = engine.begin()?;
    let seq = engine.reserve_commit_seq(&mut txn)?;
    let outcome = engine.with_write_context(&mut txn, |pool, log, chain, txn| {
        let mut w = DictWriter {
            cat,
            pool,
            log,
            chain,
            txn,
            seq,
            changed: Vec::new(),
        };
        let r = body(&mut w);
        (r, w.changed.clone())
    });
    let (result, changed) = outcome;
    match result {
        Ok(r) => {
            engine.commit(&mut txn)?;
            // 提交后：登记精确失效 + 世代自增（§4.3）——写穿过的条目由下一次
            // 读按登记精确失效后回查（正确性优先；"纯插入不失效"是后续优化）。
            for obj in &changed {
                cat.row_cache().note_change_obj(*obj);
            }
            cat.advance_commit(seq);
            Ok((r, seq.as_raw()))
        }
        Err(e) => {
            let _ = engine.rollback(&mut txn);
            cat.row_cache().bump_and_clear();
            Err(e)
        }
    }
}

impl From<bicdb_txn::write::TxnError> for DdlError {
    fn from(e: bicdb_txn::write::TxnError) -> Self {
        Self::Txn(e)
    }
}

impl From<crate::cache::CacheError> for DdlError {
    fn from(e: crate::cache::CacheError) -> Self {
        Self::Cache(e)
    }
}

impl From<bicdb_index::IndexError> for DdlError {
    fn from(e: bicdb_index::IndexError) -> Self {
        Self::Index(e)
    }
}

/// **建段**（经 redo）——DDL 的段创建口。
#[allow(clippy::too_many_arguments)]
fn create_table_segment(
    cat: &mut Catalog<'_>,
    pool: &BufferPool<'_>,
    log: &mut GroupWriter<'_, '_>,
    txn: &Txn,
    seg_type: SegType,
    obj: u32,
    dataobj: u32,
    options: &TableOptions,
) -> Result<u32, DdlError> {
    let ws = cat.ws();
    let file = cat.file_mut();
    let block = create_segment(
        pool,
        log,
        txn,
        file,
        ws,
        seg_type,
        obj,
        dataobj,
        options.itl_max,
        options.pctfree,
        0,
    )?;
    Ok(block)
}

/// 键的前 32 字节十六进制（诊断用）。
fn hex_key(key: &[u8]) -> String {
    let mut s = String::with_capacity(64);
    for b in key.iter().take(32) {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

// ───────────────────────────── 名字与对象号 ─────────────────────────────

/// **用户对象名检查**（`$` 结尾 = 保留名；预置名随需求清单扩展）。
fn check_user_name(name: &str) -> Result<(), DdlError> {
    if name.is_empty() {
        return Err(DdlError::BadTableDef("名字为空".to_owned()));
    }
    if name.ends_with('$') {
        return Err(DdlError::ReservedName(name.to_owned()));
    }
    Ok(())
}

/// **分配对象号**（C5：**内存取号 + 成批刷入**，跳号无害）。
///
/// `reserved` 给定时用保留常量（建 `stat$`/`seq$` 用——此刻序列还不存在）。
/// 否则从 `object_id` 序列取：
///
/// ```text
/// 内存批 [next, high)：next < high ⇒ 直接发 next++（零 I/O）
///                    否则         ⇒ 读 seq$.next_val，high = next_val + cache，
///                                   把 next_val = high 刷回（一次行更新 = 一批的代价）
/// 崩溃：未发的批丢失（号只前移、不重复）——**跳号无害**（设计原话）
/// ```
fn allocate_obj_number(
    w: &mut DictWriter<'_, '_, '_, '_, '_>,
    reserved: Option<u32>,
) -> Result<u32, DdlError> {
    if let Some(n) = reserved {
        return Ok(n);
    }
    // 内存批命中 ⇒ 直接发号。
    if let Some((next, high)) = w.cat.obj_seq.get() {
        if next < high {
            w.cat.obj_seq.set(Some((next + 1, high)));
            return u32::try_from(next)
                .map_err(|_| DdlError::BadTableDef("object_id 越出 u32".to_owned()));
        }
    }
    // 取新批：读 seq$ 行（键点查）+ 刷回 next_val = 批上界。
    let (seq_def, seq_block) = w.table("seq$").map_err(|_| {
        DdlError::BadTableDef("seq$ 未初始化（先跑 init_dictionary_tables）".to_owned())
    })?;
    let key_val = DictValue::Num(OBJECT_ID_SEQ);
    let col1 = seq_def
        .columns
        .first()
        .ok_or_else(|| DdlError::BadTableDef("seq$ 无列".to_owned()))?;
    let comp = row::component_bytes(&key_val, col1)?
        .ok_or_else(|| DdlError::BadTableDef("seq# 不可为 NULL".to_owned()))?;
    let hit = w.cat.lookup("i_seq_pk", &[Some(&comp)])?;
    let (rid, mut values) = hit.ok_or_else(|| {
        DdlError::BadTableDef("seq$ 缺 object_id 序列行（建区收尾未跑全）".to_owned())
    })?;
    let next = match values.get(2) {
        Some(DictValue::Num(n)) => *n,
        _ => return Err(DdlError::BadTableDef("seq$.next_val 形态非法".to_owned())),
    };
    let cache = match values.get(3) {
        Some(DictValue::Num(n)) if *n > 0 => *n,
        _ => 1, // cache 列缺省/为 0 ⇒ 逐号刷入（保守）
    };
    if next < u64::from(dict::obj_kind::USER_FIRST) {
        return Err(DdlError::BadTableDef(format!(
            "object_id {next} 低于用户对象下界 {}",
            dict::obj_kind::USER_FIRST
        )));
    }
    let high = next.saturating_add(cache);
    values[2] = DictValue::Num(high);
    w.update_row_nonkey("seq$", seq_block, seq_def, rid, &values)?;
    w.cat.obj_seq.set(Some((next + 1, high)));
    u32::try_from(next).map_err(|_| DdlError::BadTableDef("object_id 越出 u32".to_owned()))
}

impl<'a, 'b, 'io, 'lio, 'lf> DictWriter<'a, 'b, 'io, 'lio, 'lf> {
    /// **只改非键列的更新**：键列必须与旧行一致（否则索引会与行失配——
    /// 具名拒绝）。DDL 里只有 `seq$.next_val` 这类"计数器"走它。
    fn update_row_nonkey(
        &mut self,
        table: &str,
        seg_block: u32,
        def: &'static DictTable,
        rid: RowId,
        values: &[DictValue],
    ) -> Result<(), DdlError> {
        let old = self.cat.fetch(table, rid)?;
        for key in def.keys {
            let old_comps: Vec<Option<Vec<u8>>> = key
                .cols
                .iter()
                .map(|c| {
                    row::component_bytes(
                        &old[usize::from(*c - 1)],
                        &def.columns[usize::from(*c - 1)],
                    )
                })
                .collect::<Result<_, _>>()?;
            let new_comps: Vec<Option<Vec<u8>>> = key
                .cols
                .iter()
                .map(|c| {
                    row::component_bytes(
                        &values[usize::from(*c - 1)],
                        &def.columns[usize::from(*c - 1)],
                    )
                })
                .collect::<Result<_, _>>()?;
            if old_comps != new_comps {
                return Err(DdlError::BadTableDef(format!(
                    "更新键列（{table} 的 {}）——DDL 不做键变更（那是「重建索引」路径）",
                    key.name
                )));
            }
        }
        let bytes = row::encode(values, def.columns)?;
        let policy = InsertPolicy::in_place(0);
        // **跟随转发指针**：迁移过的行，落点在别处（ROWID 稳定）。
        let rid = self.cat.resolve_rid(rid)?;
        let mut access = TableAccess::new(self.pool, self.ws());
        access.update(
            self.log,
            self.chain,
            self.txn,
            self.cat.file_mut(),
            seg_block,
            rid,
            &bytes,
            &policy,
        )?;
        Ok(())
    }

    /// **删除一行**（含其索引项）：先按旧行值删索引项，再删堆行。
    fn delete_row(
        &mut self,
        table: &str,
        def: &'static DictTable,
        rid: RowId,
    ) -> Result<(), DdlError> {
        let values = self.cat.fetch(table, rid)?;
        let ws = self.ws();
        // 索引项按**稳定 ROWID**（= 索引里存的那个）删；堆行删**落点**。
        let landed = self.cat.resolve_rid(rid)?;
        for key_def in def.keys {
            let comps = key_components(&values, def, key_def)?;
            let idx_block = live_index_block(self.cat, key_def.name)?;
            let new_root = acc_index::delete_entry(
                self.pool,
                self.log,
                self.cat.file_mut(),
                ws,
                idx_block,
                self.txn,
                &comps,
                rid,
            )?;
            acc_index::write_tree_head_redo(
                self.pool,
                self.log,
                self.cat.file_mut(),
                ws,
                idx_block,
                self.txn,
                new_root,
            )?;
        }
        let mut access = TableAccess::new(self.pool, self.ws());
        access.delete(self.log, self.chain, self.txn, self.cat.file_mut(), landed)?;
        Ok(())
    }

    /// **表/索引对象的字典行写入**（obj$ 一行；namespace/type# 由调用方给）。
    fn insert_obj(&mut self, obj: u32, name: &str, ns: u32, kind: u32) -> Result<(), DdlError> {
        let values = vec![
            DictValue::Num(u64::from(obj)),
            DictValue::Text(name.to_owned()),
            DictValue::Num(u64::from(ns)),
            DictValue::Num(u64::from(kind)),
            DictValue::Num(u64::from(obj)), // dataobj# = obj#（本库口径）
            DictValue::Num(self.seq.as_raw()),
            DictValue::Num(self.seq.as_raw()), // mtime = 预约的提交序号
            DictValue::Num(1),                 // status = 有效
        ];
        self.insert_row("obj$", &values)?;
        self.write_through_obj(&values)
    }

    /// `tab$` 一行（表选项七项 + 列数）。
    fn insert_tab(&mut self, obj: u32, cols: u32, o: &TableOptions) -> Result<(), DdlError> {
        let values = vec![
            DictValue::Num(u64::from(obj)),
            DictValue::Num(u64::from(cols)),
            DictValue::Num(u64::from(o.pctfree)),
            DictValue::Num(u64::from(o.itl_max)),
            DictValue::Bool(o.transactional),
            DictValue::Num(u64::from(o.logging)),
            DictValue::Num(u64::from(o.update_mode)),
            DictValue::Num(u64::from(o.retention)),
            DictValue::Num(u64::from(o.version_keep)),
            DictValue::Num(u64::from(o.cleanup)),
            DictValue::Num(u64::from(o.embed)),
            DictValue::Bool(o.shared),
        ];
        self.insert_row("tab$", &values)?;
        self.write_through_tab(&values)
    }

    /// `col$` 每列一行 + 一次成组写穿。
    fn insert_cols(&mut self, obj: u32, columns: &[ColumnSpec]) -> Result<(), DdlError> {
        let mut rows = Vec::with_capacity(columns.len());
        for (i, c) in columns.iter().enumerate() {
            let opt = |x: Option<u32>| x.map_or(DictValue::Null, |n| DictValue::Num(u64::from(n)));
            let values = vec![
                DictValue::Num(u64::from(obj)),
                DictValue::Num(i as u64 + 1),
                DictValue::Text(c.name.clone()),
                DictValue::Num(u64::from(c.type_code as u8)),
                DictValue::Num(u64::from(c.length)),
                opt(c.precision),
                opt(c.scale),
                DictValue::Bool(c.nullable),
                DictValue::Null,   // deflt（默认值随 DDL 扩展）
                DictValue::Num(0), // flags
            ];
            self.insert_row("col$", &values)?;
            rows.push(ColRow::from_values(&values)?);
        }
        self.cat.row_cache().put_cols(self.seq, obj, rows);
        Ok(())
    }

    /// **`stat$` 一行**（统计；**唯一一张非事务表**——`logging = redo_only`，
    /// 可丢：写失败不阻断 DDL 的语义，但本实现仍走同一事务，简单且自洽）。
    fn insert_stat(&mut self, obj: u32, row_est: u64) -> Result<(), DdlError> {
        let values = vec![
            DictValue::Num(u64::from(obj)),
            DictValue::Num(row_est),
            DictValue::Num(0),                 // access_cnt
            DictValue::Null,                   // last_access
            DictValue::Num(self.seq.as_raw()), // updated_at
        ];
        self.insert_row("stat$", &values).map(|_| ())
    }

    /// `seg$` 一行（段头位置）。
    fn insert_seg(&mut self, dataobj: u32, seg_block: u32) -> Result<(), DdlError> {
        let values = vec![
            DictValue::Num(u64::from(dataobj)),
            DictValue::Num(u64::from(self.cat.file_mut().file_id())),
            DictValue::Num(u64::from(seg_block)),
            DictValue::Num(1), // iniexts
            DictValue::Num(self.seq.as_raw()),
        ];
        self.insert_row("seg$", &values)?;
        self.write_through_seg(&values)
    }

    /// `ind$` + `icol$` 行。
    fn insert_index_rows(
        &mut self,
        obj: u32,
        bobj: u32,
        unique: bool,
        col_numbers: &[u32],
    ) -> Result<(), DdlError> {
        let ind = vec![
            DictValue::Num(u64::from(obj)),
            DictValue::Num(u64::from(bobj)),
            DictValue::Num(u64::from(dict::index_kind::BTREE)),
            DictValue::Num(col_numbers.len() as u64),
            DictValue::Bool(unique),
            DictValue::Num(1), // status = 有效
            DictValue::Null,   // expr_src（表达式索引随 C4+）
        ];
        self.insert_row("ind$", &ind)?;
        self.cat
            .row_cache()
            .put_ind(self.seq, IndRow::from_values(&ind)?);
        let mut icols = Vec::with_capacity(col_numbers.len());
        for (pos, col) in col_numbers.iter().enumerate() {
            let v = vec![
                DictValue::Num(u64::from(obj)),
                DictValue::Num(pos as u64 + 1),
                DictValue::Num(u64::from(*col)),
                DictValue::Bool(false), // is_desc（V1.0 只升序）
            ];
            self.insert_row("icol$", &v)?;
            icols.push(IcolRow::from_values(&v)?);
        }
        self.cat.row_cache().put_icols(self.seq, obj, icols);
        Ok(())
    }
}

// ───────────────────────────── 建表 ─────────────────────────────

/// **`CREATE TABLE`**（`目录详设` §5.2）。
pub fn create_table(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    spec: &TableSpec,
) -> Result<CreateTableOutcome, DdlError> {
    check_user_name(&spec.name)?;
    validate_table_spec(spec)?;
    // 快速失败（真正的闸门是 `i_obj_name` 唯一索引——见模块文档）。
    if object_exists(cat, &spec.name)? {
        return Err(DdlError::AlreadyExists(spec.name.clone()));
    }
    let (outcome, seq) = with_ddl_txn(cat, engine, |w| {
        let obj = allocate_obj_number(w, None)?;
        let dataobj = obj;
        let seg_block = create_table_segment(
            w.cat,
            w.pool,
            w.log,
            w.txn,
            SegType::Heap,
            obj,
            dataobj,
            &spec.options,
        )?;
        w.insert_obj(
            obj,
            &spec.name,
            dict::namespace::TABLE,
            dict::obj_kind::TABLE,
        )?;
        w.insert_tab(obj, spec.columns.len() as u32, &spec.options)?;
        w.insert_cols(obj, &spec.columns)?;
        w.insert_seg(dataobj, seg_block)?;
        w.insert_stat(obj, 0)?; // 空表：行数估计 0（后续由统计路径刷新）
        Ok(CreateTableOutcome {
            obj,
            dataobj,
            seg_block,
            commit_seq: 0, // 提交后填
        })
    })?;
    Ok(CreateTableOutcome {
        commit_seq: seq,
        ..outcome
    })
}

/// **活对象的段头块**（`obj$` → `dataobj#` → `seg$.block_id`）。
///
/// **统一路径**：自举表（引导页权威）、`stat$`/`seq$`（DDL 建）、用户表/索引
/// 都经 `seg$` 取段头——同一份字典事实，不搞第二套映射。
pub fn live_segment_block(cat: &mut Catalog<'_>, obj: u32) -> Result<u32, DdlError> {
    let key = crate::open::comp_num(u64::from(obj));
    let obj_row = cat
        .lookup("i_obj_pk", &[Some(&key)])?
        .ok_or_else(|| DdlError::NotFound(obj.to_string()))?;
    let dataobj = match obj_row.1.get(4) {
        Some(DictValue::Num(n)) => *n,
        _ => return Err(DdlError::BadTableDef("obj$.dataobj# 形态非法".to_owned())),
    };
    let skey = crate::open::comp_num(dataobj);
    let seg_row = cat
        .lookup("i_seg_pk", &[Some(&skey)])?
        .ok_or_else(|| DdlError::NotFound(format!("seg$ 无 dataobj# {dataobj}")))?;
    match seg_row.1.get(2) {
        Some(DictValue::Num(b)) => Ok(*b as u32),
        _ => Err(DdlError::BadTableDef("seg$.block_id 形态非法".to_owned())),
    }
}

/// **按名取活对象的段头块**（`ns` = 表/索引命名空间）。
fn live_segment_block_by_name(cat: &mut Catalog<'_>, name: &str, ns: u32) -> Result<u32, DdlError> {
    if ns == dict::namespace::TABLE {
        return Ok(cat.segment_block_of(name)?);
    }
    let (obj, _kind) = object_ref_ns(cat, name, ns)?;
    live_segment_block(cat, obj)
}

/// 索引的段头块（活路径；namespace = 索引）。
fn live_index_block(cat: &mut Catalog<'_>, index: &str) -> Result<u32, DdlError> {
    live_segment_block_by_name(cat, index, dict::namespace::INDEX)
}

/// **名字存在性**（表命名空间的快速失败口——真正的闸门是唯一索引）。
fn object_exists(cat: &mut Catalog<'_>, name: &str) -> Result<bool, DdlError> {
    object_exists_ns(cat, name, dict::namespace::TABLE)
}

/// 存在性（指定命名空间）。
fn object_exists_ns(cat: &mut Catalog<'_>, name: &str, ns: u32) -> Result<bool, DdlError> {
    let ns = crate::open::comp_num(u64::from(ns));
    let nm = crate::open::comp_text(name);
    Ok(cat.lookup("i_obj_name", &[Some(&ns), Some(&nm)])?.is_some())
}

/// 表规格校验（列非空、列名合法且不重复、类型长度自洽）。
fn validate_table_spec(spec: &TableSpec) -> Result<(), DdlError> {
    if spec.columns.is_empty() {
        return Err(DdlError::BadTableDef("至少一列".to_owned()));
    }
    let mut seen = std::collections::BTreeSet::new();
    for c in &spec.columns {
        if c.name.is_empty() {
            return Err(DdlError::BadTableDef("列名为空".to_owned()));
        }
        if !seen.insert(c.name.clone()) {
            return Err(DdlError::BadTableDef(format!("列名 `{}` 重复", c.name)));
        }
        if matches!(c.type_code, ColTypeCode::Varchar2 | ColTypeCode::Bytes) && c.length == 0 {
            return Err(DdlError::BadTableDef(format!(
                "列 `{}` 是变长类型但长度 = 0",
                c.name
            )));
        }
    }
    Ok(())
}

// ───────────────────────────── 字典表初始化（建区收尾）─────────────────────────────

/// **初始化自举之外的字典表**（`目录详设` §5.1 ⑤）：建 `stat$` 与 `seq$`
/// （**保留对象号**——此刻序列尚不存在），并播下 `object_id` 序列
/// （`next_val = USER_FIRST`）。
///
/// 幂等：两张表都已存在 ⇒ 直接返回。
pub fn init_dictionary_tables(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
) -> Result<(), DdlError> {
    let stat_ok = object_exists(cat, "stat$")?;
    let seq_ok = object_exists(cat, "seq$")?;
    if stat_ok && seq_ok {
        return Ok(());
    }
    let is_public = cat.is_public();
    with_ddl_txn(cat, engine, |w| {
        if !stat_ok {
            create_kernel_table(
                w,
                "stat$",
                stat_obj_number(is_public),
                &[stat_index_obj_number(is_public)],
                kernel_stat_columns(),
                &stat_options(),
            )?;
        }
        if !seq_ok {
            create_kernel_table(
                w,
                "seq$",
                seq_obj_number(is_public),
                &[seq_index_obj_number(is_public)],
                kernel_seq_columns(),
                &TableOptions::default(),
            )?;
            // 播种 object_id 序列（next_val = 用户对象下界）。
            let values = vec![
                DictValue::Num(OBJECT_ID_SEQ),
                DictValue::Text("object_id".to_owned()),
                DictValue::Num(u64::from(dict::obj_kind::USER_FIRST)),
                DictValue::Num(20), // cache
                DictValue::Num(0),  // flags
            ];
            w.insert_row("seq$", &values)?;
        }
        Ok(())
    })?;
    Ok(())
}

/// `stat$` 的表选项：**唯一一张非事务表**（`logging = redo_only`——统计可丢）。
fn stat_options() -> TableOptions {
    TableOptions {
        logging: dict::table_opt::LOGGING_REDO_ONLY,
        transactional: false,
        ..TableOptions::default()
    }
}

/// `stat$` 的列（内核常量 → DDL 规格）。
fn kernel_stat_columns() -> Vec<ColumnSpec> {
    kernel_columns(
        dict::DICT_TABLES
            .iter()
            .find(|t| t.name == "stat$")
            .expect("stat$ 在常量表里"),
    )
}

/// `seq$` 的列。
fn kernel_seq_columns() -> Vec<ColumnSpec> {
    kernel_columns(
        dict::DICT_TABLES
            .iter()
            .find(|t| t.name == "seq$")
            .expect("seq$ 在常量表里"),
    )
}

/// 由内核列常量造 DDL 列规格（`col$` 的行值与常量**同源**）。
fn kernel_columns(t: &DictTable) -> Vec<ColumnSpec> {
    t.columns
        .iter()
        .map(|c| ColumnSpec {
            name: c.name.to_owned(),
            type_code: c.type_code,
            length: c.length,
            precision: None,
            scale: None,
            nullable: c.nullable,
        })
        .collect()
}

/// **建内核表**（保留对象号；名字带 `$`——内核自己不受"保留名"限制）。
fn create_kernel_table(
    w: &mut DictWriter<'_, '_, '_, '_, '_>,
    name: &str,
    reserved_obj: u32,
    // 各键的**保留对象号**（逐一给出——内核表的号是常量，**不得**按
    // `reserved_obj + i` 现推：那会与相邻的保留号撞车，v0.1 前的实测缺陷）。
    key_objs: &[u32],
    columns: Vec<ColumnSpec>,
    options: &TableOptions,
) -> Result<(), DdlError> {
    let obj = allocate_obj_number(w, Some(reserved_obj))?;
    let dataobj = obj;
    let seg_block = create_table_segment(
        w.cat,
        w.pool,
        w.log,
        w.txn,
        SegType::Heap,
        obj,
        dataobj,
        options,
    )?;
    w.insert_obj(obj, name, dict::namespace::TABLE, dict::obj_kind::TABLE)?;
    w.insert_tab(obj, columns.len() as u32, options)?;
    w.insert_cols(obj, &columns)?;
    w.insert_seg(dataobj, seg_block)?;
    // **内核表的键也是真索引**（`stat$`/`seq$` 的键要能被查找/维护）。
    let def = dict::DICT_TABLES
        .iter()
        .find(|t| t.name == name)
        .ok_or_else(|| DdlError::NotFound(name.to_owned()))?;
    if key_objs.len() != def.keys.len() {
        return Err(DdlError::BadTableDef(format!(
            "{name} 的保留键号个数 {} 与键数 {} 不符",
            key_objs.len(),
            def.keys.len()
        )));
    }
    for (key, idx_obj) in def.keys.iter().zip(key_objs) {
        let idx_obj = allocate_obj_number(w, Some(*idx_obj))?;
        let cols: Vec<u32> = key.cols.iter().map(|c| u32::from(*c)).collect();
        create_index_object(w, idx_obj, obj, key.name, &cols, key.unique)?;
    }
    // 统计行**在键索引建好之后**写（写 stat$ 行本身要维护 i_stat_pk）。
    w.insert_stat(obj, 0)?;
    Ok(())
}

/// **建一个索引对象**（段 + `obj$`/`ind$`/`icol$`/`seg$` 行）——返回段头块。
///
/// `CREATE INDEX` 与内核表（`stat$`/`seq$` 的键）共用本口；**不含**灌数据
/// （批量灌树由调用方随后做——内核表的键是空的，等第一次写行时维护）。
fn create_index_object(
    w: &mut DictWriter<'_, '_, '_, '_, '_>,
    obj: u32,
    base_obj: u32,
    name: &str,
    col_numbers: &[u32],
    unique: bool,
) -> Result<u32, DdlError> {
    let dataobj = obj;
    let seg_block = create_table_segment(
        w.cat,
        w.pool,
        w.log,
        w.txn,
        SegType::BTree,
        obj,
        dataobj,
        &TableOptions::default(),
    )?;
    // 空树：建初始叶页 + 树头（一切经 redo）。
    let ws = w.cat.ws();
    {
        let mut seg = w.cat.segment_at(seg_block)?;
        let file_id = seg.file_id();
        let root = {
            let mut io = bicdb_txn::index_io::TxnIndexIo::new(w.pool, w.log, &mut seg, w.txn);
            let mut store = bicdb_index::PoolStore::new(w.pool, &mut io, file_id, ws);
            let tree = bicdb_index::Tree::create(&mut store, file_id, ws)?;
            tree.root()
        };
        drop(seg);
        acc_index::write_tree_head_redo(
            w.pool,
            w.log,
            w.cat.file_mut(),
            ws,
            seg_block,
            w.txn,
            root,
        )?;
    }
    w.insert_obj(obj, name, dict::namespace::INDEX, dict::obj_kind::INDEX)?;
    w.insert_index_rows(obj, base_obj, unique, col_numbers)?;
    w.insert_seg(dataobj, seg_block)?;
    Ok(seg_block)
}

// ───────────────────────────── 建索引 ─────────────────────────────

/// **`CREATE INDEX`**（`目录详设` §5.3）：建段 + **扫描基表求键 + 排序 +
/// 批量灌树**（唯一索引的重复键 ⇒ 整个 DDL 事务回滚）。
///
/// **一处记档的简化**：排序在内存里做（`目录详设` 说"外部排序（temp 段）"）——
/// 触发条件 = 基表规模超过 DDL 可用内存（随执行器的 temp 段/WMM 接入换成外部
/// 排序；本库 V1.0 的目录表与用户表规模下内存足够）。
pub fn create_index(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    spec: &IndexSpec,
) -> Result<CreateIndexOutcome, DdlError> {
    check_user_name(&spec.name)?;
    if spec.columns.is_empty() {
        return Err(DdlError::BadIndexDef("至少一个键列".to_owned()));
    }
    if object_exists_ns(cat, &spec.name, dict::namespace::INDEX)? {
        return Err(DdlError::AlreadyExists(spec.name.clone()));
    }
    // 基表必须存在且是表（活路径：obj$ → 列定义）。
    let (table_obj, kind) = object_ref(cat, &spec.table)?;
    if kind != dict::obj_kind::TABLE {
        return Err(DdlError::WrongKind(spec.table.clone()));
    }
    // 列定义与快照无关（DDL 期间无并发改列）⇒ 用当前装载戳即可。
    let snap = CommitSeq::from_raw(cat.current_seq())
        .ok_or_else(|| DdlError::BadTableDef("装载戳越域".to_owned()))?;
    let base_cols = cat
        .columns(snap, table_obj)
        .map_err(|e| DdlError::BadTableDef(format!("读基表列定义：{e}")))?;
    // 键列 → 列号（按给定顺序）。
    let mut col_numbers = Vec::with_capacity(spec.columns.len());
    for cname in &spec.columns {
        let col = base_cols.iter().find(|c| c.name == *cname).ok_or_else(|| {
            DdlError::BadIndexDef(format!("键列 `{cname}` 不在表 {}", spec.table))
        })?;
        col_numbers.push(col.col);
    }
    let (scan_cols, base_seg) = scan_view(cat, table_obj)?;
    let (outcome, seq) = with_ddl_txn(cat, engine, |w| {
        create_index_inner(w, spec, table_obj, &col_numbers, scan_cols, base_seg)
    })?;
    Ok(CreateIndexOutcome {
        commit_seq: seq,
        ..outcome
    })
}

/// **基表扫描视图**：列定义（`ColDef` 形态）+ 段头块。
///
/// `col$` 是列定义的唯一事实；`ColDef.name` 是 `&'static str` ⇒ 活路径造不出来，
/// 用占位名（解码/求键只用列号与类型码）。
fn scan_view(
    cat: &mut Catalog<'_>,
    table_obj: u32,
) -> Result<(Vec<crate::dict::ColDef>, u32), DdlError> {
    let snap = CommitSeq::from_raw(cat.current_seq())
        .ok_or_else(|| DdlError::BadTableDef("装载戳越域".to_owned()))?;
    let base_cols = cat
        .columns(snap, table_obj)
        .map_err(|e| DdlError::BadTableDef(format!("读基表列定义：{e}")))?;
    let mut scan_cols: Vec<crate::dict::ColDef> = Vec::with_capacity(base_cols.len());
    for c in &base_cols {
        let type_code = crate::dict::ColTypeCode::from_u8(c.type_code as u8)
            .ok_or_else(|| DdlError::BadTableDef("列类型码不认识".to_owned()))?;
        scan_cols.push(crate::dict::ColDef {
            col: c.col as u16,
            name: "<live>",
            type_code,
            length: c.length,
            nullable: c.nullable,
        });
    }
    let base_seg = live_segment_block(cat, table_obj)?;
    Ok((scan_cols, base_seg))
}

/// **建索引的内层**（同一个 DDL 事务里做事；`create_index` 与 `rebuild_index` 共用）。
#[allow(clippy::too_many_arguments)]
fn create_index_inner(
    w: &mut DictWriter<'_, '_, '_, '_, '_>,
    spec: &IndexSpec,
    table_obj: u32,
    col_numbers: &[u32],
    scan_cols: Vec<crate::dict::ColDef>,
    base_seg: u32,
) -> Result<CreateIndexOutcome, DdlError> {
    let obj = allocate_obj_number(w, None)?;
    // ① 建索引对象（段 + 空树 + obj$/ind$/icol$/seg$ 行）。
    let seg_block = create_index_object(w, obj, table_obj, &spec.name, col_numbers, spec.unique)?;
    // ② 扫描基表求键（**当前已提交状态**——单写者下池即真值）。
    let mut entries = Vec::new();
    {
        let hwm = w.cat.segment_at(base_seg)?.hwm();
        let blocks: Vec<u32> = w.cat.segment_at(base_seg)?.data_blocks(hwm);
        for b in blocks {
            let page = {
                let fid = w.cat.file_mut().file_id();
                let rdba = bicdb_storage::rowid::Rdba::from_parts(fid, b)
                    .ok_or_else(|| DdlError::BadTableDef("块号越域".to_owned()))?;
                let key = bicdb_storage::buffer::BufferKey::new(w.cat.ws(), rdba);
                match w.pool.pin(key) {
                    Ok(g) => bicdb_storage::page::Page::from_bytes(Box::new(*g.as_bytes())),
                    Err(_) => {
                        let seg = w.cat.segment_at(base_seg)?;
                        seg.read_physical_page(b)
                            .ok_or_else(|| DdlError::BadTableDef(format!("页 {b} 未格式化")))?
                    }
                }
            };
            for slot in 1..=page.slot_count() {
                let Some(bytes) = bicdb_storage::heap::row(&page, slot) else {
                    continue;
                };
                let rid = RowId::from_parts(w.cat.file_mut().file_id(), b, slot)
                    .map_err(|_| DdlError::BadTableDef("行号越域".to_owned()))?;
                let values = row::decode(bytes, &scan_cols)?;
                let mut comps: Vec<Option<Vec<u8>>> = Vec::with_capacity(col_numbers.len());
                let mut has_null = false;
                for cn in col_numbers {
                    let idx = usize::from(*cn as u16) - 1;
                    let col_def = &scan_cols[idx];
                    let c = row::component_bytes(&values[idx], col_def)?;
                    has_null |= c.is_none();
                    comps.push(c);
                }
                let refs: Vec<Option<&[u8]>> = comps.iter().map(|c| c.as_deref()).collect();
                entries.push((bicdb_storage::key::encode(&refs), rid, has_null));
            }
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.to_bytes().cmp(&b.1.to_bytes())));
    }
    // ③a **唯一性（SQL 口径：含 NULL 的键不参与）**——相邻等键在排序后即可判定；
    //     违反 ⇒ 整条 DDL 回滚（`目录详设` §5.3 的唯一性口径）。
    if spec.unique {
        for w2 in entries.windows(2) {
            let (a, b) = (&w2[0], &w2[1]);
            if !a.2 && !b.2 && a.0 == b.0 {
                return Err(DdlError::Index(bicdb_index::IndexError::DuplicateKey {
                    key: format!("{} {}", spec.name, hex_key(&a.0)),
                }));
            }
        }
    }
    // ③b 批量灌树（结构层面；唯一性已在上一步按口径判过 ⇒ `unique = false`）。
    let ws = w.cat.ws();
    let flat: Vec<(Vec<u8>, RowId)> = entries.iter().map(|(k, r, _)| (k.clone(), *r)).collect();
    let report = acc_index::build_index(
        w.pool,
        w.log,
        w.cat.file_mut(),
        ws,
        seg_block,
        w.txn,
        &flat,
        false,
    )?;
    // 索引统计：条目数（= 索引项数）作行数估计（选路的 `entries` 口径）。
    w.insert_stat(obj, report.entries as u64)?;
    Ok(CreateIndexOutcome {
        obj,
        dataobj: obj,
        seg_block,
        entries: report.entries,
        leaf_blocks: report.leaf_blocks,
        branch_blocks: report.branch_blocks,
        commit_seq: 0,
    })
}

// ───────────────────────────── 删表 / 删索引 ─────────────────────────────

/// **`DROP TABLE`**（`目录详设` §5.4）：级联删索引 → 删字典行。
///
/// **一处记档的简化**：段回收（区归还 LMT）随"段回收"切片——当前 DROP 后
/// 段空间成"已分配但无人引用"（不破坏已提交数据；回收需要位图回收口）。
pub fn drop_table(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    name: &str,
) -> Result<DropOutcome, DdlError> {
    let obj = object_number(cat, name)?;
    let kind = object_kind(cat, name)?;
    if kind != dict::obj_kind::TABLE {
        return Err(DdlError::WrongKind(name.to_owned()));
    }
    let index_objs = index_objects_of(cat, obj)?;
    for &i in &index_objs {
        let iname = object_name(cat, i)?;
        check_kind_or(cat, &iname, dict::obj_kind::INDEX)?;
    }
    let (outcome, _seq) = with_ddl_txn(cat, engine, |w| {
        for &i in &index_objs {
            let iname = object_name(w.cat, i)?;
            drop_index_rows(w, i, &iname)?;
        }
        drop_table_rows(w, obj, name)?;
        Ok(DropOutcome {
            obj,
            indexes: index_objs,
        })
    })?;
    Ok(outcome)
}

/// **`DROP INDEX`**。
pub fn drop_index(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    name: &str,
) -> Result<DropOutcome, DdlError> {
    let obj = object_number_ns(cat, name, dict::namespace::INDEX)?;
    let kind = object_kind_ns(cat, name, dict::namespace::INDEX)?;
    if kind != dict::obj_kind::INDEX {
        return Err(DdlError::WrongKind(name.to_owned()));
    }
    let (outcome, _seq) = with_ddl_txn(cat, engine, |w| {
        drop_index_rows(w, obj, name)?;
        Ok(DropOutcome {
            obj,
            indexes: vec![obj],
        })
    })?;
    Ok(outcome)
}

/// **按名取（对象号, 类型码）**（表命名空间）。
fn object_ref(cat: &mut Catalog<'_>, name: &str) -> Result<(u32, u32), DdlError> {
    object_ref_ns(cat, name, dict::namespace::TABLE)
}

/// **按名取（对象号, 类型码）**（活路径；`ns` = 表/索引命名空间）。
fn object_ref_ns(cat: &mut Catalog<'_>, name: &str, ns: u32) -> Result<(u32, u32), DdlError> {
    let ns = crate::open::comp_num(u64::from(ns));
    let nm = crate::open::comp_text(name);
    let hit = cat
        .lookup("i_obj_name", &[Some(&ns), Some(&nm)])?
        .ok_or_else(|| DdlError::NotFound(name.to_owned()))?;
    let obj = match hit.1.first() {
        Some(DictValue::Num(n)) => *n as u32,
        _ => return Err(DdlError::BadTableDef("obj$.obj# 形态非法".to_owned())),
    };
    let kind = match hit.1.get(3) {
        Some(DictValue::Num(t)) => *t as u32,
        _ => return Err(DdlError::BadTableDef("obj$.type# 形态非法".to_owned())),
    };
    Ok((obj, kind))
}

/// 对象号（按名解析；不存在 ⇒ `NotFound`）。
fn object_number(cat: &mut Catalog<'_>, name: &str) -> Result<u32, DdlError> {
    object_number_ns(cat, name, dict::namespace::TABLE)
}

/// 对象号（指定命名空间）。
fn object_number_ns(cat: &mut Catalog<'_>, name: &str, ns_val: u32) -> Result<u32, DdlError> {
    let ns = crate::open::comp_num(u64::from(ns_val));
    let nm = crate::open::comp_text(name);
    let hit = cat
        .lookup("i_obj_name", &[Some(&ns), Some(&nm)])?
        .ok_or_else(|| DdlError::NotFound(name.to_owned()))?;
    match hit.1.first() {
        Some(DictValue::Num(n)) => Ok(*n as u32),
        _ => Err(DdlError::BadTableDef("obj$.obj# 形态非法".to_owned())),
    }
}

/// 对象类型码。
fn object_kind(cat: &mut Catalog<'_>, name: &str) -> Result<u32, DdlError> {
    object_kind_ns(cat, name, dict::namespace::TABLE)
}

/// 对象类型码（指定命名空间）。
fn object_kind_ns(cat: &mut Catalog<'_>, name: &str, ns_val: u32) -> Result<u32, DdlError> {
    let ns = crate::open::comp_num(u64::from(ns_val));
    let nm = crate::open::comp_text(name);
    let hit = cat
        .lookup("i_obj_name", &[Some(&ns), Some(&nm)])?
        .ok_or_else(|| DdlError::NotFound(name.to_owned()))?;
    match hit.1.get(3) {
        Some(DictValue::Num(t)) => Ok(*t as u32),
        _ => Err(DdlError::BadTableDef("obj$.type# 形态非法".to_owned())),
    }
}

fn object_name(cat: &mut Catalog<'_>, obj: u32) -> Result<String, DdlError> {
    let key = crate::open::comp_num(u64::from(obj));
    let hit = cat
        .lookup("i_obj_pk", &[Some(&key)])?
        .ok_or_else(|| DdlError::NotFound(obj.to_string()))?;
    match hit.1.get(1) {
        Some(DictValue::Text(t)) => Ok(t.clone()),
        _ => Err(DdlError::BadTableDef("obj$.name 形态非法".to_owned())),
    }
}

fn check_kind_or(cat: &mut Catalog<'_>, name: &str, kind: u32) -> Result<(), DdlError> {
    let ns_val = if kind == dict::obj_kind::INDEX {
        dict::namespace::INDEX
    } else {
        dict::namespace::TABLE
    };
    let k = object_kind_ns(cat, name, ns_val)?;
    if k != kind {
        return Err(DdlError::WrongKind(name.to_owned()));
    }
    Ok(())
}

/// 某表的全部索引对象号（`ind$` 的 `bobj#` 过滤；表小 ⇒ 全扫 `i_ind_pk`）。
fn index_objects_of(cat: &mut Catalog<'_>, table_obj: u32) -> Result<Vec<u32>, DdlError> {
    let inds = cat.scan_index("i_ind_pk")?;
    let mut out = Vec::new();
    for (_k, rid) in inds {
        let values = cat.fetch("ind$", rid)?;
        let (obj, bobj) = match (values.first(), values.get(1)) {
            (Some(DictValue::Num(o)), Some(DictValue::Num(b))) => (*o as u32, *b as u32),
            _ => continue,
        };
        if bobj == table_obj {
            out.push(obj);
        }
    }
    out.sort_unstable();
    Ok(out)
}

/// 删一个索引的全部字典行（`icol$` → `ind$` → `obj$` → `seg$`）。
fn drop_index_rows(
    w: &mut DictWriter<'_, '_, '_, '_, '_>,
    obj: u32,
    name: &str,
) -> Result<(), DdlError> {
    // icol$（按 obj# 前缀范围扫）。
    let icol_lo = bicdb_storage::key::encode(&[Some(&crate::open::comp_num(u64::from(obj)))]);
    let icol_hi = bicdb_storage::key::encode(&[
        Some(&crate::open::comp_num(u64::from(obj))),
        Some(&[0xFFu8; 32]),
    ]);
    let entries = w
        .cat
        .range_index("i_icol_pk", Some(&icol_lo), Some(&icol_hi))?;
    let _unused_icol_block = w.table("icol$")?;
    let icol_def = w.cat.table_def("icol$")?;
    for (_k, rid) in entries {
        w.delete_row("icol$", icol_def, rid)?;
    }
    // ind$（主键点查）。
    let _unused_ind_block = w.table("ind$")?;
    let ind_def = w.cat.table_def("ind$")?;
    let ind_key = crate::open::comp_num(u64::from(obj));
    if let Some((rid, _)) = w.cat.lookup("i_ind_pk", &[Some(&ind_key)])? {
        w.delete_row("ind$", ind_def, rid)?;
    }
    // seg$（按 dataobj# 主键）。
    let _unused_seg_block = w.table("seg$")?;
    let seg_def = w.cat.table_def("seg$")?;
    let seg_key = crate::open::comp_num(u64::from(obj));
    if let Some((rid, _)) = w.cat.lookup("i_seg_pk", &[Some(&seg_key)])? {
        w.delete_row("seg$", seg_def, rid)?;
    }
    // obj$（最后删：名字/对象号自此不可解析）。
    let _unused_obj_block = w.table("obj$")?;
    let obj_def = w.cat.table_def("obj$")?;
    let obj_key = crate::open::comp_num(u64::from(obj));
    if let Some((rid, _)) = w.cat.lookup("i_obj_pk", &[Some(&obj_key)])? {
        w.delete_row("obj$", obj_def, rid)?;
    }
    // 缓存：登记该对象失效（提交后代数自增 ⇒ 下一次读精确失效）。
    w.changed.push(obj);
    w.cat
        .row_cache()
        .note_change_name(dict::namespace::TABLE, name);
    Ok(())
}

/// 删一张表自身的字典行（**不含索引**——由 `drop_table` 先级联）。
fn drop_table_rows(
    w: &mut DictWriter<'_, '_, '_, '_, '_>,
    obj: u32,
    name: &str,
) -> Result<(), DdlError> {
    // col$（obj# 前缀范围扫）。
    let lo = bicdb_storage::key::encode(&[Some(&crate::open::comp_num(u64::from(obj)))]);
    let hi = bicdb_storage::key::encode(&[
        Some(&crate::open::comp_num(u64::from(obj))),
        Some(&[0xFFu8; 32]),
    ]);
    let cols = w.cat.range_index("i_col_pk", Some(&lo), Some(&hi))?;
    let _unused_col_block = w.table("col$")?;
    let col_def = w.cat.table_def("col$")?;
    for (_k, rid) in cols {
        w.delete_row("col$", col_def, rid)?;
    }
    // tab$。
    let _unused_tab_block = w.table("tab$")?;
    let tab_def = w.cat.table_def("tab$")?;
    let tab_key = crate::open::comp_num(u64::from(obj));
    if let Some((rid, _)) = w.cat.lookup("i_tab_pk", &[Some(&tab_key)])? {
        w.delete_row("tab$", tab_def, rid)?;
    }
    // seg$。
    let _unused_seg_block = w.table("seg$")?;
    let seg_def = w.cat.table_def("seg$")?;
    if let Some((rid, _)) = w.cat.lookup("i_seg_pk", &[Some(&tab_key)])? {
        w.delete_row("seg$", seg_def, rid)?;
    }
    // obj$。
    let _unused_obj_block = w.table("obj$")?;
    let obj_def = w.cat.table_def("obj$")?;
    if let Some((rid, _)) = w.cat.lookup("i_obj_pk", &[Some(&tab_key)])? {
        w.delete_row("obj$", obj_def, rid)?;
    }
    w.changed.push(obj);
    w.cat
        .row_cache()
        .note_change_name(dict::namespace::TABLE, name);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bicdb_storage::buffer::{BufferPool, CacheConfig, SystemClock, WalGuard};
    use bicdb_storage::controlfile::{
        ArchiveMode, ArchiveRecord, ControlFile, RedoEntries, WorkspaceEntry,
    };
    use bicdb_storage::datafile::DataFile;
    use bicdb_storage::undo::{create_undo_segment, UndoChain};
    use bicdb_workspace::io::MemFileIo;
    use bicdb_workspace::WorkspaceId;
    use std::path::Path;

    const WS: [u8; 8] = [13u8; 8];
    const UNDO_F: &str = "/mem/ddl_undo.dat";
    const DATA_F: &str = "/mem/ddl_file0.dat";

    fn seq(v: u64) -> CommitSeq {
        CommitSeq::from_raw(v).unwrap()
    }
    fn lsn(v: u64) -> bicdb_common::seq::Lsn {
        bicdb_common::seq::Lsn::from_raw(v).unwrap()
    }

    struct FakeWal;
    impl WalGuard for FakeWal {
        fn durable_lsn(&self) -> bicdb_common::seq::Lsn {
            bicdb_common::seq::Lsn::from_raw(u64::MAX >> 16).unwrap()
        }
        fn ensure_durable(&self, _t: bicdb_common::seq::Lsn) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// 一台"库"：file 0（字典：自举 + 种子）+ 撤销段 + 池 + 日志 + 引擎。
    pub(super) struct Rig {
        pool: &'static BufferPool<'static>,
        pub(super) engine: &'static Engine<'static, 'static, 'static, 'static>,
        cf_a: String,
        cf_b: String,
        wal: String,
        spec: bicdb_wal::group::GroupSpec,
    }

    pub(super) fn rig(io: &'static MemFileIo, tag: &str) -> Rig {
        let undo_file: &'static mut DataFile<'static> = Box::leak(Box::new(
            DataFile::create(io, Path::new(UNDO_F), 1, 1, WS, 512).unwrap(),
        ));
        let undo_handle = undo_file.handle();
        let undo_seg = create_undo_segment(undo_file, 2, 3, 4).unwrap();
        let mut file0 = DataFile::create(
            io,
            Path::new(DATA_F),
            0,
            bicdb_storage::bitmap::META_ROLE,
            WS,
            bicdb_storage::bitmap::FileLayout::meta().min_file_blocks() + 512,
        )
        .unwrap();
        let built = crate::create::create_dictionary(&mut file0, WS, false).unwrap();
        let mut cat = Catalog::from_entries(file0, built.entries.clone()).unwrap();
        cat.seed_own_dictionary(&built).unwrap();
        drop(cat);
        let file0_handle = DataFile::open(io, Path::new(DATA_F)).unwrap().handle();

        let pool: &'static BufferPool<'static> = Box::leak(Box::new(
            BufferPool::with_config(
                io,
                64,
                move |_ws, r| match r.file_id() {
                    0 => Some((file0_handle, r.block_id())),
                    1 => Some((undo_handle, r.block_id())),
                    _ => None,
                },
                FakeWal,
                SystemClock,
                CacheConfig::for_capacity(64),
            )
            .unwrap(),
        ));
        let cf_a = format!("/mem/ddl_cf_a_{tag}");
        let cf_b = format!("/mem/ddl_cf_b_{tag}");
        let wal = format!("/mem/ddl_wal_{tag}");
        let cf: &'static mut ControlFile<'static> = Box::leak(Box::new(
            ControlFile::format(
                io,
                Path::new(&cf_a),
                Path::new(&cf_b),
                &WorkspaceEntry {
                    workspace_id: WorkspaceId::from_raw(1).unwrap(),
                    created_at: 0,
                    derived_from: None,
                    derived_at_seq: seq(0),
                },
                &RedoEntries::new(2, 1).unwrap(),
                &ArchiveRecord::new(ArchiveMode::NoArchive),
            )
            .unwrap(),
        ));
        let spec = bicdb_wal::group::GroupSpec::new(2, 1, 8192).unwrap();
        let wal_writer = GroupWriter::create(io, cf, Path::new(&wal), spec, lsn(0)).unwrap();
        let engine: &'static Engine<'static, 'static, 'static, 'static> =
            Box::leak(Box::new(Engine::new(
                pool,
                wal_writer,
                UndoChain::open(undo_seg).with_pool(pool),
                seq(0),
            )));
        Rig {
            pool,
            engine,
            cf_a,
            cf_b,
            wal,
            spec,
        }
    }

    /// 打开目录（**接池**：活系统读法）。
    pub(super) fn open_catalog(io: &'static MemFileIo, rig: &Rig) -> Catalog<'static> {
        let mut cat = Catalog::open(io, Path::new(DATA_F)).unwrap();
        cat.attach_pool(rig.pool);
        cat
    }

    /// 用户表的列定义视图（DML 路径上由 Binder/目录给出；测试里与
    /// `user_table_spec()` 同源）。
    pub(super) fn user_cols() -> Vec<crate::dict::ColDef> {
        vec![
            crate::dict::ColDef {
                col: 1,
                name: "id",
                type_code: ColTypeCode::Number,
                length: 0,
                nullable: false,
            },
            crate::dict::ColDef {
                col: 2,
                name: "tag",
                type_code: ColTypeCode::Varchar2,
                length: 64,
                nullable: true,
            },
        ]
    }

    /// 往用户表插行（模拟 DML 路径：事务 + 表访问服务 + 提交）。
    pub(super) fn insert_user_rows(
        rig: &Rig,
        cat: &mut Catalog<'static>,
        table: &str,
        rows: &[Vec<DictValue>],
    ) -> usize {
        let (table_obj, _) = object_ref(cat, table).unwrap();
        let seg = live_segment_block(cat, table_obj).unwrap();
        let def = user_cols();
        let mut txn = rig.engine.begin().unwrap();
        let _ = rig.engine.reserve_commit_seq(&mut txn);
        let n = rig
            .engine
            .with_write_context(&mut txn, |pool, log, chain, t| {
                let mut access = TableAccess::new(pool, WS);
                let policy = InsertPolicy::in_place(0);
                for row in rows {
                    let bytes = row::encode(row, &def).unwrap();
                    access
                        .insert(log, chain, t, cat.file_mut(), seg, &bytes, &policy)
                        .unwrap();
                }
                rows.len()
            });
        rig.engine.commit(&mut txn).unwrap();
        n
    }

    pub(super) fn user_table_spec() -> TableSpec {
        TableSpec {
            name: "t1".to_owned(),
            columns: vec![
                ColumnSpec {
                    name: "id".to_owned(),
                    type_code: ColTypeCode::Number,
                    length: 0,
                    precision: None,
                    scale: None,
                    nullable: false,
                },
                ColumnSpec {
                    name: "tag".to_owned(),
                    type_code: ColTypeCode::Varchar2,
                    length: 64,
                    precision: None,
                    scale: None,
                    nullable: true,
                },
            ],
            options: TableOptions::default(),
        }
    }

    #[test]
    fn init_then_create_table_insert_and_build_index() {
        let io: &'static MemFileIo = Box::leak(Box::new(MemFileIo::new()));
        io.add_dir("/mem");
        let rig = rig(io, "t1");
        let mut cat = open_catalog(io, &rig);

        // ① 字典表初始化（建区收尾）：stat$/seq$ + object_id 序列。
        init_dictionary_tables(&mut cat, rig.engine).unwrap();
        assert!(object_exists(&mut cat, "stat$").unwrap(), "stat$ 已建");
        assert!(object_exists(&mut cat, "seq$").unwrap(), "seq$ 已建");
        // 幂等。
        init_dictionary_tables(&mut cat, rig.engine).unwrap();

        // ② 建表（用户对象号从 object_id 序列来 ⇒ ≥ 100）。
        let out = create_table(&mut cat, rig.engine, &user_table_spec()).unwrap();
        assert!(out.obj >= dict::obj_kind::USER_FIRST, "用户对象号 ≥ 100");
        assert_eq!(out.dataobj, out.obj);
        assert!(out.seg_block > 0);

        // 读路径可见（列/对象都从字典读回）。
        let cols = cat.columns(seq(out.commit_seq), out.obj).unwrap();
        assert_eq!(cols.len(), 2);
        assert_eq!(cols[0].name, "id");
        let r = cat
            .resolve(seq(out.commit_seq), dict::namespace::TABLE, "t1")
            .unwrap();
        assert_eq!(r.obj, out.obj);
        assert_eq!(r.mtime, out.commit_seq, "mtime = 预约的提交序号");
        // 重名 ⇒ 具名拒绝。
        match create_table(&mut cat, rig.engine, &user_table_spec()) {
            Err(DdlError::AlreadyExists(n)) => assert_eq!(n, "t1"),
            other => panic!("重名应拒绝：{other:?}"),
        }

        // ③ 灌 200 行（走表访问服务）。
        let rows: Vec<Vec<DictValue>> = (0..200u64)
            .map(|i| vec![DictValue::Num(i), DictValue::Text(format!("tag-{i:04}"))])
            .collect();
        insert_user_rows(&rig, &mut cat, "t1", &rows);

        // ④ 建索引（批量灌树）。
        let idx = IndexSpec {
            name: "i_t1_id".to_owned(),
            table: "t1".to_owned(),
            unique: true,
            columns: vec!["id".to_owned()],
        };
        let iout = create_index(&mut cat, rig.engine, &idx).unwrap();
        assert_eq!(iout.entries, 200, "灌入条目数 = 行数");
        assert!(iout.leaf_blocks >= 1);
        // 索引对象在字典里可见（`indexes_of` 读回）。
        let idxs = cat.indexes_of(seq(iout.commit_seq), out.obj).unwrap();
        assert_eq!(idxs.len(), 1);
        assert_eq!(idxs[0].obj, iout.obj);
        assert!(idxs[0].is_unique);
        assert_eq!(idxs[0].cols[0].col, 1);
    }

    #[test]
    fn unique_conflict_rolls_the_whole_ddl_back() {
        let io: &'static MemFileIo = Box::leak(Box::new(MemFileIo::new()));
        io.add_dir("/mem");
        let rig = rig(io, "t2");
        let mut cat = open_catalog(io, &rig);
        init_dictionary_tables(&mut cat, rig.engine).unwrap();
        let out = create_table(&mut cat, rig.engine, &user_table_spec()).unwrap();
        // 两行同 id ⇒ 唯一索引必然冲突。
        let rows = vec![
            vec![DictValue::Num(7), DictValue::Text("a".to_owned())],
            vec![DictValue::Num(7), DictValue::Text("b".to_owned())],
        ];
        insert_user_rows(&rig, &mut cat, "t1", &rows);
        let idx = IndexSpec {
            name: "i_t1_dup".to_owned(),
            table: "t1".to_owned(),
            unique: true,
            columns: vec!["id".to_owned()],
        };
        let err = match create_index(&mut cat, rig.engine, &idx) {
            Err(e) => e,
            Ok(_) => panic!("重复键应拒绝"),
        };
        assert!(
            matches!(
                err,
                DdlError::Index(bicdb_index::IndexError::DuplicateKey { .. })
            ),
            "{err}"
        );
        // **整个 DDL 事务回滚**：索引对象不存在、表的索引清单为空。
        assert!(
            cat.resolve(seq(9_999), dict::namespace::TABLE, "i_t1_dup")
                .is_err(),
            "回滚后索引名不可解析"
        );
        assert!(cat.indexes_of(seq(9_999), out.obj).unwrap().is_empty());
        // 非唯一索引可建。
        let idx2 = IndexSpec {
            unique: false,
            name: "i_t1_id_nu".to_owned(),
            ..idx
        };
        let ok = create_index(&mut cat, rig.engine, &idx2).unwrap();
        assert_eq!(ok.entries, 2);
    }

    #[test]
    fn drop_table_cascades_its_indexes() {
        let io: &'static MemFileIo = Box::leak(Box::new(MemFileIo::new()));
        io.add_dir("/mem");
        let rig = rig(io, "t3");
        let mut cat = open_catalog(io, &rig);
        init_dictionary_tables(&mut cat, rig.engine).unwrap();
        let out = create_table(&mut cat, rig.engine, &user_table_spec()).unwrap();
        let rows = vec![
            vec![DictValue::Num(1), DictValue::Text("a".to_owned())],
            vec![DictValue::Num(2), DictValue::Text("b".to_owned())],
        ];
        insert_user_rows(&rig, &mut cat, "t1", &rows);
        let idx = IndexSpec {
            name: "i_t1_id".to_owned(),
            table: "t1".to_owned(),
            unique: true,
            columns: vec!["id".to_owned()],
        };
        let iout = create_index(&mut cat, rig.engine, &idx).unwrap();
        assert_eq!(cat.indexes_of(seq(9_999), out.obj).unwrap().len(), 1);

        // DROP TABLE：级联索引。
        let d = drop_table(&mut cat, rig.engine, "t1").unwrap();
        assert_eq!(d.obj, out.obj);
        assert_eq!(d.indexes, vec![iout.obj]);
        assert!(cat
            .resolve(seq(9_999), dict::namespace::TABLE, "t1")
            .is_err());
        assert!(cat
            .resolve(seq(9_999), dict::namespace::TABLE, "i_t1_id")
            .is_err());
        assert!(cat.resolve_by_obj(seq(9_999), out.obj).is_err());
        // 名字可以复用（obj# 不回绕——新对象号不同）。
        let again = create_table(&mut cat, rig.engine, &user_table_spec()).unwrap();
        assert_ne!(again.obj, out.obj, "对象号不回绕");
    }

    /// **崩溃恢复**：DDL 后刷日志 → 丢缓存 → 仅重放 redo → 字典自洽（建的表/索引都在）。
    #[test]
    fn ddl_survives_a_crash_recovery() {
        let io: &'static MemFileIo = Box::leak(Box::new(MemFileIo::new()));
        io.add_dir("/mem");
        let rig = rig(io, "t4");
        let mut cat = open_catalog(io, &rig);
        init_dictionary_tables(&mut cat, rig.engine).unwrap();
        let out = create_table(&mut cat, rig.engine, &user_table_spec()).unwrap();
        let rows: Vec<Vec<DictValue>> = (0..50u64)
            .map(|i| vec![DictValue::Num(i), DictValue::Text(format!("t{i}"))])
            .collect();
        insert_user_rows(&rig, &mut cat, "t1", &rows);
        let idx = IndexSpec {
            name: "i_t1_id".to_owned(),
            table: "t1".to_owned(),
            unique: true,
            columns: vec!["id".to_owned()],
        };
        let iout = create_index(&mut cat, rig.engine, &idx).unwrap();

        // **刷日志 + 丢缓存**（页不回写 ⇒ 恢复只靠 redo）。
        cat = Catalog::open(io, Path::new(DATA_F)).unwrap(); // 丢弃旧的池像引用
        drop(cat);
        // 池是 leak 的：刷新日志即可（引擎的日志写口在 commit 时已耐久；
        // 这里显式再刷一次，模拟"崩溃前的 durable 点"）。
        let mut txn = rig.engine.begin().unwrap();
        let _ = rig.engine.reserve_commit_seq(&mut txn);
        let _ = rig.engine.rollback(&mut txn);

        // 重放：把恢复侧的池换掉（同一文件重新打开）。
        let groups = {
            let cf_ro = ControlFile::open(io, Path::new(&rig.cf_a), Path::new(&rig.cf_b)).unwrap();
            bicdb_wal::group::online_groups(io, &cf_ro, Path::new(&rig.wal), rig.spec).unwrap()
        };
        let f0 = DataFile::open(io, Path::new(DATA_F)).unwrap().handle();
        let undo = DataFile::open(io, Path::new(UNDO_F)).unwrap().handle();
        let mut resolve = |r: bicdb_storage::rowid::Rdba| match r.file_id() {
            0 => Some((f0, r.block_id())),
            1 => Some((undo, r.block_id())),
            _ => None,
        };
        let report = bicdb_wal::recovery::redo_from(io, &groups, lsn(0), &mut resolve).unwrap();
        assert!(report.records_applied > 50, "DDL 的改动都进了日志");

        // 重开目录（无池 = 直读文件——恢复后文件就是真值）。
        let mut cat2 = Catalog::open(io, Path::new(DATA_F)).unwrap();
        let snap = seq(9_999);
        let r = cat2.resolve(snap, dict::namespace::TABLE, "t1").unwrap();
        assert_eq!(r.obj, out.obj, "建的表在恢复后仍在");
        assert_eq!(cat2.columns(snap, out.obj).unwrap().len(), 2);
        let idxs = cat2.indexes_of(snap, out.obj).unwrap();
        assert_eq!(idxs.len(), 1, "索引也在");
        assert_eq!(idxs[0].obj, iout.obj);
        assert!(
            object_exists(&mut cat2, "seq$").unwrap(),
            "恢复后 seq$ 仍在"
        );
    }
}

// ───────────────────────────── Move 失效落点（§5.5）─────────────────────────────

/// **`Move` 的索引失效落点**（`目录详设` §5.5）：把该表全部索引的
/// **`obj$.status`（权威）** 与 **`ind$.status`（副本）** 在同一事务里置 0
/// （无效），`mtime` 一并推进到预约序号。
///
/// **本函数只做"字典侧的失效"**；`Move` 的**主体**（文件的物理搬迁、控制文件
/// 更新、暂停写窗协议）在 `arch/02` §3.4 的存储侧路径——两者由调用方按
/// "先搬文件、后落失效"（或反之，随窗口协议）编排。物理搬迁未落地前，本入口
/// 供测试与"重建索引"路径使用。
///
/// **失效的两处消费者**：① 选路期排除（[`Catalog::usable_indexes_of`]）；
/// ② 计划缓存键失配（`(obj#, mtime, status)` 三元组——`ObjectVersion.status`）。
pub fn invalidate_indexes_for_move(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    table: &str,
) -> Result<Vec<u32>, DdlError> {
    let (table_obj, kind) = object_ref(cat, table)?;
    if kind != dict::obj_kind::TABLE {
        return Err(DdlError::WrongKind(table.to_owned()));
    }
    let index_objs = index_objects_of(cat, table_obj)?;
    let (out, _seq) = with_ddl_txn(cat, engine, |w| {
        for &obj in &index_objs {
            set_object_status(w, obj, 0)?;
            set_index_status(w, obj, 0)?;
        }
        Ok(index_objs.clone())
    })?;
    Ok(out)
}

/// **重建索引**（`目录详设` §5.5 的"重建由 5.3 的 ④ 换段承担"）。
///
/// **本切片形态**：同一事务内 `DROP INDEX` + `CREATE INDEX`（同一名字、同一
/// 键列）。**记档**：对象号随之变化；"原地换段"（保留对象号）留作优化——
/// 触发条件 = 出现"依赖对象号稳定"的引用方（权限/计划缓存以外的外部引用）。
pub fn rebuild_index(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    name: &str,
) -> Result<CreateIndexOutcome, DdlError> {
    // 先读旧定义（键列名 + 唯一标志 + 基表名）。
    let (obj, kind) = object_ref_ns(cat, name, dict::namespace::INDEX)?;
    if kind != dict::obj_kind::INDEX {
        return Err(DdlError::WrongKind(name.to_owned()));
    }
    let (bobj, unique, col_numbers) = index_definition(cat, obj)?;
    let table_name = object_name(cat, bobj)?;
    let mut columns = Vec::with_capacity(col_numbers.len());
    for cn in &col_numbers {
        columns.push(column_name(cat, bobj, *cn)?);
    }
    let spec = IndexSpec {
        name: name.to_owned(),
        table: table_name,
        unique,
        columns,
    };
    // 同事务内换段：先删旧对象，再按同一规格建新（名字在这次事务里被释放）。
    let (scan_cols, base_seg) = scan_view(cat, bobj)?;
    let (out, seq) = with_ddl_txn(cat, engine, |w| {
        drop_index_rows(w, obj, name)?;
        create_index_inner(w, &spec, bobj, &col_numbers, scan_cols, base_seg)
    })?;
    Ok(CreateIndexOutcome {
        commit_seq: seq,
        ..out
    })
}

/// 索引定义（`bobj#`、唯一标志、键列号序列）——读 `ind$` + `icol$`。
fn index_definition(cat: &mut Catalog<'_>, obj: u32) -> Result<(u32, bool, Vec<u32>), DdlError> {
    let key = crate::open::comp_num(u64::from(obj));
    let (_, ind) = cat
        .lookup("i_ind_pk", &[Some(&key)])?
        .ok_or_else(|| DdlError::NotFound(obj.to_string()))?;
    let bobj = match ind.get(1) {
        Some(DictValue::Num(n)) => *n as u32,
        _ => return Err(DdlError::BadTableDef("ind$.bobj# 形态非法".to_owned())),
    };
    let unique = matches!(ind.get(4), Some(DictValue::Bool(true)));
    let lo = bicdb_storage::key::encode(&[Some(&crate::open::comp_num(u64::from(obj)))]);
    let hi = bicdb_storage::key::encode(&[
        Some(&crate::open::comp_num(u64::from(obj))),
        Some(&[0xFFu8; 32]),
    ]);
    let rows = cat.range_index("i_icol_pk", Some(&lo), Some(&hi))?;
    let mut cols: Vec<(u32, u32)> = Vec::new(); // (pos, col#)
    for (_k, rid) in rows {
        let Some(values) = cat.fetch_opt("icol$", rid)? else {
            continue;
        };
        let (pos, col) = match (values.get(1), values.get(2)) {
            (Some(DictValue::Num(p)), Some(DictValue::Num(c))) => (*p as u32, *c as u32),
            _ => continue,
        };
        cols.push((pos, col));
    }
    cols.sort_unstable();
    Ok((bobj, unique, cols.into_iter().map(|(_, c)| c).collect()))
}

/// 某对象的第 `col#` 列的列名（`col$` 点查）。
fn column_name(cat: &mut Catalog<'_>, obj: u32, col_no: u32) -> Result<String, DdlError> {
    let obj_b = crate::open::comp_num(u64::from(obj));
    let col_b = crate::open::comp_num(u64::from(col_no));
    let lo = bicdb_storage::key::encode(&[Some(&obj_b), Some(&col_b)]);
    let rows = cat.range_index("i_col_pk", Some(&lo), Some(&lo))?;
    for (_k, rid) in rows {
        let Some(values) = cat.fetch_opt("col$", rid)? else {
            continue;
        };
        if let Some(DictValue::Text(n)) = values.get(2) {
            return Ok(n.clone());
        }
    }
    Err(DdlError::BadTableDef(format!(
        "col$ 无 obj# {obj} col# {col_no}"
    )))
}

/// **改对象状态**（`obj$.status`；mtime 一并推进）。
fn set_object_status(
    w: &mut DictWriter<'_, '_, '_, '_, '_>,
    obj: u32,
    status: u64,
) -> Result<(), DdlError> {
    let (obj_def, obj_block) = w.table("obj$")?;
    let key = crate::open::comp_num(u64::from(obj));
    let hit = w
        .cat
        .lookup("i_obj_pk", &[Some(&key)])?
        .ok_or_else(|| DdlError::NotFound(obj.to_string()))?;
    let (rid, mut values) = hit;
    values[6] = DictValue::Num(w.seq.as_raw()); // mtime
    values[7] = DictValue::Num(status); // status
    w.update_row_nonkey("obj$", obj_block, obj_def, rid, &values)?;
    w.changed.push(obj);
    w.cat.row_cache().note_change_obj(obj);
    Ok(())
}

/// **改索引状态**（`ind$.status` 副本；同期 `obj$` 由调用方改）。
fn set_index_status(
    w: &mut DictWriter<'_, '_, '_, '_, '_>,
    obj: u32,
    status: u64,
) -> Result<(), DdlError> {
    let (ind_def, ind_block) = w.table("ind$")?;
    let key = crate::open::comp_num(u64::from(obj));
    let hit = w
        .cat
        .lookup("i_ind_pk", &[Some(&key)])?
        .ok_or_else(|| DdlError::NotFound(obj.to_string()))?;
    let (rid, mut values) = hit;
    values[5] = DictValue::Num(status);
    w.update_row_nonkey("ind$", ind_block, ind_def, rid, &values)?;
    Ok(())
}

#[cfg(test)]
mod c5_tests {
    use super::tests::{insert_user_rows, open_catalog, rig, user_table_spec};
    use super::*;

    /// 单列表名（`plain_t`）——与 `user_table_spec` 同形状（id, tag）。
    fn plain_spec() -> TableSpec {
        TableSpec {
            name: "plain_t".to_owned(),
            ..user_table_spec()
        }
    }

    /// 往 `plain_t` 插行（薄包装：共用既有辅助）。
    fn insert_rows_for(
        rig: &super::tests::Rig,
        cat: &mut Catalog<'static>,
        _table: &str,
        rows: &[Vec<DictValue>],
    ) -> usize {
        insert_user_rows(rig, cat, "plain_t", rows)
    }

    /// **C5 序列分配**：内存批（一次行更新发一批号）+ 跳号无害（重开工作区
    /// 后从 seq$.next_val 续发，未用的号被跳过、绝不重复）。
    #[test]
    fn sequence_allocates_in_batches_and_never_repeats() {
        let io: &'static bicdb_workspace::io::MemFileIo =
            Box::leak(Box::new(bicdb_workspace::io::MemFileIo::new()));
        io.add_dir("/mem");
        let rig = rig(io, "c5seq");
        let mut cat = open_catalog(io, &rig);
        init_dictionary_tables(&mut cat, rig.engine).unwrap();

        // seq$ 里 object_id 的 next_val 与 cache。
        let read_seq = |cat: &mut Catalog<'static>| -> (u64, u64) {
            let seq_def = dict::DICT_TABLES.iter().find(|t| t.name == "seq$").unwrap();
            let col1 = &seq_def.columns[0];
            let comp = row::component_bytes(&DictValue::Num(OBJECT_ID_SEQ), col1)
                .unwrap()
                .unwrap();
            let (_, v) = cat.lookup("i_seq_pk", &[Some(&comp)]).unwrap().unwrap();
            match (&v[2], &v[3]) {
                (DictValue::Num(n), DictValue::Num(c)) => (*n, *c),
                _ => panic!("seq$ 行形态非法"),
            }
        };
        let (next0, cache) = read_seq(&mut cat);
        assert_eq!(next0, u64::from(dict::obj_kind::USER_FIRST), "播种值 = 100");
        assert_eq!(cache, 20, "cache 列 = 20");

        // 建 3 张表 ⇒ 3 个对象号：只应发生 **1 次** 行更新（一批够用）。
        let mut objs = Vec::new();
        for i in 0..3 {
            let spec = TableSpec {
                name: format!("s{i}"),
                ..plain_spec()
            };
            objs.push(create_table(&mut cat, rig.engine, &spec).unwrap().obj);
        }
        let (next1, _) = read_seq(&mut cat);
        assert_eq!(next1, next0 + cache, "一批一次刷入：next_val = 100 + 20");
        assert_eq!(objs, vec![100, 101, 102], "号连号发放");

        // 用满这一批（再建 17 张 ⇒ 第 18 张触发第二批）。
        for i in 3..20 {
            let spec = TableSpec {
                name: format!("s{i}"),
                ..plain_spec()
            };
            objs.push(create_table(&mut cat, rig.engine, &spec).unwrap().obj);
        }
        let (next2, _) = read_seq(&mut cat);
        assert_eq!(next2, next0 + cache, "仍在这一批内（100..120）");
        let last = create_table(
            &mut cat,
            rig.engine,
            &TableSpec {
                name: "s20".to_owned(),
                ..plain_spec()
            },
        )
        .unwrap()
        .obj;
        assert_eq!(last, 120, "第 21 个号触发第二批");
        let (next3, _) = read_seq(&mut cat);
        assert_eq!(next3, 140, "第二批刷入 140");

        // **跳号无害**：丢弃内存批（模拟重开工作区）——下次分配从持久值 140 续。
        cat.obj_seq.set(None);
        let obj = create_table(
            &mut cat,
            rig.engine,
            &TableSpec {
                name: "s21".to_owned(),
                ..plain_spec()
            },
        )
        .unwrap()
        .obj;
        assert_eq!(obj, 140, "重开后从持久位点续发（120..139 = 跳号）");
        assert!(!objs.contains(&obj) && obj != last, "绝不重复");
    }

    /// **C5 stat$ 接入**：建表/建索引写统计行；索引的 `row_est` = 条目数。
    #[test]
    fn statistics_rows_are_written_on_ddl() {
        let io: &'static bicdb_workspace::io::MemFileIo =
            Box::leak(Box::new(bicdb_workspace::io::MemFileIo::new()));
        io.add_dir("/mem");
        let rig = rig(io, "c5stat");
        let mut cat = open_catalog(io, &rig);
        init_dictionary_tables(&mut cat, rig.engine).unwrap();
        let out = create_table(&mut cat, rig.engine, &plain_spec()).unwrap();

        let stat_of = |cat: &mut Catalog<'static>, obj: u32| -> Option<u64> {
            let key = crate::open::comp_num(u64::from(obj));
            let (_, v) = cat.lookup("i_stat_pk", &[Some(&key)]).unwrap()?;
            match v.get(1) {
                Some(DictValue::Num(n)) => Some(*n),
                _ => None,
            }
        };
        assert_eq!(stat_of(&mut cat, out.obj), Some(0), "建表：行数估计 0");

        // 灌 30 行 + 建索引 ⇒ 索引行数估计 = 30。
        let rows: Vec<Vec<DictValue>> = (0..30u64)
            .map(|i| vec![DictValue::Num(i), DictValue::Text(format!("t{i}"))])
            .collect();
        insert_rows_for(&rig, &mut cat, "plain_t", &rows);
        let spec = IndexSpec {
            name: "i_plain_id".to_owned(),
            table: "plain_t".to_owned(),
            unique: true,
            columns: vec!["id".to_owned()],
        };
        let iout = create_index(&mut cat, rig.engine, &spec).unwrap();
        assert_eq!(stat_of(&mut cat, iout.obj), Some(30), "索引统计 = 条目数");
    }

    /// **C5 Move 失效落点**：`obj$.status`/`ind$.status` 同事务置 0 ⇒
    /// **不进选路**（`usable_indexes_of` 为空）+ 计划缓存键失配（`status` 进版本）
    /// + **重建**后恢复可用。
    #[test]
    fn move_invalidation_removes_indexes_from_selection_and_rebuild_restores() {
        let io: &'static bicdb_workspace::io::MemFileIo =
            Box::leak(Box::new(bicdb_workspace::io::MemFileIo::new()));
        io.add_dir("/mem");
        let rig = rig(io, "c5move");
        let mut cat = open_catalog(io, &rig);
        init_dictionary_tables(&mut cat, rig.engine).unwrap();
        let out = create_table(&mut cat, rig.engine, &plain_spec()).unwrap();
        let rows: Vec<Vec<DictValue>> = (0..10u64)
            .map(|i| vec![DictValue::Num(i), DictValue::Text(format!("t{i}"))])
            .collect();
        insert_rows_for(&rig, &mut cat, "plain_t", &rows);
        let spec = IndexSpec {
            name: "i_plain_id".to_owned(),
            table: "plain_t".to_owned(),
            unique: true,
            columns: vec!["id".to_owned()],
        };
        let iout = create_index(&mut cat, rig.engine, &spec).unwrap();
        let snap = CommitSeq::from_raw(9_999).unwrap();

        // 失效前：可进选路。
        assert_eq!(cat.usable_indexes_of(snap, out.obj).unwrap().len(), 1);
        let v0 = cat.object_version(snap, iout.obj).unwrap();
        assert_eq!(v0.status, 1);

        // Move 失效。
        let invalidated = invalidate_indexes_for_move(&mut cat, rig.engine, "plain_t").unwrap();
        assert_eq!(invalidated, vec![iout.obj]);
        assert!(
            cat.usable_indexes_of(snap, out.obj).unwrap().is_empty(),
            "status ≠ 1 ⇒ 不进选路"
        );
        let v1 = cat.object_version(snap, iout.obj).unwrap();
        assert_eq!(v1.status, 0, "obj$.status 权威 = 0");
        assert!(v1.mtime > v0.mtime, "mtime 一并推进");
        // ind$ 副本同改。
        let key = crate::open::comp_num(u64::from(iout.obj));
        let (_, ind) = cat.lookup("i_ind_pk", &[Some(&key)]).unwrap().unwrap();
        assert_eq!(ind[5], DictValue::Num(0), "ind$.status 副本 = 0");
        // 清单仍能读出它（带状态）——调用方按需呈现。
        assert_eq!(cat.indexes_of(snap, out.obj).unwrap().len(), 1);

        // 重建：同事务换段 ⇒ 可用性恢复（新对象号）。
        let rebuilt = rebuild_index(&mut cat, rig.engine, "i_plain_id").unwrap();
        assert_eq!(rebuilt.entries, 10, "重建后灌满 10 条");
        assert_ne!(rebuilt.obj, iout.obj, "换段 ⇒ 新对象号（已记档）");
        let usable = cat.usable_indexes_of(snap, out.obj).unwrap();
        assert_eq!(usable.len(), 1);
        assert_eq!(usable[0].obj, rebuilt.obj);
        assert_eq!(usable[0].status, 1);
    }
}
