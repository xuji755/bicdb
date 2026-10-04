//! redo 页（512 B，`page_type = 13`）与**分片**（存储架构 §5.11，已定案）。
//!
//! ```text
//! 偏移 0    校验       4B   CRC32C 覆盖整页（本字段置零后计算）
//! 偏移 4    页位置     6B   本页在日志中的**起始 LSN**（恢复时校验顺序）
//! 偏移 10   已用长度   2B   页内有效字节数（自页体起算）——恢复扫描的终止依据
//! 偏移 12   flags      1B
//! 偏移 13   reserved   1B
//! 偏移 14   reserved   2B
//! 偏移 16   页体：记录流（线性打包，**记录可跨页**）
//!
//! 分片头（每片都带，12B）
//!   rec_id   6B   记录标识（= 首片的 LSN）
//!   frag_no  2B   本片序号（自 0 起）
//!   frag_cnt 2B   分片总数
//!   frag_len 2B   本片数据长度
//! ```
//!
//! # 为什么 redo 页小（512 B）
//!
//! redo 页是**提交关键路径**上唯一必须持久化的东西——页越小，追加与刷新的
//! 粒度越细；**对齐文件系统原子写单位**后，**日志末尾永远不会是半截页**
//! （Oracle 同理：redo 块 = OS block size）。而"分片"要解决的不是性能
//! （我们的追加本就 latch 串行化），而是**原子性**：让系统操作的大记录
//! 也能跨页，同时保持"单条原子"。
//!
//! # LSN 约定
//!
//! **LSN = 日志文件中的字节位置（页头与分片头都占位）**——可推导（"位置
//! 可推导"）；首片位置即记录的 LSN
//! （= `rec_id` = 记录头的 `lsn` 字段）。其余分片的位置可推导，**不存**。
//! **一条记录不得跨越文件边界**（§11.5.1）——文件层接入时落实。

use bicdb_common::checksum::{checksum_with_zeroed_field, verify_zeroed_field_checksum};
use bicdb_common::seq::Lsn;

use crate::record::{RecordError, RedoRecord};

/// redo 页大小（对齐文件系统原子写单位；§5.11 已定 512 B）。
pub const LOG_PAGE_SIZE: usize = 512;

/// redo 页页头长度（16B）。
pub const LOG_PAGE_HEADER_LEN: usize = 16;

/// 分片头长度（12B）。
pub const FRAGMENT_HEADER_LEN: usize = 12;

/// redo 页的校验字段（偏移与宽度）。
pub const CHECKSUM_OFFSET: usize = 0;
/// 见 [`CHECKSUM_OFFSET`]。
pub const CHECKSUM_LEN: usize = 4;

const _: () = {
    // 页头 + 分片头 + 至少 1 字节数据 ≤ 页大小（否则分片无处落）。
    assert!(LOG_PAGE_HEADER_LEN + FRAGMENT_HEADER_LEN < LOG_PAGE_SIZE);
    // 最坏单条记录（16417B）跨 34 页 = 17 KiB：组大小下限的定量（§11.5.2）。
    assert!(LOG_PAGE_SIZE == 512);
};

/// redo 页操作错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogPageError {
    /// 本页放不下该分片（须换页）。
    PageFull,
    /// 本页校验和不符。
    BadChecksum,
    /// 页内结构非法（已用长度越界 / 分片头与数据不符）。
    Malformed,
}

impl std::fmt::Display for LogPageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            LogPageError::PageFull => "redo 页放不下该分片",
            LogPageError::BadChecksum => "redo 页校验和不符",
            LogPageError::Malformed => "redo 页结构非法",
        })
    }
}

impl std::error::Error for LogPageError {}

/// 一个分片（分片头 + 数据）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fragment {
    /// 记录标识（= 首片 LSN）。
    pub rec_id: Lsn,
    /// 本片序号（自 0 起）。
    pub frag_no: u16,
    /// 分片总数。
    pub frag_cnt: u16,
    /// 本片数据。
    pub data: Vec<u8>,
}

/// 一页 redo（512 B）。
#[derive(Clone)]
pub struct LogPage {
    bytes: Box<[u8; LOG_PAGE_SIZE]>,
}

impl std::fmt::Debug for LogPage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogPage")
            .field("start_lsn", &self.start_lsn())
            .field("used", &self.used())
            .field("verify", &self.verify().is_ok())
            .finish()
    }
}

