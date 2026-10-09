//! **B-link B+Tree**（§9.1：通用索引；分裂协议 §9.1.5）。
//!
//! ```text
//! 搜索：根 → 逐层 route（"最后一个 ≤ 键"的条目）→ 叶
//!       叶上：键 ≥ 高键 ⇒ **沿 link_next 右移**（父下链滞后时的自愈，§9.1.4）
//!       叶内：目录 2..n 二分（复合键 (key, ROWID)）
//! 分裂（单阶段）：P 满 ⇒ 建 P′（复制 P 的原高键；99/1 或 50/50；
//!       P.link_next=P′、P′.link_prev=P、P.高键←P′ 最小条目、Q.link_prev=P′）
//!       ⇒ 回插父层（**重下行找父页**，可能递归分裂）⇒ 根分裂则新根（高度 +1）
//! 扫描：叶链前进/后退（双向链）；不改高键、不合并空页（§9.1.2：空页留在树中）
//! ```
//!
//! **本实现是单线程形态**：latch 由调用方（未来的池/每页内容锁）承担；
//! 结构性规则（高键、右链、单阶段分裂、父层滞后容忍）已经就位——所以把
//! 并发加上去时**不需要改树形**，只需给页访问加锁并让右移带回退重试。
//!
//! **树头**（根页 ROWID + 高度）按 §9.1.5 应落在段头页的 B+Tree 扩展区；
//! 本模块把它作为 [`Tree`] 的状态显式携带（执行器落段头的那一步接入时读写它）。

use std::cmp::Ordering;

use bicdb_storage::page::{Page, PageType};
use bicdb_storage::rowid::RowId;

use bicdb_storage::page::SLOT_ENTRY_LEN;

use crate::page::{
    cmp_entries, Entry, IndexPage, IndexPageMut, BODY_HEAD_LEN, BODY_HEAD_OFFSET, MAX_ENTRY_LEN,
};
use crate::store::{no_link, rid_of, PageStore};
use crate::IndexError;

/// 分裂形态（§9.1.5：单调追加是常态路径）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitKind {
    /// 新条目是页内最大条目 ⇒ **99/1**（原页留守、新条目独立成新页——零搬移）。
    Append99,
    /// 否则 **50/50**（按字节对半）。
    Even,
}

/// 一次插入的结果（诊断/测试）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InsertOutcome {
    /// 是否发生了分裂。
    pub splits: Vec<SplitKind>,
    /// 树高是否增长（根分裂）。
    pub grew: bool,
}

/// **FFS 的区读上限**（§5.12：一次 `pread` ≤ 8 页 = 128 KiB）——默认值。
///
/// 实际取值走 [`bicdb_storage::scan::scan_run_pages`]：**与堆扫描同一个实例
/// 参数**（`storage.multiblock_read_pages`）。此前本常量与
/// `storage::scan::SCAN_RUN_PAGES` 是两个各写 8 的同义孪生——改一处会静默分叉。
pub const FFS_RUN_PAGES: u32 = 8;

/// **索引统计量**（[`Tree::statistics`] 的产物；持久化随目录域 `stat$`，
/// 本切片只计算——口径对照 Oracle `SYS.IND$`，证据包 `index-stats-20261006`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IndexStats {
    /// 叶条目数（= 被索引行数）。
    pub entries: u64,
    /// 叶页数（Oracle `LEAFCNT` 口径）。
    pub leaf_blocks: u64,
    /// 树高 − 1（Oracle `BLEVEL` 口径；空树 = 0——示例 `BLEVEL=1` = 高度 2）。
    pub blevel: u32,
    /// **聚簇因子**（Oracle `CLUFAC` 口径，Note:39836.1）：按索引序扫叶，
    /// 相邻条目的 `(file_id, block_id)` 变化则 +1（首条计 1）。
    /// 值域 [表块数, 行数]；空索引 = 0。
    pub clustering_factor: u64,
}

/// **分裂点校正**（§9.1.5 的收尾）：按形态取的分裂点必须保证**左半 +
/// 新高键 + 目录**放得进原页——高键在分裂时从 ∞（8B）变成真实键（可达 615B）
/// 时尤其明显：不校正会把"装不下的左半"留在原页（实测 NoSpace）。
/// 校正只把左多半的条目挪到右半（不改变分裂形态的性质）。
fn adjust_split(
    items: &mut Vec<Entry>,
    mut split: usize,
    leaf: bool,
    floor: usize,
    high_len: usize,
) -> usize {
    let fits = |split: usize, items: &Vec<Entry>| -> bool {
        let bytes: usize = items[..split].iter().map(|e| e.encode(leaf).len()).sum();
        // 页体头 + 目录（含高键）+ 高键 + 左半 ≤ 页尾下界
        BODY_HEAD_OFFSET + BODY_HEAD_LEN + (split + 1) * SLOT_ENTRY_LEN + high_len + bytes <= floor
    };
    while split > 1 && !fits(split, items) {
        split -= 1;
    }
    let _ = &mut *items;
    split
}

