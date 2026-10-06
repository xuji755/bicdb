# SQL 前端设计 v0.1

> **状态**：草案（2026-10-06），**待评审**——评审通过前不进入编码。
> **上位规格**：`doc/spec/SQL.md`（REQ-SQL-001…011）、`doc/spec/ENG.md`
> （REQ-ENG-005/006）、`doc/spec/TYP.md`、`doc/spec/GRP.md`。
> **下游**：`doc/执行算子设计_v0.1.md`（物理计划的算子闭集与 WMM）。
> **依据**：证据包 `doc/evidence/sql-frontend-20261006/`（计划缓存与绑定变量的
> 成熟经验；未核验项见该包 §3）。

---

## 0 边界（先划死）

**本设计覆盖**：REQ-SQL-001 的 ①–④（编译四阶）+ **计划缓存** + 与执行器的接口
（ENG REQ-ENG-005 的 `compile`/`execute`/`cancel` 在 SQL 侧的形态）。

| 不在本设计 | 归属 |
| --- | --- |
| DDL 的**写侧**（目录表写入、对象锁、DDL 事务） | `bicdb-catalog` 详设（本设计只定 Binder/执行器需要的**只读接口契约**，§4.1） |
| 执行算子本体、WMM、temp 溢出 | `执行算子设计_v0.1.md`（已完成，§7 全落地） |
| 协议层语句表示、参数通道、结果流格式 | `API` 域（spec/SQL.md 待定项） |
| 图语言**解析** | `GRP` 域（其产物在本设计的 ② 之后汇入同一管线） |

**闭集纪律**：REQ-SQL-006 的不提供清单（子查询/CTE/窗口/JIT/并行/列存算子…）
**在语法上不存在**——解析器没有这些产生式，不是"解析后再拒绝"。

**零依赖纪律**（沿用全仓约定）：不引外部 crate；词法/语法**手写**
（生成器=新依赖+构建复杂度，且我们的语法是**闭集**，手写可控）。

---

## 1 取法与依据（一句话各表）

| 决策 | 取法 | 依据 |
| --- | --- | --- |
| 五阶段与"编译/执行"分界 | 照 REQ-SQL-001（与 PG 解析→转换→规划→执行同形，我们把"转换"拆为绑定与逻辑两阶） | spec/SQL.md §0.1；KB 条目 PG14 Internals 第 16 章（形状对照，E6） |
| **不做按值窥探、不做按值重优化**（V1.0） | 参数化计划 + 规则选路；**不**看参数值选计划 | 证据 E1/E2/E3/E4：窥探"第一次值定终身"是故障源；ACS 要直方图与代价模型才成立——**我们没有**（统计切片当前只给 entries/leaf_blocks/blevel/CF） |
| 缓存失效**靠比对不靠通知** | 键含对象/索引/配置版本；命中前比对 | REQ-SQL-009；证据 E5（"绕过正常路径改元数据 ⇒ 缓存不更新"的故障类被"唯一版本源"消掉） |
| 两层缓存：实例级 Raw AST + 工作区级其余 | 文本→AST 与工作区无关（安全），Bound 起入工作区键 | REQ-SQL-002 / REQ-SQL-009 的同一条线 |
| **语法与 AST 形状对齐 PG（简化子集）** | 文法 = PG 文法的**删减**；AST = PG 解析节点的**裁剪**（同名同形）；优先级表照抄 PG | **用户口径 2026-10-06**；取证 `evidence/sql-frontend-20261006/raw/12-pg-parser-source.txt`（REL_16_STABLE） |
| 手写词法/语法（不用生成器） | 递归下降、每个 PG 产生式对应一个解析函数；闭集语法 | 零依赖纪律；对齐后「手写」= 按 gram.y 逐条对照，成本低 |
| Catalog 只读、DDL 独立事务 | 目录无写接口；写走 DDL 路径 | ENG REQ-ENG-006；`arch/03` §3.1（字典表=普通表，DDL 天然事务化） |

**与 Oracle/PG 的刻意不同**（记档，防"为什么不像 X"）：

| | Oracle | PG | 我们 |
| --- | --- | --- | --- |
| 计划选择 | 代价 + 直方图 + 窥探（ACS 收敛） | custom/generic 自动择优（阈值细节 KB 未核验） | **规则 + 索引统计的最小代价**；无直方图、无窥探（§6.4） |
| 计划缓存键 | SQL 文本 + 环境（optimizer env / 绑定变量集） | 查询文本 + 参数类型 + 对象版本（relcache 失效） | REQ-SQL-009 的显式元组（**工作区 ID 是首项**） |
| 缓存失效 | 依赖链 + 失效通知（library cache pin/lock 体系） | relcache invalidation 广播 | **比对**：每次命中前比版本，不广播 |

