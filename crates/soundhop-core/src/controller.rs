//! High-level audio sharing, single & multi-room connection orchestration,
//! persistent per-app routing, PIN/PSK pairing, and Android session control.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use thiserror::Error;

use crate::android::{AndroidCaptureSession, AndroidManager};
use crate::backend::{
    create_backend, listener_security_from_args, AudioBackend, Listener, Module,
    SourceTunnelParams, TunnelParams,
};
use crate::config::Config;
use crate::constants::{
    MULTIROOM_SINK_NAME, PROTOCOL_VERSION, SINK_PREFIX, SOURCE_PREFIX, VERSION,
};
use crate::discovery::{discover_once, Peer};
use crate::security::{derive_key_hex, derive_pa_cookie, generate_pairing_pin, write_pa_cookie};
use crate::state::{ActivePeerSession, KnownPeer, PairedPeer, State};
use crate::telemetry::{
    AudioLevelSnapshot, AudioTelemetryMonitor, TelemetrySnapshot, TelemetryTracker,
};
use crate::util::{
    format_duration, is_own_non_loopback_ip, is_private_address, local_addresses, notify, now_secs,
    short_id, slugify, tcp_probe_rtt, tcp_reachable,
};

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
    config_path: Option<PathBuf>,
    state_path: Option<PathBuf>,
    notify_enabled: bool,
    save_enabled: Mutex<bool>,
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
            config_path,
            state_path,
            notify_enabled,
            save_enabled: Mutex::new(true),
        }
    }

    pub fn set_save_enabled(&self, enabled: bool) {
        *self.save_enabled.lock().unwrap() = enabled;
    }

    pub fn is_save_enabled(&self) -> bool {
        *self.save_enabled.lock().unwrap()
    }

    pub fn start_telemetry_monitor(self: &Arc<Self>) -> AudioTelemetryMonitor {
        AudioTelemetryMonitor::start(Arc::clone(self))
    }

    pub fn reload_from_disk(&self) {
        if !*self.save_enabled.lock().unwrap() {
            return;
        }
        if let Ok(disk_cfg) = Config::load(self.config_path.as_deref()) {
            *self.config.lock().unwrap() = disk_cfg;
        }
        let mut disk_state = State::load(self.state_path.as_deref());
        let mut guard = self.state.lock().unwrap();
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
        if !*self.save_enabled.lock().unwrap() {
            return;
        }
        let _ = self
            .config
            .lock()
            .unwrap()
            .save(self.config_path.as_deref());
        let _ = self.state.lock().unwrap().save(self.state_path.as_deref());
    }

    pub fn reload_config(&self) -> Result<Config, ControllerError> {
        let new_cfg = Config::load(self.config_path.as_deref())?;
        *self.config.lock().unwrap() = new_cfg.clone();
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
        self.state.lock().unwrap().remember_peer(KnownPeer {
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
        let st = self.state.lock().unwrap();
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
        let cfg = self.config.lock().unwrap().clone();
        let st = self.state.lock().unwrap().clone();

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
                false
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

        let volume_json = if self.backend.available() {
            let active_sink = if !st.connected_sink.is_empty() {
                st.connected_sink.clone()
            } else {
                self.backend.default_sink().unwrap_or_default()
            };
            let lvl = self.backend.get_sink_volume(&active_sink);
            let mute = self.backend.get_sink_mute(&active_sink);
            // Update synthetic audio level meter if active
            let any_stream = !self.backend.sink_inputs().unwrap_or_default().is_empty();
            let norm = if mute || !any_stream {
                0.0
            } else {
                (lvl as f32 / 100.0).clamp(0.0, 1.0) * 0.75
            };
            self.telemetry.set_audio_levels(AudioLevelSnapshot {
                peak_left: norm,
                peak_right: norm * 0.96,
                rms_left: norm * 0.7,
                rms_right: norm * 0.68,
                dbfs: if norm > 0.001 {
                    20.0 * norm.log10()
                } else {
                    -60.0
                },
                active: any_stream && !mute,
                updated_at: now_secs(),
            });
            serde_json::json!({
                "level": lvl,
                "mute": mute,
                "sink": active_sink,
            })
        } else {
            serde_json::Value::Null
        };

        let peers_json: Vec<serde_json::Value> =
            peers.unwrap_or(&[]).iter().map(|p| p.to_json()).collect();

        serde_json::json!({
            "version": VERSION,
            "protocol": PROTOCOL_VERSION,
            "config_path": cfg.path.to_string_lossy(),
            "device": {
                "name": cfg.device.name,
                "id": cfg.device.id,
            },
            "sharing": {
                "enabled": cfg.sharing.enabled,
                "port": cfg.sharing.port,
                "security": cfg.sharing.security,
                "listener_module": st.listener_module,
                "listener_created": st.listener_created,
                "listener_security": st.listener_security,
                "addresses": local_addresses(false),
            },
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
            "paired_peers": st.paired_peers.values().cloned().collect::<Vec<_>>(),
            "backend": backend_json,
        })
    }

    pub fn telemetry_snapshot(&self) -> TelemetrySnapshot {
        self.telemetry.snapshot()
    }

    // -- Sharing & Listener ----------------------------------------------

    pub fn share(
        &self,
        enabled: bool,
        notify_user: Option<bool>,
    ) -> Result<serde_json::Value, ControllerError> {
        let res = if enabled {
            let r = self.start_listener(notify_user)?;
            self.config.lock().unwrap().sharing.enabled = true;
            r
        } else {
            self.config.lock().unwrap().sharing.enabled = false;
            self.stop_listener(notify_user)?
        };
        self.save();
        Ok(res)
    }

    pub fn start_listener(
        &self,
        notify_user: Option<bool>,
    ) -> Result<serde_json::Value, ControllerError> {
        self.ensure_backend()?;
        self.reconcile_listener();
        let listener = self.ensure_listener_internal()?;
        if self.should_notify(notify_user) {
            notify(
                "SoundHop",
                &format!("Sharing this device's speakers on port {}", listener.port),
                "normal",
            );
        }
        Ok(serde_json::json!({
            "enabled": true,
            "port": listener.port,
            "security": listener.security,
            "module": listener.module_id,
            "created": listener.created,
            "addresses": local_addresses(false),
        }))
    }

    pub fn stop_listener(
        &self,
        notify_user: Option<bool>,
    ) -> Result<serde_json::Value, ControllerError> {
        let mut removed = false;
        let mut note = String::new();
        let mod_id = self.state.lock().unwrap().listener_module;
        let created = self.state.lock().unwrap().listener_created;
        if mod_id > 0 {
            if let Ok(Some(m)) = self.backend.module(mod_id) {
                if created {
                    removed = self.backend.unload_module(m.id, true)?;
                } else {
                    note = "A TCP audio listener not created by SoundHop is still running. Discovery advertisements are off.".to_string();
                }
            }
            let mut st = self.state.lock().unwrap();
            st.listener_module = 0;
            st.listener_created = false;
            st.listener_port = 0;
            st.listener_security.clear();
        }
        self.save();
        if self.should_notify(notify_user) {
            notify(
                "SoundHop",
                "No longer sharing this device's speakers",
                "normal",
            );
        }
        Ok(serde_json::json!({
            "enabled": false,
            "removed": removed,
            "note": note,
        }))
    }

    fn ensure_listener_internal(&self) -> Result<Listener, ControllerError> {
        let cfg = self.config.lock().unwrap().clone();
        let (mod_id, created) = {
            let st = self.state.lock().unwrap();
            (st.listener_module, st.listener_created)
        };
        if mod_id > 0 {
            if let Ok(Some(m)) = self.backend.module(mod_id) {
                if m.name == "module-native-protocol-tcp" {
                    let mod_port = m
                        .arg("port")
                        .and_then(|p| p.parse::<u16>().ok())
                        .unwrap_or(4713);
                    if mod_port == cfg.sharing.port {
                        let actual_sec = listener_security_from_args(&m.args);
                        let mut st = self.state.lock().unwrap();
                        st.listener_port = cfg.sharing.port;
                        st.listener_security = actual_sec.clone();
                        return Ok(Listener {
                            module_id: m.id,
                            created,
                            args: m.args,
                            port: cfg.sharing.port,
                            security: actual_sec,
                        });
                    }
                    if created {
                        let _ = self.backend.unload_module(m.id, true);
                        let mut st = self.state.lock().unwrap();
                        st.listener_module = 0;
                        st.listener_created = false;
                    }
                }
            }
        }
        let listener = self.backend.ensure_tcp_listener(
            cfg.sharing.port,
            &cfg.sharing.security,
            &cfg.sharing.allowed_networks,
        )?;
        {
            let mut st = self.state.lock().unwrap();
            st.listener_module = listener.module_id;
            st.listener_created = listener.created;
            st.listener_port = listener.port;
            st.listener_security = listener.security.clone();
        }
        self.save();
        Ok(listener)
    }

    fn reconcile_listener(&self) {
        let mod_id = self.state.lock().unwrap().listener_module;
        if mod_id == 0 {
            return;
        }
        let valid = self
            .backend
            .module(mod_id)
            .ok()
            .flatten()
            .map(|m| m.name == "module-native-protocol-tcp")
            .unwrap_or(false);
        if !valid {
            let mut st = self.state.lock().unwrap();
            st.listener_module = 0;
            st.listener_created = false;
            st.listener_port = 0;
            st.listener_security.clear();
        }
    }

    // -- Single & Multi-Room Connections ---------------------------------

    fn resolve_live_peer(&self, peer: &Peer, cfg: &Config) -> Peer {
        if !is_own_non_loopback_ip(&peer.host)
            && tcp_probe_rtt(&peer.host, peer.port, Duration::from_millis(800)).is_some()
        {
            return peer.clone();
        }
        for found in discover_once(0.7, cfg.discovery.port, &cfg.device.id) {
            if found.id == peer.id || found.name.eq_ignore_ascii_case(&peer.name) {
                self.remember_peer(&found);
                return found;
            }
        }
        peer.clone()
    }

    pub fn connect(
        &self,
        peer: &Peer,
        opts: ConnectOptions,
    ) -> Result<serde_json::Value, ControllerError> {
        self.reload_from_disk();
        let cfg = self.config.lock().unwrap().clone();
        if opts.keep_local || cfg.connection.keep_local_output {
            let mut m_opts = opts;
            m_opts.keep_local = true;
            return self.connect_multi(std::slice::from_ref(peer), m_opts);
        }
        self.ensure_backend()?;
        let resolved_peer = self.resolve_live_peer(peer, &cfg);
        let peer = &resolved_peer;
        self.validate_peer(peer, &cfg)?;

        if let Some(rtt) = tcp_probe_rtt(&peer.host, peer.port, Duration::from_millis(1500)) {
            self.telemetry.record_rtt(&peer.id, rtt);
        } else {
            return Err(ControllerError::Message(format!(
                "{} is not accepting audio at {}:{}. Is it online and sharing?",
                peer.display_name(),
                peer.host,
                peer.port
            )));
        }

        if self.state.lock().unwrap().is_connected() {
            let _ = self.disconnect_internal(false, false, false, Some(false), true, false, false);
        }

        self.remember_previous_default();
        let sink_name = Self::sink_name_for(peer);
        let selective_default = cfg.routing.default_mode == "selective";
        let switch_default = opts
            .switch_default
            .unwrap_or(cfg.connection.switch_default && !selective_default);
        let move_streams = opts
            .move_streams
            .unwrap_or(cfg.connection.move_streams && !selective_default);

        let module_id = self
            .backend
            .connect_tunnel(&TunnelParams {
                host: &peer.host,
                port: peer.port,
                sink_name: &sink_name,
                description: &peer.display_name(),
                reconnect_delay_ms: cfg.connection.reconnect_delay_ms,
                latency_ms: cfg.connection.latency_ms,
                extra_args: &cfg.connection.extra_tunnel_args,
            })
            .map_err(|e| {
                ControllerError::Message(format!(
                    "Could not create an audio tunnel to {}: {e}",
                    peer.display_name()
                ))
            })?;

        {
            let mut st = self.state.lock().unwrap();
            st.connected_peer = peer.id.clone();
            st.connected_name = peer.display_name();
            st.connected_host = peer.host.clone();
            st.connected_port = peer.port;
            st.connected_sink = sink_name.clone();
            st.connected_module = module_id;
            st.multiroom_peers = vec![ActivePeerSession {
                peer_id: peer.id.clone(),
                name: peer.display_name(),
                host: peer.host.clone(),
                port: peer.port,
                sink_name: sink_name.clone(),
                module_id,
            }];
            st.combine_module = 0;
            st.keep_local = false;
            st.connected_since = now_secs();
            st.connected_by = opts.by.clone();
            st.auto_paused = false;
        }
        self.config.lock().unwrap().connection.last_peer = peer.id.clone();
        self.remember_peer(peer);

        if opts.with_mic {
            let _ = self.enable_mic(&opts.mic_direction);
        }
        self.save();

        if switch_default {
            let _ = self.make_default(&sink_name, move_streams);
        }
        let _ = self.enforce_routed_apps();

        if self.should_notify(opts.notify_user) {
            notify(
                "SoundHop",
                &format!("Audio is now playing on {}", peer.display_name()),
                "normal",
            );
        }
        Ok(self.connection_dict())
    }

    /// Connect to multiple peers simultaneously (or 1 peer + local speakers) using `module-combine-sink`.
    pub fn connect_multi(
        &self,
        peers: &[Peer],
        opts: ConnectOptions,
    ) -> Result<serde_json::Value, ControllerError> {
        if peers.is_empty() {
            return Err(ControllerError::Message(
                "At least one peer is required for connection".to_string(),
            ));
        }
        if peers.len() == 1 && !opts.keep_local {
            return self.connect(&peers[0], opts);
        }
        self.ensure_backend()?;
        let cfg = self.config.lock().unwrap().clone();
        let resolved_peers: Vec<Peer> = peers
            .iter()
            .map(|p| self.resolve_live_peer(p, &cfg))
            .collect();
        let peers = &resolved_peers[..];
        for p in peers {
            self.validate_peer(p, &cfg)?;
            if let Some(rtt) = tcp_probe_rtt(&p.host, p.port, Duration::from_millis(1500)) {
                self.telemetry.record_rtt(&p.id, rtt);
            } else {
                return Err(ControllerError::Message(format!(
                    "{} is not accepting audio at {}:{}",
                    p.display_name(),
                    p.host,
                    p.port
                )));
            }
        }

        if self.state.lock().unwrap().is_connected() {
            let _ = self.disconnect_internal(false, false, false, Some(false), true, false, false);
        }
        self.remember_previous_default();

        let mut sessions = Vec::new();
        let mut slave_sinks = Vec::new();
        if opts.keep_local {
            let prev = self.state.lock().unwrap().previous_default_sink.clone();
            let local_sink = self.local_default_candidate(&prev);
            if !local_sink.is_empty() {
                slave_sinks.push(local_sink);
            }
        }

        for p in peers {
            let sname = Self::sink_name_for(p);
            let mid = self.backend.connect_tunnel(&TunnelParams {
                host: &p.host,
                port: p.port,
                sink_name: &sname,
                description: &p.display_name(),
                reconnect_delay_ms: cfg.connection.reconnect_delay_ms,
                latency_ms: cfg.connection.latency_ms,
                extra_args: &cfg.connection.extra_tunnel_args,
            })?;
            slave_sinks.push(sname.clone());
            sessions.push(ActivePeerSession {
                peer_id: p.id.clone(),
                name: p.display_name(),
                host: p.host.clone(),
                port: p.port,
                sink_name: sname,
                module_id: mid,
            });
            self.remember_peer(p);
        }

        let names_joined: Vec<String> = sessions.iter().map(|s| s.name.clone()).collect();
        let label = names_joined.join("+");
        let combine_mod =
            self.backend
                .create_combined_sink(MULTIROOM_SINK_NAME, &label, &slave_sinks)?;

        let first = sessions[0].clone();
        {
            let mut st = self.state.lock().unwrap();
            st.connected_peer = first.peer_id.clone();
            st.connected_name = if opts.keep_local {
                format!("{label} + Local")
            } else {
                label.clone()
            };
            st.connected_host = first.host.clone();
            st.connected_port = first.port;
            st.connected_sink = MULTIROOM_SINK_NAME.to_string();
            st.connected_module = combine_mod;
            st.multiroom_peers = sessions;
            st.combine_module = combine_mod;
            st.keep_local = opts.keep_local;
            st.connected_since = now_secs();
            st.connected_by = opts.by;
            st.auto_paused = false;
        }
        self.config.lock().unwrap().connection.last_peer = first.peer_id.clone();
        self.save();

        let switch_default = opts.switch_default.unwrap_or(cfg.connection.switch_default);
        let move_streams = opts.move_streams.unwrap_or(cfg.connection.move_streams);
        if switch_default {
            let _ = self.make_default(MULTIROOM_SINK_NAME, move_streams);
        }
        let _ = self.enforce_routed_apps();

        if self.should_notify(opts.notify_user) {
            notify(
                "SoundHop Multi-Room",
                &format!("Broadcasting audio to {label}"),
                "normal",
            );
        }
        Ok(self.connection_dict())
    }

    fn validate_peer(&self, peer: &Peer, cfg: &Config) -> Result<(), ControllerError> {
        if peer.id.trim().is_empty() {
            return Err(ControllerError::Message(
                "Invalid peer: missing device id".to_string(),
            ));
        }
        if peer.id == cfg.device.id {
            return Err(ControllerError::Message("That is this device".to_string()));
        }
        if !is_private_address(&peer.host) || is_own_non_loopback_ip(&peer.host) {
            return Err(ControllerError::Message(format!(
                "Refusing to connect to invalid or self-owned address {}",
                peer.host
            )));
        }
        if !peer.compatible() {
            return Err(ControllerError::Message(format!(
                "{} speaks SoundHop protocol {}; this version speaks {}. Update both devices.",
                peer.display_name(),
                peer.protocol,
                PROTOCOL_VERSION
            )));
        }
        Ok(())
    }

    pub fn disconnect(
        &self,
        forget: bool,
        restore: Option<bool>,
        pause_auto: bool,
        notify_user: Option<bool>,
        all_tunnels: bool,
        stop_android: bool,
    ) -> Result<serde_json::Value, ControllerError> {
        self.reload_from_disk();
        let cfg_restore = self.config.lock().unwrap().connection.restore_default;
        self.disconnect_internal(
            forget,
            restore.unwrap_or(cfg_restore),
            pause_auto,
            notify_user,
            false,
            all_tunnels,
            stop_android,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn disconnect_internal(
        &self,
        forget: bool,
        restore: bool,
        pause_auto: bool,
        notify_user: Option<bool>,
        keep_previous: bool,
        all_tunnels: bool,
        stop_android: bool,
    ) -> Result<serde_json::Value, ControllerError> {
        let mut removed = Vec::new();
        if (stop_android || all_tunnels) && self.android_session.is_active() {
            let _ = self.android_session.stop();
        }
        let st_snapshot = self.state.lock().unwrap().clone();
        if st_snapshot.combine_module > 0 {
            if let Ok(true) = self.backend.unload_module(st_snapshot.combine_module, true) {
                removed.push(st_snapshot.combine_module);
            }
        }
        for sess in &st_snapshot.multiroom_peers {
            if sess.module_id > 0 {
                if let Ok(true) = self.backend.unload_module(sess.module_id, true) {
                    removed.push(sess.module_id);
                }
            }
        }
        if st_snapshot.connected_module > 0 && !removed.contains(&st_snapshot.connected_module) {
            if let Ok(true) = self
                .backend
                .unload_module(st_snapshot.connected_module, true)
            {
                removed.push(st_snapshot.connected_module);
            }
        }
        if !st_snapshot.connected_source.is_empty() || !st_snapshot.mic_direction.is_empty() {
            let _ = self.disable_mic();
        }
        if let Ok(extra) = self.backend.unload_tunnels(SINK_PREFIX) {
            for id in extra {
                if !removed.contains(&id) {
                    removed.push(id);
                }
            }
        }

        let mut restored_to = String::new();
        if restore {
            let candidate = self.local_default_candidate(&st_snapshot.previous_default_sink);
            if !candidate.is_empty() && self.backend.set_default_sink(&candidate).is_ok() {
                restored_to = candidate.clone();
                if self.config.lock().unwrap().connection.move_streams {
                    let _ = self.backend.move_sink_inputs(&candidate, None);
                }
            }
        }

        {
            let mut st = self.state.lock().unwrap();
            st.clear_connection();
            if !keep_previous {
                st.previous_default_sink.clear();
                st.previous_default_source.clear();
            }
            if forget {
                let mut cfg = self.config.lock().unwrap();
                if cfg.connection.last_peer == st_snapshot.connected_peer {
                    cfg.connection.last_peer.clear();
                }
                if !st_snapshot.connected_peer.is_empty() {
                    st.forget_peer(&st_snapshot.connected_peer);
                }
            }
            if pause_auto {
                st.auto_paused = true;
            }
        }
        self.save();

        if (!removed.is_empty() || !restored_to.is_empty()) && self.should_notify(notify_user) {
            if !restored_to.is_empty() {
                notify(
                    "SoundHop",
                    &format!("Audio restored to {restored_to}"),
                    "normal",
                );
            } else {
                notify("SoundHop", "Remote audio disconnected", "normal");
            }
        }

        Ok(serde_json::json!({
            "disconnected": true,
            "peer": st_snapshot.connected_name,
            "removed_modules": removed,
            "restored_to": restored_to,
            "forgotten": forget,
            "auto_paused": self.state.lock().unwrap().auto_paused,
        }))
    }

    // -- Pairing (PIN / PSK v2 feature) ----------------------------------

    pub fn pair_peer(
        &self,
        peer_id_or_name: &str,
        secret: Option<&str>,
    ) -> Result<serde_json::Value, ControllerError> {
        let pin_or_psk = secret
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(generate_pairing_pin);
        let shared_key = derive_key_hex(&pin_or_psk, "soundhop-pair");
        let cookie = derive_pa_cookie(&shared_key);
        let cookie_path = write_pa_cookie(None, &cookie)
            .map_err(|e| ControllerError::Message(format!("Failed writing PA cookie: {e}")))?;
        let cookie_hex = hex::encode(cookie);

        let peer_name = self
            .state
            .lock()
            .unwrap()
            .known_peers
            .get(peer_id_or_name)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| peer_id_or_name.to_string());

        let paired = PairedPeer {
            peer_id: peer_id_or_name.to_string(),
            name: peer_name.clone(),
            shared_key_hex: shared_key.clone(),
            cookie_hex,
            paired_at: now_secs(),
        };
        self.state
            .lock()
            .unwrap()
            .paired_peers
            .insert(peer_id_or_name.to_string(), paired);
        {
            let mut cfg = self.config.lock().unwrap();
            cfg.sharing.psk_hash = shared_key.clone();
            if cfg.sharing.security == "trusted-lan" {
                cfg.sharing.security = "psk".to_string();
            }
        }
        self.save();

        Ok(serde_json::json!({
            "paired": true,
            "peer_id": peer_id_or_name,
            "name": peer_name,
            "pin_or_psk": pin_or_psk,
            "shared_key_fingerprint": &shared_key[..16],
            "cookie_path": cookie_path.to_string_lossy(),
        }))
    }

    // -- Auto-Connect Controls -------------------------------------------

    pub fn set_auto(&self, enabled: bool) -> serde_json::Value {
        self.config.lock().unwrap().connection.auto_connect = enabled;
        if enabled {
            self.state.lock().unwrap().auto_paused = false;
        }
        self.save();
        let paused = self.state.lock().unwrap().auto_paused;
        serde_json::json!({
            "auto_connect": enabled,
            "paused": paused,
        })
    }

    pub fn pause_auto(&self, paused: bool) -> serde_json::Value {
        self.state.lock().unwrap().auto_paused = paused;
        self.save();
        serde_json::json!({ "paused": paused })
    }

    // -- Microphone Pass-Through -----------------------------------------

    pub fn enable_mic(&self, direction: &str) -> Result<serde_json::Value, ControllerError> {
        let st_snap = self.state.lock().unwrap().clone();
        if !st_snap.is_connected() {
            return Err(ControllerError::Message(
                "Not connected to a remote speaker".to_string(),
            ));
        }
        let dir_val = if matches!(direction, "send" | "out" | "tx" | "to_remote") {
            "send"
        } else {
            "receive"
        };
        if !st_snap.connected_source.is_empty() {
            if st_snap.mic_direction == dir_val {
                return Ok(serde_json::json!({
                    "enabled": true,
                    "direction": dir_val,
                    "source": st_snap.connected_source,
                }));
            }
            let _ = self.disable_mic();
        }

        if dir_val == "send" {
            let dev_name = self.config.lock().unwrap().device.name.clone();
            let (r_null, r_remap, l_tun, l_loop, r_src) = self.backend.send_mic_to_remote(
                &st_snap.connected_host,
                st_snap.connected_port,
                &dev_name,
                "@DEFAULT_SOURCE@",
            )?;
            let mut st = self.state.lock().unwrap();
            st.connected_source = r_src.clone();
            st.connected_source_module = l_tun;
            st.remote_mic_null_module = r_null;
            st.remote_mic_remap_module = r_remap;
            st.local_mic_loopback_module = l_loop;
            st.mic_direction = "send".to_string();
            drop(st);
            self.save();
            Ok(serde_json::json!({
                "enabled": true,
                "direction": "send",
                "source": r_src,
                "module_id": l_tun,
            }))
        } else {
            self.remember_previous_default_source();
            let peer = Peer {
                id: st_snap.connected_peer.clone(),
                name: st_snap.connected_name.clone(),
                host: st_snap.connected_host.clone(),
                port: st_snap.connected_port,
                version: VERSION.to_string(),
                protocol: PROTOCOL_VERSION,
                security: "trusted-lan".to_string(),
                platform: "linux".to_string(),
                last_seen: now_secs(),
                source: "udp".to_string(),
                rtt_ms: None,
            };
            let source_name = Self::source_name_for(&peer);
            let mod_id = self.backend.connect_tunnel_source(&SourceTunnelParams {
                host: &peer.host,
                port: peer.port,
                source_name: &source_name,
                description: &peer.display_name(),
                remote_source: "",
                reconnect_delay_ms: 3000,
                latency_ms: None,
                extra_args: &[],
            })?;
            {
                let mut st = self.state.lock().unwrap();
                st.connected_source = source_name.clone();
                st.connected_source_module = mod_id;
                st.mic_direction = "receive".to_string();
            }
            let _ = self.backend.set_default_source(&source_name);
            let _ = self.backend.move_source_outputs(&source_name, None, &[]);
            self.save();
            Ok(serde_json::json!({
                "enabled": true,
                "direction": "receive",
                "source": source_name,
                "module_id": mod_id,
            }))
        }
    }

    pub fn disable_mic(&self) -> Result<serde_json::Value, ControllerError> {
        let st_snap = self.state.lock().unwrap().clone();
        if st_snap.connected_source.is_empty() && st_snap.mic_direction.is_empty() {
            return Ok(serde_json::json!({ "enabled": false }));
        }
        if st_snap.mic_direction == "send" {
            self.backend.stop_send_mic_to_remote(
                &st_snap.connected_host,
                st_snap.connected_port,
                st_snap.remote_mic_null_module,
                st_snap.remote_mic_remap_module,
                st_snap.connected_source_module,
                st_snap.local_mic_loopback_module,
            );
        } else {
            let local_src = self.local_default_source_candidate(&st_snap.previous_default_source);
            if !local_src.is_empty() {
                let _ = self.backend.set_default_source(&local_src);
                let _ = self.backend.move_source_outputs(
                    &local_src,
                    Some(&st_snap.connected_source),
                    &[],
                );
            }
            if st_snap.connected_source_module > 0 {
                let _ = self
                    .backend
                    .unload_module(st_snap.connected_source_module, true);
            }
            let _ = self
                .backend
                .disconnect_tunnel_source(&st_snap.connected_source);
        }
        {
            let mut st = self.state.lock().unwrap();
            st.connected_source.clear();
            st.connected_source_module = 0;
            st.remote_mic_null_module = 0;
            st.remote_mic_remap_module = 0;
            st.local_mic_loopback_module = 0;
            st.mic_direction.clear();
        }
        self.save();
        Ok(serde_json::json!({ "enabled": false }))
    }

    // -- Volume Management & Sync ----------------------------------------

    pub fn get_volume(&self) -> serde_json::Value {
        let st = self.state.lock().unwrap().clone();
        let sink = if !st.connected_sink.is_empty() {
            st.connected_sink.clone()
        } else {
            self.backend.default_sink().unwrap_or_default()
        };
        let vol = self.backend.get_sink_volume(&sink);
        let mute = self.backend.get_sink_mute(&sink);
        serde_json::json!({
            "volume": vol,
            "mute": mute,
            "sink": sink,
            "connected": st.is_connected(),
        })
    }

    pub fn set_volume(
        &self,
        volume: Option<u32>,
        mute: Option<bool>,
        sync_remote: bool,
    ) -> serde_json::Value {
        let st = self.state.lock().unwrap().clone();
        let sink = if !st.connected_sink.is_empty() {
            st.connected_sink.clone()
        } else {
            self.backend.default_sink().unwrap_or_default()
        };
        if let Some(v) = volume {
            self.backend.set_sink_volume(&sink, v);
        }
        if let Some(m) = mute {
            self.backend.set_sink_mute(&sink, m);
        }
        if sync_remote && st.is_connected() && !st.connected_host.is_empty() {
            let cur_vol = volume.unwrap_or_else(|| self.backend.get_sink_volume(&sink));
            let cur_mute = mute.unwrap_or_else(|| self.backend.get_sink_mute(&sink));
            for sess in &st.multiroom_peers {
                self.backend
                    .remote_set_volume(&sess.host, sess.port, cur_vol, Some(cur_mute));
            }
            if st.multiroom_peers.is_empty() {
                self.backend.remote_set_volume(
                    &st.connected_host,
                    st.connected_port,
                    cur_vol,
                    Some(cur_mute),
                );
            }
            let mut guard = self.state.lock().unwrap();
            guard.last_synced_volume = cur_vol as i32;
            guard.last_synced_mute = cur_mute;
        }
        self.get_volume()
    }

    pub fn sync_remote_volume(&self) -> bool {
        let st = self.state.lock().unwrap().clone();
        if !st.is_connected() || st.connected_host.is_empty() {
            return false;
        }
        let sink = if !st.connected_sink.is_empty() {
            st.connected_sink.clone()
        } else {
            self.backend.default_sink().unwrap_or_default()
        };
        let vol = self.backend.get_sink_volume(&sink);
        let mute = self.backend.get_sink_mute(&sink);
        if vol as i32 == st.last_synced_volume && mute == st.last_synced_mute {
            return false;
        }
        self.backend
            .remote_set_volume(&st.connected_host, st.connected_port, vol, Some(mute));
        let mut guard = self.state.lock().unwrap();
        guard.last_synced_volume = vol as i32;
        guard.last_synced_mute = mute;
        true
    }

    // -- App Stream Routing (with v2 Persistent Rules) -------------------

    pub fn list_apps(&self) -> Vec<serde_json::Value> {
        let st = self.state.lock().unwrap().clone();
        let cfg = self.config.lock().unwrap().clone();
        let inputs = self.backend.detailed_sink_inputs().unwrap_or_default();
        let mut out = Vec::new();
        let mut seen_keys = std::collections::HashSet::new();

        for item in inputs {
            let key = if !item.app_name.is_empty() {
                item.app_name.to_lowercase()
            } else {
                item.binary.to_lowercase()
            };
            seen_keys.insert(key.clone());
            let mut rule = st.routed_apps.get(&key).cloned().unwrap_or_default();
            let persistent = if cfg.routing.always_remote.contains(&key) {
                rule = "remote".to_string();
                true
            } else if cfg.routing.always_local.contains(&key) {
                rule = "local".to_string();
                true
            } else {
                false
            };
            out.push(serde_json::json!({
                "id": item.id,
                "sink": item.sink,
                "sink_name": item.sink_name,
                "app_name": item.app_name,
                "binary": item.binary,
                "node_name": item.node_name,
                "pid": item.pid,
                "media_name": item.media_name,
                "icon_name": item.icon_name,
                "volume_percent": item.volume_percent,
                "mute": item.mute,
                "is_remote": item.is_remote,
                "routed_rule": rule,
                "persistent": persistent,
            }));
        }

        // Include persistent config rules even if not currently playing
        for remote_app in &cfg.routing.always_remote {
            if !seen_keys.contains(remote_app) {
                seen_keys.insert(remote_app.clone());
                out.push(serde_json::json!({
                    "id": 0,
                    "sink": 0,
                    "sink_name": "",
                    "app_name": remote_app,
                    "binary": remote_app,
                    "node_name": remote_app,
                    "pid": 0,
                    "media_name": "Persistent remote rule",
                    "icon_name": remote_app,
                    "volume_percent": 100,
                    "mute": false,
                    "is_remote": true,
                    "routed_rule": "remote",
                    "persistent": true,
                }));
            }
        }
        for local_app in &cfg.routing.always_local {
            if !seen_keys.contains(local_app) {
                seen_keys.insert(local_app.clone());
                out.push(serde_json::json!({
                    "id": 0,
                    "sink": 0,
                    "sink_name": "",
                    "app_name": local_app,
                    "binary": local_app,
                    "node_name": local_app,
                    "pid": 0,
                    "media_name": "Persistent local rule",
                    "icon_name": local_app,
                    "volume_percent": 100,
                    "mute": false,
                    "is_remote": false,
                    "routed_rule": "local",
                    "persistent": true,
                }));
            }
        }
        out
    }

    pub fn route_app(
        &self,
        identifier: &str,
        target: Option<&str>,
        save_persistent: bool,
    ) -> Result<serde_json::Value, ControllerError> {
        let id_str = identifier.trim();
        let mut app_rule_key = id_str.to_string();
        for inp in self.backend.detailed_sink_inputs().unwrap_or_default() {
            if inp.id.to_string() == id_str {
                app_rule_key = inp.display_name();
                break;
            }
        }
        let st_snap = self.state.lock().unwrap().clone();
        let target_raw = target.unwrap_or("remote").trim();
        let (target_sink, target_dest) = if target_raw.is_empty() || target_raw == "remote" {
            if st_snap.connected_sink.is_empty() && !save_persistent {
                return Err(ControllerError::Message(
                    "Not connected to a remote speaker".to_string(),
                ));
            }
            (st_snap.connected_sink.clone(), "remote".to_string())
        } else if matches!(target_raw, "local" | "default") {
            let mut loc = self.local_default_candidate(&st_snap.previous_default_sink);
            if loc.is_empty() {
                loc = self.backend.default_sink().unwrap_or_default();
            }
            (loc, "local".to_string())
        } else {
            (target_raw.to_string(), target_raw.to_string())
        };

        let key_lower = app_rule_key.to_lowercase();
        {
            let mut st = self.state.lock().unwrap();
            if target_dest == "local" {
                st.routed_apps.remove(&key_lower);
                st.routed_apps.remove(&id_str.to_lowercase());
            } else {
                st.routed_apps
                    .insert(key_lower.clone(), target_dest.clone());
            }
        }
        if save_persistent {
            let mut cfg = self.config.lock().unwrap();
            cfg.routing.always_remote.retain(|x| x != &key_lower);
            cfg.routing.always_local.retain(|x| x != &key_lower);
            if target_dest == "remote" {
                cfg.routing.always_remote.push(key_lower.clone());
            } else if target_dest == "local" {
                cfg.routing.always_local.push(key_lower.clone());
            }
        }
        self.save();

        let moved = if !target_sink.is_empty() {
            self.backend.move_app_streams(id_str, &target_sink)?
        } else {
            0
        };

        Ok(serde_json::json!({
            "identifier": id_str,
            "app": app_rule_key,
            "target_sink": target_sink,
            "target_dest": target_dest,
            "moved": moved,
            "remembered": true,
            "persistent": save_persistent,
        }))
    }

    pub fn enforce_routed_apps(&self) -> usize {
        let st = self.state.lock().unwrap().clone();
        let cfg = self.config.lock().unwrap().clone();
        let mut moved = 0;
        let local_sink = self.local_default_candidate(&st.previous_default_sink);

        // 1. Enforce persistent config.routing rules
        if !st.connected_sink.is_empty() {
            for app in &cfg.routing.always_remote {
                moved += self
                    .backend
                    .move_app_streams(app, &st.connected_sink)
                    .unwrap_or(0);
            }
        }
        if !local_sink.is_empty() {
            for app in &cfg.routing.always_local {
                moved += self.backend.move_app_streams(app, &local_sink).unwrap_or(0);
            }
        }

        // 2. Enforce session state.routed_apps rules
        for (app_key, dest) in &st.routed_apps {
            if dest == "remote" && !st.connected_sink.is_empty() {
                moved += self
                    .backend
                    .move_app_streams(app_key, &st.connected_sink)
                    .unwrap_or(0);
            } else if dest == "local" && !local_sink.is_empty() {
                moved += self
                    .backend
                    .move_app_streams(app_key, &local_sink)
                    .unwrap_or(0);
            }
        }
        moved
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
        let st_snap = self.state.lock().unwrap().clone();
        if st_snap.is_connected() {
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
                self.state.lock().unwrap().clear_connection();
                actions.push("cleared stale connection state".to_string());
                if self.config.lock().unwrap().connection.restore_default {
                    let cand = self.local_default_candidate(&st_snap.previous_default_sink);
                    if !cand.is_empty() && self.backend.set_default_sink(&cand).is_ok() {
                        actions.push(format!("restored default sink to {cand}"));
                    }
                }
                self.save();
            }
        }
        self.reconcile_listener();
        let sharing_enabled = self.config.lock().unwrap().sharing.enabled;
        let has_listener = self.state.lock().unwrap().listener_module > 0;
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

    // -- Android Helpers -------------------------------------------------

    pub fn android_tool_status(&self) -> serde_json::Value {
        let st = self.android_manager.get_tool_status();
        serde_json::json!({
            "scrcpy_path": st.scrcpy_path,
            "scrcpy_version": st.scrcpy_version,
            "adb_path": st.adb_path,
            "adb_version": st.adb_version,
            "is_managed": st.is_managed,
            "ready": st.ready(),
        })
    }

    pub fn android_list_devices(&self) -> Vec<serde_json::Value> {
        let devs = self.android_manager.list_devices();
        let now = now_secs();
        {
            let mut st = self.state.lock().unwrap();
            for d in &devs {
                st.remember_android_device(
                    &d.serial,
                    &d.model,
                    &d.manufacturer,
                    if d.is_wifi { &d.ip } else { "" },
                    if d.is_wifi { d.port } else { 5555 },
                    d.is_wifi,
                    false,
                    Some(now),
                );
            }
        }
        self.save();
        devs.into_iter()
            .filter_map(|d| serde_json::to_value(d).ok())
            .collect()
    }

    pub fn android_history(&self) -> Vec<serde_json::Value> {
        let st = self.state.lock().unwrap();
        let mut devs: Vec<_> = st.known_android_devices.values().cloned().collect();
        devs.sort_by(|a, b| {
            b.last_connected
                .max(b.last_seen)
                .partial_cmp(&a.last_connected.max(a.last_seen))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        devs.into_iter()
            .filter_map(|d| serde_json::to_value(d).ok())
            .collect()
    }

    // -- Internal Helpers ------------------------------------------------

    fn ensure_backend(&self) -> Result<(), ControllerError> {
        if !self.backend.available() {
            return Err(ControllerError::Message(
                "PipeWire/PulseAudio is not reachable through pactl. Install pipewire-pulse or pulseaudio and try again.".to_string(),
            ));
        }
        Ok(())
    }

    fn should_notify(&self, explicit: Option<bool>) -> bool {
        if let Some(exp) = explicit {
            return exp;
        }
        self.notify_enabled && self.config.lock().unwrap().ui.notifications
    }

    fn remember_previous_default(&self) {
        let current = self.backend.default_sink().unwrap_or_default();
        let mut st = self.state.lock().unwrap();
        if !st.previous_default_sink.is_empty()
            && !st.previous_default_sink.starts_with(SINK_PREFIX)
            && self
                .backend
                .sink_exists(&st.previous_default_sink)
                .unwrap_or(false)
        {
            return;
        }
        if !current.is_empty()
            && !current.starts_with(SINK_PREFIX)
            && self.backend.sink_exists(&current).unwrap_or(false)
        {
            st.previous_default_sink = current;
        }
    }

    fn remember_previous_default_source(&self) {
        let current = self.backend.default_source().unwrap_or_default();
        let mut st = self.state.lock().unwrap();
        if !current.is_empty()
            && !current.starts_with(SOURCE_PREFIX)
            && !current.ends_with(".monitor")
        {
            st.previous_default_source = current;
        }
    }

    pub fn local_default_candidate(&self, preferred: &str) -> String {
        if !preferred.is_empty()
            && !preferred.starts_with(SINK_PREFIX)
            && self.backend.sink_exists(preferred).unwrap_or(false)
        {
            return preferred.to_string();
        }
        let mut sinks: Vec<_> = self
            .backend
            .sinks()
            .unwrap_or_default()
            .into_iter()
            .filter(|s| !s.name.starts_with(SINK_PREFIX))
            .collect();
        if sinks.is_empty() {
            return String::new();
        }
        sinks.sort_by_key(|s| {
            let n = s.name.to_lowercase();
            if n.contains("speaker") || n.contains("analog-stereo") || n.contains("headphone") {
                0
            } else if n.contains("usb") || n.contains("pci") {
                1
            } else {
                2
            }
        });
        sinks[0].name.clone()
    }

    fn local_default_source_candidate(&self, preferred: &str) -> String {
        if !preferred.is_empty()
            && !preferred.starts_with(SOURCE_PREFIX)
            && !preferred.ends_with(".monitor")
            && self.backend.source_exists(preferred).unwrap_or(false)
        {
            return preferred.to_string();
        }
        self.backend
            .sources()
            .unwrap_or_default()
            .into_iter()
            .find(|s| !s.name.starts_with(SOURCE_PREFIX) && !s.name.ends_with(".monitor"))
            .map(|s| s.name)
            .unwrap_or_default()
    }

    fn make_default(&self, sink_name: &str, move_streams: bool) -> Result<(), ControllerError> {
        self.backend.set_default_sink(sink_name)?;
        if move_streams {
            let _ = self.backend.move_sink_inputs(sink_name, None);
        }
        Ok(())
    }

    fn connection_dict(&self) -> serde_json::Value {
        let st = self.state.lock().unwrap();
        serde_json::json!({
            "connected": st.is_connected(),
            "peer_id": st.connected_peer,
            "name": st.connected_name,
            "host": st.connected_host,
            "port": st.connected_port,
            "sink": st.connected_sink,
            "module": st.connected_module,
            "multiroom": st.is_multiroom(),
            "multiroom_peers": st.multiroom_peers,
            "keep_local": st.keep_local,
            "source": st.connected_source,
            "with_mic": !st.connected_source.is_empty(),
            "mic_direction": st.mic_direction,
            "since": st.connected_since,
            "by": st.connected_by,
        })
    }

    pub fn find_module(&self, module_id: u32) -> Option<Module> {
        self.backend.module(module_id).ok().flatten()
    }

    pub fn listener_running(&self) -> bool {
        let mod_id = self.state.lock().unwrap().listener_module;
        if mod_id == 0 {
            return false;
        }
        let _ = tcp_reachable;
        self.find_module(mod_id)
            .map(|m| m.name == "module-native-protocol-tcp")
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::linux::LinuxAudioBackend;
    use crate::util::{CommandResult, MockRunner};

    #[test]
    fn test_controller_pair_and_persistent_routing() {
        let runner = Arc::new(MockRunner::new(|args, _| {
            let joined = args.join(" ");
            let stdout = if joined.contains("info") {
                "Server Name: PulseAudio (on PipeWire 1.2.0)\nDefault Sink: alsa_output.pci\n"
            } else if joined.contains("list short sinks") {
                "1\talsa_output.pci\tPipeWire\ts16le 2ch 48000Hz\tRUNNING\n"
            } else {
                "10\n"
            };
            Ok(CommandResult {
                args: args.to_vec(),
                returncode: 0,
                stdout: stdout.to_string(),
                stderr: String::new(),
            })
        }));
        let backend = Arc::new(LinuxAudioBackend::with_runner(runner));
        let ctrl = Controller::new(
            Some(Config::default()),
            Some(State::default()),
            Some(backend),
            None,
            None,
            false,
        );
        ctrl.set_save_enabled(false);

        let route_res = ctrl.route_app("spotify", Some("remote"), true).unwrap();
        assert_eq!(route_res["persistent"], true);
        assert!(ctrl
            .config
            .lock()
            .unwrap()
            .routing
            .always_remote
            .contains(&"spotify".to_string()));
    }
}
