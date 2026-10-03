# 知识库命令可用性实测记录（2026-10-03）

配套文档：`doc/知识库使用计划与方案_v0.2.md` 第 1.4 节、第 4.12 节、附录 A。

## 内容

| 文件 | 说明 |
| --- | --- |
| `report.json` | 54 条命令的结构化判定结果（id、命令、判定、退出码、行数、首行摘要） |
| `run_checks.py` | 批量验证脚本，可复跑（`python3 run_checks.py`） |
| `scope-semantics-followup.txt` | 后续补充验证：`--scope` 取值语义随数据库与**实体类型**变化（修正初轮结论） |
| `<ID>.txt` | 每条命令的**完整原始输出**，ID 与计划文档第 4 节分组对应 |

## 判定口径

- `OK`：退出码 0 且返回有效内容
- `PLACEHOLDER`：命中"占位内容"（本轮 0 条）
- `EMPTY`：命中"未找到/无结果"类标记（本轮 1 条，经人工复核为分类器误判，实际有内容）
- `ERR`：退出码非 0（本轮 3 条，均为用法错误，已修正）

## 结果摘要

54 条实跑：**51 条返回有效内容 / 3 条非零退出**。

- `A-11` 是**有意设计的负例**（`entity want` 未知类型），返回正确报错，属预期行为。
- `A-12`、`A-23` 为用法错误（`entity knowledge` 不支持 `--limit`、`plsql_full_check` 数据库须为位置参数），修正见计划文档 4.12 节。
- `P11-2` 被自动分类器判为 `EMPTY`，人工复核确认为误判（正文有内容）。

## 补充验证（初轮之后的修正）

`scope-semantics-followup.txt` 记录了初轮之后发现的**结论修正**：初轮只测了 OB 系统视图，得出"OB 的 scope 是租户类型 `ORACLE`/`MYSQL`"；补充验证发现该结论**只对系统对象成立**——

| 数据库 / 实体类型 | scope 取值 |
| --- | --- |
| Oracle（容器语义） | `PDB` / `CDB_ROOT` / `NON_CDB` |
| OceanBase **系统对象**（view/table/column/function/param） | 租户类型 `ORACLE` / `MYSQL` |
| OceanBase **OpSQL**（opsql/opsqllist） | 目录范围标签 `SYS` / `MYSQL` |
| MySQL | 兼容解析，不据此筛选 |

教训：**单点实测不足以推广为通则**。scope 传错不报参数错误、只返回"未找到"，正是这类结论最容易掩盖错误的地方。

## 环境快照

| 项 | 值 |
| --- | --- |
| 客户端 | `/u01/deepseek/scripts/` |
| daemon | `kg-retrieval-daemon --serve`（属主 bicagent） |
| socket | `/run/kg-retrieval/kg-retrieval.sock` |
| 配置 | `KG_RETRIEVAL_RUNTIME_CONFIG=/u01/deepseek/scripts/config/kg_retrieval_runtime_config.json` |

## 复跑说明

```bash
cd doc/evidence/kbcheck-20261003
python3 run_checks.py     # 需 /u01/deepseek/scripts 与 daemon 可用
```

复跑会覆盖同目录下的 `<ID>.txt`。若知识库图谱有更新，应在新目录按日期留档，不覆盖本记录（核验日期是证据包必填项）。
