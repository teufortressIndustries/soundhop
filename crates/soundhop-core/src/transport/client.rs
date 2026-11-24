//! SHT3 TCP client with mutual authentication and encrypted audio/control framing.

use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use socket2::{SockRef, TcpKeepalive};

use crate::transport::crypto::{
    compute_client_hello_mac, generate_nonce, verify_server_hello_mac, AeadOpenHalf, AeadSealHalf,
    AeadSession, EphemeralKeypair, Role, SessionKeys,
};
use crate::transport::wire::{
    AudioParams, ClientHello, ControlMessage, DecryptedFrame, EncryptedFrame, FrameType,
    HandshakeStatus, PingPayload, ServerHello,
};
use crate::transport::{
    TransportError, TransportResult, AEAD_TAG_LEN, FRAME_HEADER_LEN, MAX_PAYLOAD_LEN, SHT3_VERSION,
};

const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const DEFAULT_BOUNDED_WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const KEEPALIVE_IDLE_TIME: Duration = Duration::from_secs(5);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(2);
const INITIAL_WIRE_SCRATCH_CAP: usize = FRAME_HEADER_LEN + MAX_PAYLOAD_LEN + AEAD_TAG_LEN;

pub(crate) fn bounded_write_timeout(io_timeout: Option<Duration>) -> Option<Duration> {
    io_timeout.or(Some(DEFAULT_BOUNDED_WRITE_TIMEOUT))
}

pub(crate) fn configure_socket_defaults(
    stream: &TcpStream,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
) -> TransportResult<()> {
    stream.set_nodelay(true)?;
    let keepalive = TcpKeepalive::new()
        .with_time(KEEPALIVE_IDLE_TIME)
        .with_interval(KEEPALIVE_INTERVAL);
    SockRef::from(stream).set_tcp_keepalive(&keepalive)?;
    stream.set_read_timeout(read_timeout)?;
    stream.set_write_timeout(write_timeout)?;
    Ok(())
}

/// Configuration for [`Sht3ClientConnection::connect`].
#[derive(Debug, Clone)]
pub struct Sht3ClientConfig {
    pub client_device_id: String,
    pub audio_params: AudioParams,
    pub io_timeout: Option<Duration>,
    pub connect_timeout: Duration,
}

impl Sht3ClientConfig {
    pub fn new(client_device_id: impl Into<String>, audio_params: AudioParams) -> Self {
        Self {
            client_device_id: client_device_id.into(),
            audio_params,
            io_timeout: Some(Duration::from_secs(10)),
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
        }
    }
}

/// Authenticated, encrypted client-side SHT3 connection.
pub struct Sht3ClientConnection {
    writer: Sht3ClientWriter,
    open: AeadOpenHalf,
    server_device_id: String,
    audio_params: AudioParams,
    initial_volume: u8,
    initial_muted: bool,
}

/// Outbound write half of a split [`Sht3ClientConnection`], reusing a pre-allocated wire buffer.
pub struct Sht3ClientWriter {
    stream: TcpStream,
    seal: AeadSealHalf,
    wire_scratch: Vec<u8>,
}

impl Sht3ClientWriter {
    fn seal_and_write(
        &mut self,
        frame_type: FrameType,
        flags: u8,
        timestamp_samples: u32,
        payload: &[u8],
    ) -> TransportResult<()> {
        let frame = self
            .seal
            .seal_frame(frame_type, flags, timestamp_samples, payload)?;
        frame.write_into_buf(&mut self.wire_scratch, &mut self.stream)?;
        Ok(())
    }

    pub fn send_audio(
        &mut self,
        frame_type: FrameType,
        flags: u8,
        timestamp_samples: u32,
        encoded_audio: &[u8],
    ) -> TransportResult<()> {
        self.seal_and_write(frame_type, flags, timestamp_samples, encoded_audio)
    }

    pub fn send_control(&mut self, msg: &ControlMessage) -> TransportResult<()> {
        let encoded = msg.encode();
        self.seal_and_write(FrameType::Control, 0, 0, &encoded)?;
        self.stream.flush()?;
        Ok(())
    }

