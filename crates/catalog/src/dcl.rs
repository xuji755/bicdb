//! **DCL 的字典写侧**（管理面：`public` 上的 `fs$` / `ws$` / `wq$` / `user$`）。
//!
//! 依据：`doc/DCL语句设计_v0.1.md` v0.2（语句面与语义）、
//! `doc/全局控制文件设计_v0.1.md`（位置权威在控制文件、属性在这里）、
//! `doc/用户与配额管理设计_v0.1.md`（口令散列、admin 三条硬规则）。
//!
//! # 分工（本模块只做一半）
//!
//! | 一半 | 在哪 | 谁做 |
//! | --- | --- | --- |
//! | **属性**（名字、状态、配额、口令散列） | **本模块**（`public` 的字典行，DDL 事务） | 引擎 |
//! | **位置**（工作区根目录、池成员路径） | 全局控制文件（`bicdb-storage::globalctl`） | 调用方（CLI/实例层） |
//!
//! 两边靠 `workspace_id` / `fs_slot` 关联；**允许短暂不一致**，修复方向永远是
//! "以控制文件为准重建属性"（`arch/02` §2.11）。
//!
//! # 三条纪律（写侧）
//!
//! 1. **一个动作 = 一个 DDL 事务**（`with_ddl_txn`：写 undo/WAL、提交、缓存失效）；
//! 2. **去重靠唯一索引**，不靠"先查再写"（并发下"查—写"之间有窗口；
//!    真正的闸门是 `i_fs_name`/`i_fs_path`/`i_ws_name`/`i_user_name` 的插入冲突）；
//! 3. **口令永不读出**：`user$` 的读口本模块只给 `passwd` 之外的列
//!    （`UserEntry::hash` 只在认证路径取，**不提供 SQL/内省出口**）。

use bicdb_txn::engine::Engine;

use crate::ddl::{dcl_txn as with_dcl_txn, DdlError};
use crate::dict;
use crate::open::Catalog;
use crate::row::DictValue;

/// DCL 字典层错误。
#[derive(Debug)]
pub enum DclError {
    /// 底层 DDL 事务（写路径、唯一冲突、回滚失败…）。
    Ddl(DdlError),
    /// 找不到目标（按槽位 / 名字 / 工作区号 / 主体名）。
    NotFound {
        /// 找的是什么。
        what: String,
    },
    /// 已经存在（同名/同路径/同号）。
    Duplicate {
        /// 冲突说明。
        what: String,
    },
    /// 值域越界（超过列宽、槽位超出 16 位…）。
    OutOfRange {
        /// 说明。
        what: String,
    },
}

impl std::fmt::Display for DclError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DclError::Ddl(e) => write!(f, "{e}"),
            DclError::NotFound { what } => write!(f, "找不到{what}"),
            DclError::Duplicate { what } => write!(f, "重复：{what}"),
            DclError::OutOfRange { what } => write!(f, "越界：{what}"),
        }
    }
}

impl std::error::Error for DclError {}

impl From<DdlError> for DclError {
    fn from(e: DdlError) -> Self {
        DclError::Ddl(e)
    }
}

// ───────────────────────────── fs$（文件系统池）─────────────────────────────

/// `fs$` 的一行（**属性面**；位置权威在全局控制文件）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsEntry {
    /// 池槽位号（单调分配，永不复用——与全局控制文件同规）。
    pub slot: u16,
    /// 文件系统名（标识；实例内唯一）。
    pub name: String,
    /// 路径（可以是挂载点，也可以只是一个目录）。
    pub path: String,
    /// 状态（0 = 空槽位 / 1 = 在池 / 2 = 已移出——与 `globalctl` 的取值同源）。
    pub status: u32,
    /// 登记时的大小（字节；0 = 未知）。
    pub total_bytes: u64,
    /// 空闲字节（缓存值；权威是文件系统本身）。
    pub free_bytes: Option<u64>,
    /// 分配开关（`ALTER FILESYSTEM … SET ALLOCATE = ON|OFF`）。
    pub allocate: bool,
}

/// `fs$` 的状态取值（与 `bicdb_storage::globalctl` 同源）。
pub mod fs_status {
    /// 在池。
    pub const IN_POOL: u32 = 1;
    /// 已移出（墓碑；行不删——审计友好）。
    pub const REMOVED: u32 = 2;
}

