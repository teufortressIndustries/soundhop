//! SHT4 TCP control-channel client with mutual authentication, SRTP parameter negotiation,
//! and encrypted control/ping/pong/close framing.

use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use socket2::{SockRef, TcpKeepalive};
use zeroize::Zeroizing;

use crate::transport::crypto::{
    compute_client_hello_mac, generate_nonce, generate_ssrc, verify_server_hello_mac, AeadOpenHalf,
    AeadSealHalf, AeadSession, EphemeralKeypair, Role, SessionKeys,
};
use crate::transport::wire::{
    AudioParams, ClientHello, ControlMessage, DecryptedFrame, EncryptedFrame, FrameType,
    HandshakeStatus, PingPayload, ServerHello, SRTP_SUITE_AES128_ICM_HMAC_SHA1_80,
};
use crate::transport::{
    TransportError, TransportResult, AEAD_TAG_LEN, FRAME_HEADER_LEN, MAX_PAYLOAD_LEN, SHT4_VERSION,
};

const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
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

/// Authenticated, encrypted client-side SHT4 control connection.
pub struct Sht3ClientConnection {
    writer: Sht3ClientWriter,
    open: AeadOpenHalf,
    server_device_id: String,
    audio_params: AudioParams,
    initial_volume: u8,
    initial_muted: bool,
    sender_ssrc: u32,
    rtp_port: u16,
    srtp_master_key_c2s: Zeroizing<[u8; 30]>,
    srtp_master_key_s2c: Zeroizing<[u8; 30]>,
}

