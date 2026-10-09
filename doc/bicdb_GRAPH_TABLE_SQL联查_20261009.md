# GRAPH_TABLE：图查询与 SQL 联查（2026-10-09）

## 实现范围与语法

图匹配仍由 Rust 图模块执行，结果通过 SQL 内存游标进入现有关系算子。只在当前连接的工作区解析图名，不访问 Neo4j，也不建立结果副本表。

```sql
SELECT g.name, o.owner
FROM GRAPH_TABLE(
  kg, 'MATCH (n:Entity) WHERE n.db_type=$domain RETURN n.code,n.name'
  PARAMETERS '{"domain":"2123"}'
  COLUMNS (code NUMBER, name VARCHAR2(128))
) AS g
LEFT JOIN owners AS o ON g.code=o.code
WHERE g.code>0
ORDER BY g.code;
```

文法：

```text
GRAPH_TABLE(graph_name, query_text
  [PARAMETERS json_object_text]
  COLUMNS (column_name output_type [, ...])) [AS] alias
```

query_text 与 json_object_text 接受字符串字面量或 SQL 命名参数。未指定 PARAMETERS 等于空 JSON 对象。图参数仍使用 Cypher `$name`，SQL 参数使用 `:name`。

```sql
SELECT g.code FROM GRAPH_TABLE(
  kg, :query PARAMETERS :parameters COLUMNS (code NUMBER)
) g WHERE g.code=:expected;
```

COLUMNS 按 RETURN **位置**映射，并为外层 SQL 起列名，不要求与 Cypher 别名相同。声明列数必须与 RETURN 列数相同，包括空结果。列类型不依赖实际返回的行，空结果和全 NULL 列保持 NUMBER / BOOL / BYTES 协议类型。

支持 SELECT 的 INNER/LEFT JOIN、逗号连接、WHERE、GROUP BY、HAVING、DISTINCT、ORDER BY、LIMIT/OFFSET、集合运算及 INSERT SELECT。沿用 SQL 当前最多两表联查的限制，不支持 LATERAL 或关联 SQL 行输入。图中的 CALL 子查询和全文过程仍可使用。

## 类型、权限与事务

- NUMBER/INT/INTEGER：只收 Cypher 精确数字，不转浮点、不隐式解析字符串。不接受精度/小数位修饰。
- BOOL/BOOLEAN：只收布尔，不接受类型修饰。
- VARCHAR/VARCHAR2/TEXT/CHAR/BYTES：只收 Cypher 字符串；最大长度按 UTF-8 字节校验，1..65535。默认 CHAR 为 1，其余为 255。没有 CHAR 补空格或数字/JSON 隐式转换。
- GRAPH_ELEMENT/ELEMENT：只收 Cypher 节点或关系，返回 `(graph_obj, kind, id)` 瞬态身份句柄；不包含属性快照。可参与 SQL 相等、排序、哈希、JOIN、DISTINCT 和溢写。不同图、不同元素种类不会因局部 ID 相同而相等。
- 所有列可为 NULL；声明列名必须唯一，最多 256 列。
- 路径、列表、对象仍明确拒绝，不能自动编码为属性 JSON 快照。需要标量身份时仍可使用 id() / elementId()。GRAPH_ELEMENT 不能声明为普通持久表列，也不能作为 Cypher 参数；持久受管引用尚未实施。

绑定字面量时检查只读，执行参数查询时重新解析并检查只读，均在图执行前拒绝 CREATE/MERGE/SET/REMOVE/DELETE。外层 SELECT 不能借动态 Cypher 参数写图。当前用户在 PUBLIC 可以读其公开图，写操作仍禁止；图名不接受工作区或 schema 限定，不能跨工作区 JOIN。

图源读与关系扫描使用同一语句快照，活动事务内读取本人未提交变更。INSERT SELECT 先物化图源，再使用既有关系写入、唯一性检查、语句回滚与事务回滚；读取图源本身不会创建事务或修改图。绑定捕获实际 GRAPH 对象版本，计划把它标记为无段内存行源，避免误将图的存储索引当作关系索引。

查询及 JSON 参数各最多 1 MiB。每个图源沿用图模块默认行数/扩展/深度限制；同一 SQL 语句的图源（包含集合运算各分支）编码物化数据共享 100 MiB 逻辑预算。超过列长度、行编码或物化预算即报错，不截断。该预算不是整个进程 RSS 上限；本版物化游标还存在执行期复制开销，不宣称流式执行或大型知识图谱性能。

## 验证与部署

验证日志放在 `doc/evidence/bicdb-graph-table-20261009/`，覆盖解析/绑定、实际服务联查、动态参数写入拒绝、精确数值、多字节长度、空结果类型、事务与工作区范围，并回归既有图及全文接口。正式发布通过 `python3 scripts/build_bicbot_v2.py`，核对构建、安装、回执及测试二进制哈希。

所有新增测试只在隔离的 bicdb 实例内创建数据。本阶段没有连接或修改 Neo4j，没有改动 source 原始方案，没有 Git 提交或推送。
