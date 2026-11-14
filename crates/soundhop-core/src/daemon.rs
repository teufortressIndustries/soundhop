//! The SoundHop user daemon: owns the TCP audio listener, UDP/mDNS discovery,
//! `AudioEventMonitor` (`pactl subscribe`), `TelemetryMonitor`, auto-connect supervisor,
//! and local IPC control socket.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::constants::{OFFLINE_PROBE_LIMIT, SUPERVISOR_INTERVAL_SECS, VERSION};
use crate::controller::{ConnectOptions, Controller};
use crate::discovery::{discover_once, DiscoveryManager, Peer};
use crate::ipc::{is_running, request as ipc_request, IpcServer};
use crate::util::{
    command_exists, ensure_dir, is_private_address, now_secs, pid_path, runtime_dir, socket_path,
    split_host_port, tcp_probe_rtt,
};

pub struct Daemon {
    pub controller: Arc<Controller>,
    pub discovery: Mutex<Option<Arc<DiscoveryManager>>>,
    pub ipc: Mutex<Option<IpcServer>>,
    ipc_path: Option<PathBuf>,
    stop_flag: Arc<AtomicBool>,
    started_at: f64,
    advertising: Arc<AtomicBool>,
    warnings: Arc<Mutex<Vec<String>>>,
    supervisor_thread: Mutex<Option<JoinHandle<()>>>,
    audio_monitor_thread: Mutex<Option<JoinHandle<()>>>,
    audio_monitor_child: Arc<Mutex<Option<std::process::Child>>>,
}

impl Daemon {
    pub fn new(
        config_path: Option<PathBuf>,
        state_path: Option<PathBuf>,
        notify_enabled: bool,
        ipc_path: Option<PathBuf>,
    ) -> Arc<Self> {
        let controller = Arc::new(Controller::new(
            None,
            None,
            None,
            config_path,
            state_path,
            notify_enabled,
        ));
        Arc::new(Self {
            controller,
            discovery: Mutex::new(None),
            ipc: Mutex::new(None),
            ipc_path,
            stop_flag: Arc::new(AtomicBool::new(false)),
            started_at: now_secs(),
            advertising: Arc::new(AtomicBool::new(false)),
            warnings: Arc::new(Mutex::new(Vec::new())),
            supervisor_thread: Mutex::new(None),
            audio_monitor_thread: Mutex::new(None),
            audio_monitor_child: Arc::new(Mutex::new(None)),
        })
    }

    pub fn start(self: &Arc<Self>) -> Result<(), String> {
        if is_running(self.ipc_path.as_deref()) {
            self.try_replace_stale_daemon();
        }
        if is_running(self.ipc_path.as_deref()) {
            return Err("SoundHop daemon is already running".to_string());
        }
        self.stop_flag.store(false, Ordering::SeqCst);
        let _ = self.controller.reconcile();
        self.setup_sharing();
        self.setup_discovery();

        let self_weak = Arc::downgrade(self);
        let ipc_server = IpcServer::new(
            Arc::new(move |req| {
                if let Some(daemon) = self_weak.upgrade() {
                    daemon.handle_ipc(req)
                } else {
                    serde_json::json!({ "ok": false, "error": "Daemon shutting down" })
                }
            }),
            self.ipc_path.clone(),
        );
        ipc_server.start().map_err(|e| e.to_string())?;
        *self.ipc.lock().unwrap() = Some(ipc_server);

        self.write_pid();

        let sup_daemon = Arc::clone(self);
        *self.supervisor_thread.lock().unwrap() = Some(thread::spawn(move || {
            sup_daemon.supervisor_loop();
        }));

        let mon_daemon = Arc::clone(self);
        *self.audio_monitor_thread.lock().unwrap() = Some(thread::spawn(move || {
            mon_daemon.audio_monitor_loop();
        }));

        Ok(())
    }

    /// Start UDP/mDNS discovery without owning the IPC socket (used when an external
    /// `soundhop daemon` service is already running on the host).
    pub fn start_discovery_only(&self) {
        let enabled = self.controller.config.lock().unwrap().sharing.enabled;
        self.advertising.store(enabled, Ordering::SeqCst);
        self.setup_discovery();
    }

