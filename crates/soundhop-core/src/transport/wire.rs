//! SHT4 control channel wire protocol framing, handshake messages, and encrypted control payloads.

use std::io::{Read, Write};

use crate::transport::{
    TransportError, TransportResult, AEAD_TAG_LEN, FRAME_HEADER_LEN, MAX_PAYLOAD_LEN, SHT3_MAGIC,
    SHT4_VERSION,
};

/// Frame flag: stream discontinuity (e.g. after reconnect or device reset).
pub const FLAG_DISCONTINUITY: u8 = 0x01;
/// SRTP cipher/authentication suite identifier: `AES_128_CM_HMAC_SHA1_80` (RFC 3711).
pub const SRTP_SUITE_AES128_ICM_HMAC_SHA1_80: u8 = 1;

const HANDSHAKE_MSG_CLIENT_HELLO: u8 = 0x01;
const HANDSHAKE_MSG_SERVER_HELLO: u8 = 0x02;
const MAX_DEVICE_ID_LEN: usize = 256;
const MAC_LEN: usize = 32;

/// Encrypted control-channel frame type discriminant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameType {
    Control = 0x10,
    Ping = 0x11,
    Pong = 0x12,
    Close = 0xFF,
}

impl FrameType {
    pub fn from_u8(v: u8) -> TransportResult<Self> {
        match v {
            0x10 => Ok(Self::Control),
            0x11 => Ok(Self::Ping),
            0x12 => Ok(Self::Pong),
            0xFF => Ok(Self::Close),
            other => Err(TransportError::Wire(format!(
                "unknown frame type: 0x{other:02X}"
            ))),
        }
    }
}

/// 16-byte plaintext frame header (also passed verbatim as ChaCha20-Poly1305 AAD).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub frame_type: FrameType,
    pub flags: u8,
    pub payload_len: u16,
    pub seq: u64,
    pub timestamp_samples: u32,
}

impl FrameHeader {
    /// Serialize into exact 16-byte network-order representation.
    pub fn to_bytes(&self) -> [u8; FRAME_HEADER_LEN] {
        let mut buf = [0u8; FRAME_HEADER_LEN];
        buf[0] = self.frame_type as u8;
        buf[1] = self.flags;
        buf[2..4].copy_from_slice(&self.payload_len.to_be_bytes());
        buf[4..12].copy_from_slice(&self.seq.to_be_bytes());
        buf[12..16].copy_from_slice(&self.timestamp_samples.to_be_bytes());
        buf
    }

    /// Parse a 16-byte network-order header and validate `payload_len <= MAX_PAYLOAD_LEN`.
    pub fn from_bytes(buf: &[u8; FRAME_HEADER_LEN]) -> TransportResult<Self> {
        let frame_type = FrameType::from_u8(buf[0])?;
        let flags = buf[1];
        let payload_len = u16::from_be_bytes([buf[2], buf[3]]);
        if usize::from(payload_len) > MAX_PAYLOAD_LEN {
            return Err(TransportError::PayloadTooLarge {
                len: usize::from(payload_len),
                max: MAX_PAYLOAD_LEN,
            });
        }
        let seq = u64::from_be_bytes([
            buf[4], buf[5], buf[6], buf[7], buf[8], buf[9], buf[10], buf[11],
        ]);
        let timestamp_samples = u32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]]);
        Ok(Self {
            frame_type,
            flags,
            payload_len,
            seq,
            timestamp_samples,
        })
    }
}

/// An encrypted frame on the wire: 16B plaintext header + ciphertext + 16B Poly1305 tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncryptedFrame {
    pub header: FrameHeader,
    pub ciphertext_and_tag: Vec<u8>,
}

impl EncryptedFrame {
    /// Write the full frame (header + ciphertext + tag) to `w` in a single atomic `write_all`
    /// reusing `scratch`.
    pub fn write_into_buf(&self, scratch: &mut Vec<u8>, w: &mut impl Write) -> TransportResult<()> {
        let expected_body = usize::from(self.header.payload_len) + AEAD_TAG_LEN;
        if self.ciphertext_and_tag.len() != expected_body {
            return Err(TransportError::Wire(format!(
                "encrypted frame body len {} does not match header payload_len+tag {}",
                self.ciphertext_and_tag.len(),
                expected_body
            )));
        }
        let needed = FRAME_HEADER_LEN + expected_body;
        scratch.clear();
        scratch.reserve(needed);
        scratch.extend_from_slice(&self.header.to_bytes());
        scratch.extend_from_slice(&self.ciphertext_and_tag);
        w.write_all(scratch)?;
        Ok(())
    }

