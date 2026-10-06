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
//! # 算术
//!
//! [`Number::add`] / [`Number::sub`] / [`Number::mul`] / [`Number::div`] 全部
//! **在 base-100 十进制上做**（REQ-TYP-002：不得经二进制浮点）：加减乘精确，
//! 超 20 组按**半进位、远离零**舍入；除法取 20 组有效数字后同法舍入
//! （除零 = [`NumberError::DivisionByZero`]）。**Oracle 除法舍入细则未核验**
//! （知识库未收录）——见 `doc/evidence/exec-ops-20261006` 的未核验项。
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
    /// 除以零。
    DivisionByZero,
}

impl fmt::Display for NumberError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            NumberError::InvalidSyntax => "NUMBER 文本语法错误",
            NumberError::TooManyDigits => "NUMBER 有效数字超过上限（20 个数字字节）",
            NumberError::ExponentOutOfRange => "NUMBER 指数越界（−65..=62）",
            NumberError::InvalidEncoding => "NUMBER 编码非法",
            NumberError::DivisionByZero => "NUMBER 除以零",
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

    // -- 算术（十进制精确；不经浮点——REQ-TYP-002） -------------------------

    /// 取负（零不带符号）。
    #[must_use]
    pub fn neg(&self) -> Self {
        if self.is_zero() {
            return Self::zero();
        }
        Self {
            negative: !self.negative,
            groups: self.groups.clone(),
            exp: self.exp,
        }
    }

    /// **绝对值**。
    #[must_use]
    pub fn abs(&self) -> Self {
        if self.is_zero() {
            return Self::zero();
        }
        Self {
            negative: false,
            groups: self.groups.clone(),
            exp: self.exp,
        }
    }

    /// **加法**（精确；结果超 20 组按**半进位、远离零**舍入）。
    pub fn add(&self, other: &Self) -> Result<Self, NumberError> {
        if self.is_zero() {
            return Ok(other.clone());
        }
        if other.is_zero() {
            return Ok(self.clone());
        }
        if self.negative == other.negative {
            let (digits, exp) = add_magnitudes(self, other);
            Self::canonical(self.negative, digits, exp)
        } else {
            // 异号：**按幅值**取大减小，符号随幅值较大者（用 self.cmp 会因符号
            // 翻转把"大减小"的方向搞反——实测：-3 + 1 曾算出乱码）。
            match self.abs().cmp(&other.abs()) {
                Ordering::Equal => Ok(Self::zero()),
                Ordering::Greater => {
                    let (digits, exp) = sub_magnitudes(self, other);
                    Self::canonical(self.negative, digits, exp)
                }
                Ordering::Less => {
                    let (digits, exp) = sub_magnitudes(other, self);
                    Self::canonical(other.negative, digits, exp)
                }
            }
        }
    }

    /// **减法**（`self - other`）。
    pub fn sub(&self, other: &Self) -> Result<Self, NumberError> {
        self.add(&other.neg())
    }

    /// **乘法**（精确；结果超 20 组按半进位舍入）。
    pub fn mul(&self, other: &Self) -> Result<Self, NumberError> {
        if self.is_zero() || other.is_zero() {
            return Ok(Self::zero());
        }
        let prod = mul_digits(&self.groups, &other.groups);
        let exp = self.point_exp() + other.point_exp() + prod.len() as i32 - 1;
        Self::canonical(self.negative != other.negative, prod, exp)
    }

    /// **除法**（商取 20 组有效数字 + 半进位舍入；除零 ⇒
    /// [`NumberError::DivisionByZero`]）。
    ///
    /// 舍入口径：**20 个 base-100 组（40 位十进制）处、半进位、远离零**
    /// ——与编码上限（[`MAX_DIGIT_BYTES`]）一致。**注**：Oracle 的除法
    /// 舍入细则（38 位有效数字的确切落点）知识库未收录，记为**未核验**
    /// （证据包 `exec-ops-20261006` §未核验），待参考环境实测后校正。
    pub fn div(&self, other: &Self) -> Result<Self, NumberError> {
        if other.is_zero() {
            return Err(NumberError::DivisionByZero);
        }
        if self.is_zero() {
            return Ok(Self::zero());
        }
        // q = floor(|a|·100^k / |b|)，k 取到商有 ≥ 22 组（20 组 + 2 组余量）。
        let k = 22 + (other.groups.len() as i32 - self.groups.len() as i32).max(0);
        let scaled = scale_digits(&self.groups, k as usize);
        let (q, _) = div_digits(&scaled, &other.groups);
        let exp = self.point_exp() - other.point_exp() - k + q.len() as i32 - 1;
        Self::canonical(self.negative != other.negative, q, exp)
    }

    /// `value = int(groups) × 100^point_exp` 里的 `point_exp`。
    fn point_exp(&self) -> i32 {
        self.exp - self.groups.len() as i32 + 1
    }

    /// **规范形式收口**：去前导零 → 超限舍入（半进位、远离零）→ 去尾零 →
    /// 指数域检查。空数字 = 零（不带符号）。
    fn canonical(negative: bool, mut digits: Vec<u8>, mut exp: i32) -> Result<Self, NumberError> {
        while digits.first() == Some(&0) {
            digits.remove(0);
            exp -= 1;
        }
        if digits.is_empty() {
            return Ok(Self::zero());
        }
        if digits.len() > MAX_DIGIT_BYTES {
            let guard = digits[MAX_DIGIT_BYTES];
            digits.truncate(MAX_DIGIT_BYTES);
            if guard >= 50 {
                let mut i = digits.len();
                loop {
                    if i == 0 {
                        digits.insert(0, 1);
                        exp += 1;
                        break;
                    }
                    if digits[i - 1] == 99 {
                        digits[i - 1] = 0;
                        i -= 1;
                    } else {
                        digits[i - 1] += 1;
                        break;
                    }
                }
            }
        }
        while digits.last() == Some(&0) {
            digits.pop();
        }
        if !(EXP_MIN..=EXP_MAX).contains(&exp) {
            return Err(NumberError::ExponentOutOfRange);
        }
        Ok(Self {
            negative,
            groups: digits,
            exp,
        })
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

// -- 算术的 digit 数组助手（全部 base-100、大端：索引 0 = 最高位） ----------

/// 最低非零位次（`value = Σ groups[i] × 100^(exp − i)` 的 `exp − len + 1`）。
fn low_place(n: &Number) -> i32 {
    n.exp - n.groups.len() as i32 + 1
}

/// 同号**幅值相加**：返回（数字数组，首组的位次）。末位带进位余量。
fn add_magnitudes(a: &Number, b: &Number) -> (Vec<u8>, i32) {
    let low = low_place(a).min(low_place(b));
    let high = a.exp.max(b.exp) + 1;
    let span = (high - low + 1) as usize;
    let mut acc = vec![0u8; span];
    for n in [a, b] {
        for (i, &g) in n.groups.iter().enumerate() {
            let place = n.exp - i as i32;
            acc[(high - place) as usize] += g; // 同一位次至多两组 ≤ 198 < 256
        }
    }
    for i in (1..span).rev() {
        let v = u32::from(acc[i]);
        acc[i] = (v % 100) as u8;
        acc[i - 1] += (v / 100) as u8;
    }
    (acc, high)
}

/// **幅值相减**（要求 `|a| ≥ |b|`）：返回（数字数组，首组的位次）。
fn sub_magnitudes(a: &Number, b: &Number) -> (Vec<u8>, i32) {
    let low = low_place(a).min(low_place(b));
    let high = a.exp; // |a| ≥ |b| ⇒ a.exp ≥ b.exp
    let span = (high - low + 1) as usize;
    let mut acc = vec![0i32; span];
    for (i, &g) in a.groups.iter().enumerate() {
        let place = a.exp - i as i32;
        acc[(high - place) as usize] += i32::from(g);
    }
    for (i, &g) in b.groups.iter().enumerate() {
        let place = b.exp - i as i32;
        acc[(high - place) as usize] -= i32::from(g);
    }
    for i in (1..span).rev() {
        if acc[i] < 0 {
            acc[i] += 100;
            acc[i - 1] -= 1;
        }
    }
    let digits = acc.into_iter().map(|v| v as u8).collect();
    (digits, high)
}

/// **数字数组乘法**（schoolbook，base-100）。
fn mul_digits(a: &[u8], b: &[u8]) -> Vec<u8> {
    let mut out = vec![0u32; a.len() + b.len()];
    for i in (0..a.len()).rev() {
        let mut carry = 0u32;
        for j in (0..b.len()).rev() {
            let cur = out[i + j + 1] + u32::from(a[i]) * u32::from(b[j]) + carry;
            out[i + j + 1] = cur % 100;
            carry = cur / 100;
        }
        out[i] += carry;
    }
    out.into_iter().map(|v| v as u8).collect()
}

/// 数字数组 × `100^k`（追加 `k` 个零组）。
fn scale_digits(d: &[u8], k: usize) -> Vec<u8> {
    let mut out = d.to_vec();
    out.extend(std::iter::repeat(0).take(k));
    out
}

/// 规范数字数组比较（无前导零；空 = 零）。
fn cmp_digits(a: &[u8], b: &[u8]) -> Ordering {
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

/// 数字数组加法（整数语义、右对齐）。
fn add_digits(a: &[u8], b: &[u8]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(a.len().max(b.len()) + 1);
    let (mut i, mut j, mut carry) = (a.len(), b.len(), 0u8);
    while i > 0 || j > 0 || carry > 0 {
        let mut v = u16::from(carry);
        if i > 0 {
            i -= 1;
            v += u16::from(a[i]);
        }
        if j > 0 {
            j -= 1;
            v += u16::from(b[j]);
        }
        out.push((v % 100) as u8);
        carry = (v / 100) as u8;
    }
    out.reverse();
    out
}

/// 数字数组 × 小整数（0..=99）。
fn mul_small(d: &[u8], q: u8) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(d.len() + 1);
    let mut carry = 0u16;
    for &g in d.iter().rev() {
        let v = u16::from(g) * u16::from(q) + carry;
        out.push((v % 100) as u8);
        carry = v / 100;
    }
    if carry > 0 {
        out.push(carry as u8);
    }
    out.reverse();
    out
}

/// 数字数组减法（整数语义；要求 `a ≥ b`）；结果可带前导零（调用方剥）。
fn sub_digits(a: &[u8], b: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; a.len()];
    let mut borrow = 0i32;
    let (mut i, mut j) = (a.len(), b.len());
    while i > 0 {
        i -= 1;
        let mut v = i32::from(a[i]) - borrow;
        if j > 0 {
            j -= 1;
            v -= i32::from(b[j]);
        }
        if v < 0 {
            v += 100;
            borrow = 1;
        } else {
            borrow = 0;
        }
        out[i] = v as u8;
    }
    out
}

