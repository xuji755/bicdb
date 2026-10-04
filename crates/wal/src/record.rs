//! 重做记录（redo record）的字节格式（存储架构 §11.5.2/§11.5.4，均已定案）。
//!
//! ```text
//! 记录头（20 字节，固定；**不带 CRC**——§14 第 46 项）
//!   tot_len   4B   整条记录总长（含本头，**不含分片帧**）
//!   lsn       6B   本条记录的 LSN（= 首片位置；同时是"应用后该页的 page_lsn"）
//!   txn_id    6B   产生者（0 = 系统操作）
//!   op        1B   语义标签（**不参与 apply**；见 [`RecordOp`]）
//!   flags     1B   bit0 = 存在块段，bit1 = 存在主段（**全 0 = 空载荷记录**，
//!                  如"回滚完成"——信息全在头部）
//!   reserved  2B   对齐至 20B
//! 块段（页修改类）：blk_cnt 1B
//!   块引用[]：flags 1B │ rdba 5B │ chg_cnt 2B（每项 8B）
//!   块数据[]：按块顺序、每块 chg_cnt 项：offset 2B │ len 2B │ after_bytes
//! 主段（非页修改类）：payload——**无长度字段**，长度 = tot_len − 20 − 块段长
//! ```
//!
//! 三条纪律：
//! 1. **只存 after-image**——前像在 undo 里（§11.1.2）；
//! 2. **能推导的不存**：主段长度、块数、每块变更数都由结构本身给出；
//! 3. **`op` 只是标签**：apply 路径唯一（块引用定位 → 页内字节写入，
//!    幂等性由 `page_lsn` 判定）——`op` 供诊断、统计与组提交识别。

use bicdb_common::seq::Lsn;

/// 记录头长度（固定 20 字节）。
pub const RECORD_HEADER_LEN: usize = 20;

/// `flags` 位：存在块段。
pub const FLAG_BLOCK_SEGMENT: u8 = 1 << 0;
/// `flags` 位：存在主段。
pub const FLAG_MAIN_SEGMENT: u8 = 1 << 1;

/// 块引用长度。
pub const BLOCK_REF_LEN: usize = 8;
/// 变更项（offset 2B │ len 2B）头长度（after 字节另计）。
pub const CHANGE_HEADER_LEN: usize = 4;

/// `op` 取值（§11.5.4；**不参与 apply**，只用于诊断/统计/组提交识别）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordOp {
    /// 0x01 日志切换 / 文件头（主段：组号 1B │ 新日志序列号 4B）。
    LogSwitch = 0x01,
    /// 0x02 检查点（主段：与控制文件"检查点进度"同构的 24B）。
    Checkpoint = 0x02,
    /// 0x10 页修改（通用；用块段，无主段）。
    PageModification = 0x10,
    /// 0x30 提交（主段：`commit_seq` 6B）。
    Commit = 0x30,
    /// 0x31 回滚完成（主段空——信息全在头部 `txn_id`）。
    RollbackDone = 0x31,
}

impl RecordOp {
    /// 由字节解码（未列入者返回 `None`——解码不因此失败，`op` 原样保留）。
    #[must_use]
    pub const fn from_u8(raw: u8) -> Option<Self> {
        match raw {
            0x01 => Some(RecordOp::LogSwitch),
            0x02 => Some(RecordOp::Checkpoint),
            0x10 => Some(RecordOp::PageModification),
            0x30 => Some(RecordOp::Commit),
            0x31 => Some(RecordOp::RollbackDone),
            _ => None,
        }
    }

    /// 字节值。
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

/// 数据块地址（rdba，5 字节）：`file_id` 10 位 │ `block_id` 28 位。
///
/// 比 PG 的 `RelFileLocator`(12B) + `BlockNumber`(4B) 省 11 字节——
/// ROWID 的两段本来就够用（§11.5.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Rdba(u64);

impl Rdba {
    /// 由两段构造；越界拒绝。
    pub fn from_parts(file_id: u16, block_id: u32) -> Option<Self> {
        if file_id > 1023 || block_id > (1 << 28) - 1 {
            return None;
        }
        Some(Self((u64::from(file_id) << 28) | u64::from(block_id)))
    }

