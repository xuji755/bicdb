use bicdb_access::adjacency::{AccessError, AdjacencyAccess, ChainLimits};
use bicdb_access::{create_segment, TableAccess};
use bicdb_common::seq::{CommitSeq, Lsn};
use bicdb_storage::adjacency::{self as layout, Edge};
use bicdb_storage::buffer::{BufferKey, BufferPool};
use bicdb_storage::controlfile::{ArchiveRecord, ControlFile, RedoEntries, WorkspaceEntry};
use bicdb_storage::cr::ReadView;
use bicdb_storage::datafile::DataFile;
use bicdb_storage::heap::InsertPolicy;
use bicdb_storage::page::{Page, PageType};
use bicdb_storage::row::assemble_row;
use bicdb_storage::rowid::{Rdba, RowId};
use bicdb_storage::segment::{SegType, Segment};
use bicdb_storage::undo::{create_undo_segment, UndoChain};
use bicdb_txn::write::{self, TxnError};
use bicdb_wal::group::{GroupSpec, GroupWriter};
use bicdb_workspace::id::WorkspaceId;
use bicdb_workspace::io::MemFileIo;
use std::path::Path;

const WS: [u8; 8] = [34; 8];
fn seq(n: u64) -> CommitSeq {
    CommitSeq::from_raw(n).unwrap()
}
fn lsn(n: u64) -> Lsn {
    Lsn::from_raw(n).unwrap()
}
fn key(address: RowId) -> BufferKey {
    BufferKey::new(
        WS,
        Rdba::from_parts(address.file_id(), address.block_id()).unwrap(),
    )
}
fn edge(id: u64, destination: RowId, size: usize) -> Vec<u8> {
    Edge {
        id,
        destination,
        flags: 0,
        itl_slot: 0xFF,
        label: "USES",
        payload: &vec![b'p'; size],
    }
    .encode()
    .unwrap()
}
fn snapshot(pool: &BufferPool<'_>, address: RowId) -> Page {
    let guard = pool.pin(key(address)).unwrap();
    Page::from_bytes(Box::new(*guard.as_bytes()))
}
fn memory() -> MemFileIo {
    let io = MemFileIo::new();
    io.add_dir("/access");
    io.add_dir("/access/wal");
    io
}
#[derive(Clone, Copy)]
struct Fixture {
    undo: u32,
    heap: u32,
    graph: u32,
    other_graph: u32,
    source: RowId,
    destination: RowId,
}
fn fixture(
    io: &MemFileIo,
    action: impl FnOnce(
        &BufferPool<'_>,
        &mut UndoChain<'_, '_>,
        &mut GroupWriter<'_, '_>,
        &mut DataFile<'_>,
        Fixture,
    ),
) {
    let mut undo = DataFile::create(io, Path::new("/access/undo"), 1, 1, WS, 512).unwrap();
    let undo_handle = undo.handle();
    let segment = create_undo_segment(&mut undo, 2, 3, 4).unwrap();
    let undo_block = segment.page0_block();
    let mut data = DataFile::create(io, Path::new("/access/data"), 3, 3, WS, 4096).unwrap();
    let data_handle = data.handle();
    let entry = WorkspaceEntry {
        workspace_id: WorkspaceId::from_raw(1).unwrap(),
        created_at: 0,
        derived_from: None,
        derived_at_seq: seq(0),
    };
    let mut cf = ControlFile::format(
        io,
        Path::new("/access/c1"),
        Path::new("/access/c2"),
        &entry,
        &RedoEntries::new(2, 1).unwrap(),
        &ArchiveRecord::default(),
    )
    .unwrap();
    let mut log = GroupWriter::create(
        io,
        &mut cf,
        Path::new("/access/wal"),
        GroupSpec::new(2, 1, 512).unwrap(),
        lsn(0),
    )
    .unwrap();
    let pool = BufferPool::new(
        io,
        128,
        move |_, r| match r.file_id() {
            1 => Some((undo_handle, r.block_id())),
            3 => Some((data_handle, r.block_id())),
            _ => None,
        },
        log.shared(),
    )
    .unwrap();
    let mut chain = UndoChain::open(segment).with_pool(&pool);
    let mut setup = write::begin(&pool, &mut log, &mut chain, seq(1)).unwrap();
    let heap = create_segment(
        &pool,
        &mut log,
        &setup,
        &mut data,
        WS,
        SegType::Heap,
        10,
        10,
        8,
        0,
        0,
    )
    .unwrap();
    let graph = create_segment(
        &pool,
        &mut log,
        &setup,
        &mut data,
        WS,
        SegType::Adjacency,
        20,
        20,
        8,
        0,
        0,
    )
    .unwrap();
    let other_graph = create_segment(
        &pool,
        &mut log,
        &setup,
        &mut data,
        WS,
        SegType::Adjacency,
        21,
        21,
        8,
        0,
        0,
    )
    .unwrap();
    let mut table = TableAccess::new(&pool, WS);
    let policy = InsertPolicy::in_place(0);
    let mut insert = |name: &[u8]| {
        table
            .insert(
                &mut log,
                &mut chain,
                &mut setup,
                &mut data,
                heap,
                &assemble_row(0, 0xFF, &[false], &[], &[name]).unwrap(),
                &policy,
            )
            .unwrap()
    };
    let source = insert(b"source");
    let destination = insert(b"destination");
    write::commit(&pool, &mut log, &mut chain, &mut setup, seq(1)).unwrap();
    action(
        &pool,
        &mut chain,
        &mut log,
        &mut data,
        Fixture {
            undo: undo_block,
            heap,
            graph,
            other_graph,
            source,
            destination,
        },
    );
    log.flush(log.appended_lsn()).unwrap();
    pool.flush_workspace(WS).unwrap();
}

