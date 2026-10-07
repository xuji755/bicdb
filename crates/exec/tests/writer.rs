//! **真件写侧验收**：`TableAccessWriter`（`bicdb-access` 的表访问服务）
//! 接 DML 算子——**空表起步**，插入跨多页（**表增长**），读回逐行一致。
//!
//! 与 `dml.rs` 的分工：那里用夹具写"已存在的页"（写通道/事务边界验收）；
//! 这里用**真实现**（页选址 + 增长 + 事务引擎），钉住"表增长"这块此前的空缺。

mod common;

use std::cell::RefCell;

use bicdb_common::seq::{CommitSeq, Lsn};
use bicdb_exec::{
    build, collect, ExecContext, ExecEnv, Expr, PlanNode, Row, RowCursor, TableAccessWriter,
    TableWriter, Value,
};
use bicdb_storage::controlfile::{ArchiveRecord, ControlFile, RedoEntries, WorkspaceEntry};
use bicdb_storage::scan::HeapScanner;
use bicdb_storage::segment::{SegType, Segment};
use bicdb_wal::group::{GroupSpec, GroupWriter};
use bicdb_workspace::id::WorkspaceId;

use common::{build_env, mem_io, num, row, shape, Env, DATA_FID, WS};

use std::path::Path;

/// 建一张**空表**（只建段与段头：没有任何数据页格式化过——增长的起点）。
fn empty_table(env: &mut Env) -> u32 {
    let seg = Segment::create(env.data_file, SegType::Heap, 1, 1, 8, 0, 0).unwrap();
    seg.page0_block()
}

/// 真件写口（`TableWriter` 的实现）。
fn writer<'a>(
    env: &'a mut Env,
    io: &'static bicdb_workspace::io::MemFileIo,
    heap_seg: u32,
    walfile: &str,
    cf_a: &str,
    cf_b: &str,
) -> TableAccessWriter<'a, 'static, 'static, 'static> {
    let cf: &'static mut ControlFile<'static> = Box::leak(Box::new(
        ControlFile::format(
            io,
            Path::new(cf_a),
            Path::new(cf_b),
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
    // 日志写口常驻（`TableAccessWriter` 借它——与真件的 `&'a mut` 口径一致）。
    let log: &'static mut GroupWriter<'static, 'static> = Box::leak(Box::new(
        GroupWriter::create(
            io,
            cf,
            Path::new(walfile),
            GroupSpec::new(2, 1, 8192).unwrap(),
            Lsn::from_raw(0).unwrap(),
        )
        .unwrap(),
    ));
    TableAccessWriter::new(
        env.pool,
        &mut env.chain,
        log,
        env.data_file,
        heap_seg,
        WS,
        1,
    )
}

/// 批量 INSERT（走 DML 算子 + 自动提交）。
fn insert_rows(
    pool: &'static bicdb_storage::buffer::BufferPool<'static>,
    writer: &mut TableAccessWriter<'_, '_, '_, '_>,
    rows: &[Row],
    snapshot: CommitSeq,
) -> u64 {
    let plan = PlanNode::Insert {
        shape: shape(),
        rows: rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(|v| Expr::Literal(v.clone()))
                    .collect::<Vec<_>>()
            })
            .collect(),
    };
    let cell = RefCell::new(writer as &mut dyn TableWriter);
    let mut open = |_src: u32| -> Result<Box<dyn RowCursor>, bicdb_exec::ExecError> {
        unreachable!("INSERT … VALUES 不走行源")
    };
    let envx = ExecEnv {
        pool,
        chain: None,
        spill: None,
        writer: Some(&cell),
    };
    let mut op = build(&plan, &envx, &mut open).unwrap();
    let mut cx = ExecContext::new(snapshot);
    collect(op.as_mut(), &mut cx).unwrap();
    cx.rows_affected_of("Insert")
}

/// 全表读回（真件池 + CR 扫描；扫描边界 = 段 HWM）。
fn read_all(
    env: &mut Env,
    pool: &'static bicdb_storage::buffer::BufferPool<'static>,
    heap_seg: u32,
    snapshot: CommitSeq,
) -> Vec<Row> {
    // **活系统形态**：段头页池优先（no-force：直读文件会拿到旧 hwm）。
    let blocks = {
        let seg = Segment::open_pooled(pool, env.data_file, heap_seg, WS).unwrap();
        let hwm = seg.hwm();
        seg.data_blocks(hwm)
    };
    let plan = PlanNode::Project {
        input: Box::new(PlanNode::SeqScan {
            source: 0,
            shape: shape(),
        }),
        exprs: vec![Expr::Column(0), Expr::Column(1)],
    };
    let mut open = |_src: u32| {
        Ok(Box::new(HeapScanner::new(
            pool,
            &env.chain,
            bicdb_storage::cr::ReadView::new(snapshot),
            DATA_FID,
            blocks.clone(),
        )) as Box<dyn RowCursor>)
    };
    let envx = ExecEnv {
        pool,
        chain: Some(&env.chain),
        spill: None,
        writer: None,
    };
    let mut op = build(&plan, &envx, &mut open).unwrap();
    let mut cx = ExecContext::new(snapshot);
    collect(op.as_mut(), &mut cx).unwrap()
}

