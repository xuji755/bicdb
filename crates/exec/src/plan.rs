//! **物理计划节点**（切片 1 子集）与构建（计划 → 算子树）。
//!
//! 计划**不可变、无值**（REQ-SQL-001：物理计划里只有编译期决策）；
//! 每次执行由本模块**构建一棵新算子树**（REQ-SQL-004：执行状态单次新造）。

use bicdb_storage::buffer::BufferPool;
use bicdb_storage::rowid::RowId;
use bicdb_storage::undo::UndoChain;

use crate::error::ExecError;
use crate::expr::Expr;
use crate::index_scan::IndexScan;
use crate::join::{JoinKind, NestedLoop};
use crate::nodes::{Filter, Limit, Project, SeqScan};
use crate::operator::{Operator, RowCursor};
use crate::sort::{Sort, SortKey, TopN};
use crate::value::{ColKind, RowShape};

/// **执行环境**：构建算子树时的存储服务口（池 + 撤销链）。
pub struct ExecEnv<'a, 'b, 'io, 'f> {
    /// 缓冲池。
    pub pool: &'a BufferPool<'b>,
    /// 撤销链（CR 读路径）。
    pub chain: &'a UndoChain<'io, 'f>,
}

/// 行源标识（切片 1：单表；编号由计划给出，执行期映射到存储服务的表）。
pub type SourceId = u32;

/// 物理计划节点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanNode {
    /// 顺序扫描一个行源。
    SeqScan {
        /// 行源标识。
        source: SourceId,
        /// 表的行形状（列定义）。
        shape: RowShape,
    },
    /// 过滤（谓词只放行 TRUE）。
    Filter {
        /// 输入子树。
        input: Box<PlanNode>,
        /// 谓词。
        predicate: Expr,
    },
    /// 投影。
    Project {
        /// 输入子树。
        input: Box<PlanNode>,
        /// 投影表达式（顺序 = 输出列序）。
        exprs: Vec<Expr>,
    },
    /// 限行（`LIMIT n OFFSET m`）。
    Limit {
        /// 输入子树。
        input: Box<PlanNode>,
        /// `LIMIT n`。
        limit: u64,
        /// `OFFSET m`。
        offset: u64,
    },
    /// 全排序（阻塞；切片 2c 内存形态，外部归并随切片 6）。
    Sort {
        /// 输入子树。
        input: Box<PlanNode>,
        /// 排序键（按序）。
        keys: Vec<SortKey>,
    },
    /// 有界排序（`ORDER BY … LIMIT` 合并；输入仍全读——§4.1）。
    TopN {
        /// 输入子树。
        input: Box<PlanNode>,
        /// 排序键（按序）。
        keys: Vec<SortKey>,
        /// 保留行数（**已含 `OFFSET` 份额**——计划侧给）。
        keep: u64,
    },
    /// **索引扫描**（定位 + 批量回表；§2.1/§9.4）。
    IndexScan {
        /// 索引段所在文件号。
        file_id: u16,
        /// 树头（根页 ROWID——执行器从段头扩展区读得）。
        root: RowId,
        /// 键的列形态（覆盖扫描解码键用）。
        key_kind: ColKind,
        /// 回表行形状（`covered` 时忽略）。
        shape: RowShape,
        /// 范围下界（闭区间；`None` = 无界）。
        low: Option<Expr>,
        /// 范围上界（闭区间；`None` = 无界）。
        high: Option<Expr>,
        /// 覆盖扫描（不回表）。
        covered: bool,
        /// 条目上限。
        limit: Option<u64>,
        /// 回表批量大小（§9.4；`None` = 默认 256）。
        batch: Option<usize>,
    },
    /// **嵌套循环连接**（内表参数化重扫）。
    NestedLoop {
        /// 外层子树。
        outer: Box<PlanNode>,
        /// 内层子树。
        inner: Box<PlanNode>,
        /// 内表参数（对外层行求值）。
        inner_params: Vec<Expr>,
        /// 连接类型。
        kind: JoinKind,
        /// 连接条件（组合行 `outer ++ inner` 上求值）。
        qual: Option<Expr>,
        /// 内层列数（LEFT 补 NULL 用）。
        inner_width: usize,
    },
}

/// **构建算子树**：`open_cursor` 按行源标识开一个**新**行游标
/// （每次调用返回新游标——树里每个 `SeqScan` 各开一个）；索引扫描经 `env`。
pub fn build<'a, 'b: 'a, 'io: 'a, 'f: 'a>(
    node: &PlanNode,
    env: &ExecEnv<'a, 'b, 'io, 'f>,
    open_cursor: &mut dyn FnMut(SourceId) -> Result<Box<dyn RowCursor + 'a>, ExecError>,
) -> Result<Box<dyn Operator + 'a>, ExecError> {
    Ok(match node {
        PlanNode::SeqScan { source, shape } => {
            Box::new(SeqScan::new(open_cursor(*source)?, shape.clone()))
        }
        PlanNode::Filter { input, predicate } => Box::new(Filter::new(
            build(input, env, open_cursor)?,
            predicate.clone(),
        )),
        PlanNode::Project { input, exprs } => {
            Box::new(Project::new(build(input, env, open_cursor)?, exprs.clone()))
        }
        PlanNode::Limit {
            input,
            limit,
            offset,
        } => Box::new(Limit::new(build(input, env, open_cursor)?, *limit, *offset)),
        PlanNode::IndexScan {
            file_id,
            root,
            key_kind,
            shape,
            low,
            high,
            covered,
            limit,
            batch,
        } => {
            let scan = IndexScan::new(
                env.pool,
                env.chain,
                *file_id,
                *root,
                *key_kind,
                shape.clone(),
                low.clone(),
                high.clone(),
                *covered,
                *limit,
            );
            Box::new(match batch {
                Some(n) => scan.with_batch(*n),
                None => scan,
            })
        }
        PlanNode::NestedLoop {
            outer,
            inner,
            inner_params,
            kind,
            qual,
            inner_width,
        } => Box::new(NestedLoop::new(
            build(outer, env, open_cursor)?,
            build(inner, env, open_cursor)?,
            inner_params.clone(),
            *kind,
            qual.clone(),
            *inner_width,
        )),
        PlanNode::Sort { input, keys } => {
            Box::new(Sort::new(build(input, env, open_cursor)?, keys.clone()))
        }
        PlanNode::TopN { input, keys, keep } => Box::new(TopN::new(
            build(input, env, open_cursor)?,
            keys.clone(),
            *keep,
        )),
    })
}
