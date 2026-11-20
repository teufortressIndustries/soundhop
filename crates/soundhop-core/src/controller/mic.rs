//! Microphone pass-through (send local mic to remote or receive remote mic).

use crate::backend::SourceTunnelParams;
use crate::constants::{PROTOCOL_VERSION, SOURCE_PREFIX, VERSION};
use crate::discovery::Peer;
use crate::util::{lock_or_recover, now_secs};

use super::{Controller, ControllerError};

impl Controller {
    pub fn enable_mic(&self, direction: &str) -> Result<serde_json::Value, ControllerError> {
        let _op_guard = lock_or_recover(&self.connect_lock);
        self.enable_mic_internal(direction)
    }

    pub(super) fn enable_mic_internal(
        &self,
        direction: &str,
    ) -> Result<serde_json::Value, ControllerError> {
        let st_snap = lock_or_recover(&self.state).clone();
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
            let _ = self.disable_mic_internal();
        }

        if dir_val == "send" {
            let dev_name = lock_or_recover(&self.config).device.name.clone();
            let (r_null, r_remap, l_tun, l_loop, r_src) = self.backend.send_mic_to_remote(
                &st_snap.connected_host,
                st_snap.connected_port,
                &dev_name,
                "@DEFAULT_SOURCE@",
            )?;
            let mut st = lock_or_recover(&self.state);
            st.connected_source = r_src.clone();
            st.connected_source_module = l_tun;
            st.remote_mic_null_module = r_null.unwrap_or(0);
            st.remote_mic_null_set = r_null.is_some();
            st.remote_mic_remap_module = r_remap.unwrap_or(0);
            st.remote_mic_remap_set = r_remap.is_some();
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
                let mut st = lock_or_recover(&self.state);
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
        let _op_guard = lock_or_recover(&self.connect_lock);
        self.disable_mic_internal()
    }

    pub(super) fn disable_mic_internal(&self) -> Result<serde_json::Value, ControllerError> {
        let st_snap = lock_or_recover(&self.state).clone();
        if st_snap.connected_source.is_empty() && st_snap.mic_direction.is_empty() {
            return Ok(serde_json::json!({ "enabled": false }));
        }
        if st_snap.mic_direction == "send" {
            let null_opt = if st_snap.remote_mic_null_set || st_snap.remote_mic_null_module > 0 {
                Some(st_snap.remote_mic_null_module)
            } else {
                None
            };
            let remap_opt = if st_snap.remote_mic_remap_set || st_snap.remote_mic_remap_module > 0 {
                Some(st_snap.remote_mic_remap_module)
            } else {
                None
            };
            self.backend.stop_send_mic_to_remote(
                &st_snap.connected_host,
                st_snap.connected_port,
                null_opt,
                remap_opt,
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
            let mut st = lock_or_recover(&self.state);
            st.connected_source.clear();
            st.connected_source_module = 0;
            st.remote_mic_null_module = 0;
            st.remote_mic_null_set = false;
            st.remote_mic_remap_module = 0;
            st.remote_mic_remap_set = false;
            st.local_mic_loopback_module = 0;
            st.mic_direction.clear();
        }
        self.save();
        Ok(serde_json::json!({ "enabled": false }))
    }

    fn remember_previous_default_source(&self) {
        let current = self.backend.default_source().unwrap_or_default();
        let mut st = lock_or_recover(&self.state);
        if !current.is_empty()
            && !current.starts_with(SOURCE_PREFIX)
            && !current.ends_with(".monitor")
        {
            st.previous_default_source = current;
        }
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
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::backend::linux::LinuxAudioBackend;
    use crate::config::Config;
    use crate::constants::{MULTIROOM_SINK_NAME, PROTOCOL_VERSION, VERSION};
    use crate::controller::{ConnectOptions, Controller};
    use crate::discovery::Peer;
    use crate::state::State;
    use crate::util::{now_secs, CommandResult, MockRunner};

    #[test]
    fn test_connect_keep_local_preserves_with_mic() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let peer = Peer {
            id: "p_mic".to_string(),
            name: "MicPeer".to_string(),
            host: "127.0.0.1".to_string(),
            port,
            version: VERSION.to_string(),
            protocol: PROTOCOL_VERSION,
            security: "open".to_string(),
            platform: "linux".to_string(),
            last_seen: now_secs(),
            source: "manual".to_string(),
            rtt_ms: None,
        };
        let sname = Controller::sink_name_for(&peer);
        let sname_c = sname.clone();
        let loaded = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let loaded_c = Arc::clone(&loaded);
        let runner = Arc::new(MockRunner::new(move |args, _| {
            let joined = args.join(" ");
            let stdout = if joined.contains("info") {
                "Server Name: PulseAudio (on PipeWire 1.2.0)\nDefault Sink: alsa_output.pci\n"
                    .to_string()
            } else if joined.contains("list short sinks") {
                if loaded_c.load(std::sync::atomic::Ordering::SeqCst) {
                    format!(
                        "1\talsa_output.pci\tPipeWire\ts16le 2ch 48000Hz\tRUNNING\n2\t{sname_c}\tmodule-tunnel-sink\ts16le 2ch 48000Hz\tRUNNING\n3\t{MULTIROOM_SINK_NAME}\tmodule-combine-sink\ts16le 2ch 48000Hz\tRUNNING\n"
                    )
                } else {
                    "1\talsa_output.pci\tPipeWire\ts16le 2ch 48000Hz\tRUNNING\n".to_string()
                }
            } else if joined.contains("load-module") {
                loaded_c.store(true, std::sync::atomic::Ordering::SeqCst);
                "55\n".to_string()
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
        let res = ctrl
            .connect(
                &peer,
                ConnectOptions {
                    keep_local: true,
                    with_mic: true,
                    mic_direction: "send".to_string(),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(res["with_mic"], true);
        assert_eq!(res["mic_direction"], "send");
        drop(listener);
    }
}
