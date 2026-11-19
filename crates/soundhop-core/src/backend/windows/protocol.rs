//! PulseAudio Native Protocol tagstruct and pstream packet framing codec.
//!
//! Wire-compatible with Linux PulseAudio (`module-native-protocol-tcp`,
//! `module-tunnel-sink`, `libpulse`) and PipeWire (`pipewire-pulse`).

use std::collections::BTreeMap;
use std::io::{self, Read};

use crate::constants::VERSION;

// Protocol Version & Capability Flags
pub const PA_PROTOCOL_VERSION: u32 = 35;
pub const PA_MIN_PROTOCOL_VERSION: u32 = 8;
pub const PA_PROTOCOL_VERSION_MASK: u32 = 0x0000_FFFF;
pub const PA_PROTOCOL_FLAG_SHM: u32 = 0x8000_0000;
pub const PA_PROTOCOL_FLAG_MEMFD: u32 = 0x4000_0000;

// PulseAudio Command Codes (from src/pulsecore/native-common.h)
pub const PA_COMMAND_ERROR: u32 = 0;
pub const PA_COMMAND_TIMEOUT: u32 = 1;
pub const PA_COMMAND_REPLY: u32 = 2;
pub const PA_COMMAND_CREATE_PLAYBACK_STREAM: u32 = 3;
pub const PA_COMMAND_DELETE_PLAYBACK_STREAM: u32 = 4;
pub const PA_COMMAND_CREATE_RECORD_STREAM: u32 = 5;
pub const PA_COMMAND_DELETE_RECORD_STREAM: u32 = 6;
pub const PA_COMMAND_EXIT: u32 = 7;
pub const PA_COMMAND_AUTH: u32 = 8;
pub const PA_COMMAND_SET_CLIENT_NAME: u32 = 9;
pub const PA_COMMAND_LOOKUP_SINK: u32 = 10;
pub const PA_COMMAND_LOOKUP_SOURCE: u32 = 11;
pub const PA_COMMAND_DRAIN_PLAYBACK_STREAM: u32 = 12;
pub const PA_COMMAND_STAT: u32 = 13;
pub const PA_COMMAND_GET_PLAYBACK_LATENCY: u32 = 14;
pub const PA_COMMAND_GET_SERVER_INFO: u32 = 20;
pub const PA_COMMAND_GET_SINK_INFO: u32 = 21;
pub const PA_COMMAND_GET_SINK_INFO_LIST: u32 = 22;
pub const PA_COMMAND_GET_SOURCE_INFO: u32 = 23;
pub const PA_COMMAND_GET_SOURCE_INFO_LIST: u32 = 24;
pub const PA_COMMAND_SUBSCRIBE: u32 = 35;
pub const PA_COMMAND_SET_SINK_VOLUME: u32 = 36;
pub const PA_COMMAND_SET_SINK_INPUT_VOLUME: u32 = 37;
pub const PA_COMMAND_SET_SOURCE_VOLUME: u32 = 38;
pub const PA_COMMAND_SET_SINK_MUTE: u32 = 39;
pub const PA_COMMAND_SET_SOURCE_MUTE: u32 = 40;
pub const PA_COMMAND_CORK_PLAYBACK_STREAM: u32 = 41;
pub const PA_COMMAND_FLUSH_PLAYBACK_STREAM: u32 = 42;
pub const PA_COMMAND_TRIGGER_PLAYBACK_STREAM: u32 = 43;
pub const PA_COMMAND_SET_DEFAULT_SINK: u32 = 44;
pub const PA_COMMAND_SET_DEFAULT_SOURCE: u32 = 45;
pub const PA_COMMAND_PREBUF_PLAYBACK_STREAM: u32 = 60;
pub const PA_COMMAND_REQUEST: u32 = 61;
pub const PA_COMMAND_OVERFLOW: u32 = 62;
pub const PA_COMMAND_UNDERFLOW: u32 = 63;
pub const PA_COMMAND_SET_SINK_INPUT_MUTE: u32 = 70;
pub const PA_COMMAND_SET_PLAYBACK_STREAM_BUFFER_ATTR: u32 = 73;
pub const PA_COMMAND_STARTED: u32 = 87;

// PulseAudio Error Codes (from src/pulse/def.h)
pub const PA_ERR_ACCESS: u32 = 1;
pub const PA_ERR_COMMAND: u32 = 2;
pub const PA_ERR_INVALID: u32 = 3;
pub const PA_ERR_NOENTITY: u32 = 5;
pub const PA_ERR_VERSION: u32 = 17;

// PulseAudio Tag Types (from src/pulsecore/tagstruct.h)
pub const PA_TAG_INVALID: u8 = 0;
pub const PA_TAG_STRING: u8 = b't';
pub const PA_TAG_STRING_NULL: u8 = b'N';
pub const PA_TAG_U32: u8 = b'L';
pub const PA_TAG_U8: u8 = b'B';
pub const PA_TAG_U64: u8 = b'R';
pub const PA_TAG_S64: u8 = b'r';
pub const PA_TAG_SAMPLE_SPEC: u8 = b'a';
pub const PA_TAG_ARBITRARY: u8 = b'x';
pub const PA_TAG_BOOLEAN_TRUE: u8 = b'1';
pub const PA_TAG_BOOLEAN_FALSE: u8 = b'0';
pub const PA_TAG_TIMEVAL: u8 = b'T';
pub const PA_TAG_USEC: u8 = b'U';
pub const PA_TAG_CHANNEL_MAP: u8 = b'm';
pub const PA_TAG_CVOLUME: u8 = b'v';
pub const PA_TAG_PROPLIST: u8 = b'P';
pub const PA_TAG_VOLUME: u8 = b'V';
pub const PA_TAG_FORMAT_INFO: u8 = b'f';

// Sample Formats (from src/pulse/sample.h)
pub const PA_SAMPLE_U8: u8 = 0;
pub const PA_SAMPLE_S16LE: u8 = 3;
pub const PA_SAMPLE_FLOAT32LE: u8 = 5;