    /// 文件号（10 位）。
    #[must_use]
    pub fn file_id(self) -> u16 {
        (self.0 >> 28) as u16
    }

    /// 块号（28 位）。
    #[must_use]
    pub fn block_id(self) -> u32 {
        (self.0 & ((1 << 28) - 1)) as u32
    }

    /// 5 字节小端编码。
    #[must_use]
    pub fn to_bytes(self) -> [u8; 5] {
        let b = self.0.to_le_bytes();
        [b[0], b[1], b[2], b[3], b[4]]
    }

    /// 由 5 字节小端解码。
    #[must_use]
    pub fn from_bytes(bytes: &[u8; 5]) -> Self {
        let mut b = [0u8; 8];
        b[..5].copy_from_slice(bytes);
        Self(u64::from_le_bytes(b))
    }
}

/// 一处页内变更（after-image：`offset` 起写入 `len` 字节）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// 页内偏移。
    pub offset: u16,
    /// 写入字节（长度即 `len`）。
    pub after: Vec<u8>,
}

/// 一个块的引用 + 其变更集。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockRef {
    /// 块引用标志（保留；将来 FPW 标志位）。
    pub flags: u8,
    /// 块地址。
    pub rdba: Rdba,
    /// 变更项。
    pub changes: Vec<Change>,
}

/// 重做记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedoRecord {
    /// 本条记录的 LSN（= 首片位置）。
    pub lsn: Lsn,
    /// 产生者事务（0 = 系统操作）。
    pub txn_id: u64,
    /// 语义标签（原样保留未知取值）。
    pub op: u8,
    /// 块段（页修改类）。
    pub blocks: Vec<BlockRef>,
    /// 主段载荷（非页修改类；长度由 `tot_len` 推导）。
    pub main: Vec<u8>,
}

/// 记录编解码错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordError {
    /// 总长与实际字节数不符。
    LengthMismatch,
    /// 字节流截断（头 / 块段 / 变更中途）。
    Truncated,
    /// `flags` 与分段存在性不一致。
    FlagsMismatch,
    /// `txn_id` 越过 48 位域。
    TxnIdOutOfRange,
    /// 结构性字段越界（blk_cnt / chg_cnt / offset+len）。
    FieldOutOfRange,
    /// 分片 `rec_id` 与记录头 `lsn` 不一致（互为校验失败）。
    LsnMismatch,
}

impl std::fmt::Display for RecordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            RecordError::LengthMismatch => "记录总长与字节数不符",
            RecordError::Truncated => "记录字节流截断",
            RecordError::FlagsMismatch => "flags 与分段存在性不一致",
            RecordError::TxnIdOutOfRange => "txn_id 越过 48 位域",
            RecordError::FieldOutOfRange => "记录结构性字段越界",
            RecordError::LsnMismatch => "分片 rec_id 与记录头 lsn 不一致",
        })
    }
}

impl std::error::Error for RecordError {}

impl RedoRecord {
    /// 页修改记录（块段，无主段）。
    #[must_use]
    pub fn page_modification(lsn: Lsn, txn_id: u64, blocks: Vec<BlockRef>) -> Self {
        Self {
            lsn,
            txn_id,
            op: RecordOp::PageModification.as_u8(),
            blocks,
            main: Vec::new(),
        }
    }

    /// 提交记录（主段 = `commit_seq` 6B）。
    #[must_use]
    pub fn commit(lsn: Lsn, txn_id: u64, commit_seq: u64) -> Self {
        let mut main = Vec::with_capacity(6);
        main.extend_from_slice(&commit_seq.to_le_bytes()[..6]);
        Self {
            lsn,
            txn_id,
            op: RecordOp::Commit.as_u8(),
            blocks: Vec::new(),
            main,
        }
    }

    /// 回滚完成（主段空）。
    #[must_use]
    pub fn rollback_done(lsn: Lsn, txn_id: u64) -> Self {
        Self {
            lsn,
            txn_id,
            op: RecordOp::RollbackDone.as_u8(),
            blocks: Vec::new(),
            main: Vec::new(),
        }
    }

