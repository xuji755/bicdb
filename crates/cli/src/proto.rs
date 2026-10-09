//! **桥：引擎形状 ↔ 线上形状**（`bicdb-net` 的服务端/客户端两侧都用它）。
//!
//! ```text
//! 引擎形状（bicdb-sql/bicdb-exec）       线上形状（bicdb-net）
//!   QueryResult::Rows{ColumnMeta,Value}  ◀▶  Statement::Rows{Column,Value}
//!   ColKind::{Number,Bool,Bytes}         ◀▶  kind: 'n' | 'o' | 'b'
//! ```
//!
//! **为什么不把转换放进 `bicdb-net`**：`bicdb-net` 是**协议**——它必须不认识
//! 引擎（Python 驱动实现同一份规格，不带引擎；Rust 驱动同理）。桥放在
//! `bicdb-cli`（两边的依赖都在），**协议与引擎各守各的边界**。
//!
//! **双向都可逆**（除一处记档）：线上数值是十进制文本，回引擎时经
//! `Number::parse`；理论上有引擎写得出的数、文本表达不回去——那时退化为
//! `Bytes`（**不丢字节**，只丢"它是数"这个形态判断）。

use bicdb_exec::{ColKind, Value};
use bicdb_net::{Column, Statement};
use bicdb_sql::session::{ColumnMeta, QueryResult};

/// 引擎列形态 → 线上标记。
#[must_use]
pub fn kind_char(kind: ColKind) -> char {
    match kind {
        ColKind::Number => 'n',
        ColKind::Bool => 'o',
        ColKind::Bytes => 'b',
        ColKind::GraphElement => 'g',
    }
}

/// 线上标记 → 引擎列形态（不认识的标记按**字节串**——不静默改判定）。
#[must_use]
pub fn kind_of(tag: char) -> ColKind {
    match tag {
        'n' => ColKind::Number,
        'o' => ColKind::Bool,
        'g' => ColKind::GraphElement,
        _ => ColKind::Bytes,
    }
}

/// 引擎值 → 线上值。
#[must_use]
pub fn wire_value(v: &Value) -> bicdb_net::Value {
    match v {
        Value::Null => bicdb_net::Value::Null,
        Value::Bool(b) => bicdb_net::Value::Bool(*b),
        Value::Number(n) => bicdb_net::Value::Number(n.to_string()),
        Value::Bytes(b) => bicdb_net::Value::Bytes(b.clone()),
        Value::GraphElement(element) => bicdb_net::Value::GraphElement {
            graph: element.graph,
            kind: match element.kind {
                bicdb_exec::GraphElementKind::Node => 'n',
                bicdb_exec::GraphElementKind::Edge => 'e',
            },
            id: element.id,
        },
    }
}

/// 线上值 → 引擎值。
#[must_use]
pub fn engine_value(v: &bicdb_net::Value) -> Value {
    match v {
        bicdb_net::Value::Null => Value::Null,
        bicdb_net::Value::Bool(b) => Value::Bool(*b),
        bicdb_net::Value::Number(t) => match bicdb_types::Number::parse(t) {
            Ok(n) => Value::Number(n),
            Err(_) => Value::Bytes(t.as_bytes().to_vec()),
        },
        bicdb_net::Value::Bytes(b) => Value::Bytes(b.clone()),
        bicdb_net::Value::GraphElement { graph, kind, id } => {
            let kind = match kind {
                'n' => bicdb_exec::GraphElementKind::Node,
                'e' => bicdb_exec::GraphElementKind::Edge,
                _ => return Value::Bytes(format!("{graph}:{kind}:{id}").into_bytes()),
            };
            Value::GraphElement(bicdb_exec::GraphElement {
                graph: *graph,
                kind,
                id: *id,
            })
        }
    }
}

/// 命名参数（出：引擎值 → 线上值）。
#[must_use]
pub fn wire_params(named: &[(&str, Value)]) -> Vec<(String, bicdb_net::Value)> {
    named
        .iter()
        .map(|(n, v)| ((*n).to_owned(), wire_value(v)))
        .collect()
}

/// 命名参数（入：线上值 → 引擎值）。
#[must_use]
pub fn engine_params(named: &[(String, bicdb_net::Value)]) -> Vec<(String, Value)> {
    named
        .iter()
        .map(|(n, v)| (n.clone(), engine_value(v)))
        .collect()
}

