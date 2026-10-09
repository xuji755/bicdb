//! Versioned entity records over the native transactional graph heap.
//!
//! The manifest is key 0. Node/edge keys contain kind, 48-bit element identity,
//! and a 10-bit chunk ordinal. Chunk 0 is a SHA-256 checksum. Every entity has
//! independent chunks, so modifying one entity does not rewrite its neighbors.
use crate::model::fail;
use crate::{Edge, Error, Graph, GraphChanges, GraphCorpus, Limits, Node, Properties};
use bicdb_common::sha256::Sha256;
use serde_json::Value as Json;
use std::collections::{BTreeMap, BTreeSet};

const FORMAT: &str = "bicdb-graph-records-v2";
const CHUNK: usize = 4096;
const KIND_SHIFT: u32 = 60;
const ID_SHIFT: u32 = 10;
const CHUNK_MASK: u64 = (1 << ID_SHIFT) - 1;

#[derive(Debug, Clone)]
pub struct StorageImage {
    rows: BTreeMap<u64, Vec<u8>>,
    legacy: bool,
    adjacency: bool,
    stored_bytes: usize,
    /// Logical canonical sizes only; native edge payloads/topology are never rows.
    edge_sizes: BTreeMap<u64, usize>,
    corpus: Option<GraphCorpus>,
    proof_rows: BTreeMap<u64, Vec<u8>>,
}

/// Heap record changes, computed before starting a storage transaction.
/// Native edges have a separate topology plan; an empty patch does not imply
/// that a native graph statement has no edge operations.
#[derive(Debug, Clone, Default)]
pub struct StoragePatch {
    pub inserted: BTreeMap<u64, Vec<u8>>,
    pub updated: BTreeMap<u64, Vec<u8>>,
    pub removed: Vec<u64>,
    pub migrated: bool,
    pub entities_changed: usize,
    /// Complete final native corpus bytes, including logical manifest/checksums.
    /// Auxiliary and legacy patches do not establish native corpus authority.
    pub after_record_bytes: Option<usize>,
    pub after_corpus: Option<GraphCorpus>,
}
impl StoragePatch {
    pub fn is_empty(&self) -> bool {
        self.inserted.is_empty() && self.updated.is_empty() && self.removed.is_empty()
    }
    pub fn rows_changed(&self) -> usize {
        self.inserted.len() + self.updated.len() + self.removed.len()
    }
}

fn digest(bytes: &[u8]) -> Vec<u8> {
    let mut sha = Sha256::new();
    sha.update(bytes);
    sha.finalize().to_vec()
}
fn base(kind: u64, id: u64) -> u64 {
    (kind << KIND_SHIFT) | (id << ID_SHIFT)
}
fn identity(key: u64) -> Result<(u64, u64), Error> {
    let kind = key >> KIND_SHIFT;
    let id = (key & ((1 << KIND_SHIFT) - 1)) >> ID_SHIFT;
    if ![1, 2].contains(&kind) || id == 0 || id >= 1 << 48 {
        return Err(fail("invalid graph record key"));
    }
    Ok((kind, id))
}
fn encode_record(kind: u64, id: u64, record: Json) -> Result<BTreeMap<u64, Vec<u8>>, Error> {
    let bytes = serde_json::to_vec(&record).map_err(|e| fail(e.to_string()))?;
    if bytes.len().div_ceil(CHUNK) > CHUNK_MASK as usize {
        return Err(fail("graph entity record exceeds 1023 chunks"));
    }
    let key = base(kind, id);
    let mut result = BTreeMap::from([(key, digest(&bytes))]);
    result.extend(
        bytes
            .chunks(CHUNK)
            .enumerate()
            .map(|(i, bytes)| (key + i as u64 + 1, bytes.to_vec())),
    );
    Ok(result)
}
fn node(graph: &Graph, id: u64) -> Json {
    let n = &graph.nodes()[&id];
    serde_json::json!({"id":id,"labels":n.labels,"properties":n.properties})
}
fn edge(graph: &Graph, id: u64) -> Json {
    let e = &graph.edges()[&id];
    serde_json::json!({"id":id,"source":e.source,"target":e.target,"label":e.label,"properties":e.properties})
}
fn native_edge_size(edge: &Edge, limits: &Limits) -> Result<usize, Error> {
    // Preserve complete canonical corpus budgeting, including the logical edge
    // checksum, but do not apply the legacy 1023-chunk cap to topology metadata.
    crate::adjacency_record::payload_size(edge, limits)?;
    let bytes = serde_json::to_vec(&serde_json::json!({"id":edge.id,"source":edge.source,
        "target":edge.target,"label":edge.label,"properties":edge.properties}))
    .map_err(|e| fail(e.to_string()))?;
    Ok(bytes.len().saturating_add(32))
}
fn manifest(graph: &Graph) -> Result<Vec<u8>, Error> {
    manifest_values(
        graph.allocator_high_water(),
        graph.nodes().len() as u64,
        graph.edges().len() as u64,
    )
}
fn manifest_values(next_id: u64, nodes: u64, edges: u64) -> Result<Vec<u8>, Error> {
    serde_json::to_vec(
        &serde_json::json!({"format":FORMAT,"next_id":next_id,"nodes":nodes,"edges":edges}),
    )
    .map_err(|e| fail(e.to_string()))
}
fn integer(json: &Json, field: &str) -> Result<u64, Error> {
    json[field]
        .as_u64()
        .ok_or_else(|| fail(format!("invalid graph record {field}")))
}
fn props(json: &Json) -> Result<Properties, Error> {
    Ok(json["properties"]
        .as_object()
        .ok_or_else(|| fail("invalid graph record properties"))?
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect())
}

