use bicdb_graph::property_index::{FulltextHit, FulltextRequest, IndexProvider, IndexRequest};
use bicdb_graph::{execute, execute_with_indexes_deadline, parse, Deadline, Graph, Limits};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

struct Slow {
    seeks: usize,
    texts: usize,
}
impl IndexProvider for Slow {
    fn candidates(
        &mut self,
        request: &IndexRequest,
    ) -> Result<Option<Vec<u64>>, bicdb_graph::Error> {
        if request.labels == ["Leaf"] {
            self.seeks += 1;
            std::thread::sleep(Duration::from_millis(60));
        }
        Ok(None)
    }
    fn fulltext(
        &mut self,
        _: &FulltextRequest,
        staged: Option<&Graph>,
    ) -> Result<Vec<FulltextHit>, bicdb_graph::Error> {
        assert!(
            staged.is_some(),
            "writes must already have reached the staged graph"
        );
        self.texts += 1;
        std::thread::sleep(Duration::from_millis(150));
        Ok(vec![])
    }
}
fn fixture() -> Graph {
    let mut graph = Graph::new();
    execute(
        &mut graph,
        &parse("CREATE (:Hub {name:'a'}),(:Leaf {name:'b'})").unwrap(),
        &BTreeMap::new(),
        &Limits::default(),
    )
    .unwrap();
    graph
}
#[test]
fn inherited_expired_deadline_cannot_restart_or_publish() {
    let deadline = Deadline::from_start(Instant::now() - Duration::from_secs(1), 100);
    assert!(deadline.tighten(60000).check("test").is_err());
    assert!(deadline.tighten(1).check("test").is_err());
    let mut graph = fixture();
    let before = graph.clone();
    let mut provider = Slow { seeks: 0, texts: 0 };
    let error = execute_with_indexes_deadline(
        &mut graph,
        &parse("CREATE (:Transient)").unwrap(),
        &BTreeMap::new(),
        &Limits::default(),
        &mut provider,
        deadline,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("time budget"), "{error}");
    assert_eq!(graph, before);
}
#[test]
fn call_exists_and_union_share_elapsed_time_across_slow_provider_calls() {
    for query in [
        "MATCH (a:Hub) CALL (a) { MATCH (b:Leaf) RETURN b.name AS n } CALL (a) { MATCH (c:Leaf) RETURN c.name AS m } RETURN n,m",
        "MATCH (a:Hub) WHERE EXISTS { MATCH (b:Leaf) } AND EXISTS { MATCH (c:Leaf) } RETURN a",
        "MATCH (b:Leaf) RETURN b.name AS n UNION ALL MATCH (c:Leaf) RETURN c.name AS n",
    ] {
        let mut graph=fixture();let before=graph.clone();let mut provider=Slow{seeks:0,texts:0};
        let error=execute_with_indexes_deadline(&mut graph,&parse(query).unwrap(),&BTreeMap::new(),&Limits::default(),&mut provider,Deadline::new(100)).unwrap_err().to_string();
        assert!(error.contains("time budget"),"{query}: {error}");
        assert_eq!(provider.seeks,2,"test must expire on the second provider call");assert_eq!(graph,before);
    }
}
#[test]
fn slow_provider_after_staged_mutation_does_not_publish_graph() {
    let mut graph = fixture();
    let before = graph.clone();
    let mut provider = Slow { seeks: 0, texts: 0 };
    let query=parse("CREATE (:Transient) WITH 1 AS x CALL db.index.fulltext.queryNodes('words','needle',{db_type:'d1',consistency:'eventual'}) YIELD node RETURN node").unwrap();
    let error = execute_with_indexes_deadline(
        &mut graph,
        &query,
        &BTreeMap::new(),
        &Limits::default(),
        &mut provider,
        Deadline::new(100),
    )
    .unwrap_err()
    .to_string();
    assert_eq!(provider.texts, 1);
    assert!(error.contains("time budget"), "{error}");
    assert_eq!(graph, before);
}
