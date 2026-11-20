//! Plain-language UI state snapshots, view models, and status hydration.

use std::collections::HashSet;
use std::time::Instant;

use soundhop_core::android::{AdbDevice, ToolStatus};
use soundhop_core::config::Config;
use soundhop_core::controller::Controller;
use soundhop_core::discovery::Peer;
use soundhop_core::state::{ActivePeerSession, State};
use soundhop_core::telemetry::TelemetrySnapshot;
use soundhop_core::util::{is_own_non_loopback_ip, is_private_address};

use crate::lock_or_recover;

#[derive(Debug, Clone)]
pub struct PeerView {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub platform: String,
    pub security: String,
    pub rtt_ms: Option<f64>,
    pub online: bool,
    pub paired: bool,
}

impl PeerView {
    pub fn signal_label(&self) -> &'static str {
        if !self.online {
            return "Saved (offline)";
        }
        match self.rtt_ms {
            Some(r) if r < 8.0 => "Excellent signal",
            Some(r) if r < 25.0 => "Good signal",
            Some(_) => "Fair signal",
            None => "Ready to connect",
        }
    }

    pub fn platform_label(&self) -> &'static str {
        if self.platform.eq_ignore_ascii_case("windows") {
            "Windows"
        } else {
            "Linux"
        }
    }
}

#[derive(Debug, Clone)]
pub struct AppStreamView {
    pub app_name: String,
    pub media_name: String,
    pub is_remote: bool,
    pub routed_rule: String,
    pub persistent: bool,
    pub active: bool,
}

#[derive(Debug, Clone)]
pub struct GuiToast {
    pub message: String,
    pub is_error: bool,
    pub created_at: Instant,
}

#[derive(Debug, Clone)]
pub struct GuiSnapshot {
    pub daemon_connected: bool,
    pub config: Config,
    pub connected: bool,
    pub multiroom: bool,
    pub connected_peer_id: String,
    pub connected_name: String,
    pub connected_host: String,
    pub connected_port: u16,
    pub connected_duration: String,
    pub multiroom_peers: Vec<ActivePeerSession>,
    pub keep_local: bool,
    pub sharing_enabled: bool,
    pub auto_connect: bool,
    pub auto_paused: bool,
    pub volume_level: u32,
    pub volume_muted: bool,
    pub mic_enabled: bool,
    pub mic_direction: String,
    pub peers: Vec<PeerView>,
    pub apps: Vec<AppStreamView>,
    pub audio_active: bool,
    pub audio_dbfs: f32,
    pub audio_peak: f32,
    pub rtt_ms: Option<f64>,
    pub jitter_ms: Option<f64>,
    pub android_tools: ToolStatus,
    pub android_devices: Vec<AdbDevice>,
    pub android_session_active: bool,
    pub android_session_device: String,
    pub android_installing: bool,
    pub android_starting: bool,
    pub firewall_kind: String,
    pub firewall_active: bool,
    pub firewall_detail: String,
    pub service_running: bool,
    pub service_enabled: bool,
    pub autostart_installed: bool,
    pub busy_message: Option<String>,
    pub refreshing_peers: bool,
    pub refreshing_android: bool,
    pub pending_peer_id: Option<String>,
    pub show_onboarding: bool,
    pub listener_security: String,
    pub paired_peer_count: usize,
    pub hydrated: bool,
}

