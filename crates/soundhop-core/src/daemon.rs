//! The SoundHop user daemon: owns the TCP audio listener, UDP/mDNS discovery,
//! `AudioEventMonitor` (`pactl subscribe`), `TelemetryMonitor`, auto-connect supervisor,
//! and local IPC control socket.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::android::{AdbMdnsBrowser, DiscoveredAdbDevice};
use crate::constants::{OFFLINE_PROBE_LIMIT, SUPERVISOR_INTERVAL_SECS, VERSION};
use crate::controller::{ConnectOptions, Controller};
use crate::discovery::{
    default_v3_caps, discover_once_with_meta, BeaconV3Meta, DiscoveryManager, Peer,
};
use crate::ipc::{is_reachable, ping, request as ipc_request, IpcServer};
use crate::pairing::PairingServer;
use crate::util::{
    command_exists, ensure_dir, is_private_address, lock_or_recover, now_secs, pid_path,
    runtime_dir, socket_path, split_host_port, tcp_probe_rtt,
};

const ANDROID_AUTO_RECONNECT_MAX_ATTEMPTS: u32 = 3;

#[derive(Debug, Clone)]
pub(crate) struct AndroidReconnectTarget {
    pub serial: String,
    pub device_name: String,
    pub codec: String,
    pub source: String,
    pub as_mic: bool,
    pub buffer_ms: Option<u32>,
    pub bit_rate: Option<u32>,
    pub duplicate: bool,
    pub wifi_host: String,
    pub wifi_port: u16,
}

#[derive(Debug)]
pub(crate) struct AndroidSupervisorState {
    pub attempted_usb_switches: HashSet<String>,
    pub last_usb_scan: Option<Instant>,
    pub was_active: bool,
    pub last_session: Option<AndroidReconnectTarget>,
    pub reconnect_attempts: u32,
    pub max_reconnect_attempts: u32,
    pub next_reconnect_after: Option<Instant>,
    pub backoff_base: Duration,
}

impl Default for AndroidSupervisorState {
    fn default() -> Self {
        Self {
            attempted_usb_switches: HashSet::new(),
            last_usb_scan: None,
            was_active: false,
            last_session: None,
            reconnect_attempts: 0,
            max_reconnect_attempts: ANDROID_AUTO_RECONNECT_MAX_ATTEMPTS,
            next_reconnect_after: None,
            backoff_base: Duration::from_secs(2),
        }
    }
}

impl AndroidSupervisorState {
    fn backoff_for_attempt(&self, attempt: u32) -> Duration {
        let shift = attempt.saturating_sub(1).min(4);
        self.backoff_base.saturating_mul(1u32 << shift)
    }
}