---

## 2 模块与代码落点

```text
crates/sql/                      （bicdb-sql；本设计的实现落点）
  src/lexer.rs        词法：token 流 + 位置；关键字闭集
  src/parser.rs       语法：递归下降 + Pratt 表达式；语句分派
  src/ast.rs          Raw AST（**不 import 任何目录接口**；依赖检查验收）
  src/bind/           绑定：名字解析 / 类型推导 / 参数定型 / 写目标检查 / 登记点
  src/logical.rs      逻辑表示（关系代数形状）
  src/rewrite.rs      白名单变换（REQ-SQL-007 闭集）
  src/plan.rs         物理计划：选路 + 映射到 bicdb-exec::PlanNode
  src/cache.rs        两层缓存（实例级 AST / 工作区级计划）+ 键与失效
  src/session.rs      compile/execute/cancel 的会话侧装配（ENG REQ-ENG-005）
crates/catalog/                  （bicdb-catalog；只读接口在本设计定契约，写侧另详设）
   resolve / type_descriptor / object_version（ENG REQ-ENG-006）
```

**依赖方向**（ENG 不变量）：

```text
sql ──读──▶ catalog ──读──▶ storage(表访问服务)
sql ──▶ types（TYP 内核：类型规则/比较/转换）
sql ──▶ exec（只构建 PlanNode；**不反向依赖**）
ast ──✗──▶ catalog（REQ-SQL-002：AST 模块不得 import 目录接口）
```

---

## 3 ① 词法与语法（Raw AST）——**形状对齐 PostgreSQL（简化子集）**

> **用户口径（2026-10-06）**：SQL 语法**从 PG 的语法中简化**、**语法树直接兼容
> PG 语法**、解析器开发**学习 PG 的解析器与源码**——以"照抄现成文法"替代"自造
> 文法"，简化整个开发。取证：`doc/evidence/sql-frontend-20261006/raw/12-pg-parser-source.txt`
> （PG **REL_16_STABLE** 的 `gram.y` / `parsenodes.h` / `primnodes.h` 摘录 + 代理下载命令）。

**三条对齐规则**：

1. **文法 = PG 文法的删减**：保留 REQ-SQL-005 正面清单所需的产生式，删掉清单外
   构造（子查询/CTE/窗口/…）。**删减不改变保留下来的产生式的形状**——
   删的是产生式，不是重新发明。
2. **AST = PG 解析节点的裁剪**：同名同形（字段名用 Rust 命名风格，结构一一对应），
   去掉与本库面无关的字段（`intoClause`/`windowClause`/`withClause`/
   `returningList`/`onConflictClause`/继承与分区字段…）。**后续加构造时，
   把 PG 的对应字段与产生式一并搬回**。
3. **词法对齐**：`'…'`（`''` 转义）、**`"…"` 引号标识符**、数字
   `digits[.digits][e[+-]digits]`（**只记原文本**）、`--` 与 `/* */` 注释、
   `::` 转型。**标识符折叠照 PG**：未引号 ⇒ **小写折叠**（② 应用）；引号内 ⇒
   **原样保留、大小写敏感**。

### 3.1 节点映射表（PG → 我们）

