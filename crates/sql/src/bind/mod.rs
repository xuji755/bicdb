//! **② Binder：名字解析三格 + 版本捕获**（`SQL前端设计` §4.1/§4.2；切片 S2）。
//!
//! ```text
//! 会话解析一个名字，按固定顺序查三处（REQ-SQL-003 第一步；spec/SQL §0.4）：
//!   ① 对象命名空间（obj$，namespace 1=表 / 2=索引）——用户对象 + 预置对象
//!   ② 固定表命名空间（保留名清单，**不在 obj$**）：file$ / session$ / lock$
//!   ③ 字典对象——经清单转为只读行源；管理清单仅本机管理身份可读。
//! ```
//!
//! **三条规则从这里直接得到**：
//!
//! - **找不到 = "不存在"**（不区分"别人的""已删除的""从不存在的"）——
//!   REQ-ISO-006 的不可区分；本模块只有一个 [`BindError::NotFound`] 出口；
//! - **固定表的"只读"不是检查、是"没有入口"**——[`NameResolver::resolve_write_target`]
//!   里**根本没有第 ② 格**（写目标只查 ①）；
//! - **保留清单**：以 `$` 结尾的名字 + 预置对象名，`CREATE` 一律拒绝
//!   （[`check_new_object_name`]）。
//!
//! **版本捕获（§4.2）**：每个解析到的对象记 `(obj#, mtime)`、每个用到的索引记
//! `(obj#, mtime, status)`（ANN 索引另加 generation——随 RET 切片）——这些就是
//! **计划缓存键的成分**（S6），也是"失效靠比对不靠通知"的落点。
//!
//! **已落地**：类型推导（动作 3）、参数定型（动作 4）——见 `bind::expr`；
//! **登记点（动作 6）** 的"计划缓存键"部分只保证**可观测**（`BoundRefs` 带
//! `(obj#, mtime)`），缓存本体随 S6。`public` 工作区的**管理元数据过滤**已由
//! 解析策略覆盖：PUBLIC 的 user$/ws$/fs$/wq$ 仅 admin；字典行源不含口令列。
//! `file$` 的 admin 例外见 [`ResolvePolicy`]。

use std::collections::{BTreeMap, BTreeSet};

pub mod catalog;
pub mod expr;
pub mod statement;

pub use catalog::CatalogViewImpl;
pub use expr::{bind_expr, kind_name, BindScope, BoundColumn, BoundParams};
pub use statement::{
    bind_statement, BoundDdl, BoundDelete, BoundFrom, BoundInsert, BoundJoin, BoundSelect,
    BoundSetKind, BoundSetOp, BoundStatement, BoundTable, BoundUpdate,
};

/// 命名空间（三格里的第 ① 格的两个子空间）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameSpace {
    /// 表（`obj$.namespace = 1`）。
    Table,
    /// 索引（`obj$.namespace = 2`）。
    Index,
}

impl NameSpace {
    /// 目录里的命名空间码。
    #[must_use]
    pub fn code(self) -> u32 {
        match self {
            NameSpace::Table => 1,
            NameSpace::Index => 2,
        }
    }
}

/// **固定表清单**（第 ② 格；`spec/SQL` §0.4 的保留名清单）。
///
/// `session$`/`lock$` 随会话层落地（`API` REQ-API-018）；名字现在就占用，
/// 免得将来再撞名。
pub const FIXED_TABLES: &[&str] = &["file$", "recovery$", "session$", "lock$", "attachment$"];

/// SQL-visible dictionary definitions. Physical credential columns are excluded.
pub fn dictionary_columns(name: &str) -> Option<Vec<CatalogColumn>> {
    let table = bicdb_catalog::dict::DICT_TABLES
        .iter()
        .find(|table| table.name == name)?;
    Some(
        table
            .columns
            .iter()
            .filter(|column| !(name == "user$" && column.name == "passwd"))
            .enumerate()
            .map(|(index, column)| CatalogColumn {
                col: index as u32 + 1,
                name: column.name.to_owned(),
                type_code: column.type_code as u32,
                length: column.length,
                nullable: column.nullable,
            })
            .collect(),
    )
}

