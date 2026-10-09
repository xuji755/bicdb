use bicdb_common::seq::{CommitSeq, Lsn};
use bicdb_storage::adjacency::{self as layout, Edge};
use bicdb_storage::buffer::{BufferKey, BufferPool};
use bicdb_storage::controlfile::{ArchiveRecord, ControlFile, RedoEntries, WorkspaceEntry};
use bicdb_storage::cr::{reconstruct, ReadView};
use bicdb_storage::datafile::DataFile;
use bicdb_storage::heap::InsertPolicy;
use bicdb_storage::page::{Page, PageType};
use bicdb_storage::pagefile;
use bicdb_storage::rowid::{Rdba, RowId};
use bicdb_storage::undo::{create_undo_segment, UndoChain};
use bicdb_txn::write::{self as write, adjacency as access, TxnError};
use bicdb_wal::group::{GroupSpec, GroupWriter};
use bicdb_workspace::id::WorkspaceId;
use bicdb_workspace::io::MemFileIo;
use std::path::Path;

const WS: [u8; 8] = [33; 8];
fn seq(n: u64) -> CommitSeq {
    CommitSeq::from_raw(n).unwrap()
}
fn lsn(n: u64) -> Lsn {
    Lsn::from_raw(n).unwrap()
}
fn key(block: u32) -> BufferKey {
    BufferKey::new(WS, Rdba::from_parts(3, block).unwrap())
}
fn source() -> RowId {
    RowId::from_parts(3, 99, 1).unwrap()
}
fn bytes(id: u64, payload: &[u8]) -> Vec<u8> {
    Edge {
        id,
        destination: source(),
        flags: 0,
        itl_slot: 0xFF,
        label: "LINK",
        payload,
    }
    .encode()
    .unwrap()
}
fn snapshot(pool: &BufferPool<'_>, block: BufferKey) -> Page {
    let guard = pool.pin(block).unwrap();
    Page::from_bytes(Box::new(*guard.as_bytes()))
}
fn mem() -> MemFileIo {
    let io = MemFileIo::new();
    io.add_dir("/adj");
    io.add_dir("/adj/wal");
    io
}
fn fixture(
    io: &MemFileIo,
    action: impl FnOnce(&BufferPool<'_>, &mut UndoChain<'_, '_>, &mut GroupWriter<'_, '_>, u32),
) {
    let mut undo = DataFile::create(io, Path::new("/adj/undo"), 1, 1, WS, 512).unwrap();
    let undo_handle = undo.handle();
    let segment = create_undo_segment(&mut undo, 2, 3, 4).unwrap();
    let segment_block = segment.page0_block();
    let data = DataFile::create(io, Path::new("/adj/data"), 3, 3, WS, 512).unwrap();
    let data_handle = data.handle();
    for block in 1..=2 {
        let mut page = Page::new(PageType::Adjacency, WS, 3, block);
        layout::initialize(&mut page, source()).unwrap();
        pagefile::write_page(io, data_handle, block, &mut page).unwrap();
    }
    let entry = WorkspaceEntry {
        workspace_id: WorkspaceId::from_raw(1).unwrap(),
        created_at: 0,
        derived_from: None,
        derived_at_seq: seq(0),
    };
    let mut cf = ControlFile::format(
        io,
        Path::new("/adj/c1"),
        Path::new("/adj/c2"),
        &entry,
        &RedoEntries::new(2, 1).unwrap(),
        &ArchiveRecord::default(),
    )
    .unwrap();
    let mut log = GroupWriter::create(
        io,
        &mut cf,
        Path::new("/adj/wal"),
        GroupSpec::new(2, 1, 64).unwrap(),
        lsn(0),
    )
    .unwrap();
    let pool = BufferPool::new(
        io,
        128,
        move |_ws, r| match r.file_id() {
            1 => Some((undo_handle, r.block_id())),
            3 => Some((data_handle, r.block_id())),
            _ => None,
        },
        log.shared(),
    )
    .unwrap();
    let mut chain = UndoChain::open(segment).with_pool(&pool);
    action(&pool, &mut chain, &mut log, segment_block);
    log.flush(log.appended_lsn()).unwrap();
    pool.flush_workspace(WS).unwrap();
}

#[test]
fn adjacency_visibility_growth_delete_and_statement_rollback_keep_prior_work() {
    let io = mem();
    fixture(&io, |pool, chain, log, _| {
        let policy = InsertPolicy::in_place(0);
        let mut first = write::begin(pool, log, chain, seq(1)).unwrap();
        let id = access::insert(
            pool,
            log,
            chain,
            &mut first,
            key(1),
            &bytes(10, b"old"),
            &policy,
        )
        .unwrap();
        let page = snapshot(pool, key(1));
        let other = reconstruct(&page, ReadView::new(seq(0)), chain).unwrap();
        assert!(layout::record(&other, id.row_id()).is_err());
        let mine = reconstruct(
            &page,
            ReadView::new(seq(0)).with_own(Some(first.txn_id)),
            chain,
        )
        .unwrap();
        assert_eq!(
            layout::decode(layout::record(&mine, id.row_id()).unwrap())
                .unwrap()
                .payload,
            b"old"
        );
        write::commit(pool, log, chain, &mut first, seq(1)).unwrap();
        let mut next = write::begin(pool, log, chain, seq(2)).unwrap();
        access::insert(
            pool,
            log,
            chain,
            &mut next,
            key(1),
            &bytes(20, b"earlier statement"),
            &policy,
        )
        .unwrap();
        let mark = write::statement_mark(chain, &next).unwrap();
        access::update(
            pool,
            log,
            chain,
            &mut next,
            key(1),
            id.row_id(),
            &bytes(10, &[b'g'; 1000]),
            &policy,
        )
        .unwrap();
        access::delete(pool, log, chain, &mut next, key(1), id.row_id(), &policy).unwrap();
        access::compact(pool, log, &next, key(1)).unwrap();
        let page = snapshot(pool, key(1));
        let old = reconstruct(&page, ReadView::new(seq(1)), chain).unwrap();
        assert_eq!(layout::header(&old).unwrap().live_edges, 1);
        assert_eq!(
            layout::decode(layout::record(&old, id.row_id()).unwrap())
                .unwrap()
                .payload,
            b"old"
        );
        write::rollback_to_mark(pool, log, chain, &mut next, mark).unwrap();
        let page = snapshot(pool, key(1));
        layout::validate(&page).unwrap();
        assert_eq!(layout::header(&page).unwrap().live_edges, 2);
        assert_eq!(
            layout::decode(layout::record(&page, id.row_id()).unwrap())
                .unwrap()
                .payload,
            b"old"
        );
        write::commit(pool, log, chain, &mut next, seq(2)).unwrap();
    });
}
#[test]
fn adjacency_edge_lock_conflict_and_itl_growth_preserve_record_header() {
    let io = mem();
    fixture(&io, |pool, chain, log, _| {
        let policy = InsertPolicy::in_place(0);
        let mut first = write::begin(pool, log, chain, seq(1)).unwrap();
        let id = access::insert(
            pool,
            log,
            chain,
            &mut first,
            key(1),
            &bytes(10, b"first"),
            &policy,
        )
        .unwrap();
        let mut second = write::begin(pool, log, chain, seq(2)).unwrap();
        let e = access::update(
            pool,
            log,
            chain,
            &mut second,
            key(1),
            id.row_id(),
            &bytes(10, b"blocked"),
            &policy,
        )
        .unwrap_err();
        assert!(matches!(e,TxnError::RowLocked{holder,..} if holder==first.txn_id));
        access::insert(
            pool,
            log,
            chain,
            &mut second,
            key(1),
            &bytes(20, b"second"),
            &policy,
        )
        .unwrap();
        let page = snapshot(pool, key(1));
        assert!(page.header().unwrap().itl_count >= 2);
        assert_eq!(layout::header(&page).unwrap().source, source());
        layout::validate(&page).unwrap();
        let own = reconstruct(
            &page,
            ReadView::new(seq(0)).with_own(Some(second.txn_id)),
            chain,
        )
        .unwrap();
        assert!(layout::find(&own, 10).unwrap().is_none());
        assert_eq!(layout::find(&own, 20).unwrap(), Some(2));
        write::rollback(pool, log, chain, &mut second).unwrap();
        write::commit(pool, log, chain, &mut first, seq(1)).unwrap();
        let page = snapshot(pool, key(1));
        layout::validate(&page).unwrap();
        assert_eq!(layout::header(&page).unwrap().live_edges, 1);
    });
}
#[test]
fn adjacency_link_is_snapshot_visible_and_rollback_unlinks_new_tail() {
    let io = mem();
    fixture(&io, |pool, chain, log, _| {
        let policy = InsertPolicy::in_place(0);
        let mut first = write::begin(pool, log, chain, seq(1)).unwrap();
        access::insert(
            pool,
            log,
            chain,
            &mut first,
            key(1),
            &bytes(10, b"parent"),
            &policy,
        )
        .unwrap();
        write::commit(pool, log, chain, &mut first, seq(1)).unwrap();
        let mut next = write::begin(pool, log, chain, seq(2)).unwrap();
        access::insert(
            pool,
            log,
            chain,
            &mut next,
            key(2),
            &bytes(20, b"tail"),
            &policy,
        )
        .unwrap();
        access::link(
            pool,
            log,
            chain,
            &mut next,
            key(1),
            RowId::page_address(3, 2).unwrap(),
            &policy,
        )
        .unwrap();
        let parent = snapshot(pool, key(1));
        assert!(
            layout::header(&reconstruct(&parent, ReadView::new(seq(1)), chain).unwrap())
                .unwrap()
                .next
                .is_none()
        );
        assert!(layout::header(
            &reconstruct(
                &parent,
                ReadView::new(seq(1)).with_own(Some(next.txn_id)),
                chain
            )
            .unwrap()
        )
        .unwrap()
        .next
        .is_some());
        write::rollback(pool, log, chain, &mut next).unwrap();
        assert!(layout::header(&snapshot(pool, key(1)))
            .unwrap()
            .next
            .is_none());
        assert_eq!(
            layout::header(&snapshot(pool, key(2))).unwrap().live_edges,
            0
        );
    });
}
#[test]
fn adjacency_preflight_errors_leave_data_and_undo_unchanged() {
    let io = mem();
    fixture(&io, |pool, chain, log, _| {
        let policy = InsertPolicy::in_place(0);
        let mut txn = write::begin(pool, log, chain, seq(1)).unwrap();
        let before = *snapshot(pool, key(1)).as_bytes();
        let mark = write::statement_mark(chain, &txn).unwrap().at();
        assert!(access::insert(
            pool,
            log,
            chain,
            &mut txn,
            key(1),
            &bytes(1, &[1; 20000]),
            &policy
        )
        .is_err());
        assert_eq!(write::statement_mark(chain, &txn).unwrap().at(), mark);
        assert_eq!(snapshot(pool, key(1)).as_bytes(), &before);
        access::insert(
            pool,
            log,
            chain,
            &mut txn,
            key(1),
            &bytes(10, b"ok"),
            &policy,
        )
        .unwrap();
        let mark = write::statement_mark(chain, &txn).unwrap().at();
        let before = *snapshot(pool, key(1)).as_bytes();
        assert!(access::insert(
            pool,
            log,
            chain,
            &mut txn,
            key(1),
            &bytes(9, b"unordered"),
            &policy
        )
        .is_err());
        assert_eq!(write::statement_mark(chain, &txn).unwrap().at(), mark);
        assert_eq!(snapshot(pool, key(1)).as_bytes(), &before);
        write::rollback(pool, log, chain, &mut txn).unwrap();
    });
}

#[test]
fn adjacency_committed_edge_and_uncommitted_chain_recover_atomically() {
    let io = mem();
    let mut segment_block = 0;
    fixture(&io, |pool, chain, log, block| {
        segment_block = block;
        let policy = InsertPolicy::in_place(0);
        let mut winner = write::begin(pool, log, chain, seq(1)).unwrap();
        access::insert(
            pool,
            log,
            chain,
            &mut winner,
            key(1),
            &bytes(10, b"winner"),
            &policy,
        )
        .unwrap();
        write::commit(pool, log, chain, &mut winner, seq(1)).unwrap();
        let mut loser = write::begin(pool, log, chain, seq(2)).unwrap();
        access::update(
            pool,
            log,
            chain,
            &mut loser,
            key(1),
            1,
            &bytes(10, &[b'l'; 2000]),
            &policy,
        )
        .unwrap();
        access::insert(
            pool,
            log,
            chain,
            &mut loser,
            key(2),
            &bytes(20, b"loser"),
            &policy,
        )
        .unwrap();
        access::link(
            pool,
            log,
            chain,
            &mut loser,
            key(1),
            RowId::page_address(3, 2).unwrap(),
            &policy,
        )
        .unwrap();
        access::compact(pool, log, &loser, key(1)).unwrap();
        // Persist the uncommitted pages too, then discard all live process state.
        log.flush(log.appended_lsn()).unwrap();
        pool.flush_workspace(WS).unwrap();
    });
    let mut undo = DataFile::open(&io, Path::new("/adj/undo")).unwrap();
    let undo_handle = undo.handle();
    let data = DataFile::open(&io, Path::new("/adj/data")).unwrap();
    let data_handle = data.handle();
    let mut chain =
        UndoChain::open(bicdb_storage::segment::Segment::open(&mut undo, segment_block).unwrap());
    let mut cf = ControlFile::open(&io, Path::new("/adj/c1"), Path::new("/adj/c2")).unwrap();
    let spec = GroupSpec::new(2, 1, 64).unwrap();
    let groups = bicdb_wal::group::online_groups(&io, &cf, Path::new("/adj/wal"), spec).unwrap();
    let mut log = GroupWriter::open(&io, &mut cf, Path::new("/adj/wal"), spec).unwrap();
    let mut resolve = |r: Rdba| match r.file_id() {
        1 => Some((undo_handle, r.block_id())),
        3 => Some((data_handle, r.block_id())),
        _ => None,
    };
    let report =
        bicdb_wal::recovery::recover(&io, &groups, lsn(0), &mut chain, &mut log, &mut resolve)
            .unwrap();
    assert_eq!(report.undo.txns_rolled_back, 1);
    let parent = pagefile::read_page_verified(&io, data_handle, 1).unwrap();
    let tail = pagefile::read_page_verified(&io, data_handle, 2).unwrap();
    layout::validate(&parent).unwrap();
    layout::validate(&tail).unwrap();
    assert!(layout::header(&parent).unwrap().next.is_none());
    assert_eq!(layout::header(&tail).unwrap().live_edges, 0);
    assert_eq!(
        layout::decode(layout::record(&parent, 1).unwrap())
            .unwrap()
            .payload,
        b"winner"
    );
}
