# 证据包：DB Cache 内部机制定案（Oracle kcbwds / LRU / 哈希链 / DBWR）

模块：存储架构 §5.10（缓冲池）与 §11.7（检查点与后台写）　对应：**DML 写路径前置设计**
核验日期：2026-10-05　原始输出：`raw/01–26`（同目录）

> 与既有包的分工：`logswitch-mech-20261004` 定**日志组与切换**、
> `logbuffer-mech-20261005` 定**日志缓冲与 LGWR**；本包补的是**数据侧**——
> 数据库缓存的工作集/链表/缓冲区头/DBWR 写盘机制。写路径设计需要两者合起来。

## 结论（第四轮补：桶数定量与热块诊断——2026-10-05）

15. **桶数的定量规则（找到了）**：`_DB_BLOCK_HASH_BUCKETS` 的**默认值 =
    `db_block_buffers / 4`**；调整它"影响哈希链长度与 `cache buffers chains`
    latch 的争用情况"（通常不必修改）。（证据 28）
16. **热块诊断链路**（Note 163424.1）：`v$latch_children` 按 **SLEEPS** 找争用
    的**子闩锁** → 用它的 `ADDR` 关联 **`x$bh`**（`hladdr`）与 `dba_extents`
    → 看 **`x.tch`（TCH 列 = 块被 SQL 命中的次数）**判热块；"TCH 越高，块被
    访问越频繁"。闩锁在**扫描哈希链**时获取，链由 latch 的 **child** 保护。
    （证据 29）

## 结论（再补：哈希定位与桶数——2026-10-05 第三轮）

12. **快速定位 = 取模哈希 + 链上扫描**：`_DB_BLOCK_HASH_BUCKETS`（Note 33393.1）
    的原文定义——"哈希桶/链的数量；SGA 中的缓冲区**通过把所需 DBA 对该数取模**
    定位到一条哈希链，**再在链上扫描**要找的 DBA"；桶数是**质数**
    （Note 33439.1："There are a PRIME number of Hash buckets"）。
13. **闩锁粒度**：8i 源码形态（`kcbz.h`）＝"**每个 latch/bucket 结构 = 一个桶
    数组指针 + 一把 latch**；**桶在 latch 间轮转分配**"——即一把 latch 保护
    **一组桶**（不是一链一锁）；`_DB_BLOCK_MULTIPLE_HASHCHAIN_LATCHES`
    （`kcb.h`，默认 TRUE）是"每链一把 vs 全局一把"的旧口径开关。（证据 24–27）
14. **桶数的"默认比例"在知识库中未展开**——只有机制与参数存在性；"每链期望
    长度"这条定量规则**未核到**（见未核验项 5，我们按期望链长自定）。

## 结论（第四轮补：桶数定量与热块诊断——2026-10-05）

15. **桶数的定量规则（找到了）**：`_DB_BLOCK_HASH_BUCKETS` 的**默认值 =
    `db_block_buffers / 4`**；调整它"影响哈希链长度与 `cache buffers chains`
    latch 的争用情况"（通常不必修改）。（证据 28）
16. **热块诊断链路**（Note 163424.1）：`v$latch_children` 按 **SLEEPS** 找争用
    的**子闩锁** → 用它的 `ADDR` 关联 **`x$bh`**（`hladdr`）与 `dba_extents`
    → 看 **`x.tch`（TCH 列 = 块被 SQL 命中的次数）**判热块；"TCH 越高，块被
    访问越频繁"。闩锁在**扫描哈希链**时获取，链由 latch 的 **child** 保护。
    （证据 29）

## 结论（补充：淘汰/老化算法细目——2026-10-05 第二轮）

9. **8i 替换算法 = touch count + 老化减半（Note 104937.1 原文）**：
   每个缓冲区一个 `tch` 字段；命中时递增，但 **`dbagingtouchtime` 秒内的连续
   命中只计一次**；**当需要空闲缓冲区时，DBWR/前台扫描寻找低 touch count 的
   缓冲区，但不会立即淘汰——把它的 touch count 减半（aging）后继续留在缓存里
   循环**。即"touch count + 老化"取代 LRU。（证据 19）
