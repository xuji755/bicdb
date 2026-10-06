//! **临时段与临时段池**（§4.8；file 2）。
//!
//! - 段与区**同一套机制**（[`crate::segment::Segment`] + LMT 位图），不另立体
//!   系——临时段的数据页是**标准页头**（类型 10，体同堆表页：槽位目录 + 行区
//!   + ITL），所以堆表页操作（[`crate::heap`]）在临时页上原样可用；
//! - **分配状态不在盘上**——这是唯一这样的段：状态全在**内存池**里，重启即
//!   消失。由两条一起保证（不是两个真相源，因为它们从不各自演进）：
//!   **file 2 打开即重置**（[`crate::datafile::DataFile::open_temp_reset`]）
//!   + **池同时清空**（本模块的 [`TempPool::new`]）；
//! - 池里放**临时段状态对象**（Oracle `X$KTSSO` 的对应物）：归属 / 段类型 /
//!   段头位置 / 用量。**不含区清单**——区清单的权威是段头页的区映射
//!   （§5.11），状态对象只是池的账本（互补，不重复）；
//! - **复用规则**：溢出结束**不删段**（像 Oracle 7.3 起的 SEP 那样留在池里，
//!   下次同类操作直接接管）；池空才去 file 2 的 LMT 里分配新区。**区等大
//!   （128 KB）⇒ 池里任何一块都能顶替任何一块，无需按大小匹配**；
//! - **持久化差异**：不记 redo（崩溃后整体重置，重放无意义）、不进备份；
//!   **有 undo**（语句级回滚）——写入路径由 P5 的排序/哈希/分词消费者接入
//!   （临时页变更不进 redo；崩溃后补偿按"页状态早于本记录 ⇒ 空操作"兜底）。
//!
//! 初始 1 区、倍增封顶 64 区（§14 第 37 项）。

use std::collections::BTreeMap;

use crate::datafile::DataFile;
use crate::segment::{SegType, Segment, SegmentSpaceError};

/// 临时段的**初始区数**（N ≥ 1——多数溢出很小，起步给多个区是浪费）。
pub const TEMP_INITIAL_EXTENTS: u32 = 1;

/// 临时段的**区数上限**（倍增封顶；§14 第 37 项）。
pub const TEMP_MAX_EXTENTS: u32 = 64;

/// 临时段类型（开放枚举：Oracle 有 `KTATSORT`/`KTATHASH`/… 一族；
/// V1.0 至少要有**排序**与**哈希**两种）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TempKind {
    /// 排序溢出。
    Sort,
    /// 哈希溢出（哈希连接/聚合）。
    Hash,
}

/// 临时段的归属（诊断用：哪个操作/会话在用临时空间）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TempOwner {
    /// 会话标识（P3 尚无会话层——执行器给稳定值；诊断只要求可区分）。
    pub session: u64,
    /// 语句序号（同一会话内的第几条溢出语句）。
    pub statement: u32,
}

impl TempOwner {
    /// 构造。
    #[must_use]
    pub const fn new(session: u64, statement: u32) -> Self {
        Self { session, statement }
    }
}

/// **临时段状态对象**（池的账本；区清单的权威在段头页）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TempSegment {
    /// 段类型。
    pub kind: TempKind,
    /// 归属（接管后更新为当前使用者）。
    pub owner: TempOwner,
    /// 段头位置（file_id + block_id）。
    pub header_file: u16,
    /// 段头块号。
    pub header_block: u32,
    /// 已用区数。
    pub extents: u32,
    /// 已用块数（诊断口径，随分配/扩展推进）。
    pub blocks: u64,
}

/// 池内句柄（接管/归还的凭据）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TempHandle(u64);

impl TempHandle {
    /// 原始值（诊断）。
    #[must_use]
    pub fn raw(self) -> u64 {
        self.0
    }
}

/// 池统计（诊断口径：命中 = 接管现成段；未命中 = 需新建）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TempPoolStats {
    /// 接管次数（池中有现成段）。
    pub hits: u64,
    /// 未命中次数（池空 ⇒ 调用方新建）。
    pub misses: u64,
    /// 归还次数（留在池中）。
    pub releases: u64,
    /// 当前空闲段数。
    pub free_segments: usize,
    /// 当前在手段数。
    pub live_segments: usize,
}

/// **临时段池**（每工作区一份；§4.8）。
///
/// 池不做 I/O：`acquire` 未命中时由调用方新建段（[`create_temp_segment`]）
/// 再 [`TempPool::register`]——保证池本身可纯测，I/O 留在调用方。
#[derive(Debug, Default)]
pub struct TempPool {
    free: Vec<TempSegment>,
    live: BTreeMap<TempHandle, TempSegment>,
    next: u64,
    hits: u64,
    misses: u64,
    releases: u64,
}

