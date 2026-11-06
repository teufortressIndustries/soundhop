"""Unit tests for soundhop.android."""

import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import MagicMock, patch

from soundhop.android import (
    AdbDevice,
    AndroidCaptureSession,
    AndroidManager,
    AudioEncoderInfo,
)
from soundhop.util import Result


class AndroidTest(unittest.TestCase):
    def test_adb_device_model(self) -> None:
        dev = AdbDevice(
            serial="192.168.1.99:5555",
            state="device",
            model="2211133G",
            manufacturer="Xiaomi",
            android_version="15",
            sdk_version=35,
            is_wifi=True,
        )
        self.assertTrue(dev.is_ready)
        self.assertEqual(dev.display_name, "Xiaomi 2211133G")

        unauth = AdbDevice(serial="ABC123", state="unauthorized")
        self.assertFalse(unauth.is_ready)
        self.assertEqual(unauth.display_name, "ABC123")

    def test_audio_encoder_info(self) -> None:
        info = AudioEncoderInfo(
            codecs={
                "opus": ["c2.android.opus.encoder"],
                "aac": ["c2.android.aac.encoder", "OMX.google.aac.encoder"],
                "flac": ["c2.android.flac.encoder"],
            }
        )
        self.assertIn("opus", info.available_codecs)
        self.assertIn("aac", info.available_codecs)
        self.assertIn("flac", info.available_codecs)
        self.assertIn("raw", info.available_codecs)

    def test_build_capture_args(self) -> None:
        mgr = AndroidManager()
        with patch.object(mgr, "find_scrcpy", return_value=Path("/usr/bin/scrcpy")):
            args = mgr.build_capture_args(
                "192.168.1.99:5555",
                codec="aac",
                source="output",
                buffer_ms=60,
                bit_rate=192000,
                duplicate=True,
            )
            self.assertEqual(args[0], "/usr/bin/scrcpy")
            self.assertIn("-s", args)
            self.assertIn("192.168.1.99:5555", args)
            self.assertIn("--no-video", args)
            self.assertIn("--no-window", args)
            self.assertIn("--no-control", args)
            self.assertIn("--require-audio", args)
            self.assertIn("--audio-codec=aac", args)
            self.assertIn("--audio-source=output", args)
            self.assertIn("--audio-buffer=60", args)
            self.assertIn("--audio-bit-rate=192000", args)
            self.assertIn("--audio-dup", args)

    @patch("soundhop.android.run")
    def test_list_devices(self, mock_run) -> None:
        mock_output = (
            "List of devices attached\n"
            "192.168.1.99:5555      device product:fuxi_global model:2211133G device:fuxi transport_id:3\n"
            "USB001234             unauthorized transport_id:4\n"
        )
        mock_run.return_value = Result(
            args=["adb", "devices", "-l"],
            returncode=0,
            stdout=mock_output,
            stderr="",
        )

        mgr = AndroidManager()
        with (
            patch.object(mgr, "find_adb", return_value=Path("/usr/bin/adb")),
            patch.object(mgr, "enrich_device_info") as mock_enrich,
        ):
            devices = mgr.list_devices()
            self.assertEqual(len(devices), 2)
            self.assertEqual(devices[0].serial, "192.168.1.99:5555")
            self.assertTrue(devices[0].is_wifi)
            self.assertTrue(devices[0].is_ready)
            self.assertEqual(devices[0].model, "2211133G")

            self.assertEqual(devices[1].serial, "USB001234")
            self.assertFalse(devices[1].is_wifi)
            self.assertFalse(devices[1].is_ready)
            mock_enrich.assert_called_once()

    @patch("soundhop.android.run")
    def test_list_encoders(self, mock_run) -> None:
        sample_output = """scrcpy 5.0.1 <https://github.com/Genymobile/scrcpy>
[server] INFO: Device: [Xiaomi] Xiaomi 2211133G (Android 15)
[server] INFO: List of video encoders:
    --video-codec=h264 --video-encoder=c2.qti.avc.encoder
[server] INFO: List of audio encoders:
    --audio-codec=opus --audio-encoder=c2.android.opus.encoder        (sw)
    --audio-codec=aac --audio-encoder=c2.android.aac.encoder          (sw)
    --audio-codec=aac --audio-encoder=OMX.google.aac.encoder          (sw) (alias for c2.android.aac.encoder)
    --audio-codec=flac --audio-encoder=c2.android.flac.encoder        (sw)
"""
        mock_run.return_value = Result(
            args=["scrcpy", "--list-encoders"],
            returncode=0,
            stdout=sample_output,
            stderr="",
        )

        mgr = AndroidManager()
        with patch.object(mgr, "find_scrcpy", return_value=Path("/usr/bin/scrcpy")):
            info = mgr.list_encoders()
            self.assertIn("opus", info.codecs)
            self.assertIn("aac", info.codecs)
            self.assertIn("flac", info.codecs)
            self.assertEqual(len(info.codecs["aac"]), 2)

    @patch("subprocess.Popen")
    def test_capture_session(self, mock_popen) -> None:
        fake_proc = MagicMock()
        fake_proc.poll.return_value = None
        fake_proc.pid = 9999
        fake_proc.stdout.readline.side_effect = [
            "INFO: scrcpy 5.0.1",
            "",
        ]
        mock_popen.return_value = fake_proc

        mgr = AndroidManager()
        with patch.object(mgr, "find_scrcpy", return_value=Path("/usr/bin/scrcpy")):
            session = AndroidCaptureSession(mgr)
            self.assertFalse(session.is_active)

            st = session.start("192.168.1.99:5555", device_name="MyPhone", codec="aac")
            self.assertTrue(st["active"])
            self.assertEqual(st["serial"], "192.168.1.99:5555")
            self.assertEqual(st["device_name"], "MyPhone")
            self.assertEqual(st["codec"], "aac")
            self.assertEqual(st["pid"], 9999)

            st_stop = session.stop()
            self.assertFalse(st_stop["active"])
            fake_proc.terminate.assert_called_once()

    def test_adb_device_major_version(self) -> None:
        d1 = AdbDevice(serial="s1", state="device", android_version="15")
        self.assertEqual(d1.major_version, 15)

        d2 = AdbDevice(serial="s2", state="device", android_version="13.0_r1")
        self.assertEqual(d2.major_version, 13)

        d3 = AdbDevice(serial="s3", state="device", android_version="UpsideDownCake")
        self.assertEqual(d3.major_version, 0)

        d4 = AdbDevice(serial="s4", state="device", android_version="")
        self.assertEqual(d4.major_version, 0)

    def test_install_scrcpy_symlinks_and_traversal(self) -> None:
        import io
        import tarfile

        with tempfile.TemporaryDirectory() as tmpdir:
            dest_dir = Path(tmpdir) / "managed_scrcpy"
            archive_path = Path(tmpdir) / "test_scrcpy.tar.gz"

            # Create an in-memory tar.gz with:
            # 1. scrcpy binary
            # 2. libfoo.so.1 (regular file)
            # 3. libfoo.so (symlink to libfoo.so.1)
            # 4. ../../escaped.txt (traversal attempt)
            with tarfile.open(archive_path, "w:gz") as tar:
                # 1. scrcpy binary
                bin_data = b"#!/bin/sh\necho scrcpy\n"
                tinfo_bin = tarfile.TarInfo("scrcpy-bundle/scrcpy")
                tinfo_bin.size = len(bin_data)
                tinfo_bin.mode = 0o755
                tar.addfile(tinfo_bin, io.BytesIO(bin_data))

                # 2. libfoo.so.1
                lib_data = b"\x7fELFfake_so"
                tinfo_lib = tarfile.TarInfo("scrcpy-bundle/libfoo.so.1")
                tinfo_lib.size = len(lib_data)
                tinfo_lib.mode = 0o644
                tar.addfile(tinfo_lib, io.BytesIO(lib_data))

                # 3. libfoo.so (symlink to libfoo.so.1)
                tinfo_sym = tarfile.TarInfo("scrcpy-bundle/libfoo.so")
                tinfo_sym.type = tarfile.SYMTYPE
                tinfo_sym.linkname = "libfoo.so.1"
                tar.addfile(tinfo_sym)

                # 4. Hardlink: libbar.so -> scrcpy-bundle/libfoo.so.1
                tinfo_lnk = tarfile.TarInfo("scrcpy-bundle/libbar.so")
                tinfo_lnk.type = tarfile.LNKTYPE
                tinfo_lnk.linkname = "scrcpy-bundle/libfoo.so.1"
                tar.addfile(tinfo_lnk)

                # 5. Traversal attempt
                esc_data = b"pwned"
                tinfo_esc = tarfile.TarInfo("scrcpy-bundle/../../escaped.txt")
                tinfo_esc.size = len(esc_data)
                tar.addfile(tinfo_esc, io.BytesIO(esc_data))

            # Pre-populate dest_dir with a conflicting directory at libfoo.so to ensure replacement works
            (dest_dir / "libfoo.so").mkdir(parents=True, exist_ok=True)

            mgr = AndroidManager()
            with (
                patch.object(mgr, "managed_dir", return_value=dest_dir),
                patch("platform.machine", return_value="x86_64"),
                patch("urllib.request.urlopen") as mock_urlopen,
            ):
                # Mock download to return the archive content
                archive_bytes = archive_path.read_bytes()
                mock_resp = MagicMock()
                mock_resp.headers = {"Content-Length": str(len(archive_bytes))}
                mock_resp.read.side_effect = [archive_bytes, b""]
                mock_urlopen.return_value.__enter__.return_value = mock_resp

                bin_path = mgr.install_scrcpy()
                self.assertTrue(bin_path.exists())
                self.assertTrue(os.access(bin_path, os.X_OK))

                # Verify libfoo.so.1 exists
                lib_path = dest_dir / "libfoo.so.1"
                self.assertTrue(lib_path.is_file())

                # Verify libfoo.so symlink replaced directory and points to libfoo.so.1
                sym_path = dest_dir / "libfoo.so"
                self.assertTrue(sym_path.is_symlink())
                self.assertEqual(os.readlink(sym_path), "libfoo.so.1")

                # Verify hardlink libbar.so exists
                bar_path = dest_dir / "libbar.so"
                self.assertTrue(bar_path.is_file())
                self.assertEqual(bar_path.stat().st_ino, lib_path.stat().st_ino)

                # Verify traversal attempt did NOT write outside dest_dir
                escaped = Path(tmpdir) / "escaped.txt"
                self.assertFalse(escaped.exists())

    def test_get_device_wifi_ip(self) -> None:
        mgr = AndroidManager()

        # Method 1: ip -f inet addr show wlan0
        with patch.object(
            mgr,
            "_run_adb",
            return_value=Result(
                args=["adb"],
                returncode=0,
                stdout="21: wlan0: <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 1500 qdisc mq state UP group default qlen 3000\n    inet 192.168.1.123/24 brd 192.168.1.255 scope global wlan0\n",
                stderr="",
            ),
        ):
            ip = mgr.get_device_wifi_ip("USB123")
            self.assertEqual(ip, "192.168.1.123")

        # Method 1b: wlan1 via general ip addr
        with patch.object(
            mgr,
            "_run_adb",
            side_effect=[
                Result(args=["adb"], returncode=1, stdout="", stderr="Device not found"),
                Result(
                    args=["adb"],
                    returncode=0,
                    stdout="inet 192.168.1.124/24 brd 192.168.1.255 scope global wlan1\n",
                    stderr="",
                ),
            ],
        ):
            ip = mgr.get_device_wifi_ip("USB123")
            self.assertEqual(ip, "192.168.1.124")

        # Method 2: ip route show
        with patch.object(
            mgr,
            "_run_adb",
            side_effect=[
                Result(args=["adb"], returncode=1, stdout="", stderr=""),
                Result(args=["adb"], returncode=1, stdout="", stderr=""),
                Result(
                    args=["adb"],
                    returncode=0,
                    stdout="192.168.1.0/24 dev wlan0 proto kernel scope link src 192.168.1.125\n",
                    stderr="",
                ),
            ],
        ):
            ip = mgr.get_device_wifi_ip("USB123")
            self.assertEqual(ip, "192.168.1.125")

        # Method 3: getprop
        with patch.object(
            mgr,
            "_run_adb",
            side_effect=[
                Result(args=["adb"], returncode=1, stdout="", stderr=""),
                Result(args=["adb"], returncode=1, stdout="", stderr=""),
                Result(args=["adb"], returncode=1, stdout="", stderr=""),
                Result(args=["adb"], returncode=0, stdout="192.168.1.126\n", stderr=""),
            ],
        ):
            ip = mgr.get_device_wifi_ip("USB123")
            self.assertEqual(ip, "192.168.1.126")

    def test_enable_tcpip_and_connect_wifi(self) -> None:
        mgr = AndroidManager()
        with patch.object(
            mgr,
            "_run_adb",
            return_value=Result(
                args=["adb", "tcpip", "5555"],
                returncode=0,
                stdout="restarting in TCP mode port: 5555",
                stderr="",
            ),
        ):
            ok, msg = mgr.enable_tcpip("USB123")
            self.assertTrue(ok)
            self.assertIn("5555", msg)

        with patch.object(
            mgr,
            "_run_adb",
            return_value=Result(
                args=["adb", "connect", "192.168.1.50:5555"],
                returncode=0,
                stdout="connected to 192.168.1.50:5555",
                stderr="",
            ),
        ):
            ok, msg = mgr.connect_wifi("192.168.1.50")
            self.assertTrue(ok)
            self.assertIn("connected to 192.168.1.50:5555", msg.lower())


if __name__ == "__main__":
    unittest.main()
