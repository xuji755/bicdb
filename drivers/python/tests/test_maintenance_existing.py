"""Read-only maintenance/handle checks against BICDB_TEST_INI, never starts a daemon."""
from contextlib import closing
import os
import time
import unittest

import bicdb


@unittest.skipUnless(os.environ.get('BICDB_TEST_INI'), 'explicit existing database required')
class ExistingMaintenanceTests(unittest.TestCase):
    def test_idle_polls_do_not_accumulate_catalog_handles(self):
        with closing(bicdb.connect(os.environ['BICDB_TEST_INI'], autocommit=True)) as connection:
            samples = []
            for _ in range(16):
                started = time.monotonic()
                status = connection.status()
                samples.append((int(status['file_handles']), int(status['bound_workspaces']),
                                int(status['worker_threads']), time.monotonic() - started))
                self.assertIn('fulltext_active', status)
                self.assertIn('fulltext_failures', status)
                time.sleep(.2)
            self.assertEqual(samples[0][1], samples[-1][1],
                             'workspace population changed; rerun in a stable interval')
            # Allow transient catalogs in SQL/maintenance workers; compare minima
            # to avoid mistaking an in-flight worker for a retained handle.
            first = min(v[0] for v in samples[:4])
            last = min(v[0] for v in samples[-4:])
            allowance = max(v[2] for v in samples) + 2
            self.assertLessEqual(last, first + allowance, samples)
            print('handles first/last:', first, last,
                  'max STATUS seconds:', round(max(v[3] for v in samples), 4))


if __name__ == '__main__':
    unittest.main()
