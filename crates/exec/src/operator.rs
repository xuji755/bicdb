//! **算子契约**（火山拉取；设计 §1.1）。
//!
//! ```text
//! 一行一行向上拉：父在 next() 里调子；子产出 0 或 1 行。
//! - 短路是免费的（无 ORDER BY 的 LIMIT 直接打断上游）；
//! - 阻塞算子（Sort/HashAgg/HashJoin 构建侧）在 open/首次 next 里耗尽输入；
//! - 每次 next() 先查取消/截止（ENG 不变量 10）。
//! ```
//!
//! **四条纪律**（设计 §1.1）：执行器不碰页（行经存储服务）；读路径不含锁；
//! 预算随请求；计划不可变、执行状态单次新造。

use bicdb_storage::rowid::RowId;
use bicdb_storage::scan::HeapScanner;

use crate::context::ExecContext;
use crate::error::ExecError;
use crate::value::Row;

/// **行游标**（存储服务侧的拉取口；**可见性已由服务负责**）。
///
/// 生产实现 = 存储服务（表访问 `scan`）；切片 1 的直接实现是
/// `storage::scan::HeapScanner` 的适配。
pub trait RowCursor {
    /// 取下一行（ROWID + 存储行字节）；`None` = 扫完。
    fn next_row(&mut self) -> Result<Option<(RowId, Vec<u8>)>, ExecError>;

    /// **复位到扫描起点**（重扫路径：`SeqScan` 重扫 / `HashAgg` 改档重来；
    /// 快照与边界不变）。缺省 = 不可复位 ⇒ **具名错误**——绝不允许
    /// "从半路接着读"这种静默错（实测踩过：扫描只读到尾巴）。
    fn rewind(&mut self) -> Result<(), ExecError> {
        Err(ExecError::NoRescan)
    }
}

impl RowCursor for HeapScanner<'_, '_, '_, '_> {
    fn next_row(&mut self) -> Result<Option<(RowId, Vec<u8>)>, ExecError> {
        HeapScanner::next_row(self).map_err(ExecError::from)
    }

    fn rewind(&mut self) -> Result<(), ExecError> {
        HeapScanner::rewind(self);
        Ok(())
    }
}

/// **执行算子**（统一接口）。
pub trait Operator {
    /// 打开：取资源（游标、缓冲、临时段租约）；可被 rescan 重入。
    fn open(&mut self, cx: &mut ExecContext<'_>) -> Result<(), ExecError>;

    /// 取下一行（`None` = 本算子树耗尽）。**每次调用先查取消/截止**。
    fn next(&mut self, cx: &mut ExecContext<'_>) -> Result<Option<Row>, ExecError>;

    /// **参数化重扫**（NL 内表的参数变了）；缺省 = 无状态算子，无需动作。
    fn rescan(&mut self, _cx: &mut ExecContext<'_>) -> Result<(), ExecError> {
        Ok(())
    }

    /// 释放（临时段/游标/记账）；幂等；取消路径也必须走到。
    fn close(&mut self, _cx: &mut ExecContext<'_>) {}
}

/// 便捷：把整棵算子树的产出收完（测试/小结果集用；流式消费走 `next`）。
pub fn collect(op: &mut dyn Operator, cx: &mut ExecContext<'_>) -> Result<Vec<Row>, ExecError> {
    op.open(cx)?;
    let mut out = Vec::new();
    while let Some(row) = op.next(cx)? {
        out.push(row);
    }
    op.close(cx);
    Ok(out)
}
