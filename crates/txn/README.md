# bicdb-txn

事务状态机、语句快照、行锁、Undo、死锁检测

| 项 | 值 |
| --- | --- |
| 设计依据 | §8 崩溃恢复契约（§4.6.6 事务生命周期、§11.1.1 提交流程、§11.1.2 undo 受 redo 保护） |
| 对应阶段 | P3（已启动）／P4（行锁、死锁检测） |
| 当前状态 | **v0.6**——`write`（写路径：`begin`/`insert`/`delete`/`update`/`commit`/`rollback` **经缓冲池**；改长更新 ⇒ 行迁移）；行锁、死锁检测、**语句回滚点与等待-重试驱动** |

## 状态说明

- **写路径**（`write`）：DML 只改缓存里的页（写先入缓存）——
  `begin`（槽分配也经池）→ **占用 ITL 条目**（`occupy_itl`：先**延迟块清除**
  已提交旧条目 → 至多一个活动条目/事务/块 → 新占用必记 `ITL 覆盖`（带
  `txn_id` 归属）→ 写 `Active` 条目）→ 改页 → 记撤销记录 → 页差异 redo →
  `page_lsn` → 标脏；`commit` = 提交记录入流 → **等 LGWR 刷到它（提交点）**
  → 事务表槽置已提交（此步失败不回滚——由恢复前滚补标记兜底）；
  `rollback` = 沿链补偿（逐条经池、带 redo）→ 释放槽。
- **ITL 归属**：行头 `itl_slot` 由写路径**回填**（调用方给的字节会被改写）；
  撤销记录一律可失败预检先行（不在链上留幽灵记录）。
- **三条纪律**：
  · **单闩锁纪律**：持 `PageGuard` 时不得再调池（N=1 会自锁）——修改在页
    快照上完成，再分步经池回写；"未记 redo 的脏页"没有存在窗口；
  · **链读经读取源**（P1）：`UndoChain` 绑池（`with_pool`）后，链的**一切**
    内部读（记录、段头、`append_pos`、探页）走池视角——撤销页与数据页同规
    **no-force**；写路径持池入口强制校验绑定（`TxnError::UnboundUndoChain`）；
    恢复/诊断用 `Direct`（无池直读）。前台 pwrite：稳态 **0/条**、首条 1
    （新页先格式化，1 次/页）；
  · **计划器基准取池像**：`plan_extend`/`plan_allocate_extent`/
    `plan_materialize_bitmap_page` 的读-改-写基准由调用方以
    `CurrentPages` 提供（文件像会把池里已改的字节写回旧值）。
- **撤销段扩展经池 + redo**（`ensure_undo_capacity`）：`plan_extend` 的页镜像
  逐页写 redo；no-force（P1）。
- **更新类撤销**：`Update` 载荷 = 行内偏移补丁 + 旧 `itl_slot`；v1 限**等长
  就地**更新（改长明确报错，留给行迁移切片）。
- 用例：insert/commit 落盘与日志、rollback、**端到端崩溃恢复**
  （DML → 页不回写 → `recover` → 胜者在/输家回滚）、delete/update 往返、
  扩展 redo 保护、活系统 CR（未提交不可见 / 提交可见）。
- **语句回滚点**（§4.6.6 ②）：`statement_mark`（读槽的 `undo_current`，纯内存
  快照）/`rollback_to_mark`——沿链补偿到回滚点，**不释放锁、不回滚此前语句**；
  走完把槽的 `undo_current` 置回回滚点（一次槽头更新，经池 + redo）⇒ 被撤销的
  记录**不可达**，后到的回滚/恢复重放/CR 不再补偿孤链。
- **等待-重试驱动**（§5.4.2）：`lock::WaitGate`（登记/挂起/唤醒同锁——
  "唤醒先于挂起"由标记兜底，**不丢唤醒**；唤醒不做锁移交）+ `execute_with_wait`
  （`RowLocked` ⇒ 登记 → 挂起 → 从头重试；死锁环上**修改量最少者是牺牲者** ⇒
  语句级回滚 + 取消等待 + `DeadlockVictim`；`WaitPolicy::max_waits` 上限可选）。
- **事务引擎门面**（`engine`，REQ-ENG-002）：`begin` / `snapshot`（语句级 RC，
  注册表封顶 undo 保留）/ `commit`（提交记录入流 + 耐久 → 发布提交序号 →
  **唤醒等待者**）/ `rollback` / `语句回滚点` + `rollback_statement` /
  `lock_row`（获得 / 等待 / 牺牲）。**句柄不透明**（不暴露 undo/事务表/ITL）。
  共享资源各在 `Mutex` 后（V1.0 每工作区 1 个 undo 段 ⇒ 全会话共用；
  `&Engine` 可跨线程）；等待-重试驱动重构为 `StatementContext`——**挂起期间
  不持锁**（引擎的每次尝试自取日志/撤销链）。**纯加锁落行级痕**：`lock_row`
  改写行头 `itl_slot` 并追加 `Update` 撤销记录（"锁与修改同源"判据的证据，
  见待讨论清单第 21 条）。**未接**：`查询终态`（REQ-API-014；随 #63 细则）。
- **跨段限制（实测发现）**：持锁者判定 `holder_locked_this_row` 按 `txn_id`
  查**当前链所在段**的事务表——两个会话的撤销段不同时会查不到而把活动锁
  当"陈旧字节"放行。多会话部署前必须按 `txn_id.usn` **路由到持锁者的段**
  （见待讨论清单第 20 条补记）；本 crate 的跨线程用例因此让两会话共享一个段。
- 用例：insert/commit 落盘与日志、rollback、**端到端崩溃恢复**
  （DML → 页不回写 → `recover` → 胜者在/输家回滚）、delete/update 往返、
  扩展 redo 保护、活系统 CR（未提交不可见 / 提交可见）、语句回滚保留先前语句
  与锁、回滚点幂等、死锁牺牲者的语句级回滚、**跨线程等待→唤醒→重试**。
