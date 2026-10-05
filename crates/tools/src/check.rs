//! 检查器：页结构一致性与损坏识别（REQ-STO-012：P2 必须交付 `db_check`；
//! 损坏处理必须给出**明确判定**）。
//!
//! # 判定三档（REQ-STO-012 原文）
//!
//! | 判定 | 含义 | 本阶段可达性 |
//! | --- | --- | --- |
//! | [`Verdict::Usable`] | 可用 | ✅ |
//! | [`Verdict::Repairable`] | 可修复 | ⏳ **P2 不可达**——修复路径（页尾重写、段级重建等）属 P3+；枚举先立、判定后到 |
//! | [`Verdict::Unrecoverable`] | 不可恢复 | ✅（完整性失败 = "按损坏处理：报错、告警、从备份恢复"，存储架构 §11.5.4） |
//!
//! **不静默返回数据**：任何 `Error` 级发现都给出不可恢复判定。
//!
//! # 检查项（本阶段 = 已定义的那些）
//!
//! - 页级：两层完整性（页尾副本 + CRC32C）、初始化位、槽位目录越界、
//!   空闲区指针次序、**按槽位状态解析记录**（正常行 / 片段 / 转发指针）、
//!   **记录不重叠**、片段位的自洽（普通行不得带 `FRAGMENT` 位）；
//! - 堆级：**片段链**（§6.3 检查器口径：链可达、尾端为 NULL、无环、
//!   无自引用）、多父引用、**孤立片段**（warning）。
//!
//! "检查器的逻辑检查项完整清单"仍是待冻结项 2d；这里实现**已定义**部分。

use std::fmt;

use bicdb_storage::heap::{self, Heap};
use bicdb_storage::page::{Page, PageCheck, SlotStatus, MAX_SLOTS, PAGE_SIZE};
use bicdb_storage::row::{
    self, FragmentView, HeadFragment, RowView, FORWARDING_LEN, FRAGMENT_HEADER_LEN,
    ROW_HEADER_FIXED_LEN,
};
use bicdb_storage::rowid::RowId;

/// 发现级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// 警告：结构自洽但可疑（如孤立片段）。
    Warning,
    /// 错误：数据不可信。
    Error,
}

/// 一条发现（结构化行：级别 │ 代码 │ 位置 │ 说明）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// 级别。
    pub severity: Severity,
    /// 稳定代码（回归与脚本按它断言）。
    pub code: &'static str,
    /// 位置（块/槽位；镜像级检查时可为空）。
    pub location: String,
    /// 说明。
    pub detail: String,
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let level = match self.severity {
            Severity::Warning => "警告",
            Severity::Error => "错误",
        };
        write!(
            f,
            "[{level}] {} @ {} — {}",
            self.code, self.location, self.detail
        )
    }
}

/// 检查判定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// 可用。
    Usable,
    /// 可修复（P2 不可达，见模块文档）。
    Repairable,
    /// 不可恢复。
    Unrecoverable,
}

/// 检查报告（每条发现一行 + 按类计数汇总）。
#[derive(Debug, Default)]
pub struct CheckReport {
    /// 发现列表。
    pub findings: Vec<Finding>,
    /// 已检查页数。
    pub pages_checked: usize,
    /// 已检查记录数。
    pub rows_checked: usize,
    /// 已检查片段链数。
    pub chains_checked: usize,
}

impl CheckReport {
    /// 汇总判定：无 `Error` → 可用（警告不改变判定）。
    #[must_use]
    pub fn verdict(&self) -> Verdict {
        if self.findings.iter().any(|f| f.severity == Severity::Error) {
            Verdict::Unrecoverable
        } else {
            Verdict::Usable
        }
    }

