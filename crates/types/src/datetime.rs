//! `DATE` 与 `TIMESTAMP` 的物理编码（存储架构 §6.5）。
//!
//! ```text
//! DATE（固定 7 字节）：
//!   0 世纪 (year.div_euclid(100) + 100)   1 年 (year.rem_euclid(100) + 100)
//!   2 月(1–12)   3 日(1–31)   4 时+1   5 分+1   6 秒+1
//!
//! TIMESTAMP：前 7 字节与 DATE 完全一致；另加小数字段——
//!   声明精度 0   → 共 7 字节
//!   声明精度 1–9 → 共 11 字节（7 + 4 字节纳秒）
//! ```
//!
//! **偏移的两个目的**（§6.5 原文）：① **排序兼容**——字节序即时间序；
//! ② **支持公元前**——世纪可为负，`+100` 保证非负。
//! 本实现用**欧几里得除法**取世纪/年字段，负数年份（公元前）同样满足两条。
//!
//! **纳秒字段取大端**：这是 §6.0 第①条"排序友好"在 TIMESTAMP 上的延续——
//! 小端 u32 的字节比较不等于数值比较，会破坏"字节序即时间序"；
//! 大端下 4 字节的字典序与数值序一致。（这一取舍记录在
//! `doc/待讨论清单.md` 编码期新语义点，供复核。）
//!
//! **校验边界**：只做**字段范围**校验（世纪可编码、月 1–12、日 1–31、
//! 时 0–23、分/秒 0–59）；**不做日历完备校验**（闰年、月长——与成熟产品
//! 的宽容行为一致，完备性交给上层语义）。

use std::fmt;

/// `DATE` 编码长度。
pub const DATE_LEN: usize = 7;

/// `TIMESTAMP` 精度 0 的编码长度。
pub const TIMESTAMP_LEN_P0: usize = 7;

/// `TIMESTAMP` 精度 1–9 的编码长度。
pub const TIMESTAMP_LEN_P1_9: usize = 11;

/// 年份可编码范围（世纪字节 0..=255 且年为 u8 偏移）。
pub const YEAR_MIN: i32 = -10_000;
/// 见 [`YEAR_MIN`]。
pub const YEAR_MAX: i32 = 15_599;

/// 日期时间字段错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateTimeError {
    /// 字段越界（年 / 月 / 日 / 时 / 分 / 秒 / 纳秒 / 精度）。
    FieldOutOfRange,
    /// 编码字节流非法（长度不符或字段偏移失效）。
    InvalidEncoding,
}

impl fmt::Display for DateTimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            DateTimeError::FieldOutOfRange => "日期时间字段越界",
            DateTimeError::InvalidEncoding => "日期时间编码非法",
        })
    }
}

impl std::error::Error for DateTimeError {}

/// `DATE`：年月日时分秒（无时区；`CONV` §2——不存本地时间）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Date {
    year: i32,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
}

impl Date {
    /// 构造并做字段范围校验。
    pub fn new(
        year: i32,
        month: u8,
        day: u8,
        hour: u8,
        minute: u8,
        second: u8,
    ) -> Result<Self, DateTimeError> {
        if !(YEAR_MIN..=YEAR_MAX).contains(&year)
            || !(1..=12).contains(&month)
            || !(1..=31).contains(&day)
            || hour > 23
            || minute > 59
            || second > 59
        {
            return Err(DateTimeError::FieldOutOfRange);
        }
        Ok(Self {
            year,
            month,
            day,
            hour,
            minute,
            second,
        })
    }

    /// 年。
    #[must_use]
    pub fn year(&self) -> i32 {
        self.year
    }
    /// 月。
    #[must_use]
    pub fn month(&self) -> u8 {
        self.month
    }
    /// 日。
    #[must_use]
    pub fn day(&self) -> u8 {
        self.day
    }
    /// 时。
    #[must_use]
    pub fn hour(&self) -> u8 {
        self.hour
    }
    /// 分。
    #[must_use]
    pub fn minute(&self) -> u8 {
        self.minute
    }
    /// 秒。
    #[must_use]
    pub fn second(&self) -> u8 {
        self.second
    }

    /// 编码为 7 字节（字段偏移见模块文档）。
    #[must_use]
    pub fn encode(&self) -> [u8; DATE_LEN] {
        [
            (self.year.div_euclid(100) + 100) as u8,
            (self.year.rem_euclid(100) + 100) as u8,
            self.month,
            self.day,
            self.hour + 1,
            self.minute + 1,
            self.second + 1,
        ]
    }

