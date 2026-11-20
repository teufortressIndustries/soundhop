//! SoundHop Transport v3 (`SHT3`): encrypted Opus & PCM audio transport.
//!
//! Provides:
//! - Mutual-authenticated, forward-secret handshake (`X25519` + per-device `K_pair` HMAC-SHA256 + HKDF-SHA256)
//! - `ChaCha20-Poly1305` AEAD frame encryption with 16-byte plaintext header AAD and strict replay protection
//! - `AudioCodec` abstraction with zero-dep `PcmCodec` and pure-Rust `OpusCodec` (behind default `opus` feature)
//! - Adaptive jitter buffer with PLC and overrun handling
//! - Standalone DSP helpers (`apply_gain_i16`, `LinearResampler`, `PlaybackResampler`)
//! - Cross-platform 48 kHz stereo PCM traits (`PcmCapture` / `PcmPlayback`) and OS bridges (`linux_pcm`, `windows_pcm`)
//! - TCP `SHT3` server (`Sht3Listener` / `Sht3ServerConnection`), client (`Sht3ClientConnection`),
//!   and session orchestration (`Sht3SessionManager` / `TransportManager`) with live peer revocation

pub mod client;
pub mod codec;
pub mod crypto;
pub mod dsp;
pub mod jitter;
#[cfg(target_os = "linux")]
pub mod linux_pcm;
pub mod pcm;
pub mod server;
#[cfg(target_os = "windows")]
pub mod windows_pcm;
pub mod wire;

use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use thiserror::Error;

pub use client::{Sht3ClientConfig, Sht3ClientConnection};
#[cfg(feature = "opus")]
pub use codec::OpusCodec;
pub use codec::{create_codec, AudioCodec, AudioProfile, PcmCodec};
pub use crypto::{AeadSession, EphemeralKeypair, Role, SessionKeys};
pub use dsp::{apply_gain_buffer, apply_gain_i16, LinearResampler, PlaybackResampler};
pub use jitter::{JitterBuffer, JitterConfig, JitterStats, PopOutcome};
#[cfg(target_os = "linux")]
pub use linux_pcm::{LinuxPcmCapture, LinuxPcmPlayback};
pub use pcm::{
    MockPcmCapture, MockPcmPlayback, MockPcmPlaybackHandle, PcmCapture, PcmPlayback,
    PcmPlaybackFactory, PCM_CHANNELS, PCM_FRAME_SAMPLES_10MS, PCM_FRAME_SAMPLES_20MS,
    PCM_SAMPLE_RATE,
};
pub use server::{Sht3Listener, Sht3ServerConfig, Sht3ServerConnection};
#[cfg(target_os = "windows")]
pub use windows_pcm::{WindowsPcmCapture, WindowsPcmPlayback};
pub use wire::{
    AudioParams, ClientHello, CodecId, ControlMessage, DecryptedFrame, EncryptedFrame, FrameHeader,
    FrameType, HandshakeStatus, PingPayload, PongPayload, ServerHello, FLAG_DISCONTINUITY,
    FLAG_DTX, FLAG_FEC,
};

/// 4-byte protocol magic for SoundHop Transport v3.
pub const SHT3_MAGIC: [u8; 4] = *b"SHT3";
/// Wire protocol version.
pub const SHT3_VERSION: u8 = 3;
/// Default dedicated TCP port for SHT3 (leaving 4713/tcp for legacy PA-TCP).
pub const DEFAULT_V3_PORT: u16 = 4714;
/// Maximum encrypted frame payload length in bytes (pre-allocation cap).
pub const MAX_PAYLOAD_LEN: usize = 8192;
/// Fixed plaintext frame header length in bytes (used as AEAD AAD).
pub const FRAME_HEADER_LEN: usize = 16;
/// Poly1305 authentication tag length in bytes.
pub const AEAD_TAG_LEN: usize = 16;
/// Maximum number of frames per directional key before rekey is required (~93h @ 50fps).
pub const MAX_FRAMES_PER_KEY: u64 = 1 << 24;

/// Maximum per-read timeout on managed inbound sessions so `disconnect_peer` completes promptly.
const MAX_MANAGED_INBOUND_IO_TIMEOUT: Duration = Duration::from_millis(150);
/// Default outbound control/pong read timeout.
const DEFAULT_OUTBOUND_IO_TIMEOUT: Duration = Duration::from_millis(200);
/// Default interval between outbound `Ping` telemetry probes.
const DEFAULT_PING_INTERVAL: Duration = Duration::from_millis(500);

/// Result alias for transport operations.
pub type TransportResult<T> = Result<T, TransportError>;

/// Errors returned by the SHT3 transport layer.
#[derive(Debug, Error)]
pub enum TransportError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("crypto error: {0}")]
    Crypto(String),

    #[error("wire protocol error: {0}")]
    Wire(String),

    #[error("codec error: {0}")]
    Codec(String),

    #[error("authentication rejected: {0}")]
    AuthRejected(String),

    #[error("replay or out-of-order frame detected: expected seq {expected}, got {got}")]
    ReplayDetected { expected: u64, got: u64 },

    #[error("payload length {len} exceeds maximum allowed {max} bytes")]
    PayloadTooLarge { len: usize, max: usize },

    #[error("session frame limit reached; rekey required")]
    RekeyRequired,

    #[error("unsupported audio profile or codec: {0}")]
    UnsupportedProfile(String),

    #[error("connection closed")]
    Closed,
}

/// Trust store lookup interface used by the SHT3 server to authenticate paired peers.
pub trait TrustLookup: Send + Sync {
    /// Look up the 32-byte pairwise pre-shared key `K_pair` for `device_id`.
    /// Returns `None` if the peer is unknown or revoked.
    fn lookup_peer_key(&self, device_id: &str) -> Option<[u8; 32]>;
}

