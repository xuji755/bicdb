# GRAPH_TABLE 图元素句柄（2026-10-10）

## 目标与边界

`GRAPH_TABLE` 现在可以把 Cypher 返回的节点或关系映射为专用的 `GRAPH_ELEMENT`（别名 `ELEMENT`）列。句柄只表达身份，不复制标签、属性或端点快照。普通持久表仍不能声明该类型；持久引用需要另行定义删除、跨图和重建生命周期，不能由瞬态查询值代替。

```sql
SELECT a.name, b.name
FROM GRAPH_TABLE(
  kg, 'MATCH (n) RETURN n,n.name'
  COLUMNS (entity GRAPH_ELEMENT, name VARCHAR2(64))
) a
JOIN GRAPH_TABLE(
  kg, 'MATCH (n) RETURN n,n.name'
  COLUMNS (entity GRAPH_ELEMENT, name VARCHAR2(64))
) b ON a.entity=b.entity;
```

## 身份与比较规则

Cypher 内部元素仍是 `Node(u64)` / `Edge(u64)`。进入 SQL 后编码为：

```text
(graph_obj u32, kind node|edge, local_id u64)
```

逻辑规范中的 `(kind,id)` 由所在命名图解释；SQL 值把 `graph_obj` 一并带上，避免两个图都存在节点 1 时错误 JOIN。三个字段共同参与相等、排序、哈希、`DISTINCT`、聚合键和外部溢写。节点 1 与边 1 不相等，不同图的节点 1 也不相等。NULL 继续遵循 SQL 三值逻辑。

执行器瞬态/溢写编码固定为 13 字节大端：`graph_obj(4) + kind(1) + id(8)`。网络值标记为 `g`，载荷为 ASCII `graph_obj:n|e:id`；结果列 `kind` 同为 `g`。Rust 客户端返回 `Value::GraphElement`，Python 驱动返回不可变 `bicdb.GraphElement`。

## 类型纪律

- `GRAPH_ELEMENT` 只接受 Cypher 节点或关系，路径、列表和对象仍拒绝。
- 标量列不会隐式升级或降级为句柄；需要数值或文本身份时使用 `id()` / `elementId()`。
- 句柄不能作为 Cypher 参数，避免把某个 SQL 图作用域的身份注入另一图。
- 直接 `CYPHER` 的节点/边输出继续沿用原有 JSON 展示，兼容现有调用方；专用类型只由显式 `GRAPH_TABLE COLUMNS` 请求。
- 普通 `CREATE TABLE` 类型集合没有 `GRAPH_ELEMENT`，因此不能形成无生命周期约束的持久悬空引用。

## 验证

验证覆盖绑定、同图自连接、跨图同 ID 隔离、节点/边同 ID 隔离、哈希和外部溢写、协议往返、Rust 客户端桥接及 Python 实机查询。证据入口见 `doc/evidence/bicdb-graph-element-20261010/README.md`。
