//! 执行期错误（**明确判定**，不静默；错误分类归 `CONV` §4，本模块只负责
//! 执行器内部的具名判定）。

use bicdb_storage::scan::ScanError;

/// 执行期错误。
#[derive(Debug)]
pub enum ExecError {
    /// 被取消（协作式：每个 `next()` 检查；取消后释放锁/页引用/临时空间）。
    Cancelled,
    /// 超过 deadline（与取消同一路径，判定分开——诊断要能区分）。
    Deadline,
    /// 值的类型不匹配（比较/求值的两侧类型不同）。
    TypeMismatch {
        /// 期望类型。
        expected: &'static str,
        /// 实际类型。
        got: &'static str,
    },
    /// 行形状与列下标不符（计划/形状不一致——实现缺陷）。
    RowShapeMismatch {
        /// 越界的列下标。
        col: usize,
    },
    /// 存储行字节不合规范（解码严格失败）。
    BadStoredRow(String),
    /// 参数下标越界。
    ParamOutOfRange {
        /// 请求的参数下标。
        index: usize,
        /// 参数个数。
        count: usize,
    },
    /// 计划引用了不存在的行源（构建期缺陷）。
    NoSuchSource {
        /// 行源标识。
        id: u32,
    },
    /// 存储服务错误（保真外传——`scan` 模块的判定）。
    Scan(ScanError),
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecError::Cancelled => f.write_str("执行被取消"),
            ExecError::Deadline => f.write_str("执行超过截止时间"),
            ExecError::TypeMismatch { expected, got } => {
                write!(f, "类型不匹配：需要 {expected}、得到 {got}")
            }
            ExecError::RowShapeMismatch { col } => write!(f, "行形状不含第 {col} 列"),
            ExecError::BadStoredRow(why) => write!(f, "存储行解码失败：{why}"),
            ExecError::ParamOutOfRange { index, count } => {
                write!(f, "参数下标 {index} 越界（共 {count} 个参数）")
            }
            ExecError::NoSuchSource { id } => write!(f, "计划引用的行源 {id} 不存在"),
            ExecError::Scan(e) => write!(f, "存储服务：{e}"),
        }
    }
}

impl std::error::Error for ExecError {}

impl From<ScanError> for ExecError {
    fn from(e: ScanError) -> Self {
        ExecError::Scan(e)
    }
}
