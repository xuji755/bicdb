use bicdb_common::sha256::Sha256;
use bicdb_graph::corpus_proof::{
    Change, Contribution, EntityKey, Image, Patch, Reader, RecordKey, Root, Scope,
};
use bicdb_graph::{execute, parse, storage::StorageImage, Deadline, Graph, Limits};
use std::collections::{BTreeMap, BTreeSet};

fn scope() -> Scope {
    Scope {
        workspace: *b"proof-ws",
        graph: 99,
        routes: [7; 32],
    }
}
fn limits() -> Limits {
    Limits::default()
}
fn contributions(graph: &Graph) -> Vec<Contribution> {
    graph
        .nodes()
        .values()
        .map(|n| {
            let bytes = serde_json::to_vec(
                &serde_json::json!({"id":n.id,"labels":n.labels,"properties":n.properties}),
            )
            .unwrap()
            .len()
                + 32;
            Contribution::node(n, bytes).unwrap()
        })
        .chain(
            graph
                .edges()
                .values()
                .map(|e| Contribution::edge(e, &limits()).unwrap()),
        )
        .collect()
}
fn image(graph: &Graph, generation: u64) -> Image {
    Image::build(
        scope(),
        graph.allocator_high_water(),
        generation,
        contributions(graph),
        &limits(),
        Deadline::for_limits(&limits()),
    )
    .unwrap()
}
fn sample(n: usize) -> Graph {
    let mut graph = Graph::new();
    for n in 0..n {
        graph
            .add_node(
                BTreeSet::from(["N".into()]),
                BTreeMap::from([
                    ("name".into(), serde_json::json!(format!("n{n}"))),
                    ("body".into(), serde_json::json!("x".repeat(100))),
                ]),
            )
            .unwrap();
    }
    if n >= 3 {
        execute(&mut graph,&parse("MATCH (a:N {name:'n0'}),(b:N {name:'n1'}) CREATE (a)-[:LINK {weight:1}]->(b),(b)-[:BACK]->(a),(a)-[:SELF]->(a)").unwrap(),&BTreeMap::new(),&limits()).unwrap();
    }
    graph
}
fn apply(records: &mut BTreeMap<RecordKey, Vec<u8>>, patch: bicdb_graph::corpus_proof::Patch) {
    for key in patch.removed {
        assert!(!patch.written.contains_key(&key));
        records.remove(&key);
    }
    records.extend(patch.written);
}
fn reseal(bytes: &mut [u8]) {
    let end = bytes.len() - 32;
    let mut sha = Sha256::new();
    sha.update(&bytes[..end]);
    bytes[end..].copy_from_slice(&sha.finalize());
}
#[test]
fn cold_point_and_absence_proofs_read_bounded_shared_paths_in_2000_entity_corpus() {
    let graph = sample(2000);
    let source = image(&graph, 1);
    let mut fetched = BTreeSet::new();
    let bounded = Limits {
        max_expansions: 80,
        ..limits()
    };
    let mut reader = Reader::open(scope(), &bounded, Deadline::for_limits(&bounded), |key| {
        assert!(fetched.insert(key), "each record fetched once");
        Ok(source.records.get(&key).cloned())
    })
    .unwrap();
    let entity = contributions(&graph)[17];
    reader.verify_source(entity.key, Some(entity)).unwrap();
    reader.verify_source(EntityKey::node(9999), None).unwrap();
    let work = reader.work();
    reader.verify_source(entity.key, Some(entity)).unwrap();
    assert_eq!(reader.work(), work);
    assert!(work < 80, "{work}");
    assert!(reader.bytes() < 100_000);
    let mut updated = graph.clone();
    execute(
        &mut updated,
        &parse("MATCH (n:N {name:'n17'}) SET n.body='changedaftercold'").unwrap(),
        &BTreeMap::new(),
        &limits(),
    )
    .unwrap();
    let replacement = contributions(&updated)
        .into_iter()
        .find(|c| c.key == entity.key)
        .unwrap();
    let patch = reader
        .patch(
            &[Change {
                key: entity.key,
                before: Some(entity),
                after: Some(replacement),
            }],
            updated.allocator_high_water(),
            2,
        )
        .unwrap();
    assert_eq!(patch.root, image(&updated, 2).root);
    assert!(reader.work() <= 80, "{}", reader.work());
    drop(reader);
    assert!(fetched.len() < 25, "{}", fetched.len());
    assert_eq!(
        fetched
            .iter()
            .filter(|k| matches!(k, RecordKey::Bucket(_)))
            .count(),
        2
    );
}
#[test]
fn incremental_mixed_changes_match_complete_graph_native_bytes_and_fresh_proof() {
    let before = sample(50);
    let initial = image(&before, 3);
    let mut after = before.clone();
    let result=execute(&mut after,&parse("MATCH (a:N {name:'n0'}) DETACH DELETE a WITH 1 AS x CREATE (b:New {name:'new',nested:{a:[1,2,3]}}) WITH b MATCH (c:N {name:'n3'}) SET c.body='changed',c:Extra CREATE (b)-[:NEW {weight:8}]->(c)").unwrap(),&BTreeMap::new(),&limits()).unwrap();
    let old: BTreeMap<_, _> = contributions(&before)
        .into_iter()
        .map(|c| (c.key, c))
        .collect();
    let new: BTreeMap<_, _> = contributions(&after)
        .into_iter()
        .map(|c| (c.key, c))
        .collect();
    let changes: Vec<_> = result
        .changes
        .nodes()
        .keys()
        .map(|id| EntityKey::node(*id))
        .chain(result.changes.edges().keys().map(|id| EntityKey::edge(*id)))
        .map(|key| Change {
            key,
            before: old.get(&key).copied(),
            after: new.get(&key).copied(),
        })
        .collect();
    let mut reader = Reader::open(scope(), &limits(), Deadline::for_limits(&limits()), |key| {
        Ok(initial.records.get(&key).cloned())
    })
    .unwrap();
    let patch = reader
        .patch(&changes, after.allocator_high_water(), 4)
        .unwrap();
    assert_eq!(
        reader.root(),
        initial.root,
        "candidate does not mutate original snapshot"
    );
    let expected = image(&after, 4);
    assert_eq!(patch.root, expected.root);
    let mut records = initial.records.clone();
    apply(&mut records, patch);
    assert_eq!(records, expected.records);
    let corpus = expected.root.corpus(&limits()).unwrap();
    assert_eq!(corpus.logical_bytes, after.to_bytes().unwrap().len() as u64);
    let empty = StorageImage::decode(BTreeMap::new(), &limits()).unwrap().1;
    assert_eq!(
        corpus.record_bytes,
        empty.native_record_bytes(&after, &limits()).unwrap() as u64
    );
}
#[test]
fn independent_root_rejects_forged_manifest_and_cross_workspace_route_generation() {
    let graph = sample(5);
    let source = image(&graph, 9);
    let corpus = source.root.corpus(&limits()).unwrap();
    source
        .root
        .verify_manifest(scope(), 9, corpus, &limits())
        .unwrap();
    let mut forged = corpus;
    forged.logical_bytes += 1;
    assert!(source
        .root
        .verify_manifest(scope(), 9, forged, &limits())
        .unwrap_err()
        .to_string()
        .contains("differs from manifest"));
    assert!(source
        .root
        .verify_manifest(scope(), 10, corpus, &limits())
        .is_err());
    for other in [
        Scope {
            graph: 100,
            ..scope()
        },
        Scope {
            workspace: *b"other-ws",
            ..scope()
        },
        Scope {
            routes: [8; 32],
            ..scope()
        },
    ] {
        assert!(Root::decode(&source.records[&RecordKey::Root], other, &limits()).is_err());
    }
    let mut corrupted = source.records[&RecordKey::Root].clone();
    corrupted[91] ^= 1;
    assert!(Root::decode(&corrupted, scope(), &limits()).is_err());
    reseal(&mut corrupted);
    // Re-signing root aggregates alone cannot forge the unchanged entity tree.
    let mut records = source.records.clone();
    records.insert(RecordKey::Root, corrupted);
    let mut reader = Reader::open(scope(), &limits(), Deadline::for_limits(&limits()), |key| {
        Ok(records.get(&key).cloned())
    })
    .unwrap();
    assert!(reader.entity(EntityKey::node(1)).is_err());
}
#[test]
fn rechecksummed_missing_or_misrouted_buckets_and_hash_pages_cannot_prove_source() {
    let graph = sample(20);
    let source = image(&graph, 1);
    let wanted = contributions(&graph)[0];
    let mut trace = BTreeSet::new();
    let mut reader = Reader::open(scope(), &limits(), Deadline::for_limits(&limits()), |key| {
        trace.insert(key);
        Ok(source.records.get(&key).cloned())
    })
    .unwrap();
    reader.verify_source(wanted.key, Some(wanted)).unwrap();
    drop(reader);
    let bucket = *trace
        .iter()
        .find(|k| matches!(k, RecordKey::Bucket(_)))
        .unwrap();
    let page = *trace
        .iter()
        .find(|k| matches!(k, RecordKey::HashPage(_)) && source.records.contains_key(k))
        .unwrap();
    for key in [bucket, page] {
        for missing in [true, false] {
            let mut records = source.records.clone();
            if missing {
                records.remove(&key);
            } else {
                let data = records.get_mut(&key).unwrap();
                // Page zero's first entry is node 1, which is on every path.
                // Mutating an unrelated entry would not test this selection.
                let end = if matches!(key, RecordKey::HashPage(_)) {
                    65
                } else {
                    data.len() - 33
                };
                data[end] ^= 1;
                reseal(data);
            }
            let mut reader =
                Reader::open(scope(), &limits(), Deadline::for_limits(&limits()), |key| {
                    Ok(records.get(&key).cloned())
                })
                .unwrap();
            assert!(
                reader.verify_source(wanted.key, Some(wanted)).is_err(),
                "{key:?}, missing {missing}"
            );
        }
    }
    let mut records = source.records.clone();
    let moved = records.remove(&bucket).unwrap();
    let RecordKey::Bucket(b) = bucket else {
        unreachable!()
    };
    records.insert(RecordKey::Bucket((b + 1) % 4096), moved);
    let mut reader = Reader::open(scope(), &limits(), Deadline::for_limits(&limits()), |key| {
        Ok(records.get(&key).cloned())
    })
    .unwrap();
    assert!(reader.verify_source(wanted.key, Some(wanted)).is_err());
}
#[test]
fn canonical_digest_and_original_node_record_bytes_are_verified_independently() {
    let graph = sample(1);
    let mut entities = contributions(&graph);
    let canonical = entities[0];
    entities[0].record_bytes += 123;
    let original = entities[0];
    let initial = Image::build(
        scope(),
        graph.allocator_high_water(),
        1,
        entities,
        &limits(),
        Deadline::for_limits(&limits()),
    )
    .unwrap();
    let mut reader = Reader::open(scope(), &limits(), Deadline::for_limits(&limits()), |key| {
        Ok(initial.records.get(&key).cloned())
    })
    .unwrap();
    assert!(reader
        .verify_source(canonical.key, Some(canonical))
        .is_err());
    reader.verify_source(original.key, Some(original)).unwrap();
    let patch = reader
        .patch(
            &[Change {
                key: original.key,
                before: Some(original),
                after: Some(canonical),
            }],
            graph.allocator_high_water(),
            2,
        )
        .unwrap();
    assert_eq!(patch.root, image(&graph, 2).root);
    assert_eq!(
        patch.root.corpus(&limits()).unwrap().logical_bytes,
        initial.root.corpus(&limits()).unwrap().logical_bytes
    );
    assert_eq!(
        patch.root.corpus(&limits()).unwrap().record_bytes + 123,
        initial.root.corpus(&limits()).unwrap().record_bytes
    );
    let mut different = canonical;
    different.digest[0] ^= 1;
    assert!(reader
        .verify_source(canonical.key, Some(different))
        .is_err());
}
#[test]
fn patch_failure_and_old_snapshot_preserve_prior_proof_records() {
    let graph = sample(5);
    let initial = image(&graph, 1);
    let old = initial.records.clone();
    let original = contributions(&graph)[0];
    let mut changed = original;
    changed.digest[0] ^= 1;
    changed.canonical_bytes += 9;
    changed.record_bytes += 9;
    let mut reader = Reader::open(scope(), &limits(), Deadline::for_limits(&limits()), |key| {
        Ok(old.get(&key).cloned())
    })
    .unwrap();
    let patch = reader
        .patch(
            &[Change {
                key: original.key,
                before: Some(original),
                after: Some(changed),
            }],
            graph.allocator_high_water(),
            2,
        )
        .unwrap();
    let mut committed = old.clone();
    apply(&mut committed, patch);
    reader.verify_source(original.key, Some(original)).unwrap();
    assert_eq!(initial.records, old);
    let mut current = Reader::open(scope(), &limits(), Deadline::for_limits(&limits()), |key| {
        Ok(committed.get(&key).cloned())
    })
    .unwrap();
    current.verify_source(changed.key, Some(changed)).unwrap();
    assert!(current.verify_source(original.key, Some(original)).is_err());
    assert!(current
        .patch(
            &[Change {
                key: original.key,
                before: Some(original),
                after: None
            }],
            graph.allocator_high_water(),
            3
        )
        .is_err());
    assert_eq!(current.root().generation, 2);
    assert_eq!(
        committed[&RecordKey::Root],
        current.root().encode(&limits()).unwrap()
    );
    // Mixed-generation records cannot satisfy even a rechecksummed current root.
    let mut mixed = old;
    mixed.insert(RecordKey::Root, committed[&RecordKey::Root].clone());
    let mut mixed_reader =
        Reader::open(scope(), &limits(), Deadline::for_limits(&limits()), |key| {
            Ok(mixed.get(&key).cloned())
        })
        .unwrap();
    assert!(mixed_reader.entity(original.key).is_err());
}
#[test]
fn delete_all_prunes_empty_tree_and_allocator_digits_have_exact_header_deltas() {
    let graph = sample(3);
    let initial = image(&graph, 1);
    let changes: Vec<_> = contributions(&graph)
        .into_iter()
        .map(|c| Change {
            key: c.key,
            before: Some(c),
            after: None,
        })
        .collect();
    let mut reader = Reader::open(scope(), &limits(), Deadline::for_limits(&limits()), |key| {
        Ok(initial.records.get(&key).cloned())
    })
    .unwrap();
    let patch = reader.patch(&changes, 100000, 2).unwrap();
    let expected = Image::build(
        scope(),
        100000,
        2,
        [],
        &limits(),
        Deadline::for_limits(&limits()),
    )
    .unwrap();
    assert_eq!(patch.root, expected.root);
    let mut records = initial.records.clone();
    apply(&mut records, patch);
    assert_eq!(records, expected.records);
    let mut reader = Reader::open(scope(), &limits(), Deadline::for_limits(&limits()), |key| {
        Ok(records.get(&key).cloned())
    })
    .unwrap();
    reader.verify_source(EntityKey::node(1), None).unwrap();
    let patch = reader.patch(&[], 1000000, 3).unwrap();
    assert_eq!(
        patch.root.corpus(&limits()).unwrap().logical_bytes,
        expected.root.corpus(&limits()).unwrap().logical_bytes + 1
    );
    assert_eq!(
        patch.root.corpus(&limits()).unwrap().record_bytes,
        expected.root.corpus(&limits()).unwrap().record_bytes + 1
    );
    assert_eq!(patch.written.len(), 1);
    assert!(patch.removed.is_empty());
}
#[test]
fn global_source_work_byte_deadline_and_generation_bounds_fail_without_a_patch() {
    let graph = sample(20);
    let initial = image(&graph, 1);
    let tiny = Limits {
        max_text_bytes: 1024,
        ..limits()
    };
    assert!(
        Reader::open(scope(), &tiny, Deadline::for_limits(&tiny), |key| Ok(
            initial.records.get(&key).cloned()
        ))
        .is_err()
    );
    let bounded = Limits {
        max_expansions: 1,
        ..limits()
    };
    let mut reader = Reader::open(scope(), &bounded, Deadline::for_limits(&bounded), |key| {
        Ok(initial.records.get(&key).cloned())
    })
    .unwrap();
    assert!(reader
        .entity(EntityKey::node(1))
        .unwrap_err()
        .to_string()
        .contains("budget"));
    assert!(
        Reader::open(scope(), &limits(), Deadline::new(0), |key| Ok(initial
            .records
            .get(&key)
            .cloned()))
        .err()
        .unwrap()
        .to_string()
        .contains("time budget")
    );
    let mut reader = Reader::open(scope(), &limits(), Deadline::for_limits(&limits()), |key| {
        Ok(initial.records.get(&key).cloned())
    })
    .unwrap();
    assert!(reader.patch(&[], 1, 2).is_err());
    assert!(reader.patch(&[], graph.allocator_high_water(), 1).is_err());
    let max = Image::build(
        scope(),
        graph.allocator_high_water(),
        u64::MAX,
        contributions(&graph),
        &limits(),
        Deadline::for_limits(&limits()),
    )
    .unwrap();
    let mut reader = Reader::open(scope(), &limits(), Deadline::for_limits(&limits()), |key| {
        Ok(max.records.get(&key).cloned())
    })
    .unwrap();
    assert!(reader.patch(&[], graph.allocator_high_water(), 0).is_err());
}
#[test]
fn duplicate_entities_shared_identity_and_reused_allocator_holes_are_rejected() {
    let graph = sample(1);
    let entity = contributions(&graph)[0];
    assert!(Image::build(
        scope(),
        2,
        1,
        [entity, entity],
        &limits(),
        Deadline::for_limits(&limits())
    )
    .is_err());
    let initial = image(&graph, 1);
    let mut reader = Reader::open(scope(), &limits(), Deadline::for_limits(&limits()), |key| {
        Ok(initial.records.get(&key).cloned())
    })
    .unwrap();
    let change = Change {
        key: entity.key,
        before: Some(entity),
        after: None,
    };
    assert!(reader.patch(&[change, change], 2, 2).is_err());
    let mut node = entity;
    node.key = EntityKey::node(2);
    let mut edge = node;
    edge.key = EntityKey::edge(2);
    edge.record_bytes = edge.canonical_bytes + 32;
    assert!(reader
        .patch(
            &[
                Change {
                    key: node.key,
                    before: None,
                    after: Some(node)
                },
                Change {
                    key: edge.key,
                    before: None,
                    after: Some(edge)
                }
            ],
            3,
            2
        )
        .is_err());
    assert!(Image::build(
        scope(),
        3,
        1,
        [node, edge],
        &limits(),
        Deadline::for_limits(&limits())
    )
    .is_err());
    let empty = Image::build(
        scope(),
        10,
        1,
        [],
        &limits(),
        Deadline::for_limits(&limits()),
    )
    .unwrap();
    let mut reader = Reader::open(scope(), &limits(), Deadline::for_limits(&limits()), |key| {
        Ok(empty.records.get(&key).cloned())
    })
    .unwrap();
    assert!(reader
        .patch(
            &[Change {
                key: entity.key,
                before: None,
                after: Some(entity)
            }],
            10,
            2
        )
        .unwrap_err()
        .to_string()
        .contains("reuse"));
}

