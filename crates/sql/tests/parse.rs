//! **切片 S1 验收**：词法 / 语法 / Raw AST。
//!
//! 钉住：语句闭集（REQ-SQL-005 正面清单）可解析；**清单外构造零接受**
//! （REQ-SQL-006 的"语法上不存在"）；错误带字节区间；AST 不 import 目录接口
//! （REQ-SQL-002 的验收原文，用源码文本自检）。

use bicdb_sql::ast::{AlterAction, BinaryOp, Literal, OptionValue, SetOpKind, Stmt, TxnStmt};
use bicdb_sql::parser::{parse, parse_many};

/// 语句闭集语料（正面清单逐类各若干）。
const CORPUS: &[&str] = &[
    // ── DDL ──
    "CREATE TABLE t (id NUMBER NOT NULL, name BYTES)",
    "CREATE TABLE memory (id NUMBER) WITH (table_type = memory, retention = '7d')",
    "DROP TABLE t",
    "CREATE INDEX ix ON t (id)",
    "CREATE UNIQUE INDEX ix ON t (name)",
    "CREATE INDEX jx ON t (json_get(doc, 'a.b'))",
    "CREATE INDEX vx ON g VERTEX (name)",
    "CREATE INDEX ex ON g EDGE (weight)",
    "DROP INDEX ix",
    "CREATE GRAPH g",
    "DROP GRAPH g",
    "CREATE WORKSPACE FOR USER alice NAME 'a1'",
    "CREATE WORKSPACE FOR USER alice CLONE OF 7",
    "ALTER WORKSPACE 7 SET NAME = 'x'",
    "ALTER WORKSPACE 7 SET NAME = NULL",
    "ALTER WORKSPACE 7 SET QUOTA (data = 1024, undo = 512, temp = 4096, asset = 2048)",
    "DROP WORKSPACE 7",
    // ── DML ──
    "SELECT 1",
    "SELECT * FROM t",
    "SELECT id, name FROM t WHERE id = 1",
    "SELECT DISTINCT name FROM t",
    "SELECT t.id FROM t JOIN u ON t.id = u.id",
    "SELECT t.id FROM t INNER JOIN u ON t.id = u.id",
    "SELECT * FROM t LEFT JOIN u ON t.id = u.id",
    "SELECT * FROM t LEFT OUTER JOIN u ON t.id = u.id",
    "SELECT * FROM t, u WHERE t.id = u.id",
    "SELECT g, COUNT(*), SUM(x), AVG(x), MIN(x), MAX(x) FROM t GROUP BY g HAVING COUNT(*) > 1",
    "SELECT COUNT(DISTINCT x) FROM t",
    "SELECT g FROM t GROUP BY g ORDER BY g DESC, 1 ASC",
    "SELECT * FROM t ORDER BY id LIMIT 10 OFFSET 5",
    "SELECT a FROM t UNION SELECT a FROM u",
    "SELECT a FROM t UNION ALL SELECT a FROM u",
    "SELECT a FROM t INTERSECT SELECT a FROM u",
    "SELECT a FROM t EXCEPT ALL SELECT a FROM u",
    "SELECT a FROM t UNION SELECT a FROM u UNION ALL SELECT a FROM v ORDER BY 1 LIMIT 3",
    "INSERT INTO t VALUES (1, 'a')",
    "INSERT INTO t (id, name) VALUES (1, 'a'), (2, 'b')",
    "UPDATE t SET name = 'x', id = id + 1 WHERE id > 1",
    "DELETE FROM t WHERE id = 1",
    "DELETE FROM t",
    // ── 事务 ──
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    // ── 表达式构件 ──
    "SELECT CASE WHEN a IS NULL THEN 0 WHEN a > 1 THEN 1 ELSE 2 END FROM t",
    "SELECT CASE a WHEN 1 THEN 'x' ELSE 'y' END FROM t",
    "SELECT CAST(a AS NUMBER(10,2)) FROM t",
    "SELECT COALESCE(a, b, 0), NULLIF(a, b) FROM t",
    "SELECT a BETWEEN 1 AND 10, a NOT BETWEEN 1 AND 10 FROM t",
    "SELECT a IN (1, 2, 3), a NOT IN (1, 2) FROM t",
    "SELECT a IS NOT NULL, NOT (a = 1 AND b = 2) FROM t",
    "SELECT -a + +b * 2 / (c - 1) FROM t",
    "SELECT json_get(doc, 'a.b'), json_exists(doc, 'x'), json_set(doc, 'x', 1) FROM t",
    "SELECT v <-> :q, v <=> :q, v <#> :q FROM t ORDER BY v <-> :q LIMIT 5",
];

