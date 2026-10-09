//! Canonical logical payloads and snapshot metadata for authoritative adjacency.
//! SQL must publish these with graph-owned routes in one ordinary transaction;
//! this codec does not migrate v2 graphs or open any external database.

use crate::model::{fail, validate_properties};
use crate::{Edge, Error, Limits};
use bicdb_common::sha256::Sha256;
use bicdb_storage::adjacency::{self as page, Edge as PhysicalEdge};
use bicdb_storage::rowid::RowId;
use serde_json::Value as Json;
use std::collections::BTreeMap;

const MAGIC: &[u8; 8] = b"BICADJ3\0";
const HEADER: usize = 57;
const CHUNK: usize = 4096;
const CHUNKS: usize = 1023;
const OVERFLOW_KIND: u64 = 3;
const ENTRY_KIND: u64 = 4;
const ENTRY_MAGIC: &[u8; 8] = b"BICENT3\0";
/// Maximum canonical document bytes kept inline, including full type name.
pub const INLINE_LIMIT: usize = 4096;

fn digest(bytes: &[u8]) -> [u8; 32] {
    let mut sha = Sha256::new();
    sha.update(bytes);
    sha.finalize()
}
fn content_digest(
    id: u64,
    logical_source: u64,
    logical_target: u64,
    source: RowId,
    destination: RowId,
    document: &[u8],
) -> [u8; 32] {
    let mut sha = Sha256::new();
    sha.update(b"bicdb-adjacency-content-v3");
    for identity in [id, logical_source, logical_target] {
        sha.update(&six(identity));
    }
    sha.update(&source.to_bytes());
    sha.update(&destination.to_bytes());
    sha.update(document);
    sha.finalize()
}
fn identity(id: u64) -> Result<(), Error> {
    if id == 0 || id >= 1 << 48 {
        Err(fail("invalid adjacency logical identity"))
    } else {
        Ok(())
    }
}
fn base(kind: u64, id: u64) -> Result<u64, Error> {
    identity(id)?;
    Ok((kind << 60) | (id << 10))
}
fn six(id: u64) -> [u8; 6] {
    id.to_le_bytes()[..6].try_into().expect("six bytes")
}
fn read_six(bytes: &[u8]) -> u64 {
    let mut raw = [0; 8];
    raw[..6].copy_from_slice(bytes);
    u64::from_le_bytes(raw)
}

/// One physical edge and its optional property/type-only overflow records.
/// Topology is authoritative in the adjacency page, never in overflow heap rows.
#[derive(Debug, Clone)]
pub struct EncodedEdge {
    /// Source owner for the adjacency page; resolved from this graph's node heap.
    pub source: RowId,
    /// Complete type-5 edge record, with transaction slot initially 0xFF.
    pub record: Vec<u8>,
    /// Kind-3 checksum/chunks, keyed by edge ID. Empty for an inline document.
    pub overflow: BTreeMap<u64, Vec<u8>>,
}
/// Select external encoding even for a small document if an existing stable
/// edge extent cannot grow. External descriptors have fixed length; the caller
/// does not need to relocate the edge directory ordinal to update properties.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Inline canonical documents through [`INLINE_LIMIT`], otherwise external.
    Automatic,
    /// Always external, including a small/empty property object.
    External,
}

/// Validate the same canonical document bounds used by physical encoding,
/// without resolving workspace-specific endpoint ROWIDs or allocating chunks.
/// Logical import preflight and native planning use this before publication.
pub fn payload_size(edge: &Edge, limits: &Limits) -> Result<usize, Error> {
    Ok(logical_document(edge, limits)?.len())
}
fn logical_document(edge: &Edge, limits: &Limits) -> Result<Vec<u8>, Error> {
    for id in [edge.id, edge.source, edge.target] {
        identity(id)?;
    }
    if edge.label.is_empty() {
        return Err(fail("invalid adjacency resolved endpoint/type"));
    }
    validate_properties(&edge.properties)?;
    let document =
        serde_json::to_vec(&serde_json::json!({"label":edge.label,"properties":edge.properties}))
            .map_err(|e| fail(e.to_string()))?;
    if document.len() > CHUNK * CHUNKS || document.len() > limits.max_text_bytes {
        return Err(fail("adjacency document byte/chunk budget exceeded"));
    }
    Ok(document)
}