impl LogPage {
    /// 新建以 `start_lsn` 为起始位置的空页（尚未 seal）。
    #[must_use]
    pub fn new(start_lsn: Lsn) -> Self {
        let mut bytes = Box::new([0u8; LOG_PAGE_SIZE]);
        bytes[4..10].copy_from_slice(&start_lsn.as_raw().to_le_bytes()[..6]);
        // 已用长度 = 0；flags/reserved = 0。
        Self { bytes }
    }

    /// 由既有字节（如从磁盘读入）构造。
    #[must_use]
    pub fn from_bytes(bytes: Box<[u8; LOG_PAGE_SIZE]>) -> Self {
        Self { bytes }
    }

    /// 原始字节。
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; LOG_PAGE_SIZE] {
        &self.bytes
    }

    /// 本页起始 LSN。
    #[must_use]
    pub fn start_lsn(&self) -> Lsn {
        let mut b = [0u8; 8];
        b[..6].copy_from_slice(&self.bytes[4..10]);
        Lsn::from_raw(u64::from_le_bytes(b)).expect("6 字节在 48 位域内")
    }

    /// 已用字节数（自页体起算）。
    #[must_use]
    pub fn used(&self) -> usize {
        u16::from_le_bytes(self.bytes[10..12].try_into().expect("2 字节")) as usize
    }

    fn set_used(&mut self, used: usize) {
        self.bytes[10..12].copy_from_slice(&(used as u16).to_le_bytes());
    }

    /// 本页还能容纳的数据字节数（扣除分片头；< 1 即"无法再放分片"）。
    #[must_use]
    pub fn remaining_data_capacity(&self) -> usize {
        LOG_PAGE_SIZE.saturating_sub(LOG_PAGE_HEADER_LEN + self.used() + FRAGMENT_HEADER_LEN)
    }

    /// 追加一个分片；放不下返回 [`LogPageError::PageFull`]。
    pub fn append_fragment(&mut self, frag: &Fragment) -> Result<(), LogPageError> {
        if frag.data.len() > self.remaining_data_capacity() {
            return Err(LogPageError::PageFull);
        }
        let at = LOG_PAGE_HEADER_LEN + self.used();
        let mut hdr = [0u8; FRAGMENT_HEADER_LEN];
        hdr[..6].copy_from_slice(&frag.rec_id.as_raw().to_le_bytes()[..6]);
        hdr[6..8].copy_from_slice(&frag.frag_no.to_le_bytes());
        hdr[8..10].copy_from_slice(&frag.frag_cnt.to_le_bytes());
        hdr[10..12].copy_from_slice(&(frag.data.len() as u16).to_le_bytes());
        self.bytes[at..at + FRAGMENT_HEADER_LEN].copy_from_slice(&hdr);
        self.bytes[at + FRAGMENT_HEADER_LEN..at + FRAGMENT_HEADER_LEN + frag.data.len()]
            .copy_from_slice(&frag.data);
        self.set_used(self.used() + FRAGMENT_HEADER_LEN + frag.data.len());
        Ok(())
    }

    /// 页内分片（按写入顺序；结构非法即失败）。
    pub fn fragments(&self) -> Result<Vec<Fragment>, LogPageError> {
        let mut out = Vec::new();
        let mut at = 0usize;
        while at < self.used() {
            let base = LOG_PAGE_HEADER_LEN + at;
            if base + FRAGMENT_HEADER_LEN > LOG_PAGE_SIZE {
                return Err(LogPageError::Malformed);
            }
            let mut b = [0u8; 8];
            b[..6].copy_from_slice(&self.bytes[base..base + 6]);
            let rec_id = Lsn::from_raw(u64::from_le_bytes(b)).expect("48 位域内");
            let frag_no = u16::from_le_bytes(self.bytes[base + 6..base + 8].try_into().expect("2"));
            let frag_cnt =
                u16::from_le_bytes(self.bytes[base + 8..base + 10].try_into().expect("2"));
            let frag_len =
                u16::from_le_bytes(self.bytes[base + 10..base + 12].try_into().expect("2"))
                    as usize;
            let data_at = base + FRAGMENT_HEADER_LEN;
            if data_at + frag_len > LOG_PAGE_SIZE {
                return Err(LogPageError::Malformed);
            }
            out.push(Fragment {
                rec_id,
                frag_no,
                frag_cnt,
                data: self.bytes[data_at..data_at + frag_len].to_vec(),
            });
            at += FRAGMENT_HEADER_LEN + frag_len;
        }
        if at != self.used() {
            return Err(LogPageError::Malformed);
        }
        Ok(out)
    }

    /// 收尾：重算校验和（写盘前调用；`used` 字段已随追加维护）。
    pub fn seal(&mut self) {
        let sum = checksum_with_zeroed_field(&self.bytes[..], CHECKSUM_OFFSET, CHECKSUM_LEN);
        self.bytes[CHECKSUM_OFFSET..CHECKSUM_OFFSET + CHECKSUM_LEN]
            .copy_from_slice(&sum.to_le_bytes());
    }

    /// 仅供测试：可变字节入口（篡改用）。
    #[cfg(test)]
    pub(crate) fn as_bytes_mut_for_test(&mut self) -> &mut [u8; LOG_PAGE_SIZE] {
        &mut self.bytes
    }

    /// 校验（CRC32C 覆盖整页）。
    pub fn verify(&self) -> Result<(), LogPageError> {
        if verify_zeroed_field_checksum(&self.bytes[..], CHECKSUM_OFFSET, CHECKSUM_LEN) {
            Ok(())
        } else {
            Err(LogPageError::BadChecksum)
        }
    }
}

