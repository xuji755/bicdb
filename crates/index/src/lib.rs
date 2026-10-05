//! # bicdb-index — B+Tree（通用索引）
//!
//! **B-link 形态**（§9.1：页体 §5.11 类型 2/3/4；分裂协议 §9.1.5）：
//!
//! - **页体**：页体头（`entry_count 2B │ left_child 6B`）+ **有序条目目录**
//!   （项 1 恒为**高键**——上界/锚/右移判据三合一）+ 条目区（自页尾向上）；
//!   叶页页尾 12B 双链（`link_prev`/`link_next`）；枝/根页无链。
//! - **键即字节序**（§6.0 的编码原则在此的回报）：叶页按**复合键
//!   `(key, ROWID)`**、枝页按**区分前缀**比较——不解码、无类型比较器。
//! - **高键**：页内一切条目**严格小于**它；`key_len = 0xFFFF` = **∞**（最右页，
//!   最右身份跟着 ∞ 走）；**只在建页与分裂时写**，删除/插入都不动它。
//! - **分裂**（单阶段）：99/1（新条目最大——单调追加是常态路径）或 50/50；
//!   三页四步（P′ 复制原高键、P 改存新上界、双链急切更新）；父层滞后是
//!   **容忍态**（高键 + 右链兜底），根分裂单条原子形态。
//! - **删除**：索引项直接移除；**空页留树中**（高键保证可定位，§9.1.2），
//!   页回收 V1.0 不做。
//! - **树头**（根页 ROWID + 高度）落在段头页的 B+Tree 扩展区（§9.1.5 第 8 步）；
//!   本 crate 里由 [`Tree`] 显式携带，执行器接入时读写段头。
//!
//! **页存取**经 [`store::PageStore`] 抽象（缓冲池/段由执行器接入；
//! 测试用 [`store::MemStore`]）——树算法因此可以脱离 I/O 单测。
//! **并发**：latch 由调用方提供；结构性规则已就位（见 [`tree`] 模块文档）。

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod page;
pub mod store;
pub mod tree;

pub use page::{Entry, IndexPage, IndexPageMut, KEY_LEN_INFINITY, MAX_ENTRY_LEN, MAX_KEY_LEN};
pub use store::{MemStore, PageStore};
pub use tree::{InsertOutcome, SplitKind, Tree};

/// 索引层错误（**明确判定**，不静默）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexError {
    /// 页类型不是索引页。
    NotIndexPage,
    /// 该操作要求叶页。
    NotALeaf,
    /// 页/条目/目录的结构损坏。
    Malformed(&'static str),
    /// 单条目超过上限（§5.11：一页至少放两个）。
    EntryTooLong {
        /// 实际长度。
        len: usize,
        /// 上限。
        limit: usize,
    },
    /// 页内空间不足（调用方应先 compact 再分裂）。
    NoSpace,
    /// 页不在仓里。
    BlockNotFound {
        /// 块号。
        block: u32,
    },
    /// 页仓已满（测试仓的固定容量）。
    StoreFull,
}

impl std::fmt::Display for IndexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IndexError::NotIndexPage => f.write_str("不是索引页"),
            IndexError::NotALeaf => f.write_str("该操作要求索引叶页"),
            IndexError::Malformed(why) => write!(f, "索引结构损坏：{why}"),
            IndexError::EntryTooLong { len, limit } => {
                write!(f, "索引条目 {len} 字节超过上限 {limit}")
            }
            IndexError::NoSpace => f.write_str("索引页空间不足"),
            IndexError::BlockNotFound { block } => write!(f, "页仓中没有块 {block}"),
            IndexError::StoreFull => f.write_str("页仓已满"),
        }
    }
}

impl std::error::Error for IndexError {}
