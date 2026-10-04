//! 日志缓冲与追加：**latch 串行化的 WAL 追加** + 组提交友好的刷盘
//! （存储架构 §11.1 规则 3、§11.5.2）。
//!
//! # 三条规则
//!
//! 1. **追加经 latch 串行化**（§11.5.2："写少、争用低，不用无锁技巧"）——
//!    跨页对我们没有额外代价，因为跨不跨页都要拿这把 latch；
//! 2. **持 latch 不执行文件 I/O**（`ENG` REQ-ENG-001 的不变量）：
//!    刷盘时**先在 latch 内摘出页、再在 latch 外写与 sync**；
//! 3. **刷盘是"往后刷"的**（规则 3 的组提交收益）：日志是单一顺序流——
//!    后一条记录刷盘必然覆盖前一条；等待者只要看到 `synced_lsn ≥ 目标`
//!    即可返回，**共享同一次 fsync**。
//!
//! # 刷盘点与"页被切"
//!
//! 刷盘把**截至当时的所有页**（含未写满的当前页——页头的"已用长度"界定
//! 有效区）写出去；**被刷出的页不再接受追加**，后续追加自动新开一页。
//! 于是盘上的页序列永远是"前缀"，末尾的不完整记录由恢复扫描按
//! `frag_no`/`frag_cnt` 丢弃（[`crate::logpage::TailState::Truncated`]）。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use bicdb_common::seq::Lsn;

use crate::logpage::{decode_records, write_record, LogPage, LogPageError};
use crate::record::{RecordError, RedoRecord};

/// 日志缓冲错误。
#[derive(Debug)]
pub enum WalError {
    /// 记录分片/页失败。
    Page(LogPageError),
    /// 记录编码校验失败（构造出错）。
    Record(RecordError),
    /// 刷盘 I/O 失败（**synced_lsn 不前进**——持久性点未达成）。
    Io(std::io::Error),
    /// LSN 越过 48 位域。
    LsnExhausted,
}

impl std::fmt::Display for WalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WalError::Page(e) => write!(f, "日志页错误：{e}"),
            WalError::Record(e) => write!(f, "日志记录错误：{e}"),
            WalError::Io(e) => write!(f, "日志刷盘失败：{e}"),
            WalError::LsnExhausted => f.write_str("LSN 越过 48 位域"),
        }
    }
}

impl std::error::Error for WalError {}

impl From<LogPageError> for WalError {
    fn from(e: LogPageError) -> Self {
        WalError::Page(e)
    }
}

impl From<std::io::Error> for WalError {
    fn from(e: std::io::Error) -> Self {
        WalError::Io(e)
    }
}

/// 刷盘去处（日志文件的抽象；文件实现随"日志文件"切片接入）。
pub trait LogSink {
    /// 顺序写一页（页面按调用顺序落位）。
    fn append_page(&mut self, page: &LogPage) -> std::io::Result<()>;
    /// 持久性点（fsync/fdatasync 语义）。
    fn sync(&mut self) -> std::io::Result<()>;
}

/// 内存 sink（测试与"日志缓冲"自身用例）。
#[derive(Debug, Default)]
pub struct VecLogSink {
    /// 已写入的页（按序）。
    pub pages: Vec<Vec<u8>>,
    /// sync 次数（组提交收益的观测点）。
    pub syncs: usize,
    /// 注入：第 n 次 sync 失败（1 起；0 = 不注入）。
    pub fail_sync_at: usize,
}

impl LogSink for VecLogSink {
    fn append_page(&mut self, page: &LogPage) -> std::io::Result<()> {
        self.pages.push(page.as_bytes().to_vec());
        Ok(())
    }

    fn sync(&mut self) -> std::io::Result<()> {
        self.syncs += 1;
        if self.fail_sync_at != 0 && self.syncs == self.fail_sync_at {
            return Err(std::io::Error::other("注入：sync 失败"));
        }
        Ok(())
    }
}

struct BufferState {
    /// 未刷出的页（最后一张是当前追加页；空 = 需要新页）。
    pages: Vec<LogPage>,
    /// 下一张页的起点（刷盘切页后 = 末页起点 + 512）。
    next_page_start: u64,
    /// 追加位置（下一字节的 LSN）。
    appended_lsn: u64,
    /// 已刷盘位置（可由等待者无锁读取）。
    synced_lsn: AtomicU64,
}

/// 日志缓冲。
pub struct LogBuffer {
    state: Mutex<BufferState>,
    /// 刷盘串行（与追加 latch 分离；**写与 sync 在两者之外**）。
    io: Mutex<()>,
}

