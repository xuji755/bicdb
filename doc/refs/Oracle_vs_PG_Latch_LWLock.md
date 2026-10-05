# Oracle Latch 与 PostgreSQL LWLock 算法对照（伪代码级）

> 说明：本文中的伪代码是**逻辑等价**描述（保留真实函数名、真实状态位定义、真实控制流骨架），  
> 不是源码逐行翻译。Oracle 内部符号（`kslgetl` / `kslfre` / `kslwt` / `X$KSLLT`）来自可观测的内核层与固定表；  
> PostgreSQL 符号（`LWLock*` / `S_LOCK` / `TAS` / `perform_spin_delay`）来自 `src/backend/storage/lmgr/lwlock.c`、`s_lock.c`、`s_lock.h`、`spin.h`。

---

## 0. 一句话结论

|         | Oracle Latch                                              | PostgreSQL Spinlock + LWLock                                 |
| ------- | --------------------------------------------------------- | ------------------------------------------------------------ |
| 排队      | **完全不排队**，纯"抢 + 退避 + 兜底睡眠"                                | Spinlock 不排队；**LWLock 严格 FIFO 排队（proclist）**                 |
| 唤醒      | 释放者 `post` 后**被唤醒者重新竞争**（可能抢输）                            | 释放者**直接把锁交给**队首（grant & handoff），唤醒者确信拿到                     |
| 临界区长度假设 | 微秒级，**持有者绝不允许主动睡眠/阻塞**                                    | Spinlock：微秒级；**LWLock：允许跨 I/O 持有（fsync、读盘）**                 |
| 死锁处理    | latch level 检测潜在自死锁 + **latch recovery（强夺泄漏 latch）**      | 从不强夺；靠 `deadlock_timeout` 触发死锁检测器，可中断等待                      |
| 共享模式    | 有（shared latch，少数场景）                                      | 有，且是第一等公民（`LW_VAL_SHARED` 原子计数）                              |
| 统计可见性   | **极丰富**（V$LATCH / V$LATCH_MISSES / V$LATCHHOLDER，可定位到代码行） | 几乎没有内置计数，靠 `pg_stat_activity.wait_event` + trace probe       |
| 数量      | 固定小集合，**几百 ~ 几千**（`_db_block_hash_latches` 等可控）           | 启动时确定：`NUM_INDIVIDUAL_LWLOCKS` + buffer 数 + `MaxBackends` 相关 |

**核心分歧点**：Oracle 认为 latch 的临界区必须短到"自旋比入队更便宜"，所以拒绝队列；  
PostgreSQL 承认有一大类共享结构要在**持有期间做阻塞 I/O**（`WALWriteLock` 持有时 fsync、buffer content lock 持有时 ReadBuffer），  
自旋毫无意义，于是必须排队 + 睡眠。**这不是谁更先进，而是对"临界区长度分布"的不同赌注。**

---

## 1. 三层同步原语的对应关系

理解 latch/lwlock 必须先把它放回整个同步栈里，否则会拿错对手比较。

| 层次               | Oracle                                                | PostgreSQL                                                 | 特征                                |
| ---------------- | ----------------------------------------------------- | ---------------------------------------------------------- | --------------------------------- |
| L0：内存字级原子        | 自旋 + 原子 CAS（TAS/xchg）                                 | `SpinLockAcquire`（`TAS` + `s_lock`）                        | 无队列、无睡眠、不可重入、持有期极短                |
| L1：SGA 微结构保护     | **Latch**（`kslgetl`/`kslfre`）+ 后期 **KGX Mutex**       | **LWLock**（`LWLockAcquire`/`LWLockRelease`）                | 保护共享内存结构；LWLock 可睡眠、Mutex 带自旋等待队列 |
| L2：可排队/可长期/可死锁检测 | **Enqueue**（`ksqget`，`enq: TX - row lock contention`） | **Heavyweight Lock**（`LockAcquire`/`ProcSleep`，`pg_locks`） | FIFO 队列、锁序检查、死锁图、可持有到事务结束         |

> 常见类比陷阱：把 Oracle latch 直接对应 PG LWLock。更准确的对应是  
> **Oracle latch ≈ PG spinlock + LWLock 的一部分职责**，而 **Oracle Enqueue ≈ PG Heavyweight Lock**。  
> PG 把 Oracle 的 latch 一分为二：真正微秒级的留给 spinlock，可能阻塞的升级为 LWLock。

---

## 2. Oracle Latch 算法

### 2.1 数据结构（对应 `X$KSLLT` 一行）

```c
struct latch {                       /* kslb / latch 对象 */
    volatile u32   state;            /* 0 = FREE；否则打包 {holder_pid, mode, flags} */
    u8             type;             /* exclusive / shared-capable / parent / child */
    u8             class;            /* latch class（统计分组用，V$LATCH_CLASS） */
    u8             level;            /* latch level：用于潜在自死锁检测 */
    u16            latch_num;        /* 常驻编号，V$LATCH.LATCH# */

    struct waiter *waiters;          /* post/wait 链表（非严格 FIFO，仅"有/无"语义） */
    struct latch  *children;         /* parent latch → child latch 数组 */
    struct latch  *parent;

    /* 计数器（X$KSLLT 的 KSLLT* 字段） */
    u64 gets;            /* 成功获取总数 */
    u64 misses;          /* willing-to-wait 中第一次 CAS 失败 */
    u64 spin_gets;       /* 自旋阶段内拿到的次数 */
    u64 sleep_gets;      /* 睡眠（post/wait）后被唤醒并拿到的次数 */
    u64 immediate_gets;  /* no-wait 模式成功 */
    u64 immediate_misses;/* no-wait 模式失败（调用方自己决定放弃/降级） */
    u64 sleeps;          /* 实际进入 OS 等待的次数 */
    u64 recovery_count;  /* 被"回收/强夺"的次数 */
};
```