10. **参数语义（`kcbpacc`/`kcbpasc` 模块）**：
    - `_HOT_CRITERIA`：计数达它 ⇒ 缓冲区**移到 LRU 热端**；
    - `_COOL_COUNT`（`kcbpacc`）：缓冲区**被冷却时**把计数**置为该值**；
    - `_STAY_COUNT`（`kcbpasc`）：缓冲区**被移到替换列表头部**（提升）时把计数
      **置为该值**；
    - `dbagingtouchtime`：命中合并窗口（三秒规则的参数名）。（证据 20、21、22）
11. **高计数块即使位于列表尾部也不被重用**：替换不是严格 LRU——尾部只是
    候选起点，**计数够高就不淘汰**（与第 9 条的"减半后再循环"互为因果）。
    （证据 23）

## 结论（第四轮补：桶数定量与热块诊断——2026-10-05）

15. **桶数的定量规则（找到了）**：`_DB_BLOCK_HASH_BUCKETS` 的**默认值 =
    `db_block_buffers / 4`**；调整它"影响哈希链长度与 `cache buffers chains`
    latch 的争用情况"（通常不必修改）。（证据 28）
16. **热块诊断链路**（Note 163424.1）：`v$latch_children` 按 **SLEEPS** 找争用
    的**子闩锁** → 用它的 `ADDR` 关联 **`x$bh`**（`hladdr`）与 `dba_extents`
    → 看 **`x.tch`（TCH 列 = 块被 SQL 命中的次数）**判热块；"TCH 越高，块被
    访问越频繁"。闩锁在**扫描哈希链**时获取，链由 latch 的 **child** 保护。
    （证据 29）

## 结论（再补：哈希定位与桶数——2026-10-05 第三轮）

12. **快速定位 = 取模哈希 + 链上扫描**：`_DB_BLOCK_HASH_BUCKETS`（Note 33393.1）
    的原文定义——"哈希桶/链的数量；SGA 中的缓冲区**通过把所需 DBA 对该数取模**
    定位到一条哈希链，**再在链上扫描**要找的 DBA"；桶数是**质数**
    （Note 33439.1："There are a PRIME number of Hash buckets"）。
13. **闩锁粒度**：8i 源码形态（`kcbz.h`）＝"**每个 latch/bucket 结构 = 一个桶
    数组指针 + 一把 latch**；**桶在 latch 间轮转分配**"——即一把 latch 保护
    **一组桶**（不是一链一锁）；`_DB_BLOCK_MULTIPLE_HASHCHAIN_LATCHES`
    （`kcb.h`，默认 TRUE）是"每链一把 vs 全局一把"的旧口径开关。（证据 24–27）
14. **桶数的"默认比例"在知识库中未展开**——只有机制与参数存在性；"每链期望
    长度"这条定量规则**未核到**（见未核验项 5，我们按期望链长自定）。

## 结论（第四轮补：桶数定量与热块诊断——2026-10-05）

15. **桶数的定量规则（找到了）**：`_DB_BLOCK_HASH_BUCKETS` 的**默认值 =
    `db_block_buffers / 4`**；调整它"影响哈希链长度与 `cache buffers chains`
    latch 的争用情况"（通常不必修改）。（证据 28）
16. **热块诊断链路**（Note 163424.1）：`v$latch_children` 按 **SLEEPS** 找争用
    的**子闩锁** → 用它的 `ADDR` 关联 **`x$bh`**（`hladdr`）与 `dba_extents`
    → 看 **`x.tch`（TCH 列 = 块被 SQL 命中的次数）**判热块；"TCH 越高，块被
    访问越频繁"。闩锁在**扫描哈希链**时获取，链由 latch 的 **child** 保护。
    （证据 29）

## 结论

1. **工作集（WS）是缓存的分区单位，缓冲块静态归属**：每个 WS 有自己的
   **主替换链 + 辅助链（LRU-AUX）+ 写列表（LRU-W）+ 闩锁 + 统计**；
   `X$KCBWDS` 给出 8.1 的完整字段族——`SET_ID`、**`START_BUF#/END_BUF#`**
   （该 WS 的缓冲块区间）、`CNUM_SET`、`FLAG`（`KCBWDS_MKFREE 0x1`）、
   **`CKPT_LATCH`（检查点队列闩锁）与 `SET_LATCH`（工作集闩锁）分设**、
   替换链（`NXT/PRV_REPL`、`NXT/PRV_REPLAX`、`CNUM_REPL`、`ANUM_REPL`、
   **`COLD_HD`（冷段首）**、**`HBMAX/HBUFS`（热段上限/现有）**）、
   写列表（`NXT/PRV_WRITE` + `…AX`、`CNUM/ANUM_WRITE`）；8.1 起每个 WS
   关联一个 `DBWR_NUM`。（证据 1、2、3）
