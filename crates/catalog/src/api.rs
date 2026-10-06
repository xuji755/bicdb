//! **只读面**（`目录详设` §6；REQ-ENG-006 的落地；C3b）。
//!
//! ```text
//! resolve / resolve_by_obj / columns / indexes_of / object_version / fixed_table
//! ```
//!
//! **取数路径**（每条都是同一个形状——"缓存 → 未命中 → 索引点查/范围扫 → 回填"）：
//!
//! ```text
//! ① 读行缓存（[`RowCache`]；闩内查表，不做 I/O）
//! ② 未命中 ⇒ 存储读（索引点查/范围扫 + ROWID 回表；CR 可见性由表访问服务负责）
//! ③ 回填（loaded_at = 当前的提交序号；写穿纪律见 §4.2）
//! ```
//!
//! # 三条语义要点（§6 原话）
//!
//! - **没有写方法**——写只经 DDL 路径；
//! - `resolve` 的**不可区分**：跨区名字、已删除、从未存在 ⇒ 同一个
//!   [`CatalogError::NotFound`]（**不泄露"存在但不可见"**）；
//! - `type_descriptor` 的实际形态 = [`ColumnDesc`] 交给 **TYP 内核**转类型描述子
//!   （目录不解释类型语义——REQ-SQL-010）。
//!
//! # 一处**显式记档的形态偏离**（与 §6 的 `&self` 签名）
//!
//! §6 的契约写作 `fn resolve(&self, …)`。本切片的实现是 `&mut self`：底层
//! [``Catalog``](crate::open::Catalog) 打开段要 `&mut DataFile`（缓存与文件是
//! 两个字段，可同时借，但**取数路径**本身要动文件）。**只读语义不受影响**——
//! 由"本类型不提供写方法"保证（§6 的第一条要点就是这个）。
//! 会话层引入 `Arc<Catalog>` 时再改为内部可变（缓存已按型分把，改造面小）。

use bicdb_common::seq::CommitSeq;
use bicdb_storage::key;

use crate::cache::{CacheError, ColRow, IcolRow, IndRow, ObjRow, TabRow};
use crate::fixed::{self, FixedTable};
use crate::open::{Catalog, OpenError};
use crate::row::RowCodecError;

/// 只读面错误。
#[derive(Debug)]
pub enum CatalogError {
    /// **找不到**——跨区名字、已删除、从未存在**不可区分**（§6）。
    NotFound,
    /// 打开链/存储层错误。
    Open(OpenError),
    /// 行编解码/行形状错误。
    Cache(CacheError),
    /// 行编解码（`row` 模块）错误。
    Row(RowCodecError),
}

impl std::fmt::Display for CatalogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CatalogError::NotFound => f.write_str("对象不存在"),
            CatalogError::Open(e) => write!(f, "目录读取：{e}"),
            CatalogError::Cache(e) => write!(f, "目录行缓存：{e}"),
            CatalogError::Row(e) => write!(f, "目录行：{e}"),
        }
    }
}

impl std::error::Error for CatalogError {}

impl From<OpenError> for CatalogError {
    fn from(e: OpenError) -> Self {
        Self::Open(e)
    }
}
impl From<CacheError> for CatalogError {
    fn from(e: CacheError) -> Self {
        Self::Cache(e)
    }
}
impl From<RowCodecError> for CatalogError {
    fn from(e: RowCodecError) -> Self {
        Self::Row(e)
    }
}

/// **对象引用**（§6 的 `ObjectRef`；`resolve` 的产物）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectRef {
    /// 对象号。
    pub obj: u32,
    /// 命名空间。
    pub namespace: u32,
    /// 名字。
    pub name: String,
    /// 对象类型（[`crate::dict::obj_kind`]）。
    pub type_code: u32,
    /// 数据对象号（**0 ⇒ 无段**——视图/过程；V1.0 不产生）。
    pub dataobj: u32,
    /// 状态。
    pub status: u32,
    /// 创建提交序号。
    pub ctime: u64,
    /// **最后修改提交序号**（= 版本；`mtime`）。
    pub mtime: u64,
}

