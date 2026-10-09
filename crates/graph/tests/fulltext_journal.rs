use bicdb_graph::fulltext::TextLimits;
use bicdb_graph::fulltext_journal::{Event, FixedTarget, Journal, JournalHead};
use bicdb_graph::property_index::EntityKind;
use std::collections::BTreeMap;
fn upsert(id: u64, revision: u8) -> (EntityKind, u64, Option<[u8; 32]>) {
    (EntityKind::Node, id, Some([revision; 32]))
}

#[test]
fn failed_applied_revision_proof_aborts_wait_without_advancing_consumer() {
    let journal = Journal::default()
        .register(100, 0)
        .unwrap()
        .plan(&[upsert(1, 1)], 0, &TextLimits::default())
        .unwrap();
    let before = journal.clone();
    assert!(
        FixedTarget::try_begin_with_applied(&journal, 100, None, |_| Err(bicdb_graph::Error(
            "unreadable document".into()
        )))
        .is_err()
    );
    assert_eq!(journal, before);
}
#[test]
fn no_consumer_has_no_marker_cost_and_multiple_indexes_share_events() {
    let limits = TextLimits::default();
    let empty = Journal::default();
    assert_eq!(empty, empty.plan(&[upsert(1, 1)], 10, &limits).unwrap());
    let j = empty.register(100, 0).unwrap().register(101, 0).unwrap();
    let next = j.plan(&[upsert(1, 1), upsert(2, 2)], 10, &limits).unwrap();
    assert_eq!(next.events().len(), 2);
    assert_eq!(next.source_seq(), 2);
    assert!(
        j.events().is_empty(),
        "source rollback keeps prior committed plan"
    );
    let newer = next.plan(&[upsert(1, 3)], 20, &limits).unwrap();
    assert_eq!(newer.events().len(), 2);
    assert_eq!(newer.events()[&1].seq, 3);
    assert_eq!(
        newer.events()[&1].queued_at_ms,
        10,
        "lag follows first outstanding change"
    );
}
#[test]
fn partial_cursor_does_not_claim_coverage_and_cleanup_waits_for_slowest_index() {
    let limits = TextLimits::default();
    let j = Journal::default()
        .register(100, 0)
        .unwrap()
        .register(101, 0)
        .unwrap()
        .plan(&[upsert(1, 1), upsert(2, 2), upsert(3, 3)], 10, &limits)
        .unwrap();
    let first = j.batch(100, 1).unwrap();
    let j = j.acknowledge(&first).unwrap();
    assert_eq!(j.consumers()[&100].cursor, 1);
    assert_eq!(j.consumers()[&100].covered_seq, 0);
    assert_eq!(j.events().len(), 3);
    let rest = j.batch(100, 256).unwrap();
    let j = j.acknowledge(&rest).unwrap();
    assert_eq!(j.consumers()[&100].covered_seq, 3);
    assert_eq!(j.events().len(), 3, "slow consumer retains marker history");
    assert_eq!(
        j.acknowledge(&rest).unwrap(),
        j,
        "acknowledgment replay is idempotent"
    );
    let done = j.batch(101, 256).unwrap();
    let j = j.acknowledge(&done).unwrap();
    assert!(j.events().is_empty());
    assert_eq!(j.consumers()[&101].covered_seq, 3);
}
#[test]
fn paused_consumers_keep_backlog_and_new_markers_survive_inflight_ack() {
    let limits = TextLimits::default();
    let j = Journal::default()
        .register(100, 0)
        .unwrap()
        .plan(&[upsert(1, 1)], 10, &limits)
        .unwrap();
    let batch = j.batch(100, 10).unwrap();
    let changed = j.plan(&[upsert(1, 2)], 20, &limits).unwrap();
    let acknowledged = changed.acknowledge(&batch).unwrap();
    assert_eq!(acknowledged.events()[&1].revision, Some([2; 32]));
    assert_eq!(acknowledged.consumers()[&100].covered_seq, 0);
    assert_eq!(acknowledged.consumers()[&100].cursor, 1);
    let paused = acknowledged.paused(100, true).unwrap();
    assert!(paused.batch(100, 1).is_err());
    let resumed = paused.paused(100, false).unwrap();
    let b = resumed.batch(100, 10).unwrap();
    let done = resumed.acknowledge(&b).unwrap();
    assert!(done.events().is_empty());
    assert_eq!(done.source_seq(), 2);
}
#[test]
fn native_journal_roundtrip_crash_boundaries_and_budget_errors() {
    let limits = TextLimits::default();
    let before = Journal::default().register(100, 0).unwrap();
    let after = before
        .plan(
            &[upsert(1, 1), (EntityKind::Relationship, 2, None)],
            100,
            &limits,
        )
        .unwrap();
    let rows = after.native_rows(&limits).unwrap();
    assert_eq!(Journal::from_native_rows(&rows, &limits).unwrap(), after);
    assert_eq!(
        Journal::from_native_rows(&before.native_rows(&limits).unwrap(), &limits).unwrap(),
        before,
        "not publishing source transaction leaves prior image"
    );
    let mut corrupt = rows.clone();
    corrupt.values_mut().last().unwrap()[0] ^= 1;
    assert!(Journal::from_native_rows(&corrupt, &limits).is_err());
    let mut missing = rows.clone();
    missing.pop_last();
    assert!(Journal::from_native_rows(&missing, &limits).is_err());
    assert!(after
        .plan(&[upsert(1, 2), upsert(1, 3)], 101, &limits)
        .is_err());
    assert!(before
        .plan(
            &[upsert(1, 1)],
            10,
            &TextLimits {
                max_documents: 0,
                ..limits.clone()
            }
        )
        .is_err());
    assert!(before.register(100, 0).is_err());
    assert!(before.register(101, 1).is_err());
    assert!(before.batch(100, 0).is_err());
    assert!(before
        .native_rows(&TextLimits {
            max_bytes: 1,
            ..limits
        })
        .is_err());
}

