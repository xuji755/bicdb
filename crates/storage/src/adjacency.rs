//! Type-5 authority pages for a single source vertex's ordered relationships.
//!
//! A directory ordinal is stable for the lifetime of a page: entries append in
//! increasing 48-bit identity order and deleted slots are never reused. The
//! sixteen-byte body prefix follows the dynamically sized common ITL header.
//! Compaction retains allocated record extents, including deleted records and
//! old update tails, so undo remains valid until an explicit reclaim horizon.

use crate::page::{Page, PageType, SlotEntry, SlotStatus, ADJACENCY_BODY_HEADER_LEN, MAX_SLOTS};
use crate::rowid::RowId;
use crate::undo::{RollbackError, UndoError, UndoOp, UndoPayload, UndoRecord};

/// Fixed edge header: identity, destination ROWID, flags, ITL, label/payload lengths.
pub const EDGE_HEADER_LEN: usize = 17;
/// Edge transaction-slot byte offset.
pub const EDGE_ITL_OFFSET: usize = 13;
/// Deleted edge flag; no other record flags are currently defined.
pub const DELETED: u8 = 1;
/// Next-page pointer offset relative to the body prefix, preserved across ITL growth.
pub const NEXT_PAGE_OFFSET: usize = 9;

/// Invalid adjacency format or rejected page operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdjacencyError {
    /// Page/body/directory/record bounds or metadata are malformed.
    Malformed,
    /// The record's identity, endpoint, flags or text is invalid.
    BadEdge,
    /// The page lacks space for a record or directory entry.
    PageFull,
    /// All 1023 stable directory ordinals are in use.
    SlotLimit,
    /// Append identity is not greater than every previously appended identity.
    IdentityOrder,
    /// The requested directory ordinal is missing or deleted.
    Missing,
    /// Update attempts to change a relationship's identity or destination.
    IdentityChanged,
}
impl std::fmt::Display for AdjacencyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Malformed => "malformed adjacency page",
            Self::BadEdge => "invalid adjacency edge",
            Self::PageFull => "adjacency page full",
            Self::SlotLimit => "adjacency directory limit exceeded",
            Self::IdentityOrder => "adjacency edge IDs must append in increasing order",
            Self::Missing => "adjacency edge missing",
            Self::IdentityChanged => "adjacency edge identity/endpoint cannot change",
        })
    }
}
impl std::error::Error for AdjacencyError {}

/// Borrowed canonical relationship record; property bytes are owned by its caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Edge<'a> {
    /// Nonzero, non-reused 48-bit identity.
    pub id: u64,
    /// Destination vertex's stable physical row address.
    pub destination: RowId,
    /// Only [`DELETED`] is currently permitted.
    pub flags: u8,
    /// Index into the common page ITL, or 0xFF for no transaction.
    pub itl_slot: u8,
    /// UTF-8 label, at most 255 bytes.
    pub label: &'a str,
    /// Inline property encoding, at most 65535 bytes and subject to page capacity.
    pub payload: &'a [u8],
}
impl Edge<'_> {
    /// Encode the fixed little-endian header and variable data.
    pub fn encode(self) -> Result<Vec<u8>, AdjacencyError> {
        if self.id == 0
            || self.id >= 1 << 48
            || self.destination.row_id() == 0
            || self.flags & !DELETED != 0
            || self.label.len() > 255
            || self.payload.len() > 65535
        {
            return Err(AdjacencyError::BadEdge);
        }
        let mut bytes = Vec::with_capacity(EDGE_HEADER_LEN + self.label.len() + self.payload.len());
        bytes.extend_from_slice(&self.id.to_le_bytes()[..6]);
        bytes.extend_from_slice(&self.destination.to_bytes());
        bytes.extend_from_slice(&[self.flags, self.itl_slot, self.label.len() as u8]);
        bytes.extend_from_slice(&(self.payload.len() as u16).to_le_bytes());
        bytes.extend_from_slice(self.label.as_bytes());
        bytes.extend_from_slice(self.payload);
        Ok(bytes)
    }
}
/// Decode one complete canonical edge record.
pub fn decode(bytes: &[u8]) -> Result<Edge<'_>, AdjacencyError> {
    if bytes.len() < EDGE_HEADER_LEN {
        return Err(AdjacencyError::BadEdge);
    }
    let mut id = [0; 8];
    id[..6].copy_from_slice(&bytes[..6]);
    let id = u64::from_le_bytes(id);
    let destination = RowId::from_bytes(
        bytes[6..12]
            .try_into()
            .map_err(|_| AdjacencyError::BadEdge)?,
    );
    let label_len = usize::from(bytes[14]);
    let payload_len = usize::from(u16::from_le_bytes([bytes[15], bytes[16]]));
    if EDGE_HEADER_LEN + label_len + payload_len != bytes.len()
        || id == 0
        || destination.row_id() == 0
        || bytes[12] & !DELETED != 0
    {
        return Err(AdjacencyError::BadEdge);
    }
    let label = std::str::from_utf8(&bytes[EDGE_HEADER_LEN..EDGE_HEADER_LEN + label_len])
        .map_err(|_| AdjacencyError::BadEdge)?;
    Ok(Edge {
        id,
        destination,
        flags: bytes[12],
        itl_slot: bytes[13],
        label,
        payload: &bytes[EDGE_HEADER_LEN + label_len..],
    })
}

