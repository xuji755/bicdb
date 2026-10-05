# 外部参照材料（refs）

本目录存放**用户提供的外部参照材料**——它们是本库若干设计取舍的**外部依据**，
但**不是冻结设计**：结论一律以 `doc/arch/`（及镜像 `docs/storage/`）与
`doc/evidence/` 的核对记录为准；与参照材料冲突时，以本库文档为准并开 ADR/发现条目。

| 材料 | 内容 | 用途 | 对照记录 |
| --- | --- | --- | --- |
| `Oracle_vs_PG_Latch_LWLock.md` | Oracle latch（`kslgetl`/`kslfre`，不排队/唤醒≠授权/强夺）vs PG spinlock（TAS_SPIN + 指数退避）与 LWLock（FIFO + handoff + `RELEASE_OK`）；`V$LATCH` 判读口径 | §5.10 闩锁形态、退避形状、自旋预算、统计判读（O3 触发条件） | 证据包 `doc/evidence/latch-ab-20261005/`（含本机 A/B 实测） |
| `Oracle_vs_PostgreSQL_事务_锁_UndoRedo_机制剖析.html` | 事务标识/生命周期/2PC、MVCC 与 CR、Undo vs VACUUM、WAL 与 FPI、行锁与 enqueue、死锁检测（含 PG 硬边/软边）、崩溃恢复、隔离级别、并发场景推演 | 事务/锁/undo-redo 算法的正确性核对与优化取材 | 证据包 `doc/evidence/oracle-pg-txn-lock-20261005/`（逐条对照表） |

**发布口径**：`doc/` 是**内部目录**（不随公开发布镜像走）——参照材料仅存于内部仓库。
