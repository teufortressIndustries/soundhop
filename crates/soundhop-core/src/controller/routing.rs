//! Per-application audio stream routing and persistent routing rules.

use crate::constants::SINK_PREFIX;
use crate::util::lock_or_recover;

use super::{Controller, ControllerError};

impl Controller {
    pub fn list_apps(&self) -> Vec<serde_json::Value> {
        let st = lock_or_recover(&self.state).clone();
        let cfg = lock_or_recover(&self.config).clone();
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
        let st_snap = lock_or_recover(&self.state).clone();
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
            let mut st = lock_or_recover(&self.state);
            if target_dest == "local" {
                st.routed_apps.remove(&key_lower);
                st.routed_apps.remove(&id_str.to_lowercase());
            } else {
                st.routed_apps
                    .insert(key_lower.clone(), target_dest.clone());
            }
        }
        if save_persistent {
            let mut cfg = lock_or_recover(&self.config);
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

    /// Clear temporary and/or persistent routing overrides for one app or all apps.
    pub fn unroute_app(
        &self,
        app: Option<&str>,
        remove_rule: bool,
    ) -> Result<serde_json::Value, ControllerError> {
        let trimmed = app.map(str::trim).filter(|s| !s.is_empty());
        if let Some(target_app) = trimmed {
            let key = target_app.to_lowercase();
            {
                let mut st = lock_or_recover(&self.state);
                st.routed_apps.remove(&key);
            }
            if remove_rule {
                let mut cfg = lock_or_recover(&self.config);
                cfg.routing.always_remote.retain(|x| x != &key);
                cfg.routing.always_local.retain(|x| x != &key);
            }
            self.save();
            // Best-effort restore to default/local sink if backend is available
            let st_snap = lock_or_recover(&self.state).clone();
            let target_sink = if !st_snap.connected_sink.is_empty()
                && lock_or_recover(&self.config).routing.default_mode == "all"
            {
                st_snap.connected_sink
            } else {
                let mut loc = self.local_default_candidate(&st_snap.previous_default_sink);
                if loc.is_empty() {
                    loc = self.backend.default_sink().unwrap_or_default();
                }
                loc
            };
            let moved = if !target_sink.is_empty() {
                self.backend
                    .move_app_streams(target_app, &target_sink)
                    .unwrap_or(0)
            } else {
                0
            };
            Ok(serde_json::json!({
                "ok": true,
                "app": target_app,
                "remove_rule": remove_rule,
                "moved": moved,
            }))
        } else {
            lock_or_recover(&self.state).routed_apps.clear();
            if remove_rule {
                let mut cfg = lock_or_recover(&self.config);
                cfg.routing.always_remote.clear();
                cfg.routing.always_local.clear();
            }
            self.save();
            Ok(serde_json::json!({ "ok": true, "unrouted_all": true }))
        }
    }

    pub fn enforce_routed_apps(&self) -> usize {
        let st = lock_or_recover(&self.state).clone();
        let cfg = lock_or_recover(&self.config).clone();
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

    pub(super) fn remember_previous_default(&self) {
        let current = self.backend.default_sink().unwrap_or_default();
        let mut st = lock_or_recover(&self.state);
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

    pub(super) fn make_default(
        &self,
        sink_name: &str,
        move_streams: bool,
    ) -> Result<(), ControllerError> {
        self.backend.set_default_sink(sink_name)?;
        if move_streams {
            let _ = self.backend.move_sink_inputs(sink_name, None);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::backend::linux::LinuxAudioBackend;
    use crate::config::Config;
    use crate::controller::Controller;
    use crate::state::State;
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

        let unroute_res = ctrl.unroute_app(Some("spotify"), true).unwrap();
        assert_eq!(unroute_res["remove_rule"], true);
        assert!(!ctrl
            .config
            .lock()
            .unwrap()
            .routing
            .always_remote
            .contains(&"spotify".to_string()));

        let clear_res = ctrl.clear_pairing();
        assert_eq!(clear_res["cleared"], true);
        assert!(ctrl.config.lock().unwrap().sharing.psk_hash.is_empty());
    }
}
