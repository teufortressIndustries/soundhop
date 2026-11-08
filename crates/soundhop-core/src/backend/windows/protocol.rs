//! PulseAudio Native Protocol tagstruct and packet framing codec.

use std::collections::BTreeMap;
use std::io::{self, Read};

use crate::constants::VERSION;

// PulseAudio Command Codes
pub const PA_COMMAND_ERROR: u32 = 0;
pub const PA_COMMAND_TIMEOUT: u32 = 1;
pub const PA_COMMAND_REPLY: u32 = 2;
pub const PA_COMMAND_CREATE_PLAYBACK_STREAM: u32 = 3;
pub const PA_COMMAND_DELETE_PLAYBACK_STREAM: u32 = 4;
pub const PA_COMMAND_CREATE_RECORD_STREAM: u32 = 5;
pub const PA_COMMAND_DELETE_RECORD_STREAM: u32 = 6;
pub const PA_COMMAND_AUTH: u32 = 8;
pub const PA_COMMAND_REQUEST: u32 = 9;
pub const PA_COMMAND_DRAIN_PLAYBACK_STREAM: u32 = 14;
pub const PA_COMMAND_GET_SERVER_INFO: u32 = 16;
pub const PA_COMMAND_SET_SINK_VOLUME: u32 = 17;
pub const PA_COMMAND_SET_SOURCE_VOLUME: u32 = 18;
pub const PA_COMMAND_SET_CLIENT_NAME: u32 = 19;
pub const PA_COMMAND_SET_SINK_MUTE: u32 = 20;
pub const PA_COMMAND_SET_SOURCE_MUTE: u32 = 21;
pub const PA_COMMAND_SUBSCRIBE: u32 = 23;
pub const PA_COMMAND_SET_DEFAULT_SINK: u32 = 27;

// PulseAudio Tag Types
pub const PA_TAG_INVALID: u8 = 0;
pub const PA_TAG_STRING: u8 = b's';
pub const PA_TAG_STRING_NULL: u8 = b'N';
pub const PA_TAG_U32: u8 = b'u';
pub const PA_TAG_U8: u8 = b'B';
pub const PA_TAG_U64: u8 = b't';
pub const PA_TAG_S64: u8 = b'T';
pub const PA_TAG_SAMPLE_SPEC: u8 = b'a';
pub const PA_TAG_ARBITRARY: u8 = b'x';
pub const PA_TAG_BOOLEAN_TRUE: u8 = b'1';
pub const PA_TAG_BOOLEAN_FALSE: u8 = b'0';
pub const PA_TAG_TIMEVAL: u8 = b'e';
pub const PA_TAG_USEC: u8 = b'U';
pub const PA_TAG_CHANNEL_MAP: u8 = b'm';
pub const PA_TAG_CVOLUME: u8 = b'v';
pub const PA_TAG_PROPLIST: u8 = b'P';
pub const PA_TAG_VOLUME: u8 = b'V';

// Sample Formats
pub const PA_SAMPLE_U8: u8 = 0;
pub const PA_SAMPLE_S16LE: u8 = 3;
pub const PA_SAMPLE_FLOAT32LE: u8 = 5;

pub const PA_CONTROL_CHANNEL: u32 = 0xFFFF_FFFF;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub channel: u32,
    pub data: Vec<u8>,
}

impl Packet {
    pub fn is_control(&self) -> bool {
        self.channel == PA_CONTROL_CHANNEL
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
        if self.pos + len > self.data.len() {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "EOF"));
        }
        let s = &self.data[self.pos..self.pos + len];
        self.pos += len;
        Ok(s)
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
                "Expected PA_TAG_STRING",
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
        loop {
            let mut key_bytes = Vec::new();
            loop {
                let b = self.read_byte()?;
                if b == 0 {
                    break;
                }
                key_bytes.push(b);
            }
            if key_bytes.is_empty() {
                break;
            }
            let key = String::from_utf8_lossy(&key_bytes).into_owned();
            let raw_len = self.read_slice(4)?;
            let len = u32::from_be_bytes([raw_len[0], raw_len[1], raw_len[2], raw_len[3]]) as usize;
            let val = self.read_slice(len)?;
            props.insert(key, val.to_vec());
        }
        Ok(props)
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

    pub fn write_u8(&mut self, value: u8) {
        self.buf.push(PA_TAG_U8);
        self.buf.push(value);
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
            self.buf.extend_from_slice(k.as_bytes());
            self.buf.push(0);
            let mut val_bytes = v.as_bytes().to_vec();
            if !val_bytes.ends_with(&[0]) {
                val_bytes.push(0);
            }
            self.buf
                .extend_from_slice(&(val_bytes.len() as u32).to_be_bytes());
            self.buf.extend_from_slice(&val_bytes);
        }
        self.buf.push(0);
    }
}