#[test]
fn a_new_consumer_cannot_use_reclaimed_history_and_foreign_batches_cannot_acknowledge() {
    let limits = TextLimits::default();
    let base = Journal::default().register(100, 0).unwrap();
    let first = base.plan(&[upsert(1, 1)], 10, &limits).unwrap();
    let foreign = base.plan(&[upsert(1, 2)], 10, &limits).unwrap();
    assert!(first.acknowledge(&foreign.batch(100, 10).unwrap()).is_err());
    let complete = first.acknowledge(&first.batch(100, 10).unwrap()).unwrap();
    assert!(complete.events().is_empty());
    assert!(complete.register(101, 0).is_err(), "old history is gone");
    let registered = complete.register(101, complete.source_seq()).unwrap();
    assert_eq!(registered.consumers()[&101].covered_seq, 1);
    let restored =
        Journal::from_native_rows(&registered.native_rows(&limits).unwrap(), &limits).unwrap();
    assert_eq!(restored, registered);
}

#[test]
fn sparse_foreground_plan_reads_only_touched_markers_and_matches_complete_image() {
    let limits = TextLimits::default();
    let registered = Journal::default().register(100, 0).unwrap();
    let changes: Vec<_> = (1..=1000).map(|id| upsert(id, 1)).collect();
    let before = registered.plan(&changes, 10, &limits).unwrap();
    let header_rows = before.head().native_rows(&limits).unwrap();
    let head = JournalHead::from_native_rows(&header_rows, &limits).unwrap();
    let touched = [upsert(9, 3), (EntityKind::Relationship, 1001, None)];
    let mut looked_up = Vec::new();
    let (next_head, events) = head
        .plan(&touched, 20, &limits, |id| {
            looked_up.push(id);
            Ok(before.events().get(&id).cloned())
        })
        .unwrap();
    assert_eq!(looked_up, [9, 1001]);
    assert_eq!(next_head.event_count(), 1001);
    assert_eq!(events[&9].queued_at_ms, 10);
    assert_eq!(events[&1001].queued_at_ms, 20);
    let full = before.plan(&touched, 20, &limits).unwrap();
    assert_eq!(next_head, full.head());
    let mut persisted = before.native_rows(&limits).unwrap();
    persisted.extend(next_head.native_rows(&limits).unwrap());
    for e in events.values() {
        persisted.extend(e.native_rows(&limits).unwrap());
    }
    assert_eq!(
        Journal::from_native_rows(&persisted, &limits).unwrap(),
        full
    );
    let mut marker = events[&9].native_rows(&limits).unwrap();
    assert_eq!(
        Event::from_native_rows(9, &marker).unwrap(),
        Some(events[&9].clone())
    );
    marker.values_mut().last().unwrap()[0] ^= 1;
    assert!(Event::from_native_rows(9, &marker).is_err());
    assert_eq!(Event::from_native_rows(9, &BTreeMap::new()).unwrap(), None);
    assert!(head
        .plan(&[upsert(2, 1)], 20, &limits, |_| Ok(Some(
            events[&9].clone()
        )))
        .is_err());
    let empty = JournalHead::default();
    assert!(empty
        .plan(&touched, 0, &limits, |_| panic!(
            "no consumer must not read marker"
        ))
        .unwrap()
        .1
        .is_empty());
    let rebuilt = full.rebuilt(100).unwrap();
    assert!(rebuilt.events().is_empty());
    assert_eq!(rebuilt.consumers()[&100].covered_seq, full.source_seq());
}

