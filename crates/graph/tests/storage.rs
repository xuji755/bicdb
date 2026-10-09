use bicdb_common::sha256::Sha256;
use bicdb_graph::{
    execute, parse,
    storage::{StorageImage, StoragePatch},
    Graph, Limits,
};
use std::collections::BTreeMap;

fn change(g: &mut Graph, sql: &str) {
    execute(
        g,
        &parse(sql).unwrap(),
        &BTreeMap::new(),
        &Limits::default(),
    )
    .unwrap();
}
fn apply(mut rows: BTreeMap<u64, Vec<u8>>, patch: &StoragePatch) -> BTreeMap<u64, Vec<u8>> {
    for key in &patch.removed {
        assert!(rows.remove(key).is_some());
    }
    for (key, data) in &patch.updated {
        assert!(rows.insert(*key, data.clone()).is_some());
    }
    for (key, data) in &patch.inserted {
        assert!(rows.insert(*key, data.clone()).is_none());
    }
    rows
}
fn image(g: &Graph) -> StorageImage {
    let (_, empty) = StorageImage::decode(BTreeMap::new(), &Limits::default()).unwrap();
    let rows = apply(
        BTreeMap::new(),
        &empty.patch(&Graph::new(), g, &Limits::default()).unwrap(),
    );
    StorageImage::decode(rows, &Limits::default()).unwrap().1
}
#[test]
fn records_write_only_changed_entities_and_preserve_unrelated_rows() {
    let mut before = Graph::new();
    change(
        &mut before,
        "UNWIND [1,2,3,4,5,6,7,8,9,10] AS x CREATE (n:Entity {name:'anchor',number:x})",
    );
    change(
        &mut before,
        "MATCH (a:Entity {number:1}),(b:Entity {number:2}) CREATE (a)-[:R {weight:2}]->(b)",
    );
    let initial = image(&before);
    let mut after = before.clone();
    change(
        &mut after,
        "MATCH (n:Entity {number:3}) SET n.name='changed'",
    );
    let patch = initial.patch(&before, &after, &Limits::default()).unwrap();
    assert_eq!(patch.entities_changed, 1);
    assert_eq!(patch.updated.len(), 2); // one record digest + one JSON chunk
    assert!(patch.inserted.is_empty() && patch.removed.is_empty());
    let rows = apply(initial.rows().clone(), &patch);
    let (loaded, updated) = StorageImage::decode(rows, &Limits::default()).unwrap();
    assert_eq!(loaded.to_bytes().unwrap(), after.to_bytes().unwrap());
    assert!(updated
        .patch(&loaded, &loaded, &Limits::default())
        .unwrap()
        .is_empty());
    assert_eq!(
        initial
            .rows()
            .iter()
            .filter(|(key, bytes)| updated.rows().get(key) == Some(bytes))
            .count(),
        initial.rows().len() - 2
    );
    let before = after.clone();
    change(&mut after, "MATCH (n:Entity {number:1}) DETACH DELETE n");
    let patch = updated.patch(&before, &after, &Limits::default()).unwrap();
    assert_eq!(patch.entities_changed, 2); // detached relationship is independent
    let (loaded, _) =
        StorageImage::decode(apply(updated.rows().clone(), &patch), &Limits::default()).unwrap();
    assert_eq!(loaded.to_bytes().unwrap(), after.to_bytes().unwrap());
}
#[test]
fn multichunk_entity_growth_shrink_and_checksum_failures() {
    let mut before = Graph::new();
    change(&mut before, "CREATE (n:Entity {name:'x'})");
    let initial = image(&before);
    let mut after = before.clone();
    let text = "数据库".repeat(5000);
    let params = BTreeMap::from([("text".into(), bicdb_graph::Value::String(text))]);
    execute(
        &mut after,
        &parse("MATCH (n) SET n.text=$text").unwrap(),
        &params,
        &Limits::default(),
    )
    .unwrap();
    let patch = initial.patch(&before, &after, &Limits::default()).unwrap();
    assert!(patch.inserted.len() > 8);
    let grown = apply(initial.rows().clone(), &patch);
    let (_, stored) = StorageImage::decode(grown.clone(), &Limits::default()).unwrap();
    let mut corrupt = grown.clone();
    let key = *patch.inserted.keys().next().unwrap();
    corrupt.get_mut(&key).unwrap()[0] ^= 1;
    assert!(StorageImage::decode(corrupt, &Limits::default()).is_err());
    let mut missing = grown.clone();
    missing.remove(&key);
    assert!(StorageImage::decode(missing, &Limits::default()).is_err());
    let before = after.clone();
    change(&mut after, "MATCH (n) REMOVE n.text");
    let shrink = stored.patch(&before, &after, &Limits::default()).unwrap();
    assert!(shrink.removed.len() > 8);
    let (loaded, _) = StorageImage::decode(apply(grown, &shrink), &Limits::default()).unwrap();
    assert_eq!(loaded.to_bytes().unwrap(), after.to_bytes().unwrap());
}
#[test]
fn legacy_snapshot_read_and_transactional_patch_migration() {
    let mut before = Graph::new();
    change(&mut before, "CREATE (a:A {name:'old'}),(b:B),(a)-[:R]->(b)");
    let bytes = before.to_bytes().unwrap();
    let mut sha = Sha256::new();
    sha.update(&bytes);
    let mut rows = BTreeMap::from([(0, sha.finalize().to_vec())]);
    rows.extend(
        bytes
            .chunks(4096)
            .enumerate()
            .map(|(i, bytes)| (i as u64 + 1, bytes.to_vec())),
    );
    let (loaded, legacy) = StorageImage::decode(rows.clone(), &Limits::default()).unwrap();
    assert_eq!(loaded.to_bytes().unwrap(), bytes);
    assert!(legacy.is_legacy());
    let mut after = loaded.clone();
    change(&mut after, "MATCH (a:A) SET a.name='new'");
    let patch = legacy.patch(&loaded, &after, &Limits::default()).unwrap();
    assert!(patch.migrated);
    let migrated = apply(rows.clone(), &patch);
    let (actual, image) = StorageImage::decode(migrated, &Limits::default()).unwrap();
    assert!(!image.is_legacy());
    assert_eq!(actual.to_bytes().unwrap(), after.to_bytes().unwrap());
    assert_eq!(
        StorageImage::decode(rows, &Limits::default())
            .unwrap()
            .0
            .to_bytes()
            .unwrap(),
        bytes
    );
}
#[test]
fn manifest_and_identity_corruption_and_budget_are_rejected() {
    let mut graph = Graph::new();
    change(&mut graph, "CREATE (n:A {name:'small'})");
    let image = image(&graph);
    let mut invalid = image.rows().clone();
    let mut meta: serde_json::Value = serde_json::from_slice(&invalid[&0]).unwrap();
    meta["nodes"] = serde_json::json!(2);
    invalid.insert(0, serde_json::to_vec(&meta).unwrap());
    assert!(StorageImage::decode(invalid, &Limits::default()).is_err());
    let mut invalid = image.rows().clone();
    let key = *invalid.keys().nth(1).unwrap();
    let digest = invalid.remove(&key).unwrap();
    invalid.insert(key + 4096, digest);
    assert!(StorageImage::decode(invalid, &Limits::default()).is_err());
    let small = Limits {
        max_text_bytes: 100,
        ..Limits::default()
    };
    assert!(StorageImage::decode(image.rows().clone(), &small).is_err());
    assert!(image.patch(&graph, &graph, &small).is_err());
}