    pub fn send_ping(&mut self, ping: &PingPayload) -> TransportResult<()> {
        let encoded = ping.encode();
        self.seal_and_write(FrameType::Ping, 0, 0, &encoded)?;
        self.stream.flush()?;
        Ok(())
    }

    pub fn send_close(&mut self) -> TransportResult<()> {
        self.seal_and_write(FrameType::Close, 0, 0, &[])?;
        self.stream.flush()?;
        Ok(())
    }

    pub fn flush(&mut self) -> TransportResult<()> {
        self.stream.flush()?;
        Ok(())
    }

    pub fn frames_remaining(&self) -> u64 {
        self.seal.frames_remaining()
    }
}

/// Inbound read half of a split [`Sht3ClientConnection`].
pub struct Sht3ClientReader {
    stream: TcpStream,
    open: AeadOpenHalf,
}

impl Sht3ClientReader {
    pub fn recv_frame(&mut self) -> TransportResult<DecryptedFrame> {
        let encrypted = EncryptedFrame::read_from(&mut self.stream)?;
        let payload = self.open.open_frame(&encrypted)?;
        Ok(DecryptedFrame {
            header: encrypted.header,
            payload,
        })
    }

    pub fn set_read_timeout(&self, dur: Option<Duration>) -> TransportResult<()> {
        self.stream.set_read_timeout(dur)?;
        Ok(())
    }
}

impl Sht3ClientConnection {
    /// Connect to an SHT3 server and execute the mutual-auth + X25519 handshake.
    pub fn connect(
        addr: SocketAddr,
        config: Sht3ClientConfig,
        k_pair: &[u8; 32],
    ) -> TransportResult<Self> {
        let mut stream = TcpStream::connect_timeout(&addr, config.connect_timeout)?;
        configure_socket_defaults(
            &stream,
            config.io_timeout,
            bounded_write_timeout(config.io_timeout),
        )?;

        let client_eph = EphemeralKeypair::generate();
        let client_pk = client_eph.public_bytes();
        let client_nonce = generate_nonce();

        let mut client_hello = ClientHello {
            version: SHT3_VERSION,
            flags: 0,
            client_device_id: config.client_device_id,
            client_pk,
            client_nonce,
            audio_params: config.audio_params,
            mac_c: [0u8; 32],
        };
        let client_transcript = client_hello.encode_transcript()?;
        client_hello.mac_c = compute_client_hello_mac(k_pair, &client_transcript);
        client_hello.write_to(&mut stream)?;
        stream.flush()?;

        let (server_hello, server_body) = ServerHello::read_from(&mut stream)?;
        match server_hello.status {
            HandshakeStatus::Ok => {}
            HandshakeStatus::AuthFailed => {
                return Err(TransportError::AuthRejected(format!(
                    "server '{}' rejected authentication",
                    server_hello.server_device_id
                )));
            }
            HandshakeStatus::UnsupportedVersion => {
                return Err(TransportError::Wire(
                    "server rejected SHT3 protocol version".into(),
                ));
            }
            HandshakeStatus::UnsupportedCodec => {
                return Err(TransportError::UnsupportedProfile(
                    "server rejected requested audio codec".into(),
                ));
            }
            HandshakeStatus::InternalError => {
                return Err(TransportError::Wire(
                    "server reported internal error".into(),
                ));
            }
        }

        let mut full_transcript = Vec::with_capacity(client_transcript.len() + server_body.len());
        full_transcript.extend_from_slice(&client_transcript);
        full_transcript.extend_from_slice(&server_body);

        if !verify_server_hello_mac(k_pair, &full_transcript, &server_hello.mac_s) {
            return Err(TransportError::AuthRejected(format!(
                "server '{}' failed mutual-auth MAC verification",
                server_hello.server_device_id
            )));
        }

        let shared_dh = client_eph.diffie_hellman(&server_hello.server_pk)?;
        let keys = SessionKeys::derive(
            shared_dh.as_bytes(),
            k_pair,
            &client_nonce,
            &server_hello.server_nonce,
            &full_transcript,
        )?;
        let (seal, open) = AeadSession::new(Role::Client, &keys).split();

        Ok(Self {
            writer: Sht3ClientWriter {
                stream,
                seal,
                wire_scratch: Vec::with_capacity(INITIAL_WIRE_SCRATCH_CAP),
            },
            open,
            server_device_id: server_hello.server_device_id,
            audio_params: server_hello.audio_params,
            initial_volume: server_hello.initial_volume,
            initial_muted: server_hello.initial_muted,
        })
    }

