//! **字典行缓存**（`row cache` 形态；`目录详设` §4.2/§4.3；C3a）。
//!
//! ```text
//! 型（**每型一把闩锁**——Oracle row cache objects 子锁存器的对应物，证据 E9 约束②）
//!   obj$  : by_obj# → ObjRow      ；by_name: (namespace, name) → obj#（名字解析热路径）
//!   tab$  : by_obj# → TabRow
//!   col$  : by_obj# → Vec<ColRow>          （**按 obj# 成组装入**——一次范围扫）
//!   ind$  : by_obj# → IndRow
//!   icol$ : by_obj# → Vec<IcolRow>
//!   seg$  : by_dataobj# → SegRow           （段头跳板）
//!   undo$ : by_seg# → UndoRow
//! 每个条目：{ 行值, loaded_at: CommitSeq（装入时的提交序号）, generation }
//! ```
//!
//! # 五条纪律（§4.2，逐条落到本模块）
//!
//! 1. **缓存是派生物**：任何不一致以字典行为准；miss 即回查（**本模块不碰存储**——
//!    回查在 [`crate::api`] 的读路径上，经表访问服务，CR 可见性由服务负责）；
//! 2. **不跨工作区**：每工作区一个 [`RowCache`] 实例（随 `Catalog` 走）；
//! 3. **快照门槛**：条目记 `loaded_at`；`snapshot < loaded_at` ⇒ **不命中**
//!    （读方要的是旧版本，缓存只有最新已提交版本）⇒ 走存储 CR；
//! 4. **写穿**：DDL 在**同一事务内**改字典行时同调 `put_*` 改缓存——单一写路径，
//!    没有"该发通知而没发"的窗口（单进程形态下对 Oracle enqueue 的替代，证据 E6）；
//! 5. **不进缓冲池、不进 WMM**：独立缓冲；容量 = **行数 + 字节双上限的 LRU**
//!    （Oracle 淘汰算法正文 KB 未给 ⇒ 自定，见 [`CacheCaps`]）；统计四件套照
//!    `V$ROWCACHE`（`gets`/`hits`/`misses`/`modifications`）。
//!
//! # 失效（§4.3）：世代号 + 精确失效
//!
//! ```text
//! generation（u64）：打开时 = 恢复后的提交序号；任何 DDL 提交后自增
//!   条目记装入时的 generation；读时先看 generation：
//!     相等        ⇒ 直接命中
//!     不等且有登记 ⇒ **精确失效**（只失效登记过的 obj#/name；其余条目刷新世代）
//!     不等且无登记 ⇒ **全清**
//! ```
//!
//! **前提记档**（设计原话）：**单进程引擎**——跨进程失效不做；将来出现第二个
//! 写者，本协议必须重做（不得假设它已经支持）。
//!
//! # 闩锁形态：闩内选址 → 闩外取数 → 闩内收尾
//!
//! 本模块的读只做"闩内查表"，**不做 I/O**（`§5.10` 的两阶段纪律）：调用方拿到
//! `None`（miss）后去存储取行，再 `put_*` 回填——期间不持本模块的任何闩锁。

use std::collections::HashMap;

use bicdb_common::latch::Latch;
use bicdb_common::seq::CommitSeq;

use crate::row::DictValue;

// ───────────────────────────── 型 ─────────────────────────────

/// 缓存的**型**（一张字典表一型；`ind$` 与 `icol$` 各一型——§4.2 的口径）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CacheKind {
    /// `obj$`。
    Obj,
    /// `tab$`。
    Tab,
    /// `col$`。
    Col,
    /// `ind$`。
    Ind,
    /// `icol$`。
    Icol,
    /// `seg$`。
    Seg,
    /// `undo$`。
    Undo,
}

impl CacheKind {
    /// 全部型（统计/清空按此序）。
    pub const ALL: [CacheKind; 7] = [
        CacheKind::Obj,
        CacheKind::Tab,
        CacheKind::Col,
        CacheKind::Ind,
        CacheKind::Icol,
        CacheKind::Seg,
        CacheKind::Undo,
    ];

    /// 字典表名（`V$ROWCACHE` 的 `PARAMETER` 对应物）。
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            CacheKind::Obj => "obj$",
            CacheKind::Tab => "tab$",
            CacheKind::Col => "col$",
            CacheKind::Ind => "ind$",
            CacheKind::Icol => "icol$",
            CacheKind::Seg => "seg$",
            CacheKind::Undo => "undo$",
        }
    }

    const fn index(self) -> usize {
        match self {
            CacheKind::Obj => 0,
            CacheKind::Tab => 1,
            CacheKind::Col => 2,
            CacheKind::Ind => 3,
            CacheKind::Icol => 4,
            CacheKind::Seg => 5,
            CacheKind::Undo => 6,
        }
    }
}

// ───────────────────────────── 行 ─────────────────────────────

/// 行形状/值域错误（缓存行与字典行常量不符——**响亮**）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheError {
    /// 列数或值形态与 [`crate::dict`] 的常量不符。
    Shape {
        /// 字典表名。
        table: &'static str,
        /// 列名。
        col: &'static str,
    },
    /// 数值越出该列的编码域（如 `obj# > u32::MAX`）。
    OutOfDomain {
        /// 字典表名。
        table: &'static str,
        /// 列名。
        col: &'static str,
    },
}

impl std::fmt::Display for CacheError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CacheError::Shape { table, col } => write!(f, "{table} 的行形状与列 {col} 不符"),
            CacheError::OutOfDomain { table, col } => write!(f, "{table} 的列 {col} 越出编码域"),
        }
    }
}

impl std::error::Error for CacheError {}

fn shape<T>(table: &'static str, col: &'static str) -> Result<T, CacheError> {
    Err(CacheError::Shape { table, col })
}

