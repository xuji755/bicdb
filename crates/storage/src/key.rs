//! **索引键的复合编码**（新定 2026-10-06；`arch/09` §9.1 的"键即字节序"
//! 缺的那一块：多个键列怎么拼成一个可**字节比较**的键）。
//!
//! ```text
//! 键 = 各分量的 顺序拼接，每个分量：
//!   非 NULL： 0x01 ∥ 载荷（**保序编码**；载荷内 0x00 → 0x00 0xFF 转义）∥ 0x00
//!   NULL   ： 0x02
//! ```
//!
//! **为什么这样定**（三条性质，逐条可证）：
//!
//! | # | 性质 | 理由 |
//! | --- | --- | --- |
//! | 1 | **字节序 = 元组序** | 各列编码保序（`arch/06` §6.0 原则①）；分量内 `0x00` 转义后**终止符唯一**（FDB 同款手法）；分量间由标记字节定序 |
//! | 2 | **NULL 排最后**（升序索引） | 非 NULL 分量恒以 `0x01` 起头、NULL 恒为 `0x02` ⇒ `0x01 < 0x02`，与载荷无关（Oracle 的升序索引 NULL 位置） |
//! | 3 | **前缀一致性** | 短编码是长编码的前缀 ⇒ 短者先遇终止符（`0x00` 后跟标记 `0x01/0x02`），长者该位是 `0x00 0xFF`（转义）或更大的载荷字节 ⇒ 短者恒小——与"保序编码下前缀即小值"自洽 |
//!
//! **载荷从哪来**：调用方给各列值的**保序编码**（`NUMBER::encode`、`DATE` 7B、
//! `VARCHAR2` 原始字节、`BOOLEAN` 1B……）——本模块只管**拼装与转义**，
//! 不解码、不知道类型（`arch/06` §6.0 原则③"自描述"由各列的编码承担）。

use std::fmt;

/// 分量终止符。
pub const TERMINATOR: u8 = 0x00;
/// 转义指示字节（载荷内 `0x00` → `0x00 0xFF`）。
pub const ESCAPE: u8 = 0xFF;
/// 非 NULL 分量标记（恒大于……见下：NULL 标记更大，故非 NULL 在前）。
pub const MARK_PRESENT: u8 = 0x01;
/// NULL 分量标记（**排在所有非 NULL 之后**——升序索引 NULL 最后）。
pub const MARK_NULL: u8 = 0x02;

/// 键编码错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyError {
    /// 缺终止符 / 尾部残缺。
    Truncated,
    /// 标记字节非法。
    BadMarker(u8),
    /// 转义序列非法（`0x00` 后跟的不是 `0xFF`/标记）。
    BadEscape,
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyError::Truncated => f.write_str("键编码截断（缺终止符）"),
            KeyError::BadMarker(b) => write!(f, "键编码标记字节非法：{b:#04x}"),
            KeyError::BadEscape => f.write_str("键编码转义序列非法"),
        }
    }
}

impl std::error::Error for KeyError {}

/// **追加一个非 NULL 分量**（载荷 = 该列的保序编码）。
pub fn push_present(out: &mut Vec<u8>, payload: &[u8]) {
    out.push(MARK_PRESENT);
    for &b in payload {
        if b == TERMINATOR {
            out.push(TERMINATOR);
            out.push(ESCAPE);
        } else {
            out.push(b);
        }
    }
    out.push(TERMINATOR);
}

/// **追加一个 NULL 分量**。
pub fn push_null(out: &mut Vec<u8>) {
    out.push(MARK_NULL);
}

/// **编码一个键**（各分量：`Some(载荷)` 或 `None` = NULL）。
#[must_use]
pub fn encode(components: &[Option<&[u8]>]) -> Vec<u8> {
    let mut out = Vec::new();
    for c in components {
        match c {
            Some(p) => push_present(&mut out, p),
            None => push_null(&mut out),
        }
    }
    out
}