| PG 节点 | 我们的类型 | 裁剪点 |
| --- | --- | --- |
| `RawStmt` | `Stmt`（枚举，每个变体带 `span`） | `stmt_location/stmt_len` ⇒ 字节区间 |
| `SelectStmt` | `SelectStmt` | `distinctClause`（列表，含 `DISTINCT ON`）⇒ `distinct: bool`（**无 `DISTINCT ON`**）；去 `intoClause`/`windowClause`/`withClause`/`lockingClause`/`groupDistinct`/`limitOption`；**保留** `valuesLists`（`INSERT … VALUES` 走它，照 PG）、`sortClause`/`limitOffset`/`limitCount`、**集合运算的 `op/all/larg/rarg` 左深嵌套**（不设"链"——PG 的形状更通用） |
| `InsertStmt` | `InsertStmt` | 去 `onConflictClause`/`returningList`/`withClause`/`override`；`cols`/`selectStmt` 照 PG |
| `UpdateStmt` | `UpdateStmt` | 去 `fromClause`/`returningList`/`withClause` |
| `DeleteStmt` | `DeleteStmt` | 去 `usingClause`/`returningList`/`withClause` |
| `CreateStmt` | `CreateStmt` | 去 `inhRelations`/`partbound`/`partspec`/`ofTypename`/`constraints`/`oncommit`/`tablespacename`/`accessMethod`/`if_not_exists`；**保留 `options`（`WITH (...)`, `DefElem` 列表）** |
| `ColumnDef` | `ColumnDef` | 只留 `colname`/`typeName`/`is_not_null`（+location）——default/约束/存储/压缩/排序规则在清单外 |
| `IndexStmt` | `IndexStmt` | 只留 `idxname`/`relation`/`indexParams`/`unique` + **`target_kind`（表/图顶点/图边——本库扩展，记档）** |
| `IndexElem` | `IndexElem` | 只留 `name`/`expr`（+location） |
| `DropStmt` | `DropStmt` | `objects`/`removeType`/`missing_ok`；对象类型枚举加 **GRAPH / WORKSPACE（本库扩展，记档）** |
| `RangeVar` | `RangeVar` | 只留 `relname`/`alias`/`location`（无 catalog/schema/inh/relpersistence——本库无模式层） |
| `Alias` | `Alias` | 只留 `aliasname`（无列别名清单） |
| `JoinExpr` | `JoinExpr` | 只留 `jointype`/`larg`/`rarg`/`quals` |
| `ResTarget` | `ResTarget` | 只留 `name`/`val`（+location）；无 `indirection` |
| `SortBy` | `SortBy` | `node`/`sortby_dir`/`sortby_nulls`——**`nulls` 字段留、语法暂不开 `NULLS FIRST/LAST`**（清单外；留字段使将来加入零成本） |
| `A_Expr` | `AExpr` | `kind`/`name`/`lexpr`/`rexpr`（+location）；我们有的 kind：`OP`/`IN`/`NOT_IN`/`BETWEEN`/`NOT_BETWEEN`/`NULLIF`（**`NULLIF` 走 `A_Expr` 而非独立节点——照 PG**）。向量距离 `<->`/`<=>`/`<#>` 就是 `OP` 名字 |
| `BoolExpr` | `BoolExpr` | `boolop`（AND/OR/NOT）/`args`（+location）——**`NOT` 是一元 `BoolExpr`，照 PG** |
| `NullTest` | `NullTest` | `arg`/`nulltesttype`（IS_NULL/IS_NOT_NULL） |
| `FuncCall` | `FuncCall` | `funcname`/`args`/`agg_star`/`agg_distinct`（+location）——`COALESCE` 有独立节点（见下），其余函数（`json_*` 等）走它 |
| `TypeCast` | `TypeCast` | `arg`/`typeName`（+location）——`CAST(x AS t)` 与 **`x::t`** 同一个节点（照 PG） |
| `CaseExpr`/`CaseWhen` | 同名 | 去 `casetype`/`casecollid` |
| `CoalesceExpr` | `CoalesceExpr` | `args`（`COALESCE` 有独立节点——照 PG） |
| `ColumnRef` | `ColumnRef` | `fields: Vec<ColumnRefField>`（`Name(String)` 或 `AStar`）+location |
| `A_Const` | `AConst` | `value: ConstValue`（`Int/Float/String/Bool`）+ **`isnull`**；**数字留原文本**（PG 的 `Float` 亦存文本；本库 `NUMBER` 38 位，原文本最稳） |
| `TypeName` | `TypeName` | `names`（列表）⇒ `name: String`（无模式限定）；`typmods` ⇒ 原文本列表；+location |
| `DefElem` | `DefElem` | `defname`/`arg: DefElemArg`（`Const(AConst)` 或 `Ident(String)`——PG 的 `arg` 是任意 Node，我们收窄；理由：`table_type = memory` 的裸标识符值是本库写法，PG 的 reloptions 只收字面量） |
| `TransactionStmt` | `TransactionStmt` | `kind`（BEGIN/COMMIT/ROLLBACK） |
| `VariableSetStmt` | `VariableSetStmt` | `ALTER SESSION SET/CLEAR`（PG 的 `SET` 与 `ALTER SESSION SET` 同节点）；参数白名单在 ② 判（《DCL语句设计》§1.3） |
| `AlterSystemStmt` | `AlterSystemStmt` | `ADD/DROP/ALTER FILESYSTEM`（**本库扩展**，形状仿 PG 同类 DDL 节点）——《DCL语句设计》§1.2 |
| `AlterDatabaseStmt` | `AlterDatabaseStmt` | 只含 `CLONE WORKSPACE`；**本库实例即一个库** ⇒ 不带库名（记档）——《DCL语句设计》§1.1 W2 |
| `ParamRef` | `ParamRef` | **`:name`（本库规格），非 PG 的 `$n`**——**唯一的刻意偏离**，理由 = REQ-SQL-005 明定":name，类型绑定期推导" |
| —（无 PG 对应） | `CreateGraphStmt` / `CreateWorkspaceStmt` / `AlterWorkspaceStmt` / `DropWorkspaceStmt` | 本库扩展（图/工作区 DDL）；**记档**：形状仿 PG 同类 DDL 节点（`RangeVar` + 选项列表） |