fn num(table: &'static str, col: &'static str, v: &DictValue) -> Result<u64, CacheError> {
    match v {
        DictValue::Num(n) => Ok(*n),
        _ => shape(table, col),
    }
}

fn num32(table: &'static str, col: &'static str, v: &DictValue) -> Result<u32, CacheError> {
    let n = num(table, col, v)?;
    u32::try_from(n).map_err(|_| CacheError::OutOfDomain { table, col })
}

fn text(table: &'static str, col: &'static str, v: &DictValue) -> Result<String, CacheError> {
    match v.clone() {
        DictValue::Text(t) => Ok(t),
        DictValue::Bytes(b) => String::from_utf8(b).map_err(|_| CacheError::Shape { table, col }),
        _ => shape(table, col),
    }
}

fn boolean(table: &'static str, col: &'static str, v: &DictValue) -> Result<bool, CacheError> {
    match v {
        DictValue::Bool(b) => Ok(*b),
        _ => shape(table, col),
    }
}

fn opt_num32(
    table: &'static str,
    col: &'static str,
    v: &DictValue,
) -> Result<Option<u32>, CacheError> {
    match v {
        DictValue::Null => Ok(None),
        _ => num32(table, col, v).map(Some),
    }
}

fn opt_bytes(
    table: &'static str,
    col: &'static str,
    v: &DictValue,
) -> Result<Option<Vec<u8>>, CacheError> {
    match v {
        DictValue::Null => Ok(None),
        DictValue::Bytes(b) => Ok(Some(b.clone())),
        DictValue::Text(t) => Ok(Some(t.as_bytes().to_vec())),
        _ => shape(table, col),
    }
}

fn value_bytes(v: &DictValue) -> usize {
    match v {
        DictValue::Null => 0,
        DictValue::Num(_) | DictValue::Bool(_) => 8,
        DictValue::Text(t) => t.len(),
        DictValue::Bytes(b) => b.len(),
    }
}

/// 估算一行的缓存占用（**防御性上限用**，不必精确）。
fn rows_bytes(rows: &[Vec<DictValue>]) -> usize {
    rows.iter()
        .map(|r| 16 + r.iter().map(value_bytes).sum::<usize>())
        .sum()
}

/// `obj$` 的一行（`目录详设` §6 的 `ObjectRef` 值域）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjRow {
    /// 对象号。
    pub obj: u32,
    /// 名字。
    pub name: String,
    /// 命名空间（表/索引；[`crate::dict::namespace`]）。
    pub namespace: u32,
    /// 对象类型（[`crate::dict::obj_kind`]）。
    pub type_code: u32,
    /// 数据对象号（0 = 无段）。
    pub dataobj: u32,
    /// 创建提交序号。
    pub ctime: u64,
    /// **最后修改提交序号**（= 版本号；`mtime`）。
    pub mtime: u64,
    /// 状态（0 无效 / 1 有效）。
    pub status: u32,
}

impl ObjRow {
    /// 由 `obj$` 行的值域构造。
    pub fn from_values(v: &[DictValue]) -> Result<Self, CacheError> {
        if v.len() != crate::dict::OBJ_COLS.len() {
            return shape("obj$", "<行>");
        }
        Ok(Self {
            obj: num32("obj$", "obj#", &v[0])?,
            name: text("obj$", "name", &v[1])?,
            namespace: num32("obj$", "namespace", &v[2])?,
            type_code: num32("obj$", "type#", &v[3])?,
            dataobj: num32("obj$", "dataobj#", &v[4])?,
            ctime: num("obj$", "ctime", &v[5])?,
            mtime: num("obj$", "mtime", &v[6])?,
            status: num32("obj$", "status", &v[7])?,
        })
    }

    /// 回值域（写穿/测试用）。
    #[must_use]
    pub fn to_values(&self) -> Vec<DictValue> {
        vec![
            DictValue::Num(u64::from(self.obj)),
            DictValue::Text(self.name.clone()),
            DictValue::Num(u64::from(self.namespace)),
            DictValue::Num(u64::from(self.type_code)),
            DictValue::Num(u64::from(self.dataobj)),
            DictValue::Num(self.ctime),
            DictValue::Num(self.mtime),
            DictValue::Num(u64::from(self.status)),
        ]
    }
}

/// `tab$` 的一行（七项表选项）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabRow {
    /// 表对象号。
    pub obj: u32,
    /// 列数。
    pub cols: u32,
    /// `pctfree`。
    pub pctfree: u32,
    /// ITL 槽上限。
    pub itl_max: u32,
    /// 事务性。
    pub transactional: bool,
    /// 日志模式（[`crate::dict::table_opt`]）。
    pub logging: u32,
    /// 更新模式。
    pub update_mode: u32,
    /// 保留（版本保留策略）。
    pub retention: u32,
    /// 版本保留量。
    pub version_keep: u32,
    /// 段清理策略。
    pub cleanup: u32,
    /// 嵌入策略。
    pub embed: u32,
    /// 公共数据表标志。
    pub shared: bool,
}

impl TabRow {
    /// 由 `tab$` 行的值域构造。
    pub fn from_values(v: &[DictValue]) -> Result<Self, CacheError> {
        if v.len() != crate::dict::TAB_COLS.len() {
            return shape("tab$", "<行>");
        }
        Ok(Self {
            obj: num32("tab$", "obj#", &v[0])?,
            cols: num32("tab$", "cols", &v[1])?,
            pctfree: num32("tab$", "pctfree", &v[2])?,
            itl_max: num32("tab$", "itl_max", &v[3])?,
            transactional: boolean("tab$", "transactional", &v[4])?,
            logging: num32("tab$", "logging", &v[5])?,
            update_mode: num32("tab$", "update_mode", &v[6])?,
            retention: num32("tab$", "retention", &v[7])?,
            version_keep: num32("tab$", "version_keep", &v[8])?,
            cleanup: num32("tab$", "cleanup", &v[9])?,
            embed: num32("tab$", "embed", &v[10])?,
            shared: boolean("tab$", "shared", &v[11])?,
        })
    }