关键点：

- **没有 FIFO 队列**。`waiters` 只是"有人在等"的信息，用来决定释放时是否 `post`。
- **state 是单字**，因此获取/释放都可以用一条原子指令完成——这是 Oracle latch 快的根源。
- **parent/child 分层**：如 `shared pool` 是 parent，其下几十上百个 child latch。  
  获取时按 PID / 轮转 / hash 挑一个 child，把热点打散（这是 Oracle 抗 latch 争用的主要手段）。  
  `V$LATCH_PARENT` 与 `V$LATCH_CHILDREN` 分开统计，只有 parent 的 gets 才是"逻辑获取次数"。
- **latch level**：持有 latch 时递增，若试图获取一个 level 更低的 latch，说明有潜在死锁环，  
  内核会报 ORA-00600/自死锁探测。这是 Oracle 用"静态层次"换"不做图搜索"的典型手法。
- **latch holder 不允许睡眠**。一旦持有者在临界区里做了会阻塞的事（读文件、等网络），  
  就可能被其它进程判定为"卡住的 latch"，进而触发 recovery 把 latch 夺走。

### 2.2 两种获取模式

| 模式                         | 行为                       | 适用                                          |
| -------------------------- | ------------------------ | ------------------------------------------- |
| **willing-to-wait**（默认可自旋） | 自旋 → wait posting → 睡眠等待 | 必须拿到，拿不到就不干活                                |
| **no-wait / immediate**    | 一次 CAS，失败立刻返回 `MISS`     | 有替代路径时（如换一个 hash bucket、换 freelist、直接去做慢路径） |

Oracle 大量共享结构都提供了 **no-wait 的降级路径**，这是它把争用"摊开"而不是"排队"的关键设计。

### 2.3 `kslgetl` 伪代码（willing-to-wait）

```c
#define SPIN_COUNT  _spin_count     /* 默认 2000（版本/平台相关） */
#define WAIT_POSTS  _latch_wait_posting /* "先别睡，再试几次"的轮次 */

latch_state kslgetl(latch *l, mode_t mode, bool nowait)
{
    /* ---------- 模式 0：no-wait ---------- */
    if (nowait) {
        u32 want = pack(my_pid, mode);
        if (atomic_cas(&l->state, FREE, want)) {
            l->immediate_gets++;
            return GOT;
        }
        l->immediate_misses++;
        return MISS;                      /* 调用方自己决定重试 / 换 bucket / 放弃 */
    }

    /* ---------- 阶段 1：紧自旋  ---------- */
    /* 目标：在"持有者马上就会释放"的绝大多数场景下，完全不进内核 */
    for (u32 spin = 0; spin < SPIN_COUNT; spin++) {
        if (atomic_cas(&l->state, FREE, pack(my_pid, mode))) {
            l->gets++; l->spin_gets++;
            l->misses_of_first_probe++;   /* 统计口径：第一次没中才算 miss */
            return GOT;
        }
        /* 平台相关的"降压"：让出流水线 / 检查是否需要让 CPU */
        if ((spin & 0x3F) == 0x3F)
            cpu_relax_or_yield();
        backoff(spin);                    /* 指数/分段退避，减小 cache line 抖动 */
    }

    /* ---------- 阶段 2：wait posting（Oracle 特有） ---------- */
    /* 已经自旋很久了，但 Oracle 仍不立刻睡：先把"我要等"登记出去，
       再用很短的时间片反复探一次。目的是避免"刚登记就要被叫醒"的
       OS 睡眠/唤醒开销（每次都要经历内核调度）。 */
    push_waiter(&l->waiters, self);
    for (u32 i = 0; i < WAIT_POSTS; i++) {
        if (atomic_cas(&l->state, FREE, pack(my_pid, mode))) {
            remove_waiter(&l->waiters, self);
            l->gets++; l->wait_gets++;
            return GOT;
        }
        if (posted(self))                 /* 捕获到这期间别人给的 post */
            consume_post(self);
        cpu_relax_or_yield();
    }

    /* ---------- 阶段 3：真正睡眠（post/wait） ---------- */
    for (;;) {
        if (atomic_cas(&l->state, FREE, pack(my_pid, mode))) {
            remove_waiter(&l->waiters, self);
            consume_post(self);
            l->gets++; l->sleep_gets++;
            return GOT;
        }

        l->sleeps++;
        /* 语义：注册 semaphore，然后阻塞。可被 release 一方的 post 唤醒 */
        if (wait_on_semaphore_timed(my_sem, timeout(TIMEOUT_I)) == POSTED)
            continue;                     /* 醒来永远只是"再试一次"，不是"锁给你了" */

        /* 超时：不再被动等，走 recovery 路径（见 2.5） */
        if (try_latch_recovery(l, my_pid) == RECOVERED)
            return GOT_RECOVERED;
    }
}
```

**注意阶段 3 的语义**：`sleep_gets` 统计的是"睡醒之后重新 CAS 成功"，也就是说  
**唤醒 ≠ 授权**。N 个等待者被同时 post，会一起醒来重新抢——这就是 Oracle latch 的  
thundering herd 与可能的饥饿来源；它用"自旋 + 退避"来缓解，而不是用队列来消除。

### 2.4 `kslfre` 伪代码（释放）

