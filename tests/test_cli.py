"""Tests for CLI arguments, help, and commands."""

import io
import os
import tempfile
import unittest
from contextlib import redirect_stdout

from soundhop.cli import build_parser, main
from soundhop.constants import VERSION


class CliTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmpdir = tempfile.TemporaryDirectory()
        self._orig_config = os.environ.get("XDG_CONFIG_HOME")
        self._orig_state = os.environ.get("XDG_STATE_HOME")
        os.environ["XDG_CONFIG_HOME"] = os.path.join(self.tmpdir.name, "config")
        os.environ["XDG_STATE_HOME"] = os.path.join(self.tmpdir.name, "state")

    def tearDown(self) -> None:
        if self._orig_config is not None:
            os.environ["XDG_CONFIG_HOME"] = self._orig_config
        else:
            os.environ.pop("XDG_CONFIG_HOME", None)
        if self._orig_state is not None:
            os.environ["XDG_STATE_HOME"] = self._orig_state
        else:
            os.environ.pop("XDG_STATE_HOME", None)
        self.tmpdir.cleanup()
    def test_parser_subcommands(self) -> None:
        parser = build_parser()
        # Verify subcommands parse cleanly
        args = parser.parse_args(["status", "--json"])
        self.assertEqual(args.func.__name__, "cmd_status")
        self.assertTrue(args.json)

        args = parser.parse_args(["connect", "my-laptop", "--port", "4715"])
        self.assertEqual(args.func.__name__, "cmd_connect")
        self.assertEqual(args.peer, "my-laptop")
        self.assertEqual(args.port, 4715)

        args = parser.parse_args(["share", "on"])
        self.assertEqual(args.func.__name__, "cmd_share")
        self.assertEqual(args.action, "on")

        args = parser.parse_args(["firewall", "open", "--dry-run"])
        self.assertEqual(args.func.__name__, "cmd_firewall")
        self.assertEqual(args.action, "open")
        self.assertTrue(args.dry_run)

    def test_version_command(self) -> None:
        buf = io.StringIO()
        with redirect_stdout(buf):
            ret = main(["version"])
        self.assertEqual(ret, 0)
        self.assertIn(VERSION, buf.getvalue())

    def test_bug_report_command(self) -> None:
        buf = io.StringIO()
        with redirect_stdout(buf):
            ret = main(["bug-report"])
        self.assertEqual(ret, 0)
        output = buf.getvalue()
        self.assertIn("Environment", output)
        self.assertIn("Diagnostic Checks", output)

    def test_completions_output(self) -> None:
        for shell in ("bash", "zsh", "fish"):
            buf = io.StringIO()
            with redirect_stdout(buf):
                ret = main(["completions", shell])
            self.assertEqual(ret, 0)
            output = buf.getvalue()
            self.assertIn("soundhop", output)
            self.assertIn("shop", output)

    def test_android_stop_targeted_kill_behavior(self) -> None:
        import signal
        from unittest.mock import MagicMock, patch
        from soundhop.cli import cmd_android_stop
        import argparse

        args = argparse.Namespace()

        # Case 1: Daemon running and request("android_stop") succeeds.
        # os.kill must NOT be called even though active PID is 12345.
        with patch("soundhop.cli.is_running", return_value=True), \
             patch("soundhop.cli.request") as mock_req, \
             patch("os.kill") as mock_kill:
            mock_req.side_effect = [
                {"session": {"pid": 12345}},  # android_status
                {"ok": True},                  # android_stop
            ]
            ret = cmd_android_stop(args)
            self.assertEqual(ret, 0)
            mock_kill.assert_not_called()

        # Case 2: Daemon running but request("android_stop") raises error.
        # Fallback should kill active PID 12345.
        with patch("soundhop.cli.is_running", return_value=True), \
             patch("soundhop.cli.request") as mock_req, \
             patch("os.kill") as mock_kill:
            mock_req.side_effect = [
                {"session": {"pid": 12345}},       # android_status
                RuntimeError("daemon timeout"),     # android_stop
            ]
            ret = cmd_android_stop(args)
            self.assertEqual(ret, 0)
            mock_kill.assert_called_once_with(12345, signal.SIGTERM)


if __name__ == "__main__":
    unittest.main()

