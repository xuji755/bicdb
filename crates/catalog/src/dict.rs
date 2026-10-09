//! **字典表的内核常量**（`目录详设` §3；`arch/03` §3.1.2/§3.1.3）。
//!
//! **性质**：这些是**格式常量**（持久化在 `col$` / `ind$` / 引导页里）——
//! 一旦发布不可变更（或必须以 `format_version` 区分）。本模块是它们在代码里的
//! **唯一事实源**：建区时据此写字典行，打开时据此自检（`self_check`）。
//!
//! **一处分层说明**：`arch/03` 给出**语义**（有哪些表、回答什么问题）；本模块
//! 给出**可编码的常量**（表名、列号、类型码、键、选项码）。两者不一致即自检失败。

/// 列类型码（`col$.type#`；**内部编码，进持久格式**）。
///
/// **取值口径**：本项目自定顺序编号（1 起、连续、留扩展位）；**与 Oracle 的
/// `col$.type#` 数值无关**（那是 Oracle 内部编码，未核验、不照抄）。
/// `NUMBER` 的语义子类型（`INTEGER`/`FLOAT32`/`FLOAT64`）**共用 1 号**——
/// `REQ-TYP-003` 已定"与 `NUMBER` 共用同一编码"，差别由 `precision`/`scale` 表达。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ColTypeCode {
    /// 1 = `NUMBER`（含 `INTEGER`/`FLOAT32`/`FLOAT64` 子类型）。
    Number = 1,
    /// 2 = `CHAR(n)`（定长，空格填充）。
    Char = 2,
    /// 3 = `VARCHAR2(n)`（变长）。
    Varchar2 = 3,
    /// 4 = `DATE`（7B）。
    Date = 4,
    /// 5 = `TIMESTAMP(p)`。
    Timestamp = 5,
    /// 6 = `BOOLEAN`（本库扩展；Oracle SQL 层历史上无此类型）。
    Boolean = 6,
    /// 7 = `UUID`（本库扩展）。
    Uuid = 7,
    /// 8 = `TIMESTAMP_TZ`（UTC + 原始时区）。
    TimestampTz = 8,
    /// 9 = `JSON`（内部参照 PG `jsonb`；数值不经二进制浮点）。
    Json = 9,
    /// 10 = `VECTOR(n)`（维度写在类型里）。
    Vector = 10,
    /// 11 = `ASSET_REF`（6B 资产 ID；内容不进库）。
    AssetRef = 11,
    /// 12 = **字节串**——**字典表内部列类型**（如 `col$.deflt`），
    /// 不进用户可见类型系统（`arch/03` §3.1.3）。
    Bytes = 12,
}

impl ColTypeCode {
    /// 由码值取（未知码 ⇒ `None`——打开字典时未知类型即自检失败）。
    #[must_use]
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            1 => Self::Number,
            2 => Self::Char,
            3 => Self::Varchar2,
            4 => Self::Date,
            5 => Self::Timestamp,
            6 => Self::Boolean,
            7 => Self::Uuid,
            8 => Self::TimestampTz,
            9 => Self::Json,
            10 => Self::Vector,
            11 => Self::AssetRef,
            12 => Self::Bytes,
            _ => return None,
        })
    }

    /// 是否为**用户可见**类型（`Bytes` 是字典内部列专用）。
    #[must_use]
    pub fn user_visible(self) -> bool {
        self != Self::Bytes
    }

    /// 类型名（错误文案/诊断）。
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Number => "NUMBER",
            Self::Char => "CHAR",
            Self::Varchar2 => "VARCHAR2",
            Self::Date => "DATE",
            Self::Timestamp => "TIMESTAMP",
            Self::Boolean => "BOOLEAN",
            Self::Uuid => "UUID",
            Self::TimestampTz => "TIMESTAMP_TZ",
            Self::Json => "JSON",
            Self::Vector => "VECTOR",
            Self::AssetRef => "ASSET_REF",
            Self::Bytes => "BYTES(内部)",
        }
    }
}

