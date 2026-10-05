//! `page_dump`：页转储（REQ-STO-012：P2 必须交付）。
//!
//! 只读输出：页头各字段、槽位目录（状态 / 偏移 / 行长）、按槽位状态
//! 给出的记录摘要。用于核对页格式与槽位目录——**不修改任何数据**。

use std::fmt::Write as _;

use bicdb_storage::page::{Page, SlotStatus, PAGE_SIZE, TAIL_LEN};
use bicdb_storage::row::{self, FragmentView, HeadFragment, RowView};

/// 一页的转储文本。
#[must_use]
pub fn page_dump(page: &Page) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "== 页转储（{PAGE_SIZE} 字节；校验 {:?}）==",
        page.verify()
    );
    match page.header() {
        Some(h) => {
            let _ = writeln!(
                s,
                "描述区：类型 = {:?}（{}）  格式版本 = {}  标志 = {:#04x}（初始化位 {}）",
                h.page_type,
                h.page_type.as_u8(),
                h.format_version,
                h.flags,
                page.is_initialized()
            );
            let _ = writeln!(
                s,
                "        workspace_ref = {}  file_id = {}  block_id = {}",
                hex(&h.workspace_ref),
                h.file_id,
                h.block_id
            );
            let _ = writeln!(
                s,
                "        page_lsn = {}  mod_seq = {}  校验和 = {:#010x}",
                h.page_lsn,
                h.mod_seq,
                page.stored_checksum()
            );
            let _ = writeln!(
                s,
                "事务区：itl_count = {}；空间区：slot_count = {}  free_start = {}  free_end = {}  可用 = {}",
                h.itl_count,
                h.slot_count,
                page.free_start(),
                page.free_end(),
                page.free_space()
            );
        }
        None => {
            let _ = writeln!(s, "页头无法解码（页类型字段非法）");
            return s;
        }
    }
    let _ = writeln!(s, "槽位目录：");
    let slot_count = page.slot_count();
    for i in 0..slot_count as usize {
        let Some(slot) = page.slot(i) else {
            let _ = writeln!(s, "  [{:>4}] （目录越界/页损坏）", i + 1);
            continue;
        };
        let row_no = i as u16 + 1;
        let status = match slot.status() {
            SlotStatus::Free => "空闲",
            SlotStatus::Normal => "正常行",
            SlotStatus::Forwarding => "转发指针",
            SlotStatus::FragmentHead => "片段",
        };
        let start = usize::from(slot.offset());
        let len = if slot.status() == SlotStatus::Free {
            None
        } else {
            record_len(page, start)
        };
        match len {
            Some(len) => {
                let _ = writeln!(
                    s,
                    "  行{row_no:<5} {status}  偏移 {start:<6} 长度 {len}  {}",
                    summary(page, start, slot.status())
                );
            }
            None => {
                let _ = writeln!(s, "  行{row_no:<5} {status}  偏移 {start}");
            }
        }
    }
    let _ = writeln!(
        s,
        "页尾副本：{}",
        hex(&page.as_bytes()[PAGE_SIZE - TAIL_LEN..])
    );
    s
}

fn record_len(page: &Page, start: usize) -> Option<usize> {
    let bytes = page.as_bytes();
    if start + 4 > PAGE_SIZE {
        return None;
    }
    Some(u16::from_le_bytes(bytes[start + 2..start + 4].try_into().ok()?) as usize)
}

fn summary(page: &Page, start: usize, status: SlotStatus) -> String {
    let bytes = page.as_bytes();
    let len = match record_len(page, start) {
        Some(l) if start + l <= PAGE_SIZE => l,
        _ => return "<长度越界>".to_owned(),
    };
    let record = &bytes[start..start + len];
    match status {
        SlotStatus::Normal => match RowView::new(record) {
            Ok(v) => format!(
                "flags={:#04x} itl={} 列 {} 变长 {}",
                v.header().flags,
                v.itl_slot(),
                v.header().col_count,
                v.header().var_col_count
            ),
            Err(e) => format!("<解析失败：{e}>"),
        },
        SlotStatus::Forwarding => match row::forwarding_pointer(record) {
            Ok(id) => format!("→ 块{} 行{}", id.block_id(), id.row_id()),
            Err(e) => format!("<解析失败：{e}>"),
        },
        SlotStatus::FragmentHead => {
            if let Ok(h) = HeadFragment::new(record) {
                match h.next() {
                    Some(n) => format!("头片段 → 块{} 行{}", n.block_id(), n.row_id()),
                    None => "单片段（下一片段 = NULL）".to_owned(),
                }
            } else if let Ok(v) = FragmentView::new(record) {
                match v.next() {
                    Some(n) => format!("片段 → 块{} 行{}", n.block_id(), n.row_id()),
                    None => "尾片段".to_owned(),
                }
            } else {
                "<解析失败>".to_owned()
            }
        }
        SlotStatus::Free => String::new(),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02x} "))
        .collect::<String>()
        .trim_end()
        .to_owned()
}

/// 页镜像字节（N × 16 KiB）的转储。
#[must_use]
pub fn page_image_dump(bytes: &[u8]) -> String {
    let mut s = String::new();
    for (i, chunk) in bytes.chunks(PAGE_SIZE).enumerate() {
        let _ = writeln!(s, "--- 镜像页 {i}（{} 字节）---", chunk.len());
        if chunk.len() < PAGE_SIZE {
            let _ = writeln!(s, "（末页截断，不足 {PAGE_SIZE} 字节）");
            continue;
        }
        let mut buf = Box::new([0u8; PAGE_SIZE]);
        buf.copy_from_slice(chunk);
        s.push_str(&page_dump(&Page::from_bytes(buf)));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use bicdb_storage::heap::{self, InsertPolicy};
    use bicdb_storage::page::PageType;
    use bicdb_storage::row::assemble_row;

    #[test]
    fn corrupt_itl_count_does_not_panic() {
        // 审核修复回归（F2）：损坏的 `itl_count`（超出格式上限）——转储必须
        // 降级输出，而不是在槽位访问处 panic。
        let mut page = Page::new(PageType::HeapTable, [0xAB; 8], 3, 7);
        let mut header = page.header().unwrap();
        header.itl_count = 60000;
        page.write_header(&header);
        page.seal();
        let text = page_dump(&page);
        assert!(text.contains("HeapTable"), "{text}");
    }

    #[test]
    fn dump_shows_header_and_slots() {
        let mut page = Page::new(PageType::HeapTable, [0xAB; 8], 3, 7);
        let policy = InsertPolicy::append_only();
        heap::insert_row(
            &mut page,
            &assemble_row(0, 2, &[false], &[], &[b"hello"]).unwrap(),
            &policy,
        )
        .unwrap();
        heap::delete_row(&mut page, 1).unwrap();
        heap::insert_row(
            &mut page,
            &assemble_row(0, 0, &[], &[], &[]).unwrap(),
            &policy,
        )
        .unwrap();
        page.seal();

        let text = page_dump(&page);
        assert!(text.contains("HeapTable"), "{text}");
        assert!(text.contains("block_id = 7"));
        assert!(text.contains("正常行"));
        assert!(text.contains("空闲"));
        assert!(text.contains("页尾副本"));
        assert!(text.contains("校验 Ok"), "{text}");
    }

    #[test]
    fn image_dump_marks_truncation() {
        let page = Page::new(PageType::HeapTable, [1; 8], 1, 1);
        let mut image = page.as_bytes().to_vec();
        image.truncate(PAGE_SIZE - 5);
        let text = page_image_dump(&image);
        assert!(text.contains("末页截断"), "{text}");
    }
}
