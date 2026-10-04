# 证据包：幂等记录机制对照（Oracle Transaction Guard / MySQL XA / PG 2PC / 业界先例）

模块：`spec/API.md` REQ-API-006（`idem$`）　对应：**待冻结项 #60（讨论支撑材料）**
核验日期：2026-10-04　原始输出：`raw/01–03`（同目录）

## 结论

1. **Oracle 有同类"记录物件"（提交结果持久化 + 保留期 + 周期清理 + 可查询），
   但其用途是 Application Continuity / TAC 的切换重放安全阀，不是应用层通用重试协议**：
   **Transaction Guard**（12c+）——
   - **定位**：AC/TAC 的构件——切换（计划内 drain / switchover；计划外会话 / 网络 /
     节点 / 存储 / 实例 / 站点故障）后，应用要**重放**未完成事务；重放前必须先知道
     "上一笔提交没有"。TG 回答该问题：**已提交的绝不重放、未提交的才允许重放**
     （官方表述：TG 防止 AC 重放的事务被执行超过一次，at-most-once）。
     其提交结果判定支持的事务类型含 DML 与 **DDL/DCL**；与老式 TAF 互斥（TAF 下 LTXID 恒空）。
   - **机制**：提交时留 **LTXID**（逻辑事务标识）；**提交结果持久化到系统表**
     （`LTXID_HIST`，建在 `SYSAUX` 表空间）；应用可用 `GET_LTXID_OUTCOME` 查询
     "已提交 / 未提交"；保留期由服务参数控制（`COMMIT_OUTCOME=TRUE` +
     `RETENTION_TIMEOUT`），**默认 86400 秒 = 24 小时**（上限 2592000 秒 = 30 天）；
     过期记录由 MMON **每 60 分钟清理一次**。
   - 与 `idem$` 的关系：**相同的只是"记录物件"的构造**（提交结果落盘 + 保留期 +
     周期清理 + 按号查询——说明这类记录是成熟构造）；**用途不同**：TG 服务切换重放
     （驱动/系统管理 LTXID，只回答"交没交"），`idem$` 服务应用层重试去重（客户端给键、
     同键异内容拒绝、原样重放结果——Stripe / DynamoDB 类，见结论 4）。

2. **MySQL 无内置**：内核最近邻是 XA 两阶段提交——`XA PREPARE` 后事务悬挂，
   需 `XA RECOVER` 列出并**人工裁决**（`XA COMMIT`/`XA ROLLBACK`）；
   属跨资源协调设施，不是客户端重试去重。应用层自建（唯一键 +
   `INSERT IGNORE` / `ON DUPLICATE KEY`，或自建幂等表）。

3. **PostgreSQL 无内置**：2PC（`PREPARE TRANSACTION`）**默认禁用**
   （`max_prepared_transactions = 0`），官方明确"不建议应用与交互会话使用"
   （由外部事务管理器使用）。应用层自建（`ON CONFLICT` / 唯一键 / 自建表）。

4. **客户端键语义的业界先例（与 REQ-API-006 三条语义逐条对应）**：
   - **DynamoDB**：`TransactWriteItems` 的 `ClientRequestToken`——同令牌 + 不同参数 →
     `IdempotentParameterMismatch` 异常（= 同键异指纹**拒绝**）；令牌窗口 **10 分钟**，
     过期后视为新请求（= 保留期边界）；
   - **Stripe**：幂等键保存**第一次的响应**并在重试时**原样重放**
     （响应带 `Idempotency-Replayed` 标记）；参数不符报错；键**约 24 小时后清除**；
   - **MongoDB**：retryable writes——驱动（4.x+）**默认开启**，服务端按
     (`lsid`, `txnNumber`, `stmtIds`) 去重，网络错误自动重试一次不会重复执行。

5. **本项目定位**：`idem$` 的**记录物件**采 Oracle 同款构造（随事务落盘、保留期、
   定期清理、按号可查）；**用途与键语义**采 DynamoDB / Stripe（应用层重试去重：
   客户端提供键、同键异内容拒绝、重放原样结果——与 TG 的切换重放用途不同）；
   且为**按需**（仅带键写请求产生记录）——比 Stripe / MongoDB 等的默认全开更省。

## 证据