/// 数值列宽度（`Number` = `NUMBER(38,0)`；槽位与字节数都在 `u64` 域内）。
fn num(v: u64) -> DictValue {
    DictValue::Num(v)
}

/// **登记一个文件系统**（`fs$` 插一行）。返回提交序号。
///
/// **判重**：名字与路径各由唯一索引把关（`i_fs_name` / `i_fs_path`）；
/// 冲突以 `DclError::Ddl`（唯一冲突）报出——**并发下也正确**。
pub fn insert_fs(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    e: &FsEntry,
) -> Result<u64, DclError> {
    let values = vec![
        num(u64::from(e.slot)),
        DictValue::Text(e.name.clone()),
        DictValue::Text(e.path.clone()),
        num(u64::from(e.status)),
        num(e.total_bytes),
        e.free_bytes.map_or(DictValue::Null, num),
        num(u64::from(e.allocate)),
    ];
    let (_, seq) = with_dcl_txn(cat, engine, |w| {
        w.insert_dict_row("fs$", &values)?;
        Ok(())
    })?;
    cat.row_cache().bump_and_clear();
    Ok(seq)
}

/// **改一行的非键列**（`fs$` 的 status/allocate/容量缓存）。
///
/// 键列（`fs_slot`）不得动——`DictWriter::update_row` 会当场拒绝。
pub fn update_fs_row(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    slot: u16,
    f: impl FnOnce(&mut FsEntry),
) -> Result<u64, DclError> {
    let (rid, mut entry) = find_fs_rid(cat, slot)?;
    f(&mut entry);
    let values = vec![
        num(u64::from(entry.slot)),
        DictValue::Text(entry.name.clone()),
        DictValue::Text(entry.path.clone()),
        num(u64::from(entry.status)),
        num(entry.total_bytes),
        entry.free_bytes.map_or(DictValue::Null, num),
        num(u64::from(entry.allocate)),
    ];
    let (_, seq) = with_dcl_txn(cat, engine, |w| {
        w.update_dict_row("fs$", rid, &values)?;
        Ok(())
    })?;
    cat.row_cache().bump_and_clear();
    Ok(seq)
}

/// **列出全部池成员**（含已移出——审计视角；按槽位序）。
pub fn list_fs(
    cat: &mut Catalog<'_>,
) -> Result<Vec<(bicdb_storage::rowid::RowId, FsEntry)>, DclError> {
    let rows = cat
        .scan("fs$")
        .map_err(|e| DclError::Ddl(DdlError::BadTableDef(e.to_string())))?;
    let mut out = Vec::with_capacity(rows.len());
    for (rid, values) in rows {
        out.push((rid, fs_entry_of(&values)?));
    }
    out.sort_by_key(|(_, e)| e.slot);
    Ok(out)
}

/// 按槽位取（含已移出）；找不到 ⇒ `None`。
pub fn fs_by_slot(cat: &mut Catalog<'_>, slot: u16) -> Result<Option<FsEntry>, DclError> {
    Ok(list_fs(cat)?
        .into_iter()
        .map(|(_, e)| e)
        .find(|e| e.slot == slot))
}

/// 按名字取（含已移出）。
pub fn fs_by_name(cat: &mut Catalog<'_>, name: &str) -> Result<Option<FsEntry>, DclError> {
    Ok(list_fs(cat)?
        .into_iter()
        .map(|(_, e)| e)
        .find(|e| e.name == name))
}

fn find_fs_rid(
    cat: &mut Catalog<'_>,
    slot: u16,
) -> Result<(bicdb_storage::rowid::RowId, FsEntry), DclError> {
    list_fs(cat)?
        .into_iter()
        .find(|(_, e)| e.slot == slot)
        .ok_or_else(|| DclError::NotFound {
            what: format!("池槽位 {slot}"),
        })
}

