//! **会话侧装配：compile → execute**（`SQL前端设计` §2 `src/session.rs`；ENG REQ-ENG-005）。
//!
//! ```text
//! SQL 文本 ── parse ── bind ── plan ── execute ──▶ 结果集 / 影响行数
//!               ①       ②      ③④       ⑤
//! ```
//!
//! **事务边界**（既定纪律：语句 = 一个事务，REQ-TXN-016 的 DDL 独占）：
//! - **自动提交**（默认）：每条语句自带事务（写语句 commit、出错 rollback）；
//! - **显式事务**（`BEGIN`）：持一个事务句柄，`COMMIT`/`ROLLBACK` 收尾——
//!   期间 DDL 一律拒绝（"活动事务中发 DDL ⇒ 拒绝"）。
//!
//! **本切片的会话面**：`SELECT` / `INSERT … VALUES` / `CREATE TABLE` /
//! `CREATE [UNIQUE] INDEX` / `DROP TABLE|INDEX` / `BEGIN|COMMIT|ROLLBACK`。
//! 清单外语句在**绑定期**具名拒绝（绝不静默）。

use bicdb_catalog::ddl::{self, create_index, create_table, drop_index, drop_table};
use bicdb_common::seq::CommitSeq;
use bicdb_exec::{
    build, collect, ExecContext, ExecEnv, Row, RowCursor, TableAccessWriter, TableWriter, Value,
};
use bicdb_storage::buffer::BufferPool;
use bicdb_storage::scan::HeapScanner;
use bicdb_storage::segment::Segment;
use bicdb_txn::engine::{Engine, TxnHandle};

use crate::bind::{bind_statement, BindError, CatalogView, CatalogViewImpl, NameResolver};
use crate::parser::parse_many as parse;
use crate::plan::{ddl_summary, plan_statement, PhysicalPlan, PlanKind};

/// 会话错误。
#[derive(Debug)]
pub enum SessionError {
    /// 词法/语法。
    Parse(crate::parser::ParseError),
    /// 绑定。
    Bind(BindError),
    /// 目录写侧（DDL）。
    Ddl(ddl::DdlError),
    /// 执行器。
    Exec(bicdb_exec::ExecError),
    /// 事务引擎。
    Txn(bicdb_txn::write::TxnError),
    /// 段层。
    Segment(bicdb_storage::segment::SegmentSpaceError),
    /// 会话状态非法（DDL 落在显式事务里 / 提交时无事务 …）。
    State(String),
    /// **参数面**（缺值 / 多给 / 形态不符）——语句声明了参数却没配对。
    Params(String),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::Parse(e) => write!(f, "{e}"),
            SessionError::Bind(e) => write!(f, "{e}"),
            SessionError::Ddl(e) => write!(f, "{e}"),
            SessionError::Exec(e) => write!(f, "执行：{e}"),
            SessionError::Txn(e) => write!(f, "事务：{e}"),
            SessionError::Segment(e) => write!(f, "段：{e}"),
            SessionError::State(why) => f.write_str(why),
            SessionError::Params(why) => write!(f, "参数：{why}"),
        }
    }
}

impl std::error::Error for SessionError {}

macro_rules! from_err {
    ($($v:ident <- $t:ty),* $(,)?) => {
        $(impl From<$t> for SessionError { fn from(e: $t) -> Self { Self::$v(e) } })*
    };
}
from_err!(
    Parse <- crate::parser::ParseError,
    Bind <- BindError,
    Ddl <- ddl::DdlError,
    Exec <- bicdb_exec::ExecError,
    Txn <- bicdb_txn::write::TxnError,
    Segment <- bicdb_storage::segment::SegmentSpaceError,
);

/// 一条语句的结果（协议层/CLI 的呈现形态）。
#[derive(Debug, Clone, PartialEq)]
pub enum QueryResult {
    /// 结果集。
    Rows {
        /// 列名。
        columns: Vec<String>,
        /// 行（值已是显示形态）。
        rows: Vec<Vec<String>>,
    },
    /// 影响行数（DML）。
    Affected(u64),
    /// DDL 回执。
    Ddl(String),
    /// 事务回执。
    Txn(String),
}

impl QueryResult {
    /// 结果集的显示宽（诊断/CLI 对齐）。
    #[must_use]
    pub fn row_count(&self) -> usize {
        match self {
            QueryResult::Rows { rows, .. } => rows.len(),
            _ => 0,
        }
    }
}

