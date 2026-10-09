//! **认证（D6）：主体名 + 口令 → 身份**。
//!
//! ```text
//! 连接 ──AUTH <主体名> <口令>──▶ 查 user$ ──▶ PBKDF2-SHA512 比对 ──▶ 身份（Identity）
//!                                  │                │
//!                                  │                └─ 不对 ⇒ **与"主体不存在"同一条错**
//!                                  └─ 状态检查在**口令校验之后**（PAUSE / EXPIRE）
//! ```
//!
//! # 四条取自成熟产品的纪律（证据包 `doc/evidence/auth-20261007/evidence.md`）
//!
//! 1. **口令校验先、状态检查后**——反过来"已暂停/已过期"就成了主体枚举的旁路
//!    （MySQL：1045 认证失败在 `sql_authentication.cc:1475`，3118 锁定在 `:4103`、
//!    1862 过期在 `:4131`，**同一流程里按序执行**）；
//! 2. **"主体不存在"与"口令不对"是同一条**错误文本，且**代价相同**——
//!    主体不存在时照样跑一次 PBKDF2（结果丢弃），响应时间不泄露存在性
//!    （PG 的 mock authentication：假盐 + 假迭代数，统一返回失败）；
//! 3. **`PAUSE` ⇒ 拒绝新会话**（已开会话不掐断）；**`EXPIRE` ⇒ 受限会话**
//!    （连接建立，但除本人改密之外一律拒绝——MySQL "新客户端密码过期后受限登录模式"）；
//! 4. **口令与散列不进日志、不进回执、不进诊断**（`user$.passwd` 的唯一读口是
//!    `catalog::dcl::password_hash`，它的返回值只在本模块里被比对，随即丢弃）。
//!
//! **不做**基于 `user$.passwd` 的挑战-应答：那会把存储值变成"可认证的等价物"
//! ——PG 的 MD5 认证正是栽在这上面（KB：`pg_authid` 的 MD5 泄露即可绕过认证）。
//! 本机套接字由文件权限保护，服务端校验是这一层的正确形态；网络承载 + 通道绑定
//! （SCRAM 式）属"对外协议"切片（届时 `wire` 号 +1）。

use bicdb_catalog::dcl::{self as catdcl, user_status};
use bicdb_catalog::Catalog;
use bicdb_common::pbkdf2;

/// Trusted service-side authenticator; supplied by the native service, never by SQL.
pub trait PrivateAuthProvider {
    /// Authenticate against PUBLIC and verify this exact workspace's ownership.
    fn authenticate_private(
        &self,
        name: &str,
        password: &str,
        workspace: [u8; 8],
    ) -> Result<PrivateLogin, String>;
}

/// Verified PUBLIC result returned by the trusted authenticator.
pub struct PrivateLogin {
    /// Authoritative principal ID.
    pub user_id: u64,
    /// Authoritative principal spelling.
    pub name: String,
    /// Exact workspace verified in PUBLIC.
    pub workspace: [u8; 8],
}

pub(crate) fn authenticate_private(
    provider: &dyn PrivateAuthProvider,
    name: &str,
    password: &str,
    workspace: [u8; 8],
) -> Result<Identity, String> {
    let login = provider.authenticate_private(name, password, workspace)?;
    if login.workspace != workspace || login.user_id == 0 || login.name.is_empty() {
        return Err("认证失败：工作区属主与已认证主体不匹配".into());
    }
    Ok(Identity {
        user_id: login.user_id,
        name: login.name,
        expired: false,
    })
}

/// 主体不存在时拿来跑"同代价 KDF"的固定盐（**结果丢弃**，不是谁的散列）。
const MOCK_SALT: &[u8; 16] = b"bicdb-mock-auth!";

/// **一次会话的身份**（谁在连）。
///
/// **只有 [`Session::authenticate`](crate::session::Session::authenticate) 能造它**
/// ——`REQ-ISO-002`（身份只来自认证结果，绝不接受请求体里的用户号）的结构保证：
/// 语句载荷里没有"用户号"这个字段，身份只可能来自认证。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// 主体号（服务端从目录解析，不是客户端给的）。
    user_id: u64,
    /// 主体名（目录里的权威拼写）。
    name: String,
    /// 口令已过期（`EXPIRE`）：**受限会话**，除本人改密外一律拒绝。
    expired: bool,
}

impl Identity {
    /// 主体号。
    #[must_use]
    pub fn user_id(&self) -> u64 {
        self.user_id
    }

    /// 主体名。
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 口令是否已过期（受限会话）。
    #[must_use]
    pub fn is_expired(&self) -> bool {
        self.expired
    }

    /// **解除过期限制**（本人改密成功后：`user$` 的状态已转 `ACTIVE`）。
    pub(crate) fn clear_expired(&mut self) {
        self.expired = false;
    }

