//! 恢复的**撤销阶段**（§11.2 第 3 步，§4.6.6 ③）：对分析阶段判出的**输家**
//! 执行普通回滚——**同一条代码路径**，不设"恢复专用回滚"。
//!
//! ```text
//! 对每个输家槽：
//!   ① 槽置"待回滚"（诊断：崩溃点离完成只差一步）          ┐
//!   ② 沿 undo_current 整链，从新到旧应用逆操作：          │ 每次页写
//!       读页 → 补偿（apply_undo_to_page）→ 差异（page_diff）│ 之前先落
//!       → 补偿生成 redo 并刷盘 → 写页（page_lsn 推进）    │ redo（规则 2）
//!   ③ 写"回滚完成"记录（0x31）并刷盘                       │
//!   ④ 释放槽（wrap + 1；undo 页改动同样先 redo 后写页）    ┘
//! ```
//!
//! # 为什么补偿必须生成 redo（§4.6.6 ③ 的"延伸"）
//!
//! 撤销阶段**直接改页文件**（无缓冲池，写即离开本进程）。若补偿不留 redo，
//! "槽已释放"（撤完的标志）可能先于补偿字节落盘——二次崩溃后回收站式地
//! 重放会发现：分析看不到输家（槽 Free）、页上却留着未提交的修改。
//! 所以每个页写走 **WAL 次序**：先追加补偿记录并 `flush`，其后才写页。
//!
//! # 可重入（§4.6.6 ③ 的两条结论）
//!
//! 五类补偿**幂等**（`bicdb_storage::undo::apply_undo_to_page`），所以
//! 崩溃中断**不需要断点标记**——重走整链即可；`0x31 回滚完成`记录只是
//! 让分析阶段此后一眼判出"已回滚"（省掉一次无谓重放），不是正确性前提。
//! 槽的 `undo_current` 不在回放中推进：链在恢复期内不回收，整链永远完整。
//!
//! # 与"待回滚"标记的关系
//!
//! 分析阶段把 `Active` 与 `PendingRollback` **都**当输家（§4.6.6 ③ 末），
//! 所以本阶段的 ① 只是与正常回滚同一路径的诊断标记——不是重入依据。

use std::io;

use bicdb_storage::page::{Page, PAGE_SIZE};
use bicdb_storage::pagefile::{self, PageFileError};
use bicdb_storage::rowid::Rdba;
use bicdb_storage::segment::SegmentSpaceError;
use bicdb_storage::undo::{
    apply_undo_to_page, free_slot, read_slot, txn_id_of, write_slot, RollbackError, TxnState,
    UndoChain, UndoChainError, UndoError,
};
use bicdb_workspace::io::FileIo;

use crate::apply::BlockResolver;
use crate::group::{GroupError, GroupWriter};
use crate::record::{page_diff, BlockRef, RedoRecord};

/// 撤销阶段的执行报告。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UndoReport {
    /// 被回滚的事务数。
    pub txns_rolled_back: usize,
    /// 回放的撤销记录数（含幂等空操作）。
    pub records_replayed: u64,
    /// 真正写入的页数（补偿 + 事务表标记）。
    pub pages_written: u64,
    /// 为补偿生成的 redo 记录数（页修改 + 回滚完成）。
    pub redo_records: u64,
}

