//! Named full-text results must compose with graph scopes and staged mutations.
use bicdb_graph::fulltext::{Channel, Definition, Generation, PathPart, SearchOptions, TextLimits};
use bicdb_graph::property_index::{
    EntityKind, FulltextHit, FulltextIndex, FulltextRequest, IndexProvider, IndexRequest,
};
use bicdb_graph::{execute, execute_with_indexes, parse, Error, Graph, Limits, Value};
use bicdb_types::Number;
use std::collections::BTreeMap;

fn graph() -> Graph {
    let mut graph = Graph::new();
    execute(&mut graph,&parse("CREATE (a:Entity {db_type:'d1',name:'buffer pool'}),(b:Entity {db_type:'d1',name:'buffer tuning'}),(x:Entity {db_type:'d2',name:'buffer pool'}),(a)-[:CAUSE]->(b)").unwrap(),&BTreeMap::new(),&Limits::default()).unwrap();
    graph
}
struct Native {
    source: Graph,
    generation: Generation,
    calls: usize,
    staged: usize,
    wrong_ids: Option<Vec<u64>>,
}
impl Native {
    fn new(source: &Graph) -> Self {
        let definition = Definition {
            entity: EntityKind::Node,
            labels: vec!["Entity".into()],
            fields: vec![vec![PathPart::Key("name".into())]],
        };
        Self {
            source: source.clone(),
            generation: Generation::build(definition, source, 1, 0, &TextLimits::default())
                .unwrap(),
            calls: 0,
            staged: 0,
            wrong_ids: None,
        }
    }
}
impl IndexProvider for Native {
    fn fulltext_indexes(&mut self) -> Result<Vec<FulltextIndex>, Error> {
        Ok(vec![FulltextIndex {
            name: "words".into(),
            definition: self.generation.definition().clone(),
        }])
    }
    fn candidates(&mut self, _: &IndexRequest) -> Result<Option<Vec<u64>>, Error> {
        Ok(None)
    }
    fn fulltext(
        &mut self,
        request: &FulltextRequest,
        staged: Option<&Graph>,
    ) -> Result<Vec<FulltextHit>, Error> {
        self.calls += 1;
        assert_eq!(request.entity, EntityKind::Node);
        assert_eq!(request.index, "words");
        if let Some(ids) = &self.wrong_ids {
            return Ok(ids
                .iter()
                .map(|id| FulltextHit {
                    id: *id,
                    columns: BTreeMap::new(),
                })
                .collect());
        }
        let Value::String(domain) = &request.options["db_type"] else {
            panic!("domain")
        };
        let replacement;
        let generation = if let Some(graph) = staged {
            self.staged += 1;
            replacement = Generation::build(
                self.generation.definition().clone(),
                graph,
                1,
                0,
                &TextLimits::default(),
            )?;
            &replacement
        } else {
            &self.generation
        };
        let source = staged.unwrap_or(&self.source);
        generation
            .search(
                &request.query,
                &SearchOptions {
                    domain: domain.clone(),
                    fields: vec![],
                    channel: Channel::Terms,
                },
                &TextLimits::default(),
                |id| {
                    source
                        .nodes()
                        .get(&id)
                        .map(bicdb_graph::fulltext::node_revision)
                        .transpose()
                },
            )?
            .into_iter()
            .map(|hit| {
                Ok(FulltextHit {
                    id: hit.id,
                    columns: BTreeMap::from([
                        (
                            "score".into(),
                            Value::Number(Number::parse(&hit.score.to_string()).unwrap()),
                        ),
                        (
                            "mode".into(),
                            Value::String(
                                if staged.is_some() {
                                    "strict_staged_scan"
                                } else {
                                    "FULLTEXT_BTREE"
                                }
                                .into(),
                            ),
                        ),
                        ("complete".into(), Value::Bool(true)),
                    ]),
                })
            })
            .collect()
    }
}
fn run(g: &mut Graph, p: &mut Native, q: &str) -> bicdb_graph::QueryResult {
    execute_with_indexes(
        g,
        &parse(q).unwrap(),
        &BTreeMap::new(),
        &Limits::default(),
        p,
    )
    .unwrap()
}
#[test]
fn fulltext_yield_where_match_correlated_call_and_union_preserve_scope() {
    let mut g = graph();
    let mut p = Native::new(&g);
    let r=run(&mut g,&mut p,"CALL db.index.fulltext.queryNodes('words','buffer',{db_type:'d1'}) YIELD node AS n,score,rank,mode WHERE score>0 MATCH (n)-[:CAUSE]->(m) RETURN n.name AS name,m.name AS target,rank,mode");
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], Value::String("buffer pool".into()));
    assert_eq!(r.rows[0][1], Value::String("buffer tuning".into()));
    assert_eq!(r.rows[0][3], Value::String("FULLTEXT_BTREE".into()));
    let r=run(&mut g,&mut p,"UNWIND ['pool','tuning'] AS term CALL (term) { CALL db.index.fulltext.queryNodes('words',term,{db_type:'d1'}) YIELD node RETURN node.name AS name } RETURN term,name ORDER BY term");
    assert_eq!(r.rows.len(), 2);
    let r=run(&mut g,&mut p,"CALL db.index.fulltext.queryNodes('words','pool',{db_type:'d1'}) YIELD node RETURN node.name AS name UNION CALL db.index.fulltext.queryNodes('words','tuning',{db_type:'d1'}) YIELD node RETURN node.name AS name");
    assert_eq!(r.rows.len(), 2);
    assert_eq!(p.calls, 5);
}
#[test]
fn staged_fulltext_reads_see_current_statement_and_failed_queries_leave_graph_unchanged() {
    let mut g = graph();
    let mut p = Native::new(&g);
    let r=run(&mut g,&mut p,"MATCH (n:Entity {name:'buffer pool',db_type:'d1'}) SET n.name='freshword' CALL db.index.fulltext.queryNodes('words','freshword',{db_type:'d1',consistency:'eventual'}) YIELD node,mode RETURN node.name AS name,mode");
    assert_eq!(
        r.rows,
        vec![vec![
            Value::String("freshword".into()),
            Value::String("strict_staged_scan".into())
        ]]
    );
    assert_eq!(p.staged, 1);
    let before = g.to_bytes().unwrap();
    let q=parse("CREATE (:Entity {db_type:'d1',name:'freshword'}) CALL db.index.fulltext.queryNodes('words','freshword',{db_type:'d1'}) YIELD node RETURN node").unwrap();
    let limits = Limits {
        max_rows: 1,
        ..Limits::default()
    };
    assert!(execute_with_indexes(&mut g, &q, &BTreeMap::new(), &limits, &mut p).is_err());
    assert_eq!(g.to_bytes().unwrap(), before);
}
#[test]
fn static_validation_and_provider_freshness_fail_closed() {
    let mut g = graph();
    let mut p = Native::new(&g);
    for q in [
        "CALL db.index.fulltext.queryNodes('words','buffer',{db_type:'d1'}) YIELD relationship RETURN relationship",
        "CALL db.index.fulltext.queryNodes('words','buffer',{db_type:'d1'}) YIELD node AS n,score AS n RETURN n",
        "WITH 1 AS n CALL db.index.fulltext.queryNodes('words','buffer',{db_type:'d1'}) YIELD node AS n RETURN n",
        "CALL db.index.fulltext.queryNodes('words','buffer',{db_type:'d1'}) YIELD node WHERE missing>0 RETURN node",
    ] {
        assert!(execute_with_indexes(&mut g,&parse(q).unwrap(),&BTreeMap::new(),&Limits::default(),&mut p).is_err(),"{q}");
    }
    assert_eq!(p.calls, 0);
    assert!(
        parse("CALL db.index.fulltext.queryNodes('words','buffer') YIELD node RETURN node")
            .is_err()
    );
    assert!(parse("CALL db.index.fulltext.unknown('x') YIELD node RETURN node").is_err());
    let q = parse(
        "CALL db.index.fulltext.queryNodes('words','buffer',{db_type:'d1'}) YIELD node RETURN node",
    )
    .unwrap();
    assert!(execute(&mut g, &q, &BTreeMap::new(), &Limits::default()).is_err());
    for ids in [vec![3], vec![1, 1], vec![0], vec![999]] {
        p.wrong_ids = Some(ids);
        assert!(
            execute_with_indexes(&mut g, &q, &BTreeMap::new(), &Limits::default(), &mut p).is_err()
        );
    }
}

