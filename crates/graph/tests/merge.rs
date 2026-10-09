use bicdb_graph::{execute, parse, Graph, Limits, QueryResult, Value};
use std::collections::BTreeMap;
fn run(g: &mut Graph, text: &str) -> QueryResult {
    execute(
        g,
        &parse(text).unwrap(),
        &BTreeMap::new(),
        &Limits::default(),
    )
    .unwrap_or_else(|e| panic!("{text}: {e}"))
}
fn fails(g: &mut Graph, text: &str) {
    let before = g.clone();
    assert!(
        execute(
            g,
            &parse(text).unwrap(),
            &BTreeMap::new(),
            &Limits::default()
        )
        .is_err(),
        "{text}"
    );
    assert_eq!(*g, before, "failed statement publishes no staged action");
}
#[test]
fn merge_actions_are_per_input_row_and_see_prior_staged_rows() {
    let mut g = Graph::new();
    run(&mut g,"UNWIND [1,1,2,1] AS k MERGE (n:Entity {key:k}) ON MATCH SET n.hits=n.hits+1 ON CREATE SET n.hits=0,n.created='only_once',n:Imported SET n.always=true");
    assert_eq!(g.nodes().len(), 2);
    assert_eq!(
        run(
            &mut g,
            "MATCH (n:Entity:Imported) RETURN n.key,n.hits,n.created,n.always ORDER BY n.key"
        )
        .rows,
        vec![
            vec![
                Value::integer(1),
                Value::integer(2),
                Value::String("only_once".into()),
                Value::Bool(true)
            ],
            vec![
                Value::integer(2),
                Value::integer(0),
                Value::String("only_once".into()),
                Value::Bool(true)
            ]
        ]
    );
    run(&mut g,"MERGE (n:Entity {key:1}) ON CREATE SET n.hits=1/0 ON MATCH SET n.hits=n.hits+1 ON MATCH SET n.after=n.hits+1");
    assert_eq!(
        run(&mut g, "MATCH (n:Entity {key:1}) RETURN n.hits,n.after").rows,
        vec![vec![Value::integer(3), Value::integer(4)]]
    );
}
#[test]
fn merge_updates_every_exact_match_and_preserves_parallel_relationships() {
    let mut g = Graph::new();
    run(&mut g, "CREATE (:Entity {key:1}),(:Entity {key:1})");
    let r=run(&mut g,"MERGE (n:Entity {key:1}) ON MATCH SET n.hits=coalesce(n.hits,0)+1 ON CREATE SET n.wrong=true RETURN id(n),n.hits ORDER BY id(n)");
    assert_eq!(r.rows.len(), 2);
    assert_ne!(r.rows[0][0], r.rows[1][0]);
    assert!(r.rows.iter().all(|r| r[1] == Value::integer(1)));
    assert_eq!(g.nodes().len(), 2);
    run(
        &mut g,
        "CREATE (a:A),(b:B),(a)-[:LINK {key:1}]->(b),(a)-[:LINK {key:1}]->(b)",
    );
    let r=run(&mut g,"MATCH (a:A),(b:B) MERGE (a)-[r:LINK {key:1}]->(b) ON MATCH SET r.hits=coalesce(r.hits,0)+1 ON CREATE SET r.wrong=true RETURN id(r),r.hits ORDER BY id(r)");
    assert_eq!(r.rows.len(), 2);
    assert_eq!(g.edges().len(), 2);
    assert_ne!(r.rows[0][0], r.rows[1][0]);
    assert!(r.rows.iter().all(|r| r[1] == Value::integer(1)));
}
#[test]
fn merge_complete_paths_do_not_reuse_partial_unbound_matches() {
    let mut g = Graph::new();
    run(&mut g, "CREATE (:A {key:1}),(:B {key:2})");
    let q="MERGE p=(a:A {key:1})-[r:LINK {key:3}]->(b:B {key:2}) ON CREATE SET r.hits=0 ON MATCH SET r.hits=r.hits+1 RETURN length(p),r.hits";
    assert_eq!(
        run(&mut g, q).rows,
        vec![vec![Value::integer(1), Value::integer(0)]]
    );
    assert_eq!(
        g.nodes().len(),
        4,
        "no complete path existed, so the whole unbound path is new"
    );
    assert_eq!(
        run(&mut g, q).rows,
        vec![vec![Value::integer(1), Value::integer(1)]]
    );
    assert_eq!(g.nodes().len(), 4);
    assert_eq!(g.edges().len(), 1);
    let mut g = Graph::new();
    run(&mut g, "CREATE (:A {key:1}),(:B {key:2})");
    run(&mut g,"MATCH (a:A {key:1}),(b:B {key:2}) MERGE (a:A {key:1})-[r:LINK]->(b:B {key:2}) ON CREATE SET r.created=true");
    assert_eq!(
        g.nodes().len(),
        2,
        "previously bound matching nodes are reused"
    );
    assert_eq!(g.edges().len(), 1);
    fails(
        &mut g,
        "MATCH (a:A {key:1}),(b:B {key:2}) MERGE (a:Wrong)-[:NEW]->(b)",
    );
    fails(
        &mut g,
        "MATCH (a:A {key:1}),(b:B {key:2}) MERGE (a {key:99})-[:NEW]->(b)",
    );
}
#[test]
fn undirected_merge_reuses_either_direction_and_creates_left_to_right() {
    let mut g = Graph::new();
    run(&mut g, "CREATE (a:A),(b:B),(b)-[:BACK]->(a)");
    run(
        &mut g,
        "MATCH (a:A),(b:B) MERGE (a)-[r:BACK]-(b) ON MATCH SET r.existing=true",
    );
    assert_eq!(g.edges().len(), 1);
    assert_eq!(
        run(
            &mut g,
            "MATCH (a:A)-[r:BACK]-(b:B) RETURN startNode(r)=b,r.existing"
        )
        .rows,
        vec![vec![Value::Bool(true), Value::Bool(true)]]
    );
    run(
        &mut g,
        "MATCH (a:A),(b:B) MERGE (a)-[r:NEW]-(b) ON CREATE SET r.hits=0",
    );
    run(
        &mut g,
        "MATCH (a:A),(b:B) MERGE (b)-[r:NEW]-(a) ON MATCH SET r.hits=r.hits+1",
    );
    assert_eq!(g.edges().len(), 2);
    assert_eq!(
        run(&mut g, "MATCH (a:A)-[r:NEW]->(b:B) RETURN r.hits").rows,
        vec![vec![Value::integer(1)]]
    );
    run(
        &mut g,
        "MATCH (a:A) MERGE (a)-[r:SELF]-(a) ON CREATE SET r.hits=0",
    );
    run(
        &mut g,
        "MATCH (a:A) MERGE (a)-[r:SELF]-(a) ON MATCH SET r.hits=r.hits+1",
    );
    assert_eq!(
        run(&mut g, "MATCH (a:A)-[r:SELF]-(a) RETURN r.hits").rows,
        vec![vec![Value::integer(1)]]
    );
}
#[test]
fn invalid_merge_and_inactive_actions_fail_closed_before_publication() {
    let mut g = Graph::new();
    run(&mut g, "CREATE (:Entity {key:1})");
    for q in [
        "MERGE (n:Entity {key:null})",
        "UNWIND [1,null] AS k MERGE (n:Other {key:k}) ON CREATE SET n.created=true",
        "MERGE (a)-[r:LINK {key:null}]->(b)",
        "MERGE (n:Entity {key:1}) ON CREATE SET missing.x=1",
        "MERGE (n:Entity {key:1}) ON CREATE SET n.x=$missing",
        "MERGE (n:New) ON MATCH SET n.x=unsupported()",
        "MATCH (n:Missing) MERGE (m:New) ON CREATE SET absent.x=1",
        "MERGE (n:New) ON MATCH SET n=1",
        "MERGE (n:New) ON MATCH SET n.x=count(*)",
        "MERGE (a)-[r:LINK]->(b) ON MATCH SET r:Wrong",
        "MERGE p=(n:New) ON MATCH SET p.x=1",
        "MERGE (n:New {key:count(*)})",
        "MERGE (n:New {key:1,key:2})",
        "MERGE (n:New {key:coalesce(n.key,1)})",
        "MERGE (n:New) ON CREATE SET n.x=1/0",
        "MERGE (n:Entity {key:1}) ON MATCH SET n.x=1,n.y=1/0",
    ] {
        fails(&mut g, q);
    }
    for q in [
        "MERGE (a),(b)",
        "MERGE (n) ON CREATE REMOVE n.x",
        "CREATE (n) ON CREATE SET n.x=1",
        "MERGE (n) ON OTHER SET n.x=1",
        "MERGE (n) ON MATCH SET n:Label=1",
    ] {
        assert!(parse(q).is_err(), "{q}");
    }
    // Existing CREATE behavior remains directed-only and rejects label redefinition.
    fails(&mut g, "CREATE (a)-[:LINK]-(b)");
    fails(&mut g, "MATCH (a:Entity) CREATE (a:Entity)-[:LINK]->(b)");
}
#[test]
fn merge_branch_budget_failure_discards_all_prior_actions() {
    let mut g = Graph::new();
    run(&mut g, "CREATE (:Entity {key:1}),(:Entity {key:1})");
    let before = g.clone();
    let limits = Limits {
        max_rows: 1,
        ..Limits::default()
    };
    assert!(execute(
        &mut g,
        &parse("MERGE (n:Entity {key:1}) ON MATCH SET n.x=1").unwrap(),
        &BTreeMap::new(),
        &limits
    )
    .is_err());
    assert_eq!(g, before);
    let limits = Limits {
        max_expansions: 15,
        ..Limits::default()
    };
    assert!(execute(
        &mut g,
        &parse("UNWIND [1,2,3,4] AS k MERGE (n:New {key:k}) ON CREATE SET n.a=k+1,n.b=k+2")
            .unwrap(),
        &BTreeMap::new(),
        &limits
    )
    .is_err());
    assert_eq!(g, before);
}

