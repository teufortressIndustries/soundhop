"""Tests for PulseAudio native protocol wire framing and serialization."""

import io
import unittest

from soundhop.platform.windows.protocol import (
    PA_COMMAND_AUTH,
    PA_COMMAND_CREATE_PLAYBACK_STREAM,
    PA_COMMAND_GET_SERVER_INFO,
    PA_COMMAND_REPLY,
    PA_COMMAND_SET_CLIENT_NAME,
    PA_CONTROL_CHANNEL,
    PA_SAMPLE_S16LE,
    Packet,
    SampleSpec,
    TagStructReader,
    TagStructWriter,
    make_command_auth,
    make_command_create_playback_stream,
    make_command_set_client_name,
    make_reply_auth,
    make_reply_create_playback_stream,
    make_reply_ok,
    make_reply_server_info,
    read_packet,
    write_packet,
)


class WindowsProtocolTest(unittest.TestCase):
    def test_packet_framing(self) -> None:
        data = b"Hello SoundHop"
        pkt_bytes = write_packet(channel=123, data=data)
        self.assertEqual(len(pkt_bytes), 8 + len(data))

        stream = io.BytesIO(pkt_bytes)
        pkt = read_packet(stream)
        self.assertIsNotNone(pkt)
        assert pkt is not None
        self.assertEqual(pkt.channel, 123)
        self.assertEqual(pkt.data, data)
        self.assertFalse(pkt.is_control)

    def test_control_packet_tagstruct_primitive_roundtrip(self) -> None:
        writer = TagStructWriter()
        writer.write_u32(0xDEADBEEF)
        writer.write_u64(0x0123456789ABCDEF)
        writer.write_string("SoundHop PC")
        writer.write_string(None)
        writer.write_arbitrary(b"\x01\x02\x03\x04")
        writer.write_sample_spec(SampleSpec(format=PA_SAMPLE_S16LE, channels=2, rate=48000))
        writer.write_channel_map([1, 2])
        writer.write_boolean(True)
        writer.write_boolean(False)

        encoded = writer.getvalue()
        reader = TagStructReader(encoded)

        self.assertEqual(reader.read_u32(), 0xDEADBEEF)
        self.assertEqual(reader.read_u64(), 0x0123456789ABCDEF)
        self.assertEqual(reader.read_string(), "SoundHop PC")
        self.assertIsNone(reader.read_string())
        self.assertEqual(reader.read_arbitrary(), b"\x01\x02\x03\x04")

        spec = reader.read_sample_spec()
        self.assertEqual(spec.format, PA_SAMPLE_S16LE)
        self.assertEqual(spec.channels, 2)
        self.assertEqual(spec.rate, 48000)

        ch_map = reader.read_channel_map()
        self.assertEqual(ch_map, [1, 2])

    def test_make_reply_auth(self) -> None:
        pkt_bytes = make_reply_auth(tag=42, protocol_version=35)
        pkt = read_packet(io.BytesIO(pkt_bytes))
        self.assertIsNotNone(pkt)
        assert pkt is not None
        self.assertTrue(pkt.is_control)

        reader = TagStructReader(pkt.data)
        cmd = reader.read_u32()
        tag = reader.read_u32()
        ver = reader.read_u32()

        self.assertEqual(cmd, PA_COMMAND_REPLY)
        self.assertEqual(tag, 42)
        self.assertEqual(ver, 35)

    def test_make_command_and_reply_playback_stream(self) -> None:
        # Command
        cmd_bytes = make_command_create_playback_stream(tag=7, sink_name="TestSink")
        pkt = read_packet(io.BytesIO(cmd_bytes))
        self.assertIsNotNone(pkt)
        assert pkt is not None
        self.assertTrue(pkt.is_control)

        reader = TagStructReader(pkt.data)
        self.assertEqual(reader.read_u32(), PA_COMMAND_CREATE_PLAYBACK_STREAM)
        self.assertEqual(reader.read_u32(), 7)

        # Reply
        rep_bytes = make_reply_create_playback_stream(tag=7, stream_index=0, channel=0)
        rep_pkt = read_packet(io.BytesIO(rep_bytes))
        self.assertIsNotNone(rep_pkt)
        assert rep_pkt is not None
        rep_reader = TagStructReader(rep_pkt.data)
        self.assertEqual(rep_reader.read_u32(), PA_COMMAND_REPLY)
        self.assertEqual(rep_reader.read_u32(), 7)
        self.assertEqual(rep_reader.read_u32(), 0)  # stream_index
        self.assertEqual(rep_reader.read_u32(), 0)  # channel

    def test_make_command_auth(self) -> None:
        cookie = b"\x01" * 32
        # Standard call: version and cookie
        pkt_bytes = make_command_auth(tag=10, protocol_version_or_cookie=35, cookie=cookie)
        pkt = read_packet(io.BytesIO(pkt_bytes))
        self.assertIsNotNone(pkt)
        assert pkt is not None
        reader = TagStructReader(pkt.data)
        self.assertEqual(reader.read_u32(), PA_COMMAND_AUTH)
        self.assertEqual(reader.read_u32(), 10)
        self.assertEqual(reader.read_u32(), 35)
        self.assertEqual(reader.read_arbitrary(), cookie)

        # Backward compatibility call: cookie passed directly as second argument
        pkt_bytes_compat = make_command_auth(tag=11, protocol_version_or_cookie=cookie)
        pkt_compat = read_packet(io.BytesIO(pkt_bytes_compat))
        self.assertIsNotNone(pkt_compat)
        assert pkt_compat is not None
        reader_compat = TagStructReader(pkt_compat.data)
        self.assertEqual(reader_compat.read_u32(), PA_COMMAND_AUTH)
        self.assertEqual(reader_compat.read_u32(), 11)
        self.assertEqual(reader_compat.read_u32(), 35)
        self.assertEqual(reader_compat.read_arbitrary(), cookie)

    def test_proplist_null_termination_and_roundtrip(self) -> None:
        writer = TagStructWriter()
        writer.write_proplist({"application.name": "SoundHop-Client", "media.role": "music"})
        data = writer.getvalue()

        reader = TagStructReader(data)
        props = reader.read_proplist()
        self.assertIn("application.name", props)
        self.assertIn("media.role", props)
        # Verify value ends with \0 for PulseAudio / PipeWire proplist string spec
        self.assertTrue(props["application.name"].endswith(b"\0"))
        self.assertEqual(props["application.name"].rstrip(b"\0"), b"SoundHop-Client")

    def test_skip_value_and_volume(self) -> None:
        from soundhop.platform.windows.protocol import PA_TAG_VOLUME
        writer = TagStructWriter()
        # Write PA_TAG_VOLUME manually
        writer.stream.write(bytes([PA_TAG_VOLUME]))
        writer.stream.write((65536).to_bytes(4, "big"))
        # Followed by a u32
        writer.write_u32(12345)

        reader = TagStructReader(writer.getvalue())
        reader.skip_value()  # should skip the 4-byte volume tag
        self.assertEqual(reader.read_u32(), 12345)

    def test_skip_value_timeval(self) -> None:
        from soundhop.platform.windows.protocol import PA_TAG_TIMEVAL
        writer = TagStructWriter()
        self.assertEqual(PA_TAG_TIMEVAL, ord("e"))
        writer.stream.write(bytes([PA_TAG_TIMEVAL]))
        writer.stream.write((1000).to_bytes(4, "big"))  # tv_sec
        writer.stream.write((2000).to_bytes(4, "big"))  # tv_usec
        writer.write_u32(999)

        reader = TagStructReader(writer.getvalue())
        reader.skip_value()
        self.assertEqual(reader.read_u32(), 999)

    def test_read_packet_fragmented_stream(self) -> None:
        import struct

        data = b"fragmented audio chunk payload"
        header = struct.pack(">II", 42, len(data))
        full = header + data

        class FragmentedStream:
            def __init__(self, raw: bytes, chunk_size: int = 3):
                self.raw = raw
                self.chunk_size = chunk_size
                self.pos = 0

            def read(self, n: int = -1) -> bytes:
                if self.pos >= len(self.raw):
                    return b""
                take = min(n if n > 0 else self.chunk_size, self.chunk_size, len(self.raw) - self.pos)
                res = self.raw[self.pos : self.pos + take]
                self.pos += take
                return res

        stream = FragmentedStream(full, chunk_size=3)
        pkt = read_packet(stream)
        self.assertIsNotNone(pkt)
        assert pkt is not None
        self.assertEqual(pkt.channel, 42)
        self.assertEqual(pkt.data, data)

    def test_make_reply_create_playback_stream_custom_spec(self) -> None:
        spec = SampleSpec(format=PA_SAMPLE_S16LE, channels=6, rate=44100)
        rep_bytes = make_reply_create_playback_stream(tag=12, stream_index=2, channel=4, spec=spec)
        rep_pkt = read_packet(io.BytesIO(rep_bytes))
        self.assertIsNotNone(rep_pkt)
        assert rep_pkt is not None
        rep_reader = TagStructReader(rep_pkt.data)
        self.assertEqual(rep_reader.read_u32(), PA_COMMAND_REPLY)
        self.assertEqual(rep_reader.read_u32(), 12)
        self.assertEqual(rep_reader.read_u32(), 2)  # stream index
        self.assertEqual(rep_reader.read_u32(), 4)  # channel
        rep_reader.read_u32()  # maxlength
        rep_reader.read_u32()  # tlength
        rep_reader.read_u32()  # prebuf
        rep_reader.read_u32()  # minreq
        read_spec = rep_reader.read_sample_spec()
        self.assertEqual(read_spec.rate, 44100)
        self.assertEqual(read_spec.channels, 6)
        read_ch_map = rep_reader.read_channel_map()
        self.assertEqual(read_ch_map, [1, 2, 3, 4, 5, 6])

    def test_make_reply_server_info_uses_constants_version(self) -> None:
        from soundhop.constants import VERSION
        info_bytes = make_reply_server_info(tag=5)
        pkt = read_packet(io.BytesIO(info_bytes))
        self.assertIsNotNone(pkt)
        assert pkt is not None
        reader = TagStructReader(pkt.data)
        self.assertEqual(reader.read_u32(), PA_COMMAND_REPLY)
        self.assertEqual(reader.read_u32(), 5)
        reader.read_string()  # server_name
        version_str = reader.read_string()
        self.assertEqual(version_str, VERSION)


if __name__ == "__main__":
    unittest.main()
