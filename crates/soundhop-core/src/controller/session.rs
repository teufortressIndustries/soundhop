//! Single & multi-room connection orchestration, disconnection, and auto-connect controls.

use std::time::Duration;

use crate::backend::TunnelParams;
use crate::config::Config;
use crate::constants::{MULTIROOM_SINK_NAME, PROTOCOL_VERSION, SINK_PREFIX};
use crate::discovery::{discover_once_with_psk, Peer};
use crate::state::ActivePeerSession;
use crate::util::{
    is_own_non_loopback_ip, is_private_address, lock_or_recover, notify, now_secs, tcp_probe_rtt,
};

use super::{ConnectOptions, Controller, ControllerError};

impl Controller {
    fn resolve_live_peer(&self, peer: &Peer, cfg: &Config) -> Peer {
        if !is_own_non_loopback_ip(&peer.host)
            && tcp_probe_rtt(&peer.host, peer.port, Duration::from_millis(800)).is_some()
        {
            return peer.clone();
        }
        let psk = (!cfg.sharing.psk_hash.is_empty()).then_some(cfg.sharing.psk_hash.as_str());
        for found in discover_once_with_psk(0.7, cfg.discovery.port, &cfg.device.id, psk) {
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
        let _op_guard = lock_or_recover(&self.connect_lock);
        self.connect_internal(peer, opts)
    }

    fn connect_internal(
        &self,
        peer: &Peer,
        opts: ConnectOptions,
    ) -> Result<serde_json::Value, ControllerError> {
        self.reload_state_for_op();
        let cfg = lock_or_recover(&self.config).clone();
        if opts.keep_local || cfg.connection.keep_local_output {
            let mut m_opts = opts;
            m_opts.keep_local = true;
            return self.connect_multi_internal(std::slice::from_ref(peer), m_opts);
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

        if lock_or_recover(&self.state).is_connected() {
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
            let mut st = lock_or_recover(&self.state);
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
        lock_or_recover(&self.config).connection.last_peer = peer.id.clone();
        self.remember_peer(peer);

        if opts.with_mic {
            let _ = self.enable_mic_internal(&opts.mic_direction);
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
        let _op_guard = lock_or_recover(&self.connect_lock);
        self.connect_multi_internal(peers, opts)
    }

    fn connect_multi_internal(
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
            return self.connect_internal(&peers[0], opts);
        }
        self.ensure_backend()?;
        let cfg = lock_or_recover(&self.config).clone();
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

        if lock_or_recover(&self.state).is_connected() {
            let _ = self.disconnect_internal(false, false, false, Some(false), true, false, false);
        }
        self.remember_previous_default();

        let mut sessions = Vec::new();
        let mut slave_sinks = Vec::new();
        if opts.keep_local {
            let prev = lock_or_recover(&self.state).previous_default_sink.clone();
            let local_sink = self.local_default_candidate(&prev);
            if !local_sink.is_empty() {
                slave_sinks.push(local_sink);
            }
        }

        for p in peers {
            let sname = Self::sink_name_for(p);
            match self.backend.connect_tunnel(&TunnelParams {
                host: &p.host,
                port: p.port,
                sink_name: &sname,
                description: &p.display_name(),
                reconnect_delay_ms: cfg.connection.reconnect_delay_ms,
                latency_ms: cfg.connection.latency_ms,
                extra_args: &cfg.connection.extra_tunnel_args,
            }) {
                Ok(mid) => {
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
                Err(e) => {
                    for s in &sessions {
                        let _ = self.backend.unload_module(s.module_id, true);
                    }
                    return Err(e.into());
                }
            }
        }

        let names_joined: Vec<String> = sessions.iter().map(|s| s.name.clone()).collect();
        let label = names_joined.join("+");
        let combine_mod =
            match self
                .backend
                .create_combined_sink(MULTIROOM_SINK_NAME, &label, &slave_sinks)
            {
                Ok(id) => id,
                Err(e) => {
                    for s in &sessions {
                        let _ = self.backend.unload_module(s.module_id, true);
                    }
                    return Err(e.into());
                }
            };

        let first = sessions[0].clone();
        {
            let mut st = lock_or_recover(&self.state);
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
        lock_or_recover(&self.config).connection.last_peer = first.peer_id.clone();
        if opts.with_mic {
            let _ = self.enable_mic_internal(&opts.mic_direction);
        }
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
        let _op_guard = lock_or_recover(&self.connect_lock);
        self.reload_state_for_op();
        let cfg_restore = lock_or_recover(&self.config).connection.restore_default;
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
        let st_snapshot = lock_or_recover(&self.state).clone();
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
            let _ = self.disable_mic_internal();
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
                if lock_or_recover(&self.config).connection.move_streams {
                    let _ = self.backend.move_sink_inputs(&candidate, None);
                }
            }
        }

        {
            let mut st = lock_or_recover(&self.state);
            st.clear_connection();
            if !keep_previous {
                st.previous_default_sink.clear();
                st.previous_default_source.clear();
            }
            if forget {
                let mut cfg = lock_or_recover(&self.config);
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
            "auto_paused": lock_or_recover(&self.state).auto_paused,
        }))
    }

    fn connection_dict(&self) -> serde_json::Value {
        let st = lock_or_recover(&self.state);
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
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crate::backend::linux::LinuxAudioBackend;
    use crate::config::Config;
    use crate::constants::{PROTOCOL_VERSION, VERSION};
    use crate::controller::{ConnectOptions, Controller};
    use crate::discovery::Peer;
    use crate::state::State;
    use crate::util::{now_secs, CommandResult, MockRunner};

    #[test]
    fn test_connect_multi_rolls_back_on_combined_sink_failure() {
        let listener1 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port1 = listener1.local_addr().unwrap().port();
        let listener2 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port2 = listener2.local_addr().unwrap().port();

        let peer1 = Peer {
            id: "p1".to_string(),
            name: "p1".to_string(),
            host: "127.0.0.1".to_string(),
            port: port1,
            version: VERSION.to_string(),
            protocol: PROTOCOL_VERSION,
            security: "open".to_string(),
            platform: "linux".to_string(),
            last_seen: now_secs(),
            source: "manual".to_string(),
            rtt_ms: None,
        };
        let peer2 = Peer {
            id: "p2".to_string(),
            name: "p2".to_string(),
            host: "127.0.0.1".to_string(),
            port: port2,
            ..peer1.clone()
        };

        let sname1 = Controller::sink_name_for(&peer1);
        let sname2 = Controller::sink_name_for(&peer2);
        let sname1_c = sname1.clone();
        let sname2_c = sname2.clone();

        let unloaded = Arc::new(Mutex::new(Vec::<String>::new()));
        let unloaded_c = Arc::clone(&unloaded);
        let active_sinks = Arc::new(Mutex::new(Vec::<(u32, String)>::new()));
        let active_sinks_c = Arc::clone(&active_sinks);

        let runner = Arc::new(MockRunner::new(move |args, _| {
            let joined = args.join(" ");
            if joined.contains("unload-module") {
                unloaded_c.lock().unwrap().push(joined.clone());
            }
            if joined.contains("module-combine-sink") {
                return Ok(CommandResult {
                    args: args.to_vec(),
                    returncode: 1,
                    stdout: String::new(),
                    stderr: "combine failed".to_string(),
                });
            }
            let stdout = if joined.contains("info") {
                "Server Name: PulseAudio (on PipeWire 1.2.0)\nDefault Sink: alsa_output.pci\n"
                    .to_string()
            } else if joined.contains("list short sinks") {
                let mut out =
                    "1\talsa_output.pci\tPipeWire\ts16le 2ch 48000Hz\tRUNNING\n".to_string();
                for (id, s) in active_sinks_c.lock().unwrap().iter() {
                    out.push_str(&format!(
                        "{id}\t{s}\tmodule-tunnel-sink\ts16le 2ch 48000Hz\tRUNNING\n"
                    ));
                }
                out
            } else if joined.contains("module-tunnel-sink")
                && joined.contains(&format!("sink_name={sname1_c}"))
            {
                active_sinks_c.lock().unwrap().push((201, sname1_c.clone()));
                "201\n".to_string()
            } else if joined.contains("module-tunnel-sink")
                && joined.contains(&format!("sink_name={sname2_c}"))
            {
                active_sinks_c.lock().unwrap().push((202, sname2_c.clone()));
                "202\n".to_string()
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

        let res = ctrl.connect_multi(&[peer1, peer2], ConnectOptions::default());
        assert!(res.is_err());

        let unloaded_cmds = unloaded.lock().unwrap();
        assert!(unloaded_cmds.iter().any(|c| c.contains("201")));
        assert!(unloaded_cmds.iter().any(|c| c.contains("202")));
        drop(listener1);
        drop(listener2);
    }
}