/// **会话**：一个工作区的编译-执行通道。
pub struct Session<'a, 'b, 'io, 'f> {
    pool: &'a BufferPool<'b>,
    engine: &'a Engine<'io, 'f, 'io, 'f>,
    catalog: &'a mut bicdb_catalog::Catalog<'io>,
    ws: [u8; 8],
    /// 显式事务（`BEGIN` 后持有）。
    txn: Option<TxnHandle>,
    /// 当前提交序号（新语句的快照水位）。
    seq: u64,
    /// **本事务已写过的唯一键**（语句内/事务内的重复检测；`COMMIT`/`ROLLBACK` 清空）。
    seen_keys: crate::dml_index::SeenKeys,
    /// **本语句的参数值**（按绑定期给出的**出现序**摆好；空 = 无参数）。
    exec_params: Vec<Value>,
}

impl<'a, 'b, 'io, 'f> Session<'a, 'b, 'io, 'f> {
    /// 建会话。
    #[must_use]
    pub fn new(
        pool: &'a BufferPool<'b>,
        engine: &'a Engine<'io, 'f, 'io, 'f>,
        catalog: &'a mut bicdb_catalog::Catalog<'io>,
        seq: u64,
    ) -> Self {
        let ws = catalog.ws();
        Self {
            pool,
            engine,
            catalog,
            ws,
            txn: None,
            seq,
            seen_keys: crate::dml_index::SeenKeys::new(),
            exec_params: Vec::new(),
        }
    }

    /// 当前提交序号。
    #[must_use]
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// 有没有未收尾的显式事务。
    #[must_use]
    pub fn in_transaction(&self) -> bool {
        self.txn.is_some()
    }

    /// **跑一条语句**（`sql` 可以是多条以 `;` 分隔；无参数）。
    ///
    /// 语句声明了参数（`:name`）而没有给值 ⇒ [`SessionError::Params`]——
    /// **不**让执行器抛"参数下标越界"（那是实现细节，不是用户看到的错）。
    pub fn execute(&mut self, sql: &str) -> Result<Vec<QueryResult>, SessionError> {
        self.execute_with_params(sql, &[])
    }

    /// **带参数跑一条语句**：`named` = 调用方按名给的值（语句里 `:name`）。
    ///
    /// 摆位规则 = **绑定期记下的出现序**（`BoundParams::list()`）——同一参数在
    /// 语句里出现多次只占一个位（绑定期已按名归并），因此这里按名查值即可。
    pub fn execute_with_params(
        &mut self,
        sql: &str,
        named: &[(&str, Value)],
    ) -> Result<Vec<QueryResult>, SessionError> {
        let stmts = parse(sql)?;
        let mut out = Vec::with_capacity(stmts.len());
        // **参数是"整批共用"的**：一条语句只用其中几个是正常的
        // （`BEGIN; INSERT … :p; COMMIT`）——"多给"只在**整批**都没用到时才报。
        let mut used: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for stmt in &stmts {
            out.push(self.execute_one(stmt, named, &mut used)?);
        }
        for (name, _) in named {
            if !used.contains(name) {
                return Err(SessionError::Params(format!(
                    "整批语句都没用到参数 `:{name}`"
                )));
            }
        }
        Ok(out)
    }

    fn snapshot(&self) -> CommitSeq {
        CommitSeq::from_raw(self.seq + 1).expect("48 位域内")
    }

