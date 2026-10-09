//! Deterministic, domain-scoped fallback retrieval. This is a bounded scan,
//! not a Lucene/BM25 index; rank fusion follows the reference daemon's field
//! round-robin policy and never compares unrelated field scores.
use crate::model::fail;
use crate::{Error, Graph, Limits, Value};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) fn search(
    graph: &Graph,
    query: &str,
    options: &BTreeMap<String, Value>,
    limits: &Limits,
) -> Result<Vec<u64>, Error> {
    if options
        .keys()
        .any(|k| !["db_type", "labels", "fields", "limit"].contains(&k.as_str()))
    {
        return Err(fail("unknown searchNodes option"));
    }
    let Some(Value::String(domain)) = options.get("db_type") else {
        return Err(fail("searchNodes requires explicit string db_type"));
    };
    if domain.trim().is_empty() {
        return Err(fail("empty searchNodes db_type"));
    }
    if query.len() > 4096 {
        return Err(fail("search query exceeds 4096 bytes"));
    }
    let terms = query
        .split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>();
    if terms.is_empty() || terms.len() > 16 {
        return Err(fail("search requires 1..16 terms"));
    }
    let strings = |value: Option<&Value>, default: Vec<String>| match value {
        None => Ok(default),
        Some(Value::List(v)) => v
            .iter()
            .map(|v| {
                if let Value::String(s) = v {
                    Ok(s.clone())
                } else {
                    Err(fail("search labels/fields require strings"))
                }
            })
            .collect(),
        _ => Err(fail("search labels/fields require lists")),
    };
    let labels = strings(options.get("labels"), vec![])?;
    let fields = strings(
        options.get("fields"),
        vec![
            "name".into(),
            "title".into(),
            "summary".into(),
            "search_text".into(),
        ],
    )?;
    if fields.is_empty() || fields.len() > 16 || labels.len() > 16 {
        return Err(fail("search field/label budget exceeded"));
    }
    let limit = options.get("limit").map_or(Ok(20), Value::as_usize)?;
    if limit == 0 || limit > 1000 {
        return Err(fail("search limit must be 1..1000"));
    }
    let mut candidates = vec![vec![]; fields.len()];
    let mut examined = 0usize;
    // Scope and labels are applied before field ranking and before LIMIT.
    let eligible: BTreeSet<u64> = if labels.is_empty() {
        graph.nodes.keys().copied().collect()
    } else {
        labels
            .iter()
            .flat_map(|label| graph.candidates(std::slice::from_ref(label)))
            .collect()
    };
    let whole = query.trim().to_lowercase();
    for id in eligible {
        examined += 1;
        if examined > limits.max_expansions {
            return Err(fail("search scan budget exceeded"));
        }
        let node = &graph.nodes[&id];
        if node.properties.get("db_type").and_then(|v| v.as_str()) != Some(domain) {
            continue;
        }
        for (index, field) in fields.iter().enumerate() {
            let texts = match node.properties.get(field) {
                Some(serde_json::Value::String(text)) => vec![text.as_str()],
                Some(serde_json::Value::Array(values)) => {
                    values.iter().filter_map(|v| v.as_str()).collect()
                }
                _ => vec![],
            };
            let rank = texts
                .into_iter()
                .map(str::to_lowercase)
                .filter(|text| terms.iter().all(|term| text.contains(term)))
                .map(|text| {
                    if text == whole {
                        2
                    } else if text.starts_with(&whole) {
                        1
                    } else {
                        0
                    }
                })
                .max();
            if let Some(rank) = rank {
                candidates[index].push((rank, id));
            }
        }
    }
    for field in &mut candidates {
        field.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    }
    let mut seen = BTreeSet::new();
    let mut output = vec![];
    let mut offset = 0;
    while candidates.iter().any(|field| offset < field.len()) {
        for field in &candidates {
            if let Some((_, id)) = field.get(offset) {
                if seen.insert(*id) {
                    output.push(*id);
                    if output.len() == limit {
                        return Ok(output);
                    }
                }
            }
        }
        offset += 1;
    }
    Ok(output)
}

