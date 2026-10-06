//! **切片 S1 验收**：词法 / 语法 / Raw AST（**形状对齐 PostgreSQL**）。
//!
//! 钉住：语句闭集（REQ-SQL-005 正面清单）可解析；**清单外构造零接受**
//! （REQ-SQL-006）；错误带字节区间；**标识符折叠照 PG**（未引号小写、引号保留）；
//! **优先级照 PG**（含两个反直觉点：`BETWEEN`/`IN` 比比较更紧、集合运算有优先级）；
//! AST 不 import 目录接口（REQ-SQL-002 验收原文，源码自检）。

use bicdb_sql::ast::{
    AExprKind, AlterDatabaseAction, AlterSystemAction, AlterWorkspaceAction, BoolExprType,
    ColumnRefField, ConstValue, Expr, FromItem, ObjectType, SetOperation, SortByDir, Stmt,
    TransactionStmtKind, VariableSetKind, WorkspaceSource,
};
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
    // ── DCL（`DCL语句设计` §1/§2；本库扩展组）──
    "CREATE WORKSPACE FOR USER alice",
    "CREATE WORKSPACE FOR USER alice NAME 'a1'",
    "ALTER WORKSPACE 7 SET NAME = 'x'",
    "ALTER WORKSPACE 7 SET NAME = NULL",
    "ALTER WORKSPACE 'prod' FOR USER alice SET NAME = 'p2'",
    "ALTER WORKSPACE 7 SET QUOTA (data = 1024, undo = 512, temp = 4096, asset = 2048)",
    "DROP WORKSPACE 7",
    "DROP WORKSPACE 'prod' FOR USER alice",
    "ALTER DATABASE CLONE WORKSPACE 'new' FROM WORKSPACE 7",
    "ALTER DATABASE CLONE WORKSPACE 'new' FROM WORKSPACE 'prod' FOR USER alice",
    "ALTER DATABASE CLONE WORKSPACE 'new' FROM TEMPLATE 'base'",
    "ALTER DATABASE ADD TEMPLATE 'base' FROM 7",
    "ALTER DATABASE ADD TEMPLATE 'base' FROM 'prod' FOR USER alice",
    "ALTER DATABASE ALTER WORKSPACE 7 TO TEMPLATE 'base'",
    "ALTER DATABASE DROP TEMPLATE 'base'",
    "ALTER SYSTEM ADD FILESYSTEM '/mnt/d1'",
    "ALTER SYSTEM ALTER FILESYSTEM '/mnt/d1' SET ALLOCATE = OFF",
    "ALTER SYSTEM ALTER FILESYSTEM 3 SET ALLOCATE = ON",
    "ALTER SYSTEM DROP FILESYSTEM 3",
    "ALTER SESSION SET work_area_size = '64MB'",
    "ALTER SESSION SET max_query_memory = 4294967296",
    "ALTER SESSION CLEAR work_area_size",
    // ── DML ──
    "SELECT 1",
    "SELECT * FROM t",
    "SELECT t.* FROM t",
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
    "SELECT a::NUMBER(10,2) FROM t",
    "SELECT COALESCE(a, b, 0), NULLIF(a, b) FROM t",
    "SELECT a BETWEEN 1 AND 10, a NOT BETWEEN 1 AND 10 FROM t",
    "SELECT a IN (1, 2, 3), a NOT IN (1, 2) FROM t",
    "SELECT a IS NOT NULL, NOT (a = 1 AND b = 2) FROM t",
    "SELECT -a + +b * 2 / (c - 1) FROM t",
    "SELECT json_get(doc, 'a.b'), json_exists(doc, 'x'), json_set(doc, 'x', 1) FROM t",
    "SELECT v <-> :q, v <=> :q, v <#> :q FROM t ORDER BY v <-> :q LIMIT 5",
    // ── 引号标识符（PG 口径）──
    "SELECT \"MyCol\" FROM \"MyTable\"",
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
    "UPDATE t SET id = 1 RETURNING id",
    "DELETE FROM t RETURNING id",
    "CREATE TABLE t (id NUMBER PRIMARY KEY)",
    "CREATE TABLE t (id NUMBER DEFAULT 0)",
    "CREATE TABLE t (id NUMBER CHECK (id > 0))",
    "CREATE TABLE t (id NUMBER REFERENCES u (id))",
    "SELECT * FROM t ORDER BY id FOR UPDATE",
    "SELECT * FROM t WHERE name LIKE 'a%'",
    "SELECT * FROM t ORDER BY id NULLS FIRST",
    "SAVEPOINT sp",
    "TRUNCATE TABLE t",
    "ALTER TABLE t ADD COLUMN c NUMBER",
    "SELECT * FROM t LIMIT 1 OFFSET 2 OFFSET 3",
    "DROP TABLE IF EXISTS t",
    "INSERT INTO t SELECT * FROM u",
    // ── DCL 闭集外（`DCL语句设计` §2.3：没有产生式就是没有）──
    "CREATE WORKSPACE FOR USER alice CLONE OF 7",
    "SET work_area_size = '64MB'",
    "ALTER SYSTEM SET work_memory_target = '4GiB'",
    "ALTER SYSTEM SWITCH LOGFILE",
    "ALTER DATABASE RENAME TO x",
    "ALTER DATABASE CLONE WORKSPACE 'new' FROM TABLE t",
    "ALTER WORKSPACE 7 SET QUOTA (foo = 1)",
    "ALTER WORKSPACE 7 SET OWNER = 'x'",
    "ALTER SYSTEM ALTER FILESYSTEM 3 SET ALLOCATE = MAYBE",
    "ALTER SESSION SET work_area_size",
    "DROP WORKSPACE",
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
        &stmts[0],
        Stmt::Transaction(t) if t.kind == TransactionStmtKind::Begin
    ));
    assert!(matches!(&stmts[1], Stmt::Select(_)));
    assert!(matches!(
        &stmts[2],
        Stmt::Transaction(t) if t.kind == TransactionStmtKind::Commit
    ));
    assert!(parse_many("SELECT 1 SELECT 2").is_err());
}

