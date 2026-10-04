//! `NUMBER` 的物理编码：**变长 base-100 科学计数法**（存储架构 §6.5）。
//!
//! `INTEGER` / `FLOAT32` / `FLOAT64` 与 `NUMBER` **共用这一套编码**
//! （§6.5/§6.6：整数快路径只存在于计算路径，**不得改变磁盘编码**）。
//!
//! # 格式
//!
//! ```text
//! 字节 0：符号位 + 指数
//!   位 7 (0x80)：1 = 正数或零，0 = 负数
//!   位 6–0     ：指数偏移 65
//!               正数：实际指数 = 字节 0 − 193
//!               负数：实际指数 = 62 − 字节 0
//! 字节 1..n：数字位（base 100，每位 0–99）
//!   正数：每字节 = 值 + 1
//!   负数：每字节 = 101 − 值
//!   负数末尾：标记字节 0x66（102）
//! 特殊：零 = 单字节 0x80
//! 上限：20 个数字字节（≈40 位十进制，覆盖 p ≤ 38）
//! ```
//!
//! 指数的偏移与负数取补使**字节序 = 数值序**：索引比较无需解码
//! （§6.5 的"为什么 +1 和 101−值"；负数指数取补是同一目的的另一半——
//! 知识库口径："负数的实际指数 = 62 − 第一字节"）。
//!
//! # 规范形式（同一数值只有一种编码）
//!
//! 数值 = `D1.D2D3… × 100^e`：`D1 ∈ 1..=99`（首位非零）、其余 `0..=99`、
//! `e ∈ −65..=62`、**尾零组已剥离**、组内固定两位十进制（末组不足补零）。

use std::cmp::Ordering;
use std::fmt;

/// 数字字节上限（20 个 → ≈40 位十进制，覆盖 `p ≤ 38`）。
pub const MAX_DIGIT_BYTES: usize = 20;

const EXP_MIN: i32 = -65;
const EXP_MAX: i32 = 62;

/// 负数末尾标记字节（102）。
const NEG_TERMINATOR: u8 = 102;

/// 零的编码（单字节）。
const ZERO_BYTE: u8 = 0x80;

/// `NUMBER` 解析 / 编码错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumberError {
    /// 十进制文本语法错误（只接受 `[+-]digits[.digits]`，至少一位数字）。
    InvalidSyntax,
    /// 数字位超过上限（20 个数字字节 ≈ 40 位十进制；`NUMBER(38)` 需 ≤ 38 位有效数字）。
    TooManyDigits,
    /// 指数越过 −65..=62。
    ExponentOutOfRange,
    /// 编码字节流非法（数字位越界 / 首位零组 / 负数缺标记字节 / 尾零组未剥离）。
    InvalidEncoding,
}

impl fmt::Display for NumberError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            NumberError::InvalidSyntax => "NUMBER 文本语法错误",
            NumberError::TooManyDigits => "NUMBER 有效数字超过上限（20 个数字字节）",
            NumberError::ExponentOutOfRange => "NUMBER 指数越界（−65..=62）",
            NumberError::InvalidEncoding => "NUMBER 编码非法",
        })
    }
}

impl std::error::Error for NumberError {}

/// 精确十进制数（**不得用浮点承载**；规范形式见模块文档）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Number {
    negative: bool,
    /// base-100 数字组；零 = 空向量；首位 ∈ 1..=99，其余 ∈ 0..=99，尾组非零。
    groups: Vec<u8>,
    /// base-100 指数。
    exp: i32,
}

impl Number {
    /// 零。
    #[must_use]
    pub fn zero() -> Self {
        Self {
            negative: false,
            groups: Vec::new(),
            exp: 0,
        }
    }

    /// 是否为零。
    #[must_use]
    pub fn is_zero(&self) -> bool {
        self.groups.is_empty()
    }

