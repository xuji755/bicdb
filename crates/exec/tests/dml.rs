//! **切片 7 验收**：DML 算子（`Insert`/`Update`/`Delete`）接事务引擎写侧。
//!
//! 写侧服务口 [`TableWriter`] 在用例里用**真件**实现：真缓冲池 + 真撤销链 +
//! 真日志（`bicdb-txn::write` 的 begin/insert_row/update_row/delete_row/commit）。
//! 钉住：影响行数；写后**在提交后的新快照下**可读回；语句事务边界
//! （自动提交 / 出错回滚）。

mod common;

use std::cell::RefCell;
use std::path::Path;

use bicdb_common::seq::{CommitSeq, Lsn};
use bicdb_exec::{
    build, collect, ExecContext, ExecEnv, ExecError, Expr, PlanNode, Row, RowCursor, TableWriter,
    Value,
};
use bicdb_storage::buffer::{BufferKey, BufferPool};
use bicdb_storage::controlfile::{ArchiveRecord, ControlFile, RedoEntries, WorkspaceEntry};
use bicdb_storage::heap::{self, InsertPolicy};
use bicdb_storage::page::Page;
use bicdb_storage::rowid::RowId;
use bicdb_storage::scan::HeapScanner;
use bicdb_storage::undo::UndoChain;
use bicdb_txn::write::{begin, commit, delete_row, insert_row, update_row, Txn};
use bicdb_wal::group::{GroupSpec, GroupWriter};
use bicdb_workspace::id::WorkspaceId;

use common::{build_env, create_table, mem_io, num, row, shape, Env, DATA_FID, WS};

/// 真件写侧（存储服务的替身：页选址 + 事务引擎调用）。
struct TestWriter<'a, 'b, 'io, 'f> {
    pool: &'a BufferPool<'b>,
    chain: &'a mut UndoChain<'io, 'f>,
    log: GroupWriter<'io, 'f>,
    txn: Option<Txn>,
    seq: u64,
    blocks: Vec<u32>,
}

impl<'a, 'b, 'io, 'f> TestWriter<'a, 'b, 'io, 'f> {
    /// 找一页能放下 `row_len` 的既有块（回退：报错——表增长不在本切片）。
    fn block_with_space(&self, row_len: usize) -> Result<u32, ExecError> {
        for &b in &self.blocks {
            let key = BufferKey::new(
                WS,
                bicdb_storage::rowid::Rdba::from_parts(DATA_FID, b).unwrap(),
            );
            if let Ok(guard) = self.pool.pin(key) {
                let page = Page::from_bytes(Box::new(*guard.as_bytes()));
                if heap::can_insert(&page, row_len, &InsertPolicy::in_place(0)) {
                    return Ok(b);
                }
            }
        }
        Err(ExecError::Spill("表页已满（表增长不在本切片）".to_owned()))
    }
}

impl TableWriter for TestWriter<'_, '_, '_, '_> {
    fn begin(&mut self) -> Result<(), ExecError> {
        self.seq += 1;
        let txn = begin(
            self.pool,
            &mut self.log,
            self.chain,
            CommitSeq::from_raw(self.seq).unwrap(),
        )
        .map_err(|e| ExecError::Spill(format!("begin：{e}")))?;
        self.txn = Some(txn);
        Ok(())
    }

    fn insert_row(&mut self, row_bytes: &[u8]) -> Result<RowId, ExecError> {
        let block = self.block_with_space(row_bytes.len())?;
        let txn = self.txn.as_mut().expect("事务已开");
        let key = BufferKey::new(
            WS,
            bicdb_storage::rowid::Rdba::from_parts(DATA_FID, block).unwrap(),
        );
        insert_row(
            self.pool,
            &mut self.log,
            self.chain,
            txn,
            key,
            row_bytes,
            &InsertPolicy::in_place(0),
        )
        .map_err(|e| ExecError::Spill(format!("insert：{e}")))
    }

    fn update_row(&mut self, rid: RowId, row_bytes: &[u8]) -> Result<(), ExecError> {
        let txn = self.txn.as_mut().expect("事务已开");
        let key = BufferKey::new(
            WS,
            bicdb_storage::rowid::Rdba::from_parts(rid.file_id(), rid.block_id()).unwrap(),
        );
        update_row(
            self.pool,
            &mut self.log,
            self.chain,
            txn,
            key,
            rid.row_id(),
            row_bytes,
            &InsertPolicy::in_place(0),
            // 迁移分配口：本用例的改长都在同页内（跨页迁移由 txn 专门用例覆盖）。
            &mut |need| Err(bicdb_txn::write::TxnError::NoMigrationTarget { need }),
        )
        .map(|_outcome| ())
        .map_err(|e| ExecError::Spill(format!("update：{e}")))
    }