/// Encode source/target logical IDs and a checked canonical type/property
/// document. A type over 255 UTF-8 bytes uses an empty physical label sentinel
/// and is retained in the document; it is never truncated.
pub fn encode(
    edge: &Edge,
    source: RowId,
    destination: RowId,
    placement: Placement,
    limits: &Limits,
) -> Result<EncodedEdge, Error> {
    for id in [edge.id, edge.source, edge.target] {
        identity(id)?;
    }
    if source.row_id() == 0 || destination.row_id() == 0 || edge.label.is_empty() {
        return Err(fail("invalid adjacency resolved endpoint/type"));
    }
    let document = logical_document(edge, limits)?;
    let external = placement == Placement::External || document.len() > INLINE_LIMIT;
    let hash = content_digest(
        edge.id,
        edge.source,
        edge.target,
        source,
        destination,
        &document,
    );
    let mut payload = Vec::with_capacity(HEADER + if external { 0 } else { document.len() });
    payload.extend(MAGIC);
    payload.push(u8::from(external));
    payload.extend(six(edge.source));
    payload.extend(six(edge.target));
    payload.extend((document.len() as u32).to_le_bytes());
    payload.extend(hash);
    let mut overflow = BTreeMap::new();
    if external {
        let lower = base(OVERFLOW_KIND, edge.id)?;
        overflow.insert(lower, hash.to_vec());
        for (i, chunk) in document.chunks(CHUNK).enumerate() {
            overflow.insert(lower + i as u64 + 1, chunk.to_vec());
        }
    } else {
        payload.extend(&document);
    }
    let label = if edge.label.len() <= 255 {
        edge.label.as_str()
    } else {
        ""
    };
    let record = PhysicalEdge {
        id: edge.id,
        destination,
        flags: 0,
        itl_slot: 0xFF,
        label,
        payload: &payload,
    }
    .encode()
    .map_err(|e| fail(e.to_string()))?;
    Ok(EncodedEdge {
        source,
        record,
        overflow,
    })
}

