use bicdb_graph::{
    access::AccessEntries,
    execute, parse,
    property_index::{EntityKind, PropertyIndex},
    storage::{StorageImage, StoragePatch},
    Graph, Limits, QueryResult, Value,
};
use std::collections::{BTreeMap, BTreeSet};

fn run(graph: &mut Graph, query: &str) -> QueryResult {
    execute(
        graph,
        &parse(query).unwrap(),
        &BTreeMap::new(),
        &Limits::default(),
    )
    .unwrap()
}
fn seed(graph: &mut Graph, count: u64) {
    for key in 1..=count {
        graph
            .add_node(
                BTreeSet::from(["N".into()]),
                BTreeMap::from([("key".into(), serde_json::json!(key))]),
            )
            .unwrap();
    }
}
fn apply(mut rows: BTreeMap<u64, Vec<u8>>, patch: &StoragePatch) -> BTreeMap<u64, Vec<u8>> {
    for key in &patch.removed {
        assert!(rows.remove(key).is_some());
    }
    for (key, value) in &patch.updated {
        assert!(rows.insert(*key, value.clone()).is_some());
    }
    for (key, value) in &patch.inserted {
        assert!(rows.insert(*key, value.clone()).is_none());
    }
    rows
}
fn image(graph: &Graph) -> StorageImage {
    let (empty, image) = StorageImage::decode(BTreeMap::new(), &Limits::default()).unwrap();
    StorageImage::decode(
        apply(
            BTreeMap::new(),
            &image.patch(&empty, graph, &Limits::default()).unwrap(),
        ),
        &Limits::default(),
    )
    .unwrap()
    .1
}
fn index(unique: bool) -> PropertyIndex {
    PropertyIndex {
        entity: EntityKind::Node,
        label: Some("N".into()),
        fields: vec![vec!["key".into()]],
        unique,
    }
}

#[test]
fn first_write_originals_span_calls_and_discard_net_noops() {
    let mut graph = Graph::new();
    seed(&mut graph, 2000);
    let original = graph.nodes()[&1].clone();
    let result = run(&mut graph, "MATCH (n:N {key:1}) SET n.key=3001 WITH n CALL (n) { SET n.key=3002 RETURN n AS m } SET m:Extra REMOVE m:Extra RETURN m.key");
    assert_eq!(result.changes.nodes().len(), 1);
    assert!(result.changes.edges().is_empty());
    assert_eq!(result.changes.nodes()[&1], Some(original));
    assert_eq!(result.rows[0][0], Value::integer(3002));
    let result = run(
        &mut graph,
        "MATCH (n:N {key:3002}) SET n.key=3003,n:Transient SET n.key=3002 REMOVE n:Transient",
    );
    assert!(result.mutations > 0);
    assert!(
        result.changes.is_empty(),
        "restored values must not cause source/index events"
    );
    assert!(run(&mut graph, "MATCH (n) RETURN count(n)")
        .changes
        .is_empty());
}

#[test]
fn error_restores_entities_allocator_and_existing_empty_directory_buckets() {
    let mut graph = Graph::new();
    run(
        &mut graph,
        "CREATE (a:N {key:1}),(b:N {key:2}),(z:Empty),(a)-[:R {key:1}]->(b),(a)-[:LOOP]->(a)",
    );
    run(&mut graph, "MATCH (z:Empty) DELETE z");
    let before = graph.clone();
    for query in [
        "MATCH (a:N {key:1}) SET a.key=9,a:New CREATE (c:Fresh),(c)-[:TMP]->(a) WITH a DETACH DELETE a RETURN 1/0 AS x",
        "MATCH (a:N {key:1}) WITH a CALL (a) { SET a.key=9 REMOVE a:N RETURN a AS m } CREATE (n:Fresh) RETURN 1/0 AS x",
        "MATCH ()-[r:R]->() SET r.key=9 DELETE r CREATE (:Fresh) RETURN 1/0 AS x",
    ] {
        let err = execute(&mut graph, &parse(query).unwrap(), &BTreeMap::new(), &Limits::default()).unwrap_err();
        assert!(err.to_string().contains("除以零"), "{err}");
        assert_eq!(graph, before, "{query}");
    }
    // A successful query after rollback must start a fresh journal/allocator.
    let result = run(&mut graph, "CREATE (n:Fresh) RETURN id(n) AS id");
    assert_eq!(
        result.rows[0][0],
        Value::integer(before.allocator_high_water())
    );
    assert_eq!(result.changes.nodes().len(), 1);
}

