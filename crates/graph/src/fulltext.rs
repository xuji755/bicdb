//! Versioned, bounded mixed-language inverted index. No source write path calls
//! the analyzer: callers build/publish generations in maintenance transactions.
//! Native B-tree keys contain domain, field, channel, term and source revision;
//! heap records retain positions and text for recheck/snippets. Not Lucene syntax.
use crate::model::fail;
use crate::property_index::{prefix_successor, EntityKind};
use crate::{Edge, Error, Graph, Node};
use bicdb_common::sha256::Sha256;
use serde_json::{json, Value as Json};
use std::collections::{BTreeMap, BTreeSet};

pub const ANALYZER: &str = "bic_mixed_v1";
const FORMAT: &str = "bicdb-graph-fulltext-v1";
const FORMAT_V2: &str = "bicdb-graph-fulltext-v2";
const FORMAT_V3: &str = "bicdb-graph-fulltext-v3";
const FORMAT_V4: &str = "bicdb-graph-fulltext-v4";
mod batch;
pub use batch::{CorpusBudget, NativeBatch};
mod statistics;
pub use statistics::StatisticsSelection;
const STAT_KIND: u64 = 2 << 60;
const TERM_BYTES: usize = 512;
const CHUNK: usize = 4096;
const CHUNKS: u64 = 1023;
const DOC_KIND: u64 = 1 << 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathPart {
    Key(String),
    Index(usize),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Definition {
    pub entity: EntityKind,
    /// Any label/type matches (OR), unlike a node pattern's label conjunction.
    pub labels: Vec<String>,
    pub fields: Vec<Vec<PathPart>>,
}
#[derive(Debug, Clone)]
pub struct TextLimits {
    pub max_bytes: usize,
    pub max_documents: usize,
    pub max_tokens: usize,
    pub max_query_tokens: usize,
    pub max_hits: usize,
}
impl Default for TextLimits {
    fn default() -> Self {
        Self {
            max_bytes: 100 * 1024 * 1024,
            max_documents: 100_000,
            max_tokens: 1_000_000,
            max_query_tokens: 128,
            max_hits: 10_000,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Channel {
    Terms,
    Phrase,
    Exact,
}
impl Channel {
    fn number(self) -> u8 {
        match self {
            Self::Terms => 0,
            Self::Phrase => 1,
            Self::Exact => 2,
        }
    }
    fn decode(n: u64) -> Result<Self, Error> {
        match n {
            0 => Ok(Self::Terms),
            1 => Ok(Self::Phrase),
            2 => Ok(Self::Exact),
            _ => Err(fail("invalid fulltext channel")),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub term: String,
    pub channel: Channel,
    pub field: usize,
    pub segment: usize,
    pub position: usize,
    pub start: usize,
    pub end: usize,
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Diagnostics {
    pub missing: usize,
    pub null: usize,
    pub excluded_type: usize,
    pub oversized_terms: usize,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    pub id: u64,
    pub domain: String,
    pub revision: [u8; 32],
    pub fields: Vec<Vec<String>>,
    pub tokens: Vec<Token>,
    pub diagnostics: Diagnostics,
}
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub id: u64,
    pub score: f64,
    pub field: String,
    pub snippet: String,
    pub offset: usize,
    pub line: usize,
}
#[derive(Debug, Clone)]
pub struct SearchOptions {
    pub domain: String,
    pub fields: Vec<String>,
    pub channel: Channel,
}
#[derive(Debug, Clone)]
pub struct TermRange {
    pub field: usize,
    pub term: String,
    pub lower: Vec<u8>,
    pub upper: Option<Vec<u8>>,
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Statistics {
    documents: usize,
    length: usize,
    df: BTreeMap<String, usize>,
}
type PostingKey = (String, usize, Channel, String);
type StatisticsKey = (String, usize, Channel);
/// Charge while collecting records, before an oversized corpus is retained.
/// This is a conservative logical allocation budget, not a process RSS limit.
#[derive(Default)]
/// Incremental logical budget for a collection being prepared for publication.
/// Charge each analyzed document before retaining the whole batch in memory.
pub struct DocumentBudget {
    documents: usize,
    tokens: usize,
    bytes: usize,
}
impl DocumentBudget {
    pub fn add(&mut self, doc: &Document, limits: &TextLimits) -> Result<(), Error> {
        self.documents = self.documents.saturating_add(1);
        self.tokens = self.tokens.saturating_add(doc.tokens.len());
        self.bytes = self.bytes.saturating_add(128 + doc.domain.len());
        for segments in &doc.fields {
            self.bytes = self
                .bytes
                .saturating_add(24)
                .saturating_add(segments.len().saturating_mul(24));
            for text in segments {
                self.bytes = self.bytes.saturating_add(text.len());
            }
        }
        for token in &doc.tokens {
            self.bytes = self
                .bytes
                .saturating_add(96)
                .saturating_add(token.term.len());
        }
        if self.documents > limits.max_documents
            || self.tokens > limits.max_tokens
            || self.bytes > limits.max_bytes
        {
            return Err(fail(
                "fulltext generation memory/token/document budget exceeded",
            ));
        }
        Ok(())
    }
}
/// Published generations are immutable. A failed build/batch leaves this image
/// unchanged. Native SQL owns queue durability, publication and freshness policy.
#[derive(Debug, Clone)]
pub struct Generation {
    definition: Definition,
    generation: u64,
    covered_seq: u64,
    documents: BTreeMap<u64, Document>,
    postings: BTreeMap<PostingKey, BTreeMap<u64, Vec<usize>>>,
    statistics: BTreeMap<StatisticsKey, Statistics>,
    native_format: u8,
    query_scope: Option<(String, Vec<usize>, Channel, Vec<String>)>,
    complete: bool,
}
/// Independently checksummed header. Reading it does not decode document bodies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenerationManifest {
    pub definition: Definition,
    pub generation: u64,
    pub covered_seq: u64,
    pub documents: usize,
    /// None for v1; v2 counts sequential pages, v3 counts keyed buckets.
    pub statistics_pages: Option<usize>,
    /// 1: corpus-only, 2: sequential statistics, 3: Merkle keyed buckets.
    pub storage_format: u8,
    /// Global persistent limits permit cold maintenance without reading all bodies.
    pub corpus_budget: Option<CorpusBudget>,
}
fn sha(bytes: &[u8]) -> [u8; 32] {
    let mut s = Sha256::new();
    s.update(bytes);
    s.finalize()
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn hash(value: &Json) -> Result<[u8; 32], Error> {
    Ok(sha(
        &serde_json::to_vec(value).map_err(|e| fail(e.to_string()))?
    ))
}
fn hash_decode(s: &str) -> Result<[u8; 32], Error> {
    if s.len() != 64 || !s.is_ascii() {
        return Err(fail("invalid fulltext revision"));
    }
    let mut out = [0; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
            .map_err(|_| fail("invalid fulltext revision"))?;
    }
    Ok(out)
}
fn cjk(c: char) -> bool {
    matches!(c as u32,0x3400..=0x4dbf|0x4e00..=0x9fff|0xf900..=0xfaff|0x20000..=0x323af|0x3040..=0x30ff|0xac00..=0xd7af|0x1100..=0x11ff)
}
fn lexical(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}
fn token(
    out: &mut Vec<Token>,
    term: &str,
    channel: Channel,
    location: (usize, usize, usize),
    span: (usize, usize),
    diag: &mut Diagnostics,
    limits: &TextLimits,
) -> Result<(), Error> {
    let (field, segment, position) = location;
    let (start, end) = span;
    let term = term.to_lowercase();
    if term.len() > TERM_BYTES {
        diag.oversized_terms += 1;
        return Ok(());
    }
    if out.len() >= limits.max_tokens {
        return Err(fail("fulltext token budget exceeded"));
    }
    out.push(Token {
        term,
        channel,
        field,
        segment,
        position,
        start,
        end,
    });
    Ok(())
}
/// Latin/digit/underscore runs, CJK unigrams for phrase positions, unigrams and
/// adjacent bigrams for terms. Byte offsets always refer to the original UTF-8 text.
/// Oversized terms are diagnosed and unqueryable (queries reject >512 bytes).
fn analyze(
    text: &str,
    field: usize,
    segment: usize,
    limits: &TextLimits,
) -> Result<(Vec<Token>, Diagnostics), Error> {
    if text.len() > limits.max_bytes {
        return Err(fail("fulltext text byte budget exceeded"));
    }
    let chars: Vec<_> = text.char_indices().collect();
    let mut out = vec![];
    let mut diagnostics = Diagnostics::default();
    let mut i = 0;
    let mut position = 0;
    while i < chars.len() {
        let (start, c) = chars[i];
        if !lexical(c) {
            i += 1;
            continue;
        }
        let is_cjk = cjk(c);
        let first = i;
        i += 1;
        while i < chars.len() && lexical(chars[i].1) && cjk(chars[i].1) == is_cjk {
            i += 1;
        }
        let end = chars.get(i).map_or(text.len(), |c| c.0);
        if is_cjk {
            for j in first..i {
                let lo = chars[j].0;
                let hi = chars.get(j + 1).map_or(text.len(), |c| c.0);
                token(
                    &mut out,
                    &text[lo..hi],
                    Channel::Phrase,
                    (field, segment, position + j - first),
                    (lo, hi),
                    &mut diagnostics,
                    limits,
                )?;
                token(
                    &mut out,
                    &text[lo..hi],
                    Channel::Terms,
                    (field, segment, position + j - first),
                    (lo, hi),
                    &mut diagnostics,
                    limits,
                )?;
                if j + 1 < i {
                    let hi = chars.get(j + 2).map_or(text.len(), |c| c.0);
                    token(
                        &mut out,
                        &text[lo..hi],
                        Channel::Terms,
                        (field, segment, position + j - first),
                        (lo, hi),
                        &mut diagnostics,
                        limits,
                    )?;
                }
            }
            token(
                &mut out,
                &text[start..end],
                Channel::Exact,
                (field, segment, position),
                (start, end),
                &mut diagnostics,
                limits,
            )?;
            position += i - first;
        } else {
            for channel in [Channel::Terms, Channel::Phrase, Channel::Exact] {
                token(
                    &mut out,
                    &text[start..end],
                    channel,
                    (field, segment, position),
                    (start, end),
                    &mut diagnostics,
                    limits,
                )?;
            }
            position += 1;
        }
    }
    Ok((out, diagnostics))
}
enum Selected<'a> {
    Missing,
    Null,
    Excluded,
    Value(&'a Json),
}
impl Definition {
    pub fn validate(&self) -> Result<(), Error> {
        if self.fields.is_empty() || self.fields.len() > 16 || self.labels.len() > 16 {
            return Err(fail(
                "fulltext needs 1..16 fields and at most 16 labels/types",
            ));
        }
        let mut labels = BTreeSet::new();
        for l in &self.labels {
            if l.is_empty() || l.len() > 128 || !labels.insert(l) {
                return Err(fail("invalid/duplicate fulltext label/type"));
            }
        }
        let mut fields = BTreeSet::new();
        for p in &self.fields {
            if p.is_empty() || p.len() > 16 || !matches!(p[0], PathPart::Key(_)) {
                return Err(fail("invalid fulltext JSON path"));
            }
            for part in p {
                match part {
                    PathPart::Key(k) if k.is_empty() || k.len() > 128 => {
                        return Err(fail("invalid fulltext property name"))
                    }
                    PathPart::Index(i) if *i > 1_000_000 => {
                        return Err(fail("fulltext array index exceeds budget"))
                    }
                    _ => {}
                }
            }
            if !fields.insert(Self::field_name(p)) {
                return Err(fail("duplicate fulltext field"));
            }
        }
        if serde_json::to_vec(&self.json())
            .map_err(|e| fail(e.to_string()))?
            .len()
            > 4096
        {
            return Err(fail("fulltext definition exceeds 4096 bytes"));
        }
        Ok(())
    }
    pub fn field_name(path: &[PathPart]) -> String {
        let mut s = String::new();
        for part in path {
            match part {
                PathPart::Key(k) if k.chars().all(|c| c.is_alphanumeric() || c == '_') => {
                    if !s.is_empty() {
                        s.push('.');
                    }
                    s.push_str(k);
                }
                PathPart::Key(k) => {
                    s.push('[');
                    s.push_str(&serde_json::to_string(k).expect("string"));
                    s.push(']');
                }
                PathPart::Index(i) => s.push_str(&format!("[{i}]")),
            }
        }
        s
    }
    fn json(&self) -> Json {
        json!({"analyzer":ANALYZER,"entity":if self.entity==EntityKind::Node{"node"}else{"relationship"},"labels":self.labels,"fields":self.fields.iter().map(|p|p.iter().map(|v|match v {PathPart::Key(k)=>json!({"key":k}),PathPart::Index(i)=>json!({"index":i})}).collect::<Vec<_>>()).collect::<Vec<_>>()})
    }
    fn from_json(v: &Json) -> Result<Self, Error> {
        let obj = v
            .as_object()
            .ok_or_else(|| fail("invalid fulltext definition"))?;
        if obj.len() != 4 || v["analyzer"] != ANALYZER {
            return Err(fail("unsupported fulltext definition/analyzer"));
        }
        let entity = match v["entity"].as_str() {
            Some("node") => EntityKind::Node,
            Some("relationship") => EntityKind::Relationship,
            _ => return Err(fail("invalid fulltext entity kind")),
        };
        let labels = strings(&v["labels"])?;
        let mut fields = vec![];
        for p in array(&v["fields"])? {
            let mut path = vec![];
            for part in array(p)? {
                let obj = part
                    .as_object()
                    .ok_or_else(|| fail("invalid fulltext path component"))?;
                if obj.len() != 1 {
                    return Err(fail("invalid fulltext path component"));
                }
                if let Some(k) = obj.get("key").and_then(Json::as_str) {
                    path.push(PathPart::Key(k.into()));
                } else if let Some(i) = obj.get("index").and_then(Json::as_u64) {
                    path.push(PathPart::Index(
                        usize::try_from(i).map_err(|_| fail("invalid array index"))?,
                    ));
                } else {
                    return Err(fail("invalid fulltext path component"));
                }
            }
            fields.push(path);
        }
        let d = Self {
            entity,
            labels,
            fields,
        };
        d.validate()?;
        Ok(d)
    }
    pub fn to_bytes(&self) -> Result<Vec<u8>, Error> {
        self.validate()?;
        serde_json::to_vec(&self.json()).map_err(|e| fail(e.to_string()))
    }
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > 4096 {
            return Err(fail("oversized fulltext definition"));
        }
        Self::from_json(
            &serde_json::from_slice(bytes).map_err(|_| fail("invalid fulltext definition"))?,
        )
    }
    fn selected<'a>(&self, props: &'a BTreeMap<String, Json>, path: &[PathPart]) -> Selected<'a> {
        let PathPart::Key(key) = &path[0] else {
            return Selected::Excluded;
        };
        let Some(mut v) = props.get(key) else {
            return Selected::Missing;
        };
        for part in &path[1..] {
            if v.is_null() {
                return Selected::Null;
            }
            let next = match part {
                PathPart::Key(k) => {
                    let Some(obj) = v.as_object() else {
                        return Selected::Excluded;
                    };
                    obj.get(k)
                }
                PathPart::Index(i) => {
                    let Some(a) = v.as_array() else {
                        return Selected::Excluded;
                    };
                    a.get(*i)
                }
            };
            let Some(next) = next else {
                return Selected::Missing;
            };
            v = next;
        }
        if v.is_null() {
            Selected::Null
        } else {
            Selected::Value(v)
        }
    }
    pub fn node_document(&self, n: &Node, limits: &TextLimits) -> Result<Option<Document>, Error> {
        self.validate()?;
        self.node_document_validated(n, limits)
    }
    fn node_document_validated(
        &self,
        n: &Node,
        limits: &TextLimits,
    ) -> Result<Option<Document>, Error> {
        if self.entity != EntityKind::Node
            || (!self.labels.is_empty() && !self.labels.iter().any(|l| n.labels.contains(l)))
        {
            return Ok(None);
        }
        self.document(n.id, &n.properties, node_revision(n)?, limits)
    }
    pub fn edge_document(&self, e: &Edge, limits: &TextLimits) -> Result<Option<Document>, Error> {
        self.validate()?;
        self.edge_document_validated(e, limits)
    }
    fn edge_document_validated(
        &self,
        e: &Edge,
        limits: &TextLimits,
    ) -> Result<Option<Document>, Error> {
        if self.entity != EntityKind::Relationship
            || (!self.labels.is_empty() && !self.labels.contains(&e.label))
        {
            return Ok(None);
        }
        self.document(e.id, &e.properties, edge_revision(e)?, limits)
    }
    fn document(
        &self,
        id: u64,
        props: &BTreeMap<String, Json>,
        revision: [u8; 32],
        limits: &TextLimits,
    ) -> Result<Option<Document>, Error> {
        if id == 0 || id >= 1 << 48 {
            return Err(fail("invalid fulltext source ID"));
        }
        let Some(domain) = props.get("db_type").and_then(Json::as_str) else {
            return Ok(None);
        };
        if domain.trim().is_empty() {
            return Ok(None);
        }
        if domain.len() > 128 {
            return Err(fail("fulltext domain exceeds 128 bytes"));
        }
        let mut document = Document {
            id,
            domain: domain.into(),
            revision,
            fields: vec![],
            tokens: vec![],
            diagnostics: Diagnostics::default(),
        };
        let mut bytes = 0usize;
        for (field, path) in self.fields.iter().enumerate() {
            let mut segments = vec![];
            match self.selected(props, path) {
                Selected::Missing => document.diagnostics.missing += 1,
                Selected::Null => document.diagnostics.null += 1,
                Selected::Value(Json::String(s)) => segments.push(s.clone()),
                Selected::Value(Json::Array(a)) => {
                    for v in a {
                        if let Json::String(s) = v {
                            segments.push(s.clone());
                        } else {
                            document.diagnostics.excluded_type += 1;
                        }
                    }
                }
                _ => document.diagnostics.excluded_type += 1,
            }
            for (segment, text) in segments.iter().enumerate() {
                bytes = bytes.saturating_add(text.len());
                if bytes > limits.max_bytes {
                    return Err(fail("fulltext document byte budget exceeded"));
                }
                let (tokens, d) = analyze(text, field, segment, limits)?;
                document.diagnostics.oversized_terms += d.oversized_terms;
                if document.tokens.len().saturating_add(tokens.len()) > limits.max_tokens {
                    return Err(fail("fulltext document token budget exceeded"));
                }
                document.tokens.extend(tokens);
            }
            document.fields.push(segments);
        }
        Ok(Some(document))
    }
}
pub fn node_revision(n: &Node) -> Result<[u8; 32], Error> {
    hash(&json!({"id":n.id,"labels":n.labels,"properties":n.properties}))
}
pub fn edge_revision(e: &Edge) -> Result<[u8; 32], Error> {
    hash(
        &json!({"id":e.id,"source":e.source,"target":e.target,"label":e.label,"properties":e.properties}),
    )
}
fn array(v: &Json) -> Result<&Vec<Json>, Error> {
    v.as_array().ok_or_else(|| fail("invalid fulltext array"))
}
fn strings(v: &Json) -> Result<Vec<String>, Error> {
    array(v)?
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or_else(|| fail("invalid fulltext string"))
        })
        .collect()
}
fn unsigned(v: &Json) -> Result<usize, Error> {
    usize::try_from(v.as_u64().ok_or_else(|| fail("invalid fulltext integer"))?)
        .map_err(|_| fail("fulltext integer overflow"))
}
fn query_tokens(query: &str, channel: Channel, limits: &TextLimits) -> Result<Vec<String>, Error> {
    if query.len() > 4096 {
        return Err(fail("fulltext query exceeds 4096 bytes"));
    }
    let (tokens, diagnostics) = analyze(query, 0, 0, limits)?;
    if diagnostics.oversized_terms != 0 {
        return Err(fail("fulltext query term exceeds 512 bytes"));
    }
    let terms: Vec<_> = tokens
        .into_iter()
        .filter(|t| t.channel == channel)
        .map(|t| t.term)
        .collect();
    if terms.is_empty()
        || terms.len() > limits.max_query_tokens
        || (channel == Channel::Exact && terms.len() != 1)
    {
        return Err(fail(
            "fulltext query token count invalid; exact requires one identifier",
        ));
    }
    Ok(terms)
}
fn prefix(domain: &str, field: usize, channel: Channel, term: &str) -> Vec<u8> {
    let mut key = vec![1, channel.number()];
    key.extend((domain.len() as u16).to_be_bytes());
    key.extend(domain.as_bytes());
    key.extend((field as u16).to_be_bytes());
    key.extend((term.len() as u16).to_be_bytes());
    key.extend(term.as_bytes());
    key
}
impl Generation {
    pub fn build(
        definition: Definition,
        graph: &Graph,
        generation: u64,
        covered_seq: u64,
        limits: &TextLimits,
    ) -> Result<Self, Error> {
        definition.validate()?;
        let mut documents = BTreeMap::new();
        let mut budget = DocumentBudget::default();
        match definition.entity {
            EntityKind::Node => {
                for node in graph.nodes().values() {
                    if let Some(d) = definition.node_document_validated(node, limits)? {
                        budget.add(&d, limits)?;
                        documents.insert(d.id, d);
                    }
                }
            }
            EntityKind::Relationship => {
                for edge in graph.edges().values() {
                    if let Some(d) = definition.edge_document_validated(edge, limits)? {
                        budget.add(&d, limits)?;
                        documents.insert(d.id, d);
                    }
                }
            }
        }
        Self::from_documents(definition, generation, covered_seq, documents, limits)
    }
    fn from_documents(
        definition: Definition,
        generation: u64,
        covered_seq: u64,
        documents: BTreeMap<u64, Document>,
        limits: &TextLimits,
    ) -> Result<Self, Error> {
        if generation == 0 || documents.len() > limits.max_documents {
            return Err(fail("invalid fulltext generation/document budget"));
        }
        definition.validate()?;
        let mut image = Self {
            definition,
            generation,
            covered_seq,
            documents,
            postings: BTreeMap::new(),
            statistics: BTreeMap::new(),
            native_format: 4,
            query_scope: None,
            complete: true,
        };
        let mut budget = DocumentBudget::default();
        for doc in image.documents.values() {
            if doc.fields.len() != image.definition.fields.len() {
                return Err(fail("fulltext document field count mismatch"));
            }
            budget.add(doc, limits)?;
            let mut present = BTreeSet::new();
            let mut lengths = BTreeMap::<StatisticsKey, usize>::new();
            for (i, t) in doc.tokens.iter().enumerate() {
                let sk = (doc.domain.clone(), t.field, t.channel);
                *lengths.entry(sk.clone()).or_default() += 1;
                let pk = (doc.domain.clone(), t.field, t.channel, t.term.clone());
                image
                    .postings
                    .entry(pk)
                    .or_default()
                    .entry(doc.id)
                    .or_default()
                    .push(i);
                present.insert((sk, t.term.clone()));
            }
            for (sk, len) in lengths {
                let s = image.statistics.entry(sk).or_default();
                s.documents += 1;
                s.length += len;
            }
            for (sk, term) in present {
                *image
                    .statistics
                    .entry(sk)
                    .or_default()
                    .df
                    .entry(term)
                    .or_default() += 1;
            }
        }
        Ok(image)
    }
    pub fn definition(&self) -> &Definition {
        &self.definition
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn covered_seq(&self) -> u64 {
        self.covered_seq
    }
    pub fn documents(&self) -> &BTreeMap<u64, Document> {
        &self.documents
    }
    /// Reconcile a bounded consistent source view. Unchanged revisions retain
    /// their stored tokens; only changed/new matching sources invoke analysis.
    /// This still scans source revisions and rebuilds in-memory statistics.
    pub fn synchronize(
        &self,
        graph: &Graph,
        covered_seq: u64,
        limits: &TextLimits,
    ) -> Result<(Self, BTreeSet<u64>), Error> {
        if !self.complete {
            return Err(fail("cannot synchronize a partial fulltext image"));
        }
        let mut changes = vec![];
        let mut changed = BTreeSet::new();
        let mut present = BTreeSet::new();
        let mut budget = DocumentBudget::default();
        let mut collect = |id, revision, document: Option<Document>| -> Result<(), Error> {
            if self.documents.get(&id).map(|d| d.revision) == Some(revision) {
                return Ok(());
            }
            if let Some(d) = &document {
                budget.add(d, limits)?;
            }
            if document.is_some() || self.documents.contains_key(&id) {
                changed.insert(id);
                changes.push((id, document));
            }
            Ok(())
        };
        match self.definition.entity {
            EntityKind::Node => {
                for n in graph.nodes().values() {
                    present.insert(n.id);
                    let revision = node_revision(n)?;
                    if self.documents.get(&n.id).map(|d| d.revision) == Some(revision) {
                        continue;
                    }
                    collect(
                        n.id,
                        revision,
                        self.definition.node_document_validated(n, limits)?,
                    )?;
                }
            }
            EntityKind::Relationship => {
                for e in graph.edges().values() {
                    present.insert(e.id);
                    let revision = edge_revision(e)?;
                    if self.documents.get(&e.id).map(|d| d.revision) == Some(revision) {
                        continue;
                    }
                    collect(
                        e.id,
                        revision,
                        self.definition.edge_document_validated(e, limits)?,
                    )?;
                }
            }
        }
        for id in self.documents.keys().filter(|id| !present.contains(id)) {
            changed.insert(*id);
            changes.push((*id, None));
        }
        Ok((self.apply_batch(changes, covered_seq, limits)?, changed))
    }
    /// Coalesced updates/removals are idempotent. Maintenance publishes the
    /// resulting generation and advances/cleans durable queue state atomically.
    pub fn apply_batch(
        &self,
        changes: Vec<(u64, Option<Document>)>,
        covered_seq: u64,
        limits: &TextLimits,
    ) -> Result<Self, Error> {
        if !self.complete {
            return Err(fail("cannot maintain a partial fulltext image"));
        }
        if covered_seq < self.covered_seq {
            return Err(fail("fulltext watermark cannot move backwards"));
        }
        let generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| fail("fulltext generation overflow"))?;
        let mut documents = self.documents.clone();
        let mut seen = BTreeSet::new();
        for (id, d) in changes {
            if id == 0 || id >= 1 << 48 || !seen.insert(id) {
                return Err(fail("invalid/duplicate fulltext batch source ID"));
            }
            match d {
                Some(doc) if doc.id == id => {
                    doc.validate(&self.definition, limits)?;
                    documents.insert(id, doc);
                }
                None => {
                    documents.remove(&id);
                }
                _ => return Err(fail("fulltext batch ID mismatch")),
            }
        }
        Self::from_documents(
            self.definition.clone(),
            generation,
            covered_seq,
            documents,
            limits,
        )
    }
    fn fields(&self, options: &SearchOptions) -> Result<Vec<usize>, Error> {
        if options.domain.trim().is_empty() || options.domain.len() > 128 {
            return Err(fail(
                "fulltext requires an explicit nonempty db_type (<=128 bytes)",
            ));
        }
        let names: Vec<_> = self
            .definition
            .fields
            .iter()
            .map(|p| Definition::field_name(p))
            .collect();
        if options.fields.is_empty() {
            return Ok((0..names.len()).collect());
        }
        let mut fields = BTreeSet::new();
        for name in &options.fields {
            let i = names
                .iter()
                .position(|n| n == name)
                .ok_or_else(|| fail("unknown fulltext field"))?;
            if !fields.insert(i) {
                return Err(fail("duplicate fulltext search field"));
            }
        }
        Ok(fields.into_iter().collect())
    }
    pub fn term_ranges(
        &self,
        query: &str,
        options: &SearchOptions,
        limits: &TextLimits,
    ) -> Result<Vec<TermRange>, Error> {
        let fields = self.fields(options)?;
        let terms: BTreeSet<_> = query_tokens(query, options.channel, limits)?
            .into_iter()
            .collect();
        let mut ranges = vec![];
        for term in terms {
            for field in &fields {
                let lower = prefix(&options.domain, *field, options.channel, &term);
                ranges.push(TermRange {
                    field: *field,
                    term: term.clone(),
                    upper: prefix_successor(&lower),
                    lower,
                });
            }
        }
        Ok(ranges)
    }
    /// Keys are exact term prefixes followed by a 32-byte source revision. A
    /// native consumer must reject stale revisions and recheck current sources.
    pub fn native_entries(&self, limits: &TextLimits) -> Result<Vec<(Vec<u8>, u64)>, Error> {
        self.native_entries_for(&self.documents.keys().copied().collect(), limits)
    }
    /// Posting tuples for selected changed documents only. Removed IDs emit no
    /// tuples. Old tuples remain safe because candidates recheck current tokens
    /// and source revisions; a complete REBUILD can compact those stale tuples.
    pub fn native_entries_for(
        &self,
        ids: &BTreeSet<u64>,
        limits: &TextLimits,
    ) -> Result<Vec<(Vec<u8>, u64)>, Error> {
        if !self.complete {
            return Err(fail("cannot publish partial fulltext postings"));
        }
        let mut entries = vec![];
        let mut bytes = 0usize;
        for id in ids {
            if *id == 0 || *id >= 1 << 48 {
                return Err(fail("invalid fulltext posting source ID"));
            }
            if let Some(doc) = self.documents.get(id) {
                let terms: BTreeSet<_> = doc
                    .tokens
                    .iter()
                    .map(|t| (t.field, t.channel, &t.term))
                    .collect();
                for (field, channel, term) in terms {
                    let mut key = prefix(&doc.domain, field, channel, term);
                    key.extend(doc.revision);
                    bytes = bytes.saturating_add(key.len()).saturating_add(32);
                    if key.len() > 4088 || bytes > limits.max_bytes {
                        return Err(fail("fulltext native posting key budget exceeded"));
                    }
                    entries.push((key, *id));
                }
            }
        }
        entries.sort();
        Ok(entries)
    }
    pub fn search(
        &self,
        query: &str,
        options: &SearchOptions,
        limits: &TextLimits,
        revision: impl FnMut(u64) -> Result<Option<[u8; 32]>, Error>,
    ) -> Result<Vec<Hit>, Error> {
        self.search_impl(query, options, limits, None, revision)
    }
    /// Use native B-tree candidates. Every ID and token predicate is rechecked
    /// against the current published document, then the current source revision.
    pub fn search_candidates(
        &self,
        query: &str,
        options: &SearchOptions,
        limits: &TextLimits,
        candidates: &BTreeSet<u64>,
        revision: impl FnMut(u64) -> Result<Option<[u8; 32]>, Error>,
    ) -> Result<Vec<Hit>, Error> {
        self.search_impl(query, options, limits, Some(candidates), revision)
    }
    fn search_impl(
        &self,
        query: &str,
        options: &SearchOptions,
        limits: &TextLimits,
        native_candidates: Option<&BTreeSet<u64>>,
        mut revision: impl FnMut(u64) -> Result<Option<[u8; 32]>, Error>,
    ) -> Result<Vec<Hit>, Error> {
        let fields = self.fields(options)?;
        let terms = query_tokens(query, options.channel, limits)?;
        if let Some(scope) = &self.query_scope {
            if scope
                != &(
                    options.domain.clone(),
                    fields.clone(),
                    options.channel,
                    terms.clone(),
                )
            {
                return Err(fail("partial statistics image is bound to its query"));
            }
        }
        let unique: BTreeSet<_> = terms.iter().cloned().collect();
        let candidates = if let Some(ids) = native_candidates {
            ids.clone()
        } else {
            let mut candidates: Option<BTreeSet<u64>> = None;
            for term in &unique {
                let mut ids = BTreeSet::new();
                for field in &fields {
                    if let Some(d) = self.postings.get(&(
                        options.domain.clone(),
                        *field,
                        options.channel,
                        term.clone(),
                    )) {
                        ids.extend(d.keys());
                    }
                }
                candidates = Some(if let Some(prior) = candidates {
                    prior.intersection(&ids).copied().collect()
                } else {
                    ids
                });
            }
            candidates.unwrap_or_default()
        };
        let mut out = vec![];
        for id in candidates {
            let Some(doc) = self.documents.get(&id) else {
                continue;
            };
            if unique.iter().any(|term| {
                fields.iter().all(|field| {
                    self.postings
                        .get(&(
                            options.domain.clone(),
                            *field,
                            options.channel,
                            term.clone(),
                        ))
                        .map_or(true, |d| !d.contains_key(&id))
                })
            }) {
                continue;
            }

            if revision(id)? != Some(doc.revision) {
                continue;
            }
            let mut total = 0.0;
            let mut best: Option<(f64, usize, usize)> = None;
            for field in &fields {
                let sk = (options.domain.clone(), *field, options.channel);
                let Some(stats) = self.statistics.get(&sk) else {
                    continue;
                };
                let length = doc
                    .tokens
                    .iter()
                    .filter(|t| t.field == *field && t.channel == options.channel)
                    .count();
                if length == 0 {
                    continue;
                }
                let mut score = 0.0;
                let mut matched: Vec<usize> = vec![];
                for term in &unique {
                    let pk = (
                        options.domain.clone(),
                        *field,
                        options.channel,
                        term.clone(),
                    );
                    if let Some(positions) = self.postings.get(&pk).and_then(|d| d.get(&id)) {
                        let df = *stats
                            .df
                            .get(term)
                            .ok_or_else(|| fail("posting has no corpus frequency"))?;
                        let n = stats.documents as f64;
                        let idf = (1.0 + (n - df as f64 + 0.5) / (df as f64 + 0.5)).ln();
                        let avg = stats.length as f64 / n;
                        let tf = positions.len() as f64;
                        score += idf * tf / (tf + 1.2 * (1.0 - 0.75 + 0.75 * length as f64 / avg));
                        matched.extend(positions);
                    }
                }
                let chosen = if options.channel == Channel::Phrase {
                    phrase(doc, *field, &terms)
                } else {
                    matched
                        .iter()
                        .min_by_key(|i| (doc.tokens[**i].segment, doc.tokens[**i].start))
                        .copied()
                };
                let Some(token) = chosen else {
                    continue;
                };
                total += score;
                if best
                    .as_ref()
                    .map_or(true, |(s, f, _)| score > *s || (score == *s && *field < *f))
                {
                    best = Some((score, *field, token));
                }
            }
            let Some((_, field, token)) = best else {
                continue;
            };
            let t = &doc.tokens[token];
            let text = &doc.fields[field][t.segment];
            if out.len() >= limits.max_hits {
                return Err(fail(
                    "fulltext hit budget exceeded; no silently truncated result",
                ));
            }
            out.push(Hit {
                id,
                score: total,
                field: Definition::field_name(&self.definition.fields[field]),
                snippet: snippet(text, t.start, t.end),
                offset: t.start,
                line: text[..t.start].bytes().filter(|b| *b == b'\n').count() + 1,
            });
        }
        out.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.id.cmp(&b.id)));
        Ok(out)
    }
    pub fn native_rows(&self, limits: &TextLimits) -> Result<BTreeMap<u64, Vec<u8>>, Error> {
        self.native_rows_for(&self.documents.keys().copied().collect(), limits)
    }
    /// Manifest plus selected documents. Suitable for diffing a maintenance
    /// batch; this is a partial image, not independently a complete generation.
    pub fn native_rows_for(
        &self,
        ids: &BTreeSet<u64>,
        limits: &TextLimits,
    ) -> Result<BTreeMap<u64, Vec<u8>>, Error> {
        if !self.complete {
            return Err(fail("cannot publish a partial fulltext image"));
        }
        let mut rows = BTreeMap::new();
        let mut bytes = 0usize;
        let mut header = json!({"format":FORMAT,"definition":self.definition.json(),"generation":self.generation,"covered_seq":self.covered_seq,"documents":self.documents.len()});
        if self.native_format >= 3 {
            let (count, root) = self.keyed_statistics_rows(&mut rows, &mut bytes, limits)?;
            header["format"] = json!(FORMAT_V3);
            header["statistics_pages"] = json!(count);
            header["statistics_sha256"] = json!(hex(&root));
        } else if self.native_format == 2 {
            let pages = self.statistics_pages()?;
            let mut digest = Sha256::new();
            for (i, page) in pages.iter().enumerate() {
                digest.update(&serde_json::to_vec(page).map_err(|e| fail(e.to_string()))?);
                put_record(
                    &mut rows,
                    STAT_KIND | ((i as u64 + 1) << 10),
                    page,
                    &mut bytes,
                    limits,
                )?;
            }
            header["format"] = json!(FORMAT_V2);
            header["statistics_pages"] = json!(pages.len());
            header["statistics_sha256"] = json!(hex(&digest.finalize()));
        }
        if self.native_format == 4 {
            let mut corpus = DocumentBudget::default();
            let mut document_rows = 0usize;
            for doc in self.documents.values() {
                corpus.add(doc, limits)?;
                let encoded = serde_json::to_vec(&doc.json()).map_err(|e| fail(e.to_string()))?;
                document_rows = document_rows
                    .saturating_add(encoded.len())
                    .saturating_add(32);
            }
            let mut budget = CorpusBudget {
                tokens: corpus.tokens,
                document_bytes: corpus.bytes,
                native_bytes: 0,
            };
            header["format"] = json!(FORMAT_V4);
            header["corpus_budget"] = budget.json();
            budget.native_bytes = rows
                .values()
                .map(Vec::len)
                .sum::<usize>()
                .saturating_add(document_rows)
                .saturating_add(32)
                .saturating_add(
                    serde_json::to_vec(&header)
                        .map_err(|e| fail(e.to_string()))?
                        .len(),
                );
            if budget.native_bytes > limits.max_bytes {
                return Err(fail("fulltext native corpus byte budget exceeded"));
            }
            header["corpus_budget"] = budget.json();
        }
        put_record(&mut rows, 0, &header, &mut bytes, limits)?;
        for id in ids {
            if *id == 0 || *id >= 1 << 48 {
                return Err(fail("invalid fulltext document source ID"));
            }
            let Some(doc) = self.documents.get(id) else {
                continue;
            };
            put_record(
                &mut rows,
                DOC_KIND | (doc.id << 10),
                &doc.json(),
                &mut bytes,
                limits,
            )?;
        }
        Ok(rows)
    }
    pub fn manifest_from_native_rows(
        rows: &BTreeMap<u64, Vec<u8>>,
        limits: &TextLimits,
    ) -> Result<GenerationManifest, Error> {
        if rows
            .range(0..=CHUNKS)
            .fold(0usize, |n, (_, v)| n.saturating_add(v.len()))
            > limits.max_bytes
        {
            return Err(fail("fulltext manifest byte budget exceeded"));
        }
        let header = get_record(rows, 0)?;
        let storage_format = if header["format"] == FORMAT_V4 {
            4
        } else if header["format"] == FORMAT_V3 {
            3
        } else if header["format"] == FORMAT_V2 {
            2
        } else {
            1
        };
        let v2 = storage_format != 1;
        if header.as_object().map_or(0, |h| h.len())
            != if storage_format == 4 {
                8
            } else if v2 {
                7
            } else {
                5
            }
            || !v2 && header["format"] != FORMAT
        {
            return Err(fail("unsupported fulltext manifest"));
        }
        let manifest = GenerationManifest {
            definition: Definition::from_json(&header["definition"])?,
            generation: header["generation"]
                .as_u64()
                .ok_or_else(|| fail("invalid fulltext generation"))?,
            covered_seq: header["covered_seq"]
                .as_u64()
                .ok_or_else(|| fail("invalid fulltext watermark"))?,
            documents: unsigned(&header["documents"])?,
            storage_format,
            corpus_budget: if storage_format == 4 {
                Some(CorpusBudget::decode(
                    &header["corpus_budget"],
                    unsigned(&header["documents"])?,
                    limits,
                )?)
            } else {
                None
            },
            statistics_pages: if v2 {
                Some(unsigned(&header["statistics_pages"])?)
            } else {
                None
            },
        };
        if manifest.generation == 0
            || manifest.storage_format >= 3 && manifest.statistics_pages.is_some_and(|n| n > 4096)
            || manifest
                .statistics_pages
                .is_some_and(|n| n > limits.max_tokens)
        {
            return Err(fail("invalid fulltext generation/statistics count"));
        }
        if v2 {
            hash_decode(
                header["statistics_sha256"]
                    .as_str()
                    .ok_or_else(|| fail("invalid fulltext statistics checksum"))?,
            )?;
        }
        if manifest.documents > limits.max_documents {
            return Err(fail("fulltext document count budget exceeded"));
        }
        Ok(manifest)
    }
    /// Load native checksummed document records without invoking the analyzer.
    /// Unknown ordinals, broken chunks, malformed positions and revisions fail.
    pub fn from_native_rows(
        rows: &BTreeMap<u64, Vec<u8>>,
        limits: &TextLimits,
    ) -> Result<Self, Error> {
        let mut bytes = 0usize;
        for data in rows.values() {
            bytes = bytes.saturating_add(data.len());
            if bytes > limits.max_bytes {
                return Err(fail("fulltext stored byte budget exceeded"));
            }
        }
        let GenerationManifest {
            definition,
            generation,
            covered_seq,
            documents: count,
            statistics_pages,
            storage_format,
            corpus_budget,
        } = Self::manifest_from_native_rows(rows, limits)?;
        let mut ids = BTreeSet::new();
        for key in rows.keys() {
            if *key <= CHUNKS {
                continue;
            }
            if (key >> 60 == 2 && statistics_pages.is_some())
                || (key >> 60 == 3 && storage_format >= 3)
            {
                continue;
            }
            if key >> 60 != 1 {
                return Err(fail("invalid fulltext record kind"));
            }
            let id = (key & ((1 << 60) - 1)) >> 10;
            if id == 0 || id >= 1 << 48 {
                return Err(fail("invalid fulltext record ID"));
            }
            ids.insert(id);
        }
        if ids.len() != count {
            return Err(fail("fulltext manifest document count mismatch"));
        }
        let mut documents = BTreeMap::new();
        let mut budget = DocumentBudget::default();
        for id in ids {
            let doc = Document::from_json(
                &get_record(rows, DOC_KIND | (id << 10))?,
                &definition,
                limits,
            )?;
            if doc.id != id {
                return Err(fail("fulltext document ID differs from record key"));
            }
            budget.add(&doc, limits)?;
            documents.insert(id, doc);
        }
        let mut image =
            Self::from_documents(definition, generation, covered_seq, documents, limits)?;
        image.native_format = storage_format;
        if let Some(corpus) = corpus_budget {
            if corpus.tokens != budget.tokens
                || corpus.document_bytes != budget.bytes
                || corpus.native_bytes != bytes
            {
                return Err(fail(
                    "fulltext persistent corpus budget differs from documents/records",
                ));
            }
        }
        if storage_format >= 3 {
            let manifest = Self::manifest_from_native_rows(rows, limits)?;
            if image.statistics != image.decode_keyed_statistics(rows, &manifest, limits)? {
                return Err(fail("persisted keyed statistics differ from documents"));
            }
        } else if let Some(pages) = statistics_pages {
            if image.statistics != image.decode_statistics(rows, pages, count, limits)? {
                return Err(fail("persisted fulltext statistics differ from documents"));
            }
        }
        Ok(image)
    }

    /// Header-only descriptor; use it to validate terms before opening documents.
    pub fn metadata(manifest: &GenerationManifest) -> Self {
        Self {
            definition: manifest.definition.clone(),
            generation: manifest.generation,
            covered_seq: manifest.covered_seq,
            documents: BTreeMap::new(),
            postings: BTreeMap::new(),
            statistics: BTreeMap::new(),
            native_format: manifest.storage_format,
            query_scope: None,
            complete: false,
        }
    }
    /// Native ordinal range for all independently checksummed statistics pages.
    pub fn statistics_range(manifest: &GenerationManifest) -> Option<(u64, u64)> {
        manifest.statistics_pages.map(|n| {
            if manifest.storage_format >= 3 {
                (STAT_KIND, (3u64 << 60) + ((65u64) << 10) - 1)
            } else {
                (STAT_KIND, STAT_KIND + ((n as u64 + 1) << 10) - 1)
            }
        })
    }
    /// Native ordinal range for one candidate document, including checksum.
    pub fn document_range(id: u64) -> Result<(u64, u64), Error> {
        if id == 0 || id >= 1 << 48 {
            return Err(fail("invalid fulltext candidate ID"));
        }
        let base = DOC_KIND | (id << 10);
        Ok((base, base + CHUNKS))
    }
    /// A search-only v2 image: global BM25 metadata plus selected documents.
    /// Missing candidate records are legitimate stale B-tree entries.
    pub fn from_candidate_rows(
        rows: &BTreeMap<u64, Vec<u8>>,
        candidates: &BTreeSet<u64>,
        limits: &TextLimits,
    ) -> Result<Self, Error> {
        let manifest = Self::manifest_from_native_rows(rows, limits)?;
        let pages = manifest
            .statistics_pages
            .ok_or_else(|| fail("v1 fulltext requires complete document loading"))?;
        let mut stored = 0usize;
        for (key, data) in rows {
            stored = stored.saturating_add(data.len());
            if stored > limits.max_bytes {
                return Err(fail("fulltext partial record byte budget exceeded"));
            }
            if *key > CHUNKS
                && key >> 60 != 2
                && !(key >> 60 == 3 && manifest.storage_format >= 3)
                && (key >> 60 != 1 || !candidates.contains(&((key & ((1 << 60) - 1)) >> 10)))
            {
                return Err(fail("unrequested fulltext document record"));
            }
        }
        let mut image = Self::metadata(&manifest);
        let statistics = if manifest.storage_format >= 3 {
            image.decode_keyed_statistics(rows, &manifest, limits)?
        } else {
            image.decode_statistics(rows, pages, manifest.documents, limits)?
        };
        let mut documents = BTreeMap::new();
        let mut budget = DocumentBudget::default();
        for id in candidates {
            let (base, end) = Self::document_range(*id)?;
            if rows.range(base..=end).next().is_none() {
                continue;
            }
            let doc = Document::from_json(&get_record(rows, base)?, &manifest.definition, limits)?;
            if doc.id != *id {
                return Err(fail("fulltext document ID differs from record key"));
            }
            budget.add(&doc, limits)?;
            documents.insert(*id, doc);
        }
        if documents.len() > manifest.documents {
            return Err(fail("partial fulltext document count exceeds manifest"));
        }
        image = Self::from_documents(
            manifest.definition,
            manifest.generation,
            manifest.covered_seq,
            documents,
            limits,
        )?;
        // Every selected token must have valid global corpus statistics.
        for (key, local) in &image.statistics {
            let global = statistics
                .get(key)
                .ok_or_else(|| fail("missing fulltext corpus statistics"))?;
            if local.documents > global.documents
                || local.length > global.length
                || local
                    .df
                    .iter()
                    .any(|(t, df)| global.df.get(t).map_or(true, |n| df > n))
            {
                return Err(fail("partial fulltext statistics exceed corpus"));
            }
        }
        image.statistics = statistics;
        image.native_format = manifest.storage_format;
        image.complete = false;
        Ok(image)
    }
    fn statistics_pages(&self) -> Result<Vec<Json>, Error> {
        let mut pages = Vec::new();
        let mut page = Vec::new();
        let mut bytes = 2usize;
        for ((domain, field, channel), stats) in &self.statistics {
            for (term, df) in &stats.df {
                let value = json!([
                    domain,
                    field,
                    channel.number(),
                    stats.documents,
                    stats.length,
                    term,
                    df
                ]);
                let size = serde_json::to_vec(&value)
                    .map_err(|e| fail(e.to_string()))?
                    .len()
                    + 1;
                if !page.is_empty() && bytes + size > CHUNK {
                    pages.push(Json::Array(std::mem::take(&mut page)));
                    bytes = 2;
                }
                bytes += size;
                page.push(value);
            }
        }
        if !page.is_empty() {
            pages.push(Json::Array(page));
        }
        Ok(pages)
    }
    fn decode_statistics(
        &self,
        rows: &BTreeMap<u64, Vec<u8>>,
        pages: usize,
        documents: usize,
        limits: &TextLimits,
    ) -> Result<BTreeMap<StatisticsKey, Statistics>, Error> {
        let mut stats = BTreeMap::<StatisticsKey, Statistics>::new();
        let mut digest = Sha256::new();
        let mut previous = None;
        let mut count = 0usize;
        for key in rows.keys().filter(|k| **k >> 60 == 2) {
            let page = (key & ((1 << 60) - 1)) >> 10;
            if page == 0 || page > pages as u64 {
                return Err(fail("unexpected fulltext statistics record"));
            }
        }
        for i in 0..pages {
            let page = get_record(rows, STAT_KIND | ((i as u64 + 1) << 10))?;
            digest.update(&serde_json::to_vec(&page).map_err(|e| fail(e.to_string()))?);
            let entries = array(&page)?;
            if entries.is_empty() {
                return Err(fail("empty fulltext statistics page"));
            }
            for entry in entries {
                let a = array(entry)?;
                if a.len() != 7 {
                    return Err(fail("invalid fulltext statistics entry"));
                }
                let domain = a[0]
                    .as_str()
                    .filter(|d| !d.trim().is_empty() && d.len() <= 128)
                    .ok_or_else(|| fail("invalid statistics domain"))?
                    .to_owned();
                let field = unsigned(&a[1])?;
                let channel = Channel::decode(
                    a[2].as_u64()
                        .ok_or_else(|| fail("invalid statistics channel"))?,
                )?;
                let n = unsigned(&a[3])?;
                let length = unsigned(&a[4])?;
                let term = a[5]
                    .as_str()
                    .filter(|t| !t.is_empty() && t.len() <= TERM_BYTES && t.to_lowercase() == *t)
                    .ok_or_else(|| fail("invalid statistics term"))?
                    .to_owned();
                let df = unsigned(&a[6])?;
                if field >= self.definition.fields.len()
                    || n == 0
                    || n > documents
                    || length < n
                    || length > limits.max_tokens
                    || df == 0
                    || df > n
                {
                    return Err(fail("invalid statistics counts"));
                }
                let order = (domain.clone(), field, channel, term.clone());
                if previous.as_ref().is_some_and(|old| old >= &order) {
                    return Err(fail("out-of-order/duplicate fulltext statistics"));
                }
                previous = Some(order);
                let s = stats
                    .entry((domain, field, channel))
                    .or_insert_with(|| Statistics {
                        documents: n,
                        length,
                        df: BTreeMap::new(),
                    });
                if s.documents != n || s.length != length {
                    return Err(fail("inconsistent statistics group"));
                }
                s.df.insert(term, df);
                count += 1;
                if count > limits.max_tokens {
                    return Err(fail("fulltext statistics token budget exceeded"));
                }
            }
        }
        let mut total_tokens = 0usize;
        for s in stats.values() {
            total_tokens = total_tokens.saturating_add(s.length);
            if total_tokens > limits.max_tokens {
                return Err(fail("fulltext corpus statistics token budget exceeded"));
            }
            let sum =
                s.df.values()
                    .try_fold(0usize, |n, v| n.checked_add(*v))
                    .ok_or_else(|| fail("statistics count overflow"))?;
            if sum < s.documents || sum > s.length {
                return Err(fail("inconsistent statistics frequencies"));
            }
        }
        let header = get_record(rows, 0)?;
        let expected = hash_decode(
            header["statistics_sha256"]
                .as_str()
                .ok_or_else(|| fail("missing statistics checksum"))?,
        )?;
        if digest.finalize() != expected {
            return Err(fail("fulltext corpus statistics checksum mismatch"));
        }
        Ok(stats)
    }
}
fn phrase(doc: &Document, field: usize, terms: &[String]) -> Option<usize> {
    let tokens: Vec<_> = doc
        .tokens
        .iter()
        .enumerate()
        .filter(|(_, t)| t.channel == Channel::Phrase && t.field == field)
        .collect();
    tokens.windows(terms.len()).find_map(|window| {
        window
            .iter()
            .zip(terms)
            .enumerate()
            .all(|(i, ((_, t), term))| {
                t.term == *term
                    && t.segment == window[0].1.segment
                    && t.position == window[0].1.position + i
            })
            .then_some(window[0].0)
    })
}
fn snippet(text: &str, start: usize, end: usize) -> String {
    let mut lo = start.saturating_sub(96);
    while !text.is_char_boundary(lo) {
        lo += 1;
    }
    let mut hi = end
        .saturating_add(128)
        .min(text.len())
        .min(lo.saturating_add(256));
    while !text.is_char_boundary(hi) {
        hi -= 1;
    }
    text[lo..hi].into()
}
/// Field round-robin fusion uses field-local ordering, never compares scores
/// from unrelated indexes/domains. Explicit LIMIT is applied after deduplication.
pub fn round_robin(fields: &[Vec<Hit>], limit: usize) -> Vec<Hit> {
    let mut out = vec![];
    let mut seen = BTreeSet::new();
    let max = fields.iter().map(Vec::len).max().unwrap_or(0);
    for i in 0..max {
        for field in fields {
            if let Some(hit) = field.get(i) {
                if seen.insert(hit.id) {
                    if out.len() == limit {
                        return out;
                    }
                    out.push(hit.clone());
                }
            }
        }
    }
    out
}
impl Document {
    fn validate(&self, definition: &Definition, limits: &TextLimits) -> Result<(), Error> {
        if self.id == 0
            || self.id >= 1 << 48
            || self.domain.trim().is_empty()
            || self.domain.len() > 128
            || self.fields.len() != definition.fields.len()
        {
            return Err(fail("invalid fulltext document identity/field shape"));
        }
        let mut bytes = 0usize;
        for segments in &self.fields {
            bytes = bytes.saturating_add(segments.len().saturating_mul(24));
            for text in segments {
                bytes = bytes.saturating_add(text.len());
            }
        }
        if bytes > limits.max_bytes || self.tokens.len() > limits.max_tokens {
            return Err(fail("fulltext document memory/token budget exceeded"));
        }
        let mut last = BTreeMap::new();
        let mut unique = BTreeSet::new();
        for t in &self.tokens {
            let text = self
                .fields
                .get(t.field)
                .and_then(|f| f.get(t.segment))
                .ok_or_else(|| fail("invalid fulltext token location"))?;
            if t.start >= t.end
                || t.end > text.len()
                || !text.is_char_boundary(t.start)
                || !text.is_char_boundary(t.end)
                || t.term.is_empty()
                || t.term.len() > TERM_BYTES
                || t.position > limits.max_tokens
                || text[t.start..t.end].to_lowercase() != t.term
            {
                return Err(fail("invalid fulltext token offsets/value"));
            }
            if !unique.insert((t.field, t.segment, t.channel, t.position, t.term.as_str())) {
                return Err(fail("duplicate fulltext token position"));
            }
            if let Some((position, start)) =
                last.insert((t.field, t.segment, t.channel), (t.position, t.start))
            {
                if position > t.position
                    || start > t.start
                    || (t.channel != Channel::Terms && (position == t.position || start == t.start))
                {
                    return Err(fail("out-of-order fulltext positions"));
                }
            }
        }
        Ok(())
    }
    fn json(&self) -> Json {
        json!({"id":self.id,"domain":self.domain,"revision":hex(&self.revision),"fields":self.fields,"tokens":self.tokens.iter().map(|t|json!([t.term,t.channel.number(),t.field,t.segment,t.position,t.start,t.end])).collect::<Vec<_>>(),"diagnostics":[self.diagnostics.missing,self.diagnostics.null,self.diagnostics.excluded_type,self.diagnostics.oversized_terms]})
    }
    fn from_json(v: &Json, definition: &Definition, limits: &TextLimits) -> Result<Self, Error> {
        let obj = v
            .as_object()
            .ok_or_else(|| fail("invalid fulltext document"))?;
        if obj.len() != 6 {
            return Err(fail("invalid fulltext document fields"));
        }
        let id = v["id"]
            .as_u64()
            .ok_or_else(|| fail("invalid fulltext document ID"))?;
        let domain = v["domain"]
            .as_str()
            .ok_or_else(|| fail("invalid fulltext domain"))?
            .to_owned();
        if id == 0 || id >= 1 << 48 || domain.trim().is_empty() || domain.len() > 128 {
            return Err(fail("invalid fulltext document identity/domain"));
        }
        let revision = hash_decode(
            v["revision"]
                .as_str()
                .ok_or_else(|| fail("invalid fulltext revision"))?,
        )?;
        let mut fields = vec![];
        for f in array(&v["fields"])? {
            fields.push(strings(f)?);
        }
        if fields.len() != definition.fields.len() {
            return Err(fail("fulltext document field shape mismatch"));
        }
        let values = array(&v["diagnostics"])?;
        if values.len() != 4 {
            return Err(fail("invalid fulltext diagnostics"));
        }
        let diagnostics = Diagnostics {
            missing: unsigned(&values[0])?,
            null: unsigned(&values[1])?,
            excluded_type: unsigned(&values[2])?,
            oversized_terms: unsigned(&values[3])?,
        };
        let mut tokens = vec![];
        for token in array(&v["tokens"])? {
            if tokens.len() >= limits.max_tokens {
                return Err(fail("fulltext stored token budget exceeded"));
            }
            let a = array(token)?;
            if a.len() != 7 {
                return Err(fail("invalid fulltext token shape"));
            }
            let term = a[0]
                .as_str()
                .ok_or_else(|| fail("invalid fulltext token term"))?
                .to_owned();
            let channel = Channel::decode(
                a[1].as_u64()
                    .ok_or_else(|| fail("invalid fulltext token channel"))?,
            )?;
            let t = Token {
                term,
                channel,
                field: unsigned(&a[2])?,
                segment: unsigned(&a[3])?,
                position: unsigned(&a[4])?,
                start: unsigned(&a[5])?,
                end: unsigned(&a[6])?,
            };
            tokens.push(t);
        }
        let doc = Self {
            id,
            domain,
            revision,
            fields,
            tokens,
            diagnostics,
        };
        doc.validate(definition, limits)?;
        Ok(doc)
    }
}
fn put_record(
    rows: &mut BTreeMap<u64, Vec<u8>>,
    base: u64,
    record: &Json,
    total: &mut usize,
    limits: &TextLimits,
) -> Result<(), Error> {
    let bytes = serde_json::to_vec(record).map_err(|e| fail(e.to_string()))?;
    if bytes.len().div_ceil(CHUNK) > CHUNKS as usize {
        return Err(fail("fulltext record exceeds 1023 chunks"));
    }
    *total = total.saturating_add(bytes.len()).saturating_add(32);
    if *total > limits.max_bytes {
        return Err(fail("fulltext native record byte budget exceeded"));
    }
    rows.insert(base, sha(&bytes).to_vec());
    for (i, chunk) in bytes.chunks(CHUNK).enumerate() {
        rows.insert(base + i as u64 + 1, chunk.to_vec());
    }
    Ok(())
}
fn get_record(rows: &BTreeMap<u64, Vec<u8>>, base: u64) -> Result<Json, Error> {
    let checksum = rows
        .get(&base)
        .ok_or_else(|| fail("missing fulltext record checksum"))?;
    if checksum.len() != 32 {
        return Err(fail("invalid fulltext record checksum"));
    }
    let mut bytes = vec![];
    for (i, (&key, data)) in rows.range(base + 1..=base + CHUNKS).enumerate() {
        if key != base + i as u64 + 1
            || data.is_empty()
            || data.len() > CHUNK
            || (!bytes.is_empty() && bytes.len() % CHUNK != 0)
        {
            return Err(fail("noncontiguous fulltext chunks"));
        }
        bytes.extend(data);
    }
    if bytes.is_empty() || sha(&bytes).as_slice() != checksum {
        return Err(fail("fulltext record checksum mismatch"));
    }
    serde_json::from_slice(&bytes).map_err(|_| fail("invalid fulltext record JSON"))
}