impl std::fmt::Debug for LogBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogBuffer")
            .field("appended_lsn", &self.appended_lsn())
            .field("synced_lsn", &self.synced_lsn())
            .finish()
    }
}

impl LogBuffer {
    /// 以 `start_lsn` 为日志流起点新建。
    #[must_use]
    pub fn new(start_lsn: Lsn) -> Self {
        Self {
            state: Mutex::new(BufferState {
                // 第 0 页立即就位：页体自 start + 16 起（LSN = 字节位置，
                // 页头也占位）。
                pages: vec![LogPage::new(start_lsn)],
                next_page_start: start_lsn.as_raw(),
                appended_lsn: start_lsn.as_raw() + crate::logpage::LOG_PAGE_HEADER_LEN as u64,
                synced_lsn: AtomicU64::new(start_lsn.as_raw()),
            }),
            io: Mutex::new(()),
        }
    }

    /// 追加位置（下一字节 LSN）。
    #[must_use]
    pub fn appended_lsn(&self) -> Lsn {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Lsn::from_raw(state.appended_lsn).expect("48 位域内")
    }

    /// 已刷盘位置。
    #[must_use]
    pub fn synced_lsn(&self) -> Lsn {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        Lsn::from_raw(state.synced_lsn.load(Ordering::SeqCst)).expect("48 位域内")
    }

    /// **下一次追加所在页**的起始 LSN：缓冲非空 ⇒ 当前页（其尾部空位仍可用）；
    /// 缓冲为空（刷盘切页后 / 刚新建）⇒ 下一张页的起点。
    ///
    /// 日志组切换以它为界——组与组在**页边界**相接；切换记录因此落在
    /// 新组首张页的页头之后。
    #[must_use]
    pub fn current_page_start(&self) -> Lsn {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let raw = match state.pages.last() {
            Some(last) => last.start_lsn().as_raw(),
            None => state.next_page_start,
        };
        Lsn::from_raw(raw).expect("48 位域内")
    }

    /// 预检：**现在**追加一条编码长度 `encoded_len` 的记录，其结束 LSN
    /// （结束字节的下一位置 = 该记录的占用终点）。
    ///
    /// 与 [`LogBuffer::append`] 共用同一套分片/分页规则（§`logpage::plan_fragments`）——
    /// "记录不得跨组"的提前切换据此**精确**判定，而非保守估计。
    #[must_use]
    pub fn end_lsn_if_appended(&self, encoded_len: usize) -> Lsn {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let (page_start, used) = match state.pages.last() {
            Some(last) => (last.start_lsn().as_raw(), last.used()),
            None => (state.next_page_start, 0),
        };
        let (end_page, end_used) = crate::logpage::simulate_append(page_start, used, encoded_len);
        Lsn::from_raw(end_page + crate::logpage::LOG_PAGE_HEADER_LEN as u64 + end_used as u64)
            .expect("48 位域内")
    }

