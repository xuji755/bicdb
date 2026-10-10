"""Opt-in full-text integration in synthetic workspaces of a running instance.

BICDB_TEST_INI and BICDB_TEST_ALLOW_PROVISION=1 are required. Creates two
synthetic workspaces, pauses their users on completion, never starts a daemon.
"""
import os
import secrets
import time
import unittest
import uuid

import bicdb


@unittest.skipUnless(os.environ.get('BICDB_TEST_INI') and
                     os.environ.get('BICDB_TEST_ALLOW_PROVISION') == '1',
                     'explicit existing instance and provisioning opt-in required')
class ExistingFulltextTests(unittest.TestCase):
    def test_private_batch_sees_committed_nodes_while_other_workspace_has_transaction(self):
        ini = os.environ['BICDB_TEST_INI']
        admin = bicdb.connect(ini, autocommit=True)
        self.addCleanup(admin.close)
        accounts, clients = [], []
        def cleanup():
            for client in clients:
                try:
                    client.cursor().execute('ROLLBACK')
                except bicdb.DatabaseError:
                    pass
                finally:
                    client.close()
            for account in accounts:
                admin.cursor().execute(f'ALTER USER {account} PAUSE')
        self.addCleanup(cleanup)
        for _ in range(2):
            name = 'audit_' + uuid.uuid4().hex[:14]
            password = secrets.token_urlsafe(24)
            q = admin.cursor()
            q.execute(f'CREATE WORKSPACE {name}')
            q.execute(f"CREATE USER {name} IDENTIFIED BY '{password}' USING WORKSPACE {name}")
            accounts.append(name)
            client = bicdb.connect(ini, user=name, password=password, autocommit=True)
            client.bind_workspace()
            clients.append(client)
        a, b = clients
        a.cursor().execute('BEGIN')
        q = b.cursor()
        q.execute('CREATE GRAPH audit_kg')
        q.execute('CREATE FULLTEXT GRAPH INDEX words ON audit_kg NODES (name) '
                  'OPTIONS \'{"update":"batch","interval_ms":100,"batch_rows":2}\'')
        q.execute('CYPHER audit_kg \'CREATE (:N {name:"auditneedle",db_type:"audit"})\'')
        started = time.monotonic()
        row = {}
        while time.monotonic() - started < 10:
            b.status()
            q.execute('SHOW FULLTEXT GRAPH INDEXES ON audit_kg')
            row = dict(zip((v[0] for v in q.description), q.fetchone()))
            if int(row['documents']) == 1 and int(row['shared_pending']) == 0:
                break
            time.sleep(.02)
        self.assertEqual(int(row['documents']), 1, row)
        self.assertEqual(row['covered_source_seq'], row['source_seq'])
        self.assertEqual(int(row['shared_pending']), 0)
        q.execute('CYPHER audit_kg \'MATCH (n) RETURN n.name AS name\'')
        self.assertEqual(q.fetchall(), [('auditneedle',)])
        q.execute("SEARCH FULLTEXT GRAPH INDEX words ON audit_kg FOR 'auditneedle' "
                  "OPTIONS '{\"db_type\":\"audit\",\"consistency\":\"eventual\"}'")
        self.assertEqual(len(q.fetchall()), 1)
        print('private fulltext caught up in', round(time.monotonic() - started, 3), 'seconds')


if __name__ == '__main__':
    unittest.main()
