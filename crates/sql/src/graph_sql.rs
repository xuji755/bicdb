//! Native, workspace-local graph execution. Entity records use the existing heap,
//! undo and WAL paths; no remote database is contacted by this module.
use crate::ast::{
    GraphIndexAction, GraphIndexStmt, GraphTextPathPart, ObjectType, Stmt, TransactionStmtKind,
};
use crate::graph_adjacency::{GraphRecords, NativeAdjacency};
use crate::graph_manifest::{CorpusStats, Manifest};
use crate::session::{ColumnMeta, QueryResult, Session, SessionError};
use bicdb_catalog::{ddl, dict};
use bicdb_exec::{ColKind, Value};
use bicdb_graph::access::{adjacency_prefix, label_key, AccessEntries, GraphAccess};
use bicdb_graph::corpus_proof::{
    Change as CorpusProofChange, Contribution as CorpusContribution, EntityKey as CorpusEntityKey,
    Image as CorpusProofImage, Reader as CorpusProofReader, RecordKey as CorpusRecordKey,
    Root as CorpusRoot,
};
use bicdb_graph::fulltext::{
    self, Channel, Definition as TextDefinition, Generation, PathPart, SearchOptions, TextLimits,
};
use bicdb_graph::fulltext_journal::{Event, FixedTarget, Journal, JournalHead};
use bicdb_graph::property_index::{
    EntityKind, FulltextHit, FulltextIndex, FulltextRequest, IndexProvider, IndexRequest,
    PropertyIndex,
};
use bicdb_graph::{
    storage::{StorageImage, StoragePatch},
    Graph, GraphChanges, GraphCorpus, Limits, Value as GraphValue,
};
use bicdb_types::Number;
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

fn error(e: impl std::fmt::Display) -> SessionError {
    SessionError::State(format!("图查询：{e}"))
}
fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}
/// Portable authoritative graph entities, without workspace/object namespace or
/// derived indexes, postings, revisions, pending events and maintenance state.
#[derive(Debug, Clone)]
pub struct GraphSnapshot {
    /// Exact workspace-local graph name, including quoted case.
    pub name: String,
    /// Bounded UTF-8 logical graph image with entity IDs and allocator high water.
    pub data: Vec<u8>,
}
fn num_u64(n: u64) -> Value {
    Value::Number(Number::parse(&n.to_string()).expect("u64"))
}
fn text_columns(columns: &[(&str, ColKind)]) -> Vec<ColumnMeta> {
    columns
        .iter()
        .map(|(name, kind)| ColumnMeta {
            name: (*name).into(),
            kind: *kind,
        })
        .collect()
}
fn text_json_options(
    raw: &str,
    allowed: &[&str],
) -> Result<serde_json::Map<String, serde_json::Value>, SessionError> {
    if raw.len() > 16384 {
        return Err(error("full-text options exceed 16384 bytes"));
    }
    let json: serde_json::Value = serde_json::from_str(raw).map_err(error)?;
    let map = json
        .as_object()
        .ok_or_else(|| error("full-text OPTIONS must be a JSON object"))?;
    if let Some(key) = map.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(error(format!("unknown full-text option {key}")));
    }
    Ok(map.clone())
}

fn journal_limits() -> TextLimits {
    TextLimits {
        max_documents: 600_000,
        ..TextLimits::default()
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct TextPolicy {
    batch: bool,
    interval_ms: Option<u64>,
    batch_rows: Option<usize>,
}
impl TextPolicy {
    fn manual() -> Self {
        Self {
            batch: false,
            interval_ms: None,
            batch_rows: None,
        }
    }
    fn parse(raw: &str) -> Result<Self, SessionError> {
        let opts = text_json_options(raw, &["update", "interval_ms", "batch_rows"])?;
        let batch = match opts.get("update").and_then(serde_json::Value::as_str) {
            Some("manual") => false,
            Some("batch") => true,
            _ => return Err(error("explicitly select update=manual or update=batch")),
        };
        let integer = |key: &str, min: u64, max: u64| -> Result<Option<u64>, SessionError> {
            opts.get(key)
                .map(|v| {
                    v.as_u64()
                        .filter(|n| *n >= min && *n <= max)
                        .ok_or_else(|| error(format!("{key} must be an integer in {min}..={max}")))
                })
                .transpose()
        };
        let interval_ms = integer("interval_ms", 100, 86_400_000)?;
        if !batch && interval_ms.is_some() {
            return Err(error("interval_ms requires update=batch"));
        }
        Ok(Self {
            batch,
            interval_ms,
            batch_rows: integer("batch_rows", 1, 4096)?.map(|v| v as usize),
        })
    }
    fn options(&self) -> serde_json::Value {
        let mut value = serde_json::json!({"update":if self.batch {"batch"} else {"manual"}});
        if let Some(v) = self.interval_ms {
            value["interval_ms"] = v.into();
        }
        if let Some(v) = self.batch_rows {
            value["batch_rows"] = v.into();
        }
        value
    }
}

/// Workspace-local, single-writer scheduler retained across client connections.
/// It publishes at most one bounded batch per poll and never runs inside a user transaction.
pub struct FulltextScheduler {
    next_scan: Instant,
    due: BTreeMap<(u32, u32), (Instant, u64, usize)>,
    interval_ms: u64,
    batch_rows: usize,
}
impl Default for FulltextScheduler {
    fn default() -> Self {
        Self::new(5000, 256).expect("valid built-in full-text defaults")
    }
}
impl FulltextScheduler {
    /// Configure instance defaults. Index OPTIONS override these values.
    ///
    /// # Errors
    /// Reject intervals outside 100..=86400000 ms or batch sizes outside 1..=4096.
    pub fn new(interval_ms: u64, batch_rows: usize) -> Result<Self, SessionError> {
        if !(100..=86_400_000).contains(&interval_ms) || !(1..=4096).contains(&batch_rows) {
            return Err(error("invalid full-text maintenance defaults"));
        }
        Ok(Self {
            next_scan: Instant::now(),
            due: BTreeMap::new(),
            interval_ms,
            batch_rows,
        })
    }
}
fn text_source(
    definition: &TextDefinition,
    policy: Option<&TextPolicy>,
) -> Result<Vec<u8>, SessionError> {
    let bytes = definition.to_bytes().map_err(error)?;
    let Some(policy) = policy else {
        return Ok(bytes);
    };
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(error)?;
    let bytes = serde_json::to_vec(
        &serde_json::json!({"format":"bicdb-graph-fulltext-managed-v2","definition":value,"policy":policy.options()}),
    ).map_err(error)?;
    if bytes.len() > 4096 {
        return Err(error("full-text index descriptor exceeds 4096 bytes"));
    }
    Ok(bytes)
}
fn record_patch(before: &BTreeMap<u64, Vec<u8>>, after: &BTreeMap<u64, Vec<u8>>) -> StoragePatch {
    let mut patch = StoragePatch::default();
    for (key, data) in after {
        match before.get(key) {
            None => {
                patch.inserted.insert(*key, data.clone());
            }
            Some(old) if old != data => {
                patch.updated.insert(*key, data.clone());
            }
            _ => {}
        }
    }
    patch.removed = before
        .keys()
        .filter(|key| !after.contains_key(key))
        .copied()
        .collect();
    patch
}

#[derive(Clone)]
struct NativeGraphIndex {
    obj: u32,
    name: String,
    block: u32,
    definition: PropertyIndex,
    status: u32,
}
struct GraphIndexChanges {
    block: u32,
    added: Vec<(Vec<u8>, u64)>,
}
struct NativeFulltextIndex {
    managed: bool,
    policy: TextPolicy,
    source: Vec<u8>,
    obj: u32,
    name: String,
    definition: TextDefinition,
}
#[derive(Default)]
struct IndexAccess {
    access: &'static str,
    name: String,
    seeks: usize,
    entries: usize,
    candidates: usize,
    source_checks: usize,
    documents_loaded: usize,
    statistics_records_loaded: usize,
}
struct NativeIndexProvider<'a, 'io> {
    deadline: Option<bicdb_graph::Deadline>,
    catalog: &'a mut bicdb_catalog::Catalog<'io>,
    indexes: &'a [NativeGraphIndex],
    budget: usize,
    reads: usize,
    accesses: Vec<IndexAccess>,
}
impl IndexProvider for NativeIndexProvider<'_, '_> {
    fn candidates(
        &mut self,
        request: &IndexRequest,
    ) -> Result<Option<Vec<u64>>, bicdb_graph::Error> {
        if let Some(d) = self.deadline {
            d.check("before property candidate seek")?;
        }
        let best = self
            .indexes
            .iter()
            .filter(|i| i.status == 1)
            .filter_map(|i| i.definition.seek(request).map(|s| (i, s)))
            .max_by_key(|(_, s)| s.matched_fields);
        let Some((index, seek)) = best else {
            return Ok(None);
        };
        let mut ids = BTreeSet::new();
        let mut entries = 0;
        for range in seek.ranges {
            let remaining = self.budget.saturating_sub(self.reads);
            let rows = self
                .catalog
                .graph_index_range(
                    index.obj,
                    range.lower.as_deref(),
                    range.upper.as_deref(),
                    remaining.saturating_add(1),
                )
                .map_err(|e| bicdb_graph::Error(e.to_string()))?;
            if let Some(d) = self.deadline {
                d.check("after property candidate seek")?;
            }
            self.reads = self.reads.saturating_add(rows.len());
            entries += rows.len();
            if self.reads > self.budget {
                return Err(bicdb_graph::Error(
                    "graph index entry budget exceeded; narrow predicates or REBUILD the index"
                        .into(),
                ));
            }
            for (key, id) in rows {
                if range.contains(&key) {
                    let id = id.as_raw();
                    if id == 0 {
                        return Err(bicdb_graph::Error("invalid graph index element ID".into()));
                    }
                    ids.insert(id);
                }
            }
        }
        if let Some(a) = self.accesses.iter_mut().find(|a| a.name == index.name) {
            a.seeks += 1;
            a.entries += entries;
            a.candidates += ids.len();
        } else {
            self.accesses.push(IndexAccess {
                access: "PROPERTY_BTREE",
                name: index.name.clone(),
                seeks: 1,
                entries,
                candidates: ids.len(),
                source_checks: 0,
                documents_loaded: 0,
                statistics_records_loaded: 0,
            });
        }
        Ok(Some(ids.into_iter().collect()))
    }
}
#[derive(Clone)]
struct AccessTree {
    obj: u32,
    kind: u32,
    name: String,
    block: u32,
}
struct NativeGraphReader<'s, 'a, 'b, 'io, 'f> {
    session: &'s mut Session<'a, 'b, 'io, 'f>,
    indexes: &'s [NativeGraphIndex],
    trees: &'s [AccessTree],
    record: AccessTree,
    limits: &'s Limits,
    graph_scope: Option<(String, u32)>,
    graph_obj: u32,
    adjacency: Option<Manifest>,
    native_edges: BTreeMap<u64, bicdb_graph::Edge>,
    selected_nodes: BTreeMap<u64, Vec<u8>>,
    observed_nodes: BTreeMap<u64, Option<bicdb_graph::Node>>,
    observed_edges: BTreeMap<u64, Option<bicdb_graph::Edge>>,
    reads: usize,
    record_reads: usize,
    bytes: usize,
    accesses: Vec<IndexAccess>,
}
#[derive(Default)]
struct SourceObservations {
    node_rows: BTreeMap<u64, Vec<u8>>,
    nodes: BTreeMap<u64, Option<bicdb_graph::Node>>,
    edges: BTreeMap<u64, Option<bicdb_graph::Edge>>,
}
impl SourceObservations {
    fn take(reader: &mut NativeGraphReader<'_, '_, '_, '_, '_>) -> Self {
        Self {
            node_rows: reader.selected_nodes.clone(),
            nodes: std::mem::take(&mut reader.observed_nodes),
            edges: std::mem::take(&mut reader.observed_edges),
        }
    }
    fn merge(&mut self, mut other: Self) -> Result<(), SessionError> {
        for (id, value) in other.nodes {
            if self.nodes.get(&id).is_some_and(|old| old != &value) {
                return Err(error("node source changed during corpus proof planning"));
            }
            self.nodes.insert(id, value);
        }
        for (id, value) in other.edges {
            if self.edges.get(&id).is_some_and(|old| old != &value) {
                return Err(error(
                    "relationship source changed during corpus proof planning",
                ));
            }
            self.edges.insert(id, value);
        }
        for (ordinal, data) in std::mem::take(&mut other.node_rows) {
            if self
                .node_rows
                .insert(ordinal, data.clone())
                .is_some_and(|old| old != data)
            {
                return Err(error("node record changed during corpus proof planning"));
            }
        }
        Ok(())
    }
}
struct NativeWritePlan {
    manifest: Manifest,
    nodes: StoragePatch,
    removed: Vec<u64>,
    inserted: Vec<bicdb_graph::Edge>,
    updated: Vec<bicdb_graph::Edge>,
    proof: Option<StoragePatch>,
    header: Vec<u8>,
    source_header: Vec<u8>,
}
struct NativeProofPlan<'a> {
    limits: &'a Limits,
    deadline: bicdb_graph::Deadline,
    partial: Option<(CorpusRoot, StoragePatch)>,
}
impl IndexProvider for NativeGraphReader<'_, '_, '_, '_, '_> {
    fn fulltext_indexes(&mut self) -> Result<Vec<FulltextIndex>, bicdb_graph::Error> {
        let (_, obj) = self.graph_scope.as_ref().ok_or_else(|| {
            bicdb_graph::Error("missing graph scope for automatic full-text selection".into())
        })?;
        let (indexes, access, work) = self
            .session
            .cypher_fulltext_indexes(*obj, self.limits.max_expansions.saturating_sub(self.reads))
            .map_err(|e| bicdb_graph::Error(e.to_string()))?;
        self.reads = self.reads.saturating_add(work);
        merge_text_access(&mut self.accesses, access);
        Ok(indexes)
    }

    fn candidates(
        &mut self,
        request: &IndexRequest,
    ) -> Result<Option<Vec<u64>>, bicdb_graph::Error> {
        let mut provider = NativeIndexProvider {
            deadline: self.session.graph_deadline,
            catalog: self.session.catalog,
            indexes: self.indexes,
            budget: self.limits.max_expansions,
            reads: self.reads,
            accesses: std::mem::take(&mut self.accesses),
        };
        let result = provider.candidates(request);
        self.reads = provider.reads;
        self.accesses = provider.accesses;
        result
    }
    fn fulltext(
        &mut self,
        request: &FulltextRequest,
        staged: Option<&Graph>,
    ) -> Result<Vec<FulltextHit>, bicdb_graph::Error> {
        let (graph, obj) = self.graph_scope.clone().ok_or_else(|| {
            bicdb_graph::Error("missing graph scope for full-text request".into())
        })?;
        let complete = if let Some(staged) = staged {
            let (source, _) = self
                .session
                .load_graph(&graph, self.limits)
                .map_err(|e| bicdb_graph::Error(e.to_string()))?;
            Some(staged.overlay_source(&source)?)
        } else {
            None
        };
        let (hits, access, work) = self
            .session
            .cypher_fulltext(
                &graph,
                obj,
                request,
                complete.as_ref(),
                self.limits.max_expansions.saturating_sub(self.reads),
            )
            .map_err(|e| bicdb_graph::Error(e.to_string()))?;
        self.reads = self.reads.saturating_add(work);
        merge_text_access(&mut self.accesses, access);
        Ok(hits)
    }
}
struct NativeCypherProvider<'s, 'a, 'b, 'io, 'f> {
    session: &'s mut Session<'a, 'b, 'io, 'f>,
    indexes: &'s [NativeGraphIndex],
    graph: &'s str,
    obj: u32,
    budget: usize,
    reads: usize,
    accesses: Vec<IndexAccess>,
}
impl IndexProvider for NativeCypherProvider<'_, '_, '_, '_, '_> {
    fn fulltext_indexes(&mut self) -> Result<Vec<FulltextIndex>, bicdb_graph::Error> {
        let (indexes, access, work) = self
            .session
            .cypher_fulltext_indexes(self.obj, self.budget.saturating_sub(self.reads))
            .map_err(|e| bicdb_graph::Error(e.to_string()))?;
        self.reads = self.reads.saturating_add(work);
        merge_text_access(&mut self.accesses, access);
        Ok(indexes)
    }

    fn candidates(
        &mut self,
        request: &IndexRequest,
    ) -> Result<Option<Vec<u64>>, bicdb_graph::Error> {
        let mut provider = NativeIndexProvider {
            deadline: self.session.graph_deadline,
            catalog: self.session.catalog,
            indexes: self.indexes,
            budget: self.budget,
            reads: self.reads,
            accesses: std::mem::take(&mut self.accesses),
        };
        let result = provider.candidates(request);
        self.reads = provider.reads;
        self.accesses = provider.accesses;
        result
    }
    fn fulltext(
        &mut self,
        request: &FulltextRequest,
        staged: Option<&Graph>,
    ) -> Result<Vec<FulltextHit>, bicdb_graph::Error> {
        let (hits, access, work) = self
            .session
            .cypher_fulltext(
                self.graph,
                self.obj,
                request,
                staged,
                self.budget.saturating_sub(self.reads),
            )
            .map_err(|e| bicdb_graph::Error(e.to_string()))?;
        self.reads = self.reads.saturating_add(work);
        merge_text_access(&mut self.accesses, access);
        Ok(hits)
    }
}
fn merge_text_access(accesses: &mut Vec<IndexAccess>, access: IndexAccess) {
    if let Some(old) = accesses
        .iter_mut()
        .find(|a| a.name == access.name && a.access == access.access)
    {
        old.seeks += access.seeks;
        old.entries += access.entries;
        old.candidates += access.candidates;
        old.source_checks += access.source_checks;
        old.documents_loaded += access.documents_loaded;
        old.statistics_records_loaded += access.statistics_records_loaded;
    } else {
        accesses.push(access);
    }
}
struct NativeTextResult {
    result: QueryResult,
    mode: &'static str,
    work: usize,
    posting_entries: usize,
    source_checks: usize,
    documents_loaded: usize,
    statistics_records_loaded: usize,
}
impl NativeGraphReader<'_, '_, '_, '_, '_> {
    fn corpus_root(&mut self, manifest: Manifest) -> Result<CorpusRoot, bicdb_graph::Error> {
        let (lower, upper) = CorpusRecordKey::Root.range()?;
        let rows = self.records_bounded(lower, upper, 1024)?;
        let data = CorpusRecordKey::Root
            .from_native_rows(&rows, self.limits)?
            .ok_or_else(|| bicdb_graph::Error("missing independent corpus proof root".into()))?;
        let root = CorpusRoot::decode(&data, manifest.proof_scope(), self.limits)?;
        manifest.verify_proof(root, self.limits)?;
        Ok(root)
    }
    fn snapshot_cache(
        &mut self,
        header: Option<&[u8]>,
    ) -> Result<Option<Graph>, bicdb_graph::Error> {
        let manifest = Manifest::decode(
            header,
            self.session.ws,
            self.session.catalog.file_mut().file_id() as u32,
            self.graph_obj,
            self.limits,
        )?;
        if let Some(manifest) = manifest {
            manifest.validate_catalog(
                self.session.catalog,
                self.session.engine,
                bicdb_storage::cr::ReadView::new(self.session.snapshot())
                    .with_own(self.session.txn.as_ref().map(|txn| txn.id())),
                self.limits,
            )?;
            if manifest.records.primary.obj != self.record.obj {
                return Err(bicdb_graph::Error(
                    "adjacency manifest ordinal index mismatch".into(),
                ));
            }
            // Subsequent ordinal seeks use this snapshot's route, not whatever
            // segment a later catalog rebuild would select.
            self.record.block = manifest.records.primary.block;
            self.adjacency = Some(manifest);
            Ok(Some(manifest.cache(self.limits)?))
        } else {
            bicdb_graph::storage::snapshot_cache(header, self.limits)
        }
    }
    fn native_read(
        &mut self,
        id: u64,
        direction: Option<bicdb_graph::Direction>,
        types: &[String],
    ) -> Result<Vec<bicdb_graph::Edge>, bicdb_graph::Error> {
        let manifest = self.adjacency.expect("native reader authority");
        let view = bicdb_storage::cr::ReadView::new(self.session.snapshot())
            .with_own(self.session.txn.as_ref().map(|t| t.id()));
        let ws = self.session.ws;
        let mut limits = self.limits.clone();
        limits.max_expansions = limits.max_expansions.saturating_sub(self.reads);
        limits.max_text_bytes = limits.max_text_bytes.saturating_sub(self.bytes);
        let deadline = self
            .session
            .graph_deadline
            .unwrap_or_else(|| bicdb_graph::Deadline::new(limits.max_elapsed_ms));
        let file = self.session.catalog.file_mut();
        let (edges, work) = self.session.engine.with_read_context(|pool, chain| {
            let mut port = NativeAdjacency::new(
                pool,
                ws,
                manifest.records,
                manifest.routes,
                limits,
                deadline,
            )?;
            let mut edges = BTreeMap::new();
            match direction {
                None => {
                    if let Some((_, e)) = port.edge(file, chain, view, id)? {
                        edges.insert(e.id, e);
                    }
                }
                Some(dir) => {
                    if dir != bicdb_graph::Direction::In {
                        for (_, e) in port.outgoing(file, chain, view, id, types)? {
                            edges.insert(e.id, e);
                        }
                    }
                    if dir != bicdb_graph::Direction::Out {
                        for (_, e) in port.incoming(file, chain, view, id, types)? {
                            edges.insert(e.id, e);
                        }
                    }
                }
            }
            Ok::<_, bicdb_graph::Error>((edges.into_values().collect::<Vec<_>>(), port.work()))
        })?;
        self.reads = self.reads.saturating_add(work.units());
        self.bytes = self.bytes.saturating_add(work.bytes);
        for edge in &edges {
            self.native_edges.insert(edge.id, edge.clone());
        }
        let tree = AccessTree {
            obj: manifest.routes.adjacency.obj,
            kind: dict::index_kind::GRAPH_ADJACENCY,
            name: format!("i_graph_{}_adj$", manifest.routes.graph),
            block: manifest.routes.adjacency.block,
        };
        self.note(&tree, "ADJACENCY_NATIVE", work.index_entries, edges.len());
        Ok(edges)
    }
    fn note(&mut self, tree: &AccessTree, access: &'static str, entries: usize, candidates: usize) {
        if let Some(a) = self.accesses.iter_mut().find(|a| a.name == tree.name) {
            a.seeks += 1;
            a.entries += entries;
            a.candidates += candidates;
        } else {
            self.accesses.push(IndexAccess {
                access,
                name: tree.name.clone(),
                seeks: 1,
                entries,
                candidates,
                source_checks: 0,
                documents_loaded: 0,
                statistics_records_loaded: 0,
            });
        }
    }
    fn directory(
        &mut self,
        kind: u32,
        prefixes: &[Vec<u8>],
    ) -> Result<Vec<u64>, bicdb_graph::Error> {
        self.session
            .check_graph_deadline("before graph directory")
            .map_err(|e| bicdb_graph::Error(e.to_string()))?;
        let tree = self
            .trees
            .iter()
            .find(|t| t.kind == kind)
            .ok_or_else(|| bicdb_graph::Error("missing graph access tree".into()))?
            .clone();
        let mut ids = BTreeSet::new();
        let mut entries = 0;
        for prefix in prefixes {
            let upper = bicdb_graph::property_index::prefix_successor(prefix);
            let rows = self
                .session
                .catalog
                .graph_index_range(
                    tree.obj,
                    Some(prefix),
                    upper.as_deref(),
                    self.limits
                        .max_expansions
                        .saturating_sub(self.reads)
                        .saturating_add(1),
                )
                .map_err(|e| bicdb_graph::Error(e.to_string()))?;
            self.session
                .check_graph_deadline("after graph directory")
                .map_err(|e| bicdb_graph::Error(e.to_string()))?;
            self.reads = self.reads.saturating_add(rows.len());
            entries += rows.len();
            if self.reads > self.limits.max_expansions {
                return Err(bicdb_graph::Error("graph directory/adjacency entry budget exceeded; narrow the query or REBUILD ACCESS".into()));
            }
            for (key, id) in rows {
                if key.starts_with(prefix) {
                    let id = id.as_raw();
                    if id == 0 {
                        return Err(bicdb_graph::Error("invalid access-tree element ID".into()));
                    }
                    ids.insert(id);
                }
            }
        }
        self.note(
            &tree,
            if kind == dict::index_kind::GRAPH_NODES {
                "NODE_BTREE"
            } else {
                "ADJACENCY_BTREE"
            },
            entries,
            ids.len(),
        );
        Ok(ids.into_iter().collect())
    }
    fn records(
        &mut self,
        lower: u64,
        upper: u64,
    ) -> Result<BTreeMap<u64, Vec<u8>>, bicdb_graph::Error> {
        self.records_bounded(lower, upper, 1024)
    }
    fn records_bounded(
        &mut self,
        lower: u64,
        upper: u64,
        max_entries: usize,
    ) -> Result<BTreeMap<u64, Vec<u8>>, bicdb_graph::Error> {
        if let Some(manifest) = self.adjacency {
            let view = bicdb_storage::cr::ReadView::new(self.session.snapshot())
                .with_own(self.session.txn.as_ref().map(|t| t.id()));
            let mut limits = self.limits.clone();
            limits.max_expansions = limits.max_expansions.saturating_sub(self.reads);
            limits.max_text_bytes = limits.max_text_bytes.saturating_sub(self.bytes);
            let deadline = self
                .session
                .graph_deadline
                .unwrap_or_else(|| bicdb_graph::Deadline::new(limits.max_elapsed_ms));
            let ws = self.session.ws;
            let file = self.session.catalog.file_mut();
            let (rows, work) = self.session.engine.with_read_context(|pool, chain| {
                let mut port = NativeAdjacency::new(
                    pool,
                    ws,
                    manifest.records,
                    manifest.routes,
                    limits,
                    deadline,
                )?;
                let rows = port.records_range(file, chain, view, lower, upper, max_entries)?;
                Ok::<_, bicdb_graph::Error>((rows, port.work()))
            })?;
            self.reads = self.reads.saturating_add(work.units());
            self.record_reads = self.record_reads.saturating_add(work.index_entries);
            self.bytes = self.bytes.saturating_add(work.bytes);
            self.note(
                &self.record.clone(),
                "RECORD_BTREE",
                work.index_entries,
                rows.len(),
            );
            return Ok(rows);
        }
        self.session
            .check_graph_deadline("before native records")
            .map_err(|e| bicdb_graph::Error(e.to_string()))?;
        let shape = bicdb_exec::RowShape::new(vec![ColKind::Number, ColKind::Bytes]);
        let key = |ordinal: u64| -> Result<Vec<u8>, bicdb_graph::Error> {
            let row = bicdb_exec::Row::new(vec![
                Value::Number(Number::parse(&ordinal.to_string()).expect("u64")),
                Value::Bytes(vec![]),
            ]);
            let bytes = bicdb_exec::encode_row(&row, &shape)
                .map_err(|e| bicdb_graph::Error(e.to_string()))?;
            bicdb_catalog::row::key_from_row(&bytes, &[0])
                .map_err(|e| bicdb_graph::Error(e.to_string()))
        };
        let rows = self
            .session
            .catalog
            .graph_index_range(
                self.record.obj,
                Some(&key(lower)?),
                Some(&key(upper)?),
                max_entries.saturating_add(1),
            )
            .map_err(|e| bicdb_graph::Error(e.to_string()))?;
        self.session
            .check_graph_deadline("after record seek")
            .map_err(|e| bicdb_graph::Error(e.to_string()))?;
        self.record_reads = self.record_reads.saturating_add(rows.len());
        if rows.len() > max_entries
            || self.record_reads > self.limits.max_expansions.saturating_mul(16)
        {
            return Err(bicdb_graph::Error(
                "graph record index entry budget exceeded".into(),
            ));
        }
        let snapshot = self.session.snapshot();
        let view = bicdb_storage::cr::ReadView::new(snapshot)
            .with_own(self.session.txn.as_ref().map(|t| t.id()));
        let rids: Vec<_> = rows.iter().map(|(_, rid)| *rid).collect();
        let visible = self
            .session
            .engine
            .with_read_context(|pool, chain| {
                bicdb_storage::scan::fetch_rows_resolved(pool, chain, view, &rids)
            })
            .map_err(|e| bicdb_graph::Error(e.to_string()))?;
        self.session
            .check_graph_deadline("after record fetch")
            .map_err(|e| bicdb_graph::Error(e.to_string()))?;
        let mut records = BTreeMap::new();
        let mut physical = BTreeSet::new();
        for (rid, bytes) in visible.into_iter().flatten() {
            self.session
                .check_graph_deadline("record decoding")
                .map_err(|e| bicdb_graph::Error(e.to_string()))?;
            if !physical.insert(rid.as_raw()) {
                continue;
            }
            let row = bicdb_exec::decode_row(&bytes, &shape)
                .map_err(|e| bicdb_graph::Error(e.to_string()))?;
            let [Value::Number(ordinal), Value::Bytes(data)] = row.values.as_slice() else {
                return Err(bicdb_graph::Error("invalid native graph row shape".into()));
            };
            let ordinal: u64 = ordinal
                .to_string()
                .parse()
                .map_err(|_| bicdb_graph::Error("invalid graph ordinal".into()))?;
            if ordinal < lower || ordinal > upper {
                continue;
            }
            self.bytes = self.bytes.saturating_add(data.len());
            if self.bytes > self.limits.max_text_bytes
                || records.insert(ordinal, data.clone()).is_some()
            {
                return Err(bicdb_graph::Error(
                    "graph read byte budget exceeded or duplicate ordinal".into(),
                ));
            }
        }
        self.note(
            &self.record.clone(),
            "RECORD_BTREE",
            rows.len(),
            records.len(),
        );
        Ok(records)
    }
}
impl GraphAccess for NativeGraphReader<'_, '_, '_, '_, '_> {
    fn node(&mut self, id: u64) -> Result<Option<bicdb_graph::Node>, bicdb_graph::Error> {
        let (lo, hi) = bicdb_graph::storage::entity_range(1, id)?;
        let rows = self.records(lo, hi)?;
        let node = bicdb_graph::storage::decode_node(id, &rows, self.limits)?;
        self.selected_nodes.extend(rows);
        if self
            .observed_nodes
            .insert(id, node.clone())
            .is_some_and(|old| old != node)
        {
            return Err(bicdb_graph::Error(
                "node source changed during statement".into(),
            ));
        }
        Ok(node)
    }
    fn edge(&mut self, id: u64) -> Result<Option<bicdb_graph::Edge>, bicdb_graph::Error> {
        if self.adjacency.is_some() {
            let edge = if let Some(edge) = self.native_edges.get(&id) {
                Some(edge.clone())
            } else {
                self.native_read(id, None, &[])?.pop()
            };
            if self
                .observed_edges
                .insert(id, edge.clone())
                .is_some_and(|old| old != edge)
            {
                return Err(bicdb_graph::Error(
                    "relationship source changed during statement".into(),
                ));
            }
            return Ok(edge);
        }
        let (lo, hi) = bicdb_graph::storage::entity_range(2, id)?;
        bicdb_graph::storage::decode_edge(id, &self.records(lo, hi)?, self.limits)
    }
    fn node_ids(&mut self, labels: &[String]) -> Result<Vec<u64>, bicdb_graph::Error> {
        self.directory(
            dict::index_kind::GRAPH_NODES,
            &[label_key(labels.first().map(String::as_str))],
        )
    }
    fn edge_ids(
        &mut self,
        node: u64,
        direction: bicdb_graph::Direction,
        types: &[String],
    ) -> Result<Vec<u64>, bicdb_graph::Error> {
        if self.adjacency.is_some() {
            return Ok(self
                .native_read(node, Some(direction), types)?
                .into_iter()
                .map(|e| e.id)
                .collect());
        }
        let prefixes = if types.is_empty() {
            vec![adjacency_prefix(node, None)]
        } else {
            types
                .iter()
                .map(|t| adjacency_prefix(node, Some(t)))
                .collect()
        };
        let mut ids = BTreeSet::new();
        if direction != bicdb_graph::Direction::In {
            ids.extend(self.directory(dict::index_kind::GRAPH_OUT, &prefixes)?);
        }
        if direction != bicdb_graph::Direction::Out {
            ids.extend(self.directory(dict::index_kind::GRAPH_IN, &prefixes)?);
        }
        Ok(ids.into_iter().collect())
    }
}
fn access_builds(entries: &AccessEntries) -> [ddl::GraphAccessBuild<'_>; 3] {
    [
        ddl::GraphAccessBuild {
            kind: dict::index_kind::GRAPH_NODES,
            entries: &entries.nodes,
        },
        ddl::GraphAccessBuild {
            kind: dict::index_kind::GRAPH_OUT,
            entries: &entries.outgoing,
        },
        ddl::GraphAccessBuild {
            kind: dict::index_kind::GRAPH_IN,
            entries: &entries.incoming,
        },
    ]
}

