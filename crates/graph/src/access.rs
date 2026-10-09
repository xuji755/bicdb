//! Persistent directory/adjacency keys and the read-only storage access port.
use crate::model::fail;
use crate::property_index::IndexProvider;
use crate::{Direction, Edge, Error, Graph, GraphChanges, Node};

/// A statement-scoped snapshot reader. IDs may include stale index entries;
/// the executor fetches/rechecks current entities and never truncates silently.
pub trait GraphAccess: IndexProvider {
    fn node(&mut self, id: u64) -> Result<Option<Node>, Error>;
    fn edge(&mut self, id: u64) -> Result<Option<Edge>, Error>;
    fn node_ids(&mut self, labels: &[String]) -> Result<Vec<u64>, Error>;
    fn edge_ids(
        &mut self,
        node: u64,
        direction: Direction,
        types: &[String],
    ) -> Result<Vec<u64>, Error>;
}
/// Empty label means all nodes; actual labels use a separate tagged key domain.
pub fn label_key(label: Option<&str>) -> Vec<u8> {
    let Some(label) = label else {
        return vec![0];
    };
    let mut out = vec![1];
    string_component(&mut out, label);
    out
}
fn string_component(out: &mut Vec<u8>, s: &str) {
    for b in s.bytes() {
        if b == 0 {
            out.extend([0, 255]);
        } else {
            out.push(b);
        }
    }
    out.extend([0, 0]);
}
pub fn adjacency_prefix(node: u64, kind: Option<&str>) -> Vec<u8> {
    let mut out = node.to_be_bytes()[2..].to_vec();
    if let Some(kind) = kind {
        string_component(&mut out, kind);
    }
    out
}
/// Three append-only trees: global/per-label node IDs, typed outgoing edge IDs,
/// typed incoming edge IDs. Changes/deletes are rechecked against entity rows.
#[derive(Default)]
pub struct AccessEntries {
    pub nodes: Vec<(Vec<u8>, u64)>,
    pub outgoing: Vec<(Vec<u8>, u64)>,
    pub incoming: Vec<(Vec<u8>, u64)>,
}
impl AccessEntries {
    pub fn from_graph(graph: &Graph, max_bytes: usize) -> Result<Self, Error> {
        Self::build(graph, max_bytes, true)
    }
    /// V3 topology is maintained by physical routes, so only node directory
    /// entries are needed. Long relationship types never enter legacy keys.
    pub fn from_nodes(graph: &Graph, max_bytes: usize) -> Result<Self, Error> {
        Self::build(graph, max_bytes, false)
    }
    fn build(graph: &Graph, max_bytes: usize, with_edges: bool) -> Result<Self, Error> {
        Self::build_entities(
            graph.nodes().values(),
            graph.edges().values(),
            max_bytes,
            with_edges,
        )
    }
    /// Old/new directory keys for only the net entities changed by one query.
    pub fn changed_entries(
        changes: &GraphChanges,
        after: &Graph,
        max_bytes: usize,
        with_edges: bool,
    ) -> Result<(Self, Self), Error> {
        let old = Self::build_entities(
            changes.nodes().values().filter_map(Option::as_ref),
            changes.edges().values().filter_map(Option::as_ref),
            max_bytes,
            with_edges,
        )?;
        let new = Self::build_entities(
            changes
                .nodes()
                .keys()
                .filter_map(|id| after.nodes().get(id)),
            changes
                .edges()
                .keys()
                .filter_map(|id| after.edges().get(id)),
            max_bytes,
            with_edges,
        )?;
        Ok((old, new))
    }
    fn build_entities<'a>(
        nodes: impl Iterator<Item = &'a Node>,
        edges: impl Iterator<Item = &'a Edge>,
        max_bytes: usize,
        with_edges: bool,
    ) -> Result<Self, Error> {
        let mut out = Self::default();
        let mut bytes = 0usize;
        let mut append = |entries: &mut Vec<(Vec<u8>, u64)>, key: Vec<u8>, id| {
            bytes = bytes.saturating_add(key.len()).saturating_add(32);
            if key.len() > 4088 || bytes > max_bytes {
                return Err(fail("graph directory/adjacency key budget exceeded"));
            }
            entries.push((key, id));
            Ok(())
        };
        for n in nodes {
            append(&mut out.nodes, label_key(None), n.id)?;
            for l in &n.labels {
                append(&mut out.nodes, label_key(Some(l)), n.id)?;
            }
        }
        for e in edges.filter(|_| with_edges) {
            append(
                &mut out.outgoing,
                adjacency_prefix(e.source, Some(&e.label)),
                e.id,
            )?;
            append(
                &mut out.incoming,
                adjacency_prefix(e.target, Some(&e.label)),
                e.id,
            )?;
        }
        out.nodes.sort();
        out.outgoing.sort();
        out.incoming.sort();
        Ok(out)
    }
}
