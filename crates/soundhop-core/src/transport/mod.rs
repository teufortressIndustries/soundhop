//! SoundHop Transport v4 (`gst-srtp`): GStreamer SRTP/Opus & L16 PCM media engine
//! paired with an encrypted `SHT4` TCP control channel.
//!
//! Architecture:
//! - Mutual-authenticated, forward-secret handshake (`X25519` + per-device `K_pair` HMAC-SHA256 + HKDF-SHA256)
//! - `ChaCha20-Poly1305` AEAD TCP control channel (`Control`, `Ping`, `Pong`, `Close`)
//! - GStreamer SRTP (`AES-128-ICM` + `HMAC-SHA1-80`) UDP media pipelines (`Opus` with in-band FEC/PLC or lossless `L16` PCM)
//! - Session orchestration (`Sht3SessionManager` / `TransportManager`) with live peer revocation and dead-peer watchdog

pub mod client;
pub mod crypto;
#[cfg(feature = "gstreamer")]
pub mod gst_receiver;
#[cfg(feature = "gstreamer")]
pub mod gst_runtime;
#[cfg(feature = "gstreamer")]
pub mod gst_sender;
#[cfg(all(test, feature = "gstreamer"))]
mod gst_tests;
pub mod server;
pub mod wire;

use std::collections::HashMap;
#[cfg(feature = "gstreamer")]
use std::net::TcpStream;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
#[cfg(feature = "gstreamer")]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
#[cfg(feature = "gstreamer")]
use std::time::Instant;

use thiserror::Error;

pub use crate::pairing::{InMemoryTrustStore, TrustLookup};
pub use client::{Sht3ClientConfig, Sht3ClientConnection, Sht3ClientReader, Sht3ClientWriter};
pub use crypto::{AeadSession, EphemeralKeypair, Role, SessionKeys};
#[cfg(feature = "gstreamer")]
pub use gst_receiver::{
    GstAudioReceiver, PlaybackEndpoint, PlaybackEndpointFactory, PreparedGstReceiver,
    ReceiverJitterStats,
};
#[cfg(feature = "gstreamer")]
pub use gst_runtime::{
    check_gstreamer_runtime, configure_gst_env_if_bundled, ensure_gst_init, GstreamerRuntimeStatus,
};
#[cfg(feature = "gstreamer")]
pub use gst_sender::{CaptureEndpoint, GstAudioSender, NetsimConfig, NetsimController};
pub use server::{Sht3Listener, Sht3ServerConfig, Sht3ServerConnection};
pub use wire::{
    is_valid_ssrc, AudioParams, AudioProfile, ClientHello, CodecId, ControlMessage, DecryptedFrame,
    EncryptedFrame, FrameHeader, FrameType, HandshakeStatus, PingPayload, PongPayload, ServerHello,
    FLAG_DISCONTINUITY, SRTP_SUITE_AES128_ICM_HMAC_SHA1_80,
};

/// 4-byte protocol magic retained from SHT3 so legacy v3 peers reject cleanly with `UnsupportedVersion`.
pub const SHT3_MAGIC: [u8; 4] = *b"SHT3";
/// Wire protocol version for the GStreamer SRTP/Opus control channel.
pub const SHT4_VERSION: u8 = 4;
/// Default dedicated TCP port for the encrypted control channel (leaving 4713/tcp for legacy PA-TCP).
pub const DEFAULT_V3_PORT: u16 = 4714;
/// Maximum encrypted frame payload length in bytes (pre-allocation cap).
pub const MAX_PAYLOAD_LEN: usize = 8192;
/// Fixed plaintext frame header length in bytes (used as AEAD AAD).
pub const FRAME_HEADER_LEN: usize = 16;
/// Poly1305 authentication tag length in bytes.
pub const AEAD_TAG_LEN: usize = 16;
/// Maximum number of control frames per directional key before rekey is required.
pub const MAX_FRAMES_PER_KEY: u64 = 1 << 24;

#[cfg(feature = "gstreamer")]
const MAX_MANAGED_INBOUND_IO_TIMEOUT: Duration = Duration::from_millis(100);
const DEFAULT_OUTBOUND_IO_TIMEOUT: Duration = Duration::from_millis(200);
#[cfg(feature = "gstreamer")]
const MAX_OUTBOUND_READER_TIMEOUT: Duration = Duration::from_millis(50);
const DEFAULT_PING_INTERVAL: Duration = Duration::from_millis(500);
#[cfg(feature = "gstreamer")]
const MAX_INFLIGHT_HANDSHAKES: usize = 8;
#[cfg(feature = "gstreamer")]
const PROACTIVE_REKEY_FRAMES_REMAINING: u64 = 100;
/// Tear down orphaned inbound receiver pipelines if the peer sends no Ping/Control for 5 seconds.
#[cfg(feature = "gstreamer")]
const DEAD_PEER_IDLE_TIMEOUT: Duration = Duration::from_secs(5);

/// Result alias for transport operations.
pub type TransportResult<T> = Result<T, TransportError>;

/// Errors returned by the SoundHop transport layer.
#[derive(Debug, Error)]
pub enum TransportError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("crypto error: {0}")]
    Crypto(String),

    #[error("wire protocol error: {0}")]
    Wire(String),

    #[error("pipeline error: {0}")]
    Pipeline(String),

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

// ---------------------------------------------------------------------------
// Stubs when compiled without the `gstreamer` feature (`--no-default-features`)
// ---------------------------------------------------------------------------

#[cfg(not(feature = "gstreamer"))]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct GstreamerRuntimeStatus {
    pub available: bool,
    pub version: String,
    pub missing_elements: Vec<String>,
    pub audio_src_available: bool,
    pub audio_sink_available: bool,
    pub bundled_plugin_dir: Option<String>,
}

#[cfg(not(feature = "gstreamer"))]
pub fn check_gstreamer_runtime() -> Result<GstreamerRuntimeStatus, TransportError> {
    Err(TransportError::Pipeline(
        "GStreamer transport support is disabled in this build".into(),
    ))
}

#[cfg(not(feature = "gstreamer"))]
#[derive(Debug, Clone)]
pub enum CaptureEndpoint {
    SystemPulseMonitor { device: String },
    SystemWasapiLoopback,
    TestSineWave { freq_hz: f64 },
}

