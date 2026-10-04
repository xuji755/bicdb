//! 身份：`AuthenticatedSubject`——路由的唯一身份来源。
//!
//! `ISO` REQ-ISO-002：**认证发生在引擎之外**（平台 / API 层），本库**不做认证**，
//! 它**消费**认证结果；**路由是身份的函数，不是请求参数的函数**。
//!
//! 这个类型的用途就是把上面那句话变成签名事实：需要身份的地方收的是
//! `&AuthenticatedSubject`，**没有任何接口接受"请求里的 user_id"**——
//! 伪造 `user_id` 的请求在类型层面无处可传。

use crate::id::UserId;

/// 已认证主体（不可变；路由的唯一身份来源）。
///
/// **只应由接入层在认证成功后构造**——引擎不认证（`ISO` REQ-ISO-002）。
/// 引擎内所有需要身份的位置只接收本类型，不接收裸 `UserId`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthenticatedSubject {
    user: UserId,
}

impl AuthenticatedSubject {
    /// 由认证结果构造。
    ///
    /// **调用方约定**：只有接入层（认证已经发生并成功）才能调用；
    /// 引擎内部不得以任何请求字段为输入构造本类型。
    #[must_use]
    pub fn new(verified_user: UserId) -> Self {
        Self {
            user: verified_user,
        }
    }

    /// 已认证的主体标识。
    #[must_use]
    pub fn user(&self) -> UserId {
        self.user
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subject_is_a_copyable_proof_of_authentication() {
        let u = UserId::parse("018f2a7c-3b4d-7e01-9a2b-c3d4e5f60718").unwrap();
        let s = AuthenticatedSubject::new(u);
        assert_eq!(s.user(), u);
        let s2 = s; // Copy：可作为纯值传递，无需共享可变状态
        assert_eq!(s2, s);
    }
}
