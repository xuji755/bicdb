//! **索引页的页体**（§5.11 类型 2/3/4、§9.1.3/§9.1.4）。
//!
//! ```text
//! 偏移 68   页体头（8B）：entry_count 2B │ left_child 6B
//! 偏移 76   条目目录（向高地址增长）：每项 2B（条目起始偏移）；
//!           **第 1 项恒指高键条目**（上界/锚/右移判据三合一，§9.1.4），
//!           第 2..n 项按**复合键升序**（二分只在 2..n 上进行）
//!               ───── 空闲区 ─────
//!           条目区（自页尾向上增长，下界 = 16384 − 页尾区）
//! ```
//!
//! | 页类型 | 页尾区 | 条目编码 |
//! | --- | --- | --- |
//! | 2 叶页 | 16B（`link_prev` 6B │ `link_next` 6B │ 页尾副本 4B） | `key_len 2B │ key │ ROWID 6B` |
//! | 3 枝页 / 4 根页 | 4B | `key_len 2B │ 区分前缀 │ 子页 ROWID 6B` |
//!
//! **排序键**：叶页 = **复合键 `(key, ROWID)`**（ROWID 恒唯一，重复键无歧义）；
//! 枝/根页 = **键字节**（条目尾的 ROWID 是子页指针，不参与比较）。
//! **比较就是字节比较**（§6.0 的编码原则在此的回报）——不解码、不写类型比较器。
//!
//! **高键**：页内一切有效条目**严格小于**它；`key_len = 0xFFFF` 表示
//! **无上界（∞）**——**最右身份跟着 ∞ 走**（分裂时随原页高键一起移转）。
//! 目录项 1 **永不回收**（空页也可定位，§9.1.2）。

use bicdb_storage::page::{Page, PageType, SLOT_ENTRY_LEN};
use bicdb_storage::rowid::{RowId, ROWID_LEN};

use crate::IndexError;

/// 页体头（`entry_count 2B │ left_child 6B`）的起点（紧随 68B 页头）。
pub const BODY_HEAD_OFFSET: usize = 68;

/// 页体头长度。
pub const BODY_HEAD_LEN: usize = 8;

/// 条目目录起点。
pub const DIRECTORY_OFFSET: usize = BODY_HEAD_OFFSET + BODY_HEAD_LEN;

/// 高键"无上界（∞）"的 `key_len` 保留值（合法键长 ≤ 4096）。
pub const KEY_LEN_INFINITY: u16 = 0xFFFF;

/// **单条目的长度上限**（§5.11：一页至少要能装下两个条目 ⇒ 页体 1/4）。
pub const MAX_ENTRY_LEN: usize = 4096;

/// 单条键的字节长度上限（条目 = 2B 长 + 键 + 6B ROWID ⇒ 4096 − 8）。
pub const MAX_KEY_LEN: usize = MAX_ENTRY_LEN - 2 - ROWID_LEN;

/// **一个条目**（叶页与枝页共用外壳；语义字段由上下文定）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// 叶条目：键 + 数据行 ROWID。
    Leaf {
        /// 键字节（字节序即序）。
        key: Vec<u8>,
        /// 被索引行的 ROWID。
        rowid: RowId,
    },
    /// 枝/根条目：**区分前缀** + 子页 ROWID。
    Branch {
        /// 区分前缀（§9.1.3：分裂时取两侧最长公共前缀 + 1 字节；比较按短字节串）。
        prefix: Vec<u8>,
        /// 子页 ROWID（`row_id` 恒 0，宽度统一）。
        child: RowId,
    },
    /// 高键条目（目录项 1）：`None` = 无上界（∞）。
    HighKey {
        /// 上界键；`None` = ∞（最右页）。
        key: Option<Vec<u8>>,
    },
}