#[test]
fn delta_record_patches_equal_full_diff_for_growth_shrink_detach_and_transients() {
    let mut graph = Graph::new();
    seed(&mut graph, 50);
    run(
        &mut graph,
        "MATCH (a:N {key:1}),(b:N {key:2}) CREATE (a)-[:R {key:1}]->(b),(a)-[:R {key:2}]->(a)",
    );
    let mut stored = image(&graph);
    for query in [
        "MATCH (n:N {key:3}) SET n.body=$body",
        "MATCH (n:N {key:3}) SET n.body='small'",
        "MATCH ()-[r:R {key:1}]->() SET r.body=$body",
        "MATCH (n:N {key:4}) SET n:Extra REMOVE n:Extra",
        "CREATE (t:Transient) DELETE t",
        "MATCH (n:N {key:1}) DETACH DELETE n",
    ] {
        let before = graph.clone();
        let params = BTreeMap::from([("body".into(), Value::String("数据库".repeat(5000)))]);
        let result = execute(
            &mut graph,
            &parse(query).unwrap(),
            &params,
            &Limits::default(),
        )
        .unwrap();
        let delta = stored
            .patch_changes(&result.changes, &graph, &Limits::default())
            .unwrap();
        let full = stored.patch(&before, &graph, &Limits::default()).unwrap();
        assert_eq!(delta.inserted, full.inserted, "{query}");
        assert_eq!(delta.updated, full.updated, "{query}");
        assert_eq!(delta.removed, full.removed, "{query}");
        assert_eq!(delta.entities_changed, full.entities_changed, "{query}");
        let (loaded, new) =
            StorageImage::decode(apply(stored.rows().clone(), &delta), &Limits::default()).unwrap();
        assert_eq!(loaded.to_bytes().unwrap(), graph.to_bytes().unwrap());
        stored = new;
    }
}

#[test]
fn delta_index_planning_is_bounded_by_changed_entities_and_checks_unchanged_unique_keys() {
    let mut graph = Graph::new();
    seed(&mut graph, 2000);
    let definition = index(true);
    assert!(definition.entries_with_budget(&graph, 128).is_err());
    assert!(AccessEntries::from_nodes(&graph, 128).is_err());
    let result = run(&mut graph, "MATCH (n:N {key:1}) SET n.key=3001");
    let (old, new) = definition
        .changed_entries(&result.changes, &graph, 128)
        .unwrap();
    assert_eq!((old.len(), new.len()), (1, 1));
    let (old, new) = AccessEntries::changed_entries(&result.changes, &graph, 128, false).unwrap();
    assert_eq!((old.nodes.len(), new.nodes.len()), (2, 2));
    assert!(old.outgoing.is_empty() && new.incoming.is_empty());
    let result = run(&mut graph, "MATCH (n:N {key:3001}) SET n.key=2");
    let error = definition
        .changed_entries(&result.changes, &graph, 128)
        .unwrap_err();
    assert!(error.to_string().contains("unique"));
    assert!(definition
        .changed_entries(&result.changes, &graph, 1)
        .unwrap_err()
        .to_string()
        .contains("budget"));
}