impl TempPool {
    /// 空池（"池与 file 2 同时清空"的池侧）。
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// **申请**：池中有**同类**的现成段 ⇒ 直接接管（区等大，任何一块都能顶替
    /// 任何一块）；否则 `None`——调用方新建（`create_temp_segment`）后
    /// [`TempPool::register`]。
    pub fn acquire(&mut self, kind: TempKind, owner: TempOwner) -> Option<TempHandle> {
        let at = self.free.iter().position(|s| s.kind == kind);
        let Some(at) = at else {
            self.misses += 1;
            return None;
        };
        let mut seg = self.free.swap_remove(at);
        seg.owner = owner; // 接管：归属换人
        let handle = TempHandle(self.next);
        self.next += 1;
        self.live.insert(handle, seg);
        self.hits += 1;
        Some(handle)
    }

    /// **登记一个新建的段**（池空时的补齐路径）并返回句柄。
    pub fn register(&mut self, seg: TempSegment, owner: TempOwner) -> TempHandle {
        let mut seg = seg;
        seg.owner = owner;
        let handle = TempHandle(self.next);
        self.next += 1;
        self.live.insert(handle, seg);
        handle
    }

    /// **归还**（溢出结束）：**留在池里待复用**——不删段、不还给 LMT
    /// （Oracle 7.3 那次改进的全部内容：删了再建，下一批区多半还是刚才那批）。
    pub fn release(&mut self, handle: TempHandle) -> bool {
        let Some(seg) = self.live.remove(&handle) else {
            return false;
        };
        self.free.push(seg);
        self.releases += 1;
        true
    }

    /// 在手段的只读视图（诊断/扩展用）。
    #[must_use]
    pub fn get(&self, handle: TempHandle) -> Option<&TempSegment> {
        self.live.get(&handle)
    }

    /// **记录用量推进**（扩展后由调用方回报；`extents` 超上限即拒绝——
    /// 上限由 [`TEMP_MAX_EXTENTS`] 固定）。
    pub fn note_extended(&mut self, handle: TempHandle, extents: u32) -> Result<(), TempPoolError> {
        let Some(seg) = self.live.get_mut(&handle) else {
            return Err(TempPoolError::UnknownHandle);
        };
        if extents > seg.extents {
            if extents > TEMP_MAX_EXTENTS {
                return Err(TempPoolError::BeyondExtentLimit {
                    requested: extents,
                    limit: TEMP_MAX_EXTENTS,
                });
            }
            seg.blocks = seg.blocks.saturating_add(
                u64::from(extents - seg.extents) * u64::from(crate::bitmap::EXTENT_BLOCKS),
            );
            seg.extents = extents;
        }
        Ok(())
    }

    /// 统计快照。
    #[must_use]
    pub fn stats(&self) -> TempPoolStats {
        TempPoolStats {
            hits: self.hits,
            misses: self.misses,
            releases: self.releases,
            free_segments: self.free.len(),
            live_segments: self.live.len(),
        }
    }
}

/// 池操作错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TempPoolError {
    /// 句柄不在册。
    UnknownHandle,
    /// 扩展越过区数上限（[`TEMP_MAX_EXTENTS`]）。
    BeyondExtentLimit {
        /// 请求的区数。
        requested: u32,
        /// 上限。
        limit: u32,
    },
}

impl std::fmt::Display for TempPoolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TempPoolError::UnknownHandle => f.write_str("临时段句柄不在册"),
            TempPoolError::BeyondExtentLimit { requested, limit } => {
                write!(
                    f,
                    "临时段扩展越过上限（请求 {requested} 区，上限 {limit} 区）"
                )
            }
        }
    }
}

impl std::error::Error for TempPoolError {}

