"""Only synthetic workspaces in an explicitly configured existing database."""
import os
from pathlib import Path
import re
import unittest
import uuid
import bicdb

def configuration():
    raw = os.environ.get('BICDB_TEST_INI')
    if not raw: raise unittest.SkipTest('BICDB_TEST_INI must name the existing test instance; tests never create/start databases')
    ini = Path(raw).resolve(strict=True)
    if not ini.is_file(): raise RuntimeError('standard test database parameter file required')
    raise unittest.SkipTest('synthetic-workspace fixture awaits the core-owned binding API; no instance startup or DDL fallback')

def create_workspace(ini):
    name = 'py_test_' + uuid.uuid4().hex[:16]
    c = bicdb.connect(ini, autocommit=True)
    try:
        cur = c.cursor();cur.execute(f'CREATE WORKSPACE {name}')
        match = re.search(r'工作区号 (\d+)，根 (.+?)，默认盘',cur.message)
        if not match: raise RuntimeError('workspace receipt invalid')
        return name, int(match.group(1)), Path(match.group(2))
    finally:c.close()
