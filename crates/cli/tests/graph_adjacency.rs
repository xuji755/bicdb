use bicdb_catalog::ddl;
use bicdb_cli::{
    boot::{create_instance, open_instance, Instance},
    config::InstanceParams,
};
use bicdb_common::seq::CommitSeq;
use bicdb_graph::{Deadline, Edge, Limits};
use bicdb_sql::{
    graph_adjacency::{GraphRecords, NativeAdjacency},
    session::{QueryResult, Session},
};
use bicdb_storage::{cr::ReadView, rowid::RowId};
use std::{collections::BTreeMap, path::PathBuf};

struct Fixture(PathBuf);
impl Fixture {
    fn new(tag: &str) -> Self {
        let p =
            std::env::temp_dir().join(format!("bicdb-adjacency-port-{}-{tag}", std::process::id()));
        assert!(!p.exists());
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn params(&self) -> InstanceParams {
        InstanceParams::for_init(&self.0, None, &[]).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[derive(Clone, Copy)]
struct Scope {
    records: GraphRecords,
    routes: ddl::GraphPhysicalRoutes,
    a: u64,
    b: u64,
    start: u64,
}
fn setup(f: &Fixture) -> (Instance, Scope) {
    let mut i = create_instance(&f.params(), None).unwrap();
    // Raw data-port tests create an inactive legacy route set intentionally.
    ddl::create_graph(&mut i.catalog, i.engine, "kg").unwrap();
    ddl::create_graph(&mut i.catalog, i.engine, "foreign_kg").unwrap();
    let seq = i.seq();
    let result = Session::new(i.pool, i.engine, &mut i.catalog, seq)
        .execute(
            "CYPHER kg 'CREATE (a:Entity {name:\"a\"}),(b:Entity {name:\"b\"}) RETURN id(a),id(b)'",
        )
        .unwrap();
    let Some(QueryResult::Rows { rows, .. }) = result.last() else {
        panic!("{result:?}")
    };
    let a = bicdb_sql::session::format_value(&rows[0][0])
        .parse()
        .unwrap();
    let b = bicdb_sql::session::format_value(&rows[0][1])
        .parse()
        .unwrap();
    let records = GraphRecords::resolve(
        &mut i.catalog,
        "kg",
        CommitSeq::from_raw(i.engine.current_seq()).unwrap(),
    )
    .unwrap();
    let routes = ddl::create_graph_physical_routes(
        &mut i.catalog,
        i.engine,
        "kg",
        |_, _, _, _, _, _| Ok(()),
    )
    .unwrap();
    let (start, _) =
        ddl::reserve_graph_ids(&mut i.catalog, i.engine, records.heap.obj, 100).unwrap();
    (
        i,
        Scope {
            records,
            routes,
            a,
            b,
            start,
        },
    )
}
fn edge(s: Scope, n: u64, source: u64, target: u64, bytes: usize) -> Edge {
    Edge {
        id: s.start + n,
        source,
        target,
        label: "LINK".into(),
        properties: BTreeMap::from([
            ("value".into(), serde_json::json!("文".repeat(bytes))),
            (
                "nested".into(),
                serde_json::json!({"x":[1,true,null,{"key":"值"}]}),
            ),
        ]),
    }
}
fn read(i: &mut Instance, s: Scope, view: ReadView, id: u64) -> Option<(RowId, Edge)> {
    let ws = i.catalog.ws();
    let file = i.catalog.file_mut();
    i.engine
        .with_read_context(|pool, chain| {
            NativeAdjacency::new(
                pool,
                ws,
                s.records,
                s.routes,
                Limits::default(),
                Deadline::new(60000),
            )
            .unwrap()
            .edge(file, chain, view, id)
        })
        .unwrap()
}
fn overflow_count(i: &mut Instance, s: Scope, view: ReadView) -> usize {
    let ws = i.catalog.ws();
    let file = i.catalog.file_mut();
    i.engine.with_read_context(|pool, chain| {
        let segment =
            bicdb_storage::segment::Segment::open_pooled(pool, file, s.records.heap.block, ws)
                .unwrap();
        let blocks = segment.data_blocks(segment.hwm());
        let fid = segment.file_id();
        drop(segment);
        let mut scan = bicdb_storage::scan::HeapScanner::new(pool, chain, view, fid, blocks);
        let shape = bicdb_exec::RowShape::new(vec![
            bicdb_exec::ColKind::Number,
            bicdb_exec::ColKind::Bytes,
        ]);
        let mut count = 0;
        while let Some((_, bytes)) = scan.next_row().unwrap() {
            let row = bicdb_exec::decode_row(&bytes, &shape).unwrap();
            let bicdb_exec::Value::Number(key) = &row.values[0] else {
                panic!("{row:?}")
            };
            let key: u64 = key.to_string().parse().unwrap();
            if key >> 60 == 3 {
                count += 1;
            }
        }
        count
    })
}
fn chain_ids(i: &mut Instance, s: Scope, view: ReadView, prior: RowId) -> Vec<u64> {
    let ws = i.catalog.ws();
    let file = i.catalog.file_mut();
    i.engine.with_read_context(|pool, chain| {
        let access = bicdb_access::adjacency::AdjacencyAccess::new(pool, ws);
        let (source, _) = access
            .fetch(file, s.routes.adjacency.block, prior, view, chain)
            .unwrap()
            .unwrap();
        let head = RowId::page_address(prior.file_id(), prior.block_id()).unwrap();
        access
            .read(
                file,
                s.routes.adjacency.block,
                source,
                Some(head),
                view,
                chain,
                bicdb_access::adjacency::ChainLimits::default(),
            )
            .unwrap()
            .iter()
            .map(|(_, bytes)| bicdb_storage::adjacency::decode(bytes).unwrap().id)
            .collect()
    })
}
fn write_insert(i: &mut Instance, s: Scope, edges: &[Edge]) -> Vec<RowId> {
    let mut t = i.engine.begin().unwrap();
    let view =
        ReadView::new(CommitSeq::from_raw(i.engine.current_seq()).unwrap()).with_own(Some(t.id()));
    let ws = i.catalog.ws();
    let file = i.catalog.file_mut();
    let locations = i
        .engine
        .with_write_context(&mut t, |pool, log, chain, txn| {
            let mut port = NativeAdjacency::new(
                pool,
                ws,
                s.records,
                s.routes,
                Limits::default(),
                Deadline::new(60000),
            )?;
            edges
                .iter()
                .map(|e| port.insert(file, log, chain, txn, view, e))
                .collect::<Result<Vec<_>, bicdb_graph::Error>>()
        })
        .unwrap();
    i.engine.commit(&mut t).unwrap();
    locations
}
#[test]
fn native_adjacency_port_scopes_snapshots_directions_and_restarts() {
    let f = Fixture::new("snapshot");
    let (mut i, s) = setup(&f);
    let before = ReadView::new(CommitSeq::from_raw(i.engine.current_seq()).unwrap());
    let e1 = edge(s, 0, s.a, s.b, 2);
    let mut e2 = edge(s, 1, s.a, s.b, 70000);
    e2.label = "长".repeat(1500);
    let e3 = edge(s, 2, s.b, s.b, 3);
    let locations = write_insert(&mut i, s, &[e1.clone(), e2.clone(), e3.clone()]);
    assert!(read(&mut i, s, before, e1.id).is_none());
    let view = ReadView::new(CommitSeq::from_raw(i.engine.current_seq()).unwrap());
    assert_eq!(
        read(&mut i, s, view, e2.id),
        Some((locations[1], e2.clone()))
    );
    {
        let ws = i.catalog.ws();
        let file = i.catalog.file_mut();
        i.engine.with_read_context(|pool, chain| {
            let mut p = NativeAdjacency::new(
                pool,
                ws,
                s.records,
                s.routes,
                Limits::default(),
                Deadline::new(60000),
            )
            .unwrap();
            assert_eq!(
                p.outgoing(file, chain, view, s.a, &[]).unwrap(),
                vec![(locations[0], e1.clone()), (locations[1], e2.clone())]
            );
            assert_eq!(
                p.incoming(file, chain, view, s.b, &[e2.label.clone()])
                    .unwrap(),
                vec![(locations[1], e2.clone())]
            );
            assert_eq!(p.incoming(file, chain, view, s.b, &[]).unwrap().len(), 3);
            assert!(p
                .outgoing(file, chain, before, s.a, &[])
                .unwrap()
                .is_empty());
        });
    }
    i.shutdown().unwrap();
    drop(i);
    let mut i = open_instance(&f.params()).unwrap();
    assert_eq!(read(&mut i, s, view, e2.id), Some((locations[1], e2)));
    // The isolated port fixture does not select a SQL storage version. Validate
    // its actual routes rather than treating a lazy v2 query as v3 acceptance.
    assert_eq!(
        ddl::graph_physical_routes(&mut i.catalog, "kg").unwrap(),
        Some(s.routes)
    );
    let seq = CommitSeq::from_raw(i.engine.current_seq()).unwrap();
    let reopened = GraphRecords::resolve(&mut i.catalog, "kg", seq).unwrap();
    assert_eq!(reopened.heap, s.records.heap);
    assert_eq!(reopened.primary, s.records.primary);
    i.shutdown().unwrap();
}
#[test]
fn native_adjacency_port_updates_external_chunks_keeps_old_views_and_rolls_back() {
    let f = Fixture::new("update");
    let (mut i, s) = setup(&f);
    let original = edge(s, 0, s.a, s.b, 1400);
    let rid = write_insert(&mut i, s, std::slice::from_ref(&original))[0];
    let old = ReadView::new(CommitSeq::from_raw(i.engine.current_seq()).unwrap());
    let bigger = edge(s, 0, s.a, s.b, 70000);
    let mut t = i.engine.begin().unwrap();
    let own =
        ReadView::new(CommitSeq::from_raw(i.engine.current_seq()).unwrap()).with_own(Some(t.id()));
    {
        let ws = i.catalog.ws();
        let file = i.catalog.file_mut();
        i.engine
            .with_write_context(&mut t, |pool, log, chain, txn| {
                let mut p = NativeAdjacency::new(
                    pool,
                    ws,
                    s.records,
                    s.routes,
                    Limits::default(),
                    Deadline::new(60000),
                )
                .unwrap();
                assert_eq!(p.update(file, log, chain, txn, own, &bigger).unwrap(), rid);
                assert_eq!(
                    p.edge(file, chain, old, original.id).unwrap(),
                    Some((rid, original.clone()))
                );
                assert_eq!(
                    p.edge(file, chain, own, original.id).unwrap(),
                    Some((rid, bigger.clone()))
                );
            });
    }
    i.engine.commit(&mut t).unwrap();
    let large_view = ReadView::new(CommitSeq::from_raw(i.engine.current_seq()).unwrap());
    let smaller = edge(s, 0, s.a, s.b, 2);
    let mut t = i.engine.begin().unwrap();
    let own =
        ReadView::new(CommitSeq::from_raw(i.engine.current_seq()).unwrap()).with_own(Some(t.id()));
    {
        let ws = i.catalog.ws();
        let file = i.catalog.file_mut();
        i.engine
            .with_write_context(&mut t, |pool, log, chain, txn| {
                let mut p = NativeAdjacency::new(
                    pool,
                    ws,
                    s.records,
                    s.routes,
                    Limits::default(),
                    Deadline::new(60000),
                )
                .unwrap();
                assert_eq!(p.update(file, log, chain, txn, own, &smaller).unwrap(), rid);
                assert_eq!(
                    p.edge(file, chain, large_view, original.id).unwrap(),
                    Some((rid, bigger.clone()))
                );
                p.delete(file, log, chain, txn, own, original.id).unwrap();
                assert!(p.edge(file, chain, own, original.id).unwrap().is_none());
                assert_eq!(
                    p.edge(file, chain, old, original.id).unwrap(),
                    Some((rid, original.clone()))
                );
            });
    }
    i.engine.rollback(&mut t).unwrap();
    let view = ReadView::new(CommitSeq::from_raw(i.engine.current_seq()).unwrap());
    assert_eq!(read(&mut i, s, view, original.id), Some((rid, bigger)));
    i.shutdown().unwrap();
}
#[test]
fn native_adjacency_port_budget_failure_undoes_partial_rows_indexes_and_edge() {
    let f = Fixture::new("budget");
    let (mut i, s) = setup(&f);
    let prior = edge(s, 0, s.a, s.b, 2);
    let e = edge(s, 1, s.a, s.b, 70000);
    let mut t = i.engine.begin().unwrap();
    let own =
        ReadView::new(CommitSeq::from_raw(i.engine.current_seq()).unwrap()).with_own(Some(t.id()));
    let prior_rid = {
        let ws = i.catalog.ws();
        let file = i.catalog.file_mut();
        i.engine
            .with_write_context(&mut t, |pool, log, chain, txn| {
                NativeAdjacency::new(
                    pool,
                    ws,
                    s.records,
                    s.routes,
                    Limits::default(),
                    Deadline::new(60000),
                )
                .unwrap()
                .insert(file, log, chain, txn, own, &prior)
                .unwrap()
            })
    };
    let mark = i.engine.statement_mark(&t).unwrap();
    let outcome = {
        let ws = i.catalog.ws();
        let file = i.catalog.file_mut();
        i.engine
            .with_write_context(&mut t, |pool, log, chain, txn| {
                let limits = Limits {
                    max_expansions: 20,
                    ..Limits::default()
                };
                NativeAdjacency::new(pool, ws, s.records, s.routes, limits, Deadline::new(60000))
                    .unwrap()
                    .insert(file, log, chain, txn, own, &e)
            })
    };
    assert!(outcome.unwrap_err().to_string().contains("budget"));
    assert!(
        overflow_count(&mut i, s, own) > 0,
        "exercise rollback after heap data was written"
    );
    assert_eq!(chain_ids(&mut i, s, own, prior_rid), vec![prior.id, e.id]);
    i.engine.rollback_statement(&mut t, mark).unwrap();
    assert_eq!(
        overflow_count(&mut i, s, own),
        0,
        "include rows not yet entered in the ordinal tree"
    );
    assert_eq!(chain_ids(&mut i, s, own, prior_rid), vec![prior.id]);
    assert_eq!(
        read(&mut i, s, own, prior.id),
        Some((prior_rid, prior.clone()))
    );
    assert!(read(&mut i, s, own, e.id).is_none());
    // The caller can keep its transaction and perform a later independent write.
    let next = edge(s, 2, s.a, s.b, 2);
    {
        let ws = i.catalog.ws();
        let file = i.catalog.file_mut();
        i.engine
            .with_write_context(&mut t, |pool, log, chain, txn| {
                NativeAdjacency::new(
                    pool,
                    ws,
                    s.records,
                    s.routes,
                    Limits::default(),
                    Deadline::new(60000),
                )
                .unwrap()
                .insert(file, log, chain, txn, own, &next)
                .unwrap();
            });
    }
    i.engine.commit(&mut t).unwrap();
    let view = ReadView::new(CommitSeq::from_raw(i.engine.current_seq()).unwrap());
    assert!(read(&mut i, s, view, e.id).is_none());
    assert!(read(&mut i, s, view, next.id).is_some());
    assert_eq!(read(&mut i, s, view, prior.id), Some((prior_rid, prior)));
    i.shutdown().unwrap();
}

#[test]
fn native_adjacency_port_flushed_loser_recovers_edges_metadata_and_overflow() {
    let f = Fixture::new("crash");
    let (mut i, s) = setup(&f);
    let prior = edge(s, 0, s.a, s.b, 2);
    let rid = write_insert(&mut i, s, std::slice::from_ref(&prior))[0];
    i.shutdown().unwrap();
    drop(i);
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "native_adjacency_port_crash_child",
            "--ignored",
            "--nocapture",
        ])
        .env("BICDB_ADJACENCY_PORT_CRASH_ROOT", &f.0)
        .env(
            "BICDB_ADJACENCY_PORT_CRASH_IDS",
            format!("{} {} {}", s.a, s.b, s.start),
        )
        .output()
        .unwrap();
    assert_eq!(
        result.status.code(),
        Some(99),
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    let mut i = open_instance(&f.params()).unwrap();
    let view = ReadView::new(CommitSeq::from_raw(i.engine.current_seq()).unwrap());
    assert_eq!(read(&mut i, s, view, prior.id), Some((rid, prior)));
    assert!(read(&mut i, s, view, s.start + 1).is_none());
    assert_eq!(overflow_count(&mut i, s, view), 0);
    assert_eq!(chain_ids(&mut i, s, view, rid), vec![s.start]);
    // Stale locator and reserved physical directories do not obstruct a new ID.
    let later = edge(s, 2, s.a, s.b, 2);
    write_insert(&mut i, s, std::slice::from_ref(&later));
    let view = ReadView::new(CommitSeq::from_raw(i.engine.current_seq()).unwrap());
    assert_eq!(chain_ids(&mut i, s, view, rid), vec![s.start, later.id]);
    i.shutdown().unwrap();
}
#[test]
#[ignore = "crash child actually executed by the parent recovery test"]
fn native_adjacency_port_crash_child() {
    let root = PathBuf::from(std::env::var_os("BICDB_ADJACENCY_PORT_CRASH_ROOT").unwrap());
    let ids = std::env::var("BICDB_ADJACENCY_PORT_CRASH_IDS").unwrap();
    let ids: Vec<u64> = ids
        .split_whitespace()
        .map(|id| id.parse().unwrap())
        .collect();
    let mut i = open_instance(&InstanceParams::for_init(&root, None, &[]).unwrap()).unwrap();
    let seq = CommitSeq::from_raw(i.engine.current_seq()).unwrap();
    let records = GraphRecords::resolve(&mut i.catalog, "kg", seq).unwrap();
    let routes = ddl::graph_physical_routes(&mut i.catalog, "kg")
        .unwrap()
        .unwrap();
    let s = Scope {
        records,
        routes,
        a: ids[0],
        b: ids[1],
        start: ids[2],
    };
    let replacement = edge(s, 0, s.a, s.b, 70000);
    let added = edge(s, 1, s.a, s.b, 70000);
    let mut t = i.engine.begin().unwrap();
    let own = ReadView::new(seq).with_own(Some(t.id()));
    let ws = i.catalog.ws();
    let file = i.catalog.file_mut();
    i.engine
        .with_write_context(&mut t, |pool, log, chain, txn| {
            let mut p = NativeAdjacency::new(
                pool,
                ws,
                s.records,
                s.routes,
                Limits::default(),
                Deadline::new(60000),
            )
            .unwrap();
            p.update(file, log, chain, txn, own, &replacement).unwrap();
            p.insert(file, log, chain, txn, own, &added).unwrap();
            log.flush(log.appended_lsn()).unwrap();
            pool.flush_workspace(ws).unwrap();
            std::process::exit(99)
        });
}

#[test]
fn native_adjacency_port_full_page_externalizes_small_growth_at_stable_locator() {
    let f = Fixture::new("full");
    let (mut i, s) = setup(&f);
    let old = edge(s, 0, s.a, s.b, 150);
    let mut edges = vec![old.clone()];
    edges.extend((1..=18).map(|n| edge(s, n, s.a, s.b, 333)));
    let rid = write_insert(&mut i, s, &edges)[0];
    let old_view = ReadView::new(CommitSeq::from_raw(i.engine.current_seq()).unwrap());
    let replacement = edge(s, 0, s.a, s.b, 1100);
    let mut t = i.engine.begin().unwrap();
    let own = old_view.with_own(Some(t.id()));
    {
        let ws = i.catalog.ws();
        let file = i.catalog.file_mut();
        i.engine
            .with_write_context(&mut t, |pool, log, chain, txn| {
                let mut p = NativeAdjacency::new(
                    pool,
                    ws,
                    s.records,
                    s.routes,
                    Limits::default(),
                    Deadline::new(60000),
                )
                .unwrap();
                assert_eq!(
                    p.update(file, log, chain, txn, own, &replacement).unwrap(),
                    rid
                );
                let (_, raw) = bicdb_access::adjacency::AdjacencyAccess::new(pool, ws)
                    .fetch(file, s.routes.adjacency.block, rid, own, chain)
                    .unwrap()
                    .unwrap();
                assert!(
                    bicdb_graph::adjacency_record::overflow_range(old.id, &raw, &Limits::default())
                        .unwrap()
                        .is_some(),
                    "must use external fallback despite document <4096 bytes"
                );
                assert_eq!(
                    p.edge(file, chain, old_view, old.id).unwrap(),
                    Some((rid, old.clone()))
                );
                assert_eq!(
                    p.edge(file, chain, own, old.id).unwrap(),
                    Some((rid, replacement.clone()))
                );
            });
    }
    i.engine.commit(&mut t).unwrap();
    let view = ReadView::new(CommitSeq::from_raw(i.engine.current_seq()).unwrap());
    assert_eq!(read(&mut i, s, view, old.id), Some((rid, replacement)));
    i.shutdown().unwrap();
}

#[test]
fn native_adjacency_port_endpoint_conflicts_preserve_the_source_chain() {
    let f = Fixture::new("locks");
    let (mut i, s) = setup(&f);
    let first = edge(s, 0, s.a, s.b, 2);
    let failed = edge(s, 1, s.a, s.b, 2);
    let last = edge(s, 2, s.a, s.b, 2);
    let mut one = i.engine.begin().unwrap();
    let mut two = i.engine.begin().unwrap();
    let base = CommitSeq::from_raw(i.engine.current_seq()).unwrap();
    {
        let ws = i.catalog.ws();
        let file = i.catalog.file_mut();
        i.engine
            .with_write_context(&mut one, |pool, log, chain, txn| {
                let own = ReadView::new(base).with_own(Some(txn.txn_id));
                NativeAdjacency::new(
                    pool,
                    ws,
                    s.records,
                    s.routes,
                    Limits::default(),
                    Deadline::new(60000),
                )
                .unwrap()
                .insert(file, log, chain, txn, own, &first)
                .unwrap();
            });
    }
    let failure = {
        let ws = i.catalog.ws();
        let file = i.catalog.file_mut();
        i.engine
            .with_write_context(&mut two, |pool, log, chain, txn| {
                let own = ReadView::new(base).with_own(Some(txn.txn_id));
                NativeAdjacency::new(
                    pool,
                    ws,
                    s.records,
                    s.routes,
                    Limits::default(),
                    Deadline::new(60000),
                )
                .unwrap()
                .insert(file, log, chain, txn, own, &failed)
            })
            .unwrap_err()
    };
    assert!(failure.to_string().contains("锁"), "{failure}");
    i.engine.rollback(&mut two).unwrap();
    i.engine.commit(&mut one).unwrap();
    write_insert(&mut i, s, std::slice::from_ref(&last));
    let view = ReadView::new(CommitSeq::from_raw(i.engine.current_seq()).unwrap());
    let ws = i.catalog.ws();
    let file = i.catalog.file_mut();
    i.engine.with_read_context(|pool, chain| {
        let mut p = NativeAdjacency::new(
            pool,
            ws,
            s.records,
            s.routes,
            Limits::default(),
            Deadline::new(60000),
        )
        .unwrap();
        let edges = p.outgoing(file, chain, view, s.a, &[]).unwrap();
        assert_eq!(
            edges.into_iter().map(|(_, e)| e).collect::<Vec<_>>(),
            vec![first, last]
        );
        assert!(p.edge(file, chain, view, failed.id).unwrap().is_none());
    });
    i.shutdown().unwrap();
}

#[test]
fn native_adjacency_port_rejects_foreign_routes_wrong_views_and_expired_deadlines() {
    let f = Fixture::new("scope");
    let (mut i, s) = setup(&f);
    let e = edge(s, 0, s.a, s.b, 2);
    let rid = write_insert(&mut i, s, std::slice::from_ref(&e))[0];
    let view = ReadView::new(CommitSeq::from_raw(i.engine.current_seq()).unwrap());
    let other = GraphRecords::resolve(&mut i.catalog, "foreign_kg", view.snapshot).unwrap();
    assert!(NativeAdjacency::new(
        i.pool,
        i.catalog.ws(),
        other,
        s.routes,
        Limits::default(),
        Deadline::new(60000)
    )
    .is_err());
    {
        let ws = i.catalog.ws();
        let file = i.catalog.file_mut();
        i.engine.with_read_context(|pool, chain| {
            let mut routes = s.routes;
            routes.adjacency.obj = u32::MAX;
            let mut p = NativeAdjacency::new(
                pool,
                ws,
                s.records,
                routes,
                Limits::default(),
                Deadline::new(60000),
            )
            .unwrap();
            assert!(p
                .edge(file, chain, view, e.id)
                .unwrap_err()
                .to_string()
                .contains("ownership"));
            let elapsed = bicdb_graph::Deadline::from_start(
                std::time::Instant::now() - std::time::Duration::from_millis(2),
                1,
            );
            let mut p =
                NativeAdjacency::new(pool, ws, s.records, s.routes, Limits::default(), elapsed)
                    .unwrap();
            assert!(p
                .edge(file, chain, view, e.id)
                .unwrap_err()
                .to_string()
                .contains("time budget"));
            assert!(
                p.work().index_entries == 0,
                "deadline check must precede range allocation"
            );
        });
    }
    let mut t = i.engine.begin().unwrap();
    {
        let ws = i.catalog.ws();
        let file = i.catalog.file_mut();
        i.engine
            .with_write_context(&mut t, |pool, log, chain, txn| {
                let mut p = NativeAdjacency::new(
                    pool,
                    ws,
                    s.records,
                    s.routes,
                    Limits::default(),
                    Deadline::new(60000),
                )
                .unwrap();
                assert!(p
                    .delete(file, log, chain, txn, view, e.id)
                    .unwrap_err()
                    .to_string()
                    .contains("own transaction"));
            });
    }
    i.engine.rollback(&mut t).unwrap();
    assert_eq!(read(&mut i, s, view, e.id), Some((rid, e)));
    i.shutdown().unwrap();
}

#[test]
fn native_adjacency_port_rechecks_graph_endpoints_even_with_a_valid_payload_hash() {
    let f = Fixture::new("forged");
    let (mut i, s) = setup(&f);
    let prior = edge(s, 0, s.a, s.b, 2);
    let prior_rid = write_insert(&mut i, s, std::slice::from_ref(&prior))[0];
    let seq = i.engine.current_seq();
    let result = Session::new(i.pool, i.engine, &mut i.catalog, seq)
        .execute("CYPHER foreign_kg 'CREATE (n:Entity {name:\"foreign\"}) RETURN id(n)'")
        .unwrap();
    let Some(QueryResult::Rows { rows, .. }) = result.last() else {
        panic!("{result:?}")
    };
    let foreign_id: u64 = bicdb_sql::session::format_value(&rows[0][0])
        .parse()
        .unwrap();
    let seq = CommitSeq::from_raw(i.engine.current_seq()).unwrap();
    let other = GraphRecords::resolve(&mut i.catalog, "foreign_kg", seq).unwrap();
    let ws = i.catalog.ws();
    let foreign = {
        let file = i.catalog.file_mut();
        i.engine.with_read_context(|pool, chain| {
            let segment =
                bicdb_storage::segment::Segment::open_pooled(pool, file, other.heap.block, ws)
                    .unwrap();
            let blocks = segment.data_blocks(segment.hwm());
            let fid = segment.file_id();
            drop(segment);
            let shape = bicdb_exec::RowShape::new(vec![
                bicdb_exec::ColKind::Number,
                bicdb_exec::ColKind::Bytes,
            ]);
            let mut scan =
                bicdb_storage::scan::HeapScanner::new(pool, chain, ReadView::new(seq), fid, blocks);
            while let Some((rid, bytes)) = scan.next_row().unwrap() {
                let row = bicdb_exec::decode_row(&bytes, &shape).unwrap();
                let bicdb_exec::Value::Number(key) = &row.values[0] else {
                    panic!("{row:?}")
                };
                if key.to_string() == ((1u64 << 60) | (foreign_id << 10)).to_string() {
                    return rid;
                }
            }
            panic!("missing foreign anchor")
        })
    };
    let forged = edge(s, 1, s.a, s.b, 2);
    let mut t = i.engine.begin().unwrap();
    let own = ReadView::new(seq).with_own(Some(t.id()));
    {
        let file = i.catalog.file_mut();
        i.engine
            .with_write_context(&mut t, |pool, log, chain, txn| {
                let access = bicdb_access::adjacency::AdjacencyAccess::new(pool, ws);
                let (source, _) = access
                    .fetch(file, s.routes.adjacency.block, prior_rid, own, chain)
                    .unwrap()
                    .unwrap();
                let encoded = bicdb_graph::adjacency_record::encode(
                    &forged,
                    source,
                    foreign,
                    bicdb_graph::adjacency_record::Placement::Automatic,
                    &Limits::default(),
                )
                .unwrap();
                // The codec/hash is internally valid; only graph endpoint resolution
                // proves that this physical destination is from another graph.
                assert_eq!(
                    bicdb_graph::adjacency_record::decode(
                        forged.id,
                        source,
                        &encoded.record,
                        &encoded.overflow,
                        &Limits::default()
                    )
                    .unwrap(),
                    forged
                );
                access
                    .lock_endpoints(log, chain, txn, source, foreign)
                    .unwrap();
                let head = RowId::page_address(prior_rid.file_id(), prior_rid.block_id()).unwrap();
                let appended = access
                    .append(
                        log,
                        chain,
                        txn,
                        file,
                        s.routes.adjacency.block,
                        source,
                        Some(head),
                        &encoded.record,
                        bicdb_access::adjacency::ChainLimits::default(),
                    )
                    .unwrap();
                let key = bicdb_graph::adjacency_record::locator_key(forged.id).unwrap();
                let root = bicdb_access::index::insert_entry(
                    pool,
                    log,
                    file,
                    ws,
                    s.routes.locator.block,
                    txn,
                    &key,
                    appended.edge,
                )
                .unwrap();
                bicdb_access::index::write_tree_head_redo(
                    pool,
                    log,
                    file,
                    ws,
                    s.routes.locator.block,
                    txn,
                    root,
                )
                .unwrap();
            });
    }
    i.engine.commit(&mut t).unwrap();
    let view = ReadView::new(CommitSeq::from_raw(i.engine.current_seq()).unwrap());
    let file = i.catalog.file_mut();
    i.engine.with_read_context(|pool, chain| {
        let mut port = NativeAdjacency::new(
            pool,
            ws,
            s.records,
            s.routes,
            Limits::default(),
            Deadline::new(60000),
        )
        .unwrap();
        assert!(port
            .edge(file, chain, view, forged.id)
            .unwrap_err()
            .to_string()
            .contains("endpoint mismatch"));
        assert_eq!(
            port.edge(file, chain, view, prior.id).unwrap(),
            Some((prior_rid, prior))
        );
    });
    i.shutdown().unwrap();
}