### 3.2 词法（`lexer.rs`）

| 词素 | 规则 | 归属 |
| --- | --- | --- |
| 标识符 | 字母/下划线起，后续含 `$`（保留规则在 ② 判） | ①（**原文本**；折叠在 ②） |
| 引号标识符 | `"…"`（`""` 转义）——**大小写敏感，不折叠** | ① |
| 关键字 | **闭集**（REQ-SQL-005 清单所需）；与标识符同形，按闭集判定 | ① |
| 数字字面量 | `digits[.digits][e[+-]digits]`——**只记原文本** | ①记/②解（TYP 内核） |
| 字符串字面量 | `'…'`，`''` 转义（**已解转义**的字节） | ① |
| 参数 | `:name` | ① |
| 操作符 | `+ - * / = <> < <= > >= :: <-> <=> <#>` | ① |
| 注释 | `--` 行注释、`/* */` 块注释（不嵌套） | ① |
| 位置 | 每个 token 记**字节偏移区间**（PG 记字符偏移；我们用字节——**记档差异**） | ① |

**长度上限**：语句文本、标识符、字面量各设上限（数值随 P0 数值项；超限报
**语法错误**，不截断）。

### 3.3 语法（`parser.rs`）——优先级照抄 PG

```text
（低 → 高；PG 的 %left/%right/%nonassoc 声明，摘取证 §1，简化到我们有的操作符）
UNION EXCEPT  <  INTERSECT  <  OR  <  AND  <  NOT（右结合）  <
IS / ISNULL（nonassoc）  <  < > = <= >= <>（nonassoc）  <
BETWEEN / IN / NOT（nonassoc）  <
多字符操作符（<-> <=> <#>）  <  + -  <  * /  <
一元负号（UMINUS，右结合）  <  ::  <  .
```

**两处与旧草案的差异（照 PG 修正）**：
- **`BETWEEN` / `IN` 比比较运算绑定更紧**（PG 的声明顺序如此，非直觉）；
- **集合运算有自己的优先级**（`UNION`/`EXCEPT` 同级、`INTERSECT` 更紧）——
  链式与父子形态由 `larg/rarg` 左深嵌套表达（照 PG）。

- **手写递归下降**：每个优先级一层函数；表达式按上表；每个 PG 产生式
  一一对应一个解析函数（便于对照 gram.y 复核）。
- **语句闭集**（REQ-SQL-005 正面清单，逐条有产生式）：DDL（表/索引/图/工作区）、
  DML（SELECT 家族/INSERT/UPDATE/DELETE）、事务控制、**DCL 管理语句**
  （工作区生命周期 / 文件系统池 / 会话参数——详设见
  `doc/DCL语句设计_v0.1.md`）；图语言独立入口（GRP 域）。
- **清单外即语法错误**（REQ-SQL-006）：`WITH`/`EXISTS`/窗口/`RETURNING`/`RIGHT JOIN`/
  列约束…在词法/语法层没有产生式——报错文案指向"不支持该构造"。

### 3.4 收益（"简化整个开发"的落点）

| # | 收益 |
| --- | --- |
| 1 | **文法不自造**：PG 的 `gram.y` 是现成、经充分验证的产生式集合——我们的工作是**删减**而不是发明 |
| 2 | **加构造零设计成本**：子查询/CTE/窗口/`EXPLAIN` 等在后续版本加入时，照抄 PG 的对应产生式与节点字段 |
| 3 | **②③ 的分工照 PG**：绑定/变换阶段直接对照 `parse_*.c`（`analyze.c`/`parse_clause.c`/`parse_expr.c`/`parse_target.c`/`parse_relation.c`）的职责划分（KB 条目 `356528`） |
| 4 | **差分对象更强**：与 PG 的树形状对齐后，"我们的解析器对不对"可以直接拿 PG 的 `nodeToString` 形状做人工对照 |

## 4 ② Binder（`bind/`）

绑定六动作按序（REQ-SQL-003）；错误一律**绑定期**报出。

### 4.1 名字解析（动作 1）——三格顺序