    /// 回值域。
    #[must_use]
    pub fn to_values(&self) -> Vec<DictValue> {
        vec![
            DictValue::Num(u64::from(self.obj)),
            DictValue::Num(u64::from(self.cols)),
            DictValue::Num(u64::from(self.pctfree)),
            DictValue::Num(u64::from(self.itl_max)),
            DictValue::Bool(self.transactional),
            DictValue::Num(u64::from(self.logging)),
            DictValue::Num(u64::from(self.update_mode)),
            DictValue::Num(u64::from(self.retention)),
            DictValue::Num(u64::from(self.version_keep)),
            DictValue::Num(u64::from(self.cleanup)),
            DictValue::Num(u64::from(self.embed)),
            DictValue::Bool(self.shared),
        ]
    }
}

/// `col$` 的一行（列定义；§6 的 `ColumnDesc` 值域）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColRow {
    /// 对象号。
    pub obj: u32,
    /// 列号（1 起）。
    pub col: u32,
    /// 列名。
    pub name: String,
    /// 类型码（[`crate::dict::ColTypeCode`]）。
    pub type_code: u32,
    /// 声明长度（字节）。
    pub length: u32,
    /// 精度（`NUMBER`）。
    pub precision: Option<u32>,
    /// 标度（`NUMBER`）。
    pub scale: Option<u32>,
    /// 可空。
    pub nullable: bool,
    /// 默认值（**物理编码**）。
    pub deflt: Option<Vec<u8>>,
    /// 标志位。
    pub flags: u32,
}

impl ColRow {
    /// 由 `col$` 行的值域构造。
    pub fn from_values(v: &[DictValue]) -> Result<Self, CacheError> {
        if v.len() != crate::dict::COL_COLS.len() {
            return shape("col$", "<行>");
        }
        Ok(Self {
            obj: num32("col$", "obj#", &v[0])?,
            col: num32("col$", "col#", &v[1])?,
            name: text("col$", "name", &v[2])?,
            type_code: num32("col$", "type#", &v[3])?,
            length: num32("col$", "length", &v[4])?,
            precision: opt_num32("col$", "precision", &v[5])?,
            scale: opt_num32("col$", "scale", &v[6])?,
            nullable: boolean("col$", "nullable", &v[7])?,
            deflt: opt_bytes("col$", "deflt", &v[8])?,
            flags: num32("col$", "flags", &v[9])?,
        })
    }

    /// 回值域。
    #[must_use]
    pub fn to_values(&self) -> Vec<DictValue> {
        let opt = |x: Option<u32>| x.map_or(DictValue::Null, |n| DictValue::Num(u64::from(n)));
        vec![
            DictValue::Num(u64::from(self.obj)),
            DictValue::Num(u64::from(self.col)),
            DictValue::Text(self.name.clone()),
            DictValue::Num(u64::from(self.type_code)),
            DictValue::Num(u64::from(self.length)),
            opt(self.precision),
            opt(self.scale),
            DictValue::Bool(self.nullable),
            self.deflt.clone().map_or(DictValue::Null, DictValue::Bytes),
            DictValue::Num(u64::from(self.flags)),
        ]
    }
}

/// `ind$` 的一行（索引专有）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndRow {
    /// 索引对象号。
    pub obj: u32,
    /// 基表对象号。
    pub bobj: u32,
    /// 索引类别（[`crate::dict::index_kind`]）。
    pub type_code: u32,
    /// 键列数。
    pub cols: u32,
    /// 唯一。
    pub is_unique: bool,
    /// 状态（0 无效 / 1 有效；Move 失效落此）。
    pub status: u32,
    /// 表达式键来源（表达式索引才有）。
    pub expr_src: Option<Vec<u8>>,
}

impl IndRow {
    /// 由 `ind$` 行的值域构造。
    pub fn from_values(v: &[DictValue]) -> Result<Self, CacheError> {
        if v.len() != crate::dict::IND_COLS.len() {
            return shape("ind$", "<行>");
        }
        Ok(Self {
            obj: num32("ind$", "obj#", &v[0])?,
            bobj: num32("ind$", "bobj#", &v[1])?,
            type_code: num32("ind$", "type#", &v[2])?,
            cols: num32("ind$", "cols", &v[3])?,
            is_unique: boolean("ind$", "is_unique", &v[4])?,
            status: num32("ind$", "status", &v[5])?,
            expr_src: opt_bytes("ind$", "expr_src", &v[6])?,
        })
    }

    /// 回值域。
    #[must_use]
    pub fn to_values(&self) -> Vec<DictValue> {
        vec![
            DictValue::Num(u64::from(self.obj)),
            DictValue::Num(u64::from(self.bobj)),
            DictValue::Num(u64::from(self.type_code)),
            DictValue::Num(u64::from(self.cols)),
            DictValue::Bool(self.is_unique),
            DictValue::Num(u64::from(self.status)),
            self.expr_src
                .clone()
                .map_or(DictValue::Null, DictValue::Bytes),
        ]
    }
}

/// `icol$` 的一行（索引键列组成）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IcolRow {
    /// 索引对象号。
    pub obj: u32,
    /// 键内位置（1 起；表达式键与 `ind$.expr_src` 的序对应）。
    pub pos: u32,
    /// 列号（**表达式键为 0**）。
    pub col: u32,
    /// 降序标志。**V1.0 只支持升序**（`arch/09` 的升序索引口径）。
    pub is_desc: bool,
}