/// Thread-safe in-memory [`TrustLookup`] implementation for tests and lightweight embeds.
#[derive(Debug, Default)]
pub struct InMemoryTrustStore {
    peers: RwLock<HashMap<String, [u8; 32]>>,
}

impl InMemoryTrustStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, device_id: impl Into<String>, k_pair: [u8; 32]) {
        if let Ok(mut guard) = self.peers.write() {
            guard.insert(device_id.into(), k_pair);
        }
    }

    pub fn remove(&self, device_id: &str) -> Option<[u8; 32]> {
        self.peers
            .write()
            .ok()
            .and_then(|mut guard| guard.remove(device_id))
    }
}

impl TrustLookup for InMemoryTrustStore {
    fn lookup_peer_key(&self, device_id: &str) -> Option<[u8; 32]> {
        self.peers
            .read()
            .ok()
            .and_then(|guard| guard.get(device_id).copied())
    }
}

/// Live telemetry snapshot reported by an outbound SHT3 audio stream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StreamTelemetry {
    /// Most recent round-trip time measured via `Ping`/`Pong` in microseconds.
    pub rtt_us: u64,
    /// Receiver jitter buffer depth reported in the latest `Pong`, in milliseconds.
    pub jitter_buf_ms: u16,
    /// Cumulative receiver dropped frames reported in the latest `Pong`.
    pub dropped_frames: u32,
    /// Total encoded audio frames sent by this session.
    pub frames_sent: u64,
    /// Total encoded audio payload bytes sent by this session.
    pub bytes_sent: u64,
    /// Number of `Pong` telemetry replies received.
    pub pongs_received: u64,
    /// Last known remote receiver volume percentage (`0..=150`).
    pub remote_volume: u8,
    /// Last known remote receiver mute state.
    pub remote_muted: bool,
    /// Whether the outbound streaming worker is currently active.
    pub active: bool,
}

/// Configuration for an outbound SHT3 audio streaming session.
#[derive(Debug, Clone)]
pub struct OutboundConfig {
    pub client_device_id: String,
    pub target_addr: SocketAddr,
    pub k_pair: [u8; 32],
    pub profile: AudioProfile,
    pub jitter_ms: u16,
    pub ping_interval: Duration,
    pub io_timeout: Option<Duration>,
}

impl OutboundConfig {
    /// Create an [`OutboundConfig`] targeting `target_addr` with the default profile
    /// (`OpusBalanced` when compiled with the `opus` feature, else `PcmLossless`).
    pub fn new(
        client_device_id: impl Into<String>,
        target_addr: SocketAddr,
        k_pair: [u8; 32],
    ) -> Self {
        #[cfg(feature = "opus")]
        let profile = AudioProfile::OpusBalanced;
        #[cfg(not(feature = "opus"))]
        let profile = AudioProfile::PcmLossless;

        Self {
            client_device_id: client_device_id.into(),
            target_addr,
            k_pair,
            profile,
            jitter_ms: 40,
            ping_interval: DEFAULT_PING_INTERVAL,
            io_timeout: Some(DEFAULT_OUTBOUND_IO_TIMEOUT),
        }
    }

    pub fn with_profile(mut self, profile: AudioProfile) -> Self {
        self.profile = profile;
        self
    }

    pub fn with_jitter_ms(mut self, jitter_ms: u16) -> Self {
        self.jitter_ms = jitter_ms;
        self
    }

    pub fn with_ping_interval(mut self, interval: Duration) -> Self {
        self.ping_interval = interval;
        self
    }
}

enum OutboundCommand {
    Control(ControlMessage),
    Ping,
    Stop,
}

