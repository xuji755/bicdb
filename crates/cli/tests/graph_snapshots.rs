use bicdb_cli::{
    boot::{create_instance, open_instance, Instance},
    config::InstanceParams,
    templates,
};
use bicdb_sql::session::{format_value, GraphSnapshot, QueryResult, Session};
use std::path::{Path, PathBuf};
struct Fixture(PathBuf);
impl Fixture {
    fn new(tag: &str) -> Self {
        let p =
            std::env::temp_dir().join(format!("bicdb-graph-snapshot-{}-{tag}", std::process::id()));
        assert!(!p.exists());
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn create(&self, name: &str) -> Instance {
        create_instance(
            &InstanceParams::for_init(&self.0.join(name), None, &[]).unwrap(),
            None,
        )
        .unwrap()
    }
    fn open(&self, name: &str) -> Instance {
        open_instance(&InstanceParams::for_init(&self.0.join(name), None, &[]).unwrap()).unwrap()
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
    let r = s.execute(sql).unwrap();
    let Some(QueryResult::Rows { rows, .. }) = r.last() else {
        panic!("{r:?}")
    };
    rows.iter()
        .map(|row| row.iter().map(format_value).collect())
        .collect()
}
fn digest(s: &str) -> String {
    bicdb_common::sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn write_bundle(home: &Path, script: &str, name: &str, data: &str, bytes: Option<u64>) {
    let root = home.join("templates/test");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("schema.sql"), script).unwrap();
    std::fs::write(root.join("graph-0.json"), data).unwrap();
    std::fs::write(root.join("manifest"),serde_json::json!({"format":"BICDB_GRAPH_TEMPLATE_V2","name":"test","schema_sha256":digest(script),"graphs":[{"name":name,"bytes":bytes.unwrap_or(data.len() as u64),"sha256":digest(data)}]}).to_string()).unwrap();
}
#[test]
fn graph_snapshot_preserves_entities_and_rebuilds_derived_state_before_pause() {
    let f = Fixture::new("roundtrip");
    let mut source = f.create("source");
    let expected;
    let schema="CREATE TABLE empty_rows(id NUMBER); CREATE GRAPH \"KG;知识\"; CREATE GRAPH empty_graph; CREATE UNIQUE GRAPH INDEX identities ON \"KG;知识\" NODES LABEL \"Entity\" (db_type,key); CREATE FULLTEXT GRAPH INDEX words ON \"KG;知识\" NODES (name,config.\"Question\") OPTIONS '{\"update\":\"batch\"}'; ALTER FULLTEXT GRAPH INDEX words ON \"KG;知识\" PAUSE; CREATE FULLTEXT GRAPH INDEX edgewords ON \"KG;知识\" RELATIONSHIPS TYPE \"LINK\" (name) OPTIONS '{\"update\":\"manual\"}';";
    let seed="CYPHER \"KG;知识\" 'CREATE (a:Entity:Fault {db_type:\"d1\",key:1,name:\"hubword\",config:{Question:\"数据库IO检索\",args:[1,true,null,{x:123.45678901234567890123456789}]}}),(b:Entity {db_type:\"d1\",key:2,name:\"leafword\"}),(a)-[:LINK {db_type:\"d1\",name:\"edgeword\",weight:2.5}]->(b),(a)-[:LINK {db_type:\"d1\",name:\"edgeword\"}]->(b),(a)-[:LINK {db_type:\"d1\",name:\"selfword\"}]->(a)'";
    let query =
        "CYPHER \"KG;知识\" 'MATCH (n) RETURN id(n),labels(n),properties(n) ORDER BY id(n)'";
    {
        let mut s = session(&mut source);
        s.execute(schema).unwrap();
        s.execute(seed).unwrap();
        assert!(s
            .execute("CYPHER \"KG;知识\" 'CREATE (:Unused {x:1/0})'")
            .is_err());
        expected = rows(&mut s, query);
    }
    source.shutdown().unwrap();
    drop(source);
    assert!(templates::publish(&f.0, &f.0.join("source"), "empty_only")
        .unwrap_err()
        .contains("非空"));
    templates::publish_graph_data(&f.0, &f.0.join("source"), "data").unwrap();
    let template = templates::load(&f.0, "data").unwrap();
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(f.0.join("templates/data/manifest")).unwrap())
            .unwrap();
    let ordinal = manifest["graphs"]
        .as_array()
        .unwrap()
        .iter()
        .position(|g| g["name"] == "KG;知识")
        .unwrap();
    let data: serde_json::Value = serde_json::from_slice(
        &std::fs::read(f.0.join(format!("templates/data/graph-{ordinal}.json"))).unwrap(),
    )
    .unwrap();
    assert_eq!(
        data["next_id"].as_u64().unwrap(),
        1200001,
        "export the durable sequence cursor, including a failed query's unspent reservation"
    );
    assert_eq!(template.graph_count(), 2);
    assert_eq!(template.format(), "BICDB_GRAPH_TEMPLATE_V2");
    for name in ["left", "right"] {
        let mut target = f.create(name);
        {
            let mut s = session(&mut target);
            template.apply(&mut s).unwrap();
            assert_eq!(rows(&mut s, query), expected);
            assert_eq!(
                rows(
                    &mut s,
                    "CYPHER \"KG;知识\" 'MATCH ()-[r]->() RETURN count(r)'"
                ),
                vec![vec!["3".to_owned()]]
            );
            assert_eq!(rows(&mut s,"SEARCH FULLTEXT GRAPH INDEX words ON \"KG;知识\" FOR 'hubword' OPTIONS '{\"db_type\":\"d1\",\"consistency\":\"eventual\"}'").len(),1,"paused index starts with a complete target corpus");
            assert_eq!(rows(&mut s,"SEARCH FULLTEXT GRAPH INDEX edgewords ON \"KG;知识\" FOR 'edgeword' OPTIONS '{\"db_type\":\"d1\",\"consistency\":\"eventual\"}'").len(),2);
            assert!(s
                .execute("CYPHER \"KG;知识\" 'CREATE (:Entity {db_type:\"d1\",key:1})'")
                .is_err());
            let added = rows(
                &mut s,
                "CYPHER \"KG;知识\" 'CREATE (n:Entity {db_type:\"d1\",key:3}) RETURN id(n)'",
            );
            assert!(added[0][0].parse::<u64>().unwrap() > expected[1][0].parse::<u64>().unwrap());
        }
        target.shutdown().unwrap();
        drop(target);
        let mut target = f.open(name);
        {
            let mut s = session(&mut target);
            assert_eq!(
                rows(
                    &mut s,
                    "CYPHER \"KG;知识\" 'MATCH (n:Entity) RETURN count(n)'"
                ),
                vec![vec!["3".to_owned()]]
            );
        }
        target.shutdown().unwrap();
    }
    templates::remove(&f.0, "data").unwrap();
    let mut source = f.open("source");
    {
        let mut s = session(&mut source);
        assert_eq!(rows(&mut s, query), expected);
    }
    source.shutdown().unwrap();
}
#[test]
fn checksum_matching_snapshot_corruption_and_schema_injection_are_rejected() {
    let f = Fixture::new("validation");
    let valid = r#"{"format":"bicdb-graph-v1","next_id":10,"nodes":[{"id":1,"labels":["Entity"],"properties":{}}],"edges":[]}"#;
    write_bundle(&f.0, "CREATE GRAPH kg;", "kg", valid, None);
    assert!(templates::load(&f.0, "test").is_ok());
    for (sql, name, data, size) in [
        ("CREATE GRAPH kg;", "kg", valid, Some(100 * 1024 * 1024 + 1)),
        ("CREATE GRAPH kg;", "missing", valid, None),
        ("CREATE GRAPH kg;", "kg", "{}", None),
        (
            "CREATE GRAPH kg;",
            "kg",
            r#"{"format":"bicdb-graph-v1","next_id":2,"nodes":[],"edges":[{"id":1,"source":99,"target":99,"label":"LINK","properties":{}}]}"#,
            None,
        ),
        (
            "CREATE GRAPH kg; CYPHER kg 'CREATE (:Injected)';",
            "kg",
            valid,
            None,
        ),
        (
            "CREATE GRAPH kg;",
            "kg",
            r#"{"format":"bicdb-graph-v1","next_id":1,"nodes":[{"id":1,"labels":[],"properties":{}}],"edges":[]}"#,
            None,
        ),
    ] {
        write_bundle(&f.0, sql, name, data, size);
        assert!(
            templates::load(&f.0, "test").is_err(),
            "{sql} / {name} / {data}"
        );
    }
    let huge=serde_json::json!({"format":"bicdb-graph-v1","next_id":2,"nodes":[{"id":1,"labels":["x".repeat(4*1024*1024)],"properties":{}}],"edges":[]}).to_string();
    write_bundle(&f.0, "CREATE GRAPH kg;", "kg", &huge, None);
    assert!(
        templates::load(&f.0, "test")
            .unwrap_err()
            .contains("1023 chunks"),
        "portable JSON must also be encodable as native graph records before target creation"
    );
    write_bundle(&f.0, "CREATE GRAPH kg;", "kg", valid, None);
    let manifest = f.0.join("templates/test/manifest");
    let mut meta: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
    let duplicate = meta["graphs"][0].clone();
    meta["graphs"].as_array_mut().unwrap().push(duplicate);
    std::fs::write(f.0.join("templates/test/graph-1.json"), valid).unwrap();
    std::fs::write(&manifest, meta.to_string()).unwrap();
    assert!(templates::load(&f.0, "test")
        .unwrap_err()
        .contains("exactly once"));
    write_bundle(&f.0, "CREATE GRAPH kg;", "kg", valid, None);
    let file = f.0.join("templates/test/graph-0.json");
    std::fs::remove_file(&file).unwrap();
    std::fs::write(f.0.join("external"), valid).unwrap();
    std::os::unix::fs::symlink(f.0.join("external"), &file).unwrap();
    assert!(templates::load(&f.0, "test").is_err());
}
#[test]
fn trusted_restore_refuses_nonfresh_allocator_active_transaction_and_existing_indexes() {
    let f = Fixture::new("restore");
    let mut i = f.create("target");
    let snapshot=GraphSnapshot {name:"kg".into(),data:br#"{"format":"bicdb-graph-v1","next_id":100,"nodes":[{"id":7,"labels":["Entity"],"properties":{}}],"edges":[]}"#.to_vec()};
    {
        let mut s = session(&mut i);
        s.execute("CREATE GRAPH kg").unwrap();
        s.execute("BEGIN").unwrap();
        assert!(s.restore_graph_snapshot(&snapshot).is_err());
        s.execute("ROLLBACK").unwrap();
        assert!(s.execute("CYPHER kg 'CREATE (:Unused {x:1/0})'").is_err());
        let before = rows(&mut s, "SELECT * FROM seq$");
        assert!(s
            .restore_graph_snapshot(&snapshot)
            .unwrap_err()
            .to_string()
            .contains("not fresh"));
        assert_eq!(rows(&mut s, "SELECT * FROM seq$"), before);
        let empty = GraphSnapshot {
            name: "kg".into(),
            data: br#"{"format":"bicdb-graph-v1","next_id":1,"nodes":[],"edges":[]}"#.to_vec(),
        };
        assert!(
            s.restore_graph_snapshot(&empty)
                .unwrap_err()
                .to_string()
                .contains("not fresh"),
            "an empty import must not bypass the freshness guard after a failed ID reservation"
        );
        assert_eq!(rows(&mut s, "SELECT * FROM seq$"), before);
        s.execute("DROP GRAPH kg; CREATE GRAPH kg; CREATE GRAPH INDEX ix ON kg NODES (name)")
            .unwrap();
        assert!(s.restore_graph_snapshot(&snapshot).is_err());
        s.execute("DROP GRAPH INDEX ix ON kg").unwrap();
        s.restore_graph_snapshot(&snapshot).unwrap();
        assert!(s.restore_graph_snapshot(&snapshot).is_err());
        assert_eq!(
            rows(&mut s, "CYPHER kg 'CREATE (n) RETURN id(n)'"),
            vec![vec!["100".to_owned()]]
        );
    }
    i.shutdown().unwrap();
}
