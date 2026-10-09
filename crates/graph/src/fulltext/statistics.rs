//! Deterministic statistics buckets with a sparse Merkle commitment. Querying
//! a few terms reads their buckets and proof pages, never the whole vocabulary.
use super::*;

const BUCKETS: usize = 4096;
const HASH_KIND: u64 = 3 << 60;
const HASH_PAGE: usize = 128;
type Scope = (String, Vec<usize>, Channel, Vec<String>);

/// Native records required to verify the statistics for one normalized query.
pub struct StatisticsSelection {
    definition: Definition,
    keys: BTreeSet<PostingKey>,
    buckets: BTreeSet<usize>,
    pages: BTreeSet<usize>,
    scope: Scope,
}
impl StatisticsSelection {
    /// Checksum/chunk ranges, deduplicated across fields, terms and proof paths.
    pub fn ranges(&self) -> Vec<(u64, u64)> {
        self.buckets
            .iter()
            .map(|b| STAT_KIND | ((*b as u64 + 1) << 10))
            .chain(self.pages.iter().map(|p| HASH_KIND | ((*p as u64) << 10)))
            .map(|base| (base, base + CHUNKS))
            .collect()
    }
}
fn bucket(key: &PostingKey) -> usize {
    let h = sha(&prefix(&key.0, key.1, key.2, &key.3));
    usize::from(u16::from_be_bytes([h[0], h[1]]) >> 4)
}
fn parent(left: [u8; 32], right: [u8; 32]) -> [u8; 32] {
    let mut data = b"bic-fulltext-stat-parent-v3".to_vec();
    data.extend(left);
    data.extend(right);
    sha(&data)
}
fn empty_hashes() -> [[u8; 32]; 13] {
    let mut values = [[0; 32]; 13];
    values[0] = sha(b"bic-fulltext-stat-empty-v3");
    for i in 1..values.len() {
        values[i] = parent(values[i - 1], values[i - 1]);
    }
    values
}
fn height(node: usize) -> usize {
    12 - (usize::BITS - 1 - node.leading_zeros()) as usize
}
fn leaf(value: &Json) -> Result<[u8; 32], Error> {
    let mut bytes = b"bic-fulltext-stat-leaf-v3".to_vec();
    bytes.extend(serde_json::to_vec(value).map_err(|e| fail(e.to_string()))?);
    Ok(sha(&bytes))
}
fn values(stats: &BTreeMap<StatisticsKey, Statistics>) -> BTreeMap<usize, Json> {
    let mut groups = BTreeMap::<usize, Vec<Json>>::new();
    for ((domain, field, channel), s) in stats {
        for (term, df) in &s.df {
            let key = (domain.clone(), *field, *channel, term.clone());
            groups.entry(bucket(&key)).or_default().push(json!([
                domain,
                field,
                channel.number(),
                s.documents,
                s.length,
                term,
                df
            ]));
        }
    }
    groups
        .into_iter()
        .map(|(b, v)| (b, Json::Array(v)))
        .collect()
}
fn tree(values: &BTreeMap<usize, Json>) -> Result<BTreeMap<usize, [u8; 32]>, Error> {
    let defaults = empty_hashes();
    let mut nodes = BTreeMap::new();
    for (b, value) in values {
        nodes.insert(BUCKETS + b, leaf(value)?);
    }
    for n in (1..BUCKETS).rev() {
        let default = defaults[height(n) - 1];
        let h = parent(
            *nodes.get(&(n * 2)).unwrap_or(&default),
            *nodes.get(&(n * 2 + 1)).unwrap_or(&default),
        );
        if h != defaults[height(n)] {
            nodes.insert(n, h);
        }
    }
    Ok(nodes)
}
fn hash_pages(nodes: &BTreeMap<usize, [u8; 32]>) -> BTreeMap<usize, Json> {
    let mut pages = BTreeMap::<usize, Vec<Json>>::new();
    for (n, h) in nodes {
        pages
            .entry(n / HASH_PAGE + 1)
            .or_default()
            .push(json!([n, hex(h)]));
    }
    pages
        .into_iter()
        .map(|(p, v)| (p, Json::Array(v)))
        .collect()
}
fn entry(
    value: &Json,
    definition: &Definition,
    documents: usize,
    limits: &TextLimits,
) -> Result<(PostingKey, usize, usize, usize), Error> {
    let a = array(value)?;
    if a.len() != 7 {
        return Err(fail("invalid keyed statistics entry"));
    }
    let domain = a[0]
        .as_str()
        .filter(|d| !d.trim().is_empty() && d.len() <= 128)
        .ok_or_else(|| fail("invalid keyed statistics domain"))?
        .to_owned();
    let field = unsigned(&a[1])?;
    let channel = Channel::decode(
        a[2].as_u64()
            .ok_or_else(|| fail("invalid keyed statistics channel"))?,
    )?;
    let n = unsigned(&a[3])?;
    let length = unsigned(&a[4])?;
    let term = a[5]
        .as_str()
        .filter(|t| !t.is_empty() && t.len() <= TERM_BYTES && t.to_lowercase() == *t)
        .ok_or_else(|| fail("invalid keyed statistics term"))?
        .to_owned();
    let df = unsigned(&a[6])?;
    if field >= definition.fields.len()
        || n == 0
        || n > documents
        || length < n
        || length > limits.max_tokens
        || df == 0
        || df > n
    {
        return Err(fail("invalid keyed statistics counts"));
    }
    Ok(((domain, field, channel, term), n, length, df))
}
fn decode_bucket(
    value: &Json,
    b: usize,
    definition: &Definition,
    documents: usize,
    limits: &TextLimits,
    stats: &mut BTreeMap<StatisticsKey, Statistics>,
    count: &mut usize,
) -> Result<(), Error> {
    let entries = array(value)?;
    if entries.is_empty() {
        return Err(fail("empty keyed statistics bucket"));
    }
    let mut previous = None;
    for v in entries {
        let (key, n, length, df) = entry(v, definition, documents, limits)?;
        if bucket(&key) != b || previous.as_ref().is_some_and(|old| old >= &key) {
            return Err(fail("misrouted/out-of-order keyed statistics"));
        }
        previous = Some(key.clone());
        let s = stats
            .entry((key.0, key.1, key.2))
            .or_insert_with(|| Statistics {
                documents: n,
                length,
                df: BTreeMap::new(),
            });
        if s.documents != n || s.length != length || s.df.insert(key.3, df).is_some() {
            return Err(fail("inconsistent/duplicate keyed statistics"));
        }
        *count = count.saturating_add(1);
        if *count > limits.max_tokens {
            return Err(fail("keyed statistics token budget exceeded"));
        }
    }
    Ok(())
}
fn validate_totals(
    stats: &BTreeMap<StatisticsKey, Statistics>,
    limits: &TextLimits,
    complete: bool,
) -> Result<(), Error> {
    let mut tokens = 0usize;
    for s in stats.values() {
        tokens = tokens.saturating_add(s.length);
        let df =
            s.df.values()
                .try_fold(0usize, |n, v| n.checked_add(*v))
                .ok_or_else(|| fail("statistics count overflow"))?;
        if tokens > limits.max_tokens || df > s.length || complete && df < s.documents {
            return Err(fail("inconsistent keyed corpus statistics"));
        }
    }
    Ok(())
}
impl Generation {
    pub(super) fn keyed_statistics_rows(
        &self,
        rows: &mut BTreeMap<u64, Vec<u8>>,
        bytes: &mut usize,
        limits: &TextLimits,
    ) -> Result<(usize, [u8; 32]), Error> {
        let values = values(&self.statistics);
        let nodes = tree(&values)?;
        for (b, v) in &values {
            put_record(rows, STAT_KIND | ((*b as u64 + 1) << 10), v, bytes, limits)?;
        }
        for (p, v) in hash_pages(&nodes) {
            put_record(rows, HASH_KIND | ((p as u64) << 10), &v, bytes, limits)?;
        }
        Ok((values.len(), *nodes.get(&1).unwrap_or(&empty_hashes()[12])))
    }
    pub(super) fn decode_keyed_statistics(
        &self,
        rows: &BTreeMap<u64, Vec<u8>>,
        manifest: &GenerationManifest,
        limits: &TextLimits,
    ) -> Result<BTreeMap<StatisticsKey, Statistics>, Error> {
        let mut stats = BTreeMap::new();
        let mut buckets = BTreeSet::new();
        for key in rows.keys().filter(|k| **k >> 60 == 2) {
            let b = (key & ((1 << 60) - 1)) >> 10;
            if b == 0 || b > BUCKETS as u64 {
                return Err(fail("invalid keyed statistics ordinal"));
            }
            buckets.insert(b as usize - 1);
        }
        if buckets.len() != manifest.statistics_pages.unwrap_or(0) {
            return Err(fail("keyed statistics bucket count mismatch"));
        }
        let mut count = 0;
        for b in buckets {
            let v = get_record(rows, STAT_KIND | ((b as u64 + 1) << 10))?;
            decode_bucket(
                &v,
                b,
                &self.definition,
                manifest.documents,
                limits,
                &mut stats,
                &mut count,
            )?;
        }
        validate_totals(&stats, limits, true)?;
        let mut check = Self::metadata(manifest);
        check.statistics = stats.clone();
        let mut expected = BTreeMap::new();
        let (_, root) = check.keyed_statistics_rows(&mut expected, &mut 0, limits)?;
        let header = get_record(rows, 0)?;
        if root
            != hash_decode(
                header["statistics_sha256"]
                    .as_str()
                    .ok_or_else(|| fail("missing keyed statistics root"))?,
            )?
        {
            return Err(fail("keyed statistics root mismatch"));
        }
        let actual = rows
            .iter()
            .filter(|(k, _)| **k >> 60 == 2 || **k >> 60 == 3)
            .map(|(k, v)| (*k, v.clone()))
            .collect::<BTreeMap<_, _>>();
        if expected != actual {
            return Err(fail("keyed statistics proof records differ"));
        }
        Ok(stats)
    }
    /// V3 only. V1/V2 retain their existing complete/statistics-page reader.
    pub fn statistics_selection(
        &self,
        query: &str,
        options: &SearchOptions,
        limits: &TextLimits,
    ) -> Result<Option<StatisticsSelection>, Error> {
        if self.native_format < 3 {
            return Ok(None);
        }
        let fields = self.fields(options)?;
        let terms = query_tokens(query, options.channel, limits)?;
        let mut keys = BTreeSet::new();
        for f in &fields {
            for t in &terms {
                keys.insert((options.domain.clone(), *f, options.channel, t.clone()));
            }
        }
        let buckets = keys.iter().map(bucket).collect::<BTreeSet<_>>();
        let mut pages = BTreeSet::new();
        for b in &buckets {
            let mut n = BUCKETS + b;
            while n > 1 {
                pages.insert((n ^ 1) / HASH_PAGE + 1);
                n /= 2;
            }
        }
        Ok(Some(StatisticsSelection {
            definition: self.definition.clone(),
            keys,
            buckets,
            pages,
            scope: (options.domain.clone(), fields, options.channel, terms),
        }))
    }
    /// Query-only image whose selected statistics are proved against the header
    /// root. Hash collisions retain complete logical keys inside each bucket.
    pub fn from_query_candidate_rows(
        rows: &BTreeMap<u64, Vec<u8>>,
        candidates: &BTreeSet<u64>,
        selection: &StatisticsSelection,
        limits: &TextLimits,
    ) -> Result<Self, Error> {
        let manifest = Self::manifest_from_native_rows(rows, limits)?;
        if manifest.storage_format < 3 || manifest.definition != selection.definition {
            return Err(fail("query statistics require v3"));
        }
        let expected = hash_decode(
            get_record(rows, 0)?["statistics_sha256"]
                .as_str()
                .ok_or_else(|| fail("missing keyed statistics root"))?,
        )?;
        let mut bytes = 0usize;
        for (k, v) in rows {
            bytes = bytes.saturating_add(v.len());
            let ordinal = ((k & ((1 << 60) - 1)) >> 10) as usize;
            let valid = *k <= CHUNKS
                || match k >> 60 {
                    1 => candidates.contains(&(ordinal as u64)),
                    2 => ordinal > 0 && selection.buckets.contains(&(ordinal - 1)),
                    3 => selection.pages.contains(&ordinal),
                    _ => false,
                };
            if !valid || bytes > limits.max_bytes {
                return Err(fail("unrequested/oversized query statistics records"));
            }
        }
        let mut hashes = BTreeMap::new();
        for p in &selection.pages {
            let base = HASH_KIND | ((*p as u64) << 10);
            if rows.range(base..=base + CHUNKS).next().is_none() {
                continue;
            }
            let page = get_record(rows, base)?;
            let values = array(&page)?;
            if values.is_empty() {
                return Err(fail("empty statistics proof page"));
            }
            let mut previous = 0;
            for v in values {
                let a = array(v)?;
                if a.len() != 2 {
                    return Err(fail("invalid statistics proof node"));
                }
                let n = unsigned(&a[0])?;
                if n == 0 || n >= BUCKETS * 2 || n / HASH_PAGE + 1 != *p || n <= previous {
                    return Err(fail("invalid statistics proof position"));
                }
                previous = n;
                let h = hash_decode(
                    a[1].as_str()
                        .ok_or_else(|| fail("invalid statistics proof hash"))?,
                )?;
                hashes.insert(n, h);
            }
        }
        let mut stats = BTreeMap::new();
        let mut count = 0;
        let defaults = empty_hashes();
        for b in &selection.buckets {
            let base = STAT_KIND | ((*b as u64 + 1) << 10);
            let mut h = if rows.range(base..=base + CHUNKS).next().is_none() {
                defaults[0]
            } else {
                let value = get_record(rows, base)?;
                decode_bucket(
                    &value,
                    *b,
                    &manifest.definition,
                    manifest.documents,
                    limits,
                    &mut stats,
                    &mut count,
                )?;
                leaf(&value)?
            };
            let mut n = BUCKETS + b;
            while n > 1 {
                let sibling = *hashes.get(&(n ^ 1)).unwrap_or(&defaults[height(n)]);
                h = if n % 2 == 0 {
                    parent(h, sibling)
                } else {
                    parent(sibling, h)
                };
                n /= 2;
            }
            if h != expected {
                return Err(fail("query statistics Merkle proof mismatch"));
            }
        }
        validate_totals(&stats, limits, false)?;
        // Retain only query terms, while corpus N and length stay global.
        for ((domain, field, channel), s) in &mut stats {
            s.df.retain(|term, _| {
                selection
                    .keys
                    .contains(&(domain.clone(), *field, *channel, term.clone()))
            });
        }
        stats.retain(|_, s| !s.df.is_empty());
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
        let mut image = Self::from_documents(
            manifest.definition,
            manifest.generation,
            manifest.covered_seq,
            documents,
            limits,
        )?;
        for (key, local) in &image.statistics {
            let relevant = local
                .df
                .iter()
                .filter(|(t, _)| {
                    selection
                        .keys
                        .contains(&(key.0.clone(), key.1, key.2, (*t).clone()))
                })
                .collect::<Vec<_>>();
            if relevant.is_empty() {
                continue;
            }
            let global = stats
                .get(key)
                .ok_or_else(|| fail("missing query corpus statistics"))?;
            if local.documents > global.documents
                || local.length > global.length
                || relevant
                    .iter()
                    .any(|(t, df)| global.df.get(*t).map_or(true, |n| *df > n))
            {
                return Err(fail("partial query statistics exceed corpus"));
            }
        }
        image.statistics = stats;
        image.native_format = manifest.storage_format;
        image.complete = false;
        image.query_scope = Some(selection.scope.clone());
        Ok(image)
    }
}
