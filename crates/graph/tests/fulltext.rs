use bicdb_graph::fulltext::{
    node_revision, Channel, Definition, Generation, PathPart, SearchOptions, TextLimits,
};
use bicdb_graph::property_index::EntityKind;
use bicdb_graph::{execute, parse, Graph, Limits};
use std::collections::{BTreeMap, BTreeSet};
fn field(s: &str) -> Vec<PathPart> {
    s.split('.').map(|s| PathPart::Key(s.into())).collect()
}
fn definition() -> Definition {
    Definition {
        entity: EntityKind::Node,
        labels: vec!["Entity".into(), "Fault".into()],
        fields: vec![
            field("name"),
            field("summary"),
            field("config.question"),
            field("aliases"),
            field("tags"),
        ],
    }
}
fn sample() -> Graph {
    let mut graph = Graph::new();
    execute(&mut graph,&parse("CREATE (:Entity {db_type:'d1',name:'buffer pool',summary:'buffer pool buffer',config:{question:'缓冲池调优',hidden:'hiddenword'},aliases:['IO high',7,null,'tuning'],tags:['buffer','pool']}),(:Fault {db_type:'d1',name:'buffer',summary:'pool'}),(:Other {db_type:'d1',name:'buffer pool'}),(:Entity {db_type:'d2',name:'buffer pool'}),(:Entity {name:'buffer pool'}),(:Entity {db_type:'d1',name:'innodb_buffer_pool_instances',summary:'repeat repeat words'}),(:Entity {db_type:'d1',name:'utf8',summary:'前置文本\n数据库缓冲池调优完成'})").unwrap(),&BTreeMap::new(),&Limits::default()).unwrap();
    graph
}

#[test]
fn incremental_sync_reuses_unchanged_documents_and_sparse_rows_restore_full_generation() {
    let limits = TextLimits::default();
    let mut graph = sample();
    let before = Generation::build(definition(), &graph, 1, 10, &limits).unwrap();
    let unchanged = before
        .documents()
        .values()
        .find(|d| d.fields[0] == ["buffer pool"])
        .unwrap()
        .clone();
    for query in ["MATCH (n:Entity {name:'innodb_buffer_pool_instances'}) SET n.name='new_parameter',n.summary='new source'", "CREATE (:Entity {db_type:'d1',name:'added'})", "MATCH (n:Fault) DETACH DELETE n"] {
        execute(&mut graph, &parse(query).unwrap(), &BTreeMap::new(), &Limits::default()).unwrap();
    }
    let (after, changed) = before.synchronize(&graph, 11, &limits).unwrap();
    assert_eq!(changed.len(), 3, "one update, one insert and one removal");
    assert_eq!(after.documents()[&unchanged.id], unchanged);
    assert!(!changed.contains(&unchanged.id));
    let full = Generation::build(definition(), &graph, 2, 11, &limits).unwrap();
    assert_eq!(
        after.native_rows(&limits).unwrap(),
        full.native_rows(&limits).unwrap()
    );
    let old_sparse = before.native_rows_for(&changed, &limits).unwrap();
    let new_sparse = after.native_rows_for(&changed, &limits).unwrap();
    let mut durable = before.native_rows(&limits).unwrap();
    for key in old_sparse
        .keys()
        .filter(|key| !new_sparse.contains_key(key))
    {
        durable.remove(key);
    }
    durable.extend(new_sparse);
    let restored = Generation::from_native_rows(&durable, &limits).unwrap();
    assert_eq!(
        restored.native_rows(&limits).unwrap(),
        full.native_rows(&limits).unwrap()
    );
    assert_eq!(
        after.native_entries_for(&changed, &limits).unwrap(),
        after
            .native_entries(&limits)
            .unwrap()
            .into_iter()
            .filter(|(_, id)| changed.contains(id))
            .collect::<Vec<_>>()
    );
    let (again, changed) = after.synchronize(&graph, 12, &limits).unwrap();
    assert!(changed.is_empty());
    assert_eq!(again.documents(), after.documents());
    assert!(again
        .native_rows_for(&changed, &limits)
        .unwrap()
        .keys()
        .all(|key| *key <= 1023 || key >> 60 == 2 || key >> 60 == 3));
    assert!(again
        .native_entries_for(&changed, &limits)
        .unwrap()
        .is_empty());
    assert!(again.synchronize(&graph, 11, &limits).is_err());
}