/// **预置对象名**（工作区创建时建立；属主可读写、**不可删**、**名字保留**）。
pub const PRESET_OBJECTS: &[&str] = &[
    "memory",
    "memory_version",
    "task_checkpoint",
    "source_record",
    "derived_link",
    "session",
    "asset$",
    "ref$",
    "audit",
];

/// **保留名判定**（`$` 结尾 或 预置对象名）——`CREATE TABLE/GRAPH/INDEX` 拒绝。
#[must_use]
pub fn is_reserved_name(name: &str) -> bool {
    name.ends_with('$') || PRESET_OBJECTS.contains(&name)
}

/// `CREATE` 的新名字检查。
pub fn check_new_object_name(name: &str) -> Result<(), BindError> {
    if name.is_empty() || is_reserved_name(name) {
        return Err(BindError::ReservedName(name.to_owned()));
    }
    Ok(())
}

/// 绑定错误（**绑定期一律在这里报出**）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindError {
    /// **不存在**——跨区/已删/从未存在**不可区分**（REQ-ISO-006）。
    NotFound {
        /// 名字。
        name: String,
        /// 查的命名空间。
        ns: NameSpace,
    },
    /// 保留名（`$` 结尾或预置对象名）。
    ReservedName(String),
    /// 目录/存储层失败（**基础设施错误**——与"不存在"分开，响亮）。
    Catalog(String),
    /// 列名不在作用域里（**只针对已解析到的表**；"表不存在"是 `NotFound`）。
    UnknownColumn(String),
    /// **列名有歧义**（两表连接时同名列没加限定名）——照 SQL 标准：拒绝，不猜。
    AmbiguousColumn(String),
    /// **参数无类型上下文**（如 `SELECT :p`）——推导不出即拒绝（§4.3）。
    ParamWithoutContext(String),
    /// 同一参数多处使用推出**不同类型**（§4.3）。
    ParamTypeConflict {
        /// 参数名。
        name: String,
        /// 首次推出的形态。
        first: &'static str,
        /// 再次推出的形态。
        again: &'static str,
    },
    /// 字面量非法（TYP 内核拒绝——如越出 `NUMBER` 域）。
    BadLiteral {
        /// 原文本。
        text: String,
        /// 内核给的原因。
        why: String,
    },
    /// 形态不匹配（MVP：不做隐式提升；显式 `CAST` 是唯一转换入口）。
    TypeMismatch {
        /// 出错的位置（"比较两侧"/"IN 列表"…）。
        what: String,
        /// 期望形态。
        want: &'static str,
        /// 实际形态。
        got: &'static str,
    },
    /// **写目标不可写**（固定表/`asset$`/`ref$`/`audit`/自举/`public`）。
    NotWritable(String),
    /// 清单外的构造（绑定期兜底）；附原因。
    Unsupported(String),
}

impl std::fmt::Display for BindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BindError::NotFound { name, .. } => write!(f, "对象 `{name}` 不存在"),
            BindError::ReservedName(n) => write!(f, "名字 `{n}` 是保留名（`$` 结尾或预置名）"),
            BindError::Catalog(why) => write!(f, "目录读取失败：{why}"),
            BindError::UnknownColumn(n) => write!(f, "列 `{n}` 不存在"),
            BindError::AmbiguousColumn(n) => write!(
                f,
                "列 `{n}` 有歧义（两张表都有这一列）——加限定名，如 `t.{n}`"
            ),
            BindError::ParamWithoutContext(n) => {
                write!(f, "参数 `:{n}` 无类型上下文（推导不出类型）")
            }
            BindError::ParamTypeConflict { name, first, again } => write!(
                f,
                "参数 `:{name}` 多处使用推出不同类型（{first} / {again}）"
            ),
            BindError::BadLiteral { text, why } => write!(f, "字面量 `{text}` 非法：{why}"),
            BindError::TypeMismatch { what, want, got } => {
                write!(f, "{what} 的形态不匹配：需要 {want}、得到 {got}")
            }
            BindError::NotWritable(n) => write!(f, "`{n}` 不可写（固定表/预置只读对象/自举对象）"),
            BindError::Unsupported(why) => write!(f, "绑定期不支持：{why}"),
        }
    }
}