    /// Write the full frame (header + ciphertext + tag) to `w` in a single atomic write.
    pub fn write_to<W: Write>(&self, mut w: &mut W) -> TransportResult<()> {
        let expected_body = usize::from(self.header.payload_len) + AEAD_TAG_LEN;
        let mut frame_buf = Vec::with_capacity(FRAME_HEADER_LEN + expected_body);
        self.write_into_buf(&mut frame_buf, &mut w)
    }

    /// Read a framed packet from `r`, enforcing `MAX_PAYLOAD_LEN` prior to allocating.
    ///
    /// Timeout-safe: only returns `WouldBlock`/`TimedOut` if **zero** bytes of the frame
    /// header were read before the timeout. Once the first byte of a frame arrives, it
    /// completes reading the remaining header and payload bytes so a short socket read
    /// timeout can never discard a partial header and desynchronize the AEAD stream.
    pub fn read_from<R: Read>(r: &mut R) -> TransportResult<Self> {
        let mut hdr_buf = [0u8; FRAME_HEADER_LEN];
        read_frame_bytes(r, &mut hdr_buf, false)?;
        let header = FrameHeader::from_bytes(&hdr_buf)?;
        let body_len = usize::from(header.payload_len) + AEAD_TAG_LEN;
        let mut ciphertext_and_tag = vec![0u8; body_len];
        read_frame_bytes(r, &mut ciphertext_and_tag, true)?;
        Ok(Self {
            header,
            ciphertext_and_tag,
        })
    }
}

fn read_frame_bytes<R: Read>(r: &mut R, buf: &mut [u8], mid_frame: bool) -> TransportResult<()> {
    let mut offset = 0usize;
    let mut timeout_retries = 0u32;
    while offset < buf.len() {
        match r.read(&mut buf[offset..]) {
            Ok(0) => {
                return Err(TransportError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "early EOF while reading SHT4 frame",
                )));
            }
            Ok(n) => {
                offset += n;
                timeout_retries = 0;
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if !mid_frame && offset == 0 {
                    return Err(TransportError::Io(e));
                }
                timeout_retries += 1;
                if timeout_retries > 250 {
                    return Err(TransportError::Io(e));
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(e) => return Err(TransportError::Io(e)),
        }
    }
    Ok(())
}

/// Decrypted control-channel frame returned by server/client connections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecryptedFrame {
    pub header: FrameHeader,
    pub payload: Vec<u8>,
}

/// Audio codec identifier negotiated in the v4 handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CodecId {
    Opus = 1,
    PcmS16Le = 2,
}

impl CodecId {
    pub fn from_u8(v: u8) -> TransportResult<Self> {
        match v {
            1 => Ok(Self::Opus),
            2 => Ok(Self::PcmS16Le),
            other => Err(TransportError::Wire(format!("unknown codec id: {other}"))),
        }
    }
}

/// Standard audio quality/latency profiles for SoundHop SRTP/Opus streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AudioProfile {
    /// Default: Opus 48 kHz stereo, 20 ms frames (960 samples/ch), 128 kbps constrained VBR.
    #[default]
    OpusBalanced,
    /// Low-latency: Opus 48 kHz stereo, 10 ms frames (480 samples/ch), 128 kbps constrained VBR.
    OpusLowLatency,
    /// Low-bandwidth: Opus 48 kHz stereo, 20 ms frames (960 samples/ch), 64 kbps constrained VBR.
    OpusLowBandwidth,
    /// Uncompressed lossless PCM: 48 kHz stereo L16 over SRTP.
    PcmLossless,
}