#[test]
fn physical_chunks_reject_missing_foreign_oversized_or_corrupted_records() {
    let key = RecordKey::Bucket(12);
    let data: Vec<_> = (0..9000).map(|n| (n % 251) as u8).collect();
    let good = key.native_rows(&data, &limits()).unwrap();
    assert_eq!(good.len(), 4);
    assert_eq!(
        key.from_native_rows(&good, &limits()).unwrap(),
        Some(data.clone())
    );
    assert_eq!(
        key.from_native_rows(&BTreeMap::new(), &limits()).unwrap(),
        None
    );
    let (base, end) = key.range().unwrap();
    assert!(end < i64::MAX as u64);
    assert_eq!(base >> 60, 5);
    let mut missing = good.clone();
    missing.remove(&(base + 2));
    assert!(key.from_native_rows(&missing, &limits()).is_err());
    let mut missing_checksum = good.clone();
    missing_checksum.remove(&base);
    assert!(key.from_native_rows(&missing_checksum, &limits()).is_err());
    let mut damaged = good.clone();
    damaged.get_mut(&(base + 2)).unwrap()[0] ^= 1;
    assert!(key.from_native_rows(&damaged, &limits()).is_err());
    let mut surplus = good.clone();
    surplus.insert(base + 4, vec![1]);
    assert!(key.from_native_rows(&surplus, &limits()).is_err());
    assert!(RecordKey::Bucket(13)
        .from_native_rows(&good, &limits())
        .is_err());
    assert!(RecordKey::Bucket(4096).range().is_err());
    assert!(RecordKey::HashPage(128).range().is_err());
    assert!(key.native_rows(&[], &limits()).is_err());
    let tiny = Limits {
        max_text_bytes: 8999,
        ..limits()
    };
    assert!(key.native_rows(&data, &tiny).is_err());
    assert!(key.from_native_rows(&good, &tiny).is_err());
    let source = BTreeMap::from([(key, good.clone())]);
    let shortened = Patch {
        root: image(&sample(1), 1).root,
        written: BTreeMap::from([(key, b"short".to_vec())]),
        removed: Vec::new(),
    }
    .native_patch(&source, &limits())
    .unwrap();
    assert!(shortened.inserted.is_empty());
    assert!(shortened.removed.is_empty());
    assert_eq!(shortened.updated.len(), good.len());
    let mut reused = good.clone();
    reused.extend(shortened.updated);
    assert_eq!(
        key.from_native_rows(&reused, &limits()).unwrap(),
        Some(b"short".to_vec())
    );
    let tombstone = Patch {
        root: image(&sample(1), 1).root,
        written: BTreeMap::new(),
        removed: vec![key],
    }
    .native_patch(&BTreeMap::from([(key, reused.clone())]), &limits())
    .unwrap();
    assert!(tombstone.inserted.is_empty());
    assert!(tombstone.removed.is_empty());
    reused.extend(tombstone.updated);
    assert_eq!(key.from_native_rows(&reused, &limits()).unwrap(), None);
    let mut records = BTreeMap::new();
    for key in [
        RecordKey::Root,
        RecordKey::Bucket(0),
        RecordKey::Bucket(4095),
        RecordKey::HashPage(0),
        RecordKey::HashPage(127),
    ] {
        for (key, data) in key.native_rows(b"payload", &limits()).unwrap() {
            assert!(records.insert(key, data).is_none());
        }
    }
}