    /// **追加一条记录**：latch 内分配 LSN（= 本条记录的 LSN，写进记录头）
    /// 并分片入页；返回该 LSN。
    ///
    /// `build` 收到的 LSN 必须原样放进记录（构造器自动做）。
    pub fn append(&self, build: impl FnOnce(Lsn) -> RedoRecord) -> Result<Lsn, WalError> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.pages.is_empty() {
            // 刷盘切页后：新页起于 next_page_start（页序列是文件的前缀）。
            let page =
                LogPage::new(Lsn::from_raw(state.next_page_start).ok_or(WalError::LsnExhausted)?);
            state.pages.push(page);
            state.appended_lsn = state.next_page_start + crate::logpage::LOG_PAGE_HEADER_LEN as u64;
        }
        // 当前页放不下任何分片 → 新页。
        if state.pages.last().expect("非空").remaining_data_capacity() == 0 {
            let next = state.pages.last().expect("非空").start_lsn().as_raw()
                + crate::logpage::LOG_PAGE_SIZE as u64;
            let page = LogPage::new(Lsn::from_raw(next).ok_or(WalError::LsnExhausted)?);
            state.pages.push(page);
            state.appended_lsn = next + crate::logpage::LOG_PAGE_HEADER_LEN as u64;
        }
        let lsn = Lsn::from_raw(state.appended_lsn).ok_or(WalError::LsnExhausted)?;
        let record = build(lsn);
        // 校验：构造器必须把 LSN 写进记录头（互为校验的本地一侧）。
        if record.lsn != lsn {
            return Err(WalError::Record(RecordError::LsnMismatch));
        }
        write_record(&mut state.pages, &record)?;
        // 追加位置推进 = 末页起始 LSN + 页头 16B + 该页已用字节（含本记录的
        // 分片头与数据；跨页时以记录结束所在页为准）。
        let last = state.pages.last().expect("刚写入");
        let end = last.start_lsn().as_raw()
            + crate::logpage::LOG_PAGE_HEADER_LEN as u64
            + last.used() as u64;
        debug_assert!(end >= state.appended_lsn, "追加位置单调不减");
        state.appended_lsn = end;
        Ok(lsn)
    }

    /// **刷盘到 `target`**：把截至当时的页写出去并 sync；返回实际刷到的位置。
    ///
    /// 组提交：`synced_lsn ≥ target` 即直接返回（共享同一次 fsync）。
    /// 失败时 **`synced_lsn` 不前进**（持久性点未达成，调用方不得回应提交）。
    pub fn flush_to(&self, target: Lsn, sink: &mut dyn LogSink) -> Result<Lsn, WalError> {
        let _io = self.io.lock().unwrap_or_else(|e| e.into_inner());
        {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let synced = state.synced_lsn.load(Ordering::SeqCst);
            if synced >= target.as_raw() {
                return Ok(Lsn::from_raw(synced).expect("48 位域内"));
            }
        }
        // 1) latch 内摘页（不执行 I/O）。
        let mut pages: Vec<LogPage> = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut state.pages)
        };

        // 2) latch 外写与 sync。
        //
        // **失败即放回**：页只有在 sync 成功之后才离开"未刷出"集合——
        // 否则失败一次就丢页（重试无从谈起，数据可能既不在盘上也不在缓冲）。
        // 放回后再追加也安全：同一页重写落在同一偏移（幂等）。
        let outcome = (|| -> Result<(), WalError> {
            for page in &mut pages {
                page.seal();
                sink.append_page(page)?;
            }
            sink.sync()?;
            Ok(())
        })();
        if let Err(e) = outcome {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let mut restored = pages;
            restored.append(&mut state.pages);
            state.pages = restored;
            return Err(e);
        }

        // 3) 持久性点达成：切页 + 前进 synced_lsn。
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let synced_after = pages
            .last()
            .map(|p| {
                p.start_lsn().as_raw()
                    + crate::logpage::LOG_PAGE_HEADER_LEN as u64
                    + p.used() as u64
            })
            .unwrap_or_else(|| state.synced_lsn.load(Ordering::SeqCst));
        if let Some(last) = pages.last() {
            state.next_page_start =
                last.start_lsn().as_raw() + crate::logpage::LOG_PAGE_SIZE as u64;
        }
        let cur = state.synced_lsn.load(Ordering::SeqCst);
        if synced_after > cur {
            state.synced_lsn.store(synced_after, Ordering::SeqCst);
        }
        Ok(Lsn::from_raw(synced_after).expect("48 位域内"))
    }
}

