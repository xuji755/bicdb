use bicdb_graph::{
    execute, parse,
    property_index::{EntityKind, PropertyIndex},
    Error, Graph, Limits, QueryResult,
};
use std::collections::{BTreeMap, BTreeSet};

fn run(graph: &mut Graph, text: &str) -> QueryResult {
    execute(
        graph,
        &parse(text).unwrap(),
        &BTreeMap::new(),
        &Limits::default(),
    )
    .unwrap()
}
fn nodes() -> PropertyIndex {
    PropertyIndex {
        entity: EntityKind::Node,
        label: Some("N".into()),
        fields: vec![vec!["key".into()]],
        unique: true,
    }
}

#[test]
fn complete_unique_probe_rechecks_stale_candidates_and_only_probes_changed_keys() {
    let mut graph = Graph::new();
    for key in 1..=2000 {
        graph
            .add_node(
                BTreeSet::from(["N".into()]),
                BTreeMap::from([("key".into(), serde_json::json!(key))]),
            )
            .unwrap();
    }
    let definition = nodes();
    let persisted = definition.entries(&graph).unwrap();
    let delta = run(&mut graph, "MATCH (n:N {key:1}) SET n.key=3001");
    let mut calls = 0;
    let (old, new) = definition
        .changed_entries_with_probe(&delta.changes, &graph, 128, |index, key| {
            assert_eq!(index, &definition);
            calls += 1;
            let mut ids: Vec<_> = persisted
                .iter()
                .filter(|entry| entry.0 == key)
                .map(|entry| entry.1)
                .collect();
            // Physical history can include an own changed identity, another source
            // now holding a different key, and an absent/deleted source.
            ids.extend([1, 2, 999_999]);
            Ok(Some(ids))
        })
        .unwrap();
    assert_eq!(calls, 1);
    assert_eq!((old.len(), new.len()), (1, 1));
    assert_eq!(
        definition
            .changed_entries(&delta.changes, &graph, 128)
            .unwrap(),
        (old, new)
    );
    let delta = run(&mut graph, "MATCH (n:N {key:3001}) SET n.key=2");
    assert!(definition
        .changed_entries_with_probe(&delta.changes, &graph, 128, |_, key| {
            Ok(Some(
                persisted
                    .iter()
                    .filter(|entry| entry.0 == key)
                    .map(|entry| entry.1)
                    .collect(),
            ))
        })
        .unwrap_err()
        .to_string()
        .contains("unique"));
}

#[test]
fn unique_probe_preserves_final_swaps_scope_release_and_relationship_composite_keys() {
    let mut graph = Graph::new();
    run(&mut graph, "CREATE (a:N {key:1}),(b:N {key:2}),(c:Other {key:1}),(a)-[:R {scope:'d',config:{key:'x'}}]->(b),(a)-[:R {scope:'d',config:{key:'y'}}]->(c)");
    let definition = nodes();
    let persisted = definition.entries(&graph).unwrap();
    let delta = run(
        &mut graph,
        "MATCH (a:N {key:1}),(b:N {key:2}) SET a.key=2,b.key=1",
    );
    definition
        .changed_entries_with_probe(&delta.changes, &graph, 1024, |_, key| {
            let mut ids: Vec<_> = persisted
                .iter()
                .filter(|entry| entry.0 == key)
                .map(|entry| entry.1)
                .collect();
            ids.push(3); // Historical label membership is rechecked.
            Ok(Some(ids))
        })
        .unwrap();
    let persisted = definition.entries(&graph).unwrap();
    let delta = run(
        &mut graph,
        "MATCH (a:N {key:1}) REMOVE a:N CREATE (:N {key:1})",
    );
    definition
        .changed_entries_with_probe(&delta.changes, &graph, 1024, |_, key| {
            Ok(Some(
                persisted
                    .iter()
                    .filter(|entry| entry.0 == key)
                    .map(|entry| entry.1)
                    .collect(),
            ))
        })
        .unwrap();
    let relationships = PropertyIndex {
        entity: EntityKind::Relationship,
        label: Some("R".into()),
        fields: vec![vec!["scope".into()], vec!["config".into(), "key".into()]],
        unique: true,
    };
    let persisted = relationships.entries(&graph).unwrap();
    let original = graph.clone();
    let delta = run(
        &mut graph,
        "MATCH ()-[r:R]->() WHERE r.config.key='x' SET r.config={key:'y'}",
    );
    assert!(relationships
        .changed_entries_with_probe(&delta.changes, &graph, 1024, |_, key| {
            Ok(Some(
                persisted
                    .iter()
                    .filter(|entry| entry.0 == key)
                    .map(|entry| entry.1)
                    .collect(),
            ))
        })
        .unwrap_err()
        .to_string()
        .contains("unique"));
    graph = original;
    let delta = run(&mut graph, "MATCH ()-[r:R]->() SET r.config={key:'z'}");
    assert!(relationships
        .changed_entries_with_probe(&delta.changes, &graph, 1024, |_, _| {
            panic!("changed duplicate keys must fail before any persistent probe")
        })
        .unwrap_err()
        .to_string()
        .contains("unique"));
}