#[test]
fn identifier_folding_follows_pg() {
    // 未引号 ⇒ 折叠小写（词法层，照 PG 的扫描器）。
    let Stmt::Select(s) = parse("SELECT MyCol FROM MyTable").unwrap() else {
        panic!("不是 SELECT")
    };
    let FromItem::RangeVar(rv) = &s.from_clause[0] else {
        panic!("不是表引用")
    };
    assert_eq!(rv.relname, "mytable");
    let Expr::ColumnRef(cr) = &s.target_list[0].val else {
        panic!("不是列引用")
    };
    assert_eq!(cr.fields, vec![ColumnRefField::Name("mycol".to_owned())]);

    // 引号 ⇒ 原样保留、大小写敏感。
    let Stmt::Select(s2) = parse("SELECT \"MyCol\" FROM \"Tbl\"").unwrap() else {
        panic!("不是 SELECT")
    };
    let FromItem::RangeVar(rv2) = &s2.from_clause[0] else {
        panic!("不是表引用")
    };
    assert_eq!(rv2.relname, "Tbl");
    let Expr::ColumnRef(cr2) = &s2.target_list[0].val else {
        panic!("不是列引用")
    };
    assert_eq!(cr2.fields, vec![ColumnRefField::Name("MyCol".to_owned())]);

    // 与关键字同形的名字（`name`）也按原文本折叠——不再是特例。
    let Stmt::CreateTable(ct) = parse("CREATE TABLE t (Name BYTES)").unwrap() else {
        panic!("不是 CREATE TABLE")
    };
    assert_eq!(ct.table_elts[0].colname, "name");
}

#[test]
fn select_shape_follows_pg_nodes() {
    let stmt = parse("SELECT DISTINCT a, b AS c FROM t WHERE a > 1 GROUP BY a, b HAVING COUNT(*) > 2 ORDER BY a DESC LIMIT 10 OFFSET 3").unwrap();
    let Stmt::Select(s) = stmt else {
        panic!("不是 SELECT")
    };
    assert!(s.distinct);
    assert_eq!(s.target_list.len(), 2);
    assert_eq!(s.target_list[1].name.as_deref(), Some("c"));
    assert_eq!(s.from_clause.len(), 1);
    assert!(s.where_clause.is_some());
    assert_eq!(s.group_clause.len(), 2);
    assert!(s.having_clause.is_some());
    assert_eq!(s.sort_clause.len(), 1);
    assert_eq!(s.sort_clause[0].sortby_dir, SortByDir::Desc);
    assert!(matches!(
        s.limit_count.as_deref(),
        Some(Expr::AConst(c)) if c.value == Some(ConstValue::Int("10".to_owned()))
    ));
    assert!(s.limit_offset.is_some());
    assert!(s.op.is_none());
    assert!(s.larg.is_none() && s.rarg.is_none());
}

