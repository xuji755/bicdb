# bicdb-graph

命名图、顶点/边存储、Cypher 子集、有界遍历

| 项 | 值 |
| --- | --- |
| 设计依据 | §13 有限属性图与AGE参考边界 |
| 对应阶段 | P10 |
| 当前状态 | 原生 SQL、增量实体存储、属性 / 节点 / 正反邻接 B-tree、按需访问及图全文 SQL 手动创建/查询/重建已实现；原生共享队列、源写事务接线与服务定时 batch 已实现 |

## 状态说明

提供 `Graph`、`parse`、`execute`、`Limits` 和 BFS `shortest_path`。
SQL 层提供 `CREATE/DROP GRAPH`、`SHOW GRAPHS` 和
`CYPHER graph 'query' [PARAMETERS 'JSON object']`，持久化使用原生 heap / undo / WAL。
模块运行时不连接 Neo4j。

DETACH DELETE 具有默认 10,000 条关联边的可配置预算，整条语句及 CALL/UNION 共用计数；超限丢弃全语句暂存变化。
实例参数 graph.detach_edge_limit 重启生效，0 仅允许隐式删除孤立节点。见 [删除预算](../../doc/bicdb_DETACH删除预算_20261009.md)。

MERGE 已支持 ON CREATE / ON MATCH SET、完整模式及无向关系复用；节点/关系唯一索引沿用原生发布检查，失败动作不发布暂存变化。
写路径仍完整加载图，不新增 Neo4j 并发锁算法。见 [条件更新规则](../../doc/bicdb_MERGE条件更新与约束_20261009.md)。

支持多标签、独立关系身份、方向、属性、有限路径、可选匹配、聚合、列表表达式、
EXISTS、显式作用域 CALL 子查询、UNION、写操作与失败原子性。
`CALL bicdb.searchNodes(query, options) YIELD node,rank,mode` 强制 db_type，
未指定 indexes 时先过滤后排名，分字段轮转去重，返回 `mode=bounded_scan`。
显式 indexes 接入原生全文，多索引按独立排名轮转去重，先域/OR 标签过滤再截取；
strict/eventual/暂存视图分别返回对应轮转模式，索引错误不回退扫描。
见[多索引排名融合](../../doc/bicdb_图多索引排名融合_20261009.md)。

当前图存储上限默认 100 MiB；顶点和边使用独立校验分片，写入只更新变化记录。
新图在创建事务中建立受保护的唯一记录键 B-tree，批量 UPDATE / DELETE 经树探测、
按快照回表和谓词复核；旧格式只读兼容，迁移失败 / 回滚保留原内容。
完整访问树的 v2 图按快照读取必要实体；旧图回退与写入仍全图校验，尚未完成大型知识图谱性能验收。
原生命名全文过程及显式类型 GRAPH_TABLE 已实现，支持标量和瞬态 `GRAPH_ELEMENT` 句柄的 SQL 联查与聚合。
空图初始化模板支持图及属性/全文索引定义、维护策略和暂停状态；克隆重建自己的原生访问树，不复制业务数据。
WITH GRAPH DATA 带数据逻辑图模板已接入，保留实体和分配器，重建目标索引/全文；见 [规则](../../doc/bicdb_带数据图初始化模板_20261009.md)。
APOC、无界路径、通用带业务行/reflink 快照和受管持久引用尚未实现，明确拒绝不支持的接口。
见 [空图初始化模板](../../doc/bicdb_空图初始化模板_20261009.md)。
见 [GRAPH_TABLE 规则](../../doc/bicdb_GRAPH_TABLE_SQL联查_20261009.md)。

完整语法、与旧存储规格的差异和后续路线：
[图检索增强设计](../../doc/bicdb_图检索增强与Neo4j对比设计_20261008.md)。
增量格式、索引和升级验证：
[图存储增量与索引实现](../../doc/bicdb_图存储增量与索引实现_20261009.md)。

验证：`cargo test -p bicdb-graph -p bicdb-sql -p bicdb-catalog -p bicdb-cli`。

