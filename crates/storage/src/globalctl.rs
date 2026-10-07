//! **全局控制文件**：实例级注册表（`doc/全局控制文件设计_v0.1.md`、`arch/02` §2.6）。
//!
//! # 它与工作区控制文件的关系
//!
//! **同一套物理格式与更新协议**（[`crate::controlfile::CfCore`]：20 页 × 16 KiB、
//! 双副本、单区间 undo 发布、崩溃自愈），只是**解释不同**——页头 `flags` 字节带
//! **种类**（[`crate::controlfile::CF_KIND_GLOBAL`]），把两种文件搞混会在打开时当场失败。
//!
//! ```text
//! 页 0        文件头（段偏移表 8 项 + 定长 undo 记录）
//! 页 1        固定段：库条目 64B │ 计数 48B
//! 页 2–11     工作区记录数组：580 × 280B（**按页对齐**——每页 58 条，不跨页）
//! 页 12–19    文件系统池记录数组：384 × 336B（每页 48 条）
//! ```
//!
//! # 三条语义（照 §2.6/§2.11/§2.12）
//!
//! 1. **"工作区是否存在"的唯一判据 = 它在这份文件里有没有一条 `在册` 记录**；
//! 2. **绝不在提交关键路径上**——只在建/删工作区、登记/移出文件系统时写；
//! 3. **不是恢复锚点**——丢了可由各工作区自描述（工作区控制文件里的完整路径）重建，
//!    所以它的写入顺序永远是"**文件先、控制文件最后**"。
//!
//! # 与 `public.fs$` / `public.ws$` 的分工
//!
//! | 信息 | 权威 | 性质 |
//! | --- | --- | --- |
//! | **在哪**（工作区根目录、池成员路径） | **本文件** | 启动入口；**不依赖任何工作区** |
//! | **是什么**（属主、配额、状态、用量） | `public` 的 `ws$`/`fs$` | 管理属性；可查询、可事务 |
//!
//! 两处都含 `workspace_id`/`fs_slot`，靠它关联；**允许短暂不一致**（一个是文件层、
//! 一个是管理层），修复方向永远是"以本文件为准重建管理层"。

use std::io;
use std::path::Path;

use bicdb_workspace::id::WorkspaceId;
use bicdb_workspace::io::FileIo;

use crate::controlfile::{
    blank_page, file_offset, get_u16, get_u32, get_u48, get_u64, page_body_mut, put_u16, put_u32,
    put_u48, put_u64, CfCore, ControlFileError, CF_COPIES, CF_KIND_GLOBAL, CF_PAGES,
    CF_PAGE_BODY_LEN, CF_PAGE_SIZE_FIELD, MAX_UPDATE_LEN, SEGMENT_ITEM_LEN, SEGMENT_TABLE_ITEMS,
    SEGMENT_TABLE_OFFSET, UNDO_RECORD_LEN,
};

// 记录必须落在单次更新区间上限内（编译期钉住——协议按 ≤512B 设计）。
const _: () = assert!(WORKSPACE_RECORD_LEN <= MAX_UPDATE_LEN);
const _: () = assert!(FS_RECORD_LEN <= MAX_UPDATE_LEN);
const _: () = assert!(COUNTERS_LEN <= MAX_UPDATE_LEN);

// ---------------------------------------------------------------------------
// 字节布局（唯一定义处；测试逐项钉住）
// ---------------------------------------------------------------------------

/// 格式版本（与工作区控制文件**各自独立**演进；本文件第一版 = 1）。
pub const GCF_FORMAT_VERSION: u16 = 1;

/// 库条目长度（页 1 偏移 0）。
pub const LIBRARY_ENTRY_LEN: usize = 64;
/// 库标识长度（实例标识；建实例时随机生成，跨备份可识别）。
pub const LIBRARY_ID_LEN: usize = 16;

/// 计数段长度（页 1 偏移 64）。
pub const COUNTERS_LEN: usize = 48;
/// 计数段偏移。
pub const COUNTERS_OFFSET: usize = LIBRARY_ENTRY_LEN;
/// 固定段总长。
pub const GCF_FIXED_SEGMENT_LEN: usize = COUNTERS_OFFSET + COUNTERS_LEN;

/// 工作区记录长度。
pub const WORKSPACE_RECORD_LEN: usize = 280;
/// 工作区记录起始页。
pub const WORKSPACE_PAGES_FIRST: usize = 2;
/// 工作区记录页数（页 2–11）。
pub const WORKSPACE_PAGES: usize = 10;
/// 工作区记录起始页之后的第一页（= 池记录区起点）。
pub const FS_PAGES_FIRST: usize = WORKSPACE_PAGES_FIRST + WORKSPACE_PAGES;
/// 每页容纳的工作区记录数（记录**不跨页**；尾部留空）。
pub const WORKSPACE_RECORDS_PER_PAGE: usize = CF_PAGE_BODY_LEN / WORKSPACE_RECORD_LEN;
/// 工作区记录容量。
pub const MAX_WORKSPACE_RECORDS: usize = WORKSPACE_RECORDS_PER_PAGE * WORKSPACE_PAGES;
/// 工作区根路径字段长度（绝对路径）。
pub const WORKSPACE_PATH_LEN: usize = 256;

/// 文件系统池记录长度。
pub const FS_RECORD_LEN: usize = 336;
/// 池记录页数（页 12–19）。
pub const FS_PAGES: usize = CF_PAGES - FS_PAGES_FIRST;
/// 每页容纳的池记录数。
pub const FS_RECORDS_PER_PAGE: usize = CF_PAGE_BODY_LEN / FS_RECORD_LEN;
/// 池记录容量。
pub const MAX_FS_RECORDS: usize = FS_RECORDS_PER_PAGE * FS_PAGES;
/// 池成员**名字**字段长度（标识符；创建时给、之后引用只用它）。
pub const FS_NAME_LEN: usize = 64;
/// 池成员**路径**字段长度。
pub const FS_PATH_LEN: usize = 256;

/// 记录状态：空槽位。
pub const REC_EMPTY: u8 = 0;
/// 工作区记录状态：**在册**（"存在"的唯一判据）。
pub const WS_ACTIVE: u8 = 1;
/// 工作区记录状态：**已删**（墓碑——保留供审计，**不算存在**；槽位不复用）。
pub const WS_DROPPED: u8 = 2;
/// 池记录状态：**在池**。
pub const FS_IN_POOL: u8 = 1;
/// 池记录状态：**已移出**（墓碑；槽位不复用 ⇒ `fs_slot` 永远指同一块盘的历史）。
pub const FS_REMOVED: u8 = 2;

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

