"""**连接**（PEP 249 的 ``Connection``）。

```text
连接 = 会话（协议 §8）：一条连接上的事务状态是连续的
```

**事务语义（照 DB-API，不是照引擎的自动提交）**：

- ``autocommit = False``（默认）：**写入语句前自动发 `BEGIN`**（DB-API 的隐式
  事务），由 :meth:`Connection.commit` / :meth:`Connection.rollback` 收尾；
- **纯查询不开事务**（不留长事务拖住撤销段）；
- **DDL 先提交再执行**（Oracle/MySQL 同款：DDL 不在显式事务里——引擎也
  明确拒绝"DDL 在显式事务里"）；
- ``autocommit = True``：每条语句自结（引擎的原生形态）。
"""

from __future__ import annotations

import decimal
from pathlib import Path
from typing import Iterable, Optional

from . import discover, exceptions, wire

#: 写入语句的首关键字（隐式事务判据）。本版 SQL 面只有 INSERT（见使用手册 §5）
#: ——表里留全是为了"以后支持 UPDATE/DELETE 时不用改驱动语义"。
_WRITE_KEYWORDS = frozenset({"insert", "update", "delete", "merge", "replace"})

#: DDL 首关键字（先提交再执行）。
_DDL_KEYWORDS = frozenset({"create", "drop", "alter", "truncate", "grant", "revoke"})


def _first_keyword(sql: str) -> str:
    """取 SQL 的首关键字（跳过注释与空白；大小写折叠）。"""
    text = sql.lstrip()
    while text.startswith("--") or text.startswith("/*"):
        if text.startswith("--"):
            nl = text.find("\n")
            if nl < 0:
                return ""
            text = text[nl + 1:].lstrip()
        else:
            end = text.find("*/")
            if end < 0:
                return ""
            text = text[end + 2:].lstrip()
    word = ""
    for ch in text:
        if ch.isalpha() or ch == "_":
            word += ch
        else:
            break
    return word.lower()


def python_value(v: wire.Value, kind: str, text_as_str: bool = True) -> object:
    """线上值 → Python 值（**按列形态**，不是按值猜）。

    | 形态 | Python |
    | --- | --- |
    | （NULL，任意列） | ``None`` |
    | ``n`` | ``decimal.Decimal``（**精确**——引擎的 NUMBER 任意精度） |
    | ``o`` | ``bool`` |
    | ``b`` | UTF-8 能解出 ⇒ ``str``；否则 **``bytes``**（不替换成 ``�``） |
    """
    if v.is_null():
        return None
    if kind == "n":
        text = v.as_decimal_text()
        if text is None:  # 服务说它是数值，却没有数值文本——报错，不猜
            raise exceptions.InterfaceError(f"列形态是数值，载荷却是 {v.tag!r}")
        return decimal.Decimal(text)
    if kind == "o":
        return bool(v.as_bool())
    raw = v.as_bytes()
    if text_as_str:
        try:
            return raw.decode("utf-8")
        except UnicodeDecodeError:
            return raw
    return raw