#[test]
fn fixed_wait_target_is_finite_under_coalescing_and_later_writes() {
    let limits = TextLimits::default();
    let initial = Journal::default()
        .register(100, 0)
        .unwrap()
        .register(101, 0)
        .unwrap()
        .plan(&[upsert(1, 1), upsert(2, 1)], 1, &limits)
        .unwrap();
    let wait = FixedTarget::begin(&initial, 100, None).unwrap();
    assert_eq!(wait.target_seq(), 2);
    // Original ID 1 moves beyond the target; new IDs must not extend this wait.
    let newer = initial
        .plan(&[upsert(1, 2), upsert(3, 1)], 2, &limits)
        .unwrap();
    let batch = wait.batch(&newer, 1).unwrap();
    assert_eq!(batch.events()[0].id, 1);
    assert_eq!(batch.events()[0].seq, 3);
    let (next, partial) = wait.acknowledge(&newer, &batch).unwrap();
    assert_eq!(partial.consumers()[&100].covered_seq, 0);
    assert_eq!(partial.consumers()[&100].cursor, 0);
    assert_eq!(
        partial, newer,
        "partial progress cannot reclaim old markers"
    );
    // Completed ID 1 changes again; do not chase it or the newly appended ID 4.
    let ongoing = partial
        .plan(&[upsert(1, 3), upsert(2, 2), upsert(4, 1)], 3, &limits)
        .unwrap();
    let batch = next.batch(&ongoing, 1).unwrap();
    assert_eq!(batch.events()[0].id, 2);
    let (done, published) = next.acknowledge(&ongoing, &batch).unwrap();
    assert!(done.reached(&published).unwrap());
    assert_eq!(done.remaining(), 0);
    assert_eq!(published.consumers()[&100].covered_seq, 2);
    assert_eq!(published.consumers()[&100].cursor, 2);
    assert_eq!(published.source_seq(), 7);
    assert_eq!(
        published.consumers()[&101].covered_seq,
        0,
        "other consumer remains dirty"
    );
    assert!(published.events().contains_key(&1));
    assert!(published.events().contains_key(&3));
    let restored =
        Journal::from_native_rows(&published.native_rows(&limits).unwrap(), &limits).unwrap();
    assert_eq!(restored, published);
    // Source changes between planning/analysis and publication must be rejected.
    let changed = newer.plan(&[upsert(1, 9)], 4, &limits).unwrap();
    let stale = wait.batch(&newer, 1).unwrap();
    assert!(wait.acknowledge(&changed, &stale).is_err());
    assert!(wait.batch(&newer.paused(100, true).unwrap(), 1).is_err());
    assert!(FixedTarget::begin(&initial, 100, Some(99)).is_err());
}
#[test]
fn fixed_target_retry_uses_published_revisions_and_preserves_slow_consumers() {
    let limits = TextLimits::default();
    let initial = Journal::default()
        .register(100, 0)
        .unwrap()
        .register(101, 0)
        .unwrap()
        .plan(&[upsert(1, 1), upsert(2, 2), upsert(3, 3)], 1, &limits)
        .unwrap();
    let wait = FixedTarget::begin(&initial, 100, None).unwrap();
    let batch = wait.batch(&initial, 1).unwrap();
    let (_, partial) = wait.acknowledge(&initial, &batch).unwrap();
    // Simulate timeout after atomically publishing document 1 but no full cover.
    let recovered =
        Journal::from_native_rows(&partial.native_rows(&limits).unwrap(), &limits).unwrap();
    let retry = FixedTarget::begin_with_applied(&recovered, 100, None, |e| {
        e.id == 1 && e.revision == Some([1; 32])
    })
    .unwrap();
    assert_eq!(retry.remaining(), 2);
    let batch = retry.batch(&recovered, 2).unwrap();
    assert_eq!(
        batch.events().iter().map(|e| e.id).collect::<Vec<_>>(),
        [2, 3]
    );
    let (_, published) = retry.acknowledge(&recovered, &batch).unwrap();
    assert_eq!(published.consumers()[&100].covered_seq, 3);
    assert_eq!(
        published.events().len(),
        3,
        "slow consumer retains all markers"
    );
    let satisfied = FixedTarget::begin_with_applied(&initial, 100, None, |_| true).unwrap();
    let batch = satisfied.batch(&initial, 1).unwrap();
    assert!(batch.events().is_empty());
    let (_, covered) = satisfied.acknowledge(&initial, &batch).unwrap();
    assert_eq!(covered.consumers()[&100].covered_seq, 3);
    let old = FixedTarget::begin(&initial, 100, Some(1)).unwrap();
    assert_eq!(
        old.remaining(),
        3,
        "older explicit targets conservatively capture current coalesced backlog"
    );
}
