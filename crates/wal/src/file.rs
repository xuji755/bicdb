//! 单个 redo 文件的落盘映射与扫描恢复入口（存储架构 §11.5.1）。
//!
//! **LSN = 文件字节位置**（可推导）：文件从 `start_lsn` 起，
//! 第 k 页占 `[start + k×512, start + (k+1)×512)`。
//! 因此 `页位置` 字段与物理偏移互为校验（下式两边必须相等）：
//!
//! ```text
//! 物理偏移 = page.start_lsn − file_start_lsn
//! ```
//!
//! **"一条记录不得跨越文件边界"**（§11.5.1）：本模块在
//! [`FileLogSink::append_page`] 处强制——越界的页直接拒绝
//! （文件尾空间不够由**上层切换逻辑**填充后从下一个文件开始；
//! 日志组/序列号随"日志切换"切片接入）。
//!
//! **扫描**（[`scan_log`]）：按页读入 → 逐页校验 → 重组记录；
//! 末页不完整（`used` 越界/校验和不符/分片残缺）→ **整条丢弃**并报告
//! `Truncated`——这正是"崩溃后日志末尾可能是半截"的落点。

use std::io;

use bicdb_common::seq::Lsn;
use bicdb_workspace::io::{FileHandle, FileIo};

use crate::buffer::LogSink;
use crate::logpage::{decode_records, LogPage, TailState, LOG_PAGE_SIZE};
use crate::record::RedoRecord;

/// 日志文件错误。
#[derive(Debug)]
pub enum LogFileError {
    /// 底层 I/O。
    Io(io::Error),
    /// 页位置与物理偏移不符（写错文件/位置）。
    PositionMismatch {
        /// 页自带的起始 LSN。
        page_lsn: Lsn,
        /// 按顺序应处的 LSN。
        expected: Lsn,
    },
    /// 越过文件边界（"记录不得跨文件"）。
    BeyondFileEnd,
}

impl std::fmt::Display for LogFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogFileError::Io(e) => write!(f, "日志文件 I/O：{e}"),
            LogFileError::PositionMismatch { page_lsn, expected } => {
                write!(f, "日志页位置不符：页自带 {page_lsn}，按序应为 {expected}")
            }
            LogFileError::BeyondFileEnd => f.write_str("越过日志文件边界（记录不得跨文件）"),
        }
    }
}

impl std::error::Error for LogFileError {}

impl From<io::Error> for LogFileError {
    fn from(e: io::Error) -> Self {
        LogFileError::Io(e)
    }
}

/// 一个 redo 文件的顺序写 sink。
pub struct FileLogSink<'a> {
    io: &'a dyn FileIo,
    handle: FileHandle,
    file_start_lsn: Lsn,
    /// 文件页数（容量 = pages × 512）。
    file_pages: u64,
    /// 已写到的流位置（下一页的落位）。
    written_end: u64,
}

impl<'a> FileLogSink<'a> {
    /// 包装一个已打开（或新创建）的 redo 文件。
    ///
    /// `written_end` 从 `start_lsn` 起——本 sink 只做**顺序追加**；
    /// 续写既有文件时由调用方先扫描确定末尾位置。
    #[must_use]
    pub fn new(
        io: &'a dyn FileIo,
        handle: FileHandle,
        file_start_lsn: Lsn,
        file_pages: u64,
    ) -> Self {
        Self::resume(io, handle, file_start_lsn, file_pages, 0)
    }

    /// **续写**既有文件：认定盘上已有 `written_pages` 页（文件前缀），
    /// 从下一页起追加——**已刷出的页不再改写**（重开后接在前缀之后；
    /// 前缀的合法性由调用方先扫描确定，见 `group::GroupWriter::open`）。
    #[must_use]
    pub fn resume(
        io: &'a dyn FileIo,
        handle: FileHandle,
        file_start_lsn: Lsn,
        file_pages: u64,
        written_pages: u64,
    ) -> Self {
        Self {
            io,
            handle,
            file_start_lsn,
            file_pages,
            written_end: file_start_lsn.as_raw() + written_pages * LOG_PAGE_SIZE as u64,
        }
    }

    /// 已写到（尚未 sync）的流位置。
    #[must_use]
    pub fn written_end(&self) -> Lsn {
        Lsn::from_raw(self.written_end).expect("48 位域内")
    }

    /// 已写到的页数（文件前缀长度）。
    #[must_use]
    pub fn written_pages(&self) -> u64 {
        (self.written_end - self.file_start_lsn.as_raw()) / LOG_PAGE_SIZE as u64
    }
}

impl LogSink for FileLogSink<'_> {
    fn append_page(&mut self, page: &LogPage) -> io::Result<()> {
        let expected = self.written_end;
        if page.start_lsn().as_raw() != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                LogFileError::PositionMismatch {
                    page_lsn: page.start_lsn(),
                    expected: Lsn::from_raw(expected).expect("48 位域内"),
                }
                .to_string(),
            ));
        }
        let offset = expected - self.file_start_lsn.as_raw();
        if offset + LOG_PAGE_SIZE as u64 > self.file_pages * LOG_PAGE_SIZE as u64 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                LogFileError::BeyondFileEnd.to_string(),
            ));
        }
        self.io
            .write_at(self.handle, page.as_bytes().as_slice(), offset)?;
        self.written_end += LOG_PAGE_SIZE as u64;
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        self.io.sync_data(self.handle)
    }
}

