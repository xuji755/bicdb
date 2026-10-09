use bicdb_graph::{execute, parse, Graph, Limits};
use std::collections::BTreeMap;

#[test]
fn request_can_only_tighten_workspace_policy_without_changing_it() {
    let workspace = Limits {
        max_rows: 7,
        max_depth: 3,
        max_expansions: 100,
        max_edge_expansions: 20,
        max_elapsed_ms: 200,
        max_text_bytes: 4096,
        max_detach_edges: 2,
        ..Limits::default()
    };
    let before = workspace.clone();
    let small = workspace
        .with_request_budgets(&serde_json::json!({"max_rows":1,"max_depth":1,
        "max_expansions":10,"max_edge_expansions":0,"max_elapsed_ms":100,"max_text_bytes":1024,"detach_edge_limit":0}))
        .unwrap();
    assert_eq!(small.max_nodes, workspace.max_nodes);
    assert_eq!(small.max_edges, workspace.max_edges);
    assert_eq!(small.max_edge_expansions, 0);
    assert_eq!(workspace, before);
    for raw in [
        "[]",
        "null",
        "{\"max_rows\":8}",
        "{\"max_depth\":4}",
        "{\"max_expansions\":101}",
        "{\"max_edge_expansions\":21}",
        "{\"max_elapsed_ms\":201}",
        "{\"max_elapsed_ms\":0}",
        "{\"max_elapsed_ms\":-1}",
        "{\"max_elapsed_ms\":1.0}",
        "{\"max_edge_expansions\":-1}",
        "{\"max_edge_expansions\":1.0}",
        "{\"max_text_bytes\":4097}",
        "{\"detach_edge_limit\":3}",
        "{\"max_nodes\":1}",
        "{\"max_rows\":1.0}",
        "{\"max_rows\":-1}",
        "{\"max_rows\":\"1\"}",
        "{\"max_rows\":0}",
        "{\"max_rows\":true}",
        "{\"max_depth\":18446744073709551616}",
    ] {
        assert!(
            workspace
                .with_request_budgets(&serde_json::from_str(raw).unwrap())
                .is_err(),
            "{raw}"
        );
        assert_eq!(workspace, before);
    }
    assert_eq!(
        workspace
            .with_request_budgets(&serde_json::json!({}))
            .unwrap(),
        workspace
    );
    let invalid = Limits {
        max_depth: 17,
        ..Limits::default()
    };
    assert!(invalid.validate_workspace().is_err());
}

#[test]
fn tighter_work_and_row_caps_fail_staged_writes_without_partial_graph_changes() {
    let params = BTreeMap::new();
    let mut graph = Graph::new();
    execute(
        &mut graph,
        &parse("CREATE (a:Hub),(b:Leaf),(a)-[:LINK]->(b)").unwrap(),
        &params,
        &Limits::default(),
    )
    .unwrap();
    let saved = graph.clone();
    for (query, budgets) in [
        (
            "CREATE (:Transient) WITH 1 AS v UNWIND [1,2,3] AS i RETURN i",
            serde_json::json!({"max_rows":2}),
        ),
        (
            "CREATE (:Transient) WITH 1 AS v UNWIND [1,2,3] AS i RETURN i",
            serde_json::json!({"max_expansions":2}),
        ),
        (
            "MATCH (a:Hub) DETACH DELETE a",
            serde_json::json!({"detach_edge_limit":0}),
        ),
    ] {
        let limits = Limits::default().with_request_budgets(&budgets).unwrap();
        assert!(execute(&mut graph, &parse(query).unwrap(), &params, &limits).is_err());
        assert_eq!(graph, saved);
    }
    let low = Limits::default()
        .with_request_budgets(&serde_json::json!({"max_depth":1}))
        .unwrap();
    assert!(execute(
        &mut graph,
        &parse("MATCH (n)-[*1..2]->(m) RETURN m").unwrap(),
        &params,
        &low
    )
    .is_err());
    let empty_only = Limits {
        max_nodes: 0,
        max_edges: 0,
        ..Limits::default()
    };
    empty_only.validate_workspace().unwrap();
    assert!(execute(
        &mut Graph::new(),
        &parse("CREATE (:Denied)").unwrap(),
        &params,
        &empty_only
    )
    .is_err());
}
