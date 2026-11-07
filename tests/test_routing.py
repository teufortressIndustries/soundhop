import json
import unittest
from unittest.mock import MagicMock, patch

from soundhop.backend import AudioBackend, Module, Sink, SinkInputInfo
from soundhop.controller import Controller, ControllerError
from soundhop.util import Result


class RoutingTest(unittest.TestCase):
    def test_sink_input_info_model(self) -> None:
        info = SinkInputInfo(
            id=42,
            sink=1,
            sink_name="alsa_output.pci",
            app_name="Spotify",
            binary="spotify",
            pid=12345,
            media_name="Song - Artist",
            icon_name="spotify",
            volume_percent=85,
            mute=False,
            is_remote=False,
        )
        self.assertEqual(info.display_name, "Spotify")
        d = info.to_dict()
        self.assertEqual(d["id"], 42)
        self.assertEqual(d["volume_percent"], 85)

    def test_detailed_sink_inputs_json_parsing(self) -> None:
        raw_json = json.dumps(
            [
                {
                    "index": 101,
                    "sink": 2,
                    "mute": False,
                    "volume": {"front-left": {"value_percent": "75%"}},
                    "properties": {
                        "application.name": "Firefox",
                        "application.process.binary": "firefox",
                        "application.process.id": "4567",
                        "media.name": "YouTube Audio",
                    },
                }
            ]
        )
        backend = AudioBackend(runner=lambda args, **kwargs: Result(args, 0, raw_json, ""))
        with (
            patch.object(
                backend, "sinks", return_value=[Sink(id=2, name="soundhop-remote-sink")]
            ),
            patch.object(
                backend,
                "soundhop_modules",
                return_value=[
                    Module(id=50, name="module-tunnel-sink", args="sink_name=soundhop-remote-sink")
                ],
            ),
        ):
            inputs = backend.detailed_sink_inputs()
            self.assertEqual(len(inputs), 1)
            item = inputs[0]
            self.assertEqual(item.id, 101)
            self.assertEqual(item.app_name, "Firefox")
            self.assertEqual(item.binary, "firefox")
            self.assertEqual(item.pid, 4567)
            self.assertEqual(item.volume_percent, 75)
            self.assertTrue(item.is_remote)

    def test_detailed_sink_inputs_text_fallback(self) -> None:
        text_output = """Sink Input #88
\tDriver: PipeWire
\tOwner Module: n/a
\tClient: 12
\tSink: 0
\tSample Specification: float32le 2ch 48000Hz
\tChannel Map: front-left,front-right
\tFormat: pcm, format.sample_format = "float32le"
\tVolume: front-left: 65536 / 100% / 0.00 dB,   front-right: 65536 / 100% / 0.00 dB
\tMute: no
\tProperties:
\t\tmedia.name = "AudioStream"
\t\tapplication.name = "VLC media player"
\t\tapplication.process.binary = "vlc"
\t\tapplication.process.id = "9876"
"""

        def mock_runner(args, **kwargs):
            if "-f" in args:
                return Result(args, 1, "", "unsupported flag -f")
            return Result(args, 0, text_output, "")

        backend = AudioBackend(runner=mock_runner)
        with (
            patch.object(backend, "sinks", return_value=[Sink(id=0, name="local-speaker")]),
            patch.object(backend, "soundhop_modules", return_value=[]),
        ):
            inputs = backend.detailed_sink_inputs()
            self.assertEqual(len(inputs), 1)
            item = inputs[0]
            self.assertEqual(item.id, 88)
            self.assertEqual(item.app_name, "VLC media player")
            self.assertEqual(item.binary, "vlc")
            self.assertEqual(item.pid, 9876)
            self.assertEqual(item.volume_percent, 100)
            self.assertFalse(item.is_remote)

    def test_move_app_streams(self) -> None:
        fake_inputs = [
            SinkInputInfo(
                id=10,
                sink=0,
                app_name="Spotify",
                binary="spotify",
                pid=1111,
            ),
            SinkInputInfo(
                id=20,
                sink=0,
                app_name="Firefox",
                binary="firefox",
                pid=2222,
            ),
        ]
        moved_calls = []

        def mock_pactl(cmd, *args, **kwargs):
            if cmd == "move-sink-input":
                moved_calls.append(args)
                return Result([], 0, "", "")
            return Result([], 0, "", "")

        backend = AudioBackend()
        backend._pactl = mock_pactl
        with patch.object(backend, "detailed_sink_inputs", return_value=fake_inputs):
            # Match by app_name
            moved = backend.move_app_streams("spotify", "remote-sink")
            self.assertEqual(moved, 1)
            self.assertIn(("10", "remote-sink"), moved_calls)

            # Match by ID
            moved = backend.move_app_streams(20, "local-sink")
            self.assertEqual(moved, 1)
            self.assertIn(("20", "local-sink"), moved_calls)

            # No match
            moved = backend.move_app_streams("nonexistent", "sink")
            self.assertEqual(moved, 0)

        # Stream already on target sink should be skipped (prevents pactl subscribe feedback loop)
        already_routed = [
            SinkInputInfo(
                id=30,
                sink=2,
                sink_name="remote-sink",
                app_name="Spotify",
                binary="spotify",
                pid=1111,
            )
        ]
        moved_calls.clear()
        with patch.object(backend, "detailed_sink_inputs", return_value=already_routed):
            moved = backend.move_app_streams("spotify", "remote-sink")
            self.assertEqual(moved, 0)
            self.assertEqual(len(moved_calls), 0)

    def test_controller_route_app(self) -> None:
        ctrl = Controller()
        ctrl.state.connected_sink = "soundhop-remote"
        with patch.object(ctrl.backend, "move_app_streams", return_value=1):
            res = ctrl.route_app("spotify", "remote")
            self.assertEqual(res["moved"], 1)
            self.assertEqual(res["target_sink"], "soundhop-remote")
            self.assertEqual(ctrl.state.routed_apps.get("spotify"), "remote")

        # Disconnected error when routing to remote without connection
        ctrl.state.connected_sink = ""
        with self.assertRaises(ControllerError):
            ctrl.route_app("spotify", "remote")

    def test_local_default_candidate_scoring(self) -> None:
        ctrl = Controller()
        sinks = [
            Sink(id=1, name="alsa_output.pci.HiFi__HDMI3__sink"),
            Sink(id=2, name="alsa_output.pci.HiFi__Speaker__sink"),
            Sink(id=3, name="auto_null"),
        ]
        with patch.object(ctrl.backend, "sinks", return_value=sinks):
            candidate = ctrl._local_default_candidate()
            self.assertEqual(candidate, "alsa_output.pci.HiFi__Speaker__sink")

    def test_enforce_routed_apps(self) -> None:
        ctrl = Controller()
        ctrl.state.connected_sink = "soundhop-remote"
        ctrl.state.routed_apps = {"spotify": "remote", "firefox": "local"}
        with (
            patch.object(ctrl, "_local_default_candidate", return_value="local-speaker"),
            patch.object(ctrl.backend, "move_app_streams", return_value=1) as mock_move,
        ):
            moved = ctrl.enforce_routed_apps()
            self.assertEqual(moved, 2)
            mock_move.assert_any_call("spotify", "soundhop-remote")
            mock_move.assert_any_call("firefox", "local-speaker")


if __name__ == "__main__":
    unittest.main()
