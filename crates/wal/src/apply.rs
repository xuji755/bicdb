//! 重做**应用**（§11.5.4）：恢复"重做"阶段的核心——**apply 路径唯一**。
//!
//! 无论记录的 `op` 是什么，页修改一律按"**块引用定位 → 页内字节写入**"应用；
//! 幂等性由**页上的 `page_lsn`** 统一判定：
//!
//! ```text
//! record.lsn > page.page_lsn  ⇒ 写入：逐变更写字节 → page_lsn = record.lsn
//! 否则                        ⇒ 跳过（该页已含这条修改，含乱序/重复重放）
//! ```
//!
//! # 跨块原子性由幂等重放保证
//!
//! 一条记录可有多个块引用（多块**原子**变更）。崩溃/失败发生在块与块之间时，
//! 已应用的块其 `page_lsn` 已推进、重跑时跳过；其余补上——**重放收敛**，
//! 不需要"记录级事务"。（这正是放弃 PG"每资源管理器一个 `rm_redo`"换来的
//! 回报：恢复路径只有一条。）
//!
//! # 边界
//!
//! - **页损坏按损坏处理**（§11.6 的显式取舍）：两层完整性检出失败 ⇒
//!   [`ApplyError::Damaged`]——不静默跳过、不静默返回错误数据；
//! - **不做 FPW/双写区**：本模块**不重建**撕裂页，只重放物理增量；
//! - 块定位（`rdba` → 句柄/块号）由调用方（工作区/恢复入口）经
//!   [`BlockResolver`] 提供并保证句柄已打开。

use std::io;

use bicdb_common::seq::Lsn;
use bicdb_storage::page::PAGE_SIZE;
use bicdb_storage::pagefile::{read_page_verified, write_page, PageFileError};
use bicdb_workspace::io::{FileHandle, FileIo};

use crate::record::{BlockRef, Rdba, RedoRecord};

/// 块定位器：`rdba` →（已打开的页文件句柄，块号）。
///
/// 文件角色（数据/undo/位图……）由调用方决定；返回 `None` ⇒ 无法定位
/// （数据文件缺失/未打开等）。
pub type BlockResolver<'a> = dyn FnMut(Rdba) -> Option<(FileHandle, u32)> + 'a;

/// 单条记录的应用结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ApplyReport {
    /// 本次真正写入的块数。
    pub applied: usize,
    /// 因 `page_lsn` 已越过而**跳过**的块数（该页已含这条修改）。
    pub skipped: usize,
}

impl ApplyReport {
    /// 是否什么都没改（全部跳过、或记录本就不含块修改）。
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.applied == 0
    }
}

/// 重做应用错误（**明确判定**，不静默）。
#[derive(Debug)]
pub enum ApplyError {
    /// 底层 I/O。
    Io(io::Error),
    /// 目标页损坏（页尾副本/校验和不符）——按损坏处理。
    Damaged {
        /// 块引用。
        rdba: Rdba,
    },
    /// 块无法定位（数据文件缺失/未打开）。
    Unresolved {
        /// 块引用。
        rdba: Rdba,
    },
    /// 解析到的页**身份与块引用不符**（定位器给错了文件/块）。
    IdentityMismatch {
        /// 记录里的块引用。
        expected: Rdba,
        /// 页头自述的文件号。
        found_file: u16,
        /// 页头自述的块号。
        found_block: u32,
    },
    /// 变更项越出页界（记录损坏）——在校验阶段整体拒绝，不做半套。
    ChangeOutOfBounds {
        /// 页内偏移。
        offset: u16,
        /// 写入长度。
        len: usize,
    },
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApplyError::Io(e) => write!(f, "重做应用 I/O：{e}"),
            ApplyError::Damaged { rdba } => write!(
                f,
                "重做目标页损坏（文件 {} 块 {}）——按损坏处理",
                rdba.file_id(),
                rdba.block_id()
            ),
            ApplyError::Unresolved { rdba } => write!(
                f,
                "重做块无法定位（文件 {} 块 {}）",
                rdba.file_id(),
                rdba.block_id()
            ),
            ApplyError::IdentityMismatch {
                expected,
                found_file,
                found_block,
            } => write!(
                f,
                "页身份与块引用不符：期望文件 {} 块 {}，页头自述文件 {found_file} 块 {found_block}",
                expected.file_id(),
                expected.block_id()
            ),
            ApplyError::ChangeOutOfBounds { offset, len } => {
                write!(f, "变更越出页界：偏移 {offset} + 长度 {len} > {PAGE_SIZE}")
            }
        }
    }
}

