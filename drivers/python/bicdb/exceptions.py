"""异常层次（PEP 249 的那一套；**服务的具名错误原文**进 ``message``）。

```text
Exception
└── Error                       bicdb 的错误基类
    ├── InterfaceError          驱动/连接本身的用法错（没连上、游标已关…）
    │   └── OperationalError    连不上、实例忙、连接断了
    │       └── DatabaseError   服务说"这个请求不行"
    │           ├── DataError       值/形态不对
    │           ├── IntegrityError  唯一键等完整性
    │           ├── InternalError   服务内部错
    │           ├── ProgrammingError SQL 写错了（表/列不存在、语法）
    │           └── NotSupportedError 本版不支持（引擎具名拒绝"绑定期不支持"）
```

**一条纪律**：服务的错误文本是**给人看的具名文本**（哪个对象、哪个参数、
哪一行），驱动**不改写、不翻译、不截断**——选择异常类型只是给程序一个
可判别的把手（``except bicdb.IntegrityError``），文本仍然是原文。
"""


class Warning(Exception):  # noqa: N818 —— PEP 249 规定的名字
    """PEP 249 的 Warning（保留）。"""


class Error(Exception):
    """所有 bicdb 错误的基类。"""


class InterfaceError(Error):
    """驱动侧的用法错（不是服务报的）。"""


class OperationalError(InterfaceError):
    """运行环境问题：连不上、实例忙、连接断了。"""


class DatabaseError(Error):
    """服务报的错（``message`` 是**原文**）。"""


class DataError(DatabaseError):
    """值/形态不对。"""


class IntegrityError(DatabaseError):
    """完整性约束（唯一键等）。"""


class InternalError(DatabaseError):
    """服务内部错。"""


class ProgrammingError(DatabaseError):
    """SQL 或调用的用法错（对象不存在、语法错…）。"""


class NotSupportedError(DatabaseError):
    """本版明确不支持（引擎会具名拒绝）。"""


#: 服务端错误原文 → 异常类型的**判据表**（前缀/子串 → 类型）。
#:
#: 为什么按文本判：服务没有错误码通道（协议 v0.1 的 `ERR` 只有文本）。
#: 这里的映射**只用于选异常类**——文本本身永远原样保留。协议加"错误码"
#: 字段（向后兼容，不升 wire 号）之后，这张表就该退场。
_RULES: tuple[tuple[str, type[DatabaseError]], ...] = (
    # **"未实现"排在最前**：引擎的"本版不支持"拒绝里可能带上别的字样
    # （例：`UPDATE 改唯一索引 … 的键列：本版未实现` 同时含"唯一"），
    # 那条的**原因**是不支持，不是完整性冲突——真实的唯一冲突文案是
    # "唯一约束冲突（重复键 …）"，不含"未实现"。
    ("未实现", NotSupportedError),
    ("唯一约束冲突", IntegrityError),
    ("唯一", IntegrityError),
    ("重复键", IntegrityError),
    ("绑定期不支持", NotSupportedError),
    ("不支持", NotSupportedError),
    ("不存在", ProgrammingError),
    ("语法错误", ProgrammingError),
    ("没有参数", ProgrammingError),
    ("参数", ProgrammingError),
    ("事务", ProgrammingError),
    ("类型", DataError),
    ("目录", ProgrammingError),
)


def from_server(message: str) -> DatabaseError:
    """把服务的 `ERR` 原文包成最合适的异常（**文本原样**）。"""
    for needle, kind in _RULES:
        if needle in message:
            return kind(message)
    return DatabaseError(message)