// Stream / Sink Constants
pub const PA_PSTREAM_DESCRIPTOR_SIZE: usize = 20;
pub const PA_CONTROL_CHANNEL: u32 = 0xFFFF_FFFF;
pub const PA_INVALID_INDEX: u32 = 0xFFFF_FFFF;
pub const PA_SEEK_RELATIVE: u32 = 0;
pub const PA_VOLUME_NORM: u32 = 0x10000;
pub const PA_SINK_RUNNING: u32 = 0;
pub const PA_ENCODING_PCM: u8 = 1;
pub const MAX_CONTROL_PACKET_BYTES: usize = 64 * 1024;
pub const MAX_AUDIO_PACKET_BYTES: usize = 4 * 1024 * 1024;

pub const DEFAULT_MAXLENGTH: u32 = 4_194_304;
pub const DEFAULT_TLENGTH: u32 = 96_000;
pub const DEFAULT_PREBUF: u32 = 48_000;
pub const DEFAULT_MINREQ: u32 = 4_800;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampleSpec {
    pub format: u8,
    pub channels: u8,
    pub rate: u32,
}

impl Default for SampleSpec {
    fn default() -> Self {
        Self {
            format: PA_SAMPLE_S16LE,
            channels: 2,
            rate: 48_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferAttr {
    pub maxlength: u32,
    pub tlength: u32,
    pub prebuf: u32,
    pub minreq: u32,
}

impl Default for BufferAttr {
    fn default() -> Self {
        Self {
            maxlength: DEFAULT_MAXLENGTH,
            tlength: DEFAULT_TLENGTH,
            prebuf: DEFAULT_PREBUF,
            minreq: DEFAULT_MINREQ,
        }
    }
}

impl BufferAttr {
    /// Normalize client-requested buffer attributes, replacing `u32::MAX`
    /// ("server default") with sane defaults and ensuring invariants hold.
    pub fn normalized(self) -> Self {
        let mut maxlength = if self.maxlength == PA_INVALID_INDEX || self.maxlength == 0 {
            DEFAULT_MAXLENGTH
        } else {
            self.maxlength.clamp(16_384, MAX_AUDIO_PACKET_BYTES as u32)
        };
        let tlength = if self.tlength == PA_INVALID_INDEX || self.tlength == 0 {
            DEFAULT_TLENGTH
        } else {
            self.tlength.clamp(4_096, 524_288)
        };
        if maxlength < tlength {
            maxlength = tlength;
        }
        let minreq = if self.minreq == PA_INVALID_INDEX || self.minreq == 0 {
            DEFAULT_MINREQ.min(tlength / 2).max(1_024)
        } else {
            self.minreq.clamp(1_024, (tlength / 2).max(1_024))
        };
        let prebuf = if self.prebuf == PA_INVALID_INDEX {
            tlength.saturating_sub(minreq)
        } else {
            self.prebuf.min(tlength)
        };
        Self {
            maxlength,
            tlength,
            prebuf,
            minreq,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatePlaybackStreamRequest {
    pub spec: SampleSpec,
    pub channel_map: Vec<u8>,
    pub buffer_attr: BufferAttr,
    pub corked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub channel: u32,
    pub offset: u64,
    pub flags: u32,
    pub data: Vec<u8>,
}

impl Packet {
    pub fn is_control(&self) -> bool {
        self.channel == PA_CONTROL_CHANNEL
    }
}

/// Negotiate the PulseAudio native protocol version from a raw version field
/// that may carry SHM/MEMFD flags in the upper 16 bits.
pub fn negotiate_protocol_version(remote_raw: u32) -> Result<u32, u32> {
    let remote = remote_raw & PA_PROTOCOL_VERSION_MASK;
    if remote < PA_MIN_PROTOCOL_VERSION {
        return Err(PA_ERR_VERSION);
    }
    Ok(remote.min(PA_PROTOCOL_VERSION))
}

/// Build a valid default PulseAudio channel map for `channels`.
pub fn default_channel_map(channels: u8) -> Vec<u8> {
    match channels {
        0 => Vec::new(),
        1 => vec![0], // PA_CHANNEL_POSITION_MONO
        n => (1..=n).collect(),
    }
}

pub struct TagStructReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> TagStructReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    fn read_byte(&mut self) -> io::Result<u8> {
        if self.pos >= self.data.len() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "EOF"));
        }
        let b = self.data[self.pos];
        self.pos += 1;
        Ok(b)
    }

    fn read_slice(&mut self, len: usize) -> io::Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "EOF"))?;
        if end > self.data.len() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "EOF"));
        }
        let s = &self.data[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    pub fn read_u8(&mut self) -> io::Result<u8> {
        let tag = self.read_byte()?;
        if tag != PA_TAG_U8 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Expected PA_TAG_U8, got {tag}"),
            ));
        }
        self.read_byte()
    }

    pub fn read_u32(&mut self) -> io::Result<u32> {
        let tag = self.read_byte()?;
        if tag != PA_TAG_U32 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Expected PA_TAG_U32, got {tag}"),
            ));
        }
        let raw = self.read_slice(4)?;
        Ok(u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]))
    }

    pub fn read_u64(&mut self) -> io::Result<u64> {
        let tag = self.read_byte()?;
        if tag != PA_TAG_U64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Expected PA_TAG_U64, got {tag}"),
            ));
        }
        let raw = self.read_slice(8)?;
        let mut arr = [0u8; 8];
        arr.copy_from_slice(raw);
        Ok(u64::from_be_bytes(arr))
    }

    pub fn read_s64(&mut self) -> io::Result<i64> {
        let tag = self.read_byte()?;
        if tag != PA_TAG_S64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Expected PA_TAG_S64, got {tag}"),
            ));
        }
        let raw = self.read_slice(8)?;
        let mut arr = [0u8; 8];
        arr.copy_from_slice(raw);
        Ok(i64::from_be_bytes(arr))
    }

    pub fn read_usec(&mut self) -> io::Result<u64> {
        let tag = self.read_byte()?;
        if tag != PA_TAG_USEC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Expected PA_TAG_USEC, got {tag}"),
            ));
        }
        let raw = self.read_slice(8)?;
        let mut arr = [0u8; 8];
        arr.copy_from_slice(raw);
        Ok(u64::from_be_bytes(arr))
    }

    pub fn read_timeval(&mut self) -> io::Result<(u32, u32)> {
        let tag = self.read_byte()?;
        if tag != PA_TAG_TIMEVAL {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Expected PA_TAG_TIMEVAL, got {tag}"),
            ));
        }
        let raw = self.read_slice(8)?;
        let sec = u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]);
        let usec = u32::from_be_bytes([raw[4], raw[5], raw[6], raw[7]]);
        Ok((sec, usec))
    }

    pub fn read_volume(&mut self) -> io::Result<u32> {
        let tag = self.read_byte()?;
        if tag != PA_TAG_VOLUME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Expected PA_TAG_VOLUME, got {tag}"),
            ));
        }
        let raw = self.read_slice(4)?;
        Ok(u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]))
    }

    pub fn read_boolean(&mut self) -> io::Result<bool> {
        match self.read_byte()? {
            PA_TAG_BOOLEAN_TRUE => Ok(true),
            PA_TAG_BOOLEAN_FALSE => Ok(false),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Expected boolean tag, got {other}"),
            )),
        }
    }

    pub fn read_cvolume(&mut self) -> io::Result<(u8, Vec<u32>)> {
        let tag = self.read_byte()?;
        if tag != PA_TAG_CVOLUME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Expected PA_TAG_CVOLUME",
            ));
        }
        let channels = self.read_byte()?;
        let mut vols = Vec::with_capacity(channels as usize);
        for _ in 0..channels {
            let raw = self.read_slice(4)?;
            vols.push(u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]));
        }
        Ok((channels, vols))
    }

    pub fn read_string(&mut self) -> io::Result<Option<String>> {
        let tag = self.read_byte()?;
        if tag == PA_TAG_STRING_NULL {
            return Ok(None);
        }
        if tag != PA_TAG_STRING {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Expected PA_TAG_STRING, got {tag}"),
            ));
        }
        let mut bytes = Vec::new();
        loop {
            let b = self.read_byte()?;
            if b == 0 {
                break;
            }
            bytes.push(b);
        }
        Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
    }

    pub fn read_arbitrary(&mut self) -> io::Result<Vec<u8>> {
        let tag = self.read_byte()?;
        if tag != PA_TAG_ARBITRARY {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Expected PA_TAG_ARBITRARY",
            ));
        }
        let raw_len = self.read_slice(4)?;
        let len = u32::from_be_bytes([raw_len[0], raw_len[1], raw_len[2], raw_len[3]]) as usize;
        let data = self.read_slice(len)?;
        Ok(data.to_vec())
    }

    pub fn read_sample_spec(&mut self) -> io::Result<SampleSpec> {
        let tag = self.read_byte()?;
        if tag != PA_TAG_SAMPLE_SPEC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Expected PA_TAG_SAMPLE_SPEC",
            ));
        }
        let raw = self.read_slice(6)?;
        let rate = u32::from_be_bytes([raw[2], raw[3], raw[4], raw[5]]);
        Ok(SampleSpec {
            format: raw[0],
            channels: raw[1],
            rate,
        })
    }

    pub fn read_channel_map(&mut self) -> io::Result<Vec<u8>> {
        let tag = self.read_byte()?;
        if tag != PA_TAG_CHANNEL_MAP {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Expected PA_TAG_CHANNEL_MAP",
            ));
        }
        let count = self.read_byte()? as usize;
        let map = self.read_slice(count)?;
        Ok(map.to_vec())
    }

    pub fn read_proplist(&mut self) -> io::Result<BTreeMap<String, Vec<u8>>> {
        let tag = self.read_byte()?;
        if tag != PA_TAG_PROPLIST {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Expected PA_TAG_PROPLIST",
            ));
        }
        let mut props = BTreeMap::new();
        while let Some(key) = self.read_string()? {
            let len = self.read_u32()? as usize;
            let val = self.read_arbitrary()?;
            if val.len() != len {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "Proplist value length mismatch for '{key}': expected {len}, got {}",
                        val.len()
                    ),
                ));
            }
            props.insert(key, val);
        }
        Ok(props)
    }

    pub fn read_format_info(&mut self) -> io::Result<(u8, BTreeMap<String, Vec<u8>>)> {
        let tag = self.read_byte()?;
        if tag != PA_TAG_FORMAT_INFO {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Expected PA_TAG_FORMAT_INFO",
            ));
        }
        let encoding = self.read_u8()?;
        let plist = self.read_proplist()?;
        Ok((encoding, plist))
    }
}

