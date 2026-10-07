//! **PBKDF2-HMAC-SHA512**（RFC 8018 §5.2）——本库的**口令散列**。
//!
//! # 为什么是它
//!
//! `doc/用户与配额管理设计_v0.1.md` §5.1 的冻结项：**PBKDF2-SHA512**
//! （对齐 Oracle 12c+ 的口令散列口径）；本模块是它的落点。
//!
//! # 存储形态（**自描述**，将来换参数不必猜）
//!
//! ```text
//! pbkdf2-sha512$<迭代数>$<盐 base64-no-pad>$<摘要 base64-no-pad>
//! ```
//!
//! - **盐**：16 字节，**每个口令一份**（`/dev/urandom`，退化为时间×pid 混合）；
//! - **迭代数**：写进字符串——升级默认值不影响旧行（校验时按行里的数走）；
//! - **摘要**：64 字节（SHA-512 输出）；
//! - **base64 不含填充**（`=`）——散列串要进 `user$.passwd`（`VARCHAR2(256)`），
//!   去掉填充省 2 字节且无歧义。
//!
//! # 三条纪律
//!
//! 1. **不可逆**：设计明文写着"找回 = 重置为新口令，不是取回原文"——
//!    本模块**只提供 [`hash_password`] 与 [`verify_password`]**，没有"解密"口；
//! 2. **比对是常数时间**（[`verify_password`] 用全量异或累加，不短路）；
//! 3. **口令串与旧散列都不回显、不进日志**（调用方的责任，这里只提醒）。

use crate::sha512::{self, BLOCK_LEN, DIGEST_LEN};

/// 算法标签（存储串的第一段；换算法时靠它分流）。
pub const ALGORITHM: &str = "pbkdf2-sha512";
/// 盐长度（字节）。
pub const SALT_LEN: usize = 16;
/// 默认迭代数（OWASP 2023 对 PBKDF2-HMAC-SHA512 的建议量级；**写进存储串**，
/// 所以以后调大不破旧行）。参数化入口在实例参数（`[auth] pbkdf2_iterations`）。
pub const DEFAULT_ITERATIONS: u32 = 210_000;

/// 口令散列错误（**都是"不能用"，不是"算错了"**）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pbkdf2Error {
    /// 存储串形态不认识（段数、标签、base64、长度）。
    Malformed {
        /// 说明。
        what: String,
    },
    /// 迭代数为 0（RFC 8018 要求 ≥ 1）。
    ZeroIterations,
}

impl std::fmt::Display for Pbkdf2Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Pbkdf2Error::Malformed { what } => write!(f, "口令散列串非法：{what}"),
            Pbkdf2Error::ZeroIterations => f.write_str("PBKDF2 迭代数不得为 0"),
        }
    }
}

impl std::error::Error for Pbkdf2Error {}