    /// 日志切换（主段 = 组号 1B │ 新日志序列号 4B）。
    #[must_use]
    pub fn log_switch(lsn: Lsn, group: u8, new_log_seq: u32) -> Self {
        let mut main = Vec::with_capacity(5);
        main.push(group);
        main.extend_from_slice(&new_log_seq.to_le_bytes());
        Self {
            lsn,
            txn_id: 0,
            op: RecordOp::LogSwitch.as_u8(),
            blocks: Vec::new(),
            main,
        }
    }

    /// 检查点（主段 = 24B 四段提交序号/LSN）。
    #[must_use]
    pub fn checkpoint(
        lsn: Lsn,
        checkpoint_commit_seq: u64,
        checkpoint_lsn: u64,
        current_commit_seq: u64,
        oldest_snapshot_commit_seq: u64,
    ) -> Self {
        let mut main = Vec::with_capacity(24);
        for v in [
            checkpoint_commit_seq,
            checkpoint_lsn,
            current_commit_seq,
            oldest_snapshot_commit_seq,
        ] {
            main.extend_from_slice(&v.to_le_bytes()[..6]);
        }
        Self {
            lsn,
            txn_id: 0,
            op: RecordOp::Checkpoint.as_u8(),
            blocks: Vec::new(),
            main,
        }
    }

    /// 主段解析：提交记录的 `commit_seq`。
    #[must_use]
    pub fn commit_seq(&self) -> Option<u64> {
        if self.op != RecordOp::Commit.as_u8() || self.main.len() < 6 {
            return None;
        }
        let mut b = [0u8; 8];
        b[..6].copy_from_slice(&self.main[..6]);
        Some(u64::from_le_bytes(b))
    }

    /// 编码（记录头 + 块段 + 主段；**不含分片帧**）。
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let block_segment_len: usize = if self.blocks.is_empty() {
            0
        } else {
            1 + self.blocks.len() * BLOCK_REF_LEN
                + self
                    .blocks
                    .iter()
                    .map(|b| {
                        b.changes
                            .iter()
                            .map(|c| CHANGE_HEADER_LEN + c.after.len())
                            .sum::<usize>()
                    })
                    .sum::<usize>()
        };
        let tot_len = RECORD_HEADER_LEN + block_segment_len + self.main.len();
        let mut flags = 0u8;
        if !self.blocks.is_empty() {
            flags |= FLAG_BLOCK_SEGMENT;
        }
        if !self.main.is_empty() {
            flags |= FLAG_MAIN_SEGMENT;
        }

        let mut out = Vec::with_capacity(tot_len);
        out.extend_from_slice(&(tot_len as u32).to_le_bytes());
        out.extend_from_slice(&self.lsn.as_raw().to_le_bytes()[..6]);
        out.extend_from_slice(&self.txn_id.to_le_bytes()[..6]);
        out.push(self.op);
        out.push(flags);
        out.extend_from_slice(&[0u8; 2]); // reserved
        debug_assert_eq!(out.len(), RECORD_HEADER_LEN);

