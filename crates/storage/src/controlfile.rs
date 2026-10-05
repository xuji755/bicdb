//! 工作区控制文件：**20 页 × 16 KiB 的恢复锚点**（存储架构 §2.6 / §5.11）。
//!
//! # 形态
//!
//! ```text
//! 页 0     文件头（段偏移表 8 项 + 固定偏移 132 的单条 undo 记录）
//! 页 1     固定段：工作区条目 64B │ 检查点进度 48B │ Redo 条目 128B │ 归档记录 320B
//! 页 2–19  数据文件记录数组：1024 × 280B（**按页对齐**——每页 58 条，不跨页）
//! 每页 = 页头 24B + 页体 + 页尾校验 4B（CRC32C 覆盖全页、校验字段按零参与）
//! ```
//!
//! # 两副本与更新协议（§2.6 / §5.11 已定案）
//!
//! - **副本** = 两个文件 [`CF_FILE_NAMES`]；**先写 A、成功后写 B**；打开时取
//!   "**有效（页校验通过 + undo 已回滚）且 `seq` 较大**"者；另一份损坏或落后时
//!   **由有效副本整份重建**；两份都不可用 ⇒ **该工作区无法启动**（已达到的取舍）。
//! - **单区间更新**（≤ [`MAX_UPDATE_LEN`]、不跨页、等长原地覆盖）：
//!   ① 写 undo（目标偏移 + 旧值）→ ② 写目标字节 → ③ 页 0 `seq` +1（发布）
//!   → ④ 清 undo。
//! - **崩溃后只看 undo 空不空**：非空 ⇒ 用旧值回滚目标位置再清空（幂等，
//!   "多退一次"可容忍——控制文件是状态发布，不是数据）。
//! - 因此**控制文件本身不产生 redo**（§5.11）：它由下一轮发布自愈。
//!
//! # 与设计文档的对应
//!
//! 段偏移表的段号：0 = 文件头、1 = 固定段、2 = 数据文件记录数组、3–7 保留；
//! `current_group = `[`NO_CURRENT_GROUP`] 表示"尚无当前组"（新建工作区，
//! 首次切换时置位）；magic 按**字符字节序**存储（文件中即 `BICF`）。

use std::io;
use std::path::Path;

use bicdb_common::checksum::{checksum_with_zeroed_field, crc32c, PAGE_SIZE};
use bicdb_common::seq::{CommitSeq, Lsn, SEQ_MAX};
use bicdb_workspace::id::WorkspaceId;
use bicdb_workspace::io::{FileHandle, FileIo, OpenOptions};

// ---------------------------------------------------------------------------
// 常量（字节布局的唯一定义处；测试逐项钉住）
// ---------------------------------------------------------------------------

/// 控制文件页数。
pub const CF_PAGES: usize = 20;
/// 控制文件字节大小（320 KiB）。
pub const CF_SIZE: usize = CF_PAGES * PAGE_SIZE;
/// 页头长度。
pub const CF_PAGE_HEADER_LEN: usize = 24;
/// 页尾校验长度。
pub const CF_PAGE_TRAILER_LEN: usize = 4;
/// 页体容量。
pub const CF_PAGE_BODY_LEN: usize = PAGE_SIZE - CF_PAGE_HEADER_LEN - CF_PAGE_TRAILER_LEN;
/// magic（文件中即字符串 `BICF`；设计文档记作 `0x42494346`）。
pub const CF_MAGIC: [u8; 4] = *b"BICF";
/// 当前格式版本（冻结后不可变；高于此值的工作区**拒绝打开**，绝不尝试解析）。
pub const CF_FORMAT_VERSION: u16 = 1;
/// 页大小字段值。
pub const CF_PAGE_SIZE_FIELD: u16 = PAGE_SIZE as u16;
/// 副本数（A / B）。
pub const CF_COPIES: usize = 2;
/// 两副本的约定文件名（§2.1 的 `control/` 目录）。
pub const CF_FILE_NAMES: [&str; CF_COPIES] = ["control01.ctl", "control02.ctl"];

/// 段偏移表在页 0 页体内的偏移。
pub const SEGMENT_TABLE_OFFSET: usize = 4;
/// 段偏移表项数。
pub const SEGMENT_TABLE_ITEMS: usize = 8;
/// 段偏移表单项长度。
pub const SEGMENT_ITEM_LEN: usize = 16;

/// undo 记录在页 0 页体内的固定偏移（崩溃后第一步就要读到它）。
pub const UNDO_RECORD_OFFSET: usize = 132;
/// undo 旧值上限（= 单次修改的最大字段长度 ⇒ 单次更新区间上限）。
pub const UNDO_OLD_MAX: usize = 512;
/// undo 记录定长：`target_off 4B │ old_len 2B │ old_data[512B] │ 校验 4B`。
pub const UNDO_RECORD_LEN: usize = 522;
/// 单次更新的区间上限（= [`UNDO_OLD_MAX`]）。
pub const MAX_UPDATE_LEN: usize = UNDO_OLD_MAX;

/// 页 1 固定段：工作区条目偏移/长度。
pub const WORKSPACE_ENTRY_OFFSET: usize = 0;
/// 工作区条目长度。
pub const WORKSPACE_ENTRY_LEN: usize = 64;
/// 页 1 固定段：检查点进度偏移/长度。
pub const CHECKPOINT_PROGRESS_OFFSET: usize = 64;
/// 检查点进度长度。
pub const CHECKPOINT_PROGRESS_LEN: usize = 48;
/// 页 1 固定段：Redo 条目偏移/长度。
pub const REDO_ENTRIES_OFFSET: usize = 112;
/// Redo 条目长度。
pub const REDO_ENTRIES_LEN: usize = 128;
/// 页 1 固定段：归档记录偏移/长度。
pub const ARCHIVE_RECORD_OFFSET: usize = 240;
/// 归档记录长度。
pub const ARCHIVE_RECORD_LEN: usize = 320;
/// 页 1 固定段总长。
pub const FIXED_SEGMENT_LEN: usize =
    WORKSPACE_ENTRY_LEN + CHECKPOINT_PROGRESS_LEN + REDO_ENTRIES_LEN + ARCHIVE_RECORD_LEN;

/// **墙钟采样对**（§11.10 的目标点插值用）：环容量 256 对。
pub const SAMPLE_PAIRS: usize = 256;
/// 单个采样对：`commit_seq 6B │ 时刻毫秒 6B`（各 48 位小端）。
pub const SAMPLE_PAIR_LEN: usize = 12;
/// 采样环头（`count 2B │ next 2B`，紧随固定段；随检查点区间一起发布）。
pub const SAMPLE_HEAD_OFFSET: usize = FIXED_SEGMENT_LEN;
/// 采样对数组起点。
pub const SAMPLE_PAIRS_OFFSET: usize = SAMPLE_HEAD_OFFSET + 4;

/// 数据文件记录长度。
pub const DATA_FILE_RECORD_LEN: usize = 280;
/// 数据文件记录上限（与 ROWID 的 `file_id` 10 位对齐）。
pub const MAX_DATA_FILE_RECORDS: usize = 1024;
/// 每页容纳的记录数（记录**不跨页**；剩余尾部留空）。
pub const DATA_FILE_RECORDS_PER_PAGE: usize = CF_PAGE_BODY_LEN / DATA_FILE_RECORD_LEN;
/// 数据文件记录的路径字段长度。
pub const DATA_FILE_PATH_LEN: usize = 256;

/// redo 组上限（Redo 条目内的数组长度）。
pub const MAX_REDO_GROUPS: usize = 8;
/// `current_group` 的"尚无当前组"取值。
pub const NO_CURRENT_GROUP: u8 = 0xFF;

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

/// 控制文件错误（**明确判定**，不静默）。
#[derive(Debug)]
pub enum ControlFileError {
    /// 底层 I/O。
    Io(io::Error),
    /// 页损坏（magic / 副本号 / 页号 / 页尾校验）。
    Damaged {
        /// 副本号（0 = A、1 = B）。
        copy: u8,
        /// 页号。
        page: u8,
        /// 检出原因。
        reason: &'static str,
    },
    /// 两副本都不可用——**该工作区无法启动**（设计上接受的取舍）。
    BothCopiesInvalid {
        /// 副本 A 的不可用原因。
        a: String,
        /// 副本 B 的不可用原因。
        b: String,
    },
    /// 格式版本不受支持（**拒绝解析**）。
    UnsupportedFormat {
        /// 文件中的版本号。
        found: u16,
    },
    /// 字段取值越出 48 位域（磁盘内容损坏）。
    OutOfDomain {
        /// 字段名。
        field: &'static str,
    },
    /// 更新区间超过 512 B。
    IntervalTooLarge {
        /// 实际长度。
        len: usize,
    },
    /// 更新区间越出页体（跨页或侵入页头/页尾）。
    IntervalOutOfBody,
    /// redo 条目自身不自洽（拒绝发布）。
    InconsistentRedo {
        /// 原因。
        reason: &'static str,
    },
    /// 数据文件记录下标越界。
    RecordIndexOutOfRange {
        /// 下标。
        index: usize,
    },
    /// 路径过长（> 255 字节）。
    PathTooLong {
        /// 实际长度。
        len: usize,
    },
    /// 路径含内嵌 NUL（定长字段以 NUL 表示结束）。
    PathContainsNul,
}

impl std::fmt::Display for ControlFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ControlFileError::Io(e) => write!(f, "控制文件 I/O：{e}"),
            ControlFileError::Damaged { copy, page, reason } => {
                write!(
                    f,
                    "控制文件副本 {} 页 {page} 损坏：{reason}",
                    copy_name(*copy)
                )
            }
            ControlFileError::BothCopiesInvalid { a, b } => write!(
                f,
                "控制文件两副本都不可用——该工作区无法启动：A = {a}；B = {b}"
            ),
            ControlFileError::UnsupportedFormat { found } => write!(
                f,
                "控制文件格式版本 {found} 高于引擎支持的 {}（拒绝解析）",
                CF_FORMAT_VERSION
            ),
            ControlFileError::OutOfDomain { field } => {
                write!(f, "控制文件字段 {field} 越出 48 位域")
            }
            ControlFileError::IntervalTooLarge { len } => {
                write!(f, "更新区间 {len} B 超过上限 {MAX_UPDATE_LEN} B")
            }
            ControlFileError::IntervalOutOfBody => {
                f.write_str("更新区间越出页体（不得跨页、不得侵入页头/页尾）")
            }
            ControlFileError::InconsistentRedo { reason } => {
                write!(f, "redo 条目不自洽（拒绝发布）：{reason}")
            }
            ControlFileError::RecordIndexOutOfRange { index } => {
                write!(
                    f,
                    "数据文件记录下标 {index} 越界（上限 {MAX_DATA_FILE_RECORDS}）"
                )
            }
            ControlFileError::PathTooLong { len } => write!(
                f,
                "路径 {len} B 超过上限 {} B（定长字段，末尾留 NUL）",
                DATA_FILE_PATH_LEN - 1
            ),
            ControlFileError::PathContainsNul => {
                f.write_str("路径含内嵌 NUL（定长字段以 NUL 表示结束）")
            }
        }
    }
}

