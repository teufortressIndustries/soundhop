//! SHT4 TCP control-channel listener and authenticated server-side connection.

use std::io::Write;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use zeroize::Zeroizing;

use crate::config::DEFAULT_RTP_PORT;
use crate::transport::client::{bounded_write_timeout, configure_socket_defaults};
use crate::transport::crypto::{
    compute_server_hello_mac, generate_nonce, verify_client_hello_mac, AeadSession,
    EphemeralKeypair, Role, SessionKeys,
};
use crate::transport::wire::{
    is_valid_ssrc, validate_version, AudioParams, ClientHello, CodecId, ControlMessage,
    DecryptedFrame, EncryptedFrame, FrameType, HandshakeStatus, PongPayload, ServerHello,
    SRTP_SUITE_AES128_ICM_HMAC_SHA1_80,
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
    /// Preferred UDP port for encrypted SRTP media reception (0 = OS ephemeral).
    pub rtp_port: u16,
    pub io_timeout: Option<Duration>,
}

impl Sht3ServerConfig {
    pub fn new(server_device_id: impl Into<String>) -> Self {
        Self {
            server_device_id: server_device_id.into(),
            initial_volume: 100,
            initial_muted: false,
            supported_codecs: vec![CodecId::Opus, CodecId::PcmS16Le],
            rtp_port: DEFAULT_RTP_PORT,
            io_timeout: Some(Duration::from_secs(10)),
        }
    }

    pub fn with_rtp_port(mut self, rtp_port: u16) -> Self {
        self.rtp_port = rtp_port;
        self
    }
}