class Connection:
    """一条连接（= 一个会话）。"""

    def __init__(
        self,
        link: wire.Link,
        autocommit: bool = False,
        text_as_str: bool = True,
        identity: Optional[wire.AuthOk] = None,
    ) -> None:
        self._link = link
        self._closed = False
        self.autocommit = autocommit
        self.text_as_str = text_as_str
        #: **本连接的身份**（``None`` = 没认证：本机/OS 身份）。
        self.identity = identity
        self._in_txn = False
        self._cursors: list[object] = []

    # ── 构造 ──

    @classmethod
    def connect(
        cls,
        target: object = None,
        *,
        autocommit: bool = False,
        handshake_timeout: float = wire.Link.HANDSHAKE_TIMEOUT,
        socket_path: Optional[str] = None,
        text_as_str: bool = True,
        user: Optional[str] = None,
        password: Optional[str] = None,
    ) -> "Connection":
        """连上实例。

        :param target: 参数文件或**根区目录**（None ⇒ 按 ``$BICDB_INI``/``./bicdb.ini``）
        :param socket_path: 直接给套接字路径（跳过参数文件）
        :param handshake_timeout: 握手超时（秒）——实例忙时**报错**而不是挂住
        :param user: **主体名**（给了才认证；不给 = 本机/OS 身份）
        :param password: 口令（口令走**本机套接字**；服务端 PBKDF2 比对）
        """
        if socket_path is not None:
            path = socket_path
        else:
            try:
                path = str(discover.socket_for(target))
            except discover.DiscoverError as e:
                raise exceptions.InterfaceError(str(e)) from e
        return cls._from_path(
            path, autocommit, handshake_timeout, text_as_str, user, password
        )

    @classmethod
    def _from_path(
        cls,
        path: str,
        autocommit: bool,
        handshake_timeout: float,
        text_as_str: bool,
        user: Optional[str] = None,
        password: Optional[str] = None,
    ) -> "Connection":
        try:
            link = wire.Link.connect(path, handshake_timeout=handshake_timeout)
        except (ConnectionRefusedError, FileNotFoundError) as e:
            raise exceptions.OperationalError(f"连不上实例：{e}") from e
        except TimeoutError as e:
            raise exceptions.OperationalError(str(e)) from e
        except wire.ProtocolError as e:
            raise exceptions.InterfaceError(str(e)) from e
        # **认证**（给了主体名才做）：连上之后、第一条语句之前。
        identity = None
        if user is not None:
            try:
                identity = link.auth(user, password or "")
            except wire.ServerError as e:
                # **认证不通过也要把连接收掉**：服务端一次只服务一条连接，
                # 泄漏一条半开的连接 = 后续连接全拿"实例正忙"。
                link.close()
                # 服务端的具名文案**原样透传**（错误文本是契约的一部分；
                # 它自带 `认证失败：…` 前缀——不要再加一层）。
                raise exceptions.OperationalError(str(e)) from e
        return cls(link, autocommit=autocommit, text_as_str=text_as_str, identity=identity)

    # ── 元信息 ──

    @property
    def server_version(self) -> str:
        """服务/引擎版本（握手取）。"""
        return self._link.version

    @property
    def instance(self) -> str:
        """实例根区目录。"""
        return self._link.instance

    @property
    def wire_version(self) -> int:
        """协议版本。"""
        return self._link.wire

    @property
    def in_transaction(self) -> bool:
        """有未提交的显式事务吗。"""
        return self._in_txn

    def set_trace(self, on: bool = True) -> None:
        """开关协议 trace（每帧的动词与字节数打到 stderr）。"""
        self._link.trace = on

    # ── 执行 ──

    def cursor(self) -> "Cursor":
        """新游标（DB-API）。"""
        self._check_open()
        from .cursor import Cursor

        cur = Cursor(self)
        self._cursors.append(cur)
        return cur

    def execute(self, sql: str, params: Optional[object] = None) -> "Cursor":
        """便捷形态：开游标 + 执行（返回游标，便于 ``fetchall``）。"""
        cur = self.cursor()
        cur.execute(sql, params)
        return cur

    def _run(self, sql: str, params: Iterable[tuple[str, wire.Value]] = ()) -> list[object]:
        """内部：按 DB-API 语义补事务边界，再发语句。"""
        self._check_open()
        keyword = _first_keyword(sql)
        if not self.autocommit:
            if keyword in _DDL_KEYWORDS and self._in_txn:
                # DDL 不进显式事务（Oracle/MySQL 口径）：先把当前事务提交掉。
                self.commit()
            elif keyword in _WRITE_KEYWORDS and not self._in_txn:
                self._begin()
        try:
            outcomes = self._link.sql(sql, params)
        except wire.ServerError as e:
            raise exceptions.from_server(str(e)) from None
        except wire.ProtocolError as e:
            # 帧坏了 ⇒ 这条连接不可信：标成已关，别再用。
            self._closed = True
            raise exceptions.InterfaceError(f"协议错（连接已废弃）：{e}") from e
        except OSError as e:
            self._closed = True
            raise exceptions.OperationalError(f"连接断了：{e}") from e
        # **用户直接写的事务语句**：驱动的状态要跟上，否则下一次写入会
        # 又发一次 BEGIN（"事务已开（嵌套 BEGIN）"）。
        if keyword == "begin":
            self._in_txn = True
        elif keyword in ("commit", "rollback"):
            self._in_txn = False
        return outcomes

    # ── 事务 ──

    def _begin(self) -> None:
        try:
            self._link.sql("BEGIN")
        except wire.ServerError as e:
            raise exceptions.from_server(str(e)) from None
        self._in_txn = True

    def commit(self) -> None:
        """提交（没有活动事务时是**空操作**——与 DB-API 一致）。"""
        self._check_open()
        if not self._in_txn:
            return
        try:
            self._link.sql("COMMIT")
        except wire.ServerError as e:
            raise exceptions.from_server(str(e)) from None
        self._in_txn = False

    def rollback(self) -> None:
        """回滚（没有活动事务时是**空操作**）。"""
        self._check_open()
        if not self._in_txn:
            return
        try:
            self._link.sql("ROLLBACK")
        except wire.ServerError as e:
            raise exceptions.from_server(str(e)) from None
        self._in_txn = False

    # ── 扩展面（非 DB-API；本驱动的能力，够得着协议的全部） ──

    def describe(self, name: str) -> list[wire.Column]:
        """**列定义**（`DESCRIBE`）：名/形态/可空/类型码/长度/类型名。"""
        self._check_open()
        try:
            return self._link.describe(name)
        except wire.ServerError as e:
            raise exceptions.from_server(str(e)) from None

    def status(self) -> dict:
        """服务自述（``key=value``）。"""
        self._check_open()
        return self._link.status()

    def ping(self) -> bool:
        """还连得上吗（不发 SQL，只问服务自述）。"""
        if self._closed:
            return False
        try:
            self._link.status()
            return True
        except Exception:  # noqa: BLE001 - 判活本就"任何错都算不通"
            return False

    # ── 收尾 ──

    def close(self) -> None:
        """断开。未提交的显式事务由**服务端**回滚（协议 §8）。"""
        if self._closed:
            return
        for cur in list(self._cursors):
            cur.close()
        self._cursors.clear()
        self._link.close()
        self._closed = True

    def __enter__(self) -> "Connection":
        return self

    def __exit__(self, exc_type, exc, tb) -> None:
        # DB-API 的常见约定：正常退出提交、异常退出回滚（与 sqlite3 的
        # `with` 语义一致）。**连接本身不关**（调用方还可能接着用）。
        if exc_type is None:
            self.commit()
        else:
            self.rollback()

    def __repr__(self) -> str:
        state = "closed" if self._closed else ("txn" if self._in_txn else "idle")
        return f"<bicdb.Connection instance={self.instance!r} version={self.server_version!r} {state}>"

    def _check_open(self) -> None:
        if self._closed:
            raise exceptions.InterfaceError("连接已关闭")


# 循环导入的尾巴：cursor 模块要用 Connection；在文件末尾导入。
from .cursor import Cursor  # noqa: E402,F401