pub fn read_packet<R: Read>(stream: &mut R) -> Option<Packet> {
    let mut header = [0u8; 8];
    stream.read_exact(&mut header).ok()?;
    let channel = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
    let length = u32::from_be_bytes([header[4], header[5], header[6], header[7]]) as usize;
    if length > 16 * 1024 * 1024 {
        return None;
    }
    let mut data = vec![0u8; length];
    stream.read_exact(&mut data).ok()?;
    Some(Packet { channel, data })
}

pub fn write_packet(channel: u32, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + data.len());
    out.extend_from_slice(&channel.to_be_bytes());
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(data);
    out
}

pub fn make_reply_auth(tag: u32, protocol_version: u32) -> Vec<u8> {
    let mut w = TagStructWriter::new();
    w.write_u32(PA_COMMAND_REPLY);
    w.write_u32(tag);
    w.write_u32(protocol_version);
    write_packet(PA_CONTROL_CHANNEL, &w.into_bytes())
}

pub fn make_reply_ok(tag: u32) -> Vec<u8> {
    let mut w = TagStructWriter::new();
    w.write_u32(PA_COMMAND_REPLY);
    w.write_u32(tag);
    write_packet(PA_CONTROL_CHANNEL, &w.into_bytes())
}

pub fn make_reply_server_info(tag: u32) -> Vec<u8> {
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
    w.write_u32(0);
    write_packet(PA_CONTROL_CHANNEL, &w.into_bytes())
}

pub fn make_reply_create_playback_stream(
    tag: u32,
    stream_index: u32,
    channel: u32,
    spec: SampleSpec,
) -> Vec<u8> {
    let mut w = TagStructWriter::new();
    w.write_u32(PA_COMMAND_REPLY);
    w.write_u32(tag);
    w.write_u32(stream_index);
    w.write_u32(channel);
    w.write_u32(4_194_304);
    w.write_u32(96_000);
    w.write_u32(48_000);
    w.write_u32(4_800);
    w.write_sample_spec(spec);
    let ch_map: Vec<u8> = (1..=spec.channels).collect();
    w.write_channel_map(&ch_map);
    w.write_cvolume(spec.channels, 65_536);
    w.write_boolean(false);
    write_packet(PA_CONTROL_CHANNEL, &w.into_bytes())
}

pub fn make_command_auth(tag: u32, protocol_version: u32, cookie: &[u8]) -> Vec<u8> {
    let mut w = TagStructWriter::new();
    w.write_u32(PA_COMMAND_AUTH);
    w.write_u32(tag);
    w.write_u32(protocol_version);
    w.write_arbitrary(cookie);
    write_packet(PA_CONTROL_CHANNEL, &w.into_bytes())
}

pub fn make_command_set_client_name(tag: u32, client_name: &str) -> Vec<u8> {
    let mut w = TagStructWriter::new();
    w.write_u32(PA_COMMAND_SET_CLIENT_NAME);
    w.write_u32(tag);
    w.write_proplist(&[("application.name", client_name)]);
    write_packet(PA_CONTROL_CHANNEL, &w.into_bytes())
}

pub fn make_command_create_playback_stream(
    tag: u32,
    sink_name: Option<&str>,
    spec: SampleSpec,
) -> Vec<u8> {
    let mut w = TagStructWriter::new();
    w.write_u32(PA_COMMAND_CREATE_PLAYBACK_STREAM);
    w.write_u32(tag);
    w.write_sample_spec(spec);
    w.write_channel_map(&[1, 2]);
    w.write_u32(0xFFFF_FFFF);
    w.write_string(sink_name.filter(|s| !s.is_empty()));
    w.write_u32(4_194_304);
    w.write_boolean(false);
    w.write_u32(96_000);
    w.write_u32(48_000);
    w.write_u32(4_800);
    w.write_u32(0);
    w.write_cvolume(2, 65_536);
    for _ in 0..8 {
        w.write_boolean(false);
    }
    w.write_proplist(&[("media.name", "SoundHop-Stream")]);
    write_packet(PA_CONTROL_CHANNEL, &w.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_tagstruct_and_packet_roundtrip() {
        let mut w = TagStructWriter::new();
        w.write_u32(42);
        w.write_string(Some("SoundHop"));
        w.write_string(None);
        w.write_boolean(true);
        w.write_sample_spec(SampleSpec::default());
        w.write_arbitrary(&[1, 2, 3, 4]);

        let raw_pkt = write_packet(PA_CONTROL_CHANNEL, &w.into_bytes());
        let mut cur = Cursor::new(raw_pkt);
        let pkt = read_packet(&mut cur).unwrap();
        assert!(pkt.is_control());

        let mut r = TagStructReader::new(&pkt.data);
        assert_eq!(r.read_u32().unwrap(), 42);
        assert_eq!(r.read_string().unwrap().as_deref(), Some("SoundHop"));
        assert_eq!(r.read_string().unwrap(), None);
        assert!(r.read_boolean().unwrap());
        assert_eq!(r.read_sample_spec().unwrap(), SampleSpec::default());
        assert_eq!(r.read_arbitrary().unwrap(), vec![1, 2, 3, 4]);
        assert_eq!(r.remaining(), 0);
    }
}