fn number(value: usize) -> Value {
    Value::Number(Number::parse(&value.to_string()).expect("usize"))
}

impl Session<'_, '_, '_, '_> {
    pub(crate) fn check_graph_deadline(&self, phase: &str) -> Result<(), SessionError> {
        if let Some(deadline) = self.graph_deadline {
            deadline.check(phase).map_err(error)?;
        }
        Ok(())
    }
    pub(crate) fn with_graph_deadline<T>(
        &mut self,
        milliseconds: usize,
        action: impl FnOnce(&mut Self) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        let previous = self.graph_deadline;
        self.graph_deadline = Some(previous.map_or_else(
            || bicdb_graph::Deadline::new(milliseconds),
            |d| d.tighten(milliseconds),
        ));
        let previous_ddl = self
            .catalog
            .replace_ddl_deadline(self.graph_deadline.and_then(|d| d.expires_at()));
        let result = (|| {
            self.check_graph_deadline("statement entry")?;
            action(self)
        })();
        self.graph_deadline = previous;
        self.catalog.replace_ddl_deadline(previous_ddl);
        result
    }
    pub(crate) fn execute_graph_statement(
        &mut self,
        stmt: &Stmt,
    ) -> Option<Result<QueryResult, SessionError>> {
        let graph = matches!(
            stmt,
            Stmt::CreateGraph(_) | Stmt::ShowGraphs(_) | Stmt::GraphIndex(_) | Stmt::Cypher(_)
        ) || matches!(stmt, Stmt::Drop(d) if d.remove_type == ObjectType::Graph);
        if !graph {
            return None;
        }
        Some(
            self.with_graph_deadline(self.graph_limits.max_elapsed_ms, |s| {
                s.execute_graph_statement_scoped(stmt)
                    .expect("graph statement")
            }),
        )
    }

