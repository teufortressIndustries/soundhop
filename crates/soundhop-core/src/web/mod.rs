//! Web Broadcast server orchestration, auth tokens, shared capture lifecycle, and QR helpers.
//!
//! Coordinates the single shared [`GstBroadcaster`] pipeline (lazy start on `0 -> 1` listener,
//! `5 s` idle stop when `0` listeners) with the synchronous [`rtc`], [`ws`], and [`http`] subsystems.

pub mod http;
pub mod qr;
pub mod rtc;
pub mod tls;
pub mod ws;

use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ipnet::IpNet;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::Config;
use crate::constants::{
    DEFAULT_LAN_NETWORKS, DEFAULT_WEBRTC_PORT, DEFAULT_WEB_MAX_CLIENTS, DEFAULT_WEB_PORT,
};
use crate::security::constant_time_eq;
use crate::transport::wire::AudioParams;
use crate::transport::{CaptureEndpoint, GstBroadcaster, SubscriberRegistry, TransportError};
use crate::util::{hostname_name, local_addresses, lock_or_recover};

use self::http::{HttpServerConfig, HttpServerHandle};
use self::rtc::{RtcMuxConfig, RtcMuxHandle};
use self::ws::{WsFanoutConfig, WsFanoutHandle};

/// Embedded single-page browser audio player served at `GET /`.
#[allow(dead_code)]
pub(crate) const WEB_PLAYER_HTML: &str = include_str!("web_player.html");

/// Default idle grace period before stopping [`GstBroadcaster`] after the last listener leaves.
pub const DEFAULT_IDLE_STOP_GRACE: Duration = Duration::from_secs(5);

/// Maximum allowed concurrent browser listeners (clamped upper bound).
pub const MAX_WEB_CLIENTS_LIMIT: usize = 32;

/// Errors returned by Web Broadcast operations.
#[derive(Debug, Error)]
pub enum WebError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid Web Broadcast configuration: {0}")]
    InvalidConfig(String),

    #[error("Web Broadcast is already running")]
    AlreadyRunning,

    #[error("Web Broadcast is not running")]
    NotRunning,

    #[error("unauthorized: {0}")]
    Unauthorized(String),

    #[error("forbidden: {0}")]
    Forbidden(String),

    #[error("rate limit exceeded: {0}")]
    RateLimited(String),

    #[error("maximum client limit ({max}) reached")]
    MaxClientsReached { max: usize },

    #[error("bad request: {0}")]
    BadRequest(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("service unavailable: {0}")]
    ServiceUnavailable(String),

    #[error("WebRTC error: {0}")]
    Rtc(String),

    #[error("WebSocket error: {0}")]
    WebSocket(String),

    #[error("transport error: {0}")]
    Transport(#[from] TransportError),

    #[error("QR code error: {0}")]
    Qr(String),

    #[error("internal channel closed")]
    ChannelClosed,
}

/// Result alias for Web Broadcast operations.
pub type WebResult<T> = Result<T, WebError>;

/// Audio capture endpoint & encoding target passed to [`GstBroadcaster`].
#[derive(Debug, Clone)]
pub struct WebCaptureTarget {
    pub endpoint: CaptureEndpoint,
    pub audio_params: AudioParams,
    pub label: String,
}

/// Factory invoked lazily on the `0 -> 1` listener transition to obtain the live capture target.
pub type CaptureFactory = Arc<dyn Fn() -> WebResult<WebCaptureTarget> + Send + Sync>;

/// Runtime configuration for starting [`WebBroadcastManager`].
#[derive(Debug, Clone)]
pub struct WebBroadcastConfig {
    pub http_port: u16,
    pub webrtc_udp_port: u16,
    pub bind_ip: IpAddr,
    pub require_token: bool,
    pub keep_local: bool,
    pub max_clients: usize,
    pub allowed_networks: Vec<String>,
    pub device_name: String,
    pub audio_params: AudioParams,
    pub capture_source_label: String,
    pub idle_stop_grace: Duration,
    pub https_enabled: bool,
    pub tls_cert_path: Option<PathBuf>,
    pub tls_key_path: Option<PathBuf>,
    pub tls_dir: Option<PathBuf>,
}

impl Default for WebBroadcastConfig {
    fn default() -> Self {
        Self {
            http_port: DEFAULT_WEB_PORT,
            webrtc_udp_port: DEFAULT_WEBRTC_PORT,
            bind_ip: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            require_token: true,
            keep_local: true,
            max_clients: usize::from(DEFAULT_WEB_MAX_CLIENTS),
            allowed_networks: DEFAULT_LAN_NETWORKS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            device_name: hostname_name(),
            audio_params: AudioParams::default(),
            capture_source_label: String::new(),
            idle_stop_grace: DEFAULT_IDLE_STOP_GRACE,
            https_enabled: true,
            tls_cert_path: None,
            tls_key_path: None,
            tls_dir: None,
        }
    }
}