实现进度请以仓库根 `README.md` 的阶段表为准。

属性树支持标签 / 类型、标量与组合字段、JSON 对象路径、唯一性、事务变更后的复核，
SQL 提供 CREATE / DROP / REBUILD / SHOW GRAPH INDEX 和只读 PROFILE CYPHER。
只读按需访问的实际装载数由 PROFILE 展示，键规划有预算，陈旧项通过显式 REBUILD 清理，旧段空间回收尚未实现。
详见 [图属性索引与 SQL 管理](../../doc/bicdb_图属性索引与SQL管理_20261009.md)。
原生属性索引测试：`cargo test -p bicdb-graph -p bicdb-sql -p bicdb-cli`。

持久化节点目录 / 正反邻接、按需快照读取、陈旧项过滤及三树原子重建：
[按需读取与持久化邻接](../../doc/bicdb_图按需读取与持久化邻接_20261009.md)。
SQL 维护：`ALTER GRAPH graph REBUILD ACCESS`；原生测试：`--test graph_access`。
实体权威数据仍在 heap 分片，访问树是派生结构，不是 GRP 冻结目标的专用邻接段。

全文内核提供指定 JSON 路径、域内 BM25、词项 / 短语 / 完整标识符通道、源修订复核、
不可变 generation 和原生分片 / 倒排键编码。共享队列只保存 ID / 修订 / 水位；暂停、
多消费者清理与部分进度均有测试。全文 SQL/字典及原生发布已接入，服务定时维护已接入，`searchNodes` 仍扫描。
见 [全文内核与批量队列](../../doc/bicdb_图全文内核与批量变更队列_20261009.md)。
验证：`cargo test -p bicdb-graph --test fulltext --test fulltext_journal`。

图全文使用独立 `CREATE/SHOW/SEARCH/ALTER/DROP FULLTEXT GRAPH INDEX` 语法，当前必须显式
`OPTIONS '{"update":"manual"}'`；新版 SYNC 分批消费原生共享队列，文档/倒排/确认同事务提交；REBUILD 完整换段。
旧全文索引保留扫描源的增量同步。队列与统计仍有有界内存开销，服务定时维护已接入。eventual 使用原生倒排并复核源修订，
默认 strict 明确以完整源重建保证结果，返回 strict_scan。
详见 [全文 SQL 与原生存储](../../doc/bicdb_图全文SQL与原生持久化_20261009.md)。
原生测试：`--test graph_fulltext`。

源写事务、共享队列、暂停/恢复与原生批量维护：
[共享队列实施](../../doc/bicdb_图全文共享变更队列与批量维护_20261009.md)。

定时 batch、实例参数继承与在线 OPTIONS 调整见
[定时维护实施](../../doc/bicdb_图全文定时批量维护与参数_20261009.md)。

命名 Cypher 全文过程、YIELD WHERE 与语句内暂存源视图已接入：
[过程与索引访问](../../doc/bicdb_Cypher全文过程与索引访问_20261009.md)。

`indexes:"auto"` 按 fields 顺序自动选择具备完整标签覆盖的节点全文索引，缺少字段或范围覆盖报错；
规则与证据见[自动索引选择](../../doc/bicdb_图全文自动索引选择_20261009.md)。

图全文支持固定目标 `WAIT [OPTIONS JSON]`，明确返回 READY/TIMEOUT/PAUSED 与目标覆盖状态；见[接口规则](../../doc/bicdb_图全文固定目标WAIT_20261009.md)。

图全文 v2 独立统计页与候选文档按需读取已接入；完整统计元数据、维护与旧格式仍有全量工作。
见 [候选读取规则](../../doc/bicdb_全文候选文档按需读取_20261009.md)。

全文 v3 已增加按查询词选择统计桶与 Merkle 校验页，候选查询不再装载全词汇统计。见 [实施](../../doc/bicdb_全文统计按查询词读取_20261009.md)。

