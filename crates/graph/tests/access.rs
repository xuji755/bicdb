use bicdb_graph::access::{adjacency_prefix, label_key, AccessEntries, GraphAccess};
use bicdb_graph::property_index::{EntityKind, IndexProvider, IndexRequest, PropertyIndex};
use bicdb_graph::{
    execute, execute_with_storage, parse, storage, Direction, Edge, Error, Graph, Limits, Node,
};
use std::collections::{BTreeMap, BTreeSet};

struct Snapshot {
    graph: Graph,
    index: PropertyIndex,
    nodes: usize,
    edges: usize,
}
impl IndexProvider for Snapshot {
    fn candidates(&mut self, request: &IndexRequest) -> Result<Option<Vec<u64>>, Error> {
        let Some(seek) = self.index.seek(request) else {
            return Ok(None);
        };
        Ok(Some(
            self.index
                .entries(&self.graph)?
                .into_iter()
                .filter(|(k, _)| seek.ranges.iter().any(|r| r.contains(k)))
                .map(|(_, id)| id)
                .chain([9999])
                .collect(),
        ))
    }
}
impl GraphAccess for Snapshot {
    fn node(&mut self, id: u64) -> Result<Option<Node>, Error> {
        self.nodes += 1;
        Ok(self.graph.nodes().get(&id).cloned())
    }
    fn edge(&mut self, id: u64) -> Result<Option<Edge>, Error> {
        self.edges += 1;
        Ok(self.graph.edges().get(&id).cloned())
    }
    fn node_ids(&mut self, labels: &[String]) -> Result<Vec<u64>, Error> {
        Ok(self
            .graph
            .nodes()
            .values()
            .filter(|n| labels.iter().all(|l| n.labels.contains(l)))
            .map(|n| n.id)
            .chain([9999])
            .collect())
    }
    fn edge_ids(
        &mut self,
        node: u64,
        direction: Direction,
        types: &[String],
    ) -> Result<Vec<u64>, Error> {
        // Inject wrong-type/wrong-direction/absent entries, as old native trees
        // can contain stale entries. The executor must recheck current records.
        let mut ids: Vec<_> = self
            .graph
            .neighbors(node, direction, types)
            .into_iter()
            .map(|(e, _)| e)
            .collect();
        ids.extend(self.graph.edges().keys());
        ids.push(9998);
        Ok(ids)
    }
}
fn sample() -> Snapshot {
    let mut graph = Graph::new();
    execute(&mut graph,&parse("CREATE (a:Anchor:Entity {name:'a',db_type:'2123',config:{port:10}}),(b:Entity {name:'b',db_type:'2123',config:{port:20}}),(c:Other {name:'unused'}),(a)-[:LINK {weight:1}]->(b),(a)-[:LINK {weight:2}]->(b),(b)-[:BACK]->(a),(a)-[:SELF]->(a)").unwrap(),&BTreeMap::new(),&Limits::default()).unwrap();
    Snapshot {
        graph,
        index: PropertyIndex {
            entity: EntityKind::Node,
            label: Some("Entity".into()),
            fields: vec![vec!["name".into()]],
            unique: false,
        },
        nodes: 0,
        edges: 0,
    }
}
fn cache(g: &Graph) -> Graph {
    let empty = storage::StorageImage::decode(BTreeMap::new(), &Limits::default())
        .unwrap()
        .1;
    let patch = empty.patch(&Graph::new(), g, &Limits::default()).unwrap();
    storage::snapshot_cache(
        patch.inserted.get(&0).map(Vec::as_slice),
        &Limits::default(),
    )
    .unwrap()
    .unwrap()
}
#[test]
fn lazy_point_seek_hydrates_only_the_candidate_and_returns_complete_properties() {
    let mut snapshot = sample();
    let mut cache = cache(&snapshot.graph);
    let query = parse("MATCH (n:Entity {name:'a'}) RETURN n AS node,elementId(n) AS eid").unwrap();
    let result = execute_with_storage(
        &mut cache,
        &query,
        &BTreeMap::new(),
        &Limits::default(),
        &mut snapshot,
    )
    .unwrap();
    assert_eq!(cache.nodes().len(), 1);
    assert!(cache.edges().is_empty());
    assert_eq!(snapshot.nodes, 2); // one live node + one ghost
    assert_eq!(
        result.rows[0][0].to_json(&cache)["properties"]["config"]["port"],
        10
    );
}
#[test]
fn lazy_directions_paths_optional_exists_subqueries_and_search_match_scan() {
    for sql in [
        "MATCH (a:Entity {name:'a'})-[r:LINK]->(b) RETURN b.name AS name,r.weight AS w ORDER BY w",
        "MATCH (a:Anchor)<-[r]-(b) RETURN type(r) AS t,b.name AS name ORDER BY t",
        "MATCH (a:Anchor)-[r:SELF]-(a) RETURN count(r) AS n",
        "MATCH p=shortestPath((a:Anchor)-[:LINK|BACK*1..3]->(b:Entity {name:'b'})) RETURN length(p) AS n",
        "MATCH (a:Anchor) OPTIONAL MATCH (a)-[:ABSENT]->(b) RETURN b.name AS name",
        "MATCH (a:Anchor) WHERE EXISTS { MATCH (a)-[:LINK]->(b) WHERE b.name='b' } RETURN a.name AS name",
        "MATCH (a:Anchor) CALL (a) { MATCH (a)-[r]->(b) RETURN count(r) AS n } RETURN n",
        "CALL bicdb.searchNodes('a',{db_type:'2123',limit:10}) YIELD node,rank,mode RETURN node.name AS name,rank,mode",
        "MATCH (a:Entity) WHERE a.config.port>=10 RETURN a.name AS name ORDER BY name",
        "MATCH (a:Absent) RETURN count(a) AS n",
        "UNWIND [1,2] AS x RETURN x UNION ALL RETURN 3 AS x",
    ] {
        let mut source=sample();let mut cache=cache(&source.graph);let q=parse(sql).unwrap();
        let baseline=execute(&mut source.graph.clone(),&q,&BTreeMap::new(),&Limits::default()).unwrap();
        let lazy=execute_with_storage(&mut cache,&q,&BTreeMap::new(),&Limits::default(),&mut source).unwrap_or_else(|e|panic!("{sql}: {e}"));
        assert_eq!(lazy.rows,baseline.rows,"{sql}");
    }
}
#[test]
fn partial_cache_cannot_be_used_for_writes_or_published_on_failure() {
    let mut source = sample();
    let mut cache = cache(&source.graph);
    for sql in ["CREATE (n)", "MATCH (n:Anchor) RETURN 1/0 AS n"] {
        assert!(execute_with_storage(
            &mut cache,
            &parse(sql).unwrap(),
            &BTreeMap::new(),
            &Limits::default(),
            &mut source
        )
        .is_err());
        assert!(cache.nodes().is_empty());
        assert!(cache.edges().is_empty());
    }
}

