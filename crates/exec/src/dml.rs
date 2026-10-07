//! **DML 族**（切片 7）：`Insert` / `Update` / `Delete`（设计 §2.6）。
//!
//! ```text
//! Insert：VALUES 行（字面量表达式）→ 逐行编码 → 表访问服务插入
//! Update：源行（**带 ROWID 前缀**）× SET 表达式（按原行求值）→ 逐行改
//! Delete：源行（带 ROWID 前缀）→ 逐行删
//! ```
//!
//! **两条纪律**：
//! 1. **执行器不碰页**——页选址/分配/行锁/等待全在 [`TableWriter`]（表访问
//!    服务的写侧）；算子只给"行字节"，拿回 ROWID。
//! 2. **语句 = 一个事务**（默认自动提交；SQL 面清单）：算子 `open` 开、
//!    收尾提交、出错回滚——`owns_txn = false` 时交给会话层（显式事务）。
//!
//! **ROWID 前缀约定**：[`WithRowId`] 把扫描行变成 `[ROWID 6B 字节] ++ 各列`
//! （`Update`/`Delete` 靠它定位行；投影/过滤的列号随之上移一位——Binder 负责）。

use bicdb_storage::rowid::RowId;

use crate::context::ExecContext;
use crate::error::ExecError;
use crate::expr::{self, Expr};
use crate::operator::Operator;
use crate::value::{encode_row, Row, RowShape, Value};

/// **表访问·写侧**（ENG REQ-ENG-003 的执行器侧形态）。
///
/// 实现方 = 事务引擎 + 段（页选址、行锁、等待-重试、undo/redo 全在其内）；
/// 算子只递"行字节"。
pub trait TableWriter {
    /// **事务归属**：`true` = 写侧自己开关（算子调 `begin`/`commit`/`rollback`）；
    /// `false` = **调用方（会话层）持有**——算子**不得**调用这三个方法
    /// （显式事务 `BEGIN … COMMIT` 的形态：语句不再各自提交）。
    ///
    /// 默认 `true`（自持——测试与独立执行形态）；会话层显式设为 `false`。
    fn owns_txn(&self) -> bool {
        true
    }
    /// 开语句事务（默认自动提交形态）。
    fn begin(&mut self) -> Result<(), ExecError>;
    /// 插入一行；返回 ROWID。
    fn insert_row(&mut self, row: &[u8]) -> Result<RowId, ExecError>;
    /// 按 ROWID 改一行（行锁/等待由实现方负责）。
    ///
    /// **带上旧行字节**：索引维护要"旧键删、新键插"——旧键只能从旧行算
    /// （索引项本身可能是陈旧条目，不能反推）。
    fn update_row(&mut self, rid: RowId, old: &[u8], new: &[u8]) -> Result<(), ExecError>;
    /// 按 ROWID 删一行（同上：索引要删键，得从旧行算）。
    fn delete_row(&mut self, rid: RowId, old: &[u8]) -> Result<(), ExecError>;
    /// 提交语句事务。
    fn commit(&mut self) -> Result<(), ExecError>;
    /// 回滚语句事务（出错路径）。
    fn rollback(&mut self) -> Result<(), ExecError>;
}

/// **出错收尾**（三个 DML 算子共用）：`owns_txn` 时回滚，**回滚失败不吞**。
///
/// 回滚失败意味着行锁/ITL/undo 可能没清——后续语句会撞上自己留下的残局
/// （单写者形态下就是自锁），所以与主错一并报出。与 `bicdb_catalog::ddl` 的
/// 收尾同一口径（2026-10-06 审计的 F14 只修了 DDL，DML 这一份漏了）。
fn finish_error(
    writer: &std::cell::RefCell<&mut dyn TableWriter>,
    owns_txn: bool,
    e: ExecError,
) -> ExecError {
    if !owns_txn {
        return e;
    }
    match writer.borrow_mut().rollback() {
        Ok(()) => e,
        Err(rb) => ExecError::RollbackFailed {
            main: e.to_string(),
            rollback: rb.to_string(),
        },
    }
}