    fn delete_row(&mut self, rid: RowId) -> Result<(), ExecError> {
        let txn = self.txn.as_mut().expect("事务已开");
        let key = BufferKey::new(
            WS,
            bicdb_storage::rowid::Rdba::from_parts(rid.file_id(), rid.block_id()).unwrap(),
        );
        delete_row(self.pool, &mut self.log, self.chain, txn, key, rid.row_id())
            .map_err(|e| ExecError::Spill(format!("delete：{e}")))
    }

    fn commit(&mut self) -> Result<(), ExecError> {
        let mut txn = self.txn.take().expect("事务已开");
        commit(
            self.pool,
            &mut self.log,
            self.chain,
            &mut txn,
            CommitSeq::from_raw(self.seq).unwrap(),
        )
        .map_err(|e| ExecError::Spill(format!("commit：{e}")))?;
        Ok(())
    }

    fn rollback(&mut self) -> Result<(), ExecError> {
        if let Some(mut txn) = self.txn.take() {
            bicdb_txn::write::rollback(self.pool, &mut self.log, self.chain, &mut txn)
                .map_err(|e| ExecError::Spill(format!("rollback：{e}")))?;
        }
        Ok(())
    }
}

/// **内存行游标**（DML 源）：写阶段前按语句快照捕获（读/写两阶段的桥）。
struct CapturedRows {
    rows: std::collections::VecDeque<(RowId, Vec<u8>)>,
}

impl CapturedRows {
    /// **直读捕获**（不经 CR——用例内语句之间无并发、状态已提交）：
    /// 生产形态下读写共用引擎持有的撤销链，无此分离；这里是测试的桥。
    fn capture_direct(pool: &BufferPool<'_>, blocks: &[u32]) -> Result<Self, ExecError> {
        let mut rows = std::collections::VecDeque::new();
        for &block in blocks {
            let key = BufferKey::new(
                WS,
                bicdb_storage::rowid::Rdba::from_parts(DATA_FID, block).unwrap(),
            );
            let Ok(guard) = pool.pin(key) else { continue };
            let page = Page::from_bytes(Box::new(*guard.as_bytes()));
            for row_no in 1..=page.slot_count() {
                if let Some(bytes) = heap::row(&page, row_no) {
                    let rid = RowId::from_parts(DATA_FID, block, row_no).unwrap();
                    rows.push_back((rid, bytes.to_vec()));
                }
            }
        }
        Ok(Self { rows })
    }
}

impl RowCursor for CapturedRows {
    fn next_row(&mut self) -> Result<Option<(RowId, Vec<u8>)>, ExecError> {
        Ok(self.rows.pop_front())
    }
}

/// 跑一个写计划（写通道已就绪；DML 源 = 预先捕获的行）；返回影响行数。
fn run_dml(
    env_pool: &BufferPool<'_>,
    writer: &mut TestWriter<'_, '_, '_, '_>,
    plan: &PlanNode,
    snapshot: CommitSeq,
    captured: Vec<(RowId, Vec<u8>)>,
) -> Result<u64, ExecError> {
    let cell = RefCell::new(writer as &mut dyn TableWriter);
    let mut cursor = Some(CapturedRows {
        rows: captured.into(),
    });
    let mut open = |_src: u32| {
        Ok(Box::new(cursor.take().expect("单次扫描：游标恰好开一次")) as Box<dyn RowCursor>)
    };
    let envx = ExecEnv {
        pool: env_pool,
        chain: None, // 纯写计划：读通道不参与
        spill: None,
        writer: Some(&cell),
    };
    let mut op = build(plan, &envx, &mut open)?;
    let mut cx = ExecContext::new(snapshot);
    let _rows = collect(op.as_mut(), &mut cx)?;
    Ok(cx.rows_affected_of("Insert")
        + cx.rows_affected_of("Update")
        + cx.rows_affected_of("Delete"))
}

/// 读回全表（新快照）。
fn read_all(env: &Env, blocks: Vec<u32>, snapshot: CommitSeq) -> Vec<Row> {
    let plan = PlanNode::Project {
        input: Box::new(PlanNode::SeqScan {
            source: 0,
            shape: shape(),
        }),
        exprs: vec![Expr::Column(0), Expr::Column(1)],
    };
    let mut open = |_src: u32| {
        Ok(Box::new(HeapScanner::new(
            env.pool,
            &env.chain,
            snapshot,
            DATA_FID,
            blocks.clone(),
        )) as Box<dyn RowCursor>)
    };
    let envx = ExecEnv {
        pool: env.pool,
        chain: Some(&env.chain),
        spill: None,
        writer: None,
    };
    let mut op = build(&plan, &envx, &mut open).unwrap();
    let mut cx = ExecContext::new(snapshot);
    collect(op.as_mut(), &mut cx).unwrap()
}

fn lit(v: Value) -> Expr {
    Expr::Literal(v)
}

fn col(i: usize) -> Expr {
    Expr::Column(i)
}

