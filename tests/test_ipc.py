"""Tests for Unix domain socket IPC."""

import tempfile
import unittest
from pathlib import Path

from soundhop.ipc import DaemonNotRunning, IpcError, IpcServer, is_running, request


class IpcTest(unittest.TestCase):
    def test_ipc_communication(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            sock_path = Path(tmpdir) / "test.sock"

            def handler(req: dict) -> dict:
                cmd = req.get("cmd") or req.get("command")
                if cmd == "ping":
                    return {"pong": True, "value": req.get("value")}
                elif cmd == "err":
                    raise ValueError("something bad happened")
                return {"unknown": True}

            server = IpcServer(handler, path=sock_path)
            server.start()
            try:
                self.assertTrue(is_running(sock_path))

                # Normal ping
                res = request("ping", path=sock_path, value=123)
                self.assertEqual(res, {"ok": True, "pong": True, "value": 123})

                # Error handling
                with self.assertRaises(IpcError):
                    request("err", path=sock_path)
            finally:
                server.stop()

            self.assertFalse(is_running(sock_path))
            with self.assertRaises(DaemonNotRunning):
                request("ping", path=sock_path)

    def test_ipc_tcp_fallback(self) -> None:
        """Test Windows loopback TCP fallback when AF_UNIX is unavailable."""
        import socket
        from unittest.mock import patch

        with tempfile.TemporaryDirectory() as tmpdir:
            sock_path = Path(tmpdir) / "test.sock"

            def handler(req: dict) -> dict:
                return {"pong": True, "tcp": True}

            # Simulate absence of AF_UNIX or error on AF_UNIX bind
            orig_socket = socket.socket

            def fake_socket(family, *args, **kwargs):
                if family == getattr(socket, "AF_UNIX", None):
                    raise OSError("AF_UNIX not supported on this platform")
                return orig_socket(family, *args, **kwargs)

            with patch("socket.socket", side_effect=fake_socket):
                server = IpcServer(handler, path=sock_path)
                server.start()
                try:
                    self.assertTrue(server._is_tcp)
                    self.assertTrue(sock_path.with_suffix(".port").exists())
                    self.assertTrue(sock_path.with_suffix(".auth").exists())
                    self.assertTrue(is_running(sock_path))

                    res = request("ping", path=sock_path)
                    self.assertTrue(res.get("pong"))
                    self.assertTrue(res.get("tcp"))

                    # Verify raw unauthenticated request over TCP is rejected
                    port = int(sock_path.with_suffix(".port").read_text(encoding="utf-8").strip())
                    raw_sock = orig_socket(socket.AF_INET, socket.SOCK_STREAM)
                    raw_sock.connect(("127.0.0.1", port))
                    raw_sock.sendall(b'{"cmd": "ping"}\n')
                    resp = raw_sock.recv(4096)
                    raw_sock.close()
                    import json

                    resp_json = json.loads(resp.decode("utf-8"))
                    self.assertFalse(resp_json.get("ok"))
                    self.assertIn("Unauthorized", resp_json.get("error", ""))
                finally:
                    server.stop()

            self.assertFalse(sock_path.with_suffix(".port").exists())
            self.assertFalse(sock_path.with_suffix(".auth").exists())


if __name__ == "__main__":
    unittest.main()