fn fs_entry_of(values: &[DictValue]) -> Result<FsEntry, DclError> {
    let n = |i: usize| -> Result<u64, DclError> {
        match values.get(i) {
            Some(DictValue::Num(v)) => Ok(*v),
            other => Err(DclError::OutOfRange {
                what: format!("fs$ 第 {} 列不是数值：{other:?}", i + 1),
            }),
        }
    };
    let text = |i: usize| -> Result<String, DclError> {
        match values.get(i) {
            Some(DictValue::Text(t)) => Ok(t.clone()),
            other => Err(DclError::OutOfRange {
                what: format!("fs$ 第 {} 列不是文本：{other:?}", i + 1),
            }),
        }
    };
    let slot = n(0)?;
    let slot16 = u16::try_from(slot).map_err(|_| DclError::OutOfRange {
        what: format!("fs$ 槽位超出 16 位：{slot}"),
    })?;
    Ok(FsEntry {
        slot: slot16,
        name: text(1)?,
        path: text(2)?,
        status: n(3)? as u32,
        total_bytes: n(4)?,
        free_bytes: match values.get(5) {
            Some(DictValue::Null) | None => None,
            Some(DictValue::Num(v)) => Some(*v),
            other => {
                return Err(DclError::OutOfRange {
                    what: format!("fs$.free_bytes 形态非法：{other:?}"),
                })
            }
        },
        allocate: n(6)? != 0,
    })
}

// ───────────────────────────── ws$（工作区登记）─────────────────────────────

/// `ws$` 的一行（**属性面**；根目录在全局控制文件里）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WsEntry {
    /// 工作区号（48 位，实例内唯一）。
    pub workspace_id: u64,
    /// 属主（`None` = **无主容器**，等待 `CREATE USER … USING WORKSPACE` 绑定）。
    pub user_id: Option<u64>,
    /// 工作区名（实例内唯一）。
    pub name: String,
    /// 状态（`ws_status`）。
    pub status: u32,
    /// 建区时刻（墙钟毫秒；`Timestamp` 列按标量存）。
    pub ctime_ms: u64,
    /// 角色级配额（字节）：data / undo / temp / asset。
    pub quota: [u64; 4],
    /// **默认文件系统**（`fs$` 的槽位号；`None` = 实例默认盘）。W4 的落点。
    pub default_fs: Option<u16>,
}

/// `ws$` 的状态取值。
pub mod ws_status {
    /// 在册（正常）。
    pub const ACTIVE: u32 = 1;
    /// 模板（`ALTER WORKSPACE … TO TEMPLATE`；不可作为普通区打开）。
    pub const TEMPLATE: u32 = 2;
    /// 已删（墓碑；行保留供审计）。
    pub const DROPPED: u32 = 3;
}

/// **登记一个工作区**（`ws$` 插一行，`user_id = NULL` = 无主）。
pub fn insert_ws(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    w: &WsEntry,
) -> Result<u64, DclError> {
    let values = ws_values(w);
    let (_, seq) = with_dcl_txn(cat, engine, |wr| {
        wr.insert_dict_row("ws$", &values)?;
        Ok(())
    })?;
    cat.row_cache().bump_and_clear();
    Ok(seq)
}

/// **改一行工作区登记**。
pub fn update_ws_row(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    workspace_id: u64,
    f: impl FnOnce(&mut WsEntry),
) -> Result<u64, DclError> {
    let (rid, mut entry) = find_ws_rid(cat, workspace_id)?;
    f(&mut entry);
    let values = ws_values(&entry);
    let (_, seq) = with_dcl_txn(cat, engine, |w| {
        w.update_dict_row("ws$", rid, &values)?;
        Ok(())
    })?;
    cat.row_cache().bump_and_clear();
    Ok(seq)
}

/// **改工作区名**（W5）。
///
/// **名字是键列**（`i_ws_name`）⇒ 不能就地改（`DictWriter::update_row` 会拒绝）：
/// **同一事务里删旧行、插新行**——原子性由事务给，索引两项同步维护。
pub fn rename_ws(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    workspace_id: u64,
    new_name: &str,
) -> Result<u64, DclError> {
    let (rid, mut entry) = find_ws_rid(cat, workspace_id)?;
    entry.name = new_name.to_owned();
    let values = ws_values(&entry);
    let (_, seq) = with_dcl_txn(cat, engine, |w| {
        w.delete_dict_row("ws$", rid)?;
        w.insert_dict_row("ws$", &values)?;
        Ok(())
    })?;
    cat.row_cache().bump_and_clear();
    Ok(seq)
}

/// 列出全部工作区登记（含墓碑）。
pub fn list_ws(
    cat: &mut Catalog<'_>,
) -> Result<Vec<(bicdb_storage::rowid::RowId, WsEntry)>, DclError> {
    let rows = cat
        .scan("ws$")
        .map_err(|e| DclError::Ddl(DdlError::BadTableDef(e.to_string())))?;
    let mut out = Vec::with_capacity(rows.len());
    for (rid, values) in rows {
        out.push((rid, ws_entry_of(&values)?));
    }
    out.sort_by_key(|(_, e)| e.workspace_id);
    Ok(out)
}

