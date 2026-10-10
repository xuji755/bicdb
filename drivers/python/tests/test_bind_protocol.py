"""BIND uses the existing instance connection and validates its receipt."""
import sys
from pathlib import Path
import unittest
from unittest.mock import Mock
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from bicdb import wire

class BindProtocol(unittest.TestCase):
    def connection(self, payload):
        c = object.__new__(wire.Link)
        c.call = Mock(return_value=payload)
        return c

    def test_bind_receipt_and_workspace_selection(self):
        c = self.connection(b'user_id=2\nworkspace_id=3\nname_hex=64627573657231\nroot_hex=2f7773\n')
        self.assertEqual(c.bind_workspace('3'), {'user_id':2,'workspace_id':3,'name':'dbuser1','root':'/ws'})
        c.call.assert_called_once_with('BIND', b'3')

    def test_bind_rejects_malformed_receipt(self):
        c = self.connection(b'user_id=2\nworkspace_id=0\nname_hex=61\nroot_hex=2f7773\n')
        with self.assertRaises(wire.ProtocolError): c.bind_workspace('3')

    def test_bind_server_denial_is_not_a_route_fallback(self):
        c = self.connection(b'')
        c.call.side_effect = wire.ServerError('not owned')
        with self.assertRaises(wire.ServerError): c.bind_workspace('4')
        self.assertEqual(c.call.call_count, 1)
