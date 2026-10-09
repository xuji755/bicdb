//! Cold batch maintenance: complete committed statistics plus only affected
//! documents. Unchanged document bodies and postings are never reconstructed.
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CorpusBudget {
    pub tokens: usize,
    pub document_bytes: usize,
    pub native_bytes: usize,
}
impl CorpusBudget {
    pub(super) fn json(self) -> Json {
        json!({"tokens":self.tokens,"document_bytes":self.document_bytes,
            "native_bytes":format!("{:020}",self.native_bytes)})
    }
    pub(super) fn decode(
        value: &Json,
        documents: usize,
        limits: &TextLimits,
    ) -> Result<Self, Error> {
        if value.as_object().map_or(0, |v| v.len()) != 3 {
            return Err(fail("invalid fulltext corpus budget"));
        }
        let native = value["native_bytes"]
            .as_str()
            .filter(|s| s.len() == 20 && s.bytes().all(|b| b.is_ascii_digit()))
            .ok_or_else(|| fail("invalid native corpus byte count"))?;
        let result = Self {
            tokens: unsigned(&value["tokens"])?,
            document_bytes: unsigned(&value["document_bytes"])?,
            native_bytes: native
                .parse()
                .map_err(|_| fail("native corpus byte count overflow"))?,
        };
        result.validate(documents, limits)?;
        Ok(result)
    }
    fn validate(self, documents: usize, limits: &TextLimits) -> Result<(), Error> {
        if documents > limits.max_documents
            || self.tokens > limits.max_tokens
            || self.document_bytes > limits.max_bytes
            || self.document_bytes < documents.saturating_mul(128)
            || self.native_bytes > limits.max_bytes
            || self.native_bytes < 32
        {
            return Err(fail("fulltext global corpus budget exceeded"));
        }
        Ok(())
    }
}

/// Validated before/after rows for a native transaction; this is not a partial
/// Generation that could be accidentally published as a complete corpus.
pub struct NativeBatch {
    pub before: BTreeMap<u64, Vec<u8>>,
    pub after: BTreeMap<u64, Vec<u8>>,
    pub entries: Vec<(Vec<u8>, u64)>,
    pub changed: BTreeSet<u64>,
    pub manifest: GenerationManifest,
    pub documents_loaded: usize,
}
fn document_budget(doc: &Document, limits: &TextLimits) -> Result<DocumentBudget, Error> {
    let mut budget = DocumentBudget::default();
    budget.add(doc, limits)?;
    Ok(budget)
}
fn document_groups(
    doc: &Document,
) -> (
    BTreeMap<StatisticsKey, usize>,
    BTreeSet<(StatisticsKey, String)>,
) {
    let mut lengths = BTreeMap::new();
    let mut present = BTreeSet::new();
    for t in &doc.tokens {
        let key = (doc.domain.clone(), t.field, t.channel);
        *lengths.entry(key.clone()).or_default() += 1;
        present.insert((key, t.term.clone()));
    }
    (lengths, present)
}
fn remove_statistics(
    stats: &mut BTreeMap<StatisticsKey, Statistics>,
    doc: &Document,
) -> Result<(), Error> {
    let (lengths, present) = document_groups(doc);
    for (key, term) in present {
        let group = stats
            .get_mut(&key)
            .ok_or_else(|| fail("missing old document statistics"))?;
        let df = group
            .df
            .get_mut(&term)
            .ok_or_else(|| fail("missing old document frequency"))?;
        *df = df
            .checked_sub(1)
            .ok_or_else(|| fail("document frequency underflow"))?;
        if *df == 0 {
            group.df.remove(&term);
        }
    }
    for (key, length) in lengths {
        let group = stats
            .get_mut(&key)
            .ok_or_else(|| fail("missing old statistics group"))?;
        group.documents = group
            .documents
            .checked_sub(1)
            .ok_or_else(|| fail("statistics document underflow"))?;
        group.length = group
            .length
            .checked_sub(length)
            .ok_or_else(|| fail("statistics length underflow"))?;
        if group.documents == 0 {
            if group.length != 0 || !group.df.is_empty() {
                return Err(fail("nonempty zero-document statistics"));
            }
            stats.remove(&key);
        }
    }
    Ok(())
}
fn add_statistics(
    stats: &mut BTreeMap<StatisticsKey, Statistics>,
    doc: &Document,
) -> Result<(), Error> {
    let (lengths, present) = document_groups(doc);
    for (key, length) in lengths {
        let group = stats.entry(key).or_default();
        group.documents = group
            .documents
            .checked_add(1)
            .ok_or_else(|| fail("statistics document overflow"))?;
        group.length = group
            .length
            .checked_add(length)
            .ok_or_else(|| fail("statistics length overflow"))?;
    }
    for (key, term) in present {
        let df = stats.entry(key).or_default().df.entry(term).or_default();
        *df = df
            .checked_add(1)
            .ok_or_else(|| fail("document frequency overflow"))?;
    }
    Ok(())
}
fn replace_count(value: usize, removed: usize, added: usize) -> Result<usize, Error> {
    value
        .checked_sub(removed)
        .and_then(|v| v.checked_add(added))
        .ok_or_else(|| fail("corpus counter underflow/overflow"))
}
fn bytes(rows: &BTreeMap<u64, Vec<u8>>) -> usize {
    rows.values().fold(0usize, |n, v| n.saturating_add(v.len()))
}
impl Generation {
    /// Storage format of this logical descriptor, without opening documents.
    pub fn storage_format(&self) -> u8 {
        self.native_format
    }

