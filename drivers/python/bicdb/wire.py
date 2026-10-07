"""**协议 v0.1 的线格式**（`docs/客户端协议_v0.1.md` 的 Python 实现）。

本模块只做字节：帧、值单元、动词载荷。**不认识引擎**（与 Rust 侧
`bicdb-net` 同一份规格；两侧的示例字节冻结在同一串字面量上——
本文件 `test_wire.py` 与 `crates/net/tests/wire_v01_frozen.rs`）。

```text
帧      <首行> \\n <载荷字节数> \\n <载荷字节…>
值单元  <标记><载荷字节数> \\n <载荷字节…> \\n
        - NULL │ n 十进制数值文本 │ o 布尔(0/1) │ b 十六进制字节串
```

**解析纪律（照 MySQL 包序号错乱的教训）**：每个变长字段自带字节数，
按**字节游标**读满；定长字段认不出就报错——**绝不"按行补读"**，
那会让后续字段整体错位而且看不出错。
"""

from __future__ import annotations

import socket
from dataclasses import dataclass, field
from typing import Iterable, Optional

#: 协议版本（与 `bicdb-net` 的 `WIRE_VERSION` 同源；不兼容变更才 +1）。
WIRE_VERSION = 1

#: 单帧载荷上限（与 Rust 侧同为 64 MiB）：坏对端不该让对方分配无界内存。
MAX_FRAME = 64 * 1024 * 1024

#: 首行上限。
MAX_HEAD_LINE = 1024

#: 帧类型（应答）。
OK = "OK"
ERR = "ERR"


class ProtocolError(Exception):
    """帧/载荷非法（**这条连接的字节流已不可信**）。"""


class ServerError(Exception):
    """服务回了 `ERR`（原文在 `args[0]`；连接仍可用）。"""


# ───────────────────────────── 值 ─────────────────────────────


@dataclass(frozen=True)
class Value:
    """线上的一个值（四种形态）。

    - ``tag``：``-`` / ``n`` / ``o`` / ``b``；
    - ``text``：``n``/``o`` 的文本（十进制/``0``|``1``）；``b`` 是十六进制；``-`` 为空。
    """

    tag: str
    text: str = ""

    @staticmethod
    def null() -> "Value":
        return Value("-")

    def is_null(self) -> bool:
        return self.tag == "-"

    def as_bytes(self) -> bytes:
        """原字节（``b`` 形态；NULL ⇒ 报错——**不悄悄给空**）。"""
        if self.tag == "b":
            return bytes.fromhex(self.text)
        raise ValueError(f"不是字节串形态：{self.tag!r}")

    def as_decimal_text(self) -> Optional[str]:
        return self.text if self.tag == "n" else None

    def as_bool(self) -> Optional[bool]:
        return self.text == "1" if self.tag == "o" else None

    def as_int(self) -> Optional[int]:
        if self.tag != "n":
            return None
        try:
            return int(self.text)
        except ValueError:
            return None

    def as_float(self) -> Optional[float]:
        if self.tag != "n":
            return None
        try:
            return float(self.text)
        except ValueError:
            return None


def encode_cell(value: Value) -> bytes:
    """值单元编码：``<标记><字节数>\\n<载荷>\\n``。"""
    if value.tag == "-":
        return b"-0\n\n"
    body = value.text.encode("ascii")
    return value.tag.encode("ascii") + str(len(body)).encode("ascii") + b"\n" + body + b"\n"


def python_to_value(v: object) -> Value:
    """Python 值 → 线上值（**不猜**：不认识的类型报错，不偷偷 str()）。

    | Python | 线上 |
    | --- | --- |
    | ``None`` | NULL |
    | ``bool`` | ``o``（**先判 bool**——它是 int 的子类） |
    | ``int``/``float``/``Decimal`` | ``n``（十进制文本；float 走 ``repr`` 保往返） |
    | ``str`` | ``b``（UTF-8 字节） |
    | ``bytes``/``bytearray``/``memoryview`` | ``b``（原字节） |
    """
    import decimal

    if v is None:
        return Value.null()
    if isinstance(v, bool):
        return Value("o", "1" if v else "0")
    if isinstance(v, (int, decimal.Decimal)):
        return Value("n", str(v))
    if isinstance(v, float):
        # `repr` 保证 round-trip（`str` 在旧版本上会丢位）。
        return Value("n", repr(v))
    if isinstance(v, str):
        return Value("b", v.encode("utf-8").hex())
    if isinstance(v, (bytes, bytearray, memoryview)):
        return Value("b", bytes(v).hex())
    raise TypeError(f"不支持的参数类型 {type(v).__name__}（要 str/bytes/int/float/Decimal/bool/None）")