impl std::error::Error for ControlFileError {}

impl From<io::Error> for ControlFileError {
    fn from(e: io::Error) -> Self {
        ControlFileError::Io(e)
    }
}

/// 副本的可读名字（诊断用）。
#[must_use]
pub fn copy_name(copy: u8) -> &'static str {
    match copy {
        0 => "A",
        _ => "B",
    }
}

// ---------------------------------------------------------------------------
// 小端字段读写（页内偏移访问的唯一入口）
// ---------------------------------------------------------------------------

fn get_u16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(b[off..off + 2].try_into().expect("2 字节字段"))
}

fn get_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().expect("4 字节字段"))
}

fn get_u64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().expect("8 字节字段"))
}

/// 读 48 位域（6 字节小端）。
fn get_u48(b: &[u8], off: usize) -> u64 {
    let mut raw = [0u8; 8];
    raw[..6].copy_from_slice(&b[off..off + 6]);
    u64::from_le_bytes(raw)
}

fn put_u16(b: &mut [u8], off: usize, v: u16) {
    b[off..off + 2].copy_from_slice(&v.to_le_bytes());
}

fn put_u32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

fn put_u64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

fn put_u48(b: &mut [u8], off: usize, v: u64) {
    // 值域是**编码方的责任**：超出 48 位在此截断，而 decode 侧会以
    // `OutOfDomain` 拒绝——变成"写得进、读不回"。所有现存调用方的值都
    // 远在域内；这条断言让将来的误用当场暴露（debug 构建）。
    debug_assert!(v <= SEQ_MAX, "put_u48 值超出 48 位域");
    b[off..off + 6].copy_from_slice(&v.to_le_bytes()[..6]);
}

// ---------------------------------------------------------------------------
// 页（页头 24B + 页体 + 页尾校验）
// ---------------------------------------------------------------------------

/// 页尾校验：CRC32C **覆盖全页、校验字段按零参与**（与数据页同规）。
#[must_use]
pub fn page_crc(page: &[u8; PAGE_SIZE]) -> u32 {
    checksum_with_zeroed_field(page, PAGE_SIZE - CF_PAGE_TRAILER_LEN, CF_PAGE_TRAILER_LEN)
}

fn seal_page(page: &mut [u8; PAGE_SIZE]) {
    let crc = page_crc(page);
    page[PAGE_SIZE - CF_PAGE_TRAILER_LEN..].copy_from_slice(&crc.to_le_bytes());
}

fn page_body(page: &[u8; PAGE_SIZE]) -> &[u8] {
    &page[CF_PAGE_HEADER_LEN..CF_PAGE_HEADER_LEN + CF_PAGE_BODY_LEN]
}

fn page_body_mut(page: &mut [u8; PAGE_SIZE]) -> &mut [u8] {
    &mut page[CF_PAGE_HEADER_LEN..CF_PAGE_HEADER_LEN + CF_PAGE_BODY_LEN]
}

/// 校验一页的结构：magic、副本号、页号、页尾校验。
fn validate_page(page: &[u8; PAGE_SIZE], copy: u8, page_no: u8) -> Result<(), &'static str> {
    if page[0..4] != CF_MAGIC {
        return Err("magic 不符（非控制文件页）");
    }
    if page[12] != copy {
        return Err("副本号不符");
    }
    if get_u16(page, 8) != u16::from(page_no) {
        return Err("页号不符");
    }
    if get_u32(page, PAGE_SIZE - CF_PAGE_TRAILER_LEN) != page_crc(page) {
        return Err("页尾校验和不符");
    }
    Ok(())
}

/// 新建一页（页体全零；`seq` 仅页 0 使用，其余页恒 0）。
fn blank_page(copy: u8, page_no: u8, payload_len: u16) -> Box<[u8; PAGE_SIZE]> {
    let mut page = Box::new([0u8; PAGE_SIZE]);
    page[0..4].copy_from_slice(&CF_MAGIC);
    put_u16(page.as_mut_slice(), 8, u16::from(page_no));
    put_u16(page.as_mut_slice(), 10, payload_len);
    page[12] = copy;
    seal_page(&mut page);
    page
}

// ---------------------------------------------------------------------------
// undo 记录（页 0 页体内，定长 522 B）
// ---------------------------------------------------------------------------

/// 解析出的 undo 记录。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Undo {
    target_off: u32,
    old_len: u16,
    old_data: [u8; UNDO_OLD_MAX],
}

impl Undo {
    /// 空闲（`old_len = 0`）。
    const FREE: Undo = Undo {
        target_off: 0,
        old_len: 0,
        old_data: [0u8; UNDO_OLD_MAX],
    };
}

/// 读 undo 记录；`old_len != 0` 时校验和必须通过。
fn read_undo(page0: &[u8; PAGE_SIZE]) -> Result<Undo, &'static str> {
    let base = CF_PAGE_HEADER_LEN + UNDO_RECORD_OFFSET;
    let rec = &page0[base..base + UNDO_RECORD_LEN];
    let target_off = get_u32(rec, 0);
    let old_len = get_u16(rec, 4);
    if usize::from(old_len) > UNDO_OLD_MAX {
        return Err("undo 旧值长度越界");
    }
    if old_len == 0 {
        return Ok(Undo::FREE);
    }
    let stored = get_u32(rec, UNDO_RECORD_LEN - 4);
    if stored != crc32c(&rec[..UNDO_RECORD_LEN - 4]) {
        return Err("undo 记录校验和不符");
    }
    let mut old_data = [0u8; UNDO_OLD_MAX];
    old_data[..usize::from(old_len)].copy_from_slice(&rec[6..6 + usize::from(old_len)]);
    // 目标区间必须落在文件内、且**不跨页、不侵入页头/页尾**（与写入侧同规）。
    let start = target_off as usize;
    let end = start + usize::from(old_len);
    let page_start = start / PAGE_SIZE * PAGE_SIZE;
    if end > CF_SIZE || end - page_start > PAGE_SIZE - CF_PAGE_TRAILER_LEN {
        return Err("undo 目标区间越出页体");
    }
    if start - page_start < CF_PAGE_HEADER_LEN {
        return Err("undo 目标区间越出页体");
    }
    Ok(Undo {
        target_off,
        old_len,
        old_data,
    })
}

/// 写入 undo 记录（含校验和）。
fn write_undo(page0: &mut [u8; PAGE_SIZE], target_off: u32, old: &[u8]) {
    debug_assert!(old.len() <= UNDO_OLD_MAX);
    let base = CF_PAGE_HEADER_LEN + UNDO_RECORD_OFFSET;
    let rec = &mut page0[base..base + UNDO_RECORD_LEN];
    rec.fill(0);
    put_u32(rec, 0, target_off);
    put_u16(rec, 4, old.len() as u16);
    rec[6..6 + old.len()].copy_from_slice(old);
    let crc = crc32c(&rec[..UNDO_RECORD_LEN - 4]);
    put_u32(rec, UNDO_RECORD_LEN - 4, crc);
}

/// 清 undo 记录（整条归零；`old_len = 0` 即空闲）。
fn clear_undo(page0: &mut [u8; PAGE_SIZE]) {
    let base = CF_PAGE_HEADER_LEN + UNDO_RECORD_OFFSET;
    page0[base..base + UNDO_RECORD_LEN].fill(0);
}

// ---------------------------------------------------------------------------
// 状态与记录（页 1 固定段 / 数据文件记录）
// ---------------------------------------------------------------------------

/// redo 组的**运行状态**（§11.9 的运行轴）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LogRunState {
    /// 0：从未使用（或新增未用；序列号为 0）。
    Unused = 0,
    /// 1：正在写（全实例至多一组）。
    Current = 1,
    /// 2：已写满，检查点尚未越过——崩溃恢复仍需要，不可复用。
    Active = 2,
    /// 3：检查点已越过——运行轴上可复用。
    Inactive = 3,
}

impl LogRunState {
    /// 磁盘取值。
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// 由磁盘取值解码。
    #[must_use]
    pub const fn from_u8(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(Self::Unused),
            1 => Some(Self::Current),
            2 => Some(Self::Active),
            3 => Some(Self::Inactive),
            _ => None,
        }
    }
}

/// redo 组的**归档状态**（§11.9 的归档轴）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LogArchiveState {
    /// 0：无需归档（非归档模式；或未写满即被弃用）。
    None = 0,
    /// 1：待归档。
    Needed = 1,
    /// 2：归档中。
    InFlight = 2,
    /// 3：已归档。
    Done = 3,
}

impl LogArchiveState {
    /// 磁盘取值。
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// 由磁盘取值解码。
    #[must_use]
    pub const fn from_u8(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(Self::None),
            1 => Some(Self::Needed),
            2 => Some(Self::InFlight),
            3 => Some(Self::Done),
            _ => None,
        }
    }
}

/// 一个 redo 组的条目（12 B）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RedoGroup {
    /// 日志序列号（每次被选为 `CURRENT` 时 = 上一个 + 1；`UNUSED` 恒 0）。
    pub sequence: u32,
    /// 运行状态。
    pub run: LogRunState,
    /// 归档状态。
    pub archive: LogArchiveState,
    /// **成员镜像状态位**（bit k = 成员 k 标 `STALE`；0 = 全部健康）。
    /// 落在条目的 6B 保留区第 1 字节（2026-10-05 定案：v1 成员数 ≤ 8）。
    pub member_stale: u8,
}

impl RedoGroup {
    /// 从未使用的组。
    pub const UNUSED: RedoGroup = RedoGroup {
        sequence: 0,
        run: LogRunState::Unused,
        archive: LogArchiveState::None,
        member_stale: 0,
    };
}