2. **替换不再是严格 LRU，而是 touch count**：缓冲区被**钉住时且距上次
   计数递增 ≥3 秒**才 `touch count +1`（"三秒规则"防突发访问虚高）；
   **冷端高计数块提升到热端**，低计数块被淘汰。（证据 4、5）
3. **三条链的分工**（8.1）：
   - **LRU-MAIN**：热段（头）+ 冷段（尾），**`COLD_HD` 是分界**；
     缓冲区**首次被重用**时插入**冷段头部**（不是热段）；热段有上限
     `HBMAX`；
   - **LRU-AUX**（辅助子链）：**可重用候选**——新块哈希进 AUX；DBWR
     扫描尾部确认**干净**的块也进 AUX；**前台找空闲缓冲先扫 AUX**，
     找不到才搜主链；
   - **LRU-W**（写列表）：**已老化、需写盘后才能重用**的脏块；**尾部
     加入、DBWR 从头部写**；**正在被 DBWR 写的块停在 LRU-W 的 AUX 段**；
     写完（干净、可重用）→ 移入 LRU-AUX。（证据 6、7、8、9）
4. **前台找不到空闲缓冲的流程**：扫 AUX → 主链冷端；统计
   `free_buffer_inspected`（含干净/脏/钉住）、`dirty_buffers_inspected`；
   遇脏块**移入 LRU-W**；扫描上限 `db_block_max_scan_cnt`（默认
   **缓冲数/4**）；DBWR 队列已达 **2×write_batch** 或扫完上限仍无 ⇒
   **投递"需要空闲空间"消息（Make Free）**并等待（`free buffer waits`）。
   （证据 10、11）
5. **DBWR 的 Make Free 批处理**：取 LRU 闩锁 → 清 `MKFREE` 标志 →
   若 LRU-W 空且已知空闲数 > 扫描深度的一半 ⇒ 继续睡；否则从 **LRU
   尾部**搬脏块到 LRU-W **尾部**，直到 `dbwr batch` 数量或**扫描深度**
   为止（`clean buffers scanned` 计数）；**释放闩锁后**写文件；写完的
   块放入 **LRU-AUX**。扫描深度自适应（`DBLDEPTH` 低水位/`DBHDEPTH`
   高水位/`DBIDEPTH` 增量/`DBDDEPTH` 减量）。（证据 12、13）
6. **DBWR 写原因共 11 种**（`X$KCBBES`）：高/中/低优先检查点
   （**中优 = 增量检查点**，最常见的常态写）、实例/介质恢复检查点、
   表空间检查点、**老化写（AGING）**、**脏块限流（DBUF_LMT）**、
   复用对象/复用范围、PING（RAC）；优先级三级；
   **保存码里 `KCBB_REDO` = "write deferred till log sync"**——即
   **"redo 未落盘就推迟写块"**，WAL 规则 2 在 DBWR 侧的原始出处；
   其余保存码：`BEING_WRITTEN`/`NOT_DIRTY`/`BEING_MODIFIED`/
   `QUIT`（I/O 打满）/`FLUSHED`/`FULL`（无空位）。（证据 14）
7. **哈希链**：`DBA mod 质数桶数` 的哈希桶，每桶一条双向链；**latch
   保护**（8i 起一把 latch 保护**一组桶**，桶在 latch 间轮转分配）；
   缓存层三大等待事件 `free buffer waits` / `write complete waits` /
   `buffer busy waits`，两把主 latch（`cache buffers chains` 哈希链 /
   `cache buffers lru chain` LRU 链）。（证据 15、16）
