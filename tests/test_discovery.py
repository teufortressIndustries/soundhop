"""Tests for discovery beacons, parsing, and peer store."""

import json
import time
import unittest

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
        p3 = Peer(
            id="p3", name="Device 3", host="192.168.1.12", port=4713, last_seen=time.time() - 100
        )
        store.update(p3)
        pruned = store.prune()
        self.assertEqual(len(pruned), 1)
        self.assertEqual(pruned[0], "p3")
        self.assertEqual(removed_ids, ["p1", "p3"])
        self.assertEqual(len(store.snapshot()), 1)

    def test_peer_store_rejects_public_ip(self) -> None:
        store = PeerStore()
        public_peer = Peer(id="bad-actor", name="Hacker", host="8.8.8.8", port=4713)
        self.assertFalse(store.update(public_peer))
        self.assertIsNone(store.get("bad-actor"))
        self.assertEqual(len(store.snapshot()), 0)

    def test_udp_service_probe_reply_port_sanitization(self) -> None:
        from soundhop.discovery import UdpBeaconService

        service = UdpBeaconService(
            device_id="my-dev",
            name="My Device",
            audio_port=4713,
            discovery_port=47777,
        )
        sent_destinations = []
        service._send_beacon = lambda dest: sent_destinations.append(dest)

        # 1. Probe from public WAN host should be dropped immediately
        public_probe = json.dumps(
            {"magic": "soundhop", "type": "probe", "reply_port": 50000}
        ).encode()
        service._handle(public_probe, "8.8.8.8")
        self.assertEqual(len(sent_destinations), 0)

        # 2. Probe with invalid reply_port from LAN host should default to discovery_port
        bad_port_probe = json.dumps(
            {"magic": "soundhop", "type": "probe", "reply_port": "not-a-port"}
        ).encode()
        service._handle(bad_port_probe, "192.168.1.50")
        self.assertEqual(len(sent_destinations), 1)
        self.assertEqual(sent_destinations[-1], ("192.168.1.50", 47777))

        # 3. Probe with out-of-range reply_port should default to discovery_port
        oor_probe = json.dumps(
            {"magic": "soundhop", "type": "probe", "reply_port": 999999}
        ).encode()
        service._handle(oor_probe, "192.168.1.50")
        self.assertEqual(len(sent_destinations), 2)
        self.assertEqual(sent_destinations[-1], ("192.168.1.50", 47777))

        # 4. Probe with valid reply_port should use specified port
        valid_probe = json.dumps(
            {"magic": "soundhop", "type": "probe", "reply_port": 54321}
        ).encode()
        service._handle(valid_probe, "192.168.1.50")
        self.assertEqual(len(sent_destinations), 3)
        self.assertEqual(sent_destinations[-1], ("192.168.1.50", 54321))

        # 5. Probe with boolean reply_port (True) should default to discovery_port, NOT 1
        bool_probe = json.dumps({"magic": "soundhop", "type": "probe", "reply_port": True}).encode()
        service._handle(bool_probe, "192.168.1.50")
        self.assertEqual(len(sent_destinations), 4)
        self.assertEqual(sent_destinations[-1], ("192.168.1.50", 47777))

    def test_beacon_bool_and_oversized_validation(self) -> None:
        # Boolean port must be rejected
        raw_bool_port = json.dumps(
            {"magic": "soundhop", "type": "offer", "id": "dev1", "port": True}
        ).encode()
        _, peer = parse_beacon(raw_bool_port, host="192.168.1.50")
        self.assertIsNone(peer)

        # Oversized ID must be rejected
        raw_huge_id = json.dumps(
            {"magic": "soundhop", "type": "offer", "id": "x" * 200, "port": 4713}
        ).encode()
        _, peer = parse_beacon(raw_huge_id, host="192.168.1.50")
        self.assertIsNone(peer)


if __name__ == "__main__":
    unittest.main()