impl Entry {
    /// 编码（叶/枝 24+B；高键同构，只是 `key_len` 可为 `0xFFFF`）。
    #[must_use]
    pub fn encode(&self, leaf: bool) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Entry::Leaf { key, rowid } => {
                out.extend_from_slice(&(key.len() as u16).to_le_bytes());
                out.extend_from_slice(key);
                out.extend_from_slice(&rowid.to_bytes());
            }
            Entry::Branch { prefix, child } => {
                out.extend_from_slice(&(prefix.len() as u16).to_le_bytes());
                out.extend_from_slice(prefix);
                out.extend_from_slice(&child.to_bytes());
            }
            Entry::HighKey { key } => {
                match key {
                    Some(k) => {
                        out.extend_from_slice(&(k.len() as u16).to_le_bytes());
                        out.extend_from_slice(k);
                    }
                    None => out.extend_from_slice(&KEY_LEN_INFINITY.to_le_bytes()),
                }
                // 高键条目与同页有效条目**同构**：叶页补 ROWID（全 0），枝页补子页（全 0）。
                out.extend_from_slice(&[0u8; ROWID_LEN]);
            }
        }
        let _ = leaf;
        out
    }

    /// 解码一条（`key_len` 解释 + 尾部 ROWID）。
    pub fn decode(bytes: &[u8], leaf: bool) -> Result<Self, IndexError> {
        if bytes.len() < 2 + ROWID_LEN {
            return Err(IndexError::Malformed("条目长度不足"));
        }
        let key_len = u16::from_le_bytes([bytes[0], bytes[1]]);
        if key_len == KEY_LEN_INFINITY {
            return Ok(Entry::HighKey { key: None });
        }
        let key_len = usize::from(key_len);
        if bytes.len() != 2 + key_len + ROWID_LEN {
            return Err(IndexError::Malformed("条目长度与 key_len 不符"));
        }
        let key = &bytes[2..2 + key_len];
        let rid: [u8; ROWID_LEN] = bytes[2 + key_len..2 + key_len + ROWID_LEN]
            .try_into()
            .expect("6 字节");
        let rowid = RowId::from_bytes(&rid);
        Ok(if leaf {
            Entry::Leaf {
                key: key.to_vec(),
                rowid,
            }
        } else {
            Entry::Branch {
                prefix: key.to_vec(),
                child: rowid,
            }
        })
    }

    /// 条目的**排序键**：叶 = 复合键 `(key, ROWID)`；枝 = 前缀字节（比较时把
    /// 前缀当"短字节串"，与真实键的前缀比较等价——§9.1.3 的不变量）。
    #[must_use]
    pub fn sort_key(&self) -> (Vec<u8>, u64) {
        match self {
            Entry::Leaf { key, rowid } => (key.clone(), rowid.as_raw()),
            Entry::Branch { prefix, .. } => (prefix.clone(), 0),
            Entry::HighKey { key } => (key.clone().unwrap_or_default(), 0),
        }
    }
}

/// 复合键比较（字节序 + ROWID 数值序）。
#[must_use]
pub fn cmp_entries(a: &Entry, b: &Entry) -> std::cmp::Ordering {
    let (ka, ra) = a.sort_key();
    let (kb, rb) = b.sort_key();
    ka.cmp(&kb).then(ra.cmp(&rb))
}

/// 键比较（前缀按短字节串比较——字节序即序）。
#[must_use]
pub fn cmp_key_prefix(key: &[u8], prefix: &[u8]) -> std::cmp::Ordering {
    key.cmp(prefix)
}

/// 索引页视图（只读）。
pub struct IndexPage<'a> {
    page: &'a Page,
}

impl<'a> IndexPage<'a> {
    /// 解析（要求页类型是索引叶/枝/根）。
    pub fn new(page: &'a Page) -> Result<Self, IndexError> {
        match page.header() {
            Some(h)
                if matches!(
                    h.page_type,
                    PageType::IndexLeaf | PageType::IndexBranch | PageType::IndexRoot
                ) =>
            {
                Ok(Self { page })
            }
            _ => Err(IndexError::NotIndexPage),
        }
    }

    /// 是否叶页。
    #[must_use]
    pub fn is_leaf(&self) -> bool {
        self.page.header().expect("已解析").page_type == PageType::IndexLeaf
    }

    /// 条目数（含高键，恒 ≥ 1）。
    #[must_use]
    pub fn entry_count(&self) -> u16 {
        let b = self.page.as_bytes();
        u16::from_le_bytes([b[BODY_HEAD_OFFSET], b[BODY_HEAD_OFFSET + 1]])
    }

    /// 最左子指针（叶页为全 0）。
    #[must_use]
    pub fn left_child(&self) -> RowId {
        let b = self.page.as_bytes();
        let raw: [u8; ROWID_LEN] = b[BODY_HEAD_OFFSET + 2..BODY_HEAD_OFFSET + 2 + ROWID_LEN]
            .try_into()
            .expect("6 字节");
        RowId::from_bytes(&raw)
    }