    fn execute_one<'c>(
        &mut self,
        stmt: &crate::ast::Stmt,
        named: &'c [(&'c str, Value)],
        used: &mut std::collections::HashSet<&'c str>,
    ) -> Result<QueryResult, SessionError> {
        // ① 解析已完成（调用方）；② 绑定。
        let snapshot = self.snapshot();
        let bound = {
            let mut view = CatalogViewImpl::new(self.catalog, snapshot);
            let mut resolver = NameResolver::new(&mut view);
            bind_statement(&mut resolver, stmt)?
        };
        // **参数摆位**（绑定期的清单 + 调用方按名给的值）。
        self.exec_params = place_params(&bound, named, used)?;

        match bound {
            crate::bind::BoundStatement::Transaction(kind) => self.transaction(kind),
            crate::bind::BoundStatement::CreateWorkspace { subject, .. } => {
                // 工作区 DDL 在管理面（`public` 实例 + DCL）；本会话面不做。
                Err(SessionError::State(format!(
                    "`CREATE WORKSPACE FOR USER {subject}` 走管理面（DCL，随 D2 切片）"
                )))
            }
            crate::bind::BoundStatement::Ddl(d) => {
                if self.in_transaction() {
                    return Err(SessionError::State(
                        "活动事务中发 DDL ⇒ 拒绝（REQ-TXN-016）".to_owned(),
                    ));
                }
                let summary = ddl_summary(&d);
                match d {
                    crate::bind::BoundDdl::CreateTable(spec) => {
                        create_table(self.catalog, self.engine, &spec)?;
                    }
                    crate::bind::BoundDdl::CreateIndex(spec) => {
                        create_index(self.catalog, self.engine, &spec)?;
                    }
                    crate::bind::BoundDdl::DropTable(name) => {
                        drop_table(self.catalog, self.engine, &name)?;
                    }
                    crate::bind::BoundDdl::DropIndex(name) => {
                        drop_index(self.catalog, self.engine, &name)?;
                    }
                }
                // DDL 之后装载戳前移（目录已 advance_commit）。
                self.seq = self.catalog.current_seq();
                Ok(QueryResult::Ddl(summary))
            }
            crate::bind::BoundStatement::Select(_) | crate::bind::BoundStatement::Insert(_) => {
                let plan = {
                    let mut view = CatalogViewImpl::new(self.catalog, snapshot);
                    plan_statement(&bound, &mut |obj| view.segment_block(obj))?
                };
                let Some(plan) = plan else {
                    return Err(SessionError::State("该语句无物理计划".to_owned()));
                };
                self.execute_plan(&plan, snapshot)
            }
        }
    }

    // ───────────────────────── 事务控制 ─────────────────────────

    fn transaction(
        &mut self,
        kind: crate::ast::TransactionStmtKind,
    ) -> Result<QueryResult, SessionError> {
        use crate::ast::TransactionStmtKind as K;
        match kind {
            K::Begin => {
                if self.in_transaction() {
                    return Err(SessionError::State("事务已开（嵌套 BEGIN）".to_owned()));
                }
                let txn = self.engine.begin()?;
                self.txn = Some(txn);
                Ok(QueryResult::Txn("BEGIN".to_owned()))
            }
            K::Commit => {
                let mut txn = self
                    .txn
                    .take()
                    .ok_or_else(|| SessionError::State("没有活动事务".to_owned()))?;
                let seq = self.engine.commit(&mut txn)?;
                self.seq = seq.as_raw();
                self.seen_keys.clear();
                Ok(QueryResult::Txn(format!("COMMIT（提交序号 {seq}）")))
            }
            K::Rollback => {
                let mut txn = self
                    .txn
                    .take()
                    .ok_or_else(|| SessionError::State("没有活动事务".to_owned()))?;
                let n = self.engine.rollback(&mut txn)?;
                self.seen_keys.clear();
                Ok(QueryResult::Txn(format!("ROLLBACK（撤销 {n} 条）")))
            }
        }
    }

    // ───────────────────────── 计划执行 ─────────────────────────

    fn execute_plan(
        &mut self,
        plan: &PhysicalPlan,
        snapshot: CommitSeq,
    ) -> Result<QueryResult, SessionError> {
        match plan.kind {
            PlanKind::Select => self.run_select(plan, snapshot),
            PlanKind::Insert => self.run_insert(plan, snapshot),
        }
    }

    fn run_select(
        &mut self,
        plan: &PhysicalPlan,
        snapshot: CommitSeq,
    ) -> Result<QueryResult, SessionError> {
        let source = plan
            .sources
            .first()
            .ok_or_else(|| SessionError::State("SELECT 缺行源".to_owned()))?;
        // 扫描边界：段头（池优先）+ 数据页清单。
        let (file_id, blocks) = {
            let seg = Segment::open_pooled(
                self.pool,
                self.catalog.file_mut(),
                source.seg_block,
                self.ws,
            )?;
            let fid = seg.file_id();
            let hwm = seg.hwm();
            (fid, seg.data_blocks(hwm))
        };
        let node = plan.node.clone();
        let columns = plan.output_names.clone();
        let params_in = self.exec_params.clone();
        let pool = self.pool;
        // 扫描期与 CR 共持撤销链（**读上下文**；语句内完成，不做长事务）。
        let rows = self.engine.with_read_context(|pool_ref, chain| {
            let mut open = |_src: bicdb_exec::SourceId| {
                Ok(Box::new(HeapScanner::new(
                    pool_ref,
                    chain,
                    snapshot,
                    file_id,
                    blocks.clone(),
                )) as Box<dyn RowCursor>)
            };
            let envx = ExecEnv {
                pool: pool_ref,
                chain: None,
                spill: None,
                writer: None,
            };
            let mut op = build(&node, &envx, &mut open)?;
            let mut cx = ExecContext::new(snapshot).with_params(&params_in);
            collect(op.as_mut(), &mut cx)
        })?;
        let _ = pool;
        Ok(QueryResult::Rows {
            columns,
            rows: rows.iter().map(format_row).collect(),
        })
    }

    /// **INSERT … VALUES**（写路径）。
    ///
    /// **事务归属**：会话层持事务（写侧 `owns_txn = false`）——
    /// 自动提交形态由本方法收尾，显式事务里留给 `COMMIT`。
    /// **索引维护**：语句开始时从目录取一次清单（活索引 + 键列 + 段头块），
    /// 装进写侧的口（`catalog` 只在写前读，写中不再回查字典）。
    fn run_insert(
        &mut self,
        plan: &PhysicalPlan,
        snapshot: CommitSeq,
    ) -> Result<QueryResult, SessionError> {
        let source = plan
            .sources
            .first()
            .ok_or_else(|| SessionError::State("INSERT 缺目标表".to_owned()))?;
        let node = plan.node.clone();
        let params_in = self.exec_params.clone();
        let seg_block = source.seg_block;
        let table_obj = source.table_obj;
        // 索引清单（写前一次；空清单 = 不装口，写侧零开销）。
        let ws = self.ws;
        let mut indexes = crate::dml_index::table_indexes(self.catalog, snapshot, table_obj)?;
        // **表选项**（`tab$`）：`pctfree` 管页内预留、`itl_max` 管 ITL 扩展上限——
        // 两者此前只在字典/段头里躺着，写路径恒用缺省值（本次接线修掉）。
        let opts = self
            .catalog
            .table_options(snapshot, table_obj)
            .map_err(|e| SessionError::State(format!("读表选项：{e}")))?;
        // **唯一性预检**（写前；同键活行 ⇒ 冲突，语句整体不写）。
        if indexes.has_unique() {
            let rows = plan_row_bytes(&plan.node, &self.exec_params)?;
            let mut seen = std::mem::take(&mut self.seen_keys);
            let checked = self.engine.with_read_context(|pool, chain| {
                crate::dml_index::check_unique(
                    self.catalog,
                    pool,
                    chain,
                    snapshot,
                    &indexes,
                    &rows,
                    &mut seen,
                )
            });
            self.seen_keys = seen;
            checked?;
        }
        let has_indexes = !indexes.is_empty();
        let own_txn = self.txn.is_none();
        let mut txn = match self.txn.take() {
            Some(t) => t,
            None => self.engine.begin()?,
        };
        let outcome = self
            .engine
            .with_write_context(&mut txn, |pool, log, chain, t| {
                let mut writer = TableAccessWriter::with_txn(
                    pool,
                    chain,
                    log,
                    self.catalog.file_mut(),
                    seg_block,
                    ws,
                    t,
                );
                writer.set_table_options(opts.pctfree as u8, opts.itl_max as u16);
                if has_indexes {
                    writer.set_indexes(&mut indexes);
                }
                let cell = std::cell::RefCell::new(&mut writer as &mut dyn TableWriter);
                let mut open =
                    |_src: bicdb_exec::SourceId| -> Result<Box<dyn RowCursor>, bicdb_exec::ExecError> {
                        unreachable!("INSERT … VALUES 不走行源")
                    };
                let envx = ExecEnv {
                    pool,
                    chain: None,
                    spill: None,
                    writer: Some(&cell),
                };
                let mut op = build(&node, &envx, &mut open)?;
                let mut cx = ExecContext::new(snapshot).with_params(&params_in);
                collect(op.as_mut(), &mut cx)?;
                Ok::<u64, bicdb_exec::ExecError>(cx.rows_affected_of("Insert"))
            });
        match outcome {
            Ok(affected) => {
                if own_txn {
                    let committed = self.engine.commit(&mut txn)?;
                    self.seq = committed.as_raw();
                    self.seen_keys.clear();
                } else {
                    // 显式事务：语句不提交（写侧 owns_txn = false 已挡住算子提交）。
                    self.txn = Some(txn);
                }
                Ok(QueryResult::Affected(affected))
            }
            Err(e) => {
                let _ = self.engine.rollback(&mut txn);
                self.seen_keys.clear();
                Err(SessionError::Exec(e))
            }
        }
    }
}

