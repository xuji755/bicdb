use bicdb_cli::{
    boot::{create_instance, open_instance, Instance},
    config::InstanceParams,
};
use bicdb_sql::session::{format_value, QueryResult, Session};
use bicdb_storage::cr::ReadView;
use std::{collections::BTreeMap, path::PathBuf};

struct Fixture(PathBuf);
impl Fixture {
    fn new(tag: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("bicdb-sql-adjacency-{}-{tag}", std::process::id()));
        assert!(!path.exists());
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
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
    let seq = i.seq();
    Session::new(i.pool, i.engine, &mut i.catalog, seq)
}
fn rows(s: &mut Session<'_, '_, '_, '_>, sql: &str) -> Vec<Vec<String>> {
    let result = s.execute(sql).unwrap();
    let Some(QueryResult::Rows { rows, .. }) = result.last() else {
        panic!("{result:?}")
    };
    rows.iter()
        .map(|r| r.iter().map(format_value).collect())
        .collect()
}
fn query(text: &str) -> String {
    format!("CYPHER kg '{}'", text.replace('\'', "''"))
}
fn count(s: &mut Session<'_, '_, '_, '_>) -> String {
    rows(s, &query("MATCH ()-[r]->() RETURN count(r)"))[0][0].clone()
}
fn physical(i: &mut Instance) -> BTreeMap<u64, Vec<u8>> {
    let ws = i.catalog.ws();
    let records = bicdb_sql::graph_adjacency::GraphRecords::resolve(
        &mut i.catalog,
        "kg",
        bicdb_common::seq::CommitSeq::from_raw(i.engine.current_seq() + 1).unwrap(),
    )
    .unwrap();
    let view =
        ReadView::new(bicdb_common::seq::CommitSeq::from_raw(i.engine.current_seq() + 1).unwrap());
    let file = i.catalog.file_mut();
    i.engine.with_read_context(|pool, chain| {
        let seg = bicdb_storage::segment::Segment::open_pooled(pool, file, records.heap.block, ws)
            .unwrap();
        let blocks = seg.data_blocks(seg.hwm());
        let fid = seg.file_id();
        drop(seg);
        let mut scan = bicdb_storage::scan::HeapScanner::new(pool, chain, view, fid, blocks);
        let shape = bicdb_exec::RowShape::new(vec![
            bicdb_exec::ColKind::Number,
            bicdb_exec::ColKind::Bytes,
        ]);
        let mut rows = BTreeMap::new();
        while let Some((_, bytes)) = scan.next_row().unwrap() {
            let row = bicdb_exec::decode_row(&bytes, &shape).unwrap();
            let [bicdb_exec::Value::Number(key), bicdb_exec::Value::Bytes(data)] =
                row.values.as_slice()
            else {
                panic!("{row:?}")
            };
            assert!(rows
                .insert(key.to_string().parse().unwrap(), data.clone())
                .is_none());
        }
        rows
    })
}
const SEED:&str="CREATE GRAPH kg; CYPHER kg 'CREATE (a:N {name:\"a\",db_type:\"d\"}),(b:N {name:\"b\",db_type:\"d\"}),(a)-[:LINK {name:\"oldword\",db_type:\"d\"}]->(b),(a)-[:LINK {name:\"parallel\",db_type:\"d\"}]->(b),(a)-[:LINK {name:\"loop\",db_type:\"d\"}]->(a)'";

fn legacy_seed(i: &mut Instance) {
    bicdb_catalog::ddl::create_graph(&mut i.catalog, i.engine, "kg").unwrap();
    session(i).execute(SEED.split_once(';').unwrap().1).unwrap();
    assert!(!physical(i)[&0].starts_with(b"BICGRV3\0"));
    assert!(
        bicdb_catalog::ddl::graph_physical_routes(&mut i.catalog, "kg")
            .unwrap()
            .is_none()
    );
}

#[test]
fn sql_failed_native_create_withdraws_graph_and_all_owned_metadata() {
    let f = Fixture::new("native-create");
    let mut i = create_instance(&f.params(), None).unwrap();
    let dictionaries = ["obj$", "tab$", "seg$", "ind$", "icol$"];
    let counts = |s: &mut Session<'_, '_, '_, '_>| {
        dictionaries
            .iter()
            .map(|name| rows(s, &format!("SELECT count(*) FROM {name}")))
            .collect::<Vec<_>>()
    };
    let before = counts(&mut session(&mut i));
    {
        let mut s = session(&mut i);
        s.set_graph_limits(bicdb_graph::Limits {
            max_expansions: 1,
            ..Default::default()
        })
        .unwrap();
        let failure = s.execute("CREATE GRAPH kg").unwrap_err().to_string();
        assert!(failure.contains("budget"), "{failure}");
        assert!(rows(&mut s, "SHOW GRAPHS").is_empty());
        assert_eq!(
            counts(&mut s),
            before,
            "manifest failure must withdraw the named graph and all owned metadata"
        );
    }
    i.shutdown().unwrap();
}

#[test]
fn sql_new_graph_publishes_native_layout_in_one_commit() {
    let f = Fixture::new("native-first-commit");
    let mut i = create_instance(&f.params(), None).unwrap();
    // A rolled-back DDL reserves a commit sequence too; use a fresh instance
    // so this assertion measures CREATE, rather than the preceding failure.
    let old = i.engine.current_seq();
    session(&mut i).execute("CREATE GRAPH kg").unwrap();
    assert_eq!(
        i.engine.current_seq(),
        old + 1,
        "CREATE must not commit a legacy graph first"
    );
    let stored = physical(&mut i);
    assert_eq!(stored[&0].len(), 180);
    assert!(stored[&0].starts_with(b"BICGRV3\0"));
    let (proof_lower, proof_upper) = bicdb_graph::corpus_proof::RecordKey::Root.range().unwrap();
    assert_eq!(
        stored.range(proof_lower..=proof_upper).count(),
        2,
        "empty graph publishes the independent proof root with its checksum"
    );
    assert_eq!(stored.len(), 3);
    let routes = bicdb_catalog::ddl::graph_physical_routes(&mut i.catalog, "kg")
        .unwrap()
        .unwrap();
    let seq = bicdb_common::seq::CommitSeq::from_raw(i.engine.current_seq()).unwrap();
    assert_eq!(i.catalog.indexes_of(seq, routes.graph).unwrap().len(), 8);
    let created = i.engine.current_seq();
    session(&mut i)
        .execute("ALTER GRAPH kg UPGRADE STORAGE")
        .unwrap();
    assert_eq!(
        i.engine.current_seq(),
        created,
        "new native graph upgrade is already complete"
    );
    assert!(session(&mut i).execute("CREATE GRAPH kg").is_err());
    assert_eq!(physical(&mut i), stored);
    let label = "TYPE_".to_owned() + &"长".repeat(1500);
    session(&mut i).execute(&query(&format!("CREATE (a:N {{name:\"a\"}}),(b:N {{name:\"b\"}}),(a)-[:`{label}` {{name:\"native\"}}]->(b)"))).unwrap();
    assert_eq!(count(&mut session(&mut i)), "1");
    assert!(!physical(&mut i).keys().any(|key| key >> 60 == 2));
    i.shutdown().unwrap();
    drop(i);
    let mut i = open_instance(&f.params()).unwrap();
    assert_eq!(count(&mut session(&mut i)), "1");
    i.shutdown().unwrap();
}

#[test]
fn sql_native_create_flushed_loser_removes_graph_routes_and_manifest() {
    let f = Fixture::new("native-create-crash");
    let mut i = create_instance(&f.params(), None).unwrap();
    session(&mut i).execute(SEED).unwrap();
    let original = physical(&mut i);
    let metadata = |s: &mut Session<'_, '_, '_, '_>| {
        ["obj$", "tab$", "seg$", "ind$", "icol$"]
            .iter()
            .map(|name| rows(s, &format!("SELECT count(*) FROM {name}")))
            .collect::<Vec<_>>()
    };
    let before = metadata(&mut session(&mut i));
    i.shutdown().unwrap();
    drop(i);
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "sql_native_create_crash_child",
            "--ignored",
            "--nocapture",
        ])
        .env("BICDB_NATIVE_CREATE_CRASH_ROOT", &f.0)
        .output()
        .unwrap();
    assert_eq!(
        child.status.code(),
        Some(99),
        "{}{}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
    let mut i = open_instance(&f.params()).unwrap();
    assert_eq!(physical(&mut i), original);
    assert_eq!(metadata(&mut session(&mut i)), before);
    assert!(session(&mut i)
        .execute("CYPHER lost_kg 'MATCH (n) RETURN count(n)'")
        .is_err());
    session(&mut i).execute("CREATE GRAPH lost_kg").unwrap();
    assert_eq!(
        rows(
            &mut session(&mut i),
            "CYPHER lost_kg 'MATCH (n) RETURN count(n)'"
        )[0][0],
        "0"
    );
    i.shutdown().unwrap();
}

#[test]
#[ignore = "parent executes the real uncommitted native CREATE crash"]
fn sql_native_create_crash_child() {
    let root = std::env::var_os("BICDB_NATIVE_CREATE_CRASH_ROOT").expect("parent fixture");
    let params = InstanceParams::for_init(std::path::Path::new(&root), None, &[]).unwrap();
    let mut i = open_instance(&params).unwrap();
    bicdb_catalog::ddl::create_graph_with_physical_routes(
        &mut i.catalog,
        i.engine,
        "lost_kg",
        |heap, primary, routes, cat, pool, log, chain, txn| {
            // A valid empty v3 manifest and its ordinal entrance are durable
            // before the process exits; no COMMIT or destructor is run.
            let ws = cat.ws();
            let file = cat.file_mut();
            let mut header = b"BICGRV3\0".to_vec();
            header.extend(ws);
            header.extend((file.file_id() as u32).to_be_bytes());
            for route in [
                heap,
                primary,
                routes.adjacency,
                routes.source,
                routes.locator,
                routes.incoming,
            ] {
                header.extend(route.obj.to_be_bytes());
                header.extend(route.block.to_be_bytes());
            }
            for n in [1u64, 0, 0] {
                header.extend(n.to_be_bytes());
            }
            let mut sha = bicdb_common::sha256::Sha256::new();
            sha.update(&header);
            header.extend(sha.finalize());
            assert_eq!(header.len(), 124);
            let shape = bicdb_exec::RowShape::new(vec![
                bicdb_exec::ColKind::Number,
                bicdb_exec::ColKind::Bytes,
            ]);
            let bytes = bicdb_exec::encode_row(
                &bicdb_exec::Row::new(vec![
                    bicdb_exec::Value::Number(bicdb_types::Number::parse("0").unwrap()),
                    bicdb_exec::Value::Bytes(header),
                ]),
                &shape,
            )
            .unwrap();
            let key = bicdb_catalog::row::key_from_row(&bytes, &[0]).unwrap();
            let rid = bicdb_access::TableAccess::new(pool, ws).insert(
                log,
                chain,
                txn,
                file,
                heap.block,
                &bytes,
                &bicdb_storage::heap::InsertPolicy::in_place(0),
            )?;
            let tree = bicdb_access::index::insert_entry(
                pool,
                log,
                file,
                ws,
                primary.block,
                txn,
                &key,
                rid,
            )?;
            bicdb_access::index::write_tree_head_redo(
                pool,
                log,
                file,
                ws,
                primary.block,
                txn,
                tree,
            )?;
            log.flush(log.appended_lsn()).unwrap();
            pool.flush_workspace(ws).unwrap();
            std::process::exit(99)
        },
    )
    .unwrap();
    panic!("crash callback returned");
}

#[test]
fn sql_adjacency_upgrade_preserves_ids_old_views_and_restart() {
    let f = Fixture::new("upgrade");
    let mut i = create_instance(&f.params(), None).unwrap();
    legacy_seed(&mut i);
    let before;
    {
        let mut s = session(&mut i);
        before = rows(
            &mut s,
            &query("MATCH (a)-[r]->(b) RETURN id(r),id(a),id(b),r.name ORDER BY id(r)"),
        );
    }
    let old = i.engine.current_seq();
    {
        let mut s = session(&mut i);
        s.execute("ALTER GRAPH kg UPGRADE STORAGE").unwrap();
        assert_eq!(
            rows(
                &mut s,
                &query("MATCH (a)-[r]->(b) RETURN id(r),id(a),id(b),r.name ORDER BY id(r)")
            ),
            before
        );
        assert_eq!(
            rows(
                &mut s,
                &query("MATCH (b {name:\"b\"})<-[r]-(a) RETURN count(r)")
            )[0][0],
            "2"
        );
        assert_eq!(count(&mut s), "3");
        s.execute("ALTER GRAPH kg UPGRADE STORAGE").unwrap();
    }
    let stored = physical(&mut i);
    assert!(stored[&0].starts_with(b"BICGRV3\0"));
    assert!(!stored.keys().any(|k| k >> 60 == 2));
    assert!(
        bicdb_catalog::ddl::graph_physical_routes(&mut i.catalog, "kg")
            .unwrap()
            .is_some()
    );
    {
        let mut s = Session::new(i.pool, i.engine, &mut i.catalog, old);
        assert_eq!(
            rows(
                &mut s,
                &query("MATCH (a)-[r]->(b) RETURN id(r),id(a),id(b),r.name ORDER BY id(r)")
            ),
            before
        );
    }
    i.shutdown().unwrap();
    drop(i);
    let mut i = open_instance(&f.params()).unwrap();
    assert_eq!(count(&mut session(&mut i)), "3");
    i.shutdown().unwrap();
}