/// 全局控制文件错误。
#[derive(Debug)]
pub enum GlobalCtlError {
    /// 底层（页/副本/协议）。
    Core(ControlFileError),
    /// 本文件自己的判定（字段越界、容量满、找不到）。
    Invalid {
        /// 说明。
        what: String,
    },
    /// 没有空槽位（容量上限）。
    Full {
        /// 哪一类满了。
        what: &'static str,
    },
    /// 找不到目标（按 id / 槽位 / 名字 / 路径）。
    NotFound {
        /// 找的是什么。
        what: String,
    },
    /// 已经存在（同 id 在册 / 同名在池 / 同路径在池）。
    Duplicate {
        /// 冲突说明。
        what: String,
    },
    /// 长度越界（路径/名字不适合字段宽度）。
    TooLong {
        /// 字段名。
        field: &'static str,
        /// 实际长度。
        len: usize,
        /// 字段宽度。
        max: usize,
    },
}

impl std::fmt::Display for GlobalCtlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GlobalCtlError::Core(e) => write!(f, "{e}"),
            GlobalCtlError::Invalid { what } => write!(f, "全局控制文件内容非法：{what}"),
            GlobalCtlError::Full { what } => write!(f, "{what} 已满（容量上限）"),
            GlobalCtlError::NotFound { what } => write!(f, "找不到{what}"),
            GlobalCtlError::Duplicate { what } => write!(f, "重复：{what}"),
            GlobalCtlError::TooLong { field, len, max } => {
                write!(f, "{field} 过长（{len} > {max} 字节）")
            }
        }
    }
}

impl std::error::Error for GlobalCtlError {}

impl From<ControlFileError> for GlobalCtlError {
    fn from(e: ControlFileError) -> Self {
        GlobalCtlError::Core(e)
    }
}

impl From<io::Error> for GlobalCtlError {
    fn from(e: io::Error) -> Self {
        GlobalCtlError::Core(ControlFileError::Io(e))
    }
}

// ---------------------------------------------------------------------------
// 记录
// ---------------------------------------------------------------------------

/// **库条目**（页 1 偏移 0，64B）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LibraryEntry {
    /// 实例标识（16 字节；建实例时生成）。
    pub library_id: [u8; LIBRARY_ID_LEN],
    /// 建库时刻（墙钟毫秒）。
    pub created_at: u64,
}

impl LibraryEntry {
    /// 新建（调用方给 16 字节标识与时刻）。
    #[must_use]
    pub fn new(library_id: [u8; LIBRARY_ID_LEN], created_at: u64) -> Self {
        Self {
            library_id,
            created_at,
        }
    }

    /// 编码。
    #[must_use]
    pub fn encode(&self) -> [u8; LIBRARY_ENTRY_LEN] {
        let mut out = [0u8; LIBRARY_ENTRY_LEN];
        out[..LIBRARY_ID_LEN].copy_from_slice(&self.library_id);
        put_u16(&mut out, 16, GCF_FORMAT_VERSION);
        put_u16(&mut out, 18, CF_PAGE_SIZE_FIELD);
        put_u64(&mut out, 20, self.created_at);
        out
    }

    /// 解码（校验格式版本与页大小字段）。
    pub fn decode(b: &[u8]) -> Result<Self, GlobalCtlError> {
        let version = get_u16(b, 16);
        if version != GCF_FORMAT_VERSION {
            return Err(GlobalCtlError::Invalid {
                what: format!("库条目格式版本 {version} 不受支持"),
            });
        }
        if get_u16(b, 18) != CF_PAGE_SIZE_FIELD {
            return Err(GlobalCtlError::Invalid {
                what: "库条目 page_size 字段不符".to_owned(),
            });
        }
        let mut library_id = [0u8; LIBRARY_ID_LEN];
        library_id.copy_from_slice(&b[..LIBRARY_ID_LEN]);
        Ok(Self {
            library_id,
            created_at: get_u64(b, 20),
        })
    }
}

/// **计数段**（页 1 偏移 64，48B）——信息性，**不是分配判据**。
///
/// 分配永远**扫空槽位**（`status == 0`）：计数器在崩溃窗口里可能落后，
/// 而扫描结果永远自洽。计数器只服务运维与清单输出。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counters {
    /// 工作区记录**已用槽位数**（含墓碑）。
    pub workspace_used: u32,
    /// 池记录**已用槽位数**（含已移出）。
    pub fs_used: u32,
}

impl Counters {
    /// 编码。
    #[must_use]
    pub fn encode(&self) -> [u8; COUNTERS_LEN] {
        let mut out = [0u8; COUNTERS_LEN];
        put_u32(&mut out, 0, self.workspace_used);
        put_u32(&mut out, 4, self.fs_used);
        out
    }

    /// 解码。
    #[must_use]
    pub fn decode(b: &[u8]) -> Self {
        Self {
            workspace_used: get_u32(b, 0),
            fs_used: get_u32(b, 4),
        }
    }
}

/// **工作区记录**（280B）：`workspace_id` + 状态 + 根目录。
///
/// **这就是"工作区在哪"的权威**（`arch/02` §2.11 的分工表）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRecord {
    /// 工作区标识（48 位；`public` 的 `seq$` 分配）。
    pub workspace_id: WorkspaceId,
    /// 状态（[`WS_ACTIVE`] / [`WS_DROPPED`] / [`REC_EMPTY`]）。
    pub status: u8,
    /// 建/删时刻（墙钟毫秒）。
    pub created_at: u64,
    /// 工作区根目录（绝对路径；UTF-8 兼容字节串）。
    pub root: Vec<u8>,
}

impl WorkspaceRecord {
    /// 新建一条"在册"记录。
    pub fn new(
        workspace_id: WorkspaceId,
        root: impl Into<Vec<u8>>,
        created_at: u64,
    ) -> Result<Self, GlobalCtlError> {
        let root = root.into();
        if root.is_empty() {
            return Err(GlobalCtlError::Invalid {
                what: "工作区根目录为空".to_owned(),
            });
        }
        let rec = Self {
            workspace_id,
            status: WS_ACTIVE,
            created_at,
            root,
        };
        rec.check()?;
        Ok(rec)
    }

    /// 字段宽度自检。
    pub fn check(&self) -> Result<(), GlobalCtlError> {
        if self.root.len() > WORKSPACE_PATH_LEN {
            return Err(GlobalCtlError::TooLong {
                field: "工作区根目录",
                len: self.root.len(),
                max: WORKSPACE_PATH_LEN,
            });
        }
        Ok(())
    }

    /// 编码。
    pub fn encode(&self, out: &mut [u8; WORKSPACE_RECORD_LEN]) {
        out.fill(0);
        put_u48(out, 0, self.workspace_id.as_raw());
        out[6] = self.status;
        put_u64(out, 8, self.created_at);
        put_u16(out, 16, self.root.len() as u16);
        out[20..20 + self.root.len()].copy_from_slice(&self.root);
    }

