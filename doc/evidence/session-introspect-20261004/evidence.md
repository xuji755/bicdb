# 证据包：会话内省与清理管理面定案（PG `pg_stat_activity` / `pg_locks` / CancelRequest 参照）

模块：`spec/API.md` REQ-API-018 / `spec/OPS.md` §0.4 / `spec/SQL.md` 固定表清单
对应：**用户决策（2026-10-04）**　核验日期：2026-10-04　原始输出：`raw/01–02`（同目录）

## 结论

1. **会话活动视图有 PG 的完整样板**：`pg_stat_activity`（知识库有**系统视图字段表**，9.6–18 全版本）——每会话一行：当前查询、**`state`（active / idle / idle in transaction / idle in transaction (aborted)）**、`backend_xid`（当前事务）、`wait_event_type` / `wait_event`（等待事件）、时间戳四件套（backend_start / xact_start / query_start / state_change）、身份。→ 我们的 **`session$`** 列集照此收敛（状态取值同形）。
2. **锁清单有 `pg_locks` 的样板**（知识库有字段表）：锁目标类型（relation / page / **tuple** / transactionid / advisory…）、`mode`、**`granted`（持有 vs 等待）**、`pid`、关系、事务号。→ 我们的 **`lock$`**：**行锁按对象聚合**（计数 + 样例上限，照 tuple 锁粒度），加**阻塞链**。
3. **协议侧取消的三条现实**（PG 官方文档口径）：① `BackendKeyData('K')` 于连接建立时下发（PID + **秘密键**）；② 取消 = **另开连接**发 `CancelRequest`（码 80877102，键不符即忽略）；③ **取消是协作式的——可能完全无效、发起方不能直接知道结果**；`pg_cancel_backend`（温和，到检查点才生效，重节点可能迟迟不到）vs `pg_terminate_backend`（**终止会话**——事务中止、锁释放、清理异步；优先级高于取消）。
   → 我们取两件套（`session_cancel` / `session_terminate`），但**结果按既有纪律可判定**（`API` §0.4 验收），**且不设秘密键**（会话引用绑主体 + 取消只在已认证会话内发起——比 PG 的裸密钥更强）。
4. **分层归属（用户修正，已按此落文）**：会话信息 / 重发判定 / 清理决策**属 SQL / 会话侧，不属存储引擎**——存储只做读写的原子操作；**SQL 与存储同线程空间** ⇒ 这些是**进程内状态**，直接读取（PG 读共享内存；我们读进程内结构，更直接）。**存储层零改动。**

## 证据

| # | 来源类型 | 条目/文件 | 数据库 | 主题 | 核验日期 | 适用条件 |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | 知识库 SystemView（`bic_agent_SystemView`，管理版本 9.6–18） | `pg_stat_activity` | PostgreSQL | 每会话一行：`state` 取值、`backend_xid`、`wait_event_type` / `wait_event`、`backend_start` / `xact_start` / `query_start` / `state_change`、application_name 等（22 字段） | 2026-10-04 | **列集样板** |
| 2 | 知识库 SystemView（同上） | `pg_locks` | PostgreSQL | 锁目标类型（relation / page / tuple / transactionid / advisory…）、`mode`、`granted`（持有 vs 等待）、`fastpath` 等（16 字段） | 2026-10-04 | **锁清单样板** |
| 3 | 知识库 CaseStudy | plpython3u 调用外部 API 导致 CPU 饱和（pg_howto_23） | PostgreSQL | 靠 `pg_stat_activity` 识别"大量 active + 空闲事务升高"——**会话视图的诊断用途** | 2026-10-04 | 用途佐证 |
| 4 | 外部（PG 协议文档，经检索） | `BackendKeyData` / `CancelRequest`（80877102；键不符忽略；可能无效；不能直接知道结果） | PostgreSQL | 取消协议的现实语义 | 2026-10-04 | **对照项** |
| 5 | 外部（PG 管理函数，经检索） | `pg_cancel_backend`（SIGINT，协作式）/ `pg_terminate_backend`（SIGTERM，中止+释放+终止） | PostgreSQL | 管理面两件套语义 | 2026-10-04 | **两件套样板** |

## 检索范围声明

- 知识库（2026-10-04，2 轮 problem + 1 次 `doc_retrieve`）：精确命中 `pg_stat_activity` / `pg_locks` 两个 SystemView 实体（含全版本字段表）+ 1 条诊断案例。
- 外部检索（2026-10-04，1 轮）：PG 取消协议与 cancel / terminate 语义。摘要与链接 `raw/02`。

## 冲突与未决

| 项 | 各家说法 | 本项目 | 处置 |
| --- | --- | --- | --- |
| 取消可否判定 | PG：协作式、可能无效、不能直接知道 | 我们：取消后到达的响应仍带 `request_id` + `txn_status`——**可判定**（既有验收） | 已定（强于 PG） |
| 取消密钥 | PG：每连接 SecretKey | 我们：会话引用绑主体 + 取消在已认证会话内 | 已定（不设秘密键） |
| 跨区可见性 | PG：所有用户可见他人会话状态（查询文本受限） | 我们：用户通道只见本工作区；admin 经**管理面**跨区且脱敏 | 已定（REQ-API-018 / REQ-OPS-001） |

## 本项目自主决策（定案）

1. 固定表 **`session$` / `lock$`**（`$` 结尾保留命名，照 `file$` 形制；只读、无写入口）——**与预置对象 `session`（对话记录表）明确区分**。
2. 状态取值照 PG：执行中 / 空闲 / 空闲在事务 / 事务已中止；`lock$` 行锁按对象聚合 + 阻塞链。
3. 管理面两件套 `session_cancel`（温和）/ `session_terminate`（终止）——审计留痕；终止后的未确认请求终态为"已中止"（对账可见）。
4. 存储层零改动；同线程空间直接读进程内状态。

## 未核验项

- PG `pg_stat_activity` 的权限细节（`pg_read_all_stats` 等角色）——不需要（我们按工作区/管理面两通道）。