        if !self.blocks.is_empty() {
            out.push(self.blocks.len() as u8);
            for b in &self.blocks {
                out.push(b.flags);
                out.extend_from_slice(&b.rdba.to_bytes());
                out.extend_from_slice(&(b.changes.len() as u16).to_le_bytes());
            }
            for b in &self.blocks {
                for c in &b.changes {
                    out.extend_from_slice(&c.offset.to_le_bytes());
                    out.extend_from_slice(&(c.after.len() as u16).to_le_bytes());
                    out.extend_from_slice(&c.after);
                }
            }
        }
        out.extend_from_slice(&self.main);
        debug_assert_eq!(out.len(), tot_len);
        out
    }

    /// 解码（严格：总长、`flags` 与分段存在性、各字段界）。
    pub fn decode(bytes: &[u8]) -> Result<Self, RecordError> {
        if bytes.len() < RECORD_HEADER_LEN {
            return Err(RecordError::Truncated);
        }
        let tot_len = u32::from_le_bytes(bytes[0..4].try_into().expect("4 字节")) as usize;
        if tot_len != bytes.len() {
            return Err(RecordError::LengthMismatch);
        }
        let mut lsn_b = [0u8; 8];
        lsn_b[..6].copy_from_slice(&bytes[4..10]);
        let lsn = Lsn::from_raw(u64::from_le_bytes(lsn_b)).expect("6 字节在 48 位域内");
        let mut txn_b = [0u8; 8];
        txn_b[..6].copy_from_slice(&bytes[10..16]);
        let txn_id = u64::from_le_bytes(txn_b);
        let op = bytes[16];
        let flags = bytes[17];

        let mut at = RECORD_HEADER_LEN;
        let mut blocks = Vec::new();
        if flags & FLAG_BLOCK_SEGMENT != 0 {
            if at >= bytes.len() {
                // 总长自洽却宣称有块段——flags 撒谎，不是"流截断"。
                return Err(RecordError::FlagsMismatch);
            }
            let blk_cnt = bytes[at] as usize;
            at += 1;
            if blk_cnt == 0 {
                return Err(RecordError::FlagsMismatch); // 置了位却没有块
            }
            let refs_end = at + blk_cnt * BLOCK_REF_LEN;
            if refs_end > bytes.len() {
                return Err(RecordError::Truncated);
            }
            let mut refs = Vec::with_capacity(blk_cnt);
            for i in 0..blk_cnt {
                let base = at + i * BLOCK_REF_LEN;
                let bflags = bytes[base];
                let rdba = Rdba::from_bytes(&bytes[base + 1..base + 6].try_into().expect("5 字节"));
                let chg_cnt =
                    u16::from_le_bytes(bytes[base + 6..base + 8].try_into().expect("2 字节"));
                refs.push((bflags, rdba, chg_cnt));
            }
            at = refs_end;
            for (bflags, rdba, chg_cnt) in refs {
                let mut changes = Vec::with_capacity(chg_cnt as usize);
                for _ in 0..chg_cnt {
                    if at + CHANGE_HEADER_LEN > bytes.len() {
                        return Err(RecordError::Truncated);
                    }
                    let offset = u16::from_le_bytes(bytes[at..at + 2].try_into().expect("2 字节"));
                    let len = u16::from_le_bytes(bytes[at + 2..at + 4].try_into().expect("2 字节"))
                        as usize;
                    at += CHANGE_HEADER_LEN;
                    if at + len > bytes.len() {
                        return Err(RecordError::Truncated);
                    }
                    changes.push(Change {
                        offset,
                        after: bytes[at..at + len].to_vec(),
                    });
                    at += len;
                }
                blocks.push(BlockRef {
                    flags: bflags,
                    rdba,
                    changes,
                });
            }
        }
        // `flags = 0`（无任何分段）合法——空载荷记录（如回滚完成，
        // 信息全在头部 `txn_id`）。

        let main = bytes[at..].to_vec();
        if flags & FLAG_MAIN_SEGMENT == 0 && !main.is_empty() {
            return Err(RecordError::FlagsMismatch);
        }
        if flags & FLAG_MAIN_SEGMENT != 0 && main.is_empty() {
            return Err(RecordError::FlagsMismatch);
        }
        Ok(Self {
            lsn,
            txn_id,
            op,
            blocks,
            main,
        })
    }

    /// 记录长度（编码后字节数；用于"不得跨文件"与组大小下限的判定）。
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        self.encode().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lsn(v: u64) -> Lsn {
        Lsn::from_raw(v).unwrap()
    }

    #[test]
    fn page_modification_roundtrip() {
        let rec = RedoRecord::page_modification(
            lsn(1024),
            42,
            vec![
                BlockRef {
                    flags: 0,
                    rdba: Rdba::from_parts(1, 7).unwrap(),
                    changes: vec![
                        Change {
                            offset: 100,
                            after: vec![0xAA, 0xBB],
                        },
                        Change {
                            offset: 200,
                            after: vec![0xCC],
                        },
                    ],
                },
                BlockRef {
                    flags: 0,
                    rdba: Rdba::from_parts(0, 0).unwrap(),
                    changes: vec![Change {
                        offset: 0,
                        after: vec![0x01],
                    }],
                },
            ],
        );
        let bytes = rec.encode();
        // 头 20 + 块段（1 + 2×8 + (4+2 + 4+1) + (4+1)）= 20 + 1 + 16 + 11 + 5 = 53
        assert_eq!(bytes.len(), 53);
        assert_eq!(
            u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize,
            bytes.len()
        );
        assert_eq!(bytes[16], 0x10, "op = 页修改");
        assert_eq!(bytes[17], FLAG_BLOCK_SEGMENT, "只有块段");
        let back = RedoRecord::decode(&bytes).unwrap();
        assert_eq!(back, rec);
    }

    #[test]
    fn system_records_use_main_segment_only() {
        let commit = RedoRecord::commit(lsn(512), 9, 12345);
        let bytes = commit.encode();
        assert_eq!(
            bytes.len(),
            RECORD_HEADER_LEN + 6,
            "主段 6 字节、无长度字段"
        );
        assert_eq!(bytes[17], FLAG_MAIN_SEGMENT);
        let back = RedoRecord::decode(&bytes).unwrap();
        assert_eq!(back.commit_seq(), Some(12345));
        assert_eq!(back.op, RecordOp::Commit.as_u8());

        let rb = RedoRecord::rollback_done(lsn(600), 9);
        let bytes = rb.encode();
        assert_eq!(bytes.len(), RECORD_HEADER_LEN, "空载荷记录 = 只有记录头");
        assert_eq!(bytes[17], 0, "无任何分段 ⇒ flags = 0");
        let back = RedoRecord::decode(&bytes).unwrap();
        assert_eq!(back, rb);
        assert_eq!(back.txn_id, 9, "信息全在头部");

        let cp = RedoRecord::checkpoint(lsn(700), 10, 2048, 11, 9);
        assert_eq!(cp.encode().len(), RECORD_HEADER_LEN + 24);
        assert_eq!(RedoRecord::decode(&cp.encode()).unwrap().main.len(), 24);

        let sw = RedoRecord::log_switch(lsn(800), 3, 77);
        let back = RedoRecord::decode(&sw.encode()).unwrap();
        assert_eq!(back.main, vec![3, 77, 0, 0, 0]);
    }

    #[test]
    fn contradictory_flags_are_rejected() {
        // 置了块段位却 blk_cnt = 0。
        let mut rec = RedoRecord::page_modification(
            lsn(1),
            1,
            vec![BlockRef {
                flags: 0,
                rdba: Rdba::from_parts(1, 1).unwrap(),
                changes: vec![Change {
                    offset: 0,
                    after: vec![1],
                }],
            }],
        );
        rec.blocks.clear();
        let mut bytes = rec.encode();
        bytes[17] = FLAG_BLOCK_SEGMENT; // 撒谎的 flags
        assert_eq!(
            RedoRecord::decode(&bytes).err(),
            Some(RecordError::FlagsMismatch)
        );

        // 置了主段位却无主段字节。
        let mut bytes = rec.encode();
        bytes[17] |= FLAG_MAIN_SEGMENT;
        assert_eq!(
            RedoRecord::decode(&bytes).err(),
            Some(RecordError::FlagsMismatch)
        );

        // 长度不符 / 截断。
        let good = RedoRecord::commit(lsn(1), 1, 1).encode();
        assert_eq!(
            RedoRecord::decode(&good[..good.len() - 1]).err(),
            Some(RecordError::LengthMismatch)
        );
        assert_eq!(
            RedoRecord::decode(&[0u8; 10]).err(),
            Some(RecordError::Truncated)
        );
    }

    #[test]
    fn rdba_packing() {
        let r = Rdba::from_parts(1023, (1 << 28) - 1).unwrap();
        assert_eq!(r.file_id(), 1023);
        assert_eq!(r.block_id(), (1 << 28) - 1);
        assert_eq!(Rdba::from_bytes(&r.to_bytes()), r);
        assert_eq!(r.to_bytes().len(), 5);
        assert!(Rdba::from_parts(1024, 0).is_none());
    }

    #[test]
    fn max_record_size_matches_the_frozen_number() {
        // 最坏单条 = 重写一整个数据页：头 20 + 块段(1 + 8 + 4 + 16384) = 16417B。
        let rec = RedoRecord::page_modification(
            lsn(0),
            1,
            vec![BlockRef {
                flags: 0,
                rdba: Rdba::from_parts(1, 2).unwrap(),
                changes: vec![Change {
                    offset: 0,
                    after: vec![0u8; 16 * 1024],
                }],
            }],
        );
        assert_eq!(rec.encoded_len(), 16_417, "§11.5.2 的定量");
    }
}
