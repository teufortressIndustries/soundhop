//! High-level audio sharing, single & multi-room connection orchestration,
//! persistent per-app routing, PIN/PSK pairing, and Android session control.

mod android;
mod mic;
mod pairing;
mod routing;
mod session;
mod volume;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use thiserror::Error;

use crate::android::{AndroidCaptureSession, AndroidManager};
use crate::backend::{create_backend, AudioBackend, Module};
use crate::config::{is_paired_security_mode, Config};
use crate::constants::{PROTOCOL_VERSION, SINK_PREFIX, SOURCE_PREFIX, VERSION};
use crate::discovery::{BeaconV3Meta, Peer};
use crate::pairing::TrustedPeer;
use crate::state::{KnownPeer, PairedPeer, State};
use crate::telemetry::{AudioTelemetryMonitor, TelemetrySnapshot, TelemetryTracker};
use crate::transport::{
    AudioProfile, OutboundSessionHandle, Sht3SessionManager, StreamTelemetry, DEFAULT_V3_PORT,
};
#[cfg(test)]
use crate::transport::{PcmCapture, PcmPlaybackFactory, TransportResult};
use crate::util::{
    format_duration, is_own_non_loopback_ip, local_addresses, lock_or_recover, now_secs, short_id,
    slugify,
};

pub use session::parse_audio_profile;

struct Sht3StreamMetrics {
    jitter_buf_ms: u16,
    dropped_frames: u32,
    frames_sent: u64,
    bytes_sent: u64,
    audio_profile: &'static str,
}

#[derive(Debug, Error)]
pub enum ControllerError {
    #[error("{0}")]
    Message(String),
    #[error("Backend error: {0}")]
    Backend(#[from] crate::backend::BackendError),
    #[error("Config error: {0}")]
    Config(#[from] crate::config::ConfigError),
}

#[derive(Debug, Clone)]
pub struct ConnectOptions {
    pub switch_default: Option<bool>,
    pub move_streams: Option<bool>,
    pub keep_local: bool,
    pub by: String,
    pub notify_user: Option<bool>,
    pub with_mic: bool,
    pub mic_direction: String,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        Self {
            switch_default: None,
            move_streams: None,
            keep_local: false,
            by: "manual".to_string(),
            notify_user: None,
            with_mic: false,
            mic_direction: "send".to_string(),
        }
    }
}

#[derive(Default)]
pub(super) struct DiskFileMtimes {
    config: Option<(std::time::SystemTime, u64)>,
    state: Option<(std::time::SystemTime, u64)>,
}

fn file_stamp(path: &std::path::Path) -> Option<(std::time::SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    Some((modified, meta.len()))
}

pub struct Controller {
    pub config: Mutex<Config>,
    pub state: Mutex<State>,
    pub backend: Arc<dyn AudioBackend>,
    pub android_manager: Arc<AndroidManager>,
    pub android_session: Arc<AndroidCaptureSession>,
    pub telemetry: Arc<TelemetryTracker>,
    pub sht3_manager: Arc<Sht3SessionManager>,
    pub(super) sht3_outbounds: Mutex<HashMap<String, OutboundSessionHandle>>,
    pub(super) sht3_mode_active: AtomicBool,
    pub(super) peer_v3_meta: Mutex<HashMap<String, BeaconV3Meta>>,
    #[cfg(test)]
    pub(super) playback_factory_override: Mutex<Option<PcmPlaybackFactory>>,
    #[cfg(test)]
    #[allow(clippy::type_complexity)]
    pub(super) capture_factory_override:
        Mutex<Option<Arc<dyn Fn() -> TransportResult<Box<dyn PcmCapture>> + Send + Sync>>>,
    #[cfg(test)]
    pub(super) capture_creates_null_sink: AtomicBool,
    pub(super) config_path: Option<PathBuf>,
    pub(super) state_path: Option<PathBuf>,
    pub(super) notify_enabled: bool,
    pub(super) save_enabled: Mutex<bool>,
    pub(super) connect_lock: Mutex<()>,
    pub(super) file_mtimes: Mutex<DiskFileMtimes>,
}

impl Controller {
    pub fn new(
        config: Option<Config>,
        state: Option<State>,
        backend: Option<Arc<dyn AudioBackend>>,
        config_path: Option<PathBuf>,
        state_path: Option<PathBuf>,
        notify_enabled: bool,
    ) -> Self {
        let cfg =
            config.unwrap_or_else(|| Config::load(config_path.as_deref()).unwrap_or_default());
        let st = state.unwrap_or_else(|| State::load(state_path.as_deref()));
        let be = backend.unwrap_or_else(create_backend);
        let mgr = Arc::new(AndroidManager::new(
            &cfg.android.scrcpy_path,
            &cfg.android.adb_path,
        ));
        let sess = Arc::new(AndroidCaptureSession::new(
            Arc::clone(&mgr),
            Arc::clone(&be),
        ));
        let initial_mtimes = DiskFileMtimes {
            config: file_stamp(&config_path.clone().unwrap_or_else(crate::util::config_path)),
            state: file_stamp(&state_path.clone().unwrap_or_else(crate::util::state_path)),
        };
        Self {
            config: Mutex::new(cfg),
            state: Mutex::new(st),
            backend: be,
            android_manager: mgr,
            android_session: sess,
            telemetry: Arc::new(TelemetryTracker::new()),
            sht3_manager: Arc::new(Sht3SessionManager::new()),
            sht3_outbounds: Mutex::new(HashMap::new()),
            sht3_mode_active: AtomicBool::new(false),
            peer_v3_meta: Mutex::new(HashMap::new()),
            #[cfg(test)]
            playback_factory_override: Mutex::new(None),
            #[cfg(test)]
            capture_factory_override: Mutex::new(None),
            #[cfg(test)]
            capture_creates_null_sink: AtomicBool::new(false),
            config_path,
            state_path,
            notify_enabled,
            save_enabled: Mutex::new(true),
            connect_lock: Mutex::new(()),
            file_mtimes: Mutex::new(initial_mtimes),
        }
    }

