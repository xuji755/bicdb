# GRAPH_TABLE 共享混合工作预算（2026-10-10）

## 问题

`max_edge_expansions` 已由同一 SQL 的多个 `GRAPH_TABLE` 来源共享，但原有 `max_expansions` 会在每个来源重新取得完整额度。两个来源各自声明相同 `BUDGETS` 时，可以把工作区上限放大为来源数倍；JOIN、UNION 和 INSERT SELECT 都能触发该问题。

## 实现

SQL 物化所有图表源时建立语句级 `remaining_work`，初值为工作区 `graph.max_expansions`。每个来源的有效额度为：

```text
min(来源 BUDGETS.max_expansions, statement.remaining_work)
```

来源成功后按图执行器实际 `QueryResult.expansions` 扣减。来源失败时整条 SQL 失败，局部余额随语句丢弃；后续 SQL 从工作区额度重新开始。请求级 BUDGETS 仍只能降低单个来源可用额度，不能重置或提高语句余额。

共享范围与既有边余额一致：同一物理计划的 JOIN/逗号连接、集合运算各分支及 INSERT SELECT。直接的独立 `CYPHER` 语句仍各自取得一个语句额度。

## 边界

此计数是图 Runner 的混合表达式/查询工作单位。原生 reader 的物理记录读取仍由访问路径自身预算和 PROFILE 指标约束；本切片没有把两种异构单位相加。实际持有内存和显式部分结果协议仍是独立后续项。

验证覆盖单来源恰好用满、两个来源不可各自重置、失败后下一条 SQL 重新取得额度，并回归原有跨来源边预算、集合运算和 INSERT SELECT 语句回滚。