impl WebBroadcastConfig {
    /// Build a [`WebBroadcastConfig`] from SoundHop's persisted [`Config`].
    pub fn from_app_config(
        cfg: &Config,
        audio_params: AudioParams,
        capture_source_label: impl Into<String>,
    ) -> Self {
        let allowed_networks = if cfg.sharing.allowed_networks.is_empty() {
            DEFAULT_LAN_NETWORKS
                .iter()
                .map(|s| (*s).to_string())
                .collect()
        } else {
            cfg.sharing.allowed_networks.clone()
        };
        let tls_cert_path = Some(cfg.sharing.web_tls_cert_path.trim())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);
        let tls_key_path = Some(cfg.sharing.web_tls_key_path.trim())
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);
        Self {
            http_port: cfg.sharing.web_port,
            webrtc_udp_port: cfg.sharing.webrtc_port,
            bind_ip: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            require_token: cfg.sharing.web_require_token,
            keep_local: cfg.sharing.web_keep_local,
            max_clients: usize::from(cfg.sharing.web_max_clients).clamp(1, MAX_WEB_CLIENTS_LIMIT),
            allowed_networks,
            device_name: cfg.device.name.clone(),
            audio_params,
            capture_source_label: capture_source_label.into(),
            idle_stop_grace: DEFAULT_IDLE_STOP_GRACE,
            https_enabled: cfg.sharing.web_https,
            tls_cert_path,
            tls_key_path,
            tls_dir: None,
        }
    }

    fn normalized(mut self) -> Self {
        self.max_clients = self.max_clients.clamp(1, MAX_WEB_CLIENTS_LIMIT);
        if self.allowed_networks.is_empty() {
            self.allowed_networks = DEFAULT_LAN_NETWORKS
                .iter()
                .map(|s| (*s).to_string())
                .collect();
        }
        if self.device_name.trim().is_empty() {
            self.device_name = hostname_name();
        }
        self
    }
}

/// Snapshot metadata for a single connected browser listener (WebRTC or WebSocket).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WebClientInfo {
    pub id: String,
    pub remote_addr: String,
    pub transport: String,
    pub user_agent: String,
    pub connected_since: f64,
    pub bytes_sent: u64,
    pub rtt_ms: Option<f64>,
    pub dropped_frames: u32,
}

/// Live Web Broadcast status payload exposed via `Controller` and daemon IPC.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WebBroadcastStatus {
    pub active: bool,
    pub http_port: u16,
    pub webrtc_udp_port: u16,
    pub urls: Vec<String>,
    pub localhost_url: String,
    pub token_required: bool,
    pub token: Option<String>,
    pub clients: Vec<WebClientInfo>,
    pub adb_reversed_serials: Vec<String>,
    pub keep_local: bool,
    pub pipeline_running: bool,
    pub capture_source: String,
}

impl Default for WebBroadcastStatus {
    fn default() -> Self {
        Self::inactive(true, true, Vec::new())
    }
}

impl WebBroadcastStatus {
    fn inactive(keep_local: bool, token_required: bool, adb_reversed_serials: Vec<String>) -> Self {
        Self {
            active: false,
            http_port: 0,
            webrtc_udp_port: 0,
            urls: Vec::new(),
            localhost_url: String::new(),
            token_required,
            token: None,
            clients: Vec::new(),
            adb_reversed_serials,
            keep_local,
            pipeline_running: false,
            capture_source: String::new(),
        }
    }
}

/// Result of inspecting an HTTP `Authorization: Bearer <token>` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BearerAuthResult {
    /// Auth is disabled or the supplied bearer token matched in constant time.
    Authorized,
    /// Auth is required and no `Authorization` header was supplied.
    Missing,
    /// Auth is required and the supplied header/token was malformed or invalid.
    Invalid,
}

/// Generate a 128-bit CSPRNG session token encoded as 32 lowercase hex characters.
pub fn generate_session_token() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Constant-time verification of a Web Broadcast session token.
pub fn verify_token(expected: &str, provided: &str) -> bool {
    !expected.is_empty() && constant_time_eq(expected.as_bytes(), provided.as_bytes())
}

/// Format a browser listen URL for `scheme`, `host`, and `http_port`, embedding `#t=<token>` if present.
pub fn format_listen_url_with_scheme(
    scheme: &str,
    host: &str,
    http_port: u16,
    token: Option<&str>,
) -> String {
    let formatted_host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    match token.filter(|t| !t.is_empty()) {
        Some(tok) => format!("{scheme}://{formatted_host}:{http_port}/#t={tok}"),
        None => format!("{scheme}://{formatted_host}:{http_port}/"),
    }
}

/// Format a browser listen URL for `host` and `http_port`, embedding `#t=<token>` if present.
pub fn format_listen_url(host: &str, http_port: u16, token: Option<&str>) -> String {
    format_listen_url_with_scheme("http", host, http_port, token)
}

/// Build `(lan_urls, localhost_url)` for the current machine's non-loopback LAN interfaces.
///
/// When `https_enabled` is `true`, LAN URLs use `https://` while `localhost_url` remains
/// `http://localhost:...` so USB `adb reverse` continues to work without a certificate warning.
pub fn build_listen_urls_with_https(
    http_port: u16,
    token: Option<&str>,
    https_enabled: bool,
) -> (Vec<String>, String) {
    let lan_scheme = if https_enabled { "https" } else { "http" };
    let lan_urls = local_addresses(false)
        .into_iter()
        .map(|ip| format_listen_url_with_scheme(lan_scheme, &ip, http_port, token))
        .collect();
    let localhost_url = format_listen_url("localhost", http_port, token);
    (lan_urls, localhost_url)
}

