# bicdb-txn

事务状态机、语句快照、行锁、Undo、死锁检测

| 项 | 值 |
| --- | --- |
| 设计依据 | §8 崩溃恢复契约（§4.6.6 事务生命周期、§11.1.1 提交流程、§11.1.2 undo 受 redo 保护） |
| 对应阶段 | P3（已启动）／P4（行锁、死锁检测） |
| 当前状态 | **v0.2**——`write`（写路径：`begin`/`insert`/`delete`/`update`/`commit`/`rollback` **经缓冲池**）；行锁与死锁检测随后 |

## 状态说明

- **写路径**（`write`）：DML 只改缓存里的页（写先入缓存）——
  `begin`（槽分配也经池）→ 占 ITL（记 `ITL 覆盖`）→ 改页 → 记撤销记录 →
  页差异 redo → `page_lsn` → 标脏；`commit` = 提交记录入流 → 等 LGWR 刷到它
  → 事务表槽置已提交；`rollback` = 沿链补偿（逐条经池、带 redo）→ 释放槽。
- **两条纪律**：
  · **单闩锁纪律**：持 `PageGuard` 时不得再调池（N=1 会自锁）——修改在页
    快照上完成，再分步经池回写；"未记 redo 的脏页"没有存在窗口；
  · **undo 页经池 + 写后立即 flush**：链的读取（回滚/CR）是直读段文件的，
    立即落盘让池/文件一致；数据页保持 no-force（提交路径无数据页 I/O）。
- **撤销段扩展经池 + redo**（`ensure_undo_capacity`）：`plan_extend` 的页镜像
  逐页写 redo 后 flush。
- **更新类撤销**：`Update` 载荷 = 行内偏移补丁 + 旧 `itl_slot`；v1 限**等长
  就地**更新（改长明确报错，留给行迁移切片）。
- 用例：insert/commit 落盘与日志、rollback、**端到端崩溃恢复**
  （DML → 页不回写 → `recover` → 胜者在/输家回滚）、delete/update 往返、
  扩展 redo 保护、活系统 CR（未提交不可见 / 提交可见）。
