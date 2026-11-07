"""PulseAudio Native Protocol tagstruct and packet framing for Windows.

Implements the subset of the PulseAudio native protocol wire format required
to exchange audio streams with Linux PipeWire / PulseAudio instances.
"""

from __future__ import annotations

import io
import struct
from dataclasses import dataclass
from typing import Any

from ...constants import VERSION

# PulseAudio Command Codes
PA_COMMAND_ERROR = 0
PA_COMMAND_TIMEOUT = 1
PA_COMMAND_REPLY = 2
PA_COMMAND_CREATE_PLAYBACK_STREAM = 3
PA_COMMAND_DELETE_PLAYBACK_STREAM = 4
PA_COMMAND_CREATE_RECORD_STREAM = 5
PA_COMMAND_DELETE_RECORD_STREAM = 6
PA_COMMAND_AUTH = 8
PA_COMMAND_REQUEST = 9
PA_COMMAND_DRAIN_PLAYBACK_STREAM = 14
PA_COMMAND_GET_SERVER_INFO = 16
PA_COMMAND_SET_SINK_VOLUME = 17
PA_COMMAND_SET_SOURCE_VOLUME = 18
PA_COMMAND_SET_CLIENT_NAME = 19
PA_COMMAND_SET_SINK_MUTE = 20
PA_COMMAND_SET_SOURCE_MUTE = 21
PA_COMMAND_SUBSCRIBE = 23
PA_COMMAND_SET_DEFAULT_SINK = 27

# PulseAudio Tag Types
PA_TAG_INVALID = 0
PA_TAG_STRING = ord("s")
PA_TAG_STRING_NULL = ord("N")
PA_TAG_U32 = ord("u")
PA_TAG_U8 = ord("B")
PA_TAG_U64 = ord("t")
PA_TAG_S64 = ord("T")
PA_TAG_SAMPLE_SPEC = ord("a")
PA_TAG_ARBITRARY = ord("x")
PA_TAG_BOOLEAN_TRUE = ord("1")
PA_TAG_BOOLEAN_FALSE = ord("0")
PA_TAG_TIMEVAL = ord("e")
PA_TAG_USEC = ord("U")
PA_TAG_CHANNEL_MAP = ord("m")
PA_TAG_CVOLUME = ord("v")
PA_TAG_PROPLIST = ord("P")
PA_TAG_VOLUME = ord("V")

# PulseAudio Sample Formats
PA_SAMPLE_U8 = 0
PA_SAMPLE_ALAW = 1
PA_SAMPLE_ULAW = 2
PA_SAMPLE_S16LE = 3
PA_SAMPLE_S16BE = 4
PA_SAMPLE_FLOAT32LE = 5
PA_SAMPLE_FLOAT32BE = 6
PA_SAMPLE_S32LE = 7
PA_SAMPLE_S32BE = 8

# Control descriptor channel
PA_CONTROL_CHANNEL = 0xFFFFFFFF


@dataclass
class SampleSpec:
    format: int = PA_SAMPLE_S16LE
    channels: int = 2
    rate: int = 48000


@dataclass
class Packet:
    channel: int
    data: bytes

    @property
    def is_control(self) -> bool:
        return self.channel == PA_CONTROL_CHANNEL


