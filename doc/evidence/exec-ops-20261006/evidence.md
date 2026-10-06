# 证据包：执行算子（Oracle/PG 对照核验）— 2026-10-06

**用途**：核验外部材料（`~/db-executor-operators.html`，Oracle/PG 执行算子剖析）
中**影响我们算子设计**的主张；逐条给"材料说法 → 知识库证据 → 采信/修正/未核验"。
**只作证据，不作决定。**

## 检索范围（已跑命令见 `raw/`）

| # | 命令 | 文件 |
| --- | --- | --- |
| 00 | `health_check.py --daemon` | `raw/00-health.txt` |
| 01 | `problem 'Hash Join 构建 探测 批次 溢出 hashed'`（postgresql） | `raw/01-pg-hashjoin.txt` |
| 02 | `problem 'Memoize 参数化 缓存 嵌套循环 内表'`（postgresql） | `raw/02-pg-memoize.txt` |
| 03 | `problem 'Skip Scan 跳跃扫描 前导列 B树索引'`（postgresql） | `raw/03-pg-skipscan.txt` |
| 04 | `problem 'SORT ORDER BY STOPKEY Top-N 排序 提前终止'`（oracle） | `raw/04-oracle-topn.txt` |
| 05 | `problem 'Incremental Sort 增量排序 前缀 有序 LIMIT'`（postgresql） | `raw/05-pg-incsort.txt` |
| 06/09 | `problem 'HashAggregate … 磁盘 溢出'`（postgresql） | `raw/06-pg-hashagg.txt`、`raw/09-pg-hashagg2.txt` |
| 07/10 | `problem 'In-Memory 深度向量化 SIMD'`（oracle） | `raw/07-oracle-deepvec.txt`、`raw/10-oracle-deepvec2.txt` |
| 08 | `problem 'PG18 … B树 跳跃扫描'`（postgresql） | `raw/08-pg18-skipscan.txt` |
| 11–17 | `detail` × 7（下钻核验正文） | `raw/11-…` ～ `raw/17-…` |

## 逐条核验（材料主张 → 证据 → 采信）

| # | 材料主张 | 知识库证据 | 结论 |
| --- | --- | --- | --- |
| 1 | 两引擎都是**火山拉取**模型（open/next/close/rescan），阻塞 vs 流水之分 | PG 侧为通识背景；本库方案 §14 已定"SQL 层参考 PostgreSQL 的…算子分层"（`小型多线程数据库总体开发方案_v0.2.md` §14） | **采信**（作为本库执行模型） |
| 2 | 哈希连接 = BUILD（耗尽构建侧、溢写分批）+ PROBE（流式） | `532649`："插入与探测阶段：构建阶段只写入、探测阶段只读取匹配，无需更新/删除"；`988820`（PG18 批次回退 walk-back：总内存 U 形公式、只减批次不加批次）；`263863`（并行哈希 spill FaultModel） | **采信**；分批内存公式**不照抄**（并行场景，我们单线程） |
| 3 | Memoize（PG14+）：参数化内表结果缓存，命中即免重扫；**"命中率 <5% 自动禁用"** | `988815`（`nodeMemoize.c:1171-1176` est_entries 建表、`nodeMemoize.c:577-615` 超预算 **LRU 驱逐、不落盘**、`explain.c:3647` Hits/Misses/Evictions/Overflows 四计数；"Hits 占比低说明缓存没价值或 est_entries 估错"）；`1215590`/`1488068`（演示：缓存命中 99000 次） | **采信机制**（缓存 + LRU 驱逐 + 四计数）；**"自动禁用阈值"未核验**——KB 口径是判读指标而非自动禁用，**不抄**（我们按实测命中率决定是否启用） |
| 4 | `SORT ORDER BY STOPKEY` = Top-N "提前终止" | `2065263`："Oracle 将外层 ROWNUM 条件下推到排序步骤，使排序**只需保留前 20 行**，但**仍需扫描所有匹配的 950K 行**"；`2065067`（排序键是表达式时索引序不满足 ORDER BY，仍需排序） | **修正**：Top-N 只限**排序内存/CPU**，**输入必须耗尽**；"提前终止"属于其**消费方**（LIMIT 不再拉取）与无排序短路（COUNT STOPKEY 类） |
| 5 | Incremental Sort（PG13+）：前缀有序、组内排序、对 LIMIT 有利 | `1235140`："普通 Sort 需读完所有输入才能输出；第一组排完即可输出，对 LIMIT 查询尤为有利；LIMIT 很大或每组规模巨大时收益递减"；`1235143`（官方回归测试目标；LIMIT 接近全量时**甚至变差**） | **采信**；作为**按需启用**项（触发：实测 ORDER BY 前缀有序 + 小 LIMIT 的场景出现） |
| 6 | HashAggregate 磁盘溢出（PG13+） | `557030`/`557041`（概念级确认：结果集过大时用磁盘存储） | **采信**（我们落 temp 段） |
| 7 | PG18 引入 B-tree Skip Scan | `2288804`（PG18 Index Skip Scan：跳过重复前导列值定位下一组，`INDEX(a,b)` 且 `WHERE b=5`，前导列 distinct 少时收益大） | **采信**（事实成立）；**我们维持"不做"**（既有决定：SQL 子集简单），记为将来触发 |
| 8 | PG 仍是行式火山、无官方向量化 | 与材料一致（PG 侧无列存层）；Oracle 侧 `519184`："SIMD 提高 **In-Memory 列存储**的扫描性能…每秒数十亿行"、`2229137`（SIMD 无需新硬件成本） | **采信**：向量化绑定列存层（Oracle In-Memory）；我们没有列存 ⇒ V1.0 不做向量化（与 README"明确不做"一致） |
| 9 | Oracle INDEX ROWID BATCHED（12c+）批量回表 | 既有证据包 `scan-mech-20261005`（`228665` 回表按块记账、`1054817` 覆盖索引不回表、`2065847` 聚簇因子只影响需回表路径）；本库 §9.4 已定批量回表 | **采信**（已落地于 §9.4 设计） |
| 10 | PG18 异步 I/O（io_uring）提升扫描吞吐 | `1474878`（AWS 冷缓存：io_uring 5.7s vs sync 15.1s） | 采信事实；**我们 V1.0 不做**（无并行/异步执行器；单查询单线程） |
| 11 | Oracle 26ai `FETCH FIRST` 回退 ROWNUM+COUNT STOPKEY（bug 35915968） | 未检出 | **未核验**，不影响设计 |
| 12 | 伪代码细节（`rows_removed_by_filter`、`ExecParallelHashTableAlloc` 等） | 教学简化，无对应源码条目 | **未核验**（不作为实现依据） |

## 对我们的三条直接结论（写进设计时才展开）

1. **拉取模型 + 行式**是两引擎的共同底座，也是我们的取法（方案 §14 既定）；
   **批量化只在"扫描与回表内部"**（区读 + §9.4 批量回表 + 每块一次 CR），
   对外保持行流——这是避开向量化复杂度又拿到 I/O 收益的折中。
2. **Top-N 的边界要说清**（证据 4）：堆只限内存，输入照扫；真正能短路的只有
   "无 ORDER BY 的 LIMIT"与"可短路谓词"。
3. **自适应/增量项一律"按需启用"**（Memoize、Incremental Sort、Skip Scan）：
   材料给的是机制与收益面，触发条件由我们的实测给（本库一贯纪律：
   先测量后优化）。