impl AudioProfile {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpusBalanced => "opus-balanced",
            Self::OpusLowLatency => "opus-low-latency",
            Self::OpusLowBandwidth => "opus-low-bandwidth",
            Self::PcmLossless => "pcm-lossless",
        }
    }

    pub fn codec_id(self) -> CodecId {
        match self {
            Self::OpusBalanced | Self::OpusLowLatency | Self::OpusLowBandwidth => CodecId::Opus,
            Self::PcmLossless => CodecId::PcmS16Le,
        }
    }

    pub fn sample_rate(self) -> u32 {
        48_000
    }

    pub fn channels(self) -> u8 {
        2
    }

    pub fn frame_ms(self) -> u8 {
        match self {
            Self::OpusLowLatency => 10,
            Self::OpusBalanced | Self::OpusLowBandwidth | Self::PcmLossless => 20,
        }
    }

    pub fn bitrate_kbps(self) -> u16 {
        match self {
            Self::OpusBalanced | Self::OpusLowLatency => 128,
            Self::OpusLowBandwidth => 64,
            Self::PcmLossless => 1536,
        }
    }

    /// Number of samples per channel in one frame (960 for 20ms @ 48kHz, 480 for 10ms).
    pub fn frame_samples_per_channel(self) -> usize {
        (self.sample_rate() as usize * usize::from(self.frame_ms())) / 1000
    }

    /// Total interleaved `i16` samples in one frame (`frame_samples_per_channel * channels`).
    pub fn interleaved_samples_per_frame(self) -> usize {
        self.frame_samples_per_channel() * usize::from(self.channels())
    }

    pub fn to_audio_params(self, jitter_ms: u16) -> AudioParams {
        AudioParams {
            codec: self.codec_id(),
            sample_rate: self.sample_rate(),
            channels: self.channels(),
            frame_ms: self.frame_ms(),
            bitrate_kbps: self.bitrate_kbps(),
            jitter_ms,
        }
    }

    pub fn from_audio_params(params: &AudioParams) -> TransportResult<Self> {
        if params.sample_rate != 48_000 || params.channels != 2 {
            return Err(TransportError::UnsupportedProfile(format!(
                "unsupported format: {}Hz {}ch (expected 48000Hz 2ch)",
                params.sample_rate, params.channels
            )));
        }
        match (params.codec, params.frame_ms, params.bitrate_kbps) {
            (CodecId::PcmS16Le, 20, _) => Ok(Self::PcmLossless),
            (CodecId::Opus, 10, _) => Ok(Self::OpusLowLatency),
            (CodecId::Opus, 20, kbps) if kbps <= 80 => Ok(Self::OpusLowBandwidth),
            (CodecId::Opus, 20, _) => Ok(Self::OpusBalanced),
            _ => Err(TransportError::UnsupportedProfile(format!(
                "unsupported AudioParams: {params:?}"
            ))),
        }
    }
}

/// Negotiated audio stream parameters carried in `ClientHello` and `ServerHello`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioParams {
    pub codec: CodecId,
    pub sample_rate: u32,
    pub channels: u8,
    pub frame_ms: u8,
    pub bitrate_kbps: u16,
    pub jitter_ms: u16,
}

impl Default for AudioParams {
    fn default() -> Self {
        Self {
            codec: CodecId::Opus,
            sample_rate: 48_000,
            channels: 2,
            frame_ms: 20,
            bitrate_kbps: 128,
            jitter_ms: 40,
        }
    }
}

impl AudioParams {
    pub const WIRE_LEN: usize = 11;

    pub fn encode_into(&self, out: &mut Vec<u8>) {
        out.push(self.codec as u8);
        out.extend_from_slice(&self.sample_rate.to_be_bytes());
        out.push(self.channels);
        out.push(self.frame_ms);
        out.extend_from_slice(&self.bitrate_kbps.to_be_bytes());
        out.extend_from_slice(&self.jitter_ms.to_be_bytes());
    }

    pub fn decode_from(bytes: &[u8]) -> TransportResult<Self> {
        if bytes.len() < Self::WIRE_LEN {
            return Err(TransportError::Wire("truncated AudioParams".into()));
        }
        let codec = CodecId::from_u8(bytes[0])?;
        let sample_rate = u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]);
        let channels = bytes[5];
        let frame_ms = bytes[6];
        let bitrate_kbps = u16::from_be_bytes([bytes[7], bytes[8]]);
        let jitter_ms = u16::from_be_bytes([bytes[9], bytes[10]]);
        if sample_rate == 0 || channels == 0 || frame_ms == 0 {
            return Err(TransportError::Wire(
                "invalid zero sample_rate, channels, or frame_ms in AudioParams".into(),
            ));
        }
        Ok(Self {
            codec,
            sample_rate,
            channels,
            frame_ms,
            bitrate_kbps,
            jitter_ms,
        })
    }
}

/// Handshake response status code sent in `ServerHello`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum HandshakeStatus {
    Ok = 0x00,
    AuthFailed = 0x01,
    UnsupportedVersion = 0x02,
    UnsupportedCodec = 0x03,
    InternalError = 0xFF,
}

impl HandshakeStatus {
    pub fn from_u8(v: u8) -> TransportResult<Self> {
        match v {
            0x00 => Ok(Self::Ok),
            0x01 => Ok(Self::AuthFailed),
            0x02 => Ok(Self::UnsupportedVersion),
            0x03 => Ok(Self::UnsupportedCodec),
            0xFF => Ok(Self::InternalError),
            other => Err(TransportError::Wire(format!(
                "unknown handshake status: 0x{other:02X}"
            ))),
        }
    }
}