/// Build `(lan_urls, localhost_url)` for the current machine's non-loopback LAN interfaces.
pub fn build_listen_urls(http_port: u16, token: Option<&str>) -> (Vec<String>, String) {
    build_listen_urls_with_https(http_port, token, false)
}

/// Return `true` when `ip` is loopback or belongs to `allowed_networks`.
pub(crate) fn is_ip_allowed(ip: &IpAddr, allowed_networks: &[String]) -> bool {
    let normalized = match ip {
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(*v6)),
        IpAddr::V4(_) => *ip,
    };
    if normalized.is_loopback() {
        return true;
    }
    let nets: &[String] = allowed_networks;
    for net_str in nets {
        if let Ok(net) = net_str.parse::<IpNet>() {
            if net.contains(&normalized) {
                return true;
            }
        } else if let Ok(single) = net_str.parse::<IpAddr>() {
            if single == normalized {
                return true;
            }
        }
    }
    false
}

/// Returns `true` when this binary was compiled with GStreamer Opus/audio capture support.
pub const fn gstreamer_available() -> bool {
    cfg!(feature = "gstreamer")
}

struct AuthState {
    require_token: bool,
    token: String,
}

struct PipelineState {
    broadcaster: Option<GstBroadcaster>,
    listener_count: usize,
    idle_deadline: Option<Instant>,
    stopping: bool,
    capture_source: String,
    last_error: Option<String>,
}

struct PipelineSupervisor {
    capture_factory: CaptureFactory,
    registry: Arc<SubscriberRegistry>,
    idle_stop_grace: Duration,
    potato_mode: AtomicBool,
    start_lock: Mutex<()>,
    state: Mutex<PipelineState>,
    condvar: Condvar,
    reaper_thread: Mutex<Option<JoinHandle<()>>>,
}

impl PipelineSupervisor {
    fn new(
        capture_factory: CaptureFactory,
        registry: Arc<SubscriberRegistry>,
        initial_capture_source: String,
        idle_stop_grace: Duration,
    ) -> WebResult<Arc<Self>> {
        let supervisor = Arc::new(Self {
            capture_factory,
            registry,
            idle_stop_grace,
            potato_mode: AtomicBool::new(false),
            start_lock: Mutex::new(()),
            state: Mutex::new(PipelineState {
                broadcaster: None,
                listener_count: 0,
                idle_deadline: None,
                stopping: false,
                capture_source: initial_capture_source,
                last_error: None,
            }),
            condvar: Condvar::new(),
            reaper_thread: Mutex::new(None),
        });

        let bg = Arc::clone(&supervisor);
        let handle = thread::Builder::new()
            .name("web-pipeline-idle".into())
            .spawn(move || bg.reaper_loop())?;
        *lock_or_recover(&supervisor.reaper_thread) = Some(handle);
        Ok(supervisor)
    }