```c
void kslfre(latch *l, mode_t mode)
{
    u32 old = atomic_read(&l->state);

    /* 先检查等待者，决定 release 是否需要走 post 慢路径 */
    if (!is_empty(l->waiters)) {
        /* Oracle 的语义：把 latch 置为 FREE，然后 post 等待者，让大家重新竞争。
           "先置位、后 post" 的顺序是有意为之——避免唤醒到一半 latch 还是 held。 */
        atomic_write(&l->state, FREE);
        __memory_barrier();

        if (mode == SHARED && still_shared_holders(l))
            return;                        /* 共享释放：其它共享持有者还在，不必唤醒 */
        post_all_or_one(&l->waiters);       /* 通常把所有等待者从 sem 唤醒（re-spin） */
        l->waiters_woken++;
    } else {
        atomic_write(&l->state, FREE);
    }
    /* 无队列 → 无 FIFO 保证 → 无"顺序唤醒"，也不需要维护队列一致性 */
}
```

对比 PG：PG 的 `LWLockRelease` 在唤醒路径上**承担了授权责任**，Oracle 的 `kslfre` 只负责"打开门"。

### 2.5 Latch Recovery（Oracle 独有，也是它最危险/最强大的部分）

因为 latch 不排队，内核无法从数据结构上知道"谁欠我一次释放"。  
当等待时间超过阈值（或等待者发现持有者所在进程状态异常、序号过期、持有时间远超 `longhold`），  
等待者会**直接把 latch 抢过来**：

```c
bool try_latch_recovery(latch *l, pid_t me)
{
    u32 cur = atomic_read(&l->state);
    if (cur == FREE) return NOT_NEEDED;

    pid_t holder = unpack_pid(cur);

    /* 判定条件（逻辑等价）：持有者已消失 / 持有者长时间未运行 /
       持有时间超过 _latch_recovery 类隐藏参数阈值 /
       该 latch 已被判定为 orphan */
    if (holder == me)                     /* 自持有自等 → 自死锁，直接报错 */
        raise_self_deadlock(l);

    if (holder_is_gone(holder) || hold_time_exceeded(l) || orphan_suspected(l)) {
        /* 原子地"把持有者替换成我" */
        if (atomic_cas(&l->state, cur, pack(me, EXCLUSIVE))) {
            l->recovery_count++;
            return RECOVERED;
        }
    }
    return NOT_RECOVERED;
}
```

后果：**被抢走 latch 的原持有者会在一个它以为受保护的区域里继续跑**，  
所以 Oracle 有 ORA-00600 [latch recovery] / 数据块损坏的理论风险窗口。  
PG 则完全相反：绝不强夺，宁可让死锁检测器报错（`deadlock detected`）把事务打回。

### 2.6 从 latch 到 mutex（10gR2 之后的演进）

`latch → KGX mutex`（`cursor: pin X/S`、`library cache: mutex X`、`V$MUTEX_SLEEP`）：

```c
struct kgx_mutex {
    volatile u32 state;      /* refcount | EXCLUSIVE | VALID | WAITERS */
    struct list_head waiters;
};

bool kgxgetmutex(kgx_mutex *m, mode, bool nowait)
{
    /* 与 latch 同族：自旋 → 乐观 CAS → 挂 waiters 链表 → sem post/wait
       区别：mutex 的持有边界更小（往往就是几条指令 + 一次内存写），
             且总是"绑定到具体的游标/对象"，消除了 parent/child 的间接层 */
}
```

含义：Oracle 自己也承认"latch 的抽象对一个只有几十条指令的临界区来说太重"，  
于是把最热的 library cache / cursor 路径改成了 mutex。这恰好印证了 PostgreSQL 的分层思路。

### 2.7 诊断视图

| 视图                                                | 用途                                                                                        |
| ------------------------------------------------- | ----------------------------------------------------------------------------------------- |
| `V$LATCH` / `V$LATCH_PARENT` / `V$LATCH_CHILDREN` | GETS / MISSES / SPIN_GETS / SLEEP_GETS / IMMEDIATE_GETS / IMMEDIATE_MISSES / SLEEPS       |
| `V$LATCH_MISSES`                                  | **WHERE 列直接给出热点代码位置**（如 `kcbgtcr`、`kglhdgn`）+ NWFAIL/SLEEP/WTR_SLP/LONGHOLD               |
| `V$LATCHHOLDER`                                   | 当前持有者 PID + latch 地址 + 已获取次数（定位"谁卡住了"）                                                    |
| `V$LATCH_CLASS`                                   | 按 class 聚合，`_latch_classes` 可调                                                            |
| 等待事件                                              | `latch: cache buffers chains`、`latch: shared pool`、`latch: redo copy`、`latch free`（10g 前） |
| `V$MUTEX_SLEEP` / `V$MUTEX_SLEEP_HISTORY`         | mutex 争用的"谁等谁"                                                                            |

判读要点：  
`MISSES/GETS` 是**争用强度**，`SLEEP_GETS/GETS` 是**是否真的打进了内核**。  
前者高但后者低 = 自旋够用（健康）；两者都高 = 临界区过长或 latch 数量过少。

---

## 3. PostgreSQL Spinlock 与 LWLock 算法

### 3.1 第 0 层：spinlock

```c
/* ---- include/storage/s_lock.h ---- */
#define TAS(lock)         __sync_lock_test_and_set(lock, 1)   /* x86: lock xchg */
#define TAS_SPIN(lock)    spin_read_until_zero_then_tas(lock) /* 先只读轮询，减少 cache 行独占 */
#define S_UNLOCK(lock)    __sync_lock_release(lock)
#define S_LOCK_FREE(lock) (*(lock) == 0)
#define S_LOCK(lock)      (TAS(lock) ? s_lock(lock, __FILE__, __LINE__) : 0)
```