    /// 解码（空槽位返回 `None`）。
    pub fn decode(b: &[u8]) -> Result<Option<Self>, GlobalCtlError> {
        let raw = get_u48(b, 0);
        let status = b[6];
        if status == REC_EMPTY {
            return Ok(None);
        }
        let workspace_id = WorkspaceId::from_raw(raw).ok_or_else(|| GlobalCtlError::Invalid {
            what: format!("工作区记录的工作区标识非法：{raw}"),
        })?;
        let len = usize::from(get_u16(b, 16));
        if len > WORKSPACE_PATH_LEN {
            return Err(GlobalCtlError::Invalid {
                what: format!("工作区根目录长度越界：{len}"),
            });
        }
        Ok(Some(Self {
            workspace_id,
            status,
            created_at: get_u64(b, 8),
            root: b[20..20 + len].to_vec(),
        }))
    }
}

/// **文件系统池记录**（336B）：`fs_slot` + 名字 + 路径 + 分配开关。
///
/// **名字是标识**（`DCL语句设计` v0.2 F1：`CREATE FILESYSTEM <名> USING '<路径>'`），
/// **路径只是创建参数**——之后引用一律用名字或槽位。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsRecord {
    /// 池槽位号（**单调分配、永不复用**——移出后槽位仍是那块盘的历史）。
    pub slot: u16,
    /// 状态（[`FS_IN_POOL`] / [`FS_REMOVED`] / [`REC_EMPTY`]）。
    pub status: u8,
    /// 分配开关（`ALTER FILESYSTEM … SET ALLOCATE = ON|OFF`；`false` = 退役排水）。
    pub allocate: bool,
    /// 登记时刻（墙钟毫秒）。
    pub created_at: u64,
    /// 文件系统名（实例内唯一）。
    pub name: Vec<u8>,
    /// 路径（可以是挂载点，也可以只是一个目录）。
    pub path: Vec<u8>,
}

impl FsRecord {
    /// 新建一条"在池"记录。
    pub fn new(
        slot: u16,
        name: impl Into<Vec<u8>>,
        path: impl Into<Vec<u8>>,
        created_at: u64,
    ) -> Result<Self, GlobalCtlError> {
        let rec = Self {
            slot,
            status: FS_IN_POOL,
            allocate: true,
            created_at,
            name: name.into(),
            path: path.into(),
        };
        rec.check()?;
        if rec.name.is_empty() {
            return Err(GlobalCtlError::Invalid {
                what: "文件系统名为空".to_owned(),
            });
        }
        Ok(rec)
    }

    /// 字段宽度自检。
    pub fn check(&self) -> Result<(), GlobalCtlError> {
        if self.name.len() > FS_NAME_LEN {
            return Err(GlobalCtlError::TooLong {
                field: "文件系统名",
                len: self.name.len(),
                max: FS_NAME_LEN,
            });
        }
        if self.path.len() > FS_PATH_LEN {
            return Err(GlobalCtlError::TooLong {
                field: "文件系统路径",
                len: self.path.len(),
                max: FS_PATH_LEN,
            });
        }
        Ok(())
    }

    /// 编码。
    pub fn encode(&self, out: &mut [u8; FS_RECORD_LEN]) {
        out.fill(0);
        put_u16(out, 0, self.slot);
        out[2] = self.status;
        out[3] = u8::from(self.allocate);
        put_u64(out, 4, self.created_at);
        out[12] = self.name.len() as u8;
        out[13] = self.path.len() as u8;
        out[16..16 + self.name.len()].copy_from_slice(&self.name);
        let path_off = 16 + FS_NAME_LEN;
        out[path_off..path_off + self.path.len()].copy_from_slice(&self.path);
    }

    /// 解码（空槽位返回 `None`）。
    pub fn decode(b: &[u8]) -> Result<Option<Self>, GlobalCtlError> {
        let status = b[2];
        if status == REC_EMPTY {
            return Ok(None);
        }
        let name_len = usize::from(b[12]);
        let path_len = usize::from(b[13]);
        if name_len > FS_NAME_LEN || path_len > FS_PATH_LEN {
            return Err(GlobalCtlError::Invalid {
                what: format!("池记录字段长度越界（名字 {name_len}、路径 {path_len}）"),
            });
        }
        let path_off = 16 + FS_NAME_LEN;
        Ok(Some(Self {
            slot: get_u16(b, 0),
            status,
            allocate: b[3] != 0,
            created_at: get_u64(b, 4),
            name: b[16..16 + name_len].to_vec(),
            path: b[path_off..path_off + path_len].to_vec(),
        }))
    }
}

// ---------------------------------------------------------------------------
// 定位（记录数组按页对齐，不跨页）
// ---------------------------------------------------------------------------

/// 第 `index` 条工作区记录的（页号, 页内偏移）。
#[must_use]
pub fn workspace_record_location(index: usize) -> Option<(u8, usize)> {
    if index >= MAX_WORKSPACE_RECORDS {
        return None;
    }
    let page = WORKSPACE_PAGES_FIRST + index / WORKSPACE_RECORDS_PER_PAGE;
    let off = (index % WORKSPACE_RECORDS_PER_PAGE) * WORKSPACE_RECORD_LEN;
    Some((page as u8, off))
}

/// 第 `index` 条池记录的（页号, 页内偏移）。
#[must_use]
pub fn fs_record_location(index: usize) -> Option<(u8, usize)> {
    if index >= MAX_FS_RECORDS {
        return None;
    }
    let page = FS_PAGES_FIRST + index / FS_RECORDS_PER_PAGE;
    let off = (index % FS_RECORDS_PER_PAGE) * FS_RECORD_LEN;
    Some((page as u8, off))
}

// ---------------------------------------------------------------------------
// 句柄
// ---------------------------------------------------------------------------

/// **全局控制文件**（两副本；协议与副本选择见 [`CfCore`]）。
pub struct GlobalControlFile<'a> {
    core: CfCore<'a>,
}

impl<'a> GlobalControlFile<'a> {
    /// **新建**（两副本）：写文件头（段偏移表 + 空 undo）、固定段（库条目 + 零计数）、
    /// 记录数组（全空槽位）。**不做隐式 fsync**——持久性点由调用方用 [`Self::sync`] 表达。
    pub fn format(
        io: &'a dyn FileIo,
        path_a: &Path,
        path_b: &Path,
        library: &LibraryEntry,
    ) -> Result<Self, GlobalCtlError> {
        let mut gcf = Self {
            core: CfCore::create(io, path_a, path_b, CF_KIND_GLOBAL)?,
        };
        for copy in 0..CF_COPIES {
            gcf.write_initial_pages(copy, library)?;
        }
        Ok(gcf)
    }

