//! PipeWire / PulseAudio backend built on the `pactl` CLI, including Multi-Room
//! `module-combine-sink` support, per-app stream routing, and microphone pass-through.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::backend::{
    listener_security_from_args, AudioBackend, BackendError, Listener, Module, Sink, SinkInput,
    SinkInputInfo, Source, SourceOutput, SourceTunnelParams, TunnelParams,
};
use crate::constants::{
    DEFAULT_LAN_NETWORKS, SINK_PREFIX, SINK_TIMEOUT_SECS, SOURCE_PREFIX, SOURCE_TIMEOUT_SECS,
};
use crate::security::{ensure_pa_cookie_at, soundhop_cookie_path};
use crate::util::{sanitize_sink_name, tcp_server_arg, CommandResult, ProcessRunner, SystemRunner};

fn file_sha256(path: &Path) -> Option<[u8; 32]> {
    let bytes = std::fs::read(path).ok()?;
    let digest = Sha256::digest(&bytes);
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    Some(out)
}

pub struct LinuxAudioBackend {
    pub pactl: String,
    runner: Arc<dyn ProcessRunner>,
    sleep_fn: Arc<dyn Fn(Duration) + Send + Sync>,
    info_cache: Mutex<Option<HashMap<String, String>>>,
    remote_prev_default_sources: Mutex<HashMap<String, String>>,
    loaded_cookie_hash: Mutex<Option<[u8; 32]>>,
    cookie_path_override: Option<PathBuf>,
}

impl Default for LinuxAudioBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl LinuxAudioBackend {
    pub fn new() -> Self {
        Self {
            pactl: "pactl".to_string(),
            runner: Arc::new(SystemRunner),
            sleep_fn: Arc::new(std::thread::sleep),
            info_cache: Mutex::new(None),
            remote_prev_default_sources: Mutex::new(HashMap::new()),
            loaded_cookie_hash: Mutex::new(None),
            cookie_path_override: None,
        }
    }

    pub fn with_runner(runner: Arc<dyn ProcessRunner>) -> Self {
        Self {
            pactl: "pactl".to_string(),
            runner,
            sleep_fn: Arc::new(|_| {}),
            info_cache: Mutex::new(None),
            remote_prev_default_sources: Mutex::new(HashMap::new()),
            loaded_cookie_hash: Mutex::new(None),
            cookie_path_override: None,
        }
    }

    #[cfg(test)]
    pub fn with_runner_and_cookie_path(
        runner: Arc<dyn ProcessRunner>,
        cookie_path: PathBuf,
    ) -> Self {
        Self {
            pactl: "pactl".to_string(),
            runner,
            sleep_fn: Arc::new(|_| {}),
            info_cache: Mutex::new(None),
            remote_prev_default_sources: Mutex::new(HashMap::new()),
            loaded_cookie_hash: Mutex::new(None),
            cookie_path_override: Some(cookie_path),
        }
    }

    fn cookie_path(&self) -> PathBuf {
        self.cookie_path_override
            .clone()
            .unwrap_or_else(soundhop_cookie_path)
    }

    fn pactl_call(&self, args: &[&str], check: bool) -> Result<CommandResult, BackendError> {
        let mut full = Vec::with_capacity(args.len() + 1);
        full.push(self.pactl.clone());
        for a in args {
            full.push((*a).to_string());
        }
        let mut env_map = HashMap::new();
        env_map.insert("LC_ALL".to_string(), "C".to_string());
        env_map.insert("LANG".to_string(), "C".to_string());
        if args.contains(&"-s") {
            let cookie_file = soundhop_cookie_path();
            if cookie_file.exists() {
                env_map.insert(
                    "PULSE_COOKIE".to_string(),
                    cookie_file.to_string_lossy().into_owned(),
                );
            }
        }
        let res = self
            .runner
            .run(&full, Some(&env_map), Duration::from_secs(15))?;
        if check && !res.ok() {
            let msg = res.combined_output();
            return Err(BackendError::Message(if msg.is_empty() {
                format!("pactl {} failed", args.join(" "))
            } else {
                msg
            }));
        }
        Ok(res)
    }

    pub fn info(&self, refresh: bool) -> Result<HashMap<String, String>, BackendError> {
        if !refresh {
            if let Some(cached) = self
                .info_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
            {
                return Ok(cached);
            }
        }
        let res = self.pactl_call(&["info"], true)?;
        let mut map = HashMap::new();
        for line in res.stdout.lines() {
            if let Some((k, v)) = line.split_once(": ") {
                map.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
        *self.info_cache.lock().unwrap_or_else(|e| e.into_inner()) = Some(map.clone());
        Ok(map)
    }

    pub fn module_for_sink(&self, sink_name: &str) -> Result<Option<Module>, BackendError> {
        for m in self.modules()? {
            if (m.name.starts_with("module-tunnel-") || m.name == "module-combine-sink")
                && m.arg("sink_name").as_deref() == Some(sink_name)
            {
                return Ok(Some(m));
            }
        }
        Ok(None)
    }

    pub fn module_for_source(&self, source_name: &str) -> Result<Option<Module>, BackendError> {
        for m in self.modules()? {
            if m.name.starts_with("module-tunnel-")
                && m.arg("source_name").as_deref() == Some(source_name)
            {
                return Ok(Some(m));
            }
        }
        Ok(None)
    }

    fn wait_for_sink(&self, name: &str, timeout_secs: f64) -> bool {
        let deadline = Instant::now() + Duration::from_secs_f64(timeout_secs);
        loop {
            if self.sink_exists(name).unwrap_or(false) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            (self.sleep_fn)(Duration::from_millis(80));
        }
    }

    fn wait_for_sink_gone(&self, name: &str, timeout_secs: f64) -> bool {
        let deadline = Instant::now() + Duration::from_secs_f64(timeout_secs);
        loop {
            if !self.sink_exists(name).unwrap_or(false) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            (self.sleep_fn)(Duration::from_millis(80));
        }
    }

    fn stop_send_mic_to_remote_internal(
        &self,
        host: &str,
        port: u16,
        remote_null_id: Option<u32>,
        remote_remap_id: Option<u32>,
        local_tunnel_id: Option<u32>,
        local_loopback_id: Option<u32>,
    ) {
        if let Some(loopback_id) = local_loopback_id {
            let _ = self.unload_module(loopback_id, true);
        }
        if let Some(tunnel_id) = local_tunnel_id {
            let _ = self.unload_module(tunnel_id, true);
        }
        if !host.is_empty() && port > 0 {
            let remote_target = tcp_server_arg(host, port);
            if let Some(prev_src) = self
                .remote_prev_default_sources
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&remote_target)
            {
                if !prev_src.is_empty() {
                    let _ = self.pactl_call(
                        &["-s", &remote_target, "set-default-source", &prev_src],
                        false,
                    );
                }
            }
            if let Some(remap_id) = remote_remap_id {
                let _ = self.pactl_call(
                    &["-s", &remote_target, "unload-module", &remap_id.to_string()],
                    false,
                );
            }
            if let Some(null_id) = remote_null_id {
                let _ = self.pactl_call(
                    &["-s", &remote_target, "unload-module", &null_id.to_string()],
                    false,
                );
            }
        }
    }

    fn wait_for_source(&self, name: &str, timeout_secs: f64) -> bool {
        let deadline = Instant::now() + Duration::from_secs_f64(timeout_secs);
        loop {
            if self.source_exists(name).unwrap_or(false) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            (self.sleep_fn)(Duration::from_millis(80));
        }
    }

    fn source_output_owner_modules(&self) -> HashMap<u32, u32> {
        let mut map = HashMap::new();
        let Ok(res) = self.pactl_call(&["list", "source-outputs"], false) else {
            return map;
        };
        if !res.ok() {
            return map;
        }
        let mut current_id: Option<u32> = None;
        for line in res.stdout.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("Source Output #") {
                current_id = rest.trim().parse::<u32>().ok();
            } else if let Some(id) = current_id {
                if let Some(rest) = trimmed.strip_prefix("Owner Module:") {
                    if let Ok(mod_id) = rest.trim().parse::<u32>() {
                        map.insert(id, mod_id);
                    }
                }
            }
        }
        map
    }
}