/// **计划里的行字面量 → 行字节**（唯一性预检用；与写侧的编码同一份）。
///
/// `INSERT … VALUES` 的行在计划里恒为**字面量**（绑定期的形态）——非字面量
/// 走到这里即计划形状不符（明确报错，不静默跳过预检）。
fn plan_row_bytes(
    node: &bicdb_exec::PlanNode,
    params: &[Value],
) -> Result<Vec<Vec<u8>>, SessionError> {
    let bicdb_exec::PlanNode::Insert { shape, rows } = node else {
        return Err(SessionError::State(
            "INSERT 的计划节点不是 Insert".to_owned(),
        ));
    };
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let mut values = Vec::with_capacity(row.len());
        for e in row {
            match e {
                bicdb_exec::Expr::Literal(v) => values.push(v.clone()),
                // **参数**：按已摆好的位取值（与执行期同一份序列）。
                bicdb_exec::Expr::Param(i) => values.push(
                    params
                        .get(*i)
                        .cloned()
                        .ok_or_else(|| SessionError::Params(format!("参数位 {i} 无值")))?,
                ),
                other => {
                    return Err(SessionError::State(format!(
                        "唯一性预检只认字面量与参数，行里出现 {other:?}"
                    )))
                }
            }
        }
        out.push(bicdb_exec::encode_row(&Row::new(values), shape).map_err(SessionError::Exec)?);
    }
    Ok(out)
}