impl ObjectRef {
    fn of(row: &ObjRow) -> Self {
        Self {
            obj: row.obj,
            namespace: row.namespace,
            name: row.name.clone(),
            type_code: row.type_code,
            dataobj: row.dataobj,
            status: row.status,
            ctime: row.ctime,
            mtime: row.mtime,
        }
    }
}

/// **列描述**（§6 的 `ColumnDesc`；目录**不解释类型语义**——转类型描述子是 TYP 内核的事）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDesc {
    /// 列号（1 起）。
    pub col: u32,
    /// 列名。
    pub name: String,
    /// 类型码。
    pub type_code: u32,
    /// 声明长度（字节）。
    pub length: u32,
    /// 精度。
    pub precision: Option<u32>,
    /// 标度。
    pub scale: Option<u32>,
    /// 可空。
    pub nullable: bool,
}

impl ColumnDesc {
    fn of(row: &ColRow) -> Self {
        Self {
            col: row.col,
            name: row.name.clone(),
            type_code: row.type_code,
            length: row.length,
            precision: row.precision,
            scale: row.scale,
            nullable: row.nullable,
        }
    }
}

/// 索引键的一列（`icol$` 的一行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexCol {
    /// 键内位置（1 起）。
    pub pos: u32,
    /// 列号（**0 = 表达式键**——键来源在 [`IndexRef::expr_src`]）。
    pub col: u32,
    /// 降序标志（V1.0 恒 false）。
    pub is_desc: bool,
}

/// **索引引用**（§6 的 `IndexRef`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexRef {
    /// 索引对象号。
    pub obj: u32,
    /// 基表对象号。
    pub bobj: u32,
    /// 索引类别（[`crate::dict::index_kind`]）。
    pub kind: u32,
    /// 唯一。
    pub is_unique: bool,
    /// 状态（**Move 失效落此**：0 无效 ⇒ 不进选路）。
    pub status: u32,
    /// 键列组成（按位置序）。
    pub cols: Vec<IndexCol>,
    /// 表达式键来源（表达式索引才有）。
    pub expr_src: Option<Vec<u8>>,
}

/// `ind$` 全量 + `icol$` 按对象成组（`load_all_indexes` 的返回形态）。
type LoadedIndexes = (Vec<IndRow>, std::collections::BTreeMap<u32, Vec<IcolRow>>);

/// **DML 索引维护清单**的一条（表的每个**可用**索引 = 一条）。
///
/// 用途：DML 路径（INSERT 的表访问写侧）按它维护索引项——目录把"索引清单 +
/// 键列构成 + 段头块"一次给全，写侧不必再查字典（写侧在事务里，不宜回查）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DmlIndex {
    /// 索引对象号。
    pub obj: u32,
    /// 索引名（诊断/错误消息）。
    pub name: String,
    /// 索引段头块（`seg$.block_id` 现取）。
    pub seg_page0: u32,
    /// 键列在**行内**的 0 基序号（与外层 `types` 同序）。
    pub cols: Vec<usize>,
    /// 唯一索引。
    pub unique: bool,
}

/// **对象版本**（计划缓存的比对依据，REQ-SQL-009）。
///
/// 三元组 `(obj#, mtime, status)`（`目录详设` §5.5）：`Move` 引起的索引失效
/// **只动 status**（`obj$.status` 权威 + `ind$.status` 副本）——键因此失配，
/// 陈旧计划被重编译，而不是靠"通知"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectVersion {
    /// 对象号。
    pub obj: u32,
    /// 最后修改提交序号。
    pub mtime: u64,
    /// 状态（1 = 有效；`Move` 后索引为 0 ⇒ 不进选路 + 计划键失配）。
    pub status: u32,
}

impl<'io> Catalog<'io> {
    // ───────────────────────── 装载戳 ─────────────────────────

