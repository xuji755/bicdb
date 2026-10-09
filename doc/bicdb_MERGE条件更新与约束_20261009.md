# MERGE 条件更新与约束（2026-10-09）

本阶段补齐可重复图数据导入需要的 `ON CREATE SET`、`ON MATCH SET`，并完善完整路径及无向关系的 MERGE 行为。运行时仍只使用 bicdb 原生图、索引、事务和变更队列，不连接 Neo4j。

语义参考 [Neo4j 官方 MERGE 文档](https://neo4j.com/docs/cypher-manual/current/clauses/merge/)：查找完整模式；未找到时创建；条件动作依是否创建执行；属性值不能为 null；无向关系先匹配两种方向，创建时从左到右。bicdb 只实现下述有界子集，不宣称完整 Cypher 25 或 Neo4j 并发锁兼容。

## 语法和导入示例

```sql
CREATE GRAPH kg;
CREATE UNIQUE GRAPH INDEX node_key ON kg NODES LABEL "Entity" (db_type,identity);
CREATE FULLTEXT GRAPH INDEX words ON kg NODES LABEL "Entity" (name)
    OPTIONS '{"update":"manual"}';

CYPHER kg 'UNWIND $items AS item
 MERGE (n:Entity {db_type:$domain,identity:item.key})
 ON CREATE SET n.name=item.name,n.hits=0,n:Imported
 ON MATCH SET n.name=item.name,n.hits=n.hits+1
 RETURN n.identity AS identity,n.name AS name'
 PARAMETERS '{"domain":"d1","items":[{"key":1,"name":"firstword"},{"key":1,"name":"freshword"}]}';

ALTER FULLTEXT GRAPH INDEX words ON kg WAIT;
```

一条 MERGE 必须是一个完整路径模式。逗号分隔多个模式被明确拒绝，应拆成多个 MERGE 子句。两种 ON 动作可以按任意顺序给出；同类动作出现多次时按该分支中出现的顺序执行。动作使用当前 SET 子集：实体属性赋值和节点标签增加；不新增 SET 整图属性替换、`+=` 或动态标签语法。

MERGE、ON 动作后仍能接普通 SET、WITH、RETURN、命名全文过程以及已有子查询/UNION。CALL 子查询内支持同样语法，变量必须显式导入。

## 匹配、创建与更新

- 每条输入行先匹配完整模式，保留全部完整匹配及独立关系身份；匹配分支只执行 ON MATCH。
- 完整匹配为空时创建完整路径，只执行 ON CREATE。未绑定节点不因已有部分模式而被自动复用；需要复用时，在前面的 MATCH 或 MERGE 中绑定节点。
- 前面已绑定且满足标签/属性的节点可以在新路径中复用；不满足声明模式时报错，不修改它或另建一个同名变量的节点。
- 无向 MERGE 查找正向和反向关系；没有完整匹配时，从模式左侧创建到右侧。自环和多个平行匹配分别保留既有身份。CREATE 的方向要求不变。
- 在 UNWIND 多行导入中，后续输入能够看到前一输入创建或更新后的暂存图。第一次实际变更后使用暂存图扫描，避免持久索引尚未发布而遗漏新实体。

MERGE 模式的属性表达式只允许引用该子句之前已绑定的变量，不支持引用本 MERGE 新声明的节点/边来计算模式属性。模式固定属性为 null 时拒绝；动态参数、表达式或缺失属性在执行到该输入时为 null，也拒绝整条语句。bicdb 的结构化 JSON 属性仍保留原有边界，并未改为 Neo4j 属性类型系统。

## 校验、事务与索引

两种动作的参数、函数、变量作用域、SET 目标及已知实体类型在执行前校验，即使某分支未执行或前面的 MATCH 没有结果，也不能隐藏不存在的变量或非法接口。聚合不能作为 MERGE 模式属性或条件 SET 的值。非活动分支不会求值，例如已匹配节点的 `ON CREATE SET n.x=1/0` 不会执行除法。

动作使用原有属性/标签更新入口，计入表达式和行预算。查询在完整暂存副本执行：任何输入、动作、完整匹配扩展或预算失败，都不发布本语句的部分结果。

原生 SQL 发布前检查最终图的所有既有属性索引约束，节点和关系唯一键均适用。部分匹配后创建与唯一键冲突、ON MATCH 修改成重复键等均失败，图记录、属性/访问树和全文源队列不保留本语句的更改。外层显式事务中更早的成功语句继续保留；ROLLBACK 和崩溃恢复撤销未提交内容。耐久 ID 预留允许失败后留空号，仍不会复用。

上述约束是 bicdb 当前暂存结果发布检查，不新增 Neo4j 的并发端点锁或每算子即时约束算法；当前服务本身仍是单写者、单工作区连接模型。写路径仍完整加载/校验图，不应把有索引的 MERGE 宣称为大型图低成本增量导入。

条件动作产生的真实实体变化进入既有共享队列，不分析全文；匹配且未实际改变实体，不追加实体事件。同语句内动作之后的全文过程使用严格暂存视图；提交后 eventual 查询继续复核修订，批量 WAIT 再发布新全文结果。

MERGE 始终分类为写操作，即使实际只命中现有节点、ON 分支没有执行。PUBLIC 的普通用户、只读 PROFILE 和 GRAPH_TABLE 不能借条件动作调用写路径。

## 验证及剩余边界

核心测试覆盖重复输入、全部匹配、平行关系、完整/部分路径、绑定端点、无向/自环、NULL、非法非活动动作、行/表达式预算、CALL/UNION 和参数。

真实服务测试覆盖：条件导入的属性树与全文队列、暂存全文视图、实体 ID 与重启；节点/关系唯一键冲突和动作失败不污染记录/队列；外层事务先前成功内容、ROLLBACK；仅终止隔离测试实例的实际崩溃后恢复；PUBLIC、PROFILE、GRAPH_TABLE 只读限制。证据见 [验证记录](evidence/bicdb-graph-merge-20261009/README.md)。

尚未完成的整体图目标仍包括专用邻接段、受管引用与删除传播、图元素句柄、带数据快照、旧段回收、全文统计定点读取和大型性能验收。本阶段不修改 Neo4j 数据或既有用户数据库内容。