#[test]
fn complete_recovery_validation_rejects_extra_noncanonical_and_mixed_proof_rows() {
    let graph = sample(100);
    let source = image(&graph, 1);
    let decoded = Image::decode(
        source.records.clone(),
        scope(),
        &limits(),
        Deadline::for_limits(&limits()),
    )
    .unwrap();
    assert_eq!(decoded.root, source.root);
    assert_eq!(decoded.records, source.records);
    let bucket = *source
        .records
        .keys()
        .find(|k| matches!(k, RecordKey::Bucket(_)))
        .unwrap();
    let page = *source
        .records
        .keys()
        .find(|k| matches!(k, RecordKey::HashPage(_)))
        .unwrap();
    for key in [RecordKey::Root, bucket, page] {
        let mut records = source.records.clone();
        records.remove(&key);
        assert!(
            Image::decode(records, scope(), &limits(), Deadline::for_limits(&limits())).is_err()
        );
    }
    let mut records = source.records.clone();
    records.insert(RecordKey::Bucket(4096), source.records[&bucket].clone());
    assert!(Image::decode(records, scope(), &limits(), Deadline::for_limits(&limits())).is_err());
    let mut records = source.records.clone();
    let data = records.get_mut(&page).unwrap();
    let end = data.len() - 33;
    data[end] ^= 1;
    reseal(data);
    assert!(Image::decode(records, scope(), &limits(), Deadline::for_limits(&limits())).is_err());
    let old = sample(1);
    let small = image(&old, 1);
    let mut records = source.records.clone();
    records.insert(RecordKey::Root, small.records[&RecordKey::Root].clone());
    assert!(Image::decode(records, scope(), &limits(), Deadline::for_limits(&limits())).is_err());
    // Empty initialization has no gratuitous complete-tree work.
    let bounded = Limits {
        max_expansions: 1,
        ..limits()
    };
    let empty = Image::build(scope(), 1, 1, [], &bounded, Deadline::for_limits(&bounded)).unwrap();
    assert_eq!(empty.records.len(), 1);
    assert!(empty.work <= 1);
}