#[test]
fn final_unique_keys_allow_swaps_and_nulls_but_reject_changed_collisions() {
    let mut graph = Graph::new();
    run(&mut graph, "CREATE (:N {key:1}),(:N {key:2}),(:N)");
    let definition = index(true);
    let result = run(
        &mut graph,
        "MATCH (a:N {key:1}),(b:N {key:2}) SET a.key=2,b.key=1",
    );
    let (old, new) = definition
        .changed_entries(&result.changes, &graph, 1024)
        .unwrap();
    assert_eq!(old.len(), 2);
    assert_eq!(new.iter().map(|e| e.1).collect::<BTreeSet<_>>().len(), 2);
    let result = run(&mut graph, "MATCH (n:N) REMOVE n.key");
    assert_eq!(
        definition
            .changed_entries(&result.changes, &graph, 1024)
            .unwrap()
            .1
            .len(),
        2
    );
    let result = run(&mut graph, "MATCH (n:N) SET n.key=9");
    assert!(definition
        .changed_entries(&result.changes, &graph, 1024)
        .unwrap_err()
        .to_string()
        .contains("unique"));
}

#[test]
fn bounded_storage_size_matches_complete_json_at_exact_budget_boundary() {
    let mut graph = Graph::new();
    for query in [
        "CREATE (a:N {name:'数据库',escaped:'a\\\"b'}),(b:M),(a)-[:R {name:'한국어'}]->(b)",
        "MATCH (a:N) REMOVE a:N SET a:新标签",
        "MATCH (a:N) DELETE a",
    ] {
        run(&mut graph, query);
        let bytes = graph.to_bytes().unwrap().len();
        assert_eq!(graph.storage_size(bytes).unwrap(), bytes);
        assert!(graph.storage_size(bytes - 1).is_err());
    }
    let before = graph.clone();
    let limit = graph.to_bytes().unwrap().len();
    let error = execute(
        &mut graph,
        &parse("CREATE (:Overflow {name:'large'})").unwrap(),
        &BTreeMap::new(),
        &Limits {
            max_text_bytes: limit,
            ..Limits::default()
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("budget"));
    assert_eq!(graph, before);
}

#[test]
fn relationship_json_composite_and_label_changes_match_full_index_differences() {
    let mut graph = Graph::new();
    run(&mut graph, "CREATE (a:N {key:1}),(b:N {key:2}),(a)-[:R {scope:'d',config:{key:'x'}}]->(b),(a)-[:R {scope:'d',config:{key:'y'}}]->(a)");
    let before = graph.clone();
    let result = run(
        &mut graph,
        "MATCH ()-[r:R {scope:'d'}]->() WHERE r.config.key='x' SET r.config={key:'z'}",
    );
    let definition = PropertyIndex {
        entity: EntityKind::Relationship,
        label: Some("R".into()),
        fields: vec![vec!["scope".into()], vec!["config".into(), "key".into()]],
        unique: true,
    };
    let (old, new) = definition
        .changed_entries(&result.changes, &graph, 1024)
        .unwrap();
    assert_eq!((old.len(), new.len()), (1, 1));
    let original: BTreeSet<_> = definition.entries(&before).unwrap().into_iter().collect();
    let current: BTreeSet<_> = definition.entries(&graph).unwrap().into_iter().collect();
    assert_eq!(
        new.into_iter()
            .filter(|entry| !old.contains(entry))
            .collect::<BTreeSet<_>>(),
        current.difference(&original).cloned().collect()
    );
    let result = run(&mut graph, "MATCH ()-[r:R]->() SET r.config={key:'z'}");
    assert!(definition
        .changed_entries(&result.changes, &graph, 1024)
        .unwrap_err()
        .to_string()
        .contains("unique"));
    let result = run(
        &mut graph,
        "MATCH (a:N {key:1}) REMOVE a:N CREATE (:N {key:1})",
    );
    let (old, new) = index(true)
        .changed_entries(&result.changes, &graph, 1024)
        .unwrap();
    assert_eq!((old.len(), new.len()), (1, 1));
    assert_ne!(
        old[0].1, new[0].1,
        "removed-label entity must release its unique key"
    );
}
