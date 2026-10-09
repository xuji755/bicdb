# Cypher 全文过程与索引访问（2026-10-09）

Cypher 已接入原生命名全文索引，节点与关系可以从全文结果开始继续匹配、过滤、遍历和投影。复用现有 SQL 全文的文档、倒排、域限制、源修订校验和 strict/eventual 语义。本轮没有连接或修改 Neo4j。

## 查询接口

```sql
CYPHER kg 'CALL db.index.fulltext.queryNodes(
  "words","buffer pool",{db_type:"mariadb",consistency:"eventual"}
) YIELD node AS n,score,field,snippet,complete
WHERE score>0
MATCH (n)-[:CAUSE]->(m)
RETURN n.name,m.name,score,field,snippet,complete
ORDER BY score DESC';

CYPHER kg 'CALL db.index.fulltext.queryRelationships(
  "links","buffer",{db_type:"mariadb",consistency:"eventual"}
) YIELD relationship AS r,score
RETURN type(r),startNode(r).name,endNode(r).name,score';
```

两个过程均要求三个参数：索引名称 STRING、查询 STRING、选项 MAP。db_type 必须显式给出，不能从用户、图或记忆猜测。只接受当前图拥有且实体种类匹配的全文索引；未知名称、别的图的索引、将关系索引用于节点过程均报错。索引名字符串按目录实际名称匹配。

选项与 SQL SEARCH 一致：db_type、fields、labels、channel、consistency、limit。labels 仅供节点检索，按 OR 过滤且先于 limit。fields 为已索引路径名称列表；channel 为 terms/phrase/exact；consistency 默认 strict，也可指定 eventual；limit 为 0–10000 的整数。未知键或类型错误拒绝。标点、语言分词、固定 JSON 路径和评分仍由 bic_mixed_v1 与自有 BM25 内核定义，不支持 Lucene 查询语言，也不承诺其分数兼容。

YIELD 必须显式列出结果，支持 AS 别名。节点列为 node，关系列为 relationship；共同列为 rank、score、field、snippet、byte_offset、line_number、mode、generation、covered_commit_seq、complete、covered_source_seq、source_seq。未知列、错误实体列、重复别名和覆盖已有变量在静态检查时报错。rank 为本次调用排序后的 1 起位置；YIELD WHERE 过滤后不会重新编号。后续 RETURN、ORDER BY、LIMIT、WITH、MATCH、OPTIONAL MATCH、相关 CALL 和 UNION 沿用现有有界 Cypher 语义。

```sql
CYPHER kg 'UNWIND $terms AS term
CALL (term) {
  CALL db.index.fulltext.queryNodes($index,term,$options)
  YIELD node,score WHERE score>0
  RETURN node.name AS name,score
}
RETURN term,name,score'
PARAMETERS '{"terms":["buffer","IO"],"index":"words","options":{"db_type":"mariadb","consistency":"eventual"}}';
```

域限制应用于被索引实体，在候选排名和 limit 之前执行。后续图遍历使用普通 MATCH 条件，需要节点或边的额外域条件时显式写 WHERE/属性模式；db_type 是检索域选择，不替代工作区访问控制。

## 一致性和语句内写入

只读 eventual 查询使用原生倒排候选并复核当前源版本，不返回已经修改、删除或变域的旧实体；尚未维护的新内容可能缺失。模式为 FULLTEXT_BTREE，complete/source 水位与 SQL SEARCH 完全相同。strict 查询从当前语句可见源建立有界完整视图，包括同一用户事务尚未提交的源变化，模式为 strict_scan。

若当前 Cypher 语句已经 CREATE/SET/REMOVE/DELETE，再调用全文过程，会使用当前暂存图构建 strict 视图。即便请求 eventual，也不能用语句之前的倒排冒充当前源。这种结果标记 strict_staged_scan，complete=true 表示当前暂存源视图；提交/源水位均为 NULL，避免把尚未发布的变化标成持久覆盖。generation 表示本次使用的已发布索引基线版本。

```sql
CYPHER kg 'MATCH (n:Entity {name:"old",db_type:"mariadb"})
SET n.name="fresh"
CALL db.index.fulltext.queryNodes("words","fresh",{db_type:"mariadb",consistency:"eventual"})
YIELD node,mode,complete,covered_source_seq
RETURN node.name,mode,complete,covered_source_seq';
```

Cypher 成功后仍只按原有事务提交源与队列，不将临时分析发布为全文 generation。查询后续失败则不提交暂存图。显式用户事务仍按 COMMIT/ROLLBACK 工作；PUBLIC 具名用户能读全文过程，含写入的 Cypher 仍被拒绝。

## 存储访问与预算

图内核的 IndexProvider 增加命名全文端口，默认明确拒绝未提供的能力，不将未知索引偷偷降级为扫描。原生按需 GraphAccess 和旧图/写查询的完整图路径均接入同一 SQL 全文实现。命中 ID 在图执行器中再次验证范围、重复、实体存在及请求域，并装载节点或关系，使后续表达式和图匹配使用当前实体。

相关调用共用语句工作预算。每次元数据/调用检查、原生倒排项和全文源检查均计入，strict 每次完整源扫描也计入；结果为空、YIELD WHERE false 或 limit=0 不允许绕过源读取预算。超限明确报错，不返回部分成功结果，含写入语句不会因此提交部分暂存变化。

PROFILE 保留原有列，并追加 fulltext_source_checks；对应过程的访问记录显示真实模式、索引名称、调用次数、倒排项数、命中数及源检查数。graph_nodes_loaded/graph_edges_loaded 表示执行器保留的实体缓存，不能单独代表 strict 构建和修订校验期间的全部读取；源检查列用于区分这些开销。冷热全文语料装载仍不是仅按命中读取。

## 验证和剩余工作

覆盖节点/关系、片段与路径、分域、SQL/过程评分一致、YIELD WHERE、相关子查询、UNION、参数、PROFILE、未知索引和跨图拒绝、语句内暂存修改与删除、用户事务回滚、失败后不提交源、重启及 PUBLIC 只读身份。

后续已为 bicdb.searchNodes 增加显式 indexes 和多索引轮转融合，见[图多索引排名融合](bicdb_图多索引排名融合_20261009.md)。未指定索引时仍走 bounded_scan；随后已完成[自动索引选择](bicdb_图全文自动索引选择_20261009.md)；固定目标 WAIT、GRAPH_TABLE、图模板、受管引用和退休段回收仍待实现。

证据见 [Cypher 全文验证](evidence/bicdb-graph-fulltext-cypher-20261009/README.md)，维护说明见 [定时维护](bicdb_图全文定时批量维护与参数_20261009.md)。

后续已实现[固定目标 WAIT](bicdb_图全文固定目标WAIT_20261009.md)，其覆盖与超时语义见对应说明。