```c
/* ---- s_lock.c ---- */
int s_lock(volatile slock_t *lock, const char *file, int line)
{
    SpinDelayStatus delay = init_spin_delay(file, line);

    while (TAS_SPIN(lock))
        perform_spin_delay(&delay);      /* 关键：退避与让出都发生在这里 */

    finish_spin_delay(&delay);
    return delay.delays;                 /* 返回"我自旋了多少轮"，用于统计/调试 */
}

void perform_spin_delay(SpinDelayStatus *st)
{
    /* 1) 前 spins_per_delay 轮：纯 CPU 停留指令，不惊动调度器 */
    if (++st->spins < st->spins_per_delay) {
        pg_spin_delay();                 /* x86: _mm_pause / rep; nop */
        return;
    }

    /* 2) 中间：逐级增加"每次 sleep 前允许的空转轮数"（指数型），
          目的是把大量短暂争用吸收在用户态，不要把调度器叫醒 */
    if (st->cur_delay < NUM_DELAYS /* 1000 */) {
        st->cur_delay++;
        st->spins_per_delay = Min(st->spins_per_delay * 2, MAX_SPINS_PER_DELAY);
        return;
    }

    /* 3) 尾部：1ms 级 usleep，之后逐渐变成 sched_yield 让出整个时间片；
          同时允许执行 backlog 等安全检查 */
    if (st->cur_delay == NUM_DELAYS + 1)
        pg_usleep(1000);
    else {
        pg_usleep(1000);
        st->cur_delay++;
    }
}
```


**PG 的自旋退避是"轮数指数增长 + 睡眠时长固定"**，
Oracle 是"轮数固定（`_spin_count`）+ 让出策略平台相关"。
前者在"几百个核抢同一把锁"的现代机器上表现更好（减少了 cache line 独占流量）。

使用纪律（写在 PG 头文件注释里，属于硬约束）：
**持有 spinlock 期间不得 `elog`、不得分配内存、不得调用任何可能阻塞/抛错的函数**，也**不可重入**。

### 3.2 LWLock 的数据结构与状态字

```c
typedef struct LWLock {
    uint16          tranche;     /* tranche id，用于命名/统计（BufferContent、WALWrite...） */
    pg_atomic_uint32 state;      /* 唯一的状态字：模式 + 共享计数 + 三个 flag */
    proclist_head   waiters;     /* FIFO 等待队列：PGPROC.links 组成的侵入式双向链表 */
#ifdef LOCK_DEBUG
    pg_atomic_uint32 nwaiters;
    struct PGPROC   *owner;      /* 最后一个 exclusive 持有者（调试） */
#endif
} LWLock;
```

```c
/* ---- lwlock.h：state 的位分配（真实定义） ---- */
#define LW_FLAG_HAS_WAITERS  ((uint32) 1 << 30)   /* 队列非空 */
#define LW_FLAG_RELEASE_OK   ((uint32) 1 << 29)   /* 释放侧握手位，见 3.6 */
#define LW_FLAG_LOCKED       ((uint32) 1 << 28)   /* "等待队列锁"自己被占用 */
#define LW_VAL_EXCLUSIVE     ((uint32) 1 << 24)   /* 排他持有 */
#define LW_VAL_SHARED        ((uint32) 1)         /* 共享计数为 1 */
#define LW_LOCK_MASK         ((uint32) ((1 << 25) - 1))
```

三种"锁"叠在同一片内存上：

1. **锁本身**（bit24/共享计数）
2. **队列锁**（bit28 `LW_FLAG_LOCKED`）——保护 `waiters` 链表和 HAS_WAITERS 位，一把用 CAS 实现的微自旋锁
3. **握手位**（bit29 `RELEASE_OK`）——消除"释放者绕过队列锁"与"等待者检查队列"之间的丢唤醒竞态

### 3.3 `LWLockAcquire` 伪代码

```c
bool LWLockAcquire(LWLock *lock, LWLockMode mode)
{
    /* ================= 快路径：期望完全无原子竞争 ================= */
    if (mode == LW_EXCLUSIVE) {
        uint32 expected = 0;
        if (pg_atomic_compare_exchange_u32(&lock->state, &expected, LW_VAL_EXCLUSIVE))
            goto got_lock;                        /* 零额外开销，一次 CAS */
    } else {
        uint32 old = pg_atomic_fetch_add_u32(&lock->state, LW_VAL_SHARED);
        if (!(old & (LW_VAL_EXCLUSIVE | LW_FLAG_LOCKED | LW_FLAG_HAS_WAITERS)))
            goto got_lock;                        /* 共享获取是原子加，不加锁 */
        /* 有人排他持有 / 队列锁被别人拿着 / 有人在排队
           → 回退计数，走慢路径（"有人在排队"时不能插队，保证 FIFO） */
        pg_atomic_sub_fetch_u32(&lock->state, LW_VAL_SHARED);
    }

    /* ================= 慢路径：排队 + 睡眠 ================= */
    for (;;) {
        LWLockWaitListLock(lock);       /* 先拿"队列锁"（LW_FLAG_LOCKED 自旋） */

        /* 拿到队列锁意味着此刻没有人正在改队列 / 正在 grant，
           所以可以安全地再判断一次"是否可以直接授予"，避免无谓睡眠 */
        if (LWLockCanGrant(lock, mode)) {
            LWLockGrantLock(lock, mode);   /* 直接写 state，把锁给我自己 */
            LWLockWaitListUnlock(lock);
            goto got_lock;                 /* 常见：等的人不多，醒来前就被授予 */
        }

        /* 不能授予 → 挂到队尾（FIFO），并标记 HAS_WAITERS */
        proclist_push_tail(&lock->waiters, &MyProc->links);
        pg_atomic_fetch_or_u32(&lock->state, LW_FLAG_HAS_WAITERS);
        LWLockWaitListUnlock(lock);

        /* ---- 睡在自己的信号量上 ---- */
        MyProc->lwWaiting = (mode == LW_SHARED) ? LW_WAIT_SHARED : LW_WAIT_EXCLUSIVE;
        int extraWaits = 0;
        for (;;) {
            PGSemaphoreLock(&MyProc->sem);     /* 可被死锁检测器/中断唤醒 */
            if (MyProc->lwWaiting == LW_WAIT_NONE)
                break;                          /* 授权者清掉了标记 => 锁归我了 */
            extraWaits++;                       /* 伪唤醒（信号量多余计数）→ 继续睡 */
        }
        while (extraWaits-- > 0)
            PGSemaphoreUnlock(&MyProc->sem);    /* 清掉多余的计数，避免下轮误醒 */
        goto got_lock;
    }

got_lock:
    TRACE_POSTGRESQL_LWLOCK_ACQUIRE(T_NAME(lock), T_ACQUIRE(mode));
    return true;
}
```

