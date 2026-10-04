# 外部检索摘要：PgQ（SkyTools）机制（2026-10-04，2 轮 WebSearch）

## 1. PgQ 是什么

- SkyTools 的 PostgreSQL 队列（Skype / Marko Kreen；PL/pgSQL + Python + C；`pgqd` ticker daemon；
  ISC 许可；Londiste 建立其上；现代打包 = `pgq/pgq` 扩展 / `PgQue` 纯 SQL 再实现）。

## 2. 与本次设计相关的四条机制

1. **确认驱动投递**：消费者循环 `next_batch(queue, consumer)` → `get_batch_events(batch_id)` →
   处理 → **`finish_batch(batch_id)`（ACK）** → COMMIT。**未确认的批次会被重投**（至少一次）。
   若消费者处理失败 → 事务回滚 → 批次重投。
2. **"工作与确认同事务" = 效果至多一次的诀窍**：同库处理时，业务工作与 `finish_batch` 在
   **同一事务**提交——提交覆盖两者，"已确认 ⟺ 已做"，无中间态；
   跨库时才需要"在处理事务里记录已处理 ID（batch_id / event_id）、重投时跳过"（`pgq_ext` 做法）。
3. **未确认窗口按确认水位裁剪**：ack 推进消费者游标（位点模型，非删除消息）；表轮转 +
   无未处理引用后 TRUNCATE 回收；**停滞的消费者阻塞回收而不是丢数据**。
4. **重试与死信**：`pgq.batch_retry(batch_id, retry_seconds)` / `pgq.event_retry(...)` 延迟重投；
   ticker 负责调度；`max_retries` 上限之后进死信（DLQ）。

## 3. 明确不抄的部分（对象级差异）

- ticker / tick 批次调度、3 表轮转、下发式消费模型——**不需要**；
  我们只取三条原则：**确认驱动、同事务、按确认序号裁剪**。
- PgQ 的投递语义是"至少一次 + 消费方记录去重"；我们把它对偶成
  **"请求必回执 + 服务端终态记录 → 重发至多执行一次"**。

链接（检索结果）：
- https://beta.pgcon.org/2009/schedule/attachments/91_pgq.pdf （PgQ, PgCon 2009 讲义）
- https://github.com/NikolayS/PgQue/blob/main/docs/pgq-concepts.md （PgQ 概念文档）
- https://www.percona.com/blog/pgq-and-pgque-workflow-engines-you-might-need-in-postgresql/

## 4. 知识库对照

PgQ **未收录**（`doc_retrieve 'PgQ'` 命中的是 DuckPGQ，不相关，见 raw/01 §3）。