# ───────────────────────────── 字节游标 ─────────────────────────────


class Reader:
    """按**声明的字节数**读满的游标（不按行切分）。"""

    def __init__(self, buf: bytes) -> None:
        self.raw = buf
        self.pos = 0

    def done(self) -> bool:
        return self.pos >= len(self.raw)

    def _line(self) -> bytes:
        """读一行（不含换行；到载荷末尾也算一行）。"""
        end = self.raw.find(b"\n", self.pos)
        if end < 0:
            out = self.raw[self.pos:]
            self.pos = len(self.raw)
            return out
        out = self.raw[self.pos:end]
        self.pos = end + 1
        return out

    def peek(self) -> bytes:
        return self.raw[self.pos:self.pos + 1]

    def _take(self, n: int, what: str) -> bytes:
        if self.pos + n > len(self.raw):
            raise ProtocolError(f"{what}：要 {n} 字节，载荷只剩 {len(self.raw) - self.pos} 字节")
        out = self.raw[self.pos:self.pos + n]
        self.pos += n
        return out

    def _nl(self, what: str) -> None:
        if self.peek() == b"\n":
            self.pos += 1
            return
        raise ProtocolError(f"{what}：字段后应是换行")

    def head(self, tag: str, what: str) -> int:
        """``<标记> <数>`` 头行。"""
        line = self._line()
        if not line.startswith(tag.encode("ascii")):
            raise ProtocolError(f"{what}：期待标记 `{tag}`，读到 `{line[:1].decode('latin1')}`")
        try:
            return int(line[1:].strip() or b"0")
        except ValueError as e:
            raise ProtocolError(f"{what}：头行 {line!r} 里不是数") from e

    def byte_prefixed(self, tag: str, what: str) -> bytes:
        """``<标记> <字节数>\\n<字节>\\n``。"""
        n = self.head(tag, what)
        body = self._take(n, what)
        self._nl(what)
        return body

    def cell(self) -> Value:
        line = self._line()
        if not line:
            raise ProtocolError("值单元：空行")
        tag = chr(line[0])
        try:
            n = int(line[1:].strip() or b"0")
        except ValueError as e:
            raise ProtocolError(f"值单元：头行 {line!r} 里不是字节数") from e
        body = self._take(n, "值单元")
        self._nl("值单元")
        return Value(tag, body.decode("ascii", "replace"))

    def column(self) -> "Column":
        line = self._line().decode("utf-8", "replace")
        parts = line.split()
        if len(parts) != 7 or parts[0] != "C":
            raise ProtocolError(f"列记录字段不全：{line!r}")
        _, kind, nullable, type_code, length, nlen, tlen = parts
        name = self._take(int(nlen), "列名")
        self._nl("列名")
        type_name = self._take(int(tlen), "类型名")
        self._nl("类型名")
        return Column(
            name=name.decode("utf-8", "replace"),
            kind=kind[0] if kind else "b",
            nullable=nullable == "1",
            type_code=int(type_code),
            length=int(length),
            type_name=type_name.decode("utf-8", "replace"),
        )

    def finish(self) -> None:
        if not self.done():
            raise ProtocolError(f"载荷还有 {len(self.raw) - self.pos} 字节没读完")


# ───────────────────────────── 列与语句 ─────────────────────────────


@dataclass
class Column:
    """一列（结果集与 `DESCRIBE` 同形；结果集里元数据是占位值）。"""

    name: str
    kind: str
    nullable: bool = True
    type_code: int = 0
    length: int = 0
    type_name: str = ""


@dataclass
class Rows:
    """结果集。"""

    columns: list[Column] = field(default_factory=list)
    rows: list[list[Value]] = field(default_factory=list)


@dataclass
class Affected:
    """影响行数。"""

    count: int


@dataclass
class Ddl:
    """DDL 回执。"""

    text: str


@dataclass
class Txn:
    """事务回执。"""

    text: str


def encode_sql(sql: str, params: Iterable[tuple[str, Value]] = ()) -> bytes:
    """`SQL` 请求载荷（协议 §5.1）。"""
    params = list(params)
    out = bytearray()
    out += f"P {len(params)}\n".encode("ascii")
    for name, value in params:
        raw = name.encode("utf-8")
        out += f"N {len(raw)}\n".encode("ascii")
        out += raw + b"\n"
        out += encode_cell(value)
    raw_sql = sql.encode("utf-8")
    out += f"Q {len(raw_sql)}\n".encode("ascii")
    out += raw_sql + b"\n"
    return bytes(out)