/// 从 sink 中按序写入的页字节重组记录（供测试与恢复入口复用）。
///
/// 返回（记录, 是否干净）。
#[must_use]
pub fn decode_sink_pages(sink_pages: &[Vec<u8>]) -> (Vec<RedoRecord>, bool) {
    let pages: Vec<LogPage> = sink_pages
        .iter()
        .map(|b| LogPage::from_bytes(Box::new(b.as_slice().try_into().expect("512B"))))
        .collect();
    let (records, tail, errors) = decode_records(&pages);
    (
        records,
        tail == crate::logpage::TailState::Clean && errors.is_empty(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{BlockRef, Change, Rdba, RECORD_HEADER_LEN};

    fn lsn(v: u64) -> Lsn {
        Lsn::from_raw(v).unwrap()
    }

    fn commit_rec(lsn: Lsn, txn: u64, seq: u64) -> RedoRecord {
        RedoRecord::commit(lsn, txn, seq)
    }

    fn big_rec(lsn: Lsn, payload: usize) -> RedoRecord {
        RedoRecord::page_modification(
            lsn,
            1,
            vec![BlockRef {
                flags: 0,
                rdba: Rdba::from_parts(1, 1).unwrap(),
                changes: vec![Change {
                    offset: 0,
                    after: vec![0x77; payload],
                }],
            }],
        )
    }

    #[test]
    fn append_assigns_monotonic_lsns_and_records_match() {
        let buf = LogBuffer::new(lsn(0));
        let l1 = buf.append(|l| commit_rec(l, 1, 10)).unwrap();
        let l2 = buf.append(|l| commit_rec(l, 2, 11)).unwrap();
        assert_eq!(l1, lsn(16), "页 0 的页体起点");
        // 第二条 LSN = 16 + 第一条记录（头 20 + 主段 6）+ 分片头 12。
        assert_eq!(l2.as_raw(), 16 + (RECORD_HEADER_LEN + 6 + 12) as u64);
        assert!(buf.appended_lsn() > l2);

        let mut sink = VecLogSink::default();
        let synced = buf.flush_to(buf.appended_lsn(), &mut sink).unwrap();
        assert_eq!(synced, buf.appended_lsn());
        assert_eq!(sink.syncs, 1);
        let (records, ok) = decode_sink_pages(&sink.pages);
        assert!(ok);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].commit_seq(), Some(10));
        assert_eq!(records[1].commit_seq(), Some(11));
    }

    #[test]
    fn flush_cuts_the_page_and_further_appends_open_a_new_one() {
        let buf = LogBuffer::new(lsn(0));
        buf.append(|l| commit_rec(l, 1, 1)).unwrap();
        let mut sink = VecLogSink::default();
        buf.flush_to(buf.appended_lsn(), &mut sink).unwrap();
        assert_eq!(sink.pages.len(), 1);

        // 被刷出的页不再接受追加：后续记录落到新页。
        buf.append(|l| commit_rec(l, 2, 2)).unwrap();
        buf.flush_to(buf.appended_lsn(), &mut sink).unwrap();
        assert_eq!(sink.pages.len(), 2, "新页");
        let p1: [u8; 512] = sink.pages[1].as_slice().try_into().unwrap();
        assert_eq!(LogPage::from_bytes(Box::new(p1)).start_lsn().as_raw(), 512);

        // 盘上的页序列 = 前缀：全部记录都能重组。
        let (records, ok) = decode_sink_pages(&sink.pages);
        assert!(ok);
        assert_eq!(records.len(), 2);
    }

    #[test]
    fn group_commit_shares_one_sync() {
        let buf = LogBuffer::new(lsn(0));
        let a = buf.append(|l| commit_rec(l, 1, 1)).unwrap();
        let _b = buf.append(|l| commit_rec(l, 2, 2)).unwrap();
        let mut sink = VecLogSink::default();

        // 第一个等待者刷到自己的 LSN：一次 sync。
        buf.flush_to(a, &mut sink).unwrap();
        assert_eq!(sink.syncs, 1);
        // 第二个等待者的目标已被覆盖：**不再产生 I/O**（组提交收益）。
        let synced = buf.flush_to(buf.appended_lsn(), &mut sink).unwrap();
        assert_eq!(sink.syncs, 1, "共享同一次 fsync");
        assert_eq!(synced, buf.appended_lsn());
    }

    #[test]
    fn failed_sync_does_not_advance_synced_lsn() {
        let buf = LogBuffer::new(lsn(0));
        buf.append(|l| commit_rec(l, 1, 1)).unwrap();
        let mut sink = VecLogSink {
            fail_sync_at: 1,
            ..Default::default()
        };
        assert!(buf.flush_to(buf.appended_lsn(), &mut sink).is_err());
        assert_eq!(buf.synced_lsn(), lsn(0), "持久性点未达成");
        // 重试成功 → 前进。
        sink.fail_sync_at = 0;
        let synced = buf.flush_to(buf.appended_lsn(), &mut sink).unwrap();
        assert!(synced > lsn(0));
    }

    #[test]
    fn records_spanning_pages_roundtrip_through_the_buffer() {
        let buf = LogBuffer::new(lsn(0));
        buf.append(|l| commit_rec(l, 1, 1)).unwrap();
        buf.append(|l| big_rec(l, 1200)).unwrap(); // 跨页
        buf.append(|l| commit_rec(l, 2, 2)).unwrap();
        let mut sink = VecLogSink::default();
        buf.flush_to(buf.appended_lsn(), &mut sink).unwrap();

        let (records, ok) = decode_sink_pages(&sink.pages);
        assert!(ok);
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].commit_seq(), Some(1));
        assert_eq!(records[1].blocks[0].changes[0].after.len(), 1200);
        assert_eq!(records[2].commit_seq(), Some(2));
        // 页序按 LSN 递进。
        for w in sink.pages.windows(2) {
            let a: [u8; 512] = w[0].as_slice().try_into().unwrap();
            let b: [u8; 512] = w[1].as_slice().try_into().unwrap();
            assert_eq!(
                LogPage::from_bytes(Box::new(b)).start_lsn().as_raw(),
                LogPage::from_bytes(Box::new(a)).start_lsn().as_raw() + 512
            );
        }
    }

    #[test]
    fn append_rejects_a_record_whose_lsn_was_not_used() {
        let buf = LogBuffer::new(lsn(0));
        let err = buf.append(|_| commit_rec(lsn(999), 1, 1)).unwrap_err();
        assert!(matches!(err, WalError::Record(RecordError::LsnMismatch)));
    }
}
