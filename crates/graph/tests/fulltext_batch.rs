use bicdb_graph::fulltext::{Channel, Definition, Generation, PathPart, SearchOptions, TextLimits};
use bicdb_graph::property_index::EntityKind;
use bicdb_graph::{execute, parse, Graph, Limits};
use std::collections::{BTreeMap, BTreeSet};
fn definition() -> Definition {
    Definition {
        entity: EntityKind::Node,
        labels: vec!["Entity".into()],
        fields: vec![
            vec![PathPart::Key("name".into())],
            vec![PathPart::Key("summary".into())],
            vec![PathPart::Key("tags".into())],
        ],
    }
}
fn query(graph: &mut Graph, sql: &str) {
    execute(
        graph,
        &parse(sql).unwrap(),
        &BTreeMap::new(),
        &Limits::default(),
    )
    .unwrap();
}
fn selected(rows: &BTreeMap<u64, Vec<u8>>, ids: &BTreeSet<u64>) -> BTreeMap<u64, Vec<u8>> {
    rows.iter()
        .filter(|(k, _)| {
            **k <= 1023
                || **k >> 60 == 2
                || **k >> 60 == 3
                || ids.contains(&((**k & ((1 << 60) - 1)) >> 10))
        })
        .map(|(k, v)| (*k, v.clone()))
        .collect()
}
fn apply(
    durable: &mut BTreeMap<u64, Vec<u8>>,
    before: &BTreeMap<u64, Vec<u8>>,
    after: &BTreeMap<u64, Vec<u8>>,
) {
    for key in before.keys().filter(|k| !after.contains_key(k)) {
        durable.remove(key);
    }
    durable.extend(after.iter().map(|(k, v)| (*k, v.clone())));
}

#[test]
fn cold_native_batches_match_independent_full_build_across_updates_domains_and_removals() {
    let limits = TextLimits::default();
    let mut graph = Graph::new();
    query(&mut graph,"CREATE (:Entity {db_type:'d1',name:'alpha alpha',summary:'数据库缓冲池',tags:['io','tag']}),(:Entity {db_type:'d1',name:'beta'}),(:Entity {db_type:'d2',name:'alpha'})");
    for i in 0..250 {
        graph
            .add_node(
                BTreeSet::from(["Entity".into()]),
                BTreeMap::from([
                    ("db_type".into(), serde_json::json!("unrelated")),
                    ("name".into(), serde_json::json!(format!("word{i}"))),
                    ("summary".into(), serde_json::json!("unrelated ".repeat(30))),
                ]),
            )
            .unwrap();
    }
    let mut full = Generation::build(definition(), &graph, 1, 0, &limits).unwrap();
    let mut durable = full.native_rows(&limits).unwrap();
    let cases = [
        "MATCH (n {name:'alpha alpha'}) SET n.name='fresh fresh fresh',n.tags=['io','new']",
        "MATCH (n {name:'beta'}) REMOVE n:Entity",
        "MATCH (n {db_type:'d2'}) SET n.db_type='d1'",
        "CREATE (:Entity {db_type:'d3',name:'added'})",
        "MATCH (n {name:'fresh fresh fresh'}) DELETE n",
        "CREATE (:Entity {db_type:'d0',name:null,summary:7})",
    ];
    for (step, sql) in cases.iter().enumerate() {
        query(&mut graph, sql);
        let seq = step as u64 + 1;
        let (_, changed) = full.synchronize(&graph, seq, &limits).unwrap();
        let changes = changed
            .iter()
            .map(|id| {
                (
                    *id,
                    graph
                        .nodes()
                        .get(id)
                        .and_then(|n| definition().node_document(n, &limits).unwrap()),
                )
            })
            .collect();
        let rows = selected(&durable, &changed);
        let batch = Generation::apply_native_batch(&rows, changes, seq, &limits).unwrap();
        assert!(
            batch.documents_loaded <= 1,
            "unaffected 250 documents stay unopened"
        );
        let expected =
            Generation::build(definition(), &graph, full.generation() + 1, seq, &limits).unwrap();
        assert_eq!(
            batch.entries,
            expected
                .native_entries_for(&batch.changed, &limits)
                .unwrap()
        );
        apply(&mut durable, &batch.before, &batch.after);
        assert_eq!(
            durable,
            expected.native_rows(&limits).unwrap(),
            "full-build oracle: {sql}"
        );
        let restored = Generation::from_native_rows(&durable, &limits).unwrap();
        assert_eq!(restored.documents(), expected.documents());
        let opts = SearchOptions {
            domain: "d1".into(),
            fields: vec![],
            channel: Channel::Terms,
        };
        let revision = |id| {
            graph
                .nodes()
                .get(&id)
                .map(bicdb_graph::fulltext::node_revision)
                .transpose()
        };
        assert_eq!(
            restored.search("alpha", &opts, &limits, revision).unwrap(),
            expected.search("alpha", &opts, &limits, revision).unwrap()
        );
        full = expected;
    }
}