#[test]
fn adjacency_segment_growth_uses_type_five_and_snapshot_reads_all_pages() {
    let io = memory();
    fixture(&io, |pool, chain, log, data, f| {
        let access = AdjacencyAccess::new(pool, WS);
        let limits = ChainLimits::default();
        let mut txn = write::begin(pool, log, chain, seq(2)).unwrap();
        let mut head = None;
        access
            .lock_endpoints(log, chain, &txn, f.source, f.destination)
            .unwrap();
        for id in 1..=40 {
            let result = access
                .append(
                    log,
                    chain,
                    &mut txn,
                    data,
                    f.graph,
                    f.source,
                    head,
                    &edge(id, f.destination, 1200),
                    limits,
                )
                .unwrap();
            head = Some(result.head);
            assert_eq!(
                snapshot(pool, result.edge).header().unwrap().page_type,
                PageType::Adjacency
            );
        }
        assert!(access
            .read(
                data,
                f.graph,
                f.source,
                head,
                ReadView::new(seq(1)),
                chain,
                limits
            )
            .unwrap()
            .is_empty());
        let own = access
            .read(
                data,
                f.graph,
                f.source,
                head,
                ReadView::new(seq(1)).with_own(Some(txn.txn_id)),
                chain,
                limits,
            )
            .unwrap();
        assert_eq!(own.len(), 40);
        assert!(own
            .windows(2)
            .all(|w| layout::decode(&w[0].1).unwrap().id < layout::decode(&w[1].1).unwrap().id));
        write::commit(pool, log, chain, &mut txn, seq(2)).unwrap();
        assert_eq!(
            access
                .read(
                    data,
                    f.graph,
                    f.source,
                    head,
                    ReadView::new(seq(2)),
                    chain,
                    limits
                )
                .unwrap()
                .len(),
            40
        );
        let segment = Segment::open_pooled(pool, data, f.graph, WS).unwrap();
        assert_eq!(segment.header().seg_type, SegType::Adjacency);
        assert!(segment.data_blocks(segment.hwm()).len() >= 3);
    });
}