/// 按工作区号取。
pub fn ws_by_id(cat: &mut Catalog<'_>, id: u64) -> Result<Option<WsEntry>, DclError> {
    Ok(list_ws(cat)?
        .into_iter()
        .map(|(_, e)| e)
        .find(|e| e.workspace_id == id))
}

/// 按名字取（**实例内唯一**——v0.2：不是"属主内唯一"）。
pub fn ws_by_name(cat: &mut Catalog<'_>, name: &str) -> Result<Option<WsEntry>, DclError> {
    Ok(list_ws(cat)?
        .into_iter()
        .map(|(_, e)| e)
        .find(|e| e.name == name))
}

fn find_ws_rid(
    cat: &mut Catalog<'_>,
    id: u64,
) -> Result<(bicdb_storage::rowid::RowId, WsEntry), DclError> {
    list_ws(cat)?
        .into_iter()
        .find(|(_, e)| e.workspace_id == id)
        .ok_or_else(|| DclError::NotFound {
            what: format!("工作区 {id}"),
        })
}

fn ws_values(w: &WsEntry) -> Vec<DictValue> {
    vec![
        num(w.workspace_id),
        w.user_id.map_or(DictValue::Null, num),
        DictValue::Text(w.name.clone()),
        num(u64::from(w.status)),
        num(w.ctime_ms),
        num(w.quota[0]),
        num(w.quota[1]),
        num(w.quota[2]),
        num(w.quota[3]),
        w.default_fs.map_or(DictValue::Null, |s| num(u64::from(s))),
    ]
}

fn ws_entry_of(values: &[DictValue]) -> Result<WsEntry, DclError> {
    let n = |i: usize| -> Result<u64, DclError> {
        match values.get(i) {
            Some(DictValue::Num(v)) => Ok(*v),
            other => Err(DclError::OutOfRange {
                what: format!("ws$ 第 {} 列不是数值：{other:?}", i + 1),
            }),
        }
    };
    let text = |i: usize| -> Result<String, DclError> {
        match values.get(i) {
            Some(DictValue::Text(t)) => Ok(t.clone()),
            other => Err(DclError::OutOfRange {
                what: format!("ws$ 第 {} 列不是文本：{other:?}", i + 1),
            }),
        }
    };
    Ok(WsEntry {
        workspace_id: n(0)?,
        user_id: match values.get(1) {
            Some(DictValue::Null) | None => None,
            Some(DictValue::Num(v)) => Some(*v),
            other => {
                return Err(DclError::OutOfRange {
                    what: format!("ws$.user_id 形态非法：{other:?}"),
                })
            }
        },
        name: text(2)?,
        status: n(3)? as u32,
        ctime_ms: n(4)?,
        quota: [n(5)?, n(6)?, n(7)?, n(8)?],
        default_fs: match values.get(9) {
            Some(DictValue::Null) | None => None,
            Some(DictValue::Num(v)) => {
                Some(u16::try_from(*v).map_err(|_| DclError::OutOfRange {
                    what: format!("ws$.default_fs 超出 16 位：{v}"),
                })?)
            }
            other => {
                return Err(DclError::OutOfRange {
                    what: format!("ws$.default_fs 形态非法：{other:?}"),
                })
            }
        },
    })
}

// ───────────────────────────── wq$（盘级配额）─────────────────────────────

/// `wq$` 的一行：**某个工作区在某块盘上的上限**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WqEntry {
    /// 工作区号。
    pub workspace_id: u64,
    /// 池槽位（`fs$` 的槽位号）。
    pub fs_slot: u16,
    /// 上限（字节）。
    pub quota_bytes: u64,
}

/// **写/改一条盘级配额**（有则改、无则插——键 = `(工作区, 槽位)`）。
pub fn put_wq(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    e: &WqEntry,
) -> Result<u64, DclError> {
    let existing = list_wq(cat)?
        .into_iter()
        .find(|(_, r)| r.workspace_id == e.workspace_id && r.fs_slot == e.fs_slot);
    let values = vec![
        num(e.workspace_id),
        num(u64::from(e.fs_slot)),
        num(e.quota_bytes),
    ];
    let (_, seq) = match existing {
        Some((rid, _)) => with_dcl_txn(cat, engine, |w| {
            w.update_dict_row("wq$", rid, &values)?;
            Ok(())
        })?,
        None => with_dcl_txn(cat, engine, |w| {
            w.insert_dict_row("wq$", &values)?;
            Ok(())
        })?,
    };
    cat.row_cache().bump_and_clear();
    Ok(seq)
}