/// SHT4 `ClientHello` message (Client -> Server).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientHello {
    pub version: u8,
    pub flags: u8,
    pub client_device_id: String,
    pub client_pk: [u8; 32],
    pub client_nonce: [u8; 16],
    pub audio_params: AudioParams,
    /// Fresh random non-zero, non-`u32::MAX` RTP SSRC chosen by the sender.
    pub sender_ssrc: u32,
    /// Requested SRTP cipher/auth suite (`SRTP_SUITE_AES128_ICM_HMAC_SHA1_80 = 1`).
    pub srtp_suite: u8,
    pub mac_c: [u8; MAC_LEN],
}

impl ClientHello {
    /// Canonical transcript bytes covered by `mac_c`, `mac_s`, and HKDF key derivation.
    pub fn encode_transcript(&self) -> TransportResult<Vec<u8>> {
        let id_bytes = self.client_device_id.as_bytes();
        if id_bytes.is_empty() || id_bytes.len() > MAX_DEVICE_ID_LEN {
            return Err(TransportError::Wire(format!(
                "invalid client_device_id length: {}",
                id_bytes.len()
            )));
        }
        let mut out = Vec::with_capacity(
            4 + 2 + 2 + id_bytes.len() + 32 + 16 + AudioParams::WIRE_LEN + 4 + 1,
        );
        out.extend_from_slice(&SHT3_MAGIC);
        out.push(self.version);
        out.push(self.flags);
        out.extend_from_slice(&(id_bytes.len() as u16).to_be_bytes());
        out.extend_from_slice(id_bytes);
        out.extend_from_slice(&self.client_pk);
        out.extend_from_slice(&self.client_nonce);
        self.audio_params.encode_into(&mut out);
        out.extend_from_slice(&self.sender_ssrc.to_be_bytes());
        out.push(self.srtp_suite);
        Ok(out)
    }

    /// Write framed `ClientHello` to `w`.
    pub fn write_to<W: Write>(&self, w: &mut W) -> TransportResult<()> {
        let body = self.encode_transcript()?;
        let total_len = body.len() + MAC_LEN;
        if total_len > MAX_PAYLOAD_LEN {
            return Err(TransportError::PayloadTooLarge {
                len: total_len,
                max: MAX_PAYLOAD_LEN,
            });
        }
        w.write_all(&SHT3_MAGIC)?;
        w.write_all(&[HANDSHAKE_MSG_CLIENT_HELLO])?;
        w.write_all(&(total_len as u16).to_be_bytes())?;
        w.write_all(&body)?;
        w.write_all(&self.mac_c)?;
        Ok(())
    }

    /// Read framed `ClientHello` from `r`, returning both the parsed struct and exact transcript bytes.
    ///
    /// Tolerates older v3 `ClientHello` layouts (which omit `sender_ssrc` and `srtp_suite`) so
    /// the server can inspect `version != SHT4_VERSION` and reply cleanly with
    /// `HandshakeStatus::UnsupportedVersion`.
    pub fn read_from<R: Read>(r: &mut R) -> TransportResult<(Self, Vec<u8>)> {
        let raw = read_handshake_envelope(r, HANDSHAKE_MSG_CLIENT_HELLO)?;
        let min_v3_len = MAC_LEN + 4 + 2 + 2 + 32 + 16 + AudioParams::WIRE_LEN;
        if raw.len() < min_v3_len {
            return Err(TransportError::Wire("truncated ClientHello".into()));
        }
        let body_len = raw.len() - MAC_LEN;
        let transcript = raw[..body_len].to_vec();
        let mut mac_c = [0u8; MAC_LEN];
        mac_c.copy_from_slice(&raw[body_len..]);

        let mut cursor = 0usize;
        if &transcript[cursor..cursor + 4] != SHT3_MAGIC.as_slice() {
            return Err(TransportError::Wire("invalid ClientHello magic".into()));
        }
        cursor += 4;
        let version = transcript[cursor];
        let flags = transcript[cursor + 1];
        cursor += 2;
        let id_len = usize::from(u16::from_be_bytes([
            transcript[cursor],
            transcript[cursor + 1],
        ]));
        cursor += 2;
        if id_len == 0 || id_len > MAX_DEVICE_ID_LEN || cursor + id_len > transcript.len() {
            return Err(TransportError::Wire(
                "invalid client_device_id length in ClientHello".into(),
            ));
        }
        let client_device_id = String::from_utf8(transcript[cursor..cursor + id_len].to_vec())
            .map_err(|_| TransportError::Wire("non-UTF8 client_device_id".into()))?;
        cursor += id_len;

        let remaining = transcript.len().saturating_sub(cursor);
        let v3_tail = 32 + 16 + AudioParams::WIRE_LEN;
        let v4_tail = v3_tail + 4 + 1;

        if version == SHT4_VERSION {
            if remaining != v4_tail {
                return Err(TransportError::Wire(
                    "unexpected trailing or missing bytes in v4 ClientHello".into(),
                ));
            }
        } else if remaining != v3_tail && remaining != v4_tail {
            return Err(TransportError::Wire(
                "unexpected trailing or missing bytes in ClientHello".into(),
            ));
        }

        let mut client_pk = [0u8; 32];
        client_pk.copy_from_slice(&transcript[cursor..cursor + 32]);
        cursor += 32;
        let mut client_nonce = [0u8; 16];
        client_nonce.copy_from_slice(&transcript[cursor..cursor + 16]);
        cursor += 16;
        let audio_params =
            AudioParams::decode_from(&transcript[cursor..cursor + AudioParams::WIRE_LEN])?;
        cursor += AudioParams::WIRE_LEN;

        let (sender_ssrc, srtp_suite) = if cursor + 5 == transcript.len() {
            let ssrc = u32::from_be_bytes([
                transcript[cursor],
                transcript[cursor + 1],
                transcript[cursor + 2],
                transcript[cursor + 3],
            ]);
            let suite = transcript[cursor + 4];
            (ssrc, suite)
        } else {
            (0, 0)
        };

        Ok((
            Self {
                version,
                flags,
                client_device_id,
                client_pk,
                client_nonce,
                audio_params,
                sender_ssrc,
                srtp_suite,
                mac_c,
            },
            transcript,
        ))
    }
}

