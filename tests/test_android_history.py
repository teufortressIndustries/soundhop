import tempfile
import unittest
from pathlib import Path
from unittest.mock import MagicMock, patch

from soundhop.android import AndroidManager
from soundhop.state import KnownAndroidDevice, State
from soundhop.util import Result


class AndroidHistoryTest(unittest.TestCase):
    def test_known_android_device_model(self) -> None:
        dev = KnownAndroidDevice(
            serial="192.168.1.50:5555",
            model="Pixel 8",
            manufacturer="Google",
            last_ip="192.168.1.50",
            last_port=5555,
            last_seen=100.0,
            last_connected=100.0,
            is_wifi=True,
        )
        self.assertEqual(dev.display_name, "Google Pixel 8")
        data = dev.from_dict(
            {
                "serial": "192.168.1.50:5555",
                "model": "Pixel 8",
                "manufacturer": "Google",
                "last_ip": "192.168.1.50",
                "last_port": 5555,
                "last_seen": 100.0,
                "last_connected": 100.0,
                "is_wifi": True,
            }
        )
        self.assertEqual(data.serial, "192.168.1.50:5555")
        self.assertTrue(data.is_wifi)

    def test_state_remember_and_prune(self) -> None:
        state = State()
        now = 1700000000.0
        dev = state.remember_android_device(
            "device123",
            model="Galaxy S23",
            manufacturer="Samsung",
            ip="192.168.1.10",
            port=5555,
            is_wifi=True,
            connected=True,
            when=now,
        )
        self.assertEqual(dev.serial, "device123")
        self.assertEqual(dev.model, "Galaxy S23")
        self.assertEqual(dev.last_seen, now)
        self.assertEqual(dev.last_connected, now)

        # Update existing
        state.remember_android_device(
            "device123",
            model="Galaxy S23 Ultra",
            when=now + 100.0,
        )
        self.assertEqual(state.known_android_devices["device123"].model, "Galaxy S23 Ultra")
        self.assertEqual(state.known_android_devices["device123"].last_seen, now + 100.0)

        # Persistence save and load
        with tempfile.TemporaryDirectory() as td:
            path = Path(td) / "state.json"
            # Disable auto-pruning with old timestamp for persistence test
            with patch("soundhop.state.daemon_time", return_value=now + 200.0):
                state.save(path)
                loaded = State.load(path)
                self.assertIn("device123", loaded.known_android_devices)
                self.assertEqual(
                    loaded.known_android_devices["device123"].model, "Galaxy S23 Ultra"
                )

                # Forget
                loaded.forget_android_device("device123")
                self.assertNotIn("device123", loaded.known_android_devices)

    def test_reconnect_known_devices(self) -> None:
        mgr = AndroidManager()
        known = [
            KnownAndroidDevice(
                serial="192.168.1.101:5555",
                last_ip="192.168.1.101",
                last_port=5555,
                is_wifi=True,
            )
        ]
        with (
            patch.object(mgr, "list_devices", return_value=[]),
            patch.object(mgr, "connect_wifi", return_value=(True, "connected")),
        ):
            connected = mgr.reconnect_known_devices(known)
            self.assertEqual(connected, ["192.168.1.101:5555"])

    def test_discover_mdns_services(self) -> None:
        mgr = AndroidManager()
        fake_stdout = (
            "List of discoverable devices\n"
            "Pixel_Phone    _adb._tcp.    192.168.1.55:41235\n"
            "Galaxy_Tab     _adb-tls-connect._tcp.    192.168.1.56:39999\n"
        )
        with (
            patch.object(mgr, "find_adb", return_value=Path("/usr/bin/adb")),
            patch.object(mgr, "_run_adb", return_value=Result([], 0, fake_stdout, "")),
        ):
            services = mgr.discover_mdns_services()
            self.assertEqual(len(services), 2)
            self.assertEqual(services[0], ("Pixel_Phone", "192.168.1.55", 41235))
            self.assertEqual(services[1], ("Galaxy_Tab", "192.168.1.56", 39999))

    def test_android_capture_as_mic(self) -> None:
        from soundhop.android import AndroidCaptureSession
        from soundhop.backend import AudioBackend

        backend = AudioBackend(runner=lambda args, **kwargs: Result(args, 0, "", ""))
        mgr = AndroidManager()

        with (
            patch.object(backend, "is_pipewire", return_value=False),
            patch.object(backend, "load_module", side_effect=[111, 222]),
            patch.object(backend, "unload_module", return_value=True) as mock_unload,
            patch.object(backend, "default_source", return_value="alsa_input.hardware"),
            patch.object(backend, "set_default_source") as mock_set_source,
            patch.object(backend, "move_source_outputs") as mock_move,
            patch.object(mgr, "find_scrcpy", return_value=Path("/usr/bin/scrcpy")),
            patch("subprocess.Popen") as mock_popen,
        ):
            proc_mock = MagicMock()
            proc_mock.poll.return_value = None
            proc_mock.pid = 9999
            proc_mock.stdout.readline.return_value = ""
            mock_popen.return_value = proc_mock

            session = AndroidCaptureSession(mgr, backend=backend)
            status = session.start("device_serial", device_name="MyPhone", as_mic=True)

            self.assertTrue(status["active"])
            self.assertTrue(status["as_mic"])
            self.assertEqual(status["virtual_source"], "soundhop_android_mic")
            self.assertEqual(session._virtual_sink_module, 111)
            self.assertEqual(session._virtual_source_module, 222)

            # PULSE_SINK should be set in environment for first Popen call (scrcpy)
            scrcpy_call = mock_popen.call_args_list[0]
            _, kwargs = scrcpy_call
            self.assertIn("env", kwargs)
            self.assertEqual(kwargs["env"]["PULSE_SINK"], "soundhop_android_mic_sink")

            mock_set_source.assert_called_with("soundhop_android_mic")
            mock_move.assert_called_with("soundhop_android_mic", exclude_modules=[222, 111])

            # Stop capture
            stop_status = session.stop()
            self.assertFalse(stop_status["active"])
            self.assertFalse(stop_status["as_mic"])
            mock_unload.assert_any_call(222)
            mock_unload.assert_any_call(111)
            mock_set_source.assert_called_with("alsa_input.hardware")
            mock_move.assert_called_with("alsa_input.hardware")

    def test_android_capture_as_mic_pipewire(self) -> None:
        from soundhop.android import AndroidCaptureSession
        from soundhop.backend import AudioBackend

        backend = AudioBackend(runner=lambda args, **kwargs: Result(args, 0, "", ""))
        mgr = AndroidManager()

        with (
            patch.object(backend, "is_pipewire", return_value=True),
            patch.object(backend, "load_module", side_effect=[555, 666]),
            patch.object(backend, "unload_module", return_value=True) as mock_unload,
            patch.object(backend, "default_source", return_value="alsa_input.mic"),
            patch.object(backend, "set_default_source") as mock_set_source,
            patch.object(backend, "move_source_outputs") as mock_move,
            patch.object(mgr, "find_scrcpy", return_value=Path("/usr/bin/scrcpy")),
            patch("subprocess.Popen") as mock_popen,
        ):
            proc_mock = MagicMock()
            proc_mock.poll.return_value = None
            proc_mock.pid = 8888
            proc_mock.stdout.readline.return_value = ""
            mock_popen.return_value = proc_mock

            session = AndroidCaptureSession(mgr, backend=backend)
            status = session.start("device_serial", device_name="MyPhone", as_mic=True)

            self.assertTrue(status["active"])
            self.assertTrue(status["as_mic"])
            self.assertEqual(status["virtual_source"], "soundhop_android_mic")
            self.assertEqual(session._virtual_sink_module, 555)
            self.assertEqual(session._virtual_source_module, 666)

            scrcpy_call = mock_popen.call_args_list[0]
            _, kwargs = scrcpy_call
            self.assertIn("env", kwargs)
            self.assertIn("target.object", kwargs["env"]["PIPEWIRE_PROPS"])
            self.assertEqual(kwargs["env"]["PIPEWIRE_NODE"], "soundhop_android_mic_sink")

            mock_set_source.assert_called_with("soundhop_android_mic")
            mock_move.assert_called_with("soundhop_android_mic", exclude_modules=[666, 555])

            stop_status = session.stop()
            self.assertFalse(stop_status["active"])
            mock_unload.assert_any_call(666)
            mock_unload.assert_any_call(555)
            mock_set_source.assert_called_with("alsa_input.mic")


if __name__ == "__main__":
    unittest.main()