/// 把一条记录**分片**写入页序列（继续写 `pages` 的最后一页，满则新建页）。
///
/// - 分片除末片外**满装**（填到页的剩余容量）；
/// - 新页的起始 LSN = 上一页起始 LSN + 512（日志是致密字节流）；
/// - `record.lsn` 即首片位置（`rec_id`），与记录头的 `lsn` 字段一致。
pub fn write_record(pages: &mut Vec<LogPage>, record: &RedoRecord) -> Result<(), LogPageError> {
    let bytes = record.encode();
    let data_len = bytes.len();
    let full_capacity = LOG_PAGE_SIZE - LOG_PAGE_HEADER_LEN - FRAGMENT_HEADER_LEN;

    // 先定分片尺寸：先填当前页剩余，其后每片满装（除末片）。
    let mut sizes: Vec<usize> = Vec::new();
    let mut remaining = data_len;
    if let Some(p) = pages.last() {
        let cap = p.remaining_data_capacity();
        if cap > 0 && remaining > 0 {
            let take = cap.min(remaining);
            sizes.push(take);
            remaining -= take;
        }
    }
    while remaining > 0 {
        let take = full_capacity.min(remaining);
        sizes.push(take);
        remaining -= take;
    }

    let frag_cnt = sizes.len() as u16;
    debug_assert!(frag_cnt >= 1, "记录非空 ⇒ 至少一片");
    let mut offset = 0usize;
    for (frag_no, take) in sizes.into_iter().enumerate() {
        let need_new_page = match pages.last() {
            None => true,
            Some(p) => p.remaining_data_capacity() == 0,
        };
        if need_new_page {
            let start = match pages.last() {
                Some(p) => Lsn::from_raw(p.start_lsn().as_raw() + LOG_PAGE_SIZE as u64)
                    .expect("LSN 在 48 位域内"),
                None => {
                    // 空序列：本记录的 LSN 是**首片的字节位置**（含页头），
                    // 故页起点 = 首片位置 − 页头，且必须页对齐。
                    let raw = record.lsn.as_raw();
                    let start = raw.checked_sub(LOG_PAGE_HEADER_LEN as u64);
                    match start {
                        Some(s) if s % LOG_PAGE_SIZE as u64 == 0 => {
                            Lsn::from_raw(s).expect("48 位域内")
                        }
                        _ => return Err(LogPageError::PageFull),
                    }
                }
            };
            pages.push(LogPage::new(start));
        }
        let page = pages.last_mut().expect("刚保证存在");
        page.append_fragment(&Fragment {
            rec_id: record.lsn,
            frag_no: frag_no as u16,
            frag_cnt,
            data: bytes[offset..offset + take].to_vec(),
        })?;
        offset += take;
    }
    debug_assert_eq!(offset, data_len);
    Ok(())
}

/// 恢复扫描的终止状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailState {
    /// 干净结束（末尾正好是完整记录/空页）。
    Clean,
    /// **末尾不完整**：分片序列被截断——整条记录**丢弃**（与"日志末尾
    /// 不完整就停"同理）。
    Truncated,
}

