"""Tests for state persistence."""

import tempfile
import time
import unittest
from pathlib import Path

from soundhop.discovery import Peer
from soundhop.state import State


class StateTest(unittest.TestCase):
    def test_state_persistence(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            path = Path(tmpdir) / "state.json"
            state = State()
            state.connected_sink = "soundhop_laptop"
            state.connected_peer = "dev-1234"
            state.connected_name = "Laptop"
            state.previous_default_sink = "alsa_output.pci"
            state.auto_paused = True

            peer = Peer(
                id="dev-5678",
                name="Desktop",
                host="192.168.1.50",
                port=4713,
            )
            state.remember_peer(peer, when=time.time())
            state.save(path)

            loaded = State.load(path)
            self.assertEqual(loaded.connected_sink, "soundhop_laptop")
            self.assertEqual(loaded.connected_peer, "dev-1234")
            self.assertEqual(loaded.connected_name, "Laptop")
            self.assertEqual(loaded.previous_default_sink, "alsa_output.pci")
            self.assertTrue(loaded.auto_paused)
            self.assertIn("dev-5678", loaded.known_peers)
            self.assertEqual(loaded.known_peers["dev-5678"].host, "192.168.1.50")

            loaded.forget_peer("dev-5678")
            self.assertNotIn("dev-5678", loaded.known_peers)


if __name__ == "__main__":
    unittest.main()