#[test]
fn independent_manifest_survives_corrupt_document_and_delta_detects_label_domain_changes() {
    let limits = TextLimits::default();
    let mut graph = sample();
    let before = Generation::build(definition(), &graph, 1, 10, &limits).unwrap();
    let mut rows = before.native_rows(&limits).unwrap();
    let manifest = Generation::manifest_from_native_rows(&rows, &limits).unwrap();
    let document_key = *rows
        .keys()
        .find(|k| **k >> 60 == 1 && **k & 1023 != 0)
        .unwrap();
    rows.get_mut(&document_key).unwrap()[0] ^= 1;
    assert_eq!(
        Generation::manifest_from_native_rows(&rows, &limits).unwrap(),
        manifest
    );
    assert!(Generation::from_native_rows(&rows, &limits).is_err());
    rows.get_mut(&0).unwrap()[0] ^= 1;
    assert!(Generation::manifest_from_native_rows(&rows, &limits).is_err());
    for query in [
        "MATCH (n:Entity {name:'innodb_buffer_pool_instances'}) REMOVE n:Entity",
        "MATCH (n:Fault) SET n.db_type='d2'",
        "MATCH (n:Other) SET n:Entity",
    ] {
        execute(
            &mut graph,
            &parse(query).unwrap(),
            &BTreeMap::new(),
            &Limits::default(),
        )
        .unwrap();
    }
    let (after, changed) = before.synchronize(&graph, 11, &limits).unwrap();
    assert_eq!(changed.len(), 3);
    assert_eq!(
        after.native_rows(&limits).unwrap(),
        Generation::build(definition(), &graph, 2, 11, &limits)
            .unwrap()
            .native_rows(&limits)
            .unwrap()
    );
    assert!(after
        .native_rows_for(&BTreeSet::from([0]), &limits)
        .is_err());
    assert!(after
        .native_entries_for(&BTreeSet::from([1 << 48]), &limits)
        .is_err());
}
fn options(channel: Channel, fields: &[&str]) -> SearchOptions {
    SearchOptions {
        domain: "d1".into(),
        channel,
        fields: fields.iter().map(|s| s.to_string()).collect(),
    }
}
fn search(
    index: &Generation,
    graph: &Graph,
    q: &str,
    o: &SearchOptions,
) -> Vec<bicdb_graph::fulltext::Hit> {
    index
        .search(q, o, &TextLimits::default(), |id| {
            graph.nodes().get(&id).map(node_revision).transpose()
        })
        .unwrap()
}
#[test]
fn domain_labels_field_union_phrase_and_exact_identifier_boundaries() {
    let g = sample();
    let i = Generation::build(definition(), &g, 1, 10, &TextLimits::default()).unwrap();
    assert_eq!(
        i.documents().len(),
        5,
        "unscoped and wrong-label nodes excluded"
    );
    assert_eq!(
        search(
            &i,
            &g,
            "buffer pool",
            &options(Channel::Terms, &["name", "summary"])
        )
        .len(),
        2
    );
    assert_eq!(
        search(
            &i,
            &g,
            "buffer pool",
            &options(Channel::Phrase, &["name", "summary"])
        )
        .len(),
        1,
        "phrase cannot cross fields"
    );
    assert_eq!(
        search(&i, &g, "buffer pool", &options(Channel::Phrase, &["tags"])).len(),
        0,
        "phrase cannot cross array elements"
    );
    assert_eq!(
        search(&i, &g, "buffer pool", &options(Channel::Terms, &["tags"])).len(),
        1
    );
    assert_eq!(
        search(
            &i,
            &g,
            "INNODB_BUFFER_POOL_INSTANCES",
            &options(Channel::Exact, &[])
        )
        .len(),
        1
    );
    assert_eq!(
        search(&i, &g, "buffer", &options(Channel::Exact, &["name"])).len(),
        2,
        "exact matches a complete lexical identifier, not an entire field string"
    );
    assert!(
        search(&i, &g, "buffer_pool", &options(Channel::Exact, &[])).is_empty(),
        "an identifier substring is not an exact term"
    );
    assert!(i
        .search(
            "buffer pool",
            &options(Channel::Exact, &[]),
            &TextLimits::default(),
            |_| Ok(None)
        )
        .is_err());
    assert!(i
        .search(
            "buffer",
            &SearchOptions {
                domain: String::new(),
                ..options(Channel::Terms, &[])
            },
            &TextLimits::default(),
            |_| Ok(None)
        )
        .is_err());
    assert!(i
        .search(
            "buffer",
            &options(Channel::Terms, &["missing"]),
            &TextLimits::default(),
            |_| Ok(None)
        )
        .is_err());
    assert!(
        search(&i, &g, "hiddenword", &options(Channel::Terms, &[])).is_empty(),
        "non-target JSON field is never indexed"
    );
}
#[test]
fn cjk_subterms_positions_utf8_offsets_and_field_type_diagnostics() {
    let g = sample();
    let i = Generation::build(definition(), &g, 1, 10, &TextLimits::default()).unwrap();
    let hits = search(&i, &g, "冲池", &options(Channel::Terms, &[]));
    assert_eq!(hits.len(), 2);
    assert_eq!(
        search(&i, &g, "池", &options(Channel::Terms, &[])).len(),
        2,
        "single CJK character also indexed"
    );
    assert_eq!(
        search(
            &i,
            &g,
            "数据库缓冲池",
            &options(Channel::Phrase, &["summary"])
        )
        .len(),
        1
    );
    assert_eq!(
        search(&i, &g, "缓冲调优", &options(Channel::Phrase, &[])).len(),
        0,
        "noncontiguous characters are not a phrase"
    );
    let hit = search(&i, &g, "数据库", &options(Channel::Phrase, &["summary"]))
        .pop()
        .unwrap();
    assert_eq!(hit.offset, "前置文本\n".len());
    assert_eq!(hit.line, 2);
    assert!(hit.snippet.contains("数据库"));
    let doc = i
        .documents()
        .values()
        .find(|d| d.fields[0] == ["buffer pool"])
        .unwrap();
    assert_eq!(doc.diagnostics.excluded_type, 2);
    let mut d = definition();
    d.fields = vec![
        field("config.none"),
        field("config"),
        vec![PathPart::Key("aliases".into()), PathPart::Index(0)],
    ];
    let n = g
        .nodes()
        .values()
        .find(|n| n.properties["name"] == "buffer pool" && n.properties["db_type"] == "d1")
        .unwrap();
    let doc = d.node_document(n, &TextLimits::default()).unwrap().unwrap();
    assert_eq!(doc.diagnostics.missing, 1);
    assert_eq!(doc.diagnostics.excluded_type, 1);
    assert_eq!(doc.fields[2], ["IO high"]);
}
#[test]
fn bm25_statistics_are_domain_local_and_match_the_reference_formula() {
    let mut g = Graph::new();
    for name in ["buffer", "buffer buffer"] {
        g.add_node(
            BTreeSet::from(["Entity".into()]),
            BTreeMap::from([
                ("db_type".into(), serde_json::json!("d1")),
                ("name".into(), serde_json::json!(name)),
            ]),
        )
        .unwrap();
    }
    let mut d = definition();
    d.fields = vec![field("name")];
    let base = Generation::build(d.clone(), &g, 1, 0, &TextLimits::default()).unwrap();
    let hits = search(&base, &g, "buffer", &options(Channel::Terms, &[]));
    let idf = (1.0_f64 + (2.0 - 2.0 + 0.5) / (2.0 + 0.5)).ln();
    let expected = idf * 2.0 / (2.0 + 1.2 * (0.25 + 0.75 * 2.0 / 1.5));
    assert!((hits[0].score - expected).abs() < 1e-12);
    assert_eq!(hits[0].id, 2);
    for _ in 0..100 {
        g.add_node(
            BTreeSet::from(["Entity".into()]),
            BTreeMap::from([
                ("db_type".into(), serde_json::json!("d2")),
                ("name".into(), serde_json::json!("buffer")),
            ]),
        )
        .unwrap();
    }
    let extended = Generation::build(d, &g, 1, 0, &TextLimits::default()).unwrap();
    let next = search(&extended, &g, "buffer", &options(Channel::Terms, &[]));
    assert_eq!(hits, next, "other domains do not alter rank/statistics");
}
#[test]
fn batch_publication_rechecks_source_revision_and_keeps_failed_generation_intact() {
    let mut g = sample();
    let i = Generation::build(definition(), &g, 1, 10, &TextLimits::default()).unwrap();
    let id = search(
        &i,
        &g,
        "innodb_buffer_pool_instances",
        &options(Channel::Exact, &[]),
    )[0]
    .id;
    execute(
        &mut g,
        &parse("MATCH (n:Entity {name:'innodb_buffer_pool_instances'}) SET n.name='new_parameter'")
            .unwrap(),
        &BTreeMap::new(),
        &Limits::default(),
    )
    .unwrap();
    assert!(
        search(
            &i,
            &g,
            "innodb_buffer_pool_instances",
            &options(Channel::Exact, &[])
        )
        .is_empty(),
        "stale source revision cannot answer"
    );
    let doc = i
        .definition()
        .node_document(&g.nodes()[&id], &TextLimits::default())
        .unwrap();
    let next = i
        .apply_batch(vec![(id, doc.clone())], 11, &TextLimits::default())
        .unwrap();
    assert_eq!(next.generation(), 2);
    assert_eq!(next.covered_seq(), 11);
    assert_eq!(
        search(&next, &g, "new_parameter", &options(Channel::Exact, &[])).len(),
        1
    );
    assert!(next
        .apply_batch(
            vec![(id, doc.clone()), (id, doc.clone())],
            12,
            &TextLimits::default()
        )
        .is_err());
    assert!(next.apply_batch(vec![], 9, &TextLimits::default()).is_err());
    let mut bad = doc.unwrap();
    bad.tokens[0].end = usize::MAX;
    assert!(next
        .apply_batch(vec![(id, Some(bad))], 12, &TextLimits::default())
        .is_err());
    assert_eq!(
        search(&next, &g, "new_parameter", &options(Channel::Exact, &[])).len(),
        1,
        "failed batch leaves active generation intact"
    );
    let tombstone = next
        .apply_batch(vec![(id, None)], 12, &TextLimits::default())
        .unwrap();
    assert!(search(
        &tombstone,
        &g,
        "new_parameter",
        &options(Channel::Exact, &[])
    )
    .is_empty());
    assert_eq!(
        next.search_candidates(
            "innodb_buffer_pool_instances",
            &options(Channel::Exact, &[]),
            &TextLimits::default(),
            &BTreeSet::from([id, 9999]),
            |n| g.nodes().get(&n).map(node_revision).transpose()
        )
        .unwrap()
        .len(),
        0,
        "stale native posting IDs must be token-rechecked"
    );
}
#[test]
fn native_document_chunks_roundtrip_without_reanalysis_and_detect_corruption() {
    let g = sample();
    let limits = TextLimits::default();
    let i = Generation::build(definition(), &g, 3, 27, &limits).unwrap();
    let rows = i.native_rows(&limits).unwrap();
    let loaded = Generation::from_native_rows(&rows, &limits).unwrap();
    assert_eq!(i.documents(), loaded.documents());
    assert_eq!(
        i.native_entries(&limits).unwrap(),
        loaded.native_entries(&limits).unwrap()
    );
    assert_eq!(
        search(&i, &g, "缓冲池", &options(Channel::Phrase, &[])),
        search(&loaded, &g, "缓冲池", &options(Channel::Phrase, &[]))
    );
    let mut corrupted = rows.clone();
    corrupted.values_mut().last().unwrap()[0] ^= 1;
    assert!(Generation::from_native_rows(&corrupted, &limits).is_err());
    let mut missing = rows.clone();
    missing.pop_last();
    assert!(Generation::from_native_rows(&missing, &limits).is_err());
    assert!(Generation::from_native_rows(
        &rows,
        &TextLimits {
            max_bytes: 100,
            ..limits.clone()
        }
    )
    .is_err());
    assert!(Generation::build(definition(), &g, 0, 10, &limits).is_err());
    assert!(Generation::build(
        definition(),
        &g,
        1,
        10,
        &TextLimits {
            max_tokens: 1,
            ..limits.clone()
        }
    )
    .is_err());
    let bad = Definition {
        fields: vec![field("name"), field("name")],
        ..definition()
    };
    assert!(bad.validate().is_err());
    assert_eq!(
        Definition::from_bytes(&definition().to_bytes().unwrap()).unwrap(),
        definition()
    );
}
#[test]
fn native_postings_survive_btree_splits_reopen_and_candidate_recheck() {
    use bicdb_index::{MemStore, Tree};
    use bicdb_storage::rowid::RowId;
    let mut g = Graph::new();
    for i in 0..800 {
        g.add_node(
            BTreeSet::from(["Entity".into()]),
            BTreeMap::from([
                (
                    "db_type".into(),
                    serde_json::json!(if i < 400 { "d1" } else { "d2" }),
                ),
                ("name".into(), serde_json::json!(format!("buffer item_{i}"))),
            ]),
        )
        .unwrap();
    }
    let mut d = definition();
    d.fields = vec![field("name")];
    let index = Generation::build(d, &g, 1, 0, &TextLimits::default()).unwrap();
    let mut store = MemStore::new(4096, 3, [7; 8]);
    let root;
    {
        let mut tree = Tree::create(&mut store, 3, [7; 8]).unwrap();
        for (key, id) in index.native_entries(&TextLimits::default()).unwrap() {
            let b = id.to_le_bytes();
            tree.insert(&key, RowId::from_bytes(b[..6].try_into().unwrap()))
                .unwrap();
        }
        tree.validate().unwrap();
        assert!(tree.height() > 0);
        root = tree.root();
    }
    let o = options(Channel::Terms, &[]);
    let ranges = index
        .term_ranges("buffer item_17", &o, &TextLimits::default())
        .unwrap();
    let mut candidates = BTreeSet::new();
    {
        let mut tree = Tree::open(&mut store, 3, root).unwrap();
        for r in ranges {
            for (key, rid) in tree
                .range(Some(&r.lower), r.upper.as_deref(), 5000)
                .unwrap()
            {
                if key.starts_with(&r.lower) {
                    candidates.insert(rid.as_raw());
                }
            }
        }
    }
    let native = index
        .search_candidates(
            "buffer item_17",
            &o,
            &TextLimits::default(),
            &candidates,
            |id| g.nodes().get(&id).map(node_revision).transpose(),
        )
        .unwrap();
    let baseline = search(&index, &g, "buffer item_17", &o);
    assert_eq!(native, baseline);
    assert_eq!(native.len(), 1);
    assert_eq!(native[0].id, 18);
}