/// **解码一个键**（回各分量的载荷；**诊断/回表用**——比较路径不需要解码）。
pub fn decode(key: &[u8]) -> Result<Vec<Option<Vec<u8>>>, KeyError> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < key.len() {
        let mark = key[i];
        i += 1;
        match mark {
            MARK_NULL => out.push(None),
            MARK_PRESENT => {
                let mut payload = Vec::new();
                loop {
                    let b = *key.get(i).ok_or(KeyError::Truncated)?;
                    i += 1;
                    if b == TERMINATOR {
                        // 转义：0x00 0xFF ⇒ 载荷里的 0x00。
                        if key.get(i) == Some(&ESCAPE) {
                            payload.push(TERMINATOR);
                            i += 1;
                            continue;
                        }
                        break;
                    }
                    payload.push(b);
                }
                out.push(Some(payload));
            }
            b => return Err(KeyError::BadMarker(b)),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(p: Option<&[u8]>) -> Vec<u8> {
        encode(&[p])
    }

    #[test]
    fn single_component_round_trip_and_escape() {
        assert_eq!(
            decode(&enc(Some(b"abc"))).unwrap(),
            vec![Some(b"abc".to_vec())]
        );
        // 载荷含 0x00：转义往返。
        let payload = [1u8, 0x00, 2, 0x00, 0x00, 3];
        let key = enc(Some(&payload));
        assert_eq!(decode(&key).unwrap(), vec![Some(payload.to_vec())]);
        // 转义确实发生（出现 0x00 0xFF 序列）。
        assert!(key.windows(2).any(|w| w == [0x00, 0xFF]));
        // NULL 往返。
        assert_eq!(decode(&enc(None)).unwrap(), vec![None]);
        // 截断/坏标记。
        assert!(matches!(
            decode(&[MARK_PRESENT, 1, 2]),
            Err(KeyError::Truncated)
        ));
        assert!(matches!(decode(&[0x77]), Err(KeyError::BadMarker(0x77))));
    }

    #[test]
    fn null_sorts_after_everything() {
        // NULL 排最后：与任何非 NULL 载荷（含以 0xFF 起头的字节串）相比都更大。
        let null_key = enc(None);
        for payload in [&b""[..], &b"a"[..], &[0xFF, 0xFF][..], &[0xFE][..]] {
            let k = enc(Some(payload));
            assert!(k < null_key, "非 NULL {payload:?} 应在 NULL 之前");
        }
    }

    #[test]
    fn byte_order_equals_tuple_order() {
        // 单分量：前缀关系与数值序一致（保序编码的性质在此被拼装保持）。
        let a = enc(Some(&[0xC1, 0x02])); // 例：NUMBER 1 的编码形态
        let b = enc(Some(&[0xC1, 0x02, 0x32])); // 例：1.5（更长的编码）
        assert!(a < b, "短编码是长编码的前缀 ⇒ 短者小");
        // 复分量：先比第一分量，再比第二。
        let k1 = encode(&[Some(&[0x01]), Some(b"a")]);
        let k2 = encode(&[Some(&[0x01]), Some(b"b")]);
        let k3 = encode(&[Some(&[0x02]), Some(b"a")]);
        assert!(k1 < k2, "同首分量 ⇒ 比分量 2");
        assert!(k2 < k3, "分量 1 更大 ⇒ 整体更大（不受分量 2 影响）");
        // 复分量 + NULL 位置。
        let k4 = encode(&[Some(&[0x02]), None]);
        assert!(k3 < k4, "同首分量 ⇒ 非 NULL 二分量在前");
    }

    #[test]
    fn payload_with_escaped_zero_still_orders_after_a_prefix() {
        // 前缀情形 + 转义：P 与 P∥0x00∥X 的次序仍与"前缀即小"一致。
        let short = enc(Some(&[0xC1, 0x02]));
        let long = enc(Some(&[0xC1, 0x02, 0x00, 0x07]));
        assert!(short < long, "短者先遇终止符（0x00 后跟标记 < 0xFF）");
    }

    #[test]
    fn overflow_free_composite_keys_keep_their_shape() {
        // 字典表的两类典型复合键：NUMBER+NUMBER、NUMBER+VARCHAR2。
        let k = encode(&[Some(&[0xC2]), Some(&[0xC3])]);
        assert_eq!(
            decode(&k).unwrap(),
            vec![Some(vec![0xC2]), Some(vec![0xC3])]
        );
        let k2 = encode(&[Some(&[0x02]), Some("obj$".as_bytes())]);
        assert_eq!(
            decode(&k2).unwrap(),
            vec![Some(vec![0x02]), Some(b"obj$".to_vec())]
        );
    }
}
