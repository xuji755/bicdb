//! **切片 S1 验收**：词法 / 语法 / Raw AST（**形状对齐 PostgreSQL**）。
//!
//! 钉住：语句闭集（REQ-SQL-005 正面清单）可解析；**清单外构造零接受**
//! （REQ-SQL-006）；错误带字节区间；**标识符折叠照 PG**（未引号小写、引号保留）；
//! **优先级照 PG**（含两个反直觉点：`BETWEEN`/`IN` 比比较更紧、集合运算有优先级）；
//! AST 不 import 目录接口（REQ-SQL-002 验收原文，源码自检）。

use bicdb_sql::ast::{
    AExprKind, AlterDatabaseAction, AlterUserAction, AlterWorkspaceAction, BoolExprType,
    ColumnRefField, ConstValue, Expr, FromItem, ObjectType, QuotaAmount, SetOperation, SortByDir,
    Stmt, TransactionStmtKind, VariableSetKind,
};
use bicdb_sql::parser::{parse, parse_many};

#[test]
fn graph_storage_rebuild_requires_complete_distinct_clause() {
    let Stmt::GraphIndex(stmt) = parse("ALTER GRAPH \"KG;知识\" REBUILD STORAGE").unwrap() else {
        panic!("graph storage rebuild")
    };
    assert_eq!(stmt.graph, "KG;知识");
    assert!(stmt.name.is_none());
    assert!(matches!(
        stmt.action,
        bicdb_sql::ast::GraphIndexAction::RebuildStorage
    ));
    for text in [
        "ALTER GRAPH kg REBUILD",
        "ALTER GRAPH kg REBUILD RECORDS",
        "ALTER GRAPH kg REBUILD STORAGE EXTRA",
        "ALTER GRAPH INDEX ix ON kg REBUILD STORAGE",
    ] {
        assert!(parse(text).is_err(), "{text}");
    }
}

#[test]
fn graph_storage_upgrade_requires_complete_explicit_clause() {
    let Stmt::GraphIndex(stmt) = parse("ALTER GRAPH \"KG;知识\" UPGRADE STORAGE").unwrap() else {
        panic!("graph upgrade")
    };
    assert_eq!(stmt.graph, "KG;知识");
    assert!(stmt.name.is_none());
    assert!(matches!(
        stmt.action,
        bicdb_sql::ast::GraphIndexAction::UpgradeStorage
    ));
    for text in [
        "ALTER GRAPH kg UPGRADE",
        "ALTER GRAPH kg UPGRADE RECORDS",
        "ALTER GRAPH INDEX ix ON kg UPGRADE STORAGE",
    ] {
        assert!(parse(text).is_err(), "{text}");
    }
}

#[test]
fn graph_data_template_requires_explicit_complete_clause() {
    for (text, data) in [
        ("ALTER DATABASE ADD TEMPLATE 't' FROM 7", false),
        (
            "ALTER DATABASE ADD TEMPLATE 't' FROM seed WITH GRAPH DATA",
            true,
        ),
    ] {
        let Stmt::AlterDatabase(stmt) = parse(text).unwrap() else {
            panic!("template")
        };
        assert!(
            matches!(stmt.action,AlterDatabaseAction::AddTemplate { graph_data,.. } if graph_data==data)
        );
    }
    for text in [
        "ALTER DATABASE ADD TEMPLATE 't' FROM seed WITH DATA",
        "ALTER DATABASE ADD TEMPLATE 't' FROM seed WITH GRAPH",
        "ALTER DATABASE ADD TEMPLATE 't' FROM seed WITH GRAPH ROWS",
        "ALTER DATABASE DROP TEMPLATE 't' WITH GRAPH DATA",
    ] {
        assert!(parse(text).is_err(), "{text}");
    }
}