    /// 按代码计数（报告汇总段）。
    #[must_use]
    pub fn counts(&self) -> Vec<(&'static str, usize)> {
        let mut out: Vec<(&'static str, usize)> = Vec::new();
        for f in &self.findings {
            match out.iter_mut().find(|(c, _)| *c == f.code) {
                Some((_, n)) => *n += 1,
                None => out.push((f.code, 1)),
            }
        }
        out.sort_unstable();
        out
    }

    fn push(
        &mut self,
        severity: Severity,
        code: &'static str,
        location: impl Into<String>,
        detail: impl Into<String>,
    ) {
        self.findings.push(Finding {
            severity,
            code,
            location: location.into(),
            detail: detail.into(),
        });
    }

    /// 文本报告（每条发现一行 + 汇总段）。
    #[must_use]
    pub fn render(&self) -> String {
        let mut s = String::new();
        for f in &self.findings {
            s.push_str(&f.to_string());
            s.push('\n');
        }
        let errors = self
            .findings
            .iter()
            .filter(|f| f.severity == Severity::Error)
            .count();
        let warnings = self.findings.len() - errors;
        s.push_str(&format!(
            "汇总：页 {} / 记录 {} / 片段链 {}；发现 {}（错误 {errors} / 警告 {warnings}）→ 判定 {:?}\n",
            self.pages_checked,
            self.rows_checked,
            self.chains_checked,
            self.findings.len(),
            self.verdict()
        ));
        for (code, n) in self.counts() {
            s.push_str(&format!("  {code}: {n}\n"));
        }
        s
    }
}

/// 单页结构检查（页级；不含跨页的片段链分析）。
pub fn check_page(page: &Page, report: &mut CheckReport, location: &str) {
    report.pages_checked += 1;

    // 1. 两层完整性：先页尾（廉价）后校验和。
    match page.verify() {
        PageCheck::Ok => {}
        PageCheck::FracturedBlock => {
            report.push(
                Severity::Error,
                "INTEGRITY_TAIL",
                location,
                "头尾副本不一致（断裂块——只写了一半）",
            );
            return;
        }
        PageCheck::ChecksumMismatch => {
            report.push(
                Severity::Error,
                "INTEGRITY_CHECKSUM",
                location,
                "CRC32C 不符（数据损坏）",
            );
            return;
        }
        PageCheck::UnknownPageType => {
            report.push(
                Severity::Error,
                "FORMAT_UNKNOWN_TYPE",
                location,
                "页类型无法解码",
            );
            return;
        }
    }

    let Some(header) = page.header() else {
        report.push(
            Severity::Error,
            "FORMAT_UNKNOWN_TYPE",
            location,
            "页类型无法解码",
        );
        return;
    };

    // 2. 初始化位（未完成的页不得使用）。
    if !page.is_initialized() {
        report.push(
            Severity::Error,
            "PAGE_NOT_INITIALIZED",
            location,
            "未置初始化完成位",
        );
    }

    // 3. 头部与空闲区次序。
    let floor = page.row_area_floor();
    let bytes = page.as_bytes();
    if page.fixed_header_end() > floor {
        report.push(
            Severity::Error,
            "HEADER_OVERFLOW",
            location,
            format!(
                "固定头末尾 {} 越过行区下界 {floor}",
                page.fixed_header_end()
            ),
        );
        return;
    }
    let free_start = page.free_start();
    let free_end = page.free_end();
    if free_start > free_end || free_end > floor {
        report.push(
            Severity::Error,
            "FREE_SPACE_INVERTED",
            location,
            format!("空闲区指针非法：[{free_start}, {free_end})，行区下界 {floor}"),
        );
        return;
    }
    if header.slot_count as usize > MAX_SLOTS {
        report.push(
            Severity::Error,
            "SLOT_COUNT_OVER",
            location,
            format!("槽位数 {} 超过上限 {MAX_SLOTS}", header.slot_count),
        );
        return;
    }

    // 4. 逐槽位解析记录、检查区间与重叠。
    let mut spans: Vec<(usize, usize, u16)> = Vec::new();
    for i in 0..header.slot_count as usize {
        let Some(slot) = page.slot(i) else {
            report.push(
                Severity::Error,
                "SLOT_DIR_OUT_OF_PAGE",
                location,
                format!("槽位目录第 {i} 项越出页尾（页损坏）"),
            );
            break;
        };
        let row_no = i as u16 + 1;
        let loc = format!("{location} 槽{row_no}");
        if slot.status() == SlotStatus::Free {
            continue;
        }
        let start = usize::from(slot.offset());
        if start < free_end || start + ROW_HEADER_FIXED_LEN > floor {
            report.push(
                Severity::Error,
                "ROW_OUT_OF_BOUNDS",
                &loc,
                format!("记录起点 {start} 不在行区 [{free_end}, {floor})"),
            );
            continue;
        }
        let len =
            u16::from_le_bytes(bytes[start + 2..start + 4].try_into().expect("2 字节")) as usize;
        let end = start + len;
        if end > floor {
            report.push(
                Severity::Error,
                "ROW_OUT_OF_BOUNDS",
                &loc,
                format!("记录 [{start}, {end}) 越过行区下界 {floor}"),
            );
            continue;
        }
        let record = &bytes[start..end];
        if record.is_empty() {
            report.push(Severity::Error, "ROW_EMPTY", &loc, "槽位指向零长度记录");
            continue;
        }
        match slot.status() {
            SlotStatus::Normal => {
                if record[0] & row::row_flags::FRAGMENT != 0 {
                    report.push(
                        Severity::Error,
                        "FLAG_STATUS_MISMATCH",
                        &loc,
                        "普通行槽位带 FRAGMENT 位",
                    );
                }
                if let Err(e) =
                    RowView::new(record).and_then(|v| v.validate_var_offsets(0).map(|_| v))
                {
                    // 变长偏移的取值校验需要列定义（fixed_len）——页级只能做"可解析"检查。
                    let _ = e;
                }
                if RowView::new(record).is_err() {
                    report.push(
                        Severity::Error,
                        "ROW_UNPARSABLE",
                        &loc,
                        "记录无法按完整行解析",
                    );
                }
            }
            SlotStatus::Forwarding => {
                if record.len() != FORWARDING_LEN || row::forwarding_pointer(record).is_err() {
                    report.push(
                        Severity::Error,
                        "FORWARDING_BAD",
                        &loc,
                        format!("转发指针须恰 {FORWARDING_LEN} 字节"),
                    );
                }
            }
            SlotStatus::FragmentHead => {
                if record[0] & row::row_flags::FRAGMENT == 0 {
                    report.push(
                        Severity::Error,
                        "FLAG_STATUS_MISMATCH",
                        &loc,
                        "片段槽位未置 FRAGMENT 位",
                    );
                }
                if record.len() < FRAGMENT_HEADER_LEN {
                    report.push(
                        Severity::Error,
                        "FRAGMENT_UNPARSABLE",
                        &loc,
                        "片段记录不足短行头",
                    );
                } else if FragmentView::new(record).is_err() && HeadFragment::new(record).is_err() {
                    report.push(Severity::Error, "FRAGMENT_UNPARSABLE", &loc, "片段无法解析");
                }
            }
            SlotStatus::Free => unreachable!(),
        }
        report.rows_checked += 1;
        spans.push((start, end, row_no));
    }

    // 重叠检测（按起点排序后相邻比对）。
    spans.sort_unstable();
    for w in spans.windows(2) {
        let (s1, e1, n1) = w[0];
        let (s2, _e2, n2) = w[1];
        if s2 < e1 {
            report.push(
                Severity::Error,
                "ROW_OVERLAP",
                format!("{location} 槽{n1}/槽{n2}"),
                format!("记录区间重叠：[{s1}, {e1}) 与从 {s2} 开始"),
            );
        }
    }
}

/// 堆检查（多页 + 片段链）。
pub fn check_heap(heap: &Heap) -> CheckReport {
    let mut report = CheckReport::default();
    for (block_id, page) in heap.pages_iter() {
        check_page(page, &mut report, &format!("块{block_id}"));
    }
    if report
        .findings
        .iter()
        .any(|f| f.severity == Severity::Error)
    {
        // 页级已出错：不再做链分析（结论不会更可信）。
        return report;
    }
    check_fragment_chains(heap, &mut report);
    report
}

/// 片段链检查（§6.3 检查器口径：链可达、尾端为 NULL、无环、无自引用）。
///
/// 头/中的区分按 `row_flags` 的 **`FRAGMENT_HEAD` 位**（保留位 3 启用；
/// 见 `storage::row::row_flags`）——仅凭字节形状无法可靠区分（中片段的
/// next 指针字节可伪装成完整行头）。
fn check_fragment_chains(heap: &Heap, report: &mut CheckReport) {
    struct Frag {
        id: RowId,
        is_head_shape: bool,
        next: Option<RowId>,
    }
    let mut frags: Vec<Frag> = Vec::new();
    for (block_id, page) in heap.pages_iter() {
        let file_id = page.header().map_or(1, |h| h.file_id);
        for row_no in 1..=page.slot_count() {
            if heap::slot_status(page, row_no) != Some(SlotStatus::FragmentHead) {
                continue;
            }
            let Some(bytes) = heap::row(page, row_no) else {
                continue;
            };
            let Ok(id) = RowId::from_parts(file_id, block_id, row_no) else {
                continue;
            };
            if bytes.is_empty() {
                continue; // 零长片段：无标志位可读（`heap::row` 对畸形槽可能返回空）
            }
            let is_head_shape = bytes[0] & row::row_flags::FRAGMENT_HEAD != 0;
            let next = if is_head_shape {
                HeadFragment::new(bytes).ok().and_then(|h| h.next())
            } else {
                FragmentView::new(bytes).ok().and_then(|v| v.next())
            };
            frags.push(Frag {
                id,
                is_head_shape,
                next,
            });
        }
    }

    let mut visited: Vec<RowId> = Vec::new();

    // 每个链首都走一遍（链首被"他人"引用 = 多父引用；自引用由行走捕获为环）。
    for entry in frags.iter().filter(|f| f.is_head_shape) {
        let loc = fmt_rowid(entry.id);
        if frags
            .iter()
            .any(|f| f.id != entry.id && f.next == Some(entry.id))
        {
            report.push(
                Severity::Error,
                "FRAGMENT_MULTI_PARENT",
                loc.clone(),
                "链首被其他片段引用（应只有链内前驱指向它）",
            );
        }
        report.chains_checked += 1;
        visited.push(entry.id);
        let mut seen = vec![entry.id];
        let mut cur = entry.next;
        while let Some(id) = cur {
            if seen.contains(&id) {
                report.push(
                    Severity::Error,
                    "FRAGMENT_LOOP",
                    loc.clone(),
                    format!("链上成环（回到 {}）", fmt_rowid(id)),
                );
                break;
            }
            let Some(f) = frags.iter().find(|f| f.id == id) else {
                report.push(
                    Severity::Error,
                    "FRAGMENT_UNREACHABLE",
                    loc.clone(),
                    format!("next 指向不存在的 {}", fmt_rowid(id)),
                );
                break;
            };
            if f.is_head_shape {
                report.push(
                    Severity::Error,
                    "FRAGMENT_MULTI_PARENT",
                    loc.clone(),
                    format!("{} 带完整行头，不应出现在链中段", fmt_rowid(id)),
                );
                break;
            }
            seen.push(id);
            visited.push(id);
            cur = f.next;
        }
    }
    // 孤立片段：不属于任何可达链（既不是链首、也没被走过）。
    for f in &frags {
        if !visited.contains(&f.id) {
            report.push(
                Severity::Warning,
                "FRAGMENT_ORPHAN",
                fmt_rowid(f.id),
                "片段不属于任何可达链（可能为删除残留，待回收）",
            );
        }
    }
}

fn fmt_rowid(id: RowId) -> String {
    format!("块{} 行{}", id.block_id(), id.row_id())
}

/// 对一段**页镜像字节**（N × 16 KiB）做检查；末尾不足一页即报截断。
pub fn check_page_image(bytes: &[u8]) -> CheckReport {
    let mut report = CheckReport::default();
    if bytes.is_empty() {
        report.push(Severity::Error, "IMAGE_EMPTY", "", "镜像为空");
        return report;
    }
    if bytes.len() % PAGE_SIZE != 0 {
        report.push(
            Severity::Error,
            "IMAGE_TRUNCATED",
            "",
            format!("末页不足 {PAGE_SIZE} 字节（文件截断）"),
        );
    }
    for (i, chunk) in bytes.chunks(PAGE_SIZE).enumerate() {
        if chunk.len() < PAGE_SIZE {
            break;
        }
        let mut buf = Box::new([0u8; PAGE_SIZE]);
        buf.copy_from_slice(chunk);
        check_page(&Page::from_bytes(buf), &mut report, &format!("页{i}"));
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use bicdb_storage::fragment;
    use bicdb_storage::heap::InsertPolicy;
    use bicdb_storage::page::{PageType, TAIL_LEN};
    use bicdb_storage::row::assemble_row;

    fn row_of(payload: &[u8]) -> Vec<u8> {
        assemble_row(0, 0, &[false], &[], &[payload]).unwrap()
    }

    #[test]
    fn clean_heap_is_usable() {
        let mut heap = Heap::new([1; 8], InsertPolicy::append_only());
        for i in 0..5u8 {
            heap.insert(&row_of(&[b'x' + i; 100])).unwrap();
        }
        let report = check_heap(&heap);
        assert_eq!(report.verdict(), Verdict::Usable, "{}", report.render());
        assert!(report.findings.is_empty());
        assert_eq!(report.pages_checked, 1);
        assert_eq!(report.rows_checked, 5);
    }

    #[test]
    fn fragmented_heap_passes_chain_checks() {
        let policy = InsertPolicy::append_only();
        let mut heap = Heap::new([2; 8], policy);
        let big = row_of(&vec![9u8; PAGE_SIZE * 2]);
        fragment::insert_row(&mut heap, &big, &policy).unwrap();
        let report = check_heap(&heap);
        assert_eq!(report.verdict(), Verdict::Usable, "{}", report.render());
        assert_eq!(report.chains_checked, 1);
        assert!(report.findings.is_empty(), "{}", report.render());
    }

    #[test]
    fn broken_chain_pointer_is_reported() {
        let policy = InsertPolicy::append_only();
        let mut heap = Heap::new([3; 8], policy);
        let big = row_of(&vec![9u8; PAGE_SIZE + 100]);
        let id = fragment::insert_row(&mut heap, &big, &policy).unwrap();
        let head_len = HeadFragment::new(heap.get(id).unwrap())
            .unwrap()
            .header()
            .data_start();
        let bogus = RowId::from_parts(1, 999, 1).unwrap();
        heap.patch_record(id, head_len, &bogus.to_bytes()).unwrap();

        let report = check_heap(&heap);
        assert_eq!(report.verdict(), Verdict::Unrecoverable);
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.code == "FRAGMENT_UNREACHABLE"),
            "{}",
            report.render()
        );
    }

