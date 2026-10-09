//! Native adjacency data port used by the SQL graph adapter.
//!
//! All routes are graph-owned and resolved before entering the ordinary engine
//! context. This port never starts, commits or rolls back a transaction. Its
//! caller must publish the graph manifest, node changes, property indexes and
//! full-text events in the same transaction, and roll back on any error.
//! Merely creating these routes does not switch a v2 SQL graph to v3.

use bicdb_access::adjacency::{AccessError, AdjacencyAccess, ChainLimits};
use bicdb_catalog::{ddl, dict, Catalog};
use bicdb_common::seq::CommitSeq;
use bicdb_exec::{ColKind, Row, RowShape, Value};
use bicdb_graph::adjacency_record::{self as codec, Placement, SourceEntry};
use bicdb_graph::{Deadline, Edge, Error, Limits};
use bicdb_storage::buffer::{BufferKey, BufferPool};
use bicdb_storage::cr::ReadView;
use bicdb_storage::datafile::DataFile;
use bicdb_storage::heap::InsertPolicy;
use bicdb_storage::page::Page;
use bicdb_storage::rowid::{Rdba, RowId};
use bicdb_storage::segment::{self, SegType, Segment};
use bicdb_storage::undo::UndoChain;
use bicdb_txn::write::Txn;
use bicdb_types::Number;
use bicdb_wal::group::GroupWriter;
use std::collections::{BTreeMap, BTreeSet};

fn error(e: impl std::fmt::Display) -> Error {
    Error(e.to_string())
}
fn shape() -> RowShape {
    RowShape::new(vec![ColKind::Number, ColKind::Bytes])
}
fn row(ordinal: u64, data: &[u8]) -> Result<Vec<u8>, Error> {
    bicdb_exec::encode_row(
        &Row::new(vec![
            Value::Number(Number::parse(&ordinal.to_string()).expect("u64")),
            Value::Bytes(data.to_vec()),
        ]),
        &shape(),
    )
    .map_err(error)
}
fn ordinal_key(ordinal: u64) -> Result<Vec<u8>, Error> {
    bicdb_catalog::row::key_from_row(&row(ordinal, &[])?, &[0]).map_err(error)
}
fn decode_row(bytes: &[u8]) -> Result<(u64, Vec<u8>), Error> {
    let row = bicdb_exec::decode_row(bytes, &shape()).map_err(error)?;
    let [Value::Number(ordinal), Value::Bytes(data)] = row.values.as_slice() else {
        return Err(error("invalid graph ordinal row"));
    };
    Ok((ordinal.to_string().parse().map_err(error)?, data.clone()))
}

