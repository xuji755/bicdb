use bicdb_cli::boot::{create_instance, open_instance, Instance};
use bicdb_cli::config::InstanceParams;
use bicdb_sql::session::{format_value, QueryResult, Session, MAX_DETACH_EDGE_LIMIT};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

struct Fixture(PathBuf);
impl Fixture {
    fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!("bicdb-detach-{}-{tag}", std::process::id()));
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
    fn count(&self, pattern: &str) -> String {
        let params = InstanceParams::for_init(&self.root(), None, &[]).unwrap();
        let mut inst = open_instance(&params).unwrap();
        let count = {
            let seq = inst.seq();
            let mut s = Session::new(inst.pool, inst.engine, &mut inst.catalog, seq);
            count(&mut s, pattern)
        };
        inst.shutdown().unwrap();
        count
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn count(s: &mut Session<'_, '_, '_, '_>, pattern: &str) -> String {
    let r = s
        .execute(&format!("CYPHER kg 'MATCH {pattern} RETURN count(*) AS n'"))
        .unwrap();
    let Some(QueryResult::Rows { rows, .. }) = r.last() else {
        panic!("{r:?}")
    };
    assert_eq!(rows.len(), 1);
    format_value(&rows[0][0])
}
fn sql(q: &str) -> String {
    format!("CYPHER kg '{}'", q.replace('\'', "''"))
}
const SEED: &str = "CREATE (a:Hub),(b:Leaf),(a)-[:LINK]->(b),(a)-[:LINK]->(b)";
const DELETE: &str = "MATCH (a:Hub) DETACH DELETE a";
fn assert_success(o: &Output) {
    assert!(
        o.status.success(),
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
}
fn assert_budget(o: &Output, limit: usize) {
    let text = String::from_utf8_lossy(&o.stderr);
    assert!(
        text.contains("DETACH DELETE relationship budget exceeded"),
        "{text}"
    );
    assert!(text.contains(&format!("limit {limit}")), "{text}");
}
#[test]
fn trusted_session_bound_validation_and_transaction_rollback() {
    let f = Fixture::new("session");
    let params = InstanceParams::for_init(&f.root(), None, &[]).unwrap();
    let mut inst: Instance = create_instance(&params, None).unwrap();
    {
        let seq = inst.seq();
        let mut s = Session::new(inst.pool, inst.engine, &mut inst.catalog, seq);
        s.execute("CREATE GRAPH kg").unwrap();
        s.execute(&sql(SEED)).unwrap();
        s.set_graph_detach_edge_limit(1).unwrap();
        assert!(s
            .set_graph_detach_edge_limit(MAX_DETACH_EDGE_LIMIT + 1)
            .is_err());
        let e = s.execute(&sql(DELETE)).unwrap_err().to_string();
        assert!(
            e.contains("limit 1"),
            "invalid configuration must preserve the last valid bound: {e}"
        );
        assert_eq!(count(&mut s, "(n)"), "2");
        assert_eq!(count(&mut s, "()-[r]->()"), "2");
        s.set_graph_detach_edge_limit(2).unwrap();
        s.execute("BEGIN").unwrap();
        s.execute(&sql(DELETE)).unwrap();
        assert_eq!(count(&mut s, "(n)"), "1");
        assert_eq!(count(&mut s, "()-[r]->()"), "0");
        s.execute("ROLLBACK").unwrap();
        assert_eq!(count(&mut s, "(n)"), "2");
        assert_eq!(count(&mut s, "()-[r]->()"), "2");
    }
    inst.shutdown().unwrap();
}
#[test]
fn direct_sql_and_repl_use_file_defaults_and_cli_override() {
    let f = Fixture::new("commands");
    let root = f.root();
    let path = root.to_str().unwrap();
    assert_success(&f.run(&[
        "init",
        path,
        "-c",
        "init.file0_initial_blocks=1024",
        "-c",
        "init.undo_initial_blocks=512",
        "-c",
        "graph.detach_edge_limit=0",
    ]));
    assert_success(&f.run(&[
        "sql",
        "-p",
        path,
        &format!("CREATE GRAPH kg; {}", sql(SEED)),
    ]));
    for (options, limit) in [(None, 0), (Some("graph.detach_edge_limit=1"), 1)] {
        let mut args = vec!["sql", "-p", path];
        if let Some(o) = options {
            args.extend(["-c", o]);
        }
        let q = sql(DELETE);
        args.push(&q);
        let out = f.run(&args);
        assert!(!out.status.success());
        assert_budget(&out, limit);
        assert_eq!(f.count("(n)"), "2");
        assert_eq!(f.count("()-[r]->()"), "2");
    }
    assert_success(&f.run(&[
        "sql",
        "-p",
        path,
        "-c",
        "graph.detach_edge_limit=2",
        &sql(DELETE),
    ]));
    assert_eq!(f.count("(n)"), "1");
    assert_eq!(f.count("()-[r]->()"), "0");
    assert_success(&f.run(&["sql", "-p", path, &sql(SEED)]));
    for (override_, expected_nodes, expected_edges) in [
        (None, "3", "2"),
        (Some("graph.detach_edge_limit=2"), "2", "0"),
    ] {
        let mut cmd = f.command();
        cmd.args(["shell", "-p", path]);
        if let Some(o) = override_ {
            cmd.args(["-c", o]);
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(format!("{};\n.quit\n", sql(DELETE)).as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert_success(&out);
        if override_.is_none() {
            assert_budget(&out, 0);
        } else {
            assert!(!String::from_utf8_lossy(&out.stderr).contains("budget exceeded"));
        }
        assert_eq!(f.count("(n)"), expected_nodes);
        assert_eq!(f.count("()-[r]->()"), expected_edges);
    }
}