/// **对象种类码**（`obj$.type#`）——`arch/03` §3.1.3：**照 Oracle 预留**，
/// V1.0 不支持的也留号、不挪用。
pub mod obj_kind {
    /// 索引。
    pub const INDEX: u32 = 1;
    /// 表。
    pub const TABLE: u32 = 2;
    /// bicdb 原生命名图（不占用 Oracle 预留类型码）。
    pub const GRAPH: u32 = 256;
    /// Protected native full-text document heap, owned by a graph index.
    pub const GRAPH_FULLTEXT_DATA: u32 = 257;
    /// Protected graph-wide full-text change journal heap.
    pub const GRAPH_FULLTEXT_QUEUE: u32 = 258;
    /// 簇（留位）。
    pub const CLUSTER: u32 = 3;
    /// 视图（留位）。
    pub const VIEW: u32 = 4;
    /// 同义词（留位）。
    pub const SYNONYM: u32 = 5;
    /// 序列（留位；`seq$` 是系统表、不走 `obj$`）。
    pub const SEQUENCE: u32 = 6;
    /// 过程（留位）。
    pub const PROCEDURE: u32 = 7;
    /// 函数（留位）。
    pub const FUNCTION: u32 = 8;
    /// 包（留位）。
    pub const PACKAGE: u32 = 9;
    /// 包体（留位）。
    pub const PACKAGE_BODY: u32 = 11;
    /// 触发器（留位）。
    pub const TRIGGER: u32 = 12;
    /// 类型（留位）。
    pub const TYPE: u32 = 13;
    /// 类型体（留位）。
    pub const TYPE_BODY: u32 = 14;
    /// 表分区（留位）。
    pub const TABLE_PARTITION: u32 = 19;
    /// 索引分区（留位）。
    pub const INDEX_PARTITION: u32 = 20;
    /// 库（留位）。
    pub const LIBRARY: u32 = 22;
    /// 目录（留位）。
    pub const DIRECTORY: u32 = 23;
    /// **对象号的两个方向**（`arch/03` §3.1.3）：自举对象 ≤ 99、用户对象 ≥ 100。
    pub const BOOTSTRAP_MAX: u32 = 99;
    /// 用户对象起始号。
    pub const USER_FIRST: u32 = 100;
}

/// **索引种类码**（`ind$.type#`）。
pub mod index_kind {
    /// B+Tree。
    pub const BTREE: u32 = 0;
    /// ANN（向量）。
    pub const ANN: u32 = 1;
    /// 邻接（图的边）。
    pub const ADJACENCY: u32 = 2;
    /// Graph property B-tree; leaf payload is a 48-bit element ID, not a heap ROWID.
    pub const GRAPH_PROPERTY: u32 = 256;
    /// Graph node directory (global and per-label); leaf payload is an element ID.
    pub const GRAPH_NODES: u32 = 257;
    /// Graph outgoing adjacency; key is endpoint and relationship type.
    pub const GRAPH_OUT: u32 = 258;
    /// Graph incoming adjacency; key is endpoint and relationship type.
    pub const GRAPH_IN: u32 = 259;
    /// Native full-text postings; payload is an element ID, not a heap ROWID.
    pub const GRAPH_FULLTEXT: u32 = 260;
    /// Graph-owned type-3 authority segment (not a B-tree).
    pub const GRAPH_ADJACENCY: u32 = 261;
    /// Physical source ROWID -> snapshot-bearing source metadata heap ROWID.
    pub const GRAPH_SOURCE_ENTRY: u32 = 262;
    /// Edge ID -> stable type-5 directory ROWID.
    pub const GRAPH_EDGE_LOCATOR: u32 = 263;
    /// Physical destination/type/source/edge reverse candidates.
    pub const GRAPH_REVERSE: u32 = 264;
    /// These trees are maintained by graph execution, never by ordinary heap DML.
    pub const fn is_graph_auxiliary(kind: u32) -> bool {
        matches!(
            kind,
            GRAPH_PROPERTY
                | GRAPH_NODES
                | GRAPH_OUT
                | GRAPH_IN
                | GRAPH_FULLTEXT
                | GRAPH_ADJACENCY
                | GRAPH_SOURCE_ENTRY
                | GRAPH_EDGE_LOCATOR
                | GRAPH_REVERSE
        )
    }
}

/// **命名空间**（`obj$.namespace`）——表与索引各自独立（可同名）。
pub mod namespace {
    /// 表。
    pub const TABLE: u32 = 1;
    /// 索引。
    pub const INDEX: u32 = 2;
}

