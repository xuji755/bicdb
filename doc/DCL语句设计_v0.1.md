# DCL 管理语句设计 v0.1

> **状态**：草案（2026-10-06），**待评审**——评审通过前不进入编码。
> **触发**：用户口径（2026-10-06）——工作区创建/克隆与**文件系统注册**需要设计
> 一套 DCL 语法（`CREATE WORKSPACE`、`ALTER SYSTEM ADD FILESYSTEM …`、
> `ALTER DATABASE CLONE WORKSPACE … FROM …` 等）。
> **上位**：`doc/spec/SQL.md` REQ-SQL-005（工作区 DDL 三条语义）/REQ-SQL-006；
> `doc/arch/02` §2.6（控制文件）/§2.8（文件系统池）/§2.12（创建与克隆三步）；
> `doc/arch/03` §3.1.3（`user$`/`ws$`/`fs$`）、§3.1.4（固定表）；
> `doc/目录详设_v0.1.md` §5.1（建区协议）。
> **下游**：`doc/SQL前端设计_v0.1.md`（语法与 AST 形状）、`bicdb-catalog`（字典写侧）、
> `bicdb-daemon`（池与控制文件）。

---

## 0 边界与共同纪律

**DCL = 管理面语句**：对象是**工作区、实例资源、会话参数**——不是用户数据。
因此**不进算子管线**（④ 物理计划不产出算子树）：执行落点是**目录写 + 控制文件更新**，
但仍走**同一条 `compile → execute` 通道**（`SQL` §0.2 的"没有第二条路"：
资格、审计、取消、请求标签透传一个不少）。

| # | 共同纪律 | 依据 |
| --- | --- | --- |
| 1 | **仅 admin**（`ALTER SESSION` 除外——会话自己的参数）；**资格检查先于对象查找**（否则"不存在"与"不是你的"之间的差别是一次探测） | REQ-SQL-005 |
| 2 | **在 `public` 上执行**（管理元数据 `ws$`/`fs$` 在那里）；**单事务不跨工作区** | REQ-ISO-005；REQ-SQL-005 |
| 3 | **审计留痕**（含请求标签） | `OPS` REQ-OPS-003 |
| 4 | **闭集**：清单外即语法错误（没有产生式）——与 REQ-SQL-006 同规 | 本设计 §1 |
| 5 | **语法形状照 PG**（节点同名同形；本库扩展逐条记档） | `SQL前端设计` §3 的用户口径 |

---

## 1 语句闭集（V1.0）

### 1.1 工作区生命周期

| # | 语句 | 语义落点 |
| --- | --- | --- |
| W1 | `CREATE WORKSPACE FOR USER <主体> [NAME '<名>']` | 三步协议（`目录详设` §5.1）：建文件 + 引导页 → 建字典（DDL 事务）→ **登记 `public.ws$`（唯一的可见性分界）**；`NAME` 缺省 = 跟随属主名（`arch/03` §2.11） |
| W2 | `ALTER DATABASE CLONE WORKSPACE '<新名>' FROM <源 workspace_id>` | **克隆独立成句**（用户形状）：校验**同一 owner**（REQ-ISO-012）；文件按 reflink 复制（`arch/03` §2.7）；`seq$` **原样延续**（序号空间不重置，§2.12）；新工作区登记 `ws$`（同一 owner、新名字） |
| W3 | `ALTER WORKSPACE <id> SET NAME = '<名>' \| NULL` | 改名；`NULL` = 回到"跟随用户名"（`arch/03` §2.11）；同属主内名字唯一（`ws$` 的 `(user_id, name)` 唯一索引） |
| W4 | `ALTER WORKSPACE <id> SET QUOTA (data = <n>, undo = <n>, temp = <n>, asset = <n>)` | 四配额**按角色分设**（`arch/02` §2.2）；只允许**不小于当前占用**的调整（向下越界 ⇒ 具名拒绝——不静默截断） |
| W5 | `DROP WORKSPACE <id>` | 三步的反向（`arch/03` §2.12）：**先从 `ws$` 摘除（可见性先断）** → 释放资源 → 删目录；`public` 工作区**不可删**（预置实例对象） |

> **与现规格的一处替换（评审点①）**：REQ-SQL-005 现写 `CREATE WORKSPACE … [CLONE OF <id>]`。
> 本设计把克隆**独立成句**（W2，用户形状）——**同一件事不设两个入口**，建议
> `CREATE` 的 `CLONE OF` 从句**删除**，需求文字随评审同步。

### 1.2 实例资源：文件系统池