/// Explicit indexed retrieval keeps each native ranking independent. Unspecified
/// indexes retain the legacy substring scan; invalid/unsupported indexes never
/// silently fall back to a scan with different matching semantics.
pub(crate) struct IndexedSearch {
    pub requests: Vec<crate::property_index::FulltextRequest>,
    pub domain: String,
    pub labels: Vec<String>,
    pub limit: usize,
    pub eventual: bool,
    pub automatic: bool,
}
pub(crate) fn indexed(
    query: &str,
    options: &BTreeMap<String, Value>,
) -> Result<Option<IndexedSearch>, Error> {
    let Some(indexes) = options.get("indexes") else {
        return Ok(None);
    };
    if options.keys().any(|k| {
        ![
            "db_type",
            "labels",
            "fields",
            "limit",
            "indexes",
            "consistency",
            "channel",
        ]
        .contains(&k.as_str())
    }) {
        return Err(fail("unknown indexed searchNodes option"));
    }
    let Some(Value::String(domain)) = options.get("db_type") else {
        return Err(fail("searchNodes requires explicit string db_type"));
    };
    if domain.trim().is_empty() {
        return Err(fail("empty searchNodes db_type"));
    }
    if query.len() > 4096 || query.trim().is_empty() {
        return Err(fail("invalid indexed search query"));
    }
    let strings = |v: &Value, max_bytes: usize| -> Result<Vec<String>, Error> {
        let Value::List(values) = v else {
            return Err(fail("indexed search options require string lists"));
        };
        if values.len() > 16 {
            return Err(fail("indexed search option list exceeds 16 entries"));
        }
        let mut seen = BTreeSet::new();
        values
            .iter()
            .map(|v| match v {
                Value::String(s)
                    if !s.trim().is_empty() && s.len() <= max_bytes && seen.insert(s.clone()) =>
                {
                    Ok(s.clone())
                }
                _ => Err(fail(
                    "indexed search lists require distinct nonempty strings",
                )),
            })
            .collect()
    };
    let automatic = matches!(indexes, Value::String(s) if s == "auto");
    let indexes = if automatic {
        vec![]
    } else {
        strings(indexes, 128)?
    };
    if !automatic && indexes.is_empty() {
        return Err(fail("indexed search needs 1..16 indexes"));
    }
    let labels = options
        .get("labels")
        .map(|labels| strings(labels, 128))
        .transpose()?
        .unwrap_or_default();
    if let Some(fields) = options.get("fields") {
        strings(fields, 4096)?;
    }
    let limit = options.get("limit").map_or(Ok(20), Value::as_usize)?;
    if limit == 0 || limit > 1000 {
        return Err(fail("search limit must be 1..1000"));
    }
    let eventual = match options.get("consistency") {
        None => false,
        Some(Value::String(s)) if s == "strict" => false,
        Some(Value::String(s)) if s == "eventual" => true,
        _ => return Err(fail("consistency must be strict or eventual")),
    };
    if let Some(channel) = options.get("channel") {
        if !matches!(channel, Value::String(s) if ["terms", "phrase", "exact"].contains(&s.as_str()))
        {
            return Err(fail("unknown indexed search channel"));
        }
    }
    let mut native = options.clone();
    native.remove("indexes");
    native.insert("limit".into(), Value::integer(limit as u64));
    let requests = if automatic {
        let fields = match options.get("fields") {
            Some(fields) => strings(fields, 4096)?,
            None => vec![
                "name".into(),
                "title".into(),
                "summary".into(),
                "search_text".into(),
            ],
        };
        if fields.is_empty() {
            return Err(fail("automatic search needs 1..16 explicit fields"));
        }
        fields
            .into_iter()
            .map(|field| {
                let mut options = native.clone();
                options.insert("fields".into(), Value::List(vec![Value::String(field)]));
                crate::property_index::FulltextRequest {
                    entity: crate::property_index::EntityKind::Node,
                    index: String::new(),
                    query: query.into(),
                    options,
                }
            })
            .collect()
    } else {
        indexes
            .into_iter()
            .map(|index| crate::property_index::FulltextRequest {
                entity: crate::property_index::EntityKind::Node,
                index,
                query: query.into(),
                options: native.clone(),
            })
            .collect()
    };
    Ok(Some(IndexedSearch {
        requests,
        automatic,
        domain: domain.clone(),
        labels,
        limit,
        eventual,
    }))
}
/// Fuse ordinal ranks rather than comparing unrelated BM25 scores. Taking at
/// most the global limit from each already-filtered stream is sufficient: a
/// stream's later distinct item already has at least limit distinct predecessors.
pub(crate) fn round_robin(streams: &[Vec<u64>], limit: usize) -> Vec<u64> {
    let mut seen = BTreeSet::new();
    let mut result = vec![];
    for offset in 0..streams.iter().map(Vec::len).max().unwrap_or(0) {
        for stream in streams {
            if let Some(id) = stream.get(offset) {
                if seen.insert(*id) {
                    result.push(*id);
                    if result.len() == limit {
                        return result;
                    }
                }
            }
        }
    }
    result
}

