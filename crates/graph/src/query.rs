use crate::access::GraphAccess;
use crate::model::{decimal, fail, json_map, validate_properties};
use crate::parser::{Clause, EdgePattern, Expr, NodePattern, Pattern, Projection, Query};
use crate::property_index::{
    EntityKind, FulltextHit, FulltextIndex, FulltextRequest, IndexProvider, IndexRequest,
    PropertyPredicate, SeekOp,
};
use crate::{
    Deadline, Direction, Error, Graph, GraphChanges, GraphCorpus, Limits, Path, Properties, Value,
};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

type Row = BTreeMap<String, Value>;
#[derive(Debug, Clone, PartialEq)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    pub mutations: usize,
    pub expansions: usize,
    pub edge_expansions: usize,
    /// Net changes from a successful write; empty for reads and reverted edits.
    pub changes: GraphChanges,
}
struct Runner<'a, 'p> {
    graph: &'a mut Graph,
    params: &'a Row,
    limits: &'a Limits,
    deadline: Deadline,
    expansions: usize,
    edge_expansions: usize,
    mutations: usize,
    detach_edges: usize,
    source: Source<'p>,
    absent_nodes: BTreeSet<u64>,
    absent_edges: BTreeSet<u64>,
    corpus: Option<GraphCorpus>,
}
enum Source<'a> {
    Indexes(&'a mut dyn IndexProvider),
    Storage(&'a mut dyn GraphAccess),
}
impl Source<'_> {
    fn candidates(&mut self, request: &IndexRequest) -> Result<Option<Vec<u64>>, Error> {
        match self {
            Self::Indexes(p) => p.candidates(request),
            Self::Storage(p) => p.candidates(request),
        }
    }
    fn fulltext_indexes(&mut self) -> Result<Vec<FulltextIndex>, Error> {
        match self {
            Self::Indexes(p) => p.fulltext_indexes(),
            Self::Storage(p) => p.fulltext_indexes(),
        }
    }
    fn fulltext(
        &mut self,
        request: &FulltextRequest,
        staged: Option<&Graph>,
    ) -> Result<Vec<FulltextHit>, Error> {
        match self {
            Self::Indexes(p) => p.fulltext(request, staged),
            Self::Storage(p) => p.fulltext(request, staged),
        }
    }
}

/// A failed query never changes the caller's graph. Budget exhaustion is an error,
/// rather than a truncated result that could be mistaken for a complete answer.
pub fn execute(
    graph: &mut Graph,
    query: &Query,
    parameters: &Row,
    limits: &Limits,
) -> Result<QueryResult, Error> {
    struct ScanOnly;
    impl IndexProvider for ScanOnly {
        fn candidates(&mut self, _: &IndexRequest) -> Result<Option<Vec<u64>>, Error> {
            Ok(None)
        }
    }
    execute_with_indexes(graph, query, parameters, limits, &mut ScanOnly)
}

/// Execute with native candidate access. Every candidate is rechecked; once a
/// mutation occurs, remaining clauses scan the staged graph so new/changed
/// entities cannot be missed by the pre-statement persistent trees.
pub fn execute_with_indexes(
    graph: &mut Graph,
    query: &Query,
    parameters: &Row,
    limits: &Limits,
    indexes: &mut dyn IndexProvider,
) -> Result<QueryResult, Error> {
    execute_with_indexes_deadline(
        graph,
        query,
        parameters,
        limits,
        indexes,
        Deadline::for_limits(limits),
    )
}

/// Preserve an outer statement clock across native loading and nested providers.
pub fn execute_with_indexes_deadline(
    graph: &mut Graph,
    query: &Query,
    parameters: &Row,
    limits: &Limits,
    indexes: &mut dyn IndexProvider,
    deadline: Deadline,
) -> Result<QueryResult, Error> {
    execute_source(
        graph,
        query,
        parameters,
        limits,
        Source::Indexes(indexes),
        deadline.tighten(limits.max_elapsed_ms),
        None,
    )
}

/// Run a read-only query against a statement-scoped snapshot. The caller passes
/// a manifest-initialized empty graph cache; only fetched entities are retained.
/// This port cannot write a partial image back to storage.
pub fn execute_with_storage(
    graph: &mut Graph,
    query: &Query,
    parameters: &Row,
    limits: &Limits,
    storage: &mut dyn GraphAccess,
) -> Result<QueryResult, Error> {
    execute_with_storage_deadline(
        graph,
        query,
        parameters,
        limits,
        storage,
        Deadline::for_limits(limits),
    )
}

/// Preserve the enclosing SQL clock; storage reads cannot restart the timer.
pub fn execute_with_storage_deadline(
    graph: &mut Graph,
    query: &Query,
    parameters: &Row,
    limits: &Limits,
    storage: &mut dyn GraphAccess,
    deadline: Deadline,
) -> Result<QueryResult, Error> {
    if !query.is_read_only() {
        return Err(fail("storage access requires a read-only query"));
    }
    execute_source(
        graph,
        query,
        parameters,
        limits,
        Source::Storage(storage),
        deadline.tighten(limits.max_elapsed_ms),
        None,
    )
}
/// Execute a write on a manifest-initialized cache with complete source budgets.
/// Storage reads and first-write originals are merged with this statement's
/// overlay; the returned cache must only be persisted through a partial patch.
#[allow(clippy::too_many_arguments)]
pub fn execute_with_storage_write_deadline(
    graph: &mut Graph,
    query: &Query,
    parameters: &Row,
    limits: &Limits,
    storage: &mut dyn GraphAccess,
    corpus: GraphCorpus,
    deadline: Deadline,
) -> Result<QueryResult, Error> {
    corpus.validate(limits)?;
    if query.is_read_only() {
        return Err(fail("partial write access requires a write query"));
    }
    // This is a clone of only the caller's cache, never the full source.
    let initial = graph.clone();
    let result = execute_source(
        graph,
        query,
        parameters,
        limits,
        Source::Storage(storage),
        deadline.tighten(limits.max_elapsed_ms),
        Some(corpus),
    );
    if result.is_err() {
        *graph = initial;
    }
    result
}
#[allow(clippy::too_many_arguments)]
fn execute_source(
    graph: &mut Graph,
    query: &Query,
    parameters: &Row,
    limits: &Limits,
    source: Source<'_>,
    deadline: Deadline,
    corpus: Option<GraphCorpus>,
) -> Result<QueryResult, Error> {
    deadline.check("before staging")?;
    if query.is_read_only() {
        let lazy = matches!(source, Source::Storage(_));
        let mut cache = graph.clone();
        let result = execute_in_place(
            &mut cache, query, parameters, limits, source, deadline, corpus,
        )?;
        if lazy {
            *graph = cache;
        }
        return Ok(result);
    }
    // First-write originals restore both entities and allocator on every error.
    // Nested CALL/UNION share the Runner and this single statement journal.
    graph.begin_write()?;
    match execute_in_place(graph, query, parameters, limits, source, deadline, corpus) {
        Ok(mut result) => {
            result.changes = graph.finish_write();
            Ok(result)
        }
        Err(error) => {
            graph.rollback_write();
            Err(error)
        }
    }
}
#[allow(clippy::too_many_arguments)]
fn execute_in_place(
    graph: &mut Graph,
    query: &Query,
    parameters: &Row,
    limits: &Limits,
    source: Source<'_>,
    deadline: Deadline,
    corpus: Option<GraphCorpus>,
) -> Result<QueryResult, Error> {
    deadline.check("before validation")?;
    validate(query, parameters)?;
    let output_scope = check_scope(query, BTreeMap::new())?;
    deadline.check("after binding")?;
    let mut runner = Runner {
        graph,
        params: parameters,
        limits,
        deadline,
        expansions: 0,
        edge_expansions: 0,
        mutations: 0,
        detach_edges: 0,
        source,
        absent_nodes: BTreeSet::new(),
        absent_edges: BTreeSet::new(),
        corpus,
    };
    let (mut columns, rows) = runner.run(query, vec![Row::new()])?;
    if columns.is_empty() && matches!(query.clauses.last(), Some(Clause::Project(_, true))) {
        columns = output_scope.into_keys().collect();
    }
    if let Some(corpus) = runner.corpus {
        corpus.changed(runner.graph.staged_changes(), runner.graph, limits)?;
    } else if runner.graph.nodes.len() > limits.max_nodes
        || runner.graph.edges.len() > limits.max_edges
        || runner
            .graph
            .storage_size_deadline(limits.max_text_bytes, Some(deadline))?
            > limits.max_text_bytes
    {
        return Err(fail("graph storage budget exceeded"));
    }
    let result = QueryResult {
        columns: columns.clone(),
        rows: rows
            .iter()
            .map(|r| {
                columns
                    .iter()
                    .map(|c| r.get(c).cloned().unwrap_or(Value::Null))
                    .collect()
            })
            .collect(),
        mutations: runner.mutations,
        expansions: runner.expansions,
        edge_expansions: runner.edge_expansions,
        changes: GraphChanges::default(),
    };
    let mut result_bytes = 0usize;
    for row in &result.rows {
        for value in row {
            result_bytes = result_bytes.saturating_add(
                serde_json::to_vec(&value.to_json(graph))
                    .map_err(|e| fail(e.to_string()))?
                    .len(),
            );
            if result_bytes > limits.max_text_bytes {
                return Err(fail("graph result byte budget exceeded"));
            }
        }
    }
    deadline.check("before publishing staged graph")?;
    Ok(result)
}

