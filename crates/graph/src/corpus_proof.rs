//! Snapshot-scoped independent corpus aggregates for native partial writes.
//!
//! SQL must publish these records with the entity/topology/manifest transaction.
//! This module owns no SQL, files, cache receipts or remote database connection.
//! It does not grant authority to a manifest merely because its checksum is valid.
use crate::{model::fail, storage::StoragePatch, Deadline, Edge, Error, GraphCorpus, Limits, Node};
use bicdb_common::sha256::Sha256;
use std::collections::{BTreeMap, BTreeSet};

const BUCKETS: usize = 4096;
const PAGE: usize = 64;
const ROOT_MAGIC: &[u8; 8] = b"BICGCP1\0";
const ROOT_LEN: usize = 164;
const LEAF_LEN: usize = 57;
const HASH_LEN: usize = 66;
const NATIVE_KIND: u64 = 5 << 60;
const CHUNK: usize = 4096;
const CHUNK_MASK: u64 = 1023;
type Hash = [u8; 32];

/// Logical proof record addresses. SQL assigns a disjoint physical namespace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RecordKey {
    Root,
    Bucket(u16),
    HashPage(u8),
}
impl RecordKey {
    /// Reserved kind-5 proof rows in the graph's ordinal tree. This namespace
    /// does not describe nodes, edges, property indexes or a second topology.
    pub fn range(self) -> Result<(u64, u64), Error> {
        let id = match self {
            Self::Root => 0,
            Self::Bucket(b) if usize::from(b) < BUCKETS => u64::from(b) + 1,
            Self::HashPage(p) if p <= 127 => BUCKETS as u64 + 1 + u64::from(p),
            _ => return Err(fail("invalid corpus proof record address")),
        };
        let base = NATIVE_KIND | (id << 10);
        Ok((base, base + CHUNK_MASK))
    }
    pub fn from_native_ordinal(ordinal: u64) -> Result<Self, Error> {
        if ordinal >> 60 != 5 {
            return Err(fail("foreign corpus proof native ordinal"));
        }
        let id = (ordinal & ((1 << 60) - 1)) >> 10;
        let key = if id == 0 {
            Self::Root
        } else if id <= BUCKETS as u64 {
            Self::Bucket((id - 1) as u16)
        } else if id <= BUCKETS as u64 + 128 {
            Self::HashPage((id - BUCKETS as u64 - 1) as u8)
        } else {
            return Err(fail("invalid corpus proof native ordinal"));
        };
        let (lower, upper) = key.range()?;
        if ordinal < lower || ordinal > upper {
            return Err(fail("misrouted corpus proof native ordinal"));
        }
        Ok(key)
    }
    /// Fixed-size physical rows for one checked logical proof record. The
    /// length prefix separates logical data from padding, so replacements can
    /// reuse existing heap rows and B-tree entries.
    pub fn native_rows(
        self,
        data: &[u8],
        limits: &Limits,
    ) -> Result<BTreeMap<u64, Vec<u8>>, Error> {
        self.native_rows_min(Some(data), 1, limits)
    }
    fn native_rows_min(
        self,
        data: Option<&[u8]>,
        minimum_chunks: usize,
        limits: &Limits,
    ) -> Result<BTreeMap<u64, Vec<u8>>, Error> {
        if minimum_chunks == 0 || minimum_chunks > CHUNK_MASK as usize {
            return Err(fail("invalid corpus proof native chunk high-water"));
        }
        let mut payload = Vec::new();
        match data {
            Some(data) if !data.is_empty() && data.len() <= limits.max_text_bytes => {
                payload.extend((data.len() as u64).to_be_bytes());
                payload.extend(data);
            }
            Some(_) => {
                return Err(fail(
                    "corpus proof native record byte/chunk budget exceeded",
                ));
            }
            None => payload.extend(u64::MAX.to_be_bytes()),
        }
        let chunks = payload.len().div_ceil(CHUNK).max(minimum_chunks);
        if chunks > CHUNK_MASK as usize {
            return Err(fail(
                "corpus proof native record byte/chunk budget exceeded",
            ));
        }
        payload.resize(chunks * CHUNK, 0);
        let (base, _) = self.range()?;
        let mut rows = BTreeMap::from([(base, sha(&payload).to_vec())]);
        rows.extend(
            payload
                .chunks_exact(CHUNK)
                .enumerate()
                .map(|(n, data)| (base + n as u64 + 1, data.to_vec())),
        );
        Ok(rows)
    }
    /// Decode only the selected range from the same snapshot. Partial/missing
    /// chunks fail; a completely absent range remains a proved-empty candidate.
    pub fn from_native_rows(
        self,
        rows: &BTreeMap<u64, Vec<u8>>,
        limits: &Limits,
    ) -> Result<Option<Vec<u8>>, Error> {
        let (base, end) = self.range()?;
        if rows.is_empty() {
            return Ok(None);
        }
        if rows.len() < 2 || rows.len() > 1024 || rows.get(&base).map(Vec::len) != Some(32) {
            return Err(fail("invalid corpus proof native checksum/chunk count"));
        }
        let mut bytes = Vec::new();
        for (n, (&key, data)) in rows.iter().enumerate().skip(1) {
            if key != base + n as u64 || key > end || data.len() != CHUNK {
                return Err(fail("noncontiguous/misrouted corpus proof chunks"));
            }
            bytes.extend(data);
        }
        if sha(&bytes).as_slice() != rows[&base] {
            return Err(fail("corpus proof native checksum mismatch"));
        }
        let length = u64::from_be_bytes(
            bytes[..8]
                .try_into()
                .map_err(|_| fail("invalid corpus proof native length"))?,
        );
        if length == u64::MAX {
            if bytes[8..].iter().any(|byte| *byte != 0) {
                return Err(fail("nonzero corpus proof native tombstone padding"));
            }
            return Ok(None);
        }
        let length = usize::try_from(length)
            .map_err(|_| fail("corpus proof native logical length overflow"))?;
        if length == 0 || length > limits.max_text_bytes || length > bytes.len() - 8 {
            return Err(fail("corpus proof native logical byte budget exceeded"));
        }
        let end = 8 + length;
        if bytes[end..].iter().any(|byte| *byte != 0) {
            return Err(fail("nonzero corpus proof native record padding"));
        }
        Ok(Some(bytes[8..end].to_vec()))
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scope {
    pub workspace: [u8; 8],
    pub graph: u32,
    /// SHA-256 of the frozen physical route descriptor, including file identity.
    pub routes: Hash,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum EntityKind {
    Node,
    Edge,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct EntityKey {
    pub kind: EntityKind,
    pub id: u64,
}
impl EntityKey {
    pub fn node(id: u64) -> Self {
        Self {
            kind: EntityKind::Node,
            id,
        }
    }
    pub fn edge(id: u64) -> Self {
        Self {
            kind: EntityKind::Edge,
            id,
        }
    }
    fn validate(self, next_id: u64) -> Result<(), Error> {
        if self.id == 0 || self.id >= next_id || next_id > 1 << 48 {
            return Err(fail("invalid corpus proof entity/allocator"));
        }
        Ok(())
    }
    fn tag(self) -> u8 {
        match self.kind {
            EntityKind::Node => 1,
            EntityKind::Edge => 2,
        }
    }
    fn bucket(self) -> u16 {
        let mut data = b"bic-graph-corpus-key-v1".to_vec();
        data.push(self.tag());
        data.extend(self.id.to_be_bytes());
        let hash = sha(&data);
        u16::from_be_bytes([hash[0], hash[1]]) >> 4
    }
}
/// One canonical entity digest, independent of its raw JSON/chunk representation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Contribution {
    pub key: EntityKey,
    pub canonical_bytes: u64,
    /// Node checksum plus original chunks, or native edge logical size plus 32.
    pub record_bytes: u64,
    pub digest: Hash,
}
impl Contribution {
    pub fn node(node: &Node, original_record_bytes: usize) -> Result<Self, Error> {
        Self::from_json(
            EntityKey::node(node.id),
            serde_json::json!({"id":node.id,"labels":node.labels,"properties":node.properties}),
            Some(original_record_bytes),
        )
    }
    pub fn canonical_node(node: &Node) -> Result<Self, Error> {
        let value =
            serde_json::json!({"id":node.id,"labels":node.labels,"properties":node.properties});
        let bytes = serde_json::to_vec(&value).map_err(|e| fail(e.to_string()))?;
        Self::from_json(
            EntityKey::node(node.id),
            value,
            Some(
                bytes
                    .len()
                    .checked_add(32)
                    .ok_or_else(|| fail("corpus node byte overflow"))?,
            ),
        )
    }
    pub fn edge(edge: &Edge, limits: &Limits) -> Result<Self, Error> {
        crate::adjacency_record::payload_size(edge, limits)?;
        Self::from_json(
            EntityKey::edge(edge.id),
            serde_json::json!({"id":edge.id,"source":edge.source,"target":edge.target,
                "label":edge.label,"properties":edge.properties}),
            None,
        )
    }
    fn from_json(
        key: EntityKey,
        value: serde_json::Value,
        bytes: Option<usize>,
    ) -> Result<Self, Error> {
        let data = serde_json::to_vec(&value).map_err(|e| fail(e.to_string()))?;
        let mut scoped = b"bic-graph-corpus-entity-v1".to_vec();
        scoped.push(key.tag());
        scoped.extend(key.id.to_be_bytes());
        scoped.extend(&data);
        let record_bytes = match bytes {
            Some(bytes) => bytes as u64,
            None => (data.len() as u64)
                .checked_add(32)
                .ok_or_else(|| fail("corpus size overflow"))?,
        };
        let result = Self {
            key,
            canonical_bytes: data.len() as u64,
            record_bytes,
            digest: sha(&scoped),
        };
        result.validate(1 << 48)?;
        Ok(result)
    }
    fn validate(self, next_id: u64) -> Result<(), Error> {
        self.key.validate(next_id)?;
        if self.canonical_bytes == 0
            || self.record_bytes <= 32
            || self.key.kind == EntityKind::Edge
                && self.canonical_bytes.checked_add(32) != Some(self.record_bytes)
        {
            return Err(fail("invalid corpus contribution"));
        }
        Ok(())
    }
    fn aggregate(self) -> Aggregate {
        Aggregate {
            nodes: u64::from(self.key.kind == EntityKind::Node),
            edges: u64::from(self.key.kind == EntityKind::Edge),
            canonical_bytes: self.canonical_bytes,
            record_bytes: self.record_bytes,
        }
    }
    fn encode(self, bytes: &mut Vec<u8>) {
        bytes.push(self.key.tag());
        for n in [self.key.id, self.canonical_bytes, self.record_bytes] {
            bytes.extend(n.to_be_bytes());
        }
        bytes.extend(self.digest);
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Aggregate {
    pub nodes: u64,
    pub edges: u64,
    pub canonical_bytes: u64,
    pub record_bytes: u64,
}
impl Aggregate {
    fn add(self, other: Self) -> Result<Self, Error> {
        let add = |a: u64, b: u64| {
            a.checked_add(b)
                .ok_or_else(|| fail("corpus aggregate overflow"))
        };
        Ok(Self {
            nodes: add(self.nodes, other.nodes)?,
            edges: add(self.edges, other.edges)?,
            canonical_bytes: add(self.canonical_bytes, other.canonical_bytes)?,
            record_bytes: add(self.record_bytes, other.record_bytes)?,
        })
    }
    fn encode(self, data: &mut Vec<u8>) {
        for n in [
            self.nodes,
            self.edges,
            self.canonical_bytes,
            self.record_bytes,
        ] {
            data.extend(n.to_be_bytes());
        }
    }
    fn validate(self, limits: &Limits) -> Result<(), Error> {
        let count = self
            .nodes
            .checked_add(self.edges)
            .ok_or_else(|| fail("corpus count overflow"))?;
        if self.nodes > limits.max_nodes as u64 || self.edges > limits.max_edges as u64 {
            return Err(fail("global graph element budget exceeded"));
        }
        if self.canonical_bytes > limits.max_text_bytes as u64
            || self.record_bytes > limits.max_text_bytes as u64
        {
            return Err(fail("global graph storage byte budget exceeded"));
        }
        if count == 0 && self != Self::default()
            || self.canonical_bytes < count
            || count
                .checked_mul(33)
                .map_or(true, |n| self.record_bytes < n)
        {
            return Err(fail("invalid corpus aggregate"));
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Commitment {
    aggregate: Aggregate,
    hash: Hash,
}
fn sha(bytes: &[u8]) -> Hash {
    let mut sha = Sha256::new();
    sha.update(bytes);
    sha.finalize()
}
fn height(node: usize) -> usize {
    12 - (usize::BITS - 1 - node.leading_zeros()) as usize
}
fn parent(left: Commitment, right: Commitment) -> Result<Commitment, Error> {
    let aggregate = left.aggregate.add(right.aggregate)?;
    let mut data = b"bic-graph-corpus-parent-v1".to_vec();
    left.aggregate.encode(&mut data);
    data.extend(left.hash);
    right.aggregate.encode(&mut data);
    data.extend(right.hash);
    Ok(Commitment {
        aggregate,
        hash: sha(&data),
    })
}
fn defaults() -> [Commitment; 13] {
    let empty = Commitment {
        aggregate: Aggregate::default(),
        hash: sha(b"bic-graph-corpus-empty-v1"),
    };
    let mut values = [empty; 13];
    for n in 1..values.len() {
        values[n] = parent(values[n - 1], values[n - 1]).expect("empty aggregate");
    }
    values
}
fn leaf(bucket: u16, entries: &BTreeMap<EntityKey, Contribution>) -> Result<Commitment, Error> {
    if entries.is_empty() {
        return Ok(defaults()[0]);
    }
    let mut data = b"bic-graph-corpus-bucket-v1".to_vec();
    data.extend(bucket.to_be_bytes());
    let mut aggregate = Aggregate::default();
    for value in entries.values() {
        aggregate = aggregate.add(value.aggregate())?;
        value.encode(&mut data);
    }
    Ok(Commitment {
        aggregate,
        hash: sha(&data),
    })
}
/// The independently persisted root. The SQL manifest must match its corpus,
/// generation and scope; these fields cannot be inferred from a partial cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Root {
    pub scope: Scope,
    pub next_id: u64,
    pub generation: u64,
    pub aggregate: Aggregate,
    pub hash: Hash,
}
impl Root {
    fn commitment(self) -> Commitment {
        Commitment {
            aggregate: self.aggregate,
            hash: self.hash,
        }
    }
    pub fn corpus(self, limits: &Limits) -> Result<GraphCorpus, Error> {
        self.aggregate.validate(limits)?;
        if self.scope.graph == 0
            || self.generation == 0
            || self.next_id == 0
            || self.next_id > 1 << 48
        {
            return Err(fail("invalid corpus root scope/generation/allocator"));
        }
        if self.aggregate == Aggregate::default() && self.hash != defaults()[12].hash {
            return Err(fail("invalid empty corpus root"));
        }
        let logical = serde_json::to_vec(&serde_json::json!({"format":"bicdb-graph-v1",
            "next_id":self.next_id,"nodes":[],"edges":[]}))
        .map_err(|e| fail(e.to_string()))?
        .len() as u64;
        let physical = serde_json::to_vec(&serde_json::json!({"format":"bicdb-graph-records-v2",
            "next_id":self.next_id,"nodes":self.aggregate.nodes,"edges":self.aggregate.edges}))
        .map_err(|e| fail(e.to_string()))?
        .len() as u64;
        let logical_bytes = logical
            .checked_add(self.aggregate.canonical_bytes)
            .and_then(|n| n.checked_add(self.aggregate.nodes.saturating_sub(1)))
            .and_then(|n| n.checked_add(self.aggregate.edges.saturating_sub(1)))
            .ok_or_else(|| fail("corpus logical size overflow"))?;
        let record_bytes = physical
            .checked_add(self.aggregate.record_bytes)
            .ok_or_else(|| fail("corpus record size overflow"))?;
        let corpus = GraphCorpus {
            next_id: self.next_id,
            nodes: self.aggregate.nodes,
            edges: self.aggregate.edges,
            logical_bytes,
            record_bytes,
        };
        corpus.validate(limits)?;
        Ok(corpus)
    }
    pub fn verify_manifest(
        self,
        scope: Scope,
        generation: u64,
        corpus: GraphCorpus,
        limits: &Limits,
    ) -> Result<(), Error> {
        if self.scope != scope || self.generation != generation || self.corpus(limits)? != corpus {
            return Err(fail("independent corpus proof differs from manifest"));
        }
        Ok(())
    }
    pub fn encode(self, limits: &Limits) -> Result<Vec<u8>, Error> {
        self.corpus(limits)?;
        let mut data = ROOT_MAGIC.to_vec();
        data.extend(self.scope.workspace);
        data.extend(self.scope.graph.to_be_bytes());
        data.extend(self.scope.routes);
        data.extend(self.next_id.to_be_bytes());
        data.extend(self.generation.to_be_bytes());
        self.aggregate.encode(&mut data);
        data.extend(self.hash);
        data.extend(sha(&data));
        debug_assert_eq!(data.len(), ROOT_LEN);
        Ok(data)
    }
    pub fn decode(data: &[u8], expected: Scope, limits: &Limits) -> Result<Self, Error> {
        if data.len() != ROOT_LEN
            || &data[..8] != ROOT_MAGIC
            || sha(&data[..ROOT_LEN - 32]) != data[ROOT_LEN - 32..]
        {
            return Err(fail("invalid corpus root checksum/format"));
        }
        let root = Self {
            scope: Scope {
                workspace: data[8..16].try_into().expect("length"),
                graph: u32::from_be_bytes(data[16..20].try_into().expect("length")),
                routes: data[20..52].try_into().expect("length"),
            },
            next_id: number(data, 52),
            generation: number(data, 60),
            aggregate: aggregate(data, 68),
            hash: data[100..132].try_into().expect("length"),
        };
        if root.scope != expected {
            return Err(fail("corpus root scope/routes mismatch"));
        }
        root.corpus(limits)?;
        Ok(root)
    }
}
fn number(data: &[u8], offset: usize) -> u64 {
    u64::from_be_bytes(
        data[offset..offset + 8]
            .try_into()
            .expect("validated length"),
    )
}
fn aggregate(data: &[u8], offset: usize) -> Aggregate {
    Aggregate {
        nodes: number(data, offset),
        edges: number(data, offset + 8),
        canonical_bytes: number(data, offset + 16),
        record_bytes: number(data, offset + 24),
    }
}
fn seal(mut data: Vec<u8>) -> Vec<u8> {
    data.extend(sha(&data));
    data
}
fn unseal(data: &[u8], unit: usize) -> Result<&[u8], Error> {
    if data.len() <= 32
        || (data.len() - 32) % unit != 0
        || sha(&data[..data.len() - 32]) != data[data.len() - 32..]
    {
        return Err(fail("invalid corpus proof record checksum/length"));
    }
    Ok(&data[..data.len() - 32])
}
fn encode_bucket(entries: &BTreeMap<EntityKey, Contribution>) -> Vec<u8> {
    let mut data = Vec::new();
    for value in entries.values() {
        value.encode(&mut data);
    }
    seal(data)
}
fn decode_bucket(
    data: &[u8],
    bucket: u16,
    next_id: u64,
    limits: &Limits,
) -> Result<BTreeMap<EntityKey, Contribution>, Error> {
    let data = unseal(data, LEAF_LEN)?;
    if bucket >= BUCKETS as u16
        || data.len() / LEAF_LEN > limits.max_nodes.saturating_add(limits.max_edges)
    {
        return Err(fail("corpus proof bucket count/routing budget exceeded"));
    }
    let mut entries = BTreeMap::new();
    let mut previous = None;
    for data in data.chunks_exact(LEAF_LEN) {
        let kind = match data[0] {
            1 => EntityKind::Node,
            2 => EntityKind::Edge,
            _ => return Err(fail("invalid corpus entity kind")),
        };
        let key = EntityKey {
            kind,
            id: number(data, 1),
        };
        let value = Contribution {
            key,
            canonical_bytes: number(data, 9),
            record_bytes: number(data, 17),
            digest: data[25..57].try_into().expect("length"),
        };
        value.validate(next_id)?;
        if key.bucket() != bucket || previous.is_some_and(|old| old >= key) {
            return Err(fail("misrouted/duplicate/out-of-order corpus entity"));
        }
        previous = Some(key);
        entries.insert(key, value);
    }
    leaf(bucket, &entries)?.aggregate.validate(limits)?;
    Ok(entries)
}
fn encode_page(entries: &BTreeMap<usize, Commitment>) -> Vec<u8> {
    let mut data = Vec::new();
    for (n, value) in entries {
        data.extend((*n as u16).to_be_bytes());
        value.aggregate.encode(&mut data);
        data.extend(value.hash);
    }
    seal(data)
}
fn decode_page(
    data: &[u8],
    page: u8,
    limits: &Limits,
) -> Result<BTreeMap<usize, Commitment>, Error> {
    let data = unseal(data, HASH_LEN)?;
    if data.len() / HASH_LEN > PAGE || page > 127 {
        return Err(fail("invalid corpus hash page count/routing"));
    }
    let mut values = BTreeMap::new();
    let mut previous = 0;
    let defaults = defaults();
    for data in data.chunks_exact(HASH_LEN) {
        let n = usize::from(u16::from_be_bytes(data[..2].try_into().expect("length")));
        if n == 0 || n >= BUCKETS * 2 || n / PAGE != usize::from(page) || n <= previous {
            return Err(fail("misrouted/duplicate/out-of-order corpus hash node"));
        }
        let value = Commitment {
            aggregate: aggregate(data, 2),
            hash: data[34..66].try_into().expect("length"),
        };
        value.aggregate.validate(limits)?;
        if value == defaults[height(n)] || value.aggregate == Aggregate::default() {
            return Err(fail("noncanonical empty corpus hash node"));
        }
        previous = n;
        values.insert(n, value);
    }
    Ok(values)
}
struct Meter {
    limits: Limits,
    clock: Deadline,
    work: usize,
    bytes: usize,
}
impl Meter {
    fn new(limits: &Limits, clock: Deadline) -> Self {
        Self {
            limits: limits.clone(),
            clock,
            work: 0,
            bytes: 0,
        }
    }
    fn charge(&mut self, work: usize, bytes: usize) -> Result<(), Error> {
        self.clock.check("corpus proof")?;
        self.work = self
            .work
            .checked_add(work)
            .ok_or_else(|| fail("corpus proof work overflow"))?;
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .ok_or_else(|| fail("corpus proof byte overflow"))?;
        if self.work > self.limits.max_expansions || self.bytes > self.limits.max_text_bytes {
            return Err(fail("corpus proof work/byte budget exceeded"));
        }
        Ok(())
    }
}
/// Complete initial proof or migration result. SQL persists all records together.
#[derive(Debug)]
pub struct Image {
    pub root: Root,
    pub records: BTreeMap<RecordKey, Vec<u8>>,
    pub work: usize,
}
impl Image {
    pub fn build(
        scope: Scope,
        next_id: u64,
        generation: u64,
        entities: impl IntoIterator<Item = Contribution>,
        limits: &Limits,
        clock: Deadline,
    ) -> Result<Self, Error> {
        let mut meter = Meter::new(limits, clock);
        let mut buckets = BTreeMap::<u16, BTreeMap<EntityKey, Contribution>>::new();
        for entity in entities {
            meter.charge(1, LEAF_LEN)?;
            entity.validate(next_id)?;
            if buckets
                .entry(entity.key.bucket())
                .or_default()
                .insert(entity.key, entity)
                .is_some()
            {
                return Err(fail("duplicate corpus proof entity"));
            }
        }
        // Graph identities share one allocator; a node and edge cannot share ID.
        let ids: BTreeSet<_> = buckets
            .values()
            .flat_map(|b| b.keys().map(|k| k.id))
            .collect();
        if ids.len() != buckets.values().map(BTreeMap::len).sum::<usize>() {
            return Err(fail("duplicate corpus proof logical identity"));
        }
        let empty = defaults();
        let mut nodes = BTreeMap::new();
        let mut records = BTreeMap::new();
        for (b, entries) in buckets {
            let commitment = leaf(b, &entries)?;
            commitment.aggregate.validate(limits)?;
            nodes.insert(BUCKETS + usize::from(b), commitment);
            records.insert(RecordKey::Bucket(b), encode_bucket(&entries));
        }
        let mut frontier: BTreeSet<_> = nodes.keys().map(|n| n / 2).collect();
        while !frontier.is_empty() {
            let mut next = BTreeSet::new();
            for n in frontier {
                meter.charge(1, 0)?;
                let default = empty[height(n) - 1];
                let value = parent(
                    *nodes.get(&(n * 2)).unwrap_or(&default),
                    *nodes.get(&(n * 2 + 1)).unwrap_or(&default),
                )?;
                nodes.insert(n, value);
                if n > 1 {
                    next.insert(n / 2);
                }
            }
            frontier = next;
        }
        let value = *nodes.get(&1).unwrap_or(&empty[12]);
        let root = Root {
            scope,
            next_id,
            generation,
            aggregate: value.aggregate,
            hash: value.hash,
        };
        let mut pages = BTreeMap::<u8, BTreeMap<usize, Commitment>>::new();
        for (n, value) in nodes {
            pages.entry((n / PAGE) as u8).or_default().insert(n, value);
        }
        for (p, entries) in pages {
            records.insert(RecordKey::HashPage(p), encode_page(&entries));
        }
        records.insert(RecordKey::Root, root.encode(limits)?);
        // Output is budgeted separately from the temporary contribution stream.
        let bytes = records
            .values()
            .try_fold(0usize, |n, b| n.checked_add(b.len()))
            .ok_or_else(|| fail("corpus proof size overflow"))?;
        if bytes > limits.max_text_bytes {
            return Err(fail("corpus proof output byte budget exceeded"));
        }
        Ok(Self {
            root,
            records,
            work: meter.work,
        })
    }
    /// Full migration/recovery verification rejects extra and noncanonical
    /// proof rows as well as missing buckets/pages. Ordinary point reads use
    /// Reader instead and never perform this complete reconstruction.
    pub fn decode(
        records: BTreeMap<RecordKey, Vec<u8>>,
        scope: Scope,
        limits: &Limits,
        clock: Deadline,
    ) -> Result<Self, Error> {
        let mut meter = Meter::new(limits, clock);
        let root = Root::decode(
            records
                .get(&RecordKey::Root)
                .ok_or_else(|| fail("missing independent corpus proof root"))?,
            scope,
            limits,
        )?;
        let mut contributions = Vec::new();
        for (key, data) in &records {
            key.range()?;
            meter.charge(1, data.len())?;
            match key {
                RecordKey::Bucket(b) => contributions
                    .extend(decode_bucket(data, *b, root.next_id, limits)?.into_values()),
                RecordKey::HashPage(p) => {
                    decode_page(data, *p, limits)?;
                }
                RecordKey::Root => {}
            }
        }
        let mut image = Self::build(
            scope,
            root.next_id,
            root.generation,
            contributions,
            limits,
            clock,
        )?;
        meter.charge(image.work, 0)?;
        if image.root != root || image.records != records {
            return Err(fail("complete corpus proof differs from records"));
        }
        image.work = meter.work;
        Ok(image)
    }
    pub fn decode_native(
        rows: BTreeMap<u64, Vec<u8>>,
        scope: Scope,
        limits: &Limits,
        clock: Deadline,
    ) -> Result<Self, Error> {
        let mut physical = BTreeMap::<RecordKey, BTreeMap<u64, Vec<u8>>>::new();
        for (ordinal, data) in rows {
            physical
                .entry(RecordKey::from_native_ordinal(ordinal)?)
                .or_default()
                .insert(ordinal, data);
        }
        let mut logical = BTreeMap::new();
        for (key, rows) in physical {
            if let Some(data) = key.from_native_rows(&rows, limits)? {
                logical.insert(key, data);
            }
        }
        Self::decode(logical, scope, limits, clock)
    }
    /// Complete physical kind-5 rows for CREATE, migration and route rebuild.
    pub fn native_patch(&self, limits: &Limits) -> Result<StoragePatch, Error> {
        let mut patch = StoragePatch::default();
        for (key, data) in &self.records {
            for (ordinal, data) in key.native_rows(data, limits)? {
                if patch.inserted.insert(ordinal, data).is_some() {
                    return Err(fail("duplicate corpus proof native ordinal"));
                }
            }
        }
        Ok(patch)
    }
    pub fn replace_native_patch(
        &self,
        source: &BTreeMap<u64, Vec<u8>>,
        limits: &Limits,
    ) -> Result<StoragePatch, Error> {
        let mut physical = BTreeMap::<RecordKey, BTreeMap<u64, Vec<u8>>>::new();
        for (&ordinal, data) in source {
            physical
                .entry(RecordKey::from_native_ordinal(ordinal)?)
                .or_default()
                .insert(ordinal, data.clone());
        }
        let mut patch = StoragePatch::default();
        let keys: BTreeSet<_> = physical
            .keys()
            .copied()
            .chain(self.records.keys().copied())
            .collect();
        for key in keys {
            let old = physical.get(&key);
            let minimum = old.map_or(1, |rows| rows.len().saturating_sub(1).max(1));
            let desired =
                key.native_rows_min(self.records.get(&key).map(Vec::as_slice), minimum, limits)?;
            for (&ordinal, data) in &desired {
                match source.get(&ordinal) {
                    Some(before) if before == data => {}
                    Some(_) => {
                        patch.updated.insert(ordinal, data.clone());
                    }
                    None => {
                        patch.inserted.insert(ordinal, data.clone());
                    }
                }
            }
        }
        Ok(patch)
    }
}
/// Every replacement includes the first source contribution. Missing source
/// data or a digest mismatch cannot be interpreted as deletion/uniqueness proof.
#[derive(Clone, Copy, Debug)]
pub struct Change {
    pub key: EntityKey,
    pub before: Option<Contribution>,
    pub after: Option<Contribution>,
}
/// SQL applies this complete record delta in its existing statement savepoint.
#[derive(Debug)]
pub struct Patch {
    pub root: Root,
    pub written: BTreeMap<RecordKey, Vec<u8>>,
    pub removed: Vec<RecordKey>,
}
impl Patch {
    /// Translate a logical proof delta to graph heap rows. `source` contains
    /// the complete old physical range for every written/removed logical key.
    pub fn native_patch(
        &self,
        source: &BTreeMap<RecordKey, BTreeMap<u64, Vec<u8>>>,
        limits: &Limits,
    ) -> Result<StoragePatch, Error> {
        let mut patch = StoragePatch::default();
        let changed: BTreeSet<_> = self
            .written
            .keys()
            .copied()
            .chain(self.removed.iter().copied())
            .collect();
        for key in changed {
            let old = source
                .get(&key)
                .ok_or_else(|| fail("missing source corpus proof physical range"))?;
            let new = key.native_rows_min(
                self.written.get(&key).map(Vec::as_slice),
                old.len().saturating_sub(1).max(1),
                limits,
            )?;
            for (&ordinal, data) in &new {
                match old.get(&ordinal) {
                    Some(before) if before == data => {}
                    Some(_) => {
                        patch.updated.insert(ordinal, data.clone());
                    }
                    None => {
                        patch.inserted.insert(ordinal, data.clone());
                    }
                }
            }
        }
        Ok(patch)
    }
}
/// Snapshot loader. It reads only selected buckets and shared Merkle pages.
/// The callback must resolve all records through the same database ReadView.
pub struct Reader<F> {
    root: Root,
    fetch: F,
    meter: Meter,
    pages: BTreeMap<u8, BTreeMap<usize, Commitment>>,
    buckets: BTreeMap<u16, BTreeMap<EntityKey, Contribution>>,
    empty: [Commitment; 13],
}
impl<F: FnMut(RecordKey) -> Result<Option<Vec<u8>>, Error>> Reader<F> {
    pub fn open(
        scope: Scope,
        limits: &Limits,
        clock: Deadline,
        mut fetch: F,
    ) -> Result<Self, Error> {
        let mut meter = Meter::new(limits, clock);
        meter.charge(1, 0)?;
        let data =
            fetch(RecordKey::Root)?.ok_or_else(|| fail("missing independent corpus proof root"))?;
        meter.charge(0, data.len())?;
        let root = Root::decode(&data, scope, limits)?;
        Ok(Self {
            root,
            fetch,
            meter,
            pages: BTreeMap::new(),
            buckets: BTreeMap::new(),
            empty: defaults(),
        })
    }
    pub fn work(&self) -> usize {
        self.meter.work
    }
    pub fn bytes(&self) -> usize {
        self.meter.bytes
    }
    pub fn root(&self) -> Root {
        self.root
    }
    fn load_page(&mut self, page: u8) -> Result<(), Error> {
        if !self.pages.contains_key(&page) {
            self.meter.charge(1, 0)?;
            let values = match (self.fetch)(RecordKey::HashPage(page))? {
                Some(data) => {
                    self.meter.charge(0, data.len())?;
                    decode_page(&data, page, &self.meter.limits)?
                }
                None => BTreeMap::new(),
            };
            self.pages.insert(page, values);
        }
        Ok(())
    }
    fn node(&mut self, n: usize) -> Result<Commitment, Error> {
        let page = (n / PAGE) as u8;
        self.load_page(page)?;
        Ok(*self.pages[&page].get(&n).unwrap_or(&self.empty[height(n)]))
    }
    fn load_bucket(&mut self, b: u16) -> Result<(), Error> {
        if self.buckets.contains_key(&b) {
            return Ok(());
        }
        self.meter.charge(1, 0)?;
        let entries = match (self.fetch)(RecordKey::Bucket(b))? {
            Some(data) => {
                self.meter.charge(0, data.len())?;
                decode_bucket(&data, b, self.root.next_id, &self.meter.limits)?
            }
            None => BTreeMap::new(),
        };
        self.meter.charge(entries.len(), 0)?;
        let mut value = leaf(b, &entries)?;
        let mut n = BUCKETS + usize::from(b);
        // Verify both the bucket and all aggregate/digest ancestors. Default
        // absence is valid only if its complete path matches the frozen root.
        loop {
            self.meter.charge(1, 0)?;
            if self.node(n)? != value {
                return Err(fail("corpus proof bucket/ancestor differs"));
            }
            if n == 1 {
                break;
            }
            let sibling = self.node(n ^ 1)?;
            value = if n % 2 == 0 {
                parent(value, sibling)?
            } else {
                parent(sibling, value)?
            };
            n /= 2;
        }
        if value != self.root.commitment() {
            return Err(fail("corpus Merkle proof differs from independent root"));
        }
        self.buckets.insert(b, entries);
        Ok(())
    }
    /// Missing entity is a proved absence, not an unloaded cache entry.
    pub fn entity(&mut self, key: EntityKey) -> Result<Option<Contribution>, Error> {
        // An ID at/after the old high water can still require proved absence
        // before CREATE. It must fit the implementation's global allocator.
        key.validate(1 << 48)?;
        self.load_bucket(key.bucket())?;
        Ok(self.buckets[&key.bucket()].get(&key).copied())
    }
    pub fn verify_source(
        &mut self,
        key: EntityKey,
        source: Option<Contribution>,
    ) -> Result<(), Error> {
        if source.is_some_and(|c| c.key != key) || self.entity(key)? != source {
            return Err(fail("source entity differs from independent corpus proof"));
        }
        Ok(())
    }
    /// Build an atomic candidate; the reader/root remain on the old snapshot.
    pub fn patch(
        &mut self,
        changes: &[Change],
        next_id: u64,
        generation: u64,
    ) -> Result<Patch, Error> {
        if next_id < self.root.next_id
            || next_id > 1 << 48
            || self.root.generation.checked_add(1) != Some(generation)
        {
            return Err(fail("invalid corpus patch allocator/generation"));
        }
        let mut seen = BTreeMap::new();
        let mut affected = BTreeMap::<u16, Vec<&Change>>::new();
        for change in changes {
            self.meter.charge(1, 0)?;
            if seen.insert(change.key, change.after).is_some()
                || change.before.is_none() && change.after.is_none()
            {
                return Err(fail("duplicate/empty corpus change"));
            }
            self.verify_source(change.key, change.before)?;
            if let Some(after) = change.after {
                after.validate(next_id)?;
                if change.before.is_none() && after.key.id < self.root.next_id {
                    return Err(fail("corpus creation cannot reuse an allocated identity"));
                }
                if after.key != change.key {
                    return Err(fail("corpus change identity differs"));
                }
            }
            affected
                .entry(change.key.bucket())
                .or_default()
                .push(change);
        }
        // Check allocator-wide identity sharing, including the other entity kind.
        for change in changes
            .iter()
            .filter(|c| c.before.is_none() && c.after.is_some())
        {
            let other = EntityKey {
                id: change.key.id,
                kind: match change.key.kind {
                    EntityKind::Node => EntityKind::Edge,
                    EntityKind::Edge => EntityKind::Node,
                },
            };
            if seen.get(&other).is_some_and(Option::is_some) || self.entity(other)?.is_some() {
                return Err(fail("duplicate corpus proof logical identity"));
            }
        }
        let mut overlay = BTreeMap::new();
        let mut written = BTreeMap::new();
        let mut removed = Vec::new();
        for (b, changes) in &affected {
            let mut entries = self.buckets[b].clone();
            for change in changes {
                match change.after {
                    Some(value) => {
                        entries.insert(change.key, value);
                    }
                    None => {
                        entries.remove(&change.key);
                    }
                }
            }
            let value = leaf(*b, &entries)?;
            value.aggregate.validate(&self.meter.limits)?;
            overlay.insert(BUCKETS + usize::from(*b), value);
            if entries.is_empty() {
                removed.push(RecordKey::Bucket(*b));
            } else {
                written.insert(RecordKey::Bucket(*b), encode_bucket(&entries));
            }
        }
        let mut frontier: BTreeSet<_> = overlay.keys().map(|n| n / 2).collect();
        while !frontier.is_empty() {
            let mut next = BTreeSet::new();
            for n in frontier {
                self.meter.charge(1, 0)?;
                let left = match overlay.get(&(n * 2)) {
                    Some(value) => *value,
                    None => self.node(n * 2)?,
                };
                let right = match overlay.get(&(n * 2 + 1)) {
                    Some(value) => *value,
                    None => self.node(n * 2 + 1)?,
                };
                overlay.insert(n, parent(left, right)?);
                if n > 1 {
                    next.insert(n / 2);
                }
            }
            frontier = next;
        }
        let value = *overlay.get(&1).unwrap_or(&self.root.commitment());
        let root = Root {
            next_id,
            generation,
            aggregate: value.aggregate,
            hash: value.hash,
            ..self.root
        };
        root.corpus(&self.meter.limits)?;
        let touched: BTreeSet<_> = overlay.keys().map(|n| (n / PAGE) as u8).collect();
        for page in touched {
            self.load_page(page)?;
            let mut values = self.pages[&page].clone();
            for (n, value) in overlay
                .iter()
                .filter(|(n, _)| **n / PAGE == usize::from(page))
            {
                if *value == self.empty[height(*n)] {
                    values.remove(n);
                } else {
                    values.insert(*n, *value);
                }
            }
            if values.is_empty() {
                removed.push(RecordKey::HashPage(page));
            } else {
                written.insert(RecordKey::HashPage(page), encode_page(&values));
            }
        }
        written.insert(RecordKey::Root, root.encode(&self.meter.limits)?);
        let bytes = written
            .values()
            .try_fold(0usize, |n, b| n.checked_add(b.len()))
            .ok_or_else(|| fail("corpus patch byte overflow"))?;
        self.meter.charge(0, bytes)?;
        Ok(Patch {
            root,
            written,
            removed,
        })
    }
}