    /// 目录项 `i`（0 起；**项 0 = 高键**）。
    fn dir_item(&self, i: usize) -> Result<u16, IndexError> {
        if i >= usize::from(self.entry_count()) {
            return Err(IndexError::Malformed("目录项越界"));
        }
        let at = DIRECTORY_OFFSET + i * SLOT_ENTRY_LEN;
        let b = self.page.as_bytes();
        if at + 2 > b.len() {
            return Err(IndexError::Malformed("目录项越界"));
        }
        Ok(u16::from_le_bytes([b[at], b[at + 1]]))
    }

    /// 读条目 `i`。
    pub fn entry(&self, i: usize) -> Result<Entry, IndexError> {
        let off = usize::from(self.dir_item(i)?);
        let floor = self.page.row_area_floor();
        if off < DIRECTORY_OFFSET || off + 2 + ROWID_LEN > floor {
            return Err(IndexError::Malformed("条目偏移越界"));
        }
        let b = self.page.as_bytes();
        let key_len = u16::from_le_bytes([b[off], b[off + 1]]);
        let end = if key_len == KEY_LEN_INFINITY {
            off + 2 + ROWID_LEN
        } else {
            off + 2 + usize::from(key_len) + ROWID_LEN
        };
        if end > floor {
            return Err(IndexError::Malformed("条目越过行区下界"));
        }
        Entry::decode(&b[off..end], self.is_leaf())
    }

    /// 高键（`None` = ∞）。**有界高键与有效条目同构编码**（§9.1.4"与同页有效
    /// 条目同构"）——只有位置（目录项 1）标记它的身份，所以这里按"取它的键"
    /// 解释，而不是要求它解码成某个专用变体。
    pub fn high_key(&self) -> Result<Option<Vec<u8>>, IndexError> {
        match self.entry(0)? {
            Entry::HighKey { key } => Ok(key),
            Entry::Leaf { key, .. } => Ok(Some(key)),
            Entry::Branch { prefix, .. } => Ok(Some(prefix)),
        }
    }

    /// 叶页的前后链。
    pub fn links(&self) -> Result<(RowId, RowId), IndexError> {
        if !self.is_leaf() {
            return Err(IndexError::NotALeaf);
        }
        let b = self.page.as_bytes();
        let at = crate::page::PAGE_SIZE_FOR_TAIL - INDEX_LEAF_TAIL;
        let prev: [u8; ROWID_LEN] = b[at..at + ROWID_LEN].try_into().expect("6 字节");
        let next: [u8; ROWID_LEN] = b[at + ROWID_LEN..at + 2 * ROWID_LEN]
            .try_into()
            .expect("6 字节");
        Ok((RowId::from_bytes(&prev), RowId::from_bytes(&next)))
    }