#[derive(Default)]
pub struct TagStructWriter {
    buf: Vec<u8>,
}

impl TagStructWriter {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    pub fn write_u32(&mut self, value: u32) {
        self.buf.push(PA_TAG_U32);
        self.buf.extend_from_slice(&value.to_be_bytes());
    }

    pub fn write_u64(&mut self, value: u64) {
        self.buf.push(PA_TAG_U64);
        self.buf.extend_from_slice(&value.to_be_bytes());
    }

    pub fn write_s64(&mut self, value: i64) {
        self.buf.push(PA_TAG_S64);
        self.buf.extend_from_slice(&value.to_be_bytes());
    }

    pub fn write_u8(&mut self, value: u8) {
        self.buf.push(PA_TAG_U8);
        self.buf.push(value);
    }

    pub fn write_usec(&mut self, value: u64) {
        self.buf.push(PA_TAG_USEC);
        self.buf.extend_from_slice(&value.to_be_bytes());
    }

    pub fn write_timeval(&mut self, sec: u32, usec: u32) {
        self.buf.push(PA_TAG_TIMEVAL);
        self.buf.extend_from_slice(&sec.to_be_bytes());
        self.buf.extend_from_slice(&usec.to_be_bytes());
    }

    pub fn write_volume(&mut self, volume: u32) {
        self.buf.push(PA_TAG_VOLUME);
        self.buf.extend_from_slice(&volume.to_be_bytes());
    }