/// HMAC-SHA512（RFC 2104）。
fn hmac_sha512(key: &[u8], msg: &[u8]) -> [u8; DIGEST_LEN] {
    // 键过长先哈希；不足则右补零到分组长度。
    let mut block = [0u8; BLOCK_LEN];
    if key.len() > BLOCK_LEN {
        block[..DIGEST_LEN].copy_from_slice(&sha512::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK_LEN];
    let mut opad = [0x5cu8; BLOCK_LEN];
    for i in 0..BLOCK_LEN {
        ipad[i] ^= block[i];
        opad[i] ^= block[i];
    }
    let mut inner = sha512::Sha512::new();
    inner.update(&ipad);
    inner.update(msg);
    let inner = inner.finalize();
    let mut outer = sha512::Sha512::new();
    outer.update(&opad);
    outer.update(&inner);
    outer.finalize()
}

/// **PBKDF2-HMAC-SHA512**（输出 `DIGEST_LEN` 字节 = 一个分组，够用且不必拼接）。
///
/// # Errors
/// 迭代数为 0。
pub fn pbkdf2_sha512(
    password: &[u8],
    salt: &[u8],
    iterations: u32,
) -> Result<[u8; DIGEST_LEN], Pbkdf2Error> {
    if iterations == 0 {
        return Err(Pbkdf2Error::ZeroIterations);
    }
    // U1 = PRF(P, S || INT(1))；此后 T_i 迭代异或。
    let mut msg = Vec::with_capacity(salt.len() + 4);
    msg.extend_from_slice(salt);
    msg.extend_from_slice(&1u32.to_be_bytes());
    let mut u = hmac_sha512(password, &msg);
    let mut t = u;
    for _ in 1..iterations {
        u = hmac_sha512(password, &u);
        for i in 0..DIGEST_LEN {
            t[i] ^= u[i];
        }
    }
    Ok(t)
}

/// **算一个口令的存储串**（新的盐、按给定迭代数）。
///
/// # Errors
/// 迭代数为 0。
pub fn hash_password(password: &str, iterations: u32) -> Result<String, Pbkdf2Error> {
    let salt = random_salt();
    hash_password_with_salt(password, &salt, iterations)
}

/// 同上，但盐由调用方给（测试要确定性）。
///
/// # Errors
/// 迭代数为 0。
pub fn hash_password_with_salt(
    password: &str,
    salt: &[u8],
    iterations: u32,
) -> Result<String, Pbkdf2Error> {
    let dk = pbkdf2_sha512(password.as_bytes(), salt, iterations)?;
    Ok(format!(
        "{ALGORITHM}${iterations}${}${}",
        b64(salt),
        b64(&dk)
    ))
}

/// **校验口令**（常数时间比对；形态非法 ⇒ `Ok(false)`——不给攻击者形态探测）。
#[must_use]
pub fn verify_password(password: &str, stored: &str) -> bool {
    match parse(stored) {
        Ok((iterations, salt, expected)) => {
            let Ok(dk) = pbkdf2_sha512(password.as_bytes(), &salt, iterations) else {
                return false;
            };
            // 常数时间：逐字节异或累加，不短路（长度不符直接 false——长度不是秘密）。
            if dk.len() != expected.len() {
                return false;
            }
            let mut diff = 0u8;
            for (a, b) in dk.iter().zip(expected.iter()) {
                diff |= a ^ b;
            }
            diff == 0
        }
        Err(_) => false,
    }
}

/// 解析存储串 ⇒ `(迭代数, 盐, 摘要)`。
///
/// # Errors
/// 形态非法（段数/标签/base64/长度/迭代数 0）。
pub fn parse(stored: &str) -> Result<(u32, Vec<u8>, Vec<u8>), Pbkdf2Error> {
    let mut parts = stored.split('$');
    let (alg, iters, salt, dk) = (parts.next(), parts.next(), parts.next(), parts.next());
    if parts.next().is_some() {
        return Err(Pbkdf2Error::Malformed {
            what: "段数多于 4".to_owned(),
        });
    }
    let (Some(alg), Some(iters), Some(salt), Some(dk)) = (alg, iters, salt, dk) else {
        return Err(Pbkdf2Error::Malformed {
            what: "段数少于 4".to_owned(),
        });
    };
    if alg != ALGORITHM {
        return Err(Pbkdf2Error::Malformed {
            what: format!("算法 `{alg}` 不是 {ALGORITHM}"),
        });
    }
    let iterations: u32 = iters.parse().map_err(|_| Pbkdf2Error::Malformed {
        what: format!("迭代数不是整数：`{iters}`"),
    })?;
    if iterations == 0 {
        return Err(Pbkdf2Error::ZeroIterations);
    }
    let salt = unb64(salt).ok_or_else(|| Pbkdf2Error::Malformed {
        what: "盐不是合法 base64".to_owned(),
    })?;
    let dk = unb64(dk).ok_or_else(|| Pbkdf2Error::Malformed {
        what: "摘要不是合法 base64".to_owned(),
    })?;
    if dk.len() != DIGEST_LEN {
        return Err(Pbkdf2Error::Malformed {
            what: format!("摘要长度 {} ≠ {DIGEST_LEN}", dk.len()),
        });
    }
    Ok((iterations, salt, dk))
}

/// 盐的随机源（`/dev/urandom`；不可用则退化为"时间 × pid"——**仅影响唯一性，
/// 不是密钥强度**）。
#[must_use]
pub fn random_salt() -> [u8; SALT_LEN] {
    use std::io::Read;
    let mut out = [0u8; SALT_LEN];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        if f.read_exact(&mut out).is_ok() {
            return out;
        }
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let pid = u128::from(std::process::id());
    let mix = now.rotate_left(41) ^ pid.wrapping_mul(0x9E37_79B9_7F4A_7C15_1B87_35DF);
    out.copy_from_slice(&mix.to_le_bytes());
    out
}

// ───────────────────────── base64（RFC 4648，**无填充**）─────────────────────────

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// 编码（去填充）。
#[must_use]
pub fn b64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(B64[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(B64[n as usize & 63] as char);
        }
    }
    out
}