#[test]
fn adjacency_tail_growth_statement_rollback_does_not_lose_committed_edges() {
    let io = memory();
    fixture(&io, |pool, chain, log, data, f| {
        let access = AdjacencyAccess::new(pool, WS);
        let limits = ChainLimits::default();
        let mut first = write::begin(pool, log, chain, seq(2)).unwrap();
        let head = access
            .append(
                log,
                chain,
                &mut first,
                data,
                f.graph,
                f.source,
                None,
                &edge(1, f.destination, 13000),
                limits,
            )
            .unwrap()
            .head;
        write::commit(pool, log, chain, &mut first, seq(2)).unwrap();
        let mut next = write::begin(pool, log, chain, seq(3)).unwrap();
        let mark = write::statement_mark(chain, &next).unwrap();
        access
            .append(
                log,
                chain,
                &mut next,
                data,
                f.graph,
                f.source,
                Some(head),
                &edge(2, f.destination, 13000),
                limits,
            )
            .unwrap();
        assert_eq!(
            access
                .read(
                    data,
                    f.graph,
                    f.source,
                    Some(head),
                    ReadView::new(seq(2)),
                    chain,
                    limits
                )
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            access
                .read(
                    data,
                    f.graph,
                    f.source,
                    Some(head),
                    ReadView::new(seq(2)).with_own(Some(next.txn_id)),
                    chain,
                    limits
                )
                .unwrap()
                .len(),
            2
        );
        write::rollback_to_mark(pool, log, chain, &mut next, mark).unwrap();
        assert!(layout::header(&snapshot(pool, head))
            .unwrap()
            .next
            .is_none());
        access
            .append(
                log,
                chain,
                &mut next,
                data,
                f.graph,
                f.source,
                Some(head),
                &edge(3, f.destination, 13000),
                limits,
            )
            .unwrap();
        write::commit(pool, log, chain, &mut next, seq(3)).unwrap();
        let ids: Vec<_> = access
            .read(
                data,
                f.graph,
                f.source,
                Some(head),
                ReadView::new(seq(3)),
                chain,
                limits,
            )
            .unwrap()
            .into_iter()
            .map(|(_, b)| layout::decode(&b).unwrap().id)
            .collect();
        assert_eq!(ids, [1, 3]);
    });
}

#[test]
fn adjacency_endpoint_delete_conflicts_and_missing_endpoint_never_allocates_edge() {
    let io = memory();
    fixture(&io, |pool, chain, log, data, f| {
        let access = AdjacencyAccess::new(pool, WS);
        let limits = ChainLimits::default();
        let before_hwm = Segment::open_pooled(pool, data, f.graph, WS).unwrap().hwm();
        let mut deleting = write::begin(pool, log, chain, seq(2)).unwrap();
        write::lock_row(
            pool,
            log,
            chain,
            &deleting,
            key(f.destination),
            f.destination.row_id(),
            &InsertPolicy::in_place(0),
        )
        .unwrap();
        let mut creating = write::begin(pool, log, chain, seq(3)).unwrap();
        let mark = write::statement_mark(chain, &creating).unwrap();
        let error = access
            .append(
                log,
                chain,
                &mut creating,
                data,
                f.graph,
                f.source,
                None,
                &edge(1, f.destination, 100),
                limits,
            )
            .unwrap_err();
        assert!(
            matches!(error, AccessError::Native(bicdb_access::TableAccessError::Txn(TxnError::RowLocked { holder, .. })) if holder == deleting.txn_id),
            "{error:?}"
        );
        write::rollback_to_mark(pool, log, chain, &mut creating, mark).unwrap();
        TableAccess::new(pool, WS)
            .delete(
                log,
                chain,
                &mut deleting,
                data,
                f.destination,
                &InsertPolicy::in_place(0),
            )
            .unwrap();
        assert!(access
            .append(
                log,
                chain,
                &mut creating,
                data,
                f.graph,
                f.source,
                None,
                &edge(2, f.destination, 100),
                limits
            )
            .is_err());
        write::rollback_to_mark(pool, log, chain, &mut creating, mark).unwrap();
        write::commit(pool, log, chain, &mut deleting, seq(2)).unwrap();
        assert!(access
            .append(
                log,
                chain,
                &mut creating,
                data,
                f.graph,
                f.source,
                None,
                &edge(2, f.destination, 100),
                limits
            )
            .is_err());
        assert_eq!(
            Segment::open_pooled(pool, data, f.graph, WS).unwrap().hwm(),
            before_hwm
        );
        write::rollback(pool, log, chain, &mut creating).unwrap();
    });
}