class TagStructReader:
    def __init__(self, data: bytes) -> None:
        self.stream = io.BytesIO(data)

    def remaining(self) -> int:
        cur = self.stream.tell()
        self.stream.seek(0, io.SEEK_END)
        end = self.stream.tell()
        self.stream.seek(cur, io.SEEK_SET)
        return end - cur

    def read_u32(self) -> int:
        tag = self.stream.read(1)
        if not tag:
            raise EOFError("Unexpected EOF reading tag")
        if tag[0] != PA_TAG_U32:
            raise ValueError(f"Expected PA_TAG_U32, got {tag!r}")
        raw = self.stream.read(4)
        if len(raw) < 4:
            raise EOFError("Unexpected EOF reading u32")
        return struct.unpack(">I", raw)[0]

    def read_u64(self) -> int:
        tag = self.stream.read(1)
        if not tag:
            raise EOFError("Unexpected EOF reading tag")
        if tag[0] != PA_TAG_U64:
            raise ValueError(f"Expected PA_TAG_U64, got {tag!r}")
        raw = self.stream.read(8)
        if len(raw) < 8:
            raise EOFError("Unexpected EOF reading u64")
        return struct.unpack(">Q", raw)[0]

    def read_boolean(self) -> bool:
        tag = self.stream.read(1)
        if not tag:
            raise EOFError("Unexpected EOF reading tag")
        if tag[0] == PA_TAG_BOOLEAN_TRUE:
            return True
        if tag[0] == PA_TAG_BOOLEAN_FALSE:
            return False
        raise ValueError(f"Expected boolean tag, got {tag!r}")

    def read_cvolume(self) -> tuple[int, list[int]]:
        tag = self.stream.read(1)
        if not tag:
            raise EOFError("Unexpected EOF reading tag")
        if tag[0] != PA_TAG_CVOLUME:
            raise ValueError(f"Expected PA_TAG_CVOLUME, got {tag!r}")
        raw_ch = self.stream.read(1)
        if not raw_ch:
            raise EOFError("Unexpected EOF reading channels")
        channels = raw_ch[0]
        vols: list[int] = []
        for _ in range(channels):
            raw_vol = self.stream.read(4)
            if len(raw_vol) < 4:
                raise EOFError("Unexpected EOF reading channel volume")
            vols.append(struct.unpack(">I", raw_vol)[0])
        return channels, vols

    def read_string(self) -> str | None:
        tag = self.stream.read(1)
        if not tag:
            raise EOFError("Unexpected EOF reading tag")
        if tag[0] == PA_TAG_STRING_NULL:
            return None
        if tag[0] != PA_TAG_STRING:
            raise ValueError(f"Expected PA_TAG_STRING, got {tag!r}")
        chars = bytearray()
        while True:
            b = self.stream.read(1)
            if not b:
                raise EOFError("Unexpected EOF reading string")
            if b[0] == 0:
                break
            chars.extend(b)
        return chars.decode("utf-8", errors="replace")

    def read_arbitrary(self) -> bytes:
        tag = self.stream.read(1)
        if not tag:
            raise EOFError("Unexpected EOF reading tag")
        if tag[0] != PA_TAG_ARBITRARY:
            raise ValueError(f"Expected PA_TAG_ARBITRARY, got {tag!r}")
        raw_len = self.stream.read(4)
        if len(raw_len) < 4:
            raise EOFError("Unexpected EOF reading arbitrary length")
        length = struct.unpack(">I", raw_len)[0]
        data = self.stream.read(length)
        if len(data) < length:
            raise EOFError("Unexpected EOF reading arbitrary data")
        return data

    def read_sample_spec(self) -> SampleSpec:
        tag = self.stream.read(1)
        if not tag:
            raise EOFError("Unexpected EOF reading tag")
        if tag[0] != PA_TAG_SAMPLE_SPEC:
            raise ValueError(f"Expected PA_TAG_SAMPLE_SPEC, got {tag!r}")
        raw = self.stream.read(6)
        if len(raw) < 6:
            raise EOFError("Unexpected EOF reading sample spec")
        fmt, channels, rate = struct.unpack(">BBI", raw)
        return SampleSpec(format=fmt, channels=channels, rate=rate)

    def read_channel_map(self) -> list[int]:
        tag = self.stream.read(1)
        if not tag:
            raise EOFError("Unexpected EOF reading tag")
        if tag[0] != PA_TAG_CHANNEL_MAP:
            raise ValueError(f"Expected PA_TAG_CHANNEL_MAP, got {tag!r}")
        channels = self.stream.read(1)
        if not channels:
            raise EOFError("Unexpected EOF reading channel map count")
        count = channels[0]
        raw_map = self.stream.read(count)
        return list(raw_map)

    def read_proplist(self) -> dict[str, bytes]:
        tag = self.stream.read(1)
        if not tag:
            raise EOFError("Unexpected EOF reading tag")
        if tag[0] != PA_TAG_PROPLIST:
            raise ValueError(f"Expected PA_TAG_PROPLIST, got {tag!r}")
        props: dict[str, bytes] = {}
        while True:
            # read null-terminated key name
            chars = bytearray()
            while True:
                b = self.stream.read(1)
                if not b:
                    raise EOFError("Unexpected EOF reading proplist key")
                if b[0] == 0:
                    break
                chars.extend(b)
            if not chars:
                break
            key = chars.decode("utf-8", errors="replace")
            # read length (u32) then value
            raw_len = self.stream.read(4)
            length = struct.unpack(">I", raw_len)[0]
            val = self.stream.read(length)
            props[key] = val
        return props

    def skip_value(self) -> None:
        """Skip any single tagged value."""
        tag = self.stream.read(1)
        if not tag:
            return
        t = tag[0]
        if t in (PA_TAG_STRING_NULL, PA_TAG_BOOLEAN_TRUE, PA_TAG_BOOLEAN_FALSE):
            return
        if t == PA_TAG_U8:
            self.stream.read(1)
        elif t in (PA_TAG_U32, PA_TAG_VOLUME):
            self.stream.read(4)
        elif t in (PA_TAG_U64, PA_TAG_S64, PA_TAG_USEC):
            self.stream.read(8)
        elif t == PA_TAG_SAMPLE_SPEC:
            self.stream.read(6)
        elif t == PA_TAG_TIMEVAL:
            self.stream.read(8)
        elif t == PA_TAG_STRING:
            while True:
                b = self.stream.read(1)
                if not b or b[0] == 0:
                    break
        elif t == PA_TAG_ARBITRARY:
            raw_len = self.stream.read(4)
            if len(raw_len) == 4:
                length = struct.unpack(">I", raw_len)[0]
                self.stream.read(length)
        elif t == PA_TAG_CHANNEL_MAP:
            count_b = self.stream.read(1)
            if count_b:
                self.stream.read(count_b[0])
        elif t == PA_TAG_CVOLUME:
            count_b = self.stream.read(1)
            if count_b:
                self.stream.read(count_b[0] * 4)
        elif t == PA_TAG_PROPLIST:
            while True:
                chars = bytearray()
                while True:
                    b = self.stream.read(1)
                    if not b or b[0] == 0:
                        break
                    chars.extend(b)
                if not chars:
                    break
                raw_len = self.stream.read(4)
                if len(raw_len) == 4:
                    self.stream.read(struct.unpack(">I", raw_len)[0])


