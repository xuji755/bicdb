# 外部检索摘要（2026-10-04，6 轮，经 WebSearch）
# 用途：支撑 evidence.md 的第 1 / 3 / 4 条结论。以下为检索结果摘要与本轮取用的关键数字。

## 1. Oracle Transaction Guard（LTXID / 保留期 / 清理）

关键事实（Oracle 官方文档与白皮书口径）：

- 机制：提交时保存 **LTXID**（逻辑事务标识）；**提交结果持久化**在事务历史表
  （`LTXID_HIST` / `LTXID_TRANS`），**默认建在 `SYSAUX` 表空间**。
- 查询：`DBMS_APP_CONT.GET_LTXID_OUTCOME` 回答"已提交 / 未提交 / 用户调用是否完整"。
- 保留期：服务参数 `RETENTION_TIMEOUT` 与 `COMMIT_OUTCOME=TRUE` 配合；
  **默认 86400 秒（24 小时）**；最新文档口径**上限 2592000 秒（30 天）**；
  12c 白皮书提示可按需更短或更长（可超一周）；专利示例给出 OLTP 建议 3–6 小时、批处理 24 小时。
- 清理：MMON 定期清过期记录——**单实例每 60 分钟一次**，跨实例每 12 小时一次。
- 过期后：结果删除，`GET_LTXID_OUTCOME` 不再能解析。
- 场景原文（专利）："if the commit statement is not returned to the client
  (for foreground or instance or network message…)"——即覆盖网络丢应答的消歧。

链接：
- https://www.oracle.com/technetwork/database/database-cloud/private/transaction-guard-wp-12c-1966209.pdf
- https://docs.oracle.com/en/database/oracle/oracle-database/26/adfns/database-development-guide.pdf
- （RAC 指南：LTXID_HIST 建于 SYSAUX；专利 EP2928160B1 摘录见检索结果）

## 2. AWS DynamoDB TransactWriteItems / ClientRequestToken

关键事实：

- 提供 `ClientRequestToken` 使调用幂等：多次相同调用 = 一次效果。
- **令牌在首次请求完成后 10 分钟内有效**；超窗后同令牌请求**视为新请求**。
- 窗口内同令牌但参数变化 → **`IdempotentParameterMismatch` 异常**。
- 后续调用响应与首次可能不同（计费口径变化），但副作用只有一次。
- AWS SDK 会自动填充该字段。

链接：
- https://awscli.amazonaws.com/v2/documentation/api/2.8.7/reference/dynamodb/transact-write-items.html
- https://docs.aws.amazon.com/botocore/latest/reference/services/dynamodb/client/transact_write_items.html

## 3. Stripe Idempotent requests

关键事实：

- 幂等键（客户端生成，≤255 字符，建议 UUID 级别高熵；不应含敏感数据）。
- **保存第一次请求的状态码与响应体**；后续同键请求**原样重放**该响应，
  并在响应中带 `Idempotency-Replayed` 标记。
- 参数与首次不符 → `idempotency_error`。
- 键**至少在 24 小时后**才可能被清除；清除后同键重发 = 新请求（重新执行）。
- 校验失败/并发冲突等"未开始执行"的请求不保存结果，可直接重试。
- 仅对 POST 有意义（GET/DELETE 本身幂等，不应带键）。

链接：
- https://docs.stripe.com/api/idempotent_requests

## 4. MongoDB Retryable Writes

关键事实：

- 框架：服务器会话（server sessions，3.6+）；命令携带 `lsid` + `txnNumber`，
  语句数组内用 `stmtIds` 标识每条写。
- 重试时驱动携带**相同** (`lsid`, `txnNumber`) 重发；**服务端按会话事务号去重**，
  识别为重复投递则不重复执行。
- **驱动 4.x（对应 4.2+ 服务器）默认开启**（`retryWrites=true`）；
  要求副本集/分片，单机不支持；需确认写关注（w:0 不可重试）。
- 只对单文档写与事务提交/中止自动重试（一次）；多文档操作需应用自理。
- 说明：底层动机即"网络错误时客户端无法知道写是否已应用"。

链接：
- https://www.mongodb.com/docs/manual/core/retryable-writes/
- https://www.mongodb.com/ja-jp/docs/v8.0/reference/server-sessions/

## 5. PostgreSQL 2PC / max_prepared_transactions（默认禁用）

关键事实：

- `PREPARE TRANSACTION 'id'` 提供两阶段提交；`max_prepared_transactions` 控制并发 prepared 上限。
- **默认值 0 = 完全禁用**（历史从 50 降 5 再降至 0；用于防"遗忘的 prepared 事务"引发维护事故，
  甚至 anti-wraparound 停机）。
- 官方建议：非有意使用 2PC 时应保持 0；启用时值应至少 ≥ `max_connections`；
  **prepared transaction 一般不建议应用或交互会话使用**——应由外部事务管理器管理。

链接：
- https://pgpedia.info/m/max_prepared_transactions.html
- https://www.highgo.ca/2020/01/28/understanding-prepared-transactions-and-handling-the-orphans/

## 6. MySQL XA（无客户端幂等键；最近邻机制为手工裁决）

关键事实：

- `XA RECOVER` 列出所有 PREPARED 状态 XA 事务（任意客户端可查，需 `XA_RECOVER_ADMIN`）。
- 提交结果未知时正确恢复 = **重试提交**（已成功的提交不可回滚）；再次裁决已解决的事务
  会报 `ER_XAER_NOTA`（未知 XID）。
- 8.2+ `xa_detach_on_prepare` 默认开：prepared 事务可从会话脱挂、由任意连接提交；
  prepared 状态跨重启持久。
- 崩溃在 PREPARE 与 COMMIT 之间的边界上存在 binlog 一致性告警（官方已知限制）。
- 应用侧幂等 = `INSERT IGNORE` / 唯一键 / outbox 等自建模式。

链接：
- （官方 RefMan 8.x PDF 与 Percona XA 材料，见检索结果；
  MySQL 内部机制的知识库条目见 raw/01）
