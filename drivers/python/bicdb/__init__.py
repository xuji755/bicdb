"""# bicdb —— Python 驱动（PEP 249 / DB-API 2.0 的子集）

`docs/客户端协议_v0.1.md` 的 Python 实现（Rust 侧是 `bicdb-net` + `bicdb-client`，
**同一份规格**）。只用标准库。

```python
import bicdb

conn = bicdb.connect("/data/bicdb")          # 参数文件或根区目录（协议 §1）
cur = conn.cursor()
cur.execute("CREATE TABLE t (id NUMBER NOT NULL, name VARCHAR2(32))")
cur.execute("INSERT INTO t VALUES (:id, :name)", {"id": 1, "name": "alpha"})
conn.commit()

cur.execute("SELECT id, name FROM t ORDER BY id")
print(cur.description[0][0], cur.fetchall())  # id [('1', 'alpha')] → (Decimal('1'), 'alpha')
conn.close()
```

## 能力与边界

| 有 | 没有（后续切片） |
| --- | --- |
| 本机连接、具名参数、结果集、`describe`、服务自述 | TCP/多机、认证与授权 |
| **一条连接 = 一个会话**（事务跨语句；写入前自动 `BEGIN`，DB-API 语义） | 连接池、服务端游标分页、异步/流式 |
| 值的无损传递（字节串原字节；非 UTF-8 **不替换**成 `�`） | 协议全量（封套/幂等/CAS/ACK 对账） |

**两条要知道的实例性质**（协议 §8；服务端 V1.0 的单写者纪律）：

1. 服务**一次只服务一条连接**——第二条连接会被明确挡回（`OperationalError`，
   "实例正忙"），不是无声挂住；
2. 一条连接上**一次只有一个未决请求**（同步一问一答）；要并发请开多条连接
   （在 V1.0 里就是排队）。
"""

from __future__ import annotations

from . import exceptions, wire
from .connection import Connection
from .cursor import Cursor
from .exceptions import (  # noqa: F401 - 供 `except bicdb.X` 用
    DataError,
    DatabaseError,
    Error,
    IntegrityError,
    InterfaceError,
    InternalError,
    NotSupportedError,
    OperationalError,
    ProgrammingError,
    Warning,
)

__version__ = "0.2.0"
__all__ = [
    "connect",
    "Connection",
    "Cursor",
    "Warning",
    "Error",
    "InterfaceError",
    "OperationalError",
    "DatabaseError",
    "DataError",
    "IntegrityError",
    "InternalError",
    "ProgrammingError",
    "NotSupportedError",
    "apilevel",
    "threadsafety",
    "paramstyle",
    "Binary",
    "Date",
    "Time",
    "Timestamp",
    "DateFromTicks",
    "TimeFromTicks",
    "TimestampFromTicks",
    "STRING",
    "BINARY",
    "NUMBER",
    "DATETIME",
    "BOOLEAN",
    "ROWID",
    "__version__",
]

# ── PEP 249 的模块级声明 ──

#: 支持的 DB-API 版本。
apilevel = "2.0"

#: 线程安全级别：连接可跨线程共享吗？**1 = 可以共享模块，连接不可以**。
#: （一条连接一次只有一个未决请求——多线程用一条连接必须自己加锁。）
threadsafety = 1

#: 参数风格：SQL 里写 `:名字`，参数给 dict（与引擎的具名参数同形）。
paramstyle = "named"


