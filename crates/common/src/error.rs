//! 统一错误模型（`CONV` §4；协议字段语义见 `API` §0.4）。
//!
//! 三个要点，全部来自设计原文：
//!
//! 1. **错误码形状**：`BIC-<五位数字>`，常用码与 Oracle 对齐（同语义同数字），
//!    对齐表之外的**自 `BIC-10000` 起顺序分配**；**只增不改、不复用**
//!    （错误码的历史即兼容承诺）。
//! 2. **四类必须可区分**（`CONV` §4.3，`API` §0.4 的字段判定表）：
//!    可重试冲突 / 资源拒绝 / 已中止事务 / 提交结果未知——
//!    **仅凭字段组合判定**，不做字符串匹配。本模块把这张表做成
//!    [`EngineError::advice`]：**字段是唯一事实源**，类别是导出量
//!    （不设可与之矛盾的冗余 `class` 字段）。
//! 3. **`message` 不得包含私有数据内容**（REQ-OPS-002）——纪律，
//!    由构造方遵守，见 [`EngineError`] 文档。

use std::fmt;

/// 稳定错误码（`BIC-<五位数字>`；`CONV` §4.1，D-02 已定）。
///
/// 九个与 Oracle 对齐的常用码在此冻结；对齐之外**自
/// [`BicCode::ALLOCATION_FLOOR`] 起顺序分配**——新增码在本类型上加常量，
/// **只增不改、不复用**。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BicCode(u32);

impl BicCode {
    /// `BIC-00001` 唯一约束冲突（ORA-00001）。
    pub const UNIQUE_VIOLATION: Self = Self(1);
    /// `BIC-00054` 资源忙：锁不可得 / 等待超时（ORA-00054）。
    pub const RESOURCE_BUSY: Self = Self(54);
    /// `BIC-00060` 死锁检出，**语句级**回滚（ORA-00060；`TXN` D-03）。
    pub const DEADLOCK_DETECTED: Self = Self(60);
    /// `BIC-00904` 无效标识符：列 / 名字不存在（ORA-00904）。
    pub const INVALID_IDENTIFIER: Self = Self(904);
    /// `BIC-00942` 表（对象）不存在（ORA-00942）。
    pub const OBJECT_NOT_FOUND: Self = Self(942);
    /// `BIC-01400` 不得插入 NULL（ORA-01400）。
    pub const NULL_NOT_ALLOWED: Self = Self(1400);
    /// `BIC-01555` 快照过旧（ORA-01555）。
    pub const SNAPSHOT_TOO_OLD: Self = Self(15555);
    /// `BIC-01722` 无效数字（ORA-01722）。
    pub const INVALID_NUMBER: Self = Self(17222);
    /// `BIC-02292` 存在引用，删除被拒（ORA-02292）。
    pub const REFERENCED_OBJECT_EXISTS: Self = Self(2292);

    /// 自分配区间起点：`BIC-10000` 起顺序分配（对齐表之外的码）。
    pub const ALLOCATION_FLOOR: u32 = 10_000;

    /// 由数字段构造（合法域：1..=99999——五位数字段）。
    #[must_use]
    pub fn from_raw(raw: u32) -> Option<Self> {
        if (1..=99_999).contains(&raw) {
            Some(Self(raw))
        } else {
            None
        }
    }

    /// 数字段。
    #[must_use]
    pub fn as_raw(self) -> u32 {
        self.0
    }

    /// 文本形态（`BIC-00001`）。
    #[must_use]
    pub fn to_code_string(self) -> String {
        format!("BIC-{:05}", self.0)
    }
}

impl fmt::Display for BicCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BIC-{:05}", self.0)
    }
}

/// 事务状态（协议取值：`none` / `active` / `aborted` / `unknown`；
/// `CONV` §4.2、`API` §0.4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TxnStatus {
    /// 不在事务中。
    None,
    /// 在事务中（事务仍可用）。
    Active,
    /// 事务已被回滚（连接仍可用；**必须新开事务**）。
    Aborted,
    /// **提交结果未知**（消歧靠协议对账，`API` REQ-API-014）。
    Unknown,
}

impl TxnStatus {
    /// 协议文本（小写取值）。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            TxnStatus::None => "none",
            TxnStatus::Active => "active",
            TxnStatus::Aborted => "aborted",
            TxnStatus::Unknown => "unknown",
        }
    }
}