#[test]
fn star_is_a_column_ref_field_like_pg() {
    let Stmt::Select(s) = parse("SELECT * FROM t").unwrap() else {
        panic!("不是 SELECT")
    };
    let Expr::ColumnRef(cr) = &s.target_list[0].val else {
        panic!("`*` 应是 ColumnRef（照 PG）")
    };
    assert_eq!(cr.fields, vec![ColumnRefField::AStar]);

    let Stmt::Select(s2) = parse("SELECT t.* FROM t").unwrap() else {
        panic!("不是 SELECT")
    };
    let Expr::ColumnRef(cr2) = &s2.target_list[0].val else {
        panic!("`t.*` 应是 ColumnRef")
    };
    assert_eq!(
        cr2.fields,
        vec![ColumnRefField::Name("t".to_owned()), ColumnRefField::AStar]
    );
}

#[test]
fn set_ops_nest_left_deep_with_pg_precedence() {
    let Stmt::Select(s) =
        parse("SELECT a FROM t UNION SELECT a FROM u UNION ALL SELECT a FROM v").unwrap()
    else {
        panic!("不是 SELECT")
    };
    // 左深：((t UNION u) UNION ALL v)
    assert_eq!(s.op, Some(SetOperation::Union));
    assert!(s.all);
    let larg = s.larg.as_deref().expect("左操作数");
    assert_eq!(larg.op, Some(SetOperation::Union));
    assert!(!larg.all, "未写 ALL ⇒ 去重");
    assert!(larg.larg.is_some() && larg.rarg.is_some());
    assert!(s.rarg.is_some());

    // **INTERSECT 比 UNION 紧**（照 PG 的优先级）：a UNION b INTERSECT c
    // ⇒ a UNION (b INTERSECT c)
    let Stmt::Select(s2) =
        parse("SELECT a FROM t UNION SELECT a FROM u INTERSECT SELECT a FROM v").unwrap()
    else {
        panic!("不是 SELECT")
    };
    assert_eq!(s2.op, Some(SetOperation::Union));
    let rarg = s2.rarg.as_deref().expect("右操作数");
    assert_eq!(rarg.op, Some(SetOperation::Intersect));

    // ORDER BY / LIMIT 属**整个查询表达式**（外层节点）。
    let Stmt::Select(s3) =
        parse("SELECT a FROM t UNION SELECT a FROM u ORDER BY 1 LIMIT 2").unwrap()
    else {
        panic!("不是 SELECT")
    };
    assert_eq!(s3.op, Some(SetOperation::Union));
    assert_eq!(s3.sort_clause.len(), 1);
    assert!(s3.limit_count.is_some());
}