/// **新建一个临时段**（在 file 2 上：`SegType::Temporary` + 初始 1 区）并返回
/// 它的状态对象（调用方随后 [`TempPool::register`]）。
///
/// 临时段**有盘上段头页**（格式与其他段头页相同）——但它是"内存池在同一
/// 会话内的附属物"，不是持久元数据：file 2 打开即重置，两者同生共死。
pub fn create_temp_segment<'io, 'f>(
    file: &'f mut DataFile<'io>,
    kind: TempKind,
    obj: u32,
    dataobj: u32,
    itl_max: u8,
) -> Result<(Segment<'io, 'f>, TempSegment), SegmentSpaceError> {
    let segment = Segment::create(file, SegType::Temporary, obj, dataobj, itl_max, 0, 0)?;
    let header_block = segment.page0_block();
    let file_id = segment.file_id();
    let extents = u32::from(segment.header().extent_count);
    let seg = TempSegment {
        kind,
        owner: TempOwner::new(0, 0),
        header_file: file_id,
        header_block,
        extents,
        blocks: u64::from(crate::bitmap::EXTENT_BLOCKS),
    };
    Ok((segment, seg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datafile::DataFile;
    use std::path::Path;

    const WS: [u8; 8] = [7u8; 8];

    #[test]
    fn pool_reuses_a_released_segment_without_touching_io() {
        // 接管 = 零 I/O：归还一个段后，同类申请直接命中它。
        let mut pool = TempPool::new();
        let seg = TempSegment {
            kind: TempKind::Sort,
            owner: TempOwner::new(1, 1),
            header_file: 2,
            header_block: 321,
            extents: 1,
            blocks: 8,
        };
        let h0 = pool.register(seg, TempOwner::new(1, 1));
        assert!(pool.release(h0));
        assert_eq!(pool.stats().releases, 1);
        assert_eq!(pool.stats().free_segments, 1);

        let h1 = pool
            .acquire(TempKind::Sort, TempOwner::new(2, 1))
            .expect("接管");
        assert_eq!(
            pool.get(h1).unwrap().owner,
            TempOwner::new(2, 1),
            "归属换人"
        );
        assert_eq!(
            pool.get(h1).unwrap().header_block,
            321,
            "整段复用（同一段头）"
        );
        assert_eq!(pool.stats().hits, 1);
        assert_eq!(pool.stats().free_segments, 0);

        // 类型不同不混用（排序的段不被哈希接管）。
        assert!(pool.acquire(TempKind::Hash, TempOwner::new(3, 1)).is_none());
        assert_eq!(pool.stats().misses, 1);
    }

    #[test]
    fn extension_accounting_and_the_cap() {
        let mut pool = TempPool::new();
        let h = pool.register(
            TempSegment {
                kind: TempKind::Hash,
                owner: TempOwner::new(1, 1),
                header_file: 2,
                header_block: 321,
                extents: 1,
                blocks: 8,
            },
            TempOwner::new(1, 1),
        );
        pool.note_extended(h, 2).unwrap();
        let seg = *pool.get(h).unwrap();
        assert_eq!(seg.extents, 2);
        assert_eq!(seg.blocks, 16, "倍增的用量记账");
        assert!(matches!(
            pool.note_extended(h, TEMP_MAX_EXTENTS + 1),
            Err(TempPoolError::BeyondExtentLimit { .. })
        ));
        // 回退请求（不超过当前）不改变任何东西。
        pool.note_extended(h, 1).unwrap();
        assert_eq!(pool.get(h).unwrap().extents, 2);
    }

    #[test]
    fn temp_file_reset_makes_previous_extents_free_again() {
        // §4.8：file 2 打开即重置（重写文件头与页分配位图）——
        // 上一会话分配过的区在重开后**全部回到 LMT 里**。
        let io = bicdb_workspace::io::MemFileIo::new();
        io.add_dir("/mem");
        let path = Path::new("/mem/ws_temp");
        let mut file = DataFile::open_temp_reset(&io, path, WS, 512).unwrap();
        assert_eq!(file.file_id(), crate::datafile::TEMP_FILE_ID);
        let first = file.allocate_extent().unwrap();
        let second = file.allocate_extent().unwrap();
        assert_ne!(
            first.first_block_in(crate::bitmap::FileLayout::standard()),
            second.first_block_in(crate::bitmap::FileLayout::standard())
        );
        file.sync().unwrap();
        drop(file);

        // 重开：位图整体清空 ⇒ 第一次分配又拿到第一个区。
        let mut again = DataFile::open_temp_reset(&io, path, WS, 512).unwrap();
        let reused = again.allocate_extent().unwrap();
        assert_eq!(
            reused.first_block_in(crate::bitmap::FileLayout::standard()),
            first.first_block_in(crate::bitmap::FileLayout::standard()),
            "重置后从头分配"
        );
    }

    #[test]
    fn temp_segment_pages_are_standard_pages() {
        // 临时段建在 file 2 上、数据页是**标准页头**（类型 10）：堆表的行操作
        // 在临时页上原样可用（§4.8"机制统一"）。
        let io = bicdb_workspace::io::MemFileIo::new();
        io.add_dir("/mem");
        let mut file = DataFile::open_temp_reset(&io, Path::new("/mem/ws_temp"), WS, 512).unwrap();
        let (segment, seg) = create_temp_segment(&mut file, TempKind::Hash, 7, 8, 4).unwrap();
        assert_eq!(seg.kind, TempKind::Hash);
        assert_eq!(seg.header_file, crate::datafile::TEMP_FILE_ID);
        assert_eq!(seg.header_block, segment.page0_block());
        assert_eq!(seg.extents, 1, "初始 1 区");

        // 逻辑页 2 = 首个数据页：格式化为临时页并插一行。
        let block = segment.logical_block(2).expect("首区已映射");
        let mut page = crate::page::Page::new(crate::page::PageType::Temporary, WS, 2, block);
        let row = crate::row::assemble_row(0, 0xFF, &[false], &[], &[b"temp-row"]).unwrap();
        crate::heap::insert_row(&mut page, &row, &crate::heap::InsertPolicy::append_only())
            .unwrap();
        segment.write_page(2, &mut page).unwrap();

        // 读回（页自洽、行可取）。
        let back = segment.read_page(2).unwrap();
        assert_eq!(
            back.header().unwrap().page_type,
            crate::page::PageType::Temporary
        );
        assert_eq!(
            crate::heap::row(&back, 1),
            Some(&row[..]),
            "临时页与堆表页同规"
        );
    }
}
