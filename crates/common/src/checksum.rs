//! 页校验和：**CRC32C**，覆盖全页，`checksum` 字段置零后计算
//! （存储架构 §5.3 描述区首字段；页 = 16 KiB）。
//!
//! - **页是完整性的边界**：记录头不带 CRC（§14 第 46 项已定），
//!   撕裂页由页校验和**检出**，检出后按损坏处理（`arch` §11.5.4）；
//! - **字节序**：磁盘格式定死**小端**（REQ-PRT-003）——校验和字段按
//!   小端读写；
//! - **实现与架构无关**：这里只有标量（查表）实现，编译期生成表；
//!   将来若加 SIMD/硬件加速，按 REQ-PRT-003 **只作加速路径、
//!   标量回退**，结果逐位一致。
//!
//! 正确性由**独立模型**把关（REQ-MNT-005）：测试里有一份逐位（bit-by-bit）
//! 参考实现与公开测试向量（"123456789" → `0xE3069283`，RFC 3720 / iSCSI）。

/// 页大小（16 KiB；存储架构——页、区与容量的全部换算基准）。
pub const PAGE_SIZE: usize = 16 * 1024;

/// 页内 `checksum` 字段偏移（描述区首字段）。
pub const PAGE_CHECKSUM_OFFSET: usize = 0;

/// 页内 `checksum` 字段宽度（字节）。
pub const PAGE_CHECKSUM_LEN: usize = 4;

/// CRC32C（Castagnoli）反射多项式。
const CRC32C_POLY_REFLECTED: u32 = 0x82F6_3B78;

const fn build_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ CRC32C_POLY_REFLECTED
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

static TABLE: [u32; 256] = build_table();

/// 增量 CRC32C（不复制数据也能对"中间挖空"的页求校验和）。
#[derive(Debug, Clone, Copy)]
pub struct Crc32c {
    state: u32,
}

impl Crc32c {
    /// 新计算器（初值 `!0`）。
    #[must_use]
    pub const fn new() -> Self {
        Self { state: !0 }
    }

    /// 追加一段数据。
    pub fn update(&mut self, data: &[u8]) {
        let mut crc = self.state;
        for &b in data {
            crc = (crc >> 8) ^ TABLE[((crc ^ u32::from(b)) & 0xff) as usize];
        }
        self.state = crc;
    }

    /// 结束并返回校验和（末取反）。
    #[must_use]
    pub const fn finish(self) -> u32 {
        !self.state
    }
}

impl Default for Crc32c {
    fn default() -> Self {
        Self::new()
    }
}

/// 一次性 CRC32C。
#[must_use]
pub fn crc32c(data: &[u8]) -> u32 {
    let mut c = Crc32c::new();
    c.update(data);
    c.finish()
}

/// 通用形态：**指定字段按零参与**、覆盖整块的校验和（不修改调用方数据）。
///
/// 数据页（16 KiB，字段在偏移 0）与 redo 页（512 B，字段同在偏移 0）
/// 共用同一套纪律（`arch` §5.11：redo 页"校验 4B 覆盖整页"）。
#[must_use]
pub fn checksum_with_zeroed_field(data: &[u8], field_offset: usize, field_len: usize) -> u32 {
    debug_assert!(field_offset + field_len <= data.len());
    let mut c = Crc32c::new();
    c.update(&data[..field_offset]);
    c.update(&vec![0u8; field_len]);
    c.update(&data[field_offset + field_len..]);
    c.finish()
}

/// 计算整页校验和：`checksum` 字段按**零**参与计算，页内容不被修改。
///
/// `page.len()` 必须等于 [`PAGE_SIZE`]（`debug` 下断言；发布形态下按调用约定）。
#[must_use]
pub fn page_checksum(page: &[u8]) -> u32 {
    debug_assert_eq!(page.len(), PAGE_SIZE, "页大小固定 16 KiB");
    checksum_with_zeroed_field(page, PAGE_CHECKSUM_OFFSET, PAGE_CHECKSUM_LEN)
}