/// 列出全部盘级配额。
pub fn list_wq(
    cat: &mut Catalog<'_>,
) -> Result<Vec<(bicdb_storage::rowid::RowId, WqEntry)>, DclError> {
    let rows = cat
        .scan("wq$")
        .map_err(|e| DclError::Ddl(DdlError::BadTableDef(e.to_string())))?;
    let mut out = Vec::with_capacity(rows.len());
    for (rid, values) in rows {
        let n = |i: usize| -> Result<u64, DclError> {
            match values.get(i) {
                Some(DictValue::Num(v)) => Ok(*v),
                other => Err(DclError::OutOfRange {
                    what: format!("wq$ 第 {} 列不是数值：{other:?}", i + 1),
                }),
            }
        };
        let slot = u16::try_from(n(1)?).map_err(|_| DclError::OutOfRange {
            what: "wq$ 槽位超出 16 位".to_owned(),
        })?;
        out.push((
            rid,
            WqEntry {
                workspace_id: n(0)?,
                fs_slot: slot,
                quota_bytes: n(2)?,
            },
        ));
    }
    Ok(out)
}

// ───────────────────────────── user$（主体）─────────────────────────────

/// `user$` 的一行。
///
/// **口令散列不在其中**：它只在认证路径按 `user_id` 单独取
/// （[`password_hash`]）——"管理面只给元数据，口令永不读出"（admin 硬规则 B）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserEntry {
    /// 主体号（48 位，实例内唯一）。
    pub user_id: u64,
    /// 主体名（实例内唯一）。
    pub name: String,
    /// 状态（`user_status`）。
    pub status: u32,
    /// 建主体时刻（墙钟毫秒）。
    pub ctime_ms: u64,
}

/// `user$` 的状态取值。
pub mod user_status {
    /// 正常（可开会话）。
    pub const ACTIVE: u32 = 1;
    /// 暂停（`ALTER USER … PAUSE`：**拒绝新会话**，已开会话不掐断）。
    pub const PAUSED: u32 = 2;
    /// 口令已过期（`… EXPIRE`：下次登录必须改密——随 D6 认证强制）。
    pub const EXPIRED: u32 = 3;
}

/// **建一个主体**（`user$` 插一行；`passwd` = **调用方算好的散列**）。
///
/// 口令散列的算法与参数在**认证切片**（PBKDF2-SHA512）；本层只存不解释。
pub fn insert_user(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    u: &UserEntry,
    passwd_hash: &str,
) -> Result<u64, DclError> {
    let values = vec![
        num(u.user_id),
        DictValue::Text(u.name.clone()),
        DictValue::Text(passwd_hash.to_owned()),
        num(u64::from(u.status)),
        num(u.ctime_ms),
    ];
    let (_, seq) = with_dcl_txn(cat, engine, |w| {
        w.insert_dict_row("user$", &values)?;
        Ok(())
    })?;
    cat.row_cache().bump_and_clear();
    Ok(seq)
}

/// **改一个主体的状态**（PAUSE/RESUME/EXPIRE）。
pub fn set_user_status(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    user_id: u64,
    status: u32,
) -> Result<u64, DclError> {
    let (rid, u) = find_user_rid(cat, user_id)?;
    let values = vec![
        num(u.user_id),
        DictValue::Text(u.name.clone()),
        // 状态行改写**不碰口令列**：取旧散列原样写回（读它是为了不改它——
        // 这一处是"写回"不是"读出"，散列不出本函数）。
        DictValue::Text(current_hash(cat, rid)?),
        num(u64::from(status)),
        num(u.ctime_ms),
    ];
    let (_, seq) = with_dcl_txn(cat, engine, |w| {
        w.update_dict_row("user$", rid, &values)?;
        Ok(())
    })?;
    cat.row_cache().bump_and_clear();
    Ok(seq)
}