/// SHT4 `ServerHello` message (Server -> Client).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerHello {
    pub status: HandshakeStatus,
    pub server_device_id: String,
    pub server_pk: [u8; 32],
    pub server_nonce: [u8; 16],
    pub audio_params: AudioParams,
    pub initial_volume: u8,
    pub initial_muted: bool,
    /// Actual bound UDP port on which the server's SRTP receiver is listening.
    pub rtp_port: u16,
    /// Negotiated SRTP cipher/auth suite (`SRTP_SUITE_AES128_ICM_HMAC_SHA1_80 = 1`).
    pub srtp_suite: u8,
    pub mac_s: [u8; MAC_LEN],
}

impl ServerHello {
    /// Construct an error `ServerHello` (e.g., `AuthFailed`, `UnsupportedVersion`) with zeroed key material.
    pub fn error(status: HandshakeStatus, server_device_id: impl Into<String>) -> Self {
        Self {
            status,
            server_device_id: server_device_id.into(),
            server_pk: [0u8; 32],
            server_nonce: [0u8; 16],
            audio_params: AudioParams::default(),
            initial_volume: 100,
            initial_muted: false,
            rtp_port: 0,
            srtp_suite: 0,
            mac_s: [0u8; MAC_LEN],
        }
    }

    /// Canonical bytes of `ServerHello` (excluding `mac_s`) appended to `ClientHello` transcript.
    pub fn encode_body(&self) -> TransportResult<Vec<u8>> {
        let id_bytes = self.server_device_id.as_bytes();
        if id_bytes.is_empty() || id_bytes.len() > MAX_DEVICE_ID_LEN {
            return Err(TransportError::Wire(format!(
                "invalid server_device_id length: {}",
                id_bytes.len()
            )));
        }
        let mut out = Vec::with_capacity(
            4 + 1 + 2 + id_bytes.len() + 32 + 16 + AudioParams::WIRE_LEN + 2 + 2 + 1,
        );
        out.extend_from_slice(&SHT3_MAGIC);
        out.push(self.status as u8);
        out.extend_from_slice(&(id_bytes.len() as u16).to_be_bytes());
        out.extend_from_slice(id_bytes);
        out.extend_from_slice(&self.server_pk);
        out.extend_from_slice(&self.server_nonce);
        self.audio_params.encode_into(&mut out);
        out.push(self.initial_volume);
        out.push(u8::from(self.initial_muted));
        out.extend_from_slice(&self.rtp_port.to_be_bytes());
        out.push(self.srtp_suite);
        Ok(out)
    }

    /// Write framed `ServerHello` to `w`.
    pub fn write_to<W: Write>(&self, w: &mut W) -> TransportResult<()> {
        let body = self.encode_body()?;
        let total_len = body.len() + MAC_LEN;
        if total_len > MAX_PAYLOAD_LEN {
            return Err(TransportError::PayloadTooLarge {
                len: total_len,
                max: MAX_PAYLOAD_LEN,
            });
        }
        w.write_all(&SHT3_MAGIC)?;
        w.write_all(&[HANDSHAKE_MSG_SERVER_HELLO])?;
        w.write_all(&(total_len as u16).to_be_bytes())?;
        w.write_all(&body)?;
        w.write_all(&self.mac_s)?;
        Ok(())
    }

