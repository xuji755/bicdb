# 主题 → 命令映射

全部命令于 2026-10-03 实测有效。执行前先 `source /u01/deepseek/scripts/env.sh && cd /u01/deepseek/scripts`。

用法：按主题找到对应行 → 跑命令 → 结果不理想就按"检索纪律"第 1 条换词改写 → 下钻核验。

## 存储与页结构（P2）

| 主题 | 命令 |
| --- | --- |
| 槽位页结构、页目录分裂 | `kb_graph.py mysql problem 'InnoDB 页结构 B+树 分裂 redo log 恢复'` |
| 页校验和实现 | `kb_graph.py postgresql entity all '源码 实现 heapam 页面结构'` |
| 跨页行 / 大行外联 | `kb_graph.py postgresql problem 'TOAST 大字段 外联存储 压缩'` |
| 行迁移与行链接 | `kb_graph.py oracle entity knowledge '行链接 行迁移 链式行'` |
| 空闲空间管理与页复用 | `kb_graph.py postgresql entity knowledge 'FSM 空闲空间映射 页面复用'` |
| Oracle 逻辑存储（块/区/段） | `kb_graph.py oracle entity knowledge '块 区 段 逻辑存储结构'` |

> 行迁移条目实测给出：行标志位（H/F/L/P/N）、转发指针 nrid、`CHAIN_CNT`、`PCTFREE` 处理方式——直接对应本项目跨页行与行片段设计。

## 事务、WAL 与恢复（P3）

| 主题 | 命令 |
| --- | --- |
| 全页写与部分页保护 | `kb_graph.py postgresql problem 'WAL full_page_writes 部分页写入 崩溃恢复'` |
| 检查点与重做起点 | `kb_graph.py postgresql problem '检查点 checkpoint 恢复 重做 起点'` |
| Undo 与一致性读 | `kb_graph.py oracle entity knowledge 'Undo 一致性读 前镜像'` |
| Redo 循环写与 LSN | `kb_graph.py mysql problem 'InnoDB redo log 循环写 LSN 恢复'` |
| 快照过旧 / Undo 耗尽 | `kb_graph.py oracle problem 'ORA-01555 快照过旧'` |
| 故障注入与恢复演练 | `kb_graph.py postgresql problem '故障注入 崩溃恢复 演练'` |

## 并发与索引（P4）

| 主题 | 命令 |
| --- | --- |
| B-Tree 并发分裂协议 | `kb_graph.py postgresql problem 'B-Tree 页分裂 Lehman Yao 并发 索引'` |
| GIN 索引结构与并发 | `kb_graph.py postgresql entity knowledge 'GIN 源码 关键结构'` |
| MVCC 可见性判定 | `kb_graph.py postgresql problem 'MVCC 多版本 可见性'` |
| 唯一索引并发冲突 | `kb_graph.py postgresql entity knowledge '唯一索引 并发 插入 冲突'` |
| 死锁与锁等待 | `kb_graph.py oracle problem '死锁 ORA-00060 锁等待'` |
| latch 闩锁争用 | `kb_graph.py oracle problem 'latch 闩锁 争用 自旋'` |

> 实测命中 Lehman-Yao 协议与 right-link 并发分裂；唯一索引条目给出 `_bt_check_unique` 与源码文件位置。

## SQL 引擎与 JSON（P5）

| 主题 | 命令 |
| --- | --- |
| 解析/语义分析分层 | `kb_graph.py postgresql entity knowledge '解析器 原始语法树 语义分析 绑定'` |
| 执行计划算子 | `kb_graph.py oracle entity operator 'NESTED LOOPS'`（换算子名） |
| JSON 类型与路径索引 | `kb_graph.py postgresql problem 'JSON 类型 路径 索引 表达式'` |
| 计划缓存与失效 | `kb_graph.py ob problem '执行计划缓存 失效 绑定变量'` |
| PL/SQL 对象与包 | `plsql_kg.py oracle detail --catalog-name <对象> --catalog-kind function --version 11gR2` |
| PL/SQL 上下文补查 | `plsql_kg.py oracle context --catalog <类型:对象> --query '<问题>'` |

## 类型与 Oracle 兼容（P0、P5）

| 主题 | 命令 |
| --- | --- |
| NUMBER 精度与标度 | `kb_graph.py oracle entity knowledge 'NUMBER 精度 标度 舍入'` |
| 空字符串与 NULL 语义 | `kb_graph.py oracle problem '空字符串 NULL 语义'` |
| NLS 与字符集 | `doc_retrieve.py 'NLS_LANG AL32UTF8' --db oracle` |

## 隔离、资产与记忆（P1、P6）

| 主题 | 命令 |
| --- | --- |
| 权限模型（对比用） | `kb_graph.py oracle entity knowledge '权限 角色 对象权限 最小权限'` |
| 路径穿越与权限攻击 | `kb_graph.py linux problem '符号链接 路径穿越 权限 提权'` |
| 资源隔离与配额 | `kb_graph.py ob problem '资源隔离 租户 资源单元 配额'` |
| 大对象外部化一致性 | `kb_graph.py mysql problem '大对象 外部文件 一致性 校验'` |
| 级联删除与依赖 | `kb_graph.py postgresql entity knowledge '级联删除 依赖 引用完整性'` |
| TTL 与过期清理 | `kb_graph.py redis problem 'TTL 过期 淘汰 惰性删除'` |
| 幂等与重试 | `kb_graph.py postgresql entity knowledge '幂等 事务 重试 唯一键'` |