    pub fn write_string(&mut self, value: Option<&str>) {
        match value {
            None => self.buf.push(PA_TAG_STRING_NULL),
            Some(s) => {
                self.buf.push(PA_TAG_STRING);
                self.buf.extend_from_slice(s.as_bytes());
                self.buf.push(0);
            }
        }
    }

    pub fn write_arbitrary(&mut self, data: &[u8]) {
        self.buf.push(PA_TAG_ARBITRARY);
        self.buf
            .extend_from_slice(&(data.len() as u32).to_be_bytes());
        self.buf.extend_from_slice(data);
    }

    pub fn write_sample_spec(&mut self, spec: SampleSpec) {
        self.buf.push(PA_TAG_SAMPLE_SPEC);
        self.buf.push(spec.format);
        self.buf.push(spec.channels);
        self.buf.extend_from_slice(&spec.rate.to_be_bytes());
    }

    pub fn write_channel_map(&mut self, entries: &[u8]) {
        self.buf.push(PA_TAG_CHANNEL_MAP);
        self.buf.push(entries.len() as u8);
        self.buf.extend_from_slice(entries);
    }

    pub fn write_boolean(&mut self, value: bool) {
        self.buf.push(if value {
            PA_TAG_BOOLEAN_TRUE
        } else {
            PA_TAG_BOOLEAN_FALSE
        });
    }

    pub fn write_cvolume(&mut self, channels: u8, volume: u32) {
        self.buf.push(PA_TAG_CVOLUME);
        self.buf.push(channels);
        for _ in 0..channels {
            self.buf.extend_from_slice(&volume.to_be_bytes());
        }
    }

    pub fn write_proplist(&mut self, props: &[(&str, &str)]) {
        self.buf.push(PA_TAG_PROPLIST);
        for (k, v) in props {
            self.write_string(Some(k));
            let mut val_bytes = v.as_bytes().to_vec();
            if !val_bytes.ends_with(&[0]) {
                val_bytes.push(0);
            }
            self.write_u32(val_bytes.len() as u32);
            self.write_arbitrary(&val_bytes);
        }
        self.write_string(None);
    }

    pub fn write_format_info(&mut self, encoding: u8, props: &[(&str, &str)]) {
        self.buf.push(PA_TAG_FORMAT_INFO);
        self.write_u8(encoding);
        self.write_proplist(props);
    }
}

/// Read one PulseAudio pstream frame with its 20-byte big-endian descriptor:
/// `[length, channel, offset_hi, offset_lo, flags]`.
///
/// Preserves `TimedOut` and `WouldBlock` when raised at a packet boundary so
/// idle clients are not disconnected on socket read timeouts.
pub fn read_packet<R: Read>(stream: &mut R) -> io::Result<Packet> {
    let mut header = [0u8; PA_PSTREAM_DESCRIPTOR_SIZE];
    let first = loop {
        match stream.read(&mut header[..1]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "EOF reading packet header",
                ));
            }
            Ok(n) => break n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    };
    if first < PA_PSTREAM_DESCRIPTOR_SIZE {
        stream.read_exact(&mut header[first..]).map_err(|e| {
            if matches!(
                e.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ) {
                io::Error::new(io::ErrorKind::UnexpectedEof, "Timed out mid-header")
            } else {
                e
            }
        })?;
    }
    let length = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let channel = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
    let offset_hi = u32::from_be_bytes([header[8], header[9], header[10], header[11]]);
    let offset_lo = u32::from_be_bytes([header[12], header[13], header[14], header[15]]);
    let flags = u32::from_be_bytes([header[16], header[17], header[18], header[19]]);
    let offset = ((offset_hi as u64) << 32) | (offset_lo as u64);

    let max_len = if channel == PA_CONTROL_CHANNEL {
        MAX_CONTROL_PACKET_BYTES
    } else {
        MAX_AUDIO_PACKET_BYTES
    };
    if length > max_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Packet length {length} exceeds maximum {max_len}"),
        ));
    }
    let mut data = vec![0u8; length];
    if length > 0 {
        stream.read_exact(&mut data).map_err(|e| {
            if matches!(
                e.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ) {
                io::Error::new(io::ErrorKind::UnexpectedEof, "Timed out mid-packet")
            } else {
                e
            }
        })?;
    }
    Ok(Packet {
        channel,
        offset,
        flags,
        data,
    })
}

/// Encode a PulseAudio pstream frame with its 20-byte big-endian descriptor:
/// `[length, channel, offset_hi=0, offset_lo=0, flags=0]`.
pub fn write_packet(channel: u32, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(PA_PSTREAM_DESCRIPTOR_SIZE + data.len());
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(&channel.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes()); // offset_hi
    out.extend_from_slice(&0u32.to_be_bytes()); // offset_lo
    out.extend_from_slice(&PA_SEEK_RELATIVE.to_be_bytes()); // flags
    out.extend_from_slice(data);
    out
}

pub fn make_reply_error(tag: u32, error_code: u32) -> Vec<u8> {
    let mut w = TagStructWriter::new();
    w.write_u32(PA_COMMAND_ERROR);
    w.write_u32(tag);
    w.write_u32(error_code);
    write_packet(PA_CONTROL_CHANNEL, &w.into_bytes())
}

pub fn make_reply_auth(tag: u32, protocol_version: u32) -> Vec<u8> {
    let mut w = TagStructWriter::new();
    w.write_u32(PA_COMMAND_REPLY);
    w.write_u32(tag);
    // Clear SHM/MEMFD flags: TCP transport does not support shared memory.
    w.write_u32(protocol_version & PA_PROTOCOL_VERSION_MASK);
    write_packet(PA_CONTROL_CHANNEL, &w.into_bytes())
}