impl std::error::Error for BindError {}

/// **解析策略**（随会话身份；V1.0 无角色模型 ⇒ 默认属主视角）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResolvePolicy {
    /// 是不是 admin（`public` 的管理元数据 `file$` 只有 admin 可见）。
    pub is_admin: bool,
}

/// 目录里解析到的对象（Binder 的输入形态；`obj$.namespace = 1/2`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogObject {
    /// 对象号。
    pub obj: u32,
    /// 名字。
    pub name: String,
    /// 命名空间。
    pub namespace: NameSpace,
    /// 对象类型（`obj_kind`；Binder 据此判"是不是表/索引"）。
    pub type_code: u32,
    /// 数据对象号（0 = 无段）。
    pub dataobj: u32,
    /// 状态（1 = 有效）。
    pub status: u32,
    /// **最后修改提交序号**（版本捕获的成分）。
    pub mtime: u64,
}

/// 一列的目录形态（类型描述子由 TYP 内核转换——目录不解释类型语义）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogColumn {
    /// 列号（1 起）。
    pub col: u32,
    /// 列名。
    pub name: String,
    /// 类型码。
    pub type_code: u32,
    /// 声明长度。
    pub length: u32,
    /// 可空。
    pub nullable: bool,
}

/// 一个索引的目录形态（键列 + 唯一性 + 状态 + 版本）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogIndex {
    /// 索引对象号。
    pub obj: u32,
    /// 基表对象号。
    pub bobj: u32,
    /// 键列号（按位置序）。
    pub cols: Vec<u32>,
    /// 唯一。
    pub unique: bool,
    /// 状态（**`Move` 后为 0 ⇒ 不进选路**，`目录详设` §5.5）。
    pub status: u32,
    /// 最后修改提交序号。
    pub mtime: u64,
}

/// **目录只读面**（Binder 的取数口；ENG REQ-ENG-006 的形态）。
///
/// 真件实现 = [`CatalogViewImpl`]（`bicdb-catalog::Catalog`）；
/// 用例可用内存假件（本模块的测试）——**三格规则在本模块、不在实现里**。
pub trait CatalogView {
    /// 按名解析（**不可区分**：找不到一律 `NotFound`）。
    fn resolve(&mut self, ns: NameSpace, name: &str) -> Result<CatalogObject, BindError>;
    /// 列枚举（`SELECT *` / `INSERT` 无列名时用）。
    fn columns(&mut self, obj: u32) -> Result<Vec<CatalogColumn>, BindError>;
    /// 索引枚举（含 `status`）。
    fn indexes_of(&mut self, obj: u32) -> Result<Vec<CatalogIndex>, BindError>;
    /// 对象版本与状态（计划缓存键的成分）。
    fn object_version(&mut self, obj: u32) -> Result<(u64, u32), BindError>;
    /// **固定表的列定义**（第 ② 格；`None` = 不认识这张固定表）。
    ///
    /// 行不在目录里——**查询时由引擎即时产生**（`file$` 的内容 = 控制文件
    /// 内存映像），所以这里只回答"有没有这张表、有哪些列"。
    fn fixed_columns(&mut self, name: &str) -> Result<Option<Vec<CatalogColumn>>, BindError>;
    /// 是不是 `public` 工作区（`file$` 的可见性规则之一）。
    fn is_public(&self) -> bool;
    /// **段头块**（`seg$.block_id`）——物理计划开扫描用（活路径：`obj$` → `seg$`）。
    fn segment_block(&mut self, obj: u32) -> Result<u32, BindError>;
}