def encode_auth(user: str, password: str) -> bytes:
    """`AUTH` 请求载荷（协议 §4.3）：主体名 + 口令，两段都自带字节数。

    **口令只应走本机套接字**（文件权限保护）；载荷里没有"用户号"——
    身份由服务端从目录解析（REQ-ISO-002）。
    """
    out = bytearray()
    for tag, text in (("U", user), ("S", password)):
        raw = text.encode("utf-8")
        out += f"{tag} {len(raw)}\n".encode("ascii")
        out += raw + b"\n"
    return bytes(out)


def decode_auth_ok(payload: bytes) -> dict[str, object]:
    """`AUTH` 的 OK 应答（`key=value` 若干行，与 `HELLO` 同规）。"""
    got: dict[str, str] = {}
    for line in payload.decode("utf-8", "replace").splitlines():
        if "=" in line:
            k, v = line.split("=", 1)
            got[k] = v.strip()
    return {
        "user": got.get("user", ""),
        "user_id": int(got.get("user_id", "0") or "0"),
        "expired": got.get("status", "") == "expired",
    }


@dataclass
class AuthOk:
    """认证应答（`AUTH` 成功）：谁在连、口令是否已过期。"""

    user: str
    user_id: int
    expired: bool = False

    @property
    def describe(self) -> str:
        """展示名（与 Rust 侧同一口径）。"""
        if self.expired:
            return f"主体 `{self.user}`（{self.user_id}；口令已过期 ⇒ 受限会话）"
        return f"主体 `{self.user}`（{self.user_id}）"


def decode_statements(payload: bytes) -> list[object]:
    """语句结果序列（协议 §5.2）。"""
    r = Reader(payload)
    out: list[object] = []
    while not r.done():
        mark = r.peek()
        if mark == b"R":
            line = r._line().decode("ascii")
            parts = line.split()
            if len(parts) != 3:
                raise ProtocolError(f"结果集头行非法：{line!r}")
            ncols, nrows = int(parts[1]), int(parts[2])
            columns = [r.column() for _ in range(ncols)]
            rows = [[r.cell() for _ in range(ncols)] for _ in range(nrows)]
            out.append(Rows(columns, rows))
        elif mark == b"A":
            out.append(Affected(r.head("A", "影响行数")))
        elif mark == b"D":
            out.append(Ddl(r.byte_prefixed("D", "DDL 回执").decode("utf-8", "replace")))
        elif mark == b"T":
            out.append(Txn(r.byte_prefixed("T", "事务回执").decode("utf-8", "replace")))
        else:
            raise ProtocolError(f"未知语句标记 {mark!r}（第 {r.pos} 字节）")
    return out


def encode_auth_ok(user: str, user_id: int, expired: bool = False) -> bytes:
    """`AUTH` 的 OK 应答（**仅供测试/工具用**——服务端在 Rust 侧）。

    与 Rust 的 `AuthOk::encode` 同一份字面量：字节级示例冻结在两侧测试里。
    """
    return f"user={user}\nuser_id={user_id}\nstatus={'expired' if expired else 'active'}\n".encode(
        "utf-8"
    )


def encode_columns(columns: Iterable[Column]) -> bytes:  # 仅供测试/工具用（服务端在 Rust 侧）
    columns = list(columns)
    out = bytearray(f"C {len(columns)}\n".encode("ascii"))
    for c in columns:
        out += _column_bytes(c)
    return bytes(out)


def decode_columns(payload: bytes) -> list[Column]:
    """`DESCRIBE` 应答（协议 §6）。"""
    r = Reader(payload)
    n = r.head("C", "列数")
    out = [r.column() for _ in range(n)]
    r.finish()
    return out


def _column_bytes(c: Column) -> bytes:
    name = c.name.encode("utf-8")
    type_name = c.type_name.encode("utf-8")
    head = f"C {c.kind} {1 if c.nullable else 0} {c.type_code} {c.length} {len(name)} {len(type_name)}\n"
    return head.encode("ascii") + name + b"\n" + type_name + b"\n"


# ───────────────────────────── 帧 ─────────────────────────────


def write_frame(sock: socket.socket, head: str, payload: bytes = b"") -> None:
    """``<首行>\\n<字节数>\\n<载荷>``。"""
    if len(payload) > MAX_FRAME:
        raise ProtocolError(f"载荷 {len(payload)} 字节超过上限 {MAX_FRAME}")
    sock.sendall(head.encode("ascii") + b"\n" + str(len(payload)).encode("ascii") + b"\n" + payload)


def _read_line(sock: socket.socket) -> bytes:
    out = bytearray()
    while True:
        b = sock.recv(1)
        if not b:
            raise ProtocolError("连接在读到换行前就断了（半帧）")
        if b == b"\n":
            return bytes(out)
        out += b
        if len(out) > MAX_HEAD_LINE:
            raise ProtocolError("首行过长")


