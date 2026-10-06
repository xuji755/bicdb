# bicdb

A workspace-isolated memory and asset database for AI agents.
面向 AI Agent 的用户隔离记忆与资产数据库。

**English**　|　[**中文**](#中文)

---

## English

### What it is

bicdb is a single-machine, workspace-isolated, multi-threaded transactional
database designed for **AI agents** rather than general-purpose RDBMS workloads.

It unifies five kinds of data under one transactional store:

| Purpose | Description |
| --- | --- |
| **Knowledge** | Documents, chunks, facts, and their provenance and derivation links |
| **Memory** | Mid-term task state and long-term facts/preferences, with versions and lifecycle |
| **Configuration** | Versioned config, prompt templates, tool parameters, model configs |
| **Topology** | Entity relations and dependency structure, queryable by bounded traversal |
| **Assets** | Immutable large objects (models, documents, images) kept as managed external files |

### Status

> **Design frozen (2026-10). Phases P1–P4 done; P5 in progress.**
> Storage, WAL/recovery, transactions, the buffer pool and the B+Tree index are
> implemented and covered by tests (`cargo test --workspace`). P5 so far: the
> workspace **catalog** (dictionary tables `obj$`/`tab$`/`col$`/`ind$`/`icol$`/
> `seg$`/`stat$`/`seq$`, DDL write side, row cache), the **table access service**
> (`bicdb-access`), the **SQL front end** (lexer/parser → binder → physical plan
> → session) and a **runnable CLI** (`bicdb init/sql/shell`) — see
> [Quick start](#quick-start). Still open: UPDATE/DELETE, aggregates and joins,
> the logical rewrite layer, and the daemon/protocol surface; all recorded with
> explicit triggers in `doc/待讨论清单.md`.

### Documentation

| Document | Contents |
| --- | --- |
| [Requirements](docs/requirements.md) | What V1.0 must do, with numbered requirements and priorities |
| [Design](docs/design.md) | Overall architecture: isolation, storage, transactions, retrieval, graph, phases |
| [Storage design](docs/storage.md) | Storage layer: file layout, page format, ROWID, recovery. **Design frozen (2026-10) — all pending items closed** |
| [Platform support](docs/platform-support.md) | Supported architectures and compatibility baseline |
| [Changelog](CHANGELOG.md) | Release notes, starting with v0.1.0 |

### Core constraints

- **User isolation.** A user can only access their own private data. There is no
  cross-user authorization, ownership transfer, or delegated read. Only the
  reserved `public` workspace is shared read-only with all users.
- **In-place update with Undo.** Oracle-style page heap tables, in-place updates,
  Undo, WAL, and crash recovery. Versioning is expressed at the **data model
  layer** (e.g. a main node plus version nodes) — the storage engine does *not*
  keep multiple versions of a row in the heap.
- **Not** an ASTORE-style in-heap multi-version design. **Not** LSM-tree as the
  primary storage organization.
- **Layered SQL.** Raw AST → binder → logical plan → physical plan → execution,
  following PostgreSQL's separation of parsing from semantic analysis.

### Workload profile

| Characteristic | Value | Consequence |
| --- | --- | --- |
| Read : write ratio | Read-dominant | The read path is the optimization focus |
| Write concurrency | Limited; no hot-row contention | Multi-writer correctness kept; no hot-spot optimizations |
| Read concurrency | High | Reads must be lock-free, concurrent, cacheable |
| Transaction shape | Short; no long-running transactions | Snapshot and Undo reclamation pressure is bounded |

Derived requirement: **the read path takes no row locks, does not block writers,
and is not blocked by them.**

### Modules

| Crate | Responsibility |
| --- | --- |
| `bicdb-common` | Error codes, LSN/SCN types, RAII, page checksum primitives |
| `bicdb-workspace` | Workspace context, identity and routing, root handles, quotas |
| `bicdb-storage` | Page format, slotted heap tables, cross-page rows, buffer pool |
| `bicdb-wal` | WAL records, redo/undo, checkpoints, recovery re-entrancy |
| `bicdb-txn` | Transaction state, statement snapshots, row locks, deadlock detection |
| `bicdb-index` | B+Tree: unique, composite, range access, concurrent splits |
| `bicdb-types` | Oracle-compatible scalars, JSON document model, vector type |
| `bicdb-catalog` | Catalog resolution, type descriptors, object versions |
| `bicdb-access` | Table access service: page selection/growth, row write, index maintenance |
| `bicdb-sql` | Raw AST, binder, logical/physical plans, executor, session |
| `bicdb-asset` | Asset references, streaming I/O, reference tracking and reclamation |
| `bicdb-memory` | Memory revisions, checkpoints, TTL, derivation and delete propagation |
| `bicdb-retrieval` | Inverted index and tokenization, exact vector, RRF fusion, HNSW/IVFFlat |
| `bicdb-graph` | Named graphs, vertex/edge storage, Cypher subset, bounded traversal |
| `bicdb-net` | Versioned request protocol, ACK/reconcile, sessions, SDK/CLI plumbing |
| `bicdb-daemon` | Worker process entry, bounded execution pool, maintenance threads |
| `bicdb-tools` | Diagnostics: `page_dump`, `db_check` |
| `bicdb-cli` | `bicdb` binary: instance bootstrap, service lifecycle, SQL execution, shell |
| `bicdb-sqlplus` | `bicdbcli`: standalone SQL*Plus-style client (buffer, slash commands, SPOOL, scripts) |

### Development phases

| Phase | Goal |
| --- | --- |
| P0 | Freeze contracts and threat boundaries |
| P1 | Workspace and test foundation |
| P2 | Scalar types and page storage |
| P3 | WAL, Undo, and recovery |
| P4 | Concurrency and B+Tree |
| P5 | SQL, JSON, and SDK basics |
| P6 | External assets and memory lifecycle |
| P7 | Full-text and exact hybrid retrieval |
| P8 | HNSW |
| P9 | IVFFlat |
| P10 | Bounded property graph |
| P11 | Isolation and full-stack reliability |
| P12 | Controlled trial release |

Milestones: M1 = recoverable storage (P3); M2 = usable private memory and assets
(P6); M3 = usable retrieval (P7); M4 = multi-model data (P8–P10); M5 = controlled
trial (P12).

### Platform support

Supports both **x86_64** and **aarch64**; development is primarily on ARM64.

Runtime baseline: **glibc ≥ 2.34**, **kernel ≥ 5.14**, no upper bound.

> The glibc floor is set by the highest versioned symbol a binary actually
> references, not by the build machine's glibc. The current floor of 2.34 comes
> from glibc merging libpthread into libc: Rust's standard library depends on
> pthread symbols, so any threaded Rust binary requires `GLIBC_2.34`.
> See [`docs/platform-support.md`](docs/platform-support.md).

CI runs its gates inside a Debian 12 container on both architectures, with a
mechanical check that no binary requires a symbol above the baseline.

### Quick start

```bash
cargo build --release

./target/release/bicdb init  ./demo      # also writes ./demo/bicdb.ini (the instance parameters)
./target/release/bicdb sql   -p ./demo "CREATE TABLE t (id NUMBER NOT NULL, name VARCHAR2(32))"
./target/release/bicdb sql   -p ./demo "INSERT INTO t VALUES (1, 'alpha'); INSERT INTO t VALUES (2, 'beta')"
./target/release/bicdb sql   -p ./demo "CREATE UNIQUE INDEX t_pk ON t (id)"
./target/release/bicdb sql   -p ./demo "SELECT id, name FROM t WHERE id >= 1 ORDER BY id DESC LIMIT 10"
./target/release/bicdb shell -p ./demo         # interactive; `;` ends a statement

# background service + SQL*Plus-style client
./target/release/bicdb start -p ./demo         # detached service, instance lock, log
./target/release/bicdb status -p ./demo
./target/release/bicdb params -p ./demo        # every knob + source (default/file/cli)
./target/release/bicdbcli -p ./demo            # buffer, `/` re-runs, SPOOL, @script, DESC
./target/release/bicdb stop  -p ./demo         # clean shutdown (full checkpoint)
```

`init` creates a real on-disk instance (dictionary file, undo segment, WAL group
directory, two control-file copies). Every command opens the instance through
**crash recovery** and closes it with a **full checkpoint**, so an interrupted
process loses nothing that was committed. Uniqueness is enforced on `INSERT`
(including inside an open transaction); `BEGIN`/`COMMIT`/`ROLLBACK` work as
statements of one session.

### Build

Requires a recent Rust stable toolchain; the minimum supported version is
`rust-version` in `Cargo.toml`. The toolchain is intentionally not pinned to an
exact version, to keep builds portable.

```bash
cargo build --workspace
cargo test  --workspace
cargo fmt   --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

### Explicitly out of scope

Full Oracle SQL compatibility and PL/SQL; RAC, distributed transactions,
cross-workspace write transactions; in-database model execution; complex LSM
storage; in-database LOBs; multi-node high availability and automatic failover;
window functions, JIT, and intra-query parallelism; unbounded variable-length
graph paths; cross-user authorization (permanently).

### License

[Apache License 2.0](LICENSE)

---

## 中文

### 这是什么

bicdb 是面向 **AI Agent** 的单机、按工作区隔离、多线程事务型数据库，
不以通用 RDBMS 负载为目标。

它在同一个事务存储中统一保存五类数据：

| 用途 | 说明 |
| --- | --- |
| **知识** | 文档、分块、事实，及其来源与派生关系 |
| **记忆** | 中期任务状态与长期事实/偏好，含版本与生命周期 |
| **配置** | 版本化配置、提示模板、工具参数、模型配置 |
| **拓扑** | 实体关系与依赖结构，支持有界遍历查询 |
| **资产** | 不可变大对象（模型、文档、图片），以受管理的外部文件保存 |

### 当前状态

> **设计已冻结（2026-10）；P1–P4 已实现，P5 进行中。**
> 存储、WAL/恢复、事务、缓冲池与 B+Tree 索引均已实现并有测试覆盖
> （`cargo test --workspace`）。P5 已落地：**目录**（字典表
> `obj$`/`tab$`/`col$`/`ind$`/`icol$`/`seg$`/`stat$`/`seq$`、DDL 写侧、行缓存）、
> **表访问服务**（`bicdb-access`）、**SQL 前端**（词法/语法 → 绑定 → 物理计划 →
> 会话）与**可运行的 CLI**（`bicdb init/sql/shell`，见
> [快速上手](#快速上手)）。仍待做：UPDATE/DELETE、聚合与连接、逻辑变换层、
> daemon/协议面——均挂明确触发条件（见 `doc/待讨论清单.md`）。

### 文档

| 文档 | 内容 |
| --- | --- |
| [需求文档](docs/requirements.md) | V1.0 必须做到什么，逐条编号与优先级 |
| [总体设计](docs/design.md) | 隔离、存储、事务、检索、图，以及研发阶段 |
| [存储结构设计](docs/storage.md) | 文件布局、页格式、ROWID、恢复。**设计冻结（2026-10）**——全部待冻结项已关闭 |
| [平台支持](docs/platform-support.md) | 支持的架构与兼容基线 |
| [更新日志](CHANGELOG.md) | 版本说明，自 v0.1.0 起 |

### 核心约束

- **用户隔离**。用户只能访问本人的私有数据，不提供跨用户授权、所有权转移
  或代理读取。只有保留工作区 `public` 向所有用户只读共享。
- **原地更新 + Undo**。采用 Oracle 风格的页式堆表、原地更新、Undo、WAL 与
  崩溃恢复。多版本由**数据模型层**表达（如主节点 + 版本节点）——存储引擎
  **不**在堆中保留行的多个版本。
- **不采用** ASTORE 式堆内多版本，**不以** LSM-Tree 作为主存储组织。
- **SQL 分层**。原始语法树 → 绑定 → 逻辑计划 → 物理计划 → 执行，
  参考 PostgreSQL 解析与语义分析分离的方法。

### 工作负载画像

| 特征 | 取值 | 对设计的含义 |
| --- | --- | --- |
| 读:写比例 | 读占绝对多数 | 读路径是优化重点 |
| 写并发 | 有限，无热点行争用 | 保留多写者正确性；不做热点优化 |
| 读并发 | 高 | 读必须无锁、可并发、可缓存 |
| 事务形态 | 短事务，无长事务 | 快照与 Undo 回收压力可控 |

据此确定的要求：**读路径不获取行锁、不阻塞写、也不被写阻塞。**

### 模块结构

| crate | 职责 |
| --- | --- |
| `bicdb-common` | 错误码、LSN/SCN 类型、RAII、页校验和基础工具 |
| `bicdb-workspace` | 工作区上下文、身份与路由、根目录句柄、配额 |
| `bicdb-storage` | 页格式、槽位堆表、跨页行、缓冲池 |
| `bicdb-wal` | WAL 记录、重做与撤销、检查点、恢复重入 |
| `bicdb-txn` | 事务状态、语句快照、行锁、死锁检测 |
| `bicdb-index` | B+Tree：唯一键、复合键、范围访问、并发分裂 |
| `bicdb-types` | Oracle 兼容标量、JSON 文档模型、向量类型 |
| `bicdb-catalog` | Catalog 解析、类型描述、对象版本 |
| `bicdb-access` | 表访问服务：页选址与增长、行写、索引维护 |
| `bicdb-sql` | 原始语法树、绑定、逻辑/物理计划、执行器、会话 |
| `bicdb-asset` | 资产引用、流式读写、引用登记与回收 |
| `bicdb-memory` | 记忆版本、检查点、TTL、派生依赖与删除传播 |
| `bicdb-retrieval` | 倒排索引与分词、精确向量、RRF 融合、HNSW/IVFFlat |
| `bicdb-graph` | 命名图、顶点/边存储、Cypher 子集、有界遍历 |
| `bicdb-net` | 版本化请求协议、ACK/对账、会话、SDK/CLI 对接 |
| `bicdb-daemon` | 工作进程入口、有界执行池、维护线程 |
| `bicdb-tools` | 诊断工具：`page_dump`、`db_check` |
| `bicdb-cli` | `bicdb` 命令行：建区、服务生命周期、执行 SQL、交互式 shell |
| `bicdb-sqlplus` | `bicdbcli`：SQL*Plus 形态的独立客户端（缓冲、斜杠命令、SPOOL、脚本） |

### 研发阶段

| 阶段 | 目标 |
| --- | --- |
| P0 | 冻结契约与威胁边界 |
| P1 | 工作区及测试底座 |
| P2 | 标量类型与页式存储 |
| P3 | WAL、Undo 及恢复 |
| P4 | 并发与 B+Tree |
| P5 | SQL、JSON 与 SDK 基础 |
| P6 | 外部资产与记忆生命周期 |
| P7 | 全文与精确混合检索 |
| P8 | HNSW |
| P9 | IVFFlat |
| P10 | 有限属性图 |
| P11 | 隔离与全栈可靠性 |
| P12 | 受控试用发布 |

里程碑：M1 = P3 可恢复存储；M2 = P6 可用私有记忆与资产；M3 = P7 可用检索；
M4 = P8–P10 多模型数据能力；M5 = P12 受控试用。

### 平台支持

**同时支持 x86_64 与 aarch64**，开发期以 ARM64 为主。

运行基线：**glibc ≥ 2.34**、**内核 ≥ 5.14**，不设上界。

> glibc 下限由**二进制实际引用的最高版本符号**决定，而非构建机的 glibc 版本。
> 当前下限 2.34 来自 glibc 将 libpthread 并入 libc：Rust 标准库依赖 pthread
> 系列符号，因此任何含线程的 Rust 二进制都要求 `GLIBC_2.34`。
> 详见 [`docs/platform-support.md`](docs/platform-support.md)。

CI 在 Debian 12 容器中、对两个架构分别执行门禁，并机械校验产物不引用高于
基线的符号。

### 快速上手

```bash
cargo build --release

./target/release/bicdb init  ./demo      # 并在其下生成 bicdb.ini（实例参数文件）
./target/release/bicdb sql   -p ./demo "CREATE TABLE t (id NUMBER NOT NULL, name VARCHAR2(32))"
./target/release/bicdb sql   -p ./demo "INSERT INTO t VALUES (1, 'alpha'); INSERT INTO t VALUES (2, 'beta')"
./target/release/bicdb sql   -p ./demo "CREATE UNIQUE INDEX t_pk ON t (id)"
./target/release/bicdb sql   -p ./demo "SELECT id, name FROM t WHERE id >= 1 ORDER BY id DESC LIMIT 10"
./target/release/bicdb shell -p ./demo         # 交互式；`;` 结尾执行

# 后台服务 + SQL*Plus 形态客户端
./target/release/bicdb start -p ./demo         # 分离进程 + 实例锁 + 日志
./target/release/bicdb status -p ./demo
./target/release/bicdb params -p ./demo         # 全部可调项 + 来源（默认/文件/命令行）
./target/release/bicdbcli -p ./demo            # 缓冲、`/` 重跑、SPOOL、@脚本、DESC
./target/release/bicdb stop  -p ./demo         # 干净关闭（完全检查点）
```

`init` 建出一个**真盘实例**（字典文件、撤销段、日志组目录、控制文件双副本）。
每条命令都经**崩溃恢复**打开、以**完全检查点**关闭——进程被打断也不会丢已提交的
数据。唯一性在 `INSERT` 上把守（显式事务内同样把守）；`BEGIN`/`COMMIT`/`ROLLBACK`
按一个会话的语句工作。

### 构建

要求 Rust 近期 stable；可编译的最低版本见 `Cargo.toml` 的 `rust-version`。
工具链刻意不做精确固定，以保持构建的普适性。

```bash
cargo build --workspace
cargo test  --workspace
cargo fmt   --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

### 明确不做

完整 Oracle SQL 兼容与 PL/SQL；RAC、分布式事务、跨工作区写事务；库内模型执行；
复杂 LSM 存储；库内 LOB；多节点高可用与自动故障切换；窗口函数、JIT 与单查询
并行；无限变长图路径；跨用户授权（永久不做）。

### 许可证

[Apache License 2.0](LICENSE)
