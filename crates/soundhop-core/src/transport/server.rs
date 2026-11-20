//! SHT3 TCP listener and authenticated server-side connection.

use std::io::Write;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use crate::transport::crypto::{
    compute_server_hello_mac, generate_nonce, verify_client_hello_mac, AeadSession,
    EphemeralKeypair, Role, SessionKeys,
};
use crate::transport::wire::{
    validate_version, AudioParams, ClientHello, CodecId, ControlMessage, DecryptedFrame,
    EncryptedFrame, FrameType, HandshakeStatus, PongPayload, ServerHello,
};
use crate::transport::{TransportError, TransportResult, TrustLookup};

/// Configuration for [`Sht3Listener`].
#[derive(Debug, Clone)]
pub struct Sht3ServerConfig {
    pub server_device_id: String,
    pub initial_volume: u8,
    pub initial_muted: bool,
    pub supported_codecs: Vec<CodecId>,
    pub io_timeout: Option<Duration>,
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

    /// Accept one TCP connection and perform the SHT3 mutual-auth handshake.
    ///
    /// Unknown peers or peers with an invalid `ClientHello` MAC are immediately sent
    /// `HandshakeStatus::AuthFailed` and rejected with [`TransportError::AuthRejected`]
    /// before any audio/codec state is allocated.
    pub fn accept(&self) -> TransportResult<Sht3ServerConnection> {
        let (stream, _peer_addr) = self.listener.accept()?;
        self.handshake(stream)
    }

    /// Perform the SHT3 server handshake on an already-accepted [`TcpStream`].
    pub fn handshake(&self, mut stream: TcpStream) -> TransportResult<Sht3ServerConnection> {
        stream.set_nodelay(true)?;
        stream.set_read_timeout(self.config.io_timeout)?;
        stream.set_write_timeout(self.config.io_timeout)?;

        let (client_hello, client_transcript) = ClientHello::read_from(&mut stream)?;

        if !validate_version(client_hello.version) {
            let err_hello = ServerHello::error(
                HandshakeStatus::UnsupportedVersion,
                self.config.server_device_id.clone(),
            );
            let _ = err_hello.write_to(&mut stream);
            let _ = stream.flush();
            return Err(TransportError::Wire(format!(
                "unsupported SHT3 version: {}",
                client_hello.version
            )));
        }

        let Some(k_pair) = self.trust.lookup_peer_key(&client_hello.client_device_id) else {
            send_auth_failed(&mut stream, &self.config.server_device_id);
            return Err(TransportError::AuthRejected(format!(
                "unknown or unpaired device '{}'",
                client_hello.client_device_id
            )));
        };

        if !verify_client_hello_mac(&k_pair, &client_transcript, &client_hello.mac_c) {
            send_auth_failed(&mut stream, &self.config.server_device_id);
            return Err(TransportError::AuthRejected(format!(
                "invalid ClientHello MAC from '{}'",
                client_hello.client_device_id
            )));
        }

        if !self
            .config
            .supported_codecs
            .contains(&client_hello.audio_params.codec)
        {
            let err_hello = ServerHello::error(
                HandshakeStatus::UnsupportedCodec,
                self.config.server_device_id.clone(),
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
            server_device_id: self.config.server_device_id.clone(),
            server_pk,
            server_nonce,
            audio_params: negotiated_params,
            initial_volume: self.config.initial_volume,
            initial_muted: self.config.initial_muted,
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

        Ok(Sht3ServerConnection {
            stream,
            session,
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
        frame.write_to(&mut self.stream)?;
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

    pub fn recv_frame(&mut self) -> TransportResult<DecryptedFrame> {
        let encrypted = EncryptedFrame::read_from(&mut self.stream)?;
        let payload = self.session.open_frame(&encrypted)?;
        Ok(DecryptedFrame {
            header: encrypted.header,
            payload,
        })
    }
}
