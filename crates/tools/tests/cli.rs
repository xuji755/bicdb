//! 工具可用性用例（REQ-STO-012 验收："`page_dump`/`db_check` 在 P2 可用"）。
//!
//! 直接运行两个二进制（`CARGO_BIN_EXE_*`），验证退出码与输出。

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use bicdb_storage::heap::{self, InsertPolicy};
use bicdb_storage::page::{Page, PageType, PAGE_SIZE};
use bicdb_storage::row::assemble_row;

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时钟应晚于 UNIX_EPOCH")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "bicdb-tools-test-{tag}-{}-{nanos}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).expect("创建测试目录");
    dir
}

fn page_image(pages: &[Page]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pages.len() * PAGE_SIZE);
    for p in pages {
        out.extend_from_slice(p.as_bytes());
    }
    out
}

fn clean_page() -> Page {
    let mut page = Page::new(PageType::HeapTable, [0x11; 8], 1, 1);
    let policy = InsertPolicy::append_only();
    heap::insert_row(
        &mut page,
        &assemble_row(0, 0, &[false], &[], &[b"hello"]).unwrap(),
        &policy,
    )
    .unwrap();
    heap::insert_row(
        &mut page,
        &assemble_row(0, 0, &[false], &[], &[b"world"]).unwrap(),
        &policy,
    )
    .unwrap();
    page.seal();
    page
}

#[test]
fn db_check_exit_codes_and_messages() {
    let dir = temp_dir("dbcheck");

    // 干净镜像 → 0。
    let clean = dir.join("clean.bin");
    fs::write(&clean, page_image(&[clean_page()])).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_db_check"))
        .arg(&clean)
        .output()
        .expect("运行 db_check");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("判定 Usable"), "{text}");

    // 损坏镜像（改一个行区字节）→ 1 + 完整性发现。
    let mut damaged = clean_page();
    damaged.as_bytes_mut()[9000] ^= 0xFF;
    let bad = dir.join("bad.bin");
    fs::write(&bad, page_image(&[damaged])).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_db_check"))
        .arg(&bad)
        .output()
        .expect("运行 db_check");
    assert_eq!(out.status.code(), Some(1));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("INTEGRITY_CHECKSUM"), "{text}");

    // 截断镜像 → 1 + IMAGE_TRUNCATED。
    let truncated = dir.join("trunc.bin");
    fs::write(&truncated, vec![0u8; PAGE_SIZE - 7]).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_db_check"))
        .arg(&truncated)
        .output()
        .expect("运行 db_check");
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stdout).contains("IMAGE_TRUNCATED"));

    // 用法错误 → 2。
    let out = Command::new(env!("CARGO_BIN_EXE_db_check"))
        .output()
        .expect("运行 db_check");
    assert_eq!(out.status.code(), Some(2));

    fs::remove_dir_all(&dir).expect("清理测试目录");
}

#[test]
fn page_dump_shows_header_slots_and_extracts_a_page() {
    let dir = temp_dir("pagedump");
    let image = dir.join("img.bin");
    fs::write(&image, page_image(&[clean_page(), clean_page()])).unwrap();

    // 全量转储。
    let out = Command::new(env!("CARGO_BIN_EXE_page_dump"))
        .arg(&image)
        .output()
        .expect("运行 page_dump");
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("页转储"), "{text}");
    assert!(text.contains("HeapTable"));
    assert!(text.contains("正常行"));
    assert_eq!(text.matches("== 页转储").count(), 2, "两页都应被转储");

    // 指定页。
    let out = Command::new(env!("CARGO_BIN_EXE_page_dump"))
        .arg(&image)
        .arg("1")
        .output()
        .expect("运行 page_dump");
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout)
            .matches("== 页转储")
            .count(),
        1
    );

    // 页序号越界 → 2。
    let out = Command::new(env!("CARGO_BIN_EXE_page_dump"))
        .arg(&image)
        .arg("9")
        .output()
        .expect("运行 page_dump");
    assert_eq!(out.status.code(), Some(2));

    fs::remove_dir_all(&dir).expect("清理测试目录");
}