    /// **当前的提交序号**（装载戳 = 缓存条目的 `loaded_at`）。
    ///
    /// 打开期由调用方设为**恢复后的最新已提交序号**（§4.1：打开是恢复语义）；
    /// 此后由提交路径推进（§4.3 的世代号同源）。
    pub fn set_current_seq(&mut self, seq: CommitSeq) {
        self.current_seq = seq.as_raw();
    }

    /// 当前的提交序号。
    #[must_use]
    pub fn current_seq(&self) -> u64 {
        self.current_seq
    }

    /// **DDL 钩子：提交后推进**（世代自增 + 装载戳前移）。
    ///
    /// 调用方须**先**用 [`Catalog::row_cache`] 的 `note_change_*` 登记改动
    /// （精确失效），再调本方法（§4.3："提交路径的一个钩子，先自增后放行"）。
    pub fn advance_commit(&mut self, seq: CommitSeq) {
        self.current_seq = seq.as_raw();
        self.cache.bump_generation();
    }

    // ───────────────────────── 解析 ─────────────────────────

    /// **按名解析**（§6）。
    pub fn resolve(
        &mut self,
        snapshot: CommitSeq,
        ns: u32,
        name: &str,
    ) -> Result<ObjectRef, CatalogError> {
        if let Some(obj) = self.cache.get_obj_by_name(snapshot, ns, name) {
            if let Some(row) = self.cache.get_obj(snapshot, obj) {
                return Ok(ObjectRef::of(&row));
            }
        }
        // 未命中 ⇒ 存储读：`i_obj_name (namespace, name)` 点查。
        let ns_b = crate::open::comp_num(u64::from(ns));
        let name_b = crate::open::comp_text(name);
        let hit = self
            .lookup("i_obj_name", &[Some(&ns_b), Some(&name_b)])?
            .ok_or(CatalogError::NotFound)?;
        let row = ObjRow::from_values(&hit.1)?;
        self.insert_obj_row(row)
            .ok_or(CatalogError::NotFound)
            .map(|r| ObjectRef::of(&r))
    }

    /// **按对象号解析**（§6）。
    pub fn resolve_by_obj(
        &mut self,
        snapshot: CommitSeq,
        obj: u32,
    ) -> Result<ObjectRef, CatalogError> {
        if let Some(row) = self.cache.get_obj(snapshot, obj) {
            return Ok(ObjectRef::of(&row));
        }
        let key_b = crate::open::comp_num(u64::from(obj));
        let hit = self
            .lookup("i_obj_pk", &[Some(&key_b)])?
            .ok_or(CatalogError::NotFound)?;
        let row = ObjRow::from_values(&hit.1)?;
        self.insert_obj_row(row)
            .ok_or(CatalogError::NotFound)
            .map(|r| ObjectRef::of(&r))
    }

    /// 回填 `obj$` 行（装载戳 = 当前提交序号）。
    fn insert_obj_row(&self, row: ObjRow) -> Option<ObjRow> {
        let stamp = CommitSeq::from_raw(self.current_seq).ok_or(()).ok()?;
        self.cache.put_obj(stamp, row.clone());
        Some(row)
    }

    // ───────────────────────── 列 ─────────────────────────

    /// **列定义**（§6；`col$` 按 `obj#` 成组）。
    pub fn columns(
        &mut self,
        snapshot: CommitSeq,
        obj: u32,
    ) -> Result<Vec<ColumnDesc>, CatalogError> {
        if let Some(rows) = self.cache.get_cols(snapshot, obj) {
            return Ok(rows.iter().map(ColumnDesc::of).collect());
        }
        let rows = self.load_columns(obj)?;
        // 无列：对象不存在 ⇒ NotFound；存在但非表（视图/过程）⇒ 空表列。
        if rows.is_empty() {
            self.resolve_by_obj(snapshot, obj)?; // 不存在则此处 NotFound
            return Ok(Vec::new());
        }
        Ok(rows.iter().map(ColumnDesc::of).collect())
    }