**池语义以 `arch/02` §2.8 为准**（实例级；加入无前置；**移出前该文件系统上不得有
任何数据文件**——硬条件，先 `Move` 走再移出）。池定义存**全局控制文件**（权威、
可重建），`public.fs$` 供查询/审计（`arch/03` §3.1.4）。

| # | 语句 | 语义落点 |
| --- | --- | --- |
| F1 | `ALTER SYSTEM ADD FILESYSTEM '<挂载点路径>'` | 校验：路径存在、可写、**非符号链接**（`FileIo` 的既有纪律）；写**全局控制文件**（双副本、慢路径）+ 插 `public.fs$` 一行（`status = 在池`）；新加入者**立即参与后续分配** |
| F2 | `ALTER SYSTEM ALTER FILESYSTEM <槽位号> SET ALLOCATE = ON \| OFF` | **退役前的排水阀**：`OFF` = 不再分配新文件到该文件系统，**已有文件不动**（配合 `Move`，`arch/03` §3.4） |
| F3 | `ALTER SYSTEM DROP FILESYSTEM <槽位号>` | **硬前置**（§2.8）：该文件系统上**无任何数据文件**——检查取"全局控制文件的工作区清单 × 各工作区控制文件的完整路径前缀"（路径是权威、`arch/02` §2.6）；通过 ⇒ 全局控制文件更新 + `fs$` 行置 `status = 已移出`（**不删行**——审计友好） |

### 1.3 会话参数（闭合面）

| # | 语句 | 语义落点 |
| --- | --- | --- |
| S1 | `ALTER SESSION SET <参数> = <值>` | **闭合参数表**（V1.0 仅两个，皆属 WMM 会话面）：`work_area_size`（设 ⇒ 切 MANUAL、每内存区固定上限；**清零回 AUTO**）、`max_query_memory`（AUTO 下的会话总量约束）。约束照 `执行算子设计` §4.2：**≤ 租户配额，超过显式拒绝（不静默钳制）；值不进计划**（REQ-SQL-001） |
| S2 | `ALTER SESSION CLEAR <参数>` | 回到默认（`work_area_size` 清零 = 回 AUTO） |

**纪律**：**任何主体**可用（改的是自己的会话）；不写审计（非管理面）；
`SHOW` 仍**不提供**（内省走固定表 `session$` 与 API，`spec/SQL.md` 待定项）。

> **与现规格的一处放行（评审点②）**：REQ-SQL-006 现写"会话变量（`SET`/`SHOW`）
> 不提供"。WMM 的会话设置（已冻结）需要 `ALTER SESSION SET` 这一**闭合面**
> ——建议改为"**不提供通用会话变量**；仅 `ALTER SESSION SET/CLEAR` 的
> **白名单参数**（见 `CONV` §8 与会话参数清单）"，`SHOW` 维持不提供。

---

## 2 语法与 AST（照 PG 的形状）

**节点映射**（与 `SQL前端设计` §3.1 同表补三行）：

| PG 节点 | 我们的类型 | 说明 |
| --- | --- | --- |
| `VariableSetStmt` | `VariableSetStmt { kind: Set\|Clear, name: String, args: Vec<AConst>, location }` | `ALTER SESSION SET/CLEAR`（PG 的 `SET` 与 `ALTER SESSION SET` 同节点）；`is_local`/`CURRENT` 面在清单外 |
| `AlterSystemStmt` | `AlterSystemStmt { action: AlterSystemAction, location }`；`AlterSystemAction = AddFilesystem{mount}\|AlterFilesystem{slot, allocate}\|DropFilesystem{slot}` | `ADD/DROP/ALTER FILESYSTEM` 是本库扩展（记档；形状仿 PG 同类 DDL 节点） |
| `AlterDatabaseStmt`（PG，带库名） | `AlterDatabaseStmt { action: CloneWorkspace{name, source}, location }` | **本库实例即一个"库"** ⇒ `ALTER DATABASE` **不带库名**（记档差异）；`ACTION` 目前只有 `CLONE WORKSPACE` |

**语法要点**：
- 挂载点用**字符串字面量**（`'<path>'`）——名字位置一律不用路径；
- 槽位号用**数字字面量**（`<slot>`）；
- 未知的 `ALTER SYSTEM`/`ALTER DATABASE` 动作 ⇒ **解析期拒绝**（闭集纪律）；
  未在白名单的会话参数 ⇒ **绑定期拒绝**（"参数不存在"，不静默忽略）。

---

## 3 执行落点（模块分工）