/// **B-link B+Tree**（通用索引）。
pub struct Tree<'s, S: PageStore> {
    store: &'s mut S,
    file_id: u16,
    /// 树头：根页地址。
    root: RowId,
    /// 高度（0 = 根即叶）。
    height: u32,
}

impl<'s, S: PageStore> Tree<'s, S> {
    /// **建树**：分配一张空叶页作为初始根（§9.1.5 第 8 步：空索引 = 空叶页）。
    pub fn create(store: &'s mut S, file_id: u16, ws: [u8; 8]) -> Result<Self, IndexError> {
        let block = store.allocate()?;
        let mut page = Page::new(PageType::IndexLeaf, ws, file_id, block);
        {
            let mut m = IndexPageMut::new(&mut page)?;
            m.init(None, None)?; // 空叶：高键 = ∞
            m.set_links(no_link(), no_link())?;
        }
        store.write(block, &mut page)?;
        Ok(Self {
            store,
            file_id,
            root: rid_of(file_id, block),
            height: 0,
        })
    }

    /// **打开既有树**（树头来自段头页扩展区，§9.1.5 第 8 步）。
    ///
    /// **高度由页类型推**：从根下行**最左子指针**直到叶页——根/枝页（类型
    /// 3/4）自述层级，树头因此只存 6B 的根页 ROWID（§5.11 的类型扩展区没有
    /// 高度字段）。空头（全 0）请先 [`Tree::create`] 建初始空叶页再写头。
    pub fn open(store: &'s mut S, file_id: u16, root: RowId) -> Result<Self, IndexError> {
        let mut height = 0u32;
        let mut block = crate::store::block_of(root);
        loop {
            let page = store.read(block)?;
            let view = IndexPage::new(&page)?;
            if view.is_leaf() {
                break;
            }
            block = crate::store::block_of(view.left_child());
            height += 1;
            if height > 64 {
                return Err(IndexError::Malformed("树高异常（疑似坏页/环）"));
            }
        }
        Ok(Self {
            store,
            file_id,
            root,
            height,
        })
    }

    /// 树头（根页地址）——执行器把它写进段头页的 B+Tree 扩展区。
    #[must_use]
    pub fn root(&self) -> RowId {
        self.root
    }

    /// 由已知（根、高度）装配（**批量灌树**用：页已由建树过程写好）。
    pub(crate) fn from_parts(store: &'s mut S, file_id: u16, root: RowId, height: u32) -> Self {
        Self {
            store,
            file_id,
            root,
            height,
        }
    }