/// Source vertex, number of live entries, and optional next adjacency page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// Every entry in this page belongs to this source vertex.
    pub source: RowId,
    /// Deleted slots remain in the directory but do not contribute to this count.
    pub live_edges: u16,
    /// A page-address ROWID (row number zero), or no successor.
    pub next: Option<RowId>,
}
/// Read and validate the type-specific prefix.
pub fn header(page: &Page) -> Result<Header, AdjacencyError> {
    let common = page.header().ok_or(AdjacencyError::Malformed)?;
    let at = page.fixed_header_end();
    if common.page_type != PageType::Adjacency
        || !page.is_initialized()
        || at + ADJACENCY_BODY_HEADER_LEN > page.row_area_floor()
        || page.slot_count() as usize > MAX_SLOTS
        || page.free_start() > page.free_end()
        || page.free_end() > page.row_area_floor()
    {
        return Err(AdjacencyError::Malformed);
    }
    let b = &page.as_bytes()[at..at + ADJACENCY_BODY_HEADER_LEN];
    let source = RowId::from_bytes(b[..6].try_into().map_err(|_| AdjacencyError::Malformed)?);
    let live_edges = u16::from_le_bytes([b[6], b[7]]);
    let next = RowId::from_bytes(b[9..15].try_into().map_err(|_| AdjacencyError::Malformed)?);
    if source.row_id() == 0
        || b[8] != 0
        || b[15] != 0
        || live_edges > page.slot_count()
        || (next.as_raw() != 0
            && (next.row_id() != 0
                || next.file_id() != common.file_id
                || next.block_id() == common.block_id))
    {
        return Err(AdjacencyError::Malformed);
    }
    Ok(Header {
        source,
        live_edges,
        next: (next.as_raw() != 0).then_some(next),
    })
}
/// Initialize an empty, already physically formatted type-5 page for a source.
pub fn initialize(page: &mut Page, source: RowId) -> Result<(), AdjacencyError> {
    if page.header().map(|h| h.page_type) != Some(PageType::Adjacency)
        || page.slot_count() != 0
        || source.row_id() == 0
    {
        return Err(AdjacencyError::Malformed);
    }
    let at = page.fixed_header_end();
    let body = page
        .as_bytes_mut()
        .get_mut(at..at + ADJACENCY_BODY_HEADER_LEN)
        .ok_or(AdjacencyError::Malformed)?;
    body.fill(0);
    body[..6].copy_from_slice(&source.to_bytes());
    page.seal();
    header(page)?;
    Ok(())
}
/// Set a same-file, non-self next page; the transactional caller records its undo.
pub fn set_next(page: &mut Page, next: Option<RowId>) -> Result<(), AdjacencyError> {
    header(page)?;
    let common = page.header().ok_or(AdjacencyError::Malformed)?;
    if next.is_some_and(|p| {
        p.as_raw() == 0
            || p.row_id() != 0
            || p.file_id() != common.file_id
            || p.block_id() == common.block_id
    }) {
        return Err(AdjacencyError::Malformed);
    }
    let at = page.fixed_header_end() + NEXT_PAGE_OFFSET;
    page.as_bytes_mut()[at..at + 6].copy_from_slice(&next.map_or([0; 6], RowId::to_bytes));
    Ok(())
}
fn set_count(page: &mut Page, count: u16) {
    let at = page.fixed_header_end() + 6;
    page.as_bytes_mut()[at..at + 2].copy_from_slice(&count.to_le_bytes());
}
fn slot(page: &Page, ordinal: u16) -> Result<SlotEntry, AdjacencyError> {
    let index = ordinal.checked_sub(1).ok_or(AdjacencyError::Missing)? as usize;
    page.slot(index).ok_or(AdjacencyError::Missing)
}
// All allocated extents are retained, even when an update grew into a new
// physical location. This conservative allocation boundary protects old undo.
fn extent(page: &Page, ordinal: u16) -> Result<(usize, usize), AdjacencyError> {
    let start = usize::from(slot(page, ordinal)?.offset());
    if start < page.free_end() || start < page.free_start() || start >= page.row_area_floor() {
        return Err(AdjacencyError::Malformed);
    }
    let mut end = page.row_area_floor();
    for index in 0..usize::from(page.slot_count()) {
        let offset = usize::from(page.slot(index).ok_or(AdjacencyError::Malformed)?.offset());
        if offset > start {
            end = end.min(offset);
        }
        if index + 1 != usize::from(ordinal) && offset == start {
            return Err(AdjacencyError::Malformed);
        }
    }
    Ok((start, end))
}
fn raw_record(page: &Page, ordinal: u16) -> Result<&[u8], AdjacencyError> {
    let (start, end) = extent(page, ordinal)?;
    if end - start < EDGE_HEADER_LEN {
        return Err(AdjacencyError::Malformed);
    }
    let b = &page.as_bytes()[start..end];
    let len =
        EDGE_HEADER_LEN + usize::from(b[14]) + usize::from(u16::from_le_bytes([b[15], b[16]]));
    let record = b.get(..len).ok_or(AdjacencyError::Malformed)?;
    decode(record)?;
    Ok(record)
}
/// Read a live edge by stable directory ordinal.
pub fn record(page: &Page, ordinal: u16) -> Result<&[u8], AdjacencyError> {
    header(page)?;
    if slot(page, ordinal)?.status() != SlotStatus::Normal {
        return Err(AdjacencyError::Missing);
    }
    let bytes = raw_record(page, ordinal)?;
    if decode(bytes)?.flags & DELETED != 0 {
        return Err(AdjacencyError::Malformed);
    }
    Ok(bytes)
}
/// Binary-search the ordered directory, including tombstones in the key order.
pub fn find(page: &Page, id: u64) -> Result<Option<u16>, AdjacencyError> {
    header(page)?;
    let mut low = 1;
    let mut high = page.slot_count() + 1;
    while low < high {
        let middle = low + (high - low) / 2;
        let key = decode(raw_record(page, middle)?)?.id;
        if key < id {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    if low <= page.slot_count()
        && decode(raw_record(page, low)?)?.id == id
        && slot(page, low)?.status() == SlotStatus::Normal
    {
        Ok(Some(low))
    } else {
        Ok(None)
    }
}
/// Greatest identity ever appended, including rolled-back/deleted entries.
pub fn last_identity(page: &Page) -> Result<Option<u64>, AdjacencyError> {
    header(page)?;
    if page.slot_count() == 0 {
        Ok(None)
    } else {
        Ok(Some(decode(raw_record(page, page.slot_count())?)?.id))
    }
}
/// Append a new edge without reusing any directory ordinal or identity.
pub fn append(page: &mut Page, bytes: &[u8]) -> Result<u16, AdjacencyError> {
    let h = header(page)?;
    let edge = decode(bytes)?;
    if edge.flags != 0
        || (edge.itl_slot != 0xFF
            && u16::from(edge.itl_slot)
                >= page.header().ok_or(AdjacencyError::Malformed)?.itl_count)
    {
        return Err(AdjacencyError::BadEdge);
    }
    let slots = page.slot_count();
    if slots as usize >= MAX_SLOTS {
        return Err(AdjacencyError::SlotLimit);
    }
    if slots != 0 && edge.id <= decode(raw_record(page, slots)?)?.id {
        return Err(AdjacencyError::IdentityOrder);
    }
    if page.free_space() < bytes.len() + 2 {
        return Err(AdjacencyError::PageFull);
    }
    let at = page.free_end() - bytes.len();
    let entry = SlotEntry::new(at as u16, SlotStatus::Normal).ok_or(AdjacencyError::Malformed)?;
    page.as_bytes_mut()[at..at + bytes.len()].copy_from_slice(bytes);
    page.set_slot_count(slots + 1)
        .map_err(|_| AdjacencyError::SlotLimit)?;
    page.set_slot(slots as usize, entry);
    page.set_free_end(at);
    set_count(page, h.live_edges + 1);
    Ok(slots + 1)
}
/// Mark an edge deleted while retaining its full allocated extent and stable slot.
pub fn delete(page: &mut Page, ordinal: u16) -> Result<(), AdjacencyError> {
    let h = header(page)?;
    record(page, ordinal)?;
    let entry = slot(page, ordinal)?;
    let at = usize::from(entry.offset());
    page.as_bytes_mut()[at + 12] |= DELETED;
    page.set_slot(
        ordinal as usize - 1,
        SlotEntry::new(entry.offset(), SlotStatus::Free).ok_or(AdjacencyError::Malformed)?,
    );
    set_count(page, h.live_edges - 1);
    Ok(())
}
/// Replace variable edge data, keeping identity/endpoint and the stable slot.
/// Shrinking retains undo space; growth uses fresh free space, never an old slot.
pub fn update(page: &mut Page, ordinal: u16, bytes: &[u8]) -> Result<(), AdjacencyError> {
    header(page)?;
    let old = decode(record(page, ordinal)?)?;
    let next = decode(bytes)?;
    if old.id != next.id || old.destination != next.destination {
        return Err(AdjacencyError::IdentityChanged);
    }
    if next.flags != 0
        || (next.itl_slot != 0xFF
            && u16::from(next.itl_slot)
                >= page.header().ok_or(AdjacencyError::Malformed)?.itl_count)
    {
        return Err(AdjacencyError::BadEdge);
    }
    let (start, end) = extent(page, ordinal)?;
    let at = if bytes.len() <= end - start {
        start
    } else {
        if bytes.len() > page.free_space() {
            return Err(AdjacencyError::PageFull);
        }
        page.free_end() - bytes.len()
    };
    page.as_bytes_mut()[at..at + bytes.len()].copy_from_slice(bytes);
    if at != start {
        page.set_slot(
            ordinal as usize - 1,
            SlotEntry::new(at as u16, SlotStatus::Normal).ok_or(AdjacencyError::Malformed)?,
        );
        page.set_free_end(at);
    }
    Ok(())
}
/// Validate metadata, ordered identities, extents, live count and transaction slots.
pub fn validate(page: &Page) -> Result<(), AdjacencyError> {
    let h = header(page)?;
    let mut previous = 0;
    let mut live = 0;
    for ordinal in 1..=page.slot_count() {
        let status = slot(page, ordinal)?.status();
        let edge = decode(raw_record(page, ordinal)?)?;
        if edge.id <= previous
            || !matches!(status, SlotStatus::Free | SlotStatus::Normal)
            || (status == SlotStatus::Free) != (edge.flags & DELETED != 0)
            || (edge.itl_slot != 0xFF
                && u16::from(edge.itl_slot)
                    >= page.header().ok_or(AdjacencyError::Malformed)?.itl_count)
        {
            return Err(AdjacencyError::Malformed);
        }
        previous = edge.id;
        if status == SlotStatus::Normal {
            live += 1;
        }
    }
    if live != h.live_edges {
        return Err(AdjacencyError::Malformed);
    }
    Ok(())
}
/// Pack allocated extents in directory order without discarding any undo bytes.
/// This is not horizon-based garbage collection and may reclaim zero bytes.
pub fn compact_retaining_undo(page: &mut Page) -> Result<usize, AdjacencyError> {
    validate(page)?;
    let before = page.free_space();
    let mut extents = Vec::with_capacity(page.slot_count() as usize);
    for ordinal in 1..=page.slot_count() {
        let (start, end) = extent(page, ordinal)?;
        extents.push((
            slot(page, ordinal)?.status(),
            page.as_bytes()[start..end].to_vec(),
        ));
    }
    let mut at = page.row_area_floor();
    for (index, (status, bytes)) in extents.into_iter().enumerate() {
        at = at
            .checked_sub(bytes.len())
            .ok_or(AdjacencyError::Malformed)?;
        if at < page.free_start() {
            return Err(AdjacencyError::Malformed);
        }
        page.as_bytes_mut()[at..at + bytes.len()].copy_from_slice(&bytes);
        page.set_slot(
            index,
            SlotEntry::new(at as u16, status).ok_or(AdjacencyError::Malformed)?,
        );
    }
    page.set_free_end(at);
    Ok(page.free_space().saturating_sub(before))
}

pub(crate) fn apply_undo(page: &mut Page, record: &UndoRecord) -> Result<(), RollbackError> {
    let mut staged = Page::from_bytes(Box::new(*page.as_bytes()));
    apply_undo_inner(&mut staged, record)?;
    header(&staged).map_err(|_| RollbackError::Undo(UndoError::MalformedRecord))?;
    *page = staged;
    Ok(())
}

fn apply_undo_inner(page: &mut Page, record: &UndoRecord) -> Result<(), RollbackError> {
    let bad = |_| RollbackError::Undo(UndoError::MalformedRecord);
    header(page).map_err(bad)?;
    let ordinal = record.rowid.row_id();
    if ordinal == 0 {
        // Existing Update wire format, with a page-address target, is used only
        // for this fixed metadata field. It cannot overwrite arbitrary headers.
        let UndoPayload::Update {
            old_itl_slot: 0xFF,
            patches,
        } = &record.payload
        else {
            return Err(RollbackError::Undo(UndoError::MalformedRecord));
        };
        if record.op != UndoOp::Update
            || patches.len() != 1
            || patches[0].0 as usize != NEXT_PAGE_OFFSET
            || patches[0].1.len() != 6
        {
            return Err(RollbackError::Undo(UndoError::MalformedRecord));
        }
        let pointer = RowId::from_bytes(
            patches[0]
                .1
                .as_slice()
                .try_into()
                .map_err(|_| RollbackError::Undo(UndoError::MalformedRecord))?,
        );
        return set_next(page, (pointer.as_raw() != 0).then_some(pointer)).map_err(bad);
    }
    let Ok(entry) = slot(page, ordinal) else {
        return Ok(());
    };
    match (&record.op, &record.payload) {
        (UndoOp::Insert, UndoPayload::None) => {
            if entry.status() == SlotStatus::Normal {
                delete(page, ordinal).map_err(bad)?;
            }
        }
        (UndoOp::Delete, UndoPayload::FullRow(bytes)) => {
            let old = decode(bytes).map_err(bad)?;
            if entry.status() == SlotStatus::Normal {
                if raw_record(page, ordinal).map_err(bad)? == bytes {
                    return Ok(());
                }
                return Err(RollbackError::SlotReused {
                    rowid: record.rowid,
                });
            }
            let retained = decode(raw_record(page, ordinal).map_err(bad)?).map_err(bad)?;
            if entry.status() != SlotStatus::Free
                || (retained.id, retained.destination) != (old.id, old.destination)
                || old.flags != 0
                || (old.itl_slot != 0xFF
                    && u16::from(old.itl_slot)
                        >= page
                            .header()
                            .ok_or(RollbackError::Undo(UndoError::MalformedRecord))?
                            .itl_count)
            {
                return Err(RollbackError::SlotReused {
                    rowid: record.rowid,
                });
            }
            let (start, end) = extent(page, ordinal).map_err(bad)?;
            if bytes.len() > end - start {
                return Err(RollbackError::RegionOccupied {
                    rowid: record.rowid,
                });
            }
            let count = header(page).map_err(bad)?.live_edges;
            page.as_bytes_mut()[start..start + bytes.len()].copy_from_slice(bytes);
            page.set_slot(
                ordinal as usize - 1,
                SlotEntry::new(start as u16, SlotStatus::Normal)
                    .ok_or(RollbackError::Undo(UndoError::MalformedRecord))?,
            );
            set_count(page, count + 1);
        }
        (
            UndoOp::Update,
            UndoPayload::Update {
                old_itl_slot,
                patches,
            },
        ) => {
            if entry.status() == SlotStatus::Free {
                return Ok(());
            }
            let original = decode(raw_record(page, ordinal).map_err(bad)?).map_err(bad)?;
            let original_key = (original.id, original.destination);
            let (start, end) = extent(page, ordinal).map_err(bad)?;
            for (offset, bytes) in patches {
                let at = start
                    .checked_add(*offset as usize)
                    .ok_or(RollbackError::Undo(UndoError::MalformedRecord))?;
                if at > end || bytes.len() > end - at {
                    return Err(RollbackError::Undo(UndoError::MalformedRecord));
                }
                page.as_bytes_mut()[at..at + bytes.len()].copy_from_slice(bytes);
            }
            page.as_bytes_mut()[start + EDGE_ITL_OFFSET] = *old_itl_slot;
            let restored = decode(raw_record(page, ordinal).map_err(bad)?).map_err(bad)?;
            if (restored.id, restored.destination) != original_key
                || restored.flags != 0
                || (restored.itl_slot != 0xFF
                    && u16::from(restored.itl_slot)
                        >= page
                            .header()
                            .ok_or(RollbackError::Undo(UndoError::MalformedRecord))?
                            .itl_count)
            {
                return Err(RollbackError::Undo(UndoError::MalformedRecord));
            }
        }
        _ => return Err(RollbackError::Undo(UndoError::MalformedRecord)),
    }
    Ok(())
}