/// 从页序列重组全部记录；返回（记录, 终止状态）。
///
/// 防线：分片按 `rec_id` 归类、`frag_no` 连续、`frag_cnt` 一致、
/// 首片的 `rec_id` 必须等于重组后记录头的 `lsn`（互为校验）。
pub fn decode_records(pages: &[LogPage]) -> (Vec<RedoRecord>, TailState, Vec<RecordError>) {
    let mut records = Vec::new();
    let mut errors = Vec::new();
    let mut pending: Option<(Lsn, u16, Vec<u8>)> = None; // (rec_id, 已收片数, 数据)
    let mut tail = TailState::Clean;

    for page in pages {
        let frags = match page.fragments() {
            Ok(f) => f,
            Err(_) => {
                tail = TailState::Truncated;
                break;
            }
        };
        for frag in frags {
            match &mut pending {
                Some((rec_id, count, data)) if *rec_id == frag.rec_id => {
                    if frag.frag_no != *count {
                        // 分片序号断裂：丢弃当前记录，从此不完整。
                        pending = None;
                        tail = TailState::Truncated;
                        break;
                    }
                    data.extend_from_slice(&frag.data);
                    *count += 1;
                    if *count == frag.frag_cnt {
                        let (id, _, bytes) = pending.take().expect("在途");
                        decode_one(id, &bytes, &mut records, &mut errors, &mut tail);
                    }
                }
                Some(_) => {
                    // 上一记录未收完却出现新 rec_id：截断。
                    pending = None;
                    tail = TailState::Truncated;
                    break;
                }
                None => {
                    if frag.frag_no != 0 {
                        // 从中间开始：头部缺失，视作截断。
                        tail = TailState::Truncated;
                        break;
                    }
                    let mut data = Vec::new();
                    data.extend_from_slice(&frag.data);
                    if frag.frag_cnt == 1 {
                        decode_one(frag.rec_id, &data, &mut records, &mut errors, &mut tail);
                    } else {
                        pending = Some((frag.rec_id, 1, data));
                    }
                }
            }
        }
        if tail == TailState::Truncated {
            break;
        }
    }
    if pending.is_some() {
        tail = TailState::Truncated; // 末尾缺片
    }
    (records, tail, errors)
}

fn decode_one(
    rec_id: Lsn,
    bytes: &[u8],
    records: &mut Vec<RedoRecord>,
    errors: &mut Vec<RecordError>,
    tail: &mut TailState,
) {
    match RedoRecord::decode(bytes) {
        Ok(rec) if rec.lsn == rec_id => records.push(rec),
        Ok(_) => {
            errors.push(RecordError::LsnMismatch);
            *tail = TailState::Truncated;
        }
        Err(e) => {
            errors.push(e);
            *tail = TailState::Truncated;
        }
    }
}

/// 单条记录的编码长度 → 需要的分片数与页数（诊断/组大小规划用）。
#[must_use]
pub fn pages_needed_for_record(record: &RedoRecord) -> usize {
    let mut pages: Vec<LogPage> = Vec::new();
    write_record(&mut pages, record).expect("空序列必成功");
    pages.len()
}