/// TCP listener speaking SHT4 with mandatory `TrustLookup` mutual authentication.
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

    /// Accept one raw TCP connection without running the SHT4 handshake yet.
    pub fn accept_raw(&self) -> TransportResult<(TcpStream, SocketAddr)> {
        Ok(self.listener.accept()?)
    }

    /// Accept one TCP connection and perform the SHT4 mutual-auth handshake.
    ///
    /// Unknown peers or peers with an invalid `ClientHello` MAC are immediately sent
    /// `HandshakeStatus::AuthFailed` and rejected with [`TransportError::AuthRejected`]
    /// before any media or receiver resources are allocated.
    pub fn accept(&self) -> TransportResult<Sht3ServerConnection> {
        let (stream, _peer_addr) = self.accept_raw()?;
        self.handshake(stream)
    }

    /// Perform the SHT4 server handshake on an already-accepted [`TcpStream`].
    pub fn handshake(&self, stream: TcpStream) -> TransportResult<Sht3ServerConnection> {
        Self::handshake_arc(stream, &self.config, self.trust.as_ref())
    }

    /// Perform the SHT4 server handshake using `config.rtp_port` as the advertised RTP port.
    pub fn handshake_arc(
        stream: TcpStream,
        config: &Sht3ServerConfig,
        trust: &dyn TrustLookup,
    ) -> TransportResult<Sht3ServerConnection> {
        let advertised_port = if config.rtp_port == 0 {
            DEFAULT_RTP_PORT
        } else {
            config.rtp_port
        };
        let (conn, ()) = Self::handshake_with_media(
            stream,
            config,
            trust,
            |_peer_id, _params| Ok((advertised_port, ())),
            |(), _ssrc, _keys| Ok(()),
        )?;
        Ok(conn)
    }

    /// Perform the SHT4 server handshake with two-phase media setup:
    ///
    /// 1. Validates protocol version, `TrustLookup` key presence, `ClientHello` MAC, SRTP suite,
    ///    SSRC, and codec **before** invoking `prepare_media`.
    /// 2. Calls `prepare_media(peer_device_id, negotiated_params)` to bind the UDP SRTP port and
    ///    return `(bound_rtp_port, prepared_media)`.
    /// 3. Binds `bound_rtp_port` into `ServerHello`, computes `mac_s`, and derives `SessionKeys`.
    /// 4. Calls `activate_media(prepared_media, sender_ssrc, &keys)` **before** flushing
    ///    `ServerHello` to the client so the SRTP receiver is already `Playing` before the client
    ///    can transmit its first packet.
    pub fn handshake_with_media<P, A, FPrep, FAct>(
        mut stream: TcpStream,
        config: &Sht3ServerConfig,
        trust: &dyn TrustLookup,
        prepare_media: FPrep,
        activate_media: FAct,
    ) -> TransportResult<(Sht3ServerConnection, A)>
    where
        FPrep: FnOnce(&str, AudioParams) -> TransportResult<(u16, P)>,
        FAct: FnOnce(P, u32, &SessionKeys) -> TransportResult<A>,
    {
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
                "unsupported SHT4 version: {}",
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

        if !is_valid_ssrc(client_hello.sender_ssrc) {
            send_error_hello(
                &mut stream,
                HandshakeStatus::InternalError,
                &config.server_device_id,
            );
            return Err(TransportError::Wire(format!(
                "invalid sender_ssrc 0x{:08X} in ClientHello",
                client_hello.sender_ssrc
            )));
        }

        if client_hello.srtp_suite != SRTP_SUITE_AES128_ICM_HMAC_SHA1_80 {
            send_error_hello(
                &mut stream,
                HandshakeStatus::UnsupportedCodec,
                &config.server_device_id,
            );
            return Err(TransportError::UnsupportedProfile(format!(
                "unsupported SRTP suite {}",
                client_hello.srtp_suite
            )));
        }

        if !config
            .supported_codecs
            .contains(&client_hello.audio_params.codec)
        {
            send_error_hello(
                &mut stream,
                HandshakeStatus::UnsupportedCodec,
                &config.server_device_id,
            );
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

        let (bound_rtp_port, prepared_media) =
            match prepare_media(&client_hello.client_device_id, negotiated_params) {
                Ok(res) if res.0 > 0 => res,
                Ok(_) => {
                    send_error_hello(
                        &mut stream,
                        HandshakeStatus::InternalError,
                        &config.server_device_id,
                    );
                    return Err(TransportError::Pipeline(
                        "media receiver bound invalid RTP port 0".into(),
                    ));
                }
                Err(err) => {
                    send_error_hello(
                        &mut stream,
                        HandshakeStatus::InternalError,
                        &config.server_device_id,
                    );
                    return Err(err);
                }
            };

        let mut server_hello = ServerHello {
            status: HandshakeStatus::Ok,
            server_device_id: config.server_device_id.clone(),
            server_pk,
            server_nonce,
            audio_params: negotiated_params,
            initial_volume: config.initial_volume,
            initial_muted: config.initial_muted,
            rtp_port: bound_rtp_port,
            srtp_suite: SRTP_SUITE_AES128_ICM_HMAC_SHA1_80,
            mac_s: [0u8; 32],
        };
        let server_body = server_hello.encode_body()?;

        let mut full_transcript = Vec::with_capacity(client_transcript.len() + server_body.len());
        full_transcript.extend_from_slice(&client_transcript);
        full_transcript.extend_from_slice(&server_body);

        server_hello.mac_s = compute_server_hello_mac(&k_pair, &full_transcript);

        let keys = SessionKeys::derive(
            shared_dh.as_bytes(),
            &k_pair,
            &client_hello.client_nonce,
            &server_nonce,
            &full_transcript,
        )?;

        let activated_media = match activate_media(prepared_media, client_hello.sender_ssrc, &keys)
        {
            Ok(m) => m,
            Err(err) => {
                send_error_hello(
                    &mut stream,
                    HandshakeStatus::InternalError,
                    &config.server_device_id,
                );
                return Err(err);
            }
        };

        server_hello.write_to(&mut stream)?;
        stream.flush()?;

        let srtp_master_key_c2s = Zeroizing::new(keys.srtp_master_key_c2s);
        let srtp_master_key_s2c = Zeroizing::new(keys.srtp_master_key_s2c);
        let session = AeadSession::new(Role::Server, &keys);

        stream.set_read_timeout(config.io_timeout)?;
        stream.set_write_timeout(bounded_write_timeout(config.io_timeout))?;

        Ok((
            Sht3ServerConnection {
                stream,
                session,
                wire_scratch: Vec::with_capacity(INITIAL_WIRE_SCRATCH_CAP),
                peer_device_id: client_hello.client_device_id,
                audio_params: negotiated_params,
                sender_ssrc: client_hello.sender_ssrc,
                rtp_port: bound_rtp_port,
                srtp_master_key_c2s,
                srtp_master_key_s2c,
            },
            activated_media,
        ))
    }
}

fn send_auth_failed(stream: &mut TcpStream, server_device_id: &str) {
    send_error_hello(stream, HandshakeStatus::AuthFailed, server_device_id);
}

fn send_error_hello(stream: &mut TcpStream, status: HandshakeStatus, server_device_id: &str) {
    let err_hello = ServerHello::error(status, server_device_id.to_string());
    let _ = err_hello.write_to(stream);
    let _ = stream.flush();
}

/// Authenticated, encrypted server-side SHT4 control connection.
pub struct Sht3ServerConnection {
    stream: TcpStream,
    session: AeadSession,
    wire_scratch: Vec<u8>,
    peer_device_id: String,
    audio_params: AudioParams,
    sender_ssrc: u32,
    rtp_port: u16,
    srtp_master_key_c2s: Zeroizing<[u8; 30]>,
    srtp_master_key_s2c: Zeroizing<[u8; 30]>,
}

impl Sht3ServerConnection {
    pub fn peer_device_id(&self) -> &str {
        &self.peer_device_id
    }

    pub fn audio_params(&self) -> &AudioParams {
        &self.audio_params
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
    use crate::transport::wire::AudioProfile;
    use crate::transport::InMemoryTrustStore;

    use super::*;

    #[test]
    fn handshake_timeout_rejects_silent_slowloris_socket() {
        let trust = Arc::new(InMemoryTrustStore::new());
        let mut server_cfg = Sht3ServerConfig::new("slowloris-server");
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