/// An OR-label index is complete for the requested OR scope only if it is
/// unrestricted or contains every requested label. Without a scope, only an
/// unrestricted index can cover future/unlabelled nodes. Prefer fewer indexed
/// fields, then bytewise name, so selection is stable across dictionary order.
pub(crate) fn resolve_automatic(
    search: &mut IndexedSearch,
    indexes: Vec<crate::property_index::FulltextIndex>,
) -> Result<(), Error> {
    let mut names = BTreeSet::new();
    for index in &indexes {
        if index.name.is_empty() || !names.insert(&index.name) {
            return Err(fail("invalid/duplicate automatic full-text index metadata"));
        }
        index.definition.validate()?;
    }
    for request in &mut search.requests {
        let Value::List(fields) = &request.options["fields"] else {
            unreachable!()
        };
        let Value::String(field) = &fields[0] else {
            unreachable!()
        };
        let chosen = indexes
            .iter()
            .filter(|index| {
                index.definition.entity == crate::property_index::EntityKind::Node
                    && (index.definition.labels.is_empty()
                        || !search.labels.is_empty()
                            && search
                                .labels
                                .iter()
                                .all(|label| index.definition.labels.contains(label)))
                    && index
                        .definition
                        .fields
                        .iter()
                        .any(|path| crate::fulltext::Definition::field_name(path) == *field)
            })
            .min_by(|a, b| {
                a.definition
                    .fields
                    .len()
                    .cmp(&b.definition.fields.len())
                    .then(a.name.cmp(&b.name))
            });
        request.index = chosen
            .ok_or_else(|| {
                fail(format!(
                    "no complete node full-text index covers field {field} and requested labels"
                ))
            })?
            .name
            .clone();
    }
    Ok(())
}

#[cfg(test)]
mod automatic_tests {
    use super::*;
    use crate::fulltext::{Definition, PathPart};
    use crate::property_index::{EntityKind, FulltextIndex};
    fn index(name: &str, fields: &[&str], labels: &[&str]) -> FulltextIndex {
        FulltextIndex {
            name: name.into(),
            definition: Definition {
                entity: EntityKind::Node,
                labels: labels.iter().map(|s| s.to_string()).collect(),
                fields: fields
                    .iter()
                    .map(|s| vec![PathPart::Key(s.to_string())])
                    .collect(),
            },
        }
    }
    #[test]
    fn automatic_plan_is_stable_complete_and_field_ordered() {
        let options = BTreeMap::from([
            ("db_type".into(), Value::String("d1".into())),
            ("indexes".into(), Value::String("auto".into())),
            (
                "labels".into(),
                Value::List(vec![Value::String("A".into()), Value::String("B".into())]),
            ),
            (
                "fields".into(),
                Value::List(vec![
                    Value::String("summary".into()),
                    Value::String("name".into()),
                ]),
            ),
        ]);
        let mut indexes = vec![
            index("zname", &["name"], &[]),
            index("wide", &["name", "summary"], &[]),
            index("scoped", &["summary"], &["A"]),
            index("aname", &["name"], &[]),
        ];
        for _ in 0..2 {
            let mut search = indexed("needle", &options).unwrap().unwrap();
            resolve_automatic(&mut search, indexes.clone()).unwrap();
            assert_eq!(
                search
                    .requests
                    .iter()
                    .map(|r| r.index.as_str())
                    .collect::<Vec<_>>(),
                ["wide", "aname"]
            );
            assert_eq!(
                search.requests[0].options["fields"],
                Value::List(vec![Value::String("summary".into())])
            );
            indexes.reverse();
        }
        let mut search = indexed("needle", &options).unwrap().unwrap();
        assert!(resolve_automatic(
            &mut search,
            vec![index("scoped", &["name", "summary"], &["A"])]
        )
        .is_err());
        assert!(resolve_automatic(
            &mut search,
            vec![
                index("duplicate", &["name"], &[]),
                index("duplicate", &["summary"], &[])
            ]
        )
        .is_err());
        let mut edge = index("edge", &["name", "summary"], &[]);
        edge.definition.entity = EntityKind::Relationship;
        assert!(resolve_automatic(&mut search, vec![edge]).is_err());
    }
}