pub fn make_reply_ok(tag: u32) -> Vec<u8> {
    let mut w = TagStructWriter::new();
    w.write_u32(PA_COMMAND_REPLY);
    w.write_u32(tag);
    write_packet(PA_CONTROL_CHANNEL, &w.into_bytes())
}

pub fn make_reply_u32(tag: u32, value: u32) -> Vec<u8> {
    let mut w = TagStructWriter::new();
    w.write_u32(PA_COMMAND_REPLY);
    w.write_u32(tag);
    w.write_u32(value);
    write_packet(PA_CONTROL_CHANNEL, &w.into_bytes())
}

pub fn make_reply_set_client_name(tag: u32, protocol_version: u32, client_index: u32) -> Vec<u8> {
    let mut w = TagStructWriter::new();
    w.write_u32(PA_COMMAND_REPLY);
    w.write_u32(tag);
    if protocol_version >= 13 {
        w.write_u32(client_index);
    }
    write_packet(PA_CONTROL_CHANNEL, &w.into_bytes())
}

pub fn make_reply_server_info(tag: u32, protocol_version: u32) -> Vec<u8> {
    let mut w = TagStructWriter::new();
    w.write_u32(PA_COMMAND_REPLY);
    w.write_u32(tag);
    w.write_string(Some("SoundHop Windows Audio Server"));
    w.write_string(Some(VERSION));
    w.write_string(Some("soundhop"));
    w.write_string(Some("windows"));
    w.write_sample_spec(SampleSpec::default());
    w.write_string(Some("SoundHop-Speakers"));
    w.write_string(Some("SoundHop-Mic"));
    w.write_u32(0); // cookie
    if protocol_version >= 15 {
        w.write_channel_map(&[1, 2]);
    }
    write_packet(PA_CONTROL_CHANNEL, &w.into_bytes())
}

pub fn make_reply_sink_info(
    tag: u32,
    protocol_version: u32,
    volume_pct: u32,
    muted: bool,
) -> Vec<u8> {
    let spec = SampleSpec::default();
    let ch_map = default_channel_map(spec.channels);
    let pa_vol = ((volume_pct.min(150) as u64 * PA_VOLUME_NORM as u64) / 100) as u32;

    let mut w = TagStructWriter::new();
    w.write_u32(PA_COMMAND_REPLY);
    w.write_u32(tag);

    // sink_fill_tagstruct (from protocol-native.c)
    w.write_u32(0); // sink index
    w.write_string(Some("SoundHop-Speakers"));
    w.write_string(Some("SoundHop Windows Speakers"));
    w.write_sample_spec(spec);
    w.write_channel_map(&ch_map);
    w.write_u32(PA_INVALID_INDEX); // owner module index
    w.write_cvolume(spec.channels, pa_vol);
    w.write_boolean(muted);
    w.write_u32(PA_INVALID_INDEX); // monitor source index
    w.write_string(None); // monitor source name
    w.write_usec(20_000); // latency (20ms)
    w.write_string(Some("soundhop-wasapi")); // driver
    w.write_u32(0); // flags

    if protocol_version >= 13 {
        w.write_proplist(&[("device.description", "SoundHop Windows Speakers")]);
        w.write_usec(20_000); // configured/requested latency
    }
    if protocol_version >= 15 {
        w.write_volume(PA_VOLUME_NORM); // base_volume
        w.write_u32(PA_SINK_RUNNING); // state
        w.write_u32(PA_VOLUME_NORM + 1); // n_volume_steps
        w.write_u32(PA_INVALID_INDEX); // card index
    }
    if protocol_version >= 16 {
        w.write_u32(0); // n_ports
        w.write_string(None); // active_port
    }
    if protocol_version >= 21 {
        w.write_u8(1); // n_formats
        w.write_format_info(PA_ENCODING_PCM, &[]);
    }

    write_packet(PA_CONTROL_CHANNEL, &w.into_bytes())
}

pub fn parse_create_playback_stream(
    r: &mut TagStructReader<'_>,
    protocol_version: u32,
) -> io::Result<CreatePlaybackStreamRequest> {
    if protocol_version < 13 {
        let _name = r.read_string()?;
    }
    let spec = r.read_sample_spec()?;
    let channel_map = r.read_channel_map()?;
    let _sink_index = r.read_u32()?;
    let _sink_name = r.read_string()?;
    let maxlength = r.read_u32()?;
    let corked = r.read_boolean()?;
    let tlength = r.read_u32()?;
    let prebuf = r.read_u32()?;
    let minreq = r.read_u32()?;
    let _syncid = r.read_u32()?;
    let _volume = r.read_cvolume()?;

    if protocol_version >= 12 {
        // no_remap, no_remix, fix_format, fix_rate, fix_channels, no_move, variable_rate
        for _ in 0..7 {
            let _ = r.read_boolean()?;
        }
    }
    if protocol_version >= 13 {
        let _muted = r.read_boolean()?;
        let _adjust_latency = r.read_boolean()?;
        let _proplist = r.read_proplist()?;
    }
    if protocol_version >= 14 {
        let _volume_set = r.read_boolean()?;
        let _early_requests = r.read_boolean()?;
    }
    if protocol_version >= 15 {
        let _muted_set = r.read_boolean()?;
        let _dont_inhibit_auto_suspend = r.read_boolean()?;
        let _fail_on_suspend = r.read_boolean()?;
    }
    if protocol_version >= 17 {
        let _relative_volume = r.read_boolean()?;
    }
    if protocol_version >= 18 {
        let _passthrough = r.read_boolean()?;
    }
    if protocol_version >= 21 {
        let n_formats = r.read_u8()?;
        for _ in 0..n_formats {
            let _ = r.read_format_info()?;
        }
    }

    let buffer_attr = BufferAttr {
        maxlength,
        tlength,
        prebuf,
        minreq,
    }
    .normalized();

    Ok(CreatePlaybackStreamRequest {
        spec,
        channel_map,
        buffer_attr,
        corked,
    })
}