#[test]
fn indexed_search_nodes_checks_provider_scope_and_preserves_staged_reads() {
    let mut g = graph();
    let mut p = Native::new(&g);
    let r=run(&mut g,&mut p,"CALL bicdb.searchNodes('buffer',{db_type:'d1',indexes:['words'],labels:['Entity'],consistency:'eventual'}) YIELD node,rank,mode WHERE rank>1 RETURN node.name AS name,mode");
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][1], Value::String("fulltext_round_robin".into()));
    let r=run(&mut g,&mut p,"MATCH (n:Entity {name:'buffer pool',db_type:'d1'}) SET n.name='newword' CALL bicdb.searchNodes('newword',{db_type:'d1',indexes:['words']}) YIELD node,mode RETURN node.name AS name,mode");
    assert_eq!(
        r.rows,
        vec![vec![
            Value::String("newword".into()),
            Value::String("strict_staged_round_robin".into())
        ]]
    );
    assert_eq!(p.staged, 1);
    let q = parse(
        "CALL bicdb.searchNodes('buffer',{db_type:'d1',indexes:['words']}) YIELD node RETURN node",
    )
    .unwrap();
    assert!(execute(&mut g, &q, &BTreeMap::new(), &Limits::default()).is_err());
    for ids in [vec![3], vec![1, 1], vec![999], vec![0]] {
        p.wrong_ids = Some(ids);
        assert!(
            execute_with_indexes(&mut g, &q, &BTreeMap::new(), &Limits::default(), &mut p).is_err()
        );
    }
    p.wrong_ids = Some(vec![1]);
    let before = g.to_bytes().unwrap();
    let q=parse("CREATE (:Entity {db_type:'d1',name:'rollbackword'}) CALL bicdb.searchNodes('buffer',{db_type:'d1',indexes:['words'],labels:['Forbidden']}) YIELD node RETURN node").unwrap();
    assert!(
        execute_with_indexes(&mut g, &q, &BTreeMap::new(), &Limits::default(), &mut p).is_err()
    );
    assert_eq!(g.to_bytes().unwrap(), before);
    let calls = p.calls;
    for options in [
        "{indexes:[]}",
        "{db_type:'d1',indexes:['words','words']}",
        "{db_type:'d1',indexes:['words'],limit:0}",
        "{db_type:'d1',indexes:['words'],labels:[1]}",
    ] {
        let q = parse(&format!(
            "CALL bicdb.searchNodes('buffer',{options}) YIELD node RETURN node"
        ))
        .unwrap();
        assert!(
            execute_with_indexes(&mut g, &q, &BTreeMap::new(), &Limits::default(), &mut p).is_err()
        );
    }
    assert_eq!(p.calls, calls, "invalid options fail before native access");
}

