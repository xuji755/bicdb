//! 页文件持久化用例（REQ-STO-012："**关闭重开后结果一致**"）。
//!
//! 三件事在这里闭环：
//! 1. P1 的 FileIO 替换接口（`OsFileIo` / `MemFileIo` / `FaultInjecting`）
//!    与 P2 的页格式；
//! 2. 故障注入（撕裂写）→ 两层完整性检出 → **明确判定**（不静默返回数据）；
//! 3. 关闭（关句柄、丢实例）→ 重开 → 读回一致。

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use bicdb_storage::heap::{self, InsertPolicy};
use bicdb_storage::page::{Page, PageCheck, PageType, PAGE_SIZE};
use bicdb_storage::pagefile::{self, PageFileError};
use bicdb_storage::row::assemble_row;
use bicdb_workspace::io::{
    FaultInjecting, FaultOp, FaultRule, FileIo, MemFileIo, OpenOptions, OsFileIo,
};

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时钟应晚于 UNIX_EPOCH")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "bicdb-pagefile-{tag}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("创建测试目录");
    dir
}

fn data_page(block_id: u32, payload: &[u8]) -> Page {
    let mut page = Page::new(PageType::HeapTable, [0x42; 8], 1, block_id);
    let row = assemble_row(0, 0, &[false], &[], &[payload]);
    heap::insert_row(&mut page, &row, &InsertPolicy::append_only()).unwrap();
    page
}

#[test]
fn close_reopen_roundtrip_is_byte_identical() {
    let dir = temp_dir("roundtrip");
    let path = dir.join("data.bin");

    // ---- 第一次打开：写 3 块 + 落盘 ----
    let want: Vec<Vec<u8>> = (1..=3u32)
        .map(|b| {
            let mut p = data_page(b, format!("payload-{b}").as_bytes());
            p.seal();
            p.as_bytes().to_vec()
        })
        .collect();
    {
        let io = OsFileIo::new();
        let handle = pagefile::create(&io, &path, 8).expect("创建页文件");
        for (i, bytes) in want.iter().enumerate() {
            let mut page = Page::from_bytes(Box::new(bytes.as_slice().try_into().unwrap()));
            pagefile::write_page(&io, handle, (i + 1) as u32, &mut page).unwrap();
        }
        pagefile::sync(&io, handle).expect("落盘");
        pagefile::close(&io, handle).expect("关闭");
        // io 实例在此丢弃（模拟进程退出）。
    }

    // ---- 重新打开（全新 io 实例）：读回逐字节一致 ----
    let io = OsFileIo::new();
    let handle = pagefile::open(&io, &path).expect("重开页文件");
    assert_eq!(pagefile::blocks(&io, handle).unwrap(), 8);
    for (i, bytes) in want.iter().enumerate() {
        let page = pagefile::read_page_verified(&io, handle, (i + 1) as u32).expect("读回并校验");
        assert_eq!(
            page.as_bytes().as_slice(),
            bytes.as_slice(),
            "块 {} 逐字节一致",
            i + 1
        );
    }
    // 未写过的块：零页 → 页类型字段为 0 → 明确判定"无法解码"（不是静默通过）。
    match pagefile::read_page_verified(&io, handle, 5) {
        Err(PageFileError::Damaged {
            block: 5,
            check: PageCheck::UnknownPageType,
        }) => {}
        other => panic!("零块应报 UnknownPageType，实际 {other:?}"),
    }
    pagefile::close(&io, handle).unwrap();
    std::fs::remove_dir_all(&dir).expect("清理测试目录");
}

#[test]
fn mem_backend_obeys_the_same_contract() {
    let io = MemFileIo::new();
    io.add_dir("/mem");
    let path = PathBuf::from("/mem/pagefile.bin");

    let handle = pagefile::create(&io, &path, 4).unwrap();
    let mut page = data_page(1, b"memory");
    pagefile::write_page(&io, handle, 1, &mut page).unwrap();
    pagefile::sync(&io, handle).unwrap();
    pagefile::close(&io, handle).unwrap();

    let handle = pagefile::open(&io, &path).unwrap();
    let back = pagefile::read_page_verified(&io, handle, 1).unwrap();
    assert_eq!(back.as_bytes(), page.as_bytes());
    pagefile::close(&io, handle).unwrap();
}

