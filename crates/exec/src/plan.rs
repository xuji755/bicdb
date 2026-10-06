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
pub struct ExecEnv<'a, 'b, 'io, 'f, 's> {
    /// 缓冲池。
    pub pool: &'a BufferPool<'b>,
    /// 撤销链（CR 读路径）；`None` = 本次执行无读通道（纯写计划/测试）。
    pub chain: Option<&'a UndoChain<'io, 'f>>,
    /// 溢出空间（切片 6b：Sort 等超预算时落 temp 段；`None` = 不支持溢出）。
    pub spill: Option<&'a crate::spill::SpillSpace<'s>>,
    /// 表访问·写侧（切片 7：DML 算子用；`None` = 无写通道）。
    pub writer: Option<&'a std::cell::RefCell<&'a mut dyn crate::dml::TableWriter>>,
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
    /// **带 ROWID 的扫描**（DML 源；行 = `[ROWID 6B] ++ 各列`）。
    WithRowId {
        /// 行源标识。
        source: SourceId,
        /// 表的行形状。
        shape: RowShape,
    },
    /// **插入**（`INSERT INTO t VALUES …`；不产出结果行）。
    Insert {
        /// 表的行形状。
        shape: RowShape,
        /// VALUES 行（每行 = 一列一个表达式）。
        rows: Vec<Vec<Expr>>,
    },
    /// **更新**（源行带 ROWID；SET 表达式按原行求值）。
    Update {
        /// 源子树（必须经 `WithRowId`）。
        input: Box<PlanNode>,
        /// `(列号, 新值表达式)`。
        sets: Vec<(usize, Expr)>,
        /// 表的行形状。
        shape: RowShape,
    },
    /// **删除**（源行带 ROWID）。
    Delete {
        /// 源子树（必须经 `WithRowId`）。
        input: Box<PlanNode>,
    },
    /// **合并追加**（`UNION ALL`；流水）。
    Append {
        /// 子输入（按序）。
        inputs: Vec<PlanNode>,
    },
    /// **相邻去重**（输入须已按去重键有序；`SELECT DISTINCT` / `UNION` 去重）。
    Unique {
        /// 输入子树。
        input: Box<PlanNode>,
        /// 去重键（`None` = 全列升序，宽度由 `width` 给）。
        keys: Option<Vec<crate::sort::SortKey>>,
        /// 行宽（全列键用）。
        width: usize,
    },
    /// **集合运算**（`INTERSECT [ALL]` / `EXCEPT [ALL]`；两侧自动按全列排序）。
    SetOp {
        /// 左侧子树。
        left: Box<PlanNode>,
        /// 右侧子树。
        right: Box<PlanNode>,
        /// 运算种类。
        kind: crate::setops::SetOpKind,
        /// `ALL`（多重集）与否（去重形态）。
        all: bool,
        /// 行宽（全列比较键）。
        width: usize,
    },
    /// **单组聚合**（无 `GROUP BY`；空输入恒出一行）。
    ScalarAgg {
        /// 输入子树。
        input: Box<PlanNode>,
        /// 聚合项（按序）。
        aggs: Vec<crate::agg::AggSpec>,
    },
    /// **哈希聚合**（有 `GROUP BY`；输出键出现序、组内稳定）。
    HashAgg {
        /// 输入子树。
        input: Box<PlanNode>,
        /// 分组键表达式。
        groups: Vec<Expr>,
        /// 聚合项（按序）。
        aggs: Vec<crate::agg::AggSpec>,
    },
    /// **有序聚合**（输入按分组键有序；换组即出——流式首组）。
    SortedAgg {
        /// 输入子树。
        input: Box<PlanNode>,
        /// 分组键表达式。
        groups: Vec<Expr>,
        /// 聚合项（按序）。
        aggs: Vec<crate::agg::AggSpec>,
    },
    /// **哈希连接**（构建侧建表 + 探测侧流式；组合行 = 探测 ++ 构建）。
    HashJoin {
        /// 构建侧（小表）。
        build: Box<PlanNode>,
        /// 探测侧（大表）。
        probe: Box<PlanNode>,
        /// 构建侧键表达式。
        build_keys: Vec<Expr>,
        /// 探测侧键表达式。
        probe_keys: Vec<Expr>,
        /// 连接类型。
        kind: JoinKind,
        /// 连接条件（组合行 = 探测 ++ 构建 上求值；`None` = 键等值即匹配）。
        qual: Option<Expr>,
        /// 构建侧列数（LEFT 补 NULL 用）。
        build_width: usize,
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
pub fn build<'a, 'b: 'a, 'io: 'a, 'f: 'a, 's: 'a>(
    node: &PlanNode,
    env: &ExecEnv<'a, 'b, 'io, 'f, 's>,
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
            let chain = env.chain.ok_or(ExecError::NoWriter)?;
            let scan = IndexScan::new(
                env.pool,
                chain,
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
        PlanNode::WithRowId { source, shape } => Box::new(crate::dml::WithRowId::new(
            open_cursor(*source)?,
            shape.clone(),
        )),
        PlanNode::Insert { shape, rows } => {
            let writer = env.writer.ok_or(ExecError::NoWriter)?;
            Box::new(crate::dml::Insert::new(
                writer,
                true,
                shape.clone(),
                rows.clone(),
            ))
        }
        PlanNode::Update { input, sets, shape } => {
            let writer = env.writer.ok_or(ExecError::NoWriter)?;
            Box::new(crate::dml::Update::new(
                build(input, env, open_cursor)?,
                writer,
                true,
                sets.clone(),
                shape.clone(),
            ))
        }
        PlanNode::Delete { input } => {
            let writer = env.writer.ok_or(ExecError::NoWriter)?;
            Box::new(crate::dml::Delete::new(
                build(input, env, open_cursor)?,
                writer,
                true,
            ))
        }
        PlanNode::Append { inputs } => {
            let mut ops = Vec::with_capacity(inputs.len());
            for i in inputs {
                ops.push(build(i, env, open_cursor)?);
            }
            Box::new(crate::setops::Append::new(ops))
        }
        PlanNode::Unique { input, keys, width } => {
            let keys = keys
                .clone()
                .unwrap_or_else(|| crate::setops::all_columns_keys(*width));
            // 相邻去重要求输入按去重键有序 ⇒ 一律前置 `Sort`
            // （输入已有序时的冗余排序属**优化**，随索引序复用切片再消）。
            let inner = Sort::new(build(input, env, open_cursor)?, keys.clone());
            let inner: Box<dyn Operator + 'a> = match env.spill {
                Some(spill) => Box::new(inner.with_spill(spill)),
                None => Box::new(inner),
            };
            Box::new(crate::setops::Unique::new(inner, keys))
        }
        PlanNode::SetOp {
            left,
            right,
            kind,
            all,
            width,
        } => {
            let keys = crate::setops::all_columns_keys(*width);
            // 两侧必须按**同一总序**排序（相等即相邻）；有溢出空间就接上。
            let l = match env.spill {
                Some(spill) => {
                    Sort::new(build(left, env, open_cursor)?, keys.clone()).with_spill(spill)
                }
                None => Sort::new(build(left, env, open_cursor)?, keys.clone()),
            };
            let r = match env.spill {
                Some(spill) => {
                    Sort::new(build(right, env, open_cursor)?, keys.clone()).with_spill(spill)
                }
                None => Sort::new(build(right, env, open_cursor)?, keys.clone()),
            };
            Box::new(crate::setops::SetOp::new(
                Box::new(l),
                Box::new(r),
                *kind,
                *all,
                keys,
            ))
        }
        PlanNode::ScalarAgg { input, aggs } => Box::new(crate::agg::ScalarAgg::new(
            build(input, env, open_cursor)?,
            aggs.clone(),
        )),
        PlanNode::HashAgg {
            input,
            groups,
            aggs,
        } => Box::new(crate::agg::HashAgg::new(
            build(input, env, open_cursor)?,
            groups.clone(),
            aggs.clone(),
        )),
        PlanNode::SortedAgg {
            input,
            groups,
            aggs,
        } => Box::new(crate::agg::SortedAgg::new(
            build(input, env, open_cursor)?,
            groups.clone(),
            aggs.clone(),
        )),
        PlanNode::HashJoin {
            build: build_side,
            probe: probe_side,
            build_keys,
            probe_keys,
            kind,
            qual,
            build_width,
        } => Box::new(crate::hash_join::HashJoin::new(
            build(build_side, env, open_cursor)?,
            build(probe_side, env, open_cursor)?,
            build_keys.clone(),
            probe_keys.clone(),
            *kind,
            qual.clone(),
            *build_width,
        )),
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
            let inner = build(input, env, open_cursor)?;
            Box::new(match env.spill {
                Some(spill) => Sort::new(inner, keys.clone()).with_spill(spill),
                None => Sort::new(inner, keys.clone()),
            })
        }
        PlanNode::TopN { input, keys, keep } => Box::new(TopN::new(
            build(input, env, open_cursor)?,
            keys.clone(),
            *keep,
        )),
    })
}