8. **缓冲区头（kcbbh）**：LRU/哈希链指针、**用户列表**（持用者）、
   **等待者列表**、flags、buffer state、低/高/恢复 RBA、变更状态、
   **检查点队列（CKPTQ）与文件队列（FQ）链接**、工作集描述、FOQ 标志、
   锁元素、缓冲区地址、**touch count 与时间统计**。（证据 17、18）

## 证据

| # | 来源类型 | 条目 ID（前缀 `4:8ed6c541-fe08-4ade-ae68-a79bf45316f4:`） | 数据库 | 主题 | 核验日期 | 适用条件 |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | 知识库 KnowledgeEntry | `:3257632` | Oracle | Working Set（kcbwbl）：每 WS 独立 LRU/LRU-W/LRU-AUX 链与闩锁；统计族 | 2026-10-05 | 结构 |
| 2 | 知识库 KnowledgeEntry | `:1996842`（Note 43632.1） | Oracle | **X$KCBWDS 全字段**（8.1）：START/END_BUF#、MKFREE、CKPT_LATCH/SET_LATCH、COLD_HD、HBMAX/HBUFS、REPL/WRITE 主+AUX 链、扫描深度四参数 | 2026-10-05 | **字段级** |
| 3 | 知识库 KnowledgeEntry | `:1997366`（Note 121623.1） | Oracle | X$KCBWBPD：缓冲池 → 连续 WS 区间（多池时） | 2026-10-05 | 结构 |
| 4 | 知识库 KnowledgeEntry | `:270353` | Oracle | **8.1 替换列表变化**：touch count、冷段头插入、AUX 定义、LRU-W 尾进头出 | 2026-10-05 | **算法核心** |
| 5 | 知识库 KnowledgeEntry | `:1070522` | Oracle | **触摸计数三秒规则**：钉住时且间隔 >3s 才 +1；冷端高计数升热端 | 2026-10-05 | **算法核心** |
| 6 | 知识库 KnowledgeEntry | `:270351` | Oracle | 三链结构（AUX/MAIN/W）与流动：哈希入 AUX、重用入冷段头、DBWR 扫主链尾 | 2026-10-05 | **算法核心** |
| 7 | 知识库 KnowledgeEntry | `:3256603` | Oracle | LRU-AUX：写盘完成的块进 AUX，前台**优先**从 AUX 拿 | 2026-10-05 | 算法 |
| 8 | 知识库 KnowledgeEntry | `:3256615` | Oracle | LRU 多链总称；替换仍是"冷端优先"的近似 LRU | 2026-10-05 | 概念 |
| 9 | 知识库 KnowledgeEntry | `:3259104` | Oracle | 分配从冷端找、跳过钉住/脏；脏块移 LRU-W | 2026-10-05 | 流程 |
| 10 | 知识库 KnowledgeEntry | `:1993935`（Note 33612.1） | Oracle | **前台找空闲缓冲伪码**：计数、移 LRU-W、上限 `db_block_max_scan_cnt`（默认 缓冲数/4）、2×write_batch 退出并投递 | 2026-10-05 | **流程** |
| 11 | 知识库 KnowledgeEntry | `:1995652`（Note 33629.1） | Oracle | `dbwr_make_free_requests` 统计（Make Free 协议） | 2026-10-05 | 诊断 |
| 12 | 知识库 KnowledgeEntry | `:3256209` | Oracle | **DBWR Make Free 批处理**：清标志、LRU 尾扫 → LRU-W、batch/扫描深度、写完入 AUX | 2026-10-05 | **流程** |
| 13 | 知识库 KnowledgeEntry | `:1996842`（同上） | Oracle | `SUM_WRT/SUM_SCN/NUM_SCN/HOT_SCN/PIN_SCN/WRT_MOV/AUX_MOV` 与四扫描深度 | 2026-10-05 | 统计 |
| 14 | 知识库 KnowledgeEntry | `:2040682`（Note 113818.1） | Oracle | **X$KCBBES：11 种写原因 + 三级优先级 + 8 个保存码**（含 `KCBB_REDO`） | 2026-10-05 | **触发条件** |
| 15 | 知识库 KnowledgeEntry | `:1993852`（Note 33439.1） | Oracle | 哈希 = DBA mod **质数**桶数；每桶双向链；latch 保护（8i：一 latch 保护一组桶） | 2026-10-05 | 结构 |
| 16 | 知识库 KnowledgeEntry | `:1565357`、`:151931` | Oracle | 三大等待事件 + 两把主 latch（hash chain / lru chain） | 2026-10-05 | 诊断 |
| 17 | 知识库 KnowledgeEntry | `:1537362` | Oracle | **kcbbh 字段族**（含 CKPTQ、FQ、touch count/时间） | 2026-10-05 | **字段级** |
| 18 | 知识库 KnowledgeEntry | `:1537352` | Oracle | kcbbh：LRU/哈希指针、用户列表、等待者列表 | 2026-10-05 | 结构 |
| 19 | 知识库 KnowledgeEntry | `:364630`（Note 104937.1） | Oracle | **8i 新替换算法**：`tch` 字段、`dbagingtouchtime` 合并窗口、**扫描不减半不淘汰** | 2026-10-05 | **算法核心** |
| 20 | 知识库 KnowledgeEntry | `:203534` | Oracle | Touch Count 三参数：`_HOT_CRITERIA`（升热端）/`_COOL_COUNT`（冷却置值）/`_STAY_COUNT`（保留值） | 2026-10-05 | **参数语义** |
| 21 | 知识库 KnowledgeEntry | `:1994508`（Note 68477.1） | Oracle | `_DB_AGING_COOL_COUNT`（`kcbpacc`）：冷却时置的计数 | 2026-10-05 | 参数语义 |
| 22 | 知识库 KnowledgeEntry | `:1994511`（Note 68480.1） | Oracle | `_DB_AGING_STAY_COUNT`（`kcbpasc`）：移到替换列表头时置的计数 | 2026-10-05 | 参数语义 |
| 23 | 知识库 KnowledgeEntry | `:1541572` | Oracle | 计数够高的块**即使位于列表尾也不被重用** | 2026-10-05 | 算法核心 |
| 24 | 知识库 KnowledgeEntry | `:2048560`（Note 33393.1） | Oracle | **`_DB_BLOCK_HASH_BUCKETS` 原文**：DBA 对桶数取模 → 链上扫描 | 2026-10-05 | **定位机制** |
| 25 | 知识库 KnowledgeEntry | `:1993852`（Note 33439.1） | Oracle | 桶数取**质数**；latch 保护哈希链 | 2026-10-05 | 定量 |
| 26 | 知识库 KnowledgeEntry | `:1996837`（kcbz.h 引文） | Oracle | 8i：latch 结构 = **桶数组指针 + 一把 latch**；**桶轮转分配**到 latch | 2026-10-05 | **闩锁粒度** |
| 27 | 知识库 KnowledgeEntry | `:1994391`（Note 33954.1） | Oracle | `_DB_BLOCK_MULTIPLE_HASHCHAIN_LATCHES`（`kcb.h`，默认 TRUE） | 2026-10-05 | 闩锁粒度 |
| 28 | 知识库 KnowledgeEntry | `:153944` | Oracle | **`_DB_BLOCK_HASH_BUCKETS` 默认 = `db_block_buffers / 4`**；影响链长与 latch 争用 | 2026-10-05 | **定量规则** |
| 29 | 知识库 KnowledgeEntry | `:1996588`（Note 163424.1） | Oracle | **热块诊断**：latch children 的 SLEEPS → `x$bh.hladdr` → **`x.tch`** → `dba_extents` | 2026-10-05 | 诊断 |