/// **版本捕获集**（`SQL前端设计` §4.2）：Bound 的组成之一、计划缓存键的成分。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BoundRefs {
    objects: BTreeMap<u32, u64>,
    indexes: BTreeMap<u32, (u64, u32)>,
    fixed_tables: BTreeSet<String>,
}

impl BoundRefs {
    /// 记一个对象（`(obj#, mtime)`）。
    pub fn note_object(&mut self, obj: u32, mtime: u64) {
        // 同一对象多次解析取**最老**版本（同一语句内不会变；取最老更保守）。
        self.objects
            .entry(obj)
            .and_modify(|m| *m = (*m).min(mtime))
            .or_insert(mtime);
    }

    /// 记一个索引（`(obj#, mtime, status)`）。
    pub fn note_index(&mut self, obj: u32, mtime: u64, status: u32) {
        self.indexes.insert(obj, (mtime, status));
    }

    /// 记一张固定表（无版本——行是即时产生的）。
    pub fn note_fixed_table(&mut self, name: &str) {
        self.fixed_tables.insert(name.to_owned());
    }

    /// 对象版本集（诊断/缓存键）。
    #[must_use]
    pub fn objects(&self) -> &BTreeMap<u32, u64> {
        &self.objects
    }

    /// 索引版本集（诊断/缓存键）。
    #[must_use]
    pub fn indexes(&self) -> &BTreeMap<u32, (u64, u32)> {
        &self.indexes
    }

    /// 固定表集。
    #[must_use]
    pub fn fixed_tables(&self) -> &BTreeSet<String> {
        &self.fixed_tables
    }

    /// 空否（诊断）。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.objects.is_empty() && self.indexes.is_empty() && self.fixed_tables.is_empty()
    }
}

/// 三格解析的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedName {
    /// 第 ① 格：对象命名空间里的普通对象（`obj# ≥ 100`）。
    Object(CatalogObject),
    /// 只读行源：固定表或经过投影的字典表（没有写入口）。
    FixedTable(&'static str),
}

impl ResolvedName {
    /// 对象形态（固定表 ⇒ `None`）。
    #[must_use]
    pub fn object(&self) -> Option<&CatalogObject> {
        match self {
            ResolvedName::Object(o) => Some(o),
            ResolvedName::FixedTable(_) => None,
        }
    }

    /// 名字。
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            ResolvedName::Object(o) => &o.name,
            ResolvedName::FixedTable(n) => n,
        }
    }
}

/// **名字解析器**（三格 + 版本捕获；一个语句一个实例）。
pub struct NameResolver<'v, V: CatalogView> {
    view: &'v mut V,
    policy: ResolvePolicy,
    refs: BoundRefs,
}

impl<'v, V: CatalogView> NameResolver<'v, V> {
    /// 建解析器（默认属主视角）。
    pub fn new(view: &'v mut V) -> Self {
        Self {
            view,
            policy: ResolvePolicy::default(),
            refs: BoundRefs::default(),
        }
    }

    /// 指定策略（admin 视角等）。
    pub fn with_policy(view: &'v mut V, policy: ResolvePolicy) -> Self {
        Self {
            view,
            policy,
            refs: BoundRefs::default(),
        }
    }