#[test]
fn lazy_edge_candidates_charge_ghosts_before_fetch_and_repeat_even_when_cached() {
    let query = parse("MATCH (a:Anchor)-[:LINK]->(b) RETURN b.name").unwrap();
    let mut snapshot = sample();
    let mut cache = cache(&snapshot.graph);
    let limits = Limits {
        max_edge_expansions: 4,
        ..Limits::default()
    };
    let error = execute_with_storage(&mut cache, &query, &BTreeMap::new(), &limits, &mut snapshot)
        .unwrap_err()
        .to_string();
    assert!(error.contains("edge expansion budget"), "{error}");
    assert_eq!(
        snapshot.edges, 0,
        "over-wide candidate set must fail before record fetch"
    );
    assert!(cache.nodes().is_empty());
    let limits = Limits {
        max_edge_expansions: 5,
        ..Limits::default()
    };
    let result =
        execute_with_storage(&mut cache, &query, &BTreeMap::new(), &limits, &mut snapshot).unwrap();
    assert_eq!(
        result.edge_expansions, 5,
        "four distinct live candidate IDs plus one ghost; duplicate IDs count once per visit"
    );
    assert_eq!(result.rows.len(), 2);
    assert_eq!(snapshot.edges, 5);
    let before = cache.clone();
    let query = parse("MATCH (a:Anchor) CALL (a) { MATCH (a)-[:LINK]->(b) RETURN count(b) AS n } CALL (a) { MATCH (a)-[:LINK]->(c) RETURN count(c) AS m } RETURN n,m").unwrap();
    let limits = Limits {
        max_edge_expansions: 9,
        ..Limits::default()
    };
    assert!(
        execute_with_storage(&mut cache, &query, &BTreeMap::new(), &limits, &mut snapshot)
            .unwrap_err()
            .to_string()
            .contains("edge expansion budget")
    );
    assert_eq!(cache, before);
}
#[test]
fn protected_keys_separate_global_labels_nul_types_and_enforce_budgets() {
    assert_ne!(label_key(None), label_key(Some("")));
    assert!(!label_key(Some("A\0B")).starts_with(&label_key(Some("A"))));
    assert!(!adjacency_prefix(7, Some("L\0X")).starts_with(&adjacency_prefix(7, Some("L"))));
    assert!(adjacency_prefix(7, Some("L")).starts_with(&adjacency_prefix(7, None)));
    let source = sample();
    let entries = AccessEntries::from_graph(&source.graph, 1 << 20).unwrap();
    assert_eq!(entries.outgoing.len(), 4);
    assert_eq!(entries.incoming.len(), 4);
    assert_eq!(
        entries
            .nodes
            .iter()
            .filter(|(k, _)| k == &label_key(None))
            .count(),
        3
    );
    assert!(AccessEntries::from_graph(&source.graph, 1).is_err());
    let mut graph = Graph::new();
    graph
        .add_node(BTreeSet::from(["x".repeat(4090)]), BTreeMap::new())
        .unwrap();
    assert!(AccessEntries::from_graph(&graph, 1 << 20).is_err());
}

