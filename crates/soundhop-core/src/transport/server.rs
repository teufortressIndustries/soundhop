//! SHT3 TCP listener and authenticated server-side connection.

use std::io::Write;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use crate::transport::client::{bounded_write_timeout, configure_socket_defaults};
use crate::transport::crypto::{
    compute_server_hello_mac, generate_nonce, verify_client_hello_mac, AeadOpenHalf, AeadSession,
    EphemeralKeypair, Role, SessionKeys,
};
use crate::transport::wire::{
    validate_version, AudioParams, ClientHello, CodecId, ControlMessage, DecryptedFrame,
    EncryptedFrame, FrameType, HandshakeStatus, PongPayload, ServerHello,
};
use crate::transport::{
    TransportError, TransportResult, TrustLookup, AEAD_TAG_LEN, FRAME_HEADER_LEN, MAX_PAYLOAD_LEN,
};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
const INITIAL_WIRE_SCRATCH_CAP: usize = FRAME_HEADER_LEN + MAX_PAYLOAD_LEN + AEAD_TAG_LEN;

/// Configuration for [`Sht3Listener`].
#[derive(Debug, Clone)]
pub struct Sht3ServerConfig {
    pub server_device_id: String,
    pub initial_volume: u8,
    pub initial_muted: bool,
    pub supported_codecs: Vec<CodecId>,
    pub io_timeout: Option<Duration>,
    /// Enable the optional encrypted SHT3-over-UDP audio data plane.
    pub enable_udp: bool,
}

impl Sht3ServerConfig {
    pub fn new(server_device_id: impl Into<String>) -> Self {
        #[cfg(feature = "opus")]
        let supported_codecs = vec![CodecId::Opus, CodecId::PcmS16Le];
        #[cfg(not(feature = "opus"))]
        let supported_codecs = vec![CodecId::PcmS16Le];

        Self {
            server_device_id: server_device_id.into(),
            initial_volume: 100,
            initial_muted: false,
            supported_codecs,
            io_timeout: Some(Duration::from_secs(10)),
            enable_udp: true,
        }
    }
}

/// TCP listener speaking SHT3 with mandatory `TrustLookup` mutual authentication.
pub struct Sht3Listener {
    listener: TcpListener,
    config: Sht3ServerConfig,
    trust: Arc<dyn TrustLookup>,
}

impl Sht3Listener {
    pub fn bind(
        addr: SocketAddr,
        config: Sht3ServerConfig,
        trust: Arc<dyn TrustLookup>,
    ) -> TransportResult<Self> {
        let listener = TcpListener::bind(addr)?;
        Ok(Self {
            listener,
            config,
            trust,
        })
    }

    pub fn local_addr(&self) -> TransportResult<SocketAddr> {
        Ok(self.listener.local_addr()?)
    }

    /// Accept one raw TCP connection without running the SHT3 handshake yet.
    pub fn accept_raw(&self) -> TransportResult<(TcpStream, SocketAddr)> {
        Ok(self.listener.accept()?)
    }

    /// Accept one TCP connection and perform the SHT3 mutual-auth handshake.
    ///
    /// Unknown peers or peers with an invalid `ClientHello` MAC are immediately sent
    /// `HandshakeStatus::AuthFailed` and rejected with [`TransportError::AuthRejected`]
    /// before any audio/codec state is allocated.
    pub fn accept(&self) -> TransportResult<Sht3ServerConnection> {
        let (stream, _peer_addr) = self.accept_raw()?;
        self.handshake(stream)
    }

    /// Perform the SHT3 server handshake on an already-accepted [`TcpStream`].
    pub fn handshake(&self, stream: TcpStream) -> TransportResult<Sht3ServerConnection> {
        Self::handshake_arc(stream, &self.config, self.trust.as_ref())
    }