/// Redo 条目（页 1 固定段，128 B）：头 + 8 组。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RedoEntries {
    /// 组数（1..=8）。
    pub group_count: u8,
    /// 成员数（1..=8；V1.0 默认 2）。
    pub member_count: u8,
    /// 当前组（[`NO_CURRENT_GROUP`] = 尚无）。
    pub current_group: u8,
    /// 每组条目。
    pub groups: [RedoGroup; MAX_REDO_GROUPS],
}

impl RedoEntries {
    /// 新建：全部组 `UNUSED`、无当前组。
    pub fn new(group_count: u8, member_count: u8) -> Result<Self, ControlFileError> {
        let entries = Self {
            group_count,
            member_count,
            current_group: NO_CURRENT_GROUP,
            groups: [RedoGroup::UNUSED; MAX_REDO_GROUPS],
        };
        entries.validate()?;
        Ok(entries)
    }

    /// 自洽校验（**发布前拒绝不自洽的状态**）：
    /// 组数/成员数在界内；至多一个 `CURRENT` 且 `current_group` 与之互指；
    /// `UNUSED` 组序列号为 0。
    pub fn validate(&self) -> Result<(), ControlFileError> {
        if self.group_count == 0 || usize::from(self.group_count) > MAX_REDO_GROUPS {
            return Err(ControlFileError::InconsistentRedo {
                reason: "组数应在 1..=8",
            });
        }
        if self.member_count == 0 || usize::from(self.member_count) > MAX_REDO_GROUPS {
            return Err(ControlFileError::InconsistentRedo {
                reason: "成员数应在 1..=8",
            });
        }
        let member_mask: u8 = if self.member_count >= 8 {
            0xFF
        } else {
            (1u8 << self.member_count) - 1
        };
        let mut current_seen = None;
        for (i, g) in self
            .groups
            .iter()
            .enumerate()
            .take(usize::from(self.group_count))
        {
            if g.member_stale & !member_mask != 0 {
                return Err(ControlFileError::InconsistentRedo {
                    reason: "成员镜像位超出成员数",
                });
            }
            match g.run {
                LogRunState::Unused => {
                    if g.sequence != 0 {
                        return Err(ControlFileError::InconsistentRedo {
                            reason: "UNUSED 组序列号应为 0",
                        });
                    }
                }
                LogRunState::Current => {
                    if current_seen.is_some() {
                        return Err(ControlFileError::InconsistentRedo {
                            reason: "至多一个 CURRENT 组",
                        });
                    }
                    current_seen = Some(i as u8);
                }
                LogRunState::Active | LogRunState::Inactive => {}
            }
        }
        if self.current_group == NO_CURRENT_GROUP {
            if current_seen.is_some() {
                return Err(ControlFileError::InconsistentRedo {
                    reason: "存在 CURRENT 组时 current_group 必须指向它",
                });
            }
        } else {
            if usize::from(self.current_group) >= usize::from(self.group_count) {
                return Err(ControlFileError::InconsistentRedo {
                    reason: "current_group 超出组数",
                });
            }
            if current_seen != Some(self.current_group) {
                return Err(ControlFileError::InconsistentRedo {
                    reason: "current_group 与 CURRENT 组不符",
                });
            }
        }
        Ok(())
    }

    /// 编码到 128 B。
    pub fn encode(&self, out: &mut [u8]) {
        debug_assert_eq!(out.len(), REDO_ENTRIES_LEN);
        out.fill(0);
        out[0] = self.group_count;
        out[1] = self.member_count;
        out[2] = self.current_group;
        for (i, g) in self.groups.iter().enumerate() {
            let off = 4 + i * 12;
            put_u32(out, off, g.sequence);
            out[off + 4] = g.run.as_u8();
            out[off + 5] = g.archive.as_u8();
            out[off + 6] = g.member_stale; // 6B 保留区的第 1 字节：成员镜像位
        }
    }

    /// 由 128 B 解码（未知状态值 ⇒ 损坏）。
    pub fn decode(b: &[u8]) -> Result<Self, ControlFileError> {
        debug_assert_eq!(b.len(), REDO_ENTRIES_LEN);
        let mut groups = [RedoGroup::UNUSED; MAX_REDO_GROUPS];
        for (i, g) in groups.iter_mut().enumerate() {
            let off = 4 + i * 12;
            let run = LogRunState::from_u8(b[off + 4]).ok_or(ControlFileError::OutOfDomain {
                field: "redo 运行状态",
            })?;
            let archive =
                LogArchiveState::from_u8(b[off + 5]).ok_or(ControlFileError::OutOfDomain {
                    field: "redo 归档状态",
                })?;
            *g = RedoGroup {
                sequence: get_u32(b, off),
                run,
                archive,
                member_stale: b[off + 6],
            };
        }
        Ok(Self {
            group_count: b[0],
            member_count: b[1],
            current_group: b[2],
            groups,
        })
    }
}

/// 检查点进度（页 1 固定段，48 B）。
///
/// "检查点 LSN"就是**低水位**：该 LSN 之前的修改已全部落盘（§11.7）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointProgress {
    /// 检查点提交序号。
    pub checkpoint_commit_seq: CommitSeq,
    /// 检查点 LSN（= 恢复起点）。
    pub checkpoint_lsn: Lsn,
    /// 当前提交序号。
    pub current_commit_seq: CommitSeq,
    /// 最老快照提交序号。
    pub oldest_snapshot_commit_seq: CommitSeq,
    /// 墙钟时戳（标量，用于 PITR 采样对；不作先后判定）。
    pub timestamp: u64,
}

impl Default for CheckpointProgress {
    fn default() -> Self {
        let zero_seq = CommitSeq::from_raw(0).expect("0 在 48 位域内");
        Self {
            checkpoint_commit_seq: zero_seq,
            checkpoint_lsn: Lsn::from_raw(0).expect("0 在 48 位域内"),
            current_commit_seq: zero_seq,
            oldest_snapshot_commit_seq: zero_seq,
            timestamp: 0,
        }
    }
}

impl CheckpointProgress {
    /// 编码到 48 B。
    pub fn encode(&self, out: &mut [u8]) {
        debug_assert_eq!(out.len(), CHECKPOINT_PROGRESS_LEN);
        out.fill(0);
        put_u48(out, 0, self.checkpoint_commit_seq.as_raw());
        put_u48(out, 6, self.checkpoint_lsn.as_raw());
        put_u48(out, 12, self.current_commit_seq.as_raw());
        put_u48(out, 18, self.oldest_snapshot_commit_seq.as_raw());
        put_u64(out, 24, self.timestamp);
    }

    /// 由 48 B 解码（越出 48 位域 ⇒ 损坏）。
    pub fn decode(b: &[u8]) -> Result<Self, ControlFileError> {
        debug_assert_eq!(b.len(), CHECKPOINT_PROGRESS_LEN);
        let seq = |off: usize, field: &'static str| {
            CommitSeq::from_raw(get_u48(b, off)).ok_or(ControlFileError::OutOfDomain { field })
        };
        Ok(Self {
            checkpoint_commit_seq: seq(0, "检查点提交序号")?,
            checkpoint_lsn: Lsn::from_raw(get_u48(b, 6)).ok_or(ControlFileError::OutOfDomain {
                field: "检查点 LSN",
            })?,
            current_commit_seq: seq(12, "当前提交序号")?,
            oldest_snapshot_commit_seq: seq(18, "最老快照提交序号")?,
            timestamp: get_u64(b, 24),
        })
    }
}

/// 采样环头：已写对数（≤256）与下一个写入槽。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SampleHead {
    /// 已写入的对数（到达 256 后恒 256）。
    pub count: u16,
    /// 下一个写入槽（环）。
    pub next: u16,
}

impl SampleHead {
    fn encode(&self, out: &mut [u8]) {
        out[0..2].copy_from_slice(&self.count.to_le_bytes());
        out[2..4].copy_from_slice(&self.next.to_le_bytes());
    }
    fn decode(b: &[u8]) -> Self {
        Self {
            count: u16::from_le_bytes([b[0], b[1]]),
            next: u16::from_le_bytes([b[2], b[3]]),
        }
    }
}

/// 工作区条目（页 1 固定段，64 B）：身份与克隆血缘（§2.7）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceEntry {
    /// 工作区标识（48 位；0 保留）。
    pub workspace_id: WorkspaceId,
    /// 创建时间（墙钟时戳，标量）。
    pub created_at: u64,
    /// 克隆血缘：源工作区（`None` = 非克隆）。
    pub derived_from: Option<WorkspaceId>,
    /// 克隆时的源提交序号（§2.7 的起点规则）。
    pub derived_at_seq: CommitSeq,
}

impl WorkspaceEntry {
    /// 编码到 64 B。
    pub fn encode(&self, out: &mut [u8]) {
        debug_assert_eq!(out.len(), WORKSPACE_ENTRY_LEN);
        out.fill(0);
        put_u48(out, 0, self.workspace_id.as_raw());
        put_u64(out, 6, self.created_at);
        put_u48(out, 14, self.derived_from.map_or(0, WorkspaceId::as_raw));
        put_u48(out, 20, self.derived_at_seq.as_raw());
    }

    /// 由 64 B 解码。
    pub fn decode(b: &[u8]) -> Result<Self, ControlFileError> {
        debug_assert_eq!(b.len(), WORKSPACE_ENTRY_LEN);
        let workspace_id =
            WorkspaceId::from_raw(get_u48(b, 0)).ok_or(ControlFileError::OutOfDomain {
                field: "workspace_id",
            })?;
        let raw_from = get_u48(b, 14);
        let derived_from = if raw_from == 0 {
            None
        } else {
            Some(
                WorkspaceId::from_raw(raw_from).ok_or(ControlFileError::OutOfDomain {
                    field: "derived_from",
                })?,
            )
        };
        Ok(Self {
            workspace_id,
            created_at: get_u64(b, 6),
            derived_from,
            derived_at_seq: CommitSeq::from_raw(get_u48(b, 20)).ok_or(
                ControlFileError::OutOfDomain {
                    field: "derived_at_seq",
                },
            )?,
        })
    }
}

/// 归档模式（页 1 归档记录首字节）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveMode {
    /// 0：非归档模式。
    NoArchive = 0,
    /// 1：归档模式（创建工作区时指定；**默认开启**，§11.9）。
    ArchiveLog = 1,
}

