//! 日志文件闭环：**写入 → 关盘 → 重开 → 扫描恢复**（§11.5.1、REQ-STO-012 的
//! "关闭重开后结果一致"在 WAL 上的对应物）。

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use bicdb_common::seq::Lsn;
use bicdb_wal::buffer::{LogBuffer, LogSink};
use bicdb_wal::file::{scan_log, FileLogSink};
use bicdb_wal::logpage::{decode_records, LogPage, TailState, LOG_PAGE_SIZE};
use bicdb_wal::record::{BlockRef, Change, Rdba, RedoRecord};
use bicdb_workspace::io::{FileIo, OpenOptions, OsFileIo};

const FILE_PAGES: u64 = 64;

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时钟应晚于 UNIX_EPOCH")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "bicdb-wal-test-{tag}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("创建测试目录");
    dir
}

fn create_log_file(io: &OsFileIo, path: &Path) -> bicdb_workspace::io::FileHandle {
    let handle = io
        .open(
            path,
            OpenOptions::new().read(true).write(true).create_new(true),
        )
        .expect("创建日志文件");
    io.set_len(handle, FILE_PAGES * LOG_PAGE_SIZE as u64)
        .expect("预置长度");
    handle
}

fn commit(lsn: Lsn, txn: u64, seq: u64) -> RedoRecord {
    RedoRecord::commit(lsn, txn, seq)
}

fn big(lsn: Lsn, payload: usize) -> RedoRecord {
    RedoRecord::page_modification(
        lsn,
        txn_of(lsn),
        vec![BlockRef {
            flags: 0,
            rdba: Rdba::from_parts(1, 3).unwrap(),
            changes: vec![Change {
                offset: 0,
                after: vec![0xA5; payload],
            }],
        }],
    )
}

fn txn_of(_lsn: Lsn) -> u64 {
    1
}

#[test]
fn write_close_reopen_scan_roundtrip() {
    let dir = temp_dir("roundtrip");
    let path = dir.join("redo01.log");
    let start = Lsn::from_raw(0).unwrap();

    let written: Vec<RedoRecord>;
    {
        let io = OsFileIo::new();
        let handle = create_log_file(&io, &path);
        let buf = LogBuffer::new(start);
        // 一条小记录 + 一条跨页记录 + 一条提交，全部刷盘后关闭。
        buf.append(|l| commit(l, 1, 100)).unwrap();
        buf.append(|l| big(l, 1200)).unwrap();
        buf.append(|l| commit(l, 2, 101)).unwrap();
        let mut sink = FileLogSink::new(&io, handle, start, FILE_PAGES);
        let synced = buf.flush_to(buf.appended_lsn(), &mut sink).unwrap();
        assert!(synced > start);
        io.close(handle).expect("关闭");
        drop(io);
        written = vec![]; // 占位（真实来源在下面重新扫描）
        let _ = written;
    }

    // 重开（全新 io 实例）：扫描回来。
    let io = OsFileIo::new();
    let handle = io.open(&path, OpenOptions::new().read(true)).expect("重开");
    let result = scan_log(&io, handle, start, FILE_PAGES).unwrap();
    assert_eq!(result.tail, TailState::Clean);
    assert!(result.first_bad_page.is_none());
    assert_eq!(result.records.len(), 3);
    assert_eq!(result.records[0].commit_seq(), Some(100));
    assert_eq!(result.records[1].blocks[0].changes[0].after.len(), 1200);
    assert_eq!(result.records[2].commit_seq(), Some(101));
    io.close(handle).unwrap();
    std::fs::remove_dir_all(&dir).expect("清理测试目录");
}

#[test]
fn crashed_tail_is_dropped_on_scan() {
    let dir = temp_dir("crash");
    let path = dir.join("redo01.log");
    let start = Lsn::from_raw(0).unwrap();

    let file_pages_written: u64;
    {
        let io = OsFileIo::new();
        let handle = create_log_file(&io, &path);
        let buf = LogBuffer::new(start);
        buf.append(|l| commit(l, 1, 1)).unwrap();
        buf.append(|l| big(l, 2000)).unwrap(); // 跨多页
        let mut sink = FileLogSink::new(&io, handle, start, FILE_PAGES);
        buf.flush_to(buf.appended_lsn(), &mut sink).unwrap();
        file_pages_written = (sink.written_end().as_raw()).div_ceil(LOG_PAGE_SIZE as u64);
        io.close(handle).unwrap();
    }

    // 模拟崩溃：文件被截断到最后一页之前（大记录的分片残缺）。
    let truncated_len = (file_pages_written - 1) * LOG_PAGE_SIZE as u64;
    {
        let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(truncated_len).unwrap();
    }

    let io = OsFileIo::new();
    let handle = io.open(&path, OpenOptions::new().read(true)).unwrap();
    let result = scan_log(&io, handle, start, FILE_PAGES).unwrap();
    assert_eq!(result.tail, TailState::Truncated, "残缺记录整条丢弃");
    assert_eq!(result.records.len(), 1, "只保留完整的第一条");
    assert_eq!(result.records[0].commit_seq(), Some(1));
    io.close(handle).unwrap();
    std::fs::remove_dir_all(&dir).expect("清理测试目录");
}