#[test]
fn relationship_types_revisions_and_multilingual_array_paths() {
    use bicdb_graph::fulltext::{edge_revision, round_robin};
    use bicdb_graph::storage::StorageImage;
    let limits = TextLimits::default();
    let mut graph = Graph::new();
    execute(&mut graph, &parse("CREATE (a:Entity {db_type:'d1',name:'入口'}),(b:Entity {db_type:'d1',name:'出口'}),(a)-[:CAUSE {db_type:'d1',config:{answers:['数据库調整','데이터베이스 조정']}}]->(b),(b)-[:OTHER {db_type:'d1',config:{answers:['数据库調整']}}]->(a)").unwrap(), &BTreeMap::new(), &Limits::default()).unwrap();
    let definition = Definition {
        entity: EntityKind::Relationship,
        labels: vec!["CAUSE".into()],
        fields: vec![field("config.answers")],
    };
    let index = Generation::build(definition, &graph, 1, 0, &limits).unwrap();
    assert_eq!(index.documents().len(), 1);
    let search_edge = |query, channel| {
        index
            .search(query, &options(channel, &[]), &limits, |id| {
                graph.edges().get(&id).map(edge_revision).transpose()
            })
            .unwrap()
    };
    let japanese = search_edge("調整", Channel::Terms);
    let korean = search_edge("베이스", Channel::Phrase);
    assert_eq!(japanese.len(), 1);
    assert_eq!(korean.len(), 1);
    assert_eq!(japanese[0].id, korean[0].id);
    assert!(search_edge("調整 데이터", Channel::Phrase).is_empty());
    assert_eq!(
        round_robin(&[japanese.clone(), korean.clone()], 10).len(),
        1
    );
    assert!(round_robin(&[japanese, korean], 0).is_empty());

    let (before, empty) = StorageImage::decode(BTreeMap::new(), &Limits::default()).unwrap();
    let patch = empty.patch(&before, &graph, &Limits::default()).unwrap();
    for (id, node) in graph.nodes() {
        assert_eq!(
            patch.inserted[&((1 << 60) | (id << 10))],
            node_revision(node).unwrap()
        );
    }
    for (id, edge) in graph.edges() {
        assert_eq!(
            patch.inserted[&((2 << 60) | (id << 10))],
            edge_revision(edge).unwrap()
        );
    }
}

