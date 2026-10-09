use bicdb_graph::{execute, parse, Direction, Graph, Limits, Value};
use std::collections::BTreeMap;

fn query(g: &mut Graph, source: &str) -> bicdb_graph::QueryResult {
    execute(
        g,
        &parse(source).unwrap(),
        &BTreeMap::new(),
        &Limits::default(),
    )
    .unwrap_or_else(|e| panic!("{source}: {e}"))
}
fn graph() -> Graph {
    let mut g = Graph::new();
    query(&mut g,"CREATE (f:Fault:Entity {db_type:'2123',name:'IO high'}), (k:Knowledge {db_type:'2123',name:'buffer pool',tags:['IO','memory']}), (p:Parameter {db_type:'2123',name:'innodb_buffer_pool_size'}), (x:Fault {db_type:'mysql',name:'IO high'}), (f)-[:CAUSES {weight:2}]->(k), (f)-[:CAUSES {weight:3}]->(k), (k)-[:RELATED]->(p), (p)-[:BACK]->(f), (f)-[:SELF]->(f)");
    g
}
#[test]
fn scoped_identity_direction_and_parallel_edges() {
    let mut g = graph();
    let r=query(&mut g,"MATCH (f:Fault:Entity)-[r:CAUSES]-(n) WHERE f.db_type = '2123' RETURN elementId(f) AS eid, n.name AS name, id(r) AS edge, startNode(r)=f AS outgoing ORDER BY edge");
    assert_eq!(r.rows.len(), 2);
    assert_ne!(r.rows[0][2], r.rows[1][2]);
    assert_eq!(r.rows[0][3], Value::Bool(true));
    assert_eq!(
        query(&mut g, "MATCH (f:Fault)-[r:SELF]-(f) RETURN count(r) AS c").rows[0][0],
        Value::integer(1)
    );
    assert_eq!(
        query(
            &mut g,
            "MATCH (p:Parameter)<-[r:RELATED]-(k) RETURN k.name AS name"
        )
        .rows[0][0],
        Value::String("buffer pool".into())
    );
}
#[test]
fn optional_match_predicate_preserves_unmatched_rows() {
    let mut g = graph();
    let r=query(&mut g,"MATCH (f:Fault) OPTIONAL MATCH (f)-[r:CAUSES]->(k) WHERE r.weight > 2 RETURN f.db_type AS domain, count(r) AS links, collect(k.name) AS names ORDER BY domain");
    assert_eq!(r.rows.len(), 2);
    assert_eq!(r.rows[0][1], Value::integer(1));
    assert_eq!(r.rows[1][1], Value::integer(0));
    assert_eq!(r.rows[1][2], Value::List(vec![]));
}
#[test]
fn aggregation_with_scope_distinct_and_order() {
    let mut g = graph();
    let r=query(&mut g,"MATCH (f:Fault)-[r]->(k) WHERE f.db_type='2123' WITH f, count(r) AS n, collect(DISTINCT type(r)) AS types WHERE n > 1 RETURN f.name AS name,n,types ORDER BY n DESC LIMIT 2");
    assert_eq!(r.rows[0][1], Value::integer(3));
    assert_eq!(
        query(
            &mut g,
            "MATCH (n:Absent) RETURN count(*) AS c,collect(n) AS nodes"
        )
        .rows,
        vec![vec![Value::integer(0), Value::List(vec![])]]
    );
    assert_eq!(
        query(
            &mut g,
            "UNWIND [3,1,3] AS n RETURN DISTINCT n ORDER BY n SKIP 1 LIMIT 1"
        )
        .rows,
        vec![vec![Value::integer(3)]]
    );
}
#[test]
fn bounded_paths_and_shortest_bfs_with_cycles() {
    let mut g = graph();
    let r=query(&mut g,"MATCH path=(f:Fault:Entity)-[r:CAUSES|RELATED*2..2]->(p:Parameter) RETURN length(path) AS hops,size(r) AS edges");
    assert_eq!(r.rows.len(), 2);
    assert_eq!(r.rows[0], vec![Value::integer(2), Value::integer(2)]);
    let r=query(&mut g,"MATCH path=shortestPath((f:Fault:Entity)-[:CAUSES|RELATED*1..4]->(p:Parameter)) RETURN length(path) AS hops");
    assert_eq!(r.rows, vec![vec![Value::integer(2)]]);
    let f = g
        .nodes()
        .values()
        .find(|n| n.labels.contains("Entity"))
        .unwrap()
        .id;
    let p = g
        .nodes()
        .values()
        .find(|n| n.labels.contains("Parameter"))
        .unwrap()
        .id;
    assert_eq!(
        g.shortest_path(f, p, Direction::Out, &[], &Limits::default())
            .unwrap()
            .unwrap()
            .edges
            .len(),
        2
    );
    assert!(parse("MATCH (a)-[*]->(b) RETURN b").is_err());
}
#[test]
fn parameters_unicode_exact_numbers_and_three_valued_logic() {
    let mut g = graph();
    let mut params = BTreeMap::new();
    params.insert("domain".into(), Value::String("2123".into()));
    let r = execute(
        &mut g,
        &parse("MATCH (f:Fault) WHERE f.db_type=$domain RETURN f.name AS name").unwrap(),
        &params,
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(r.rows.len(), 1);
    let r=query(&mut g,"RETURN 9007199254740993 + 1 AS exact,1e-3 AS decimal,toLower('数据库IO') AS text,null AND false AS a,null OR true AS b,2 IN [1,null] AS c");
    assert_eq!(r.rows[0][0].to_json(&g).to_string(), "9007199254740994");
    assert_eq!(r.rows[0][2], Value::String("数据库io".into()));
    assert_eq!(
        &r.rows[0][3..],
        &[Value::Bool(false), Value::Bool(true), Value::Null]
    );
}
#[test]
fn list_case_reduce_quantifiers_exists_and_union() {
    let mut g = graph();
    let r=query(&mut g,"MATCH (f:Fault:Entity) RETURN [x IN labels(f) WHERE x <> 'Entity' | toLower(x)] AS tags,reduce(s=0,x IN [1,2,3] | s+x) AS score,any(x IN [null,2] WHERE x=2) AS any,CASE WHEN f.name CONTAINS 'IO' THEN 5 ELSE 0 END AS bonus");
    assert_eq!(
        r.rows[0][0],
        Value::List(vec![Value::String("fault".into())])
    );
    assert_eq!(r.rows[0][1], Value::integer(6));
    assert_eq!(query(&mut g,"MATCH (f:Fault) WHERE NOT EXISTS { MATCH (f)-[:CAUSES]->(k) } RETURN f.db_type AS domain").rows,vec![vec![Value::String("mysql".into())]]);
    assert_eq!(
        query(&mut g, "RETURN 1 AS n UNION RETURN 1 AS n")
            .rows
            .len(),
        1
    );
    assert_eq!(
        query(&mut g, "RETURN 1 AS n UNION ALL RETURN 1 AS n")
            .rows
            .len(),
        2
    );
}
#[test]
fn mutations_are_atomic_and_merge_is_idempotent() {
    let mut g = graph();
    let before = g.clone();
    assert!(execute(
        &mut g,
        &parse("CREATE (n:Bad) RETURN 1/0 AS v").unwrap(),
        &BTreeMap::new(),
        &Limits::default()
    )
    .is_err());
    assert_eq!(before, g);
    query(&mut g,"MERGE (n:Preference {name:'language'}) SET n.value='中文',n:Entity RETURN n.value AS value");
    let n = g.nodes().len();
    query(
        &mut g,
        "MERGE (n:Preference {name:'language'}) SET n.value='日本語'",
    );
    assert_eq!(g.nodes().len(), n);
    assert_eq!(
        query(
            &mut g,
            "MATCH (n:Preference:Entity) RETURN n.value AS value"
        )
        .rows[0][0],
        Value::String("日本語".into())
    );
    query(&mut g, "MATCH (n:Preference) REMOVE n:Entity,n.value");
    assert!(query(&mut g, "MATCH (n:Preference:Entity) RETURN n")
        .rows
        .is_empty());
    assert!(execute(
        &mut g,
        &parse("MATCH (n:Knowledge) DELETE n").unwrap(),
        &BTreeMap::new(),
        &Limits::default()
    )
    .is_err());
    query(&mut g, "MATCH (n:Knowledge) DETACH DELETE n");
    assert_eq!(g.edges().len(), 2);
}
#[test]
fn snapshot_roundtrip_validation_and_fail_closed_empty_graph() {
    let g = graph();
    assert_eq!(
        Graph::from_bytes(&g.to_bytes().unwrap(), &Limits::default()).unwrap(),
        g
    );
    assert!(Graph::from_bytes(br#"{"format":"bicdb-graph-v1","next_id":1,"nodes":[{"id":1,"labels":[],"properties":{}}],"edges":[]}"#,&Limits::default()).is_err());
    let mut g = Graph::new();
    assert!(execute(
        &mut g,
        &parse("MATCH (n) RETURN unsupported(n) AS x").unwrap(),
        &BTreeMap::new(),
        &Limits::default()
    )
    .is_err());
    assert!(execute(
        &mut g,
        &parse("MATCH (n) WHERE n.x=$missing RETURN n").unwrap(),
        &BTreeMap::new(),
        &Limits::default()
    )
    .is_err());
    assert!(parse("CALL db.index.fulltext.queryNodes('x','y') YIELD node RETURN node").is_err());
}
#[test]
fn traversal_budget_is_explicit_and_does_not_publish_partial_writes() {
    let mut g = graph();
    let before = g.clone();
    let limits = Limits {
        max_expansions: 3,
        ..Limits::default()
    };
    assert!(execute(
        &mut g,
        &parse("CREATE (n:New) MATCH (f)-[*1..4]-(x) RETURN x").unwrap(),
        &BTreeMap::new(),
        &limits
    )
    .is_err());
    assert_eq!(g, before);
    let deeply_nested = format!("RETURN {}1{} AS x", "(".repeat(100), ")".repeat(100));
    assert!(parse(&deeply_nested).is_err());
    assert!(parse(&format!("RETURN {}1 AS n", "1+".repeat(1000))).is_err());
    assert!(parse(&format!("MATCH (n){} RETURN n", "--(n)".repeat(1000))).is_err());
}
#[test]
fn rust_daemon_style_direct_and_via_subquery_keeps_edge_identity() {
    let mut g = graph();
    let result=query(&mut g,"MATCH (f:Fault:Entity) CALL (f) { MATCH (f)-[r:CAUSES]->(n) RETURN n.name AS name,elementId(r) AS eid,'direct' AS origin UNION ALL MATCH (f)-[via:CAUSES]->(ke)-[r:RELATED]->(n) RETURN n.name AS name,elementId(r) AS eid,'via' AS origin } RETURN name,eid,origin ORDER BY origin,eid");
    assert_eq!(result.rows.len(), 4);
    assert_eq!(result.rows[0][2], Value::String("direct".into()));
    assert_eq!(result.rows[2][2], Value::String("via".into()));
    assert!(!parse("CALL () { CREATE (n) RETURN n } RETURN n")
        .unwrap()
        .is_read_only());
}
#[test]
fn relationship_uniqueness_spans_patterns_in_one_match() {
    let mut g = graph();
    assert!(
        query(&mut g, "MATCH (a)-[r:SELF]->(a),(a)-[r:SELF]->(a) RETURN r")
            .rows
            .is_empty()
    );
    assert_eq!(
        query(
            &mut g,
            "MATCH (a)-[r:SELF]->(a) MATCH (a)-[r:SELF]->(a) RETURN r"
        )
        .rows
        .len(),
        1
    );
    assert!(execute(
        &mut g,
        &parse("MATCH (n:Missing) SET n={x:1}").unwrap(),
        &BTreeMap::new(),
        &Limits::default()
    )
    .is_err());
}
#[test]
fn field_round_robin_search_scopes_before_limit_and_marks_scan_mode() {
    let mut g = graph();
    query(&mut g,"CREATE (a:Knowledge {db_type:'mysql',name:'IO'}), (b:Knowledge {db_type:'2123',name:'IO',summary:'IO high'}), (c:Knowledge {db_type:'2123',name:'another',summary:'IO'})");
    let r=query(&mut g,"CALL bicdb.searchNodes('io',{db_type:'2123',labels:['Knowledge'],fields:['name','summary'],limit:2}) YIELD node,rank,mode RETURN node.name AS name,rank,mode");
    assert_eq!(r.rows.len(), 2);
    assert_eq!(
        r.rows[0],
        vec![
            Value::String("IO".into()),
            Value::integer(1),
            Value::String("bounded_scan".into())
        ]
    );
    assert_eq!(r.rows[1][0], Value::String("another".into()));
    query(
        &mut g,
        "CREATE (n:Parameter {db_type:'2123',alias:['disk','IO tuning']})",
    );
    let r=query(&mut g,"CALL bicdb.searchNodes('io',{db_type:'2123',labels:['Fault','Parameter'],fields:['name','alias'],limit:10}) YIELD node RETURN labels(node) AS labels");
    assert_eq!(r.rows.len(), 2);
    assert!(execute(
        &mut g,
        &parse("CALL bicdb.searchNodes('io',{}) YIELD node RETURN node").unwrap(),
        &BTreeMap::new(),
        &Limits::default()
    )
    .is_err());
    assert!(execute(
        &mut g,
        &parse("MATCH (n:Fault:Entity) DELETE n RETURN labels(n) AS labels").unwrap(),
        &BTreeMap::new(),
        &Limits::default()
    )
    .is_err());
}

#[test]
fn scope_errors_are_rejected_even_without_matches() {
    let mut g = Graph::new();
    for source in [
        "MATCH (n) RETURN missing",
        "MATCH p=(p) RETURN p",
        "MATCH (n) RETURN n.name + count(*) AS mixed",
        "MATCH (n) WITH n.name AS name RETURN n",
    ] {
        assert!(
            execute(
                &mut g,
                &parse(source).unwrap(),
                &BTreeMap::new(),
                &Limits::default()
            )
            .is_err(),
            "{source}"
        );
    }
    let r = query(&mut g, "MATCH (n) RETURN *");
    assert!(r.rows.is_empty());
    assert_eq!(r.columns, vec!["n"]);
    let r = query(
        &mut g,
        "UNWIND [1,2,3] AS n RETURN reduce(s=0,x IN collect(n) | s+x) AS total",
    );
    assert_eq!(r.rows[0][0], Value::integer(6));
    query(
        &mut g,
        "CREATE (n:Person {name:'same',v:1}), (m:Person {name:'same',v:2})",
    );
    let r = query(
        &mut g,
        "MATCH (n:Person) RETURN n.name AS name,n.name + toString(count(*)) AS decorated",
    );
    assert_eq!(r.rows[0][1], Value::String("same2".into()));
}
