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

/// 主体标识（UUID，16 字节；`arch` §3：`user$` 以 UUID 为主键）。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct UserId([u8; 16]);

impl UserId {
    /// 由 16 字节原始值构造。
    #[must_use]
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// 原始字节。
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// 解析 UUID 文本：接受**连字符形态**（`8-4-4-4-12`）或 32 位纯十六进制；
    /// 十六进制接受大写与小写，其余字符一律拒绝。
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let mut out = [0u8; 16];
        let mut nibbles = 0usize;
        for c in s.chars() {
            if c == '-' {
                continue;
            }
            let v = c.to_digit(16)?;
            let idx = nibbles / 2;
            if idx >= 16 {
                return None;
            }
            if nibbles % 2 == 0 {
                out[idx] = (v as u8) << 4;
            } else {
                out[idx] |= v as u8;
            }
            nibbles += 1;
        }
        if nibbles == 32 {
            Some(Self(out))
        } else {
            None
        }
    }
}

impl fmt::Display for UserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = &self.0;
        write!(
            f,
            "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13],
            b[14], b[15]
        )
    }
}

impl fmt::Debug for UserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "UserId({self})")
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
    fn user_id_parse_and_display_roundtrip() {
        let s = "018f2a7c-3b4d-7e01-9a2b-c3d4e5f60718";
        let u = UserId::parse(s).expect("合法 UUID 应可解析");
        assert_eq!(u.to_string(), s);
        // 纯十六进制形态
        let u2 = UserId::parse("018f2a7c3b4d7e019a2bc3d4e5f60718").expect("纯 hex 应可解析");
        assert_eq!(u, u2);
        // 大写可解析、显示归一小写
        let u3 = UserId::parse("018F2A7C3B4D7E019A2BC3D4E5F60718").unwrap();
        assert_eq!(u3, u);
    }

    #[test]
    fn user_id_parse_rejects_malformed() {
        assert!(UserId::parse("").is_none());
        assert!(UserId::parse("018f2a7c").is_none(), "长度不足");
        assert!(
            UserId::parse("018f2a7c-3b4d-7e01-9a2b-c3d4e5f6071g").is_none(),
            "非法字符"
        );
        assert!(
            UserId::parse("018f2a7c3b4d7e019a2bc3d4e5f6071800").is_none(),
            "长度过长"
        );
    }
}