#[test]
fn graph_above_old_eight_mib_limit_keeps_single_entity_writes_small() {
    let mut before = Graph::new();
    let text = "x".repeat(100_000);
    for i in 0..100 {
        before
            .add_node(
                std::collections::BTreeSet::from(["Document".into()]),
                BTreeMap::from([
                    ("text".into(), serde_json::json!(text)),
                    ("number".into(), serde_json::json!(i)),
                ]),
            )
            .unwrap();
    }
    let initial = image(&before);
    let stored_bytes: usize = initial.rows().values().map(Vec::len).sum();
    assert!(stored_bytes > 8 * 1024 * 1024);
    let old_limit = Limits {
        max_text_bytes: 8 * 1024 * 1024,
        ..Limits::default()
    };
    assert!(StorageImage::decode(initial.rows().clone(), &old_limit).is_err());
    let mut after = before.clone();
    change(
        &mut after,
        "MATCH (n:Document {number:42}) SET n.note='one record'",
    );
    let patch = initial.patch(&before, &after, &Limits::default()).unwrap();
    assert_eq!(patch.entities_changed, 1);
    assert!(patch.rows_changed() < initial.rows().len() / 50);
    let (loaded, _) =
        StorageImage::decode(apply(initial.rows().clone(), &patch), &Limits::default()).unwrap();
    assert_eq!(loaded.nodes().len(), 100);
    assert_eq!(loaded.to_bytes().unwrap(), after.to_bytes().unwrap());
}

