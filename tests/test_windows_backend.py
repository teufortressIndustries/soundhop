"""Tests for WindowsAudioBackend lifecycle, listener, and tunnel management."""

import unittest
from unittest.mock import MagicMock, patch

from soundhop.platform.windows.backend import WindowsAudioBackend


class WindowsBackendTest(unittest.TestCase):
    def test_backend_info_and_sinks(self) -> None:
        backend = WindowsAudioBackend()
        self.assertTrue(backend.available())
        self.assertFalse(backend.is_pipewire())
        self.assertIn("Windows Audio", backend.server_name())

        sinks = backend.sinks()
        self.assertGreaterEqual(len(sinks), 1)
        self.assertTrue(backend.sink_exists(sinks[0].name))

    def test_listener_lifecycle(self) -> None:
        backend = WindowsAudioBackend()
        try:
            # Mocking socket so we don't bind on a system port during tests
            with patch("socket.socket") as mock_sock_cls:
                mock_sock = MagicMock()
                mock_sock_cls.return_value = mock_sock

                listener = backend.ensure_tcp_listener(port=4713, security="trusted-lan")
                self.assertEqual(listener.port, 4713)
                self.assertEqual(listener.security, "trusted-lan")
                self.assertTrue(listener.created)

                # Re-ensure returns existing
                listener2 = backend.ensure_tcp_listener(port=4713, security="trusted-lan")
                self.assertFalse(listener2.created)

                # Check modules
                mods = backend.modules()
                self.assertEqual(len(mods), 1)
                self.assertEqual(mods[0].name, "module-native-protocol-tcp")

                # Remove listener
                removed = backend.remove_listener(listener.module_id)
                self.assertTrue(removed)
                self.assertEqual(len(backend.modules()), 0)
        finally:
            backend._stop_listener_locked()

    def test_connect_and_disconnect_tunnel(self) -> None:
        backend = WindowsAudioBackend()
        with patch.object(backend, "load_module", return_value=123):
            with patch("soundhop.platform.windows.backend._ActiveTunnelSession.start"):
                mod_id = backend.connect_tunnel(
                    host="192.168.1.100",
                    port=4713,
                    sink_name="soundhop_remote",
                    description="remote-device",
                )
                self.assertEqual(mod_id, 123)

                # Check active sender
                self.assertIn("soundhop_remote", backend._active_senders)

                # Active sender must be reflected in sinks() and sink_exists()
                self.assertTrue(backend.sink_exists("soundhop_remote"))
                tunnel_sink = backend.sink("soundhop_remote")
                self.assertIsNotNone(tunnel_sink)
                assert tunnel_sink is not None
                self.assertEqual(tunnel_sink.name, "soundhop_remote")
                self.assertEqual(tunnel_sink.driver, "module-tunnel-sink")

                # Test default sink switching
                backend.set_default_sink("soundhop_remote")
                self.assertEqual(backend.default_sink(), "soundhop_remote")

                # Disconnect via unload_module
                unloaded = backend.unload_module(mod_id)
                self.assertTrue(unloaded)
                self.assertNotIn("soundhop_remote", backend._active_senders)
                self.assertFalse(backend.sink_exists("soundhop_remote"))

    def test_unload_tunnels(self) -> None:
        backend = WindowsAudioBackend()
        with patch.object(backend, "load_module", side_effect=[201, 202]):
            with patch("soundhop.platform.windows.backend._ActiveTunnelSession.start"):
                backend.connect_tunnel(
                    host="1.1.1.1", port=4713, sink_name="soundhop_node1", description="node1"
                )
                backend.connect_tunnel(
                    host="1.1.1.2", port=4713, sink_name="soundhop_node2", description="node2"
                )
                self.assertEqual(len(backend._active_senders), 2)

                removed = backend.unload_tunnels()
                self.assertEqual(sorted(removed), [201, 202])
                self.assertEqual(len(backend._active_senders), 0)

    def test_backend_subclass_and_dispatch(self) -> None:
        import sys

        from soundhop.backend import AudioBackend

        self.assertTrue(issubclass(WindowsAudioBackend, AudioBackend))

        backend = WindowsAudioBackend()
        self.assertIsInstance(backend, AudioBackend)

        # Test AudioBackend __new__ factory on win32
        with patch.object(sys, "platform", "win32"):
            auto_backend = AudioBackend()
            self.assertIsInstance(auto_backend, WindowsAudioBackend)

            # When runner is provided, it falls back to AudioBackend
            fake_runner = MagicMock()
            linux_backend = AudioBackend(runner=fake_runner)
            self.assertIsInstance(linux_backend, AudioBackend)
            self.assertNotIsInstance(linux_backend, WindowsAudioBackend)

    def test_active_tunnel_handshake_flow(self) -> None:
        import io

        from soundhop.platform.windows.backend import _ActiveTunnelSession
        from soundhop.platform.windows.protocol import (
            PA_COMMAND_AUTH,
            PA_COMMAND_CREATE_PLAYBACK_STREAM,
            PA_COMMAND_SET_CLIENT_NAME,
            TagStructReader,
            make_reply_auth,
            make_reply_create_playback_stream,
            make_reply_ok,
            read_packet,
        )

        session = _ActiveTunnelSession(
            host="127.0.0.1",
            port=4713,
            sink_name="soundhop_test",
            module_id=999,
        )

        sent_packets = []

        class FakeSocket:
            def __init__(self):
                self.sent_bytes = bytearray()
                self._stream = io.BytesIO()

            def sendall(self, data: bytes):
                self.sent_bytes.extend(data)
                pkt = read_packet(io.BytesIO(data))
                if pkt and pkt.is_control:
                    sent_packets.append(pkt)
                    reader = TagStructReader(pkt.data)
                    cmd = reader.read_u32()
                    tag = reader.read_u32()
                    if cmd == PA_COMMAND_AUTH:
                        ver = reader.read_u32()
                        self._write_reply(make_reply_auth(tag, ver))
                    elif cmd == PA_COMMAND_SET_CLIENT_NAME:
                        self._write_reply(make_reply_ok(tag))
                    elif cmd == PA_COMMAND_CREATE_PLAYBACK_STREAM:
                        self._write_reply(
                            make_reply_create_playback_stream(tag, stream_index=1, channel=3)
                        )

            def _write_reply(self, reply_bytes: bytes):
                pos = self._stream.tell()
                self._stream.seek(0, io.SEEK_END)
                self._stream.write(reply_bytes)
                self._stream.seek(pos)

            def makefile(self, mode: str, buffering: int = 0):
                return self._stream

            def close(self):
                pass

            def recv(self, bufsize: int, flags: int = 0):
                return b""

        fake_sock = FakeSocket()
        with patch("socket.create_connection", return_value=fake_sock):
            with patch("soundhop.platform.windows.backend.WindowsLoopbackRecorder") as mock_rec_cls:
                mock_rec = MagicMock()
                mock_rec.is_alive.return_value = True
                mock_rec_cls.return_value = mock_rec

                # Run sender loop once
                def stop_after_delay():
                    session._stop.set()

                import threading

                threading.Timer(0.1, stop_after_delay).start()
                session._run_sender()

                # Verify sent packets: Auth, Client Name, Create Playback Stream
                self.assertGreaterEqual(len(sent_packets), 3)

                # Auth packet must have PA_COMMAND_AUTH and version
                auth_reader = TagStructReader(sent_packets[0].data)
                self.assertEqual(auth_reader.read_u32(), PA_COMMAND_AUTH)
                self.assertEqual(auth_reader.read_u32(), 0)  # tag 0
                self.assertEqual(auth_reader.read_u32(), 35)  # protocol version

                # Playback stream creation must target remote default sink (empty/None)
                stream_reader = TagStructReader(sent_packets[2].data)
                self.assertEqual(stream_reader.read_u32(), PA_COMMAND_CREATE_PLAYBACK_STREAM)
                self.assertEqual(stream_reader.read_u32(), 2)  # tag 2
                stream_reader.read_sample_spec()
                stream_reader.read_channel_map()
                stream_reader.read_u32()  # sink index
                sink_target = stream_reader.read_string()
                self.assertIn(sink_target, ("", None))

                # Handshake succeeded and stream channel set
                self.assertEqual(session._stream_channel, 3)
                mock_rec.start.assert_called_once()

    def test_tunnel_reconnect_on_remote_disconnect(self) -> None:
        import socket

        from soundhop.platform.windows.backend import _ActiveTunnelSession

        session = _ActiveTunnelSession(
            host="127.0.0.1",
            port=4713,
            sink_name="soundhop_recon",
            module_id=888,
            reconnect_delay_ms=100,
        )

        attempts = 0

        def fake_create_conn(*args, **kwargs):
            nonlocal attempts
            attempts += 1
            if attempts == 1:
                # First attempt connects then immediately triggers EOF or error
                s1, s2 = socket.socketpair()
                s2.close()
                return s1
            else:
                session._stop.set()
                raise OSError("Connection failed")

        with patch("socket.create_connection", side_effect=fake_create_conn):
            session._run_sender()
            self.assertGreaterEqual(attempts, 2)

    def test_default_sink_reset_on_unload(self) -> None:
        backend = WindowsAudioBackend()
        backend.load_module("module-tunnel-sink", ["sink_name=soundhop_test"])
        session = MagicMock()
        session.module_id = 101
        session.is_alive.return_value = True
        backend._active_senders["soundhop_test"] = session

        backend.set_default_sink("soundhop_test")
        self.assertEqual(backend.default_sink(), "soundhop_test")

        backend.unload_module(101)
        self.assertNotEqual(backend.default_sink(), "soundhop_test")


if __name__ == "__main__":
    unittest.main()