/// 最坏单条记录（重写整页）的页数——§11.5.2 定量的机械校验。
#[must_use]
pub const fn worst_case_pages() -> usize {
    // 16417B 记录：首片 484B（512−16−12），其后每片 484B。
    16_417usize.div_ceil(LOG_PAGE_SIZE - LOG_PAGE_HEADER_LEN - FRAGMENT_HEADER_LEN)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{BlockRef, Change, Rdba, RECORD_HEADER_LEN};

    fn lsn(v: u64) -> Lsn {
        Lsn::from_raw(v).unwrap()
    }

    fn big_record(lsn_v: u64, payload: usize) -> RedoRecord {
        RedoRecord::page_modification(
            lsn(lsn_v),
            7,
            vec![BlockRef {
                flags: 0,
                rdba: Rdba::from_parts(1, 5).unwrap(),
                changes: vec![Change {
                    offset: 0,
                    after: vec![0x5A; payload],
                }],
            }],
        )
    }

    #[test]
    fn single_fragment_record_roundtrip() {
        let rec = RedoRecord::commit(lsn(16), 3, 42);
        let mut pages = Vec::new();
        write_record(&mut pages, &rec).unwrap();
        assert_eq!(pages.len(), 1);
        let page = &pages[0];
        assert_eq!(page.start_lsn(), lsn(0), "页 0 起点 = 首片位置 − 16");
        assert_eq!(page.used(), FRAGMENT_HEADER_LEN + rec.encoded_len());
        for p in pages.iter_mut() {
            p.seal();
        }
        assert_eq!(pages[0].verify(), Ok(()));

        let (records, tail, errors) = decode_records(&pages);
        assert_eq!(tail, TailState::Clean);
        assert!(errors.is_empty());
        assert_eq!(records, vec![rec]);
    }

    #[test]
    fn multi_page_record_reassembles_exactly() {
        // 3 页量级的记录。
        let rec = big_record(512 + 16, 1200);
        let mut pages = Vec::new();
        write_record(&mut pages, &rec).unwrap();
        assert!(
            pages.len() >= 3,
            "1200B 载荷跨多页，实际 {} 页",
            pages.len()
        );
        for p in pages.iter_mut() {
            p.seal();
        }
        // 每页起始 LSN 逐一递进 512。
        for w in pages.windows(2) {
            assert_eq!(w[1].start_lsn().as_raw(), w[0].start_lsn().as_raw() + 512);
        }
        let (records, tail, errors) = decode_records(&pages);
        assert_eq!(tail, TailState::Clean);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(records, vec![rec]);
    }

    #[test]
    fn worst_case_record_takes_thirty_four_pages() {
        let rec = big_record(16, 16 * 1024); // 最坏：重写整页
        assert_eq!(rec.encoded_len(), 16_417);
        assert_eq!(pages_needed_for_record(&rec), 34, "§11.5.2 的定量");
        assert_eq!(worst_case_pages(), 34);
        assert_eq!(34 * LOG_PAGE_SIZE, 17 * 1024, "组大小下限 ≥ 17 KiB");
    }

    #[test]
    fn truncated_tail_drops_the_incomplete_record() {
        let rec1 = RedoRecord::commit(lsn(16), 1, 11);
        let rec2 = big_record(1024, 900);
        let mut pages = Vec::new();
        write_record(&mut pages, &rec1).unwrap();
        write_record(&mut pages, &rec2).unwrap();
        for p in pages.iter_mut() {
            p.seal();
        }
        // 丢掉最后一页 → 第二条记录不完整。
        let truncated = &pages[..pages.len() - 1];
        let (records, tail, _) = decode_records(truncated);
        assert_eq!(tail, TailState::Truncated);
        assert_eq!(
            records,
            vec![rec1.clone()],
            "完整记录保留、残缺记录整条丢弃"
        );

        // 完整序列则两条都在。
        let (records, tail, errors) = decode_records(&pages);
        assert_eq!(tail, TailState::Clean);
        assert!(errors.is_empty());
        assert_eq!(records, vec![rec1, rec2]);
    }

    #[test]
    fn tampered_page_fails_checksum() {
        let rec = RedoRecord::commit(lsn(16), 1, 1);
        let mut pages = Vec::new();
        write_record(&mut pages, &rec).unwrap();
        pages[0].seal();
        assert_eq!(pages[0].verify(), Ok(()));
        pages[0].as_bytes_mut_for_test()[100] ^= 0xFF;
        assert_eq!(pages[0].verify(), Err(LogPageError::BadChecksum));
    }

    #[test]
    fn changing_lsn_must_match_rec_id() {
        // 伪造：分片 rec_id 与记录头 lsn 不一致 → 解码报错并判截断。
        let rec = RedoRecord::commit(lsn(64), 1, 1);
        let bytes = rec.encode();
        let mut page = LogPage::new(lsn(0));
        page.append_fragment(&Fragment {
            rec_id: lsn(128), // 撒谎
            frag_no: 0,
            frag_cnt: 1,
            data: bytes,
        })
        .unwrap();
        page.seal();
        let (records, tail, errors) = decode_records(&[page]);
        assert!(records.is_empty());
        assert_eq!(tail, TailState::Truncated);
        assert!(!errors.is_empty());
    }

    #[test]
    fn page_capacity_and_full_behaviour() {
        let mut page = LogPage::new(lsn(0));
        assert_eq!(page.remaining_data_capacity(), 512 - 16 - 12);
        page.append_fragment(&Fragment {
            rec_id: lsn(0),
            frag_no: 0,
            frag_cnt: 1,
            data: vec![0; 484],
        })
        .unwrap();
        assert_eq!(page.remaining_data_capacity(), 0);
        assert_eq!(
            page.append_fragment(&Fragment {
                rec_id: lsn(0),
                frag_no: 1,
                frag_cnt: 2,
                data: vec![1],
            }),
            Err(LogPageError::PageFull)
        );
    }

    #[test]
    fn empty_payload_record_roundtrip_through_pages() {
        let rec = RedoRecord::rollback_done(lsn(16), 5);
        let mut pages = Vec::new();
        write_record(&mut pages, &rec).unwrap();
        for p in pages.iter_mut() {
            p.seal();
        }
        let (records, tail, errors) = decode_records(&pages);
        assert_eq!(tail, TailState::Clean);
        assert!(errors.is_empty());
        assert_eq!(records, vec![rec]);
        assert_eq!(records[0].encoded_len(), RECORD_HEADER_LEN);
    }
}