    /// **打开**既有全局控制文件（副本选择与自愈见 [`CfCore::open`]）。
    pub fn open(io: &'a dyn FileIo, path_a: &Path, path_b: &Path) -> Result<Self, GlobalCtlError> {
        Ok(Self {
            core: CfCore::open(io, path_a, path_b, CF_KIND_GLOBAL)?,
        })
    }

    /// 当前生效副本的页 0 `seq`。
    #[must_use]
    pub fn sequence(&self) -> u32 {
        self.core.sequence()
    }

    /// 当前生效副本号（0 = A、1 = B）。
    #[must_use]
    pub fn active_copy(&self) -> u8 {
        self.core.active_copy()
    }

    /// 持久性点：两副本 `fdatasync`。
    pub fn sync(&self) -> Result<(), GlobalCtlError> {
        self.core.sync()?;
        Ok(())
    }

    /// 关闭两副本句柄。
    pub fn close(self) -> Result<(), GlobalCtlError> {
        self.core.close()?;
        Ok(())
    }

    // -- 读 ----------------------------------------------------------------

    /// 库条目。
    pub fn library_entry(&self) -> Result<LibraryEntry, GlobalCtlError> {
        let b = self.core.read_fixed::<LIBRARY_ENTRY_LEN>(1, 0)?;
        LibraryEntry::decode(&b)
    }

    /// 计数段（信息性；见 [`Counters`]）。
    pub fn counters(&self) -> Result<Counters, GlobalCtlError> {
        let b = self.core.read_fixed::<COUNTERS_LEN>(1, COUNTERS_OFFSET)?;
        Ok(Counters::decode(&b))
    }

    /// **全部工作区记录**（含墓碑，按槽位序）。
    pub fn workspace_records(&self) -> Result<Vec<WorkspaceRecord>, GlobalCtlError> {
        let mut out = Vec::new();
        for index in 0..MAX_WORKSPACE_RECORDS {
            if let Some(rec) = self.workspace_record(index)? {
                out.push(rec);
            }
        }
        Ok(out)
    }

    /// **在册工作区**（`status == WS_ACTIVE`）——"实例里有哪些工作区"的答案。
    pub fn workspaces(&self) -> Result<Vec<WorkspaceRecord>, GlobalCtlError> {
        Ok(self
            .workspace_records()?
            .into_iter()
            .filter(|r| r.status == WS_ACTIVE)
            .collect())
    }

    /// 按标识找（含墓碑；要"存在"请再判 `status`）。
    pub fn workspace_by_id(
        &self,
        id: WorkspaceId,
    ) -> Result<Option<WorkspaceRecord>, GlobalCtlError> {
        Ok(self
            .workspace_records()?
            .into_iter()
            .find(|r| r.workspace_id == id))
    }

    /// 按根目录找（路径必须完全一致）。
    pub fn workspace_by_root(
        &self,
        root: &[u8],
    ) -> Result<Option<WorkspaceRecord>, GlobalCtlError> {
        Ok(self
            .workspace_records()?
            .into_iter()
            .find(|r| r.root == root))
    }

    /// 第 `index` 条工作区记录（空槽位 = `None`）。
    pub fn workspace_record(
        &self,
        index: usize,
    ) -> Result<Option<WorkspaceRecord>, GlobalCtlError> {
        let (page, off) = workspace_record_location(index).ok_or(GlobalCtlError::Invalid {
            what: format!("工作区记录下标越界：{index}"),
        })?;
        let b = self.core.read_fixed::<WORKSPACE_RECORD_LEN>(page, off)?;
        WorkspaceRecord::decode(&b)
    }

    /// **全部池记录**（含已移出，按槽位序）。
    pub fn fs_records(&self) -> Result<Vec<FsRecord>, GlobalCtlError> {
        let mut out = Vec::new();
        for index in 0..MAX_FS_RECORDS {
            if let Some(rec) = self.fs_record(index)? {
                out.push(rec);
            }
        }
        Ok(out)
    }

    /// **在池的文件系统**（`status == FS_IN_POOL`）。
    pub fn fs_members(&self) -> Result<Vec<FsRecord>, GlobalCtlError> {
        Ok(self
            .fs_records()?
            .into_iter()
            .filter(|r| r.status == FS_IN_POOL)
            .collect())
    }

    /// 按槽位找（含已移出）。
    pub fn fs_by_slot(&self, slot: u16) -> Result<Option<FsRecord>, GlobalCtlError> {
        Ok(self.fs_records()?.into_iter().find(|r| r.slot == slot))
    }

    /// 按名字找（含已移出；**在池判定由调用方做**）。
    pub fn fs_by_name(&self, name: &[u8]) -> Result<Option<FsRecord>, GlobalCtlError> {
        Ok(self.fs_records()?.into_iter().find(|r| r.name == name))
    }

    /// 第 `index` 条池记录（空槽位 = `None`）。
    pub fn fs_record(&self, index: usize) -> Result<Option<FsRecord>, GlobalCtlError> {
        let (page, off) = fs_record_location(index).ok_or(GlobalCtlError::Invalid {
            what: format!("池记录下标越界：{index}"),
        })?;
        let b = self.core.read_fixed::<FS_RECORD_LEN>(page, off)?;
        FsRecord::decode(&b)
    }

    // -- 写（建/删工作区、登记/移出池成员）-----------------------------------

    /// **登记一个工作区**：找第一个空槽位写入（**追加式**——已用槽位与墓碑都不复用）。
    ///
    /// **顺序纪律**（`arch/02` §2.11）：这是三步协议的**第三步**，必须最后做——
    /// 它一做，"工作区存在"就对外成立。返回写入的槽位号。
    pub fn insert_workspace(&mut self, rec: &WorkspaceRecord) -> Result<usize, GlobalCtlError> {
        rec.check()?;
        if rec.status != WS_ACTIVE {
            return Err(GlobalCtlError::Invalid {
                what: "新登记的工作区记录状态必须是「在册」".to_owned(),
            });
        }
        if let Some(existing) = self.workspace_by_id(rec.workspace_id)? {
            if existing.status == WS_ACTIVE {
                return Err(GlobalCtlError::Duplicate {
                    what: format!("工作区 {} 已在册", rec.workspace_id),
                });
            }
        }
        let index = self.first_empty_workspace_slot()?;
        self.write_workspace_record(index, rec)?;
        self.refresh_counters()?;
        Ok(index)
    }

    /// 改写一条工作区记录（**墓碑化**用它：`status = WS_DROPPED`）。
    ///
    /// `DROP WORKSPACE` 的**第一步**就是"先从在册清单里摘除"（可见性先断）。
    pub fn write_workspace_record(
        &mut self,
        index: usize,
        rec: &WorkspaceRecord,
    ) -> Result<(), GlobalCtlError> {
        rec.check()?;
        let (page, off) = workspace_record_location(index).ok_or(GlobalCtlError::Invalid {
            what: format!("工作区记录下标越界：{index}"),
        })?;
        let mut buf = [0u8; WORKSPACE_RECORD_LEN];
        rec.encode(&mut buf);
        self.core
            .update_interval(file_offset(usize::from(page), off), &buf)?;
        Ok(())
    }