    /// Split this connection into independent write and read halves for full-duplex operation.
    pub fn into_split(self) -> TransportResult<(Sht3ClientWriter, Sht3ClientReader)> {
        let read_stream = self.writer.stream.try_clone()?;
        Ok((
            self.writer,
            Sht3ClientReader {
                stream: read_stream,
                open: self.open,
            },
        ))
    }

    pub fn server_device_id(&self) -> &str {
        &self.server_device_id
    }

    pub fn audio_params(&self) -> &AudioParams {
        &self.audio_params
    }

    pub fn initial_volume(&self) -> u8 {
        self.initial_volume
    }

    pub fn initial_muted(&self) -> bool {
        self.initial_muted
    }

    pub fn frames_remaining(&self) -> u64 {
        self.writer.frames_remaining()
    }

    pub fn send_audio(
        &mut self,
        frame_type: FrameType,
        flags: u8,
        timestamp_samples: u32,
        encoded_audio: &[u8],
    ) -> TransportResult<()> {
        self.writer
            .send_audio(frame_type, flags, timestamp_samples, encoded_audio)
    }

    pub fn send_control(&mut self, msg: &ControlMessage) -> TransportResult<()> {
        self.writer.send_control(msg)
    }

    pub fn send_ping(&mut self, ping: &PingPayload) -> TransportResult<()> {
        self.writer.send_ping(ping)
    }

    pub fn send_close(&mut self) -> TransportResult<()> {
        self.writer.send_close()
    }

    pub fn flush(&mut self) -> TransportResult<()> {
        self.writer.flush()
    }

    pub fn set_read_timeout(&self, dur: Option<Duration>) -> TransportResult<()> {
        self.writer.stream.set_read_timeout(dur)?;
        Ok(())
    }