    /// Perform the SHT3 server handshake on an already-accepted [`TcpStream`] using shared
    /// configuration and trust lookup, enforcing a strict 2-second handshake timeout before
    /// applying `config.io_timeout` after authentication succeeds.
    pub fn handshake_arc(
        mut stream: TcpStream,
        config: &Sht3ServerConfig,
        trust: &dyn TrustLookup,
    ) -> TransportResult<Sht3ServerConnection> {
        configure_socket_defaults(&stream, Some(HANDSHAKE_TIMEOUT), Some(HANDSHAKE_TIMEOUT))?;

        let (client_hello, client_transcript) = ClientHello::read_from(&mut stream)?;

        if !validate_version(client_hello.version) {
            let err_hello = ServerHello::error(
                HandshakeStatus::UnsupportedVersion,
                config.server_device_id.clone(),
            );
            let _ = err_hello.write_to(&mut stream);
            let _ = stream.flush();
            return Err(TransportError::Wire(format!(
                "unsupported SHT3 version: {}",
                client_hello.version
            )));
        }

        let Some(k_pair) = trust.lookup_peer_key(&client_hello.client_device_id) else {
            send_auth_failed(&mut stream, &config.server_device_id);
            return Err(TransportError::AuthRejected(format!(
                "unknown or unpaired device '{}'",
                client_hello.client_device_id
            )));
        };

        if !verify_client_hello_mac(&k_pair, &client_transcript, &client_hello.mac_c) {
            send_auth_failed(&mut stream, &config.server_device_id);
            return Err(TransportError::AuthRejected(format!(
                "invalid ClientHello MAC from '{}'",
                client_hello.client_device_id
            )));
        }

        if !config
            .supported_codecs
            .contains(&client_hello.audio_params.codec)
        {
            let err_hello = ServerHello::error(
                HandshakeStatus::UnsupportedCodec,
                config.server_device_id.clone(),
            );
            let _ = err_hello.write_to(&mut stream);
            let _ = stream.flush();
            return Err(TransportError::UnsupportedProfile(format!(
                "codec {:?} is not supported by server",
                client_hello.audio_params.codec
            )));
        }

        let server_eph = EphemeralKeypair::generate();
        let server_pk = server_eph.public_bytes();
        let server_nonce = generate_nonce();
        let negotiated_params = client_hello.audio_params;

        let shared_dh = server_eph.diffie_hellman(&client_hello.client_pk)?;

        let mut server_hello = ServerHello {
            status: HandshakeStatus::Ok,
            server_device_id: config.server_device_id.clone(),
            server_pk,
            server_nonce,
            audio_params: negotiated_params,
            initial_volume: config.initial_volume,
            initial_muted: config.initial_muted,
            mac_s: [0u8; 32],
        };
        let server_body = server_hello.encode_body()?;

        let mut full_transcript = Vec::with_capacity(client_transcript.len() + server_body.len());
        full_transcript.extend_from_slice(&client_transcript);
        full_transcript.extend_from_slice(&server_body);

        server_hello.mac_s = compute_server_hello_mac(&k_pair, &full_transcript);
        server_hello.write_to(&mut stream)?;
        stream.flush()?;

        let keys = SessionKeys::derive(
            shared_dh.as_bytes(),
            &k_pair,
            &client_hello.client_nonce,
            &server_nonce,
            &full_transcript,
        )?;
        let session = AeadSession::new(Role::Server, &keys);
        let udp_keys = keys.derive_udp_keys()?;
        let (_, udp_open) = AeadSession::new(Role::Server, &udp_keys).split();

        stream.set_read_timeout(config.io_timeout)?;
        stream.set_write_timeout(bounded_write_timeout(config.io_timeout))?;

        Ok(Sht3ServerConnection {
            stream,
            session,
            udp_open: Some(udp_open),
            wire_scratch: Vec::with_capacity(INITIAL_WIRE_SCRATCH_CAP),
            peer_device_id: client_hello.client_device_id,
            audio_params: negotiated_params,
        })
    }
}

fn send_auth_failed(stream: &mut TcpStream, server_device_id: &str) {
    let err_hello = ServerHello::error(HandshakeStatus::AuthFailed, server_device_id.to_string());
    let _ = err_hello.write_to(stream);
    let _ = stream.flush();
}