    /// **把一个工作区标为已删**（墓碑；槽位不复用）。返回是否找到在册记录。
    pub fn mark_workspace_dropped(
        &mut self,
        id: WorkspaceId,
        dropped_at: u64,
    ) -> Result<bool, GlobalCtlError> {
        let Some(index) = self.workspace_index_of(id)? else {
            return Ok(false);
        };
        let mut rec = self
            .workspace_record(index)?
            .ok_or_else(|| GlobalCtlError::Invalid {
                what: "定位到的工作区槽位为空".to_owned(),
            })?;
        if rec.status != WS_ACTIVE {
            return Ok(false);
        }
        rec.status = WS_DROPPED;
        rec.created_at = dropped_at;
        self.write_workspace_record(index, &rec)?;
        self.refresh_counters()?;
        Ok(true)
    }

    /// **变更工作区根目录**（迁移：`Move`/换盘；`arch/02` §3.4）。
    pub fn set_workspace_root(
        &mut self,
        id: WorkspaceId,
        root: impl Into<Vec<u8>>,
    ) -> Result<(), GlobalCtlError> {
        let new_root = root.into();
        let index = self
            .workspace_index_of(id)?
            .ok_or_else(|| GlobalCtlError::NotFound {
                what: format!("工作区 {id}"),
            })?;
        let mut rec = self
            .workspace_record(index)?
            .ok_or_else(|| GlobalCtlError::Invalid {
                what: "定位到的工作区槽位为空".to_owned(),
            })?;
        rec.root = new_root;
        self.write_workspace_record(index, &rec)
    }

    /// **登记一个池成员**：分配槽位（**单调、永不复用**）并写入。返回槽位号。
    ///
    /// 判重两条：**名字实例内唯一**、**路径唯一**（同一目录登记两次没有意义）。
    pub fn insert_fs(
        &mut self,
        name: impl Into<Vec<u8>>,
        path: impl Into<Vec<u8>>,
        created_at: u64,
    ) -> Result<u16, GlobalCtlError> {
        let name = name.into();
        let path = path.into();
        if let Some(existing) = self.fs_by_name(&name)? {
            if existing.status == FS_IN_POOL {
                return Err(GlobalCtlError::Duplicate {
                    what: format!("文件系统名 `{}` 已在池", String::from_utf8_lossy(&name)),
                });
            }
        }
        if let Some(existing) = self.fs_members()?.into_iter().find(|r| r.path == path) {
            return Err(GlobalCtlError::Duplicate {
                what: format!(
                    "路径 `{}` 已是池成员 `{}`",
                    String::from_utf8_lossy(&path),
                    String::from_utf8_lossy(&existing.name)
                ),
            });
        }
        let slot = self.peek_next_fs_slot()?;
        let rec = FsRecord::new(slot, name, path, created_at)?;
        let index = self.first_empty_fs_slot()?;
        self.write_fs_record(index, &rec)?;
        self.refresh_counters()?;
        Ok(slot)
    }

    /// 改写一条池记录。
    pub fn write_fs_record(&mut self, index: usize, rec: &FsRecord) -> Result<(), GlobalCtlError> {
        rec.check()?;
        let (page, off) = fs_record_location(index).ok_or(GlobalCtlError::Invalid {
            what: format!("池记录下标越界：{index}"),
        })?;
        let mut buf = [0u8; FS_RECORD_LEN];
        rec.encode(&mut buf);
        self.core
            .update_interval(file_offset(usize::from(page), off), &buf)?;
        Ok(())
    }

    /// **分配开关**（`ALTER FILESYSTEM … SET ALLOCATE = ON|OFF`；F2 的排水阀）。
    pub fn set_fs_allocate(&mut self, slot: u16, allocate: bool) -> Result<(), GlobalCtlError> {
        let (index, mut rec) = self.fs_slot_entry(slot)?;
        if rec.status != FS_IN_POOL {
            return Err(GlobalCtlError::NotFound {
                what: format!("在池的文件系统（槽位 {slot}）"),
            });
        }
        rec.allocate = allocate;
        self.write_fs_record(index, &rec)
    }

    /// **把一个池成员标为已移出**（墓碑；`DROP FILESYSTEM` 的硬前置由调用方判）。
    pub fn mark_fs_removed(&mut self, slot: u16) -> Result<(), GlobalCtlError> {
        let (index, mut rec) = self.fs_slot_entry(slot)?;
        if rec.status != FS_IN_POOL {
            return Err(GlobalCtlError::NotFound {
                what: format!("在池的文件系统（槽位 {slot}）"),
            });
        }
        rec.status = FS_REMOVED;
        rec.allocate = false;
        self.write_fs_record(index, &rec)?;
        self.refresh_counters()?;
        Ok(())
    }

    // -- 内部 --------------------------------------------------------------

    fn write_initial_pages(
        &mut self,
        copy: usize,
        library: &LibraryEntry,
    ) -> Result<(), GlobalCtlError> {
        // 页 0：文件头（段偏移表 + 空 undo）。
        let mut page0 = blank_page(
            copy as u8,
            0,
            (4 + SEGMENT_TABLE_ITEMS * SEGMENT_ITEM_LEN + UNDO_RECORD_LEN) as u16,
            CF_KIND_GLOBAL,
        );
        {
            let body = page_body_mut(&mut page0);
            put_u16(body, 0, GCF_FORMAT_VERSION);
            put_u16(body, 2, CF_PAGE_SIZE_FIELD);
            let segments = [
                (0u8, 0u16, 1u16, 0u16, 0u16),
                (1, 1, 1, 0, 2),
                (
                    2,
                    WORKSPACE_PAGES_FIRST as u16,
                    WORKSPACE_PAGES as u16,
                    WORKSPACE_RECORD_LEN as u16,
                    MAX_WORKSPACE_RECORDS as u16,
                ),
                (
                    3,
                    FS_PAGES_FIRST as u16,
                    FS_PAGES as u16,
                    FS_RECORD_LEN as u16,
                    MAX_FS_RECORDS as u16,
                ),
                (4, 0, 0, 0, 0),
                (5, 0, 0, 0, 0),
                (6, 0, 0, 0, 0),
                (7, 0, 0, 0, 0),
            ];
            for (i, &(seg, start, pages, size, count)) in segments.iter().enumerate() {
                let off = SEGMENT_TABLE_OFFSET + i * SEGMENT_ITEM_LEN;
                body[off..off + SEGMENT_ITEM_LEN]
                    .copy_from_slice(&segment_entry(seg, start, pages, size, count));
            }
        }
        self.core.write_page(copy, 0, &mut page0)?;

        // 页 1：固定段（库条目 + 零计数）。
        let mut page1 = blank_page(copy as u8, 1, GCF_FIXED_SEGMENT_LEN as u16, CF_KIND_GLOBAL);
        {
            let body = page_body_mut(&mut page1);
            body[..LIBRARY_ENTRY_LEN].copy_from_slice(&library.encode());
            body[COUNTERS_OFFSET..COUNTERS_OFFSET + COUNTERS_LEN]
                .copy_from_slice(&Counters::default().encode());
        }
        self.core.write_page(copy, 1, &mut page1)?;

        // 记录数组：全空槽位（页体全零）。
        for page_no in WORKSPACE_PAGES_FIRST..CF_PAGES {
            let payload = if page_no < FS_PAGES_FIRST {
                (WORKSPACE_RECORDS_PER_PAGE * WORKSPACE_RECORD_LEN) as u16
            } else {
                (FS_RECORDS_PER_PAGE * FS_RECORD_LEN) as u16
            };
            let mut page = blank_page(copy as u8, page_no as u8, payload, CF_KIND_GLOBAL);
            self.core.write_page(copy, page_no as u8, &mut page)?;
        }
        Ok(())
    }