#[allow(clippy::too_many_arguments)]
pub fn make_reply_create_playback_stream(
    tag: u32,
    protocol_version: u32,
    channel: u32,
    stream_index: u32,
    initial_missing: u32,
    buffer_attr: BufferAttr,
    spec: SampleSpec,
    channel_map: &[u8],
) -> Vec<u8> {
    let mut w = TagStructWriter::new();
    w.write_u32(PA_COMMAND_REPLY);
    w.write_u32(tag);
    w.write_u32(channel);
    w.write_u32(stream_index);
    w.write_u32(initial_missing);

    if protocol_version >= 9 {
        w.write_u32(buffer_attr.maxlength);
        w.write_u32(buffer_attr.tlength);
        w.write_u32(buffer_attr.prebuf);
        w.write_u32(buffer_attr.minreq);
    }
    if protocol_version >= 12 {
        w.write_sample_spec(spec);
        let fallback_map = default_channel_map(spec.channels);
        let effective_map = if channel_map.len() == spec.channels as usize {
            channel_map
        } else {
            &fallback_map
        };
        w.write_channel_map(effective_map);
        w.write_u32(0); // sink index (must not be PA_INVALID_INDEX)
        w.write_string(Some("SoundHop-Speakers"));
        w.write_boolean(false); // suspended
    }
    if protocol_version >= 13 {
        w.write_usec(20_000); // configured_sink_latency
    }
    if protocol_version >= 21 {
        w.write_format_info(PA_ENCODING_PCM, &[]);
    }

    write_packet(PA_CONTROL_CHANNEL, &w.into_bytes())
}

/// Build a server->client `PA_COMMAND_REQUEST` packet granting `bytes_requested`
/// bytes of write credit on `channel`.
pub fn make_command_request(channel: u32, bytes_requested: u32) -> Vec<u8> {
    let mut w = TagStructWriter::new();
    w.write_u32(PA_COMMAND_REQUEST);
    w.write_u32(PA_INVALID_INDEX); // server-initiated notification tag = -1
    w.write_u32(channel);
    w.write_u32(bytes_requested);
    write_packet(PA_CONTROL_CHANNEL, &w.into_bytes())
}

pub fn make_reply_playback_latency(
    tag: u32,
    protocol_version: u32,
    client_tv: (u32, u32),
    write_index: i64,
    read_index: i64,
) -> Vec<u8> {
    let mut w = TagStructWriter::new();
    w.write_u32(PA_COMMAND_REPLY);
    w.write_u32(tag);
    w.write_usec(20_000); // sink_usec
    w.write_usec(0); // source_usec
    w.write_boolean(true); // playing
    w.write_timeval(client_tv.0, client_tv.1); // local_time echoed
    w.write_timeval(client_tv.0, client_tv.1); // remote_time
    w.write_s64(write_index);
    w.write_s64(read_index);
    if protocol_version >= 13 {
        w.write_u64(0); // underrun_for
        w.write_u64(read_index.max(0) as u64); // playing_for
    }
    write_packet(PA_CONTROL_CHANNEL, &w.into_bytes())
}

pub fn make_command_auth(tag: u32, protocol_version: u32, cookie: &[u8]) -> Vec<u8> {
    let mut w = TagStructWriter::new();
    w.write_u32(PA_COMMAND_AUTH);
    w.write_u32(tag);
    w.write_u32(protocol_version & PA_PROTOCOL_VERSION_MASK);
    w.write_arbitrary(cookie);
    write_packet(PA_CONTROL_CHANNEL, &w.into_bytes())
}

pub fn make_command_set_client_name(tag: u32, protocol_version: u32, client_name: &str) -> Vec<u8> {
    let mut w = TagStructWriter::new();
    w.write_u32(PA_COMMAND_SET_CLIENT_NAME);
    w.write_u32(tag);
    if protocol_version < 13 {
        w.write_string(Some(client_name));
    } else {
        w.write_proplist(&[("application.name", client_name)]);
    }
    write_packet(PA_CONTROL_CHANNEL, &w.into_bytes())
}