## 检索范围声明

- 已检索（2026-10-05，7 轮 problem + 15 次 detail，原始输出存 `raw/01–26`）：
  `'buffer cache LRU 链 LRU-W LRU-AUX …'`、`'kcbwds 工作集 描述符 …'`、
  `'哈希链 cache buffers chains latch …'`、`'buffer header kcbbh …'`、
  `'DBWR 扫描 LRU 尾部 … 检查点队列 …'`、`'touch count …'`（两次，第二次
  命中）、`'DBWR 触发 检查点 …'`、`'free buffer waits …'`（命中）、
  `'冷端 两次 触摸 …'`；detail `:270351/:270353/:3256603/:3256209/:3256615/`
  `:3257632/:1537352/:1537362/:1996842/:1993852/:1995610/:1070522/`
  `:1993935/:3259104/:2040682/:1994508`。命中取 18 组。
  **按召回预算有界，非全库穷尽。**
- 未命中/未展开：**`_db_aging_*` 各参数的默认数值**（语义已核——冷却置值/
  提升置值/热判据——但具体数字未展开）；**`db_block_write_batch` /
  `dbwr_scan_depth` 的默认值**；**`DB_BLOCK_HASH_BUCKETS` 的默认桶数**；
  **热段上限 `HBMAX` 的取值规则**（只有字段语义）。