/// Outbound write half of a split [`Sht3ClientConnection`].
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
    /// Connect to an SHT4 server and execute the mutual-auth + X25519 + SRTP key handshake.
    pub fn connect(
        addr: SocketAddr,
        config: Sht3ClientConfig,
        k_pair: &[u8; 32],
    ) -> TransportResult<Self> {
        let mut stream = TcpStream::connect_timeout(&addr, config.connect_timeout)?;
        // Use a >=2s handshake timeout while waiting for ServerHello (peer media
        // pipeline cold-start can exceed steady-state io_timeout such as 200ms).
        let handshake_timeout = Some(
            config
                .io_timeout
                .map_or(HANDSHAKE_TIMEOUT, |d| d.max(HANDSHAKE_TIMEOUT)),
        );
        configure_socket_defaults(&stream, handshake_timeout, handshake_timeout)?;

        let client_eph = EphemeralKeypair::generate();
        let client_pk = client_eph.public_bytes();
        let client_nonce = generate_nonce();
        let sender_ssrc = generate_ssrc();

        let mut client_hello = ClientHello {
            version: SHT4_VERSION,
            flags: 0,
            client_device_id: config.client_device_id,
            client_pk,
            client_nonce,
            audio_params: config.audio_params,
            sender_ssrc,
            srtp_suite: SRTP_SUITE_AES128_ICM_HMAC_SHA1_80,
            mac_c: [0u8; 32],
        };
        let client_transcript = client_hello.encode_transcript()?;
        client_hello.mac_c = compute_client_hello_mac(k_pair, &client_transcript);
        client_hello.write_to(&mut stream)?;
        stream.flush()?;

        let (server_hello, server_body) = ServerHello::read_from(&mut stream)?;
        stream.set_read_timeout(config.io_timeout)?;
        stream.set_write_timeout(bounded_write_timeout(config.io_timeout))?;
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
                    "server rejected SHT4 protocol version".into(),
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

        if server_hello.srtp_suite != SRTP_SUITE_AES128_ICM_HMAC_SHA1_80 {
            return Err(TransportError::Wire(format!(
                "server returned unsupported SRTP suite {}",
                server_hello.srtp_suite
            )));
        }
        if server_hello.rtp_port == 0 {
            return Err(TransportError::Wire(
                "server returned invalid rtp_port 0".into(),
            ));
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
        let srtp_master_key_c2s = Zeroizing::new(keys.srtp_master_key_c2s);
        let srtp_master_key_s2c = Zeroizing::new(keys.srtp_master_key_s2c);
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
            sender_ssrc,
            rtp_port: server_hello.rtp_port,
            srtp_master_key_c2s,
            srtp_master_key_s2c,
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

    pub fn sender_ssrc(&self) -> u32 {
        self.sender_ssrc
    }

    pub fn rtp_port(&self) -> u16 {
        self.rtp_port
    }

    pub fn srtp_master_key_c2s(&self) -> &[u8; 30] {
        &self.srtp_master_key_c2s
    }

    pub fn srtp_master_key_s2c(&self) -> &[u8; 30] {
        &self.srtp_master_key_s2c
    }

    pub fn frames_remaining(&self) -> u64 {
        self.writer.frames_remaining()
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

    use crate::transport::server::{Sht3Listener, Sht3ServerConfig};
    use crate::transport::wire::{
        AudioProfile, ControlMessage, FrameType, PingPayload, PongPayload,
    };
    use crate::transport::{InMemoryTrustStore, TransportError};

    use super::*;

    #[test]
    fn loopback_v4_handshake_control_and_ping_pong() {
        let k_pair = [0x5A; 32];
        let trust = Arc::new(InMemoryTrustStore::new());
        trust.insert("client-pc", k_pair);

        let mut server_cfg = Sht3ServerConfig::new("server-speaker");
        server_cfg.initial_volume = 72;
        server_cfg.initial_muted = false;
        server_cfg.rtp_port = 51234;

        let listener =
            Sht3Listener::bind("127.0.0.1:0".parse().unwrap(), server_cfg, trust).unwrap();
        let addr = listener.local_addr().unwrap();

        let profile = AudioProfile::OpusBalanced;

        let server_handle = thread::spawn(move || {
            let mut conn = listener.accept().unwrap();
            assert_eq!(conn.peer_device_id(), "client-pc");
            assert_eq!(conn.rtp_port(), 51234);
            assert_ne!(conn.sender_ssrc(), 0);
            assert_ne!(conn.sender_ssrc(), u32::MAX);
            let srv_c2s_key = *conn.srtp_master_key_c2s();
            let srv_ssrc = conn.sender_ssrc();

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
                jitter_buf_ms: 40,
                dropped_frames: 0,
            })
            .unwrap();

            // Receive Close.
            let close_frame = conn.recv_frame().unwrap();
            assert_eq!(close_frame.header.frame_type, FrameType::Close);
            (srv_c2s_key, srv_ssrc)
        });

        let client_cfg = Sht3ClientConfig::new("client-pc", profile.to_audio_params(40));
        let mut client = Sht3ClientConnection::connect(addr, client_cfg, &k_pair).unwrap();
        assert_eq!(client.server_device_id(), "server-speaker");
        assert_eq!(client.initial_volume(), 72);
        assert!(!client.initial_muted());
        assert_eq!(client.rtp_port(), 51234);
        let cli_c2s_key = *client.srtp_master_key_c2s();
        let cli_ssrc = client.sender_ssrc();

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
        let (srv_c2s_key, srv_ssrc) = server_handle.join().unwrap();
        assert_eq!(cli_c2s_key, srv_c2s_key);
        assert_eq!(cli_ssrc, srv_ssrc);
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
            let err1 = listener.accept().err().expect("unknown peer must fail");
            assert!(matches!(err1, TransportError::AuthRejected(_)));

            let err2 = listener.accept().err().expect("wrong key peer must fail");
            assert!(matches!(err2, TransportError::AuthRejected(_)));
        });

        let cfg1 = Sht3ClientConfig::new(
            "stranger-device",
            AudioProfile::PcmLossless.to_audio_params(40),
        );
        let c_err1 = Sht3ClientConnection::connect(addr, cfg1, &[0x99; 32])
            .err()
            .expect("client should see AuthRejected");
        assert!(matches!(c_err1, TransportError::AuthRejected(_)));

        let cfg2 =
            Sht3ClientConfig::new("known-peer", AudioProfile::PcmLossless.to_audio_params(40));
        let c_err2 = Sht3ClientConnection::connect(addr, cfg2, &[0x22; 32])
            .err()
            .expect("client with wrong key should see AuthRejected");
        assert!(matches!(c_err2, TransportError::AuthRejected(_)));

        server_handle.join().unwrap();
    }

    #[test]
    fn rejects_legacy_v3_client_with_unsupported_version() {
        let k_pair = [0x44; 32];
        let trust = Arc::new(InMemoryTrustStore::new());
        trust.insert("old-v3-client", k_pair);

        let listener = Sht3Listener::bind(
            "127.0.0.1:0".parse().unwrap(),
            Sht3ServerConfig::new("v4-server"),
            trust,
        )
        .unwrap();
        let addr = listener.local_addr().unwrap();

        let server_handle = thread::spawn(move || {
            let err = listener.accept().err().expect("v3 client must be rejected");
            assert!(matches!(err, TransportError::Wire(_)));
        });

        // Craft a raw v3 ClientHello (version = 3, no sender_ssrc / srtp_suite).
        let mut stream = TcpStream::connect(addr).unwrap();
        let id = b"old-v3-client";
        let mut body = Vec::new();
        body.extend_from_slice(&crate::transport::SHT3_MAGIC);
        body.push(3u8); // old version 3
        body.push(0u8);
        body.extend_from_slice(&(id.len() as u16).to_be_bytes());
        body.extend_from_slice(id);
        body.extend_from_slice(&[0x11; 32]);
        body.extend_from_slice(&[0x22; 16]);
        AudioParams::default().encode_into(&mut body);
        let mac = compute_client_hello_mac(&k_pair, &body);
        let total_len = (body.len() + 32) as u16;
        stream.write_all(&crate::transport::SHT3_MAGIC).unwrap();
        stream.write_all(&[0x01]).unwrap();
        stream.write_all(&total_len.to_be_bytes()).unwrap();
        stream.write_all(&body).unwrap();
        stream.write_all(&mac).unwrap();
        stream.flush().unwrap();

        let (sh, _) = ServerHello::read_from(&mut stream).unwrap();
        assert_eq!(sh.status, HandshakeStatus::UnsupportedVersion);

        server_handle.join().unwrap();
    }

    #[test]
    fn into_split_concurrent_full_duplex_control_and_telemetry() {
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
            let mut saw_ping = false;
            let mut saw_ctrl = false;
            loop {
                let frame = conn.recv_frame().unwrap();
                match frame.header.frame_type {
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
                    FrameType::Pong => {}
                }
            }
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

        writer
            .send_ping(&PingPayload {
                client_time_us: 42_000,
            })
            .unwrap();
        writer.send_control(&ControlMessage::SetVolume(55)).unwrap();
        writer.flush().unwrap();
        assert_eq!(writer.frames_remaining(), initial_remaining - 2);

        reader_thread.join().unwrap();
        writer.send_close().unwrap();
        assert_eq!(writer.frames_remaining(), initial_remaining - 3);
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

    #[test]
    fn tampering_with_server_rtp_port_or_srtp_suite_invalidates_mac() {
        let k_pair = [0x77; 32];
        let trust = Arc::new(InMemoryTrustStore::new());
        trust.insert("tamper-client", k_pair);

        let raw_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = raw_listener.local_addr().unwrap();

        let mitm = thread::spawn(move || {
            let (mut stream, _) = raw_listener.accept().unwrap();
            let (ch, client_transcript) = ClientHello::read_from(&mut stream).unwrap();
            assert_eq!(ch.version, SHT4_VERSION);
            assert!(crate::transport::crypto::verify_client_hello_mac(
                &k_pair,
                &client_transcript,
                &ch.mac_c
            ));

            let s_eph = EphemeralKeypair::generate();
            let mut sh = ServerHello {
                status: HandshakeStatus::Ok,
                server_device_id: "mitm-srv".into(),
                server_pk: s_eph.public_bytes(),
                server_nonce: generate_nonce(),
                audio_params: ch.audio_params,
                initial_volume: 100,
                initial_muted: false,
                rtp_port: 4716,
                srtp_suite: SRTP_SUITE_AES128_ICM_HMAC_SHA1_80,
                mac_s: [0u8; 32],
            };
            let body = sh.encode_body().unwrap();
            let mut full = Vec::with_capacity(client_transcript.len() + body.len());
            full.extend_from_slice(&client_transcript);
            full.extend_from_slice(&body);
            sh.mac_s = crate::transport::crypto::compute_server_hello_mac(&k_pair, &full);
            // Tamper with rtp_port after computing mac_s -> client must reject!
            sh.rtp_port = 9999;
            sh.write_to(&mut stream).unwrap();
            stream.flush().unwrap();
        });

        let cfg = Sht3ClientConfig::new(
            "tamper-client",
            AudioProfile::OpusBalanced.to_audio_params(40),
        );
        let err = Sht3ClientConnection::connect(addr, cfg, &k_pair)
            .err()
            .expect("tampered rtp_port must fail ServerHello MAC verification");
        assert!(matches!(err, TransportError::AuthRejected(_)));
        mitm.join().unwrap();
    }

    #[test]
    fn slow_server_hello_exceeding_io_timeout_succeeds_and_applies_io_timeout_after_handshake() {
        let k_pair = [0x88; 32];
        let raw_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = raw_listener.local_addr().unwrap();

        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();

        let server_handle = thread::spawn(move || {
            let (mut stream, _) = raw_listener.accept().unwrap();
            let (ch, client_transcript) = ClientHello::read_from(&mut stream).unwrap();
            assert_eq!(ch.version, SHT4_VERSION);
            assert!(crate::transport::crypto::verify_client_hello_mac(
                &k_pair,
                &client_transcript,
                &ch.mac_c
            ));

            // Simulate peer media pipeline cold-start exceeding steady-state 200ms io_timeout.
            thread::sleep(Duration::from_millis(350));

            let s_eph = EphemeralKeypair::generate();
            let mut sh = ServerHello {
                status: HandshakeStatus::Ok,
                server_device_id: "slow-srv".into(),
                server_pk: s_eph.public_bytes(),
                server_nonce: generate_nonce(),
                audio_params: ch.audio_params,
                initial_volume: 100,
                initial_muted: false,
                rtp_port: 4716,
                srtp_suite: SRTP_SUITE_AES128_ICM_HMAC_SHA1_80,
                mac_s: [0u8; 32],
            };
            let body = sh.encode_body().unwrap();
            let mut full = Vec::with_capacity(client_transcript.len() + body.len());
            full.extend_from_slice(&client_transcript);
            full.extend_from_slice(&body);
            sh.mac_s = crate::transport::crypto::compute_server_hello_mac(&k_pair, &full);
            sh.write_to(&mut stream).unwrap();
            stream.flush().unwrap();

            // Hold socket open without sending frames until client verifies post-handshake io_timeout.
            let _ = done_rx.recv();
        });

        let mut cfg = Sht3ClientConfig::new(
            "slow-hello-client",
            AudioProfile::OpusBalanced.to_audio_params(40),
        );
        cfg.io_timeout = Some(Duration::from_millis(200));

        let mut client = Sht3ClientConnection::connect(addr, cfg, &k_pair)
            .expect("handshake must use >=2s timeout and survive 350ms ServerHello");
        assert_eq!(client.server_device_id(), "slow-srv");

        // Post-handshake reads must honor the configured 200ms io_timeout, not the 2s handshake timeout.
        let read_start = std::time::Instant::now();
        let err = client
            .recv_frame()
            .expect_err("idle post-handshake read must time out at io_timeout");
        assert!(
            matches!(
                err,
                TransportError::Io(ref io_err)
                    if matches!(
                        io_err.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    )
            ),
            "expected timeout I/O error, got {err:?}"
        );
        assert!(
            read_start.elapsed() < Duration::from_secs(1),
            "post-handshake io_timeout took too long: {:?}",
            read_start.elapsed()
        );

        let _ = done_tx.send(());
        server_handle.join().unwrap();
    }
}