class TagStructWriter:
    def __init__(self) -> None:
        self.stream = io.BytesIO()

    def getvalue(self) -> bytes:
        return self.stream.getvalue()

    def write_u32(self, value: int) -> None:
        self.stream.write(bytes([PA_TAG_U32]))
        self.stream.write(struct.pack(">I", value & 0xFFFFFFFF))

    def write_u64(self, value: int) -> None:
        self.stream.write(bytes([PA_TAG_U64]))
        self.stream.write(struct.pack(">Q", value & 0xFFFFFFFFFFFFFFFF))

    def write_u8(self, value: int) -> None:
        self.stream.write(bytes([PA_TAG_U8]))
        self.stream.write(bytes([value & 0xFF]))

    def write_string(self, value: str | None) -> None:
        if value is None:
            self.stream.write(bytes([PA_TAG_STRING_NULL]))
        else:
            self.stream.write(bytes([PA_TAG_STRING]))
            self.stream.write(value.encode("utf-8") + b"\0")

    def write_arbitrary(self, data: bytes) -> None:
        self.stream.write(bytes([PA_TAG_ARBITRARY]))
        self.stream.write(struct.pack(">I", len(data)))
        self.stream.write(data)

    def write_sample_spec(self, spec: SampleSpec) -> None:
        self.stream.write(bytes([PA_TAG_SAMPLE_SPEC]))
        self.stream.write(struct.pack(">BBI", spec.format, spec.channels, spec.rate))

    def write_channel_map(self, map_entries: list[int]) -> None:
        self.stream.write(bytes([PA_TAG_CHANNEL_MAP]))
        self.stream.write(bytes([len(map_entries)]))
        self.stream.write(bytes(map_entries))

    def write_boolean(self, value: bool) -> None:
        self.stream.write(bytes([PA_TAG_BOOLEAN_TRUE if value else PA_TAG_BOOLEAN_FALSE]))

    def write_cvolume(self, channels: int = 2, volume: int = 65536) -> None:
        self.stream.write(bytes([PA_TAG_CVOLUME]))
        self.stream.write(bytes([channels]))
        for _ in range(channels):
            self.stream.write(struct.pack(">I", volume))

    def write_usec(self, value: int = 0) -> None:
        self.stream.write(bytes([PA_TAG_USEC]))
        self.stream.write(struct.pack(">Q", value))

    def write_proplist(self, props: dict[str, str]) -> None:
        self.stream.write(bytes([PA_TAG_PROPLIST]))
        for k, v in props.items():
            encoded_key = k.encode("utf-8") + b"\0"
            val_bytes = v.encode("utf-8")
            if not val_bytes.endswith(b"\0"):
                val_bytes += b"\0"
            self.stream.write(encoded_key)
            self.stream.write(struct.pack(">I", len(val_bytes)))
            self.stream.write(val_bytes)
        self.stream.write(b"\0")  # End of proplist