/// 扫描结果。
#[derive(Debug)]
pub struct ScanResult {
    /// 重组出的记录（按序）。
    pub records: Vec<RedoRecord>,
    /// 扫描走过的页数（含末尾残缺页）。
    pub pages_scanned: u64,
    /// 末尾状态（干净 / 截断）。
    pub tail: TailState,
    /// 首个损坏页（块内序号；`None` = 无）。
    pub first_bad_page: Option<u64>,
}

/// 扫描一个 redo 文件：逐页读入并校验 → 重组记录。
///
/// 末尾不完整（校验和不符 / 分片残缺）→ 停止扫描并返回 `Truncated`。
pub fn scan_log(
    io: &dyn FileIo,
    handle: FileHandle,
    file_start_lsn: Lsn,
    file_pages: u64,
) -> Result<ScanResult, LogFileError> {
    // 崩溃后文件可能短于声明容量：以**实际长度**为准（不足一页的尾巴 = 截断）。
    let size = io.size(handle)?;
    let usable_pages = (size / LOG_PAGE_SIZE as u64).min(file_pages);

    let mut pages = Vec::new();
    let mut first_bad_page = None;
    let mut i = 0u64;
    while i < usable_pages {
        let offset = i * LOG_PAGE_SIZE as u64;
        let mut buf = Box::new([0u8; LOG_PAGE_SIZE]);
        io.read_exact_at(handle, buf.as_mut_slice(), offset)?;
        let page = LogPage::from_bytes(buf);
        // 空页（全零 = 从未写过）：日志到此为止。
        if page.as_bytes().iter().all(|&b| b == 0) {
            break;
        }
        // 页位置必须与物理位置一致（互为校验）。
        let expected = file_start_lsn.as_raw() + offset;
        if page.start_lsn().as_raw() != expected {
            first_bad_page = Some(i);
            break;
        }
        if page.verify().is_err() {
            first_bad_page = Some(i);
            break;
        }
        pages.push(page);
        i += 1;
    }
    let (records, tail, _errors) = decode_records(&pages);
    Ok(ScanResult {
        records,
        pages_scanned: pages.len() as u64,
        tail: if first_bad_page.is_some() || size % LOG_PAGE_SIZE as u64 != 0 {
            TailState::Truncated
        } else {
            tail
        },
        first_bad_page,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_mismatch_is_rejected() {
        // 用 MemFileIo 走一遍"页位置与应处位置不符"。
        let io = bicdb_workspace::io::MemFileIo::new();
        io.add_dir("/mem");
        let handle = io
            .open(
                std::path::Path::new("/mem/redo01.log"),
                bicdb_workspace::io::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true),
            )
            .unwrap();
        io.set_len(handle, 4 * LOG_PAGE_SIZE as u64).unwrap();
        let mut sink = FileLogSink::new(&io, handle, Lsn::from_raw(0).unwrap(), 4);
        // 首片位置 16 ⇒ 页 0 起点 0 ✓
        let mut pages = Vec::new();
        crate::logpage::write_record(
            &mut pages,
            &RedoRecord::commit(Lsn::from_raw(16).unwrap(), 1, 7),
        )
        .unwrap();
        pages[0].seal();
        sink.append_page(&pages[0]).expect("页 0 落位");

        // 伪造一张位置不对的页。
        let mut bad = LogPage::new(Lsn::from_raw(2048).unwrap());
        bad.append_fragment(&crate::logpage::Fragment {
            rec_id: Lsn::from_raw(2064).unwrap(),
            frag_no: 0,
            frag_cnt: 1,
            data: RedoRecord::commit(Lsn::from_raw(2064).unwrap(), 1, 8).encode(),
        })
        .unwrap();
        bad.seal();
        let err = sink.append_page(&bad).expect_err("位置不符必须拒绝");
        assert!(err.to_string().contains("位置不符"));
    }

    #[test]
    fn beyond_file_end_is_rejected() {
        let io = bicdb_workspace::io::MemFileIo::new();
        io.add_dir("/mem");
        let handle = io
            .open(
                std::path::Path::new("/mem/redo02.log"),
                bicdb_workspace::io::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true),
            )
            .unwrap();
        io.set_len(handle, LOG_PAGE_SIZE as u64).unwrap();
        let mut sink = FileLogSink::new(&io, handle, Lsn::from_raw(0).unwrap(), 1);
        let mut pages = Vec::new();
        crate::logpage::write_record(
            &mut pages,
            &RedoRecord::commit(Lsn::from_raw(16).unwrap(), 1, 1),
        )
        .unwrap();
        pages[0].seal();
        sink.append_page(&pages[0]).unwrap();
        // 第二页越过 1 页容量的文件边界。
        let mut second = LogPage::new(Lsn::from_raw(512).unwrap());
        second
            .append_fragment(&crate::logpage::Fragment {
                rec_id: Lsn::from_raw(528).unwrap(),
                frag_no: 0,
                frag_cnt: 1,
                data: RedoRecord::commit(Lsn::from_raw(528).unwrap(), 1, 2).encode(),
            })
            .unwrap();
        second.seal();
        let err = sink.append_page(&second).expect_err("越界必须拒绝");
        assert!(err.to_string().contains("不得跨文件"));
    }
}