#[test]
fn torn_write_is_detected_on_read_back() {
    let dir = temp_dir("torn");
    let path = dir.join("torn.bin");

    // 第 2 次 write_at 注入：先写入前一半字节，再失败（撕裂写）。
    let io = FaultInjecting::new(OsFileIo::new());
    io.add_rule(FaultRule::torn_write(
        2,
        PAGE_SIZE / 2,
        std::io::ErrorKind::Other,
    ));

    let handle = pagefile::create(&io, &path, 4).expect("创建");
    let mut p1 = data_page(1, b"first");
    pagefile::write_page(&io, handle, 1, &mut p1).expect("第 1 次写成功");

    let mut p2 = data_page(2, b"second-which-must-fail");
    let err = pagefile::write_page(&io, handle, 2, &mut p2).expect_err("第 2 次写被注入撕裂");
    assert_eq!(err.kind(), std::io::ErrorKind::Other);

    // 读回：块 1 完整；块 2 是撕裂现场——**必须被检出**，绝不静默返回数据。
    let check = |block: u32| -> Result<(), PageCheck> {
        match pagefile::read_page_verified(&io, handle, block) {
            Ok(_) => Ok(()),
            Err(PageFileError::Damaged { check, .. }) => Err(check),
            Err(e) => panic!("意外错误：{e}"),
        }
    };
    assert_eq!(check(1), Ok(()));
    assert!(check(2).is_err(), "撕裂页必须被检出");
    pagefile::close(&io, handle).unwrap();
    std::fs::remove_dir_all(&dir).expect("清理测试目录");
}

#[test]
fn failed_sync_is_surfaced_not_swallowed() {
    let dir = temp_dir("syncerr");
    let path = dir.join("sync.bin");
    let io = FaultInjecting::new(OsFileIo::new());

    let handle = pagefile::create(&io, &path, 2).unwrap();
    let mut page = data_page(1, b"data");
    pagefile::write_page(&io, handle, 1, &mut page).unwrap();

    // 第一次 sync 注入失败：错误必须浮出（提交路径据此决定是否算持久）。
    io.add_rule(FaultRule::once(
        FaultOp::SyncData,
        1,
        std::io::ErrorKind::Other,
    ));
    assert!(pagefile::sync(&io, handle).is_err());
    // 第二次恢复。
    pagefile::sync(&io, handle).expect("重试应成功");
    pagefile::close(&io, handle).unwrap();
    std::fs::remove_dir_all(&dir).expect("清理测试目录");
}

#[test]
fn addressing_maps_blocks_to_sixteen_kib_offsets() {
    let dir = temp_dir("addr");
    let path = dir.join("addr.bin");
    let io = OsFileIo::new();
    let handle = pagefile::create(&io, &path, 3).unwrap();

    let mut page = data_page(2, b"block-two");
    pagefile::write_page(&io, handle, 2, &mut page).unwrap();
    pagefile::sync(&io, handle).unwrap();

    // 从文件偏移 2 × 16384 处读回同一页的内容。
    let mut raw = vec![0u8; PAGE_SIZE];
    io.read_exact_at(handle, &mut raw, 2 * PAGE_SIZE as u64)
        .unwrap();
    assert_eq!(raw, page.as_bytes().to_vec(), "块 2 = 偏移 32768");

    pagefile::close(&io, handle).unwrap();
    std::fs::remove_dir_all(&dir).expect("清理测试目录");
}

#[test]
fn create_is_create_new() {
    let dir = temp_dir("createnew");
    let path = dir.join("once.bin");
    let io = OsFileIo::new();
    let h = pagefile::create(&io, &path, 1).unwrap();
    pagefile::close(&io, h).unwrap();
    assert_eq!(
        io.open(&path, OpenOptions::new().write(true).create_new(true))
            .expect_err("已存在必须拒绝")
            .kind(),
        std::io::ErrorKind::AlreadyExists
    );
    std::fs::remove_dir_all(&dir).expect("清理测试目录");
}
