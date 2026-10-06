# bicdb-sql

Raw AST、Binder、逻辑/物理计划、执行器

| 项 | 值 |
| --- | --- |
| 设计依据 | `doc/SQL前端设计_v0.1.md`（草案，待评审）；`doc/spec/SQL.md`（REQ-SQL-001…011） |
| 对应阶段 | P5 |
| 当前状态 | **v0.2**——切片 **S1 已落地**，且**形状对齐 PostgreSQL**（用户口径 2026-10-06）：`lexer`（token + 字节区间；关键字闭集；引号标识符；未引号名照 PG 折叠小写）+ `ast`（Raw AST **同名同形于 PG 解析节点**；不 import 目录）+ `parser`（**照 PG 的产生式删减**；优先级表照抄 gram.y） |

## 状态说明

- **五阶段**（REQ-SQL-001）：文本 → ① Raw AST → ② Bound Query → ③ 逻辑 →
  ④ 物理计划 → ⑤ 执行。**①②③④ 是编译，⑤ 是执行**。
- **本切片（S1）**：词法 / 语法 / Raw AST——**对齐 PostgreSQL**（简化子集）：
  - 文法 = PG 文法的**删减**、AST = PG 解析节点的**裁剪**（`parsenodes.h`/
    `primnodes.h` REL_16_STABLE；映射表见设计 §3.1）；**优先级表照抄 gram.y**
    ——含两个反直觉点：`BETWEEN`/`IN` 比比较更紧、集合运算各有一级
    （`UNION`/`EXCEPT` 同级、`INTERSECT` 更紧）；
  - 词法：**引号标识符**（大小写敏感）；未引号名**照 PG 折叠小写**；
    数字含 `.5`/`1e3` 形态、**只记原文本**（值的解析在 ② 走 TYP 内核）；
  - 闭集纪律（REQ-SQL-006）：子查询/CTE/窗口/`RIGHT JOIN`/`RETURNING`/
    `LIKE`/列约束/`INSERT…SELECT`/`NULLS FIRST`等**语法层没有产生式**；
  - 错误带**字节区间**；`parse_many` 按 `;` 分句。
- **未落地**：Binder（②）、逻辑表示与变换（③）、物理计划（④）、计划缓存、
  执行接口——切片 S2–S8 见设计 §10；`GRAPH_TABLE` 随图域接入（**响亮拒绝**，
  不静默当普通表名）。

实现进度请以仓库根 `README.md` 的阶段表为准。