| 语句 | `bicdb-sql` | `bicdb-catalog`（public 实例） | `bicdb-daemon` / 存储 | 审计 |
| --- | --- | --- | --- | --- |
| W1/W2/W5 | 解析 + 绑定（admin 资格先验） | `ws$` 写 + 建区/克隆协议（`目录详设` §5.1） | 工作区目录、控制文件、reflink | ✅ |
| W3/W4 | 同上 | `ws$` 行更新（唯一索引守名） | 配额落 `ws$` 四列 | ✅ |
| F1/F3 | 同上 | `fs$` 写 | **全局控制文件**双副本 + 挂载点校验/探测 | ✅ |
| F2 | 同上 | `fs$` 行更新（`status`） | 分配策略的开关（池状态） | ✅ |
| S1/S2 | 解析 + 绑定（参数白名单 + 值域） | — | 会话上下文（WMM 双模式） | ✗（非管理面） |

**并发与冲突**：DCL 之间天然串行化（都在 `public` 上、单事务）；与 `Move`
（F2/F3 的配套）**不得并发**——F3 的前置检查与移出动作在**同一事务**里完成
（检查即锁：`arch/03` §3.4 的"暂停写访问的窗口"协议复用）。

---

## 4 与规格的同步点（评审点汇总）

| # | 点 | 建议 |
| --- | --- | --- |
| ① | REQ-SQL-005 的语句清单没有 DCL 类目（现只写"工作区 DDL"三条） | **增补**：按本设计 §1 的闭集写入清单（W1–W5 / F1–F3 / S1–S2）；**删除** `CREATE … CLONE OF` 从句（克隆独立成句 W2） |
| ② | REQ-SQL-006 写"会话变量 `SET`/`SHOW` 不提供" | **改为**：不提供通用会话变量；`ALTER SESSION SET/CLEAR` 仅**白名单参数**（WMM 两项起）；`SHOW` 维持不提供 |
| ③ | `spec/ENG.md` REQ-ENG-005 的 `execute` 面 | 补一句：DCL 走同一 `compile→execute` 通道，**不产出算子树**（执行落点为目录/控制文件写） |
| ④ | `spec/API.md` 的管理面（REQ-API-018 会话内省 / cancel / terminate） | 与本设计**正交**：那些是协议面命令，不是 SQL 语句——不改 |

---

## 5 实施切片

| 片 | 内容 | 依赖 |
| --- | --- | --- |
| **D1** | 语法与 AST：三个新节点（`VariableSetStmt`/`AlterSystemStmt`/`AlterDatabaseStmt`）+ 闭集拒绝 + 用例（含"未知动作/未知参数各拒绝"） | `bicdb-sql` S1 已落地（纯解析，无新依赖）——**可先行** |
| **D2** | 工作区生命周期 W1–W5（建区/克隆/改名/配额/删区） | `目录详设` C1–C4（`ws$`/建区协议） |
| **D3** | 文件系统池 F1–F3（**全局控制文件** + `fs$` + 挂载点校验 + 排水/移出前置检查） | `arch/02` §2.6 的全局控制文件与池（**尚未实现**——需先落该存储件）+ `arch/03` §3.4 的 Move |
| **D4** | 会话参数 S1/S2（接 WMM 的 AUTO/MANUAL 切换 + 会话上下文） | 会话层（随 `SQL前端设计` S7 与执行器/会话层切片） |

---

## 6 本期暂缓（非永久不做）+ 触发条件

| 项 | 触发条件 |
| --- | --- |
| `ALTER SYSTEM SET <实例参数>` 的一般面（如 `work_memory_target`） | **在线可调**成为运维需求时（当前：P0 冻结值 + 配置 + 重启；改它要评估 WMM 的缩容语义） |
| 日志/检查点/归档命令（`ALTER SYSTEM SWITCH LOGFILE`、`ARCHIVE LOG`…） | `OPS` 域命令面落地时（V1.0 由内部调度与工具承担） |
| `ALTER WORKSPACE <id> ADD DATAFILE/SIZE`（手动加数据文件） | 自动增长（`arch/03` §3.3）实测不足时；当前"容量足够才建、不够即报错"已覆盖 |
| `EXPORT/IMPORT`、表空间（本库无此概念）、`ALTER USER`（无授权模型） | 需求引入时 |

---

## 附：与既有文档的同步点

- `doc/SQL前端设计_v0.1.md` §3.1 节点表 + §3.3 语句闭集 + §10 切片 S7 → 按 §2 补三行。
- `doc/目录详设_v0.1.md` §5.6（工作区 DDL）→ 指向本设计（W1–W5 的展开）；
  §5.1 的"三步协议"不变。
- `doc/spec/SQL.md` REQ-SQL-005/006、`doc/spec/ENG.md` REQ-ENG-005 → 按 §4 评审后同步。
