# bicdb

A workspace-isolated memory and asset database for AI agents.
面向 AI Agent 的用户隔离记忆与资产数据库。

**English**　|　[**中文**](#中文)

---

## English

### What it is

bicdb is a single-machine, workspace-isolated, multi-connection transactional
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

> **v0.3.2 (2026-10-10).** The on-disk engine, WAL/Undo recovery,
> transactions, buffer pool, heap tables, B+Tree indexes, catalog and local
> service protocol are working and covered by the workspace test suite. The SQL
> layer supports DDL/DML, aggregates, two-table joins, set operations,
> `INSERT … SELECT`, named parameters and rule-based index access.
>
> The management plane now includes filesystems, users, private workspaces,
> local password authentication, authenticated workspace binding, read-only
> dictionary views and structure-only workspace templates. A service accepts
> multiple independent connections; statements are currently serialized by one
> instance executor.
>
> The graph layer is implemented as native transactional storage. It provides a
> documented Cypher subset, bounded paths and `shortestPath`, scalar/composite/
> JSON-path property indexes, `PROFILE CYPHER`, SQL `GRAPH_TABLE`, and native
> BM25 full-text indexes for nodes and relationships. Full-text maintenance can
> be strict, eventual, manual, batched or timer-driven.
>
> This is an active development release, not Neo4j compatibility. Full Cypher
> 25, APOC/GDS, user-defined procedures, unbounded traversal, graph vector and
> spatial indexes, parallel statement execution, remote TCP transport and
> production-scale performance qualification remain open.

### Documentation

| Document | Contents |
| --- | --- |
| [Requirements](docs/requirements.md) | What V1.0 must do, with numbered requirements and priorities |
| [Design](docs/design.md) | Overall architecture: isolation, storage, transactions, retrieval, graph, phases |
| [Storage design](docs/storage.md) | Storage layer: file layout, page format, ROWID, recovery. **Design frozen (2026-10) — all pending items closed** |
| [Platform support](docs/platform-support.md) | Supported architectures and compatibility baseline |
| [Manual](docs/使用手册.md) | **User manual** (Chinese): capability table, installation, workspaces, SQL, operations and drivers |
| [Graph and full-text guide](docs/图数据库与全文检索.md) | Native property graph, Cypher subset, property/full-text indexes, `GRAPH_TABLE`, maintenance and limits |
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
| `bicdb-net` | **Local client protocol v0.1** (frames, verbs, wire values, drivers); the full versioned request protocol (envelope/ACK/reconcile, TCP+auth, server cursors) is a later slice — see `docs/客户端协议_v0.1.md` §0 |
| `bicdb-daemon` | Worker process entry, bounded execution pool, maintenance threads |
| `bicdb-tools` | Diagnostics: `page_dump`, `db_check` |
| `bicdb-cli` | `bicdb` binary: instance bootstrap, service lifecycle, SQL execution, shell |
| `bicdb-sqlplus` | `bicdbcli`: standalone SQL*Plus-style client (buffer, slash commands, SPOOL, scripts) |
| `bicdb-client` | Rust driver on top of `bicdb-net` (`Connection`, `ResultSet`, `Row`, `Value`) |
| `bicdb` (Python, `drivers/python`) | Python driver: DB-API 2.0 subset, stdlib only |

### Current capability map

| Area | Current state |
| --- | --- |
| Durable relational core | Implemented: heap, B+Tree, WAL/Undo, recovery, transactions and checkpoints |
| SQL and clients | Implemented subset: DDL/DML, query operations, CLI, SQL*Plus-style client, Rust and Python drivers |
| Users and workspaces | Implemented locally: password authentication, private workspace binding, dictionary reads and structure templates |
| Property graph | Implemented subset: native adjacency, Cypher reads/writes, bounded paths, property indexes and `GRAPH_TABLE` |
| Graph full text | Implemented: BM25, node/relationship scopes, JSON paths, strict/eventual reads and deferred maintenance |
| Assets and higher data models | Storage primitives and selected SQL surfaces exist; the public contracts continue to evolve |
| Later work | TCP, parallel execution, vector/spatial graph indexes, complete Cypher ecosystem and scale qualification |

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

From your own program, connect with a driver over the control socket (the
service must be running) — spec: [docs/客户端协议_v0.1.md](docs/客户端协议_v0.1.md):

```rust
// Rust: crates/client (package `bicdb-client`)
let mut conn = bicdb_client::Connection::connect("./demo")?;
let rs = conn.query("SELECT id, name FROM t WHERE id = :id", &[("id", 1_i64.into())])?;
```

```python
# Python: drivers/python (package `bicdb`, DB-API 2.0 subset, stdlib only)
import bicdb
conn = bicdb.connect("./demo")
cur = conn.cursor()
cur.execute("SELECT id, name FROM t WHERE id = :id", {"id": 1})
print(cur.fetchall())
```

There is a **local test environment** at `~/bicdb/demo` (created by
[`scripts/demo.sh`](scripts/demo.sh)) — a fixed instance with seeded data
(`t`/`emp`/`big`), for hands-on testing and driver work:

```bash
scripts/demo.sh create    # 建区 + 起服务 + 灌测试数据（幂等）
scripts/demo.sh cli       # SQL*Plus-shaped client against it
scripts/demo.sh py        # connect with the Python driver
scripts/demo.sh reset     # 回到初始数据
```

`init` creates a real on-disk instance (dictionary file, undo segment, WAL group
directory, two control-file copies). Every command opens the instance through
**crash recovery** and closes it with a **full checkpoint**, so an interrupted
process loses nothing that was committed. Uniqueness is enforced on `INSERT`
(including inside an open transaction); `BEGIN`/`COMMIT`/`ROLLBACK` work as
statements of one session.

### Install

```bash
scripts/install.sh                  # picks a writable, visible root and says which
export BICDB_HOME=$HOME/bicdb-home && export PATH="$BICDB_HOME/app/bin:$PATH"
bicdb home                          # program / data / log / backup — one command
bicdb init && bicdb start -p public && bicdb list
```

**One root, four directories** — so nobody has to go looking for logs or data:

```text
<BICDB_HOME>/app/      programs (read-only; upgrading replaces this one)
             /public/  the PUBLIC workspace (control/ · wal/ · data/)
             /log/     all database logs (public.log)
             /backup/  default backup destination
```

Reinstalling never touches `public/`; `scripts/uninstall.sh` keeps data by
default (`--purge` removes it) and refuses while a workspace is running.
The installation layout is documented in the [manual](docs/使用手册.md) §2.

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

bicdb 是面向 **AI Agent** 的单机、按工作区隔离、多连接事务型数据库，
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

> **v0.3.2（2026-10-10）**。磁盘存储、WAL/Undo 恢复、事务、缓冲池、堆表、
> B+Tree、目录和本机服务协议已经可运行，并由工作区测试覆盖。SQL 已支持 DDL/DML、
> 聚合、两表连接、集合运算、`INSERT … SELECT`、具名参数和规则式索引访问。
>
> 管理面已经支持文件系统池、用户、私有工作区、本机口令认证、认证后的工作区绑定、
> 只读字典查询和仅复制结构的工作区模板。同一服务可保持多条独立连接；语句目前仍由
> 单实例执行器串行调度。
>
> 图模块使用原生事务存储，已支持明确边界的 Cypher 子集、有界路径与
> `shortestPath`、标量/组合/JSON 路径属性索引、`PROFILE CYPHER`、SQL
> `GRAPH_TABLE`，以及节点和关系上的原生 BM25 全文索引。全文维护支持 strict、
> eventual、manual、batch 和定时模式。
>
> 当前版本仍在开发，不宣称与 Neo4j 完全兼容。完整 Cypher 25、APOC/GDS、用户自定义
> 过程、无界遍历、图向量/空间索引、语句并行执行、远程 TCP 和生产规模性能验证尚未完成。

### 文档

| 文档 | 内容 |
| --- | --- |
| [需求文档](docs/requirements.md) | V1.0 必须做到什么，逐条编号与优先级 |
| [总体设计](docs/design.md) | 隔离、存储、事务、检索、图，以及研发阶段 |
| [存储结构设计](docs/storage.md) | 文件布局、页格式、ROWID、恢复。**设计冻结（2026-10）**——全部待冻结项已关闭 |
| [平台支持](docs/platform-support.md) | 支持的架构与兼容基线 |
| [使用手册](docs/使用手册.md) | 能力现状、安装、工作区、SQL、运维与驱动 |
| [图数据库与全文检索](docs/图数据库与全文检索.md) | 原生属性图、Cypher 子集、属性/全文索引、`GRAPH_TABLE`、维护方式和边界 |
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
| `bicdb-net` | **本机客户端协议 v0.1**（帧/动词/线上值模型/驱动）；版本化请求协议全量（封套与 ACK 对账、TCP+认证、服务端游标）是后续切片——见 `docs/客户端协议_v0.1.md` §0 |
| `bicdb-daemon` | 工作进程入口、有界执行池、维护线程 |
| `bicdb-tools` | 诊断工具：`page_dump`、`db_check` |
| `bicdb-cli` | `bicdb` 命令行：建区、服务生命周期、执行 SQL、交互式 shell |
| `bicdb-sqlplus` | `bicdbcli`：SQL*Plus 形态的独立客户端（缓冲、斜杠命令、SPOOL、脚本） |
| `bicdb-client` | Rust 驱动（建在 `bicdb-net` 上：`Connection`/`ResultSet`/`Row`/`Value`） |
| `bicdb`（Python，`drivers/python`） | Python 驱动：DB-API 2.0 子集，只用标准库 |

### 当前能力图

| 领域 | 当前状态 |
| --- | --- |
| 持久关系内核 | 已实现：堆表、B+Tree、WAL/Undo、恢复、事务和检查点 |
| SQL 与客户端 | 已实现限定子集：DDL/DML、查询、CLI、SQL*Plus 形态客户端、Rust/Python 驱动 |
| 用户与工作区 | 已实现本机认证、私有工作区绑定、字典只读查询和结构模板 |
| 属性图 | 已实现原生邻接、Cypher 读写、有界路径、属性索引和 `GRAPH_TABLE` |
| 图全文 | 已实现 BM25、节点/关系范围、JSON 路径、strict/eventual 查询和延迟维护 |
| 资产与上层模型 | 存储原语和部分 SQL 接口已经存在，公开契约仍在演进 |
| 后续工作 | TCP、并行执行、图向量/空间索引、完整 Cypher 生态与规模化验证 |

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

在自己的程序里连：起服务后用驱动（规格 [docs/客户端协议_v0.1.md](docs/客户端协议_v0.1.md)，
Rust 与 Python 实现同一份）：

```rust
// Rust：crates/client（包名 `bicdb-client`）
let mut conn = bicdb_client::Connection::connect("./demo")?;
let rs = conn.query("SELECT id, name FROM t WHERE id = :id", &[("id", 1_i64.into())])?;
```

```python
# Python：drivers/python（包名 `bicdb`，DB-API 2.0 子集，只用标准库）
import bicdb
conn = bicdb.connect("./demo")
cur = conn.cursor()
cur.execute("SELECT id, name FROM t WHERE id = :id", {"id": 1})
print(cur.fetchall())
```

本机有一处**固定的测试环境** `~/bicdb/demo`（由 [`scripts/demo.sh`](scripts/demo.sh) 建）：
带测试数据（`t`/`emp`/`big`）的实例，手测与驱动联调都用它：

```bash
scripts/demo.sh create    # 建区 + 起服务 + 灌测试数据（幂等）
scripts/demo.sh cli       # 连它进 SQL*Plus 形态客户端
scripts/demo.sh py        # 用 Python 驱动连一下（自检）
scripts/demo.sh reset     # 回到初始数据
```

`init` 建出一个**真盘实例**（字典文件、撤销段、日志组目录、控制文件双副本）。
每条命令都经**崩溃恢复**打开、以**完全检查点**关闭——进程被打断也不会丢已提交的
数据。唯一性在 `INSERT` 上把守（显式事务内同样把守）；`BEGIN`/`COMMIT`/`ROLLBACK`
按一个会话的语句工作。

### 安装

```bash
scripts/install.sh                  # 挑一个可写、可见的根并打印是哪个
export BICDB_HOME=$HOME/bicdb-home && export PATH="$BICDB_HOME/app/bin:$PATH"
bicdb home                          # 程序/数据/日志/备份——一条命令看全
bicdb init && bicdb start -p public && bicdb list
```

**一个根，四个目录**——运维不用到处找日志、找数据：

```text
<BICDB_HOME>/app/      程序（只读；升级 = 换这一目录）
             /public/  PUBLIC 工作区（control/ · wal/ · data/）
             /log/     数据库日志（public.log）
             /backup/  默认的备份包落地处
```

重装永不碰 `public/`；卸载默认保数据（`--purge` 才删），有工作区在跑时拒绝卸载。
安装布局见[使用手册](docs/使用手册.md) §2。

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