#[test]
fn native_graph_index_grammar_preserves_case_and_rejects_ambiguous_targets() {
    use bicdb_sql::ast::GraphIndexAction;
    let Stmt::GraphIndex(index) = parse(
        "CREATE UNIQUE GRAPH INDEX ix ON kg NODES LABEL \"Entity\" (db_type, config.\"Port\")",
    )
    .unwrap() else {
        panic!("graph index");
    };
    assert_eq!(index.graph, "kg");
    assert_eq!(index.name.as_deref(), Some("ix"));
    assert_eq!(
        index.action,
        GraphIndexAction::Create {
            entity: "nodes".into(),
            label: Some("Entity".into()),
            fields: vec![vec!["db_type".into()], vec!["config".into(), "Port".into()]],
            unique: true
        }
    );
    for text in [
        "CREATE GRAPH INDEX ix ON kg RELATIONSHIPS TYPE \"LINK\" (weight)",
        "CREATE GRAPH INDEX ix ON kg NODES LABEL \"Entity\" ()",
        "SHOW GRAPH INDEXES ON kg",
        "DROP GRAPH INDEX ix ON kg",
        "ALTER GRAPH INDEX ix ON kg REBUILD",
    ] {
        assert!(
            matches!(parse(text).unwrap(), Stmt::GraphIndex(_)),
            "{text}"
        );
    }
    let Stmt::Cypher(query) =
        parse("PROFILE CYPHER kg 'RETURN $value AS v' PARAMETERS '{\"value\":1}'").unwrap()
    else {
        panic!("profile");
    };
    assert!(query.profile);
    for text in [
        "CREATE GRAPH INDEX ix ON kg NODES TYPE \"Entity\" (name)",
        "CREATE GRAPH INDEX ix ON kg RELATIONSHIPS LABEL \"LINK\" (weight)",
        "CREATE GRAPH INDEX ix ON kg (name)",
        "CREATE GRAPH INDEX ix ON kg NODES (name,)",
        "DROP GRAPH INDEX ix",
        "SHOW GRAPH INDEXES",
        "ALTER GRAPH INDEX ix ON kg",
        "PROFILE PROFILE CYPHER kg 'RETURN 1'",
    ] {
        assert!(parse(text).is_err(), "{text}");
    }
}

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
    // ── DCL（`DCL语句设计` v0.2 §1/§2；本库扩展组）──
    // F 组：三件套（名字是标识，路径只是创建参数）
    "CREATE FILESYSTEM data1 USING '/mnt/d1'",
    "CREATE FILESYSTEM 'data 1' USING '/mnt/d1'",
    "ALTER FILESYSTEM 'data1' SET ALLOCATE = OFF",
    "ALTER FILESYSTEM data1 SET ALLOCATE = OFF",
    "ALTER FILESYSTEM 3 SET ALLOCATE = ON",
    "DROP FILESYSTEM 3",
    "DROP FILESYSTEM 'data1'",
    "DROP FILESYSTEM data1",
    // W 组：无主容器 + DEFAULT FILESYSTEM / FROM TEMPLATE / 盘级配额
    "CREATE WORKSPACE prod",
    "CREATE WORKSPACE prod DEFAULT FILESYSTEM 'data1'",
    "CREATE WORKSPACE prod FROM TEMPLATE 'base'",
    "CREATE WORKSPACE prod QUOTA 1073741824 ON FILESYSTEM 'data1'",
    "CREATE WORKSPACE prod FROM TEMPLATE 'base' DEFAULT FILESYSTEM 3",
    "CREATE WORKSPACE prod QUOTA UNLIMITED ON FILESYSTEM 3 QUOTA 1024 ON FILESYSTEM 'data1'",
    "ALTER WORKSPACE 7 SET NAME = 'x'",
    "ALTER WORKSPACE 7 SET QUOTA (data = 1024, undo = 512, temp = 4096, asset = 2048)",
    "ALTER WORKSPACE prod ADD FILESYSTEM 3",
    "ALTER WORKSPACE 'prod' ADD FILESYSTEM 'data1' QUOTA 1024 ON FILESYSTEM 'data1'",
    "ALTER WORKSPACE 'prod' SET DEFAULT FILESYSTEM 'data1'",
    "ALTER WORKSPACE 7 TO TEMPLATE 'base'",
    "DROP WORKSPACE 7",
    "DROP WORKSPACE 'prod', 9",
    // U 组：绑定的落点（USING WORKSPACE 必选）
    "CREATE USER alice IDENTIFIED BY 's3cr3t' USING WORKSPACE 'prod'",
    "CREATE USER alice IDENTIFIED BY 's3cr3t' USING WORKSPACE 7",
    "CREATE USER alice IDENTIFIED BY 's3cr3t' USING WORKSPACE alice_ws",
    "ALTER USER alice IDENTIFIED BY 'new'",
    "ALTER USER alice IDENTIFIED BY 'new' EXPIRE",
    "ALTER USER alice IDENTIFIED BY 'new' REPLACE 'old'",
    "ALTER USER alice PAUSE",
    "ALTER USER alice RESUME",
    "ALTER USER alice USING WORKSPACE 'prod'",
    "ALTER USER alice DROP WORKSPACE 7",
    "DROP USER alice",
    "DROP USER alice CASCADE",
    // T 组：模板（克隆/原地固化各自只有一条路）
    "ALTER DATABASE ADD TEMPLATE 'base' FROM 7",
    "ALTER DATABASE ADD TEMPLATE 'base' FROM 'prod'",
    "ALTER DATABASE DROP TEMPLATE 'base'",
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
    // ── DCL 闭集外（`DCL语句设计` v0.2 §2：没有产生式就是没有；含 v0.2 删掉的老形式）──
    "CREATE WORKSPACE FOR USER alice",
    "CREATE WORKSPACE FOR USER alice CLONE OF 7",
    "SET work_area_size = '64MB'",
    "ALTER SYSTEM SET work_memory_target = '4GiB'",
    "ALTER SYSTEM SWITCH LOGFILE",
    "ALTER SYSTEM ADD FILESYSTEM '/mnt/d1'",
    "ALTER DATABASE RENAME TO x",
    "ALTER DATABASE CLONE WORKSPACE 'new' FROM TABLE t",
    "ALTER DATABASE ALTER WORKSPACE 7 TO TEMPLATE 'b'",
    "ALTER WORKSPACE 7 SET QUOTA (foo = 1)",
    "ALTER WORKSPACE 7 SET OWNER = 'x'",
    "ALTER WORKSPACE 7 SET NAME = NULL",
    "ALTER FILESYSTEM 3 SET ALLOCATE = MAYBE",
    "ALTER FILESYSTEM 3 SET ALLOCATE OFF",
    "CREATE FILESYSTEM d1 '/mnt/d1'",
    "CREATE FILESYSTEM d1 USING /mnt/d1",
    "CREATE USER alice IDENTIFIED BY 'x'",
    "ALTER USER alice IDENTIFIED BY 'x' REPLACE",
    "DROP USER",
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
fn graph_table_has_explicit_scalar_schema_and_non_lateral_arguments() {
    let Stmt::Select(s) = parse("SELECT g.code FROM GRAPH_TABLE(kg, :query PARAMETERS :opts COLUMNS (code NUMBER, title VARCHAR2(128), ok BOOLEAN)) AS g").unwrap() else { panic!("select") };
    let FromItem::GraphTable(g) = &s.from_clause[0] else {
        panic!("graph table")
    };
    assert_eq!(g.graph, "kg");
    assert_eq!(g.alias.as_ref().unwrap().aliasname, "g");
    assert_eq!(g.columns.len(), 3);
    assert_eq!(g.columns[1].type_name.typmods, vec!["128"]);
    assert!(matches!(g.query, Expr::ParamRef(_)));
    for sql in [
        "SELECT * FROM GRAPH_TABLE(kg)",
        "SELECT * FROM GRAPH_TABLE(kg, 'RETURN 1')",
        "SELECT * FROM GRAPH_TABLE(public.kg, 'RETURN 1' COLUMNS (v NUMBER))",
        "SELECT * FROM GRAPH_TABLE(kg, t.query COLUMNS (v NUMBER))",
        "SELECT * FROM GRAPH_TABLE(kg, 'RETURN 1' PARAMETERS NULL COLUMNS (v NUMBER))",
        "SELECT * FROM GRAPH_TABLE(kg, 'RETURN 1' COLUMNS ())",
        "SELECT * FROM GRAPH_TABLE(kg, 'RETURN 1' COLUMNS (v NUMBER,))",
        "SELECT * FROM GRAPH_TABLE(kg, 'RETURN 1' COLUMNS (v NUMBER NOT NULL))",
    ] {
        assert!(parse(sql).is_err(), "{sql}");
    }
}

