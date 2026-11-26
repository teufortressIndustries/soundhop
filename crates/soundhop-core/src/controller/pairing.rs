//! Audio sharing listener lifecycle, PIN/PSK pairing, and Pairing v2 interactive requests.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::backend::Listener;
use crate::config::{is_paired_security_mode, Config};
use crate::pairing::{
    identity_key_path, initiate_pair_request_with_manager, DeviceIdentity, PairingManager,
    PendingPairRequest, TrustStore, TrustedPeer, DEFAULT_PAIR_PORT, PENDING_REQUEST_TTL_SECS,
};
use crate::security::{
    derive_key_hex, derive_pa_cookie, generate_pairing_pin, soundhop_cookie_path, write_pa_cookie,
};
use crate::state::PairedPeer;
use crate::transport::{
    PlaybackEndpoint, PlaybackEndpointFactory, Sht3ServerConfig, TrustLookup, DEFAULT_V3_PORT,
};
use crate::util::{local_addresses, lock_or_recover, notify, now_secs, tcp_reachable};

use super::{Controller, ControllerError};

static PAIRING_MANAGERS: OnceLock<Mutex<HashMap<String, Arc<PairingManager>>>> = OnceLock::new();

impl Controller {
    // -- Sharing & Listener ----------------------------------------------

    pub fn share(
        &self,
        enabled: bool,
        notify_user: Option<bool>,
    ) -> Result<serde_json::Value, ControllerError> {
        let res = if enabled {
            let r = self.start_listener(notify_user)?;
            lock_or_recover(&self.config).sharing.enabled = true;
            r
        } else {
            lock_or_recover(&self.config).sharing.enabled = false;
            self.stop_listener(notify_user)?
        };
        self.save();
        Ok(res)
    }