/// Read only the overflow range selected by a verified physical descriptor.
/// Inline records return None. Limits are checked before any heap fetch.
pub fn overflow_range(
    expected: u64,
    record: &[u8],
    limits: &Limits,
) -> Result<Option<(u64, u64)>, Error> {
    let descriptor = descriptor(expected, record, limits)?;
    Ok(if descriptor.external {
        let lower = base(OVERFLOW_KIND, expected)?;
        Some((lower, lower + descriptor.length.div_ceil(CHUNK) as u64))
    } else {
        None
    })
}
struct Descriptor<'a> {
    physical: PhysicalEdge<'a>,
    external: bool,
    source: u64,
    target: u64,
    length: usize,
    hash: [u8; 32],
}
fn descriptor<'a>(
    expected: u64,
    record: &'a [u8],
    limits: &Limits,
) -> Result<Descriptor<'a>, Error> {
    identity(expected)?;
    let physical = page::decode(record).map_err(|e| fail(e.to_string()))?;
    let b = physical.payload;
    if physical.id != expected
        || physical.flags != 0
        || b.len() < HEADER
        || &b[..8] != MAGIC
        || b[8] > 1
    {
        return Err(fail("invalid adjacency payload descriptor"));
    }
    let source = read_six(&b[9..15]);
    let target = read_six(&b[15..21]);
    identity(source)?;
    identity(target)?;
    let length = u32::from_le_bytes(b[21..25].try_into().expect("four bytes")) as usize;
    let external = b[8] == 1;
    if length == 0
        || length > CHUNK * CHUNKS
        || length > limits.max_text_bytes
        || (external && b.len() != HEADER)
        || (!external && (length > INLINE_LIMIT || b.len() != HEADER + length))
    {
        return Err(fail(
            "adjacency descriptor byte/chunk budget or length invalid",
        ));
    }
    Ok(Descriptor {
        physical,
        external,
        source,
        target,
        length,
        hash: b[25..57].try_into().expect("hash"),
    })
}
/// Decode one edge, verifying exact chunk identity/count/length and both
/// checksum copies. SQL must additionally resolve logical IDs to the physical
/// page source and destination under the same snapshot and graph scope.
pub fn decode(
    expected: u64,
    source: RowId,
    record: &[u8],
    overflow: &BTreeMap<u64, Vec<u8>>,
    limits: &Limits,
) -> Result<Edge, Error> {
    if source.row_id() == 0 {
        return Err(fail("invalid adjacency source owner"));
    }
    let d = descriptor(expected, record, limits)?;
    let document = if d.external {
        let lower = base(OVERFLOW_KIND, expected)?;
        let count = d.length.div_ceil(CHUNK);
        if overflow.len() != count + 1
            || overflow.get(&lower).map(Vec::as_slice) != Some(d.hash.as_slice())
        {
            return Err(fail("adjacency overflow checksum/count mismatch"));
        }
        let mut document = Vec::with_capacity(d.length);
        for ordinal in 1..=count {
            let chunk = overflow
                .get(&(lower + ordinal as u64))
                .ok_or_else(|| fail("missing adjacency overflow chunk"))?;
            let required = CHUNK.min(d.length - (ordinal - 1) * CHUNK);
            if chunk.len() != required {
                return Err(fail("adjacency overflow chunk length mismatch"));
            }
            document.extend(chunk);
        }
        document
    } else {
        if !overflow.is_empty() {
            return Err(fail("unexpected inline adjacency overflow rows"));
        }
        d.physical.payload[HEADER..].to_vec()
    };
    if content_digest(
        expected,
        d.source,
        d.target,
        source,
        d.physical.destination,
        &document,
    ) != d.hash
    {
        return Err(fail("adjacency document checksum mismatch"));
    }
    let json: Json =
        serde_json::from_slice(&document).map_err(|_| fail("invalid adjacency document JSON"))?;
    let object = json
        .as_object()
        .ok_or_else(|| fail("invalid adjacency document shape"))?;
    if object.len() != 2 || !object.contains_key("label") || !object.contains_key("properties") {
        return Err(fail("invalid adjacency document fields"));
    }
    let label = json["label"]
        .as_str()
        .filter(|l| !l.is_empty())
        .ok_or_else(|| fail("invalid adjacency document type"))?;
    if d.physical.label != if label.len() <= 255 { label } else { "" } {
        return Err(fail("adjacency physical/document type mismatch"));
    }
    let properties = json["properties"]
        .as_object()
        .ok_or_else(|| fail("invalid adjacency properties"))?
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    validate_properties(&properties)?;
    // Reject duplicate fields, alternative ordering/escaping and noncanonical
    // blobs; the authoritative writer always emits this exact representation.
    if serde_json::to_vec(&json).map_err(|e| fail(e.to_string()))? != document {
        return Err(fail("noncanonical adjacency document"));
    }
    Ok(Edge {
        id: expected,
        source: d.source,
        target: d.target,
        label: label.to_owned(),
        properties,
    })
}

