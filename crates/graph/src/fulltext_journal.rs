//! Pure plan/consume state machine for the graph-wide lightweight change journal.
//! Plans contain IDs, revisions and counters, never source text or tokens. Native
//! SQL must persist the plan in the source transaction, and publish index+ack in
//! one maintenance transaction. This module does not run a background worker.
use crate::fulltext::TextLimits;
use crate::model::fail;
use crate::property_index::EntityKind;
use crate::Error;
use bicdb_common::sha256::Sha256;
use serde_json::{json, Value as Json};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub entity: EntityKind,
    pub id: u64,
    pub seq: u64,
    pub revision: Option<[u8; 32]>,
    pub queued_at_ms: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Consumer {
    pub covered_seq: u64,
    pub cursor: u64,
    pub paused: bool,
}
#[derive(Debug, Clone)]
pub struct Batch {
    consumer: u32,
    events: Vec<Event>,
    target_cursor: u64,
    source_seq: u64,
    expected: Consumer,
}
impl Batch {
    pub fn consumer(&self) -> u32 {
        self.consumer
    }
    pub fn events(&self) -> &[Event] {
        &self.events
    }
    pub fn target_cursor(&self) -> u64 {
        self.target_cursor
    }
    pub fn source_seq(&self) -> u64 {
        self.source_seq
    }
}
/// Small independently decoded journal metadata. Foreground writers read this
/// header plus only markers for affected IDs, never clone the entire backlog.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JournalHead {
    source_seq: u64,
    consumers: BTreeMap<u32, Consumer>,
    events: usize,
}
impl JournalHead {
    pub fn source_seq(&self) -> u64 {
        self.source_seq
    }
    pub fn consumers(&self) -> &BTreeMap<u32, Consumer> {
        &self.consumers
    }
    pub fn event_count(&self) -> usize {
        self.events
    }
    pub fn native_rows(&self, limits: &TextLimits) -> Result<BTreeMap<u64, Vec<u8>>, Error> {
        let header = json!({"format":"bicdb-graph-fulltext-journal-v1","source_seq":self.source_seq,"consumers":self.consumers.iter().map(|(id,c)|json!([id,c.covered_seq,c.cursor,c.paused])).collect::<Vec<_>>(),"events":self.events});
        let mut rows = BTreeMap::new();
        put(&mut rows, 0, &header, &mut 0, limits)?;
        Ok(rows)
    }
    pub fn from_native_rows(
        rows: &BTreeMap<u64, Vec<u8>>,
        limits: &TextLimits,
    ) -> Result<Self, Error> {
        let header = get(rows, 0)?;
        if header.as_object().map_or(0, |v| v.len()) != 4
            || header["format"] != "bicdb-graph-fulltext-journal-v1"
        {
            return Err(fail("unsupported fulltext journal header"));
        }
        let source_seq = number(&header["source_seq"])?;
        let events = usize::try_from(number(&header["events"])?)
            .map_err(|_| fail("invalid fulltext event count"))?;
        if events > limits.max_documents {
            return Err(fail("fulltext event count budget exceeded"));
        }
        let consumers = header["consumers"]
            .as_array()
            .ok_or_else(|| fail("invalid fulltext consumers"))?;
        if consumers.len() > 64 {
            return Err(fail("fulltext consumer count budget exceeded"));
        }
        let mut out = Self {
            source_seq,
            events,
            consumers: BTreeMap::new(),
        };
        for v in consumers {
            let a = v
                .as_array()
                .ok_or_else(|| fail("invalid fulltext consumer"))?;
            if a.len() != 4 {
                return Err(fail("invalid fulltext consumer"));
            }
            let id =
                u32::try_from(number(&a[0])?).map_err(|_| fail("invalid fulltext consumer ID"))?;
            let c = Consumer {
                covered_seq: number(&a[1])?,
                cursor: number(&a[2])?,
                paused: a[3]
                    .as_bool()
                    .ok_or_else(|| fail("invalid fulltext pause state"))?,
            };
            if id < 100
                || c.covered_seq > c.cursor
                || c.cursor > source_seq
                || out.consumers.insert(id, c).is_some()
            {
                return Err(fail("invalid/duplicate fulltext consumer watermark"));
            }
        }
        if out.consumers.is_empty() && events != 0 {
            return Err(fail("orphaned fulltext journal events"));
        }
        let size = rows
            .range(0..=16)
            .fold(0usize, |n, (_, v)| n.saturating_add(v.len()));
        if size > limits.max_bytes {
            return Err(fail("fulltext journal header byte budget exceeded"));
        }
        Ok(out)
    }
    /// The caller supplies live markers for exactly the touched IDs using its
    /// source transaction view. Plans do not acknowledge or publish themselves.
    pub fn plan(
        &self,
        changes: &[(EntityKind, u64, Option<[u8; 32]>)],
        queued_at_ms: u64,
        limits: &TextLimits,
        mut lookup: impl FnMut(u64) -> Result<Option<Event>, Error>,
    ) -> Result<(Self, BTreeMap<u64, Event>), Error> {
        if self.consumers.is_empty() {
            return Ok((self.clone(), BTreeMap::new()));
        }
        let mut next = self.clone();
        let mut events = BTreeMap::new();
        for (entity, id, revision) in changes {
            if *id == 0 || *id >= 1 << 48 || events.contains_key(id) {
                return Err(fail("invalid/duplicate fulltext change ID"));
            }
            let prior = lookup(*id)?;
            if prior.as_ref().is_some_and(|p| {
                p.id != *id
                    || p.entity != *entity
                    || p.seq == 0
                    || p.seq > self.source_seq
                    || self.events == 0
                    || p.seq
                        <= self
                            .consumers
                            .values()
                            .map(|c| c.covered_seq)
                            .min()
                            .unwrap_or(self.source_seq)
            }) {
                return Err(fail("invalid previous fulltext marker"));
            }
            next.source_seq = next
                .source_seq
                .checked_add(1)
                .ok_or_else(|| fail("fulltext source sequence overflow"))?;
            if prior.is_none() {
                next.events = next
                    .events
                    .checked_add(1)
                    .ok_or_else(|| fail("fulltext event count overflow"))?;
            }
            events.insert(
                *id,
                Event {
                    entity: *entity,
                    id: *id,
                    seq: next.source_seq,
                    revision: *revision,
                    queued_at_ms: prior.map_or(queued_at_ms, |p| p.queued_at_ms),
                },
            );
        }
        // Conservative encoded size, including reserved bounded header chunks.
        if next.events > limits.max_documents
            || next
                .events
                .saturating_mul(384)
                .saturating_add(16 * 4096 + 32)
                > limits.max_bytes
        {
            return Err(fail("fulltext pending journal budget exceeded"));
        }
        Ok((next, events))
    }
}
impl Event {
    pub fn ordinal(id: u64) -> Result<u64, Error> {
        if id == 0 || id >= 1 << 48 {
            return Err(fail("invalid fulltext event ID"));
        }
        Ok((3 << 60) | (id << 10))
    }
    pub fn native_rows(&self, limits: &TextLimits) -> Result<BTreeMap<u64, Vec<u8>>, Error> {
        let key = Self::ordinal(self.id)?;
        if self.seq == 0 {
            return Err(fail("invalid fulltext event sequence"));
        }
        let mut rows = BTreeMap::new();
        put(
            &mut rows,
            key,
            &json!({"id":self.id,"entity":if self.entity==EntityKind::Node{"node"}else{"relationship"},"seq":self.seq,"revision":self.revision.map(|r|r.iter().map(|b|format!("{b:02x}")).collect::<String>()),"queued_at_ms":self.queued_at_ms}),
            &mut 0,
            limits,
        )?;
        Ok(rows)
    }
    pub fn from_native_rows(id: u64, rows: &BTreeMap<u64, Vec<u8>>) -> Result<Option<Self>, Error> {
        let key = Self::ordinal(id)?;
        if rows.is_empty() {
            return Ok(None);
        }
        if rows.len() != 2 || rows.keys().any(|k| *k != key && *k != key + 1) {
            return Err(fail("invalid fulltext event ordinals"));
        }
        let v = get(rows, key)?;
        if v.as_object().map_or(0, |v| v.len()) != 5 || number(&v["id"])? != id {
            return Err(fail("invalid fulltext event record"));
        }
        let entity = match v["entity"].as_str() {
            Some("node") => EntityKind::Node,
            Some("relationship") => EntityKind::Relationship,
            _ => return Err(fail("invalid fulltext event kind")),
        };
        let seq = number(&v["seq"])?;
        if seq == 0 {
            return Err(fail("invalid fulltext event sequence"));
        }
        let revision = if v["revision"].is_null() {
            None
        } else {
            let s = v["revision"]
                .as_str()
                .ok_or_else(|| fail("invalid event revision"))?;
            if s.len() != 64 || !s.is_ascii() {
                return Err(fail("invalid event revision"));
            }
            let mut hash = [0; 32];
            for (i, b) in hash.iter_mut().enumerate() {
                *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
                    .map_err(|_| fail("invalid event revision"))?;
            }
            Some(hash)
        };
        Ok(Some(Self {
            entity,
            id,
            seq,
            revision,
            queued_at_ms: number(&v["queued_at_ms"])?,
        }))
    }
}
/// A cursor tracks partially applied batches. The completeness watermark only
/// advances after all visible pending changes are applied; coalescing a marker
/// must never hide an older dirty revision behind a falsely advanced watermark.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Journal {
    source_seq: u64,
    events: BTreeMap<u64, Event>,
    consumers: BTreeMap<u32, Consumer>,
}
impl Journal {
    pub fn head(&self) -> JournalHead {
        JournalHead {
            source_seq: self.source_seq,
            consumers: self.consumers.clone(),
            events: self.events.len(),
        }
    }
    /// A full consistent rebuild covers the current source sequence, including
    /// previously coalesced markers, while preserving the consumer pause state.
    pub fn rebuilt(&self, index: u32) -> Result<Self, Error> {
        let mut next = self.clone();
        let c = next
            .consumers
            .get_mut(&index)
            .ok_or_else(|| fail("unknown fulltext consumer"))?;
        c.cursor = next.source_seq;
        c.covered_seq = next.source_seq;
        next.cleanup();
        Ok(next)
    }
    pub fn source_seq(&self) -> u64 {
        self.source_seq
    }
    pub fn events(&self) -> &BTreeMap<u64, Event> {
        &self.events
    }
    pub fn consumers(&self) -> &BTreeMap<u32, Consumer> {
        &self.consumers
    }
    pub fn register(&self, index: u32, baseline: u64) -> Result<Self, Error> {
        if index < 100
            // Older markers may already have been reclaimed. Registration
            // must follow a full build at the current consistent source view.
            || baseline != self.source_seq
            || self.consumers.len() >= 64
            || self.consumers.contains_key(&index)
        {
            return Err(fail("invalid/duplicate fulltext consumer"));
        }
        let mut next = self.clone();
        next.consumers.insert(
            index,
            Consumer {
                covered_seq: baseline,
                cursor: baseline,
                paused: false,
            },
        );
        Ok(next)
    }
    pub fn unregister(&self, index: u32) -> Result<Self, Error> {
        let mut next = self.clone();
        if next.consumers.remove(&index).is_none() {
            return Err(fail("unknown fulltext consumer"));
        }
        next.cleanup();
        Ok(next)
    }
    pub fn paused(&self, index: u32, paused: bool) -> Result<Self, Error> {
        let mut next = self.clone();
        next.consumers
            .get_mut(&index)
            .ok_or_else(|| fail("unknown fulltext consumer"))?
            .paused = paused;
        Ok(next)
    }
    pub fn plan(
        &self,
        changes: &[(EntityKind, u64, Option<[u8; 32]>)],
        queued_at_ms: u64,
        limits: &TextLimits,
    ) -> Result<Self, Error> {
        let (head, events) = self.head().plan(changes, queued_at_ms, limits, |id| {
            Ok(self.events.get(&id).cloned())
        })?;
        let mut next = self.clone();
        next.source_seq = head.source_seq;
        next.events.extend(events);
        Ok(next)
    }
    pub fn batch(&self, index: u32, rows: usize) -> Result<Batch, Error> {
        let c = *self
            .consumers
            .get(&index)
            .ok_or_else(|| fail("unknown fulltext consumer"))?;
        if c.paused || rows == 0 || rows > 100_000 {
            return Err(fail("fulltext consumer paused or batch_rows invalid"));
        }
        let mut events: Vec<_> = self
            .events
            .values()
            .filter(|e| e.seq > c.cursor)
            .cloned()
            .collect();
        events.sort_by_key(|e| e.seq);
        let total = events.len();
        events.truncate(rows);
        let target_cursor = if events.len() == total {
            self.source_seq
        } else {
            events.last().expect("nonempty partial batch").seq
        };
        Ok(Batch {
            consumer: index,
            events,
            target_cursor,
            source_seq: self.source_seq,
            expected: c,
        })
    }
    /// Replays after an already committed acknowledgment are idempotent.
    /// Concurrent/new source markers remain above the acknowledged cursor.
    pub fn acknowledge(&self, batch: &Batch) -> Result<Self, Error> {
        let c = *self
            .consumers
            .get(&batch.consumer)
            .ok_or_else(|| fail("unknown fulltext consumer"))?;
        if batch.target_cursor < c.cursor {
            return Ok(self.clone());
        }
        if c == batch.expected
            && batch.target_cursor == c.cursor
            && c.covered_seq == self.source_seq
        {
            return Ok(self.clone());
        }
        if c != batch.expected {
            if batch.target_cursor == c.cursor && !c.paused {
                return Ok(self.clone());
            }
            return Err(fail("fulltext consumer changed while a batch was running"));
        }
        if c.paused
            || batch.source_seq > self.source_seq
            || batch.target_cursor > batch.source_seq
            || batch.target_cursor < c.cursor
        {
            return Err(fail("invalid fulltext acknowledgment watermark"));
        }
        let applied: BTreeMap<_, _> = batch.events.iter().map(|e| (e.id, e)).collect();
        for event in &batch.events {
            let current = self
                .events
                .get(&event.id)
                .ok_or_else(|| fail("fulltext acknowledgment belongs to another journal"))?;
            if current != event && current.seq <= batch.source_seq {
                return Err(fail("fulltext acknowledgment source differs from batch"));
            }
        }
        if self.events.values().any(|event| {
            event.seq > c.cursor
                && event.seq <= batch.target_cursor
                && applied.get(&event.id).copied() != Some(event)
        }) {
            return Err(fail("fulltext acknowledgment skips unapplied changes"));
        }
        let mut next = self.clone();
        let c = next.consumers.get_mut(&batch.consumer).expect("consumer");
        c.cursor = batch.target_cursor;
        if !next.events.values().any(|e| e.seq > c.cursor) {
            c.cursor = next.source_seq;
            c.covered_seq = next.source_seq;
        }
        next.cleanup();
        Ok(next)
    }
    fn cleanup(&mut self) {
        let minimum = self
            .consumers
            .values()
            .map(|c| c.covered_seq)
            .min()
            .unwrap_or(self.source_seq);
        self.events.retain(|_, e| e.seq > minimum);
    }
    /// Native protected-heap image: checksummed bounded header chunks and
    /// one checksum/data pair per coalesced source element.
    pub fn native_rows(&self, limits: &TextLimits) -> Result<BTreeMap<u64, Vec<u8>>, Error> {
        let mut rows = self.head().native_rows(limits)?;
        for event in self.events.values() {
            rows.extend(event.native_rows(limits)?);
        }
        if rows.values().fold(0usize, |n, v| n.saturating_add(v.len())) > limits.max_bytes {
            return Err(fail("fulltext journal stored byte budget exceeded"));
        }
        Ok(rows)
    }
    pub fn from_native_rows(
        rows: &BTreeMap<u64, Vec<u8>>,
        limits: &TextLimits,
    ) -> Result<Self, Error> {
        if rows.values().fold(0usize, |n, v| n.saturating_add(v.len())) > limits.max_bytes {
            return Err(fail("fulltext journal stored byte budget exceeded"));
        }
        let head = JournalHead::from_native_rows(rows, limits)?;
        let mut journal = Self {
            source_seq: head.source_seq,
            consumers: head.consumers,
            events: BTreeMap::new(),
        };
        let mut ids = BTreeSet::new();
        for key in rows.keys() {
            if *key <= 16 {
                continue;
            }
            if key >> 60 != 3 || key & 1023 > 1 {
                return Err(fail("invalid fulltext journal ordinal"));
            }
            ids.insert((key & ((1 << 60) - 1)) >> 10);
        }
        if ids.len() != head.events {
            return Err(fail("fulltext journal event count mismatch"));
        }
        let mut seqs = BTreeSet::new();
        for id in ids {
            let key = Event::ordinal(id)?;
            let slice = rows
                .range(key..=key + 1)
                .map(|(k, v)| (*k, v.clone()))
                .collect();
            let event = Event::from_native_rows(id, &slice)?
                .ok_or_else(|| fail("missing fulltext event"))?;
            if event.seq > head.source_seq || !seqs.insert(event.seq) {
                return Err(fail("invalid fulltext event sequence"));
            }
            journal.events.insert(id, event);
        }
        Ok(journal)
    }
}