#[cfg(not(feature = "gstreamer"))]
#[derive(Clone)]
pub enum PlaybackEndpoint {
    SystemPulse,
    SystemWasapi,
    TestAppSink(Arc<Mutex<Vec<i16>>>),
}

#[cfg(not(feature = "gstreamer"))]
pub type PlaybackEndpointFactory =
    Arc<dyn Fn(&str) -> TransportResult<PlaybackEndpoint> + Send + Sync>;

// ---------------------------------------------------------------------------
// Transport Engine Traits & Telemetry
// ---------------------------------------------------------------------------

/// Narrow abstraction over a running outbound audio stream handle.
pub trait TransportOutbound: Send + Sync {
    /// Returns `true` while the outbound streaming worker is still running.
    fn is_running(&self) -> bool;

    /// Authenticated remote server `device_id`.
    fn server_device_id(&self) -> &str;

    /// Snapshot live RTT, jitter buffer, drop, and volume telemetry.
    fn telemetry(&self) -> StreamTelemetry;

    /// Canonical profile name for the negotiated audio stream.
    fn profile_name(&self) -> &'static str;

    /// Forward a volume change (`0..=150`) to the remote receiver; returns `true` if queued.
    fn set_volume(&self, pct: u8) -> bool;

    /// Forward a mute state change to the remote receiver; returns `true` if queued.
    fn set_mute(&self, muted: bool) -> bool;

    /// Trigger an immediate `Ping`/`Pong` telemetry probe; returns `true` if queued.
    fn request_ping(&self) -> bool;

    /// Request graceful shutdown and join the worker thread.
    fn stop(&self);
}

/// Narrow abstraction over an inbound audio transport listener/session engine.
pub trait AudioTransportEngine: Send + Sync {
    /// Stable identifier for this transport engine (`"gst-srtp"`).
    fn engine_id(&self) -> &'static str;

    /// Bind and start the inbound transport listener on `bind_addr`.
    fn start_listener(
        &self,
        bind_addr: SocketAddr,
        server_device_id: String,
        rtp_port: u16,
        trust: Arc<dyn TrustLookup>,
        playback_factory: PlaybackEndpointFactory,
    ) -> TransportResult<SocketAddr>;

    /// Stop the inbound listener and disconnect all active inbound peer sessions.
    fn stop_listener(&self);

    /// Address the inbound listener is currently bound to, if running.
    fn listener_addr(&self) -> Option<SocketAddr>;

    /// List `device_id`s of all currently connected inbound peers.
    fn active_inbound_peers(&self) -> Vec<String>;

    /// Immediately terminate the active inbound stream for `device_id`.
    fn disconnect_peer(&self, device_id: &str) -> bool;
}

/// Live telemetry snapshot reported by an outbound SRTP/Opus audio stream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StreamTelemetry {
    /// Most recent round-trip time measured via `Ping`/`Pong` in microseconds.
    pub rtt_us: u64,
    /// Receiver jitter buffer depth reported in the latest `Pong`, in milliseconds.
    pub jitter_buf_ms: u16,
    /// Cumulative receiver dropped frames reported in the latest `Pong`.
    pub dropped_frames: u32,
    /// Total encrypted SRTP audio packets sent by this session.
    pub frames_sent: u64,
    /// Total encrypted SRTP audio bytes sent by this session.
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

/// Configuration for an outbound SRTP/Opus audio streaming session.
#[derive(Debug, Clone)]
pub struct OutboundConfig {
    pub client_device_id: String,
    pub target_addr: SocketAddr,
    pub k_pair: [u8; 32],
    pub profile: AudioProfile,
    pub jitter_ms: u16,
    pub ping_interval: Duration,
    pub io_timeout: Option<Duration>,
    pub reconnect_window: Duration,
}