#[test]
fn malformed_batch_positions_and_aggregate_corpus_budgets_fail_atomically() {
    let graph = sample();
    let limits = TextLimits::default();
    let index = Generation::build(definition(), &graph, 1, 0, &limits).unwrap();
    let mut doc = index.documents().values().next().unwrap().clone();
    doc.tokens[0].start = doc.tokens[0].end;
    assert!(index
        .apply_batch(vec![(doc.id, Some(doc))], 1, &limits)
        .is_err());
    assert_eq!(index.generation(), 1);
    assert_eq!(index.covered_seq(), 0);
    let total = index
        .documents()
        .values()
        .map(|d| d.tokens.len())
        .sum::<usize>();
    let tighter = TextLimits {
        max_tokens: total - 1,
        ..limits.clone()
    };
    assert!(Generation::build(definition(), &graph, 1, 0, &tighter).is_err());
    assert!(Generation::from_native_rows(&index.native_rows(&limits).unwrap(), &tighter).is_err());
    let node = graph.nodes().values().next().unwrap();
    let paths = Definition {
        fields: vec![
            field("config.none"),
            field("name.child"),
            field("config.question.child"),
        ],
        ..definition()
    };
    let mut nullable = node.clone();
    nullable
        .properties
        .insert("config".into(), serde_json::Value::Null);
    let null_doc = paths.node_document(&nullable, &limits).unwrap().unwrap();
    assert_eq!(null_doc.diagnostics.null, 2);
    assert_eq!(null_doc.diagnostics.excluded_type, 1);
    let nonnull_doc = paths.node_document(node, &limits).unwrap().unwrap();
    assert_eq!(nonnull_doc.diagnostics.missing, 1);
    assert_eq!(nonnull_doc.diagnostics.excluded_type, 2);
}