```text
① 对象命名空间（obj$，namespace 1=表 / 2=索引）：本人对象 + 预置对象
② 固定表命名空间（保留名清单，不在 obj$）：file$、session$、lock$ …
③ 自举对象（obj# ≤ 99）：**不在会话解析范围内**（引擎内部用）
```

- **保留规则**：以 `$` 结尾的名字、预置对象名 —— `CREATE` 一律拒绝（§0.4 spec）。
- **不可区分**：找不到 = "不存在"（不区分别人的/已删的/从未存在的）。
- **只读入口**：`public` 的显式只读入口（ISO §13.5 过滤层）。
- **写目标解析只有第 ① 格**——固定表**没有写入口**（不是检查，是查不到）。

**Catalog 契约**（本设计对 `bicdb-catalog` 的只读要求，ENG REQ-ENG-006）：

```text
resolve(名字, 命名空间) -> ObjectRef{ obj#, kind, dataobj#, 属主 }
type_descriptor(obj#, 列号) -> 类型描述子（TYP 内核的形态）
object_version(obj#) -> mtime（最后修改的提交序号；计划缓存键的成分）
列枚举(obj#) -> [(列名, 列号, 类型, NOT NULL)]      // SELECT * / INSERT 无列名时用
索引枚举(表 obj#) -> [(索引 obj#, 键列, 唯一性, status)]
```

### 4.2 版本捕获（动作 2）

每个解析到的对象记 `(obj#, mtime)`；每个用到的索引记 `(obj#, mtime, status)`
（ANN 索引另加 generation —— RET REQ-RET-022）。**这些就是缓存键的成分**。

### 4.3 类型推导（动作 3）与参数定型（动作 4）

- 全部走 **TYP 内核**（REQ-SQL-010）：比较器、转换表、NULL 三值逻辑，
  **前端不实现任何独立的类型规则**（含字面量解析：`NUMBER` 文本在 ② 走内核）。
- **参数 `:name` 的类型由用法推导**：出现在 `col = :p` ⇒ 取 col 类型；推导不出
  （如 `SELECT :p`）⇒ **拒绝**（"参数无类型上下文"）。同一参数多处使用必须推导出
  **同一类型**，否则绑定期错误。
- **Bound Query 形状**：

```text
BoundQuery {
  expr:     类型化表达式树（每个节点带类型；参数节点带 (名, 类型)）
  objects:  对象引用集 {(obj#, mtime)}
  indexes:  索引引用集 {(obj#, mtime, status[, generation])}
  params:   参数清单 [(名, 类型)]（**顺序即执行期参数序**）
  shapes:   结果列形状（列名 + 类型 —— 协议层元数据的来源）
  writes:   写目标（表 obj# + 列号集）+ 登记点清单
}
```

### 4.4 写目标检查（动作 5）与登记点（动作 6）

- 写目标不得是：固定表 / `asset$` / `ref$` / 自举对象 / `public`（写侧只有标准接口）。
- **登记点标记**：写含**受管引用值**的列时，标出同一事务内维护 `ref$` 的位置
  （AST/GRP 的规则）；**投影裁剪不得裁掉登记点**（REQ-SQL-007）。

---

## 5 ③ 逻辑表示与变换

### 5.1 逻辑形状（关系代数 + DML）

```text
LogicalPlan := Scan(表, 列投影) | Filter | Join{inner|left, on}
             | Project | Aggregate{group, aggs} | Sort | Limit
             | SetOp{union|intersect|except, all} | Values
             | Insert|Update|Delete
```

语义与 `执行算子设计` §2 的算子闭集**一一对应但不含物理决策**（不选索引、
不选连接算法、不定排序实现）。

### 5.2 变换白名单（REQ-SQL-007 闭集——白名单之外不做，哪怕"显然正确"）

| 变换 | 约束 |
| --- | --- |
| 谓词下推 | 不得越过外连接；不得推到可能产 NULL 的行集一侧（除非谓词可证不受 NULL 影响） |
| 常量折叠 | 折叠失败（除零等）⇒ **绑定期错误**（允许出错变早，不允许变没/变晚） |
| 等值传递 | 仅 `=` 且两侧类型相同（TYP 内核判定） |
| 投影裁剪 | 不得裁掉可能报错、或带登记点的表达式 |
| 连接顺序 | 只在**内连接**之间重排（外连接位置固定） |
| 访问路径选择 | 全扫 / B+Tree / 表达式索引 / 图扩展 / ANN——只影响代价，不影响结果 |