impl ArchiveMode {
    /// 磁盘取值。
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// 由磁盘取值解码。
    #[must_use]
    pub const fn from_u8(raw: u8) -> Option<Self> {
        match raw {
            0 => Some(Self::NoArchive),
            1 => Some(Self::ArchiveLog),
            _ => None,
        }
    }
}

/// 归档记录（页 1 固定段，320 B）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveRecord {
    /// 归档模式。
    pub mode: ArchiveMode,
    /// 标志位（暂未使用，保持 0）。
    pub flags: u8,
    /// 最后归档的日志序列号。
    pub last_archived_sequence: u32,
    target_path: [u8; DATA_FILE_PATH_LEN],
}

impl Default for ArchiveRecord {
    /// 默认 = 归档模式开启（§11.9 的默认值），目标路径为空（由建工作区时落定）。
    fn default() -> Self {
        Self::new(ArchiveMode::ArchiveLog)
    }
}

impl ArchiveRecord {
    /// 新建（目标路径为空）。
    #[must_use]
    pub fn new(mode: ArchiveMode) -> Self {
        Self {
            mode,
            flags: 0,
            last_archived_sequence: 0,
            target_path: [0u8; DATA_FILE_PATH_LEN],
        }
    }

    /// 归档目标路径（按首个 NUL 截断）。
    #[must_use]
    pub fn target_path(&self) -> &[u8] {
        read_fixed_path(&self.target_path)
    }

    /// 设置归档目标路径（≤ 255 字节、不得含内嵌 NUL）。
    pub fn set_target_path(&mut self, path: &[u8]) -> Result<(), ControlFileError> {
        set_fixed_path(&mut self.target_path, path)
    }

    /// 编码到 320 B。
    pub fn encode(&self, out: &mut [u8]) {
        debug_assert_eq!(out.len(), ARCHIVE_RECORD_LEN);
        out.fill(0);
        out[0] = self.mode.as_u8();
        out[1] = self.flags;
        put_u32(out, 2, self.last_archived_sequence);
        out[6..6 + DATA_FILE_PATH_LEN].copy_from_slice(&self.target_path);
    }

    /// 由 320 B 解码。
    pub fn decode(b: &[u8]) -> Result<Self, ControlFileError> {
        debug_assert_eq!(b.len(), ARCHIVE_RECORD_LEN);
        let mut target_path = [0u8; DATA_FILE_PATH_LEN];
        target_path.copy_from_slice(&b[6..6 + DATA_FILE_PATH_LEN]);
        Ok(Self {
            mode: ArchiveMode::from_u8(b[0]).ok_or(ControlFileError::OutOfDomain {
                field: "归档模式",
            })?,
            flags: b[1],
            last_archived_sequence: get_u32(b, 2),
            target_path,
        })
    }
}

/// 数据文件记录（页 2–19 的记录数组，280 B/条）。
///
/// `status = 0` 表示**槽位空闲**（记录未使用）；其余 status/role 取值由
/// 段管理切片定义（§4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataFileRecord {
    /// 文件号（与 ROWID 的 `file_id` 10 位对齐）。
    pub file_id: u16,
    /// 文件角色（0 元数据 / 1 Undo / 2 临时 / 3.. 数据，§2.2）。
    pub role: u8,
    /// 状态（0 = 空闲槽位）。
    pub status: u8,
    /// 标志位。
    pub flags: u16,
    /// **创建时大小**（块数，48 位；当前大小以文件头自述为准）。
    pub creation_blocks: u64,
    /// 创建时间（墙钟时戳，标量）。
    pub created_at: u64,
    path: [u8; DATA_FILE_PATH_LEN],
}

impl DataFileRecord {
    /// 新建（路径为空、创建时大小为 0）。
    #[must_use]
    pub fn new(file_id: u16, role: u8) -> Self {
        Self {
            file_id,
            role,
            status: 0,
            flags: 0,
            creation_blocks: 0,
            created_at: 0,
            path: [0u8; DATA_FILE_PATH_LEN],
        }
    }

    /// 完整路径（按首个 NUL 截断）。
    #[must_use]
    pub fn path(&self) -> &[u8] {
        read_fixed_path(&self.path)
    }

    /// 设置完整路径（≤ 255 字节、不得含内嵌 NUL）。
    pub fn set_path(&mut self, path: &[u8]) -> Result<(), ControlFileError> {
        set_fixed_path(&mut self.path, path)
    }

    /// 编码到 280 B。
    pub fn encode(&self, out: &mut [u8]) {
        debug_assert_eq!(out.len(), DATA_FILE_RECORD_LEN);
        out.fill(0);
        put_u16(out, 0, self.file_id);
        out[2] = self.role;
        out[3] = self.status;
        put_u16(out, 4, self.flags);
        debug_assert!(self.creation_blocks <= SEQ_MAX, "创建大小超出 48 位编码域");
        put_u48(out, 6, self.creation_blocks);
        put_u64(out, 12, self.created_at);
        out[24..24 + DATA_FILE_PATH_LEN].copy_from_slice(&self.path);
    }

    /// 由 280 B 解码。
    pub fn decode(b: &[u8]) -> Result<Self, ControlFileError> {
        debug_assert_eq!(b.len(), DATA_FILE_RECORD_LEN);
        if get_u48(b, 6) > SEQ_MAX {
            return Err(ControlFileError::OutOfDomain {
                field: "文件创建时大小",
            });
        }
        let mut path = [0u8; DATA_FILE_PATH_LEN];
        path.copy_from_slice(&b[24..24 + DATA_FILE_PATH_LEN]);
        Ok(Self {
            file_id: get_u16(b, 0),
            role: b[2],
            status: b[3],
            flags: get_u16(b, 4),
            creation_blocks: get_u48(b, 6),
            created_at: get_u64(b, 12),
            path,
        })
    }
}

fn read_fixed_path(field: &[u8; DATA_FILE_PATH_LEN]) -> &[u8] {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    &field[..end]
}

fn set_fixed_path(
    field: &mut [u8; DATA_FILE_PATH_LEN],
    path: &[u8],
) -> Result<(), ControlFileError> {
    if path.len() >= DATA_FILE_PATH_LEN {
        return Err(ControlFileError::PathTooLong { len: path.len() });
    }
    if path.contains(&0) {
        return Err(ControlFileError::PathContainsNul);
    }
    field.fill(0);
    field[..path.len()].copy_from_slice(path);
    Ok(())
}

// ---------------------------------------------------------------------------
// 布局换算
// ---------------------------------------------------------------------------

/// 数据文件记录下标 →（页号，页体内偏移）；越界返回 `None`。
#[must_use]
pub const fn data_record_location(index: usize) -> Option<(u8, usize)> {
    if index >= MAX_DATA_FILE_RECORDS {
        return None;
    }
    let page = 2 + index / DATA_FILE_RECORDS_PER_PAGE;
    let off = (index % DATA_FILE_RECORDS_PER_PAGE) * DATA_FILE_RECORD_LEN;
    Some((page as u8, off))
}

/// 记录数组页的 `payload_len`（最后页不足一整页）。
const fn data_page_payload(page_no: usize) -> u16 {
    let before = (page_no - 2) * DATA_FILE_RECORDS_PER_PAGE;
    let rest = MAX_DATA_FILE_RECORDS.saturating_sub(before);
    let n = if rest < DATA_FILE_RECORDS_PER_PAGE {
        rest
    } else {
        DATA_FILE_RECORDS_PER_PAGE
    };
    (n * DATA_FILE_RECORD_LEN) as u16
}

/// 段偏移表项编码（16 B）。
fn segment_entry(seg: u8, start_page: u16, pages: u16, item_size: u16, count: u16) -> [u8; 16] {
    let mut e = [0u8; SEGMENT_ITEM_LEN];
    e[0] = seg;
    put_u16(&mut e, 1, start_page);
    put_u16(&mut e, 3, pages);
    put_u16(&mut e, 5, item_size);
    put_u16(&mut e, 7, count);
    e
}

/// 页内固定字段的**文件偏移**（更新协议的目标偏移）。
const fn file_offset(page_no: usize, body_off: usize) -> u32 {
    (page_no * PAGE_SIZE + CF_PAGE_HEADER_LEN + body_off) as u32
}

// ---------------------------------------------------------------------------
// 控制文件
// ---------------------------------------------------------------------------

/// 打开（或新建）后的控制文件：绑定一个 [`FileIo`] 与两个副本句柄。
pub struct ControlFile<'a> {
    io: &'a dyn FileIo,
    handles: [FileHandle; CF_COPIES],
    /// 当前生效副本（0 = A、1 = B）：读取与诊断以它为准。
    active: usize,
    /// 各副本的页 0 `seq`（内存镜像；发布成功即更新）。
    seqs: [u32; CF_COPIES],
}

impl<'a> ControlFile<'a> {
    /// 新建控制文件（两副本），写入初始内容：工作区条目、空检查点进度、
    /// 给定的 Redo 条目（新建时全 `UNUSED`）与归档记录。
    ///
    /// **不做隐式 fsync**——持久性点由调用方用 [`ControlFile::sync`] 表达。
    pub fn format(
        io: &'a dyn FileIo,
        path_a: &Path,
        path_b: &Path,
        workspace: &WorkspaceEntry,
        redo: &RedoEntries,
        archive: &ArchiveRecord,
    ) -> Result<Self, ControlFileError> {
        redo.validate()?;
        let handles = [create_file(io, path_a)?, create_file(io, path_b)?];
        let mut cf = Self {
            io,
            handles,
            active: 0,
            seqs: [0; CF_COPIES],
        };
        for copy in 0..CF_COPIES {
            cf.write_initial_pages(copy, workspace, redo, archive)?;
        }
        Ok(cf)
    }

    /// 打开既有控制文件：逐副本校验 + **undo 回滚**，取"有效且 `seq` 较大"者；
    /// 另一副本损坏/落后时由有效副本**整份重建**；两份都不可用 ⇒ 拒绝启动。
    pub fn open(
        io: &'a dyn FileIo,
        path_a: &Path,
        path_b: &Path,
    ) -> Result<Self, ControlFileError> {
        let opts = OpenOptions::new().read(true).write(true);
        let handles = [io.open(path_a, opts)?, io.open(path_b, opts)?];
        let mut seqs = [0u32; CF_COPIES];
        let mut fails: [Option<String>; CF_COPIES] = [None, None];
        for copy in 0..CF_COPIES {
            match heal_copy(io, handles[copy], copy as u8) {
                Ok(seq) => seqs[copy] = seq,
                Err(reason) => fails[copy] = Some(reason),
            }
        }
        let active = match (&fails[0], &fails[1]) {
            (None, None) => usize::from(seqs[1] > seqs[0]),
            (None, Some(_)) => {
                rebuild_copy(io, handles[0], 0, handles[1], 1)?;
                seqs[1] = seqs[0];
                0
            }
            (Some(_), None) => {
                rebuild_copy(io, handles[1], 1, handles[0], 0)?;
                seqs[0] = seqs[1];
                1
            }
            (Some(a), Some(b)) => {
                return Err(ControlFileError::BothCopiesInvalid {
                    a: a.clone(),
                    b: b.clone(),
                })
            }
        };
        Ok(Self {
            io,
            handles,
            active,
            seqs,
        })
    }