#[test]
fn incomplete_error_invalid_identity_and_null_unique_probes_have_explicit_semantics() {
    let mut graph = Graph::new();
    run(&mut graph, "CREATE (:N {key:1}),(:N {key:2})");
    let definition = nodes();
    let original = graph.clone();
    let delta = run(&mut graph, "MATCH (n:N {key:1}) SET n.key=2");
    assert!(definition
        .changed_entries_with_probe(&delta.changes, &graph, 1024, |_, _| Ok(None))
        .unwrap_err()
        .to_string()
        .contains("unique"));
    let error = definition
        .changed_entries_with_probe(&delta.changes, &graph, 1024, |_, _| {
            Err(Error("incomplete probe budget".into()))
        })
        .unwrap_err();
    assert_eq!(error.to_string(), "incomplete probe budget");
    for id in [0, 1 << 48] {
        assert!(definition
            .changed_entries_with_probe(&delta.changes, &graph, 1024, |_, _| Ok(Some(vec![id])))
            .unwrap_err()
            .to_string()
            .contains("identity"));
    }
    graph = original;
    let delta = run(&mut graph, "MATCH (n:N) REMOVE n.key");
    let (_, entries) = definition
        .changed_entries_with_probe(&delta.changes, &graph, 1024, |_, _| {
            panic!("null-containing keys do not require uniqueness probes")
        })
        .unwrap();
    assert_eq!(entries.len(), 2);
}

#[test]
fn partial_unique_proof_resolves_uncached_sources_and_refuses_incomplete_candidates() {
    let mut cache = Graph::new();
    run(&mut cache, "CREATE (n:N {key:1})");
    let result = run(&mut cache, "MATCH (n:N) SET n.key=2");
    let definition = nodes();
    let error = definition
        .changed_entries_with_source(
            &result.changes,
            &cache,
            128,
            |_, _| Ok(Some(vec![2])),
            |_, id| {
                assert_eq!(id, 2);
                Ok(Some(BTreeMap::from([("key".into(), serde_json::json!(2))])))
            },
        )
        .unwrap_err();
    assert!(error.to_string().contains("unique graph property index"));
    assert!(definition
        .changed_entries_with_source(
            &result.changes,
            &cache,
            128,
            |_, _| Ok(None),
            |_, _| panic!("incomplete source proof must fail first")
        )
        .unwrap_err()
        .to_string()
        .contains("requires complete"));
    assert!(definition
        .changed_entries_with_source(
            &result.changes,
            &cache,
            128,
            |_, _| Ok(Some(vec![2])),
            |_, _| Err(Error("source read failed".into()))
        )
        .unwrap_err()
        .to_string()
        .contains("source read failed"));
    definition
        .changed_entries_with_source(
            &result.changes,
            &cache,
            128,
            |_, _| Ok(Some(vec![2])),
            |_, _| Ok(None),
        )
        .unwrap();
}
