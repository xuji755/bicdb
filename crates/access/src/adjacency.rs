//! Source-clustered type-3 segment access. The caller maintains graph-owned
//! entry/reverse/property indexes in the same transaction and commits once.
//! Source/destination ROWIDs must be resolved within that caller's graph.

use crate::heap::next_append_logical;
use crate::TableAccessError;
use bicdb_storage::adjacency::{self as layout, AdjacencyError};
use bicdb_storage::buffer::{BufferKey, BufferPool};
use bicdb_storage::cr::{self, ReadView};
use bicdb_storage::datafile::DataFile;
use bicdb_storage::heap::InsertPolicy;
use bicdb_storage::page::{Page, PageType, ITL_ENTRY_LEN, MAX_SLOTS};
use bicdb_storage::rowid::{Rdba, RowId};
use bicdb_storage::segment::{SegType, Segment, SegmentSpaceError};
use bicdb_storage::undo::UndoChain;
use bicdb_txn::write::{self, adjacency as writes, Txn};
use bicdb_wal::group::GroupWriter;
use std::collections::BTreeSet;

/// Segment access, adjacency format, chain/scope, or bounded-read failure.
#[derive(Debug)]
pub enum AccessError {
    /// Native allocation/transaction error.
    Native(TableAccessError),
    /// Relationship format/capacity/order error.
    Layout(AdjacencyError),
    /// Snapshot undo could not be reconstructed.
    Snapshot(cr::CrError),
    /// Wrong segment, endpoint/page address, chain owner, order or cycle.
    Chain(&'static str),
    /// Explicit caller budget cannot cover the complete chain/result.
    Budget(&'static str),
}
impl std::fmt::Display for AccessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Native(e) => e.fmt(f),
            Self::Layout(e) => e.fmt(f),
            Self::Snapshot(e) => e.fmt(f),
            Self::Chain(e) | Self::Budget(e) => f.write_str(e),
        }
    }
}
impl std::error::Error for AccessError {}
impl From<TableAccessError> for AccessError {
    fn from(e: TableAccessError) -> Self {
        Self::Native(e)
    }
}
impl From<write::TxnError> for AccessError {
    fn from(e: write::TxnError) -> Self {
        Self::Native(TableAccessError::Txn(e))
    }
}
impl From<AdjacencyError> for AccessError {
    fn from(e: AdjacencyError) -> Self {
        Self::Layout(e)
    }
}