/// Native ordinal range for a single entity (checksum and up to 1023 chunks).
pub fn entity_range(kind: u64, id: u64) -> Result<(u64, u64), Error> {
    if ![1, 2].contains(&kind) || id == 0 || id >= 1 << 48 {
        return Err(fail("invalid graph entity identity"));
    }
    let lower = base(kind, id);
    Ok((lower, lower + CHUNK_MASK))
}
/// Initialize an empty cache from a v2 manifest. `None` denotes a legacy
/// snapshot, whose reader must use the existing full validation path.
pub fn snapshot_cache(header: Option<&[u8]>, limits: &Limits) -> Result<Option<Graph>, Error> {
    let Some(header) = header else {
        return Ok(Some(Graph::new()));
    };
    if header.len() == 32 {
        return Ok(None);
    }
    if header.len() > CHUNK {
        return Err(fail("oversized graph manifest"));
    }
    let meta: Json = serde_json::from_slice(header).map_err(|_| fail("invalid graph manifest"))?;
    if meta["format"] != FORMAT {
        return Err(fail("unsupported graph storage format"));
    }
    if integer(&meta, "nodes")? > limits.max_nodes as u64
        || integer(&meta, "edges")? > limits.max_edges as u64
    {
        return Err(fail("graph element budget exceeded"));
    }
    let mut graph = Graph::new();
    graph.finish_load(integer(&meta, "next_id")?)?;
    Ok(Some(graph))
}
fn decode_entity(
    kind: u64,
    id: u64,
    rows: &BTreeMap<u64, Vec<u8>>,
    limits: &Limits,
) -> Result<Option<Json>, Error> {
    if rows.is_empty() {
        return Ok(None);
    }
    let (lower, upper) = entity_range(kind, id)?;
    let checksum = rows
        .get(&lower)
        .ok_or_else(|| fail("missing graph entity checksum"))?;
    if checksum.len() != 32 || rows.len() > 1024 {
        return Err(fail("invalid graph entity checksum/chunk count"));
    }
    let mut bytes = Vec::new();
    for (i, (&key, data)) in rows.iter().enumerate().skip(1) {
        if key != lower + i as u64
            || key > upper
            || data.is_empty()
            || data.len() > CHUNK
            || (!bytes.is_empty() && bytes.len() % CHUNK != 0)
        {
            return Err(fail("noncontiguous graph entity chunks"));
        }
        if bytes.len().saturating_add(data.len()) > limits.max_text_bytes {
            return Err(fail("graph entity byte budget exceeded"));
        }
        bytes.extend(data);
    }
    if bytes.is_empty() || digest(&bytes) != *checksum {
        return Err(fail("graph entity checksum mismatch"));
    }
    let record: Json =
        serde_json::from_slice(&bytes).map_err(|_| fail("invalid graph entity JSON"))?;
    if integer(&record, "id")? != id {
        return Err(fail("graph record ID differs from key"));
    }
    Ok(Some(record))
}
/// Decode exactly one snapshot entity, never unrelated graph rows.
pub fn decode_node(
    id: u64,
    rows: &BTreeMap<u64, Vec<u8>>,
    limits: &Limits,
) -> Result<Option<Node>, Error> {
    let Some(record) = decode_entity(1, id, rows, limits)? else {
        return Ok(None);
    };
    let labels = record["labels"]
        .as_array()
        .ok_or_else(|| fail("invalid graph record labels"))?
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or_else(|| fail("invalid graph label"))
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    let properties = props(&record)?;
    crate::model::validate_properties(&properties)?;
    Ok(Some(Node {
        id,
        labels,
        properties,
    }))
}
/// Endpoints are checked against the same snapshot by the executor after fetch.
pub fn decode_edge(
    id: u64,
    rows: &BTreeMap<u64, Vec<u8>>,
    limits: &Limits,
) -> Result<Option<Edge>, Error> {
    let Some(record) = decode_entity(2, id, rows, limits)? else {
        return Ok(None);
    };
    let label = record["label"]
        .as_str()
        .ok_or_else(|| fail("invalid relationship type"))?
        .to_owned();
    let source = integer(&record, "source")?;
    let target = integer(&record, "target")?;
    if label.is_empty() || source == 0 || target == 0 || source >= 1 << 48 || target >= 1 << 48 {
        return Err(fail("invalid relationship endpoint/type"));
    }
    let properties = props(&record)?;
    crate::model::validate_properties(&properties)?;
    Ok(Some(Edge {
        id,
        source,
        target,
        label,
        properties,
    }))
}