impl GuiSnapshot {
    pub fn from_controller(controller: &Controller) -> Self {
        let cfg = lock_or_recover(&controller.config).clone();
        let st = lock_or_recover(&controller.state).clone();

        Self {
            daemon_connected: false,
            show_onboarding: !cfg.ui.onboarding_completed && st.known_peers.is_empty(),
            listener_security: st.listener_security.clone(),
            paired_peer_count: st.paired_peers.len(),
            config: cfg.clone(),
            connected: st.is_connected(),
            multiroom: st.is_multiroom(),
            connected_peer_id: st.connected_peer.clone(),
            connected_name: st.connected_name.clone(),
            connected_host: st.connected_host.clone(),
            connected_port: st.connected_port,
            connected_duration: String::new(),
            multiroom_peers: st.multiroom_peers.clone(),
            keep_local: st.keep_local,
            sharing_enabled: cfg.sharing.enabled || st.listener_module > 0,
            auto_connect: cfg.connection.auto_connect,
            auto_paused: st.auto_paused,
            volume_level: 100,
            volume_muted: false,
            mic_enabled: !st.connected_source.is_empty(),
            mic_direction: st.mic_direction.clone(),
            peers: Vec::new(),
            apps: Vec::new(),
            audio_active: false,
            audio_dbfs: -60.0,
            audio_peak: 0.0,
            rtt_ms: None,
            jitter_ms: None,
            android_tools: ToolStatus {
                scrcpy_path: None,
                scrcpy_version: String::new(),
                adb_path: None,
                adb_version: String::new(),
                is_managed: false,
            },
            android_devices: Vec::new(),
            android_session_active: st.android_active,
            android_session_device: st.android_model,
            android_installing: false,
            android_starting: false,
            firewall_kind: "none".to_string(),
            firewall_active: false,
            firewall_detail: String::new(),
            service_running: false,
            service_enabled: false,
            autostart_installed: false,
            busy_message: None,
            refreshing_peers: false,
            refreshing_android: false,
            pending_peer_id: None,
            hydrated: false,
        }
    }

    pub fn output_headline(&self) -> String {
        if self.multiroom && !self.multiroom_peers.is_empty() {
            format!("Playing on {} speakers", self.multiroom_peers.len())
        } else if self.connected && !self.connected_name.is_empty() {
            format!("Playing on {}", self.connected_name)
        } else {
            "Playing on This Device".to_string()
        }
    }

    pub fn output_subtitle(&self) -> String {
        if self.multiroom && !self.multiroom_peers.is_empty() {
            let names: Vec<&str> = self
                .multiroom_peers
                .iter()
                .map(|p| p.name.as_str())
                .collect();
            if self.keep_local {
                format!("{} + This device", names.join(", "))
            } else {
                names.join(", ")
            }
        } else if self.connected {
            if let Some(rtt) = self.rtt_ms {
                if rtt < 15.0 {
                    "Connected • Great signal".to_string()
                } else {
                    "Connected • Wireless audio active".to_string()
                }
            } else {
                "Connected • Streaming wirelessly".to_string()
            }
        } else if self.sharing_enabled {
            format!(
                "Local speakers active • Visible as \"{}\"",
                self.config.device.name
            )
        } else {
            "Local speakers active".to_string()
        }
    }

    pub fn is_peer_connected(&self, peer_id: &str) -> bool {
        if self.multiroom_peers.iter().any(|p| p.peer_id == peer_id) {
            return true;
        }
        self.connected && self.connected_peer_id == peer_id
    }
}

pub fn security_friendly_label(mode: &str) -> &'static str {
    match mode {
        "psk" => "PIN-Paired Devices Only",
        "cookie" => "System Audio Cookie",
        "open" => "Open (No Check)",
        _ => "Home Wi-Fi (Automatic)",
    }
}

pub(super) struct RefreshUpdate {
    daemon_connected: bool,
    show_onboarding_initial: bool,
    config: Config,
    connected: bool,
    multiroom: bool,
    connected_peer_id: String,
    connected_name: String,
    connected_host: String,
    connected_port: u16,
    connected_duration: String,
    multiroom_peers: Vec<ActivePeerSession>,
    keep_local: bool,
    sharing_enabled: bool,
    auto_connect: bool,
    auto_paused: bool,
    volume_level: u32,
    volume_muted: bool,
    mic_enabled: bool,
    mic_direction: String,
    peers: Vec<PeerView>,
    apps: Vec<AppStreamView>,
    audio_active: bool,
    audio_dbfs: f32,
    audio_peak: f32,
    rtt_ms: Option<f64>,
    jitter_ms: Option<f64>,
    android_session_active: bool,
    android_session_device: String,
    listener_security: String,
    paired_peer_count: usize,
}