impl fmt::Display for TxnStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// **仅凭字段组合**判定的重试类别（`CONV` §4.3 / `API` §0.4 的判定表）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RetryAdvice {
    /// 可重试冲突：死锁牺牲、CAS 失败、序列化冲突——可自动重试（幂等操作）。
    RetryableConflict,
    /// 资源拒绝：配额耗尽、并发超限——等待（`retry_after_ms`）后重试。
    ResourceLimit,
    /// 已中止事务：**必须新开事务**，不得重试原事务。
    TransactionAborted,
    /// 提交结果未知：**不得盲目重试**——用协议对账（`sync` / `status`）消歧。
    CommitOutcomeUnknown,
    /// 其余错误：不可重试（也非上述四类）。
    NoRetry,
}

/// 引擎错误（`CONV` §4.2 的字段）。
///
/// **`message` 不得包含私有数据内容**（REQ-OPS-002）——调用方纪律：
/// 只写"什么错了"，不写"内容是什么"（列值、口令、资产字节等一律不进消息）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineError {
    code: BicCode,
    txn_status: TxnStatus,
    retryable: bool,
    retry_after_ms: Option<u32>,
    message: String,
}

impl EngineError {
    /// 直接构造（字段组合须自洽；`debug_assert` 校验 `API` §0.4 的组合律）。
    ///
    /// 组合律：`retryable = true` 只与 `none` / `active` 同现；
    /// `aborted` / `unknown` 一律 `retryable = false`；
    /// `retry_after_ms` 只在 `retryable = true` 时有意义。
    /// 优先使用四个类别构造器。
    #[must_use]
    pub fn new(
        code: BicCode,
        txn_status: TxnStatus,
        retryable: bool,
        retry_after_ms: Option<u32>,
        message: impl Into<String>,
    ) -> Self {
        debug_assert!(
            !retryable || matches!(txn_status, TxnStatus::None | TxnStatus::Active),
            "retryable 不得与 aborted/unknown 同现（API §0.4）"
        );
        debug_assert!(
            retry_after_ms.is_none() || retryable,
            "retry_after_ms 只在可重试时有意义"
        );
        Self {
            code,
            txn_status,
            retryable,
            retry_after_ms,
            message: message.into(),
        }
    }

    /// **可重试冲突**：`retryable = true`、不带 `retry_after_ms`。
    #[must_use]
    pub fn retryable_conflict(
        code: BicCode,
        txn_status: TxnStatus,
        message: impl Into<String>,
    ) -> Self {
        Self::new(code, txn_status, true, None, message)
    }

    /// **资源拒绝**：`retryable = true` + `retry_after_ms` 有效（必填）。
    #[must_use]
    pub fn resource_limit(
        code: BicCode,
        txn_status: TxnStatus,
        retry_after_ms: u32,
        message: impl Into<String>,
    ) -> Self {
        Self::new(code, txn_status, true, Some(retry_after_ms), message)
    }

    /// **已中止事务**：`txn_status = aborted`、不可重试原事务。
    #[must_use]
    pub fn transaction_aborted(code: BicCode, message: impl Into<String>) -> Self {
        Self::new(code, TxnStatus::Aborted, false, None, message)
    }

    /// **提交结果未知**：`txn_status = unknown`、不可盲目重试。
    #[must_use]
    pub fn commit_outcome_unknown(code: BicCode, message: impl Into<String>) -> Self {
        Self::new(code, TxnStatus::Unknown, false, None, message)
    }

    /// 普通错误（不可重试、非四类）。
    #[must_use]
    pub fn plain(code: BicCode, txn_status: TxnStatus, message: impl Into<String>) -> Self {
        Self::new(code, txn_status, false, None, message)
    }

    /// **仅凭字段**判定重试类别（`API` §0.4 的判定表；不做字符串匹配）。
    #[must_use]
    pub fn advice(&self) -> RetryAdvice {
        match (self.retryable, self.retry_after_ms, self.txn_status) {
            (true, Some(_), _) => RetryAdvice::ResourceLimit,
            (true, None, _) => RetryAdvice::RetryableConflict,
            (false, _, TxnStatus::Unknown) => RetryAdvice::CommitOutcomeUnknown,
            (false, _, TxnStatus::Aborted) => RetryAdvice::TransactionAborted,
            (false, _, _) => RetryAdvice::NoRetry,
        }
    }

    /// 稳定错误码。
    #[must_use]
    pub fn code(&self) -> BicCode {
        self.code
    }

    /// 事务状态。
    #[must_use]
    pub fn txn_status(&self) -> TxnStatus {
        self.txn_status
    }