    pub fn start_listener(
        &self,
        notify_user: Option<bool>,
    ) -> Result<serde_json::Value, ControllerError> {
        let paired_only = is_paired_security_mode(&lock_or_recover(&self.config).sharing.security);
        if !paired_only {
            self.ensure_backend()?;
        }
        self.reconcile_listener();
        let listener = self.ensure_listener_internal()?;
        let v3_addr = self.sht3_manager.listener_addr();
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
            "v3_active": v3_addr.is_some(),
            "v3_port": v3_addr.map(|a| a.port()),
            "legacy_pa_disabled": self.is_legacy_pa_disabled(),
            "addresses": local_addresses(false),
        }))
    }

    pub fn stop_listener(
        &self,
        notify_user: Option<bool>,
    ) -> Result<serde_json::Value, ControllerError> {
        let (removed, note) = self.stop_legacy_pa_listener()?;
        let v3_was_running = self.sht3_manager.listener_addr().is_some();
        self.sht3_manager.stop_listener();
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
            "removed": removed || v3_was_running,
            "v3_stopped": v3_was_running,
            "note": note,
        }))
    }

    fn stop_legacy_pa_listener(&self) -> Result<(bool, String), ControllerError> {
        let mut removed = false;
        let mut note = String::new();
        let (mod_id, created) = {
            let st = lock_or_recover(&self.state);
            (st.listener_module, st.listener_created)
        };
        if mod_id > 0 {
            if let Ok(Some(m)) = self.backend.module(mod_id) {
                if created {
                    removed = self.backend.unload_module(m.id, true)?;
                } else {
                    note = "A TCP audio listener not created by SoundHop is still running. Discovery advertisements are off.".to_string();
                }
            }
            let mut st = lock_or_recover(&self.state);
            st.listener_module = 0;
            st.listener_created = false;
            st.listener_port = 0;
            st.listener_security.clear();
        }
        Ok((removed, note))
    }

    fn playback_factory(&self) -> PlaybackEndpointFactory {
        #[cfg(test)]
        if let Some(factory) = lock_or_recover(&self.playback_factory_override).clone() {
            return factory;
        }
        #[cfg(target_os = "linux")]
        {
            Arc::new(|_peer_id: &str| Ok(PlaybackEndpoint::SystemPulse))
        }
        #[cfg(target_os = "windows")]
        {
            Arc::new(|_peer_id: &str| Ok(PlaybackEndpoint::SystemWasapi))
        }
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        {
            Arc::new(|_peer_id: &str| {
                Err(crate::transport::TransportError::Pipeline(
                    "Encrypted audio playback is not supported on this platform".into(),
                ))
            })
        }
    }

    fn sync_trust_store_from_state(&self) -> Arc<TrustStore> {
        let store = self.pairing_manager().trust_store();
        let state_peers = lock_or_recover(&self.state).trusted_peers.clone();
        for peer in state_peers {
            if !store.contains(&peer.device_id) {
                store.upsert(peer);
            }
        }
        store
    }

    pub(super) fn start_sht3_listener_internal(
        &self,
        port: u16,
        cfg: &Config,
    ) -> Result<SocketAddr, ControllerError> {
        if port > 0 {
            if let Some(existing) = self.sht3_manager.listener_addr() {
                if existing.port() == port {
                    return Ok(existing);
                }
            }
        }
        let trust: Arc<dyn TrustLookup> = self.sync_trust_store_from_state();
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port);
        let server_cfg =
            Sht3ServerConfig::new(cfg.device.id.clone()).with_rtp_port(cfg.sharing.rtp_port);
        self.sht3_manager
            .start_listener(addr, server_cfg, trust, self.playback_factory())
            .map_err(|e| {
                ControllerError::Message(format!(
                    "Could not start encrypted audio listener on port {port}: {e}"
                ))
            })
    }

    pub(super) fn ensure_listener_internal(&self) -> Result<Listener, ControllerError> {
        let cfg = lock_or_recover(&self.config).clone();
        let paired_only = is_paired_security_mode(&cfg.sharing.security);
        let v3_port = if cfg.sharing.v3_port == 0 {
            DEFAULT_V3_PORT
        } else {
            cfg.sharing.v3_port
        };

        if paired_only {
            // Hard enforcement: unload legacy PA-TCP (4713/tcp) completely and accept
            // inbound audio ONLY over SHT3 backed by TrustStore.
            let _ = self.stop_legacy_pa_listener();
            let bound = self.start_sht3_listener_internal(v3_port, &cfg)?;
            {
                let mut st = lock_or_recover(&self.state);
                st.listener_module = 0;
                st.listener_created = false;
                st.listener_port = 0;
                st.listener_security = "paired".to_string();
            }
            self.save();
            return Ok(Listener {
                module_id: 0,
                created: true,
                args: format!("sht3 port={}", bound.port()),
                port: bound.port(),
                security: "paired".to_string(),
            });
        }

        let (mod_id, created) = {
            let st = lock_or_recover(&self.state);
            (st.listener_module, st.listener_created)
        };
        if mod_id > 0 {
            if let Ok(Some(m)) = self.backend.module(mod_id) {
                if m.name == "module-native-protocol-tcp" {
                    let mod_port = m
                        .arg("port")
                        .and_then(|p| p.parse::<u16>().ok())
                        .unwrap_or(4713);
                    if mod_port != cfg.sharing.port && created {
                        let _ = self.backend.unload_module(m.id, true);
                        let mut st = lock_or_recover(&self.state);
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
        // In trusted-lan / open mode, keep legacy PA-TCP active AND optionally run SHT3
        // on v3_port for paired peers.
        let _ = self.start_sht3_listener_internal(v3_port, &cfg);
        {
            let mut st = lock_or_recover(&self.state);
            st.listener_module = listener.module_id;
            st.listener_created = listener.created;
            st.listener_port = listener.port;
            st.listener_security = listener.security.clone();
        }
        self.save();
        Ok(listener)
    }

    pub(super) fn reconcile_listener(&self) {
        let mod_id = lock_or_recover(&self.state).listener_module;
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
            let mut st = lock_or_recover(&self.state);
            st.listener_module = 0;
            st.listener_created = false;
            st.listener_port = 0;
            st.listener_security.clear();
        }
    }

    pub fn listener_running(&self) -> bool {
        let paired_only = is_paired_security_mode(&lock_or_recover(&self.config).sharing.security);
        if paired_only {
            return self.sht3_manager.listener_addr().is_some();
        }
        let mod_id = lock_or_recover(&self.state).listener_module;
        if mod_id == 0 {
            return false;
        }
        let _ = tcp_reachable;
        self.find_module(mod_id)
            .map(|m| m.name == "module-native-protocol-tcp")
            .unwrap_or(false)
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

        let peer_name = lock_or_recover(&self.state)
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
        lock_or_recover(&self.state)
            .paired_peers
            .insert(peer_id_or_name.to_string(), paired);
        let should_reload = {
            let mut cfg = lock_or_recover(&self.config);
            cfg.sharing.psk_hash = shared_key.clone();
            if cfg.sharing.security == "trusted-lan" {
                cfg.sharing.security = "psk".to_string();
            }
            cfg.sharing.enabled
        };
        if should_reload || self.listener_running() {
            let _ = self.ensure_listener_internal();
        }
        self.save();
        self.log_line(
            "INFO",
            &format!(
                "Legacy pairing configured for peer '{}' ({})",
                peer_name, peer_id_or_name
            ),
        );

        Ok(serde_json::json!({
            "paired": true,
            "peer_id": peer_id_or_name,
            "name": peer_name,
            "pin_or_psk": pin_or_psk,
            "shared_key_fingerprint": &shared_key[..16],
            "cookie_path": cookie_path.to_string_lossy(),
        }))
    }

    pub fn clear_pairing(&self) -> serde_json::Value {
        self.clear_pairing_with_cookie_path(&soundhop_cookie_path())
    }

    pub(crate) fn clear_pairing_with_cookie_path(&self, cookie_path: &Path) -> serde_json::Value {
        lock_or_recover(&self.state).paired_peers.clear();
        let should_reload = {
            let mut cfg = lock_or_recover(&self.config);
            cfg.sharing.psk_hash.clear();
            if matches!(cfg.sharing.security.as_str(), "psk" | "paired" | "pin") {
                cfg.sharing.security = "trusted-lan".to_string();
            }
            cfg.sharing.enabled
        };
        if let Err(e) = std::fs::remove_file(cookie_path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                let _ = e;
            }
        }
        if should_reload || self.listener_running() {
            let _ = self.ensure_listener_internal();
        }
        self.save();
        self.log_line("INFO", "Cleared legacy pairing credentials");
        serde_json::json!({ "ok": true, "cleared": true })
    }

    // -- Pairing v2 (Per-Device Identity & Interactive Pair Requests) ----

    fn identity_path_for_controller(&self) -> PathBuf {
        self.config_path
            .as_ref()
            .and_then(|p| p.parent())
            .map(|dir| dir.join("identity.key"))
            .unwrap_or_else(identity_key_path)
    }

    /// Load or create this device's persistent Ed25519 [`DeviceIdentity`].
    pub fn device_identity(&self) -> Result<DeviceIdentity, ControllerError> {
        let device_id = lock_or_recover(&self.config).device.id.clone();
        let path = self.identity_path_for_controller();
        DeviceIdentity::load_or_create(Some(&path), Some(&device_id))
            .map_err(|e| ControllerError::Message(format!("Failed loading device identity: {e}")))
    }

    /// Return the [`PairingManager`] associated with this controller, initialized with
    /// the controller's persisted [`TrustedPeer`]s.
    pub fn pairing_manager(&self) -> Arc<PairingManager> {
        let device_id = lock_or_recover(&self.config).device.id.clone();
        let registry = PAIRING_MANAGERS.get_or_init(|| Mutex::new(HashMap::new()));
        let mut guard = lock_or_recover(registry);
        Arc::clone(guard.entry(device_id).or_insert_with(|| {
            let existing = lock_or_recover(&self.state).trusted_peers.clone();
            Arc::new(PairingManager::new(Arc::new(TrustStore::from_peers(
                existing,
            ))))
        }))
    }

    /// Persist a newly paired [`TrustedPeer`] into `State` and the live [`TrustStore`].
    pub fn record_trusted_peer(&self, peer: TrustedPeer) {
        let peer_name = peer.display_name.clone();
        let peer_id = peer.device_id.clone();
        self.pairing_manager().trust_store().upsert(peer.clone());
        lock_or_recover(&self.state).upsert_trusted_peer(peer);
        self.save();
        self.log_line(
            "INFO",
            &format!("Trusted peer recorded: {peer_name} ({peer_id})"),
        );
    }

    /// Initiate a Pairing v2 TCP handshake against `host:pair_port` (optionally using a
    /// headless one-time `code`). Persists the resulting [`TrustedPeer`] on success.
    pub fn pair_request(
        &self,
        host: &str,
        pair_port: Option<u16>,
        code: Option<&str>,
    ) -> Result<serde_json::Value, ControllerError> {
        let trimmed_host = host.trim();
        if trimmed_host.is_empty() {
            return Err(ControllerError::Message(
                "Target host is required for pair_request".to_string(),
            ));
        }
        let (display_name, default_port) = {
            let cfg = lock_or_recover(&self.config);
            let port = if cfg.sharing.pair_port == 0 {
                DEFAULT_PAIR_PORT
            } else {
                cfg.sharing.pair_port
            };
            (cfg.device.name.clone(), port)
        };
        let port = pair_port.filter(|&p| p > 0).unwrap_or(default_port);
        let identity = self.device_identity()?;
        let mgr = self.pairing_manager();
        let outcome = initiate_pair_request_with_manager(
            trimmed_host,
            port,
            &identity,
            &display_name,
            code,
            Duration::from_secs(PENDING_REQUEST_TTL_SECS),
            Some(&mgr),
            None,
        )
        .map_err(|e| {
            self.log_line(
                "WARN",
                &format!("Pair request to {trimmed_host}:{port} failed: {e}"),
            );
            ControllerError::Message(e.to_string())
        })?;

        self.record_trusted_peer(outcome.peer.clone());
        Ok(serde_json::json!({
            "ok": true,
            "paired": true,
            "request_id": outcome.request_id,
            "sas_code": outcome.sas_code,
            "peer": outcome.peer.to_public_json(),
        }))
    }

    /// Return all active incoming Pairing v2 requests awaiting approval.
    pub fn pending_pair_requests(&self) -> Vec<PendingPairRequest> {
        self.pairing_manager().pending_requests()
    }

    /// Approve a pending incoming Pairing v2 request by `request_id` (or matching SAS code).
    pub fn approve_pair_request(&self, id: &str) -> Result<serde_json::Value, ControllerError> {
        let req = self
            .pairing_manager()
            .approve(id)
            .map_err(|e| ControllerError::Message(e.to_string()))?;
        self.log_line("INFO", &format!("Approved pair request {id}"));
        Ok(serde_json::json!({
            "ok": true,
            "approved": true,
            "request": req,
        }))
    }

    /// Deny a pending incoming Pairing v2 request by `request_id` (or matching SAS code).
    pub fn deny_pair_request(&self, id: &str) -> Result<serde_json::Value, ControllerError> {
        let req = self
            .pairing_manager()
            .deny(id)
            .map_err(|e| ControllerError::Message(e.to_string()))?;
        self.log_line("INFO", &format!("Denied pair request {id}"));
        Ok(serde_json::json!({
            "ok": true,
            "denied": true,
            "request": req,
        }))
    }

    /// Revoke a trusted peer by `device_id`, removing it from both `State` and `TrustStore`
    /// and immediately dropping any active SHT3 session for that device.
    pub fn revoke_trusted_peer(
        &self,
        device_id: &str,
    ) -> Result<serde_json::Value, ControllerError> {
        let target = device_id.trim();
        if target.is_empty() {
            return Err(ControllerError::Message(
                "device_id is required to revoke a trusted peer".to_string(),
            ));
        }
        let removed_store = self.pairing_manager().revoke(target).is_some();
        let removed_state = lock_or_recover(&self.state).remove_trusted_peer(target);
        let disconnected_inbound = self.sht3_manager.disconnect_peer(target);
        let disconnected_outbound = {
            let mut handles_to_stop = Vec::new();
            {
                let mut guard = lock_or_recover(&self.sht3_outbounds);
                if let Some(h) = guard.remove(target) {
                    handles_to_stop.push(h);
                }
                let matching_keys: Vec<String> = guard
                    .iter()
                    .filter(|(_, h)| h.server_device_id() == target)
                    .map(|(k, _)| k.clone())
                    .collect();
                for k in matching_keys {
                    if let Some(h) = guard.remove(&k) {
                        handles_to_stop.push(h);
                    }
                }
            }
            let had_any = !handles_to_stop.is_empty();
            for handle in handles_to_stop {
                handle.stop();
            }
            had_any
        };
        if removed_state {
            self.save();
        }
        if removed_store || removed_state {
            self.log_line(
                "INFO",
                &format!(
                    "Revoked trusted peer {target} (disconnected={})",
                    disconnected_inbound || disconnected_outbound
                ),
            );
        }
        Ok(serde_json::json!({
            "ok": true,
            "revoked": removed_store || removed_state,
            "disconnected": disconnected_inbound || disconnected_outbound,
            "device_id": target,
        }))
    }

    /// Return all persisted [`TrustedPeer`]s (synced with the live [`TrustStore`]).
    pub fn trusted_peers(&self) -> Vec<TrustedPeer> {
        let mut merged: HashMap<String, TrustedPeer> = HashMap::new();
        for p in &lock_or_recover(&self.state).trusted_peers {
            if !p.device_id.trim().is_empty() && p.device_id != "lan-group" {
                merged.insert(p.device_id.clone(), p.clone());
            }
        }
        for p in self.pairing_manager().trust_store().list() {
            merged.insert(p.device_id.clone(), p);
        }
        let mut out: Vec<TrustedPeer> = merged.into_values().collect();
        out.sort_by(|a, b| {
            a.display_name
                .to_lowercase()
                .cmp(&b.display_name.to_lowercase())
        });
        out
    }

    /// Generate a 120-second one-time pairing code for headless / Raspberry Pi devices.
    pub fn generate_headless_pair_code(&self) -> serde_json::Value {
        let code = self.pairing_manager().generate_one_time_code();
        serde_json::json!({
            "ok": true,
            "code": code.code,
            "created_at": code.created_at,
            "expires_at": code.expires_at,
            "ttl_secs": code.ttl_secs,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::backend::linux::LinuxAudioBackend;
    use crate::config::Config;
    use crate::controller::Controller;
    use crate::state::{PairedPeer, State};
    use crate::util::{CommandResult, MockRunner};

    #[test]
    fn test_status_redacts_paired_peer_secrets_while_disk_preserves_them() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("state.json");
        let cfg_path = dir.path().join("config.toml");

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

        let secret_key = "deadbeef0123456789abcdef01234567";
        let secret_cookie = "cafebabe9876543210fedcba98765432";
        ctrl.state.lock().unwrap().paired_peers.insert(
            "peer-sec".to_string(),
            PairedPeer {
                peer_id: "peer-sec".to_string(),
                name: "Studio Speaker".to_string(),
                shared_key_hex: secret_key.to_string(),
                cookie_hex: secret_cookie.to_string(),
                paired_at: 1234.5,
            },
        );
        ctrl.save();

        // Disk persistence must still contain the secrets.
        let reloaded = State::try_load(Some(&state_path)).unwrap();
        let saved_peer = reloaded.paired_peers.get("peer-sec").unwrap();
        assert_eq!(saved_peer.shared_key_hex, secret_key);
        assert_eq!(saved_peer.cookie_hex, secret_cookie);

        // status() JSON must NOT expose raw secrets or secret keys.
        let status = ctrl.status(None);
        let serialized = serde_json::to_string(&status).unwrap();
        assert!(!serialized.contains(secret_key));
        assert!(!serialized.contains(secret_cookie));
        assert!(!serialized.contains("shared_key_hex"));
        assert!(!serialized.contains("cookie_hex"));

        let paired = status["paired_peers"].as_array().unwrap();
        assert_eq!(paired.len(), 1);
        assert_eq!(paired[0]["peer_id"], "peer-sec");
        assert_eq!(paired[0]["name"], "Studio Speaker");
        assert_eq!(paired[0]["has_shared_key"], true);
        assert_eq!(paired[0]["has_cookie"], true);
    }

    #[test]
    fn test_clear_pairing_deletes_cookie_and_reverts_security() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("state.json");
        let cfg_path = dir.path().join("config.toml");
        let cookie_path = dir.path().join("pulse-cookie");
        crate::security::write_pa_cookie(
            Some(&cookie_path),
            &crate::security::derive_pa_cookie("stale-pin-key"),
        )
        .unwrap();
        assert!(cookie_path.exists());

        let runner = Arc::new(MockRunner::new(|args, _| {
            Ok(CommandResult {
                args: args.to_vec(),
                returncode: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }));
        let backend = Arc::new(LinuxAudioBackend::with_runner(runner));
        let mut cfg = Config::default();
        cfg.sharing.security = "psk".to_string();
        cfg.sharing.psk_hash = "some-derived-hash".to_string();
        let mut st = State::default();
        st.paired_peers.insert(
            "peer-1".to_string(),
            PairedPeer {
                peer_id: "peer-1".to_string(),
                name: "Living Room".to_string(),
                shared_key_hex: "abc".to_string(),
                cookie_hex: "def".to_string(),
                paired_at: 100.0,
            },
        );
        let ctrl = Controller::new(
            Some(cfg),
            Some(st),
            Some(backend),
            Some(cfg_path),
            Some(state_path),
            false,
        );

        let res = ctrl.clear_pairing_with_cookie_path(&cookie_path);
        assert_eq!(res["ok"], true);
        assert_eq!(res["cleared"], true);
        assert!(
            !cookie_path.exists(),
            "clear_pairing must delete stale pulse-cookie from disk"
        );
        assert!(ctrl.state.lock().unwrap().paired_peers.is_empty());
        assert!(ctrl.config.lock().unwrap().sharing.psk_hash.is_empty());
        assert_eq!(ctrl.config.lock().unwrap().sharing.security, "trusted-lan");

        // Calling again when cookie is already gone must succeed (NotFound ignored)
        let res2 = ctrl.clear_pairing_with_cookie_path(&cookie_path);
        assert_eq!(res2["ok"], true);
    }

    #[cfg(feature = "gstreamer")]
    #[test]
    fn test_paired_mode_disables_legacy_pa_tcp_and_enforces_sht3_trust_store() {
        use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
        use std::sync::Mutex;

        use crate::pairing::TrustedPeer;
        use crate::transport::{
            AudioProfile, PlaybackEndpoint, Sht3ClientConfig, Sht3ClientConnection, TransportError,
        };

        let unloaded = Arc::new(Mutex::new(Vec::<String>::new()));
        let unloaded_c = Arc::clone(&unloaded);
        let pa_listener_loaded = Arc::new(Mutex::new(false));
        let pa_loaded_c = Arc::clone(&pa_listener_loaded);

        let runner = Arc::new(MockRunner::new(move |args, _| {
            let joined = args.join(" ");
            let stdout = if joined.contains("info") {
                "Server Name: PulseAudio (on PipeWire 1.2.0)\nDefault Sink: alsa_output.pci\n"
                    .to_string()
            } else if joined.contains("unload-module") {
                unloaded_c.lock().unwrap().push(joined.clone());
                *pa_loaded_c.lock().unwrap() = false;
                String::new()
            } else if joined.contains("load-module module-native-protocol-tcp") {
                *pa_loaded_c.lock().unwrap() = true;
                "42\n".to_string()
            } else if joined.contains("list short modules") {
                if *pa_loaded_c.lock().unwrap() {
                    "42\tmodule-native-protocol-tcp\tport=4713 auth-anonymous=1 auth-ip-acl=127.0.0.1;192.168.0.0/16\n"
                        .to_string()
                } else {
                    String::new()
                }
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

        let free_port = {
            let sock = TcpListener::bind("127.0.0.1:0").unwrap();
            sock.local_addr().unwrap().port()
        };

        let mut cfg = Config::default();
        cfg.sharing.security = "trusted-lan".to_string();
        cfg.sharing.v3_port = free_port;
        cfg.sharing.rtp_port = 0;

        let backend = Arc::new(LinuxAudioBackend::with_runner(runner));
        let ctrl = Controller::new(
            Some(cfg),
            Some(State::default()),
            Some(backend),
            None,
            None,
            false,
        );
        ctrl.set_save_enabled(false);
        ctrl.set_playback_factory(Arc::new(|_peer_id| {
            Ok(PlaybackEndpoint::TestAppSink(Arc::new(Mutex::new(
                Vec::new(),
            ))))
        }));

        // 1. In trusted-lan mode, legacy PA-TCP is active and v4 listener runs alongside.
        let lan_res = ctrl.start_listener(Some(false)).unwrap();
        assert_eq!(lan_res["module"], 42);
        assert_eq!(lan_res["legacy_pa_disabled"], false);
        assert_eq!(lan_res["v3_active"], true);
        assert_eq!(ctrl.state.lock().unwrap().listener_module, 42);

        // 2. Switching to "paired" MUST unload legacy PA-TCP and accept inbound ONLY over TrustStore.
        {
            let mut c = ctrl.config.lock().unwrap();
            c.set_dotted("sharing.security", "paired").unwrap();
            c.sharing.rtp_port = 0;
        }
        let paired_res = ctrl.start_listener(Some(false)).unwrap();
        assert_eq!(paired_res["module"], 0);
        assert_eq!(paired_res["security"], "paired");
        assert_eq!(paired_res["v3_active"], true);
        assert_eq!(paired_res["legacy_pa_disabled"], true);
        assert_eq!(ctrl.state.lock().unwrap().listener_module, 0);
        assert!(
            unloaded
                .lock()
                .unwrap()
                .iter()
                .any(|cmd| cmd.contains("42")),
            "paired mode must unload legacy PA-TCP module 42"
        );
        assert!(ctrl.is_v3_active());
        assert!(ctrl.is_legacy_pa_disabled());
        assert!(ctrl.listener_running());

        let status = ctrl.status(None);
        assert_eq!(status["v3_active"], true);
        assert_eq!(status["legacy_pa_disabled"], true);
        assert_eq!(status["sharing"]["v3_active"], true);
        assert_eq!(status["sharing"]["legacy_pa_disabled"], true);

        let v3_addr = SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            ctrl.sht3_manager.listener_addr().unwrap().port(),
        );

        // 3. Untrusted device must be rejected by TrustStore.
        let untrusted_cfg = Sht3ClientConfig::new(
            "untrusted-dev",
            AudioProfile::PcmLossless.to_audio_params(20),
        );
        let err = Sht3ClientConnection::connect(v3_addr, untrusted_cfg, &[0x11u8; 32]);
        assert!(
            matches!(err, Err(TransportError::AuthRejected(_))),
            "untrusted device must be rejected by TrustStore"
        );

        // 4. Adding a TrustedPeer allows v4 handshake to succeed.
        let k_pair = [0x42u8; 32];
        ctrl.record_trusted_peer(TrustedPeer {
            device_id: "trusted-dev".to_string(),
            display_name: "Trusted Laptop".to_string(),
            ed25519_pub: [0x22u8; 32],
            pairwise_secret: k_pair,
            fingerprint: "1122-3344-5566-7788".to_string(),
            paired_at: 100.0,
        });
        let trusted_cfg =
            Sht3ClientConfig::new("trusted-dev", AudioProfile::PcmLossless.to_audio_params(20));
        let mut conn = Sht3ClientConnection::connect(v3_addr, trusted_cfg, &k_pair)
            .expect("trusted peer should pass v4 handshake");
        assert!(conn.rtp_port() > 0);
        let _ = conn.send_close();

        ctrl.stop_listener(Some(false)).unwrap();
        assert!(!ctrl.is_v3_active());
    }

    #[cfg(feature = "gstreamer")]
    #[test]
    fn test_revoke_trusted_peer_immediately_drops_active_sht3_session() {
        use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
        use std::sync::Mutex;
        use std::time::{Duration, Instant};

        use crate::pairing::TrustedPeer;
        use crate::transport::{
            AudioProfile, CaptureEndpoint, OutboundConfig, OutboundSessionHandle, PlaybackEndpoint,
            Sht3ClientConfig, Sht3ClientConnection, TransportError,
        };

        let runner = Arc::new(MockRunner::new(|args, _| {
            Ok(CommandResult {
                args: args.to_vec(),
                returncode: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }));

        let free_port = {
            let sock = TcpListener::bind("127.0.0.1:0").unwrap();
            sock.local_addr().unwrap().port()
        };

        let mut cfg = Config::default();
        cfg.sharing.security = "paired".to_string();
        cfg.sharing.v3_port = free_port;
        cfg.sharing.rtp_port = 0;

        let backend = Arc::new(LinuxAudioBackend::with_runner(runner));
        let ctrl = Controller::new(
            Some(cfg),
            Some(State::default()),
            Some(backend),
            None,
            None,
            false,
        );
        ctrl.set_save_enabled(false);
        ctrl.set_playback_factory(Arc::new(|_peer_id| {
            Ok(PlaybackEndpoint::TestAppSink(Arc::new(Mutex::new(
                Vec::new(),
            ))))
        }));

        let k_pair = [0x7Au8; 32];
        ctrl.record_trusted_peer(TrustedPeer {
            device_id: "peer-revoked".to_string(),
            display_name: "Guest Laptop".to_string(),
            ed25519_pub: [0x55u8; 32],
            pairwise_secret: k_pair,
            fingerprint: "AABB-CCDD-EEFF-0011".to_string(),
            paired_at: 200.0,
        });

        ctrl.start_listener(Some(false)).unwrap();
        let v3_addr = SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            ctrl.sht3_manager.listener_addr().unwrap().port(),
        );

        let outbound_cfg =
            OutboundConfig::new("peer-revoked", v3_addr, zeroize::Zeroizing::new(k_pair))
                .with_profile(AudioProfile::PcmLossless);
        let outbound = OutboundSessionHandle::start(
            outbound_cfg,
            CaptureEndpoint::TestSineWave {
                freq_hz: 440.0,
                netsim: None,
            },
        )
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        while !ctrl
            .sht3_manager
            .active_inbound_peers()
            .iter()
            .any(|id| id == "peer-revoked")
        {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for peer-revoked to appear in active_inbound_peers"
            );
            std::thread::sleep(Duration::from_millis(15));
        }

        // Revoke the peer -> must immediately disconnect the active inbound session.
        let revoke_res = ctrl.revoke_trusted_peer("peer-revoked").unwrap();
        assert_eq!(revoke_res["ok"], true);
        assert_eq!(revoke_res["revoked"], true);
        assert_eq!(revoke_res["disconnected"], true);
        assert!(
            ctrl.sht3_manager.active_inbound_peers().is_empty(),
            "revoked peer must be dropped immediately from transport manager"
        );

        // Subsequent reconnect attempt must fail authentication.
        let retry_cfg = Sht3ClientConfig::new(
            "peer-revoked",
            AudioProfile::PcmLossless.to_audio_params(20),
        );
        let retry = Sht3ClientConnection::connect(v3_addr, retry_cfg, &k_pair);
        assert!(matches!(retry, Err(TransportError::AuthRejected(_))));

        outbound.stop();
        ctrl.stop_listener(Some(false)).unwrap();
    }
}