    /// **二分查找**（只在目录 2..n 上进行；返回（是否命中, 插入位——目录下标））。
    ///
    /// 用于叶页的点查/插入定位与枝页的路由；比较是 [`cmp_entries`]。
    pub fn search(&self, target: &Entry) -> Result<(bool, usize), IndexError> {
        let n = usize::from(self.entry_count());
        let (mut lo, mut hi) = (1usize, n); // 目录下标（0 = 高键不参与）
        while lo < hi {
            let mid = (lo + hi) / 2;
            let e = self.entry(mid)?;
            match cmp_entries(&e, target) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Equal => return Ok((true, mid)),
                std::cmp::Ordering::Greater => hi = mid,
            }
        }
        Ok((false, lo))
    }

    /// 枝/根页**路由**：取"最后一个 ≤ 目标键"的条目（§9.1.4：父条目可陈旧，
    /// 陈旧但合法——高键负责兜底右移）。返回（子页, 是否用了最左子指针）。
    pub fn route(&self, key: &[u8]) -> Result<(RowId, bool), IndexError> {
        let target = Entry::Leaf {
            key: key.to_vec(),
            rowid: RowId::from_bytes(&[0u8; ROWID_LEN]),
        };
        let (hit, at) = self.search(&target)?;
        // "最后一个 ≤ 目标键"：命中分隔键时**取它自己**（分隔键是右子树的最小键，
        // 相等的键属于右子树）；未命中取左邻；都没有 ⇒ 最左子指针。
        let idx = if hit { at } else { at.saturating_sub(1) };
        if idx == 0 {
            return Ok((self.left_child(), true));
        }
        match self.entry(idx)? {
            Entry::Branch { child, .. } => Ok((child, false)),
            _ => Err(IndexError::Malformed("枝页条目不是分支条目")),
        }
    }

    /// **需要右移吗**（§9.1.4）：`目标键 ≥ 本页高键`（∞ 高键永不右移）。
    pub fn needs_right_move(&self, key: &[u8], rowid: Option<RowId>) -> Result<bool, IndexError> {
        let Some(hk) = self.high_key()? else {
            return Ok(false); // ∞：最右页
        };
        let target = match rowid {
            Some(r) => Entry::Leaf {
                key: key.to_vec(),
                rowid: r,
            },
            None => Entry::Leaf {
                key: key.to_vec(),
                rowid: RowId::from_bytes(&[0u8; ROWID_LEN]),
            },
        };
        let hk_entry = Entry::Leaf {
            key: hk,
            rowid: RowId::from_bytes(&[0u8; ROWID_LEN]),
        };
        Ok(cmp_entries(&target, &hk_entry) != std::cmp::Ordering::Less)
    }
}

/// 页尾区长度（叶 16B，枝/根 4B）——由 `PageType::tail_len` 给出，这里取叶页。
pub const PAGE_SIZE_FOR_TAIL: usize = bicdb_storage::page::PAGE_SIZE;

/// 叶页页尾里链的起点（页尾区开头）。
const INDEX_LEAF_TAIL: usize = bicdb_storage::page::INDEX_LEAF_TAIL_LEN;

/// 索引页的可变视图（写侧）。
pub struct IndexPageMut<'a> {
    page: &'a mut Page,
}

impl<'a> IndexPageMut<'a> {
    /// 解析。
    pub fn new(page: &'a mut Page) -> Result<Self, IndexError> {
        match page.header() {
            Some(h)
                if matches!(
                    h.page_type,
                    PageType::IndexLeaf | PageType::IndexBranch | PageType::IndexRoot
                ) =>
            {
                Ok(Self { page })
            }
            _ => Err(IndexError::NotIndexPage),
        }
    }

    fn is_leaf(&self) -> bool {
        self.page.header().expect("已解析").page_type == PageType::IndexLeaf
    }

    /// 条目数（含高键）。
    #[must_use]
    pub fn entry_count(&self) -> u16 {
        IndexPage::new(self.page).expect("已解析").entry_count()
    }

    /// 写**页体头**的 `entry_count`（注意不是页头的 `slot_count`——索引页不用
    /// 槽位目录，两个字段在物理上不同位置）。
    fn set_entry_count(&mut self, n: u16) {
        let b = self.page.as_bytes_mut();
        b[BODY_HEAD_OFFSET..BODY_HEAD_OFFSET + 2].copy_from_slice(&n.to_le_bytes());
    }

    /// 高键（`None` = ∞）。
    pub fn high_key(&self) -> Result<Option<Vec<u8>>, IndexError> {
        IndexPage::new(self.page)?.high_key()
    }

    /// 叶页前后链。
    pub fn links(&self) -> Result<(RowId, RowId), IndexError> {
        IndexPage::new(self.page)?.links()
    }