def connect(
    target: object = None,
    *,
    autocommit: bool = False,
    handshake_timeout: float = wire.Link.HANDSHAKE_TIMEOUT,
    socket_path: str | None = None,
    text_as_str: bool = True,
    user: str | None = None,
    password: str | None = None,
) -> Connection:
    """连上实例（PEP 249 的 ``connect``）。

    :param target: 参数文件或**根区目录**（``None`` ⇒ ``$BICDB_INI``、再退
        到当前目录的 ``./bicdb.ini``）
    :param autocommit: ``False``（默认）＝ DB-API 语义（写入前自动 `BEGIN`，
        由 ``commit``/``rollback`` 收尾）；``True`` = 每条语句自结（引擎原生）
    :param handshake_timeout: 握手超时秒数（实例忙时报错，不挂住）
    :param socket_path: 直接给套接字路径（跳过参数文件寻址）
    :param text_as_str: 字节串列能按 UTF-8 解出就给 ``str``（否则仍给 ``bytes``）
    :param user: **主体名**（给了才认证；不给 = 本机/OS 身份）
    :param password: 口令（只应走本机套接字；服务端 PBKDF2 比对）
    """
    # 注：引擎没有"隐式事务"这个开关（协议里只有 BEGIN/COMMIT/ROLLBACK）——
    # `autocommit=False` 的语义由驱动补（写入语句前自动发 BEGIN，
    # 见 `Connection._run`）。
    return Connection.connect(
        target,
        autocommit=autocommit,
        handshake_timeout=handshake_timeout,
        socket_path=socket_path,
        text_as_str=text_as_str,
        user=user,
        password=password,
    )


# ── PEP 249 的类型对象（与 `Cursor.description[i][1]` 可比） ──


class _DBAPITypeObject:
    """PEP 249 的类型对象（拿它与 ``description[i][1]`` 比）。"""

    def __init__(self, name: str, *codes: object) -> None:
        self.name = name
        self.codes = codes

    def __eq__(self, other: object) -> bool:
        return other in self.codes

    def __ne__(self, other: object) -> bool:
        return other not in self.codes

    def __hash__(self) -> int:
        return hash(self.name)

    def __repr__(self) -> str:
        return f"bicdb.{self.name}"


# 引擎的类型码见 `ColTypeCode`（1 NUMBER / 2 CHAR / 3 VARCHAR2 / 4 DATE /
# 5 TIMESTAMP / 6 BOOLEAN / 7 UUID / 8 TIMESTAMP_TZ / 9 JSON / 10 VECTOR /
# 11 ASSET_REF / 12 BYTES）。
STRING = _DBAPITypeObject("STRING", 2, 3, 7, 9, 12)
BINARY = _DBAPITypeObject("BINARY", 12, 11, 10)
NUMBER = _DBAPITypeObject("NUMBER", 1)
DATETIME = _DBAPITypeObject("DATETIME", 4, 5, 8)
BOOLEAN = _DBAPITypeObject("BOOLEAN", 6)
ROWID = _DBAPITypeObject("ROWID", 0)


def Binary(data: object) -> bytes:
    """PEP 249 的 ``Binary``（本驱动：字节串就是 ``bytes``）。"""
    return bytes(data)  # type: ignore[arg-type]


def Date(year: int, month: int, day: int) -> str:
    """``DATE``（引擎的 DATE 是 7 字节；驱动按 ISO 文本传）。"""
    return f"{year:04d}-{month:02d}-{day:02d}"


def Time(hour: int, minute: int, second: int) -> str:
    """``TIME``（同上）。"""
    return f"{hour:02d}:{minute:02d}:{second:02d}"


def Timestamp(year: int, month: int, day: int, hour: int, minute: int, second: int) -> str:
    """``TIMESTAMP``（同上）。"""
    return f"{Date(year, month, day)} {Time(hour, minute, second)}"


def DateFromTicks(ticks: float) -> str:
    """由 epoch 秒构造 ``DATE``。"""
    import time as _time

    t = _time.localtime(ticks)
    return Date(t.tm_year, t.tm_mon, t.tm_mday)


def TimeFromTicks(ticks: float) -> str:
    """由 epoch 秒构造 ``TIME``。"""
    import time as _time

    t = _time.localtime(ticks)
    return Time(t.tm_hour, t.tm_min, t.tm_sec)


def TimestampFromTicks(ticks: float) -> str:
    """由 epoch 秒构造 ``TIMESTAMP``。"""
    import time as _time

    t = _time.localtime(ticks)
    return Timestamp(t.tm_year, t.tm_mon, t.tm_mday, t.tm_hour, t.tm_min, t.tm_sec)