#[test]
fn successive_snapshot_batches_preserve_canonical_proof_rows_and_corpus_sizes() {
    let mut graph = sample(30);
    let mut records = image(&graph, 1).records;
    for generation in 2..=14 {
        let before = graph.clone();
        let text=format!("MATCH (n:N {{name:'n3'}}) SET n.body='round{generation}' WITH n CREATE (a:Added {{round:{generation}}}),(a)-[:R {{round:{generation}}}]->(n)");
        let result = execute(
            &mut graph,
            &parse(&text).unwrap(),
            &BTreeMap::new(),
            &limits(),
        )
        .unwrap();
        let old: BTreeMap<_, _> = contributions(&before)
            .into_iter()
            .map(|c| (c.key, c))
            .collect();
        let new: BTreeMap<_, _> = contributions(&graph)
            .into_iter()
            .map(|c| (c.key, c))
            .collect();
        let changes: Vec<_> = result
            .changes
            .nodes()
            .keys()
            .map(|id| EntityKey::node(*id))
            .chain(result.changes.edges().keys().map(|id| EntityKey::edge(*id)))
            .map(|key| Change {
                key,
                before: old.get(&key).copied(),
                after: new.get(&key).copied(),
            })
            .collect();
        let mut reader = Reader::open(scope(), &limits(), Deadline::for_limits(&limits()), |key| {
            Ok(records.get(&key).cloned())
        })
        .unwrap();
        let patch = reader
            .patch(&changes, graph.allocator_high_water(), generation)
            .unwrap();
        drop(reader);
        let expected = image(&graph, generation);
        assert_eq!(patch.root, expected.root, "round {generation}");
        apply(&mut records, patch);
        assert_eq!(records, expected.records, "round {generation}");
        assert_eq!(
            expected.root.corpus(&limits()).unwrap().logical_bytes,
            graph.to_bytes().unwrap().len() as u64
        );
    }
}