    /// 解析十进制文本（`[+-]digits[.digits]`；不接受指数记法与空白）。
    pub fn parse(text: &str) -> Result<Self, NumberError> {
        let bytes = text.as_bytes();
        if bytes.is_empty() {
            return Err(NumberError::InvalidSyntax);
        }
        let (negative, rest) = match bytes[0] {
            b'-' => (true, &bytes[1..]),
            b'+' => (false, &bytes[1..]),
            _ => (false, bytes),
        };
        let mut int_len: i64 = 0;
        let mut digits: Vec<u8> = Vec::new();
        let mut seen_point = false;
        let mut any_digit = false;
        for &b in rest {
            match b {
                b'0'..=b'9' => {
                    digits.push(b - b'0');
                    if !seen_point {
                        int_len += 1;
                    }
                    any_digit = true;
                }
                b'.' if !seen_point => seen_point = true,
                _ => return Err(NumberError::InvalidSyntax),
            }
        }
        if !any_digit {
            return Err(NumberError::InvalidSyntax);
        }
        Self::from_digits(digits, int_len, negative)
    }

    /// 由"十进制数字串 + 小数点前位数"构造规范形式。
    ///
    /// 记 `value = digits × 10^(L − n)`（`L = int_len` 可为负——小数点落在
    /// 数字串之前，如 `0.05` 的有效串 `5` 对应 `L = −1`）。
    fn from_digits(
        mut digits: Vec<u8>,
        mut int_len: i64,
        negative: bool,
    ) -> Result<Self, NumberError> {
        // 去前导零：去掉多少，L 同步减多少（可为负）。
        let lead = digits.iter().take_while(|&&d| d == 0).count();
        if lead > 0 {
            digits.drain(..lead);
            int_len -= lead as i64;
        }
        // 去尾零（不改数值）。
        while digits.last() == Some(&0) {
            digits.pop();
        }
        if digits.is_empty() {
            return Ok(Self::zero());
        }

        // P = 首位数字的十进制幂；e = floor(P / 2)，使 M = value / 100^e ∈ [1, 100)。
        let p = int_len - 1;
        let e = p.div_euclid(2);
        if !(EXP_MIN as i64..=EXP_MAX as i64).contains(&e) {
            return Err(NumberError::ExponentOutOfRange);
        }
        // M 的整数位数：pos = L − 2e ∈ {1, 2}（由构造保证）。
        let pos = (int_len - 2 * e) as usize;
        debug_assert!(pos == 1 || pos == 2, "M ∈ [1,100) ⇒ 整数位 1..2");

        // M 的数字串 = digits 前 pos 位（不足补零）；其余为小数部分。
        let mut int_part: Vec<u8> = digits.iter().copied().take(pos).collect();
        while int_part.len() < pos {
            int_part.push(0);
        }
        let frac_part: &[u8] = if digits.len() > pos {
            &digits[pos..]
        } else {
            &[]
        };

        let mut groups = Vec::with_capacity(1 + frac_part.len().div_ceil(2));
        groups.push(digit_slice_value(&int_part));
        let mut rest = frac_part;
        while !rest.is_empty() {
            let take = rest.len().min(2);
            let mut pair = rest[..take].to_vec();
            if pair.len() == 1 {
                pair.push(0); // base-100 组内是两位十进制
            }
            groups.push(digit_slice_value(&pair));
            rest = &rest[take..];
        }
        // 尾零组剥离（规范形式）。
        while groups.last() == Some(&0) {
            groups.pop();
        }
        if groups.is_empty() {
            return Ok(Self::zero());
        }
        if groups.len() > MAX_DIGIT_BYTES {
            return Err(NumberError::TooManyDigits);
        }
        debug_assert!((1..=99).contains(&groups[0]), "首位组归一化");
        Ok(Self {
            negative,
            groups,
            exp: e as i32,
        })
    }