impl IcolRow {
    /// 由 `icol$` 行的值域构造。
    pub fn from_values(v: &[DictValue]) -> Result<Self, CacheError> {
        if v.len() != crate::dict::ICOL_COLS.len() {
            return shape("icol$", "<行>");
        }
        Ok(Self {
            obj: num32("icol$", "obj#", &v[0])?,
            pos: num32("icol$", "pos#", &v[1])?,
            col: num32("icol$", "col#", &v[2])?,
            is_desc: boolean("icol$", "is_desc", &v[3])?,
        })
    }

    /// 回值域。
    #[must_use]
    pub fn to_values(&self) -> Vec<DictValue> {
        vec![
            DictValue::Num(u64::from(self.obj)),
            DictValue::Num(u64::from(self.pos)),
            DictValue::Num(u64::from(self.col)),
            DictValue::Bool(self.is_desc),
        ]
    }
}

/// `seg$` 的一行（段头跳板）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegRow {
    /// 数据对象号。
    pub dataobj: u32,
    /// 段头所在文件。
    pub file_id: u32,
    /// 段头所在块。
    pub block_id: u32,
    /// 初始区数。
    pub iniexts: u32,
    /// 创建提交序号。
    pub ctime: u64,
}

impl SegRow {
    /// 由 `seg$` 行的值域构造。
    pub fn from_values(v: &[DictValue]) -> Result<Self, CacheError> {
        if v.len() != crate::dict::SEG_COLS.len() {
            return shape("seg$", "<行>");
        }
        Ok(Self {
            dataobj: num32("seg$", "dataobj#", &v[0])?,
            file_id: num32("seg$", "file_id", &v[1])?,
            block_id: num32("seg$", "block_id", &v[2])?,
            iniexts: num32("seg$", "iniexts", &v[3])?,
            ctime: num("seg$", "ctime", &v[4])?,
        })
    }

    /// 回值域。
    #[must_use]
    pub fn to_values(&self) -> Vec<DictValue> {
        vec![
            DictValue::Num(u64::from(self.dataobj)),
            DictValue::Num(u64::from(self.file_id)),
            DictValue::Num(u64::from(self.block_id)),
            DictValue::Num(u64::from(self.iniexts)),
            DictValue::Num(self.ctime),
        ]
    }
}

/// `undo$` 的一行（undo 段本身；**不进 `obj$`**）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoRow {
    /// 段号。
    pub seg: u32,
    /// 状态。
    pub status: u32,
    /// 段头所在文件。
    pub file_id: u32,
    /// 段头所在块。
    pub block_id: u32,
    /// 创建提交序号。
    pub ctime: u64,
}

impl UndoRow {
    /// 由 `undo$` 行的值域构造。
    pub fn from_values(v: &[DictValue]) -> Result<Self, CacheError> {
        if v.len() != crate::dict::UNDO_COLS.len() {
            return shape("undo$", "<行>");
        }
        Ok(Self {
            seg: num32("undo$", "seg#", &v[0])?,
            status: num32("undo$", "status", &v[1])?,
            file_id: num32("undo$", "file_id", &v[2])?,
            block_id: num32("undo$", "block_id", &v[3])?,
            ctime: num("undo$", "ctime", &v[4])?,
        })
    }

    /// 回值域。
    #[must_use]
    pub fn to_values(&self) -> Vec<DictValue> {
        vec![
            DictValue::Num(u64::from(self.seg)),
            DictValue::Num(u64::from(self.status)),
            DictValue::Num(u64::from(self.file_id)),
            DictValue::Num(u64::from(self.block_id)),
            DictValue::Num(self.ctime),
        ]
    }
}

// ───────────────────────────── 统计 ─────────────────────────────

/// 缓存统计（`V$ROWCACHE` 四件套）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CacheStats {
    /// 查请求数。
    pub gets: u64,
    /// 命中数。
    pub hits: u64,
    /// 未命中数。
    pub misses: u64,
    /// 修改数（写穿/失效造成的条目变更）。
    pub modifications: u64,
}

impl CacheStats {
    /// 命中率（`gets == 0` ⇒ 0）。
    #[must_use]
    pub fn hit_ratio(&self) -> f64 {
        if self.gets == 0 {
            0.0
        } else {
            self.hits as f64 / self.gets as f64
        }
    }

    /// 未命中率（`GETMISSES/GETS`；**只作诊断参考，不写成阈值**——证据 E8）。
    #[must_use]
    pub fn miss_ratio(&self) -> f64 {
        if self.gets == 0 {
            0.0
        } else {
            self.misses as f64 / self.gets as f64
        }
    }
}

/// 容量上限（**行数 + 字节双上限**；默认值是防御性的——字典行数天然有界于对象数）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheCaps {
    /// 每型最大条目数（`col$`/`icol$` 型的"一条"= 一个对象的整组）。
    pub max_rows: usize,
    /// 每型最大估算字节数。
    pub max_bytes: usize,
}

impl Default for CacheCaps {
    fn default() -> Self {
        Self {
            max_rows: 4096,
            max_bytes: 4 << 20, // 4 MiB
        }
    }
}

// ───────────────────────────── 条目与型状态 ─────────────────────────────

/// 键：`obj$`/`tab$`/`col$`/`ind$`/`icol$` 用对象号，`seg$` 用数据对象号，
/// `undo$` 用段号（**都是 u32 域**）。
type Key = u32;

#[derive(Debug, Clone)]
struct Entry {
    row: CachedRow,
    loaded_at: u64,
    generation: u64,
    last_used: u64,
    bytes: usize,
}

/// 一个条目承载的**行值**（按型的行类型）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum CachedRow {
    Obj(ObjRow),
    Tab(TabRow),
    Col(Vec<ColRow>),
    Ind(IndRow),
    Icol(Vec<IcolRow>),
    Seg(SegRow),
    Undo(UndoRow),
}

