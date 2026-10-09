use bicdb_storage::adjacency::{self as a, AdjacencyError, Edge};
use bicdb_storage::page::{Page, PageType, SlotEntry, SlotStatus};
use bicdb_storage::rowid::RowId;
use bicdb_storage::undo::{apply_undo_to_page, UndoOp, UndoPayload, UndoRecord};

fn source() -> RowId {
    RowId::from_parts(3, 99, 1).unwrap()
}
fn page() -> Page {
    let mut page = Page::new(PageType::Adjacency, [3; 8], 4, 7);
    a::initialize(&mut page, source()).unwrap();
    page
}
fn bytes(id: u64, text: &[u8]) -> Vec<u8> {
    Edge {
        id,
        destination: source(),
        flags: 0,
        itl_slot: 0xFF,
        label: "关联",
        payload: text,
    }
    .encode()
    .unwrap()
}
fn undo(ordinal: u16, op: UndoOp, payload: UndoPayload) -> UndoRecord {
    UndoRecord {
        prev: None,
        op,
        flags: 0,
        rowid: if ordinal == 0 {
            RowId::page_address(4, 7).unwrap()
        } else {
            RowId::from_parts(4, 7, ordinal).unwrap()
        },
        payload,
    }
}

#[test]
fn canonical_layout_order_and_tombstones_preserve_nonreused_identity() {
    let mut p = page();
    assert_eq!(p.slot_directory_start(), 84);
    assert_eq!(&p.as_bytes()[68..74], &source().to_bytes());
    let raw = bytes((1 << 48) - 1, b"{}");
    assert_eq!(&raw[..6], &[255; 6]);
    assert_eq!(raw.len(), 17 + "关联".len() + 2);
    assert_eq!(a::append(&mut p, &bytes(10, b"first")).unwrap(), 1);
    a::append(&mut p, &bytes(20, b"second")).unwrap();
    assert_eq!(a::find(&p, 20).unwrap(), Some(2));
    a::delete(&mut p, 2).unwrap();
    assert_eq!(a::find(&p, 20).unwrap(), None);
    let before = *p.as_bytes();
    assert_eq!(
        a::append(&mut p, &bytes(19, b"reuse")),
        Err(AdjacencyError::IdentityOrder)
    );
    assert_eq!(p.as_bytes(), &before);
    assert_eq!(a::append(&mut p, &bytes(30, b"third")).unwrap(), 3);
    assert_eq!(a::header(&p).unwrap().live_edges, 2);
    assert_eq!(p.slot_count(), 3);
    a::validate(&p).unwrap();
    p.seal();
    assert_eq!(p.verify(), bicdb_storage::page::PageCheck::Ok);
}
#[test]
fn itl_growth_moves_body_and_directory_but_heap_cannot_write_adjacency() {
    let mut p = page();
    a::append(&mut p, &bytes(1, b"a")).unwrap();
    a::append(&mut p, &bytes(2, b"b")).unwrap();
    a::set_next(&mut p, Some(RowId::page_address(4, 8).unwrap())).unwrap();
    let h = a::header(&p).unwrap();
    for n in 1..4 {
        bicdb_storage::itl::grow(&mut p, 8).unwrap();
        assert_eq!(p.slot_directory_start(), 84 + n * 24);
        assert_eq!(a::header(&p).unwrap(), h);
        assert_eq!(a::decode(a::record(&p, 1).unwrap()).unwrap().payload, b"a");
        assert_eq!(a::decode(a::record(&p, 2).unwrap()).unwrap().payload, b"b");
    }
    let before = *p.as_bytes();
    let row = bicdb_storage::row::assemble_row(0, 1, &[false], &[], &[b"x".as_slice()]).unwrap();
    assert!(bicdb_storage::heap::insert_row(&mut p, &row, &Default::default()).is_err());
    assert_eq!(p.as_bytes(), &before);
    a::validate(&p).unwrap();
}
#[test]
fn growing_update_and_delete_undo_survive_physical_compaction() {
    let mut p = page();
    let first = bytes(1, b"old");
    let second = bytes(2, b"second");
    a::append(&mut p, &first).unwrap();
    a::append(&mut p, &second).unwrap();
    a::update(&mut p, 1, &bytes(1, &[b'x'; 500])).unwrap();
    let original_position = p.slot(0).unwrap().offset();
    a::delete(&mut p, 2).unwrap();
    a::compact_retaining_undo(&mut p).unwrap();
    assert_ne!(
        p.slot(0).unwrap().offset(),
        original_position,
        "compaction must actually relocate an edge"
    );
    let restore = undo(2, UndoOp::Delete, UndoPayload::FullRow(second.clone()));
    apply_undo_to_page(&mut p, &restore).unwrap();
    apply_undo_to_page(&mut p, &restore).unwrap();
    let restore = undo(
        1,
        UndoOp::Update,
        UndoPayload::Update {
            old_itl_slot: 0xFF,
            patches: vec![(0, first.clone())],
        },
    );
    apply_undo_to_page(&mut p, &restore).unwrap();
    apply_undo_to_page(&mut p, &restore).unwrap();
    assert_eq!(a::record(&p, 1).unwrap(), first);
    assert_eq!(a::record(&p, 2).unwrap(), second);
    a::validate(&p).unwrap();
    let remove = undo(1, UndoOp::Insert, UndoPayload::None);
    apply_undo_to_page(&mut p, &remove).unwrap();
    apply_undo_to_page(&mut p, &remove).unwrap();
    assert_eq!(a::header(&p).unwrap().live_edges, 1);
    a::validate(&p).unwrap();
}
#[test]
fn metadata_undo_uses_body_relative_address_after_itl_growth() {
    let mut p = page();
    a::append(&mut p, &bytes(1, b"a")).unwrap();
    a::set_next(&mut p, Some(RowId::page_address(4, 8).unwrap())).unwrap();
    bicdb_storage::itl::grow(&mut p, 8).unwrap();
    let restore = undo(
        0,
        UndoOp::Update,
        UndoPayload::Update {
            old_itl_slot: 0xFF,
            patches: vec![(a::NEXT_PAGE_OFFSET as u32, vec![0; 6])],
        },
    );
    apply_undo_to_page(&mut p, &restore).unwrap();
    apply_undo_to_page(&mut p, &restore).unwrap();
    assert!(a::header(&p).unwrap().next.is_none());
    assert_eq!(a::decode(a::record(&p, 1).unwrap()).unwrap().payload, b"a");
}
#[test]
fn repeated_variable_updates_delete_and_compaction_match_an_undo_model() {
    let mut p = page();
    let mut expected = Vec::new();
    for id in 1..=20 {
        let raw = bytes(id, &[id as u8; 30]);
        a::append(&mut p, &raw).unwrap();
        expected.push(Some(raw));
    }
    let original = expected.clone();
    let mut history = Vec::new();
    let mut random = 173u64;
    for step in 0..180 {
        random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
        let index = (random >> 32) as usize % expected.len();
        if let Some(old) = expected[index].clone() {
            let ordinal = index as u16 + 1;
            if step % 11 == 0 {
                a::delete(&mut p, ordinal).unwrap();
                history.push((
                    index,
                    old.clone(),
                    undo(ordinal, UndoOp::Delete, UndoPayload::FullRow(old)),
                ));
                expected[index] = None;
            } else {
                let replacement = bytes(
                    index as u64 + 1,
                    &vec![step as u8; 20 + (random as usize % 1000)],
                );
                let before = *p.as_bytes();
                match a::update(&mut p, ordinal, &replacement) {
                    Ok(()) => {
                        history.push((
                            index,
                            old.clone(),
                            undo(
                                ordinal,
                                UndoOp::Update,
                                UndoPayload::Update {
                                    old_itl_slot: 0xFF,
                                    patches: vec![(0, old)],
                                },
                            ),
                        ));
                        expected[index] = Some(replacement);
                    }
                    Err(AdjacencyError::PageFull) => assert_eq!(p.as_bytes(), &before),
                    unexpected => panic!("{unexpected:?}"),
                }
            }
        }
        if step % 7 == 0 {
            a::compact_retaining_undo(&mut p).unwrap();
        }
        a::validate(&p).unwrap();
        for (index, value) in expected.iter().enumerate() {
            assert_eq!(a::record(&p, index as u16 + 1).ok(), value.as_deref());
        }
    }
    assert!(history.len() > 30);
    for (index, old, operation) in history.into_iter().rev() {
        apply_undo_to_page(&mut p, &operation).unwrap();
        a::compact_retaining_undo(&mut p).unwrap();
        expected[index] = Some(old);
        a::validate(&p).unwrap();
        for (index, value) in expected.iter().enumerate() {
            assert_eq!(a::record(&p, index as u16 + 1).ok(), value.as_deref());
        }
    }
    assert_eq!(expected, original);
}

