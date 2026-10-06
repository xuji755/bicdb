//! 两个**不得混用**的单调量：`Lsn`（日志序号）与 `CommitSeq`（提交序号）。
//!
//! `CONV` §3（REQ-TXN-004）原文：
//!
//! | 名称 | 语义 | 用途 | 性质 |
//! | --- | --- | --- | --- |
//! | **LSN** | 日志中的**字节位置** | WAL 记录定位、恢复起点、页上 `page_lsn` | 物理量 |
//! | **提交序号** | **逻辑提交点编号** | 快照构造、可见性判定 | 逻辑量 |
//!
//! 三条硬规则（本模块用**类型**落实）：
//!
//! 1. 快照只由提交序号构造；LSN 不得出现在可见性判定中；
//! 2. `page_lsn` 只用于恢复期判断"该页是否已包含某日志的效果"；
//! 3. **两者不得互相赋值、比较或换算**——"实现上建议使用**不同类型**
//!    （newtype）使混用无法通过编译"（`CONV` §3 原文）。
//!
//! 宽度照 `CONV` §1.2：**48 位无符号（6 字节）**，与 ROWID、ID 序列同惯例。

use std::fmt;

/// 48 位上限（`CONV` §1.2 的宽度惯例）。
pub const SEQ_MAX: u64 = (1 << 48) - 1;

/// 校验 48 位域。
const fn in_domain(raw: u64) -> bool {
    raw <= SEQ_MAX
}

/// 日志序号（LSN）——**物理量**：日志流中的字节位置。
///
/// 只用于 WAL 记录定位、恢复起点与页上的 `page_lsn`；
/// **不得**进入可见性判定、**不得**与 [`CommitSeq`] 互相赋值/比较/换算。
///
/// ```compile_fail
/// // 混用必须无法通过编译（CONV §3 规则 3）
/// use bicdb_common::seq::{CommitSeq, Lsn};
/// let lsn: Lsn = CommitSeq::from_raw(1).unwrap();
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Lsn(u64);

impl Lsn {
    /// 由原始值构造；越过 48 位域返回 `None`。
    #[must_use]
    pub const fn from_raw(raw: u64) -> Option<Self> {
        if in_domain(raw) {
            Some(Self(raw))
        } else {
            None
        }
    }

    /// 原始值。
    #[must_use]
    pub const fn as_raw(self) -> u64 {
        self.0
    }
}

impl fmt::Display for Lsn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// 提交序号——**逻辑量**：逻辑提交点的编号；快照与可见性判定的唯一依据。
///
/// **不得**与 [`Lsn`] 互相赋值/比较/换算（`CONV` §3 规则 3）；
/// 墙钟不得用于判定先后——逻辑先后一律用它（`CONV` §2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CommitSeq(u64);

impl CommitSeq {
    /// 由原始值构造；越过 48 位域返回 `None`。
    #[must_use]
    pub const fn from_raw(raw: u64) -> Option<Self> {
        if in_domain(raw) {
            Some(Self(raw))
        } else {
            None
        }
    }

    /// 原始值。
    #[must_use]
    pub const fn as_raw(self) -> u64 {
        self.0
    }
}

impl fmt::Display for CommitSeq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_is_48_bits() {
        assert!(Lsn::from_raw(SEQ_MAX).is_some());
        assert!(Lsn::from_raw(SEQ_MAX + 1).is_none());
        assert!(CommitSeq::from_raw(SEQ_MAX).is_some());
        assert!(CommitSeq::from_raw(SEQ_MAX + 1).is_none());
    }

    #[test]
    fn ordering_is_numeric() {
        let a = CommitSeq::from_raw(2).unwrap();
        let b = CommitSeq::from_raw(10).unwrap();
        assert!(a < b, "按数值比较；文本序无关");
        let l1 = Lsn::from_raw(2).unwrap();
        let l2 = Lsn::from_raw(10).unwrap();
        assert!(l1 < l2);
    }

    #[test]
    fn text_is_decimal() {
        assert_eq!(CommitSeq::from_raw(123).unwrap().to_string(), "123");
        assert_eq!(Lsn::from_raw(4096).unwrap().to_string(), "4096");
    }

    #[test]
    fn two_monotonic_values_never_compare_each_other() {
        // 类型不同 ⇒ 相等比较都无法通过编译；这里以"各自独立"做语义检查：
        // 同一数值在两个类型里互不相干（不是同一个量）。
        let lsn = Lsn::from_raw(42).unwrap();
        let seq = CommitSeq::from_raw(42).unwrap();
        assert_eq!(
            lsn.as_raw(),
            seq.as_raw(),
            "数值可相同，语义无关——由类型区分"
        );
    }
}