fn corpus(source: &Graph) -> bicdb_graph::GraphCorpus {
    let empty = storage::StorageImage::decode(BTreeMap::new(), &Limits::default())
        .unwrap()
        .1;
    let rows = empty
        .patch(&Graph::new(), source, &Limits::default())
        .unwrap()
        .inserted;
    bicdb_graph::GraphCorpus {
        next_id: source.allocator_high_water(),
        nodes: source.nodes().len() as u64,
        edges: source.edges().len() as u64,
        logical_bytes: source.to_bytes().unwrap().len() as u64,
        record_bytes: rows.values().map(|v| v.len() as u64).sum(),
    }
}
fn partial_write(
    cache: &mut Graph,
    snapshot: &mut Snapshot,
    text: &str,
    limits: &Limits,
) -> Result<bicdb_graph::QueryResult, Error> {
    let corpus = corpus(&snapshot.graph);
    bicdb_graph::execute_with_storage_write_deadline(
        cache,
        &parse(text).unwrap(),
        &BTreeMap::new(),
        limits,
        snapshot,
        corpus,
        bicdb_graph::Deadline::for_limits(limits),
    )
}
#[test]
fn partial_point_write_counts_full_corpus_without_hydrating_untouched_entities() {
    let mut source = sample();
    let mut cache = cache(&source.graph);
    let baseline = corpus(&source.graph);
    let result = partial_write(
        &mut cache,
        &mut source,
        "MATCH (n:Entity {name:'a'}) SET n.name='new' RETURN n.name",
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(cache.nodes().len(), 1);
    assert!(cache.edges().is_empty());
    assert_eq!(source.nodes, 2);
    assert_eq!(result.changes.nodes().len(), 1);
    let mut complete = source.graph.clone();
    execute(
        &mut complete,
        &parse("MATCH (n:Entity {name:'a'}) SET n.name='new'").unwrap(),
        &BTreeMap::new(),
        &Limits::default(),
    )
    .unwrap();
    let measured = baseline
        .changed(&result.changes, &cache, &Limits::default())
        .unwrap();
    assert_eq!(measured.nodes, 3);
    assert_eq!(measured.edges, 4);
    assert_eq!(
        measured.logical_bytes,
        complete.to_bytes().unwrap().len() as u64
    );
}
#[test]
fn partial_overlay_sees_created_edges_and_never_resurrects_deleted_source() {
    let mut source = sample();
    let mut staged = cache(&source.graph);
    let r=partial_write(&mut staged,&mut source,"MATCH (a:Anchor),(b:Entity {name:'b'}) CREATE (a)-[:NEW]->(b) WITH a MATCH (a)-[r:NEW]->(b) RETURN count(r)",&Limits::default()).unwrap();
    assert_eq!(r.rows[0][0].to_json(&staged), serde_json::json!(1));
    let mut source = sample();
    let mut staged = cache(&source.graph);
    let r = partial_write(
        &mut staged,
        &mut source,
        "MATCH (a:Anchor) DETACH DELETE a WITH 1 AS x MATCH (n) RETURN count(n)",
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(r.rows[0][0].to_json(&staged), serde_json::json!(2));
    assert!(!staged.nodes().contains_key(&1));
    assert!(staged.edges().is_empty());
    assert_eq!(r.changes.edges().len(), 4);
    let mut source = sample();
    let mut staged = cache(&source.graph);
    let before = staged.clone();
    let error = partial_write(
        &mut staged,
        &mut source,
        "MATCH (a:Anchor) DELETE a",
        &Limits::default(),
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("still has relationships"),
        "{error}"
    );
    assert_eq!(staged, before);
}
#[test]
fn partial_global_limits_and_failed_cache_imports_preserve_statement_input() {
    let mut source = sample();
    let mut staged = cache(&source.graph);
    let before = staged.clone();
    let limit = Limits {
        max_nodes: 3,
        ..Limits::default()
    };
    let error = partial_write(&mut staged, &mut source, "CREATE (n:Entity)", &limit).unwrap_err();
    assert!(
        error.to_string().contains("global graph element"),
        "{error}"
    );
    assert_eq!(staged, before);
    let limit = Limits {
        max_text_bytes: corpus(&source.graph).record_bytes as usize + 100,
        ..Limits::default()
    };
    let growth =
        (corpus(&source.graph).record_bytes - corpus(&source.graph).logical_bytes + 200) as usize;
    let error = partial_write(
        &mut staged,
        &mut source,
        &format!("MATCH (a:Anchor) SET a.body='{}'", "x".repeat(growth)),
        &limit,
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("global graph storage"),
        "{error}"
    );
    assert_eq!(staged, before);
    assert!(partial_write(
        &mut staged,
        &mut source,
        "MATCH (a:Anchor) SET a:X WITH a RETURN 1/0",
        &Limits::default()
    )
    .is_err());
    assert_eq!(staged, before);
    let r = execute_with_storage(
        &mut staged,
        &parse("MATCH (a:Anchor) RETURN a.name").unwrap(),
        &BTreeMap::new(),
        &Limits::default(),
        &mut source,
    )
    .unwrap();
    assert_eq!(r.rows[0][0].to_json(&staged), serde_json::json!("a"));
}
#[test]
fn partial_patch_matches_complete_native_delta_for_growth_detach_and_allocator_digits() {
    for text in [
        "MATCH (a:Anchor) SET a.name='grownvalue'",
        "MATCH (a:Anchor) DETACH DELETE a",
        "CREATE (n:Z {name:'new'})",
        "MATCH (a:Anchor),(b:Entity {name:'b'}) CREATE (a)-[:NEW]->(b)",
    ] {
        let mut source = sample();
        let baseline = corpus(&source.graph);
        let empty = storage::StorageImage::decode(BTreeMap::new(), &Limits::default())
            .unwrap()
            .1;
        let all = empty
            .patch(&Graph::new(), &source.graph, &Limits::default())
            .unwrap()
            .inserted;
        let (_, full) = storage::StorageImage::from_adjacency(
            all.iter()
                .filter(|(k, _)| **k >> 60 == 1)
                .map(|(k, v)| (*k, v.clone()))
                .collect(),
            source.graph.edges().values().cloned(),
            all[&0].clone(),
            &Limits::default(),
        )
        .unwrap();
        let mut staged = cache(&source.graph);
        staged.reserve_ids(100000, 200000).unwrap();
        let r = partial_write(&mut staged, &mut source, text, &Limits::default()).unwrap();
        let selected = all
            .iter()
            .filter(|(k, _)| {
                **k >> 60 == 1
                    && (staged
                        .nodes()
                        .contains_key(&((**k & ((1 << 60) - 1)) >> 10))
                        || r.changes
                            .nodes()
                            .contains_key(&((**k & ((1 << 60) - 1)) >> 10)))
            })
            .map(|(k, v)| (*k, v.clone()))
            .collect();
        let partial = storage::StorageImage::partial_adjacency(
            selected,
            source.graph.edges().values().cloned(),
            baseline,
            &Limits::default(),
        )
        .unwrap();
        assert!(partial.is_partial());
        assert!(partial
            .patch(&source.graph, &staged, &Limits::default())
            .is_err());
        let actual = partial
            .patch_changes(&r.changes, &staged, &Limits::default())
            .unwrap();
        let mut complete = source.graph.clone();
        complete.reserve_ids(100000, 200000).unwrap();
        let delta = execute(
            &mut complete,
            &parse(text).unwrap(),
            &BTreeMap::new(),
            &Limits::default(),
        )
        .unwrap();
        let expected = full
            .patch_changes(&delta.changes, &complete, &Limits::default())
            .unwrap();
        assert_eq!(actual.inserted, expected.inserted, "{text}");
        assert_eq!(actual.updated, expected.updated, "{text}");
        assert_eq!(actual.removed, expected.removed, "{text}");
        assert_eq!(
            actual.after_record_bytes, expected.after_record_bytes,
            "{text}"
        );
        let summary = actual.after_corpus.unwrap();
        assert_eq!(
            summary.logical_bytes,
            complete.to_bytes().unwrap().len() as u64
        );
        assert_eq!(summary.nodes, complete.nodes().len() as u64);
        assert_eq!(summary.edges, complete.edges().len() as u64);
    }
}