#[test]
fn tampered_page_stops_the_scan() {
    let dir = temp_dir("tamper");
    let path = dir.join("redo01.log");
    let start = Lsn::from_raw(0).unwrap();

    {
        let io = OsFileIo::new();
        let handle = create_log_file(&io, &path);
        let buf = LogBuffer::new(start);
        buf.append(|l| commit(l, 1, 1)).unwrap();
        let mut sink = FileLogSink::new(&io, handle, start, FILE_PAGES);
        buf.flush_to(buf.appended_lsn(), &mut sink).unwrap();
        // 刷盘切页后再追加 → 第二条记录落到第 2 页。
        buf.append(|l| commit(l, 2, 2)).unwrap();
        buf.flush_to(buf.appended_lsn(), &mut sink).unwrap();
        io.close(handle).unwrap();
    }
    // 篡改第 2 页（偏移 512 + 100）。
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.seek(SeekFrom::Start(LOG_PAGE_SIZE as u64 + 100)).unwrap();
        f.write_all(&[0xFF]).unwrap();
    }

    let io = OsFileIo::new();
    let handle = io.open(&path, OpenOptions::new().read(true)).unwrap();
    let result = scan_log(&io, handle, start, FILE_PAGES).unwrap();
    assert_eq!(result.first_bad_page, Some(1), "第 2 页校验失败");
    assert_eq!(result.records.len(), 1, "坏页之前的记录保留");
    io.close(handle).unwrap();
    std::fs::remove_dir_all(&dir).expect("清理测试目录");
}

#[test]
fn buffer_and_file_disagree_on_position_is_caught() {
    // 用另一起始 LSN 打开同一文件 → 页位置与物理偏移不符，拒绝写。
    let dir = temp_dir("mismatch");
    let path = dir.join("redo01.log");
    {
        let io = OsFileIo::new();
        let handle = create_log_file(&io, &path);
        let buf = LogBuffer::new(Lsn::from_raw(0).unwrap());
        buf.append(|l| commit(l, 1, 1)).unwrap();
        let mut sink = FileLogSink::new(&io, handle, Lsn::from_raw(0).unwrap(), FILE_PAGES);
        buf.flush_to(buf.appended_lsn(), &mut sink).unwrap();
        io.close(handle).unwrap();
    }
    let io = OsFileIo::new();
    let handle = io
        .open(&path, OpenOptions::new().read(true).write(true))
        .unwrap();
    // 起始 LSN 说成 512：扫描应当发现页 0 自带 0 ≠ 512。
    let result = scan_log(&io, handle, Lsn::from_raw(512).unwrap(), FILE_PAGES).unwrap();
    assert_eq!(result.first_bad_page, Some(0));
    assert!(result.records.is_empty());
    io.close(handle).unwrap();
    std::fs::remove_dir_all(&dir).expect("清理测试目录");
}

#[test]
fn sink_rejects_out_of_order_pages() {
    let dir = temp_dir("order");
    let path = dir.join("redo01.log");
    let io = OsFileIo::new();
    let handle = create_log_file(&io, &path);
    let mut sink = FileLogSink::new(&io, handle, Lsn::from_raw(0).unwrap(), FILE_PAGES);
    let mut pages: Vec<LogPage> = Vec::new();
    bicdb_wal::logpage::write_record(&mut pages, &commit(Lsn::from_raw(16).unwrap(), 1, 1))
        .unwrap();
    pages[0].seal();
    sink.append_page(&pages[0]).unwrap();
    // 乱序：再来一张页位置倒退的页。
    let mut pages2: Vec<LogPage> = Vec::new();
    bicdb_wal::logpage::write_record(&mut pages2, &commit(Lsn::from_raw(16).unwrap(), 1, 2))
        .unwrap();
    let mut backward = LogPage::new(Lsn::from_raw(0).unwrap());
    backward
        .append_fragment(&bicdb_wal::logpage::Fragment {
            rec_id: Lsn::from_raw(16).unwrap(),
            frag_no: 0,
            frag_cnt: 1,
            data: commit(Lsn::from_raw(16).unwrap(), 1, 2).encode(),
        })
        .unwrap();
    backward.seal();
    assert!(sink.append_page(&backward).is_err());
    io.close(handle).unwrap();
    std::fs::remove_dir_all(&dir).expect("清理测试目录");
}

#[test]
fn decode_records_helper_agrees_with_scan() {
    // 同一批记录：经 decode_records（内存页）与经 scan_log（真实文件）结果一致。
    let io = OsFileIo::new();
    let dir = temp_dir("agree");
    let path = dir.join("redo01.log");
    let handle = create_log_file(&io, &path);
    let buf = LogBuffer::new(Lsn::from_raw(0).unwrap());
    buf.append(|l| commit(l, 5, 9)).unwrap();
    let mut sink = FileLogSink::new(&io, handle, Lsn::from_raw(0).unwrap(), FILE_PAGES);
    buf.flush_to(buf.appended_lsn(), &mut sink).unwrap();

    let (mem_records, ok) = bicdb_wal::buffer::decode_sink_pages(&sink_pages_probe(&io, handle));
    assert!(ok);
    let scan = scan_log(&io, handle, Lsn::from_raw(0).unwrap(), FILE_PAGES).unwrap();
    assert_eq!(mem_records, scan.records);
    assert_eq!(scan.records[0].commit_seq(), Some(9));
    let _ = decode_records(&[]).0;
    io.close(handle).unwrap();
    std::fs::remove_dir_all(&dir).expect("清理测试目录");
}

fn sink_pages_probe(io: &OsFileIo, handle: bicdb_workspace::io::FileHandle) -> Vec<Vec<u8>> {
    // 读回前若干页（直到全零页）。
    let mut out = Vec::new();
    for i in 0..FILE_PAGES {
        let mut buf = vec![0u8; LOG_PAGE_SIZE];
        io.read_exact_at(handle, &mut buf, i * LOG_PAGE_SIZE as u64)
            .unwrap();
        if buf.iter().all(|&b| b == 0) {
            break;
        }
        out.push(buf);
    }
    out
}
