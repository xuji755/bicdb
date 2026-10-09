use bicdb_types::Number;
use serde_json::{Map, Value as Json};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;

pub type Properties = BTreeMap<String, Json>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Error {}
pub(crate) fn fail(message: impl Into<String>) -> Error {
    Error(message.into())
}

#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub id: u64,
    pub labels: BTreeSet<String>,
    pub properties: Properties,
}
#[derive(Debug, Clone, PartialEq)]
pub struct Edge {
    pub id: u64,
    pub source: u64,
    pub target: u64,
    pub label: String,
    pub properties: Properties,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Path {
    pub nodes: Vec<u64>,
    pub edges: Vec<u64>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Out,
    In,
    Both,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(Number),
    String(String),
    Node(u64),
    Edge(u64),
    List(Vec<Value>),
    Map(BTreeMap<String, Value>),
    Path(Path),
}
impl Value {
    pub fn from_json(value: &Json) -> Result<Self, Error> {
        Ok(match value {
            Json::Null => Self::Null,
            Json::Bool(v) => Self::Bool(*v),
            Json::Number(n) => Self::Number(decimal(&n.to_string())?),
            Json::String(s) => Self::String(s.clone()),
            Json::Array(a) => Self::List(a.iter().map(Self::from_json).collect::<Result<_, _>>()?),
            Json::Object(o) => Self::Map(
                o.iter()
                    .map(|(k, v)| Ok((k.clone(), Self::from_json(v)?)))
                    .collect::<Result<_, Error>>()?,
            ),
        })
    }
    pub fn integer(value: u64) -> Self {
        Self::Number(Number::parse(&value.to_string()).expect("u64 fits NUMBER"))
    }
    pub fn as_usize(&self) -> Result<usize, Error> {
        if let Self::Number(n) = self {
            return n
                .to_string()
                .parse()
                .map_err(|_| fail("nonnegative integer required"));
        }
        Err(fail("nonnegative integer required"))
    }
    pub fn to_json(&self, graph: &Graph) -> Json {
        match self {
            Self::Null => Json::Null,
            Self::Bool(b) => Json::Bool(*b),
            Self::Number(n) => serde_json::from_str(&n.to_string()).expect("NUMBER emits JSON decimal"),
            Self::String(s) => Json::String(s.clone()),
            Self::List(a) => Json::Array(a.iter().map(|v| v.to_json(graph)).collect()),
            Self::Map(m) => Json::Object(m.iter().map(|(k,v)| (k.clone(),v.to_json(graph))).collect()),
            Self::Node(id) => graph.nodes.get(id).map_or(Json::Null, |n| serde_json::json!({"id":id,"element_id":graph.element_id('n', *id),"labels":n.labels,"properties":n.properties})),
            Self::Edge(id) => graph.edges.get(id).map_or(Json::Null, |e| serde_json::json!({"id":id,"element_id":graph.element_id('e', *id),"source":e.source,"target":e.target,"type":e.label,"properties":e.properties})),
            Self::Path(p) => serde_json::json!({"nodes":p.nodes.iter().map(|id| Self::Node(*id).to_json(graph)).collect::<Vec<_>>(),"relationships":p.edges.iter().map(|id|Self::Edge(*id).to_json(graph)).collect::<Vec<_>>() }),
        }
    }
    pub(crate) fn truth(&self) -> Result<Option<bool>, Error> {
        match self {
            Self::Null => Ok(None),
            Self::Bool(b) => Ok(Some(*b)),
            _ => Err(fail("predicate must be BOOLEAN or null")),
        }
    }
}

pub(crate) fn decimal(text: &str) -> Result<Number, Error> {
    let expanded;
    let value = if let Some((mantissa, exponent)) = text.split_once(['e', 'E']) {
        let e: i32 = exponent
            .parse()
            .map_err(|_| fail("invalid numeric exponent"))?;
        if !(-130..=125).contains(&e) {
            return Err(fail("numeric exponent out of range"));
        }
        let negative = mantissa.starts_with('-');
        let m = mantissa.trim_start_matches(['-', '+']);
        let point = m.find('.').unwrap_or(m.len()) as i32 + e;
        let digits = m.replace('.', "");
        let body = if point <= 0 {
            format!("0.{}{}", "0".repeat((-point) as usize), digits)
        } else if point as usize >= digits.len() {
            format!("{}{}", digits, "0".repeat(point as usize - digits.len()))
        } else {
            format!(
                "{}.{}",
                &digits[..point as usize],
                &digits[point as usize..]
            )
        };
        expanded = format!("{}{body}", if negative { "-" } else { "" });
        expanded.as_str()
    } else {
        text
    };
    Number::parse(value).map_err(|e| fail(e.to_string()))
}

/// Default statement-wide number of incident relationships removed by DETACH.
pub const DEFAULT_DETACH_EDGE_LIMIT: usize = 10_000;
/// Native graph SQL's configured ceiling; the normal graph edge bound is 500k.
pub const MAX_DETACH_EDGE_LIMIT: usize = 500_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    pub max_nodes: usize,
    pub max_edges: usize,
    /// Distinct incident relationships removed by DETACH across the entire query.
    pub max_detach_edges: usize,
    pub max_rows: usize,
    pub max_expansions: usize,
    /// Adjacency candidates examined, including repeated visits and DETACH incidence.
    pub max_edge_expansions: usize,
    /// Cooperative statement duration, including native loading/publication preparation.
    pub max_elapsed_ms: usize,
    pub max_depth: usize,
    pub max_text_bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_nodes: 100_000,
            max_edges: 500_000,
            max_detach_edges: DEFAULT_DETACH_EDGE_LIMIT,
            max_rows: 10_000,
            max_expansions: 100_000,
            max_edge_expansions: 100_000,
            max_elapsed_ms: 60_000,
            max_depth: 16,
            max_text_bytes: 100 * 1024 * 1024,
        }
    }
}