两个关键设计点：

* **`lwWaiting` 是"授权凭证"**：只有 `LWLockWakeup`/`LWLockGrantLock` 会把它清成 `LW_WAIT_NONE`。
  所以"睡醒"与"拿到"是可以分离判断的——这就把 Oracle 那种"醒来再抢、可能抢输"的不确定性彻底消掉了。
* **快路径允许 barging（插队）的唯一条件是队列为空**（`!LW_FLAG_HAS_WAITERS`）。
  PG 用这一条把公平性从"绝对 FIFO"降级为"**近似 FIFO，无饥饿死锁**"，换来了快路径完全无锁。

### 3.4 `LWLockWaitListLock` / `LWLockCanGrant` / `LWLockGrantLock`

```c
/* 队列锁：用状态字里的一位实现的"锁的锁"。注意它是自旋锁，绝不睡眠。 */
static void LWLockWaitListLock(LWLock *lock)
{
    for (;;) {
        uint32 old = pg_atomic_fetch_or_u32(&lock->state, LW_FLAG_LOCKED);
        if (!(old & LW_FLAG_LOCKED))
            return;                              /* 我拿到了队列锁 */

        /* 已经有人拿着：先做"只读轮询"等待，不做 CAS，
           避免多个自旋者把 cache line 在核间来回踢（这是 PG 的关键优化） */
        for (;;) {
            if (!(pg_atomic_read_u32(&lock->state) & LW_FLAG_LOCKED))
                break;
            pg_spin_delay();
        }
    }
}

static bool LWLockCanGrant(LWLock *lock, LWLockMode mode)
{
    uint32 state = pg_atomic_read_u32(&lock->state);
    if (mode == LW_EXCLUSIVE)
        return (state & LW_LOCK_MASK) == 0;       /* 完全空闲 */
    else
        return !(state & LW_VAL_EXCLUSIVE) &&    /* 无人排他 */
               !(state & LW_FLAG_LOCKED) &&      /* 此处由持有队列锁的我们保证 */
               ((state & LW_LOCK_MASK) < LW_LOCK_MASK);
}

static void LWLockGrantLock(LWLock *lock, LWLockMode mode)
{
    /* 必须在持有队列锁的前提下调用：直接把它自己的模式写进 state
       这里不 post 任何信号量——是"自己授予自己"，不需要唤醒 */
    if (mode == LW_EXCLUSIVE)
        pg_atomic_write_u32(&lock->state, LW_VAL_EXCLUSIVE | LW_FLAG_LOCKED);
    else
        pg_atomic_fetch_add_u32(&lock->state, LW_VAL_SHARED);
}
```

### 3.5 `LWLockWakeup`（由释放者执行，是"handoff 授权"）

```c
static void LWLockWakeup(LWLock *lock)
{
    bool wokeup_somebody = false;

    while (!proclist_is_empty(&lock->waiters)) {
        PGPROC   *head = proclist_head_element(&lock->waiters);
        LWLockMode wmode = head->lwWaitMode;

        /* 队首如果是 exclusive 等待者，而当前有人共享持有 → 不能授予，停止 */
        if (wmode == LW_EXCLUSIVE && (lock_state_has_shared_holders(lock)))
            break;
        if (wmode == LW_SHARED && (lock_state_has_exclusive(lock)))
            break;

        proclist_pop_head(&lock->waiters);

        if (wmode == LW_EXCLUSIVE) {
            pg_atomic_write_u32(&lock->state, LW_VAL_EXCLUSIVE);  /* 直接交给它 */
        } else {
            pg_atomic_fetch_add_u32(&lock->state, LW_VAL_SHARED);
        }

        /* 授权凭据：清标记（这一步之后等待者醒来就会 break 出循环） */
        head->lwWaiting = LW_WAIT_NONE;
        head->lwWaitMode = LW_WAIT_NONE;
        wanted_wakeup_count++;

        /* 逐个 post。PG 特意避免批量唤醒，因为 semop 在部分平台上要抢内核锁 */
        PGSemaphoreUnlock(&head->sem);
        wokeup_somebody = true;

        if (wmode == LW_EXCLUSIVE)
            break;                       /* 排他只能授予一个，剩下的人继续等 */
        /* 共享可以连续授予队首一串共享等待者（真正的批量 handoff） */
    }

    if (!proclist_is_empty(&lock->waiters))
        pg_atomic_fetch_or_u32(&lock->state, LW_FLAG_HAS_WAITERS);
    else
        pg_atomic_fetch_and_u32(&lock->state, ~LW_FLAG_HAS_WAITERS);

    /* 唤醒完成后重新开启"快路径可以让别人插队"的开关 */
    pg_atomic_fetch_or_u32(&lock->state, LW_FLAG_RELEASE_OK);
}
```