    pub fn remember_peer_v3_meta(&self, peer_id: &str, meta: BeaconV3Meta) {
        if peer_id.trim().is_empty() || meta.is_empty() {
            return;
        }
        lock_or_recover(&self.peer_v3_meta).insert(peer_id.to_string(), meta);
    }

    pub fn peer_v3_meta(&self, peer_id: &str) -> Option<BeaconV3Meta> {
        lock_or_recover(&self.peer_v3_meta).get(peer_id).cloned()
    }

    pub fn is_v3_active(&self) -> bool {
        self.sht3_manager.listener_addr().is_some()
    }

    pub fn is_legacy_pa_disabled(&self) -> bool {
        let sec = lock_or_recover(&self.config).sharing.security.clone();
        let mod_id = lock_or_recover(&self.state).listener_module;
        is_paired_security_mode(&sec) && mod_id == 0
    }

    pub fn sht3_outbound_active(&self) -> bool {
        let guard = lock_or_recover(&self.sht3_outbounds);
        !guard.is_empty() && guard.values().all(OutboundSessionHandle::is_running)
    }

    pub fn has_dead_sht3_session(&self) -> bool {
        if !self.sht3_mode_active.load(Ordering::Acquire) {
            return false;
        }
        let guard = lock_or_recover(&self.sht3_outbounds);
        guard.is_empty() || guard.values().any(|h| !h.is_running())
    }

    pub fn is_sht3_peer_active(&self, peer_id: &str) -> bool {
        lock_or_recover(&self.sht3_outbounds)
            .get(peer_id)
            .is_some_and(OutboundSessionHandle::is_running)
    }

    /// Sync RTT samples from all running SHT3 outbound streams into `TelemetryTracker`.
    pub fn sync_sht3_telemetry(&self) -> bool {
        let snapshots: Vec<(String, StreamTelemetry)> = {
            let guard = lock_or_recover(&self.sht3_outbounds);
            guard
                .iter()
                .filter(|(_, handle)| handle.is_running())
                .map(|(peer_id, handle)| (peer_id.clone(), handle.telemetry()))
                .collect()
        };
        let mut recorded = false;
        for (peer_id, stats) in snapshots {
            if stats.pongs_received > 0 && stats.rtt_us > 0 {
                let rtt_ms = stats.rtt_us as f64 / 1000.0;
                self.telemetry.record_rtt(&peer_id, rtt_ms);
                recorded = true;
            }
        }
        recorded
    }

    /// Tear down a dead SHT3 outbound session and its virtual sinks while preserving
    /// `cfg.connection.last_peer` and `auto_paused = false` so auto-connect can recover.
    pub fn teardown_dead_sht3_session(&self) -> Result<Option<serde_json::Value>, ControllerError> {
        let _op_guard = lock_or_recover(&self.connect_lock);
        if !lock_or_recover(&self.state).is_connected() || !self.has_dead_sht3_session() {
            return Ok(None);
        }
        let restore = lock_or_recover(&self.config).connection.restore_default;
        let res = self.disconnect_internal(false, restore, false, None, false, false, false)?;
        Ok(Some(res))
    }

    fn sht3_stream_metrics(&self) -> Option<Sht3StreamMetrics> {
        let guard = lock_or_recover(&self.sht3_outbounds);
        if guard.is_empty() {
            return None;
        }
        let mut jitter_buf_ms = 0u16;
        let mut dropped_frames = 0u32;
        let mut frames_sent = 0u64;
        let mut bytes_sent = 0u64;
        let mut profile_opt: Option<AudioProfile> = None;
        for handle in guard.values() {
            let t = handle.telemetry();
            jitter_buf_ms = jitter_buf_ms.max(t.jitter_buf_ms);
            dropped_frames = dropped_frames.saturating_add(t.dropped_frames);
            frames_sent = frames_sent.saturating_add(t.frames_sent);
            bytes_sent = bytes_sent.saturating_add(t.bytes_sent);
            if profile_opt.is_none() {
                profile_opt = AudioProfile::from_audio_params(&handle.audio_params()).ok();
            }
        }
        let audio_profile = match profile_opt {
            Some(AudioProfile::OpusBalanced) | None => "opus-balanced",
            Some(AudioProfile::OpusLowLatency) => "opus-low-latency",
            Some(AudioProfile::OpusUltraLowLatency) => "opus-ultra-low-latency",
            Some(AudioProfile::OpusLowBandwidth) => "opus-low-bandwidth",
            Some(AudioProfile::PcmLossless) => "pcm-lossless",
        };
        Some(Sht3StreamMetrics {
            jitter_buf_ms,
            dropped_frames,
            frames_sent,
            bytes_sent,
            audio_profile,
        })
    }