    pub fn recv_frame(&mut self) -> TransportResult<DecryptedFrame> {
        let encrypted = EncryptedFrame::read_from(&mut self.writer.stream)?;
        let payload = self.open.open_frame(&encrypted)?;
        Ok(DecryptedFrame {
            header: encrypted.header,
            payload,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use crate::transport::codec::{create_codec, AudioProfile};
    use crate::transport::jitter::{JitterBuffer, JitterConfig};
    use crate::transport::server::{Sht3Listener, Sht3ServerConfig};
    use crate::transport::wire::{ControlMessage, FrameType, PingPayload, PongPayload};
    use crate::transport::{InMemoryTrustStore, TransportError};

    use super::*;

    fn test_sine_frame(spc: usize, idx: usize) -> Vec<i16> {
        let mut out = Vec::with_capacity(spc * 2);
        let base = idx * spc;
        for i in 0..spc {
            let t = (base + i) as f64 / 48_000.0;
            let s = ((2.0 * std::f64::consts::PI * 440.0 * t).sin() * 12_000.0) as i16;
            out.push(s);
            out.push(-s);
        }
        out
    }

    #[test]
    fn loopback_pcm_audio_control_and_ping_pong() {
        let k_pair = [0x5A; 32];
        let trust = Arc::new(InMemoryTrustStore::new());
        trust.insert("client-pc", k_pair);

        let mut server_cfg = Sht3ServerConfig::new("server-speaker");
        server_cfg.initial_volume = 72;
        server_cfg.initial_muted = false;

        let listener =
            Sht3Listener::bind("127.0.0.1:0".parse().unwrap(), server_cfg, trust).unwrap();
        let addr = listener.local_addr().unwrap();

        let profile = AudioProfile::PcmLossless;
        let expected_pcm_0 = test_sine_frame(profile.frame_samples_per_channel(), 0);
        let expected_pcm_1 = test_sine_frame(profile.frame_samples_per_channel(), 1);
        let expected_pcm_0_srv = expected_pcm_0.clone();
        let expected_pcm_1_srv = expected_pcm_1.clone();

        let server_handle = thread::spawn(move || {
            let mut conn = listener.accept().unwrap();
            assert_eq!(conn.peer_device_id(), "client-pc");
            let mut codec = create_codec(conn.audio_params()).unwrap();
            let mut jb = JitterBuffer::new(JitterConfig::default());

            // Receive 2 encrypted PCM frames.
            for (i, expected) in [expected_pcm_0_srv, expected_pcm_1_srv]
                .into_iter()
                .enumerate()
            {
                let frame = conn.recv_frame().unwrap();
                assert_eq!(frame.header.frame_type, FrameType::AudioPcm);
                assert_eq!(frame.header.seq, i as u64);
                let mut pcm = Vec::new();
                codec.decode(&frame.payload, &mut pcm).unwrap();
                assert_eq!(pcm, expected);
                jb.push(pcm);
            }
            assert!(jb.is_primed());

            // Receive Control::SetVolume and reply with VolumeState.
            let ctrl = conn.recv_frame().unwrap();
            assert_eq!(ctrl.header.frame_type, FrameType::Control);
            assert_eq!(
                ControlMessage::decode(&ctrl.payload).unwrap(),
                ControlMessage::SetVolume(65)
            );
            conn.send_control(&ControlMessage::VolumeState {
                volume: 65,
                muted: false,
            })
            .unwrap();

            // Receive Ping and reply with Pong.
            let ping_frame = conn.recv_frame().unwrap();
            let ping = PingPayload::decode(&ping_frame.payload).unwrap();
            assert_eq!(ping.client_time_us, 999_001);
            conn.send_pong(&PongPayload {
                client_time_us: ping.client_time_us,
                jitter_buf_ms: jb.stats().buffered_ms,
                dropped_frames: 0,
            })
            .unwrap();

            // Receive Close.
            let close_frame = conn.recv_frame().unwrap();
            assert_eq!(close_frame.header.frame_type, FrameType::Close);
        });

        let client_cfg = Sht3ClientConfig::new("client-pc", profile.to_audio_params(40));
        let mut client = Sht3ClientConnection::connect(addr, client_cfg, &k_pair).unwrap();
        assert_eq!(client.server_device_id(), "server-speaker");
        assert_eq!(client.initial_volume(), 72);
        assert!(!client.initial_muted());

        let mut codec = create_codec(client.audio_params()).unwrap();
        for (i, pcm) in [&expected_pcm_0, &expected_pcm_1].into_iter().enumerate() {
            let mut pkt = Vec::new();
            codec.encode(pcm, &mut pkt).unwrap();
            client
                .send_audio(FrameType::AudioPcm, 0, (i as u32) * 960, &pkt)
                .unwrap();
        }
        client.send_control(&ControlMessage::SetVolume(65)).unwrap();

        let state_frame = client.recv_frame().unwrap();
        assert_eq!(
            ControlMessage::decode(&state_frame.payload).unwrap(),
            ControlMessage::VolumeState {
                volume: 65,
                muted: false
            }
        );

        client
            .send_ping(&PingPayload {
                client_time_us: 999_001,
            })
            .unwrap();
        let pong_frame = client.recv_frame().unwrap();
        let pong = PongPayload::decode(&pong_frame.payload).unwrap();
        assert_eq!(pong.client_time_us, 999_001);
        assert_eq!(pong.jitter_buf_ms, 40);

        client.send_close().unwrap();
        server_handle.join().unwrap();
    }

    #[cfg(feature = "opus")]
    #[test]
    fn loopback_opus_encrypted_stream_and_control() {
        let k_pair = [0xC3; 32];
        let trust = Arc::new(InMemoryTrustStore::new());
        trust.insert("opus-sender", k_pair);

        let listener = Sht3Listener::bind(
            "127.0.0.1:0".parse().unwrap(),
            Sht3ServerConfig::new("opus-receiver"),
            trust,
        )
        .unwrap();
        let addr = listener.local_addr().unwrap();

        let profile = AudioProfile::OpusBalanced;

        let server_handle = thread::spawn(move || {
            let mut conn = listener.accept().unwrap();
            let mut codec = create_codec(conn.audio_params()).unwrap();
            let mut total_energy = 0i64;
            for idx in 0..4u64 {
                let frame = conn.recv_frame().unwrap();
                assert_eq!(frame.header.frame_type, FrameType::AudioOpus);
                assert_eq!(frame.header.seq, idx);
                let mut pcm = Vec::new();
                let n = codec.decode(&frame.payload, &mut pcm).unwrap();
                assert_eq!(n, 1920);
                total_energy += pcm.iter().map(|&s| i64::from(s).abs()).sum::<i64>();
            }
            assert!(total_energy > 200_000, "decoded Opus audio was silent");

            let ctrl = conn.recv_frame().unwrap();
            assert_eq!(
                ControlMessage::decode(&ctrl.payload).unwrap(),
                ControlMessage::SetMute(true)
            );
            conn.send_control(&ControlMessage::BitrateHint(64)).unwrap();
        });

        let client_cfg = Sht3ClientConfig::new("opus-sender", profile.to_audio_params(40));
        let mut client = Sht3ClientConnection::connect(addr, client_cfg, &k_pair).unwrap();
        let mut codec = create_codec(client.audio_params()).unwrap();

        for idx in 0..4 {
            let pcm = test_sine_frame(profile.frame_samples_per_channel(), idx);
            let mut pkt = Vec::new();
            codec.encode(&pcm, &mut pkt).unwrap();
            client
                .send_audio(FrameType::AudioOpus, 0, (idx as u32) * 960, &pkt)
                .unwrap();
        }
        client.send_control(&ControlMessage::SetMute(true)).unwrap();

        let hint = client.recv_frame().unwrap();
        assert_eq!(
            ControlMessage::decode(&hint.payload).unwrap(),
            ControlMessage::BitrateHint(64)
        );
        server_handle.join().unwrap();
    }

    #[test]
    fn rejects_unpaired_and_bad_mac_peers() {
        let trust = Arc::new(InMemoryTrustStore::new());
        trust.insert("known-peer", [0x11; 32]);

        let listener = Sht3Listener::bind(
            "127.0.0.1:0".parse().unwrap(),
            Sht3ServerConfig::new("strict-server"),
            trust,
        )
        .unwrap();
        let addr = listener.local_addr().unwrap();

        let server_handle = thread::spawn(move || {
            // 1. Unknown device id
            let err1 = listener.accept().err().expect("unknown peer must fail");
            assert!(matches!(err1, TransportError::AuthRejected(_)));

            // 2. Known device id with wrong K_pair (bad MAC)
            let err2 = listener.accept().err().expect("wrong key peer must fail");
            assert!(matches!(err2, TransportError::AuthRejected(_)));
        });

        // 1. Unknown device id
        let cfg1 = Sht3ClientConfig::new(
            "stranger-device",
            AudioProfile::PcmLossless.to_audio_params(40),
        );
        let c_err1 = Sht3ClientConnection::connect(addr, cfg1, &[0x99; 32])
            .err()
            .expect("client should see AuthRejected");
        assert!(matches!(c_err1, TransportError::AuthRejected(_)));

        // 2. Known device id with wrong key
        let cfg2 =
            Sht3ClientConfig::new("known-peer", AudioProfile::PcmLossless.to_audio_params(40));
        let c_err2 = Sht3ClientConnection::connect(addr, cfg2, &[0x22; 32])
            .err()
            .expect("client with wrong key should see AuthRejected");
        assert!(matches!(c_err2, TransportError::AuthRejected(_)));

        server_handle.join().unwrap();
    }

    #[test]
    fn into_split_concurrent_full_duplex_audio_and_telemetry() {
        let k_pair = [0x6B; 32];
        let trust = Arc::new(InMemoryTrustStore::new());
        trust.insert("duplex-client", k_pair);

        let listener = Sht3Listener::bind(
            "127.0.0.1:0".parse().unwrap(),
            Sht3ServerConfig::new("duplex-server"),
            trust,
        )
        .unwrap();
        let addr = listener.local_addr().unwrap();

        let server_handle = thread::spawn(move || {
            let mut conn = listener.accept().unwrap();
            let mut audio_count = 0usize;
            let mut saw_ping = false;
            let mut saw_ctrl = false;
            loop {
                let frame = conn.recv_frame().unwrap();
                match frame.header.frame_type {
                    FrameType::AudioPcm => {
                        audio_count += 1;
                    }
                    FrameType::Ping => {
                        let ping = PingPayload::decode(&frame.payload).unwrap();
                        conn.send_pong(&PongPayload {
                            client_time_us: ping.client_time_us,
                            jitter_buf_ms: 40,
                            dropped_frames: 0,
                        })
                        .unwrap();
                        saw_ping = true;
                    }
                    FrameType::Control => {
                        assert_eq!(
                            ControlMessage::decode(&frame.payload).unwrap(),
                            ControlMessage::SetVolume(55)
                        );
                        conn.send_control(&ControlMessage::VolumeState {
                            volume: 55,
                            muted: false,
                        })
                        .unwrap();
                        saw_ctrl = true;
                    }
                    FrameType::Close => break,
                    _ => {}
                }
            }
            assert_eq!(audio_count, 20);
            assert!(saw_ping && saw_ctrl);
        });

        let client_cfg = Sht3ClientConfig::new(
            "duplex-client",
            AudioProfile::PcmLossless.to_audio_params(40),
        );
        assert_eq!(client_cfg.connect_timeout, Duration::from_secs(3));
        let conn = Sht3ClientConnection::connect(addr, client_cfg, &k_pair).unwrap();
        let (mut writer, mut reader) = conn.into_split().unwrap();
        reader
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();

        let initial_remaining = writer.frames_remaining();

        let reader_thread = thread::spawn(move || {
            let mut got_pong = false;
            let mut got_vol = false;
            while !(got_pong && got_vol) {
                let frame = reader.recv_frame().unwrap();
                match frame.header.frame_type {
                    FrameType::Pong => {
                        let pong = PongPayload::decode(&frame.payload).unwrap();
                        assert_eq!(pong.client_time_us, 42_000);
                        got_pong = true;
                    }
                    FrameType::Control => {
                        assert_eq!(
                            ControlMessage::decode(&frame.payload).unwrap(),
                            ControlMessage::VolumeState {
                                volume: 55,
                                muted: false,
                            }
                        );
                        got_vol = true;
                    }
                    other => panic!("unexpected server frame: {other:?}"),
                }
            }
        });

        let dummy_pcm = vec![0x12u8; 64];
        for i in 0..20u32 {
            writer
                .send_audio(FrameType::AudioPcm, 0, i * 960, &dummy_pcm)
                .unwrap();
            if i == 5 {
                writer
                    .send_ping(&PingPayload {
                        client_time_us: 42_000,
                    })
                    .unwrap();
            }
            if i == 10 {
                writer.send_control(&ControlMessage::SetVolume(55)).unwrap();
            }
        }
        writer.flush().unwrap();
        assert_eq!(writer.frames_remaining(), initial_remaining - 22);

        reader_thread.join().unwrap();
        writer.send_close().unwrap();
        assert_eq!(writer.frames_remaining(), initial_remaining - 23);
        server_handle.join().unwrap();
    }

    #[test]
    fn connect_timeout_bounds_unresponsive_peer() {
        let mut cfg = Sht3ClientConfig::new(
            "timeout-client",
            AudioProfile::PcmLossless.to_audio_params(40),
        );
        assert_eq!(cfg.connect_timeout, Duration::from_secs(3));
        cfg.connect_timeout = Duration::from_millis(60);

        // TEST-NET-1 (RFC 5737) blackhole address: either times out in ~60ms or fails immediately
        // with NetworkUnreachable/HostUnreachable in offline environments.
        let blackhole: SocketAddr = "192.0.2.1:4714".parse().unwrap();
        let start = std::time::Instant::now();
        let err = Sht3ClientConnection::connect(blackhole, cfg, &[0x11; 32])
            .err()
            .expect("connect to blackhole must fail");
        assert!(matches!(err, TransportError::Io(_)));
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "connect_timeout took too long: {:?}",
            start.elapsed()
        );
    }
}