    fn reaper_loop(&self) {
        let mut guard = lock_or_recover(&self.state);
        loop {
            if guard.stopping {
                break;
            }
            if guard.listener_count == 0 {
                if let Some(deadline) = guard.idle_deadline {
                    let now = Instant::now();
                    if now >= deadline {
                        guard.idle_deadline = None;
                        let broadcaster = guard.broadcaster.take();
                        drop(guard);
                        if let Some(b) = broadcaster {
                            b.stop();
                        }
                        guard = lock_or_recover(&self.state);
                        continue;
                    }
                    let wait_for = deadline.saturating_duration_since(now);
                    let (next_guard, _) = self
                        .condvar
                        .wait_timeout(guard, wait_for)
                        .unwrap_or_else(|e| e.into_inner());
                    guard = next_guard;
                    continue;
                }
            }
            guard = self.condvar.wait(guard).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn acquire_listener(&self) -> WebResult<()> {
        let _start_guard = lock_or_recover(&self.start_lock);

        let need_start = {
            let mut guard = lock_or_recover(&self.state);
            if guard.stopping {
                return Err(WebError::NotRunning);
            }
            guard.listener_count = guard.listener_count.saturating_add(1);
            guard.idle_deadline = None;
            self.condvar.notify_all();
            guard.broadcaster.is_none()
        };

        if !need_start {
            return Ok(());
        }

        let start_res = (self.capture_factory)().and_then(|target| {
            let label = target.label;
            let broadcaster = GstBroadcaster::start_with_registry(
                target.endpoint,
                target.audio_params,
                Arc::clone(&self.registry),
            )
            .map_err(|err| match err {
                TransportError::Pipeline(msg) if !gstreamer_available() => {
                    WebError::ServiceUnavailable(msg)
                }
                other => WebError::Transport(other),
            })?;
            Ok((broadcaster, label))
        });

        match start_res {
            Ok((broadcaster, label)) => {
                if self.potato_mode.load(Ordering::Acquire) {
                    broadcaster.set_potato_mode(true);
                }
                let stop_immediately = {
                    let mut guard = lock_or_recover(&self.state);
                    if guard.stopping {
                        true
                    } else {
                        guard.broadcaster = Some(broadcaster);
                        if !label.is_empty() {
                            guard.capture_source = label;
                        }
                        guard.last_error = None;
                        return Ok(());
                    }
                };
                if stop_immediately {
                    return Err(WebError::NotRunning);
                }
                unreachable!()
            }
            Err(err) => {
                let mut guard = lock_or_recover(&self.state);
                guard.listener_count = guard.listener_count.saturating_sub(1);
                guard.last_error = Some(err.to_string());
                Err(err)
            }
        }
    }

    fn release_listener(&self) {
        let mut guard = lock_or_recover(&self.state);
        guard.listener_count = guard.listener_count.saturating_sub(1);
        if guard.listener_count == 0 && guard.broadcaster.is_some() && !guard.stopping {
            guard.idle_deadline = Some(Instant::now() + self.idle_stop_grace);
            self.condvar.notify_all();
        }
    }

    fn is_running(&self) -> bool {
        let guard = lock_or_recover(&self.state);
        guard.broadcaster.as_ref().is_some_and(|b| b.is_healthy())
    }

    fn capture_source(&self) -> String {
        lock_or_recover(&self.state).capture_source.clone()
    }

    fn listener_count(&self) -> usize {
        lock_or_recover(&self.state).listener_count
    }

    fn set_potato_mode(&self, enabled: bool) {
        self.potato_mode.store(enabled, Ordering::Release);
        let guard = lock_or_recover(&self.state);
        if let Some(ref broadcaster) = guard.broadcaster {
            broadcaster.set_potato_mode(enabled);
        }
    }

    fn is_potato_mode(&self) -> bool {
        self.potato_mode.load(Ordering::Acquire)
    }

    fn shutdown(&self) {
        let _start_guard = lock_or_recover(&self.start_lock);
        let broadcaster = {
            let mut guard = lock_or_recover(&self.state);
            guard.stopping = true;
            guard.idle_deadline = None;
            let b = guard.broadcaster.take();
            self.condvar.notify_all();
            b
        };
        drop(_start_guard);

        if let Some(b) = broadcaster {
            b.stop();
        }
        if let Some(handle) = lock_or_recover(&self.reaper_thread).take() {
            let _ = handle.join();
        }
    }
}

/// Shared runtime coordinator passed to [`rtc`], [`ws`], and [`http`] subsystems.
///
/// Enforces combined `max_clients` across WebRTC + WebSocket listeners, tracks live auth
/// tokens across rotations, and drives lazy start / `5 s` idle stop of [`GstBroadcaster`].
pub struct BroadcastRuntime {
    registry: Arc<SubscriberRegistry>,
    max_clients: usize,
    active_clients: AtomicUsize,
    http_port: AtomicU16,
    webrtc_udp_port: AtomicU16,
    allowed_networks: Vec<String>,
    device_name: String,
    keep_local: bool,
    stopped: AtomicBool,
    auth: RwLock<AuthState>,
    pipeline: Arc<PipelineSupervisor>,
}

/// RAII capacity permit ensuring combined WebRTC + WebSocket client counts stay within `max_clients`.
pub struct ClientSlotGuard {
    runtime: Arc<BroadcastRuntime>,
}

impl Drop for ClientSlotGuard {
    fn drop(&mut self) {
        self.runtime.active_clients.fetch_sub(1, Ordering::AcqRel);
    }
}

/// RAII listener permit driving lazy `0 -> 1` pipeline start and `5 s` idle stop on `1 -> 0`.
pub struct ListenerGuard {
    runtime: Arc<BroadcastRuntime>,
}

impl Drop for ListenerGuard {
    fn drop(&mut self) {
        self.runtime.pipeline.release_listener();
    }
}

impl BroadcastRuntime {
    /// Shared Opus/PCM subscriber registry owned by this broadcast session.
    pub fn registry(&self) -> &Arc<SubscriberRegistry> {
        &self.registry
    }

    /// Atomically reserve one slot against `max_clients` (combined WebRTC + WebSocket).
    pub fn try_acquire_client_slot(self: &Arc<Self>) -> WebResult<ClientSlotGuard> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(WebError::NotRunning);
        }
        let max = self.max_clients;
        loop {
            let cur = self.active_clients.load(Ordering::Acquire);
            if cur >= max {
                return Err(WebError::MaxClientsReached { max });
            }
            if self
                .active_clients
                .compare_exchange_weak(cur, cur + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break;
            }
        }
        Ok(ClientSlotGuard {
            runtime: Arc::clone(self),
        })
    }