全文 v4 增加持久化全语料预算，受管 SYNC/WAIT/定时批次在冷启动后只读取受影响的旧正文，通过贡献增减更新 BM25；v1/v2/v3 保留只读兼容，首次维护完整升级。统计元数据仍完整读取/编码，源写入仍完整加载/校验，REBUILD 仍完整；写语句自身改用首次原值 journal 和净变更集规划，见[局部写入规划](../../doc/bicdb_图语句变更集与局部写入规划_20261009.md)。见 [冷批次维护](../../doc/bicdb_全文冷批次维护_20261009.md)。

既有图预算已接入工作区配置和 CYPHER/GRAPH_TABLE 的 BUDGETS 请求入口；请求只能降低上限，服务与后台维护使用相同工作区政策。协作式图语句时间限制已接入；实际内存计账及显式部分结果协议仍待完成。见 [预算规则](../../doc/bicdb_图工作区预算与请求上限_20261009.md)。

独立邻接候选预算 `Limits::max_edge_expansions` 与表达式/访问工作分开；子查询共享，失败不发布暂存图。SQL/配置及多 GRAPH_TABLE 共享规则见[实施](../../doc/bicdb_图独立展开边预算_20261009.md)。

同一 SQL 的多个 GRAPH_TABLE 来源同时共享 `max_expansions` Runner 混合工作余额；来源 BUDGETS 不能重置额度。见[共享工作预算](../../doc/bicdb_GRAPH_TABLE共享工作预算_20261010.md)。

图语句 `max_elapsed_ms` 默认 60000、范围 1..60000，请求只可降低。继承的单调时钟涵盖子查询、原生读取、暂存与提交前安全点；成功提交后不追加超时失败。单次 I/O/提交不能抢占。PROFILE 追加 elapsed_ms。见[时间预算与回滚边界](../../doc/bicdb_图语句时间预算_20261009.md)。

写语句原地执行并以首次原值 journal 保证错误回滚；`QueryResult.changes` 返回净变更集，记录/索引/邻接/全文发布共用该集合。它不是可发布的部分图快照；完整源读取仍在；SQL 唯一键已采用持久精确候选探测及最终源复核，纯图 `changed_entries` 无证明时保留流式 fallback，见[唯一键探测](../../doc/bicdb_持久唯一键探测与快照回表_20261009.md)。v3 规划 image 不再构造旧格式边记录，见[原生规划](../../doc/bicdb_原生图写入规划与边载荷边界_20261009.md)。

SQL 原生 manifest 已支持旧 124 字节及带全局字节统计/递增快照版本的 148 字节头。读旧头不改写，写入及物理路由重建共同事务更新；完整源加载校验统计，原生边 patch 保留最终语料大小。此阶段仍完整加载源图、逐实体测量逻辑字节，不代表已完成局部加载写入、并发锁或实际内存计账。详见 ../../doc/bicdb_图全局统计与快照版本_20261009.md。

SQL 已接入原生局部写入、全局统计净增减、source resolver 唯一候选核验、持久独立聚合证明以及当前头行锁/版本提交验证。旧的 storage 查询接口保持只读；部分 image 必须使用变化集 patch。冷启动和无关提交后的新图可直接验证证明路径并局部写入；旧无证明头仍先完整验证。修改后的全文仍使用完整覆盖语料；实际并发、GC及大型性能保持未验收。详见 ../../doc/bicdb_按需写入覆盖层与提交验证_20261009.md 和 ../../doc/bicdb_独立语料聚合证明与冷图写入接入_20261009.md。

`corpus_proof` 的独立实体摘要、聚合 Merkle 树和局部 delta 已接入 SQL。kind-5 证明采用固定 4 KiB 分块和可复用墓碑，并与实体、索引、全文队列及 manifest 联合提交；冷启动点写通过同一快照核对实际观察源和证明路径。完整加载仍从真实实体重建证明，证明内部一致性不能代替实体核验。详见[实现与验证边界](../../doc/bicdb_独立语料聚合证明与冷图写入接入_20261009.md)。