impl AudioBackend for LinuxAudioBackend {
    fn available(&self) -> bool {
        if !self.runner.command_exists(&self.pactl) {
            return false;
        }
        self.pactl_call(&["info"], false)
            .map(|r| r.ok())
            .unwrap_or(false)
    }

    fn invalidate(&self) {
        *self.info_cache.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    fn server_name(&self) -> Result<String, BackendError> {
        Ok(self
            .info(false)?
            .get("Server Name")
            .cloned()
            .unwrap_or_else(|| "PulseAudio".to_string()))
    }

    fn server_version(&self) -> Result<String, BackendError> {
        Ok(self
            .info(false)?
            .get("Server Version")
            .cloned()
            .unwrap_or_default())
    }

    fn is_pipewire(&self) -> Result<bool, BackendError> {
        let name = self.server_name()?.to_lowercase();
        let ver = self.server_version()?.to_lowercase();
        Ok(name.contains("pipewire") || ver.contains("pipewire"))
    }

    fn default_sink(&self) -> Result<String, BackendError> {
        Ok(self
            .info(false)?
            .get("Default Sink")
            .cloned()
            .unwrap_or_default())
    }

    fn default_source(&self) -> Result<String, BackendError> {
        Ok(self
            .info(false)?
            .get("Default Source")
            .cloned()
            .unwrap_or_default())
    }

    fn modules(&self) -> Result<Vec<Module>, BackendError> {
        let res = self.pactl_call(&["list", "short", "modules"], true)?;
        let mut out = Vec::new();
        for line in res.stdout.lines() {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() < 2 {
                continue;
            }
            let Ok(id) = parts[0].trim().parse::<u32>() else {
                continue;
            };
            out.push(Module {
                id,
                name: parts[1].trim().to_string(),
                args: parts
                    .get(2)
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default(),
            });
        }
        Ok(out)
    }

    fn sinks(&self) -> Result<Vec<Sink>, BackendError> {
        let res = self.pactl_call(&["list", "short", "sinks"], true)?;
        let mut out = Vec::new();
        for line in res.stdout.lines() {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() < 2 {
                continue;
            }
            let Ok(id) = parts[0].trim().parse::<u32>() else {
                continue;
            };
            out.push(Sink {
                id,
                name: parts[1].trim().to_string(),
                driver: parts
                    .get(2)
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default(),
                spec: parts
                    .get(3)
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default(),
                state: parts
                    .get(4)
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default(),
            });
        }
        Ok(out)
    }

    fn sources(&self) -> Result<Vec<Source>, BackendError> {
        let res = self.pactl_call(&["list", "short", "sources"], true)?;
        let mut out = Vec::new();
        for line in res.stdout.lines() {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() < 2 {
                continue;
            }
            let Ok(id) = parts[0].trim().parse::<u32>() else {
                continue;
            };
            out.push(Source {
                id,
                name: parts[1].trim().to_string(),
                driver: parts
                    .get(2)
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default(),
                spec: parts
                    .get(3)
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default(),
                state: parts
                    .get(4)
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default(),
            });
        }
        Ok(out)
    }

    fn sink_inputs(&self) -> Result<Vec<SinkInput>, BackendError> {
        let res = self.pactl_call(&["list", "short", "sink-inputs"], true)?;
        let mut out = Vec::new();
        for line in res.stdout.lines() {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() < 2 {
                continue;
            }
            let (Ok(id), Ok(sink)) = (
                parts[0].trim().parse::<u32>(),
                parts[1].trim().parse::<u32>(),
            ) else {
                continue;
            };
            let client = parts
                .get(2)
                .and_then(|s| s.trim().parse::<u32>().ok())
                .unwrap_or(0);
            let driver = parts
                .get(3)
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            out.push(SinkInput {
                id,
                sink,
                client,
                driver,
            });
        }
        Ok(out)
    }

    fn detailed_sink_inputs(&self) -> Result<Vec<SinkInputInfo>, BackendError> {
        let sinks_by_id: HashMap<u32, String> = self
            .sinks()
            .unwrap_or_default()
            .into_iter()
            .map(|s| (s.id, s.name))
            .collect();
        let remote_sink_names: HashSet<String> = self
            .soundhop_modules()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|m| m.arg("sink_name"))
            .collect();

        if let Ok(res) = self.pactl_call(&["-f", "json", "list", "sink-inputs"], false) {
            if res.ok() && !res.stdout.trim().is_empty() {
                if let Ok(serde_json::Value::Array(items)) = serde_json::from_str(&res.stdout) {
                    let mut parsed = Vec::new();
                    for raw in items {
                        let id = raw.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                        let sink = raw.get("sink").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                        let sink_name = sinks_by_id.get(&sink).cloned().unwrap_or_default();
                        let mute = raw.get("mute").and_then(|v| v.as_bool()).unwrap_or(false);
                        let mut vol_pct = 100u32;
                        if let Some(vol_obj) = raw.get("volume").and_then(|v| v.as_object()) {
                            for ch_val in vol_obj.values() {
                                if let Some(vp) =
                                    ch_val.get("value_percent").and_then(|v| v.as_str())
                                {
                                    if let Ok(n) = vp.trim().trim_end_matches('%').parse::<u32>() {
                                        vol_pct = n;
                                        break;
                                    }
                                }
                            }
                        }
                        let props = raw.get("properties").and_then(|v| v.as_object());
                        let prop_str = |key: &str| -> String {
                            props
                                .and_then(|p| p.get(key))
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .trim()
                                .to_string()
                        };
                        let node_name = prop_str("node.name");
                        let mut app_name = prop_str("application.name");
                        let mut binary = prop_str("application.process.binary");
                        if app_name.is_empty() && !node_name.is_empty() {
                            app_name = node_name.clone();
                        }
                        if binary.is_empty() && !node_name.is_empty() {
                            binary = node_name.clone();
                        }
                        let pid = prop_str("application.process.id")
                            .parse::<u32>()
                            .unwrap_or(0);
                        let is_remote = remote_sink_names.contains(&sink_name)
                            || sink_name.starts_with(SINK_PREFIX);
                        parsed.push(SinkInputInfo {
                            id,
                            sink,
                            sink_name,
                            app_name,
                            binary,
                            node_name,
                            pid,
                            media_name: prop_str("media.name"),
                            icon_name: prop_str("application.icon_name"),
                            volume_percent: vol_pct,
                            mute,
                            is_remote,
                        });
                    }
                    return Ok(parsed);
                }
            }
        }

        let Ok(res) = self.pactl_call(&["list", "sink-inputs"], false) else {
            return Ok(Vec::new());
        };
        if !res.ok() {
            return Ok(Vec::new());
        }
        Ok(parse_text_sink_inputs(
            &res.stdout,
            &sinks_by_id,
            &remote_sink_names,
        ))
    }

    fn source_outputs(&self) -> Result<Vec<SourceOutput>, BackendError> {
        let Ok(res) = self.pactl_call(&["list", "short", "source-outputs"], false) else {
            return Ok(Vec::new());
        };
        if !res.ok() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for line in res.stdout.lines() {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() < 2 {
                continue;
            }
            let Ok(id) = parts[0].trim().parse::<u32>() else {
                continue;
            };
            let source = parts[1].trim().parse::<u32>().unwrap_or(0);
            let client = parts
                .get(2)
                .and_then(|s| s.trim().parse::<u32>().ok())
                .unwrap_or(0);
            let driver = parts
                .get(3)
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            out.push(SourceOutput {
                id,
                source,
                client,
                driver,
            });
        }
        Ok(out)
    }