| # | 来源类型 | 条目/文件 | 数据库 | 主题 | 核验日期 | 适用条件 |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | 官方白皮书/文档（外部，经检索） | Transaction Guard 白皮书（oracle.com）；JDBC / Concepts / Admin / RAC 指南 | Oracle | **定位＝AC/TAC 切换重放安全阀**（已提交不重放、at-most-once；判定支持含 DDL/DCL 的事务类型）；`LTXID_HIST`（SYSAUX）；`RETENTION_TIMEOUT` 默认 86400s、上限 2592000s；MMON 每 60 分钟清理；`GET_LTXID_OUTCOME` | 2026-10-04 | 机制对照（**知识库未命中**；用途经 2026-10-04 修正） |
| 2 | 知识库 KnowledgeEntry（前缀 `4:8ed6c541-fe08-4ade-ae68-a79bf45316f4:`） | `:2073662` / `:2081650` / `:569486` / `:2082721` / `:2089047` | MySQL | XA 悬挂事务恢复路径、detach 后任意连接可提交、`XA RECOVER` 手工检查、TC_LOG 骨架 | 2026-10-04 | 支撑"无内置幂等键，最近邻为 XA 手工裁决" |
| 3 | 官方文档（外部，经检索） | PostgreSQL `max_prepared_transactions`（默认 0）与使用警告 | PostgreSQL | 2PC 默认禁用、不建议应用使用 | 2026-10-04 | 支撑"无内置" |
| 4 | 官方文档（外部，经检索） | DynamoDB `TransactWriteItems`；Stripe Idempotent requests；MongoDB Retryable Writes | 多 | 客户端键语义先例（窗口、参数不符拒绝、原样重放、默认开启） | 2026-10-04 | 语义对照 |

（`raw/01` = MySQL 知识库命中原文；`raw/02` = Oracle / PG 检索留痕；`raw/03` = 外部检索摘要与链接。）

## 检索范围声明

- 知识库（2026-10-04）：oracle 域 **0 命中**（`doc_retrieve` 0 匹配 + `kb_graph problem` 0 条——**未收录 Transaction Guard 条目**）；
  postgresql 域命中 6 条**均不相关**（`\watch` / CTE / SSI 等）；
  mysql 域命中 6 条，其中 **5 条为 XA 事务族**（1 条连接池案例，不相关）。
  **按召回预算有界，非全库穷尽**（留痕 `raw/02`）。
- 外部检索（2026-10-04，8 轮）：Oracle Transaction Guard（用途 / 机制 / AC-TAC 关系）、
  AWS DynamoDB、Stripe、MongoDB、PostgreSQL、MySQL XA。
  摘要与链接 `raw/03`。

## 冲突与未决

| 项 | 各家说法 | 本项目 | 处置 |
| --- | --- | --- | --- |
| 用途 | Oracle（TG）：AC/TAC 切换重放安全阀；DynamoDB / Stripe：应用层重试去重 | 应用层重试去重（Stripe / DynamoDB 类，非 TG 类） | 已明确（2026-10-04 修正） |
| 键由谁发 | Oracle：系统发 LTXID；DynamoDB / Stripe：客户端发 | 客户端发（要"同键异内容拒绝"与"重放原样结果"） | 已定向（随 #60 定稿） |
| 记录存哪 | Oracle：系统表；MongoDB：服务端会话状态；Stripe：自有存储 | 每工作区系统表（复用普通表机制，事务原子性免费） | 已定向 |
| 保留期数值 | Oracle 默认 24h（上限 30d）；Stripe ≥24h；DynamoDB 10min | 建议默认 24h | **待确认（#60）** |
| 清理节奏 | Oracle：每 60 分钟（MMON） | 建议并入实例级清理 worker（小时级） | **待确认（#60）** |

## 本项目自主决策（备定稿，待确认）

1. `idem$` 的**记录物件**照 Oracle 同款构造（同事务落盘 + 保留期 + 周期清理 + 按键查询）。
2. **用途与键语义**照 Stripe / DynamoDB（客户端键、同键异内容拒绝、重放原样结果——
   应用层重试去重，见结论 1 / 结论 4）。
3. 按需：仅带 `idempotency_key` 的写产生记录（优于全开形态的 Stripe / MongoDB）。
4. 保留期默认 24h、清理小时级（与 Oracle 默认对齐）——**数值随 #60 一并确认**。

## 未核验项

- Oracle `LTXID_HIST` 的内部结构（字节级）——不需要（我们复用普通表机制 + 事务原子性）。
- MySQL 第三方内核插件是否提供更贴近的机制——未展开（社区方案属应用层）。