/// **索引维护口**（表访问写侧的可选增量）。
///
/// ```text
/// 算子 insert_row ──▶ 写侧（堆写 + 行锁 + undo/redo）──▶ 本口（每个可用索引一项）
/// ```
///
/// **为什么在 exec 侧开口**：索引清单与"行字节 → 键字节"的规则是**目录/会话**
/// 的知识（`col$`/`ind$` + 行编解码），执行器不碰；写侧只负责"什么时候调"。
/// **顺序**：先堆写、后索引（键里的 ROWID 来自堆写的返回值）。
pub trait IndexMaintenance {
    /// 行插入后：为表的每个**可用**索引插条目（唯一性由表访问服务把守）。
    ///
    /// 参数 = 写上下文的四件套（池/日志/数据文件/事务）——实现方按需转发给
    /// `bicdb-access`；`row` = 刚写进去的**行字节**（全列宽）。
    #[allow(clippy::too_many_arguments)]
    fn after_insert(
        &mut self,
        pool: &bicdb_storage::buffer::BufferPool<'_>,
        log: &mut bicdb_wal::group::GroupWriter<'_, '_>,
        file: &mut bicdb_storage::datafile::DataFile<'_>,
        ws: [u8; 8],
        txn: &bicdb_txn::write::Txn,
        rid: RowId,
        row: &[u8],
    ) -> Result<(), ExecError>;

    /// 行更新后（**旧键删、新键插**；键没变就什么都不做）。
    #[allow(clippy::too_many_arguments)]
    fn after_update(
        &mut self,
        pool: &bicdb_storage::buffer::BufferPool<'_>,
        log: &mut bicdb_wal::group::GroupWriter<'_, '_>,
        file: &mut bicdb_storage::datafile::DataFile<'_>,
        ws: [u8; 8],
        txn: &bicdb_txn::write::Txn,
        rid: RowId,
        old: &[u8],
        new: &[u8],
    ) -> Result<(), ExecError> {
        let _ = (pool, log, file, ws, txn, rid, old, new);
        Err(ExecError::NoIndexMaintenance("UPDATE"))
    }

    /// 行删除后（**删键**；键从**被删的行字节**算）。
    #[allow(clippy::too_many_arguments)]
    fn after_delete(
        &mut self,
        pool: &bicdb_storage::buffer::BufferPool<'_>,
        log: &mut bicdb_wal::group::GroupWriter<'_, '_>,
        file: &mut bicdb_storage::datafile::DataFile<'_>,
        ws: [u8; 8],
        txn: &bicdb_txn::write::Txn,
        rid: RowId,
        old: &[u8],
    ) -> Result<(), ExecError> {
        let _ = (pool, log, file, ws, txn, rid, old);
        Err(ExecError::NoIndexMaintenance("DELETE"))
    }
}

/// **带 ROWID 的扫描**：把存储行游标变成 `[ROWID 6B 字节] ++ 各列`（DML 源用）。
///
/// 直接包**行游标**（存储服务口）——行号的来源在服务侧，不必给
/// `SeqScan`/`IndexScan` 加旁路字段。
pub struct WithRowId<'a> {
    cursor: Box<dyn crate::operator::RowCursor + 'a>,
    shape: RowShape,
    slot: usize,
    opened: bool,
}

impl<'a> WithRowId<'a> {
    /// 构造。
    #[must_use]
    pub fn new(cursor: Box<dyn crate::operator::RowCursor + 'a>, shape: RowShape) -> Self {
        Self {
            cursor,
            shape,
            slot: 0,
            opened: false,
        }
    }
}

impl Operator for WithRowId<'_> {
    fn open(&mut self, cx: &mut ExecContext<'_>) -> Result<(), ExecError> {
        if !self.opened {
            self.slot = cx.register_op("WithRowId");
            self.opened = true;
        }
        Ok(())
    }

    fn next(&mut self, cx: &mut ExecContext<'_>) -> Result<Option<Row>, ExecError> {
        cx.check()?;
        let Some((rid, bytes)) = self.cursor.next_row()? else {
            return Ok(None);
        };
        let row = crate::value::decode_row(&bytes, &self.shape)?;
        let mut values = Vec::with_capacity(row.values.len() + 1);
        values.push(Value::Bytes(rid.to_bytes().to_vec()));
        values.extend(row.values);
        cx.note_row(self.slot);
        Ok(Some(Row::new(values)))
    }
}