def read_frame(sock: socket.socket) -> tuple[str, bytes]:
    """读一帧（**读满**声明的字节数；半帧即错）。"""
    head = _read_line(sock).decode("ascii", "replace").strip()
    try:
        n = int(_read_line(sock).strip() or b"0")
    except ValueError as e:
        raise ProtocolError("长度行不是数") from e
    if n > MAX_FRAME:
        raise ProtocolError(f"载荷 {n} 字节超过上限 {MAX_FRAME}")
    body = bytearray()
    while len(body) < n:
        chunk = sock.recv(n - len(body))
        if not chunk:
            raise ProtocolError(f"半帧：说 {n} 字节，只收到 {len(body)} 字节")
        body += chunk
    return head, bytes(body)


# ───────────────────────────── 一块儿发 ─────────────────────────────


class Link:
    """一条连接上的请求-应答（**一问一答**；协议 §8）。"""

    def __init__(self, sock: socket.socket, wire: int, version: str, instance: str, trace: bool = False) -> None:
        self.sock = sock
        self.wire = wire
        self.version = version
        self.instance = instance
        self.trace = trace

    #: 握手超时（秒）：服务一次只服务一条连接——**没有超时就是无声挂住**。
    HANDSHAKE_TIMEOUT = 5.0

    @classmethod
    def connect(cls, path: str, handshake_timeout: float = HANDSHAKE_TIMEOUT) -> "Link":
        """连上并握手（版本不符**拒绝连**）。"""
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.settimeout(handshake_timeout)
        try:
            sock.connect(path)
        except FileNotFoundError as e:
            sock.close()
            raise ConnectionRefusedError(f"套接字不存在：{path}（实例没在跑？）") from e
        except (PermissionError, OSError) as e:
            sock.close()
            raise ConnectionRefusedError(f"连不上 {path}：{e}") from e
        link = cls(sock, 0, "", "")
        try:
            text = link.call("HELLO").decode("utf-8", "replace")
        except socket.timeout as e:
            sock.close()
            raise TimeoutError(
                "实例正忙：服务一次只服务一条连接（V1.0 单写者）——稍后重试"
            ) from e
        except ProtocolError:
            sock.close()
            raise
        got = {}
        for line in text.splitlines():
            if "=" in line:
                k, v = line.split("=", 1)
                got[k] = v
        link.wire = int(got.get("wire", "0") or "0")
        link.version = got.get("version", "")
        link.instance = got.get("instance", "")
        if link.wire != WIRE_VERSION:
            sock.close()
            raise ProtocolError(
                f"协议版本不兼容：服务端 {link.wire}，客户端 {WIRE_VERSION}——升级较旧的一端"
            )
        sock.settimeout(None)  # 握手之后不限时（长查询是正常的）
        return link

    def call(self, verb: str, payload: bytes = b"") -> bytes:
        """一次请求-应答（`ERR` ⇒ :class:`ServerError`）。"""
        if self.trace:
            import sys

            print(f"[bicdb] → {verb} ({len(payload)} 字节)", file=sys.stderr)
        write_frame(self.sock, verb, payload)
        head, body = read_frame(self.sock)
        if self.trace:
            import sys

            print(f"[bicdb] ← {head} ({len(body)} 字节)", file=sys.stderr)
        if head == OK:
            return body
        if head == ERR:
            raise ServerError(body.decode("utf-8", "replace"))
        raise ProtocolError(f"未知应答首行 `{head}`")

    def auth(self, user: str, password: str) -> AuthOk:
        """**认证**（连上之后、任何业务请求之前）。

        服务端失败语义（协议 §4.3）："主体不存在"与"口令不对"**同一条**错误
        ——不要从错误文本反推"是不是名字写错了"。
        """
        got = decode_auth_ok(self.call("AUTH", encode_auth(user, password)))
        return AuthOk(
            user=str(got["user"]), user_id=int(got["user_id"]), expired=bool(got["expired"])
        )

    def sql(self, sql: str, params: Iterable[tuple[str, Value]] = ()) -> list[object]:
        return decode_statements(self.call("SQL", encode_sql(sql, params)))

    def describe(self, name: str) -> list[Column]:
        return decode_columns(self.call("DESCRIBE", name.encode("utf-8")))

    def status(self) -> dict[str, str]:
        out = {}
        for line in self.call("STATUS").decode("utf-8", "replace").splitlines():
            if "=" in line:
                k, v = line.split("=", 1)
                out[k] = v.strip()
        return out

    def shutdown(self, mode: str) -> str:
        return self.call("SHUTDOWN", mode.encode("ascii")).decode("utf-8", "replace")

    def close(self) -> None:
        try:
            self.sock.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        self.sock.close()