#[test]
fn insert_into_an_empty_table_grows_pages_and_reads_back() {
    let io = mem_io();
    let mut env = build_env(io);
    let heap_seg = empty_table(&mut env);
    let snap_before = env.snapshot;

    let pool = env.pool;
    // 空表：读回 0 行。
    assert!(read_all(&mut env, pool, heap_seg, snap_before).is_empty());
    let hwm_before = {
        let seg = Segment::open_pooled(pool, env.data_file, heap_seg, WS).unwrap();
        seg.hwm()
    };

    // 插入 600 行（**跨多页**：表从"没有任何数据页"起步）。
    let rows: Vec<Row> = (1..=600).map(|i| row(i, &format!("t{i:04}"))).collect();
    let pool = env.pool;
    let affected = {
        let mut w = writer(
            &mut env,
            io,
            heap_seg,
            "/mem/w_wal",
            "/mem/w_c1",
            "/mem/w_c2",
        );
        insert_rows(pool, &mut w, &rows, CommitSeq::from_raw(1).unwrap())
    };
    assert_eq!(affected, 600, "影响行数 = 插入行数");

    // 表已增长；读回逐行一致（新快照 = 提交后水位）。
    let snap_after = CommitSeq::from_raw(2).unwrap();
    let hwm_after = {
        let seg = Segment::open_pooled(pool, env.data_file, heap_seg, WS).unwrap();
        seg.hwm()
    };
    assert!(
        hwm_after > hwm_before,
        "表增长：hwm {hwm_before} → {hwm_after}"
    );
    let mut back = read_all(&mut env, pool, heap_seg, snap_after);
    assert_eq!(back.len(), 600, "读回行数");
    back.sort_by(|a, b| format!("{:?}", a.values).cmp(&format!("{:?}", b.values)));
    let mut want = rows.clone();
    want.sort_by(|a, b| format!("{:?}", a.values).cmp(&format!("{:?}", b.values)));
    assert_eq!(back, want, "逐行一致");
}

#[test]
fn update_and_delete_round_trip_on_a_grown_table() {
    let io = mem_io();
    let mut env = build_env(io);
    let heap_seg = empty_table(&mut env);
    let snap1 = CommitSeq::from_raw(1).unwrap();

    let rows: Vec<Row> = (1..=40).map(|i| row(i, &format!("t{i:03}"))).collect();
    let pool = env.pool;
    {
        let mut w = writer(
            &mut env,
            io,
            heap_seg,
            "/mem/w2_wal",
            "/mem/w2_c1",
            "/mem/w2_c2",
        );
        assert_eq!(insert_rows(pool, &mut w, &rows, snap1), 40);
    }
    let snap2 = CommitSeq::from_raw(2).unwrap();

    // 读回取 ROWID（UPDATE/DELETE 的源 = `WithRowId` 形态；这里直接按 id 定位）。
    let some = read_all(&mut env, pool, heap_seg, snap2);
    assert_eq!(some.len(), 40);

    // 按 ROWID 定位（写口要独占 `env`，定位先行）。
    let rid = find_rid(&mut env, heap_seg, snap2, 7);
    let rid2 = find_rid(&mut env, heap_seg, snap2, 8);
    // 走写口的按 ROWID 更新/删除（**等长替换**：不触发迁移）。
    {
        let mut w = writer(
            &mut env,
            io,
            heap_seg,
            "/mem/w2b_wal",
            "/mem/w2b_c1",
            "/mem/w2b_c2",
        );
        w.begin().unwrap();
        let new_row = encode_row(&row(7, "T007"));
        // 旧行 = 插入时的原值（`row(7, "t007")`）——索引维护从它算旧键。
        let old_row = encode_row(&row(7, "t007"));
        w.update_row(rid, &old_row, &new_row).unwrap();
        let old2 = encode_row(&row(8, "t008"));
        w.delete_row(rid2, &old2).unwrap();
        w.commit().unwrap();
    }
    let snap3 = CommitSeq::from_raw(3).unwrap();
    let after = read_all(&mut env, pool, heap_seg, snap3);
    assert_eq!(after.len(), 39, "删除一行后 39 行");
    assert!(
        after
            .iter()
            .any(|r| r.values == vec![num("7"), Value::Bytes(b"T007".to_vec())]),
        "更新后的行可见"
    );
    assert!(
        !after
            .iter()
            .any(|r| r.values == vec![num("8"), Value::Bytes(b"t008".to_vec())]),
        "删除的行不可见"
    );
}

/// 找某个 id 的 ROWID（**真件读路径**：池 + CR 扫描；提交后快照）。
fn find_rid(
    env: &mut Env,
    heap_seg: u32,
    snapshot: CommitSeq,
    id: i64,
) -> bicdb_storage::rowid::RowId {
    use bicdb_exec::decode_row;
    let pool = env.pool;
    let blocks = {
        let seg = Segment::open_pooled(pool, env.data_file, heap_seg, WS).unwrap();
        let hwm = seg.hwm();
        seg.data_blocks(hwm)
    };
    let mut scanner = HeapScanner::new(
        pool,
        &env.chain,
        bicdb_storage::cr::ReadView::new(snapshot),
        DATA_FID,
        blocks,
    );
    while let Some((rid, bytes)) = scanner.next_row().unwrap() {
        let decoded = decode_row(&bytes, &shape()).unwrap();
        if decoded.values[0] == num(&id.to_string()) {
            return rid;
        }
    }
    panic!("找不到 id={id} 的行");
}

/// 行 → 字节（与执行器同法）。
fn encode_row(r: &Row) -> Vec<u8> {
    bicdb_exec::encode_row(r, &shape()).unwrap()
}