/// 从行首取 ROWID（`[6B] ++ 列` 约定）。
fn rowid_of(row: &Row) -> Result<(RowId, &[Value]), ExecError> {
    match row.values.split_first() {
        Some((Value::Bytes(b), rest)) if b.len() == 6 => {
            let mut raw = [0u8; 6];
            raw.copy_from_slice(b);
            Ok((RowId::from_bytes(&raw), rest))
        }
        _ => Err(ExecError::RowShapeMismatch { col: 0 }),
    }
}

/// **`Insert`**（`INSERT INTO t VALUES …`；不产出结果行）。
pub struct Insert<'w> {
    writer: &'w std::cell::RefCell<&'w mut dyn TableWriter>,
    owns_txn: bool,
    shape: RowShape,
    rows: Vec<Vec<Expr>>,
    done: bool,
    slot: usize,
    opened: bool,
}

impl<'w> Insert<'w> {
    /// 构造（`owns_txn` = 默认自动提交形态）。
    #[must_use]
    pub fn new(
        writer: &'w std::cell::RefCell<&'w mut dyn TableWriter>,
        owns_txn: bool,
        shape: RowShape,
        rows: Vec<Vec<Expr>>,
    ) -> Self {
        Self {
            writer,
            owns_txn,
            shape,
            rows,
            done: false,
            slot: 0,
            opened: false,
        }
    }
}

impl Operator for Insert<'_> {
    fn open(&mut self, cx: &mut ExecContext<'_>) -> Result<(), ExecError> {
        if !self.opened {
            self.slot = cx.register_op("Insert");
            self.opened = true;
        }
        if self.owns_txn {
            self.writer.borrow_mut().begin()?;
        }
        Ok(())
    }

    fn next(&mut self, cx: &mut ExecContext<'_>) -> Result<Option<Row>, ExecError> {
        if self.done {
            return Ok(None);
        }
        self.done = true;
        let mut affected = 0u64;
        let result = (|| -> Result<u64, ExecError> {
            for exprs in &self.rows {
                cx.check()?;
                let values: Vec<Value> = exprs
                    .iter()
                    .map(|e| expr::eval(e, &Row::new(Vec::new()), cx.params()))
                    .collect::<Result<_, _>>()?;
                let bytes = encode_row(&Row::new(values), &self.shape)?;
                self.writer.borrow_mut().insert_row(&bytes)?;
                affected += 1;
            }
            Ok(affected)
        })();
        match result {
            Ok(n) => {
                if self.owns_txn {
                    self.writer.borrow_mut().commit()?;
                }
                cx.note_affected(self.slot, n);
                Ok(None) // DML 不产出结果行（SQL 面无 RETURNING）
            }
            Err(e) => Err(finish_error(self.writer, self.owns_txn, e)),
        }
    }
}

/// **`Delete`**（源行带 ROWID）。
pub struct Delete<'a, 'w> {
    input: Box<dyn Operator + 'a>,
    writer: &'w std::cell::RefCell<&'w mut dyn TableWriter>,
    owns_txn: bool,
    /// 表的行形状（旧行重编码用——索引删键要它）。
    shape: RowShape,
    done: bool,
    slot: usize,
    opened: bool,
}

impl<'a, 'w> Delete<'a, 'w> {
    /// 构造。
    #[must_use]
    pub fn new(
        input: Box<dyn Operator + 'a>,
        writer: &'w std::cell::RefCell<&'w mut dyn TableWriter>,
        owns_txn: bool,
        shape: RowShape,
    ) -> Self {
        Self {
            input,
            writer,
            owns_txn,
            shape,
            done: false,
            slot: 0,
            opened: false,
        }
    }
}

impl Operator for Delete<'_, '_> {
    fn open(&mut self, cx: &mut ExecContext<'_>) -> Result<(), ExecError> {
        if !self.opened {
            self.slot = cx.register_op("Delete");
            self.opened = true;
        }
        if self.owns_txn {
            self.writer.borrow_mut().begin()?;
        }
        self.input.open(cx)
    }

