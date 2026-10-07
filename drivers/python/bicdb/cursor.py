"""**游标**（PEP 249 的 ``Cursor``）。

结果在 ``execute`` 时就**全部取回**（协议 v0.1 没有游标分页——这是它明确的
边界；`fetchmany`/`fetchall` 只是从内存里拿）。行是 **tuple**，列序与
:attr:`Cursor.description` 一致。
"""

from __future__ import annotations

from typing import Iterable, Optional, Sequence  # noqa: F401 - 注解用

from . import exceptions, wire
from .connection import python_value


class Cursor:
    """一条语句的结果面。"""

    def __init__(self, conn) -> None:
        self._conn = conn
        self._closed = False
        self._rows: list[tuple] = []
        self._pos = 0
        self._description: Optional[list[tuple]] = None
        self._rowcount = -1
        self._columns: list[wire.Column] = []
        self.arraysize = 1
        self._message = ""

    # ── 执行 ──

    def execute(self, sql: str, params: Optional[object] = None) -> "Cursor":
        """执行一条语句（``params`` = 具名参数：``{":名字 去冒号": 值}`` 或序列）。

        具名参数写 ``{"id": 1}``（SQL 里写 ``:id``）；序列则按 **位置** 配给
        SQL 里**出现顺序**的参数名——两种写法都行，混用不行。
        """
        self._check_open()
        pairs = self._bind(sql, params)
        outcomes = self._conn._run(sql, pairs)
        self._set_outcome(outcomes)
        return self

    def executemany(self, sql: str, seq_of_params: Iterable[object]) -> "Cursor":
        """逐组参数执行同一条语句（``rowcount`` = 各组之和）。"""
        self._check_open()
        total = 0
        for params in seq_of_params:
            self.execute(sql, params)
            if self._rowcount > 0:
                total += self._rowcount
        self._rowcount = total
        return self

    def _bind(self, sql: str, params: Optional[object]) -> list[tuple[str, wire.Value]]:
        if params is None:
            return []
        if isinstance(params, dict):
            return [(str(k).lstrip(":"), wire.python_to_value(v)) for k, v in params.items()]
        if isinstance(params, (str, bytes, bytearray)):
            raise exceptions.ProgrammingError("参数要 dict（具名）或序列（按位置），不是单个字符串")
        names = _param_names(sql)
        values = list(params)  # type: ignore[arg-type]
        if len(names) != len(values):
            raise exceptions.ProgrammingError(
                f"参数个数不符：SQL 里有 {len(names)} 个（{', '.join(names) or '无'}），"
                f"给了 {len(values)} 个"
            )
        return list(zip(names, (wire.python_to_value(v) for v in values)))

    def _set_outcome(self, outcomes: Sequence[object]) -> None:
        self._rows = []
        self._pos = 0
        self._description = None
        self._columns = []
        self._message = ""
        last = outcomes[-1] if outcomes else None
        if isinstance(last, wire.Rows):
            self._columns = last.columns
            self._description = [
                (
                    c.name,
                    _type_code(c),
                    None,  # display_size
                    c.length or None,  # internal_size
                    None,  # precision
                    None,  # scale
                    c.nullable,
                )
                for c in last.columns
            ]
            text_as_str = bool(self._conn.text_as_str)
            self._rows = [
                tuple(
                    python_value(cell, last.columns[i].kind if i < len(last.columns) else "b", text_as_str)
                    for i, cell in enumerate(row)
                )
                for row in last.rows
            ]
            self._rowcount = len(self._rows)
        elif isinstance(last, wire.Affected):
            self._rowcount = last.count
        elif isinstance(last, wire.Ddl):
            self._rowcount = 0
            self._message = last.text
        elif isinstance(last, wire.Txn):
            self._rowcount = 0
            self._message = last.text
        else:
            self._rowcount = -1

    # ── 取结果 ──

    def fetchone(self) -> Optional[tuple]:
        """下一行；没有了给 ``None``。"""
        self._check_open()
        if self._pos >= len(self._rows):
            return None
        row = self._rows[self._pos]
        self._pos += 1
        return row

    def fetchmany(self, size: Optional[int] = None) -> list[tuple]:
        """最多 ``size`` 行（默认 ``arraysize``）。"""
        self._check_open()
        n = self.arraysize if size is None else size
        out = self._rows[self._pos:self._pos + max(0, n)]
        self._pos += len(out)
        return out

    def fetchall(self) -> list[tuple]:
        """剩下的全部行。"""
        self._check_open()
        out = self._rows[self._pos:]
        self._pos = len(self._rows)
        return out

    # ── 元信息 ──

    @property
    def description(self) -> Optional[list[tuple]]:
        """``(name, type_code, display_size, internal_size, precision, scale, null_ok)``。"""
        return self._description

    @property
    def rowcount(self) -> int:
        """影响/取回的行数（结果集 = 行数；DML = 影响数；DDL/事务 = 0）。"""
        return self._rowcount

    @property
    def lastrowid(self) -> None:
        """本版没有自增行号（见使用手册 §5）。"""
        return None

    @property
    def message(self) -> str:
        """DDL/事务回执的文本（**服务的原文**；非 DB-API，方便脚本打日志）。"""
        return self._message

    @property
    def columns(self) -> list[wire.Column]:
        """列定义（含形态与类型码；非 DB-API）。"""
        return self._columns

    # ── 收尾 ──

    def close(self) -> None:
        """关游标（结果仍在已取回的那份内存里；再取就报错）。"""
        self._closed = True
        self._rows = []
        self._pos = 0

    def setinputsizes(self, sizes) -> None:
        """PEP 249 的空实现（本驱动按 Python 类型定型）。"""

    def setoutputsize(self, size, column=None) -> None:
        """PEP 249 的空实现。"""

    def __iter__(self) -> "Cursor":
        return self

    def __next__(self) -> tuple:
        row = self.fetchone()
        if row is None:
            raise StopIteration
        return row

    def __enter__(self) -> "Cursor":
        return self

    def __exit__(self, exc_type, exc, tb) -> None:
        self.close()

    def __repr__(self) -> str:
        state = "closed" if self._closed else f"rows={len(self._rows)} at={self._pos}"
        return f"<bicdb.Cursor {state}>"

    def _check_open(self) -> None:
        if self._closed:
            raise exceptions.InterfaceError("游标已关闭")