    #[test]
    fn self_reference_is_reported() {
        let policy = InsertPolicy::append_only();
        let mut heap = Heap::new([4; 8], policy);
        let big = row_of(&vec![9u8; PAGE_SIZE + 100]);
        let id = fragment::insert_row(&mut heap, &big, &policy).unwrap();
        let head_len = HeadFragment::new(heap.get(id).unwrap())
            .unwrap()
            .header()
            .data_start();
        heap.patch_record(id, head_len, &id.to_bytes()).unwrap();

        let report = check_heap(&heap);
        assert!(
            report.findings.iter().any(|f| f.code == "FRAGMENT_LOOP"),
            "{}",
            report.render()
        );
    }

    #[test]
    fn orphan_fragment_is_a_warning() {
        let policy = InsertPolicy::append_only();
        let mut heap = Heap::new([5; 8], policy);
        let big = row_of(&vec![9u8; PAGE_SIZE + 100]);
        let id = fragment::insert_row(&mut heap, &big, &policy).unwrap();
        // 把头片段的 next 清空：其余片段成为孤立片段。
        let head_len = HeadFragment::new(heap.get(id).unwrap())
            .unwrap()
            .header()
            .data_start();
        heap.patch_record(id, head_len, &[0u8; 6]).unwrap();

        let report = check_heap(&heap);
        assert_eq!(report.verdict(), Verdict::Usable, "孤立片段只是警告");
        assert!(
            report.findings.iter().any(|f| f.code == "FRAGMENT_ORPHAN"),
            "{}",
            report.render()
        );
    }