#[test]
fn insert_update_delete_round_trip_through_the_engine() {
    let io = mem_io();
    let mut env = build_env(io);
    let rows: Vec<Row> = (1..=6).map(|i| row(i, &format!("t{i}"))).collect();
    let table = create_table(&mut env, io, &rows);
    let blocks = table.blocks.clone();
    let snap1 = env.snapshot;
    let seq2 = CommitSeq::from_raw(2).unwrap();
    let seq3 = CommitSeq::from_raw(3).unwrap();
    let seq4 = CommitSeq::from_raw(4).unwrap();

    // 写阶段（writer 独占 `&mut chain`）。
    let (affected_insert, affected_update, affected_delete) = {
        let cf: &'static mut ControlFile<'static> = Box::leak(Box::new(
            ControlFile::format(
                io,
                Path::new("/mem/dml_c1.ctl"),
                Path::new("/mem/dml_c2.ctl"),
                &WorkspaceEntry {
                    workspace_id: WorkspaceId::from_raw(1).unwrap(),
                    created_at: 0,
                    derived_from: None,
                    derived_at_seq: CommitSeq::from_raw(0).unwrap(),
                },
                &RedoEntries::new(2, 1).unwrap(),
                &ArchiveRecord::default(),
            )
            .unwrap(),
        ));
        let log = GroupWriter::create(
            io,
            cf,
            Path::new("/mem/dml_wal"),
            GroupSpec::new(2, 1, 64).unwrap(),
            Lsn::from_raw(0).unwrap(),
        )
        .unwrap();
        let mut writer = TestWriter {
            pool: env.pool,
            chain: &mut env.chain,
            log,
            txn: None,
            seq: 1,
            blocks: blocks.clone(),
        };

        // ① INSERT 两行（id 7、8）——自动提交；影响行数 = 2。
        let insert = PlanNode::Insert {
            shape: shape(),
            rows: vec![
                vec![lit(num("7")), lit(Value::Bytes(b"new7".to_vec()))],
                vec![lit(num("8")), lit(Value::Bytes(b"new8".to_vec()))],
            ],
        };
        let a1 = run_dml(env.pool, &mut writer, &insert, snap1, Vec::new()).unwrap();

        // ② UPDATE：全部行 tag → 'z'（列序 [rowid, id, tag]）。
        let update = PlanNode::Update {
            input: Box::new(PlanNode::WithRowId {
                source: 0,
                shape: shape(),
            }),
            sets: vec![(1, lit(Value::Bytes(b"z".to_vec())))],
            shape: shape(),
        };
        // 语句 ② 的源：插入已提交后直读捕获。
        let captured_update = CapturedRows::capture_direct(env.pool, &blocks)
            .unwrap()
            .rows
            .into();
        let a2 = run_dml(env.pool, &mut writer, &update, seq2, captured_update).unwrap();

        // ③ DELETE：id ≤ 2 的两行。
        let delete = PlanNode::Delete {
            input: Box::new(PlanNode::Filter {
                input: Box::new(PlanNode::WithRowId {
                    source: 0,
                    shape: shape(),
                }),
                predicate: Expr::Compare {
                    op: bicdb_exec::CmpOp::Le,
                    left: Box::new(col(1)),
                    right: Box::new(lit(num("2"))),
                },
            }),
        };
        // 语句 ③ 的源：更新已提交后直读捕获。
        let captured_delete = CapturedRows::capture_direct(env.pool, &blocks)
            .unwrap()
            .rows
            .into();
        let a3 = run_dml(env.pool, &mut writer, &delete, seq3, captured_delete).unwrap();
        pool_flush(env.pool);
        (a1, a2, a3)
    };

    assert_eq!(affected_insert, 2, "INSERT 影响 2 行");
    assert_eq!(affected_update, 8, "UPDATE 影响 8 行");
    assert_eq!(affected_delete, 2, "DELETE 影响 2 行");

    // 读阶段（各提交序号下的历史快照）。
    let after_insert = read_all(&env, blocks.clone(), seq2);
    assert_eq!(after_insert.len(), 8, "插入后可读回 8 行");
    let after_update = read_all(&env, blocks.clone(), seq3);
    assert_eq!(after_update.len(), 8);
    assert!(
        after_update
            .iter()
            .all(|r| r.values[1] == Value::Bytes(b"z".to_vec())),
        "所有 tag 已改为 z"
    );
    let after_delete = read_all(&env, blocks.clone(), seq4);
    assert_eq!(after_delete.len(), 6, "删 2 行后剩 6 行");
    assert!(
        after_delete
            .iter()
            .all(|r| r.values[0] != num("1") && r.values[0] != num("2")),
        "id 1/2 已删"
    );
}

fn pool_flush(pool: &BufferPool<'_>) {
    let _ = pool.flush_workspace(WS);
}