fn number(v: &Json) -> Result<u64, Error> {
    v.as_u64()
        .ok_or_else(|| fail("invalid fulltext journal number"))
}
fn digest(bytes: &[u8]) -> [u8; 32] {
    let mut sha = Sha256::new();
    sha.update(bytes);
    sha.finalize()
}
fn put(
    rows: &mut BTreeMap<u64, Vec<u8>>,
    key: u64,
    v: &Json,
    total: &mut usize,
    limits: &TextLimits,
) -> Result<(), Error> {
    let bytes = serde_json::to_vec(v).map_err(|e| fail(e.to_string()))?;
    *total = total.saturating_add(bytes.len()).saturating_add(32);
    if bytes.len() > (if key == 0 { 16 * 4096 } else { 4096 }) || *total > limits.max_bytes {
        return Err(fail("fulltext journal row/byte budget exceeded"));
    }
    rows.insert(key, digest(&bytes).to_vec());
    for (i, chunk) in bytes.chunks(4096).enumerate() {
        rows.insert(key + 1 + i as u64, chunk.to_vec());
    }
    Ok(())
}
fn get(rows: &BTreeMap<u64, Vec<u8>>, key: u64) -> Result<Json, Error> {
    let checksum = rows
        .get(&key)
        .ok_or_else(|| fail("missing fulltext journal checksum"))?;
    let mut bytes = Vec::new();
    for (i, (&ordinal, data)) in rows
        .range(key + 1..=key + (if key == 0 { 16 } else { 1 }))
        .enumerate()
    {
        if ordinal != key + i as u64 + 1
            || data.is_empty()
            || data.len() > 4096
            || (!bytes.is_empty() && bytes.len() % 4096 != 0)
        {
            return Err(fail("invalid fulltext journal chunks"));
        }
        bytes.extend(data);
    }
    if bytes.is_empty() || digest(&bytes).as_slice() != checksum {
        return Err(fail("fulltext journal checksum mismatch"));
    }
    serde_json::from_slice(&bytes).map_err(|_| fail("invalid fulltext journal JSON"))
}