#[test]
fn merge_actions_compose_with_scoped_calls_union_and_parameters() {
    let mut g = Graph::new();
    let q=parse("UNWIND $keys AS k CALL (k) { MERGE (n:Entity {key:k}) ON CREATE SET n.hits=0 ON MATCH SET n.hits=n.hits+1 RETURN n.hits AS hits } RETURN hits").unwrap();
    assert!(!q.is_read_only());
    let params = BTreeMap::from([(
        "keys".to_owned(),
        Value::List(vec![Value::integer(1), Value::integer(1)]),
    )]);
    assert_eq!(
        execute(&mut g, &q, &params, &Limits::default())
            .unwrap()
            .rows,
        vec![vec![Value::integer(0)], vec![Value::integer(1)]]
    );
    let q=parse("MERGE (n:Entity {key:1}) ON MATCH SET n.hits=n.hits+1 RETURN n.hits AS hits UNION ALL MERGE (n:Entity {key:1}) ON MATCH SET n.hits=n.hits+1 RETURN n.hits AS hits").unwrap();
    assert!(!q.is_read_only());
    assert_eq!(
        execute(&mut g, &q, &BTreeMap::new(), &Limits::default())
            .unwrap()
            .rows,
        vec![vec![Value::integer(2)], vec![Value::integer(3)]]
    );
    fails(&mut g,"MATCH (m:Absent) CALL (m) { MERGE (n:New) ON MATCH SET absent.x=1 RETURN n AS created } RETURN created");
}
