//! Audio sharing listener lifecycle, PIN/PSK pairing, and Pairing v2 interactive requests.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::backend::Listener;
use crate::pairing::{
    identity_key_path, initiate_pair_request, DeviceIdentity, PairingManager, PendingPairRequest,
    TrustStore, TrustedPeer, DEFAULT_PAIR_PORT, PENDING_REQUEST_TTL_SECS,
};
use crate::security::{
    derive_key_hex, derive_pa_cookie, generate_pairing_pin, soundhop_cookie_path, write_pa_cookie,
};
use crate::state::PairedPeer;
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

    pub(super) fn ensure_listener_internal(&self) -> Result<Listener, ControllerError> {
        let cfg = lock_or_recover(&self.config).clone();
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
        self.pairing_manager().trust_store().upsert(peer.clone());
        lock_or_recover(&self.state).upsert_trusted_peer(peer);
        self.save();
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
        let outcome = initiate_pair_request(
            trimmed_host,
            port,
            &identity,
            &display_name,
            code,
            Duration::from_secs(PENDING_REQUEST_TTL_SECS),
            None,
        )
        .map_err(|e| ControllerError::Message(e.to_string()))?;

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
        Ok(serde_json::json!({
            "ok": true,
            "denied": true,
            "request": req,
        }))
    }

    /// Revoke a trusted peer by `device_id`, removing it from both `State` and `TrustStore`.
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
        if removed_state {
            self.save();
        }
        Ok(serde_json::json!({
            "ok": true,
            "revoked": removed_store || removed_state,
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
}