    #[test]
    fn tampered_page_is_unrecoverable() {
        let mut page = Page::new(PageType::HeapTable, [6; 8], 1, 1);
        let row = row_of(b"hello");
        heap::insert_row(&mut page, &row, &InsertPolicy::append_only()).unwrap();
        page.seal();
        // 篡改行区一个字节（校验和会抓住）。
        let floor = page.row_area_floor();
        page.as_bytes_mut()[floor - 1] ^= 0x01;

        let mut report = CheckReport::default();
        check_page(&page, &mut report, "页0");
        assert_eq!(report.verdict(), Verdict::Unrecoverable);
        assert_eq!(report.findings[0].code, "INTEGRITY_CHECKSUM");
    }

    #[test]
    fn fractured_block_is_reported_before_checksum() {
        let mut page = Page::new(PageType::HeapTable, [6; 8], 1, 1);
        page.as_bytes_mut()[PAGE_SIZE - 2] ^= 0xFF; // 破坏页尾副本
        let mut report = CheckReport::default();
        check_page(&page, &mut report, "页0");
        assert_eq!(report.findings[0].code, "INTEGRITY_TAIL");
    }

    #[test]
    fn overlapping_rows_are_detected() {
        let mut page = Page::new(PageType::HeapTable, [6; 8], 1, 1);
        let policy = InsertPolicy::append_only();
        heap::insert_row(&mut page, &row_of(b"aaaa"), &policy).unwrap();
        heap::insert_row(&mut page, &row_of(b"bbbb"), &policy).unwrap();
        // 把第 2 个槽位的偏移改成与第 1 个相同（人为重叠）。
        let first_offset = page.slot(0).unwrap().offset();
        let entry = bicdb_storage::page::SlotEntry::new(first_offset, SlotStatus::Normal).unwrap();
        page.set_slot(1, entry);
        page.seal();

        let mut report = CheckReport::default();
        check_page(&page, &mut report, "页0");
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.code == "ROW_OVERLAP" || f.code == "ROW_INSIDE_FREE_SPACE"),
            "{}",
            report.render()
        );
        assert_eq!(report.verdict(), Verdict::Unrecoverable);
    }

    #[test]
    fn zero_length_record_is_reported_not_panicked() {
        // 审核修复回归（F1）：槽位指向零长度记录（`row_len` 字段为 0）——
        // 旧代码在 `record[0]` 直接 panic；现报具名损坏。
        let mut page = Page::new(PageType::HeapTable, [6; 8], 1, 1);
        let start = page.row_area_floor() - 16;
        page.set_free_end(start);
        page.set_slot_count(1).unwrap();
        page.set_slot(
            0,
            bicdb_storage::page::SlotEntry::new(
                start as u16,
                bicdb_storage::page::SlotStatus::Normal,
            )
            .unwrap(),
        );
        page.seal();

        let mut report = CheckReport::default();
        check_page(&page, &mut report, "页0");
        assert!(
            report.findings.iter().any(|f| f.code == "ROW_EMPTY"),
            "{}",
            report.render()
        );
    }

    #[test]
    fn page_image_checks_truncation_and_clean_pages() {
        // 一页干净镜像 + 一个截断的镜像。
        let mut page = Page::new(PageType::HeapTable, [7; 8], 1, 1);
        heap::insert_row(&mut page, &row_of(b"data"), &InsertPolicy::append_only()).unwrap();
        page.seal();
        let image = page.as_bytes().to_vec();
        let report = check_page_image(&image);
        assert_eq!(report.verdict(), Verdict::Usable, "{}", report.render());

        let truncated = &image[..PAGE_SIZE - 10];
        let report = check_page_image(truncated);
        assert!(report.findings.iter().any(|f| f.code == "IMAGE_TRUNCATED"));

        // 空镜像。
        assert_eq!(check_page_image(&[]).verdict(), Verdict::Unrecoverable);

        // 页头之后的行区下界与页尾长度一致（防回归）。
        assert_eq!(page.row_area_floor(), PAGE_SIZE - TAIL_LEN);
    }
}