/// 撤销阶段错误。
#[derive(Debug)]
pub enum UndoPhaseError {
    /// 底层 I/O。
    Io(io::Error),
    /// 日志写入/刷盘错误。
    Group(GroupError),
    /// 页文件错误（损坏按损坏处理）。
    Page(PageFileError),
    /// 事务表访问错误。
    Undo(UndoError),
    /// undo 链读取错误。
    Chain(UndoChainError),
    /// 补偿动作错误（含"更新类暂缓"——本切片明确不蒙混）。
    Rollback(RollbackError),
    /// 块无法定位。
    Unresolved(Rdba),
    /// 段读取错误（undo 段头页/区映射）。
    Segment(SegmentSpaceError),
    /// 链/事务表结构不自洽（自述对不上）。
    Malformed(&'static str),
}

impl std::fmt::Display for UndoPhaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UndoPhaseError::Io(e) => write!(f, "撤销阶段 I/O：{e}"),
            UndoPhaseError::Group(e) => write!(f, "撤销阶段日志：{e}"),
            UndoPhaseError::Page(e) => write!(f, "撤销阶段页文件：{e}"),
            UndoPhaseError::Undo(e) => write!(f, "撤销阶段事务表：{e}"),
            UndoPhaseError::Chain(e) => write!(f, "撤销阶段链读取：{e}"),
            UndoPhaseError::Rollback(e) => write!(f, "撤销阶段补偿：{e}"),
            UndoPhaseError::Unresolved(r) => write!(
                f,
                "撤销阶段块无法定位（文件 {} 块 {}）",
                r.file_id(),
                r.block_id()
            ),
            UndoPhaseError::Segment(e) => write!(f, "撤销阶段段访问：{e}"),
            UndoPhaseError::Malformed(s) => write!(f, "撤销阶段结构不自洽：{s}"),
        }
    }
}

impl std::error::Error for UndoPhaseError {}

impl From<io::Error> for UndoPhaseError {
    fn from(e: io::Error) -> Self {
        UndoPhaseError::Io(e)
    }
}
impl From<GroupError> for UndoPhaseError {
    fn from(e: GroupError) -> Self {
        UndoPhaseError::Group(e)
    }
}
impl From<PageFileError> for UndoPhaseError {
    fn from(e: PageFileError) -> Self {
        UndoPhaseError::Page(e)
    }
}
impl From<UndoError> for UndoPhaseError {
    fn from(e: UndoError) -> Self {
        UndoPhaseError::Undo(e)
    }
}
impl From<UndoChainError> for UndoPhaseError {
    fn from(e: UndoChainError) -> Self {
        UndoPhaseError::Chain(e)
    }
}
impl From<RollbackError> for UndoPhaseError {
    fn from(e: RollbackError) -> Self {
        UndoPhaseError::Rollback(e)
    }
}
impl From<SegmentSpaceError> for UndoPhaseError {
    fn from(e: SegmentSpaceError) -> Self {
        UndoPhaseError::Segment(e)
    }
}

/// **撤销阶段**：回滚 `slots` 列出的输家（槽号来自
/// [`bicdb_storage::undo::repair_committed_slots`] 的输家扫描）。
///
/// 补偿经 `writer` 落 redo（WAL 次序）；**所有**页定位（数据页与 undo 段
/// 头页）一律经 `resolve`——undo 段的文件角色由调用方在解析器里给出。
pub fn rollback_losers(
    io: &dyn FileIo,
    writer: &mut GroupWriter<'_, '_>,
    chain: &UndoChain<'_, '_>,
    slots: &[u16],
    resolve: &mut BlockResolver<'_>,
) -> Result<UndoReport, UndoPhaseError> {
    let mut report = UndoReport::default();
    for &slot in slots {
        let head_page = chain.segment().read_page(0)?;
        let txn_slot = read_slot(&head_page, slot)?;
        if txn_slot.state == TxnState::Free {
            continue; // 已撤完（重复调用/陈旧列表）——不二次 `wrap`
        }
        let txn_raw = txn_id_of(slot, &txn_slot).as_raw();

        // ① 置"待回滚"（诊断）。
        if txn_slot.state != TxnState::PendingRollback {
            let mut page = chain.segment().read_page(0)?;
            let before = *page.as_bytes();
            let mut marked = read_slot(&page, slot)?;
            marked.state = TxnState::PendingRollback;
            write_slot(&mut page, slot, &marked)?;
            write_page_with_redo(
                io,
                writer,
                &undo_page_target(chain, resolve)?,
                &before,
                &mut page,
                txn_raw,
                &mut report,
            )?;
        }

        // ② 整链从新到旧应用逆操作。
        let mut at = txn_slot.undo_current;
        while let Some(pos) = at {
            let record = chain.read(pos)?;
            at = record.prev;
            let rdba = Rdba::from_parts(record.rowid.file_id(), record.rowid.block_id())
                .ok_or(UndoPhaseError::Malformed("撤销记录的行号越出 RDBA 域"))?;
            let (handle, block) = resolve(rdba).ok_or(UndoPhaseError::Unresolved(rdba))?;
            let mut page = pagefile::read_page_verified(io, handle, block)?;
            let before = *page.as_bytes();
            apply_undo_to_page(&mut page, &record)?;
            write_page_with_redo(
                io,
                writer,
                &(handle, block, rdba),
                &before,
                &mut page,
                txn_raw,
                &mut report,
            )?;
            report.records_replayed += 1;
        }

        // ③ "回滚完成"标记（此后分析一眼判出；不是正确性前提——补偿幂等）。
        let lsn = writer.append(|l| RedoRecord::rollback_done(l, txn_raw))?;
        writer.flush(lsn)?;
        report.redo_records += 1;

        // ④ 释放事务表槽（undo 段头页同样先 redo 后写页）。
        let mut page = chain.segment().read_page(0)?;
        let before = *page.as_bytes();
        free_slot(&mut page, slot)?;
        write_page_with_redo(
            io,
            writer,
            &undo_page_target(chain, resolve)?,
            &before,
            &mut page,
            txn_raw,
            &mut report,
        )?;

        report.txns_rolled_back += 1;
    }
    Ok(report)
}