## 检索与向量 ANN（P7、P8、P9）

| 主题 | 命令 |
| --- | --- |
| pgvector 源码全貌 | `kb_graph.py postgresql entity knowledge 'pgvector 源码学习'` |
| 距离度量与操作符类 | `kb_graph.py postgresql entity knowledge 'pgvector 距离 余弦 内积 精度'` |
| HNSW 数据结构 | `kb_graph.py postgresql entity knowledge 'HNSW关键数据结构'` |
| HNSW 页级锁分工 | `kb_graph.py postgresql entity knowledge 'pgvector HNSW 插入 图结构 邻居选择'` |
| HNSW 删除与修图 | `kb_graph.py postgresql entity knowledge 'HNSW VACUUM 修复图 三阶段'` |
| HNSW/IVFFlat 页结构 | `kb_graph.py postgresql entity knowledge 'HNSW页面结构 PASE 实现'` |
| IVFFlat 训练与探测 | `kb_graph.py postgresql entity knowledge 'IVFFlat 训练 nprobe 列表'` |
| 中文分词 | `kb_graph.py postgresql entity knowledge 'pg_tokenizer 源码学习'` |
| 倒排与 BM25 | `kb_graph.py postgresql entity knowledge 'VectorChord BM25 倒排列表'` |
| 混合检索融合 | `kb_graph.py postgresql problem '混合检索 融合 排序 RRF'` |
| 磁盘型 ANN（后续） | `kb_graph.py postgresql entity knowledge 'pgvectorscale 源码学习 StreamingDiskANN'` |
| 取正文 | `doc_retrieve.py 'HNSW' --db postgresql` |

> **注意**：基础 `HNSW` 概念条目是**占位内容**，不可采用。ANN 结论必须落到"源码学习"系列条目。

## 属性图（P10）

| 主题 | 命令 |
| --- | --- |
| AGE 源码结构 | `kb_graph.py postgresql entity knowledge 'Apache AGE 源码仓库 文件结构'` |
| openCypher 标准 | `kb_graph.py postgresql entity knowledge 'openCypher CREATE MATCH RETURN'` |
| DuckPGQ 图查询 | `kb_graph.py postgresql entity knowledge 'DuckPGQ 源码学习 核心概念'` |
| 有界遍历与预算 | `kb_graph.py postgresql problem '图遍历 变长路径 深度限制'` |

## 运维诊断（辅助）

| 主题 | 命令 |
| --- | --- |
| 综合分析（实体+诊断链+影响） | `kb_graph.py <db> analyze '<问题描述>'` |
| 分层诊断 | `kb_graph.py <db> diagnose '<问题描述>'` |
| 故障模型检索 | `kb_graph.py <db> fault '<故障关键词>'` |
| 诊断传导链展开 | `kb_graph.py <db> diagnostic '<精确故障别名>'` |
| 深度关联证据 | `kg_deep_diagnose.py <db> '<关键词>' --limit 10 --format json` |
| 日志/错误码规范化后检索 | `kg_log_search.py --db <db> --stdin` |
| 备份一致性 | `kb_graph.py oracle problem 'RMAN 备份 校验 一致性'` |
| 磁盘满与慢 fsync | `kb_graph.py postgresql problem '磁盘满 fsync 慢 数据页 刷盘'` |

## 系统对象查询（需要版本/范围）

**`--scope` 的取值语义随数据库与实体类型变化**，传错不会报参数错误，而是返回"未找到同时符合版本和范围条件的对象"——容易被误读为"知识库没有这个数据"。

```bash
# Oracle：scope 为容器语义（PDB / CDB_ROOT / NON_CDB）
kb_graph.py oracle entity view 'DBA_TABLESPACES' --version 19c --scope PDB
kb_graph.py oracle entity param 'shared_pool_size' --version 19c
kb_graph.py oracle entity function '<用途或名称>' --version 19c

# OceanBase 系统对象：scope 为【租户类型】ORACLE / MYSQL
kb_graph.py ob entity view 'SYS.GV$OB_MEMORY' --version 4.2.1 --scope ORACLE
kb_graph.py ob entity function '计算数值的绝对值' --version 4.2.1 --scope MYSQL

# OceanBase OpSQL：scope 为【目录范围标签】SYS / MYSQL
kb_graph.py ob entity opsqllist --version 4.2.1
kb_graph.py ob entity opsql '合并阻塞' --version 4.2.1 --scope SYS

# MySQL：兼容解析 --scope 但不据此筛选
kb_graph.py mysql entity view 'information_schema.TABLES'
```

> 视图名必须 `schema.` 限定（如 `SYS.GV$OB_MEMORY`、`SYS.DBA_TABLESPACES`），裸名会报"匹配多个对象"。
> 不确定 scope 取值时：先不传试一次（OpSQL 不传可正常返回），或先跑 `opsqllist` 看该条目的实际范围列。

## 批量与效率

```bash
# 多关键词一次查（Rust 后台可并发，按输入顺序合并）
kb_graph.py <db> batch '<词1>' '<词2>' '<词3>'

# 缓存与耗时
health_check.py --cache-stats
```

图谱更新后需清理 daemon 缓存（`health_check.py --clear-cache`，**属变更运行时状态**，由维护者执行）。
