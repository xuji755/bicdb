"""**协议 v0.1 的字节级冻结**（与 `crates/net/tests/wire_v01_frozen.rs` **同一串字面量**）。

两个实现（Rust / Python）各自实现同一份规格——规格示例一旦漂移，两边会
"各自都能跑、互相说不通"。这里把示例钉死；Rust 侧有同款断言。
"""

from __future__ import annotations

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from bicdb import wire  # noqa: E402

#: 协议 §7.1 的请求示例（逐字节）。
REQUEST_BYTES = b"P 1\nN 2\nid\nn1\n7\nQ 38\nSELECT id, name FROM t WHERE id = :id;\n"

#: 协议 §7.2 的结果示例（逐字节）。
RESPONSE_BYTES = (
    b"R 2 2\n"
    b"C n 1 0 0 2 0\nID\n\n"
    b"C b 1 0 0 4 0\nNAME\n\n"
    b"n1\n7\n"
    b"b10\n616c706861\n"
    b"-0\n\n"
    b"b12\ne6b189e5ad97\n"
)


#: 协议 §7.3 的认证请求示例（逐字节）：主体 `alice`、口令 `s3cr3t`。
AUTH_REQUEST_BYTES = b"U 5\nalice\nS 6\ns3cr3t\n"

#: 协议 §7.4 的认证应答示例（逐字节）。
AUTH_OK_BYTES = b"user=alice\nuser_id=7\nstatus=active\n"


class FrozenAuthExample(unittest.TestCase):
    def test_auth_request_matches_the_frozen_example(self):
        got = wire.encode_auth("alice", "s3cr3t")
        self.assertEqual(len(got), 21, got)
        self.assertEqual(got, AUTH_REQUEST_BYTES)

    def test_auth_reply_matches_the_frozen_example(self):
        got = wire.encode_auth_ok("alice", 7, False)
        self.assertEqual(len(got), 35, got)
        self.assertEqual(got, AUTH_OK_BYTES)

    def test_auth_reply_decodes_back(self):
        ok = wire.decode_auth_ok(AUTH_OK_BYTES)
        self.assertEqual(ok["user"], "alice")
        self.assertEqual(ok["user_id"], 7)
        self.assertFalse(ok["expired"])
        expired = wire.decode_auth_ok(b"user=alice\nuser_id=7\nstatus=expired\n")
        self.assertTrue(expired["expired"])

    def test_a_truncated_or_misaligned_auth_payload_is_an_error(self):
        # 长度是权威：截断必须报错，不许"尽力解出半条"。
        # （Python 侧是客户端、不发 AUTH——按 §4.3 的字节规则读一遍即可。）
        for cut in range(1, len(AUTH_REQUEST_BYTES)):
            with self.assertRaises(wire.ProtocolError):
                read_auth(AUTH_REQUEST_BYTES[: -cut])
        # 段序错：认得出的标记就该认，认不出就报错。
        with self.assertRaises(wire.ProtocolError):
            read_auth(b"S 3\nabc\nU 5\nalice\n")
        # 多一段：读到头才算解对。
        with self.assertRaises(wire.ProtocolError):
            read_auth(AUTH_REQUEST_BYTES + b"X 1\na\n")


def read_auth(payload: bytes) -> tuple[str, str]:
    """按协议 §4.3 读一份 `AUTH` 请求载荷（真件在服务端）。"""
    r = wire.Reader(payload)
    user = r.byte_prefixed("U", "主体名")
    password = r.byte_prefixed("S", "口令")
    r.finish()
    return user.decode("utf-8"), password.decode("utf-8")


