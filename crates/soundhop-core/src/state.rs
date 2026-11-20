//! Persistent runtime state (single & multi-room connections, paired peers, known devices).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::pairing::TrustedPeer;
use crate::util::{atomic_write, is_own_non_loopback_ip, now_secs, state_path};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct KnownPeer {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub last_seen: f64,
    pub version: String,
    pub security: String,
    pub platform: String,
}

impl Default for KnownPeer {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            host: String::new(),
            port: 0,
            last_seen: 0.0,
            version: String::new(),
            security: "trusted-lan".to_string(),
            platform: "linux".to_string(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PairedPeer {
    pub peer_id: String,
    pub name: String,
    /// Shared HMAC key hex derived during PIN or PSK pairing.
    pub shared_key_hex: String,
    /// Synchronized PulseAudio cookie hex (512 hex chars = 256 bytes).
    pub cookie_hex: String,
    pub paired_at: f64,
}

impl PairedPeer {
    /// Returns a public JSON representation safe for status/IPC output, omitting raw secrets.
    pub fn to_public_json(&self) -> serde_json::Value {
        serde_json::json!({
            "peer_id": self.peer_id,
            "name": self.name,
            "paired_at": self.paired_at,
            "has_shared_key": !self.shared_key_hex.is_empty(),
            "has_cookie": !self.cookie_hex.is_empty(),
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ActivePeerSession {
    pub peer_id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub sink_name: String,
    pub module_id: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct KnownAndroidDevice {
    pub serial: String,
    pub model: String,
    pub manufacturer: String,
    pub last_ip: String,
    pub last_port: u16,
    pub last_seen: f64,
    pub last_connected: f64,
    pub is_wifi: bool,
    pub auto_connect: bool,
}

impl Default for KnownAndroidDevice {
    fn default() -> Self {
        Self {
            serial: String::new(),
            model: String::new(),
            manufacturer: String::new(),
            last_ip: String::new(),
            last_port: 5555,
            last_seen: 0.0,
            last_connected: 0.0,
            is_wifi: false,
            auto_connect: false,
        }
    }
}

impl KnownAndroidDevice {
    pub fn display_name(&self) -> String {
        let mut parts = Vec::new();
        if !self.manufacturer.is_empty() {
            parts.push(self.manufacturer.as_str());
        }
        if !self.model.is_empty() && self.model != self.manufacturer {
            parts.push(self.model.as_str());
        }
        if parts.is_empty() {
            self.serial.clone()
        } else {
            parts.join(" ")
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct State {
    pub previous_default_sink: String,
    pub previous_default_source: String,
    pub connected_peer: String,
    pub connected_name: String,
    pub connected_host: String,
    pub connected_port: u16,
    pub connected_sink: String,
    pub connected_module: u32,
    pub connected_source: String,
    pub connected_source_module: u32,
    /// Multi-room peer sessions when connected to 1 or more peers.
    pub multiroom_peers: Vec<ActivePeerSession>,
    pub combine_module: u32,
    pub keep_local: bool,
    pub mic_direction: String,
    pub remote_mic_null_module: u32,
    pub remote_mic_null_set: bool,
    pub remote_mic_remap_module: u32,
    pub remote_mic_remap_set: bool,
    pub local_mic_loopback_module: u32,
    pub connected_since: f64,
    pub connected_by: String,
    pub auto_paused: bool,
    pub listener_module: u32,
    pub listener_created: bool,
    pub listener_port: u16,
    pub listener_security: String,
    pub routed_apps: BTreeMap<String, String>,
    pub last_synced_volume: i32,
    pub last_synced_mute: bool,
    pub known_peers: BTreeMap<String, KnownPeer>,
    pub paired_peers: BTreeMap<String, PairedPeer>,
    #[serde(default)]
    pub trusted_peers: Vec<TrustedPeer>,
    pub known_android_devices: BTreeMap<String, KnownAndroidDevice>,
    pub daemon_last_started: f64,
    pub android_active: bool,
    pub android_as_mic: bool,
    pub android_serial: String,
    pub android_model: String,
    pub android_codec: String,
    pub android_source: String,
    pub android_pid: u32,
    pub android_since: f64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            previous_default_sink: String::new(),
            previous_default_source: String::new(),
            connected_peer: String::new(),
            connected_name: String::new(),
            connected_host: String::new(),
            connected_port: 0,
            connected_sink: String::new(),
            connected_module: 0,
            connected_source: String::new(),
            connected_source_module: 0,
            multiroom_peers: Vec::new(),
            combine_module: 0,
            keep_local: false,
            mic_direction: String::new(),
            remote_mic_null_module: 0,
            remote_mic_null_set: false,
            remote_mic_remap_module: 0,
            remote_mic_remap_set: false,
            local_mic_loopback_module: 0,
            connected_since: 0.0,
            connected_by: "manual".to_string(),
            auto_paused: false,
            listener_module: 0,
            listener_created: false,
            listener_port: 0,
            listener_security: String::new(),
            routed_apps: BTreeMap::new(),
            last_synced_volume: -1,
            last_synced_mute: false,
            known_peers: BTreeMap::new(),
            paired_peers: BTreeMap::new(),
            trusted_peers: Vec::new(),
            known_android_devices: BTreeMap::new(),
            daemon_last_started: 0.0,
            android_active: false,
            android_as_mic: false,
            android_serial: String::new(),
            android_model: String::new(),
            android_codec: String::new(),
            android_source: String::new(),
            android_pid: 0,
            android_since: 0.0,
        }
    }
}

impl State {
    pub fn try_load(path: Option<&Path>) -> Result<Self, String> {
        let target = path.map(Path::to_path_buf).unwrap_or_else(state_path);
        if !target.exists() {
            return Err(format!("State file does not exist: {}", target.display()));
        }
        let raw = fs::read_to_string(&target)
            .map_err(|e| format!("Failed to read state file {}: {e}", target.display()))?;
        let mut st = serde_json::from_str::<Self>(&raw)
            .map_err(|e| format!("Failed to parse state file {}: {e}", target.display()))?;
        st.known_peers
            .retain(|_, p| !is_own_non_loopback_ip(&p.host));
        st.sanitize_legacy_pairing();
        Ok(st)
    }

    pub fn load(path: Option<&Path>) -> Self {
        Self::try_load(path).unwrap_or_default()
    }

    pub fn save(&mut self, path: Option<&Path>) -> std::io::Result<PathBuf> {
        let target = path.map(Path::to_path_buf).unwrap_or_else(state_path);
        self.sanitize_legacy_pairing();
        self.prune_known_peers(90.0 * 86400.0, 50);
        self.prune_known_android_devices(90.0 * 86400.0, 50);
        let text = serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string());
        atomic_write(&target, &(text + "\n"))?;
        Ok(target)
    }

    /// Migrate/drop synthetic `"lan-group"` pairing entries from legacy state files.
    pub fn sanitize_legacy_pairing(&mut self) {
        self.paired_peers.remove("lan-group");
        self.trusted_peers
            .retain(|p| !p.device_id.trim().is_empty() && p.device_id != "lan-group");
    }

    /// Insert or update a [`TrustedPeer`] by `device_id`, ignoring empty or synthetic `"lan-group"` IDs.
    pub fn upsert_trusted_peer(&mut self, peer: TrustedPeer) {
        if peer.device_id.trim().is_empty() || peer.device_id == "lan-group" {
            return;
        }
        if let Some(existing) = self
            .trusted_peers
            .iter_mut()
            .find(|p| p.device_id == peer.device_id)
        {
            *existing = peer;
        } else {
            self.trusted_peers.push(peer);
        }
    }

    /// Remove a [`TrustedPeer`] by `device_id`. Returns `true` if one was removed.
    pub fn remove_trusted_peer(&mut self, device_id: &str) -> bool {
        let before = self.trusted_peers.len();
        self.trusted_peers.retain(|p| p.device_id != device_id);
        self.trusted_peers.len() < before
    }

    /// Look up a [`TrustedPeer`] by `device_id`.
    pub fn find_trusted_peer(&self, device_id: &str) -> Option<&TrustedPeer> {
        self.trusted_peers.iter().find(|p| p.device_id == device_id)
    }

    pub fn is_connected(&self) -> bool {
        !self.connected_peer.is_empty() && !self.connected_sink.is_empty()
    }

    pub fn is_multiroom(&self) -> bool {
        self.multiroom_peers.len() > 1 || self.combine_module > 0
    }

    pub fn clear_connection(&mut self) {
        self.connected_peer.clear();
        self.connected_name.clear();
        self.connected_host.clear();
        self.connected_port = 0;
        self.connected_sink.clear();
        self.connected_module = 0;
        self.connected_source.clear();
        self.connected_source_module = 0;
        self.multiroom_peers.clear();
        self.combine_module = 0;
        self.keep_local = false;
        self.mic_direction.clear();
        self.remote_mic_null_module = 0;
        self.remote_mic_null_set = false;
        self.remote_mic_remap_module = 0;
        self.remote_mic_remap_set = false;
        self.local_mic_loopback_module = 0;
        self.connected_since = 0.0;
        self.connected_by = "manual".to_string();
    }

    pub fn remember_peer(&mut self, peer: KnownPeer) {
        if peer.id.is_empty() || is_own_non_loopback_ip(&peer.host) {
            return;
        }
        self.known_peers.insert(peer.id.clone(), peer);
    }

    pub fn forget_peer(&mut self, peer_id: &str) {
        self.known_peers.remove(peer_id);
    }

    pub fn prune_known_peers(&mut self, max_age_secs: f64, limit: usize) {
        if self.known_peers.is_empty() {
            return;
        }
        let cutoff = now_secs() - max_age_secs;
        self.known_peers.retain(|_, p| p.last_seen >= cutoff);
        if self.known_peers.len() > limit {
            let mut entries: Vec<KnownPeer> = self.known_peers.values().cloned().collect();
            entries.sort_by(|a, b| {
                b.last_seen
                    .partial_cmp(&a.last_seen)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            entries.truncate(limit);
            self.known_peers = entries.into_iter().map(|p| (p.id.clone(), p)).collect();
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn remember_android_device(
        &mut self,
        serial: &str,
        model: &str,
        manufacturer: &str,
        ip: &str,
        port: u16,
        is_wifi: bool,
        connected: bool,
        when: Option<f64>,
    ) -> KnownAndroidDevice {
        let now = when.unwrap_or_else(now_secs);
        let entry = self
            .known_android_devices
            .entry(serial.to_string())
            .or_insert_with(|| KnownAndroidDevice {
                serial: serial.to_string(),
                ..Default::default()
            });
        if !model.is_empty() {
            entry.model = model.to_string();
        }
        if !manufacturer.is_empty() {
            entry.manufacturer = manufacturer.to_string();
        }
        if !ip.is_empty() {
            entry.last_ip = ip.to_string();
        }
        if port > 0 {
            entry.last_port = port;
        }
        if is_wifi {
            entry.is_wifi = true;
        }
        entry.last_seen = now;
        if connected {
            entry.last_connected = now;
        }
        entry.clone()
    }

    pub fn prune_known_android_devices(&mut self, max_age_secs: f64, limit: usize) {
        if self.known_android_devices.is_empty() {
            return;
        }
        let cutoff = now_secs() - max_age_secs;
        self.known_android_devices
            .retain(|_, d| d.last_seen.max(d.last_connected) >= cutoff);
        if self.known_android_devices.len() > limit {
            let mut entries: Vec<KnownAndroidDevice> =
                self.known_android_devices.values().cloned().collect();
            entries.sort_by(|a, b| {
                b.last_seen
                    .max(b.last_connected)
                    .partial_cmp(&a.last_seen.max(a.last_connected))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            entries.truncate(limit);
            self.known_android_devices =
                entries.into_iter().map(|d| (d.serial.clone(), d)).collect();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_state_roundtrip_and_pruning() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut state = State::default();
        let now = now_secs();
        state.remember_peer(KnownPeer {
            id: "peer1".to_string(),
            name: "Desk PC".to_string(),
            host: "192.168.1.20".to_string(),
            port: 4713,
            last_seen: now,
            version: "2.0.0".to_string(),
            security: "psk".to_string(),
            platform: "linux".to_string(),
        });
        state.remember_peer(KnownPeer {
            id: "stale_peer".to_string(),
            name: "Old PC".to_string(),
            host: "192.168.1.99".to_string(),
            port: 4713,
            last_seen: 100.0,
            version: "1.0.0".to_string(),
            security: "trusted-lan".to_string(),
            platform: "linux".to_string(),
        });
        state.save(Some(&path)).unwrap();

        let loaded = State::load(Some(&path));
        assert!(loaded.known_peers.contains_key("peer1"));
        assert!(!loaded.known_peers.contains_key("stale_peer"));
    }

    #[test]
    fn test_state_trusted_peers_and_lan_group_migration() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");

        // Legacy JSON containing synthetic "lan-group" and no "trusted_peers" field
        let legacy_json = serde_json::json!({
            "paired_peers": {
                "lan-group": {
                    "peer_id": "lan-group",
                    "name": "LAN Group",
                    "shared_key_hex": "aa",
                    "cookie_hex": "bb",
                    "paired_at": 100.0
                },
                "real-peer": {
                    "peer_id": "real-peer",
                    "name": "Real Peer",
                    "shared_key_hex": "cc",
                    "cookie_hex": "dd",
                    "paired_at": 200.0
                }
            }
        });
        fs::write(&path, serde_json::to_string_pretty(&legacy_json).unwrap()).unwrap();

        let mut loaded = State::try_load(Some(&path)).unwrap();
        assert!(!loaded.paired_peers.contains_key("lan-group"));
        assert!(loaded.paired_peers.contains_key("real-peer"));
        assert!(loaded.trusted_peers.is_empty());

        loaded.upsert_trusted_peer(TrustedPeer {
            device_id: "lan-group".to_string(),
            display_name: "Synthetic".to_string(),
            ed25519_pub: [1u8; 32],
            pairwise_secret: [2u8; 32],
            fingerprint: "0102-0304-0506-0708".to_string(),
            paired_at: 300.0,
        });
        assert!(loaded.trusted_peers.is_empty());

        let tp = TrustedPeer {
            device_id: "dev-123".to_string(),
            display_name: "Studio".to_string(),
            ed25519_pub: [3u8; 32],
            pairwise_secret: [4u8; 32],
            fingerprint: "A1B2-C3D4-E5F6-0708".to_string(),
            paired_at: 400.0,
        };
        loaded.upsert_trusted_peer(tp.clone());
        loaded.save(Some(&path)).unwrap();

        let reloaded = State::try_load(Some(&path)).unwrap();
        assert_eq!(reloaded.trusted_peers.len(), 1);
        assert_eq!(reloaded.find_trusted_peer("dev-123"), Some(&tp));
    }
}
