use bicdb_graph::adjacency_record::{self as codec, Placement, SourceEntry};
use bicdb_graph::{Edge, Limits};
use bicdb_storage::adjacency as page;
use bicdb_storage::rowid::RowId;
use serde_json::json;
use std::collections::BTreeMap;

fn source() -> RowId {
    RowId::from_parts(3, 400, 1).unwrap()
}
fn target() -> RowId {
    RowId::from_parts(3, 401, 2).unwrap()
}
fn edge(label: &str, size: usize) -> Edge {
    Edge {
        id: 50,
        source: 10,
        target: 20,
        label: label.into(),
        properties: BTreeMap::from([
            (
                "config".into(),
                json!({"Question":"资源使用率高","items":[true,null,{"k":"v"}]}),
            ),
            ("text".into(), json!("x".repeat(size))),
            (
                "number".into(),
                serde_json::from_str("12345678901234567890.1234567890123456789").unwrap(),
            ),
        ]),
    }
}
fn assert_same(actual: &Edge, expected: &Edge) {
    assert_eq!(
        (actual.id, actual.source, actual.target),
        (expected.id, expected.source, expected.target)
    );
    assert_eq!(actual.label, expected.label);
    assert_eq!(actual.properties, expected.properties);
}
#[test]
fn inline_external_long_type_and_large_nested_properties_roundtrip_exactly() {
    let limits = Limits::default();
    for (label, size, placement) in [
        ("LINK".to_owned(), 10, Placement::Automatic),
        ("LINK".to_owned(), 10, Placement::External),
        ("关系".repeat(100), 10, Placement::Automatic),
        ("LINK".to_owned(), 200_000, Placement::Automatic),
        ("L".repeat(4080), 50, Placement::Automatic),
    ] {
        let expected = edge(&label, size);
        let encoded = codec::encode(&expected, source(), target(), placement, &limits).unwrap();
        let physical = page::decode(&encoded.record).unwrap();
        assert_eq!(physical.destination, target());
        assert_eq!(encoded.source, source());
        if label.len() > 255 {
            assert!(physical.label.is_empty());
        } else {
            assert_eq!(physical.label, label);
        }
        assert_same(
            &codec::decode(
                expected.id,
                source(),
                &encoded.record,
                &encoded.overflow,
                &limits,
            )
            .unwrap(),
            &expected,
        );
        assert_eq!(
            codec::overflow_range(expected.id, &encoded.record, &limits)
                .unwrap()
                .is_some(),
            !encoded.overflow.is_empty()
        );
        for row in encoded.overflow.values().skip(1) {
            let text = String::from_utf8_lossy(row);
            assert!(!text.contains("\"source\":10"));
            assert!(!text.contains("\"target\":20"));
        }
    }
}
#[test]
fn descriptor_and_chunk_corruption_and_cross_identity_are_rejected() {
    let limits = Limits::default();
    let expected = edge("LINK", 20_000);
    let encoded =
        codec::encode(&expected, source(), target(), Placement::Automatic, &limits).unwrap();
    assert!(codec::decode(51, source(), &encoded.record, &encoded.overflow, &limits).is_err());
    assert!(codec::decode(50, target(), &encoded.record, &encoded.overflow, &limits).is_err());
    let original = page::decode(&encoded.record).unwrap();
    let wrong_target = page::Edge {
        destination: source(),
        ..original
    }
    .encode()
    .unwrap();
    assert!(codec::decode(50, source(), &wrong_target, &encoded.overflow, &limits).is_err());
    let low = *encoded.overflow.first_key_value().unwrap().0;
    for case in 0..5 {
        let mut rows = encoded.overflow.clone();
        match case {
            0 => {
                rows.remove(&(low + 1));
            }
            1 => {
                rows.insert(low + 100, vec![0]);
            }
            2 => {
                rows.get_mut(&low).unwrap()[0] ^= 1;
            }
            3 => {
                rows.get_mut(&(low + 1)).unwrap()[0] ^= 1;
            }
            _ => {
                rows.get_mut(&(low + 1)).unwrap().pop();
            }
        }
        assert!(codec::decode(50, source(), &encoded.record, &rows, &limits).is_err());
    }
    for length in 0..encoded.record.len() {
        assert!(codec::overflow_range(50, &encoded.record[..length], &limits).is_err());
    }
    let physical = page::decode(&encoded.record).unwrap();
    for offset in [0, 8, 9, 15, 21, 25] {
        let mut payload = physical.payload.to_vec();
        payload[offset] ^= 255;
        let bad = page::Edge {
            payload: &payload,
            ..physical
        }
        .encode()
        .unwrap();
        assert!(codec::decode(50, source(), &bad, &encoded.overflow, &limits).is_err());
    }
    let mut inline = codec::encode(
        &edge("OTHER", 10),
        source(),
        target(),
        Placement::Automatic,
        &limits,
    )
    .unwrap();
    inline.overflow = encoded.overflow.clone();
    assert!(codec::decode(50, source(), &inline.record, &inline.overflow, &limits).is_err());
}
#[test]
fn external_fallback_shrinks_descriptor_and_budget_preflight_does_not_fetch() {
    let limits = Limits::default();
    let small = edge("LINK", 500);
    let big = edge("LINK", 30_000);
    let original =
        codec::encode(&small, source(), target(), Placement::Automatic, &limits).unwrap();
    let changed = codec::encode(&big, source(), target(), Placement::External, &limits).unwrap();
    assert!(changed.record.len() < original.record.len());
    let tight = Limits {
        max_text_bytes: 4096,
        ..limits
    };
    assert!(codec::overflow_range(50, &changed.record, &tight).is_err());
    assert!(codec::encode(&big, source(), target(), Placement::Automatic, &tight).is_err());
    assert!(codec::encode(
        &small,
        RowId::page_address(3, 400).unwrap(),
        target(),
        Placement::Automatic,
        &limits
    )
    .is_err());
    assert!(codec::encode(
        &edge("LINK", 4096 * 1023),
        source(),
        target(),
        Placement::External,
        &limits
    )
    .is_err());
}
#[test]
fn external_update_preserves_stable_edge_slot_when_inline_growth_cannot_fit() {
    let limits = Limits::default();
    let small = edge("LINK", 500);
    let larger = edge("LINK", 3000);
    let before = codec::encode(&small, source(), target(), Placement::Automatic, &limits).unwrap();
    let candidate =
        codec::encode(&larger, source(), target(), Placement::Automatic, &limits).unwrap();
    let external =
        codec::encode(&larger, source(), target(), Placement::External, &limits).unwrap();
    let mut block =
        bicdb_storage::page::Page::new(bicdb_storage::page::PageType::Adjacency, [1; 8], 3, 500);
    page::initialize(&mut block, source()).unwrap();
    let ordinal = page::append(&mut block, &before.record).unwrap();
    let filler = page::Edge {
        id: 51,
        destination: target(),
        flags: 0,
        itl_slot: 0xFF,
        label: "FILL",
        payload: &vec![0; 14000],
    }
    .encode()
    .unwrap();
    page::append(&mut block, &filler).unwrap();
    assert_eq!(
        page::update(&mut block, ordinal, &candidate.record),
        Err(page::AdjacencyError::PageFull)
    );
    page::update(&mut block, ordinal, &external.record).unwrap();
    page::validate(&block).unwrap();
    assert_same(
        &codec::decode(
            50,
            source(),
            page::record(&block, ordinal).unwrap(),
            &external.overflow,
            &limits,
        )
        .unwrap(),
        &larger,
    );
    let undo = bicdb_storage::undo::UndoRecord {
        prev: None,
        op: bicdb_storage::undo::UndoOp::Update,
        flags: 0,
        rowid: RowId::from_parts(3, 500, ordinal).unwrap(),
        payload: bicdb_storage::undo::UndoPayload::Update {
            old_itl_slot: 0xFF,
            patches: vec![(0, before.record.clone())],
        },
    };
    bicdb_storage::undo::apply_undo_to_page(&mut block, &undo).unwrap();
    assert_eq!(page::record(&block, ordinal).unwrap(), before.record);
    assert_same(
        &codec::decode(
            50,
            source(),
            page::record(&block, ordinal).unwrap(),
            &BTreeMap::new(),
            &limits,
        )
        .unwrap(),
        &small,
    );
}