    /// Acquire a live listener permit, lazily starting [`GstBroadcaster`] on `0 -> 1`.
    pub fn acquire_listener(self: &Arc<Self>) -> WebResult<ListenerGuard> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(WebError::NotRunning);
        }
        self.pipeline.acquire_listener()?;
        Ok(ListenerGuard {
            runtime: Arc::clone(self),
        })
    }

    /// Current combined WebRTC + WebSocket client count holding capacity slots.
    pub fn active_client_count(&self) -> usize {
        self.active_clients.load(Ordering::Acquire)
    }

    /// Configured combined client limit.
    pub fn max_clients(&self) -> usize {
        self.max_clients
    }

    /// Current number of active media listeners keeping [`GstBroadcaster`] alive.
    pub fn active_listener_count(&self) -> usize {
        self.pipeline.listener_count()
    }

    /// Enable or disable Potato Opus tuning on the shared broadcast pipeline.
    pub fn set_potato_mode(&self, enabled: bool) {
        self.pipeline.set_potato_mode(enabled);
    }

    /// Whether Potato Opus tuning is currently enabled for this broadcast session.
    pub fn is_potato_mode(&self) -> bool {
        self.pipeline.is_potato_mode()
    }

    /// Whether [`GstBroadcaster`] is currently running and healthy.
    pub fn is_pipeline_running(&self) -> bool {
        self.pipeline.is_running()
    }

    /// Human-readable description of the active capture source.
    pub fn capture_source(&self) -> String {
        self.pipeline.capture_source()
    }

    /// Configured `keep_local` mode.
    pub fn keep_local(&self) -> bool {
        self.keep_local
    }

    /// Device display name advertised at `GET /api/info`.
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// Configured LAN allowlist CIDRs/IPs.
    pub fn allowed_networks(&self) -> &[String] {
        &self.allowed_networks
    }

    /// Check whether `ip` is permitted by `allowed_networks` (loopback is always permitted).
    pub fn is_ip_allowed(&self, ip: &IpAddr) -> bool {
        is_ip_allowed(ip, &self.allowed_networks)
    }

    /// Bound HTTP/WS TCP port.
    pub fn http_port(&self) -> u16 {
        self.http_port.load(Ordering::Acquire)
    }

    /// Bound WebRTC ICE-Lite UDP port.
    pub fn webrtc_udp_port(&self) -> u16 {
        self.webrtc_udp_port.load(Ordering::Acquire)
    }

    /// Whether bearer token authentication is enforced for HTTP/WS endpoints.
    pub fn token_required(&self) -> bool {
        self.auth
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .require_token
    }

    /// Current ephemeral session token when `token_required()` is `true`.
    pub fn current_token(&self) -> Option<String> {
        let guard = self.auth.read().unwrap_or_else(|e| e.into_inner());
        if guard.require_token {
            Some(guard.token.clone())
        } else {
            None
        }
    }

    /// Rotate the ephemeral 128-bit hex token in place.
    pub fn rotate_token(&self) -> Option<String> {
        let next = generate_session_token();
        let mut guard = self.auth.write().unwrap_or_else(|e| e.into_inner());
        guard.token = next;
        if guard.require_token {
            Some(guard.token.clone())
        } else {
            None
        }
    }

    /// Constant-time check of a raw token string (for WebSocket subprotocol / auth frame).
    pub fn verify_token(&self, provided: &str) -> bool {
        let guard = self.auth.read().unwrap_or_else(|e| e.into_inner());
        if !guard.require_token {
            return true;
        }
        verify_token(&guard.token, provided)
    }

    /// Constant-time check of an HTTP `Authorization` header value (`Bearer <token>`).
    pub fn verify_http_bearer(&self, auth_header: Option<&str>) -> BearerAuthResult {
        let guard = self.auth.read().unwrap_or_else(|e| e.into_inner());
        if !guard.require_token {
            return BearerAuthResult::Authorized;
        }
        let Some(raw) = auth_header.map(str::trim).filter(|s| !s.is_empty()) else {
            return BearerAuthResult::Missing;
        };
        let Some((scheme, token)) = raw.split_once(' ') else {
            return BearerAuthResult::Invalid;
        };
        if !scheme.eq_ignore_ascii_case("bearer") {
            return BearerAuthResult::Invalid;
        }
        if verify_token(&guard.token, token.trim()) {
            BearerAuthResult::Authorized
        } else {
            BearerAuthResult::Invalid
        }
    }

    fn set_bound_ports(&self, http_port: u16, webrtc_udp_port: u16) {
        self.http_port.store(http_port, Ordering::Release);
        self.webrtc_udp_port
            .store(webrtc_udp_port, Ordering::Release);
    }

    fn mark_stopped(&self) {
        self.stopped.store(true, Ordering::Release);
    }
}

struct ActiveBroadcast {
    runtime: Arc<BroadcastRuntime>,
    rtc: Arc<RtcMuxHandle>,
    ws: Arc<WsFanoutHandle>,
    http: HttpServerHandle,
    https_enabled: bool,
}

/// Top-level Web Broadcast lifecycle manager owned by `Controller`.
pub struct WebBroadcastManager {
    op_lock: Mutex<()>,
    active: Mutex<Option<ActiveBroadcast>>,
    last_keep_local: Mutex<bool>,
    last_require_token: Mutex<bool>,
    adb_reversed_serials: Mutex<Vec<String>>,
}

impl Default for WebBroadcastManager {
    fn default() -> Self {
        Self::new()
    }
}

