import unittest
from unittest.mock import MagicMock, patch

from soundhop.backend import AudioBackend
from soundhop.controller import Controller
from soundhop.platform.windows.protocol import (
    PA_COMMAND_SET_SINK_MUTE,
    PA_COMMAND_SET_SINK_VOLUME,
    TagStructReader,
    make_command_set_sink_mute,
    make_command_set_sink_volume,
    read_packet,
)
from soundhop.util import Result


class VolumeSyncTest(unittest.TestCase):
    def test_protocol_volume_packets(self) -> None:
        pkt_bytes = make_command_set_sink_volume(tag=42, volume=32768, sink_name="test-sink")
        import io

        pkt = read_packet(io.BytesIO(pkt_bytes))
        self.assertIsNotNone(pkt)
        reader = TagStructReader(pkt.data)
        cmd = reader.read_u32()
        tag = reader.read_u32()
        sink_idx = reader.read_u32()
        sink_name = reader.read_string()
        cv_channels, cv_vols = reader.read_cvolume()

        self.assertEqual(cmd, PA_COMMAND_SET_SINK_VOLUME)
        self.assertEqual(tag, 42)
        self.assertEqual(sink_idx, 0xFFFFFFFF)
        self.assertEqual(sink_name, "test-sink")
        self.assertEqual(cv_channels, 2)
        self.assertEqual(cv_vols[0], 32768)

    def test_protocol_mute_packets(self) -> None:
        pkt_bytes = make_command_set_sink_mute(tag=43, mute=True, sink_name="test-sink")
        import io

        pkt = read_packet(io.BytesIO(pkt_bytes))
        self.assertIsNotNone(pkt)
        reader = TagStructReader(pkt.data)
        cmd = reader.read_u32()
        tag = reader.read_u32()
        sink_idx = reader.read_u32()
        sink_name = reader.read_string()
        mute = reader.read_boolean()

        self.assertEqual(cmd, PA_COMMAND_SET_SINK_MUTE)
        self.assertEqual(tag, 43)
        self.assertEqual(sink_name, "test-sink")
        self.assertTrue(mute)

    def test_backend_volume_methods(self) -> None:
        calls = []

        def mock_pactl(*args, **kwargs):
            calls.append(args)
            if "get-sink-volume" in args:
                return Result(args, 0, "Volume: front-left: 52428 /  80% / -5.74 dB\n", "")
            if "get-sink-mute" in args:
                return Result(args, 0, "Mute: no\n", "")
            return Result(args, 0, "", "")

        backend = AudioBackend()
        backend._pactl = mock_pactl

        vol = backend.get_sink_volume("alsa_output.sink")
        self.assertEqual(vol, 80)

        mute = backend.get_sink_mute("alsa_output.sink")
        self.assertFalse(mute)

        backend.set_sink_volume("alsa_output.sink", 90)
        self.assertIn(("set-sink-volume", "alsa_output.sink", "90%"), calls)

        backend.set_sink_mute("alsa_output.sink", True)
        self.assertIn(("set-sink-mute", "alsa_output.sink", "1"), calls)

        backend.remote_set_volume("192.168.1.10", 4713, 65, mute=False)
        self.assertIn(("-s", "tcp:192.168.1.10:4713", "set-sink-volume", "@DEFAULT_SINK@", "65%"), calls)
        self.assertIn(("-s", "tcp:192.168.1.10:4713", "set-sink-mute", "@DEFAULT_SINK@", "0"), calls)

    def test_controller_volume_sync(self) -> None:
        ctrl = Controller()
        ctrl.state.connected_peer = "test-peer"
        ctrl.state.connected_sink = "soundhop_sink"
        ctrl.state.connected_host = "192.168.1.10"
        ctrl.state.connected_port = 4713

        with (
            patch.object(ctrl.backend, "get_sink_volume", return_value=85),
            patch.object(ctrl.backend, "get_sink_mute", return_value=False),
            patch.object(ctrl.backend, "set_sink_volume") as mock_set_vol,
            patch.object(ctrl.backend, "remote_set_volume") as mock_remote_set,
        ):
            res = ctrl.set_volume(volume=70, sync_remote=True)
            self.assertEqual(res["volume"], 85)
            mock_set_vol.assert_called_once_with("soundhop_sink", 70)
            mock_remote_set.assert_called_once_with("192.168.1.10", 4713, 70, mute=False)

    def test_sync_remote_volume_dedup(self) -> None:
        ctrl = Controller()
        ctrl.state.connected_peer = "test-peer"
        ctrl.state.connected_sink = "soundhop_sink"
        ctrl.state.connected_host = "192.168.1.10"
        ctrl.state.connected_port = 4713
        ctrl.state.last_synced_volume = 80
        ctrl.state.last_synced_mute = False

        with patch.object(ctrl.backend, "remote_set_volume") as mock_remote:
            # Same volume -> should not call remote
            synced = ctrl.sync_remote_volume(volume=80, mute=False)
            self.assertFalse(synced)
            mock_remote.assert_not_called()

            # New volume -> should call remote and update state
            synced = ctrl.sync_remote_volume(volume=85, mute=False)
            self.assertTrue(synced)
            mock_remote.assert_called_once_with("192.168.1.10", 4713, 85, mute=False)
            self.assertEqual(ctrl.state.last_synced_volume, 85)


if __name__ == "__main__":
    unittest.main()