**四条不变量**（差分验收判据）：NULL 三值逻辑逐点保持；外连接不退化；
类型转换点不移动；错误行为不变（可变早）。

---

## 6 ④ 物理计划

### 6.1 目标：`bicdb-exec` 的 `PlanNode` 闭集（已完成，直接映射）

| 逻辑形态 | 物理节点（exec） |
| --- | --- |
| Scan | `SeqScan` / `IndexScan{low,high,covered,batch}` / `FastFullScan`（统计类）/ `IndexOnlyScan`（覆盖=IndexScan covered） |
| Filter / Project / Limit | 同名节点 |
| Join | `NestedLoop`（参数化重扫）/ `HashJoin`（等值，INNER/LEFT） |
| Aggregate | `HashAgg` / `SortedAgg`；无 GROUP BY ⇒ `ScalarAgg` |
| Sort / LIMIT+ORDER | `Sort` / `TopN` |
| 去重/集合 | `Sort`+`Unique`（UNION/DISTINCT）；`Append`（UNION ALL）；`SetOp`（INTERSECT/EXCEPT 归并） |
| DML | `Insert` / `Update` / `Delete` + `WithRowId`（源行带 ROWID 前缀） |

**计划不可变**（REQ-SQL-004）：节点里**没有值**（无字面量的运行期结果、无游标位置）；
执行态（参数值、游标、哈希表、取消标志）全在单次执行对象里。

### 6.2 访问路径选择（V1.0：规则 + 索引统计的最小代价）

**可用统计**（#43 已落地）：每索引 `entries`、`leaf_blocks`、`blevel`、
`clustering_factor`（CF）。**没有**直方图、没有表级行数统计。

```text
规则（按序，先命中先用）：
① 谓词含某索引**全部键列**的等值 ⇒ 索引（唯一索引 ⇒ 唯一扫描；非唯一 ⇒ 范围）
② 谓词含索引**前导列的等值/范围** ⇒ 估算行数 r ≈ entries × sel：
     sel（无直方图）= 等值：1 / 不同值估计（本版取 1/10 保守）; 范围：1/3（启发式）
   代价 ≈ blevel × c_page + r × c_rowid + (r × CF / entries) × c_block
   vs 全扫代价 ≈ table_blocks × c_block   —— 取小者
③ 覆盖可用（投影列 ⊆ 索引列 ∪ 键）⇒ 覆盖扫描（无回表）
④ 否则全扫
```

**口径写死**：① 无直方图 ⇒ 均匀分布假设（**不许**用参数值窥探绕过——见 §1）；
② 代价常数（c_page/c_rowid/c_block）取**相对值**（只需比较大小）；
③ 统计缺失（索引从未统计过）⇒ 一律全扫（**确定**行为，不是随机）。

### 6.3 连接 / 聚合 / 排序 / 集合的选择规则

| 决策 | 规则 |
| --- | --- |
| Join（等值、INNER） | 内表在连接键上有可用索引且外表估算小 ⇒ `NestedLoop`（参数化重扫）；否则 `HashJoin`（构建侧 = 估算小的一侧） |
| Join（等值、LEFT） | `HashJoin`（构建侧 = **右/内**侧，LEFT 的补 NULL 语义在算子内）；内表有唯一索引且外表小 ⇒ `NestedLoop` |
| Join（非等值） | `NestedLoop`（INNER/LEFT） |
| 聚合 | 输入已按分组键有序（索引序/上游 Sort）⇒ `SortedAgg`；否则 `HashAgg` |
| DISTINCT 聚合 | 修饰，不换算子（`AggSpec.distinct`） |
| ORDER BY + LIMIT | `TopN`（输入仍全读——设计 §4.1）；仅 ORDER BY ⇒ `Sort` |
| SELECT DISTINCT | `Sort`（键=投影列）+ `Unique` |
| 集合运算 | `UNION ALL` ⇒ `Append`；`UNION` ⇒ `Append` + `Sort` + `Unique`；`INTERSECT [ALL]`/`EXCEPT [ALL]` ⇒ `Sort` 两侧 + `SetOp` |

### 6.4 刻意不做的（本期，触发条件写死）

| 不做 | 触发条件 |
| --- | --- |
| 直方图驱动选择率、代价模型标定 | 统计切片 2（列级统计）落地后 |
| **按参数值窥探/择优**（custom plan 形态） | 直方图 + 代价模型齐备后（证据 E1–E4：没有这两样，窥探只有害） |
| 自适应计划（执行中换计划、ACS 形态） | 同上 + 实测有必要的热点 |
| `Memoize` / `IncrementalSort` / `SkipScan` / `Materialize` | 执行算子设计 §4.3 的触发条件原样 |

