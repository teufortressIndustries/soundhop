"""Unit tests for soundhop.android."""

import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import MagicMock, patch

from soundhop.android import (
    AdbDevice,
    AndroidCaptureSession,
    AndroidError,
    AndroidManager,
    AudioEncoderInfo,
    ToolStatus,
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
        with patch.object(mgr, "find_adb", return_value=Path("/usr/bin/adb")), \
             patch.object(mgr, "enrich_device_info") as mock_enrich:
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


if __name__ == "__main__":
    unittest.main()
