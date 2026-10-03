---
name: db-principle-query
description: 用 BIC-QA 知识库检索数据库原理、源码实现与故障经验，并产出可入档的证据包。当需要了解 Oracle/PostgreSQL/MySQL/OceanBase 等数据库的内部机制或源码设计（如 MVCC 可见性、WAL 与崩溃恢复、Undo 一致性读、页结构与跨页行、B+Tree 并发分裂、HNSW/IVFFlat 向量索引、属性图、中文分词），或为 bicdb 各阶段任务（P0–P12）准备设计依据时使用。触发词：查原理、知识库、KB、源码学习、证据包、某机制怎么实现、某结构怎么设计。
---

# 数据库原理查询（BIC-QA 知识库）

用知识库检索数据库**原理、源码实现与故障经验**，并把结论沉淀成**可入档、可复核的证据包**。

知识库是**证据来源之一，不是决策者**。它用于快速定位"哪个文件、哪个函数、哪个机制"，最终实现细节以冻结源码与官方文档为准。

权威依据：`doc/知识库使用计划与方案_v0.2.md`（命令实测记录见 `doc/evidence/kbcheck-20261003/`）。

## 何时用 / 何时不用

**用**：
- 引入本项目尚无先例的机制或协议（新索引结构、新恢复路径、新并发协议）
- Oracle 兼容语义的边界（类型、比较、转换、空值）
- 设计出现"两种做法取舍"、需要外部经验证据时
- 为某个模块/协议准备证据包

**不用**：
- 结论已冻结在既有证据包中 → 直接引用，不重复检索
- 纯本项目内部约定（工作区隔离契约、目录结构、错误码定义）
- 已有明确官方文档可直接引用 → 直接引官方文档更权威

## 环境准备

每次会话首次调用前执行：

```bash
source /u01/deepseek/scripts/env.sh
cd /u01/deepseek/scripts
python3 health_check.py --daemon        # 确认 daemon 可用
```

若 daemon 不可达或许可证失效：**降级**——只做已有冻结证据覆盖的独立任务并标记"未核验"，新机制类任务暂停，不得以记忆或推测代替证据。

## 工作流

### 第 1 步：定位知识域与阶段

选定数据库域（**不可跨域替代**，语法相似也不行）：

`oracle`=2101　`mysql`=2102　`postgresql`=2104（pg）　`oceanbase`=2203（ob）　`linux`=1111

选域原则：Oracle 机制看 oracle；存储/并发/ANN/图优先 postgresql（源码学习系列最丰富）；InnoDB 页结构看 mysql；LSM/租户看 ob。

### 第 2 步：检索（先窄后宽）

按顺序尝试，不理想就换措辞：

```bash
# ① 机制/原理（三路召回 + 重排，最常用）
python3 kb_graph.py <db> problem '<机制> <关键词>'

# ② 已知主题名
python3 kb_graph.py <db> entity knowledge '<主题>'

# ③ 源码实现（命中"源码学习"系列的关键入口）
python3 kb_graph.py <db> entity all '源码 实现 <结构名>'

# ④ 取正文（汇总条目只有摘要，正文在这里）
python3 doc_retrieve.py '<关键词>' --db <db>

# ⑤ 下钻到具体条目（必做，见第 3 步）
python3 kb_graph.py <db> detail '<elementId>'
```

主题 → 命令的完整映射（含各阶段已验证命令）见 `references/query-map.md`。

### 第 3 步：下钻核验（强制）

**进入结论的每一条，必须 `detail` 打开原始条目或 `doc_retrieve` 取正文。** 检索输出的摘要/排序分不算证据。

核验时确认三件事：
1. **不是占位内容**——若出现"该条目为占位内容""需参考相关文档"，丢弃该条，换"源码学习"系列条目
2. **有出处**——条目是否给出源码文件、函数名、文档链接
3. **适用条件**——版本、范围、前提、已知失效场景

### 第 4 步：产出证据包

按 `references/evidence-package.md` 的模板输出，落档到 `doc/evidence/<模块>-<日期>/`。

**冲突必须记录，不得静默择一**：知识库说法与官方文档/源码不一致时，开 ADR 并在证据包"冲突与未决"中留痕。

## 检索纪律

1. **措辞敏感，务必改写重试**——实测：`'B-Tree 页分裂 Lehman Yao 并发 索引'` 命中良好；`'行迁移 行链接 跨页行 大行存储'` 首位命中的是无关 MOS Note。**多关键词 + 中英词变体 + 换词重试**，一次不中就改写，不要判定"知识库没有"。
2. **中英双查**——同一概念分别用中文术语与英文术语检索（"一致性读"与"consistent read"）。
3. **源码类走专门入口**——普通 `entity knowledge` 常只返回概念摘要；用 `entity all '源码 实现 ...'` 或检索"源码学习"才能拿到实现级细节。
4. **召回有界，不得过度声明**——工具自述"候选覆盖有界…未宣称全库穷尽"。证据包写"已检索范围"，不写"已穷尽全部资料"。
5. **版本/范围显式且语义正确**——见下表陷阱 ②③。
6. **不重复全库检索**——已冻结证据不重跑；同一问题一次证据包。
7. **检索失败即记录**——无结果或仅占位内容时标"未核验"，不推断、不补写。

