"""Tests for utility helpers."""

import tempfile
import unittest
from pathlib import Path

from soundhop.util import (
    atomic_write,
    format_age,
    format_duration,
    sanitize_sink_name,
    slugify,
    split_host_port,
    tcp_server_arg,
)


class UtilTest(unittest.TestCase):
    def test_slugify(self) -> None:
        self.assertEqual(slugify("My Fedora-Laptop! 2026"), "my_fedora_laptop_2026")
        self.assertEqual(slugify(""), "device")
        self.assertEqual(slugify("---"), "device")
        self.assertEqual(slugify("CachyOS_Audio"), "cachyos_audio")

    def test_split_host_port(self) -> None:
        self.assertEqual(split_host_port("192.168.1.10:4713", 4713), ("192.168.1.10", 4713))
        self.assertEqual(split_host_port("192.168.1.10", 4713), ("192.168.1.10", 4713))
        self.assertEqual(split_host_port("[::1]:5000", 4713), ("::1", 5000))
        self.assertEqual(split_host_port("fe80::1", 4713), ("fe80::1", 4713))

    def test_tcp_server_arg(self) -> None:
        self.assertEqual(tcp_server_arg("192.168.1.5", 4713), "tcp:192.168.1.5:4713")
        self.assertEqual(tcp_server_arg("::1", 4713), "tcp:[::1]:4713")
        self.assertEqual(tcp_server_arg("localhost", 4713), "tcp:localhost:4713")

    def test_sanitize_sink_name(self) -> None:
        self.assertEqual(sanitize_sink_name("soundhop_fedora"), "soundhop_fedora")
        self.assertEqual(sanitize_sink_name("fedora-laptop"), "fedora-laptop")
        self.assertEqual(sanitize_sink_name("fedora laptop!"), "fedora_laptop")

    def test_format_age(self) -> None:
        self.assertEqual(format_age(0), "just now")
        self.assertEqual(format_age(None), "never")
        self.assertEqual(format_age(5), "5s ago")
        self.assertEqual(format_age(120), "2m ago")
        self.assertEqual(format_age(7200), "2h ago")

    def test_format_duration(self) -> None:
        self.assertEqual(format_duration(45), "45s")
        self.assertEqual(format_duration(125), "2m 05s")
        self.assertEqual(format_duration(3665), "1h 01m")

    def test_atomic_write(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            target = Path(tmpdir) / "sub" / "test.txt"
            atomic_write(target, "hello world\n", mode=0o600)
            self.assertTrue(target.exists())
            self.assertEqual(target.read_text(), "hello world\n")
            # Overwrite
            atomic_write(target, "updated\n")
            self.assertEqual(target.read_text(), "updated\n")


if __name__ == "__main__":
    unittest.main()