    /// 存储读 `col$`：`i_col_pk (obj#, col#)` 的**前缀范围扫**。
    fn load_columns(&mut self, obj: u32) -> Result<Vec<ColRow>, CatalogError> {
        let lo = key::encode(&[Some(&crate::open::comp_num(u64::from(obj)))]);
        let hi = key::encode(&[
            Some(&crate::open::comp_num(u64::from(obj))),
            // 上界：第二分量取"大于一切 NUMBER 编码"的字节（`col#` 是数值列；
            // 编码域 ≤ 24 字节 ⇒ 32 字节的 0xFF 恒在它之后）。
            Some(&[0xFFu8; 32]),
        ]);
        let entries = self.range_index("i_col_pk", Some(&lo), Some(&hi))?;
        let mut rows = Vec::with_capacity(entries.len());
        for (_comps, rid) in entries {
            let Some(values) = self.fetch_opt("col$", rid)? else {
                continue; // 死索引项（回滚孤儿）
            };
            rows.push(ColRow::from_values(&values)?);
        }
        rows.sort_by_key(|r| r.col);
        if !rows.is_empty() {
            let stamp = CommitSeq::from_raw(self.current_seq).unwrap_or_else(zero_seq);
            self.cache.put_cols(stamp, obj, rows.clone());
        }
        Ok(rows)
    }

    // ───────────────────────── 表选项 ─────────────────────────

    /// **表选项**（`tab$` 一行；DML/DDL 的行为参数：`pctfree`/`itl_max`/…）。
    ///
    /// 缓存优先，未命中回查 `i_tab_pk`（与 `columns` 同一形态）。
    pub fn table_options(&mut self, snapshot: CommitSeq, obj: u32) -> Result<TabRow, CatalogError> {
        if let Some(row) = self.cache.get_tab(snapshot, obj) {
            return Ok(row);
        }
        let key = crate::open::comp_num(u64::from(obj));
        let hit = self
            .lookup("i_tab_pk", &[Some(&key)])?
            .ok_or(CatalogError::NotFound)?;
        let row = TabRow::from_values(&hit.1)?;
        let stamp = CommitSeq::from_raw(self.current_seq).unwrap_or_else(zero_seq);
        self.cache.put_tab(stamp, row.clone());
        Ok(row)
    }

    // ───────────────────────── 索引 ─────────────────────────

    /// **可进选路的索引清单**（`目录详设` §5.5）：`status == 1` 且 `bobj#` 有效。
    ///
    /// **这是选路的唯一入口**——`status ≠ 1`（`Move` 失效、未建成）的索引在此
    /// 被排除，调用方（优化器）不需要自己过滤，也就不会"忘了过滤"。
    pub fn usable_indexes_of(
        &mut self,
        snapshot: CommitSeq,
        table_obj: u32,
    ) -> Result<Vec<IndexRef>, CatalogError> {
        Ok(self
            .indexes_of(snapshot, table_obj)?
            .into_iter()
            .filter(|i| i.status == 1)
            .collect())
    }

    /// **某表的索引清单**（§6；`ind$` 按 `bobj#` 过滤 + `icol$` 组装键列）。
    ///
    /// V1.0 无 `bobj#` 索引 ⇒ 全扫 `i_ind_pk`/`i_icol_pk`（索引数有界），
    /// 结果行**写穿缓存**供后续 `resolve`/点查用。
    ///
    /// **`_snapshot` 在本路径上没有拦截面**（记档）：目录的读是**当前已提交
    /// 状态**（单写者下池即真值，与 `ddl` 的扫描同一条口径）；缓存条目的
    /// 装载戳取 `current_seq`（内容比任何更老的快照都新，不能拿快照号冒充）。
    /// 并发写者接入时这里要改成快照读（见 `doc/待讨论清单.md`）。
    pub fn indexes_of(
        &mut self,
        _snapshot: CommitSeq,
        table_obj: u32,
    ) -> Result<Vec<IndexRef>, CatalogError> {
        let (inds, icols) = self.load_all_indexes()?;
        let mut out = Vec::new();
        for ind in inds.iter().filter(|i| i.bobj == table_obj) {
            // **键列取自本次装载的事实**（不是从缓存再取一次）：
            // 缓存有快照门（`snapshot < loaded_at` ⇒ 未命中），在那里退化会得到
            // **空键列**——所有行同键，唯一索引要么全判冲突要么全漏判，且无声。
            let cols = icols.get(&ind.obj).cloned().unwrap_or_default();
            out.push(IndexRef {
                obj: ind.obj,
                bobj: ind.bobj,
                kind: ind.type_code,
                is_unique: ind.is_unique,
                status: ind.status,
                cols: cols
                    .iter()
                    .map(|c| IndexCol {
                        pos: c.pos,
                        col: c.col,
                        is_desc: c.is_desc,
                    })
                    .collect(),
                expr_src: ind.expr_src.clone(),
            });
        }
        out.sort_by_key(|i| i.obj);
        Ok(out)
    }

