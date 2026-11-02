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


if __name__ == "__main__":
    unittest.main()
