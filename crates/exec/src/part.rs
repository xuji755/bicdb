//! **哈希分区共用件**（设计 §4.2.1 ③）：确定性哈希 + 分区数/深度策略。
//!
//! 供 `HashAgg` 与 `HashJoin` 复用；SORT 与 HASH 共用同一 [`crate::spill::SpillSpace`]，
//! 本模块只管"怎么分"（怎么落/怎么读回在 spill 层）。

use crate::error::ExecError;
use crate::value::{row_bytes, Value};

/// **一级分区数**（无统计信息下的稳妥默认；代价模型到位后由计划侧给）。
pub const PARTITIONS_LEVEL1: usize = 16;
/// 分区数上限（一层内的桶数）。
pub const MAX_PARTITIONS: usize = 64;
/// **深度上限**：超限 = 极端键偏斜（哈希怎么分都挤在一个桶）⇒ 按当前额度
/// 继续并计 `multi-pass`（设计 §4.2.1 ③）。
pub const MAX_PARTITION_DEPTH: u32 = 3;

/// **FNV-1a 64 位**，值走**规范编码**（同键必同哈希；确定性、不依赖进程
/// 随机种子——分区函数只要求单次执行内自洽，确定是为了复现与用例断言）。
pub fn hash_key(seed: u64, values: &[Value]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = OFFSET ^ seed;
    for v in values {
        match v {
            Value::Null => mix(&mut h, &[0], PRIME),
            Value::Bool(b) => mix(&mut h, &[1, u8::from(*b)], PRIME),
            Value::Number(n) => {
                let bytes = n.encode();
                mix(&mut h, &[2], PRIME);
                mix(&mut h, &bytes, PRIME);
            }
            Value::Bytes(b) => {
                mix(&mut h, &[3], PRIME);
                mix(&mut h, &(b.len() as u64).to_le_bytes(), PRIME);
                mix(&mut h, b, PRIME);
            }
        }
    }
    h
}

fn mix(h: &mut u64, bytes: &[u8], prime: u64) {
    for &b in bytes {
        *h ^= u64::from(b);
        *h = h.wrapping_mul(prime);
    }
}

/// **一个桶的行字节**（分区缓冲记账口径）。
#[must_use]
pub fn bucket_row_bytes(row: &crate::value::Row) -> u64 {
    row_bytes(row) as u64 + 32
}

/// **分区数**：`clamp(next_pow2(2 × used / budget), 2, 64)`（设计 §4.2.1 ③）。
#[must_use]
pub fn parts_for(used: u64, budget: u64) -> usize {
    let budget = budget.max(1);
    let want = used.saturating_mul(2).saturating_div(budget).max(1);
    let n = want.checked_next_power_of_two().unwrap_or(u64::MAX);
    (n as usize).clamp(2, MAX_PARTITIONS)
}

/// **第 `depth` 层的哈希种子**（同层自洽即可；换层换种子）。
#[must_use]
pub fn seed_for(depth: u32) -> u64 {
    0x9e37_79b9_7f4a_7c15u64.wrapping_mul(u64::from(depth) + 1)
}

/// 桶号（`hash % parts`）。
#[must_use]
pub fn bucket_of(seed: u64, parts: usize, values: &[Value]) -> usize {
    (hash_key(seed, values) % parts as u64) as usize
}

/// **分区缓冲**（落 temp 前的按桶缓存；设计 §4.2.1 ③）。
///
/// 刷盘策略 = **刷最大桶**（总量超额度时写掉当前最大的一个桶，一次一个 run）：
/// 缓冲总量 ≤ 额度 + 一行，且小额度下不会"每次刷全部桶"产生海量小 run
/// （每 run 至少一页——temp 页是 16 KiB，小 run 浪费的是整个页）。
pub struct BucketBuffer {
    seed: u64,
    parts: usize,
    budget: u64,
    buckets: Vec<Vec<crate::value::Row>>,
    bytes: Vec<u64>,
    total: u64,
}

