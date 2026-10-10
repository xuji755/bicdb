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
//! 1..N       自举集（N = 15 普通 / 27 public；建区期连号，见 create.rs）
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
    /// Internal dictionary tables and indexes are SQL read-only.
    ReadOnlyDictionary(String),
    /// **唯一键冲突**（DCL 的字典行：`fs$` 名字/路径、`ws$` 名字、`user$` 名字）。
    ///
    /// 单独一支是为了文案：`AlreadyExists` 会套上"对象 `…` 已存在"的外壳，
    /// 而这里的载荷已经是一句完整的话（点名表、键、冲突值）。
    UniqueViolation(String),
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
            DdlError::ReadOnlyDictionary(n) => write!(f, "字典对象 `{n}` 只读，不能修改或删除"),
            DdlError::AlreadyExists(n) => write!(f, "对象 `{n}` 已存在"),
            DdlError::UniqueViolation(why) => f.write_str(why),
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
pub(crate) struct DictWriter<'a, 'b, 'io, 'lio, 'lf> {
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

impl Catalog<'_> {
    pub(crate) fn check_ddl_deadline(&self) -> Result<(), DdlError> {
        if self
            .ddl_deadline
            .is_some_and(|end| std::time::Instant::now() >= end)
        {
            return Err(DdlError::BadIndexDef(
                "graph statement time budget exceeded (phase native DDL)".into(),
            ));
        }
        Ok(())
    }
}

impl<'a, 'b, 'io, 'lio, 'lf> DictWriter<'a, 'b, 'io, 'lio, 'lf> {
    fn ws(&self) -> [u8; 8] {
        self.cat.ws()
    }