#[test]
fn sql_adjacency_writes_share_transaction_with_indexes_and_fulltext() {
    let f = Fixture::new("writes");
    let mut i = create_instance(&f.params(), None).unwrap();
    {
        let mut s = session(&mut i);
        s.execute(SEED).unwrap();
        s.execute("CREATE UNIQUE GRAPH INDEX names ON kg NODES (name); CREATE FULLTEXT GRAPH INDEX words ON kg RELATIONSHIPS TYPE \"LINK\" (name) OPTIONS '{\"update\":\"manual\"}'; ALTER GRAPH kg UPGRADE STORAGE").unwrap();
        let big = "文".repeat(70000);
        s.execute(&query(&format!("MATCH (a {{name:\"a\"}})-[r {{name:\"oldword\"}}]->(b) SET r.name=\"newword\",r.payload=\"{big}\""))).unwrap();
        assert_eq!(
            rows(
                &mut s,
                &query("MATCH ()-[r]->() RETURN r.name ORDER BY r.name")
            ),
            vec![
                vec!["loop".to_string()],
                vec!["newword".to_string()],
                vec!["parallel".to_string()]
            ]
        );
        assert_eq!(rows(&mut s,"SEARCH FULLTEXT GRAPH INDEX words ON kg FOR 'newword' OPTIONS '{\"db_type\":\"d\",\"consistency\":\"strict\"}'").len(),1);
        assert!(rows(&mut s,"SEARCH FULLTEXT GRAPH INDEX words ON kg FOR 'newword' OPTIONS '{\"db_type\":\"d\",\"consistency\":\"eventual\"}'").is_empty());
        s.execute("ALTER FULLTEXT GRAPH INDEX words ON kg SYNC")
            .unwrap();
        assert_eq!(rows(&mut s,"SEARCH FULLTEXT GRAPH INDEX words ON kg FOR 'newword' OPTIONS '{\"db_type\":\"d\",\"consistency\":\"eventual\"}'").len(),1);
        let previous = count(&mut s);
        s.execute("BEGIN").unwrap();
        s.execute(&query("MATCH (a {name:\"a\"}),(b {name:\"b\"}) CREATE (a)-[:LINK {name:\"uncommitted\",db_type:\"d\"}]->(b)")).unwrap();
        assert_eq!(count(&mut s), "4");
        assert!(s.execute(&query("CREATE (:N {name:\"a\"})")).is_err());
        assert_eq!(count(&mut s), "4", "failed statement keeps earlier success");
        s.execute("ROLLBACK").unwrap();
        assert_eq!(count(&mut s), previous);
        assert!(rows(&mut s,"SEARCH FULLTEXT GRAPH INDEX words ON kg FOR 'uncommitted' OPTIONS '{\"db_type\":\"d\",\"consistency\":\"strict\"}'").is_empty());
        s.execute(&query("MATCH (b {name:\"b\"}) DETACH DELETE b"))
            .unwrap();
        assert_eq!(count(&mut s), "1");
        assert_eq!(
            rows(
                &mut s,
                "SELECT n FROM GRAPH_TABLE(kg,'MATCH (n) RETURN n.name' COLUMNS(n VARCHAR2(32)))"
            ),
            vec![vec!["a".to_string()]]
        );
        s.execute("ALTER FULLTEXT GRAPH INDEX words ON kg SYNC")
            .unwrap();
        assert!(rows(&mut s,"SEARCH FULLTEXT GRAPH INDEX words ON kg FOR 'newword' OPTIONS '{\"db_type\":\"d\",\"consistency\":\"eventual\"}'").is_empty());
    }
    assert!(!physical(&mut i).keys().any(|k| k >> 60 == 2));
    i.shutdown().unwrap();
}

#[test]
fn graph_table_element_handles_are_typed_and_graph_scoped() {
    let f = Fixture::new("element-handles");
    let mut i = create_instance(&f.params(), None).unwrap();
    let mut s = session(&mut i);
    s.execute(
        "CREATE GRAPH kg; CREATE GRAPH other; \
         CYPHER kg 'CREATE (a:N {name:\"a\"}),(b:N {name:\"b\"}),(a)-[:LINK]->(b)'; \
         CYPHER other 'CREATE (:N {name:\"foreign\"})'",
    )
    .unwrap();

    let result = s
        .execute("SELECT x.entity,x.name FROM GRAPH_TABLE(kg,'MATCH (n) RETURN n,n.name ORDER BY n.name' COLUMNS(entity GRAPH_ELEMENT,name VARCHAR2(32))) x")
        .unwrap();
    let Some(QueryResult::Rows {
        columns,
        rows: values,
    }) = result.last()
    else {
        panic!("{result:?}")
    };
    assert_eq!(columns[0].kind, bicdb_exec::ColKind::GraphElement);
    assert_eq!(values.len(), 2);
    assert!(matches!(values[0][0], bicdb_exec::Value::GraphElement(_)));

    assert_eq!(
        rows(
            &mut s,
            "SELECT a.name,b.name FROM \
             GRAPH_TABLE(kg,'MATCH (n) RETURN n,n.name' COLUMNS(entity GRAPH_ELEMENT,name VARCHAR2(32))) a \
             JOIN GRAPH_TABLE(kg,'MATCH (n) RETURN n,n.name' COLUMNS(entity GRAPH_ELEMENT,name VARCHAR2(32))) b \
             ON a.entity=b.entity ORDER BY a.name",
        ),
        vec![
            vec!["a".to_string(), "a".to_string()],
            vec!["b".to_string(), "b".to_string()],
        ]
    );
    assert!(rows(
        &mut s,
        "SELECT a.name,b.name FROM \
         GRAPH_TABLE(kg,'MATCH (n) RETURN n,n.name' COLUMNS(entity GRAPH_ELEMENT,name VARCHAR2(32))) a \
         JOIN GRAPH_TABLE(other,'MATCH (n) RETURN n,n.name' COLUMNS(entity GRAPH_ELEMENT,name VARCHAR2(32))) b \
         ON a.entity=b.entity",
    )
    .is_empty());
    assert!(rows(
        &mut s,
        "SELECT a.entity FROM \
         GRAPH_TABLE(kg,'MATCH (n) RETURN n' COLUMNS(entity GRAPH_ELEMENT)) a \
         JOIN GRAPH_TABLE(kg,'MATCH ()-[r]->() RETURN r' COLUMNS(entity GRAPH_ELEMENT)) b \
         ON a.entity=b.entity",
    )
    .is_empty());
    drop(s);
    i.shutdown().unwrap();
}

#[test]
fn sql_adjacency_long_types_templates_and_atomic_failed_upgrade() {
    let f = Fixture::new("long");
    let mut i = create_instance(&f.params(), None).unwrap();
    legacy_seed(&mut i);
    {
        let mut s = session(&mut i);
        s.execute("BEGIN").unwrap();
        assert!(s.execute("ALTER GRAPH kg UPGRADE STORAGE").is_err());
        s.execute("ROLLBACK").unwrap();
    }
    let before = physical(&mut i);
    {
        let mut s = session(&mut i);
        let limits = bicdb_graph::Limits {
            max_expansions: 3,
            ..bicdb_graph::Limits::default()
        };
        s.set_graph_limits(limits).unwrap();
        assert!(s.execute("ALTER GRAPH kg UPGRADE STORAGE").is_err());
    }
    assert_eq!(physical(&mut i), before);
    assert!(
        bicdb_catalog::ddl::graph_physical_routes(&mut i.catalog, "kg")
            .unwrap()
            .is_none()
    );
    let snapshot;
    {
        let mut s = session(&mut i);
        s.execute("ALTER GRAPH kg UPGRADE STORAGE").unwrap();
        let label = "TYPE_".to_string() + &"长".repeat(1500);
        s.execute(&query(&format!("MATCH (a {{name:\"a\"}}),(b {{name:\"b\"}}) CREATE (b)-[:`{label}` {{name:\"long\"}}]->(a)"))).unwrap();
        assert_eq!(
            rows(
                &mut s,
                &query(&format!(
                    "MATCH (a {{name:\"a\"}})<-[r:`{label}`]-(b) RETURN r.name"
                ))
            )[0][0],
            "long"
        );
        let (schema, snapshots) = s.graph_initialization_snapshot(true).unwrap();
        assert!(schema.contains("CREATE GRAPH"));
        assert_eq!(snapshots.len(), 1);
        assert_eq!(
            bicdb_graph::Graph::from_bytes(&snapshots[0].data, &bicdb_graph::Limits::default())
                .unwrap()
                .edges()
                .len(),
            4
        );
        snapshot = snapshots[0].clone();
    }
    let target = Fixture::new("restored");
    let mut restored = create_instance(&target.params(), None).unwrap();
    {
        let mut s = session(&mut restored);
        s.execute("CREATE GRAPH kg").unwrap();
        s.restore_graph_snapshot(&snapshot).unwrap();
        assert_eq!(count(&mut s), "4");
    }
    assert!(physical(&mut restored)[&0].starts_with(b"BICGRV3\0"));
    restored.shutdown().unwrap();
    i.shutdown().unwrap();
}

#[test]
fn sql_adjacency_budget_failure_rolls_back_native_rows_and_journal() {
    let f = Fixture::new("budget");
    let mut i = create_instance(&f.params(), None).unwrap();
    {
        let mut s = session(&mut i);
        s.execute("CREATE GRAPH kg; ALTER GRAPH kg UPGRADE STORAGE; CREATE FULLTEXT GRAPH INDEX words ON kg RELATIONSHIPS (name) OPTIONS '{\"update\":\"manual\"}'").unwrap();
    }
    let before = physical(&mut i);
    {
        let mut s = session(&mut i);
        let limits = bicdb_graph::Limits {
            max_expansions: 15,
            ..bicdb_graph::Limits::default()
        };
        s.set_graph_limits(limits).unwrap();
        let error=s.execute(&query("CREATE (a:N {db_type:\"d\"}),(b:N {db_type:\"d\"}),(a)-[:LINK {name:\"failedword\",db_type:\"d\"}]->(b)")).unwrap_err();
        let error = error.to_string();
        assert!(
            error.contains("native adjacency work/byte budget")
                || error.contains("corpus proof work/byte budget"),
            "{error}"
        );
    }
    assert_eq!(
        physical(&mut i),
        before,
        "native partial writes and manifest are rolled back"
    );
    {
        let mut s = session(&mut i);
        assert_eq!(count(&mut s), "0");
        s.execute("ALTER FULLTEXT GRAPH INDEX words ON kg SYNC")
            .unwrap();
        assert!(rows(&mut s,"SEARCH FULLTEXT GRAPH INDEX words ON kg FOR 'failedword' OPTIONS '{\"db_type\":\"d\",\"consistency\":\"eventual\"}'").is_empty());
        s.execute(&query("CREATE (a:N {db_type:\"d\"}),(b:N {db_type:\"d\"}),(a)-[:LINK {name:\"goodword\",db_type:\"d\"}]->(b)")).unwrap();
        assert_eq!(count(&mut s), "1");
    }
    i.shutdown().unwrap();
}

#[test]
fn sql_adjacency_real_crash_restores_graph_indexes_and_fulltext() {
    let f = Fixture::new("crash");
    let mut i = create_instance(&f.params(), None).unwrap();
    {
        let mut s = session(&mut i);
        s.execute(SEED).unwrap();
        s.execute("ALTER GRAPH kg UPGRADE STORAGE; CREATE FULLTEXT GRAPH INDEX words ON kg RELATIONSHIPS (name) OPTIONS '{\"update\":\"manual\"}'").unwrap();
    }
    let before = physical(&mut i);
    i.shutdown().unwrap();
    drop(i);
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "sql_adjacency_crash_child",
            "--nocapture",
        ])
        .env("BICDB_SQL_ADJACENCY_CRASH_ROOT", &f.0)
        .output()
        .unwrap();
    assert_eq!(
        child.status.code(),
        Some(99),
        "{}",
        String::from_utf8_lossy(&child.stderr)
    );
    let mut i = open_instance(&f.params()).unwrap();
    assert_eq!(physical(&mut i), before);
    {
        let mut s = session(&mut i);
        assert_eq!(count(&mut s), "3");
        assert_eq!(
            rows(
                &mut s,
                &query("MATCH ()-[r {name:\"oldword\"}]->() RETURN r.name")
            )[0][0],
            "oldword"
        );
        assert_eq!(rows(&mut s,"SEARCH FULLTEXT GRAPH INDEX words ON kg FOR 'oldword' OPTIONS '{\"db_type\":\"d\",\"consistency\":\"eventual\"}'").len(),1);
        s.execute("ALTER FULLTEXT GRAPH INDEX words ON kg SYNC")
            .unwrap();
        assert!(rows(&mut s,"SEARCH FULLTEXT GRAPH INDEX words ON kg FOR 'loserword' OPTIONS '{\"db_type\":\"d\",\"consistency\":\"strict\"}'").is_empty());
    }
    i.shutdown().unwrap();
}

#[test]
#[ignore = "parent executes the real SQL crash child"]
fn sql_adjacency_crash_child() {
    let path = PathBuf::from(std::env::var_os("BICDB_SQL_ADJACENCY_CRASH_ROOT").unwrap());
    let mut i = open_instance(&InstanceParams::for_init(&path, None, &[]).unwrap()).unwrap();
    let pool = i.pool;
    let engine = i.engine;
    let ws = i.catalog.ws();
    let mut s = session(&mut i);
    s.execute("BEGIN").unwrap();
    let big = "文".repeat(70000);
    s.execute(&query(&format!(
        "MATCH ()-[r {{name:\"oldword\"}}]->() SET r.name=\"loserword\",r.payload=\"{big}\""
    )))
    .unwrap();
    s.execute(&query("MATCH (a {name:\"a\"}),(b {name:\"b\"}) CREATE (a)-[:LINK {name:\"loserword\",db_type:\"d\"}]->(b)")).unwrap();
    assert_eq!(count(&mut s), "4");
    // The SQL transaction remains active; exit bypasses Session::drop.
    let mut flush_context = engine.begin().unwrap();
    engine.with_write_context(&mut flush_context, |_, log, _, _| {
        log.flush(log.appended_lsn()).unwrap();
        pool.flush_workspace(ws).unwrap();
        std::process::exit(99)
    });
}