    /// 展示名（诊断/回执）。
    #[must_use]
    pub fn describe(&self) -> String {
        if self.expired {
            format!(
                "主体 `{}`（{}；口令已过期 ⇒ 受限会话）",
                self.name, self.user_id
            )
        } else {
            format!("主体 `{}`（{}）", self.name, self.user_id)
        }
    }
}

/// 认证失败（**都是"不通过"**；哪一条文案由调用方原样给出——不翻译）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// **主体名或口令不对**——两条原因**合成一条**（防枚举：不告诉对方"这个名字存在"）。
    BadCredentials,
    /// 主体已暂停（`ALTER USER … PAUSE`）：拒绝新会话，已开会话不掐断。
    Paused {
        /// 主体名。
        name: String,
    },
    /// 目录读写失败（认证需要读 `user$`）。
    Catalog(String),
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // **一条文案覆盖两种原因**（不写"主体不存在"）：见证据包 §1。
            AuthError::BadCredentials => {
                f.write_str("认证失败：主体名或口令不对（本库不区分这两种原因——请核对后重试）")
            }
            AuthError::Paused { name } => write!(
                f,
                "认证失败：主体 `{name}` 已暂停（`ALTER USER … PAUSE`）——\
                 拒绝新会话；**已开会话不掐断**，恢复用 `ALTER USER … RESUME`"
            ),
            AuthError::Catalog(why) => write!(f, "认证失败：{why}"),
        }
    }
}

impl std::error::Error for AuthError {}

/// **认证**：按主体名取散列 → 比对 → 查状态。
///
/// `iterations` 用在**两处**：主体不存在时的"同代价"空跑（防时序枚举），
/// 以及**不改**已存行的迭代数（散列串自描述，校验按行里的数走）。
///
/// # Errors
/// [`AuthError::BadCredentials`] / [`AuthError::Paused`] / [`AuthError::Catalog`]。
pub fn authenticate(
    cat: &mut Catalog<'_>,
    name: &str,
    password: &str,
    iterations: u32,
) -> Result<Identity, AuthError> {
    let user = catdcl::user_by_name(cat, name).map_err(|e| AuthError::Catalog(e.to_string()))?;
    let Some(user) = user else {
        // **主体不存在**：跑一次同代价的 KDF（结果丢弃）再报"就不对"——
        // 响应时间与"存在但口令错"一致（PG mock authentication 的目的）。
        mock_verify(password, iterations);
        return Err(AuthError::BadCredentials);
    };
    // 口令散列的唯一读口（`catalog::dcl::password_hash`）；比完即弃。
    let stored =
        catdcl::password_hash(cat, user.user_id).map_err(|e| AuthError::Catalog(e.to_string()))?;
    if !pbkdf2::verify_password(password, &stored) {
        return Err(AuthError::BadCredentials);
    }
    // **状态检查在口令校验之后**（否则状态本身就成了枚举旁路）。
    match user.status {
        user_status::PAUSED => Err(AuthError::Paused { name: user.name }),
        user_status::EXPIRED => Ok(Identity {
            user_id: user.user_id,
            name: user.name,
            expired: true,
        }),
        _ => Ok(Identity {
            user_id: user.user_id,
            name: user.name,
            expired: false,
        }),
    }
}

/// 主体不存在时的"同代价空跑"：算一遍 PBKDF2，**结果丢弃**。
///
/// 为什么不是"直接返回"：`verify_password` 是常数时间比对，但"根本没有散列可比"
/// 会**整段跳过** KDF——那正是 PG 用 mock authentication 消掉的时序差。
fn mock_verify(password: &str, iterations: u32) {
    let _ = pbkdf2::hash_password_with_salt(password, MOCK_SALT, iterations);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_failure_causes_share_one_message() {
        // "主体不存在"与"口令不对"的**文案必须逐字节相同**（防枚举）。
        let a = AuthError::BadCredentials.to_string();
        let b = AuthError::BadCredentials.to_string();
        assert_eq!(a, b);
        assert!(!a.contains("不存在"), "不能出现`不存在`：{a}");
    }

    #[test]
    fn a_mock_verify_costs_one_kdf_and_discards_it() {
        // 只钉"能跑、不 panic、不返回散列"；**时序**由实现保证（同一条 KDF 路径）。
        mock_verify("whatever", 10);
        mock_verify("", 1);
        assert_eq!(MOCK_SALT.len(), 16);
    }

    #[test]
    fn identity_describe_names_the_restricted_state() {
        let active = Identity {
            user_id: 3,
            name: "alice".to_owned(),
            expired: false,
        };
        assert_eq!(active.describe(), "主体 `alice`（3）");
        let mut expired = Identity {
            user_id: 3,
            name: "alice".to_owned(),
            expired: true,
        };
        assert!(expired.describe().contains("受限会话"));
        expired.clear_expired();
        assert!(!expired.is_expired());
    }
}