    /// 表定义（**内核常量**：字典表才有）+ 段头块（**活路径**：obj$ → seg$）。
    fn table(&mut self, name: &str) -> Result<(&'static DictTable, u32), DdlError> {
        self.cat.check_ddl_deadline()?;
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

    /// **DCL 用的字典行写口**：**先做唯一键预检**（在写事务内、按行字节判），
    /// 再插行。
    ///
    /// **为什么要这一道**：索引层**不判唯一性**（`access/index.rs` 的既定取舍：
    /// 索引层只看槽位死活，槽位复用会把合法插入误判成冲突）——唯一性判定放在
    /// "行字节可见的层"。SQL 的表/索引 DDL 有各自的预检；**DCL 的字典行**
    /// （`fs$` 名字/路径、`ws$` 名字、`user$` 名字）过去没有，这一道补上，
    /// 判据与 SQL 侧同源：取候选行、重算键、**逐字节比**。
    pub(crate) fn insert_dict_row(
        &mut self,
        table: &str,
        values: &[DictValue],
    ) -> Result<RowId, DdlError> {
        self.ensure_unique_keys(table, values)?;
        self.insert_row(table, values)
    }

    /// **唯一键预检**（每个 `unique` 键各查一次；命中即 `Duplicate`）。
    ///
    /// 判据链条：键列值 → 键分量字节（与行内同源）→ 该索引上取候选 ROWID →
    /// 取候选行 → **重算键分量并逐字节比**（槽位复用留下的陈旧条目因此不会被
    /// 误判——它的行重算出来是别的键）。
    fn ensure_unique_keys(&mut self, table: &str, values: &[DictValue]) -> Result<(), DdlError> {
        let (def, _seg) = self.table(table)?;
        for key_def in def.keys {
            if !key_def.unique {
                continue;
            }
            let comps_bytes = key_components(values, def, key_def)?;
            let comps = bicdb_storage::key::decode(&comps_bytes)
                .map_err(|e| DdlError::BadIndexDef(e.to_string()))?;
            let refs: Vec<Option<&[u8]>> = comps.iter().map(|c| c.as_deref()).collect();
            let Some((rid, row)) = self
                .cat
                .lookup(key_def.name, &refs)
                .map_err(|e| DdlError::BadIndexDef(e.to_string()))?
            else {
                continue;
            };
            // 候选行的键列值 **与该索引的键列** 对齐后重算（行值按表列序排）。
            let candidate: Vec<DictValue> = def
                .columns
                .iter()
                .map(|c| {
                    row.get(usize::from(c.col) - 1)
                        .cloned()
                        .unwrap_or(DictValue::Null)
                })
                .collect();
            let cand_bytes = key_components(&candidate, def, key_def)?;
            if cand_bytes == comps_bytes {
                let shown = key_def
                    .cols
                    .iter()
                    .map(|c| {
                        let v = values.get(usize::from(*c) - 1);
                        match v {
                            Some(DictValue::Text(t)) => t.clone(),
                            Some(DictValue::Num(n)) => n.to_string(),
                            _ => "?".to_owned(),
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(DdlError::UniqueViolation(format!(
                    "{table} 的唯一键 {} 冲突：{shown} 已存在（ROWID {rid}）",
                    key_def.name
                )));
            }
        }
        Ok(())
    }

    /// **DCL 用的字典行改口**（见 [`DictWriter::update_row`] 的键列限制）。
    pub(crate) fn update_dict_row(
        &mut self,
        table: &str,
        rid: RowId,
        values: &[DictValue],
    ) -> Result<(), DdlError> {
        self.update_row(table, rid, values)
    }

    /// **删一行字典行**（`DROP USER` 等：行真删——名字要能被再次使用）。
    ///
    /// 索引项按该表的每个键同步删除（与插入对称）。
    pub(crate) fn delete_dict_row(&mut self, table: &str, rid: RowId) -> Result<(), DdlError> {
        // 删行按 ROWID 定位（不经段头）——段头块在 `table()` 里校验存在即可。
        let (def, _seg_block) = self.table(table)?;
        let values = self
            .cat
            .fetch(table, rid)
            .map_err(|e| DdlError::BadTableDef(format!("读旧行失败：{e}")))?;
        let policy = InsertPolicy::in_place(0);
        let ws = self.ws();
        TableAccess::new(self.pool, ws).delete(
            self.log,
            self.chain,
            self.txn,
            self.cat.file_mut(),
            rid,
            &policy,
        )?;
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
        Ok(())
    }

    /// **改写一行字典行**（DCL 用：`fs$` 的状态/开关、`ws$` 的属主/配额、
    /// `user$` 的口令/状态——**都不是键列**）。
    ///
    /// **键列不允许改**：改了就得"删旧索引项 + 插新索引项"，那是两条路径；
    /// 这里**当场拒绝**（`DclError` 之外的调用方也没法绕：校验在写之前）。
    /// 要改键列 ⇒ 调用方先 `delete_row` 再 `insert_row`。
    fn update_row(
        &mut self,
        table: &str,
        rid: RowId,
        values: &[DictValue],
    ) -> Result<(), DdlError> {
        let (def, seg_block) = self.table(table)?;
        // 键列不变校验：取旧行，逐键比较键分量。
        let old = self
            .cat
            .fetch(table, rid)
            .map_err(|e| DdlError::BadTableDef(format!("读旧行失败：{e}")))?;
        for key_def in def.keys {
            if !key_def.unique {
                continue;
            }
            for &col_no in key_def.cols {
                let idx = usize::from(col_no) - 1;
                if old.get(idx) != values.get(idx) {
                    return Err(DdlError::BadTableDef(format!(
                        "字典行改写不得动键列（{table}.{}，键 {}）——先删后插",
                        def.columns[idx].name, key_def.name
                    )));
                }
            }
        }
        let bytes = crate::row::encode(values, def.columns)
            .map_err(|e| DdlError::BadTableDef(e.to_string()))?;
        let policy = InsertPolicy::in_place(0);
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
    dcl_txn(cat, engine, body)
}

/// **DCL 事务壳**（`crate::dcl` 用；与 DDL 同一条路径——"没有第二条路"）。
pub(crate) fn dcl_txn<R>(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    body: impl FnOnce(&mut DictWriter<'_, '_, '_, '_, '_>) -> Result<R, DdlError>,
) -> Result<(R, u64), DdlError> {
    cat.check_ddl_deadline()?;
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
        let r = body(&mut w).and_then(|r| {
            w.cat.check_ddl_deadline()?;
            Ok(r)
        });
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
            // 回滚失败**不吞**：DDL 的错误是主错，但回滚失败意味着锁/undo 可能
            // 未清（后续语句会撞上），要一并报出来。
            let rolled = engine.rollback(&mut txn);
            cat.row_cache().bump_and_clear();
            // A new object-ID batch may have been reserved in this transaction.
            // Its seq$ high-water mark has rolled back; retaining the in-memory
            // batch could commit IDs that a later reservation issues again.
            // Drop cached IDs even when rollback fails; only durable seq$ state
            // may seed subsequent allocations. Gaps in committed batches are safe.
            cat.obj_seq.set(None);
            match rolled {
                Ok(_) => Err(e),
                Err(rb) => Err(DdlError::BadTableDef(format!(
                    "DDL 失败且回滚也失败：{e} / {rb}"
                ))),
            }
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
    let (seq_def, seq_block) = w.table("seq$").map_err(|error| match error {
        DdlError::NotFound(_) => {
            DdlError::BadTableDef("seq$ 未初始化（先跑 init_dictionary_tables）".to_owned())
        }
        // Preserve timeout/I/O errors rather than misreporting a missing sequence.
        other => other,
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

    /// Delete the heap version, retaining redo-only index candidates. Removing
    /// candidates here would make restored rows unreachable after rollback or
    /// crash recovery, and prevent CR readers finding a previous dictionary row.
    /// Lookup/range callers recheck row visibility and keys; GC needs a horizon.
    fn delete_row(
        &mut self,
        table: &str,
        _def: &'static DictTable,
        rid: RowId,
    ) -> Result<(), DdlError> {
        // Preserve the stable index entrance when following a forwarding row.
        self.cat.fetch(table, rid)?;
        let landed = self.cat.resolve_rid(rid)?;
        let mut access = TableAccess::new(self.pool, self.ws());
        access.delete(
            self.log,
            self.chain,
            self.txn,
            self.cat.file_mut(),
            landed,
            &InsertPolicy::in_place(0),
        )?;
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
        graph_source: Option<(u32, &[u8])>,
    ) -> Result<(), DdlError> {
        let ind = vec![
            DictValue::Num(u64::from(obj)),
            DictValue::Num(u64::from(bobj)),
            DictValue::Num(u64::from(
                graph_source.map_or(dict::index_kind::BTREE, |(kind, _)| kind),
            )),
            DictValue::Num(col_numbers.len() as u64),
            DictValue::Bool(unique),
            DictValue::Num(1), // status = 有效
            graph_source.map_or(DictValue::Null, |(_, v)| DictValue::Bytes(v.to_vec())),
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
    create_table_kind(cat, engine, spec, dict::obj_kind::TABLE)
}

/// Create a legacy graph descriptor and transactional record heap atomically.
/// Kept for legacy layout adapters; SQL uses `create_graph_with_physical_routes`.
pub fn create_graph(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    name: &str,
) -> Result<CreateTableOutcome, DdlError> {
    create_table_kind(cat, engine, &graph_table_spec(name), dict::obj_kind::GRAPH)
}

fn graph_table_spec(name: &str) -> TableSpec {
    let columns = [
        ("ordinal", ColTypeCode::Number, 0),
        ("data", ColTypeCode::Bytes, 4096),
    ]
    .into_iter()
    .map(|(name, type_code, length)| ColumnSpec {
        name: name.into(),
        type_code,
        length,
        precision: None,
        scale: None,
        nullable: false,
    })
    .collect();
    TableSpec {
        name: name.into(),
        columns,
        options: TableOptions::default(),
    }
}

/// Create a graph, its record/ordinal trees and all four physical routes in
/// one DDL transaction. The callback must publish the layout manifest before
/// returning; any failure rolls back the named graph and every owned object.
pub fn create_graph_with_physical_routes(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    name: &str,
    publish: impl FnOnce(
        GraphPhysicalRoute,
        GraphPhysicalRoute,
        &GraphPhysicalRoutes,
        &mut Catalog<'_>,
        &BufferPool<'_>,
        &mut GroupWriter<'_, '_>,
        &mut UndoChain<'_, '_>,
        &mut Txn,
    ) -> Result<(), DdlError>,
) -> Result<CreateTableOutcome, DdlError> {
    let spec = graph_table_spec(name);
    validate_new_table(cat, &spec)?;
    let (outcome, seq) = with_ddl_txn(cat, engine, |w| {
        let (outcome, primary) = create_table_kind_inner(w, &spec, dict::obj_kind::GRAPH)?;
        let routes = create_graph_physical_routes_inner(w, outcome.obj)?;
        publish(
            GraphPhysicalRoute {
                obj: outcome.obj,
                block: outcome.seg_block,
            },
            primary.expect("graph ordinal tree"),
            &routes,
            w.cat,
            w.pool,
            w.log,
            w.chain,
            w.txn,
        )?;
        Ok(outcome)
    })?;
    Ok(CreateTableOutcome {
        commit_seq: seq,
        ..outcome
    })
}

/// Add the graph-owned key tree to an older graph heap. Call only outside a
/// user transaction, before its first automatic-commit write. Read-only graph
/// queries never perform this DDL or mutate the old snapshot format.
pub fn ensure_graph_storage_index(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    name: &str,
) -> Result<(), DdlError> {
    let (obj, kind) = object_ref(cat, name)?;
    if kind != dict::obj_kind::GRAPH {
        return Err(DdlError::WrongKind(name.into()));
    }
    for index in index_objects_of(cat, obj)? {
        let (_, unique, columns) = index_definition(cat, index)?;
        if unique && columns == [1] {
            return Ok(());
        }
    }
    let spec = IndexSpec {
        name: format!("i_graph_{obj}$"),
        table: name.into(),
        unique: true,
        columns: vec!["ordinal".into()],
    };
    let segment = live_segment_block(cat, obj)?;
    with_ddl_txn(cat, engine, |writer| {
        create_index_inner(writer, &spec, obj, &[1], segment)
    })?;
    Ok(())
}

fn create_table_kind(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    spec: &TableSpec,
    kind: u32,
) -> Result<CreateTableOutcome, DdlError> {
    validate_new_table(cat, spec)?;
    let (outcome, seq) = with_ddl_txn(cat, engine, |w| {
        create_table_kind_inner(w, spec, kind).map(|(outcome, _)| outcome)
    })?;
    Ok(CreateTableOutcome {
        commit_seq: seq,
        ..outcome
    })
}

fn validate_new_table(cat: &mut Catalog<'_>, spec: &TableSpec) -> Result<(), DdlError> {
    check_user_name(&spec.name)?;
    validate_table_spec(spec)?;
    // 快速失败（真正的闸门是 `i_obj_name` 唯一索引——见模块文档）。
    if object_exists(cat, &spec.name)? {
        return Err(DdlError::AlreadyExists(spec.name.clone()));
    }
    Ok(())
}

fn create_table_kind_inner(
    w: &mut DictWriter<'_, '_, '_, '_, '_>,
    spec: &TableSpec,
    kind: u32,
) -> Result<(CreateTableOutcome, Option<GraphPhysicalRoute>), DdlError> {
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
    w.insert_obj(obj, &spec.name, dict::namespace::TABLE, kind)?;
    w.insert_tab(obj, spec.columns.len() as u32, &spec.options)?;
    w.insert_cols(obj, &spec.columns)?;
    w.insert_seg(dataobj, seg_block)?;
    w.insert_stat(obj, 0)?; // 空表：行数估计 0（后续由统计路径刷新）
    let mut primary = None;
    if kind == dict::obj_kind::GRAPH {
        // A reserved, graph-owned unique key tree is created in the same
        // DDL transaction. Ordinary CREATE/DROP INDEX cannot manage it.
        let index_obj = allocate_obj_number(w, None)?;
        let block = create_index_object(w, index_obj, obj, &format!("i_graph_{obj}$"), &[1], true)?;
        primary = Some(GraphPhysicalRoute {
            obj: index_obj,
            block,
        });
        for kind in [
            dict::index_kind::GRAPH_NODES,
            dict::index_kind::GRAPH_OUT,
            dict::index_kind::GRAPH_IN,
        ] {
            create_graph_access_tree(w, obj, kind, &[])?;
        }
    }
    Ok((
        CreateTableOutcome {
            obj,
            dataobj,
            seg_block,
            commit_seq: 0, // 提交后填
        },
        primary,
    ))
}

/// **活对象的段头块**（`obj$` → `dataobj#` → `seg$.block_id`）。
///
/// **统一路径**：自举表（引导页权威）、`stat$`/`seq$`（DDL 建）、用户表/索引
/// 都经 `seg$` 取段头——同一份字典事实，不搞第二套映射。
pub fn live_segment_block(cat: &mut Catalog<'_>, obj: u32) -> Result<u32, DdlError> {
    Ok(live_segment_location(cat, obj)?.1)
}

/// **按对象取活段的（文件号, 段头块）**（`seg$.file_id` / `seg$.block_id`）。
///
/// 与 [`live_segment_block`] 同一趟查找，只是把文件号也带出来——**索引扫描**
/// 要它（`IndexScan` 的 `file_id` 就是"索引段在哪个文件"）。
pub fn live_segment_location(cat: &mut Catalog<'_>, obj: u32) -> Result<(u16, u32), DdlError> {
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
    let file_id = match seg_row.1.get(1) {
        Some(DictValue::Num(f)) => u16::try_from(*f)
            .map_err(|_| DdlError::BadTableDef(format!("seg$.file_id {f} 超出 16 位")))?,
        _ => return Err(DdlError::BadTableDef("seg$.file_id 形态非法".to_owned())),
    };
    match seg_row.1.get(2) {
        Some(DictValue::Num(b)) => Ok((file_id, *b as u32)),
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
    create_index_object_with_source(w, obj, base_obj, name, col_numbers, unique, None)
}

#[allow(clippy::too_many_arguments)]
fn create_index_object_with_source(
    w: &mut DictWriter<'_, '_, '_, '_, '_>,
    obj: u32,
    base_obj: u32,
    name: &str,
    col_numbers: &[u32],
    unique: bool,
    graph_source: Option<(u32, &[u8])>,
) -> Result<u32, DdlError> {
    let dataobj = obj;
    let seg_block = create_empty_index_segment(w, obj)?;
    w.insert_obj(obj, name, dict::namespace::INDEX, dict::obj_kind::INDEX)?;
    w.insert_index_rows(obj, base_obj, unique, col_numbers, graph_source)?;
    w.insert_seg(dataobj, seg_block)?;
    Ok(seg_block)
}

fn create_empty_index_segment(
    w: &mut DictWriter<'_, '_, '_, '_, '_>,
    obj: u32,
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
    Ok(seg_block)
}

/// Native graph expression-tree creation/rebuild outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphIndexOutcome {
    /// Stable index object ID.
    pub obj: u32,
    /// Number of current element entries.
    pub entries: usize,
    /// DDL commit watermark.
    pub commit_seq: u64,
}

fn graph_index_identity(
    cat: &mut Catalog<'_>,
    name: &str,
    graph: &str,
) -> Result<(u32, u32), DdlError> {
    let (base, kind) = object_ref(cat, graph)?;
    if kind != dict::obj_kind::GRAPH {
        return Err(DdlError::WrongKind(graph.into()));
    }
    let obj = object_number_ns(cat, name, dict::namespace::INDEX)?;
    let index = cat
        .indexes_of(
            CommitSeq::from_raw(cat.current_seq()).expect("commit sequence"),
            base,
        )
        .map_err(|e| DdlError::BadIndexDef(e.to_string()))?
        .into_iter()
        .find(|i| i.obj == obj && i.kind == dict::index_kind::GRAPH_PROPERTY);
    if index.is_none() {
        return Err(DdlError::BadIndexDef(
            "index is not a property index of this graph".into(),
        ));
    }
    Ok((obj, base))
}

fn build_graph_tree(
    w: &mut DictWriter<'_, '_, '_, '_, '_>,
    obj: u32,
    base: u32,
    name: &str,
    source: &[u8],
    unique: bool,
    entries: &[(Vec<u8>, u64)],
) -> Result<(), DdlError> {
    let block = create_index_object_with_source(
        w,
        obj,
        base,
        name,
        &[0],
        unique,
        Some((dict::index_kind::GRAPH_PROPERTY, source)),
    )?;
    fill_graph_tree(w, block, entries, unique)?;
    w.insert_stat(obj, entries.len() as u64)?;
    Ok(())
}

fn fill_graph_tree(
    w: &mut DictWriter<'_, '_, '_, '_, '_>,
    block: u32,
    entries: &[(Vec<u8>, u64)],
    unique: bool,
) -> Result<(), DdlError> {
    let mut prepared = Vec::with_capacity(entries.len());
    for (key, id) in entries {
        w.cat.check_ddl_deadline()?;
        if *id == 0 || *id >= 1 << 48 || key.len() > bicdb_index::MAX_KEY_LEN {
            return Err(DdlError::BadIndexDef("invalid graph index entry".into()));
        }
        let bytes = id.to_le_bytes();
        let payload = RowId::from_bytes(bytes[..6].try_into().expect("element ID"));
        prepared.push((key.clone(), payload));
    }
    prepared.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    bicdb_txn::write::checkpoint_safe_point(w.pool, w.log, w.chain)?;
    let ws = w.cat.ws();
    acc_index::build_index_checkpointed(
        w.pool,
        w.log,
        w.chain,
        w.cat.file_mut(),
        ws,
        block,
        w.txn,
        &prepared,
        unique,
    )?;
    Ok(())
}

/// Create a graph-owned property B-tree atomically with its metadata. Source is
/// a validated graph index descriptor; entries contain typed keys and IDs.
#[allow(clippy::too_many_arguments)]
pub fn create_graph_property_index(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    name: &str,
    graph: &str,
    source: &[u8],
    unique: bool,
    entries: &[(Vec<u8>, u64)],
) -> Result<GraphIndexOutcome, DdlError> {
    check_user_name(name)?;
    if source.is_empty() || source.len() > 4096 {
        return Err(DdlError::BadIndexDef("invalid graph index source".into()));
    }
    if object_exists_ns(cat, name, dict::namespace::INDEX)? {
        return Err(DdlError::AlreadyExists(name.into()));
    }
    let (base, kind) = object_ref(cat, graph)?;
    if kind != dict::obj_kind::GRAPH {
        return Err(DdlError::WrongKind(graph.into()));
    }
    let (obj, seq) = with_ddl_txn(cat, engine, |w| {
        let obj = allocate_obj_number(w, None)?;
        build_graph_tree(w, obj, base, name, source, unique, entries)?;
        Ok(obj)
    })?;
    Ok(GraphIndexOutcome {
        obj,
        entries: entries.len(),
        commit_seq: seq,
    })
}

/// Rebuild into a fresh segment in one DDL transaction. Failed builds restore
/// the old dictionary routing and tree; successful builds remove stale entries.
pub fn rebuild_graph_property_index(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    name: &str,
    graph: &str,
    source: &[u8],
    unique: bool,
    entries: &[(Vec<u8>, u64)],
) -> Result<GraphIndexOutcome, DdlError> {
    let (obj, _) = graph_index_identity(cat, name, graph)?;
    let key = crate::open::comp_num(u64::from(obj));
    let (_, definition) = cat
        .lookup("i_ind_pk", &[Some(&key)])?
        .ok_or_else(|| DdlError::NotFound(name.into()))?;
    if definition.get(4) != Some(&DictValue::Bool(unique))
        || definition.get(6) != Some(&DictValue::Bytes(source.to_vec()))
    {
        return Err(DdlError::BadIndexDef(
            "REBUILD cannot change a graph index definition".into(),
        ));
    }
    let (_, seq) = with_ddl_txn(cat, engine, |w| {
        reset_graph_tree(w, obj, entries, unique)?;
        Ok(())
    })?;
    Ok(GraphIndexOutcome {
        obj,
        entries: entries.len(),
        commit_seq: seq,
    })
}

fn reset_graph_tree(
    w: &mut DictWriter<'_, '_, '_, '_, '_>,
    obj: u32,
    entries: &[(Vec<u8>, u64)],
    unique: bool,
) -> Result<(), DdlError> {
    // Keep dictionary key rows intact: their B-tree delete has no undo.
    // Build a fresh segment, then change only undo-protected non-key values.
    let block = create_empty_index_segment(w, obj)?;
    fill_graph_tree(w, block, entries, unique)?;
    set_index_status(w, obj, 1)?;
    switch_graph_segment(w, obj, block, entries.len())
}

fn switch_graph_segment(
    w: &mut DictWriter<'_, '_, '_, '_, '_>,
    obj: u32,
    block: u32,
    rows: usize,
) -> Result<(), DdlError> {
    let key = crate::open::comp_num(u64::from(obj));
    let (def, seg_block) = w.table("seg$")?;
    let (rid, mut values) = w
        .cat
        .lookup("i_seg_pk", &[Some(&key)])?
        .ok_or_else(|| DdlError::NotFound(obj.to_string()))?;
    values[2] = DictValue::Num(u64::from(block));
    values[4] = DictValue::Num(w.seq.as_raw());
    w.update_row_nonkey("seg$", seg_block, def, rid, &values)?;
    w.write_through_seg(&values)?;
    let (def, stat_block) = w.table("stat$")?;
    let (rid, mut values) = w
        .cat
        .lookup("i_stat_pk", &[Some(&key)])?
        .ok_or_else(|| DdlError::NotFound(obj.to_string()))?;
    values[1] = DictValue::Num(rows as u64);
    values[4] = DictValue::Num(w.seq.as_raw());
    w.update_row_nonkey("stat$", stat_block, def, rid, &values)?;
    set_object_status(w, obj, 1)?;
    Ok(())
}

/// Prepared native full-text image; SQL validates the versioned document codec.
pub struct GraphFulltextBuild<'a> {
    /// Versioned, validated full-text definition.
    pub source: &'a [u8],
    /// Domain/field/channel/term/revision keys with element-ID payloads.
    pub entries: &'a [(Vec<u8>, u64)],
    /// Checksummed document records, keyed by native ordinal.
    pub rows: &'a std::collections::BTreeMap<u64, Vec<u8>>,
}

/// Protected document heap name derived from its owning stable index ID.
pub fn graph_fulltext_store_name(index: u32) -> String {
    format!("gft_{index}$")
}

/// Protected graph-wide lightweight full-text change journal.
pub fn graph_fulltext_journal_name(graph: u32) -> String {
    format!("gftq_{graph}$")
}
type NativeRecordRows = std::collections::BTreeMap<u64, Vec<u8>>;
type JournalBuilder<'a> = dyn FnMut(u32) -> Result<NativeRecordRows, DdlError> + 'a;

fn replace_graph_fulltext_journal(
    w: &mut DictWriter<'_, '_, '_, '_, '_>,
    graph: u32,
    rows: &NativeRecordRows,
) -> Result<(), DdlError> {
    if rows.is_empty() {
        return Err(DdlError::BadIndexDef("empty full-text journal".into()));
    }
    let name = graph_fulltext_journal_name(graph);
    if !object_exists_ns(w.cat, &name, dict::namespace::TABLE)? {
        let data = allocate_obj_number(w, None)?;
        let options = TableOptions::default();
        let heap = create_table_segment(
            w.cat,
            w.pool,
            w.log,
            w.txn,
            SegType::Heap,
            data,
            data,
            &options,
        )?;
        w.insert_obj(
            data,
            &name,
            dict::namespace::TABLE,
            dict::obj_kind::GRAPH_FULLTEXT_QUEUE,
        )?;
        w.insert_tab(data, 2, &options)?;
        w.insert_cols(data, &fulltext_columns())?;
        w.insert_seg(data, heap)?;
        w.insert_stat(data, rows.len() as u64)?;
        let key = allocate_obj_number(w, None)?;
        let tree = create_index_object(w, key, data, &format!("i_gftq_{graph}$"), &[1], true)?;
        w.insert_stat(key, rows.len() as u64)?;
        fill_fulltext_heap(w, heap, tree, rows)?;
    } else {
        let (data, kind) = object_ref(w.cat, &name)?;
        if kind != dict::obj_kind::GRAPH_FULLTEXT_QUEUE {
            return Err(DdlError::WrongKind(name));
        }
        let keys = w
            .cat
            .indexes_of(
                CommitSeq::from_raw(w.cat.current_seq()).expect("sequence"),
                data,
            )
            .map_err(|e| DdlError::BadIndexDef(e.to_string()))?;
        if keys.len() != 1
            || keys[0].kind != dict::index_kind::BTREE
            || !keys[0].is_unique
            || keys[0].cols.len() != 1
            || keys[0].cols[0].col != 1
        {
            return Err(DdlError::BadIndexDef(
                "invalid full-text journal tree".into(),
            ));
        }
        let key = keys[0].obj;
        let heap = create_table_segment(
            w.cat,
            w.pool,
            w.log,
            w.txn,
            SegType::Heap,
            data,
            data,
            &TableOptions::default(),
        )?;
        let tree = create_empty_index_segment(w, key)?;
        fill_fulltext_heap(w, heap, tree, rows)?;
        switch_graph_segment(w, key, tree, rows.len())?;
        set_index_status(w, key, 1)?;
        switch_graph_segment(w, data, heap, rows.len())?;
    }
    Ok(())
}
fn drop_graph_fulltext_journal(
    w: &mut DictWriter<'_, '_, '_, '_, '_>,
    graph: u32,
) -> Result<(), DdlError> {
    let name = graph_fulltext_journal_name(graph);
    if !object_exists_ns(w.cat, &name, dict::namespace::TABLE)? {
        return Ok(());
    }
    let (data, kind) = object_ref(w.cat, &name)?;
    if kind != dict::obj_kind::GRAPH_FULLTEXT_QUEUE {
        return Err(DdlError::WrongKind(name));
    }
    for index in index_objects_of(w.cat, data)? {
        let index_name = object_name(w.cat, index)?;
        drop_index_rows(w, index, &index_name)?;
    }
    drop_table_rows(w, data, &name)
}

fn fulltext_identity(cat: &mut Catalog<'_>, name: &str, graph: &str) -> Result<u32, DdlError> {
    let (base, kind) = object_ref(cat, graph)?;
    if kind != dict::obj_kind::GRAPH {
        return Err(DdlError::WrongKind(graph.into()));
    }
    let obj = object_number_ns(cat, name, dict::namespace::INDEX)?;
    if !cat
        .indexes_of(
            CommitSeq::from_raw(cat.current_seq()).expect("sequence"),
            base,
        )
        .map_err(|e| DdlError::BadIndexDef(e.to_string()))?
        .iter()
        .any(|i| i.obj == obj && i.kind == dict::index_kind::GRAPH_FULLTEXT)
    {
        return Err(DdlError::BadIndexDef(
            "not a full-text index of this graph".into(),
        ));
    }
    Ok(obj)
}

fn fulltext_columns() -> Vec<ColumnSpec> {
    [
        ("ordinal", ColTypeCode::Number, 0),
        ("data", ColTypeCode::Bytes, 4096),
    ]
    .into_iter()
    .map(|(name, type_code, length)| ColumnSpec {
        name: name.into(),
        type_code,
        length,
        precision: None,
        scale: None,
        nullable: false,
    })
    .collect()
}

fn fill_fulltext_heap(
    w: &mut DictWriter<'_, '_, '_, '_, '_>,
    heap: u32,
    key_tree: u32,
    rows: &std::collections::BTreeMap<u64, Vec<u8>>,
) -> Result<(), DdlError> {
    let columns = [
        dict::ColDef {
            col: 1,
            name: "ordinal",
            type_code: ColTypeCode::Number,
            length: 0,
            nullable: false,
        },
        dict::ColDef {
            col: 2,
            name: "data",
            type_code: ColTypeCode::Bytes,
            length: 4096,
            nullable: false,
        },
    ];
    let mut bytes = 0usize;
    let mut keys = Vec::with_capacity(rows.len());
    let ws = w.ws();
    for (ordinal, data) in rows {
        w.cat.check_ddl_deadline()?;
        bytes = bytes.saturating_add(data.len());
        if data.len() > 4096 || bytes > 100 * 1024 * 1024 {
            return Err(DdlError::BadIndexDef(
                "full-text record byte budget exceeded".into(),
            ));
        }
        bicdb_txn::write::checkpoint_safe_point(w.pool, w.log, w.chain)?;
        let encoded = row::encode(
            &[DictValue::Num(*ordinal), DictValue::Bytes(data.clone())],
            &columns,
        )?;
        let rid = TableAccess::new(w.pool, ws).insert(
            w.log,
            w.chain,
            w.txn,
            w.cat.file_mut(),
            heap,
            &encoded,
            &InsertPolicy::in_place(0),
        )?;
        let key = row::key_from_row(&encoded, &[0])?;
        keys.push((key, rid));
    }
    keys.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    w.cat.check_ddl_deadline()?;
    bicdb_txn::write::checkpoint_safe_point(w.pool, w.log, w.chain)?;
    acc_index::build_index_checkpointed(
        w.pool,
        w.log,
        w.chain,
        w.cat.file_mut(),
        ws,
        key_tree,
        w.txn,
        &keys,
        true,
    )?;
    Ok(())
}

/// Create postings, protected document heap and ordinal tree in one native DDL transaction.
pub fn create_graph_fulltext_index(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    name: &str,
    graph: &str,
    build: &GraphFulltextBuild<'_>,
) -> Result<GraphIndexOutcome, DdlError> {
    create_graph_fulltext_index_inner(cat, engine, name, graph, build, None)
}

/// Atomically create a full-text index and register its stable allocated ID in
/// the graph journal. The callback is a pure prepared-image builder.
pub fn create_graph_fulltext_index_with_journal(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    name: &str,
    graph: &str,
    build: &GraphFulltextBuild<'_>,
    mut journal: impl FnMut(u32) -> Result<NativeRecordRows, DdlError>,
) -> Result<GraphIndexOutcome, DdlError> {
    create_graph_fulltext_index_inner(cat, engine, name, graph, build, Some(&mut journal))
}

fn create_graph_fulltext_index_inner(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    name: &str,
    graph: &str,
    build: &GraphFulltextBuild<'_>,
    mut journal: Option<&mut JournalBuilder<'_>>,
) -> Result<GraphIndexOutcome, DdlError> {
    check_user_name(name)?;
    if build.source.is_empty() || build.source.len() > 4096 || build.rows.is_empty() {
        return Err(DdlError::BadIndexDef("invalid full-text image".into()));
    }
    if object_exists_ns(cat, name, dict::namespace::INDEX)? {
        return Err(DdlError::AlreadyExists(name.into()));
    }
    let (base, kind) = object_ref(cat, graph)?;
    if kind != dict::obj_kind::GRAPH {
        return Err(DdlError::WrongKind(graph.into()));
    }
    let (obj, seq) = with_ddl_txn(cat, engine, |w| {
        let obj = allocate_obj_number(w, None)?;
        let block = create_index_object_with_source(
            w,
            obj,
            base,
            name,
            &[0],
            false,
            Some((dict::index_kind::GRAPH_FULLTEXT, build.source)),
        )?;
        fill_graph_tree(w, block, build.entries, false)?;
        w.insert_stat(obj, build.entries.len() as u64)?;
        let data = allocate_obj_number(w, None)?;
        let data_name = graph_fulltext_store_name(obj);
        let options = TableOptions::default();
        let heap = create_table_segment(
            w.cat,
            w.pool,
            w.log,
            w.txn,
            SegType::Heap,
            data,
            data,
            &options,
        )?;
        w.insert_obj(
            data,
            &data_name,
            dict::namespace::TABLE,
            dict::obj_kind::GRAPH_FULLTEXT_DATA,
        )?;
        w.insert_tab(data, 2, &options)?;
        w.insert_cols(data, &fulltext_columns())?;
        w.insert_seg(data, heap)?;
        w.insert_stat(data, build.rows.len() as u64)?;
        let key_obj = allocate_obj_number(w, None)?;
        let key_block =
            create_index_object(w, key_obj, data, &format!("i_gft_{obj}$"), &[1], true)?;
        w.insert_stat(key_obj, build.rows.len() as u64)?;
        fill_fulltext_heap(w, heap, key_block, build.rows)?;
        if let Some(builder) = journal.as_mut() {
            let rows = builder(obj)?;
            replace_graph_fulltext_journal(w, base, &rows)?;
        }

        Ok(obj)
    })?;
    Ok(GraphIndexOutcome {
        obj,
        entries: build.entries.len(),
        commit_seq: seq,
    })
}

/// Replace all three physical segments; only commit publishes their stable routes.
pub fn rebuild_graph_fulltext_index(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    name: &str,
    graph: &str,
    build: &GraphFulltextBuild<'_>,
) -> Result<GraphIndexOutcome, DdlError> {
    rebuild_graph_fulltext_index_inner(cat, engine, name, graph, build, None)
}

/// Publish fresh full-text segments and the rebuilt consumer watermark atomically.
pub fn rebuild_graph_fulltext_index_with_journal(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    name: &str,
    graph: &str,
    build: &GraphFulltextBuild<'_>,
    journal: &NativeRecordRows,
) -> Result<GraphIndexOutcome, DdlError> {
    rebuild_graph_fulltext_index_inner(cat, engine, name, graph, build, Some(journal))
}

fn rebuild_graph_fulltext_index_inner(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    name: &str,
    graph: &str,
    build: &GraphFulltextBuild<'_>,
    journal: Option<&NativeRecordRows>,
) -> Result<GraphIndexOutcome, DdlError> {
    let obj = fulltext_identity(cat, name, graph)?;
    let base = object_number(cat, graph)?;
    let index = cat
        .indexes_of(
            CommitSeq::from_raw(cat.current_seq()).expect("sequence"),
            base,
        )
        .map_err(|e| DdlError::BadIndexDef(e.to_string()))?
        .into_iter()
        .find(|i| i.obj == obj)
        .expect("identity");
    if index.expr_src.as_deref() != Some(build.source) || build.rows.is_empty() {
        return Err(DdlError::BadIndexDef(
            "REBUILD cannot change full-text definition".into(),
        ));
    }
    let (data, kind) = object_ref(cat, &graph_fulltext_store_name(obj))?;
    if kind != dict::obj_kind::GRAPH_FULLTEXT_DATA {
        return Err(DdlError::WrongKind(name.into()));
    }
    let keys = cat
        .indexes_of(
            CommitSeq::from_raw(cat.current_seq()).expect("sequence"),
            data,
        )
        .map_err(|e| DdlError::BadIndexDef(e.to_string()))?;
    if keys.len() != 1
        || keys[0].kind != dict::index_kind::BTREE
        || !keys[0].is_unique
        || keys[0].cols.len() != 1
        || keys[0].cols[0].col != 1
    {
        return Err(DdlError::BadIndexDef(
            "invalid full-text record tree".into(),
        ));
    }
    let key_obj = keys[0].obj;
    let (_, seq) = with_ddl_txn(cat, engine, |w| {
        let heap = create_table_segment(
            w.cat,
            w.pool,
            w.log,
            w.txn,
            SegType::Heap,
            data,
            data,
            &TableOptions::default(),
        )?;
        let key_tree = create_empty_index_segment(w, key_obj)?;
        fill_fulltext_heap(w, heap, key_tree, build.rows)?;
        reset_graph_tree(w, obj, build.entries, false)?;
        switch_graph_segment(w, key_obj, key_tree, build.rows.len())?;
        set_index_status(w, key_obj, 1)?;
        switch_graph_segment(w, data, heap, build.rows.len())?;
        if let Some(rows) = journal {
            replace_graph_fulltext_journal(w, base, rows)?;
        }

        Ok(())
    })?;
    Ok(GraphIndexOutcome {
        obj,
        entries: build.entries.len(),
        commit_seq: seq,
    })
}

fn drop_fulltext_store(w: &mut DictWriter<'_, '_, '_, '_, '_>, index: u32) -> Result<(), DdlError> {
    let name = graph_fulltext_store_name(index);
    let (data, kind) = object_ref(w.cat, &name)?;
    if kind != dict::obj_kind::GRAPH_FULLTEXT_DATA {
        return Err(DdlError::WrongKind(name));
    }
    for key in index_objects_of(w.cat, data)? {
        let key_name = object_name(w.cat, key)?;
        drop_index_rows(w, key, &key_name)?;
    }
    drop_table_rows(w, data, &name)
}

/// Replace a validated full-text descriptor using a non-key dictionary update.
/// The expected descriptor is compared before publication. SQL callers preserve
/// the text definition and alter only maintenance policy; no tree is rebuilt.
///
/// # Errors
/// Reject wrong ownership, invalid lengths or a descriptor changed by the caller.
pub fn alter_graph_fulltext_source(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    name: &str,
    graph: &str,
    expected: &[u8],
    source: &[u8],
) -> Result<u64, DdlError> {
    if source.is_empty() || source.len() > 4096 {
        return Err(DdlError::BadIndexDef(
            "invalid full-text descriptor size".into(),
        ));
    }
    let obj = fulltext_identity(cat, name, graph)?;
    let (_, seq) = with_ddl_txn(cat, engine, |w| {
        let key = crate::open::comp_num(u64::from(obj));
        let (rid, mut values) = w
            .cat
            .lookup("i_ind_pk", &[Some(&key)])?
            .ok_or_else(|| DdlError::NotFound(name.into()))?;
        if values[6] != DictValue::Bytes(expected.to_vec()) {
            return Err(DdlError::BadIndexDef("full-text descriptor changed".into()));
        }
        let (def, block) = w.table("ind$")?;
        values[6] = DictValue::Bytes(source.to_vec());
        w.update_row_nonkey("ind$", block, def, rid, &values)?;
        set_object_status(w, obj, 1)?;
        Ok(())
    })?;
    Ok(seq)
}

/// Drop only a full-text index owned by the stated graph, including its document heap.
pub fn drop_graph_fulltext_index(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    name: &str,
    graph: &str,
) -> Result<DropOutcome, DdlError> {
    drop_graph_fulltext_index_inner(cat, engine, name, graph, None)
}

/// Remove the index and unregister its graph journal consumer in one DDL transaction.
pub fn drop_graph_fulltext_index_with_journal(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    name: &str,
    graph: &str,
    journal: &NativeRecordRows,
) -> Result<DropOutcome, DdlError> {
    drop_graph_fulltext_index_inner(cat, engine, name, graph, Some(journal))
}

fn drop_graph_fulltext_index_inner(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    name: &str,
    graph: &str,
    journal: Option<&NativeRecordRows>,
) -> Result<DropOutcome, DdlError> {
    let obj = fulltext_identity(cat, name, graph)?;
    let (out, _) = with_ddl_txn(cat, engine, |w| {
        drop_fulltext_store(w, obj)?;
        drop_index_rows(w, obj, name)?;
        if let Some(rows) = journal {
            let base = object_number(w.cat, graph)?;
            replace_graph_fulltext_journal(w, base, rows)?;
        }

        Ok(DropOutcome {
            obj,
            indexes: vec![],
        })
    })?;
    Ok(out)
}

/// Descriptor for a protected directory/adjacency tree; not a property expression.
pub fn graph_access_descriptor(kind: u32) -> Option<&'static [u8]> {
    match kind {
        dict::index_kind::GRAPH_NODES => Some(b"bicdb-graph-access-v1:nodes"),
        dict::index_kind::GRAPH_OUT => Some(b"bicdb-graph-access-v1:out"),
        dict::index_kind::GRAPH_IN => Some(b"bicdb-graph-access-v1:in"),
        _ => None,
    }
}

/// Descriptor for a v3 physical route. These are graph-owned companions, not
/// property expressions. Adjacency authority is a segment, never a B-tree.
pub fn graph_physical_descriptor(kind: u32) -> Option<&'static [u8]> {
    match kind {
        dict::index_kind::GRAPH_ADJACENCY => Some(b"bicdb-graph-physical-v3:adjacency"),
        dict::index_kind::GRAPH_SOURCE_ENTRY => Some(b"bicdb-graph-physical-v3:source"),
        dict::index_kind::GRAPH_EDGE_LOCATOR => Some(b"bicdb-graph-physical-v3:locator"),
        dict::index_kind::GRAPH_REVERSE => Some(b"bicdb-graph-physical-v3:reverse"),
        _ => None,
    }
}
/// One graph-owned physical object and its current native segment header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraphPhysicalRoute {
    /// Dictionary object ID, owned by `GraphPhysicalRoutes::graph`.
    pub obj: u32,
    /// Segment-header block in the graph's workspace datafile.
    pub block: u32,
}
/// Complete route set for the v3 SQL adapter. Presence alone does not mean a
/// v2 graph has migrated: its transactional graph manifest chooses authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraphPhysicalRoutes {
    /// Logical named graph dictionary object ID.
    pub graph: u32,
    /// Type-3 authoritative adjacency segment.
    pub adjacency: GraphPhysicalRoute,
    /// Physical source -> snapshot metadata row B-tree.
    pub source: GraphPhysicalRoute,
    /// Logical edge ID -> stable edge ROWID B-tree.
    pub locator: GraphPhysicalRoute,
    /// Physical incoming/type/source/edge candidate B-tree.
    pub incoming: GraphPhysicalRoute,
}
fn physical_route_suffix(kind: u32) -> &'static str {
    match kind {
        dict::index_kind::GRAPH_ADJACENCY => "adj",
        dict::index_kind::GRAPH_SOURCE_ENTRY => "source",
        dict::index_kind::GRAPH_EDGE_LOCATOR => "locator",
        _ => "reverse",
    }
}
/// Resolve and validate all four routes without writing. Incomplete, duplicate,
/// forged-descriptor or wrong-segment sets fail rather than silently rebuilding.
pub fn graph_physical_routes(
    cat: &mut Catalog<'_>,
    graph: &str,
) -> Result<Option<GraphPhysicalRoutes>, DdlError> {
    let (base, kind) = object_ref(cat, graph)?;
    if kind != dict::obj_kind::GRAPH {
        return Err(DdlError::WrongKind(graph.into()));
    }
    let indexes = cat
        .indexes_of(
            CommitSeq::from_raw(cat.current_seq()).expect("sequence"),
            base,
        )
        .map_err(|e| DdlError::BadIndexDef(e.to_string()))?;
    let mut routes = std::collections::BTreeMap::new();
    for index in indexes {
        let Some(descriptor) = graph_physical_descriptor(index.kind) else {
            continue;
        };
        if index.status != 1
            || index.is_unique
            || index.cols.len() != 1
            || index.cols[0].col != 0
            || index.cols[0].pos != 1
            || index.cols[0].is_desc
            || index.expr_src.as_deref() != Some(descriptor)
        {
            return Err(DdlError::BadIndexDef(
                "invalid graph physical route descriptor".into(),
            ));
        }
        let block = live_segment_block(cat, index.obj)?;
        let segment = cat.segment_at(block)?;
        let expected = if index.kind == dict::index_kind::GRAPH_ADJACENCY {
            SegType::Adjacency
        } else {
            SegType::BTree
        };
        if segment.header().seg_type != expected
            || segment.header().obj != index.obj
            || segment.header().dataobj != index.obj
        {
            return Err(DdlError::BadIndexDef(
                "invalid graph physical segment ownership/type".into(),
            ));
        }
        if routes
            .insert(
                index.kind,
                GraphPhysicalRoute {
                    obj: index.obj,
                    block,
                },
            )
            .is_some()
        {
            return Err(DdlError::BadIndexDef(
                "duplicate graph physical route".into(),
            ));
        }
    }
    if routes.is_empty() {
        return Ok(None);
    }
    if routes.len() != 4 {
        return Err(DdlError::BadIndexDef(
            "incomplete graph physical routes".into(),
        ));
    }
    Ok(Some(GraphPhysicalRoutes {
        graph: base,
        adjacency: routes[&dict::index_kind::GRAPH_ADJACENCY],
        source: routes[&dict::index_kind::GRAPH_SOURCE_ENTRY],
        locator: routes[&dict::index_kind::GRAPH_EDGE_LOCATOR],
        incoming: routes[&dict::index_kind::GRAPH_REVERSE],
    }))
}
/// Create the complete managed route set and run a migration/fill callback in
/// the same DDL transaction. The callback must publish authority/manifest and
/// source rows atomically; callers may also create an empty inactive route set.
/// Read-only queries never call this API. Rejects any existing physical routes.
pub fn create_graph_physical_routes(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    graph: &str,
    populate: impl FnOnce(
        &GraphPhysicalRoutes,
        &mut Catalog<'_>,
        &BufferPool<'_>,
        &mut GroupWriter<'_, '_>,
        &mut UndoChain<'_, '_>,
        &mut Txn,
    ) -> Result<(), DdlError>,
) -> Result<GraphPhysicalRoutes, DdlError> {
    if graph_physical_routes(cat, graph)?.is_some() {
        return Err(DdlError::BadIndexDef(
            "graph physical routes already exist".into(),
        ));
    }
    let (base, _) = object_ref(cat, graph)?;
    let (routes, _) = with_ddl_txn(cat, engine, |w| {
        let routes = create_graph_physical_routes_inner(w, base)?;
        populate(&routes, w.cat, w.pool, w.log, w.chain, w.txn)?;
        Ok(routes)
    })?;
    Ok(routes)
}

/// Prepare authority for an explicitly validated legacy graph migration.
/// A complete existing inactive set is replaced with fresh routes in the same
/// transaction as population. This avoids adopting redo-only candidates left
/// by an aborted migration. Invalid/incomplete/foreign sets fail validation.
/// The caller must prove that the current manifest still selects legacy data.
pub fn prepare_graph_migration_routes(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    graph: &str,
    populate: impl FnOnce(
        &GraphPhysicalRoutes,
        &mut Catalog<'_>,
        &BufferPool<'_>,
        &mut GroupWriter<'_, '_>,
        &mut UndoChain<'_, '_>,
        &mut Txn,
    ) -> Result<(), DdlError>,
) -> Result<GraphPhysicalRoutes, DdlError> {
    match graph_physical_routes(cat, graph)? {
        None => create_graph_physical_routes(cat, engine, graph, populate),
        Some(_) => rebuild_graph_physical_routes(cat, engine, graph, populate),
    }
}

/// Replace all four physical routes in one transaction. Old dictionary rows
/// remain reconstructible by CR, and old segments are retained for snapshot
/// readers. The callback republishes source metadata and manifest atomically.
pub fn rebuild_graph_physical_routes(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    graph: &str,
    populate: impl FnOnce(
        &GraphPhysicalRoutes,
        &mut Catalog<'_>,
        &BufferPool<'_>,
        &mut GroupWriter<'_, '_>,
        &mut UndoChain<'_, '_>,
        &mut Txn,
    ) -> Result<(), DdlError>,
) -> Result<GraphPhysicalRoutes, DdlError> {
    let old = graph_physical_routes(cat, graph)?
        .ok_or_else(|| DdlError::BadIndexDef("graph has no physical routes to rebuild".into()))?;
    let mut objects = Vec::new();
    for route in [old.adjacency, old.source, old.locator, old.incoming] {
        objects.push((route.obj, object_name(cat, route.obj)?));
    }
    let (routes, _) = with_ddl_txn(cat, engine, |w| {
        for (obj, name) in objects {
            drop_index_rows(w, obj, &name)?;
            let key = crate::open::comp_num(u64::from(obj));
            if let Some((rid, _)) = w.cat.lookup("i_stat_pk", &[Some(&key)])? {
                let def = dict::DICT_TABLES
                    .iter()
                    .find(|table| table.name == "stat$")
                    .expect("stat$ kernel definition");
                w.delete_row("stat$", def, rid)?;
            }
        }
        let routes = create_graph_physical_routes_inner(w, old.graph)?;
        populate(&routes, w.cat, w.pool, w.log, w.chain, w.txn)?;
        w.changed.push(old.graph);
        Ok(routes)
    })?;
    Ok(routes)
}

fn create_graph_physical_routes_inner(
    w: &mut DictWriter<'_, '_, '_, '_, '_>,
    base: u32,
) -> Result<GraphPhysicalRoutes, DdlError> {
    let mut created = Vec::new();
    for kind in [
        dict::index_kind::GRAPH_ADJACENCY,
        dict::index_kind::GRAPH_SOURCE_ENTRY,
        dict::index_kind::GRAPH_EDGE_LOCATOR,
        dict::index_kind::GRAPH_REVERSE,
    ] {
        w.cat.check_ddl_deadline()?;
        let obj = allocate_obj_number(w, None)?;
        let name = format!("i_graph_{base}_{}$", physical_route_suffix(kind));
        let descriptor = graph_physical_descriptor(kind).expect("physical kind");
        let block = if kind == dict::index_kind::GRAPH_ADJACENCY {
            let block = create_table_segment(
                w.cat,
                w.pool,
                w.log,
                w.txn,
                SegType::Adjacency,
                obj,
                obj,
                &TableOptions::default(),
            )?;
            w.insert_obj(obj, &name, dict::namespace::INDEX, dict::obj_kind::INDEX)?;
            w.insert_index_rows(obj, base, false, &[0], Some((kind, descriptor)))?;
            w.insert_seg(obj, block)?;
            block
        } else {
            create_index_object_with_source(
                w,
                obj,
                base,
                &name,
                &[0],
                false,
                Some((kind, descriptor)),
            )?
        };
        w.insert_stat(obj, 0)?;
        created.push(GraphPhysicalRoute { obj, block });
    }
    let routes = GraphPhysicalRoutes {
        graph: base,
        adjacency: created[0],
        source: created[1],
        locator: created[2],
        incoming: created[3],
    };
    Ok(routes)
}

/// Input for a graph-owned native tree, built from a validated graph image.
pub struct GraphAccessBuild<'a> {
    /// GRAPH_NODES, GRAPH_OUT or GRAPH_IN.
    pub kind: u32,
    /// Ordered keys and 48-bit element IDs.
    pub entries: &'a [(Vec<u8>, u64)],
}
fn create_graph_access_tree(
    w: &mut DictWriter<'_, '_, '_, '_, '_>,
    base: u32,
    kind: u32,
    entries: &[(Vec<u8>, u64)],
) -> Result<u32, DdlError> {
    let source = graph_access_descriptor(kind)
        .ok_or_else(|| DdlError::BadIndexDef("invalid graph access kind".into()))?;
    let suffix = match kind {
        dict::index_kind::GRAPH_NODES => "nodes",
        dict::index_kind::GRAPH_OUT => "out",
        _ => "in",
    };
    let obj = allocate_obj_number(w, None)?;
    let block = create_index_object_with_source(
        w,
        obj,
        base,
        &format!("i_graph_{base}_{suffix}$"),
        &[0],
        false,
        Some((kind, source)),
    )?;
    fill_graph_tree(w, block, entries, false)?;
    w.insert_stat(obj, entries.len() as u64)?;
    Ok(obj)
}

/// Populate missing protected access trees of an older graph atomically.
/// Caller must be outside a user transaction; read-only queries never call this.
pub fn ensure_graph_access_indexes(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    graph: &str,
    builds: &[GraphAccessBuild<'_>],
) -> Result<(), DdlError> {
    let (base, kind) = object_ref(cat, graph)?;
    if kind != dict::obj_kind::GRAPH {
        return Err(DdlError::WrongKind(graph.into()));
    }
    let mut requested = std::collections::BTreeSet::new();
    for build in builds {
        if graph_access_descriptor(build.kind).is_none() || !requested.insert(build.kind) {
            return Err(DdlError::BadIndexDef(
                "invalid/duplicate access tree build".into(),
            ));
        }
    }
    if requested.len() != 3 {
        return Err(DdlError::BadIndexDef(
            "three access tree builds required".into(),
        ));
    }
    let indexes = cat
        .indexes_of(
            CommitSeq::from_raw(cat.current_seq()).expect("sequence"),
            base,
        )
        .map_err(|e| DdlError::BadIndexDef(e.to_string()))?;
    let mut existing = std::collections::BTreeSet::new();
    for i in indexes {
        if let Some(source) = graph_access_descriptor(i.kind) {
            if i.status != 1
                || i.is_unique
                || i.cols.len() != 1
                || i.cols[0].col != 0
                || i.expr_src.as_deref() != Some(source)
                || !existing.insert(i.kind)
            {
                return Err(DdlError::BadIndexDef(
                    "invalid graph access metadata".into(),
                ));
            }
        }
    }
    if existing.len() == 3 {
        return Ok(());
    }
    with_ddl_txn(cat, engine, |w| {
        for build in builds {
            if !existing.contains(&build.kind) {
                create_graph_access_tree(w, base, build.kind, build.entries)?;
            }
        }
        Ok(())
    })?;
    Ok(())
}

/// Atomically rebuild all three protected access trees, or create missing old
/// graph trees. Object identity survives; old routing is undo-protected on error.
pub fn rebuild_graph_access_indexes(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    graph: &str,
    builds: &[GraphAccessBuild<'_>],
) -> Result<(), DdlError> {
    let (base, kind) = object_ref(cat, graph)?;
    if kind != dict::obj_kind::GRAPH {
        return Err(DdlError::WrongKind(graph.into()));
    }
    let mut requested = std::collections::BTreeSet::new();
    for b in builds {
        if graph_access_descriptor(b.kind).is_none() || !requested.insert(b.kind) {
            return Err(DdlError::BadIndexDef(
                "invalid/duplicate access build".into(),
            ));
        }
    }
    if requested.len() != 3 {
        return Err(DdlError::BadIndexDef("three access builds required".into()));
    }
    let indexes = cat
        .indexes_of(
            CommitSeq::from_raw(cat.current_seq()).expect("sequence"),
            base,
        )
        .map_err(|e| DdlError::BadIndexDef(e.to_string()))?;
    let mut existing = std::collections::BTreeMap::new();
    for i in indexes {
        if let Some(source) = graph_access_descriptor(i.kind) {
            if i.is_unique
                || i.cols.len() != 1
                || i.cols[0].col != 0
                || i.expr_src.as_deref() != Some(source)
                || existing.insert(i.kind, i.obj).is_some()
            {
                return Err(DdlError::BadIndexDef(
                    "invalid graph access metadata".into(),
                ));
            }
        }
    }
    with_ddl_txn(cat, engine, |w| {
        for b in builds {
            if let Some(obj) = existing.get(&b.kind) {
                reset_graph_tree(w, *obj, b.entries, false)?;
            } else {
                create_graph_access_tree(w, base, b.kind, b.entries)?;
            }
        }
        Ok(())
    })?;
    Ok(())
}

/// Explicit graph-index drop; ordinary DROP INDEX cannot bypass ownership.
pub fn drop_graph_property_index(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    name: &str,
    graph: &str,
) -> Result<DropOutcome, DdlError> {
    let (obj, _) = graph_index_identity(cat, name, graph)?;
    let (out, _) = with_ddl_txn(cat, engine, |w| {
        drop_index_rows(w, obj, name)?;
        Ok(DropOutcome {
            obj,
            indexes: vec![],
        })
    })?;
    Ok(out)
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
    let base_seg = live_segment_block(cat, table_obj)?;
    let (outcome, seq) = with_ddl_txn(cat, engine, |w| {
        create_index_inner(w, spec, table_obj, &col_numbers, base_seg)
    })?;
    Ok(CreateIndexOutcome {
        commit_seq: seq,
        ..outcome
    })
}

/// **建索引的内层**（同一个 DDL 事务里做事；`create_index` 与 `rebuild_index` 共用）。
#[allow(clippy::too_many_arguments)]
fn create_index_inner(
    w: &mut DictWriter<'_, '_, '_, '_, '_>,
    spec: &IndexSpec,
    table_obj: u32,
    col_numbers: &[u32],
    base_seg: u32,
) -> Result<CreateIndexOutcome, DdlError> {
    let obj = allocate_obj_number(w, None)?;
    // ① 建索引对象（段 + 空树 + obj$/ind$/icol$/seg$ 行）。
    let seg_block = create_index_object(w, obj, table_obj, &spec.name, col_numbers, spec.unique)?;
    // ② 扫描基表求键（**当前已提交状态**——单写者下池即真值）。
    let ordinals: Vec<usize> = col_numbers
        .iter()
        .map(|cn| usize::from(*cn as u16) - 1)
        .collect();
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
                // **键 = 行内列字节直取**（`arch/06` §6.0："索引比较即字节比较"）：
                // 不经 `DictValue`——字典的值模型只承载整数 NUMBER，走那一趟会让
                // 小数/大数段的键"求不出来"（实测：`CREATE INDEX` 于含 1.5 的表报
                // "数值列越出字典域"）。
                let rk = row::row_key(bytes, &ordinals)?;
                entries.push((rk.bytes, rid, rk.has_null));
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
    drop_heap_kind(cat, engine, name, dict::obj_kind::TABLE)
}

/// Drop a named graph without permitting DROP TABLE to bypass its type.
pub fn drop_graph(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    name: &str,
) -> Result<DropOutcome, DdlError> {
    drop_heap_kind(cat, engine, name, dict::obj_kind::GRAPH)
}

fn drop_heap_kind(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    name: &str,
    expected: u32,
) -> Result<DropOutcome, DdlError> {
    let obj = object_number(cat, name)?;
    if obj < dict::obj_kind::USER_FIRST {
        return Err(DdlError::ReadOnlyDictionary(name.to_owned()));
    }
    let kind = object_kind(cat, name)?;
    if kind != expected {
        return Err(DdlError::WrongKind(name.to_owned()));
    }
    let index_objs = index_objects_of(cat, obj)?;
    for &i in &index_objs {
        let iname = object_name(cat, i)?;
        check_kind_or(cat, &iname, dict::obj_kind::INDEX)?;
    }
    let (outcome, _seq) = with_ddl_txn(cat, engine, |w| {
        for &i in &index_objs {
            let key = crate::open::comp_num(u64::from(i));
            let (_, definition) = w
                .cat
                .lookup("i_ind_pk", &[Some(&key)])?
                .ok_or_else(|| DdlError::NotFound(i.to_string()))?;
            if definition.get(2)
                == Some(&DictValue::Num(u64::from(dict::index_kind::GRAPH_FULLTEXT)))
            {
                drop_fulltext_store(w, i)?;
            }
            let iname = object_name(w.cat, i)?;
            drop_index_rows(w, i, &iname)?;
        }
        if expected == dict::obj_kind::GRAPH {
            drop_graph_fulltext_journal(w, obj)?;
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
    if obj < dict::obj_kind::USER_FIRST {
        return Err(DdlError::ReadOnlyDictionary(name.to_owned()));
    }
    let kind = object_kind_ns(cat, name, dict::namespace::INDEX)?;
    if kind != dict::obj_kind::INDEX {
        return Err(DdlError::WrongKind(name.to_owned()));
    }
    let (base, _, _) = index_definition(cat, obj)?;
    let base_name = object_name(cat, base)?;
    if matches!(
        object_ref(cat, &base_name)?.1,
        dict::obj_kind::GRAPH
            | dict::obj_kind::GRAPH_FULLTEXT_DATA
            | dict::obj_kind::GRAPH_FULLTEXT_QUEUE
    ) {
        return Err(DdlError::BadIndexDef(
            "graph storage index is managed by its graph".into(),
        ));
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

/// Enumerate visible dictionary rows; raw tree entries can outlive rolled-back DDL.
fn index_objects_of(cat: &mut Catalog<'_>, table_obj: u32) -> Result<Vec<u32>, DdlError> {
    let mut out = Vec::new();
    for (_, values) in cat.scan("ind$")? {
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
    let icol_def = w.cat.table_def("icol$")?;
    for (_k, rid) in entries {
        w.delete_row("icol$", icol_def, rid)?;
    }
    // ind$（主键点查）。
    let ind_def = w.cat.table_def("ind$")?;
    let ind_key = crate::open::comp_num(u64::from(obj));
    if let Some((rid, _)) = w.cat.lookup("i_ind_pk", &[Some(&ind_key)])? {
        w.delete_row("ind$", ind_def, rid)?;
    }
    // seg$（按 dataobj# 主键）。
    let seg_def = w.cat.table_def("seg$")?;
    let seg_key = crate::open::comp_num(u64::from(obj));
    if let Some((rid, _)) = w.cat.lookup("i_seg_pk", &[Some(&seg_key)])? {
        w.delete_row("seg$", seg_def, rid)?;
    }
    // obj$（最后删：名字/对象号自此不可解析）。
    let obj_def = w.cat.table_def("obj$")?;
    let obj_key = crate::open::comp_num(u64::from(obj));
    if let Some((rid, _)) = w.cat.lookup("i_obj_pk", &[Some(&obj_key)])? {
        w.delete_row("obj$", obj_def, rid)?;
    }
    // 缓存：登记该对象失效（提交后代数自增 ⇒ 下一次读精确失效）。
    w.changed.push(obj);
    w.cat
        .row_cache()
        .note_change_name(dict::namespace::INDEX, name);
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
    let col_def = w.cat.table_def("col$")?;
    for (_k, rid) in cols {
        w.delete_row("col$", col_def, rid)?;
    }
    // tab$。
    let tab_def = w.cat.table_def("tab$")?;
    let tab_key = crate::open::comp_num(u64::from(obj));
    if let Some((rid, _)) = w.cat.lookup("i_tab_pk", &[Some(&tab_key)])? {
        w.delete_row("tab$", tab_def, rid)?;
    }
    // seg$。
    let seg_def = w.cat.table_def("seg$")?;
    if let Some((rid, _)) = w.cat.lookup("i_seg_pk", &[Some(&tab_key)])? {
        w.delete_row("seg$", seg_def, rid)?;
    }
    // obj$。
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
pub(crate) mod tests {
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

    #[test]
    fn graph_key_tree_is_owned_protected_idempotent_and_dropped_with_graph() {
        let io: &'static MemFileIo = Box::leak(Box::new(MemFileIo::new()));
        io.add_dir("/mem");
        let rig = rig(io, "graph_tree");
        let mut cat = open_catalog(io, &rig);
        init_dictionary_tables(&mut cat, rig.engine).unwrap();
        let created = create_graph(&mut cat, rig.engine, "knowledge").unwrap();
        let indexes = cat.indexes_of(seq(cat.current_seq()), created.obj).unwrap();
        assert_eq!(indexes.len(), 4);
        assert!(indexes[0].is_unique);
        assert_eq!(indexes[0].cols.len(), 1);
        assert_eq!(indexes[0].cols[0].col, 1);
        let index_name = format!("i_graph_{}$", created.obj);
        assert!(matches!(
            drop_index(&mut cat, rig.engine, &index_name),
            Err(DdlError::BadIndexDef(_))
        ));
        assert!(matches!(
            rebuild_index(&mut cat, rig.engine, &index_name),
            Err(DdlError::BadIndexDef(_))
        ));
        let previous = cat.current_seq();
        ensure_graph_storage_index(&mut cat, rig.engine, "knowledge").unwrap();
        assert_eq!(cat.current_seq(), previous);
        let dropped = drop_graph(&mut cat, rig.engine, "knowledge").unwrap();
        assert_eq!(
            dropped.indexes,
            indexes.iter().map(|i| i.obj).collect::<Vec<_>>()
        );
        assert!(cat
            .resolve(seq(cat.current_seq()), dict::namespace::INDEX, &index_name)
            .is_err());
    }

    #[test]
    fn graph_physical_routes_are_typed_snapshot_routed_protected_and_reopenable() {
        let io: &'static MemFileIo = Box::leak(Box::new(MemFileIo::new()));
        io.add_dir("/mem");
        let rig = rig(io, "graph_physical_routes");
        let mut cat = open_catalog(io, &rig);
        init_dictionary_tables(&mut cat, rig.engine).unwrap();
        let graph = create_graph(&mut cat, rig.engine, "knowledge").unwrap();
        assert_eq!(graph_physical_routes(&mut cat, "knowledge").unwrap(), None);
        let cols = [
            dict::ColDef {
                col: 1,
                name: "ordinal",
                type_code: ColTypeCode::Number,
                length: 0,
                nullable: false,
            },
            dict::ColDef {
                col: 2,
                name: "data",
                type_code: ColTypeCode::Bytes,
                length: 4096,
                nullable: false,
            },
        ];
        let mut metadata = None;
        let routes = create_graph_physical_routes(
            &mut cat,
            rig.engine,
            "knowledge",
            |routes, cat, pool, log, chain, txn| {
                let ws = cat.ws();
                let bytes = row::encode(
                    &[DictValue::Num(4u64 << 60), DictValue::Bytes(vec![7; 32])],
                    &cols,
                )?;
                let rid = TableAccess::new(pool, ws).insert(
                    log,
                    chain,
                    txn,
                    cat.file_mut(),
                    graph.seg_block,
                    &bytes,
                    &InsertPolicy::in_place(0),
                )?;
                let root = acc_index::insert_entry(
                    pool,
                    log,
                    cat.file_mut(),
                    ws,
                    routes.source.block,
                    txn,
                    b"owner",
                    rid,
                )?;
                acc_index::write_tree_head_redo(
                    pool,
                    log,
                    cat.file_mut(),
                    ws,
                    routes.source.block,
                    txn,
                    root,
                )?;
                metadata = Some(rid);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            graph_physical_routes(&mut cat, "knowledge").unwrap(),
            Some(routes)
        );
        assert_eq!(
            cat.segment_at(routes.adjacency.block)
                .unwrap()
                .header()
                .seg_type,
            SegType::Adjacency
        );
        assert!(cat
            .graph_index_range(routes.adjacency.obj, None, None, 10)
            .is_err());
        assert_eq!(
            cat.graph_index_range(routes.source.obj, None, None, 10)
                .unwrap(),
            vec![(b"owner".to_vec(), metadata.unwrap())]
        );
        for route in [
            routes.adjacency,
            routes.source,
            routes.locator,
            routes.incoming,
        ] {
            let name = object_name(&mut cat, route.obj).unwrap();
            assert!(drop_index(&mut cat, rig.engine, &name).is_err());
        }
        assert!(create_graph_physical_routes(
            &mut cat,
            rig.engine,
            "knowledge",
            |_, _, _, _, _, _| Ok(())
        )
        .is_err());
        rig.pool.flush_workspace(WS).unwrap();
        drop(cat);
        let mut reopened = open_catalog(io, &rig);
        assert_eq!(
            graph_physical_routes(&mut reopened, "knowledge").unwrap(),
            Some(routes)
        );
        // The raw test Catalog::open does not run instance startup recovery or
        // adopt the Engine watermark; the surviving engine is authoritative.
        let view = bicdb_storage::cr::ReadView::new(seq(rig.engine.current_seq()));
        let rows = rig
            .engine
            .with_read_context(|pool, chain| {
                bicdb_storage::scan::fetch_rows_resolved(pool, chain, view, &[metadata.unwrap()])
            })
            .unwrap();
        let decoded = row::decode(&rows[0].as_ref().unwrap().1, &cols).unwrap();
        assert_eq!(
            decoded,
            vec![DictValue::Num(4u64 << 60), DictValue::Bytes(vec![7; 32])]
        );
        let mut updating = rig.engine.begin().unwrap();
        rig.engine
            .with_write_context(&mut updating, |pool, log, chain, txn| {
                let ws = reopened.ws();
                let bytes = row::encode(
                    &[DictValue::Num(4u64 << 60), DictValue::Bytes(vec![8; 32])],
                    &cols,
                )
                .unwrap();
                TableAccess::new(pool, ws)
                    .update(
                        log,
                        chain,
                        txn,
                        reopened.file_mut(),
                        graph.seg_block,
                        metadata.unwrap(),
                        &bytes,
                        &InsertPolicy::in_place(0),
                    )
                    .unwrap();
            });
        for (read_view, expected_byte) in [(view, 7), (view.with_own(Some(updating.id())), 8)] {
            let rows = rig
                .engine
                .with_read_context(|pool, chain| {
                    bicdb_storage::scan::fetch_rows_resolved(
                        pool,
                        chain,
                        read_view,
                        &[metadata.unwrap()],
                    )
                })
                .unwrap();
            assert_eq!(
                row::decode(&rows[0].as_ref().unwrap().1, &cols).unwrap()[1],
                DictValue::Bytes(vec![expected_byte; 32])
            );
        }
        rig.engine.rollback(&mut updating).unwrap();
        let rows = rig
            .engine
            .with_read_context(|pool, chain| {
                bicdb_storage::scan::fetch_rows_resolved(pool, chain, view, &[metadata.unwrap()])
            })
            .unwrap();
        assert_eq!(
            row::decode(&rows[0].as_ref().unwrap().1, &cols).unwrap()[1],
            DictValue::Bytes(vec![7; 32])
        );
        let dropped = drop_graph(&mut reopened, rig.engine, "knowledge").unwrap();
        for route in [
            routes.adjacency,
            routes.source,
            routes.locator,
            routes.incoming,
        ] {
            assert!(dropped.indexes.contains(&route.obj));
        }
        assert!(graph_physical_routes(&mut reopened, "knowledge").is_err());
    }

    #[test]
    fn graph_physical_route_callback_failure_rolls_back_all_objects_and_heap_data() {
        let io: &'static MemFileIo = Box::leak(Box::new(MemFileIo::new()));
        io.add_dir("/mem");
        let rig = rig(io, "graph_physical_failure");
        let mut cat = open_catalog(io, &rig);
        init_dictionary_tables(&mut cat, rig.engine).unwrap();
        let graph = create_graph(&mut cat, rig.engine, "knowledge").unwrap();
        let previous = cat
            .indexes_of(seq(cat.current_seq()), graph.obj)
            .unwrap()
            .iter()
            .map(|i| i.obj)
            .collect::<Vec<_>>();
        let mut written = None;
        let failure = create_graph_physical_routes(
            &mut cat,
            rig.engine,
            "knowledge",
            |routes, cat, pool, log, chain, txn| {
                let ws = cat.ws();
                let bytes = bicdb_storage::row::assemble_row(
                    0,
                    0xFF,
                    &[false],
                    &[],
                    &[b"uncommitted source metadata".as_slice()],
                )
                .unwrap();
                let rid = TableAccess::new(pool, ws).insert(
                    log,
                    chain,
                    txn,
                    cat.file_mut(),
                    graph.seg_block,
                    &bytes,
                    &InsertPolicy::in_place(0),
                )?;
                written = Some(rid);
                let root = acc_index::insert_entry(
                    pool,
                    log,
                    cat.file_mut(),
                    ws,
                    routes.locator.block,
                    txn,
                    b"edge",
                    rid,
                )?;
                acc_index::write_tree_head_redo(
                    pool,
                    log,
                    cat.file_mut(),
                    ws,
                    routes.locator.block,
                    txn,
                    root,
                )?;
                Err(DdlError::BadIndexDef(
                    "injected after physical route fill".into(),
                ))
            },
        );
        assert!(failure.is_err());
        assert_eq!(graph_physical_routes(&mut cat, "knowledge").unwrap(), None);
        assert_eq!(
            cat.indexes_of(seq(cat.current_seq()), graph.obj)
                .unwrap()
                .iter()
                .map(|i| i.obj)
                .collect::<Vec<_>>(),
            previous
        );
        let view = bicdb_storage::cr::ReadView::new(seq(cat.current_seq()));
        let rows = rig
            .engine
            .with_read_context(|pool, chain| {
                bicdb_storage::scan::fetch_rows_resolved(pool, chain, view, &[written.unwrap()])
            })
            .unwrap();
        assert!(rows[0].is_none());
        let routes =
            create_graph_physical_routes(&mut cat, rig.engine, "knowledge", |_, _, _, _, _, _| {
                Ok(())
            })
            .unwrap();
        assert_eq!(
            graph_physical_routes(&mut cat, "knowledge").unwrap(),
            Some(routes)
        );
    }

    #[test]
    fn graph_physical_partial_routes_are_rejected_without_silent_creation() {
        let io: &'static MemFileIo = Box::leak(Box::new(MemFileIo::new()));
        io.add_dir("/mem");
        let rig = rig(io, "graph_physical_partial");
        let mut cat = open_catalog(io, &rig);
        init_dictionary_tables(&mut cat, rig.engine).unwrap();
        let graph = create_graph(&mut cat, rig.engine, "knowledge").unwrap();
        with_ddl_txn(&mut cat, rig.engine, |w| {
            let obj = allocate_obj_number(w, None)?;
            create_index_object_with_source(
                w,
                obj,
                graph.obj,
                "partial$",
                &[0],
                false,
                Some((
                    dict::index_kind::GRAPH_SOURCE_ENTRY,
                    graph_physical_descriptor(dict::index_kind::GRAPH_SOURCE_ENTRY).unwrap(),
                )),
            )?;
            Ok(())
        })
        .unwrap();
        let previous = cat.current_seq();
        assert!(graph_physical_routes(&mut cat, "knowledge").is_err());
        assert!(create_graph_physical_routes(
            &mut cat,
            rig.engine,
            "knowledge",
            |_, _, _, _, _, _| Ok(())
        )
        .is_err());
        assert_eq!(cat.current_seq(), previous);
    }

    #[test]
    fn graph_access_rebuild_is_atomic_across_three_tree_routes() {
        let io: &'static MemFileIo = Box::leak(Box::new(MemFileIo::new()));
        io.add_dir("/mem");
        let rig = rig(io, "graph_access_rebuild");
        let mut cat = open_catalog(io, &rig);
        init_dictionary_tables(&mut cat, rig.engine).unwrap();
        let graph = create_graph(&mut cat, rig.engine, "knowledge").unwrap();
        let original = vec![(b"original".to_vec(), 17)];
        let builds = [
            GraphAccessBuild {
                kind: dict::index_kind::GRAPH_NODES,
                entries: &original,
            },
            GraphAccessBuild {
                kind: dict::index_kind::GRAPH_OUT,
                entries: &original,
            },
            GraphAccessBuild {
                kind: dict::index_kind::GRAPH_IN,
                entries: &original,
            },
        ];
        rebuild_graph_access_indexes(&mut cat, rig.engine, "knowledge", &builds).unwrap();
        let indexes = cat.indexes_of(seq(cat.current_seq()), graph.obj).unwrap();
        let routes: Vec<_> = indexes
            .iter()
            .filter(|i| graph_access_descriptor(i.kind).is_some())
            .map(|i| (i.obj, live_segment_block(&mut cat, i.obj).unwrap()))
            .collect();
        assert_eq!(routes.len(), 3);
        let good = vec![(b"changed".to_vec(), 39)];
        let bad = vec![(b"changed".to_vec(), 0)];
        let failed = [
            GraphAccessBuild {
                kind: dict::index_kind::GRAPH_NODES,
                entries: &good,
            },
            GraphAccessBuild {
                kind: dict::index_kind::GRAPH_OUT,
                entries: &good,
            },
            GraphAccessBuild {
                kind: dict::index_kind::GRAPH_IN,
                entries: &bad,
            },
        ];
        assert!(rebuild_graph_access_indexes(&mut cat, rig.engine, "knowledge", &failed).is_err());
        for (obj, block) in routes {
            assert_eq!(live_segment_block(&mut cat, obj).unwrap(), block);
            let rows = cat.graph_index_range(obj, None, None, 10).unwrap();
            assert_eq!(
                rows.iter()
                    .map(|(key, id)| (key.clone(), id.as_raw()))
                    .collect::<Vec<_>>(),
                original
            );
        }
        let previous = cat.current_seq();
        ensure_graph_access_indexes(&mut cat, rig.engine, "knowledge", &builds).unwrap();
        assert_eq!(cat.current_seq(), previous);
        assert!(
            rebuild_graph_access_indexes(&mut cat, rig.engine, "knowledge", &builds[..2]).is_err()
        );
    }

    #[test]
    fn fulltext_native_routes_are_stable_atomic_protected_and_graph_owned() {
        let io: &'static MemFileIo = Box::leak(Box::new(MemFileIo::new()));
        io.add_dir("/mem");
        let rig = rig(io, "fulltext_native_routes");
        let mut cat = open_catalog(io, &rig);
        init_dictionary_tables(&mut cat, rig.engine).unwrap();
        create_graph(&mut cat, rig.engine, "knowledge").unwrap();
        let rows =
            std::collections::BTreeMap::from([(0, vec![0; 32]), (1, b"old-document".to_vec())]);
        let entries = vec![(b"old-term".to_vec(), 17)];
        let source = b"validated-fulltext-definition";
        let build = GraphFulltextBuild {
            source,
            entries: &entries,
            rows: &rows,
        };
        let invalid = vec![(b"bad".to_vec(), 0)];
        assert!(create_graph_fulltext_index(
            &mut cat,
            rig.engine,
            "bad",
            "knowledge",
            &GraphFulltextBuild {
                entries: &invalid,
                ..build
            }
        )
        .is_err());
        assert!(!object_exists_ns(&mut cat, "bad", dict::namespace::INDEX).unwrap());
        let index = create_graph_fulltext_index(&mut cat, rig.engine, "words", "knowledge", &build)
            .unwrap();
        let data_name = graph_fulltext_store_name(index.obj);
        let (data, kind) = object_ref(&mut cat, &data_name).unwrap();
        assert_eq!(kind, dict::obj_kind::GRAPH_FULLTEXT_DATA);
        let key = index_objects_of(&mut cat, data).unwrap()[0];
        let routes = [index.obj, data, key].map(|obj| live_segment_block(&mut cat, obj).unwrap());
        assert!(drop_table(&mut cat, rig.engine, &data_name).is_err());
        let key_name = object_name(&mut cat, key).unwrap();
        assert!(drop_index(&mut cat, rig.engine, &key_name).is_err());
        assert!(rebuild_index(&mut cat, rig.engine, &key_name).is_err());
        assert!(rebuild_graph_fulltext_index(
            &mut cat,
            rig.engine,
            "words",
            "knowledge",
            &GraphFulltextBuild {
                entries: &invalid,
                ..build
            }
        )
        .is_err());
        for (obj, expected) in [index.obj, data, key].into_iter().zip(routes) {
            assert_eq!(live_segment_block(&mut cat, obj).unwrap(), expected);
        }
        assert_eq!(
            cat.graph_index_range(index.obj, None, None, 10).unwrap()[0].0,
            b"old-term"
        );
        let new_rows =
            std::collections::BTreeMap::from([(0, vec![1; 32]), (1, b"new-document".to_vec())]);
        let new_entries = vec![(b"new-term".to_vec(), 29)];
        // Inject a missing final metadata row: rebuilding switches the posting
        // and ordinal routes before the document route's statistics fail.
        // Native undo must restore all three routes, including the one whose
        // seg$ update already happened before the failure was detected.
        with_ddl_txn(&mut cat, rig.engine, |w| {
            let key = crate::open::comp_num(u64::from(data));
            let (rid, _) = w
                .cat
                .lookup("i_stat_pk", &[Some(&key)])?
                .expect("statistics");
            let (def, _) = w.table("stat$")?;
            w.delete_row("stat$", def, rid)
        })
        .unwrap();
        assert!(rebuild_graph_fulltext_index(
            &mut cat,
            rig.engine,
            "words",
            "knowledge",
            &GraphFulltextBuild {
                source,
                entries: &new_entries,
                rows: &new_rows
            }
        )
        .is_err());
        for (obj, previous) in [index.obj, data, key].into_iter().zip(routes) {
            assert_eq!(live_segment_block(&mut cat, obj).unwrap(), previous);
        }
        assert_eq!(
            cat.graph_index_range(index.obj, None, None, 10).unwrap()[0].0,
            b"old-term"
        );
        with_ddl_txn(&mut cat, rig.engine, |w| {
            w.insert_stat(data, rows.len() as u64)
        })
        .unwrap();
        let rebuilt = rebuild_graph_fulltext_index(
            &mut cat,
            rig.engine,
            "words",
            "knowledge",
            &GraphFulltextBuild {
                source,
                entries: &new_entries,
                rows: &new_rows,
            },
        )
        .unwrap();
        assert_eq!(rebuilt.obj, index.obj);
        for (obj, previous) in [index.obj, data, key].into_iter().zip(routes) {
            assert_ne!(live_segment_block(&mut cat, obj).unwrap(), previous);
        }
        create_graph(&mut cat, rig.engine, "other").unwrap();
        assert!(drop_graph_fulltext_index(&mut cat, rig.engine, "words", "other").is_err());
        drop_graph(&mut cat, rig.engine, "knowledge").unwrap();
        assert!(!object_exists(&mut cat, &data_name).unwrap());
        assert!(!object_exists_ns(&mut cat, "words", dict::namespace::INDEX).unwrap());
        assert!(!object_exists_ns(&mut cat, &key_name, dict::namespace::INDEX).unwrap());
    }

    #[test]
    fn fulltext_journal_registration_and_rebuild_failures_restore_all_routes() {
        let io: &'static MemFileIo = Box::leak(Box::new(MemFileIo::new()));
        io.add_dir("/mem");
        let rig = rig(io, "fulltext_journal_atomic");
        let mut cat = open_catalog(io, &rig);
        init_dictionary_tables(&mut cat, rig.engine).unwrap();
        let graph = create_graph(&mut cat, rig.engine, "knowledge").unwrap();
        let rows = NativeRecordRows::from([(0, vec![0; 32]), (1, b"documents".to_vec())]);
        let entries = vec![(b"term".to_vec(), 17)];
        let build = GraphFulltextBuild {
            source: b"validated-descriptor",
            rows: &rows,
            entries: &entries,
        };
        assert!(create_graph_fulltext_index_with_journal(
            &mut cat,
            rig.engine,
            "failed",
            "knowledge",
            &build,
            |_| Err(DdlError::BadIndexDef(
                "injected registration failure".into()
            ))
        )
        .is_err());
        assert!(!object_exists_ns(&mut cat, "failed", dict::namespace::INDEX).unwrap());
        let queue_name = graph_fulltext_journal_name(graph.obj);
        assert!(!object_exists(&mut cat, &queue_name).unwrap());
        let queue_rows = NativeRecordRows::from([(0, vec![0; 32]), (1, b"journal".to_vec())]);
        let index = create_graph_fulltext_index_with_journal(
            &mut cat,
            rig.engine,
            "words",
            "knowledge",
            &build,
            |_| Ok(queue_rows.clone()),
        )
        .unwrap();
        let (data, _) = object_ref(&mut cat, &graph_fulltext_store_name(index.obj)).unwrap();
        let key = index_objects_of(&mut cat, data).unwrap()[0];
        let (queue, kind) = object_ref(&mut cat, &queue_name).unwrap();
        assert_eq!(kind, dict::obj_kind::GRAPH_FULLTEXT_QUEUE);
        let queue_key = index_objects_of(&mut cat, queue).unwrap()[0];
        let ids = [index.obj, data, key, queue, queue_key];
        let routes: Vec<_> = ids
            .iter()
            .map(|id| live_segment_block(&mut cat, *id).unwrap())
            .collect();
        let invalid = NativeRecordRows::from([
            (0, vec![0; 32]),
            (1, b"journal".to_vec()),
            (2, vec![0; 4097]),
        ]);
        assert!(rebuild_graph_fulltext_index_with_journal(
            &mut cat,
            rig.engine,
            "words",
            "knowledge",
            &build,
            &invalid
        )
        .is_err());
        assert_eq!(
            ids.iter()
                .map(|id| live_segment_block(&mut cat, *id).unwrap())
                .collect::<Vec<_>>(),
            routes,
            "journal failure after full-text route switches must roll everything back"
        );
        assert!(drop_index(&mut cat, rig.engine, &format!("i_gftq_{}$", graph.obj)).is_err());
        assert!(drop_table(&mut cat, rig.engine, &queue_name).is_err());
        rebuild_graph_fulltext_index_with_journal(
            &mut cat,
            rig.engine,
            "words",
            "knowledge",
            &build,
            &queue_rows,
        )
        .unwrap();
        for (id, old) in ids.iter().zip(routes) {
            assert_ne!(live_segment_block(&mut cat, *id).unwrap(), old);
        }
        drop_graph(&mut cat, rig.engine, "knowledge").unwrap();
        assert!(!object_exists(&mut cat, &queue_name).unwrap());
        assert!(!object_exists_ns(
            &mut cat,
            &format!("i_gftq_{}$", graph.obj),
            dict::namespace::INDEX
        )
        .unwrap());
    }

    #[test]
    fn graph_property_rebuild_failure_restores_old_routing_and_entries() {
        let io: &'static MemFileIo = Box::leak(Box::new(MemFileIo::new()));
        io.add_dir("/mem");
        let rig = rig(io, "graph_property_rebuild");
        let mut cat = open_catalog(io, &rig);
        init_dictionary_tables(&mut cat, rig.engine).unwrap();
        let graph = create_graph(&mut cat, rig.engine, "knowledge").unwrap();
        let source = b"validated-descriptor";
        let entries = vec![(b"alpha".to_vec(), 17), (b"beta".to_vec(), 29)];
        let created = create_graph_property_index(
            &mut cat,
            rig.engine,
            "names",
            "knowledge",
            source,
            false,
            &entries,
        )
        .unwrap();
        let old_block = live_segment_block(&mut cat, created.obj).unwrap();
        let rows = cat.graph_index_range(created.obj, None, None, 10).unwrap();
        assert_eq!(
            rows.iter()
                .map(|(key, id)| (key.clone(), id.as_raw()))
                .collect::<Vec<_>>(),
            entries
        );
        // Fail after replacing metadata and inserting an entry in the new tree.
        let invalid = vec![(b"new".to_vec(), 31), (b"invalid".to_vec(), 1 << 48)];
        assert!(rebuild_graph_property_index(
            &mut cat,
            rig.engine,
            "names",
            "knowledge",
            source,
            false,
            &invalid
        )
        .is_err());
        assert_eq!(
            live_segment_block(&mut cat, created.obj).unwrap(),
            old_block
        );
        let rows = cat.graph_index_range(created.obj, None, None, 10).unwrap();
        assert_eq!(
            rows.iter()
                .map(|(key, id)| (key.clone(), id.as_raw()))
                .collect::<Vec<_>>(),
            entries
        );
        assert!(create_graph_property_index(
            &mut cat,
            rig.engine,
            "failed",
            "knowledge",
            source,
            false,
            &invalid
        )
        .is_err());
        assert!(cat
            .resolve(seq(cat.current_seq()), dict::namespace::INDEX, "failed")
            .is_err());
        assert_eq!(
            cat.indexes_of(seq(cat.current_seq()), graph.obj)
                .unwrap()
                .len(),
            5
        );
        let rebuilt = rebuild_graph_property_index(
            &mut cat,
            rig.engine,
            "names",
            "knowledge",
            source,
            false,
            &[(b"replacement".to_vec(), 39)],
        )
        .unwrap();
        assert_eq!(rebuilt.obj, created.obj);
        assert_ne!(
            live_segment_block(&mut cat, created.obj).unwrap(),
            old_block
        );
        let rows = cat.graph_index_range(created.obj, None, None, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1.as_raw(), 39);
    }

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
    pub(crate) struct Rig {
        pool: &'static BufferPool<'static>,
        pub(crate) engine: &'static Engine<'static, 'static, 'static, 'static>,
        cf_a: String,
        cf_b: String,
        wal: String,
        spec: bicdb_wal::group::GroupSpec,
    }

    pub(super) fn rig(io: &'static MemFileIo, tag: &str) -> Rig {
        rig_with(io, tag, false)
    }

    /// 同上，但可指定**是不是 `public`**（管理面：`user$`/`ws$`/`fs$`/`wq$`）。
    pub(crate) fn rig_with(io: &'static MemFileIo, tag: &str, is_public: bool) -> Rig {
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
        let built = crate::create::create_dictionary(&mut file0, WS, is_public).unwrap();
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
    pub(crate) fn open_catalog(io: &'static MemFileIo, rig: &Rig) -> Catalog<'static> {
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
    if obj < dict::obj_kind::USER_FIRST {
        return Err(DdlError::ReadOnlyDictionary(name.to_owned()));
    }
    if kind != dict::obj_kind::INDEX {
        return Err(DdlError::WrongKind(name.to_owned()));
    }
    let (bobj, unique, col_numbers) = index_definition(cat, obj)?;
    let base_name = object_name(cat, bobj)?;
    if matches!(
        object_ref(cat, &base_name)?.1,
        dict::obj_kind::GRAPH
            | dict::obj_kind::GRAPH_FULLTEXT_DATA
            | dict::obj_kind::GRAPH_FULLTEXT_QUEUE
    ) {
        return Err(DdlError::BadIndexDef(
            "graph storage index is managed by its graph".into(),
        ));
    }
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
    let base_seg = live_segment_block(cat, bobj)?;
    let (out, seq) = with_ddl_txn(cat, engine, |w| {
        drop_index_rows(w, obj, name)?;
        create_index_inner(w, &spec, bobj, &col_numbers, base_seg)
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

/// Reserve durable graph element IDs independently of user transaction rollback.
/// Unused IDs may be skipped; deleted and rolled-back IDs are never reused.
pub fn reserve_graph_ids(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    obj: u32,
    count: u64,
) -> Result<(u64, u64), DdlError> {
    reserve_graph_ids_from(cat, engine, obj, count, None)
}

/// Inspect whether the native sequence has never reserved graph element IDs.
/// Provisioning holds the single-writer instance lock; nonempty range imports
/// also recheck freshness inside their reservation transaction.
pub fn graph_ids_are_fresh(cat: &mut Catalog<'_>, obj: u32) -> Result<bool, DdlError> {
    Ok(graph_allocator_next(cat, obj)?.map_or(true, |next| next == 1))
}

/// Read the durable next-unused ID, including unspent and failed-query ranges.
pub fn graph_allocator_next(cat: &mut Catalog<'_>, obj: u32) -> Result<Option<u64>, DdlError> {
    let comp = crate::open::comp_num((1u64 << 32) + u64::from(obj));
    match cat.lookup("i_seq_pk", &[Some(&comp)])? {
        None => Ok(None),
        Some((_, values)) => match values.get(2) {
            Some(DictValue::Num(next)) if *next > 0 && *next < 1 << 48 => Ok(Some(*next)),
            _ => Err(DdlError::BadTableDef("invalid graph sequence".into())),
        },
    }
}

/// Initialize a nonempty snapshot ID range in a fresh native sequence.
/// The freshness guard and update share one native DDL transaction.
pub fn reserve_initial_graph_ids(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    obj: u32,
    count: u64,
) -> Result<(u64, u64), DdlError> {
    reserve_graph_ids_from(cat, engine, obj, count, Some(1))
}
fn reserve_graph_ids_from(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    obj: u32,
    count: u64,
    expected: Option<u64>,
) -> Result<(u64, u64), DdlError> {
    if count == 0 {
        return Err(DdlError::BadTableDef("empty graph ID reservation".into()));
    }
    let (range, _) = with_ddl_txn(cat, engine, |w| {
        let (def, block) = w.table("seq$")?;
        let seq_id = (1u64 << 32) + u64::from(obj);
        let comp = crate::open::comp_num(seq_id);
        let hit = w.cat.lookup("i_seq_pk", &[Some(&comp)])?;
        let next = match &hit {
            Some((_, v)) => match v.get(2) {
                Some(DictValue::Num(n)) => *n,
                _ => return Err(DdlError::BadTableDef("invalid graph sequence".into())),
            },
            None => 1,
        };
        if next == 0 || next >= 1 << 48 {
            return Err(DdlError::BadTableDef("invalid graph sequence".into()));
        }
        if expected.is_some_and(|value| value != next) {
            return Err(DdlError::BadTableDef(
                "graph snapshot allocator is not fresh".into(),
            ));
        }
        let high = next
            .checked_add(count)
            .filter(|n| *n < 1 << 48)
            .ok_or_else(|| DdlError::BadTableDef("graph ID space exhausted".into()))?;
        if let Some((rid, mut values)) = hit {
            values[2] = DictValue::Num(high);
            w.update_row_nonkey("seq$", block, def, rid, &values)?;
        } else {
            w.insert_row(
                "seq$",
                &[
                    DictValue::Num(seq_id),
                    DictValue::Text(format!("graph_{obj}")),
                    DictValue::Num(high),
                    DictValue::Num(count),
                    DictValue::Num(0),
                ],
            )?;
        }
        Ok((next, high - 1))
    })?;
    Ok(range)
}