#[test]
fn expression_precedence_follows_pg() {
    // OR < AND
    let Stmt::Select(s) = parse("SELECT a OR b AND c FROM t").unwrap() else {
        panic!("不是 SELECT")
    };
    let Expr::BoolExpr(b) = &s.target_list[0].val else {
        panic!("应为 BoolExpr")
    };
    assert_eq!(b.boolop, BoolExprType::Or);
    assert!(matches!(b.args[1], Expr::BoolExpr(ref inner) if inner.boolop == BoolExprType::And));

    // 左结合：1 - 2 - 3 ⇒ (1-2)-3
    let Stmt::Select(s2) = parse("SELECT 1 - 2 - 3 FROM t").unwrap() else {
        panic!("不是 SELECT")
    };
    let Expr::AExpr(a) = &s2.target_list[0].val else {
        panic!("应为 AExpr")
    };
    assert_eq!(a.name, "-");
    assert!(matches!(a.lexpr.as_deref(), Some(Expr::AExpr(_))));

    // 一元负号（UMINUS，右结合）紧于乘法：-a * b ⇒ (-a) * b
    let Stmt::Select(s3) = parse("SELECT -a * b FROM t").unwrap() else {
        panic!("不是 SELECT")
    };
    let Expr::AExpr(m) = &s3.target_list[0].val else {
        panic!("应为 AExpr")
    };
    assert_eq!(m.name, "*");
    let Expr::AExpr(neg) = m.lexpr.as_deref().expect("左操作数") else {
        panic!("应为一元 AExpr")
    };
    assert_eq!(neg.name, "-");
    assert!(neg.lexpr.is_none(), "一元形态：无左操作数（照 PG）");

    // **BETWEEN 比比较更紧**（PG 的声明顺序，反直觉但照抄）：
    // `a = 1 BETWEEN 2 AND 3` ⇒ a = (1 BETWEEN 2 AND 3)
    let Stmt::Select(s4) = parse("SELECT a = 1 BETWEEN 2 AND 3 FROM t").unwrap() else {
        panic!("不是 SELECT")
    };
    let Expr::AExpr(eq) = &s4.target_list[0].val else {
        panic!("应为 AExpr")
    };
    assert_eq!(eq.name, "=");
    assert!(matches!(
        eq.rexpr.as_deref(),
        Some(Expr::AExpr(inner)) if inner.kind == AExprKind::Between
    ));

    // NOT 是一元 BoolExpr，且 `NOT a IS NULL` ⇒ NOT (a IS NULL)（IS 比 NOT 紧）
    let Stmt::Select(s5) = parse("SELECT NOT a IS NULL FROM t").unwrap() else {
        panic!("不是 SELECT")
    };
    let Expr::BoolExpr(n) = &s5.target_list[0].val else {
        panic!("应为 BoolExpr")
    };
    assert_eq!(n.boolop, BoolExprType::Not);
    assert!(
        matches!(n.args[0], Expr::NullTest(_)),
        "NOT 作用在 IS NULL 之上"
    );
}

#[test]
fn in_between_nullif_follow_pg_node_shapes() {
    let Stmt::Select(s) = parse("SELECT a IN (1, 2) FROM t").unwrap() else {
        panic!("不是 SELECT")
    };
    let Expr::AExpr(ae) = &s.target_list[0].val else {
        panic!("应为 AExpr（照 PG，IN 不是独立节点）")
    };
    assert_eq!(ae.kind, AExprKind::In);
    assert_eq!(ae.name, "=", "照 PG：IN 的 name 是 =");
    assert_eq!(ae.rexpr_list.len(), 2, "值表在 rexpr_list");
    assert!(ae.rexpr.is_none());

    let Stmt::Select(s2) = parse("SELECT a BETWEEN 1 AND 9 FROM t").unwrap() else {
        panic!("不是 SELECT")
    };
    let Expr::AExpr(bt) = &s2.target_list[0].val else {
        panic!("应为 AExpr")
    };
    assert_eq!(bt.kind, AExprKind::Between);
    assert_eq!(bt.name, "BETWEEN");
    assert_eq!(
        bt.rexpr_list.len(),
        2,
        "上下界在 rexpr_list（照 PG 的二元列表）"
    );

    let Stmt::Select(s3) = parse("SELECT NULLIF(a, b) FROM t").unwrap() else {
        panic!("不是 SELECT")
    };
    let Expr::AExpr(nf) = &s3.target_list[0].val else {
        panic!("NULLIF 走 A_Expr（照 PG）")
    };
    assert_eq!(nf.kind, AExprKind::NullIf);
}

#[test]
fn literals_keep_pg_const_shapes() {
    let Stmt::Select(s) = parse("SELECT 1.50, 'a''b', NULL, TRUE, 1e3 FROM t").unwrap() else {
        panic!("不是 SELECT")
    };
    let vals: Vec<Option<ConstValue>> = s
        .target_list
        .iter()
        .map(|t| match &t.val {
            Expr::AConst(c) => c.value.clone(),
            other => panic!("不是常量：{other:?}"),
        })
        .collect();
    // 数字：**原文本**（`1.50` 不折叠；Float 含 `.`）
    assert_eq!(vals[0], Some(ConstValue::Float("1.50".to_owned())));
    assert_eq!(vals[1], Some(ConstValue::Str(b"a'b".to_vec())));
    assert_eq!(vals[2], None, "NULL ⇒ value = None（≡ PG 的 isnull）");
    assert_eq!(vals[3], Some(ConstValue::Bool(true)));
    assert_eq!(vals[4], Some(ConstValue::Float("1e3".to_owned())));
}

