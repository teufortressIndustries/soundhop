"""Tests for AudioBackend parsing and pactl interactions."""

import unittest

from soundhop.backend import AudioBackend, Module, listener_security
from soundhop.util import Result


class BackendTest(unittest.TestCase):
    def test_module_arg_parser(self) -> None:
        m = Module(
            id=42,
            name="module-native-protocol-tcp",
            args="port=4713 auth-anonymous=1 auth-ip-acl=127.0.0.1;192.168.0.0/16",
        )
        self.assertEqual(m.arg("port"), "4713")
        self.assertEqual(m.arg("auth-anonymous"), "1")
        self.assertEqual(m.arg("missing", "default"), "default")
        self.assertEqual(m.arg("auth-ip-acl"), "127.0.0.1;192.168.0.0/16")

    def test_listener_security_detection(self) -> None:
        self.assertEqual(
            listener_security("port=4713 auth-anonymous=1 auth-ip-acl=127.0.0.1;192.168.0.0/16"),
            "trusted-lan",
        )
        self.assertEqual(
            listener_security("port=4713 auth-anonymous=1"),
            "open",
        )
        self.assertEqual(
            listener_security("port=4713"),
            "cookie",
        )

    def test_backend_parsing(self) -> None:
        def fake_runner(cmd, **kwargs):
            cmd_str = " ".join(cmd)
            if "pactl info" in cmd_str:
                return Result(
                    args=cmd,
                    stdout=(
                        "Server String: /run/user/1000/pulse/native\n"
                        "Server Name: PulseAudio (on PipeWire 1.2.6)\n"
                        "Default Sink: alsa_output.pci-00_1f.3.analog-stereo\n"
                    ),
                    stderr="",
                    returncode=0,
                )
            elif "pactl list short sinks" in cmd_str:
                return Result(
                    args=cmd,
                    stdout=(
                        "52\talsa_output.pci-00_1f.3.analog-stereo\tPipeWire\ts32le 2ch 48000Hz\tRUNNING\n"
                        "60\tsoundhop_cachyos\tPipeWire\ts16le 2ch 44100Hz\tIDLE\n"
                    ),
                    stderr="",
                    returncode=0,
                )
            elif "pactl list short modules" in cmd_str:
                return Result(
                    args=cmd,
                    stdout=(
                        "10\tmodule-native-protocol-tcp\tport=4713 auth-anonymous=1\n"
                        "15\tmodule-tunnel-sink\tserver=tcp:192.168.1.10:4713 sink_name=soundhop_cachyos\n"
                    ),
                    stderr="",
                    returncode=0,
                )
            return Result(args=cmd, stdout="", stderr="", returncode=0)

        backend = AudioBackend(runner=fake_runner)
        self.assertTrue(backend.is_pipewire())
        self.assertEqual(backend.default_sink(), "alsa_output.pci-00_1f.3.analog-stereo")

        sinks = backend.sinks()
        self.assertEqual(len(sinks), 2)
        self.assertEqual(sinks[0].name, "alsa_output.pci-00_1f.3.analog-stereo")
        self.assertEqual(sinks[1].name, "soundhop_cachyos")

        modules = backend.modules()
        self.assertEqual(len(modules), 2)
        self.assertEqual(modules[0].name, "module-native-protocol-tcp")
        self.assertEqual(modules[1].name, "module-tunnel-sink")

        soundhop_mods = backend.soundhop_modules()
        self.assertEqual(len(soundhop_mods), 1)
        self.assertEqual(soundhop_mods[0].id, 15)


if __name__ == "__main__":
    unittest.main()
