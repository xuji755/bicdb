# 证据包：扫描机制与并发（Oracle 对照）— 2026-10-05

**用途**：回答"表/索引扫描（全表、索引全扫、快速全扫、范围）、多块读、
回表批量、缓冲池并发、redo 并发、提交确认"这批设计问题（问题 1–9），
为 P4/P5 设计切片备料。**只作证据，不作决定。**

## 检索范围（已跑命令见 `raw/`）

| # | 命令 | 文件 |
| --- | --- | --- |
| 01 | `health_check.py --daemon` | `raw/01-health.txt` |
| 02 | `kb_graph.py oracle problem '高水位 HWM 全表扫描 段'` | `raw/02-hwm.txt` |
| 03 | `problem '多块读 全表扫描 数据块 I/O 读取'` | `raw/03-multiblock.txt` |
| 04 | `problem '索引快速全扫描 索引全扫描 范围扫描 区别'` | `raw/04-index-scan.txt` |
| 05 | `problem '直接路径读 全表扫描 绕过 缓冲池'` | `raw/05-direct-path.txt` |
| 06 | `problem '索引扫描 回表 一致性读 数据块 批量 顺序'` | `raw/06-rowid-batch.txt` |
| 07 | `problem 'cache buffers chains latch 争用 热块 buffer busy waits'` | `raw/07-latch.txt` |
| 08 | `problem 'redo 私有 strand log buffer latch 并发 提交'` | `raw/08-redo-strand.txt` |
| 09 | `problem '提交 确认 客户端 log file sync LGWR 写入 返回'` | `raw/09-commit-ack.txt` |
| 10 | `detail` × 8（见下表的 elementId 末段） | `raw/10-detail-<id>.txt` |

## 证据行（均经 `detail` 下钻核验正文）

| elementId 末段 | 来源 | 要点 |
| --- | --- | --- |
| 203391 | Table Access Full | 全表扫描 = **多块读**，每次块数由 `db_file_multiblock_read_count` 调；**串行**全表扫描把块读入缓冲池 **LRU 端**；**并行**扫描不在池中时走**直接 I/O 读入 PGA**；>约 10% 行数才划算；**HWM = 表曾用过的最后一个块，删除不降、空块仍被扫** |
| 1559730 | Db file scattered read 与全表扫描 | 多块连续读取对应等待事件；边界：单次块数多时等待总时长仍可能高 |
| 246255 | 索引快速全扫描（FFS） | 读**所有块（含分支，仅用叶）**、**可并行**、机制**类似全表扫描**；7.3 引入；范围扫描**不可并行**（块非顺序） |
| 3259347 | 索引全扫描 vs FFS | 索引全扫描以"索引序"产出（可用于避免排序）；FFS 不保序 |
| 1054817 | 索引扫描 | 查询只碰索引列 ⇒ **不回表**（覆盖索引） |
| 2065847 | 集群因子 | **只影响需要回表的范围扫描与索引全扫描**；不回表/返回少量行时可忽略 |
| 1995236 / 1993822 | Note 75709.1 rowid 物理存储 | B*Tree 叶块存 **6B 受限 rowid = 数据块地址 + 槽号**（V8 扩展 rowid 10B 是伪列格式） |
| 228665 | 唯一索引回表 CPU 成本 | 回表按 **`data_block_size`** 计固定 CPU 成本——回表成本按"块"记账 |
| 1567548 | 直接路径读入 PGA | 大表全表扫描**绕过缓冲池**、避免缓存污染；**小表相对缓存尺寸不再直读**（有阈值） |
| 1995752 | `TABLE_SCANS_DIRECT_READ` 统计 | 直读**不进 buffer cache**（kds.h/kdstsd） |
| 2040012 | Redo Parallelism && IMU | 高并发下 redo allocation latch 争用 ⇒ **shared strand**：log buffer 划多块、**每 strand 单独 latch**（`_log_parallelism_max/dynamic`）；10g private strand：shared pool 里 **65K 私有内存**，事务直接写其中 |
| 2046002 | Frits Hoogland：redo part 11 | 私有 strand = 进程独占缓冲，**提交时批量刷入公共 strand** 再写联机日志；大事务需提前刷；`X$KTIFF` 查残留 |
| 2042654 | Private Strand Flush Not Complete | 切换时私有 strand 未全部并入 ⇒ 切换等待（strand 机制的已知代价面） |
| 2047423 | log file sync 故障排除 | 提交等待 = 会话等 **LGWR 把 log buffer 写盘并收到确认**；与 `log file parallel write`（LGWR 实际 I/O）关联 |
| 2049392 | lfsdiag.sql | 提交相关的 `commit_logging` / `commit_wait` / `commit_write` 参数；ASH 里 log file sync 对应 LGWR 的 parallel write |
| 2045104 / 2049006 | latch 争用 TAR | `cache buffers chains` / `cache buffers lru chain` latch 争用 + `free buffer waits` 的实例（多为 SR 标题，正文有限） |