/// **重置口令**（`ALTER USER … IDENTIFIED BY …`：admin 的初始化/找回）。
///
/// 旧散列被**覆盖**——**读不回**（PBKDF2 不可逆），这正是设计要的语义。
pub fn set_user_password(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    user_id: u64,
    new_hash: &str,
) -> Result<u64, DclError> {
    let (rid, u) = find_user_rid(cat, user_id)?;
    let values = vec![
        num(u.user_id),
        DictValue::Text(u.name.clone()),
        DictValue::Text(new_hash.to_owned()),
        num(u64::from(u.status)),
        num(u.ctime_ms),
    ];
    let (_, seq) = with_dcl_txn(cat, engine, |w| {
        w.update_dict_row("user$", rid, &values)?;
        Ok(())
    })?;
    cat.row_cache().bump_and_clear();
    Ok(seq)
}

/// **取口令散列**——**只在认证路径用**（REQ-ISO-002 的落点，随 D6）。
///
/// 这是本模块**唯一**能读到 `passwd` 的口；**不进 SQL 面、不进内省**
/// （admin 硬规则 B：口令散列连 admin 也读不到）。
pub fn password_hash(cat: &mut Catalog<'_>, user_id: u64) -> Result<String, DclError> {
    let (rid, _) = find_user_rid(cat, user_id)?;
    current_hash(cat, rid)
}

/// 列出全部主体（**不含口令**）。
pub fn list_users(cat: &mut Catalog<'_>) -> Result<Vec<UserEntry>, DclError> {
    let rows = cat
        .scan("user$")
        .map_err(|e| DclError::Ddl(DdlError::BadTableDef(e.to_string())))?;
    let mut out = Vec::with_capacity(rows.len());
    for (_, values) in rows {
        out.push(user_entry_of(&values)?);
    }
    out.sort_by_key(|u| u.user_id);
    Ok(out)
}

/// 按主体名取。
pub fn user_by_name(cat: &mut Catalog<'_>, name: &str) -> Result<Option<UserEntry>, DclError> {
    Ok(list_users(cat)?.into_iter().find(|u| u.name == name))
}

/// 按主体号取。
pub fn user_by_id(cat: &mut Catalog<'_>, id: u64) -> Result<Option<UserEntry>, DclError> {
    Ok(list_users(cat)?.into_iter().find(|u| u.user_id == id))
}

/// **分配一个主体号** = 已用最大值 + 1（**从 1 起、不复用**）。
///
/// 与工作区号同规：扫出来的事实，不是计数器（崩溃窗口里永远自洽）。
pub fn allocate_user_id(cat: &mut Catalog<'_>) -> Result<u64, DclError> {
    let mut max = 0u64;
    for u in list_users(cat)? {
        max = max.max(u.user_id);
    }
    Ok(max + 1)
}

/// **某主体名下的工作区**（按工作区号序；含无主的？不含——只看 `user_id = Some(id)`）。
pub fn workspaces_of_user(cat: &mut Catalog<'_>, user_id: u64) -> Result<Vec<WsEntry>, DclError> {
    Ok(list_ws(cat)?
        .into_iter()
        .map(|(_, w)| w)
        .filter(|w| w.user_id == Some(user_id) && w.status != ws_status::DROPPED)
        .collect())
}

/// **删一个主体**（`DROP USER`；行**真删**——主体名要能被再次使用）。
pub fn delete_user(
    cat: &mut Catalog<'_>,
    engine: &Engine<'_, '_, '_, '_>,
    user_id: u64,
) -> Result<u64, DclError> {
    let (rid, _) = find_user_rid(cat, user_id)?;
    let (_, seq) = with_dcl_txn(cat, engine, |w| {
        w.delete_dict_row("user$", rid)?;
        Ok(())
    })?;
    cat.row_cache().bump_and_clear();
    Ok(seq)
}

fn find_user_rid(
    cat: &mut Catalog<'_>,
    id: u64,
) -> Result<(bicdb_storage::rowid::RowId, UserEntry), DclError> {
    let rows = cat
        .scan("user$")
        .map_err(|e| DclError::Ddl(DdlError::BadTableDef(e.to_string())))?;
    for (rid, values) in rows {
        if let Some(DictValue::Num(v)) = values.first() {
            if *v == id {
                return Ok((rid, user_entry_of(&values)?));
            }
        }
    }
    Err(DclError::NotFound {
        what: format!("主体 {id}"),
    })
}