- 占位内容：无。

## 冲突与未决

| 项 | 知识库说法 | 另一说法 | 处置 |
| --- | --- | --- | --- |
| LRU-W 与检查点队列的关系 | LRU-W 按**老化序**（尾进头出）；CKPTQ 是**独立的按低 RBA 链** | X$KCBWDS 两者分列字段 | **我们合并为一条**（见自主决策 2）——我们的低水位判据就是"首次变脏 LSN"，两者序一致，合并省一套链 |
| touch count 的阈值 | 语义已核（热判据/冷却置值/提升置值 + 扫描减半），**数值**未见 | — | 取值自定：热判据 2 / 冷却 0 / 提升置 2（见自主决策 8） |

## 本项目自主决策

1. **分区 = 工作区（已有设计不变），本包补内部结构**：每分区 = 一组
   哈希桶 + 主替换链（热/冷两段，`COLD_HD` 语义）+ LRU-AUX 子链 +
   LRU-W + `SET_LATCH`；检查点队列按 §11.7 的设计**每工作区**一条。
   工作区 → 分区 = `H(workspace_id) mod N`（§5.10 已定），缓冲块静态
   归属其工作区的分区 ✓（对应 `START_BUF#/END_BUF#` 的静态归属）。
2. **LRU-W 与检查点队列合并为一条"写列表"**：排序键 = **首次变脏的
   LSN**。理由：Oracle 分设是因为它的检查点推进按低 RBA、老化写按访问
   序，两条目标不同；我们的低水位判据就是"已全部落盘点"，**两条序在
   我们的画像下一致**，合并后"DBWR 从头部写 = 低水位前移"天然成立
   （§11.7 的既有设计）。
3. **touch count 照抄三秒规则，阈值为自定参数**：钉住时且距上次递增
   ≥3s 才 +1；**计数达热判据（2）→ 热段头**，且**提升时把计数置为
   `stay_count`（2）**（`kcbpasc` 语义）；**冷却（新装入/热段溢出退回）置
   `cool_count`（0）**（`kcbpacc` 语义）；热段上限 = 分区缓冲数的 1/4
   （`HBMAX` 语义，取值自定）。数值未核验，配置项，可调。
4. **前台找空闲缓冲照 8.1 流程，含老化减半**：AUX → 主链冷端；跳过钉住/
   正写；脏块交写列表；**冷端候选 `tch > cool_count` ⇒ 计数减半（aging）、
   继续扫，不立即淘汰**（Note 104937.1 的原文行为）；上限 = 缓冲数/4；
   达 2×batch 或超上限 ⇒ Make Free 并等待（`free buffer waits`）；
   "正在写的块"的等待 = `write complete waits`。
5. **DBWR 写原因收敛为我们的四类**（对应 `KCBB_*`）：**增量检查点**
   （中优，常态）、**Make Free**（前台压力）、**老化写**（后台周期）、
   **脏块限流**（脏比例到线）；恢复期写块并入 checkpoint 类。
   **`KCBB_REDO` 的落点 = 我们的 WAL 规则 2**：页 `page_lsn` 未持久化
   则**推迟写**（催 LGWR），照抄"write deferred till log sync"。
6. **不采用**（显式）：RAC 的 **PING 链**（单实例）、**LRU-XO/XR**
   （对象/区间复用链——V1 段删除不做在线对象级清理）、**NUMA
   `PROC_GROUP`**、**多缓冲池**（KEEP/RECYCLE——留配置位不实现）。
7. **等待与诊断口径映射**（照 Oracle 命名，便于对照）：
   `free buffer waits` / `write complete waits` / `buffer busy waits` /
   `log file sync`（§11.5.5）/ `log buffer space`（§11.5.5）；每分区记
   `FBWAIT/WCWAIT/BBWAIT` 与 `free/dirty/pinned inspected`（照 X$KCBWDS）。