/// Authenticated, encrypted server-side SHT3 connection.
pub struct Sht3ServerConnection {
    stream: TcpStream,
    session: AeadSession,
    udp_open: Option<AeadOpenHalf>,
    wire_scratch: Vec<u8>,
    peer_device_id: String,
    audio_params: AudioParams,
}

impl Sht3ServerConnection {
    pub fn peer_device_id(&self) -> &str {
        &self.peer_device_id
    }

    pub fn audio_params(&self) -> &AudioParams {
        &self.audio_params
    }

    pub fn local_addr(&self) -> TransportResult<SocketAddr> {
        Ok(self.stream.local_addr()?)
    }

    /// Take the derived UDP inbound AEAD open half for the optional SHT3-over-UDP data plane.
    pub fn take_udp_open(&mut self) -> Option<AeadOpenHalf> {
        self.udp_open.take()
    }

    pub fn send_frame(
        &mut self,
        frame_type: FrameType,
        flags: u8,
        timestamp_samples: u32,
        payload: &[u8],
    ) -> TransportResult<()> {
        let frame = self
            .session
            .seal_frame(frame_type, flags, timestamp_samples, payload)?;
        frame.write_into_buf(&mut self.wire_scratch, &mut self.stream)?;
        Ok(())
    }

    pub fn send_control(&mut self, msg: &ControlMessage) -> TransportResult<()> {
        let encoded = msg.encode();
        self.send_frame(FrameType::Control, 0, 0, &encoded)?;
        self.stream.flush()?;
        Ok(())
    }

    pub fn send_pong(&mut self, pong: &PongPayload) -> TransportResult<()> {
        let encoded = pong.encode();
        self.send_frame(FrameType::Pong, 0, 0, &encoded)?;
        self.stream.flush()?;
        Ok(())
    }

    pub fn send_close(&mut self) -> TransportResult<()> {
        self.send_frame(FrameType::Close, 0, 0, &[])?;
        self.stream.flush()?;
        Ok(())
    }

    pub fn set_read_timeout(&self, dur: Option<Duration>) -> TransportResult<()> {
        self.stream.set_read_timeout(dur)?;
        Ok(())
    }

