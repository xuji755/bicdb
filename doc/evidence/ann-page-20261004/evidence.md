# 证据包：ANN 索引页的失效与并发定案（pgvector 参照）

模块：存储架构 §5.11（ANN 索引页）/ §9.3.3（页面组织）　对应：待冻结项 #8（ANN 部分）
核验日期：2026-10-04　原始输出：`raw/01–05`（同目录）

## 结论

1. pgvector 的 HNSW 物理结构为三段（**metapage / element tuple = 向量数据 + heap TID /
   neighbor tuple = 邻居 ID 数组**），此前已被本设计吸收并留一处改动（**邻居不内联 element、
   独立成定长槽**——解开"单页记录上限"对维度的压制）。本次下钻补到**失效与清理机制**：
   VACUUM 三阶段——`RemoveHeapTids`（扫元素页、删失效堆 TID、建删除哈希表）→ `RepairGraph`
   （先处理入口点、再逐元素重找邻居并更新**双向连接**）→ `MarkDeleted`（**清空向量数据、
   失效邻居连接、递增元素版本号——4 位、到 15 回绕**）。
2. 并发与运维三件事：`HNSW_UPDATE_LOCK` 位于**页面 0**（协调插入与修复）、`HNSW_SCAN_LOCK`
   位于**页面 1**（协调扫描与删除）；VACUUM 扫描用 `BAS_BULKREAD` 防污染缓冲池；
   **修复 vs 重建的判据——删除 < 10% 用 VACUUM、> 20% 推荐 `REINDEX CONCURRENTLY`**。
3. element / neighbor 元组的**字节级布局**在知识库里未展开（有界召回，多次换词未命中）——
   不影响定案：结构决策已有，字节格式本就由我们自主定义（§5.11 定长槽方案），
   本次**照 pgvector 的版本号手法补 2 字节**：`flags 1B`（失效）+ `version 1B`（递增，语义同
   pgvector 的 4 位回绕；我们不抠 4 位）。

## 证据

| # | 来源类型 | 条目 ID（前缀 `4:8ed6c541-fe08-4ade-ae68-a79bf45316f4:`） | 数据库 | 主题 | 核验日期 | 适用条件 |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | 知识库 KnowledgeEntry | `:980090` | PostgreSQL | pgvector HNSW 物理结构：metapage / element tuple（数据+heap TID）/ neighbor tuple | 2026-10-04 | 结构总览 |
| 2 | 知识库 KnowledgeEntry | `:367047` | PostgreSQL | **三阶段 VACUUM 全文** + 两把锁（**页 0** UPDATE / **页 1** SCAN）+ `BAS_BULKREAD` + REINDEX/VACUUM 判据（10% / 20%） | 2026-10-04 | **失效与并发** |
| 3 | 知识库 KnowledgeEntry | `:980108` | PostgreSQL | pgvector 总览：IVFFlat 与 HNSW | 2026-10-04 | 背景 |
| 4 | 知识库 KnowledgeEntry（CaseStudy） | `:1474421` | PostgreSQL | 构建内存溢出案例（10 万条后超出 maintenance_work_mem）——§9.3.2 ① 的实践证据 | 2026-10-04 | 背景 |

## 检索范围声明

- 已检索（2026-10-04，4 轮 problem + 2 次 detail，原始输出存 `raw/01–05`）：
  `'pgvector HNSW 页 结构 metapage element tuple neighbor tuple'`、
  `'pgvector 元素元组 层级 删除标记 堆 TID 向量数据'`、
  `'pgvector neighbor tuple element tuple 布局 字段 大小'`、
  `'pgvector HNSW 插入页 空闲空间 新元素 分配 metapage insertPage'`；detail `:367047` / `:980090`。
  命中取 4 条。**按召回预算有界，非全库穷尽。**
- 未命中/占位：element/neighbor **元组字节级布局**（多轮措辞均未展开）；
  `insertPage`（插入页空闲空间跟踪）无直接条目；`HNSW 最大层级`实现常量未检索。

## 冲突与未决

| 项 | 知识库说法 | 官方文档/源码说法 | 处置 |
| --- | --- | --- | --- |
| 字节级元组布局 | 未展开 | — | 不影响：结构决策已定（三段式 + 定长槽），字节格式**本项目自主定义**（§5.11） |
| 失效元素的处置 | pgvector：修图（VACUUM 三阶段）或重建（> 20%） | — | **我们取"不修图"**：置失效 + 回查过滤 + 阈值重建（见自主决策 2） |
| 插入页跟踪 | pgvector 在 metapage 维护 `insertPage` | — | 我们复用通用空间管理：段头"插入提示" + 段内位图页（§5.11），不新增机制 |

## 本项目自主决策

1. **element 槽补 `flags 1B` + `version 1B`**（照 pgvector 的版本号手法）：源行删除或向量更新时
   **置失效 + 版本递增**；**扫描者读到元素后复验版本/失效位**（检出扫描期间被并发失效的元素）。
2. **更新语义 = "插入新元素入图 + 旧元素置失效"**——不原地改向量、不修图；
   正确性由**回查**（`spec/RET.md` REQ-RET-023）保证；退化到阈值即**重建**
   （判据参照 pgvector 的 10% / 20%）。
3. **不设全图锁**（对照 pgvector 的页 0 / 页 1 两把锁）：页级 latch 照通用规则；
   跨页的**图级不一致可容忍**——回查过滤 + 完整性清单兜底。
4. **WAL 立场**：ANN 页受 WAL 保护（防半页写与链断裂），但**图级一致性不依赖 redo 的原子边界**——
   崩溃后由 `(generation, 源提交水位, 完整性清单)` 判定"可用 / 重建"。
5. **入口点更新的既有文字保持**（§5.11：删除入口点时选最高层级的非入口元素）——
   与 pgvector 的 `RepairGraph`"先处理入口点"一致，互为印证。

## 未核验项

- pgvector 元素版本号的**位级载体**（4 位存放于何字段）未下钻——本设计取 1B，语义同构即可。
- HNSW 的最大层级常量、`insertPage` 的选取算法未检索——本设计以 §5.11 既有空间管理兜住。