/// Handle to a running outbound SHT3 audio stream.
///
/// Dropping the handle stops the streaming worker and joins its thread cleanly.
pub struct OutboundSessionHandle {
    server_device_id: String,
    audio_params: AudioParams,
    stop_flag: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    cmd_tx: mpsc::Sender<OutboundCommand>,
    telemetry: Arc<Mutex<StreamTelemetry>>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl OutboundSessionHandle {
    /// Connect to the target SHT3 listener and start streaming PCM from `capture`.
    pub fn start(
        config: OutboundConfig,
        mut capture: Box<dyn PcmCapture>,
    ) -> TransportResult<Self> {
        let mut client_cfg = Sht3ClientConfig::new(
            config.client_device_id,
            config.profile.to_audio_params(config.jitter_ms),
        );
        client_cfg.io_timeout = config.io_timeout;

        let mut conn =
            Sht3ClientConnection::connect(config.target_addr, client_cfg, &config.k_pair)?;
        let audio_params = *conn.audio_params();
        let server_device_id = conn.server_device_id().to_string();
        let initial_volume = conn.initial_volume();
        let initial_muted = conn.initial_muted();

        let mut codec = create_codec(&audio_params)?;
        let frame_type = match audio_params.codec {
            CodecId::Opus => FrameType::AudioOpus,
            CodecId::PcmS16Le => FrameType::AudioPcm,
        };
        let frame_samples = codec.interleaved_samples_per_frame();
        let frame_spc = codec.frame_samples_per_channel() as u32;

        let stop_flag = Arc::new(AtomicBool::new(false));
        let running = Arc::new(AtomicBool::new(true));
        let telemetry = Arc::new(Mutex::new(StreamTelemetry {
            remote_volume: initial_volume,
            remote_muted: initial_muted,
            active: true,
            ..StreamTelemetry::default()
        }));
        let (cmd_tx, cmd_rx) = mpsc::channel::<OutboundCommand>();

        let stop_c = Arc::clone(&stop_flag);
        let running_c = Arc::clone(&running);
        let telemetry_c = Arc::clone(&telemetry);
        let ping_interval = config.ping_interval;

        let worker = thread::spawn(move || {
            let start_instant = Instant::now();
            let mut last_ping_at: Option<Instant> = None;
            let mut pcm_buf = vec![0i16; frame_samples];
            let mut encoded_buf = Vec::with_capacity(MAX_PAYLOAD_LEN);
            let mut timestamp_samples: u32 = 0;

            'stream: while !stop_c.load(Ordering::Acquire) {
                // Drain pending control/ping commands before encoding the next frame.
                while let Ok(cmd) = cmd_rx.try_recv() {
                    match cmd {
                        OutboundCommand::Control(msg) => {
                            if conn.send_control(&msg).is_err() {
                                break 'stream;
                            }
                            if matches!(
                                msg,
                                ControlMessage::SetVolume(_) | ControlMessage::SetMute(_)
                            ) && !poll_outbound_reply(
                                &mut conn,
                                &telemetry_c,
                                start_instant,
                                false,
                            ) {
                                break 'stream;
                            }
                        }
                        OutboundCommand::Ping => {
                            if !exchange_ping(&mut conn, &telemetry_c, start_instant) {
                                break 'stream;
                            }
                            last_ping_at = Some(Instant::now());
                        }
                        OutboundCommand::Stop => {
                            stop_c.store(true, Ordering::Release);
                            break 'stream;
                        }
                    }
                }

                let n = match capture.read_frame(&mut pcm_buf) {
                    Ok(0) => {
                        // Clean EOF: ensure at least one telemetry ping was exchanged if none yet.
                        let has_pong = telemetry_c
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .pongs_received
                            > 0;
                        if !has_pong {
                            let _ = exchange_ping(&mut conn, &telemetry_c, start_instant);
                        }
                        let _ = conn.send_close();
                        break 'stream;
                    }
                    Ok(n) => n,
                    Err(_) => break 'stream,
                };

                if stop_c.load(Ordering::Acquire) {
                    let _ = conn.send_close();
                    break 'stream;
                }

                if n < frame_samples {
                    pcm_buf[n..frame_samples].fill(0);
                }

                encoded_buf.clear();
                let Ok(pkt_len) = codec.encode(&pcm_buf[..frame_samples], &mut encoded_buf) else {
                    break 'stream;
                };

                if conn
                    .send_audio(frame_type, 0, timestamp_samples, &encoded_buf)
                    .is_err()
                    || conn.flush().is_err()
                {
                    break 'stream;
                }
                timestamp_samples = timestamp_samples.wrapping_add(frame_spc);

                {
                    let mut t = telemetry_c.lock().unwrap_or_else(|e| e.into_inner());
                    t.frames_sent += 1;
                    t.bytes_sent += pkt_len as u64;
                }

                let should_ping = match last_ping_at {
                    None => false,
                    Some(prev) => prev.elapsed() >= ping_interval,
                };
                if last_ping_at.is_none() {
                    last_ping_at = Some(Instant::now());
                } else if should_ping {
                    let now_us = start_instant
                        .elapsed()
                        .as_micros()
                        .min(u128::from(u64::MAX)) as u64;
                    let _ = conn.send_ping(&PingPayload {
                        client_time_us: now_us,
                    });
                    let _ = poll_outbound_reply_nonblocking(&mut conn, &telemetry_c, start_instant);
                    last_ping_at = Some(Instant::now());
                }
            }

            if stop_c.load(Ordering::Acquire) {
                let _ = conn.send_close();
            }

            running_c.store(false, Ordering::Release);
            let mut t = telemetry_c.lock().unwrap_or_else(|e| e.into_inner());
            t.active = false;
        });

        Ok(Self {
            server_device_id,
            audio_params,
            stop_flag,
            running,
            cmd_tx,
            telemetry,
            worker: Mutex::new(Some(worker)),
        })
    }

    /// Forward a `ControlMessage::SetVolume` to the remote receiver.
    pub fn set_volume(&self, pct: u8) -> TransportResult<()> {
        self.send_control(ControlMessage::SetVolume(pct.min(150)))
    }

    /// Forward a `ControlMessage::SetMute` to the remote receiver.
    pub fn set_mute(&self, muted: bool) -> TransportResult<()> {
        self.send_control(ControlMessage::SetMute(muted))
    }

    /// Enqueue an arbitrary [`ControlMessage`] to be sent over the encrypted stream.
    pub fn send_control(&self, msg: ControlMessage) -> TransportResult<()> {
        self.cmd_tx
            .send(OutboundCommand::Control(msg))
            .map_err(|_| TransportError::Closed)
    }

    /// Trigger an immediate `Ping`/`Pong` telemetry probe.
    pub fn request_ping(&self) -> TransportResult<()> {
        self.cmd_tx
            .send(OutboundCommand::Ping)
            .map_err(|_| TransportError::Closed)
    }

    /// Snapshot live RTT, jitter buffer, drop, and volume telemetry.
    pub fn telemetry(&self) -> StreamTelemetry {
        *self.telemetry.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Returns `true` while the outbound streaming worker is still running.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    /// Authenticated remote server `device_id`.
    pub fn server_device_id(&self) -> &str {
        &self.server_device_id
    }

    /// Negotiated [`AudioParams`] for this stream.
    pub fn audio_params(&self) -> AudioParams {
        self.audio_params
    }

    /// Request graceful shutdown and join the worker thread.
    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::Release);
        let _ = self.cmd_tx.send(OutboundCommand::Stop);
        if let Some(handle) = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = handle.join();
        }
    }

    /// Wait until the worker finishes on its own (e.g. finite capture EOF).
    pub fn join(&self) {
        if let Some(handle) = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = handle.join();
        }
    }
}

impl Drop for OutboundSessionHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