#[test]
fn sparse_batches_enforce_corpus_limits_and_fail_without_changing_old_rows() {
    let limits = TextLimits::default();
    let mut graph = Graph::new();
    query(
        &mut graph,
        "CREATE (:Entity {db_type:'d1',name:'one'}),(:Entity {db_type:'d1',name:'two'})",
    );
    let generation = Generation::build(definition(), &graph, 1, 0, &limits).unwrap();
    let durable = generation.native_rows(&limits).unwrap();
    query(&mut graph, "CREATE (:Entity {db_type:'d1',name:'three'})");
    let id = *graph.nodes().keys().last().unwrap();
    let doc = definition()
        .node_document(&graph.nodes()[&id], &limits)
        .unwrap()
        .unwrap();
    let rows = selected(&durable, &BTreeSet::from([id]));
    let saved = rows.clone();
    let budget = Generation::manifest_from_native_rows(&durable, &limits)
        .unwrap()
        .corpus_budget
        .unwrap();
    for tight in [
        TextLimits {
            max_documents: 2,
            ..limits.clone()
        },
        TextLimits {
            max_tokens: budget.tokens,
            ..limits.clone()
        },
        TextLimits {
            max_bytes: budget.native_bytes + 1,
            ..limits.clone()
        },
    ] {
        assert!(
            Generation::apply_native_batch(&rows, vec![(id, Some(doc.clone()))], 1, &tight)
                .is_err()
        );
        assert_eq!(rows, saved);
    }
    assert!(
        Generation::apply_native_batch(&rows, vec![(id, None), (id, None)], 1, &limits).is_err()
    );
    assert!(Generation::apply_native_batch(&rows, vec![(0, None)], 1, &limits).is_err());
    let mut mismatch = doc.clone();
    mismatch.id += 1;
    assert!(Generation::apply_native_batch(&rows, vec![(id, Some(mismatch))], 1, &limits).is_err());
    let mut extra = rows.clone();
    extra.extend(
        durable
            .iter()
            .filter(|(k, _)| **k >> 60 == 1)
            .map(|(k, v)| (*k, v.clone())),
    );
    assert!(
        Generation::apply_native_batch(&extra, vec![(id, Some(doc.clone()))], 1, &limits).is_err()
    );
    // Logical document budgets can exceed encoded bytes; two individually valid
    // token-heavy documents must not bypass a global byte limit in small batches.
    let mut dense = Graph::new();
    let id1 = dense
        .add_node(
            BTreeSet::from(["Entity".into()]),
            BTreeMap::from([
                ("db_type".into(), serde_json::json!("d1")),
                ("name".into(), serde_json::json!("word ".repeat(2000))),
            ]),
        )
        .unwrap();
    let full = Generation::build(definition(), &dense, 1, 0, &limits).unwrap();
    let durable = full.native_rows(&limits).unwrap();
    let cost = Generation::manifest_from_native_rows(&durable, &limits)
        .unwrap()
        .corpus_budget
        .unwrap();
    assert!(cost.document_bytes > cost.native_bytes);
    let id2 = dense
        .add_node(
            BTreeSet::from(["Entity".into()]),
            dense.nodes()[&id1].properties.clone(),
        )
        .unwrap();
    let doc = definition()
        .node_document(&dense.nodes()[&id2], &limits)
        .unwrap();
    let rows = selected(&durable, &BTreeSet::from([id2]));
    assert!(Generation::apply_native_batch(
        &rows,
        vec![(id2, doc)],
        1,
        &TextLimits {
            max_bytes: cost.document_bytes + 1,
            ..limits
        }
    )
    .is_err());
}

#[test]
fn sparse_batches_ignore_unrequested_bodies_but_validate_requested_records_and_global_metadata() {
    let limits = TextLimits::default();
    let mut graph = Graph::new();
    query(
        &mut graph,
        "CREATE (:Entity {db_type:'d1',name:'one'}),(:Entity {db_type:'d1',name:'two'})",
    );
    let full = Generation::build(definition(), &graph, 1, 0, &limits).unwrap();
    let mut durable = full.native_rows(&limits).unwrap();
    let other = Generation::document_range(2).unwrap().0;
    durable.get_mut(&other).unwrap()[0] ^= 1;
    assert!(Generation::from_native_rows(&durable, &limits).is_err());
    query(&mut graph, "MATCH (n {name:'one'}) SET n.name='fresh'");
    let doc = definition()
        .node_document(&graph.nodes()[&1], &limits)
        .unwrap();
    let rows = selected(&durable, &BTreeSet::from([1]));
    let result = Generation::apply_native_batch(&rows, vec![(1, doc.clone())], 1, &limits).unwrap();
    assert_eq!(result.documents_loaded, 1);
    for kind in [1, 2, 3] {
        let mut broken = rows.clone();
        let key = *broken
            .keys()
            .find(|k| **k >> 60 == kind && **k & 1023 == 0)
            .unwrap();
        broken.get_mut(&key).unwrap()[0] ^= 1;
        assert!(
            Generation::apply_native_batch(&broken, vec![(1, doc.clone())], 1, &limits).is_err()
        );
    }
    let same = full.documents()[&1].clone();
    let clean = full.native_rows(&limits).unwrap();
    let rows = selected(&clean, &BTreeSet::from([1]));
    let no_op = Generation::apply_native_batch(&rows, vec![(1, Some(same))], 0, &limits).unwrap();
    assert!(no_op.changed.is_empty());
    assert!(no_op.entries.is_empty());
    assert_eq!(
        no_op.manifest.corpus_budget,
        Generation::manifest_from_native_rows(&clean, &limits)
            .unwrap()
            .corpus_budget
    );
}