class FrozenExample(unittest.TestCase):
    def test_request_payload_matches_the_frozen_example(self):
        got = wire.encode_sql("SELECT id, name FROM t WHERE id = :id;", [("id", wire.Value("n", "7"))])
        self.assertEqual(len(got), 60, got)
        self.assertEqual(got, REQUEST_BYTES)

    def test_request_decodes_back(self):
        r = wire.Reader(REQUEST_BYTES)
        self.assertEqual(r.head("P", "参数数"), 1)
        self.assertEqual(r.byte_prefixed("N", "参数名"), b"id")
        cell = r.cell()
        self.assertEqual((cell.tag, cell.text), ("n", "7"))
        self.assertEqual(
            r.byte_prefixed("Q", "SQL"), b"SELECT id, name FROM t WHERE id = :id;"
        )
        r.finish()

    def test_response_payload_matches_the_frozen_example(self):
        # 用 Rust 侧的编码形态在 Python 里重建，再逐字节比。
        out = bytearray(b"R 2 2\n")
        out += wire._column_bytes(wire.Column("ID", "n"))
        out += wire._column_bytes(wire.Column("NAME", "b"))
        out += wire.encode_cell(wire.Value("n", "7"))
        out += wire.encode_cell(wire.Value("b", b"alpha".hex()))
        out += wire.encode_cell(wire.Value.null())
        out += wire.encode_cell(wire.Value("b", "汉字".encode("utf-8").hex()))
        self.assertEqual(len(out), 85, out)
        self.assertEqual(bytes(out), RESPONSE_BYTES)

    def test_response_decodes_back(self):
        stmts = wire.decode_statements(RESPONSE_BYTES)
        self.assertEqual(len(stmts), 1)
        rows = stmts[0]
        self.assertIsInstance(rows, wire.Rows)
        self.assertEqual([c.name for c in rows.columns], ["ID", "NAME"])
        self.assertEqual([c.kind for c in rows.columns], ["n", "b"])
        self.assertEqual(rows.rows[0][0].as_int(), 7)
        self.assertEqual(rows.rows[0][1].as_bytes(), b"alpha")
        self.assertTrue(rows.rows[1][0].is_null())
        self.assertEqual(rows.rows[1][1].as_bytes().decode("utf-8"), "汉字")

    def test_columns_payload_round_trips(self):
        cols = [
            wire.Column("id", "n", nullable=False, type_code=1, length=22, type_name="NUMBER"),
            wire.Column("名 字", "b", nullable=True, type_code=3, length=32, type_name="VARCHAR2(32)"),
        ]
        payload = wire.encode_columns(cols)
        back = wire.decode_columns(payload)
        self.assertEqual([(c.name, c.kind, c.nullable, c.type_code, c.length, c.type_name) for c in back],
                         [(c.name, c.kind, c.nullable, c.type_code, c.length, c.type_name) for c in cols])
        # 多余字节 ⇒ 错位，**报错**（不是"尽力解"）。
        with self.assertRaises(wire.ProtocolError):
            wire.decode_columns(payload + b"x")


class Framing(unittest.TestCase):
    def test_a_half_frame_is_an_error(self):
        """半帧不算帧：说 100 字节只给 4 —— **报错**，不按行补读。"""
        import socket as _socket

        a, b = _socket.socketpair()
        try:
            a.sendall(b"OK\n100\nhalf")
            a.close()  # 对端**提前关**：说 100 字节，只给了 4 字节
            with self.assertRaises(wire.ProtocolError):
                wire.read_frame(b)
        finally:
            b.close()

    def test_frame_carries_a_payload_with_newlines(self):
        import socket as _socket

        a, b = _socket.socketpair()
        try:
            payload = b"line1\nline2\n"
            wire.write_frame(a, "SQL", payload)
            head, body = wire.read_frame(b)
            self.assertEqual((head, body), ("SQL", payload))
        finally:
            a.close()
            b.close()

    def test_values_round_trip_with_odd_payloads(self):
        cases = [
            wire.Value.null(),
            wire.Value("n", "123.450"),
            wire.Value("o", "1"),
            wire.Value("b", ""),
            wire.Value("b", b"\x00\xff\n".hex()),
        ]
        for v in cases:
            got = wire.Reader(wire.encode_cell(v)).cell()
            self.assertEqual(got, v)


class PythonValues(unittest.TestCase):
    def test_python_to_value_does_not_guess(self):
        self.assertEqual(wire.python_to_value(None), wire.Value.null())
        self.assertEqual(wire.python_to_value(True), wire.Value("o", "1"))
        self.assertEqual(wire.python_to_value(7), wire.Value("n", "7"))
        self.assertEqual(wire.python_to_value("ab"), wire.Value("b", b"ab".hex()))
        self.assertEqual(wire.python_to_value(b"\x00\xff"), wire.Value("b", "00ff"))
        with self.assertRaises(TypeError):
            wire.python_to_value(object())


if __name__ == "__main__":
    unittest.main()