impl std::error::Error for ApplyError {}

impl From<io::Error> for ApplyError {
    fn from(e: io::Error) -> Self {
        ApplyError::Io(e)
    }
}

/// **应用一条记录的全部块修改**（幂等；见模块文档）。
pub fn apply_record(
    io: &dyn FileIo,
    record: &RedoRecord,
    resolver: &mut BlockResolver<'_>,
) -> Result<ApplyReport, ApplyError> {
    let mut report = ApplyReport::default();
    for block in &record.blocks {
        if apply_block(io, record.lsn, block, resolver)? {
            report.applied += 1;
        } else {
            report.skipped += 1;
        }
    }
    Ok(report)
}

/// 应用单个块；返回是否真正写入（`false` = 已含、跳过）。
fn apply_block(
    io: &dyn FileIo,
    record_lsn: Lsn,
    block: &BlockRef,
    resolver: &mut BlockResolver<'_>,
) -> Result<bool, ApplyError> {
    let rdba = block.rdba;
    let (handle, block_no) = resolver(rdba).ok_or(ApplyError::Unresolved { rdba })?;

    let mut page = read_page_verified(io, handle, block_no).map_err(|e| match e {
        PageFileError::Damaged { .. } => ApplyError::Damaged { rdba },
        PageFileError::Io(e) => ApplyError::Io(e),
    })?;
    let header = page.header().ok_or(ApplyError::Damaged { rdba })?;
    if header.file_id != rdba.file_id() || header.block_id != rdba.block_id() {
        return Err(ApplyError::IdentityMismatch {
            expected: rdba,
            found_file: header.file_id,
            found_block: header.block_id,
        });
    }

    // 幂等门：页已含这条（或更新的）修改 ⇒ 跳过。
    if record_lsn <= header.page_lsn {
        return Ok(false);
    }

    // 先整体校验越界（记录损坏则在写之前整体拒绝）。
    for ch in &block.changes {
        if usize::from(ch.offset) + ch.after.len() > PAGE_SIZE {
            return Err(ApplyError::ChangeOutOfBounds {
                offset: ch.offset,
                len: ch.after.len(),
            });
        }
    }

    for ch in &block.changes {
        let at = usize::from(ch.offset);
        page.as_bytes_mut()[at..at + ch.after.len()].copy_from_slice(&ch.after);
    }
    // **先应用、后收尾**：变更可以落在页头字段上（ITL 数、槽位数……），
    // 因此重读页头——只把 `page_lsn` 推到本记录、`mod_seq` 推进，
    // 其余字段以变更后的值为准。
    let mut header = page.header().ok_or(ApplyError::Damaged { rdba })?;
    header.page_lsn = record_lsn;
    page.write_header(&header);
    page.bump_mod_seq();
    write_page(io, handle, block_no, &mut page)?; // seal + 定址写
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::io::ErrorKind;
    use std::path::Path;

    use bicdb_storage::page::{Page, PageType, WORKSPACE_REF_LEN};
    use bicdb_storage::pagefile;
    use bicdb_workspace::io::{FaultInjecting, FaultOp, FaultRule, MemFileIo, OpenOptions};

    use super::*;
    use crate::record::{Change, RecordOp};

    const FILE: &str = "/mem/file1.dat";

    fn lsn(v: u64) -> Lsn {
        Lsn::from_raw(v).unwrap()
    }

    fn rdba(file: u16, block: u32) -> Rdba {
        Rdba::from_parts(file, block).unwrap()
    }

    /// 建一个 1 块页文件，块 0 是堆表页（身份 = 传入的 rdba）。
    fn make_file(io: &dyn FileIo, path: &str, file_id: u16, block_id: u32) -> FileHandle {
        let h = pagefile::create(io, Path::new(path), 1).unwrap();
        let mut page = Page::new(
            PageType::HeapTable,
            [0u8; WORKSPACE_REF_LEN],
            file_id,
            block_id,
        );
        pagefile::write_page(io, h, block_id, &mut page).unwrap();
        h
    }

    fn mod_record(lsn_v: u64, blocks: Vec<BlockRef>) -> RedoRecord {
        RedoRecord::page_modification(lsn(lsn_v), 1, blocks)
    }

    fn change(offset: u16, bytes: &[u8]) -> Change {
        Change {
            offset,
            after: bytes.to_vec(),
        }
    }

    fn block(r: Rdba, changes: Vec<Change>) -> BlockRef {
        BlockRef {
            flags: 0,
            rdba: r,
            changes,
        }
    }

    #[test]
    fn applies_bytes_and_advances_page_lsn() {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let h = make_file(&io, FILE, 1, 0);
        let rec = mod_record(1000, vec![block(rdba(1, 0), vec![change(100, &[0xAA; 8])])]);
        let mut resolver = |_: Rdba| Some((h, 0));
        let report = apply_record(&io, &rec, &mut resolver).unwrap();
        assert_eq!(report.applied, 1);
        assert_eq!(report.skipped, 0);

        let page = read_page_verified(&io, h, 0).unwrap();
        assert_eq!(&page.as_bytes()[100..108], &[0xAA; 8]);
        let header = page.header().unwrap();
        assert_eq!(header.page_lsn, lsn(1000));
        assert_eq!(header.mod_seq, 2, "mod_seq 推进（1 → 2）");
    }

    #[test]
    fn reapply_is_idempotent() {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let h = make_file(&io, FILE, 1, 0);
        let rec = mod_record(1000, vec![block(rdba(1, 0), vec![change(200, &[0x11; 4])])]);
        let mut resolver = |_: Rdba| Some((h, 0));
        assert_eq!(apply_record(&io, &rec, &mut resolver).unwrap().applied, 1);
        let before = read_page_verified(&io, h, 0).unwrap();
        // 重放同一条：跳过，页逐字节不变。
        let report = apply_record(&io, &rec, &mut resolver).unwrap();
        assert_eq!(report.skipped, 1);
        assert!(report.is_noop());
        let after = read_page_verified(&io, h, 0).unwrap();
        assert_eq!(before.as_bytes(), after.as_bytes());
    }

    #[test]
    fn older_record_is_skipped() {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let h = make_file(&io, FILE, 1, 0);
        let mut resolver = |_: Rdba| Some((h, 0));
        let newer = mod_record(2000, vec![block(rdba(1, 0), vec![change(300, &[0x22; 4])])]);
        let older = mod_record(1500, vec![block(rdba(1, 0), vec![change(300, &[0x33; 4])])]);
        assert_eq!(apply_record(&io, &newer, &mut resolver).unwrap().applied, 1);
        let report = apply_record(&io, &older, &mut resolver).unwrap();
        assert_eq!(report.skipped, 1, "乱序/较旧记录不得覆盖");
        let page = read_page_verified(&io, h, 0).unwrap();
        assert_eq!(&page.as_bytes()[300..304], &[0x22; 4]);
        assert_eq!(page.header().unwrap().page_lsn, lsn(2000));
    }

    #[test]
    fn multi_block_record_applies_all_blocks() {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let h1 = make_file(&io, FILE, 1, 0);
        let h2 = make_file(&io, "/mem/file2.dat", 2, 0);
        let rec = mod_record(
            3000,
            vec![
                block(rdba(1, 0), vec![change(100, &[1; 2])]),
                block(rdba(2, 0), vec![change(200, &[2; 2])]),
            ],
        );
        let mut resolver = |r: Rdba| match r.file_id() {
            1 => Some((h1, 0)),
            _ => Some((h2, 0)),
        };
        let report = apply_record(&io, &rec, &mut resolver).unwrap();
        assert_eq!(report.applied, 2);
        assert_eq!(
            &read_page_verified(&io, h1, 0).unwrap().as_bytes()[100..102],
            &[1; 2]
        );
        assert_eq!(
            &read_page_verified(&io, h2, 0).unwrap().as_bytes()[200..202],
            &[2; 2]
        );
    }

    #[test]
    fn partial_failure_converges_on_retry() {
        // 单块页文件 + 一次故障注入。
        let mem = MemFileIo::new();
        mem.add_dir("/mem");
        let h1 = make_file(&mem, FILE, 1, 0);
        let h2 = make_file(&mem, "/mem/file2.dat", 2, 0);
        let fio = FaultInjecting::new(mem);
        // 第二次写 = 第二个块的写盘：失败 ⇒ 第一块已应用、第二块未应用。
        fio.add_rule(FaultRule::once(FaultOp::Write, 2, ErrorKind::Other));
        let rec = mod_record(
            4000,
            vec![
                block(rdba(1, 0), vec![change(30, &[7; 2])]),
                block(rdba(2, 0), vec![change(40, &[8; 2])]),
            ],
        );
        let mut resolver = |r: Rdba| match r.file_id() {
            1 => Some((h1, 0)),
            _ => Some((h2, 0)),
        };
        assert!(apply_record(&fio, &rec, &mut resolver).is_err());
        assert_eq!(
            &read_page_verified(&fio, h1, 0).unwrap().as_bytes()[30..32],
            &[7; 2],
            "第一块已应用"
        );
        assert_ne!(
            &read_page_verified(&fio, h2, 0).unwrap().as_bytes()[40..42],
            &[8; 2],
            "第二块未应用"
        );
        // 重跑：第一块跳过、第二块补上——重放收敛。
        let report = apply_record(&fio, &rec, &mut resolver).unwrap();
        assert_eq!((report.applied, report.skipped), (1, 1));
        assert_eq!(
            &read_page_verified(&fio, h2, 0).unwrap().as_bytes()[40..42],
            &[8; 2]
        );
    }

    #[test]
    fn unresolved_and_identity_mismatch_are_errors() {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let h = make_file(&io, FILE, 1, 0);
        let rec = mod_record(5000, vec![block(rdba(1, 0), vec![change(0, &[9; 1])])]);
        // 无法定位。
        let mut none = |_: Rdba| None;
        assert!(matches!(
            apply_record(&io, &rec, &mut none),
            Err(ApplyError::Unresolved { .. })
        ));
        // 定位到了身份不符的页（rdba 说文件 2，页头自述文件 1）。
        let wrong = mod_record(5000, vec![block(rdba(2, 0), vec![change(0, &[9; 1])])]);
        let mut misrouted = |_: Rdba| Some((h, 0));
        assert!(matches!(
            apply_record(&io, &wrong, &mut misrouted),
            Err(ApplyError::IdentityMismatch { .. })
        ));
    }

    #[test]
    fn out_of_bounds_change_rejected_before_writing() {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let h = make_file(&io, FILE, 1, 0);
        let rec = mod_record(
            6000,
            vec![block(rdba(1, 0), vec![change(16000, &[0u8; 1000])])],
        );
        let mut resolver = |_: Rdba| Some((h, 0));
        assert!(matches!(
            apply_record(&io, &rec, &mut resolver),
            Err(ApplyError::ChangeOutOfBounds { .. })
        ));
        // 未写：page_lsn 未动、字节区未被污染。
        let page = read_page_verified(&io, h, 0).unwrap();
        assert_eq!(page.header().unwrap().page_lsn, lsn(0));
    }

    #[test]
    fn damaged_page_is_reported_not_skipped() {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let h = make_file(&io, FILE, 1, 0);
        // 篡改页体一个字节 ⇒ 校验和不符。
        let mut pump = Box::new([0u8; PAGE_SIZE]);
        io.read_exact_at(h, pump.as_mut_slice(), 0).unwrap();
        pump[500] ^= 0xFF;
        io.write_at(h, pump.as_slice(), 0).unwrap();
        let rec = mod_record(7000, vec![block(rdba(1, 0), vec![change(0, &[1; 1])])]);
        let mut resolver = |_: Rdba| Some((h, 0));
        assert!(matches!(
            apply_record(&io, &rec, &mut resolver),
            Err(ApplyError::Damaged { .. })
        ));
    }

    #[test]
    fn header_field_changes_survive_page_lsn_update() {
        // 变更可落在页头字段（ITL 数、槽位数、free_end……）：先应用、后重读页头
        // 只推进 page_lsn/mod_seq——头字段变更不得被写回覆盖。
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let h = make_file(&io, FILE, 1, 0);
        let rec = mod_record(
            9000,
            vec![block(
                rdba(1, 0),
                vec![change(
                    bicdb_storage::page::FREE_END_OFFSET as u16,
                    &1234u16.to_le_bytes(),
                )],
            )],
        );
        let mut resolver = |_: Rdba| Some((h, 0));
        assert_eq!(apply_record(&io, &rec, &mut resolver).unwrap().applied, 1);
        let page = read_page_verified(&io, h, 0).unwrap();
        let header = page.header().unwrap();
        assert_eq!(header.free_end, 1234, "头字段变更保留");
        assert_eq!(header.page_lsn, lsn(9000));
        assert_eq!(header.file_id, 1, "其余头字段不被破坏");
    }

    #[test]
    fn record_without_blocks_is_a_noop() {
        let io = MemFileIo::new();
        let rec = RedoRecord::commit(lsn(8000), 1, 1);
        let mut resolver = |_: Rdba| None;
        let report = apply_record(&io, &rec, &mut resolver).unwrap();
        assert!(report.is_noop());
        assert_eq!(rec.op, RecordOp::Commit.as_u8());
        // 打开选项未被使用（编译期证明 resolver 不被调用即可）。
        let _ = OpenOptions::new().read(true);
    }
}
