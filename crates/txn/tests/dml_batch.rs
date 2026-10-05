//! **批 DML 的真实路径曲线（P1 验收③）**：撤销页 no-force 前/后的单线程吞吐。
//!
//! ```text
//! cargo test -p bicdb-txn --release --test dml_batch -- --ignored --nocapture
//! ```
//!
//! 两种形态**同一夹具**对拍：
//!
//! - **旧规**：每条 `insert_row` 之后 `pool.flush_workspace`（= P1 前的"每页
//!   flush 让文件与池一致"；等价成本是每条 DML 2 次前台 pwrite——撤销记录页 +
//!   段头页）；
//! - **P1**：不 flush（撤销页 no-force；耐久性由 WAL 规则 2 + 重放保证）。
//!
//! 注意介质是 `MemFileIo`：**次数差是硬结论**（探针给出 2/条 → 0），墙钟差只
//! 反映"每次 flush 的页处理 + 路径开销"，真实块设备上 pwrite 的代价更高。

use std::path::Path;
use std::time::Instant;

use bicdb_common::seq::{CommitSeq, Lsn};
use bicdb_storage::buffer::{BufferKey, BufferPool, CacheConfig, SystemClock, WalGuard};
use bicdb_storage::controlfile::{ArchiveRecord, ControlFile, RedoEntries};
use bicdb_storage::datafile::DataFile;
use bicdb_storage::heap::{HeapError, InsertPolicy};
use bicdb_storage::page::{Page, PageType};
use bicdb_storage::row::assemble_row;
use bicdb_storage::rowid::Rdba;
use bicdb_storage::undo::{create_undo_segment, UndoChain};
use bicdb_txn::write::{begin, commit, insert_row, TxnError};
use bicdb_wal::group::{GroupSpec, GroupWriter};
use bicdb_workspace::io::MemFileIo;

const WS: [u8; 8] = [7u8; 8];
const UNDO_F: &str = "/mem/undo.dat";
const DATA_F: &str = "/mem/data.dat";
const WAL: &str = "/mem/x.log";
const A: &str = "/mem/ctl.a";
const B: &str = "/mem/ctl.b";

struct FakeWal;
impl WalGuard for FakeWal {
    fn durable_lsn(&self) -> Lsn {
        Lsn::from_raw(u64::MAX >> 16).expect("域内")
    }
    fn ensure_durable(&self, _t: Lsn) -> std::io::Result<()> {
        Ok(())
    }
}

fn row_bytes(payload: &[u8]) -> Vec<u8> {
    assemble_row(0, 1, &[false], &[], &[payload]).expect("组装")
}

fn rdba(file: u16, block: u32) -> Rdba {
    Rdba::from_parts(file, block).expect("域内")
}

/// 跑一轮：`count` 条 insert + 一次提交；`per_record_flush` = 旧规形态。
fn run(io: &MemFileIo, count: usize, per_record_flush: bool) -> f64 {
    let mut undo_file = DataFile::create(io, Path::new(UNDO_F), 1, 1, WS, 512).expect("undo");
    let undo_handle = undo_file.handle();
    let segment = create_undo_segment(&mut undo_file, 2, 3, 4).expect("段");
    // 预格式化若干数据页（批 DML 填满一页就换下一页；页满由写路径**预检**
    // 拒绝——不留幽灵撤销记录，换键重试即可）。
    const DATA_PAGES: u32 = 64;
    let data_handle = {
        let data_file = DataFile::create(io, Path::new(DATA_F), 3, 3, WS, 512).expect("data");
        let h = data_file.handle();
        for block in 1..=DATA_PAGES {
            let mut page = Page::new(PageType::HeapTable, WS, 3, block);
            bicdb_storage::pagefile::write_page(io, h, block, &mut page).expect("写页");
        }
        h
    };
    let pool = BufferPool::with_config(
        io,
        8,
        move |_ws, r| match r.file_id() {
            1 => Some((undo_handle, r.block_id())),
            3 => Some((data_handle, r.block_id())),
            _ => None,
        },
        FakeWal,
        SystemClock,
        CacheConfig::for_capacity(8),
    )
    .expect("池");
    let mut chain = UndoChain::open(segment).with_pool(&pool);
    let mut cf = ControlFile::format(
        io,
        Path::new(A),
        Path::new(B),
        &bicdb_storage::controlfile::WorkspaceEntry {
            workspace_id: bicdb_workspace::id::WorkspaceId::from_raw(1).expect("域内"),
            created_at: 0,
            derived_from: None,
            derived_at_seq: CommitSeq::from_raw(0).expect("域内"),
        },
        &RedoEntries::new(8, 1).expect("域内"),
        &ArchiveRecord::default(),
    )
    .expect("控制文件");
    let mut log = GroupWriter::create(
        io,
        &mut cf,
        Path::new(WAL),
        GroupSpec::new(8, 1, 512).expect("组"),
        Lsn::from_raw(0).expect("域内"),
    )
    .expect("日志");

    let start = Instant::now();
    let mut txn = begin(
        &pool,
        &mut log,
        &mut chain,
        CommitSeq::from_raw(1).expect("域内"),
    )
    .expect("begin");
    let mut page_no = 1u32;
    for i in 0..count {
        let row = row_bytes(format!("row-{i:08}").as_bytes());
        loop {
            let key = BufferKey::new(WS, rdba(3, page_no));
            match insert_row(
                &pool,
                &mut log,
                &mut chain,
                &mut txn,
                key,
                &row,
                &InsertPolicy::in_place(0),
            ) {
                Ok(_) => break,
                Err(TxnError::Heap(HeapError::PageFull)) => {
                    page_no += 1;
                    assert!(page_no <= DATA_PAGES, "数据页不够（调大 DATA_PAGES）");
                }
                Err(e) => panic!("insert: {e}"),
            }
            if per_record_flush {
                pool.flush_workspace(WS).expect("flush");
            }
        }
        if per_record_flush {
            pool.flush_workspace(WS).expect("flush");
        }
    }
    commit(
        &pool,
        &mut log,
        &mut chain,
        &mut txn,
        CommitSeq::from_raw(1).expect("域内"),
    )
    .expect("commit");
    let secs = start.elapsed().as_secs_f64();
    count as f64 / secs
}

#[test]
#[ignore = "本地基准（跑法见模块文档）；不进 CI"]
fn dml_batch_p1_ab() {
    let count: usize = std::env::var("DML_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000);
    println!("| 形态 | 条数 | 吞吐（条/秒） | 相对 |");
    println!("| --- | --- | --- | --- |");
    let mut base = 0.0f64;
    for round in 0..2 {
        let io = MemFileIo::new();
        io.add_dir("/mem");
        let old = run(&io, count, true);
        let io2 = MemFileIo::new();
        io2.add_dir("/mem");
        let new = run(&io2, count, false);
        println!("| 旧规（每条 flush） | {count} | {old:.0} | 1.00 |");
        println!(
            "| P1（no-force） | {count} | {new:.0} | {:.2}× |",
            new / old
        );
        if round == 1 {
            base = new / old;
        }
    }
    println!("（第二轮比值 {base:.2}×；介质 MemFileIo——次数差是硬结论，墙钟差为下界）");
}
