# 事务/锁/Undo-Redo 对照核对（用户提供资料 × 本库实现）

- **日期**：2026-10-05　**来源**：用户提供《Oracle / PostgreSQL 事务、锁、Undo-Redo
  机制剖析（伪代码级）》（`~/Oracle_vs_PostgreSQL_事务_锁_UndoRedo_机制剖析.html`）
  + 知识库复核（Oracle/PostgreSQL 域，2026-10-05）
- **目的**：逐条核对本库既有算法与两家成熟做法的异同；确认正确性、提取可落优化

## 一、"我们的取法与两家一致"（核对通过，无需改）

| 主题 | 本库 | Oracle / PG | 判读 |
| --- | --- | --- | --- |
| 事务标识 | `txn_id = usn 8 │ slot 8 │ wrap 32` | Oracle `USN.SLOT.SEQ`（16/16/32） | 同形（压缩）✓ |
| 行锁落点 | ITL 数组 + 行头 `itl_slot` 字节 | Oracle 同款（`lb` 字节） | ✓ |
| ITL 语义 | `txn_id/undo 指针/state/lock_cnt/commit_seq` | `XID/UBA/Flag/Lck/SCN` | ✓（`lock_cnt` = `Lck`） |
| 无锁升级 | 不做 | Oracle 设计上不存在 | ✓ |
| 等待对象 | 按**持锁者 txn_id** 挂等待者 | Oracle TX enqueue（id1=usn, id2=slot） | 同形 ✓ |
| 提交点 | 提交记录耐久即提交 | Oracle commit SCN + redo 刷盘 | ✓ |
| 提交后槽标记 | 经池 + redo（尽力而为；恢复**前滚补标记**兜底） | Oracle：槽更新只写 redo，块由"下一个访问者"延迟清除；PG：clog 靠 WAL 重放重建 | 三者同构 ✓（我们已无-force，等价的"延迟"由池承担） |
| 回滚自身写 redo | ✓ | Oracle ✓ | ✓ |
| CR 构造 | ITL 判定 → 沿链倒带前像 → `ITL 覆盖` 还原 | Oracle `oracle_consistent_read` 同构 | ✓ |
| CR 不落笔 | 重建在内存副本，源页不动 | Oracle 会在 CR 路径做块清除并写 redo | **我们更省**（读路径零写）✓ |
| 死锁检测 | 等待 ≥3s → 建图 → DFS → 牺牲者 = 修改量最少 | Oracle 同款（3s + DFS + 最低代价） | ✓ |
| 恢复 | 两阶段（重做 → 撤销 + 分析定胜者） | Oracle 两阶段 / PG 一阶段 | 我们 = Oracle 形态（有 undo）✓ |
| 撕裂页 | 不做 FPW/双写区，推给 COW 文件系统 + 校验和**检出** | PG 全页写 / MySQL 双写区 | §11.5.4 的取舍与理由已在案 ✓（KB 复核：FPW 关闭 + 非 COW ⇒ 无法修复，与我们"检出即报"一致） |

## 二、发现与动作

### ① 死锁检测的锁域（**已修**）

原实现：`gate.with_registry(|r| ctx.detect(r, …))`——检测在**门锁内**锁撤销链并
`chain.lookup` 选牺牲者，而链页可能不在池里 ⇒ **在门锁内读盘**，把"登记/挂起/
唤醒"全卡住。这违反我们自己的设计口径（§5.4.2 ④"检测器对该结构无锁读，容忍
一次瞬时不精确"）——文档 §7.3 记录的同款教训（PG：检测在持有锁管理器锁时运行
会放大竞争）。

**修法**：`lock::WaitGraph`（冻结快照：边 + 最老等待时长，门锁内克隆、立即
还锁）+ `detect_deadlock_from(&graph, chain, threshold)`；驱动先
`gate.snapshot(now)` 再出锁检测。单机形态的 `detect_deadlock(registry, …)`
保留为包装。用例：快照冻结语义 + 门锁不被检测占用。