/// **表选项码**（`tab$` 的各枚举列；`arch/03` §3.1.3 / `arch/08`）。
pub mod table_opt {
    /// `logging`：全量 redo。
    pub const LOGGING_FULL: u32 = 0;
    /// `logging`：只记必要 redo（统计等可丢表）。
    pub const LOGGING_REDO_ONLY: u32 = 1;
    /// `logging`：不记 redo。
    pub const LOGGING_NONE: u32 = 2;
    /// `update_mode`：就地更新。
    pub const UPDATE_IN_PLACE: u32 = 0;
    /// `update_mode`：只追加。
    pub const UPDATE_APPEND_ONLY: u32 = 1;
    /// `cleanup`：不清（保留）。
    pub const CLEANUP_NONE: u32 = 0;
    /// `cleanup`：按段丢弃。
    pub const CLEANUP_SEGMENT_DROP: u32 = 1;
    /// `cleanup`：按行删除。
    pub const CLEANUP_ROW_DELETE: u32 = 2;
    /// `embed`：不嵌入。
    pub const EMBED_NONE: u32 = 0;
    /// `embed`：异步嵌入。
    pub const EMBED_ASYNC: u32 = 1;
}

/// 一列的定义（字典表自身的列——**内核常量**）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColDef {
    /// 列号（自 1 起，连续）。
    pub col: u16,
    /// 列名。
    pub name: &'static str,
    /// 类型码。
    pub type_code: ColTypeCode,
    /// 可变长列的声明上限（`CHAR`/`VARCHAR2` 的字节数；其余 0）。
    pub length: u32,
    /// 可空。
    pub nullable: bool,
}

/// 一个键（唯一索引）的定义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyDef {
    /// 索引名（**名字保留**；`obj$` 的两个键见下）。
    pub name: &'static str,
    /// 键列号（按序）。
    pub cols: &'static [u16],
    /// 是否唯一（V1.0 字典表的键**全部唯一**——主键也不过是一个唯一索引）。
    pub unique: bool,
}

/// 一张字典表（内核常量）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DictTable {
    /// 表名（**保留名**：`$` 结尾或预置名）。
    pub name: &'static str,
    /// 列定义（按列号序）。
    pub columns: &'static [ColDef],
    /// 键（唯一索引）。
    pub keys: &'static [KeyDef],
    /// 是否**自举集**成员（引导页给出段头位置；`stat$`/`seq$` 由自举层描述，不在内）。
    pub bootstrap: bool,
}

const fn col(col: u16, name: &'static str, t: ColTypeCode, len: u32, nullable: bool) -> ColDef {
    ColDef {
        col,
        name,
        type_code: t,
        length: len,
        nullable,
    }
}

// ───────────────────── 自举层七张（arch/03 §3.1.3）─────────────────────

/// `obj$` —— 所有对象的根定义。
pub static OBJ_COLS: &[ColDef] = &[
    col(1, "obj#", ColTypeCode::Number, 0, false),
    col(2, "name", ColTypeCode::Varchar2, 128, false),
    col(3, "namespace", ColTypeCode::Number, 0, false),
    col(4, "type#", ColTypeCode::Number, 0, false),
    col(5, "dataobj#", ColTypeCode::Number, 0, false),
    col(6, "ctime", ColTypeCode::Number, 0, false),
    col(7, "mtime", ColTypeCode::Number, 0, false),
    col(8, "status", ColTypeCode::Number, 0, false),
];
/// `obj$` 的键：按号 + **按名**（`(namespace, name)`——名字解析的热路径）。
pub static OBJ_KEYS: &[KeyDef] = &[
    KeyDef {
        name: "i_obj_pk",
        cols: &[1],
        unique: true,
    },
    KeyDef {
        name: "i_obj_name",
        cols: &[3, 2],
        unique: true,
    },
];

/// `tab$` —— 表专有（七项表选项在此）。
pub static TAB_COLS: &[ColDef] = &[
    col(1, "obj#", ColTypeCode::Number, 0, false),
    col(2, "cols", ColTypeCode::Number, 0, false),
    col(3, "pctfree", ColTypeCode::Number, 0, false),
    col(4, "itl_max", ColTypeCode::Number, 0, false),
    col(5, "transactional", ColTypeCode::Boolean, 0, false),
    col(6, "logging", ColTypeCode::Number, 0, false),
    col(7, "update_mode", ColTypeCode::Number, 0, false),
    col(8, "retention", ColTypeCode::Number, 0, false),
    col(9, "version_keep", ColTypeCode::Number, 0, false),
    col(10, "cleanup", ColTypeCode::Number, 0, false),
    col(11, "embed", ColTypeCode::Number, 0, false),
    col(12, "shared", ColTypeCode::Boolean, 0, false),
];
/// `tab$` 的键。
pub static TAB_KEYS: &[KeyDef] = &[KeyDef {
    name: "i_tab_pk",
    cols: &[1],
    unique: true,
}];

