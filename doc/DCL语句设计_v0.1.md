# DCL 管理语句设计 v0.1

> **状态**：草案（2026-10-06），**待评审**——评审通过前不进入编码。
> **触发**：用户口径（2026-10-06）——(1) 工作区创建/克隆与**文件系统注册**需要
> 一套 DCL 语法（`CREATE WORKSPACE`、`ALTER SYSTEM ADD FILESYSTEM …`、
> `ALTER DATABASE CLONE WORKSPACE … FROM …`）；(2) 引入**工作区模板**
> （数据结构 + 可有初始化数据），可直接克隆生成特定类型的 workspace
> （`ALTER DATABASE ADD TEMPLATE … FROM …` / `… ALTER WORKSPACE … TO TEMPLATE …`）。
> **上位**：`doc/spec/SQL.md` REQ-SQL-005/006；`doc/arch/02` §2.6（控制文件）/
> §2.8（文件系统池）/§2.11–2.12（命名、创建与克隆）；`doc/arch/03` §3.1.3
> （`user$`/`ws$`/`fs$`）、§3.1.4（固定表）、§3.4（Move）；`doc/目录详设_v0.1.md` §5.1。
> **下游**：`doc/SQL前端设计_v0.1.md`（语法与 AST 形状）、`bicdb-catalog`、`bicdb-daemon`。

---

## 0 边界与共同纪律

**DCL = 管理面语句**：对象是**工作区、模板、实例资源、会话参数**——不是用户数据。
**不进算子管线**（④ 物理计划不产出算子树）：执行落点是**目录写 + 控制文件/文件复制**，
但仍走**同一条 `compile → execute` 通道**（`SQL` §0.2"没有第二条路"：资格、审计、
取消、请求标签透传一个不少）。

| # | 共同纪律 | 依据 |
| --- | --- | --- |
| 1 | **仅 admin**（`ALTER SESSION` 除外——会话自己的参数）；**资格检查先于对象查找** | REQ-SQL-005 |
| 2 | **在 `public` 上执行**（管理元数据 `ws$`/`fs$`/`tpl$` 在那里）；**单事务不跨工作区** | REQ-ISO-005 |
| 3 | **审计留痕**（含请求标签） | `OPS` REQ-OPS-003 |
| 4 | **闭集**：清单外即语法错误（没有产生式） | 本设计 §1/§2 |
| 5 | **语法形状照 PG**（节点同名同形；本库扩展逐条记档） | `SQL前端设计` §3 用户口径 |

---

## 1 语句闭集（V1.0）

### 1.1 工作区生命周期

| # | 语句 | 语义落点 |
| --- | --- | --- |
| W1 | `CREATE WORKSPACE FOR USER <主体> [NAME '<名>']` | 三步协议（`目录详设` §5.1）：建文件 + 引导页 → 建字典（DDL 事务）→ **登记 `public.ws$`（唯一可见性分界）**；`NAME` 缺省 = 跟随属主名 |
| W2 | `ALTER DATABASE CLONE WORKSPACE '<新名>' FROM WORKSPACE <源 id>` <br> `ALTER DATABASE CLONE WORKSPACE '<新名>' FROM TEMPLATE '<模板名>'` | **克隆独立成句**（用户形状）；**两种源**（工作区 / 模板）同一实现：校验**同一 owner**（REQ-ISO-012）/模板可见性 → **暂停写访问窗内 reflink 复制**（`arch/02` §3.4 的窗口协议同形）→ 新工作区登记 `ws$`；`seq$` **原样延续**（§2.12） |
| W3 | `ALTER WORKSPACE <id> SET NAME = '<名>' \| NULL` | 改名；`NULL` = 回到"跟随用户名"（§2.11）；同属主内名字唯一 |
| W4 | `ALTER WORKSPACE <id> SET QUOTA (data=…, undo=…, temp=…, asset=…)` | 四配额按角色分设；**只允许不小于当前占用**（向下越界 ⇒ 具名拒绝，不静默截断） |
| W5 | `DROP WORKSPACE <id>` | 反向三步：**先从 `ws$` 摘除（可见性先断）** → 释放资源 → 删目录；`public` 与模板**不可此路删除** |

> **与现规格的一处替换（评审点①）**：REQ-SQL-005 现写 `CREATE WORKSPACE … [CLONE OF <id>]`。
> 克隆**独立成句**（W2）——一件事不设两个入口，建议删掉 `CLONE OF` 从句，
> 需求文字随评审同步。

### 1.2 实例资源：文件系统池

**池语义以 `arch/02` §2.8 为准**（实例级；加入无前置；**移出前该文件系统上不得有
任何数据文件**——硬条件，先 `Move` 走再移出）；池定义存**全局控制文件**（权威、可重建），
`public.fs$` 供查询/审计。

