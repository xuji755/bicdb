//! 执行期错误（**明确判定**，不静默；错误分类归 `CONV` §4，本模块只负责
//! 执行器内部的具名判定）。

use bicdb_index::IndexError;
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
    /// 计划含写算子但执行环境没给写通道（构建期缺陷）。
    NoWriter,
    /// 计划引用了不存在的行源（构建期缺陷）。
    NoSuchSource {
        /// 行源标识。
        id: u32,
    },
    /// 存储服务错误（保真外传——`scan` 模块的判定）。
    Scan(ScanError),
    /// 索引层错误（保真外传——`bicdb-index` 的判定）。
    Index(IndexError),
    /// 数值运算越出 `NUMBER` 域（溢出/下溢——`TYP` 的明确判定，不回绕）。
    NumericOverflow,
    /// 除以零（SQL 层的确定错误）。
    DivisionByZero,
    /// **溢出底座错误**（temp 段读写/序列化——切片 6b）。
    Spill(String),
    /// **表访问服务**（REQ-ENG-003 的写侧——保真外传）。
    TableAccess(bicdb_access::TableAccessError),
    /// **行游标不可复位**（重扫路径要求可复位来源——`SeqScan` 重扫 /
    /// `HashAgg` 改档重来；缺省的 [`crate::operator::RowCursor::rewind`]）。
    NoRescan,
    /// **工作内存超预算**（切片 2c 的临时形态：外部归并/分区随切片 6 的
    /// WMM + temp 段接入——届时本错误在正常路径不可达）。
    WorkMemoryExceeded {
        /// 已用（估计字节）。
        used: u64,
        /// 预算（字节；`0` = 未设预算）。
        budget: u64,
    },
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
            ExecError::NoWriter => f.write_str("计划含写算子，但执行环境未提供写通道"),
            ExecError::NoSuchSource { id } => write!(f, "计划引用的行源 {id} 不存在"),
            ExecError::Scan(e) => write!(f, "存储服务：{e}"),
            ExecError::Index(e) => write!(f, "索引：{e}"),
            ExecError::Spill(why) => write!(f, "溢出（temp 段）：{why}"),
            ExecError::TableAccess(e) => write!(f, "{e}"),
            ExecError::NoRescan => f.write_str("行游标不可复位（重扫要求可复位来源）"),
            ExecError::NumericOverflow => f.write_str("数值运算越出 NUMBER 域"),
            ExecError::DivisionByZero => f.write_str("除以零"),
            ExecError::WorkMemoryExceeded { used, budget } => write!(
                f,
                "工作内存超预算：已用约 {used} 字节、预算 {budget} 字节（溢出随切片 6）"
            ),
        }
    }
}

impl std::error::Error for ExecError {}

impl From<ScanError> for ExecError {
    fn from(e: ScanError) -> Self {
        ExecError::Scan(e)
    }
}

impl From<IndexError> for ExecError {
    fn from(e: IndexError) -> Self {
        ExecError::Index(e)
    }
}