pub struct Daemon {
    pub controller: Arc<Controller>,
    pub discovery: Mutex<Option<Arc<DiscoveryManager>>>,
    pub android_mdns: Mutex<Option<Arc<AdbMdnsBrowser>>>,
    pub pairing_server: Mutex<Option<PairingServer>>,
    pub ipc: Mutex<Option<IpcServer>>,
    ipc_path: Option<PathBuf>,
    stop_flag: Arc<AtomicBool>,
    started_at: f64,
    advertising: Arc<AtomicBool>,
    android_manually_stopped: AtomicBool,
    android_last_session: Mutex<Option<AndroidReconnectTarget>>,
    warnings: Arc<Mutex<Vec<String>>>,
    supervisor_thread: Mutex<Option<JoinHandle<()>>>,
    audio_monitor_thread: Mutex<Option<JoinHandle<()>>>,
    audio_monitor_child: Arc<Mutex<Option<std::process::Child>>>,
    disable_log: bool,
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
        Self::with_controller(controller, ipc_path)
    }

    pub fn with_controller(controller: Arc<Controller>, ipc_path: Option<PathBuf>) -> Arc<Self> {
        Self::with_controller_inner(controller, ipc_path, false)
    }

    pub fn new_for_test(
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
        Self::with_controller_for_test(controller, ipc_path)
    }

    pub fn with_controller_for_test(
        controller: Arc<Controller>,
        ipc_path: Option<PathBuf>,
    ) -> Arc<Self> {
        Self::with_controller_inner(controller, ipc_path, true)
    }

    fn with_controller_inner(
        controller: Arc<Controller>,
        ipc_path: Option<PathBuf>,
        disable_log: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            controller,
            discovery: Mutex::new(None),
            android_mdns: Mutex::new(None),
            pairing_server: Mutex::new(None),
            ipc: Mutex::new(None),
            ipc_path,
            stop_flag: Arc::new(AtomicBool::new(false)),
            started_at: now_secs(),
            advertising: Arc::new(AtomicBool::new(false)),
            android_manually_stopped: AtomicBool::new(false),
            android_last_session: Mutex::new(None),
            warnings: Arc::new(Mutex::new(Vec::new())),
            supervisor_thread: Mutex::new(None),
            audio_monitor_thread: Mutex::new(None),
            audio_monitor_child: Arc::new(Mutex::new(None)),
            disable_log,
        })
    }

    pub fn start(self: &Arc<Self>) -> Result<(), String> {
        if is_reachable(self.ipc_path.as_deref()) {
            self.try_replace_stale_daemon();
        }
        if is_reachable(self.ipc_path.as_deref()) {
            return Err("SoundHop daemon is already running".to_string());
        }
        if !self.disable_log {
            crate::util::init_daemon_log();
        }
        self.stop_flag.store(false, Ordering::SeqCst);
        let _ = self.controller.reconcile();
        self.setup_sharing();
        self.setup_pairing_server();
        self.setup_discovery();
        self.setup_android_mdns();

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
        *lock_or_recover(&self.ipc) = Some(ipc_server);

        self.write_pid();

        let sup_daemon = Arc::clone(self);
        *lock_or_recover(&self.supervisor_thread) = Some(thread::spawn(move || {
            sup_daemon.supervisor_loop();
        }));

        let mon_daemon = Arc::clone(self);
        *lock_or_recover(&self.audio_monitor_thread) = Some(thread::spawn(move || {
            mon_daemon.audio_monitor_loop();
        }));

        Ok(())
    }

    /// Start UDP/mDNS discovery without owning the IPC socket (used when an external
    /// `soundhop daemon` service is already running on the host).
    pub fn start_discovery_only(&self) {
        let enabled = lock_or_recover(&self.controller.config).sharing.enabled;
        self.advertising.store(enabled, Ordering::SeqCst);
        self.setup_discovery();
    }

    fn try_replace_stale_daemon(&self) {
        let Some(info) = ping(self.ipc_path.as_deref()) else {
            return;
        };
        if info.version == VERSION {
            return;
        }
        crate::util::append_log_line(
            "INFO",
            &format!(
                "Replacing stale daemon v{} (pid={:?}) with v{VERSION}",
                info.version, info.pid
            ),
        );
        let _ = ipc_request(
            "stop",
            serde_json::json!({}),
            self.ipc_path.as_deref(),
            Duration::from_secs(1),
        );
        let tracked_pid = info.pid.filter(|&pid| crate::ipc::is_soundhop_process(pid));
        #[cfg(unix)]
        if let Some(pid) = tracked_pid {
            let _ = crate::ipc::terminate_process(pid);
        }
        for attempt in 0..30 {
            let ipc_down = !is_reachable(self.ipc_path.as_deref());
            let proc_exited = tracked_pid
                .map(|pid| !crate::ipc::is_process_alive(pid))
                .unwrap_or(true);
            if ipc_down && proc_exited {
                break;
            }
            // On Windows, give graceful IPC stop 1.5s before falling back to TerminateProcess.
            #[cfg(windows)]
            if attempt == 15 {
                if let Some(pid) = tracked_pid {
                    let _ = crate::ipc::terminate_process(pid);
                }
            }
            #[cfg(not(windows))]
            let _ = attempt;
            thread::sleep(Duration::from_millis(100));
        }
        if !is_reachable(self.ipc_path.as_deref()) {
            let sock = self.ipc_path.clone().unwrap_or_else(socket_path);
            let _ = fs::remove_file(&sock);
            let _ = fs::remove_file(sock.with_extension("port"));
            let _ = fs::remove_file(sock.with_extension("auth"));
        }
    }

    pub fn run_until_stopped(self: &Arc<Self>) -> Result<(), String> {
        crate::util::install_signal_handlers();
        self.start()?;
        while !self.stop_flag.load(Ordering::SeqCst) && !crate::util::signal_stop_requested() {
            thread::sleep(Duration::from_millis(40));
        }
        self.shutdown();
        Ok(())
    }

    pub fn request_stop(&self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(mut child) = lock_or_recover(&self.audio_monitor_child).take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    pub fn shutdown(&self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(mut child) = lock_or_recover(&self.audio_monitor_child).take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if self.controller.android_session.is_active() {
            let _ = self.controller.android_session.stop();
        }
        if let Some(ipc) = lock_or_recover(&self.ipc).take() {
            ipc.stop();
        }
        if let Some(srv) = lock_or_recover(&self.pairing_server).take() {
            srv.stop();
        }
        self.controller.sht3_manager.stop_listener();
        let _ = self.controller.stop_web_broadcast();
        if let Some(disc) = lock_or_recover(&self.discovery).take() {
            disc.stop();
        }
        if let Some(browser) = lock_or_recover(&self.android_mdns).take() {
            browser.stop();
        }
        if let Some(h) = lock_or_recover(&self.supervisor_thread).take() {
            let _ = h.join();
        }
        if let Some(h) = lock_or_recover(&self.audio_monitor_thread).take() {
            let _ = h.join();
        }
        let disconnect_on_quit = lock_or_recover(&self.controller.config)
            .connection
            .disconnect_on_quit;
        if disconnect_on_quit && lock_or_recover(&self.controller.state).is_connected() {
            let _ = self
                .controller
                .disconnect(false, None, false, Some(false), true, false);
        }
        let _ = fs::remove_file(pid_path());
        crate::util::append_log_line("INFO", "SoundHop daemon stopped");
    }

    fn setup_sharing(&self) {
        self.advertising.store(false, Ordering::SeqCst);
        let cfg = lock_or_recover(&self.controller.config).sharing.clone();
        if !cfg.enabled {
            let _ = self.controller.stop_listener(Some(false));
        } else {
            match self.controller.start_listener(Some(false)) {
                Ok(_) => self.advertising.store(true, Ordering::SeqCst),
                Err(e) => {
                    let msg = format!("Could not start the audio listener: {e}");
                    crate::util::append_log_line("WARN", &msg);
                    lock_or_recover(&self.warnings).push(msg);
                }
            }
        }

        if !self.controller.web_broadcast.is_active() {
            self.controller.cleanup_stale_web_adb_reverse();
        }
        if cfg.web_enabled && !self.controller.web_broadcast.is_active() {
            match self.controller.start_web_broadcast(None, None, None) {
                Ok(st) => {
                    if cfg.web_adb_reverse {
                        let _ =
                            self.controller
                                .web_adb_reverse(None, Some(st.http_port), true, false);
                    }
                }
                Err(e) => {
                    let msg = format!("Could not start Web Broadcast: {e}");
                    crate::util::append_log_line("WARN", &msg);
                    lock_or_recover(&self.warnings).push(msg);
                }
            }
        }
    }

    fn sync_web_broadcast_on_config_change(
        &self,
        old_cfg: &crate::config::SharingConfig,
        new_cfg: &crate::config::SharingConfig,
    ) {
        if !self.controller.web_broadcast.is_active() {
            return;
        }
        if old_cfg.web_enabled && !new_cfg.web_enabled {
            let _ = self.controller.stop_web_broadcast();
            return;
        }
        let needs_rebind = old_cfg.web_port != new_cfg.web_port
            || old_cfg.webrtc_port != new_cfg.webrtc_port
            || old_cfg.web_require_token != new_cfg.web_require_token
            || old_cfg.web_keep_local != new_cfg.web_keep_local
            || old_cfg.web_max_clients != new_cfg.web_max_clients;
        if !needs_rebind {
            return;
        }
        let prev_serials = self.controller.web_broadcast_status().adb_reversed_serials;
        let _ = self.controller.stop_web_broadcast();
        match self.controller.start_web_broadcast(None, None, None) {
            Ok(st) => {
                for serial in prev_serials {
                    let _ = self.controller.web_adb_reverse(
                        Some(&serial),
                        Some(st.http_port),
                        true,
                        false,
                    );
                }
            }
            Err(e) => {
                let msg = format!("Could not rebind Web Broadcast after config change: {e}");
                crate::util::append_log_line("WARN", &msg);
                lock_or_recover(&self.warnings).push(msg);
            }
        }
    }

    fn setup_pairing_server(&self) {
        if let Some(old) = lock_or_recover(&self.pairing_server).take() {
            old.stop();
        }
        let cfg = lock_or_recover(&self.controller.config).clone();
        let identity = match self.controller.device_identity() {
            Ok(id) => id,
            Err(e) => {
                let msg = format!("Could not load device identity for Pairing v2: {e}");
                crate::util::append_log_line("WARN", &msg);
                lock_or_recover(&self.warnings).push(msg);
                return;
            }
        };
        let manager = self.controller.pairing_manager();
        let ctrl_weak = Arc::downgrade(&self.controller);
        manager.set_on_paired(move |peer| {
            if let Some(ctrl) = ctrl_weak.upgrade() {
                lock_or_recover(&ctrl.state).upsert_trusted_peer(peer);
                ctrl.save();
            }
        });
        match PairingServer::start_on_port(
            cfg.sharing.pair_port,
            identity,
            cfg.device.name.clone(),
            manager,
        ) {
            Ok(srv) => {
                *lock_or_recover(&self.pairing_server) = Some(srv);
            }
            Err(e) => {
                let msg = format!(
                    "Pairing v2 server unavailable on port {}: {e}",
                    cfg.sharing.pair_port
                );
                crate::util::append_log_line("WARN", &msg);
                lock_or_recover(&self.warnings).push(msg);
            }
        }
    }

    fn setup_discovery(&self) {
        if let Some(old) = lock_or_recover(&self.discovery).take() {
            old.stop();
        }
        let cfg = lock_or_recover(&self.controller.config).clone();
        let psk = if cfg.sharing.psk_hash.is_empty() {
            None
        } else {
            Some(cfg.sharing.psk_hash.clone())
        };
        let pub_fp = self
            .controller
            .device_identity()
            .ok()
            .map(|id| id.fingerprint());
        let v3_meta = BeaconV3Meta {
            v3_port: Some(cfg.sharing.v3_port),
            pair_port: Some(cfg.sharing.pair_port),
            pub_fp,
            caps: default_v3_caps(),
        };
        let disc = Arc::new(
            DiscoveryManager::new(
                cfg.device.id.clone(),
                cfg.device.name.clone(),
                cfg.sharing.port,
                cfg.discovery.port,
                cfg.sharing.security.clone(),
                psk,
                cfg.discovery.interval_s,
                cfg.discovery.udp,
                cfg.discovery.mdns,
            )
            .with_v3_meta(v3_meta),
        );
        let ctrl_clone = Arc::clone(&self.controller);
        let disc_store = Arc::clone(&disc.store);
        disc.store.set_on_peer(move |peer| {
            if let Some(meta) = disc_store.v3_meta(&peer.id) {
                ctrl_clone.remember_peer_v3_meta(&peer.id, meta);
            }
            ctrl_clone.remember_peer(peer);
        });
        let warnings = disc.start(self.advertising.load(Ordering::SeqCst));
        for w in &warnings {
            crate::util::append_log_line("WARN", &format!("Discovery: {w}"));
        }
        lock_or_recover(&self.warnings).extend(warnings);
        *lock_or_recover(&self.discovery) = Some(disc);
    }

    fn setup_android_mdns(&self) {
        let enabled = lock_or_recover(&self.controller.config)
            .android
            .mdns_discovery;
        if !enabled {
            if let Some(old) = lock_or_recover(&self.android_mdns).take() {
                old.stop();
            }
            return;
        }
        let mut guard = lock_or_recover(&self.android_mdns);
        if guard.is_some() {
            return;
        }
        let browser = Arc::new(AdbMdnsBrowser::new());
        if let Err(err) = browser.start() {
            let msg = format!("Android ADB mDNS discovery unavailable: {err}");
            crate::util::append_log_line("WARN", &msg);
            lock_or_recover(&self.warnings).push(msg);
            return;
        }
        *guard = Some(browser);
    }

    pub fn android_mdns_devices(&self) -> Vec<DiscoveredAdbDevice> {
        let enabled = lock_or_recover(&self.controller.config)
            .android
            .mdns_discovery;
        if !enabled {
            return Vec::new();
        }
        let mut merged: BTreeMap<String, DiscoveredAdbDevice> = BTreeMap::new();
        if let Some(browser) = lock_or_recover(&self.android_mdns).as_ref() {
            for dev in browser.devices() {
                merged.insert(dev.endpoint(), dev);
            }
        }
        for dev in self
            .controller
            .android_manager
            .discover_mdns_devices(true, Duration::ZERO)
        {
            merged.insert(dev.endpoint(), dev);
        }
        merged.into_values().collect()
    }

    fn write_pid(&self) {
        let p = pid_path();
        if let Some(parent) = p.parent() {
            let _ = ensure_dir(parent);
        }
        let _ = fs::write(&p, format!("{}\n", std::process::id()));
        lock_or_recover(&self.controller.state).daemon_last_started = now_secs();
        self.controller.save();
    }

    fn should_stop(&self) -> bool {
        self.stop_flag.load(Ordering::Relaxed) || crate::util::signal_stop_requested()
    }

    /// Sleeps up to `total` in `slice` increments, returning `true` early if stop is requested.
    fn sleep_interruptible(&self, total: Duration, slice: Duration) -> bool {
        let deadline = Instant::now() + total;
        while Instant::now() < deadline {
            if self.should_stop() {
                return true;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            thread::sleep(remaining.min(slice));
        }
        self.should_stop()
    }

    fn supervisor_loop(&self) {
        let mut offline_count = 0u32;
        let mut last_reconcile = Instant::now();
        let mut android_tracker = AndroidSupervisorState::default();
        while !self.should_stop() {
            if self.sleep_interruptible(
                Duration::from_secs_f64(SUPERVISOR_INTERVAL_SECS),
                Duration::from_millis(50),
            ) {
                break;
            }
            if last_reconcile.elapsed() >= Duration::from_secs(10) {
                let _ = self.controller.reconcile();
                last_reconcile = Instant::now();
            } else {
                self.controller.reload_from_disk();
            }

            let st = lock_or_recover(&self.controller.state).clone();
            let cfg = lock_or_recover(&self.controller.config).clone();

            if st.is_connected() {
                if st.connected_peer == "web-broadcast" {
                    offline_count = 0;
                } else if self.controller.has_dead_sht3_session() {
                    let _ = self.controller.teardown_dead_sht3_session();
                    offline_count = 0;
                } else {
                    let sht3_active = self.controller.sht3_outbound_active();
                    if sht3_active {
                        self.controller.sync_sht3_telemetry();
                    }
                    if sht3_active && self.controller.is_sht3_peer_active(&st.connected_peer) {
                        offline_count = 0;
                    } else if let Some(rtt) = tcp_probe_rtt(
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
                }
            } else if cfg.connection.auto_connect
                && !st.auto_paused
                && !cfg.connection.last_peer.is_empty()
            {
                let target_id = &cfg.connection.last_peer;
                let peer_opt = lock_or_recover(&self.discovery)
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

            self.run_android_supervisor_tick(&mut android_tracker);
        }
    }

    pub(crate) fn run_android_supervisor_tick(&self, tracker: &mut AndroidSupervisorState) {
        let cfg = lock_or_recover(&self.controller.config).clone();

        // 1. Auto-switch newly connected USB Android devices to ADB TCP/IP Wi-Fi mode,
        // and prune any tracked Web Broadcast `adb reverse` serials whose device disappeared.
        const ANDROID_USB_SCAN_INTERVAL: Duration = Duration::from_secs(8);
        let mut scanned_devices: Option<Vec<crate::android::AdbDevice>> = None;
        if cfg.android.auto_switch_wifi {
            if tracker
                .last_usb_scan
                .is_none_or(|t| t.elapsed() >= ANDROID_USB_SCAN_INTERVAL)
            {
                tracker.last_usb_scan = Some(Instant::now());
                let devices = self.controller.android_manager.list_devices();
                let live_serials: HashSet<&str> =
                    devices.iter().map(|d| d.serial.as_str()).collect();
                tracker
                    .attempted_usb_switches
                    .retain(|s| live_serials.contains(s.as_str()));

                for dev in &devices {
                    if dev.is_wifi || !dev.is_ready() {
                        continue;
                    }
                    if tracker.attempted_usb_switches.contains(&dev.serial) {
                        continue;
                    }
                    tracker.attempted_usb_switches.insert(dev.serial.clone());
                    match self
                        .controller
                        .android_manager
                        .auto_switch_usb_to_wifi(dev, true, 5555)
                    {
                        Ok(Some(outcome)) => {
                            let now = now_secs();
                            {
                                let mut st = lock_or_recover(&self.controller.state);
                                st.remember_android_device(
                                    &outcome.wifi_serial,
                                    &outcome.model,
                                    &outcome.manufacturer,
                                    &outcome.ip,
                                    outcome.port,
                                    true,
                                    true,
                                    Some(now),
                                );
                            }
                            self.controller.save();
                            crate::util::append_log_line(
                                "INFO",
                                &format!(
                                    "Auto-switched Android USB device {} to Wi-Fi {}",
                                    outcome.usb_serial, outcome.wifi_serial
                                ),
                            );
                        }
                        Ok(None) => {}
                        Err(err) => {
                            crate::util::append_log_line(
                                "WARN",
                                &format!("Auto Wi-Fi switch skipped for {}: {err}", dev.serial),
                            );
                        }
                    }
                }
                scanned_devices = Some(devices);
            }
        } else {
            tracker.attempted_usb_switches.clear();
            tracker.last_usb_scan = None;
        }

        self.prune_web_adb_reverse_serials(&cfg, scanned_devices.as_deref());

        // 2. Bounded auto-reconnect on Android audio session drop.
        if self.controller.android_session.is_active() {
            let sess = self.controller.android_session.status();
            let serial = sess["serial"].as_str().unwrap_or("").to_string();
            let device_name = sess["device_name"].as_str().unwrap_or("").to_string();
            let codec = sess["codec"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or(&cfg.android.default_codec)
                .to_string();
            let source = sess["source"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or(&cfg.android.default_source)
                .to_string();
            let as_mic = sess["as_mic"].as_bool().unwrap_or(false);
            let (wifi_host, wifi_port) = split_wifi_serial(&serial);
            let target = AndroidReconnectTarget {
                serial,
                device_name,
                codec,
                source,
                as_mic,
                buffer_ms: Some(cfg.android.buffer_ms),
                bit_rate: Some(cfg.android.bit_rate),
                duplicate: cfg.android.duplicate_audio,
                wifi_host,
                wifi_port,
            };
            tracker.was_active = true;
            tracker.reconnect_attempts = 0;
            tracker.next_reconnect_after = None;
            tracker.last_session = Some(target.clone());
            *lock_or_recover(&self.android_last_session) = Some(target);
            self.android_manually_stopped.store(false, Ordering::SeqCst);
            return;
        }

        if !cfg.android.auto_reconnect || self.android_manually_stopped.load(Ordering::SeqCst) {
            tracker.reconnect_attempts = 0;
            tracker.next_reconnect_after = None;
            return;
        }

        if tracker.reconnect_attempts >= tracker.max_reconnect_attempts {
            return;
        }
        if tracker
            .next_reconnect_after
            .is_some_and(|deadline| Instant::now() < deadline)
        {
            return;
        }

        let target_opt = self.resolve_android_reconnect_target(tracker, &cfg);
        let Some(target) = target_opt else {
            return;
        };

        tracker.reconnect_attempts += 1;
        if !target.wifi_host.is_empty() {
            let _ = self
                .controller
                .android_manager
                .connect_wifi(&target.wifi_host, target.wifi_port);
        }

        match self.controller.android_session.start(
            &target.serial,
            &target.device_name,
            Some(&target.codec),
            Some(&target.source),
            target.as_mic,
            target.buffer_ms,
            target.bit_rate,
            target.duplicate,
        ) {
            Ok(_) => {
                tracker.reconnect_attempts = 0;
                tracker.next_reconnect_after = None;
                tracker.was_active = true;
                tracker.last_session = Some(target.clone());
                *lock_or_recover(&self.android_last_session) = Some(target.clone());
                let now = now_secs();
                {
                    let mut st = lock_or_recover(&self.controller.state);
                    st.android_active = true;
                    st.android_serial = target.serial.clone();
                    st.android_model = target.device_name.clone();
                    st.android_codec = target.codec.clone();
                    st.android_source = target.source.clone();
                    st.android_as_mic = target.as_mic;
                    st.android_since = now;
                    st.remember_android_device(
                        &target.serial,
                        &target.device_name,
                        "",
                        &target.wifi_host,
                        target.wifi_port,
                        !target.wifi_host.is_empty(),
                        true,
                        Some(now),
                    );
                }
                self.controller.save();
                crate::util::append_log_line(
                    "INFO",
                    &format!(
                        "Auto-reconnected Android audio session for {}",
                        target.serial
                    ),
                );
            }
            Err(err) => {
                let backoff = tracker.backoff_for_attempt(tracker.reconnect_attempts);
                tracker.next_reconnect_after = Some(Instant::now() + backoff);
                crate::util::append_log_line(
                    "WARN",
                    &format!(
                        "Android auto-reconnect attempt {}/{} for {} failed: {err}",
                        tracker.reconnect_attempts, tracker.max_reconnect_attempts, target.serial
                    ),
                );
            }
        }
    }

    fn prune_web_adb_reverse_serials(
        &self,
        cfg: &crate::config::Config,
        scanned_devices: Option<&[crate::android::AdbDevice]>,
    ) {
        let tracked_state = lock_or_recover(&self.controller.state)
            .web_adb_reverse_serials
            .clone();
        let tracked_mgr = self.controller.android_manager.tracked_reverse_tunnels();
        let need_auto_usb =
            cfg.sharing.web_adb_reverse && self.controller.web_broadcast.is_active();
        if tracked_state.is_empty() && tracked_mgr.is_empty() && !need_auto_usb {
            return;
        }

        let owned_devices;
        let devices = match scanned_devices {
            Some(list) => list,
            None => {
                owned_devices = self.controller.android_manager.list_devices();
                &owned_devices
            }
        };
        let live_serials: HashSet<&str> = devices.iter().map(|d| d.serial.as_str()).collect();

        let mut all_tracked: std::collections::BTreeSet<String> =
            tracked_state.iter().cloned().collect();
        for (s, _) in tracked_mgr {
            all_tracked.insert(s);
        }

        let mut removed_any = false;
        for serial in &all_tracked {
            if !live_serials.contains(serial.as_str()) {
                let _ = self
                    .controller
                    .android_manager
                    .remove_reverse_tunnel(serial, None);
                removed_any = true;
            }
        }
        if removed_any {
            let updated = {
                let mut st = lock_or_recover(&self.controller.state);
                st.web_adb_reverse_serials
                    .retain(|s| live_serials.contains(s.as_str()));
                st.web_adb_reverse_serials.clone()
            };
            self.controller
                .web_broadcast
                .set_adb_reversed_serials(updated);
            self.controller.save();
        }

        if need_auto_usb {
            let active_port = self.controller.web_broadcast_status().http_port;
            if active_port > 0 {
                let current_reversed = lock_or_recover(&self.controller.state)
                    .web_adb_reverse_serials
                    .clone();
                for dev in devices {
                    if !dev.is_wifi && dev.is_ready() && !current_reversed.contains(&dev.serial) {
                        let _ = self.controller.web_adb_reverse(
                            Some(&dev.serial),
                            Some(active_port),
                            true,
                            false,
                        );
                    }
                }
            }
        }
    }

    fn resolve_android_reconnect_target(
        &self,
        tracker: &AndroidSupervisorState,
        cfg: &crate::config::Config,
    ) -> Option<AndroidReconnectTarget> {
        if tracker.was_active {
            if let Some(target) = tracker
                .last_session
                .clone()
                .or_else(|| lock_or_recover(&self.android_last_session).clone())
            {
                return Some(target);
            }
        }

        let st = lock_or_recover(&self.controller.state).clone();
        if st.android_active && !st.android_serial.is_empty() {
            let (mut wifi_host, mut wifi_port) = split_wifi_serial(&st.android_serial);
            if wifi_host.is_empty() {
                if let Some(known) = st.known_android_devices.get(&st.android_serial) {
                    if !known.last_ip.is_empty() {
                        wifi_host = known.last_ip.clone();
                        wifi_port = known.last_port;
                    }
                }
            }
            let serial = if !wifi_host.is_empty() && !st.android_serial.contains(':') {
                format!("{wifi_host}:{wifi_port}")
            } else {
                st.android_serial.clone()
            };
            return Some(AndroidReconnectTarget {
                serial,
                device_name: st.android_model.clone(),
                codec: if st.android_codec.is_empty() {
                    cfg.android.default_codec.clone()
                } else {
                    st.android_codec.clone()
                },
                source: if st.android_source.is_empty() {
                    cfg.android.default_source.clone()
                } else {
                    st.android_source.clone()
                },
                as_mic: st.android_as_mic,
                buffer_ms: Some(cfg.android.buffer_ms),
                bit_rate: Some(cfg.android.bit_rate),
                duplicate: cfg.android.duplicate_audio,
                wifi_host,
                wifi_port,
            });
        }

        // Fall back to a known device in `android_history` marked `auto_connect` that was
        // previously connected.
        let known = st
            .known_android_devices
            .values()
            .filter(|d| d.auto_connect && d.last_connected > 0.0)
            .max_by(|a, b| {
                a.last_connected
                    .partial_cmp(&b.last_connected)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })?;
        let serial = if !known.last_ip.is_empty() {
            format!("{}:{}", known.last_ip, known.last_port)
        } else {
            known.serial.clone()
        };
        if serial.is_empty() {
            return None;
        }
        Some(AndroidReconnectTarget {
            serial,
            device_name: known.display_name(),
            codec: cfg.android.default_codec.clone(),
            source: cfg.android.default_source.clone(),
            as_mic: false,
            buffer_ms: Some(cfg.android.buffer_ms),
            bit_rate: Some(cfg.android.bit_rate),
            duplicate: cfg.android.duplicate_audio,
            wifi_host: known.last_ip.clone(),
            wifi_port: known.last_port,
        })
    }

    fn audio_monitor_loop(&self) {
        if !command_exists("pactl") {
            self.controller.backend.set_subscribe_active(false);
            while !self.should_stop() {
                if lock_or_recover(&self.controller.state).is_connected() {
                    let _ = self.controller.sync_remote_volume();
                    let _ = self.controller.enforce_routed_apps();
                }
                if self.sleep_interruptible(Duration::from_millis(500), Duration::from_millis(50)) {
                    break;
                }
            }
            return;
        }

        while !self.should_stop() {
            let mut cmd = Command::new("pactl");
            cmd.arg("subscribe")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            crate::util::close_fds_pre_exec(&mut cmd);
            crate::util::set_parent_death_signal(&mut cmd);
            let stdout_opt = {
                let mut child_guard = self
                    .audio_monitor_child
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if self.should_stop() {
                    break;
                }
                match cmd.spawn() {
                    Ok(mut child) => {
                        let out = child.stdout.take();
                        *child_guard = Some(child);
                        out
                    }
                    Err(_) => None,
                }
            };
            if stdout_opt.is_none() {
                if self.sleep_interruptible(Duration::from_secs(3), Duration::from_millis(50)) {
                    break;
                }
                continue;
            }

            if let Some(stdout) = stdout_opt {
                self.controller.backend.set_subscribe_active(true);
                let reader = BufReader::new(stdout);
                let mut last_app_sync: Option<Instant> = None;
                let mut last_vol_sync: Option<Instant> = None;
                for line in reader.lines().map_while(Result::ok) {
                    if self.should_stop() {
                        break;
                    }
                    self.controller.backend.invalidate_on_event(&line);
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
                self.controller.backend.set_subscribe_active(false);
            }

            if let Some(mut child) = lock_or_recover(&self.audio_monitor_child).take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            if !self.should_stop()
                && self.sleep_interruptible(Duration::from_secs(1), Duration::from_millis(50))
            {
                break;
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
        let cfg = lock_or_recover(&self.controller.config).clone();
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

        let live_peers = {
            let guard = lock_or_recover(&self.discovery);
            if let Some(d) = guard.as_ref() {
                let peers = d.peers();
                for p in &peers {
                    if let Some(meta) = d.peer_v3_meta(&p.id) {
                        self.controller.remember_peer_v3_meta(&p.id, meta);
                    }
                }
                peers
            } else {
                Vec::new()
            }
        };

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
        let matched_known_id = {
            let st = self
                .controller
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            st.known_peers
                .values()
                .find(|kp| kp.name.eq_ignore_ascii_case(q) || kp.host.eq_ignore_ascii_case(q))
                .map(|kp| kp.id.clone())
        };
        if let Some(id) = matched_known_id {
            return self
                .controller
                .known_peer(&id)
                .ok_or_else(|| format!("Unknown device: {q}"));
        }
        let psk = (!cfg.sharing.psk_hash.is_empty()).then_some(cfg.sharing.psk_hash.as_str());
        for (found, meta) in discover_once_with_meta(0.8, cfg.discovery.port, &cfg.device.id, psk) {
            if let Some(m) = meta {
                self.controller.remember_peer_v3_meta(&found.id, m);
            }
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
                let peers = lock_or_recover(&self.discovery)
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
                            "v3_active": self.controller.is_v3_active(),
                            "legacy_pa_disabled": self.controller.is_legacy_pa_disabled(),
                            "trusted_peers_count": self.controller.trusted_peers().len(),
                            "warnings": *lock_or_recover(&self.warnings),
                            "runtime_dir": runtime_dir().to_string_lossy(),
                        }),
                    );
                }
                st
            }
            "peers" | "discover" => {
                let timeout = req
                    .get("timeout")
                    .and_then(|v| v.as_f64())
                    .filter(|t| t.is_finite() && *t > 0.0)
                    .map(|t| t.clamp(0.1, 15.0));
                let disc_opt = self
                    .discovery
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                if let Some(disc) = disc_opt {
                    disc.probe();
                    if let Some(t) = timeout {
                        thread::sleep(Duration::from_secs_f64(t));
                    }
                    let list: Vec<Value> = disc.peers().into_iter().map(|p| p.to_json()).collect();
                    serde_json::json!({ "ok": true, "peers": list })
                } else if let Some(t) = timeout {
                    let cfg = self
                        .controller
                        .config
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone();
                    let psk =
                        (!cfg.sharing.psk_hash.is_empty()).then_some(cfg.sharing.psk_hash.as_str());
                    let found_with_meta =
                        discover_once_with_meta(t, cfg.discovery.port, &cfg.device.id, psk);
                    let mut list = Vec::with_capacity(found_with_meta.len());
                    for (p, meta) in found_with_meta {
                        if let Some(m) = meta {
                            self.controller.remember_peer_v3_meta(&p.id, m);
                        }
                        self.controller.remember_peer(&p);
                        list.push(p.to_json());
                    }
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
                let latency_ms = req
                    .get("latency_ms")
                    .and_then(|v| v.as_u64())
                    .and_then(|v| u32::try_from(v).ok())
                    .filter(|&v| v > 0);
                let remember = req
                    .get("remember")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                if let Some(lat) = latency_ms {
                    self.controller
                        .config
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .connection
                        .latency_ms = Some(lat);
                }

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
                        Ok(res) => {
                            if remember {
                                if let Some(first) = resolved.first() {
                                    self.controller
                                        .config
                                        .lock()
                                        .unwrap_or_else(|e| e.into_inner())
                                        .connection
                                        .last_peer = first.id.clone();
                                    self.controller.remember_peer(first);
                                    self.controller.save();
                                }
                            }
                            serde_json::json!({ "ok": true, "connection": res })
                        }
                        Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
                    }
                } else {
                    match self.resolve_peer(peer_arg, host, port, name) {
                        Ok(peer) => match self.controller.connect(&peer, opts) {
                            Ok(res) => {
                                if remember {
                                    self.controller
                                        .config
                                        .lock()
                                        .unwrap_or_else(|e| e.into_inner())
                                        .connection
                                        .last_peer = peer.id.clone();
                                    self.controller.remember_peer(&peer);
                                    self.controller.save();
                                }
                                serde_json::json!({ "ok": true, "connection": res })
                            }
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
                if req.get("clear").and_then(|v| v.as_bool()).unwrap_or(false) {
                    let res = self.controller.clear_pairing();
                    self.setup_sharing();
                    self.setup_discovery();
                    return res;
                }
                let peer = req.get("peer").and_then(|v| v.as_str()).unwrap_or("");
                let secret = req.get("secret").and_then(|v| v.as_str());
                match self.controller.pair_peer(peer, secret) {
                    Ok(res) => {
                        self.setup_sharing();
                        self.setup_discovery();
                        res
                    }
                    Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
                }
            }
            "pair_request" => {
                let explicit_host = req
                    .get("host")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let peer_query = req
                    .get("peer")
                    .or_else(|| req.get("query"))
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let pair_port = req
                    .get("pair_port")
                    .or_else(|| req.get("port"))
                    .and_then(|v| v.as_u64())
                    .and_then(|p| u16::try_from(p).ok())
                    .filter(|&p| p > 0);
                let code = req.get("code").and_then(|v| v.as_str());

                let resolved_host = if let Some(h) = explicit_host {
                    Ok(h.to_string())
                } else if let Some(q) = peer_query {
                    self.resolve_peer(Some(q), None, None, None).map(|p| p.host)
                } else {
                    Err("Specify host or peer for pair_request".to_string())
                };
                match resolved_host {
                    Ok(host) => match self.controller.pair_request(&host, pair_port, code) {
                        Ok(res) => res,
                        Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
                    },
                    Err(e) => serde_json::json!({ "ok": false, "error": e }),
                }
            }
            "pair_pending" => {
                let requests = self.controller.pending_pair_requests();
                serde_json::json!({
                    "ok": true,
                    "requests": requests,
                })
            }
            "pair_approve" => {
                let id = req
                    .get("request_id")
                    .or_else(|| req.get("id"))
                    .or_else(|| req.get("code"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                match self.controller.approve_pair_request(id) {
                    Ok(res) => res,
                    Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
                }
            }
            "pair_deny" => {
                let id = req
                    .get("request_id")
                    .or_else(|| req.get("id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                match self.controller.deny_pair_request(id) {
                    Ok(res) => res,
                    Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
                }
            }
            "pair_revoke" => {
                let device_id = req
                    .get("device_id")
                    .or_else(|| req.get("peer_id"))
                    .or_else(|| req.get("id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                match self.controller.revoke_trusted_peer(device_id) {
                    Ok(res) => res,
                    Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
                }
            }
            "pair_list" => {
                let peers: Vec<Value> = self
                    .controller
                    .trusted_peers()
                    .iter()
                    .map(|p| p.to_public_json())
                    .collect();
                serde_json::json!({
                    "ok": true,
                    "trusted_peers": peers,
                })
            }
            "pair_code" => self.controller.generate_headless_pair_code(),
            "share" => {
                let enabled = req.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
                let port_changed = if let Some(p) = req
                    .get("port")
                    .and_then(|v| v.as_u64())
                    .and_then(|p| u16::try_from(p).ok())
                    .filter(|&p| p > 0)
                {
                    let mut cfg = self
                        .controller
                        .config
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    let changed = cfg.sharing.port != p;
                    cfg.sharing.port = p;
                    changed
                } else {
                    false
                };
                match self.controller.share(enabled, None) {
                    Ok(mut res) => {
                        self.advertising.store(enabled, Ordering::SeqCst);
                        if port_changed {
                            self.setup_discovery();
                        } else if let Some(disc) = lock_or_recover(&self.discovery).as_ref() {
                            disc.set_advertising(enabled);
                        }
                        if let Some(obj) = res.as_object_mut() {
                            obj.entry("ok".to_string()).or_insert(Value::Bool(true));
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
            "unroute" => {
                let all = req.get("all").and_then(|v| v.as_bool()).unwrap_or(false);
                let app = if all {
                    None
                } else {
                    req.get("app").and_then(|v| v.as_str())
                };
                let remove_rule = req
                    .get("remove_rule")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                match self.controller.unroute_app(app, remove_rule) {
                    Ok(res) => res,
                    Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
                }
            }
            "firewall_status" => {
                let cfg = lock_or_recover(&self.controller.config).clone();
                let st = crate::firewall::detect_for_config(&cfg);
                serde_json::json!({
                    "ok": true,
                    "kind": st.kind,
                    "active": st.active,
                    "detail": st.detail,
                    "ports_allowed": st.ports_allowed,
                })
            }
            "firewall_open" => {
                let cfg = self
                    .controller
                    .config
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                let res = crate::firewall::open_ports_for_config_non_interactive(&cfg, false);
                res.to_json()
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
                        let st = lock_or_recover(&self.controller.state);
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
                        let enabled = lock_or_recover(&self.controller.config)
                            .connection
                            .auto_connect;
                        let paused = lock_or_recover(&self.controller.state).auto_paused;
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
                    let old_sharing = lock_or_recover(&self.controller.config).sharing.clone();
                    let res = lock_or_recover(&self.controller.config).apply_patch(patch);
                    if let Err(e) = res {
                        return serde_json::json!({ "ok": false, "error": e.to_string() });
                    }
                    let new_sharing = lock_or_recover(&self.controller.config).sharing.clone();
                    self.controller.save();
                    self.sync_web_broadcast_on_config_change(&old_sharing, &new_sharing);
                    self.setup_sharing();
                    self.setup_discovery();
                    self.setup_android_mdns();
                }
                let cfg_json = lock_or_recover(&self.controller.config).to_json_value();
                serde_json::json!({ "ok": true, "config": cfg_json })
            }
            "reload" => {
                let old_sharing = lock_or_recover(&self.controller.config).sharing.clone();
                match self.controller.reload_config() {
                    Ok(cfg) => {
                        self.sync_web_broadcast_on_config_change(&old_sharing, &cfg.sharing);
                        self.setup_sharing();
                        self.setup_discovery();
                        self.setup_android_mdns();
                        serde_json::json!({ "ok": true, "config": cfg.to_json_value() })
                    }
                    Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
                }
            }
            "android_status" => {
                serde_json::json!({
                    "ok": true,
                    "session": self.controller.android_session.status(),
                    "tools": self.controller.android_tool_status(),
                    "devices": self.controller.android_list_devices(),
                    "mdns_devices": self.android_mdns_devices(),
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
                    Ok(st) => {
                        self.android_manually_stopped.store(false, Ordering::SeqCst);
                        let (wifi_host, wifi_port) = split_wifi_serial(serial);
                        let cfg_android = lock_or_recover(&self.controller.config).android.clone();
                        let eff_codec = codec
                            .map(str::to_string)
                            .unwrap_or(cfg_android.default_codec);
                        let eff_source = source
                            .map(str::to_string)
                            .unwrap_or(cfg_android.default_source);
                        *lock_or_recover(&self.android_last_session) =
                            Some(AndroidReconnectTarget {
                                serial: serial.to_string(),
                                device_name: name.to_string(),
                                codec: eff_codec.clone(),
                                source: eff_source.clone(),
                                as_mic,
                                buffer_ms,
                                bit_rate,
                                duplicate,
                                wifi_host: wifi_host.clone(),
                                wifi_port,
                            });
                        let now = now_secs();
                        {
                            let mut state = lock_or_recover(&self.controller.state);
                            state.android_active = true;
                            state.android_serial = serial.to_string();
                            state.android_model = name.to_string();
                            state.android_codec = eff_codec;
                            state.android_source = eff_source;
                            state.android_as_mic = as_mic;
                            state.android_since = now;
                            state.remember_android_device(
                                serial,
                                name,
                                "",
                                &wifi_host,
                                wifi_port,
                                !wifi_host.is_empty(),
                                true,
                                Some(now),
                            );
                        }
                        self.controller.save();
                        serde_json::json!({ "ok": true, "session": st })
                    }
                    Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
                }
            }
            "android_stop" => {
                self.android_manually_stopped.store(true, Ordering::SeqCst);
                *lock_or_recover(&self.android_last_session) = None;
                let st = self.controller.android_session.stop();
                {
                    let mut state = lock_or_recover(&self.controller.state);
                    state.android_active = false;
                }
                self.controller.save();
                serde_json::json!({ "ok": true, "session": st })
            }
            "web_start" | "broadcast_start" => {
                let port = req
                    .get("port")
                    .and_then(|v| v.as_u64())
                    .and_then(|p| u16::try_from(p).ok());
                let keep_local = req.get("keep_local").and_then(|v| v.as_bool());
                let require_token = req.get("require_token").and_then(|v| v.as_bool());
                let adb_reverse_opt = req.get("adb_reverse").and_then(|v| v.as_bool());
                let serial = req
                    .get("serial")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let open_browser = req
                    .get("open_browser")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                match self
                    .controller
                    .start_web_broadcast(port, keep_local, require_token)
                {
                    Ok(mut status) => {
                        if adb_reverse_opt == Some(true) {
                            match self.controller.web_adb_reverse(
                                serial,
                                Some(status.http_port),
                                true,
                                open_browser,
                            ) {
                                Ok(next) => status = next,
                                Err(e) => {
                                    let _ = self.controller.stop_web_broadcast();
                                    return serde_json::json!({
                                        "ok": false,
                                        "error": e.to_string(),
                                    });
                                }
                            }
                        } else if adb_reverse_opt.is_none()
                            && lock_or_recover(&self.controller.config)
                                .sharing
                                .web_adb_reverse
                        {
                            if let Ok(next) = self.controller.web_adb_reverse(
                                serial,
                                Some(status.http_port),
                                true,
                                open_browser,
                            ) {
                                status = next;
                            }
                        }
                        serde_json::json!({ "ok": true, "web_broadcast": status })
                    }
                    Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
                }
            }
            "web_stop" | "broadcast_stop" => match self.controller.stop_web_broadcast() {
                Ok(status) => serde_json::json!({ "ok": true, "web_broadcast": status }),
                Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
            },
            "web_status" | "broadcast_status" => serde_json::json!({
                "ok": true,
                "web_broadcast": self.controller.web_broadcast_status(),
            }),
            "web_rotate_token" => match self.controller.rotate_web_token() {
                Ok(status) => serde_json::json!({
                    "ok": true,
                    "token": status.token,
                    "urls": status.urls,
                    "web_broadcast": status,
                }),
                Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
            },
            "web_kick" => {
                let client_id = req
                    .get("client_id")
                    .or_else(|| req.get("id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                match self.controller.kick_web_client(client_id) {
                    Ok(kicked) => serde_json::json!({
                        "ok": true,
                        "kicked": kicked,
                        "web_broadcast": self.controller.web_broadcast_status(),
                    }),
                    Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
                }
            }
            "web_adb_reverse" => {
                let serial = req
                    .get("serial")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let port = req
                    .get("port")
                    .and_then(|v| v.as_u64())
                    .and_then(|p| u16::try_from(p).ok())
                    .filter(|&p| p > 0);
                let enable = req.get("enable").and_then(|v| v.as_bool()).unwrap_or(true);
                let open_browser = req
                    .get("open_browser")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                match self
                    .controller
                    .web_adb_reverse(serial, port, enable, open_browser)
                {
                    Ok(status) => serde_json::json!({ "ok": true, "web_broadcast": status }),
                    Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
                }
            }
            "logs" => {
                let limit = req
                    .get("lines")
                    .or_else(|| req.get("limit"))
                    .and_then(|v| v.as_u64())
                    .map(|n| (n as usize).clamp(1, 2000))
                    .unwrap_or(200);
                let lines = self.collect_recent_logs(limit);
                serde_json::json!({
                    "ok": true,
                    "lines": lines,
                })
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

    fn collect_recent_logs(&self, max_lines: usize) -> Vec<String> {
        let mut lines = Vec::new();
        let path = crate::util::log_path();
        if let Ok(raw) = fs::read_to_string(&path) {
            for line in raw.lines() {
                if !line.trim().is_empty() {
                    lines.push(line.to_string());
                }
            }
        }
        {
            let warnings = self.warnings.lock().unwrap_or_else(|e| e.into_inner());
            for w in warnings.iter() {
                lines.push(format!("[daemon] {w}"));
            }
        }
        let android_st = self.controller.android_session.status();
        if let Some(arr) = android_st.get("recent_logs").and_then(|v| v.as_array()) {
            for item in arr {
                if let Some(s) = item.as_str() {
                    lines.push(s.to_string());
                }
            }
        }
        if lines.len() > max_lines {
            let drain_count = lines.len() - max_lines;
            lines.drain(0..drain_count);
        }
        lines
    }
}

fn split_wifi_serial(serial: &str) -> (String, u16) {
    if let Some((host, port_str)) = serial.rsplit_once(':') {
        let clean_host = host.trim_matches(|c| c == '[' || c == ']');
        if !clean_host.is_empty() {
            let port = port_str.parse::<u16>().unwrap_or(5555);
            return (clean_host.to_string(), port);
        }
    }
    (String::new(), 5555)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_try_replace_stale_daemon_stops_old_version() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("daemon.sock");
        let cfg_path = dir.path().join("config.toml");
        let state_path = dir.path().join("state.json");
        let stop_called = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop_called);
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();

        let old_server = Arc::new(IpcServer::new(
            Arc::new(move |req| {
                let cmd = req.get("cmd").and_then(|v| v.as_str()).unwrap_or("");
                match cmd {
                    "ping" => serde_json::json!({
                        "ok": true,
                        "pong": true,
                        "version": "2.0.4",
                    }),
                    "stop" => {
                        stop_flag.store(true, Ordering::SeqCst);
                        let _ = stop_tx.send(());
                        serde_json::json!({ "ok": true, "stopping": true })
                    }
                    _ => serde_json::json!({ "ok": true }),
                }
            }),
            Some(sock.clone()),
        ));
        old_server.start().unwrap();

        let stopper = Arc::clone(&old_server);
        let stopper_thread = thread::spawn(move || {
            let _ = stop_rx.recv_timeout(Duration::from_secs(10));
            stopper.stop();
        });

        let daemon = Daemon::new(Some(cfg_path), Some(state_path), false, Some(sock.clone()));
        daemon.try_replace_stale_daemon();
        let _ = stopper_thread.join();

        assert!(stop_called.load(Ordering::SeqCst));
        assert!(!is_reachable(Some(&sock)));
    }

    #[test]
    fn test_resolve_peer_by_known_name_and_host_does_not_deadlock() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");
        let state_path = dir.path().join("state.json");
        let daemon = Daemon::new(Some(cfg_path), Some(state_path), false, None);
        daemon.controller.set_save_enabled(false);

        daemon.controller.state.lock().unwrap().known_peers.insert(
            "peer-living-room".to_string(),
            crate::state::KnownPeer {
                id: "peer-living-room".to_string(),
                name: "Living Room".to_string(),
                host: "192.168.1.50".to_string(),
                port: 4713,
                last_seen: now_secs(),
                ..Default::default()
            },
        );

        let by_name = daemon
            .resolve_peer(Some("living room"), None, None, None)
            .expect("resolve_peer by known name should succeed without deadlocking");
        assert_eq!(by_name.id, "peer-living-room");
        assert_eq!(by_name.host, "192.168.1.50");

        let by_host = daemon
            .resolve_peer(Some("192.168.1.50"), None, None, None)
            .expect("resolve_peer by known host should succeed without deadlocking");
        assert_eq!(by_host.id, "peer-living-room");
    }

    #[test]
    fn test_ipc_logs_discover_and_connect_share_options() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");
        let state_path = dir.path().join("state.json");
        let daemon = Daemon::new(Some(cfg_path), Some(state_path), false, None);
        daemon.controller.set_save_enabled(false);

        // "logs" returns {"ok": true, "lines": [...]} including daemon warnings.
        daemon
            .warnings
            .lock()
            .unwrap()
            .push("test warning line".to_string());
        let logs_resp = daemon.handle_ipc(serde_json::json!({ "cmd": "logs", "lines": 50 }));
        assert_eq!(logs_resp["ok"], true);
        let lines = logs_resp["lines"].as_array().unwrap();
        assert!(lines
            .iter()
            .any(|l| l.as_str().is_some_and(|s| s.contains("test warning line"))));

        // "discover" works both without and with optional "timeout".
        let disc_default = daemon.handle_ipc(serde_json::json!({ "cmd": "discover" }));
        assert_eq!(disc_default["ok"], true);
        assert!(disc_default["peers"].is_array());

        let disc_timeout =
            daemon.handle_ipc(serde_json::json!({ "cmd": "discover", "timeout": 0.1 }));
        assert_eq!(disc_timeout["ok"], true);
        assert!(disc_timeout["peers"].is_array());

        // "share" applies optional "port" to config (even if backend is unavailable in test env).
        let _ = daemon.handle_ipc(serde_json::json!({
            "cmd": "share",
            "enabled": false,
            "port": 4899,
        }));
        assert_eq!(daemon.controller.config.lock().unwrap().sharing.port, 4899);

        // "connect" applies optional "latency_ms" to config.
        let _ = daemon.handle_ipc(serde_json::json!({
            "cmd": "connect",
            "host": "127.0.0.1",
            "port": 1,
            "latency_ms": 45,
            "remember": true,
        }));
        assert_eq!(
            daemon
                .controller
                .config
                .lock()
                .unwrap()
                .connection
                .latency_ms,
            Some(45)
        );
    }

    #[test]
    fn test_daemon_android_auto_switch_wifi_and_mdns_gating() {
        use crate::android::AndroidManager;
        use crate::backend::linux::LinuxAudioBackend;
        use crate::util::{CommandResult, MockRunner};

        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");
        let state_path = dir.path().join("state.json");
        let fake_adb = dir.path().join("adb");
        fs::write(&fake_adb, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&fake_adb, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let adb_calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let adb_calls_c = Arc::clone(&adb_calls);
        let runner = Arc::new(MockRunner::new(move |args, _| {
            let joined = args.join(" ");
            adb_calls_c.lock().unwrap().push(joined.clone());
            if joined.contains("devices -l") {
                return Ok(CommandResult {
                    args: args.to_vec(),
                    returncode: 0,
                    stdout: "List of devices attached\nUSB12345\tdevice product:shiba model:Pixel_8 device:shiba transport_id:1\n".to_string(),
                    stderr: String::new(),
                });
            }
            if joined.contains("ip -f inet addr show wlan0") {
                return Ok(CommandResult {
                    args: args.to_vec(),
                    returncode: 0,
                    stdout: "    inet 192.168.1.91/24 scope global wlan0\n".to_string(),
                    stderr: String::new(),
                });
            }
            if joined.contains("tcpip 5555") {
                return Ok(CommandResult {
                    args: args.to_vec(),
                    returncode: 0,
                    stdout: "restarting in TCP mode port: 5555\n".to_string(),
                    stderr: String::new(),
                });
            }
            if joined.contains("connect 192.168.1.91:5555") {
                return Ok(CommandResult {
                    args: args.to_vec(),
                    returncode: 0,
                    stdout: "connected to 192.168.1.91:5555\n".to_string(),
                    stderr: String::new(),
                });
            }
            if joined.contains("mdns services") {
                return Ok(CommandResult {
                    args: args.to_vec(),
                    returncode: 0,
                    stdout: "List of discovered mdns services\nadb-pixel\t_adb-tls-connect._tcp.\t192.168.1.91:38455\n".to_string(),
                    stderr: String::new(),
                });
            }
            Ok(CommandResult {
                args: args.to_vec(),
                returncode: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }));

        let backend = Arc::new(LinuxAudioBackend::with_runner(Arc::clone(&runner) as _));
        let android_mgr = Arc::new(AndroidManager::with_runner(
            "",
            &fake_adb.to_string_lossy(),
            runner,
        ));
        let mut controller = Controller::new(
            None,
            None,
            Some(backend),
            Some(cfg_path),
            Some(state_path),
            false,
        );
        controller.android_manager = Arc::clone(&android_mgr);
        let controller = Arc::new(controller);
        controller.set_save_enabled(false);
        let daemon = Daemon::with_controller(Arc::clone(&controller), None);

        let mut tracker = AndroidSupervisorState::default();

        // 1. auto_switch_wifi = false -> does NOT switch USB device or populate history.
        {
            let mut cfg = lock_or_recover(&daemon.controller.config);
            cfg.android.auto_switch_wifi = false;
            cfg.android.auto_reconnect = false;
            cfg.android.mdns_discovery = false;
        }
        daemon.run_android_supervisor_tick(&mut tracker);
        assert!(daemon.controller.android_history().is_empty());
        assert!(daemon.android_mdns_devices().is_empty());

        // 2. auto_switch_wifi = true -> switches USB device to Wi-Fi and records in android_history.
        {
            let mut cfg = lock_or_recover(&daemon.controller.config);
            cfg.android.auto_switch_wifi = true;
            cfg.android.mdns_discovery = true;
        }
        daemon.run_android_supervisor_tick(&mut tracker);
        let history = daemon.controller.android_history();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0]["serial"], "192.168.1.91:5555");
        assert_eq!(history[0]["is_wifi"], true);

        // 3. mdns_discovery = true -> exposes discovered wireless Android endpoints.
        let mdns_devs = daemon.android_mdns_devices();
        assert_eq!(mdns_devs.len(), 1);
        assert_eq!(mdns_devs[0].endpoint(), "192.168.1.91:38455");
        assert!(mdns_devs[0].tls);
    }

    #[test]
    fn test_daemon_android_auto_reconnect_bounded_and_respects_flag_and_manual_stop() {
        use crate::android::{AndroidCaptureSession, AndroidManager};
        use crate::backend::linux::LinuxAudioBackend;
        use crate::util::{CommandResult, MockRunner};

        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");
        let state_path = dir.path().join("state.json");
        let fake_adb = dir.path().join("adb");
        fs::write(&fake_adb, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&fake_adb, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let connect_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let connect_calls_c = Arc::clone(&connect_calls);
        let runner = Arc::new(MockRunner::new(move |args, _| {
            let joined = args.join(" ");
            if joined.contains("connect 192.168.1.92:5555") {
                connect_calls_c.fetch_add(1, Ordering::SeqCst);
                return Ok(CommandResult {
                    args: args.to_vec(),
                    returncode: 0,
                    stdout: "connected to 192.168.1.92:5555\n".to_string(),
                    stderr: String::new(),
                });
            }
            Ok(CommandResult {
                args: args.to_vec(),
                returncode: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }));

        let backend: Arc<dyn crate::backend::AudioBackend> =
            Arc::new(LinuxAudioBackend::with_runner(Arc::clone(&runner) as _));
        // Create an executable stub with a nonexistent shebang so `find_scrcpy()` selects it
        // (never falling back to any system `scrcpy`) while `Command::spawn` fails deterministically.
        let failing_scrcpy = dir.path().join("failing-scrcpy");
        fs::write(
            &failing_scrcpy,
            b"#!/nonexistent/soundhop-test-interpreter\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&failing_scrcpy, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let android_mgr = Arc::new(AndroidManager::with_runner(
            &failing_scrcpy.to_string_lossy(),
            &fake_adb.to_string_lossy(),
            runner,
        ));
        let android_session = Arc::new(AndroidCaptureSession::new(
            Arc::clone(&android_mgr),
            Arc::clone(&backend),
        ));
        let mut controller = Controller::new(
            None,
            None,
            Some(backend),
            Some(cfg_path),
            Some(state_path),
            false,
        );
        controller.android_manager = android_mgr;
        controller.android_session = android_session;
        let controller = Arc::new(controller);
        controller.set_save_enabled(false);

        // Seed state with a previously active/known Wi-Fi Android device in android_history.
        {
            let mut st = lock_or_recover(&controller.state);
            st.android_active = true;
            st.android_serial = "192.168.1.92:5555".to_string();
            st.android_model = "Pixel 8".to_string();
            st.remember_android_device(
                "192.168.1.92:5555",
                "Pixel 8",
                "Google",
                "192.168.1.92",
                5555,
                true,
                true,
                Some(now_secs()),
            );
        }

        let daemon = Daemon::with_controller(Arc::clone(&controller), None);
        let mut tracker = AndroidSupervisorState {
            max_reconnect_attempts: 2,
            backoff_base: Duration::ZERO,
            ..Default::default()
        };

        // 1. auto_reconnect = false -> zero reconnect attempts.
        {
            let mut cfg = lock_or_recover(&daemon.controller.config);
            cfg.android.auto_switch_wifi = false;
            cfg.android.auto_reconnect = false;
        }
        daemon.run_android_supervisor_tick(&mut tracker);
        assert_eq!(tracker.reconnect_attempts, 0);
        assert_eq!(connect_calls.load(Ordering::SeqCst), 0);

        // 2. auto_reconnect = true -> attempts bounded to max_reconnect_attempts (2).
        lock_or_recover(&daemon.controller.config)
            .android
            .auto_reconnect = true;
        daemon.run_android_supervisor_tick(&mut tracker);
        daemon.run_android_supervisor_tick(&mut tracker);
        daemon.run_android_supervisor_tick(&mut tracker);
        assert_eq!(tracker.reconnect_attempts, 2);
        assert_eq!(connect_calls.load(Ordering::SeqCst), 2);

        // 3. Manual stop resets and prevents further auto-reconnect attempts.
        tracker.reconnect_attempts = 0;
        let _ = daemon.handle_ipc(serde_json::json!({ "cmd": "android_stop" }));
        daemon.run_android_supervisor_tick(&mut tracker);
        assert_eq!(tracker.reconnect_attempts, 0);
        assert_eq!(connect_calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn test_daemon_pairing_v2_ipc_commands() {
        let dir_resp = tempfile::tempdir().unwrap();
        let dir_init = tempfile::tempdir().unwrap();
        let resp_daemon = Daemon::new(
            Some(dir_resp.path().join("config.toml")),
            Some(dir_resp.path().join("state.json")),
            false,
            None,
        );
        let init_daemon = Daemon::new(
            Some(dir_init.path().join("config.toml")),
            Some(dir_init.path().join("state.json")),
            false,
            None,
        );

        // Bind responder PairingServer on an ephemeral loopback port
        let resp_identity = resp_daemon.controller.device_identity().unwrap();
        let resp_manager = resp_daemon.controller.pairing_manager();
        let resp_ctrl_weak = Arc::downgrade(&resp_daemon.controller);
        resp_manager.set_on_paired(move |peer| {
            if let Some(ctrl) = resp_ctrl_weak.upgrade() {
                lock_or_recover(&ctrl.state).upsert_trusted_peer(peer);
                ctrl.save();
            }
        });
        let srv = PairingServer::start(
            std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 0),
            resp_identity,
            "Responder Host".to_string(),
            resp_manager,
        )
        .unwrap();
        let pair_port = srv.local_addr().port();
        *lock_or_recover(&resp_daemon.pairing_server) = Some(srv);

        // 1. Headless code flow via IPC: pair_code -> pair_request -> pair_list -> pair_revoke
        let code_resp = resp_daemon.handle_ipc(serde_json::json!({ "cmd": "pair_code" }));
        assert_eq!(code_resp["ok"], true);
        let code = code_resp["code"].as_str().unwrap().to_string();

        let req_resp = init_daemon.handle_ipc(serde_json::json!({
            "cmd": "pair_request",
            "host": "127.0.0.1",
            "pair_port": pair_port,
            "code": code,
        }));
        assert_eq!(req_resp["ok"], true);
        assert_eq!(req_resp["paired"], true);

        let list_init = init_daemon.handle_ipc(serde_json::json!({ "cmd": "pair_list" }));
        assert_eq!(list_init["ok"], true);
        let trusted = list_init["trusted_peers"].as_array().unwrap();
        assert_eq!(trusted.len(), 1);
        let resp_dev_id = trusted[0]["device_id"].as_str().unwrap().to_string();
        assert!(
            trusted[0].get("pairwise_secret").is_none(),
            "pair_list must never expose raw pairwise_secret"
        );

        let revoke_resp = init_daemon.handle_ipc(serde_json::json!({
            "cmd": "pair_revoke",
            "device_id": resp_dev_id,
        }));
        assert_eq!(revoke_resp["ok"], true);
        assert_eq!(revoke_resp["revoked"], true);
        let list_after = init_daemon.handle_ipc(serde_json::json!({ "cmd": "pair_list" }));
        assert!(list_after["trusted_peers"].as_array().unwrap().is_empty());

        // 2. Interactive deny flow via IPC: pair_request -> pair_pending -> pair_deny
        let init_clone = Arc::clone(&init_daemon);
        let client_thread = thread::spawn(move || {
            init_clone.handle_ipc(serde_json::json!({
                "cmd": "pair_request",
                "host": "127.0.0.1",
                "pair_port": pair_port,
            }))
        });

        let pending_id = loop {
            let p = resp_daemon.handle_ipc(serde_json::json!({ "cmd": "pair_pending" }));
            let arr = p["requests"].as_array().unwrap();
            if !arr.is_empty() {
                break arr[0]["request_id"].as_str().unwrap().to_string();
            }
            thread::sleep(Duration::from_millis(10));
        };
        let deny_resp = resp_daemon.handle_ipc(serde_json::json!({
            "cmd": "pair_deny",
            "request_id": pending_id,
        }));
        assert_eq!(deny_resp["ok"], true);
        assert_eq!(deny_resp["denied"], true);

        let client_res = client_thread.join().unwrap();
        assert_eq!(client_res["ok"], false);

        resp_daemon.shutdown();
    }

    #[test]
    fn test_daemon_prunes_disappeared_web_adb_reverse_serials_and_cleans_stale_on_startup() {
        use crate::android::AndroidManager;
        use crate::backend::linux::LinuxAudioBackend;
        use crate::util::{CommandResult, MockRunner};

        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("config.toml");
        let state_path = dir.path().join("state.json");
        let fake_adb = dir.path().join("adb");
        fs::write(&fake_adb, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&fake_adb, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let show_device = Arc::new(AtomicBool::new(true));
        let show_device_c = Arc::clone(&show_device);
        let removed_cmds = Arc::new(Mutex::new(Vec::<String>::new()));
        let removed_cmds_c = Arc::clone(&removed_cmds);

        let runner = Arc::new(MockRunner::new(move |args, _| {
            let joined = args.join(" ");
            if joined.contains("devices -l") {
                let stdout = if show_device_c.load(Ordering::SeqCst) {
                    "List of devices attached\nUSB999\tdevice product:shiba model:Pixel_8 device:shiba transport_id:1\n"
                        .to_string()
                } else {
                    "List of devices attached\n".to_string()
                };
                return Ok(CommandResult {
                    args: args.to_vec(),
                    returncode: 0,
                    stdout,
                    stderr: String::new(),
                });
            }
            if joined.contains("reverse --remove") {
                removed_cmds_c.lock().unwrap().push(joined);
            }
            Ok(CommandResult {
                args: args.to_vec(),
                returncode: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }));

        let backend = Arc::new(LinuxAudioBackend::with_runner(Arc::clone(&runner) as _));
        let android_mgr = Arc::new(AndroidManager::with_runner(
            "",
            &fake_adb.to_string_lossy(),
            runner,
        ));
        let mut controller = Controller::new(
            None,
            None,
            Some(backend),
            Some(cfg_path),
            Some(state_path),
            false,
        );
        controller.android_manager = android_mgr;
        let controller = Arc::new(controller);
        controller.set_save_enabled(false);
        {
            let mut cfg = lock_or_recover(&controller.config);
            cfg.sharing.enabled = false;
            cfg.sharing.web_enabled = false;
            cfg.android.auto_switch_wifi = false;
            cfg.android.auto_reconnect = false;
        }
        let daemon = Daemon::with_controller(Arc::clone(&controller), None);

        // 1. Stale web_adb_reverse_serials from an unclean shutdown are cleaned up in setup_sharing().
        {
            let mut st = lock_or_recover(&daemon.controller.state);
            st.web_adb_reverse_serials.push("STALE_USB".to_string());
        }
        daemon.setup_sharing();
        assert!(lock_or_recover(&daemon.controller.state)
            .web_adb_reverse_serials
            .is_empty());
        assert!(removed_cmds
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.contains("STALE_USB") && c.contains("reverse --remove-all")));

        // 2. Active reverse tunnel stays while USB999 is present, and is pruned when it disappears.
        removed_cmds.lock().unwrap().clear();
        daemon
            .controller
            .web_adb_reverse(Some("USB999"), Some(4717), true, false)
            .unwrap();
        assert_eq!(
            lock_or_recover(&daemon.controller.state).web_adb_reverse_serials,
            vec!["USB999".to_string()]
        );

        let mut tracker = AndroidSupervisorState::default();
        daemon.run_android_supervisor_tick(&mut tracker);
        assert_eq!(
            lock_or_recover(&daemon.controller.state).web_adb_reverse_serials,
            vec!["USB999".to_string()]
        );

        show_device.store(false, Ordering::SeqCst);
        daemon.run_android_supervisor_tick(&mut tracker);
        assert!(lock_or_recover(&daemon.controller.state)
            .web_adb_reverse_serials
            .is_empty());
        assert!(daemon
            .controller
            .android_manager
            .tracked_reverse_tunnels()
            .is_empty());
        assert!(removed_cmds
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.contains("USB999") && c.contains("reverse --remove")));
    }
}
