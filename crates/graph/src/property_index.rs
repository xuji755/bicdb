//! Typed composite property keys and conservative index seek requests. Native
//! SQL supplies persistent B-trees; the graph core always rechecks candidates.
use crate::model::fail;
use crate::{Edge, Error, Graph, GraphChanges, Node, Properties, Value};
use serde_json::Value as Json;
use std::collections::BTreeSet;

const MAX_KEY: usize = 4088;
pub type PropertyEntries = Vec<(Vec<u8>, u64)>;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntityKind {
    Node,
    Relationship,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropertyIndex {
    pub entity: EntityKind,
    pub label: Option<String>,
    pub fields: Vec<Vec<String>>,
    pub unique: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeekOp {
    Equal,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
}
#[derive(Debug, Clone, PartialEq)]
pub struct PropertyPredicate {
    pub field: Vec<String>,
    pub op: SeekOp,
    pub value: Value,
}
#[derive(Debug, Clone)]
pub struct IndexRequest {
    pub entity: EntityKind,
    pub labels: Vec<String>,
    pub predicates: Vec<PropertyPredicate>,
}
/// None means that no safe index path exists; Some(empty) is a complete empty
/// seek. Implementations must report budget exhaustion rather than truncate.
pub trait IndexProvider {
    fn candidates(&mut self, request: &IndexRequest) -> Result<Option<Vec<u64>>, Error>;
    /// Visible full-text definitions owned by the current graph. Enumerating
    /// metadata must share the statement work budget; it must not query sources.
    fn fulltext_indexes(&mut self) -> Result<Vec<FulltextIndex>, Error> {
        Err(fail(
            "automatic full-text selection requires a native index provider",
        ))
    }
    /// Query a named, domain-scoped full-text index in the current statement view.
    /// A staged graph is supplied after mutations and must take precedence over
    /// persisted source/index state. Providers must enforce their read budgets,
    /// freshness checks and index ownership; lack of an index is an error.
    fn fulltext(
        &mut self,
        _request: &FulltextRequest,
        _staged: Option<&Graph>,
    ) -> Result<Vec<FulltextHit>, Error> {
        Err(fail(
            "named full-text search requires a native index provider",
        ))
    }
}
/// A graph-owned native definition used for conservative automatic selection.
#[derive(Debug, Clone)]
pub struct FulltextIndex {
    pub name: String,
    pub definition: crate::fulltext::Definition,
}
/// Explicit native full-text request; options use Cypher scalar/list/map values.
pub struct FulltextRequest {
    pub entity: EntityKind,
    pub index: String,
    pub query: String,
    pub options: std::collections::BTreeMap<String, Value>,
}
/// One verified source ID and its scalar search metadata (score, snippet, watermarks).
pub struct FulltextHit {
    pub id: u64,
    pub columns: std::collections::BTreeMap<String, Value>,
}
#[derive(Debug, Clone)]
pub struct KeyRange {
    pub lower: Option<Vec<u8>>,
    pub upper: Option<Vec<u8>>,
}
impl KeyRange {
    pub fn contains(&self, key: &[u8]) -> bool {
        self.lower.as_ref().map_or(true, |v| key >= v.as_slice())
            && self.upper.as_ref().map_or(true, |v| key < v.as_slice())
    }
}
#[derive(Debug, Clone)]
pub struct IndexSeek {
    pub ranges: Vec<KeyRange>,
    pub matched_fields: usize,
}

fn component(out: &mut Vec<u8>, value: &Value) {
    let (tag, bytes) = match value {
        Value::Null => (0x10, vec![]),
        Value::Bool(b) => (0x20, vec![u8::from(*b)]),
        Value::Number(n) => (0x30, n.encode()),
        Value::String(s) => (0x40, s.as_bytes().to_vec()),
        // Non-scalar values share a residual bucket. Equality for those values
        // falls back to graph evaluation; scalar range seeks include this bucket
        // to retain the original comparison-type errors.
        _ => (0x50, vec![]),
    };
    out.extend([1, tag]);
    for b in bytes {
        if b == 0 {
            out.extend([0, 255]);
        } else {
            out.push(b);
        }
    }
    out.push(0);
}
pub fn prefix_successor(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut value = prefix.to_vec();
    while let Some(last) = value.pop() {
        if last < 255 {
            value.push(last + 1);
            return Some(value);
        }
    }
    None
}
fn scalar(value: &Value) -> bool {
    matches!(value, Value::Bool(_) | Value::Number(_) | Value::String(_))
}
fn property(properties: &Properties, field: &[String]) -> Result<Value, Error> {
    let mut value = properties.get(&field[0]);
    for part in &field[1..] {
        value = match value {
            None | Some(Json::Null) => None,
            Some(Json::Object(object)) => object.get(part),
            Some(_) => return Err(fail("indexed JSON path ancestor must be an object or null")),
        };
    }
    value.map_or(Ok(Value::Null), Value::from_json)
}
impl PropertyIndex {
    pub fn validate(&self) -> Result<(), Error> {
        if self.fields.len() > 8
            || (self.fields.is_empty() && (self.label.is_none() || self.unique))
        {
            return Err(fail(
                "property index needs 1..8 fields, or a nonunique label-only definition",
            ));
        }
        if self
            .label
            .as_ref()
            .is_some_and(|v| v.is_empty() || v.len() > 128)
        {
            return Err(fail("invalid index label/type"));
        }
        let mut seen = BTreeSet::new();
        for field in &self.fields {
            if field.is_empty()
                || field.len() > 16
                || field.iter().any(|p| p.is_empty() || p.len() > 128)
                || !seen.insert(field)
            {
                return Err(fail("invalid or duplicate property index field"));
            }
        }
        Ok(())
    }
    pub fn to_bytes(&self) -> Result<Vec<u8>, Error> {
        self.validate()?;
        let bytes = serde_json::to_vec(&serde_json::json!({"format":"bicdb-graph-property-v1", "entity":if self.entity==EntityKind::Node {"node"} else {"relationship"}, "label":self.label, "fields":self.fields,"unique":self.unique})).map_err(|e| fail(e.to_string()))?;
        if bytes.len() > 4096 {
            return Err(fail("property index definition exceeds 4096 bytes"));
        }
        Ok(bytes)
    }
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > 4096 {
            return Err(fail("oversized property index definition"));
        }
        let value: Json =
            serde_json::from_slice(bytes).map_err(|_| fail("invalid property index definition"))?;
        let object = value
            .as_object()
            .ok_or_else(|| fail("invalid property index definition"))?;
        if object.len() != 5
            || object.keys().any(|key| {
                !matches!(
                    key.as_str(),
                    "format" | "entity" | "label" | "fields" | "unique"
                )
            })
            || value["format"] != "bicdb-graph-property-v1"
        {
            return Err(fail("unsupported property index definition"));
        }
        let entity = match value["entity"].as_str() {
            Some("node") => EntityKind::Node,
            Some("relationship") => EntityKind::Relationship,
            _ => return Err(fail("invalid indexed entity kind")),
        };
        let label = match &value["label"] {
            Json::Null => None,
            Json::String(s) => Some(s.clone()),
            _ => return Err(fail("invalid index label")),
        };
        let fields = value["fields"]
            .as_array()
            .ok_or_else(|| fail("invalid index fields"))?
            .iter()
            .map(|v| {
                v.as_array()
                    .ok_or_else(|| fail("invalid index field path"))?
                    .iter()
                    .map(|v| {
                        v.as_str()
                            .map(str::to_owned)
                            .ok_or_else(|| fail("invalid index field part"))
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .collect::<Result<Vec<_>, _>>()?;
        let unique = value["unique"]
            .as_bool()
            .ok_or_else(|| fail("invalid index uniqueness"))?;
        let result = Self {
            entity,
            label,
            fields,
            unique,
        };
        result.validate()?;
        Ok(result)
    }
    fn key(&self, properties: &Properties) -> Result<(Vec<u8>, bool), Error> {
        let mut key = vec![];
        let mut comparable = true;
        for field in &self.fields {
            let value = property(properties, field)?;
            if self.unique && !matches!(value, Value::Null) && !scalar(&value) {
                return Err(fail("unique property index requires scalar field values"));
            }
            comparable &= scalar(&value);
            component(&mut key, &value);
            if key.len() > MAX_KEY {
                return Err(fail(
                    "property B-tree key exceeds 4088 bytes; use shorter indexed values",
                ));
            }
        }
        Ok((key, comparable))
    }
    pub fn entries(&self, graph: &Graph) -> Result<Vec<(Vec<u8>, u64)>, Error> {
        self.entries_with_budget(graph, crate::Limits::default().max_text_bytes)
    }
    /// Bound in-memory key planning independently of the serialized graph.
    pub fn entries_with_budget(
        &self,
        graph: &Graph,
        max_bytes: usize,
    ) -> Result<Vec<(Vec<u8>, u64)>, Error> {
        self.entries_from(
            self.selected_properties(graph.nodes().values(), graph.edges().values()),
            max_bytes,
        )
    }
    fn selected_properties<'a>(
        &'a self,
        nodes: impl Iterator<Item = &'a Node> + 'a,
        edges: impl Iterator<Item = &'a Edge> + 'a,
    ) -> Box<dyn Iterator<Item = (u64, &'a Properties)> + 'a> {
        match self.entity {
            EntityKind::Node => Box::new(
                nodes
                    .filter(|n| self.label.as_ref().map_or(true, |l| n.labels.contains(l)))
                    .map(|n| (n.id, &n.properties)),
            ),
            EntityKind::Relationship => Box::new(
                edges
                    .filter(|e| self.label.as_ref().map_or(true, |l| &e.label == l))
                    .map(|e| (e.id, &e.properties)),
            ),
        }
    }
    fn entries_from<'a>(
        &self,
        properties: impl Iterator<Item = (u64, &'a Properties)>,
        max_bytes: usize,
    ) -> Result<PropertyEntries, Error> {
        self.validate()?;
        let mut entries = vec![];
        let mut unique = BTreeSet::new();
        let mut bytes = 0usize;
        for (id, properties) in properties {
            let (key, comparable) = self.key(properties)?;
            bytes = bytes.saturating_add(key.len()).saturating_add(32);
            if bytes > max_bytes {
                return Err(fail("graph property index key planning budget exceeded"));
            }
            if self.unique && comparable && !unique.insert(key.clone()) {
                return Err(fail("unique graph property index violation"));
            }
            entries.push((key, id));
        }
        entries.sort();
        Ok(entries)
    }
    /// Produce old/new keys and check uniqueness against the complete final graph.
    /// Callers with complete persistent key membership may supply exact probes
    /// through `changed_entries_with_probe` to avoid scanning unchanged entities.
    pub fn changed_entries(
        &self,
        changes: &GraphChanges,
        after: &Graph,
        max_bytes: usize,
    ) -> Result<(PropertyEntries, PropertyEntries), Error> {
        self.changed_entries_with_probe(changes, after, max_bytes, |_, _| Ok(None))
    }
    /// An exact probe returns all persisted candidates for a complete comparable
    /// key, including historical/aborted entries. `Some(empty)` proves an empty
    /// seek; `None` requests a streaming fallback. A truncated probe must error.
    /// Candidates are rechecked against final source values; changed identities
    /// are already checked together, so key swaps/deletions remain valid.
    pub fn changed_entries_with_probe(
        &self,
        changes: &GraphChanges,
        after: &Graph,
        max_bytes: usize,
        mut probe: impl FnMut(&Self, &[u8]) -> Result<Option<Vec<u64>>, Error>,
    ) -> Result<(PropertyEntries, PropertyEntries), Error> {
        self.changed_entries_resolved(
            changes,
            after,
            max_bytes,
            &mut probe,
            |index, id| {
                Ok(match index.entity {
                    EntityKind::Node => after.nodes().get(&id).and_then(|n| {
                        index
                            .label
                            .as_ref()
                            .map_or(true, |l| n.labels.contains(l))
                            .then(|| n.properties.clone())
                    }),
                    EntityKind::Relationship => after.edges().get(&id).and_then(|e| {
                        index
                            .label
                            .as_ref()
                            .map_or(true, |l| &e.label == l)
                            .then(|| e.properties.clone())
                    }),
                })
            },
            true,
        )
    }
    /// Complete persistent candidate proofs with explicit source resolution.
    /// Cache absence is not deletion. An unresolved candidate must be fetched
    /// at the statement snapshot or fail, never become a false empty proof.
    pub fn changed_entries_with_source(
        &self,
        changes: &GraphChanges,
        after: &Graph,
        max_bytes: usize,
        mut probe: impl FnMut(&Self, &[u8]) -> Result<Option<Vec<u64>>, Error>,
        mut source: impl FnMut(&Self, u64) -> Result<Option<Properties>, Error>,
    ) -> Result<(PropertyEntries, PropertyEntries), Error> {
        self.changed_entries_resolved(changes, after, max_bytes, &mut probe, &mut source, false)
    }
    #[allow(clippy::too_many_arguments)]
    fn changed_entries_resolved(
        &self,
        changes: &GraphChanges,
        after: &Graph,
        max_bytes: usize,
        mut probe: impl FnMut(&Self, &[u8]) -> Result<Option<Vec<u64>>, Error>,
        mut source: impl FnMut(&Self, u64) -> Result<Option<Properties>, Error>,
        allow_fallback: bool,
    ) -> Result<(PropertyEntries, PropertyEntries), Error> {
        let before = self.entries_from(
            self.selected_properties(
                changes.nodes().values().filter_map(Option::as_ref),
                changes.edges().values().filter_map(Option::as_ref),
            ),
            max_bytes,
        )?;
        let current = || {
            self.selected_properties(
                changes
                    .nodes()
                    .keys()
                    .filter_map(|id| after.nodes().get(id)),
                changes
                    .edges()
                    .keys()
                    .filter_map(|id| after.edges().get(id)),
            )
        };
        let new = self.entries_from(current(), max_bytes)?;
        if self.unique && !new.is_empty() {
            let mut keys = BTreeSet::new();
            for (_, props) in current() {
                let (key, comparable) = self.key(props)?;
                if comparable {
                    keys.insert(key);
                }
            }
            let changed = match self.entity {
                EntityKind::Node => changes.nodes().keys().copied().collect::<BTreeSet<_>>(),
                EntityKind::Relationship => {
                    changes.edges().keys().copied().collect::<BTreeSet<_>>()
                }
            };
            let mut fallback = BTreeSet::new();
            for key in keys {
                let Some(candidates) = probe(self, &key)? else {
                    if !allow_fallback {
                        return Err(fail(
                            "partial uniqueness proof requires complete candidates",
                        ));
                    }
                    fallback.insert(key);
                    continue;
                };
                for id in candidates {
                    if id == 0 || id >= 1 << 48 {
                        return Err(fail("invalid unique index candidate identity"));
                    }
                    if changed.contains(&id) {
                        continue;
                    }
                    let props = source(self, id)?;
                    if let Some(props) = props {
                        let (actual, comparable) = self.key(&props)?;
                        if comparable && actual == key {
                            return Err(fail("unique graph property index violation"));
                        }
                    }
                }
            }
            if !fallback.is_empty() {
                for (id, props) in
                    self.selected_properties(after.nodes().values(), after.edges().values())
                {
                    if changed.contains(&id) {
                        continue;
                    }
                    let (key, comparable) = self.key(props)?;
                    if comparable && fallback.contains(&key) {
                        return Err(fail("unique graph property index violation"));
                    }
                }
            }
        }
        Ok((before, new))
    }
    /// Equality prefixes followed by at most one range column. Missing trailing
    /// properties are indexed too, so a partial composite seek stays complete.
    pub fn seek(&self, request: &IndexRequest) -> Option<IndexSeek> {
        if request.entity != self.entity
            || self.label.as_ref().is_some_and(|l| {
                if self.entity == EntityKind::Relationship {
                    request.labels.len() != 1 || request.labels[0] != *l
                } else {
                    !request.labels.contains(l)
                }
            })
        {
            return None;
        }
        let mut prefix = vec![];
        let mut matched = 0;
        for field in &self.fields {
            if let Some(p) = request
                .predicates
                .iter()
                .find(|p| &p.field == field && p.op == SeekOp::Equal && scalar(&p.value))
            {
                component(&mut prefix, &p.value);
                matched += 1;
                continue;
            }
            let predicates: Vec<_> = request
                .predicates
                .iter()
                .filter(|p| &p.field == field && p.op != SeekOp::Equal && scalar(&p.value))
                .collect();
            let first = predicates.first().copied();
            if let Some(first) = first {
                let mut type_prefix = prefix.clone();
                let tag = match first.value {
                    Value::Bool(_) => 0x20,
                    Value::Number(_) => 0x30,
                    _ => 0x40,
                };
                type_prefix.extend([1, tag]);
                let type_end = prefix_successor(&type_prefix);
                let mut lower = Some(type_prefix.clone());
                let mut upper = type_end.clone();
                if predicates.iter().any(|p| {
                    std::mem::discriminant(&p.value) != std::mem::discriminant(&first.value)
                }) {
                    return None;
                }
                for p in predicates {
                    if std::mem::discriminant(&p.value) != std::mem::discriminant(&first.value) {
                        continue;
                    }
                    let mut bound = prefix.clone();
                    component(&mut bound, &p.value);
                    match p.op {
                        SeekOp::Greater => {
                            let b = prefix_successor(&bound);
                            if b > lower {
                                lower = b;
                            }
                        }
                        SeekOp::GreaterEqual => {
                            if lower.as_ref().map_or(true, |v| &bound > v) {
                                lower = Some(bound);
                            }
                        }
                        SeekOp::Less => {
                            if upper.as_ref().map_or(true, |v| &bound < v) {
                                upper = Some(bound);
                            }
                        }
                        SeekOp::LessEqual => {
                            let b = prefix_successor(&bound);
                            if b < upper {
                                upper = b;
                            }
                        }
                        SeekOp::Equal => {}
                    }
                }
                // Keep values of other types in the candidate set: evaluating a
                // mixed-type range must still report a comparison type mismatch.
                let mut ranges = vec![
                    KeyRange {
                        lower: Some(prefix.clone()),
                        upper: Some(type_prefix),
                    },
                    KeyRange {
                        lower: type_end,
                        upper: prefix_successor(&prefix),
                    },
                ];
                if lower
                    .as_ref()
                    .zip(upper.as_ref())
                    .map_or(true, |(l, u)| l < u)
                {
                    ranges.push(KeyRange { lower, upper });
                }
                return Some(IndexSeek {
                    ranges,
                    matched_fields: matched + 1,
                });
            }
            break;
        }
        if matched == 0 && !self.fields.is_empty() {
            return None;
        }
        Some(IndexSeek {
            ranges: vec![KeyRange {
                lower: Some(prefix.clone()),
                upper: prefix_successor(&prefix),
            }],
            matched_fields: matched,
        })
    }
}