#[test]
fn exhausted_48_bit_allocator_remains_readable_without_reusing_ids() {
    let mut graph = Graph::new();
    let last = (1u64 << 48) - 1;
    graph.reserve_ids(last, last).unwrap();
    assert_eq!(
        graph
            .add_node(Default::default(), Default::default())
            .unwrap(),
        last
    );
    let (mut restored, _) =
        StorageImage::decode(image(&graph).rows().clone(), &Limits::default()).unwrap();
    assert!(restored
        .add_node(Default::default(), Default::default())
        .is_err());
    assert_eq!(
        Graph::from_bytes(&graph.to_bytes().unwrap(), &Limits::default())
            .unwrap()
            .nodes()
            .len(),
        1
    );
}

#[test]
fn snapshot_entity_read_validates_only_requested_chunks_and_manifest() {
    let mut g = Graph::new();
    change(
        &mut g,
        "CREATE (a:Entity {name:'a'}),(b:Entity {name:'b'}),(a)-[:LINK]->(b)",
    );
    let image = image(&g);
    let rows = image.rows();
    let cache =
        bicdb_graph::storage::snapshot_cache(rows.get(&0).map(Vec::as_slice), &Limits::default())
            .unwrap()
            .unwrap();
    assert!(cache.nodes().is_empty());
    assert_eq!(cache.allocator_high_water(), g.allocator_high_water());
    let id = *g.nodes().keys().next().unwrap();
    let (lo, hi) = bicdb_graph::storage::entity_range(1, id).unwrap();
    let selected: BTreeMap<_, _> = rows.range(lo..=hi).map(|(k, v)| (*k, v.clone())).collect();
    assert_eq!(
        bicdb_graph::storage::decode_node(id, &selected, &Limits::default())
            .unwrap()
            .as_ref(),
        g.nodes().get(&id)
    );
    let eid = *g.edges().keys().next().unwrap();
    let (lo, hi) = bicdb_graph::storage::entity_range(2, eid).unwrap();
    let edges: BTreeMap<_, _> = rows.range(lo..=hi).map(|(k, v)| (*k, v.clone())).collect();
    assert_eq!(
        bicdb_graph::storage::decode_edge(eid, &edges, &Limits::default())
            .unwrap()
            .as_ref(),
        g.edges().get(&eid)
    );
    assert!(
        bicdb_graph::storage::decode_node(999, &BTreeMap::new(), &Limits::default())
            .unwrap()
            .is_none()
    );
    let mut corrupt = selected.clone();
    corrupt.values_mut().last().unwrap()[0] ^= 1;
    assert!(bicdb_graph::storage::decode_node(id, &corrupt, &Limits::default()).is_err());
    assert!(bicdb_graph::storage::entity_range(1, 0).is_err());
    assert!(bicdb_graph::storage::entity_range(3, id).is_err());
    assert!(
        bicdb_graph::storage::snapshot_cache(Some(b"bad manifest"), &Limits::default()).is_err()
    );
    assert!(
        bicdb_graph::storage::snapshot_cache(Some(&[0; 32]), &Limits::default())
            .unwrap()
            .is_none()
    );
}