#[test]
fn adjacency_scope_budget_and_cycle_errors_are_explicit() {
    let io = memory();
    fixture(&io, |pool, chain, log, data, f| {
        let access = AdjacencyAccess::new(pool, WS);
        let limits = ChainLimits::default();
        let mut txn = write::begin(pool, log, chain, seq(2)).unwrap();
        let head = access
            .append(
                log,
                chain,
                &mut txn,
                data,
                f.graph,
                f.source,
                None,
                &edge(1, f.destination, 13000),
                limits,
            )
            .unwrap()
            .head;
        let tail = access
            .append(
                log,
                chain,
                &mut txn,
                data,
                f.graph,
                f.source,
                Some(head),
                &edge(2, f.destination, 13000),
                limits,
            )
            .unwrap()
            .edge;
        write::commit(pool, log, chain, &mut txn, seq(2)).unwrap();
        let view = ReadView::new(seq(2));
        assert!(matches!(
            access.read(
                data,
                f.graph,
                f.destination,
                Some(head),
                view,
                chain,
                limits
            ),
            Err(AccessError::Chain(_))
        ));
        assert!(matches!(
            access.read(
                data,
                f.other_graph,
                f.source,
                Some(head),
                view,
                chain,
                limits
            ),
            Err(AccessError::Chain(_))
        ));
        assert!(matches!(
            access.read(data, f.heap, f.source, Some(head), view, chain, limits),
            Err(AccessError::Chain(_))
        ));
        assert!(matches!(
            AdjacencyAccess::new(pool, [35; 8]).read(
                data,
                f.graph,
                f.source,
                Some(head),
                view,
                chain,
                limits
            ),
            Err(AccessError::Chain(_))
        ));
        for cap in [
            ChainLimits { pages: 1, ..limits },
            ChainLimits { edges: 1, ..limits },
            ChainLimits {
                bytes: 100,
                ..limits
            },
        ] {
            assert!(matches!(
                access.read(data, f.graph, f.source, Some(head), view, chain, cap),
                Err(AccessError::Budget(_))
            ));
        }
        let mut cycle = write::begin(pool, log, chain, seq(3)).unwrap();
        write::adjacency::link(
            pool,
            log,
            chain,
            &mut cycle,
            key(tail),
            head,
            &InsertPolicy::in_place(0),
        )
        .unwrap();
        assert!(matches!(
            access.read(
                data,
                f.graph,
                f.source,
                Some(head),
                view.with_own(Some(cycle.txn_id)),
                chain,
                limits
            ),
            Err(AccessError::Chain("adjacency chain cycle"))
        ));
        write::rollback(pool, log, chain, &mut cycle).unwrap();
        assert_eq!(
            access
                .read(data, f.graph, f.source, Some(head), view, chain, limits)
                .unwrap()
                .len(),
            2
        );
    });
}

#[test]
fn adjacency_scoped_update_delete_keep_locators_and_endpoint_locks() {
    let io = memory();
    fixture(&io, |pool, chain, log, data, f| {
        let access = AdjacencyAccess::new(pool, WS);
        let limits = ChainLimits::default();
        let mut first = write::begin(pool, log, chain, seq(2)).unwrap();
        let original = access
            .append(
                log,
                chain,
                &mut first,
                data,
                f.graph,
                f.source,
                None,
                &edge(1, f.destination, 20),
                limits,
            )
            .unwrap();
        write::commit(pool, log, chain, &mut first, seq(2)).unwrap();
        let mut next = write::begin(pool, log, chain, seq(3)).unwrap();
        let mark = write::statement_mark(chain, &next).unwrap();
        assert!(access
            .update(
                log,
                chain,
                &mut next,
                data,
                f.other_graph,
                f.source,
                original.edge,
                &edge(1, f.destination, 100)
            )
            .is_err());
        assert!(access
            .update(
                log,
                chain,
                &mut next,
                data,
                f.graph,
                f.source,
                original.edge,
                &edge(1, f.source, 100)
            )
            .is_err());
        assert_eq!(write::statement_mark(chain, &next).unwrap().at(), mark.at());
        access
            .update(
                log,
                chain,
                &mut next,
                data,
                f.graph,
                f.source,
                original.edge,
                &edge(1, f.destination, 1000),
            )
            .unwrap();
        let mut deleting = write::begin(pool, log, chain, seq(4)).unwrap();
        let blocked = TableAccess::new(pool, WS)
            .delete(
                log,
                chain,
                &mut deleting,
                data,
                f.destination,
                &InsertPolicy::in_place(0),
            )
            .unwrap_err();
        assert!(
            matches!(blocked, bicdb_access::TableAccessError::Txn(TxnError::RowLocked { holder, .. }) if holder == next.txn_id)
        );
        write::rollback(pool, log, chain, &mut deleting).unwrap();
        access
            .delete(
                log,
                chain,
                &mut next,
                data,
                f.graph,
                f.source,
                original.edge,
            )
            .unwrap();
        assert!(access
            .read(
                data,
                f.graph,
                f.source,
                Some(original.head),
                ReadView::new(seq(2)).with_own(Some(next.txn_id)),
                chain,
                limits
            )
            .unwrap()
            .is_empty());
        assert_eq!(
            access
                .read(
                    data,
                    f.graph,
                    f.source,
                    Some(original.head),
                    ReadView::new(seq(2)),
                    chain,
                    limits
                )
                .unwrap()
                .len(),
            1
        );
        write::rollback_to_mark(pool, log, chain, &mut next, mark).unwrap();
        let rows = access
            .read(
                data,
                f.graph,
                f.source,
                Some(original.head),
                ReadView::new(seq(2)).with_own(Some(next.txn_id)),
                chain,
                limits,
            )
            .unwrap();
        assert_eq!(rows[0].0, original.edge);
        assert_eq!(layout::decode(&rows[0].1).unwrap().payload.len(), 20);
        write::commit(pool, log, chain, &mut next, seq(3)).unwrap();
    });
}