    fn load_module(&self, name: &str, args: &[String]) -> Result<u32, BackendError> {
        let mut call_args: Vec<&str> = Vec::with_capacity(args.len() + 2);
        call_args.push("load-module");
        call_args.push(name);
        for a in args {
            call_args.push(a.as_str());
        }
        let res = self.pactl_call(&call_args, true)?;
        let text = res.stdout.trim();
        for token in text.split_whitespace() {
            if let Ok(id) = token.parse::<u32>() {
                return Ok(id);
            }
        }
        Err(BackendError::Message(format!(
            "pactl did not return a module id (output: {text:?})"
        )))
    }

    fn unload_module(&self, module_id: u32, ignore_missing: bool) -> Result<bool, BackendError> {
        let id_str = module_id.to_string();
        let res = self.pactl_call(&["unload-module", &id_str], false)?;
        if !res.ok() {
            let msg = res.combined_output();
            if ignore_missing || msg.contains("No such entity") || msg.contains("not loaded") {
                return Ok(false);
            }
            return Err(BackendError::Message(if msg.is_empty() {
                format!("Could not unload module {module_id}")
            } else {
                msg
            }));
        }
        Ok(true)
    }

    fn unload_tunnels(&self, sink_prefix: &str) -> Result<Vec<u32>, BackendError> {
        let mut removed = Vec::new();
        for module in self.modules()? {
            let is_tunnel_or_combine =
                module.name.starts_with("module-tunnel-") || module.name == "module-combine-sink";
            if !is_tunnel_or_combine {
                continue;
            }
            if let Some(sn) = module.arg("sink_name") {
                if sn.starts_with(sink_prefix) && self.unload_module(module.id, true)? {
                    removed.push(module.id);
                }
            }
        }
        Ok(removed)
    }

    fn soundhop_modules(&self) -> Result<Vec<Module>, BackendError> {
        let mut out = Vec::new();
        for m in self.modules()? {
            let sn = m.arg("sink_name").unwrap_or_default();
            let src = m.arg("source_name").unwrap_or_default();
            if sn.starts_with(SINK_PREFIX)
                || sn.starts_with("soundhop_rem_mic_sink_")
                || src.starts_with(SOURCE_PREFIX)
                || src.starts_with("soundhop_rem_mic_")
            {
                out.push(m);
            }
        }
        Ok(out)
    }

    fn set_default_sink(&self, name: &str) -> Result<(), BackendError> {
        if name.trim().is_empty() {
            return Err(BackendError::Message(
                "Refusing to set an empty default sink".to_string(),
            ));
        }
        self.pactl_call(&["set-default-sink", name], true)?;
        self.invalidate();
        Ok(())
    }

    fn set_default_source(&self, name: &str) -> Result<(), BackendError> {
        if name.trim().is_empty() {
            return Err(BackendError::Message(
                "Refusing to set an empty default source".to_string(),
            ));
        }
        self.pactl_call(&["set-default-source", name], true)?;
        self.invalidate();
        Ok(())
    }

    fn move_sink_inputs(&self, target: &str, source: Option<&str>) -> Result<usize, BackendError> {
        let wanted_sink_id = if let Some(src_name) = source {
            match self.sink(src_name)? {
                Some(s) => Some(s.id),
                None => return Ok(0),
            }
        } else {
            None
        };
        let mut moved = 0;
        for inp in self.sink_inputs()? {
            if let Some(wanted) = wanted_sink_id {
                if inp.sink != wanted {
                    continue;
                }
            }
            let id_str = inp.id.to_string();
            if let Ok(res) = self.pactl_call(&["move-sink-input", &id_str, target], false) {
                if res.ok() {
                    moved += 1;
                }
            }
        }
        Ok(moved)
    }

    fn move_source_outputs(
        &self,
        target: &str,
        source: Option<&str>,
        exclude_modules: &[u32],
    ) -> Result<usize, BackendError> {
        let wanted_src_id = if let Some(src_name) = source {
            match self.source(src_name)? {
                Some(s) => Some(s.id),
                None => return Ok(0),
            }
        } else {
            None
        };
        let exclude_set: HashSet<u32> = exclude_modules.iter().copied().collect();
        let owner_modules: HashMap<u32, u32> = if exclude_set.is_empty() {
            HashMap::new()
        } else {
            self.source_output_owner_modules()
        };
        let mut moved = 0;
        for so in self.source_outputs()? {
            if !exclude_set.is_empty() {
                if let Some(owner_mod) = owner_modules.get(&so.id) {
                    if exclude_set.contains(owner_mod) {
                        continue;
                    }
                }
            }
            if let Some(wanted) = wanted_src_id {
                if so.source != wanted {
                    continue;
                }
            }
            let id_str = so.id.to_string();
            if let Ok(res) = self.pactl_call(&["move-source-output", &id_str, target], false) {
                if res.ok() {
                    moved += 1;
                }
            }
        }
        Ok(moved)
    }

    fn move_app_streams(&self, identifier: &str, target: &str) -> Result<usize, BackendError> {
        let needle = identifier.trim().to_lowercase();
        if needle.is_empty() {
            return Ok(0);
        }
        let mut moved = 0;
        for item in self.detailed_sink_inputs()? {
            let matched = item.id.to_string() == needle
                || (item.pid > 0 && item.pid.to_string() == needle)
                || (!item.binary.is_empty() && item.binary.to_lowercase().contains(&needle))
                || (!item.node_name.is_empty() && item.node_name.to_lowercase().contains(&needle))
                || (!item.app_name.is_empty()
                    && (item.app_name.to_lowercase().contains(&needle)
                        || needle.contains(&item.app_name.to_lowercase())))
                || (!item.media_name.is_empty()
                    && item.media_name.to_lowercase().contains(&needle));
            if matched {
                if !item.sink_name.is_empty() && item.sink_name == target {
                    continue;
                }
                let id_str = item.id.to_string();
                if let Ok(res) = self.pactl_call(&["move-sink-input", &id_str, target], false) {
                    if res.ok() {
                        moved += 1;
                    }
                }
            }
        }
        Ok(moved)
    }

    fn ensure_tcp_listener(
        &self,
        port: u16,
        security: &str,
        allowed_networks: &[String],
    ) -> Result<Listener, BackendError> {
        let desired_sec = match security {
            "psk" | "paired" | "pin" | "cookie" => "cookie",
            "open" => "open",
            _ => "trusted-lan",
        };
        let desired_acl = if desired_sec == "open" {
            None
        } else {
            Some(effective_allowed_networks(allowed_networks).join(";"))
        };
        let cookie_path = self.cookie_path();
        let desired_cookie = if desired_sec == "cookie" {
            Some(cookie_path.to_string_lossy().into_owned())
        } else {
            None
        };
        let current_cookie_hash = if desired_sec == "cookie" {
            file_sha256(&cookie_path)
        } else {
            None
        };
        let loaded_cookie_hash = *self
            .loaded_cookie_hash
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for module in self.modules()? {
            if module.name != "module-native-protocol-tcp" {
                continue;
            }
            let mod_port = module
                .arg("port")
                .and_then(|p| p.parse::<u16>().ok())
                .unwrap_or(4713);
            if mod_port == port {
                let actual_sec = listener_security_from_args(&module.args);
                let actual_acl = module.arg("auth-ip-acl");
                let actual_cookie = module.arg("auth-cookie");
                let cookie_matches = if desired_sec == "cookie" {
                    actual_cookie == desired_cookie
                        && module.arg("auth-cookie-enabled").as_deref() != Some("0")
                        && current_cookie_hash.is_some()
                        && loaded_cookie_hash == current_cookie_hash
                } else {
                    true
                };
                let matches =
                    actual_sec == desired_sec && actual_acl == desired_acl && cookie_matches;
                if matches {
                    return Ok(Listener {
                        module_id: module.id,
                        created: false,
                        args: module.args.clone(),
                        port,
                        security: desired_sec.to_string(),
                    });
                }
                // Security mode, cookie path/content, or allowed_networks changed: replace the existing listener
                let _ = self.unload_module(module.id, true);
            }
        }

        let (args, new_cookie_hash): (Vec<String>, Option<[u8; 32]>) = match desired_sec {
            "cookie" => {
                let cookie_file = ensure_pa_cookie_at(Some(&cookie_path), None).map_err(|e| {
                    BackendError::Message(format!("Failed to prepare PulseAudio cookie: {e}"))
                })?;
                let hash = file_sha256(&cookie_file);
                (
                    build_listener_args(port, desired_sec, allowed_networks, Some(&cookie_file)),
                    hash,
                )
            }
            _ => (
                build_listener_args(port, desired_sec, allowed_networks, None),
                None,
            ),
        };
        let module_id = self.load_module("module-native-protocol-tcp", &args)?;
        *self
            .loaded_cookie_hash
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = new_cookie_hash;
        Ok(Listener {
            module_id,
            created: true,
            args: args.join(" "),
            port,
            security: desired_sec.to_string(),
        })
    }