#[test]
fn snapshot_entry_is_fixed_width_and_routes_are_ordered_and_collision_safe() {
    let entry = SourceEntry {
        source: source(),
        head: RowId::page_address(3, 600).unwrap(),
        tail: RowId::page_address(3, 650).unwrap(),
        last_id: 50,
    };
    let raw = entry.encode().unwrap();
    assert_eq!(raw.len(), 32);
    assert_eq!(SourceEntry::decode(&raw).unwrap(), entry);
    for length in 0..32 {
        assert!(SourceEntry::decode(&raw[..length]).is_err());
    }
    for bad in [
        SourceEntry {
            last_id: 0,
            ..entry
        },
        SourceEntry {
            head: source(),
            ..entry
        },
        SourceEntry {
            tail: RowId::page_address(4, 650).unwrap(),
            ..entry
        },
    ] {
        assert!(bad.encode().is_err());
    }
    assert_ne!(
        codec::source_ordinal(50).unwrap(),
        codec::overflow_range(
            50,
            &codec::encode(
                &edge("LINK", 5000),
                source(),
                target(),
                Placement::Automatic,
                &Limits::default()
            )
            .unwrap()
            .record,
            &Limits::default()
        )
        .unwrap()
        .unwrap()
        .0
    );
    assert!(codec::source_key(source()).unwrap() < codec::source_key(target()).unwrap());
    assert!(codec::locator_key(49).unwrap() < codec::locator_key(50).unwrap());
    assert!(codec::locator_key(0).is_err());
    assert!(codec::locator_key(1 << 48).is_err());
    for label in [
        "LINK".to_owned(),
        "A\0B".to_owned(),
        "L".repeat(4080),
        "\0".repeat(4080),
    ] {
        let prefix = codec::incoming_prefix(target(), Some(&label)).unwrap();
        let key = codec::incoming_key(target(), &label, source(), 50).unwrap();
        assert!(key.starts_with(&prefix));
        assert!(key.len() <= 4088);
        assert!(key.starts_with(&codec::incoming_prefix(target(), None).unwrap()));
    }
    assert_ne!(
        codec::incoming_prefix(target(), Some("A\0B")).unwrap(),
        codec::incoming_prefix(target(), Some("A")).unwrap()
    );
    assert_ne!(
        codec::incoming_prefix(target(), Some(&"L".repeat(4080))).unwrap(),
        codec::incoming_prefix(target(), Some(&"L".repeat(4081))).unwrap()
    );
}
