use bicdb_graph::property_index::{
    EntityKind, IndexProvider, IndexRequest, PropertyIndex, PropertyPredicate, SeekOp,
};
use bicdb_graph::{execute, execute_with_indexes, parse, Graph, Limits, Value};
use std::collections::{BTreeMap, BTreeSet};

fn definition(fields: &[&[&str]]) -> PropertyIndex {
    PropertyIndex {
        entity: EntityKind::Node,
        label: Some("Entity".into()),
        fields: fields
            .iter()
            .map(|f| f.iter().map(|s| s.to_string()).collect())
            .collect(),
        unique: false,
    }
}
struct Fixture {
    index: PropertyIndex,
    entries: Vec<(Vec<u8>, u64)>,
    seeks: usize,
}
impl IndexProvider for Fixture {
    fn candidates(
        &mut self,
        request: &IndexRequest,
    ) -> Result<Option<Vec<u64>>, bicdb_graph::Error> {
        let Some(seek) = self.index.seek(request) else {
            return Ok(None);
        };
        self.seeks += 1;
        Ok(Some(
            self.entries
                .iter()
                .filter(|(key, _)| seek.ranges.iter().any(|r| r.contains(key)))
                .map(|(_, id)| *id)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
        ))
    }
}
fn sample() -> Graph {
    let mut g = Graph::new();
    execute(&mut g,&parse("CREATE (a:Entity {db_type:'2123',name:'alpha',config:{port:10}}),(b:Entity {db_type:'2123',name:'beta',config:{port:20}}),(c:Entity {db_type:'other',name:'alpha'}),(d:Entity {db_type:'2123'}),(a)-[:LINK {weight:1}]->(b),(b)-[:OTHER {weight:1}]->(c)").unwrap(),&BTreeMap::new(),&Limits::default()).unwrap();
    g
}
#[test]
fn composite_json_seek_matches_scan_and_keeps_missing_trailing_fields() {
    let g = sample();
    let index = definition(&[&["db_type"], &["config", "port"]]);
    let mut fixture = Fixture {
        entries: index.entries(&g).unwrap(),
        index,
        seeks: 0,
    };
    for source in [
        "MATCH (n:Entity {db_type:'2123'}) RETURN id(n) AS id ORDER BY id",
        "MATCH (n:Entity) WHERE n.db_type='2123' AND n.config.port>=10 AND n.config.port<20 RETURN n.name AS name",
        "MATCH (n:Entity) WHERE n.db_type='2123' AND 20<=n.config['port'] RETURN n.name AS name",
        "OPTIONAL MATCH (n:Entity {db_type:'absent'}) RETURN n.name AS name",
        "UNWIND ['2123','other'] AS domain MATCH (n:Entity {db_type:domain}) RETURN n.name AS name ORDER BY name",
    ] {
        let q=parse(source).unwrap();let params=BTreeMap::new();let limits=Limits::default();
        let baseline=execute(&mut g.clone(),&q,&params,&limits).unwrap();
        let indexed=execute_with_indexes(&mut g.clone(),&q,&params,&limits,&mut fixture).unwrap();
        assert_eq!(indexed.rows,baseline.rows,"{source}");
    }
    assert_eq!(fixture.seeks, 6);
}
#[test]
fn mutation_overlay_and_relationship_type_union_remain_complete() {
    let g = sample();
    let index = definition(&[&["name"]]);
    let mut f = Fixture {
        entries: index.entries(&g).unwrap(),
        index,
        seeks: 0,
    };
    let q=parse("MATCH (n:Entity {name:'alpha'}) SET n.name='new' WITH count(n) AS changed MATCH (x:Entity {name:'new'}) RETURN count(x) AS n").unwrap();
    let result = execute_with_indexes(
        &mut g.clone(),
        &q,
        &BTreeMap::new(),
        &Limits::default(),
        &mut f,
    )
    .unwrap();
    assert_eq!(result.rows, vec![vec![Value::integer(2)]]);
    assert_eq!(f.seeks, 1);
    let index = PropertyIndex {
        entity: EntityKind::Relationship,
        label: Some("LINK".into()),
        fields: vec![vec!["weight".into()]],
        unique: false,
    };
    let mut f = Fixture {
        entries: index.entries(&g).unwrap(),
        index,
        seeks: 0,
    };
    let q = parse("MATCH ()-[r:LINK|OTHER]->() WHERE r.weight=1 RETURN count(r) AS n").unwrap();
    assert_eq!(
        execute_with_indexes(
            &mut g.clone(),
            &q,
            &BTreeMap::new(),
            &Limits::default(),
            &mut f
        )
        .unwrap()
        .rows,
        vec![vec![Value::integer(2)]]
    );
    assert_eq!(
        f.seeks, 0,
        "a single-type tree cannot answer an OR of relationship types"
    );
    let q = parse("MATCH ()-[r:LINK]->() WHERE r.weight=1 RETURN count(r) AS n").unwrap();
    assert_eq!(
        execute_with_indexes(
            &mut g.clone(),
            &q,
            &BTreeMap::new(),
            &Limits::default(),
            &mut f
        )
        .unwrap()
        .rows,
        vec![vec![Value::integer(1)]]
    );
    assert_eq!(f.seeks, 1);
}
#[test]
fn typed_decimal_nul_and_string_prefix_keys_have_exact_boundaries() {
    let mut g = Graph::new();
    for number in [
        "-99999999999999999999",
        "-1.5",
        "-0.001",
        "0",
        "0.0001",
        "1.5",
        "9007199254740993",
    ] {
        let value = serde_json::from_str(number).unwrap();
        g.add_node(
            BTreeSet::from(["Entity".into()]),
            BTreeMap::from([("value".into(), value)]),
        )
        .unwrap();
    }
    let index = definition(&[&["value"]]);
    let entries = index.entries(&g).unwrap();
    assert_eq!(
        entries.iter().map(|(_, id)| *id).collect::<Vec<_>>(),
        (1..=7).collect::<Vec<_>>()
    );
    let mut fixture = Fixture {
        index,
        entries,
        seeks: 0,
    };
    let q = parse(
        "MATCH (n:Entity) WHERE n.value>-0.001 AND n.value<=1.5 RETURN n.value AS v ORDER BY v",
    )
    .unwrap();
    assert_eq!(
        execute_with_indexes(
            &mut g.clone(),
            &q,
            &BTreeMap::new(),
            &Limits::default(),
            &mut fixture
        )
        .unwrap()
        .rows,
        execute(&mut g, &q, &BTreeMap::new(), &Limits::default())
            .unwrap()
            .rows
    );
    let mut g = Graph::new();
    for text in ["a", "a\0", "a\0x", "ab", "数据库"] {
        g.add_node(
            BTreeSet::from(["Entity".into()]),
            BTreeMap::from([("value".into(), serde_json::json!(text))]),
        )
        .unwrap();
    }
    let index = definition(&[&["value"]]);
    let entries = index.entries(&g).unwrap();
    assert_eq!(
        entries.iter().map(|(_, id)| *id).collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5]
    );
    let request = IndexRequest {
        entity: EntityKind::Node,
        labels: vec!["Entity".into()],
        predicates: vec![PropertyPredicate {
            field: vec!["value".into()],
            op: SeekOp::Equal,
            value: Value::String("a\0".into()),
        }],
    };
    let seek = index.seek(&request).unwrap();
    assert_eq!(
        entries
            .iter()
            .filter(|(key, _)| seek.ranges[0].contains(key))
            .map(|(_, id)| *id)
            .collect::<Vec<_>>(),
        vec![2]
    );
}
#[test]
fn unique_nulls_invalid_shapes_and_mixed_range_types_are_explicit() {
    let mut g = sample();
    let mut index = definition(&[&["name"]]);
    index.unique = true;
    assert!(index.entries(&g).is_err());
    index = definition(&[&["config", "port"]]);
    index.unique = true;
    assert_eq!(index.entries(&g).unwrap().len(), 4);
    g.add_node(
        BTreeSet::from(["Entity".into()]),
        BTreeMap::from([("config".into(), serde_json::json!(5))]),
    )
    .unwrap();
    assert!(index.entries(&g).is_err());
    let mut g = sample();
    g.add_node(
        BTreeSet::from(["Entity".into()]),
        BTreeMap::from([("config".into(), serde_json::json!({"port":"wrong type"}))]),
    )
    .unwrap();
    let index = definition(&[&["config", "port"]]);
    let mut f = Fixture {
        entries: index.entries(&g).unwrap(),
        index,
        seeks: 0,
    };
    let q = parse("MATCH (n:Entity) WHERE n.config.port>=10 RETURN n").unwrap();
    let expected = execute(&mut g.clone(), &q, &BTreeMap::new(), &Limits::default()).unwrap_err();
    let actual =
        execute_with_indexes(&mut g, &q, &BTreeMap::new(), &Limits::default(), &mut f).unwrap_err();
    assert_eq!(expected, actual);
    let bytes = f.index.to_bytes().unwrap();
    assert_eq!(PropertyIndex::from_bytes(&bytes).unwrap(), f.index);
    assert!(PropertyIndex::from_bytes(b"{}").is_err());
    let mut malformed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    malformed.as_object_mut().unwrap().remove("label");
    malformed["unexpected"] = serde_json::Value::Null;
    assert!(
        PropertyIndex::from_bytes(&serde_json::to_vec(&malformed).unwrap()).is_err(),
        "missing label must not silently widen index coverage"
    );
    assert!(
        f.index.entries_with_budget(&g, 1).is_err(),
        "planning budget must fail explicitly"
    );
}