/// Request-local finite barrier. Pending IDs are captured once, including newer
/// coalesced revisions when an older explicit target cannot be separated. New
/// IDs never extend the barrier; completed IDs are not chased after later writes.
#[derive(Debug, Clone)]
pub struct FixedTarget {
    consumer: u32,
    target: u64,
    expected: Consumer,
    remaining: BTreeMap<u64, (EntityKind, u64)>,
}
#[derive(Debug, Clone)]
pub struct TargetBatch {
    consumer: u32,
    target: u64,
    expected: Consumer,
    events: Vec<Event>,
}
impl TargetBatch {
    pub fn events(&self) -> &[Event] {
        &self.events
    }
}
impl FixedTarget {
    pub fn begin(journal: &Journal, consumer: u32, target: Option<u64>) -> Result<Self, Error> {
        Self::begin_with_applied(journal, consumer, target, |_| false)
    }
    /// The caller proves revisions already present in its atomically published
    /// generation (or published tombstones). This lets retries skip durable
    /// partial work without inferring completeness from a numerical cursor.
    pub fn begin_with_applied(
        journal: &Journal,
        consumer: u32,
        target: Option<u64>,
        mut applied: impl FnMut(&Event) -> bool,
    ) -> Result<Self, Error> {
        Self::try_begin_with_applied(journal, consumer, target, |event| Ok(applied(event)))
    }
    /// Fallible source proof; an unreadable document aborts WAIT planning.
    pub fn try_begin_with_applied(
        journal: &Journal,
        consumer: u32,
        target: Option<u64>,
        mut applied: impl FnMut(&Event) -> Result<bool, Error>,
    ) -> Result<Self, Error> {
        let expected = *journal
            .consumers
            .get(&consumer)
            .ok_or_else(|| fail("unknown fulltext consumer"))?;
        let target = target.unwrap_or(journal.source_seq);
        if target > journal.source_seq {
            return Err(fail("fulltext wait target exceeds current source sequence"));
        }
        let mut remaining = BTreeMap::new();
        if expected.covered_seq < target {
            for event in journal.events.values().filter(|e| e.seq > expected.cursor) {
                if !applied(event)? {
                    remaining.insert(event.id, (event.entity, event.seq));
                }
            }
        }
        Ok(Self {
            consumer,
            target,
            expected,
            remaining,
        })
    }
    pub fn target_seq(&self) -> u64 {
        self.target
    }
    pub fn remaining(&self) -> usize {
        self.remaining.len()
    }
    pub fn reached(&self, journal: &Journal) -> Result<bool, Error> {
        Ok(journal
            .consumers
            .get(&self.consumer)
            .ok_or_else(|| fail("unknown fulltext consumer"))?
            .covered_seq
            >= self.target)
    }
    pub fn batch(&self, journal: &Journal, rows: usize) -> Result<TargetBatch, Error> {
        if rows == 0 || rows > 100_000 {
            return Err(fail("invalid fulltext wait batch_rows"));
        }
        let current = *journal
            .consumers
            .get(&self.consumer)
            .ok_or_else(|| fail("unknown fulltext consumer"))?;
        if current != self.expected || current.paused {
            return Err(fail("fulltext wait consumer changed or paused"));
        }
        // Stable captured ID order avoids repeatedly sorting the entire backlog.
        let events = self
            .remaining
            .iter()
            .take(rows)
            .map(|(id, (entity, seq))| {
                let event = journal
                    .events
                    .get(id)
                    .ok_or_else(|| fail("fulltext wait marker disappeared"))?;
                if event.entity != *entity || event.seq < *seq {
                    return Err(fail("fulltext wait marker changed identity"));
                }
                Ok(event.clone())
            })
            .collect::<Result<Vec<_>, Error>>()?;
        Ok(TargetBatch {
            consumer: self.consumer,
            target: self.target,
            expected: self.expected,
            events,
        })
    }
    /// Publish this progress atomically with the batch documents. Partial batches
    /// keep all consumer watermarks/markers, so timeout/restart can safely replay.
    /// The final proof advances only the frozen target; later markers remain dirty.
    pub fn acknowledge(
        &self,
        journal: &Journal,
        batch: &TargetBatch,
    ) -> Result<(Self, Journal), Error> {
        let current = *journal
            .consumers
            .get(&self.consumer)
            .ok_or_else(|| fail("unknown fulltext consumer"))?;
        if batch.consumer != self.consumer
            || batch.target != self.target
            || batch.expected != self.expected
            || current != self.expected
            || current.paused
            || self.target > journal.source_seq
        {
            return Err(fail("invalid fulltext wait acknowledgment"));
        }
        if batch.events.is_empty() && !self.remaining.is_empty() {
            return Err(fail("fulltext wait batch skips pending targets"));
        }
        let mut next = self.clone();
        for event in &batch.events {
            let Some((entity, seq)) = next.remaining.remove(&event.id) else {
                return Err(fail(
                    "fulltext wait acknowledgment repeats/introduces an ID",
                ));
            };
            if event.entity != entity
                || event.seq < seq
                || journal.events.get(&event.id) != Some(event)
            {
                return Err(fail("fulltext wait source changed during batch"));
            }
        }
        let mut published = journal.clone();
        if next.remaining.is_empty() {
            let consumer = published
                .consumers
                .get_mut(&self.consumer)
                .expect("checked consumer");
            consumer.covered_seq = consumer.covered_seq.max(self.target);
            consumer.cursor = consumer.cursor.max(self.target);
            published.cleanup();
        }
        Ok((next, published))
    }
}