---

## 7 计划缓存（`cache.rs`；REQ-SQL-009）

### 7.1 两层

```text
L1（实例级）：语句文本（规范化后）→ Raw AST
             ——与工作区无关（AST 不含对象引用；REQ-SQL-002 的安全面）
L2（工作区级）：键（下表）→ CompiledPlan{BoundQuery, 物理计划, 结果形状}
             ——**跨工作区不共享**（键的首项就是工作区 ID）
```

### 7.2 缓存键（元组，任一不匹配即未命中）

| 成分 | 来源 |
| --- | --- |
| **工作区 ID** | `WorkspaceContext`（首项——隔离） |
| 规范化语句文本 | L1 产物（字面量归一化：字面量 → 占位；**文本归一化不改变语义**） |
| 参数类型签名 | ② 推导的参数类型列表 |
| 对象版本集合 | ② 捕获的每个 `(obj#, mtime)` |
| 类型版本 | TYP 内核版本标识 |
| 索引引用 | `(obj#, mtime, status)`；ANN 另加 generation |
| 相关配置版本 | 参与编译决策的配置项 revision（CFG） |

**失效 = 比对失败**：不广播、不依赖反转；建/删索引、表选项修改、Move 致失效
⇒ 都落在 `mtime/status/generation` 上，**自然失配**（证据 E5 的反面：
没有"该发通知而没发"的可能）。

### 7.3 生命周期与并发（REQ-SQL-004）

- 计划对象**不可变**（`Arc<CompiledPlan>`）；缓存按 **LRU** 驱逐（上限=实例参数，
  数值随 P0）；驱逐只减缓存，不影响在跑执行（执行持有 `Arc`）。
- **每次执行新造执行态**：参数值、游标、WMM 内存区、取消标志都在执行对象里；
  N 个会话并发执行同一计划 ⇒ 无共享可写内存（TSan 验收）。

### 7.4 参数化与字面量（记档）

- **字面量按占位归一**（`WHERE id = 3` 与 `= 4` 命中同一计划）——键里的"规范化文本"
  即此形态；归一规则与常量折叠联动（折叠后仍为常量的不占位）。
- 归一**不得**改变语义：`IN (…)` 列表长度、`LIMIT n` 的值**参与文本**（不占位）——
  它们影响计划形状；只归一**比较/算术位置的标量字面量**。

---

## 8 与执行的接口（ENG REQ-ENG-005 的 SQL 侧）

```text
compile(会话上下文, 语句文本) -> Arc<CompiledPlan> + 结果形状
   ①→④（缓存命中则跳到返回）；错误在此报出（语法/名字/类型/权限）
execute(CompiledPlan, 类型化参数值, 请求标签, deadline, 取消句柄)
   -> 行批次流 + 结束状态
   - 参数值类型必须与 ② 推导一致（不符 ⇒ 执行前拒绝，不进入执行）
   - 事务语义：默认自动提交（每条语句一事务）；BEGIN 起显式事务；
     DDL 在活动事务中拒绝（TXN REQ-TXN-016）
   - **请求标签透传至 commit**（REQ-ENG-002）
   - 结果**流式**（算子拉取），批次大小归协议层；SQL 层不提供游标语句
cancel(执行句柄) -> 释放锁/页引用/临时空间（走 `ExecContext` 的取消/截止路径）
```

**扩展面汇入**（REQ-SQL-008）：图语言与 ANN/表值函数在 ② 之后**转换为受控计划
节点**（带 `workspace_id`）；执行期对 ANN 索引 generation/水位**复核一次**
（编译到执行之间可失效 ⇒ 回退全扫，不报错）。

---

## 9 错误与阶段归属（REQ-SQL-001 的验收面）

| 错误类 | 阶段 | 形态 |
| --- | --- | --- |
| 语法错误 / 清单外构造 | ① | 带**字节偏移区间**；"不支持该构造"指向 REQ-SQL-006 |
| 名字不存在 / 类型不匹配 / 参数无上下文 / 写固定表 / 写 public | ② | 一律绑定期；名字不可区分 |
| 常量折叠失败（除零） | ② | 允许"出错变早" |
| 约束、等待超时、资源、溢出上限 | ⑤ | 按 CONV §4 分类；`ExecError` 保真外传 |
| **DDL 资格（仅 admin）** | ②**之前** | 资格检查**先于对象查找**（否则"不存在"与"不是你的"之间的差别是一次探测） |

---

## 10 实施切片（编码顺序；每片可独立验收）