## 结论（供设计引用；"未核验"项不得当证据）

1. **HWM 语义与我们 §4.3 定案一致**：已格式化边界、删除不降、全表扫描包含
   空块；**全表扫描的终止条件就是 HWM**（HWM 之上未格式化、不可读）。
2. **多块读**是 Oracle 全表扫描与 FFS 的共同机制（`db_file_multiblock_read_count`）；
   单块读留给索引路径（1559730/246255）。我们尚无多块读——而"区 = 8 页 =
   128 KB 物理连续"（§4/§5.11 预留式布局）天然就是多块读单元。
3. **三种扫描的定位**：范围扫描（有序、不可并行、需回表，受聚簇因子影响）；
   索引全扫描（有序、沿叶链）；FFS（无序、可并行、读全部块仅用叶、像全表扫描）。
   我们**只设计了范围扫描**（叶双向链 + B-link，§9.1.x）；IFS/FFS 未设计。
4. **回表按块记账**（228665）且 rowid 自带 DBA+槽（1995236）⇒ 批量回表 =
   按 DBA 排序后每块一次读取；**我们的 CR 是块级重建**，同块多行还能共享一次
   链回放——比 Oracle 的收益更大（详见答复 §6）。
5. **大扫描的缓存污染**：Oracle 用直接路径读绕池（阈值控制，1567548）；
   我们用 touch-count 三秒规则避免一次性扫描升热（§5.10 已定"一次性扫描
   不该污染热段"），是否再引入"扫描直读"待定案。
6. **latch 分片**：Oracle 把 hash 桶分给多把 latch（8i 形态，§5.10 已引
   kcbz.h）；我们已定 N=1 起步、P4 按**工作区**分区（争用边界 = 工作区，
   与 Oracle"按硬件分"不同，§5.10）。
7. **redo 并发**：Oracle 走 shared/private strand（每 strand 一把 latch、
   私有缓冲、提交归并）；我们 §11.5.5 已定**不引入 strand**（无 IMU、事务短），
   P4 只做"latch 内分配位置/占槽、latch 外拷贝"（kcrfwr 两把闩锁）+ 组提交；
   且**每工作区独立的日志组集**天然按工作区分片。
8. **提交确认**：`log file sync` 的语义 = 等记录写盘并被确认（2047423），
   与我们的"提交点 = 提交记录 fsync"逐字同义。

## 未核验 / 存疑

- "回表前按 rowid 排序批量"无直接 KB 条目（按 1995236 + 228665 推理，
  与 Oracle 优化器行为一致但未取到内部机制原文）。
- latch 争用条目多为 TAR/案例标题，正文有限；桶/latch 的结构细节以既有
  证据包 `buffercache-mech-20261005/`（kcbz.h、Note 33439.1 等）为准。
- 直接路径读的**阈值参数**（`_small_table_threshold` 等）未检索；若引入
  再补。
- 结论 3 的"IFS 可避免排序"来自 3259347 摘要，正文未逐句核验。
