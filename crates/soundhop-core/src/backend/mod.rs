//! AudioBackend trait and shared graph models (`Sink`, `Source`, `Module`, `SinkInputInfo`).

pub mod linux;
pub mod windows;

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::constants::SINK_PREFIX;

#[derive(Debug, Error)]
pub enum BackendError {
    #[error("{0}")]
    Message(String),
    #[error("Audio tool error: {0}")]
    Util(#[from] crate::util::UtilError),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Module {
    pub id: u32,
    pub name: String,
    pub args: String,
}

impl Module {
    pub fn arg(&self, key: &str) -> Option<String> {
        let needle = format!("{key}=");
        for token in self.args.split_whitespace() {
            if let Some(val) = token.strip_prefix(&needle) {
                return Some(val.trim_matches('"').trim_matches('\'').to_string());
            }
        }
        None
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Sink {
    pub id: u32,
    pub name: String,
    pub driver: String,
    pub spec: String,
    pub state: String,
}

impl Sink {
    pub fn is_soundhop(&self) -> bool {
        self.name.starts_with(SINK_PREFIX)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Source {
    pub id: u32,
    pub name: String,
    pub driver: String,
    pub spec: String,
    pub state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SinkInput {
    pub id: u32,
    pub sink: u32,
    pub client: u32,
    pub driver: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SinkInputInfo {
    pub id: u32,
    pub sink: u32,
    pub sink_name: String,
    pub app_name: String,
    pub binary: String,
    pub node_name: String,
    pub pid: u32,
    pub media_name: String,
    pub icon_name: String,
    pub volume_percent: u32,
    pub mute: bool,
    pub is_remote: bool,
}

impl SinkInputInfo {
    pub fn display_name(&self) -> String {
        for candidate in [
            &self.app_name,
            &self.binary,
            &self.node_name,
            &self.media_name,
        ] {
            if !candidate.trim().is_empty() {
                return candidate.trim().to_string();
            }
        }
        format!("Stream #{}", self.id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceOutput {
    pub id: u32,
    pub source: u32,
    pub client: u32,
    pub driver: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Listener {
    pub module_id: u32,
    pub created: bool,
    pub args: String,
    pub port: u16,
    pub security: String,
}

#[derive(Debug, Clone, Default)]
pub struct TunnelParams<'a> {
    pub host: &'a str,
    pub port: u16,
    pub sink_name: &'a str,
    pub description: &'a str,
    pub reconnect_delay_ms: u64,
    pub latency_ms: Option<u32>,
    pub extra_args: &'a [String],
}

#[derive(Debug, Clone, Default)]
pub struct SourceTunnelParams<'a> {
    pub host: &'a str,
    pub port: u16,
    pub source_name: &'a str,
    pub description: &'a str,
    pub remote_source: &'a str,
    pub reconnect_delay_ms: u64,
    pub latency_ms: Option<u32>,
    pub extra_args: &'a [String],
}

pub trait AudioBackend: Send + Sync {
    fn available(&self) -> bool;
    fn invalidate(&self);
    fn server_name(&self) -> Result<String, BackendError>;
    fn server_version(&self) -> Result<String, BackendError>;
    fn server_label(&self) -> Result<String, BackendError> {
        let name = self.server_name()?;
        let ver = self.server_version()?;
        Ok(format!("{name} {ver}").trim().to_string())
    }
    fn is_pipewire(&self) -> Result<bool, BackendError>;
    fn default_sink(&self) -> Result<String, BackendError>;
    fn default_source(&self) -> Result<String, BackendError>;

    fn modules(&self) -> Result<Vec<Module>, BackendError>;
    fn module(&self, module_id: u32) -> Result<Option<Module>, BackendError> {
        Ok(self.modules()?.into_iter().find(|m| m.id == module_id))
    }
    fn sinks(&self) -> Result<Vec<Sink>, BackendError>;
    fn sink(&self, name: &str) -> Result<Option<Sink>, BackendError> {
        Ok(self.sinks()?.into_iter().find(|s| s.name == name))
    }
    fn sink_exists(&self, name: &str) -> Result<bool, BackendError> {
        Ok(self.sink(name)?.is_some())
    }
    fn sources(&self) -> Result<Vec<Source>, BackendError>;
    fn source(&self, name: &str) -> Result<Option<Source>, BackendError> {
        Ok(self.sources()?.into_iter().find(|s| s.name == name))
    }
    fn source_exists(&self, name: &str) -> Result<bool, BackendError> {
        Ok(self.source(name)?.is_some())
    }
    fn sink_inputs(&self) -> Result<Vec<SinkInput>, BackendError>;
    fn detailed_sink_inputs(&self) -> Result<Vec<SinkInputInfo>, BackendError>;
    fn source_outputs(&self) -> Result<Vec<SourceOutput>, BackendError>;

    fn load_module(&self, name: &str, args: &[String]) -> Result<u32, BackendError>;
    fn unload_module(&self, module_id: u32, ignore_missing: bool) -> Result<bool, BackendError>;
    fn unload_tunnels(&self, sink_prefix: &str) -> Result<Vec<u32>, BackendError>;
    fn soundhop_modules(&self) -> Result<Vec<Module>, BackendError>;

    fn set_default_sink(&self, name: &str) -> Result<(), BackendError>;
    fn set_default_source(&self, name: &str) -> Result<(), BackendError>;
    fn move_sink_inputs(&self, target: &str, source: Option<&str>) -> Result<usize, BackendError>;
    fn move_source_outputs(
        &self,
        target: &str,
        source: Option<&str>,
        exclude_modules: &[u32],
    ) -> Result<usize, BackendError>;
    fn move_app_streams(&self, identifier: &str, target: &str) -> Result<usize, BackendError>;

    fn ensure_tcp_listener(
        &self,
        port: u16,
        security: &str,
        allowed_networks: &[String],
    ) -> Result<Listener, BackendError>;
    fn remove_listener(&self, module_id: u32) -> Result<bool, BackendError> {
        self.unload_module(module_id, true)
    }

    fn connect_tunnel(&self, params: &TunnelParams<'_>) -> Result<u32, BackendError>;
    fn disconnect_tunnel(&self, sink_name: &str) -> Result<bool, BackendError>;

    /// Create a multi-room combined sink (`module-combine-sink`) broadcasting to `slave_sinks`.
    fn create_combined_sink(
        &self,
        sink_name: &str,
        description: &str,
        slave_sinks: &[String],
    ) -> Result<u32, BackendError>;

    fn connect_tunnel_source(&self, params: &SourceTunnelParams<'_>) -> Result<u32, BackendError>;
    fn disconnect_tunnel_source(&self, source_name: &str) -> Result<bool, BackendError>;
    fn send_mic_to_remote(
        &self,
        host: &str,
        port: u16,
        description: &str,
        local_source: &str,
    ) -> Result<(u32, u32, u32, u32, String), BackendError>;
    fn stop_send_mic_to_remote(
        &self,
        host: &str,
        port: u16,
        remote_null_id: u32,
        remote_remap_id: u32,
        local_tunnel_id: u32,
        local_loopback_id: u32,
    );
    fn cleanup_orphaned_remote_mic_modules(&self) -> Result<Vec<u32>, BackendError>;

    fn get_sink_volume(&self, sink_name: &str) -> u32;
    fn set_sink_volume(&self, sink_name: &str, volume_percent: u32);
    fn get_sink_mute(&self, sink_name: &str) -> bool;
    fn set_sink_mute(&self, sink_name: &str, mute: bool);
    fn remote_set_volume(&self, host: &str, port: u16, volume_percent: u32, mute: Option<bool>);
}

pub fn listener_security_from_args(args: &str) -> String {
    if !args.contains("auth-anonymous=1") {
        "cookie".to_string()
    } else if args.contains("auth-ip-acl") {
        "trusted-lan".to_string()
    } else {
        "open".to_string()
    }
}

/// Create the platform-appropriate default `AudioBackend`.
pub fn create_backend() -> Arc<dyn AudioBackend> {
    if cfg!(windows) {
        Arc::new(windows::WindowsAudioBackend::new())
    } else {
        Arc::new(linux::LinuxAudioBackend::new())
    }
}
