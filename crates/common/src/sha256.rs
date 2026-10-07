//! **SHA-256**（FIPS 180-4）——本仓唯一的密码学哈希，**自己实现、无第三方依赖**。
//!
//! # 为什么在仓里实现（而不是引 crate）
//!
//! 用途只有一个：**`workspace_ref = H(workspace_id)` 截断至 8 字节**
//! （`doc/workspace_ref设计_v0.1.md` §3.1：`H` 取 SHA-256 或 BLAKE3，**P0 冻结具体算法**；
//! 本文件即冻结落点：**SHA-256，取前 8 字节**）。这个用途是**一致性检查**，
//! 不是密码学边界（同文档 §"威胁模型"）——所以选型的判据是
//! **可验证、可复现、无依赖**，而不是速度：
//!
//! - **有官方测试向量**（FIPS 180-4 附录 + 长消息向量），实现的对错可被独立复核；
//! - 不引第三方 crate ⇒ 构建可复现、审计面小（与"第三方依赖最小化"的既有纪律一致）；
//! - 只在**打开工作区时算一次**并缓存（每页访问只是一次 8 字节比较）——
//!   性能不在关键路径上。
//!
//! **它不是密钥**：不用于加密、不用于认证、不用于完整性签名（页校验另有 CRC32C）。

/// 摘要长度（字节）。
pub const DIGEST_LEN: usize = 32;
/// 分组长度（字节）。
pub const BLOCK_LEN: usize = 64;

/// 256 位摘要。
pub type Digest = [u8; DIGEST_LEN];

const K: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

const H0: [u32; 8] = [
    0x6a09_e667,
    0xbb67_ae85,
    0x3c6e_f372,
    0xa54f_f53a,
    0x510e_527f,
    0x9b05_688c,
    0x1f83_d9ab,
    0x5be0_cd19,
];

/// **流式哈希**（`update` 多次 + `finalize` 一次；纯函数式用法见 [`digest`]）。
#[derive(Debug, Clone)]
pub struct Sha256 {
    state: [u32; 8],
    buf: [u8; BLOCK_LEN],
    buf_len: usize,
    total_len: u64,
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256 {
    /// 新建。
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: H0,
            buf: [0u8; BLOCK_LEN],
            buf_len: 0,
            total_len: 0,
        }
    }

    /// 追加数据。
    pub fn update(&mut self, mut data: &[u8]) {
        self.total_len = self.total_len.wrapping_add(data.len() as u64);
        if self.buf_len > 0 {
            let need = BLOCK_LEN - self.buf_len;
            let take = need.min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len == BLOCK_LEN {
                let block = self.buf;
                self.compress(&block);
                self.buf_len = 0;
            }
        }
        while data.len() >= BLOCK_LEN {
            let mut block = [0u8; BLOCK_LEN];
            block.copy_from_slice(&data[..BLOCK_LEN]);
            self.compress(&block);
            data = &data[BLOCK_LEN..];
        }
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buf_len = data.len();
        }
    }

    /// 收尾并输出摘要（消费 `self`）。
    #[must_use]
    pub fn finalize(mut self) -> Digest {
        let bit_len = self.total_len.wrapping_mul(8);
        // 填充：0x80 + 若干 0 + 64 位长度。
        let mut pad = [0u8; BLOCK_LEN * 2];
        pad[0] = 0x80;
        let pad_len = if self.buf_len < 56 {
            56 - self.buf_len
        } else {
            56 + BLOCK_LEN - self.buf_len
        };
        // 最后 8 字节 = 大端长度（放在 padding 之后的分组尾巴上）。
        let mut tail = [0u8; BLOCK_LEN * 2];
        let total = self.buf_len + pad_len;
        tail[..self.buf_len].copy_from_slice(&self.buf[..self.buf_len]);
        tail[self.buf_len] = 0x80;
        tail[total..total + 8].copy_from_slice(&bit_len.to_be_bytes());
        let _ = pad;
        let mut off = 0;
        while off < total + 8 {
            let mut block = [0u8; BLOCK_LEN];
            block.copy_from_slice(&tail[off..off + BLOCK_LEN]);
            self.compress(&block);
            off += BLOCK_LEN;
        }
        let mut out = [0u8; DIGEST_LEN];
        for (i, word) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }

    fn compress(&mut self, block: &[u8; BLOCK_LEN]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        let add = [a, b, c, d, e, f, g, h];
        for (i, v) in add.iter().enumerate() {
            self.state[i] = self.state[i].wrapping_add(*v);
        }
    }
}

/// 一次算完（便利函数）。
#[must_use]
pub fn digest(data: &[u8]) -> Digest {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(d: &Digest) -> String {
        d.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn fips_vectors() {
        // FIPS 180-4 的标准测试向量（含空串与多分组）。
        assert_eq!(
            hex(&digest(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&digest(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&digest(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        assert_eq!(
            hex(&digest(b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu")),
            "cf5b16a778af8380036ce59e7b0492370b249b11e8f07a51afac45037afee9d1"
        );
    }

    #[test]
    fn streaming_matches_one_shot_and_handles_block_boundaries() {
        // 1 MiB 的 'a'（官方长向量只给 1e6 个 a 的摘要；这里用自洽性：
        // 分块喂与一次喂必须一致，且跨 64B 边界不丢字节）。
        let data = vec![b'a'; 1_000_000];
        let one = digest(&data);
        for chunk in [1usize, 3, 7, 63, 64, 65, 127, 128, 1000] {
            let mut h = Sha256::new();
            for part in data.chunks(chunk) {
                h.update(part);
            }
            assert_eq!(h.finalize(), one, "分块 {chunk} 与一次喂不一致");
        }
        // 官方长向量：10^6 个 'a'
        assert_eq!(
            hex(&one),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn update_after_empty_update_is_fine() {
        let mut h = Sha256::new();
        h.update(b"");
        h.update(b"ab");
        h.update(b"");
        h.update(b"c");
        assert_eq!(
            hex(&h.finalize()),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