#[test]
fn sql_adjacency_old_v3_view_keeps_deleted_nodes_and_external_properties() {
    let f = Fixture::new("old-v3");
    let mut i = create_instance(&f.params(), None).unwrap();
    {
        let mut s = session(&mut i);
        s.execute(SEED).unwrap();
        s.execute("ALTER GRAPH kg UPGRADE STORAGE").unwrap();
    }
    let old = i.engine.current_seq();
    {
        let mut s = session(&mut i);
        let big = "文".repeat(70000);
        s.execute(&query(&format!(
            "MATCH ()-[r {{name:\"oldword\"}}]->() SET r.name=\"changed\",r.payload=\"{big}\""
        )))
        .unwrap();
        s.execute(&query("MATCH (b {name:\"b\"}) DETACH DELETE b"))
            .unwrap();
        assert_eq!(count(&mut s), "1");
    }
    {
        let mut s = Session::new(i.pool, i.engine, &mut i.catalog, old);
        assert_eq!(count(&mut s), "3");
        assert_eq!(
            rows(
                &mut s,
                &query("MATCH ()-[r {name:\"oldword\"}]->() RETURN r.name")
            )[0][0],
            "oldword"
        );
    }
    i.shutdown().unwrap();
}

#[test]
fn sql_adjacency_rejects_foreign_manifest_route_with_valid_checksum() {
    let f = Fixture::new("foreign-manifest");
    let mut i = create_instance(&f.params(), None).unwrap();
    {
        let mut s = session(&mut i);
        s.execute(SEED).unwrap();
        s.execute(
            "ALTER GRAPH kg UPGRADE STORAGE; CREATE GRAPH other; ALTER GRAPH other UPGRADE STORAGE",
        )
        .unwrap();
    }
    let foreign = bicdb_catalog::ddl::graph_physical_routes(&mut i.catalog, "other")
        .unwrap()
        .unwrap();
    let mut header = physical(&mut i)[&0].clone();
    header[44..48].copy_from_slice(&foreign.source.obj.to_be_bytes());
    header[48..52].copy_from_slice(&foreign.source.block.to_be_bytes());
    let mut sha = bicdb_common::sha256::Sha256::new();
    let checksum = header.len() - 32;
    sha.update(&header[..checksum]);
    header[checksum..].copy_from_slice(&sha.finalize());
    let records = bicdb_sql::graph_adjacency::GraphRecords::resolve(
        &mut i.catalog,
        "kg",
        bicdb_common::seq::CommitSeq::from_raw(i.engine.current_seq() + 1).unwrap(),
    )
    .unwrap();
    let view =
        ReadView::new(bicdb_common::seq::CommitSeq::from_raw(i.engine.current_seq() + 1).unwrap());
    let ws = i.catalog.ws();
    let file = i.catalog.file_mut();
    let shape = bicdb_exec::RowShape::new(vec![
        bicdb_exec::ColKind::Number,
        bicdb_exec::ColKind::Bytes,
    ]);
    let rid = i.engine.with_read_context(|pool, chain| {
        let seg = bicdb_storage::segment::Segment::open_pooled(pool, file, records.heap.block, ws)
            .unwrap();
        let blocks = seg.data_blocks(seg.hwm());
        let fid = seg.file_id();
        drop(seg);
        let mut scan = bicdb_storage::scan::HeapScanner::new(pool, chain, view, fid, blocks);
        while let Some((rid, bytes)) = scan.next_row().unwrap() {
            let row = bicdb_exec::decode_row(&bytes, &shape).unwrap();
            if format_value(&row.values[0]) == "0" {
                return rid;
            }
        }
        panic!("missing manifest");
    });
    let bytes = bicdb_exec::encode_row(
        &bicdb_exec::Row::new(vec![
            bicdb_exec::Value::Number(bicdb_types::Number::parse("0").unwrap()),
            bicdb_exec::Value::Bytes(header),
        ]),
        &shape,
    )
    .unwrap();
    let mut t = i.engine.begin().unwrap();
    i.engine
        .with_write_context(&mut t, |pool, log, chain, txn| {
            bicdb_access::TableAccess::new(pool, ws)
                .update(
                    log,
                    chain,
                    txn,
                    file,
                    records.heap.block,
                    rid,
                    &bytes,
                    &bicdb_storage::heap::InsertPolicy::in_place(0),
                )
                .unwrap();
        });
    i.engine.commit(&mut t).unwrap();
    let error = session(&mut i)
        .execute(&query("MATCH ()-[r]->() RETURN count(r)"))
        .unwrap_err();
    assert!(
        error.to_string().contains("belongs to another graph"),
        "{error}"
    );
    i.shutdown().unwrap();
}

fn edge_rows(s: &mut Session<'_, '_, '_, '_>) -> Vec<Vec<String>> {
    rows(
        s,
        &query(
            "MATCH (a)-[r]->(b) RETURN id(r),id(a),id(b),type(r),r.name,r.payload ORDER BY id(r)",
        ),
    )
}
fn routes(i: &mut Instance) -> bicdb_catalog::ddl::GraphPhysicalRoutes {
    bicdb_catalog::ddl::graph_physical_routes(&mut i.catalog, "kg")
        .unwrap()
        .unwrap()
}
fn graph_copy(i: &mut Instance) -> bicdb_graph::Graph {
    let (_, snapshots) = session(i).graph_initialization_snapshot(true).unwrap();
    assert_eq!(snapshots.len(), 1);
    bicdb_graph::Graph::from_bytes(&snapshots[0].data, &bicdb_graph::Limits::default()).unwrap()
}
fn metadata_counts(i: &mut Instance) -> Vec<Vec<Vec<String>>> {
    ["obj$", "tab$", "seg$", "ind$", "icol$", "stat$"]
        .iter()
        .map(|name| rows(&mut session(i), &format!("SELECT count(*) FROM {name}")))
        .collect()
}

#[test]
fn sql_storage_rebuild_keeps_ids_snapshots_indexes_and_pending_fulltext() {
    let f = Fixture::new("rebuild-snapshots");
    let mut i = create_instance(&f.params(), None).unwrap();
    let label = "TYPE_".to_owned() + &"长".repeat(1500);
    let big = "文".repeat(70000);
    {
        let mut s = session(&mut i);
        s.execute(SEED).unwrap();
        s.execute("CREATE UNIQUE GRAPH INDEX names ON kg NODES (name); CREATE FULLTEXT GRAPH INDEX words ON kg RELATIONSHIPS (name) OPTIONS '{\"update\":\"manual\"}'").unwrap();
        s.execute(&query(&format!("MATCH (a {{name:\"a\"}}),(b {{name:\"b\"}}) CREATE (b)-[:`{label}` {{name:\"longword\",db_type:\"d\",payload:\"{big}\"}}]->(a)"))).unwrap();
        s.execute(&query(
            "MATCH ()-[r {name:\"oldword\"}]->() SET r.name=\"pendingword\"",
        ))
        .unwrap();
    }
    let before = edge_rows(&mut session(&mut i));
    let ft = rows(&mut session(&mut i), "SHOW FULLTEXT GRAPH INDEXES ON kg");
    let counts = metadata_counts(&mut i);
    let manifest = physical(&mut i)[&0].clone();
    let old = routes(&mut i);
    let snapshot = i.engine.current_seq();
    let seq = bicdb_common::seq::CommitSeq::from_raw(snapshot).unwrap();
    // Warm names before same-name replacement; namespace invalidation matters.
    for suffix in ["adj", "source", "locator", "reverse"] {
        i.catalog
            .resolve(
                seq,
                bicdb_catalog::dict::namespace::INDEX,
                &format!("i_graph_{}_{suffix}$", old.graph),
            )
            .unwrap();
    }
    let preserved: Vec<_> = i
        .catalog
        .indexes_of(seq, old.graph)
        .unwrap()
        .into_iter()
        .filter(|idx| {
            ![
                old.adjacency.obj,
                old.source.obj,
                old.locator.obj,
                old.incoming.obj,
            ]
            .contains(&idx.obj)
        })
        .collect();
    session(&mut i)
        .execute("ALTER GRAPH kg REBUILD STORAGE")
        .unwrap();
    assert_eq!(
        i.engine.current_seq(),
        snapshot + 1,
        "replacement and manifest have one commit"
    );
    let new = routes(&mut i);
    assert_eq!(new.graph, old.graph);
    for (left, right) in [
        (old.adjacency, new.adjacency),
        (old.source, new.source),
        (old.locator, new.locator),
        (old.incoming, new.incoming),
    ] {
        assert_ne!(left.obj, right.obj);
        assert_ne!(left.block, right.block);
    }
    let current = bicdb_common::seq::CommitSeq::from_raw(i.engine.current_seq()).unwrap();
    for (suffix, obj) in [
        ("adj", new.adjacency.obj),
        ("source", new.source.obj),
        ("locator", new.locator.obj),
        ("reverse", new.incoming.obj),
    ] {
        let name = format!("i_graph_{}_{suffix}$", new.graph);
        assert_eq!(
            i.catalog
                .resolve(current, bicdb_catalog::dict::namespace::INDEX, &name)
                .unwrap()
                .obj,
            obj
        );
        // Generic Catalog::resolve is current-state by design. Its explicit
        // snapshot lookup must find the previous same-name dictionary row.
        let namespace =
            bicdb_catalog::open::comp_num(u64::from(bicdb_catalog::dict::namespace::INDEX));
        let key = bicdb_catalog::open::comp_text(&name);
        let historical = i.engine.with_read_context(|pool, chain| {
            i.catalog
                .lookup_snapshot(
                    "i_obj_name",
                    &[Some(&namespace), Some(&key)],
                    pool,
                    chain,
                    ReadView::new(seq),
                    &mut 1000,
                )
                .unwrap()
                .unwrap()
                .1
        });
        assert_ne!(
            bicdb_catalog::cache::ObjRow::from_values(&historical)
                .unwrap()
                .obj,
            obj
        );
    }
    let after: Vec<_> = i
        .catalog
        .indexes_of(current, new.graph)
        .unwrap()
        .into_iter()
        .filter(|idx| {
            ![
                new.adjacency.obj,
                new.source.obj,
                new.locator.obj,
                new.incoming.obj,
            ]
            .contains(&idx.obj)
        })
        .collect();
    assert_eq!(after, preserved);
    assert_eq!(metadata_counts(&mut i), counts);
    let next = physical(&mut i)[&0].clone();
    assert_eq!(
        &manifest[..36],
        &next[..36],
        "heap and ordinal route unchanged"
    );
    assert_eq!(
        &manifest[68..92],
        &next[68..92],
        "logical allocation and counts unchanged"
    );
    assert_eq!(edge_rows(&mut session(&mut i)), before);
    assert_eq!(
        rows(&mut session(&mut i), "SHOW FULLTEXT GRAPH INDEXES ON kg"),
        ft,
        "maintenance cannot enqueue content changes or advance FT cursor"
    );
    assert_eq!(
        edge_rows(&mut Session::new(
            i.pool,
            i.engine,
            &mut i.catalog,
            snapshot
        )),
        before
    );
    {
        let mut s = session(&mut i);
        assert_eq!(rows(&mut s,"SEARCH FULLTEXT GRAPH INDEX words ON kg FOR 'pendingword' OPTIONS '{\"db_type\":\"d\",\"consistency\":\"strict\"}'").len(),1);
        assert!(rows(&mut s,"SEARCH FULLTEXT GRAPH INDEX words ON kg FOR 'pendingword' OPTIONS '{\"db_type\":\"d\",\"consistency\":\"eventual\"}'").is_empty());
        s.execute("ALTER FULLTEXT GRAPH INDEX words ON kg SYNC")
            .unwrap();
        assert_eq!(rows(&mut s,"SEARCH FULLTEXT GRAPH INDEX words ON kg FOR 'longword' OPTIONS '{\"db_type\":\"d\",\"consistency\":\"eventual\"}'").len(),1);
        s.execute(&query(
            "MATCH ()-[r {name:\"longword\"}]->() SET r.name=\"laterword\",r.payload=\"small\"",
        ))
        .unwrap();
        s.execute(&query("MATCH ()-[r {name:\"parallel\"}]->() DELETE r"))
            .unwrap();
        s.execute("ALTER GRAPH kg REBUILD STORAGE").unwrap();
        s.execute("ALTER FULLTEXT GRAPH INDEX words ON kg SYNC")
            .unwrap();
        assert_eq!(rows(&mut s,"SEARCH FULLTEXT GRAPH INDEX words ON kg FOR 'laterword' OPTIONS '{\"db_type\":\"d\",\"consistency\":\"eventual\"}'").len(),1);
        assert!(
            s.execute(&query("CREATE (:N {name:\"a\"})")).is_err(),
            "unique index still enforced"
        );
    }
    assert_eq!(
        edge_rows(&mut Session::new(
            i.pool,
            i.engine,
            &mut i.catalog,
            snapshot
        )),
        before,
        "original generation survives two replacements and content edits"
    );
    let final_rows = edge_rows(&mut session(&mut i));
    assert_eq!(final_rows.len(), 3);
    i.shutdown().unwrap();
    drop(i);
    let mut i = open_instance(&f.params()).unwrap();
    assert_eq!(edge_rows(&mut session(&mut i)), final_rows);
    assert_eq!(routes(&mut i).graph, old.graph);
    i.shutdown().unwrap();
}

fn query_work_threshold(i: &mut Instance, text: &str) -> usize {
    for limit in 1..256 {
        let mut s = session(i);
        s.set_graph_limits(bicdb_graph::Limits {
            max_expansions: limit,
            ..Default::default()
        })
        .unwrap();
        match s.execute(&query(text)) {
            Ok(_) => return limit,
            Err(e) => assert!(
                e.to_string().contains("budget")
                    || e.to_string().contains("work/deadline exceeded"),
                "{e}"
            ),
        }
    }
    panic!("small query never fits its native work budget");
}