    /// 全扫 `ind$` + `icol$`（结果写穿缓存）。
    fn load_all_indexes(&mut self) -> Result<LoadedIndexes, CatalogError> {
        let ind_entries = self.scan_index("i_ind_pk")?;
        let mut inds = Vec::with_capacity(ind_entries.len());
        for (_k, rid) in ind_entries {
            let Some(values) = self.fetch_opt("ind$", rid)? else {
                continue; // 死索引项（回滚孤儿）
            };
            inds.push(IndRow::from_values(&values)?);
        }
        let icol_entries = self.scan_index("i_icol_pk")?;
        let mut by_obj: std::collections::BTreeMap<u32, Vec<IcolRow>> = Default::default();
        for (_k, rid) in icol_entries {
            let Some(values) = self.fetch_opt("icol$", rid)? else {
                continue; // 死索引项（回滚孤儿）
            };
            let row = IcolRow::from_values(&values)?;
            by_obj.entry(row.obj).or_default().push(row);
        }
        let stamp = CommitSeq::from_raw(self.current_seq).unwrap_or_else(zero_seq);
        for rows in by_obj.values_mut() {
            rows.sort_by_key(|r| r.pos);
        }
        for ind in &inds {
            self.cache.put_ind(stamp, ind.clone());
        }
        for (obj, rows) in &by_obj {
            self.cache.put_icols(stamp, *obj, rows.clone());
        }
        Ok((inds, by_obj))
    }

    // ───────────────────────── 版本 ─────────────────────────

    /// **对象版本**（= `mtime`；§6，计划缓存的比对依据）。
    pub fn object_version(
        &mut self,
        snapshot: CommitSeq,
        obj: u32,
    ) -> Result<ObjectVersion, CatalogError> {
        let r = self.resolve_by_obj(snapshot, obj)?;
        Ok(ObjectVersion {
            obj: r.obj,
            mtime: r.mtime,
            status: r.status,
        })
    }

    // ───────────────────────── 固定表 ─────────────────────────

    /// **固定表**（§6：`file$` 不查字典；内容 = 控制文件的内存映像）。
    ///
    /// 形态记档：§6 写作 `fixed_table(&self, name)`——本切片取
    /// `(name, files)` 两参（目录**不持有控制文件**：它的生命周期在实例/工作区
    /// 打开链）。调用方把 `ControlFile::data_file_records()` 的结果传进来即可。
    #[must_use]
    pub fn fixed_table(
        &self,
        name: &str,
        files: &[bicdb_storage::controlfile::DataFileRecord],
    ) -> Option<FixedTable> {
        fixed::table(name, files)
    }

    /// 缓存的诊断口（统计照 `V$ROWCACHE`）。
    #[must_use]
    pub fn row_cache(&self) -> &crate::cache::RowCache {
        &self.cache
    }
}

fn zero_seq() -> CommitSeq {
    CommitSeq::from_raw(0).expect("0 在 48 位域内")
}

