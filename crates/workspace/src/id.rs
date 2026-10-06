//! 标识类型：`UserId`（主体，UUID 形态）与 `WorkspaceId`（工作区，48 位序列）。
//!
//! 宽度与序列的约定见 `CONV` §1.1 / §1.2：**48 位、从 1 起、0 保留为"无"、不回绕**；
//! 对外文本表示按**十进制**。

use std::fmt;

/// 48 位上限（`CONV` §1.2 的宽度约定：与提交序号、LSN、ROWID 同为 48 位）。
pub const WORKSPACE_ID_MAX: u64 = (1 << 48) - 1;

/// 工作区标识（48 位；由 `public` 的 `seq$` 分配——实例级序列）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkspaceId(u64);

impl WorkspaceId {
    /// 从原始数值构造。`0`（保留为"无"）或越过 48 位上限时返回 `None`。
    #[must_use]
    pub fn from_raw(raw: u64) -> Option<Self> {
        if raw == 0 || raw > WORKSPACE_ID_MAX {
            None
        } else {
            Some(Self(raw))
        }
    }

    /// 原始数值。
    #[must_use]
    pub fn as_raw(self) -> u64 {
        self.0
    }
}

impl fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// 主体标识（48 位实例级序列值；`CONV` §1.1/§1.2——由 `public` 的 `seq$` 分配，
/// 与 `workspace_id` 同规：**从 1 起、0 保留为"无"、不回绕**；文本按十进制）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UserId(u64);

impl UserId {
    /// 从原始数值构造。`0`（保留为"无"）或越过 48 位上限时返回 `None`。
    #[must_use]
    pub fn from_raw(raw: u64) -> Option<Self> {
        if raw == 0 || raw > WORKSPACE_ID_MAX {
            None
        } else {
            Some(Self(raw))
        }
    }

    /// 原始数值。
    #[must_use]
    pub fn as_raw(self) -> u64 {
        self.0
    }
}

impl fmt::Display for UserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_id_bounds() {
        assert!(WorkspaceId::from_raw(0).is_none(), "0 保留为『无』");
        assert!(WorkspaceId::from_raw(1).is_some());
        assert!(WorkspaceId::from_raw(WORKSPACE_ID_MAX).is_some());
        assert!(WorkspaceId::from_raw(WORKSPACE_ID_MAX + 1).is_none());
    }

    #[test]
    fn workspace_id_text_is_decimal() {
        assert_eq!(WorkspaceId::from_raw(12345).unwrap().to_string(), "12345");
    }

    #[test]
    fn user_id_has_the_same_shape_as_workspace_id() {
        assert!(UserId::from_raw(0).is_none(), "0 保留为『无』");
        assert!(UserId::from_raw(1).is_some());
        assert!(UserId::from_raw(WORKSPACE_ID_MAX).is_some());
        assert!(UserId::from_raw(WORKSPACE_ID_MAX + 1).is_none());
        assert_eq!(
            UserId::from_raw(42).unwrap().to_string(),
            "42",
            "文本十进制"
        );
    }
}