#[test]
fn sql_storage_rebuild_compacts_tombstones_and_restarts_empty_sources() {
    let f = Fixture::new("rebuild-compaction");
    let mut i = create_instance(&f.params(), None).unwrap();
    session(&mut i).execute(SEED).unwrap();
    for n in 0..12 {
        session(&mut i).execute(&query(&format!("MATCH (a {{name:\"a\"}}),(b {{name:\"b\"}}) CREATE (a)-[:LINK {{name:\"ghost{n}\"}}]->(b)"))).unwrap();
    }
    session(&mut i)
        .execute(&query(
            "MATCH ()-[r]->() WHERE r.name STARTS WITH \"ghost\" DELETE r",
        ))
        .unwrap();
    let outgoing = "MATCH (a {name:\"a\"})-[r]->(b) RETURN id(r)";
    let incoming = "MATCH (b {name:\"b\"})<-[r]-(a) RETURN id(r)";
    let expected = rows(&mut session(&mut i), &query(outgoing));
    let before = query_work_threshold(&mut i, outgoing);
    let reverse_before = query_work_threshold(&mut i, incoming);
    let snapshot = i.engine.current_seq();
    session(&mut i)
        .execute("ALTER GRAPH kg REBUILD STORAGE")
        .unwrap();
    assert_eq!(rows(&mut session(&mut i), &query(outgoing)), expected);
    let after = query_work_threshold(&mut i, outgoing);
    let reverse_after = query_work_threshold(&mut i, incoming);
    assert!(
        after < before,
        "native tombstone scan cost {before} -> {after}"
    );
    assert!(
        reverse_after < reverse_before,
        "reverse candidate scan cost {reverse_before} -> {reverse_after}"
    );
    assert_eq!(
        count(&mut Session::new(
            i.pool,
            i.engine,
            &mut i.catalog,
            snapshot
        )),
        "3"
    );
    session(&mut i)
        .execute(&query("MATCH ()-[r]->() DELETE r"))
        .unwrap();
    session(&mut i)
        .execute("ALTER GRAPH kg REBUILD STORAGE")
        .unwrap();
    assert_eq!(count(&mut session(&mut i)), "0");
    session(&mut i)
        .execute(&query(
            "MATCH (a {name:\"a\"}),(b {name:\"b\"}) CREATE (a)-[:LINK {name:\"fresh\"}]->(b)",
        ))
        .unwrap();
    assert_eq!(count(&mut session(&mut i)), "1");
    i.shutdown().unwrap();
}

#[test]
fn sql_storage_rebuild_requires_native_idle_graph_and_handles_empty_graph() {
    let f = Fixture::new("rebuild-contract");
    let mut i = create_instance(&f.params(), None).unwrap();
    legacy_seed(&mut i);
    let before = physical(&mut i);
    let err = session(&mut i)
        .execute("ALTER GRAPH kg REBUILD STORAGE")
        .unwrap_err()
        .to_string();
    assert!(err.contains("upgrade legacy"), "{err}");
    assert_eq!(physical(&mut i), before);
    assert!(
        bicdb_catalog::ddl::graph_physical_routes(&mut i.catalog, "kg")
            .unwrap()
            .is_none()
    );
    session(&mut i)
        .execute("ALTER GRAPH kg UPGRADE STORAGE")
        .unwrap();
    {
        let mut s = session(&mut i);
        s.execute("BEGIN").unwrap();
        assert!(s
            .execute("ALTER GRAPH kg REBUILD STORAGE")
            .unwrap_err()
            .to_string()
            .contains("idle"));
        s.execute("ROLLBACK").unwrap();
        s.execute("DROP GRAPH kg; CREATE GRAPH kg; ALTER GRAPH kg REBUILD STORAGE")
            .unwrap();
        assert_eq!(count(&mut s), "0");
        s.execute(&query("CREATE (:N {name:\"after-empty-rebuild\"})"))
            .unwrap();
    }
    i.shutdown().unwrap();
}

fn manifest_location(
    i: &mut Instance,
    records: bicdb_sql::graph_adjacency::GraphRecords,
) -> bicdb_storage::rowid::RowId {
    let ws = i.catalog.ws();
    let view =
        ReadView::new(bicdb_common::seq::CommitSeq::from_raw(i.engine.current_seq()).unwrap());
    let file = i.catalog.file_mut();
    i.engine.with_read_context(|pool, chain| {
        let segment =
            bicdb_storage::segment::Segment::open_pooled(pool, file, records.heap.block, ws)
                .unwrap();
        let fid = segment.file_id();
        let blocks = segment.data_blocks(segment.hwm());
        drop(segment);
        let mut scan = bicdb_storage::scan::HeapScanner::new(pool, chain, view, fid, blocks);
        let shape = bicdb_exec::RowShape::new(vec![
            bicdb_exec::ColKind::Number,
            bicdb_exec::ColKind::Bytes,
        ]);
        while let Some((rid, bytes)) = scan.next_row().unwrap() {
            let row = bicdb_exec::decode_row(&bytes, &shape).unwrap();
            if format_value(&row.values[0]) == "0" {
                return rid;
            }
        }
        panic!("missing graph manifest");
    })
}
fn rewrite_test_ordinal(i: &mut Instance, ordinal: u64, data: Vec<u8>) {
    let seq = bicdb_common::seq::CommitSeq::from_raw(i.engine.current_seq()).unwrap();
    let records =
        bicdb_sql::graph_adjacency::GraphRecords::resolve(&mut i.catalog, "kg", seq).unwrap();
    let ws = i.catalog.ws();
    let view = ReadView::new(seq);
    let file = i.catalog.file_mut();
    let rid = i.engine.with_read_context(|pool, chain| {
        let segment =
            bicdb_storage::segment::Segment::open_pooled(pool, file, records.heap.block, ws)
                .unwrap();
        let fid = segment.file_id();
        let blocks = segment.data_blocks(segment.hwm());
        drop(segment);
        let mut scan = bicdb_storage::scan::HeapScanner::new(pool, chain, view, fid, blocks);
        let shape = bicdb_exec::RowShape::new(vec![
            bicdb_exec::ColKind::Number,
            bicdb_exec::ColKind::Bytes,
        ]);
        while let Some((rid, bytes)) = scan.next_row().unwrap() {
            let row = bicdb_exec::decode_row(&bytes, &shape).unwrap();
            if format_value(&row.values[0]) == ordinal.to_string() {
                return rid;
            }
        }
        panic!("missing graph ordinal {ordinal}");
    });
    let bytes = bicdb_exec::encode_row(
        &bicdb_exec::Row::new(vec![
            bicdb_exec::Value::Number(bicdb_types::Number::parse(&ordinal.to_string()).unwrap()),
            bicdb_exec::Value::Bytes(data),
        ]),
        &bicdb_exec::RowShape::new(vec![
            bicdb_exec::ColKind::Number,
            bicdb_exec::ColKind::Bytes,
        ]),
    )
    .unwrap();
    let mut txn = i.engine.begin().unwrap();
    i.engine
        .with_write_context(&mut txn, |pool, log, chain, txn| {
            bicdb_access::TableAccess::new(pool, ws)
                .update(
                    log,
                    chain,
                    txn,
                    file,
                    records.heap.block,
                    rid,
                    &bytes,
                    &bicdb_storage::heap::InsertPolicy::in_place(0),
                )
                .unwrap();
        });
    i.engine.commit(&mut txn).unwrap();
}
fn replacement_manifest(
    mut header: Vec<u8>,
    routes: bicdb_catalog::ddl::GraphPhysicalRoutes,
) -> Vec<u8> {
    for (offset, route) in [
        (36, routes.adjacency),
        (44, routes.source),
        (52, routes.locator),
        (60, routes.incoming),
    ] {
        header[offset..offset + 4].copy_from_slice(&route.obj.to_be_bytes());
        header[offset + 4..offset + 8].copy_from_slice(&route.block.to_be_bytes());
    }
    let mut sha = bicdb_common::sha256::Sha256::new();
    let checksum = header.len() - 32;
    sha.update(&header[..checksum]);
    header[checksum..].copy_from_slice(&sha.finalize());
    header
}
fn manifest_row(header: Vec<u8>) -> Vec<u8> {
    bicdb_exec::encode_row(
        &bicdb_exec::Row::new(vec![
            bicdb_exec::Value::Number(bicdb_types::Number::parse("0").unwrap()),
            bicdb_exec::Value::Bytes(header),
        ]),
        &bicdb_exec::RowShape::new(vec![
            bicdb_exec::ColKind::Number,
            bicdb_exec::ColKind::Bytes,
        ]),
    )
    .unwrap()
}

#[test]
fn sql_storage_rebuild_partial_failure_restores_routes_metadata_and_manifest() {
    let f = Fixture::new("rebuild-partial-failure");
    let mut i = create_instance(&f.params(), None).unwrap();
    session(&mut i).execute(SEED).unwrap();
    session(&mut i).execute("CREATE FULLTEXT GRAPH INDEX words ON kg RELATIONSHIPS (name) OPTIONS '{\"update\":\"manual\"}'").unwrap();
    let graph = graph_copy(&mut i);
    let before = physical(&mut i);
    let old = routes(&mut i);
    let counts = metadata_counts(&mut i);
    let ft = rows(&mut session(&mut i), "SHOW FULLTEXT GRAPH INDEXES ON kg");
    for failure in ["callback", "budget", "deadline"] {
        let ws = i.catalog.ws();
        let seq = bicdb_common::seq::CommitSeq::from_raw(i.engine.current_seq()).unwrap();
        let records =
            bicdb_sql::graph_adjacency::GraphRecords::resolve(&mut i.catalog, "kg", seq).unwrap();
        let rid = manifest_location(&mut i, records);
        let mut wrote = false;
        let err = bicdb_catalog::ddl::rebuild_graph_physical_routes(
            &mut i.catalog,
            i.engine,
            "kg",
            |new, cat, pool, log, chain, txn| {
                let limits = bicdb_graph::Limits::default();
                let view = ReadView::new(seq).with_own(Some(txn.txn_id));
                let file = cat.file_mut();
                let mut port = bicdb_sql::graph_adjacency::NativeAdjacency::new(
                    pool,
                    ws,
                    records,
                    *new,
                    limits.clone(),
                    bicdb_graph::Deadline::for_limits(&limits),
                )
                .unwrap();
                port.insert(
                    file,
                    log,
                    chain,
                    txn,
                    view,
                    graph.edges().values().next().unwrap(),
                )
                .unwrap();
                let bytes = manifest_row(replacement_manifest(before[&0].clone(), *new));
                bicdb_access::TableAccess::new(pool, ws)
                    .update(
                        log,
                        chain,
                        txn,
                        file,
                        records.heap.block,
                        rid,
                        &bytes,
                        &bicdb_storage::heap::InsertPolicy::in_place(0),
                    )
                    .unwrap();
                wrote = true;
                let reason = if failure == "callback" {
                    "intentional populate failure".to_string()
                } else {
                    let limits = bicdb_graph::Limits {
                        max_expansions: if failure == "budget" { 1 } else { 100000 },
                        ..limits
                    };
                    let deadline = if failure == "deadline" {
                        bicdb_graph::Deadline::from_start(
                            std::time::Instant::now() - std::time::Duration::from_secs(61),
                            60000,
                        )
                    } else {
                        bicdb_graph::Deadline::for_limits(&limits)
                    };
                    let mut port = bicdb_sql::graph_adjacency::NativeAdjacency::new(
                        pool, ws, records, *new, limits, deadline,
                    )
                    .unwrap();
                    port.insert(
                        file,
                        log,
                        chain,
                        txn,
                        view,
                        graph.edges().values().nth(1).unwrap(),
                    )
                    .unwrap_err()
                    .to_string()
                };
                Err(bicdb_catalog::ddl::DdlError::BadIndexDef(reason))
            },
        )
        .unwrap_err()
        .to_string();
        assert!(wrote, "fault occurs after real edge/source/manifest writes");
        assert!(
            err.contains(if failure == "callback" {
                "intentional"
            } else {
                "budget"
            }),
            "{err}"
        );
        assert_eq!(routes(&mut i), old);
        assert_eq!(physical(&mut i), before);
        assert_eq!(metadata_counts(&mut i), counts);
        assert_eq!(
            rows(&mut session(&mut i), "SHOW FULLTEXT GRAPH INDEXES ON kg"),
            ft
        );
        assert_eq!(count(&mut session(&mut i)), "3");
    }
    session(&mut i)
        .execute("ALTER GRAPH kg REBUILD STORAGE")
        .unwrap();
    assert_eq!(count(&mut session(&mut i)), "3");
    i.shutdown().unwrap();
}

#[test]
fn sql_storage_rebuild_flushed_loser_recovers_old_generation() {
    let f = Fixture::new("rebuild-crash");
    let mut i = create_instance(&f.params(), None).unwrap();
    session(&mut i).execute(SEED).unwrap();
    let big = "文".repeat(70000);
    session(&mut i)
        .execute(&query(&format!(
            "MATCH ()-[r {{name:\"oldword\"}}]->() SET r.payload=\"{big}\""
        )))
        .unwrap();
    session(&mut i).execute("CREATE UNIQUE GRAPH INDEX names ON kg NODES (name); CREATE FULLTEXT GRAPH INDEX words ON kg RELATIONSHIPS (name) OPTIONS '{\"update\":\"manual\"}'").unwrap();
    let before = physical(&mut i);
    let old = routes(&mut i);
    let counts = metadata_counts(&mut i);
    let expected = edge_rows(&mut session(&mut i));
    let ft = rows(&mut session(&mut i), "SHOW FULLTEXT GRAPH INDEXES ON kg");
    i.shutdown().unwrap();
    drop(i);
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "sql_storage_rebuild_crash_child",
            "--ignored",
            "--nocapture",
        ])
        .env("BICDB_STORAGE_REBUILD_CRASH_ROOT", &f.0)
        .output()
        .unwrap();
    assert_eq!(
        child.status.code(),
        Some(99),
        "{}{}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
    let mut i = open_instance(&f.params()).unwrap();
    assert_eq!(routes(&mut i), old);
    assert_eq!(physical(&mut i), before);
    assert_eq!(metadata_counts(&mut i), counts);
    assert_eq!(edge_rows(&mut session(&mut i)), expected);
    assert_eq!(
        rows(&mut session(&mut i), "SHOW FULLTEXT GRAPH INDEXES ON kg"),
        ft
    );
    session(&mut i)
        .execute("ALTER GRAPH kg REBUILD STORAGE")
        .unwrap();
    assert_eq!(edge_rows(&mut session(&mut i)), expected);
    i.shutdown().unwrap();
    drop(i);
    let mut i = open_instance(&f.params()).unwrap();
    assert_eq!(edge_rows(&mut session(&mut i)), expected);
    i.shutdown().unwrap();
}