#[test]
fn insert_values_go_through_select_stmt_like_pg() {
    let Stmt::Insert(ins) = parse("INSERT INTO t (id, name) VALUES (1, 'a'), (2, 'b')").unwrap()
    else {
        panic!("不是 INSERT")
    };
    assert_eq!(ins.relation.relname, "t");
    assert_eq!(ins.cols.len(), 2);
    assert_eq!(ins.cols[0].name.as_deref(), Some("id"));
    let src = ins.select_stmt.as_deref().expect("来源");
    let lists = src
        .values_lists
        .as_ref()
        .expect("VALUES 走 values_lists（照 PG）");
    assert_eq!(lists.len(), 2);
    assert_eq!(lists[0].len(), 2);
}

#[test]
fn ddl_shapes_follow_pg_nodes() {
    let Stmt::CreateTable(ct) =
        parse("CREATE TABLE t (id NUMBER(10,2) NOT NULL, name BYTES) WITH (table_type = memory)")
            .unwrap()
    else {
        panic!("不是 CREATE TABLE")
    };
    assert_eq!(ct.relation.relname, "t");
    assert_eq!(ct.table_elts.len(), 2);
    assert_eq!(ct.table_elts[0].type_name.name, "number");
    assert_eq!(ct.table_elts[0].type_name.typmods, vec!["10", "2"]);
    assert!(ct.table_elts[0].is_not_null);
    assert_eq!(ct.options.len(), 1);
    assert_eq!(ct.options[0].defname, "table_type");

    let Stmt::Index(ix) = parse("CREATE UNIQUE INDEX ix ON g VERTEX (name)").unwrap() else {
        panic!("不是 CREATE INDEX")
    };
    assert!(ix.unique);
    assert_eq!(ix.relation.relname, "g");
    assert_eq!(ix.index_params[0].name.as_deref(), Some("name"));

    let Stmt::Index(jx) = parse("CREATE INDEX jx ON t (json_get(doc, 'a.b'))").unwrap() else {
        panic!("不是 CREATE INDEX")
    };
    assert!(
        jx.index_params[0].expr.is_some(),
        "表达式索引走 IndexElem.expr"
    );

    let Stmt::Drop(d) = parse("DROP INDEX ix").unwrap() else {
        panic!("不是 DROP")
    };
    assert_eq!(d.remove_type, ObjectType::Index);
    assert_eq!(d.objects[0].relname, "ix");
}

#[test]
fn type_cast_and_double_colon_share_the_node() {
    let a = parse("SELECT CAST(x AS NUMBER) FROM t").unwrap();
    let b = parse("SELECT x::NUMBER FROM t").unwrap();
    let get = |s: &Stmt| -> (String, String) {
        let Stmt::Select(sel) = s else {
            panic!("不是 SELECT")
        };
        let Expr::TypeCast(tc) = &sel.target_list[0].val else {
            panic!("应为 TypeCast")
        };
        let Expr::ColumnRef(arg) = tc.arg.as_ref() else {
            panic!("操作数应为列引用")
        };
        (format!("{:?}", arg.fields), tc.type_name.name.clone())
    };
    // 两处拼写的**结构相同**（位置随写法不同而不同——PG 亦然）。
    assert_eq!(get(&a), get(&b), "`CAST(x AS t)` 与 `x::t` 同节点（照 PG）");
}

#[test]
fn errors_carry_byte_spans() {
    let e = parse("SELECT * FROM t WHERE").unwrap_err();
    assert!(e.span.start >= 20, "错误位置落在语句内：{e:?}");
    let e2 = parse("SELECT * FROM t WHERE name LIKE 'a%'").unwrap_err();
    assert!(e2.message.contains("LIKE"), "指向不支持清单：{e2}");
    let e3 = parse("SELECT * FROM t RIGHT JOIN u ON 1 = 1").unwrap_err();
    assert!(e3.message.contains("RIGHT"), "{e3}");
    let e4 = parse("SELECT * FROM t ORDER BY id NULLS FIRST").unwrap_err();
    assert!(e4.message.contains("NULLS"), "{e4}");
}

#[test]
fn graph_table_is_loudly_deferred() {
    let e = parse("SELECT * FROM GRAPH_TABLE (g)").unwrap_err();
    assert!(e.message.contains("GRAPH_TABLE"), "{e}");
}