    fn try_replace_stale_daemon(&self) {
        let Ok(resp) = ipc_request(
            "ping",
            serde_json::json!({}),
            self.ipc_path.as_deref(),
            Duration::from_secs(2),
        ) else {
            return;
        };
        let running_ver = resp
            .get("version")
            .or_else(|| resp.get("daemon").and_then(|d| d.get("version")))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if running_ver == VERSION {
            return;
        }
        let _ = ipc_request(
            "stop",
            serde_json::json!({}),
            self.ipc_path.as_deref(),
            Duration::from_secs(1),
        );
        #[cfg(unix)]
        {
            let pid_opt = resp
                .get("pid")
                .or_else(|| resp.get("daemon").and_then(|d| d.get("pid")))
                .and_then(|v| v.as_u64());
            if let Some(pid) = pid_opt {
                if crate::ipc::is_soundhop_process(pid) {
                    unsafe {
                        libc::kill(pid as libc::pid_t, libc::SIGTERM);
                    }
                }
            }
        }
        for _ in 0..25 {
            if !is_running(self.ipc_path.as_deref()) {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        if !is_running(self.ipc_path.as_deref()) {
            let sock = self.ipc_path.clone().unwrap_or_else(socket_path);
            let _ = fs::remove_file(sock);
        }
    }

    pub fn run_until_stopped(self: &Arc<Self>) -> Result<(), String> {
        crate::util::install_signal_handlers();
        self.start()?;
        while !self.stop_flag.load(Ordering::SeqCst) && !crate::util::signal_stop_requested() {
            thread::sleep(Duration::from_millis(150));
        }
        self.shutdown();
        Ok(())
    }

    pub fn request_stop(&self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(mut child) = self.audio_monitor_child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    pub fn shutdown(&self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(mut child) = self.audio_monitor_child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(ipc) = self.ipc.lock().unwrap().take() {
            ipc.stop();
        }
        if let Some(disc) = self.discovery.lock().unwrap().take() {
            disc.stop();
        }
        if let Some(h) = self.supervisor_thread.lock().unwrap().take() {
            let _ = h.join();
        }
        if let Some(h) = self.audio_monitor_thread.lock().unwrap().take() {
            let _ = h.join();
        }
        let disconnect_on_quit = self
            .controller
            .config
            .lock()
            .unwrap()
            .connection
            .disconnect_on_quit;
        if disconnect_on_quit {
            if self.controller.state.lock().unwrap().is_connected() {
                let _ = self
                    .controller
                    .disconnect(false, None, false, Some(false), true, false);
            }
            if self.controller.android_session.is_active() {
                let _ = self.controller.android_session.stop();
            }
        }
        let _ = fs::remove_file(pid_path());
    }

    fn setup_sharing(&self) {
        self.advertising.store(false, Ordering::SeqCst);
        let enabled = self.controller.config.lock().unwrap().sharing.enabled;
        if !enabled {
            return;
        }
        match self.controller.start_listener(Some(false)) {
            Ok(_) => self.advertising.store(true, Ordering::SeqCst),
            Err(e) => {
                self.warnings
                    .lock()
                    .unwrap()
                    .push(format!("Could not start the audio listener: {e}"));
            }
        }
    }

    fn setup_discovery(&self) {
        if let Some(old) = self.discovery.lock().unwrap().take() {
            old.stop();
        }
        let cfg = self.controller.config.lock().unwrap().clone();
        let psk = if cfg.sharing.psk_hash.is_empty() {
            None
        } else {
            Some(cfg.sharing.psk_hash.clone())
        };
        let disc = Arc::new(DiscoveryManager::new(
            cfg.device.id.clone(),
            cfg.device.name.clone(),
            cfg.sharing.port,
            cfg.discovery.port,
            cfg.sharing.security.clone(),
            psk,
            cfg.discovery.interval_s,
            cfg.discovery.udp,
            cfg.discovery.mdns,
        ));
        let ctrl_clone = Arc::clone(&self.controller);
        disc.store.set_on_peer(move |peer| {
            ctrl_clone.remember_peer(peer);
        });
        let warnings = disc.start(self.advertising.load(Ordering::SeqCst));
        self.warnings.lock().unwrap().extend(warnings);
        *self.discovery.lock().unwrap() = Some(disc);
    }

    fn write_pid(&self) {
        let p = pid_path();
        if let Some(parent) = p.parent() {
            let _ = ensure_dir(parent);
        }
        let _ = fs::write(&p, format!("{}\n", std::process::id()));
        self.controller.state.lock().unwrap().daemon_last_started = now_secs();
        self.controller.save();
    }

    fn supervisor_loop(&self) {
        let mut offline_count = 0u32;
        let mut last_reconcile = Instant::now();
        while !self.stop_flag.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_secs_f64(SUPERVISOR_INTERVAL_SECS));
            if self.stop_flag.load(Ordering::SeqCst) {
                break;
            }
            if last_reconcile.elapsed() >= Duration::from_secs(10) {
                let _ = self.controller.reconcile();
                last_reconcile = Instant::now();
            } else {
                self.controller.reload_from_disk();
            }

            let st = self.controller.state.lock().unwrap().clone();
            let cfg = self.controller.config.lock().unwrap().clone();

            if st.is_connected() {
                if let Some(rtt) = tcp_probe_rtt(
                    &st.connected_host,
                    st.connected_port,
                    Duration::from_millis(1200),
                ) {
                    offline_count = 0;
                    self.controller
                        .telemetry
                        .record_rtt(&st.connected_peer, rtt);
                } else {
                    offline_count += 1;
                    if offline_count >= OFFLINE_PROBE_LIMIT {
                        let _ = self.controller.disconnect(
                            false,
                            None,
                            false,
                            Some(true),
                            false,
                            false,
                        );
                        offline_count = 0;
                    }
                }
            } else if cfg.connection.auto_connect
                && !st.auto_paused
                && !cfg.connection.last_peer.is_empty()
            {
                let target_id = &cfg.connection.last_peer;
                let peer_opt = self
                    .discovery
                    .lock()
                    .unwrap()
                    .as_ref()
                    .and_then(|d| d.store.get(target_id))
                    .or_else(|| self.controller.known_peer(target_id));
                if let Some(peer) = peer_opt {
                    let _ = self.controller.connect(
                        &peer,
                        ConnectOptions {
                            by: "auto".to_string(),
                            ..Default::default()
                        },
                    );
                }
            }
        }
    }

    fn audio_monitor_loop(&self) {
        if !command_exists("pactl") {
            while !self.stop_flag.load(Ordering::SeqCst) {
                if self.controller.state.lock().unwrap().is_connected() {
                    let _ = self.controller.sync_remote_volume();
                    let _ = self.controller.enforce_routed_apps();
                }
                thread::sleep(Duration::from_millis(500));
            }
            return;
        }

        while !self.stop_flag.load(Ordering::SeqCst) {
            let mut cmd = Command::new("pactl");
            cmd.arg("subscribe")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            crate::util::close_fds_pre_exec(&mut cmd);
            let Ok(mut child) = cmd.spawn() else {
                thread::sleep(Duration::from_secs(3));
                continue;
            };
            let stdout_opt = child.stdout.take();
            *self.audio_monitor_child.lock().unwrap() = Some(child);

            if let Some(stdout) = stdout_opt {
                let reader = BufReader::new(stdout);
                let mut last_app_sync: Option<Instant> = None;
                let mut last_vol_sync: Option<Instant> = None;
                for line in reader.lines().map_while(Result::ok) {
                    if self.stop_flag.load(Ordering::SeqCst) {
                        break;
                    }
                    let lower = line.to_lowercase();
                    if lower.contains("on sink-input") {
                        if last_app_sync.is_none_or(|t| t.elapsed() >= Duration::from_millis(150)) {
                            let _ = self.controller.enforce_routed_apps();
                            last_app_sync = Some(Instant::now());
                        }
                    } else if lower.contains("on sink")
                        && last_vol_sync.is_none_or(|t| t.elapsed() >= Duration::from_millis(150))
                    {
                        let _ = self.controller.sync_remote_volume();
                        last_vol_sync = Some(Instant::now());
                    }
                }
            }

            if let Some(mut child) = self.audio_monitor_child.lock().unwrap().take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            if !self.stop_flag.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_secs(1));
            }
        }
    }

    pub fn resolve_peer(
        &self,
        query: Option<&str>,
        host: Option<&str>,
        port: Option<u16>,
        name: Option<&str>,
    ) -> Result<Peer, String> {
        let cfg = self.controller.config.lock().unwrap().clone();
        if let Some(h) = host.filter(|s| !s.trim().is_empty()) {
            let (parsed_host, parsed_port) = split_host_port(h, port.unwrap_or(cfg.sharing.port));
            if !is_private_address(&parsed_host) {
                return Err(format!("Refusing non-private host: {parsed_host}"));
            }
            let p_id = format!("manual-{parsed_host}-{parsed_port}");
            return Ok(Peer {
                id: p_id,
                name: name
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or(&parsed_host)
                    .to_string(),
                host: parsed_host,
                port: parsed_port,
                version: VERSION.to_string(),
                protocol: crate::constants::PROTOCOL_VERSION,
                security: "trusted-lan".to_string(),
                platform: "linux".to_string(),
                last_seen: now_secs(),
                source: "manual".to_string(),
                rtt_ms: None,
            });
        }

        let live_peers = self
            .discovery
            .lock()
            .unwrap()
            .as_ref()
            .map(|d| d.peers())
            .unwrap_or_default();

        let q = query.unwrap_or("").trim();
        if q.is_empty() {
            if !cfg.connection.last_peer.is_empty() {
                if let Some(p) = live_peers
                    .iter()
                    .find(|p| p.id == cfg.connection.last_peer)
                    .cloned()
                    .or_else(|| self.controller.known_peer(&cfg.connection.last_peer))
                {
                    return Ok(p);
                }
            }
            if live_peers.len() == 1 {
                return Ok(live_peers[0].clone());
            }
            if live_peers.is_empty() {
                return Err("No devices found on the local network.".to_string());
            }
            return Err("Multiple devices found; specify a name, ID, or host.".to_string());
        }

        let q_lower = q.to_lowercase();
        for p in &live_peers {
            if p.id.eq_ignore_ascii_case(q)
                || p.name.eq_ignore_ascii_case(q)
                || p.host.eq_ignore_ascii_case(q)
                || p.name.to_lowercase().contains(&q_lower)
            {
                return Ok(p.clone());
            }
        }
        if let Some(kp) = self.controller.known_peer(q) {
            return Ok(kp);
        }
        for kp in self.controller.state.lock().unwrap().known_peers.values() {
            if kp.name.eq_ignore_ascii_case(q) || kp.host.eq_ignore_ascii_case(q) {
                return self
                    .controller
                    .known_peer(&kp.id)
                    .ok_or_else(|| format!("Unknown device: {q}"));
            }
        }
        for found in discover_once(0.8, cfg.discovery.port, &cfg.device.id) {
            self.controller.remember_peer(&found);
            if found.id.eq_ignore_ascii_case(q)
                || found.name.eq_ignore_ascii_case(q)
                || found.host.eq_ignore_ascii_case(q)
                || found.name.to_lowercase().contains(&q_lower)
            {
                return Ok(found);
            }
        }
        if is_private_address(q) {
            let (parsed_host, parsed_port) = split_host_port(q, port.unwrap_or(cfg.sharing.port));
            return Ok(Peer {
                id: format!("manual-{parsed_host}-{parsed_port}"),
                name: name.unwrap_or(&parsed_host).to_string(),
                host: parsed_host,
                port: parsed_port,
                version: VERSION.to_string(),
                protocol: crate::constants::PROTOCOL_VERSION,
                security: "trusted-lan".to_string(),
                platform: "linux".to_string(),
                last_seen: now_secs(),
                source: "manual".to_string(),
                rtt_ms: None,
            });
        }
        Err(format!("Device not found: {q}"))
    }

    pub fn handle_ipc(&self, req: Value) -> Value {
        let cmd = req.get("cmd").and_then(|v| v.as_str()).unwrap_or("");
        match cmd {
            "ping" => serde_json::json!({
                "ok": true,
                "pong": true,
                "pid": std::process::id(),
                "version": VERSION,
            }),
            "status" => {
                let peers = self
                    .discovery
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|d| d.peers())
                    .unwrap_or_default();
                let mut st = self.controller.status(Some(&peers));
                if let Some(obj) = st.as_object_mut() {
                    obj.insert("ok".to_string(), Value::Bool(true));
                    obj.insert(
                        "daemon".to_string(),
                        serde_json::json!({
                            "running": true,
                            "pid": std::process::id(),
                            "uptime_s": (now_secs() - self.started_at).max(0.0),
                            "advertising": self.advertising.load(Ordering::SeqCst),
                            "warnings": *self.warnings.lock().unwrap(),
                            "runtime_dir": runtime_dir().to_string_lossy(),
                        }),
                    );
                }
                st
            }
            "peers" | "discover" => {
                if let Some(disc) = self.discovery.lock().unwrap().as_ref() {
                    disc.probe();
                    let list: Vec<Value> = disc.peers().into_iter().map(|p| p.to_json()).collect();
                    serde_json::json!({ "ok": true, "peers": list })
                } else {
                    serde_json::json!({ "ok": true, "peers": [] })
                }
            }
            "connect" => {
                let peer_arg = req
                    .get("peer")
                    .or_else(|| req.get("query"))
                    .and_then(|v| v.as_str());
                let peers_arr = req.get("peers").and_then(|v| v.as_array());
                let host = req.get("host").and_then(|v| v.as_str());
                let port = req.get("port").and_then(|v| v.as_u64()).map(|p| p as u16);
                let name = req.get("name").and_then(|v| v.as_str());
                let switch_default = req.get("switch_default").and_then(|v| v.as_bool());
                let move_streams = req.get("move_streams").and_then(|v| v.as_bool());
                let keep_local = req
                    .get("keep_local")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let with_mic = req
                    .get("with_mic")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let mic_direction = req
                    .get("mic_direction")
                    .and_then(|v| v.as_str())
                    .unwrap_or("send")
                    .to_string();

                let opts = ConnectOptions {
                    switch_default,
                    move_streams,
                    keep_local,
                    by: "manual".to_string(),
                    notify_user: None,
                    with_mic,
                    mic_direction,
                };

                if let Some(arr) = peers_arr.filter(|a| !a.is_empty()) {
                    let mut resolved = Vec::new();
                    for item in arr {
                        if let Some(s) = item.as_str() {
                            match self.resolve_peer(Some(s), None, port, None) {
                                Ok(p) => resolved.push(p),
                                Err(e) => return serde_json::json!({ "ok": false, "error": e }),
                            }
                        }
                    }
                    match self.controller.connect_multi(&resolved, opts) {
                        Ok(res) => serde_json::json!({ "ok": true, "connection": res }),
                        Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
                    }
                } else {
                    match self.resolve_peer(peer_arg, host, port, name) {
                        Ok(peer) => match self.controller.connect(&peer, opts) {
                            Ok(res) => serde_json::json!({ "ok": true, "connection": res }),
                            Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
                        },
                        Err(e) => serde_json::json!({ "ok": false, "error": e }),
                    }
                }
            }
            "disconnect" => {
                let forget = req.get("forget").and_then(|v| v.as_bool()).unwrap_or(false);
                let restore = req.get("restore").and_then(|v| v.as_bool());
                let all = req.get("all").and_then(|v| v.as_bool()).unwrap_or(false);
                match self
                    .controller
                    .disconnect(forget, restore, true, None, all, false)
                {
                    Ok(res) => res,
                    Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
                }
            }
            "pair" => {
                let peer = req.get("peer").and_then(|v| v.as_str()).unwrap_or("");
                let secret = req.get("secret").and_then(|v| v.as_str());
                match self.controller.pair_peer(peer, secret) {
                    Ok(res) => res,
                    Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
                }
            }
            "share" => {
                let enabled = req.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
                match self.controller.share(enabled, None) {
                    Ok(res) => {
                        self.advertising.store(enabled, Ordering::SeqCst);
                        if let Some(disc) = self.discovery.lock().unwrap().as_ref() {
                            disc.set_advertising(enabled);
                        }
                        res
                    }
                    Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
                }
            }
            "apps" => {
                serde_json::json!({ "ok": true, "apps": self.controller.list_apps() })
            }
            "route" => {
                let app = req.get("app").and_then(|v| v.as_str()).unwrap_or("");
                let target = req.get("target").and_then(|v| v.as_str());
                let save = req.get("save").and_then(|v| v.as_bool()).unwrap_or(false);
                match self.controller.route_app(app, target, save) {
                    Ok(res) => res,
                    Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
                }
            }
            "mic" => {
                let action = req
                    .get("action")
                    .and_then(|v| v.as_str())
                    .unwrap_or("status");
                let direction = req
                    .get("direction")
                    .and_then(|v| v.as_str())
                    .unwrap_or("send");
                match action {
                    "on" | "enable" | "send" | "receive" => {
                        let dir = if matches!(action, "send" | "receive") {
                            action
                        } else {
                            direction
                        };
                        match self.controller.enable_mic(dir) {
                            Ok(res) => res,
                            Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
                        }
                    }
                    "off" | "disable" => match self.controller.disable_mic() {
                        Ok(res) => res,
                        Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
                    },
                    _ => {
                        let st = self.controller.state.lock().unwrap();
                        serde_json::json!({
                            "ok": true,
                            "enabled": !st.connected_source.is_empty(),
                            "direction": st.mic_direction,
                            "source": st.connected_source,
                        })
                    }
                }
            }
            "volume" => {
                let vol = req.get("volume").and_then(|v| v.as_u64()).map(|v| v as u32);
                let mute = req.get("mute").and_then(|v| v.as_bool());
                if vol.is_some() || mute.is_some() {
                    self.controller.set_volume(vol, mute, true)
                } else {
                    self.controller.get_volume()
                }
            }
            "auto" => {
                let action = req
                    .get("action")
                    .and_then(|v| v.as_str())
                    .unwrap_or("status");
                match action {
                    "on" => self.controller.set_auto(true),
                    "off" => self.controller.set_auto(false),
                    "pause" => self.controller.pause_auto(true),
                    "resume" => self.controller.pause_auto(false),
                    _ => {
                        let enabled = self
                            .controller
                            .config
                            .lock()
                            .unwrap()
                            .connection
                            .auto_connect;
                        let paused = self.controller.state.lock().unwrap().auto_paused;
                        serde_json::json!({ "ok": true, "auto_connect": enabled, "paused": paused })
                    }
                }
            }
            "telemetry" => {
                serde_json::json!({
                    "ok": true,
                    "telemetry": self.controller.telemetry_snapshot(),
                })
            }
            "config_patch" => {
                if let Some(patch) = req.get("patch") {
                    let res = self.controller.config.lock().unwrap().apply_patch(patch);
                    if let Err(e) = res {
                        return serde_json::json!({ "ok": false, "error": e.to_string() });
                    }
                    self.controller.save();
                    self.setup_sharing();
                    self.setup_discovery();
                }
                let cfg_json = self.controller.config.lock().unwrap().to_json_value();
                serde_json::json!({ "ok": true, "config": cfg_json })
            }
            "reload" => match self.controller.reload_config() {
                Ok(cfg) => {
                    self.setup_sharing();
                    self.setup_discovery();
                    serde_json::json!({ "ok": true, "config": cfg.to_json_value() })
                }
                Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
            },
            "android_status" => {
                serde_json::json!({
                    "ok": true,
                    "session": self.controller.android_session.status(),
                    "tools": self.controller.android_tool_status(),
                    "devices": self.controller.android_list_devices(),
                    "history": self.controller.android_history(),
                })
            }
            "android_start" => {
                let serial = req.get("serial").and_then(|v| v.as_str()).unwrap_or("");
                let name = req
                    .get("device_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let codec = req.get("codec").and_then(|v| v.as_str());
                let source = req.get("source").and_then(|v| v.as_str());
                let as_mic = req.get("as_mic").and_then(|v| v.as_bool()).unwrap_or(false);
                let buffer_ms = req
                    .get("buffer_ms")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32);
                let bit_rate = req
                    .get("bit_rate")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32);
                let duplicate = req
                    .get("duplicate")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                match self.controller.android_session.start(
                    serial, name, codec, source, as_mic, buffer_ms, bit_rate, duplicate,
                ) {
                    Ok(st) => serde_json::json!({ "ok": true, "session": st }),
                    Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
                }
            }
            "android_stop" => {
                let st = self.controller.android_session.stop();
                serde_json::json!({ "ok": true, "session": st })
            }
            "stop" => {
                self.request_stop();
                serde_json::json!({ "ok": true, "stopping": true })
            }
            _ => serde_json::json!({
                "ok": false,
                "error": format!("Unknown IPC command: {cmd}")
            }),
        }
    }
}
