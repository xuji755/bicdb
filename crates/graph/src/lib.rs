//! # bicdb-graph
//!
//! 命名图、顶点/边存储、Cypher 子集、有界遍历
//!
//! - 设计依据：§13 有限属性图与AGE参考边界
//! - 对应阶段：P10
//!
//! A bounded property-graph query core. Native SQL owns persistence, identity and
//! transactions; this crate never opens a remote Neo4j connection.

#![forbid(unsafe_code)]

pub mod access;
pub mod adjacency_record;
pub mod corpus_proof;
mod deadline;
pub mod fulltext;
pub mod fulltext_journal;
mod model;
mod parser;
pub mod property_index;
mod query;
mod retrieval;
pub mod storage;

pub use deadline::Deadline;
pub use model::{
    Direction, Edge, Error, Graph, GraphChanges, GraphCorpus, Limits, Node, Path, Properties,
    Value, DEFAULT_DETACH_EDGE_LIMIT, MAX_DETACH_EDGE_LIMIT,
};
pub use parser::{parse, Query};
pub use query::{
    execute, execute_with_indexes, execute_with_indexes_deadline, execute_with_storage,
    execute_with_storage_deadline, execute_with_storage_write_deadline, QueryResult,
};