    /// Read framed `ServerHello` from `r`, returning both the parsed struct and exact body bytes.
    ///
    /// Accepts legacy v3 error `ServerHello` responses (without `rtp_port`/`srtp_suite`) when
    /// `status != HandshakeStatus::Ok` so v4 clients speaking to an older v3 server still receive
    /// a clean `UnsupportedVersion` error.
    pub fn read_from<R: Read>(r: &mut R) -> TransportResult<(Self, Vec<u8>)> {
        let raw = read_handshake_envelope(r, HANDSHAKE_MSG_SERVER_HELLO)?;
        let min_v3_len = MAC_LEN + 4 + 1 + 2 + 32 + 16 + AudioParams::WIRE_LEN + 2;
        if raw.len() < min_v3_len {
            return Err(TransportError::Wire("truncated ServerHello".into()));
        }
        let body_len = raw.len() - MAC_LEN;
        let body = raw[..body_len].to_vec();
        let mut mac_s = [0u8; MAC_LEN];
        mac_s.copy_from_slice(&raw[body_len..]);

        let mut cursor = 0usize;
        if &body[cursor..cursor + 4] != SHT3_MAGIC.as_slice() {
            return Err(TransportError::Wire("invalid ServerHello magic".into()));
        }
        cursor += 4;
        let status = HandshakeStatus::from_u8(body[cursor])?;
        cursor += 1;
        let id_len = usize::from(u16::from_be_bytes([body[cursor], body[cursor + 1]]));
        cursor += 2;
        if id_len == 0 || id_len > MAX_DEVICE_ID_LEN || cursor + id_len > body.len() {
            return Err(TransportError::Wire(
                "invalid server_device_id length in ServerHello".into(),
            ));
        }
        let server_device_id = String::from_utf8(body[cursor..cursor + id_len].to_vec())
            .map_err(|_| TransportError::Wire("non-UTF8 server_device_id".into()))?;
        cursor += id_len;

        let remaining = body.len().saturating_sub(cursor);
        let v3_tail = 32 + 16 + AudioParams::WIRE_LEN + 2;
        let v4_tail = v3_tail + 2 + 1;

        if status == HandshakeStatus::Ok {
            if remaining != v4_tail {
                return Err(TransportError::Wire(
                    "unexpected trailing or missing bytes in v4 ServerHello".into(),
                ));
            }
        } else if remaining != v3_tail && remaining != v4_tail {
            return Err(TransportError::Wire(
                "unexpected trailing or missing bytes in ServerHello".into(),
            ));
        }

        let mut server_pk = [0u8; 32];
        server_pk.copy_from_slice(&body[cursor..cursor + 32]);
        cursor += 32;
        let mut server_nonce = [0u8; 16];
        server_nonce.copy_from_slice(&body[cursor..cursor + 16]);
        cursor += 16;
        let audio_params = AudioParams::decode_from(&body[cursor..cursor + AudioParams::WIRE_LEN])?;
        cursor += AudioParams::WIRE_LEN;
        let initial_volume = body[cursor];
        let initial_muted = body[cursor + 1] != 0;
        cursor += 2;

        let (rtp_port, srtp_suite) = if cursor + 3 == body.len() {
            let port = u16::from_be_bytes([body[cursor], body[cursor + 1]]);
            let suite = body[cursor + 2];
            (port, suite)
        } else {
            (0, 0)
        };

        Ok((
            Self {
                status,
                server_device_id,
                server_pk,
                server_nonce,
                audio_params,
                initial_volume,
                initial_muted,
                rtp_port,
                srtp_suite,
                mac_s,
            },
            body,
        ))
    }
}

fn read_handshake_envelope<R: Read>(r: &mut R, expected_type: u8) -> TransportResult<Vec<u8>> {
    let mut prefix = [0u8; 7];
    r.read_exact(&mut prefix)?;
    if prefix[0..4] != SHT3_MAGIC {
        return Err(TransportError::Wire("invalid SHT3 magic".into()));
    }
    if prefix[4] != expected_type {
        return Err(TransportError::Wire(format!(
            "unexpected handshake message type 0x{:02X} (expected 0x{:02X})",
            prefix[4], expected_type
        )));
    }
    let len = usize::from(u16::from_be_bytes([prefix[5], prefix[6]]));
    if len > MAX_PAYLOAD_LEN {
        return Err(TransportError::PayloadTooLarge {
            len,
            max: MAX_PAYLOAD_LEN,
        });
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok(payload)
}

/// Encrypted control messages carried inside `FrameType::Control` frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlMessage {
    SetVolume(u8),
    SetMute(bool),
    VolumeState { volume: u8, muted: bool },
    DelayOffsetMs(i16),
}

