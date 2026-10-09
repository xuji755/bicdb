use bicdb_graph::{execute, parse, Direction, Graph, Limits, QueryResult};
use std::collections::BTreeMap;

fn graph() -> Graph {
    let mut graph = Graph::new();
    execute(&mut graph, &parse("CREATE (a:Hub),(b:Leaf),(c:Leaf),(a)-[:LINK {w:1}]->(b),(a)-[:LINK {w:2}]->(c),(b)-[:BACK]->(a),(a)-[:SELF]->(a)").unwrap(), &BTreeMap::new(), &Limits::default()).unwrap();
    graph
}
fn run(g: &mut Graph, query: &str, cap: usize) -> Result<QueryResult, bicdb_graph::Error> {
    execute(
        g,
        &parse(query).unwrap(),
        &BTreeMap::new(),
        &Limits {
            max_edge_expansions: cap,
            ..Limits::default()
        },
    )
}
fn reject(g: &mut Graph, query: &str, cap: usize) {
    let before = g.clone();
    let error = run(g, query, cap).unwrap_err().to_string();
    assert!(error.contains("edge expansion budget"), "{query}: {error}");
    assert_eq!(*g, before, "failed query must not publish its staged graph");
}

#[test]
fn edge_candidates_are_independent_of_expression_work_and_residual_matches() {
    let mut g = graph();
    let point = run(&mut g, "MATCH (n:Hub) RETURN n", 0).unwrap();
    assert_eq!(point.edge_expansions, 0);
    assert!(point.expansions > 0);
    let q = "MATCH (a:Hub)-[r:LINK]->(b) RETURN r.w AS w ORDER BY w";
    let result = run(&mut g, q, 2).unwrap();
    assert_eq!(result.edge_expansions, 2);
    assert!(result.expansions > result.edge_expansions);
    assert_eq!(result.rows.len(), 2);
    reject(&mut g, q, 1);
    let filtered = "MATCH (a:Hub)-[r:LINK {w:99}]->(b) RETURN b";
    reject(&mut g, filtered, 1);
    assert!(run(&mut g, filtered, 2).unwrap().rows.is_empty());
    let error = execute(
        &mut g,
        &parse("UNWIND [1,2,3] AS i RETURN i").unwrap(),
        &BTreeMap::new(),
        &Limits {
            max_expansions: 2,
            max_edge_expansions: 0,
            ..Limits::default()
        },
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("expansion/expression budget"), "{error}");
}

#[test]
fn repeated_adjacency_visits_self_loops_and_paths_consume_their_actual_candidates() {
    let mut g = graph();
    assert_eq!(
        run(&mut g, "MATCH (a:Hub)-[:SELF]-(a) RETURN a", 1)
            .unwrap()
            .edge_expansions,
        1
    );
    assert_eq!(
        run(&mut g, "MATCH (a:Hub)<-[:BACK]-(b) RETURN b", 1)
            .unwrap()
            .edge_expansions,
        1
    );
    let q = "MATCH (a:Hub)-[:SELF*1..2]->(b) RETURN b";
    reject(&mut g, q, 1);
    assert_eq!(run(&mut g, q, 2).unwrap().edge_expansions, 2);
    // Revisiting an already-used relationship is still examined work.
    let q = "MATCH (a:Hub)-[:LINK*1..2]-(b) RETURN b";
    reject(&mut g, q, 3);
    assert_eq!(run(&mut g, q, 4).unwrap().edge_expansions, 4);
    let q = "MATCH p=shortestPath((a:Hub)-[:LINK*1..2]-(b:Leaf)) RETURN length(p)";
    reject(&mut g, q, 3);
    assert_eq!(run(&mut g, q, 4).unwrap().edge_expansions, 4);
}

#[test]
fn call_exists_union_and_staged_writes_cannot_reset_the_edge_counter() {
    let mut g = graph();
    for q in [
        "MATCH (a:Hub) CALL (a) { MATCH (a)-[:LINK]->(b) RETURN count(b) AS n } CALL (a) { MATCH (a)-[:LINK]->(c) RETURN count(c) AS m } RETURN n,m",
        "MATCH (a:Hub) WHERE EXISTS { MATCH (a)-[:LINK]->(b) } WITH a MATCH (a)-[:LINK]->(c) RETURN c",
        "MATCH (a:Hub)-[:LINK]->(b) RETURN b UNION ALL MATCH (a:Hub)-[:LINK]->(b) RETURN b",
    ] {
        reject(&mut g, q, 3);
        assert_eq!(run(&mut g, q, 4).unwrap().edge_expansions, 4, "{q}");
    }
    reject(
        &mut g,
        "CREATE (:Transient) WITH 1 AS x MATCH (a:Hub)-[:LINK]->(b) SET b.touched=true RETURN b",
        1,
    );
}

#[test]
fn detach_counts_incidence_work_separately_from_distinct_deleted_edges() {
    let mut g = graph();
    reject(&mut g, "MATCH (a:Hub) DETACH DELETE a", 4);
    // Four distinct relationships; the self-loop has two incidence entries.
    let result = execute(
        &mut g,
        &parse("MATCH (a:Hub) DETACH DELETE a").unwrap(),
        &BTreeMap::new(),
        &Limits {
            max_edge_expansions: 5,
            max_detach_edges: 4,
            ..Limits::default()
        },
    )
    .unwrap();
    assert_eq!(result.edge_expansions, 5);
    assert!(g.edges().is_empty());
    let mut g = graph();
    reject(&mut g, "MATCH (a:Hub)-[:LINK]->(b) DETACH DELETE a", 6);
    // Traversal 2 plus the first DETACH incidence scan 5 exceeds six even
    // before the second matching row is reached.
}

#[test]
fn standalone_bfs_checks_edge_budget_before_visited_filtering() {
    let g = graph();
    let source = *g
        .nodes()
        .values()
        .find(|n| n.labels.contains("Hub"))
        .map(|n| &n.id)
        .unwrap();
    let target = *g
        .nodes()
        .values()
        .filter(|n| n.labels.contains("Leaf"))
        .map(|n| &n.id)
        .max()
        .unwrap();
    let types = vec!["LINK".to_owned()];
    let low = Limits {
        max_edge_expansions: 1,
        ..Limits::default()
    };
    assert!(g
        .shortest_path(source, target, Direction::Out, &types, &low)
        .unwrap_err()
        .to_string()
        .contains("edge expansion budget"));
    let exact = Limits {
        max_edge_expansions: 2,
        ..Limits::default()
    };
    assert_eq!(
        g.shortest_path(source, target, Direction::Out, &types, &exact)
            .unwrap()
            .unwrap()
            .edges
            .len(),
        1
    );
    let zero = Limits {
        max_edge_expansions: 0,
        ..Limits::default()
    };
    assert!(g
        .shortest_path(source, source, Direction::Both, &[], &zero)
        .unwrap()
        .unwrap()
        .edges
        .is_empty());
}
