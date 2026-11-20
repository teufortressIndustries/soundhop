//! Single & multi-room connection orchestration, disconnection, and auto-connect controls.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use crate::backend::TunnelParams;
use crate::config::{is_paired_security_mode, Config};
use crate::constants::{MULTIROOM_SINK_NAME, PROTOCOL_VERSION, SINK_PREFIX};
use crate::discovery::{discover_once_with_psk, BeaconV3Meta, Peer};
use crate::pairing::TrustedPeer;
use crate::security::soundhop_cookie_path;
use crate::state::ActivePeerSession;
use crate::transport::{AudioProfile, OutboundConfig, PcmCapture, DEFAULT_V3_PORT};
use crate::util::{
    is_own_non_loopback_ip, is_private_address, lock_or_recover, notify, now_secs, tcp_probe_rtt,
};

use super::{ConnectOptions, Controller, ControllerError};

/// Parse `config.audio.profile` into an [`AudioProfile`], defaulting to [`AudioProfile::OpusBalanced`].
pub fn parse_audio_profile(raw: &str) -> AudioProfile {
    match raw.trim().to_lowercase().as_str() {
        "opus-low-latency" | "opus_low_latency" => AudioProfile::OpusLowLatency,
        "opus-low-bandwidth" | "opus_low_bandwidth" => AudioProfile::OpusLowBandwidth,
        "pcm-lossless" | "pcm_lossless" => AudioProfile::PcmLossless,
        _ => AudioProfile::OpusBalanced,
    }
}

