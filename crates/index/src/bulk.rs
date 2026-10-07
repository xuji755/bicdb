//! **批量灌树（自底向上建树）**——`目录详设` §5.3 ④ 的 CREATE INDEX 落点。
//!
//! ```text
//! 调用方：扫描全表逐行求键 → **外部排序**（temp 段）→ 本模块一次建树
//! 本模块：叶子层顺序装填（含叶链双向）→ 逐层向上装填 → 单页即根
//! ```
//!
//! **与逐行插入的区别**：逐行随机插入每次都要下行 + 分裂（页写放大、随机 I/O）；
//! 批量建树只**顺序**写每页一次（`arch/09` §9.1.5 的建索引路径），且填充率可控。
//!
//! **输入契约**：`entries` 必须**按键升序**（调用方排序阶段保证）；
//! **唯一索引**（`unique = true`）在装填时检出相邻等键 ⇒
//! [`IndexError::DuplicateKey`]（DDL 事务据此整体回滚——设计 §5.3 原话）；
//! 非唯一索引允许等键（不同 ROWID 的重复键合法）。

use bicdb_storage::page::{Page, PageType};
use bicdb_storage::rowid::RowId;

use crate::page::{Entry, IndexPageMut, MAX_ENTRY_LEN};
use crate::store::{no_link, rid_of, PageStore};
use crate::{IndexError, Tree};

/// 建树结果（诊断/统计用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BulkLoadReport {
    /// 新根页地址。
    pub root: RowId,
    /// 树高（0 = 根即叶）。
    pub height: u32,
    /// 叶页数。
    pub leaf_blocks: usize,
    /// 枝/根页数（高度 ≥ 1 时的上层页）。
    pub branch_blocks: usize,
    /// 灌入条目数。
    pub entries: usize,
}

/// 叶页的目标填充率（百分比）——建索引的页间留白（后续插入的余地）。
/// 100 = 装到放不下为止；默认 90（`arch/09` 的建索引留白口径）。
pub const DEFAULT_FILL_PERCENT: u8 = 90;

/// 进程级当前值（实例打开时设定一次；实例参数 `index.bulk_fill_percent`）。
static FILL_PERCENT: std::sync::atomic::AtomicU8 =
    std::sync::atomic::AtomicU8::new(DEFAULT_FILL_PERCENT);

/// 当前的建索引填充率（`CREATE INDEX`/重建走它——调用方不必各自传常量）。
#[must_use]
pub fn bulk_fill_percent() -> u8 {
    FILL_PERCENT.load(std::sync::atomic::Ordering::Relaxed)
}

/// **设定建索引填充率**（10–100%）。
///
/// # Errors
/// 越出 10..=100。
pub fn set_bulk_fill_percent(percent: u8) -> Result<(), &'static str> {
    if !(10..=100).contains(&percent) {
        return Err("bulk_fill_percent 要落在 10–100");
    }
    FILL_PERCENT.store(percent, std::sync::atomic::Ordering::Relaxed);
    Ok(())
}