#[test]
#[ignore = "parent executes the real uncommitted physical route rebuild crash"]
fn sql_storage_rebuild_crash_child() {
    let path = PathBuf::from(
        std::env::var_os("BICDB_STORAGE_REBUILD_CRASH_ROOT").expect("parent fixture"),
    );
    let mut i = open_instance(&InstanceParams::for_init(&path, None, &[]).unwrap()).unwrap();
    let graph = graph_copy(&mut i);
    let header = physical(&mut i)[&0].clone();
    let ws = i.catalog.ws();
    let seq = bicdb_common::seq::CommitSeq::from_raw(i.engine.current_seq()).unwrap();
    let records =
        bicdb_sql::graph_adjacency::GraphRecords::resolve(&mut i.catalog, "kg", seq).unwrap();
    let rid = manifest_location(&mut i, records);
    bicdb_catalog::ddl::rebuild_graph_physical_routes(
        &mut i.catalog,
        i.engine,
        "kg",
        |new, cat, pool, log, chain, txn| {
            let limits = bicdb_graph::Limits::default();
            let view = ReadView::new(seq).with_own(Some(txn.txn_id));
            let file = cat.file_mut();
            let mut port = bicdb_sql::graph_adjacency::NativeAdjacency::new(
                pool,
                ws,
                records,
                *new,
                limits.clone(),
                bicdb_graph::Deadline::for_limits(&limits),
            )
            .unwrap();
            for edge in graph.edges().values() {
                port.insert(file, log, chain, txn, view, edge).unwrap();
            }
            let bytes = manifest_row(replacement_manifest(header, *new));
            bicdb_access::TableAccess::new(pool, ws)
                .update(
                    log,
                    chain,
                    txn,
                    file,
                    records.heap.block,
                    rid,
                    &bytes,
                    &bicdb_storage::heap::InsertPolicy::in_place(0),
                )
                .unwrap();
            log.flush(log.appended_lsn()).unwrap();
            pool.flush_workspace(ws).unwrap();
            std::process::exit(99)
        },
    )
    .unwrap();
    panic!("crash callback returned");
}

fn legacy_image(i: &mut Instance) -> (bicdb_graph::Graph, bicdb_graph::storage::StorageImage) {
    bicdb_graph::storage::StorageImage::decode(physical(i), &bicdb_graph::Limits::default())
        .unwrap()
}
fn migration_manifest(
    graph: &bicdb_graph::Graph,
    records: bicdb_sql::graph_adjacency::GraphRecords,
    routes: bicdb_catalog::ddl::GraphPhysicalRoutes,
    ws: [u8; 8],
    file: u32,
) -> Vec<u8> {
    let mut header = b"BICGRV3\0".to_vec();
    header.extend(ws);
    header.extend(file.to_be_bytes());
    for route in [
        records.heap,
        records.primary,
        routes.adjacency,
        routes.source,
        routes.locator,
        routes.incoming,
    ] {
        header.extend(route.obj.to_be_bytes());
        header.extend(route.block.to_be_bytes());
    }
    for count in [
        graph.allocator_high_water(),
        graph.nodes().len() as u64,
        graph.edges().len() as u64,
    ] {
        header.extend(count.to_be_bytes());
    }
    let mut sha = bicdb_common::sha256::Sha256::new();
    sha.update(&header);
    header.extend(sha.finalize());
    assert_eq!(header.len(), 124);
    header
}

#[test]
fn sql_upgrade_replaces_inactive_routes_with_aborted_native_candidates() {
    let f = Fixture::new("upgrade-inactive");
    let mut i = create_instance(&f.params(), None).unwrap();
    legacy_seed(&mut i);
    session(&mut i).execute("CREATE FULLTEXT GRAPH INDEX words ON kg RELATIONSHIPS (name) OPTIONS '{\"update\":\"manual\"}'").unwrap();
    let old = bicdb_catalog::ddl::create_graph_physical_routes(
        &mut i.catalog,
        i.engine,
        "kg",
        |_, _, _, _, _, _| Ok(()),
    )
    .unwrap();
    let (graph, image) = legacy_image(&mut i);
    let original = physical(&mut i);
    let expected = edge_rows(&mut session(&mut i));
    let ws = i.catalog.ws();
    let seq = bicdb_common::seq::CommitSeq::from_raw(i.engine.current_seq()).unwrap();
    let records =
        bicdb_sql::graph_adjacency::GraphRecords::resolve(&mut i.catalog, "kg", seq).unwrap();
    let patch = image
        .adjacency_migration_patch(&graph, &bicdb_graph::Limits::default())
        .unwrap();
    let mut transaction = i.engine.begin().unwrap();
    i.engine
        .with_write_context(&mut transaction, |pool, log, chain, txn| {
            let limits = bicdb_graph::Limits::default();
            let view = ReadView::new(seq).with_own(Some(txn.txn_id));
            let file = i.catalog.file_mut();
            let mut port = bicdb_sql::graph_adjacency::NativeAdjacency::new(
                pool,
                ws,
                records,
                old,
                limits.clone(),
                bicdb_graph::Deadline::for_limits(&limits),
            )
            .unwrap();
            port.apply_records(file, log, chain, txn, view, &patch)
                .unwrap();
            port.insert(
                file,
                log,
                chain,
                txn,
                view,
                graph.edges().values().next().unwrap(),
            )
            .unwrap();
        });
    i.engine.rollback(&mut transaction).unwrap();
    assert_eq!(physical(&mut i), original);
    assert!(
        !i.catalog
            .graph_index_range(old.locator.obj, None, None, 10)
            .unwrap()
            .is_empty(),
        "real aborted migration leaves redo-only candidates"
    );
    let ft = rows(&mut session(&mut i), "SHOW FULLTEXT GRAPH INDEXES ON kg");
    let metadata = metadata_counts(&mut i);
    let snapshot = i.engine.current_seq();
    assert_eq!(
        edge_rows(&mut session(&mut i)),
        expected,
        "read-only SQL stays on legacy authority"
    );
    session(&mut i)
        .execute("ALTER GRAPH kg UPGRADE STORAGE")
        .unwrap();
    assert_eq!(
        i.engine.current_seq(),
        snapshot + 1,
        "adoption replacement and migration publish one commit"
    );
    let fresh = routes(&mut i);
    assert_ne!(fresh.adjacency, old.adjacency);
    assert_ne!(fresh.source, old.source);
    assert_ne!(fresh.locator, old.locator);
    assert_ne!(fresh.incoming, old.incoming);
    assert_eq!(metadata_counts(&mut i), metadata);
    assert_eq!(edge_rows(&mut session(&mut i)), expected);
    assert_eq!(
        rows(&mut session(&mut i), "SHOW FULLTEXT GRAPH INDEXES ON kg"),
        ft
    );
    assert_eq!(
        edge_rows(&mut Session::new(
            i.pool,
            i.engine,
            &mut i.catalog,
            snapshot
        )),
        expected
    );
    let upgraded = i.engine.current_seq();
    session(&mut i)
        .execute("ALTER GRAPH kg UPGRADE STORAGE")
        .unwrap();
    assert_eq!(i.engine.current_seq(), upgraded);
    i.shutdown().unwrap();
    drop(i);
    let mut i = open_instance(&f.params()).unwrap();
    assert_eq!(edge_rows(&mut session(&mut i)), expected);
    i.shutdown().unwrap();
}

fn route_descriptor(
    i: &mut Instance,
    obj: u32,
    value: Option<Vec<u8>>,
) -> Vec<bicdb_catalog::row::DictValue> {
    let key = bicdb_catalog::open::comp_num(u64::from(obj));
    let (rid, mut values) = i
        .catalog
        .lookup("i_ind_pk", &[Some(&key)])
        .unwrap()
        .unwrap();
    if let Some(value) = value {
        values[6] = bicdb_catalog::row::DictValue::Bytes(value);
        let def = bicdb_catalog::dict::DICT_TABLES
            .iter()
            .find(|table| table.name == "ind$")
            .unwrap();
        let bytes = bicdb_catalog::row::encode(&values, def.columns).unwrap();
        let block = i.catalog.table_segment_block("ind$").unwrap();
        let ws = i.catalog.ws();
        let file = i.catalog.file_mut();
        let mut transaction = i.engine.begin().unwrap();
        i.engine
            .with_write_context(&mut transaction, |pool, log, chain, txn| {
                bicdb_access::TableAccess::new(pool, ws)
                    .update(
                        log,
                        chain,
                        txn,
                        file,
                        block,
                        rid,
                        &bytes,
                        &bicdb_storage::heap::InsertPolicy::in_place(0),
                    )
                    .unwrap();
            });
        i.engine.commit(&mut transaction).unwrap();
        i.catalog.advance_commit(
            bicdb_common::seq::CommitSeq::from_raw(i.engine.current_seq()).unwrap(),
        );
    }
    values
}
#[test]
fn sql_upgrade_rejects_forged_inactive_routes_before_any_ddl() {
    let f = Fixture::new("upgrade-forged-inactive");
    let mut i = create_instance(&f.params(), None).unwrap();
    legacy_seed(&mut i);
    let old = bicdb_catalog::ddl::create_graph_physical_routes(
        &mut i.catalog,
        i.engine,
        "kg",
        |_, _, _, _, _, _| Ok(()),
    )
    .unwrap();
    let original = route_descriptor(&mut i, old.locator.obj, None);
    route_descriptor(
        &mut i,
        old.locator.obj,
        Some(b"forged graph descriptor".to_vec()),
    );
    let stored = physical(&mut i);
    let metadata = metadata_counts(&mut i);
    let seq = i.engine.current_seq();
    let error = session(&mut i)
        .execute("ALTER GRAPH kg UPGRADE STORAGE")
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("invalid graph physical route descriptor"),
        "{error}"
    );
    assert_eq!(
        i.engine.current_seq(),
        seq,
        "invalid routes fail before even reserving a commit sequence"
    );
    assert_eq!(physical(&mut i), stored);
    assert_eq!(metadata_counts(&mut i), metadata);
    let bicdb_catalog::row::DictValue::Bytes(descriptor) = original[6].clone() else {
        panic!("descriptor");
    };
    route_descriptor(&mut i, old.locator.obj, Some(descriptor));
    session(&mut i)
        .execute("ALTER GRAPH kg UPGRADE STORAGE")
        .unwrap();
    assert_eq!(count(&mut session(&mut i)), "3");
    i.shutdown().unwrap();
}
fn pack_v1_records(i: &mut Instance) {
    let graph = graph_copy(i);
    let bytes = graph.to_bytes().unwrap();
    let mut sha = bicdb_common::sha256::Sha256::new();
    sha.update(&bytes);
    let mut packed = BTreeMap::from([(0, sha.finalize().to_vec())]);
    packed.extend(
        bytes
            .chunks(4096)
            .enumerate()
            .map(|(n, data)| (n as u64 + 1, data.to_vec())),
    );
    let seq = bicdb_common::seq::CommitSeq::from_raw(i.engine.current_seq()).unwrap();
    let records =
        bicdb_sql::graph_adjacency::GraphRecords::resolve(&mut i.catalog, "kg", seq).unwrap();
    let ws = i.catalog.ws();
    let shape = bicdb_exec::RowShape::new(vec![
        bicdb_exec::ColKind::Number,
        bicdb_exec::ColKind::Bytes,
    ]);
    let file = i.catalog.file_mut();
    let old = i.engine.with_read_context(|pool, chain| {
        let segment =
            bicdb_storage::segment::Segment::open_pooled(pool, file, records.heap.block, ws)
                .unwrap();
        let blocks = segment.data_blocks(segment.hwm());
        let fid = segment.file_id();
        drop(segment);
        let mut scan =
            bicdb_storage::scan::HeapScanner::new(pool, chain, ReadView::new(seq), fid, blocks);
        let mut ids = vec![];
        while let Some((rid, _)) = scan.next_row().unwrap() {
            ids.push(rid);
        }
        ids
    });
    let mut transaction = i.engine.begin().unwrap();
    i.engine
        .with_write_context(&mut transaction, |pool, log, chain, txn| {
            let mut table = bicdb_access::TableAccess::new(pool, ws);
            for rid in old {
                table
                    .delete(
                        log,
                        chain,
                        txn,
                        file,
                        rid,
                        &bicdb_storage::heap::InsertPolicy::in_place(0),
                    )
                    .unwrap();
            }
            for (key, data) in packed {
                let row = bicdb_exec::Row::new(vec![
                    bicdb_exec::Value::Number(
                        bicdb_types::Number::parse(&key.to_string()).unwrap(),
                    ),
                    bicdb_exec::Value::Bytes(data),
                ]);
                let bytes = bicdb_exec::encode_row(&row, &shape).unwrap();
                let key = bicdb_catalog::row::key_from_row(&bytes, &[0]).unwrap();
                let rid = table
                    .insert(
                        log,
                        chain,
                        txn,
                        file,
                        records.heap.block,
                        &bytes,
                        &bicdb_storage::heap::InsertPolicy::in_place(0),
                    )
                    .unwrap();
                let root = bicdb_access::index::insert_entry(
                    pool,
                    log,
                    file,
                    ws,
                    records.primary.block,
                    txn,
                    &key,
                    rid,
                )
                .unwrap();
                bicdb_access::index::write_tree_head_redo(
                    pool,
                    log,
                    file,
                    ws,
                    records.primary.block,
                    txn,
                    root,
                )
                .unwrap();
            }
        });
    i.engine.commit(&mut transaction).unwrap();
    assert_eq!(physical(i)[&0].len(), 32);
}

