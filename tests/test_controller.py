"""Tests for the high-level Controller."""

import tempfile
import unittest
from pathlib import Path

from unittest.mock import patch

from soundhop.backend import AudioBackend
from soundhop.config import Config
from soundhop.controller import Controller
from soundhop.discovery import Peer
from soundhop.state import State
from soundhop.util import Result


class ControllerTest(unittest.TestCase):
    @patch("soundhop.controller.tcp_reachable", return_value=True)
    def test_controller_lifecycle(self, mock_tcp) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            cfg_path = Path(tmpdir) / "config.toml"
            state_path = Path(tmpdir) / "state.json"

            config = Config(cfg_path)
            config.device.name = "MyMachine"
            config.sharing.port = 4713
            config.save()

            loaded_modules = {}
            current_default_sink = ["alsa_output.pci"]

            def fake_runner(cmd, **kwargs):
                cmd_str = " ".join(cmd)
                if "pactl info" in cmd_str:
                    return Result(
                        args=cmd,
                        stdout=f"Server Name: PulseAudio (on PipeWire 1.2)\nDefault Sink: {current_default_sink[0]}\n",
                        stderr="",
                        returncode=0,
                    )
                elif "pactl list short sinks" in cmd_str:
                    lines = ["1\talsa_output.pci\tPipeWire\ts32le 2ch 48000Hz\tRUNNING"]
                    for mod_id, (mname, margs) in loaded_modules.items():
                        if "module-tunnel-sink" in mname:
                            import re
                            m = re.search(r"sink_name=([^\s]+)", margs)
                            sname = m.group(1) if m else "soundhop_remote"
                            lines.append(f"100\t{sname}\tPipeWire\ts16le 2ch 44100Hz\tIDLE")
                    return Result(args=cmd, stdout="\n".join(lines) + "\n", stderr="", returncode=0)
                elif "pactl list short modules" in cmd_str:
                    lines = [f"{mid}\t{mname}\t{margs}" for mid, (mname, margs) in loaded_modules.items()]
                    return Result(args=cmd, stdout="\n".join(lines) + "\n", stderr="", returncode=0)
                elif "pactl list short sink-inputs" in cmd_str:
                    return Result(args=cmd, stdout="", stderr="", returncode=0)
                elif "pactl load-module" in cmd_str:
                    mod_id = len(loaded_modules) + 10
                    mod_name = cmd[2]
                    mod_args = " ".join(cmd[3:])
                    loaded_modules[mod_id] = (mod_name, mod_args)
                    return Result(args=cmd, stdout=f"{mod_id}\n", stderr="", returncode=0)
                elif "pactl unload-module" in cmd_str:
                    mod_id = int(cmd[2])
                    loaded_modules.pop(mod_id, None)
                    return Result(args=cmd, stdout="", stderr="", returncode=0)
                elif "pactl set-default-sink" in cmd_str:
                    current_default_sink[0] = cmd[2]
                    return Result(args=cmd, stdout="", stderr="", returncode=0)
                return Result(args=cmd, stdout="", stderr="", returncode=0)

            backend = AudioBackend(runner=fake_runner, sleep=lambda _: None)
            controller = Controller(
                config=config,
                state_path=state_path,
                backend=backend,
                notify_enabled=False,
            )

            # Test share
            share_res = controller.share(True)
            self.assertTrue(share_res["enabled"])
            self.assertEqual(len(loaded_modules), 1)

            # Test connect
            remote_peer = Peer(
                id="remote-1",
                name="Living Room",
                host="192.168.1.99",
                port=4713,
            )
            conn_res = controller.connect(remote_peer)
            self.assertTrue(conn_res["connected"])
            self.assertTrue(conn_res["sink"].startswith("soundhop_"))
            self.assertEqual(current_default_sink[0], conn_res["sink"])
            self.assertTrue(controller.state.connected)

            # Test status
            st = controller.status([remote_peer])
            self.assertTrue(st["connection"]["connected"])
            self.assertEqual(st["connection"]["name"], "Living Room")

            # Test disconnect
            disc_res = controller.disconnect(restore=True)
            self.assertTrue(disc_res["disconnected"])
            self.assertFalse(controller.state.connected)
            self.assertEqual(current_default_sink[0], "alsa_output.pci")


if __name__ == "__main__":
    unittest.main()