/// Frozen record heap and primary B-tree identities, resolved in one graph.
#[derive(Debug, Clone, Copy)]
pub struct GraphRecords {
    /// Protected graph heap, with `(ordinal NUMBER, data BYTES)` rows.
    pub heap: ddl::GraphPhysicalRoute,
    /// Ordinal index; its values are real heap ROWIDs.
    pub primary: ddl::GraphPhysicalRoute,
}
impl GraphRecords {
    /// Resolve the protected graph and its unique ordinal index without writing.
    pub fn resolve(cat: &mut Catalog<'_>, graph: &str, seq: CommitSeq) -> Result<Self, Error> {
        let object = cat
            .resolve(seq, dict::namespace::TABLE, graph)
            .map_err(error)?;
        if object.type_code != dict::obj_kind::GRAPH || object.status != 1 {
            return Err(error("adjacency records require a live named graph"));
        }
        let candidates: Vec<_> = cat
            .indexes_of(seq, object.obj)
            .map_err(error)?
            .into_iter()
            .filter(|i| {
                i.kind == dict::index_kind::BTREE
                    && i.status == 1
                    && i.is_unique
                    && i.cols.len() == 1
                    && i.cols[0].col == 1
                    && i.cols[0].pos == 1
                    && !i.cols[0].is_desc
            })
            .collect();
        if candidates.len() != 1 {
            return Err(error("missing or ambiguous graph ordinal index"));
        }
        Ok(Self {
            heap: ddl::GraphPhysicalRoute {
                obj: object.obj,
                block: ddl::live_segment_block(cat, object.obj).map_err(error)?,
            },
            primary: ddl::GraphPhysicalRoute {
                obj: candidates[0].obj,
                block: ddl::live_segment_block(cat, candidates[0].obj).map_err(error)?,
            },
        })
    }
}
/// Native work counters; retained bytes do not pretend to measure process RSS.
#[derive(Debug, Clone, Copy, Default)]
pub struct AdjacencyWork {
    /// Candidate entries returned by native B-trees.
    pub index_entries: usize,
    /// Heap records examined, including invisible candidates.
    pub heap_rows: usize,
    /// Heap mutations attempted through this port.
    pub heap_writes: usize,
    /// B-tree maintenance attempts, including already-present entries.
    pub index_writes: usize,
    /// Adjacency pages examined or allocated.
    pub adjacency_pages: usize,
    /// Edge directory slots examined, including tombstones.
    pub directory_entries: usize,
    /// Retained/read/planned native payload bytes.
    pub bytes: usize,
}
impl AdjacencyWork {
    pub(crate) fn units(self) -> usize {
        self.index_entries
            .saturating_add(self.heap_rows)
            .saturating_add(self.heap_writes)
            .saturating_add(self.index_writes)
            .saturating_add(self.adjacency_pages)
            .saturating_add(self.directory_entries)
    }
    fn check(self, limits: &Limits, deadline: Deadline) -> Result<(), Error> {
        deadline.check("native adjacency")?;
        if self.units() > limits.max_expansions || self.bytes > limits.max_text_bytes {
            return Err(error("native adjacency work/byte budget exceeded"));
        }
        Ok(())
    }
}
#[derive(Clone)]
struct Record {
    /// Stable index entrance and currently resolved location are distinguished.
    entrance: RowId,
    location: RowId,
    data: Vec<u8>,
}
#[derive(Clone, Copy)]
struct Anchor {
    id: u64,
    rid: RowId,
}
struct Located {
    rid: RowId,
    source: RowId,
    bytes: Vec<u8>,
    edge: Edge,
}
/// One statement's scoped adjacency reader/writer; routes never follow a later
/// catalog rebuild. Endpoints are resolved from this graph's checksum rows.
pub struct NativeAdjacency<'a, 'io> {
    pool: &'a BufferPool<'io>,
    ws: [u8; 8],
    records: GraphRecords,
    routes: ddl::GraphPhysicalRoutes,
    limits: Limits,
    deadline: Deadline,
    work: AdjacencyWork,
    seen_nodes: BTreeSet<u64>,
    seen_edges: BTreeSet<u64>,
    table: bicdb_access::TableAccess<'a, 'io>,
}
impl<'a, 'io> NativeAdjacency<'a, 'io> {
    /// Bind frozen graph routes and a statement's shared limits/clock.
    pub fn new(
        pool: &'a BufferPool<'io>,
        ws: [u8; 8],
        records: GraphRecords,
        routes: ddl::GraphPhysicalRoutes,
        limits: Limits,
        deadline: Deadline,
    ) -> Result<Self, Error> {
        if routes.graph != records.heap.obj {
            return Err(error("adjacency routes belong to another graph"));
        }
        let objects = [
            records.heap,
            records.primary,
            routes.adjacency,
            routes.source,
            routes.locator,
            routes.incoming,
        ];
        if objects.iter().any(|r| r.obj == 0 || r.block == 0)
            || objects.iter().map(|r| r.obj).collect::<BTreeSet<_>>().len() != 6
            || objects
                .iter()
                .map(|r| r.block)
                .collect::<BTreeSet<_>>()
                .len()
                != 6
        {
            return Err(error("invalid or aliased adjacency route set"));
        }
        Ok(Self {
            pool,
            ws,
            records,
            routes,
            limits,
            deadline,
            work: AdjacencyWork::default(),
            seen_nodes: BTreeSet::new(),
            seen_edges: BTreeSet::new(),
            table: bicdb_access::TableAccess::new(pool, ws),
        })
    }
    /// Report native work performed through this statement port.
    pub fn work(&self) -> AdjacencyWork {
        self.work
    }
    fn check(&self) -> Result<(), Error> {
        self.work.check(&self.limits, self.deadline)
    }
    fn chain_limits(&self) -> ChainLimits {
        ChainLimits {
            pages: self.limits.max_expansions.clamp(1, 100000),
            edges: self.limits.max_edges.min(500000),
            bytes: self.limits.max_text_bytes.min(100 * 1024 * 1024),
        }
    }
    fn segment(
        &self,
        file: &mut DataFile<'_>,
        route: ddl::GraphPhysicalRoute,
        kind: SegType,
    ) -> Result<(), Error> {
        let segment = Segment::open_pooled(self.pool, file, route.block, self.ws).map_err(error)?;
        let header = segment.header();
        if header.seg_type != kind
            || header.obj != route.obj
            || header.dataobj != route.obj
            || segment.workspace_ref() != self.ws
        {
            return Err(error("adjacency segment ownership/type mismatch"));
        }
        Ok(())
    }
    fn member(&self, file: &mut DataFile<'_>, rid: RowId) -> Result<(), Error> {
        self.segment(file, self.records.heap, SegType::Heap)?;
        let heap = Segment::open_pooled(self.pool, file, self.records.heap.block, self.ws)
            .map_err(error)?;
        if rid.file_id() != heap.file_id()
            || rid.row_id() == 0
            || !heap
                .logical_of_block(rid.block_id())
                .is_some_and(|p| p < heap.hwm())
        {
            return Err(error("graph record ROWID outside its graph heap"));
        }
        Ok(())
    }
    fn range(
        &mut self,
        file: &mut DataFile<'_>,
        route: ddl::GraphPhysicalRoute,
        lower: Option<&[u8]>,
        upper: Option<&[u8]>,
    ) -> Result<Vec<(Vec<u8>, RowId)>, Error> {
        self.check()?;
        self.segment(file, route, SegType::BTree)?;
        let rdba = Rdba::from_parts(file.file_id(), route.block)
            .ok_or_else(|| error("invalid graph route address"))?;
        let page = self
            .pool
            .pin(BufferKey::new(self.ws, rdba))
            .map_err(error)?;
        let header = Page::from_bytes(Box::new(*page.as_bytes()));
        drop(page);
        let root = segment::read_tree_head(&header).map_err(error)?;
        let mut store = bicdb_index::ReadOnlyStore::new(self.pool, file.file_id(), self.ws);
        let mut tree = bicdb_index::Tree::open(&mut store, file.file_id(), root).map_err(error)?;
        let rows = tree
            .range(
                lower,
                upper,
                self.limits
                    .max_expansions
                    .saturating_sub(self.work.units())
                    .saturating_add(1),
            )
            .map_err(error)?;
        self.work.index_entries = self.work.index_entries.saturating_add(rows.len());
        self.check()?;
        Ok(rows)
    }
    fn record_at(
        &mut self,
        file: &mut DataFile<'_>,
        chain: &UndoChain<'_, '_>,
        view: ReadView,
        rid: RowId,
        ordinal: u64,
    ) -> Result<Option<Record>, Error> {
        self.member(file, rid)?;
        self.work.heap_rows = self.work.heap_rows.saturating_add(1);
        self.check()?;
        let visible = bicdb_storage::scan::fetch_rows_resolved(self.pool, chain, view, &[rid])
            .map_err(error)?;
        let Some((location, bytes)) = visible.into_iter().next().flatten() else {
            return Ok(None);
        };
        self.member(file, location)?;
        let (actual, data) = decode_row(&bytes)?;
        // Stale index keys are candidates, not ownership/equality proofs.
        if ordinal != actual {
            return Ok(None);
        }
        self.work.bytes = self.work.bytes.saturating_add(data.len());
        self.check()?;
        Ok(Some(Record {
            entrance: rid,
            location,
            data,
        }))
    }
    fn record(
        &mut self,
        file: &mut DataFile<'_>,
        chain: &UndoChain<'_, '_>,
        view: ReadView,
        ordinal: u64,
    ) -> Result<Option<Record>, Error> {
        let key = ordinal_key(ordinal)?;
        let candidates = self.range(file, self.records.primary, Some(&key), Some(&key))?;
        let mut result: Option<Record> = None;
        for (candidate, rid) in candidates {
            if candidate != key {
                continue;
            }
            if let Some(record) = self.record_at(file, chain, view, rid, ordinal)? {
                if result
                    .as_ref()
                    .is_some_and(|old| old.location != record.location)
                {
                    return Err(error("duplicate visible graph ordinal"));
                }
                result = Some(record);
            }
        }
        Ok(result)
    }
    /// Resolve ordinal candidates using this manifest's frozen segment routes.
    /// Every row is checked against its actual key, graph heap and snapshot.
    pub(crate) fn records_range(
        &mut self,
        file: &mut DataFile<'_>,
        chain: &UndoChain<'_, '_>,
        view: ReadView,
        lower: u64,
        upper: u64,
        max_entries: usize,
    ) -> Result<BTreeMap<u64, Vec<u8>>, Error> {
        let candidates = self.range(
            file,
            self.records.primary,
            Some(&ordinal_key(lower)?),
            Some(&ordinal_key(upper)?),
        )?;
        if candidates.len() > max_entries {
            return Err(error("graph record index entry budget exceeded"));
        }
        let mut result = BTreeMap::new();
        let mut seen = BTreeSet::new();
        for (key, rid) in candidates {
            self.member(file, rid)?;
            self.work.heap_rows = self.work.heap_rows.saturating_add(1);
            self.check()?;
            let row = bicdb_storage::scan::fetch_rows_resolved(self.pool, chain, view, &[rid])
                .map_err(error)?;
            let Some((location, bytes)) = row.into_iter().next().flatten() else {
                continue;
            };
            self.member(file, location)?;
            let (ordinal, data) = decode_row(&bytes)?;
            if ordinal < lower || ordinal > upper || key != ordinal_key(ordinal)? {
                continue;
            }
            if !seen.insert(location) {
                continue;
            }
            self.work.bytes = self.work.bytes.saturating_add(data.len());
            self.check()?;
            if result.insert(ordinal, data).is_some() {
                return Err(error("duplicate graph ordinal"));
            }
        }
        Ok(result)
    }
    fn anchor(
        &mut self,
        file: &mut DataFile<'_>,
        chain: &UndoChain<'_, '_>,
        view: ReadView,
        id: u64,
    ) -> Result<Option<Anchor>, Error> {
        let ordinal = bicdb_graph::storage::entity_range(1, id)?.0;
        let Some(record) = self.record(file, chain, view, ordinal)? else {
            return Ok(None);
        };
        if record.data.len() != 32 || record.entrance != record.location {
            return Err(error(
                "invalid or moved graph vertex anchor; Move protocol required",
            ));
        }
        self.seen_nodes.insert(id);
        if self.seen_nodes.len() > self.limits.max_nodes {
            return Err(error("native adjacency node budget exceeded"));
        }
        Ok(Some(Anchor {
            id,
            rid: record.entrance,
        }))
    }
    fn source_entry(
        &mut self,
        file: &mut DataFile<'_>,
        chain: &UndoChain<'_, '_>,
        view: ReadView,
        anchor: Anchor,
    ) -> Result<Option<SourceEntry>, Error> {
        let key = codec::source_key(anchor.rid)?;
        let candidates = self.range(file, self.routes.source, Some(&key), Some(&key))?;
        let mut result = None;
        let mut rows = BTreeSet::new();
        for (candidate, rid) in candidates {
            if candidate != key {
                continue;
            }
            if let Some(record) =
                self.record_at(file, chain, view, rid, codec::source_ordinal(anchor.id)?)?
            {
                if !rows.insert(record.location) {
                    continue;
                }
                let entry = SourceEntry::decode(&record.data)?;
                if entry.source != anchor.rid || result.is_some() {
                    return Err(error("invalid or duplicate graph source entry"));
                }
                result = Some(entry);
            }
        }
        Ok(result)
    }
    fn decoded(
        &mut self,
        file: &mut DataFile<'_>,
        chain: &UndoChain<'_, '_>,
        view: ReadView,
        source: RowId,
        bytes: &[u8],
        expected: u64,
    ) -> Result<Edge, Error> {
        self.work.bytes = self.work.bytes.saturating_add(bytes.len());
        self.check()?;
        let mut overflow = BTreeMap::new();
        if let Some((lower, upper)) = codec::overflow_range(expected, bytes, &self.limits)? {
            for ordinal in lower..=upper {
                let record = self
                    .record(file, chain, view, ordinal)?
                    .ok_or_else(|| error("missing adjacency overflow row"))?;
                overflow.insert(ordinal, record.data);
            }
        }
        let edge = codec::decode(expected, source, bytes, &overflow, &self.limits)?;
        let physical = bicdb_storage::adjacency::decode(bytes).map_err(error)?;
        let src = self
            .anchor(file, chain, view, edge.source)?
            .ok_or_else(|| error("missing graph source anchor"))?;
        let dst = self
            .anchor(file, chain, view, edge.target)?
            .ok_or_else(|| error("missing graph target anchor"))?;
        if src.rid != source || dst.rid != physical.destination {
            return Err(error("adjacency logical/physical endpoint mismatch"));
        }
        self.seen_edges.insert(edge.id);
        if self.seen_edges.len() > self.limits.max_edges {
            return Err(error("native adjacency edge budget exceeded"));
        }
        Ok(edge)
    }
    fn located(
        &mut self,
        file: &mut DataFile<'_>,
        chain: &UndoChain<'_, '_>,
        view: ReadView,
        id: u64,
    ) -> Result<Option<Located>, Error> {
        let key = codec::locator_key(id)?;
        let candidates = self.range(file, self.routes.locator, Some(&key), Some(&key))?;
        let mut found = None;
        let mut seen = BTreeSet::new();
        for (candidate, rid) in candidates {
            if candidate != key || !seen.insert(rid) {
                continue;
            }
            self.segment(file, self.routes.adjacency, SegType::Adjacency)?;
            self.work.adjacency_pages = self.work.adjacency_pages.saturating_add(1);
            self.work.directory_entries = self.work.directory_entries.saturating_add(1);
            self.check()?;
            let Some((source, bytes)) = AdjacencyAccess::new(self.pool, self.ws)
                .fetch(file, self.routes.adjacency.block, rid, view, chain)
                .map_err(error)?
            else {
                continue;
            };
            let edge = self.decoded(file, chain, view, source, &bytes, id)?;
            if found.is_some() {
                return Err(error("duplicate live graph edge locator"));
            }
            found = Some(Located {
                rid,
                source,
                bytes,
                edge,
            });
        }
        self.check()?;
        Ok(found)
    }
    /// Read a single edge through its actual stable adjacency ROWID.
    pub fn edge(
        &mut self,
        file: &mut DataFile<'_>,
        chain: &UndoChain<'_, '_>,
        view: ReadView,
        id: u64,
    ) -> Result<Option<(RowId, Edge)>, Error> {
        Ok(self
            .located(file, chain, view, id)?
            .map(|v| (v.rid, v.edge)))
    }
    /// Read a complete outgoing chain with metadata, pages, endpoints and
    /// external properties all evaluated under the same snapshot.
    pub fn outgoing(
        &mut self,
        file: &mut DataFile<'_>,
        chain: &UndoChain<'_, '_>,
        view: ReadView,
        node: u64,
        types: &[String],
    ) -> Result<Vec<(RowId, Edge)>, Error> {
        let Some(anchor) = self.anchor(file, chain, view, node)? else {
            return Ok(vec![]);
        };
        let Some(entry) = self.source_entry(file, chain, view, anchor)? else {
            return Ok(vec![]);
        };
        self.segment(file, self.routes.adjacency, SegType::Adjacency)?;
        let limits = self.chain_limits();
        let mut pages = BTreeSet::new();
        let mut tail = None;
        let records = AdjacencyAccess::new(self.pool, self.ws)
            .read_with(
                file,
                self.routes.adjacency.block,
                anchor.rid,
                Some(entry.head),
                view,
                chain,
                limits,
                |address, slots| {
                    tail = Some(address);
                    if pages.insert(address) {
                        self.work.adjacency_pages = self.work.adjacency_pages.saturating_add(1);
                    }
                    self.work.directory_entries = self.work.directory_entries.saturating_add(slots);
                    self.work.check(&self.limits, self.deadline).map_err(|e| {
                        if e.0.contains("time budget") {
                            AccessError::Budget("graph statement time budget exceeded")
                        } else {
                            AccessError::Budget("native adjacency work/byte budget exceeded")
                        }
                    })
                },
            )
            .map_err(error)?;
        if tail != Some(entry.tail) {
            return Err(error("graph source tail metadata mismatch"));
        }
        let mut result = vec![];
        for (rid, bytes) in records {
            let id = bicdb_storage::adjacency::decode(&bytes).map_err(error)?.id;
            if id > entry.last_id {
                return Err(error("graph source identity watermark mismatch"));
            }
            let edge = self.decoded(file, chain, view, anchor.rid, &bytes, id)?;
            if types.is_empty() || types.contains(&edge.label) {
                result.push((rid, edge));
            }
        }
        self.check()?;
        Ok(result)
    }
    /// Read incoming candidates and recheck full type and both endpoints.
    pub fn incoming(
        &mut self,
        file: &mut DataFile<'_>,
        chain: &UndoChain<'_, '_>,
        view: ReadView,
        node: u64,
        types: &[String],
    ) -> Result<Vec<(RowId, Edge)>, Error> {
        let Some(target) = self.anchor(file, chain, view, node)? else {
            return Ok(vec![]);
        };
        let prefixes = if types.is_empty() {
            vec![codec::incoming_prefix(target.rid, None)?]
        } else {
            types
                .iter()
                .map(|t| codec::incoming_prefix(target.rid, Some(t)))
                .collect::<Result<Vec<_>, _>>()?
        };
        let mut result = BTreeMap::new();
        for prefix in prefixes {
            let upper = bicdb_graph::property_index::prefix_successor(&prefix);
            for (key, rid) in
                self.range(file, self.routes.incoming, Some(&prefix), upper.as_deref())?
            {
                if !key.starts_with(&prefix) {
                    continue;
                }
                self.segment(file, self.routes.adjacency, SegType::Adjacency)?;
                self.work.adjacency_pages = self.work.adjacency_pages.saturating_add(1);
                self.work.directory_entries = self.work.directory_entries.saturating_add(1);
                self.check()?;
                let Some((source, bytes)) = AdjacencyAccess::new(self.pool, self.ws)
                    .fetch(file, self.routes.adjacency.block, rid, view, chain)
                    .map_err(error)?
                else {
                    continue;
                };
                let id = bicdb_storage::adjacency::decode(&bytes).map_err(error)?.id;
                let edge = self.decoded(file, chain, view, source, &bytes, id)?;
                if edge.target != node
                    || (!types.is_empty() && !types.contains(&edge.label))
                    || codec::incoming_key(target.rid, &edge.label, source, id)? != key
                {
                    continue;
                }
                if result
                    .insert(id, (rid, edge))
                    .is_some_and(|(old, _)| old != rid)
                {
                    return Err(error("duplicate incoming graph edge identity"));
                }
            }
        }
        self.check()?;
        Ok(result.into_values().collect())
    }
    #[allow(clippy::too_many_arguments)]
    fn index(
        &mut self,
        file: &mut DataFile<'_>,
        log: &mut GroupWriter<'_, '_>,
        txn: &Txn,
        route: ddl::GraphPhysicalRoute,
        key: &[u8],
        rid: RowId,
    ) -> Result<(), Error> {
        self.work.index_writes = self.work.index_writes.saturating_add(1);
        self.check()?;
        self.segment(file, route, SegType::BTree)?;
        let root = bicdb_access::index::insert_entry(
            self.pool,
            log,
            file,
            self.ws,
            route.block,
            txn,
            key,
            rid,
        )
        .map_err(error)?;
        bicdb_access::index::write_tree_head_redo(
            self.pool,
            log,
            file,
            self.ws,
            route.block,
            txn,
            root,
        )
        .map_err(error)
    }
    #[allow(clippy::too_many_arguments)]
    fn put(
        &mut self,
        file: &mut DataFile<'_>,
        log: &mut GroupWriter<'_, '_>,
        chain: &mut UndoChain<'_, '_>,
        txn: &mut Txn,
        view: ReadView,
        ordinal: u64,
        data: &[u8],
    ) -> Result<RowId, Error> {
        bicdb_txn::write::checkpoint_safe_point(self.pool, log, chain).map_err(error)?;
        if data.len() > 4096 {
            return Err(error("oversized graph native record"));
        }
        let previous = self.record(file, chain, view, ordinal)?;
        self.work.heap_writes = self.work.heap_writes.saturating_add(1);
        self.check()?;
        let bytes = row(ordinal, data)?;
        if let Some(old) = previous {
            if bytes.len() <= row(ordinal, &old.data)?.len() {
                self.table
                    .update(
                        log,
                        chain,
                        txn,
                        file,
                        self.records.heap.block,
                        old.location,
                        &bytes,
                        &InsertPolicy::in_place(0),
                    )
                    .map_err(error)?;
                return Ok(old.entrance);
            }
            // Do not grow a snapshot-bearing overflow row through a forwarding
            // pointer: retain its old entrance for old views, append a new row.
            self.work.heap_writes = self.work.heap_writes.saturating_add(1);
            self.check()?;
            self.table
                .delete(
                    log,
                    chain,
                    txn,
                    file,
                    old.location,
                    &InsertPolicy::in_place(0),
                )
                .map_err(error)?;
        }
        let rid = self
            .table
            .insert(
                log,
                chain,
                txn,
                file,
                self.records.heap.block,
                &bytes,
                &InsertPolicy::in_place(0),
            )
            .map_err(error)?;
        self.index(
            file,
            log,
            txn,
            self.records.primary,
            &ordinal_key(ordinal)?,
            rid,
        )?;
        Ok(rid)
    }
    fn write_view(view: ReadView, txn: &Txn) -> Result<(), Error> {
        if view.own != Some(txn.txn_id) {
            return Err(error(
                "adjacency mutation requires its own transaction view",
            ));
        }
        Ok(())
    }
    /// Serialize publication on the current manifest row and reject a stale
    /// source generation/routes, including edge edits with unchanged counts.
    /// Called inside one engine write context before any topology mutation.
    #[allow(clippy::too_many_arguments)]
    pub fn guard_authority(
        &mut self,
        file: &mut DataFile<'_>,
        log: &mut GroupWriter<'_, '_>,
        chain: &mut UndoChain<'_, '_>,
        txn: &mut Txn,
        expected: &[u8],
    ) -> Result<(), Error> {
        let view = ReadView::new(
            CommitSeq::from_raw(bicdb_common::seq::SEQ_MAX).expect("latest visibility"),
        )
        .with_own(Some(txn.txn_id));
        let current = self
            .record(file, chain, view, 0)?
            .ok_or_else(|| error("missing current native authority"))?;
        let location = current.location;
        let block = BufferKey::new(
            self.ws,
            Rdba::from_parts(location.file_id(), location.block_id())
                .ok_or_else(|| error("invalid manifest row address"))?,
        );
        bicdb_txn::write::lock_row(
            self.pool,
            log,
            chain,
            txn,
            block,
            location.row_id(),
            &InsertPolicy::in_place(0),
        )
        .map_err(error)?;
        if current.data != expected {
            return Err(error("native graph source changed; retry the statement"));
        }
        Ok(())
    }
    /// Apply graph heap rows inside the caller's transaction, retaining old
    /// index entrances on growth. Node checksum rows remain fixed-size anchors.
    /// This trusted kernel API does not validate logical graph constraints;
    /// callers must validate the image and atomically publish its authority.
    #[allow(clippy::too_many_arguments)]
    pub fn apply_records(
        &mut self,
        file: &mut DataFile<'_>,
        log: &mut GroupWriter<'_, '_>,
        chain: &mut UndoChain<'_, '_>,
        txn: &mut Txn,
        view: ReadView,
        patch: &bicdb_graph::storage::StoragePatch,
    ) -> Result<(), Error> {
        Self::write_view(view, txn)?;
        for key in &patch.removed {
            self.remove(file, log, chain, txn, view, *key)?;
        }
        for (key, data) in patch.updated.iter().chain(&patch.inserted) {
            self.put(file, log, chain, txn, view, *key, data)?;
        }
        Ok(())
    }
    /// Insert a previously reserved, never-used logical edge ID. The caller
    /// supplies the latest engine sequence with own transaction visibility and
    /// holds graph write serialization through publication. Endpoint locks are
    /// acquired before the source metadata is looked up.
    #[allow(clippy::too_many_arguments)]
    pub fn insert(
        &mut self,
        file: &mut DataFile<'_>,
        log: &mut GroupWriter<'_, '_>,
        chain: &mut UndoChain<'_, '_>,
        txn: &mut Txn,
        view: ReadView,
        edge: &Edge,
    ) -> Result<RowId, Error> {
        bicdb_txn::write::checkpoint_safe_point(self.pool, log, chain).map_err(error)?;
        Self::write_view(view, txn)?;
        let src = self
            .anchor(file, chain, view, edge.source)?
            .ok_or_else(|| error("missing graph source node"))?;
        let dst = self
            .anchor(file, chain, view, edge.target)?
            .ok_or_else(|| error("missing graph target node"))?;
        let key = codec::locator_key(edge.id)?;
        if !self
            .range(file, self.routes.locator, Some(&key), Some(&key))?
            .is_empty()
            || self.anchor(file, chain, view, edge.id)?.is_some()
        {
            return Err(error("graph identity already reserved in native routes"));
        }
        self.segment(file, self.routes.adjacency, SegType::Adjacency)?;
        let access = AdjacencyAccess::new(self.pool, self.ws);
        access
            .lock_endpoints(log, chain, txn, src.rid, dst.rid)
            .map_err(error)?;
        let previous = self.source_entry(file, chain, view, src)?;
        if previous.is_some_and(|p| edge.id <= p.last_id) {
            return Err(error("source edge identity order violated"));
        }
        let encoded = codec::encode(edge, src.rid, dst.rid, Placement::Automatic, &self.limits)?;
        self.seen_edges.insert(edge.id);
        if self.seen_edges.len() > self.limits.max_edges {
            return Err(error("native adjacency edge budget exceeded"));
        }
        self.work.bytes = self.work.bytes.saturating_add(encoded.record.len());
        for bytes in encoded.overflow.values() {
            self.work.bytes = self.work.bytes.saturating_add(bytes.len());
        }
        self.work.directory_entries = self.work.directory_entries.saturating_add(1);
        self.check()?;
        let limits = self.chain_limits();
        let mut visited = BTreeSet::new();
        let added = access
            .append_with(
                log,
                chain,
                txn,
                file,
                self.routes.adjacency.block,
                src.rid,
                previous.map(|p| p.head),
                &encoded.record,
                limits,
                |address, slots| {
                    if visited.insert(address) {
                        self.work.adjacency_pages = self.work.adjacency_pages.saturating_add(1);
                    }
                    self.work.directory_entries = self.work.directory_entries.saturating_add(slots);
                    self.work.check(&self.limits, self.deadline).map_err(|e| {
                        if e.0.contains("time budget") {
                            AccessError::Budget("graph statement time budget exceeded")
                        } else {
                            AccessError::Budget("native adjacency work/byte budget exceeded")
                        }
                    })
                },
            )
            .map_err(error)?;
        for (ordinal, bytes) in encoded.overflow {
            self.put(file, log, chain, txn, view, ordinal, &bytes)?;
        }
        let entry = SourceEntry {
            source: src.rid,
            head: added.head,
            tail: RowId::page_address(added.edge.file_id(), added.edge.block_id())
                .map_err(error)?,
            last_id: edge.id,
        };
        let metadata = self.put(
            file,
            log,
            chain,
            txn,
            view,
            codec::source_ordinal(src.id)?,
            &entry.encode()?,
        )?;
        self.index(
            file,
            log,
            txn,
            self.routes.source,
            &codec::source_key(src.rid)?,
            metadata,
        )?;
        self.index(file, log, txn, self.routes.locator, &key, added.edge)?;
        self.index(
            file,
            log,
            txn,
            self.routes.incoming,
            &codec::incoming_key(dst.rid, &edge.label, src.rid, edge.id)?,
            added.edge,
        )?;
        self.check()?;
        Ok(added.edge)
    }
    #[allow(clippy::too_many_arguments)]
    fn remove(
        &mut self,
        file: &mut DataFile<'_>,
        log: &mut GroupWriter<'_, '_>,
        chain: &mut UndoChain<'_, '_>,
        txn: &mut Txn,
        view: ReadView,
        ordinal: u64,
    ) -> Result<(), Error> {
        bicdb_txn::write::checkpoint_safe_point(self.pool, log, chain).map_err(error)?;
        if let Some(record) = self.record(file, chain, view, ordinal)? {
            self.work.heap_writes = self.work.heap_writes.saturating_add(1);
            self.check()?;
            bicdb_access::TableAccess::new(self.pool, self.ws)
                .delete(
                    log,
                    chain,
                    txn,
                    file,
                    record.location,
                    &InsertPolicy::in_place(0),
                )
                .map_err(error)?;
        }
        // Keep candidate index entries: old snapshots still need the old row.
        Ok(())
    }
    /// Update properties at the same directory ROWID. Inline growth that cannot
    /// fit switches to external chunks in this transaction; topology/type are
    /// immutable, as in the current Cypher SET model.
    #[allow(clippy::too_many_arguments)]
    pub fn update(
        &mut self,
        file: &mut DataFile<'_>,
        log: &mut GroupWriter<'_, '_>,
        chain: &mut UndoChain<'_, '_>,
        txn: &mut Txn,
        view: ReadView,
        edge: &Edge,
    ) -> Result<RowId, Error> {
        bicdb_txn::write::checkpoint_safe_point(self.pool, log, chain).map_err(error)?;
        Self::write_view(view, txn)?;
        let old = self
            .located(file, chain, view, edge.id)?
            .ok_or_else(|| error("missing graph edge"))?;
        if (old.edge.source, old.edge.target, &old.edge.label)
            != (edge.source, edge.target, &edge.label)
        {
            return Err(error("native edge topology/type cannot change"));
        }
        let destination = bicdb_storage::adjacency::decode(&old.bytes)
            .map_err(error)?
            .destination;
        let old_range = codec::overflow_range(edge.id, &old.bytes, &self.limits)?;
        let mut encoded = codec::encode(
            edge,
            old.source,
            destination,
            Placement::Automatic,
            &self.limits,
        )?;
        self.work.bytes = self.work.bytes.saturating_add(encoded.record.len());
        for bytes in encoded.overflow.values() {
            self.work.bytes = self.work.bytes.saturating_add(bytes.len());
        }
        self.check()?;
        let access = AdjacencyAccess::new(self.pool, self.ws);
        let result = access.update(
            log,
            chain,
            txn,
            file,
            self.routes.adjacency.block,
            old.source,
            old.rid,
            &encoded.record,
        );
        if let Err(failure) = result {
            if !matches!(
                failure,
                AccessError::Native(bicdb_access::TableAccessError::Txn(
                    bicdb_txn::write::TxnError::Adjacency(
                        bicdb_storage::adjacency::AdjacencyError::PageFull
                    )
                ))
            ) {
                return Err(error(failure));
            }
            encoded = codec::encode(
                edge,
                old.source,
                destination,
                Placement::External,
                &self.limits,
            )?;
            self.work.bytes = self.work.bytes.saturating_add(32);
            self.check()?;
            access
                .update(
                    log,
                    chain,
                    txn,
                    file,
                    self.routes.adjacency.block,
                    old.source,
                    old.rid,
                    &encoded.record,
                )
                .map_err(error)?;
        }
        for (&ordinal, bytes) in &encoded.overflow {
            self.put(file, log, chain, txn, view, ordinal, bytes)?;
        }
        if let Some((lower, upper)) = old_range {
            for ordinal in lower..=upper {
                if !encoded.overflow.contains_key(&ordinal) {
                    self.remove(file, log, chain, txn, view, ordinal)?;
                }
            }
        }
        self.check()?;
        Ok(old.rid)
    }
    /// Tombstone an edge and delete its current external property rows. Source,
    /// locator and reverse candidates remain for old snapshots; current reads
    /// recheck the CR edge and node anchors and never expose stale candidates.
    #[allow(clippy::too_many_arguments)]
    pub fn delete(
        &mut self,
        file: &mut DataFile<'_>,
        log: &mut GroupWriter<'_, '_>,
        chain: &mut UndoChain<'_, '_>,
        txn: &mut Txn,
        view: ReadView,
        id: u64,
    ) -> Result<(), Error> {
        bicdb_txn::write::checkpoint_safe_point(self.pool, log, chain).map_err(error)?;
        Self::write_view(view, txn)?;
        let old = self
            .located(file, chain, view, id)?
            .ok_or_else(|| error("missing graph edge"))?;
        let range = codec::overflow_range(id, &old.bytes, &self.limits)?;
        self.check()?;
        AdjacencyAccess::new(self.pool, self.ws)
            .delete(
                log,
                chain,
                txn,
                file,
                self.routes.adjacency.block,
                old.source,
                old.rid,
            )
            .map_err(error)?;
        if let Some((lower, upper)) = range {
            for ordinal in lower..=upper {
                self.remove(file, log, chain, txn, view, ordinal)?;
            }
        }
        self.check()
    }
}