    fn ensure_recovery_object_accessible(&self, object_id: u32) -> Result<(), SessionError> {
        if let Some(reason) = self.pool.object_fault(self.ws, u64::from(object_id)) {
            return Err(error(format!(
                "对象 {object_id} 已由恢复管理隔离：{reason}"
            )));
        }
        Ok(())
    }
    fn execute_graph_statement_scoped(
        &mut self,
        stmt: &Stmt,
    ) -> Option<Result<QueryResult, SessionError>> {
        match stmt {
            Stmt::CreateGraph(g) => Some((|| {
                if self.in_transaction() {
                    return Err(error("活动事务中不能创建图"));
                }
                crate::bind::check_new_object_name(&g.graph.relname)?;
                let limits = self.graph_limits.clone();
                let deadline = self.graph_deadline.expect("graph create clock");
                let ws = self.ws;
                let seq = bicdb_common::seq::CommitSeq::from_raw(self.engine.current_seq())
                    .expect("engine sequence");
                ddl::create_graph_with_physical_routes(
                    self.catalog,
                    self.engine,
                    &g.graph.relname,
                    |heap, primary, routes, cat, pool, log, chain, txn| {
                        let records = GraphRecords { heap, primary };
                        let file = cat.file_mut();
                        let (graph, image) = StorageImage::empty_adjacency(&limits)
                            .map_err(|e| ddl::DdlError::BadIndexDef(e.to_string()))?;
                        let stats = CorpusStats::measure(
                            &graph,
                            image
                                .native_record_bytes(&graph, &limits)
                                .map_err(|e| ddl::DdlError::BadIndexDef(e.to_string()))?,
                            1,
                            &limits,
                        )
                        .map_err(|e| ddl::DdlError::BadIndexDef(e.to_string()))?;
                        let mut manifest = Manifest::from_graph(
                            ws,
                            file.file_id() as u32,
                            records,
                            *routes,
                            &graph,
                            stats,
                        );
                        let proof = CorpusProofImage::build(
                            manifest.proof_scope(),
                            graph.allocator_high_water(),
                            stats.generation,
                            image
                                .corpus_contributions(&graph, &limits)
                                .map_err(|e| ddl::DdlError::BadIndexDef(e.to_string()))?,
                            &limits,
                            deadline,
                        )
                        .map_err(|e| ddl::DdlError::BadIndexDef(e.to_string()))?;
                        manifest
                            .install_proof(proof.root, &limits)
                            .map_err(|e| ddl::DdlError::BadIndexDef(e.to_string()))?;
                        let mut patch = proof
                            .native_patch(&limits)
                            .map_err(|e| ddl::DdlError::BadIndexDef(e.to_string()))?;
                        patch.inserted.insert(
                            0,
                            manifest
                                .encode(&limits)
                                .map_err(|e| ddl::DdlError::BadIndexDef(e.to_string()))?,
                        );
                        let view = bicdb_storage::cr::ReadView::new(seq).with_own(Some(txn.txn_id));
                        NativeAdjacency::new(pool, ws, records, *routes, limits, deadline)
                            .and_then(|mut port| {
                                port.apply_records(file, log, chain, txn, view, &patch)
                            })
                            .map_err(|e| ddl::DdlError::BadIndexDef(e.to_string()))
                    },
                )?;
                self.seq = self.catalog.current_seq();
                Ok(QueryResult::Ddl(format!(
                    "CREATE GRAPH {}",
                    g.graph.relname
                )))
            })()),
            Stmt::Drop(d) if d.remove_type == ObjectType::Graph => Some((|| {
                if self.in_transaction() {
                    return Err(error("活动事务中不能删除图"));
                }
                if d.missing_ok {
                    return Err(error("DROP GRAPH IF EXISTS 尚未支持"));
                }
                if d.objects.len() != 1 {
                    return Err(error("DROP GRAPH 一次只支持一个图"));
                }
                for object in &d.objects {
                    ddl::drop_graph(self.catalog, self.engine, &object.relname)?;
                }
                self.fulltext_cache = None;
                self.seq = self.catalog.current_seq();
                Ok(QueryResult::Ddl("DROP GRAPH".into()))
            })()),
            Stmt::ShowGraphs(_) => Some((|| {
                let graphs = self.catalog.graphs().map_err(error)?;
                Ok(QueryResult::Rows {
                    columns: vec![
                        ColumnMeta {
                            name: "graph_name".into(),
                            kind: ColKind::Bytes,
                        },
                        ColumnMeta {
                            name: "object_id".into(),
                            kind: ColKind::Number,
                        },
                    ],
                    rows: graphs
                        .into_iter()
                        .map(|g| {
                            vec![
                                Value::Bytes(g.name.into_bytes()),
                                Value::Number(Number::parse(&g.obj.to_string()).expect("u32")),
                            ]
                        })
                        .collect(),
                })
            })()),
            Stmt::GraphIndex(g) => Some(self.graph_index(g)),
            Stmt::Cypher(c) => Some(self.cypher(c)),
            _ => None,
        }
    }
    /// Export empty graph definitions for initialization templates, never rows,
    /// allocators, object IDs, postings or pending source events.
    pub fn empty_graph_initialization_sql(&mut self) -> Result<String, SessionError> {
        self.graph_initialization_snapshot(false)
            .map(|(sql, _)| sql)
    }
    /// Export committed definitions, optionally with logical graph snapshots.
    /// The trusted provisioner retains exclusive ownership of the source instance.
    pub fn graph_initialization_snapshot(
        &mut self,
        include_data: bool,
    ) -> Result<(String, Vec<GraphSnapshot>), SessionError> {
        if self.in_transaction() {
            return Err(error(
                "graph schema export requires an idle committed workspace",
            ));
        }
        if !self.is_management_identity() {
            return Err(error("graph snapshot export requires management identity"));
        }
        let mut script = String::new();
        let mut snapshots = Vec::new();
        let mut data_bytes = 0usize;
        let append = |script: &mut String, statement: String| -> Result<(), SessionError> {
            if script.len().saturating_add(statement.len()) > 1024 * 1024 {
                return Err(error("graph initialization schema exceeds 1 MiB"));
            }
            script.push_str(&statement);
            Ok(())
        };
        for graph in self.catalog.graphs().map_err(error)? {
            // Complete validation includes edges, even if a lazy count of nodes
            // would find nothing. Deleted IDs and historical index rows are not copied.
            let (mut image, _) = self.load_graph(&graph.name, &self.graph_limits.clone())?;
            if !include_data && (!image.nodes().is_empty() || !image.edges().is_empty()) {
                return Err(error(format!(
                    "初始化模板不允许业务数据：图 `{}` 非空",
                    graph.name
                )));
            }
            if include_data {
                if let Some(next) = ddl::graph_allocator_next(self.catalog, graph.obj)? {
                    if next < image.allocator_high_water() {
                        return Err(error(
                            "source graph sequence is behind its stored allocator",
                        ));
                    }
                    // Only the logical image changes; never publish to the source.
                    image.reserve_ids(next, next).map_err(error)?;
                }
                let data = image.to_bytes().map_err(error)?;
                data_bytes = data_bytes.saturating_add(data.len());
                if data_bytes > self.graph_limits.max_text_bytes {
                    return Err(error(
                        "graph template snapshots exceed shared 100 MiB budget",
                    ));
                }
                snapshots.push(GraphSnapshot {
                    name: graph.name.clone(),
                    data,
                });
            }
            self.access_trees(graph.obj)?;
            // Physical routes are derived workspace objects, not template
            // definitions. Validate inactive route sets too; future v3 snapshot
            // export must still decode its authoritative graph before this point.
            ddl::graph_physical_routes(self.catalog, &graph.name)?;
            for index in self
                .catalog
                .indexes_of(self.snapshot(), graph.obj)
                .map_err(error)?
            {
                if ![
                    dict::index_kind::BTREE,
                    dict::index_kind::GRAPH_PROPERTY,
                    dict::index_kind::GRAPH_FULLTEXT,
                ]
                .contains(&index.kind)
                    && ddl::graph_access_descriptor(index.kind).is_none()
                    && ddl::graph_physical_descriptor(index.kind).is_none()
                {
                    return Err(error("unsupported graph index in initialization schema"));
                }
            }
            let name = quoted(&graph.name);
            append(&mut script, format!("CREATE GRAPH {name};\n"))?;
            let mut properties = self.property_indexes(graph.obj)?;
            properties.sort_by(|a, b| a.name.cmp(&b.name));
            for index in properties {
                if index.status != 1 {
                    return Err(error("invalid property index cannot be exported as ready"));
                }
                if include_data {
                    index.definition.entries(&image).map_err(error)?;
                }
                let d = index.definition;
                let entity = if d.entity == EntityKind::Node {
                    "NODES"
                } else {
                    "RELATIONSHIPS"
                };
                let marker = if d.entity == EntityKind::Node {
                    "LABEL"
                } else {
                    "TYPE"
                };
                let label = d
                    .label
                    .as_ref()
                    .map_or(String::new(), |l| format!(" {marker} {}", quoted(l)));
                let fields = d
                    .fields
                    .iter()
                    .map(|path| {
                        path.iter()
                            .map(|part| quoted(part))
                            .collect::<Vec<_>>()
                            .join(".")
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                append(
                    &mut script,
                    format!(
                        "CREATE {}GRAPH INDEX {} ON {name} {entity}{label} ({fields});\n",
                        if d.unique { "UNIQUE " } else { "" },
                        quoted(&index.name)
                    ),
                )?;
            }
            let mut fulltext = self.fulltext_indexes(graph.obj)?;
            let head = self.checked_journal_head(graph.obj, &fulltext)?;
            fulltext.sort_by(|a, b| a.name.cmp(&b.name));
            for index in fulltext {
                let d = &index.definition;
                let entity = if d.entity == EntityKind::Node {
                    "NODES"
                } else {
                    "RELATIONSHIPS"
                };
                let marker = if d.entity == EntityKind::Node {
                    "LABEL"
                } else {
                    "TYPE"
                };
                let labels = if d.labels.is_empty() {
                    String::new()
                } else {
                    format!(
                        " {marker} ({})",
                        d.labels
                            .iter()
                            .map(|label| quoted(label))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                let fields = d
                    .fields
                    .iter()
                    .map(|path| {
                        let mut field = String::new();
                        for part in path {
                            match part {
                                PathPart::Key(key) => {
                                    if !field.is_empty() {
                                        field.push('.');
                                    }
                                    field.push_str(&quoted(key));
                                }
                                PathPart::Index(i) => field.push_str(&format!("[{i}]")),
                            }
                        }
                        field
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let options = index.policy.options().to_string().replace('\'', "''");
                append(
                    &mut script,
                    format!(
                        "CREATE FULLTEXT GRAPH INDEX {} ON {name} {entity}{labels} ({fields}) OPTIONS '{options}';\n",
                        quoted(&index.name)
                    ),
                )?;
                if index.managed
                    && head.as_ref().expect("checked managed journal").consumers()[&index.obj]
                        .paused
                {
                    append(
                        &mut script,
                        format!(
                            "ALTER FULLTEXT GRAPH INDEX {} ON {name} PAUSE;\n",
                            quoted(&index.name)
                        ),
                    )?;
                }
            }
        }
        Ok((script, snapshots))
    }

    /// Validate untrusted template bytes before any target files are created.
    pub fn validate_graph_snapshot(data: &[u8]) -> Result<(), SessionError> {
        if data.is_empty() {
            return Err(error("empty graph snapshot file"));
        }
        let graph = Graph::from_bytes(data, &Limits::default()).map_err(error)?;
        if graph.allocator_high_water() >= 1 << 48 {
            return Err(error(
                "native snapshot allocator exceeds reservable 48-bit space",
            ));
        }
        let limits = Limits::default();
        let (empty, image) = StorageImage::empty_adjacency(&limits).map_err(error)?;
        image.patch(&empty, &graph, &limits).map_err(error)?;
        let entries = AccessEntries::from_nodes(&graph, limits.max_text_bytes).map_err(error)?;
        let bytes = access_builds(&entries).iter().fold(0usize, |total, build| {
            build.entries.iter().fold(total, |size, (key, _)| {
                size.saturating_add(key.len()).saturating_add(32)
            })
        });
        if bytes > limits.max_text_bytes {
            return Err(error("snapshot access planning budget exceeded"));
        }
        Ok(())
    }

    /// Restore authoritative entities into a newly created empty graph before
    /// user indexes are built. Normal native heap/access-tree publication is atomic.
    /// This trusted API is deliberately not exposed as arbitrary SQL/Cypher DML.
    pub fn restore_graph_snapshot(&mut self, snapshot: &GraphSnapshot) -> Result<(), SessionError> {
        if self.graph_deadline.is_none() {
            return self.with_graph_deadline(self.graph_limits.max_elapsed_ms, |s| {
                s.restore_graph_snapshot(snapshot)
            });
        }
        if self.in_transaction() || !self.is_management_identity() {
            return Err(error(
                "graph snapshot restore requires idle management identity",
            ));
        }
        Self::validate_graph_snapshot(&snapshot.data)?;
        let limits = self.graph_limits.clone();
        let graph = Graph::from_bytes(&snapshot.data, &limits).map_err(error)?;
        let obj = self
            .catalog
            .resolve(self.snapshot(), dict::namespace::TABLE, &snapshot.name)
            .map_err(error)?;
        self.ensure_recovery_object_accessible(obj.obj)?;
        if obj.type_code != dict::obj_kind::GRAPH || obj.status != 1 {
            return Err(error("snapshot target is not a ready graph"));
        }
        if !self.property_indexes(obj.obj)?.is_empty()
            || !self.fulltext_indexes(obj.obj)?.is_empty()
        {
            return Err(error(
                "restore graph snapshot before creating user graph indexes",
            ));
        }
        let (before, _) = self.load_graph(&snapshot.name, &limits)?;
        if !before.nodes().is_empty()
            || !before.edges().is_empty()
            || before.allocator_high_water() != 1
        {
            return Err(error(
                "graph snapshot target must be newly created and empty",
            ));
        }
        if !ddl::graph_ids_are_fresh(self.catalog, obj.obj)? {
            return Err(error("graph snapshot allocator is not fresh"));
        }
        // Snapshot data is logical and portable; restore it directly into the
        // current physical layout, including long relationship types.
        self.upgrade_graph_storage(&snapshot.name)?;
        let (before, image) = self.load_graph(&snapshot.name, &limits)?;
        let (trees, record) = self.access_trees(obj.obj)?;
        if trees.len() != 3 || record.is_none() {
            return Err(error("snapshot target lacks ready native access trees"));
        }
        let patch = image.patch(&before, &graph, &limits).map_err(error)?;
        let entries = AccessEntries::from_nodes(&graph, limits.max_text_bytes).map_err(error)?;
        let mut changes = Vec::new();
        let mut bytes = 0usize;
        for build in access_builds(&entries) {
            let tree = trees
                .iter()
                .find(|t| t.kind == build.kind)
                .ok_or_else(|| error("snapshot target access tree missing"))?;
            bytes = build.entries.iter().fold(bytes, |total, (key, _)| {
                total.saturating_add(key.len()).saturating_add(32)
            });
            if bytes > limits.max_text_bytes {
                return Err(error("snapshot access planning budget exceeded"));
            }
            changes.push(GraphIndexChanges {
                block: tree.block,
                added: build.entries.to_vec(),
            });
        }
        // Imported IDs and allocator gaps cannot be reused by target writes.
        let reserved = graph.allocator_high_water() - 1;
        if reserved != 0 {
            ddl::reserve_initial_graph_ids(self.catalog, self.engine, obj.obj, reserved)?;
            self.seq = self.catalog.current_seq();
        }
        let manifest = self
            .graph_manifest(&snapshot.name, &limits)?
            .ok_or_else(|| error("snapshot target lacks native graph authority"))?;
        let plan = Self::plan_native_write(
            manifest,
            &patch,
            &before,
            &graph,
            &image,
            &limits,
            self.graph_deadline
                .unwrap_or_else(|| bicdb_graph::Deadline::for_limits(&limits)),
        )?;
        self.persist_planned_record_patches(&[(&snapshot.name, &patch)], &changes, vec![Some(plan)])
    }

    fn access_trees(
        &mut self,
        obj: u32,
    ) -> Result<(Vec<AccessTree>, Option<AccessTree>), SessionError> {
        let snapshot = self.snapshot();
        let mut trees = vec![];
        let mut record = None;
        let mut kinds = BTreeSet::new();
        for index in self.catalog.indexes_of(snapshot, obj).map_err(error)? {
            let storage = index.kind == dict::index_kind::BTREE
                && index.is_unique
                && index.cols.len() == 1
                && index.cols[0].col == 1;
            let access = ddl::graph_access_descriptor(index.kind);
            if !storage && access.is_none() {
                continue;
            }
            if index.status != 1
                || index.cols.len() != 1
                || index.cols[0].col != if storage { 1 } else { 0 }
                || (access.is_some()
                    && (index.is_unique
                        || index.expr_src.as_deref() != access
                        || !kinds.insert(index.kind)))
            {
                return Err(error("invalid graph access index metadata"));
            }
            let name = self
                .catalog
                .resolve_by_obj(snapshot, index.obj)
                .map_err(error)?
                .name;
            let tree = AccessTree {
                obj: index.obj,
                kind: index.kind,
                name,
                block: ddl::live_segment_block(self.catalog, index.obj)?,
            };
            if storage {
                if record.replace(tree).is_some() {
                    return Err(error("duplicate graph storage index"));
                }
            } else {
                trees.push(tree);
            }
        }
        Ok((trees, record))
    }
    fn unique_graph_candidates(
        &mut self,
        index: &NativeGraphIndex,
        key: &[u8],
        limits: &Limits,
        work: &mut usize,
    ) -> Result<Vec<u64>, SessionError> {
        if index.status != 1 || !index.definition.unique {
            return Err(error("unique graph proof requires a ready unique index"));
        }
        self.check_graph_deadline("before unique graph candidate seek")?;
        *work = work.saturating_add(1);
        if *work > limits.max_expansions {
            return Err(error("unique graph index proof work budget exceeded"));
        }
        let remaining = limits.max_expansions.saturating_sub(*work);
        let byte_rows = limits.max_text_bytes / key.len().saturating_add(32);
        let row_limit = remaining.min(byte_rows);
        // One extra row distinguishes a complete seek from budget truncation.
        let rows = self
            .catalog
            .graph_index_range(index.obj, Some(key), Some(key), row_limit.saturating_add(1))
            .map_err(error)?;
        self.check_graph_deadline("after unique graph candidate seek")?;
        *work = work.saturating_add(rows.len());
        if *work > limits.max_expansions {
            return Err(error(
                "unique graph index proof work budget exceeded; narrow the write or REBUILD the index",
            ));
        }
        if rows.len() > byte_rows {
            return Err(error("unique graph index proof byte budget exceeded"));
        }
        rows.into_iter()
            .map(|(stored, id)| {
                if stored != key {
                    return Err(error("malformed exact unique graph candidate key"));
                }
                Ok(id.as_raw())
            })
            .collect()
    }
    fn property_indexes(&mut self, obj: u32) -> Result<Vec<NativeGraphIndex>, SessionError> {
        let snapshot = self.snapshot();
        let mut out = vec![];
        for index in self.catalog.indexes_of(snapshot, obj).map_err(error)? {
            if index.kind != dict::index_kind::GRAPH_PROPERTY {
                continue;
            }
            let definition = PropertyIndex::from_bytes(
                index
                    .expr_src
                    .as_deref()
                    .ok_or_else(|| error("missing graph index descriptor"))?,
            )
            .map_err(error)?;
            if index.is_unique != definition.unique
                || index.cols.len() != 1
                || index.cols[0].col != 0
            {
                return Err(error("inconsistent graph index metadata"));
            }
            let name = self
                .catalog
                .resolve_by_obj(snapshot, index.obj)
                .map_err(error)?
                .name;
            let block = ddl::live_segment_block(self.catalog, index.obj)?;
            out.push(NativeGraphIndex {
                obj: index.obj,
                name,
                block,
                definition,
                status: index.status,
            });
        }
        Ok(out)
    }
    fn graph_index(&mut self, stmt: &GraphIndexStmt) -> Result<QueryResult, SessionError> {
        let snapshot = self.snapshot();
        let graph = self
            .catalog
            .resolve(snapshot, dict::namespace::TABLE, &stmt.graph)
            .map_err(error)?;
        self.ensure_recovery_object_accessible(graph.obj)?;
        if graph.type_code != dict::obj_kind::GRAPH || graph.status != 1 {
            return Err(error("object is not a native graph"));
        }
        if matches!(stmt.action, GraphIndexAction::UpgradeStorage) {
            return self.upgrade_graph_storage(&stmt.graph);
        }
        if matches!(stmt.action, GraphIndexAction::RebuildStorage) {
            return self.rebuild_graph_storage(&stmt.graph);
        }
        if matches!(
            stmt.action,
            GraphIndexAction::FulltextCreate { .. }
                | GraphIndexAction::FulltextShow
                | GraphIndexAction::FulltextSearch { .. }
                | GraphIndexAction::FulltextSync
                | GraphIndexAction::FulltextWait { .. }
                | GraphIndexAction::FulltextRebuild
                | GraphIndexAction::FulltextPause
                | GraphIndexAction::FulltextResume
                | GraphIndexAction::FulltextConfigure { .. }
                | GraphIndexAction::FulltextDrop
        ) {
            return self.fulltext_statement(stmt, graph.obj);
        }
        let indexes = self.property_indexes(graph.obj)?;
        if matches!(stmt.action, GraphIndexAction::Show) {
            let columns = [
                ("index_name", ColKind::Bytes),
                ("object_id", ColKind::Number),
                ("entity_kind", ColKind::Bytes),
                ("label_type", ColKind::Bytes),
                ("fields", ColKind::Bytes),
                ("is_unique", ColKind::Bool),
                ("status", ColKind::Bytes),
            ]
            .into_iter()
            .map(|(name, kind)| ColumnMeta {
                name: name.into(),
                kind,
            })
            .collect();
            let rows = indexes
                .into_iter()
                .map(|i| {
                    vec![
                        Value::Bytes(i.name.into_bytes()),
                        number(i.obj as usize),
                        Value::Bytes(if i.definition.entity == EntityKind::Node {
                            b"NODE".to_vec()
                        } else {
                            b"RELATIONSHIP".to_vec()
                        }),
                        i.definition
                            .label
                            .map_or(Value::Null, |v| Value::Bytes(v.into_bytes())),
                        Value::Bytes(serde_json::to_vec(&i.definition.fields).expect("paths")),
                        Value::Bool(i.definition.unique),
                        Value::Bytes(if i.status == 1 {
                            b"READY".to_vec()
                        } else {
                            b"INVALID".to_vec()
                        }),
                    ]
                })
                .collect();
            return Ok(QueryResult::Rows { columns, rows });
        }
        if self.in_transaction() {
            return Err(error(
                "graph index DDL is not allowed in an active transaction",
            ));
        }
        if matches!(stmt.action, GraphIndexAction::RebuildAccess) {
            let limits = self.graph_limits.clone();
            let (graph, image) = self.load_graph(&stmt.graph, &limits)?;
            let entries = (if image.uses_adjacency() {
                AccessEntries::from_nodes
            } else {
                AccessEntries::from_graph
            })(&graph, limits.max_text_bytes)
            .map_err(error)?;
            ddl::rebuild_graph_access_indexes(
                self.catalog,
                self.engine,
                &stmt.graph,
                &access_builds(&entries),
            )?;
            self.seq = self.catalog.current_seq();
            return Ok(QueryResult::Ddl(format!(
                "GRAPH {} REBUILD ACCESS",
                stmt.graph
            )));
        }
        let name = stmt
            .name
            .as_deref()
            .ok_or_else(|| error("missing graph index name"))?;
        match &stmt.action {
            GraphIndexAction::Create {
                entity,
                label,
                fields,
                unique,
            } => {
                crate::bind::check_new_object_name(name)?;
                if indexes.len() >= 64 {
                    return Err(error("graph property index limit is 64"));
                }
                let definition = PropertyIndex {
                    entity: if entity == "nodes" {
                        EntityKind::Node
                    } else {
                        EntityKind::Relationship
                    },
                    label: label.clone(),
                    fields: fields.clone(),
                    unique: *unique,
                };
                let source = definition.to_bytes().map_err(error)?;
                let (graph, _) = self.load_graph(&stmt.graph, &self.graph_limits.clone())?;
                let entries = definition.entries(&graph).map_err(error)?;
                ddl::create_graph_property_index(
                    self.catalog,
                    self.engine,
                    name,
                    &stmt.graph,
                    &source,
                    *unique,
                    &entries,
                )?;
            }
            GraphIndexAction::Drop => {
                ddl::drop_graph_property_index(self.catalog, self.engine, name, &stmt.graph)?;
            }
            GraphIndexAction::Rebuild => {
                let index = indexes
                    .iter()
                    .find(|i| i.name == name)
                    .ok_or_else(|| error("property index is not owned by this graph"))?;
                let (graph, _) = self.load_graph(&stmt.graph, &self.graph_limits.clone())?;
                let entries = index.definition.entries(&graph).map_err(error)?;
                ddl::rebuild_graph_property_index(
                    self.catalog,
                    self.engine,
                    name,
                    &stmt.graph,
                    &index.definition.to_bytes().map_err(error)?,
                    index.definition.unique,
                    &entries,
                )?;
            }
            _ => unreachable!("full-text and SHOW are handled above"),
        }
        self.seq = self.catalog.current_seq();
        Ok(QueryResult::Ddl(format!("GRAPH INDEX {name}")))
    }
    // Explicit native ordinal access avoids a cost-based heap-scan fallback in
    // the foreground, whose old row estimate could lag a large pending queue.
    fn native_record_rows(
        &mut self,
        name: &str,
        kind: u32,
        lower: u64,
        upper: u64,
    ) -> Result<Option<BTreeMap<u64, Vec<u8>>>, SessionError> {
        let snapshot = self.snapshot();
        let object = match self.catalog.resolve(snapshot, dict::namespace::TABLE, name) {
            Ok(object) => object,
            Err(bicdb_catalog::api::CatalogError::NotFound) => return Ok(None),
            Err(e) => return Err(error(e)),
        };
        self.ensure_recovery_object_accessible(object.obj)?;
        if object.type_code != kind || object.status != 1 {
            return Err(error("invalid protected full-text record store"));
        }
        let keys = self
            .catalog
            .indexes_of(snapshot, object.obj)
            .map_err(error)?;
        if keys.len() != 1
            || keys[0].status != 1
            || keys[0].kind != dict::index_kind::BTREE
            || !keys[0].is_unique
            || keys[0].cols.len() != 1
            || keys[0].cols[0].col != 1
        {
            return Err(error("invalid protected full-text ordinal tree"));
        }
        let record = AccessTree {
            obj: keys[0].obj,
            kind: keys[0].kind,
            name: name.into(),
            block: ddl::live_segment_block(self.catalog, keys[0].obj)?,
        };
        let limits = self.graph_limits.clone();
        let mut reader = NativeGraphReader {
            session: self,
            indexes: &[],
            trees: &[],
            record,
            limits: &limits,
            graph_scope: None,
            graph_obj: object.obj,
            adjacency: None,
            native_edges: BTreeMap::new(),
            selected_nodes: BTreeMap::new(),
            observed_nodes: BTreeMap::new(),
            observed_edges: BTreeMap::new(),
            reads: 0,
            record_reads: 0,
            bytes: 0,
            accesses: vec![],
        };
        // One entity keeps its 1024-row cap. A complete statistics/proof range
        // spans many independent records, with its own bounded collection cap.
        let max_entries = if upper.saturating_sub(lower) > 1023 {
            100_000
        } else {
            1024
        };
        Ok(Some(
            reader
                .records_bounded(lower, upper, max_entries)
                .map_err(error)?,
        ))
    }
    fn journal_head(&mut self, graph: u32) -> Result<Option<JournalHead>, SessionError> {
        self.native_record_rows(
            &ddl::graph_fulltext_journal_name(graph),
            dict::obj_kind::GRAPH_FULLTEXT_QUEUE,
            0,
            16,
        )?
        .map(|rows| JournalHead::from_native_rows(&rows, &journal_limits()).map_err(error))
        .transpose()
    }
    fn load_journal(&mut self, graph: u32) -> Result<Journal, SessionError> {
        self.check_graph_deadline("load_journal")?;
        if self.journal_head(graph)?.is_none() {
            return Ok(Journal::default());
        }
        Journal::from_native_rows(
            &self.load_graph_rows(
                &ddl::graph_fulltext_journal_name(graph),
                journal_limits().max_bytes,
            )?,
            &journal_limits(),
        )
        .map_err(error)
    }
    fn checked_journal_head(
        &mut self,
        graph: u32,
        indexes: &[NativeFulltextIndex],
    ) -> Result<Option<JournalHead>, SessionError> {
        let head = self.journal_head(graph)?;
        let expected: BTreeSet<_> = indexes
            .iter()
            .filter(|i| i.managed)
            .map(|i| i.obj)
            .collect();
        let actual: BTreeSet<_> = head
            .as_ref()
            .map(|h| h.consumers().keys().copied().collect())
            .unwrap_or_default();
        if expected != actual {
            return Err(error(
                "full-text journal consumers differ from graph-owned indexes",
            ));
        }
        Ok(head)
    }
    fn source_journal_patch(
        &mut self,
        graph: u32,
        delta: &GraphChanges,
        after: &Graph,
    ) -> Result<Option<(String, StoragePatch)>, SessionError> {
        let indexes = self.fulltext_indexes(graph)?;
        let Some(head) = self.checked_journal_head(graph, &indexes)? else {
            return Ok(None);
        };
        if head.consumers().is_empty() {
            return Ok(None);
        }
        // Logical net changes drive the journal. In particular, converting a
        // v1 layout must not enqueue every unchanged entity's reencoded chunks.
        let mut changes = BTreeMap::new();
        for id in delta.nodes().keys() {
            let revision = after
                .nodes()
                .get(id)
                .map(fulltext::node_revision)
                .transpose()
                .map_err(error)?;
            changes.insert(*id, (EntityKind::Node, *id, revision));
        }
        for id in delta.edges().keys() {
            let revision = after
                .edges()
                .get(id)
                .map(fulltext::edge_revision)
                .transpose()
                .map_err(error)?;
            changes.insert(*id, (EntityKind::Relationship, *id, revision));
        }
        if changes.is_empty() {
            return Ok(None);
        }
        let name = ddl::graph_fulltext_journal_name(graph);
        let limits = journal_limits();
        let mut old_rows = head.native_rows(&limits).map_err(error)?;
        let now = u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(error)?
                .as_millis(),
        )
        .map_err(error)?;
        let (next, events) = head
            .plan(
                &changes.into_values().collect::<Vec<_>>(),
                now,
                &limits,
                |id| {
                    let key = Event::ordinal(id)?;
                    let rows = self
                        .native_record_rows(
                            &name,
                            dict::obj_kind::GRAPH_FULLTEXT_QUEUE,
                            key,
                            key + 1,
                        )
                        .map_err(|e| bicdb_graph::Error(e.to_string()))?
                        .ok_or_else(|| bicdb_graph::Error("missing full-text journal".into()))?;
                    let event = Event::from_native_rows(id, &rows)?;
                    old_rows.extend(rows);
                    Ok(event)
                },
            )
            .map_err(error)?;
        let mut new_rows = next.native_rows(&limits).map_err(error)?;
        for event in events.values() {
            new_rows.extend(event.native_rows(&limits).map_err(error)?);
        }
        Ok(Some((name, record_patch(&old_rows, &new_rows))))
    }
    fn fulltext_batch_documents(
        &mut self,
        graph_name: &str,
        graph: u32,
        index: &NativeFulltextIndex,
        events: &[Event],
    ) -> Result<Vec<(u64, Option<fulltext::Document>)>, SessionError> {
        self.check_graph_deadline("fulltext_batch_documents")?;
        let limits = self.graph_limits.clone();
        let text_limits = TextLimits::default();
        let mut budget = fulltext::DocumentBudget::default();
        let (trees, record) = self.access_trees(graph)?;
        let mut result = vec![];
        let mut validated_endpoints = BTreeSet::new();
        if let Some(record) = record {
            let mut reader = NativeGraphReader {
                session: self,
                indexes: &[],
                trees: &trees,
                record,
                limits: &limits,
                graph_scope: None,
                graph_obj: graph,
                adjacency: None,
                native_edges: BTreeMap::new(),
                selected_nodes: BTreeMap::new(),
                observed_nodes: BTreeMap::new(),
                observed_edges: BTreeMap::new(),
                reads: 0,
                record_reads: 0,
                bytes: 0,
                accesses: vec![],
            };
            let header = reader.records(0, 0).map_err(error)?;
            if let Some(cache) = reader
                .snapshot_cache(header.get(&0).map(Vec::as_slice))
                .map_err(error)?
            {
                for event in events
                    .iter()
                    .filter(|e| e.entity == index.definition.entity)
                {
                    if event.id >= cache.allocator_high_water() {
                        return Err(error("full-text marker ID exceeds graph manifest"));
                    }
                    let document = match event.entity {
                        EntityKind::Node => {
                            let source = reader.node(event.id).map_err(error)?;
                            if source
                                .as_ref()
                                .map(fulltext::node_revision)
                                .transpose()
                                .map_err(error)?
                                != event.revision
                            {
                                return Err(error("full-text marker differs from source revision"));
                            }
                            source
                                .as_ref()
                                .map(|n| index.definition.node_document(n, &text_limits))
                                .transpose()
                                .map_err(error)?
                                .flatten()
                        }
                        EntityKind::Relationship => {
                            let source = reader.edge(event.id).map_err(error)?;
                            if source
                                .as_ref()
                                .map(fulltext::edge_revision)
                                .transpose()
                                .map_err(error)?
                                != event.revision
                            {
                                return Err(error("full-text marker differs from source revision"));
                            }
                            if let Some(e) = &source {
                                if e.source >= cache.allocator_high_water()
                                    || e.target >= cache.allocator_high_water()
                                {
                                    return Err(error("invalid full-text relationship endpoints"));
                                }
                                for endpoint in [e.source, e.target] {
                                    if !validated_endpoints.contains(&endpoint) {
                                        if reader.node(endpoint).map_err(error)?.is_none() {
                                            return Err(error(
                                                "invalid full-text relationship endpoints",
                                            ));
                                        }
                                        validated_endpoints.insert(endpoint);
                                    }
                                }
                            }
                            source
                                .as_ref()
                                .map(|e| index.definition.edge_document(e, &text_limits))
                                .transpose()
                                .map_err(error)?
                                .flatten()
                        }
                    };
                    if let Some(d) = &document {
                        budget.add(d, &text_limits).map_err(error)?;
                    }
                    result.push((event.id, document));
                }
                return Ok(result);
            }
        }
        // Legacy source images retain their existing bounded complete validation.
        let (source, _) = self.load_graph(graph_name, &limits)?;
        for event in events
            .iter()
            .filter(|e| e.entity == index.definition.entity)
        {
            let document = match event.entity {
                EntityKind::Node => {
                    let n = source.nodes().get(&event.id);
                    if n.map(fulltext::node_revision).transpose().map_err(error)? != event.revision
                    {
                        return Err(error("full-text marker differs from source revision"));
                    }
                    n.map(|n| index.definition.node_document(n, &text_limits))
                        .transpose()
                        .map_err(error)?
                        .flatten()
                }
                EntityKind::Relationship => {
                    let e = source.edges().get(&event.id);
                    if e.map(fulltext::edge_revision).transpose().map_err(error)? != event.revision
                    {
                        return Err(error("full-text marker differs from source revision"));
                    }
                    e.map(|e| index.definition.edge_document(e, &text_limits))
                        .transpose()
                        .map_err(error)?
                        .flatten()
                }
            };
            if let Some(d) = &document {
                budget.add(d, &text_limits).map_err(error)?;
            }
            result.push((event.id, document));
        }
        Ok(result)
    }
    fn publish_fulltext_batch(
        &mut self,
        graph_name: &str,
        graph: u32,
        index: &NativeFulltextIndex,
        journal: &Journal,
        old: &Generation,
        batch_rows: usize,
    ) -> Result<(Journal, Generation, usize), SessionError> {
        self.check_graph_deadline("publish_fulltext_batch")?;
        let consumer = journal
            .consumers()
            .get(&index.obj)
            .ok_or_else(|| error("missing full-text consumer"))?;
        if old.covered_seq() != consumer.covered_seq {
            return Err(error("full-text document watermark differs from journal"));
        }
        if consumer.paused {
            return Err(error("full-text consumer is paused; RESUME before SYNC"));
        }
        let batch = journal.batch(index.obj, batch_rows).map_err(error)?;
        let documents = self.fulltext_batch_documents(graph_name, graph, index, batch.events())?;
        let next_journal = journal.acknowledge(&batch).map_err(error)?;
        let (generation, changed, published_journal) = self.publish_fulltext_documents(
            graph_name,
            graph,
            index,
            journal,
            &next_journal,
            old,
            documents,
        )?;
        Ok((published_journal, generation, changed))
    }
    fn publish_fulltext_documents(
        &mut self,
        graph_name: &str,
        graph: u32,
        index: &NativeFulltextIndex,
        journal: &Journal,
        next_journal: &Journal,
        old: &Generation,
        documents: Vec<(u64, Option<fulltext::Document>)>,
    ) -> Result<(Generation, usize, Journal), SessionError> {
        self.check_graph_deadline("publish_fulltext_documents")?;
        let limits = TextLimits::default();
        let changed: BTreeSet<_> = documents.iter().map(|(id, _)| *id).collect();
        let covered_seq = next_journal.consumers()[&index.obj].covered_seq;
        // Updating v4 statistics through the generic heap DML path scans the
        // protected record store and becomes quadratic for a real corpus. Once
        // the corpus is nontrivial, coalesce every pending event into one native
        // bulk rebuild. The graph snapshot and the rebuilt journal watermark are
        // published by the same DDL transaction.
        if old.storage_format() == 4 && !changed.is_empty() {
            let rebuilt_journal = journal.rebuilt(index.obj).map_err(error)?;
            let (source_graph, _) = self.load_graph(graph_name, &self.graph_limits.clone())?;
            let generation = Generation::build(
                index.definition.clone(),
                &source_graph,
                old.generation()
                    .checked_add(1)
                    .ok_or_else(|| error("full-text generation overflow"))?,
                journal.source_seq(),
                &limits,
            )
            .map_err(error)?;
            let rows = generation.native_rows(&limits).map_err(error)?;
            let entries = generation.native_entries(&limits).map_err(error)?;
            let build = ddl::GraphFulltextBuild {
                source: &index.source,
                entries: &entries,
                rows: &rows,
            };
            ddl::rebuild_graph_fulltext_index_with_journal(
                self.catalog,
                self.engine,
                &index.name,
                graph_name,
                &build,
                &rebuilt_journal
                    .native_rows(&journal_limits())
                    .map_err(error)?,
            )?;
            self.seq = self.catalog.current_seq();
            self.fulltext_cache = None;
            return Ok((generation, changed.len(), rebuilt_journal));
        }
        let (generation, patch, added) = if old.storage_format() == 4 {
            let name = ddl::graph_fulltext_store_name(index.obj);
            let mut before = self
                .native_record_rows(&name, dict::obj_kind::GRAPH_FULLTEXT_DATA, 0, 1023)?
                .ok_or_else(|| error("missing full-text manifest"))?;
            let manifest =
                Generation::manifest_from_native_rows(&before, &limits).map_err(error)?;
            if manifest.definition != index.definition
                || manifest.generation != old.generation()
                || manifest.covered_seq != old.covered_seq()
            {
                return Err(error("full-text maintenance generation changed"));
            }
            let (lower, upper) = Generation::statistics_range(&manifest)
                .ok_or_else(|| error("missing maintenance statistics"))?;
            let mut size = before.values().map(Vec::len).sum::<usize>();
            let mut append = |rows: BTreeMap<u64, Vec<u8>>| -> Result<(), SessionError> {
                for (key, data) in rows {
                    size = size.saturating_add(data.len());
                    if size > limits.max_bytes {
                        return Err(error("full-text maintenance record budget exceeded"));
                    }
                    before.insert(key, data);
                }
                Ok(())
            };
            append(
                self.native_record_rows(&name, dict::obj_kind::GRAPH_FULLTEXT_DATA, lower, upper)?
                    .ok_or_else(|| error("missing maintenance statistics store"))?,
            )?;
            for id in &changed {
                let (lower, upper) = Generation::document_range(*id).map_err(error)?;
                append(
                    self.native_record_rows(
                        &name,
                        dict::obj_kind::GRAPH_FULLTEXT_DATA,
                        lower,
                        upper,
                    )?
                    .ok_or_else(|| error("missing maintenance document store"))?,
                )?;
            }
            let batch = Generation::apply_native_batch(&before, documents, covered_seq, &limits)
                .map_err(error)?;
            self.fulltext_maintenance_documents_loaded = self
                .fulltext_maintenance_documents_loaded
                .saturating_add(batch.documents_loaded);
            (
                Generation::metadata(&batch.manifest),
                record_patch(&batch.before, &batch.after),
                batch.entries,
            )
        } else {
            let generation = old
                .apply_batch(documents, covered_seq, &limits)
                .map_err(error)?;
            let patch = record_patch(
                &old.native_rows_for(&changed, &limits).map_err(error)?,
                &generation
                    .native_rows_for(&changed, &limits)
                    .map_err(error)?,
            );
            let added = generation
                .native_entries_for(&changed, &limits)
                .map_err(error)?;
            (generation, patch, added)
        };
        let queue_patch = record_patch(
            &journal.native_rows(&journal_limits()).map_err(error)?,
            &next_journal.native_rows(&journal_limits()).map_err(error)?,
        );
        let block = ddl::live_segment_block(self.catalog, index.obj)?;
        self.fulltext_cache = None;
        self.persist_record_patches(
            &[
                (&ddl::graph_fulltext_store_name(index.obj), &patch),
                (&ddl::graph_fulltext_journal_name(graph), &queue_patch),
            ],
            &[GraphIndexChanges { block, added }],
        )?;
        Ok((generation, changed.len(), next_journal.clone()))
    }
    fn wait_fulltext_journal(
        &mut self,
        graph_name: &str,
        graph: u32,
        index: &NativeFulltextIndex,
        raw_options: &str,
    ) -> Result<QueryResult, SessionError> {
        self.fulltext_maintenance_documents_loaded = 0;
        let started = Instant::now();
        let options = text_json_options(raw_options, &["target_source_seq", "timeout_ms"])?;
        let integer = |name: &str| {
            options
                .get(name)
                .map(|v| {
                    v.as_u64()
                        .ok_or_else(|| error(format!("{name} must be an unsigned integer")))
                })
                .transpose()
        };
        let target = integer("target_source_seq")?;
        let timeout = integer("timeout_ms")?.unwrap_or(30000);
        if timeout > 60000 {
            return Err(error("timeout_ms must be in 0..=60000"));
        }
        let mut journal = self.load_journal(graph)?;
        let mut generation = self.load_fulltext_for_maintenance(index)?;
        if generation.covered_seq() != journal.consumers()[&index.obj].covered_seq {
            return Err(error("full-text document watermark differs from journal"));
        }
        let mut wait = FixedTarget::try_begin_with_applied(&journal, index.obj, target, |event| {
            if event.entity != index.definition.entity {
                return Ok(true);
            }
            let revision = if generation.storage_format() == 4 {
                self.fulltext_old_revision(index, event.id)
                    .map_err(|e| bicdb_graph::Error(e.to_string()))?
            } else {
                generation.documents().get(&event.id).map(|d| d.revision)
            };
            Ok(revision == event.revision)
        })
        .map_err(error)?;
        let mut batches = 0;
        let mut changed_total = 0;
        let status = loop {
            let consumer = journal
                .consumers()
                .get(&index.obj)
                .ok_or_else(|| error("missing full-text consumer"))?;
            if generation.covered_seq() != consumer.covered_seq {
                return Err(error("full-text document watermark differs from journal"));
            }
            if wait.reached(&journal).map_err(error)? {
                break "READY";
            }
            if consumer.paused {
                break "PAUSED";
            }
            if started.elapsed() >= Duration::from_millis(timeout) {
                break "TIMEOUT";
            }
            let batch = wait
                .batch(
                    &journal,
                    index.policy.batch_rows.unwrap_or(self.fulltext_batch_rows),
                )
                .map_err(error)?;
            let documents =
                self.fulltext_batch_documents(graph_name, graph, index, batch.events())?;
            let (next_wait, next_journal) = wait.acknowledge(&journal, &batch).map_err(error)?;
            let (next_generation, changed, published_journal) = self.publish_fulltext_documents(
                graph_name,
                graph,
                index,
                &journal,
                &next_journal,
                &generation,
                documents,
            )?;
            wait = next_wait;
            generation = std::sync::Arc::new(next_generation);
            batches += 1;
            changed_total += changed;
            // The committed batch is exactly this prepared journal image.
            // Re-reading after the final COMMIT could turn successful coverage
            // into a timeout error while the acknowledgement is already durable.
            journal = published_journal;
        };
        let consumer = journal.consumers()[&index.obj];
        Ok(QueryResult::Rows {
            columns: text_columns(&[
                ("index_name", ColKind::Bytes),
                ("status", ColKind::Bytes),
                ("target_source_seq", ColKind::Number),
                ("covered_source_seq", ColKind::Number),
                ("source_seq", ColKind::Number),
                ("generation", ColKind::Number),
                ("target_reached", ColKind::Bool),
                ("source_complete", ColKind::Bool),
                ("paused", ColKind::Bool),
                ("batches", ColKind::Number),
                ("changed", ColKind::Number),
                ("elapsed_ms", ColKind::Number),
                ("documents_loaded", ColKind::Number),
            ]),
            rows: vec![vec![
                Value::Bytes(index.name.as_bytes().to_vec()),
                Value::Bytes(status.as_bytes().to_vec()),
                num_u64(wait.target_seq()),
                num_u64(consumer.covered_seq),
                num_u64(journal.source_seq()),
                num_u64(generation.generation()),
                Value::Bool(consumer.covered_seq >= wait.target_seq()),
                Value::Bool(consumer.covered_seq == journal.source_seq()),
                Value::Bool(consumer.paused),
                number(batches),
                number(changed_total),
                num_u64(u64::try_from(started.elapsed().as_millis()).map_err(error)?),
                number(self.fulltext_maintenance_documents_loaded),
            ]],
        })
    }
    fn sync_fulltext_journal(
        &mut self,
        graph_name: &str,
        graph: u32,
        index: &NativeFulltextIndex,
    ) -> Result<QueryResult, SessionError> {
        self.fulltext_maintenance_documents_loaded = 0;
        let mut journal = self.load_journal(graph)?;
        let target = journal.source_seq();
        let mut batches = 0;
        let mut changed_total = 0;
        let mut old = self.load_fulltext_for_maintenance(index)?;
        loop {
            let consumer = journal
                .consumers()
                .get(&index.obj)
                .ok_or_else(|| error("missing full-text consumer"))?;
            if old.covered_seq() != consumer.covered_seq {
                return Err(error("full-text document watermark differs from journal"));
            }
            if consumer.paused {
                return Err(error("full-text consumer is paused; RESUME before SYNC"));
            }
            if consumer.covered_seq >= target {
                break;
            }
            let (next, generation, changed) = self.publish_fulltext_batch(
                graph_name,
                graph,
                index,
                &journal,
                &old,
                index.policy.batch_rows.unwrap_or(self.fulltext_batch_rows),
            )?;
            journal = next;
            old = std::sync::Arc::new(generation);
            batches += 1;
            changed_total += changed;
        }
        Ok(QueryResult::Ddl(format!("SYNC FULLTEXT GRAPH INDEX {} GENERATION {} SOURCE_SEQ {target} BATCHES {batches} CHANGED {changed_total} DOCUMENTS_LOADED {}", index.name, old.generation(), self.fulltext_maintenance_documents_loaded)))
    }
    pub(crate) fn poll_fulltext_scheduler(
        &mut self,
        scheduler: &mut FulltextScheduler,
    ) -> Result<Option<String>, SessionError> {
        self.with_graph_deadline(self.graph_limits.max_elapsed_ms, |s| {
            s.poll_fulltext_scheduler_scoped(scheduler)
        })
    }
    fn poll_fulltext_scheduler_scoped(
        &mut self,
        scheduler: &mut FulltextScheduler,
    ) -> Result<Option<String>, SessionError> {
        let now = Instant::now();
        if now < scheduler.next_scan {
            return Ok(None);
        }
        scheduler.next_scan = now + Duration::from_millis(100);
        let mut live = BTreeSet::new();
        let mut candidates = vec![];
        for graph in self.catalog.graphs().map_err(error)? {
            self.check_graph_deadline("full-text scheduler metadata scan")?;
            for index in self.fulltext_indexes(graph.obj)? {
                if !index.managed || !index.policy.batch {
                    continue;
                }
                let key = (graph.obj, index.obj);
                live.insert(key);
                let interval = index.policy.interval_ms.unwrap_or(scheduler.interval_ms);
                let rows = index.policy.batch_rows.unwrap_or(scheduler.batch_rows);
                let state = scheduler.due.entry(key).or_insert((
                    now + Duration::from_millis(interval),
                    interval,
                    rows,
                ));
                if state.1 != interval || state.2 != rows {
                    *state = (now + Duration::from_millis(interval), interval, rows);
                }
                if state.0 <= now {
                    candidates.push((state.0, key, graph.name.clone(), index));
                }
            }
        }
        scheduler.due.retain(|key, _| live.contains(key));
        self.check_graph_deadline("after scheduler metadata scan")?;
        candidates.sort_by_key(|(time, key, _, _)| (*time, *key));
        let Some((_, key, name, index)) = candidates.into_iter().next() else {
            return Ok(None);
        };
        let state = scheduler.due.get_mut(&key).expect("live scheduled index");
        // Backoff is set before any fallible work, so an unhealthy consumer cannot starve others.
        state.0 = now + Duration::from_millis(state.1);
        let result = (|| {
            let indexes = self.fulltext_indexes(key.0)?;
            let head = self
                .checked_journal_head(key.0, &indexes)?
                .ok_or_else(|| error("missing managed journal"))?;
            let consumer = &head.consumers()[&index.obj];
            if consumer.paused || consumer.covered_seq == head.source_seq() {
                return Ok(None);
            }
            self.fulltext_maintenance_documents_loaded = 0;
            let journal = self.load_journal(key.0)?;
            let old = self.load_fulltext_for_maintenance(&index)?;
            let (_, generation, changed) =
                self.publish_fulltext_batch(&name, key.0, &index, &journal, &old, state.2)?;
            Ok(Some(format!(
                "全文后台维护 {}.{} GENERATION {} COVERED_SOURCE_SEQ {} CHANGED {changed} DOCUMENTS_LOADED {}",
                name,
                index.name,
                generation.generation(),
                generation.covered_seq(),
                self.fulltext_maintenance_documents_loaded
            )))
        })();
        if result.is_err() {
            // Retry at most once per second even with a short configured interval.
            state.0 = Instant::now() + Duration::from_millis(state.1.max(1000));
        }
        result
    }
    fn fulltext_indexes(&mut self, graph: u32) -> Result<Vec<NativeFulltextIndex>, SessionError> {
        self.check_graph_deadline("full-text index metadata")?;
        let snapshot = self.snapshot();
        let mut out = vec![];
        for index in self.catalog.indexes_of(snapshot, graph).map_err(error)? {
            self.check_graph_deadline("full-text descriptor decoding")?;
            if index.kind != dict::index_kind::GRAPH_FULLTEXT {
                continue;
            }
            if index.status != 1
                || index.is_unique
                || index.cols.len() != 1
                || index.cols[0].col != 0
            {
                return Err(error("invalid native full-text index metadata"));
            }
            let source = index
                .expr_src
                .as_deref()
                .ok_or_else(|| error("missing full-text definition"))?;
            let value: serde_json::Value = serde_json::from_slice(source).map_err(error)?;
            let v1 = value["format"] == "bicdb-graph-fulltext-managed-v1";
            let v2 = value["format"] == "bicdb-graph-fulltext-managed-v2";
            let managed = v1 || v2;
            let policy = if v2 {
                TextPolicy::parse(&value["policy"].to_string())?
            } else {
                TextPolicy::manual()
            };
            let definition = if managed {
                if value.as_object().map_or(0, |v| v.len()) != if v2 { 3 } else { 2 } {
                    return Err(error("invalid managed full-text descriptor"));
                }
                TextDefinition::from_bytes(
                    &serde_json::to_vec(&value["definition"]).map_err(error)?,
                )
                .map_err(error)?
            } else {
                TextDefinition::from_bytes(source).map_err(error)?
            };
            let name = self
                .catalog
                .resolve_by_obj(snapshot, index.obj)
                .map_err(error)?
                .name;
            out.push(NativeFulltextIndex {
                managed,
                policy,
                source: source.to_vec(),
                obj: index.obj,
                name,
                definition,
            });
        }
        Ok(out)
    }
    fn load_fulltext(
        &mut self,
        index: &NativeFulltextIndex,
    ) -> Result<std::sync::Arc<Generation>, SessionError> {
        self.check_graph_deadline("load_fulltext")?;
        let name = ddl::graph_fulltext_store_name(index.obj);
        let object = self
            .catalog
            .resolve(self.snapshot(), dict::namespace::TABLE, &name)
            .map_err(error)?;
        self.ensure_recovery_object_accessible(object.obj)?;
        if object.type_code != dict::obj_kind::GRAPH_FULLTEXT_DATA || object.status != 1 {
            return Err(error("invalid full-text document store"));
        }
        let checksum = self.fulltext_stamp(&name)?;
        if let Some((obj, stamp, generation)) = &self.fulltext_cache {
            if *obj == index.obj
                && *stamp == checksum
                && generation.definition() == &index.definition
            {
                return Ok(std::sync::Arc::clone(generation));
            }
        }
        // Keep only one logical 100 MiB generation rather than accumulating
        // every queried index. Source freshness is still rechecked per query.
        self.fulltext_cache = None;
        let limits = TextLimits::default();
        let generation = Generation::from_native_rows(
            &self.load_graph_rows(
                &name,
                limits.max_bytes.min(self.graph_limits.max_text_bytes),
            )?,
            &limits,
        )
        .map_err(error)?;
        if generation.definition() != &index.definition {
            return Err(error("full-text definition differs from document manifest"));
        }
        let generation = std::sync::Arc::new(generation);
        self.fulltext_cache = Some((index.obj, checksum, std::sync::Arc::clone(&generation)));
        Ok(generation)
    }
    fn load_fulltext_for_maintenance(
        &mut self,
        index: &NativeFulltextIndex,
    ) -> Result<std::sync::Arc<Generation>, SessionError> {
        self.check_graph_deadline("load_fulltext_for_maintenance")?;
        let metadata = self.fulltext_query_metadata(index)?;
        if metadata.storage_format() == 4 {
            return Ok(std::sync::Arc::new(metadata));
        }
        let old = self.load_fulltext(index)?;
        self.fulltext_maintenance_documents_loaded = self
            .fulltext_maintenance_documents_loaded
            .saturating_add(old.documents().len());
        Ok(old)
    }
    fn fulltext_old_revision(
        &mut self,
        index: &NativeFulltextIndex,
        id: u64,
    ) -> Result<Option<[u8; 32]>, SessionError> {
        self.check_graph_deadline("fulltext_old_revision")?;
        let name = ddl::graph_fulltext_store_name(index.obj);
        let header = self
            .native_record_rows(&name, dict::obj_kind::GRAPH_FULLTEXT_DATA, 0, 1023)?
            .ok_or_else(|| error("missing WAIT manifest"))?;
        let manifest = Generation::manifest_from_native_rows(&header, &TextLimits::default())
            .map_err(error)?;
        if manifest.definition != index.definition {
            return Err(error("WAIT definition differs from manifest"));
        }
        let (lower, upper) = Generation::document_range(id).map_err(error)?;
        let rows = self
            .native_record_rows(&name, dict::obj_kind::GRAPH_FULLTEXT_DATA, lower, upper)?
            .ok_or_else(|| error("missing WAIT document store"))?;
        let doc =
            Generation::document_from_native_rows(&manifest, id, &rows, &TextLimits::default())
                .map_err(error)?;
        self.fulltext_maintenance_documents_loaded = self
            .fulltext_maintenance_documents_loaded
            .saturating_add(usize::from(doc.is_some()));
        Ok(doc.map(|d| d.revision))
    }
    fn fulltext_query_metadata(
        &mut self,
        index: &NativeFulltextIndex,
    ) -> Result<Generation, SessionError> {
        let name = ddl::graph_fulltext_store_name(index.obj);
        let rows = self
            .native_record_rows(&name, dict::obj_kind::GRAPH_FULLTEXT_DATA, 0, 1023)?
            .ok_or_else(|| error("missing full-text document store"))?;
        let manifest =
            Generation::manifest_from_native_rows(&rows, &TextLimits::default()).map_err(error)?;
        if manifest.definition != index.definition {
            return Err(error("full-text definition differs from manifest"));
        }
        Ok(Generation::metadata(&manifest))
    }
    fn load_fulltext_candidates(
        &mut self,
        index: &NativeFulltextIndex,
        candidates: &BTreeSet<u64>,
        query: &str,
        options: &SearchOptions,
        work_budget: Option<usize>,
    ) -> Result<(std::sync::Arc<Generation>, usize, usize), SessionError> {
        self.check_graph_deadline("load_fulltext_candidates")?;
        let name = ddl::graph_fulltext_store_name(index.obj);
        let limits = TextLimits::default();
        let mut rows = self
            .native_record_rows(&name, dict::obj_kind::GRAPH_FULLTEXT_DATA, 0, 1023)?
            .ok_or_else(|| error("missing full-text document store"))?;
        let manifest = Generation::manifest_from_native_rows(&rows, &limits).map_err(error)?;
        if manifest.definition != index.definition {
            return Err(error("full-text definition differs from manifest"));
        }
        if candidates.is_empty() {
            return Ok((std::sync::Arc::new(Generation::metadata(&manifest)), 0, 0));
        }
        let Some((lower, upper)) = Generation::statistics_range(&manifest) else {
            // Preserve v1 BM25 scores until maintenance atomically publishes v2.
            let generation = self.load_fulltext(index)?;
            let work = generation.documents().len();
            if work_budget.is_some_and(|budget| work > budget) {
                return Err(error("full-text legacy loading work budget exceeded"));
            }
            return Ok((generation, work, 0));
        };
        let max_query_bytes = self.graph_limits.max_text_bytes;
        let mut bytes = rows.values().map(Vec::len).sum::<usize>();
        let mut record_work = 0usize;
        let mut append = |records: BTreeMap<u64, Vec<u8>>| -> Result<(), SessionError> {
            record_work = record_work.saturating_add(records.len());
            if work_budget.is_some_and(|budget| record_work > budget) {
                return Err(error("full-text record loading work budget exceeded"));
            }
            for (key, value) in records {
                bytes = bytes.saturating_add(value.len());
                if bytes > max_query_bytes {
                    return Err(error("full-text query record byte budget exceeded"));
                }
                rows.insert(key, value);
            }
            Ok(())
        };
        let selection = Generation::metadata(&manifest)
            .statistics_selection(query, options, &limits)
            .map_err(error)?;
        let ranges = selection
            .as_ref()
            .map_or_else(|| vec![(lower, upper)], |s| s.ranges());
        let mut statistics_work = 0usize;
        for (lower, upper) in ranges {
            let records = self
                .native_record_rows(&name, dict::obj_kind::GRAPH_FULLTEXT_DATA, lower, upper)?
                .ok_or_else(|| error("missing full-text statistics store"))?;
            statistics_work = statistics_work.saturating_add(records.len());
            append(records)?;
        }
        for id in candidates {
            let (lower, upper) = Generation::document_range(*id).map_err(error)?;
            append(
                self.native_record_rows(&name, dict::obj_kind::GRAPH_FULLTEXT_DATA, lower, upper)?
                    .ok_or_else(|| error("missing full-text document store"))?,
            )?;
        }
        Ok((
            std::sync::Arc::new(if let Some(selection) = selection {
                Generation::from_query_candidate_rows(&rows, candidates, &selection, &limits)
                    .map_err(error)?
            } else {
                Generation::from_candidate_rows(&rows, candidates, &limits).map_err(error)?
            }),
            record_work,
            statistics_work,
        ))
    }
    fn fulltext_stamp(&mut self, name: &str) -> Result<[u8; 32], SessionError> {
        let rows = self
            .native_record_rows(name, dict::obj_kind::GRAPH_FULLTEXT_DATA, 0, 0)?
            .ok_or_else(|| error("missing full-text document store"))?;
        let data = rows
            .get(&0)
            .ok_or_else(|| error("missing full-text manifest checksum"))?;
        data.as_slice()
            .try_into()
            .map_err(|_| error("invalid full-text manifest checksum length"))
    }
    fn fulltext_statement(
        &mut self,
        stmt: &GraphIndexStmt,
        graph_obj: u32,
    ) -> Result<QueryResult, SessionError> {
        self.check_graph_deadline("fulltext_statement")?;
        let indexes = self.fulltext_indexes(graph_obj)?;
        let head = self.checked_journal_head(graph_obj, &indexes)?;
        if matches!(stmt.action, GraphIndexAction::FulltextShow) {
            let mut rows = vec![];
            for index in &indexes {
                let generation = self.fulltext_query_metadata(index)?;
                let store = ddl::graph_fulltext_store_name(index.obj);
                let header = self
                    .native_record_rows(&store, dict::obj_kind::GRAPH_FULLTEXT_DATA, 0, 1023)?
                    .ok_or_else(|| error("missing full-text manifest"))?;
                let manifest =
                    Generation::manifest_from_native_rows(&header, &TextLimits::default())
                        .map_err(error)?;
                if index.managed
                    && head.as_ref().expect("checked head").consumers()[&index.obj].covered_seq
                        != generation.covered_seq()
                {
                    return Err(error("full-text document watermark differs from journal"));
                }
                rows.push(vec![
                    Value::Bytes(index.name.as_bytes().to_vec()),
                    number(index.obj as usize),
                    Value::Bytes(if index.definition.entity == EntityKind::Node {
                        b"NODE".to_vec()
                    } else {
                        b"RELATIONSHIP".to_vec()
                    }),
                    Value::Bytes(serde_json::to_vec(&index.definition.labels).map_err(error)?),
                    Value::Bytes(
                        serde_json::to_vec(
                            &index
                                .definition
                                .fields
                                .iter()
                                .map(|p| TextDefinition::field_name(p))
                                .collect::<Vec<_>>(),
                        )
                        .map_err(error)?,
                    ),
                    Value::Bytes(if index.policy.batch {
                        b"BATCH".to_vec()
                    } else {
                        b"MANUAL".to_vec()
                    }),
                    num_u64(generation.generation()),
                    if index.managed {
                        Value::Null
                    } else {
                        num_u64(generation.covered_seq())
                    },
                    number(manifest.documents),
                    Value::Bytes(
                        if index.managed
                            && head.as_ref().expect("checked head").consumers()[&index.obj].paused
                        {
                            b"PAUSED".to_vec()
                        } else if index.managed
                            && generation.covered_seq()
                                != head.as_ref().expect("checked head").source_seq()
                        {
                            b"DIRTY".to_vec()
                        } else {
                            b"READY_GENERATION".to_vec()
                        },
                    ),
                    if index.managed {
                        num_u64(generation.covered_seq())
                    } else {
                        Value::Null
                    },
                    if index.managed {
                        num_u64(head.as_ref().expect("checked head").source_seq())
                    } else {
                        Value::Null
                    },
                    if index.managed {
                        num_u64(head.as_ref().expect("checked head").consumers()[&index.obj].cursor)
                    } else {
                        Value::Null
                    },
                    if index.managed {
                        number(head.as_ref().expect("checked head").event_count())
                    } else {
                        Value::Null
                    },
                    if index.policy.batch {
                        num_u64(
                            index
                                .policy
                                .interval_ms
                                .unwrap_or(self.fulltext_interval_ms),
                        )
                    } else {
                        Value::Null
                    },
                    num_u64(index.policy.batch_rows.unwrap_or(self.fulltext_batch_rows) as u64),
                    Value::Bytes(if index.policy.interval_ms.is_some() {
                        b"INDEX".to_vec()
                    } else {
                        b"INSTANCE".to_vec()
                    }),
                    Value::Bytes(if index.policy.batch_rows.is_some() {
                        b"INDEX".to_vec()
                    } else {
                        b"INSTANCE".to_vec()
                    }),
                    Value::Bytes(format!("v{}", manifest.storage_format).into_bytes()),
                    manifest.statistics_pages.map_or(Value::Null, number),
                ]);
            }
            return Ok(QueryResult::Rows {
                columns: text_columns(&[
                    ("index_name", ColKind::Bytes),
                    ("object_id", ColKind::Number),
                    ("entity_kind", ColKind::Bytes),
                    ("labels_types", ColKind::Bytes),
                    ("fields", ColKind::Bytes),
                    ("update_mode", ColKind::Bytes),
                    ("generation", ColKind::Number),
                    ("covered_commit_seq", ColKind::Number),
                    ("documents", ColKind::Number),
                    ("status", ColKind::Bytes),
                    ("covered_source_seq", ColKind::Number),
                    ("source_seq", ColKind::Number),
                    ("cursor", ColKind::Number),
                    ("shared_pending", ColKind::Number),
                    ("interval_ms", ColKind::Number),
                    ("batch_rows", ColKind::Number),
                    ("interval_source", ColKind::Bytes),
                    ("batch_rows_source", ColKind::Bytes),
                    ("storage_format", ColKind::Bytes),
                    ("statistics_pages", ColKind::Number),
                ]),
                rows,
            });
        }
        let name = stmt
            .name
            .as_deref()
            .ok_or_else(|| error("missing full-text index name"))?;
        if let GraphIndexAction::FulltextSearch { query, options } = &stmt.action {
            let index = indexes
                .iter()
                .find(|i| i.name == name)
                .ok_or_else(|| error("full-text index is not owned by this graph"))?;
            let generation = self.fulltext_query_metadata(index)?;
            return self.fulltext_search(
                stmt.graph.as_str(),
                graph_obj,
                index,
                &generation,
                query,
                options,
                head.as_ref(),
            );
        }
        if self.in_transaction() {
            return Err(error(
                "full-text index DDL/SYNC is not allowed in an active transaction",
            ));
        }
        if let GraphIndexAction::FulltextWait { options } = &stmt.action {
            let index = indexes
                .iter()
                .find(|i| i.name == name)
                .ok_or_else(|| error("full-text index is not owned by this graph"))?;
            if !index.managed {
                return Err(error(
                    "legacy full-text index has no source sequence for WAIT",
                ));
            }
            return self.wait_fulltext_journal(&stmt.graph, graph_obj, index, options);
        }
        if let GraphIndexAction::FulltextConfigure { options } = &stmt.action {
            let index = indexes
                .iter()
                .find(|i| i.name == name)
                .ok_or_else(|| error("full-text index is not owned by this graph"))?;
            if !index.managed {
                return Err(error("legacy full-text index has no managed journal"));
            }
            let policy = TextPolicy::parse(options)?;
            let source = text_source(&index.definition, Some(&policy))?;
            self.seq = ddl::alter_graph_fulltext_source(
                self.catalog,
                self.engine,
                name,
                &stmt.graph,
                &index.source,
                &source,
            )?;
            return Ok(QueryResult::Ddl(format!(
                "ALTER FULLTEXT GRAPH INDEX {name} OPTIONS {}",
                policy.options()
            )));
        }
        if matches!(
            stmt.action,
            GraphIndexAction::FulltextPause | GraphIndexAction::FulltextResume
        ) {
            let index = indexes
                .iter()
                .find(|i| i.name == name)
                .ok_or_else(|| error("full-text index is not owned by this graph"))?;
            if !index.managed {
                return Err(error("legacy full-text index has no managed journal"));
            }
            let journal = self.load_journal(graph_obj)?;
            let paused = matches!(stmt.action, GraphIndexAction::FulltextPause);
            let next = journal.paused(index.obj, paused).map_err(error)?;
            let patch = record_patch(
                &journal.native_rows(&journal_limits()).map_err(error)?,
                &next.native_rows(&journal_limits()).map_err(error)?,
            );
            self.persist_record_patches(
                &[(&ddl::graph_fulltext_journal_name(graph_obj), &patch)],
                &[],
            )?;
            return Ok(QueryResult::Ddl(format!(
                "{} FULLTEXT GRAPH INDEX {name}",
                if paused { "PAUSE" } else { "RESUME" }
            )));
        }
        if matches!(stmt.action, GraphIndexAction::FulltextDrop) {
            let index = indexes
                .iter()
                .find(|i| i.name == name)
                .ok_or_else(|| error("full-text index is not owned by this graph"))?;
            if index.managed {
                let journal = self
                    .load_journal(graph_obj)?
                    .unregister(index.obj)
                    .map_err(error)?;
                ddl::drop_graph_fulltext_index_with_journal(
                    self.catalog,
                    self.engine,
                    name,
                    &stmt.graph,
                    &journal.native_rows(&journal_limits()).map_err(error)?,
                )?;
            } else {
                ddl::drop_graph_fulltext_index(self.catalog, self.engine, name, &stmt.graph)?;
            }
            self.fulltext_cache = None;
            self.seq = self.catalog.current_seq();
            return Ok(QueryResult::Ddl(format!(
                "DROP FULLTEXT GRAPH INDEX {name}"
            )));
        }
        let limits = TextLimits::default();
        let managed = matches!(stmt.action, GraphIndexAction::FulltextCreate { .. })
            || indexes.iter().any(|i| i.name == name && i.managed);
        let journal = if managed {
            Some(self.load_journal(graph_obj)?)
        } else {
            None
        };
        let policy = match &stmt.action {
            GraphIndexAction::FulltextCreate { options, .. } => TextPolicy::parse(options)?,
            _ => indexes
                .iter()
                .find(|i| i.name == name)
                .expect("existing full-text index")
                .policy
                .clone(),
        };
        let (definition, next_generation) = match &stmt.action {
            GraphIndexAction::FulltextCreate {
                entity,
                labels,
                fields,
                ..
            } => {
                crate::bind::check_new_object_name(name)?;
                if indexes.len() >= 64 {
                    return Err(error("full-text index limit is 64 per graph"));
                }
                let d = TextDefinition {
                    entity: if entity == "nodes" {
                        EntityKind::Node
                    } else {
                        EntityKind::Relationship
                    },
                    labels: labels.clone(),
                    fields: fields
                        .iter()
                        .map(|p| {
                            p.iter()
                                .map(|v| match v {
                                    GraphTextPathPart::Key(k) => PathPart::Key(k.clone()),
                                    GraphTextPathPart::Index(i) => PathPart::Index(*i),
                                })
                                .collect()
                        })
                        .collect(),
                };
                d.validate().map_err(error)?;
                (d, 1)
            }
            GraphIndexAction::FulltextSync => {
                let index = indexes
                    .iter()
                    .find(|i| i.name == name)
                    .ok_or_else(|| error("full-text index is not owned by this graph"))?;
                if index.managed {
                    return self.sync_fulltext_journal(&stmt.graph, graph_obj, index);
                }
                let old = self.load_fulltext(index)?;
                let (graph, _) = self.load_graph(&stmt.graph, &self.graph_limits.clone())?;
                let (generation, changed) = old
                    .synchronize(&graph, self.snapshot().as_raw(), &limits)
                    .map_err(error)?;
                let before = old.native_rows_for(&changed, &limits).map_err(error)?;
                let after = generation
                    .native_rows_for(&changed, &limits)
                    .map_err(error)?;
                let mut patch = StoragePatch {
                    entities_changed: changed.len(),
                    ..StoragePatch::default()
                };
                for (key, data) in &after {
                    match before.get(key) {
                        None => {
                            patch.inserted.insert(*key, data.clone());
                        }
                        Some(prior) if prior != data => {
                            patch.updated.insert(*key, data.clone());
                        }
                        _ => {}
                    }
                }
                patch.removed = before
                    .keys()
                    .filter(|key| !after.contains_key(key))
                    .copied()
                    .collect();
                let entries = generation
                    .native_entries_for(&changed, &limits)
                    .map_err(error)?;
                let block = ddl::live_segment_block(self.catalog, index.obj)?;
                self.fulltext_cache = None;
                self.persist_graph(
                    &ddl::graph_fulltext_store_name(index.obj),
                    &patch,
                    &[GraphIndexChanges {
                        block,
                        added: entries,
                    }],
                )?;
                // Native DML commit already advanced the Session snapshot.
                // The catalog's last DDL sequence may be older than this
                // publication; resetting to it would hide committed chunks.
                return Ok(QueryResult::Ddl(format!(
                    "SYNC FULLTEXT GRAPH INDEX {name} GENERATION {} CHANGED {}",
                    generation.generation(),
                    changed.len()
                )));
            }
            GraphIndexAction::FulltextRebuild => {
                let index = indexes
                    .iter()
                    .find(|i| i.name == name)
                    .ok_or_else(|| error("full-text index is not owned by this graph"))?;
                // Rebuilding a corrupt document body only needs the valid
                // manifest version and the authoritative catalog definition.
                let store = ddl::graph_fulltext_store_name(index.obj);
                let results = self.graph_sql(
                    &format!(
                        "SELECT ordinal,data FROM {} WHERE ordinal<=1023 ORDER BY ordinal",
                        quoted(&store)
                    ),
                    &[],
                )?;
                let rows = Self::graph_rows_from_result(&results, 1023 * 4096 + 32)?;
                let manifest =
                    Generation::manifest_from_native_rows(&rows, &limits).map_err(error)?;
                if manifest.definition != index.definition {
                    return Err(error("full-text definition differs from document manifest"));
                }
                (
                    index.definition.clone(),
                    manifest
                        .generation
                        .checked_add(1)
                        .ok_or_else(|| error("full-text generation overflow"))?,
                )
            }
            _ => unreachable!("full-text action"),
        };
        self.fulltext_cache = None;
        let (graph, _) = self.load_graph(&stmt.graph, &self.graph_limits.clone())?;
        let generation = Generation::build(
            definition.clone(),
            &graph,
            next_generation,
            journal
                .as_ref()
                .map_or(self.snapshot().as_raw(), Journal::source_seq),
            &limits,
        )
        .map_err(error)?;
        let rows = generation.native_rows(&limits).map_err(error)?;
        let entries = generation.native_entries(&limits).map_err(error)?;
        let source = if matches!(stmt.action, GraphIndexAction::FulltextCreate { .. }) {
            text_source(&definition, managed.then_some(&policy))?
        } else {
            indexes
                .iter()
                .find(|i| i.name == name)
                .expect("rebuild identity")
                .source
                .clone()
        };
        let build = ddl::GraphFulltextBuild {
            source: &source,
            entries: &entries,
            rows: &rows,
        };
        if matches!(stmt.action, GraphIndexAction::FulltextCreate { .. }) {
            let journal = journal.expect("managed create");
            ddl::create_graph_fulltext_index_with_journal(
                self.catalog,
                self.engine,
                name,
                &stmt.graph,
                &build,
                |obj| {
                    journal
                        .register(obj, journal.source_seq())
                        .and_then(|j| j.native_rows(&journal_limits()))
                        .map_err(|e| ddl::DdlError::BadIndexDef(e.to_string()))
                },
            )?;
        } else if let Some(journal) = journal {
            let index = indexes
                .iter()
                .find(|i| i.name == name)
                .expect("rebuild identity");
            let rows = journal
                .rebuilt(index.obj)
                .and_then(|j| j.native_rows(&journal_limits()))
                .map_err(error)?;
            ddl::rebuild_graph_fulltext_index_with_journal(
                self.catalog,
                self.engine,
                name,
                &stmt.graph,
                &build,
                &rows,
            )?;
        } else {
            ddl::rebuild_graph_fulltext_index(
                self.catalog,
                self.engine,
                name,
                &stmt.graph,
                &build,
            )?;
        }
        self.seq = self.catalog.current_seq();
        Ok(QueryResult::Ddl(format!(
            "FULLTEXT GRAPH INDEX {name} GENERATION {next_generation}"
        )))
    }
    fn cypher_fulltext_indexes(
        &mut self,
        obj: u32,
        budget: usize,
    ) -> Result<(Vec<FulltextIndex>, IndexAccess, usize), SessionError> {
        self.check_graph_deadline("cypher_fulltext_indexes")?;
        if budget == 0 {
            return Err(error("full-text planning work budget exhausted"));
        }
        let indexes = self.fulltext_indexes(obj)?;
        let work = indexes.len().saturating_add(1);
        if work > budget {
            return Err(error("full-text planning work budget exceeded"));
        }
        let access = IndexAccess {
            access: "FULLTEXT_PLAN",
            name: "<auto>".into(),
            seeks: 1,
            entries: indexes.len(),
            candidates: 0,
            source_checks: 0,
            documents_loaded: 0,
            statistics_records_loaded: 0,
        };
        Ok((
            indexes
                .into_iter()
                .map(|index| FulltextIndex {
                    name: index.name,
                    definition: index.definition,
                })
                .collect(),
            access,
            work,
        ))
    }
    fn cypher_fulltext(
        &mut self,
        graph: &str,
        obj: u32,
        request: &FulltextRequest,
        staged: Option<&Graph>,
        budget: usize,
    ) -> Result<(Vec<FulltextHit>, IndexAccess, usize), SessionError> {
        self.check_graph_deadline("cypher_fulltext")?;
        if budget == 0 {
            return Err(error("full-text statement work budget exhausted"));
        }
        let indexes = self.fulltext_indexes(obj)?;
        let index = indexes
            .iter()
            .find(|i| i.name == request.index)
            .ok_or_else(|| error("full-text index is not owned by this graph"))?;
        if index.definition.entity != request.entity {
            return Err(error("full-text index has wrong entity kind"));
        }
        let head = self.checked_journal_head(obj, &indexes)?;
        let generation = self.fulltext_query_metadata(index)?;
        let options = GraphValue::Map(request.options.clone())
            .to_json(&Graph::new())
            .to_string();
        let result = self.fulltext_search_view(
            graph,
            obj,
            index,
            &generation,
            &request.query,
            &options,
            head.as_ref(),
            staged,
            Some(budget - 1),
        )?;
        let QueryResult::Rows { columns, rows } = result.result else {
            unreachable!("full-text rows");
        };
        let mut hits = vec![];
        for row in rows {
            let Value::Number(id) = &row[0] else {
                return Err(error("invalid full-text ID"));
            };
            let id = id.to_string().parse().map_err(error)?;
            let mut metadata = BTreeMap::new();
            for (column, value) in columns.iter().zip(row).skip(1) {
                let value = match value {
                    Value::Null => GraphValue::Null,
                    Value::Number(n) => GraphValue::Number(n),
                    Value::Bool(b) => GraphValue::Bool(b),
                    Value::Bytes(b) => GraphValue::String(String::from_utf8(b).map_err(error)?),
                    Value::GraphElement(_) => {
                        return Err(error("GRAPH_ELEMENT cannot be used as a Cypher parameter"))
                    }
                };
                metadata.insert(column.name.clone(), value);
            }
            hits.push(FulltextHit {
                id,
                columns: metadata,
            });
        }
        let access = IndexAccess {
            access: result.mode,
            name: index.name.clone(),
            seeks: 1,
            entries: result.posting_entries,
            candidates: hits.len(),
            source_checks: result.source_checks,
            documents_loaded: result.documents_loaded,
            statistics_records_loaded: result.statistics_records_loaded,
        };
        // Manifest/ownership lookup costs work even if a posting seek is empty.
        // This also bounds nested correlated calls that produce no rows.
        Ok((hits, access, result.work.saturating_add(1)))
    }
    #[allow(clippy::too_many_arguments)]
    fn fulltext_search(
        &mut self,
        graph_name: &str,
        graph_obj: u32,
        index: &NativeFulltextIndex,
        generation: &Generation,
        query: &str,
        raw_options: &str,
        head: Option<&JournalHead>,
    ) -> Result<QueryResult, SessionError> {
        self.fulltext_search_view(
            graph_name,
            graph_obj,
            index,
            generation,
            query,
            raw_options,
            head,
            None,
            None,
        )
        .map(|r| r.result)
    }
    #[allow(clippy::too_many_arguments)]
    fn fulltext_search_view(
        &mut self,
        graph_name: &str,
        graph_obj: u32,
        index: &NativeFulltextIndex,
        generation: &Generation,
        query: &str,
        raw_options: &str,
        head: Option<&JournalHead>,
        staged: Option<&Graph>,
        work_budget: Option<usize>,
    ) -> Result<NativeTextResult, SessionError> {
        self.check_graph_deadline("fulltext_search_view")?;
        let work_budget = work_budget
            .unwrap_or(self.graph_limits.max_expansions)
            .min(self.graph_limits.max_expansions);
        if index.managed
            && head
                .ok_or_else(|| error("missing full-text journal"))?
                .consumers()[&index.obj]
                .covered_seq
                != generation.covered_seq()
        {
            return Err(error("full-text document watermark differs from journal"));
        }
        let opts = text_json_options(
            raw_options,
            &[
                "db_type",
                "fields",
                "labels",
                "channel",
                "consistency",
                "limit",
            ],
        )?;
        let domain = opts
            .get("db_type")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| error("full-text search requires string db_type"))?;
        let fields = match opts.get("fields") {
            None => vec![],
            Some(serde_json::Value::Array(a)) if a.len() <= 16 => a
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(String::from)
                        .ok_or_else(|| error("fields must contain strings"))
                })
                .collect::<Result<Vec<_>, _>>()?,
            _ => return Err(error("fields must be an array of at most 16 strings")),
        };
        let labels = match opts.get("labels") {
            None => vec![],
            Some(serde_json::Value::Array(values)) if values.len() <= 16 => {
                let mut seen = BTreeSet::new();
                values
                    .iter()
                    .map(|value| {
                        let label = value
                            .as_str()
                            .filter(|s| !s.trim().is_empty() && s.len() <= 128)
                            .ok_or_else(|| {
                                error("labels must contain nonempty strings of at most 128 bytes")
                            })?;
                        if !seen.insert(label) {
                            return Err(error("duplicate full-text label"));
                        }
                        Ok(label.to_owned())
                    })
                    .collect::<Result<Vec<_>, SessionError>>()?
            }
            _ => return Err(error("labels must be an array of at most 16 strings")),
        };
        if opts.contains_key("labels") && index.definition.entity != EntityKind::Node {
            return Err(error("labels are supported only for node full-text search"));
        }
        let eligible = |node: &&bicdb_graph::Node| {
            labels.is_empty() || labels.iter().any(|label| node.labels.contains(label))
        };
        let channel = match opts.get("channel") {
            None => Channel::Terms,
            Some(serde_json::Value::String(v)) => match v.as_str() {
                "terms" => Channel::Terms,
                "phrase" => Channel::Phrase,
                "exact" => Channel::Exact,
                _ => return Err(error("unknown full-text channel")),
            },
            _ => return Err(error("channel must be a string")),
        };
        let strict = match opts.get("consistency") {
            None => true,
            Some(serde_json::Value::String(v)) if v == "strict" => true,
            Some(serde_json::Value::String(v)) if v == "eventual" => false,
            _ => return Err(error("consistency must be strict or eventual")),
        };
        let strict = strict || staged.is_some();
        let limit = match opts.get("limit") {
            None => 10000,
            Some(v) => v
                .as_u64()
                .filter(|n| *n <= 10000)
                .ok_or_else(|| error("limit must be an unsigned integer <=10000"))?
                as usize,
        };
        let options = SearchOptions {
            domain: domain.into(),
            fields,
            channel,
        };
        let limits = TextLimits::default();
        let snapshot = self.snapshot().as_raw();
        let mut work;
        let mut posting_entries = 0;
        let mut source_checks;
        let documents_loaded;
        let mut statistics_records_loaded = 0;
        let (hits, mode, covered) = if strict {
            // A mutation within this Cypher statement takes precedence over the
            // pre-statement persisted source and postings, even for eventual requests.
            let loaded;
            let graph = if let Some(graph) = staged {
                graph
            } else {
                loaded = self.load_graph(graph_name, &self.graph_limits.clone())?.0;
                &loaded
            };
            work = graph.nodes().len().saturating_add(graph.edges().len());
            source_checks = work;
            if work > work_budget {
                return Err(error("full-text statement source work budget exceeded"));
            }
            let current = Generation::build(
                index.definition.clone(),
                graph,
                generation.generation(),
                snapshot,
                &limits,
            )
            .map_err(error)?;
            documents_loaded = current.documents().len();
            let hits = current
                .search(query, &options, &limits, |id| {
                    match index.definition.entity {
                        EntityKind::Node => graph
                            .nodes()
                            .get(&id)
                            .filter(eligible)
                            .map(fulltext::node_revision)
                            .transpose(),
                        EntityKind::Relationship => graph
                            .edges()
                            .get(&id)
                            .map(fulltext::edge_revision)
                            .transpose(),
                    }
                })
                .map_err(error)?;
            (
                hits,
                if staged.is_some() {
                    "strict_staged_scan"
                } else {
                    "strict_scan"
                },
                snapshot,
            )
        } else {
            let mut by_term = BTreeMap::<String, BTreeSet<u64>>::new();
            let mut read = 0usize;
            for range in generation
                .term_ranges(query, &options, &limits)
                .map_err(error)?
            {
                let rows = self
                    .catalog
                    .graph_index_range(
                        index.obj,
                        Some(&range.lower),
                        range.upper.as_deref(),
                        work_budget.saturating_add(1).saturating_sub(read),
                    )
                    .map_err(error)?;
                read = read.saturating_add(rows.len());
                if read > work_budget {
                    return Err(error(
                        "full-text posting budget exceeded; REBUILD or narrow the query",
                    ));
                }
                let ids = by_term.entry(range.term).or_default();
                for (key, id) in rows {
                    if key.starts_with(&range.lower) {
                        ids.insert(id.as_raw());
                    }
                }
            }
            // AND across query terms, OR across selected fields. Ghost tuples
            // still require current document/source checks after intersection.
            let candidates = by_term
                .into_values()
                .reduce(|a, b| a.intersection(&b).copied().collect())
                .unwrap_or_default();
            let (selected, record_work, statistics_work) = self.load_fulltext_candidates(
                index,
                &candidates,
                query,
                &options,
                Some(work_budget.saturating_sub(read)),
            )?;
            if selected.generation() != generation.generation()
                || selected.covered_seq() != generation.covered_seq()
            {
                return Err(error("full-text generation changed during query"));
            }
            documents_loaded = selected.documents().len();
            statistics_records_loaded = statistics_work;
            let generation = selected.as_ref();
            work = read.saturating_add(record_work);
            posting_entries = read;
            source_checks = 0usize;
            let (trees, record) = self.access_trees(graph_obj)?;
            let native_limits = self.graph_limits.clone();
            let mut lazy_hits = None;
            let mut validated_endpoints = BTreeSet::new();
            if let Some(record) = record {
                let mut reader = NativeGraphReader {
                    session: self,
                    indexes: &[],
                    trees: &trees,
                    record,
                    limits: &native_limits,
                    graph_scope: None,
                    graph_obj,
                    adjacency: None,
                    native_edges: BTreeMap::new(),
                    selected_nodes: BTreeMap::new(),
                    observed_nodes: BTreeMap::new(),
                    observed_edges: BTreeMap::new(),
                    reads: 0,
                    record_reads: 0,
                    bytes: 0,
                    accesses: vec![],
                };
                let header = reader.records(0, 0).map_err(error)?;
                if let Some(cache) = reader
                    .snapshot_cache(header.get(&0).map(Vec::as_slice))
                    .map_err(error)?
                {
                    lazy_hits = Some(
                        generation
                            .search_candidates(query, &options, &limits, &candidates, |id| {
                                source_checks += 1;
                                if work.saturating_add(source_checks) > work_budget { return Err(bicdb_graph::Error("full-text source check budget exceeded".into())); }
                                match index.definition.entity {
                                    EntityKind::Node => match reader.node(id)? {
                                        None=>Ok(None),Some(node)=> {
                                            if id>=cache.allocator_high_water() {return Err(bicdb_graph::Error("full-text source ID exceeds graph manifest".into()));}
                                            if eligible(&&node) { fulltext::node_revision(&node).map(Some) } else { Ok(None) }
                                        }
                                    },
                                    EntityKind::Relationship => match reader.edge(id)? {
                                        None=>Ok(None),Some(edge)=> {
                                            if id>=cache.allocator_high_water() || edge.source>=cache.allocator_high_water() || edge.target>=cache.allocator_high_water()
                                                {
                                                return Err(bicdb_graph::Error("invalid full-text relationship identity/endpoints".into()));
                                            }
                                            for endpoint in [edge.source,edge.target] {
                                                if !validated_endpoints.contains(&endpoint) {
                                                    if reader.node(endpoint)?.is_none() { return Err(bicdb_graph::Error("invalid full-text relationship endpoints".into())); }
                                                    validated_endpoints.insert(endpoint);
                                                }
                                            }
                                            fulltext::edge_revision(&edge).map(Some)
                                        }
                                    },
                                }
                            })
                            .map_err(error)?,
                    );
                }
            }
            let hits = match lazy_hits {
                Some(hits) => hits,
                None => {
                    let (graph, _) = self.load_graph(graph_name, &native_limits)?;
                    source_checks = graph.nodes().len().saturating_add(graph.edges().len());
                    if work.saturating_add(source_checks) > work_budget {
                        return Err(error("full-text statement source work budget exceeded"));
                    }
                    generation
                        .search_candidates(query, &options, &limits, &candidates, |id| match index
                            .definition
                            .entity
                        {
                            EntityKind::Node => graph
                                .nodes()
                                .get(&id)
                                .filter(eligible)
                                .map(fulltext::node_revision)
                                .transpose(),
                            EntityKind::Relationship => graph
                                .edges()
                                .get(&id)
                                .map(fulltext::edge_revision)
                                .transpose(),
                        })
                        .map_err(error)?
                }
            };
            source_checks = source_checks.saturating_add(validated_endpoints.len());
            work = work.saturating_add(source_checks);
            if work > work_budget {
                return Err(error("full-text statement work budget exceeded"));
            }
            (hits, "FULLTEXT_BTREE", generation.covered_seq())
        };
        let rows = hits
            .into_iter()
            .take(limit)
            .map(|hit| {
                Ok(vec![
                    num_u64(hit.id),
                    Value::Number(Number::parse(&hit.score.to_string()).map_err(error)?),
                    Value::Bytes(hit.field.into_bytes()),
                    Value::Bytes(hit.snippet.into_bytes()),
                    number(hit.offset),
                    number(hit.line),
                    Value::Bytes(mode.as_bytes().to_vec()),
                    num_u64(generation.generation()),
                    if staged.is_some() || index.managed && !strict {
                        Value::Null
                    } else {
                        num_u64(covered)
                    },
                    Value::Bool(
                        strict
                            || index.managed
                                && head.is_some_and(|h| generation.covered_seq() == h.source_seq()),
                    ),
                    if index.managed && staged.is_none() {
                        num_u64(if strict {
                            head.expect("managed head").source_seq()
                        } else {
                            generation.covered_seq()
                        })
                    } else {
                        Value::Null
                    },
                    if index.managed && staged.is_none() {
                        num_u64(head.expect("managed head").source_seq())
                    } else {
                        Value::Null
                    },
                ])
            })
            .collect::<Result<Vec<_>, SessionError>>()?;
        self.check_graph_deadline("after full-text result materialization")?;
        if rows.len() > self.graph_limits.max_rows {
            return Err(error("full-text result row budget exceeded"));
        }
        Ok(NativeTextResult {
            mode,
            work,
            posting_entries,
            source_checks,
            documents_loaded,
            statistics_records_loaded,
            result: QueryResult::Rows {
                columns: text_columns(&[
                    ("element_id", ColKind::Number),
                    ("score", ColKind::Number),
                    ("field", ColKind::Bytes),
                    ("snippet", ColKind::Bytes),
                    ("byte_offset", ColKind::Number),
                    ("line_number", ColKind::Number),
                    ("mode", ColKind::Bytes),
                    ("generation", ColKind::Number),
                    ("covered_commit_seq", ColKind::Number),
                    ("complete", ColKind::Bool),
                    ("covered_source_seq", ColKind::Number),
                    ("source_seq", ColKind::Number),
                ]),
                rows,
            },
        })
    }
    fn graph_sql(
        &mut self,
        sql: &str,
        params: &[(&str, Value)],
    ) -> Result<Vec<QueryResult>, SessionError> {
        let old = std::mem::replace(&mut self.graph_internal, true);
        let saved = std::mem::take(&mut self.exec_params);
        let result = self.execute_with_params(sql, params);
        self.exec_params = saved;
        self.graph_internal = old;
        result
    }
    fn load_graph(
        &mut self,
        name: &str,
        limits: &Limits,
    ) -> Result<(Graph, StorageImage), SessionError> {
        let rows = self.load_graph_rows(name, limits.max_text_bytes)?;
        let obj = self
            .catalog
            .resolve(self.snapshot(), dict::namespace::TABLE, name)
            .map_err(error)?;
        self.ensure_recovery_object_accessible(obj.obj)?;
        let native = if obj.type_code == dict::obj_kind::GRAPH {
            Manifest::decode(
                rows.get(&0).map(Vec::as_slice),
                self.ws,
                self.catalog.file_mut().file_id() as u32,
                obj.obj,
                limits,
            )
            .map_err(error)?
        } else {
            None
        };
        let result = if let Some(manifest) = native {
            manifest
                .validate_catalog(
                    self.catalog,
                    self.engine,
                    bicdb_storage::cr::ReadView::new(self.snapshot())
                        .with_own(self.txn.as_ref().map(|txn| txn.id())),
                    limits,
                )
                .map_err(error)?;
            let mut nodes = BTreeMap::new();
            let mut proof_rows = BTreeMap::new();
            let mut ids = Vec::new();
            for (key, data) in rows {
                if key == 0 {
                    continue;
                }
                match key >> 60 {
                    1 => {
                        if key & 1023 == 0 {
                            ids.push((key & ((1 << 60) - 1)) >> 10);
                        }
                        nodes.insert(key, data);
                    }
                    3 | 4 => {}
                    5 => {
                        proof_rows.insert(key, data);
                    }
                    _ => return Err(error("non-adjacency topology in v3 graph")),
                }
            }
            let view = bicdb_storage::cr::ReadView::new(self.snapshot())
                .with_own(self.txn.as_ref().map(|t| t.id()));
            let deadline = self
                .graph_deadline
                .unwrap_or_else(|| bicdb_graph::Deadline::new(limits.max_elapsed_ms));
            let ws = self.ws;
            let file = self.catalog.file_mut();
            let edges = self
                .engine
                .with_read_context(|pool, chain| {
                    let mut port = NativeAdjacency::new(
                        pool,
                        ws,
                        manifest.records,
                        manifest.routes,
                        limits.clone(),
                        deadline,
                    )?;
                    let mut edges = BTreeMap::new();
                    for id in ids {
                        for (_, edge) in port.outgoing(file, chain, view, id, &[])? {
                            if edges.insert(edge.id, edge).is_some() {
                                return Err(bicdb_graph::Error(
                                    "duplicate native relationship identity".into(),
                                ));
                            }
                        }
                    }
                    Ok::<_, bicdb_graph::Error>(edges)
                })
                .map_err(error)?;
            let mut loaded = StorageImage::from_adjacency(
                nodes,
                edges.into_values(),
                manifest.logical_header().map_err(error)?,
                limits,
            )
            .map_err(error)?;
            loaded
                .1
                .set_proof_rows(proof_rows.clone(), limits)
                .map_err(error)?;
            manifest
                .verify_corpus(
                    &loaded.0,
                    loaded
                        .1
                        .native_record_bytes(&loaded.0, limits)
                        .map_err(error)?,
                    limits,
                )
                .map_err(error)?;
            if manifest.proof.is_some() {
                let proof = CorpusProofImage::decode_native(
                    proof_rows,
                    manifest.proof_scope(),
                    limits,
                    deadline,
                )
                .map_err(error)?;
                manifest.verify_proof(proof.root, limits).map_err(error)?;
                let expected = CorpusProofImage::build(
                    manifest.proof_scope(),
                    loaded.0.allocator_high_water(),
                    manifest
                        .stats
                        .expect("proof requires statistics")
                        .generation,
                    loaded
                        .1
                        .corpus_contributions(&loaded.0, limits)
                        .map_err(error)?,
                    limits,
                    deadline,
                )
                .map_err(error)?;
                if expected.root != proof.root || expected.records != proof.records {
                    return Err(error("independent corpus proof differs from graph records"));
                }
            }
            if manifest.stats.is_some() {
                self.engine.remember_graph_authority(
                    self.ws,
                    obj.obj,
                    self.txn.as_ref().map(|t| t.id()),
                    self.seq,
                    &manifest.encode(limits).map_err(error)?,
                );
            }
            loaded
        } else {
            StorageImage::decode(rows, limits).map_err(error)?
        };
        self.check_graph_deadline("after graph decoding")?;
        Ok(result)
    }
    fn graph_manifest(
        &mut self,
        name: &str,
        limits: &Limits,
    ) -> Result<Option<Manifest>, SessionError> {
        let obj = self
            .catalog
            .resolve(self.snapshot(), dict::namespace::TABLE, name)
            .map_err(error)?;
        self.ensure_recovery_object_accessible(obj.obj)?;
        if obj.type_code != dict::obj_kind::GRAPH {
            return Ok(None);
        }
        let results = self.graph_sql(
            &format!("SELECT ordinal,data FROM {} WHERE ordinal=0", quoted(name)),
            &[],
        )?;
        let header = Self::graph_rows_from_result(&results, 4096)?;
        Manifest::decode(
            header.get(&0).map(Vec::as_slice),
            self.ws,
            self.catalog.file_mut().file_id() as u32,
            obj.obj,
            limits,
        )
        .map_err(error)
    }
    #[allow(clippy::too_many_arguments)]
    fn plan_partial_corpus_proof(
        &mut self,
        manifest: Manifest,
        expected: CorpusRoot,
        observations: &SourceObservations,
        changes: &GraphChanges,
        after: &Graph,
        limits: &Limits,
        deadline: bicdb_graph::Deadline,
        used_work: usize,
    ) -> Result<(CorpusRoot, StoragePatch), SessionError> {
        let mut observed = Vec::new();
        for (&id, node) in &observations.nodes {
            let contribution = match node {
                Some(node) => {
                    let (lower, upper) =
                        bicdb_graph::storage::entity_range(1, id).map_err(error)?;
                    let rows: BTreeMap<_, _> = observations
                        .node_rows
                        .range(lower..=upper)
                        .map(|(key, data)| (*key, data.clone()))
                        .collect();
                    let decoded =
                        bicdb_graph::storage::decode_node(id, &rows, limits).map_err(error)?;
                    if decoded.as_ref() != Some(node) {
                        return Err(error(format!(
                            "observed node {id} rows differ from source: {decoded:?} != {node:?}"
                        )));
                    }
                    let bytes = rows.values().try_fold(0usize, |total, data| {
                        total
                            .checked_add(data.len())
                            .ok_or_else(|| error("observed node byte overflow"))
                    })?;
                    Some(CorpusContribution::node(node, bytes).map_err(error)?)
                }
                None => None,
            };
            observed.push((CorpusEntityKey::node(id), contribution));
        }
        for (&id, edge) in &observations.edges {
            observed.push((
                CorpusEntityKey::edge(id),
                edge.as_ref()
                    .map(|edge| CorpusContribution::edge(edge, limits))
                    .transpose()
                    .map_err(error)?,
            ));
        }
        let mut proof_changes = Vec::new();
        for (&id, old) in changes.nodes() {
            let key = CorpusEntityKey::node(id);
            let before = if old.is_none() {
                None
            } else {
                observed
                    .iter()
                    .find(|(candidate, _)| *candidate == key)
                    .map(|(_, value)| *value)
                    .ok_or_else(|| error("changed node lacks a source observation"))?
            };
            if before.is_some() != old.is_some() {
                return Err(error("changed node source presence differs"));
            }
            let after = after
                .nodes()
                .get(&id)
                .map(CorpusContribution::canonical_node)
                .transpose()
                .map_err(error)?;
            proof_changes.push(CorpusProofChange { key, before, after });
        }
        for (&id, old) in changes.edges() {
            let key = CorpusEntityKey::edge(id);
            let before = if old.is_none() {
                None
            } else {
                observed
                    .iter()
                    .find(|(candidate, _)| *candidate == key)
                    .map(|(_, value)| *value)
                    .ok_or_else(|| error("changed relationship lacks a source observation"))?
            };
            if before.is_some() != old.is_some() {
                return Err(error("changed relationship source presence differs"));
            }
            let after = after
                .edges()
                .get(&id)
                .map(|edge| CorpusContribution::edge(edge, limits))
                .transpose()
                .map_err(error)?;
            proof_changes.push(CorpusProofChange { key, before, after });
        }
        let generation = manifest
            .stats
            .expect("proof manifest is measured")
            .generation
            .checked_add(1)
            .ok_or_else(|| error("adjacency generation exhausted"))?;
        let view = bicdb_storage::cr::ReadView::new(self.snapshot())
            .with_own(self.txn.as_ref().map(|txn| txn.id()));
        let ws = self.ws;
        let file = self.catalog.file_mut();
        self.engine
            .with_read_context(|pool, chain| {
                let mut proof_limits = limits.clone();
                proof_limits.max_expansions = proof_limits.max_expansions.saturating_sub(used_work);
                let mut port = NativeAdjacency::new(
                    pool,
                    ws,
                    manifest.records,
                    manifest.routes,
                    proof_limits.clone(),
                    deadline,
                )?;
                let mut source = BTreeMap::new();
                let mut reader = CorpusProofReader::open(
                    manifest.proof_scope(),
                    &proof_limits,
                    deadline,
                    |key| {
                        let (lower, upper) = key.range()?;
                        let rows = port.records_range(file, chain, view, lower, upper, 1024)?;
                        let result = key.from_native_rows(&rows, &proof_limits)?;
                        source.insert(key, rows);
                        Ok(result)
                    },
                )?;
                if reader.root() != expected {
                    return Err(bicdb_graph::Error(
                        "corpus proof root changed during statement".into(),
                    ));
                }
                for (key, value) in &observed {
                    reader.verify_source(*key, *value)?;
                }
                let patch =
                    reader.patch(&proof_changes, after.allocator_high_water(), generation)?;
                let root = patch.root;
                let proof_work = reader.work();
                let proof_bytes = reader.bytes();
                drop(reader);
                let native = patch.native_patch(&source, &proof_limits)?;
                let access = port.work();
                let total_work = used_work
                    .checked_add(proof_work)
                    .and_then(|work| work.checked_add(access.units()));
                let total_bytes = proof_bytes.checked_add(access.bytes);
                if total_work.map_or(true, |work| work > limits.max_expansions)
                    || total_bytes.map_or(true, |bytes| bytes > limits.max_text_bytes)
                {
                    return Err(bicdb_graph::Error(format!(
                        "corpus proof shared work/byte budget exceeded (work {}/{}; bytes {}/{})",
                        total_work.unwrap_or(usize::MAX),
                        limits.max_expansions,
                        total_bytes.unwrap_or(usize::MAX),
                        limits.max_text_bytes
                    )));
                }
                Ok((root, native))
            })
            .map_err(error)
    }
    fn plan_native_write(
        mut manifest: Manifest,
        patch: &StoragePatch,
        before: &Graph,
        after: &Graph,
        image: &StorageImage,
        limits: &Limits,
        deadline: bicdb_graph::Deadline,
    ) -> Result<NativeWritePlan, SessionError> {
        if after.allocator_high_water() < manifest.next_id {
            return Err(error("native allocator cannot move backwards"));
        }
        let source_header = manifest.encode(limits).map_err(error)?;
        let mut nodes = patch.clone();
        nodes.removed.retain(|key| key >> 60 == 1);
        nodes.inserted.retain(|key, _| key >> 60 == 1);
        nodes.updated.retain(|key, _| key >> 60 == 1);
        let removed = before
            .edges()
            .keys()
            .filter(|id| !after.edges().contains_key(id))
            .copied()
            .collect();
        let mut inserted = Vec::new();
        let mut updated = Vec::new();
        for (id, edge) in after.edges() {
            match before.edges().get(id) {
                None => inserted.push(edge.clone()),
                Some(old) if old != edge => updated.push(edge.clone()),
                _ => {}
            }
        }
        manifest.advance(after, patch, limits).map_err(error)?;
        manifest.next_id = after.allocator_high_water();
        manifest.nodes = patch
            .after_corpus
            .map_or(after.nodes().len() as u64, |c| c.nodes);
        manifest.edges = patch
            .after_corpus
            .map_or(after.edges().len() as u64, |c| c.edges);
        let proof = if image.is_partial() {
            None
        } else {
            let stats = manifest.stats.expect("advance establishes statistics");
            let proof = CorpusProofImage::build(
                manifest.proof_scope(),
                after.allocator_high_water(),
                stats.generation,
                image
                    .final_corpus_contributions(None, after, limits)
                    .map_err(error)?,
                limits,
                deadline,
            )
            .map_err(error)?;
            manifest.install_proof(proof.root, limits).map_err(error)?;
            Some(
                proof
                    .replace_native_patch(image.proof_rows(), limits)
                    .map_err(error)?,
            )
        };
        let header = manifest.encode(limits).map_err(error)?;
        Ok(NativeWritePlan {
            manifest,
            nodes,
            removed,
            inserted,
            updated,
            proof,
            header,
            source_header,
        })
    }
    fn plan_native_changes(
        mut manifest: Manifest,
        patch: &StoragePatch,
        changes: &GraphChanges,
        after: &Graph,
        image: &StorageImage,
        proof_plan: NativeProofPlan<'_>,
    ) -> Result<NativeWritePlan, SessionError> {
        let NativeProofPlan {
            limits,
            deadline,
            partial: partial_proof,
        } = proof_plan;
        if after.allocator_high_water() < manifest.next_id {
            return Err(error("native allocator cannot move backwards"));
        }
        let source_header = manifest.encode(limits).map_err(error)?;
        let mut nodes = patch.clone();
        nodes.removed.retain(|key| key >> 60 == 1);
        nodes.inserted.retain(|key, _| key >> 60 == 1);
        nodes.updated.retain(|key, _| key >> 60 == 1);
        let mut removed = Vec::new();
        let mut inserted = Vec::new();
        let mut updated = Vec::new();
        for (id, old) in changes.edges() {
            match (old, after.edges().get(id)) {
                (Some(_), None) => removed.push(*id),
                (None, Some(edge)) => inserted.push(edge.clone()),
                (Some(_), Some(edge)) => updated.push(edge.clone()),
                (None, None) => return Err(error("empty native edge change")),
            }
        }
        manifest.advance(after, patch, limits).map_err(error)?;
        manifest.next_id = after.allocator_high_water();
        manifest.nodes = patch
            .after_corpus
            .map_or(after.nodes().len() as u64, |c| c.nodes);
        manifest.edges = patch
            .after_corpus
            .map_or(after.edges().len() as u64, |c| c.edges);
        let proof = if image.is_partial() {
            partial_proof
                .map(|(root, patch)| {
                    manifest.install_proof(root, limits).map_err(error)?;
                    Ok::<StoragePatch, SessionError>(patch)
                })
                .transpose()?
        } else {
            let stats = manifest.stats.expect("advance establishes statistics");
            let proof = CorpusProofImage::build(
                manifest.proof_scope(),
                after.allocator_high_water(),
                stats.generation,
                image
                    .final_corpus_contributions(Some(changes), after, limits)
                    .map_err(error)?,
                limits,
                deadline,
            )
            .map_err(error)?;
            manifest.install_proof(proof.root, limits).map_err(error)?;
            Some(
                proof
                    .replace_native_patch(image.proof_rows(), limits)
                    .map_err(error)?,
            )
        };
        let header = manifest.encode(limits).map_err(error)?;
        Ok(NativeWritePlan {
            manifest,
            nodes,
            removed,
            inserted,
            updated,
            proof,
            header,
            source_header,
        })
    }

    fn upgrade_graph_storage(&mut self, name: &str) -> Result<QueryResult, SessionError> {
        if self.in_transaction() {
            return Err(error("graph storage upgrade requires an idle workspace"));
        }
        let limits = self.graph_limits.clone();
        let (graph, image) = self.load_graph(name, &limits)?;
        if image.uses_adjacency() {
            return Ok(QueryResult::Ddl(format!("GRAPH {name} STORAGE V3")));
        }
        // Reject an incomplete/forged inactive set before preparatory DDL.
        ddl::graph_physical_routes(self.catalog, name)?;
        ddl::ensure_graph_storage_index(self.catalog, self.engine, name)?;
        let entries = AccessEntries::from_graph(&graph, limits.max_text_bytes).map_err(error)?;
        ddl::ensure_graph_access_indexes(
            self.catalog,
            self.engine,
            name,
            &access_builds(&entries),
        )?;
        self.seq = self.catalog.current_seq();
        let records = GraphRecords::resolve(self.catalog, name, self.snapshot()).map_err(error)?;
        let nodes = image
            .adjacency_migration_patch(&graph, &limits)
            .map_err(error)?;
        let stats = CorpusStats::measure(
            &graph,
            image.native_record_bytes(&graph, &limits).map_err(error)?,
            1,
            &limits,
        )
        .map_err(error)?;
        let ws = self.ws;
        let seq = bicdb_common::seq::CommitSeq::from_raw(self.engine.current_seq())
            .expect("engine sequence");
        let deadline = self.graph_deadline.expect("graph upgrade clock");
        ddl::prepare_graph_migration_routes(
            self.catalog,
            self.engine,
            name,
            |routes, cat, pool, log, chain, txn| {
                let view = bicdb_storage::cr::ReadView::new(seq).with_own(Some(txn.txn_id));
                let file = cat.file_mut();
                let mut manifest = Manifest::from_graph(
                    ws,
                    file.file_id() as u32,
                    records,
                    *routes,
                    &graph,
                    stats,
                );
                let proof = CorpusProofImage::build(
                    manifest.proof_scope(),
                    graph.allocator_high_water(),
                    stats.generation,
                    image
                        .corpus_contributions(&graph, &limits)
                        .map_err(|e| ddl::DdlError::BadIndexDef(e.to_string()))?,
                    &limits,
                    deadline,
                )
                .map_err(|e| ddl::DdlError::BadIndexDef(e.to_string()))?;
                manifest
                    .install_proof(proof.root, &limits)
                    .map_err(|e| ddl::DdlError::BadIndexDef(e.to_string()))?;
                let proof = proof
                    .native_patch(&limits)
                    .map_err(|e| ddl::DdlError::BadIndexDef(e.to_string()))?;
                let mut port =
                    NativeAdjacency::new(pool, ws, records, *routes, limits.clone(), deadline)
                        .map_err(|e| ddl::DdlError::BadIndexDef(e.to_string()))?;
                let migrate = (|| {
                    port.apply_records(file, log, chain, txn, view, &nodes)?;
                    port.apply_records(file, log, chain, txn, view, &proof)?;
                    for edge in graph.edges().values() {
                        port.insert(file, log, chain, txn, view, edge)?;
                    }
                    let header = StoragePatch {
                        inserted: BTreeMap::from([(0, manifest.encode(&limits)?)]),
                        ..StoragePatch::default()
                    };
                    port.apply_records(file, log, chain, txn, view, &header)
                })();
                migrate.map_err(|e| ddl::DdlError::BadIndexDef(e.to_string()))
            },
        )?;
        self.seq = self.catalog.current_seq();
        self.fulltext_cache = None;
        Ok(QueryResult::Ddl(format!(
            "ALTER GRAPH {name} UPGRADE STORAGE V3"
        )))
    }

    fn rebuild_graph_storage(&mut self, name: &str) -> Result<QueryResult, SessionError> {
        if self.in_transaction() {
            return Err(error("graph storage rebuild requires an idle workspace"));
        }
        let limits = self.graph_limits.clone();
        let (graph, image) = self.load_graph(name, &limits)?;
        if !image.uses_adjacency() {
            return Err(error(
                "upgrade legacy graph storage before rebuilding physical routes",
            ));
        }
        let old = self
            .graph_manifest(name, &limits)?
            .ok_or_else(|| error("missing native graph manifest"))?;
        let records = old.records;
        let generation = old.stats.map_or(Ok(1), |stats| {
            stats
                .generation
                .checked_add(1)
                .ok_or_else(|| error("adjacency generation exhausted"))
        })?;
        let stats = CorpusStats::measure(
            &graph,
            image.native_record_bytes(&graph, &limits).map_err(error)?,
            generation,
            &limits,
        )
        .map_err(error)?;
        let ws = self.ws;
        let seq = bicdb_common::seq::CommitSeq::from_raw(self.engine.current_seq())
            .expect("engine sequence");
        let deadline = self.graph_deadline.expect("graph rebuild clock");
        ddl::rebuild_graph_physical_routes(
            self.catalog,
            self.engine,
            name,
            |routes, cat, pool, log, chain, txn| {
                let view = bicdb_storage::cr::ReadView::new(seq).with_own(Some(txn.txn_id));
                let file = cat.file_mut();
                let mut manifest = Manifest::from_graph(
                    ws,
                    file.file_id() as u32,
                    records,
                    *routes,
                    &graph,
                    stats,
                );
                let proof = CorpusProofImage::build(
                    manifest.proof_scope(),
                    graph.allocator_high_water(),
                    stats.generation,
                    image
                        .corpus_contributions(&graph, &limits)
                        .map_err(|e| ddl::DdlError::BadIndexDef(e.to_string()))?,
                    &limits,
                    deadline,
                )
                .map_err(|e| ddl::DdlError::BadIndexDef(e.to_string()))?;
                manifest
                    .install_proof(proof.root, &limits)
                    .map_err(|e| ddl::DdlError::BadIndexDef(e.to_string()))?;
                let proof = proof
                    .native_patch(&limits)
                    .map_err(|e| ddl::DdlError::BadIndexDef(e.to_string()))?;
                let mut port =
                    NativeAdjacency::new(pool, ws, records, *routes, limits.clone(), deadline)
                        .map_err(|e| ddl::DdlError::BadIndexDef(e.to_string()))?;
                let rebuilt = (|| {
                    port.apply_records(file, log, chain, txn, view, &proof)?;
                    for edge in graph.edges().values() {
                        port.insert(file, log, chain, txn, view, edge)?;
                    }
                    let header = StoragePatch {
                        updated: BTreeMap::from([(0, manifest.encode(&limits)?)]),
                        ..StoragePatch::default()
                    };
                    port.apply_records(file, log, chain, txn, view, &header)
                })();
                rebuilt.map_err(|e| ddl::DdlError::BadIndexDef(e.to_string()))
            },
        )?;
        self.seq = self.catalog.current_seq();
        self.fulltext_cache = None;
        Ok(QueryResult::Ddl(format!(
            "ALTER GRAPH {name} REBUILD STORAGE"
        )))
    }
    fn load_graph_rows(
        &mut self,
        name: &str,
        max_bytes: usize,
    ) -> Result<BTreeMap<u64, Vec<u8>>, SessionError> {
        self.check_graph_deadline("before graph loading")?;
        let results = self.graph_sql(
            &format!("SELECT ordinal,data FROM {} ORDER BY ordinal", quoted(name)),
            &[],
        )?;
        self.check_graph_deadline("after graph loading")?;
        // Native proof metadata has an independent byte ceiling. Source decode
        // and proof decode each apply `max_bytes`; this temporary materializer
        // therefore admits at most their checked sum.
        let result = Self::graph_rows_from_result(
            &results,
            max_bytes
                .checked_mul(2)
                .ok_or_else(|| error("graph loading byte budget overflow"))?,
        )?;
        self.check_graph_deadline("after graph row materialization")?;
        Ok(result)
    }
    fn graph_rows_from_result(
        results: &[QueryResult],
        max_bytes: usize,
    ) -> Result<BTreeMap<u64, Vec<u8>>, SessionError> {
        let Some(QueryResult::Rows { rows, .. }) = results.last() else {
            return Err(error("图存储行形态非法"));
        };
        let mut image = BTreeMap::new();
        let mut bytes = 0usize;
        for row in rows {
            let [Value::Number(key), Value::Bytes(data)] = row.as_slice() else {
                return Err(error("图存储记录形态非法"));
            };
            let key: u64 = key.to_string().parse().map_err(|_| error("图存储键非法"))?;
            bytes = bytes.saturating_add(data.len());
            if bytes > max_bytes || image.insert(key, data.clone()).is_some() {
                return Err(error("图存储超过预算或存在重复键"));
            }
        }
        Ok(image)
    }
    fn persist_graph(
        &mut self,
        name: &str,
        patch: &StoragePatch,
        index_changes: &[GraphIndexChanges],
    ) -> Result<(), SessionError> {
        self.persist_record_patches(&[(name, patch)], index_changes)
    }
    fn persist_record_patches(
        &mut self,
        records: &[(&str, &StoragePatch)],
        index_changes: &[GraphIndexChanges],
    ) -> Result<(), SessionError> {
        // Raw ordinal patches cannot reconstruct native topology. All native
        // graph callers supply validated logical edge plans explicitly; these
        // generic record helpers serve auxiliary full-text/queue stores only.
        for (name, _) in records {
            if self
                .graph_manifest(name, &self.graph_limits.clone())?
                .is_some()
            {
                return Err(error(
                    "native graph requires an explicit topology publication plan",
                ));
            }
        }
        let native = records.iter().map(|_| None).collect();
        self.persist_planned_record_patches(records, index_changes, native)
    }
    fn persist_planned_record_patches(
        &mut self,
        records: &[(&str, &StoragePatch)],
        index_changes: &[GraphIndexChanges],
        native: Vec<Option<NativeWritePlan>>,
    ) -> Result<(), SessionError> {
        if records.len() != native.len() {
            return Err(error("graph publication plan width mismatch"));
        }
        self.check_graph_deadline("before native graph publication")?;
        if records.iter().all(|(_, p)| p.is_empty())
            && index_changes.iter().all(|v| v.added.is_empty())
            && native.iter().flatten().all(|plan| {
                plan.nodes.is_empty()
                    && plan.removed.is_empty()
                    && plan.inserted.is_empty()
                    && plan.updated.is_empty()
            })
        {
            return Ok(());
        }
        let owned = self.txn.is_none();
        if owned {
            self.transaction(TransactionStmtKind::Begin)?;
        }
        let mark = self
            .engine
            .statement_mark(self.txn.as_ref().expect("transaction"))?;
        let result = (|| {
            for ((name, patch), plan) in records.iter().zip(&native) {
                if plan.is_some() {
                    continue;
                }
                self.check_graph_deadline("before record patch")?;
                let name = quoted(name);
                // Batch row changes so the current DML materializer scans at most
                // once per batch, rather than once for every entity chunk. Bound
                // batch width also limits SQL AST and parameter allocations.
                for key in &patch.removed {
                    self.check_graph_deadline("before record delete batch")?;
                    let value = Value::Number(Number::parse(&key.to_string()).expect("u64"));
                    self.graph_sql(
                        &format!("DELETE FROM {name} WHERE ordinal=:k"),
                        &[("k", value)],
                    )?;
                    self.check_graph_deadline("after record delete batch")?;
                }
                for (key, data) in &patch.updated {
                    self.check_graph_deadline("before record update batch")?;
                    let key = Value::Number(Number::parse(&key.to_string()).expect("u64"));
                    let data = Value::Bytes(data.clone());
                    self.graph_sql(
                        &format!("UPDATE {name} SET data=:d WHERE ordinal=:k"),
                        &[("d", data), ("k", key)],
                    )?;
                    self.check_graph_deadline("after record update batch")?;
                }
                let inserts: Vec<_> = patch.inserted.iter().collect();
                for batch in inserts.chunks(256) {
                    self.check_graph_deadline("before record insert batch")?;
                    let mut values = vec![];
                    let mut tuples = vec![];
                    for (i, (key, data)) in batch.iter().enumerate() {
                        values.push((
                            format!("k{i}"),
                            Value::Number(Number::parse(&key.to_string()).expect("u64")),
                        ));
                        values.push((format!("d{i}"), Value::Bytes(data.to_vec())));
                        tuples.push(format!("(:k{i},:d{i})"));
                    }
                    let refs: Vec<(&str, Value)> = values
                        .iter()
                        .map(|(k, v)| (k.as_str(), v.clone()))
                        .collect();
                    self.graph_sql(
                        &format!(
                            "INSERT INTO {name} (ordinal,data) VALUES {}",
                            tuples.join(",")
                        ),
                        &refs,
                    )?;
                    self.check_graph_deadline("after record insert batch")?;
                }
            }
            let mut txn = self.txn.take().expect("graph transaction");
            let ws = self.ws;
            let deadline = self.graph_deadline;
            let limits = self.graph_limits.clone();
            let view = bicdb_storage::cr::ReadView::new(self.snapshot()).with_own(Some(txn.id()));
            let file = self.catalog.file_mut();
            let maintenance = self
                .engine
                .with_write_context(&mut txn, |pool, log, chain, txn| {
                    for plan in native.iter().flatten() {
                        let clock = deadline
                            .unwrap_or_else(|| bicdb_graph::Deadline::new(limits.max_elapsed_ms));
                        let mut port = NativeAdjacency::new(
                            pool,
                            ws,
                            plan.manifest.records,
                            plan.manifest.routes,
                            limits.clone(),
                            clock,
                        )
                        .map_err(error)?;
                        port.guard_authority(file, log, chain, txn, &plan.source_header)
                            .map_err(error)?;
                        for id in &plan.removed {
                            port.delete(file, log, chain, txn, view, *id)
                                .map_err(error)?;
                        }
                        port.apply_records(file, log, chain, txn, view, &plan.nodes)
                            .map_err(error)?;
                        for edge in &plan.updated {
                            port.update(file, log, chain, txn, view, edge)
                                .map_err(error)?;
                        }
                        for edge in &plan.inserted {
                            port.insert(file, log, chain, txn, view, edge)
                                .map_err(error)?;
                        }
                        if let Some(proof) = &plan.proof {
                            port.apply_records(file, log, chain, txn, view, proof)
                                .map_err(error)?;
                        }
                        let header = StoragePatch {
                            updated: BTreeMap::from([(0, plan.header.clone())]),
                            ..StoragePatch::default()
                        };
                        port.apply_records(file, log, chain, txn, view, &header)
                            .map_err(error)?;
                    }
                    for change in index_changes {
                        for (key, id) in &change.added {
                            if let Some(d) = deadline {
                                d.check("before native index insert").map_err(error)?;
                            }
                            bicdb_txn::write::checkpoint_safe_point(pool, log, chain)?;
                            let bytes = id.to_le_bytes();
                            let payload = bicdb_storage::rowid::RowId::from_bytes(
                                bytes[..6].try_into().expect("element ID"),
                            );
                            let root = bicdb_access::index::insert_entry(
                                pool,
                                log,
                                file,
                                ws,
                                change.block,
                                txn,
                                key,
                                payload,
                            )
                            .map_err(error)?;
                            bicdb_access::index::write_tree_head_redo(
                                pool,
                                log,
                                file,
                                ws,
                                change.block,
                                txn,
                                root,
                            )
                            .map_err(error)?;
                        }
                    }
                    Ok::<_, SessionError>(())
                });
            self.txn = Some(txn);
            maintenance?;
            // The last cancellable point is before COMMIT. Rollback below runs
            // without deadline checks, even after the clock has expired.
            self.check_graph_deadline("before native graph commit")?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                if owned {
                    self.transaction(TransactionStmtKind::Commit)?;
                }
                Ok(())
            }
            Err(main) => {
                if owned {
                    self.transaction(TransactionStmtKind::Rollback)?;
                } else {
                    let mut txn = self.txn.take().expect("transaction");
                    let rollback = self.engine.rollback_statement(&mut txn, mark);
                    self.txn = Some(txn);
                    rollback?;
                }
                Err(main)
            }
        }
    }
    pub(crate) fn graph_table_rows(
        &mut self,
        source: &crate::bind::statement::BoundGraphTable,
        remaining_edges: &mut usize,
        remaining_work: &mut usize,
    ) -> Result<Vec<Vec<Value>>, SessionError> {
        use crate::bind::statement::TableArgument;
        let argument = |arg: &TableArgument| -> Result<String, SessionError> {
            let bytes = match arg {
                TableArgument::Literal(bytes) => bytes.as_slice(),
                TableArgument::Parameter(index) => match self.exec_params.get(*index) {
                    Some(Value::Bytes(bytes)) => bytes.as_slice(),
                    _ => return Err(error("GRAPH_TABLE arguments must be non-null text")),
                },
            };
            if bytes.len() > 1024 * 1024 {
                return Err(error("GRAPH_TABLE argument exceeds 1 MiB"));
            }
            std::str::from_utf8(bytes)
                .map(str::to_owned)
                .map_err(|_| error("GRAPH_TABLE arguments must be UTF-8"))
        };
        let statement = crate::ast::CypherStmt {
            graph: source.graph.clone(),
            query: argument(&source.query)?,
            parameters: argument(&source.parameters)?,
            budgets: argument(&source.budgets)?,
            profile: false,
            location: crate::lexer::Span { start: 0, end: 0 },
        };
        match self.cypher_output(
            &statement,
            Some(&source.columns),
            Some(remaining_edges),
            Some(remaining_work),
        )? {
            QueryResult::Rows { rows, .. } => Ok(rows),
            _ => Err(error("GRAPH_TABLE did not return rows")),
        }
    }

    fn cypher(&mut self, c: &crate::ast::CypherStmt) -> Result<QueryResult, SessionError> {
        self.cypher_output(c, None, None, None)
    }

    fn cypher_output(
        &mut self,
        c: &crate::ast::CypherStmt,
        declared: Option<&[crate::bind::statement::GraphTableColumn]>,
        remaining_edges: Option<&mut usize>,
        remaining_work: Option<&mut usize>,
    ) -> Result<QueryResult, SessionError> {
        if c.budgets.len() > 4096 {
            return Err(error("BUDGETS exceeds 4096 bytes"));
        }
        let budgets =
            serde_json::from_str(&c.budgets).map_err(|_| error("BUDGETS must be valid JSON"))?;
        let limits = self
            .graph_limits
            .with_request_budgets(&budgets)
            .map_err(error)?;
        if limits.max_text_bytes < self.graph_limits.max_text_bytes {
            // A complete legacy generation cached under a wider byte ceiling
            // must not bypass this request's bounded loading path.
            self.fulltext_cache = None;
        }
        let workspace = std::mem::replace(&mut self.graph_limits, limits);
        let result = self.with_graph_deadline(self.graph_limits.max_elapsed_ms, |s| {
            s.cypher_output_scoped(c, declared, remaining_edges, remaining_work)
        });
        self.graph_limits = workspace;
        result
    }

    // Provider calls inherit this request's tighter limits as well; the wrapper
    // restores workspace policy on every success/error return.
    fn cypher_output_scoped(
        &mut self,
        c: &crate::ast::CypherStmt,
        declared: Option<&[crate::bind::statement::GraphTableColumn]>,
        remaining_edges: Option<&mut usize>,
        remaining_work: Option<&mut usize>,
    ) -> Result<QueryResult, SessionError> {
        if c.parameters.len() > 1024 * 1024 {
            return Err(error("图参数超过 1 MiB"));
        }
        self.check_graph_deadline("before Cypher parse")?;
        let query = bicdb_graph::parse(&c.query).map_err(error)?;
        self.check_graph_deadline("after Cypher parse")?;
        if declared.is_some() && !query.is_read_only() {
            return Err(error("GRAPH_TABLE only accepts read-only Cypher queries"));
        }
        let json: serde_json::Value = serde_json::from_str(&c.parameters)
            .map_err(|_| error("PARAMETERS 必须是合法 JSON 对象"))?;
        let object = json
            .as_object()
            .ok_or_else(|| error("PARAMETERS 必须是 JSON 对象"))?;
        let params: BTreeMap<String, GraphValue> = object
            .iter()
            .map(|(k, v)| Ok((k.clone(), GraphValue::from_json(v).map_err(error)?)))
            .collect::<Result<_, SessionError>>()?;
        let obj = self
            .catalog
            .resolve(self.snapshot(), dict::namespace::TABLE, &c.graph)
            .map_err(error)?;
        self.ensure_recovery_object_accessible(obj.obj)?;
        if obj.type_code != dict::obj_kind::GRAPH || obj.status != 1 {
            return Err(error("对象不是命名图"));
        }
        if c.profile && !query.is_read_only() {
            return Err(error("PROFILE CYPHER only runs read-only queries"));
        }
        let indexes = self.property_indexes(obj.obj)?;
        if !query.is_read_only() && indexes.iter().any(|i| i.status != 1) {
            return Err(error(
                "REBUILD invalid graph property indexes before writing",
            ));
        }
        self.check_graph_deadline("after graph metadata")?;
        let deadline = self.graph_deadline.expect("Cypher statement clock");
        let mut limits = self.graph_limits.clone();
        if let Some(remaining) = remaining_edges.as_deref() {
            limits.max_edge_expansions = limits.max_edge_expansions.min(*remaining);
        }
        if let Some(remaining) = remaining_work.as_deref() {
            limits.max_expansions = limits.max_expansions.min(*remaining);
        }
        let (mut trees, record) = self.access_trees(obj.obj)?;
        let mut lazy_result = None;
        let mut partial_image = None;
        let mut write_manifest = None;
        let mut ids_reserved = false;
        let mut source_reads = 0;
        if trees.len() == 3 {
            if let Some(record) = record.clone() {
                let mut reader = NativeGraphReader {
                    session: self,
                    indexes: &indexes,
                    trees: &trees,
                    record,
                    limits: &limits,
                    graph_scope: Some((c.graph.clone(), obj.obj)),
                    graph_obj: obj.obj,
                    adjacency: None,
                    native_edges: BTreeMap::new(),
                    selected_nodes: BTreeMap::new(),
                    observed_nodes: BTreeMap::new(),
                    observed_edges: BTreeMap::new(),
                    reads: 0,
                    record_reads: 0,
                    bytes: 0,
                    accesses: vec![],
                };
                let header = reader.records(0, 0).map_err(error)?;
                if let Some(mut cache) = reader
                    .snapshot_cache(header.get(&0).map(Vec::as_slice))
                    .map_err(error)?
                {
                    if header.is_empty() {
                        // A freshly created/rolled-back empty graph may have ghost
                        // tree entries, but no visible entity can lack a manifest.
                        let ids = reader.node_ids(&[]).map_err(error)?;
                        for id in ids {
                            if reader.node(id).map_err(error)?.is_some() {
                                return Err(error("missing graph manifest"));
                            }
                        }
                    }
                    cache.set_namespace(format!(
                        "{}:g{}",
                        reader
                            .session
                            .ws
                            .iter()
                            .map(|b| format!("{b:02x}"))
                            .collect::<String>(),
                        obj.obj
                    ));
                    let native = reader.adjacency;
                    if !query.is_read_only() {
                        write_manifest = native;
                    }
                    let persisted_root = if !query.is_read_only()
                        && native.is_some_and(|manifest| manifest.proof.is_some())
                    {
                        Some(
                            reader
                                .corpus_root(native.expect("proof manifest"))
                                .map_err(error)?,
                        )
                    } else {
                        None
                    };
                    let ready = persisted_root.is_some()
                        || native.is_some_and(|m| m.stats.is_some())
                            && reader.session.engine.graph_authority_validated(
                                reader.session.ws,
                                obj.obj,
                                reader.session.txn.as_ref().map(|t| t.id()),
                                reader.session.seq,
                                header.get(&0).expect("native header"),
                            );
                    if query.is_read_only() || ready {
                        let result = if query.is_read_only() {
                            bicdb_graph::execute_with_storage_deadline(
                                &mut cache,
                                &query,
                                &params,
                                &limits,
                                &mut reader,
                                deadline,
                            )
                        } else {
                            let manifest = native.expect("validated native authority");
                            let corpus = persisted_root
                                .map(|root| root.corpus(&limits))
                                .unwrap_or_else(|| manifest.corpus(&limits))
                                .map_err(error)?;
                            corpus.validate(&limits).map_err(error)?;
                            let (start, end) = ddl::reserve_graph_ids(
                                reader.session.catalog,
                                reader.session.engine,
                                obj.obj,
                                limits.max_nodes as u64 + limits.max_edges as u64,
                            )?;
                            if start < manifest.next_id {
                                return Err(error("reserved graph IDs precede source allocator"));
                            }
                            reader.session.seq = reader.session.catalog.current_seq();
                            cache.reserve_ids(start, end).map_err(error)?;
                            ids_reserved = true;
                            bicdb_graph::execute_with_storage_write_deadline(
                                &mut cache,
                                &query,
                                &params,
                                &limits,
                                &mut reader,
                                corpus,
                                deadline,
                            )
                        }
                        .map_err(error)?;
                        let observations = SourceObservations::take(&mut reader);
                        if !query.is_read_only() {
                            let manifest = native.expect("validated source");
                            let stats = manifest.stats.expect("validated statistics");
                            partial_image = Some(
                                StorageImage::partial_adjacency(
                                    std::mem::take(&mut reader.selected_nodes),
                                    std::mem::take(&mut reader.native_edges).into_values(),
                                    GraphCorpus {
                                        next_id: manifest.next_id,
                                        nodes: manifest.nodes,
                                        edges: manifest.edges,
                                        logical_bytes: stats.logical_bytes,
                                        record_bytes: stats.record_bytes,
                                    },
                                    &limits,
                                )
                                .map_err(error)?,
                            );
                        }
                        source_reads = reader.reads;
                        lazy_result = Some((
                            cache,
                            result,
                            std::mem::take(&mut reader.accesses),
                            observations,
                            persisted_root,
                        ));
                    }
                }
            }
        }
        let (mut graph, image, lazy_execution, mut observations, source_proof) = match lazy_result {
            Some((graph, result, accesses, observations, proof)) => (
                graph,
                partial_image,
                Some((result, accesses)),
                observations,
                proof,
            ),
            None => {
                if !query.is_read_only() && write_manifest.is_none() {
                    write_manifest = self.graph_manifest(&c.graph, &limits)?;
                }
                let (graph, image) = self.load_graph(&c.graph, &limits)?;
                (
                    graph,
                    Some(image),
                    None,
                    SourceObservations::default(),
                    None,
                )
            }
        };
        // Only a v1 format conversion needs the full original logical image.
        // Normal v2/v3 writes receive first-write originals from the executor.
        let before = if !query.is_read_only() && image.as_ref().is_some_and(StorageImage::is_legacy)
        {
            graph.clone()
        } else {
            Graph::new()
        };
        graph.set_namespace(format!(
            "{}:g{}",
            self.ws
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>(),
            obj.obj
        ));
        if !query.is_read_only() && !ids_reserved {
            if !self.in_transaction() {
                ddl::ensure_graph_storage_index(self.catalog, self.engine, &c.graph)?;
                if trees.len() != 3 {
                    let entries = (if image.as_ref().is_some_and(StorageImage::uses_adjacency) {
                        AccessEntries::from_nodes
                    } else {
                        AccessEntries::from_graph
                    })(&graph, limits.max_text_bytes)
                    .map_err(error)?;
                    ddl::ensure_graph_access_indexes(
                        self.catalog,
                        self.engine,
                        &c.graph,
                        &access_builds(&entries),
                    )?;
                }
                self.seq = self.catalog.current_seq();
                trees = self.access_trees(obj.obj)?.0;
            }
            // Legacy explicit transactions keep their full-image read/write
            // behavior. Missing access trees are built outside user transactions;
            // any existing protected tree still receives the entity changes.
            let (start, end) = ddl::reserve_graph_ids(
                self.catalog,
                self.engine,
                obj.obj,
                limits.max_nodes as u64 + limits.max_edges as u64,
            )?;
            self.seq = self.catalog.current_seq();
            graph.reserve_ids(start, end).map_err(error)?;
        }
        let (result, mut accesses, index_reads) = if let Some((result, accesses)) = lazy_execution {
            (result, accesses, source_reads)
        } else {
            let mut provider = NativeCypherProvider {
                session: self,
                graph: &c.graph,
                obj: obj.obj,
                indexes: &indexes,
                budget: limits.max_expansions,
                reads: 0,
                accesses: vec![],
            };
            let result = bicdb_graph::execute_with_indexes_deadline(
                &mut graph,
                &query,
                &params,
                &limits,
                &mut provider,
                deadline,
            )
            .map_err(error)?;
            (
                result,
                std::mem::take(&mut provider.accesses),
                provider.reads,
            )
        };
        if let Some(remaining) = remaining_edges {
            *remaining = remaining
                .checked_sub(result.edge_expansions)
                .ok_or_else(|| error("GRAPH_TABLE statement edge expansion budget exceeded"))?;
        }
        if let Some(remaining) = remaining_work {
            *remaining = remaining
                .checked_sub(result.expansions)
                .ok_or_else(|| error("GRAPH_TABLE statement work budget exceeded"))?;
        }
        accesses.sort_by_key(|a| match a.access {
            "PROPERTY_BTREE" => 0,
            "NODE_BTREE" => 1,
            "ADJACENCY_BTREE" => 2,
            _ => 3,
        });
        self.check_graph_deadline("after Cypher execution")?;
        if c.profile {
            let columns = [
                ("access", ColKind::Bytes),
                ("index_name", ColKind::Bytes),
                ("seeks", ColKind::Number),
                ("entries_read", ColKind::Number),
                ("candidates", ColKind::Number),
                ("rows_returned", ColKind::Number),
                ("graph_nodes_loaded", ColKind::Number),
                ("graph_edges_loaded", ColKind::Number),
                ("fulltext_source_checks", ColKind::Number),
                ("fulltext_documents_loaded", ColKind::Number),
                ("fulltext_statistics_records_loaded", ColKind::Number),
                ("edge_expansions", ColKind::Number),
                ("work_units", ColKind::Number),
                ("elapsed_ms", ColKind::Number),
            ]
            .into_iter()
            .map(|(name, kind)| ColumnMeta {
                name: name.into(),
                kind,
            })
            .collect();
            let accesses = if accesses.is_empty() {
                vec![IndexAccess::default()]
            } else {
                accesses
            };
            let rows = accesses
                .into_iter()
                .map(|a| {
                    vec![
                        Value::Bytes(if a.name.is_empty() {
                            b"SCAN".to_vec()
                        } else {
                            a.access.as_bytes().to_vec()
                        }),
                        if a.name.is_empty() {
                            Value::Null
                        } else {
                            Value::Bytes(a.name.into_bytes())
                        },
                        number(a.seeks),
                        number(a.entries),
                        number(a.candidates),
                        number(result.rows.len()),
                        number(graph.nodes().len()),
                        number(graph.edges().len()),
                        number(a.source_checks),
                        number(a.documents_loaded),
                        number(a.statistics_records_loaded),
                        number(result.edge_expansions),
                        number(result.expansions),
                        num_u64(deadline.elapsed_ms()),
                    ]
                })
                .collect();
            self.check_graph_deadline("after PROFILE formatting")?;
            return Ok(QueryResult::Rows { columns, rows });
        }
        if let Some(declared) = declared {
            if result.columns.len() != declared.len() {
                return Err(error(format!(
                    "GRAPH_TABLE RETURN has {} columns; COLUMNS declares {}",
                    result.columns.len(),
                    declared.len()
                )));
            }
            let columns = declared
                .iter()
                .map(|c| ColumnMeta {
                    name: c.name.clone(),
                    kind: c.kind,
                })
                .collect();
            let rows = result
                .rows
                .iter()
                .map(|row| {
                    row.iter()
                        .zip(declared)
                        .map(|(value, column)| match (value, column.kind) {
                            (GraphValue::Null, _) => Ok(Value::Null),
                            (GraphValue::Number(n), ColKind::Number) => {
                                Ok(Value::Number(n.clone()))
                            }
                            (GraphValue::Bool(b), ColKind::Bool) => Ok(Value::Bool(*b)),
                            (GraphValue::Node(id), ColKind::GraphElement) => {
                                Ok(Value::GraphElement(bicdb_exec::GraphElement {
                                    graph: obj.obj,
                                    kind: bicdb_exec::GraphElementKind::Node,
                                    id: *id,
                                }))
                            }
                            (GraphValue::Edge(id), ColKind::GraphElement) => {
                                Ok(Value::GraphElement(bicdb_exec::GraphElement {
                                    graph: obj.obj,
                                    kind: bicdb_exec::GraphElementKind::Edge,
                                    id: *id,
                                }))
                            }
                            (GraphValue::String(text), ColKind::Bytes)
                                if text.len() <= column.length =>
                            {
                                Ok(Value::Bytes(text.as_bytes().to_vec()))
                            }
                            (GraphValue::String(_), ColKind::Bytes) => Err(error(format!(
                                "GRAPH_TABLE column `{}` exceeds {} UTF-8 bytes",
                                column.name, column.length
                            ))),
                            _ => Err(error(format!(
                                "GRAPH_TABLE column `{}` requires {:?}; RETURN a matching value",
                                column.name, column.kind
                            ))),
                        })
                        .collect::<Result<Vec<_>, SessionError>>()
                })
                .collect::<Result<Vec<_>, _>>()?;
            self.check_graph_deadline("after GRAPH_TABLE scalar conversion")?;
            return Ok(QueryResult::Rows { columns, rows });
        }
        // Convert/validate the result before publishing changes to storage.
        // Mixed Cypher scalar columns become JSON text, preserving type per cell.
        let mut columns = vec![];
        let mut kinds = vec![];
        for (index, name) in result.columns.iter().enumerate() {
            let mut kind = None;
            let mut mixed = false;
            for row in &result.rows {
                let next = match &row[index] {
                    GraphValue::Null => continue,
                    GraphValue::Number(_) => ColKind::Number,
                    GraphValue::Bool(_) => ColKind::Bool,
                    _ => ColKind::Bytes,
                };
                if kind.is_some_and(|k| k != next) {
                    mixed = true;
                }
                kind = Some(next);
            }
            let kind = if mixed {
                ColKind::Bytes
            } else {
                kind.unwrap_or(ColKind::Bytes)
            };
            columns.push(ColumnMeta {
                name: name.clone(),
                kind,
            });
            kinds.push((kind, mixed));
        }
        let rows = result
            .rows
            .iter()
            .map(|row| {
                row.iter()
                    .zip(&kinds)
                    .map(|(v, (kind, mixed))| {
                        Ok(match v {
                            GraphValue::Null => Value::Null,
                            GraphValue::Number(n) if *kind == ColKind::Number => {
                                Value::Number(n.clone())
                            }
                            GraphValue::Bool(b) if *kind == ColKind::Bool => Value::Bool(*b),
                            GraphValue::String(s) if !mixed => Value::Bytes(s.as_bytes().to_vec()),
                            _ => {
                                Value::Bytes(serde_json::to_vec(&v.to_json(&graph)).map_err(error)?)
                            }
                        })
                    })
                    .collect::<Result<Vec<_>, SessionError>>()
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.check_graph_deadline("after Cypher result conversion")?;
        if !query.is_read_only() {
            let image = image
                .as_ref()
                .expect("write owns a verified planning image");
            let patch = if image.is_legacy() {
                image.patch(&before, &graph, &limits)
            } else {
                image.patch_changes(&result.changes, &graph, &limits)
            }
            .map_err(error)?;
            let mut index_changes = vec![];
            let mut planned_bytes = 0usize;
            // Uniqueness proof shares one remaining ceiling across all indexes,
            // after expression/traversal work and provider entry reads. Count an
            // empty seek too; ghost entries cannot make maintenance unbounded.
            let mut unique_work = result.expansions.saturating_add(index_reads);
            for index in indexes {
                let proof_reader = std::cell::RefCell::new(NativeGraphReader {
                    session: self,
                    indexes: std::slice::from_ref(&index),
                    trees: &trees,
                    record: record.clone().unwrap_or_else(|| AccessTree {
                        obj: 0,
                        kind: 0,
                        name: String::new(),
                        block: 0,
                    }),
                    limits: &limits,
                    graph_scope: Some((c.graph.clone(), obj.obj)),
                    graph_obj: obj.obj,
                    adjacency: write_manifest,
                    native_edges: BTreeMap::new(),
                    selected_nodes: BTreeMap::new(),
                    observed_nodes: BTreeMap::new(),
                    observed_edges: BTreeMap::new(),
                    reads: 0,
                    record_reads: 0,
                    bytes: 0,
                    accesses: vec![],
                });
                let shared_work = std::cell::Cell::new(unique_work);
                let (old_entries, current) = index
                    .definition
                    .changed_entries_with_source(
                        &result.changes,
                        &graph,
                        limits.max_text_bytes,
                        |_, key| {
                            let mut reader = proof_reader.borrow_mut();
                            let mut work = shared_work.get();
                            let result = reader
                                .session
                                .unique_graph_candidates(&index, key, &limits, &mut work)
                                .map(Some)
                                .map_err(|e| bicdb_graph::Error(e.to_string()));
                            shared_work.set(work);
                            result
                        },
                        |definition, id| {
                            let mut reader = proof_reader.borrow_mut();
                            let work = shared_work.get();
                            reader.reads = work;
                            let result = match definition.entity {
                                EntityKind::Node => match graph.nodes().get(&id).cloned() {
                                    Some(n) => Some(n),
                                    None if image.is_partial() => reader.node(id)?,
                                    None => None,
                                }
                                .and_then(|n| {
                                    definition
                                        .label
                                        .as_ref()
                                        .map_or(true, |l| n.labels.contains(l))
                                        .then_some(n.properties)
                                }),
                                EntityKind::Relationship => match graph.edges().get(&id).cloned() {
                                    Some(e) => Some(e),
                                    None if image.is_partial() => reader.edge(id)?,
                                    None => None,
                                }
                                .and_then(|e| {
                                    definition
                                        .label
                                        .as_ref()
                                        .map_or(true, |l| &e.label == l)
                                        .then_some(e.properties)
                                }),
                            };
                            shared_work.set(reader.reads.max(work));
                            Ok(result)
                        },
                    )
                    .map_err(error)?;
                unique_work = shared_work.get();
                let more = {
                    let mut reader = proof_reader.borrow_mut();
                    SourceObservations::take(&mut reader)
                };
                drop(proof_reader);
                observations.merge(more)?;
                self.check_graph_deadline("after unique graph candidate proof")?;
                let original: BTreeSet<_> = old_entries.into_iter().collect();
                let mut added = vec![];
                for entry in current {
                    if original.contains(&entry) {
                        continue;
                    }
                    planned_bytes = planned_bytes
                        .saturating_add(entry.0.len())
                        .saturating_add(32);
                    if planned_bytes > limits.max_text_bytes {
                        return Err(error("graph index maintenance planning budget exceeded"));
                    }
                    added.push(entry);
                }
                index_changes.push(GraphIndexChanges {
                    block: index.block,
                    added,
                });
            }
            if !trees.is_empty() {
                let (old_access, new_access) = AccessEntries::changed_entries(
                    &result.changes,
                    &graph,
                    limits.max_text_bytes,
                    !image.uses_adjacency(),
                )
                .map_err(error)?;
                for (old, new) in access_builds(&old_access)
                    .iter()
                    .zip(access_builds(&new_access))
                {
                    let originals: BTreeSet<_> = old.entries.iter().cloned().collect();
                    let mut added = vec![];
                    for entry in new.entries {
                        if originals.contains(entry) {
                            continue;
                        }
                        planned_bytes = planned_bytes
                            .saturating_add(entry.0.len())
                            .saturating_add(32);
                        if planned_bytes > limits.max_text_bytes {
                            return Err(error("graph access maintenance planning budget exceeded"));
                        }
                        added.push(entry.clone());
                    }
                    let Some(tree) = trees.iter().find(|t| t.kind == new.kind) else {
                        continue;
                    };
                    index_changes.push(GraphIndexChanges {
                        block: tree.block,
                        added,
                    });
                }
            }
            let partial_proof = if let Some(root) = source_proof {
                if result.changes.is_empty() {
                    None
                } else {
                    Some(self.plan_partial_corpus_proof(
                        write_manifest.ok_or_else(|| error("missing frozen proof manifest"))?,
                        root,
                        &observations,
                        &result.changes,
                        &graph,
                        &limits,
                        deadline,
                        unique_work,
                    )?)
                }
            } else {
                None
            };
            let queue = self.source_journal_patch(obj.obj, &result.changes, &graph)?;
            // Use only the net changed edge identities, with the same manifest
            // and statement transaction as node/index/full-text publication.
            let native = if image.uses_adjacency() {
                let manifest =
                    write_manifest.ok_or_else(|| error("missing frozen native graph authority"))?;
                Some(Self::plan_native_changes(
                    manifest,
                    &patch,
                    &result.changes,
                    &graph,
                    image,
                    NativeProofPlan {
                        limits: &limits,
                        deadline,
                        partial: partial_proof,
                    },
                )?)
            } else {
                None
            };
            let published_authority = native.as_ref().map(|p| p.header.clone());
            if let Some((name, queue_patch)) = queue {
                self.persist_planned_record_patches(
                    &[(&c.graph, &patch), (&name, &queue_patch)],
                    &index_changes,
                    vec![native, None],
                )?;
            } else {
                self.persist_planned_record_patches(
                    &[(&c.graph, &patch)],
                    &index_changes,
                    vec![native],
                )?;
            }
            if let Some(header) = published_authority {
                self.engine.remember_graph_authority(
                    self.ws,
                    obj.obj,
                    self.txn.as_ref().map(|t| t.id()),
                    self.seq,
                    &header,
                );
            }
        }
        if columns.is_empty() {
            Ok(QueryResult::Affected(result.mutations as u64))
        } else {
            Ok(QueryResult::Rows { columns, rows })
        }
    }
}