/// 闭集之外的构造（REQ-SQL-006）——**语法上不存在**，零接受。
const REJECTED: &[&str] = &[
    "WITH x AS (SELECT 1) SELECT * FROM x",
    "SELECT * FROM t WHERE EXISTS (SELECT 1)",
    "SELECT (SELECT 1) FROM t",
    "SELECT * FROM t WHERE id IN (SELECT id FROM u)",
    "SELECT ROW_NUMBER() OVER (ORDER BY id) FROM t",
    "SELECT * FROM t RIGHT JOIN u ON t.id = u.id",
    "SELECT * FROM t FULL JOIN u ON t.id = u.id",
    "SELECT * FROM t NATURAL JOIN u",
    "SELECT * FROM t JOIN u USING (id)",
    "INSERT INTO t SELECT * FROM u",
    "UPDATE t SET id = 1 RETURNING id",
    "DELETE FROM t RETURNING id",
    "CREATE TABLE t (id NUMBER PRIMARY KEY)",
    "CREATE TABLE t (id NUMBER DEFAULT 0)",
    "CREATE TABLE t (id NUMBER CHECK (id > 0))",
    "CREATE TABLE t (id NUMBER REFERENCES u (id))",
    "SELECT * FROM t ORDER BY id FOR UPDATE",
    "SELECT * FROM t WHERE name LIKE 'a%'",
    "SAVEPOINT sp",
    "TRUNCATE TABLE t",
    "ALTER TABLE t ADD COLUMN c NUMBER",
    "SELECT * FROM t LIMIT 1 OFFSET 2 OFFSET 3",
];

#[test]
fn statement_closure_all_parses() {
    for sql in CORPUS {
        let parsed = parse(sql);
        assert!(parsed.is_ok(), "应可解析：{sql}\n错误：{:?}", parsed.err());
    }
}

#[test]
fn closed_set_constructs_are_never_accepted() {
    for sql in REJECTED {
        let parsed = parse(sql);
        assert!(
            parsed.is_err(),
            "清单外构造必须零接受：{sql}\n解析成了：{parsed:?}"
        );
    }
}

#[test]
fn parse_many_splits_statements() {
    let stmts = parse_many("BEGIN; SELECT 1; COMMIT;").unwrap();
    assert_eq!(stmts.len(), 3);
    assert!(matches!(
        stmts[0],
        Stmt::Txn {
            kind: TxnStmt::Begin,
            ..
        }
    ));
    assert!(matches!(stmts[1], Stmt::Select(_)));
    assert!(matches!(
        stmts[2],
        Stmt::Txn {
            kind: TxnStmt::Commit,
            ..
        }
    ));
    // 缺分号分隔 ⇒ 报错（不静默拼接）。
    assert!(parse_many("SELECT 1 SELECT 2").is_err());
}

#[test]
fn select_shape_is_faithful() {
    let stmt = parse("SELECT DISTINCT a, b AS c FROM t WHERE a > 1 GROUP BY a, b HAVING COUNT(*) > 2 ORDER BY a DESC LIMIT 10 OFFSET 3").unwrap();
    let Stmt::Select(s) = stmt else {
        panic!("不是 SELECT")
    };
    assert!(s.first.distinct);
    assert_eq!(s.first.projection.len(), 2);
    assert_eq!(s.first.projection[1].alias.as_deref(), Some("c"));
    assert!(s.first.from.is_some());
    assert!(s.first.filter.is_some());
    assert_eq!(s.first.group_by.len(), 2);
    assert!(s.first.having.is_some());
    assert_eq!(s.order_by.len(), 1);
    assert!(s.order_by[0].desc);
    assert_eq!(s.limit.as_deref(), Some("10"));
    assert_eq!(s.offset.as_deref(), Some("3"));
    assert!(s.set_ops.is_empty());
}