/// `col$` —— 列定义（`deflt` 存**物理编码**、一列装完）。
pub static COL_COLS: &[ColDef] = &[
    col(1, "obj#", ColTypeCode::Number, 0, false),
    col(2, "col#", ColTypeCode::Number, 0, false),
    col(3, "name", ColTypeCode::Varchar2, 128, false),
    col(4, "type#", ColTypeCode::Number, 0, false),
    col(5, "length", ColTypeCode::Number, 0, false),
    col(6, "precision", ColTypeCode::Number, 0, true),
    col(7, "scale", ColTypeCode::Number, 0, true),
    col(8, "nullable", ColTypeCode::Boolean, 0, false),
    col(9, "deflt", ColTypeCode::Bytes, 0, true),
    col(10, "flags", ColTypeCode::Number, 0, false),
];
/// `col$` 的键：`(obj#, col#)` —— "取某对象的全部列" = 一次范围扫。
pub static COL_KEYS: &[KeyDef] = &[KeyDef {
    name: "i_col_pk",
    cols: &[1, 2],
    unique: true,
}];

/// `ind$` —— 索引专有。
pub static IND_COLS: &[ColDef] = &[
    col(1, "obj#", ColTypeCode::Number, 0, false),
    col(2, "bobj#", ColTypeCode::Number, 0, false),
    col(3, "type#", ColTypeCode::Number, 0, false),
    col(4, "cols", ColTypeCode::Number, 0, false),
    col(5, "is_unique", ColTypeCode::Boolean, 0, false),
    col(6, "status", ColTypeCode::Number, 0, false),
    // 表达式索引的键来源（`icol$.col# = 0` 时按 `pos#` 对应；
    // `目录详设` §5.3 的评审点——字典不能引用 AST，键必须可重建）。
    col(7, "expr_src", ColTypeCode::Bytes, 0, true),
];
/// `ind$` 的键。
pub static IND_KEYS: &[KeyDef] = &[KeyDef {
    name: "i_ind_pk",
    cols: &[1],
    unique: true,
}];

/// `icol$` —— 索引键列组成。
pub static ICOL_COLS: &[ColDef] = &[
    col(1, "obj#", ColTypeCode::Number, 0, false),
    col(2, "pos#", ColTypeCode::Number, 0, false),
    col(3, "col#", ColTypeCode::Number, 0, false),
    col(4, "is_desc", ColTypeCode::Boolean, 0, false),
];
/// `icol$` 的键。
pub static ICOL_KEYS: &[KeyDef] = &[KeyDef {
    name: "i_icol_pk",
    cols: &[1, 2],
    unique: true,
}];

/// `seg$` —— 持久段登记（表段/索引段；undo 走 `undo$`、临时段不入字典）。
pub static SEG_COLS: &[ColDef] = &[
    col(1, "dataobj#", ColTypeCode::Number, 0, false),
    col(2, "file_id", ColTypeCode::Number, 0, false),
    col(3, "block_id", ColTypeCode::Number, 0, false),
    col(4, "iniexts", ColTypeCode::Number, 0, false),
    col(5, "ctime", ColTypeCode::Number, 0, false),
];
/// `seg$` 的键（入口是数据对象号）。
pub static SEG_KEYS: &[KeyDef] = &[KeyDef {
    name: "i_seg_pk",
    cols: &[1],
    unique: true,
}];

/// `undo$` —— Undo 段本身（V1.0 单段、位置固定；**不进 `obj$`**）。
pub static UNDO_COLS: &[ColDef] = &[
    col(1, "seg#", ColTypeCode::Number, 0, false),
    col(2, "status", ColTypeCode::Number, 0, false),
    col(3, "file_id", ColTypeCode::Number, 0, false),
    col(4, "block_id", ColTypeCode::Number, 0, false),
    col(5, "ctime", ColTypeCode::Number, 0, false),
];
/// `undo$` 的键。
pub static UNDO_KEYS: &[KeyDef] = &[KeyDef {
    name: "i_undo_pk",
    cols: &[1],
    unique: true,
}];

// ───────────────────── 普通字典表（由自举层描述）─────────────────────

/// `stat$` —— 启发式统计（**唯一一张非事务表**：`logging = redo_only`，可丢）。
pub static STAT_COLS: &[ColDef] = &[
    col(1, "obj#", ColTypeCode::Number, 0, false),
    col(2, "row_est", ColTypeCode::Number, 0, false),
    col(3, "access_cnt", ColTypeCode::Number, 0, false),
    col(4, "last_access", ColTypeCode::Timestamp, 0, true),
    col(5, "updated_at", ColTypeCode::Timestamp, 0, true),
];
/// `stat$` 的键。
pub static STAT_KEYS: &[KeyDef] = &[KeyDef {
    name: "i_stat_pk",
    cols: &[1],
    unique: true,
}];

