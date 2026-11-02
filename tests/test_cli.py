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


if __name__ == "__main__":
    unittest.main()