impl<'s, S: PageStore> Tree<'s, S> {
    /// **批量灌树**（自底向上；见模块文档的输入契约）。
    ///
    /// 空输入 ⇒ 一张空叶页（= [`Tree::create`] 的形态）。
    /// 返回（树，建树报告）——报告供 DDL 侧记账（建了多少页）。
    pub fn bulk_load(
        store: &'s mut S,
        file_id: u16,
        ws: [u8; 8],
        entries: &[(Vec<u8>, RowId)],
        unique: bool,
        fill_percent: u8,
    ) -> Result<(Self, BulkLoadReport), IndexError> {
        if entries.is_empty() {
            let tree = Self::create(store, file_id, ws)?;
            let report = BulkLoadReport {
                root: tree.root(),
                height: 0,
                leaf_blocks: 1,
                branch_blocks: 0,
                entries: 0,
            };
            return Ok((tree, report));
        }
        let fill = fill_percent.clamp(1, 100) as usize;
        // 输入必须升序（相邻比较；重复键由 `unique` 决定是否拒绝）。
        for w in entries.windows(2) {
            match w[0].0.cmp(&w[1].0) {
                std::cmp::Ordering::Greater => return Err(IndexError::NotSorted),
                std::cmp::Ordering::Equal if unique => {
                    return Err(IndexError::DuplicateKey {
                        key: hex_prefix(&w[0].0),
                    })
                }
                _ => {}
            }
        }

        // ── 叶层：顺序装填 + 叶链 ──
        let mut children: Vec<(Vec<u8>, u32)> = Vec::new(); // （最小键, 块号）
        let mut i = 0usize;
        let mut prev_leaf: Option<u32> = None;
        let mut prev_prev: Option<u32> = None;
        let mut leaf_blocks = 0usize;
        while i < entries.len() {
            let block = store.allocate()?;
            let mut page = Page::new(PageType::IndexLeaf, ws, file_id, block);
            let mut k = pack_leaf(&mut page, &entries[i..], fill)?;
            debug_assert!(k > 0, "至少装一条（入口已校验条目长度）");
            // 高键 = 下一页的最小键（末页 ∞）。装填时已预留，但 `set_high_key`
            // 是**峰值**占用（旧高键 + 新键并存到 compact）⇒ 极端情形退一条重试。
            while k > 0 {
                let high = entries.get(i + k).map(|(key, _)| key.clone());
                if build_leaf_page(&mut page, &entries[i..i + k], high.clone())?.is_some() {
                    break;
                }
                k -= 1; // 退一条（该页留白；下一条进下一页）
            }
            if k == 0 {
                return Err(IndexError::NoSpace);
            }
            {
                let mut m = IndexPageMut::new(&mut page)?;
                m.set_links(
                    prev_leaf.map_or(no_link(), |b| rid_of(file_id, b)),
                    no_link(),
                )?;
            }
            store.write(block, &mut page)?;
            // 前一叶的 `link_next` 指向本页（读-改-写，与分裂同款）。
            if let Some(prev) = prev_leaf {
                let mut p = store.read(prev)?;
                {
                    let mut m = IndexPageMut::new(&mut p)?;
                    m.set_links(
                        prev_prev.map_or(no_link(), |b| rid_of(file_id, b)),
                        rid_of(file_id, block),
                    )?;
                }
                store.write(prev, &mut p)?;
            }
            children.push((entries[i].0.clone(), block));
            prev_prev = prev_leaf;
            prev_leaf = Some(block);
            leaf_blocks += 1;
            i += k;
        }

        // ── 上层：逐层装填，单页即根 ──
        let mut height = 0u32;
        let mut branch_blocks = 0usize;
        while children.len() > 1 {
            let is_last_level = fits_one_page(&children, false);
            let mut next: Vec<(Vec<u8>, u32)> = Vec::new();
            let mut j = 0usize;
            let mut first_of_level = true;
            while j < children.len() {
                let block = store.allocate()?;
                let leaf_page = false;
                let page_type = if is_last_level {
                    PageType::IndexRoot
                } else {
                    PageType::IndexBranch
                };
                let _ = leaf_page;
                let mut page = Page::new(page_type, ws, file_id, block);
                let mut k = pack_branch(&mut page, file_id, &children[j..], fill)?;
                while k > 1 {
                    let high = children.get(j + k).map(|(key, _)| key.clone());
                    if build_branch_page(&mut page, file_id, &children[j..j + k], high)?.is_some() {
                        break;
                    }
                    k -= 1;
                }
                if k == 0 {
                    return Err(IndexError::NoSpace);
                }
                store.write(block, &mut page)?;
                next.push((children[j].0.clone(), block));
                branch_blocks += 1;
                let _ = first_of_level;
                first_of_level = false;
                j += k;
            }
            children = next;
            height += 1;
            if is_last_level {
                break;
            }
        }
        let root = rid_of(file_id, children[0].1);
        let report = BulkLoadReport {
            root,
            height,
            leaf_blocks,
            branch_blocks,
            entries: entries.len(),
        };
        Ok((Self::from_parts(store, file_id, root, height), report))
    }
}

/// **重建一页叶**（init + 装 `items` + 落高键 + compact）。
///
/// 返回 `Some(())` = 成功；`None` = 高键放不下（调用方退一条重试——
/// `set_high_key` 是**峰值**占用：旧高键与新键并存到 `compact`）。
fn build_leaf_page(
    page: &mut Page,
    items: &[(Vec<u8>, RowId)],
    high: Option<Vec<u8>>,
) -> Result<Option<()>, IndexError> {
    let mut m = IndexPageMut::new(page)?;
    m.init(None, None)?;
    for (key, rid) in items {
        let e = Entry::Leaf {
            key: key.clone(),
            rowid: *rid,
        };
        m.insert_entry(&e)?;
    }
    if let Some(hk) = high {
        if m.set_high_key(Some(hk)).is_err() {
            return Ok(None);
        }
        m.compact()?; // 旧高键字节是死空间（分裂同款注记）
    }
    Ok(Some(()))
}