/// `seq$` —— 系统序列（对外 ID 的分配者；**不提供用户 `CREATE SEQUENCE`**）。
pub static SEQ_COLS: &[ColDef] = &[
    col(1, "seq#", ColTypeCode::Number, 0, false),
    col(2, "name", ColTypeCode::Varchar2, 64, false),
    col(3, "next_val", ColTypeCode::Number, 0, false),
    col(4, "cache", ColTypeCode::Number, 0, false),
    col(5, "flags", ColTypeCode::Number, 0, false),
];
/// `seq$` 的键。
pub static SEQ_KEYS: &[KeyDef] = &[KeyDef {
    name: "i_seq_pk",
    cols: &[1],
    unique: true,
}];

// ───────────────────── public 独有的三张（arch/03 §3.1.3）─────────────────────

/// `user$` —— 主体。
pub static USER_COLS: &[ColDef] = &[
    col(1, "user_id", ColTypeCode::Number, 0, false),
    col(2, "name", ColTypeCode::Varchar2, 128, false),
    col(3, "passwd", ColTypeCode::Varchar2, 256, false),
    col(4, "status", ColTypeCode::Number, 0, false),
    col(5, "ctime", ColTypeCode::Timestamp, 0, false),
];
/// `user$` 的键：主键 + **登录名唯一**。
pub static USER_KEYS: &[KeyDef] = &[
    KeyDef {
        name: "i_user_pk",
        cols: &[1],
        unique: true,
    },
    KeyDef {
        name: "i_user_name",
        cols: &[2],
        unique: true,
    },
];

/// `ws$` —— 工作区登记（**v0.2 形态**：`DCL语句设计` v0.2 §4 的格式变更）。
///
/// 两处与旧形态不同，都是**依赖顺序 `FS → WORKSPACE → USER` 的推论**：
/// - `user_id` **可空**：建区时**无主**（属主只在 `CREATE USER … USING WORKSPACE`
///   一处落定）；`NULL` = 无主容器（不可打开，等待绑定）；
/// - `name` **NOT NULL + 实例内唯一**：建区那一刻还没有属主，名字没法用属主限定。
pub static WS_COLS: &[ColDef] = &[
    col(1, "workspace_id", ColTypeCode::Number, 0, false),
    col(2, "user_id", ColTypeCode::Number, 0, true), // NULL = 无主（等待 CREATE USER 绑定）
    col(3, "name", ColTypeCode::Varchar2, 128, false),
    col(4, "status", ColTypeCode::Number, 0, false),
    col(5, "ctime", ColTypeCode::Timestamp, 0, false),
    col(6, "quota_data", ColTypeCode::Number, 0, false),
    col(7, "quota_undo", ColTypeCode::Number, 0, false),
    col(8, "quota_temp", ColTypeCode::Number, 0, false),
    col(9, "quota_asset", ColTypeCode::Number, 0, false),
    // W4：新数据文件默认落哪块盘（`fs$` 的槽位号；NULL = 实例默认盘）。
    col(10, "default_fs", ColTypeCode::Number, 0, true),
];
/// `ws$` 的键：主键 + **名字实例内唯一**（v0.2：不是"属主内唯一"——见上）。
pub static WS_KEYS: &[KeyDef] = &[
    KeyDef {
        name: "i_ws_pk",
        cols: &[1],
        unique: true,
    },
    KeyDef {
        name: "i_ws_name",
        cols: &[3],
        unique: true,
    },
];

/// `fs$` —— 文件系统池（实例级资源；**位置权威在全局控制文件**，这里供查询/审计）。
///
/// **v0.2 形态**（`DCL语句设计` v0.2 §4）：**名字是标识**（`CREATE FILESYSTEM <名>
/// USING '<路径>'` 给的名字，实例内唯一）、**路径只是创建参数**（`mount_path`
/// 因此改名 `path`，并可只是一个目录）。
pub static FS_COLS: &[ColDef] = &[
    col(1, "fs_slot", ColTypeCode::Number, 0, false),
    col(2, "name", ColTypeCode::Varchar2, 128, false),
    col(3, "path", ColTypeCode::Varchar2, 256, false),
    col(4, "status", ColTypeCode::Number, 0, false),
    col(5, "total_bytes", ColTypeCode::Number, 0, false),
    col(6, "free_bytes", ColTypeCode::Number, 0, true), // 缓存值，权威是文件系统本身
    col(7, "allocate", ColTypeCode::Number, 0, false),  // 1 = ON / 0 = OFF（F2 的排水阀）
];
/// `fs$` 的键：主键 + **名字唯一** + **路径唯一**（同一目录登记两次没有意义）。
pub static FS_KEYS: &[KeyDef] = &[
    KeyDef {
        name: "i_fs_pk",
        cols: &[1],
        unique: true,
    },
    KeyDef {
        name: "i_fs_name",
        cols: &[2],
        unique: true,
    },
    KeyDef {
        name: "i_fs_path",
        cols: &[3],
        unique: true,
    },
];