### ② Undo 页"写后立即 flush"的前台成本（**已修**：2026-10-05，P1 落地）

现状：每条撤销记录（`write_undo_page_change`）后**立即 `pool.flush`**——即
每条 DML 一次 16 KB 的**前台 pwrite**（无 fsync ✓，但 syscall 与写流量与修改
次数同阶）。Oracle/PG 的 undo/CLOG 页都**没有**这条前台写：Oracle 靠"只写 redo、
块延迟清除"，PG 靠"clog 是缓存、WAL 是事实来源"。

根因：**CR/回滚直读段文件**（`UndoChain::read` 走 `DataFile` 直读，不经池）
⇒ 必须让文件与池一致。修法：链读取走缓冲池（undo 页转 no-force，与数据页
同规），随后"立即 flush"即可删除。代价：`cr::reconstruct`/回滚链回放要接池
（已有池句柄的调用点不受影响；`UndoChain` 需一个"经池读页"的口）。
**判据**：小事务提交路径的 pwrite 数（当时每条 undo 记录 1 次）。

**修复结果**（`doc/并发与内存优化方案_v0.1.md` P1）：链读改经池（读取源
`Direct`/`Pool`），三类 flush 删除；实测稳态 **2 次/条 → 0 次/条**（首条 5 → 1：
只剩新树撤销页"先格式化落盘"，1 次/页），批 DML 单线程 **2.44×**。实现中还
发现另有 5 处直读点（`append_position`/回收探页/计划器基准/`page_lsn` 预置/
`lookup`）必须一并改经读取源——见待讨论清单第 26 条与 §12.3.1 的检查表。

### ③ ITL 空间不足与事务表槽耗尽：Oracle 等待、我们报错（**记档**）

- Oracle：`enq: TX - allocate ITL entry`（KB Note:549074.1：INITRANS/PCTFREE
  不足 ⇒ **等待**，而非报错）；undo 段槽满 ⇒ `ORA-30036` 或 `enq: US` 等待。
- 我们：`NoSlotAvailable`（ITL）/ `NoFreeSlot`（事务表）**报错**。
- 落法（会话层随执行器接入时实现）：登记在"该块全部活动事务 / 该段全部活动
  事务"的 TX 上（我们的等待表天然支持多持锁者登记），任一结束即唤醒重试；
  在此之前，执行器把这两个错误当**可重试资源类**（`CONV` §4.3）。

### ④ CR 块缓存（**记档**）

Oracle 把 CR 版本缓存进 buffer cache（KB Note:33438.1：buffer header 有
`CR status` 字段），重复长查询命中省重放。我们每次重建（扫描批内按块共享
一次 ✓）。收益在"同一批块被多次长查询反复读"；代价是失效逻辑（提交/清除
都会让 CR 过期）。**暂不做**，与"扫描直读"同批按实测再定。

### ⑤ 软边/硬边的前提（**写进设计口径**）

PG 需要"软边/硬边 + 递归打断"是因为存在**锁模式升级等待**（S→X 同锁转换）；
我们**没有任何转换**（行锁只有 X + 重入；对象锁是登记式）⇒ 等待边全为硬边
⇒ 简单 DFS 成立。**前提条件**：一旦引入共享锁或模式升级（如未来对象锁的
S→X），必须补 PG 式的软边处理。已记入 §5.4.2。

### ⑥ 运维教训（不进代码，进清单）

- 悬挂 prepared 事务同时钉住 undo/锁与"清理推进"（2PC 本库 V1.0 不做；
  若资产/会话协议将来引入 prepared 态，此教训适用）。
- 唯一键冲突的正确形态 = **等冲突事务的 TX → 醒来重查**（文档 §10 场景 2）：
  与我们"登记在持锁者 + 醒来重头重试"的模型同构，落唯一索引切片时照办。