impl WebBroadcastManager {
    /// Create an inactive [`WebBroadcastManager`].
    pub fn new() -> Self {
        Self {
            op_lock: Mutex::new(()),
            active: Mutex::new(None),
            last_keep_local: Mutex::new(true),
            last_require_token: Mutex::new(true),
            adb_reversed_serials: Mutex::new(Vec::new()),
        }
    }

    /// Start the HTTP, WebSocket, and WebRTC servers with lazy shared capture.
    pub fn start(
        &self,
        config: WebBroadcastConfig,
        capture_factory: CaptureFactory,
    ) -> WebResult<WebBroadcastStatus> {
        let _op_guard = lock_or_recover(&self.op_lock);
        if lock_or_recover(&self.active).is_some() {
            return Err(WebError::AlreadyRunning);
        }

        let cfg = config.normalized();
        let tls_material = if cfg.https_enabled {
            let material = match (&cfg.tls_cert_path, &cfg.tls_key_path) {
                (Some(cert), Some(key)) => tls::load_custom_tls(cert, key)?,
                (None, None) => {
                    let dir = cfg.tls_dir.clone().unwrap_or_else(tls::default_web_tls_dir);
                    tls::load_or_generate_lan_tls(&dir, &cfg.device_name, &[])?
                }
                _ => {
                    return Err(WebError::InvalidConfig(
                        "both web_tls_cert_path and web_tls_key_path must be set together".into(),
                    ));
                }
            };
            Some(material)
        } else {
            None
        };

        let registry = Arc::new(SubscriberRegistry::new());
        let pipeline = PipelineSupervisor::new(
            capture_factory,
            Arc::clone(&registry),
            cfg.capture_source_label.clone(),
            cfg.idle_stop_grace,
        )?;

        let runtime = Arc::new(BroadcastRuntime {
            registry,
            max_clients: cfg.max_clients,
            active_clients: AtomicUsize::new(0),
            http_port: AtomicU16::new(cfg.http_port),
            webrtc_udp_port: AtomicU16::new(cfg.webrtc_udp_port),
            allowed_networks: cfg.allowed_networks,
            device_name: cfg.device_name,
            keep_local: cfg.keep_local,
            stopped: AtomicBool::new(false),
            auth: RwLock::new(AuthState {
                require_token: cfg.require_token,
                token: generate_session_token(),
            }),
            pipeline,
        });

        let rtc = match RtcMuxHandle::start(RtcMuxConfig {
            bind_ip: cfg.bind_ip,
            udp_port: cfg.webrtc_udp_port,
            runtime: Arc::clone(&runtime),
        }) {
            Ok(h) => Arc::new(h),
            Err(err) => {
                runtime.mark_stopped();
                runtime.pipeline.shutdown();
                return Err(err);
            }
        };

        let ws = match WsFanoutHandle::start(WsFanoutConfig {
            runtime: Arc::clone(&runtime),
        }) {
            Ok(h) => Arc::new(h),
            Err(err) => {
                runtime.mark_stopped();
                rtc.stop();
                runtime.pipeline.shutdown();
                return Err(err);
            }
        };

        let http = match HttpServerHandle::start(HttpServerConfig {
            bind_ip: cfg.bind_ip,
            http_port: cfg.http_port,
            runtime: Arc::clone(&runtime),
            rtc: Arc::clone(&rtc),
            ws: Arc::clone(&ws),
            tls: tls_material,
        }) {
            Ok(h) => h,
            Err(err) => {
                runtime.mark_stopped();
                ws.stop();
                rtc.stop();
                runtime.pipeline.shutdown();
                return Err(err);
            }
        };

        runtime.set_bound_ports(http.local_port(), rtc.local_port());
        *lock_or_recover(&self.last_keep_local) = cfg.keep_local;
        *lock_or_recover(&self.last_require_token) = cfg.require_token;

        let session = ActiveBroadcast {
            runtime,
            rtc,
            ws,
            http,
            https_enabled: cfg.https_enabled,
        };
        let status = self.build_active_status(&session);
        *lock_or_recover(&self.active) = Some(session);
        Ok(status)
    }

    /// Stop all Web Broadcast servers, disconnect active listeners, and stop the capture pipeline.
    pub fn stop(&self) -> WebBroadcastStatus {
        let _op_guard = lock_or_recover(&self.op_lock);
        let session = lock_or_recover(&self.active).take();
        if let Some(session) = session {
            session.runtime.mark_stopped();
            session.http.stop();
            session.ws.stop();
            session.rtc.stop();
            session.runtime.pipeline.shutdown();
        }
        let keep_local = *lock_or_recover(&self.last_keep_local);
        let token_required = *lock_or_recover(&self.last_require_token);
        let adb_serials = lock_or_recover(&self.adb_reversed_serials).clone();
        WebBroadcastStatus::inactive(keep_local, token_required, adb_serials)
    }

    /// Returns `true` when the Web Broadcast servers are currently running.
    pub fn is_active(&self) -> bool {
        lock_or_recover(&self.active).is_some()
    }

