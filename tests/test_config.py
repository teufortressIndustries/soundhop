"""Tests for configuration parsing, serialization, and validation."""

import tempfile
import unittest
from pathlib import Path

from soundhop.config import (
    Config,
    ConfigError,
    _as_bool,
    parse_config_value,
)


class ConfigTest(unittest.TestCase):
    def test_default_config(self) -> None:
        cfg = Config(Path("/tmp/soundhop-dummy.toml"))
        self.assertEqual(len(cfg.device.id), 32)
        self.assertTrue(bool(cfg.device.name))
        self.assertEqual(cfg.sharing.port, 4713)
        self.assertEqual(cfg.sharing.security, "trusted-lan")
        self.assertTrue(cfg.connection.auto_connect)
        self.assertTrue(cfg.connection.disconnect_on_quit)
        self.assertTrue(cfg.discovery.udp)

    def test_as_bool(self) -> None:
        self.assertTrue(_as_bool(True, False))
        self.assertFalse(_as_bool(False, True))
        self.assertTrue(_as_bool("true", False))
        self.assertTrue(_as_bool("YES", False))
        self.assertTrue(_as_bool("on", False))
        self.assertTrue(_as_bool("1", False))
        self.assertFalse(_as_bool("false", True))
        self.assertFalse(_as_bool("no", True))
        self.assertFalse(_as_bool("off", True))
        self.assertFalse(_as_bool("0", True))
        self.assertEqual(_as_bool("invalid", True), True)

    def test_roundtrip_toml(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            cfg_path = Path(tmpdir) / "config.toml"
            cfg = Config(path=cfg_path)
            cfg.device.name = "Test-Rig"
            cfg.sharing.port = 4720
            cfg.sharing.enabled = True
            cfg.connection.auto_connect = False
            cfg.connection.disconnect_on_quit = False
            cfg.save()

            loaded = Config.load(cfg_path)
            self.assertEqual(loaded.device.name, "Test-Rig")
            self.assertEqual(loaded.sharing.port, 4720)
            self.assertTrue(loaded.sharing.enabled)
            self.assertFalse(loaded.connection.auto_connect)
            self.assertFalse(loaded.connection.disconnect_on_quit)

    def test_normalize_validation(self) -> None:
        cfg = Config(Path("/tmp/test.toml"))
        cfg.sharing.security = "invalid-security-mode"
        with self.assertRaises(ConfigError):
            cfg.normalize()

        cfg.sharing.security = "trusted-lan"
        cfg.sharing.allowed_networks = ["not-an-ip-network"]
        with self.assertRaises(ConfigError):
            cfg.normalize()

        cfg.sharing.allowed_networks = "not-a-list-or-str"
        cfg.sharing.allowed_networks = 12345  # type: ignore
        with self.assertRaises(ConfigError):
            cfg.normalize()

    def test_parse_config_value(self) -> None:
        self.assertEqual(parse_config_value("true"), True)
        self.assertEqual(parse_config_value("false"), False)
        self.assertEqual(parse_config_value("4713"), 4713)
        self.assertEqual(
            parse_config_value('["192.168.1.0/24", "10.0.0.0/8"]'),
            ["192.168.1.0/24", "10.0.0.0/8"],
        )
        self.assertEqual(parse_config_value("hello world"), "hello world")


if __name__ == "__main__":
    unittest.main()