    /// 当前生效副本的页 0 `seq`（副本内更新序号；每次更新 +1）。
    #[must_use]
    pub fn sequence(&self) -> u32 {
        self.seqs[self.active]
    }

    /// 当前生效副本号（0 = A、1 = B）。
    #[must_use]
    pub fn active_copy(&self) -> u8 {
        self.active as u8
    }

    /// 持久性点：两副本 `fdatasync`。
    pub fn sync(&self) -> Result<(), ControlFileError> {
        for h in self.handles {
            self.io.sync_data(h)?;
        }
        Ok(())
    }

    /// 关闭两副本句柄。
    pub fn close(self) -> Result<(), ControlFileError> {
        for h in self.handles {
            self.io.close(h)?;
        }
        Ok(())
    }

    // -- 读取（以生效副本为准）------------------------------------------------

    /// 工作区条目。
    pub fn workspace_entry(&self) -> Result<WorkspaceEntry, ControlFileError> {
        WorkspaceEntry::decode(&self.read_fixed::<WORKSPACE_ENTRY_LEN>(1, WORKSPACE_ENTRY_OFFSET)?)
    }

    /// 检查点进度。
    pub fn checkpoint_progress(&self) -> Result<CheckpointProgress, ControlFileError> {
        CheckpointProgress::decode(
            &self.read_fixed::<CHECKPOINT_PROGRESS_LEN>(1, CHECKPOINT_PROGRESS_OFFSET)?,
        )
    }

    /// Redo 条目。
    pub fn redo_entries(&self) -> Result<RedoEntries, ControlFileError> {
        RedoEntries::decode(&self.read_fixed::<REDO_ENTRIES_LEN>(1, REDO_ENTRIES_OFFSET)?)
    }

    /// 归档记录。
    /// **读采样环头**。
    pub fn sample_head(&self) -> Result<SampleHead, ControlFileError> {
        let b = self.read_fixed::<4>(1, SAMPLE_HEAD_OFFSET)?;
        Ok(SampleHead::decode(&b))
    }

    /// **读全部采样对**（按写入顺序：最老 → 最新）。
    pub fn sample_pairs(&self) -> Result<Vec<(CommitSeq, u64)>, ControlFileError> {
        let head = self.sample_head()?;
        let page = self.read_page(1, 1)?;
        let mut out = Vec::with_capacity(usize::from(head.count));
        let n = usize::from(head.count).min(SAMPLE_PAIRS);
        // 环：count = 256 时的起点 = next（最老）；否则从 0 起。
        let start = if n == SAMPLE_PAIRS {
            head.next as usize % SAMPLE_PAIRS
        } else {
            0
        };
        for k in 0..n {
            let idx = (start + k) % SAMPLE_PAIRS;
            let off = CF_PAGE_HEADER_LEN + SAMPLE_PAIRS_OFFSET + idx * SAMPLE_PAIR_LEN;
            let seq = get_u48(page.as_ref(), off);
            let ts = get_u48(page.as_ref(), off + 6);
            out.push((
                CommitSeq::from_raw(seq).ok_or(ControlFileError::OutOfDomain {
                    field: "采样对提交序号",
                })?,
                ts,
            ));
        }
        Ok(out)
    }

    /// **追加一对采样**（环写；头也随之更新——两次小更新，各在 512B 内）。
    pub fn append_sample_pair(
        &mut self,
        seq: CommitSeq,
        timestamp_ms: u64,
    ) -> Result<(), ControlFileError> {
        let mut head = self.sample_head()?;
        let idx = usize::from(head.next) % SAMPLE_PAIRS;
        let mut buf = [0u8; SAMPLE_PAIR_LEN];
        put_u48(&mut buf, 0, seq.as_raw());
        put_u48(&mut buf, 6, timestamp_ms & 0x0000_FFFF_FFFF_FFFF);
        self.update_interval(
            file_offset(1, SAMPLE_PAIRS_OFFSET + idx * SAMPLE_PAIR_LEN),
            &buf,
        )?;
        head.count = head.count.saturating_add(1).min(SAMPLE_PAIRS as u16);
        head.next = ((idx + 1) % SAMPLE_PAIRS) as u16;
        let mut hb = [0u8; 4];
        head.encode(&mut hb);
        self.update_interval(file_offset(1, SAMPLE_HEAD_OFFSET), &hb)?;
        Ok(())
    }

    /// 归档记录。
    pub fn archive_record(&self) -> Result<ArchiveRecord, ControlFileError> {
        ArchiveRecord::decode(&self.read_fixed::<ARCHIVE_RECORD_LEN>(1, ARCHIVE_RECORD_OFFSET)?)
    }

    /// 数据文件记录（按数组下标）。
    pub fn data_file_record(&self, index: usize) -> Result<DataFileRecord, ControlFileError> {
        let (page_no, off) =
            data_record_location(index).ok_or(ControlFileError::RecordIndexOutOfRange { index })?;
        DataFileRecord::decode(&self.read_fixed::<DATA_FILE_RECORD_LEN>(page_no, off)?)
    }

    // -- 发布（单区间更新协议；两副本依次）------------------------------------

    /// 发布工作区条目（64 B 区间）。
    pub fn write_workspace_entry(
        &mut self,
        entry: &WorkspaceEntry,
    ) -> Result<(), ControlFileError> {
        let mut buf = [0u8; WORKSPACE_ENTRY_LEN];
        entry.encode(&mut buf);
        self.update_interval(file_offset(1, WORKSPACE_ENTRY_OFFSET), &buf)
    }

    /// 发布检查点进度（48 B 区间）。
    pub fn write_checkpoint_progress(
        &mut self,
        progress: &CheckpointProgress,
    ) -> Result<(), ControlFileError> {
        let mut buf = [0u8; CHECKPOINT_PROGRESS_LEN];
        progress.encode(&mut buf);
        self.update_interval(file_offset(1, CHECKPOINT_PROGRESS_OFFSET), &buf)
    }

    /// 发布 Redo 条目（128 B 区间）。
    pub fn write_redo_entries(&mut self, redo: &RedoEntries) -> Result<(), ControlFileError> {
        redo.validate()?;
        let mut buf = [0u8; REDO_ENTRIES_LEN];
        redo.encode(&mut buf);
        self.update_interval(file_offset(1, REDO_ENTRIES_OFFSET), &buf)
    }

    /// 发布归档记录（320 B 区间）。
    pub fn write_archive_record(
        &mut self,
        archive: &ArchiveRecord,
    ) -> Result<(), ControlFileError> {
        let mut buf = [0u8; ARCHIVE_RECORD_LEN];
        archive.encode(&mut buf);
        self.update_interval(file_offset(1, ARCHIVE_RECORD_OFFSET), &buf)
    }

    /// 发布数据文件记录（280 B 区间）。
    pub fn write_data_file_record(
        &mut self,
        index: usize,
        record: &DataFileRecord,
    ) -> Result<(), ControlFileError> {
        let (page_no, off) =
            data_record_location(index).ok_or(ControlFileError::RecordIndexOutOfRange { index })?;
        let mut buf = [0u8; DATA_FILE_RECORD_LEN];
        record.encode(&mut buf);
        self.update_interval(file_offset(usize::from(page_no), off), &buf)
    }

    /// **联合发布**：检查点进度 + Redo 条目（§11.9 的"状态迁移算法"发布纪律）。
    ///
    /// 两个字段在页 1 上相邻（64..240，共 176 B ≤ 512 B），**同一次更新**发布：
    /// 降级（`ACTIVE` → `INACTIVE`）绝不能先于低水位独立落盘，否则崩溃后
    /// "可复用声明"会早于"恢复起点"，被复用的组会剪断日志链。
    pub fn write_checkpoint_and_groups(
        &mut self,
        progress: &CheckpointProgress,
        redo: &RedoEntries,
    ) -> Result<(), ControlFileError> {
        redo.validate()?;
        let mut buf = [0u8; CHECKPOINT_PROGRESS_LEN + REDO_ENTRIES_LEN];
        progress.encode(&mut buf[..CHECKPOINT_PROGRESS_LEN]);
        redo.encode(&mut buf[CHECKPOINT_PROGRESS_LEN..]);
        self.update_interval(file_offset(1, CHECKPOINT_PROGRESS_OFFSET), &buf)
    }

    // -- 内部 ---------------------------------------------------------------

    fn read_page(
        &self,
        copy: usize,
        page_no: u8,
    ) -> Result<Box<[u8; PAGE_SIZE]>, ControlFileError> {
        read_raw(self.io, self.handles[copy], copy as u8, page_no)
    }

    fn write_page(
        &self,
        copy: usize,
        page_no: u8,
        page: &mut [u8; PAGE_SIZE],
    ) -> Result<(), ControlFileError> {
        seal_page(page);
        self.io.write_at(
            self.handles[copy],
            page.as_slice(),
            u64::from(page_no) * PAGE_SIZE as u64,
        )?;
        Ok(())
    }

    fn read_fixed<const N: usize>(
        &self,
        page_no: u8,
        body_off: usize,
    ) -> Result<[u8; N], ControlFileError> {
        let page = self.read_page(self.active, page_no)?;
        let body = page_body(&page);
        let mut out = [0u8; N];
        out.copy_from_slice(&body[body_off..body_off + N]);
        Ok(out)
    }