/// `wq$` —— **盘级配额**（工作区 × 文件系统 × 上限；`DCL语句设计` v0.2 §4）。
///
/// 与 `ws$` 的四列（**角色级**：data/undo/temp/asset）构成两级配额。
/// 键 = `(workspace_id, fs_slot)`：一个区在一块盘上至多一条。
pub static WQ_COLS: &[ColDef] = &[
    col(1, "workspace_id", ColTypeCode::Number, 0, false),
    col(2, "fs_slot", ColTypeCode::Number, 0, false),
    col(3, "quota_bytes", ColTypeCode::Number, 0, false),
];
/// `wq$` 的键：主键 = `(工作区, 盘槽位)`。
pub static WQ_KEYS: &[KeyDef] = &[KeyDef {
    name: "i_wq_pk",
    cols: &[1, 2],
    unique: true,
}];

/// **全部字典表**（普通工作区九张 + `public` 独有三张；固定表 `file$` 不在此，
/// 它不落盘——`arch/03` §3.1.4）。
pub static DICT_TABLES: &[DictTable] = &[
    DictTable {
        name: "obj$",
        columns: OBJ_COLS,
        keys: OBJ_KEYS,
        bootstrap: true,
    },
    DictTable {
        name: "tab$",
        columns: TAB_COLS,
        keys: TAB_KEYS,
        bootstrap: true,
    },
    DictTable {
        name: "col$",
        columns: COL_COLS,
        keys: COL_KEYS,
        bootstrap: true,
    },
    DictTable {
        name: "ind$",
        columns: IND_COLS,
        keys: IND_KEYS,
        bootstrap: true,
    },
    DictTable {
        name: "icol$",
        columns: ICOL_COLS,
        keys: ICOL_KEYS,
        bootstrap: true,
    },
    DictTable {
        name: "seg$",
        columns: SEG_COLS,
        keys: SEG_KEYS,
        bootstrap: true,
    },
    DictTable {
        name: "undo$",
        columns: UNDO_COLS,
        keys: UNDO_KEYS,
        bootstrap: true,
    },
    DictTable {
        name: "stat$",
        columns: STAT_COLS,
        keys: STAT_KEYS,
        bootstrap: false,
    },
    DictTable {
        name: "seq$",
        columns: SEQ_COLS,
        keys: SEQ_KEYS,
        bootstrap: false,
    },
    DictTable {
        name: "user$",
        columns: USER_COLS,
        keys: USER_KEYS,
        bootstrap: true, // public 工作区的自举集成员（普通工作区不建）
    },
    DictTable {
        name: "ws$",
        columns: WS_COLS,
        keys: WS_KEYS,
        bootstrap: true,
    },
    DictTable {
        name: "fs$",
        columns: FS_COLS,
        keys: FS_KEYS,
        bootstrap: true,
    },
    DictTable {
        name: "wq$",
        columns: WQ_COLS,
        keys: WQ_KEYS,
        bootstrap: true,
    },
];

/// **普通工作区的自举条目数**：7 张表 + 8 个索引 = **15**（`目录详设` §2.2）。
///
/// `public`（管理面）另加 4 表 + 8 索引 = 12 ⇒ **27**（v0.2：`user$`/`ws$`/`fs$`/`wq$`）。
#[must_use]
pub fn bootstrap_entries_normal() -> usize {
    DICT_TABLES
        .iter()
        .filter(|t| t.bootstrap && !is_public_only(t.name))
        .map(|t| 1 + t.keys.len())
        .sum()
}

/// **`public` 工作区的自举条目数**：普通 15 + 三张表（user$/ws$/fs$）+ 其 5 个索引 = **23**。
#[must_use]
pub fn bootstrap_entries_public() -> usize {
    DICT_TABLES
        .iter()
        .filter(|t| t.bootstrap)
        .map(|t| 1 + t.keys.len())
        .sum()
}