/// **重建一页枝/根**（`left_child` = 第一个子页）。
fn build_branch_page(
    page: &mut Page,
    file_id: u16,
    children: &[(Vec<u8>, u32)],
    high: Option<Vec<u8>>,
) -> Result<Option<()>, IndexError> {
    let mut m = IndexPageMut::new(page)?;
    m.init(Some(rid_of(file_id, children[0].1)), None)?;
    for (key, block) in &children[1..] {
        let e = Entry::Branch {
            prefix: key.clone(),
            child: rid_of(file_id, *block),
        };
        m.insert_entry(&e)?;
    }
    if let Some(hk) = high {
        if m.set_high_key(Some(hk)).is_err() {
            return Ok(None);
        }
        m.compact()?;
    }
    Ok(Some(()))
}

/// 装填一页叶（返回装入条数）；**为高键预留**（高键 = 下一条未装条目的键）。
fn pack_leaf(
    page: &mut Page,
    rest: &[(Vec<u8>, RowId)],
    fill_percent: usize,
) -> Result<usize, IndexError> {
    let mut m = IndexPageMut::new(page)?;
    m.init(None, None)?; // 先以 ∞ 高键建页（随后落真实高键）
    let total = m.free_space();
    let budget = total * fill_percent / 100;
    let mut used = 0usize;
    let mut k = 0usize;
    while k < rest.len() {
        let (key, rid) = &rest[k];
        let e = Entry::Leaf {
            key: key.clone(),
            rowid: *rid,
        };
        let need = e.encode(true).len();
        if need > MAX_ENTRY_LEN {
            return Err(IndexError::EntryTooLong {
                len: need,
                limit: MAX_ENTRY_LEN,
            });
        }
        // 预留高键：装下本条后，高键 = 下一条（若还有）。
        let reserve = match rest.get(k + 1) {
            Some((next_key, _)) => Entry::HighKey {
                key: Some(next_key.clone()),
            }
            .encode(true)
            .len(),
            None => 0,
        };
        if used + need + reserve > budget || m.insert_entry(&e).is_err() {
            break;
        }
        used += need;
        k += 1;
    }
    if k == 0 && !rest.is_empty() {
        // 一条都装不下 ⇒ 条目超页容量（单条上限的兜底判定）。
        let need = Entry::Leaf {
            key: rest[0].0.clone(),
            rowid: rest[0].1,
        }
        .encode(true)
        .len();
        return Err(IndexError::EntryTooLong {
            len: need,
            limit: MAX_ENTRY_LEN,
        });
    }
    Ok(k)
}

/// 装填一页枝/根（返回装入的子页数；`left_child` = 第一个子页）。
fn pack_branch(
    page: &mut Page,
    file_id: u16,
    rest: &[(Vec<u8>, u32)],
    fill_percent: usize,
) -> Result<usize, IndexError> {
    let mut m = IndexPageMut::new(page)?;
    m.init(Some(rid_of(file_id, rest[0].1)), None)?;
    let total = m.free_space();
    let budget = total * fill_percent / 100;
    let mut used = 0usize;
    let mut k = 1usize; // 第一个子页走 left_child
    while k < rest.len() {
        let (key, block) = &rest[k];
        let e = Entry::Branch {
            prefix: key.clone(),
            child: rid_of(file_id, *block),
        };
        let need = e.encode(false).len();
        let reserve = match rest.get(k + 1) {
            Some((next_key, _)) => Entry::HighKey {
                key: Some(next_key.clone()),
            }
            .encode(false)
            .len(),
            None => 0,
        };
        if used + need + reserve > budget || m.insert_entry(&e).is_err() {
            break;
        }
        used += need;
        k += 1;
    }
    Ok(k)
}

/// 这些子页能否全部装进**一页**（决定该层是否就是最后（根）层）。
fn fits_one_page(children: &[(Vec<u8>, u32)], _leaf: bool) -> bool {
    // 试装在一张临时页上（不落盘）；装得下 ⇒ 该层单页即根。
    let mut probe = Page::new(PageType::IndexRoot, [0u8; 8], 0, 0);
    match pack_branch(&mut probe, 0, children, 100) {
        Ok(k) => k >= children.len(),
        Err(_) => false,
    }
}

fn hex_prefix(key: &[u8]) -> String {
    let n = key.len().min(32);
    let mut s = String::with_capacity(n * 2);
    for b in &key[..n] {
        s.push_str(&format!("{b:02x}"));
    }
    s
}