impl RefreshUpdate {
    pub(super) fn from_status(
        daemon_up: bool,
        cfg: Config,
        st: State,
        local_telem: TelemetrySnapshot,
        status_json: &serde_json::Value,
        apps_json: Vec<serde_json::Value>,
        live_peers: &[Peer],
    ) -> Self {
        // Parse connection block
        let conn = status_json.get("connection").filter(|c| !c.is_null());
        let connected = conn.is_some();
        let multiroom = conn
            .and_then(|c| c.get("multiroom"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let connected_peer_id = conn
            .and_then(|c| c.get("peer_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let connected_name = conn
            .and_then(|c| c.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let connected_host = conn
            .and_then(|c| c.get("host"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let connected_port = conn
            .and_then(|c| c.get("port"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u16;
        let connected_duration = conn
            .and_then(|c| c.get("duration"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let keep_local = conn
            .and_then(|c| c.get("keep_local"))
            .and_then(|v| v.as_bool())
            .unwrap_or(st.keep_local);
        let multiroom_peers: Vec<ActivePeerSession> = conn
            .and_then(|c| c.get("multiroom_peers"))
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or(st.multiroom_peers.clone());
        let rtt_ms = conn.and_then(|c| c.get("rtt_ms")).and_then(|v| v.as_f64());
        let jitter_ms = conn
            .and_then(|c| c.get("jitter_ms"))
            .and_then(|v| v.as_f64());

        // Volume
        let vol_obj = status_json.get("volume").filter(|v| !v.is_null());
        let volume_level = vol_obj
            .and_then(|v| v.get("level"))
            .and_then(|v| v.as_u64())
            .unwrap_or(100) as u32;
        let volume_muted = vol_obj
            .and_then(|v| v.get("mute"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // Sharing & Auto
        let sharing_enabled = status_json
            .get("sharing")
            .and_then(|s| s.get("enabled"))
            .and_then(|v| v.as_bool())
            .unwrap_or(cfg.sharing.enabled);
        let listener_security = status_json
            .get("sharing")
            .and_then(|s| s.get("listener_security"))
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| st.listener_security.clone());
        let auto_connect = status_json
            .get("auto")
            .and_then(|a| a.get("enabled"))
            .and_then(|v| v.as_bool())
            .unwrap_or(cfg.connection.auto_connect);
        let auto_paused = status_json
            .get("auto")
            .and_then(|a| a.get("paused"))
            .and_then(|v| v.as_bool())
            .unwrap_or(st.auto_paused);

        // Mic
        let mic_enabled = conn
            .and_then(|c| c.get("with_mic"))
            .and_then(|v| v.as_bool())
            .unwrap_or(!st.connected_source.is_empty());
        let mic_direction = conn
            .and_then(|c| c.get("mic_direction"))
            .and_then(|v| v.as_str())
            .unwrap_or(&st.mic_direction)
            .to_string();

        // Android session
        let and_obj = status_json.get("android");
        let android_session_active = and_obj
            .and_then(|a| a.get("active"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let android_session_device = and_obj
            .and_then(|a| a.get("device_name"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Audio level (prefer local telemetry if active, else daemon snapshot)
        let daemon_audio = status_json.get("telemetry").and_then(|t| t.get("audio"));
        let (audio_active, audio_dbfs, audio_peak) = if local_telem.audio.active {
            (
                true,
                local_telem.audio.dbfs,
                local_telem
                    .audio
                    .peak_left
                    .max(local_telem.audio.peak_right),
            )
        } else if let Some(da) = daemon_audio {
            let active = da.get("active").and_then(|v| v.as_bool()).unwrap_or(false);
            let dbfs = da.get("dbfs").and_then(|v| v.as_f64()).unwrap_or(-60.0) as f32;
            let pl = da.get("peak_left").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
            let pr = da.get("peak_right").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
            (active, dbfs, pl.max(pr))
        } else {
            (
                false,
                local_telem.audio.dbfs,
                local_telem
                    .audio
                    .peak_left
                    .max(local_telem.audio.peak_right),
            )
        };

        // Peers (merge live + remembered)
        let paired_ids: HashSet<String> =
            if let Some(arr) = status_json.get("paired_peers").and_then(|v| v.as_array()) {
                arr.iter()
                    .filter_map(|item| item.get("peer_id").and_then(|v| v.as_str()))
                    .filter(|pid| !pid.is_empty())
                    .map(String::from)
                    .collect()
            } else {
                st.paired_peers.keys().cloned().collect()
            };
        let mut peer_views = Vec::new();
        let mut seen_ids = HashSet::new();
        for p in live_peers {
            if p.id == cfg.device.id || seen_ids.contains(&p.id) {
                continue;
            }
            if !is_private_address(&p.host) || is_own_non_loopback_ip(&p.host) {
                continue;
            }
            seen_ids.insert(p.id.clone());
            peer_views.push(PeerView {
                id: p.id.clone(),
                name: p.name.clone(),
                host: p.host.clone(),
                port: p.port,
                platform: p.platform.clone(),
                security: p.security.clone(),
                rtt_ms: p.rtt_ms,
                online: true,
                paired: paired_ids.contains(&p.id),
            });
        }
        for kp in st.known_peers.values() {
            if kp.id.is_empty()
                || kp.id == cfg.device.id
                || seen_ids.contains(&kp.id)
                || !is_private_address(&kp.host)
                || is_own_non_loopback_ip(&kp.host)
            {
                continue;
            }
            seen_ids.insert(kp.id.clone());
            let is_conn =
                connected_peer_id == kp.id || multiroom_peers.iter().any(|mp| mp.peer_id == kp.id);
            peer_views.push(PeerView {
                id: kp.id.clone(),
                name: kp.name.clone(),
                host: kp.host.clone(),
                port: kp.port,
                platform: kp.platform.clone(),
                security: kp.security.clone(),
                rtt_ms: if is_conn { rtt_ms } else { None },
                online: is_conn,
                paired: paired_ids.contains(&kp.id),
            });
        }

        // Apps
        let apps: Vec<AppStreamView> = apps_json
            .into_iter()
            .map(|a| {
                let id = a.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
                let app_name = a
                    .get("app_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("App")
                    .to_string();
                let media_name = a
                    .get("media_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let is_remote = a
                    .get("is_remote")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let routed_rule = a
                    .get("routed_rule")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let persistent = a
                    .get("persistent")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                AppStreamView {
                    app_name,
                    media_name,
                    is_remote,
                    routed_rule,
                    persistent,
                    active: id > 0,
                }
            })
            .collect();

        let show_onboarding_initial = !cfg.ui.onboarding_completed && st.known_peers.is_empty();
        Self {
            daemon_connected: daemon_up,
            show_onboarding_initial,
            config: cfg,
            connected,
            multiroom,
            connected_peer_id,
            connected_name,
            connected_host,
            connected_port,
            connected_duration,
            multiroom_peers,
            keep_local,
            sharing_enabled,
            auto_connect,
            auto_paused,
            volume_level,
            volume_muted,
            mic_enabled,
            mic_direction,
            peers: peer_views,
            apps,
            audio_active,
            audio_dbfs,
            audio_peak,
            rtt_ms,
            jitter_ms,
            android_session_active,
            android_session_device,
            listener_security,
            paired_peer_count: paired_ids.len(),
        }
    }

    pub(super) fn apply_to(self, guard: &mut GuiSnapshot, onboarding_reopened: bool) {
        if !guard.hydrated && !onboarding_reopened {
            guard.show_onboarding = self.show_onboarding_initial;
        }
        guard.hydrated = true;
        guard.daemon_connected = self.daemon_connected;
        guard.config = self.config;
        guard.connected = self.connected;
        guard.multiroom = self.multiroom;
        guard.connected_peer_id = self.connected_peer_id;
        guard.connected_name = self.connected_name;
        guard.connected_host = self.connected_host;
        guard.connected_port = self.connected_port;
        guard.connected_duration = self.connected_duration;
        guard.multiroom_peers = self.multiroom_peers;
        guard.keep_local = self.keep_local;
        guard.sharing_enabled = self.sharing_enabled;
        guard.auto_connect = self.auto_connect;
        guard.auto_paused = self.auto_paused;
        guard.volume_level = self.volume_level;
        guard.volume_muted = self.volume_muted;
        guard.mic_enabled = self.mic_enabled;
        guard.mic_direction = self.mic_direction;
        guard.peers = self.peers;
        guard.apps = self.apps;
        guard.audio_active = self.audio_active;
        guard.audio_dbfs = self.audio_dbfs;
        guard.audio_peak = self.audio_peak;
        guard.rtt_ms = self.rtt_ms;
        guard.jitter_ms = self.jitter_ms;
        guard.android_session_active = self.android_session_active;
        guard.android_session_device = self.android_session_device;
        guard.listener_security = self.listener_security;
        guard.paired_peer_count = self.paired_peer_count;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soundhop_core::constants::{PROTOCOL_VERSION, VERSION};
    use soundhop_core::state::{KnownPeer, PairedPeer};
    use soundhop_core::telemetry::AudioLevelSnapshot;

    fn empty_telemetry() -> TelemetrySnapshot {
        TelemetrySnapshot {
            audio: AudioLevelSnapshot::default(),
            peers: Default::default(),
        }
    }

    #[test]
    fn test_peer_view_paired_is_false_for_unpaired_psk_peers_and_exposes_listener_security() {
        let cfg = Config::default();
        let mut st = State::default();
        st.known_peers.insert(
            "known-unpaired-psk".to_string(),
            KnownPeer {
                id: "known-unpaired-psk".to_string(),
                name: "Saved Studio".to_string(),
                host: "192.168.1.51".to_string(),
                port: 4713,
                version: VERSION.to_string(),
                security: "psk".to_string(),
                platform: "linux".to_string(),
                last_seen: 100.0,
            },
        );
        st.paired_peers.insert(
            "live-paired-psk".to_string(),
            PairedPeer {
                peer_id: "live-paired-psk".to_string(),
                name: "Paired Desk".to_string(),
                shared_key_hex: "abc".to_string(),
                cookie_hex: "def".to_string(),
                paired_at: 100.0,
            },
        );

        let live_peers = vec![
            Peer {
                id: "live-unpaired-psk".to_string(),
                name: "Unpaired Host".to_string(),
                host: "192.168.1.50".to_string(),
                port: 4713,
                version: VERSION.to_string(),
                protocol: PROTOCOL_VERSION,
                security: "psk".to_string(),
                platform: "linux".to_string(),
                last_seen: 100.0,
                source: "udp".to_string(),
                rtt_ms: Some(3.2),
            },
            Peer {
                id: "live-paired-psk".to_string(),
                name: "Paired Desk".to_string(),
                host: "192.168.1.52".to_string(),
                port: 4713,
                version: VERSION.to_string(),
                protocol: PROTOCOL_VERSION,
                security: "psk".to_string(),
                platform: "linux".to_string(),
                last_seen: 100.0,
                source: "udp".to_string(),
                rtt_ms: Some(4.1),
            },
        ];

        let status_json = serde_json::json!({
            "sharing": {
                "enabled": true,
                "listener_security": "cookie",
            },
            "paired_peers": [
                { "peer_id": "live-paired-psk", "name": "Paired Desk" }
            ],
        });

        let update = RefreshUpdate::from_status(
            true,
            cfg,
            st,
            empty_telemetry(),
            &status_json,
            Vec::new(),
            &live_peers,
        );

        assert_eq!(update.listener_security, "cookie");
        assert_eq!(update.paired_peer_count, 1);

        let unpaired_live = update
            .peers
            .iter()
            .find(|p| p.id == "live-unpaired-psk")
            .unwrap();
        assert!(
            !unpaired_live.paired,
            "unpaired live psk peer must NOT be marked paired"
        );

        let unpaired_known = update
            .peers
            .iter()
            .find(|p| p.id == "known-unpaired-psk")
            .unwrap();
        assert!(
            !unpaired_known.paired,
            "unpaired known psk peer must NOT be marked paired"
        );

        let paired_live = update
            .peers
            .iter()
            .find(|p| p.id == "live-paired-psk")
            .unwrap();
        assert!(paired_live.paired);
    }
}