/// 值的便捷判定（诊断/测试）：是不是"有段"的对象。
#[must_use]
pub fn has_segment(r: &ObjectRef) -> bool {
    r.dataobj != 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::CacheKind;
    use crate::create::{create_dictionary, BuiltDictionary};
    use crate::dict;
    use bicdb_storage::bitmap::{FileLayout, META_ROLE};
    use bicdb_storage::datafile::DataFile;
    use bicdb_workspace::io::MemFileIo;
    use std::path::Path;

    const WS: [u8; 8] = [7u8; 8];

    fn seq(v: u64) -> CommitSeq {
        CommitSeq::from_raw(v).unwrap()
    }

    /// 真件：建 file 0 → 建自举集 → 种子 → 重开 → 设装载戳。
    fn seeded<'a>(io: &'a MemFileIo, path: &str) -> (Catalog<'a>, BuiltDictionary) {
        let mut file = DataFile::create(
            io,
            Path::new(path),
            0,
            META_ROLE,
            WS,
            FileLayout::meta().min_file_blocks() + 512,
        )
        .unwrap();
        let built = create_dictionary(&mut file, WS, false).unwrap();
        let mut cat = Catalog::from_entries(file, built.entries.clone()).unwrap();
        cat.seed_own_dictionary(&built).unwrap();
        drop(cat);
        let mut cat = Catalog::open(io, Path::new(path)).unwrap();
        cat.set_current_seq(seq(10_000));
        (cat, built)
    }

    fn obj_of(built: &BuiltDictionary, name: &str) -> u32 {
        built.object(name).expect("自举对象").obj
    }

    #[test]
    fn resolve_by_name_and_by_obj_agree_and_cache_warms_up() {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let (mut cat, built) = seeded(&io, "/mem/a1.dat");
        let want = obj_of(&built, "obj$");

        let r = cat
            .resolve(seq(10_000), dict::namespace::TABLE, "obj$")
            .unwrap();
        assert_eq!(r.obj, want);
        assert_eq!(r.name, "obj$");
        assert_eq!(r.namespace, dict::namespace::TABLE);
        assert_eq!(r.type_code, dict::obj_kind::TABLE);
        assert_eq!(r.dataobj, want, "自举对象 obj# = dataobj#");
        assert_eq!(r.status, 1);
        assert!(has_segment(&r));

        // 首次：by_name 一次 miss（回查存储后一次回填）；此后按名/按号全命中。
        let first = cat.row_cache().stats_of(CacheKind::Obj);
        assert_eq!(
            (first.misses, first.hits, first.modifications),
            (1, 0, 1),
            "{first:?}"
        );

        let again = cat
            .resolve(seq(10_000), dict::namespace::TABLE, "obj$")
            .unwrap();
        assert_eq!(again, r);
        let same = cat.resolve_by_obj(seq(10_000), want).unwrap();
        assert_eq!(same, r, "按号解析与按名解析同源");
        let cached = cat.row_cache().stats_of(CacheKind::Obj);
        assert_eq!((cached.misses, cached.hits), (1, 3), "{cached:?}");
    }

    #[test]
    fn not_found_is_indistinguishable_across_reasons() {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let (mut cat, built) = seeded(&io, "/mem/a2.dat");
        // 从未存在。
        assert!(matches!(
            cat.resolve(seq(10_000), dict::namespace::TABLE, "nope"),
            Err(CatalogError::NotFound)
        ));
        // 存在但命名空间不同（"obj$" 是表，不在索引命名空间）——同一个 NotFound。
        assert!(matches!(
            cat.resolve(seq(10_000), dict::namespace::INDEX, "obj$"),
            Err(CatalogError::NotFound)
        ));
        // 对象号不存在。
        assert!(matches!(
            cat.resolve_by_obj(seq(10_000), 9999),
            Err(CatalogError::NotFound)
        ));
        // 空名字。
        assert!(matches!(
            cat.resolve(seq(10_000), dict::namespace::TABLE, ""),
            Err(CatalogError::NotFound)
        ));
        assert!(obj_of(&built, "obj$") != 0, "自举集确实存在（反例对照）");
    }

    #[test]
    fn columns_come_back_in_column_order_from_a_prefix_scan() {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let (mut cat, built) = seeded(&io, "/mem/a3.dat");
        let obj = obj_of(&built, "col$");
        let cols = cat.columns(seq(10_000), obj).unwrap();
        assert_eq!(cols.len(), dict::COL_COLS.len());
        for (i, c) in cols.iter().enumerate() {
            let def = &dict::COL_COLS[i];
            assert_eq!(c.col, u32::from(def.col), "按列号序");
            assert_eq!(c.name, def.name);
            assert_eq!(c.type_code, u32::from(def.type_code as u8));
            assert_eq!(c.nullable, def.nullable);
        }
        // 第二次走缓存（col$ 型的 gets 增长、misses 不再增长）。
        let before = cat.row_cache().stats_of(CacheKind::Col);
        let again = cat.columns(seq(10_000), obj).unwrap();
        assert_eq!(again, cols);
        let after = cat.row_cache().stats_of(CacheKind::Col);
        assert_eq!(after.misses, before.misses, "缓存命中，无回查");
        assert_eq!(after.hits, before.hits + 1);
        // **前缀范围扫不串行**：另一张表的列组互不含混。
        let tab = cat.columns(seq(10_000), obj_of(&built, "tab$")).unwrap();
        assert_eq!(tab.len(), dict::TAB_COLS.len());
        assert_eq!(tab[0].name, "obj#");
        assert_ne!(tab.len(), cols.len(), "两组的列数不同（若串组会相等）");
    }

    #[test]
    fn columns_of_a_non_table_object_are_empty_but_unknown_objects_are_not_found() {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let (mut cat, built) = seeded(&io, "/mem/a4.dat");
        // 索引对象没有 col$ 行 ⇒ 空列（不是 NotFound）。
        let idx = obj_of(&built, "i_obj_pk");
        assert!(cat.columns(seq(10_000), idx).unwrap().is_empty());
        // 完全不存在的对象 ⇒ NotFound。
        assert!(matches!(
            cat.columns(seq(10_000), 9999),
            Err(CatalogError::NotFound)
        ));
    }

    #[test]
    fn indexes_of_lists_keys_with_their_columns() {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let (mut cat, built) = seeded(&io, "/mem/a5.dat");
        let obj = obj_of(&built, "obj$");
        let idx = cat.indexes_of(seq(10_000), obj).unwrap();
        assert_eq!(idx.len(), 2, "obj$ 有两个键");
        let pk = idx
            .iter()
            .find(|i| i.obj == obj_of(&built, "i_obj_pk"))
            .unwrap();
        assert_eq!(pk.bobj, obj);
        assert!(pk.is_unique);
        assert_eq!(pk.kind, dict::index_kind::BTREE);
        assert_eq!(pk.status, 1);
        assert_eq!(pk.cols.len(), 1);
        assert_eq!(pk.cols[0].col, 1, "i_obj_pk = (obj#)");
        assert!(!pk.cols[0].is_desc);
        let name = idx
            .iter()
            .find(|i| i.obj == obj_of(&built, "i_obj_name"))
            .unwrap();
        let cols: Vec<u32> = name.cols.iter().map(|c| c.col).collect();
        assert_eq!(
            cols,
            vec![3, 2],
            "i_obj_name = (namespace, name)，按 pos# 序"
        );
        assert_eq!(
            name.cols.iter().map(|c| c.pos).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert!(name.expr_src.is_none(), "非表达式索引");
        // 别的表看不到这些键。
        let tab_keys = cat.indexes_of(seq(10_000), obj_of(&built, "tab$")).unwrap();
        assert_eq!(tab_keys.len(), 1);
        assert_eq!(tab_keys[0].obj, obj_of(&built, "i_tab_pk"));
    }

    #[test]
    fn write_through_makes_the_new_version_visible_at_once() {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let (mut cat, built) = seeded(&io, "/mem/a6.dat");
        let obj = obj_of(&built, "obj$");
        assert_eq!(
            cat.object_version(seq(10_000), obj).unwrap().mtime,
            0,
            "建区种子 mtime = 0"
        );
        // **写穿**（§4.2 纪律 4）：DDL 在同一事务内改字典行时同改缓存 ⇒ 立即可见。
        cat.row_cache().put_obj(
            seq(10_042),
            ObjRow {
                obj,
                name: "obj$".to_owned(),
                namespace: dict::namespace::TABLE,
                type_code: dict::obj_kind::TABLE,
                dataobj: obj,
                ctime: 0,
                mtime: 42,
                status: 1,
            },
        );
        cat.set_current_seq(seq(10_042));
        assert_eq!(cat.object_version(seq(10_042), obj).unwrap().mtime, 42);
        assert_eq!(
            cat.resolve(seq(10_042), dict::namespace::TABLE, "obj$")
                .unwrap()
                .mtime,
            42,
            "名字路径同样看到新版本"
        );
    }

    #[test]
    fn stale_snapshot_reads_storage_and_cache_defers_to_the_dictionary_row() {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let (mut cat, built) = seeded(&io, "/mem/a7.dat");
        let obj = obj_of(&built, "obj$");
        // 缓存里放一个"更新"的行（loaded_at = 10000）。
        let mut row = ObjRow {
            obj,
            name: "obj$".to_owned(),
            namespace: dict::namespace::TABLE,
            type_code: dict::obj_kind::TABLE,
            dataobj: obj,
            ctime: 0,
            mtime: 99,
            status: 1,
        };
        cat.row_cache().put_obj(seq(10_000), row.clone());
        assert_eq!(cat.object_version(seq(10_000), obj).unwrap().mtime, 99);
        // **旧快照**（< 装载点）⇒ 不命中缓存，回存储读（CR）——拿到字典行的真值。
        let old = cat.resolve(seq(5), dict::namespace::TABLE, "obj$").unwrap();
        assert_eq!(old.mtime, 0, "字典行是权威（纪律 1）");
        // 回填把缓存也拉回字典行的值（装载戳 = 当前提交序号）。
        row.mtime = 0;
        let _ = row;
        assert_eq!(
            cat.resolve(seq(10_000), dict::namespace::TABLE, "obj$")
                .unwrap()
                .mtime,
            0
        );
    }

    #[test]
    fn generation_bump_forces_a_recheck_that_still_answers() {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let (mut cat, built) = seeded(&io, "/mem/a8.dat");
        let obj = obj_of(&built, "obj$");
        let first = cat
            .resolve(seq(10_000), dict::namespace::TABLE, "obj$")
            .unwrap();
        // DDL 登记了"我改了 obj$"（虽然这里没真改）⇒ 精确失效 ⇒ 回查，答案不变。
        cat.row_cache().note_change_obj(obj);
        cat.advance_commit(seq(10_001));
        let again = cat
            .resolve(seq(10_001), dict::namespace::TABLE, "obj$")
            .unwrap();
        assert_eq!(again, first);
        let s = cat.row_cache().stats_of(CacheKind::Obj);
        assert_eq!(
            (s.gets, s.hits, s.misses, s.modifications),
            (2, 0, 2, 2),
            "失效后按名回查一次（名字映射随对象条目一起撤）：{s:?}"
        );
    }

    #[test]
    fn fixed_table_file_reads_the_control_file_image() {
        use bicdb_storage::controlfile::DataFileRecord;
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let (cat, _built) = seeded(&io, "/mem/a9.dat");
        let mut a = DataFileRecord::new(0, 0);
        a.status = 1;
        a.set_path(b"/mnt/ws/file0").unwrap();
        let mut b = DataFileRecord::new(3, 3);
        b.status = 1;
        b.creation_blocks = 4096;
        b.set_path(b"/mnt/ws/data_03").unwrap();
        let t = cat.fixed_table("file$", &[a, b]).expect("file$ 固定表");
        assert_eq!(t.cardinality(), 2);
        assert_eq!(t.columns, crate::fixed::FILE_COLUMNS);
        assert_eq!(t.rows[1][4], crate::row::DictValue::Num(4096));
        assert!(cat.fixed_table("session$", &[]).is_none(), "会话层未落地");
    }
}