    /// 是否可原样重试。
    #[must_use]
    pub fn retryable(&self) -> bool {
        self.retryable
    }

    /// 建议等待（毫秒；仅在资源拒绝类有效）。
    #[must_use]
    pub fn retry_after_ms(&self) -> Option<u32> {
        self.retry_after_ms
    }

    /// 人类可读消息（**不得含私有数据内容**）。
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for EngineError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// REQ-API-012 的验收原句："四类错误**仅凭字段**可区分（不做字符串匹配）"。
    #[test]
    fn four_classes_are_distinguishable_by_fields_only() {
        let conflict = EngineError::retryable_conflict(
            BicCode::DEADLOCK_DETECTED,
            TxnStatus::Active,
            "死锁，语句级回滚",
        );
        let resource = EngineError::resource_limit(
            BicCode::RESOURCE_BUSY,
            TxnStatus::Active,
            1_000,
            "额度触顶",
        );
        let aborted =
            EngineError::transaction_aborted(BicCode::from_raw(10_001).unwrap(), "事务已回滚");
        let unknown =
            EngineError::commit_outcome_unknown(BicCode::from_raw(10_002).unwrap(), "提交结果未知");

        assert_eq!(conflict.advice(), RetryAdvice::RetryableConflict);
        assert_eq!(resource.advice(), RetryAdvice::ResourceLimit);
        assert_eq!(aborted.advice(), RetryAdvice::TransactionAborted);
        assert_eq!(unknown.advice(), RetryAdvice::CommitOutcomeUnknown);

        // 字段组合与判定表一致。
        assert!(conflict.retryable() && conflict.retry_after_ms().is_none());
        assert!(resource.retryable() && resource.retry_after_ms().is_some());
        assert!(!aborted.retryable() && aborted.txn_status() == TxnStatus::Aborted);
        assert!(!unknown.retryable() && unknown.txn_status() == TxnStatus::Unknown);

        // 普通错误不落四类。
        let plain = EngineError::plain(BicCode::INVALID_NUMBER, TxnStatus::None, "无效数字");
        assert_eq!(plain.advice(), RetryAdvice::NoRetry);
    }

    #[test]
    fn frozen_codes_are_pinned() {
        // "只增不改、不复用"的机械保证：这九个数一旦变动，本用例即红。
        assert_eq!(BicCode::UNIQUE_VIOLATION.as_raw(), 1);
        assert_eq!(BicCode::RESOURCE_BUSY.as_raw(), 54);
        assert_eq!(BicCode::DEADLOCK_DETECTED.as_raw(), 60);
        assert_eq!(BicCode::INVALID_IDENTIFIER.as_raw(), 904);
        assert_eq!(BicCode::OBJECT_NOT_FOUND.as_raw(), 942);
        assert_eq!(BicCode::NULL_NOT_ALLOWED.as_raw(), 1400);
        assert_eq!(BicCode::SNAPSHOT_TOO_OLD.as_raw(), 15555);
        assert_eq!(BicCode::INVALID_NUMBER.as_raw(), 17222);
        assert_eq!(BicCode::REFERENCED_OBJECT_EXISTS.as_raw(), 2292);
        // 新码自 10000 起。
        assert_eq!(BicCode::ALLOCATION_FLOOR, 10_000);
        assert!(BicCode::from_raw(10_000).is_some());
        assert!(
            BicCode::from_raw(9_999).is_some(),
            "域内但未分配——分配纪律是文档约定"
        );
    }

    #[test]
    fn code_text_is_bic_five_digits() {
        assert_eq!(BicCode::UNIQUE_VIOLATION.to_string(), "BIC-00001");
        assert_eq!(BicCode::SNAPSHOT_TOO_OLD.to_string(), "BIC-15555");
        assert_eq!(
            EngineError::plain(BicCode::OBJECT_NOT_FOUND, TxnStatus::None, "对象不存在")
                .to_string(),
            "BIC-00942: 对象不存在"
        );
        assert!(BicCode::from_raw(0).is_none());
        assert!(BicCode::from_raw(100_000).is_none());
    }

    #[test]
    fn txn_status_protocol_text() {
        assert_eq!(TxnStatus::None.as_str(), "none");
        assert_eq!(TxnStatus::Active.as_str(), "active");
        assert_eq!(TxnStatus::Aborted.as_str(), "aborted");
        assert_eq!(TxnStatus::Unknown.as_str(), "unknown");
    }
}
