use bicdb_catalog::{ddl, dict, row};
use bicdb_cli::boot::{create_instance, open_instance, Instance};
use bicdb_cli::config::InstanceParams;
use bicdb_common::seq::CommitSeq;
use bicdb_sql::session::{format_value, QueryResult, Session};
use bicdb_storage::heap::InsertPolicy;
use bicdb_storage::rowid::RowId;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Fixture(PathBuf);
impl Fixture {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "bicdb-physical-routes-{}-{tag}",
            std::process::id()
        ));
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
fn session(i: &mut Instance) -> Session<'_, 'static, 'static, 'static> {
    let seq = i.engine.current_seq();
    Session::new(i.pool, i.engine, &mut i.catalog, seq)
}
fn answer(i: &mut Instance) -> Vec<Vec<String>> {
    let result = session(i)
        .execute("CYPHER kg 'MATCH (n) RETURN n.name ORDER BY n.name'")
        .unwrap();
    let Some(QueryResult::Rows { rows, .. }) = result.last() else {
        panic!("{result:?}")
    };
    rows.iter()
        .map(|r| r.iter().map(format_value).collect())
        .collect()
}
fn cols() -> [dict::ColDef; 2] {
    [
        dict::ColDef {
            col: 1,
            name: "ordinal",
            type_code: dict::ColTypeCode::Number,
            length: 0,
            nullable: false,
        },
        dict::ColDef {
            col: 2,
            name: "data",
            type_code: dict::ColTypeCode::Bytes,
            length: 4096,
            nullable: false,
        },
    ]
}
fn manifest_row(i: &mut Instance) -> (u32, RowId) {
    let seq = CommitSeq::from_raw(i.engine.current_seq()).unwrap();
    let graph = i
        .catalog
        .resolve(seq, dict::namespace::TABLE, "kg")
        .unwrap();
    let primary = i
        .catalog
        .indexes_of(seq, graph.obj)
        .unwrap()
        .into_iter()
        .find(|idx| idx.kind == dict::index_kind::BTREE && idx.is_unique)
        .unwrap();
    let block = ddl::live_segment_block(&mut i.catalog, graph.obj).unwrap();
    let bytes = row::encode(
        &[row::DictValue::Num(0), row::DictValue::Bytes(vec![])],
        &cols(),
    )
    .unwrap();
    let key = row::key_from_row(&bytes, &[0]).unwrap();
    let candidates = i
        .catalog
        .graph_index_range(primary.obj, Some(&key), Some(&key), 1024)
        .unwrap();
    let view = bicdb_storage::cr::ReadView::new(seq);
    let ids: Vec<_> = candidates.iter().map(|(_, rid)| *rid).collect();
    let visible = i
        .engine
        .with_read_context(|pool, chain| {
            bicdb_storage::scan::fetch_rows_resolved(pool, chain, view, &ids)
        })
        .unwrap();
    let rid = visible
        .into_iter()
        .flatten()
        .find_map(|(rid, bytes)| {
            let decoded = row::decode(&bytes, &cols()).unwrap();
            (decoded[0] == row::DictValue::Num(0)).then_some(rid)
        })
        .unwrap();
    (block, rid)
}
#[test]
fn inactive_physical_routes_preserve_sql_snapshot_templates_and_restart() {
    let f = Fixture::new("inactive");
    let mut i = create_instance(&f.params(), None).unwrap();
    // Explicit legacy fixture: inactive route creation must not migrate it.
    ddl::create_graph(&mut i.catalog, i.engine, "kg").unwrap();
    session(&mut i)
        .execute("CYPHER kg 'CREATE (:Entity {name:\"kept\"})'")
        .unwrap();
    let expected = answer(&mut i);
    let (schema, snapshot) = session(&mut i).graph_initialization_snapshot(true).unwrap();
    let routes = ddl::create_graph_physical_routes(
        &mut i.catalog,
        i.engine,
        "kg",
        |_, _, _, _, _, _| Ok(()),
    )
    .unwrap();
    assert_eq!(answer(&mut i), expected);
    let (after_schema, after_snapshot) =
        session(&mut i).graph_initialization_snapshot(true).unwrap();
    assert_eq!(schema, after_schema);
    assert_eq!(snapshot.len(), after_snapshot.len());
    assert_eq!(snapshot[0].data, after_snapshot[0].data);
    i.shutdown().unwrap();
    drop(i);
    let mut i = open_instance(&f.params()).unwrap();
    assert_eq!(
        ddl::graph_physical_routes(&mut i.catalog, "kg").unwrap(),
        Some(routes)
    );
    assert_eq!(answer(&mut i), expected);
    i.shutdown().unwrap();
}
#[test]
fn crash_during_physical_route_and_manifest_publication_recovers_prior_sql_graph() {
    let f = Fixture::new("crash");
    let mut i = create_instance(&f.params(), None).unwrap();
    // Explicit legacy fixture: inactive route creation must not migrate it.
    ddl::create_graph(&mut i.catalog, i.engine, "kg").unwrap();
    session(&mut i)
        .execute("CYPHER kg 'CREATE (:Entity {name:\"kept\"})'")
        .unwrap();
    let expected = answer(&mut i);
    i.shutdown().unwrap();
    drop(i);
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "graph_physical_route_crash_child",
            "--ignored",
            "--nocapture",
        ])
        .env("BICDB_PHYSICAL_ROUTE_CRASH_ROOT", &f.0)
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(99),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let mut i = open_instance(&f.params()).unwrap();
    assert_eq!(
        ddl::graph_physical_routes(&mut i.catalog, "kg").unwrap(),
        None
    );
    assert_eq!(answer(&mut i), expected);
    i.shutdown().unwrap();
}
#[test]
#[ignore = "crash helper, actually executed by parent isolation/recovery test"]
fn graph_physical_route_crash_child() {
    let root = std::env::var_os("BICDB_PHYSICAL_ROUTE_CRASH_ROOT").expect("parent fixture path");
    let params = InstanceParams::for_init(Path::new(&root), None, &[]).unwrap();
    let mut i = open_instance(&params).unwrap();
    let (heap, rid) = manifest_row(&mut i);
    ddl::create_graph_physical_routes(
        &mut i.catalog,
        i.engine,
        "kg",
        |_, cat, pool, log, chain, txn| {
            let ws = cat.ws();
            let bytes = row::encode(
                &[
                    row::DictValue::Num(0),
                    row::DictValue::Bytes(b"uncommitted invalid v3 manifest".to_vec()),
                ],
                &cols(),
            )?;
            bicdb_access::TableAccess::new(pool, ws).update(
                log,
                chain,
                txn,
                cat.file_mut(),
                heap,
                rid,
                &bytes,
                &InsertPolicy::in_place(0),
            )?;
            log.flush(log.appended_lsn()).unwrap();
            pool.flush_workspace(ws).unwrap();
            std::process::exit(99)
        },
    )
    .unwrap();
    panic!("crash callback did not exit");
}