impl OutboundConfig {
    /// Create an [`OutboundConfig`] targeting `target_addr` with the default `OpusBalanced` profile.
    pub fn new(
        client_device_id: impl Into<String>,
        target_addr: SocketAddr,
        k_pair: [u8; 32],
    ) -> Self {
        Self {
            client_device_id: client_device_id.into(),
            target_addr,
            k_pair,
            profile: AudioProfile::OpusBalanced,
            jitter_ms: 40,
            ping_interval: DEFAULT_PING_INTERVAL,
            io_timeout: Some(DEFAULT_OUTBOUND_IO_TIMEOUT),
            reconnect_window: Duration::ZERO,
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

    pub fn with_reconnect_window(mut self, window: Duration) -> Self {
        self.reconnect_window = window;
        self
    }
}

#[cfg_attr(not(feature = "gstreamer"), allow(dead_code))]
enum OutboundCommand {
    Control(ControlMessage),
    Ping,
    Stop,
}

/// Handle to a running outbound SRTP/Opus audio stream.
///
/// Dropping the handle stops the GStreamer pipeline, closes the TCP control channel,
/// and joins the worker thread cleanly.
pub struct OutboundSessionHandle {
    server_device_id: String,
    audio_params: AudioParams,
    current_ssrc: Arc<AtomicU32>,
    current_rtp_port: Arc<AtomicU16>,
    stop_flag: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    cmd_tx: Mutex<mpsc::Sender<OutboundCommand>>,
    telemetry: Arc<Mutex<StreamTelemetry>>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

#[cfg(feature = "gstreamer")]
struct OutboundReaderHandle {
    conn_stop: Arc<AtomicBool>,
    conn_broken: Arc<AtomicBool>,
    fatal_error: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

#[cfg(feature = "gstreamer")]
impl OutboundReaderHandle {
    fn stop_and_join(&mut self) {
        self.conn_stop.store(true, Ordering::Release);
        if let Some(handle) = self.join.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(feature = "gstreamer")]
struct OutboundReaderShared {
    session_stop: Arc<AtomicBool>,
    remote_closed: Arc<AtomicBool>,
    telemetry: Arc<Mutex<StreamTelemetry>>,
    start_instant: Instant,
    read_timeout: Duration,
}

#[cfg(feature = "gstreamer")]
fn spawn_outbound_reader(
    mut reader: Sht3ClientReader,
    shared: &OutboundReaderShared,
) -> OutboundReaderHandle {
    use std::io::ErrorKind;

    let conn_stop = Arc::new(AtomicBool::new(false));
    let conn_broken = Arc::new(AtomicBool::new(false));
    let fatal_error = Arc::new(AtomicBool::new(false));

    let _ = reader.set_read_timeout(Some(shared.read_timeout));

    let session_stop = Arc::clone(&shared.session_stop);
    let remote_closed = Arc::clone(&shared.remote_closed);
    let telemetry = Arc::clone(&shared.telemetry);
    let start_instant = shared.start_instant;
    let conn_stop_c = Arc::clone(&conn_stop);
    let conn_broken_c = Arc::clone(&conn_broken);
    let fatal_error_c = Arc::clone(&fatal_error);

    let join = std::thread::spawn(move || loop {
        match reader.recv_frame() {
            Ok(frame) => {
                match frame.header.frame_type {
                    FrameType::Pong => {
                        if let Ok(pong) = PongPayload::decode(&frame.payload) {
                            let now_us = start_instant
                                .elapsed()
                                .as_micros()
                                .min(u128::from(u64::MAX))
                                as u64;
                            let rtt_us = now_us.saturating_sub(pong.client_time_us);
                            let mut t = telemetry.lock().unwrap_or_else(|e| e.into_inner());
                            t.rtt_us = rtt_us;
                            t.jitter_buf_ms = pong.jitter_buf_ms;
                            t.dropped_frames = pong.dropped_frames;
                            t.pongs_received += 1;
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
                    }
                    FrameType::Close => {
                        remote_closed.store(true, Ordering::Release);
                        break;
                    }
                    FrameType::Ping => {}
                }
                if session_stop.load(Ordering::Acquire) {
                    break;
                }
            }
            Err(TransportError::Io(ref e))
                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
            {
                if session_stop.load(Ordering::Acquire) || conn_stop_c.load(Ordering::Acquire) {
                    break;
                }
            }
            Err(TransportError::Wire(_))
            | Err(TransportError::Crypto(_))
            | Err(TransportError::PayloadTooLarge { .. }) => {
                fatal_error_c.store(true, Ordering::Release);
                break;
            }
            Err(_) => {
                conn_broken_c.store(true, Ordering::Release);
                break;
            }
        }
    });

    OutboundReaderHandle {
        conn_stop,
        conn_broken,
        fatal_error,
        join: Some(join),
    }
}

#[cfg(feature = "gstreamer")]
#[allow(clippy::too_many_arguments)]
fn try_reconnect_outbound(
    reason_rekey: bool,
    config: &OutboundConfig,
    client_cfg: &Sht3ClientConfig,
    capture: &CaptureEndpoint,
    last_requested_volume: Option<u8>,
    last_requested_mute: Option<bool>,
    old_sender: &mut Option<GstAudioSender>,
    old_writer: Option<Sht3ClientWriter>,
    reader_handle: &mut OutboundReaderHandle,
    reader_shared: &OutboundReaderShared,
    current_ssrc: &AtomicU32,
    current_rtp_port: &AtomicU16,
) -> Option<(GstAudioSender, Sht3ClientWriter, OutboundReaderHandle)> {
    if let Some(sender) = old_sender.take() {
        sender.stop();
    }
    if reason_rekey {
        if let Some(mut w) = old_writer {
            let _ = w.send_close();
        }
    } else {
        drop(old_writer);
    }

    reader_handle.stop_and_join();

    if reader_shared.session_stop.load(Ordering::Acquire)
        || reader_shared.remote_closed.load(Ordering::Acquire)
        || reader_handle.fatal_error.load(Ordering::Acquire)
    {
        return None;
    }

    let window = if reason_rekey {
        config.reconnect_window.max(Duration::from_secs(3))
    } else {
        config.reconnect_window
    };
    if window.is_zero() {
        return None;
    }

    let deadline = Instant::now() + window;
    let mut backoff = if reason_rekey {
        Duration::ZERO
    } else {
        Duration::from_millis(100)
    };

    loop {
        if !backoff.is_zero() {
            let wait_until = (Instant::now() + backoff).min(deadline);
            while Instant::now() < wait_until {
                if reader_shared.session_stop.load(Ordering::Acquire) {
                    return None;
                }
                let rem = wait_until.saturating_duration_since(Instant::now());
                std::thread::sleep(rem.min(Duration::from_millis(25)));
            }
        }

        let now = Instant::now();
        if now >= deadline || reader_shared.session_stop.load(Ordering::Acquire) {
            return None;
        }

        let mut attempt_cfg = client_cfg.clone();
        let remaining = deadline.saturating_duration_since(now);
        attempt_cfg.connect_timeout = attempt_cfg
            .connect_timeout
            .min(remaining.max(Duration::from_millis(100)));

        match Sht3ClientConnection::connect(config.target_addr, attempt_cfg, &config.k_pair) {
            Ok(conn) => {
                let negotiated = *conn.audio_params();
                let new_ssrc = conn.sender_ssrc();
                let new_rtp_port = conn.rtp_port();
                let new_srtp_key = *conn.srtp_master_key_c2s();
                let Ok((mut new_writer, new_reader)) = conn.into_split() else {
                    return None;
                };
                let Ok(new_sender) = GstAudioSender::start(
                    capture.clone(),
                    negotiated,
                    config.target_addr.ip(),
                    new_rtp_port,
                    new_ssrc,
                    new_srtp_key,
                ) else {
                    return None;
                };

                let mut ctrl_io_err = false;
                if let Some(vol) = last_requested_volume {
                    match new_writer.send_control(&ControlMessage::SetVolume(vol)) {
                        Ok(()) => {}
                        Err(TransportError::Io(_)) => ctrl_io_err = true,
                        Err(_) => {
                            new_sender.stop();
                            return None;
                        }
                    }
                }
                if !ctrl_io_err {
                    if let Some(muted) = last_requested_mute {
                        match new_writer.send_control(&ControlMessage::SetMute(muted)) {
                            Ok(()) => {}
                            Err(TransportError::Io(_)) => ctrl_io_err = true,
                            Err(_) => {
                                new_sender.stop();
                                return None;
                            }
                        }
                    }
                }
                if !ctrl_io_err {
                    current_ssrc.store(new_ssrc, Ordering::Release);
                    current_rtp_port.store(new_rtp_port, Ordering::Release);
                    let new_reader_handle = spawn_outbound_reader(new_reader, reader_shared);
                    return Some((new_sender, new_writer, new_reader_handle));
                }
                new_sender.stop();
            }
            Err(
                TransportError::AuthRejected(_)
                | TransportError::UnsupportedProfile(_)
                | TransportError::Wire(_)
                | TransportError::Crypto(_)
                | TransportError::PayloadTooLarge { .. },
            ) => {
                return None;
            }
            Err(_) => {}
        }

        backoff = if backoff.is_zero() {
            Duration::from_millis(100)
        } else {
            (backoff * 2).min(Duration::from_secs(1))
        };
    }
}

impl OutboundSessionHandle {
    /// Connect to the target SHT4 listener and start streaming encrypted SRTP audio from `capture`.
    #[cfg(feature = "gstreamer")]
    pub fn start(config: OutboundConfig, capture: CaptureEndpoint) -> TransportResult<Self> {
        let mut client_cfg = Sht3ClientConfig::new(
            config.client_device_id.clone(),
            config.profile.to_audio_params(config.jitter_ms),
        );
        client_cfg.io_timeout = config.io_timeout;

        let conn =
            Sht3ClientConnection::connect(config.target_addr, client_cfg.clone(), &config.k_pair)?;
        let audio_params = *conn.audio_params();
        let server_device_id = conn.server_device_id().to_string();
        let initial_volume = conn.initial_volume();
        let initial_muted = conn.initial_muted();
        let initial_ssrc = conn.sender_ssrc();
        let initial_rtp_port = conn.rtp_port();
        let initial_srtp_key = *conn.srtp_master_key_c2s();

        let initial_sender = GstAudioSender::start(
            capture.clone(),
            audio_params,
            config.target_addr.ip(),
            initial_rtp_port,
            initial_ssrc,
            initial_srtp_key,
        )?;

        let (initial_writer, initial_reader) = match conn.into_split() {
            Ok(pair) => pair,
            Err(err) => {
                initial_sender.stop();
                return Err(err);
            }
        };

        let stop_flag = Arc::new(AtomicBool::new(false));
        let running = Arc::new(AtomicBool::new(true));
        let remote_closed = Arc::new(AtomicBool::new(false));
        let current_ssrc = Arc::new(AtomicU32::new(initial_ssrc));
        let current_rtp_port = Arc::new(AtomicU16::new(initial_rtp_port));
        let telemetry = Arc::new(Mutex::new(StreamTelemetry {
            remote_volume: initial_volume,
            remote_muted: initial_muted,
            active: true,
            ..StreamTelemetry::default()
        }));
        let (cmd_tx, cmd_rx) = mpsc::channel::<OutboundCommand>();

        let start_instant = Instant::now();
        let reader_timeout = config
            .io_timeout
            .unwrap_or(DEFAULT_OUTBOUND_IO_TIMEOUT)
            .min(MAX_OUTBOUND_READER_TIMEOUT);
        let reader_shared = OutboundReaderShared {
            session_stop: Arc::clone(&stop_flag),
            remote_closed: Arc::clone(&remote_closed),
            telemetry: Arc::clone(&telemetry),
            start_instant,
            read_timeout: reader_timeout,
        };
        let mut reader_handle = spawn_outbound_reader(initial_reader, &reader_shared);

        let stop_c = Arc::clone(&stop_flag);
        let running_c = Arc::clone(&running);
        let telemetry_c = Arc::clone(&telemetry);
        let ssrc_c = Arc::clone(&current_ssrc);
        let rtp_port_c = Arc::clone(&current_rtp_port);
        let ping_interval = config.ping_interval;

        let worker = std::thread::spawn(move || {
            let mut sender_opt = Some(initial_sender);
            let mut writer_opt = Some(initial_writer);
            let mut last_ping_at = Instant::now();
            let mut last_requested_volume: Option<u8> = None;
            let mut last_requested_mute: Option<bool> = None;
            let mut prior_frames_sent: u64 = 0;
            let mut prior_bytes_sent: u64 = 0;

            'stream: while !stop_c.load(Ordering::Acquire)
                && !remote_closed.load(Ordering::Acquire)
                && !reader_handle.fatal_error.load(Ordering::Acquire)
            {
                // Update transmitted SRTP packet/byte counters from the active sender pipeline.
                if let Some(ref sender) = sender_opt {
                    let cur_frames = prior_frames_sent.saturating_add(sender.frames_sent());
                    let cur_bytes = prior_bytes_sent.saturating_add(sender.bytes_sent());
                    {
                        let mut t = telemetry_c.lock().unwrap_or_else(|e| e.into_inner());
                        t.frames_sent = cur_frames;
                        t.bytes_sent = cur_bytes;
                    }
                    if !sender.is_healthy() {
                        break 'stream;
                    }
                } else {
                    break 'stream;
                }

                let need_rekey = writer_opt
                    .as_ref()
                    .is_some_and(|w| w.frames_remaining() <= PROACTIVE_REKEY_FRAMES_REMAINING);
                if reader_handle.conn_broken.load(Ordering::Acquire) || need_rekey {
                    if let Some(ref s) = sender_opt {
                        prior_frames_sent = prior_frames_sent.saturating_add(s.frames_sent());
                        prior_bytes_sent = prior_bytes_sent.saturating_add(s.bytes_sent());
                    }
                    let Some((new_sender, new_writer, new_reader_handle)) = try_reconnect_outbound(
                        need_rekey,
                        &config,
                        &client_cfg,
                        &capture,
                        last_requested_volume,
                        last_requested_mute,
                        &mut sender_opt,
                        writer_opt.take(),
                        &mut reader_handle,
                        &reader_shared,
                        &ssrc_c,
                        &rtp_port_c,
                    ) else {
                        break 'stream;
                    };
                    sender_opt = Some(new_sender);
                    writer_opt = Some(new_writer);
                    reader_handle = new_reader_handle;
                    last_ping_at = Instant::now();
                    continue 'stream;
                }

                let wait_slice = ping_interval
                    .saturating_sub(last_ping_at.elapsed())
                    .min(Duration::from_millis(25));
                match cmd_rx.recv_timeout(wait_slice) {
                    Ok(OutboundCommand::Control(msg)) => {
                        match msg {
                            ControlMessage::SetVolume(v) => last_requested_volume = Some(v),
                            ControlMessage::SetMute(m) => last_requested_mute = Some(m),
                            _ => {}
                        }
                        let send_res = match writer_opt.as_mut() {
                            Some(w) => w.send_control(&msg),
                            None => break 'stream,
                        };
                        if let Err(err) = send_res {
                            if matches!(err, TransportError::Io(_)) {
                                if let Some(ref s) = sender_opt {
                                    prior_frames_sent =
                                        prior_frames_sent.saturating_add(s.frames_sent());
                                    prior_bytes_sent =
                                        prior_bytes_sent.saturating_add(s.bytes_sent());
                                }
                                let Some((new_sender, new_writer, new_reader_handle)) =
                                    try_reconnect_outbound(
                                        false,
                                        &config,
                                        &client_cfg,
                                        &capture,
                                        last_requested_volume,
                                        last_requested_mute,
                                        &mut sender_opt,
                                        writer_opt.take(),
                                        &mut reader_handle,
                                        &reader_shared,
                                        &ssrc_c,
                                        &rtp_port_c,
                                    )
                                else {
                                    break 'stream;
                                };
                                sender_opt = Some(new_sender);
                                writer_opt = Some(new_writer);
                                reader_handle = new_reader_handle;
                                if !matches!(
                                    msg,
                                    ControlMessage::SetVolume(_) | ControlMessage::SetMute(_)
                                ) {
                                    if let Some(w) = writer_opt.as_mut() {
                                        if w.send_control(&msg).is_err() {
                                            break 'stream;
                                        }
                                    }
                                }
                            } else {
                                break 'stream;
                            }
                        }
                    }
                    Ok(OutboundCommand::Ping) => {
                        let now_us = start_instant
                            .elapsed()
                            .as_micros()
                            .min(u128::from(u64::MAX)) as u64;
                        let send_res = match writer_opt.as_mut() {
                            Some(w) => w.send_ping(&PingPayload {
                                client_time_us: now_us,
                            }),
                            None => break 'stream,
                        };
                        match send_res {
                            Ok(()) => last_ping_at = Instant::now(),
                            Err(TransportError::Io(_)) => {
                                if let Some(ref s) = sender_opt {
                                    prior_frames_sent =
                                        prior_frames_sent.saturating_add(s.frames_sent());
                                    prior_bytes_sent =
                                        prior_bytes_sent.saturating_add(s.bytes_sent());
                                }
                                let Some((new_sender, new_writer, new_reader_handle)) =
                                    try_reconnect_outbound(
                                        false,
                                        &config,
                                        &client_cfg,
                                        &capture,
                                        last_requested_volume,
                                        last_requested_mute,
                                        &mut sender_opt,
                                        writer_opt.take(),
                                        &mut reader_handle,
                                        &reader_shared,
                                        &ssrc_c,
                                        &rtp_port_c,
                                    )
                                else {
                                    break 'stream;
                                };
                                sender_opt = Some(new_sender);
                                writer_opt = Some(new_writer);
                                reader_handle = new_reader_handle;
                                last_ping_at = Instant::now();
                            }
                            Err(_) => break 'stream,
                        }
                    }
                    Ok(OutboundCommand::Stop) => {
                        stop_c.store(true, Ordering::Release);
                        break 'stream;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break 'stream,
                }

                if last_ping_at.elapsed() >= ping_interval {
                    let now_us = start_instant
                        .elapsed()
                        .as_micros()
                        .min(u128::from(u64::MAX)) as u64;
                    if let Some(w) = writer_opt.as_mut() {
                        if let Err(TransportError::Io(_)) = w.send_ping(&PingPayload {
                            client_time_us: now_us,
                        }) {
                            if let Some(ref s) = sender_opt {
                                prior_frames_sent =
                                    prior_frames_sent.saturating_add(s.frames_sent());
                                prior_bytes_sent = prior_bytes_sent.saturating_add(s.bytes_sent());
                            }
                            let Some((new_sender, new_writer, new_reader_handle)) =
                                try_reconnect_outbound(
                                    false,
                                    &config,
                                    &client_cfg,
                                    &capture,
                                    last_requested_volume,
                                    last_requested_mute,
                                    &mut sender_opt,
                                    writer_opt.take(),
                                    &mut reader_handle,
                                    &reader_shared,
                                    &ssrc_c,
                                    &rtp_port_c,
                                )
                            else {
                                break 'stream;
                            };
                            sender_opt = Some(new_sender);
                            writer_opt = Some(new_writer);
                            reader_handle = new_reader_handle;
                        }
                    }
                    last_ping_at = Instant::now();
                }
            }

            if let Some(sender) = sender_opt.take() {
                let final_frames = prior_frames_sent.saturating_add(sender.frames_sent());
                let final_bytes = prior_bytes_sent.saturating_add(sender.bytes_sent());
                {
                    let mut t = telemetry_c.lock().unwrap_or_else(|e| e.into_inner());
                    t.frames_sent = final_frames;
                    t.bytes_sent = final_bytes;
                }
                sender.stop();
            }

            if stop_c.load(Ordering::Acquire) && !remote_closed.load(Ordering::Acquire) {
                if let Some(mut w) = writer_opt.take() {
                    let _ = w.send_close();
                }
            } else {
                drop(writer_opt.take());
            }

            reader_handle.stop_and_join();

            running_c.store(false, Ordering::Release);
            let mut t = telemetry_c.lock().unwrap_or_else(|e| e.into_inner());
            t.active = false;
        });

        Ok(Self {
            server_device_id,
            audio_params,
            current_ssrc,
            current_rtp_port,
            stop_flag,
            running,
            cmd_tx: Mutex::new(cmd_tx),
            telemetry,
            worker: Mutex::new(Some(worker)),
        })
    }

    #[cfg(not(feature = "gstreamer"))]
    pub fn start(_config: OutboundConfig, _capture: CaptureEndpoint) -> TransportResult<Self> {
        Err(TransportError::Pipeline(
            "GStreamer transport support is disabled in this build".into(),
        ))
    }

    /// Forward a `ControlMessage::SetVolume` to the remote receiver.
    pub fn set_volume(&self, pct: u8) -> TransportResult<()> {
        self.send_control(ControlMessage::SetVolume(pct.min(150)))
    }

    /// Forward a `ControlMessage::SetMute` to the remote receiver.
    pub fn set_mute(&self, muted: bool) -> TransportResult<()> {
        self.send_control(ControlMessage::SetMute(muted))
    }

    /// Enqueue an arbitrary [`ControlMessage`] to be sent over the encrypted control channel.
    pub fn send_control(&self, msg: ControlMessage) -> TransportResult<()> {
        self.cmd_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .send(OutboundCommand::Control(msg))
            .map_err(|_| TransportError::Closed)
    }

    /// Trigger an immediate `Ping`/`Pong` telemetry probe.
    pub fn request_ping(&self) -> TransportResult<()> {
        self.cmd_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
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

    /// Current active sender RTP SSRC (refreshed on reconnect).
    pub fn sender_ssrc(&self) -> u32 {
        self.current_ssrc.load(Ordering::Acquire)
    }

    /// Current remote receiver UDP RTP port (refreshed on reconnect).
    pub fn rtp_port(&self) -> u16 {
        self.current_rtp_port.load(Ordering::Acquire)
    }

    /// Request graceful shutdown and join the worker thread.
    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::Release);
        let _ = self
            .cmd_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .send(OutboundCommand::Stop);
        if let Some(handle) = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = handle.join();
        }
    }

    /// Wait until the worker finishes on its own.
    pub fn join(&self) {
        if let Some(handle) = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = handle.join();
        }
    }
}

impl TransportOutbound for OutboundSessionHandle {
    fn is_running(&self) -> bool {
        OutboundSessionHandle::is_running(self)
    }

    fn server_device_id(&self) -> &str {
        OutboundSessionHandle::server_device_id(self)
    }

    fn telemetry(&self) -> StreamTelemetry {
        OutboundSessionHandle::telemetry(self)
    }

    fn profile_name(&self) -> &'static str {
        AudioProfile::from_audio_params(&self.audio_params)
            .map(AudioProfile::as_str)
            .unwrap_or("opus-balanced")
    }

    fn set_volume(&self, pct: u8) -> bool {
        OutboundSessionHandle::set_volume(self, pct).is_ok()
    }

    fn set_mute(&self, muted: bool) -> bool {
        OutboundSessionHandle::set_mute(self, muted).is_ok()
    }

    fn request_ping(&self) -> bool {
        OutboundSessionHandle::request_ping(self).is_ok()
    }

    fn stop(&self) {
        OutboundSessionHandle::stop(self);
    }
}

impl Drop for OutboundSessionHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Diagnostic telemetry snapshot for an inbound SRTP/Opus peer session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InboundPeerDiagnostics {
    pub audio_frames_received: u64,
    pub jb_buffered_ms: u16,
    pub jb_lost: u64,
    pub jb_late: u64,
    pub jb_percent: u8,
    pub jitter_us: f64,
    pub jitter_ms: f64,
    pub rtp_port: u16,
    pub volume: u8,
    pub muted: bool,
    pub sender_ssrc: u32,
}

#[cfg_attr(not(feature = "gstreamer"), allow(dead_code))]
struct ActiveInboundPeer {
    session_id: u64,
    stop_flag: Arc<AtomicBool>,
    #[cfg(test)]
    abrupt_stop: Arc<AtomicBool>,
    diagnostics: Arc<Mutex<InboundPeerDiagnostics>>,
    worker: Option<JoinHandle<()>>,
}

#[cfg_attr(not(feature = "gstreamer"), allow(dead_code))]
struct RunningListener {
    local_addr: SocketAddr,
    stop_flag: Arc<AtomicBool>,
    accept_thread: Option<JoinHandle<()>>,
}

/// Manages inbound SRTP/Opus listener sessions (with per-`device_id` instant revocation)
/// and outbound GStreamer SRTP audio streams.
pub struct Sht3SessionManager {
    listener: Mutex<Option<RunningListener>>,
    active_peers: Arc<Mutex<HashMap<String, ActiveInboundPeer>>>,
    last_peer_diagnostics: Arc<Mutex<HashMap<String, InboundPeerDiagnostics>>>,
    #[cfg_attr(not(feature = "gstreamer"), allow(dead_code))]
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
            last_peer_diagnostics: Arc::new(Mutex::new(HashMap::new())),
            next_session_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Bind and start the SHT4 control listener on `addr` with GStreamer SRTP media receivers.
    #[cfg(feature = "gstreamer")]
    pub fn start_listener(
        &self,
        addr: SocketAddr,
        mut config: Sht3ServerConfig,
        trust: Arc<dyn TrustLookup>,
        playback_factory: PlaybackEndpointFactory,
    ) -> TransportResult<SocketAddr> {
        ensure_gst_init()?;
        self.stop_listener();

        config.io_timeout = Some(
            config
                .io_timeout
                .map_or(MAX_MANAGED_INBOUND_IO_TIMEOUT, |d| {
                    d.min(MAX_MANAGED_INBOUND_IO_TIMEOUT)
                }),
        );
        let handshake_config = Arc::new(config.clone());
        let handshake_trust = Arc::clone(&trust);

        let listener = Sht3Listener::bind(addr, config, trust)?;
        let local_addr = listener.local_addr()?;

        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_c = Arc::clone(&stop_flag);
        let peers_c = Arc::clone(&self.active_peers);
        let last_diag_c = Arc::clone(&self.last_peer_diagnostics);
        let next_id_c = Arc::clone(&self.next_session_id);
        let inflight_handshakes = Arc::new(AtomicUsize::new(0));

        let accept_thread = std::thread::spawn(move || {
            while !stop_c.load(Ordering::Acquire) {
                match listener.accept_raw() {
                    Ok((stream, _peer_addr)) => {
                        if stop_c.load(Ordering::Acquire) {
                            drop(stream);
                            break;
                        }
                        if inflight_handshakes
                            .try_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
                                (cur < MAX_INFLIGHT_HANDSHAKES).then_some(cur + 1)
                            })
                            .is_err()
                        {
                            drop(stream);
                            continue;
                        }

                        let hs_counter = Arc::clone(&inflight_handshakes);
                        let hs_cfg = Arc::clone(&handshake_config);
                        let hs_trust = Arc::clone(&handshake_trust);
                        let hs_stop = Arc::clone(&stop_c);
                        let hs_peers = Arc::clone(&peers_c);
                        let hs_last_diag = Arc::clone(&last_diag_c);
                        let hs_next_id = Arc::clone(&next_id_c);
                        let hs_factory = Arc::clone(&playback_factory);

                        std::thread::spawn(move || {
                            struct HandshakeSlotGuard(Arc<AtomicUsize>);
                            impl Drop for HandshakeSlotGuard {
                                fn drop(&mut self) {
                                    self.0.fetch_sub(1, Ordering::Release);
                                }
                            }
                            let _slot_guard = HandshakeSlotGuard(hs_counter);

                            let Ok((conn, receiver)) = Sht3Listener::handshake_with_media(
                                stream,
                                &hs_cfg,
                                hs_trust.as_ref(),
                                |peer_id, audio_params| {
                                    let endpoint = hs_factory(peer_id)?;
                                    let prepared = PreparedGstReceiver::bind(
                                        hs_cfg.rtp_port,
                                        audio_params,
                                        endpoint,
                                        hs_cfg.initial_volume,
                                        hs_cfg.initial_muted,
                                    )?;
                                    let bound_port = prepared.rtp_port();
                                    Ok((bound_port, prepared))
                                },
                                |prepared, sender_ssrc, keys| {
                                    prepared.start(sender_ssrc, keys.srtp_master_key_c2s)
                                },
                            ) else {
                                return;
                            };

                            let _ = conn.set_read_timeout(Some(MAX_MANAGED_INBOUND_IO_TIMEOUT));

                            if hs_stop.load(Ordering::Acquire) {
                                receiver.stop();
                                return;
                            }

                            spawn_inbound_peer(
                                conn,
                                receiver,
                                &hs_peers,
                                &hs_last_diag,
                                &hs_next_id,
                                &hs_stop,
                            );
                        });
                    }
                    Err(_) => {
                        if stop_c.load(Ordering::Acquire) {
                            break;
                        }
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

    #[cfg(not(feature = "gstreamer"))]
    pub fn start_listener(
        &self,
        _addr: SocketAddr,
        _config: Sht3ServerConfig,
        _trust: Arc<dyn TrustLookup>,
        _playback_factory: PlaybackEndpointFactory,
    ) -> TransportResult<SocketAddr> {
        Err(TransportError::Pipeline(
            "GStreamer transport support is disabled in this build".into(),
        ))
    }

    /// Address the control listener is currently bound to, if running.
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

    /// Snapshot of inbound diagnostics for `device_id` (active or most recently closed).
    pub fn inbound_peer_diagnostics(&self, device_id: &str) -> Option<InboundPeerDiagnostics> {
        let active_diag = {
            let guard = self.active_peers.lock().unwrap_or_else(|e| e.into_inner());
            guard.get(device_id).map(|p| Arc::clone(&p.diagnostics))
        };
        if let Some(diag) = active_diag {
            return Some(diag.lock().unwrap_or_else(|e| e.into_inner()).clone());
        }
        self.last_peer_diagnostics
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(device_id)
            .cloned()
    }

    /// Start an outbound SRTP/Opus audio streaming session from `capture` to `config.target_addr`.
    pub fn start_outbound(
        &self,
        config: OutboundConfig,
        capture: CaptureEndpoint,
    ) -> TransportResult<OutboundSessionHandle> {
        OutboundSessionHandle::start(config, capture)
    }

    /// Stop the inbound listener and immediately disconnect all active inbound peer sessions.
    pub fn stop_listener(&self) {
        let running = {
            let mut guard = self.listener.lock().unwrap_or_else(|e| e.into_inner());
            guard.take()
        };
        if let Some(mut l) = running {
            l.stop_flag.store(true, Ordering::Release);
            let wake_addr = wake_socket_addr(l.local_addr);
            #[cfg(feature = "gstreamer")]
            let _ = TcpStream::connect_timeout(&wake_addr, Duration::from_millis(100));
            #[cfg(not(feature = "gstreamer"))]
            let _ = wake_addr;
            if let Some(handle) = l.accept_thread.take() {
                let _ = handle.join();
            }
        }

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

    /// Test-only helper that simulates an abrupt server crash (dropping the TCP listener and
    /// active peer sockets WITHOUT sending `FrameType::Close`, triggering client reconnect).
    #[cfg(all(test, feature = "gstreamer"))]
    pub(crate) fn simulate_abrupt_crash(&self) {
        {
            let guard = self.active_peers.lock().unwrap_or_else(|e| e.into_inner());
            for peer in guard.values() {
                peer.abrupt_stop.store(true, Ordering::Release);
            }
        }
        self.stop_listener();
    }
}

impl Drop for Sht3SessionManager {
    fn drop(&mut self) {
        self.stop_listener();
    }
}

impl AudioTransportEngine for Sht3SessionManager {
    fn engine_id(&self) -> &'static str {
        "gst-srtp"
    }

    fn start_listener(
        &self,
        bind_addr: SocketAddr,
        server_device_id: String,
        rtp_port: u16,
        trust: Arc<dyn TrustLookup>,
        playback_factory: PlaybackEndpointFactory,
    ) -> TransportResult<SocketAddr> {
        Sht3SessionManager::start_listener(
            self,
            bind_addr,
            Sht3ServerConfig::new(server_device_id).with_rtp_port(rtp_port),
            trust,
            playback_factory,
        )
    }

    fn stop_listener(&self) {
        Sht3SessionManager::stop_listener(self);
    }

    fn listener_addr(&self) -> Option<SocketAddr> {
        Sht3SessionManager::listener_addr(self)
    }

    fn active_inbound_peers(&self) -> Vec<String> {
        Sht3SessionManager::active_inbound_peers(self)
    }

    fn disconnect_peer(&self, device_id: &str) -> bool {
        Sht3SessionManager::disconnect_peer(self, device_id)
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

#[cfg(feature = "gstreamer")]
fn snapshot_receiver_diag(receiver: &GstAudioReceiver, sender_ssrc: u32) -> InboundPeerDiagnostics {
    let stats = receiver.jitter_stats();
    InboundPeerDiagnostics {
        audio_frames_received: stats.num_pushed,
        jb_buffered_ms: stats.buffered_ms,
        jb_lost: stats.num_lost,
        jb_late: stats.num_late,
        jb_percent: stats.percent,
        jitter_us: stats.avg_jitter_ns as f64 / 1000.0,
        jitter_ms: stats.avg_jitter_ns as f64 / 1_000_000.0,
        rtp_port: receiver.rtp_port(),
        volume: receiver.volume(),
        muted: receiver.is_muted(),
        sender_ssrc,
    }
}

#[cfg(feature = "gstreamer")]
fn spawn_inbound_peer(
    mut conn: Sht3ServerConnection,
    receiver: GstAudioReceiver,
    active_peers: &Arc<Mutex<HashMap<String, ActiveInboundPeer>>>,
    last_peer_diagnostics: &Arc<Mutex<HashMap<String, InboundPeerDiagnostics>>>,
    next_session_id: &Arc<AtomicU64>,
    listener_stop: &Arc<AtomicBool>,
) {
    use std::io::ErrorKind;

    let peer_id = conn.peer_device_id().to_string();
    let sender_ssrc = conn.sender_ssrc();
    let session_id = next_session_id.fetch_add(1, Ordering::Relaxed);
    let peer_stop = Arc::new(AtomicBool::new(false));
    #[cfg(test)]
    let abrupt_stop = Arc::new(AtomicBool::new(false));
    let diagnostics = Arc::new(Mutex::new(snapshot_receiver_diag(&receiver, sender_ssrc)));

    let peer_stop_c = Arc::clone(&peer_stop);
    #[cfg(test)]
    let abrupt_stop_c = Arc::clone(&abrupt_stop);
    let listener_stop_c = Arc::clone(listener_stop);
    let peers_map = Arc::clone(active_peers);
    let last_diag_map = Arc::clone(last_peer_diagnostics);
    let diag_c = Arc::clone(&diagnostics);
    let peer_id_c = peer_id.clone();

    let worker = std::thread::spawn(move || {
        let mut last_activity = Instant::now();
        let mut remote_closed = false;

        while !peer_stop_c.load(Ordering::Acquire)
            && !listener_stop_c.load(Ordering::Acquire)
            && receiver.is_healthy()
        {
            match conn.recv_frame() {
                Ok(frame) => {
                    last_activity = Instant::now();
                    match frame.header.frame_type {
                        FrameType::Control => {
                            if let Ok(msg) = ControlMessage::decode(&frame.payload) {
                                match msg {
                                    ControlMessage::SetVolume(pct) => {
                                        receiver.set_volume(pct);
                                        let state_msg = ControlMessage::VolumeState {
                                            volume: receiver.volume(),
                                            muted: receiver.is_muted(),
                                        };
                                        if conn.send_control(&state_msg).is_err() {
                                            break;
                                        }
                                    }
                                    ControlMessage::SetMute(muted) => {
                                        receiver.set_mute(muted);
                                        let state_msg = ControlMessage::VolumeState {
                                            volume: receiver.volume(),
                                            muted: receiver.is_muted(),
                                        };
                                        if conn.send_control(&state_msg).is_err() {
                                            break;
                                        }
                                    }
                                    ControlMessage::VolumeState { .. }
                                    | ControlMessage::DelayOffsetMs(_) => {}
                                }
                            }
                        }
                        FrameType::Ping => {
                            if let Ok(ping) = PingPayload::decode(&frame.payload) {
                                let stats = receiver.jitter_stats();
                                let snap = snapshot_receiver_diag(&receiver, sender_ssrc);
                                *diag_c.lock().unwrap_or_else(|e| e.into_inner()) = snap;
                                let pong = PongPayload {
                                    client_time_us: ping.client_time_us,
                                    jitter_buf_ms: stats.buffered_ms,
                                    dropped_frames: stats.dropped_frames,
                                };
                                if conn.send_pong(&pong).is_err() {
                                    break;
                                }
                            }
                        }
                        FrameType::Close => {
                            remote_closed = true;
                            break;
                        }
                        FrameType::Pong => {}
                    }
                }
                Err(TransportError::Io(ref e))
                    if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                {
                    let snap = snapshot_receiver_diag(&receiver, sender_ssrc);
                    *diag_c.lock().unwrap_or_else(|e| e.into_inner()) = snap;
                    if last_activity.elapsed() >= DEAD_PEER_IDLE_TIMEOUT {
                        break;
                    }
                }
                Err(_) => break,
            }
        }

        let final_diag = snapshot_receiver_diag(&receiver, sender_ssrc);
        *diag_c.lock().unwrap_or_else(|e| e.into_inner()) = final_diag.clone();
        last_diag_map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(peer_id_c.clone(), final_diag);

        // Deadlock-free pipeline teardown on owning Rust thread.
        receiver.stop();

        #[cfg(test)]
        let suppress_close = abrupt_stop_c.load(Ordering::Acquire);
        #[cfg(not(test))]
        let suppress_close = false;

        if !suppress_close
            && !remote_closed
            && (peer_stop_c.load(Ordering::Acquire) || listener_stop_c.load(Ordering::Acquire))
        {
            let _ = conn.send_close();
        }

        let mut guard = peers_map.lock().unwrap_or_else(|e| e.into_inner());
        if guard
            .get(&peer_id_c)
            .is_some_and(|entry| entry.session_id == session_id)
        {
            guard.remove(&peer_id_c);
        }
    });

    // Replace any stale session for the same device_id and join it outside the map lock.
    let previous = {
        let mut guard = active_peers.lock().unwrap_or_else(|e| e.into_inner());
        guard.insert(
            peer_id,
            ActiveInboundPeer {
                session_id,
                stop_flag: peer_stop,
                #[cfg(test)]
                abrupt_stop,
                diagnostics,
                worker: Some(worker),
            },
        )
    };
    if let Some(mut old) = previous {
        old.stop_flag.store(true, Ordering::Release);
        if let Some(handle) = old.worker.take() {
            let _ = handle.join();
        }
    }
}
