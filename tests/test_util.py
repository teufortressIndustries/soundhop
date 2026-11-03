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

    def test_atomic_write_concurrency(self) -> None:
        import concurrent.futures
        with tempfile.TemporaryDirectory() as tmpdir:
            target = Path(tmpdir) / "concurrent.txt"

            def writer(idx: int) -> None:
                atomic_write(target, f"thread-{idx}\n", mode=0o600)

            with concurrent.futures.ThreadPoolExecutor(max_workers=8) as ex:
                futures = [ex.submit(writer, i) for i in range(50)]
                for f in concurrent.futures.as_completed(futures):
                    f.result()

            self.assertTrue(target.exists())
            content = target.read_text()
            self.assertTrue(content.startswith("thread-"))
            self.assertEqual(target.stat().st_mode & 0o777, 0o600)

    def test_is_private_address(self) -> None:
        from soundhop.util import is_private_address
        # Private IPv4
        self.assertTrue(is_private_address("127.0.0.1"))
        self.assertTrue(is_private_address("192.168.1.1"))
        self.assertTrue(is_private_address("10.0.0.1"))
        self.assertTrue(is_private_address("172.16.0.1"))
        self.assertTrue(is_private_address("169.254.1.1"))
        # Localhost string
        self.assertTrue(is_private_address("localhost"))
        # Private IPv6 & bracketed
        self.assertTrue(is_private_address("::1"))
        self.assertTrue(is_private_address("[::1]"))
        self.assertTrue(is_private_address("fe80::1"))
        self.assertTrue(is_private_address("[fe80::1]"))
        self.assertTrue(is_private_address("fe80::1%eth0"))
        self.assertTrue(is_private_address("fd00::1"))
        # Public IP & bracketed
        self.assertFalse(is_private_address("8.8.8.8"))
        self.assertFalse(is_private_address("[8.8.8.8]"))
        self.assertFalse(is_private_address("1.1.1.1"))
        self.assertFalse(is_private_address("93.184.216.34"))
        # Empty or invalid
        self.assertFalse(is_private_address(""))
        self.assertFalse(is_private_address("invalid-host-that-does-not-exist-12345.xyz"))

    def test_broadcast_addresses_cache(self) -> None:
        from soundhop.util import broadcast_addresses, invalidate_broadcast_cache
        from unittest.mock import patch
        invalidate_broadcast_cache()
        addrs1 = broadcast_addresses()
        self.assertIn("127.0.0.1", addrs1)
        self.assertIn("255.255.255.255", addrs1)

        # Call again with run patched to fail; should return cached targets without spawning subprocess
        with patch("soundhop.util.run", side_effect=RuntimeError("Subprocess should not be called")):
            addrs2 = broadcast_addresses(include_loopback=False)
            self.assertNotIn("127.0.0.1", addrs2)
            self.assertIn("255.255.255.255", addrs2)

        # Invalidate cache; next call without loopback must call run again
        invalidate_broadcast_cache()
        addrs3 = broadcast_addresses(include_loopback=False)
        self.assertIn("255.255.255.255", addrs3)


if __name__ == "__main__":
    unittest.main()

