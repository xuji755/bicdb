# bicdb —— Python 驱动

`docs/客户端协议_v0.1.md` 的 Python 实现（**只用标准库**；Rust 侧是
`bicdb-net` + `bicdb-client`，同一份规格）。PEP 249（DB-API 2.0）的子集。

```python
import bicdb

conn = bicdb.connect("/data/bicdb")            # 参数文件或根区目录；或 $BICDB_INI
cur = conn.cursor()
cur.execute("CREATE TABLE t (id NUMBER NOT NULL, name VARCHAR2(32))")
cur.execute("INSERT INTO t VALUES (:id, :name)", {"id": 1, "name": "alpha"})
# 写入前驱动自动发 BEGIN（DB-API 的隐式事务）——由 commit/rollback 收尾
conn.commit()

cur.execute("SELECT id, name FROM t WHERE id = :id", {"id": 1})
print(cur.description[0][0])   # 'id'
print(cur.fetchall())          # [(Decimal('1'), 'alpha')]
conn.close()
```

**要先把服务起起来**（驱动连的是控制套接字）：

```bash
bicdb init /data/bicdb
bicdb start -p /data/bicdb          # 起服务（后台）
# … 用驱动 ……（见上）
bicdb stop -p /data/bicdb
```

## PEP 249 声明

| 项 | 值 | 说明 |
| --- | --- | --- |
| `apilevel` | `"2.0"` | |
| `threadsafety` | `1` | 模块可共享；**连接不可**（一条连接一次只有一个未决请求） |
| `paramstyle` | `"named"` | SQL 里写 `:名字`，参数给 `dict`；也给**序列**（按参数出现顺序配名） |
| 异常 | `bicdb.Error` 及其子类 | `IntegrityError`/`ProgrammingError`/`NotSupportedError`/`OperationalError`/… |
| 类型对象 | `bicdb.STRING`/`NUMBER`/`BINARY`/`DATETIME`/`BOOLEAN`/`ROWID` | 与 `description[i][1]` 可比（`DESCRIBE` 给引擎类型码；结果集只有形态） |
| `Binary`/`Date`/`Time`/`Timestamp`/`*FromTicks` | 有 | 日期时间按 ISO 文本传（引擎的 `DATE`/`TIMESTAMP` 是字节串） |

## 值的映射（**不猜**）

| SQL | Python（出） | Python（入） |
| --- | --- | --- |
| `NULL` | `None` | `None` |
| `NUMBER` | `decimal.Decimal`（**精确**） | `int` / `float` / `Decimal` / `bool` |
| `BOOLEAN` | `bool` | `bool` |
| 字节串列（`VARCHAR2`/`CHAR`/`RAW`…） | UTF-8 能解出 ⇒ `str`；否则 **`bytes`** | `str` / `bytes` |

- **非 UTF-8 的字节不会被替换成 `�`**：解不出就给 `bytes`（原字节无损）。
  想一律拿原字节：`bicdb.connect(..., text_as_str=False)`。
- **数值留 `Decimal`**：引擎的 `NUMBER` 是任意精度，转 `float` 会丢精度——
  要 `int`/`float` 自己转（`int(d)` / `float(d)`）。

## 事务（DB-API 语义，不是引擎的自动提交）

| 模式 | 行为 |
| --- | --- |
| `autocommit=False`（默认） | **写入语句前自动 `BEGIN`**；`commit()`/`rollback()` 收尾；**纯查询不开事务**；**DDL 先提交再执行**（Oracle/MySQL 口径） |
| `autocommit=True` | 每条语句自结（引擎的原生形态） |

在游标里直接写 `BEGIN`/`COMMIT`/`ROLLBACK` 也认（驱动会跟上状态）。
连接断开时服务端回滚未提交的显式事务（协议 §8）。`with conn:` 正常退出提交、
异常退出回滚。

## 实例的两条性质（用之前要知道）

1. **一个实例可同时保持多条独立连接**：每条连接有自己的认证身份、快照和显式事务；
   空闲连接在下一条请求前刷新到最新提交。连接数由 `[service] max_connections`
   控制（默认 64）。
2. **一条连接一次只有一个未决请求**（同步一问一答）；实例执行器按请求串行执行语句。
   连接断开时服务端回滚该连接的未提交事务。

## 边界（与协议 v0.1 一致）

| 有 | 没有（后续切片） |
| --- | --- |
| 本机连接、具名参数、结果集、`describe`、服务自述、协议 trace | TCP/多机、认证与授权 |
| 一条连接 = 一个会话（事务跨语句） | 连接池、**服务端游标分页**（结果在 `execute` 时全部取回）、异步 |
| 值的无损传递（含非 UTF-8 字节） | 协议全量（封套/幂等与 CAS/ACK 对账） |

## 测试

```bash
# 协议字节（不起服务；与 Rust 侧 `wire_v01_frozen.rs` 同一串字面量）
python3 -m unittest discover -s drivers/python/tests -p 'test_wire.py'

# 实机（要先 cargo build，或给 BICDB_BIN）
cargo build
python3 -m unittest discover -s drivers/python/tests
```

仓库根跑：`python3 -m unittest discover -s drivers/python/tests`（用例自己
`bicdb init`/`start`/`stop` 一个临时实例）。
