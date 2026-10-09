"""**Python 驱动的实机验收**：起一个真服务（`bicdb init` + `bicdb start`），
用它跑一遍 DB-API 面。

钉的是"驱动**真能**说话"：建表/写入/查询/参数/事务/`describe`/错误类型/
非 UTF-8 字节串无损/读己所写/多会话隔离。**不用 mock**——协议错了就得在这儿现形。

跑法（仓库根）：``python3 -m unittest discover -s drivers/python/tests``
（``BICDB_BIN`` 可指定 bicdb 可执行文件；默认取 ``target/debug/bicdb``）。
"""

from __future__ import annotations

import decimal
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import bicdb  # noqa: E402


def repo_root() -> Path:
    return Path(__file__).resolve().parents[3]


def bicdb_bin() -> Path:
    env = os.environ.get("BICDB_BIN")
    if env:
        return Path(env)
    return repo_root() / "target" / "debug" / "bicdb"


def run(*args: str) -> subprocess.CompletedProcess:
    out = subprocess.run(
        [str(bicdb_bin()), *args], capture_output=True, text=True, check=False
    )
    if out.returncode != 0:
        raise AssertionError(f"bicdb {' '.join(args)} 失败：{out.stdout}{out.stderr}")
    return out


@unittest.skipUnless(bicdb_bin().exists(), f"没找到 bicdb 可执行文件（{bicdb_bin()}）——先 cargo build")
class LiveDriver(unittest.TestCase):
    """一个实例贯穿整个类；每个用例自己开/关连接。"""

    @classmethod
    def setUpClass(cls) -> None:
        cls.root = Path(tempfile.mkdtemp(prefix="bicdb-py-"))
        run("init", str(cls.root))
        run("start", "-p", str(cls.root), "-w", "30")

    @classmethod
    def tearDownClass(cls) -> None:
        subprocess.run(
            [str(bicdb_bin()), "stop", "-p", str(cls.root), "-m", "immediate"],
            capture_output=True,
            check=False,
        )
        shutil.rmtree(cls.root, ignore_errors=True)

    def setUp(self) -> None:
        self.conn = bicdb.connect(str(self.root))
        self.addCleanup(self.conn.close)

    # ── 基本 ──

    def test_connect_and_handshake(self):
        self.assertTrue(self.conn.server_version.startswith("0.2."), self.conn.server_version)
        self.assertEqual(self.conn.wire_version, bicdb.wire.WIRE_VERSION)
        self.assertEqual(Path(self.conn.instance), Path(self.root))
        self.assertTrue(self.conn.ping())

    def test_ddl_dml_select_and_description(self):
        cur = self.conn.cursor()
        cur.execute("CREATE TABLE t (id NUMBER NOT NULL, name VARCHAR2(32))")
        self.assertEqual(cur.rowcount, 0)
        self.assertIn("CREATE TABLE", cur.message)
        cur.execute("INSERT INTO t VALUES (:id, :name)", {"id": 1, "name": "alpha"})
        self.assertEqual(cur.rowcount, 1)
        self.conn.commit()

        cur.execute("SELECT id, name FROM t ORDER BY id")
        self.assertEqual([c[0] for c in cur.description], ["id", "name"])
        self.assertEqual(cur.description[0][1], bicdb.NUMBER)
        self.assertEqual(cur.description[1][1], bicdb.STRING)
        self.assertEqual(cur.rowcount, 1)
        self.assertEqual(cur.fetchall(), [(decimal.Decimal(1), "alpha")])
        self.assertIsNone(cur.fetchone())

    def test_parameter_styles(self):
        cur = self.conn.cursor()
        cur.execute("CREATE TABLE p (a NUMBER NOT NULL, b VARCHAR2(8))")
        # 具名（dict）
        cur.execute("INSERT INTO p VALUES (:a, :b)", {"a": 1, "b": "x"})
        # 按位置（序列 ⇒ 按 SQL 里参数**出现顺序**配名）
        cur.execute("INSERT INTO p VALUES (:a, :b)", (2, "y"))
        self.conn.commit()
        rows = cur.execute("SELECT a, b FROM p ORDER BY a").fetchall()
        self.assertEqual(rows, [(decimal.Decimal(1), "x"), (decimal.Decimal(2), "y")])
        # 个数不符 ⇒ 具名拒绝
        with self.assertRaises(bicdb.ProgrammingError):
            cur.execute("SELECT a FROM p WHERE a = :a AND b = :b", [1])

    def test_value_types_are_lossless(self):
        cur = self.conn.cursor()
        cur.execute("CREATE TABLE v (k NUMBER NOT NULL, s VARCHAR2(16), b BOOLEAN)")
        raw = bytes([0x00, 0xFF, 0x0A, 0x80])
        cur.execute("INSERT INTO v VALUES (:k, :s, :b)", {"k": 1, "s": raw, "b": True})
        cur.execute("INSERT INTO v VALUES (:k, :s, :b)", {"k": 2, "s": None, "b": False})
        self.conn.commit()
        cur.execute("SELECT k, s, b FROM v ORDER BY k")
        r1, r2 = cur.fetchall()
        # 非 UTF-8 的字节**原样**回来（不替换成 �）
        self.assertEqual(r1[1], raw)
        self.assertIs(True, r1[2])
        self.assertIsNone(r2[1])
        self.assertIs(False, r2[2])
        # 文本列按 UTF-8 解出就给 str
        cur.execute("CREATE TABLE w (s VARCHAR2(16))")
        cur.execute("INSERT INTO w VALUES (:s)", {"s": "汉字"})
        self.conn.commit()
        self.assertEqual(cur.execute("SELECT s FROM w").fetchall(), [("汉字",)])

    def test_graph_table_returns_typed_element_handles(self):
        cur = self.conn.cursor()
        cur.execute("CREATE GRAPH py_element")
        cur.execute("CYPHER py_element 'CREATE (:N {name:\"python\"})'")
        row = cur.execute(
            "SELECT entity,name FROM GRAPH_TABLE(py_element,"
            "'MATCH (n) RETURN n,n.name' "
            "COLUMNS(entity GRAPH_ELEMENT,name VARCHAR2(32)))"
        ).fetchone()
        self.assertIsInstance(row[0], bicdb.GraphElement)
        self.assertEqual((row[0].kind, row[0].id, row[1]), ("n", 1, "python"))

    def test_explicit_sql_transactions_via_cursor(self):
        cur = self.conn.cursor()
        cur.execute("CREATE TABLE x (id NUMBER NOT NULL)")
        cur.execute("BEGIN")
        cur.execute("INSERT INTO x VALUES (1)")
        # **读己所写**（协议之外由引擎保证；这里是端到端确认）
        self.assertEqual(cur.execute("SELECT id FROM x").fetchall(), [(decimal.Decimal(1),)])
        cur.execute("ROLLBACK")
        self.assertEqual(cur.execute("SELECT id FROM x").fetchall(), [])

    def test_implicit_transaction_and_commit_rollback(self):
        cur = self.conn.cursor()
        cur.execute("CREATE TABLE y (id NUMBER NOT NULL)")
        self.assertFalse(self.conn.in_transaction)
        cur.execute("INSERT INTO y VALUES (1)")  # 写入前驱动自动 BEGIN
        self.assertTrue(self.conn.in_transaction)
        self.conn.rollback()
        self.assertFalse(self.conn.in_transaction)
        self.assertEqual(cur.execute("SELECT id FROM y").fetchall(), [])

        cur.execute("INSERT INTO y VALUES (2)")
        self.conn.commit()
        self.assertFalse(self.conn.in_transaction)
        self.assertEqual(cur.execute("SELECT id FROM y").fetchall(), [(decimal.Decimal(2),)])

    def test_executemany(self):
        cur = self.conn.cursor()
        cur.execute("CREATE TABLE m (id NUMBER NOT NULL, v VARCHAR2(8))")
        n = cur.executemany(
            "INSERT INTO m VALUES (:id, :v)",
            [{"id": 1, "v": "a"}, {"id": 2, "v": "b"}, {"id": 3, "v": "c"}],
        ).rowcount
        self.assertEqual(n, 3)
        self.conn.commit()
        self.assertEqual(cur.execute("SELECT id FROM m ORDER BY id").fetchall(),
                         [(decimal.Decimal(1),), (decimal.Decimal(2),), (decimal.Decimal(3),)])

    def test_fetchmany_and_iteration(self):
        cur = self.conn.cursor()
        cur.execute("CREATE TABLE f (id NUMBER NOT NULL)")
        cur.executemany("INSERT INTO f VALUES (:id)", [{"id": i} for i in range(5)])
        self.conn.commit()
        cur.execute("SELECT id FROM f ORDER BY id")
        cur.arraysize = 2
        self.assertEqual(len(cur.fetchmany()), 2)
        self.assertEqual(len(cur.fetchmany(1)), 1)
        rest = list(cur)
        self.assertEqual(len(rest), 2)

    def test_errors_are_classified_with_the_original_text(self):
        cur = self.conn.cursor()
        with self.assertRaises(bicdb.ProgrammingError) as ctx:
            cur.execute("SELECT * FROM nope")
        self.assertIn("nope", str(ctx.exception))  # 原文（服务给的具名文本）

        cur.execute("CREATE TABLE u (id NUMBER NOT NULL)")
        cur.execute("CREATE UNIQUE INDEX uu ON u (id)")
        cur.execute("INSERT INTO u VALUES (1)")
        self.conn.commit()
        with self.assertRaises(bicdb.IntegrityError):
            cur.execute("INSERT INTO u VALUES (1)")
        self.conn.rollback()

        with self.assertRaises(bicdb.NotSupportedError):
            cur.execute("UPDATE u SET id = 2")  # 本版明确不支持

    def test_describe(self):
        cur = self.conn.cursor()
        cur.execute("CREATE TABLE d (id NUMBER NOT NULL, name VARCHAR2(32))")
        cols = self.conn.describe("d")
        self.assertEqual([c.name for c in cols], ["id", "name"])
        self.assertEqual(cols[0].kind, "n")
        self.assertFalse(cols[0].nullable)
        self.assertEqual(cols[1].type_name, "VARCHAR2(32)")
        self.assertTrue(cols[1].nullable)

    def test_column_constraints_and_parameterized_dml(self):
        cur = self.conn.cursor()
        cur.execute("CREATE TABLE constraints_t (id NUMBER NOT NULL, v VARCHAR2(3))")
        with self.assertRaises(bicdb.IntegrityError):
            cur.execute("INSERT INTO constraints_t VALUES (:id, :v)", {"id": None, "v": "ok"})
        self.conn.rollback()
        with self.assertRaises(bicdb.DataError):
            cur.execute("INSERT INTO constraints_t VALUES (1, :v)", {"v": "汉字"})
        self.conn.rollback()
        cur.execute("INSERT INTO constraints_t VALUES (1, 'old')")
        self.conn.commit()
        cur.execute("UPDATE constraints_t SET v = :v WHERE id = :id", {"v": "new", "id": 1})
        self.assertEqual(cur.rowcount, 1)
        self.conn.commit()
        cur.execute("SELECT v FROM constraints_t WHERE id = 1")
        self.assertEqual(cur.fetchone(), ("new",))
        cur.execute("DELETE FROM constraints_t WHERE id = :id", {"id": 1})
        self.assertEqual(cur.rowcount, 1)
        self.conn.commit()

    def test_autocommit_mode(self):
        conn = bicdb.connect(str(self.root), autocommit=True)
        cur = conn.cursor()
        cur.execute("CREATE TABLE a (id NUMBER NOT NULL)")
        cur.execute("INSERT INTO a VALUES (1)")
        # 引擎的原生形态：每条语句自结（驱动一次 BEGIN 都没发）。
        self.assertFalse(conn.in_transaction)
        conn.close()
        # **断开再连**：行已经在（自结过了）——与隐式事务模式对照。
        conn2 = bicdb.connect(str(self.root))
        self.addCleanup(conn2.close)
        self.assertEqual(
            conn2.cursor().execute("SELECT id FROM a").fetchall(),
            [(decimal.Decimal(1),)],
        )

    def test_two_connections_isolate_and_refresh_transactions(self):
        cur = self.conn.cursor()
        cur.execute("CREATE TABLE multi_t (id NUMBER NOT NULL)")
        self.conn.commit()
        other = bicdb.connect(str(self.root))
        self.addCleanup(other.close)
        cur.execute("INSERT INTO multi_t VALUES (1)")
        other_cur = other.cursor()
        other_cur.execute("SELECT count(*) FROM multi_t")
        self.assertEqual(other_cur.fetchone(), (decimal.Decimal(0),))
        self.conn.commit()
        other_cur.execute("SELECT count(*) FROM multi_t")
        self.assertEqual(other_cur.fetchone(), (decimal.Decimal(1),))

    def test_connect_without_a_running_instance_is_a_named_error(self):
        empty = Path(tempfile.mkdtemp(prefix="bicdb-py-empty-"))
        self.addCleanup(shutil.rmtree, empty, True)
        run("init", str(empty))  # 建区但不起服务
        with self.assertRaises(bicdb.OperationalError):
            bicdb.connect(str(empty))
        with self.assertRaises(bicdb.InterfaceError):  # 寻址失败（不是连接失败）
            bicdb.connect(str(empty / "nope"))

    def test_context_manager_commits_or_rolls_back(self):
        cur = self.conn.cursor()
        cur.execute("CREATE TABLE c (id NUMBER NOT NULL)")
        self.conn.commit()
        with self.conn:
            cur.execute("INSERT INTO c VALUES (1)")
        self.assertFalse(self.conn.in_transaction)
        self.assertEqual(cur.execute("SELECT id FROM c").fetchall(), [(decimal.Decimal(1),)])
        with self.assertRaises(RuntimeError):
            with self.conn:
                cur.execute("INSERT INTO c VALUES (2)")
                raise RuntimeError("炸了")
        self.assertEqual(len(cur.execute("SELECT id FROM c").fetchall()), 1, "异常应回滚")


if __name__ == "__main__":
    unittest.main()