#[test]
fn sql_legacy_migration_flushed_phase_crashes_restore_source_and_allow_retry() {
    for version in [1, 2] {
        for inactive in [false, true] {
            for phase in ["nodes", "edge", "manifest"] {
                let f = Fixture::new(&format!("migration-phase-{version}-{inactive}-{phase}"));
                let mut i = create_instance(&f.params(), None).unwrap();
                legacy_seed(&mut i);
                let big = "文".repeat(70000);
                session(&mut i)
                    .execute(&query(&format!(
                        "MATCH ()-[r {{name:\"oldword\"}}]->() SET r.payload=\"{big}\""
                    )))
                    .unwrap();
                if version == 1 {
                    pack_v1_records(&mut i);
                }
                session(&mut i).execute("CREATE UNIQUE GRAPH INDEX names ON kg NODES (name); CREATE FULLTEXT GRAPH INDEX words ON kg RELATIONSHIPS (name) OPTIONS '{\"update\":\"manual\"}'").unwrap();
                if inactive {
                    bicdb_catalog::ddl::create_graph_physical_routes(
                        &mut i.catalog,
                        i.engine,
                        "kg",
                        |_, _, _, _, _, _| Ok(()),
                    )
                    .unwrap();
                }
                let stored = physical(&mut i);
                let expected = edge_rows(&mut session(&mut i));
                let metadata = metadata_counts(&mut i);
                let routes =
                    bicdb_catalog::ddl::graph_physical_routes(&mut i.catalog, "kg").unwrap();
                let ft = rows(&mut session(&mut i), "SHOW FULLTEXT GRAPH INDEXES ON kg");
                i.shutdown().unwrap();
                drop(i);
                let child = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "sql_legacy_migration_phase_crash_child",
                        "--ignored",
                        "--nocapture",
                    ])
                    .env("BICDB_LEGACY_MIGRATION_CRASH_ROOT", &f.0)
                    .env("BICDB_LEGACY_MIGRATION_CRASH_PHASE", phase)
                    .output()
                    .unwrap();
                assert_eq!(
                    child.status.code(),
                    Some(99),
                    "v{version}, inactive={inactive}, phase={phase}: {}{}",
                    String::from_utf8_lossy(&child.stdout),
                    String::from_utf8_lossy(&child.stderr)
                );
                let mut i = open_instance(&f.params()).unwrap();
                assert_eq!(physical(&mut i), stored);
                assert_eq!(
                    bicdb_catalog::ddl::graph_physical_routes(&mut i.catalog, "kg").unwrap(),
                    routes
                );
                assert_eq!(metadata_counts(&mut i), metadata);
                assert_eq!(edge_rows(&mut session(&mut i)), expected);
                assert_eq!(
                    rows(&mut session(&mut i), "SHOW FULLTEXT GRAPH INDEXES ON kg"),
                    ft
                );
                session(&mut i)
                    .execute("ALTER GRAPH kg UPGRADE STORAGE")
                    .unwrap();
                assert!(physical(&mut i)[&0].starts_with(b"BICGRV3\0"));
                assert_eq!(edge_rows(&mut session(&mut i)), expected);
                assert_eq!(rows(&mut session(&mut i),"SEARCH FULLTEXT GRAPH INDEX words ON kg FOR 'oldword' OPTIONS '{\"db_type\":\"d\",\"consistency\":\"eventual\"}'").len(),1);
                session(&mut i)
                    .execute(&query(
                        "MATCH ()-[r {name:\"oldword\"}]->() SET r.name=\"afterword\"",
                    ))
                    .unwrap();
                session(&mut i)
                    .execute("ALTER FULLTEXT GRAPH INDEX words ON kg SYNC")
                    .unwrap();
                assert_eq!(rows(&mut session(&mut i),"SEARCH FULLTEXT GRAPH INDEX words ON kg FOR 'afterword' OPTIONS '{\"db_type\":\"d\",\"consistency\":\"eventual\"}'").len(),1);
                i.shutdown().unwrap();
            }
        }
    }
}
#[test]
#[ignore = "parent executes v1/v2 migrations at three actual flushed stages, with/without inactive routes"]
fn sql_legacy_migration_phase_crash_child() {
    let path = PathBuf::from(
        std::env::var_os("BICDB_LEGACY_MIGRATION_CRASH_ROOT").expect("parent fixture"),
    );
    let phase = std::env::var("BICDB_LEGACY_MIGRATION_CRASH_PHASE").unwrap();
    let mut i = open_instance(&InstanceParams::for_init(&path, None, &[]).unwrap()).unwrap();
    let (graph, image) = legacy_image(&mut i);
    let limits = bicdb_graph::Limits::default();
    let patch = image.adjacency_migration_patch(&graph, &limits).unwrap();
    let ws = i.catalog.ws();
    let seq = bicdb_common::seq::CommitSeq::from_raw(i.engine.current_seq()).unwrap();
    let records =
        bicdb_sql::graph_adjacency::GraphRecords::resolve(&mut i.catalog, "kg", seq).unwrap();
    bicdb_catalog::ddl::prepare_graph_migration_routes(
        &mut i.catalog,
        i.engine,
        "kg",
        |routes, cat, pool, log, chain, txn| {
            let view = ReadView::new(seq).with_own(Some(txn.txn_id));
            let file = cat.file_mut();
            let mut port = bicdb_sql::graph_adjacency::NativeAdjacency::new(
                pool,
                ws,
                records,
                *routes,
                limits.clone(),
                bicdb_graph::Deadline::for_limits(&limits),
            )
            .unwrap();
            port.apply_records(file, log, chain, txn, view, &patch)
                .unwrap();
            if phase != "nodes" {
                for edge in graph.edges().values() {
                    port.insert(file, log, chain, txn, view, edge).unwrap();
                    if phase == "edge" {
                        break;
                    }
                }
            }
            if phase == "manifest" {
                let header =
                    migration_manifest(&graph, records, *routes, ws, file.file_id() as u32);
                port.apply_records(
                    file,
                    log,
                    chain,
                    txn,
                    view,
                    &bicdb_graph::storage::StoragePatch {
                        inserted: BTreeMap::from([(0, header)]),
                        ..Default::default()
                    },
                )
                .unwrap();
            }
            log.flush(log.appended_lsn()).unwrap();
            pool.flush_workspace(ws).unwrap();
            std::process::exit(99)
        },
    )
    .unwrap();
    panic!("crash callback returned");
}

#[test]
fn sql_statement_deltas_preserve_unique_checks_net_noops_fulltext_and_transaction_rollback() {
    for layout in [1, 2, 3] {
        let f = Fixture::new(&format!("write-delta-v{layout}"));
        let mut i = create_instance(&f.params(), None).unwrap();
        if layout == 3 {
            session(&mut i).execute(SEED).unwrap();
        } else {
            legacy_seed(&mut i);
            if layout == 1 {
                pack_v1_records(&mut i);
            }
        }
        session(&mut i).execute("CREATE UNIQUE GRAPH INDEX names ON kg NODES (name); CREATE FULLTEXT GRAPH INDEX words ON kg RELATIONSHIPS (name) OPTIONS '{\"update\":\"manual\"}'").unwrap();
        // Creating a full-text index may migrate v1 source records; upgrade format
        // coverage itself remains in the separate genuine legacy matrix.
        let before = rows(
            &mut session(&mut i),
            &query("MATCH (a)-[r]->(b) RETURN id(r) AS rid,a.name AS source,b.name AS target,r.name AS relationship ORDER BY rid"),
        );
        let ft = rows(&mut session(&mut i), "SHOW FULLTEXT GRAPH INDEXES ON kg");
        session(&mut i).execute(&query("MATCH (a:N {name:\"a\"}) SET a.name=\"temporary\",a:Temporary SET a.name=\"a\" REMOVE a:Temporary")).unwrap();
        session(&mut i).execute(&query("MATCH ()-[r {name:\"oldword\"}]->() SET r.name=\"temporary\" SET r.name=\"oldword\"")).unwrap();
        assert_eq!(
            rows(&mut session(&mut i), "SHOW FULLTEXT GRAPH INDEXES ON kg"),
            ft
        );
        {
            let mut s = session(&mut i);
            s.execute("BEGIN").unwrap();
            s.execute(&query("MATCH (a:N {name:\"a\"}) SET a.name=\"alpha\""))
                .unwrap();
            let error = s.execute(&query("MATCH (a:N {name:\"alpha\"}) SET a.name=\"b\" WITH a MATCH ()-[r {name:\"oldword\"}]->() SET r.name=\"badword\"")).unwrap_err().to_string();
            assert!(error.contains("unique"), "{error}");
            assert_eq!(
                rows(
                    &mut s,
                    &query("MATCH (a:N {name:\"alpha\"}) RETURN count(a)")
                )[0][0],
                "1"
            );
            assert_eq!(
                rows(
                    &mut s,
                    &query("MATCH ()-[r {name:\"badword\"}]->() RETURN count(r)")
                )[0][0],
                "0"
            );
            s.execute(&query("MATCH (a:N {name:\"alpha\"}) WITH a CALL (a) { SET a.name=\"committed\" RETURN a AS n } SET n:Extra")).unwrap();
            s.execute(&query(
                "MATCH ()-[r {name:\"oldword\"}]->() SET r.name=\"pendingword\"",
            ))
            .unwrap();
            s.execute("ROLLBACK").unwrap();
        }
        assert_eq!(
            rows(
                &mut session(&mut i),
                &query("MATCH (a)-[r]->(b) RETURN id(r) AS rid,a.name AS source,b.name AS target,r.name AS relationship ORDER BY rid")
            ),
            before
        );
        assert_eq!(
            rows(&mut session(&mut i), "SHOW FULLTEXT GRAPH INDEXES ON kg"),
            ft
        );
        session(&mut i)
            .execute(&query(
                "MATCH ()-[r {name:\"oldword\"}]->() SET r.name=\"finalword\"",
            ))
            .unwrap();
        session(&mut i)
            .execute("ALTER FULLTEXT GRAPH INDEX words ON kg SYNC")
            .unwrap();
        if layout == 3 {
            assert!(!physical(&mut i).keys().any(|key| key >> 60 == 2));
        }
        i.shutdown().unwrap();
        drop(i);
        let mut i = open_instance(&f.params()).unwrap();
        assert_eq!(count(&mut session(&mut i)), "3");
        assert_eq!(rows(&mut session(&mut i), "SEARCH FULLTEXT GRAPH INDEX words ON kg FOR 'finalword' OPTIONS '{\"db_type\":\"d\",\"consistency\":\"eventual\"}'").len(), 1);
        i.shutdown().unwrap();
    }
}

#[test]
fn sql_native_snapshot_boundary_edge_only_update_keeps_old_view_fulltext_and_restart() {
    let f = Fixture::new("native-boundary-planning");
    let mut graph = bicdb_graph::Graph::new();
    for name in ["a", "b"] {
        graph
            .add_node(
                std::collections::BTreeSet::from(["N".into()]),
                BTreeMap::from([("name".into(), serde_json::json!(name))]),
            )
            .unwrap();
    }
    let properties = BTreeMap::from([
        ("name".into(), serde_json::json!("oldword")),
        ("db_type".into(), serde_json::json!("d")),
    ]);
    let prototype = bicdb_graph::Edge {
        id: 3,
        source: 1,
        target: 2,
        label: "R".into(),
        properties: properties.clone(),
    };
    let overhead =
        bicdb_graph::adjacency_record::payload_size(&prototype, &bicdb_graph::Limits::default())
            .unwrap()
            - 1;
    graph
        .add_edge(1, 2, "L".repeat(4096 * 1023 - overhead), properties)
        .unwrap();
    let snapshot = bicdb_sql::session::GraphSnapshot {
        name: "kg".into(),
        data: graph.to_bytes().unwrap(),
    };
    Session::validate_graph_snapshot(&snapshot.data).unwrap();
    let mut oversized = serde_json::from_slice::<serde_json::Value>(&snapshot.data).unwrap();
    oversized["edges"][0]["label"] = serde_json::json!("L".repeat(4096 * 1023 - overhead + 1));
    assert!(
        Session::validate_graph_snapshot(&serde_json::to_vec(&oversized).unwrap())
            .unwrap_err()
            .to_string()
            .contains("chunk budget")
    );
    let mut i = create_instance(&f.params(), None).unwrap();
    session(&mut i).execute("CREATE GRAPH kg").unwrap();
    session(&mut i).restore_graph_snapshot(&snapshot).unwrap();
    let before = physical(&mut i);
    let old = i.engine.current_seq();
    // First SET starts at the imported allocator high water. Counts/HWM and
    // logical heap patch remain unchanged; generation must still record edge work.
    session(&mut i)
        .execute(&query("MATCH ()-[r]->() SET r.name=\"newword\""))
        .unwrap();
    let changed = physical(&mut i);
    assert_eq!(&changed[&0][68..92], &before[&0][68..92]);
    assert_eq!(
        u64::from_be_bytes(changed[&0][108..116].try_into().unwrap()),
        u64::from_be_bytes(before[&0][108..116].try_into().unwrap()) + 1
    );
    assert_eq!(
        rows(
            &mut session(&mut i),
            &query("MATCH ()-[r]->() RETURN r.name")
        )[0][0],
        "newword"
    );
    {
        let mut s = Session::new(i.pool, i.engine, &mut i.catalog, old);
        assert_eq!(
            rows(&mut s, &query("MATCH ()-[r]->() RETURN r.name"))[0][0],
            "oldword"
        );
    }
    session(&mut i).execute("CREATE UNIQUE GRAPH INDEX names ON kg RELATIONSHIPS (name); CREATE FULLTEXT GRAPH INDEX words ON kg RELATIONSHIPS (name) OPTIONS '{\"update\":\"manual\"}'").unwrap();
    session(&mut i)
        .execute(&query("MATCH ()-[r]->() SET r.name=\"endword\""))
        .unwrap();
    session(&mut i)
        .execute("ALTER FULLTEXT GRAPH INDEX words ON kg SYNC")
        .unwrap();
    let (_, exported) = session(&mut i).graph_initialization_snapshot(true).unwrap();
    assert_eq!(exported.len(), 1);
    assert_eq!(
        bicdb_graph::Graph::from_bytes(&exported[0].data, &bicdb_graph::Limits::default())
            .unwrap()
            .edges()[&3]
            .label,
        graph.edges()[&3].label
    );
    assert!(!physical(&mut i).keys().any(|key| key >> 60 == 2));
    i.shutdown().unwrap();
    drop(i);
    let mut i = open_instance(&f.params()).unwrap();
    assert_eq!(
        rows(
            &mut session(&mut i),
            &query("MATCH ()-[r]->() RETURN id(r),r.name")
        )[0],
        vec!["3", "endword"]
    );
    assert_eq!(rows(&mut session(&mut i),"SEARCH FULLTEXT GRAPH INDEX words ON kg FOR 'endword' OPTIONS '{\"db_type\":\"d\",\"consistency\":\"eventual\"}'").len(),1);
    i.shutdown().unwrap();
}