impl ControlMessage {
    pub fn encode(&self) -> Vec<u8> {
        match *self {
            Self::SetVolume(vol) => vec![0x01, vol],
            Self::SetMute(muted) => vec![0x02, u8::from(muted)],
            Self::VolumeState { volume, muted } => vec![0x03, volume, u8::from(muted)],
            Self::DelayOffsetMs(ms) => {
                let b = ms.to_be_bytes();
                vec![0x05, b[0], b[1]]
            }
        }
    }

    pub fn decode(bytes: &[u8]) -> TransportResult<Self> {
        if bytes.is_empty() {
            return Err(TransportError::Wire("empty control message".into()));
        }
        match bytes[0] {
            0x01 if bytes.len() == 2 => Ok(Self::SetVolume(bytes[1])),
            0x02 if bytes.len() == 2 => Ok(Self::SetMute(bytes[1] != 0)),
            0x03 if bytes.len() == 3 => Ok(Self::VolumeState {
                volume: bytes[1],
                muted: bytes[2] != 0,
            }),
            0x05 if bytes.len() == 3 => Ok(Self::DelayOffsetMs(i16::from_be_bytes([
                bytes[1], bytes[2],
            ]))),
            tag => Err(TransportError::Wire(format!(
                "invalid control message tag 0x{tag:02X} with len {}",
                bytes.len()
            ))),
        }
    }
}

/// Payload for `FrameType::Ping`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PingPayload {
    pub client_time_us: u64,
}

impl PingPayload {
    pub fn encode(&self) -> [u8; 8] {
        self.client_time_us.to_be_bytes()
    }

    pub fn decode(bytes: &[u8]) -> TransportResult<Self> {
        if bytes.len() != 8 {
            return Err(TransportError::Wire(format!(
                "invalid PingPayload length: {}",
                bytes.len()
            )));
        }
        let mut arr = [0u8; 8];
        arr.copy_from_slice(bytes);
        Ok(Self {
            client_time_us: u64::from_be_bytes(arr),
        })
    }
}

/// Payload for `FrameType::Pong`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PongPayload {
    pub client_time_us: u64,
    pub jitter_buf_ms: u16,
    pub dropped_frames: u32,
}

impl PongPayload {
    pub fn encode(&self) -> [u8; 14] {
        let mut out = [0u8; 14];
        out[0..8].copy_from_slice(&self.client_time_us.to_be_bytes());
        out[8..10].copy_from_slice(&self.jitter_buf_ms.to_be_bytes());
        out[10..14].copy_from_slice(&self.dropped_frames.to_be_bytes());
        out
    }

    pub fn decode(bytes: &[u8]) -> TransportResult<Self> {
        if bytes.len() != 14 {
            return Err(TransportError::Wire(format!(
                "invalid PongPayload length: {}",
                bytes.len()
            )));
        }
        Ok(Self {
            client_time_us: u64::from_be_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
            ]),
            jitter_buf_ms: u16::from_be_bytes([bytes[8], bytes[9]]),
            dropped_frames: u32::from_be_bytes([bytes[10], bytes[11], bytes[12], bytes[13]]),
        })
    }
}

/// Validate that a `ClientHello` protocol version matches `SHT4_VERSION`.
pub fn validate_version(version: u8) -> bool {
    version == SHT4_VERSION
}