def _type_code(c: wire.Column) -> object:
    """列的 ``type_code``：结果集里没有真类型码 ⇒ 按**形态**给 DB-API 类型对象。

    （`DESCRIBE` 给的是引擎类型码，与 :mod:`bicdb` 里的 ``NUMBER``/``STRING``
    等对象可比；结果集只有形态——这就是协议 §0 记档的边界。）
    """
    from . import BINARY, BOOLEAN, NUMBER, STRING

    if c.type_code:
        return c.type_code
    if c.kind == "n":
        return NUMBER
    if c.kind == "o":
        return BOOLEAN
    return STRING


def _param_names(sql: str) -> list[str]:
    """SQL 里 :名字 的出现顺序（跳过 ``'字符串'``、``--注释``、``::`` 转型）。"""
    names: list[str] = []
    i, n = 0, len(sql)
    while i < n:
        ch = sql[i]
        if ch == "'":
            j = sql.find("'", i + 1)
            i = n if j < 0 else j + 1
            continue
        if ch == "-" and sql.startswith("--", i):
            j = sql.find("\n", i)
            i = n if j < 0 else j + 1
            continue
        if ch == ":":
            j = i + 1
            if j < n and (sql[j].isalpha() or sql[j] == "_"):
                while j < n and (sql[j].isalnum() or sql[j] == "_"):
                    j += 1
                names.append(sql[i + 1:j])
                i = j
                continue
        i += 1
    return names
