use bicdb_graph::{execute, parse, Graph, Limits, Value, DEFAULT_DETACH_EDGE_LIMIT};
use std::collections::{BTreeMap, BTreeSet};
fn run(g: &mut Graph, text: &str, limit: usize) -> bicdb_graph::QueryResult {
    execute(
        g,
        &parse(text).unwrap(),
        &BTreeMap::new(),
        &Limits {
            max_detach_edges: limit,
            ..Limits::default()
        },
    )
    .unwrap_or_else(|e| panic!("{text}: {e}"))
}
fn over_limit(g: &mut Graph, text: &str, limit: usize) {
    let before = g.clone();
    let e = execute(
        g,
        &parse(text).unwrap(),
        &BTreeMap::new(),
        &Limits {
            max_detach_edges: limit,
            ..Limits::default()
        },
    )
    .unwrap_err()
    .to_string();
    assert!(
        e.contains("DETACH DELETE relationship budget exceeded"),
        "{e}"
    );
    assert!(e.contains(&format!("limit {limit}")), "{e}");
    assert_eq!(
        g, &before,
        "no partial node, relationship, label or allocator changes"
    );
}
fn separate_hubs() -> Graph {
    let mut g = Graph::new();
    run(&mut g,"CREATE (a:Hub {key:1}),(b:Hub {key:2}),(x),(y),(a)-[:LINK]->(x),(x)-[:LINK]->(a),(b)-[:LINK]->(y),(y)-[:LINK]->(b)",10);
    g
}
#[test]
fn incidence_budget_deduplicates_self_loops_parallel_edges_and_shared_endpoints() {
    let mut g = Graph::new();
    run(&mut g,"CREATE (a:Hub),(b:Neighbor),(c:Incoming),(a)-[:LINK]->(b),(a)-[:LINK]->(b),(c)-[:LINK]->(a),(a)-[:SELF]->(a)",10);
    assert_eq!(g.edges().len(), 4);
    over_limit(&mut g, "MATCH (a:Hub),(b:Neighbor) DETACH DELETE a,b", 3);
    // The self-loop appears in two incidence lists, and shared endpoints repeat
    // both parallel keys; all four physical relationship identities count once.
    let r = run(&mut g, "MATCH (a:Hub),(b:Neighbor) DETACH DELETE a,b", 4);
    assert_eq!(
        r.mutations, 2,
        "keep affected count for explicitly selected entities"
    );
    assert_eq!(g.nodes().len(), 1);
    assert!(g.edges().is_empty());
    assert!(g
        .nodes()
        .values()
        .next()
        .unwrap()
        .labels
        .contains("Incoming"));
}
#[test]
fn detach_budget_cannot_reset_at_clause_call_or_union_boundaries() {
    for q in [
        "MATCH (a:Hub {key:1}),(b:Hub {key:2}) DETACH DELETE a DETACH DELETE b",
        "MATCH (a:Hub {key:1}) DETACH DELETE a WITH 1 AS marker MATCH (b:Hub {key:2}) DETACH DELETE b",
        "UNWIND [1,2] AS k CALL (k) { MATCH (n:Hub {key:k}) DETACH DELETE n RETURN k AS done } RETURN done",
        "MATCH (a:Hub {key:1}) DETACH DELETE a RETURN 1 AS done UNION ALL MATCH (b:Hub {key:2}) DETACH DELETE b RETURN 2 AS done",
    ] {
        let mut g=separate_hubs();
        over_limit(&mut g,q,3);
        run(&mut g,q,4);
        assert_eq!(g.nodes().len(),2);
        assert!(g.edges().is_empty());
    }
}
#[test]
fn zero_budget_permits_isolated_nodes_and_explicit_edge_deletion_without_bypass() {
    let mut g = Graph::new();
    run(
        &mut g,
        "CREATE (a:Hub),(b:Leaf),(c:Isolated),(a)-[:LINK]->(b)",
        0,
    );
    run(&mut g, "MATCH (c:Isolated) DETACH DELETE c", 0);
    over_limit(&mut g, "MATCH (a:Hub)-[r]->(b) DETACH DELETE a,r", 0);
    let before = g.clone();
    assert!(execute(
        &mut g,
        &parse("MATCH (a:Hub) DELETE a").unwrap(),
        &BTreeMap::new(),
        &Limits::default()
    )
    .is_err());
    assert_eq!(
        g, before,
        "ordinary DELETE still requires no incident relationships"
    );
    // Explicitly removing an edge in a preceding clause changes the topology;
    // it is no longer incident when DETACH executes and cannot be double counted.
    run(&mut g, "MATCH (a:Hub)-[r]->(b) DELETE r DETACH DELETE a", 0);
    assert_eq!(g.nodes().len(), 1);
    assert!(g.edges().is_empty());
}
#[test]
fn shared_edges_removed_by_an_earlier_detach_are_not_charged_twice() {
    let mut g = Graph::new();
    run(&mut g,"CREATE (a:Hub {key:1}),(b:Hub {key:2}),(x),(y),(a)-[:LINK]->(x),(a)-[:LINK]->(b),(b)-[:LINK]->(y)",10);
    over_limit(
        &mut g,
        "MATCH (a:Hub {key:1}),(b:Hub {key:2}) DETACH DELETE a DETACH DELETE b",
        2,
    );
    run(
        &mut g,
        "MATCH (a:Hub {key:1}),(b:Hub {key:2}) DETACH DELETE a DETACH DELETE b",
        3,
    );
    assert_eq!(g.nodes().len(), 2);
    assert!(g.edges().is_empty());
}
#[test]
fn incidence_preflight_consumes_expression_work_without_deleting_part_of_a_hub() {
    let mut g = Graph::new();
    run(&mut g,"CREATE (a:Hub),(b) WITH a,b UNWIND [1,2,3,4,5,6,7,8,9,10,11,12] AS i CREATE (a)-[:LINK]->(b)",100);
    let before = g.clone();
    let e = execute(
        &mut g,
        &parse("MATCH (a:Hub) DETACH DELETE a").unwrap(),
        &BTreeMap::new(),
        &Limits {
            max_expansions: 10,
            max_detach_edges: 100,
            ..Limits::default()
        },
    )
    .unwrap_err()
    .to_string();
    assert!(
        e.contains("graph expansion/expression budget exceeded"),
        "{e}"
    );
    assert_eq!(g, before);
}
#[test]
fn default_budget_rejects_10001_edges_and_accepts_the_exact_boundary() {
    assert_eq!(
        Limits::default().max_detach_edges,
        DEFAULT_DETACH_EDGE_LIMIT
    );
    assert_eq!(DEFAULT_DETACH_EDGE_LIMIT, 10000);
    let mut g = Graph::new();
    let hub = g
        .add_node(BTreeSet::from(["Hub".to_owned()]), BTreeMap::new())
        .unwrap();
    let leaf = g.add_node(BTreeSet::new(), BTreeMap::new()).unwrap();
    let mut last = 0;
    for _ in 0..=DEFAULT_DETACH_EDGE_LIMIT {
        last = g
            .add_edge(hub, leaf, "LINK".to_owned(), BTreeMap::new())
            .unwrap();
    }
    over_limit(
        &mut g,
        "MATCH (a:Hub) DETACH DELETE a",
        DEFAULT_DETACH_EDGE_LIMIT,
    );
    // Remove exactly one relationship, then the remaining 10000 are permitted.
    g.remove_edge(last);
    let r = execute(
        &mut g,
        &parse("MATCH (a:Hub) DETACH DELETE a").unwrap(),
        &BTreeMap::new(),
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(r.mutations, 1);
    assert_eq!(g.nodes().len(), 1);
    assert!(g.edges().is_empty());
    assert_eq!(
        run(&mut g, "MATCH (n) RETURN count(n) AS n", 0).rows,
        vec![vec![Value::integer(1)]]
    );
}