    /// 编码为字节流（规范形式）。
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        if self.is_zero() {
            return vec![ZERO_BYTE];
        }
        let mut out = Vec::with_capacity(self.groups.len() + 2);
        if self.negative {
            out.push((62 - self.exp) as u8);
            for &d in &self.groups {
                out.push((101 - d as i32) as u8);
            }
            out.push(NEG_TERMINATOR);
        } else {
            out.push((193 + self.exp) as u8);
            for &d in &self.groups {
                out.push((d as i32 + 1) as u8);
            }
        }
        out
    }

    /// 编码字节数。
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        self.encode().len()
    }

    /// 由编码字节流解码（**严格**：非规范形式一律拒绝）。
    pub fn decode(bytes: &[u8]) -> Result<Self, NumberError> {
        if bytes.is_empty() {
            return Err(NumberError::InvalidEncoding);
        }
        if bytes.len() == 1 && bytes[0] == ZERO_BYTE {
            return Ok(Self::zero());
        }
        let b0 = bytes[0];
        let negative = b0 < ZERO_BYTE;
        let exp = if negative {
            62 - b0 as i32
        } else {
            b0 as i32 - 193
        };
        if !(EXP_MIN..=EXP_MAX).contains(&exp) {
            return Err(NumberError::InvalidEncoding);
        }
        let mut digit_bytes = &bytes[1..];
        if negative {
            if digit_bytes.last() != Some(&NEG_TERMINATOR) {
                return Err(NumberError::InvalidEncoding);
            }
            digit_bytes = &digit_bytes[..digit_bytes.len() - 1];
        }
        if digit_bytes.is_empty() || digit_bytes.len() > MAX_DIGIT_BYTES {
            return Err(NumberError::InvalidEncoding);
        }
        let mut groups = Vec::with_capacity(digit_bytes.len());
        for (i, &b) in digit_bytes.iter().enumerate() {
            let d = if negative {
                101 - b as i32
            } else {
                b as i32 - 1
            };
            if !(0..=99).contains(&d) || (i == 0 && d == 0) {
                return Err(NumberError::InvalidEncoding);
            }
            groups.push(d as u8);
        }
        if groups.last() == Some(&0) {
            return Err(NumberError::InvalidEncoding); // 尾零组未剥离 = 非规范
        }
        Ok(Self {
            negative,
            groups,
            exp,
        })
    }

    /// 规范十进制文本（无多余符号与末尾零）。
    #[must_use]
    pub fn to_decimal_string(&self) -> String {
        if self.is_zero() {
            return "0".to_owned();
        }
        // 数字串：首组 1–2 位，其后每组固定 2 位。
        let mut ms: Vec<u8> = Vec::new();
        let first = self.groups[0];
        if first >= 10 {
            ms.push(first / 10);
        }
        ms.push(first % 10);
        let first_len = ms.len();
        for &g in &self.groups[1..] {
            ms.push(g / 10);
            ms.push(g % 10);
        }
        // 小数点位置 = 首组位数 + 2e。
        let point = first_len as i64 + 2 * self.exp as i64;
        let len = ms.len() as i64;

        let (mut int_str, mut frac_str) = if point <= 0 {
            let mut frac = String::new();
            for _ in 0..(-point) {
                frac.push('0');
            }
            for &d in &ms {
                frac.push((b'0' + d) as char);
            }
            ("0".to_owned(), frac)
        } else if point >= len {
            let mut int = String::with_capacity(point as usize);
            for &d in &ms {
                int.push((b'0' + d) as char);
            }
            for _ in 0..(point - len) {
                int.push('0');
            }
            (int, String::new())
        } else {
            let cut = point as usize;
            let int: String = ms[..cut].iter().map(|&d| (b'0' + d) as char).collect();
            let frac: String = ms[cut..].iter().map(|&d| (b'0' + d) as char).collect();
            (int, frac)
        };
        // 末尾零不显著。
        while frac_str.ends_with('0') {
            frac_str.pop();
        }
        let mut s = String::new();
        if self.negative {
            s.push('-');
        }
        s.push_str(&int_str);
        if !frac_str.is_empty() {
            s.push('.');
            s.push_str(&frac_str);
        }
        int_str.clear();
        s
    }

    /// 数值比较（规范形式下与字节序一致——由属性测试保证）。
    fn cmp_numeric(&self, other: &Self) -> Ordering {
        match (self.is_zero(), other.is_zero()) {
            (true, true) => return Ordering::Equal,
            (true, false) => {
                return if other.negative {
                    Ordering::Greater
                } else {
                    Ordering::Less
                };
            }
            (false, true) => {
                return if self.negative {
                    Ordering::Less
                } else {
                    Ordering::Greater
                };
            }
            (false, false) => {}
        }
        if self.negative != other.negative {
            return if self.negative {
                Ordering::Less
            } else {
                Ordering::Greater
            };
        }
        // 同号：比量值（指数 → 组序列；前缀关系下长者量值更大，
        // 因为尾零组已剥离、多出的组必然带非零贡献）。
        let mag = match self.exp.cmp(&other.exp) {
            Ordering::Equal => {
                let a = &self.groups;
                let b = &other.groups;
                let mut ord = Ordering::Equal;
                for i in 0..a.len().min(b.len()) {
                    if a[i] != b[i] {
                        ord = a[i].cmp(&b[i]);
                        break;
                    }
                }
                if ord == Ordering::Equal {
                    a.len().cmp(&b.len())
                } else {
                    ord
                }
            }
            o => o,
        };
        if self.negative {
            mag.reverse()
        } else {
            mag
        }
    }
}