    /// 单区间更新：两副本依次走 ①②③④（先 A 成功后 B）。
    fn update_interval(&mut self, target_off: u32, new: &[u8]) -> Result<(), ControlFileError> {
        if new.is_empty() {
            return Err(ControlFileError::IntervalOutOfBody);
        }
        if new.len() > MAX_UPDATE_LEN {
            return Err(ControlFileError::IntervalTooLarge { len: new.len() });
        }
        let start = target_off as usize;
        let end = start + new.len();
        let page_start = start / PAGE_SIZE * PAGE_SIZE;
        if end > CF_SIZE
            || end - page_start > PAGE_SIZE - CF_PAGE_TRAILER_LEN
            || start - page_start < CF_PAGE_HEADER_LEN
        {
            return Err(ControlFileError::IntervalOutOfBody);
        }
        for copy in 0..CF_COPIES {
            self.update_on_copy(copy, target_off, new)?;
        }
        Ok(())
    }

    /// 一个副本上的完整更新协议（§2.6）：
    /// ① undo（旧值）→ ② 目标字节 → ③ 页 0 `seq` +1（发布）→ ④ 清 undo。
    fn update_on_copy(
        &mut self,
        copy: usize,
        target_off: u32,
        new: &[u8],
    ) -> Result<(), ControlFileError> {
        let page_no = (target_off as usize / PAGE_SIZE) as u8;
        let in_off = target_off as usize % PAGE_SIZE;

        // ① 写 undo 记录（旧值）。
        let mut page0 = self.read_page(copy, 0)?;
        let old = {
            let target = self.read_page(copy, page_no)?;
            target[in_off..in_off + new.len()].to_vec()
        };
        write_undo(&mut page0, target_off, &old);
        self.write_page(copy, 0, &mut page0)?;

        // ② 写目标字节（原地覆盖）。
        let mut target = self.read_page(copy, page_no)?;
        target[in_off..in_off + new.len()].copy_from_slice(new);
        self.write_page(copy, page_no, &mut target)?;

        // ③ 发布：页 0 `seq` +1。
        let mut page0 = self.read_page(copy, 0)?;
        let seq = get_u32(page0.as_slice(), 4).wrapping_add(1);
        put_u32(page0.as_mut_slice(), 4, seq);
        self.write_page(copy, 0, &mut page0)?;
        self.seqs[copy] = seq;

        // ④ 清 undo 记录。
        let mut page0 = self.read_page(copy, 0)?;
        clear_undo(&mut page0);
        self.write_page(copy, 0, &mut page0)?;
        Ok(())
    }

    /// 新建时的整份写入（一个副本的全部 20 页）。
    fn write_initial_pages(
        &mut self,
        copy: usize,
        workspace: &WorkspaceEntry,
        redo: &RedoEntries,
        archive: &ArchiveRecord,
    ) -> Result<(), ControlFileError> {
        // 页 0：文件头（格式版本 + 段偏移表 + 空 undo）。
        let mut page0 = blank_page(
            copy as u8,
            0,
            (4 + SEGMENT_TABLE_ITEMS * SEGMENT_ITEM_LEN + UNDO_RECORD_LEN) as u16,
        );
        {
            let body = page_body_mut(&mut page0);
            put_u16(body, 0, CF_FORMAT_VERSION);
            put_u16(body, 2, CF_PAGE_SIZE_FIELD);
            let segments = [
                (0u8, 0u16, 1u16, 0u16, 0u16),
                (1, 1, 1, 0, 4),
                (
                    2,
                    2,
                    (CF_PAGES - 2) as u16,
                    DATA_FILE_RECORD_LEN as u16,
                    MAX_DATA_FILE_RECORDS as u16,
                ),
                (3, 0, 0, 0, 0),
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
        self.write_page(copy, 0, &mut page0)?;

        // 页 1：固定段。
        let mut page1 = blank_page(copy as u8, 1, FIXED_SEGMENT_LEN as u16);
        {
            let body = page_body_mut(&mut page1);
            workspace.encode(
                &mut body[WORKSPACE_ENTRY_OFFSET..WORKSPACE_ENTRY_OFFSET + WORKSPACE_ENTRY_LEN],
            );
            CheckpointProgress::default().encode(
                &mut body[CHECKPOINT_PROGRESS_OFFSET
                    ..CHECKPOINT_PROGRESS_OFFSET + CHECKPOINT_PROGRESS_LEN],
            );
            redo.encode(&mut body[REDO_ENTRIES_OFFSET..REDO_ENTRIES_OFFSET + REDO_ENTRIES_LEN]);
            archive.encode(
                &mut body[ARCHIVE_RECORD_OFFSET..ARCHIVE_RECORD_OFFSET + ARCHIVE_RECORD_LEN],
            );
        }
        self.write_page(copy, 1, &mut page1)?;

        // 页 2–19：数据文件记录数组（全零 = 全部空闲槽位）。
        for page_no in 2..CF_PAGES {
            let mut page = blank_page(copy as u8, page_no as u8, data_page_payload(page_no));
            self.write_page(copy, page_no as u8, &mut page)?;
        }
        Ok(())
    }
}

impl std::fmt::Debug for ControlFile<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlFile")
            .field("active", &copy_name(self.active as u8))
            .field("sequence", &self.sequence())
            .finish_non_exhaustive()
    }
}

/// 读并校验一页。
fn read_raw(
    io: &dyn FileIo,
    handle: FileHandle,
    copy: u8,
    page_no: u8,
) -> Result<Box<[u8; PAGE_SIZE]>, ControlFileError> {
    let mut buf = Box::new([0u8; PAGE_SIZE]);
    io.read_exact_at(
        handle,
        buf.as_mut_slice(),
        u64::from(page_no) * PAGE_SIZE as u64,
    )?;
    validate_page(&buf, copy, page_no).map_err(|reason| ControlFileError::Damaged {
        copy,
        page: page_no,
        reason,
    })?;
    Ok(buf)
}

/// 新建并预置长度。
fn create_file(io: &dyn FileIo, path: &Path) -> io::Result<FileHandle> {
    let handle = io.open(
        path,
        OpenOptions::new().read(true).write(true).create_new(true),
    )?;
    io.set_len(handle, CF_SIZE as u64)?;
    Ok(handle)
}

/// 打开一个副本：校验页 0 → 校验格式版本 → **undo 回滚**（若有）。
///
/// 返回该副本的 `seq`；任何一步失败 ⇒ `Err(原因)`（副本不可用）。
fn heal_copy(io: &dyn FileIo, handle: FileHandle, copy: u8) -> Result<u32, String> {
    let mut page0 = read_raw(io, handle, copy, 0).map_err(|e| e.to_string())?;
    let version = get_u16(page_body(&page0), 0);
    if version != CF_FORMAT_VERSION {
        return Err(format!("格式版本 {version} 不受支持"));
    }
    if get_u16(page_body(&page0), 2) != CF_PAGE_SIZE_FIELD {
        return Err("page_size 字段不符".to_string());
    }
    let undo = read_undo(&page0).map_err(str::to_string)?;
    if undo.old_len != 0 {
        // 回滚：先恢复目标字节，再清 undo（次序不能反——反了则崩溃窗口不安全）。
        let target_page = (undo.target_off as usize / PAGE_SIZE) as u8;
        let in_page = undo.target_off as usize % PAGE_SIZE;
        let len = usize::from(undo.old_len);
        let mut target = read_raw(io, handle, copy, target_page).map_err(|e| e.to_string())?;
        target[in_page..in_page + len].copy_from_slice(&undo.old_data[..len]);
        seal_page(&mut target);
        io.write_at(
            handle,
            target.as_slice(),
            u64::from(target_page) * PAGE_SIZE as u64,
        )
        .map_err(|e| e.to_string())?;
        clear_undo(&mut page0);
        seal_page(&mut page0);
        io.write_at(handle, page0.as_slice(), 0)
            .map_err(|e| e.to_string())?;
    }
    Ok(get_u32(page0.as_slice(), 4))
}