impl Limits {
    /// Validate workspace ceilings against the native implementation's hard
    /// limits. Lower ceilings reject work; they never truncate a graph image.
    pub fn validate_workspace(&self) -> Result<(), Error> {
        let hard = Self::default();
        for (name, value, minimum, maximum) in [
            ("max_nodes", self.max_nodes, 0, hard.max_nodes),
            ("max_edges", self.max_edges, 0, hard.max_edges),
            (
                "detach_edge_limit",
                self.max_detach_edges,
                0,
                MAX_DETACH_EDGE_LIMIT,
            ),
            ("max_rows", self.max_rows, 1, hard.max_rows),
            (
                "max_expansions",
                self.max_expansions,
                1,
                hard.max_expansions,
            ),
            (
                "max_edge_expansions",
                self.max_edge_expansions,
                0,
                hard.max_edge_expansions,
            ),
            (
                "max_elapsed_ms",
                self.max_elapsed_ms,
                1,
                hard.max_elapsed_ms,
            ),
            ("max_depth", self.max_depth, 1, hard.max_depth),
            (
                "max_text_bytes",
                self.max_text_bytes,
                1024,
                hard.max_text_bytes,
            ),
        ] {
            if !(minimum..=maximum).contains(&value) {
                return Err(fail(format!("graph.{name} must be {minimum}..={maximum}")));
            }
        }
        Ok(())
    }

    /// A request can tighten execution budgets but cannot change corpus limits
    /// or increase any workspace ceiling. Unknown fields are rejected.
    pub fn with_request_budgets(&self, value: &Json) -> Result<Self, Error> {
        self.validate_workspace()?;
        let object = value
            .as_object()
            .ok_or_else(|| fail("BUDGETS must be a JSON object"))?;
        let mut result = self.clone();
        for (name, raw) in object {
            let (target, minimum, maximum) = match name.as_str() {
                "max_rows" => (&mut result.max_rows, 1, self.max_rows),
                "max_expansions" => (&mut result.max_expansions, 1, self.max_expansions),
                "max_edge_expansions" => {
                    (&mut result.max_edge_expansions, 0, self.max_edge_expansions)
                }
                "max_elapsed_ms" => (&mut result.max_elapsed_ms, 1, self.max_elapsed_ms),
                "max_depth" => (&mut result.max_depth, 1, self.max_depth),
                "max_text_bytes" => (&mut result.max_text_bytes, 1024, self.max_text_bytes),
                "detach_edge_limit" => (&mut result.max_detach_edges, 0, self.max_detach_edges),
                _ => {
                    return Err(fail(format!(
                        "BUDGETS unknown graph request budget `{name}`"
                    )))
                }
            };
            let number = raw.as_u64().and_then(|v| usize::try_from(v).ok())
                .filter(|n| (minimum..=maximum).contains(n))
                .ok_or_else(|| fail(format!("BUDGETS {name} must be an integer {minimum}..={maximum} (workspace ceiling)")))?;
            *target = number;
        }
        Ok(result)
    }
}