/// 结果（出：`bicdb-sql` → 线上）。
#[must_use]
pub fn statements(results: &[QueryResult]) -> Vec<Statement> {
    results
        .iter()
        .map(|r| match r {
            QueryResult::Rows { columns, rows } => Statement::Rows {
                columns: columns
                    .iter()
                    .map(|m| Column {
                        name: m.name.clone(),
                        kind: kind_char(m.kind),
                        // 结果集的列定义只有名与形态（可空/类型码属于 `DESCRIBE`）。
                        nullable: true,
                        type_code: 0,
                        length: 0,
                        type_name: String::new(),
                    })
                    .collect(),
                rows: rows
                    .iter()
                    .map(|row| row.iter().map(wire_value).collect())
                    .collect(),
            },
            QueryResult::Affected(n) => Statement::Affected(*n),
            QueryResult::Ddl(t) => Statement::Ddl(t.clone()),
            QueryResult::Txn(t) => Statement::Txn(t.clone()),
        })
        .collect()
}

/// 结果（入：线上 → `bicdb-sql` 形状——**呈现代码两条路共用一份**）。
#[must_use]
pub fn results(list: &[Statement]) -> Vec<QueryResult> {
    list.iter()
        .map(|s| match s {
            Statement::Rows { columns, rows } => QueryResult::Rows {
                columns: columns
                    .iter()
                    .map(|c| ColumnMeta {
                        name: c.name.clone(),
                        kind: kind_of(c.kind),
                    })
                    .collect(),
                rows: rows
                    .iter()
                    .map(|row| row.iter().map(engine_value).collect())
                    .collect(),
            },
            Statement::Affected(n) => QueryResult::Affected(*n),
            Statement::Ddl(t) => QueryResult::Ddl(t.clone()),
            Statement::Txn(t) => QueryResult::Txn(t.clone()),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bicdb_types::Number;

    fn round_trip(v: Value) {
        let back = engine_value(&wire_value(&v));
        match (&v, &back) {
            (Value::Number(a), Value::Number(b)) => assert_eq!(a.to_string(), b.to_string()),
            _ => assert_eq!(v, back),
        }
    }

    #[test]
    fn values_round_trip() {
        round_trip(Value::Null);
        round_trip(Value::Bool(true));
        round_trip(Value::Number(Number::parse("123.450").expect("数")));
        round_trip(Value::Number(Number::parse("-0.001").expect("数")));
        round_trip(Value::Bytes(b"raw\x00\xff".to_vec()));
        round_trip(Value::Bytes(Vec::new()));
        round_trip(Value::GraphElement(bicdb_exec::GraphElement {
            graph: 42,
            kind: bicdb_exec::GraphElementKind::Edge,
            id: 9,
        }));
    }

    #[test]
    fn result_round_trip() {
        let src = vec![
            QueryResult::Rows {
                columns: vec![
                    ColumnMeta {
                        name: "ID".to_owned(),
                        kind: ColKind::Number,
                    },
                    ColumnMeta {
                        name: "NAME".to_owned(),
                        kind: ColKind::Bytes,
                    },
                ],
                rows: vec![
                    vec![
                        Value::Number(Number::parse("1").expect("数")),
                        Value::Bytes("alpha".as_bytes().to_vec()),
                    ],
                    vec![Value::Null, Value::Bytes(vec![0xff, 0x00])],
                ],
            },
            QueryResult::Affected(3),
            QueryResult::Ddl("已建表 t".to_owned()),
            QueryResult::Txn("已提交".to_owned()),
        ];
        // 走一遍**线上的字节**（不是只比结构），这样编码/解码两边都被覆盖。
        let bytes = bicdb_net::message::encode_statements(&statements(&src));
        let back = results(&bicdb_net::message::decode_statements(&bytes).expect("解"));
        assert_eq!(back.len(), src.len());
        let QueryResult::Rows { columns, rows } = &back[0] else {
            panic!("第一条应是结果集");
        };
        assert_eq!(columns.len(), 2);
        assert_eq!(columns[0].kind, ColKind::Number);
        assert_eq!(columns[1].kind, ColKind::Bytes);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], Value::Number(Number::parse("1").expect("数")));
        assert_eq!(rows[0][1], Value::Bytes(b"alpha".to_vec()));
        assert_eq!(rows[1][0], Value::Null);
        assert_eq!(rows[1][1], Value::Bytes(vec![0xff, 0x00]));
        assert_eq!(back[1], QueryResult::Affected(3));
        assert_eq!(back[2], QueryResult::Ddl("已建表 t".to_owned()));
        assert_eq!(back[3], QueryResult::Txn("已提交".to_owned()));
    }
}