/// 由有效副本整份重建另一副本（仅改副本号字段并重算校验）。
fn rebuild_copy(
    io: &dyn FileIo,
    from: FileHandle,
    from_copy: u8,
    to: FileHandle,
    to_copy: u8,
) -> Result<(), ControlFileError> {
    for page_no in 0..CF_PAGES {
        let mut page = read_raw(io, from, from_copy, page_no as u8)?;
        page[12] = to_copy;
        seal_page(&mut page);
        io.write_at(to, page.as_slice(), page_no as u64 * PAGE_SIZE as u64)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::ErrorKind;

    use bicdb_workspace::io::{FaultInjecting, FaultOp, FaultRule, MemFileIo};

    use super::*;

    const A: &str = "/mem/control01.ctl";
    const B: &str = "/mem/control02.ctl";

    fn new_mem() -> MemFileIo {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        io
    }

    fn ws_entry() -> WorkspaceEntry {
        WorkspaceEntry {
            workspace_id: WorkspaceId::from_raw(7).unwrap(),
            created_at: 1_700_000_000_000,
            derived_from: None,
            derived_at_seq: CommitSeq::from_raw(0).unwrap(),
        }
    }

    fn redo_default() -> RedoEntries {
        RedoEntries::new(2, 2).unwrap()
    }

    fn archive_default() -> ArchiveRecord {
        let mut a = ArchiveRecord::default();
        a.set_target_path(b"/mnt/ws/arch").unwrap();
        a
    }

    fn progress(current: u64, lsn: u64) -> CheckpointProgress {
        CheckpointProgress {
            checkpoint_commit_seq: CommitSeq::from_raw(9).unwrap(),
            checkpoint_lsn: Lsn::from_raw(lsn).unwrap(),
            current_commit_seq: CommitSeq::from_raw(current).unwrap(),
            oldest_snapshot_commit_seq: CommitSeq::from_raw(0).unwrap(),
            timestamp: 42,
        }
    }

    fn format_cf<'a>(io: &'a dyn FileIo) -> ControlFile<'a> {
        ControlFile::format(
            io,
            Path::new(A),
            Path::new(B),
            &ws_entry(),
            &redo_default(),
            &archive_default(),
        )
        .unwrap()
    }

    fn reopen(io: &dyn FileIo) -> ControlFile<'_> {
        ControlFile::open(io, Path::new(A), Path::new(B)).unwrap()
    }

    fn open_rw(io: &dyn FileIo, path: &str) -> FileHandle {
        io.open(Path::new(path), OpenOptions::new().read(true).write(true))
            .unwrap()
    }

    /// 建好并关盘，再包上故障注入（计数从零开始）。
    fn format_then_wrap() -> FaultInjecting<MemFileIo> {
        let mem = new_mem();
        {
            let cf = format_cf(&mem);
            cf.sync().unwrap();
            cf.close().unwrap();
        }
        FaultInjecting::new(mem)
    }

    // -- 布局钉住 ------------------------------------------------------------

    #[test]
    fn layout_constants_are_pinned() {
        assert_eq!(CF_SIZE, 320 * 1024);
        assert_eq!(
            SEGMENT_TABLE_OFFSET + SEGMENT_TABLE_ITEMS * SEGMENT_ITEM_LEN,
            UNDO_RECORD_OFFSET
        );
        assert_eq!(UNDO_RECORD_OFFSET + UNDO_RECORD_LEN, 654);
        assert_eq!(FIXED_SEGMENT_LEN, 560);
        assert_eq!(
            CHECKPOINT_PROGRESS_OFFSET + CHECKPOINT_PROGRESS_LEN,
            REDO_ENTRIES_OFFSET
        );
        assert_eq!(
            REDO_ENTRIES_OFFSET + REDO_ENTRIES_LEN,
            ARCHIVE_RECORD_OFFSET
        );
        assert_eq!(
            ARCHIVE_RECORD_OFFSET + ARCHIVE_RECORD_LEN,
            FIXED_SEGMENT_LEN
        );
        // 记录数组：每页 58 条、容量精确覆盖 1024（17 页满 + 1 页 38 条）。
        assert_eq!(DATA_FILE_RECORDS_PER_PAGE, 58);
        assert_eq!(MAX_DATA_FILE_RECORDS - DATA_FILE_RECORDS_PER_PAGE * 17, 38);
        // 容量关系是编译期常量，故用 const 断言（放到常量块里检查）。
        const { assert!(DATA_FILE_RECORDS_PER_PAGE * (CF_PAGES - 2) >= MAX_DATA_FILE_RECORDS) };
        assert_eq!(data_page_payload(2), 58 * 280);
        assert_eq!(data_page_payload(19), 38 * 280);
        assert_eq!(data_record_location(0), Some((2, 0)));
        assert_eq!(data_record_location(1023), Some((19, 37 * 280)));
        assert_eq!(data_record_location(1024), None);
    }

    // -- 新建 / 重开 ---------------------------------------------------------

    #[test]
    fn format_writes_both_copies_and_reopens() {
        let io = new_mem();
        {
            let cf = format_cf(&io);
            assert_eq!(cf.sequence(), 0);
            assert_eq!(cf.workspace_entry().unwrap(), ws_entry());
            assert_eq!(
                cf.checkpoint_progress().unwrap(),
                CheckpointProgress::default()
            );
            assert_eq!(cf.archive_record().unwrap(), archive_default());
            let r = cf.redo_entries().unwrap();
            assert_eq!((r.group_count, r.member_count), (2, 2));
            assert_eq!(r.current_group, NO_CURRENT_GROUP);
            assert!(r
                .groups
                .iter()
                .all(|g| g.run == LogRunState::Unused && g.sequence == 0));
            cf.sync().unwrap();
            cf.close().unwrap();
        }
        // 两个文件的长度都 = 320 KiB。
        for path in [A, B] {
            let h = io
                .open(Path::new(path), OpenOptions::new().read(true))
                .unwrap();
            assert_eq!(io.size(h).unwrap(), CF_SIZE as u64);
        }
        let cf = reopen(&io);
        assert_eq!(cf.workspace_entry().unwrap(), ws_entry());
        assert_eq!(cf.sequence(), 0);
    }

    #[test]
    fn page0_and_page1_headers_are_pinned() {
        let io = new_mem();
        format_cf(&io);
        let h = open_rw(&io, A);

        let p0 = read_raw(&io, h, 0, 0).unwrap();
        assert_eq!(&p0[0..4], b"BICF", "magic 按字符字节序落盘");
        assert_eq!(get_u16(p0.as_slice(), 8), 0, "页号");
        assert_eq!(get_u16(p0.as_slice(), 10), 654, "页 0 payload_len");
        assert_eq!(p0[12], 0, "副本号 A");
        assert_eq!(get_u32(p0.as_slice(), 4), 0, "初始 seq = 0");
        let body = page_body(&p0);
        assert_eq!(get_u16(body, 0), CF_FORMAT_VERSION);
        assert_eq!(get_u16(body, 2), CF_PAGE_SIZE_FIELD);
        // 段偏移表：段 0/1/2 各自的行；段 3–7 保留。
        let seg =
            |i: usize| &body[SEGMENT_TABLE_OFFSET + i * SEGMENT_ITEM_LEN..][..SEGMENT_ITEM_LEN];
        assert_eq!(seg(0)[0], 0);
        assert_eq!(get_u16(seg(0), 3), 1, "文件头段：1 页");
        assert_eq!(seg(1)[0], 1);
        assert_eq!(get_u16(seg(1), 1), 1, "固定段起始页 1");
        assert_eq!(get_u16(seg(1), 7), 4, "固定段 4 条记录");
        assert_eq!(seg(2)[0], 2);
        assert_eq!(get_u16(seg(2), 1), 2, "记录数组起始页 2");
        assert_eq!(get_u16(seg(2), 3), (CF_PAGES - 2) as u16, "记录数组页数");
        assert_eq!(get_u16(seg(2), 5), 280);
        assert_eq!(get_u16(seg(2), 7), 1024);
        for i in 3..8 {
            assert_eq!(get_u16(seg(i), 1), 0, "段 {i} 保留");
        }
        // 页 1 与记录页的 payload_len。
        let p1 = read_raw(&io, h, 0, 1).unwrap();
        assert_eq!(get_u16(p1.as_slice(), 10), 560);
        let p2 = read_raw(&io, h, 0, 2).unwrap();
        assert_eq!(get_u16(p2.as_slice(), 10), 58 * 280);
        let p19 = read_raw(&io, h, 0, 19).unwrap();
        assert_eq!(get_u16(p19.as_slice(), 10), 38 * 280);
        // B 副本的副本号字段。
        let hb = open_rw(&io, B);
        let pb = read_raw(&io, hb, 1, 0).unwrap();
        assert_eq!(pb[12], 1);
    }

    // -- 更新协议 ------------------------------------------------------------

    #[test]
    fn update_protocol_publishes_and_clears_undo() {
        let io = new_mem();
        let mut cf = format_cf(&io);
        let p = progress(9, 4096);
        cf.write_checkpoint_progress(&p).unwrap();
        assert_eq!(cf.sequence(), 1, "一次更新 = seq +1");
        assert_eq!(cf.checkpoint_progress().unwrap(), p);
        cf.sync().unwrap();

        // 原始页：undo 已清、seq = 1，两副本一致。
        for (path, copy) in [(A, 0u8), (B, 1u8)] {
            let h = open_rw(&io, path);
            let p0 = read_raw(&io, h, copy, 0).unwrap();
            assert_eq!(get_u32(p0.as_slice(), 4), 1, "副本 {copy} seq");
            assert_eq!(read_undo(&p0).unwrap(), Undo::FREE, "副本 {copy} undo 已清");
        }
        // 重开后值仍在。
        let cf = reopen(&io);
        assert_eq!(cf.checkpoint_progress().unwrap(), p);
        assert_eq!(cf.sequence(), 1);
    }

    #[test]
    fn joint_publish_updates_progress_and_groups() {
        let io = new_mem();
        let mut cf = format_cf(&io);
        let p = progress(21, 8192);
        let mut r = redo_default();
        r.groups[0] = RedoGroup {
            member_stale: 0,
            sequence: 1,
            run: LogRunState::Current,
            archive: LogArchiveState::None,
        };
        r.current_group = 0;
        cf.write_checkpoint_and_groups(&p, &r).unwrap();
        assert_eq!(cf.sequence(), 1);
        let cf = reopen(&io);
        assert_eq!(cf.checkpoint_progress().unwrap(), p);
        assert_eq!(cf.redo_entries().unwrap(), r);
    }

    /// 在更新协议的四个写点各注入一次失败（含撕裂），重开后**都必须回到旧值**。
    fn crash_update_at(write_nth: u64, torn: Option<usize>) -> (u8, u32) {
        let fio = format_then_wrap();
        match torn {
            Some(bytes) => fio.add_rule(FaultRule::torn_write(write_nth, bytes, ErrorKind::Other)),
            None => fio.add_rule(FaultRule::once(FaultOp::Write, write_nth, ErrorKind::Other)),
        }
        {
            let mut cf = reopen(&fio);
            let err = cf.write_checkpoint_progress(&progress(9, 4096));
            assert!(err.is_err(), "注入口 {write_nth}（撕裂 {torn:?}）应失败");
        }
        let mut cf = reopen(&fio);
        assert_eq!(
            cf.checkpoint_progress().unwrap(),
            CheckpointProgress::default(),
            "崩溃点 {write_nth}（撕裂 {torn:?}）后应回退到旧值"
        );
        let out = (cf.active_copy(), cf.sequence());
        // 恢复后仍可继续发布，且新旧值都持久。
        cf.write_checkpoint_progress(&progress(9, 4096)).unwrap();
        let cf = reopen(&fio);
        assert_eq!(cf.checkpoint_progress().unwrap(), progress(9, 4096));
        out
    }

    #[test]
    fn crash_at_each_protocol_write_recovers_old_value() {
        // ① undo / ② 目标 / ③ 发布 / ④ 清 undo —— 四个"未发生即失败"的写点。
        for nth in 1..=4u64 {
            crash_update_at(nth, None);
        }
        // 撕裂写：目标（②）、发布（③）、清 undo（④）。
        for nth in [2u64, 3, 4] {
            crash_update_at(nth, Some(100));
        }
        // ③ 之后 seq 已 +1（"多计一次可容忍"）。
        let (_, seq) = crash_update_at(4, None);
        assert_eq!(seq, 1, "④ 前崩溃：值回退、seq 多计一次");
        // ① 之前/② 之前：seq 未动。
        assert_eq!(crash_update_at(1, None).1, 0);
        assert_eq!(crash_update_at(2, None).1, 0);
    }

    #[test]
    fn torn_publish_invalidates_copy_and_other_wins() {
        let fio = format_then_wrap();
        // 第 3 次写 = 副本 A 的"发布"写（页 0），撕裂后 A 的页 0 校验失败。
        fio.add_rule(FaultRule::torn_write(3, 64, ErrorKind::Other));
        {
            let mut cf = reopen(&fio);
            assert!(cf.write_checkpoint_progress(&progress(9, 4096)).is_err());
        }
        let cf = reopen(&fio);
        assert_eq!(cf.active_copy(), 1, "A 无效 ⇒ 生效副本 B");
        assert_eq!(cf.sequence(), 0, "B 未参与本次更新");
        assert_eq!(
            cf.checkpoint_progress().unwrap(),
            CheckpointProgress::default()
        );
        // A 已被整份重建：再发布一次后两副本一致。
        let mut cf = cf;
        cf.write_checkpoint_progress(&progress(9, 4096)).unwrap();
        let cf = reopen(&fio);
        assert_eq!(cf.checkpoint_progress().unwrap(), progress(9, 4096));
        let p0a = read_raw(&fio, open_rw(&fio, A), 0, 0).unwrap();
        let p0b = read_raw(&fio, open_rw(&fio, B), 1, 0).unwrap();
        assert_eq!(
            get_u32(p0a.as_slice(), 4),
            get_u32(p0b.as_slice(), 4),
            "两副本 seq 一致"
        );
    }

    #[test]
    fn copy_selection_prefers_valid_and_larger_seq() {
        let fio = format_then_wrap();
        {
            let mut cf = reopen(&fio);
            cf.write_checkpoint_progress(&progress(5, 100)).unwrap();
        }
        // B 的第 1 次写失败 ⇒ A 领先一个版本。
        fio.reset_counters();
        fio.add_rule(FaultRule::once(FaultOp::Write, 5, ErrorKind::Other));
        {
            let mut cf = reopen(&fio);
            assert!(cf.write_checkpoint_progress(&progress(9, 4096)).is_err());
        }
        let cf = reopen(&fio);
        assert_eq!(cf.active_copy(), 0, "A 的 seq 更大");
        assert_eq!(cf.sequence(), 2);
        assert_eq!(cf.checkpoint_progress().unwrap(), progress(9, 4096));
    }

    #[test]
    fn both_copies_invalid_fails_open() {
        let io = new_mem();
        format_cf(&io);
        for path in [A, B] {
            let h = open_rw(&io, path);
            io.write_at(h, &[0xAAu8; 64], 0).unwrap();
        }
        let err = ControlFile::open(&io, Path::new(A), Path::new(B)).unwrap_err();
        assert!(matches!(err, ControlFileError::BothCopiesInvalid { .. }));
    }

    #[test]
    fn unsupported_format_version_is_refused() {
        let io = new_mem();
        format_cf(&io);
        for (path, copy) in [(A, 0u8), (B, 1u8)] {
            let h = open_rw(&io, path);
            let mut p0 = read_raw(&io, h, copy, 0).unwrap();
            put_u16(page_body_mut(&mut p0), 0, 99);
            seal_page(&mut p0);
            io.write_at(h, p0.as_slice(), 0).unwrap();
        }
        match ControlFile::open(&io, Path::new(A), Path::new(B)).unwrap_err() {
            ControlFileError::BothCopiesInvalid { a, b } => {
                assert!(a.contains("格式版本"), "A：{a}");
                assert!(b.contains("格式版本"), "B：{b}");
            }
            other => panic!("预期 BothCopiesInvalid，得到 {other:?}"),
        }
    }

    #[test]
    fn bad_undo_checksum_invalidates_copy() {
        let io = new_mem();
        format_cf(&io);
        let h = open_rw(&io, A);
        let mut p0 = read_raw(&io, h, 0, 0).unwrap();
        write_undo(
            &mut p0,
            file_offset(1, CHECKPOINT_PROGRESS_OFFSET),
            &[7u8; 48],
        );
        // 破坏旧值中的一个字节 ⇒ undo 校验必然失败。
        p0[CF_PAGE_HEADER_LEN + UNDO_RECORD_OFFSET + 6] ^= 0xFF;
        seal_page(&mut p0);
        io.write_at(h, p0.as_slice(), 0).unwrap();
        let cf = reopen(&io);
        assert_eq!(cf.active_copy(), 1, "A 的 undo 不可信 ⇒ B 生效");
    }

    #[test]
    fn interval_bounds_are_enforced() {
        let io = new_mem();
        let mut cf = format_cf(&io);
        let too_long = vec![0u8; MAX_UPDATE_LEN + 1];
        assert!(matches!(
            cf.update_interval(0, &too_long),
            Err(ControlFileError::IntervalTooLarge { .. })
        ));
        assert!(matches!(
            cf.update_interval(0, &[]),
            Err(ControlFileError::IntervalOutOfBody)
        ));
        let small = [0u8; 8];
        // 侵入页头。
        assert!(matches!(
            cf.update_interval(8, &small),
            Err(ControlFileError::IntervalOutOfBody)
        ));
        // 跨页（页 1 页体末尾 + 4 字节）。
        let end_of_p1_body = (PAGE_SIZE + CF_PAGE_HEADER_LEN + CF_PAGE_BODY_LEN - 4) as u32;
        assert!(matches!(
            cf.update_interval(end_of_p1_body, &small),
            Err(ControlFileError::IntervalOutOfBody)
        ));
        // 侵入页尾。
        let trailer = (PAGE_SIZE - CF_PAGE_TRAILER_LEN - 4) as u32;
        assert!(matches!(
            cf.update_interval(trailer, &small),
            Err(ControlFileError::IntervalOutOfBody)
        ));
    }

    // -- 记录编解码 ----------------------------------------------------------

    #[test]
    fn sample_pairs_roundtrip_and_wrap() {
        let io = new_mem();
        let mut cf = format_cf(&io);
        assert!(cf.sample_pairs().unwrap().is_empty());
        for k in 1..=3u64 {
            cf.append_sample_pair(CommitSeq::from_raw(k * 10).unwrap(), k * 1_000)
                .unwrap();
        }
        assert_eq!(
            cf.sample_pairs().unwrap(),
            vec![
                (CommitSeq::from_raw(10).unwrap(), 1_000),
                (CommitSeq::from_raw(20).unwrap(), 2_000),
                (CommitSeq::from_raw(30).unwrap(), 3_000),
            ]
        );
        // 环写满：只剩最后 256 对，按写入顺序（最老 → 最新）。
        for k in 4..=260u64 {
            cf.append_sample_pair(CommitSeq::from_raw(k * 10).unwrap(), k * 1_000)
                .unwrap();
        }
        let pairs = cf.sample_pairs().unwrap();
        assert_eq!(pairs.len(), SAMPLE_PAIRS);
        assert_eq!(
            pairs[0],
            (CommitSeq::from_raw(50).unwrap(), 5_000),
            "最老的已回卷"
        );
        assert_eq!(
            pairs[SAMPLE_PAIRS - 1],
            (CommitSeq::from_raw(2600).unwrap(), 260_000)
        );
        let head = cf.sample_head().unwrap();
        assert_eq!(head.count, SAMPLE_PAIRS as u16);
        assert_eq!(head.next, (260 % SAMPLE_PAIRS) as u16);
    }

    #[test]
    fn redo_entries_validation_and_roundtrip() {
        // 存在 CURRENT 组但 current_group 未指 ⇒ 拒绝。
        let mut r = redo_default();
        r.groups[0] = RedoGroup {
            member_stale: 0,
            sequence: 5,
            run: LogRunState::Current,
            archive: LogArchiveState::None,
        };
        assert!(matches!(
            r.validate(),
            Err(ControlFileError::InconsistentRedo { .. })
        ));
        r.current_group = 0;
        assert!(r.validate().is_ok());

        // UNUSED 组序列号必须为 0。
        let mut r2 = redo_default();
        r2.groups[1].sequence = 3;
        assert!(r2.validate().is_err());

        // 至多一个 CURRENT。
        let mut r3 = RedoEntries::new(3, 2).unwrap();
        r3.groups[0].run = LogRunState::Current;
        r3.groups[1].run = LogRunState::Current;
        r3.current_group = 0;
        assert!(r3.validate().is_err());

        // 编码字节钉住 + 往返。
        let mut buf = [0u8; REDO_ENTRIES_LEN];
        r.encode(&mut buf);
        assert_eq!(&buf[0..4], &[2, 2, 0, 0], "头：组数/成员数/当前组/保留");
        assert_eq!(&buf[4..16], &[5, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(RedoEntries::decode(&buf).unwrap(), r);
        // 未知状态值 ⇒ 损坏。
        buf[4 + 4] = 9;
        assert!(matches!(
            RedoEntries::decode(&buf),
            Err(ControlFileError::OutOfDomain { .. })
        ));
        // 发布前校验：不一致的条目被拒绝。
        let io = new_mem();
        let mut cf = format_cf(&io);
        assert!(matches!(
            cf.write_redo_entries(&r2),
            Err(ControlFileError::InconsistentRedo { .. })
        ));
    }

    #[test]
    fn data_file_record_roundtrip_bounds_and_path() {
        let io = new_mem();
        let mut cf = format_cf(&io);
        let mut rec = DataFileRecord::new(3, 3);
        rec.status = 1;
        rec.creation_blocks = 128;
        rec.created_at = 99;
        rec.set_path(b"/mnt/ws/data/ws_undo").unwrap();
        cf.write_data_file_record(1023, &rec).unwrap();
        assert_eq!(cf.data_file_record(1023).unwrap(), rec);
        assert!(matches!(
            cf.data_file_record(1024),
            Err(ControlFileError::RecordIndexOutOfRange { .. })
        ));
        assert!(matches!(
            cf.write_data_file_record(1024, &rec),
            Err(ControlFileError::RecordIndexOutOfRange { .. })
        ));
        // 路径边界：255 字节可、256 拒、内嵌 NUL 拒。
        let mut r2 = DataFileRecord::new(4, 3);
        assert!(r2.set_path(&[b'x'; 255]).is_ok());
        assert_eq!(r2.path().len(), 255);
        assert!(matches!(
            r2.set_path(&[b'x'; 256]),
            Err(ControlFileError::PathTooLong { .. })
        ));
        assert!(matches!(
            r2.set_path(b"a/b\0c"),
            Err(ControlFileError::PathContainsNul)
        ));
    }

    #[test]
    fn workspace_entry_roundtrip_with_lineage() {
        let io = new_mem();
        let mut cf = format_cf(&io);
        let mut entry = ws_entry();
        entry.derived_from = Some(WorkspaceId::from_raw(3).unwrap());
        entry.derived_at_seq = CommitSeq::from_raw(12).unwrap();
        cf.write_workspace_entry(&entry).unwrap();
        let cf = reopen(&io);
        assert_eq!(cf.workspace_entry().unwrap(), entry);
        assert_eq!(cf.sequence(), 1);
    }
}