#[test]
fn index_pruning_preserves_error_order_and_null_short_circuit() {
    let mut g = sample();
    // The composite equality prefix would exclude this row, but its earlier
    // range comparison must still fail. A NULL prefix also doesn't short circuit.
    execute(&mut g, &parse("CREATE (:Entity {db_type:'other',config:{port:'bad'}}),(:Entity {config:{port:'bad'}})").unwrap(), &BTreeMap::new(), &Limits::default()).unwrap();
    let index = definition(&[&["db_type"], &["config", "port"]]);
    let mut f = Fixture {
        entries: index.entries(&g).unwrap(),
        index,
        seeks: 0,
    };
    for source in [
        "MATCH (n:Entity) WHERE n.config.port>5 AND n.db_type='2123' RETURN n",
        "MATCH (n:Entity) WHERE n.db_type='2123' AND n.config.port>5 RETURN n",
        "MATCH (n:Entity {name:1/0,db_type:'absent'}) RETURN n",
        "MATCH (n:Entity),(x:Entity {name:1/0}) WHERE n.db_type='absent' RETURN n",
    ] {
        let q = parse(source).unwrap();
        let baseline =
            execute(&mut g.clone(), &q, &BTreeMap::new(), &Limits::default()).unwrap_err();
        let indexed = execute_with_indexes(
            &mut g.clone(),
            &q,
            &BTreeMap::new(),
            &Limits::default(),
            &mut f,
        )
        .unwrap_err();
        assert_eq!(indexed, baseline, "{source}");
    }
    assert_eq!(
        f.seeks, 0,
        "unsafe reordered expressions must fall back to original scan"
    );
    // A nested path outside the index may be malformed on an excluded row.
    let index = definition(&[&["db_type"]]);
    let mut f = Fixture {
        entries: index.entries(&g).unwrap(),
        index,
        seeks: 0,
    };
    g.add_node(
        BTreeSet::from(["Entity".into()]),
        BTreeMap::from([
            ("db_type".into(), serde_json::json!("other")),
            ("config".into(), serde_json::json!(4)),
        ]),
    )
    .unwrap();
    let q =
        parse("MATCH (n:Entity) WHERE n.config.port=5 AND n.db_type='absent' RETURN n").unwrap();
    assert_eq!(
        execute(&mut g.clone(), &q, &BTreeMap::new(), &Limits::default()).unwrap_err(),
        execute_with_indexes(&mut g, &q, &BTreeMap::new(), &Limits::default(), &mut f).unwrap_err()
    );
    assert_eq!(f.seeks, 0);
}