#[derive(Debug, Default)]
struct KindState {
    entries: HashMap<Key, Entry>,
    /// `obj$` 型专属：`(namespace, name)` → obj#。
    names: HashMap<(u32, String), Key>,
    tick: u64,
    bytes: usize,
    stats: CacheStats,
}

impl KindState {
    fn remove(&mut self, key: Key) -> Option<Entry> {
        let e = self.entries.remove(&key)?;
        self.bytes = self.bytes.saturating_sub(e.bytes);
        if let CachedRow::Obj(o) = &e.row {
            self.names.remove(&(o.namespace, o.name.clone()));
        }
        Some(e)
    }

    fn evict_to_caps(&mut self, caps: CacheCaps) {
        while self.entries.len() > caps.max_rows || self.bytes > caps.max_bytes {
            let Some(victim) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| *k)
            else {
                break;
            };
            self.remove(victim);
        }
    }
}

// ───────────────────────────── 缓存本体 ─────────────────────────────

/// 失效登记（DDL 报告"我改了谁"）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Change {
    /// 按对象号。
    Obj(Key),
    /// 按名字（`obj$` 型的 `by_name` 映射）。
    Name(u32, String),
}

#[derive(Debug, Default)]
struct Pending {
    /// 有没有登记（`false` ⇒ 世代不等时**全清**）。
    registered: bool,
    changes: Vec<Change>,
}

/// **每工作区一份的字典行缓存**（§4.2）。
#[derive(Debug)]
pub struct RowCache {
    kinds: [Latch<KindState>; CacheKind::ALL.len()],
    generation: std::sync::atomic::AtomicU64,
    pending: Latch<Pending>,
    caps: CacheCaps,
}

impl RowCache {
    /// 建缓存（`open_generation` = 打开时的提交序号；§4.3）。
    #[must_use]
    pub fn new(open_generation: u64) -> Self {
        Self::with_caps(open_generation, CacheCaps::default())
    }

    /// 建缓存并指定容量上限。
    #[must_use]
    pub fn with_caps(open_generation: u64, caps: CacheCaps) -> Self {
        Self {
            kinds: std::array::from_fn(|_| Latch::new("row_cache", KindState::default())),
            generation: std::sync::atomic::AtomicU64::new(open_generation),
            pending: Latch::new("row_cache_gen", Pending::default()),
            caps,
        }
    }