#[test]
fn complete_statistics_reads_preserve_global_scores_and_avoid_unselected_document_bodies() {
    let graph = sample();
    let limits = TextLimits::default();
    let full = Generation::build(definition(), &graph, 1, 10, &limits).unwrap();
    let rows = full.native_rows(&limits).unwrap();
    let manifest = Generation::manifest_from_native_rows(&rows, &limits).unwrap();
    assert!(manifest.statistics_pages.is_some());
    for channel in [Channel::Terms, Channel::Phrase, Channel::Exact] {
        let options = SearchOptions {
            domain: "d1".into(),
            fields: vec!["name".into()],
            channel,
        };
        let expected = full
            .search("buffer", &options, &limits, |id| {
                node_revision(&graph.nodes()[&id]).map(Some)
            })
            .unwrap();
        let candidates = expected.iter().map(|hit| hit.id).collect::<BTreeSet<_>>();
        let selected: BTreeMap<_, _> = rows
            .iter()
            .filter(|(key, _)| {
                **key <= 1023
                    || *key >> 60 == 2
                    || *key >> 60 == 3
                    || candidates.contains(&((**key & ((1 << 60) - 1)) >> 10))
            })
            .map(|(key, v)| (*key, v.clone()))
            .collect();
        let partial = Generation::from_candidate_rows(&selected, &candidates, &limits).unwrap();
        let actual = partial
            .search_candidates("buffer", &options, &limits, &candidates, |id| {
                node_revision(&graph.nodes()[&id]).map(Some)
            })
            .unwrap();
        assert_eq!(actual.len(), expected.len());
        for (a, b) in actual.iter().zip(expected.iter()) {
            assert_eq!(a.id, b.id);
            assert_eq!(a.score, b.score);
            assert_eq!(a.snippet, b.snippet);
            assert_eq!(a.offset, b.offset);
        }
        assert!(partial.native_rows(&limits).is_err());
        assert!(partial.apply_batch(vec![], 11, &limits).is_err());
    }
    let candidate = *full.documents().keys().next().unwrap();
    let ids = BTreeSet::from([candidate]);
    let selected: BTreeMap<_, _> = rows
        .iter()
        .filter(|(key, _)| {
            **key <= 1023
                || *key >> 60 == 2
                || *key >> 60 == 3
                || ids.contains(&((**key & ((1 << 60) - 1)) >> 10))
        })
        .map(|(key, v)| (*key, v.clone()))
        .collect();
    let mut broken = rows.clone();
    let other = *full
        .documents()
        .keys()
        .find(|id| **id != candidate)
        .unwrap();
    let (lower, _) = Generation::document_range(other).unwrap();
    broken.get_mut(&lower).unwrap()[0] ^= 1;
    assert!(Generation::from_native_rows(&broken, &limits).is_err());
    assert!(Generation::from_candidate_rows(&selected, &ids, &limits).is_ok());
    let mut corrupted = selected.clone();
    let key = *corrupted
        .keys()
        .find(|k| **k >> 60 == 2 && **k & 1023 == 0)
        .unwrap();
    corrupted.get_mut(&key).unwrap()[0] ^= 1;
    assert!(Generation::from_candidate_rows(&corrupted, &ids, &limits).is_err());
    let mut missing = selected.clone();
    missing.remove(&key);
    assert!(Generation::from_candidate_rows(&missing, &ids, &limits).is_err());
}