    fn connect_tunnel(&self, params: &TunnelParams<'_>) -> Result<u32, BackendError> {
        let sink_name = sanitize_sink_name(params.sink_name, 64);
        if self.sink(&sink_name)?.is_some() {
            if let Some(mod_entry) = self.module_for_sink(&sink_name)? {
                let _ = self.unload_module(mod_entry.id, true);
            }
            (self.sleep_fn)(Duration::from_millis(120));
            if self.sink_exists(&sink_name)? {
                return Err(BackendError::Message(format!(
                    "A sink named {sink_name:?} already exists and is not owned by SoundHop"
                )));
            }
        }

        let safe_extra: Vec<String> = params
            .extra_args
            .iter()
            .filter_map(|arg| crate::config::validate_extra_tunnel_arg(arg).ok())
            .collect();
        let safe_desc = sanitize_sink_name(params.description, 64).replace('_', "-");
        let mut args = vec![
            format!("server={}", tcp_server_arg(params.host, params.port)),
            format!("sink_name={sink_name}"),
        ];
        if params.reconnect_delay_ms > 0 {
            args.push(format!(
                "reconnect_delay_msec={}",
                params.reconnect_delay_ms
            ));
        }
        if let Some(lat) = params.latency_ms {
            if lat > 0 {
                args.push(format!("latency_msec={lat}"));
            }
        }
        if !safe_extra.iter().any(|x| x.starts_with("rate=")) {
            args.push("rate=48000".to_string());
        }
        if !safe_extra.iter().any(|x| x.starts_with("channels=")) {
            args.push("channels=2".to_string());
        }
        if !safe_extra.iter().any(|x| x.starts_with("format=")) {
            args.push("format=s16le".to_string());
        }
        if let Some(cookie_file) = params.cookie_path.as_deref().filter(|p| p.exists()) {
            args.push(format!("cookie={}", cookie_file.display()));
        }
        args.extend(safe_extra);
        args.push(format!(
            "sink_properties=device.description=SoundHop-{safe_desc}"
        ));

        let module_id = self.load_module("module-tunnel-sink", &args)?;
        if !self.wait_for_sink(&sink_name, SINK_TIMEOUT_SECS) {
            let _ = self.unload_module(module_id, true);
            return Err(BackendError::Message(format!(
                "PipeWire accepted the tunnel but sink {sink_name:?} did not appear"
            )));
        }
        Ok(module_id)
    }

    fn disconnect_tunnel(&self, sink_name: &str) -> Result<bool, BackendError> {
        let Some(module) = self.module_for_sink(sink_name)? else {
            return Ok(false);
        };
        self.unload_module(module.id, true)
    }

    fn create_combined_sink(
        &self,
        sink_name: &str,
        description: &str,
        slave_sinks: &[String],
    ) -> Result<u32, BackendError> {
        let sink_name = sanitize_sink_name(sink_name, 64);
        if let Some(existing_mod) = self.module_for_sink(&sink_name)? {
            let _ = self.unload_module(existing_mod.id, true);
            if !self.wait_for_sink_gone(&sink_name, SINK_TIMEOUT_SECS) {
                return Err(BackendError::Message(format!(
                    "Existing combined sink {sink_name:?} did not disappear after unload"
                )));
            }
        }
        let safe_desc = sanitize_sink_name(description, 64).replace('_', "-");
        let slaves_arg = slave_sinks.join(",");
        let args = vec![
            format!("sink_name={sink_name}"),
            format!("slaves={slaves_arg}"),
            format!("sink_properties=device.description=SoundHop-MultiRoom-{safe_desc}"),
        ];
        let module_id = self.load_module("module-combine-sink", &args)?;
        if !self.wait_for_sink(&sink_name, SINK_TIMEOUT_SECS) {
            let _ = self.unload_module(module_id, true);
            return Err(BackendError::Message(format!(
                "Combined sink {sink_name:?} did not appear"
            )));
        }
        Ok(module_id)
    }

    fn connect_tunnel_source(&self, params: &SourceTunnelParams<'_>) -> Result<u32, BackendError> {
        let source_name = sanitize_sink_name(params.source_name, 64);
        if self.source(&source_name)?.is_some() {
            if let Some(m) = self.module_for_source(&source_name)? {
                let _ = self.unload_module(m.id, true);
            }
            (self.sleep_fn)(Duration::from_millis(120));
            if self.source_exists(&source_name)? {
                return Err(BackendError::Message(format!(
                    "A source named {source_name:?} already exists and is not owned by SoundHop"
                )));
            }
        }
        let safe_desc = sanitize_sink_name(params.description, 64).replace('_', "-");
        let mut args = vec![
            format!("server={}", tcp_server_arg(params.host, params.port)),
            format!("source_name={source_name}"),
        ];
        if !params.remote_source.is_empty() {
            args.push(format!("source={}", params.remote_source));
        }
        if params.reconnect_delay_ms > 0 {
            args.push(format!(
                "reconnect_delay_msec={}",
                params.reconnect_delay_ms
            ));
        }
        if let Some(lat) = params.latency_ms {
            if lat > 0 {
                args.push(format!("latency_msec={lat}"));
            }
        }
        let safe_extra: Vec<String> = params
            .extra_args
            .iter()
            .filter_map(|arg| crate::config::validate_extra_tunnel_arg(arg).ok())
            .collect();
        if !safe_extra.iter().any(|x| x.starts_with("rate=")) {
            args.push("rate=48000".to_string());
        }
        if !safe_extra.iter().any(|x| x.starts_with("channels=")) {
            args.push("channels=2".to_string());
        }
        if !safe_extra.iter().any(|x| x.starts_with("format=")) {
            args.push("format=s16le".to_string());
        }
        let cookie_file = self.cookie_path();
        if cookie_file.exists() {
            args.push(format!("cookie={}", cookie_file.display()));
        }
        args.extend(safe_extra);
        args.push(format!(
            "source_properties=device.description=SoundHop-Microphone-{safe_desc}"
        ));
        let module_id = self.load_module("module-tunnel-source", &args)?;
        if !self.wait_for_source(&source_name, SOURCE_TIMEOUT_SECS) {
            let _ = self.unload_module(module_id, true);
            return Err(BackendError::Message(format!(
                "PipeWire accepted the tunnel but source {source_name:?} did not appear"
            )));
        }
        Ok(module_id)
    }

    fn disconnect_tunnel_source(&self, source_name: &str) -> Result<bool, BackendError> {
        let Some(module) = self.module_for_source(source_name)? else {
            return Ok(false);
        };
        self.unload_module(module.id, true)
    }