/// Per-call bounded chain traversal. These are trusted physical-port limits;
/// a graph SQL adapter must map its statement/workspace budgets onto them.
#[derive(Debug, Clone, Copy)]
pub struct ChainLimits {
    /// Maximum pages examined, including empty/tombstoned pages.
    pub pages: usize,
    /// Maximum live records retained by a complete read.
    pub edges: usize,
    /// Maximum complete-record bytes retained (not process RSS).
    pub bytes: usize,
}
impl Default for ChainLimits {
    fn default() -> Self {
        Self {
            pages: 100000,
            edges: 500000,
            bytes: 100 * 1024 * 1024,
        }
    }
}
impl ChainLimits {
    fn validate(self) -> Result<(), AccessError> {
        if self.pages == 0
            || self.pages > 100000
            || self.edges > 500000
            || self.bytes > 100 * 1024 * 1024
        {
            Err(AccessError::Budget("invalid adjacency chain limits"))
        } else {
            Ok(())
        }
    }
}
/// An appended edge and the head to publish in the caller's source-entry tree.
#[derive(Debug, Clone, Copy)]
pub struct AppendResult {
    /// Stable page-address ROWID of the source's first adjacency page.
    pub head: RowId,
    /// Stable directory ROWID for this edge.
    pub edge: RowId,
}
/// Physical segment port sharing the ordinary buffer pool, undo and WAL.
pub struct AdjacencyAccess<'a, 'io> {
    pool: &'a BufferPool<'io>,
    workspace: [u8; 8],
}
impl<'a, 'io> AdjacencyAccess<'a, 'io> {
    /// Bind to one workspace; addresses never select a different workspace.
    pub fn new(pool: &'a BufferPool<'io>, workspace: [u8; 8]) -> Self {
        Self { pool, workspace }
    }
    fn key(&self, address: RowId) -> Result<BufferKey, AccessError> {
        Ok(BufferKey::new(
            self.workspace,
            Rdba::from_parts(address.file_id(), address.block_id())
                .ok_or(AccessError::Chain("invalid adjacency page address"))?,
        ))
    }
    fn current(&self, address: RowId) -> Result<Page, AccessError> {
        let guard = self
            .pool
            .pin(self.key(address)?)
            .map_err(TableAccessError::Pool)?;
        Ok(Page::from_bytes(Box::new(*guard.as_bytes())))
    }
    fn address(&self, file: u16, block: u32) -> Result<RowId, AccessError> {
        RowId::page_address(file, block)
            .map_err(|_| AccessError::Chain("invalid adjacency page address"))
    }
    fn check_segment(&self, segment: &Segment<'_, '_>) -> Result<(), AccessError> {
        if segment.header().seg_type != SegType::Adjacency
            || segment.workspace_ref() != self.workspace
        {
            return Err(AccessError::Chain(
                "expected workspace-owned adjacency segment type 3",
            ));
        }
        Ok(())
    }
    fn check_page(
        &self,
        segment: &Segment<'_, '_>,
        address: RowId,
        source: RowId,
        page: &Page,
    ) -> Result<(), AccessError> {
        let common = page
            .header()
            .ok_or(AccessError::Chain("malformed adjacency page header"))?;
        if address.row_id() != 0
            || address.as_raw() == 0
            || address.file_id() != segment.file_id()
            || !segment
                .logical_of_block(address.block_id())
                .is_some_and(|logical| logical < segment.hwm())
            || common.file_id != address.file_id()
            || common.block_id != address.block_id()
            || common.workspace_ref != self.workspace
            || layout::header(page)?.source != source
        {
            return Err(AccessError::Chain(
                "adjacency page outside source/segment/workspace",
            ));
        }
        layout::validate(page)?;
        Ok(())
    }
    /// Allocate/format one source-owned page through ordinary segment and WAL APIs.
    #[allow(clippy::too_many_arguments)]
    pub fn allocate(
        &self,
        log: &mut GroupWriter<'_, '_>,
        txn: &Txn,
        file: &mut DataFile<'_>,
        segment_header: u32,
        source: RowId,
    ) -> Result<RowId, AccessError> {
        if source.row_id() == 0 {
            return Err(AccessError::Chain("source must be a vertex row"));
        }
        let fid = file.file_id();
        let mut segment = Segment::open_pooled(self.pool, file, segment_header, self.workspace)
            .map_err(TableAccessError::Segment)?;
        self.check_segment(&segment)?;
        let logical = next_append_logical(self.pool, log, &mut segment, txn, self.workspace)?;
        let block = segment
            .logical_block(logical)
            .ok_or(TableAccessError::Segment(SegmentSpaceError::BitmapCoverage))?;
        let (before, after) = {
            let mut current = |b| self.current(self.address(fid, b).ok()?).ok();
            segment
                .plan_advance_append(logical + 1, &mut current)
                .map_err(TableAccessError::Segment)?
        };
        write::write_page_change(
            self.pool,
            log,
            txn.raw(),
            self.key(self.address(fid, segment_header)?)?,
            before.as_bytes(),
            after.as_bytes(),
            false,
        )?;
        let address = self.address(fid, block)?;
        let mut fresh = Page::new(PageType::Adjacency, self.workspace, fid, block);
        layout::initialize(&mut fresh, source)?;
        let mut format = |p: &Page| -> Result<(), AccessError> {
            let mut copy = Page::from_bytes(Box::new(*p.as_bytes()));
            segment
                .write_physical_page(block, &mut copy)
                .map_err(TableAccessError::Segment)?;
            segment.sync().map_err(TableAccessError::Segment)?;
            Ok(())
        };
        write::fresh_page_with_redo(
            self.pool,
            log,
            txn.raw(),
            self.key(address)?,
            &fresh,
            &mut format,
        )?;
        Ok(address)
    }
    /// Append to a source chain, locking both resolved vertices in ROWID order.
    /// Caller must roll back its statement on failure and atomically publish the
    /// returned head plus reverse/property indexes; this method never commits.
    /// The source head must be looked up while holding the endpoint locks from
    /// `lock_endpoints`; a stale head obtained before locking is not safe.
    #[allow(clippy::too_many_arguments)]
    pub fn append(
        &self,
        log: &mut GroupWriter<'_, '_>,
        chain: &mut UndoChain<'_, '_>,
        txn: &mut Txn,
        file: &mut DataFile<'_>,
        segment_header: u32,
        source: RowId,
        head: Option<RowId>,
        bytes: &[u8],
        limits: ChainLimits,
    ) -> Result<AppendResult, AccessError> {
        self.append_with(
            log,
            chain,
            txn,
            file,
            segment_header,
            source,
            head,
            bytes,
            limits,
            |_, _| Ok(()),
        )
    }
    /// Append with a caller cancellation/work checkpoint on every examined or
    /// newly allocated page. The callback never commits or releases row locks.
    #[allow(clippy::too_many_arguments)]
    pub fn append_with(
        &self,
        log: &mut GroupWriter<'_, '_>,
        chain: &mut UndoChain<'_, '_>,
        txn: &mut Txn,
        file: &mut DataFile<'_>,
        segment_header: u32,
        source: RowId,
        head: Option<RowId>,
        bytes: &[u8],
        limits: ChainLimits,
        mut visit: impl FnMut(RowId, usize) -> Result<(), AccessError>,
    ) -> Result<AppendResult, AccessError> {
        limits.validate()?;
        let edge = layout::decode(bytes)?;
        let mut probe = Page::new(PageType::Adjacency, self.workspace, file.file_id(), 0);
        layout::initialize(&mut probe, source)?;
        layout::append(&mut probe, bytes)?;
        let policy = InsertPolicy::in_place(0);
        self.lock_endpoints(log, chain, txn, source, edge.destination)?;
        let mut tail = None;
        let mut at = head;
        let mut seen = BTreeSet::new();
        let mut maximum = 0;
        {
            let segment = Segment::open_pooled(self.pool, file, segment_header, self.workspace)
                .map_err(TableAccessError::Segment)?;
            self.check_segment(&segment)?;
            while let Some(address) = at {
                visit(address, 0)?;
                if seen.len() >= limits.pages {
                    return Err(AccessError::Budget("adjacency page budget exceeded"));
                }
                if !seen.insert(address) {
                    return Err(AccessError::Chain("adjacency chain cycle"));
                }
                let page = self.current(address)?;
                self.check_page(&segment, address, source, &page)?;
                visit(address, page.slot_count() as usize)?;
                if let Some(last) = layout::last_identity(&page)? {
                    let first_id = (1..=page.slot_count())
                        .next()
                        .map(|_| {
                            // Includes deleted identities; directory order was validated.
                            let entry = page
                                .slot(0)
                                .ok_or(AccessError::Chain("missing adjacency slot"))?;
                            let offset = entry.offset() as usize;
                            let mut raw = [0; 8];
                            raw[..6].copy_from_slice(&page.as_bytes()[offset..offset + 6]);
                            Ok::<u64, AccessError>(u64::from_le_bytes(raw))
                        })
                        .transpose()?
                        .unwrap_or(0);
                    if first_id <= maximum {
                        return Err(AccessError::Chain("unordered adjacency chain identities"));
                    }
                    maximum = last;
                }
                at = layout::header(&page)?.next;
                tail = Some((address, page));
            }
        }
        if edge.id <= maximum {
            return Err(AdjacencyError::IdentityOrder.into());
        }
        let target = match &tail {
            Some((address, page))
                if (page.slot_count() as usize) < MAX_SLOTS
                    && page.free_space() >= bytes.len() + 2 + ITL_ENTRY_LEN =>
            {
                *address
            }
            _ => {
                if seen.len() >= limits.pages {
                    return Err(AccessError::Budget("adjacency page budget exceeded"));
                }
                let address = self.allocate(log, txn, file, segment_header, source)?;
                visit(address, 0)?;
                address
            }
        };
        let position = writes::insert(
            self.pool,
            log,
            chain,
            txn,
            self.key(target)?,
            bytes,
            &policy,
        )?;
        if let Some((previous, _)) = tail {
            if previous != target {
                writes::link(
                    self.pool,
                    log,
                    chain,
                    txn,
                    self.key(previous)?,
                    target,
                    &policy,
                )?;
            }
        }
        Ok(AppendResult {
            head: head.unwrap_or(target),
            edge: position,
        })
    }
    /// Lock graph-resolved, live heap vertices in physical ROWID order. Graph
    /// callers acquire these before reading the source-head index, and retain
    /// them through all forward/reverse/property index writes and final commit.
    /// On conflict, roll back the statement before waiting/retrying; partial
    /// endpoint locking is otherwise retained by the ordinary transaction.
    pub fn lock_endpoints(
        &self,
        log: &mut GroupWriter<'_, '_>,
        chain: &mut UndoChain<'_, '_>,
        txn: &Txn,
        source: RowId,
        destination: RowId,
    ) -> Result<(), AccessError> {
        let policy = InsertPolicy::in_place(0);
        for endpoint in BTreeSet::from([source, destination]) {
            if endpoint.row_id() == 0 {
                return Err(AccessError::Chain("endpoint must be a vertex row"));
            }
            write::lock_row(
                self.pool,
                log,
                chain,
                txn,
                self.key(endpoint)?,
                endpoint.row_id(),
                &policy,
            )?;
        }
        Ok(())
    }
    fn edge_page(
        &self,
        file: &mut DataFile<'_>,
        segment_header: u32,
        source: RowId,
        edge: RowId,
    ) -> Result<Page, AccessError> {
        let segment = Segment::open_pooled(self.pool, file, segment_header, self.workspace)
            .map_err(TableAccessError::Segment)?;
        self.check_segment(&segment)?;
        let address = self.address(edge.file_id(), edge.block_id())?;
        let page = self.current(address)?;
        self.check_page(&segment, address, source, &page)?;
        layout::record(&page, edge.row_id())?;
        Ok(page)
    }
    /// Replace edge data at a graph-owned stable locator, preserving its ID and
    /// destination. Endpoint locks, edge undo and caller index changes share the
    /// same transaction; the caller rolls back the statement on any failure.
    #[allow(clippy::too_many_arguments)]
    pub fn update(
        &self,
        log: &mut GroupWriter<'_, '_>,
        chain: &mut UndoChain<'_, '_>,
        txn: &mut Txn,
        file: &mut DataFile<'_>,
        segment_header: u32,
        source: RowId,
        edge: RowId,
        bytes: &[u8],
    ) -> Result<(), AccessError> {
        let page = self.edge_page(file, segment_header, source, edge)?;
        let old = layout::decode(layout::record(&page, edge.row_id())?)?;
        let replacement = layout::decode(bytes)?;
        if (old.id, old.destination) != (replacement.id, replacement.destination) {
            return Err(AdjacencyError::IdentityChanged.into());
        }
        self.lock_endpoints(log, chain, txn, source, old.destination)?;
        writes::update(
            self.pool,
            log,
            chain,
            txn,
            self.key(edge)?,
            edge.row_id(),
            bytes,
            &InsertPolicy::in_place(0),
        )?;
        Ok(())
    }
    /// Delete an edge at a graph-owned locator with ordered endpoint locks and
    /// a full old-value undo record. Directory ordinals remain reserved.
    #[allow(clippy::too_many_arguments)]
    pub fn delete(
        &self,
        log: &mut GroupWriter<'_, '_>,
        chain: &mut UndoChain<'_, '_>,
        txn: &mut Txn,
        file: &mut DataFile<'_>,
        segment_header: u32,
        source: RowId,
        edge: RowId,
    ) -> Result<(), AccessError> {
        let page = self.edge_page(file, segment_header, source, edge)?;
        let old = layout::decode(layout::record(&page, edge.row_id())?)?;
        self.lock_endpoints(log, chain, txn, source, old.destination)?;
        writes::delete(
            self.pool,
            log,
            chain,
            txn,
            self.key(edge)?,
            edge.row_id(),
            &InsertPolicy::in_place(0),
        )?;
        Ok(())
    }
    /// Fetch exactly one stable locator at a snapshot. Its physical source is
    /// returned for the graph adapter to verify against a graph-scoped node.
    /// A deleted or not-yet-visible directory entry returns None; it is never
    /// read through the ordinary heap decoder.
    pub fn fetch(
        &self,
        file: &mut DataFile<'_>,
        segment_header: u32,
        edge: RowId,
        view: ReadView,
        chain: &UndoChain<'_, '_>,
    ) -> Result<Option<(RowId, Vec<u8>)>, AccessError> {
        if edge.row_id() == 0 {
            return Err(AccessError::Chain(
                "edge locator must name a directory entry",
            ));
        }
        let segment = Segment::open_pooled(self.pool, file, segment_header, self.workspace)
            .map_err(TableAccessError::Segment)?;
        self.check_segment(&segment)?;
        let address = self.address(edge.file_id(), edge.block_id())?;
        let current = self.current(address)?;
        let source = layout::header(&current)?.source;
        self.check_page(&segment, address, source, &current)?;
        let page = cr::reconstruct(&current, view, chain).map_err(AccessError::Snapshot)?;
        self.check_page(&segment, address, source, &page)?;
        let Some(slot) = page.slot(edge.row_id() as usize - 1) else {
            return Ok(None);
        };
        if slot.status() == bicdb_storage::page::SlotStatus::Free {
            return Ok(None);
        }
        Ok(Some((
            source,
            layout::record(&page, edge.row_id())?.to_vec(),
        )))
    }