这一段是 PG 与 Oracle 分歧最直观的地方：
**释放者负责"点名"，被点名的人醒来即持有；Oracle 是全员抢跑，快慢由运气决定。**

### 3.6 `LWLockRelease` 与 `RELEASE_OK` 握手协议

```c
void LWLockRelease(LWLock *lock)
{
    uint32 oldstate;
    bool   need_wakeup;

    /* ---- 快速释放：只做一次原子操作 ---- */
    if (mode == LW_EXCLUSIVE) {
        /* 清 EXCLUSIVE 位的同时看 HAS_WAITERS；若无人等，就彻底结束 */
        oldstate = pg_atomic_fetch_and_u32(&lock->state, ~LW_VAL_EXCLUSIVE);
        need_wakeup = (oldstate & LW_FLAG_HAS_WAITERS) != 0;
    } else {
        oldstate = pg_atomic_fetch_sub_u32(&lock->state, LW_VAL_SHARED);
        /* 只有"减到 0 且有人在等"才需要唤醒（共享释放到还有共享持有者时不必） */
        need_wakeup = (oldstate & LW_FLAG_HAS_WAITERS) &&
                      ((oldstate & LW_LOCK_MASK) == LW_VAL_SHARED);
    }

    if (!need_wakeup) {
        /* 无人等待：不碰队列锁，release 就是一条原子指令 —— 这就是
           RELEASE_OK 机制要保护的最常见路径（无争用时释放零额外成本） */
        return;
    }

    /* ---- 慢释放：有人等，进队列锁 ---- */
    LWLockWaitListLock(lock);

    if (lock->state & LW_FLAG_HAS_WAITERS)      /* 二次确认，防止重复 wakeup */
        LWLockWakeup(lock);

    LWLockWaitListUnlock(lock);
}
```

**`LW_FLAG_RELEASE_OK` 解决的问题（丢唤醒竞态）**：
释放者若每次都去抢队列锁，无争用时也会付出原子操作成本。
所以引入该位作为"我现在可以不做 handoff 就直接放锁"的承诺：
释放者绕过队列锁释放时，会先清掉 `RELEASE_OK`；
而等待者/新来者在没有队列锁保护的情况下观察状态时，
若发现 `RELEASE_OK` 已被清掉，就知道"有人正在负责唤醒队列"，于是安心回去睡。

### 3.7 `LWLockDequeueSelf`（可中断等待的正确退出）

LWLock 的等待是**可中断**的（等待期间可能因为 `QueryCancel`、死锁检测、SIGTERM 而返回）。
问题：如果你已被 grant（`lwWaiting == NONE`）但还没醒来就被中断了，锁就泄漏了。所以：

```c
static void LWLockDequeueSelf(LWLock *lock)
{
    if (MyProc->lwWaiting == LW_WAIT_NONE) {     /* 已经被授权 */
        /* 直接当作"已持有"返回给上层，由上层负责释放 */
        return;
    }

    LWLockWaitListLock(lock);                    /* 必须是原子的摘链动作 */
    if (MyProc->lwWaiting == LW_WAIT_NONE) {
        /* 在拿队列锁的间隙里被授权了 */
        LWLockWaitListUnlock(lock);
        return;
    }

    proclist_delete(&lock->waiters, &MyProc->links);

    if (proclist_is_empty(&lock->waiters)) {
        pg_atomic_fetch_and_u32(&lock->state, ~LW_FLAG_HAS_WAITERS);
        /* 如果锁此刻已经空闲，必须由"我这个退出者"负责把锁交给下一个人，
           否则没人会来唤醒队列 —— 这是最容易被漏掉的一步 */
        if (LWLockFree(lock))
            LWLockWakeup(lock);
    }
    LWLockWaitListUnlock(lock);
}
```

### 3.8 变体 API（Oracle 没有对应物）

```c
/* 1) 不进队列的尝试获取：纯快路径 */
bool LWLockConditionalAcquire(LWLock *lock, LWLockMode mode);

/* 2) 等待期间检查受保护的变量：把"锁"用作条件变量
      典型用途：WAL 插入锁 —— 我已把 WAL 写进公共缓冲区，
      其它后端等我的 currpos 前进（说明我已插完），而不必真的等锁 */
bool LWLockWaitForVar(LWLock *lock, uint64 *valptr, uint64 oldval, uint64 *newval)
{
    if (*valptr != oldval)
        return false;                    /* 条件已满足，不必等 */
    /* 以 LW_WAIT_VAR 模式入队（不参与 grant，只是"等条件"），
       然后 LWLockUpdateVar() 唤醒所有 var 等待者 */
}
void LWLockUpdateVar(LWLock *lock, uint64 *valptr, uint64 val)
{
    *valptr = val;                       /* 在队列锁保护下发布新值 */
    wakeup_all_var_waiters(lock);
}

/* 3) "拿不到就等，等到别人做完"，用于避免重复劳动 */
bool LWLockAcquireOrWait(LWLock *lock, LWLockMode mode);
```

### 3.9 buffer header 上的"状态字伪自旋锁"

PG 里 99% 的 buffer 元数据操作不用 LWLock，而是把 bit 直接塞进 `BufferDesc.state`：

