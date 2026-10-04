# 证据包：图伴随对象与 `ref$` 定稿（Apache AGE 源码参照）

模块：存储架构 §9.2 / `spec/GRP.md` §0.2、§0.4（实现定稿追加）
对应：**待冻结项 #58（结案）**　核验日期：2026-10-04
原始输出：`raw/00–05`（同目录；01–02 为知识库检索未命中留痕，03–05 为 AGE 源码留档）

## 结论

1. **知识库没有 AGE 专域**（`postgresql` 域两轮检索未命中，见 `raw/01–02`）——
   参照改走 **AGE 源码**（`apache/age`，经代理获取 tarball，`raw/00` 存 sha256 溯源）。
2. **AGE 的目录组织**（源码级，2026-10-04）：
   - `ag_catalog.ag_graph(graphid oid, name, namespace)`——每图一行，含唯一索引（graphid / name / namespace）；
   - `ag_catalog.ag_label(name, graph FK, id, kind, relation, seq_name)`——每标签一行；
     `label_id` 域 = **1..65535（16 位，0 无效）**；`kind` = `'v'` / `'e'`；
     `relation` = 标签的物理表；**`seq_name` = 每标签一条独立序列**；
     唯一索引 (name, graph) / (graph, id) / (relation)；
   - **元素 id = 64 位复合 `graphid`：高 16 位标签号 + 低 48 位条目号**
     （`graphid.h`：`ENTRY_ID_BITS (32+16)`、`ENTRY_ID_MASK 0x0000ffffffffffff`、
     `GET_LABEL_ID(id) = id >> ENTRY_ID_BITS`）。
3. **逐面对照的结论**（#58 的定稿依据）：
   - **登记层**：AGE 用两张目录表 + 每图 schema + 每标签一张继承表；**我们不新增目录表**——
     图 = 顶点表自身的字典行 + **`graph 1b` 表选项**；标签 = 数据字段（顶点行 / 条目的 `label`）；
   - **id 层**：两者同为"**两段式**"——AGE 把标签段**编进** 64 位 id（16+48），
     我们把标签段**留在数据里**、id 用 48 位全局序列（D-01）——**方向收敛**；
   - **隔离层**：AGE 每图一个 PG schema；我们以工作区为隔离刻度（图共享表空间，伴随对象用命名后缀）。

## 证据

| # | 来源类型 | 条目/文件 | 数据库 | 主题 | 核验日期 | 适用条件 |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | **源码**（`apache/age` master） | `sql/age_main.sql` L49–95（`raw/03`） | PostgreSQL 扩展 | `ag_graph` / `ag_label` DDL；`label_id` 域 1..65535；`kind 'v'/'e'`；`seq_name`；唯一索引组 | 2026-10-04 | 目录形态 |
| 2 | **源码** | `src/include/catalog/ag_label.h`（`raw/04`） | PostgreSQL 扩展 | 顶点/边标签表元组布局（id+properties / id+start_id+end_id+properties）；`insert_label` 等接口 | 2026-10-04 | 标签表形态 |
| 3 | **源码** | `src/include/utils/graphid.h`（`raw/05`） | PostgreSQL 扩展 | **`graphid = int64`；LABEL_ID 16 位（1..65535）；ENTRY_ID 48 位；`GET_LABEL_ID` 右移 48** | 2026-10-04 | **id 复合布局** |
| 4 | 知识库检索 | `raw/01–02` | postgresql 域 | 两轮检索**未命中** AGE 条目（候选 0 / 6，相关 0） | 2026-10-04 | 留痕 |

## 检索范围声明

- 知识库：2 轮（未命中，见上）。
- 源码：经代理（`127.0.0.1:7890`）获取 `github.com/apache/age` master tarball
  （**sha256 = `32e920ba…ace0142b`**，`raw/00-age-tarball.sha256`），
  抽取 `sql/age_main.sql`、`ag_label.h`、`graphid.h` 相关段（`raw/03–05`）。
  **按 API 限流所限未列全目录**；抽取面覆盖本次对照所需的三处。

## 冲突与未决

| 项 | 知识库说法 | 官方文档/源码说法 | 处置 |
| --- | --- | --- | --- |
| AGE 语言面/插件形态 | — | 已在本项目 §0.3 定"取语言面、不取插件与继承式标签表" | 本包不推翻；本包补的是**目录形态的对照** |

## 本项目自主决策

1. **表选项 `graph 1b`**：`table_opts` 2B 位域收回第 9 位（原保留 9b → 8b）；
   仅顶点表置位；不提供 DDL 入口（建图 / 删图 = 引擎事务）。
2. **`ref$` 物理布局**：普通堆表；五列（`from_obj#` 4B / `from_rowid` 6B / `from_sub` 6B /
   `to_kind` 1B / `to_id` 6B）；**正向唯一** + **反向非唯一** B+Tree；列编码照通用规则不特判。
3. **回收路径**：无专用路径——普通行生命周期（删除即标记 → 提交后可复用；defrag 照常）；
   **失效不删行**（读时判定）；不依赖任何清理任务。
4. **AGE 参照的作用域 = 目录/登记形态**；边存储维持"按 `src` 聚簇"（§9.2，已冻结）。

## 未核验项

- AGE 的标签表**触发器**维护路径（`age_trig.sql`）与 `create_graph` 的完整函数体——本包未展开；
  与我们单表模型无对应物，**不需要**。
- AGE 在更高版本对目录的演进——以 master@2026-10-04 为准（sha256 已录）。