## 已知陷阱与用法修正

以下均为 2026-10-03 实测（54 条命令），**原写法会失败或误判**：

| # | 错误写法 / 误判 | 正确做法 | 现象 |
| --- | --- | --- | --- |
| ① | `plsql_full_check.py --db oracle --sql '...'` | `plsql_full_check.py oracle --sql '...'`（数据库是**位置参数**） | 返回 `{"status":"input_error","message":"first argument must be database"}` |
| ② | OB 视图 `--scope SYS` | `--scope ORACLE`（**OB 的 scope 取值随实体类型不同**，见下方说明） | "未找到同时符合版本和范围条件的对象"——**易被误读为"知识库没这个数据"，实为 scope 传错** |
| ③ | `entity view 'GV$OB_MEMORY'`（裸名） | `'SYS.GV$OB_MEMORY'`（**schema 限定**；单引号防 shell 展开 `$`） | "匹配多个对象，请用 schema.name 限定" |
| ④ | `entity knowledge '<主题>' --limit N` | 去掉 `--limit`（该子命令不支持） | "当前命令不支持参数：--limit" |
| ⑤ | 引用 `entity view` 的字段总数 | 落到**单一版本/范围组合**分节再引用 | 工具自带声明："合并字段总数（跨版本/范围并集，**不代表单个版本列数**）" |
| ⑥ | 把摘要/排序分当证据 | 必须 `detail` 下钻 | 汇总条目只有摘要与目录，正文在单篇条目/`DocText` |

**OceanBase 的 `--scope` 取值随实体类型不同**（实测确认，易踩）：

| 实体类型 | scope 取值 | 实测 |
| --- | --- | --- |
| 系统对象（view / table / column / function / param） | **租户类型**：`ORACLE` / `MYSQL` | `entity view 'SYS.GV$OB_MEMORY' --scope ORACLE` ✓；`--scope SYS` ✗ |
| OpSQL（`opsql` / `opsqllist`） | **OpSQL 目录范围标签**：`SYS` / `MYSQL` / `ORACLE` | `entity opsql '合并阻塞' --scope SYS` ✓；`--scope ORACLE` ✗ |

不确定时：**先不传 `--scope` 试一次**（OpSQL 不传 scope 可正常返回），或先跑 `opsqllist` 看该条目的实际范围列，再带上正确取值。Oracle 的 scope 是容器语义（`PDB`/`CDB_ROOT`/`NON_CDB`），MySQL 兼容解析 `--scope` 但不据此筛选。

**能力缺口（不要依赖）**：`equivalent` 因 `EQUIVALENT_TO` 边未构建而**不可用**；`discover` 默认无输出；`impact` 需实体可解析；`diagnostic` 需**精确故障别名**（先用 `fault '<关键词>'` 找别名）。

## 高价值知识系列

知识库中最值得优先检索的是**源码学习系列**，实测已确认存在：

| 系列 | 对本项目的对应 |
| --- | --- |
| pgvector 源码学习（26 篇） | HNSW / IVFFlat 实现（P8、P9 主参考） |
| VectorChord / pgvectorscale | 向量索引、BM25 倒排、磁盘型 ANN |
| DuckDB / Milvus（31 篇） | 向量化执行、检索架构 |
| pg_tokenizer（29 篇） | 中文分词（P7） |
| DuckPGQ / Apache AGE | 属性图与 openCypher（P10） |
| SQLite B-Tree / InnoDB 页结构 | 槽位页、页目录分裂（P2） |
| PostgreSQL `heapam.c`/`checksum.c`/GIN | 堆表、页校验、索引并发（P2、P4） |

## 输出规范

- 面向用户回答时：先给**可判定结论**，再列**证据与出处**，最后标**未核验项**
- 每条结论附 `elementId` 或文档路径，便于复核
- 明确区分来源类型：Oracle 机制 / PostgreSQL 方法 / AGE 能力 / ANN 算法 / **本项目自主决策**
- 未检索到的部分如实标注，不用推测补全

## 参考文件

- `references/query-map.md`——各阶段/主题 → 已验证命令映射
- `references/evidence-package.md`——证据包模板、填写规范与完整示例
- 计划文档第 2 节——能力边界（强项与弱项全表）
- `doc/evidence/kbcheck-20261003/`——54 条命令实测原始输出，可复跑