fn current_hash(
    cat: &mut Catalog<'_>,
    rid: bicdb_storage::rowid::RowId,
) -> Result<String, DclError> {
    // 行级读：`fetch` 按 ROWID 取整行（`user$` 的第 3 列 = passwd）。
    let values = cat
        .fetch("user$", rid)
        .map_err(|e| DclError::Ddl(DdlError::BadTableDef(e.to_string())))?;
    match values.get(2) {
        Some(DictValue::Text(t)) => Ok(t.clone()),
        other => Err(DclError::OutOfRange {
            what: format!("user$.passwd 形态非法：{other:?}"),
        }),
    }
}

fn user_entry_of(values: &[DictValue]) -> Result<UserEntry, DclError> {
    let n = |i: usize| -> Result<u64, DclError> {
        match values.get(i) {
            Some(DictValue::Num(v)) => Ok(*v),
            other => Err(DclError::OutOfRange {
                what: format!("user$ 第 {} 列不是数值：{other:?}", i + 1),
            }),
        }
    };
    let text = |i: usize| -> Result<String, DclError> {
        match values.get(i) {
            Some(DictValue::Text(t)) => Ok(t.clone()),
            other => Err(DclError::OutOfRange {
                what: format!("user$ 第 {} 列不是文本：{other:?}", i + 1),
            }),
        }
    };
    Ok(UserEntry {
        user_id: n(0)?,
        name: text(1)?,
        status: n(3)? as u32,
        ctime_ms: n(4)?,
    })
}

/// 字典表名清单（诊断用；确认 DCL 只碰这几张）。
#[must_use]
pub fn management_tables() -> Vec<&'static str> {
    dict::DICT_TABLES
        .iter()
        .filter(|t| dict::is_public_only(t.name))
        .map(|t| t.name)
        .collect()
}

#[cfg(test)]
mod tests {
    use bicdb_workspace::io::MemFileIo;

    use super::*;
    use crate::ddl::tests::{open_catalog, rig_with};

    fn public_rig(tag: &str) -> (&'static MemFileIo, crate::ddl::tests::Rig) {
        let io: &'static MemFileIo = Box::leak(Box::new(MemFileIo::new()));
        io.add_dir("/mem");
        let rig = rig_with(io, tag, true);
        (io, rig)
    }

    fn fs_entry(slot: u16, name: &str, path: &str) -> FsEntry {
        FsEntry {
            slot,
            name: name.to_owned(),
            path: path.to_owned(),
            status: fs_status::IN_POOL,
            total_bytes: 0,
            free_bytes: None,
            allocate: true,
        }
    }

