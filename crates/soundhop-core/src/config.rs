//! Typed TOML configuration with v2 persistent routing rules and security modes.

use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use ipnet::IpNet;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::constants::{
    DEFAULT_AUDIO_BIT_RATE, DEFAULT_AUDIO_BUFFER_MS, DEFAULT_AUDIO_CODEC, DEFAULT_AUDIO_PORT,
    DEFAULT_DISCOVERY_PORT, DEFAULT_LAN_NETWORKS, SECURITY_MODES,
};
use crate::util::{atomic_write, config_path, hostname_name, new_device_id};

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("{0}")]
    Invalid(String),
    #[error("Cannot read {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("Cannot parse TOML in {path}: {source}")]
    TomlParse {
        path: PathBuf,
        source: toml::de::Error,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DeviceConfig {
    pub name: String,
    pub id: String,
}

impl Default for DeviceConfig {
    fn default() -> Self {
        Self {
            name: hostname_name(),
            id: new_device_id(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SharingConfig {
    pub enabled: bool,
    pub port: u16,
    pub security: String,
    pub allowed_networks: Vec<String>,
    /// Optional SHA-256 / HMAC pre-shared key hash for `psk` / `pin` security mode.
    pub psk_hash: String,
}

impl Default for SharingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            port: DEFAULT_AUDIO_PORT,
            security: "trusted-lan".to_string(),
            allowed_networks: DEFAULT_LAN_NETWORKS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            psk_hash: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ConnectionConfig {
    #[serde(alias = "auto_reconnect")]
    pub auto_connect: bool,
    pub switch_default: bool,
    pub restore_default: bool,
    pub move_streams: bool,
    pub disconnect_on_quit: bool,
    pub keep_local_output: bool,
    pub last_peer: String,
    pub reconnect_delay_ms: u64,
    #[serde(alias = "buffer_latency_ms")]
    pub latency_ms: Option<u32>,
    pub extra_tunnel_args: Vec<String>,
    pub disconnect_grace_s: f64,
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            auto_connect: true,
            switch_default: true,
            restore_default: true,
            move_streams: true,
            disconnect_on_quit: true,
            keep_local_output: false,
            last_peer: String::new(),
            reconnect_delay_ms: 3000,
            latency_ms: None,
            extra_tunnel_args: Vec::new(),
            disconnect_grace_s: 12.0,
        }
    }
}

/// Persistent per-application stream routing rules (v2 feature).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RoutingConfig {
    /// Default routing mode on connect: `"all"` or `"selective"`.
    pub default_mode: String,
    /// App names or binary names that should always be routed to remote speakers when connected.
    pub always_remote: Vec<String>,
    /// App names or binary names that should always stay on local speakers when connected.
    pub always_local: Vec<String>,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            default_mode: "all".to_string(),
            always_remote: Vec::new(),
            always_local: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DiscoveryConfig {
    pub udp: bool,
    pub mdns: bool,
    pub port: u16,
    pub interval_s: f64,
    pub timeout_s: f64,
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            udp: true,
            mdns: true,
            port: DEFAULT_DISCOVERY_PORT,
            interval_s: 3.0,
            timeout_s: 20.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct UiConfig {
    pub notifications: bool,
    pub start_tray: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            notifications: true,
            start_tray: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AndroidConfig {
    pub scrcpy_path: String,
    pub adb_path: String,
    pub default_codec: String,
    pub default_source: String,
    pub buffer_ms: u32,
    pub bit_rate: u32,
    pub duplicate_audio: bool,
    pub auto_switch_wifi: bool,
    pub auto_reconnect: bool,
    pub mdns_discovery: bool,
    pub last_serial: String,
}

impl Default for AndroidConfig {
    fn default() -> Self {
        Self {
            scrcpy_path: String::new(),
            adb_path: String::new(),
            default_codec: DEFAULT_AUDIO_CODEC.to_string(),
            default_source: "output".to_string(),
            buffer_ms: DEFAULT_AUDIO_BUFFER_MS,
            bit_rate: DEFAULT_AUDIO_BIT_RATE,
            duplicate_audio: false,
            auto_switch_wifi: true,
            auto_reconnect: true,
            mdns_discovery: true,
            last_serial: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    #[serde(skip)]
    pub path: PathBuf,
    pub device: DeviceConfig,
    pub sharing: SharingConfig,
    pub connection: ConnectionConfig,
    pub routing: RoutingConfig,
    pub discovery: DiscoveryConfig,
    pub ui: UiConfig,
    pub android: AndroidConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            path: config_path(),
            device: DeviceConfig::default(),
            sharing: SharingConfig::default(),
            connection: ConnectionConfig::default(),
            routing: RoutingConfig::default(),
            discovery: DiscoveryConfig::default(),
            ui: UiConfig::default(),
            android: AndroidConfig::default(),
        }
    }
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self, ConfigError> {
        let target = path.map(Path::to_path_buf).unwrap_or_else(config_path);
        if !target.exists() {
            let mut cfg = Self {
                path: target,
                ..Self::default()
            };
            cfg.normalize()?;
            let _ = cfg.save(None);
            return Ok(cfg);
        }
        let raw_text = fs::read_to_string(&target).map_err(|source| ConfigError::Io {
            path: target.clone(),
            source,
        })?;
        let mut cfg: Self = toml::from_str(&raw_text).map_err(|source| ConfigError::TomlParse {
            path: target.clone(),
            source,
        })?;
        cfg.path = target;
        cfg.normalize()?;
        Ok(cfg)
    }

    pub fn normalize(&mut self) -> Result<(), ConfigError> {
        let trimmed_name = self.device.name.trim();
        self.device.name = if trimmed_name.is_empty() {
            hostname_name()
        } else {
            trimmed_name.to_string()
        };
        if self.device.id.trim().is_empty() {
            self.device.id = new_device_id();
        } else {
            self.device.id = self.device.id.trim().to_string();
        }

        if self.sharing.port == 0 {
            self.sharing.port = DEFAULT_AUDIO_PORT;
        }
        self.sharing.security = normalize_security(&self.sharing.security)?;
        self.sharing.psk_hash = self.sharing.psk_hash.trim().to_string();

        let mut valid_nets = Vec::new();
        let source_nets: Vec<String> = if self.sharing.allowed_networks.is_empty() {
            DEFAULT_LAN_NETWORKS
                .iter()
                .map(|s| (*s).to_string())
                .collect()
        } else {
            self.sharing.allowed_networks.clone()
        };
        for net_str in source_nets {
            let trimmed = net_str.trim();
            if trimmed.is_empty() {
                continue;
            }
            let valid = trimmed.parse::<IpNet>().is_ok() || trimmed.parse::<IpAddr>().is_ok();
            if !valid {
                return Err(ConfigError::Invalid(format!(
                    "Invalid network in sharing.allowed_networks: {trimmed}"
                )));
            }
            if !valid_nets.contains(&trimmed.to_string()) {
                valid_nets.push(trimmed.to_string());
            }
        }
        self.sharing.allowed_networks = valid_nets;

        if self.discovery.port == 0 {
            self.discovery.port = DEFAULT_DISCOVERY_PORT;
        }
        self.discovery.interval_s = self.discovery.interval_s.max(1.0);
        self.discovery.timeout_s = self.discovery.timeout_s.max(5.0);

        if let Some(0) = self.connection.latency_ms {
            self.connection.latency_ms = None;
        }
        self.connection.disconnect_grace_s = self.connection.disconnect_grace_s.max(3.0);

        let mode = self.routing.default_mode.trim().to_lowercase();
        self.routing.default_mode = if mode == "selective" {
            "selective".to_string()
        } else {
            "all".to_string()
        };
        self.routing.always_remote = clean_app_list(&self.routing.always_remote);
        self.routing.always_local = clean_app_list(&self.routing.always_local);

        self.android.scrcpy_path = self.android.scrcpy_path.trim().to_string();
        self.android.adb_path = self.android.adb_path.trim().to_string();
        let codec = self.android.default_codec.trim().to_lowercase();
        self.android.default_codec = if codec.is_empty() {
            DEFAULT_AUDIO_CODEC.to_string()
        } else {
            codec
        };
        let src = self.android.default_source.trim().to_lowercase();
        self.android.default_source = if src.is_empty() {
            "output".to_string()
        } else {
            src
        };
        self.android.buffer_ms = self.android.buffer_ms.max(10);
        self.android.bit_rate = self.android.bit_rate.max(16_000);
        self.android.last_serial = self.android.last_serial.trim().to_string();
        Ok(())
    }

    pub fn save(&mut self, path: Option<&Path>) -> Result<PathBuf, ConfigError> {
        let target = path
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.path.clone());
        let text = self.to_toml_string();
        atomic_write(&target, &text).map_err(|source| ConfigError::Io {
            path: target.clone(),
            source,
        })?;
        self.path = target.clone();
        Ok(target)
    }

    pub fn to_toml_string(&self) -> String {
        let body = toml::to_string_pretty(self).unwrap_or_default();
        format!(
            "# SoundHop v2.0 configuration\n# See 'soundhop --help' or the README for descriptions.\n\n{body}"
        )
    }

    pub fn to_json_value(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_else(|_| serde_json::json!({}))
    }

    pub fn get_dotted(&self, dotted: &str) -> Result<serde_json::Value, ConfigError> {
        let normalized_key = resolve_config_alias(dotted);
        let root = self.to_json_value();
        let mut current = &root;
        for part in normalized_key.split('.') {
            match current.get(part) {
                Some(v) => current = v,
                None => {
                    return Err(ConfigError::Invalid(format!(
                        "Unknown configuration key: {dotted}"
                    )));
                }
            }
        }
        Ok(current.clone())
    }

    pub fn set_dotted(&mut self, dotted: &str, raw_value: &str) -> Result<(), ConfigError> {
        let normalized_key = resolve_config_alias(dotted);
        let parts: Vec<&str> = normalized_key.split('.').collect();
        if parts.len() != 2 {
            return Err(ConfigError::Invalid(
                "Use section.key, for example connection.auto_connect".to_string(),
            ));
        }
        let (section, key) = (parts[0], parts[1]);
        if section == "device" && key == "id" {
            return Err(ConfigError::Invalid(
                "device.id is managed automatically".to_string(),
            ));
        }
        let mut root = self.to_json_value();
        let Some(sec_obj) = root.get_mut(section).and_then(|v| v.as_object_mut()) else {
            return Err(ConfigError::Invalid(format!(
                "Unknown configuration key: {dotted}"
            )));
        };
        if !sec_obj.contains_key(key) {
            return Err(ConfigError::Invalid(format!(
                "Unknown configuration key: {dotted}"
            )));
        }
        let parsed_val = parse_config_value(raw_value);
        sec_obj.insert(key.to_string(), parsed_val);
        let path_backup = self.path.clone();
        let mut updated: Config = serde_json::from_value(root)
            .map_err(|e| ConfigError::Invalid(format!("Invalid value for {dotted}: {e}")))?;
        updated.path = path_backup;
        updated.normalize()?;
        *self = updated;
        Ok(())
    }

    pub fn apply_patch(&mut self, patch: &serde_json::Value) -> Result<(), ConfigError> {
        let Some(patch_obj) = patch.as_object() else {
            return Ok(());
        };
        let mut root = self.to_json_value();
        if let Some(root_obj) = root.as_object_mut() {
            for (sec_name, sec_vals) in patch_obj {
                if let (Some(target_sec), Some(src_sec)) = (
                    root_obj.get_mut(sec_name).and_then(|v| v.as_object_mut()),
                    sec_vals.as_object(),
                ) {
                    for (k, v) in src_sec {
                        if sec_name == "device" && k == "id" {
                            continue;
                        }
                        if target_sec.contains_key(k) {
                            target_sec.insert(k.clone(), v.clone());
                        }
                    }
                }
            }
        }
        let path_backup = self.path.clone();
        let mut updated: Config = serde_json::from_value(root)
            .map_err(|e| ConfigError::Invalid(format!("Invalid config patch: {e}")))?;
        updated.path = path_backup;
        updated.normalize()?;
        *self = updated;
        Ok(())
    }
}

fn clean_app_list(list: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for item in list {
        let t = item.trim().to_lowercase();
        if !t.is_empty() && !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

fn resolve_config_alias(dotted: &str) -> &str {
    match dotted {
        "connection.auto_reconnect" => "connection.auto_connect",
        "connection.buffer_latency_ms" | "sharing.buffer_latency_ms" => "connection.latency_ms",
        other => other,
    }
}

pub fn normalize_security(raw: &str) -> Result<String, ConfigError> {
    let lower = raw.trim().to_lowercase();
    let mapped = match lower.as_str() {
        "lan" | "trusted" | "private" => "trusted-lan",
        "anonymous" | "none" => "open",
        other => other,
    };
    if SECURITY_MODES.contains(&mapped) {
        Ok(mapped.to_string())
    } else {
        Err(ConfigError::Invalid(format!(
            "sharing.security must be one of {}",
            SECURITY_MODES.join(", ")
        )))
    }
}

pub fn parse_config_value(raw: &str) -> serde_json::Value {
    let text = raw.trim();
    let lower = text.to_lowercase();
    if matches!(lower.as_str(), "true" | "yes" | "on") {
        return serde_json::Value::Bool(true);
    }
    if matches!(lower.as_str(), "false" | "no" | "off") {
        return serde_json::Value::Bool(false);
    }
    if matches!(lower.as_str(), "none" | "null") {
        return serde_json::Value::Null;
    }
    if text.starts_with('[') && text.ends_with(']') {
        let inner = &text[1..text.len() - 1];
        let items: Vec<serde_json::Value> = inner
            .split(',')
            .map(|s| s.trim().trim_matches('"').trim_matches('\''))
            .filter(|s| !s.is_empty())
            .map(|s| serde_json::Value::String(s.to_string()))
            .collect();
        return serde_json::Value::Array(items);
    }
    if let Ok(i) = text.parse::<i64>() {
        return serde_json::Value::Number(i.into());
    }
    if let Ok(f) = text.parse::<f64>() {
        if let Some(num) = serde_json::Number::from_f64(f) {
            return serde_json::Value::Number(num);
        }
    }
    let unquoted = text.trim_matches('"').trim_matches('\'');
    serde_json::Value::String(unquoted.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_load_save_and_dotted() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_file = dir.path().join("config.toml");
        let mut cfg = Config::load(Some(&cfg_file)).unwrap();
        assert!(cfg_file.exists());
        assert_eq!(cfg.sharing.security, "trusted-lan");

        cfg.set_dotted("sharing.security", "psk").unwrap();
        cfg.set_dotted("routing.always_remote", "[\"spotify\", \"vlc\"]")
            .unwrap();
        cfg.set_dotted("connection.keep_local_output", "true")
            .unwrap();
        cfg.save(None).unwrap();

        let reloaded = Config::load(Some(&cfg_file)).unwrap();
        assert_eq!(reloaded.sharing.security, "psk");
        assert_eq!(
            reloaded.routing.always_remote,
            vec!["spotify".to_string(), "vlc".to_string()]
        );
        assert!(reloaded.connection.keep_local_output);
        assert_eq!(
            reloaded.get_dotted("sharing.security").unwrap(),
            serde_json::Value::String("psk".to_string())
        );
    }

    #[test]
    fn test_invalid_network_rejected() {
        let mut cfg = Config::default();
        cfg.sharing.allowed_networks = vec!["not-an-ip".to_string()];
        assert!(cfg.normalize().is_err());
    }
}