    /// **`SELECT` 表源**：三格顺序查（① 对象 → ② 固定表 → ③ 出局）。
    pub fn resolve_table(&mut self, name: &str) -> Result<ResolvedName, BindError> {
        // Dictionary sources are resolved before ordinary objects, including
        // stat$/seq$ which are not bootstrap tables.
        match self.view.resolve(NameSpace::Table, name) {
            Ok(obj)
                if bicdb_catalog::dict::DICT_TABLES
                    .iter()
                    .any(|table| table.name == name) =>
            {
                if bicdb_catalog::dict::is_public_only(name) && !self.policy.is_admin {
                    return Err(BindError::NotFound {
                        name: name.to_owned(),
                        ns: NameSpace::Table,
                    });
                }
                let table = bicdb_catalog::dict::DICT_TABLES
                    .iter()
                    .find(|table| table.name == name)
                    .expect("matched dictionary");
                self.refs.note_object(obj.obj, obj.mtime);
                return Ok(ResolvedName::FixedTable(table.name));
            }
            Ok(obj) if obj.obj > BOOTSTRAP_MAX => {
                self.refs.note_object(obj.obj, obj.mtime);
                return Ok(ResolvedName::Object(obj));
            }
            Ok(_bootstrap) => { /* ③ 出局：落到固定表/不存在 */ }
            Err(BindError::NotFound { .. }) => {}
            Err(e) => return Err(e),
        }
        // ② 固定表命名空间。
        if let Some(fixed) = FIXED_TABLES.iter().find(|t| **t == name) {
            if *fixed == "attachment$" && (self.view.is_public() || self.policy.is_admin) {
                return Err(BindError::NotFound {
                    name: name.to_owned(),
                    ns: NameSpace::Table,
                });
            }
            // `public` 的管理元数据 `file$`：只有 admin 可见（`public` 的
            // `file$` 对普通用户不可见，本工作区的 `file$` 对其属主可见）。
            if matches!(*fixed, "file$" | "recovery$")
                && self.view.is_public()
                && !self.policy.is_admin
            {
                return Err(BindError::NotFound {
                    name: name.to_owned(),
                    ns: NameSpace::Table,
                });
            }
            if self.view.fixed_columns(name)?.is_some() {
                self.refs.note_fixed_table(name);
                return Ok(ResolvedName::FixedTable(fixed));
            }
        }
        Err(BindError::NotFound {
            name: name.to_owned(),
            ns: NameSpace::Table,
        })
    }

    /// **索引名解析**（只有第 ① 格的索引命名空间）。
    pub fn resolve_index(&mut self, name: &str) -> Result<CatalogObject, BindError> {
        let obj = self.view.resolve(NameSpace::Index, name)?;
        if obj.obj <= BOOTSTRAP_MAX {
            return Err(BindError::NotFound {
                name: name.to_owned(),
                ns: NameSpace::Index,
            });
        }
        self.refs.note_object(obj.obj, obj.mtime);
        Ok(obj)
    }

    /// **写目标解析**（`INSERT/UPDATE/DELETE` 的表；**没有第 ② 格**）。
    ///
    /// 固定表不在这一格 ⇒ "只读 = 没有入口"（写 `file$` 的表现是"不存在"）。
    pub fn resolve_write_target(&mut self, name: &str) -> Result<CatalogObject, BindError> {
        let obj = self.view.resolve(NameSpace::Table, name)?;
        if bicdb_catalog::dict::DICT_TABLES
            .iter()
            .any(|table| table.name == name)
        {
            return Err(BindError::NotWritable(name.to_owned()));
        }
        if obj.obj <= BOOTSTRAP_MAX {
            return Err(BindError::NotFound {
                name: name.to_owned(),
                ns: NameSpace::Table,
            });
        }
        if obj.type_code != bicdb_catalog::dict::obj_kind::TABLE {
            return Err(BindError::NotWritable(name.to_owned()));
        }
        self.refs.note_object(obj.obj, obj.mtime);
        Ok(obj)
    }

    /// 记一个索引的版本（选路后回填缓存键；`Move` 后 `status = 0` 即键失配）。
    pub fn note_index_version(&mut self, obj: u32, mtime: u64, status: u32) {
        self.refs.note_index(obj, mtime, status);
    }

    /// 目录只读面的借用（列/索引枚举等）。
    pub fn view(&mut self) -> &mut V {
        self.view
    }

    /// 收尾：取出捕获到的版本集。
    #[must_use]
    pub fn into_refs(self) -> BoundRefs {
        self.refs
    }

    /// 当前已捕获的版本集（借用）。
    #[must_use]
    pub fn refs(&self) -> &BoundRefs {
        &self.refs
    }
}

/// 自举对象号上界（内部对象只经专用只读行源查询；与 catalog 同源）。
pub const BOOTSTRAP_MAX: u32 = bicdb_catalog::obj_kind::BOOTSTRAP_MAX;

#[cfg(test)]
mod tests;