fn exchange_ping(
    conn: &mut Sht3ClientConnection,
    telemetry: &Arc<Mutex<StreamTelemetry>>,
    start_instant: Instant,
) -> bool {
    let now_us = start_instant
        .elapsed()
        .as_micros()
        .min(u128::from(u64::MAX)) as u64;
    if conn
        .send_ping(&PingPayload {
            client_time_us: now_us,
        })
        .is_err()
    {
        return false;
    }
    poll_outbound_reply(conn, telemetry, start_instant, true)
}

fn poll_outbound_reply_nonblocking(
    conn: &mut Sht3ClientConnection,
    telemetry: &Arc<Mutex<StreamTelemetry>>,
    start_instant: Instant,
) -> bool {
    let _ = conn.set_read_timeout(Some(Duration::from_millis(2)));
    let ok = poll_outbound_reply(conn, telemetry, start_instant, true);
    let _ = conn.set_read_timeout(Some(DEFAULT_OUTBOUND_IO_TIMEOUT));
    ok
}

/// Read reply frame(s) from the server after sending a `Ping` or `Control` message.
/// Returns `false` if the server closed or failed the connection.
fn poll_outbound_reply(
    conn: &mut Sht3ClientConnection,
    telemetry: &Arc<Mutex<StreamTelemetry>>,
    start_instant: Instant,
    expect_pong: bool,
) -> bool {
    for _ in 0..4 {
        match conn.recv_frame() {
            Ok(frame) => match frame.header.frame_type {
                FrameType::Pong => {
                    if let Ok(pong) = PongPayload::decode(&frame.payload) {
                        let now_us = start_instant
                            .elapsed()
                            .as_micros()
                            .min(u128::from(u64::MAX)) as u64;
                        let rtt_us = now_us.saturating_sub(pong.client_time_us);
                        let mut t = telemetry.lock().unwrap_or_else(|e| e.into_inner());
                        t.rtt_us = rtt_us;
                        t.jitter_buf_ms = pong.jitter_buf_ms;
                        t.dropped_frames = pong.dropped_frames;
                        t.pongs_received += 1;
                    }
                    if expect_pong {
                        return true;
                    }
                }
                FrameType::Control => {
                    if let Ok(ControlMessage::VolumeState { volume, muted }) =
                        ControlMessage::decode(&frame.payload)
                    {
                        let mut t = telemetry.lock().unwrap_or_else(|e| e.into_inner());
                        t.remote_volume = volume;
                        t.remote_muted = muted;
                    }
                    if !expect_pong {
                        return true;
                    }
                }
                FrameType::Close => return false,
                _ => {}
            },
            Err(TransportError::Io(ref e))
                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
            {
                return true;
            }
            Err(_) => return false,
        }
    }
    true
}

struct ActiveInboundPeer {
    session_id: u64,
    stop_flag: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

struct RunningListener {
    local_addr: SocketAddr,
    stop_flag: Arc<AtomicBool>,
    accept_thread: Option<JoinHandle<()>>,
}

/// Manages SHT3 inbound listener sessions (with per-`device_id` instant revocation)
/// and outbound PCM-to-SHT3 audio streams.
pub struct Sht3SessionManager {
    listener: Mutex<Option<RunningListener>>,
    active_peers: Arc<Mutex<HashMap<String, ActiveInboundPeer>>>,
    next_session_id: Arc<AtomicU64>,
}

/// Convenient alias for [`Sht3SessionManager`].
pub type TransportManager = Sht3SessionManager;

impl Default for Sht3SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl Sht3SessionManager {
    pub fn new() -> Self {
        Self {
            listener: Mutex::new(None),
            active_peers: Arc::new(Mutex::new(HashMap::new())),
            next_session_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Bind and start the SHT3 inbound listener on `addr`.
    ///
    /// Authenticates incoming peers against `trust`, spawns a [`PcmPlayback`] sink per
    /// authenticated peer via `playback_factory`, and tracks active sessions by `device_id`
    /// so [`Self::disconnect_peer`] can drop any revoked peer immediately.
    pub fn start_listener(
        &self,
        addr: SocketAddr,
        mut config: Sht3ServerConfig,
        trust: Arc<dyn TrustLookup>,
        playback_factory: PcmPlaybackFactory,
    ) -> TransportResult<SocketAddr> {
        self.stop_listener();

        // Cap per-read timeout so idle inbound sessions observe disconnect_peer/stop promptly.
        config.io_timeout = Some(
            config
                .io_timeout
                .map_or(MAX_MANAGED_INBOUND_IO_TIMEOUT, |d| {
                    d.min(MAX_MANAGED_INBOUND_IO_TIMEOUT)
                }),
        );
        let initial_volume = config.initial_volume;
        let initial_muted = config.initial_muted;

        let listener = Sht3Listener::bind(addr, config, trust)?;
        let local_addr = listener.local_addr()?;

        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_c = Arc::clone(&stop_flag);
        let peers_c = Arc::clone(&self.active_peers);
        let next_id_c = Arc::clone(&self.next_session_id);

        let accept_thread = thread::spawn(move || {
            while !stop_c.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok(conn) => {
                        if stop_c.load(Ordering::Acquire) {
                            drop(conn);
                            break;
                        }
                        spawn_inbound_peer(
                            conn,
                            &peers_c,
                            &next_id_c,
                            &stop_c,
                            &playback_factory,
                            initial_volume,
                            initial_muted,
                        );
                    }
                    Err(_) => {
                        if stop_c.load(Ordering::Acquire) {
                            break;
                        }
                        // Auth failure, malformed handshake, or transient socket error: keep listening.
                    }
                }
            }
        });

        let mut guard = self.listener.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(RunningListener {
            local_addr,
            stop_flag,
            accept_thread: Some(accept_thread),
        });
        Ok(local_addr)
    }