fn effective_outbound_profile(raw: &str) -> AudioProfile {
    let parsed = parse_audio_profile(raw);
    #[cfg(feature = "opus")]
    {
        parsed
    }
    #[cfg(not(feature = "opus"))]
    {
        let _ = parsed;
        AudioProfile::PcmLossless
    }
}

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

        let trusted_peer = self.lookup_trusted_peer(&peer.id);
        let v3_meta = self.peer_v3_meta(&peer.id);
        if let (Some(trusted), Some(meta)) =
            (trusted_peer, v3_meta.filter(BeaconV3Meta::supports_sht3))
        {
            return self.connect_sht3_internal(peer, &trusted, &meta, &cfg, opts);
        }
        if is_paired_security_mode(&peer.security) {
            return Err(ControllerError::Message(
                "This device requires pairing over encrypted SHT3 transport. Pair with it first."
                    .to_string(),
            ));
        }

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

        let cookie_path = self.tunnel_cookie_path(peer, &cfg);
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
                cookie_path,
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

    fn lookup_trusted_peer(&self, peer_id: &str) -> Option<TrustedPeer> {
        self.pairing_manager()
            .trust_store()
            .get(peer_id)
            .or_else(|| {
                lock_or_recover(&self.state)
                    .find_trusted_peer(peer_id)
                    .cloned()
            })
    }

    fn create_outbound_capture(
        &self,
        sink_name: &str,
        description: &str,
        frame_ms: u8,
    ) -> Result<(Box<dyn PcmCapture>, u32), ControllerError> {
        #[cfg(test)]
        if let Some(factory) = lock_or_recover(&self.capture_factory_override).clone() {
            let _ = frame_ms;
            let capture = factory().map_err(|e| {
                ControllerError::Message(format!("Could not start SHT3 audio capture: {e}"))
            })?;
            return Ok((capture, 0));
        }

        #[cfg(target_os = "linux")]
        {
            let clean_desc = crate::util::slugify(description, 48);
            let module_id = self
                .backend
                .load_module(
                    "module-null-sink",
                    &[
                        format!("sink_name={sink_name}"),
                        format!("sink_properties=device.description={clean_desc}"),
                    ],
                )
                .map_err(|e| {
                    ControllerError::Message(format!(
                        "Could not create virtual SHT3 sink {sink_name}: {e}"
                    ))
                })?;
            let monitor = format!("{sink_name}.monitor");
            match crate::transport::LinuxPcmCapture::spawn_with_latency(
                &monitor,
                u32::from(frame_ms),
            ) {
                Ok(cap) => Ok((Box::new(cap) as Box<dyn PcmCapture>, module_id)),
                Err(e) => {
                    let _ = self.backend.unload_module(module_id, true);
                    Err(ControllerError::Message(format!(
                        "Could not start Linux PCM capture on {monitor}: {e}"
                    )))
                }
            }
        }

        #[cfg(target_os = "windows")]
        {
            let _ = (sink_name, description, frame_ms);
            let cap = crate::transport::WindowsPcmCapture::start().map_err(|e| {
                ControllerError::Message(format!("Could not start Windows loopback capture: {e}"))
            })?;
            Ok((Box::new(cap) as Box<dyn PcmCapture>, 0))
        }

        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        {
            let _ = (sink_name, description, frame_ms);
            Err(ControllerError::Message(
                "SHT3 outbound audio capture is not supported on this platform".to_string(),
            ))
        }
    }

    fn connect_sht3_internal(
        &self,
        peer: &Peer,
        trusted: &TrustedPeer,
        meta: &BeaconV3Meta,
        cfg: &Config,
        opts: ConnectOptions,
    ) -> Result<serde_json::Value, ControllerError> {
        let default_v3_port = if cfg.sharing.v3_port == 0 {
            DEFAULT_V3_PORT
        } else {
            cfg.sharing.v3_port
        };
        let v3_port = meta.v3_port.filter(|&p| p > 0).unwrap_or(default_v3_port);
        let target_ip: IpAddr = peer.host.parse().map_err(|e| {
            ControllerError::Message(format!("Invalid peer address {}: {e}", peer.host))
        })?;

        if let Some(rtt) = tcp_probe_rtt(&peer.host, v3_port, Duration::from_millis(1500)) {
            self.telemetry.record_rtt(&peer.id, rtt);
        } else {
            return Err(ControllerError::Message(format!(
                "{} is not accepting encrypted SHT3 audio at {}:{}. Is it online and sharing?",
                peer.display_name(),
                peer.host,
                v3_port
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

        let profile = effective_outbound_profile(&cfg.audio.profile);
        let jitter_ms = cfg
            .connection
            .latency_ms
            .map(|ms| ms.clamp(10, 500) as u16)
            .unwrap_or(40);
        let outbound_cfg = OutboundConfig::new(
            cfg.device.id.clone(),
            SocketAddr::new(target_ip, v3_port),
            trusted.pairwise_secret,
        )
        .with_profile(profile)
        .with_jitter_ms(jitter_ms);

        let (capture, module_id) =
            self.create_outbound_capture(&sink_name, &peer.display_name(), profile.frame_ms())?;
        let handle = match self.sht3_manager.start_outbound(outbound_cfg, capture) {
            Ok(h) => h,
            Err(e) => {
                if module_id > 0 {
                    let _ = self.backend.unload_module(module_id, true);
                }
                return Err(ControllerError::Message(format!(
                    "Could not establish encrypted SHT3 audio stream to {}: {e}",
                    peer.display_name()
                )));
            }
        };

        *lock_or_recover(&self.sht3_outbound) = Some(handle);

        {
            let mut st = lock_or_recover(&self.state);
            st.connected_peer = peer.id.clone();
            st.connected_name = peer.display_name();
            st.connected_host = peer.host.clone();
            st.connected_port = v3_port;
            st.connected_sink = sink_name.clone();
            st.connected_module = module_id;
            st.multiroom_peers = vec![ActivePeerSession {
                peer_id: peer.id.clone(),
                name: peer.display_name(),
                host: peer.host.clone(),
                port: v3_port,
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

        if switch_default && module_id > 0 {
            let _ = self.make_default(&sink_name, move_streams);
        }
        let _ = self.enforce_routed_apps();

        if self.should_notify(opts.notify_user) {
            notify(
                "SoundHop",
                &format!(
                    "Encrypted audio (SHT3) is now playing on {}",
                    peer.display_name()
                ),
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
            let cookie_path = self.tunnel_cookie_path(p, &cfg);
            match self.backend.connect_tunnel(&TunnelParams {
                host: &p.host,
                port: p.port,
                sink_name: &sname,
                description: &p.display_name(),
                reconnect_delay_ms: cfg.connection.reconnect_delay_ms,
                latency_ms: cfg.connection.latency_ms,
                extra_args: &cfg.connection.extra_tunnel_args,
                cookie_path,
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

    pub(crate) fn tunnel_cookie_path(&self, peer: &Peer, cfg: &Config) -> Option<PathBuf> {
        let peer_requires_auth =
            matches!(peer.security.as_str(), "psk" | "paired" | "pin" | "cookie");
        let manual_with_local_auth = peer.source == "manual"
            && matches!(
                cfg.sharing.security.as_str(),
                "psk" | "paired" | "pin" | "cookie"
            );
        if !peer_requires_auth && !manual_with_local_auth {
            return None;
        }
        let has_pairing = !cfg.sharing.psk_hash.trim().is_empty()
            && !lock_or_recover(&self.state).paired_peers.is_empty();
        if has_pairing || peer.security == "cookie" {
            Some(soundhop_cookie_path())
        } else {
            None
        }
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
        if is_paired_security_mode(&peer.security) {
            if self.lookup_trusted_peer(&peer.id).is_none() {
                return Err(ControllerError::Message(
                    "This device requires pairing. Pair with it first.".to_string(),
                ));
            }
            let has_sht3 = self
                .peer_v3_meta(&peer.id)
                .is_some_and(|meta| meta.supports_sht3());
            if !has_sht3 {
                return Err(ControllerError::Message(format!(
                    "{} requires pairing but does not advertise SHT3 encrypted transport. Update SoundHop on both devices.",
                    peer.display_name()
                )));
            }
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
        if let Some(handle) = lock_or_recover(&self.sht3_outbound).take() {
            handle.stop();
        }
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
        let transport = if self.sht3_outbound_active() {
            "sht3"
        } else {
            "pa-tcp"
        };
        serde_json::json!({
            "connected": st.is_connected(),
            "peer_id": st.connected_peer,
            "name": st.connected_name,
            "host": st.connected_host,
            "port": st.connected_port,
            "sink": st.connected_sink,
            "module": st.connected_module,
            "transport": transport,
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

    #[test]
    fn test_tunnel_cookie_path_omits_cookie_in_trusted_lan_and_validate_peer_enforces_pairing() {
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
            None,
            None,
            false,
        );
        ctrl.set_save_enabled(false);

        let lan_peer = Peer {
            id: "peer-lan".to_string(),
            name: "Living Room".to_string(),
            host: "192.168.1.40".to_string(),
            port: 4713,
            version: VERSION.to_string(),
            protocol: PROTOCOL_VERSION,
            security: "trusted-lan".to_string(),
            platform: "linux".to_string(),
            last_seen: now_secs(),
            source: "udp".to_string(),
            rtt_ms: None,
        };
        let psk_peer = Peer {
            id: "peer-psk".to_string(),
            name: "Studio".to_string(),
            security: "psk".to_string(),
            ..lan_peer.clone()
        };
        let paired_peer = Peer {
            id: "peer-paired".to_string(),
            name: "Office".to_string(),
            security: "paired".to_string(),
            ..lan_peer.clone()
        };

        let cfg_unpaired = ctrl.config.lock().unwrap().clone();
        // Unpaired client: trusted-lan has no cookie and passes validation
        assert!(ctrl.tunnel_cookie_path(&lan_peer, &cfg_unpaired).is_none());
        assert!(ctrl.validate_peer(&lan_peer, &cfg_unpaired).is_ok());

        // Unpaired client: psk and paired peers must be rejected with actionable error
        let err_psk = ctrl
            .validate_peer(&psk_peer, &cfg_unpaired)
            .unwrap_err()
            .to_string();
        assert_eq!(err_psk, "This device requires pairing. Pair with it first.");
        let err_paired = ctrl
            .validate_peer(&paired_peer, &cfg_unpaired)
            .unwrap_err()
            .to_string();
        assert_eq!(
            err_paired,
            "This device requires pairing. Pair with it first."
        );
        assert!(ctrl.tunnel_cookie_path(&psk_peer, &cfg_unpaired).is_none());

        // Once paired via Pairing v2 + advertising SHT3: psk/paired peers pass validation.
        // Without SHT3 advertisement, paired peers MUST still be rejected (never fall back to PA-TCP).
        {
            let mut cfg = ctrl.config.lock().unwrap();
            cfg.sharing.security = "psk".to_string();
            cfg.sharing.psk_hash = "deadbeef".to_string();
        }
        ctrl.state.lock().unwrap().paired_peers.insert(
            "lan-group".to_string(),
            crate::state::PairedPeer {
                peer_id: "lan-group".to_string(),
                name: "lan-group".to_string(),
                shared_key_hex: "deadbeef".to_string(),
                cookie_hex: "cafebabe".to_string(),
                paired_at: 100.0,
            },
        );
        for target_id in ["peer-psk", "peer-paired"] {
            ctrl.record_trusted_peer(crate::pairing::TrustedPeer {
                device_id: target_id.to_string(),
                display_name: target_id.to_string(),
                ed25519_pub: [0x11u8; 32],
                pairwise_secret: [0x22u8; 32],
                fingerprint: "0102-0304-0506-0708".to_string(),
                paired_at: 100.0,
            });
        }
        let cfg_paired = ctrl.config.lock().unwrap().clone();
        // Trusted peer WITHOUT SHT3 metadata must still be rejected with an actionable error.
        let err_no_sht3 = ctrl
            .validate_peer(&paired_peer, &cfg_paired)
            .unwrap_err()
            .to_string();
        assert!(
            err_no_sht3.contains("SHT3"),
            "expected SHT3 error, got: {err_no_sht3}"
        );

        // Once the peer advertises SHT3 (v3_port + caps), validation succeeds.
        for target_id in ["peer-psk", "peer-paired"] {
            ctrl.remember_peer_v3_meta(
                target_id,
                crate::discovery::BeaconV3Meta {
                    v3_port: Some(4714),
                    pair_port: Some(4715),
                    pub_fp: Some("0102-0304-0506-0708".to_string()),
                    caps: vec!["sht3".to_string(), "opus".to_string(), "pcm".to_string()],
                },
            );
        }
        assert!(ctrl.validate_peer(&psk_peer, &cfg_paired).is_ok());
        assert!(ctrl.validate_peer(&paired_peer, &cfg_paired).is_ok());
        assert!(ctrl.tunnel_cookie_path(&psk_peer, &cfg_paired).is_some());
        assert!(ctrl.tunnel_cookie_path(&paired_peer, &cfg_paired).is_some());
        assert!(
            ctrl.tunnel_cookie_path(&lan_peer, &cfg_paired).is_none(),
            "trusted-lan peers must never receive pulse-cookie"
        );
    }

    #[test]
    fn test_config_audio_profile_maps_to_audio_profile() {
        use crate::controller::parse_audio_profile;
        use crate::transport::AudioProfile;

        assert_eq!(
            parse_audio_profile("opus-balanced"),
            AudioProfile::OpusBalanced
        );
        assert_eq!(
            parse_audio_profile("opus-low-latency"),
            AudioProfile::OpusLowLatency
        );
        assert_eq!(
            parse_audio_profile("opus-low-bandwidth"),
            AudioProfile::OpusLowBandwidth
        );
        assert_eq!(
            parse_audio_profile("pcm-lossless"),
            AudioProfile::PcmLossless
        );
        // Case-insensitive, whitespace-trimmed, and unknown/empty default to OpusBalanced.
        assert_eq!(
            parse_audio_profile("  OPUS-LOW-LATENCY "),
            AudioProfile::OpusLowLatency
        );
        assert_eq!(parse_audio_profile(""), AudioProfile::OpusBalanced);
        assert_eq!(
            parse_audio_profile("unknown-profile"),
            AudioProfile::OpusBalanced
        );
        assert_eq!(
            parse_audio_profile(&Config::default().audio.profile),
            AudioProfile::OpusBalanced
        );
    }

    #[test]
    fn test_outbound_connect_uses_sht3_for_trusted_peer_and_never_falls_back_to_pa_tcp() {
        use std::net::TcpListener;
        use std::time::Duration;

        use crate::discovery::BeaconV3Meta;
        use crate::pairing::TrustedPeer;
        use crate::transport::{MockPcmCapture, MockPcmPlayback, PcmCapture, PcmPlayback};

        let tunnel_calls = Arc::new(Mutex::new(0usize));
        let tunnel_calls_c = Arc::clone(&tunnel_calls);

        let make_runner = move || {
            let tc = Arc::clone(&tunnel_calls_c);
            Arc::new(MockRunner::new(move |args, _| {
                let joined = args.join(" ");
                if joined.contains("module-tunnel-sink") {
                    *tc.lock().unwrap() += 1;
                }
                let stdout = if joined.contains("info") {
                    "Server Name: PulseAudio (on PipeWire 1.2.0)\nDefault Sink: alsa_output.pci\n"
                        .to_string()
                } else {
                    String::new()
                };
                Ok(CommandResult {
                    args: args.to_vec(),
                    returncode: 0,
                    stdout,
                    stderr: String::new(),
                })
            }))
        };

        let free_v3_port = {
            let sock = TcpListener::bind("127.0.0.1:0").unwrap();
            sock.local_addr().unwrap().port()
        };

        // Start a remote receiver Controller in "paired" mode on free_v3_port.
        let mut server_cfg = Config::default();
        server_cfg.device.id = "server-v3".to_string();
        server_cfg.sharing.security = "paired".to_string();
        server_cfg.sharing.v3_port = free_v3_port;
        let server_ctrl = Controller::new(
            Some(server_cfg),
            Some(State::default()),
            Some(Arc::new(LinuxAudioBackend::with_runner(make_runner()))),
            None,
            None,
            false,
        );
        server_ctrl.set_save_enabled(false);
        server_ctrl.set_playback_factory(Arc::new(|_peer_id| {
            let (playback, _handle) = MockPcmPlayback::pair();
            Ok(Box::new(playback) as Box<dyn PcmPlayback>)
        }));

        let k_pair = [0x99u8; 32];
        server_ctrl.record_trusted_peer(TrustedPeer {
            device_id: "client-v3".to_string(),
            display_name: "Client Rig".to_string(),
            ed25519_pub: [0x33u8; 32],
            pairwise_secret: k_pair,
            fingerprint: "1234-5678-9ABC-DEF0".to_string(),
            paired_at: 100.0,
        });
        server_ctrl.start_listener(Some(false)).unwrap();
        let bound_v3_port = server_ctrl.sht3_manager.listener_addr().unwrap().port();

        // Client controller configured with pcm-lossless profile.
        let mut client_cfg = Config::default();
        client_cfg.device.id = "client-v3".to_string();
        client_cfg.audio.profile = "pcm-lossless".to_string();
        let client_ctrl = Controller::new(
            Some(client_cfg),
            Some(State::default()),
            Some(Arc::new(LinuxAudioBackend::with_runner(make_runner()))),
            None,
            None,
            false,
        );
        client_ctrl.set_save_enabled(false);
        client_ctrl.set_capture_factory(Arc::new(|| {
            let cap =
                MockPcmCapture::sine_wave(440.0, Some(60)).with_pace(Duration::from_millis(15));
            Ok(Box::new(cap) as Box<dyn PcmCapture>)
        }));

        let server_peer = Peer {
            id: "server-v3".to_string(),
            name: "Studio Receiver".to_string(),
            host: "127.0.0.1".to_string(),
            port: 4713,
            version: VERSION.to_string(),
            protocol: PROTOCOL_VERSION,
            security: "paired".to_string(),
            platform: "linux".to_string(),
            last_seen: now_secs(),
            source: "udp".to_string(),
            rtt_ms: None,
        };

        // 1. Before trusting server-v3, connect() must fail and NEVER invoke module-tunnel-sink.
        let err = client_ctrl
            .connect(&server_peer, ConnectOptions::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("requires pairing"));
        assert_eq!(*tunnel_calls.lock().unwrap(), 0);

        // 2. Trust server-v3 and record its SHT3 BeaconV3Meta -> connect() uses SHT3!
        client_ctrl.record_trusted_peer(TrustedPeer {
            device_id: "server-v3".to_string(),
            display_name: "Studio Receiver".to_string(),
            ed25519_pub: [0x44u8; 32],
            pairwise_secret: k_pair,
            fingerprint: "FEDC-BA98-7654-3210".to_string(),
            paired_at: 100.0,
        });
        client_ctrl.remember_peer_v3_meta(
            "server-v3",
            BeaconV3Meta {
                v3_port: Some(bound_v3_port),
                pair_port: Some(4715),
                pub_fp: Some("FEDC-BA98-7654-3210".to_string()),
                caps: vec!["sht3".to_string(), "opus".to_string(), "pcm".to_string()],
            },
        );

        let conn_res = client_ctrl
            .connect(&server_peer, ConnectOptions::default())
            .expect("trusted SHT3 connect should succeed");
        assert_eq!(conn_res["connected"], true);
        assert_eq!(conn_res["transport"], "sht3");
        assert_eq!(conn_res["port"], bound_v3_port);
        assert_eq!(
            *tunnel_calls.lock().unwrap(),
            0,
            "SHT3 connection must not load legacy PA module-tunnel-sink"
        );

        client_ctrl
            .disconnect(false, Some(false), false, Some(false), false, false)
            .unwrap();
        assert!(!client_ctrl.sht3_outbound_active());
        server_ctrl.stop_listener(Some(false)).unwrap();
    }
}