    fn send_mic_to_remote(
        &self,
        host: &str,
        port: u16,
        description: &str,
        local_source: &str,
    ) -> Result<(Option<u32>, Option<u32>, u32, u32, String), BackendError> {
        let safe_desc = sanitize_sink_name(description, 32).to_lowercase();
        let remote_sink_name = format!("soundhop_rem_mic_sink_{safe_desc}");
        let remote_source_name = format!("soundhop_rem_mic_{safe_desc}");
        let local_tunnel_name = format!("soundhop_tx_mic_{safe_desc}");
        let remote_target = tcp_server_arg(host, port);

        let res = self.pactl_call(
            &[
                "-s",
                &remote_target,
                "load-module",
                "module-null-sink",
                &format!("sink_name={remote_sink_name}"),
                &format!("sink_properties=device.description=SoundHop-Mic-Sink-{safe_desc}"),
            ],
            true,
        )?;
        let remote_null_id = res
            .stdout
            .split_whitespace()
            .find_map(|t| t.parse::<u32>().ok());

        let mut remote_remap_id: Option<u32> = None;
        let mut actual_source = format!("{remote_sink_name}.monitor");
        if let Ok(remap_res) = self.pactl_call(
            &[
                "-s",
                &remote_target,
                "load-module",
                "module-remap-source",
                &format!("source_name={remote_source_name}"),
                &format!("master={remote_sink_name}.monitor"),
            ],
            false,
        ) {
            if remap_res.ok() {
                if let Some(id) = remap_res
                    .stdout
                    .split_whitespace()
                    .find_map(|t| t.parse::<u32>().ok())
                {
                    remote_remap_id = Some(id);
                    actual_source = remote_source_name;
                }
            }
        }

        let mut tunnel_args = vec![
            format!("server={remote_target}"),
            format!("sink={remote_sink_name}"),
            format!("sink_name={local_tunnel_name}"),
            "rate=48000".to_string(),
            "channels=2".to_string(),
            "format=s16le".to_string(),
        ];
        let cookie_file = soundhop_cookie_path();
        if cookie_file.exists() {
            tunnel_args.push(format!("cookie={}", cookie_file.display()));
        }

        let local_tunnel_id = match self.load_module("module-tunnel-sink", &tunnel_args) {
            Ok(id) => id,
            Err(e) => {
                self.stop_send_mic_to_remote_internal(
                    host,
                    port,
                    remote_null_id,
                    remote_remap_id,
                    None,
                    None,
                );
                return Err(e);
            }
        };

        // Wait for the local tunnel sink to register in PipeWire/PulseAudio before
        // attaching module-loopback so loopback does not race sink creation.
        let _ = self.wait_for_sink(&local_tunnel_name, SINK_TIMEOUT_SECS);

        let local_loopback_id = match self.load_module(
            "module-loopback",
            &[
                format!("source={local_source}"),
                format!("sink={local_tunnel_name}"),
                "latency_msec=30".to_string(),
            ],
        ) {
            Ok(id) => id,
            Err(e) => {
                self.stop_send_mic_to_remote_internal(
                    host,
                    port,
                    remote_null_id,
                    remote_remap_id,
                    Some(local_tunnel_id),
                    None,
                );
                return Err(e);
            }
        };

        // Only switch the remote default source after local tunnel & loopback setup succeeds.
        if let Ok(info_res) = self.pactl_call(&["-s", &remote_target, "info"], false) {
            if info_res.ok() {
                if let Some(prev_src) = info_res.stdout.lines().find_map(|line| {
                    let (k, v) = line.split_once(": ")?;
                    (k.trim() == "Default Source").then(|| v.trim().to_string())
                }) {
                    if !prev_src.is_empty() && prev_src != actual_source {
                        self.remote_prev_default_sources
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(remote_target.clone(), prev_src);
                    }
                }
            }
        }
        let _ = self.pactl_call(
            &["-s", &remote_target, "set-default-source", &actual_source],
            false,
        );

        Ok((
            remote_null_id,
            remote_remap_id,
            local_tunnel_id,
            local_loopback_id,
            actual_source,
        ))
    }

    fn stop_send_mic_to_remote(
        &self,
        host: &str,
        port: u16,
        remote_null_id: Option<u32>,
        remote_remap_id: Option<u32>,
        local_tunnel_id: u32,
        local_loopback_id: u32,
    ) {
        self.stop_send_mic_to_remote_internal(
            host,
            port,
            remote_null_id,
            remote_remap_id,
            Some(local_tunnel_id),
            Some(local_loopback_id),
        );
    }

    fn cleanup_orphaned_remote_mic_modules(&self) -> Result<Vec<u32>, BackendError> {
        let mods = self.modules()?;
        let rem_nulls: Vec<Module> = mods
            .iter()
            .filter(|m| {
                m.name == "module-null-sink"
                    && m.arg("sink_name")
                        .map(|s| s.starts_with("soundhop_rem_mic_sink_"))
                        .unwrap_or(false)
            })
            .cloned()
            .collect();
        let mut rem_remaps: Vec<Module> = mods
            .into_iter()
            .filter(|m| {
                m.name == "module-remap-source"
                    && m.arg("source_name")
                        .map(|s| s.starts_with("soundhop_rem_mic_"))
                        .unwrap_or(false)
            })
            .collect();
        if rem_nulls.is_empty() && rem_remaps.is_empty() {
            return Ok(Vec::new());
        }

        let sinks_by_name: HashMap<String, u32> =
            self.sinks()?.into_iter().map(|s| (s.name, s.id)).collect();
        let active_sink_ids: HashSet<u32> =
            self.sink_inputs()?.into_iter().map(|i| i.sink).collect();
        let mut removed = Vec::new();
        let mut active_monitors = HashSet::new();

        for null_mod in rem_nulls {
            let sink_name = null_mod.arg("sink_name").unwrap_or_default();
            if let Some(&sid) = sinks_by_name.get(&sink_name) {
                if active_sink_ids.contains(&sid) {
                    active_monitors.insert(format!("{sink_name}.monitor"));
                    continue;
                }
            }
            let monitor_name = format!("{sink_name}.monitor");
            let mut keep_remaps = Vec::new();
            for remap_mod in rem_remaps {
                if remap_mod.arg("master").as_deref() == Some(monitor_name.as_str()) {
                    if self.unload_module(remap_mod.id, true)? {
                        removed.push(remap_mod.id);
                    }
                } else {
                    keep_remaps.push(remap_mod);
                }
            }
            rem_remaps = keep_remaps;
            if self.unload_module(null_mod.id, true)? {
                removed.push(null_mod.id);
            }
        }

        for remap_mod in rem_remaps {
            let master = remap_mod.arg("master").unwrap_or_default();
            if !active_monitors.contains(&master) && self.unload_module(remap_mod.id, true)? {
                removed.push(remap_mod.id);
            }
        }

        Ok(removed)
    }

    fn get_sink_volume(&self, sink_name: &str) -> u32 {
        if let Ok(res) = self.pactl_call(&["get-sink-volume", sink_name], false) {
            if res.ok() {
                if let Some(pct) = extract_first_percentage(&res.stdout) {
                    return pct;
                }
            }
        }
        100
    }

    fn set_sink_volume(&self, sink_name: &str, volume_percent: u32) {
        let vol = volume_percent.min(150);
        let _ = self.pactl_call(&["set-sink-volume", sink_name, &format!("{vol}%")], false);
    }

    fn get_sink_mute(&self, sink_name: &str) -> bool {
        if let Ok(res) = self.pactl_call(&["get-sink-mute", sink_name], false) {
            if res.ok() {
                return res.stdout.to_lowercase().contains("yes");
            }
        }
        false
    }

    fn set_sink_mute(&self, sink_name: &str, mute: bool) {
        let val = if mute { "1" } else { "0" };
        let _ = self.pactl_call(&["set-sink-mute", sink_name, val], false);
    }

