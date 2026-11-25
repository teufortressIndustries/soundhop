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
pub mod udp;
pub mod windows_pcm;
pub mod wire;

use std::collections::{HashMap, VecDeque};
use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use thiserror::Error;

pub use client::{Sht3ClientConfig, Sht3ClientConnection, Sht3ClientReader, Sht3ClientWriter};
#[cfg(feature = "opus")]
pub use codec::OpusCodec;
pub use codec::{create_codec, AudioCodec, AudioProfile, PcmCodec};
pub use crypto::{AeadSession, EphemeralKeypair, ReplayWindow, Role, SessionKeys};
pub use dsp::{
    apply_gain_buffer, apply_gain_i16, AdaptiveResampler, DriftController, LinearResampler,
    PlaybackResampler,
};
pub use jitter::{JitterBuffer, JitterConfig, JitterStats, PopOutcome};
#[cfg(target_os = "linux")]
pub use linux_pcm::{LinuxPcmCapture, LinuxPcmPlayback};
pub use pcm::{
    ClockedMockPcmPlayback, ClockedMockPcmPlaybackHandle, MockPcmCapture, MockPcmPlayback,
    MockPcmPlaybackHandle, PcmCapture, PcmPlayback, PcmPlaybackFactory, PCM_CHANNELS,
    PCM_FRAME_SAMPLES_10MS, PCM_FRAME_SAMPLES_20MS, PCM_SAMPLE_RATE,
};
pub use server::{Sht3Listener, Sht3ServerConfig, Sht3ServerConnection};
pub use udp::{bind_dscp_ef_udp, set_dscp_ef, Sht3UdpReceiver, Sht3UdpSender, UdpTarget, DSCP_EF};
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
/// Upper bound for the dedicated outbound reader's socket poll timeout so stop/reconnect joins stay snappy.
const MAX_OUTBOUND_READER_TIMEOUT: Duration = Duration::from_millis(50);
/// Default interval between outbound `Ping` telemetry probes.
const DEFAULT_PING_INTERVAL: Duration = Duration::from_millis(500);
/// Maximum concurrent in-flight SHT3 server handshakes accepted before dropping excess raw sockets.
const MAX_INFLIGHT_HANDSHAKES: usize = 8;
/// Rekey proactively once directional AEAD budget falls to this many frames remaining.
const PROACTIVE_REKEY_FRAMES_REMAINING: u64 = 100;
/// Minimum holdoff before an idle primed jitter buffer emits PLC frames.
const MIN_PLC_HOLDOFF_MS: u64 = 60;
/// Rolling window for detecting inbound underrun spikes.
const BITRATE_UNDERRUN_WINDOW: Duration = Duration::from_secs(2);
/// Clean delivery window required before stepping Opus bitrate back up.
const BITRATE_RECOVERY_WINDOW: Duration = Duration::from_secs(15);
/// Minimum interval between periodic inbound diagnostic log lines.
const INBOUND_DIAG_LOG_INTERVAL: Duration = Duration::from_secs(10);
/// Minimum interval between anomaly-triggered inbound diagnostic log lines.
const INBOUND_DIAG_ANOMALY_COOLDOWN: Duration = Duration::from_secs(2);

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
    pub reconnect_window: Duration,
    /// Prefer encrypted SHT3-over-UDP (DSCP EF `0xB8`) for audio frames when the receiver
    /// advertises a UDP port over the TCP control channel, falling back to TCP automatically.
    pub prefer_udp: bool,
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
            reconnect_window: Duration::ZERO,
            prefer_udp: false,
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

    pub fn with_prefer_udp(mut self, prefer_udp: bool) -> Self {
        self.prefer_udp = prefer_udp;
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

struct OutboundReaderHandle {
    conn_stop: Arc<AtomicBool>,
    conn_broken: Arc<AtomicBool>,
    fatal_error: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl OutboundReaderHandle {
    fn stop_and_join(&mut self) {
        self.conn_stop.store(true, Ordering::Release);
        if let Some(handle) = self.join.take() {
            let _ = handle.join();
        }
    }
}

struct OutboundReaderShared {
    session_stop: Arc<AtomicBool>,
    remote_closed: Arc<AtomicBool>,
    pending_bitrate_kbps: Arc<AtomicU16>,
    udp_audio_port: Arc<AtomicU16>,
    telemetry: Arc<Mutex<StreamTelemetry>>,
    start_instant: Instant,
    read_timeout: Duration,
}

fn spawn_outbound_reader(
    mut reader: Sht3ClientReader,
    shared: &OutboundReaderShared,
) -> OutboundReaderHandle {
    let conn_stop = Arc::new(AtomicBool::new(false));
    let conn_broken = Arc::new(AtomicBool::new(false));
    let fatal_error = Arc::new(AtomicBool::new(false));

    let _ = reader.set_read_timeout(Some(shared.read_timeout));

    let session_stop = Arc::clone(&shared.session_stop);
    let remote_closed = Arc::clone(&shared.remote_closed);
    let pending_bitrate_kbps = Arc::clone(&shared.pending_bitrate_kbps);
    let udp_audio_port = Arc::clone(&shared.udp_audio_port);
    let telemetry = Arc::clone(&shared.telemetry);
    let start_instant = shared.start_instant;
    let conn_stop_c = Arc::clone(&conn_stop);
    let conn_broken_c = Arc::clone(&conn_broken);
    let fatal_error_c = Arc::clone(&fatal_error);

    let join = thread::spawn(move || loop {
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
                        if let Ok(msg) = ControlMessage::decode(&frame.payload) {
                            match msg {
                                ControlMessage::VolumeState { volume, muted } => {
                                    let mut t = telemetry.lock().unwrap_or_else(|e| e.into_inner());
                                    t.remote_volume = volume;
                                    t.remote_muted = muted;
                                }
                                ControlMessage::BitrateHint(kbps) => {
                                    pending_bitrate_kbps
                                        .store(kbps.clamp(16, 510), Ordering::Release);
                                }
                                ControlMessage::UdpAudioPort(port) => {
                                    udp_audio_port.store(port, Ordering::Release);
                                }
                                ControlMessage::SetVolume(_)
                                | ControlMessage::SetMute(_)
                                | ControlMessage::DelayOffsetMs(_) => {}
                            }
                        }
                    }
                    FrameType::Close => {
                        remote_closed.store(true, Ordering::Release);
                        break;
                    }
                    FrameType::AudioOpus | FrameType::AudioPcm | FrameType::Ping => {}
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

fn drain_capture_until(
    capture: &mut dyn PcmCapture,
    pcm_buf: &mut [i16],
    deadline: Instant,
    stop_flag: &AtomicBool,
) -> bool {
    while Instant::now() < deadline {
        if stop_flag.load(Ordering::Acquire) {
            return false;
        }
        let step_start = Instant::now();
        match capture.read_frame(pcm_buf) {
            Ok(0) | Err(_) => return false,
            Ok(_) => {
                let _ = capture.take_discontinuity();
            }
        }
        if step_start.elapsed() < Duration::from_millis(2) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if !remaining.is_zero() {
                thread::sleep(remaining.min(Duration::from_millis(10)));
            }
        }
    }
    !stop_flag.load(Ordering::Acquire)
}

#[allow(clippy::too_many_arguments)]
fn try_reconnect_outbound(
    reason_rekey: bool,
    config: &OutboundConfig,
    client_cfg: &Sht3ClientConfig,
    capture: &mut dyn PcmCapture,
    pcm_buf: &mut [i16],
    codec: &mut dyn AudioCodec,
    last_requested_volume: Option<u8>,
    last_requested_mute: Option<bool>,
    old_writer: Option<Sht3ClientWriter>,
    reader_handle: &mut OutboundReaderHandle,
    reader_shared: &OutboundReaderShared,
) -> Option<(Sht3ClientWriter, OutboundReaderHandle)> {
    if reason_rekey {
        if let Some(mut w) = old_writer {
            let _ = w.send_close();
        }
    } else {
        drop(old_writer);
    }

    // Drain any buffered frames (such as FrameType::Close) before checking remote_closed.
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
            if !drain_capture_until(capture, pcm_buf, wait_until, &reader_shared.session_stop) {
                return None;
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
                let Ok((mut new_writer, new_reader)) = conn.into_split() else {
                    return None;
                };
                codec.reset();

                let mut ctrl_io_err = false;
                if let Some(vol) = last_requested_volume {
                    match new_writer.send_control(&ControlMessage::SetVolume(vol)) {
                        Ok(()) => {}
                        Err(TransportError::Io(_)) => ctrl_io_err = true,
                        Err(_) => return None,
                    }
                }
                if !ctrl_io_err {
                    if let Some(muted) = last_requested_mute {
                        match new_writer.send_control(&ControlMessage::SetMute(muted)) {
                            Ok(()) => {}
                            Err(TransportError::Io(_)) => ctrl_io_err = true,
                            Err(_) => return None,
                        }
                    }
                }
                if !ctrl_io_err {
                    let new_reader_handle = spawn_outbound_reader(new_reader, reader_shared);
                    return Some((new_writer, new_reader_handle));
                }
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
    /// Connect to the target SHT3 listener and start streaming PCM from `capture`.
    pub fn start(
        config: OutboundConfig,
        mut capture: Box<dyn PcmCapture>,
    ) -> TransportResult<Self> {
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

        let mut codec = create_codec(&audio_params)?;
        let frame_type = match audio_params.codec {
            CodecId::Opus => FrameType::AudioOpus,
            CodecId::PcmS16Le => FrameType::AudioPcm,
        };
        let frame_samples = codec.interleaved_samples_per_frame();
        let frame_spc = codec.frame_samples_per_channel() as u32;

        let stop_flag = Arc::new(AtomicBool::new(false));
        capture.attach_stop_flag(Arc::clone(&stop_flag));

        let (initial_writer, initial_reader) = conn.into_split()?;

        let running = Arc::new(AtomicBool::new(true));
        let remote_closed = Arc::new(AtomicBool::new(false));
        let pending_bitrate_kbps = Arc::new(AtomicU16::new(0));
        let udp_audio_port = Arc::new(AtomicU16::new(0));
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
            pending_bitrate_kbps: Arc::clone(&pending_bitrate_kbps),
            udp_audio_port: Arc::clone(&udp_audio_port),
            telemetry: Arc::clone(&telemetry),
            start_instant,
            read_timeout: reader_timeout,
        };
        let mut reader_handle = spawn_outbound_reader(initial_reader, &reader_shared);

        let stop_c = Arc::clone(&stop_flag);
        let running_c = Arc::clone(&running);
        let telemetry_c = Arc::clone(&telemetry);
        let ping_interval = config.ping_interval;
        let server_device_id_c = server_device_id.clone();

        let worker = thread::spawn(move || {
            let mut writer_opt = Some(initial_writer);
            let mut udp_sender: Option<Sht3UdpSender> = None;
            let mut udp_failed = false;
            let mut last_ping_at: Option<Instant> = None;
            let mut pcm_buf = vec![0i16; frame_samples];
            let mut encoded_buf = Vec::with_capacity(MAX_PAYLOAD_LEN);
            let mut timestamp_samples: u32 = 0;
            let mut pending_discontinuity = false;
            let mut last_requested_volume: Option<u8> = None;
            let mut last_requested_mute: Option<bool> = None;

            'stream: while !stop_c.load(Ordering::Acquire)
                && !remote_closed.load(Ordering::Acquire)
                && !reader_handle.fatal_error.load(Ordering::Acquire)
            {
                if reader_handle.conn_broken.load(Ordering::Acquire) {
                    let Some((new_writer, new_reader_handle)) = try_reconnect_outbound(
                        false,
                        &config,
                        &client_cfg,
                        capture.as_mut(),
                        &mut pcm_buf,
                        codec.as_mut(),
                        last_requested_volume,
                        last_requested_mute,
                        writer_opt.take(),
                        &mut reader_handle,
                        &reader_shared,
                    ) else {
                        break 'stream;
                    };
                    writer_opt = Some(new_writer);
                    reader_handle = new_reader_handle;
                    pending_discontinuity = true;
                }

                // Drain pending control/ping commands before encoding the next frame.
                while let Ok(cmd) = cmd_rx.try_recv() {
                    match cmd {
                        OutboundCommand::Control(msg) => {
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
                                    let Some((new_writer, new_reader_handle)) =
                                        try_reconnect_outbound(
                                            false,
                                            &config,
                                            &client_cfg,
                                            capture.as_mut(),
                                            &mut pcm_buf,
                                            codec.as_mut(),
                                            last_requested_volume,
                                            last_requested_mute,
                                            writer_opt.take(),
                                            &mut reader_handle,
                                            &reader_shared,
                                        )
                                    else {
                                        break 'stream;
                                    };
                                    writer_opt = Some(new_writer);
                                    reader_handle = new_reader_handle;
                                    pending_discontinuity = true;
                                    // Re-send non-volume/mute control if it failed mid-disconnect.
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
                        OutboundCommand::Ping => {
                            let now_us = start_instant
                                .elapsed()
                                .as_micros()
                                .min(u128::from(u64::MAX))
                                as u64;
                            let send_res = match writer_opt.as_mut() {
                                Some(w) => w.send_ping(&PingPayload {
                                    client_time_us: now_us,
                                }),
                                None => break 'stream,
                            };
                            match send_res {
                                Ok(()) => {
                                    last_ping_at = Some(Instant::now());
                                }
                                Err(TransportError::Io(_)) => {
                                    let Some((new_writer, new_reader_handle)) =
                                        try_reconnect_outbound(
                                            false,
                                            &config,
                                            &client_cfg,
                                            capture.as_mut(),
                                            &mut pcm_buf,
                                            codec.as_mut(),
                                            last_requested_volume,
                                            last_requested_mute,
                                            writer_opt.take(),
                                            &mut reader_handle,
                                            &reader_shared,
                                        )
                                    else {
                                        break 'stream;
                                    };
                                    writer_opt = Some(new_writer);
                                    reader_handle = new_reader_handle;
                                    pending_discontinuity = true;
                                }
                                Err(_) => break 'stream,
                            }
                        }
                        OutboundCommand::Stop => {
                            stop_c.store(true, Ordering::Release);
                            break 'stream;
                        }
                    }
                }

                let hint = pending_bitrate_kbps.swap(0, Ordering::AcqRel);
                if hint > 0 {
                    let _ = codec.set_bitrate_kbps(hint);
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
                            if let Some(w) = writer_opt.as_mut() {
                                let now_us = start_instant
                                    .elapsed()
                                    .as_micros()
                                    .min(u128::from(u64::MAX))
                                    as u64;
                                if w.send_ping(&PingPayload {
                                    client_time_us: now_us,
                                })
                                .is_ok()
                                {
                                    let pong_deadline = Instant::now() + Duration::from_millis(500);
                                    while Instant::now() < pong_deadline {
                                        let pongs = telemetry_c
                                            .lock()
                                            .unwrap_or_else(|e| e.into_inner())
                                            .pongs_received;
                                        if pongs > 0
                                            || remote_closed.load(Ordering::Acquire)
                                            || reader_handle.conn_broken.load(Ordering::Acquire)
                                            || reader_handle.fatal_error.load(Ordering::Acquire)
                                            || stop_c.load(Ordering::Acquire)
                                        {
                                            break;
                                        }
                                        thread::sleep(Duration::from_millis(2));
                                    }
                                }
                            }
                        }
                        if let Some(mut w) = writer_opt.take() {
                            let _ = w.send_close();
                        }
                        break 'stream;
                    }
                    Ok(n) => n,
                    Err(_) => break 'stream,
                };

                if stop_c.load(Ordering::Acquire)
                    || remote_closed.load(Ordering::Acquire)
                    || reader_handle.fatal_error.load(Ordering::Acquire)
                {
                    break 'stream;
                }

                if capture.take_discontinuity() {
                    codec.reset();
                    pending_discontinuity = true;
                }

                let hint = pending_bitrate_kbps.swap(0, Ordering::AcqRel);
                if hint > 0 {
                    let _ = codec.set_bitrate_kbps(hint);
                }

                if n < frame_samples {
                    pcm_buf[n..frame_samples].fill(0);
                }

                // Proactive rekey before directional AEAD frame limit is exhausted.
                let tcp_rekey = writer_opt
                    .as_ref()
                    .is_some_and(|w| w.frames_remaining() <= PROACTIVE_REKEY_FRAMES_REMAINING);
                let udp_rekey = udp_sender
                    .as_ref()
                    .is_some_and(|s| s.frames_remaining_min() <= PROACTIVE_REKEY_FRAMES_REMAINING);
                if tcp_rekey || udp_rekey {
                    udp_audio_port.store(0, Ordering::Release);
                    udp_sender = None;
                    udp_failed = false;
                    let Some((new_writer, new_reader_handle)) = try_reconnect_outbound(
                        true,
                        &config,
                        &client_cfg,
                        capture.as_mut(),
                        &mut pcm_buf,
                        codec.as_mut(),
                        last_requested_volume,
                        last_requested_mute,
                        writer_opt.take(),
                        &mut reader_handle,
                        &reader_shared,
                    ) else {
                        break 'stream;
                    };
                    writer_opt = Some(new_writer);
                    reader_handle = new_reader_handle;
                    pending_discontinuity = true;
                }

                // Lazily attach the encrypted SHT3-over-UDP data plane once the server
                // advertises its UDP audio port over the TCP control channel.
                if config.prefer_udp && udp_sender.is_none() && !udp_failed {
                    let port = udp_audio_port.load(Ordering::Acquire);
                    if port > 0 {
                        if let Some(udp_seal) = writer_opt.as_mut().and_then(|w| w.take_udp_seal())
                        {
                            let bind_ip = match config.target_addr {
                                SocketAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                                SocketAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
                            };
                            match Sht3UdpSender::bind(SocketAddr::new(bind_ip, 0)) {
                                Ok(mut sender) => {
                                    let udp_addr = SocketAddr::new(config.target_addr.ip(), port);
                                    sender.add_target(
                                        server_device_id_c.clone(),
                                        udp_addr,
                                        udp_seal,
                                    );
                                    udp_sender = Some(sender);
                                }
                                Err(_) => {
                                    udp_failed = true;
                                }
                            }
                        }
                    }
                }

                encoded_buf.clear();
                let Ok(pkt_len) = codec.encode(&pcm_buf[..frame_samples], &mut encoded_buf) else {
                    break 'stream;
                };

                let flags = if pending_discontinuity {
                    FLAG_DISCONTINUITY
                } else {
                    0
                };
                let mut sent_via_udp = false;
                if let Some(sender) = udp_sender.as_mut() {
                    if sender
                        .send_to_peer(
                            &server_device_id_c,
                            frame_type,
                            flags,
                            timestamp_samples,
                            &encoded_buf,
                        )
                        .is_ok()
                    {
                        sent_via_udp = true;
                    } else {
                        // Fall back to TCP SHT3 audio seamlessly on UDP send failure.
                        udp_sender = None;
                        udp_failed = true;
                    }
                }
                let send_res = if sent_via_udp {
                    Ok(())
                } else {
                    match writer_opt.as_mut() {
                        Some(w) => w
                            .send_audio(frame_type, flags, timestamp_samples, &encoded_buf)
                            .and_then(|()| w.flush()),
                        None => break 'stream,
                    }
                };
                match send_res {
                    Ok(()) => {
                        pending_discontinuity = false;
                        timestamp_samples = timestamp_samples.wrapping_add(frame_spc);
                        {
                            let mut t = telemetry_c.lock().unwrap_or_else(|e| e.into_inner());
                            t.frames_sent += 1;
                            t.bytes_sent += pkt_len as u64;
                        }
                    }
                    Err(TransportError::Io(_)) => {
                        let Some((new_writer, new_reader_handle)) = try_reconnect_outbound(
                            false,
                            &config,
                            &client_cfg,
                            capture.as_mut(),
                            &mut pcm_buf,
                            codec.as_mut(),
                            last_requested_volume,
                            last_requested_mute,
                            writer_opt.take(),
                            &mut reader_handle,
                            &reader_shared,
                        ) else {
                            break 'stream;
                        };
                        writer_opt = Some(new_writer);
                        reader_handle = new_reader_handle;
                        pending_discontinuity = true;
                        continue 'stream;
                    }
                    Err(_) => break 'stream,
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
                    if let Some(w) = writer_opt.as_mut() {
                        if let Err(TransportError::Io(_)) = w.send_ping(&PingPayload {
                            client_time_us: now_us,
                        }) {
                            let Some((new_writer, new_reader_handle)) = try_reconnect_outbound(
                                false,
                                &config,
                                &client_cfg,
                                capture.as_mut(),
                                &mut pcm_buf,
                                codec.as_mut(),
                                last_requested_volume,
                                last_requested_mute,
                                writer_opt.take(),
                                &mut reader_handle,
                                &reader_shared,
                            ) else {
                                break 'stream;
                            };
                            writer_opt = Some(new_writer);
                            reader_handle = new_reader_handle;
                            pending_discontinuity = true;
                        }
                    }
                    last_ping_at = Some(Instant::now());
                }
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

/// Diagnostic telemetry snapshot for an inbound SHT3 peer session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InboundPeerDiagnostics {
    pub audio_frames_received: u64,
    pub decode_errors: u64,
    pub jb_underruns: u64,
    pub jb_overruns: u64,
    pub jb_plc_frames: u64,
    pub jb_buffered_ms: u16,
    pub sink_buffered_ms: Option<f64>,
    pub sink_target_ms: f64,
    pub sink_underruns: u64,
    pub sink_overruns: u64,
    pub drift_ratio: f64,
    pub drift_ppm: f64,
    pub drift_smoothed_fill_ms: f64,
    pub jitter_us: f64,
    pub jitter_ms: f64,
    pub max_inter_arrival_gap_us: u64,
}

struct ActiveInboundPeer {
    session_id: u64,
    stop_flag: Arc<AtomicBool>,
    diagnostics: Arc<Mutex<InboundPeerDiagnostics>>,
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
    last_peer_diagnostics: Arc<Mutex<HashMap<String, InboundPeerDiagnostics>>>,
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

        let accept_thread = thread::spawn(move || {
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

                        thread::spawn(move || {
                            struct HandshakeSlotGuard(Arc<AtomicUsize>);
                            impl Drop for HandshakeSlotGuard {
                                fn drop(&mut self) {
                                    self.0.fetch_sub(1, Ordering::Release);
                                }
                            }
                            let _slot_guard = HandshakeSlotGuard(hs_counter);

                            let Ok(conn) =
                                Sht3Listener::handshake_arc(stream, &hs_cfg, hs_trust.as_ref())
                            else {
                                return;
                            };

                            let poll_ms =
                                (u64::from(conn.audio_params().frame_ms.max(2)) / 2).max(5);
                            let frame_timeout = Duration::from_millis(poll_ms)
                                .min(hs_cfg.io_timeout.unwrap_or(MAX_MANAGED_INBOUND_IO_TIMEOUT))
                                .min(MAX_MANAGED_INBOUND_IO_TIMEOUT);
                            let _ = conn.set_read_timeout(Some(frame_timeout));

                            if !hs_stop.load(Ordering::Acquire) {
                                spawn_inbound_peer(
                                    conn,
                                    &hs_peers,
                                    &hs_last_diag,
                                    &hs_next_id,
                                    &hs_stop,
                                    &hs_factory,
                                    initial_volume,
                                    initial_muted,
                                    hs_cfg.enable_udp,
                                );
                            }
                        });
                    }
                    Err(_) => {
                        if stop_c.load(Ordering::Acquire) {
                            break;
                        }
                        // Transient accept error: keep listening.
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

struct BitrateAdaptation {
    nominal_kbps: u16,
    current_kbps: u16,
    underrun_times: VecDeque<Instant>,
    last_underrun_at: Option<Instant>,
    last_step_up_at: Option<Instant>,
    underrun_window: Duration,
    recovery_window: Duration,
}

impl BitrateAdaptation {
    fn new(nominal_kbps: u16) -> Self {
        let clamped = nominal_kbps.clamp(32, 510);
        Self {
            nominal_kbps: clamped,
            current_kbps: clamped,
            underrun_times: VecDeque::with_capacity(8),
            last_underrun_at: None,
            last_step_up_at: None,
            underrun_window: BITRATE_UNDERRUN_WINDOW,
            recovery_window: BITRATE_RECOVERY_WINDOW,
        }
    }

    fn step_down_kbps(current: u16) -> u16 {
        if current > 128 {
            128
        } else if current > 96 {
            96
        } else if current > 64 {
            64
        } else if current > 48 {
            48
        } else {
            32
        }
    }

    fn step_up_kbps(current: u16, nominal: u16) -> u16 {
        let next = if current < 48 {
            48
        } else if current < 64 {
            64
        } else if current < 96 {
            96
        } else if current < 128 {
            128
        } else {
            nominal
        };
        next.min(nominal)
    }

    fn on_underruns(&mut self, now: Instant, count: u64) -> Option<u16> {
        if count == 0 {
            return None;
        }
        self.last_underrun_at = Some(now);
        self.last_step_up_at = None;
        for _ in 0..count.min(16) {
            self.underrun_times.push_back(now);
        }
        while self
            .underrun_times
            .front()
            .is_some_and(|&t| now.saturating_duration_since(t) > self.underrun_window)
        {
            self.underrun_times.pop_front();
        }
        if self.underrun_times.len() >= 3 {
            self.underrun_times.clear();
            let next = Self::step_down_kbps(self.current_kbps);
            if next < self.current_kbps {
                self.current_kbps = next;
                return Some(next);
            }
        }
        None
    }

    fn on_clean_frame(&mut self, now: Instant) -> Option<u16> {
        if self.current_kbps >= self.nominal_kbps {
            return None;
        }
        let anchor = self.last_step_up_at.or(self.last_underrun_at)?;
        if now.saturating_duration_since(anchor) >= self.recovery_window {
            let next = Self::step_up_kbps(self.current_kbps, self.nominal_kbps);
            self.last_step_up_at = Some(now);
            if next > self.current_kbps {
                self.current_kbps = next;
                return Some(next);
            }
        }
        None
    }
}

fn write_inbound_pcm(
    playback: &mut dyn PcmPlayback,
    pcm: &[i16],
    resampler: &mut AdaptiveResampler,
    resampled_pcm: &mut Vec<i16>,
    ratio: f64,
) -> bool {
    if playback.buffered_ms().is_some() {
        resampled_pcm.clear();
        resampler.process_stereo(pcm, ratio, resampled_pcm);
        playback.write_frame(resampled_pcm).is_ok()
    } else {
        playback.write_frame(pcm).is_ok()
    }
}

#[allow(clippy::too_many_arguments)]
fn snapshot_inbound_diagnostics(
    jb: &JitterBuffer,
    playback: &dyn PcmPlayback,
    drift_ctrl: &DriftController,
    audio_frames_received: u64,
    decode_errors: u64,
    jitter_us: f64,
    max_inter_arrival_gap_us: u64,
) -> InboundPeerDiagnostics {
    let stats = jb.stats();
    InboundPeerDiagnostics {
        audio_frames_received,
        decode_errors,
        jb_underruns: stats.underruns,
        jb_overruns: stats.dropped_overrun,
        jb_plc_frames: stats.plc_frames,
        jb_buffered_ms: stats.buffered_ms,
        sink_buffered_ms: playback.buffered_ms(),
        sink_target_ms: playback.sink_target_ms(),
        sink_underruns: playback.sink_underruns(),
        sink_overruns: playback.sink_overruns(),
        drift_ratio: drift_ctrl.ratio(),
        drift_ppm: drift_ctrl.ppm(),
        drift_smoothed_fill_ms: drift_ctrl.smoothed_fill_ms(),
        jitter_us,
        jitter_ms: jitter_us / 1000.0,
        max_inter_arrival_gap_us,
    }
}

#[allow(clippy::too_many_arguments)]
fn maybe_emit_inbound_diag_log(
    peer_id: &str,
    diag: &InboundPeerDiagnostics,
    now: Instant,
    last_periodic_log_at: &mut Instant,
    last_anomaly_log_at: &mut Option<Instant>,
    prev_anomalies: &mut (u64, u64, u64, u64, u64),
) {
    let cur_anomalies = (
        diag.decode_errors,
        diag.jb_underruns,
        diag.jb_overruns,
        diag.sink_underruns,
        diag.sink_overruns,
    );
    let anomaly_increased = cur_anomalies != *prev_anomalies;
    let periodic_due =
        now.saturating_duration_since(*last_periodic_log_at) >= INBOUND_DIAG_LOG_INTERVAL;
    let anomaly_due = anomaly_increased
        && last_anomaly_log_at
            .is_none_or(|t| now.saturating_duration_since(t) >= INBOUND_DIAG_ANOMALY_COOLDOWN);

    if !periodic_due && !anomaly_due {
        return;
    }

    let level = if anomaly_increased { "WARN" } else { "INFO" };
    let sink_ms_str = diag
        .sink_buffered_ms
        .map_or_else(|| "none".to_string(), |ms| format!("{ms:.1}"));
    let msg = format!(
        "sht3_inbound peer={peer_id} frames={} decode_err={} jb_ms={} jb_over={} jb_under={} jb_plc={} sink_ms={sink_ms_str} sink_over={} sink_under={} drift_ppm={:.1} ratio={:.6} jitter_ms={:.2} max_gap_ms={:.2}",
        diag.audio_frames_received,
        diag.decode_errors,
        diag.jb_buffered_ms,
        diag.jb_overruns,
        diag.jb_underruns,
        diag.jb_plc_frames,
        diag.sink_overruns,
        diag.sink_underruns,
        diag.drift_ppm,
        diag.drift_ratio,
        diag.jitter_ms,
        (diag.max_inter_arrival_gap_us as f64) / 1000.0,
    );
    crate::util::append_log_line(level, &msg);

    *prev_anomalies = cur_anomalies;
    if periodic_due {
        *last_periodic_log_at = now;
    }
    if anomaly_due {
        *last_anomaly_log_at = Some(now);
    }
}

#[allow(clippy::too_many_arguments)]
fn maybe_pace_inbound_plc(
    jb: &mut JitterBuffer,
    codec: &mut dyn AudioCodec,
    playback: &mut dyn PcmPlayback,
    conn: &mut Sht3ServerConnection,
    out_pcm: &mut Vec<i16>,
    resampled_pcm: &mut Vec<i16>,
    resampler: &mut AdaptiveResampler,
    drift_ratio: f64,
    audio_frames_received: u64,
    last_audio_at: Option<Instant>,
    last_pace_at: &mut Option<Instant>,
    plc_holdoff: Duration,
    frame_interval: Duration,
    prev_underruns: &mut u64,
    bitrate_ctrl: &mut Option<BitrateAdaptation>,
) -> bool {
    let min_frames = (jb.effective_target_frames() * 2) as u64;
    if !jb.is_primed()
        || audio_frames_received < min_frames
        || !last_audio_at.is_some_and(|t| t.elapsed() >= plc_holdoff)
    {
        return true;
    }

    let now = Instant::now();
    if last_pace_at.is_some_and(|t| now.saturating_duration_since(t) < frame_interval) {
        return true;
    }

    out_pcm.clear();
    if jb.pop_with_codec(codec, out_pcm).is_ok()
        && !write_inbound_pcm(playback, out_pcm, resampler, resampled_pcm, drift_ratio)
    {
        return false;
    }
    *last_pace_at = Some(now);

    let cur_underruns = jb.stats().underruns;
    if cur_underruns > *prev_underruns {
        let delta = cur_underruns - *prev_underruns;
        *prev_underruns = cur_underruns;
        if let Some(ref mut ctrl) = bitrate_ctrl {
            if let Some(new_kbps) = ctrl.on_underruns(now, delta) {
                let _ = conn.send_control(&ControlMessage::BitrateHint(new_kbps));
            }
        }
    }
    true
}

#[allow(clippy::too_many_arguments)]
fn process_inbound_audio_frame(
    frame: &DecryptedFrame,
    jb: &mut JitterBuffer,
    codec: &mut dyn AudioCodec,
    playback: &mut dyn PcmPlayback,
    conn: &mut Sht3ServerConnection,
    drift_ctrl: &mut DriftController,
    resampler: &mut AdaptiveResampler,
    decoded_pcm: &mut Vec<i16>,
    out_pcm: &mut Vec<i16>,
    resampled_pcm: &mut Vec<i16>,
    frame_samples: usize,
    frame_ms_u16: u16,
    frame_dt_secs: f64,
    nominal_interval_us: u64,
    audio_frames_received: &mut u64,
    decode_errors: &mut u64,
    last_audio_at: &mut Option<Instant>,
    last_arrival_at: &mut Option<Instant>,
    last_pace_at: &mut Option<Instant>,
    prev_underruns: &mut u64,
    jitter_us: &mut f64,
    max_inter_arrival_gap_us: &mut u64,
    bitrate_ctrl: &mut Option<BitrateAdaptation>,
) -> bool {
    let now = Instant::now();
    if frame.header.flags & FLAG_DISCONTINUITY != 0 {
        jb.reset();
        codec.reset();
        drift_ctrl.reset();
        resampler.reset();
        *prev_underruns = jb.stats().underruns;
        *last_arrival_at = None;
    }
    if let Some(prev_at) = *last_arrival_at {
        let gap_us = now
            .saturating_duration_since(prev_at)
            .as_micros()
            .min(u128::from(u64::MAX)) as u64;
        if gap_us > *max_inter_arrival_gap_us {
            *max_inter_arrival_gap_us = gap_us;
        }
        let d_us = (gap_us as f64 - nominal_interval_us as f64).abs();
        *jitter_us += (d_us - *jitter_us) / 16.0;
    }
    *last_arrival_at = Some(now);

    decoded_pcm.clear();
    let decode_ok = codec.decode(&frame.payload, decoded_pcm).is_ok();
    if !decode_ok {
        *decode_errors += 1;
        decoded_pcm.clear();
        if codec.decode_plc(decoded_pcm).is_err() || decoded_pcm.is_empty() {
            decoded_pcm.resize(frame_samples, 0);
        }
    }
    *audio_frames_received += 1;
    *last_audio_at = Some(now);
    *last_pace_at = None;

    jb.push_realtime(std::mem::take(decoded_pcm));
    decoded_pcm.reserve(frame_samples);

    if jb.is_primed() {
        let steady_cushion = jb.effective_target_frames().saturating_sub(1);
        let target_ms = f64::from(
            (steady_cushion * usize::from(frame_ms_u16)).min(usize::from(u16::MAX)) as u16,
        ) + playback.sink_target_ms();
        drift_ctrl.set_target_ms(target_ms);
        while jb.stats().buffered_frames > steady_cushion {
            if jb.concealed_debt() > 0
                && playback
                    .buffered_ms()
                    .is_some_and(|ms| ms >= playback.sink_target_ms() * 0.75)
                && jb.catchup_concealed_frame()
            {
                continue;
            }
            out_pcm.clear();
            match jb.pop_with_codec(codec, out_pcm) {
                Ok(_) => {
                    if !write_inbound_pcm(
                        playback,
                        out_pcm,
                        resampler,
                        resampled_pcm,
                        drift_ctrl.ratio(),
                    ) {
                        return false;
                    }
                    if let Some(sink_ms) = playback.buffered_ms() {
                        let total_ms = f64::from(jb.stats().buffered_ms) + sink_ms;
                        drift_ctrl.update(total_ms, frame_dt_secs);
                    }
                }
                Err(_) => break,
            }
        }
    }

    if decode_ok {
        if let Some(ref mut ctrl) = bitrate_ctrl {
            if let Some(new_kbps) = ctrl.on_clean_frame(now) {
                let _ = conn.send_control(&ControlMessage::BitrateHint(new_kbps));
            }
        }
    }
    true
}

#[allow(clippy::too_many_arguments)]
fn spawn_inbound_peer(
    mut conn: Sht3ServerConnection,
    active_peers: &Arc<Mutex<HashMap<String, ActiveInboundPeer>>>,
    last_peer_diagnostics: &Arc<Mutex<HashMap<String, InboundPeerDiagnostics>>>,
    next_session_id: &Arc<AtomicU64>,
    manager_stop: &Arc<AtomicBool>,
    playback_factory: &PcmPlaybackFactory,
    initial_volume: u8,
    initial_muted: bool,
    enable_udp: bool,
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
    let diagnostics = Arc::new(Mutex::new(InboundPeerDiagnostics {
        sink_target_ms: playback.sink_target_ms(),
        drift_ratio: 1.0,
        ..InboundPeerDiagnostics::default()
    }));

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
    let last_diag_c = Arc::clone(last_peer_diagnostics);
    let diagnostics_c = Arc::clone(&diagnostics);
    let peer_id_c = peer_id.clone();

    let worker = thread::spawn(move || {
        let mut jb = JitterBuffer::new(JitterConfig {
            target_ms: audio_params.jitter_ms,
            frame_ms: audio_params.frame_ms,
            ..JitterConfig::default()
        });
        let frame_samples = codec.interleaved_samples_per_frame();
        let frame_ms_u16 = u16::from(audio_params.frame_ms.max(1));
        let frame_dt_secs = f64::from(frame_ms_u16) / 1000.0;
        let nominal_interval_us = u64::from(frame_ms_u16) * 1000;
        let frame_interval = Duration::from_millis(u64::from(frame_ms_u16));
        let plc_holdoff =
            Duration::from_millis(u64::from(audio_params.jitter_ms).max(MIN_PLC_HOLDOFF_MS));
        let initial_steady_cushion = jb.effective_target_frames().saturating_sub(1);
        let initial_target_ms = f64::from(
            (initial_steady_cushion * usize::from(frame_ms_u16)).min(usize::from(u16::MAX)) as u16,
        ) + playback.sink_target_ms();
        let mut drift_ctrl = DriftController::new(initial_target_ms);
        let mut resampler = AdaptiveResampler::new();
        let mut decoded_pcm = Vec::with_capacity(frame_samples);
        let mut out_pcm = Vec::with_capacity(frame_samples);
        let mut resampled_pcm = Vec::with_capacity(frame_samples + 16);
        let mut audio_frames_received: u64 = 0;
        let mut decode_errors: u64 = 0;
        let mut last_audio_at: Option<Instant> = None;
        let mut last_arrival_at: Option<Instant> = None;
        let mut last_pace_at: Option<Instant> = None;
        let mut prev_underruns: u64 = 0;
        let mut jitter_us: f64 = 0.0;
        let mut max_inter_arrival_gap_us: u64 = 0;
        let mut last_periodic_log_at = Instant::now();
        let mut last_anomaly_log_at: Option<Instant> = None;
        let mut prev_anomalies: (u64, u64, u64, u64, u64) = (0, 0, 0, 0, 0);
        let mut bitrate_ctrl = (audio_params.codec == CodecId::Opus)
            .then(|| BitrateAdaptation::new(audio_params.bitrate_kbps));
        let mut clean_close = false;

        let mut udp_rx: Option<Sht3UdpReceiver> = if enable_udp {
            conn.take_udp_open().and_then(|udp_open| {
                let bind_ip = conn
                    .local_addr()
                    .map(|a| a.ip())
                    .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
                match Sht3UdpReceiver::bind(SocketAddr::new(bind_ip, 0), udp_open) {
                    Ok(rx) => {
                        let _ = rx.set_nonblocking(true);
                        if let Ok(addr) = rx.local_addr() {
                            let _ = conn.send_control(&ControlMessage::UdpAudioPort(addr.port()));
                        }
                        Some(rx)
                    }
                    Err(_) => None,
                }
            })
        } else {
            None
        };

        while !peer_stop_c.load(Ordering::Acquire) && !manager_stop_c.load(Ordering::Acquire) {
            if let Some(ref mut rx) = udp_rx {
                let mut udp_write_failed = false;
                for _ in 0..16 {
                    match rx.recv_frame() {
                        Ok((udp_frame, _src)) => {
                            if matches!(
                                udp_frame.header.frame_type,
                                FrameType::AudioOpus | FrameType::AudioPcm
                            ) && !process_inbound_audio_frame(
                                &udp_frame,
                                &mut jb,
                                &mut *codec,
                                &mut *playback,
                                &mut conn,
                                &mut drift_ctrl,
                                &mut resampler,
                                &mut decoded_pcm,
                                &mut out_pcm,
                                &mut resampled_pcm,
                                frame_samples,
                                frame_ms_u16,
                                frame_dt_secs,
                                nominal_interval_us,
                                &mut audio_frames_received,
                                &mut decode_errors,
                                &mut last_audio_at,
                                &mut last_arrival_at,
                                &mut last_pace_at,
                                &mut prev_underruns,
                                &mut jitter_us,
                                &mut max_inter_arrival_gap_us,
                                &mut bitrate_ctrl,
                            ) {
                                udp_write_failed = true;
                                break;
                            }
                        }
                        Err(TransportError::Io(ref e))
                            if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                        {
                            break;
                        }
                        Err(_) => continue,
                    }
                }
                if udp_write_failed {
                    break;
                }
            }

            match conn.recv_frame() {
                Ok(frame) => {
                    if peer_stop_c.load(Ordering::Acquire) || manager_stop_c.load(Ordering::Acquire)
                    {
                        break;
                    }
                    match frame.header.frame_type {
                        FrameType::AudioOpus | FrameType::AudioPcm => {
                            if !process_inbound_audio_frame(
                                &frame,
                                &mut jb,
                                &mut *codec,
                                &mut *playback,
                                &mut conn,
                                &mut drift_ctrl,
                                &mut resampler,
                                &mut decoded_pcm,
                                &mut out_pcm,
                                &mut resampled_pcm,
                                frame_samples,
                                frame_ms_u16,
                                frame_dt_secs,
                                nominal_interval_us,
                                &mut audio_frames_received,
                                &mut decode_errors,
                                &mut last_audio_at,
                                &mut last_arrival_at,
                                &mut last_pace_at,
                                &mut prev_underruns,
                                &mut jitter_us,
                                &mut max_inter_arrival_gap_us,
                                &mut bitrate_ctrl,
                            ) {
                                break;
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
                                    | ControlMessage::DelayOffsetMs(_)
                                    | ControlMessage::UdpAudioPort(_) => {}
                                }
                            }
                            if !maybe_pace_inbound_plc(
                                &mut jb,
                                &mut *codec,
                                &mut *playback,
                                &mut conn,
                                &mut out_pcm,
                                &mut resampled_pcm,
                                &mut resampler,
                                drift_ctrl.ratio(),
                                audio_frames_received,
                                last_audio_at,
                                &mut last_pace_at,
                                plc_holdoff,
                                frame_interval,
                                &mut prev_underruns,
                                &mut bitrate_ctrl,
                            ) {
                                break;
                            }
                        }
                        FrameType::Ping => {
                            if let Ok(ping) = PingPayload::decode(&frame.payload) {
                                let stats = jb.stats();
                                let sink_ms_u64 = playback
                                    .buffered_ms()
                                    .filter(|ms| ms.is_finite() && *ms >= 0.0)
                                    .map_or(0u64, |ms| ms.round() as u64);
                                let jitter_buf_ms = u64::from(stats.buffered_ms)
                                    .saturating_add(sink_ms_u64)
                                    .min(u64::from(u16::MAX))
                                    as u16;
                                let dropped_frames = stats
                                    .dropped_overrun
                                    .saturating_add(playback.sink_overruns())
                                    .min(u64::from(u32::MAX))
                                    as u32;
                                let pong = PongPayload {
                                    client_time_us: ping.client_time_us,
                                    jitter_buf_ms,
                                    dropped_frames,
                                };
                                if conn.send_pong(&pong).is_err() {
                                    break;
                                }
                            }
                            if !maybe_pace_inbound_plc(
                                &mut jb,
                                &mut *codec,
                                &mut *playback,
                                &mut conn,
                                &mut out_pcm,
                                &mut resampled_pcm,
                                &mut resampler,
                                drift_ctrl.ratio(),
                                audio_frames_received,
                                last_audio_at,
                                &mut last_pace_at,
                                plc_holdoff,
                                frame_interval,
                                &mut prev_underruns,
                                &mut bitrate_ctrl,
                            ) {
                                break;
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
                    if !maybe_pace_inbound_plc(
                        &mut jb,
                        &mut *codec,
                        &mut *playback,
                        &mut conn,
                        &mut out_pcm,
                        &mut resampled_pcm,
                        &mut resampler,
                        drift_ctrl.ratio(),
                        audio_frames_received,
                        last_audio_at,
                        &mut last_pace_at,
                        plc_holdoff,
                        frame_interval,
                        &mut prev_underruns,
                        &mut bitrate_ctrl,
                    ) {
                        break;
                    }
                }
                Err(_) => break,
            }

            let diag = snapshot_inbound_diagnostics(
                &jb,
                &*playback,
                &drift_ctrl,
                audio_frames_received,
                decode_errors,
                jitter_us,
                max_inter_arrival_gap_us,
            );
            maybe_emit_inbound_diag_log(
                &peer_id_c,
                &diag,
                Instant::now(),
                &mut last_periodic_log_at,
                &mut last_anomaly_log_at,
                &mut prev_anomalies,
            );
            *diagnostics_c.lock().unwrap_or_else(|e| e.into_inner()) = diag;
        }

        // On clean client close (not revocation), flush any remaining buffered jitter frames.
        if clean_close
            && !peer_stop_c.load(Ordering::Acquire)
            && !manager_stop_c.load(Ordering::Acquire)
        {
            while jb.is_primed() && jb.stats().buffered_frames > 0 {
                out_pcm.clear();
                match jb.pop_with_codec(&mut *codec, &mut out_pcm) {
                    Ok(_) => {
                        if !write_inbound_pcm(
                            &mut *playback,
                            &out_pcm,
                            &mut resampler,
                            &mut resampled_pcm,
                            drift_ctrl.ratio(),
                        ) {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        } else if peer_stop_c.load(Ordering::Acquire) && !manager_stop_c.load(Ordering::Acquire) {
            let _ = conn.send_close();
            // Give loopback peers a brief window to read FrameType::Close before closing the socket.
            thread::sleep(Duration::from_millis(5));
        }

        let final_diag = snapshot_inbound_diagnostics(
            &jb,
            &*playback,
            &drift_ctrl,
            audio_frames_received,
            decode_errors,
            jitter_us,
            max_inter_arrival_gap_us,
        );
        *diagnostics_c.lock().unwrap_or_else(|e| e.into_inner()) = final_diag.clone();
        last_diag_c
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(peer_id_c.clone(), final_diag);

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
            diagnostics,
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
            for lag in 0..800usize {
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

    #[cfg(feature = "opus")]
    struct StallBurstCapture {
        frame_idx: usize,
        sample_idx: usize,
        stop_flag: Option<Arc<AtomicBool>>,
        mid_stream_hook: Option<Box<dyn FnMut(usize) + Send>>,
        stall_pred: Option<Box<dyn Fn() -> bool + Send>>,
    }

    #[cfg(feature = "opus")]
    impl StallBurstCapture {
        fn new() -> Self {
            Self {
                frame_idx: 0,
                sample_idx: 0,
                stop_flag: None,
                mid_stream_hook: None,
                stall_pred: None,
            }
        }

        fn with_hook(mut self, hook: impl FnMut(usize) + Send + 'static) -> Self {
            self.mid_stream_hook = Some(Box::new(hook));
            self
        }

        fn with_stall_until(mut self, pred: impl Fn() -> bool + Send + 'static) -> Self {
            self.stall_pred = Some(Box::new(pred));
            self
        }
    }

    #[cfg(feature = "opus")]
    impl PcmCapture for StallBurstCapture {
        fn attach_stop_flag(&mut self, stop_flag: Arc<AtomicBool>) {
            self.stop_flag = Some(stop_flag);
        }

        fn read_frame(&mut self, buf: &mut [i16]) -> TransportResult<usize> {
            if self
                .stop_flag
                .as_ref()
                .is_some_and(|f| f.load(Ordering::Acquire))
            {
                return Ok(0);
            }
            if let Some(ref mut hook) = self.mid_stream_hook {
                hook(self.frame_idx);
            }
            // Frames 0..8: normal 20ms pace.
            // Before frame 8: Wi-Fi stall (poll until PLC emitted if stall_pred set, else 200ms).
            // Frames 8..14: immediate burst (0ms delay).
            // Frames 14..24: normal 20ms pace.
            // Frame >= 24: EOF.
            if self.frame_idx >= 24 {
                return Ok(0);
            }
            if self.frame_idx < 8 || self.frame_idx >= 14 {
                thread::sleep(Duration::from_millis(20));
            }
            if self.frame_idx == 8 {
                if let Some(ref pred) = self.stall_pred {
                    let stall_start = Instant::now();
                    thread::sleep(Duration::from_millis(120));
                    while !pred() && stall_start.elapsed() < Duration::from_millis(450) {
                        thread::sleep(Duration::from_millis(5));
                    }
                } else {
                    thread::sleep(Duration::from_millis(200));
                }
            }
            let spc = buf.len() / 2;
            for i in 0..spc {
                let t = (self.sample_idx + i) as f64 / f64::from(PCM_SAMPLE_RATE);
                let s = ((2.0 * std::f64::consts::PI * 440.0 * t).sin() * 14_000.0) as i16;
                buf[i * 2] = s;
                buf[i * 2 + 1] = -s;
            }
            self.sample_idx += spc;
            self.frame_idx += 1;
            Ok(spc * 2)
        }
    }

    #[cfg(feature = "opus")]
    #[test]
    fn wifi_stall_triggers_opus_plc_and_recovers_without_latency_inflation() {
        let k_pair = [0xC1; 32];
        let trust = Arc::new(InMemoryTrustStore::new());
        trust.insert("stall-sender", k_pair);

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
                Sht3ServerConfig::new("stall-receiver"),
                trust,
                factory,
            )
            .unwrap();

        let (post_burst_tx, post_burst_rx) = mpsc::channel::<()>();
        let mut ping_sent_flag = false;
        let pb_for_stall = Arc::clone(&playback_handle);
        let capture = Box::new(
            StallBurstCapture::new()
                .with_hook(move |idx| {
                    if idx == 18 && !ping_sent_flag {
                        ping_sent_flag = true;
                        let _ = post_burst_tx.send(());
                    }
                })
                .with_stall_until(move || {
                    pb_for_stall
                        .lock()
                        .unwrap()
                        .as_ref()
                        .is_some_and(|h| h.frame_count() >= 9)
                }),
        );

        let out_cfg = OutboundConfig::new("stall-sender", addr, k_pair)
            .with_profile(AudioProfile::OpusBalanced)
            .with_jitter_ms(40);
        let session = manager.start_outbound(out_cfg, capture).unwrap();

        // Trigger a telemetry ping during post-burst recovery to verify jitter depth stayed bounded.
        assert!(post_burst_rx.recv_timeout(Duration::from_secs(3)).is_ok());
        session.request_ping().unwrap();
        assert!(wait_until(Duration::from_secs(1), || {
            session.telemetry().pongs_received >= 1
        }));
        let mid_telemetry = session.telemetry();
        assert!(
            mid_telemetry.jitter_buf_ms <= 40,
            "jitter buffer permanently inflated after burst: {} ms",
            mid_telemetry.jitter_buf_ms
        );

        session.join();
        let rx_handle = playback_handle.lock().unwrap().clone().unwrap();
        assert!(wait_until(Duration::from_secs(2), || rx_handle.is_closed()));

        assert_eq!(session.telemetry().frames_sent, 24);
        assert!(
            rx_handle.frame_count() > 24,
            "expected PLC frames during Wi-Fi stall, got {} frames",
            rx_handle.frame_count()
        );
        let frames = rx_handle.frames();
        assert!(
            !crate::transport::codec::is_digital_silence(&frames[8], 10),
            "PLC frame emitted during stall was digital silence"
        );
    }

    #[cfg(feature = "opus")]
    #[test]
    fn live_server_restart_mid_stream_seamlessly_reconnects_and_resumes() {
        let k_pair = [0xC2; 32];
        let trust = Arc::new(InMemoryTrustStore::new());
        trust.insert("reconnect-sender", k_pair);

        let rx1_slot: Arc<Mutex<Option<MockPcmPlaybackHandle>>> = Arc::new(Mutex::new(None));
        let rx1_c = Arc::clone(&rx1_slot);
        let factory1: PcmPlaybackFactory = Arc::new(move |_peer_id| {
            let (play, handle) = MockPcmPlayback::pair();
            *rx1_c.lock().unwrap() = Some(handle);
            Ok(Box::new(play))
        });

        let manager1 = Sht3SessionManager::new();
        let addr = manager1
            .start_listener(
                "127.0.0.1:0".parse().unwrap(),
                Sht3ServerConfig::new("restart-receiver"),
                Arc::clone(&trust) as Arc<dyn TrustLookup>,
                factory1,
            )
            .unwrap();

        let stop_cap = Arc::new(AtomicBool::new(false));
        let capture = Box::new(
            MockPcmCapture::sine_wave(440.0, None)
                .with_pace(Duration::from_millis(10))
                .with_stop_flag(Arc::clone(&stop_cap)),
        );
        let out_cfg = OutboundConfig::new("reconnect-sender", addr, k_pair)
            .with_profile(AudioProfile::OpusBalanced)
            .with_jitter_ms(40)
            .with_reconnect_window(Duration::from_secs(3));

        let session = manager1.start_outbound(out_cfg, capture).unwrap();
        assert!(wait_until(Duration::from_secs(2), || {
            rx1_slot
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|h| h.frame_count() >= 4)
        }));

        session.set_volume(67).unwrap();
        session.set_mute(true).unwrap();
        let rx1 = rx1_slot.lock().unwrap().clone().unwrap();
        assert!(wait_until(Duration::from_secs(2), || {
            rx1.volume() == 67 && rx1.is_muted()
        }));

        // Drop listener (simulating abrupt server restart without FrameType::Close).
        drop(manager1);
        thread::sleep(Duration::from_millis(120));
        assert!(
            session.is_running(),
            "outbound session dropped prematurely inside reconnect_window"
        );

        // Restart listener on the exact same port.
        let rx2_slot: Arc<Mutex<Option<MockPcmPlaybackHandle>>> = Arc::new(Mutex::new(None));
        let rx2_c = Arc::clone(&rx2_slot);
        let factory2: PcmPlaybackFactory = Arc::new(move |_peer_id| {
            let (play, handle) = MockPcmPlayback::pair();
            *rx2_c.lock().unwrap() = Some(handle);
            Ok(Box::new(play))
        });

        let manager2 = Sht3SessionManager::new();
        manager2
            .start_listener(
                addr,
                Sht3ServerConfig::new("restart-receiver"),
                Arc::clone(&trust) as Arc<dyn TrustLookup>,
                factory2,
            )
            .unwrap();

        assert!(wait_until(Duration::from_secs(3), || {
            manager2.inbound_peer_count() == 1
                && rx2_slot
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|h| h.frame_count() >= 6)
        }));
        assert!(session.is_running());

        let rx2 = rx2_slot.lock().unwrap().clone().unwrap();
        assert!(wait_until(Duration::from_secs(2), || {
            rx2.volume() == 67 && rx2.is_muted()
        }));
        assert!(
            rx2.total_energy() > 100_000,
            "resumed Opus stream after reconnect had unexpectedly low energy"
        );

        stop_cap.store(true, Ordering::Release);
        session.stop();
    }

    #[test]
    fn revoked_or_explicitly_closed_peer_stops_immediately_despite_reconnect_window() {
        // Case A: explicit disconnect_peer (FrameType::Close) terminates immediately even while trusted.
        let k_pair = [0xC3; 32];
        let trust = Arc::new(InMemoryTrustStore::new());
        trust.insert("close-peer", k_pair);

        let rx_slot: Arc<Mutex<Option<MockPcmPlaybackHandle>>> = Arc::new(Mutex::new(None));
        let rx_c = Arc::clone(&rx_slot);
        let factory: PcmPlaybackFactory = Arc::new(move |_peer_id| {
            let (play, handle) = MockPcmPlayback::pair();
            *rx_c.lock().unwrap() = Some(handle);
            Ok(Box::new(play))
        });

        let manager = Sht3SessionManager::new();
        let addr = manager
            .start_listener(
                "127.0.0.1:0".parse().unwrap(),
                Sht3ServerConfig::new("strict-srv"),
                Arc::clone(&trust) as Arc<dyn TrustLookup>,
                factory.clone(),
            )
            .unwrap();

        let stop_cap_a = Arc::new(AtomicBool::new(false));
        let cap_a = Box::new(
            MockPcmCapture::sine_wave(440.0, None)
                .with_pace(Duration::from_millis(10))
                .with_stop_flag(Arc::clone(&stop_cap_a)),
        );
        let cfg_a = OutboundConfig::new("close-peer", addr, k_pair)
            .with_profile(AudioProfile::PcmLossless)
            .with_reconnect_window(Duration::from_secs(5));
        let session_a = manager.start_outbound(cfg_a, cap_a).unwrap();

        assert!(wait_until(Duration::from_secs(2), || {
            rx_slot
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|h| h.frame_count() >= 2)
        }));

        let close_start = Instant::now();
        assert!(manager.disconnect_peer("close-peer"));
        assert!(wait_until(Duration::from_secs(2), || !session_a.is_running()));
        assert!(
            close_start.elapsed() < Duration::from_secs(2),
            "FrameType::Close should abort immediately without waiting 5s reconnect_window, took {:?}",
            close_start.elapsed()
        );
        assert_eq!(manager.inbound_peer_count(), 0);
        stop_cap_a.store(true, Ordering::Release);

        // Case B: revoked peer hits AuthRejected on reconnect attempt and aborts immediately.
        trust.insert("revoked-midstream", k_pair);
        *rx_slot.lock().unwrap() = None;
        let stop_cap_b = Arc::new(AtomicBool::new(false));
        let cap_b = Box::new(
            MockPcmCapture::sine_wave(440.0, None)
                .with_pace(Duration::from_millis(10))
                .with_stop_flag(Arc::clone(&stop_cap_b)),
        );
        let cfg_b = OutboundConfig::new("revoked-midstream", addr, k_pair)
            .with_profile(AudioProfile::PcmLossless)
            .with_reconnect_window(Duration::from_secs(5));
        let session_b = manager.start_outbound(cfg_b, cap_b).unwrap();
        assert!(wait_until(Duration::from_secs(2), || {
            rx_slot
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|h| h.frame_count() >= 2)
        }));

        trust.remove("revoked-midstream");
        manager.stop_listener();
        manager
            .start_listener(
                addr,
                Sht3ServerConfig::new("strict-srv"),
                Arc::clone(&trust) as Arc<dyn TrustLookup>,
                factory,
            )
            .unwrap();

        let revoke_start = Instant::now();
        assert!(wait_until(Duration::from_millis(2500), || {
            !session_b.is_running()
        }));
        assert!(
            revoke_start.elapsed() < Duration::from_millis(2500),
            "AuthRejected should abort immediately without waiting 5s reconnect_window, took {:?}",
            revoke_start.elapsed()
        );
        stop_cap_b.store(true, Ordering::Release);
    }

    #[test]
    fn stalled_half_open_socket_does_not_block_legitimate_peer() {
        use std::io::Write;

        let k_pair = [0xC4; 32];
        let trust = Arc::new(InMemoryTrustStore::new());
        trust.insert("legit-peer", k_pair);

        let rx_slot: Arc<Mutex<Option<MockPcmPlaybackHandle>>> = Arc::new(Mutex::new(None));
        let rx_c = Arc::clone(&rx_slot);
        let factory: PcmPlaybackFactory = Arc::new(move |_peer_id| {
            let (play, handle) = MockPcmPlayback::pair();
            *rx_c.lock().unwrap() = Some(handle);
            Ok(Box::new(play))
        });

        let manager = Sht3SessionManager::new();
        let addr = manager
            .start_listener(
                "127.0.0.1:0".parse().unwrap(),
                Sht3ServerConfig::new("concurrent-hs-server"),
                trust,
                factory,
            )
            .unwrap();

        // Open 3 stalled half-open sockets that send partial magic and never finish ClientHello.
        let mut stalled_sockets = Vec::new();
        for _ in 0..3 {
            let mut s = TcpStream::connect(addr).unwrap();
            s.write_all(b"SHT3").unwrap();
            s.flush().unwrap();
            stalled_sockets.push(s);
        }
        thread::sleep(Duration::from_millis(30));

        // Legitimate peer must connect, handshake, and finish streaming in < 1500ms.
        let start = Instant::now();
        let capture = Box::new(MockPcmCapture::sine_wave(440.0, Some(5)));
        let cfg =
            OutboundConfig::new("legit-peer", addr, k_pair).with_profile(AudioProfile::PcmLossless);
        let session = manager.start_outbound(cfg, capture).unwrap();
        session.join();

        let rx = rx_slot.lock().unwrap().clone().unwrap();
        assert!(wait_until(Duration::from_millis(1200), || rx.is_closed()));
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(1500),
            "stalled sockets delayed legitimate peer: {:?}",
            elapsed
        );
        assert_eq!(rx.frame_count(), 5);

        drop(stalled_sockets);
    }

    #[cfg(feature = "opus")]
    struct MultiToneCapture {
        sample_idx: usize,
        pace: Duration,
        stop_flag: Option<Arc<AtomicBool>>,
        discontinuity: Arc<AtomicBool>,
    }

    #[cfg(feature = "opus")]
    impl MultiToneCapture {
        fn new(pace: Duration, discontinuity: Arc<AtomicBool>) -> Self {
            Self {
                sample_idx: 0,
                pace,
                stop_flag: None,
                discontinuity,
            }
        }
    }

    #[cfg(feature = "opus")]
    impl PcmCapture for MultiToneCapture {
        fn attach_stop_flag(&mut self, stop_flag: Arc<AtomicBool>) {
            self.stop_flag = Some(stop_flag);
        }

        fn take_discontinuity(&mut self) -> bool {
            self.discontinuity.swap(false, Ordering::AcqRel)
        }

        fn read_frame(&mut self, buf: &mut [i16]) -> TransportResult<usize> {
            if self
                .stop_flag
                .as_ref()
                .is_some_and(|f| f.load(Ordering::Acquire))
            {
                return Ok(0);
            }
            thread::sleep(self.pace);
            if self
                .stop_flag
                .as_ref()
                .is_some_and(|f| f.load(Ordering::Acquire))
            {
                return Ok(0);
            }
            let spc = buf.len() / 2;
            for i in 0..spc {
                let t = (self.sample_idx + i) as f64 / f64::from(PCM_SAMPLE_RATE);
                let l = (8000.0 * (2.0 * std::f64::consts::PI * 110.0 * t).sin()
                    + 6000.0 * (2.0 * std::f64::consts::PI * 660.0 * t).sin()
                    + 3500.0 * (2.0 * std::f64::consts::PI * 2640.0 * t).cos()
                    + 2500.0 * (2.0 * std::f64::consts::PI * 4186.0 * t).sin())
                .clamp(-32000.0, 32000.0) as i16;
                let r = (8000.0 * (2.0 * std::f64::consts::PI * 147.0 * t).sin()
                    + 5500.0 * (2.0 * std::f64::consts::PI * 880.0 * t).cos()
                    + 3000.0 * (2.0 * std::f64::consts::PI * 3520.0 * t).sin())
                .clamp(-32000.0, 32000.0) as i16;
                buf[i * 2] = l;
                buf[i * 2 + 1] = r;
            }
            self.sample_idx += spc;
            Ok(spc * 2)
        }
    }

    #[cfg(feature = "opus")]
    #[test]
    fn dynamic_bitrate_hint_lowers_and_restores_opus_payload_rate() {
        let k_pair = [0xC5; 32];
        let trust = Arc::new(InMemoryTrustStore::new());
        trust.insert("bitrate-sender", k_pair);

        let listener = Sht3Listener::bind(
            "127.0.0.1:0".parse().unwrap(),
            Sht3ServerConfig::new("bitrate-receiver"),
            trust,
        )
        .unwrap();
        let addr = listener.local_addr().unwrap();

        let saw_discontinuity = Arc::new(AtomicBool::new(false));
        let saw_disc_c = Arc::clone(&saw_discontinuity);

        let server = thread::spawn(move || {
            let mut conn = listener.accept().unwrap();

            fn next_audio_len(conn: &mut Sht3ServerConnection, saw_disc: &AtomicBool) -> usize {
                loop {
                    let frame = conn.recv_frame().unwrap();
                    if frame.header.frame_type == FrameType::AudioOpus {
                        if frame.header.flags & FLAG_DISCONTINUITY != 0 {
                            saw_disc.store(true, Ordering::Release);
                        }
                        return frame.payload.len();
                    }
                }
            }

            // Warmup & baseline @ 128 kbps.
            for _ in 0..5 {
                let _ = next_audio_len(&mut conn, &saw_disc_c);
            }
            let baseline_sum: usize = (0..15)
                .map(|_| next_audio_len(&mut conn, &saw_disc_c))
                .sum();

            // Lower bitrate to 32 kbps via ControlMessage::BitrateHint.
            conn.send_control(&ControlMessage::BitrateHint(32)).unwrap();
            for _ in 0..10 {
                let _ = next_audio_len(&mut conn, &saw_disc_c);
            }
            let low_sum: usize = (0..15)
                .map(|_| next_audio_len(&mut conn, &saw_disc_c))
                .sum();

            // Restore bitrate back to 128 kbps.
            conn.send_control(&ControlMessage::BitrateHint(128))
                .unwrap();
            for _ in 0..10 {
                let _ = next_audio_len(&mut conn, &saw_disc_c);
            }
            let restored_sum: usize = (0..15)
                .map(|_| next_audio_len(&mut conn, &saw_disc_c))
                .sum();

            (baseline_sum, low_sum, restored_sum)
        });

        let disc_flag = Arc::new(AtomicBool::new(true));
        let capture = Box::new(MultiToneCapture::new(
            Duration::from_millis(5),
            Arc::clone(&disc_flag),
        ));
        let cfg = OutboundConfig::new("bitrate-sender", addr, k_pair)
            .with_profile(AudioProfile::OpusBalanced);
        let session = OutboundSessionHandle::start(cfg, capture).unwrap();

        let (baseline_sum, low_sum, restored_sum) = server.join().unwrap();
        session.stop();

        assert!(
            saw_discontinuity.load(Ordering::Acquire),
            "expected FLAG_DISCONTINUITY on frame after capture discontinuity"
        );
        assert!(
            low_sum * 2 < baseline_sum,
            "BitrateHint(32) did not substantially lower payload bytes: baseline={baseline_sum}, low={low_sum}"
        );
        assert!(
            restored_sum > low_sum * 2,
            "BitrateHint(128) did not restore payload bytes: low={low_sum}, restored={restored_sum}"
        );
    }

    #[test]
    fn bitrate_adaptation_steps_down_on_burst_and_recovers_after_clean_window() {
        let mut ctrl = BitrateAdaptation::new(128);
        let t0 = Instant::now();

        assert_eq!(ctrl.on_underruns(t0, 1), None);
        assert_eq!(ctrl.on_underruns(t0 + Duration::from_millis(50), 1), None);
        assert_eq!(
            ctrl.on_underruns(t0 + Duration::from_millis(100), 1),
            Some(96)
        );
        assert_eq!(
            ctrl.on_underruns(t0 + Duration::from_millis(300), 3),
            Some(64)
        );
        assert_eq!(
            ctrl.on_underruns(t0 + Duration::from_millis(500), 3),
            Some(48)
        );
        assert_eq!(
            ctrl.on_underruns(t0 + Duration::from_millis(700), 3),
            Some(32)
        );
        // Floor at 32 kbps.
        assert_eq!(ctrl.on_underruns(t0 + Duration::from_millis(900), 3), None);

        // Must not step up before 15s of clean delivery.
        let t_last_err = t0 + Duration::from_millis(900);
        assert_eq!(
            ctrl.on_clean_frame(t_last_err + Duration::from_secs(10)),
            None
        );

        // Steps up 32 -> 48 -> 64 -> 96 -> 128 across 15s clean intervals.
        assert_eq!(
            ctrl.on_clean_frame(t_last_err + Duration::from_secs(15)),
            Some(48)
        );
        assert_eq!(
            ctrl.on_clean_frame(t_last_err + Duration::from_secs(30)),
            Some(64)
        );
        assert_eq!(
            ctrl.on_clean_frame(t_last_err + Duration::from_secs(45)),
            Some(96)
        );
        assert_eq!(
            ctrl.on_clean_frame(t_last_err + Duration::from_secs(60)),
            Some(128)
        );
        assert_eq!(
            ctrl.on_clean_frame(t_last_err + Duration::from_secs(75)),
            None
        );
    }

    struct VirtualClockedPlayback {
        inner: ClockedMockPcmPlayback,
        nominal_frames_per_tick: usize,
    }

    impl VirtualClockedPlayback {
        fn new(inner: ClockedMockPcmPlayback, nominal_frames_per_tick: usize) -> Self {
            Self {
                inner,
                nominal_frames_per_tick,
            }
        }
    }

    impl PcmPlayback for VirtualClockedPlayback {
        fn write_frame(&mut self, samples: &[i16]) -> TransportResult<()> {
            self.inner
                .drain_samples_for_frames(self.nominal_frames_per_tick);
            self.inner.write_frame(samples)
        }

        fn set_volume(&self, pct: u8) {
            self.inner.set_volume(pct);
        }

        fn set_mute(&self, muted: bool) {
            self.inner.set_mute(muted);
        }

        fn volume(&self) -> u8 {
            self.inner.volume()
        }

        fn is_muted(&self) -> bool {
            self.inner.is_muted()
        }

        fn buffered_ms(&self) -> Option<f64> {
            self.inner.buffered_ms()
        }

        fn sink_target_ms(&self) -> f64 {
            self.inner.sink_target_ms()
        }

        fn sink_underruns(&self) -> u64 {
            self.inner.sink_underruns()
        }

        fn sink_overruns(&self) -> u64 {
            self.inner.sink_overruns()
        }
    }

    struct StereoToneCapture {
        sample_idx: usize,
        total_frames: usize,
        frames_emitted: usize,
        freq_l_hz: f64,
        freq_r_hz: f64,
        ping_after_frame: Option<usize>,
        ping_tx: Option<mpsc::Sender<()>>,
    }

    impl PcmCapture for StereoToneCapture {
        fn read_frame(&mut self, buf: &mut [i16]) -> TransportResult<usize> {
            if self.frames_emitted >= self.total_frames {
                return Ok(0);
            }
            if let Some(trigger) = self.ping_after_frame {
                if self.frames_emitted == trigger {
                    if let Some(tx) = self.ping_tx.take() {
                        let _ = tx.send(());
                    }
                }
            }
            let spc = buf.len() / 2;
            for i in 0..spc {
                let t = (self.sample_idx + i) as f64 / f64::from(PCM_SAMPLE_RATE);
                buf[i * 2] =
                    ((2.0 * std::f64::consts::PI * self.freq_l_hz * t).sin() * 12_000.0) as i16;
                buf[i * 2 + 1] =
                    ((2.0 * std::f64::consts::PI * self.freq_r_hz * t).sin() * 12_000.0) as i16;
            }
            self.sample_idx += spc;
            self.frames_emitted += 1;
            Ok(spc * 2)
        }
    }

    fn quadrature_tone_metrics(
        samples: &[i16],
        channel_offset: usize,
        freq_hz: f64,
        effective_sample_rate_hz: f64,
    ) -> (f64, f64) {
        let ch: Vec<f64> = samples
            .iter()
            .skip(channel_offset)
            .step_by(2)
            .map(|&s| f64::from(s))
            .collect();
        let end = ch.len().saturating_sub(960);
        let start = end.saturating_sub(48_000);
        let slice = &ch[start..end];
        let omega = 2.0 * std::f64::consts::PI * freq_hz / effective_sample_rate_hz;

        let mut sc = 0.0f64;
        let mut ss = 0.0f64;
        let mut cc = 0.0f64;
        let mut ss_norm = 0.0f64;
        let mut cs = 0.0f64;
        for (i, &y) in slice.iter().enumerate() {
            let angle = omega * (i as f64);
            let c = angle.cos();
            let s = angle.sin();
            sc += y * c;
            ss += y * s;
            cc += c * c;
            ss_norm += s * s;
            cs += c * s;
        }
        let det = cc * ss_norm - cs * cs;
        let a = (sc * ss_norm - ss * cs) / det;
        let b = (ss * cc - sc * cs) / det;

        let mut sig_pow = 0.0f64;
        let mut err_pow = 0.0f64;
        let mut dot = 0.0f64;
        let mut y_pow = 0.0f64;
        for (i, &y) in slice.iter().enumerate() {
            let angle = omega * (i as f64);
            let fit = a * angle.cos() + b * angle.sin();
            let err = y - fit;
            sig_pow += fit * fit;
            err_pow += err * err;
            dot += y * fit;
            y_pow += y * y;
        }
        let corr = dot / ((y_pow.sqrt() * sig_pow.sqrt()) + 1e-12);
        let snr_db = 10.0 * (sig_pow / (err_pow + 1e-12)).log10();
        (corr, snr_db)
    }

    #[test]
    fn hypothesis_1_drift_reproduction_and_closed_loop_compensation() {
        // Part 1: Fixed-rate (ratio = 1.0) into ClockedMockPcmPlayback drifts into underruns/overruns.
        for &skew_ppm in &[500.0_f64, -500.0_f64] {
            let dac_rate_hz = f64::from(PCM_SAMPLE_RATE) / (1.0 + skew_ppm * 1e-6);
            let (pb, handle) = ClockedMockPcmPlayback::pair(dac_rate_hz);
            let mut vpb = VirtualClockedPlayback::new(pb.with_record_output(false), 960);
            let frame = vec![4_000i16; PCM_FRAME_SAMPLES_20MS];
            for _ in 0..14_000 {
                vpb.write_frame(&frame).unwrap();
            }
            if skew_ppm > 0.0 {
                assert!(
                    handle.overruns() > 0,
                    "expected fixed-rate overrun at +{skew_ppm} ppm"
                );
            } else {
                assert!(
                    handle.underruns() > 0,
                    "expected fixed-rate underrun at {skew_ppm} ppm"
                );
            }
        }

        // Part 2: Closed-loop Sht3SessionManager with DriftController + AdaptiveResampler
        // locks buffer fill cleanly across +/-200 ppm for PCM (and Opus when enabled).
        let profiles = [
            (AudioProfile::PcmLossless, 20_000usize, 45.0_f64),
            #[cfg(feature = "opus")]
            (AudioProfile::OpusBalanced, 15_000usize, 20.0_f64),
        ];

        for (profile, total_frames, min_snr_db) in profiles {
            for &skew_ppm in &[200.0_f64, -200.0_f64] {
                let skew = skew_ppm * 1e-6;
                let dac_rate_hz = f64::from(PCM_SAMPLE_RATE) / (1.0 + skew);
                let spc = profile.frame_samples_per_channel();

                let k_pair = [0xD1; 32];
                let trust = Arc::new(InMemoryTrustStore::new());
                let peer_id = "h1-sender";
                trust.insert(peer_id, k_pair);

                let sink_handle: Arc<Mutex<Option<ClockedMockPcmPlaybackHandle>>> =
                    Arc::new(Mutex::new(None));
                let sink_handle_c = Arc::clone(&sink_handle);
                let factory: PcmPlaybackFactory = Arc::new(move |_peer| {
                    let (pb, h) = ClockedMockPcmPlayback::pair(dac_rate_hz);
                    *sink_handle_c.lock().unwrap() = Some(h);
                    Ok(Box::new(VirtualClockedPlayback::new(pb, spc)))
                });

                let manager = Sht3SessionManager::new();
                let addr = manager
                    .start_listener(
                        "127.0.0.1:0".parse().unwrap(),
                        Sht3ServerConfig::new("h1-receiver"),
                        trust,
                        factory,
                    )
                    .unwrap();

                let (ping_tx, ping_rx) = mpsc::channel();
                let capture = Box::new(StereoToneCapture {
                    sample_idx: 0,
                    total_frames,
                    frames_emitted: 0,
                    freq_l_hz: 440.0,
                    freq_r_hz: 660.0,
                    ping_after_frame: Some(total_frames - 50),
                    ping_tx: Some(ping_tx),
                });
                let cfg = OutboundConfig::new(peer_id, addr, k_pair)
                    .with_profile(profile)
                    .with_jitter_ms(40)
                    .with_ping_interval(Duration::from_millis(25));
                let session = manager.start_outbound(cfg, capture).unwrap();
                let _ = ping_rx.recv_timeout(Duration::from_secs(10));
                let _ = session.request_ping();
                session.join();

                let h = sink_handle.lock().unwrap().clone().unwrap();
                assert!(wait_until(Duration::from_secs(2), || h.is_closed()));

                assert_eq!(
                    h.underruns(),
                    0,
                    "{profile:?} @ {skew_ppm} ppm had sink underruns"
                );
                assert_eq!(
                    h.overruns(),
                    0,
                    "{profile:?} @ {skew_ppm} ppm had sink overruns"
                );
                assert!(
                    (h.buffered_ms() - h.target_ms()).abs() < 2.5,
                    "{profile:?} @ {skew_ppm} ppm final sink fill {:.2} ms missed target {:.2} ms",
                    h.buffered_ms(),
                    h.target_ms()
                );

                let diag = manager
                    .inbound_peer_diagnostics(peer_id)
                    .expect("missing inbound diagnostics");
                assert_eq!(diag.audio_frames_received, total_frames as u64);
                assert_eq!(diag.decode_errors, 0);
                assert!(
                    (diag.drift_ppm - skew_ppm).abs() < 25.0,
                    "{profile:?} @ {skew_ppm} ppm converged to {:.2} ppm",
                    diag.drift_ppm
                );

                let out = h.output_samples();
                let eff_rate = f64::from(PCM_SAMPLE_RATE) / diag.drift_ratio;
                let (corr_l, snr_l) = quadrature_tone_metrics(&out, 0, 440.0, eff_rate);
                let (corr_r, snr_r) = quadrature_tone_metrics(&out, 1, 660.0, eff_rate);
                assert!(
                    corr_l > 0.99 && snr_l > min_snr_db,
                    "{profile:?} @ {skew_ppm} ppm L fidelity low: corr={corr_l:.5}, snr={snr_l:.2} dB"
                );
                assert!(
                    corr_r > 0.99 && snr_r > min_snr_db,
                    "{profile:?} @ {skew_ppm} ppm R fidelity low: corr={corr_r:.5}, snr={snr_r:.2} dB"
                );
            }
        }
    }

    #[cfg(feature = "opus")]
    #[test]
    fn hypothesis_2_stall_burst_with_clocked_sink_triggers_plc_and_records_jitter_spike() {
        let k_pair = [0xD2; 32];
        let trust = Arc::new(InMemoryTrustStore::new());
        let peer_id = "h2-stall-sender";
        trust.insert(peer_id, k_pair);

        let sink_handle: Arc<Mutex<Option<ClockedMockPcmPlaybackHandle>>> =
            Arc::new(Mutex::new(None));
        let sink_handle_c = Arc::clone(&sink_handle);
        let factory: PcmPlaybackFactory = Arc::new(move |_peer| {
            let (pb, h) = ClockedMockPcmPlayback::pair(f64::from(PCM_SAMPLE_RATE));
            *sink_handle_c.lock().unwrap() = Some(h);
            Ok(Box::new(pb.with_realtime_clock()))
        });

        let manager = Sht3SessionManager::new();
        let addr = manager
            .start_listener(
                "127.0.0.1:0".parse().unwrap(),
                Sht3ServerConfig::new("h2-receiver"),
                trust,
                factory,
            )
            .unwrap();

        let (post_burst_tx, post_burst_rx) = mpsc::channel::<()>();
        let mut ping_sent = false;
        let capture = Box::new(StallBurstCapture::new().with_hook(move |idx| {
            if idx == 18 && !ping_sent {
                ping_sent = true;
                let _ = post_burst_tx.send(());
            }
        }));

        let cfg = OutboundConfig::new(peer_id, addr, k_pair)
            .with_profile(AudioProfile::OpusBalanced)
            .with_jitter_ms(40);
        let session = manager.start_outbound(cfg, capture).unwrap();

        assert!(post_burst_rx.recv_timeout(Duration::from_secs(3)).is_ok());
        session.request_ping().unwrap();
        assert!(wait_until(Duration::from_secs(1), || {
            session.telemetry().pongs_received >= 1
        }));
        let mid_telemetry = session.telemetry();
        // Total buffered_ms = jb_ms (~20-40 after cushion bump) + sink_ms (~40-80) <= 140 ms
        // (well below 200 ms stall + burst inflation).
        assert!(
            mid_telemetry.jitter_buf_ms <= 140,
            "total jitter+sink latency inflated after burst: {} ms",
            mid_telemetry.jitter_buf_ms
        );

        session.join();
        let h = sink_handle.lock().unwrap().clone().unwrap();
        assert!(wait_until(Duration::from_secs(2), || h.is_closed()));
        assert_eq!(h.overruns(), 0, "clocked sink overflowed on burst");

        let diag = manager
            .inbound_peer_diagnostics(peer_id)
            .expect("missing inbound diagnostics");
        assert!(
            diag.jb_buffered_ms <= 40,
            "jitter buffer stayed inflated after recovery: {} ms",
            diag.jb_buffered_ms
        );
        assert!(
            diag.sink_buffered_ms.is_some_and(|ms| ms <= 90.0),
            "clocked sink fill stayed inflated: {:?}",
            diag.sink_buffered_ms
        );
        assert!(
            diag.jb_plc_frames > 0,
            "expected Opus PLC frames during 200ms stall"
        );
        assert!(
            diag.max_inter_arrival_gap_us >= 150_000,
            "expected >=150ms arrival gap spike, got {} us",
            diag.max_inter_arrival_gap_us
        );
        assert!(
            diag.jitter_ms > 5.0,
            "expected RFC 3550 jitter spike > 5ms, got {:.2} ms",
            diag.jitter_ms
        );
    }

    #[test]
    fn hypothesis_3_decode_error_triggers_plc_fallback_without_dropping_cadence() {
        let profiles = [
            (AudioProfile::PcmLossless, vec![0xAAu8, 0xBBu8]),
            #[cfg(feature = "opus")]
            (AudioProfile::OpusBalanced, vec![0x03u8]),
        ];

        for (profile, bad_payload) in profiles {
            let k_pair = [0xD3; 32];
            let trust = Arc::new(InMemoryTrustStore::new());
            let peer_id = "h3-corrupt-sender";
            trust.insert(peer_id, k_pair);

            let rx_handle_slot: Arc<Mutex<Option<MockPcmPlaybackHandle>>> =
                Arc::new(Mutex::new(None));
            let rx_c = Arc::clone(&rx_handle_slot);
            let factory: PcmPlaybackFactory = Arc::new(move |_peer| {
                let (play, handle) = MockPcmPlayback::pair();
                *rx_c.lock().unwrap() = Some(handle);
                Ok(Box::new(play))
            });

            let manager = Sht3SessionManager::new();
            let addr = manager
                .start_listener(
                    "127.0.0.1:0".parse().unwrap(),
                    Sht3ServerConfig::new("h3-receiver"),
                    trust,
                    factory,
                )
                .unwrap();

            let params = profile.to_audio_params(40);
            let mut client = Sht3ClientConnection::connect(
                addr,
                Sht3ClientConfig::new(peer_id, params),
                &k_pair,
            )
            .unwrap();
            let mut encoder = create_codec(&params).unwrap();
            let spc = profile.frame_samples_per_channel();
            let frame_type = profile.frame_type();

            let total_frames = 12usize;
            let mut encoded = Vec::new();
            let mut pcm = vec![0i16; spc * 2];
            for f_idx in 0..total_frames {
                let ts = (f_idx * spc) as u32;
                if f_idx == 5 {
                    client.send_audio(frame_type, 0, ts, &bad_payload).unwrap();
                } else {
                    for i in 0..spc {
                        let t = (f_idx * spc + i) as f64 / f64::from(PCM_SAMPLE_RATE);
                        let s = ((2.0 * std::f64::consts::PI * 440.0 * t).sin() * 14_000.0) as i16;
                        pcm[i * 2] = s;
                        pcm[i * 2 + 1] = -s;
                    }
                    encoded.clear();
                    encoder.encode(&pcm, &mut encoded).unwrap();
                    client.send_audio(frame_type, 0, ts, &encoded).unwrap();
                }
            }
            client.send_close().unwrap();

            let rx_handle = wait_until(Duration::from_secs(2), || {
                rx_handle_slot
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|h| h.is_closed())
            });
            assert!(rx_handle, "{profile:?} receiver did not close cleanly");

            let handle = rx_handle_slot.lock().unwrap().clone().unwrap();
            assert_eq!(
                handle.frame_count(),
                total_frames,
                "{profile:?} lost frame cadence on decode error"
            );

            let diag = manager
                .inbound_peer_diagnostics(peer_id)
                .expect("missing inbound diagnostics");
            assert_eq!(diag.decode_errors, 1);
            assert_eq!(diag.audio_frames_received, total_frames as u64);

            if profile.codec_id() == CodecId::Opus {
                let frames = handle.frames();
                assert!(
                    !crate::transport::codec::is_digital_silence(&frames[5], 10),
                    "expected non-silent Opus PLC fallback on corrupted slot 5"
                );
            }
        }
    }

    #[cfg(feature = "opus")]
    #[test]
    fn end_to_end_udp_audio_data_plane_with_tcp_control_and_fallback() {
        let k_pair = [0x91; 32];
        let trust = Arc::new(InMemoryTrustStore::new());
        trust.insert("udp-sender", k_pair);

        // 1. UDP-enabled server + prefer_udp=true + 5ms OpusUltraLowLatency
        let rx_handle_slot: Arc<Mutex<Option<MockPcmPlaybackHandle>>> = Arc::new(Mutex::new(None));
        let slot_c = Arc::clone(&rx_handle_slot);
        let factory: PcmPlaybackFactory = Arc::new(move |_peer_id| {
            let (play, handle) = MockPcmPlayback::pair();
            *slot_c.lock().unwrap() = Some(handle);
            Ok(Box::new(play))
        });

        let manager = Sht3SessionManager::new();
        let addr = manager
            .start_listener(
                "127.0.0.1:0".parse().unwrap(),
                Sht3ServerConfig::new("udp-receiver"),
                trust.clone(),
                factory,
            )
            .unwrap();

        let spc = AudioProfile::OpusUltraLowLatency.frame_samples_per_channel();
        assert_eq!(spc, 240);
        let mut frames = Vec::new();
        for f in 0..30usize {
            let mut pcm = Vec::with_capacity(spc * 2);
            for i in 0..spc {
                let t = (f * spc + i) as f64 / 48_000.0;
                let s = ((2.0 * std::f64::consts::PI * 440.0 * t).sin() * 12_000.0) as i16;
                pcm.push(s);
                pcm.push(s);
            }
            frames.push(pcm);
        }

        let cfg = OutboundConfig::new("udp-sender", addr, k_pair)
            .with_profile(AudioProfile::OpusUltraLowLatency)
            .with_jitter_ms(15)
            .with_prefer_udp(true);
        let session = manager
            .start_outbound(
                cfg,
                Box::new(MockPcmCapture::from_frames(frames).with_pace(Duration::from_millis(5))),
            )
            .unwrap();
        session.set_volume(77).unwrap();
        session.join();

        let tel = session.telemetry();
        assert_eq!(tel.frames_sent, 30);
        assert_eq!(tel.remote_volume, 77);
        assert!(tel.pongs_received >= 1);

        assert!(wait_until(Duration::from_secs(2), || {
            rx_handle_slot
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|h| h.is_closed() && h.frame_count() >= 20)
        }));

        // 2. Fallback to TCP when server has enable_udp = false even if client prefer_udp = true
        let rx_fb_slot: Arc<Mutex<Option<MockPcmPlaybackHandle>>> = Arc::new(Mutex::new(None));
        let fb_c = Arc::clone(&rx_fb_slot);
        let fb_factory: PcmPlaybackFactory = Arc::new(move |_peer_id| {
            let (play, handle) = MockPcmPlayback::pair();
            *fb_c.lock().unwrap() = Some(handle);
            Ok(Box::new(play))
        });

        let mut no_udp_cfg = Sht3ServerConfig::new("tcp-fallback-receiver");
        no_udp_cfg.enable_udp = false;
        let fb_addr = manager
            .start_listener(
                "127.0.0.1:0".parse().unwrap(),
                no_udp_cfg,
                trust,
                fb_factory,
            )
            .unwrap();

        let fb_frames = vec![vec![500i16; spc * 2]; 12];
        let fb_session = manager
            .start_outbound(
                OutboundConfig::new("udp-sender", fb_addr, k_pair)
                    .with_profile(AudioProfile::OpusUltraLowLatency)
                    .with_jitter_ms(15)
                    .with_prefer_udp(true),
                Box::new(MockPcmCapture::from_frames(fb_frames)),
            )
            .unwrap();
        fb_session.join();
        assert_eq!(fb_session.telemetry().frames_sent, 12);
        assert!(wait_until(Duration::from_secs(2), || {
            rx_fb_slot
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|h| h.is_closed() && h.frame_count() >= 10)
        }));
    }
}