    /// 由 7 字节解码（字段偏移失效即拒绝）。
    pub fn decode(bytes: &[u8]) -> Result<Self, DateTimeError> {
        if bytes.len() != DATE_LEN {
            return Err(DateTimeError::InvalidEncoding);
        }
        let (c, y, m, d, hh, mm, ss) = (
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6],
        );
        if !(100..=199).contains(&y) || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
            return Err(DateTimeError::InvalidEncoding);
        }
        if !(1..=24).contains(&hh) || !(1..=60).contains(&mm) || !(1..=60).contains(&ss) {
            return Err(DateTimeError::InvalidEncoding);
        }
        let year = (i32::from(c) - 100) * 100 + (i32::from(y) - 100);
        Self::new(year, m, d, hh - 1, mm - 1, ss - 1).map_err(|_| DateTimeError::InvalidEncoding)
    }

    /// 文本形态（`YYYY-MM-DD HH:MM:SS`；公元前年份带负号）。
    #[must_use]
    pub fn to_text(&self) -> String {
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_text())
    }
}

/// `TIMESTAMP` 的小数秒精度（0–9）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Precision(u8);

impl Precision {
    /// 构造（0–9）。
    pub fn new(p: u8) -> Result<Self, DateTimeError> {
        if p > 9 {
            return Err(DateTimeError::FieldOutOfRange);
        }
        Ok(Self(p))
    }

    /// 精度值。
    #[must_use]
    pub fn get(self) -> u8 {
        self.0
    }

    /// 该精度的编码长度（7 或 11）。
    #[must_use]
    pub fn encoded_len(self) -> usize {
        if self.0 == 0 {
            TIMESTAMP_LEN_P0
        } else {
            TIMESTAMP_LEN_P1_9
        }
    }
}

/// `TIMESTAMP`：`DATE` 7 字节 + 可选纳秒（精度 1–9）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timestamp {
    date: Date,
    nanos: u32,
    precision: Precision,
}

impl Timestamp {
    /// 构造；精度 0 时纳秒必须为 0，且纳秒必须能被声明精度**精确表示**
    /// （如精度 3 时 `nanos % 1_000_000 == 0`）——规范形式唯一。
    pub fn new(date: Date, nanos: u32, precision: Precision) -> Result<Self, DateTimeError> {
        if nanos >= 1_000_000_000 {
            return Err(DateTimeError::FieldOutOfRange);
        }
        let unit = 10u32.pow(9 - u32::from(precision.get()));
        if nanos % unit != 0 {
            return Err(DateTimeError::FieldOutOfRange);
        }
        Ok(Self {
            date,
            nanos,
            precision,
        })
    }

    /// 日期部分。
    #[must_use]
    pub fn date(&self) -> &Date {
        &self.date
    }

    /// 纳秒。
    #[must_use]
    pub fn nanos(&self) -> u32 {
        self.nanos
    }

    /// 声明精度。
    #[must_use]
    pub fn precision(&self) -> Precision {
        self.precision
    }

