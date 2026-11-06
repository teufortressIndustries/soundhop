"""Tests for daemon supervision and connection resolution."""

import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from soundhop.constants import OFFLINE_PROBE_LIMIT
from soundhop.controller import ControllerError
from soundhop.daemon import Daemon


class DaemonTest(unittest.TestCase):
    def test_offline_counter_increments_and_resets(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            cfg_path = Path(tmpdir) / "config.toml"
            state_path = Path(tmpdir) / "state.json"
            ipc_path = Path(tmpdir) / "test.sock"

            daemon = Daemon(
                config_path=cfg_path,
                state_path=state_path,
                notify_enabled=False,
                ipc_path=ipc_path,
            )
            daemon.controller.state.connected_sink = "soundhop_test"
            daemon.controller.state.connected_peer = "peer-1"
            daemon.controller.state.connected_name = "Remote Device"
            daemon.controller.state.connected_host = "192.168.1.100"
            daemon.controller.state.connected_port = 4713
            self.assertTrue(daemon.controller.state.connected)

            # Mock tcp_reachable to return False (simulating peer going offline)
            with (
                patch("soundhop.daemon.tcp_reachable", return_value=False),
                patch.object(daemon.controller, "reconcile"),
                patch.object(daemon.controller, "disconnect") as mock_disconnect,
            ):
                # Tick 1: count should become 1
                daemon._supervise()
                self.assertEqual(daemon._offline_count, 1)
                mock_disconnect.assert_not_called()

                # Tick 2: count should become 2
                daemon._supervise()
                self.assertEqual(daemon._offline_count, 2)
                mock_disconnect.assert_not_called()

                # If peer comes back online on tick 3, count should reset to 0
                with patch("soundhop.daemon.tcp_reachable", return_value=True):
                    daemon._supervise()
                    self.assertEqual(daemon._offline_count, 0)
                    mock_disconnect.assert_not_called()

                # Tick 1 again
                daemon._supervise()
                self.assertEqual(daemon._offline_count, 1)

                # Continue offline until OFFLINE_PROBE_LIMIT
                daemon._offline_count = OFFLINE_PROBE_LIMIT - 1
                daemon._supervise()
                self.assertEqual(daemon._offline_count, 0)
                mock_disconnect.assert_called_once()

    def test_resolve_peer_rejects_public_ip(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            cfg_path = Path(tmpdir) / "config.toml"
            state_path = Path(tmpdir) / "state.json"
            ipc_path = Path(tmpdir) / "test.sock"

            daemon = Daemon(
                config_path=cfg_path,
                state_path=state_path,
                notify_enabled=False,
                ipc_path=ipc_path,
            )

            # Manual host with public IP
            with self.assertRaises(ControllerError) as ctx:
                daemon._resolve_peer({"host": "8.8.8.8"})
            self.assertIn("not a private LAN address", str(ctx.exception))

            # Valid private host should succeed
            peer = daemon._resolve_peer({"host": "192.168.1.200"})
            self.assertEqual(peer.host, "192.168.1.200")

            # android_wifi with public IP must be rejected
            res = daemon.handle({"cmd": "android_wifi", "host": "8.8.8.8"})
            self.assertFalse(res.get("ok"))
            self.assertIn("Refusing to connect to non-private address", res.get("message", ""))

    def test_supervise_resets_counter_when_connected_host_missing(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            cfg_path = Path(tmpdir) / "config.toml"
            state_path = Path(tmpdir) / "state.json"
            ipc_path = Path(tmpdir) / "test.sock"

            daemon = Daemon(
                config_path=cfg_path,
                state_path=state_path,
                notify_enabled=False,
                ipc_path=ipc_path,
            )
            daemon.controller.state.connected_sink = "soundhop_test"
            daemon.controller.state.connected_peer = "peer-1"
            daemon.controller.state.connected_host = ""  # Missing host
            daemon.controller.state.connected_port = 0
            daemon._offline_count = 5

            daemon._supervise()
            self.assertEqual(daemon._offline_count, 0)


if __name__ == "__main__":
    unittest.main()