/// Validate that an RTP SSRC is neither `0` nor `u32::MAX`.
pub fn is_valid_ssrc(ssrc: u32) -> bool {
    ssrc != 0 && ssrc != u32::MAX
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn frame_header_roundtrip_and_payload_cap() {
        let hdr = FrameHeader {
            frame_type: FrameType::Control,
            flags: FLAG_DISCONTINUITY,
            payload_len: 1024,
            seq: 42,
            timestamp_samples: 960 * 42,
        };
        let raw = hdr.to_bytes();
        assert_eq!(raw.len(), FRAME_HEADER_LEN);
        let parsed = FrameHeader::from_bytes(&raw).unwrap();
        assert_eq!(parsed, hdr);

        // Legacy TCP audio frame types (0x01, 0x02) must be rejected.
        assert!(FrameType::from_u8(0x01).is_err());
        assert!(FrameType::from_u8(0x02).is_err());

        let mut oversize = raw;
        oversize[2..4].copy_from_slice(&(MAX_PAYLOAD_LEN as u16 + 1).to_be_bytes());
        let err = FrameHeader::from_bytes(&oversize).unwrap_err();
        assert!(matches!(err, TransportError::PayloadTooLarge { .. }));
    }

    #[test]
    fn client_and_server_hello_v4_roundtrip() {
        let ch = ClientHello {
            version: SHT4_VERSION,
            flags: 0,
            client_device_id: "desktop-linux-1".into(),
            client_pk: [0x11; 32],
            client_nonce: [0x22; 16],
            audio_params: AudioParams::default(),
            sender_ssrc: 0xDEAD_BEEF,
            srtp_suite: SRTP_SUITE_AES128_ICM_HMAC_SHA1_80,
            mac_c: [0x33; 32],
        };
        let mut buf = Vec::new();
        ch.write_to(&mut buf).unwrap();
        let (decoded_ch, transcript) = ClientHello::read_from(&mut Cursor::new(&buf)).unwrap();
        assert_eq!(decoded_ch, ch);
        assert_eq!(transcript, ch.encode_transcript().unwrap());

        let sh = ServerHello {
            status: HandshakeStatus::Ok,
            server_device_id: "living-room-pi".into(),
            server_pk: [0x44; 32],
            server_nonce: [0x55; 16],
            audio_params: AudioParams::default(),
            initial_volume: 85,
            initial_muted: true,
            rtp_port: 4716,
            srtp_suite: SRTP_SUITE_AES128_ICM_HMAC_SHA1_80,
            mac_s: [0x66; 32],
        };
        let mut sbuf = Vec::new();
        sh.write_to(&mut sbuf).unwrap();
        let (decoded_sh, body) = ServerHello::read_from(&mut Cursor::new(&sbuf)).unwrap();
        assert_eq!(decoded_sh, sh);
        assert_eq!(body, sh.encode_body().unwrap());
    }

    #[test]
    fn control_ping_pong_roundtrip() {
        let msgs = [
            ControlMessage::SetVolume(75),
            ControlMessage::SetMute(true),
            ControlMessage::VolumeState {
                volume: 80,
                muted: false,
            },
            ControlMessage::DelayOffsetMs(-15),
        ];
        for msg in msgs {
            let encoded = msg.encode();
            let decoded = ControlMessage::decode(&encoded).unwrap();
            assert_eq!(decoded, msg);
        }

        // Legacy BitrateHint tag (0x04) must be rejected.
        assert!(ControlMessage::decode(&[0x04, 0x00, 0x80]).is_err());

        let ping = PingPayload {
            client_time_us: 123_456_789,
        };
        assert_eq!(PingPayload::decode(&ping.encode()).unwrap(), ping);

        let pong = PongPayload {
            client_time_us: 123_456_789,
            jitter_buf_ms: 40,
            dropped_frames: 2,
        };
        assert_eq!(PongPayload::decode(&pong.encode()).unwrap(), pong);
    }

    #[test]
    fn audio_profile_roundtrip() {
        for profile in [
            AudioProfile::OpusBalanced,
            AudioProfile::OpusLowLatency,
            AudioProfile::OpusLowBandwidth,
            AudioProfile::PcmLossless,
        ] {
            let params = profile.to_audio_params(40);
            assert_eq!(AudioProfile::from_audio_params(&params).unwrap(), profile);
        }
    }

    struct ChunkyTimeoutReader {
        data: Vec<u8>,
        pos: usize,
        chunk_size: usize,
        timeout_next: bool,
    }

    impl Read for ChunkyTimeoutReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.pos == 0 && self.timeout_next {
                self.timeout_next = false;
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "idle before frame start",
                ));
            }
            if self.pos >= self.data.len() {
                return Ok(0);
            }
            if self.pos > 0 && self.timeout_next {
                self.timeout_next = false;
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "mid-frame stall",
                ));
            }
            let take = self
                .chunk_size
                .min(buf.len())
                .min(self.data.len() - self.pos);
            buf[..take].copy_from_slice(&self.data[self.pos..self.pos + take]);
            self.pos += take;
            self.timeout_next = true;
            Ok(take)
        }
    }

    #[test]
    fn encrypted_frame_read_from_recovers_mid_frame_timeout() {
        let frame = EncryptedFrame {
            header: FrameHeader {
                frame_type: FrameType::Control,
                flags: 0,
                payload_len: 3,
                seq: 7,
                timestamp_samples: 0,
            },
            ciphertext_and_tag: vec![0xAB; 3 + AEAD_TAG_LEN],
        };
        let mut wire = Vec::new();
        frame.write_to(&mut wire).unwrap();

        let mut reader = ChunkyTimeoutReader {
            data: wire,
            pos: 0,
            chunk_size: 4,
            timeout_next: true,
        };
        // First read at offset 0 returns WouldBlock immediately.
        let err = EncryptedFrame::read_from(&mut reader).unwrap_err();
        assert!(matches!(
            err,
            TransportError::Io(ref e) if e.kind() == std::io::ErrorKind::WouldBlock
        ));
        // Second read starts consuming the header and completes despite mid-frame TimedOut stalls.
        let decoded = EncryptedFrame::read_from(&mut reader).unwrap();
        assert_eq!(decoded, frame);
    }
}