    /// Enable or disable Potato Opus tuning on the active Web Broadcast session.
    pub fn set_potato_mode(&self, enabled: bool) {
        let guard = lock_or_recover(&self.active);
        if let Some(ref session) = *guard {
            session.runtime.set_potato_mode(enabled);
        }
    }

    /// Whether Potato Opus tuning is currently enabled on the active Web Broadcast session.
    pub fn is_potato_mode(&self) -> bool {
        let guard = lock_or_recover(&self.active);
        guard
            .as_ref()
            .is_some_and(|session| session.runtime.is_potato_mode())
    }

    /// Return a full live status snapshot.
    pub fn status(&self) -> WebBroadcastStatus {
        let guard = lock_or_recover(&self.active);
        if let Some(ref session) = *guard {
            self.build_active_status(session)
        } else {
            let keep_local = *lock_or_recover(&self.last_keep_local);
            let token_required = *lock_or_recover(&self.last_require_token);
            let adb_serials = lock_or_recover(&self.adb_reversed_serials).clone();
            WebBroadcastStatus::inactive(keep_local, token_required, adb_serials)
        }
    }

    /// List currently connected WebRTC and WebSocket clients.
    pub fn list_clients(&self) -> Vec<WebClientInfo> {
        let guard = lock_or_recover(&self.active);
        match *guard {
            Some(ref session) => Self::collect_clients(session),
            None => Vec::new(),
        }
    }

    /// Rotate the ephemeral 128-bit session token and return the updated status.
    pub fn rotate_token(&self) -> WebResult<WebBroadcastStatus> {
        let guard = lock_or_recover(&self.active);
        let session = guard.as_ref().ok_or(WebError::NotRunning)?;
        session.runtime.rotate_token();
        Ok(self.build_active_status(session))
    }

    /// Kick an active WebRTC or WebSocket client by ID. Returns `true` if found.
    pub fn kick_client(&self, client_id: &str) -> WebResult<bool> {
        let guard = lock_or_recover(&self.active);
        let session = guard.as_ref().ok_or(WebError::NotRunning)?;
        Ok(session.rtc.kick_peer(client_id) || session.ws.kick_client(client_id))
    }

    /// Update the list of USB ADB serials currently holding an active `adb reverse` tunnel.
    pub fn set_adb_reversed_serials(&self, serials: Vec<String>) {
        *lock_or_recover(&self.adb_reversed_serials) = serials;
    }

    /// Access the active [`BroadcastRuntime`] when running.
    pub fn runtime(&self) -> Option<Arc<BroadcastRuntime>> {
        lock_or_recover(&self.active)
            .as_ref()
            .map(|s| Arc::clone(&s.runtime))
    }

    fn collect_clients(session: &ActiveBroadcast) -> Vec<WebClientInfo> {
        let mut clients = session.rtc.list_clients();
        clients.extend(session.ws.list_clients());
        clients.sort_by(|a, b| {
            a.connected_since
                .partial_cmp(&b.connected_since)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.id.cmp(&b.id))
        });
        clients
    }

    fn build_active_status(&self, session: &ActiveBroadcast) -> WebBroadcastStatus {
        let http_port = session.runtime.http_port();
        let webrtc_udp_port = session.runtime.webrtc_udp_port();
        let token = session.runtime.current_token();
        let token_required = session.runtime.token_required();
        let (urls, localhost_url) =
            build_listen_urls_with_https(http_port, token.as_deref(), session.https_enabled);
        let adb_reversed_serials = lock_or_recover(&self.adb_reversed_serials).clone();

        WebBroadcastStatus {
            active: true,
            http_port,
            webrtc_udp_port,
            urls,
            localhost_url,
            token_required,
            token,
            clients: Self::collect_clients(session),
            adb_reversed_serials,
            keep_local: session.runtime.keep_local(),
            pipeline_running: session.runtime.is_pipeline_running(),
            capture_source: session.runtime.capture_source(),
        }
    }
}

impl Drop for WebBroadcastManager {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_capture_factory() -> CaptureFactory {
        Arc::new(|| {
            #[cfg(feature = "gstreamer")]
            let endpoint = CaptureEndpoint::TestSineWave {
                freq_hz: 440.0,
                netsim: None,
            };
            #[cfg(not(feature = "gstreamer"))]
            let endpoint = CaptureEndpoint::TestSineWave { freq_hz: 440.0 };

            Ok(WebCaptureTarget {
                endpoint,
                audio_params: AudioParams::default(),
                label: "test-sine-440".into(),
            })
        })
    }