| # | 语句 | 语义落点 |
| --- | --- | --- |
| F1 | `ALTER SYSTEM ADD FILESYSTEM '<挂载点>'` | 校验：存在、可写、**非符号链接**（`FileIo` 既有纪律）；写**全局控制文件**（双副本·慢路径）+ 插 `public.fs$`（`status = 在池`）；新加入者立即参与后续分配 |
| F2 | `ALTER SYSTEM ALTER FILESYSTEM <槽位> SET ALLOCATE = ON \| OFF` | **退役前的排水阀**：`OFF` = 不再分配新文件、已有文件不动（配合 `Move`） |
| F3 | `ALTER SYSTEM DROP FILESYSTEM <槽位>` | **硬前置**：该盘无任何数据文件（判据 = 全局控制文件的工作区清单 × 各工作区控制文件的完整路径前缀）；通过 ⇒ 全局控制文件更新 + `fs$` 行置 `已移出`（**不删行**——审计友好） |

### 1.3 会话参数（闭合面）

| # | 语句 | 语义落点 |
| --- | --- | --- |
| S1 | `ALTER SESSION SET <参数> = <值>` | **白名单参数**（V1.0 仅两个，皆属 WMM 会话面）：`work_area_size`（设 ⇒ 切 MANUAL、每内存区固定上限）、`max_query_memory`（AUTO 下的会话总量约束）。**≤ 租户配额，超过显式拒绝；值不进计划**（REQ-SQL-001） |
| S2 | `ALTER SESSION CLEAR <参数>` | 回默认（`work_area_size` 清零 = 回 AUTO） |

**纪律**：**任何主体**可用；不写审计；`SHOW` 仍**不提供**（内省走固定表 `session$` 与 API）。

> **与现规格的一处放行（评审点②）**：REQ-SQL-006 现写"会话变量（`SET`/`SHOW`）不提供"。
> WMM 的会话设置（已冻结）需要 `ALTER SESSION SET/CLEAR` 这一**闭合面**——建议改为
> "不提供**通用**会话变量；仅白名单参数"；`SHOW` 维持不提供。

### 1.4 **工作区模板**（用户口径 2026-10-06）

**是什么**：一份**可克隆的工作区快照**——含**数据结构**（字典）与**初始化数据**
（用户表内容）；克隆即得"某种特定类型的工作区"（例：记忆库骨架、项目起始区）。

**两条来源**（用户给出了两种拼写，语义确实不同 ⇒ **两条都收，各自记档**）：

| # | 语句 | 语义 |
| --- | --- | --- |
| T1 | `ALTER DATABASE ADD TEMPLATE '<模板名>' FROM <源 workspace_id>` | **由源区制作模板**：暂停写窗内 reflink 快照 → 模板区；**源区保持可写、不受影响**（"从生产库抽模板"） |
| T2 | `ALTER DATABASE ALTER WORKSPACE <id> TO TEMPLATE '<模板名>'` | **原地转换**：该区 `status → template`；此后**不可作为普通区打开**（"把已建好的区固化为模板"） |
| T3 | `ALTER DATABASE DROP TEMPLATE '<模板名>'` | 删模板（**硬前置**：无任何工作区自它克隆？——**否**：克隆是复制，不建立依赖；直接删，审计留痕） |

**模板的内容与快照纪律**：

| 项 | 规定 |
| --- | --- |
| 复制什么 | **file 0（字典）+ 全部数据文件**（reflink；`arch/02` §2.7） |
| **不复制**什么 | **redo / undo 的内容**——制作点是一个**干净检查点**（暂停写窗内 flush 完成），快照里没有未提交事务；克隆出的新工作区**从该快照的 `file_scn` 起写自己的新日志**（`目录详设` §2.4），undo 段重置 |
| 序号 | 克隆沿用 `seq$` 延续（§2.12）：新区的 ID 分配从快照的值继续（**不重置**，防陈旧引用撞车） |
| 一致性 | 制作/克隆都在**暂停写访问窗**内完成（与 `Move` 同形的窗口协议，`arch/02` §3.4） |

**存储与登记（评审点③）**：

- **登记表**：新增 `public` 字典表 **`tpl$`**——`tpl_id`（主键，序列）/ `name`（**唯一索引**）/
  `source_ws` / `ctime` / `status` / `note`。
  **为什么不复用 `ws$.status`**：`ws$` 的语义是"**可打开的工作区**"（属主、四配额、
  会话、日志、会话配额都在其上）；模板**不可打开**、**不占配额**、**不属任何用户**——
  塞进 `ws$` 会让每一处"列工作区"都要记得过滤，且配额/属主列对模板无意义。
