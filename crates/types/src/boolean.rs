//! `BOOLEAN` 的物理编码：**1 字节（0/1）**（存储架构 §6.6）。
//!
//! NULL 由行级 NULL 位图表达，**不在数据区**（§6.6 注）——因此本编码
//! 没有"第三态"。

/// `BOOLEAN` 编码长度。
pub const BOOLEAN_LEN: usize = 1;

/// `BOOLEAN` 解码错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BooleanEncodingError;

impl std::fmt::Display for BooleanEncodingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BOOLEAN 编码非法（只接受 0/1 单字节）")
    }
}

impl std::error::Error for BooleanEncodingError {}

/// 编码：`false → 0x00`、`true → 0x01`。
#[must_use]
pub fn encode(value: bool) -> [u8; BOOLEAN_LEN] {
    [u8::from(value)]
}

/// 解码（严格：只接受单字节 0/1）。
pub fn decode(bytes: &[u8]) -> Result<bool, BooleanEncodingError> {
    match bytes {
        [0] => Ok(false),
        [1] => Ok(true),
        _ => Err(BooleanEncodingError),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_strictness() {
        assert_eq!(encode(false), [0]);
        assert_eq!(encode(true), [1]);
        assert!(!decode(&[0]).unwrap());
        assert!(decode(&[1]).unwrap());
        // 严格：其余一律拒绝（NULL 不在数据区，2 也不是"第三态"）。
        assert!(decode(&[2]).is_err());
        assert!(decode(&[]).is_err());
        assert!(decode(&[1, 0]).is_err());
        assert!(decode(&[0xFF]).is_err());
    }
}