    #[test]
    fn fs_rows_insert_find_update_and_survive_reopen() {
        let (io, rig) = public_rig("dcl_fs");
        let mut cat = open_catalog(io, &rig);

        // 插两行：槽位与名字/路径都对得上。
        insert_fs(&mut cat, rig.engine, &fs_entry(1, "data1", "/srv/data1")).unwrap();
        insert_fs(&mut cat, rig.engine, &fs_entry(2, "data2", "/srv/data2")).unwrap();
        let all = list_fs(&mut cat).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].1.name, "data1");
        assert_eq!(all[1].1.slot, 2);
        assert!(all[0].1.allocate, "默认 ON");

        // 唯一索引把关：同名、同路径都拒绝（`DclError::Ddl`）。
        assert!(insert_fs(&mut cat, rig.engine, &fs_entry(3, "data1", "/srv/other")).is_err());
        assert!(insert_fs(&mut cat, rig.engine, &fs_entry(3, "data3", "/srv/data1")).is_err());

        // 改非键列：ALLOCATE = OFF（排水阀）。
        update_fs_row(&mut cat, rig.engine, 1, |e| {
            e.allocate = false;
            e.total_bytes = 1024;
            e.free_bytes = Some(512);
        })
        .unwrap();
        let e = fs_by_slot(&mut cat, 1).unwrap().expect("在池");
        assert!(!e.allocate);
        assert_eq!(e.total_bytes, 1024);
        assert_eq!(e.free_bytes, Some(512));
        assert_eq!(e.name, "data1", "改开关不动名字");

        // 移出（墓碑；行不删）。
        update_fs_row(&mut cat, rig.engine, 2, |e| e.status = fs_status::REMOVED).unwrap();
        assert_eq!(list_fs(&mut cat).unwrap().len(), 2, "墓碑保留");
        let in_pool: Vec<_> = list_fs(&mut cat)
            .unwrap()
            .into_iter()
            .filter(|(_, e)| e.status == fs_status::IN_POOL)
            .collect();
        assert_eq!(in_pool.len(), 1);

        // 关 → 开：行还在（DDL 事务真落盘）。
        drop(cat);
        let mut cat = open_catalog(io, &rig);
        assert_eq!(fs_by_name(&mut cat, "data1").unwrap().unwrap().slot, 1);
    }

    #[test]
    fn ws_rows_carry_owner_and_two_level_quota() {
        let (io, rig) = public_rig("dcl_ws");
        let mut cat = open_catalog(io, &rig);

        // **无主容器**（user_id = NULL）——依赖顺序的落点。
        let w = WsEntry {
            workspace_id: 7,
            user_id: None,
            name: "alice_ws".to_owned(),
            status: ws_status::ACTIVE,
            ctime_ms: 42,
            quota: [1, 2, 3, 4],
            default_fs: Some(1),
        };
        insert_ws(&mut cat, rig.engine, &w).unwrap();
        let got = ws_by_name(&mut cat, "alice_ws").unwrap().expect("在册");
        assert_eq!(got.user_id, None, "建区时无主");
        assert_eq!(got.quota, [1, 2, 3, 4]);
        assert_eq!(got.default_fs, Some(1), "W4 的默认盘落进 ws$");

        // 名字**实例内唯一**（v0.2）：同名再插 ⇒ 唯一冲突。
        let dup = WsEntry {
            workspace_id: 8,
            ..w.clone()
        };
        assert!(insert_ws(&mut cat, rig.engine, &dup).is_err());

        // 绑定属主（CREATE USER … USING WORKSPACE 的写侧）。
        update_ws_row(&mut cat, rig.engine, 7, |e| e.user_id = Some(100)).unwrap();
        assert_eq!(ws_by_id(&mut cat, 7).unwrap().unwrap().user_id, Some(100));

        // 盘级配额（wq$）：有则改、无则插。
        put_wq(
            &mut cat,
            rig.engine,
            &WqEntry {
                workspace_id: 7,
                fs_slot: 1,
                quota_bytes: 1024,
            },
        )
        .unwrap();
        put_wq(
            &mut cat,
            rig.engine,
            &WqEntry {
                workspace_id: 7,
                fs_slot: 1,
                quota_bytes: 2048,
            },
        )
        .unwrap();
        assert_eq!(list_wq(&mut cat).unwrap().len(), 1, "同键改而非插");
        assert_eq!(list_wq(&mut cat).unwrap()[0].1.quota_bytes, 2048);
    }

    #[test]
    fn users_keep_the_hash_out_of_the_metadata_view() {
        let (io, rig) = public_rig("dcl_users");
        let mut cat = open_catalog(io, &rig);

        let u = UserEntry {
            user_id: 100,
            name: "alice".to_owned(),
            status: user_status::ACTIVE,
            ctime_ms: 7,
        };
        insert_user(&mut cat, rig.engine, &u, "pbkdf2$fake-hash").unwrap();
        // 元数据面：有名字/状态，**没有散列**（结构上就没有这个字段）。
        let listed = list_users(&mut cat).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "alice");

        // 认证路径才取散列。
        assert_eq!(password_hash(&mut cat, 100).unwrap(), "pbkdf2$fake-hash");

        // 暂停 → 恢复 → 重置口令（旧散列被覆盖，读不回）。
        set_user_status(&mut cat, rig.engine, 100, user_status::PAUSED).unwrap();
        assert_eq!(
            user_by_name(&mut cat, "alice").unwrap().unwrap().status,
            user_status::PAUSED
        );
        set_user_status(&mut cat, rig.engine, 100, user_status::ACTIVE).unwrap();
        set_user_password(&mut cat, rig.engine, 100, "pbkdf2$new").unwrap();
        assert_eq!(password_hash(&mut cat, 100).unwrap(), "pbkdf2$new");

        // 删主体：名字可再被使用。
        delete_user(&mut cat, rig.engine, 100).unwrap();
        assert!(user_by_name(&mut cat, "alice").unwrap().is_none());
        insert_user(&mut cat, rig.engine, &u, "pbkdf2$again").unwrap();
        assert_eq!(list_users(&mut cat).unwrap().len(), 1);
    }

    #[test]
    fn management_tables_are_exactly_the_public_four() {
        assert_eq!(management_tables(), vec!["user$", "ws$", "fs$", "wq$"]);
    }
}