impl StorageImage {
    /// Canonical entity contributions for an independently persisted corpus
    /// proof. Existing native node rows retain their exact physical byte size;
    /// a legacy/migration image uses the canonical target node representation.
    pub fn corpus_contributions(
        &self,
        graph: &Graph,
        limits: &Limits,
    ) -> Result<Vec<crate::corpus_proof::Contribution>, Error> {
        let mut result = Vec::with_capacity(graph.nodes().len() + graph.edges().len());
        for entity in graph.nodes().values() {
            let bytes = if self.adjacency {
                let (lower, upper) = entity_range(1, entity.id)?;
                let selected = self.rows.range(lower..=upper).collect::<Vec<_>>();
                if decode_node(
                    entity.id,
                    &selected
                        .iter()
                        .map(|(key, value)| (**key, (*value).clone()))
                        .collect(),
                    limits,
                )?
                .as_ref()
                    != Some(entity)
                {
                    return Err(fail("corpus proof node differs from native source"));
                }
                selected.iter().try_fold(0usize, |total, (_, data)| {
                    total
                        .checked_add(data.len())
                        .ok_or_else(|| fail("corpus proof node byte overflow"))
                })?
            } else {
                encode_record(1, entity.id, node(graph, entity.id))?
                    .values()
                    .try_fold(0usize, |total, data| {
                        total
                            .checked_add(data.len())
                            .ok_or_else(|| fail("corpus proof node byte overflow"))
                    })?
            };
            result.push(crate::corpus_proof::Contribution::node(entity, bytes)?);
        }
        for edge in graph.edges().values() {
            result.push(crate::corpus_proof::Contribution::edge(edge, limits)?);
        }
        Ok(result)
    }
    pub fn final_corpus_contributions(
        &self,
        changes: Option<&GraphChanges>,
        graph: &Graph,
        limits: &Limits,
    ) -> Result<Vec<crate::corpus_proof::Contribution>, Error> {
        let mut result = Vec::with_capacity(graph.nodes().len() + graph.edges().len());
        for entity in graph.nodes().values() {
            let unchanged_native = self.adjacency
                && changes.is_some_and(|changes| !changes.nodes().contains_key(&entity.id));
            let bytes = if unchanged_native {
                let (lower, upper) = entity_range(1, entity.id)?;
                let selected = self.rows.range(lower..=upper).collect::<Vec<_>>();
                if decode_node(
                    entity.id,
                    &selected
                        .iter()
                        .map(|(key, value)| (**key, (*value).clone()))
                        .collect(),
                    limits,
                )?
                .as_ref()
                    != Some(entity)
                {
                    return Err(fail("unchanged proof node differs from native source"));
                }
                selected.iter().try_fold(0usize, |total, (_, data)| {
                    total
                        .checked_add(data.len())
                        .ok_or_else(|| fail("corpus proof node byte overflow"))
                })?
            } else {
                encode_record(1, entity.id, node(graph, entity.id))?
                    .values()
                    .try_fold(0usize, |total, data| {
                        total
                            .checked_add(data.len())
                            .ok_or_else(|| fail("corpus proof node byte overflow"))
                    })?
            };
            result.push(crate::corpus_proof::Contribution::node(entity, bytes)?);
        }
        for edge in graph.edges().values() {
            result.push(crate::corpus_proof::Contribution::edge(edge, limits)?);
        }
        Ok(result)
    }
    pub fn set_proof_rows(
        &mut self,
        rows: BTreeMap<u64, Vec<u8>>,
        limits: &Limits,
    ) -> Result<(), Error> {
        let mut bytes = 0usize;
        for (ordinal, data) in &rows {
            crate::corpus_proof::RecordKey::from_native_ordinal(*ordinal)?;
            bytes = bytes
                .checked_add(data.len())
                .ok_or_else(|| fail("corpus proof physical byte overflow"))?;
        }
        if bytes > limits.max_text_bytes {
            return Err(fail("corpus proof physical byte budget exceeded"));
        }
        self.proof_rows = rows;
        Ok(())
    }
    pub fn proof_rows(&self) -> &BTreeMap<u64, Vec<u8>> {
        &self.proof_rows
    }
    /// Retain only validated physical node records and scalar size metadata.
    /// Decoded native edges move directly into the graph, without constructing
    /// checksum/chunk rows or a second legacy topology. The SQL caller validates
    /// frozen physical routes and the original adjacency source at its snapshot.
    pub fn from_adjacency(
        mut node_rows: BTreeMap<u64, Vec<u8>>,
        edges: impl IntoIterator<Item = Edge>,
        logical_header: Vec<u8>,
        limits: &Limits,
    ) -> Result<(Graph, Self), Error> {
        if node_rows.keys().any(|key| key >> KIND_SHIFT != 1) {
            return Err(fail("adjacency projection requires only node records"));
        }
        let mut graph = snapshot_cache(Some(&logical_header), limits)?
            .ok_or_else(|| fail("adjacency projection requires a logical manifest"))?;
        let meta: Json =
            serde_json::from_slice(&logical_header).map_err(|_| fail("invalid graph manifest"))?;
        let expected_nodes = integer(&meta, "nodes")?;
        let expected_edges = integer(&meta, "edges")?;
        let mut stored_bytes = logical_header.len();
        for data in node_rows.values() {
            stored_bytes = stored_bytes.saturating_add(data.len());
            if stored_bytes > limits.max_text_bytes {
                return Err(fail("graph storage byte budget exceeded"));
            }
        }
        let mut iter = node_rows.iter().peekable();
        while let Some((&key, data)) = iter.next() {
            let (kind, id) = identity(key)?;
            if kind != 1 || key & CHUNK_MASK != 0 {
                return Err(fail("missing graph entity checksum"));
            }
            let mut selected = BTreeMap::from([(key, data.clone())]);
            while iter
                .peek()
                .is_some_and(|(next, _)| **next & !CHUNK_MASK == key)
            {
                let (&next, data) = iter.next().expect("peeked node chunk");
                selected.insert(next, data.clone());
            }
            let node = decode_node(id, &selected, limits)?
                .ok_or_else(|| fail("missing adjacency node"))?;
            graph.cache_node(node)?;
            if graph.nodes().len() as u64 > expected_nodes {
                return Err(fail("graph manifest counts differ from records"));
            }
        }
        let mut edge_sizes = BTreeMap::new();
        for edge in edges {
            if graph.edges().contains_key(&edge.id) {
                return Err(fail("duplicate adjacency edge identity"));
            }
            let size = native_edge_size(&edge, limits)?;
            stored_bytes = stored_bytes.saturating_add(size);
            if stored_bytes > limits.max_text_bytes {
                return Err(fail("graph storage byte budget exceeded"));
            }
            edge_sizes.insert(edge.id, size);
            graph.cache_edge(edge)?;
            if graph.edges().len() as u64 > expected_edges {
                return Err(fail("graph manifest counts differ from records"));
            }
        }
        if graph.nodes().len() as u64 != expected_nodes
            || graph.edges().len() as u64 != expected_edges
        {
            return Err(fail("graph manifest counts differ from records"));
        }
        node_rows.insert(0, logical_header);
        Ok((
            graph,
            Self {
                rows: node_rows,
                legacy: false,
                adjacency: true,
                stored_bytes,
                edge_sizes,
                corpus: None,
                proof_rows: BTreeMap::new(),
            },
        ))
    }
    /// Validated selected rows plus complete source totals. This object is a
    /// delta-planning image only; `rows()` is not a complete persistent graph.
    /// The SQL adapter must establish the source authority before calling it.
    pub fn partial_adjacency(
        mut rows: BTreeMap<u64, Vec<u8>>,
        edges: impl IntoIterator<Item = Edge>,
        corpus: GraphCorpus,
        limits: &Limits,
    ) -> Result<Self, Error> {
        corpus.validate(limits)?;
        if rows.keys().any(|key| key >> KIND_SHIFT != 1) {
            return Err(fail("partial native image requires node rows only"));
        }
        let ids: BTreeSet<_> = rows
            .keys()
            .map(|key| (key & ((1 << KIND_SHIFT) - 1)) >> ID_SHIFT)
            .collect();
        if ids.len() as u64 > corpus.nodes {
            return Err(fail("partial nodes exceed global count"));
        }
        for id in ids {
            let (lo, hi) = entity_range(1, id)?;
            let selected = rows
                .range(lo..=hi)
                .map(|(key, data)| (*key, data.clone()))
                .collect();
            decode_node(id, &selected, limits)?
                .ok_or_else(|| fail("missing partial native node"))?;
            if id >= corpus.next_id {
                return Err(fail("partial node exceeds source allocator"));
            }
        }
        let mut edge_sizes = BTreeMap::new();
        for edge in edges {
            if edge.id >= corpus.next_id
                || edge.source >= corpus.next_id
                || edge.target >= corpus.next_id
                || edge_sizes
                    .insert(edge.id, native_edge_size(&edge, limits)?)
                    .is_some()
            {
                return Err(fail("invalid partial native edge"));
            }
        }
        if edge_sizes.len() as u64 > corpus.edges {
            return Err(fail("partial edges exceed global count"));
        }
        rows.insert(
            0,
            manifest_values(corpus.next_id, corpus.nodes, corpus.edges)?,
        );
        Ok(Self {
            rows,
            legacy: false,
            adjacency: true,
            stored_bytes: usize::try_from(corpus.record_bytes)
                .map_err(|_| fail("global corpus byte overflow"))?,
            edge_sizes,
            corpus: Some(corpus),
            proof_rows: BTreeMap::new(),
        })
    }
    pub fn is_partial(&self) -> bool {
        self.corpus.is_some()
    }
    /// An empty native planning image for preflight/import, not a new authority.
    pub fn empty_adjacency(limits: &Limits) -> Result<(Graph, Self), Error> {
        Self::from_adjacency(
            BTreeMap::new(),
            std::iter::empty(),
            manifest(&Graph::new())?,
            limits,
        )
    }
    /// This is a node-record/size planning image of native adjacency authority.
    pub fn uses_adjacency(&self) -> bool {
        self.adjacency
    }
    pub fn rows(&self) -> &BTreeMap<u64, Vec<u8>> {
        &self.rows
    }
    pub fn is_legacy(&self) -> bool {
        self.legacy
    }
    /// Complete corpus size for a native manifest. Legacy migration validates
    /// the target representation once; an existing native image retains the
    /// source's already validated scalar size, including noncanonical node JSON.
    pub fn native_record_bytes(&self, graph: &Graph, limits: &Limits) -> Result<usize, Error> {
        if self.adjacency {
            return Ok(self.stored_bytes);
        }
        let mut bytes = manifest(graph)?.len();
        for id in graph.nodes().keys() {
            for record in encode_record(1, *id, node(graph, *id))?.values() {
                bytes = bytes
                    .checked_add(record.len())
                    .ok_or_else(|| fail("graph corpus byte overflow"))?;
            }
        }
        for edge in graph.edges().values() {
            bytes = bytes
                .checked_add(native_edge_size(edge, limits)?)
                .ok_or_else(|| fail("graph corpus byte overflow"))?;
        }
        if bytes > limits.max_text_bytes {
            return Err(fail("graph storage byte budget exceeded"));
        }
        Ok(bytes)
    }
    /// Stage only node records for an explicit adjacency migration. The caller
    /// supplies the graph validated from this image, fills edges in native
    /// routes, and replaces the retained legacy manifest in the same transaction.
    /// No intermediate record-only image is a committed graph generation.
    pub fn adjacency_migration_patch(
        &self,
        graph: &Graph,
        limits: &Limits,
    ) -> Result<StoragePatch, Error> {
        if self.adjacency {
            return Err(fail("adjacency migration requires a legacy record image"));
        }
        if graph.nodes().len() > limits.max_nodes || graph.edges().len() > limits.max_edges {
            return Err(fail("graph element budget exceeded"));
        }
        let mut desired = BTreeMap::new();
        if let Some(header) = self.rows.get(&0) {
            desired.insert(0, header.clone());
        }
        let mut bytes = desired.values().map(Vec::len).sum::<usize>();
        for id in graph.nodes().keys() {
            for (key, data) in encode_record(1, *id, node(graph, *id))? {
                bytes = bytes.saturating_add(data.len());
                if bytes > limits.max_text_bytes {
                    return Err(fail("graph migration node byte budget exceeded"));
                }
                desired.insert(key, data);
            }
        }
        if bytes > limits.max_text_bytes {
            return Err(fail("graph migration node byte budget exceeded"));
        }
        let mut patch = StoragePatch {
            migrated: self.legacy,
            ..StoragePatch::default()
        };
        let keep: BTreeSet<_> = desired.keys().copied().collect();
        for (key, data) in desired {
            match self.rows.get(&key) {
                Some(old) if old == &data => {}
                Some(_) => {
                    patch.updated.insert(key, data);
                }
                None => {
                    patch.inserted.insert(key, data);
                }
            }
        }
        patch.removed = self
            .rows
            .keys()
            .filter(|key| !keep.contains(key))
            .copied()
            .collect();
        Ok(patch)
    }
    /// Validate every row, checksum, endpoint and manifest count before exposing
    /// the graph. Old snapshots remain readable and are never changed on read.
    pub fn decode(rows: BTreeMap<u64, Vec<u8>>, limits: &Limits) -> Result<(Graph, Self), Error> {
        let mut stored_bytes = 0usize;
        for data in rows.values() {
            stored_bytes = stored_bytes.saturating_add(data.len());
            if stored_bytes > limits.max_text_bytes {
                return Err(fail("graph storage byte budget exceeded"));
            }
        }
        if rows.is_empty() {
            return Ok((
                Graph::new(),
                Self {
                    rows,
                    legacy: false,
                    adjacency: false,
                    stored_bytes,
                    edge_sizes: BTreeMap::new(),
                    corpus: None,
                    proof_rows: BTreeMap::new(),
                },
            ));
        }
        let header = rows.get(&0).ok_or_else(|| fail("missing graph manifest"))?;
        let graph = if header.len() == 32 {
            let mut bytes = Vec::new();
            for (ordinal, (key, data)) in rows.iter().enumerate().skip(1) {
                if *key != ordinal as u64 || data.is_empty() || data.len() > CHUNK {
                    return Err(fail("invalid legacy graph chunks"));
                }
                if bytes.len().saturating_add(data.len()) > limits.max_text_bytes {
                    return Err(fail("graph storage byte budget exceeded"));
                }
                bytes.extend_from_slice(data);
            }
            if &digest(&bytes) != header {
                return Err(fail("graph storage checksum mismatch"));
            }
            Graph::from_bytes(&bytes, limits)?
        } else {
            if header.len() > CHUNK {
                return Err(fail("oversized graph manifest"));
            }
            let meta: Json =
                serde_json::from_slice(header).map_err(|_| fail("invalid graph manifest"))?;
            if meta["format"] != FORMAT {
                return Err(fail("unsupported graph storage format"));
            }
            let nodes = integer(&meta, "nodes")?;
            let edges = integer(&meta, "edges")?;
            if nodes > limits.max_nodes as u64 || edges > limits.max_edges as u64 {
                return Err(fail("graph element budget exceeded"));
            }
            let mut graph = Graph::new();
            let mut iter = rows.range(1..).peekable();
            let mut total = header.len();
            while let Some((&key, checksum)) = iter.next() {
                let (kind, id) = identity(key)?;
                if key & CHUNK_MASK != 0 || checksum.len() != 32 {
                    return Err(fail("missing graph entity checksum"));
                }
                let mut bytes = Vec::new();
                let mut ordinal = 1u64;
                while iter.peek().is_some_and(|(k, _)| **k & !CHUNK_MASK == key) {
                    let (&chunk, data) = iter.next().expect("peeked chunk");
                    if chunk != key + ordinal
                        || data.is_empty()
                        || data.len() > CHUNK
                        || (!bytes.is_empty() && bytes.len() % CHUNK != 0)
                    {
                        return Err(fail("noncontiguous graph entity chunks"));
                    }
                    total = total.saturating_add(data.len());
                    if total > limits.max_text_bytes {
                        return Err(fail("graph storage byte budget exceeded"));
                    }
                    bytes.extend_from_slice(data);
                    ordinal += 1;
                }
                if bytes.is_empty() || &digest(&bytes) != checksum {
                    return Err(fail("graph entity checksum mismatch"));
                }
                let record: Json = serde_json::from_slice(&bytes)
                    .map_err(|_| fail("invalid graph entity JSON"))?;
                if integer(&record, "id")? != id {
                    return Err(fail("graph record ID differs from key"));
                }
                graph.reserve_ids(id, id)?;
                if kind == 1 {
                    let labels: BTreeSet<String> = record["labels"]
                        .as_array()
                        .ok_or_else(|| fail("invalid graph record labels"))?
                        .iter()
                        .map(|v| {
                            v.as_str()
                                .map(str::to_owned)
                                .ok_or_else(|| fail("invalid graph label"))
                        })
                        .collect::<Result<_, _>>()?;
                    graph.add_node(labels, props(&record)?)?;
                    if graph.nodes().len() > limits.max_nodes {
                        return Err(fail("graph node budget exceeded"));
                    }
                } else {
                    let label = record["label"]
                        .as_str()
                        .ok_or_else(|| fail("invalid relationship type"))?;
                    graph.add_edge(
                        integer(&record, "source")?,
                        integer(&record, "target")?,
                        label.into(),
                        props(&record)?,
                    )?;
                    if graph.edges().len() > limits.max_edges {
                        return Err(fail("graph edge budget exceeded"));
                    }
                }
            }
            if graph.nodes().len() as u64 != nodes || graph.edges().len() as u64 != edges {
                return Err(fail("graph manifest counts differ from records"));
            }
            graph.finish_load(integer(&meta, "next_id")?)?;
            graph
        };
        let legacy = header.len() == 32;
        Ok((
            graph,
            Self {
                rows,
                legacy,
                adjacency: false,
                stored_bytes,
                edge_sizes: BTreeMap::new(),
                corpus: None,
                proof_rows: BTreeMap::new(),
            },
        ))
    }
    /// Serialize only new/changed entities. Unchanged payloads and native row
    /// identities remain untouched, including the tail of a large record.
    pub fn patch(
        &self,
        before: &Graph,
        after: &Graph,
        limits: &Limits,
    ) -> Result<StoragePatch, Error> {
        if self.is_partial() {
            return Err(fail("partial native image requires a change-set patch"));
        }
        let mut ids = Vec::new();
        for kind in [1, 2] {
            let candidates: BTreeSet<u64> = if kind == 1 {
                before
                    .nodes()
                    .keys()
                    .chain(after.nodes().keys())
                    .copied()
                    .collect()
            } else {
                before
                    .edges()
                    .keys()
                    .chain(after.edges().keys())
                    .copied()
                    .collect()
            };
            for id in candidates {
                let unchanged = if kind == 1 {
                    before.nodes().get(&id) == after.nodes().get(&id)
                } else {
                    before.edges().get(&id) == after.edges().get(&id)
                };
                if self.legacy || !unchanged {
                    ids.push((kind, id));
                }
            }
        }
        self.patch_entities(after, ids, limits)
    }
    /// Plan only net changes from execution against this validated image.
    /// Legacy v1 needs a full format conversion and must use `patch` instead.
    /// The delta must belong to the query that produced `after` from this image.
    pub fn patch_changes(
        &self,
        changes: &GraphChanges,
        after: &Graph,
        limits: &Limits,
    ) -> Result<StoragePatch, Error> {
        if self.legacy {
            return Err(fail("legacy graph requires complete migration planning"));
        }
        let final_corpus = self
            .corpus
            .map(|source| source.changed(changes, after, limits))
            .transpose()?;
        if self.corpus.is_some() {
            for (id, old) in changes.nodes() {
                let (lo, hi) = entity_range(1, *id)?;
                let selected = self
                    .rows
                    .range(lo..=hi)
                    .map(|(key, data)| (*key, data.clone()))
                    .collect();
                if decode_node(*id, &selected, limits)?.as_ref() != old.as_ref() {
                    return Err(fail("partial node original differs from source"));
                }
            }
            for (id, old) in changes.edges() {
                if old.is_some() != self.edge_sizes.contains_key(id) {
                    return Err(fail("missing partial edge original"));
                }
            }
        }
        let ids = changes
            .nodes()
            .keys()
            .map(|id| (1, *id))
            .chain(changes.edges().keys().map(|id| (2, *id)));
        let mut patch = self.patch_entities_counted(after, ids, limits, final_corpus)?;
        if let Some(mut corpus) = final_corpus {
            corpus.record_bytes = patch
                .after_record_bytes
                .ok_or_else(|| fail("missing partial final bytes"))?
                as u64;
            corpus.validate(limits)?;
            patch.after_corpus = Some(corpus);
        }
        Ok(patch)
    }
    fn patch_entities(
        &self,
        after: &Graph,
        ids: impl IntoIterator<Item = (u64, u64)>,
        limits: &Limits,
    ) -> Result<StoragePatch, Error> {
        self.patch_entities_counted(after, ids, limits, None)
    }
    fn patch_entities_counted(
        &self,
        after: &Graph,
        ids: impl IntoIterator<Item = (u64, u64)>,
        limits: &Limits,
        corpus: Option<GraphCorpus>,
    ) -> Result<StoragePatch, Error> {
        if after.nodes().len() > limits.max_nodes || after.edges().len() > limits.max_edges {
            return Err(fail("graph element budget exceeded"));
        }
        let mut patch = StoragePatch {
            migrated: self.legacy,
            ..StoragePatch::default()
        };
        let header = match corpus {
            Some(c) => manifest_values(c.next_id, c.nodes, c.edges)?,
            None => manifest(after)?,
        };
        let mut desired = BTreeMap::from([(0, header)]);
        let mut old_keys = BTreeSet::from([0]);
        if self.legacy {
            old_keys.extend(self.rows.keys().copied());
        }
        let mut old_edge_bytes = 0usize;
        let mut new_edge_bytes = 0usize;
        for (kind, id) in ids {
            patch.entities_changed += 1;
            if self.adjacency && kind == 2 {
                old_edge_bytes =
                    old_edge_bytes.saturating_add(self.edge_sizes.get(&id).copied().unwrap_or(0));
                if let Some(edge) = after.edges().get(&id) {
                    new_edge_bytes = new_edge_bytes.saturating_add(native_edge_size(edge, limits)?);
                }
                continue;
            }
            let key = base(kind, id);
            old_keys.extend(self.rows.range(key..=key + CHUNK_MASK).map(|(key, _)| *key));
            let exists = if kind == 1 {
                after.nodes().contains_key(&id)
            } else {
                after.edges().contains_key(&id)
            };
            if exists {
                desired.extend(encode_record(
                    kind,
                    id,
                    if kind == 1 {
                        node(after, id)
                    } else {
                        edge(after, id)
                    },
                )?);
            }
        }
        for key in old_keys {
            if self.rows.contains_key(&key) && !desired.contains_key(&key) {
                patch.removed.push(key);
            }
        }
        for (key, bytes) in desired {
            match self.rows.get(&key) {
                Some(old) if old == &bytes => {}
                Some(_) => {
                    patch.updated.insert(key, bytes);
                }
                None => {
                    patch.inserted.insert(key, bytes);
                }
            }
        }
        // Include checksums and manifest in the storage budget as well as JSON.
        let old_size = self.stored_bytes;
        let removed: usize = patch.removed.iter().map(|key| self.rows[key].len()).sum();
        let replaced: usize = patch.updated.keys().map(|key| self.rows[key].len()).sum();
        let added: usize = patch
            .updated
            .values()
            .chain(patch.inserted.values())
            .map(Vec::len)
            .sum();
        let new_size = removed
            .checked_add(replaced)
            .and_then(|n| n.checked_add(old_edge_bytes))
            .and_then(|n| old_size.checked_sub(n))
            .and_then(|n| n.checked_add(added))
            .and_then(|n| n.checked_add(new_edge_bytes))
            .ok_or_else(|| fail("graph corpus byte overflow/underflow"))?;
        if new_size > limits.max_text_bytes {
            return Err(fail("graph storage byte budget exceeded"));
        }
        if self.adjacency {
            patch.after_record_bytes = Some(new_size);
        }
        Ok(patch)
    }
}