/// **依赖检查**（REQ-SQL-002 的验收原文）：AST 模块不 import 任何目录接口。
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
    let parser_src = include_str!("../src/parser.rs");
    for needle in ["bicdb_catalog", "catalog::"] {
        assert!(!parser_src.contains(needle), "解析期不得碰目录");
    }
}

#[test]
fn dcl_shapes_follow_the_frozen_design() {
    // ── W1：NAME 缺省 ⇒ None（"跟随属主名"由绑定/DDL 侧展开）──
    let Stmt::CreateWorkspace(cw) = parse("CREATE WORKSPACE FOR USER alice").unwrap() else {
        panic!("不是 CREATE WORKSPACE")
    };
    assert_eq!(cw.subject, "alice");
    assert!(cw.name.is_none(), "NAME 可缺省");
    let Stmt::CreateWorkspace(cw2) = parse("CREATE WORKSPACE FOR USER Alice NAME 'a1'").unwrap()
    else {
        panic!("不是 CREATE WORKSPACE")
    };
    assert_eq!(cw2.subject, "alice", "主体名是标识符 ⇒ 折叠照 PG");
    assert_eq!(cw2.name.as_deref(), Some(&b"a1"[..]));

    // ── WorkRef 双形态：id 与 名字[FOR USER]（解析只认形态）──
    let Stmt::AlterWorkspace(aw) = parse("ALTER WORKSPACE 7 SET NAME = NULL").unwrap() else {
        panic!("不是 ALTER WORKSPACE")
    };
    assert_eq!(aw.workspace.id, Some(7));
    assert!(aw.workspace.name.is_none() && aw.workspace.user.is_none());
    let Stmt::AlterWorkspace(aw2) = parse(
        "ALTER WORKSPACE 'prod' FOR USER alice SET QUOTA (data = 1, undo = 2, temp = 3, asset = 4)",
    )
    .unwrap() else {
        panic!("不是 ALTER WORKSPACE")
    };
    assert_eq!(aw2.workspace.name.as_deref(), Some(&b"prod"[..]));
    assert_eq!(aw2.workspace.user.as_deref(), Some("alice"));
    assert_eq!(aw2.workspace.id, None);
    let AlterWorkspaceAction::SetQuota(items) = &aw2.action else {
        panic!("不是配额")
    };
    assert_eq!(items.len(), 4);
    assert_eq!(items[0].defname, "data");

    // ── DROP WORKSPACE 走同一条引用位 ──
    let Stmt::Drop(d) = parse("DROP WORKSPACE 'prod' FOR USER alice, 9").unwrap() else {
        panic!("不是 DROP")
    };
    assert_eq!(d.remove_type, ObjectType::Workspace);
    assert!(d.objects.is_empty(), "工作区不走 RangeVar");
    assert_eq!(d.workspaces.len(), 2);
    assert_eq!(d.workspaces[0].name.as_deref(), Some(&b"prod"[..]));
    assert_eq!(d.workspaces[1].id, Some(9));

    // ── W2 克隆：两种源同一节点 ──
    let Stmt::AlterDatabase(ad) =
        parse("ALTER DATABASE CLONE WORKSPACE 'new' FROM WORKSPACE 7").unwrap()
    else {
        panic!("不是 ALTER DATABASE")
    };
    let AlterDatabaseAction::CloneWorkspace { name, source } = &ad.action else {
        panic!("不是克隆")
    };
    assert_eq!(name.as_slice(), b"new");
    assert!(matches!(source, WorkspaceSource::Workspace(w) if w.id == Some(7)));
    let Stmt::AlterDatabase(ad2) =
        parse("ALTER DATABASE CLONE WORKSPACE 'new' FROM TEMPLATE 'base'").unwrap()
    else {
        panic!("不是 ALTER DATABASE")
    };
    assert!(matches!(
        &ad2.action,
        AlterDatabaseAction::CloneWorkspace {
            source: WorkspaceSource::Template(t),
            ..
        } if t.as_slice() == b"base"
    ));

    // ── T1/T2/T3 ──
    let Stmt::AlterDatabase(t1) =
        parse("ALTER DATABASE ADD TEMPLATE 'b' FROM 'prod' FOR USER alice").unwrap()
    else {
        panic!("不是 ALTER DATABASE")
    };
    assert!(matches!(
        &t1.action,
        AlterDatabaseAction::AddTemplate { name, from }
            if name.as_slice() == b"b" && from.user.as_deref() == Some("alice")
    ));
    let Stmt::AlterDatabase(t2) =
        parse("ALTER DATABASE ALTER WORKSPACE 7 TO TEMPLATE 'b'").unwrap()
    else {
        panic!("不是 ALTER DATABASE")
    };
    assert!(matches!(
        &t2.action,
        AlterDatabaseAction::WorkspaceToTemplate { ws, name }
            if ws.id == Some(7) && name.as_slice() == b"b"
    ));
    let Stmt::AlterDatabase(t3) = parse("ALTER DATABASE DROP TEMPLATE 'b'").unwrap() else {
        panic!("不是 ALTER DATABASE")
    };
    assert!(matches!(
        &t3.action,
        AlterDatabaseAction::DropTemplate { name } if name.as_slice() == b"b"
    ));

    // ── F 组：三种动作 + FsRef 双形态 ──
    let Stmt::AlterSystem(f1) = parse("ALTER SYSTEM ADD FILESYSTEM '/mnt/d1'").unwrap() else {
        panic!("不是 ALTER SYSTEM")
    };
    assert!(matches!(
        &f1.action,
        AlterSystemAction::AddFilesystem { mount } if mount.as_slice() == b"/mnt/d1"
    ));
    let Stmt::AlterSystem(f2) =
        parse("ALTER SYSTEM ALTER FILESYSTEM '/mnt/d1' SET ALLOCATE = OFF").unwrap()
    else {
        panic!("不是 ALTER SYSTEM")
    };
    assert!(matches!(
        &f2.action,
        AlterSystemAction::AlterFilesystem { fs, allocate: false }
            if fs.mount.as_deref() == Some(&b"/mnt/d1"[..]) && fs.slot.is_none()
    ));
    let Stmt::AlterSystem(f3) = parse("ALTER SYSTEM DROP FILESYSTEM 3").unwrap() else {
        panic!("不是 ALTER SYSTEM")
    };
    assert!(matches!(
        &f3.action,
        AlterSystemAction::DropFilesystem { fs } if fs.slot == Some(3)
    ));

    // ── S 组：SET 带值 / CLEAR 无值（白名单在 ② 判——解析照收）──
    let Stmt::VariableSet(v1) = parse("ALTER SESSION SET work_area_size = '64MB'").unwrap() else {
        panic!("不是 ALTER SESSION")
    };
    assert_eq!(v1.kind, VariableSetKind::Set);
    assert_eq!(v1.name, "work_area_size");
    assert!(matches!(
        &v1.args[0].value,
        Some(ConstValue::Str(s)) if s.as_slice() == b"64MB"
    ));
    let Stmt::VariableSet(v2) = parse("ALTER SESSION SET max_query_memory = 4294967296").unwrap()
    else {
        panic!("不是 ALTER SESSION")
    };
    assert!(matches!(
        &v2.args[0].value,
        Some(ConstValue::Int(s)) if s == "4294967296"
    ));
    let Stmt::VariableSet(v3) = parse("ALTER SESSION CLEAR work_area_size").unwrap() else {
        panic!("不是 ALTER SESSION")
    };
    assert_eq!(v3.kind, VariableSetKind::Clear);
    assert!(v3.args.is_empty(), "CLEAR 无值");
}

#[test]
fn clone_of_is_gone_with_a_pointed_message() {
    let e = parse("CREATE WORKSPACE FOR USER alice CLONE OF 7").unwrap_err();
    assert!(e.message.contains("CLONE"), "{e}");
    assert!(
        e.message.contains("ALTER DATABASE"),
        "错误文案指向替代语句：{e}"
    );
    // 闭集外动作的文案同样响亮。
    let e2 = parse("ALTER SYSTEM SWITCH LOGFILE").unwrap_err();
    assert!(e2.message.contains("FILESYSTEM"), "{e2}");
    let e3 = parse("ALTER DATABASE RENAME TO x").unwrap_err();
    assert!(e3.message.contains("TEMPLATE"), "{e3}");
    let e4 = parse("ALTER WORKSPACE 7 SET QUOTA (foo = 1)").unwrap_err();
    assert!(e4.message.contains("foo"), "点出非法配额键：{e4}");
}