    /// **格式化一张空索引页**：高键 = ∞（单页即最右）、链自指（叶页：
    /// `prev = next = 自身`?  —— 采用 `0` 表示"无"：初始叶页 `prev = next = 0`）。
    pub fn init(
        &mut self,
        left_child: Option<RowId>,
        high_key: Option<Vec<u8>>,
    ) -> Result<(), IndexError> {
        let b = self.page.as_bytes_mut();
        // 页体头。
        b[BODY_HEAD_OFFSET..BODY_HEAD_OFFSET + 2].copy_from_slice(&1u16.to_le_bytes()); // 仅高键
        let lc = left_child.map_or([0u8; ROWID_LEN], |r| r.to_bytes());
        b[BODY_HEAD_OFFSET + 2..BODY_HEAD_OFFSET + 2 + ROWID_LEN].copy_from_slice(&lc);
        // 条目区：把高键条目放在页尾之下。
        let entry = Entry::HighKey { key: high_key }.encode(self.is_leaf());
        let floor = self.page.row_area_floor();
        let at = floor - entry.len();
        let b = self.page.as_bytes_mut();
        b[at..at + entry.len()].copy_from_slice(&entry);
        self.page.set_free_end(at);
        // 目录项 0。
        let b = self.page.as_bytes_mut();
        b[DIRECTORY_OFFSET..DIRECTORY_OFFSET + 2].copy_from_slice(&(at as u16).to_le_bytes());
        // 叶页：链置 0（无前驱/后继）。
        if self.is_leaf() {
            let at = PAGE_SIZE_FOR_TAIL - INDEX_LEAF_TAIL;
            let b = self.page.as_bytes_mut();
            b[at..at + 2 * ROWID_LEN].fill(0);
        }
        Ok(())
    }

    /// 剩余可用空间（空闲区 − 目录项开销；一次插入要一条目录项）。
    #[must_use]
    pub fn free_space(&self) -> usize {
        let dir_end = DIRECTORY_OFFSET + usize::from(self.entry_count()) * SLOT_ENTRY_LEN;
        self.page
            .free_end()
            .saturating_sub(dir_end + SLOT_ENTRY_LEN)
    }

    /// 追加/插入一个**有效条目**（保持目录按键升序；条目字节放条目区顶部）。
    ///
    /// 返回它的目录下标。
    pub fn insert_entry(&mut self, entry: &Entry) -> Result<usize, IndexError> {
        let encoded = entry.encode(self.is_leaf());
        if encoded.len() > MAX_ENTRY_LEN {
            return Err(IndexError::EntryTooLong {
                len: encoded.len(),
                limit: MAX_ENTRY_LEN,
            });
        }
        if encoded.len() > self.free_space() {
            return Err(IndexError::NoSpace);
        }
        // 定位（二分，目录 2..n）。
        let ip = IndexPage::new(self.page)?;
        let (_, at) = ip.search(entry)?;
        let n = usize::from(ip.entry_count());
        // 条目字节：放在当前 free_end 之下。
        let floor = self.page.row_area_floor();
        let off = self.page.free_end() - encoded.len();
        if off < DIRECTORY_OFFSET + (n + 1) * SLOT_ENTRY_LEN {
            return Err(IndexError::NoSpace);
        }
        let _ = floor;
        {
            let b = self.page.as_bytes_mut();
            b[off..off + encoded.len()].copy_from_slice(&encoded);
        }
        self.page.set_free_end(off);
        // 目录：a..n 右移一项，插入 at。
        {
            let b = self.page.as_bytes_mut();
            let dir = DIRECTORY_OFFSET;
            b.copy_within(dir + at * 2..dir + n * 2, dir + (at + 1) * 2);
            b[dir + at * 2..dir + at * 2 + 2].copy_from_slice(&(off as u16).to_le_bytes());
        }
        self.set_entry_count((n + 1) as u16);
        Ok(at)
    }

    /// 删除目录项 `i` 指向的条目（**不动高键**；字节留在原处，由 `compact` 回收）。
    pub fn remove_entry(&mut self, i: usize) -> Result<Entry, IndexError> {
        if i == 0 {
            return Err(IndexError::Malformed("高键条目不可删除"));
        }
        let ip = IndexPage::new(self.page)?;
        let removed = ip.entry(i)?;
        let n = usize::from(ip.entry_count());
        {
            let b = self.page.as_bytes_mut();
            let dir = DIRECTORY_OFFSET;
            b.copy_within(dir + (i + 1) * 2..dir + n * 2, dir + i * 2);
        }
        self.set_entry_count((n - 1) as u16);
        Ok(removed)
    }