- **模板目录**：实例级模板区（`<池挂载点>/<模板根>/<tpl_id>/…`，布局同工作区目录树）——
  `arch/02` §2.1 的目录布局需**增一节**（评审点一并评审）。
- **权限**：制作/删除/克隆（使用）模板 **都是 admin**（与工作区创建同规）。
  **"普通主体从模板自助建区"** 是另一个需求（放宽 `CREATE WORKSPACE` 的 admin 限制）
  ——**记暂缓**，触发条件：产品确需自助建区时（需求未含）。

---

## 2 DCL 语法

### 2.1 产生式（BNF；照 PG 的形状，本库扩展标记 ★）

```text
-- ── 工作区生命周期 ──────────────────────────────────────────
CreateWorkspaceStmt:
      CREATE WORKSPACE FOR USER 主体名 [ NAME Str ]

★ CloneWorkspaceStmt:
      ALTER DATABASE CLONE WORKSPACE Str FROM WORKSPACE 整数
    | ALTER DATABASE CLONE WORKSPACE Str FROM TEMPLATE  Str

★ AddTemplateStmt:
      ALTER DATABASE ADD TEMPLATE Str FROM 整数

★ WorkspaceToTemplateStmt:
      ALTER DATABASE ALTER WORKSPACE 整数 TO TEMPLATE Str

★ DropTemplateStmt:
      ALTER DATABASE DROP TEMPLATE Str

AlterWorkspaceStmt:
      ALTER WORKSPACE 整数 SET NAME Eq ( Str | NULL )
    | ALTER WORKSPACE 整数 SET QUOTA '(' QuotaItem ( ',' QuotaItem )* ')'
QuotaItem:  ( data | undo | temp | asset ) Eq 整数

DropWorkspaceStmt:
      DROP WORKSPACE 整数

-- ── 文件系统池（★ 全部为本库扩展）──────────────────────────
★ AlterSystemStmt:
      ALTER SYSTEM ADD   FILESYSTEM Str
    | ALTER SYSTEM ALTER FILESYSTEM 整数 SET ALLOCATE Eq （ ON | OFF ）
    | ALTER SYSTEM DROP  FILESYSTEM 整数

-- ── 会话参数（闭合白名单；照 PG 的 SET/ALTER SESSION 同节点）──
VariableSetStmt:
      ALTER SESSION SET   参数名 Eq SetValue
    | ALTER SESSION CLEAR 参数名
参数名： { work_area_size, max_query_memory }        -- 白名单在 ② 判
SetValue:  整数 | 浮点 | Str                          -- 内存量照 PG：'64MB' / '4GiB' / 纯数字（字节）
```

**词法补充**：`主体名` = 标识符；`整数` = 数字字面量（工作区 id / 槽位号）；
`Str` = 单引号字符串（模板名、挂载点、配额名）；**挂载点与模板名一律用字符串**，
不做名字解析（不是对象名）。

### 2.2 AST 映射（补进 `SQL前端设计` §3.1 的表）

| PG 节点 | 我们的类型 | 说明 |
| --- | --- | --- |
| `VariableSetStmt` | `VariableSetStmt { kind: Set\|Clear, name, args: Vec<AConst>, location }` | `ALTER SESSION SET/CLEAR`（PG 的 `SET` 与 `ALTER SESSION SET` 同节点）；`is_local`/CURRENT 面在清单外 |
| `AlterSystemStmt` | `AlterSystemStmt { action: AddFilesystem{mount} \| AlterFilesystem{slot, allocate} \| DropFilesystem{slot}, location }` | 本库扩展（记档） |
| `AlterDatabaseStmt`（PG 带库名） | `AlterDatabaseStmt { action: AlterDatabaseAction, location }`；`AlterDatabaseAction = CloneWorkspace{name, source: WorkspaceSource} \| AddTemplate{name, from} \| WorkspaceToTemplate{ws, name} \| DropTemplate{name}`；`WorkspaceSource = Workspace(u64) \| Template(String)` | **本库实例即一个"库"** ⇒ 不带库名（记档） |

### 2.3 语法要点

- 未知的 `ALTER SYSTEM`/`ALTER DATABASE` 动作 ⇒ **解析期拒绝**（闭集纪律，
  没有产生式）；
- 白名单外的会话参数 ⇒ **绑定期拒绝**（"参数不存在"，**不静默忽略**）；
- 配额项之外的键（`ALTER WORKSPACE … SET QUOTA (foo=1)`）⇒ 解析期拒绝
  （`QuotaItem` 的产生式只有四个键）。

---

## 3 执行落点（模块分工）