    /// 计数段（信息性）刷新：扫一遍已用槽位。
    fn refresh_counters(&mut self) -> Result<(), GlobalCtlError> {
        let ws_used = self.used_workspace_slots()?;
        let fs_used = self.used_fs_slots()?;
        let counters = Counters {
            workspace_used: ws_used,
            fs_used,
        };
        self.core
            .update_interval(file_offset(1, COUNTERS_OFFSET), &counters.encode())?;
        Ok(())
    }

    fn used_workspace_slots(&self) -> Result<u32, GlobalCtlError> {
        let mut n = 0u32;
        for index in 0..MAX_WORKSPACE_RECORDS {
            if self.workspace_record(index)?.is_some() {
                n += 1;
            }
        }
        Ok(n)
    }

    fn used_fs_slots(&self) -> Result<u32, GlobalCtlError> {
        let mut n = 0u32;
        for index in 0..MAX_FS_RECORDS {
            if self.fs_record(index)?.is_some() {
                n += 1;
            }
        }
        Ok(n)
    }

    fn first_empty_workspace_slot(&self) -> Result<usize, GlobalCtlError> {
        for index in 0..MAX_WORKSPACE_RECORDS {
            if self.workspace_record(index)?.is_none() {
                return Ok(index);
            }
        }
        Err(GlobalCtlError::Full {
            what: "工作区记录"
        })
    }

    fn first_empty_fs_slot(&self) -> Result<usize, GlobalCtlError> {
        for index in 0..MAX_FS_RECORDS {
            if self.fs_record(index)?.is_none() {
                return Ok(index);
            }
        }
        Err(GlobalCtlError::Full { what: "池记录" })
    }

    fn workspace_index_of(&self, id: WorkspaceId) -> Result<Option<usize>, GlobalCtlError> {
        for index in 0..MAX_WORKSPACE_RECORDS {
            if let Some(rec) = self.workspace_record(index)? {
                if rec.workspace_id == id {
                    return Ok(Some(index));
                }
            }
        }
        Ok(None)
    }

    fn fs_slot_entry(&self, slot: u16) -> Result<(usize, FsRecord), GlobalCtlError> {
        for index in 0..MAX_FS_RECORDS {
            if let Some(rec) = self.fs_record(index)? {
                if rec.slot == slot {
                    return Ok((index, rec));
                }
            }
        }
        Err(GlobalCtlError::NotFound {
            what: format!("池槽位 {slot}"),
        })
    }

    /// **分配一个工作区标识** = 已用最大值 + 1（**从 1 起、不复用**）。
    ///
    /// **为什么由这份文件分配**：它是实例级注册表，且**必须在 `public` 不可用时
    /// 也能工作**（`public` 可能是刚被建出来的那一个、也可能正坏着）。
    /// 与槽位分配同规：**扫出来的事实，不是计数器**（崩溃窗口里永远自洽）。
    pub fn allocate_workspace_id(&self) -> Result<WorkspaceId, GlobalCtlError> {
        let mut max: u64 = 0;
        for index in 0..MAX_WORKSPACE_RECORDS {
            if let Some(rec) = self.workspace_record(index)? {
                max = max.max(rec.workspace_id.as_raw());
            }
        }
        WorkspaceId::from_raw(max + 1).ok_or(GlobalCtlError::Full {
            what: "工作区标识空间（48 位用尽）",
        })
    }

    /// **下一个槽位号** = 已用过的最大槽位 + 1（**永不复用**）。
    ///
    /// **只读**：调用方在写 `fs$` 行之前先取它（写序：属性先、注册表最后）。
    pub fn next_fs_slot(&self) -> Result<u16, GlobalCtlError> {
        self.peek_next_fs_slot()
    }

    fn peek_next_fs_slot(&self) -> Result<u16, GlobalCtlError> {
        let mut max: Option<u16> = None;
        for index in 0..MAX_FS_RECORDS {
            if let Some(rec) = self.fs_record(index)? {
                max = Some(max.map_or(rec.slot, |m: u16| m.max(rec.slot)));
            }
        }
        let next = max.map_or(1u16, |m| m.saturating_add(1));
        if next == u16::MAX {
            return Err(GlobalCtlError::Full {
                what: "池槽位号"
            });
        }
        Ok(next)
    }
}

/// 段偏移表项（与工作区控制文件同构；本模块的段表布局见模块文档）。
fn segment_entry(seg: u8, start_page: u16, pages: u16, item_size: u16, count: u16) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[0] = seg;
    out[1..3].copy_from_slice(&start_page.to_le_bytes());
    out[3..5].copy_from_slice(&pages.to_le_bytes());
    out[5..7].copy_from_slice(&item_size.to_le_bytes());
    out[7..9].copy_from_slice(&count.to_le_bytes());
    out
}