/// Snapshot-bearing source metadata stored as a protected ordinary heap row.
/// Its B-tree points to the metadata row, never directly to a mutable head value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceEntry {
    /// Source vertex's stable graph-resolved heap ROWID.
    pub source: RowId,
    /// First and last source-owned type-5 pages.
    pub head: RowId,
    /// Last page for a future validated constant-time append port.
    pub tail: RowId,
    /// Largest identity ever linked; does not decrease on logical deletion.
    pub last_id: u64,
}
impl SourceEntry {
    /// Fixed 32-byte encoding, suitable for in-place metadata/undo updates.
    pub fn encode(self) -> Result<[u8; 32], Error> {
        self.validate()?;
        let mut out = [0; 32];
        out[..8].copy_from_slice(ENTRY_MAGIC);
        out[8..14].copy_from_slice(&self.source.to_bytes());
        out[14..20].copy_from_slice(&self.head.to_bytes());
        out[20..26].copy_from_slice(&self.tail.to_bytes());
        out[26..32].copy_from_slice(&six(self.last_id));
        Ok(out)
    }
    /// Strict metadata decoding; physical segment/chain ownership is checked by
    /// the graph access port before using these page addresses.
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() != 32 || &bytes[..8] != ENTRY_MAGIC {
            return Err(fail("invalid adjacency source entry"));
        }
        let entry = Self {
            source: RowId::from_bytes(bytes[8..14].try_into().expect("rowid")),
            head: RowId::from_bytes(bytes[14..20].try_into().expect("rowid")),
            tail: RowId::from_bytes(bytes[20..26].try_into().expect("rowid")),
            last_id: read_six(&bytes[26..32]),
        };
        entry.validate()?;
        Ok(entry)
    }
    fn validate(self) -> Result<(), Error> {
        identity(self.last_id)?;
        if self.source.row_id() == 0
            || self.head.as_raw() == 0
            || self.tail.as_raw() == 0
            || self.head.row_id() != 0
            || self.tail.row_id() != 0
            || self.head.file_id() != self.tail.file_id()
        {
            return Err(fail("invalid adjacency source entry addresses"));
        }
        Ok(())
    }
}
/// Ordinal for the snapshot-bearing source entry, separate from node chunks.
pub fn source_ordinal(id: u64) -> Result<u64, Error> {
    base(ENTRY_KIND, id)
}
/// Ordered source-entry index key; index payload is its snapshot metadata ROWID.
pub fn source_key(source: RowId) -> Result<Vec<u8>, Error> {
    if source.row_id() == 0 {
        return Err(fail("invalid adjacency source key"));
    }
    Ok(source.as_raw().to_be_bytes()[2..].to_vec())
}
/// Ordered edge-location key; index payload is an actual stable edge ROWID.
pub fn locator_key(id: u64) -> Result<Vec<u8>, Error> {
    identity(id)?;
    Ok(id.to_be_bytes()[2..].to_vec())
}
/// Destination prefix for the protected reverse tree, optionally narrowed by type.
pub fn incoming_prefix(destination: RowId, label: Option<&str>) -> Result<Vec<u8>, Error> {
    let mut out = source_key(destination)?;
    if let Some(label) = label {
        // Keep existing long types addressable even when a covering suffix
        // leaves fewer bytes than the old v2 type index. Hashed candidates are
        // rechecked against the authoritative document; collisions cannot
        // decide type equality. The tag separates raw text from hash bytes.
        if label
            .len()
            .saturating_add(label.bytes().filter(|b| *b == 0).count())
            <= 4067
        {
            out.push(0);
            escaped(&mut out, label);
        } else {
            out.push(1);
            out.extend(digest(label.as_bytes()));
        }
    }
    Ok(out)
}
fn escaped(out: &mut Vec<u8>, text: &str) {
    for byte in text.bytes() {
        if byte == 0 {
            out.extend([0, 255]);
        } else {
            out.push(byte);
        }
    }
    out.extend([0, 0]);
}
/// Cover source/edge identity and a type selector, preserving embedded NUL and
/// long labels. Long type selectors are hash candidates and require rechecking.
/// The payload is the edge's actual ROWID, rechecked under the statement view.
pub fn incoming_key(
    destination: RowId,
    label: &str,
    source: RowId,
    id: u64,
) -> Result<Vec<u8>, Error> {
    if label.is_empty() {
        return Err(fail("empty adjacency reverse type"));
    }
    let mut out = incoming_prefix(destination, Some(label))?;
    out.extend(source_key(source)?);
    out.extend(locator_key(id)?);
    if out.len() > 4088 {
        return Err(fail("adjacency reverse key byte budget exceeded"));
    }
    Ok(out)
}