    /// Decode exactly one old document for WAIT's applied-revision proof.
    pub fn document_from_native_rows(
        manifest: &GenerationManifest,
        id: u64,
        rows: &BTreeMap<u64, Vec<u8>>,
        limits: &TextLimits,
    ) -> Result<Option<Document>, Error> {
        let (lower, upper) = Self::document_range(id)?;
        if rows.is_empty() {
            return Ok(None);
        }
        if manifest.documents == 0
            || bytes(rows) > limits.max_bytes
            || rows.keys().any(|k| *k < lower || *k > upper)
        {
            return Err(fail("unrequested/oversized old document records"));
        }
        let doc = Document::from_json(&get_record(rows, lower)?, &manifest.definition, limits)?;
        if doc.id != id {
            return Err(fail("fulltext document ID differs from record key"));
        }
        Ok(Some(doc))
    }

    /// Apply a coalesced batch to a v4 header, all committed statistics/proofs,
    /// and exactly the requested old document records. No unaffected bodies.
    pub fn apply_native_batch(
        rows: &BTreeMap<u64, Vec<u8>>,
        changes: Vec<(u64, Option<Document>)>,
        covered_seq: u64,
        limits: &TextLimits,
    ) -> Result<NativeBatch, Error> {
        let manifest = Self::manifest_from_native_rows(rows, limits)?;
        if manifest.storage_format != 4 {
            return Err(fail("sparse maintenance requires v4 corpus budget"));
        }
        if covered_seq < manifest.covered_seq {
            return Err(fail("fulltext watermark cannot move backwards"));
        }
        let mut requested = BTreeSet::new();
        for (id, doc) in &changes {
            Self::document_range(*id)?;
            if !requested.insert(*id) {
                return Err(fail("duplicate fulltext batch source ID"));
            }
            if let Some(doc) = doc {
                if doc.id != *id {
                    return Err(fail("fulltext batch ID mismatch"));
                }
                doc.validate(&manifest.definition, limits)?;
            }
        }
        let old = Self::from_candidate_rows(rows, &requested, limits)?;
        let mut budget = manifest
            .corpus_budget
            .ok_or_else(|| fail("missing corpus budget"))?;
        if bytes(rows) > budget.native_bytes {
            return Err(fail("partial records exceed native corpus budget"));
        }
        let mut statistics = old.statistics;
        if statistics.values().map(|s| s.length).sum::<usize>() != budget.tokens {
            return Err(fail("statistics token count differs from corpus budget"));
        }
        let mut count = manifest.documents;
        let mut documents = old.documents;
        let documents_loaded = documents.len();
        let mut changed = BTreeSet::new();
        for (id, new) in changes {
            if documents.get(&id) == new.as_ref() {
                continue;
            }
            changed.insert(id);
            if let Some(prior) = documents.remove(&id) {
                let cost = document_budget(&prior, limits)?;
                budget.tokens = replace_count(budget.tokens, cost.tokens, 0)?;
                budget.document_bytes = replace_count(budget.document_bytes, cost.bytes, 0)?;
                count = replace_count(count, 1, 0)?;
                remove_statistics(&mut statistics, &prior)?;
            }
            if let Some(doc) = new {
                let cost = document_budget(&doc, limits)?;
                budget.tokens = replace_count(budget.tokens, 0, cost.tokens)?;
                budget.document_bytes = replace_count(budget.document_bytes, 0, cost.bytes)?;
                count = replace_count(count, 0, 1)?;
                add_statistics(&mut statistics, &doc)?;
                documents.insert(id, doc);
            }
        }
        // Corpus budgets apply to all documents, not just this small batch.
        budget.validate(count, limits)?;
        let mut metadata = Self::metadata(&manifest);
        metadata.statistics = statistics;
        metadata.generation = manifest
            .generation
            .checked_add(1)
            .ok_or_else(|| fail("fulltext generation overflow"))?;
        metadata.covered_seq = covered_seq;
        let mut after = BTreeMap::new();
        let mut size = 0usize;
        let (pages, root) = metadata.keyed_statistics_rows(&mut after, &mut size, limits)?;
        for doc in documents.values() {
            put_record(
                &mut after,
                DOC_KIND | (doc.id << 10),
                &doc.json(),
                &mut size,
                limits,
            )?;
        }
        let mut header = json!({"format":FORMAT_V4,"definition":manifest.definition.json(),
            "generation":metadata.generation,"covered_seq":covered_seq,"documents":count,
            "statistics_pages":pages,"statistics_sha256":hex(&root),"corpus_budget":budget.json()});
        put_record(&mut after, 0, &header, &mut size, limits)?;
        budget.native_bytes = replace_count(budget.native_bytes, bytes(rows), bytes(&after))?;
        budget.validate(count, limits)?;
        header["corpus_budget"] = budget.json();
        after.retain(|k, _| *k > CHUNKS);
        // Fixed-width native byte count keeps header size stable across this update.
        put_record(&mut after, 0, &header, &mut 0, limits)?;
        let next_manifest = Self::manifest_from_native_rows(&after, limits)?;
        let next_stats = metadata.decode_keyed_statistics(&after, &next_manifest, limits)?;
        if next_stats.values().map(|s| s.length).sum::<usize>() != budget.tokens {
            return Err(fail("batch statistics token budget differs"));
        }
        let entries = Self::from_documents(
            manifest.definition,
            metadata.generation,
            covered_seq,
            documents,
            limits,
        )?
        .native_entries_for(&changed, limits)?;
        Ok(NativeBatch {
            before: rows.clone(),
            after,
            entries,
            changed,
            manifest: next_manifest,
            documents_loaded,
        })
    }
}