/// 数字位切片 → 数值（0..=99）。
fn digit_slice_value(digits: &[u8]) -> u8 {
    let mut v: u16 = 0;
    for &d in digits {
        v = v * 10 + d as u16;
    }
    v as u8
}

impl PartialOrd for Number {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Number {
    fn cmp(&self, other: &Self) -> Ordering {
        self.cmp_numeric(other)
    }
}

impl fmt::Display for Number {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_decimal_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 已知向量：Oracle `dump()` 的规范转储（与 KB 口径一致）。
    #[test]
    fn canonical_oracle_vectors() {
        let cases: &[(&str, &[u8])] = &[
            ("0", &[0x80]),
            ("1", &[193, 2]),
            ("100", &[194, 2]),
            ("-1", &[62, 100, 102]),
            ("0.5", &[192, 51]),
            ("1234.56", &[194, 13, 35, 57]),
            ("-1234.56", &[61, 89, 67, 45, 102]),
        ];
        for (text, bytes) in cases {
            let n = Number::parse(text).expect("可解析");
            assert_eq!(n.encode(), *bytes, "{text} 的编码");
            let back = Number::decode(bytes).expect("可解码");
            assert_eq!(back.to_decimal_string(), *text, "{text} 往返");
        }
    }

    #[test]
    fn zero_is_canonical_single_byte() {
        for t in ["0", "-0", "0.0", "0.000", "+0"] {
            let n = Number::parse(t).unwrap();
            assert!(n.is_zero(), "{t} 是零");
            assert_eq!(n.encode(), vec![0x80], "{t} 规范为零");
        }
    }

    #[test]
    fn normalization_and_text_roundtrip() {
        let cases: &[(&str, &str)] = &[
            ("1.0", "1"),
            ("12.340", "12.34"),
            ("0.50", "0.5"),
            ("0.05", "0.05"),
            ("10", "10"),
            ("-0.0", "0"),
            ("+7", "7"),
            ("0.0001", "0.0001"),
            ("1000000", "1000000"),
        ];
        for (text, want) in cases {
            let n = Number::parse(text).unwrap();
            assert_eq!(n.to_decimal_string(), *want, "{text} 规范文本");
            assert_eq!(Number::decode(&n.encode()).unwrap(), n, "{text} 往返");
        }
        // 同一数值的不同文本 → 同一编码。
        assert_eq!(
            Number::parse("1.0").unwrap().encode(),
            Number::parse("1").unwrap().encode()
        );
        assert_eq!(
            Number::parse("12.340").unwrap().encode(),
            Number::parse("12.34").unwrap().encode()
        );
    }