    #[test]
    fn token_generation_and_constant_time_verification() {
        let t1 = generate_session_token();
        let t2 = generate_session_token();
        assert_eq!(t1.len(), 32);
        assert_eq!(t2.len(), 32);
        assert!(t1.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')));
        assert_ne!(t1, t2);
        assert!(verify_token(&t1, &t1));
        assert!(!verify_token(&t1, &t2));
        assert!(!verify_token(&t1, ""));
        assert!(!verify_token("", ""));
    }

    #[test]
    fn ip_allowlist_checks_loopback_and_lan_cidrs() {
        let nets = vec!["192.168.1.0/24".to_string(), "10.0.0.5".to_string()];
        assert!(is_ip_allowed(&"127.0.0.1".parse().unwrap(), &nets));
        assert!(is_ip_allowed(&"::1".parse().unwrap(), &nets));
        assert!(is_ip_allowed(&"192.168.1.42".parse().unwrap(), &nets));
        assert!(is_ip_allowed(&"10.0.0.5".parse().unwrap(), &nets));
        assert!(is_ip_allowed(
            &"::ffff:192.168.1.99".parse().unwrap(),
            &nets
        ));
        assert!(!is_ip_allowed(&"8.8.8.8".parse().unwrap(), &nets));
        assert!(!is_ip_allowed(&"192.168.2.10".parse().unwrap(), &nets));
    }

    #[test]
    fn format_urls_with_and_without_token() {
        assert_eq!(
            format_listen_url("192.168.1.10", 4717, Some("abcd")),
            "http://192.168.1.10:4717/#t=abcd"
        );
        assert_eq!(
            format_listen_url("localhost", 4717, None),
            "http://localhost:4717/"
        );
        assert_eq!(
            format_listen_url("::1", 4717, Some("abcd")),
            "http://[::1]:4717/#t=abcd"
        );
    }

    #[test]
    fn manager_enforces_max_clients_and_token_rotation() {
        let mgr = WebBroadcastManager::new();
        let cfg = WebBroadcastConfig {
            http_port: 0,
            webrtc_udp_port: 0,
            bind_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            max_clients: 2,
            idle_stop_grace: Duration::from_millis(80),
            https_enabled: false,
            ..Default::default()
        };

        let st1 = mgr
            .start(cfg, dummy_capture_factory())
            .expect("start succeeds");
        assert!(st1.active);
        assert!(st1.token_required);
        let tok1 = st1.token.clone().expect("token present");
        assert!(!st1.pipeline_running);

        let rt = mgr.runtime().expect("runtime available");
        assert_eq!(
            rt.verify_http_bearer(Some(&format!("Bearer {tok1}"))),
            BearerAuthResult::Authorized
        );
        assert_eq!(rt.verify_http_bearer(None), BearerAuthResult::Missing);
        assert_eq!(
            rt.verify_http_bearer(Some("Bearer wrong")),
            BearerAuthResult::Invalid
        );

        let s1 = rt.try_acquire_client_slot().expect("slot 1");
        let s2 = rt.try_acquire_client_slot().expect("slot 2");
        assert!(matches!(
            rt.try_acquire_client_slot(),
            Err(WebError::MaxClientsReached { max: 2 })
        ));
        drop(s1);
        let _s3 = rt.try_acquire_client_slot().expect("slot freed");
        drop(s2);

        let st2 = mgr.rotate_token().expect("rotate succeeds");
        let tok2 = st2.token.expect("rotated token");
        assert_ne!(tok1, tok2);
        assert!(!rt.verify_token(&tok1));
        assert!(rt.verify_token(&tok2));

        let stopped = mgr.stop();
        assert!(!stopped.active);
        assert!(stopped.token.is_none());
    }

    #[cfg(feature = "gstreamer")]
    #[test]
    fn lazy_pipeline_starts_on_first_listener_and_stops_after_idle_grace() {
        let mgr = WebBroadcastManager::new();
        let cfg = WebBroadcastConfig {
            http_port: 0,
            webrtc_udp_port: 0,
            bind_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            idle_stop_grace: Duration::from_millis(80),
            https_enabled: false,
            ..Default::default()
        };

        mgr.start(cfg, dummy_capture_factory()).expect("start");
        let rt = mgr.runtime().expect("runtime");
        assert!(!rt.is_pipeline_running());

        let l1 = rt.acquire_listener().expect("0 -> 1 starts pipeline");
        assert!(rt.is_pipeline_running());
        assert_eq!(rt.active_listener_count(), 1);

        let l2 = rt.acquire_listener().expect("1 -> 2 reuses pipeline");
        assert_eq!(rt.active_listener_count(), 2);

        drop(l1);
        assert!(rt.is_pipeline_running());
        drop(l2);
        // Immediately after last listener drops, pipeline stays warm during grace period.
        assert!(rt.is_pipeline_running());

        // Wait past the 80 ms grace period; idle reaper should stop the pipeline.
        std::thread::sleep(Duration::from_millis(220));
        assert!(!rt.is_pipeline_running());

        mgr.stop();
    }

    #[cfg(not(feature = "gstreamer"))]
    #[test]
    fn acquire_listener_returns_service_unavailable_without_gstreamer() {
        let mgr = WebBroadcastManager::new();
        let cfg = WebBroadcastConfig {
            http_port: 0,
            webrtc_udp_port: 0,
            bind_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            https_enabled: false,
            ..Default::default()
        };
        mgr.start(cfg, dummy_capture_factory()).expect("start");
        let rt = mgr.runtime().expect("runtime");
        let err = rt
            .acquire_listener()
            .err()
            .expect("should fail without gstreamer");
        assert!(matches!(err, WebError::ServiceUnavailable(_)));
        mgr.stop();
    }
}