    fn remote_set_volume(&self, host: &str, port: u16, volume_percent: u32, mute: Option<bool>) {
        let server_target = tcp_server_arg(host, port);
        let vol = volume_percent.min(150);
        let _ = self.pactl_call(
            &[
                "-s",
                &server_target,
                "set-sink-volume",
                "@DEFAULT_SINK@",
                &format!("{vol}%"),
            ],
            false,
        );
        if let Some(m) = mute {
            let val = if m { "1" } else { "0" };
            let _ = self.pactl_call(
                &["-s", &server_target, "set-sink-mute", "@DEFAULT_SINK@", val],
                false,
            );
        }
    }
}

fn effective_allowed_networks(allowed_networks: &[String]) -> Vec<String> {
    let is_default_broad = allowed_networks.len() == DEFAULT_LAN_NETWORKS.len()
        && allowed_networks
            .iter()
            .zip(DEFAULT_LAN_NETWORKS.iter())
            .all(|(a, b)| a == b);
    let mut nets: Vec<String> = if is_default_broad {
        let active = crate::util::active_lan_subnets();
        if !active.is_empty() {
            let mut scoped = vec!["127.0.0.1".to_string(), "::1".to_string()];
            for sub in active {
                if !scoped.contains(&sub) {
                    scoped.push(sub);
                }
            }
            scoped
        } else {
            allowed_networks.to_vec()
        }
    } else {
        allowed_networks.to_vec()
    };
    if !nets.iter().any(|n| n == "127.0.0.1") {
        nets.insert(0, "127.0.0.1".to_string());
    }
    nets
}

fn build_listener_args(
    port: u16,
    security: &str,
    allowed_networks: &[String],
    cookie_path: Option<&std::path::Path>,
) -> Vec<String> {
    match security {
        "psk" | "pin" | "cookie" => {
            let nets = effective_allowed_networks(allowed_networks);
            let mut args = vec![format!("port={port}")];
            if let Some(cookie_file) = cookie_path {
                args.push(format!("auth-cookie={}", cookie_file.display()));
            }
            args.push("auth-cookie-enabled=1".to_string());
            args.push(format!("auth-ip-acl={}", nets.join(";")));
            args
        }
        "open" => vec![format!("port={port}"), "auth-anonymous=1".to_string()],
        _ => {
            let nets = effective_allowed_networks(allowed_networks);
            vec![
                format!("port={port}"),
                "auth-anonymous=1".to_string(),
                format!("auth-ip-acl={}", nets.join(";")),
            ]
        }
    }
}

fn extract_first_percentage(text: &str) -> Option<u32> {
    for part in text.split('%') {
        let digits: String = part
            .chars()
            .rev()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        if !digits.is_empty() {
            if let Ok(val) = digits.parse::<u32>() {
                return Some(val);
            }
        }
    }
    None
}