impl BucketBuffer {
    /// 新建（`budget` = 该次分区可用的工作内存额度）。
    #[must_use]
    pub fn new(seed: u64, parts: usize, budget: u64) -> Self {
        Self {
            seed,
            parts,
            budget,
            buckets: (0..parts).map(|_| Vec::new()).collect(),
            bytes: vec![0; parts],
            total: 0,
        }
    }

    /// 当前缓冲总量（字节）。
    #[must_use]
    pub fn total(&self) -> u64 {
        self.total
    }

    /// 收一行（按 `key` 的哈希入桶；超额度即刷最大桶）。
    pub fn push(
        &mut self,
        key: &[Value],
        row: crate::value::Row,
        space: &crate::spill::SpillSpace<'_>,
        runs: &mut [Vec<usize>],
    ) -> Result<(), ExecError> {
        let p = bucket_of(self.seed, self.parts, key);
        let b = bucket_row_bytes(&row);
        self.bytes[p] += b;
        self.total += b;
        self.buckets[p].push(row);
        if self.total > self.budget {
            self.flush_largest(space, runs)?;
        }
        Ok(())
    }

    /// 收尾：所有非空桶落 run。
    pub fn flush_all(
        &mut self,
        space: &crate::spill::SpillSpace<'_>,
        runs: &mut [Vec<usize>],
    ) -> Result<(), ExecError> {
        for p in 0..self.parts {
            if !self.buckets[p].is_empty() {
                self.flush_bucket(p, space, runs)?;
            }
        }
        Ok(())
    }

    /// 刷当前最大的桶（同量取小桶号——确定）。
    fn flush_largest(
        &mut self,
        space: &crate::spill::SpillSpace<'_>,
        runs: &mut [Vec<usize>],
    ) -> Result<(), ExecError> {
        let mut pick = 0usize;
        let mut best = 0u64;
        for p in 0..self.parts {
            if self.bytes[p] > best {
                best = self.bytes[p];
                pick = p;
            }
        }
        if best == 0 {
            return Ok(());
        }
        self.flush_bucket(pick, space, runs)
    }

    fn flush_bucket(
        &mut self,
        p: usize,
        space: &crate::spill::SpillSpace<'_>,
        runs: &mut [Vec<usize>],
    ) -> Result<(), ExecError> {
        let rows = std::mem::take(&mut self.buckets[p]);
        self.total -= self.bytes[p];
        self.bytes[p] = 0;
        runs[p].push(space.write_run(&rows)?);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bicdb_types::Number;

    #[test]
    fn equal_keys_hash_equally_and_seeds_differ() {
        let a = vec![
            Value::Number(Number::parse("1.50").unwrap()),
            Value::Bytes(b"x".to_vec()),
        ];
        let b = vec![
            Value::Number(Number::parse("1.5").unwrap()), // 同值不同写法 ⇒ 规范编码相同
            Value::Bytes(b"x".to_vec()),
        ];
        assert_eq!(
            hash_key(0, &a),
            hash_key(0, &b),
            "等值 ⇒ 等哈希（规范编码）"
        );
        assert_ne!(hash_key(0, &a), hash_key(7, &a), "换种子换哈希");
        // NULL 与空字节串不同哈希（标记区分）。
        assert_ne!(
            hash_key(0, &[Value::Null]),
            hash_key(0, &[Value::Bytes(Vec::new())])
        );
    }

    #[test]
    fn parts_policy_is_bounded_and_monotone() {
        assert_eq!(parts_for(0, 1024), 2);
        assert_eq!(parts_for(512, 1024), 2); // 2×512/1024 = 1 → next_pow2 = 1 → 下限 2
        assert_eq!(parts_for(1024, 1024), 2);
        assert_eq!(parts_for(3 * 1024, 1024), 8); // 2×3 = 6 → 8
        assert_eq!(parts_for(u64::MAX, 1), MAX_PARTITIONS, "上限 64");
        assert!(parts_for(1 << 20, 1) <= MAX_PARTITIONS);
    }
}