    fn next(&mut self, cx: &mut ExecContext<'_>) -> Result<Option<Row>, ExecError> {
        if self.done {
            return Ok(None);
        }
        let result = (|| -> Result<u64, ExecError> {
            let mut affected = 0u64;
            while let Some(row) = self.input.next(cx)? {
                let (rid, values) = rowid_of(&row)?;
                // 旧行字节 = 去掉 ROWID 前缀的列按表形状重编码（索引删键要用）。
                let old = crate::value::encode_row(&Row::new(values.to_vec()), &self.shape)?;
                self.writer.borrow_mut().delete_row(rid, &old)?;
                affected += 1;
            }
            Ok(affected)
        })();
        match result {
            Ok(n) => {
                if self.owns_txn {
                    self.writer.borrow_mut().commit()?;
                }
                self.done = true;
                cx.note_affected(self.slot, n);
                Ok(None)
            }
            Err(e) => {
                // 出错即终结：不置 `done` 的话，再调 `next()` 会因为输入已耗尽
                // 返回 `Ok(n)` 甚至提交——与"出错即终止"矛盾。
                self.done = true;
                Err(finish_error(self.writer, self.owns_txn, e))
            }
        }
    }

    fn close(&mut self, cx: &mut ExecContext<'_>) {
        self.input.close(cx);
    }
}

/// **`Update`**（源行带 ROWID；`sets` = （列号, 新值表达式）——表达式按**原行**求值）。
pub struct Update<'a, 'w> {
    input: Box<dyn Operator + 'a>,
    writer: &'w std::cell::RefCell<&'w mut dyn TableWriter>,
    owns_txn: bool,
    sets: Vec<(usize, Expr)>,
    shape: RowShape,
    done: bool,
    slot: usize,
    opened: bool,
}

impl<'a, 'w> Update<'a, 'w> {
    /// 构造。
    #[must_use]
    pub fn new(
        input: Box<dyn Operator + 'a>,
        writer: &'w std::cell::RefCell<&'w mut dyn TableWriter>,
        owns_txn: bool,
        sets: Vec<(usize, Expr)>,
        shape: RowShape,
    ) -> Self {
        Self {
            input,
            writer,
            owns_txn,
            sets,
            shape,
            done: false,
            slot: 0,
            opened: false,
        }
    }
}

impl Operator for Update<'_, '_> {
    fn open(&mut self, cx: &mut ExecContext<'_>) -> Result<(), ExecError> {
        if !self.opened {
            self.slot = cx.register_op("Update");
            self.opened = true;
        }
        if self.owns_txn {
            self.writer.borrow_mut().begin()?;
        }
        self.input.open(cx)
    }

    fn next(&mut self, cx: &mut ExecContext<'_>) -> Result<Option<Row>, ExecError> {
        if self.done {
            return Ok(None);
        }
        let result = (|| -> Result<u64, ExecError> {
            let mut affected = 0u64;
            while let Some(row) = self.input.next(cx)? {
                let (rid, values) = rowid_of(&row)?;
                // SET 表达式按**原行**求值（各表达式互不影响）。
                let original = Row::new(values.to_vec());
                let mut new_values = values.to_vec();
                for (col, e) in &self.sets {
                    let v = expr::eval(e, &original, cx.params())?;
                    *new_values
                        .get_mut(*col)
                        .ok_or(ExecError::RowShapeMismatch { col: *col })? = v;
                }
                let old = encode_row(&original, &self.shape)?;
                let bytes = encode_row(&Row::new(new_values), &self.shape)?;
                self.writer.borrow_mut().update_row(rid, &old, &bytes)?;
                affected += 1;
            }
            Ok(affected)
        })();
        match result {
            Ok(n) => {
                if self.owns_txn {
                    self.writer.borrow_mut().commit()?;
                }
                self.done = true;
                cx.note_affected(self.slot, n);
                Ok(None)
            }
            Err(e) => {
                // 出错即终结：不置 `done` 的话，再调 `next()` 会因为输入已耗尽
                // 返回 `Ok(n)` 甚至提交——与"出错即终止"矛盾。
                self.done = true;
                Err(finish_error(self.writer, self.owns_txn, e))
            }
        }
    }

    fn close(&mut self, cx: &mut ExecContext<'_>) {
        self.input.close(cx);
    }
}