    /// Address the SHT3 listener is currently bound to, if running.
    pub fn listener_addr(&self) -> Option<SocketAddr> {
        self.listener
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|l| l.local_addr)
    }

    /// Immediately terminate the active inbound stream for `device_id` (e.g. on peer revocation).
    ///
    /// Returns `true` if an active session for `device_id` was found and disconnected.
    pub fn disconnect_peer(&self, device_id: &str) -> bool {
        let removed = {
            let mut guard = self.active_peers.lock().unwrap_or_else(|e| e.into_inner());
            guard.remove(device_id)
        };
        if let Some(mut peer) = removed {
            peer.stop_flag.store(true, Ordering::Release);
            if let Some(handle) = peer.worker.take() {
                let _ = handle.join();
            }
            true
        } else {
            false
        }
    }

    /// List `device_id`s of all currently connected inbound peers.
    pub fn active_inbound_peers(&self) -> Vec<String> {
        self.active_peers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect()
    }

    /// Number of currently connected inbound peers.
    pub fn inbound_peer_count(&self) -> usize {
        self.active_peers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    /// Start an outbound SHT3 audio streaming session from `capture` to `config.target_addr`.
    pub fn start_outbound(
        &self,
        config: OutboundConfig,
        capture: Box<dyn PcmCapture>,
    ) -> TransportResult<OutboundSessionHandle> {
        OutboundSessionHandle::start(config, capture)
    }

    /// Stop the SHT3 listener and immediately disconnect all active inbound peer sessions.
    pub fn stop_listener(&self) {
        let running = {
            let mut guard = self.listener.lock().unwrap_or_else(|e| e.into_inner());
            guard.take()
        };
        if let Some(mut l) = running {
            l.stop_flag.store(true, Ordering::Release);
            // Wake the blocking TcpListener::accept() call.
            let wake_addr = wake_socket_addr(l.local_addr);
            let _ = TcpStream::connect_timeout(&wake_addr, Duration::from_millis(100));
            if let Some(handle) = l.accept_thread.take() {
                let _ = handle.join();
            }
        }

        // Disconnect and join all active inbound peers.
        let peers: Vec<ActiveInboundPeer> = {
            let mut guard = self.active_peers.lock().unwrap_or_else(|e| e.into_inner());
            guard.drain().map(|(_, v)| v).collect()
        };
        for mut peer in peers {
            peer.stop_flag.store(true, Ordering::Release);
            if let Some(handle) = peer.worker.take() {
                let _ = handle.join();
            }
        }
    }
}

impl Drop for Sht3SessionManager {
    fn drop(&mut self) {
        self.stop_listener();
    }
}

fn wake_socket_addr(addr: SocketAddr) -> SocketAddr {
    match addr.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => {
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), addr.port())
        }
        IpAddr::V6(ip) if ip.is_unspecified() => {
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), addr.port())
        }
        _ => addr,
    }
}

