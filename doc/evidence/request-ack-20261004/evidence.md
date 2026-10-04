# 证据包：请求确认与对账协议定案（PgQ 与 PG `pg_xact_status` 参照）

模块：`spec/API.md` REQ-API-014（新增）/ `spec/TXN.md` REQ-TXN-011（改写）/ REQ-API-006、007（撤销）
对应：**待冻结项 #60（撤销）/ #63（成稿待复核）**　核验日期：2026-10-04　原始输出：`raw/01–03`（同目录）

## 结论

1. **"提交与否的判定"有 PG 的现成样板**：**`pg_xact_status`（9.6 起为 `txid_status(bigint)`；PG 13 更名）**——
   按事务号点查提交状态（`committed` / `aborted` / `in progress` / `NULL`），
   **官方场景即"连接在 COMMIT 期间断开"**（定位为两阶段提交的轻量替代，来自 Craig Ringer 的
   "Transaction traceability" 补丁链）。状态存于**提交日志**（CLOG / `pg_xact`，每事务 2 bit）；
   查阅持有 `ClogTruncationLock` 与截断互斥（防竞态）；
   **随清理截断——"太旧 → NULL"**。客户端须自行取号（`txid_current()`），PG 不会自动把 xid 给客户端。
2. **"确认与窗口"的投递骨架有 PgQ（SkyTools）**：确认驱动（处理完成才 ACK、未确认会重投）；
   **"工作与确认同事务"是效果至多一次的诀窍**（同库：业务与 `finish_batch` 同事务提交；
   跨库：处理事务里记录已处理 ID、重投时跳过）；未确认窗口按确认序号裁剪；失败即回滚重投；
   超过重试上限进死信。
3. **本项目据此定案**（`spec/API.md` REQ-API-014）：请求序号（服务端分配、回执必带；客户端维护**已确认序号**）；
   终态四值（已提交 / 未提交〔可安全重发〕/ 在途 / 不可判定）；断连**中止回滚**、已提交不可撤销；
   重连 `sync`（按已确认序号批量）+ `status`（单点，PG 样板）；终态记录**随提交路径持久化**
   （PgQ"工作与确认同事务"的对应物）、按**已确认序号 + 时间/容量上限**裁剪；
   **超出窗口 → 不可判定**（= PG "too old → NULL"）；读请求不参与对账；语义级重复由数据自然键自理。
   **结果回放**：窗口内尽力（内存）、超窗只回终态（与 PG 只回状态同规）。
4. **随之撤销**：REQ-API-006（`idempotency_key` / `idem$`）与 REQ-API-007（`expected_revision`——
   并发冲突改由事务机制承担：RC + 行级排他锁串行化，等待 + 末位胜；需要"拒绝"语义用 SQL 条件写）。
   **`asset$` 的 `idempotency_key` 列为普通唯一约束**（`REQ-AST-005`），不受影响。

## 证据

| # | 来源类型 | 条目/文件 | 数据库 | 主题 | 核验日期 | 适用条件 |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | 知识库 KnowledgeEntry（前缀 `4:8ed6c541-fe08-4ade-ae68-a79bf45316f4:`） | `:1262055` / `:490359` / `:4153880` / `:2299005` / `:1270978` | PostgreSQL | CLOG = 每事务 2 bit 的提交状态位图（`pg_xact` 目录）。用于可见性判定；从库在 CLOG 复制中断时产生"存疑事务" | 2026-10-04 | 机制背景 |
| 2 | 知识库官方文档（源码学习系列） | `PG18 事务管理器深度学习(01)：事务日志(clog)与提交时序核心算法`（`doc_retrieve` 命中，锚点 `clog.c` / `transam.c` / `xact.c` 等） | PostgreSQL | 提交状态写入/读取链路、组提交（`TransactionGroupUpdateXidStatus`）、WAL 与 clog 双写一致性 | 2026-10-04 | 源码级背景 |
| 3 | 外部（PG 补丁链/文档，经检索） | "Transaction traceability – `txid_status(bigint)`"（Craig Ringer）；`pg_xact_status` 文档 | PostgreSQL | **点查语义**（committed / aborted / in progress / NULL）；**"COMMIT 期间断连"官方场景**；CLOG oldest-XID 跟踪与 `ClogTruncationLock`（查询与截断互斥）；两阶段提交的轻量替代 | 2026-10-04 | **直接样板** |
| 4 | 外部（SkyTools / PgCon 2009 / PgQue 文档，经检索） | PgQ：`next_batch` → `get_batch_events` → `finish_batch`；`pgq_ext`（已处理记录） | 多 | 确认驱动投递；**"工作与确认同事务"**；水位裁剪与表轮转回收；失败回滚重投；`batch_retry` / `event_retry` 与死信 | 2026-10-04 | **骨架参照** |
| 5 | 知识库 FaultModel | `…:1290311`（Rollback 未知故障） | PostgreSQL | "客户端发送 rollback 请求后未收到 ACK 响应，无法确定回滚是否执行成功" | 2026-10-04 | 问题留痕 |

## 检索范围声明

- 知识库（2026-10-04，2 轮 + 1 次 `doc_retrieve`）：postgresql 域命中 **CLOG 族条目与
  "PG18 事务管理器深度学习"源码学习文档**（上表 1/2）；
  **未命中 `pg_xact_status` / `txid_status` 条目本身**（召回有界，留痕 `raw/01`）。
  PgQ **未收录**（"PgQ"检索命中的是 DuckPGQ 案例，不相关）。
- 外部检索（2026-10-04，4 轮）：PG `pg_xact_status` 语义与 CLOG 截断；PgQ 机制与确认协议。
  摘要与链接 `raw/02–03`。

## 冲突与未决

| 项 | 各家说法 | 本项目 | 处置 |
| --- | --- | --- | --- |
| 查询键 | PG：事务号（客户端须自己取 `txid_current()`）；DynamoDB：客户端令牌 | **已确认序号**（批量对账）+ `request_id` / 提交序号（单点） | 已定（REQ-API-014） |
| 保留窗口 | PG：随 CLOG 截断（动态、无固定秒数）；DynamoDB：10 分钟 | 已确认序号 + 时间/容量上限（建议初始 24 小时起，P0 冻结） | **数值待冻结** |
| 结果回放 | PG：只回状态；PgQ：载荷随重投 | 窗口内尽力回放（内存）、超窗只回终态 | 已定 |
| 客户端取号 | PG 不自动给 xid、需自行 `txid_current()` | 服务端分配序号、**回执必带**（少一次往返） | 已定（改进点） |

## 本项目自主决策（#63 正文，待复核）

1. 终态四值；断连**中止回滚**（PgQ 同款"失败即回滚、重投"）；同序号重发兜底**不重复执行**。
2. 终态记录**随提交路径持久化**、按已确认序号 + 上限裁剪；**超窗不可判定**（PG "too old" 同款）。
3. 读不参与对账；语义级重复不在协议范围（由数据自然键自理）。
4. 撤销 REQ-API-006 / 007；`asset$` 唯一约束列保留；`idem$`、协议幂等键字段不再存在。

## 未核验项

- CLOG 的 oldest-XID 跟踪与截断协议的字节级细节——不需要（我们按"已确认序号 + 上限"自定窗口）。
- PgQ 的 ticker / 批次调度与队列表轮转——不需要（只取确认驱动、同事务、裁剪三原则）。