fn parse_text_sink_inputs(
    text: &str,
    sinks_by_id: &HashMap<u32, String>,
    remote_sink_names: &HashSet<String>,
) -> Vec<SinkInputInfo> {
    let mut results = Vec::new();
    let mut current: Option<SinkInputInfo> = None;

    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Sink Input #") {
            if let Some(prev) = current.take() {
                results.push(prev);
            }
            if let Ok(id) = rest.trim().parse::<u32>() {
                current = Some(SinkInputInfo {
                    id,
                    sink: 0,
                    sink_name: String::new(),
                    app_name: String::new(),
                    binary: String::new(),
                    node_name: String::new(),
                    pid: 0,
                    media_name: String::new(),
                    icon_name: String::new(),
                    volume_percent: 100,
                    mute: false,
                    is_remote: false,
                });
            }
            continue;
        }
        let Some(item) = current.as_mut() else {
            continue;
        };
        if let Some(rest) = trimmed.strip_prefix("Sink:") {
            if let Ok(s_id) = rest.trim().parse::<u32>() {
                item.sink = s_id;
                item.sink_name = sinks_by_id.get(&s_id).cloned().unwrap_or_default();
                item.is_remote = remote_sink_names.contains(&item.sink_name)
                    || item.sink_name.starts_with(SINK_PREFIX);
            }
        } else if let Some(rest) = trimmed.strip_prefix("Mute:") {
            item.mute = rest.trim().eq_ignore_ascii_case("yes");
        } else if trimmed.starts_with("Volume:") {
            if let Some(pct) = extract_first_percentage(trimmed) {
                item.volume_percent = pct;
            }
        } else if let Some((k, v)) = trimmed.split_once('=') {
            let key = k.trim();
            let val = v.trim().trim_matches('"');
            match key {
                "application.name" => item.app_name = val.to_string(),
                "application.process.binary" => item.binary = val.to_string(),
                "node.name" => {
                    item.node_name = val.to_string();
                    if item.app_name.is_empty() {
                        item.app_name = val.to_string();
                    }
                    if item.binary.is_empty() {
                        item.binary = val.to_string();
                    }
                }
                "application.process.id" => item.pid = val.parse::<u32>().unwrap_or(0),
                "media.name" => item.media_name = val.to_string(),
                "application.icon_name" => item.icon_name = val.to_string(),
                _ => {}
            }
        }
    }
    if let Some(last) = current {
        results.push(last);
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::MockRunner;

    #[test]
    fn test_linux_backend_info_and_multiroom_combine() {
        let runner = Arc::new(MockRunner::new(|args, _| {
            let joined = args.join(" ");
            let stdout = if joined.contains("info") {
                "Server Name: PulseAudio (on PipeWire 1.2.0)\nServer Version: 15.0.0\nDefault Sink: alsa_output.pci\nDefault Source: alsa_input.pci\n"
            } else if joined.contains("list short sinks") {
                "1\talsa_output.pci\tPipeWire\ts16le 2ch 48000Hz\tRUNNING\n2\tsoundhop_multiroom\tmodule-combine-sink\ts16le 2ch 48000Hz\tRUNNING\n"
            } else if joined.contains("list short modules") {
                "10\tmodule-native-protocol-tcp\tport=4713 auth-anonymous=1 auth-ip-acl=127.0.0.1\n"
            } else if joined.contains("load-module") {
                "42\n"
            } else {
                ""
            };
            Ok(CommandResult {
                args: args.to_vec(),
                returncode: 0,
                stdout: stdout.to_string(),
                stderr: String::new(),
            })
        }));

        let backend = LinuxAudioBackend::with_runner(runner);
        assert!(backend.available());
        assert!(backend.is_pipewire().unwrap());
        assert_eq!(backend.default_sink().unwrap(), "alsa_output.pci");

        let mod_id = backend
            .create_combined_sink(
                "soundhop_multiroom",
                "LivingRoom+Kitchen",
                &[
                    "soundhop_living_111111".to_string(),
                    "soundhop_kitchen_222222".to_string(),
                ],
            )
            .unwrap();
        assert_eq!(mod_id, 42);
    }

    #[test]
    fn test_linux_backend_filters_unsafe_extra_args_and_builds_cookie_acl() {
        let cookie = std::path::Path::new("/tmp/test-soundhop-cookie");
        let args = build_listener_args(4713, "psk", &["192.168.1.0/24".to_string()], Some(cookie));
        assert!(args.contains(&"auth-cookie=/tmp/test-soundhop-cookie".to_string()));
        assert!(args.contains(&"auth-cookie-enabled=1".to_string()));
        assert!(args.contains(&"auth-ip-acl=127.0.0.1;192.168.1.0/24".to_string()));
        assert!(!args.contains(&"auth-anonymous=1".to_string()));

        let recorded = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let rec_c = Arc::clone(&recorded);
        let loaded = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let loaded_c = Arc::clone(&loaded);
        let runner = Arc::new(MockRunner::new(move |call_args, _| {
            rec_c.lock().unwrap().push(call_args.to_vec());
            let joined = call_args.join(" ");
            let stdout = if joined.contains("list short sinks") {
                if loaded_c.load(std::sync::atomic::Ordering::SeqCst) {
                    "1\talsa_output.pci\tPipeWire\ts16le 2ch 48000Hz\tRUNNING\n2\tsoundhop_peer_1\tmodule-tunnel-sink\ts16le 2ch 48000Hz\tRUNNING\n"
                } else {
                    "1\talsa_output.pci\tPipeWire\ts16le 2ch 48000Hz\tRUNNING\n"
                }
            } else if joined.contains("load-module") {
                loaded_c.store(true, std::sync::atomic::Ordering::SeqCst);
                "77\n"
            } else {
                ""
            };
            Ok(CommandResult {
                args: call_args.to_vec(),
                returncode: 0,
                stdout: stdout.to_string(),
                stderr: String::new(),
            })
        }));
        let backend = LinuxAudioBackend::with_runner(runner);
        let extra = vec![
            "cookie=/etc/shadow".to_string(),
            "server=tcp:evil:4713".to_string(),
            "latency_msec=90".to_string(),
        ];
        let mid = backend
            .connect_tunnel(&TunnelParams {
                host: "192.168.1.50",
                port: 4713,
                sink_name: "soundhop_peer_1",
                description: "Peer1",
                reconnect_delay_ms: 3000,
                latency_ms: None,
                extra_args: &extra,
                cookie_path: None,
            })
            .unwrap();
        assert_eq!(mid, 77);
        let calls = recorded.lock().unwrap();
        let load_call = calls
            .iter()
            .find(|c| c.iter().any(|a| a == "load-module"))
            .unwrap();
        assert!(!load_call.iter().any(|a| a == "cookie=/etc/shadow"));
        assert!(!load_call.iter().any(|a| a == "server=tcp:evil:4713"));
        assert!(load_call.iter().any(|a| a == "latency_msec=90"));
    }

    #[test]
    fn test_ensure_tcp_listener_replaces_mismatched_security_and_mic_id_zero() {
        let unloaded = Arc::new(Mutex::new(Vec::<String>::new()));
        let unloaded_c = Arc::clone(&unloaded);
        let calls_log = Arc::new(Mutex::new(Vec::<String>::new()));
        let calls_log_c = Arc::clone(&calls_log);
        let runner = Arc::new(MockRunner::new(move |args, _| {
            let joined = args.join(" ");
            calls_log_c.lock().unwrap().push(joined.clone());
            if joined.contains("unload-module") {
                unloaded_c.lock().unwrap().push(joined.clone());
            }
            let stdout = if joined.contains("list short modules") {
                "10\tmodule-native-protocol-tcp\tport=4713 auth-anonymous=1\n"
            } else if joined.contains("list short sinks") {
                "1\tsoundhop_tx_mic_desk\tmodule-tunnel-sink\ts16le 2ch 48000Hz\tRUNNING\n"
            } else if joined.contains("info") {
                "Default Source: alsa_input.remote_orig\n"
            } else if joined.contains("load-module") {
                "0\n"
            } else {
                ""
            };
            Ok(CommandResult {
                args: args.to_vec(),
                returncode: 0,
                stdout: stdout.to_string(),
                stderr: String::new(),
            })
        }));

        let backend = LinuxAudioBackend::with_runner(runner);
        let listener = backend
            .ensure_tcp_listener(4713, "psk", &["192.168.1.0/24".to_string()])
            .unwrap();
        assert!(listener.created);
        assert_eq!(listener.security, "cookie");
        assert!(unloaded
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.contains("unload-module 10")));

        // Verify module ID 0 is unloaded for all 4 modules (remote + local) and remote default source is restored
        unloaded.lock().unwrap().clear();
        calls_log.lock().unwrap().clear();
        let (r_null, r_remap, l_tun, l_loop, _) = backend
            .send_mic_to_remote("192.168.1.50", 4713, "Desk", "@DEFAULT_SOURCE@")
            .unwrap();
        assert_eq!(r_null, Some(0));
        assert_eq!(r_remap, Some(0));
        assert_eq!(l_tun, 0);
        assert_eq!(l_loop, 0);

        // Verify set-default-source happened AFTER module-loopback was loaded
        {
            let log = calls_log.lock().unwrap();
            let loopback_pos = log
                .iter()
                .position(|c| c.contains("load-module module-loopback"))
                .unwrap();
            let set_def_pos = log
                .iter()
                .position(|c| c.contains("set-default-source soundhop_rem_mic_desk"))
                .unwrap();
            assert!(set_def_pos > loopback_pos);
        }

        backend.stop_send_mic_to_remote("192.168.1.50", 4713, r_null, r_remap, l_tun, l_loop);
        let calls = unloaded.lock().unwrap();
        assert_eq!(
            calls
                .iter()
                .filter(|c| c.contains("unload-module 0"))
                .count(),
            4
        );
        assert!(calls_log
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.contains("set-default-source alsa_input.remote_orig")));
    }

    #[test]
    fn test_pactl_call_sets_c_locale_env() {
        let seen_envs = Arc::new(Mutex::new(Vec::<HashMap<String, String>>::new()));
        let seen_c = Arc::clone(&seen_envs);
        let runner = Arc::new(MockRunner::new(move |args, env| {
            if let Some(e) = env {
                seen_c.lock().unwrap().push(e.clone());
            }
            Ok(CommandResult {
                args: args.to_vec(),
                returncode: 0,
                stdout: "Server Name: PulseAudio\n".to_string(),
                stderr: String::new(),
            })
        }));

        let backend = LinuxAudioBackend::with_runner(runner);
        let _ = backend.server_name().unwrap();
        let envs = seen_envs.lock().unwrap();
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0].get("LC_ALL").map(String::as_str), Some("C"));
        assert_eq!(envs[0].get("LANG").map(String::as_str), Some("C"));
    }

    #[test]
    fn test_ensure_tcp_listener_checks_cookie_path_and_consistent_security() {
        let cookie_path = soundhop_cookie_path().to_string_lossy().into_owned();
        let unloaded = Arc::new(Mutex::new(Vec::<String>::new()));
        let unloaded_c = Arc::clone(&unloaded);
        let wrong_cookie = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let wrong_c = Arc::clone(&wrong_cookie);
        let cookie_for_closure = cookie_path.clone();

        let runner = Arc::new(MockRunner::new(move |args, _| {
            let joined = args.join(" ");
            if joined.contains("unload-module") {
                unloaded_c.lock().unwrap().push(joined.clone());
                wrong_c.store(false, std::sync::atomic::Ordering::SeqCst);
            }
            let stdout = if joined.contains("list short modules") {
                if wrong_c.load(std::sync::atomic::Ordering::SeqCst) {
                    "10\tmodule-native-protocol-tcp\tport=4713 auth-cookie=/wrong/path auth-cookie-enabled=1 auth-ip-acl=127.0.0.1;192.168.1.0/24\n".to_string()
                } else {
                    format!("11\tmodule-native-protocol-tcp\tport=4713 auth-cookie={cookie_for_closure} auth-cookie-enabled=1 auth-ip-acl=127.0.0.1;192.168.1.0/24\n")
                }
            } else if joined.contains("load-module") {
                "11\n".to_string()
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

        let backend = LinuxAudioBackend::with_runner(runner);
        // Wrong auth-cookie path must replace module 10
        let first = backend
            .ensure_tcp_listener(4713, "psk", &["192.168.1.0/24".to_string()])
            .unwrap();
        assert!(first.created);
        assert_eq!(first.security, "cookie");
        assert!(unloaded
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.contains("unload-module 10")));

        // Matching auth-cookie path must reuse module 11 with identical Listener.security
        let second = backend
            .ensure_tcp_listener(4713, "psk", &["192.168.1.0/24".to_string()])
            .unwrap();
        assert!(!second.created);
        assert_eq!(second.module_id, 11);
        assert_eq!(second.security, first.security);
    }

    #[test]
    fn test_send_mic_to_remote_rolls_back_without_setting_remote_default_on_loopback_failure() {
        let calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let calls_c = Arc::clone(&calls);
        let runner = Arc::new(MockRunner::new(move |args, _| {
            let joined = args.join(" ");
            calls_c.lock().unwrap().push(joined.clone());
            if joined.contains("load-module module-loopback") {
                return Ok(CommandResult {
                    args: args.to_vec(),
                    returncode: 1,
                    stdout: String::new(),
                    stderr: "loopback failed".to_string(),
                });
            }
            let stdout = if joined.contains("list short sinks") {
                "1\tsoundhop_tx_mic_desk\tmodule-tunnel-sink\ts16le 2ch 48000Hz\tRUNNING\n"
            } else if joined.contains("load-module") {
                "0\n"
            } else {
                ""
            };
            Ok(CommandResult {
                args: args.to_vec(),
                returncode: 0,
                stdout: stdout.to_string(),
                stderr: String::new(),
            })
        }));

        let backend = LinuxAudioBackend::with_runner(runner);
        let err = backend
            .send_mic_to_remote("192.168.1.50", 4713, "Desk", "@DEFAULT_SOURCE@")
            .unwrap_err();
        assert!(err.to_string().contains("loopback failed"));
        let log = calls.lock().unwrap();
        // Remote default source was never changed
        assert!(!log.iter().any(|c| c.contains("set-default-source")));
        // Local tunnel (id 0) + remote remap (id 0) + remote null (id 0) were all unloaded
        assert_eq!(
            log.iter().filter(|c| c.contains("unload-module 0")).count(),
            3
        );
    }

    #[test]
    fn test_ensure_tcp_listener_reloads_on_cookie_content_change() {
        let dir = tempfile::tempdir().unwrap();
        let cookie_file = dir.path().join("pulse-cookie");
        let cookie_str = cookie_file.to_string_lossy().into_owned();
        let cookie_for_closure = cookie_str.clone();

        // Write initial cookie A
        crate::security::write_pa_cookie(
            Some(&cookie_file),
            &crate::security::derive_pa_cookie("pin-alpha-1111"),
        )
        .unwrap();

        let next_id = Arc::new(std::sync::atomic::AtomicU32::new(20));
        let next_id_c = Arc::clone(&next_id);
        let active_id = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let active_id_c = Arc::clone(&active_id);
        let unloaded = Arc::new(Mutex::new(Vec::<String>::new()));
        let unloaded_c = Arc::clone(&unloaded);

        let runner = Arc::new(MockRunner::new(move |args, _| {
            let joined = args.join(" ");
            let stdout = if joined.contains("unload-module") {
                unloaded_c.lock().unwrap().push(joined.clone());
                active_id_c.store(0, std::sync::atomic::Ordering::SeqCst);
                String::new()
            } else if joined.contains("list short modules") {
                let id = active_id_c.load(std::sync::atomic::Ordering::SeqCst);
                if id > 0 {
                    format!(
                        "{id}\tmodule-native-protocol-tcp\tport=4713 auth-cookie={cookie_for_closure} auth-cookie-enabled=1 auth-ip-acl=127.0.0.1;192.168.1.0/24\n"
                    )
                } else {
                    String::new()
                }
            } else if joined.contains("load-module") {
                let id = next_id_c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                active_id_c.store(id, std::sync::atomic::Ordering::SeqCst);
                format!("{id}\n")
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

        let backend = LinuxAudioBackend::with_runner_and_cookie_path(runner, cookie_file.clone());
        let nets = vec!["192.168.1.0/24".to_string()];

        // First call loads module 20
        let first = backend.ensure_tcp_listener(4713, "psk", &nets).unwrap();
        assert!(first.created);
        assert_eq!(first.module_id, 20);

        // Second call with identical cookie bytes reuses module 20
        let second = backend.ensure_tcp_listener(4713, "psk", &nets).unwrap();
        assert!(!second.created);
        assert_eq!(second.module_id, 20);
        assert!(unloaded.lock().unwrap().is_empty());

        // Overwrite cookie file with different bytes (same path!)
        crate::security::write_pa_cookie(
            Some(&cookie_file),
            &crate::security::derive_pa_cookie("pin-beta-2222"),
        )
        .unwrap();

        // Must unload module 20 and load fresh module 21
        let third = backend.ensure_tcp_listener(4713, "psk", &nets).unwrap();
        assert!(third.created);
        assert_eq!(third.module_id, 21);
        assert!(unloaded
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.contains("unload-module 20")));
    }

    #[test]
    fn test_connect_tunnel_uses_explicit_cookie_path_only() {
        let dir = tempfile::tempdir().unwrap();
        let cookie_file = dir.path().join("pulse-cookie");
        crate::security::write_pa_cookie(
            Some(&cookie_file),
            &crate::security::derive_pa_cookie("pin-tunnel-test"),
        )
        .unwrap();

        let recorded = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let rec_c = Arc::clone(&recorded);
        let loaded = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let loaded_c = Arc::clone(&loaded);
        let runner = Arc::new(MockRunner::new(move |call_args, _| {
            rec_c.lock().unwrap().push(call_args.to_vec());
            let joined = call_args.join(" ");
            let stdout = if joined.contains("list short sinks") {
                if loaded_c.load(std::sync::atomic::Ordering::SeqCst) {
                    "1\tsoundhop_peer_a\tmodule-tunnel-sink\ts16le 2ch 48000Hz\tRUNNING\n"
                } else {
                    ""
                }
            } else if joined.contains("unload-module") {
                loaded_c.store(false, std::sync::atomic::Ordering::SeqCst);
                ""
            } else if joined.contains("load-module") {
                loaded_c.store(true, std::sync::atomic::Ordering::SeqCst);
                "88\n"
            } else {
                ""
            };
            Ok(CommandResult {
                args: call_args.to_vec(),
                returncode: 0,
                stdout: stdout.to_string(),
                stderr: String::new(),
            })
        }));

        let backend = LinuxAudioBackend::with_runner_and_cookie_path(runner, cookie_file.clone());

        // Without cookie_path (trusted-lan): must NOT pass cookie= even though cookie_file exists on disk
        backend
            .connect_tunnel(&TunnelParams {
                host: "192.168.1.50",
                port: 4713,
                sink_name: "soundhop_peer_a",
                description: "PeerA",
                reconnect_delay_ms: 1000,
                latency_ms: None,
                extra_args: &[],
                cookie_path: None,
            })
            .unwrap();
        {
            let calls = recorded.lock().unwrap();
            let load_call = calls
                .iter()
                .find(|c| c.iter().any(|a| a == "load-module"))
                .unwrap();
            assert!(!load_call.iter().any(|a| a.starts_with("cookie=")));
        }

        // With explicit cookie_path (psk/paired): must pass cookie=<path>
        loaded.store(false, std::sync::atomic::Ordering::SeqCst);
        recorded.lock().unwrap().clear();
        backend
            .connect_tunnel(&TunnelParams {
                host: "192.168.1.50",
                port: 4713,
                sink_name: "soundhop_peer_a",
                description: "PeerA",
                reconnect_delay_ms: 1000,
                latency_ms: None,
                extra_args: &[],
                cookie_path: Some(cookie_file.clone()),
            })
            .unwrap();
        {
            let calls = recorded.lock().unwrap();
            let load_call = calls
                .iter()
                .find(|c| c.iter().any(|a| a == "load-module"))
                .unwrap();
            let expected = format!("cookie={}", cookie_file.display());
            assert!(load_call.iter().any(|a| a == &expected));
        }
    }
}