#[test]
fn adjacency_migration_stages_only_nodes_and_keeps_legacy_authority_until_publication() {
    let mut g = Graph::new();
    change(
        &mut g,
        "CREATE (a:A {name:'a'}),(b:B {name:'b'}),(a)-[:R {text:'edgeword'}]->(b)",
    );
    let modern = image(&g);
    let json = g.to_bytes().unwrap();
    let mut sha = Sha256::new();
    sha.update(&json);
    let mut legacy = BTreeMap::from([(0, sha.finalize().to_vec())]);
    legacy.extend(
        json.chunks(4096)
            .enumerate()
            .map(|(i, data)| (i as u64 + 1, data.to_vec())),
    );
    for source in [modern.rows().clone(), legacy] {
        let (g, image) = StorageImage::decode(source.clone(), &Limits::default()).unwrap();
        let patch = image
            .adjacency_migration_patch(&g, &Limits::default())
            .unwrap();
        assert!(patch
            .inserted
            .keys()
            .chain(patch.updated.keys())
            .all(|key| key >> 60 == 1));
        let staged = apply(source.clone(), &patch);
        assert_eq!(staged[&0], source[&0]);
        assert!(staged.keys().all(|key| *key == 0 || key >> 60 == 1));
        for id in g.nodes().keys() {
            let (lower, upper) = bicdb_graph::storage::entity_range(1, *id).unwrap();
            let selected = staged
                .range(lower..=upper)
                .map(|(key, data)| (*key, data.clone()))
                .collect();
            assert_eq!(
                bicdb_graph::storage::decode_node(*id, &selected, &Limits::default())
                    .unwrap()
                    .unwrap(),
                g.nodes()[id]
            );
        }
        assert!(image
            .adjacency_migration_patch(
                &g,
                &Limits {
                    max_nodes: 1,
                    ..Default::default()
                }
            )
            .is_err());
        assert!(image
            .adjacency_migration_patch(
                &g,
                &Limits {
                    max_text_bytes: 1,
                    ..Default::default()
                }
            )
            .is_err());
    }
    let nodes: BTreeMap<_, _> = modern
        .rows()
        .iter()
        .filter(|(key, _)| *key >> 60 == 1)
        .map(|(key, data)| (*key, data.clone()))
        .collect();
    let (_, native) = StorageImage::from_adjacency(
        nodes,
        g.edges().values().cloned(),
        modern.rows()[&0].clone(),
        &Limits::default(),
    )
    .unwrap();
    assert!(native
        .adjacency_migration_patch(&g, &Limits::default())
        .is_err());
}

fn logical_header(graph: &Graph) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({"format":"bicdb-graph-records-v2",
        "next_id":graph.allocator_high_water(),"nodes":graph.nodes().len(),"edges":graph.edges().len()})).unwrap()
}
fn node_records(graph: &Graph) -> BTreeMap<u64, Vec<u8>> {
    image(graph)
        .rows()
        .iter()
        .filter(|(key, _)| *key >> 60 == 1)
        .map(|(key, data)| (*key, data.clone()))
        .collect()
}