fn query_rows(
    generation: &Generation,
    rows: &BTreeMap<u64, Vec<u8>>,
    ids: &BTreeSet<u64>,
    query: &str,
    options: &SearchOptions,
) -> (
    BTreeMap<u64, Vec<u8>>,
    bicdb_graph::fulltext::StatisticsSelection,
) {
    let plan = generation
        .statistics_selection(query, options, &TextLimits::default())
        .unwrap()
        .unwrap();
    let mut ranges = plan.ranges();
    ranges.push((0, 1023));
    ranges.extend(
        ids.iter()
            .map(|id| Generation::document_range(*id).unwrap()),
    );
    let selected = rows
        .iter()
        .filter(|(k, _)| ranges.iter().any(|(l, u)| **k >= *l && **k <= *u))
        .map(|(k, v)| (*k, v.clone()))
        .collect();
    (selected, plan)
}

#[test]
fn keyed_query_statistics_keep_global_scores_and_bound_loading_to_selected_terms() {
    let mut graph = sample();
    for i in 0..1200 {
        graph
            .add_node(
                BTreeSet::from(["Entity".into()]),
                BTreeMap::from([
                    (
                        "db_type".into(),
                        serde_json::json!(format!("unrelated_{i}")),
                    ),
                    ("name".into(), serde_json::json!(format!("word_{i}"))),
                    ("summary".into(), serde_json::json!(format!("summary_{i}"))),
                ]),
            )
            .unwrap();
    }
    let limits = TextLimits::default();
    let full = Generation::build(definition(), &graph, 1, 10, &limits).unwrap();
    let rows = full.native_rows(&limits).unwrap();
    assert_eq!(
        Generation::manifest_from_native_rows(&rows, &limits)
            .unwrap()
            .storage_format,
        4
    );
    let total_statistics = rows
        .keys()
        .filter(|k| **k >> 60 == 2 || **k >> 60 == 3)
        .count();
    for (query, channel, fields) in [
        ("buffer pool", Channel::Terms, vec!["name", "summary"]),
        ("缓冲池", Channel::Phrase, vec!["config.question"]),
        ("innodb_buffer_pool_instances", Channel::Exact, vec!["name"]),
    ] {
        let options = options(channel, &fields);
        let expected = search(&full, &graph, query, &options);
        assert!(!expected.is_empty());
        let ids = expected.iter().map(|h| h.id).collect();
        let (selected, plan) = query_rows(&full, &rows, &ids, query, &options);
        let loaded = selected
            .keys()
            .filter(|k| **k >> 60 == 2 || **k >> 60 == 3)
            .count();
        assert!(
            loaded * 10 < total_statistics,
            "selected {loaded}, total {total_statistics}"
        );
        let partial =
            Generation::from_query_candidate_rows(&selected, &ids, &plan, &limits).unwrap();
        let actual = partial
            .search_candidates(query, &options, &limits, &ids, |id| {
                node_revision(&graph.nodes()[&id]).map(Some)
            })
            .unwrap();
        assert_eq!(
            actual, expected,
            "IDs, exact score, snippets, UTF-8 offsets and order"
        );
        let other = *rows
            .keys()
            .find(|k| **k >> 60 == 2 && !selected.contains_key(k))
            .unwrap();
        let mut damaged = rows.clone();
        damaged.get_mut(&other).unwrap()[0] ^= 1;
        assert!(Generation::from_native_rows(&damaged, &limits).is_err());
        let (requested, _) = query_rows(&full, &damaged, &ids, query, &options);
        assert!(
            Generation::from_query_candidate_rows(&requested, &ids, &plan, &limits).is_ok(),
            "unrequested vocabulary records are not loaded"
        );
        assert!(partial.native_rows(&limits).is_err());
        assert!(partial.apply_batch(vec![], 11, &limits).is_err());
        assert!(
            partial
                .search("another", &options, &limits, |_| Ok(None))
                .is_err(),
            "query-only stats cannot answer another query"
        );
        assert!(Generation::from_query_candidate_rows(
            &selected,
            &ids,
            &plan,
            &TextLimits {
                max_bytes: 100,
                ..limits.clone()
            }
        )
        .is_err());
    }
    let ids = BTreeSet::from([999999]);
    let opts = options(Channel::Terms, &["name"]);
    let (selected, plan) = query_rows(&full, &rows, &ids, "definitelymissing", &opts);
    let partial = Generation::from_query_candidate_rows(&selected, &ids, &plan, &limits).unwrap();
    assert!(
        partial
            .search_candidates("definitelymissing", &opts, &limits, &ids, |_| Ok(None))
            .unwrap()
            .is_empty(),
        "verify absence for stale postings"
    );
}

