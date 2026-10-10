"""Read-only BIND regression against an explicitly selected running database.

Set BICDB_TEST_INI, BICDB_TEST_USER and BICDB_TEST_PASSWORD. No instance or
workspace is created, no data is written, and credentials are never printed.
"""
import os
import unittest

import bicdb
from bicdb import wire


class ExistingBindTests(unittest.TestCase):
    def setUp(self):
        keys = ('BICDB_TEST_INI', 'BICDB_TEST_USER', 'BICDB_TEST_PASSWORD')
        if not all(os.environ.get(key) for key in keys):
            self.skipTest('explicit running database and test credentials required')
        self.connection = bicdb.connect(os.environ[keys[0]], user=os.environ[keys[1]],
                                        password=os.environ[keys[2]], autocommit=True)
        self.addCleanup(self.connection.close)

    def test_malformed_bind_preserves_identity_and_allows_retry(self):
        expected = self.connection.route_owned()
        with self.assertRaises(wire.ServerError):
            self.connection._link.call('BIND', b'\xff')
        self.assertEqual(self.connection.route_owned(), expected)
        self.assertEqual(self.connection.bind_workspace(), expected)

    def test_missing_workspace_then_retry_and_duplicate_bind_preserves_sql(self):
        expected = self.connection.route_owned()
        with self.assertRaises(bicdb.DatabaseError):
            self.connection.bind_workspace('nonexistent_bind_regression_workspace')
        self.assertEqual(self.connection.bind_workspace(), expected)
        with self.assertRaises(bicdb.DatabaseError):
            self.connection.bind_workspace()
        cursor = self.connection.cursor()
        cursor.execute('SELECT COUNT(*) FROM tab$')
        self.assertGreater(int(cursor.fetchone()[0]), 0)
        self.assertEqual(self.connection.status()['workspace_kind'], 'private')


if __name__ == '__main__':
    unittest.main()