/// 校验：字段现值与重算值一致（通用块形态；字段在 `field_offset`）。
#[must_use]
pub fn verify_zeroed_field_checksum(data: &[u8], field_offset: usize, field_len: usize) -> bool {
    let stored = u32::from_le_bytes(
        data[field_offset..field_offset + field_len]
            .try_into()
            .expect("4 字节字段"),
    );
    stored == checksum_with_zeroed_field(data, field_offset, field_len)
}

/// 把计算出的校验和写回 `checksum` 字段（小端），返回写入值。
pub fn set_page_checksum(page: &mut [u8]) -> u32 {
    let sum = page_checksum(page);
    page[PAGE_CHECKSUM_OFFSET..PAGE_CHECKSUM_OFFSET + PAGE_CHECKSUM_LEN]
        .copy_from_slice(&sum.to_le_bytes());
    sum
}

/// 校验：字段现值与重算值一致。
#[must_use]
pub fn verify_page_checksum(page: &[u8]) -> bool {
    let stored = u32::from_le_bytes(
        page[PAGE_CHECKSUM_OFFSET..PAGE_CHECKSUM_OFFSET + PAGE_CHECKSUM_LEN]
            .try_into()
            .expect("4 字节字段"),
    );
    stored == page_checksum(page)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn published_vectors() {
        // RFC 3720 (iSCSI) 附录 B.4 的著名向量。
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(b""), 0x0000_0000);
        assert_eq!(crc32c(&[0x00]), 0x527D_5351);
    }

    /// 独立模型（REQ-MNT-005）：逐位展开、无查表——与实现**不共享代码路径**。
    fn crc32c_naive(data: &[u8]) -> u32 {
        let mut crc: u32 = !0;
        for &byte in data {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ CRC32C_POLY_REFLECTED
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    #[test]
    fn table_matches_independent_naive_model() {
        // 确定性伪随机数据（简单 LCG，不引入依赖）。
        let mut state: u64 = 0x0123_4567_89AB_CDEF;
        let mut data = Vec::with_capacity(4096);
        for _ in 0..4096 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            data.push((state >> 33) as u8);
        }
        let expected = crc32c_naive(&data);
        assert_eq!(crc32c(&data), expected, "查表实现与逐位独立模型一致");
        // 同一模型也覆盖公开向量，避免模型自身写错时"自洽地错"。
        assert_eq!(crc32c_naive(b"123456789"), 0xE306_9283);
    }

    #[test]
    fn incremental_equals_oneshot() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i * 31) as u8).collect();
        let mut c = Crc32c::new();
        c.update(&data[..1]);
        c.update(&data[1..7]);
        c.update(&data[7..]);
        assert_eq!(c.finish(), crc32c(&data));
    }

    #[test]
    fn page_roundtrip_and_tamper_detection() {
        let mut page = vec![0u8; PAGE_SIZE];
        for (i, b) in page.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let sum = set_page_checksum(&mut page);
        assert!(verify_page_checksum(&page));
        // 校验和字段本身 ≠ 0（否则下面的"改字段"用例无意义）。
        assert_ne!(sum, 0);

        // 任意位置的篡改都能检出。
        for pos in [4usize, 1234, PAGE_SIZE - 1] {
            let mut tampered = page.clone();
            tampered[pos] ^= 0x01;
            assert!(!verify_page_checksum(&tampered), "偏移 {pos} 的篡改应检出");
        }
        // 只改 checksum 字段也能检出。
        let mut tampered = page.clone();
        tampered[PAGE_CHECKSUM_OFFSET] ^= 0xff;
        assert!(!verify_page_checksum(&tampered));

        // 计算不修改页内容：对未写回校验和的页计算两次结果一致。
        let blank = vec![7u8; PAGE_SIZE];
        let a = page_checksum(&blank);
        let b = page_checksum(&blank);
        assert_eq!(a, b);
        assert_eq!(&blank[..4], &[7, 7, 7, 7], "page_checksum 不得原地写");
    }
}