#[test]
fn adjacency_real_segments_and_loser_tail_survive_wal_recovery() {
    let io = memory();
    let mut saved = None;
    let mut head = None;
    fixture(&io, |pool, chain, log, data, f| {
        saved = Some(f);
        let access = AdjacencyAccess::new(pool, WS);
        let limits = ChainLimits::default();
        let mut winner = write::begin(pool, log, chain, seq(2)).unwrap();
        head = Some(
            access
                .append(
                    log,
                    chain,
                    &mut winner,
                    data,
                    f.graph,
                    f.source,
                    None,
                    &edge(1, f.destination, 13000),
                    limits,
                )
                .unwrap()
                .head,
        );
        write::commit(pool, log, chain, &mut winner, seq(2)).unwrap();
        let mut loser = write::begin(pool, log, chain, seq(3)).unwrap();
        access
            .append(
                log,
                chain,
                &mut loser,
                data,
                f.graph,
                f.source,
                head,
                &edge(2, f.destination, 13000),
                limits,
            )
            .unwrap();
        log.flush(log.appended_lsn()).unwrap();
        pool.flush_workspace(WS).unwrap();
    });
    let f = saved.unwrap();
    let mut undo = DataFile::open(&io, Path::new("/access/undo")).unwrap();
    let undo_handle = undo.handle();
    let mut data = DataFile::open(&io, Path::new("/access/data")).unwrap();
    let data_handle = data.handle();
    let mut chain = UndoChain::open(Segment::open(&mut undo, f.undo).unwrap());
    let mut cf = ControlFile::open(&io, Path::new("/access/c1"), Path::new("/access/c2")).unwrap();
    let spec = GroupSpec::new(2, 1, 512).unwrap();
    let groups = bicdb_wal::group::online_groups(&io, &cf, Path::new("/access/wal"), spec).unwrap();
    let mut log = GroupWriter::open(&io, &mut cf, Path::new("/access/wal"), spec).unwrap();
    let mut resolve = |r: Rdba| match r.file_id() {
        1 => Some((undo_handle, r.block_id())),
        3 => Some((data_handle, r.block_id())),
        _ => None,
    };
    let report =
        bicdb_wal::recovery::recover(&io, &groups, lsn(0), &mut chain, &mut log, &mut resolve)
            .unwrap();
    assert_eq!(report.undo.txns_rolled_back, 1);
    let pool = BufferPool::new(
        &io,
        128,
        move |_, r| match r.file_id() {
            1 => Some((undo_handle, r.block_id())),
            3 => Some((data_handle, r.block_id())),
            _ => None,
        },
        log.shared(),
    )
    .unwrap();
    let chain = chain.with_pool(&pool);
    let rows = AdjacencyAccess::new(&pool, WS)
        .read(
            &mut data,
            f.graph,
            f.source,
            head,
            ReadView::new(seq(2)),
            &chain,
            ChainLimits::default(),
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(layout::decode(&rows[0].1).unwrap().id, 1);
    assert!(layout::header(&snapshot(&pool, head.unwrap()))
        .unwrap()
        .next
        .is_none());
}