def _read_exact(stream: Any, count: int) -> bytes | None:
    """Read exactly count bytes from a stream, handling partial reads gracefully."""
    buf = bytearray()
    while len(buf) < count:
        needed = count - len(buf)
        chunk = stream.read(needed)
        if not chunk:
            return None
        buf.extend(chunk)
    return bytes(buf)


def read_packet(stream: io.BytesIO | Any) -> Packet | None:
    """Read a single framing packet: 4 bytes channel, 4 bytes length, payload."""
    header = _read_exact(stream, 8)
    if not header or len(header) < 8:
        return None
    channel, length = struct.unpack(">II", header)
    data = _read_exact(stream, length)
    if data is None or len(data) < length:
        return None
    return Packet(channel=channel, data=data)


def write_packet(channel: int, data: bytes) -> bytes:
    """Encode a packet with 8-byte header."""
    return struct.pack(">II", channel, len(data)) + data


# Helper high-level builders
def make_reply_auth(tag: int, protocol_version: int = 35) -> bytes:
    writer = TagStructWriter()
    writer.write_u32(PA_COMMAND_REPLY)
    writer.write_u32(tag)
    writer.write_u32(protocol_version)
    return write_packet(PA_CONTROL_CHANNEL, writer.getvalue())


def make_reply_ok(tag: int) -> bytes:
    writer = TagStructWriter()
    writer.write_u32(PA_COMMAND_REPLY)
    writer.write_u32(tag)
    return write_packet(PA_CONTROL_CHANNEL, writer.getvalue())


def make_reply_server_info(
    tag: int,
    server_name: str = "SoundHop Windows Audio Server",
    server_version: str = VERSION,
    user_name: str = "soundhop",
    host_name: str = "windows",
    default_sink: str = "SoundHop-Speakers",
    default_source: str = "SoundHop-Mic",
) -> bytes:
    writer = TagStructWriter()
    writer.write_u32(PA_COMMAND_REPLY)
    writer.write_u32(tag)
    writer.write_string(server_name)
    writer.write_string(server_version)
    writer.write_string(user_name)
    writer.write_string(host_name)
    writer.write_sample_spec(SampleSpec(format=PA_SAMPLE_S16LE, channels=2, rate=48000))
    writer.write_string(default_sink)
    writer.write_string(default_source)
    writer.write_u32(0)  # cookie
    return write_packet(PA_CONTROL_CHANNEL, writer.getvalue())


def make_reply_create_playback_stream(
    tag: int,
    stream_index: int = 0,
    channel: int = 0,
    maxlength: int = 4194304,
    tlength: int = 96000,
    prebuf: int = 48000,
    minreq: int = 4800,
    spec: SampleSpec | None = None,
) -> bytes:
    if spec is None:
        spec = SampleSpec(format=PA_SAMPLE_S16LE, channels=2, rate=48000)
    writer = TagStructWriter()
    writer.write_u32(PA_COMMAND_REPLY)
    writer.write_u32(tag)
    writer.write_u32(stream_index)
    writer.write_u32(channel)
    writer.write_u32(maxlength)
    writer.write_u32(tlength)
    writer.write_u32(prebuf)
    writer.write_u32(minreq)
    writer.write_sample_spec(spec)
    channel_map = [1, 2] if spec.channels == 2 else list(range(1, spec.channels + 1))
    writer.write_channel_map(channel_map)
    writer.write_cvolume(spec.channels, 65536)
    writer.write_boolean(False)  # muted
    return write_packet(PA_CONTROL_CHANNEL, writer.getvalue())