#[test]
fn cypher_and_graph_table_accept_separate_request_budgets() {
    let Stmt::Cypher(c) =
        parse("PROFILE CYPHER kg 'RETURN $v' PARAMETERS '{\"v\":1}' BUDGETS '{\"max_rows\":1}'")
            .unwrap()
    else {
        panic!("cypher")
    };
    assert_eq!(c.budgets, "{\"max_rows\":1}");
    assert!(c.profile);
    let Stmt::Cypher(c) = parse("CYPHER kg 'RETURN 1' BUDGETS '{}'").unwrap() else {
        panic!("cypher")
    };
    assert_eq!(c.parameters, "{}");
    let Stmt::Select(s) =
        parse("SELECT * FROM GRAPH_TABLE(kg,:q PARAMETERS :p BUDGETS :b COLUMNS (v NUMBER))")
            .unwrap()
    else {
        panic!("select")
    };
    let FromItem::GraphTable(g) = &s.from_clause[0] else {
        panic!("graph table")
    };
    assert!(matches!(g.budgets, Some(Expr::ParamRef(_))));
    for sql in [
        "CYPHER kg 'RETURN 1' BUDGETS 1",
        "CYPHER kg 'RETURN 1' BUDGETS '{}' BUDGETS '{}'",
        "CYPHER kg 'RETURN 1' BUDGETS '{}' PARAMETERS '{}'",
        "SELECT * FROM GRAPH_TABLE(kg,'RETURN 1' BUDGETS t.value COLUMNS (v NUMBER))",
        "SELECT * FROM GRAPH_TABLE(kg,'RETURN 1' BUDGETS NULL COLUMNS (v NUMBER))",
    ] {
        assert!(parse(sql).is_err(), "{sql}");
    }
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
    // ── F 组三件套：`CREATE FILESYSTEM <名> USING '<路径>'`——**名字是标识，路径只是创建参数** ──
    let Stmt::CreateFilesystem(f1) = parse("CREATE FILESYSTEM data1 USING '/mnt/d1'").unwrap()
    else {
        panic!("不是 CREATE FILESYSTEM")
    };
    assert_eq!(f1.name, "data1", "名字（引用位）");
    assert_eq!(f1.path.as_slice(), b"/mnt/d1", "路径（创建参数）");
    // `名 := 标识符 | Str`（两者等价；字符串形态容纳带空格的名字）
    let Stmt::CreateFilesystem(f1b) = parse("CREATE FILESYSTEM 'data 1' USING '/mnt/d1'").unwrap()
    else {
        panic!("不是 CREATE FILESYSTEM")
    };
    assert_eq!(f1b.name, "data 1");
    let Stmt::AlterFilesystem(f2) = parse("ALTER FILESYSTEM 'data1' SET ALLOCATE = OFF").unwrap()
    else {
        panic!("不是 ALTER FILESYSTEM")
    };
    assert_eq!(f2.fs.name.as_deref(), Some(&b"data1"[..]), "引用位是名字");
    assert!(!f2.allocate);
    let Stmt::DropFilesystem(f3) = parse("DROP FILESYSTEM 3").unwrap() else {
        panic!("不是 DROP FILESYSTEM")
    };
    assert_eq!(f3.fs.slot, Some(3), "引用位也可以给池槽位");

    // ── W1：**无主容器** + 三个可选项（DEFAULT FILESYSTEM / FROM TEMPLATE / 盘级配额）──
    let Stmt::CreateWorkspace(cw) = parse("CREATE WORKSPACE prod").unwrap() else {
        panic!("不是 CREATE WORKSPACE")
    };
    assert_eq!(cw.name, "prod", "名字是标识符 ⇒ 折叠照 PG");
    assert!(
        cw.default_fs.is_none() && cw.from_template.is_none() && cw.quotas.is_empty(),
        "三个可选项都可缺省"
    );
    let Stmt::CreateWorkspace(cw2) = parse(
        "CREATE WORKSPACE Prod DEFAULT FILESYSTEM 'data1' FROM TEMPLATE 'base' \
         QUOTA 1073741824 ON FILESYSTEM 'data1' QUOTA UNLIMITED ON FILESYSTEM 3",
    )
    .unwrap() else {
        panic!("不是 CREATE WORKSPACE")
    };
    assert_eq!(cw2.name, "prod");
    assert_eq!(
        cw2.default_fs.as_ref().and_then(|f| f.name.as_deref()),
        Some(&b"data1"[..])
    );
    assert_eq!(cw2.from_template.as_deref(), Some(&b"base"[..]));
    assert_eq!(cw2.quotas.len(), 2, "盘级配额可多个");
    assert_eq!(cw2.quotas[0].amount, QuotaAmount::Bytes(1073741824));
    assert_eq!(cw2.quotas[1].amount, QuotaAmount::Unlimited);
    assert_eq!(cw2.quotas[1].fs.slot, Some(3));

    // ── WorkRef 双形态：id / 名字（**没有 `FOR USER`**——名字实例内唯一）──
    let Stmt::AlterWorkspace(aw) =
        parse("ALTER WORKSPACE 7 SET QUOTA (data = 1, undo = 2, temp = 3, asset = 4)").unwrap()
    else {
        panic!("不是 ALTER WORKSPACE")
    };
    assert_eq!(aw.workspace.id, Some(7));
    assert!(aw.workspace.name.is_none());
    let AlterWorkspaceAction::SetQuota(items) = &aw.action else {
        panic!("不是配额")
    };
    assert_eq!(items.len(), 4);
    assert_eq!(items[0].defname, "data");

    // ── W3/W4/W5/W7 ──
    let Stmt::AlterWorkspace(w3) =
        parse("ALTER WORKSPACE 'prod' ADD FILESYSTEM 3 QUOTA 1024 ON FILESYSTEM 3").unwrap()
    else {
        panic!("不是 ALTER WORKSPACE")
    };
    assert!(matches!(
        &w3.action,
        AlterWorkspaceAction::AddFilesystem { fs, quota: Some(q) }
            if fs.slot == Some(3) && q.amount == QuotaAmount::Bytes(1024)
    ));
    let Stmt::AlterWorkspace(w4) =
        parse("ALTER WORKSPACE 'prod' SET DEFAULT FILESYSTEM 'data1'").unwrap()
    else {
        panic!("不是 ALTER WORKSPACE")
    };
    assert!(matches!(
        &w4.action,
        AlterWorkspaceAction::SetDefaultFilesystem { fs }
            if fs.name.as_deref() == Some(&b"data1"[..])
    ));
    let Stmt::AlterWorkspace(w5) = parse("ALTER WORKSPACE 'prod' SET NAME = 'p2'").unwrap() else {
        panic!("不是 ALTER WORKSPACE")
    };
    assert!(matches!(&w5.action, AlterWorkspaceAction::SetName(n) if n.as_slice() == b"p2"));
    let Stmt::AlterWorkspace(w7) = parse("ALTER WORKSPACE 7 TO TEMPLATE 'base'").unwrap() else {
        panic!("不是 ALTER WORKSPACE")
    };
    assert!(matches!(
        &w7.action,
        AlterWorkspaceAction::ToTemplate { name } if name.as_slice() == b"base"
    ));
    // Runtime open modes are a closed, typed set. FORCE only modifies READ WRITE.
    let Stmt::AlterWorkspace(ro) = parse("ALTER WORKSPACE prod OPEN READ ONLY").unwrap() else {
        panic!("不是 ALTER WORKSPACE")
    };
    assert!(matches!(
        ro.action,
        AlterWorkspaceAction::Open(bicdb_sql::WorkspaceOpenMode::ReadOnly)
    ));
    let Stmt::AlterWorkspace(rw) = parse("ALTER WORKSPACE 7 OPEN READ WRITE").unwrap() else {
        panic!("不是 ALTER WORKSPACE")
    };
    assert!(matches!(
        rw.action,
        AlterWorkspaceAction::Open(bicdb_sql::WorkspaceOpenMode::ReadWrite)
    ));
    let Stmt::AlterWorkspace(force) =
        parse("ALTER WORKSPACE 'prod' OPEN READ WRITE FORCE").unwrap()
    else {
        panic!("不是 ALTER WORKSPACE")
    };
    assert!(matches!(
        force.action,
        AlterWorkspaceAction::Open(bicdb_sql::WorkspaceOpenMode::ReadWriteForce)
    ));
    for invalid in [
        "ALTER WORKSPACE prod OPEN",
        "ALTER WORKSPACE prod OPEN WRITE",
        "ALTER WORKSPACE prod OPEN READ",
        "ALTER WORKSPACE prod OPEN READ ONLY FORCE",
    ] {
        assert!(parse(invalid).is_err(), "必须拒绝：{invalid}");
    }
    let Stmt::AlterWorkspace(page_verify) =
        parse("ALTER WORKSPACE prod VERIFY RECOVERY PAGE 7 123").unwrap()
    else {
        panic!("不是 ALTER WORKSPACE")
    };
    assert!(matches!(
        page_verify.action,
        AlterWorkspaceAction::VerifyRecovery(bicdb_sql::RecoveryVerifyScope::Page {
            file_id: 7,
            block_id: 123
        })
    ));
    let Stmt::AlterWorkspace(object_verify) =
        parse("ALTER WORKSPACE 7 VERIFY RECOVERY OBJECT 4294967295").unwrap()
    else {
        panic!("不是 ALTER WORKSPACE")
    };
    assert!(matches!(
        object_verify.action,
        AlterWorkspaceAction::VerifyRecovery(bicdb_sql::RecoveryVerifyScope::Object {
            object_id: u32::MAX
        })
    ));
    for invalid in [
        "ALTER WORKSPACE prod VERIFY",
        "ALTER WORKSPACE prod VERIFY RECOVERY",
        "ALTER WORKSPACE prod VERIFY RECOVERY PAGE 7",
        "ALTER WORKSPACE prod VERIFY RECOVERY PAGE -1 2",
        "ALTER WORKSPACE prod VERIFY RECOVERY PAGE 65536 2",
        "ALTER WORKSPACE prod VERIFY RECOVERY OBJECT 4294967296",
        "ALTER WORKSPACE prod VERIFY RECOVERY WORKSPACE",
    ] {
        assert!(parse(invalid).is_err(), "必须拒绝：{invalid}");
    }

    // ── 引用位的两种写法等价：`名 := 标识符 | Str`（`prod` = `'prod'`）──
    let Stmt::AlterFilesystem(bare) = parse("ALTER FILESYSTEM data2 SET ALLOCATE = OFF").unwrap()
    else {
        panic!("不是 ALTER FILESYSTEM")
    };
    assert_eq!(
        bare.fs.name.as_deref(),
        Some(&b"data2"[..]),
        "裸标识符也能引用"
    );
    let Stmt::Drop(dbare) = parse("DROP WORKSPACE prod").unwrap() else {
        panic!("不是 DROP")
    };
    assert_eq!(dbare.workspaces[0].name.as_deref(), Some(&b"prod"[..]));

    // ── DROP WORKSPACE 走同一条引用位（可多个）──
    let Stmt::Drop(d) = parse("DROP WORKSPACE 'prod', 9").unwrap() else {
        panic!("不是 DROP")
    };
    assert_eq!(d.remove_type, ObjectType::Workspace);
    assert!(d.objects.is_empty(), "工作区不走 RangeVar");
    assert_eq!(d.workspaces.len(), 2);
    assert_eq!(d.workspaces[0].name.as_deref(), Some(&b"prod"[..]));
    assert_eq!(d.workspaces[1].id, Some(9));

    // ── U 组：**有了工作区才能建用户**（`USING WORKSPACE` 在产生式里是必选）──
    let Stmt::CreateUser(u1) =
        parse("CREATE USER alice IDENTIFIED BY 's3cr3t' USING WORKSPACE 'prod'").unwrap()
    else {
        panic!("不是 CREATE USER")
    };
    assert_eq!(u1.name, "alice");
    assert_eq!(u1.password.as_slice(), b"s3cr3t", "口令只在认证路径用");
    assert_eq!(u1.using_workspace.name.as_deref(), Some(&b"prod"[..]));
    let Stmt::AlterUser(u2) = parse("ALTER USER alice IDENTIFIED BY 'new' EXPIRE").unwrap() else {
        panic!("不是 ALTER USER")
    };
    assert!(matches!(
        &u2.action,
        AlterUserAction::SetPassword { new, expire: true } if new.as_slice() == b"new"
    ));
    let Stmt::AlterUser(u3) = parse("ALTER USER alice IDENTIFIED BY 'new' REPLACE 'old'").unwrap()
    else {
        panic!("不是 ALTER USER")
    };
    assert!(matches!(
        &u3.action,
        AlterUserAction::ReplacePassword { new, old }
            if new.as_slice() == b"new" && old.as_slice() == b"old"
    ));
    let Stmt::AlterUser(u4) = parse("ALTER USER alice PAUSE").unwrap() else {
        panic!("不是 ALTER USER")
    };
    assert_eq!(u4.action, AlterUserAction::SetPaused(true));
    let Stmt::AlterUser(u4b) = parse("ALTER USER alice RESUME").unwrap() else {
        panic!("不是 ALTER USER")
    };
    assert_eq!(u4b.action, AlterUserAction::SetPaused(false));
    let Stmt::AlterUser(u5) = parse("ALTER USER alice USING WORKSPACE 7").unwrap() else {
        panic!("不是 ALTER USER")
    };
    assert!(matches!(&u5.action, AlterUserAction::UsingWorkspace(w) if w.id == Some(7)));
    let Stmt::AlterUser(u6) = parse("ALTER USER alice DROP WORKSPACE 'prod'").unwrap() else {
        panic!("不是 ALTER USER")
    };
    assert!(matches!(
        &u6.action,
        AlterUserAction::DropWorkspace(w) if w.name.as_deref() == Some(&b"prod"[..])
    ));
    let Stmt::DropUser(u7) = parse("DROP USER alice").unwrap() else {
        panic!("不是 DROP USER")
    };
    assert!(!u7.cascade, "默认不连工作区一起删");
    let Stmt::DropUser(u7b) = parse("DROP USER Alice CASCADE").unwrap() else {
        panic!("不是 DROP USER")
    };
    assert_eq!(u7b.name, "alice", "主体名折叠照 PG");
    assert!(u7b.cascade);

    // ── T 组：只剩两条（克隆是 W2、原地固化是 W7——一件事不设两个入口）──
    let Stmt::AlterDatabase(t1) = parse("ALTER DATABASE ADD TEMPLATE 'b' FROM 7").unwrap() else {
        panic!("不是 ALTER DATABASE")
    };
    assert!(matches!(
        &t1.action,
        AlterDatabaseAction::AddTemplate { name, from, graph_data: false }
            if name.as_slice() == b"b" && from.id == Some(7)
    ));
    let Stmt::AlterDatabase(t2) = parse("ALTER DATABASE DROP TEMPLATE 'b'").unwrap() else {
        panic!("不是 ALTER DATABASE")
    };
    assert!(matches!(
        &t2.action,
        AlterDatabaseAction::DropTemplate { name } if name.as_slice() == b"b"
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
fn removed_forms_are_rejected_with_pointed_messages() {
    // v0.2 删掉 `CREATE WORKSPACE FOR USER`：名字位上来的是关键字 ⇒ 点名"标识符或字符串"。
    let e = parse("CREATE WORKSPACE FOR USER alice").unwrap_err();
    assert!(
        e.message.contains("标识符") || e.message.contains("名字"),
        "{e}"
    );
    // 旧的 `ALTER SYSTEM …FILESYSTEM` 三件套 ⇒ 指向 `CREATE/ALTER/DROP FILESYSTEM`。
    let e = parse("ALTER SYSTEM ADD FILESYSTEM '/mnt/d1'").unwrap_err();
    assert!(
        e.message.contains("CREATE / ALTER / DROP FILESYSTEM"),
        "{e}"
    );
    let e2 = parse("ALTER SYSTEM SWITCH LOGFILE").unwrap_err();
    assert!(e2.message.contains("FILESYSTEM"), "{e2}");
    // 克隆与原地固化在 `ALTER DATABASE` 下的两条老路 ⇒ 各自指向 W2 / W7。
    let e = parse("ALTER DATABASE CLONE WORKSPACE 'new' FROM 7").unwrap_err();
    assert!(e.message.contains("CREATE WORKSPACE"), "{e}");
    let e = parse("ALTER DATABASE ALTER WORKSPACE 7 TO TEMPLATE 'b'").unwrap_err();
    assert!(e.message.contains("ALTER WORKSPACE"), "{e}");
    let e3 = parse("ALTER DATABASE RENAME TO x").unwrap_err();
    assert!(e3.message.contains("TEMPLATE"), "{e3}");
    // `FOR USER` 限定没了：`SET` 位置上来的是 `FOR` ⇒ 点出期望。
    let e = parse("ALTER WORKSPACE 'prod' FOR USER alice SET NAME = 'p2'").unwrap_err();
    assert!(e.message.contains("SET"), "{e}");
    // `SET NAME = NULL` 不再提供（v0.2 的 BNF 只有 `Eq Str`）。
    let e = parse("ALTER WORKSPACE 7 SET NAME = NULL").unwrap_err();
    assert!(
        e.message.contains("标识符") || e.message.contains("名字"),
        "{e}"
    );
    // 闭集外动作的文案同样响亮。
    let e4 = parse("ALTER WORKSPACE 7 SET QUOTA (foo = 1)").unwrap_err();
    assert!(e4.message.contains("foo"), "点出非法配额键：{e4}");
    // `CREATE USER` 缺 `USING WORKSPACE` ⇒ 点出必选。
    let e5 = parse("CREATE USER alice IDENTIFIED BY 'x'").unwrap_err();
    assert!(e5.message.contains("USING"), "{e5}");
    // 配额只能是整数或 `UNLIMITED`。
    let e6 = parse("CREATE WORKSPACE p QUOTA '10G' ON FILESYSTEM 3").unwrap_err();
    assert!(e6.message.contains("UNLIMITED"), "{e6}");
}

#[test]
fn graph_access_rebuild_has_distinct_syntax_and_rejects_unknown_maintenance() {
    let statements = parse_many("ALTER GRAPH kbg REBUILD ACCESS;").unwrap();
    assert!(
        matches!(&statements[0],bicdb_sql::ast::Stmt::GraphIndex(s) if s.name.is_none() && matches!(s.action,bicdb_sql::ast::GraphIndexAction::RebuildAccess))
    );
    for sql in [
        "ALTER GRAPH kbg REBUILD",
        "ALTER GRAPH kbg REBUILD UNKNOWN",
        "ALTER GRAPH INDEX ix ON kbg REBUILD ACCESS",
    ] {
        assert!(parse_many(sql).is_err(), "{sql}");
    }
}

#[test]
fn fulltext_graph_sql_has_separate_targets_fixed_json_paths_and_closed_maintenance() {
    use bicdb_sql::ast::{GraphIndexAction, GraphTextPathPart};
    let Stmt::GraphIndex(index)=parse("CREATE FULLTEXT GRAPH INDEX ix ON kg NODES LABEL (\"Entity\",\"Fault\") (name,config.answers[0].\"Title\") OPTIONS '{\"update\":\"manual\"}'").unwrap() else {panic!("fulltext")};
    let GraphIndexAction::FulltextCreate {
        labels,
        fields,
        options,
        ..
    } = index.action
    else {
        panic!("create")
    };
    assert_eq!(labels, ["Entity", "Fault"]);
    assert_eq!(
        fields[1],
        vec![
            GraphTextPathPart::Key("config".into()),
            GraphTextPathPart::Key("answers".into()),
            GraphTextPathPart::Index(0),
            GraphTextPathPart::Key("Title".into())
        ]
    );
    assert_eq!(options, "{\"update\":\"manual\"}");
    let Stmt::GraphIndex(sync) = parse("ALTER FULLTEXT GRAPH INDEX ix ON kg SYNC").unwrap() else {
        panic!("sync");
    };
    let Stmt::GraphIndex(rebuild) = parse("ALTER FULLTEXT GRAPH INDEX ix ON kg REBUILD").unwrap()
    else {
        panic!("rebuild");
    };
    assert!(matches!(sync.action, GraphIndexAction::FulltextSync));
    assert!(matches!(rebuild.action, GraphIndexAction::FulltextRebuild));
    for sql in [
        "CREATE FULLTEXT GRAPH INDEX ix ON kg RELATIONSHIPS TYPE \"CAUSE\" (config.answer)",
        "SEARCH FULLTEXT GRAPH INDEX ix ON kg FOR 'buffer pool' OPTIONS '{\"db_type\":\"d1\"}'",
        "SHOW FULLTEXT GRAPH INDEXES ON kg",
        "ALTER FULLTEXT GRAPH INDEX ix ON kg SYNC",
        "ALTER FULLTEXT GRAPH INDEX ix ON kg WAIT",
        "ALTER FULLTEXT GRAPH INDEX ix ON kg WAIT OPTIONS '{\"timeout_ms\":0,\"target_source_seq\":3}'",
        "ALTER FULLTEXT GRAPH INDEX ix ON kg REBUILD",
        "ALTER FULLTEXT GRAPH INDEX ix ON kg PAUSE",
        "ALTER FULLTEXT GRAPH INDEX ix ON kg RESUME",
        "ALTER FULLTEXT GRAPH INDEX ix ON kg OPTIONS '{\"update\":\"batch\",\"interval_ms\":5000}'",
        "DROP FULLTEXT GRAPH INDEX ix ON kg",
    ] {
        assert!(matches!(parse(sql).unwrap(), Stmt::GraphIndex(_)), "{sql}");
    }
    for sql in [
        "CREATE FULLTEXT INDEX ix ON kg (name)",
        "CREATE FULLTEXT GRAPH INDEX ix ON kg NODES ()",
        "CREATE FULLTEXT GRAPH INDEX ix ON kg NODES (config[1.5])",
        "CREATE FULLTEXT GRAPH INDEX ix ON kg NODES (config[-1])",
        "CREATE FULLTEXT GRAPH INDEX ix ON kg RELATIONSHIPS LABEL x (name)",
        "ALTER FULLTEXT GRAPH INDEX ix ON kg UNKNOWN",
        "SEARCH FULLTEXT GRAPH INDEX ix ON kg 'buffer'",
        "SHOW FULLTEXT GRAPH INDEXES",
        "SELECT config[0] FROM t",
    ] {
        assert!(parse(sql).is_err(), "{sql}");
    }
}

#[test]
fn graph_fulltext_wait_options_are_optional_and_closed() {
    use bicdb_sql::ast::{GraphIndexAction, Stmt};
    let Stmt::GraphIndex(wait) = parse("ALTER FULLTEXT GRAPH INDEX ix ON kg WAIT").unwrap() else {
        panic!("wait")
    };
    assert_eq!(
        wait.action,
        GraphIndexAction::FulltextWait {
            options: "{}".into()
        }
    );
    for sql in [
        "ALTER FULLTEXT GRAPH INDEX ix ON kg WAIT 10",
        "ALTER FULLTEXT GRAPH INDEX ix ON kg WAIT OPTIONS",
        "ALTER FULLTEXT GRAPH INDEX ix ON kg WAIT OPTIONS 10",
        "ALTER FULLTEXT GRAPH INDEX ix ON kg WAIT OPTIONS '{}' SYNC",
    ] {
        assert!(parse(sql).is_err(), "{sql}");
    }
}