/// `public` 独有的三张（普通工作区不建）。
#[must_use]
pub fn is_public_only(name: &str) -> bool {
    matches!(name, "user$" | "ws$" | "fs$" | "wq$")
}

/// **自举计划**（按建区顺序展开）：`(表, 其每个键)`——**表先、索引后**。
///
/// **顺序即身份**：引导页条目只有 `dataobj#`；建区（`create`）与打开
/// （`open`）用**同一份计划**对位，因此这里的顺序是**格式的一部分**。
#[must_use]
pub fn bootstrap_plan(is_public: bool) -> Vec<(&'static DictTable, Option<KeyDef>)> {
    let mut out = Vec::new();
    for t in DICT_TABLES {
        if !t.bootstrap {
            continue; // stat$/seq$ 由自举层描述（DDL 路径建），不在引导页
        }
        if is_public_only(t.name) && !is_public {
            continue; // 普通工作区不建 public 独有三张
        }
        out.push((t, None));
        for k in t.keys {
            out.push((t, Some(*k)));
        }
    }
    out
}

/// **单表自检**（`self_check` 的判据本体；测试可直接喂坏表验证判据）。
///
/// `seen_keys` 跨表累积（键名全局唯一）。检查项见 [`self_check`]。
pub fn check_table(t: &DictTable, seen_keys: &mut Vec<&'static str>) -> Result<(), String> {
    if !t.name.ends_with('$') && !is_public_only(t.name) {
        return Err(format!("字典表名必须是保留名（`$` 结尾）：{}", t.name));
    }
    for (i, c) in t.columns.iter().enumerate() {
        if c.col as usize != i + 1 {
            return Err(format!(
                "{}：列号必须自 1 连续（第 {} 项是 {}）",
                t.name,
                i + 1,
                c.col
            ));
        }
    }
    for (i, c) in t.columns.iter().enumerate() {
        if t.columns[i + 1..].iter().any(|o| o.name == c.name) {
            return Err(format!("{}：列名重复 {}", t.name, c.name));
        }
    }
    for k in t.keys {
        if seen_keys.contains(&k.name) {
            return Err(format!("键名重复：{}", k.name));
        }
        seen_keys.push(k.name);
        for &c in k.cols {
            if !t.columns.iter().any(|col| col.col == c) {
                return Err(format!("{}：键 {} 引用不存在的列号 {c}", t.name, k.name));
            }
        }
    }
    if t.bootstrap && !t.keys.iter().any(|k| k.unique) {
        return Err(format!(
            "自举表 {} 必须有唯一索引（arch/03 §3.1.1）",
            t.name
        ));
    }
    Ok(())
}