    /// 高度。
    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }

    // -- 读路径 -------------------------------------------------------------

    /// **下行到键所在的叶**（逐层 `route`；不在内层右移——高键兜底在叶链上，
    /// §9.1.4/§9.1.5 的"过期的页只可能偏左"）。
    fn descend(&mut self, key: &[u8]) -> Result<u32, IndexError> {
        let mut block = crate::store::block_of(self.root);
        for _ in 0..self.height {
            let page = self.store.read(block)?;
            let view = IndexPage::new(&page)?;
            let (child, _left) = view.route(key)?;
            block = crate::store::block_of(child);
        }
        Ok(block)
    }

    /// 高键仅保存 key，不含 ROWID：先右移，再沿前驱校正重复键的复合下界。
    fn settle_leaf(
        &mut self,
        mut block: u32,
        key: &[u8],
        rowid: Option<RowId>,
    ) -> Result<u32, IndexError> {
        loop {
            let page = self.store.read(block)?;
            let view = IndexPage::new(&page)?;
            if !view.needs_right_move(key, rowid)? {
                break;
            }
            let (_, next) = view.links()?;
            if next == no_link() {
                break; // ∞ 高键已由 needs_right_move 排除；防御
            }
            block = crate::store::block_of(next);
        }
        let target = Entry::Leaf {
            key: key.to_vec(),
            rowid: rowid.unwrap_or_else(|| RowId::from_bytes(&[0u8; 6])),
        };
        let mut seen = std::collections::HashSet::new();
        let mut candidate = block;
        loop {
            if !seen.insert(block) {
                return Err(IndexError::Malformed("叶前驱链形成环"));
            }
            let page = self.store.read(block)?;
            let previous = IndexPage::new(&page)?.links()?.0;
            if previous == no_link() {
                return Ok(candidate);
            }
            let previous_block = crate::store::block_of(previous);
            let previous_page = self.store.read(previous_block)?;
            let previous_view = IndexPage::new(&previous_page)?;
            let count = usize::from(previous_view.entry_count());
            if count > 1 && cmp_entries(&previous_view.entry(count - 1)?, &target) == Ordering::Less
            {
                return Ok(candidate);
            }
            if count > 1 {
                candidate = previous_block;
            }
            block = previous_block;
        }
    }

    /// **点查**：返回该键的第一个 ROWID（重复键时最小者）。
    pub fn lookup(&mut self, key: &[u8]) -> Result<Option<RowId>, IndexError> {
        let block = self.descend(key)?;
        let block = self.settle_leaf(block, key, None)?;
        let page = self.store.read(block)?;
        let view = IndexPage::new(&page)?;
        let target = Entry::Leaf {
            key: key.to_vec(),
            rowid: RowId::from_bytes(&[0u8; 6]),
        };
        let (hit, at) = view.search(&target)?;
        if hit {
            if let Entry::Leaf { rowid, .. } = view.entry(at)? {
                return Ok(Some(rowid));
            }
        } else if at < usize::from(view.entry_count()) {
            // 未命中：检查插入位上是否恰好同键（(key, 最小 ROWID) 的定位语义）。
            if let Entry::Leaf { key: k, rowid } = view.entry(at)? {
                if k == key {
                    return Ok(Some(rowid));
                }
            }
        }
        Ok(None)
    }

    /// **范围扫描**（闭区间 `[from, to]`；`None` = 无界）——沿叶链前进。
    pub fn range(
        &mut self,
        from: Option<&[u8]>,
        to: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<(Vec<u8>, RowId)>, IndexError> {
        let start_block = match from {
            Some(k) => {
                let b = self.descend(k)?;
                self.settle_leaf(b, k, None)?
            }
            None => self.leftmost_leaf()?,
        };
        let mut out = Vec::new();
        let mut block = start_block;
        'outer: loop {
            let page = self.store.read(block)?;
            let view = IndexPage::new(&page)?;
            let n = usize::from(view.entry_count());
            for i in 1..n {
                if let Entry::Leaf { key, rowid } = view.entry(i)? {
                    if let Some(f) = from {
                        if key.as_slice() < f {
                            continue;
                        }
                    }
                    if let Some(t) = to {
                        if key.as_slice() > t {
                            break 'outer;
                        }
                    }
                    out.push((key, rowid));
                    if out.len() >= limit {
                        break 'outer;
                    }
                }
            }
            let (_, next) = view.links()?;
            if next == no_link() {
                break;
            }
            block = crate::store::block_of(next);
        }
        Ok(out)
    }

    /// **全扫描**（IFS：叶链顺序）。
    pub fn full_scan(&mut self, limit: usize) -> Result<Vec<(Vec<u8>, RowId)>, IndexError> {
        self.range(None, None, limit)
    }

    /// **快速全扫描 FFS**（§9.1.6）：按**物理块序**读全段索引页（连续块成组
    /// 区读），**只取叶页条目——不沿叶链、不保序、不承诺快照一致性**。
    ///
    /// 与 IFS 的分工：IFS 产出**有序流**（可直接满足 `ORDER BY`）；FFS 产出
    /// **无序**，只喂不接受顺序的消费方（统计/无条件聚合）。块的合法性由
    /// 页校验 + **页头自证**（块号/文件与请求一致）保证——不需要叶链协议。
    pub fn fast_full_scan(&mut self, limit: usize) -> Result<Vec<(Vec<u8>, RowId)>, IndexError> {
        let mut out = Vec::new();
        let blocks = self.store.blocks()?;
        let mut at = 0usize;
        while at < blocks.len() {
            // 连续块成组（区读）：枚举为升序 = 物理序，相邻且差 1 即可同组。
            let first = blocks[at];
            let mut count = 1u32;
            while at + (count as usize) < blocks.len()
                && blocks[at + count as usize] == first + count
                && count < bicdb_storage::scan::scan_run_pages()
            {
                count += 1;
            }
            let pages = self.store.read_run(first, count)?;
            if pages.len() != count as usize {
                return Err(IndexError::Malformed("区读返回的页数与请求不符"));
            }
            for (i, page) in pages.iter().enumerate() {
                // **块引用自证**：读回的页必须自述为请求的那一块（串页防线）。
                match page.header() {
                    Some(h) if h.block_id == first + i as u32 && h.file_id == self.file_id => {}
                    _ => {
                        return Err(IndexError::Malformed("FFS 读回的页自述身份与请求不符"));
                    }
                }
                // 非索引页 = 枚举实现缺陷（执行器应排除段头/位图页）——响亮失败。
                let view = IndexPage::new(page)?;
                if !view.is_leaf() {
                    continue; // 枝/根页：读但不取（Oracle FFS 同款，§9.1.6）
                }
                let n = usize::from(view.entry_count());
                for j in 1..n {
                    if let Entry::Leaf { key, rowid } = view.entry(j)? {
                        out.push((key, rowid));
                        if out.len() >= limit {
                            return Ok(out);
                        }
                    }
                }
            }
            at += count as usize;
        }
        Ok(out)
    }

    /// **索引统计**（沿叶链一趟；**零回表 I/O**——只比较条目自带的 ROWID 字节）。
    ///
    /// **聚簇因子**按 Oracle `CLUFAC` 口径（Note:39836.1，证据包
    /// `index-stats-20261006`）：**按索引序**扫叶条目，当前条目的
    /// `(file_id, block_id)` 与上一条**不同**则计数 +1（首条计 1）——
    /// 值域 [表块数, 行数]：接近块数 = 索引序与块序贴合，接近行数 = 随机。
    /// 用途：`CF × 选择率 ≈ 回表块成本`（只进**需回表**的范围扫描/IFS 成本）。
    pub fn statistics(&mut self) -> Result<IndexStats, IndexError> {
        let mut block = self.leftmost_leaf()?;
        let mut entries = 0u64;
        let mut leaf_blocks = 0u64;
        let mut clustering_factor = 0u64;
        let mut prev: Option<(u16, u32)> = None;
        loop {
            let page = self.store.read(block)?;
            let view = IndexPage::new(&page)?;
            if !view.is_leaf() {
                return Err(IndexError::Malformed("叶链上出现非叶页"));
            }
            leaf_blocks += 1;
            let n = usize::from(view.entry_count());
            for i in 1..n {
                if let Entry::Leaf { rowid, .. } = view.entry(i)? {
                    entries += 1;
                    let cur = (rowid.file_id(), rowid.block_id());
                    if prev != Some(cur) {
                        clustering_factor += 1;
                    }
                    prev = Some(cur);
                }
            }
            let (_, next) = view.links()?;
            if next == no_link() {
                // 链的尽头必须是 ∞ 页（与 `validate` 同款防线）。
                if view.high_key()?.is_some() {
                    return Err(IndexError::Malformed("叶链提前终止（尽头非 ∞ 页）"));
                }
                break;
            }
            block = crate::store::block_of(next);
        }
        Ok(IndexStats {
            entries,
            leaf_blocks,
            blevel: self.height,
            clustering_factor,
        })
    }

    /// 最左叶（下行时取最左子指针）。
    fn leftmost_leaf(&mut self) -> Result<u32, IndexError> {
        let mut block = crate::store::block_of(self.root);
        for _ in 0..self.height {
            let page = self.store.read(block)?;
            let view = IndexPage::new(&page)?;
            block = crate::store::block_of(view.left_child());
        }
        Ok(block)
    }

    // -- 写路径 -------------------------------------------------------------

    /// **插入**（`(key, ROWID)` 复合键；重复键按 ROWID 排序）。
    ///
    /// 99/1 或 50/50 分裂（§9.1.5）——**单调追加密钥序列命中 99/1 常态路径**。
    pub fn insert(&mut self, key: &[u8], rowid: RowId) -> Result<InsertOutcome, IndexError> {
        let entry = Entry::Leaf {
            key: key.to_vec(),
            rowid,
        };
        if entry.encode(true).len() > MAX_ENTRY_LEN {
            return Err(IndexError::EntryTooLong {
                len: entry.encode(true).len(),
                limit: MAX_ENTRY_LEN,
            });
        }
        // 叶定位（右移自愈）。
        let leaf_block = {
            let b = self.descend(key)?;
            self.settle_leaf(b, key, Some(rowid))?
        };
        // 插入或分裂。
        let mut page = self.store.read(leaf_block)?;
        {
            let mut m = IndexPageMut::new(&mut page)?;
            if m.insert_entry(&entry).is_ok() {
                self.store.write(leaf_block, &mut page)?;
                return Ok(InsertOutcome {
                    splits: Vec::new(),
                    grew: false,
                });
            }
            // 无空间：先压紧（回收删除空洞）再试。
            m.compact()?;
            if m.insert_entry(&entry).is_ok() {
                self.store.write(leaf_block, &mut page)?;
                return Ok(InsertOutcome {
                    splits: Vec::new(),
                    grew: false,
                });
            }
        }
        // 分裂（单阶段）。
        let (kind, new_block) = self.split_leaf(leaf_block, &mut page, &entry)?;
        // 回插父层（重下行找父页；可能递归分裂）；根分裂则长高。
        let grew = self.insert_into_parent(leaf_block, new_block, 0)?;
        Ok(InsertOutcome {
            splits: vec![kind],
            grew,
        })
    }

    /// **叶分裂**（§9.1.5 第 3–5 步）：返回（形态, 新页块号）。
    fn split_leaf(
        &mut self,
        block: u32,
        page: &mut bicdb_storage::page::Page,
        new_entry: &Entry,
    ) -> Result<(SplitKind, u32), IndexError> {
        // 形态判定：新条目是否大于页内现存最大有效条目。
        let kind = {
            let view = IndexPage::new(page)?;
            let n = usize::from(view.entry_count());
            let last = if n > 1 {
                Some(view.entry(n - 1)?)
            } else {
                None
            };
            match last {
                Some(l) if cmp_entries(new_entry, &l) == Ordering::Greater => SplitKind::Append99,
                None => SplitKind::Append99,
                _ => SplitKind::Even,
            }
        };
        // 目标条目集：原有效条目 + 新条目，按序。
        let mut items: Vec<Entry> = {
            let view = IndexPage::new(page)?;
            let n = usize::from(view.entry_count());
            let mut v = Vec::with_capacity(n);
            for i in 1..n {
                v.push(view.entry(i)?);
            }
            v
        };
        items.push(new_entry.clone());
        items.sort_by(cmp_entries);

        // 分配新页（类型 = 叶）。
        let new_block = self.store.allocate()?;
        let (ws, file_id) = {
            let h = page.header().expect("索引页");
            (h.workspace_ref, h.file_id)
        };
        let mut new_page =
            bicdb_storage::page::Page::new(PageType::IndexLeaf, ws, file_id, new_block);
        // 分裂点。
        let old_high = {
            let view = IndexPage::new(page)?;
            view.high_key()?
        };
        let floor = page.row_area_floor();
        let sep_key = match &items[items.len() - 1] {
            Entry::Leaf { key, .. } => key.clone(),
            Entry::Branch { prefix, .. } => prefix.clone(),
            Entry::HighKey { .. } => return Err(IndexError::Malformed("分裂条目非法")),
        };
        let high_len = Entry::HighKey {
            key: Some(sep_key.clone()),
        }
        .encode(true)
        .len();
        let split = match kind {
            SplitKind::Append99 => {
                let last = items.len() - 1;
                adjust_split(&mut items, last, true, floor, high_len)
            }
            SplitKind::Even => {
                let total: usize = items.iter().map(|e| e.encode(true).len()).sum();
                let mut acc = 0usize;
                let mut split = 1usize;
                for (i, e) in items.iter().enumerate() {
                    acc += e.encode(true).len();
                    if acc * 2 >= total {
                        split = (i + 1).min(items.len() - 1).max(1);
                        break;
                    }
                }
                adjust_split(&mut items, split, true, floor, high_len)
            }
        };
        let (left_items, right_items) = items.split_at(split);
        // **原页的自身链**（§9.1.1 三页协议）：P′ 插在 P 与 Q 之间——
        // P′ 继承 P 的**后继**，Q 的**前驱**改指 P′。这必须在 `keep_high_key_only`
        // （会把原页的链清零）**之前**读：漏掉继承时 P′ 恒以 no_link 收尾，
        // 链在 P′ 处提前终止（实测：仅当 P 非最右叶时暴露，单调追加掩盖了它）。
        let (old_prev, old_next) = {
            let view = IndexPage::new(page)?;
            view.links()?
        };
        // 新页：复制原高键（∞ 随之移转）+ 右半条目 + 链（P ← P′ → Q）。
        {
            let mut m = IndexPageMut::new(&mut new_page)?;
            m.init(None, old_high.clone())?;
            m.set_links(rid_of(self.file_id, block), old_next)?;
            for e in right_items {
                m.insert_entry(e)?;
            }
        }
        // 原页：高键 ← 右半最小条目 + 只留左半 + link_next = P′。
        {
            let mut m = IndexPageMut::new(page)?;
            // **次序要紧**：先把体归零（只留高键占位）→ **先写新上界** →
            // 再插左半。反过来（先插左半再改高键）会让"高键的两份拷贝"
            // 同时占页——满页分裂时必然放不下（实测 underflow/NoSpace）。
            m.keep_high_key_only()?;
            let new_high = match right_items.first() {
                Some(Entry::Leaf { key, .. }) => key.clone(),
                Some(Entry::Branch { prefix, .. }) => prefix.clone(),
                _ => return Err(IndexError::Malformed("分裂右半为空")),
            };
            m.set_high_key(Some(new_high.clone()))?;
            // **必须 compact**：`set_high_key` 把新高键追加在条目区顶部、旧高键
            // 的字节留在原处（死空间，直到下次 compact 才回收）——不回收的话
            // 左半可用的预算比 `adjust_split` 的估算少一份高键，满页分裂时
            // 插到一半 NoSpace（实测：607B 键 + 25 条左半恰好差 615B）。
            m.compact()?;
            for e in left_items {
                m.insert_entry(e)?;
            }
            m.set_links(old_prev, rid_of(self.file_id, new_block))?;
            self.store.write(block, page)?;
            // Q.link_prev = P′（§9.1.1 的"第三页"）。
            if old_next != no_link() {
                let q_block = crate::store::block_of(old_next);
                let mut q = self.store.read(q_block)?;
                // **先读后写**：Q 的自身链只读一次——若先写再读，读到的是
                // 刚写下的值（曾因此把 `link_next` 写成自指，叶链断裂）。
                let qn = {
                    let qv = IndexPage::new(&q)?;
                    qv.links()?.1
                };
                let mut qm = IndexPageMut::new(&mut q)?;
                qm.set_links(rid_of(self.file_id, new_block), qn)?;
                self.store.write(q_block, &mut q)?;
            }
        }
        // 新页落盘（放在最后：先建、后改原页已满足崩溃容忍；顺序对单线程无碍）。
        {
            let mut m = IndexPageMut::new(&mut new_page)?;
            // 右半最小条目的键 = 新页的最小键（父条目用它）。
            let _ = m.compact();
        }
        self.store.write(new_block, &mut new_page)?;
        Ok((kind, new_block))
    }

    /// **回插父层**（§9.1.5 第 6–7 步）：重下行找父页、插入 `(P′ 最小条目 → P′)`；
    /// 父满 ⇒ 递归分裂；根分裂 ⇒ 新根 + 高度 +1。
    fn insert_into_parent(
        &mut self,
        left_block: u32,
        right_block: u32,
        level: u32,
    ) -> Result<bool, IndexError> {
        // 新页的最小条目（父条目的键）。
        let right_page = self.store.read(right_block)?;
        let right_view = IndexPage::new(&right_page)?;
        let min_entry = if usize::from(right_view.entry_count()) > 1 {
            right_view.entry(1)?
        } else {
            return Ok(false); // 空右页（不该发生）：不动父层
        };
        let sep_key = match &min_entry {
            Entry::Leaf { key, .. } => key.clone(),
            Entry::Branch { prefix, .. } => prefix.clone(),
            Entry::HighKey { .. } => return Err(IndexError::Malformed("右页最小条目非法")),
        };
        let branch_entry = Entry::Branch {
            prefix: sep_key.clone(),
            child: rid_of(self.file_id, right_block),
        };
        // 被分裂的是**根**吗（§9.1.5 第 7 步）——按"它是不是当前根"判，
        // 不能按高度判：高度 ≥ 1 时根是分支页，其分裂同样会递归到这一层。
        if crate::store::block_of(self.root) == left_block {
            let old_root_block = left_block;
            let root_is_branch = self.height > 0;
            if root_is_branch {
                // 旧根改类型 4 → 3（根页与分支页结构相同，只是类型字节变化）。
                let mut old_root = self.store.read(old_root_block)?;
                {
                    let mut h = old_root.header().ok_or(IndexError::Malformed("页头缺失"))?;
                    h.page_type = PageType::IndexBranch;
                    old_root.write_header(&h);
                }
                self.store.write(old_root_block, &mut old_root)?;
            }
            // 高度 0：旧根是**叶**，保持类型 2——**不**改成分支（叶分裂出的
            // 两半都是叶；新根才是分支）。
            // 新根（类型 4）：left_child = 旧根 + 一条目。
            let new_root_block = self.store.allocate()?;
            let (ws, file_id) = {
                let p = self.store.read(old_root_block)?;
                let h = p.header().expect("页头");
                (h.workspace_ref, h.file_id)
            };
            let mut new_root =
                bicdb_storage::page::Page::new(PageType::IndexRoot, ws, file_id, new_root_block);
            {
                let mut m = IndexPageMut::new(&mut new_root)?;
                m.init(Some(rid_of(self.file_id, old_root_block)), None)?;
                m.insert_entry(&branch_entry)?;
            }
            self.store.write(new_root_block, &mut new_root)?;
            self.root = rid_of(self.file_id, new_root_block);
            self.height += 1;
            return Ok(true);
        }
        // 非根：下行到**父层**（被分裂节点在 `level`，其父在 `level + 1`）——
        // 按分隔键重下行，停在父页。
        let parent_block = self.descend_to_level(&sep_key, level + 1)?;
        let mut parent = self.store.read(parent_block)?;
        {
            let mut m = IndexPageMut::new(&mut parent)?;
            if m.insert_entry(&branch_entry).is_ok() {
                self.store.write(parent_block, &mut parent)?;
                return Ok(false);
            }
            m.compact()?;
            if m.insert_entry(&branch_entry).is_ok() {
                self.store.write(parent_block, &mut parent)?;
                return Ok(false);
            }
        }
        // 父页满 ⇒ 分裂父页（递归）。
        let (_, new_parent_block) = self.split_branch(parent_block, &mut parent, &branch_entry)?;
        self.insert_into_parent(parent_block, new_parent_block, level + 1)
    }

    /// 下行到第 `level` 层（0 = 叶层）的页；用于回插父层时重找父页。
    fn descend_to_level(&mut self, key: &[u8], level: u32) -> Result<u32, IndexError> {
        let mut block = crate::store::block_of(self.root);
        let mut at = self.height;
        while at > level {
            let page = self.store.read(block)?;
            let view = IndexPage::new(&page)?;
            let (child, _) = view.route(key)?;
            block = crate::store::block_of(child);
            at -= 1;
        }
        Ok(block)
    }

    /// **枝/根页分裂**（与叶分裂同构：无链、无 ROWID 复合键）。
    fn split_branch(
        &mut self,
        block: u32,
        page: &mut bicdb_storage::page::Page,
        new_entry: &Entry,
    ) -> Result<(SplitKind, u32), IndexError> {
        let kind = {
            let view = IndexPage::new(page)?;
            let n = usize::from(view.entry_count());
            match if n > 1 {
                Some(view.entry(n - 1)?)
            } else {
                None
            } {
                Some(l) if cmp_entries(new_entry, &l) == Ordering::Greater => SplitKind::Append99,
                None => SplitKind::Append99,
                _ => SplitKind::Even,
            }
        };
        let mut items: Vec<Entry> = {
            let view = IndexPage::new(page)?;
            let n = usize::from(view.entry_count());
            let mut v = Vec::with_capacity(n);
            for i in 1..n {
                v.push(view.entry(i)?);
            }
            v
        };
        items.push(new_entry.clone());
        items.sort_by(cmp_entries);
        let old_high = IndexPage::new(page)?.high_key()?;
        let old_left_child = IndexPage::new(page)?.left_child();
        let floor = page.row_area_floor();
        let sep_key = match &items[items.len() - 1] {
            Entry::Branch { prefix, .. } => prefix.clone(),
            _ => return Err(IndexError::Malformed("枝页分裂条目非法")),
        };
        let high_len = Entry::HighKey { key: Some(sep_key) }.encode(false).len();
        let split = match kind {
            SplitKind::Append99 => {
                let last = items.len() - 1;
                adjust_split(&mut items, last, false, floor, high_len)
            }
            SplitKind::Even => {
                let split = (items.len() / 2).max(1).min(items.len() - 1);
                adjust_split(&mut items, split, false, floor, high_len)
            }
        };
        let (left_items, right_items) = items.split_at(split);
        // 新枝页：left_child = 右半第一条目对应的**左邻**——即右半第一条目的
        // 前一条目所指向的子页（B+Tree：n 条目 n+1 子页）。
        let right_first_index = left_items.len();
        let new_left_child = if right_first_index == 0 {
            old_left_child
        } else {
            match &items[right_first_index - 1] {
                Entry::Branch { child, .. } => *child,
                _ => return Err(IndexError::Malformed("枝页条目非法")),
            }
        };
        let new_block = self.store.allocate()?;
        let (ws, file_id) = {
            let h = page.header().expect("索引页");
            (h.workspace_ref, h.file_id)
        };
        let mut new_page =
            bicdb_storage::page::Page::new(PageType::IndexBranch, ws, file_id, new_block);
        {
            let mut m = IndexPageMut::new(&mut new_page)?;
            m.init(Some(new_left_child), old_high)?;
            for e in right_items {
                m.insert_entry(e)?;
            }
        }
        {
            let mut m = IndexPageMut::new(page)?;
            m.keep_high_key_only()?;
            let new_high = match right_items.first() {
                Some(Entry::Branch { prefix, .. }) => prefix.clone(),
                _ => return Err(IndexError::Malformed("分裂右半为空")),
            };
            m.set_high_key(Some(new_high))?;
            m.compact()?; // 同上：回收旧高键的死空间
            for e in left_items {
                m.insert_entry(e)?;
            }
            self.store.write(block, page)?;
        }
        self.store.write(new_block, &mut new_page)?;
        Ok((kind, new_block))
    }

    /// **删除**（§9.1.2：索引项随删除事务直接移除；空页留树中，不合并）。
    pub fn delete(&mut self, key: &[u8], rowid: RowId) -> Result<bool, IndexError> {
        let block = self.descend(key)?;
        let block = self.settle_leaf(block, key, Some(rowid))?;
        let mut page = self.store.read(block)?;
        let target = Entry::Leaf {
            key: key.to_vec(),
            rowid,
        };
        let at = {
            let view = IndexPage::new(&page)?;
            let (hit, at) = view.search(&target)?;
            if !hit {
                return Ok(false);
            }
            at
        };
        {
            let mut m = IndexPageMut::new(&mut page)?;
            m.remove_entry(at)?;
        }
        self.store.write(block, &mut page)?;
        Ok(true)
    }

    /// **结构自检**（检查器口径的最小子集，待冻结项 2d 清单⑥）：
    /// 叶链可达（从左最叶沿链走到底，且链的尽头才是 ∞ 页）、页内键序单调、
    /// 条目 < 高键、链的双向一致，以及与**根下行清点**的叶条目数一致
    /// （链断/叶子失联在这条上现形——实测叶链自指就是这样被抓住的）。
    pub fn validate(&mut self) -> Result<(), IndexError> {
        let mut block = self.leftmost_leaf()?;
        {
            // 最左叶必须真的没有前驱。
            let first = self.store.read(block)?;
            let fv = IndexPage::new(&first)?;
            if fv.links()?.0 != no_link() {
                return Err(IndexError::Malformed("最左叶的 link_prev 非空"));
            }
        }
        let mut prev_key: Option<(Vec<u8>, u64)> = None;
        let mut chained = 0usize;
        loop {
            let page = self.store.read(block)?;
            let view = IndexPage::new(&page)?;
            let high = view.high_key()?;
            let n = usize::from(view.entry_count());
            if n == 0 {
                return Err(IndexError::Malformed("目录至少含高键"));
            }
            for i in 1..n {
                let e = view.entry(i)?;
                let sk = e.sort_key();
                if let Some(p) = &prev_key {
                    if *p >= sk {
                        return Err(IndexError::Malformed("叶链键序非单调"));
                    }
                }
                // 高键不含 ROWID；重复 key 可以跨叶，等于高键合法。
                if let Some(hk) = &high {
                    if sk.0.as_slice() > hk.as_slice() {
                        return Err(IndexError::Malformed("条目大于高键"));
                    }
                }
                prev_key = Some(sk);
                chained += 1;
            }
            let (_, next) = view.links()?;
            if next == no_link() {
                // 链的尽头 = 最右页：高键必须无界（∞）。
                if high.is_some() {
                    return Err(IndexError::Malformed("叶链提前终止（尽头非 ∞ 页）"));
                }
                break;
            }
            let next_block = crate::store::block_of(next);
            let np = self.store.read(next_block)?;
            let nv = IndexPage::new(&np)?;
            let (nlp, _) = nv.links()?;
            if nlp != rid_of(self.file_id, block) {
                return Err(IndexError::Malformed("叶链 link_prev 不指向左邻"));
            }
            block = next_block;
        }
        // 根下行清点：每张叶恰被访问一次、叶条目总数与链走的一致。
        let via_tree = self.count_leaf_entries(crate::store::block_of(self.root), self.height)?;
        if via_tree != chained {
            return Err(IndexError::Malformed("链上的叶条目数与树内不符（叶失联）"));
        }
        Ok(())
    }

    /// 自 `block`（层级 `level`）下行清点叶条目总数（[`Self::validate`] 用）。
    fn count_leaf_entries(&mut self, block: u32, level: u32) -> Result<usize, IndexError> {
        let page = self.store.read(block)?;
        let view = IndexPage::new(&page)?;
        let n = usize::from(view.entry_count());
        if n == 0 {
            return Err(IndexError::Malformed("目录至少含高键"));
        }
        if level == 0 {
            if !view.is_leaf() {
                return Err(IndexError::Malformed("该高度上不是叶页"));
            }
            return Ok(n - 1);
        }
        if view.is_leaf() {
            return Err(IndexError::Malformed("叶页出现在非叶高度"));
        }
        let mut total =
            self.count_leaf_entries(crate::store::block_of(view.left_child()), level - 1)?;
        for i in 1..n {
            let child = match view.entry(i)? {
                Entry::Branch { child, .. } => child,
                _ => return Err(IndexError::Malformed("枝页条目非枝形态")),
            };
            total += self.count_leaf_entries(crate::store::block_of(child), level - 1)?;
        }
        Ok(total)
    }
}