```c
/* ---- bufmgr.c ---- */
static inline uint32 LockBufHdr(BufferDesc *desc)
{
    SpinDelayStatus delay = init_spin_delay(...);
    uint32 old;

    for (;;) {
        old = pg_atomic_read_u32(&desc->state);
        if (!(old & BM_LOCKED)) {
            /* CAS 把 BM_LOCKED 置上；顺带把 refcount 等字段一起原子读出来 */
            if (pg_atomic_compare_exchange_u32(&desc->state, &old, old | BM_LOCKED))
                return old;              /* 返回"锁上之前"的完整状态字，调用方直接用 */
        }
        perform_spin_delay(&delay);
    }
}
```

含义：**"锁"和"数据"被编码在同一个字里**，一次原子操作既上锁又读到一致快照。
这和 Oracle 的 `buffer pin` / `buffer handle` 思路同源，但 Oracle 的 `cache buffers chains` latch 是独立对象。
PG 真正的 buffer 级别"等"发生在 `buf->content_lock`（LWLock，**exclusive 时是写 buffer，shared 时是读 buffer**）
以及 `BM_IO_IN_PROGRESS`（带条件变量的 I/O 等待 `WaitIO`）。


### 3.10 加锁顺序约定与诊断

PG **不**在 LWLock 层面做锁序检查（那是 heavyweight lock 的 `LOCK_DEBUG` 功能），
而是靠代码约定，其中最著名的是 buffer 访问的三段式顺序：

```
缓存未命中/需置换时：  LockBufHdr → (hash) BufferMapping LWLock → 释放 → content LWLock
严禁：               持有 content LWLock 时再去拿 BufferMapping LWLock
```

（正因如此 `LockBuffer` 里会有"若要升级到 exclusive，必须先放掉 shared 再重新获取"的写法，
并且 PG 明确指出 **LWLock 不支持共享→排他的原地升级**。）

诊断手段：

| 需求 | 手段 |
|---|---|
| 谁在等哪把 LWLock | `pg_stat_activity.wait_event_type='LWLock'`, `wait_event='BufferContent'/'WALWrite'/'WALInsert'/'LockManager'/'PredicateXact'...` |
| 队列长度/睡眠次数 | 无内置视图；需 DTrace/SystemTap probe：`lwlock__acquire`, `lwlock__wait__start`, `lwlock__wait__done`, `lwlock__release`, `lwlock__condacquire`, `lwlock__condacquire__fail` |
| 代码定位 | 编译期 `LOCK_DEBUG`（记录 `owner`/`nwaiters`）、`pg_lwlock_check_...`、`-DLWLOCK_DEBUG` |
| tranche 命名 | `LWLockRegisterTranche` / `GetNamedLWLockTranche`，决定等待事件名 |
| 自旋参数 | 无隐藏参数，退避参数是编译期常量（`NUM_DELAYS`、`MIN/MAX_SPINS_PER_DELAY`） |

---

## 4. 逐项对照总表

| 维度 | Oracle Latch | PG Spinlock | PG LWLock |
|---|---|---|---|
| 排队 | 不排队 | 不排队 | **FIFO（proclist）** |
| 等待方式 | 自旋 → wait posting → sem post/wait | 纯自旋（pause/usleep/yield） | 睡眠在 `PGPROC->sem` |
| 唤醒语义 | 醒后重新竞争（可抢输） | — | **释放者直接授权（handoff）** |
| 公平性 | 弱（有饥饿可能） | — | 近似 FIFO（队列空时允许 barging） |
| 共享模式 | 有（罕见） | 无（只能排他） | 一等公民，原子计数实现 |
| 可升级 | 无 | — | 不允许 shared→exclusive 原地升级 |
| 持有期是否可阻塞 | **绝对不可以** | 绝对不可以 | **允许（设计上就要跨 I/O）** |
| 死锁处理 | latch level + **recovery 强夺** | — | 不加锁序检查；`deadlock_timeout` + 可中断等待 |
| 超时 | `_latch_wait_posting` / 隐藏参数 / recovery 阈值 | 无超时，一直自旋 | 无固定超时，但**可被中断**（`LWLockDequeueSelf` 兜安全） |
| 状态位紧凑度 | 单字 state + 独立 waiters 指针 | 单 bool | 单字 state（模式+计数+3 flag）+ 队列指针 |
| 队列保护 | 无队列故无需 | — | 需要"队列锁" `LW_FLAG_LOCKED`（锁的锁） |
| 数量控制 | `_db_block_hash_latches`、parent/child、`_kgl_latch_count` | 编译期 | 启动期由 buffer 数 / MaxBackends 决定，**不可运行时增加** |
| 统计 | 极丰富（V$LATCH*，可到代码行） | 无 | 基本无（wait_event + trace probe） |
| 持有者可见性 | `V$LATCHHOLDER` | 无 | 仅 LOCK_DEBUG 下的 `owner` |
| 失败降级 | **no-wait + MISS → 走替代路径** | — | `LWLockConditionalAcquire` 返回 false |
| 条件变量能力 | 无（用 enqueue / 其它机制替代） | 无 | **有**：`LWLockWaitForVar`/`UpdateVar` |

---

## 5. 典型争用场景与调优

### 5.1 Oracle 侧