    /// 重写高键（分裂时：原页改存新上界）。
    pub fn set_high_key(&mut self, key: Option<Vec<u8>>) -> Result<(), IndexError> {
        let entry = Entry::HighKey { key }.encode(self.is_leaf());
        if entry.len() > MAX_ENTRY_LEN {
            return Err(IndexError::EntryTooLong {
                len: entry.len(),
                limit: MAX_ENTRY_LEN,
            });
        }
        let at = self
            .page
            .free_end()
            .checked_sub(entry.len())
            .ok_or(IndexError::NoSpace)?;
        if at < DIRECTORY_OFFSET + usize::from(self.entry_count()) * SLOT_ENTRY_LEN {
            return Err(IndexError::NoSpace);
        }
        {
            let b = self.page.as_bytes_mut();
            b[at..at + entry.len()].copy_from_slice(&entry);
        }
        self.page.set_free_end(at);
        let b = self.page.as_bytes_mut();
        b[DIRECTORY_OFFSET..DIRECTORY_OFFSET + 2].copy_from_slice(&(at as u16).to_le_bytes());
        Ok(())
    }

    /// 写叶页前后链。
    pub fn set_links(&mut self, prev: RowId, next: RowId) -> Result<(), IndexError> {
        if !self.is_leaf() {
            return Err(IndexError::NotALeaf);
        }
        let at = PAGE_SIZE_FOR_TAIL - INDEX_LEAF_TAIL;
        let b = self.page.as_bytes_mut();
        b[at..at + ROWID_LEN].copy_from_slice(&prev.to_bytes());
        b[at + ROWID_LEN..at + 2 * ROWID_LEN].copy_from_slice(&next.to_bytes());
        Ok(())
    }

    /// **只留高键**（分裂/重建用）：目录回落为 1 项、条目区只保留高键、
    /// `left_child` 与叶链不变——随后可把条目**重新插入**（不重复占字节）。
    pub fn keep_high_key_only(&mut self) -> Result<(), IndexError> {
        let (lc, links) = {
            let v = IndexPage::new(self.page)?;
            let links = if v.is_leaf() { Some(v.links()?) } else { None };
            (v.left_child(), links)
        };
        let high = self.high_key()?;
        self.init(Some(lc), high)?;
        if let Some((p, n)) = links {
            self.set_links(p, n)?;
        }
        Ok(())
    }

    /// **压紧**（回收删除与 `set_high_key` 留下的空洞）：条目按目录序重写到
    /// 条目区顶部，高键放最下。
    ///
    /// **装不下即 `NoSpace` 且不动页**——先算总量再写：让失败保持"页原样"，
    /// 调用方（分裂路径）可以安全地把 `NoSpace` 继续上报。
    pub fn compact(&mut self) -> Result<(), IndexError> {
        let n = usize::from(IndexPage::new(self.page)?.entry_count());
        let mut entries = Vec::with_capacity(n);
        {
            let ip = IndexPage::new(self.page)?;
            for i in 0..n {
                entries.push(ip.entry(i)?);
            }
        }
        // 重排：高键先放，其余按目录序（目录序已是升序）——统一把"高键"放最后
        // （条目区最深处）。编码次序与写入次序一致（i = n−1, …, 0）。
        let mut encoded: Vec<Vec<u8>> = Vec::with_capacity(n);
        let mut total = 0usize;
        for i in (0..n).rev() {
            let e = if i == 0 {
                // 高键与有效条目**同构编码**：按"取它的键"归一化（∞ 保持 ∞）。
                let key = match &entries[0] {
                    Entry::HighKey { key } => key.clone(),
                    Entry::Leaf { key, .. } => Some(key.clone()),
                    Entry::Branch { prefix, .. } => Some(prefix.clone()),
                };
                Entry::HighKey { key }
            } else {
                entries[i].clone()
            };
            let enc = e.encode(self.is_leaf());
            total += enc.len();
            encoded.push(enc);
        }
        let floor = self.page.row_area_floor();
        if floor < DIRECTORY_OFFSET + n * SLOT_ENTRY_LEN + total {
            return Err(IndexError::NoSpace);
        }
        let mut at = floor;
        let mut offsets = vec![0u16; n];
        for (k, enc) in encoded.iter().enumerate() {
            let i = n - 1 - k;
            at -= enc.len();
            let b = self.page.as_bytes_mut();
            b[at..at + enc.len()].copy_from_slice(enc);
            offsets[i] = at as u16;
        }
        self.page.set_free_end(at);
        for (i, off) in offsets.iter().enumerate() {
            let b = self.page.as_bytes_mut();
            let dir = DIRECTORY_OFFSET + i * SLOT_ENTRY_LEN;
            b[dir..dir + 2].copy_from_slice(&off.to_le_bytes());
        }
        Ok(())
    }
}