    #[cfg(test)]
    pub(crate) fn set_playback_factory(&self, factory: PcmPlaybackFactory) {
        *lock_or_recover(&self.playback_factory_override) = Some(factory);
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn set_capture_factory(
        &self,
        factory: Arc<dyn Fn() -> TransportResult<Box<dyn PcmCapture>> + Send + Sync>,
    ) {
        *lock_or_recover(&self.capture_factory_override) = Some(factory);
    }

    #[cfg(test)]
    pub(crate) fn set_capture_creates_null_sink(&self, enabled: bool) {
        self.capture_creates_null_sink
            .store(enabled, Ordering::Release);
    }

    pub fn set_save_enabled(&self, enabled: bool) {
        *lock_or_recover(&self.save_enabled) = enabled;
    }

    pub fn is_save_enabled(&self) -> bool {
        *lock_or_recover(&self.save_enabled)
    }

    pub fn start_telemetry_monitor(self: &Arc<Self>) -> AudioTelemetryMonitor {
        AudioTelemetryMonitor::start(Arc::clone(self))
    }

    pub fn reload_from_disk(&self) {
        if !*lock_or_recover(&self.save_enabled) {
            return;
        }
        // Skip overwriting in-memory state if a connect/disconnect/mic operation is in flight.
        let Ok(_op_guard) = self.connect_lock.try_lock() else {
            return;
        };
        let cfg_path = self
            .config_path
            .clone()
            .unwrap_or_else(crate::util::config_path);
        let st_path = self
            .state_path
            .clone()
            .unwrap_or_else(crate::util::state_path);
        let cfg_stamp = file_stamp(&cfg_path);
        let st_stamp = file_stamp(&st_path);
        let (cfg_changed, st_changed) = {
            let mtimes = lock_or_recover(&self.file_mtimes);
            (cfg_stamp != mtimes.config, st_stamp != mtimes.state)
        };
        if !cfg_changed && !st_changed {
            return;
        }

        if cfg_changed {
            if let Ok(disk_cfg) = Config::load(self.config_path.as_deref()) {
                *lock_or_recover(&self.config) = disk_cfg;
            }
            lock_or_recover(&self.file_mtimes).config = cfg_stamp;
        }
        if !st_changed {
            return;
        }
        lock_or_recover(&self.file_mtimes).state = st_stamp;
        let Ok(mut disk_state) = State::try_load(self.state_path.as_deref()) else {
            return;
        };
        let mut guard = lock_or_recover(&self.state);
        for (k, v) in &guard.known_peers {
            if !is_own_non_loopback_ip(&v.host) {
                match disk_state.known_peers.get_mut(k) {
                    Some(disk_peer) => {
                        if v.last_seen >= disk_peer.last_seen {
                            *disk_peer = v.clone();
                        }
                    }
                    None => {
                        disk_state.known_peers.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        if disk_state.connected_peer == guard.connected_peer
            && disk_state.connected_sink == guard.connected_sink
            && guard.last_synced_volume >= 0
        {
            disk_state.last_synced_volume = guard.last_synced_volume;
            disk_state.last_synced_mute = guard.last_synced_mute;
        }
        *guard = disk_state;
    }

    pub(super) fn reload_state_for_op(&self) {
        if !*lock_or_recover(&self.save_enabled) {
            return;
        }
        if let Ok(disk_cfg) = Config::load(self.config_path.as_deref()) {
            *lock_or_recover(&self.config) = disk_cfg;
        }
        let mut disk_state = State::load(self.state_path.as_deref());
        let mut guard = lock_or_recover(&self.state);
        for (k, v) in &guard.known_peers {
            if !is_own_non_loopback_ip(&v.host) {
                disk_state
                    .known_peers
                    .entry(k.clone())
                    .or_insert_with(|| v.clone());
            }
        }
        *guard = disk_state;
    }

    pub fn save(&self) {
        if !*lock_or_recover(&self.save_enabled) {
            return;
        }
        let _ = lock_or_recover(&self.config).save(self.config_path.as_deref());
        let _ = lock_or_recover(&self.state).save(self.state_path.as_deref());
        let cfg_path = self
            .config_path
            .clone()
            .unwrap_or_else(crate::util::config_path);
        let st_path = self
            .state_path
            .clone()
            .unwrap_or_else(crate::util::state_path);
        let mut mtimes = lock_or_recover(&self.file_mtimes);
        mtimes.config = file_stamp(&cfg_path);
        mtimes.state = file_stamp(&st_path);
    }

    pub fn reload_config(&self) -> Result<Config, ControllerError> {
        let new_cfg = Config::load(self.config_path.as_deref())?;
        *lock_or_recover(&self.config) = new_cfg.clone();
        Ok(new_cfg)
    }

    pub fn sink_name_for(peer: &Peer) -> String {
        let label = slugify(&peer.display_name(), 32);
        format!("{SINK_PREFIX}{label}_{}", short_id(&peer.id, 6))
    }

    pub fn source_name_for(peer: &Peer) -> String {
        let label = slugify(&peer.display_name(), 32);
        format!("{SOURCE_PREFIX}{label}_{}", short_id(&peer.id, 6))
    }

    pub fn remember_peer(&self, peer: &Peer) {
        if let Some(rtt) = peer.rtt_ms {
            self.telemetry.record_rtt(&peer.id, rtt);
        }
        lock_or_recover(&self.state).remember_peer(KnownPeer {
            id: peer.id.clone(),
            name: peer.name.clone(),
            host: peer.host.clone(),
            port: peer.port,
            last_seen: now_secs(),
            version: peer.version.clone(),
            security: peer.security.clone(),
            platform: peer.platform.clone(),
        });
    }

    pub fn known_peer(&self, peer_id: &str) -> Option<Peer> {
        let st = lock_or_recover(&self.state);
        let kp = st.known_peers.get(peer_id)?;
        let rtt_ms = self.telemetry.peer_stats(peer_id).map(|s| s.last_rtt_ms);
        Some(Peer {
            id: kp.id.clone(),
            name: kp.name.clone(),
            host: kp.host.clone(),
            port: kp.port,
            version: kp.version.clone(),
            protocol: PROTOCOL_VERSION,
            security: kp.security.clone(),
            platform: kp.platform.clone(),
            last_seen: kp.last_seen,
            source: "remembered".to_string(),
            rtt_ms,
        })
    }

    pub fn status(&self, peers: Option<&[Peer]>) -> serde_json::Value {
        self.reload_from_disk();
        let cfg = lock_or_recover(&self.config).clone();
        let st = lock_or_recover(&self.state).clone();

        let mut known_list: Vec<KnownPeer> = st.known_peers.values().cloned().collect();
        known_list.sort_by(|a, b| {
            b.last_seen
                .partial_cmp(&a.last_seen)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let backend_available = self.backend.available();
        let backend_json = if backend_available {
            let sinks_json: Vec<serde_json::Value> = self
                .backend
                .sinks()
                .unwrap_or_default()
                .into_iter()
                .map(|s| {
                    serde_json::json!({
                        "name": s.name,
                        "state": s.state,
                        "spec": s.spec,
                        "soundhop": s.is_soundhop(),
                    })
                })
                .collect();
            serde_json::json!({
                "available": true,
                "server": self.backend.server_label().unwrap_or_default(),
                "pipewire": self.backend.is_pipewire().unwrap_or(false),
                "default_sink": self.backend.default_sink().unwrap_or_default(),
                "sinks": sinks_json,
            })
        } else {
            serde_json::json!({ "available": false })
        };

        let connection_json = if st.is_connected() {
            let sht3_out_active = self.sht3_outbound_active();
            let sink_present = self
                .backend
                .sink_exists(&st.connected_sink)
                .unwrap_or(false);
            let module_present = if st.connected_module > 0 {
                self.backend
                    .module(st.connected_module)
                    .ok()
                    .flatten()
                    .is_some()
            } else {
                sht3_out_active
            };
            let dur = if st.connected_since > 0.0 {
                Some(now_secs() - st.connected_since)
            } else {
                None
            };
            let net_stats = self.telemetry.peer_stats(&st.connected_peer);
            let sht3_metrics = self.sht3_stream_metrics();
            serde_json::json!({
                "connected": true,
                "peer_id": st.connected_peer,
                "name": st.connected_name,
                "host": st.connected_host,
                "port": st.connected_port,
                "sink": st.connected_sink,
                "module": st.connected_module,
                "transport": if sht3_out_active { "sht3" } else { "pa-tcp" },
                "multiroom": st.is_multiroom(),
                "multiroom_peers": st.multiroom_peers,
                "keep_local": st.keep_local,
                "source": st.connected_source,
                "with_mic": !st.connected_source.is_empty(),
                "mic_direction": st.mic_direction,
                "since": st.connected_since,
                "duration": format_duration(dur),
                "by": st.connected_by,
                "sink_present": sink_present,
                "module_present": module_present,
                "rtt_ms": net_stats.as_ref().map(|n| n.last_rtt_ms),
                "jitter_ms": net_stats.as_ref().map(|n| n.jitter_ms),
                "jitter_buf_ms": sht3_metrics.as_ref().map(|m| m.jitter_buf_ms),
                "dropped_frames": sht3_metrics.as_ref().map(|m| m.dropped_frames),
                "frames_sent": sht3_metrics.as_ref().map(|m| m.frames_sent),
                "bytes_sent": sht3_metrics.as_ref().map(|m| m.bytes_sent),
                "audio_profile": sht3_metrics.as_ref().map(|m| m.audio_profile),
            })
        } else {
            serde_json::Value::Null
        };

        let volume_json = self.status_volume_json(&st, backend_available);

        let peers_json: Vec<serde_json::Value> =
            peers.unwrap_or(&[]).iter().map(|p| p.to_json()).collect();

        let trusted_list = self.trusted_peers();
        let trusted_peers_count = trusted_list.len();
        let v3_addr = self.sht3_manager.listener_addr();
        let v3_active = v3_addr.is_some();
        let legacy_pa_disabled =
            is_paired_security_mode(&cfg.sharing.security) && st.listener_module == 0;

        serde_json::json!({
            "version": VERSION,
            "protocol": PROTOCOL_VERSION,
            "config_path": cfg.path.to_string_lossy(),
            "device": {
                "name": cfg.device.name,
                "id": cfg.device.id,
            },
            "audio": {
                "profile": cfg.audio.profile,
            },
            "sharing": {
                "enabled": cfg.sharing.enabled,
                "port": cfg.sharing.port,
                "v3_port": cfg.sharing.v3_port,
                "pair_port": cfg.sharing.pair_port,
                "security": cfg.sharing.security,
                "listener_module": st.listener_module,
                "listener_created": st.listener_created,
                "listener_security": st.listener_security,
                "v3_active": v3_active,
                "v3_addr": v3_addr.map(|a| a.to_string()),
                "v3_inbound_peers": self.sht3_manager.active_inbound_peers(),
                "legacy_pa_disabled": legacy_pa_disabled,
                "trusted_peers_count": trusted_peers_count,
                "addresses": local_addresses(false),
            },
            "v3_active": v3_active,
            "legacy_pa_disabled": legacy_pa_disabled,
            "trusted_peers_count": trusted_peers_count,
            "auto": {
                "enabled": cfg.connection.auto_connect,
                "paused": st.auto_paused,
                "last_peer": cfg.connection.last_peer,
            },
            "routing": {
                "default_mode": cfg.routing.default_mode,
                "always_remote": cfg.routing.always_remote,
                "always_local": cfg.routing.always_local,
            },
            "android": self.android_session.status(),
            "connection": connection_json,
            "volume": volume_json,
            "telemetry": self.telemetry.snapshot(),
            "peers": peers_json,
            "known_peers": known_list,
            "paired_peers": st
                .paired_peers
                .values()
                .map(PairedPeer::to_public_json)
                .collect::<Vec<_>>(),
            "trusted_peers": trusted_list
                .iter()
                .map(TrustedPeer::to_public_json)
                .collect::<Vec<_>>(),
            "backend": backend_json,
        })
    }

    pub fn telemetry_snapshot(&self) -> TelemetrySnapshot {
        self.telemetry.snapshot()
    }

    // -- Reconciliation & Diagnostics ------------------------------------

    pub fn reconcile(&self) -> serde_json::Value {
        self.reload_from_disk();
        let mut actions = Vec::new();
        if !self.backend.available() {
            return serde_json::json!({
                "available": false,
                "actions": ["pactl is not reachable"],
            });
        }
        let st_snap = lock_or_recover(&self.state).clone();
        if st_snap.is_connected() && self.has_dead_sht3_session() {
            if let Ok(Some(res)) = self.teardown_dead_sht3_session() {
                actions.push("cleared dead SHT3 session".to_string());
                if let Some(restored) = res.get("restored_to").and_then(|v| v.as_str()) {
                    if !restored.is_empty() {
                        actions.push(format!("restored default sink to {restored}"));
                    }
                }
            }
        } else if !st_snap.is_connected() && self.has_dead_sht3_session() {
            let handles: Vec<OutboundSessionHandle> = lock_or_recover(&self.sht3_outbounds)
                .drain()
                .map(|(_, h)| h)
                .collect();
            for h in handles {
                h.stop();
            }
            self.sht3_mode_active.store(false, Ordering::Release);
            actions.push("cleaned up orphaned SHT3 session handles".to_string());
        } else if st_snap.is_connected() && !self.sht3_outbound_active() {
            let sink_ok = self
                .backend
                .sink_exists(&st_snap.connected_sink)
                .unwrap_or(false);
            let mod_ok = st_snap.connected_module > 0
                && self
                    .backend
                    .module(st_snap.connected_module)
                    .ok()
                    .flatten()
                    .is_some();
            if !sink_ok || !mod_ok {
                lock_or_recover(&self.state).clear_connection();
                actions.push("cleared stale connection state".to_string());
                if lock_or_recover(&self.config).connection.restore_default {
                    let cand = self.local_default_candidate(&st_snap.previous_default_sink);
                    if !cand.is_empty() && self.backend.set_default_sink(&cand).is_ok() {
                        actions.push(format!("restored default sink to {cand}"));
                    }
                }
                self.save();
            }
        }
        self.reconcile_listener();
        let cfg_snap = lock_or_recover(&self.config).clone();
        let sharing_enabled = cfg_snap.sharing.enabled;
        let has_listener = self.listener_running();
        if sharing_enabled && !has_listener && self.ensure_listener_internal().is_ok() {
            actions.push("resurrected audio listener".to_string());
        } else if sharing_enabled && !self.is_v3_active() {
            let v3_port = if cfg_snap.sharing.v3_port == 0 {
                DEFAULT_V3_PORT
            } else {
                cfg_snap.sharing.v3_port
            };
            if self
                .start_sht3_listener_internal(v3_port, &cfg_snap)
                .is_ok()
            {
                actions.push("restarted SHT3 listener".to_string());
            }
        }
        if let Ok(removed_mics) = self.backend.cleanup_orphaned_remote_mic_modules() {
            if !removed_mics.is_empty() {
                actions.push(format!(
                    "cleaned up orphaned remote mic modules {removed_mics:?}"
                ));
            }
        }
        serde_json::json!({
            "available": true,
            "actions": actions,
        })
    }

    // -- Internal Helpers ------------------------------------------------

    pub(super) fn ensure_backend(&self) -> Result<(), ControllerError> {
        if !self.backend.available() {
            return Err(ControllerError::Message(
                "PipeWire/PulseAudio is not reachable through pactl. Install pipewire-pulse or pulseaudio and try again.".to_string(),
            ));
        }
        Ok(())
    }

    pub(super) fn should_notify(&self, explicit: Option<bool>) -> bool {
        if let Some(exp) = explicit {
            return exp;
        }
        self.notify_enabled && lock_or_recover(&self.config).ui.notifications
    }

    pub fn set_auto(&self, enabled: bool) -> serde_json::Value {
        lock_or_recover(&self.config).connection.auto_connect = enabled;
        if enabled {
            lock_or_recover(&self.state).auto_paused = false;
        }
        self.save();
        let paused = lock_or_recover(&self.state).auto_paused;
        serde_json::json!({
            "auto_connect": enabled,
            "paused": paused,
        })
    }

    pub fn pause_auto(&self, paused: bool) -> serde_json::Value {
        lock_or_recover(&self.state).auto_paused = paused;
        self.save();
        serde_json::json!({ "paused": paused })
    }

    pub fn find_module(&self, module_id: u32) -> Option<Module> {
        self.backend.module(module_id).ok().flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::linux::LinuxAudioBackend;
    use crate::util::{CommandResult, MockRunner};

    #[test]
    fn test_reload_from_disk_preserves_live_state_and_handles_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");
        let state_path = dir.path().join("state.json");

        let runner = Arc::new(MockRunner::new(|args, _| {
            Ok(CommandResult {
                args: args.to_vec(),
                returncode: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }));
        let backend = Arc::new(LinuxAudioBackend::with_runner(runner));
        let ctrl = Controller::new(
            Some(Config::default()),
            Some(State::default()),
            Some(backend),
            Some(cfg_path),
            Some(state_path.clone()),
            false,
        );

        // Populate live state and save to disk.
        {
            let mut st = ctrl.state.lock().unwrap();
            st.connected_peer = "peer-1".to_string();
            st.connected_sink = "soundhop_peer1".to_string();
            st.known_peers.insert(
                "peer-1".to_string(),
                KnownPeer {
                    id: "peer-1".to_string(),
                    name: "Old Name".to_string(),
                    host: "192.168.1.10".to_string(),
                    port: 4713,
                    last_seen: 100.0,
                    ..Default::default()
                },
            );
        }
        ctrl.save();

        // Update live state in memory without saving to disk.
        {
            let mut st = ctrl.state.lock().unwrap();
            st.last_synced_volume = 72;
            st.last_synced_mute = true;
            st.known_peers.insert(
                "peer-1".to_string(),
                KnownPeer {
                    id: "peer-1".to_string(),
                    name: "New Name".to_string(),
                    host: "192.168.1.25".to_string(),
                    port: 4714,
                    last_seen: 200.0,
                    ..Default::default()
                },
            );
        }

        // Reloading valid disk state must preserve newer live known_peers & volume sync cache.
        ctrl.reload_from_disk();
        {
            let st = ctrl.state.lock().unwrap();
            assert_eq!(st.last_synced_volume, 72);
            assert!(st.last_synced_mute);
            let kp = st.known_peers.get("peer-1").unwrap();
            assert_eq!(kp.name, "New Name");
            assert_eq!(kp.host, "192.168.1.25");
            assert_eq!(kp.port, 4714);
        }

        // Corrupt the state file on disk -> reload_from_disk must NOT wipe live state.
        std::fs::write(&state_path, "{ invalid json").unwrap();
        ctrl.reload_from_disk();
        {
            let st = ctrl.state.lock().unwrap();
            assert_eq!(st.connected_peer, "peer-1");
            assert_eq!(st.last_synced_volume, 72);
            assert!(st.known_peers.contains_key("peer-1"));
        }

        // Remove the state file -> reload_from_disk must NOT wipe live state.
        std::fs::remove_file(&state_path).unwrap();
        ctrl.reload_from_disk();
        {
            let st = ctrl.state.lock().unwrap();
            assert_eq!(st.connected_peer, "peer-1");
            assert!(st.known_peers.contains_key("peer-1"));
        }
    }

    #[test]
    fn test_reconcile_clears_dead_sht3_session_and_restarts_v3_listener() {
        use std::net::TcpListener;
        use std::sync::atomic::AtomicU32;
        use std::time::{Duration, Instant};

        use crate::discovery::{BeaconV3Meta, Peer};
        use crate::pairing::TrustedPeer;
        use crate::transport::{MockPcmCapture, MockPcmPlayback, PcmCapture, PcmPlayback};

        let make_mock_runner = || {
            let next_id = Arc::new(AtomicU32::new(301));
            let active_nulls = Arc::new(Mutex::new(Vec::<(u32, String)>::new()));
            let unloaded = Arc::new(Mutex::new(Vec::<u32>::new()));
            let default_sinks = Arc::new(Mutex::new(Vec::<String>::new()));
            let pa_listener_active = Arc::new(AtomicBool::new(false));

            let next_id_c = Arc::clone(&next_id);
            let active_nulls_c = Arc::clone(&active_nulls);
            let unloaded_c = Arc::clone(&unloaded);
            let default_sinks_c = Arc::clone(&default_sinks);
            let pa_c = Arc::clone(&pa_listener_active);

            let runner = Arc::new(MockRunner::new(move |args, _| {
                let joined = args.join(" ");
                let stdout = if joined.contains("info") {
                    "Server Name: PulseAudio (on PipeWire 1.2.0)\nDefault Sink: alsa_output.pci\n"
                        .to_string()
                } else if joined.contains("set-default-sink") {
                    if let Some(target) = args.last() {
                        default_sinks_c.lock().unwrap().push(target.clone());
                    }
                    String::new()
                } else if joined.contains("load-module module-native-protocol-tcp") {
                    pa_c.store(true, Ordering::SeqCst);
                    "42\n".to_string()
                } else if joined.contains("load-module module-null-sink") {
                    let id = next_id_c.fetch_add(1, Ordering::SeqCst);
                    let sink_name = args
                        .iter()
                        .find_map(|a| a.strip_prefix("sink_name="))
                        .unwrap_or("soundhop_null")
                        .to_string();
                    active_nulls_c.lock().unwrap().push((id, sink_name));
                    format!("{id}\n")
                } else if joined.contains("unload-module") {
                    if let Some(id) = args.last().and_then(|s| s.parse::<u32>().ok()) {
                        unloaded_c.lock().unwrap().push(id);
                        active_nulls_c.lock().unwrap().retain(|(mid, _)| *mid != id);
                        if id == 42 {
                            pa_c.store(false, Ordering::SeqCst);
                        }
                    }
                    String::new()
                } else if joined.contains("list short sinks") {
                    let mut out =
                        "1\talsa_output.pci\tPipeWire\ts16le 2ch 48000Hz\tRUNNING\n".to_string();
                    for (id, sname) in active_nulls_c.lock().unwrap().iter() {
                        out.push_str(&format!(
                            "{id}\t{sname}\tmodule-null-sink\ts16le 2ch 48000Hz\tRUNNING\n"
                        ));
                    }
                    out
                } else if joined.contains("list short modules") {
                    let mut out = String::new();
                    if pa_c.load(Ordering::SeqCst) {
                        out.push_str("42\tmodule-native-protocol-tcp\tport=4713\n");
                    }
                    for (id, sname) in active_nulls_c.lock().unwrap().iter() {
                        out.push_str(&format!("{id}\tmodule-null-sink\tsink_name={sname}\n"));
                    }
                    out
                } else {
                    String::new()
                };
                Ok(CommandResult {
                    args: args.to_vec(),
                    returncode: 0,
                    stdout,
                    stderr: String::new(),
                })
            }));
            (runner, unloaded, default_sinks)
        };

        let free_v3_port = {
            let sock = TcpListener::bind("127.0.0.1:0").unwrap();
            sock.local_addr().unwrap().port()
        };

        let (srv_runner, _, _) = make_mock_runner();
        let mut srv_cfg = Config::default();
        srv_cfg.device.id = "server-dead".to_string();
        srv_cfg.sharing.security = "trusted-lan".to_string();
        srv_cfg.sharing.enabled = true;
        srv_cfg.sharing.v3_port = free_v3_port;
        let server_ctrl = Controller::new(
            Some(srv_cfg),
            Some(State::default()),
            Some(Arc::new(LinuxAudioBackend::with_runner(srv_runner))),
            None,
            None,
            false,
        );
        server_ctrl.set_save_enabled(false);
        server_ctrl.set_playback_factory(Arc::new(|_| {
            let (playback, _) = MockPcmPlayback::pair();
            Ok(Box::new(playback) as Box<dyn PcmPlayback>)
        }));

        let k_pair = [0x5Au8; 32];
        server_ctrl.record_trusted_peer(TrustedPeer {
            device_id: "client-dead".to_string(),
            display_name: "Client".to_string(),
            ed25519_pub: [0x11u8; 32],
            pairwise_secret: k_pair,
            fingerprint: "1111-2222-3333-4444".to_string(),
            paired_at: 100.0,
        });
        server_ctrl.start_listener(Some(false)).unwrap();
        let bound_v3_port = server_ctrl.sht3_manager.listener_addr().unwrap().port();

        let (cli_runner, cli_unloaded, cli_default_sinks) = make_mock_runner();
        let mut cli_cfg = Config::default();
        cli_cfg.device.id = "client-dead".to_string();
        cli_cfg.audio.profile = "pcm-lossless".to_string();
        cli_cfg.connection.restore_default = true;
        cli_cfg.sharing.enabled = false;
        let client_ctrl = Controller::new(
            Some(cli_cfg),
            Some(State::default()),
            Some(Arc::new(LinuxAudioBackend::with_runner(cli_runner))),
            None,
            None,
            false,
        );
        client_ctrl.set_save_enabled(false);
        client_ctrl.set_capture_creates_null_sink(true);
        client_ctrl.set_capture_factory(Arc::new(|| {
            // Finite 2-frame capture exits cleanly, turning the session into a dead SHT3 zombie.
            let cap = MockPcmCapture::sine_wave(440.0, Some(2)).with_pace(Duration::from_millis(5));
            Ok(Box::new(cap) as Box<dyn PcmCapture>)
        }));
        client_ctrl.record_trusted_peer(TrustedPeer {
            device_id: "server-dead".to_string(),
            display_name: "Dead Peer".to_string(),
            ed25519_pub: [0x22u8; 32],
            pairwise_secret: k_pair,
            fingerprint: "5555-6666-7777-8888".to_string(),
            paired_at: 100.0,
        });
        client_ctrl.remember_peer_v3_meta(
            "server-dead",
            BeaconV3Meta {
                v3_port: Some(bound_v3_port),
                pair_port: Some(4715),
                pub_fp: Some("5555-6666-7777-8888".to_string()),
                caps: vec!["sht3".to_string(), "pcm".to_string()],
            },
        );

        let peer = Peer {
            id: "server-dead".to_string(),
            name: "Dead Peer".to_string(),
            host: "127.0.0.1".to_string(),
            port: 4713,
            version: VERSION.to_string(),
            protocol: PROTOCOL_VERSION,
            security: "paired".to_string(),
            platform: "linux".to_string(),
            last_seen: now_secs(),
            source: "udp".to_string(),
            rtt_ms: None,
        };
        client_ctrl
            .connect(&peer, ConnectOptions::default())
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        while !client_ctrl.has_dead_sht3_session() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for finite SHT3 outbound session to become dead"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!client_ctrl.sht3_outbound_active());
        assert!(client_ctrl.state.lock().unwrap().is_connected());

        // reconcile() must tear down the zombie SHT3 session, unload module-null-sink 301,
        // restore default sink, and preserve last_peer + auto_paused=false for auto-reconnect.
        let rec = client_ctrl.reconcile();
        let actions = rec["actions"].as_array().unwrap();
        assert!(actions
            .iter()
            .any(|a| a.as_str() == Some("cleared dead SHT3 session")));
        assert!(!client_ctrl.state.lock().unwrap().is_connected());
        assert!(!client_ctrl.has_dead_sht3_session());
        assert_eq!(
            client_ctrl.config.lock().unwrap().connection.last_peer,
            "server-dead"
        );
        assert!(!client_ctrl.state.lock().unwrap().auto_paused);
        assert!(cli_unloaded.lock().unwrap().contains(&301));
        assert!(cli_default_sinks
            .lock()
            .unwrap()
            .iter()
            .any(|s| s == "alsa_output.pci"));

        // Verify reconcile() also restarts SHT3 listener when sharing.enabled && !is_v3_active().
        assert!(server_ctrl.is_v3_active());
        server_ctrl.sht3_manager.stop_listener();
        assert!(!server_ctrl.is_v3_active());
        assert!(server_ctrl.listener_running());
        let srv_rec = server_ctrl.reconcile();
        assert!(server_ctrl.is_v3_active());
        assert!(srv_rec["actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a.as_str() == Some("restarted SHT3 listener")));
        server_ctrl.stop_listener(Some(false)).unwrap();
    }

    #[test]
    fn test_sync_sht3_telemetry_and_status_metrics() {
        use std::net::TcpListener;
        use std::time::{Duration, Instant};

        use crate::discovery::{BeaconV3Meta, Peer};
        use crate::pairing::TrustedPeer;
        use crate::transport::{MockPcmCapture, MockPcmPlayback, PcmCapture, PcmPlayback};

        let make_runner = || {
            Arc::new(MockRunner::new(|args, _| {
                let joined = args.join(" ");
                let stdout = if joined.contains("info") {
                    "Server Name: PulseAudio (on PipeWire 1.2.0)\nDefault Sink: alsa_output.pci\n"
                        .to_string()
                } else {
                    String::new()
                };
                Ok(CommandResult {
                    args: args.to_vec(),
                    returncode: 0,
                    stdout,
                    stderr: String::new(),
                })
            }))
        };

        let free_v3_port = {
            let sock = TcpListener::bind("127.0.0.1:0").unwrap();
            sock.local_addr().unwrap().port()
        };

        let mut srv_cfg = Config::default();
        srv_cfg.device.id = "server-tel".to_string();
        srv_cfg.sharing.security = "paired".to_string();
        srv_cfg.sharing.v3_port = free_v3_port;
        let server_ctrl = Controller::new(
            Some(srv_cfg),
            Some(State::default()),
            Some(Arc::new(LinuxAudioBackend::with_runner(make_runner()))),
            None,
            None,
            false,
        );
        server_ctrl.set_save_enabled(false);
        server_ctrl.set_playback_factory(Arc::new(|_| {
            let (playback, _) = MockPcmPlayback::pair();
            Ok(Box::new(playback) as Box<dyn PcmPlayback>)
        }));

        let k_pair = [0x66u8; 32];
        server_ctrl.record_trusted_peer(TrustedPeer {
            device_id: "client-tel".to_string(),
            display_name: "Client".to_string(),
            ed25519_pub: [0x11u8; 32],
            pairwise_secret: k_pair,
            fingerprint: "1111-2222-3333-4444".to_string(),
            paired_at: 100.0,
        });
        server_ctrl.start_listener(Some(false)).unwrap();
        let bound_v3_port = server_ctrl.sht3_manager.listener_addr().unwrap().port();

        let mut cli_cfg = Config::default();
        cli_cfg.device.id = "client-tel".to_string();
        cli_cfg.audio.profile = "pcm-lossless".to_string();
        let client_ctrl = Controller::new(
            Some(cli_cfg),
            Some(State::default()),
            Some(Arc::new(LinuxAudioBackend::with_runner(make_runner()))),
            None,
            None,
            false,
        );
        client_ctrl.set_save_enabled(false);
        client_ctrl.set_capture_factory(Arc::new(|| {
            let cap =
                MockPcmCapture::sine_wave(440.0, Some(200)).with_pace(Duration::from_millis(15));
            Ok(Box::new(cap) as Box<dyn PcmCapture>)
        }));
        client_ctrl.record_trusted_peer(TrustedPeer {
            device_id: "server-tel".to_string(),
            display_name: "Telemetry Receiver".to_string(),
            ed25519_pub: [0x22u8; 32],
            pairwise_secret: k_pair,
            fingerprint: "9999-8888-7777-6666".to_string(),
            paired_at: 100.0,
        });
        client_ctrl.remember_peer_v3_meta(
            "server-tel",
            BeaconV3Meta {
                v3_port: Some(bound_v3_port),
                pair_port: Some(4715),
                pub_fp: Some("9999-8888-7777-6666".to_string()),
                caps: vec!["sht3".to_string(), "pcm".to_string()],
            },
        );

        let peer = Peer {
            id: "server-tel".to_string(),
            name: "Telemetry Receiver".to_string(),
            host: "127.0.0.1".to_string(),
            port: 4713,
            version: VERSION.to_string(),
            protocol: PROTOCOL_VERSION,
            security: "paired".to_string(),
            platform: "linux".to_string(),
            last_seen: now_secs(),
            source: "udp".to_string(),
            rtt_ms: None,
        };
        client_ctrl
            .connect(&peer, ConnectOptions::default())
            .unwrap();

        // Trigger a Ping/Pong exchange and wait for Pong telemetry.
        {
            let guard = client_ctrl.sht3_outbounds.lock().unwrap();
            let handle = guard.get("server-tel").unwrap();
            handle.request_ping().unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let ready = {
                let guard = client_ctrl.sht3_outbounds.lock().unwrap();
                let t = guard.get("server-tel").unwrap().telemetry();
                t.pongs_received > 0 && t.frames_sent > 0
            };
            if ready {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for SHT3 pong & frames"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        assert!(client_ctrl.sync_sht3_telemetry());
        let peer_stats = client_ctrl.telemetry.peer_stats("server-tel").unwrap();
        assert!(peer_stats.last_rtt_ms > 0.0);

        let status = client_ctrl.status(None);
        let conn = &status["connection"];
        assert_eq!(conn["transport"], "sht3");
        assert_eq!(conn["audio_profile"], "pcm-lossless");
        assert!(conn["frames_sent"].as_u64().unwrap() > 0);
        assert!(conn["bytes_sent"].as_u64().unwrap() > 0);
        assert!(conn["jitter_buf_ms"].is_u64());
        assert!(conn["dropped_frames"].is_u64());
        assert!(conn["rtt_ms"].as_f64().unwrap() > 0.0);

        client_ctrl
            .disconnect(false, Some(false), false, Some(false), false, false)
            .unwrap();
        server_ctrl.stop_listener(Some(false)).unwrap();
    }
}