| 现象 | 原因 | 处理方向 |
|---|---|---|
| `latch: cache buffers chains` 高 | 热点块被多会话反复访问；hash bucket 数不足 | 增加 `_db_block_hash_latches`；改造热点块访问模式；用 `V$LATCH_MISSES.WHERE` 定位到 `kcbgtcr`/`kcbgcur` |
| `latch free`（10g 前）+ 高 `SLEEP_GETS` | 自旋完全无效，已进内核睡眠 | 说明临界区过长或有 latch 泄漏，查 `V$LATCHHOLDER` |
| `buffer busy waits` | 已不是 latch，而是 buffer 级等待 | 看 segment 级 header/block 等待，必要时调整 `_db_block_hash_latches`、ASSM 段头争用（用多段/分区打散） |
| `latch: shared pool` / `library cache` | 共享池结构争用 | 10gR2 后大部分被 mutex 取代；绑定变量、cursor_sharing、共享池分区 |
| `latch: redo copy` / `redo allocation` | 高并发 redo 生成 | private redo strands（`_log_private_mul`）、`log_parallelism`、`redo copy` 的 no-wait 路径 |
| MIT 高延迟下 `latch free` 长尾 | 自旋不适用（CPU 被切走） | Oracle 自身会倾向用 post/wait；`_spin_count` 在该平台通常预设较小 |
| **LONGHOLD_COUNT 上升** | **有人在临界区里做了慢操作（这是 Oracle 最忌讳的事）** | `V$LATCH_MISSES.LONGHOLD_COUNT` 指向的代码基本等同于一个 bug 线索 |

调优原则：**先用 `IMMEDIATE_MISSES` 判断降级路径是否够用，再判断 `SLEEP_GETS/GETS` 是否可接受。**
Oracle 的 health check 是不需要追求 `MISSES = 0`，因为自旋成功是设计内的。

### 5.2 PostgreSQL 侧

| 现象 | 原因 | 处理方向 |
|---|---|---|
| `LWLock:BufferMapping` 高 | 共享缓冲池不足 / 全表扫描污染 / hash 分区争用 | 加 `shared_buffers`（有效上限受工作集限制）、`effective_cache_size`、避免大表 seq scan；PG 17+ `streaming read` 减少一次性大范围 pin |
| `LWLock:BufferContent` 高 | 同一页面被并发读写（热点页） | 通常是应用锁粒度问题（`FOR UPDATE` 热点行）；检查是否发生了大量的 page 级冲突 |
| `LWLock:WALInsert` 高 | WAL 插入锁（PG 9.6+ 是 `NUM_XLOGINSERT_LOCKS = 8` 组） | 减少小事务、增大 `wal_buffers`、`commit_delay`/`commit_siblings`（注意这只在特定负载下有效）、`synchronous_commit` 调优 |
| `LWLock:WALWrite` 高 | 大量后端等 fsync | 更快的存储（这是真瓶颈，不全是锁问题）、`synchronous_commit=off`/`local`（按业务容忍度） |
| `LWLock:LockManager` 高（PG14+） | 分区锁表（取代 fast-path） | 症状通常是**极多短事务 + 大量不同锁对象**；减少锁对象数量、提高事务粒度 |
| `spinlock` 自旋导致的 CPU 100% 但 `pg_stat_activity` 无明显等待 | **持有者在自旋期间做了不该做的事**（PG 的 spinlock 代码路径错误，或第三方扩展违规） | 用 perf 抓 `s_lock`/`perform_spin_delay` 栈；第三方扩展里在 spinlock 中 `elog`/分配内存是头号嫌疑 |
| `deadlock detected` 频繁 | 加锁顺序不一致 | 统一事务内访问表的顺序（这是 heavyweight lock 层，但常被和 LWLock 混淆） |

调优原则：**PG 不能直接调 spinlock/LWLock 参数**，
所有优化都要落到"减少临界区冲突"或"减少跨 I/O 持锁"这两件事上。
`LWLock` 等待事件名（tranche）就是最好的线索——它直接告诉你争用发生在哪个子系统。

---

## 6. 一页速记

```
Oracle Latch                              PG Spinlock + LWLock
------------------------------            ---------------------------------
state: 单字 {pid,mode,flag}               state: 单字 {flags|EXCL|shared_count}
waiters: 仅"有人在等"的标记                waiters: FIFO proclist（真实队列）

get:  CAS ×N(SPIN_COUNT)                  spin get: TAS + 指数退避 pause/usleep
      → wait posting 若干次               lw get:   CAS(EXCL) / fetch_add(SHARED)
      → sem post/wait 睡眠                          → 队列锁 → 入队 → 睡眠
      → 醒后重抢（可能再输）                        → 醒来即持有（handoff）
      → 超时则 latch recovery 强夺        release:  原子清位；若有人等 →
free: 置 FREE → post → 大家重抢                    队列锁 → LWLockWakeup 点名授权
                                         握手: RELEASE_OK 位消除丢唤醒
队列: 无 → 快，但有饥饿/惊群              队列: 有 → 公平，但有"锁的锁"成本
哲学: 临界区必须微秒级，             哲学: 分两层——微秒级用 spinlock，
      持有者不许阻塞                        会阻塞的用可睡眠的 LWLock
风险: recovery 强夺 → 保护失效窗口        风险: 队列锁自旋、barging 时序复杂
可见性: V$LATCH* 极丰富                   可见性: wait_event + trace probe
```

---

## 7. 参考

* Oracle：`X$KSLLT` / `V$LATCH` / `V$LATCH_MISSES` / `V$LATCHHOLDER` / `V$LATCH_CLASS` / `V$MUTEX_SLEEP`；
  内核符号 `kslgetl`、`kslfre`、`kslwt`、`kslps`；隐藏参数 `_spin_count`、`_latch_wait_posting`、`_db_block_hash_latches`
* PostgreSQL：`src/backend/storage/lmgr/lwlock.c`、`lwlock.h`、`s_lock.c`、`s_lock.h`、`spin.h`、
  `src/backend/storage/buffer/bufmgr.c`（`LockBufHdr`）、`src/backend/storage/lmgr/lock.c`（heavyweight lock）
