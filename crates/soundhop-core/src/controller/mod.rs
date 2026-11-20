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
use crate::transport::{OutboundSessionHandle, Sht3SessionManager};
#[cfg(test)]
use crate::transport::{PcmCapture, PcmPlaybackFactory, TransportResult};
use crate::util::{
    format_duration, is_own_non_loopback_ip, local_addresses, lock_or_recover, now_secs, short_id,
    slugify,
};

pub use session::parse_audio_profile;

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

pub struct Controller {
    pub config: Mutex<Config>,
    pub state: Mutex<State>,
    pub backend: Arc<dyn AudioBackend>,
    pub android_manager: Arc<AndroidManager>,
    pub android_session: Arc<AndroidCaptureSession>,
    pub telemetry: Arc<TelemetryTracker>,
    pub sht3_manager: Arc<Sht3SessionManager>,
    pub(super) sht3_outbound: Mutex<Option<OutboundSessionHandle>>,
    pub(super) peer_v3_meta: Mutex<HashMap<String, BeaconV3Meta>>,
    #[cfg(test)]
    pub(super) playback_factory_override: Mutex<Option<PcmPlaybackFactory>>,
    #[cfg(test)]
    #[allow(clippy::type_complexity)]
    pub(super) capture_factory_override:
        Mutex<Option<Arc<dyn Fn() -> TransportResult<Box<dyn PcmCapture>> + Send + Sync>>>,
    pub(super) config_path: Option<PathBuf>,
    pub(super) state_path: Option<PathBuf>,
    pub(super) notify_enabled: bool,
    pub(super) save_enabled: Mutex<bool>,
    pub(super) connect_lock: Mutex<()>,
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
        Self {
            config: Mutex::new(cfg),
            state: Mutex::new(st),
            backend: be,
            android_manager: mgr,
            android_session: sess,
            telemetry: Arc::new(TelemetryTracker::new()),
            sht3_manager: Arc::new(Sht3SessionManager::new()),
            sht3_outbound: Mutex::new(None),
            peer_v3_meta: Mutex::new(HashMap::new()),
            #[cfg(test)]
            playback_factory_override: Mutex::new(None),
            #[cfg(test)]
            capture_factory_override: Mutex::new(None),
            config_path,
            state_path,
            notify_enabled,
            save_enabled: Mutex::new(true),
            connect_lock: Mutex::new(()),
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
        lock_or_recover(&self.sht3_outbound)
            .as_ref()
            .is_some_and(OutboundSessionHandle::is_running)
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
        if let Ok(disk_cfg) = Config::load(self.config_path.as_deref()) {
            *lock_or_recover(&self.config) = disk_cfg;
        }
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

        let backend_json = if self.backend.available() {
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
            })
        } else {
            serde_json::Value::Null
        };

        let volume_json = self.status_volume_json(&st);

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
        if st_snap.is_connected() && !self.sht3_outbound_active() {
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
        let sharing_enabled = lock_or_recover(&self.config).sharing.enabled;
        let has_listener = self.listener_running();
        if sharing_enabled && !has_listener && self.ensure_listener_internal().is_ok() {
            actions.push("resurrected audio listener".to_string());
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
}