    /// 编码（长度由精度决定：7 或 11）。
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = self.date.encode().to_vec();
        if self.precision.get() > 0 {
            // 大端：4 字节字典序 = 数值序（排序兼容，见模块文档）。
            out.extend_from_slice(&self.nanos.to_be_bytes());
        }
        out
    }

    /// 由字节解码（长度必须与声明精度一致）。
    pub fn decode(bytes: &[u8], precision: Precision) -> Result<Self, DateTimeError> {
        if bytes.len() != precision.encoded_len() {
            return Err(DateTimeError::InvalidEncoding);
        }
        let date = Date::decode(&bytes[..DATE_LEN])?;
        let nanos = if precision.get() == 0 {
            0
        } else {
            u32::from_be_bytes(bytes[DATE_LEN..DATE_LEN + 4].try_into().expect("4 字节"))
        };
        Self::new(date, nanos, precision).map_err(|_| DateTimeError::InvalidEncoding)
    }

    /// 文本形态（含小数秒，按精度输出）。
    #[must_use]
    pub fn to_text(&self) -> String {
        if self.precision.get() == 0 {
            return self.date.to_text();
        }
        let full = format!("{:09}", self.nanos);
        format!(
            "{}.{}",
            self.date.to_text(),
            &full[..self.precision.get() as usize]
        )
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_text())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_known_vector_and_roundtrip() {
        // Oracle dump：DATE '2026-10-04 00:00:00' = 120,126,10,4,1,1,1
        let d = Date::new(2026, 10, 4, 0, 0, 0).unwrap();
        assert_eq!(d.encode(), [120, 126, 10, 4, 1, 1, 1]);
        assert_eq!(Date::decode(&d.encode()).unwrap(), d);
        assert_eq!(d.to_text(), "2026-10-04 00:00:00");
    }

    #[test]
    fn date_byte_order_is_time_order() {
        let mut dates = [
            Date::new(1969, 12, 31, 23, 59, 59).unwrap(),
            Date::new(1970, 1, 1, 0, 0, 0).unwrap(),
            Date::new(2026, 10, 4, 12, 30, 45).unwrap(),
            Date::new(2026, 10, 4, 12, 30, 46).unwrap(),
            Date::new(2026, 11, 1, 0, 0, 0).unwrap(),
        ];
        dates.sort_unstable();
        for w in dates.windows(2) {
            assert!(
                w[0].encode() < w[1].encode(),
                "字节序即时间序：{} vs {}",
                w[0],
                w[1]
            );
        }
    }

    #[test]
    fn bc_years_are_encodable() {
        // 公元前 1 年（year = -1）：世纪 = floor(-1/100) = -1 → 99；年 = 99 → 199。
        let d = Date::new(-1, 3, 15, 0, 0, 0).unwrap();
        assert_eq!(d.encode()[0], 99);
        assert_eq!(d.encode()[1], 199);
        assert_eq!(Date::decode(&d.encode()).unwrap(), d);
        // 编码仍保序：更早的年份字节更小。
        let earlier = Date::new(-100, 1, 1, 0, 0, 0).unwrap();
        assert!(earlier.encode() < d.encode());
    }

    #[test]
    fn date_field_ranges() {
        assert_eq!(
            Date::new(2026, 0, 1, 0, 0, 0),
            Err(DateTimeError::FieldOutOfRange)
        );
        assert_eq!(
            Date::new(2026, 13, 1, 0, 0, 0),
            Err(DateTimeError::FieldOutOfRange)
        );
        assert_eq!(
            Date::new(2026, 1, 32, 0, 0, 0),
            Err(DateTimeError::FieldOutOfRange)
        );
        assert_eq!(
            Date::new(2026, 1, 1, 24, 0, 0),
            Err(DateTimeError::FieldOutOfRange)
        );
        assert_eq!(
            Date::new(2026, 1, 1, 0, 60, 0),
            Err(DateTimeError::FieldOutOfRange)
        );
        assert_eq!(
            Date::new(2026, 1, 1, 0, 0, 60),
            Err(DateTimeError::FieldOutOfRange)
        );
        assert_eq!(
            Date::new(YEAR_MAX + 1, 1, 1, 0, 0, 0),
            Err(DateTimeError::FieldOutOfRange)
        );
        // 范围校验不做日历完备：2 月 30 日可存（与成熟产品一致）。
        assert!(Date::new(2026, 2, 30, 0, 0, 0).is_ok());
    }

    #[test]
    fn timestamp_precision_and_lengths() {
        let d = Date::new(2026, 10, 4, 12, 0, 0).unwrap();
        let p0 = Precision::new(0).unwrap();
        let ts0 = Timestamp::new(d, 0, p0).unwrap();
        assert_eq!(ts0.encode().len(), TIMESTAMP_LEN_P0);
        assert_eq!(ts0.to_text(), "2026-10-04 12:00:00");

        let p6 = Precision::new(6).unwrap();
        let ts6 = Timestamp::new(d, 123_456_000, p6).unwrap();
        assert_eq!(ts6.encode().len(), TIMESTAMP_LEN_P1_9);
        assert_eq!(ts6.to_text(), "2026-10-04 12:00:00.123456");
        assert_eq!(Timestamp::decode(&ts6.encode(), p6).unwrap(), ts6);

        let p3 = Precision::new(3).unwrap();
        let ts3 = Timestamp::new(d, 123_000_000, p3).unwrap();
        assert_eq!(ts3.to_text(), "2026-10-04 12:00:00.123");

        // 精度 0 时纳秒必须为 0；纳秒必须能被精度精确表示。
        assert!(Timestamp::new(d, 1, p0).is_err());
        assert!(Timestamp::new(d, 123_456_000, p3).is_err());
        assert_eq!(Precision::new(10), Err(DateTimeError::FieldOutOfRange));

        // 解码长度必须与声明精度一致。
        assert!(Timestamp::decode(&ts6.encode(), p0).is_err());
        assert!(Timestamp::decode(&ts0.encode(), p6).is_err());
    }

    #[test]
    fn timestamp_nanos_are_order_compatible() {
        let d = Date::new(2026, 10, 4, 12, 0, 0).unwrap();
        let p9 = Precision::new(9).unwrap();
        let a = Timestamp::new(d, 1, p9).unwrap();
        let b = Timestamp::new(d, 256, p9).unwrap(); // 0x100：小端会在此处失序
        let c = Timestamp::new(d, 999_999_999, p9).unwrap();
        assert!(a.encode() < b.encode());
        assert!(b.encode() < c.encode());
    }

    #[test]
    fn decode_rejects_malformed() {
        assert!(Date::decode(&[120, 126, 10, 4, 1, 1]).is_err(), "长度不足");
        assert!(
            Date::decode(&[120, 99, 10, 4, 1, 1, 1]).is_err(),
            "年字段偏移失效"
        );
        assert!(Date::decode(&[120, 126, 0, 4, 1, 1, 1]).is_err(), "月为 0");
        assert!(
            Date::decode(&[120, 126, 10, 4, 0, 1, 1]).is_err(),
            "时为 0（应为 hour+1）"
        );
    }
}