/// **参数摆位**：绑定期的清单（名 + 形态 + 出现序）× 调用方按名给的值
/// ⇒ 执行期参数序列（位置 = 出现序）。
///
/// 三条判定都是**具名**的：缺值 / 多给（语句里没这个参数）/ 形态不符。
/// `NULL` 对任何形态都放行（它本来就没有类型）。
fn place_params<'n>(
    bound: &crate::bind::BoundStatement,
    named: &'n [(&'n str, Value)],
    used: &mut std::collections::HashSet<&'n str>,
) -> Result<Vec<Value>, SessionError> {
    use crate::bind::BoundStatement as B;
    let declared: Vec<(&str, bicdb_exec::ColKind)> = match bound {
        B::Select(s) => s.params.list(),
        B::Insert(i) => i.params.list(),
        _ => Vec::new(),
    };
    for (name, _) in named {
        if declared.iter().any(|(d, _)| *d == *name) {
            used.insert(name);
        }
    }
    let mut out = Vec::with_capacity(declared.len());
    for (name, kind) in &declared {
        let (_, v) = named
            .iter()
            .find(|(n, _)| n == name)
            .ok_or_else(|| SessionError::Params(format!("缺参数值 `:{name}`")))?;
        if !matches!(v, Value::Null) {
            let ok = matches!(
                (kind, v),
                (bicdb_exec::ColKind::Number, Value::Number(_))
                    | (bicdb_exec::ColKind::Bytes, Value::Bytes(_))
                    | (bicdb_exec::ColKind::Bool, Value::Bool(_))
            );
            if !ok {
                return Err(SessionError::Params(format!(
                    "参数 `:{name}` 应是{}，给的是{}",
                    crate::bind::kind_name(*kind),
                    value_kind_name(v)
                )));
            }
        }
        out.push(v.clone());
    }
    Ok(out)
}

/// 值的形态名（诊断）。
fn value_kind_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "NULL",
        Value::Number(_) => "数值",
        Value::Bytes(_) => "字节串",
        Value::Bool(_) => "布尔",
    }
}

/// **会话收尾**：显式事务还没收尾就丢会话 ⇒ 回滚（不留输家给下次恢复）。
impl Drop for Session<'_, '_, '_, '_> {
    fn drop(&mut self) {
        if let Some(mut txn) = self.txn.take() {
            let _ = self.engine.rollback(&mut txn);
        }
    }
}

/// 行 → 显示串（CLI/协议层的呈现；NULL 显式写出）。
fn format_row(r: &Row) -> Vec<String> {
    r.values.iter().map(format_value).collect()
}

/// 值 → 显示串。
#[must_use]
pub fn format_value(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_owned(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Bytes(b) => String::from_utf8(b.clone()).unwrap_or_else(|_| format!("0x{}", hex(b))),
    }
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}