#[test]
fn native_planning_keeps_only_node_rows_and_enforces_total_edge_delta_budget() {
    let mut graph = Graph::new();
    change(&mut graph, "CREATE (a:N),(b:N)");
    let nodes = node_records(&graph);
    change(
        &mut graph,
        "MATCH (a),(b) WHERE id(a)=1 AND id(b)=2 CREATE (a)-[:R {body:'old'}]->(b)",
    );
    let (mut graph, native) = StorageImage::from_adjacency(
        nodes,
        graph.edges().values().cloned(),
        logical_header(&graph),
        &Limits::default(),
    )
    .unwrap();
    assert!(native.rows().keys().all(|key| *key == 0 || key >> 60 == 1));
    assert!(
        StorageImage::decode(native.rows().clone(), &Limits::default()).is_err(),
        "node rows alone are not a complete native authority"
    );
    let result = execute(
        &mut graph,
        &parse("MATCH ()-[r]->() SET r.body=$body").unwrap(),
        &BTreeMap::from([(
            "body".into(),
            bicdb_graph::Value::String("x".repeat(100000)),
        )]),
        &Limits::default(),
    )
    .unwrap();
    let patch = native
        .patch_changes(&result.changes, &graph, &Limits::default())
        .unwrap();
    assert_eq!(patch.entities_changed, 1);
    assert!(
        patch.is_empty(),
        "edge-only updates require an independent native edge plan"
    );
    assert!(
        native
            .patch_changes(
                &result.changes,
                &graph,
                &Limits {
                    max_text_bytes: 10000,
                    ..Default::default()
                }
            )
            .is_err(),
        "edge payload must remain in the whole-corpus budget"
    );
    let (mut graph, grown) = StorageImage::from_adjacency(
        native
            .rows()
            .iter()
            .filter(|(key, _)| **key != 0)
            .map(|(key, data)| (*key, data.clone()))
            .collect(),
        graph.edges().values().cloned(),
        logical_header(&graph),
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(
        patch.after_record_bytes,
        Some(
            grown
                .native_record_bytes(&graph, &Limits::default())
                .unwrap()
        )
    );
    let result = execute(
        &mut graph,
        &parse("MATCH ()-[r]->() SET r.body='small'").unwrap(),
        &BTreeMap::new(),
        &Limits::default(),
    )
    .unwrap();
    assert!(
        grown
            .patch_changes(
                &result.changes,
                &graph,
                &Limits {
                    max_text_bytes: 10000,
                    ..Default::default()
                }
            )
            .is_ok(),
        "shrinking must remove the original edge contribution"
    );
}

#[test]
fn native_planning_validates_nodes_endpoints_counts_identity_and_payload_limits() {
    let mut graph = Graph::new();
    change(&mut graph, "CREATE (a:N),(b:N)");
    let nodes = node_records(&graph);
    change(
        &mut graph,
        "MATCH (a),(b) WHERE id(a)=1 AND id(b)=2 CREATE (a)-[:R]->(b)",
    );
    let edges = graph.edges().values().cloned().collect::<Vec<_>>();
    let header = logical_header(&graph);
    let verify =
        |rows, edges, header| StorageImage::from_adjacency(rows, edges, header, &Limits::default());
    assert!(verify(nodes.clone(), vec![], header.clone()).is_err());
    assert!(verify(
        nodes.clone(),
        vec![edges[0].clone(), edges[0].clone()],
        header.clone()
    )
    .is_err());
    let mut invalid = edges[0].clone();
    invalid.target = 999;
    assert!(verify(nodes.clone(), vec![invalid], header.clone()).is_err());
    let mut invalid = edges[0].clone();
    invalid.id = graph.allocator_high_water();
    assert!(verify(nodes.clone(), vec![invalid], header.clone()).is_err());
    let mut invalid = edges[0].clone();
    invalid.label.clear();
    assert!(verify(nodes.clone(), vec![invalid], header.clone()).is_err());
    let mut corrupt = nodes.clone();
    *corrupt.values_mut().next().unwrap() = vec![0; 32];
    assert!(verify(corrupt, edges.clone(), header.clone()).is_err());
    let mut orphan = nodes.clone();
    orphan.remove(&(1 << 60 | 1 << 10));
    assert!(verify(orphan, edges.clone(), header.clone()).is_err());
    let mut invalid = edges[0].clone();
    invalid.label = "L".repeat(4096 * 1023);
    assert!(verify(nodes, vec![invalid], header)
        .unwrap_err()
        .to_string()
        .contains("chunk budget"));
}

#[test]
fn native_planning_accepts_physical_document_boundary_without_legacy_edge_rechunking() {
    let mut graph = Graph::new();
    change(&mut graph, "CREATE (a:N),(b:N)");
    let nodes = node_records(&graph);
    let properties = BTreeMap::from([("body".into(), serde_json::json!(""))]);
    let prototype = bicdb_graph::Edge {
        id: 3,
        source: 1,
        target: 2,
        label: "R".into(),
        properties: properties.clone(),
    };
    let overhead =
        bicdb_graph::adjacency_record::payload_size(&prototype, &Limits::default()).unwrap() - 1;
    let id = graph
        .add_edge(1, 2, "L".repeat(4096 * 1023 - overhead), properties)
        .unwrap();
    assert_eq!(
        bicdb_graph::adjacency_record::payload_size(&graph.edges()[&id], &Limits::default())
            .unwrap(),
        4096 * 1023
    );
    assert!(
        image_if_possible(&graph).is_err(),
        "legacy metadata/chunks exceed the old per-record cap at this valid native boundary"
    );
    let (loaded, native) = StorageImage::from_adjacency(
        nodes,
        graph.edges().values().cloned(),
        logical_header(&graph),
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(loaded.to_bytes().unwrap(), graph.to_bytes().unwrap());
    assert!(native.rows().keys().all(|key| *key == 0 || key >> 60 == 1));
    let (empty, empty_native) = StorageImage::empty_adjacency(&Limits::default()).unwrap();
    let patch = empty_native
        .patch(&empty, &graph, &Limits::default())
        .unwrap();
    assert!(patch.inserted.keys().all(|key| *key == 0 || key >> 60 == 1));
}
fn image_if_possible(graph: &Graph) -> Result<StoragePatch, bicdb_graph::Error> {
    let (empty, image) = StorageImage::decode(BTreeMap::new(), &Limits::default())?;
    image.patch(&empty, graph, &Limits::default())
}
