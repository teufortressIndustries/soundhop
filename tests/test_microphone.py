import unittest
from unittest.mock import patch

from soundhop.backend import AudioBackend, Module, Source
from soundhop.controller import Controller
from soundhop.util import Result


class MicrophoneTest(unittest.TestCase):
    def test_source_model(self) -> None:
        src = Source(id=3, name="alsa_input.pci-00", driver="PipeWire", spec="s16le 2ch 48000Hz", state="RUNNING")
        self.assertEqual(src.id, 3)
        self.assertEqual(src.name, "alsa_input.pci-00")
        self.assertEqual(src.description, "alsa_input.pci-00")

    def test_backend_sources_listing(self) -> None:
        pactl_output = "1\talsa_input.pci\tPipeWire\ts16le 2ch 48000Hz\tRUNNING\n2\tsoundhop_source_peer\tmodule-tunnel-source\ts16le 2ch 48000Hz\tIDLE\n"
        backend = AudioBackend(runner=lambda args, **kwargs: Result(args, 0, pactl_output, ""))
        sources = backend.sources()
        self.assertEqual(len(sources), 2)
        self.assertEqual(sources[0].name, "alsa_input.pci")
        self.assertEqual(sources[1].name, "soundhop_source_peer")
        self.assertTrue(backend.source_exists("soundhop_source_peer"))
        self.assertFalse(backend.source_exists("nonexistent"))

    def test_connect_and_disconnect_tunnel_source(self) -> None:
        loaded_modules = []

        def mock_runner(args, **kwargs):
            if "load-module" in args:
                loaded_modules.append(args)
                return Result(args, 0, "77\n", "")
            if "unload-module" in args:
                return Result(args, 0, "", "")
            return Result(args, 0, "", "")

        backend = AudioBackend(runner=mock_runner, sleep=lambda s: None)
        with (
            patch.object(backend, "source", return_value=None),
            patch.object(backend, "source_exists", side_effect=[False, True]),
            patch.object(
                backend,
                "module_for_source",
                return_value=Module(id=77, name="module-tunnel-source", args="source_name=soundhop_source_test"),
            ),
        ):
            mod_id = backend.connect_tunnel_source(
                host="192.168.1.50",
                port=4713,
                source_name="soundhop_source_test",
                description="TestPeer",
            )
            self.assertEqual(mod_id, 77)
            self.assertEqual(len(loaded_modules), 1)
            self.assertIn("module-tunnel-source", loaded_modules[0])

            unloaded = backend.disconnect_tunnel_source("soundhop_source_test")
            self.assertTrue(unloaded)

    def test_controller_mic_lifecycle_receive(self) -> None:
        from soundhop.state import State

        backend = AudioBackend(runner=lambda args, **kwargs: Result(args, 0, "", ""))
        ctrl = Controller(backend=backend)
        ctrl.state = State()
        ctrl.save = lambda: None  # type: ignore[method-assign]
        ctrl.state.connected_peer = "peer1"
        ctrl.state.connected_name = "Desktop"
        ctrl.state.connected_host = "192.168.1.100"
        ctrl.state.connected_port = 4713
        ctrl.state.connected_sink = "soundhop_sink"

        with (
            patch.object(ctrl.backend, "connect_tunnel_source", return_value=88),
            patch.object(ctrl.backend, "disconnect_tunnel_source", return_value=True),
            patch.object(ctrl.backend, "unload_module", return_value=True),
            patch.object(ctrl.backend, "set_default_source") as mock_set_source,
            patch.object(ctrl.backend, "move_source_outputs") as mock_move_outputs,
            patch.object(ctrl, "_local_default_source_candidate", return_value="alsa_input.mic1"),
        ):
            # Enable receive mic (remote PC mic -> local device)
            res = ctrl.enable_mic(direction="receive")
            self.assertTrue(res["enabled"])
            self.assertEqual(res["direction"], "receive")
            self.assertTrue(ctrl.state.connected_source.startswith("soundhop_source_"))
            self.assertEqual(ctrl.state.connected_source_module, 88)
            mock_set_source.assert_called_with(ctrl.state.connected_source)
            mock_move_outputs.assert_called_with(ctrl.state.connected_source)

            # Disable mic
            res = ctrl.disable_mic()
            self.assertFalse(res["enabled"])
            self.assertEqual(ctrl.state.connected_source, "")
            self.assertEqual(ctrl.state.connected_source_module, 0)
            mock_set_source.assert_called_with("alsa_input.mic1")
            mock_move_outputs.assert_called_with("alsa_input.mic1", source=mock_set_source.call_args_list[0][0][0])

    def test_controller_mic_lifecycle_send(self) -> None:
        from soundhop.state import State

        backend = AudioBackend(runner=lambda args, **kwargs: Result(args, 0, "", ""))
        ctrl = Controller(backend=backend)
        ctrl.state = State()
        ctrl.save = lambda: None  # type: ignore[method-assign]
        ctrl.state.connected_peer = "peer1"
        ctrl.state.connected_name = "Desktop"
        ctrl.state.connected_host = "192.168.1.100"
        ctrl.state.connected_port = 4713
        ctrl.state.connected_sink = "soundhop_sink"

        with (
            patch.object(ctrl.backend, "send_mic_to_remote", return_value=(101, 102, 103, 104, "soundhop_rem_mic_Laptop")) as mock_send,
            patch.object(ctrl.backend, "stop_send_mic_to_remote", return_value=True) as mock_stop_send,
        ):
            # Enable send mic (local laptop mic -> remote PC)
            res = ctrl.enable_mic(direction="send")
            self.assertTrue(res["enabled"])
            self.assertEqual(res["direction"], "send")
            self.assertEqual(ctrl.state.mic_direction, "send")
            self.assertEqual(ctrl.state.remote_mic_null_module, 101)
            self.assertEqual(ctrl.state.remote_mic_remap_module, 102)
            self.assertEqual(ctrl.state.connected_source_module, 103)
            self.assertEqual(ctrl.state.local_mic_loopback_module, 104)
            self.assertEqual(ctrl.state.connected_source, "soundhop_rem_mic_Laptop")
            mock_send.assert_called_once()

            # Disable mic
            res = ctrl.disable_mic()
            self.assertFalse(res["enabled"])
            self.assertEqual(ctrl.state.connected_source, "")
            self.assertEqual(ctrl.state.mic_direction, "")
            mock_stop_send.assert_called_once_with(
                host="192.168.1.100",
                port=4713,
                remote_null_id=101,
                remote_remap_id=102,
                local_tunnel_id=103,
                local_loopback_id=104,
            )

    def test_local_default_source_candidate_scoring(self) -> None:
        backend = AudioBackend(runner=lambda args, **kwargs: Result(args, 0, "", ""))
        ctrl = Controller(backend=backend)
        sources = [
            Source(id=1, name="alsa_output.pci.monitor"),
            Source(id=2, name="alsa_input.pci.HiFi__Mic1__source"),
            Source(id=3, name="auto_null"),
        ]
        with patch.object(ctrl.backend, "sources", return_value=sources):
            candidate = ctrl._local_default_source_candidate()
            self.assertEqual(candidate, "alsa_input.pci.HiFi__Mic1__source")

    def test_cleanup_orphaned_remote_mic_modules(self) -> None:
        from soundhop.backend import Sink, SinkInput

        backend = AudioBackend(runner=lambda args, **kwargs: Result(args, 0, "", ""))
        mods = [
            Module(id=301, name="module-null-sink", args="sink_name=soundhop_rem_mic_sink_laptop"),
            Module(
                id=302,
                name="module-remap-source",
                args="source_name=soundhop_rem_mic_laptop master=soundhop_rem_mic_sink_laptop.monitor",
            ),
            Module(id=401, name="module-null-sink", args="sink_name=soundhop_rem_mic_sink_active"),
            Module(
                id=402,
                name="module-remap-source",
                args="source_name=soundhop_rem_mic_active master=soundhop_rem_mic_sink_active.monitor",
            ),
        ]
        sinks = [
            Sink(id=10, name="soundhop_rem_mic_sink_laptop"),
            Sink(id=20, name="soundhop_rem_mic_sink_active"),
        ]
        # Only sink 20 has an active sink-input; sink 10 is orphaned
        inputs = [SinkInput(id=1, sink=20)]
        unloaded: list[int] = []

        with (
            patch.object(backend, "modules", return_value=mods),
            patch.object(backend, "sinks", return_value=sinks),
            patch.object(backend, "sink_inputs", return_value=inputs),
            patch.object(
                backend,
                "unload_module",
                side_effect=lambda mid, **kw: unloaded.append(mid) or True,
            ),
        ):
            removed = backend.cleanup_orphaned_remote_mic_modules()
            self.assertEqual(sorted(removed), [301, 302])
            self.assertEqual(sorted(unloaded), [301, 302])


if __name__ == "__main__":
    unittest.main()