def make_command_auth(
    tag: int = 0,
    protocol_version_or_cookie: int | bytes = 35,
    cookie: bytes = b"\0" * 32,
) -> bytes:
    if isinstance(protocol_version_or_cookie, (bytes, bytearray)):
        protocol_version = 35
        cookie_bytes = bytes(protocol_version_or_cookie)
    else:
        protocol_version = int(protocol_version_or_cookie)
        cookie_bytes = cookie
    writer = TagStructWriter()
    writer.write_u32(PA_COMMAND_AUTH)
    writer.write_u32(tag)
    writer.write_u32(protocol_version)
    writer.write_arbitrary(cookie_bytes)
    return write_packet(PA_CONTROL_CHANNEL, writer.getvalue())


def make_command_set_client_name(tag: int = 1, client_name: str = "SoundHop-Sender") -> bytes:
    writer = TagStructWriter()
    writer.write_u32(PA_COMMAND_SET_CLIENT_NAME)
    writer.write_u32(tag)
    writer.write_proplist({"application.name": client_name})
    return write_packet(PA_CONTROL_CHANNEL, writer.getvalue())


def make_command_create_playback_stream(
    tag: int = 2,
    sink_name: str = "",
    spec: SampleSpec | None = None,
) -> bytes:
    if spec is None:
        spec = SampleSpec(format=PA_SAMPLE_S16LE, channels=2, rate=48000)
    writer = TagStructWriter()
    writer.write_u32(PA_COMMAND_CREATE_PLAYBACK_STREAM)
    writer.write_u32(tag)
    writer.write_sample_spec(spec)
    writer.write_channel_map([1, 2])
    writer.write_u32(0xFFFFFFFF)  # sink index (default)
    writer.write_string(None if not sink_name else sink_name)
    writer.write_u32(4194304)  # maxlength
    writer.write_boolean(False)  # corked
    writer.write_u32(96000)  # tlength
    writer.write_u32(48000)  # prebuf
    writer.write_u32(4800)  # minreq
    writer.write_u32(0)  # fragsize
    writer.write_cvolume(2, 65536)
    writer.write_boolean(False)  # muted
    writer.write_boolean(False)  # no_remix
    writer.write_boolean(False)  # no_remap
    writer.write_boolean(False)  # fix_format
    writer.write_boolean(False)  # fix_rate
    writer.write_boolean(False)  # fix_channels
    writer.write_boolean(False)  # no_move
    writer.write_boolean(False)  # variable_rate
    writer.write_proplist({"media.name": "SoundHop-Stream"})
    return write_packet(PA_CONTROL_CHANNEL, writer.getvalue())


def make_command_set_sink_volume(
    tag: int,
    volume: int,
    sink_index: int = 0xFFFFFFFF,
    sink_name: str | None = None,
    channels: int = 2,
) -> bytes:
    writer = TagStructWriter()
    writer.write_u32(PA_COMMAND_SET_SINK_VOLUME)
    writer.write_u32(tag)
    writer.write_u32(sink_index)
    writer.write_string(sink_name)
    writer.write_cvolume(channels, volume)
    return write_packet(PA_CONTROL_CHANNEL, writer.getvalue())


def make_command_set_sink_mute(
    tag: int,
    mute: bool,
    sink_index: int = 0xFFFFFFFF,
    sink_name: str | None = None,
) -> bytes:
    writer = TagStructWriter()
    writer.write_u32(PA_COMMAND_SET_SINK_MUTE)
    writer.write_u32(tag)
    writer.write_u32(sink_index)
    writer.write_string(sink_name)
    writer.write_boolean(mute)
    return write_packet(PA_CONTROL_CHANNEL, writer.getvalue())