fn spawn_inbound_peer(
    mut conn: Sht3ServerConnection,
    active_peers: &Arc<Mutex<HashMap<String, ActiveInboundPeer>>>,
    next_session_id: &Arc<AtomicU64>,
    manager_stop: &Arc<AtomicBool>,
    playback_factory: &PcmPlaybackFactory,
    initial_volume: u8,
    initial_muted: bool,
) {
    let peer_id = conn.peer_device_id().to_string();
    let audio_params = *conn.audio_params();

    let Ok(mut codec) = create_codec(&audio_params) else {
        let _ = conn.send_close();
        return;
    };
    let Ok(mut playback) = (playback_factory)(&peer_id) else {
        let _ = conn.send_close();
        return;
    };
    playback.set_volume(initial_volume);
    playback.set_mute(initial_muted);

    let session_id = next_session_id.fetch_add(1, Ordering::Relaxed);
    let peer_stop = Arc::new(AtomicBool::new(false));

    // Replace any pre-existing session for the same device_id.
    let previous = {
        let mut guard = active_peers.lock().unwrap_or_else(|e| e.into_inner());
        guard.remove(&peer_id)
    };
    if let Some(mut prev) = previous {
        prev.stop_flag.store(true, Ordering::Release);
        if let Some(h) = prev.worker.take() {
            let _ = h.join();
        }
    }

    let peer_stop_c = Arc::clone(&peer_stop);
    let manager_stop_c = Arc::clone(manager_stop);
    let peers_c = Arc::clone(active_peers);
    let peer_id_c = peer_id.clone();

    let worker = thread::spawn(move || {
        let mut jb = JitterBuffer::new(JitterConfig {
            target_ms: audio_params.jitter_ms,
            frame_ms: audio_params.frame_ms,
            ..JitterConfig::default()
        });
        let mut decoded_pcm = Vec::with_capacity(codec.interleaved_samples_per_frame());
        let mut clean_close = false;

        while !peer_stop_c.load(Ordering::Acquire) && !manager_stop_c.load(Ordering::Acquire) {
            match conn.recv_frame() {
                Ok(frame) => {
                    if peer_stop_c.load(Ordering::Acquire) || manager_stop_c.load(Ordering::Acquire)
                    {
                        break;
                    }
                    match frame.header.frame_type {
                        FrameType::AudioOpus | FrameType::AudioPcm => {
                            if frame.header.flags & FLAG_DISCONTINUITY != 0 {
                                jb.reset();
                            }
                            decoded_pcm.clear();
                            if codec.decode(&frame.payload, &mut decoded_pcm).is_err() {
                                continue;
                            }
                            jb.push(decoded_pcm.clone());
                            if jb.is_primed() {
                                let mut write_failed = false;
                                while jb.stats().buffered_frames > 0 {
                                    if let PopOutcome::Frame(frame) = jb.pop() {
                                        if playback.write_frame(&frame).is_err() {
                                            write_failed = true;
                                            break;
                                        }
                                    } else {
                                        break;
                                    }
                                }
                                if write_failed {
                                    break;
                                }
                            }
                        }
                        FrameType::Control => {
                            if let Ok(msg) = ControlMessage::decode(&frame.payload) {
                                match msg {
                                    ControlMessage::SetVolume(pct) => {
                                        playback.set_volume(pct);
                                        let _ = conn.send_control(&ControlMessage::VolumeState {
                                            volume: playback.volume(),
                                            muted: playback.is_muted(),
                                        });
                                    }
                                    ControlMessage::SetMute(muted) => {
                                        playback.set_mute(muted);
                                        let _ = conn.send_control(&ControlMessage::VolumeState {
                                            volume: playback.volume(),
                                            muted: playback.is_muted(),
                                        });
                                    }
                                    ControlMessage::VolumeState { .. }
                                    | ControlMessage::BitrateHint(_)
                                    | ControlMessage::DelayOffsetMs(_) => {}
                                }
                            }
                        }
                        FrameType::Ping => {
                            if let Ok(ping) = PingPayload::decode(&frame.payload) {
                                let stats = jb.stats();
                                let pong = PongPayload {
                                    client_time_us: ping.client_time_us,
                                    jitter_buf_ms: stats.buffered_ms,
                                    dropped_frames: stats.dropped_overrun.min(u64::from(u32::MAX))
                                        as u32,
                                };
                                if conn.send_pong(&pong).is_err() {
                                    break;
                                }
                            }
                        }
                        FrameType::Close => {
                            clean_close = true;
                            break;
                        }
                        FrameType::Pong => {}
                    }
                }
                Err(TransportError::Io(ref e))
                    if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                {
                    continue;
                }
                Err(_) => break,
            }
        }

        // On clean client close (not revocation), flush any remaining buffered jitter frames.
        if clean_close
            && !peer_stop_c.load(Ordering::Acquire)
            && !manager_stop_c.load(Ordering::Acquire)
        {
            while jb.stats().buffered_frames > 0 {
                if let PopOutcome::Frame(frame) = jb.pop() {
                    if playback.write_frame(&frame).is_err() {
                        break;
                    }
                } else {
                    break;
                }
            }
        } else if peer_stop_c.load(Ordering::Acquire) {
            let _ = conn.send_close();
        }

        // Drop playback and connection before removing map entry.
        drop(playback);
        drop(conn);

        let mut guard = peers_c.lock().unwrap_or_else(|e| e.into_inner());
        if guard
            .get(&peer_id_c)
            .is_some_and(|entry| entry.session_id == session_id)
        {
            guard.remove(&peer_id_c);
        }
    });

    let mut guard = active_peers.lock().unwrap_or_else(|e| e.into_inner());
    guard.insert(
        peer_id,
        ActiveInboundPeer {
            session_id,
            stop_flag: peer_stop,
            worker: Some(worker),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_until(timeout: Duration, mut pred: impl FnMut() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if pred() {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        pred()
    }

    #[test]
    fn end_to_end_pcm_lossless_streaming_and_telemetry() {
        let k_pair = [0x42; 32];
        let trust = Arc::new(InMemoryTrustStore::new());
        trust.insert("sender-pcm", k_pair);

        let playback_handles: Arc<Mutex<Vec<MockPcmPlaybackHandle>>> =
            Arc::new(Mutex::new(Vec::new()));
        let handles_c = Arc::clone(&playback_handles);
        let factory: PcmPlaybackFactory = Arc::new(move |_peer_id| {
            let (play, handle) = MockPcmPlayback::pair();
            handles_c.lock().unwrap().push(handle);
            Ok(Box::new(play))
        });

        let manager = Sht3SessionManager::new();
        let addr = manager
            .start_listener(
                "127.0.0.1:0".parse().unwrap(),
                Sht3ServerConfig::new("receiver-pcm"),
                trust,
                factory,
            )
            .unwrap();

        let expected_frames: Vec<Vec<i16>> = (0..5)
            .map(|f| {
                (0..PCM_FRAME_SAMPLES_20MS)
                    .map(|i| ((f * 100 + i) as i16).wrapping_mul(7))
                    .collect()
            })
            .collect();
        let capture = Box::new(MockPcmCapture::from_frames(expected_frames.clone()));

        let out_cfg = OutboundConfig::new("sender-pcm", addr, k_pair)
            .with_profile(AudioProfile::PcmLossless)
            .with_jitter_ms(40);
        let session = manager.start_outbound(out_cfg, capture).unwrap();
        assert_eq!(session.server_device_id(), "receiver-pcm");

        // Wait for finite 5-frame capture to finish and close cleanly.
        session.join();

        let telemetry = session.telemetry();
        assert_eq!(telemetry.frames_sent, 5);
        assert_eq!(
            telemetry.bytes_sent,
            5 * (PCM_FRAME_SAMPLES_20MS as u64 * 2)
        );
        assert!(telemetry.pongs_received >= 1);
        assert!(!telemetry.active);

        let handle = playback_handles.lock().unwrap().first().cloned().unwrap();
        assert!(wait_until(Duration::from_secs(2), || handle.is_closed()));
        assert_eq!(handle.frames(), expected_frames);
    }

    #[cfg(feature = "opus")]
    #[test]
    fn end_to_end_opus_balanced_streaming_and_telemetry() {
        let k_pair = [0x77; 32];
        let trust = Arc::new(InMemoryTrustStore::new());
        trust.insert("sender-opus", k_pair);

        let playback_handles: Arc<Mutex<Vec<MockPcmPlaybackHandle>>> =
            Arc::new(Mutex::new(Vec::new()));
        let handles_c = Arc::clone(&playback_handles);
        let factory: PcmPlaybackFactory = Arc::new(move |_peer_id| {
            let (play, handle) = MockPcmPlayback::pair();
            handles_c.lock().unwrap().push(handle);
            Ok(Box::new(play))
        });

        let manager = TransportManager::new();
        let addr = manager
            .start_listener(
                "127.0.0.1:0".parse().unwrap(),
                Sht3ServerConfig::new("receiver-opus"),
                trust,
                factory,
            )
            .unwrap();

        let capture = Box::new(MockPcmCapture::sine_wave(440.0, Some(8)));
        let out_cfg = OutboundConfig::new("sender-opus", addr, k_pair)
            .with_profile(AudioProfile::OpusBalanced)
            .with_jitter_ms(40);
        let session = manager.start_outbound(out_cfg, capture).unwrap();
        session.join();

        let telemetry = session.telemetry();
        assert_eq!(telemetry.frames_sent, 8);
        // Opus compression must send far fewer bytes than raw PCM (8 * 3840 = 30720 B).
        assert!(telemetry.bytes_sent > 0 && telemetry.bytes_sent < 8 * 3840);
        assert!(telemetry.pongs_received >= 1);

        let handle = playback_handles.lock().unwrap().first().cloned().unwrap();
        assert!(wait_until(Duration::from_secs(2), || handle.is_closed()));
        assert_eq!(handle.frame_count(), 8);
        assert!(
            handle.total_energy() > 200_000,
            "decoded Opus stream was unexpectedly silent"
        );
    }

    #[cfg(feature = "opus")]
    #[test]
    fn end_to_end_opus_profiles_fidelity_snr() {
        for profile in [
            AudioProfile::OpusBalanced,
            AudioProfile::OpusLowLatency,
            AudioProfile::OpusLowBandwidth,
        ] {
            let k_pair = [0x88; 32];
            let trust = Arc::new(InMemoryTrustStore::new());
            trust.insert("sender-fidelity", k_pair);

            let playback_handles: Arc<Mutex<Vec<MockPcmPlaybackHandle>>> =
                Arc::new(Mutex::new(Vec::new()));
            let handles_c = Arc::clone(&playback_handles);
            let factory: PcmPlaybackFactory = Arc::new(move |_peer_id| {
                let (play, handle) = MockPcmPlayback::pair();
                handles_c.lock().unwrap().push(handle);
                Ok(Box::new(play))
            });

            let manager = Sht3SessionManager::new();
            let addr = manager
                .start_listener(
                    "127.0.0.1:0".parse().unwrap(),
                    Sht3ServerConfig::new("receiver-fidelity"),
                    trust,
                    factory,
                )
                .unwrap();

            let spc = profile.frame_samples_per_channel();
            let total_frames = 40usize;
            let mut expected_frames = Vec::with_capacity(total_frames);
            let mut in_l = Vec::with_capacity(total_frames * spc);
            for f_idx in 0..total_frames {
                let mut pcm = Vec::with_capacity(spc * 2);
                for i in 0..spc {
                    let t = (f_idx * spc + i) as f64 / 48_000.0;
                    let env = 0.25 + 0.75 * ((t * 3.0 * std::f64::consts::PI).sin().abs());
                    let l = (env
                        * (8000.0 * (2.0 * std::f64::consts::PI * 110.0 * t).sin()
                            + 6000.0 * (2.0 * std::f64::consts::PI * 660.0 * t).sin()
                            + 3000.0 * (2.0 * std::f64::consts::PI * 3520.0 * t).cos()))
                    .clamp(-32000.0, 32000.0) as i16;
                    let r = (env
                        * (8000.0 * (2.0 * std::f64::consts::PI * 110.0 * t).sin()
                            + 5500.0 * (2.0 * std::f64::consts::PI * 880.0 * t).cos()
                            + 3000.0 * (2.0 * std::f64::consts::PI * 2640.0 * t).sin()))
                    .clamp(-32000.0, 32000.0) as i16;
                    in_l.push(l);
                    pcm.push(l);
                    pcm.push(r);
                }
                expected_frames.push(pcm);
            }

            let capture = Box::new(MockPcmCapture::from_frames(expected_frames));
            let out_cfg = OutboundConfig::new("sender-fidelity", addr, k_pair)
                .with_profile(profile)
                .with_jitter_ms(40);
            let session = manager.start_outbound(out_cfg, capture).unwrap();
            session.join();

            let handle = playback_handles.lock().unwrap().first().cloned().unwrap();
            assert!(wait_until(Duration::from_secs(2), || handle.is_closed()));
            assert_eq!(handle.frame_count(), total_frames);

            let out_l: Vec<i16> = handle
                .frames()
                .into_iter()
                .flat_map(|f| f.into_iter().step_by(2))
                .collect();
            let search_start = spc * 5;
            let search_end = in_l.len() - spc * 2;
            let mut best_lag = 0usize;
            let mut best_corr = -1.0f64;
            for lag in 0..300usize {
                let mut dot = 0.0f64;
                let mut ei = 0.0f64;
                let mut eo = 0.0f64;
                for idx in search_start..search_end {
                    let a = f64::from(in_l[idx]);
                    let b = f64::from(out_l[idx + lag]);
                    dot += a * b;
                    ei += a * a;
                    eo += b * b;
                }
                let corr = dot / (ei.sqrt() * eo.sqrt() + 1e-9);
                if corr > best_corr {
                    best_corr = corr;
                    best_lag = lag;
                }
            }
            let mut sig_pow = 0.0f64;
            let mut err_pow = 0.0f64;
            for idx in search_start..search_end {
                let a = f64::from(in_l[idx]);
                let b = f64::from(out_l[idx + best_lag]);
                sig_pow += a * a;
                err_pow += (a - b) * (a - b);
            }
            let snr_db = 10.0 * (sig_pow / (err_pow + 1e-9)).log10();
            assert!(
                best_corr > 0.99 && snr_db > 22.0,
                "end-to-end SHT3 {profile:?} fidelity too low: corr={best_corr:.4}, snr={snr_db:.2} dB"
            );
        }
    }

    #[test]
    fn live_volume_and_mute_control_propagation() {
        let k_pair = [0x91; 32];
        let trust = Arc::new(InMemoryTrustStore::new());
        trust.insert("ctrl-sender", k_pair);

        let playback_handle: Arc<Mutex<Option<MockPcmPlaybackHandle>>> = Arc::new(Mutex::new(None));
        let handle_c = Arc::clone(&playback_handle);
        let factory: PcmPlaybackFactory = Arc::new(move |_peer_id| {
            let (play, handle) = MockPcmPlayback::pair();
            *handle_c.lock().unwrap() = Some(handle);
            Ok(Box::new(play))
        });

        let manager = Sht3SessionManager::new();
        let addr = manager
            .start_listener(
                "127.0.0.1:0".parse().unwrap(),
                Sht3ServerConfig::new("ctrl-receiver"),
                trust,
                factory,
            )
            .unwrap();

        let stop_cap = Arc::new(AtomicBool::new(false));
        let capture = Box::new(
            MockPcmCapture::sine_wave(440.0, None)
                .with_pace(Duration::from_millis(5))
                .with_stop_flag(Arc::clone(&stop_cap)),
        );
        let out_cfg = OutboundConfig::new("ctrl-sender", addr, k_pair)
            .with_profile(AudioProfile::PcmLossless);
        let session = manager.start_outbound(out_cfg, capture).unwrap();

        assert!(wait_until(Duration::from_secs(2), || {
            playback_handle
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|h| h.frame_count() >= 2)
        }));

        session.set_volume(42).unwrap();
        session.set_mute(true).unwrap();
        session.set_mute(false).unwrap();

        let rx_handle = playback_handle.lock().unwrap().clone().unwrap();
        assert!(wait_until(Duration::from_secs(2), || {
            rx_handle.volume() == 42 && !rx_handle.is_muted() && rx_handle.mute_history().len() >= 3
        }));

        assert!(rx_handle.volume_history().contains(&42));
        assert!(rx_handle.mute_history().ends_with(&[true, false]));

        assert!(wait_until(Duration::from_secs(2), || {
            let t = session.telemetry();
            t.remote_volume == 42 && !t.remote_muted
        }));

        stop_cap.store(true, Ordering::Release);
        session.stop();
    }

    #[test]
    fn instant_disconnect_peer_revocation_drops_stream_and_blocks_reconnect() {
        let k_pair = [0xAB; 32];
        let trust = Arc::new(InMemoryTrustStore::new());
        trust.insert("revoked-peer", k_pair);

        let playback_handle: Arc<Mutex<Option<MockPcmPlaybackHandle>>> = Arc::new(Mutex::new(None));
        let handle_c = Arc::clone(&playback_handle);
        let factory: PcmPlaybackFactory = Arc::new(move |_peer_id| {
            let (play, handle) = MockPcmPlayback::pair();
            *handle_c.lock().unwrap() = Some(handle);
            Ok(Box::new(play))
        });

        let manager = Sht3SessionManager::new();
        let addr = manager
            .start_listener(
                "127.0.0.1:0".parse().unwrap(),
                Sht3ServerConfig::new("strict-receiver"),
                Arc::clone(&trust) as Arc<dyn TrustLookup>,
                factory,
            )
            .unwrap();

        let stop_cap = Arc::new(AtomicBool::new(false));
        let capture = Box::new(
            MockPcmCapture::sine_wave(440.0, None)
                .with_pace(Duration::from_millis(5))
                .with_stop_flag(Arc::clone(&stop_cap)),
        );
        let out_cfg = OutboundConfig::new("revoked-peer", addr, k_pair)
            .with_profile(AudioProfile::PcmLossless);
        let session = manager.start_outbound(out_cfg, capture).unwrap();

        assert!(wait_until(Duration::from_secs(2), || {
            manager.inbound_peer_count() == 1
                && playback_handle
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|h| h.frame_count() >= 2)
        }));
        assert_eq!(
            manager.active_inbound_peers(),
            vec!["revoked-peer".to_string()]
        );

        let rx_handle = playback_handle.lock().unwrap().clone().unwrap();
        assert!(!rx_handle.is_closed());

        // Revoke peer in trust store and immediately drop its active stream.
        trust.remove("revoked-peer");
        let drop_start = Instant::now();
        assert!(manager.disconnect_peer("revoked-peer"));
        assert!(
            drop_start.elapsed() < Duration::from_secs(1),
            "disconnect_peer took too long: {:?}",
            drop_start.elapsed()
        );
        assert!(rx_handle.is_closed());
        assert_eq!(manager.inbound_peer_count(), 0);
        assert!(!manager.disconnect_peer("revoked-peer"));

        // Outbound session must observe the disconnect and terminate promptly.
        assert!(wait_until(Duration::from_secs(2), || !session.is_running()));

        // Any reconnection attempt from the revoked peer must be rejected.
        let retry_cfg = OutboundConfig::new("revoked-peer", addr, k_pair)
            .with_profile(AudioProfile::PcmLossless);
        let retry_err = manager
            .start_outbound(
                retry_cfg,
                Box::new(MockPcmCapture::sine_wave(440.0, Some(1))),
            )
            .err()
            .expect("revoked peer reconnect must fail");
        assert!(matches!(retry_err, TransportError::AuthRejected(_)));

        stop_cap.store(true, Ordering::Release);
    }
}
