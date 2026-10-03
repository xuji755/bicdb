# bicdb

面向 Agent 的用户隔离记忆与资产数据库。

单机、按工作区隔离、多线程执行的事务型数据库，统一保存关系数据、JSON、
记忆、属性图与向量，并以受管理的外部文件保存不可变大对象。

> **当前状态：仅目录骨架，未实现任何功能。**
> 本仓库目前只建立设计到代码的映射，所有 crate 均为占位，
> 不提供任何可调用接口，也不承诺任何接口形态。

## 核心约束

- **用户隔离**：用户只能访问本人的私有数据。不提供跨用户授权、所有权转移
  或代理读取；只有 `public` 工作区向所有用户只读共享。
- **原地更新 + Undo**：保留 Oracle 风格的页式堆表、原地更新、Undo、WAL
  与恢复体系。不采用 ASTORE 式堆内多版本路线，不以 LSM-Tree 作为主存储组织。
- **SQL 分层**：SQL 层参考 PostgreSQL 的原始解析、语义分析、计划与算子分层。

完整目标、契约、阶段与验收标准见内部总体开发方案（内部文档，不随本仓库发布）。

## 模块结构

|crate|职责|对应阶段|
| --- | --- | --- |
| [`bicdb-common`](crates/common/) | 统一错误码、LSN/SCN、RAII 与页校验和基础工具 | P2 |
| [`bicdb-workspace`](crates/workspace/) | WorkspaceContext、身份与路由、根目录句柄、配额与公平调度 | P1 |
| [`bicdb-storage`](crates/storage/) | 页格式、槽位堆表、跨页行片段、BufferPool、PageGuard | P2 |
| [`bicdb-wal`](crates/wal/) | WAL 记录、重做与撤销、检查点、恢复重入 | P3 |
| [`bicdb-txn`](crates/txn/) | 事务状态机、语句快照、行锁、Undo、死锁检测 | P3–P4 |
| [`bicdb-index`](crates/index/) | B+Tree：唯一键、复合键、范围访问、并发分裂 | P4 |
| [`bicdb-types`](crates/types/) | Oracle 兼容标量、JSON 文档表示、VECTOR 类型 | P2, P5 |
| [`bicdb-catalog`](crates/catalog/) | Catalog 解析、类型描述、对象版本 | P5 |
| [`bicdb-sql`](crates/sql/) | Raw AST、Binder、逻辑/物理计划、执行器 | P5 |
| [`bicdb-asset`](crates/asset/) | ASSET_REF、流式写读、引用登记与回收 | P6 |
| [`bicdb-memory`](crates/memory/) | 记忆版本、检查点、TTL、派生依赖与删除传播 | P6 |
| [`bicdb-retrieval`](crates/retrieval/) | 倒排与分词、精确向量、RRF 融合、HNSW/IVFFlat | P7–P9 |
| [`bicdb-graph`](crates/graph/) | 命名图、顶点/边存储、Cypher 子集、有界遍历 | P10 |
| [`bicdb-net`](crates/net/) | 版本化请求协议、幂等与 CAS、SDK/CLI 对接 | P5 |
| [`bicdb-daemon`](crates/daemon/) | 工作进程入口、有界执行池、维护线程、监督器 | P1 |
| [`bicdb-tools`](crates/tools/) | 诊断工具：`page_dump`、`db_check` | P2 |

## 研发阶段

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

## 平台支持

**同时支持 x86_64 与 aarch64（ARM64）**，两者均为受支持目标，不是"主/备"关系。
开发期以 **ARM64** 为主进行编译与测试。

工具链版本由 `rust-toolchain.toml` 固定，保证两种架构下门禁可复现。CI 在
同一 OS 版本上跑双架构矩阵，使差异只来自架构本身：

| Runner | 架构 |
| --- | --- |
| `ubuntu-24.04` | x86_64 |
| `ubuntu-24.04-arm` | aarch64 |

> 移植注意：涉及 SIMD 或字节序的代码（如向量距离计算、校验和、页编码）
> 必须避免依赖某一架构，跨架构一致性由 CI 矩阵与参考模型测试共同保证。
> 具体实现细节须先产出证据包（§22），不得凭推测编写。

## 构建

要求 Rust 工具链（版本见 `rust-toolchain.toml`；`rust-version` 下限 1.75）。

```bash
cargo build --workspace
cargo test  --workspace
cargo fmt   --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

## 目录

| 路径 | 说明 |
| --- | --- |
| `crates/` | 各功能模块（见上表） |
| `tests/` | 测试分层：`integration/`、`model/`、`crash/`、`isolation/` |
| `tools/` 中 `crates/tools` | 诊断工具 |
| `docs/` | **对外发布**的文档 |
| `doc/` | 内部研发文档，**不随本仓库发布** |

## 尚未确定

- 性能与容量数字：均为设计建议，须在 P0 冻结（性能基线与基准机规格同样待冻结，
  需明确测试在哪一架构上进行、是否需要双架构分别记录）

## 参与开发

见 [`docs/README.md`](docs/README.md)。提交前请确保格式、Clippy 与测试本地通过：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test  --workspace
```

## 许可证

[Apache License 2.0](LICENSE)。
