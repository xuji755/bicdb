//! `UUID` 的物理编码：**16 字节定长、大端**（存储架构 §6.6）。
//!
//! 文本形态为规范的连字符小写 UUID（`8-4-4-4-12`）；解析接受
//! 连字符形态与 32 位纯十六进制（大小写皆可），显示归一小写。

use std::fmt;

/// `UUID` 编码长度。
pub const UUID_LEN: usize = 16;

/// `UUID` 文本解析错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UuidParseError;

impl fmt::Display for UuidParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("UUID 文本非法（需 8-4-4-4-12 或 32 位十六进制）")
    }
}

impl std::error::Error for UuidParseError {}

/// UUID（16 字节；大端存放 = 文本各字段的自然顺序）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Uuid([u8; UUID_LEN]);

impl Uuid {
    /// 由 16 字节构造。
    #[must_use]
    pub fn from_bytes(bytes: [u8; UUID_LEN]) -> Self {
        Self(bytes)
    }

    /// 原始字节。
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; UUID_LEN] {
        &self.0
    }

    /// 解析文本（接受连字符形态或 32 位纯十六进制）。
    pub fn parse(text: &str) -> Result<Self, UuidParseError> {
        let mut out = [0u8; UUID_LEN];
        let mut nibbles = 0usize;
        for c in text.chars() {
            if c == '-' {
                continue;
            }
            let v = c.to_digit(16).ok_or(UuidParseError)?;
            if nibbles >= 2 * UUID_LEN {
                return Err(UuidParseError);
            }
            if nibbles % 2 == 0 {
                out[nibbles / 2] = (v as u8) << 4;
            } else {
                out[nibbles / 2] |= v as u8;
            }
            nibbles += 1;
        }
        if nibbles == 2 * UUID_LEN {
            Ok(Self(out))
        } else {
            Err(UuidParseError)
        }
    }

    /// 规范文本（连字符小写）。
    #[must_use]
    pub fn to_text(&self) -> String {
        let b = &self.0;
        format!(
            "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13],
            b[14], b[15]
        )
    }
}

impl fmt::Display for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_text())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_display_roundtrip() {
        let s = "018f2a7c-3b4d-7e01-9a2b-c3d4e5f60718";
        let u = Uuid::parse(s).expect("合法 UUID");
        assert_eq!(u.to_text(), s);
        // 纯十六进制与大写输入归一到同一值。
        assert_eq!(Uuid::parse("018f2a7c3b4d7e019a2bc3d4e5f60718").unwrap(), u);
        assert_eq!(Uuid::parse("018F2A7C3B4D7E019A2BC3D4E5F60718").unwrap(), u);
        // 编码即 16 字节大端（文本字段顺序）。
        assert_eq!(u.as_bytes()[0], 0x01);
        assert_eq!(u.as_bytes()[15], 0x18);
    }

    #[test]
    fn parse_rejects_malformed() {
        assert!(Uuid::parse("").is_err());
        assert!(Uuid::parse("018f2a7c").is_err(), "长度不足");
        assert!(
            Uuid::parse("018f2a7c3b4d7e019a2bc3d4e5f6071g").is_err(),
            "非法字符"
        );
        assert!(
            Uuid::parse("018f2a7c3b4d7e019a2bc3d4e5f6071800").is_err(),
            "长度过长"
        );
    }

    #[test]
    fn byte_order_matches_text_order() {
        // 大端 ⇒ 字节字典序与规范化文本序一致（排查/范围扫描友好）。
        let a = Uuid::parse("00000000-0000-0000-0000-000000000001").unwrap();
        let b = Uuid::parse("00000000-0000-0000-0000-000000000002").unwrap();
        let c = Uuid::parse("10000000-0000-0000-0000-000000000000").unwrap();
        assert!(a.as_bytes() < b.as_bytes());
        assert!(b.as_bytes() < c.as_bytes());
        assert!(a.to_text() < b.to_text());
    }
}