    pub fn recv_frame(&mut self) -> TransportResult<DecryptedFrame> {
        let encrypted = EncryptedFrame::read_from(&mut self.stream)?;
        let payload = self.session.open_frame(&encrypted)?;
        Ok(DecryptedFrame {
            header: encrypted.header,
            payload,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::io::ErrorKind;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Instant;

    use crate::transport::client::{Sht3ClientConfig, Sht3ClientConnection};
    use crate::transport::codec::AudioProfile;
    use crate::transport::InMemoryTrustStore;

    use super::*;

    #[test]
    fn handshake_timeout_rejects_silent_slowloris_socket() {
        let trust = Arc::new(InMemoryTrustStore::new());
        let mut server_cfg = Sht3ServerConfig::new("slowloris-server");
        // Set a much longer post-auth io_timeout to verify the strict 2s handshake timeout applies.
        server_cfg.io_timeout = Some(Duration::from_secs(30));

        let listener = Sht3Listener::bind(
            "127.0.0.1:0".parse().unwrap(),
            server_cfg.clone(),
            Arc::clone(&trust) as Arc<dyn TrustLookup>,
        )
        .unwrap();
        let addr = listener.local_addr().unwrap();

        let (hold_tx, hold_rx) = mpsc::channel::<()>();
        let slowloris = thread::spawn(move || {
            let mut stream = TcpStream::connect(addr).unwrap();
            // Send only the 4-byte magic and stall indefinitely without completing ClientHello.
            stream.write_all(b"SHT3").unwrap();
            stream.flush().unwrap();
            let _ = hold_rx.recv();
        });

        let (raw_stream, _peer) = listener.accept_raw().unwrap();
        let start = Instant::now();
        let err = Sht3Listener::handshake_arc(raw_stream, &server_cfg, &*trust)
            .err()
            .expect("silent slowloris socket must time out during handshake");
        let elapsed = start.elapsed();

        assert!(
            matches!(
                err,
                TransportError::Io(ref e)
                    if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
            ),
            "expected handshake timeout IO error, got: {err:?}"
        );
        assert!(
            elapsed >= Duration::from_millis(1500) && elapsed < Duration::from_secs(5),
            "expected ~2s handshake timeout, got {:?}",
            elapsed
        );

        let _ = hold_tx.send(());
        slowloris.join().unwrap();
    }

    #[test]
    fn handshake_applies_post_auth_io_timeout() {
        let k_pair = [0x3D; 32];
        let trust = Arc::new(InMemoryTrustStore::new());
        trust.insert("quiet-client", k_pair);

        let mut server_cfg = Sht3ServerConfig::new("post-auth-server");
        server_cfg.io_timeout = Some(Duration::from_millis(120));

        let listener = Sht3Listener::bind(
            "127.0.0.1:0".parse().unwrap(),
            server_cfg.clone(),
            Arc::clone(&trust) as Arc<dyn TrustLookup>,
        )
        .unwrap();
        let addr = listener.local_addr().unwrap();

        let (done_tx, done_rx) = mpsc::channel::<()>();
        let client_handle = thread::spawn(move || {
            let cfg = Sht3ClientConfig::new(
                "quiet-client",
                AudioProfile::PcmLossless.to_audio_params(40),
            );
            let _conn = Sht3ClientConnection::connect(addr, cfg, &k_pair).unwrap();
            let _ = done_rx.recv();
        });

        let (raw_stream, _peer) = listener.accept_raw().unwrap();
        let mut server_conn =
            Sht3Listener::handshake_arc(raw_stream, &server_cfg, &*trust).unwrap();

        let start = Instant::now();
        let err = server_conn
            .recv_frame()
            .expect_err("idle client should trigger post-auth io_timeout");
        let elapsed = start.elapsed();

        assert!(matches!(
            err,
            TransportError::Io(ref e)
                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
        ));
        assert!(
            elapsed < Duration::from_millis(800),
            "expected ~120ms post-auth io_timeout (not 2s handshake timeout), got {:?}",
            elapsed
        );

        let _ = done_tx.send(());
        client_handle.join().unwrap();
    }

    #[test]
    fn set_read_timeout_updates_live_server_connection_timeout() {
        let k_pair = [0x4E; 32];
        let trust = Arc::new(InMemoryTrustStore::new());
        trust.insert("timeout-client", k_pair);

        let mut server_cfg = Sht3ServerConfig::new("live-timeout-server");
        server_cfg.io_timeout = Some(Duration::from_secs(2));

        let listener = Sht3Listener::bind(
            "127.0.0.1:0".parse().unwrap(),
            server_cfg.clone(),
            Arc::clone(&trust) as Arc<dyn TrustLookup>,
        )
        .unwrap();
        let addr = listener.local_addr().unwrap();

        let (done_tx, done_rx) = mpsc::channel::<()>();
        let client_handle = thread::spawn(move || {
            let cfg = Sht3ClientConfig::new(
                "timeout-client",
                AudioProfile::PcmLossless.to_audio_params(40),
            );
            let _conn = Sht3ClientConnection::connect(addr, cfg, &k_pair).unwrap();
            let _ = done_rx.recv();
        });

        let (raw_stream, _peer) = listener.accept_raw().unwrap();
        let mut server_conn =
            Sht3Listener::handshake_arc(raw_stream, &server_cfg, &*trust).unwrap();
        server_conn
            .set_read_timeout(Some(Duration::from_millis(25)))
            .unwrap();

        let start = Instant::now();
        let err = server_conn
            .recv_frame()
            .expect_err("updated live read timeout should fire promptly on idle client");
        let elapsed = start.elapsed();

        assert!(
            matches!(
                err,
                TransportError::Io(ref e)
                    if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
            ),
            "expected WouldBlock | TimedOut after set_read_timeout, got: {err:?}"
        );
        assert!(
            elapsed < Duration::from_millis(400),
            "expected <400ms live read timeout, got {:?}",
            elapsed
        );

        let _ = done_tx.send(());
        client_handle.join().unwrap();
    }
}