#[test]
fn sql_unique_persistent_candidates_cover_ghosts_swaps_scope_relationships_and_rollback() {
    for layout in [1, 2, 3] {
        let f = Fixture::new(&format!("unique-probes-v{layout}"));
        let mut i = create_instance(&f.params(), None).unwrap();
        if layout == 3 {
            session(&mut i).execute(SEED).unwrap();
        } else {
            legacy_seed(&mut i);
            if layout == 1 {
                pack_v1_records(&mut i);
            }
        }
        session(&mut i).execute("CREATE UNIQUE GRAPH INDEX names ON kg NODES LABEL \"N\" (name); CREATE UNIQUE GRAPH INDEX relkeys ON kg RELATIONSHIPS TYPE \"LINK\" (db_type,name); CREATE FULLTEXT GRAPH INDEX words ON kg RELATIONSHIPS (name) OPTIONS '{\"update\":\"manual\"}'").unwrap();
        let original = rows(
            &mut session(&mut i),
            &query("MATCH (n:N) RETURN id(n) AS nid,n.name AS name ORDER BY nid"),
        );
        let old = i.engine.current_seq();
        let check_old = |i: &mut Instance, stage: &str| {
            let mut historical = Session::new(i.pool, i.engine, &mut i.catalog, old);
            assert_eq!(
                rows(
                    &mut historical,
                    &query("MATCH (n:N) RETURN id(n) AS nid,n.name AS name ORDER BY nid")
                ),
                original,
                "layout={layout}, stage={stage}, snapshot={old}"
            );
        };
        check_old(&mut i, "before writes");
        {
            let mut s = session(&mut i);
            s.execute("BEGIN").unwrap();
            s.execute(&query("MATCH (a:N {name:\"a\"}) SET a.name=\"temporary\""))
                .unwrap();
            let ft = rows(&mut s, "SHOW FULLTEXT GRAPH INDEXES ON kg");
            let error = s.execute(&query("MATCH ()-[r {name:\"oldword\"}]->() SET r.name=\"badword\" CREATE (:N {name:\"b\"})")).unwrap_err().to_string();
            assert!(error.contains("unique"), "{error}");
            assert_eq!(rows(&mut s, "SHOW FULLTEXT GRAPH INDEXES ON kg"), ft);
            assert_eq!(
                rows(
                    &mut s,
                    &query("MATCH (n:N {name:\"temporary\"}) RETURN count(n)")
                )[0][0],
                "1"
            );
            assert_eq!(
                rows(
                    &mut s,
                    &query("MATCH ()-[r {name:\"badword\"}]->() RETURN count(r)")
                )[0][0],
                "0"
            );
            // The old key still has a persisted candidate, but its own visible
            // source now holds temporary. Reusing a released key must succeed.
            s.execute(&query("CREATE (:N {name:\"a\"})")).unwrap();
            s.execute("ROLLBACK").unwrap();
        }
        assert_eq!(
            rows(
                &mut session(&mut i),
                &query("MATCH (n:N) RETURN id(n) AS nid,n.name AS name ORDER BY nid")
            ),
            original
        );
        check_old(&mut i, "after rollback");
        session(&mut i)
            .execute(&query(
                "MATCH (a:N {name:\"a\"}),(b:N {name:\"b\"}) SET a.name=\"b\",b.name=\"a\"",
            ))
            .unwrap();
        check_old(&mut i, "after node swap");
        session(&mut i).execute(&query("MATCH ()-[a {name:\"oldword\"}]->(),()-[b {name:\"parallel\"}]->() SET a.name=\"parallel\",b.name=\"oldword\"")).unwrap();
        check_old(&mut i, "after relationship swap");
        let error = session(&mut i)
            .execute(&query(
                "MATCH ()-[r {name:\"loop\"}]->() SET r.name=\"oldword\"",
            ))
            .unwrap_err()
            .to_string();
        assert!(error.contains("unique"), "{error}");
        session(&mut i)
            .execute(&query(
                "MATCH (n:N {name:\"a\"}) REMOVE n:N CREATE (:N {name:\"a\"})",
            ))
            .unwrap();
        {
            let mut historical = Session::new(i.pool, i.engine, &mut i.catalog, old);
            assert_eq!(
                rows(
                    &mut historical,
                    &query("MATCH (n:N) RETURN id(n) AS nid,n.name AS name ORDER BY nid")
                ),
                original,
                "layout={layout}, snapshot={old}"
            );
        }
        session(&mut i)
            .execute("ALTER FULLTEXT GRAPH INDEX words ON kg SYNC")
            .unwrap();
        i.shutdown().unwrap();
        drop(i);
        let mut i = open_instance(&f.params()).unwrap();
        assert_eq!(
            rows(
                &mut session(&mut i),
                &query("MATCH (n:N) RETURN n.name AS name ORDER BY name")
            ),
            vec![vec!["a".to_string()], vec!["b".to_string()]]
        );
        assert_eq!(rows(&mut session(&mut i), "SEARCH FULLTEXT GRAPH INDEX words ON kg FOR 'oldword' OPTIONS '{\"db_type\":\"d\",\"consistency\":\"eventual\"}'").len(), 1);
        assert!(session(&mut i)
            .execute(&query("CREATE (:N {name:\"b\"})"))
            .unwrap_err()
            .to_string()
            .contains("unique"));
        i.shutdown().unwrap();
    }
}

