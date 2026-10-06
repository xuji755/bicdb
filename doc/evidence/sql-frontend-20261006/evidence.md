# 证据包：SQL 前端设计依据（sql-frontend-20261006）

- **日期**：2026-10-06
- **用途**：`doc/SQL前端设计_v0.1.md` 的取法依据（计划缓存与绑定变量的成熟经验）
- **检索环境**：BIC-QA 知识库（daemon 正常），命令与原始输出见 `raw/`
- **覆盖声明**：检索**按召回预算选取、未宣称全库穷尽**；未取到正文的条目一律标"未核验"

---

## 1 检索清单（命令 → 原始文件）

| # | 命令 | 文件 | 结果 |
| --- | --- | --- | --- |
| 01 | `kb_graph.py postgresql problem '计划缓存 generic plan custom plan CachedPlanSource 预编译语句'` | `raw/01-pg-plancache.txt` | 命中 6 条（KnowledgeEntry ×4 / CaseStudy ×1 / FaultModel ×1） |
| 02 | `detail 965656`（generic plan） | `raw/02-pg-generic-plan.txt` | **薄条目**——只有一句"含缓存与失效逻辑，源码 plancache.c"，无机制细节 |
| 03 | `detail 1488138`（倾斜 ⇒ generic plan 非最优） | `raw/03-pg-case-skew.txt` | 案例：倾斜下 generic 选全扫、custom 选索引 |
| 04 | `detail 1404768`（元数据直改 ⇒ 缓存不更新） | `raw/04-pg-fault-cache.txt` | 故障模型：绕过正常路径改元数据，缓存不失效 |
| 05 | `doc_retrieve.py 'generic plan custom plan plancache 选择'` | `raw/05-pg-plancache-doc.txt` | 取回"添加索引性能下降和 generic plan"全文（博客，非官方） |
| 06 | `kb_graph.py postgresql entity all '源码 实现 plancache 计划缓存'` | 无输出 | **源码学习系列未覆盖 plancache**——记"未核验" |
| 07 | `kb_graph.py oracle problem '软解析 硬解析 共享游标 库缓存 绑定变量窥探'` | `raw/07-oracle-parse.txt` | 命中 6 条（窥探机制 / 游标共享 / ACS / 硬解析触发 / 案例 / 故障模型） |
| 08 | `detail 3259400`（绑定变量窥探机制与局限） | `raw/08-oracle-peeking.txt` | 窥探只在**首次硬解析**发生；此后复用共享游标不再窥探 |
| 09 | `detail 2064559`（ACS） | `raw/09-oracle-acs.txt` | 11g 自适应游标共享"基本解决"窥探问题，但引入新问题 |
| 10 | `doc_retrieve.py 'custom plan generic plan 预备语句 计划选择'` | `raw/10-pg-plan-choice.txt` | 仅元数据/摘要（PG14 Internals 第 16 章条目存在，正文未展开） |

---

## 2 采信要点（进设计）

| # | 结论 | 出处（elementId / 文件） | 用途 |
| --- | --- | --- | --- |
| E1 | **Oracle 绑定变量窥探只在首次硬解析进行**；此后复用共享游标不再窥探，计划固定为首次窥探值对应的计划——**优化器无法随后续值变化做出最佳选择**（可能索引扫/全表扫二选一，取决于第一次的值） | `4:8ed6c541…:3259400`（raw/08）；互证 `…:3259296`、`…:1556317` | 我们 V1.0 **不做按值窥探**的反面依据：宁可"对参数化形态做确定的规则选择"，也不做"第一次值定终身"的隐式选择 |
| E2 | **11g 自适应游标共享（ACS）"基本解决"窥探问题，但本身引入新问题**；遇到绑定变量 SQL 要先检查列数据分布 | `…:2064559`（raw/09） | ACS 属**代价模型 + 直方图**的产物 ⇒ 明确列为后续切片（触发条件：统计面提供直方图与选择率），本期不引入 |
| E3 | **计划不稳定的故障模型**：窥探 + 数据倾斜 ⇒ 计划异常（错误的索引/全表选择）、响应时间波动 | `…:1545752`（FaultModel）、`…:1545972`（案例） | 我们以"**参数化计划 + 对象版本比对失效**"绕开该故障类；并在验收里显式构造"同语句不同参数值"用例 |
| E4 | **PG generic plan 与 custom plan 的取舍受数据倾斜影响**：倾斜列上 generic 可能选坏计划而 custom 正确 | `…:1488138`（raw/03） | 同样的结论面；进一步支持 V1.0 不做按值择优（无直方图时 custom 也"择优"不了） |
| E5 | **元数据被绕过正常路径直接修改 ⇒ 预编译语句缓存不更新** | `…:1404768`（raw/04） | 支撑 REQ-SQL-009 的"**失效靠比对不靠通知**"：只要版本源（mtime/status/generation）是唯一事实源，就不存在"忘发通知"的漏 |
| E6 | PG 查询历经 **解析 → 转换（analyze）→ 规划 → 执行**四段；简单协议一次收发 | PG14 Internals 第 16 章（KB 收录条目；正文未展开，摘要级） | 与 REQ-SQL-001 的五阶段同形（我们把"转换"拆成绑定与逻辑两阶）；**仅作形状对照** |
| E7 | 计划缓存实现于 `src/backend/utils/cache/plancache.c`（generic plan 的缓存与失效逻辑所在） | `…:965656`（raw/02，薄条目） | 只作**源码锚点**引用；其选择阈值/内部算法**未取到正文** |

## 3 未核验项（**不抄入设计**）

| 项 | 状态 |
| --- | --- |
| PG "custom plan 先跑 N 次再比较 generic" 的具体阈值与判据 | **未核验**——KB 未取到正文（raw/06 空、raw/10 仅摘要）。本设计**不引用**该阈值；V1.0 干脆不做 custom/generic 选择 |
| PG plancache 的失效协议细节（relcache invalidation 具体链路） | **未核验**——源码学习系列未覆盖；raw/04 只给故障现象 |
| Oracle 库缓存的 LRU/latch 细节 | **未检索**（本轮范围外；计划缓存的实现形态属本项目自定，见设计 §7） |
| Oracle 除法舍入（既有未核验项） | 与本包无关，见 `exec-ops-20261006` |