fn children(e: &Expr) -> Vec<&Expr> {
    match e {
        Expr::Property(x, _) | Expr::Label(x, _) | Expr::Unary(_, x) => vec![x],
        Expr::Binary(_, a, b) | Expr::Index(a, b) => vec![a, b],
        Expr::Function(_, a, _) | Expr::List(a) => a.iter().collect(),
        Expr::Map(m) => m.iter().map(|(_, v)| v).collect(),
        Expr::Case(p, x) => p
            .iter()
            .flat_map(|(a, b)| [a, b])
            .chain(std::iter::once(x.as_ref()))
            .collect(),
        Expr::Slice(x, a, b) => std::iter::once(x.as_ref())
            .chain(a.iter().map(|e| e.as_ref()))
            .chain(b.iter().map(|e| e.as_ref()))
            .collect(),
        Expr::Comprehension(_, x, p, v) => std::iter::once(x.as_ref())
            .chain(p.iter().map(|e| e.as_ref()))
            .chain(std::iter::once(v.as_ref()))
            .collect(),
        Expr::Quantifier(_, _, a, b) => vec![a, b],
        Expr::Reduce(_, a, _, b, c) => vec![a, b, c],
        _ => vec![],
    }
}
fn aggregate(e: &Expr) -> bool {
    matches!(e, Expr::Function(n,_,_) if ["count","collect","sum","avg","min","max"].contains(&n.as_str()))
        || children(e).iter().any(|e| aggregate(e))
}
fn validate_expr(e: &Expr, params: &Row) -> Result<(), Error> {
    if let Expr::Parameter(n) = e {
        if !params.contains_key(n) {
            return Err(fail(format!("missing Cypher parameter ${n}")));
        }
    }
    if let Expr::Function(n, args, distinct) = e {
        let arity = match n.as_str() {
            "count" | "collect" | "sum" | "avg" | "min" | "max" | "id" | "elementid" | "labels"
            | "type" | "properties" | "startnode" | "endnode" | "start_id" | "end_id" | "keys"
            | "head" | "last" | "tail" | "size" | "length" | "nodes" | "relationships"
            | "tolower" | "toupper" | "trim" | "tostring" | "tointeger" | "exists" => Some(1),
            "coalesce" => None,
            _ => return Err(fail(format!("unsupported Cypher function {n}"))),
        };
        if arity.is_some_and(|count| args.len() != count) || (n == "coalesce" && args.is_empty()) {
            return Err(fail("invalid function argument count"));
        }
        if args.iter().any(|e| matches!(e, Expr::Star)) && (n != "count" || *distinct) {
            return Err(fail("star requires count(*) without DISTINCT"));
        }
        if *distinct && !["count", "collect", "sum", "avg", "min", "max"].contains(&n.as_str()) {
            return Err(fail("DISTINCT requires an aggregate"));
        }
        if ["count", "collect", "sum", "avg", "min", "max"].contains(&n.as_str())
            && args.iter().any(aggregate)
        {
            return Err(fail("nested aggregate unsupported"));
        }
    }
    if let Expr::Exists(q) = e {
        validate(q, params)?;
    }
    if let Expr::Map(items) = e {
        let mut seen = BTreeSet::new();
        for (key, _) in items {
            if !seen.insert(key) {
                return Err(fail("duplicate map key"));
            }
        }
    }
    for child in children(e) {
        validate_expr(child, params)?;
    }
    Ok(())
}
fn validate(q: &Query, params: &Row) -> Result<(), Error> {
    for (index, c) in q.clauses.iter().enumerate() {
        if matches!(c, Clause::Project(_, true)) && index + 1 != q.clauses.len() {
            return Err(fail("RETURN must end a query branch"));
        }
        let mut exprs = vec![];
        match c {
            Clause::Match(p, _, _) | Clause::Create(p) | Clause::Merge(p, _, _) => {
                for p in p {
                    for n in &p.nodes {
                        exprs.extend(n.properties.iter().map(|(_, e)| e));
                    }
                    for e in &p.edges {
                        exprs.extend(e.properties.iter().map(|(_, e)| e));
                    }
                }
                if let Clause::Match(_, _, Some(cond)) = c {
                    exprs.push(cond);
                }
                if let Clause::Merge(_, on_create, on_match) = c {
                    for (lhs, rhs) in on_create.iter().chain(on_match) {
                        exprs.extend([lhs, rhs]);
                    }
                }
            }
            Clause::Call(_, sub) => validate(sub, params)?,
            Clause::Search(args, yielded) => {
                if args.len() != 2 {
                    return Err(fail("searchNodes expects query and options"));
                }
                let mut seen = BTreeSet::new();
                for (column, alias) in yielded {
                    if !["node", "rank", "mode"].contains(&column.as_str()) || !seen.insert(alias) {
                        return Err(fail(
                            "searchNodes yields node,rank,mode without duplicate aliases",
                        ));
                    }
                }
                exprs.extend(args);
            }
            Clause::Fulltext(entity, args, yielded) => {
                if args.len() != 3 {
                    return Err(fail(
                        "full-text procedure expects index,query,options with explicit db_type",
                    ));
                }
                let entity_column = if *entity == EntityKind::Node {
                    "node"
                } else {
                    "relationship"
                };
                let mut seen = BTreeSet::new();
                for (column, alias) in yielded {
                    if column != entity_column
                        && ![
                            "rank",
                            "score",
                            "field",
                            "snippet",
                            "byte_offset",
                            "line_number",
                            "mode",
                            "generation",
                            "covered_commit_seq",
                            "complete",
                            "covered_source_seq",
                            "source_seq",
                        ]
                        .contains(&column.as_str())
                    {
                        return Err(fail("unknown full-text YIELD column or wrong entity kind"));
                    }
                    if !seen.insert(alias) {
                        return Err(fail("duplicate full-text YIELD alias"));
                    }
                }
                exprs.extend(args);
            }
            Clause::Unwind(e, _) | Clause::Filter(e) => exprs.push(e),
            Clause::Project(p, _) => {
                exprs.extend(p.items.iter().map(|(e, _)| e));
                exprs.extend(p.order.iter().map(|(e, _)| e));
                exprs.extend(p.skip.iter());
                exprs.extend(p.limit.iter());
                exprs.extend(p.filter.iter());
            }
            Clause::Set(items) => {
                for (a, b) in items {
                    exprs.extend([a, b]);
                }
            }
            Clause::Remove(e) | Clause::Delete(e, _) => exprs.extend(e),
        }
        match c {
            Clause::Create(patterns) | Clause::Merge(patterns, _, _) => {
                for p in patterns {
                    if p.shortest
                        || p.edges.iter().any(|e| {
                            e.variable_length
                                || e.types.len() != 1
                                || (e.direction == Direction::Both
                                    && matches!(c, Clause::Create(_)))
                        })
                    {
                        return Err(fail(
                            "CREATE/MERGE requires fixed typed relationships; CREATE also requires a direction",
                        ));
                    }
                }
                if let Clause::Merge(patterns, on_create, on_match) = c {
                    for pattern in patterns {
                        for properties in pattern
                            .nodes
                            .iter()
                            .map(|n| &n.properties)
                            .chain(pattern.edges.iter().map(|e| &e.properties))
                        {
                            let mut keys = BTreeSet::new();
                            for (key, expr) in properties {
                                if !keys.insert(key) {
                                    return Err(fail("duplicate MERGE property key"));
                                }
                                if aggregate(expr) {
                                    return Err(fail("MERGE properties cannot contain aggregates"));
                                }
                                if matches!(expr, Expr::Literal(Value::Null)) {
                                    return Err(fail(format!(
                                        "MERGE property {key} cannot be null"
                                    )));
                                }
                            }
                        }
                    }
                    validate_set_targets(on_create)?;
                    validate_set_targets(on_match)?;
                }
            }
            Clause::Set(items) => {
                for (lhs, _) in items {
                    if !matches!(lhs, Expr::Property(..) | Expr::Label(..)) {
                        return Err(fail("unsupported SET target"));
                    }
                }
            }
            Clause::Remove(items)
                if items
                    .iter()
                    .any(|e| !matches!(e, Expr::Property(..) | Expr::Label(..))) =>
            {
                return Err(fail("unsupported REMOVE target"));
            }
            _ => {}
        }
        for e in exprs {
            validate_expr(e, params)?;
        }
    }
    if let Some((_, right)) = &q.union {
        validate(right, params)?;
    }
    Ok(())
}
fn validate_set_targets(items: &[(Expr, Expr)]) -> Result<(), Error> {
    for (lhs, rhs) in items {
        if !matches!(lhs, Expr::Property(..) | Expr::Label(..)) {
            return Err(fail("unsupported SET target"));
        }
        if aggregate(lhs) || aggregate(rhs) {
            return Err(fail("MERGE actions cannot contain aggregates"));
        }
    }
    Ok(())
}
fn compare(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Greater,
        (_, Value::Null) => Ordering::Less,
        (Value::Number(a), Value::Number(b)) => a.cmp(b),
        (Value::String(a), Value::String(b)) => a.cmp(b),
        (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
        (Value::Node(a), Value::Node(b)) | (Value::Edge(a), Value::Edge(b)) => a.cmp(b),
        (Value::List(a), Value::List(b)) => {
            for (a, b) in a.iter().zip(b) {
                let order = compare(a, b);
                if order != Ordering::Equal {
                    return order;
                }
            }
            a.len().cmp(&b.len())
        }
        _ => format!("{a:?}").cmp(&format!("{b:?}")),
    }
}
fn equals(a: &Value, b: &Value) -> Option<bool> {
    match (a, b) {
        (Value::Null, _) | (_, Value::Null) => None,
        (Value::List(a), Value::List(b)) => {
            if a.len() != b.len() {
                return Some(false);
            }
            let mut unknown = false;
            for (a, b) in a.iter().zip(b) {
                match equals(a, b) {
                    Some(false) => return Some(false),
                    None => unknown = true,
                    _ => {}
                }
            }
            if unknown {
                None
            } else {
                Some(true)
            }
        }
        (Value::Map(a), Value::Map(b)) => {
            if a.keys().ne(b.keys()) {
                return Some(false);
            }
            let mut unknown = false;
            for (key, a) in a {
                match equals(a, &b[key]) {
                    Some(false) => return Some(false),
                    None => unknown = true,
                    _ => {}
                }
            }
            if unknown {
                None
            } else {
                Some(true)
            }
        }
        _ => Some(a == b),
    }
}
fn key(values: &[Value]) -> String {
    format!("{values:?}")
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Binding {
    Node,
    Edge,
    Edges,
    Path,
    Any,
}
type Scope = BTreeMap<String, Binding>;
fn check_expression_scope(e: &Expr, scope: &Scope) -> Result<(), Error> {
    match e {
        Expr::Variable(n) => {
            if !scope.contains_key(n) {
                return Err(fail(format!("unbound variable {n}")));
            }
        }
        Expr::Comprehension(var, list, cond, value) => {
            check_expression_scope(list, scope)?;
            let mut local = scope.clone();
            local.insert(var.clone(), Binding::Any);
            if let Some(cond) = cond {
                check_expression_scope(cond, &local)?;
            }
            check_expression_scope(value, &local)?;
            return Ok(());
        }
        Expr::Quantifier(_, var, list, cond) => {
            check_expression_scope(list, scope)?;
            let mut local = scope.clone();
            local.insert(var.clone(), Binding::Any);
            check_expression_scope(cond, &local)?;
            return Ok(());
        }
        Expr::Reduce(acc, init, var, list, body) => {
            check_expression_scope(init, scope)?;
            check_expression_scope(list, scope)?;
            let mut local = scope.clone();
            local.insert(acc.clone(), Binding::Any);
            local.insert(var.clone(), Binding::Any);
            check_expression_scope(body, &local)?;
            return Ok(());
        }
        Expr::Exists(q) => {
            check_scope(q, scope.clone())?;
        }
        _ => {}
    }
    for e in children(e) {
        check_expression_scope(e, scope)?;
    }
    Ok(())
}
fn declare(scope: &mut Scope, name: &Option<String>, binding: Binding) -> Result<(), Error> {
    if let Some(name) = name {
        if let Some(old) = scope.get(name) {
            if *old != binding && *old != Binding::Any {
                return Err(fail(format!("incompatible pattern binding {name}")));
            }
        } else {
            scope.insert(name.clone(), binding);
        }
    }
    Ok(())
}
fn check_scope(query: &Query, initial: Scope) -> Result<Scope, Error> {
    let mut scope = initial.clone();
    for clause in &query.clauses {
        match clause {
            Clause::Match(patterns, _, _)
            | Clause::Create(patterns)
            | Clause::Merge(patterns, _, _) => {
                let merge_scope = matches!(clause, Clause::Merge(..)).then(|| scope.clone());
                for p in patterns {
                    declare(&mut scope, &p.variable, Binding::Path)?;
                    for n in &p.nodes {
                        declare(&mut scope, &n.variable, Binding::Node)?;
                    }
                    for e in &p.edges {
                        declare(
                            &mut scope,
                            &e.variable,
                            if e.variable_length {
                                Binding::Edges
                            } else {
                                Binding::Edge
                            },
                        )?;
                    }
                }
                for p in patterns {
                    for e in p
                        .nodes
                        .iter()
                        .flat_map(|n| &n.properties)
                        .chain(p.edges.iter().flat_map(|e| &e.properties))
                        .map(|(_, e)| e)
                    {
                        check_expression_scope(e, merge_scope.as_ref().unwrap_or(&scope))?;
                    }
                }
                if let Clause::Match(_, _, Some(filter)) = clause {
                    check_expression_scope(filter, &scope)?;
                }
                if let Clause::Merge(_, on_create, on_match) = clause {
                    for (lhs, rhs) in on_create.iter().chain(on_match) {
                        check_expression_scope(lhs, &scope)?;
                        check_expression_scope(rhs, &scope)?;
                        let (base, label) = match lhs {
                            Expr::Property(base, _) => (base, false),
                            Expr::Label(base, _) => (base, true),
                            _ => unreachable!("SET targets were validated"),
                        };
                        if let Expr::Variable(name) = &**base {
                            let binding = scope[name];
                            if binding != Binding::Any
                                && binding != Binding::Node
                                && (label || binding != Binding::Edge)
                            {
                                return Err(fail("MERGE SET target must be a node property/label or relationship property"));
                            }
                        }
                    }
                }
            }
            Clause::Filter(expr) => check_expression_scope(expr, &scope)?,
            Clause::Unwind(expr, name) => {
                check_expression_scope(expr, &scope)?;
                scope.insert(name.clone(), Binding::Any);
            }
            Clause::Search(args, yielded) | Clause::Fulltext(_, args, yielded) => {
                for e in args {
                    check_expression_scope(e, &scope)?;
                }
                for (column, name) in yielded {
                    if scope.contains_key(name) {
                        return Err(fail("YIELD cannot shadow a variable"));
                    }
                    scope.insert(
                        name.clone(),
                        if column == "node" {
                            Binding::Node
                        } else if column == "relationship" {
                            Binding::Edge
                        } else {
                            Binding::Any
                        },
                    );
                }
            }
            Clause::Call(imported, sub) => {
                let mut local = Scope::new();
                for n in imported {
                    local.insert(
                        n.clone(),
                        *scope
                            .get(n)
                            .ok_or_else(|| fail(format!("unbound CALL import {n}")))?,
                    );
                }
                if !matches!(sub.clauses.last(), Some(Clause::Project(_, true))) {
                    return Err(fail("CALL subquery requires RETURN"));
                }
                for (name, binding) in check_scope(sub, local)? {
                    if scope.contains_key(&name) {
                        return Err(fail("CALL cannot shadow outer variable"));
                    }
                    scope.insert(name, binding);
                }
            }
            Clause::Project(p, _) => {
                let mut next = Scope::new();
                let groups = p
                    .items
                    .iter()
                    .filter(|(e, _)| !aggregate(e))
                    .map(|(e, _)| e)
                    .collect::<Vec<_>>();
                for (e, name) in &p.items {
                    check_expression_scope(e, &scope)?;
                    if aggregate(e) {
                        check_group_expression(e, &groups, &BTreeSet::new())?;
                    }
                    if matches!(e, Expr::Star) {
                        next.extend(scope.clone());
                    } else {
                        if next.contains_key(name) {
                            return Err(fail("duplicate projection column"));
                        }
                        next.insert(
                            name.clone(),
                            if let Expr::Variable(v) = e {
                                scope[v]
                            } else {
                                Binding::Any
                            },
                        );
                    }
                }
                let mut ordering = scope.clone();
                ordering.extend(next.clone());
                for (e, _) in &p.order {
                    check_expression_scope(e, &ordering)?;
                }
                if let Some(filter) = &p.filter {
                    check_expression_scope(filter, &next)?;
                }
                for e in p.skip.iter().chain(p.limit.iter()) {
                    check_expression_scope(e, &Scope::new())?;
                }
                scope = next;
            }
            Clause::Set(items) => {
                for (lhs, rhs) in items {
                    check_expression_scope(lhs, &scope)?;
                    check_expression_scope(rhs, &scope)?;
                }
            }
            Clause::Remove(exprs) | Clause::Delete(exprs, _) => {
                for e in exprs {
                    check_expression_scope(e, &scope)?;
                }
            }
        }
    }
    if let Some((_, right)) = &query.union {
        if check_scope(right, initial)?.keys().collect::<Vec<_>>()
            != scope.keys().collect::<Vec<_>>()
        {
            return Err(fail("UNION projection columns must match"));
        }
    }
    Ok(scope)
}
fn check_group_expression(
    e: &Expr,
    keys: &[&Expr],
    locals: &BTreeSet<String>,
) -> Result<(), Error> {
    if keys.contains(&e)
        || matches!(e,Expr::Function(n,_,_) if ["count","collect","sum","avg","min","max"].contains(&n.as_str()))
    {
        return Ok(());
    }
    if let Expr::Variable(n) = e {
        if !locals.contains(n) {
            return Err(fail(
                "aggregate expression references a non-grouped variable",
            ));
        }
    }
    match e {
        Expr::Comprehension(var, list, cond, value) => {
            check_group_expression(list, keys, locals)?;
            let mut local = locals.clone();
            local.insert(var.clone());
            if let Some(cond) = cond {
                check_group_expression(cond, keys, &local)?;
            }
            check_group_expression(value, keys, &local)?;
            return Ok(());
        }
        Expr::Quantifier(_, var, list, cond) => {
            check_group_expression(list, keys, locals)?;
            let mut local = locals.clone();
            local.insert(var.clone());
            check_group_expression(cond, keys, &local)?;
            return Ok(());
        }
        Expr::Reduce(acc, init, var, list, body) => {
            check_group_expression(init, keys, locals)?;
            check_group_expression(list, keys, locals)?;
            let mut local = locals.clone();
            local.extend([acc.clone(), var.clone()]);
            check_group_expression(body, keys, &local)?;
            return Ok(());
        }
        _ => {}
    }
    for e in children(e) {
        check_group_expression(e, keys, locals)?;
    }
    Ok(())
}
fn bind(row: &mut Row, name: &Option<String>, value: Value) -> bool {
    if let Some(name) = name {
        if let Some(old) = row.get(name) {
            return old == &value;
        }
        row.insert(name.clone(), value);
    }
    true
}

fn indexed_field(expr: &Expr, variable: &str) -> Option<Vec<String>> {
    match expr {
        Expr::Variable(v) if v == variable => Some(vec![]),
        Expr::Property(base, field) => {
            let mut path = indexed_field(base, variable)?;
            path.push(field.clone());
            Some(path)
        }
        Expr::Index(base, field) => {
            let Expr::Literal(Value::String(field)) = field.as_ref() else {
                return None;
            };
            let mut path = indexed_field(base, variable)?;
            path.push(field.clone());
            Some(path)
        }
        _ => None,
    }
}
fn indexed_value(expr: &Expr, row: &Row, params: &Row) -> Option<Value> {
    match expr {
        Expr::Literal(v) => Some(v.clone()),
        Expr::Parameter(k) => params.get(k).cloned(),
        Expr::Variable(k) => row.get(k).cloned(),
        _ => None,
    }
}
fn indexed_predicates(
    expr: &Expr,
    variable: &str,
    row: &Row,
    params: &Row,
) -> Option<Vec<PropertyPredicate>> {
    if let Expr::Binary(op, a, b) = expr {
        if op == "and" {
            let mut p = indexed_predicates(a, variable, row, params)?;
            p.extend(indexed_predicates(b, variable, row, params)?);
            return Some(p);
        }
        let mut op = match op.as_str() {
            "=" => SeekOp::Equal,
            "<" => SeekOp::Less,
            "<=" => SeekOp::LessEqual,
            ">" => SeekOp::Greater,
            ">=" => SeekOp::GreaterEqual,
            _ => return None,
        };
        let (field, value) = if let (Some(path), Some(value)) =
            (indexed_field(a, variable), indexed_value(b, row, params))
        {
            (path, value)
        } else {
            let path = indexed_field(b, variable)?;
            let value = indexed_value(a, row, params)?;
            op = match op {
                SeekOp::Less => SeekOp::Greater,
                SeekOp::LessEqual => SeekOp::GreaterEqual,
                SeekOp::Greater => SeekOp::Less,
                SeekOp::GreaterEqual => SeekOp::LessEqual,
                op => op,
            };
            (path, value)
        };
        if field.is_empty() {
            return None;
        }
        return Some(vec![PropertyPredicate { field, op, value }]);
    }
    None
}

impl Runner<'_, '_> {
    fn tick(&mut self) -> Result<(), Error> {
        self.deadline.check("expression/traversal")?;
        self.expansions += 1;
        if self.expansions > self.limits.max_expansions {
            Err(fail("graph expansion/expression budget exceeded"))
        } else {
            Ok(())
        }
    }
    fn charge_edge_expansions(&mut self, count: usize) -> Result<(), Error> {
        self.deadline.check("adjacency candidates")?;
        self.edge_expansions = self.edge_expansions.saturating_add(count);
        if self.edge_expansions > self.limits.max_edge_expansions {
            Err(fail("graph edge expansion budget exceeded"))
        } else {
            Ok(())
        }
    }
    fn check_rows(&self, n: usize) -> Result<(), Error> {
        self.deadline.check("intermediate rows")?;
        if n > self.limits.max_rows {
            Err(fail(
                "graph row budget exceeded; narrow the MATCH predicate",
            ))
        } else {
            Ok(())
        }
    }
    fn property(&self, v: &Value, name: &str) -> Result<Value, Error> {
        match v {
            Value::Null => Ok(Value::Null),
            Value::Map(m) => Ok(m.get(name).cloned().unwrap_or(Value::Null)),
            Value::Node(id) => self
                .graph
                .nodes
                .get(id)
                .and_then(|n| n.properties.get(name))
                .map_or(Ok(Value::Null), Value::from_json),
            Value::Edge(id) => self
                .graph
                .edges
                .get(id)
                .and_then(|e| e.properties.get(name))
                .map_or(Ok(Value::Null), Value::from_json),
            _ => Err(fail(
                "property access requires node, relationship, map or null",
            )),
        }
    }
    fn eval(&mut self, e: &Expr, row: &Row, group: Option<&[Row]>) -> Result<Value, Error> {
        self.tick()?;
        let value = match e {
            Expr::Literal(v) => v.clone(),
            Expr::Variable(n) => row
                .get(n)
                .cloned()
                .ok_or_else(|| fail(format!("unbound variable {n}")))?,
            Expr::Parameter(n) => self.params[n].clone(),
            Expr::Property(x, n) => {
                let v = self.eval(x, row, group)?;
                self.property(&v, n)?
            }
            Expr::Label(x, l) => match self.eval(x, row, group)? {
                Value::Node(id) => Value::Bool(
                    self.graph
                        .nodes
                        .get(&id)
                        .is_some_and(|n| n.labels.contains(l)),
                ),
                Value::Null => Value::Null,
                _ => return Err(fail("label test requires node")),
            },
            Expr::List(a) => Value::List(
                a.iter()
                    .map(|e| self.eval(e, row, group))
                    .collect::<Result<_, _>>()?,
            ),
            Expr::Map(a) => Value::Map(
                a.iter()
                    .map(|(k, e)| Ok((k.clone(), self.eval(e, row, group)?)))
                    .collect::<Result<_, Error>>()?,
            ),
            Expr::Unary(op, x) => {
                let v = self.eval(x, row, group)?;
                match op.as_str() {
                    "isnull" => Value::Bool(v == Value::Null),
                    "isnotnull" => Value::Bool(v != Value::Null),
                    "not" => v.truth()?.map_or(Value::Null, |b| Value::Bool(!b)),
                    "-" => match v {
                        Value::Null => Value::Null,
                        Value::Number(n) => {
                            Value::Number(decimal("0")?.sub(&n).map_err(|e| fail(e.to_string()))?)
                        }
                        _ => return Err(fail("numeric unary minus required")),
                    },
                    _ => return Err(fail("unknown unary operator")),
                }
            }
            Expr::Binary(op, a, b) => {
                let a = self.eval(a, row, group)?;
                if op == "and" && a.truth()? == Some(false) {
                    return Ok(Value::Bool(false));
                }
                if op == "or" && a.truth()? == Some(true) {
                    return Ok(Value::Bool(true));
                }
                let b = self.eval(b, row, group)?;
                binary(op, a, b)?
            }
            Expr::Case(pairs, fallback) => {
                let mut value = None;
                for (cond, x) in pairs {
                    if self.eval(cond, row, group)?.truth()? == Some(true) {
                        value = Some(self.eval(x, row, group)?);
                        break;
                    }
                }
                if let Some(v) = value {
                    v
                } else {
                    self.eval(fallback, row, group)?
                }
            }
            Expr::Index(x, i) => {
                let v = self.eval(x, row, group)?;
                let i = self.eval(i, row, group)?;
                match (v, i) {
                    (Value::Null, _) | (_, Value::Null) => Value::Null,
                    (Value::List(a), Value::Number(n)) => {
                        let i: i64 = n
                            .to_string()
                            .parse()
                            .map_err(|_| fail("integer list index required"))?;
                        let ix = if i < 0 { a.len() as i64 + i } else { i };
                        if ix < 0 {
                            Value::Null
                        } else {
                            a.get(ix as usize).cloned().unwrap_or(Value::Null)
                        }
                    }
                    (Value::Map(m), Value::String(k)) => m.get(&k).cloned().unwrap_or(Value::Null),
                    _ => return Err(fail("invalid list/map index")),
                }
            }
            Expr::Slice(x, a, b) => {
                let v = self.eval(x, row, group)?;
                if v == Value::Null {
                    return Ok(Value::Null);
                }
                let Value::List(v) = v else {
                    return Err(fail("slice requires list"));
                };
                let index = |n: i64| {
                    if n < 0 {
                        (v.len() as i64 + n).max(0) as usize
                    } else {
                        (n as usize).min(v.len())
                    }
                };
                let start = if let Some(a) = a {
                    let n = self.eval(a, row, group)?;
                    if n == Value::Null {
                        return Ok(Value::Null);
                    }
                    index(integer_signed(&n)?)
                } else {
                    0
                };
                let end = if let Some(b) = b {
                    let n = self.eval(b, row, group)?;
                    if n == Value::Null {
                        return Ok(Value::Null);
                    }
                    index(integer_signed(&n)?)
                } else {
                    v.len()
                };
                Value::List(if start > end {
                    vec![]
                } else {
                    v[start..end].to_vec()
                })
            }
            Expr::Comprehension(var, list, filter, expr) => {
                let list = self.eval(list, row, group)?;
                if list == Value::Null {
                    return Ok(Value::Null);
                }
                let Value::List(list) = list else {
                    return Err(fail("comprehension requires list"));
                };
                let mut values = vec![];
                for v in list {
                    let mut scoped = row.clone();
                    scoped.insert(var.clone(), v);
                    if let Some(f) = filter {
                        if self.eval(f, &scoped, group)?.truth()? != Some(true) {
                            continue;
                        }
                    }
                    values.push(self.eval(expr, &scoped, group)?);
                }
                Value::List(values)
            }
            Expr::Quantifier(kind, var, list, cond) => {
                let list = self.eval(list, row, group)?;
                if list == Value::Null {
                    return Ok(Value::Null);
                }
                let Value::List(list) = list else {
                    return Err(fail("quantifier requires list"));
                };
                let (mut yes, mut null, mut no) = (0, 0, 0);
                for v in list {
                    let mut scoped = row.clone();
                    scoped.insert(var.clone(), v);
                    match self.eval(cond, &scoped, group)?.truth()? {
                        Some(true) => yes += 1,
                        Some(false) => no += 1,
                        None => null += 1,
                    }
                }
                match kind.as_str() {
                    "any" => {
                        if yes > 0 {
                            Value::Bool(true)
                        } else if null > 0 {
                            Value::Null
                        } else {
                            Value::Bool(false)
                        }
                    }
                    "all" => {
                        if no > 0 {
                            Value::Bool(false)
                        } else if null > 0 {
                            Value::Null
                        } else {
                            Value::Bool(true)
                        }
                    }
                    "none" => {
                        if yes > 0 {
                            Value::Bool(false)
                        } else if null > 0 {
                            Value::Null
                        } else {
                            Value::Bool(true)
                        }
                    }
                    _ => {
                        if yes > 1 {
                            Value::Bool(false)
                        } else if null > 0 {
                            Value::Null
                        } else {
                            Value::Bool(yes == 1)
                        }
                    }
                }
            }
            Expr::Reduce(acc, init, var, list, expr) => {
                let mut value = self.eval(init, row, group)?;
                let list = self.eval(list, row, group)?;
                if list == Value::Null {
                    return Ok(Value::Null);
                }
                let Value::List(list) = list else {
                    return Err(fail("reduce requires list"));
                };
                for v in list {
                    let mut scoped = row.clone();
                    scoped.insert(var.clone(), v);
                    scoped.insert(acc.clone(), value);
                    value = self.eval(expr, &scoped, group)?;
                }
                value
            }
            Expr::Exists(q) => {
                let (_, rows) = self.run(q, vec![row.clone()])?;
                Value::Bool(!rows.is_empty())
            }
            Expr::Function(n, args, distinct) => self.function(n, args, *distinct, row, group)?,
            Expr::Star => return Err(fail("star only permitted in projection/count")),
        };
        let mut pending = vec![&value];
        let mut bytes = 0usize;
        let mut count = 0;
        while let Some(v) = pending.pop() {
            count += 1;
            if count > 65536 {
                return Err(fail("Cypher value node budget exceeded"));
            }
            match v {
                Value::String(s) => bytes = bytes.saturating_add(s.len()),
                Value::List(v) => pending.extend(v),
                Value::Map(m) => {
                    bytes = bytes.saturating_add(m.keys().map(|k| k.len()).sum::<usize>());
                    pending.extend(m.values());
                }
                _ => {}
            }
            if bytes > self.limits.max_text_bytes {
                return Err(fail("Cypher value byte budget exceeded"));
            }
        }
        Ok(value)
    }
    fn function(
        &mut self,
        name: &str,
        args: &[Expr],
        distinct: bool,
        row: &Row,
        group: Option<&[Row]>,
    ) -> Result<Value, Error> {
        if ["count", "collect", "sum", "avg", "min", "max"].contains(&name) {
            let group = group.ok_or_else(|| fail("aggregate outside projection"))?;
            let mut values = vec![];
            let mut seen = BTreeSet::new();
            for row in group {
                let v = if matches!(args[0], Expr::Star) {
                    Value::Bool(true)
                } else {
                    self.eval(&args[0], row, None)?
                };
                if v != Value::Null && (!distinct || seen.insert(key(std::slice::from_ref(&v)))) {
                    values.push(v);
                }
            }
            return Ok(match name {
                "count" => Value::integer(values.len() as u64),
                "collect" => Value::List(values),
                "min" => values.into_iter().min_by(compare).unwrap_or(Value::Null),
                "max" => values.into_iter().max_by(compare).unwrap_or(Value::Null),
                "sum" | "avg" => {
                    let mut sum = decimal("0")?;
                    for v in &values {
                        let Value::Number(n) = v else {
                            return Err(fail("numeric aggregate requires NUMBER"));
                        };
                        sum = sum.add(n).map_err(|e| fail(e.to_string()))?;
                    }
                    if name == "avg" {
                        if values.is_empty() {
                            Value::Null
                        } else {
                            Value::Number(
                                sum.div(&decimal(&values.len().to_string())?)
                                    .map_err(|e| fail(e.to_string()))?,
                            )
                        }
                    } else {
                        Value::Number(sum)
                    }
                }
                _ => unreachable!(),
            });
        }
        if name == "coalesce" {
            for arg in args {
                let v = self.eval(arg, row, group)?;
                if v != Value::Null {
                    return Ok(v);
                }
            }
            return Ok(Value::Null);
        }
        let v = self.eval(&args[0], row, group)?;
        if name == "exists" {
            return Ok(Value::Bool(v != Value::Null));
        }
        if v == Value::Null {
            return Ok(Value::Null);
        }
        if matches!(&v,Value::Node(id) if !self.graph.nodes.contains_key(id))
            || matches!(&v,Value::Edge(id) if !self.graph.edges.contains_key(id))
        {
            return Err(fail("function references a deleted graph entity"));
        }
        Ok(match (name, v) {
            ("id", Value::Node(id) | Value::Edge(id)) => Value::integer(id),
            ("elementid", Value::Node(id)) => Value::String(self.graph.element_id('n', id)),
            ("elementid", Value::Edge(id)) => Value::String(self.graph.element_id('e', id)),
            ("labels", Value::Node(id)) => Value::List(
                self.graph.nodes[&id]
                    .labels
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect(),
            ),
            ("type", Value::Edge(id)) => Value::String(self.graph.edges[&id].label.clone()),
            ("properties", Value::Node(id)) => {
                Value::from_json(&json_map(&self.graph.nodes[&id].properties))?
            }
            ("properties", Value::Edge(id)) => {
                Value::from_json(&json_map(&self.graph.edges[&id].properties))?
            }
            ("properties", Value::Map(m)) => Value::Map(m),
            ("startnode", Value::Edge(id)) => Value::Node(self.graph.edges[&id].source),
            ("endnode", Value::Edge(id)) => Value::Node(self.graph.edges[&id].target),
            ("start_id", Value::Edge(id)) => Value::integer(self.graph.edges[&id].source),
            ("end_id", Value::Edge(id)) => Value::integer(self.graph.edges[&id].target),
            ("keys", v) => {
                let m = match v {
                    Value::Map(m) => m,
                    Value::Node(id) => {
                        match Value::from_json(&json_map(&self.graph.nodes[&id].properties))? {
                            Value::Map(m) => m,
                            _ => unreachable!(),
                        }
                    }
                    Value::Edge(id) => {
                        match Value::from_json(&json_map(&self.graph.edges[&id].properties))? {
                            Value::Map(m) => m,
                            _ => unreachable!(),
                        }
                    }
                    _ => return Err(fail("keys requires properties")),
                };
                Value::List(m.into_keys().map(Value::String).collect())
            }
            ("head", Value::List(v)) => v.first().cloned().unwrap_or(Value::Null),
            ("last", Value::List(v)) => v.last().cloned().unwrap_or(Value::Null),
            ("tail", Value::List(v)) => Value::List(v.into_iter().skip(1).collect()),
            ("size", Value::List(v)) => Value::integer(v.len() as u64),
            ("size", Value::String(v)) => Value::integer(v.chars().count() as u64),
            ("length", Value::Path(p)) => Value::integer(p.edges.len() as u64),
            ("nodes", Value::Path(p)) => {
                Value::List(p.nodes.into_iter().map(Value::Node).collect())
            }
            ("relationships", Value::Path(p)) => {
                Value::List(p.edges.into_iter().map(Value::Edge).collect())
            }
            ("tolower", Value::String(s)) => Value::String(s.to_lowercase()),
            ("toupper", Value::String(s)) => Value::String(s.to_uppercase()),
            ("trim", Value::String(s)) => Value::String(s.trim().into()),
            ("tostring", Value::String(s)) => Value::String(s),
            ("tostring", Value::Number(n)) => Value::String(n.to_string()),
            ("tostring", Value::Bool(b)) => Value::String(b.to_string()),
            ("tointeger", Value::Number(n)) => {
                let text = n.to_string();
                Value::Number(decimal(text.split('.').next().unwrap_or("0"))?)
            }
            ("tointeger", Value::String(s)) => s.parse::<i64>().ok().map_or(Value::Null, |i| {
                Value::Number(decimal(&i.to_string()).expect("i64 fits NUMBER"))
            }),
            _ => return Err(fail(format!("invalid argument type for {name}"))),
        })
    }
    fn props(&mut self, items: &[(String, Expr)], row: &Row) -> Result<Properties, Error> {
        let mut map = Properties::new();
        for (key, e) in items {
            if map.contains_key(key) {
                return Err(fail("duplicate property key"));
            }
            let v = self.eval(e, row, None)?;
            if v != Value::Null {
                map.insert(key.clone(), v.to_json(self.graph));
            }
        }
        validate_properties(&map)?;
        Ok(map)
    }
    fn ensure_node(&mut self, id: u64) -> Result<(), Error> {
        self.deadline.check("before node fetch")?;
        if self.graph.nodes.contains_key(&id) || self.absent_nodes.contains(&id) {
            return Ok(());
        }
        if let Source::Storage(storage) = &mut self.source {
            let node = storage.node(id)?;
            self.deadline.check("after node fetch")?;
            match node {
                Some(node) => {
                    if node.id != id {
                        return Err(fail("snapshot returned wrong node ID"));
                    }
                    self.graph.cache_node(node)?;
                    if self.graph.nodes.len() > self.limits.max_nodes {
                        return Err(fail("loaded node budget exceeded"));
                    }
                }
                None => {
                    self.absent_nodes.insert(id);
                }
            }
        }
        Ok(())
    }
    fn ensure_edge(&mut self, id: u64) -> Result<(), Error> {
        self.deadline.check("before edge fetch")?;
        if self.graph.edges.contains_key(&id) || self.absent_edges.contains(&id) {
            return Ok(());
        }
        if let Source::Storage(storage) = &mut self.source {
            let edge = storage.edge(id)?;
            self.deadline.check("after edge fetch")?;
            let Some(edge) = edge else {
                self.absent_edges.insert(id);
                return Ok(());
            };
            if edge.id != id {
                return Err(fail("snapshot returned wrong relationship ID"));
            }
            self.ensure_node(edge.source)?;
            self.ensure_node(edge.target)?;
            self.graph.cache_edge(edge)?;
            if self.graph.edges.len() > self.limits.max_edges {
                return Err(fail("loaded relationship budget exceeded"));
            }
        }
        Ok(())
    }
    fn node_candidates(&mut self, labels: &[String]) -> Result<Vec<u64>, Error> {
        if let Source::Storage(storage) = &mut self.source {
            let mut ids = storage.node_ids(labels)?;
            ids.extend(self.graph.candidates(labels));
            ids.sort_unstable();
            ids.dedup();
            for id in &ids {
                self.ensure_node(*id)?;
            }
            ids.retain(|id| {
                self.graph
                    .nodes
                    .get(id)
                    .is_some_and(|n| labels.iter().all(|l| n.labels.contains(l)))
            });
            Ok(ids)
        } else {
            Ok(self.graph.candidates(labels))
        }
    }
    fn neighbors(
        &mut self,
        node: u64,
        direction: Direction,
        types: &[String],
    ) -> Result<Vec<(u64, u64)>, Error> {
        if let Source::Storage(storage) = &mut self.source {
            let mut ids = storage.edge_ids(node, direction, types)?;
            ids.extend(
                self.graph
                    .neighbors(node, direction, types)
                    .into_iter()
                    .map(|(id, _)| id),
            );
            // One candidate per ID for this adjacency request. A fresh visit
            // charges again even when its entity is already in the cache.
            ids.sort_unstable();
            ids.dedup();
            self.charge_edge_expansions(ids.len())?;
            for id in ids {
                // Ghosts and residual type/direction mismatches still consume
                // the candidate budget. Reject before fetching a too-wide set.
                self.ensure_edge(id)?;
            }
            Ok(self.graph.neighbors(node, direction, types))
        } else {
            let neighbors = self.graph.neighbors(node, direction, types);
            self.charge_edge_expansions(neighbors.len())?;
            Ok(neighbors)
        }
    }
    fn node_matches(&mut self, id: u64, p: &NodePattern, row: &Row) -> Result<bool, Error> {
        self.tick()?;
        self.ensure_node(id)?;
        let Some(n) = self.graph.nodes.get(&id) else {
            return Ok(false);
        };
        if !p.labels.iter().all(|l| n.labels.contains(l)) {
            return Ok(false);
        }
        if let Some(v) = p.variable.as_ref().and_then(|v| row.get(v)) {
            if v != &Value::Node(id) {
                return Ok(false);
            }
        }
        for (key, expr) in &p.properties {
            let expected = self.eval(expr, row, None)?;
            let actual = self.property(&Value::Node(id), key)?;
            if equals(&actual, &expected) != Some(true) {
                return Ok(false);
            }
        }
        Ok(true)
    }
    fn matches(
        &mut self,
        p: &Pattern,
        row: &Row,
        filter: Option<&Expr>,
    ) -> Result<Vec<Row>, Error> {
        if p.edges.iter().map(|e| e.maximum).sum::<usize>() > self.limits.max_depth {
            return Err(fail("total path exceeds configured depth budget"));
        }
        // Reordering predicates must not hide an error in an earlier pattern
        // expression. Labels remain safe, but expression-derived pruning needs
        // all pattern property values to be literal/parameter/already bound.
        let safe_patterns = p
            .nodes
            .iter()
            .flat_map(|n| n.properties.iter().map(|(_, e)| e))
            .chain(
                p.edges
                    .iter()
                    .flat_map(|e| e.properties.iter().map(|(_, e)| e)),
            )
            .all(|expr| indexed_value(expr, row, self.params).is_some());
        let anchors = match p.nodes[0].variable.as_ref().and_then(|v| row.get(v)) {
            Some(Value::Node(id)) => vec![*id],
            Some(Value::Null) => vec![],
            Some(_) => return Err(fail("node variable has incompatible type")),
            None => {
                let first = &p.nodes[0];
                let mut predicates: Vec<_> = first
                    .properties
                    .iter()
                    .filter(|_| safe_patterns)
                    .filter_map(|(field, e)| {
                        indexed_value(e, row, self.params).map(|value| PropertyPredicate {
                            field: vec![field.clone()],
                            op: SeekOp::Equal,
                            value,
                        })
                    })
                    .collect();
                if let (true, Some(filter), Some(variable)) =
                    (safe_patterns, filter, first.variable.as_ref())
                {
                    if let Some(p) = indexed_predicates(filter, variable, row, self.params) {
                        if self.index_predicates_total(EntityKind::Node, &first.labels, &p)? {
                            predicates.extend(p);
                        }
                    }
                }
                let node_candidates = if self.mutations == 0 {
                    self.source.candidates(&IndexRequest {
                        entity: EntityKind::Node,
                        labels: first.labels.clone(),
                        predicates,
                    })?
                } else {
                    None
                };
                if let Some(ids) = node_candidates {
                    ids
                } else if self.mutations == 0
                    && safe_patterns
                    && p.edges
                        .first()
                        .is_some_and(|e| e.minimum == 1 && e.maximum == 1)
                {
                    let e = &p.edges[0];
                    let mut predicates: Vec<_> = e
                        .properties
                        .iter()
                        .filter_map(|(field, v)| {
                            indexed_value(v, row, self.params).map(|value| PropertyPredicate {
                                field: vec![field.clone()],
                                op: SeekOp::Equal,
                                value,
                            })
                        })
                        .collect();
                    if let (Some(filter), Some(variable)) = (filter, e.variable.as_ref()) {
                        if let Some(p) = indexed_predicates(filter, variable, row, self.params) {
                            if self.index_predicates_total(
                                EntityKind::Relationship,
                                &e.types,
                                &p,
                            )? {
                                predicates.extend(p);
                            }
                        }
                    }
                    if let Some(ids) = self.source.candidates(&IndexRequest {
                        entity: EntityKind::Relationship,
                        labels: e.types.clone(),
                        predicates,
                    })? {
                        let mut anchors = BTreeSet::new();
                        for id in ids {
                            self.ensure_edge(id)?;
                            if let Some(edge) = self.graph.edges.get(&id) {
                                if e.direction != Direction::In {
                                    anchors.insert(edge.source);
                                }
                                if e.direction != Direction::Out {
                                    anchors.insert(edge.target);
                                }
                            }
                        }
                        anchors.into_iter().collect()
                    } else {
                        self.node_candidates(&first.labels)?
                    }
                } else {
                    self.node_candidates(&first.labels)?
                }
            }
        };
        let mut out = vec![];
        for id in anchors {
            if self.node_matches(id, &p.nodes[0], row)? {
                let mut row = row.clone();
                if bind(&mut row, &p.nodes[0].variable, Value::Node(id)) {
                    self.chain(
                        p,
                        0,
                        row,
                        Path {
                            nodes: vec![id],
                            edges: vec![],
                        },
                        &mut out,
                    )?;
                }
            }
            self.check_rows(out.len())?;
        }
        Ok(out)
    }

    // The current storage layer already loads the graph. Conservatively prove
    // that WHERE property access and range comparisons are total on that image
    // before moving them ahead of pattern/AND evaluation. If not, use the
    // original scan order (including its short-circuit and NULL semantics).
    fn index_predicates_total(
        &mut self,
        entity: EntityKind,
        labels: &[String],
        predicates: &[PropertyPredicate],
    ) -> Result<bool, Error> {
        let predicates: Vec<_> = predicates
            .iter()
            .filter(|p| p.field.len() > 1 || (p.op != SeekOp::Equal && p.value != Value::Null))
            .collect();
        if predicates.is_empty() {
            return Ok(true);
        }
        // A relationship range/type proof would require a global edge scan.
        // Keep its original evaluation order on the lazy snapshot instead.
        if entity == EntityKind::Relationship && matches!(self.source, Source::Storage(_)) {
            return Ok(false);
        }
        let values: Vec<_> = match entity {
            EntityKind::Node => self
                .node_candidates(labels)?
                .into_iter()
                .map(Value::Node)
                .collect(),
            EntityKind::Relationship => self
                .graph
                .edges
                .values()
                .filter(|e| labels.is_empty() || labels.contains(&e.label))
                .map(|e| Value::Edge(e.id))
                .collect(),
        };
        for entity in values {
            self.tick()?;
            for predicate in &predicates {
                let mut value = entity.clone();
                for part in &predicate.field {
                    let Ok(next) = self.property(&value, part) else {
                        return Ok(false);
                    };
                    value = next;
                }
                if predicate.op != SeekOp::Equal
                    && value != Value::Null
                    && predicate.value != Value::Null
                    && std::mem::discriminant(&value) != std::mem::discriminant(&predicate.value)
                {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }
    fn chain(
        &mut self,
        p: &Pattern,
        index: usize,
        row: Row,
        path: Path,
        out: &mut Vec<Row>,
    ) -> Result<(), Error> {
        if index == p.edges.len() {
            let mut row = row;
            if bind(&mut row, &p.variable, Value::Path(path.clone())) {
                let mut used = match row.remove("$used") {
                    Some(Value::List(v)) => v,
                    _ => vec![],
                };
                used.extend(path.edges.into_iter().map(Value::Edge));
                row.insert("$used".into(), Value::List(used));
                out.push(row);
                self.check_rows(out.len())?;
            }
            return Ok(());
        }
        let ep = &p.edges[index];
        if ep.maximum > self.limits.max_depth {
            return Err(fail("path exceeds configured depth budget"));
        }
        let start = *path.nodes.last().expect("nonempty path");
        let mut walks = vec![];
        let mut used = path.edges.clone();
        if let Some(Value::List(ids)) = row.get("$used") {
            used.extend(ids.iter().filter_map(|v| {
                if let Value::Edge(id) = v {
                    Some(*id)
                } else {
                    None
                }
            }));
        }
        self.walk(ep, start, vec![start], vec![], &used, &row, &mut walks)?;
        if p.shortest {
            let mut lengths = BTreeMap::<u64, usize>::new();
            for (nodes, edges) in &walks {
                let end = *nodes.last().unwrap();
                lengths
                    .entry(end)
                    .and_modify(|n| *n = (*n).min(edges.len()))
                    .or_insert(edges.len());
            }
            walks.retain(|(nodes, edges)| lengths[nodes.last().unwrap()] == edges.len());
            let mut seen = BTreeSet::new();
            walks.retain(|(nodes, _)| seen.insert(*nodes.last().unwrap()));
        }
        for (nodes, edges) in walks {
            let id = *nodes.last().unwrap();
            if !self.node_matches(id, &p.nodes[index + 1], &row)? {
                continue;
            }
            let mut scoped = row.clone();
            let v = if ep.variable_length {
                Value::List(edges.iter().copied().map(Value::Edge).collect())
            } else {
                Value::Edge(edges[0])
            };
            if !bind(&mut scoped, &ep.variable, v)
                || !bind(&mut scoped, &p.nodes[index + 1].variable, Value::Node(id))
            {
                continue;
            }
            let mut path = path.clone();
            path.nodes.extend(nodes.into_iter().skip(1));
            path.edges.extend(edges);
            self.chain(p, index + 1, scoped, path, out)?;
        }
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    fn walk(
        &mut self,
        p: &EdgePattern,
        current: u64,
        nodes: Vec<u64>,
        edges: Vec<u64>,
        used: &[u64],
        row: &Row,
        out: &mut Vec<(Vec<u64>, Vec<u64>)>,
    ) -> Result<(), Error> {
        if edges.len() >= p.minimum {
            out.push((nodes.clone(), edges.clone()));
            self.check_rows(out.len())?;
        }
        if edges.len() == p.maximum {
            return Ok(());
        }
        for (eid, next) in self.neighbors(current, p.direction, &p.types)? {
            self.tick()?;
            if edges.contains(&eid) || used.contains(&eid) {
                continue;
            }
            let mut matches = true;
            for (key, e) in &p.properties {
                let expect = self.eval(e, row, None)?;
                if equals(&self.property(&Value::Edge(eid), key)?, &expect) != Some(true) {
                    matches = false;
                    break;
                }
            }
            if !matches {
                continue;
            }
            let mut ns = nodes.clone();
            ns.push(next);
            let mut es = edges.clone();
            es.push(eid);
            self.walk(p, next, ns, es, used, row, out)?;
        }
        Ok(())
    }
    fn project(
        &mut self,
        p: &Projection,
        rows: Vec<Row>,
    ) -> Result<(Vec<String>, Vec<Row>), Error> {
        let has_agg = p.items.iter().any(|(e, _)| aggregate(e));
        let mut groups: Vec<Vec<Row>> = vec![];
        if has_agg {
            let mut positions = BTreeMap::new();
            for row in rows {
                let vals = p
                    .items
                    .iter()
                    .filter(|(e, _)| !aggregate(e))
                    .map(|(e, _)| self.eval(e, &row, None))
                    .collect::<Result<Vec<_>, _>>()?;
                let k = key(&vals);
                let index = *positions.entry(k).or_insert_with(|| {
                    groups.push(vec![]);
                    groups.len() - 1
                });
                groups[index].push(row);
            }
            if groups.is_empty() && p.items.iter().all(|(e, _)| aggregate(e)) {
                groups.push(vec![]);
            }
        } else {
            groups = rows.into_iter().map(|r| vec![r]).collect();
        }
        let mut columns = vec![];
        for (e, n) in &p.items {
            if matches!(e, Expr::Star) {
                if has_agg {
                    return Err(fail("star aggregate projection unsupported"));
                }
                if let Some(row) = groups.first().and_then(|g| g.first()) {
                    for name in row.keys() {
                        if !columns.contains(name) {
                            columns.push(name.clone());
                        }
                    }
                }
            } else {
                if columns.contains(n) {
                    return Err(fail("duplicate projection column"));
                }
                columns.push(n.clone());
            }
        }
        let mut out = vec![];
        let mut seen = BTreeSet::new();
        let empty = Row::new();
        for group in groups {
            let source = group.first().unwrap_or(&empty);
            let mut projected = Row::new();
            for (e, n) in &p.items {
                if matches!(e, Expr::Star) {
                    projected.extend(source.clone());
                } else {
                    let v = self.eval(e, source, has_agg.then_some(group.as_slice()))?;
                    projected.insert(n.clone(), v);
                }
            }
            if let Some(f) = &p.filter {
                if self.eval(f, &projected, None)?.truth()? != Some(true) {
                    continue;
                }
            }
            if p.distinct
                && !seen.insert(key(&columns
                    .iter()
                    .map(|c| projected[c].clone())
                    .collect::<Vec<_>>()))
            {
                continue;
            }
            let mut ordering = source.clone();
            ordering.extend(projected.clone());
            let keys = p
                .order
                .iter()
                .map(|(e, _)| self.eval(e, &ordering, has_agg.then_some(group.as_slice())))
                .collect::<Result<Vec<_>, _>>()?;
            out.push((projected, keys));
        }
        out.sort_by(|a, b| {
            for (index, (_, descending)) in p.order.iter().enumerate() {
                let cmp = compare(&a.1[index], &b.1[index]);
                if cmp != Ordering::Equal {
                    return if *descending { cmp.reverse() } else { cmp };
                }
            }
            Ordering::Equal
        });
        let skip = if let Some(e) = &p.skip {
            self.eval(e, &empty, None)?.as_usize()?
        } else {
            0
        };
        let limit = if let Some(e) = &p.limit {
            self.eval(e, &empty, None)?.as_usize()?
        } else {
            self.limits.max_rows
        };
        let rows = out
            .into_iter()
            .skip(skip)
            .take(limit)
            .map(|(r, _)| r)
            .collect::<Vec<_>>();
        self.check_rows(rows.len())?;
        Ok((columns, rows))
    }
    fn create(&mut self, p: &Pattern, mut row: Row, merge: bool) -> Result<Row, Error> {
        if p.shortest
            || p.edges.iter().any(|e| {
                e.variable_length
                    || (!merge && e.direction == Direction::Both)
                    || e.types.len() != 1
            })
        {
            return Err(fail(
                "CREATE requires fixed directed relationships with one type",
            ));
        }
        let mut nodes = vec![];
        for n in &p.nodes {
            let id = match n.variable.as_ref().and_then(|v| row.get(v)) {
                Some(Value::Node(id)) => {
                    let id = *id;
                    if merge {
                        if !self.node_matches(id, n, &row)? {
                            return Err(fail(
                                "bound MERGE node does not satisfy the complete pattern",
                            ));
                        }
                    } else if !n.labels.is_empty() || !n.properties.is_empty() {
                        return Err(fail("bound CREATE node cannot redefine labels/properties"));
                    }
                    id
                }
                Some(_) => return Err(fail("CREATE node binding type mismatch")),
                None => {
                    let props = self.props(&n.properties, &row)?;
                    let id = self
                        .graph
                        .add_node(n.labels.iter().cloned().collect(), props)?;
                    self.mutations += 1;
                    bind(&mut row, &n.variable, Value::Node(id));
                    id
                }
            };
            nodes.push(id);
        }
        let mut edges = vec![];
        for (index, e) in p.edges.iter().enumerate() {
            if e.variable.as_ref().is_some_and(|v| row.contains_key(v)) {
                return Err(fail("CREATE relationship variable already bound"));
            }
            let props = self.props(&e.properties, &row)?;
            let (a, b) = if e.direction == Direction::In {
                (nodes[index + 1], nodes[index])
            } else {
                (nodes[index], nodes[index + 1])
            };
            let id = self.graph.add_edge(a, b, e.types[0].clone(), props)?;
            self.mutations += 1;
            bind(&mut row, &e.variable, Value::Edge(id));
            edges.push(id);
        }
        if !bind(&mut row, &p.variable, Value::Path(Path { nodes, edges })) {
            return Err(fail("CREATE path already bound"));
        }
        Ok(row)
    }
    fn set_items(&mut self, items: &[(Expr, Expr)], row: &Row) -> Result<(), Error> {
        for (lhs, rhs) in items {
            let value = self.eval(rhs, row, None)?;
            self.update(lhs, Some(value), row)?;
        }
        Ok(())
    }
    fn validate_merge_values(&mut self, pattern: &Pattern, row: &Row) -> Result<(), Error> {
        for properties in pattern
            .nodes
            .iter()
            .map(|n| &n.properties)
            .chain(pattern.edges.iter().map(|e| &e.properties))
        {
            for (key, expr) in properties {
                if self.eval(expr, row, None)? == Value::Null {
                    return Err(fail(format!("MERGE property {key} cannot be null")));
                }
            }
        }
        Ok(())
    }
    fn update(&mut self, lhs: &Expr, value: Option<Value>, row: &Row) -> Result<(), Error> {
        match lhs {
            Expr::Property(base, key) => {
                let target = self.eval(base, row, None)?;
                let json = value.unwrap_or(Value::Null).to_json(self.graph);
                let props = match target {
                    Value::Node(id) => &mut self.graph.node_mut(id)?.properties,
                    Value::Edge(id) => &mut self.graph.edge_mut(id)?.properties,
                    Value::Null => return Ok(()),
                    _ => return Err(fail("SET/REMOVE requires graph entity")),
                };
                if json.is_null() {
                    props.remove(key);
                } else {
                    props.insert(key.clone(), json);
                }
                validate_properties(props)?;
                self.mutations += 1;
            }
            Expr::Label(base, label) => {
                let target = self.eval(base, row, None)?;
                match target {
                    Value::Node(id) => {
                        self.graph.update_label(id, label, value.is_some())?;
                        self.mutations += 1;
                    }
                    Value::Null => {}
                    _ => return Err(fail("label update requires node")),
                }
            }
            _ => return Err(fail("only property/label SET and REMOVE supported")),
        }
        Ok(())
    }
    /// Preflight before removing explicit edges or nodes. Duplicate incidence
    /// keys count as work, but self-loops/shared endpoints count as one edge.
    /// The counter belongs to this Runner, so nested CALL/UNION cannot reset it.
    fn detach_plan(&mut self, nodes: &BTreeSet<u64>) -> Result<BTreeSet<u64>, Error> {
        let mut edges = BTreeSet::new();
        let mut work = 0usize;
        for node in nodes {
            if matches!(self.source, Source::Storage(_)) {
                self.neighbors(*node, Direction::Both, &[])?;
            }
            for edge in self.graph.incident_edge_ids(*node) {
                self.deadline.check("DETACH incidence")?;
                work = work.saturating_add(1);
                if self.expansions.saturating_add(work) > self.limits.max_expansions {
                    return Err(fail("graph expansion/expression budget exceeded"));
                }
                if !matches!(self.source, Source::Storage(_))
                    && self.edge_expansions.saturating_add(work) > self.limits.max_edge_expansions
                {
                    return Err(fail("graph edge expansion budget exceeded"));
                }
                if edges.insert(*edge)
                    && self.detach_edges.saturating_add(edges.len()) > self.limits.max_detach_edges
                {
                    return Err(fail(format!(
                        "DETACH DELETE relationship budget exceeded (limit {}, already used {}); delete relationships in separate batches first",
                        self.limits.max_detach_edges, self.detach_edges
                    )));
                }
            }
        }
        self.expansions = self.expansions.saturating_add(work);
        if !matches!(self.source, Source::Storage(_)) {
            self.edge_expansions = self.edge_expansions.saturating_add(work);
        }
        self.detach_edges = self.detach_edges.saturating_add(edges.len());
        Ok(edges)
    }
    fn run(&mut self, q: &Query, initial: Vec<Row>) -> Result<(Vec<String>, Vec<Row>), Error> {
        self.deadline.check("query branch")?;
        let mut rows = initial.clone();
        let mut columns = vec![];
        for clause in &q.clauses {
            match clause {
                Clause::Match(patterns, optional, filter) => {
                    let mut next = vec![];
                    for input in rows {
                        let mut candidates = vec![input.clone()];
                        for p in patterns {
                            let mut expanded = vec![];
                            for r in &candidates {
                                // WHERE is evaluated after every comma-separated
                                // pattern. Keep that order when later patterns
                                // could fail before WHERE has been reached.
                                expanded.extend(self.matches(
                                    p,
                                    r,
                                    if patterns.len() == 1 {
                                        filter.as_ref()
                                    } else {
                                        None
                                    },
                                )?);
                                self.check_rows(expanded.len())?;
                            }
                            candidates = expanded;
                        }
                        if let Some(f) = filter {
                            let mut filtered = vec![];
                            for r in candidates {
                                if self.eval(f, &r, None)?.truth()? == Some(true) {
                                    filtered.push(r);
                                }
                            }
                            candidates = filtered;
                        }
                        if *optional && candidates.is_empty() {
                            let mut r = input;
                            for p in patterns {
                                for name in p
                                    .variable
                                    .iter()
                                    .chain(p.nodes.iter().filter_map(|n| n.variable.as_ref()))
                                    .chain(p.edges.iter().filter_map(|e| e.variable.as_ref()))
                                {
                                    r.entry(name.clone()).or_insert(Value::Null);
                                }
                            }
                            candidates.push(r);
                        }
                        for r in &mut candidates {
                            r.remove("$used");
                        }
                        next.extend(candidates);
                        self.check_rows(next.len())?;
                    }
                    rows = next;
                }
                Clause::Search(args, yielded) => {
                    let mut next = vec![];
                    for row in rows {
                        let q = self.eval(&args[0], &row, None)?;
                        let options = self.eval(&args[1], &row, None)?;
                        let (Value::String(q), Value::Map(options)) = (q, options) else {
                            return Err(fail("searchNodes expects STRING and MAP"));
                        };
                        let (found, mode) = if let Some(mut indexed) =
                            crate::retrieval::indexed(&q, &options)?
                        {
                            if indexed.automatic {
                                crate::retrieval::resolve_automatic(
                                    &mut indexed,
                                    self.source.fulltext_indexes()?,
                                )?;
                            }
                            let mut streams = vec![];
                            for request in &indexed.requests {
                                let hits = self.source.fulltext(
                                    request,
                                    (self.mutations > 0).then_some(&*self.graph),
                                )?;
                                self.expansions = self.expansions.saturating_add(hits.len());
                                if self.expansions > self.limits.max_expansions {
                                    return Err(fail("search query expansion budget exceeded"));
                                }
                                if hits.len() > indexed.limit {
                                    return Err(fail(
                                        "indexed search provider exceeded requested limit",
                                    ));
                                }
                                let mut seen = BTreeSet::new();
                                let mut stream = vec![];
                                for hit in hits {
                                    if hit.id == 0 || hit.id >= 1 << 48 || !seen.insert(hit.id) {
                                        return Err(fail(
                                            "invalid/duplicate indexed search hit ID",
                                        ));
                                    }
                                    self.ensure_node(hit.id)?;
                                    let node = self.graph.nodes.get(&hit.id).ok_or_else(|| {
                                        fail("indexed search returned missing source")
                                    })?;
                                    if node.properties.get("db_type").and_then(|v| v.as_str())
                                        != Some(indexed.domain.as_str())
                                        || !indexed.labels.is_empty()
                                            && !indexed
                                                .labels
                                                .iter()
                                                .any(|l| node.labels.contains(l))
                                    {
                                        return Err(fail("indexed search returned source outside requested scope"));
                                    }
                                    stream.push(hit.id);
                                }
                                streams.push(stream);
                            }
                            let mode = if self.mutations > 0 {
                                "strict_staged_round_robin"
                            } else if indexed.eventual {
                                "fulltext_round_robin"
                            } else {
                                "strict_round_robin"
                            };
                            (crate::retrieval::round_robin(&streams, indexed.limit), mode)
                        } else {
                            self.node_candidates(&[])?;
                            self.expansions =
                                self.expansions.saturating_add(self.graph.nodes.len());
                            if self.expansions > self.limits.max_expansions {
                                return Err(fail("search query expansion budget exceeded"));
                            }
                            (
                                crate::retrieval::search(self.graph, &q, &options, self.limits)?,
                                "bounded_scan",
                            )
                        };
                        for (rank, id) in found.into_iter().enumerate() {
                            let mut row = row.clone();
                            for (column, alias) in yielded {
                                if row.contains_key(alias) {
                                    return Err(fail("YIELD cannot shadow a variable"));
                                }
                                row.insert(
                                    alias.clone(),
                                    match column.as_str() {
                                        "node" => Value::Node(id),
                                        "rank" => Value::integer(rank as u64 + 1),
                                        _ => Value::String(mode.into()),
                                    },
                                );
                            }
                            next.push(row);
                            self.check_rows(next.len())?;
                        }
                    }
                    rows = next;
                }
                Clause::Filter(filter) => {
                    let mut next = vec![];
                    for row in rows {
                        if self.eval(filter, &row, None)?.truth()? == Some(true) {
                            next.push(row);
                        }
                    }
                    rows = next;
                }
                Clause::Fulltext(entity, args, yielded) => {
                    let mut next = vec![];
                    for row in rows {
                        let index = self.eval(&args[0], &row, None)?;
                        let query = self.eval(&args[1], &row, None)?;
                        let options = self.eval(&args[2], &row, None)?;
                        let (Value::String(index), Value::String(query), Value::Map(options)) =
                            (index, query, options)
                        else {
                            return Err(fail("full-text procedure expects STRING,STRING,MAP"));
                        };
                        let request = FulltextRequest {
                            entity: *entity,
                            index,
                            query,
                            options,
                        };
                        let hits = self
                            .source
                            .fulltext(&request, (self.mutations > 0).then_some(&*self.graph))?;
                        self.expansions = self.expansions.saturating_add(hits.len());
                        if self.expansions > self.limits.max_expansions {
                            return Err(fail("full-text expansion budget exceeded"));
                        }
                        let Some(Value::String(domain)) = request.options.get("db_type") else {
                            return Err(fail("full-text procedure requires string db_type"));
                        };
                        let mut seen = BTreeSet::new();
                        for (rank, hit) in hits.into_iter().enumerate() {
                            if hit.id == 0 || hit.id >= 1 << 48 || !seen.insert(hit.id) {
                                return Err(fail("invalid/duplicate full-text hit ID"));
                            }
                            let source_domain = if *entity == EntityKind::Node {
                                self.ensure_node(hit.id)?;
                                self.graph.nodes.get(&hit.id).map(|n| &n.properties)
                            } else {
                                self.ensure_edge(hit.id)?;
                                self.graph.edges.get(&hit.id).map(|e| &e.properties)
                            }
                            .ok_or_else(|| fail("full-text provider returned missing source"))?
                            .get("db_type")
                            .and_then(|v| v.as_str());
                            if source_domain != Some(domain.as_str()) {
                                return Err(fail(
                                    "full-text provider returned source outside requested domain",
                                ));
                            }
                            if let Some(labels) = request.options.get("labels") {
                                let Value::List(labels) = labels else {
                                    return Err(fail("full-text labels require a list"));
                                };
                                if *entity != EntityKind::Node
                                    || labels.iter().any(|label| !matches!(label, Value::String(_)))
                                    || !labels.is_empty() && !labels.iter().any(|label| {
                                        matches!(label, Value::String(label) if self.graph.nodes[&hit.id].labels.contains(label))
                                    }) {
                                    return Err(fail("full-text provider returned source outside requested labels"));
                                }
                            }
                            let mut output = row.clone();
                            for (column, alias) in yielded {
                                let value = match column.as_str() {
                                    "node" => Value::Node(hit.id),
                                    "relationship" => Value::Edge(hit.id),
                                    "rank" => Value::integer(rank as u64 + 1),
                                    _ => hit.columns.get(column).cloned().ok_or_else(|| {
                                        fail(format!("missing full-text metadata column {column}"))
                                    })?,
                                };
                                if output.insert(alias.clone(), value).is_some() {
                                    return Err(fail("YIELD cannot shadow a variable"));
                                }
                            }
                            next.push(output);
                            self.check_rows(next.len())?;
                        }
                    }
                    rows = next;
                }
                Clause::Call(imported, sub) => {
                    let mut next = vec![];
                    for row in rows {
                        let scope = imported
                            .iter()
                            .map(|n| {
                                row.get(n)
                                    .cloned()
                                    .map(|v| (n.clone(), v))
                                    .ok_or_else(|| fail(format!("unbound CALL import {n}")))
                            })
                            .collect::<Result<Row, _>>()?;
                        let (_, returned) = self.run(sub, vec![scope])?;
                        for r in returned {
                            let mut merged = row.clone();
                            for (k, v) in r {
                                if merged.contains_key(&k) {
                                    return Err(fail("CALL cannot shadow outer variable"));
                                }
                                merged.insert(k, v);
                            }
                            next.push(merged);
                            self.check_rows(next.len())?;
                        }
                    }
                    rows = next;
                }
                Clause::Unwind(expr, name) => {
                    let mut next = vec![];
                    for r in rows {
                        let v = self.eval(expr, &r, None)?;
                        let values = match v {
                            Value::List(v) => v,
                            Value::Null => vec![],
                            _ => return Err(fail("UNWIND requires list")),
                        };
                        for v in values {
                            let mut row = r.clone();
                            row.insert(name.clone(), v);
                            next.push(row);
                            self.check_rows(next.len())?;
                        }
                    }
                    rows = next;
                }
                Clause::Project(p, _) => {
                    (columns, rows) = self.project(p, rows)?;
                }
                Clause::Create(patterns) => {
                    for p in patterns {
                        let mut next = vec![];
                        for row in rows {
                            next.push(self.create(p, row, false)?);
                            self.check_rows(next.len())?;
                        }
                        rows = next;
                    }
                }
                Clause::Merge(patterns, on_create, on_match) => {
                    let pattern = &patterns[0];
                    let mut next = vec![];
                    for row in rows {
                        self.validate_merge_values(pattern, &row)?;
                        let matched = self.matches(pattern, &row, None)?;
                        if matched.is_empty() {
                            let created = self.create(pattern, row, true)?;
                            self.set_items(on_create, &created)?;
                            next.push(created);
                        } else {
                            self.check_rows(next.len().saturating_add(matched.len()))?;
                            for matched_row in matched {
                                self.set_items(on_match, &matched_row)?;
                                next.push(matched_row);
                            }
                        }
                        self.check_rows(next.len())?;
                    }
                    rows = next;
                }
                Clause::Set(items) => {
                    for row in &rows {
                        self.set_items(items, row)?;
                    }
                }
                Clause::Remove(items) => {
                    for row in &rows {
                        for lhs in items {
                            self.update(lhs, None, row)?;
                        }
                    }
                }
                Clause::Delete(items, detach) => {
                    let mut nodes = BTreeSet::new();
                    let mut edges = BTreeSet::new();
                    for row in &rows {
                        for expr in items {
                            match self.eval(expr, row, None)? {
                                Value::Node(id) => {
                                    nodes.insert(id);
                                }
                                Value::Edge(id) => {
                                    edges.insert(id);
                                }
                                Value::Null => {}
                                _ => return Err(fail("DELETE requires node or relationship")),
                            }
                        }
                    }
                    if *detach {
                        let incident = self.detach_plan(&nodes)?;
                        for id in incident {
                            self.graph.remove_edge(id);
                            self.absent_edges.insert(id);
                        }
                    }
                    for id in edges {
                        self.graph.remove_edge(id);
                        self.absent_edges.insert(id);
                        self.mutations += 1;
                    }
                    for id in nodes {
                        if !*detach && matches!(self.source, Source::Storage(_)) {
                            self.neighbors(id, Direction::Both, &[])?;
                        }
                        // DETACH preflight removed every incidence key already.
                        // Preserve the existing affected count (explicit entities).
                        self.graph.remove_node(id, false)?;
                        self.absent_nodes.insert(id);
                        self.mutations += 1;
                    }
                }
            }
            self.check_rows(rows.len())?;
        }
        if let Some((all, right)) = &q.union {
            let (other_columns, other) = self.run(right, initial)?;
            if other_columns != columns {
                return Err(fail("UNION projection columns must match"));
            }
            rows.extend(other);
            if !all {
                let mut seen = BTreeSet::new();
                rows.retain(|r| {
                    seen.insert(key(&columns
                        .iter()
                        .map(|c| r[c].clone())
                        .collect::<Vec<_>>()))
                });
            }
            self.check_rows(rows.len())?;
        }
        Ok((columns, rows))
    }
}
fn integer_signed(v: &Value) -> Result<i64, Error> {
    match v {
        Value::Number(n) => n
            .to_string()
            .parse()
            .map_err(|_| fail("signed integer required")),
        _ => Err(fail("signed integer required")),
    }
}
fn binary(op: &str, a: Value, b: Value) -> Result<Value, Error> {
    if ["and", "or", "xor"].contains(&op) {
        let (a, b) = (a.truth()?, b.truth()?);
        return Ok(match op {
            "and" => {
                if a == Some(false) || b == Some(false) {
                    Value::Bool(false)
                } else if a.is_none() || b.is_none() {
                    Value::Null
                } else {
                    Value::Bool(true)
                }
            }
            "or" => {
                if a == Some(true) || b == Some(true) {
                    Value::Bool(true)
                } else if a.is_none() || b.is_none() {
                    Value::Null
                } else {
                    Value::Bool(false)
                }
            }
            _ => match (a, b) {
                (Some(a), Some(b)) => Value::Bool(a ^ b),
                _ => Value::Null,
            },
        });
    }
    if a == Value::Null || b == Value::Null {
        return Ok(Value::Null);
    }
    Ok(match (op, a, b) {
        ("=", a, b) => equals(&a, &b).map_or(Value::Null, Value::Bool),
        ("<>", a, b) => equals(&a, &b).map_or(Value::Null, |v| Value::Bool(!v)),
        ("<" | ">" | "<=" | ">=", a, b) => {
            if std::mem::discriminant(&a) != std::mem::discriminant(&b) {
                return Err(fail("comparison type mismatch"));
            }
            let c = compare(&a, &b);
            Value::Bool(match op {
                "<" => c.is_lt(),
                ">" => c.is_gt(),
                "<=" => !c.is_gt(),
                _ => !c.is_lt(),
            })
        }
        ("in", a, Value::List(b)) => {
            let comparisons = b.iter().map(|v| equals(&a, v)).collect::<Vec<_>>();
            if comparisons.contains(&Some(true)) {
                Value::Bool(true)
            } else if comparisons.contains(&None) {
                Value::Null
            } else {
                Value::Bool(false)
            }
        }
        ("contains", Value::String(a), Value::String(b)) => Value::Bool(a.contains(&b)),
        ("starts", Value::String(a), Value::String(b)) => Value::Bool(a.starts_with(&b)),
        ("ends", Value::String(a), Value::String(b)) => Value::Bool(a.ends_with(&b)),
        ("+", Value::String(a), Value::String(b)) => Value::String(a + &b),
        ("+", Value::List(mut a), Value::List(b)) => {
            a.extend(b);
            Value::List(a)
        }
        ("+" | "-" | "*" | "/", Value::Number(a), Value::Number(b)) => Value::Number(
            match op {
                "+" => a.add(&b),
                "-" => a.sub(&b),
                "*" => a.mul(&b),
                _ => a.div(&b),
            }
            .map_err(|e| fail(e.to_string()))?,
        ),
        _ => return Err(fail(format!("invalid operands for {op}"))),
    })
}