    #[test]
    fn parse_rejects_bad_syntax() {
        for t in ["", ".", "-", "+", "1.2.3", "1e3", " 1", "1 ", "0x10", "١٢٣"] {
            assert!(Number::parse(t).is_err(), "应拒绝 {t:?}");
        }
    }

    #[test]
    fn limits_are_enforced() {
        // 41 位有效数字 → 超过 20 个数字字节。
        let long = format!("1{}", "9".repeat(40));
        assert_eq!(Number::parse(&long), Err(NumberError::TooManyDigits));
        // 38 位有效数字 → 19 个数字字节，可编码。
        let p38 = format!("1{}", "9".repeat(37));
        let n = Number::parse(&p38).expect("38 位可编码");
        assert!(n.encode().len() <= 1 + MAX_DIGIT_BYTES);
        assert_eq!(n.to_decimal_string(), p38);
        // 指数越界：1 后跟 126 个零 → e = 63；0.0…1（131 个零）→ e = −66。
        let huge = format!("1{}", "0".repeat(126));
        assert_eq!(Number::parse(&huge), Err(NumberError::ExponentOutOfRange));
        let tiny = format!("0.{}1", "0".repeat(131));
        assert_eq!(Number::parse(&tiny), Err(NumberError::ExponentOutOfRange));
    }

    #[test]
    fn decode_is_strict() {
        assert!(Number::decode(&[193, 1, 2]).is_err(), "首位零组");
        assert!(Number::decode(&[194, 2, 1]).is_err(), "尾零组未剥离");
        assert!(Number::decode(&[62, 100]).is_err(), "负数缺标记字节");
        assert!(Number::decode(&[193, 101]).is_err(), "数字位越界");
        assert!(Number::decode(&[]).is_err());
    }

    #[test]
    fn byte_order_equals_numeric_order() {
        let fixed = [
            "0", "0.0001", "0.001", "0.01", "0.05", "0.5", "0.99", "1", "1.0001", "9.99", "10",
            "12.3", "99", "100", "1234.56", "99999", "1000000", "-0.0001", "-0.001", "-0.5",
            "-0.99", "-1", "-12.3", "-100", "-1234.56", "-99999", "-1000000",
        ];
        let mut nums: Vec<Number> = fixed.iter().map(|t| Number::parse(t).unwrap()).collect();
        // 确定性随机样本（含正负、数量级）。
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        for _ in 0..300 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            let sign = if state & 1 == 0 { "-" } else { "" };
            let int = (state >> 8) % 1_000_000;
            let frac = (state >> 32) % 100_000;
            nums.push(Number::parse(&format!("{sign}{int}.{frac:05}")).unwrap());
        }
        nums.sort();
        for w in nums.windows(2) {
            let (a, b) = (&w[0], &w[1]);
            assert_eq!(a.cmp(b), Ordering::Less, "排序后 {a} < {b}");
            assert_eq!(
                a.encode().cmp(&b.encode()),
                Ordering::Less,
                "字节序不等于数值序：{a} vs {b}"
            );
        }
    }

    #[test]
    fn roundtrip_property() {
        let mut state: u64 = 0xDEAD_BEEF_CAFE_F00D;
        for _ in 0..500 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            let sign = if state & 1 == 0 { "-" } else { "" };
            let int = state % 100_000_000;
            let frac = (state >> 40) % 1_000_000_000;
            let text = format!("{sign}{int}.{frac:09}");
            let n = Number::parse(&text).unwrap();
            let decoded = Number::decode(&n.encode()).unwrap();
            assert_eq!(decoded, n, "{text} 往返");
            assert_eq!(decoded.to_decimal_string(), n.to_decimal_string());
        }
    }
}