/// undo 段头页（逻辑页 0）的定位三元组。
fn undo_page_target(
    chain: &UndoChain<'_, '_>,
    resolve: &mut BlockResolver<'_>,
) -> Result<(bicdb_workspace::io::FileHandle, u32, Rdba), UndoPhaseError> {
    let block = chain
        .segment()
        .logical_block(0)
        .ok_or(UndoPhaseError::Malformed("undo 段头页不在映射内"))?;
    let rdba = Rdba::from_parts(chain.segment().file_id(), block)
        .ok_or(UndoPhaseError::Malformed("undo 段头页地址越界"))?;
    let (handle, block_no) = resolve(rdba).ok_or(UndoPhaseError::Unresolved(rdba))?;
    Ok((handle, block_no, rdba))
}

/// 页改动落盘：**差异非空**才追加 redo 记录、刷盘（WAL 次序）、推进
/// `page_lsn`/`mod_seq` 并写页；差异为空 = 该补偿已生效过（幂等空操作），
/// 不写页也不写日志。
fn write_page_with_redo(
    io: &dyn FileIo,
    writer: &mut GroupWriter<'_, '_>,
    target: &(bicdb_workspace::io::FileHandle, u32, Rdba),
    before: &[u8; PAGE_SIZE],
    page: &mut Page,
    txn_raw: u64,
    report: &mut UndoReport,
) -> Result<(), UndoPhaseError> {
    let (handle, block_no, rdba) = *target;
    let changes = page_diff(before, page.as_bytes());
    if changes.is_empty() {
        return Ok(());
    }
    let lsn = writer.append(|l| {
        RedoRecord::page_modification(
            l,
            txn_raw,
            vec![BlockRef {
                flags: 0,
                rdba,
                changes: changes.clone(), // `append` 的闭包是 `Fn`（满则刷+重试）
            }],
        )
    })?;
    writer.flush(lsn)?; // 规则 2：补偿日志落盘先于页落盘
    report.redo_records += 1;

    let mut header = page.header().ok_or(UndoPhaseError::Malformed("页头缺失"))?;
    header.page_lsn = lsn;
    page.write_header(&header);
    page.bump_mod_seq();

    pagefile::write_page(io, handle, block_no, page)?;
    report.pages_written += 1;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use bicdb_common::seq::CommitSeq;
    use bicdb_storage::controlfile::{ArchiveRecord, ControlFile, RedoEntries, WorkspaceEntry};
    use bicdb_storage::datafile::DataFile;
    use bicdb_storage::heap::{self, InsertPolicy};
    use bicdb_storage::itl::{self, ItlEntry, ItlState};
    use bicdb_storage::page::{Page, PageType, WORKSPACE_REF_LEN};
    use bicdb_storage::pagefile;
    use bicdb_storage::row::assemble_row;
    use bicdb_storage::rowid::RowId;
    use bicdb_storage::undo::{
        create_undo_segment, read_slot, repair_committed_slots, TxnId, UndoOp, UndoPayload,
    };
    use bicdb_workspace::id::WorkspaceId;
    use bicdb_workspace::io::{FileHandle, MemFileIo};

    use super::*;
    use crate::analysis::analyze_from;
    use crate::group::{online_groups, GroupSpec, GroupWriter};
    use crate::recovery::redo_from;

    const UNDO_F: &str = "/mem/undo1.dat";
    const DATA_F: &str = "/mem/data.dat";
    const A: &str = "/mem/control01.ctl";
    const B: &str = "/mem/control02.ctl";
    const WAL: &str = "/mem/wal";
    const WS: [u8; 8] = [9u8; 8];

    fn mem() -> MemFileIo {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        io.add_dir(WAL);
        io
    }

    fn row_bytes(payload: &[u8]) -> Vec<u8> {
        assemble_row(0, 1, &[false], &[], &[payload])
    }

    fn rdba(file_id: u16, block: u32) -> Rdba {
        Rdba::from_parts(file_id, block).unwrap()
    }

    fn ws_entry() -> WorkspaceEntry {
        WorkspaceEntry {
            workspace_id: WorkspaceId::from_raw(1).unwrap(),
            created_at: 0,
            derived_from: None,
            derived_at_seq: CommitSeq::from_raw(0).unwrap(),
        }
    }

    fn resolver(
        undo: FileHandle,
        data: FileHandle,
    ) -> impl FnMut(Rdba) -> Option<(FileHandle, u32)> {
        move |r| match r.file_id() {
            1 => Some((undo, r.block_id())),
            3 => Some((data, r.block_id())),
            _ => None,
        }
    }

    /// **输家夹具**：模拟"插入一行"的写路径痕迹——占用 ITL[0]（记旧值
    /// `None` = 原为空闲）→ 插入行（记 Insert）→ 页上 ITL[0] 归该事务。
    /// 返回行号。
    fn loser_fixture(
        io: &dyn FileIo,
        chain: &mut UndoChain<'_, '_>,
        slot: u16,
        data: FileHandle,
        payload: &[u8],
    ) -> u16 {
        let wrap = read_slot(&chain.segment().read_page(0).unwrap(), slot)
            .unwrap()
            .wrap;
        let owner = TxnId::from_parts(0, slot as u8, wrap);
        let mut page = Page::new(PageType::HeapTable, [0u8; WORKSPACE_REF_LEN], 3, 0);
        let bytes = row_bytes(payload);
        let n = heap::insert_row(&mut page, &bytes, &InsertPolicy::in_place(0)).unwrap();
        let rid = RowId::from_parts(3, 0, n).unwrap();
        chain
            .append(
                slot,
                UndoOp::ItlOverwrite,
                0,
                RowId::from_parts(3, 0, 1).unwrap(),
                UndoPayload::ItlOverwrite {
                    itl_slot: 0,
                    old: None,
                },
            )
            .unwrap();
        let head = chain
            .append(slot, UndoOp::Insert, 0, rid, UndoPayload::None)
            .unwrap();
        itl::write_itl(
            &mut page,
            0,
            &ItlEntry {
                txn_id: owner,
                undo_ptr: Some(head),
                commit_seq: None,
                lock_cnt: 1,
                state: ItlState::Active,
            },
        )
        .unwrap();
        pagefile::write_page(io, data, 0, &mut page).unwrap();
        n
    }

    #[test]
    fn rolls_back_a_loser_and_writes_compensation_redo() {
        let io = mem();
        let mut file = DataFile::create(&io, Path::new(UNDO_F), 1, 1, WS, 512).unwrap();
        let undo_handle = file.handle();
        let segment = create_undo_segment(&mut file, 2, 3, 4).unwrap();
        let mut chain = UndoChain::open(segment);
        let slot = chain.allocate_slot().unwrap();
        let data = pagefile::create(&io, Path::new(DATA_F), 1).unwrap();
        let n = loser_fixture(&io, &mut chain, slot, data, b"alpha");
        let txn_raw = TxnId::from_parts(0, slot as u8, 0).as_raw();

        let spec = GroupSpec::new(4, 1, 64).unwrap();
        let mut cf = ControlFile::format(
            &io,
            Path::new(A),
            Path::new(B),
            &ws_entry(),
            &RedoEntries::new(4, 1).unwrap(),
            &ArchiveRecord::default(),
        )
        .unwrap();
        let lsn0 = bicdb_common::seq::Lsn::from_raw(0).unwrap();
        let mut writer = GroupWriter::create(&io, &mut cf, Path::new(WAL), spec, lsn0).unwrap();

        let mut resolve = resolver(undo_handle, data);
        let report = rollback_losers(&io, &mut writer, &chain, &[slot], &mut resolve).unwrap();
        assert_eq!(report.txns_rolled_back, 1);
        assert_eq!(report.records_replayed, 2, "ItlOverwrite + Insert");
        // 待回滚标记 + 两条数据页补偿（ITL 还原、删行）+ 槽释放 = 4 条
        // 页修改 redo，+ 回滚完成 = 5。
        assert_eq!(report.redo_records, 5);
        assert_eq!(report.pages_written, 4);

        // 数据页：行已删、ITL 还原为空闲、page_lsn 已推进。
        let page = pagefile::read_page_verified(&io, data, 0).unwrap();
        assert_eq!(heap::row(&page, n), None, "插入被撤销");
        assert_eq!(itl::read_itl(&page, 0).unwrap().state, ItlState::Free);
        assert!(page.header().unwrap().page_lsn > lsn0, "页推进到补偿记录");

        // 事务表槽已释放（wrap + 1）。
        let hdr = chain.segment().read_page(0).unwrap();
        let s = read_slot(&hdr, slot).unwrap();
        assert_eq!(s.state, TxnState::Free);
        assert_eq!(s.wrap, 1);

        // 日志里：补偿页修改（数据页）+ 回滚完成。
        writer.close().unwrap();
        let cf = ControlFile::open(&io, Path::new(A), Path::new(B)).unwrap();
        let groups = online_groups(&io, &cf, Path::new(WAL), spec).unwrap();
        let scan = crate::file::scan_log(
            &io,
            groups[0].handle,
            groups[0].start_lsn,
            u64::from(groups[0].file_pages),
        )
        .unwrap();
        let done = scan
            .records
            .iter()
            .find(|r| r.op == crate::record::RecordOp::RollbackDone.as_u8() && r.txn_id == txn_raw);
        assert!(done.is_some(), "回滚完成记录在流中");
        // 两条补偿都落在数据页（ITL 还原、删行）——页的 `page_lsn` = 最后一条。
        let last_compensation = scan
            .records
            .iter()
            .rev()
            .find(|r| {
                r.op == crate::record::RecordOp::PageModification.as_u8()
                    && r.blocks.iter().any(|b| b.rdba == rdba(3, 0))
            })
            .expect("数据页补偿记录在流中");
        assert_eq!(
            page.header().unwrap().page_lsn,
            last_compensation.lsn,
            "页的 page_lsn = 最后一条补偿记录的 LSN"
        );
    }

    #[test]
    fn compensation_redo_replays_after_lost_page_write() {
        let io = mem();
        let mut file = DataFile::create(&io, Path::new(UNDO_F), 1, 1, WS, 512).unwrap();
        let undo_handle = file.handle();
        let segment = create_undo_segment(&mut file, 2, 3, 4).unwrap();
        let mut chain = UndoChain::open(segment);
        let slot = chain.allocate_slot().unwrap();
        let data = pagefile::create(&io, Path::new(DATA_F), 1).unwrap();
        let n = loser_fixture(&io, &mut chain, slot, data, b"beta");

        let spec = GroupSpec::new(4, 1, 64).unwrap();
        let mut cf = ControlFile::format(
            &io,
            Path::new(A),
            Path::new(B),
            &ws_entry(),
            &RedoEntries::new(4, 1).unwrap(),
            &ArchiveRecord::default(),
        )
        .unwrap();
        let lsn0 = bicdb_common::seq::Lsn::from_raw(0).unwrap();
        let mut writer = GroupWriter::create(&io, &mut cf, Path::new(WAL), spec, lsn0).unwrap();
        let mut resolve = resolver(undo_handle, data);
        rollback_losers(&io, &mut writer, &chain, &[slot], &mut resolve).unwrap();
        writer.close().unwrap();

        // 模拟"补偿页写丢失"：把数据页恢复成回滚前那一刻（行在、ITL 活动、
        // page_lsn = 0）——补偿记录必须能把它重放回滚后状态。
        let mut page = Page::new(PageType::HeapTable, [0u8; WORKSPACE_REF_LEN], 3, 0);
        let bytes = row_bytes(b"beta");
        let _ = heap::insert_row(&mut page, &bytes, &InsertPolicy::in_place(0)).unwrap();
        itl::write_itl(
            &mut page,
            0,
            &ItlEntry {
                txn_id: TxnId::from_parts(0, slot as u8, 0),
                undo_ptr: None,
                commit_seq: None,
                lock_cnt: 1,
                state: ItlState::Active,
            },
        )
        .unwrap();
        pagefile::write_page(&io, data, 0, &mut page).unwrap();

        // 重做阶段：补偿记录重放 → 页回到回滚后状态。
        let cf = ControlFile::open(&io, Path::new(A), Path::new(B)).unwrap();
        let groups = online_groups(&io, &cf, Path::new(WAL), spec).unwrap();
        redo_from(&io, &groups, lsn0, &mut resolve).unwrap();
        let page = pagefile::read_page_verified(&io, data, 0).unwrap();
        assert_eq!(heap::row(&page, n), None, "补偿被重放：行仍不在");
        assert_eq!(itl::read_itl(&page, 0).unwrap().state, ItlState::Free);
    }

    #[test]
    fn crash_mid_undo_replays_the_whole_chain() {
        let io = mem();
        let mut file = DataFile::create(&io, Path::new(UNDO_F), 1, 1, WS, 512).unwrap();
        let undo_handle = file.handle();
        let segment = create_undo_segment(&mut file, 2, 3, 4).unwrap();
        let mut chain = UndoChain::open(segment);
        let slot = chain.allocate_slot().unwrap();
        let data = pagefile::create(&io, Path::new(DATA_F), 1).unwrap();

        // 事务动作：插入一行，再删除它（链 = 占用 + Insert + Delete）。
        let owner = TxnId::from_parts(0, slot as u8, 0);
        let mut page = Page::new(PageType::HeapTable, [0u8; WORKSPACE_REF_LEN], 3, 0);
        let bytes = row_bytes(b"gamma");
        let n = heap::insert_row(&mut page, &bytes, &InsertPolicy::in_place(0)).unwrap();
        let rid = RowId::from_parts(3, 0, n).unwrap();
        chain
            .append(
                slot,
                UndoOp::ItlOverwrite,
                0,
                RowId::from_parts(3, 0, 1).unwrap(),
                UndoPayload::ItlOverwrite {
                    itl_slot: 0,
                    old: None,
                },
            )
            .unwrap();
        chain
            .append(slot, UndoOp::Insert, 0, rid, UndoPayload::None)
            .unwrap();
        heap::delete_row(&mut page, n).unwrap();
        let head = chain
            .append(
                slot,
                UndoOp::Delete,
                0,
                rid,
                UndoPayload::FullRow(bytes.clone()),
            )
            .unwrap();
        itl::write_itl(
            &mut page,
            0,
            &ItlEntry {
                txn_id: owner,
                undo_ptr: Some(head),
                commit_seq: None,
                lock_cnt: 1,
                state: ItlState::Active,
            },
        )
        .unwrap();
        pagefile::write_page(&io, data, 0, &mut page).unwrap();

        // **模拟撤销中断在中间**：只应用了最新一条（Delete）的补偿。
        let mut page = pagefile::read_page_verified(&io, data, 0).unwrap();
        let head_rec = chain.read(head).unwrap();
        apply_undo_to_page(&mut page, &head_rec).unwrap();
        pagefile::write_page(&io, data, 0, &mut page).unwrap();
        assert_eq!(heap::row(&page, n), Some(&bytes[..]), "中途：行已恢复");

        // 恢复的撤销阶段：整链重走——Delete 补偿幂等空操作、Insert 补偿生效。
        let spec = GroupSpec::new(4, 1, 64).unwrap();
        let mut cf = ControlFile::format(
            &io,
            Path::new(A),
            Path::new(B),
            &ws_entry(),
            &RedoEntries::new(4, 1).unwrap(),
            &ArchiveRecord::default(),
        )
        .unwrap();
        let lsn0 = bicdb_common::seq::Lsn::from_raw(0).unwrap();
        let mut writer = GroupWriter::create(&io, &mut cf, Path::new(WAL), spec, lsn0).unwrap();
        let mut resolve = resolver(undo_handle, data);
        let report = rollback_losers(&io, &mut writer, &chain, &[slot], &mut resolve).unwrap();

        assert_eq!(report.records_replayed, 3);
        let page = pagefile::read_page_verified(&io, data, 0).unwrap();
        assert_eq!(heap::row(&page, n), None, "整链走完的净效果");
        assert_eq!(itl::read_itl(&page, 0).unwrap().state, ItlState::Free);
        assert_eq!(
            read_slot(&chain.segment().read_page(0).unwrap(), slot)
                .unwrap()
                .state,
            TxnState::Free
        );
    }

    #[test]
    fn free_slots_are_skipped() {
        let io = mem();
        let mut file = DataFile::create(&io, Path::new(UNDO_F), 1, 1, WS, 512).unwrap();
        let undo_handle = file.handle();
        let segment = create_undo_segment(&mut file, 2, 3, 4).unwrap();
        let mut chain = UndoChain::open(segment);
        let slot = chain.allocate_slot().unwrap();
        let data = pagefile::create(&io, Path::new(DATA_F), 1).unwrap();
        loser_fixture(&io, &mut chain, slot, data, b"delta");

        let spec = GroupSpec::new(4, 1, 64).unwrap();
        let mut cf = ControlFile::format(
            &io,
            Path::new(A),
            Path::new(B),
            &ws_entry(),
            &RedoEntries::new(4, 1).unwrap(),
            &ArchiveRecord::default(),
        )
        .unwrap();
        let lsn0 = bicdb_common::seq::Lsn::from_raw(0).unwrap();
        let mut writer = GroupWriter::create(&io, &mut cf, Path::new(WAL), spec, lsn0).unwrap();
        let mut resolve = resolver(undo_handle, data);
        rollback_losers(&io, &mut writer, &chain, &[slot], &mut resolve).unwrap();

        // 重复调用（陈旧列表）：跳过 Free 槽，不二次 `wrap`。
        let report = rollback_losers(&io, &mut writer, &chain, &[slot], &mut resolve).unwrap();
        assert_eq!(report.txns_rolled_back, 0);
        let s = read_slot(&chain.segment().read_page(0).unwrap(), slot).unwrap();
        assert_eq!(s.wrap, 1, "只推进过一次");
    }

    #[test]
    fn analysis_repair_and_undo_form_a_pipeline() {
        let io = mem();
        let mut file = DataFile::create(&io, Path::new(UNDO_F), 1, 1, WS, 512).unwrap();
        let undo_handle = file.handle();
        let segment = create_undo_segment(&mut file, 2, 3, 4).unwrap();
        let mut chain = UndoChain::open(segment);
        let loser_slot = chain.allocate_slot().unwrap();
        let winner_slot = chain.allocate_slot().unwrap();
        let data = pagefile::create(&io, Path::new(DATA_F), 1).unwrap();
        let n = loser_fixture(&io, &mut chain, loser_slot, data, b"epsilon");
        let loser_raw = TxnId::from_parts(0, loser_slot as u8, 0).as_raw();
        let winner_raw = TxnId::from_parts(0, winner_slot as u8, 0).as_raw();

        // 日志：输家的页修改（活动）+ 胜者的提交记录。
        let spec = GroupSpec::new(4, 1, 64).unwrap();
        let mut cf = ControlFile::format(
            &io,
            Path::new(A),
            Path::new(B),
            &ws_entry(),
            &RedoEntries::new(4, 1).unwrap(),
            &ArchiveRecord::default(),
        )
        .unwrap();
        let lsn0 = bicdb_common::seq::Lsn::from_raw(0).unwrap();
        let mut writer = GroupWriter::create(&io, &mut cf, Path::new(WAL), spec, lsn0).unwrap();
        writer
            .append(|l| {
                RedoRecord::page_modification(
                    l,
                    loser_raw,
                    vec![BlockRef {
                        flags: 0,
                        rdba: rdba(3, 0),
                        changes: vec![crate::record::Change {
                            offset: 4096,
                            after: vec![0xAB],
                        }],
                    }],
                )
            })
            .unwrap();
        writer
            .append(|l| RedoRecord::commit(l, winner_raw, 7))
            .unwrap();
        writer.flush(writer.appended_lsn()).unwrap();

        // ① 分析。
        let cf_ro = ControlFile::open(&io, Path::new(A), Path::new(B)).unwrap();
        let groups = online_groups(&io, &cf_ro, Path::new(WAL), spec).unwrap();
        let report = analyze_from(&io, &groups, lsn0).unwrap();
        assert_eq!(report.committed(), vec![(winner_raw, 7)]);
        assert_eq!(report.losers(), vec![loser_raw]);

        // ② 事务表修复：胜者补标记；输家 = 槽扫描（含"日志外的""）。
        let mut header = chain.segment().read_page(0).unwrap();
        let committed: Vec<(TxnId, CommitSeq)> = report
            .committed()
            .into_iter()
            .map(|(raw, seq)| {
                let mut b = [0u8; 6];
                b.copy_from_slice(&raw.to_le_bytes()[..6]);
                (TxnId::from_bytes(&b), CommitSeq::from_raw(seq).unwrap())
            })
            .collect();
        let losers = repair_committed_slots(&mut header, &committed).unwrap();
        chain.segment().write_page(0, &mut header).unwrap();
        assert_eq!(losers, vec![loser_slot]);
        let s = read_slot(&chain.segment().read_page(0).unwrap(), winner_slot).unwrap();
        assert_eq!(s.state, TxnState::Committed);
        assert_eq!(s.commit_seq, CommitSeq::from_raw(7).unwrap());

        // ③ 撤销：回滚输家。
        let mut resolve = resolver(undo_handle, data);
        let undo = rollback_losers(&io, &mut writer, &chain, &losers, &mut resolve).unwrap();
        assert_eq!(undo.txns_rolled_back, 1);
        let page = pagefile::read_page_verified(&io, data, 0).unwrap();
        assert_eq!(heap::row(&page, n), None);

        // ④ 重跑分析：输家已有"回滚完成"，不再出现在输家集合里。
        writer.flush(writer.appended_lsn()).unwrap();
        let cf_ro = ControlFile::open(&io, Path::new(A), Path::new(B)).unwrap();
        let groups = online_groups(&io, &cf_ro, Path::new(WAL), spec).unwrap();
        let report2 = analyze_from(&io, &groups, lsn0).unwrap();
        assert_eq!(report2.losers(), Vec::<u64>::new());
        let header = chain.segment().read_page(0).unwrap();
        assert!(repair_committed_slots(&mut header.clone(), &committed)
            .unwrap()
            .is_empty());
    }
}
