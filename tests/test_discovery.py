"""Tests for discovery beacons, parsing, and peer store."""

import json
import time
import unittest

from soundhop.constants import PROTOCOL_VERSION
from soundhop.discovery import (
    Peer,
    PeerStore,
    make_beacon,
    parse_beacon,
)


class DiscoveryTest(unittest.TestCase):
    def test_beacon_roundtrip(self) -> None:
        raw = make_beacon(
            device_id="dev-test1",
            name="Fedora Workstation",
            port=4713,
            security="trusted-lan",
            message_type="offer",
        )
        payload, peer = parse_beacon(raw, host="192.168.1.100")
        self.assertIsNotNone(payload)
        self.assertIsNotNone(peer)
        assert peer is not None
        self.assertEqual(peer.id, "dev-test1")
        self.assertEqual(peer.name, "Fedora Workstation")
        self.assertEqual(peer.host, "192.168.1.100")
        self.assertEqual(peer.port, 4713)
        self.assertEqual(peer.security, "trusted-lan")
        self.assertTrue(peer.compatible)

    def test_bye_beacon(self) -> None:
        raw = make_beacon(
            device_id="dev-test1",
            name="Fedora Workstation",
            port=4713,
            message_type="bye",
        )
        payload, peer = parse_beacon(raw, host="192.168.1.100")
        self.assertIsNotNone(payload)
        self.assertIsNone(peer)
        assert payload is not None
        self.assertEqual(payload["type"], "bye")
        self.assertEqual(payload["id"], "dev-test1")

    def test_invalid_beacon(self) -> None:
        self.assertEqual(parse_beacon(b"invalid json", host="1.1.1.1"), (None, None))
        wrong_magic = json.dumps({"magic": "other", "v": 1}).encode("utf-8")
        self.assertEqual(parse_beacon(wrong_magic, host="1.1.1.1"), (None, None))

    def test_peer_store(self) -> None:
        store = PeerStore()
        removed_ids = []
        store.on_remove = lambda pid: removed_ids.append(pid)

        p1 = Peer(id="p1", name="Device 1", host="192.168.1.10", port=4713)
        p2 = Peer(id="p2", name="Device 2", host="192.168.1.11", port=4713)

        store.update(p1)
        store.update(p2)
        self.assertEqual(len(store.snapshot()), 2)
        self.assertEqual(store.get("p1"), p1)

        store.remove("p1")
        self.assertEqual(len(store.snapshot()), 1)
        self.assertEqual(removed_ids, ["p1"])

        # Prune test with fake last_seen
        p3 = Peer(id="p3", name="Device 3", host="192.168.1.12", port=4713, last_seen=time.time() - 100)
        store.update(p3)
        pruned = store.prune()
        self.assertEqual(len(pruned), 1)
        self.assertEqual(pruned[0], "p3")
        self.assertEqual(len(store.snapshot()), 1)


if __name__ == "__main__":
    unittest.main()
