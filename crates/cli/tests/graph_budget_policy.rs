use bicdb_cli::boot::{create_instance, open_instance};
use bicdb_cli::config::InstanceParams;
use bicdb_sql::session::{format_value, GraphLimits, QueryResult, Session};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

struct Fixture(PathBuf);
impl Fixture {
    fn new(tag: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("bicdb-graph-policy-{}-{tag}", std::process::id()));
        assert!(!path.exists());
        std::fs::create_dir_all(path.join("home")).unwrap();
        Self(path)
    }
    fn root(&self) -> PathBuf {
        self.0.join("db")
    }
    fn command(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_bicdb"));
        c.env("BICDB_HOME", self.0.join("home"));
        c
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }
    fn count(&self) -> String {
        let params = InstanceParams::for_init(&self.root(), None, &[]).unwrap();
        let mut inst = open_instance(&params).unwrap();
        let value = {
            let mut s = Session::new(
                inst.pool,
                inst.engine,
                &mut inst.catalog,
                inst.engine.current_seq(),
            );
            count(&mut s)
        };
        inst.shutdown().unwrap();
        value
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn count(s: &mut Session<'_, '_, '_, '_>) -> String {
    let out = s
        .execute("CYPHER kg 'MATCH (n) RETURN count(*) AS n'")
        .unwrap();
    let Some(QueryResult::Rows { rows, .. }) = out.last() else {
        panic!("{out:?}")
    };
    format_value(&rows[0][0])
}
fn success(o: Output) {
    assert!(
        o.status.success(),
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
}

#[test]
fn trusted_policy_validation_and_request_errors_keep_prior_policy_and_sql_work() {
    let f = Fixture::new("trusted");
    let params = InstanceParams::for_init(&f.root(), None, &[]).unwrap();
    let mut inst = create_instance(&params, None).unwrap();
    {
        let seq = inst.seq();
        let mut s = Session::new(inst.pool, inst.engine, &mut inst.catalog, seq);
        s.set_graph_limits(GraphLimits {
            max_nodes: 2,
            max_rows: 2,
            ..GraphLimits::default()
        })
        .unwrap();
        s.execute(
            "CREATE GRAPH kg; CYPHER kg 'CREATE (:Item),(:Item)'; CREATE TABLE kept (v NUMBER)",
        )
        .unwrap();
        assert!(s
            .set_graph_limits(GraphLimits {
                max_rows: 10001,
                ..GraphLimits::default()
            })
            .is_err());
        let error = s
            .execute("CYPHER kg 'CREATE (:Denied)'")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("storage budget") || error.contains("element/allocator budget"),
            "{error}"
        );
        assert_eq!(count(&mut s), "2");
        assert!(s
            .execute("CYPHER kg 'MATCH (n) RETURN n' BUDGETS '{\"max_rows\":1}'")
            .unwrap_err()
            .to_string()
            .contains("row budget"));
        assert_eq!(count(&mut s), "2");
        assert!(s
            .execute("CYPHER kg 'RETURN 1' BUDGETS '{\"max_rows\":3}'")
            .is_err());
        s.execute("BEGIN; INSERT INTO kept VALUES (7)").unwrap();
        assert!(s.execute("CYPHER kg 'CREATE (:Denied)'").is_err());
        s.execute("COMMIT").unwrap();
        let result = s.execute("SELECT v FROM kept").unwrap();
        let Some(QueryResult::Rows { rows, .. }) = result.last() else {
            panic!("{result:?}")
        };
        assert_eq!(format_value(&rows[0][0]), "7");
        s.set_graph_limits(GraphLimits {
            max_nodes: 2,
            max_text_bytes: 4096,
            ..GraphLimits::default()
        })
        .unwrap();
        let params = serde_json::json!({"v":"x".repeat(3000)}).to_string();
        let source =
            format!("GRAPH_TABLE(kg,'RETURN $v' PARAMETERS '{params}' COLUMNS (v VARCHAR2(4096)))");
        let error = s
            .execute(&format!("SELECT a.v,b.v FROM {source} a,{source} b"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("GRAPH_TABLE statement materialization exceeds 4096 bytes"),
            "{error}"
        );
        assert_eq!(count(&mut s), "2");
    }
    inst.shutdown().unwrap();
}

#[test]
fn direct_sql_and_repl_apply_file_policy_and_startup_overrides() {
    let f = Fixture::new("entrypoints");
    let root = f.root();
    let path = root.to_str().unwrap();
    success(f.run(&[
        "init",
        path,
        "-c",
        "init.file0_initial_blocks=1024",
        "-c",
        "init.undo_initial_blocks=512",
        "-c",
        "graph.max_nodes=1",
        "-c",
        "graph.max_rows=2",
    ]));
    success(f.run(&["sql", "-p", path, "CREATE GRAPH kg"]));
    let o = f.run(&["sql", "-p", path, "CYPHER kg 'CREATE (:Item),(:Item)'"]);
    assert!(!o.status.success());
    let error = String::from_utf8_lossy(&o.stderr);
    assert!(
        error.contains("storage budget") || error.contains("element/allocator budget"),
        "{error}"
    );
    assert_eq!(f.count(), "0");
    success(f.run(&[
        "sql",
        "-p",
        path,
        "-c",
        "graph.max_nodes=2",
        "CYPHER kg 'CREATE (:Item),(:Item)'",
    ]));
    assert_eq!(f.count(), "2");
    success(f.run(&[
        "sql",
        "-p",
        path,
        "-c",
        "graph.max_nodes=2",
        "CYPHER kg 'MATCH (n) DELETE n'",
    ]));
    success(f.run(&["sql", "-p", path, "CYPHER kg 'CREATE (:Item)'"]));
    for (overrides, should_pass) in [(false, false), (true, true)] {
        let mut c = f.command();
        c.args(["shell", "-p", path]);
        if overrides {
            c.args(["-c", "graph.max_rows=3"]);
        }
        let mut child = c
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"CYPHER kg 'UNWIND [1,2,3] AS i RETURN i';\nEXIT\n")
            .unwrap();
        let out = child.wait_with_output().unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        if should_pass {
            assert!(text.contains('3') && !text.contains("row budget"), "{text}");
        } else {
            assert!(text.contains("row budget"), "{text}");
        }
    }
    assert_eq!(f.count(), "1");
}

#[test]
fn graph_table_sources_and_set_branches_share_edges_and_failed_insert_keeps_prior_work() {
    let f = Fixture::new("shared-edges");
    let params = InstanceParams::for_init(&f.root(), None, &[]).unwrap();
    let mut inst = create_instance(&params, None).unwrap();
    {
        let seq = inst.seq();
        let mut s = Session::new(inst.pool, inst.engine, &mut inst.catalog, seq);
        s.execute("CREATE GRAPH kg; CYPHER kg 'CREATE (a:Hub),(b:Leaf {n:1}),(c:Leaf {n:2}),(a)-[:LINK]->(b),(a)-[:LINK]->(c)'; CREATE TABLE kept (v NUMBER)").unwrap();
        s.set_graph_limits(GraphLimits {
            max_edge_expansions: 3,
            ..GraphLimits::default()
        })
        .unwrap();
        let source = "GRAPH_TABLE(kg,'MATCH (a:Hub)-[:LINK]->(b) RETURN b.n' BUDGETS '{\"max_edge_expansions\":2}' COLUMNS (n NUMBER))";
        let join = format!("SELECT a.n,b.n FROM {source} a,{source} b");
        for statement in [
            join.clone(),
            format!("SELECT a.n FROM {source} a UNION ALL SELECT b.n FROM {source} b"),
        ] {
            let error = s.execute(&statement).unwrap_err().to_string();
            assert!(error.contains("edge expansion budget"), "{error}");
        }
        s.execute("BEGIN; INSERT INTO kept VALUES (7)").unwrap();
        let insert = format!("INSERT INTO kept SELECT a.n FROM {source} a,{source} b");
        assert!(s
            .execute(&insert)
            .unwrap_err()
            .to_string()
            .contains("edge expansion budget"));
        s.execute("COMMIT").unwrap();
        let result = s.execute("SELECT v FROM kept").unwrap();
        let Some(QueryResult::Rows { rows, .. }) = result.last() else {
            panic!("{result:?}")
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(format_value(&rows[0][0]), "7");
        // A failed materialization must not leak the consumed count into the
        // following SQL request. Each single source consumes only two edges.
        s.execute(&format!("SELECT * FROM {source}")).unwrap();
        s.set_graph_limits(GraphLimits {
            max_edge_expansions: 4,
            ..GraphLimits::default()
        })
        .unwrap();
        let result = s.execute(&join).unwrap();
        let Some(QueryResult::Rows { rows, .. }) = result.last() else {
            panic!("{result:?}")
        };
        assert_eq!(rows.len(), 4);
        // Exhausting both edges still permits a zero-expansion source.
        s.set_graph_limits(GraphLimits {
            max_edge_expansions: 2,
            ..GraphLimits::default()
        })
        .unwrap();
        s.execute(&format!("SELECT a.n,c.n FROM {source} a,GRAPH_TABLE(kg,'RETURN 1' BUDGETS '{{\"max_edge_expansions\":0}}' COLUMNS (n NUMBER)) c")).unwrap();

        // Expression/query work is statement-wide too. Per-source BUDGETS may
        // tighten its share but cannot reset the workspace allowance.
        let work_source =
            "GRAPH_TABLE(kg,'RETURN 1' BUDGETS '{\"max_expansions\":1}' COLUMNS (n NUMBER))";
        s.set_graph_limits(GraphLimits {
            max_expansions: 1,
            ..GraphLimits::default()
        })
        .unwrap();
        s.execute(&format!("SELECT n FROM {work_source}")).unwrap();
        let failure = s
            .execute(&format!(
                "SELECT a.n,b.n FROM {work_source} a,{work_source} b"
            ))
            .unwrap_err()
            .to_string();
        assert!(failure.contains("budget"), "{failure}");
        // Failed statement accounting is local; a later request starts fresh.
        s.execute(&format!("SELECT n FROM {work_source}")).unwrap();
    }
    inst.shutdown().unwrap();
    drop(inst); // Release the instance lock before testing external CLI entrypoints.
    let root = f.root();
    let path = root.to_str().unwrap();
    let query = "CYPHER kg 'MATCH (a:Hub)-[:LINK]->(b) RETURN b.n'";
    let failed = f.run(&[
        "sql",
        "-p",
        path,
        "-c",
        "graph.max_edge_expansions=1",
        query,
    ]);
    assert!(!failed.status.success());
    let message = format!(
        "{}{}",
        String::from_utf8_lossy(&failed.stdout),
        String::from_utf8_lossy(&failed.stderr)
    );
    assert!(message.contains("edge expansion budget"), "{message}");
    success(f.run(&[
        "sql",
        "-p",
        path,
        "-c",
        "graph.max_edge_expansions=2",
        query,
    ]));
    success(f.run(&[
        "sql",
        "-p",
        path,
        "-c",
        "graph.max_edge_expansions=0",
        "CYPHER kg 'MATCH (a:Hub) RETURN 1'",
    ]));
    for (cap, should_pass) in [(1, false), (2, true)] {
        let mut command = f.command();
        command.args([
            "shell",
            "-p",
            path,
            "-c",
            &format!("graph.max_edge_expansions={cap}"),
        ]);
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(format!("{query};\nEXIT\n").as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            !text.contains("edge expansion budget"),
            should_pass,
            "{text}"
        );
    }
}

#[test]
fn catalog_deadline_after_partial_fulltext_ddl_rolls_back_and_leaves_cache_usable() {
    use bicdb_catalog::ddl;
    use std::collections::BTreeMap;
    use std::time::{Duration, Instant};
    let f = Fixture::new("ddl-deadline");
    let params = InstanceParams::for_init(&f.root(), None, &[]).unwrap();
    let mut inst = create_instance(&params, None).unwrap();
    ddl::create_graph(&mut inst.catalog, inst.engine, "kg").unwrap();
    {
        // The graph uses five object IDs. Consume another fourteen so the
        // failing fulltext DDL reserves its next ID batch inside the transaction.
        let seq = inst.engine.current_seq();
        let mut session = Session::new(inst.pool, inst.engine, &mut inst.catalog, seq);
        for index in 0..14 {
            session
                .execute(&format!("CREATE TABLE prime_{index} (v NUMBER)"))
                .unwrap();
        }
    }
    let entries = vec![(b"term".to_vec(), 1)];
    let rows = BTreeMap::from([(0, b"prepared image".to_vec())]);
    let build = ddl::GraphFulltextBuild {
        source: b"{}",
        entries: &entries,
        rows: &rows,
    };
    let end = Instant::now() + Duration::from_secs(1);
    let previous = inst.catalog.replace_ddl_deadline(Some(end));
    let mut allocated = None;
    let error = ddl::create_graph_fulltext_index_with_journal(
        &mut inst.catalog,
        inst.engine,
        "words",
        "kg",
        &build,
        |obj| {
            // This callback runs after the new index, document heap, dictionary
            // metadata and postings have already been written in the DDL transaction.
            allocated = Some(obj);
            std::thread::sleep(
                end.saturating_duration_since(Instant::now()) + Duration::from_millis(10),
            );
            Ok(rows.clone())
        },
    )
    .unwrap_err()
    .to_string();
    assert!(
        allocated.is_some(),
        "the deadline must expire after partial DDL writes"
    );
    assert!(error.contains("time budget"), "{error}");
    inst.catalog.replace_ddl_deadline(previous);
    {
        let seq = inst.engine.current_seq();
        let mut session = Session::new(inst.pool, inst.engine, &mut inst.catalog, seq);
        let result = session
            .execute("SHOW FULLTEXT GRAPH INDEXES ON kg")
            .unwrap();
        let Some(QueryResult::Rows { rows, .. }) = result.last() else {
            panic!("{result:?}")
        };
        assert!(
            rows.is_empty(),
            "timed out DDL must not leave an index or journal consumer"
        );
        session
            .execute("CREATE TABLE kept (v NUMBER); INSERT INTO kept VALUES (7)")
            .unwrap();
        session.execute("CREATE GRAPH next_graph").unwrap();
        // Exhaust the allocator's cache again: a rolled-back reservation must
        // never be reused after subsequent objects have committed.
        for index in 0..30 {
            session
                .execute(&format!("CREATE TABLE after_{index} (v NUMBER)"))
                .unwrap();
        }
    }
    let dictionary = inst.catalog.scan("obj$").unwrap();
    let mut ids = std::collections::BTreeSet::new();
    for (_, row) in dictionary {
        let bicdb_catalog::row::DictValue::Num(id) = row[0] else {
            panic!("invalid object ID")
        };
        assert!(ids.insert(id), "committed dictionary reuses object ID {id}");
    }
    inst.shutdown().unwrap();
    drop(inst);
    let mut inst = open_instance(&params).unwrap();
    {
        let mut session = Session::new(
            inst.pool,
            inst.engine,
            &mut inst.catalog,
            inst.engine.current_seq(),
        );
        let result = session
            .execute("SHOW FULLTEXT GRAPH INDEXES ON kg")
            .unwrap();
        let Some(QueryResult::Rows { rows, .. }) = result.last() else {
            panic!("{result:?}")
        };
        assert!(rows.is_empty());
        let result = session.execute("SELECT v FROM kept").unwrap();
        let Some(QueryResult::Rows { rows, .. }) = result.last() else {
            panic!("{result:?}")
        };
        assert_eq!(format_value(&rows[0][0]), "7");
    }
    inst.shutdown().unwrap();
}