/// 解码（接受去填充形态）。
#[must_use]
pub fn unb64(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for &c in bytes {
        let v = B64.iter().position(|&x| x == c)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    // 余下 bits < 8 且必须为 0（否则不是合法编码）。
    if bits >= 6 || (acc & ((1 << bits) - 1)) != 0 {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **RFC 8018 的标准测试向量**（`pbkdf2_hmac('sha512', …)`；
    /// 期望值由 Python `hashlib.pbkdf2_hmac` 独立算出并逐条核对）。
    #[test]
    fn rfc_style_vectors_match_an_independent_implementation() {
        let cases: &[(&[u8], &[u8], u32, &str)] = &[
            (
                b"password",
                b"salt",
                1,
                "867f70cf1ade02cff3752599a3a53dc4af34c7a669815ae5d513554e1c8cf252\
                 c02d470a285a0501bad999bfe943c08f050235d7d68b1da55e63f73b60a57fce",
            ),
            (
                b"password",
                b"salt",
                2,
                "e1d9c16aa681708a45f5c7c4e215ceb66e011a2e9f0040713f18aefdb866d53c\
                 f76cab2868a39b9f7840edce4fef5a82be67335c77a6068e04112754f27ccf4e",
            ),
            (
                b"passwordPASSWORDpassword",
                b"saltSALTsaltSALTsaltSALTsaltSALTsalt",
                4096,
                // 长口令 + 长盐 + 4096 轮（`hashlib.pbkdf2_hmac('sha512', …)` 核对）
                "8c0511f4c6e597c6ac6315d8f0362e225f3c501495ba23b868c005174dc4ee71\
                 115b59f9e60cd9532fa33e0f75aefe30225c583a186cd82bd4daea9724a3d3b8",
            ),
        ];
        for (p, s, iters, want) in cases {
            let dk = pbkdf2_sha512(p, s, *iters).expect("算");
            let got: String = dk.iter().map(|b| format!("{b:02x}")).collect();
            assert_eq!(got, *want, "PBKDF2({p:?}, {s:?}, {iters})");
        }
    }

    #[test]
    fn hash_and_verify_round_trip() {
        let stored = hash_password("s3cr3t-口令", 1000).expect("算");
        assert!(stored.starts_with("pbkdf2-sha512$1000$"));
        assert!(verify_password("s3cr3t-口令", &stored), "对的口令通过");
        assert!(!verify_password("wrong", &stored), "错的口令不通过");
        assert!(!verify_password("", &stored));
        // 形态非法一律 false（不给形态探测）。
        assert!(!verify_password("x", ""));
        assert!(!verify_password("x", "bcrypt$1$a$b"));
        assert!(!verify_password("x", "pbkdf2-sha512$0$AAAA$AAAA"));
        assert!(!verify_password("x", "pbkdf2-sha512$1$!!!!$AAAA"));
    }

    #[test]
    fn salt_is_per_hash_and_iterations_are_self_describing() {
        let a = hash_password("same", 100).unwrap();
        let b = hash_password("same", 100).unwrap();
        assert_ne!(a, b, "同一口令两次的散列不同（盐不同）");
        assert!(verify_password("same", &a) && verify_password("same", &b));
        // 迭代数写进串：升级默认值不影响旧行。
        let old = hash_password_with_salt("same", b"0123456789abcdef", 50).unwrap();
        assert!(old.contains("$50$"));
        assert!(verify_password("same", &old));
        let (iters, salt, dk) = parse(&old).unwrap();
        assert_eq!(iters, 50);
        assert_eq!(salt, b"0123456789abcdef");
        assert_eq!(dk.len(), DIGEST_LEN);
    }

    #[test]
    fn base64_round_trip_without_padding() {
        for len in 0..40usize {
            let data: Vec<u8> = (0..len).map(|i| (i * 37 % 256) as u8).collect();
            let enc = b64(&data);
            assert!(!enc.contains('='), "无填充：{enc}");
            assert_eq!(unb64(&enc).as_deref(), Some(data.as_slice()), "len={len}");
        }
        assert_eq!(b64(b"f"), "Zg");
        assert_eq!(b64(b"fo"), "Zm8");
        assert_eq!(b64(b"foo"), "Zm9v");
        assert_eq!(b64(b"foob"), "Zm9vYg");
        assert_eq!(b64(b"fooba"), "Zm9vYmE");
        assert_eq!(b64(b"foobar"), "Zm9vYmFy");
    }
}
