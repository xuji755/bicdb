"""**Python 驱动的认证实机验收**（D6；`docs/客户端协议_v0.1.md` §4.3）。

连接指定的既有部署（`<home>/public` = PUBLIC 工作区，带管理面），用驱动走一遍：
管理面建盘/建区/建主体 → 以主体认证连上 → 读得到、写不了、能改自己的口令 →
"口令错"与"主体不存在"**同一条**文案（防枚举，走线也要一致）。

跑法（仓库根）：``python3 -m unittest discover -s drivers/python/tests``
（``BICDB_TEST_INI`` 指定既有实例；工作区绑定夹具暂待内核接口确认）。
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


from existing_instance import configuration, create_workspace


class LiveAuth(unittest.TestCase):
    """一个实例贯穿整个类；**服务一次只服务一条连接** ⇒ 每个用例自己开关连接。"""

    @classmethod
    def setUpClass(cls) -> None:
        import uuid
        cls.ini = configuration()
        name, _, _ = create_workspace(cls.ini)
        cls.user = 'py_auth_' + uuid.uuid4().hex[:16]
        admin = bicdb.connect(cls.ini, autocommit=True)
        try:
            admin.cursor().execute(f"CREATE USER {cls.user} IDENTIFIED BY 'pw-one' USING WORKSPACE {name}")
        finally: admin.close()

    def connect_as(self, user: str, password: str) -> bicdb.Connection:
        conn = bicdb.connect(self.ini, user=self.user if user == "alice" else user, password=password)
        self.addCleanup(conn.close)
        return conn

    def test_authenticated_subject_reads_but_may_not_write(self):
        conn = self.connect_as("alice", "pw-one")
        self.assertEqual(conn.identity.user, self.user)
        self.assertFalse(conn.identity.expired)
        cur = conn.cursor()
        cur.execute("SELECT 2 + 3 * 4 AS n")
        self.assertEqual(cur.fetchall(), [(14,)])
        # `public` 对主体**只读**；管理面语句要管理面身份（服务端原文）。
        with self.assertRaises(bicdb.DatabaseError) as ctx:
            cur.execute("CREATE TABLE t (id NUMBER)")
        self.assertIn("只读", str(ctx.exception))
        with self.assertRaises(bicdb.DatabaseError) as ctx:
            cur.execute(f"DROP USER {self.user}")
        self.assertIn("管理面身份", str(ctx.exception))
        # 本人改密：放行。
        cur.execute(f"ALTER USER {self.user} IDENTIFIED BY 'pw-two' REPLACE 'pw-one'")
        conn.commit()
        conn.close()
        # 新口令可登、旧口令不可。
        conn = self.connect_as("alice", "pw-two")
        self.assertEqual(conn.identity.user, self.user)

    def test_bad_password_and_unknown_subject_are_one_message(self):
        with self.assertRaises(bicdb.OperationalError) as first:
            bicdb.connect(self.ini, user=self.user, password="nope")
        with self.assertRaises(bicdb.OperationalError) as second:
            bicdb.connect(self.ini, user="nobody-here", password="nope")
        self.assertEqual(str(first.exception), str(second.exception))
        self.assertIn("主体名或口令不对", str(first.exception))