| 语句 | `bicdb-sql` | `bicdb-catalog`（public 实例） | `bicdb-daemon` / 存储 | 审计 |
| --- | --- | --- | --- | --- |
| W1/W2/W5 | 解析 + 绑定（admin 先验） | `ws$` 写 + 建区/克隆协议 | 目录、控制文件、reflink、**暂停写窗** | ✅ |
| W3/W4 | 同上 | `ws$` 行更新（唯一索引守名） | 配额落 `ws$` 四列 | ✅ |
| T1/T2/T3 | 同上 | **`tpl$`** 写 | 模板区目录、reflink、`status` 转换 | ✅ |
| F1–F3 | 同上 | `fs$` 写 | **全局控制文件**、挂载点校验/探测、分配开关 | ✅ |
| S1/S2 | 解析 + 绑定（白名单 + 值域） | — | 会话上下文（WMM 双模式） | ✗（非管理面） |

**并发与冲突**：DCL 之间天然串行（都在 `public`、单事务）；与 `Move` **不得并发**
——F3 的前置检查与移出在同一事务里完成（窗口协议复用）；**T1/W2 的暂停写窗**
同样与写访问互斥。

---

## 4 与规格的同步点（评审点汇总）

| # | 点 | 建议 |
| --- | --- | --- |
| ① | REQ-SQL-005 无 DCL 类目；`CREATE … CLONE OF` 与 W2 重复 | **增补** DCL 闭集（W/T/F/S 四组）；**删除** `CLONE OF` 从句 |
| ② | REQ-SQL-006 写"会话变量 `SET`/`SHOW` 不提供" | 改为"不提供**通用**会话变量；`ALTER SESSION SET/CLEAR` 仅**白名单参数**"；`SHOW` 维持不提供 |
| ③ | **模板的登记与目录**（`tpl$` 新表 + 实例级模板区） | `arch/03` §3.1.3 增 `tpl$` 列定义；`arch/02` §2.1 目录布局增"模板区"一节 |
| ④ | 模板/工作区的**快照窗口协议**（不复制 redo/undo、`file_scn` 起点、`seq$` 延续） | 与 `arch/02` §2.12 的既有克隆细则对齐后，写入 §2.12 |
| ⑤ | `spec/ENG.md` REQ-ENG-005 的 `execute` 面 | 补一句：DCL 走同一通道，**不产出算子树** |
| ⑥ | `spec/API.md` 的管理面（会话内省/cancel/terminate） | 与本设计**正交**（协议面命令 ≠ SQL 语句）——不改 |

---

## 5 实施切片

| 片 | 内容 | 依赖 |
| --- | --- | --- |
| **D1** | **语法与 AST**：`VariableSetStmt`/`AlterSystemStmt`/`AlterDatabaseStmt`（含模板四动作）+ `CREATE WORKSPACE` 去掉 `CLONE OF` + 闭集拒绝 | `bicdb-sql` S1 已落地——**纯解析，可先行** |
| **D2** | 工作区生命周期 W1–W5 | `目录详设` C1–C4 |
| **D3** | 文件系统池 F1–F3 | `arch/02` §2.6 全局控制文件（**尚未实现**）+ §3.4 的 Move |
| **D4** | 会话参数 S1/S2 | 会话层（随 S7 与执行器/会话层切片） |
| **D5** | **模板 T1–T3 + W2 的模板源** | D2 + 模板区布局与 `tpl$`（评审点③） |

---

## 6 本期暂缓（非永久不做）+ 触发条件

| 项 | 触发条件 |
| --- | --- |
| `ALTER SYSTEM SET <实例参数>` 的一般面（如 `work_memory_target`） | 在线可调成为运维需求时（当前：P0 冻结值 + 配置 + 重启） |
| 日志/检查点/归档命令（`ALTER SYSTEM SWITCH LOGFILE`、`ARCHIVE LOG`…） | `OPS` 域命令面落地时（V1.0 由内部调度与工具承担） |
| 普通主体**自助用模板建区**（放宽 admin 限制） | 产品确需时（需求未含） |
| `ALTER WORKSPACE <id> ADD DATAFILE/SIZE` | 自动增长实测不足时 |
| 模板的版本/依赖管理（模板从模板派生、模板谱系） | 出现"模板的模板"真实需求时（当前：克隆是复制，不建依赖） |
| `EXPORT/IMPORT`、表空间、`ALTER USER`（无授权模型） | 需求引入时 |

---

## 附：与既有文档的同步点

- `doc/SQL前端设计_v0.1.md` §3.1 节点表 / §3.3 语句闭集 / §10 的 S7 → 按 §2.2 补。
- `doc/目录详设_v0.1.md` §5.1（建区协议，W1/W2 复用）/ §5.6（指向本设计）。
- `doc/spec/SQL.md` REQ-SQL-005/006、`doc/spec/ENG.md` REQ-ENG-005、
  `doc/arch/02` §2.12 与目录布局、`doc/arch/03` §3.1.3（`tpl$`）→ 按 §4 评审后同步。
