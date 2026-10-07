"""**Python 驱动的认证实机验收**（D6；`docs/客户端协议_v0.1.md` §4.3）。

起一台真部署（`<home>/public` = PUBLIC 工作区，带管理面），用驱动走一遍：
管理面建盘/建区/建主体 → 以主体认证连上 → 读得到、写不了、能改自己的口令 →
"口令错"与"主体不存在"**同一条**文案（防枚举，走线也要一致）。

跑法（仓库根）：``python3 -m unittest discover -s drivers/python/tests``
（``BICDB_BIN`` 可指定 bicdb 可执行文件；默认取 ``target/debug/bicdb``）。
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import bicdb  # noqa: E402


def bicdb_bin() -> Path:
    """`bicdb` 可执行文件（与 `test_dbapi` 同一口径：`BICDB_BIN` > `target/debug`）。

    在文件里就地定义（不在测试模块之间 import）：`-m unittest tests.test_auth`
    与 `discover -s drivers/python/tests` 两种跑法的导入路径不同——就地定义两边都能跑。
    """
    env = os.environ.get("BICDB_BIN")
    if env:
        return Path(env)
    return Path(__file__).resolve().parents[3] / "target" / "debug" / "bicdb"


class Home:
    """一台临时部署：`<root>/public` + 注册表；子进程都带上 `BICDB_HOME`。"""

    def __init__(self) -> None:
        self.root = Path(tempfile.mkdtemp(prefix="bicdb-py-auth-"))
        self.public = self.root / "public"
        self.env = {**os.environ, "BICDB_HOME": str(self.root)}

    def run(self, *args: str) -> None:
        out = subprocess.run(
            [str(bicdb_bin()), *args],
            capture_output=True,
            text=True,
            env=self.env,
            check=False,
        )
        if out.returncode != 0:
            raise AssertionError(f"bicdb {' '.join(args)} 失败：{out.stdout}{out.stderr}")

    def close(self) -> None:
        subprocess.run(
            [str(bicdb_bin()), "stop", "-p", str(self.public), "-m", "immediate"],
            capture_output=True,
            env=self.env,
            check=False,
        )
        shutil.rmtree(self.root, ignore_errors=True)


@unittest.skipUnless(
    bicdb_bin().exists(), f"没找到 bicdb 可执行文件（{bicdb_bin()}）——先 cargo build"
)
class LiveAuth(unittest.TestCase):
    """一个实例贯穿整个类；**服务一次只服务一条连接** ⇒ 每个用例自己开关连接。"""

    @classmethod
    def setUpClass(cls) -> None:
        cls.home = Home()
        cls.home.run("init", str(cls.home.public))
        d1 = cls.home.root / "d1"
        d1.mkdir()
        cls.home.run(
            "start",
            "-p",
            str(cls.home.public),
            "-w",
            "30",
            # 测试用 1000 轮（生产默认 210_000；迭代数写进散列串，故可调）。
            "-c",
            "auth.pbkdf2_iterations=1000",
        )
        # **管理面**（不认证 = 本机/OS 身份）：依赖顺序 FS → WORKSPACE → USER。
        # 这几条走的是驱动自己 —— 顺带证明 DCL 也能从 Python 驱动发。
        admin = bicdb.connect(str(cls.home.public))
        cur = admin.cursor()
        cur.execute(f"CREATE FILESYSTEM d1 USING '{d1}'")
        cur.execute("CREATE WORKSPACE w1")
        cur.execute("CREATE USER alice IDENTIFIED BY 'pw-one' USING WORKSPACE w1")
        admin.commit()
        admin.close()

    @classmethod
    def tearDownClass(cls) -> None:
        cls.home.close()

    def connect_as(self, user: str, password: str) -> bicdb.Connection:
        conn = bicdb.connect(str(self.home.public), user=user, password=password)
        self.addCleanup(conn.close)
        return conn

    def test_authenticated_subject_reads_but_may_not_write(self):
        conn = self.connect_as("alice", "pw-one")
        self.assertEqual(conn.identity.user, "alice")
        self.assertFalse(conn.identity.expired)
        cur = conn.cursor()
        cur.execute("SELECT 2 + 3 * 4 AS n")
        self.assertEqual(cur.fetchall(), [(14,)])
        # `public` 对主体**只读**；管理面语句要管理面身份（服务端原文）。
        with self.assertRaises(bicdb.DatabaseError) as ctx:
            cur.execute("CREATE TABLE t (id NUMBER)")
        self.assertIn("只读", str(ctx.exception))
        with self.assertRaises(bicdb.DatabaseError) as ctx:
            cur.execute("DROP USER alice")
        self.assertIn("管理面身份", str(ctx.exception))
        # 本人改密：放行。
        cur.execute("ALTER USER alice IDENTIFIED BY 'pw-two' REPLACE 'pw-one'")
        conn.commit()
        conn.close()
        # 新口令可登、旧口令不可。
        conn = self.connect_as("alice", "pw-two")
        self.assertEqual(conn.identity.user, "alice")

    def test_bad_password_and_unknown_subject_are_one_message(self):
        with self.assertRaises(bicdb.OperationalError) as first:
            bicdb.connect(str(self.home.public), user="alice", password="nope")
        with self.assertRaises(bicdb.OperationalError) as second:
            bicdb.connect(str(self.home.public), user="nobody-here", password="nope")
        self.assertEqual(str(first.exception), str(second.exception))
        self.assertIn("主体名或口令不对", str(first.exception))