    /// 当前世代号。
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation.load(std::sync::atomic::Ordering::Acquire)
    }

    /// **DDL 登记**：本事务改了哪些对象（对象号或名字）——须在提交钩子前调用。
    pub fn note_change_obj(&self, obj: u32) {
        let mut p = self.pending.lock();
        p.registered = true;
        p.changes.push(Change::Obj(obj));
    }

    /// **DDL 登记**（按名字）——对象号未知时（如"插入了新名字"）。
    pub fn note_change_name(&self, namespace: u32, name: &str) {
        let mut p = self.pending.lock();
        p.registered = true;
        p.changes.push(Change::Name(namespace, name.to_owned()));
    }

    /// **DDL 提交钩子**：世代自增（提交路径调用；先自增后放行——§4.3）。
    ///
    /// 未调 `note_change_*` 就自增 ⇒ 下一次读**全清**（保守正确）。
    pub fn bump_generation(&self) {
        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }

    /// 世代自增并**显式全清**（等价于"无登记"的语义，供粗粒度路径用）。
    pub fn bump_and_clear(&self) {
        {
            let mut p = self.pending.lock();
            p.registered = false;
            p.changes.clear();
        }
        self.bump_generation();
    }

    /// 清空全部型（**不**动世代——如关闭/重开装载）。
    pub fn clear(&self) {
        for k in CacheKind::ALL {
            let mut s = self.kinds[k.index()].lock();
            s.entries.clear();
            s.names.clear();
            s.bytes = 0;
        }
    }

    /// **入口：世代对齐**（每次读先走它）——把缓存推到与当前世代一致的状态。
    fn align(&self, kind: CacheKind) {
        let g = self.generation();
        let mut s = self.kinds[kind.index()].lock();
        let stale = s.entries.values().any(|e| e.generation != g);
        if !stale {
            return;
        }
        let mut p = self.pending.lock();
        if !p.registered {
            // 无登记 ⇒ 全清（保守正确）。
            s.entries.clear();
            s.names.clear();
            s.bytes = 0;
            p.registered = false;
            p.changes.clear();
            return;
        }
        // 有登记 ⇒ **精确失效**：只动登记过的键；其余条目刷新世代。
        let changes = std::mem::take(&mut p.changes);
        p.registered = false;
        for c in &changes {
            match c {
                Change::Obj(o) => {
                    let key = s
                        .entries
                        .iter()
                        .find(|(_, e)| entry_obj(&e.row) == *o)
                        .map(|(k, _)| *k);
                    if let Some(k) = key {
                        s.remove(k);
                    }
                }
                Change::Name(ns, name) => {
                    if let Some(k) = s.names.remove(&(*ns, name.clone())) {
                        s.remove(k);
                    }
                }
            }
        }
        for e in s.entries.values_mut() {
            e.generation = g;
        }
    }

    /// 取 `obj$` 行（`snapshot` 门槛见模块文档）。
    #[must_use]
    pub fn get_obj(&self, snapshot: CommitSeq, obj: u32) -> Option<ObjRow> {
        let row = self.get(kind_key(CacheKind::Obj, obj), snapshot)?;
        match row {
            CachedRow::Obj(o) => Some(o),
            _ => None,
        }
    }

    /// 按名取对象号（`obj$` 型的 `by_name` 映射）。
    #[must_use]
    pub fn get_obj_by_name(&self, snapshot: CommitSeq, namespace: u32, name: &str) -> Option<u32> {
        self.align(CacheKind::Obj);
        let mut s = self.kinds[CacheKind::Obj.index()].lock();
        s.stats.gets += 1;
        let Some(key) = s.names.get(&(namespace, name.to_owned())).copied() else {
            s.stats.misses += 1;
            return None;
        };
        let ok = s
            .entries
            .get(&key)
            .is_some_and(|e| snapshot.as_raw() >= e.loaded_at);
        if !ok {
            s.stats.misses += 1;
            return None;
        }
        let t = s.tick + 1;
        if let Some(e) = s.entries.get_mut(&key) {
            e.last_used = t;
        }
        s.tick = t;
        s.stats.hits += 1;
        Some(key)
    }

    /// 取 `tab$` 行。
    #[must_use]
    pub fn get_tab(&self, snapshot: CommitSeq, obj: u32) -> Option<TabRow> {
        match self.get(kind_key(CacheKind::Tab, obj), snapshot)? {
            CachedRow::Tab(t) => Some(t),
            _ => None,
        }
    }

    /// 取 `col$` 行组（按 obj# 成群）。
    #[must_use]
    pub fn get_cols(&self, snapshot: CommitSeq, obj: u32) -> Option<Vec<ColRow>> {
        match self.get(kind_key(CacheKind::Col, obj), snapshot)? {
            CachedRow::Col(c) => Some(c),
            _ => None,
        }
    }

    /// 取 `ind$` 行。
    #[must_use]
    pub fn get_ind(&self, snapshot: CommitSeq, obj: u32) -> Option<IndRow> {
        match self.get(kind_key(CacheKind::Ind, obj), snapshot)? {
            CachedRow::Ind(i) => Some(i),
            _ => None,
        }
    }

    /// 取 `icol$` 行组（按 obj# 成群）。
    #[must_use]
    pub fn get_icols(&self, snapshot: CommitSeq, obj: u32) -> Option<Vec<IcolRow>> {
        match self.get(kind_key(CacheKind::Icol, obj), snapshot)? {
            CachedRow::Icol(c) => Some(c),
            _ => None,
        }
    }

    /// 取 `seg$` 行（入口是**数据对象号**）。
    #[must_use]
    pub fn get_seg(&self, snapshot: CommitSeq, dataobj: u32) -> Option<SegRow> {
        match self.get(kind_key(CacheKind::Seg, dataobj), snapshot)? {
            CachedRow::Seg(s) => Some(s),
            _ => None,
        }
    }

    /// 取 `undo$` 行（入口是段号）。
    #[must_use]
    pub fn get_undo(&self, snapshot: CommitSeq, seg: u32) -> Option<UndoRow> {
        match self.get(kind_key(CacheKind::Undo, seg), snapshot)? {
            CachedRow::Undo(u) => Some(u),
            _ => None,
        }
    }

    /// **通用读取**（闩内查表 + 统计 + LRU 触碰；不做 I/O）。
    fn get(&self, (kind, key): (CacheKind, Key), snapshot: CommitSeq) -> Option<CachedRow> {
        self.align(kind);
        let mut s = self.kinds[kind.index()].lock();
        s.stats.gets += 1;
        let hit = s.entries.get(&key).is_some_and(|e| {
            // 快照门槛：读方快照 ≥ 装入点 ⇒ 命中即正确（单写者前提）。
            snapshot.as_raw() >= e.loaded_at && e.generation == self.generation()
        });
        if !hit {
            s.stats.misses += 1;
            return None;
        }
        let t = s.tick + 1;
        let row = {
            let e = s.entries.get_mut(&key)?;
            e.last_used = t;
            e.row.clone()
        };
        s.tick = t;
        s.stats.hits += 1;
        Some(row)
    }

    // ── 写穿（DDL 同事务内；§4.2 纪律 4）──────────────────────────

    /// 写穿 `obj$`（同时维护 `by_name` 映射）。
    pub fn put_obj(&self, loaded_at: CommitSeq, row: ObjRow) {
        let bytes = rows_bytes(std::slice::from_ref(&row.to_values()));
        self.put(
            CacheKind::Obj,
            row.obj,
            CachedRow::Obj(row),
            loaded_at,
            bytes,
        );
    }

    /// 写穿 `tab$`。
    pub fn put_tab(&self, loaded_at: CommitSeq, row: TabRow) {
        let bytes = rows_bytes(std::slice::from_ref(&row.to_values()));
        self.put(
            CacheKind::Tab,
            row.obj,
            CachedRow::Tab(row),
            loaded_at,
            bytes,
        );
    }

    /// 写穿 `col$`（整组）。
    pub fn put_cols(&self, loaded_at: CommitSeq, obj: u32, rows: Vec<ColRow>) {
        let vals: Vec<Vec<DictValue>> = rows.iter().map(ColRow::to_values).collect();
        self.put(
            CacheKind::Col,
            obj,
            CachedRow::Col(rows),
            loaded_at,
            rows_bytes(&vals),
        );
    }

    /// 写穿 `ind$`。
    pub fn put_ind(&self, loaded_at: CommitSeq, row: IndRow) {
        let bytes = rows_bytes(std::slice::from_ref(&row.to_values()));
        self.put(
            CacheKind::Ind,
            row.obj,
            CachedRow::Ind(row),
            loaded_at,
            bytes,
        );
    }

    /// 写穿 `icol$`（整组）。
    pub fn put_icols(&self, loaded_at: CommitSeq, obj: u32, rows: Vec<IcolRow>) {
        let vals: Vec<Vec<DictValue>> = rows.iter().map(IcolRow::to_values).collect();
        self.put(
            CacheKind::Icol,
            obj,
            CachedRow::Icol(rows),
            loaded_at,
            rows_bytes(&vals),
        );
    }

    /// 写穿 `seg$`。
    pub fn put_seg(&self, loaded_at: CommitSeq, row: SegRow) {
        let bytes = rows_bytes(std::slice::from_ref(&row.to_values()));
        self.put(
            CacheKind::Seg,
            row.dataobj,
            CachedRow::Seg(row),
            loaded_at,
            bytes,
        );
    }

    /// 写穿 `undo$`。
    pub fn put_undo(&self, loaded_at: CommitSeq, row: UndoRow) {
        let bytes = rows_bytes(std::slice::from_ref(&row.to_values()));
        self.put(
            CacheKind::Undo,
            row.seg,
            CachedRow::Undo(row),
            loaded_at,
            bytes,
        );
    }

    fn put(&self, kind: CacheKind, key: Key, row: CachedRow, loaded_at: CommitSeq, bytes: usize) {
        let mut s = self.kinds[kind.index()].lock();
        if let CachedRow::Obj(o) = &row {
            // 名字变了 ⇒ 旧映射要撤（改名/换命名空间）。
            let stale_name = match s.entries.get(&key).map(|old| &old.row) {
                Some(CachedRow::Obj(prev))
                    if prev.namespace != o.namespace || prev.name != o.name =>
                {
                    Some((prev.namespace, prev.name.clone()))
                }
                _ => None,
            };
            if let Some(name) = stale_name {
                s.names.remove(&name);
            }
            s.names.insert((o.namespace, o.name.clone()), key);
        }
        let tick = s.tick + 1;
        s.tick = tick;
        if let Some(old) = s.entries.remove(&key) {
            s.bytes = s.bytes.saturating_sub(old.bytes);
        }
        s.entries.insert(
            key,
            Entry {
                row,
                loaded_at: loaded_at.as_raw(),
                generation: self.generation(),
                last_used: tick,
                bytes,
            },
        );
        s.bytes += bytes;
        s.stats.modifications += 1;
        s.evict_to_caps(self.caps);
    }

    /// 某一型的统计。
    pub fn stats_of(&self, kind: CacheKind) -> CacheStats {
        self.kinds[kind.index()].lock().stats
    }

    /// 合计统计。
    #[must_use]
    pub fn stats(&self) -> CacheStats {
        let mut total = CacheStats::default();
        for k in CacheKind::ALL {
            let s = self.stats_of(k);
            total.gets += s.gets;
            total.hits += s.hits;
            total.misses += s.misses;
            total.modifications += s.modifications;
        }
        total
    }

    /// 某一型的条目数。
    pub fn len(&self, kind: CacheKind) -> usize {
        self.kinds[kind.index()].lock().entries.len()
    }

    /// 某一型是否为空。
    pub fn is_empty(&self, kind: CacheKind) -> bool {
        self.len(kind) == 0
    }

    /// 某一型的估算字节数。
    pub fn bytes(&self, kind: CacheKind) -> usize {
        self.kinds[kind.index()].lock().bytes
    }
}