#[test]
fn malformed_fields_bounds_and_undo_cannot_damage_the_page() {
    let mut p = page();
    a::append(&mut p, &bytes(1, b"payload")).unwrap();
    let raw = bytes(2, b"{}");
    for n in 0..raw.len() {
        assert!(a::decode(&raw[..n]).is_err());
    }
    let before = *p.as_bytes();
    for next in [
        RowId::page_address(5, 8).unwrap(),
        RowId::page_address(4, 7).unwrap(),
        source(),
    ] {
        assert!(a::set_next(&mut p, Some(next)).is_err());
        assert_eq!(p.as_bytes(), &before);
    }
    let bad = undo(
        1,
        UndoOp::Update,
        UndoPayload::Update {
            old_itl_slot: 0xFF,
            patches: vec![(0, vec![3]), (u32::MAX, vec![1])],
        },
    );
    assert!(apply_undo_to_page(&mut p, &bad).is_err());
    assert_eq!(p.as_bytes(), &before);
    let bad = undo(
        1,
        UndoOp::Update,
        UndoPayload::Update {
            old_itl_slot: 0xFF,
            patches: vec![(0, bytes(2, b"payload"))],
        },
    );
    assert!(apply_undo_to_page(&mut p, &bad).is_err());
    assert_eq!(p.as_bytes(), &before);
    a::delete(&mut p, 1).unwrap();
    let deleted = *p.as_bytes();
    let forged = Edge {
        id: 1,
        destination: RowId::from_parts(3, 99, 2).unwrap(),
        flags: 0,
        itl_slot: 0xFF,
        label: "关联",
        payload: b"payload",
    }
    .encode()
    .unwrap();
    assert!(apply_undo_to_page(
        &mut p,
        &undo(1, UndoOp::Delete, UndoPayload::FullRow(forged))
    )
    .is_err());
    assert_eq!(p.as_bytes(), &deleted);
    p.set_slot(0, SlotEntry::new(0, SlotStatus::Normal).unwrap());
    assert!(a::validate(&p).is_err());
    let mut p = page();
    p.as_bytes_mut()[64..66].copy_from_slice(&u16::MAX.to_le_bytes());
    assert!(a::header(&p).is_err());
    assert!(a::validate(&p).is_err());
}