/// **库标识生成**：随机（`/dev/urandom` 不可用时退回"时间 × pid"混合）。
///
/// **仅用于识别**（"这盘控制文件属于哪个实例"），**不是密钥**——
/// 照 `workspace_ref设计` §"为什么不加密钥"的同一条边界。
#[must_use]
pub fn generate_library_id() -> [u8; LIBRARY_ID_LEN] {
    use std::io::Read;
    let mut out = [0u8; LIBRARY_ID_LEN];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        if f.read_exact(&mut out).is_ok() {
            return out;
        }
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let pid = u128::from(std::process::id());
    let mix = now.rotate_left(37) ^ pid.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    out.copy_from_slice(&mix.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use bicdb_workspace::io::MemFileIo;

    use super::*;
    use crate::controlfile::{
        ArchiveMode, ArchiveRecord, ControlFile, RedoEntries, WorkspaceEntry,
    };

    const A: &str = "/mem/gcontrol01.ctl";
    const B: &str = "/mem/gcontrol02.ctl";

    fn new_mem() -> MemFileIo {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        io
    }

    fn library() -> LibraryEntry {
        LibraryEntry::new([7u8; LIBRARY_ID_LEN], 1_700_000_000_000)
    }

    fn format_gcf<'a>(io: &'a dyn FileIo) -> GlobalControlFile<'a> {
        GlobalControlFile::format(io, Path::new(A), Path::new(B), &library()).unwrap()
    }

    fn reopen(io: &dyn FileIo) -> GlobalControlFile<'_> {
        GlobalControlFile::open(io, Path::new(A), Path::new(B)).unwrap()
    }

    fn ws_rec(id: u64, root: &str) -> WorkspaceRecord {
        WorkspaceRecord::new(WorkspaceId::from_raw(id).unwrap(), root.as_bytes(), 42).unwrap()
    }

    // -- 布局钉住 ------------------------------------------------------------

    #[test]
    fn layout_constants_are_pinned() {
        assert_eq!(LIBRARY_ENTRY_LEN, 64);
        assert_eq!(COUNTERS_OFFSET, 64);
        assert_eq!(COUNTERS_LEN, 48);
        assert_eq!(GCF_FIXED_SEGMENT_LEN, 112);
        // 记录数组：每页整条数、容量精确覆盖。
        assert_eq!(WORKSPACE_RECORDS_PER_PAGE, 58);
        assert_eq!(MAX_WORKSPACE_RECORDS, 580);
        assert_eq!(FS_RECORDS_PER_PAGE, 48);
        assert_eq!(MAX_FS_RECORDS, 384);
        // 工作区区在前、池区在后，两者不重叠、加起来正好到页尾。
        assert_eq!(WORKSPACE_PAGES_FIRST, 2);
        assert_eq!(FS_PAGES_FIRST, 12);
        assert_eq!(FS_PAGES_FIRST + FS_PAGES, CF_PAGES);
        // 按页对齐打包：单条记录恒在单页内。
        assert_eq!(workspace_record_location(57), Some((2, 57 * 280)));
        assert_eq!(workspace_record_location(58), Some((3, 0)));
        assert_eq!(workspace_record_location(579), Some((11, 57 * 280)));
        assert_eq!(workspace_record_location(580), None);
        assert_eq!(fs_record_location(47), Some((12, 47 * 336)));
        assert_eq!(fs_record_location(48), Some((13, 0)));
        assert_eq!(fs_record_location(383), Some((19, 47 * 336)));
        assert_eq!(fs_record_location(384), None);
    }

    #[test]
    fn record_roundtrip_and_bounds() {
        let rec = ws_rec(9, "/u01/app/bicdb/public");
        let mut buf = [0u8; WORKSPACE_RECORD_LEN];
        rec.encode(&mut buf);
        assert_eq!(WorkspaceRecord::decode(&buf).unwrap(), Some(rec.clone()));
        // 空槽位 = None
        assert_eq!(
            WorkspaceRecord::decode(&[0u8; WORKSPACE_RECORD_LEN]).unwrap(),
            None
        );
        // 长度越界 ⇒ 具名拒绝
        let long = vec![b'x'; WORKSPACE_PATH_LEN + 1];
        assert!(matches!(
            WorkspaceRecord::new(WorkspaceId::from_raw(1).unwrap(), long, 0),
            Err(GlobalCtlError::TooLong { .. })
        ));

        let fs = FsRecord::new(3, b"data1", b"/srv/data1", 77).unwrap();
        let mut buf = [0u8; FS_RECORD_LEN];
        fs.encode(&mut buf);
        assert_eq!(FsRecord::decode(&buf).unwrap(), Some(fs.clone()));
        assert_eq!(FsRecord::decode(&[0u8; FS_RECORD_LEN]).unwrap(), None);
        assert!(matches!(
            FsRecord::new(1, vec![b'n'; FS_NAME_LEN + 1], b"/p", 0),
            Err(GlobalCtlError::TooLong { .. })
        ));
        assert!(matches!(
            FsRecord::new(1, b"n", vec![b'p'; FS_PATH_LEN + 1], 0),
            Err(GlobalCtlError::TooLong { .. })
        ));
    }

    // -- 新建 / 重开 / 自愈 --------------------------------------------------

    #[test]
    fn format_writes_both_copies_and_reopens() {
        let io = new_mem();
        {
            let gcf = format_gcf(&io);
            assert_eq!(gcf.library_entry().unwrap(), library());
            assert_eq!(gcf.counters().unwrap(), Counters::default());
            assert!(gcf.workspaces().unwrap().is_empty());
            assert!(gcf.fs_members().unwrap().is_empty());
            gcf.sync().unwrap();
            gcf.close().unwrap();
        }
        let gcf = reopen(&io);
        assert_eq!(gcf.library_entry().unwrap(), library());
        assert_eq!(gcf.active_copy(), 0, "两份 seq 相同 ⇒ 取 A");
    }

    #[test]
    fn a_workspace_control_file_is_refused_as_global() {
        let io = new_mem();
        // 用工作区控制文件的写法建一份，再用全局的口打开 ⇒ 种类不符，当场失败。
        {
            let ws = WorkspaceEntry {
                workspace_id: WorkspaceId::from_raw(1).unwrap(),
                created_at: 0,
                derived_from: None,
                derived_at_seq: bicdb_common::seq::CommitSeq::from_raw(0).unwrap(),
            };
            let cf = ControlFile::format(
                &io,
                Path::new("/mem/w01.ctl"),
                Path::new("/mem/w02.ctl"),
                &ws,
                &RedoEntries::new(2, 1).unwrap(),
                &ArchiveRecord::new(ArchiveMode::NoArchive),
            )
            .unwrap();
            cf.sync().unwrap();
            cf.close().unwrap();
        }
        let err = match GlobalControlFile::open(
            &io,
            Path::new("/mem/w01.ctl"),
            Path::new("/mem/w02.ctl"),
        ) {
            Ok(_) => panic!("工作区控制文件不该被当作全局控制文件打开"),
            Err(e) => e,
        };
        let msg = err.to_string();
        assert!(msg.contains("种类"), "{msg}");
    }

    // -- 工作区登记 ----------------------------------------------------------

    #[test]
    fn workspace_registry_insert_find_and_tombstone() {
        let io = new_mem();
        let mut gcf = format_gcf(&io);
        let a = ws_rec(1, "/home/bicdb/public");
        let b = ws_rec(2, "/home/bicdb/alice_ws");
        let ia = gcf.insert_workspace(&a).unwrap();
        let ib = gcf.insert_workspace(&b).unwrap();
        assert_eq!((ia, ib), (0, 1));
        assert_eq!(gcf.counters().unwrap().workspace_used, 2);

        let listed = gcf.workspaces().unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].workspace_id, a.workspace_id);
        assert_eq!(
            gcf.workspace_by_root(b"/home/bicdb/alice_ws").unwrap(),
            Some(b.clone())
        );
        assert_eq!(
            gcf.workspace_by_id(a.workspace_id).unwrap(),
            Some(a.clone())
        );

        // 同 id 二次登记 ⇒ 拒绝
        assert!(matches!(
            gcf.insert_workspace(&a),
            Err(GlobalCtlError::Duplicate { .. })
        ));

        // 墓碑：不在"在册"清单里，但记录仍在（槽位不复用 ⇒ 下一条拿到 2）。
        assert!(gcf.mark_workspace_dropped(a.workspace_id, 99).unwrap());
        assert_eq!(gcf.workspaces().unwrap().len(), 1);
        assert_eq!(
            gcf.workspace_by_id(a.workspace_id).unwrap().unwrap().status,
            WS_DROPPED
        );
        let c = ws_rec(3, "/home/bicdb/bob_ws");
        assert_eq!(gcf.insert_workspace(&c).unwrap(), 2);
        // 再次墓碑化同一个 ⇒ false（已经不在册）
        assert!(!gcf.mark_workspace_dropped(a.workspace_id, 123).unwrap());

        // 同一 id 重新登记（先前的墓碑不挡路——它已经不在册）
        assert!(gcf.insert_workspace(&a).is_ok());

        // 改根目录
        gcf.set_workspace_root(b.workspace_id, "/mnt/big/alice_ws")
            .unwrap();
        assert_eq!(
            gcf.workspace_by_id(b.workspace_id)
                .unwrap()
                .unwrap()
                .root
                .as_slice(),
            b"/mnt/big/alice_ws"
        );
    }

    #[test]
    fn workspace_registry_survives_reopen_and_heals_a_copy() {
        let io = new_mem();
        let a = ws_rec(11, "/data/public");
        {
            let mut gcf = format_gcf(&io);
            gcf.insert_workspace(&a).unwrap();
            gcf.sync().unwrap();
            gcf.close().unwrap();
        }
        // 撕坏 B 副本的页 0 ⇒ 打开时由 A 整份重建（内容仍一致）。
        {
            use bicdb_workspace::io::{FileIo, OpenOptions};
            let h = io
                .open(Path::new(B), OpenOptions::new().read(true).write(true))
                .unwrap();
            io.write_at(h, &[0xFFu8; 16], 0).unwrap();
            io.close(h).unwrap();
        }
        let gcf = reopen(&io);
        assert_eq!(gcf.active_copy(), 0);
        assert_eq!(gcf.workspaces().unwrap(), vec![a]);
    }

    // -- 池登记 --------------------------------------------------------------

    #[test]
    fn fs_pool_slots_are_monotonic_and_never_reused() {
        let io = new_mem();
        let mut gcf = format_gcf(&io);
        let s1 = gcf.insert_fs(b"data1", b"/srv/data1", 1).unwrap();
        let s2 = gcf.insert_fs(b"data2", b"/srv/data2", 2).unwrap();
        assert_eq!((s1, s2), (1, 2), "槽位从 1 起、单调");
        assert_eq!(gcf.counters().unwrap().fs_used, 2);

        // 同名/同路径 ⇒ 拒绝
        assert!(matches!(
            gcf.insert_fs(b"data1", b"/srv/other", 3),
            Err(GlobalCtlError::Duplicate { .. })
        ));
        assert!(matches!(
            gcf.insert_fs(b"data3", b"/srv/data1", 3),
            Err(GlobalCtlError::Duplicate { .. })
        ));

        // 移出后再登记：**槽位不复用**（3，不是 1）
        gcf.mark_fs_removed(s1).unwrap();
        assert_eq!(gcf.fs_members().unwrap().len(), 1);
        assert_eq!(gcf.fs_by_slot(s1).unwrap().unwrap().status, FS_REMOVED);
        let s3 = gcf.insert_fs(b"data1", b"/srv/data1", 4).unwrap();
        assert_eq!(s3, 3, "槽位单调——已移出的槽位永远属于那块盘的历史");
    }

    #[test]
    fn fs_allocate_toggle_is_persisted() {
        let io = new_mem();
        let slot = {
            let mut gcf = format_gcf(&io);
            let slot = gcf.insert_fs(b"data1", b"/srv/data1", 1).unwrap();
            assert!(gcf.fs_by_slot(slot).unwrap().unwrap().allocate);
            gcf.set_fs_allocate(slot, false).unwrap();
            gcf.sync().unwrap();
            gcf.close().unwrap();
            slot
        };
        let gcf = reopen(&io);
        let rec = gcf.fs_by_slot(slot).unwrap().unwrap();
        assert_eq!(rec.status, FS_IN_POOL);
        assert!(!rec.allocate, "ALLOCATE = OFF 落盘");
        // 移出的成员不能再改开关
        let mut gcf = gcf;
        gcf.mark_fs_removed(slot).unwrap();
        assert!(matches!(
            gcf.set_fs_allocate(slot, true),
            Err(GlobalCtlError::NotFound { .. })
        ));
    }

    #[test]
    fn workspace_ids_are_allocated_monotonically() {
        let io = new_mem();
        let mut gcf = format_gcf(&io);
        assert_eq!(
            gcf.allocate_workspace_id().unwrap().as_raw(),
            1,
            "空实例从 1 起"
        );
        gcf.insert_workspace(&ws_rec(1, "/data/public")).unwrap();
        assert_eq!(gcf.allocate_workspace_id().unwrap().as_raw(), 2);
        gcf.insert_workspace(&ws_rec(7, "/data/w7")).unwrap();
        assert_eq!(gcf.allocate_workspace_id().unwrap().as_raw(), 8);
        // 墓碑也占号（id 不复用）
        gcf.mark_workspace_dropped(WorkspaceId::from_raw(7).unwrap(), 0)
            .unwrap();
        assert_eq!(gcf.allocate_workspace_id().unwrap().as_raw(), 8);
    }

    #[test]
    fn library_identity_distinguishes_instances() {
        let io = new_mem();
        let other = LibraryEntry::new([9u8; LIBRARY_ID_LEN], 5);
        {
            let gcf = GlobalControlFile::format(&io, Path::new(A), Path::new(B), &other).unwrap();
            gcf.sync().unwrap();
            gcf.close().unwrap();
        }
        let gcf = reopen(&io);
        assert_eq!(
            gcf.library_entry().unwrap().library_id,
            [9u8; LIBRARY_ID_LEN]
        );
        assert_ne!(
            gcf.library_entry().unwrap().library_id,
            library().library_id
        );
    }
}