#[test]
fn query_statistics_reject_missing_corrupt_and_cross_generation_proof_records() {
    let mut graph = sample();
    let limits = TextLimits::default();
    let full = Generation::build(definition(), &graph, 1, 10, &limits).unwrap();
    let rows = full.native_rows(&limits).unwrap();
    let opts = options(Channel::Terms, &["name"]);
    let ids = search(&full, &graph, "buffer", &opts)
        .iter()
        .map(|h| h.id)
        .collect();
    let (selected, plan) = query_rows(&full, &rows, &ids, "buffer", &opts);
    assert!(Generation::from_query_candidate_rows(&selected, &ids, &plan, &limits).is_ok());
    for kind in [2, 3] {
        let key = *selected
            .keys()
            .find(|k| **k >> 60 == kind && **k & 1023 == 0)
            .unwrap();
        let mut broken = selected.clone();
        broken.get_mut(&key).unwrap()[0] ^= 1;
        assert!(Generation::from_query_candidate_rows(&broken, &ids, &plan, &limits).is_err());
        let mut missing = selected.clone();
        missing.retain(|k, _| *k < key || *k > key + 1023);
        assert!(
            Generation::from_query_candidate_rows(&missing, &ids, &plan, &limits).is_err(),
            "missing whole record cannot masquerade as absence"
        );
    }
    execute(
        &mut graph,
        &parse("CREATE (:Entity {db_type:'d1',name:'buffer buffer buffer'})").unwrap(),
        &BTreeMap::new(),
        &Limits::default(),
    )
    .unwrap();
    let new = Generation::build(definition(), &graph, 2, 11, &limits).unwrap();
    let newer = new.native_rows(&limits).unwrap();
    let mut mixed = selected.clone();
    mixed.retain(|k, _| *k > 1023);
    mixed.extend(newer.range(0..=1023).map(|(k, v)| (*k, v.clone())));
    assert!(
        Generation::from_query_candidate_rows(&mixed, &ids, &plan, &limits).is_err(),
        "valid old records cannot be spliced into a new header"
    );
    // Recomputed record checksums do not authorize altered corpus counts.
    let key = *selected
        .keys()
        .find(|k| **k >> 60 == 2 && **k & 1023 == 0)
        .unwrap();
    let mut body = Vec::new();
    for (_, bytes) in selected.range(key + 1..=key + 1023) {
        body.extend(bytes);
    }
    let mut value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    value[0][4] = serde_json::json!(value[0][4].as_u64().unwrap() + 1);
    let bytes = serde_json::to_vec(&value).unwrap();
    let mut altered = selected.clone();
    altered.retain(|k, _| *k < key || *k > key + 1023);
    altered.insert(key, bicdb_common::sha256::digest(&bytes).to_vec());
    for (i, chunk) in bytes.chunks(4096).enumerate() {
        altered.insert(key + i as u64 + 1, chunk.to_vec());
    }
    assert!(Generation::from_query_candidate_rows(&altered, &ids, &plan, &limits).is_err());
}