| 片 | 内容 | 验收 |
| --- | --- | --- |
| **S1** ✅ **已落地（2026-10-06；含 PG 对齐返工）** | 词法 + 语法 + Raw AST（L1 缓存随 S6） | ✅ `crates/sql` **v0.2**：`lexer`（token + 字节区间；关键字闭集；**引号标识符**；未引号名照 PG 折叠小写；数字含 `.5`/`1e3` 形态）+ `ast`（**同名同形于 PG 解析节点**的 Raw AST——`SelectStmt`/`InsertStmt`/`AExpr`/`BoolExpr`/`NullTest`/`FuncCall`/`TypeCast`/`CaseExpr`/`ColumnRef`/`AConst`/`RangeVar`/`JoinExpr`/`ResTarget`/`SortBy`/`IndexStmt`/`DropStmt`…，映射表 §3.1）+ `parser`（**照 PG 的产生式删减**、优先级表照抄 gram.y、集合运算 `op/all/larg/rarg` 左深嵌套）。**用例 21**：闭集语料 62 条全解析、**清单外 24 条零接受**（新增 `INSERT…SELECT`/`NULLS FIRST`/`DROP…IF EXISTS`）、**PG 形状逐点断言**（`*` 是 `ColumnRef[AStar]`、`IN`/`BETWEEN`/`NULLIF` 走 `A_Expr`、`::` 与 `CAST` 同节点、`VALUES` 走 `values_lists`）、优先级三例（含 `a = 1 BETWEEN 2 AND 3` ⇒ `a = (1 BETWEEN …)` 与 `INTERSECT` 更紧）、折叠两例（未引号小写/引号保留） |
| **S2** | Catalog 只读面 + 名字解析三格 + 版本捕获 | 跨区名不可区分；保留名拒绝；`(obj#, mtime)` 被记入 Bound（断点查验） |
| **S3** | Binder：类型推导 / 参数定型 / 写目标 / 登记点 | 类型错误全在绑定期；参数推导失败拒绝；写固定表/public 拒绝 |
| **S4** | 逻辑表示 + 白名单变换 | 与**直译执行器**两路差分（无优化 vs 优化，逐行一致）；四条不变量各有用例 |
| **S5** | 物理计划 + 接入 `bicdb-exec` | 全算子闭集覆盖；同一语义计划的开/关优化结果一致；真表端到端（含 DML 与快照） |
| **S6** | 计划缓存 + 失效 | DROP INDEX / 改表选项 / Move ⇒ 键失配重编译；跨工作区不共享；并发执行同计划互不干扰 |
| **S7** | 语句面收尾：DDL/DML/事务控制 + 工作区 DDL + **DCL**（工作区生命周期/文件系统池/会话参数——`doc/DCL语句设计_v0.1.md`） | 资格先于对象查找；DDL 在活动中拒绝；`BEGIN/COMMIT/ROLLBACK` 语义；DCL 的闭集拒绝（未知动作/未知参数各一例） |
| **S8** | 语料与验收：≥1000 手写 + 1 万生成查询的两路差分 | REQ-SQL-005/007 的验收原文 |

**依赖**：S2 需要 `bicdb-catalog` 的只读面（其写侧/DDL 与 S7 并行推进，
另详设）；S5 复用已落地的算子与 WMM。

---

## 11 本期暂缓（非永久不做）+ 触发条件

| 项 | 触发条件 |
| --- | --- |
| 子查询 / CTE / 递归 / 窗口函数 | 后续版本切片（REQ-SQL-006 闭集外；语法入口随设计加入） |
| 直方图选择率 / 完整代价模型 / 计划标定 | 统计切片 2 之后 |
| 按值窥探（custom plan）/ ACS 形态自适应 | 同上（证据 E1–E4 的教训：没有直方图与代价模型时，窥探只带来不稳定） |
| `EXPLAIN` 类诊断 | OPS 域（V1.0 不提供） |
| 视图 / `ALTER TABLE` / 约束 | 存储行格式演进问题（需求已定：删表重建是 V1.0 替代路径） |

---

## 附：与既有文档的同步点

- `doc/spec/SQL.md`：本设计是其 ①–④ 的工程化落实；**不新增需求**。
  REQ-SQL-009 的缓存键成分逐项落到 §7.2。
- `doc/执行算子设计_v0.1.md`：§6.1 的映射逐项对应其 §2 算子闭集与 §3 计划节点。
- `doc/待讨论清单.md` #48 的执行器余项（SQL 前端）由本设计承接；
  编码启动以本设计**评审通过**为前提。