#[test]
fn set_ops_chain_left_to_right_and_order_binds_to_the_expression() {
    let stmt = parse("SELECT a FROM t UNION ALL SELECT a FROM u EXCEPT SELECT a FROM v").unwrap();
    let Stmt::Select(s) = stmt else {
        panic!("不是 SELECT")
    };
    assert_eq!(s.set_ops.len(), 2);
    assert_eq!(s.set_ops[0].op, SetOpKind::Union);
    assert!(s.set_ops[0].all);
    assert_eq!(s.set_ops[1].op, SetOpKind::Except);
    assert!(!s.set_ops[1].all, "未写 ALL ⇒ 去重（SQL 标准默认）");

    // ORDER BY / LIMIT 属于**整个查询表达式**（标准形态）——链尾之后仍可写。
    let stmt2 = parse("SELECT a FROM t UNION SELECT a FROM u ORDER BY 1 LIMIT 2").unwrap();
    let Stmt::Select(s2) = stmt2 else {
        panic!("不是 SELECT")
    };
    assert_eq!(s2.set_ops.len(), 1);
    assert_eq!(s2.order_by.len(), 1);
    assert_eq!(s2.limit.as_deref(), Some("2"));
}

#[test]
fn expression_precedence_is_explicit() {
    // a OR b AND c ⇒ a OR (b AND c)
    let stmt = parse("SELECT a OR b AND c FROM t").unwrap();
    let Stmt::Select(s) = stmt else {
        panic!("不是 SELECT")
    };
    let Some(expr) = &s.first.projection[0].expr else {
        panic!("应为表达式")
    };
    let bicdb_sql::ast::Expr::Binary { op, right, .. } = expr else {
        panic!("应为二元")
    };
    assert_eq!(*op, BinaryOp::Or);
    assert!(matches!(
        **right,
        bicdb_sql::ast::Expr::Binary {
            op: BinaryOp::And,
            ..
        }
    ));

    // 1 - 2 - 3 ⇒ (1-2)-3（左结合）
    let stmt2 = parse("SELECT 1 - 2 - 3 FROM t").unwrap();
    let Stmt::Select(s2) = stmt2 else {
        panic!("不是 SELECT")
    };
    let Some(bicdb_sql::ast::Expr::Binary { left, op, .. }) = &s2.first.projection[0].expr else {
        panic!("应为二元")
    };
    assert_eq!(*op, BinaryOp::Sub);
    assert!(matches!(
        **left,
        bicdb_sql::ast::Expr::Binary {
            op: BinaryOp::Sub,
            ..
        }
    ));

    // 一元负号绑定紧于乘法：-a * b ⇒ (-a) * b
    let stmt3 = parse("SELECT -a * b FROM t").unwrap();
    let Stmt::Select(s3) = stmt3 else {
        panic!("不是 SELECT")
    };
    let Some(bicdb_sql::ast::Expr::Binary { left, op, .. }) = &s3.first.projection[0].expr else {
        panic!("应为二元")
    };
    assert_eq!(*op, BinaryOp::Mul);
    assert!(matches!(**left, bicdb_sql::ast::Expr::Unary { .. }));
}

#[test]
fn literals_keep_raw_text_and_positions() {
    let stmt = parse("SELECT 1.50, 'a''b', NULL, TRUE FROM t").unwrap();
    let Stmt::Select(s) = stmt else {
        panic!("不是 SELECT")
    };
    let lit = |i: usize| match s.first.projection[i].expr.as_ref().unwrap() {
        bicdb_sql::ast::Expr::Literal { value, span } => (value.clone(), *span),
        other => panic!("第 {i} 项不是字面量：{other:?}"),
    };
    // 数字：**原文本**（`1.50` 不折叠——值的解析在 ② 走 TYP 内核）。
    assert_eq!(lit(0).0, Literal::Number("1.50".to_owned()));
    // 字符串：`''` 转义已解。
    assert_eq!(lit(1).0, Literal::Str(b"a'b".to_vec()));
    assert_eq!(lit(2).0, Literal::Null);
    assert_eq!(lit(3).0, Literal::Bool(true));
    // 位置正确（字节区间）。
    let (_, span) = lit(0);
    assert_eq!(
        &"SELECT 1.50, 'a''b', NULL, TRUE FROM t"[span.start..span.end],
        "1.50"
    );
}

#[test]
fn errors_carry_byte_spans() {
    let e = parse("SELECT * FROM t WHERE").unwrap_err();
    assert!(e.span.start >= 20, "错误位置落在语句内：{e:?}");
    let e2 = parse("SELECT * FROM t WHERE name LIKE 'a%'").unwrap_err();
    assert!(e2.message.contains("LIKE"), "指向不支持清单：{e2}");
    let e3 = parse("SELECT * FROM t RIGHT JOIN u ON 1 = 1").unwrap_err();
    assert!(e3.message.contains("RIGHT"), "{e3}");
    let e4 = parse("GRAPH_TABLE").unwrap_err();
    assert!(!e4.message.is_empty());
}