## 未核验项

1. **`db_block_write_batch` 与 `dbwr_scan_depth` 默认值**：条目给规则不给
   默认。→ 我们取"batch = max(8, 分区缓冲数/64)"并以配置项暴露；扫描深度
   自适应从"分区缓冲数/16"起。
2. **`DB_BLOCK_HASH_BUCKETS` 默认桶数**：未展开。→ 我们取**质数**桶数、
   ≈ 2×缓冲数（Oracle 常见取值口径），配置项。
3. **热段上限 `HBMAX` 的取值规则**：只有字段语义。→ 取分区 1/4（自主决策 3）。
4. **_db_aging_* 阈值的 Oracle 原值**：语义已核（见结论 10），**数值**未核到。
   → 自定（热判据 2 / 冷却 0 / 提升置 2）并标注可调。
5. ~~哈希桶数的"默认比例 / 定量规则"未核到~~ **已核（第四轮）**：
   `_DB_BLOCK_HASH_BUCKETS` 默认 = **`db_block_buffers / 4`**。→ 我们照此：
   **桶数 = ≥ 容量/4 的最小质数**（质数取整是我们的，机制照证据）；
   定位用 **`DBA mod 桶数`**（原文口径）；闩锁按 kcbz.h 形态：一把 latch
   保护一组桶、桶轮转分配（当前单分区 = 一把，P4 再分片）。

---

## 补检索（2026-10-05 晚）：kcbwds 与 NUMA 的绑定

**问题**：kcbwds 是否可以绑定 NUMA、做内存访问性能优化？

| 证据 | 来源 | 要点 |
| --- | --- | --- |
| `raw/48-kcbwds-fields.txt`（detail `1997678`） | X$KCBWDS 字段全集 | 8.1 新增 `DBWR_NUM`（**本工作集关联的 DBWR 进程**）、**`PROC_GROUP` = "NUMA processor group for buffers in this set"**（本工作集的缓冲所属的 NUMA 处理器组）、`START_BUF#/END_BUF#`（本集的**连续缓冲区间**）——三者合起来：**工作集 = 一段缓冲 + 一组处理器 + 一个 DBWR** |
| `raw/49-db-block-numa.txt`（detail `1994517`） | Note 68926.1 `_DB_BLOCK_NUMA` | Oracle8i 隐藏参数：**"Number of NUMA nodes"**（注释 "Sequent Specific"——NUMA 机器的年代渊源） |
| `raw/52-numa-instance-mapping.txt`（detail `1994588`） | Note 68587.1 `_NUMA_INSTANCE_MAPPING` | 指定**实例允许运行的 NUMA 节点集**（ksmins） |
| `raw/50-numa-pg.txt` | X$KSMNIM / X$KSMNS | `KSMNIMPROCGRP`（处理器组）记录**实例 ↔ 处理器组**绑定；`X$KSMNS`（`KSMNSNAM/LEN/PROCGRP`）按处理器组统计**内存分配**——用于诊断"**buffer cache 亲和性配置错**"与跨节点访问延迟 |
| `raw/51-numa-alloc.txt` | Note 780466.1 "How NUMA Allocates Memory" | Linux 默认**本地优先、耗尽才远程**（`numa_memory_allocator`）——用户态"首次触碰 = 本地分配"正是据此成立 |

**结论**：可以，且 Oracle 8.1 就是这么做的——`kcbwds.PROC_GROUP` 把每个
工作集绑定到一个 NUMA 处理器组，配套"连续缓冲区间 + 每集一个 DBWR"，
让缓存命中、latch、写回都落在**本地内存**上。本库 §5.10 现把 `PROC_GROUP`
列为"不要"（理由"单机小规模"）——**若目标部署含多路（双 socket）服务器，
该理由需要复议**；落法见 §5.10 的 P4 并发路线（第二级绑定：工作区 → 工作集
→ 处理器组；帧区间按集独立分配 + 首次触碰 + 写线程亲和）。

**未核验**：`PROC_GROUP` 的取值/编码与 `_DB_BLOCK_NUMA` 的默认值未检索；
Oracle 在非 NUMA 机器上该字段的语义（恒 0？）未见原文。