/// `rem` 里能容纳的最大倍数 `q ∈ 0..=99`（`den × q ≤ rem`）。
fn largest_multiple(den: &[u8], rem: &[u8]) -> u8 {
    if cmp_digits(den, rem) == Ordering::Greater {
        return 0;
    }
    let mut acc = den.to_vec();
    let mut q = 1u8;
    loop {
        let next = add_digits(&acc, den);
        if cmp_digits(&next, rem) == Ordering::Greater || q == 99 {
            break;
        }
        acc = next;
        q += 1;
    }
    q
}

/// **长除法**（base-100）：返回（商、余数）；两者都剥前导零、空 = 零。
fn div_digits(num: &[u8], den: &[u8]) -> (Vec<u8>, Vec<u8>) {
    debug_assert!(!den.is_empty());
    let mut quot: Vec<u8> = Vec::with_capacity(num.len());
    let mut rem: Vec<u8> = Vec::new();
    for &d in num {
        rem.push(d);
        while rem.first() == Some(&0) {
            rem.remove(0);
        }
        let q = if rem.is_empty() {
            0
        } else {
            largest_multiple(den, &rem)
        };
        quot.push(q);
        if q != 0 {
            let sub = mul_small(den, q);
            rem = sub_digits(&rem, &sub);
            while rem.first() == Some(&0) {
                rem.remove(0);
            }
        }
    }
    while quot.first() == Some(&0) {
        quot.remove(0);
    }
    (quot, rem)
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

    // -- 算术（十进制精确；REQ-TYP-002） ------------------------------------

    fn num(t: &str) -> Number {
        Number::parse(t).expect("可解析")
    }

    fn sum(a: &str, b: &str) -> String {
        num(a).add(&num(b)).expect("相加").to_decimal_string()
    }

    fn diff(a: &str, b: &str) -> String {
        num(a).sub(&num(b)).expect("相减").to_decimal_string()
    }

    fn prod(a: &str, b: &str) -> String {
        num(a).mul(&num(b)).expect("相乘").to_decimal_string()
    }

    fn quot(a: &str, b: &str) -> String {
        num(a).div(&num(b)).expect("相除").to_decimal_string()
    }

    #[test]
    fn decimal_arithmetic_is_exact_not_binary_float() {
        // 0.1 + 0.2 = 0.3 精确（双精度会得到 0.30000000000000004）。
        assert_eq!(sum("0.1", "0.2"), "0.3");
        assert_eq!(sum("1.1", "2.2"), "3.3");
        assert_eq!(diff("0.3", "0.1"), "0.2");
        assert_eq!(
            diff("1", "0.99999999999999999999999999999999999999"),
            "0.00000000000000000000000000000000000001"
        );
        assert_eq!(prod("0.1", "0.2"), "0.02");
        assert_eq!(prod("1.5", "2"), "3");
        assert_eq!(quot("1", "8"), "0.125");
        assert_eq!(quot("10", "4"), "2.5");
    }

    #[test]
    fn signs_and_zero_are_canonical() {
        assert_eq!(sum("1", "-1"), "0");
        assert_eq!(diff("5", "5"), "0");
        assert_eq!(sum("-3", "1"), "-2");
        assert_eq!(prod("-3", "2"), "-6");
        assert_eq!(prod("-3", "-2"), "6");
        assert_eq!(quot("-6", "4"), "-1.5");
        assert_eq!(Number::zero().neg().to_decimal_string(), "0");
        let z = num("7").sub(&num("7")).unwrap();
        assert_eq!(z.encode(), vec![0x80], "算术零 = 规范零字节");
    }

    #[test]
    fn thirty_eight_digit_integers_stay_exact() {
        // 38 位 9 + 1 = 10^38（进位跨组、尾零组剥离后只剩 [1]）。
        let nines = "9".repeat(38);
        let one = "1".to_owned() + &"0".repeat(38);
        assert_eq!(sum(&nines, "1"), one);
        // 38 位 × 小整数：精确。
        let big = "12345678901234567890123456789012345678"; // 38 位
        assert_eq!(prod(big, "1"), big);
        // 38 位相加超限 ⇒ 半进位舍入（此处末位为 0，剥离尾零）。
        let a = "1".to_owned() + &"0".repeat(37); // 10^37
        assert_eq!(sum(&a, &a), "2".to_owned() + &"0".repeat(37));
    }

    #[test]
    fn division_keeps_twenty_groups_and_rounds_half_up_away_from_zero() {
        // 1/3 = 0.333…3（40 位 3：第 21 组 = 33 < 50，不进位）。
        let one_third = quot("1", "3");
        assert_eq!(one_third, "0.".to_owned() + &"3".repeat(40));
        // 2/3 = 0.666…7（第 21 组 = 66 ≥ 50 ⇒ 末组进位 66→67）。
        let two_thirds = quot("2", "3");
        assert_eq!(two_thirds, "0.".to_owned() + &"6".repeat(39) + "7");
        // 负数同幅值（远离零舍入）。
        assert_eq!(quot("-2", "3"), "-".to_owned() + &two_thirds);
        // 乘回：1/3 × 3 = 0.999…9（40 位 9——不是 1；除法有舍入，符合"不静默
        // 变成 1"的十进制语义）。
        assert_eq!(prod(&one_third, "3"), "0.".to_owned() + &"9".repeat(40));
    }

    #[test]
    fn division_by_zero_is_a_named_error() {
        assert_eq!(
            num("1").div(&Number::zero()),
            Err(NumberError::DivisionByZero)
        );
        // 零/非零 = 零；非零/自身 = 1。
        assert!(num("0").div(&num("7")).unwrap().is_zero());
        assert_eq!(quot("7", "7"), "1");
        assert_eq!(quot("-7", "-7"), "1");
    }

    #[test]
    fn additive_inverse_property_holds_for_exact_operands() {
        let vals = [
            "0.1",
            "-0.2",
            "1",
            "-3.5",
            "123456789.987654321",
            "-0.000000001",
            "100000000000000000000000",
            "99999999999999999999.5",
        ];
        for a in vals {
            for b in vals {
                let s = num(a).add(&num(b)).expect("相加");
                assert_eq!(
                    s.sub(&num(b)).expect("相减"),
                    num(a),
                    "({a}) + ({b}) - ({b})"
                );
            }
        }
    }

    #[test]
    fn exponent_domain_is_enforced() {
        // 上界：10^124（exp = 62）可表示；×100 ⇒ exp = 63 ⇒ 具名错误、不回绕。
        let top = num(&("1".to_owned() + &"0".repeat(124)));
        assert!(top.mul(&num("99")).is_ok(), "exp = 62 仍在域内");
        assert_eq!(
            top.mul(&num("100")).unwrap_err(),
            NumberError::ExponentOutOfRange
        );
        // 下界：10^-124（exp = −62）逐次 ÷100 到 10^-130（exp = −65，域内，
        // 注意 10^130 本身超出可表示域——不能直接构造除数）；再 ÷100 ⇒ −66 ⇒ 错。
        let mut bottom = num("1")
            .div(&num(&("1".to_owned() + &"0".repeat(124))))
            .unwrap();
        for _ in 0..3 {
            bottom = bottom.div(&num("100")).unwrap();
        }
        assert_eq!(
            bottom.to_decimal_string(),
            "0.".to_owned() + &"0".repeat(129) + "1",
            "10^-130 可表示（域下界）"
        );
        assert_eq!(
            bottom.div(&num("100")).unwrap_err(),
            NumberError::ExponentOutOfRange
        );
    }
}