#[test]
fn graph_table_is_loudly_deferred() {
    // GRAPH_TABLE 随图域接入；本切片**响亮拒绝**（不静默当成普通表名）。
    let e = parse("SELECT * FROM GRAPH_TABLE (g)").unwrap_err();
    assert!(e.message.contains("GRAPH_TABLE"), "{e}");
}

#[test]
fn ddl_shapes_are_faithful() {
    let Stmt::CreateTable(ct) =
        parse("CREATE TABLE t (id NUMBER(10,2) NOT NULL, name BYTES) WITH (table_type = memory)")
            .unwrap()
    else {
        panic!("不是 CREATE TABLE")
    };
    assert_eq!(ct.name, "t");
    assert_eq!(ct.columns.len(), 2);
    assert_eq!(ct.columns[0].type_name.name, "NUMBER");
    assert_eq!(ct.columns[0].type_name.args, vec!["10", "2"]);
    assert!(ct.columns[0].not_null);
    assert!(!ct.columns[1].not_null);
    assert_eq!(ct.options.len(), 1);
    assert_eq!(ct.options[0].name, "table_type");
    assert_eq!(ct.options[0].value, OptionValue::Ident("memory".to_owned()));

    let Stmt::CreateIndex(ci) = parse("CREATE UNIQUE INDEX ix ON g VERTEX (name)").unwrap() else {
        panic!("不是 CREATE INDEX")
    };
    assert!(ci.unique);
    assert!(matches!(ci.target, bicdb_sql::ast::IndexTarget::Vertex(ref n) if n == "g"));
    assert_eq!(ci.keys.len(), 1);

    let Stmt::AlterWorkspace(aw) =
        parse("ALTER WORKSPACE 7 SET QUOTA (data = 1, temp = 2)").unwrap()
    else {
        panic!("不是 ALTER WORKSPACE")
    };
    let AlterAction::SetQuota(items) = &aw.action else {
        panic!("不是 SET QUOTA")
    };
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].value, OptionValue::Number("1".to_owned()));
}

#[test]
fn dml_shapes_are_faithful() {
    let Stmt::Insert(ins) = parse("INSERT INTO t (id, name) VALUES (1, 'a'), (2, 'b')").unwrap()
    else {
        panic!("不是 INSERT")
    };
    assert_eq!(ins.table, "t");
    assert_eq!(ins.columns.as_ref().unwrap().len(), 2);
    assert_eq!(ins.rows.len(), 2);

    let Stmt::Update(up) = parse("UPDATE t SET a = 1, b = a + 1 WHERE c = 2").unwrap() else {
        panic!("不是 UPDATE")
    };
    assert_eq!(up.sets.len(), 2);
    assert!(up.filter.is_some());

    let Stmt::Delete(del) = parse("DELETE FROM t").unwrap() else {
        panic!("不是 DELETE")
    };
    assert!(del.filter.is_none(), "无条件 DELETE 合法（全表删）");
}

/// **依赖检查**（REQ-SQL-002 的验收原文）：AST 模块不 import 任何目录接口。
/// 用源码文本自检——比"人肉审查"可复跑。
#[test]
fn ast_module_does_not_reference_catalog() {
    let ast_src = include_str!("../src/ast.rs");
    for needle in [
        "bicdb_catalog",
        "catalog::",
        "use bicdb_storage",
        "use bicdb_txn",
    ] {
        assert!(
            !ast_src.contains(needle),
            "Raw AST 不得引用 `{needle}`（REQ-SQL-002）"
        );
    }
    // 也不得出现在 lexer/parser 的 AST 构造路径上。
    let parser_src = include_str!("../src/parser.rs");
    for needle in ["bicdb_catalog", "catalog::"] {
        assert!(!parser_src.contains(needle), "解析期不得碰目录");
    }
}

#[test]
fn keyword_colliding_names_fold_consistently() {
    // 与关键字同形的列名（`name` 是 CREATE WORKSPACE 的关键字）：
    // 词法给关键字，解析层取规范大写形态——与 ② 的大写折叠一致。
    let Stmt::CreateTable(ct) = parse("CREATE TABLE t (name BYTES)").unwrap() else {
        panic!("不是 CREATE TABLE")
    };
    assert_eq!(ct.columns[0].name, "NAME");
    // 普通标识符保留原文本（折叠在 ②）。
    let Stmt::CreateTable(ct2) = parse("CREATE TABLE T (MyCol BYTES)").unwrap() else {
        panic!("不是 CREATE TABLE")
    };
    assert_eq!(ct2.name, "T");
    assert_eq!(ct2.columns[0].name, "MyCol");
}