/// First-write originals for the net entity changes of one successful query.
/// Payloads are retained only for touched entities; reads never enter this set.
/// This is a statement delta, not a complete graph or a persistence authority.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GraphChanges {
    nodes: BTreeMap<u64, Option<Node>>,
    edges: BTreeMap<u64, Option<Edge>>,
}
impl GraphChanges {
    pub fn nodes(&self) -> &BTreeMap<u64, Option<Node>> {
        &self.nodes
    }
    pub fn edges(&self) -> &BTreeMap<u64, Option<Edge>> {
        &self.edges
    }
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty() && self.edges.is_empty()
    }
}
/// Validated complete source authority, never the size of a partial cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraphCorpus {
    pub next_id: u64,
    pub nodes: u64,
    pub edges: u64,
    pub logical_bytes: u64,
    pub record_bytes: u64,
}
impl GraphCorpus {
    pub fn validate(&self, limits: &Limits) -> Result<(), Error> {
        if self.next_id == 0
            || self.next_id > 1 << 48
            || self.nodes > limits.max_nodes as u64
            || self.edges > limits.max_edges as u64
        {
            return Err(fail("global graph element/allocator budget exceeded"));
        }
        if self.logical_bytes > limits.max_text_bytes as u64
            || self.record_bytes > limits.max_text_bytes as u64
        {
            return Err(fail("global graph storage byte budget exceeded"));
        }
        Ok(())
    }
    /// Exact canonical JSON delta, including header digit widths and commas.
    /// First-write originals may include reverted edits; those cancel exactly.
    pub fn changed(
        &self,
        changes: &GraphChanges,
        after: &Graph,
        limits: &Limits,
    ) -> Result<Self, Error> {
        let fail_size = || fail("invalid global graph corpus delta");
        let header = |next_id| {
            serde_json::to_vec(&serde_json::json!({"format":"bicdb-graph-v1",
            "next_id":next_id,"nodes":[],"edges":[]}))
            .map(|b| b.len() as u64)
            .map_err(|e| fail(e.to_string()))
        };
        let mut result = *self;
        let mut bytes = self
            .logical_bytes
            .checked_sub(header(self.next_id)?)
            .and_then(|n| n.checked_sub(self.nodes.saturating_sub(1)))
            .and_then(|n| n.checked_sub(self.edges.saturating_sub(1)))
            .ok_or_else(fail_size)?;
        let mut delta = |old: Option<serde_json::Value>,
                         new: Option<serde_json::Value>,
                         count: &mut u64|
         -> Result<(), Error> {
            if let Some(old) = old {
                *count = count.checked_sub(1).ok_or_else(fail_size)?;
                bytes = bytes
                    .checked_sub(
                        serde_json::to_vec(&old)
                            .map_err(|e| fail(e.to_string()))?
                            .len() as u64,
                    )
                    .ok_or_else(fail_size)?;
            }
            if let Some(new) = new {
                *count = count.checked_add(1).ok_or_else(fail_size)?;
                bytes = bytes
                    .checked_add(
                        serde_json::to_vec(&new)
                            .map_err(|e| fail(e.to_string()))?
                            .len() as u64,
                    )
                    .ok_or_else(fail_size)?;
            }
            Ok(())
        };
        let node =
            |n: &Node| serde_json::json!({"id":n.id,"labels":n.labels,"properties":n.properties});
        let edge = |e: &Edge| serde_json::json!({"id":e.id,"source":e.source,"target":e.target,"label":e.label,"properties":e.properties});
        for (id, old) in changes.nodes() {
            delta(
                old.as_ref().map(node),
                after.nodes().get(id).map(node),
                &mut result.nodes,
            )?;
        }
        for (id, old) in changes.edges() {
            delta(
                old.as_ref().map(edge),
                after.edges().get(id).map(edge),
                &mut result.edges,
            )?;
        }
        result.next_id = after.allocator_high_water();
        if result.next_id < self.next_id {
            return Err(fail("global graph allocator cannot move backwards"));
        }
        result.logical_bytes = bytes
            .checked_add(header(result.next_id)?)
            .and_then(|n| n.checked_add(result.nodes.saturating_sub(1)))
            .and_then(|n| n.checked_add(result.edges.saturating_sub(1)))
            .ok_or_else(fail_size)?;
        // Native record bytes are filled by the independently verified row patch.
        result.validate(limits)?;
        Ok(result)
    }
}
#[derive(Debug, Clone, PartialEq)]
struct WriteJournal {
    changes: GraphChanges,
    next_id: u64,
    id_end: u64,
    label_presence: BTreeMap<String, bool>,
    outgoing_presence: BTreeMap<u64, bool>,
    incoming_presence: BTreeMap<u64, bool>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Graph {
    pub(crate) nodes: BTreeMap<u64, Node>,
    pub(crate) edges: BTreeMap<u64, Edge>,
    next_id: u64,
    id_end: u64,
    namespace: String,
    outgoing: BTreeMap<u64, BTreeSet<u64>>,
    incoming: BTreeMap<u64, BTreeSet<u64>>,
    labels: BTreeMap<String, BTreeSet<u64>>,
    write_journal: Option<WriteJournal>,
}
impl Default for Graph {
    fn default() -> Self {
        Self::new()
    }
}
impl Graph {
    pub fn new() -> Self {
        Self {
            nodes: BTreeMap::new(),
            edges: BTreeMap::new(),
            next_id: 1,
            id_end: (1 << 48) - 1,
            namespace: "local".into(),
            outgoing: BTreeMap::new(),
            incoming: BTreeMap::new(),
            labels: BTreeMap::new(),
            write_journal: None,
        }
    }
    pub(crate) fn begin_write(&mut self) -> Result<(), Error> {
        if self.write_journal.is_some() {
            return Err(fail("nested graph write journal"));
        }
        self.write_journal = Some(WriteJournal {
            changes: GraphChanges::default(),
            next_id: self.next_id,
            id_end: self.id_end,
            label_presence: BTreeMap::new(),
            outgoing_presence: BTreeMap::new(),
            incoming_presence: BTreeMap::new(),
        });
        Ok(())
    }
    fn remember_label(&mut self, label: &str) {
        if let Some(journal) = &mut self.write_journal {
            journal
                .label_presence
                .entry(label.to_owned())
                .or_insert_with(|| self.labels.contains_key(label));
        }
    }
    fn remember_endpoints(&mut self, source: u64, target: u64) {
        if let Some(journal) = &mut self.write_journal {
            journal
                .outgoing_presence
                .entry(source)
                .or_insert_with(|| self.outgoing.contains_key(&source));
            journal
                .incoming_presence
                .entry(target)
                .or_insert_with(|| self.incoming.contains_key(&target));
        }
    }
    fn touch_node(&mut self, id: u64) {
        if let Some(journal) = &mut self.write_journal {
            journal
                .changes
                .nodes
                .entry(id)
                .or_insert_with(|| self.nodes.get(&id).cloned());
            if let Some(node) = self.nodes.get(&id) {
                for label in &node.labels {
                    journal
                        .label_presence
                        .entry(label.clone())
                        .or_insert_with(|| self.labels.contains_key(label));
                }
            }
        }
    }
    fn touch_edge(&mut self, id: u64) {
        if let Some(journal) = &mut self.write_journal {
            journal
                .changes
                .edges
                .entry(id)
                .or_insert_with(|| self.edges.get(&id).cloned());
            if let Some(edge) = self.edges.get(&id) {
                journal
                    .outgoing_presence
                    .entry(edge.source)
                    .or_insert_with(|| self.outgoing.contains_key(&edge.source));
                journal
                    .incoming_presence
                    .entry(edge.target)
                    .or_insert_with(|| self.incoming.contains_key(&edge.target));
            }
        }
    }
    pub(crate) fn node_mut(&mut self, id: u64) -> Result<&mut Node, Error> {
        self.touch_node(id);
        self.nodes.get_mut(&id).ok_or_else(|| fail("deleted node"))
    }
    pub(crate) fn edge_mut(&mut self, id: u64) -> Result<&mut Edge, Error> {
        self.touch_edge(id);
        self.edges
            .get_mut(&id)
            .ok_or_else(|| fail("deleted relationship"))
    }
    pub(crate) fn update_label(&mut self, id: u64, label: &str, add: bool) -> Result<(), Error> {
        self.remember_label(label);
        let node = self.node_mut(id)?;
        if add {
            if node.labels.insert(label.to_owned()) {
                self.labels.entry(label.to_owned()).or_default().insert(id);
            }
        } else if node.labels.remove(label) {
            if let Some(ids) = self.labels.get_mut(label) {
                ids.remove(&id);
            }
        }
        Ok(())
    }
    pub(crate) fn staged_changes(&self) -> &GraphChanges {
        &self
            .write_journal
            .as_ref()
            .expect("active write journal")
            .changes
    }
    /// Materialize a complete source only when an operation needs the complete
    /// staged corpus (e.g. full-text ranking after mutation). Do not serialize a
    /// partial graph as if it were complete source authority.
    pub fn overlay_source(&self, source: &Graph) -> Result<Graph, Error> {
        let changes = self
            .write_journal
            .as_ref()
            .ok_or_else(|| fail("overlay requires active write journal"))?;
        let mut graph = source.clone();
        for id in changes.changes.edges.keys() {
            graph.edges.remove(id);
        }
        for id in changes.changes.nodes.keys() {
            graph.nodes.remove(id);
        }
        for id in changes.changes.nodes.keys() {
            if let Some(node) = self.nodes.get(id) {
                graph.nodes.insert(*id, node.clone());
            }
        }
        for id in changes.changes.edges.keys() {
            if let Some(edge) = self.edges.get(id) {
                graph.edges.insert(*id, edge.clone());
            }
        }
        graph.labels.clear();
        graph.outgoing.clear();
        graph.incoming.clear();
        for node in graph.nodes.values() {
            for label in &node.labels {
                graph
                    .labels
                    .entry(label.clone())
                    .or_default()
                    .insert(node.id);
            }
        }
        for edge in graph.edges.values() {
            if !graph.nodes.contains_key(&edge.source) || !graph.nodes.contains_key(&edge.target) {
                return Err(fail("overlay leaves missing endpoint"));
            }
            graph
                .outgoing
                .entry(edge.source)
                .or_default()
                .insert(edge.id);
            graph
                .incoming
                .entry(edge.target)
                .or_default()
                .insert(edge.id);
        }
        graph.next_id = self.next_id;
        graph.id_end = self.id_end;
        graph.namespace = self.namespace.clone();
        Ok(graph)
    }
    pub(crate) fn finish_write(&mut self) -> GraphChanges {
        let mut changes = self
            .write_journal
            .take()
            .expect("graph write journal")
            .changes;
        changes
            .nodes
            .retain(|id, old| old.as_ref() != self.nodes.get(id));
        changes
            .edges
            .retain(|id, old| old.as_ref() != self.edges.get(id));
        changes
    }
    pub(crate) fn rollback_write(&mut self) {
        let journal = self.write_journal.take().expect("graph write journal");
        for (id, original) in journal.changes.nodes {
            if let Some(current) = self.nodes.remove(&id) {
                for label in current.labels {
                    if let Some(ids) = self.labels.get_mut(&label) {
                        ids.remove(&id);
                    }
                }
            }
            if let Some(node) = original {
                for label in &node.labels {
                    self.labels.entry(label.clone()).or_default().insert(id);
                }
                self.nodes.insert(id, node);
            }
        }
        for (id, original) in journal.changes.edges {
            if let Some(current) = self.edges.remove(&id) {
                if let Some(ids) = self.outgoing.get_mut(&current.source) {
                    ids.remove(&id);
                }
                if let Some(ids) = self.incoming.get_mut(&current.target) {
                    ids.remove(&id);
                }
            }
            if let Some(edge) = original {
                self.outgoing.entry(edge.source).or_default().insert(id);
                self.incoming.entry(edge.target).or_default().insert(id);
                self.edges.insert(id, edge);
            }
        }
        // Preserve even preexisting empty buckets without cloning whole incident
        // sets. Buckets first introduced by a failed query are removed again.
        for (label, existed) in journal.label_presence {
            if !existed {
                self.labels.remove(&label);
            }
        }
        for (node, existed) in journal.outgoing_presence {
            if !existed {
                self.outgoing.remove(&node);
            }
        }
        for (node, existed) in journal.incoming_presence {
            if !existed {
                self.incoming.remove(&node);
            }
        }
        self.next_id = journal.next_id;
        self.id_end = journal.id_end;
    }
    pub fn nodes(&self) -> &BTreeMap<u64, Node> {
        &self.nodes
    }
    pub fn edges(&self) -> &BTreeMap<u64, Edge> {
        &self.edges
    }
    pub fn allocator_high_water(&self) -> u64 {
        self.next_id
    }
    pub(crate) fn finish_load(&mut self, next_id: u64) -> Result<(), Error> {
        if next_id == 0
            || next_id > 1 << 48
            || self
                .nodes
                .keys()
                .chain(self.edges.keys())
                .any(|id| *id >= next_id)
        {
            return Err(fail("invalid graph allocator high water"));
        }
        self.next_id = next_id;
        self.id_end = (1 << 48) - 1;
        Ok(())
    }
    /// Import one validated snapshot entity without allocating or rewriting IDs.
    pub(crate) fn cache_node(&mut self, node: Node) -> Result<(), Error> {
        if node.id == 0 || node.id >= self.next_id || self.edges.contains_key(&node.id) {
            return Err(fail("invalid cached node identity"));
        }
        validate_properties(&node.properties)?;
        for label in &node.labels {
            self.labels
                .entry(label.clone())
                .or_default()
                .insert(node.id);
        }
        self.nodes.insert(node.id, node);
        Ok(())
    }
    pub(crate) fn cache_edge(&mut self, edge: Edge) -> Result<(), Error> {
        if edge.id == 0
            || edge.id >= self.next_id
            || self.nodes.contains_key(&edge.id)
            || edge.label.is_empty()
            || !self.nodes.contains_key(&edge.source)
            || !self.nodes.contains_key(&edge.target)
        {
            return Err(fail("invalid cached relationship or endpoint"));
        }
        validate_properties(&edge.properties)?;
        self.outgoing
            .entry(edge.source)
            .or_default()
            .insert(edge.id);
        self.incoming
            .entry(edge.target)
            .or_default()
            .insert(edge.id);
        self.edges.insert(edge.id, edge);
        Ok(())
    }
    pub fn set_namespace(&mut self, namespace: String) {
        self.namespace = namespace;
    }
    pub fn reserve_ids(&mut self, start: u64, end: u64) -> Result<(), Error> {
        if start == 0 || start > end || end >= 1 << 48 {
            return Err(fail("invalid graph ID reservation"));
        }
        self.next_id = start;
        self.id_end = end;
        Ok(())
    }
    fn allocate(&mut self) -> Result<u64, Error> {
        if self.next_id > self.id_end {
            return Err(fail("graph ID reservation exhausted"));
        }
        let id = self.next_id;
        self.next_id += 1;
        if self.nodes.contains_key(&id) || self.edges.contains_key(&id) {
            return Err(fail("graph ID collision"));
        }
        Ok(id)
    }
    pub fn element_id(&self, kind: char, id: u64) -> String {
        format!("{}:{kind}:{id}", self.namespace)
    }
    pub fn add_node(
        &mut self,
        labels: BTreeSet<String>,
        properties: Properties,
    ) -> Result<u64, Error> {
        validate_properties(&properties)?;
        let id = self.allocate()?;
        self.touch_node(id);
        for label in &labels {
            self.remember_label(label);
        }
        self.nodes.insert(
            id,
            Node {
                id,
                labels: labels.clone(),
                properties,
            },
        );
        for label in labels {
            self.labels.entry(label).or_default().insert(id);
        }
        Ok(id)
    }
    pub fn add_edge(
        &mut self,
        source: u64,
        target: u64,
        label: String,
        properties: Properties,
    ) -> Result<u64, Error> {
        if !self.nodes.contains_key(&source) || !self.nodes.contains_key(&target) {
            return Err(fail("edge endpoint does not exist"));
        }
        if label.is_empty() {
            return Err(fail("relationship type required"));
        }
        validate_properties(&properties)?;
        let id = self.allocate()?;
        self.touch_edge(id);
        self.remember_endpoints(source, target);
        self.edges.insert(
            id,
            Edge {
                id,
                source,
                target,
                label,
                properties,
            },
        );
        self.outgoing.entry(source).or_default().insert(id);
        self.incoming.entry(target).or_default().insert(id);
        Ok(id)
    }
    pub fn remove_edge(&mut self, id: u64) {
        self.touch_edge(id);
        if let Some(e) = self.edges.remove(&id) {
            if let Some(s) = self.outgoing.get_mut(&e.source) {
                s.remove(&id);
            }
            if let Some(s) = self.incoming.get_mut(&e.target) {
                s.remove(&id);
            }
        }
    }
    /// Iterate incidence keys without materializing a high-degree node's whole
    /// neighborhood. Callers deduplicate self-loops and shared endpoints while
    /// enforcing their budget, and stop at the first excess relationship.
    pub(crate) fn incident_edge_ids(&self, id: u64) -> impl Iterator<Item = &u64> {
        self.outgoing
            .get(&id)
            .into_iter()
            .flatten()
            .chain(self.incoming.get(&id).into_iter().flatten())
    }
    pub fn remove_node(&mut self, id: u64, detach: bool) -> Result<(), Error> {
        let edges = self.neighbors(id, Direction::Both, &[]);
        if !detach && !edges.is_empty() {
            return Err(fail("node still has relationships; use DETACH DELETE"));
        }
        for (edge, _) in edges {
            self.remove_edge(edge);
        }
        self.touch_node(id);
        if let Some(n) = self.nodes.remove(&id) {
            for label in n.labels {
                if let Some(ids) = self.labels.get_mut(&label) {
                    ids.remove(&id);
                }
            }
        }
        Ok(())
    }
    pub(crate) fn candidates(&self, labels: &[String]) -> Vec<u64> {
        if labels.is_empty() {
            return self.nodes.keys().copied().collect();
        }
        let smallest = labels
            .iter()
            .filter_map(|l| self.labels.get(l))
            .min_by_key(|s| s.len());
        if labels.iter().any(|l| !self.labels.contains_key(l)) {
            return vec![];
        }
        smallest
            .into_iter()
            .flatten()
            .filter(|id| labels.iter().all(|l| self.nodes[id].labels.contains(l)))
            .copied()
            .collect()
    }
    pub fn neighbors(&self, id: u64, direction: Direction, types: &[String]) -> Vec<(u64, u64)> {
        let mut ids = BTreeSet::new();
        if direction != Direction::In {
            ids.extend(self.outgoing.get(&id).into_iter().flatten().copied());
        }
        if direction != Direction::Out {
            ids.extend(self.incoming.get(&id).into_iter().flatten().copied());
        }
        ids.into_iter()
            .filter_map(|eid| {
                let e = &self.edges[&eid];
                if !types.is_empty() && !types.contains(&e.label) {
                    return None;
                }
                Some((eid, if e.source == id { e.target } else { e.source }))
            })
            .collect()
    }
    pub fn shortest_path(
        &self,
        source: u64,
        target: u64,
        direction: Direction,
        types: &[String],
        limits: &Limits,
    ) -> Result<Option<Path>, Error> {
        if !self.nodes.contains_key(&source) || !self.nodes.contains_key(&target) {
            return Ok(None);
        }
        let deadline = crate::Deadline::for_limits(limits);
        let mut queue = VecDeque::from([Path {
            nodes: vec![source],
            edges: vec![],
        }]);
        let mut visited = BTreeSet::from([source]);
        let mut count = 0;
        while let Some(path) = queue.pop_front() {
            deadline.check("BFS traversal")?;
            let last = *path.nodes.last().expect("path nonempty");
            if last == target {
                return Ok(Some(path));
            }
            if path.edges.len() >= limits.max_depth {
                continue;
            }
            for (eid, next) in self.neighbors(last, direction, types) {
                deadline.check("BFS edge")?;
                count += 1;
                if count > limits.max_edge_expansions {
                    return Err(fail("graph edge expansion budget exceeded"));
                }
                if count > limits.max_expansions {
                    return Err(fail("graph expansion budget exceeded"));
                }
                if visited.insert(next) {
                    let mut p = path.clone();
                    p.nodes.push(next);
                    p.edges.push(eid);
                    queue.push_back(p);
                }
            }
        }
        Ok(None)
    }
    /// Exact compact JSON size, encoding at most one entity at a time.
    /// This avoids constructing a second full JSON graph solely for a budget
    /// check. It still validates/encodes all entities, not a cached RSS measure.
    pub fn storage_size(&self, max_bytes: usize) -> Result<usize, Error> {
        self.storage_size_deadline(max_bytes, None)
    }
    pub(crate) fn storage_size_deadline(
        &self,
        max_bytes: usize,
        deadline: Option<crate::Deadline>,
    ) -> Result<usize, Error> {
        let mut size = serde_json::to_vec(&serde_json::json!({"format":"bicdb-graph-v1",
            "next_id":self.next_id,"nodes":[],"edges":[]}))
        .map_err(|e| fail(e.to_string()))?
        .len()
        .saturating_add(self.nodes.len().saturating_sub(1))
        .saturating_add(self.edges.len().saturating_sub(1));
        if size > max_bytes {
            return Err(fail("graph storage budget exceeded"));
        }
        for n in self.nodes.values() {
            if let Some(clock) = deadline {
                clock.check("graph node size validation")?;
            }
            size = size.saturating_add(
                serde_json::to_vec(&serde_json::json!({"id":n.id,
                "labels":n.labels,"properties":n.properties}))
                .map_err(|e| fail(e.to_string()))?
                .len(),
            );
            if size > max_bytes {
                return Err(fail("graph storage budget exceeded"));
            }
        }
        for e in self.edges.values() {
            if let Some(clock) = deadline {
                clock.check("graph edge size validation")?;
            }
            size = size.saturating_add(
                serde_json::to_vec(&serde_json::json!({"id":e.id,
                "source":e.source,"target":e.target,"label":e.label,"properties":e.properties}))
                .map_err(|e| fail(e.to_string()))?
                .len(),
            );
            if size > max_bytes {
                return Err(fail("graph storage budget exceeded"));
            }
        }
        Ok(size)
    }
    pub fn to_bytes(&self) -> Result<Vec<u8>, Error> {
        serde_json::to_vec(&serde_json::json!({"format":"bicdb-graph-v1","next_id":self.next_id,
            "nodes":self.nodes.values().map(|n|serde_json::json!({"id":n.id,"labels":n.labels,"properties":n.properties})).collect::<Vec<_>>(),
            "edges":self.edges.values().map(|e|serde_json::json!({"id":e.id,"source":e.source,"target":e.target,"label":e.label,"properties":e.properties})).collect::<Vec<_>>()
        })).map_err(|e|fail(e.to_string()))
    }
    pub fn from_bytes(bytes: &[u8], limits: &Limits) -> Result<Self, Error> {
        if bytes.len() > limits.max_text_bytes {
            return Err(fail("graph storage byte budget exceeded"));
        }
        if bytes.is_empty() {
            return Ok(Self::new());
        }
        let v: Json =
            serde_json::from_slice(bytes).map_err(|_| fail("invalid graph storage JSON"))?;
        if v["format"] != "bicdb-graph-v1" {
            return Err(fail("unsupported graph storage format"));
        }
        let mut graph = Self::new();
        let nodes = v["nodes"]
            .as_array()
            .ok_or_else(|| fail("missing graph nodes"))?;
        let edges = v["edges"]
            .as_array()
            .ok_or_else(|| fail("missing graph edges"))?;
        if nodes.len() > limits.max_nodes || edges.len() > limits.max_edges {
            return Err(fail("graph element budget exceeded"));
        }
        for n in nodes {
            let id = n["id"].as_u64().ok_or_else(|| fail("invalid node ID"))?;
            let labels: BTreeSet<String> = n["labels"]
                .as_array()
                .ok_or_else(|| fail("invalid node labels"))?
                .iter()
                .map(|l| {
                    l.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| fail("invalid node label"))
                })
                .collect::<Result<_, _>>()?;
            let properties: Properties = n["properties"]
                .as_object()
                .ok_or_else(|| fail("invalid node properties"))?
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            graph.reserve_ids(id, id)?;
            graph.add_node(labels, properties)?;
        }
        for e in edges {
            let num = |key: &str| {
                e[key]
                    .as_u64()
                    .ok_or_else(|| fail("invalid edge ID/endpoint"))
            };
            let properties = e["properties"]
                .as_object()
                .ok_or_else(|| fail("invalid edge properties"))?
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let id = num("id")?;
            graph.reserve_ids(id, id)?;
            graph.add_edge(
                num("source")?,
                num("target")?,
                e["label"]
                    .as_str()
                    .ok_or_else(|| fail("invalid edge type"))?
                    .to_owned(),
                properties,
            )?;
        }
        graph.finish_load(
            v["next_id"]
                .as_u64()
                .ok_or_else(|| fail("invalid graph allocator"))?,
        )?;
        Ok(graph)
    }
}

pub(crate) fn validate_properties(props: &Properties) -> Result<(), Error> {
    let bytes = serde_json::to_vec(props).map_err(|_| fail("invalid properties"))?;
    if bytes.len() > 1024 * 1024 {
        return Err(fail("properties exceed 1 MiB"));
    }
    let mut pending: Vec<(&Json, usize)> = props.values().map(|v| (v, 1)).collect();
    let mut count = 0;
    while let Some((v, depth)) = pending.pop() {
        count += 1;
        if depth > 32 || count > 65536 {
            return Err(fail("property depth/node budget exceeded"));
        }
        match v {
            Json::Number(n) => {
                decimal(&n.to_string())?;
            }
            Json::Array(a) => pending.extend(a.iter().map(|v| (v, depth + 1))),
            Json::Object(o) => pending.extend(o.values().map(|v| (v, depth + 1))),
            _ => {}
        }
    }
    Ok(())
}

pub(crate) fn json_map(props: &Properties) -> Json {
    Json::Object(
        props
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect::<Map<_, _>>(),
    )
}