#[test]
fn named_fulltext_defensively_rechecks_requested_labels() {
    let mut g = graph();
    let mut p = Native::new(&g);
    p.wrong_ids = Some(vec![1]);
    let q=parse("CALL db.index.fulltext.queryNodes('words','buffer',{db_type:'d1',labels:['Forbidden']}) YIELD node RETURN node").unwrap();
    let e =
        execute_with_indexes(&mut g, &q, &BTreeMap::new(), &Limits::default(), &mut p).unwrap_err();
    assert!(e.to_string().contains("outside requested labels"));
}

#[test]
fn automatic_fulltext_requires_complete_field_and_or_label_coverage() {
    let mut g = graph();
    let mut p = Native::new(&g);
    let r=run(&mut g,&mut p,"CALL bicdb.searchNodes('buffer',{db_type:'d1',indexes:'auto',fields:['name'],labels:['Entity'],consistency:'eventual'}) YIELD node,mode RETURN node.name AS name,mode");
    assert_eq!(r.rows.len(), 2);
    assert_eq!(r.rows[0][1], Value::String("fulltext_round_robin".into()));
    let calls = p.calls;
    for options in [
        "{db_type:'d1',indexes:'auto',fields:['name']}",
        "{db_type:'d1',indexes:'auto',fields:['name'],labels:['Entity','Other']}",
        "{db_type:'d1',indexes:'auto',fields:['name','summary'],labels:['Entity']}",
        "{db_type:'d1',indexes:'auto',fields:[],labels:['Entity']}",
        "{db_type:'d1',indexes:'auto',labels:['Entity']}",
        "{db_type:'d1',indexes:'unknown',fields:['name'],labels:['Entity']}",
    ] {
        let q = parse(&format!(
            "CALL bicdb.searchNodes('buffer',{options}) YIELD node RETURN node"
        ))
        .unwrap();
        assert!(
            execute_with_indexes(&mut g, &q, &BTreeMap::new(), &Limits::default(), &mut p).is_err(),
            "{options}"
        );
    }
    assert_eq!(
        p.calls, calls,
        "coverage is proven for every stream before querying any source"
    );
    let q=parse("CALL bicdb.searchNodes('buffer',{db_type:'d1',indexes:'auto',fields:['name'],labels:['Entity']}) YIELD node RETURN node").unwrap();
    assert!(execute(&mut g, &q, &BTreeMap::new(), &Limits::default()).is_err());
}