    /// Read a complete source chain at one snapshot, rejecting truncation/cycles
    /// and excessive retained bytes. Source visibility is checked by the caller.
    #[allow(clippy::too_many_arguments)]
    pub fn read(
        &self,
        file: &mut DataFile<'_>,
        segment_header: u32,
        source: RowId,
        head: Option<RowId>,
        view: ReadView,
        chain: &UndoChain<'_, '_>,
        limits: ChainLimits,
    ) -> Result<Vec<(RowId, Vec<u8>)>, AccessError> {
        self.read_with(
            file,
            segment_header,
            source,
            head,
            view,
            chain,
            limits,
            |_, _| Ok(()),
        )
    }
    /// Read with a caller checkpoint per page. Directory count includes deleted
    /// entries, so old/tombstoned pages cannot bypass a graph work budget.
    #[allow(clippy::too_many_arguments)]
    pub fn read_with(
        &self,
        file: &mut DataFile<'_>,
        segment_header: u32,
        source: RowId,
        head: Option<RowId>,
        view: ReadView,
        chain: &UndoChain<'_, '_>,
        limits: ChainLimits,
        mut visit: impl FnMut(RowId, usize) -> Result<(), AccessError>,
    ) -> Result<Vec<(RowId, Vec<u8>)>, AccessError> {
        limits.validate()?;
        let segment = Segment::open_pooled(self.pool, file, segment_header, self.workspace)
            .map_err(TableAccessError::Segment)?;
        self.check_segment(&segment)?;
        let mut rows = Vec::new();
        let mut seen = BTreeSet::new();
        let mut at = head;
        let mut total = 0usize;
        let mut maximum = 0;
        while let Some(address) = at {
            visit(address, 0)?;
            if seen.len() >= limits.pages {
                return Err(AccessError::Budget("adjacency page budget exceeded"));
            }
            if !seen.insert(address) {
                return Err(AccessError::Chain("adjacency chain cycle"));
            }
            let current = self.current(address)?;
            self.check_page(&segment, address, source, &current)?;
            let page = cr::reconstruct(&current, view, chain).map_err(AccessError::Snapshot)?;
            self.check_page(&segment, address, source, &page)?;
            visit(address, page.slot_count() as usize)?;
            for ordinal in 1..=page.slot_count() {
                if page
                    .slot(ordinal as usize - 1)
                    .is_some_and(|s| s.status() == bicdb_storage::page::SlotStatus::Free)
                {
                    continue;
                }
                let bytes = layout::record(&page, ordinal)?;
                let id = layout::decode(bytes)?.id;
                if id <= maximum {
                    return Err(AccessError::Chain("unordered adjacency chain identities"));
                }
                maximum = id;
                total = total.saturating_add(bytes.len());
                if rows.len() >= limits.edges || total > limits.bytes {
                    return Err(AccessError::Budget("adjacency result budget exceeded"));
                }
                rows.push((
                    RowId::from_parts(address.file_id(), address.block_id(), ordinal)
                        .map_err(|_| AccessError::Chain("invalid adjacency edge address"))?,
                    bytes.to_vec(),
                ));
            }
            at = layout::header(&page)?.next;
        }
        Ok(rows)
    }
}