#[test]
fn sql_unique_proof_bounds_ghost_reads_shares_index_work_and_rebuild_restores_capacity() {
    let f = Fixture::new("unique-proof-budget");
    let mut i = create_instance(&f.params(), None).unwrap();
    session(&mut i)
        .execute("CREATE GRAPH kg; CREATE UNIQUE GRAPH INDEX keys ON kg NODES LABEL \"N\" (key)")
        .unwrap();
    // Each physical historical membership has a different identity, while the
    // authoritative graph remains empty. A whole-graph scan would miss this cost.
    // Distinct committed statements generate actual physical candidates rather
    // than a net-empty CREATE/DELETE journal.
    for _ in 0..24 {
        session(&mut i)
            .execute(&query("CREATE (:N {key:7})"))
            .unwrap();
        session(&mut i)
            .execute(&query("MATCH (n:N) DELETE n"))
            .unwrap();
    }
    {
        let mut s = session(&mut i);
        s.execute("BEGIN").unwrap();
        s.execute(&query("CREATE (:N {key:8})")).unwrap();
        s.set_graph_limits(bicdb_graph::Limits {
            max_expansions: 12,
            ..Default::default()
        })
        .unwrap();
        let error = s
            .execute(&query("CREATE (:N {key:7})"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("unique graph index proof work budget"),
            "{error}"
        );
        s.set_graph_limits(Default::default()).unwrap();
        assert_eq!(
            rows(&mut s, &query("MATCH (n:N {key:8}) RETURN count(n)"))[0][0],
            "1"
        );
        assert_eq!(
            rows(&mut s, &query("MATCH (n:N {key:7}) RETURN count(n)"))[0][0],
            "0"
        );
        s.execute("COMMIT").unwrap();
    }
    session(&mut i)
        .execute("ALTER GRAPH INDEX keys ON kg REBUILD")
        .unwrap();
    {
        let mut s = session(&mut i);
        s.set_graph_limits(bicdb_graph::Limits {
            // The successful write also verifies and updates the persisted
            // corpus proof path; keep the separate 12-unit unique-index
            // exhaustion assertion above unchanged.
            max_expansions: 128,
            ..Default::default()
        })
        .unwrap();
        s.execute(&query("CREATE (:N {key:7})")).unwrap();
    }
    // Empty seeks count and all unique indexes share the same remaining work.
    for n in 0..15 {
        session(&mut i)
            .execute(&format!(
                "CREATE UNIQUE GRAPH INDEX extra{n} ON kg NODES LABEL \"N\" (key)"
            ))
            .unwrap();
    }
    {
        let mut s = session(&mut i);
        s.set_graph_limits(bicdb_graph::Limits {
            max_expansions: 12,
            ..Default::default()
        })
        .unwrap();
        let error = s
            .execute(&query("CREATE (:N {key:9})"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("unique graph index proof work budget"),
            "{error}"
        );
        s.set_graph_limits(Default::default()).unwrap();
        assert_eq!(
            rows(&mut s, &query("MATCH (n:N {key:9}) RETURN count(n)"))[0][0],
            "0"
        );
    }
    i.shutdown().unwrap();
}

fn rewrite_test_manifest(i: &mut Instance, mut header: Vec<u8>) {
    let checksum = header.len() - 32;
    let mut sha = bicdb_common::sha256::Sha256::new();
    sha.update(&header[..checksum]);
    header[checksum..].copy_from_slice(&sha.finalize());
    let seq = bicdb_common::seq::CommitSeq::from_raw(i.engine.current_seq()).unwrap();
    let records =
        bicdb_sql::graph_adjacency::GraphRecords::resolve(&mut i.catalog, "kg", seq).unwrap();
    let rid = manifest_location(i, records);
    let ws = i.catalog.ws();
    let file = i.catalog.file_mut();
    let bytes = manifest_row(header);
    let mut t = i.engine.begin().unwrap();
    i.engine
        .with_write_context(&mut t, |pool, log, chain, txn| {
            bicdb_access::TableAccess::new(pool, ws)
                .update(
                    log,
                    chain,
                    txn,
                    file,
                    records.heap.block,
                    rid,
                    &bytes,
                    &bicdb_storage::heap::InsertPolicy::in_place(0),
                )
                .unwrap();
        });
    i.engine.commit(&mut t).unwrap();
}
fn corpus_fields(header: &[u8]) -> [u64; 3] {
    assert!([148, 180].contains(&header.len()), "{}", header.len());
    [92, 100, 108].map(|offset| u64::from_be_bytes(header[offset..offset + 8].try_into().unwrap()))
}
fn stored_logical_bytes(i: &mut Instance, header: &[u8]) -> u64 {
    // Template export intentionally advances to the catalog sequence watermark.
    // Corpus statistics describe the stored snapshot's allocator instead.
    let mut json: serde_json::Value =
        serde_json::from_slice(&graph_copy(i).to_bytes().unwrap()).unwrap();
    json["next_id"] = serde_json::json!(u64::from_be_bytes(header[68..76].try_into().unwrap()));
    serde_json::to_vec(&json).unwrap().len() as u64
}
#[test]
fn sql_native_corpus_stats_legacy_read_write_rollback_rebuild_and_restart() {
    let f = Fixture::new("corpus-stats");
    let mut i = create_instance(&f.params(), None).unwrap();
    session(&mut i).execute(SEED).unwrap();
    let measured = physical(&mut i)[&0].clone();
    assert_eq!(
        corpus_fields(&measured)[0],
        stored_logical_bytes(&mut i, &measured)
    );
    let mut old = measured[..92].to_vec();
    old.extend([0; 32]);
    rewrite_test_manifest(&mut i, old);
    let old = physical(&mut i)[&0].clone();
    assert_eq!(old.len(), 124);
    assert_eq!(count(&mut session(&mut i)), "3");
    assert_eq!(physical(&mut i)[&0], old, "read must not upgrade metadata");
    {
        let mut s = session(&mut i);
        s.execute("BEGIN").unwrap();
        s.execute(&query("MATCH ()-[r]->() SET r.name=\"rolledback\""))
            .unwrap();
        s.execute("ROLLBACK").unwrap();
    }
    assert_eq!(physical(&mut i)[&0], old);
    let old_seq = i.engine.current_seq();
    session(&mut i)
        .execute(&query("MATCH ()-[r]->() SET r.name=\"measured\""))
        .unwrap();
    let first = physical(&mut i)[&0].clone();
    assert_eq!(corpus_fields(&first)[2], 1);
    {
        let mut s = Session::new(i.pool, i.engine, &mut i.catalog, old_seq);
        assert_eq!(count(&mut s), "3");
        assert_eq!(
            rows(
                &mut s,
                &query("MATCH ()-[r]->() RETURN r.name ORDER BY r.name")
            )[0][0],
            "loop"
        );
    }
    assert_eq!(
        corpus_fields(&first)[0],
        stored_logical_bytes(&mut i, &first)
    );
    session(&mut i)
        .execute("ALTER GRAPH kg REBUILD STORAGE")
        .unwrap();
    let rebuilt = physical(&mut i)[&0].clone();
    assert_eq!(&corpus_fields(&rebuilt)[..2], &corpus_fields(&first)[..2]);
    assert_eq!(corpus_fields(&rebuilt)[2], 2);
    i.shutdown().unwrap();
    drop(i);
    let mut i = open_instance(&f.params()).unwrap();
    assert_eq!(physical(&mut i)[&0], rebuilt);
    assert_eq!(count(&mut session(&mut i)), "3");
    session(&mut i)
        .execute(&query("MATCH ()-[r]->() SET r.name=\"changed\""))
        .unwrap();
    let committed = physical(&mut i)[&0].clone();
    assert_eq!(corpus_fields(&committed)[2], 3);
    {
        let mut s = session(&mut i);
        s.execute("BEGIN").unwrap();
        s.execute(&query("MATCH ()-[r]->() SET r.name=\"discarded\""))
            .unwrap();
        s.execute("ROLLBACK").unwrap();
    }
    assert_eq!(physical(&mut i)[&0], committed);
    i.shutdown().unwrap();
}
#[test]
fn sql_native_corpus_mismatch_and_generation_exhaustion_are_atomic_errors() {
    let f = Fixture::new("corpus-corrupt");
    let mut i = create_instance(&f.params(), None).unwrap();
    session(&mut i).execute(SEED).unwrap();
    let good = physical(&mut i)[&0].clone();
    for offset in [92, 100] {
        let mut forged = good.clone();
        let n = u64::from_be_bytes(forged[offset..offset + 8].try_into().unwrap());
        forged[offset..offset + 8].copy_from_slice(&(n + 1).to_be_bytes());
        rewrite_test_manifest(&mut i, forged);
        let before = physical(&mut i);
        let error = session(&mut i)
            .execute(&query("MATCH (n) SET n.name=\"bad\""))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("statistics differ")
                || error.contains("corpus proof differs from manifest"),
            "{error}"
        );
        assert_eq!(physical(&mut i), before);
        rewrite_test_manifest(&mut i, good.clone());
    }
    let mut exhausted = good.clone();
    exhausted[108..116].copy_from_slice(&u64::MAX.to_be_bytes());
    let proof_key = bicdb_graph::corpus_proof::RecordKey::Root;
    let (proof_base, proof_end) = proof_key.range().unwrap();
    let current = physical(&mut i);
    let proof_rows: BTreeMap<_, _> = current
        .range(proof_base..=proof_end)
        .map(|(&ordinal, data)| (ordinal, data.clone()))
        .collect();
    let mut proof = proof_key
        .from_native_rows(&proof_rows, &bicdb_graph::Limits::default())
        .unwrap()
        .unwrap();
    proof[60..68].copy_from_slice(&u64::MAX.to_be_bytes());
    let checksum = proof.len() - 32;
    let mut sha = bicdb_common::sha256::Sha256::new();
    sha.update(&proof[..checksum]);
    proof[checksum..].copy_from_slice(&sha.finalize());
    for (ordinal, data) in proof_key
        .native_rows(&proof, &bicdb_graph::Limits::default())
        .unwrap()
    {
        rewrite_test_ordinal(&mut i, ordinal, data);
    }
    rewrite_test_manifest(&mut i, exhausted);
    let before = physical(&mut i);
    let error = session(&mut i)
        .execute(&query("MATCH ()-[r]->() SET r.name=\"bad\""))
        .unwrap_err()
        .to_string();
    assert!(error.contains("generation exhausted"), "{error}");
    assert_eq!(physical(&mut i), before);
    assert_eq!(count(&mut session(&mut i)), "3");
    assert!(session(&mut i)
        .execute("ALTER GRAPH kg REBUILD STORAGE")
        .unwrap_err()
        .to_string()
        .contains("generation exhausted"));
    assert_eq!(physical(&mut i), before);
    i.shutdown().unwrap();
}

#[test]
fn sql_validated_epoch_point_writes_fetch_unique_sources_and_enforce_global_bytes() {
    let f = Fixture::new("partial-point");
    let mut i = create_instance(&f.params(), None).unwrap();
    session(&mut i).execute("CREATE GRAPH kg").unwrap();
    let mut graph = bicdb_graph::Graph::new();
    for n in 0..2000 {
        graph
            .add_node(
                std::collections::BTreeSet::from(["N".into()]),
                BTreeMap::from([
                    ("name".into(), serde_json::json!(format!("n{n}"))),
                    ("body".into(), serde_json::json!("x".repeat(100))),
                ]),
            )
            .unwrap();
    }
    session(&mut i)
        .restore_graph_snapshot(&bicdb_sql::session::GraphSnapshot {
            name: "kg".into(),
            data: graph.to_bytes().unwrap(),
        })
        .unwrap();
    session(&mut i)
        .execute("CREATE UNIQUE GRAPH INDEX by_name ON kg NODES LABEL \"N\" (name)")
        .unwrap();
    let bounded = |text: &str| format!("{} BUDGETS '{{\"max_expansions\":128}}'", query(text));
    // The persisted source proof makes the first cold point write local. It no
    // longer depends on a process-local whole-graph validation receipt.
    session(&mut i)
        .execute(&bounded("MATCH (n:N {name:'n1'}) SET n.body='cold'"))
        .unwrap();
    session(&mut i)
        .execute(&bounded("MATCH (n:N {name:'n1'}) SET n.body='changed'"))
        .unwrap();
    let before = physical(&mut i);
    let error = session(&mut i)
        .execute(&bounded("MATCH (n:N {name:'n1'}) SET n.name='n2'"))
        .unwrap_err()
        .to_string();
    assert!(error.contains("unique graph property index"), "{error}");
    assert_eq!(physical(&mut i), before);
    // Unrelated commits do not invalidate the graph-scoped persistent proof.
    session(&mut i)
        .execute("CREATE TABLE unrelated (v NUMBER)")
        .unwrap();
    session(&mut i)
        .execute(&bounded("MATCH (n:N {name:'n1'}) SET n.body='unvalidated'"))
        .unwrap();
    let before = physical(&mut i);
    let error = session(&mut i)
        .execute(&format!(
            "{} BUDGETS '{{\"max_text_bytes\":1024}}'",
            query("MATCH (n:N {name:'n1'}) SET n.body='tiny'")
        ))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("global graph storage")
            || error.contains("native adjacency work/byte budget")
            || error.contains("corpus proof work/byte budget"),
        "{error}"
    );
    assert_eq!(physical(&mut i), before);
    assert_eq!(
        rows(
            &mut session(&mut i),
            &query("MATCH (n:N {name:'n1'}) RETURN n.body")
        )[0][0],
        "unvalidated"
    );
    let fields = corpus_fields(&before[&0]);
    assert_eq!(
        u64::from_be_bytes(before[&0][76..84].try_into().unwrap()),
        2000
    );
    assert_eq!(fields[0], stored_logical_bytes(&mut i, &before[&0]));
    i.shutdown().unwrap();
    drop(i);
    let mut i = open_instance(&f.params()).unwrap();
    // Restart is deliberately cold; persisted proof remains sufficient.
    session(&mut i)
        .execute(&bounded("MATCH (n:N {name:'n1'}) SET n.body='restarted'"))
        .unwrap();
    assert_eq!(
        rows(
            &mut session(&mut i),
            &query("MATCH (n:N {name:'n1'}) RETURN n.body")
        )[0][0],
        "restarted"
    );
    i.shutdown().unwrap();
}

#[test]
fn sql_persisted_corpus_proof_rejects_resigned_root_and_keeps_graph_unchanged() {
    let f = Fixture::new("partial-proof-corrupt");
    let mut i = create_instance(&f.params(), None).unwrap();
    session(&mut i).execute(SEED).unwrap();
    let before = physical(&mut i);
    assert_eq!(before[&0].len(), 180);
    let key = bicdb_graph::corpus_proof::RecordKey::Root;
    let (base, end_ordinal) = key.range().unwrap();
    let physical_root: BTreeMap<_, _> = before
        .range(base..=end_ordinal)
        .map(|(&ordinal, data)| (ordinal, data.clone()))
        .collect();
    let mut root = key
        .from_native_rows(&physical_root, &bicdb_graph::Limits::default())
        .unwrap()
        .unwrap();
    assert_eq!(root.len(), 164);
    root[67] ^= 1;
    let end = root.len() - 32;
    let mut sha = bicdb_common::sha256::Sha256::new();
    sha.update(&root[..end]);
    root[end..].copy_from_slice(&sha.finalize());
    for (ordinal, data) in key
        .native_rows(&root, &bicdb_graph::Limits::default())
        .unwrap()
    {
        rewrite_test_ordinal(&mut i, ordinal, data);
    }
    let corrupted = physical(&mut i);
    let error = session(&mut i)
        .execute(&query("MATCH (n:N {name:'a'}) SET n.name='must-not-write'"))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("proof") && (error.contains("generation") || error.contains("manifest")),
        "{error}"
    );
    assert_eq!(physical(&mut i), corrupted);
    i.shutdown().unwrap();
}
#[test]
fn sql_partial_overlay_created_edges_detach_and_fulltext_keep_complete_results() {
    let f = Fixture::new("partial-overlay");
    let mut i = create_instance(&f.params(), None).unwrap();
    session(&mut i).execute(SEED).unwrap();
    session(&mut i).execute("CREATE FULLTEXT GRAPH INDEX names ON kg NODES (name) OPTIONS '{\"update\":\"manual\"}'").unwrap();
    session(&mut i).graph_initialization_snapshot(true).unwrap();
    let r=rows(&mut session(&mut i),&query("MATCH (a:N {name:'a'}),(b:N {name:'b'}) CREATE (a)-[:NEW]->(b) WITH a MATCH (a)-[r:NEW]->() RETURN count(r)"));
    assert_eq!(r[0][0], "1");
    let r=rows(&mut session(&mut i),&query("MATCH (a:N {name:'a'}) SET a.name='changedword' WITH a CALL db.index.fulltext.queryNodes('names','changedword',{db_type:'d'}) YIELD node RETURN node.name"));
    assert_eq!(r[0][0], "changedword");
    let before = physical(&mut i);
    assert!(session(&mut i)
        .execute(&query("MATCH (a:N {name:'changedword'}) DELETE a"))
        .unwrap_err()
        .to_string()
        .contains("still has relationships"));
    assert_eq!(physical(&mut i), before);
    session(&mut i).graph_initialization_snapshot(true).unwrap();
    let r=rows(&mut session(&mut i),&query("MATCH (a:N {name:'changedword'}) DETACH DELETE a WITH 1 AS x MATCH (n:N) RETURN count(n)"));
    assert_eq!(r[0][0], "1");
    assert_eq!(count(&mut session(&mut i)), "0");
    session(&mut i)
        .execute("ALTER FULLTEXT GRAPH INDEX names ON kg SYNC")
        .unwrap();
    assert!(rows(&mut session(&mut i),"SEARCH FULLTEXT GRAPH INDEX names ON kg FOR 'changedword' OPTIONS '{\"db_type\":\"d\",\"consistency\":\"strict\"}'").is_empty());
    i.shutdown().unwrap();
}
#[test]
fn sql_stale_native_authority_is_rejected_before_overwriting_a_committed_edge_edit() {
    let f = Fixture::new("partial-stale");
    let mut i = create_instance(&f.params(), None).unwrap();
    session(&mut i).execute(SEED).unwrap();
    let old = i.engine.current_seq();
    session(&mut i)
        .execute(&query("MATCH ()-[r]->() SET r.name='newversion'"))
        .unwrap();
    let before = physical(&mut i);
    let error = Session::new(i.pool, i.engine, &mut i.catalog, old)
        .execute(&query("MATCH ()-[r]->() SET r.name='staleversion'"))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("source changed") || error.contains("proof root changed"),
        "{error}"
    );
    assert_eq!(physical(&mut i), before);
    assert_eq!(
        rows(
            &mut session(&mut i),
            &query("MATCH ()-[r]->() RETURN r.name")
        )[0][0],
        "newversion"
    );
    i.shutdown().unwrap();
}
#[test]
fn sql_partial_owner_receipts_failed_statements_and_rollback_preserve_prior_work() {
    let f = Fixture::new("partial-owner");
    let mut i = create_instance(&f.params(), None).unwrap();
    session(&mut i).execute(SEED).unwrap();
    session(&mut i)
        .execute("CREATE UNIQUE GRAPH INDEX by_name ON kg NODES LABEL \"N\" (name)")
        .unwrap();
    let original = physical(&mut i);
    {
        let mut s = session(&mut i);
        s.execute("BEGIN").unwrap();
        s.execute(&query("MATCH (a:N {name:'a'}) SET a.body='prior'"))
            .unwrap();
        let error = s
            .execute(&query("MATCH (a:N {name:'a'}) SET a.name='b'"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("unique graph property"), "{error}");
        assert_eq!(
            rows(&mut s, &query("MATCH (a:N {name:'a'}) RETURN a.body"))[0][0],
            "prior"
        );
        s.execute(&query("MATCH (a:N {name:'a'}) SET a.body='after'"))
            .unwrap();
        s.execute("ROLLBACK").unwrap();
    }
    assert_eq!(physical(&mut i), original);
    session(&mut i)
        .execute(&query("MATCH (a:N {name:'a'}) SET a.body='committed'"))
        .unwrap();
    assert_eq!(
        rows(
            &mut session(&mut i),
            &query("MATCH (a:N {name:'a'}) RETURN a.body")
        )[0][0],
        "committed"
    );
    i.shutdown().unwrap();
}
