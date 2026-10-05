# 证据包：闩锁机制与优化（Oracle latch × PG LWLock）— 2026-10-05

**用途**：回答"闩锁如何优化"（保护内存结构、避免冲突访问），为 P4 的
分区闩锁与当前的缓冲池临界区改造备料。**只作证据，不作决定。**

## 检索范围（原始输出见 `raw/`）

| # | 命令 | 文件 |
| --- | --- | --- |
| 01 | oracle `problem 'latch 闩锁 自旋 spin willing-to-wait 争用'` | `01` |
| 02 | oracle `problem 'willing-to-wait immediate latch 请求 模式 spin_count 自旋次数'` | `02` |
| 03 | oracle `problem 'latch free 等待 获取 miss sleep 统计'` | `03` |
| 04–06 | oracle `detail`：`_LATCH_SPIN_COUNT` / `_LATCH_WAIT_POSTING` / latch free | `04`–`06` |
| 07 | pg `problem 'LWLock 轻量级锁 tranche 等待队列 实现'` | `07` |
| 08 | pg `problem 'lwlock 实现 原子 等待 链表 快速路径'` | `08` |
| 09 | pg `problem 'BufferDesc 缓冲区头 自旋锁 内容锁 pin 计数'` | `09` |
| 10–12 | pg `detail`：先 pin 后内容锁 / LWLockAttemptLock 快速路径 / LWLock 概念 | `10`–`12` |

## 证据行

### Oracle latch

| 证据 | 要点 |
| --- | --- |
| `04`（Note 33984.1 `_LATCH_SPIN_COUNT`，ksl.h） | 获取 latch **前的自旋等待次数**（与 `SPIN_COUNT` 相关）——闩锁 = **先自旋、后睡眠**的两段式 |
| `05`（Note 32835.1 `_LATCH_WAIT_POSTING`） | 自旋未获 ⇒ **睡眠重试/等待投递**；投递机制与 SPIN_COUNT 协作 |
| `06`（Note 34576.1 "latch free"） | 等待事件字段：**P1 latch 地址、P2 latch 编号（V$LATCHNAME）、P3 睡眠尝试次数**——诊断口径就是"哪个闩锁、睡了几次" |
| `02`（Note 1061799.6） | SPIN_COUNT=450 过低 ⇒ 频繁睡眠、争用；调大到 2000 缓解——**自旋/睡眠是参数化的权衡** |
| `01`（Note 433631.1） | 无 CAS 平台用 mutex 模拟 latch、自旋致高 CPU——**自旋不是免费的** |
| `03`（Note 1012049.6 / v$latch） | 每类闩锁统计：**gets / misses / sleeps / gethitratio**——闩锁是**具名类别**（V$LATCH ↔ V$LATCHNAME），可观测性是设计的一部分 |
| （既有证据包 kcbb） | `cache buffers chains` 与 `cache buffers lru chain` **分设**；桶**在 latch 间轮转**（kcbz.h：桶数组指针 + 一把 latch）——**分片即降争用** |

### PostgreSQL LWLock

| 证据 | 要点 |
| --- | --- |
| `12`（概念条目） | 持有时短、**共享 + 独占**两模式、**无死锁检测**（但有可观测性）——与 Oracle latch 同族定位 |
| `11`（lwlock.c:795-856 `LWLockAttemptLock`） | **快速路径 = 单次原子 CAS**：SHARE 只查排他位（加共享计数）、EXCLUSIVE 要求计数全零；CAS 失败也写回原值充当**内存屏障**——**无竞争时 wait-free**，零系统调用 |
| `10`（先 pin 后内容锁） | **必须先 pin 再取 per-buffer 内容锁**：否则页可能在取锁前被时钟扫描驱逐，锁失去保护对象——pin（引用计数）与闩锁**职责分离** |
| `09`（BufferDesc 核心结构） | 缓冲区头 = **状态标志 + 自旋锁 + 内容锁** + 身份——**每缓冲一锁，不是全局一把** |
| `07`（LWLockAcquire / 等待队列） | 竞争时 `LWLockQueueSelf` 入 **FIFO 等待队列**（proclist），释放者唤醒队首——但**被唤醒者仍需重新竞争**（不是直接移交，避免 convoy/饥饿 vs 保序的取舍） |
| `08`（tranche） | 轻量锁按 **tranche 命名/分组**注册，供 `pg_stat_activity` 等待事件（`LWLock:BufferContent` 形态）与 `pg_locks` 诊断 |

## 结论（供设计引用）

1. **闩锁的两条正交优化轴**：① **临界区纪律**——闩锁只护"数据结构操作"，
   **I/O 与长工作必须在闩锁外**（PG 的 pin/内容锁分离、Oracle 的 latch 持有
   以指令计、`KCBB_REDO` 推迟写在闩外）；② **降低争用面**——分片（Oracle 桶×
   latch 轮转）与 per-object 锁（PG 每缓冲一内容锁）。
2. **先自旋后睡眠**是共同形态（`_LATCH_SPIN_COUNT` / LWLock 的 CAS 快速路径），
   但**自旋参数化且可伤 CPU**（433631.1）；现代实现（PG 9.6+）把"无竞争"
   做进**单次 CAS**，把系统调用（futex/信号）留给竞争路径。
3. **闩锁必须具名、可统计**（V$LATCHNAME + gets/misses/sleeps；PG tranche +
   等待事件）——"闩锁 free"没有名字就没有优化。
4. **无死锁检测是闩锁的定义性质**（LWLock 概念条目直言"无死锁检测"；Oracle
   latch 不可长等）——顺序规则（先 pin 后内容锁、持闩不等待业务锁）替代检测。
5. 我们的现状（代码核对）：缓冲池一把 `Mutex<Inner>` 统管且 **pin 在持锁期间
   做文件读**（最坏形态）；守卫生命周期 = 持锁（单守卫纪律由此而来）；
   `LogBuffer::flush_to` 已是"锁内摘页、锁外 I/O"的正确形态（可作范式）。

## 未核验 / 存疑

- PG 9.6 的 `LWLock` 重写（`LW_FLAG_HAS_WAITERS`、`LWLockWaitList` 无锁
  入队）未检索到实现级条目（`08`/`11` 覆盖了快速路径与队列语义，够本轮使用）。
- Oracle latch 的**共享模式**（`kslgetl` 的 mode 参数）未检索到原文；我们若
  需要"读共享"版闩锁，以 PG SHARE/EXCLUSIVE 为形态依据（证据 `11`）。