/// 条目的对象号（用于精确失效：登记的是 `obj#`，而键可能是数据对象号/段号）。
fn entry_obj(row: &CachedRow) -> u32 {
    match row {
        CachedRow::Obj(o) => o.obj,
        CachedRow::Tab(t) => t.obj,
        CachedRow::Col(c) => c.first().map_or(0, |r| r.obj),
        CachedRow::Ind(i) => i.obj,
        CachedRow::Icol(c) => c.first().map_or(0, |r| r.obj),
        CachedRow::Seg(s) => s.dataobj,
        CachedRow::Undo(u) => u.seg,
    }
}

fn kind_key(kind: CacheKind, key: Key) -> (CacheKind, Key) {
    (kind, key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dict::{namespace, obj_kind};

    fn seq(v: u64) -> CommitSeq {
        CommitSeq::from_raw(v).unwrap()
    }

    fn obj_row(obj: u32, name: &str) -> ObjRow {
        ObjRow {
            obj,
            name: name.to_owned(),
            namespace: namespace::TABLE,
            type_code: obj_kind::TABLE,
            dataobj: obj,
            ctime: 0,
            mtime: 0,
            status: 1,
        }
    }

    #[test]
    fn hits_and_misses_are_counted_per_kind() {
        let c = RowCache::new(0);
        assert!(c.get_obj(seq(10), 7).is_none(), "空缓存 ⇒ miss");
        c.put_obj(seq(10), obj_row(7, "t$"));
        assert_eq!(c.get_obj(seq(10), 7).unwrap().name, "t$", "回填后命中");
        let s = c.stats_of(CacheKind::Obj);
        assert_eq!((s.gets, s.hits, s.misses, s.modifications), (2, 1, 1, 1));
        assert!((s.hit_ratio() - 0.5).abs() < f64::EPSILON);
        // **型分**：别的型不背 obj$ 的账。
        assert_eq!(c.stats_of(CacheKind::Tab).gets, 0);
        assert_eq!(c.stats().gets, 2, "合计只含 obj$ 的两次");
    }

    #[test]
    fn snapshot_below_loaded_at_does_not_hit() {
        let c = RowCache::new(0);
        c.put_obj(seq(100), obj_row(1, "a$"));
        assert!(c.get_obj(seq(50), 1).is_none(), "旧快照要旧版本 ⇒ 不命中");
        assert!(c.get_obj(seq(100), 1).is_some(), "快照 ≥ 装载点 ⇒ 命中");
        assert!(c.get_obj(seq(999), 1).is_some(), "更新的快照也命中");
        assert_eq!(c.stats_of(CacheKind::Obj).misses, 1);
    }

    #[test]
    fn name_index_serves_the_parse_hot_path() {
        let c = RowCache::new(0);
        c.put_obj(seq(5), obj_row(9, "obj$"));
        assert_eq!(c.get_obj_by_name(seq(5), namespace::TABLE, "obj$"), Some(9));
        assert_eq!(c.get_obj_by_name(seq(5), namespace::INDEX, "obj$"), None);
        assert_eq!(c.get_obj_by_name(seq(5), namespace::TABLE, "nope"), None);
        // 改名 ⇒ 旧名撤、新名立。
        let mut renamed = obj_row(9, "obj2$");
        renamed.mtime = 42;
        c.put_obj(seq(6), renamed);
        assert_eq!(c.get_obj_by_name(seq(6), namespace::TABLE, "obj$"), None);
        assert_eq!(
            c.get_obj_by_name(seq(6), namespace::TABLE, "obj2$"),
            Some(9)
        );
    }

    #[test]
    fn bump_without_registration_clears_all() {
        fn tab_row(obj: u32) -> TabRow {
            TabRow::from_values(&[
                DictValue::Num(u64::from(obj)),
                DictValue::Num(3),
                DictValue::Num(0),
                DictValue::Num(8),
                DictValue::Bool(true),
                DictValue::Num(u64::from(crate::dict::table_opt::LOGGING_FULL)),
                DictValue::Num(u64::from(crate::dict::table_opt::UPDATE_IN_PLACE)),
                DictValue::Num(0),
                DictValue::Num(0),
                DictValue::Num(u64::from(crate::dict::table_opt::CLEANUP_NONE)),
                DictValue::Num(u64::from(crate::dict::table_opt::EMBED_NONE)),
                DictValue::Bool(false),
            ])
            .unwrap()
        }
        let c = RowCache::new(0);
        c.put_obj(seq(1), obj_row(1, "a$"));
        c.put_tab(seq(1), tab_row(1));
        c.bump_generation();
        assert!(c.get_obj(seq(1), 1).is_none(), "无登记 ⇒ 全清");
        assert!(c.get_tab(seq(1), 1).is_none());
        assert_eq!(c.len(CacheKind::Obj), 0);
    }

    #[test]
    fn precise_invalidation_touches_only_registered_objects() {
        let c = RowCache::new(0);
        c.put_obj(seq(1), obj_row(1, "a$"));
        c.put_obj(seq(1), obj_row(2, "b$"));
        c.note_change_obj(1);
        c.bump_generation();
        assert!(
            c.get_obj(seq(1), 2).is_some(),
            "未登记的对象仍命中（世代刷新）"
        );
        assert!(c.get_obj(seq(1), 1).is_none(), "登记过的对象精确失效");
        // 名字映射随对象条目一起撤。
        assert_eq!(c.get_obj_by_name(seq(1), namespace::TABLE, "a$"), None);
        assert_eq!(c.get_obj_by_name(seq(1), namespace::TABLE, "b$"), Some(2));
    }

    #[test]
    fn precise_invalidation_by_name() {
        let c = RowCache::new(0);
        c.put_obj(seq(1), obj_row(3, "c$"));
        c.note_change_name(namespace::TABLE, "c$");
        c.bump_generation();
        assert_eq!(c.get_obj_by_name(seq(1), namespace::TABLE, "c$"), None);
        assert!(c.get_obj(seq(1), 3).is_none(), "名字登记也撤条目本体");
    }

    #[test]
    fn grouped_kinds_round_trip_and_invalidate_by_object() {
        let c = RowCache::new(0);
        let cols = vec![ColRow::from_values(&[
            DictValue::Num(5),
            DictValue::Num(1),
            DictValue::Text("a".to_owned()),
            DictValue::Num(u64::from(crate::dict::ColTypeCode::Number as u8)),
            DictValue::Num(0),
            DictValue::Null,
            DictValue::Null,
            DictValue::Bool(false),
            DictValue::Null,
            DictValue::Num(0),
        ])
        .unwrap()];
        c.put_cols(seq(1), 5, cols.clone());
        assert_eq!(c.get_cols(seq(1), 5).unwrap(), cols);
        assert!(c.get_cols(seq(1), 6).is_none());
        c.note_change_obj(5);
        c.bump_generation();
        assert!(
            c.get_cols(seq(2), 5).is_none(),
            "按 obj# 精确失效（条目里的 obj# 生效）"
        );
    }

    #[test]
    fn lru_evicts_under_row_and_byte_caps() {
        let caps = CacheCaps {
            max_rows: 2,
            max_bytes: 1 << 20,
        };
        let c = RowCache::with_caps(0, caps);
        c.put_obj(seq(1), obj_row(1, "a$"));
        c.put_obj(seq(1), obj_row(2, "b$"));
        // 触碰 1 ⇒ 最久未用是 2。
        assert!(c.get_obj(seq(1), 1).is_some());
        c.put_obj(seq(1), obj_row(3, "c$"));
        assert_eq!(c.len(CacheKind::Obj), 2, "行数上限生效");
        assert!(c.get_obj(seq(1), 2).is_none(), "最久未用者被淘汰");
        assert!(c.get_obj(seq(1), 1).is_some() && c.get_obj(seq(1), 3).is_some());
        // 字节上限：给一个极小的字节额度。
        let c2 = RowCache::with_caps(
            0,
            CacheCaps {
                max_rows: 100,
                max_bytes: 1,
            },
        );
        c2.put_obj(seq(1), obj_row(1, "a$"));
        assert_eq!(
            c2.len(CacheKind::Obj),
            0,
            "字节上限把刚装入的挤掉（防御性上限）"
        );
    }

    #[test]
    fn generation_moves_forward_and_clear_keeps_it() {
        let c = RowCache::new(77);
        assert_eq!(c.generation(), 77, "打开时 = 恢复后的提交序号");
        c.bump_generation();
        assert_eq!(c.generation(), 78);
        c.clear();
        assert_eq!(c.generation(), 78, "clear 不动世代");
        c.bump_and_clear();
        assert_eq!(c.generation(), 79);
    }
}