pub fn make_command_create_playback_stream(
    tag: u32,
    protocol_version: u32,
    sink_name: Option<&str>,
    spec: SampleSpec,
) -> Vec<u8> {
    let ch_map = default_channel_map(spec.channels);
    let mut w = TagStructWriter::new();
    w.write_u32(PA_COMMAND_CREATE_PLAYBACK_STREAM);
    w.write_u32(tag);
    if protocol_version < 13 {
        w.write_string(Some("SoundHop-Stream"));
    }
    w.write_sample_spec(spec);
    w.write_channel_map(&ch_map);
    w.write_u32(PA_INVALID_INDEX);
    w.write_string(sink_name.filter(|s| !s.is_empty()));
    w.write_u32(DEFAULT_MAXLENGTH);
    w.write_boolean(false); // corked
    w.write_u32(DEFAULT_TLENGTH);
    w.write_u32(DEFAULT_PREBUF);
    w.write_u32(DEFAULT_MINREQ);
    w.write_u32(0); // syncid
    w.write_cvolume(spec.channels, PA_VOLUME_NORM);

    if protocol_version >= 12 {
        for _ in 0..7 {
            w.write_boolean(false);
        }
    }
    if protocol_version >= 13 {
        w.write_boolean(false); // muted
        w.write_boolean(false); // adjust_latency
        w.write_proplist(&[("media.name", "SoundHop-Stream")]);
    }
    if protocol_version >= 14 {
        w.write_boolean(true); // volume_set
        w.write_boolean(false); // early_requests
    }
    if protocol_version >= 15 {
        w.write_boolean(false); // muted_set
        w.write_boolean(false); // dont_inhibit_auto_suspend
        w.write_boolean(false); // fail_on_suspend
    }
    if protocol_version >= 17 {
        w.write_boolean(false); // relative_volume
    }
    if protocol_version >= 18 {
        w.write_boolean(false); // passthrough
    }
    if protocol_version >= 21 {
        w.write_u8(0); // n_formats = 0 (use sample_spec)
    }

    write_packet(PA_CONTROL_CHANNEL, &w.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_pstream_20_byte_descriptor_layout() {
        let payload = b"hello";
        let raw_ctrl = write_packet(PA_CONTROL_CHANNEL, payload);
        assert_eq!(raw_ctrl.len(), PA_PSTREAM_DESCRIPTOR_SIZE + payload.len());
        // [length=5, channel=0xFFFF_FFFF, offset_hi=0, offset_lo=0, flags=0]
        let expected_hdr: [u8; 20] = [
            0, 0, 0, 5, // length
            0xFF, 0xFF, 0xFF, 0xFF, // channel = PA_CONTROL_CHANNEL
            0, 0, 0, 0, // offset_hi
            0, 0, 0, 0, // offset_lo
            0, 0, 0, 0, // flags
        ];
        assert_eq!(&raw_ctrl[..20], &expected_hdr);
        assert_eq!(&raw_ctrl[20..], payload);

        let raw_audio = write_packet(7, &[0xAA, 0xBB, 0xCC]);
        let expected_audio_hdr: [u8; 20] = [
            0, 0, 0, 3, // length
            0, 0, 0, 7, // channel = 7
            0, 0, 0, 0, // offset_hi
            0, 0, 0, 0, // offset_lo
            0, 0, 0, 0, // flags
        ];
        assert_eq!(&raw_audio[..20], &expected_audio_hdr);

        let mut cur = Cursor::new(raw_audio);
        let pkt = read_packet(&mut cur).unwrap();
        assert!(!pkt.is_control());
        assert_eq!(pkt.channel, 7);
        assert_eq!(pkt.offset, 0);
        assert_eq!(pkt.flags, 0);
        assert_eq!(pkt.data, vec![0xAA, 0xBB, 0xCC]);
    }

    #[test]
    fn test_proplist_wire_layout_and_roundtrip() {
        let mut w = TagStructWriter::new();
        w.write_proplist(&[("application.name", "SoundHop")]);
        let bytes = w.into_bytes();

        let mut expected = Vec::new();
        expected.push(b'P'); // PA_TAG_PROPLIST
        expected.push(b't'); // PA_TAG_STRING
        expected.extend_from_slice(b"application.name\0");
        expected.push(b'L'); // PA_TAG_U32
        expected.extend_from_slice(&9u32.to_be_bytes()); // "SoundHop\0".len() == 9
        expected.push(b'x'); // PA_TAG_ARBITRARY
        expected.extend_from_slice(&9u32.to_be_bytes());
        expected.extend_from_slice(b"SoundHop\0");
        expected.push(b'N'); // PA_TAG_STRING_NULL terminator

        assert_eq!(bytes, expected);

        let mut r = TagStructReader::new(&bytes);
        let props = r.read_proplist().unwrap();
        assert_eq!(props.len(), 1);
        assert_eq!(
            props.get("application.name").unwrap().as_slice(),
            b"SoundHop\0"
        );
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn test_protocol_version_masking_strips_shm_and_memfd_flags() {
        let raw_client = 35 | PA_PROTOCOL_FLAG_SHM | PA_PROTOCOL_FLAG_MEMFD;
        assert_eq!(negotiate_protocol_version(raw_client), Ok(35));
        assert_eq!(
            negotiate_protocol_version(16 | PA_PROTOCOL_FLAG_SHM),
            Ok(16)
        );
        assert_eq!(
            negotiate_protocol_version(99 | PA_PROTOCOL_FLAG_SHM),
            Ok(PA_PROTOCOL_VERSION)
        );
        assert_eq!(negotiate_protocol_version(7), Err(PA_ERR_VERSION));

        // Reply must clear upper flag bits
        let reply_pkt = make_reply_auth(1, 35 | PA_PROTOCOL_FLAG_SHM | PA_PROTOCOL_FLAG_MEMFD);
        let pkt = read_packet(&mut Cursor::new(reply_pkt)).unwrap();
        let mut r = TagStructReader::new(&pkt.data);
        assert_eq!(r.read_u32().unwrap(), PA_COMMAND_REPLY);
        assert_eq!(r.read_u32().unwrap(), 1);
        assert_eq!(r.read_u32().unwrap(), 35);
    }

    #[test]
    fn test_tagstruct_and_packet_roundtrip() {
        let mut w = TagStructWriter::new();
        w.write_u32(42);
        w.write_u64(0x0102_0304_0506_0708);
        w.write_s64(-12345);
        w.write_usec(20_000);
        w.write_timeval(1_700_000_000, 123_456);
        w.write_volume(PA_VOLUME_NORM);
        w.write_string(Some("SoundHop"));
        w.write_string(None);
        w.write_boolean(true);
        w.write_sample_spec(SampleSpec::default());
        w.write_arbitrary(&[1, 2, 3, 4]);
        w.write_format_info(PA_ENCODING_PCM, &[("format.sample_format", "\"s16le\"")]);

        let raw_pkt = write_packet(PA_CONTROL_CHANNEL, &w.into_bytes());
        let mut cur = Cursor::new(raw_pkt);
        let pkt = read_packet(&mut cur).unwrap();
        assert!(pkt.is_control());

        let mut r = TagStructReader::new(&pkt.data);
        assert_eq!(r.read_u32().unwrap(), 42);
        assert_eq!(r.read_u64().unwrap(), 0x0102_0304_0506_0708);
        assert_eq!(r.read_s64().unwrap(), -12345);
        assert_eq!(r.read_usec().unwrap(), 20_000);
        assert_eq!(r.read_timeval().unwrap(), (1_700_000_000, 123_456));
        assert_eq!(r.read_volume().unwrap(), PA_VOLUME_NORM);
        assert_eq!(r.read_string().unwrap().as_deref(), Some("SoundHop"));
        assert_eq!(r.read_string().unwrap(), None);
        assert!(r.read_boolean().unwrap());
        assert_eq!(r.read_sample_spec().unwrap(), SampleSpec::default());
        assert_eq!(r.read_arbitrary().unwrap(), vec![1, 2, 3, 4]);
        let (enc, fmt_props) = r.read_format_info().unwrap();
        assert_eq!(enc, PA_ENCODING_PCM);
        assert!(fmt_props.contains_key("format.sample_format"));
        assert_eq!(r.remaining(), 0);

        // Oversized control packet is rejected without allocating
        let mut oversized_hdr = Vec::new();
        oversized_hdr.extend_from_slice(&((MAX_CONTROL_PACKET_BYTES + 1) as u32).to_be_bytes());
        oversized_hdr.extend_from_slice(&PA_CONTROL_CHANNEL.to_be_bytes());
        oversized_hdr.extend_from_slice(&[0u8; 12]);
        let mut bad_cur = Cursor::new(oversized_hdr);
        assert_eq!(
            read_packet(&mut bad_cur).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn test_create_playback_stream_request_and_reply_roundtrip_across_versions() {
        for ver in [12u32, 13, 14, 15, 16, 17, 18, 21, 35] {
            let spec = SampleSpec::default();
            let req_bytes = make_command_create_playback_stream(7, ver, Some("SoundHop"), spec);
            let req_pkt = read_packet(&mut Cursor::new(req_bytes)).unwrap();
            let mut r = TagStructReader::new(&req_pkt.data);
            assert_eq!(r.read_u32().unwrap(), PA_COMMAND_CREATE_PLAYBACK_STREAM);
            assert_eq!(r.read_u32().unwrap(), 7);
            let parsed = parse_create_playback_stream(&mut r, ver).unwrap();
            assert_eq!(parsed.spec, spec);
            assert_eq!(parsed.channel_map, vec![1, 2]);
            assert_eq!(r.remaining(), 0, "unconsumed bytes at protocol v{ver}");

            let reply_bytes = make_reply_create_playback_stream(
                7,
                ver,
                0,
                0,
                parsed.buffer_attr.tlength,
                parsed.buffer_attr,
                parsed.spec,
                &parsed.channel_map,
            );
            let reply_pkt = read_packet(&mut Cursor::new(reply_bytes)).unwrap();
            let mut rr = TagStructReader::new(&reply_pkt.data);
            assert_eq!(rr.read_u32().unwrap(), PA_COMMAND_REPLY);
            assert_eq!(rr.read_u32().unwrap(), 7);
            assert_eq!(rr.read_u32().unwrap(), 0); // channel
            assert_eq!(rr.read_u32().unwrap(), 0); // stream_index
            assert_eq!(rr.read_u32().unwrap(), parsed.buffer_attr.tlength); // missing
            assert_eq!(rr.read_u32().unwrap(), parsed.buffer_attr.maxlength);
            assert_eq!(rr.read_u32().unwrap(), parsed.buffer_attr.tlength);
            assert_eq!(rr.read_u32().unwrap(), parsed.buffer_attr.prebuf);
            assert_eq!(rr.read_u32().unwrap(), parsed.buffer_attr.minreq);
            assert_eq!(rr.read_sample_spec().unwrap(), spec);
            assert_eq!(rr.read_channel_map().unwrap(), vec![1, 2]);
            assert_eq!(rr.read_u32().unwrap(), 0); // sink_index
            assert_eq!(
                rr.read_string().unwrap().as_deref(),
                Some("SoundHop-Speakers")
            );
            assert!(!rr.read_boolean().unwrap()); // suspended
            if ver >= 13 {
                assert_eq!(rr.read_usec().unwrap(), 20_000);
            }
            if ver >= 21 {
                let (enc, _) = rr.read_format_info().unwrap();
                assert_eq!(enc, PA_ENCODING_PCM);
            }
            assert_eq!(
                rr.remaining(),
                0,
                "unconsumed reply bytes at protocol v{ver}"
            );
        }
    }

    #[test]
    fn test_command_request_encoding() {
        let raw = make_command_request(3, 48_000);
        let pkt = read_packet(&mut Cursor::new(raw)).unwrap();
        assert!(pkt.is_control());
        let mut r = TagStructReader::new(&pkt.data);
        assert_eq!(r.read_u32().unwrap(), PA_COMMAND_REQUEST);
        assert_eq!(r.read_u32().unwrap(), PA_INVALID_INDEX);
        assert_eq!(r.read_u32().unwrap(), 3);
        assert_eq!(r.read_u32().unwrap(), 48_000);
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn test_read_packet_distinguishes_timeout_vs_eof() {
        struct ErrorReader(io::ErrorKind);
        impl Read for ErrorReader {
            fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::new(self.0, "simulated"))
            }
        }

        let err_timeout = read_packet(&mut ErrorReader(io::ErrorKind::TimedOut)).unwrap_err();
        assert_eq!(err_timeout.kind(), io::ErrorKind::TimedOut);

        let err_would_block = read_packet(&mut ErrorReader(io::ErrorKind::WouldBlock)).unwrap_err();
        assert_eq!(err_would_block.kind(), io::ErrorKind::WouldBlock);

        let mut empty = Cursor::new(Vec::<u8>::new());
        let err_eof = read_packet(&mut empty).unwrap_err();
        assert_eq!(err_eof.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn test_tagstruct_read_slice_checked_add_overflow() {
        let data = [PA_TAG_ARBITRARY, 0xFF, 0xFF, 0xFF, 0xFF];
        let mut r = TagStructReader::new(&data);
        let err = r.read_slice(usize::MAX).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);

        // Also advance pos > 0 and test pos + usize::MAX overflow
        let _ = r.read_byte().unwrap();
        let err2 = r.read_slice(usize::MAX).unwrap_err();
        assert_eq!(err2.kind(), io::ErrorKind::UnexpectedEof);
    }
}