#[test]
fn keyed_statistics_hash_collisions_keep_distinct_terms_and_full_identities() {
    let limits = TextLimits::default();
    let options = options(Channel::Exact, &["name"]);
    let mut graph = Graph::new();
    let definition = Definition {
        entity: EntityKind::Node,
        labels: vec![],
        fields: vec![field("name")],
    };
    let empty = Generation::build(definition.clone(), &graph, 1, 0, &limits).unwrap();
    let mut buckets = BTreeMap::new();
    let mut collision = None;
    for i in 0..4097 {
        let term = format!("collision{i}");
        let key = empty
            .term_ranges(&term, &options, &limits)
            .unwrap()
            .remove(0)
            .lower;
        let digest = bicdb_common::sha256::digest(&key);
        let b = u16::from_be_bytes([digest[0], digest[1]]) >> 4;
        if let Some(previous) = buckets.insert(b, term.clone()) {
            collision = Some((previous, term));
            break;
        }
    }
    let (first, second) = collision.unwrap();
    let mut ids = BTreeSet::new();
    for term in [&first, &second] {
        ids.insert(
            graph
                .add_node(
                    BTreeSet::new(),
                    BTreeMap::from([
                        ("db_type".into(), serde_json::json!("d1")),
                        ("name".into(), serde_json::json!(term)),
                    ]),
                )
                .unwrap(),
        );
    }
    let generation = Generation::build(definition, &graph, 1, 0, &limits).unwrap();
    let rows = generation.native_rows(&limits).unwrap();
    for term in [first, second] {
        let expected = search(&generation, &graph, &term, &options);
        assert_eq!(expected.len(), 1);
        let (selected, plan) = query_rows(&generation, &rows, &ids, &term, &options);
        let partial =
            Generation::from_query_candidate_rows(&selected, &ids, &plan, &limits).unwrap();
        let actual = partial
            .search_candidates(&term, &options, &limits, &ids, |id| {
                node_revision(&graph.nodes()[&id]).map(Some)
            })
            .unwrap();
        assert_eq!(
            actual, expected,
            "same native bucket cannot alias two complete keys"
        );
    }
}