/// **格式常量自检**（打开工作区时跑一遍；失败即拒绝打开——响亮，不静默）。
///
/// 检查项：
/// 1. 表名唯一、**保留名**（`$` 结尾 或 预置名）；
/// 2. 列号自 1 **连续**、列名唯一、类型码已知；
/// 3. 每张表的键**列号都指向存在的列**、键名唯一；
/// 4. **每张自举表至少一个唯一索引**（否则自举后仍无法按名/按号定位——
///    `arch/03` §3.1.1 的硬要求）；
/// 5. 条目数与设计的定量（普通 15 / public 23）一致。
pub fn self_check() -> Result<(), String> {
    let mut seen_tables = Vec::new();
    let mut seen_keys = Vec::new();
    for t in DICT_TABLES {
        if seen_tables.contains(&t.name) {
            return Err(format!("表名重复：{}", t.name));
        }
        seen_tables.push(t.name);
        check_table(t, &mut seen_keys)?;
    }
    if bootstrap_entries_normal() != 15 {
        return Err(format!(
            "普通工作区自举条目数应为 15，实为 {}",
            bootstrap_entries_normal()
        ));
    }
    if bootstrap_entries_public() != 27 {
        return Err(format!(
            "public 自举条目数应为 27，实为 {}",
            bootstrap_entries_public()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_constants_self_check_passes() {
        self_check().expect("内核常量自检通过");
    }

    #[test]
    fn bootstrap_set_sizes_match_the_design() {
        // 普通 15 = 7 表 + 8 索引；
        // public 27 = 15 + 4 表（user$/ws$/fs$/wq$）+ 8 索引（目录详设 §2.2 + v0.2）。
        assert_eq!(bootstrap_entries_normal(), 15);
        assert_eq!(bootstrap_entries_public(), 27);
        assert_eq!(OBJ_KEYS.len(), 2, "obj$ 两个键：按号 + 按名");
        assert!(OBJ_KEYS.iter().any(|k| k.cols == [1]));
        assert!(
            OBJ_KEYS.iter().any(|k| k.cols == [3, 2]),
            "按名：(namespace, name)"
        );
    }

    #[test]
    fn type_codes_are_stable_and_bytes_is_internal() {
        assert_eq!(ColTypeCode::Number as u8, 1);
        assert_eq!(ColTypeCode::Varchar2 as u8, 3);
        assert_eq!(ColTypeCode::Bytes as u8, 12);
        assert!(!ColTypeCode::Bytes.user_visible(), "BYTES 是字典内部列");
        assert!(ColTypeCode::Json.user_visible());
        for v in 1..=12u8 {
            let c = ColTypeCode::from_u8(v).expect("已知码");
            assert_eq!(c as u8, v, "码值往返一致");
        }
        assert!(ColTypeCode::from_u8(13).is_none(), "未定义码拒绝");
        assert!(ColTypeCode::from_u8(0).is_none());
    }

    /// 坏表 1：列号跳号（1、3）。
    static BAD_COLS: DictTable = DictTable {
        name: "x$",
        columns: &[
            col(1, "a", ColTypeCode::Number, 0, false),
            col(3, "b", ColTypeCode::Number, 0, false),
        ],
        keys: &[KeyDef {
            name: "k_bad_cols",
            cols: &[1],
            unique: true,
        }],
        bootstrap: false,
    };
    /// 坏表 2：自举表**无键**（arch/03 §3.1.1 的硬要求）。
    static BAD_NO_KEY: DictTable = DictTable {
        name: "y$",
        columns: &[col(1, "a", ColTypeCode::Number, 0, false)],
        keys: &[],
        bootstrap: true,
    };
    /// 坏表 3：表名不是保留名。
    static BAD_NAME: DictTable = DictTable {
        name: "plain",
        columns: &[col(1, "a", ColTypeCode::Number, 0, false)],
        keys: &[KeyDef {
            name: "k_bad_name",
            cols: &[1],
            unique: true,
        }],
        bootstrap: false,
    };
    /// 坏表 4：键引用不存在的列号。
    static BAD_KEY_COL: DictTable = DictTable {
        name: "z$",
        columns: &[col(1, "a", ColTypeCode::Number, 0, false)],
        keys: &[KeyDef {
            name: "k_bad_key_col",
            cols: &[9],
            unique: true,
        }],
        bootstrap: false,
    };

    #[test]
    fn check_table_rejects_broken_tables() {
        let mut seen = Vec::new();
        assert!(check_table(&BAD_COLS, &mut seen)
            .unwrap_err()
            .contains("连续"));
        assert!(check_table(&BAD_NO_KEY, &mut seen)
            .unwrap_err()
            .contains("唯一索引"));
        assert!(check_table(&BAD_NAME, &mut seen)
            .unwrap_err()
            .contains("保留名"));
        assert!(check_table(&BAD_KEY_COL, &mut seen)
            .unwrap_err()
            .contains("列号 9"));
        // 键名跨表唯一：同名键第二次即拒绝。
        let mut seen2 = Vec::new();
        assert!(check_table(&BAD_KEY_COL, &mut seen2).is_err());
        assert!(
            check_table(&BAD_KEY_COL, &mut seen2)
                .unwrap_err()
                .contains("键名重复"),
            "同名键再次登记即拒"
        );
    }

    #[test]
    fn public_only_tables_are_exactly_four() {
        let names: Vec<&str> = DICT_TABLES
            .iter()
            .filter(|t| is_public_only(t.name))
            .map(|t| t.name)
            .collect();
        assert_eq!(names, vec!["user$", "ws$", "fs$", "wq$"]);
    }

    #[test]
    fn obj_kind_reserved_numbers_follow_oracle() {
        // arch/03 §3.1.3：编号空间照 Oracle 预留，不挪用。
        assert_eq!(obj_kind::INDEX, 1);
        assert_eq!(obj_kind::TABLE, 2);
        assert_eq!(obj_kind::TABLE_PARTITION, 19);
        assert_eq!(obj_kind::DIRECTORY, 23);
        assert_eq!(obj_kind::BOOTSTRAP_MAX, 99);
        assert_eq!(obj_kind::USER_FIRST, 100);
        assert_eq!(index_kind::BTREE, 0);
        assert_eq!(namespace::TABLE, 1);
        assert_eq!(namespace::INDEX, 2);
    }
}
