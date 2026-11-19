//! Non-blocking background worker and plain-language state snapshot for the Control Center GUI.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use eframe::egui;
use soundhop_core::android::{AdbDevice, ToolStatus};
use soundhop_core::config::Config;
use soundhop_core::constants::PROTOCOL_VERSION;
use soundhop_core::controller::{ConnectOptions, Controller};
use soundhop_core::discovery::{discover_once_with_psk, Peer};
use soundhop_core::firewall::{detect as detect_firewall, open_ports};
use soundhop_core::ipc::{is_running as ipc_is_running, request as ipc_request};
use soundhop_core::service::{
    install as install_service, status as service_status, uninstall as uninstall_service,
};
use soundhop_core::state::ActivePeerSession;
use soundhop_core::util::{is_own_non_loopback_ip, is_private_address, now_secs, split_host_port};

use crate::{lock_or_recover, read_or_recover, write_or_recover};

#[derive(Debug, Clone)]
pub struct PeerView {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub platform: String,
    pub security: String,
    pub rtt_ms: Option<f64>,
    pub online: bool,
    pub paired: bool,
}

impl PeerView {
    pub fn signal_label(&self) -> &'static str {
        if !self.online {
            return "Saved (offline)";
        }
        match self.rtt_ms {
            Some(r) if r < 8.0 => "Excellent signal",
            Some(r) if r < 25.0 => "Good signal",
            Some(_) => "Fair signal",
            None => "Ready to connect",
        }
    }

    pub fn platform_label(&self) -> &'static str {
        if self.platform.eq_ignore_ascii_case("windows") {
            "Windows"
        } else {
            "Linux"
        }
    }
}

#[derive(Debug, Clone)]
pub struct AppStreamView {
    pub app_name: String,
    pub media_name: String,
    pub is_remote: bool,
    pub routed_rule: String,
    pub persistent: bool,
    pub active: bool,
}

#[derive(Debug, Clone)]
pub struct GuiToast {
    pub message: String,
    pub is_error: bool,
    pub created_at: Instant,
}

#[derive(Debug, Clone)]
pub struct GuiSnapshot {
    pub daemon_connected: bool,
    pub config: Config,
    pub connected: bool,
    pub multiroom: bool,
    pub connected_peer_id: String,
    pub connected_name: String,
    pub connected_host: String,
    pub connected_port: u16,
    pub connected_duration: String,
    pub multiroom_peers: Vec<ActivePeerSession>,
    pub keep_local: bool,
    pub sharing_enabled: bool,
    pub auto_connect: bool,
    pub auto_paused: bool,
    pub volume_level: u32,
    pub volume_muted: bool,
    pub mic_enabled: bool,
    pub mic_direction: String,
    pub peers: Vec<PeerView>,
    pub apps: Vec<AppStreamView>,
    pub audio_active: bool,
    pub audio_dbfs: f32,
    pub audio_peak: f32,
    pub rtt_ms: Option<f64>,
    pub jitter_ms: Option<f64>,
    pub android_tools: ToolStatus,
    pub android_devices: Vec<AdbDevice>,
    pub android_session_active: bool,
    pub android_session_device: String,
    pub android_installing: bool,
    pub android_starting: bool,
    pub firewall_kind: String,
    pub firewall_active: bool,
    pub firewall_detail: String,
    pub service_running: bool,
    pub service_enabled: bool,
    pub autostart_installed: bool,
    pub busy_message: Option<String>,
    pub refreshing_peers: bool,
    pub refreshing_android: bool,
    pub pending_peer_id: Option<String>,
    pub show_onboarding: bool,
    pub paired_count: usize,
    pub hydrated: bool,
}

impl GuiSnapshot {
    pub fn from_controller(controller: &Controller) -> Self {
        let cfg = lock_or_recover(&controller.config).clone();
        let st = lock_or_recover(&controller.state).clone();

        Self {
            daemon_connected: false,
            show_onboarding: !cfg.ui.onboarding_completed && st.known_peers.is_empty(),
            paired_count: st.paired_peers.len(),
            config: cfg.clone(),
            connected: st.is_connected(),
            multiroom: st.is_multiroom(),
            connected_peer_id: st.connected_peer.clone(),
            connected_name: st.connected_name.clone(),
            connected_host: st.connected_host.clone(),
            connected_port: st.connected_port,
            connected_duration: String::new(),
            multiroom_peers: st.multiroom_peers.clone(),
            keep_local: st.keep_local,
            sharing_enabled: cfg.sharing.enabled || st.listener_module > 0,
            auto_connect: cfg.connection.auto_connect,
            auto_paused: st.auto_paused,
            volume_level: 100,
            volume_muted: false,
            mic_enabled: !st.connected_source.is_empty(),
            mic_direction: st.mic_direction.clone(),
            peers: Vec::new(),
            apps: Vec::new(),
            audio_active: false,
            audio_dbfs: -60.0,
            audio_peak: 0.0,
            rtt_ms: None,
            jitter_ms: None,
            android_tools: ToolStatus {
                scrcpy_path: None,
                scrcpy_version: String::new(),
                adb_path: None,
                adb_version: String::new(),
                is_managed: false,
            },
            android_devices: Vec::new(),
            android_session_active: st.android_active,
            android_session_device: st.android_model,
            android_installing: false,
            android_starting: false,
            firewall_kind: "none".to_string(),
            firewall_active: false,
            firewall_detail: String::new(),
            service_running: false,
            service_enabled: false,
            autostart_installed: false,
            busy_message: None,
            refreshing_peers: false,
            refreshing_android: false,
            pending_peer_id: None,
            hydrated: false,
        }
    }

    pub fn output_headline(&self) -> String {
        if self.multiroom && !self.multiroom_peers.is_empty() {
            format!("Playing on {} speakers", self.multiroom_peers.len())
        } else if self.connected && !self.connected_name.is_empty() {
            format!("Playing on {}", self.connected_name)
        } else {
            "Playing on This Device".to_string()
        }
    }

    pub fn output_subtitle(&self) -> String {
        if self.multiroom && !self.multiroom_peers.is_empty() {
            let names: Vec<&str> = self
                .multiroom_peers
                .iter()
                .map(|p| p.name.as_str())
                .collect();
            if self.keep_local {
                format!("{} + This device", names.join(", "))
            } else {
                names.join(", ")
            }
        } else if self.connected {
            if let Some(rtt) = self.rtt_ms {
                if rtt < 15.0 {
                    "Connected • Great signal".to_string()
                } else {
                    "Connected • Wireless audio active".to_string()
                }
            } else {
                "Connected • Streaming wirelessly".to_string()
            }
        } else if self.sharing_enabled {
            format!(
                "Local speakers active • Visible as \"{}\"",
                self.config.device.name
            )
        } else {
            "Local speakers active".to_string()
        }
    }

    pub fn is_peer_connected(&self, peer_id: &str) -> bool {
        if self.multiroom_peers.iter().any(|p| p.peer_id == peer_id) {
            return true;
        }
        self.connected && self.connected_peer_id == peer_id
    }
}

pub fn security_friendly_label(mode: &str) -> &'static str {
    match mode {
        "psk" => "PIN-Paired Devices Only",
        "cookie" => "System Audio Cookie",
        "open" => "Open (No Check)",
        _ => "Home Wi-Fi (Automatic)",
    }
}

pub enum GuiCommand {
    RefreshFast,
    RefreshPeers,
    RefreshAndroid,
    RefreshSystem,
    ConnectPeer {
        peer_id: String,
    },
    ConnectAddress {
        host_port: String,
    },
    ConnectMulti {
        peer_ids: Vec<String>,
        keep_local: bool,
    },
    Disconnect,
    SetShare {
        enabled: bool,
    },
    SetAutoConnect {
        enabled: bool,
    },
    SetVolume {
        level: u32,
    },
    SetMute {
        muted: bool,
    },
    RouteApp {
        app: String,
        target: String,
        save: bool,
    },
    UnrouteApp {
        app: String,
        remove_rule: bool,
    },
    SetDefaultRoutingMode {
        mode: String,
    },
    SetMic {
        direction: Option<String>,
    },
    SaveConfigPatch {
        patch: serde_json::Value,
        toast: Option<String>,
    },
    ApplyPairingPin {
        pin: String,
    },
    ClearPairing,
    AndroidInstallTools,
    AndroidSwitchWifi {
        serial: String,
    },
    AndroidConnectWifi {
        ip_port: String,
    },
    AndroidDisconnectWifi {
        serial: String,
    },
    AndroidStart {
        serial: String,
        device_name: String,
        codec: String,
        source: String,
        buffer_ms: u32,
        duplicate: bool,
        as_mic: bool,
    },
    AndroidStop,
    OpenFirewall,
    SetAutostart {
        enabled: bool,
    },
    CompleteOnboarding {
        device_name: String,
        enable_sharing: bool,
        enable_autostart: bool,
        open_firewall: bool,
    },
    ReopenOnboarding,
}

fn is_mutating_command(cmd: &GuiCommand) -> bool {
    !matches!(
        cmd,
        GuiCommand::RefreshFast
            | GuiCommand::RefreshPeers
            | GuiCommand::RefreshAndroid
            | GuiCommand::RefreshSystem
    )
}

pub struct GuiBackend {
    pub snapshot: Arc<RwLock<GuiSnapshot>>,
    tx: Option<mpsc::Sender<GuiCommand>>,
    toasts: Arc<Mutex<Vec<GuiToast>>>,
    ctx: Option<egui::Context>,
    mutation_seq: Arc<AtomicU64>,
    active_commands: Arc<AtomicUsize>,
    stop_flag: Arc<AtomicBool>,
    poller_wake: Arc<(Mutex<bool>, Condvar)>,
    cmd_handle: Option<JoinHandle<()>>,
    poller_handle: Option<JoinHandle<()>>,
}

impl GuiBackend {
    pub fn start(controller: Arc<Controller>, ctx: Option<egui::Context>) -> Self {
        Self::start_with_toast(controller, ctx, None)
    }

    pub fn start_with_toast(
        controller: Arc<Controller>,
        ctx: Option<egui::Context>,
        initial_toast: Option<String>,
    ) -> Self {
        let initial = GuiSnapshot::from_controller(&controller);
        let snapshot = Arc::new(RwLock::new(initial));
        let toasts = Arc::new(Mutex::new(Vec::new()));
        if let Some(msg) = initial_toast {
            lock_or_recover(&toasts).push(GuiToast {
                message: msg,
                is_error: false,
                created_at: Instant::now(),
            });
        }
        let (tx, rx) = mpsc::channel::<GuiCommand>();
        let stop_flag = Arc::new(AtomicBool::new(false));
        let onboarding_reopened = Arc::new(AtomicBool::new(false));
        let mutation_seq = Arc::new(AtomicU64::new(0));
        let active_commands = Arc::new(AtomicUsize::new(0));
        let cached_live_peers = Arc::new(Mutex::new(Vec::new()));
        let controller_sync_lock = Arc::new(Mutex::new(()));
        let poller_wake = Arc::new((Mutex::new(false), Condvar::new()));

        let cmd_worker = WorkerState {
            controller: Arc::clone(&controller),
            snapshot: Arc::clone(&snapshot),
            toasts: Arc::clone(&toasts),
            ctx: ctx.clone(),
            cached_live_peers: Arc::clone(&cached_live_peers),
            mutation_seq: Arc::clone(&mutation_seq),
            active_commands: Arc::clone(&active_commands),
            onboarding_reopened: Arc::clone(&onboarding_reopened),
            controller_sync_lock: Arc::clone(&controller_sync_lock),
            use_ipc: true,
        };

        let mut poller_worker = WorkerState {
            controller: Arc::clone(&controller),
            snapshot: Arc::clone(&snapshot),
            toasts: Arc::clone(&toasts),
            ctx: ctx.clone(),
            cached_live_peers,
            mutation_seq: Arc::clone(&mutation_seq),
            active_commands: Arc::clone(&active_commands),
            onboarding_reopened: Arc::clone(&onboarding_reopened),
            controller_sync_lock,
            use_ipc: true,
        };

        let cmd_handle = thread::spawn(move || {
            let mut worker = cmd_worker;
            while let Ok(cmd) = rx.recv() {
                let mutating = is_mutating_command(&cmd);
                worker.handle_command(cmd);
                if mutating {
                    worker.active_commands.fetch_sub(1, Ordering::SeqCst);
                }
                worker.request_repaint();
            }
        });

        let stop_poller = Arc::clone(&stop_flag);
        let wake_poller = Arc::clone(&poller_wake);
        let poller_handle = thread::spawn(move || {
            let _telemetry_monitor = if controller.is_save_enabled() {
                Some(controller.start_telemetry_monitor())
            } else {
                None
            };

            poller_worker.refresh_fast(false);
            poller_worker.refresh_peers();
            poller_worker.refresh_android();
            poller_worker.refresh_system();
            poller_worker.request_repaint();

            let mut last_fast = Instant::now();
            let mut last_peers = Instant::now();

            while !stop_poller.load(Ordering::Relaxed) {
                let (lock, cvar) = &*wake_poller;
                let guard = lock_or_recover(lock);
                if !*guard {
                    let _ = cvar.wait_timeout(guard, Duration::from_millis(650));
                }
                if stop_poller.load(Ordering::Relaxed) {
                    break;
                }

                if last_fast.elapsed() >= Duration::from_millis(650) {
                    poller_worker.refresh_fast(false);
                    poller_worker.request_repaint();
                    last_fast = Instant::now();
                }

                if last_peers.elapsed() >= Duration::from_secs(3) {
                    poller_worker.refresh_peers();
                    poller_worker.refresh_fast(false);
                    poller_worker.request_repaint();
                    last_peers = Instant::now();
                }
            }
        });

        Self {
            snapshot,
            tx: Some(tx),
            toasts,
            ctx,
            mutation_seq,
            active_commands,
            stop_flag,
            poller_wake,
            cmd_handle: Some(cmd_handle),
            poller_handle: Some(poller_handle),
        }
    }

    pub fn send(&self, cmd: GuiCommand) {
        let mutating = is_mutating_command(&cmd);
        self.apply_optimistic_update(&cmd);
        if mutating {
            self.active_commands.fetch_add(1, Ordering::SeqCst);
        }
        if let Some(ref ctx) = self.ctx {
            ctx.request_repaint();
        }
        if let Some(ref tx) = self.tx {
            if tx.send(cmd).is_err() && mutating {
                self.active_commands.fetch_sub(1, Ordering::SeqCst);
            }
        } else if mutating {
            self.active_commands.fetch_sub(1, Ordering::SeqCst);
        }
    }

    fn apply_optimistic_update(&self, cmd: &GuiCommand) {
        self.mutation_seq.fetch_add(1, Ordering::SeqCst);
        let mut snap = write_or_recover(&self.snapshot);
        match cmd {
            GuiCommand::RefreshFast | GuiCommand::RefreshSystem => {}
            GuiCommand::RefreshPeers => {
                snap.refreshing_peers = true;
            }
            GuiCommand::RefreshAndroid => {
                snap.refreshing_android = true;
            }
            GuiCommand::ConnectPeer { peer_id } => {
                snap.pending_peer_id = Some(peer_id.clone());
                snap.busy_message = Some("Connecting to speaker...".to_string());
            }
            GuiCommand::ConnectAddress { host_port } => {
                let (host, _) = split_host_port(host_port.trim(), 4713);
                if !host.is_empty() {
                    snap.busy_message = Some(format!("Connecting to {host}..."));
                }
            }
            GuiCommand::ConnectMulti { peer_ids, .. } => {
                if !peer_ids.is_empty() {
                    snap.busy_message = Some("Starting multi-room audio...".to_string());
                }
            }
            GuiCommand::Disconnect => {
                snap.busy_message = Some("Switching back to this device...".to_string());
            }
            GuiCommand::SetShare { enabled } => {
                snap.sharing_enabled = *enabled;
            }
            GuiCommand::SetAutoConnect { enabled } => {
                snap.auto_connect = *enabled;
            }
            GuiCommand::SetVolume { level } => {
                snap.volume_level = *level;
            }
            GuiCommand::SetMute { muted } => {
                snap.volume_muted = *muted;
            }
            GuiCommand::RouteApp { app, target, save } => {
                let is_remote = target == "remote";
                let mut matched = false;
                for existing in snap
                    .apps
                    .iter_mut()
                    .filter(|a| a.app_name.eq_ignore_ascii_case(app))
                {
                    matched = true;
                    existing.is_remote = is_remote;
                    existing.routed_rule = target.clone();
                    if *save {
                        existing.persistent = true;
                    }
                }
                if !matched && *save {
                    snap.apps.push(AppStreamView {
                        app_name: app.clone(),
                        media_name: String::new(),
                        is_remote,
                        routed_rule: target.clone(),
                        persistent: true,
                        active: false,
                    });
                }
            }
            GuiCommand::UnrouteApp { app, remove_rule } => {
                for existing in snap
                    .apps
                    .iter_mut()
                    .filter(|a| a.app_name.eq_ignore_ascii_case(app))
                {
                    existing.is_remote = false;
                    if *remove_rule {
                        existing.persistent = false;
                        existing.routed_rule.clear();
                    }
                }
                if *remove_rule {
                    snap.apps.retain(|a| a.active || a.persistent);
                }
            }
            GuiCommand::SetDefaultRoutingMode { mode } => {
                snap.config.routing.default_mode = mode.clone();
            }
            GuiCommand::SetMic { direction } => {
                snap.mic_enabled = direction.is_some();
                if let Some(dir) = direction {
                    snap.mic_direction = dir.clone();
                }
            }
            GuiCommand::SaveConfigPatch { patch, .. } => {
                let _ = snap.config.apply_patch(patch);
            }
            GuiCommand::ApplyPairingPin { pin } => {
                if pin.trim().len() >= 6 {
                    snap.config.sharing.security = "psk".to_string();
                }
            }
            GuiCommand::ClearPairing => {
                snap.config.sharing.psk_hash.clear();
                snap.config.sharing.security = "trusted-lan".to_string();
            }
            GuiCommand::AndroidInstallTools => {
                snap.android_installing = true;
            }
            GuiCommand::AndroidSwitchWifi { .. } => {
                snap.busy_message = Some("Switching phone to Wi-Fi...".to_string());
            }
            GuiCommand::AndroidConnectWifi { ip_port } => {
                if !ip_port.trim().is_empty() {
                    snap.busy_message = Some(format!("Connecting to {}...", ip_port.trim()));
                }
            }
            GuiCommand::AndroidDisconnectWifi { .. } => {}
            GuiCommand::AndroidStart { device_name, .. } => {
                snap.android_starting = true;
                snap.busy_message = Some(format!("Starting audio from {device_name}..."));
            }
            GuiCommand::AndroidStop => {
                snap.android_session_active = false;
            }
            GuiCommand::OpenFirewall => {
                snap.busy_message = Some("Opening firewall ports...".to_string());
            }
            GuiCommand::SetAutostart { enabled } => {
                snap.autostart_installed = *enabled;
                snap.service_enabled = *enabled;
            }
            GuiCommand::CompleteOnboarding {
                device_name,
                enable_sharing,
                enable_autostart,
                ..
            } => {
                snap.show_onboarding = false;
                snap.sharing_enabled = *enable_sharing;
                snap.autostart_installed = *enable_autostart;
                if !device_name.trim().is_empty() {
                    snap.config.device.name = device_name.trim().to_string();
                }
            }
            GuiCommand::ReopenOnboarding => {
                snap.show_onboarding = true;
            }
        }
    }

    pub fn read(&self) -> GuiSnapshot {
        read_or_recover(&self.snapshot).clone()
    }

    pub fn latest_toast(&self) -> Option<GuiToast> {
        let mut guard = lock_or_recover(&self.toasts);
        guard.retain(|t| t.created_at.elapsed() < Duration::from_secs(4));
        guard.last().cloned()
    }

    pub fn dismiss_toast(&self) {
        lock_or_recover(&self.toasts).clear();
    }
}

impl Drop for GuiBackend {
    fn drop(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        self.tx.take();
        {
            let (lock, cvar) = &*self.poller_wake;
            let mut stopped = lock_or_recover(lock);
            *stopped = true;
            cvar.notify_all();
        }
        // Detach worker threads on drop instead of blocking the UI thread on
        // in-flight IPC/network/install operations (e.g. 15s firewall or scrcpy download).
        self.cmd_handle.take();
        self.poller_handle.take();
    }
}

struct WorkerState {
    controller: Arc<Controller>,
    snapshot: Arc<RwLock<GuiSnapshot>>,
    toasts: Arc<Mutex<Vec<GuiToast>>>,
    ctx: Option<egui::Context>,
    cached_live_peers: Arc<Mutex<Vec<Peer>>>,
    mutation_seq: Arc<AtomicU64>,
    active_commands: Arc<AtomicUsize>,
    onboarding_reopened: Arc<AtomicBool>,
    controller_sync_lock: Arc<Mutex<()>>,
    use_ipc: bool,
}

impl WorkerState {
    fn request_repaint(&self) {
        if let Some(ref ctx) = self.ctx {
            ctx.request_repaint();
        }
    }

    fn push_toast(&self, message: impl Into<String>, is_error: bool) {
        let mut guard = lock_or_recover(&self.toasts);
        guard.push(GuiToast {
            message: message.into(),
            is_error,
            created_at: Instant::now(),
        });
        if guard.len() > 3 {
            let drain = guard.len() - 3;
            guard.drain(0..drain);
        }
    }

    fn set_busy(&self, msg: Option<String>) {
        write_or_recover(&self.snapshot).busy_message = msg;
        self.request_repaint();
    }

    fn handle_command(&mut self, cmd: GuiCommand) {
        match cmd {
            GuiCommand::RefreshFast => self.refresh_fast(true),
            GuiCommand::RefreshPeers => {
                self.refresh_peers();
                self.refresh_fast(true);
                write_or_recover(&self.snapshot).refreshing_peers = false;
            }
            GuiCommand::RefreshAndroid => {
                self.refresh_android();
                write_or_recover(&self.snapshot).refreshing_android = false;
            }
            GuiCommand::RefreshSystem => self.refresh_system(),
            GuiCommand::ConnectPeer { peer_id } => {
                self.set_busy(Some("Connecting to speaker...".to_string()));
                let res = if ipc_is_running(None) {
                    ipc_request(
                        "connect",
                        serde_json::json!({ "peer": peer_id }),
                        None,
                        Duration::from_secs(10),
                    )
                    .map_err(|e| e.to_string())
                    .and_then(|v| {
                        if v.get("ok").and_then(|x| x.as_bool()).unwrap_or(true) {
                            Ok(v)
                        } else {
                            Err(v
                                .get("error")
                                .and_then(|x| x.as_str())
                                .unwrap_or("Could not connect")
                                .to_string())
                        }
                    })
                } else if let Some(peer) = self.find_peer_for_id(&peer_id) {
                    let _sync = lock_or_recover(&self.controller_sync_lock);
                    self.controller
                        .connect(
                            &peer,
                            ConnectOptions {
                                notify_user: Some(true),
                                by: "gui".to_string(),
                                ..Default::default()
                            },
                        )
                        .map_err(|e| e.to_string())
                } else {
                    Err("Speaker is not currently reachable.".to_string())
                };
                write_or_recover(&self.snapshot).pending_peer_id = None;
                self.set_busy(None);
                match res {
                    Ok(_) => {
                        self.push_toast("Connected to remote speaker.", false);
                        self.refresh_fast(true);
                    }
                    Err(e) => {
                        self.push_toast(e, true);
                        self.refresh_fast(true);
                    }
                }
            }
            GuiCommand::ConnectAddress { host_port } => {
                let (host, port) = split_host_port(host_port.trim(), 4713);
                if host.is_empty() {
                    self.set_busy(None);
                    self.push_toast("Enter an IP address or hostname first.", true);
                    return;
                }
                self.set_busy(Some(format!("Connecting to {host}...")));
                let res = if ipc_is_running(None) {
                    ipc_request(
                        "connect",
                        serde_json::json!({ "host": host, "port": port }),
                        None,
                        Duration::from_secs(10),
                    )
                    .map_err(|e| e.to_string())
                    .and_then(|v| {
                        if v.get("ok").and_then(|x| x.as_bool()).unwrap_or(true) {
                            Ok(v)
                        } else {
                            Err(v
                                .get("error")
                                .and_then(|x| x.as_str())
                                .unwrap_or("Could not connect")
                                .to_string())
                        }
                    })
                } else {
                    let peer = Peer {
                        id: format!("manual-{host}-{port}"),
                        name: host.clone(),
                        host: host.clone(),
                        port,
                        version: String::new(),
                        protocol: PROTOCOL_VERSION,
                        security: "trusted-lan".to_string(),
                        platform: "linux".to_string(),
                        last_seen: now_secs(),
                        source: "manual".to_string(),
                        rtt_ms: None,
                    };
                    let _sync = lock_or_recover(&self.controller_sync_lock);
                    self.controller
                        .connect(
                            &peer,
                            ConnectOptions {
                                notify_user: Some(true),
                                by: "gui".to_string(),
                                ..Default::default()
                            },
                        )
                        .map_err(|e| e.to_string())
                };
                self.set_busy(None);
                match res {
                    Ok(_) => {
                        self.push_toast(format!("Connected to {host}."), false);
                        self.refresh_fast(true);
                        self.refresh_peers();
                    }
                    Err(e) => {
                        self.push_toast(e, true);
                        self.refresh_fast(true);
                    }
                }
            }
            GuiCommand::ConnectMulti {
                peer_ids,
                keep_local,
            } => {
                if peer_ids.is_empty() {
                    self.set_busy(None);
                    self.push_toast("Select at least one speaker.", true);
                    return;
                }
                self.set_busy(Some("Starting multi-room audio...".to_string()));
                let res = if ipc_is_running(None) {
                    ipc_request(
                        "connect",
                        serde_json::json!({
                            "peers": peer_ids,
                            "keep_local": keep_local,
                        }),
                        None,
                        Duration::from_secs(12),
                    )
                    .map_err(|e| e.to_string())
                    .and_then(|v| {
                        if v.get("ok").and_then(|x| x.as_bool()).unwrap_or(true) {
                            Ok(v)
                        } else {
                            Err(v
                                .get("error")
                                .and_then(|x| x.as_str())
                                .unwrap_or("Multi-room connection failed")
                                .to_string())
                        }
                    })
                } else {
                    let targets: Vec<Peer> = peer_ids
                        .iter()
                        .filter_map(|id| self.find_peer_for_id(id))
                        .collect();
                    let _sync = lock_or_recover(&self.controller_sync_lock);
                    self.controller
                        .connect_multi(
                            &targets,
                            ConnectOptions {
                                keep_local,
                                notify_user: Some(true),
                                by: "gui".to_string(),
                                ..Default::default()
                            },
                        )
                        .map_err(|e| e.to_string())
                };
                self.set_busy(None);
                match res {
                    Ok(_) => {
                        self.push_toast("Multi-room speakers connected.", false);
                        self.refresh_fast(true);
                    }
                    Err(e) => {
                        self.push_toast(e, true);
                        self.refresh_fast(true);
                    }
                }
            }
            GuiCommand::Disconnect => {
                self.set_busy(Some("Switching back to this device...".to_string()));
                let res = if ipc_is_running(None) {
                    ipc_request(
                        "disconnect",
                        serde_json::json!({}),
                        None,
                        Duration::from_secs(6),
                    )
                    .map(|_| ())
                    .map_err(|e| e.to_string())
                } else {
                    let _sync = lock_or_recover(&self.controller_sync_lock);
                    self.controller
                        .disconnect(false, None, true, Some(true), false, false)
                        .map(|_| ())
                        .map_err(|e| e.to_string())
                };
                self.set_busy(None);
                match res {
                    Ok(_) => {
                        self.push_toast("Audio returned to this device.", false);
                        self.refresh_fast(true);
                    }
                    Err(e) => {
                        self.push_toast(e, true);
                        self.refresh_fast(true);
                    }
                }
            }
            GuiCommand::SetShare { enabled } => {
                let res = if ipc_is_running(None) {
                    ipc_request(
                        "share",
                        serde_json::json!({ "enabled": enabled }),
                        None,
                        Duration::from_secs(5),
                    )
                    .map(|_| ())
                    .map_err(|e| e.to_string())
                } else {
                    let _sync = lock_or_recover(&self.controller_sync_lock);
                    self.controller
                        .share(enabled, Some(true))
                        .map(|_| ())
                        .map_err(|e| e.to_string())
                };
                match res {
                    Ok(_) => {
                        self.push_toast(
                            if enabled {
                                "Others can now play audio on your speakers."
                            } else {
                                "Speaker sharing turned off."
                            },
                            false,
                        );
                        self.refresh_fast(true);
                    }
                    Err(e) => {
                        self.push_toast(e, true);
                        self.refresh_fast(true);
                    }
                }
            }
            GuiCommand::SetAutoConnect { enabled } => {
                let res = if ipc_is_running(None) {
                    ipc_request(
                        "auto",
                        serde_json::json!({ "action": if enabled { "on" } else { "off" } }),
                        None,
                        Duration::from_secs(3),
                    )
                    .map_err(|e| e.to_string())
                    .and_then(|v| {
                        if v.get("ok").and_then(|x| x.as_bool()).unwrap_or(true) {
                            Ok(())
                        } else {
                            Err(v
                                .get("error")
                                .and_then(|x| x.as_str())
                                .unwrap_or("Failed to update auto-connect")
                                .to_string())
                        }
                    })
                } else {
                    let _sync = lock_or_recover(&self.controller_sync_lock);
                    let v = self.controller.set_auto(enabled);
                    if v.get("ok").and_then(|x| x.as_bool()).unwrap_or(true) {
                        Ok(())
                    } else {
                        Err(v
                            .get("error")
                            .and_then(|x| x.as_str())
                            .unwrap_or("Failed to update auto-connect")
                            .to_string())
                    }
                };
                if let Err(e) = res {
                    self.push_toast(format!("Failed to update auto-connect: {e}"), true);
                }
                self.refresh_fast(true);
            }
            GuiCommand::SetVolume { level } => {
                let res = if ipc_is_running(None) {
                    ipc_request(
                        "volume",
                        serde_json::json!({ "volume": level }),
                        None,
                        Duration::from_secs(2),
                    )
                    .map_err(|e| e.to_string())
                    .and_then(|v| {
                        if v.get("ok").and_then(|x| x.as_bool()).unwrap_or(true) {
                            Ok(())
                        } else {
                            Err(v
                                .get("error")
                                .and_then(|x| x.as_str())
                                .unwrap_or("Failed to set volume")
                                .to_string())
                        }
                    })
                } else {
                    let _sync = lock_or_recover(&self.controller_sync_lock);
                    let v = self.controller.set_volume(Some(level), None, true);
                    if v.get("ok").and_then(|x| x.as_bool()).unwrap_or(true) {
                        Ok(())
                    } else {
                        Err(v
                            .get("error")
                            .and_then(|x| x.as_str())
                            .unwrap_or("Failed to set volume")
                            .to_string())
                    }
                };
                match res {
                    Ok(_) => {
                        write_or_recover(&self.snapshot).volume_level = level;
                    }
                    Err(e) => {
                        self.push_toast(format!("Failed to set volume: {e}"), true);
                        self.refresh_fast(true);
                    }
                }
            }
            GuiCommand::SetMute { muted } => {
                let res = if ipc_is_running(None) {
                    ipc_request(
                        "volume",
                        serde_json::json!({ "mute": muted }),
                        None,
                        Duration::from_secs(2),
                    )
                    .map_err(|e| e.to_string())
                    .and_then(|v| {
                        if v.get("ok").and_then(|x| x.as_bool()).unwrap_or(true) {
                            Ok(())
                        } else {
                            Err(v
                                .get("error")
                                .and_then(|x| x.as_str())
                                .unwrap_or("Failed to update mute")
                                .to_string())
                        }
                    })
                } else {
                    let _sync = lock_or_recover(&self.controller_sync_lock);
                    let v = self.controller.set_volume(None, Some(muted), true);
                    if v.get("ok").and_then(|x| x.as_bool()).unwrap_or(true) {
                        Ok(())
                    } else {
                        Err(v
                            .get("error")
                            .and_then(|x| x.as_str())
                            .unwrap_or("Failed to update mute")
                            .to_string())
                    }
                };
                match res {
                    Ok(_) => {
                        write_or_recover(&self.snapshot).volume_muted = muted;
                    }
                    Err(e) => {
                        self.push_toast(format!("Failed to update mute: {e}"), true);
                        self.refresh_fast(true);
                    }
                }
            }
            GuiCommand::RouteApp { app, target, save } => {
                let res = if ipc_is_running(None) {
                    ipc_request(
                        "route",
                        serde_json::json!({ "app": app, "target": target, "save": save }),
                        None,
                        Duration::from_secs(4),
                    )
                    .map_err(|e| e.to_string())
                } else {
                    let _sync = lock_or_recover(&self.controller_sync_lock);
                    self.controller
                        .route_app(&app, Some(&target), save)
                        .map_err(|e| e.to_string())
                };
                match res {
                    Ok(_) => self.refresh_fast(true),
                    Err(e) => {
                        self.push_toast(e, true);
                        self.refresh_fast(true);
                    }
                }
            }
            GuiCommand::UnrouteApp { app, remove_rule } => {
                let res = if ipc_is_running(None) {
                    ipc_request(
                        "unroute",
                        serde_json::json!({ "app": app, "remove_rule": remove_rule }),
                        None,
                        Duration::from_secs(4),
                    )
                    .map_err(|e| e.to_string())
                } else {
                    let _sync = lock_or_recover(&self.controller_sync_lock);
                    self.controller
                        .unroute_app(Some(&app), remove_rule)
                        .map_err(|e| e.to_string())
                };
                match res {
                    Ok(_) => {
                        if remove_rule {
                            self.push_toast("Removed saved app rule.", false);
                        }
                        self.refresh_fast(true);
                    }
                    Err(e) => {
                        self.push_toast(e, true);
                        self.refresh_fast(true);
                    }
                }
            }
            GuiCommand::SetDefaultRoutingMode { mode } => {
                self.apply_config_patch(
                    serde_json::json!({ "routing": { "default_mode": mode } }),
                    Some("Updated default app routing.".to_string()),
                );
            }
            GuiCommand::SetMic { direction } => {
                let res = if ipc_is_running(None) {
                    match direction.as_deref() {
                        Some(dir) => ipc_request(
                            "mic",
                            serde_json::json!({ "action": "enable", "direction": dir }),
                            None,
                            Duration::from_secs(5),
                        ),
                        None => ipc_request(
                            "mic",
                            serde_json::json!({ "action": "off" }),
                            None,
                            Duration::from_secs(5),
                        ),
                    }
                    .map_err(|e| e.to_string())
                    .and_then(|v| {
                        if v.get("ok").and_then(|x| x.as_bool()).unwrap_or(true) {
                            Ok(v)
                        } else {
                            Err(v
                                .get("error")
                                .and_then(|x| x.as_str())
                                .unwrap_or("Microphone update failed")
                                .to_string())
                        }
                    })
                } else {
                    let _sync = lock_or_recover(&self.controller_sync_lock);
                    match direction.as_deref() {
                        Some(dir) => self.controller.enable_mic(dir).map_err(|e| e.to_string()),
                        None => self.controller.disable_mic().map_err(|e| e.to_string()),
                    }
                };
                match res {
                    Ok(_) => {
                        let msg = match direction.as_deref() {
                            Some("send") => "Sending your microphone to the connected speaker.",
                            Some("receive") => "Listening to the remote device's microphone.",
                            _ => "Microphone sharing turned off.",
                        };
                        self.push_toast(msg, false);
                        self.refresh_fast(true);
                    }
                    Err(e) => {
                        self.push_toast(e, true);
                        self.refresh_fast(true);
                    }
                }
            }
            GuiCommand::SaveConfigPatch { patch, toast } => {
                self.apply_config_patch(patch, toast);
            }
            GuiCommand::ApplyPairingPin { pin } => {
                let trimmed = pin.trim().to_string();
                if trimmed.len() < 6 {
                    self.push_toast("Enter a code with at least 6 characters.", true);
                    self.refresh_fast(true);
                    return;
                }
                let res = if ipc_is_running(None) {
                    ipc_request(
                        "pair",
                        serde_json::json!({ "peer": "lan-group", "secret": trimmed }),
                        None,
                        Duration::from_secs(5),
                    )
                    .map_err(|e| e.to_string())
                } else {
                    let _sync = lock_or_recover(&self.controller_sync_lock);
                    self.controller
                        .pair_peer("lan-group", Some(&trimmed))
                        .map_err(|e| e.to_string())
                };
                match res {
                    Ok(_) => {
                        self.push_toast(
                            "Devices paired! Enter the same code on your other device.",
                            false,
                        );
                        self.refresh_fast(true);
                    }
                    Err(e) => {
                        self.push_toast(e, true);
                        self.refresh_fast(true);
                    }
                }
            }
            GuiCommand::ClearPairing => {
                let res = if ipc_is_running(None) {
                    ipc_request(
                        "pair",
                        serde_json::json!({ "clear": true }),
                        None,
                        Duration::from_secs(5),
                    )
                    .map(|_| ())
                    .map_err(|e| e.to_string())
                } else {
                    let _sync = lock_or_recover(&self.controller_sync_lock);
                    self.controller.clear_pairing();
                    Ok(())
                };
                match res {
                    Ok(_) => {
                        self.push_toast(
                            "Pairing cleared. Switched back to Home Wi-Fi mode.",
                            false,
                        );
                        self.refresh_fast(true);
                    }
                    Err(e) => {
                        self.push_toast(e, true);
                        self.refresh_fast(true);
                    }
                }
            }
            GuiCommand::AndroidInstallTools => {
                write_or_recover(&self.snapshot).android_installing = true;
                self.request_repaint();
                let res = self.controller.android_manager.install_scrcpy(None, None);
                write_or_recover(&self.snapshot).android_installing = false;
                match res {
                    Ok(_) => {
                        self.push_toast("Audio helper installed and ready!", false);
                        self.refresh_android();
                    }
                    Err(e) => self.push_toast(format!("Setup failed: {e}"), true),
                }
            }
            GuiCommand::AndroidSwitchWifi { serial } => {
                self.set_busy(Some("Switching phone to Wi-Fi...".to_string()));
                let mgr = &self.controller.android_manager;
                if let Some(ip) = mgr.get_device_wifi_ip(&serial) {
                    let (_, msg1) = mgr.enable_tcpip(&serial, 5555);
                    let (ok2, msg2) = mgr.connect_wifi(&ip, 5555);
                    self.set_busy(None);
                    self.push_toast(
                        if ok2 {
                            format!("Wireless ready at {ip} — you can unplug USB now.")
                        } else {
                            format!("{msg1} {msg2}")
                        },
                        !ok2,
                    );
                    self.refresh_android();
                } else {
                    self.set_busy(None);
                    self.push_toast(
                        "Couldn't find your phone's Wi-Fi IP. Make sure it's on the same Wi-Fi.",
                        true,
                    );
                }
            }
            GuiCommand::AndroidConnectWifi { ip_port } => {
                let target = ip_port.trim();
                if target.is_empty() {
                    self.set_busy(None);
                    self.push_toast("Enter your phone's Wi-Fi IP address.", true);
                    return;
                }
                self.set_busy(Some(format!("Connecting to {target}...")));
                let (ok, msg) = self.controller.android_manager.connect_wifi(target, 5555);
                self.set_busy(None);
                self.push_toast(msg, !ok);
                self.refresh_android();
            }
            GuiCommand::AndroidDisconnectWifi { serial } => {
                let (ok, msg) = self
                    .controller
                    .android_manager
                    .disconnect_wifi(Some(&serial));
                self.push_toast(msg, !ok);
                self.refresh_android();
            }
            GuiCommand::AndroidStart {
                serial,
                device_name,
                codec,
                source,
                buffer_ms,
                duplicate,
                as_mic,
            } => {
                write_or_recover(&self.snapshot).android_starting = true;
                self.set_busy(Some(format!("Starting audio from {device_name}...")));
                let res = if ipc_is_running(None) {
                    ipc_request(
                        "android_start",
                        serde_json::json!({
                            "serial": serial,
                            "device_name": device_name,
                            "codec": codec,
                            "source": source,
                            "as_mic": as_mic,
                            "buffer_ms": buffer_ms,
                            "bit_rate": 128_000,
                            "duplicate": duplicate,
                        }),
                        None,
                        Duration::from_secs(10),
                    )
                    .map_err(|e| e.to_string())
                    .and_then(|v| {
                        if v.get("ok").and_then(|x| x.as_bool()).unwrap_or(true) {
                            Ok(v)
                        } else {
                            Err(v
                                .get("error")
                                .and_then(|x| x.as_str())
                                .unwrap_or("Failed to start phone audio")
                                .to_string())
                        }
                    })
                } else {
                    self.controller
                        .android_session
                        .start(
                            &serial,
                            &device_name,
                            Some(&codec),
                            Some(&source),
                            as_mic,
                            Some(buffer_ms),
                            Some(128_000),
                            duplicate,
                        )
                        .map_err(|e| e.to_string())
                };
                write_or_recover(&self.snapshot).android_starting = false;
                self.set_busy(None);
                match res {
                    Ok(_) => {
                        self.push_toast(format!("Listening to {device_name}."), false);
                        self.refresh_fast(true);
                    }
                    Err(e) => {
                        self.push_toast(e, true);
                        self.refresh_fast(true);
                    }
                }
            }
            GuiCommand::AndroidStop => {
                if ipc_is_running(None) {
                    let _ = ipc_request(
                        "android_stop",
                        serde_json::json!({}),
                        None,
                        Duration::from_secs(5),
                    );
                }
                let _ = self.controller.android_session.stop();
                self.push_toast("Stopped phone audio.", false);
                self.refresh_fast(true);
            }
            GuiCommand::OpenFirewall => {
                self.set_busy(Some("Opening firewall ports...".to_string()));
                let ok = if ipc_is_running(None) {
                    ipc_request(
                        "firewall_open",
                        serde_json::json!({}),
                        None,
                        Duration::from_secs(15),
                    )
                    .map(|v| v.get("ok").and_then(|x| x.as_bool()).unwrap_or(false))
                    .unwrap_or(false)
                } else {
                    let cfg = lock_or_recover(&self.controller.config).clone();
                    open_ports(
                        cfg.sharing.port,
                        cfg.discovery.port,
                        false,
                        cfg.discovery.mdns,
                    )
                    .ok
                };
                self.set_busy(None);
                if ok {
                    self.push_toast("Firewall rules updated for SoundHop.", false);
                } else {
                    self.push_toast(
                        "Couldn't update firewall automatically. Try running 'soundhop firewall open' in a terminal.",
                        true,
                    );
                }
                self.refresh_system();
            }
            GuiCommand::SetAutostart { enabled } => {
                if enabled {
                    let svc_res = install_service(true, true, true);
                    let ok = svc_res
                        .get("enabled")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                        || !svc_res
                            .get("tray_autostart")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .is_empty();
                    if ok {
                        self.apply_config_patch(
                            serde_json::json!({ "ui": { "start_tray": true } }),
                            Some("SoundHop will now start quietly when you log in.".to_string()),
                        );
                    } else {
                        self.push_toast("Couldn't enable login startup automatically.", true);
                    }
                } else {
                    let _ = uninstall_service();
                    let st = service_status();
                    let still_on = st.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false)
                        || st
                            .get("tray_autostart")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                    if !still_on {
                        self.apply_config_patch(
                            serde_json::json!({ "ui": { "start_tray": false } }),
                            Some("Removed SoundHop from login startup.".to_string()),
                        );
                    } else {
                        self.push_toast("Couldn't completely remove login startup.", true);
                    }
                }
                self.refresh_system();
            }
            GuiCommand::CompleteOnboarding {
                device_name,
                enable_sharing,
                enable_autostart,
                open_firewall,
            } => {
                let clean_name = device_name.trim().to_string();
                let mut ui_patch = serde_json::json!({
                    "ui": {
                        "onboarding_completed": true,
                        "start_tray": enable_autostart,
                    },
                    "sharing": {
                        "enabled": enable_sharing,
                    }
                });
                if !clean_name.is_empty() {
                    ui_patch["device"] = serde_json::json!({ "name": clean_name });
                }
                self.apply_config_patch(ui_patch, None);

                let mut failures: Vec<&str> = Vec::new();
                let share_ok = if ipc_is_running(None) {
                    ipc_request(
                        "share",
                        serde_json::json!({ "enabled": enable_sharing }),
                        None,
                        Duration::from_secs(4),
                    )
                    .map(|v| v.get("ok").and_then(|x| x.as_bool()).unwrap_or(true))
                    .unwrap_or(false)
                } else {
                    let _sync = lock_or_recover(&self.controller_sync_lock);
                    self.controller.share(enable_sharing, Some(true)).is_ok()
                };
                if !share_ok {
                    failures.push("speaker sharing");
                }

                if enable_autostart {
                    let svc_res = install_service(true, true, true);
                    let svc_ok = svc_res
                        .get("enabled")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                        || !svc_res
                            .get("tray_autostart")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .is_empty();
                    if !svc_ok {
                        failures.push("login startup");
                    }
                }
                if open_firewall {
                    let fw_ok = if ipc_is_running(None) {
                        ipc_request(
                            "firewall_open",
                            serde_json::json!({}),
                            None,
                            Duration::from_secs(15),
                        )
                        .map(|v| v.get("ok").and_then(|x| x.as_bool()).unwrap_or(false))
                        .unwrap_or(false)
                    } else {
                        let cfg = lock_or_recover(&self.controller.config).clone();
                        open_ports(
                            cfg.sharing.port,
                            cfg.discovery.port,
                            false,
                            cfg.discovery.mdns,
                        )
                        .ok
                    };
                    if !fw_ok {
                        failures.push("firewall rules");
                    }
                }
                self.onboarding_reopened.store(false, Ordering::SeqCst);
                write_or_recover(&self.snapshot).show_onboarding = false;
                if failures.is_empty() {
                    self.push_toast("You're all set! SoundHop is ready.", false);
                } else {
                    self.push_toast(
                        format!(
                            "Setup finished, but failed to configure: {}.",
                            failures.join(", ")
                        ),
                        true,
                    );
                }
                self.refresh_fast(true);
                self.refresh_peers();
                self.refresh_system();
            }
            GuiCommand::ReopenOnboarding => {
                self.onboarding_reopened.store(true, Ordering::SeqCst);
                write_or_recover(&self.snapshot).show_onboarding = true;
            }
        }
    }

    fn apply_config_patch(&mut self, patch: serde_json::Value, toast: Option<String>) {
        if ipc_is_running(None) {
            match ipc_request(
                "config_patch",
                serde_json::json!({ "patch": patch }),
                None,
                Duration::from_secs(4),
            ) {
                Ok(resp) => {
                    if !resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(true) {
                        let err = resp
                            .get("error")
                            .and_then(|v| v.as_str())
                            .unwrap_or("Could not save setting");
                        self.push_toast(err, true);
                        self.refresh_fast(true);
                        return;
                    }
                    {
                        let _sync = lock_or_recover(&self.controller_sync_lock);
                        self.controller.reload_from_disk();
                    }
                    if let Some(msg) = toast {
                        self.push_toast(msg, false);
                    }
                    self.refresh_fast(true);
                }
                Err(e) => {
                    self.push_toast(e.to_string(), true);
                    self.refresh_fast(true);
                }
            }
        } else {
            let res = {
                let _sync = lock_or_recover(&self.controller_sync_lock);
                let r = lock_or_recover(&self.controller.config).apply_patch(&patch);
                if r.is_ok() {
                    self.controller.save();
                    let _ = self.controller.enforce_routed_apps();
                }
                r
            };
            match res {
                Ok(_) => {
                    if let Some(msg) = toast {
                        self.push_toast(msg, false);
                    }
                    self.refresh_fast(true);
                }
                Err(e) => {
                    self.push_toast(e.to_string(), true);
                    self.refresh_fast(true);
                }
            }
        }
    }

    fn find_peer_for_id(&self, peer_id: &str) -> Option<Peer> {
        if let Some(p) = lock_or_recover(&self.cached_live_peers)
            .iter()
            .find(|p| p.id == peer_id)
        {
            return Some(p.clone());
        }
        self.controller.known_peer(peer_id)
    }

    fn refresh_peers(&mut self) {
        if ipc_is_running(None) {
            if let Ok(resp) = ipc_request(
                "peers",
                serde_json::json!({}),
                None,
                Duration::from_millis(800),
            ) {
                if let Some(arr) = resp.get("peers").and_then(|v| v.as_array()) {
                    let parsed: Vec<Peer> = arr
                        .iter()
                        .filter_map(|item| serde_json::from_value::<Peer>(item.clone()).ok())
                        .collect();
                    *lock_or_recover(&self.cached_live_peers) = parsed;
                    return;
                }
            }
        }
        let cfg = lock_or_recover(&self.controller.config).clone();
        let psk = (!cfg.sharing.psk_hash.is_empty()).then_some(cfg.sharing.psk_hash.as_str());
        let found = discover_once_with_psk(0.7, cfg.discovery.port, &cfg.device.id, psk);
        for p in &found {
            self.controller.remember_peer(p);
        }
        *lock_or_recover(&self.cached_live_peers) = found;
    }

    fn refresh_android(&mut self) {
        let tools = self.controller.android_manager.get_tool_status();
        let devices = self.controller.android_manager.list_devices();
        let mut guard = write_or_recover(&self.snapshot);
        guard.android_tools = tools;
        guard.android_devices = devices;
    }

    fn refresh_system(&mut self) {
        let (fw_kind, fw_active, fw_detail) = if ipc_is_running(None) {
            if let Ok(resp) = ipc_request(
                "firewall_status",
                serde_json::json!({}),
                None,
                Duration::from_millis(700),
            ) {
                (
                    resp.get("kind")
                        .and_then(|v| v.as_str())
                        .unwrap_or("none")
                        .to_string(),
                    resp.get("active")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    resp.get("detail")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                )
            } else {
                let fw = detect_firewall();
                (fw.kind, fw.active, fw.detail)
            }
        } else {
            let fw = detect_firewall();
            (fw.kind, fw.active, fw.detail)
        };

        let svc = service_status();
        let mut guard = write_or_recover(&self.snapshot);
        guard.firewall_kind = fw_kind;
        guard.firewall_active = fw_active;
        guard.firewall_detail = fw_detail;
        guard.service_running = svc.get("active").and_then(|v| v.as_bool()).unwrap_or(false);
        guard.service_enabled = svc
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        guard.autostart_installed = svc
            .get("tray_autostart")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
    }

    fn refresh_fast(&mut self, allow_during_command: bool) {
        let seq_before = self.mutation_seq.load(Ordering::SeqCst);
        if !allow_during_command && self.active_commands.load(Ordering::SeqCst) > 0 {
            return;
        }

        let daemon_up = self.use_ipc && ipc_is_running(None);
        let status_val = if daemon_up {
            ipc_request(
                "status",
                serde_json::json!({}),
                None,
                Duration::from_millis(800),
            )
            .ok()
        } else {
            None
        };

        let apps_val = if daemon_up {
            ipc_request(
                "apps",
                serde_json::json!({}),
                None,
                Duration::from_millis(600),
            )
            .ok()
            .and_then(|v| v.get("apps").cloned())
            .and_then(|v| v.as_array().cloned())
        } else {
            None
        };

        let live_peers = lock_or_recover(&self.cached_live_peers).clone();

        let (cfg, st, local_telem, status_json, apps_json) = {
            let _sync_guard = if allow_during_command {
                Some(lock_or_recover(&self.controller_sync_lock))
            } else {
                if self.active_commands.load(Ordering::SeqCst) > 0
                    || self.mutation_seq.load(Ordering::SeqCst) != seq_before
                {
                    return;
                }
                match self.controller_sync_lock.try_lock() {
                    Ok(g) => Some(g),
                    Err(_) => return,
                }
            };

            self.controller.reload_from_disk();
            let cfg = lock_or_recover(&self.controller.config).clone();
            let st = lock_or_recover(&self.controller.state).clone();
            let local_telem = self.controller.telemetry_snapshot();

            let status_json =
                status_val.unwrap_or_else(|| self.controller.status(Some(&live_peers)));
            let apps_json = apps_val.unwrap_or_else(|| self.controller.list_apps());
            (cfg, st, local_telem, status_json, apps_json)
        };

        // Parse connection block
        let conn = status_json.get("connection").filter(|c| !c.is_null());
        let connected = conn.is_some();
        let multiroom = conn
            .and_then(|c| c.get("multiroom"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let connected_peer_id = conn
            .and_then(|c| c.get("peer_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let connected_name = conn
            .and_then(|c| c.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let connected_host = conn
            .and_then(|c| c.get("host"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let connected_port = conn
            .and_then(|c| c.get("port"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u16;
        let connected_duration = conn
            .and_then(|c| c.get("duration"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let keep_local = conn
            .and_then(|c| c.get("keep_local"))
            .and_then(|v| v.as_bool())
            .unwrap_or(st.keep_local);
        let multiroom_peers: Vec<ActivePeerSession> = conn
            .and_then(|c| c.get("multiroom_peers"))
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or(st.multiroom_peers.clone());
        let rtt_ms = conn.and_then(|c| c.get("rtt_ms")).and_then(|v| v.as_f64());
        let jitter_ms = conn
            .and_then(|c| c.get("jitter_ms"))
            .and_then(|v| v.as_f64());

        // Volume
        let vol_obj = status_json.get("volume").filter(|v| !v.is_null());
        let volume_level = vol_obj
            .and_then(|v| v.get("level"))
            .and_then(|v| v.as_u64())
            .unwrap_or(100) as u32;
        let volume_muted = vol_obj
            .and_then(|v| v.get("mute"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // Sharing & Auto
        let sharing_enabled = status_json
            .get("sharing")
            .and_then(|s| s.get("enabled"))
            .and_then(|v| v.as_bool())
            .unwrap_or(cfg.sharing.enabled);
        let auto_connect = status_json
            .get("auto")
            .and_then(|a| a.get("enabled"))
            .and_then(|v| v.as_bool())
            .unwrap_or(cfg.connection.auto_connect);
        let auto_paused = status_json
            .get("auto")
            .and_then(|a| a.get("paused"))
            .and_then(|v| v.as_bool())
            .unwrap_or(st.auto_paused);

        // Mic
        let mic_enabled = conn
            .and_then(|c| c.get("with_mic"))
            .and_then(|v| v.as_bool())
            .unwrap_or(!st.connected_source.is_empty());
        let mic_direction = conn
            .and_then(|c| c.get("mic_direction"))
            .and_then(|v| v.as_str())
            .unwrap_or(&st.mic_direction)
            .to_string();

        // Android session
        let and_obj = status_json.get("android");
        let android_session_active = and_obj
            .and_then(|a| a.get("active"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let android_session_device = and_obj
            .and_then(|a| a.get("device_name"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Audio level (prefer local telemetry if active, else daemon snapshot)
        let daemon_audio = status_json.get("telemetry").and_then(|t| t.get("audio"));
        let (audio_active, audio_dbfs, audio_peak) = if local_telem.audio.active {
            (
                true,
                local_telem.audio.dbfs,
                local_telem
                    .audio
                    .peak_left
                    .max(local_telem.audio.peak_right),
            )
        } else if let Some(da) = daemon_audio {
            let active = da.get("active").and_then(|v| v.as_bool()).unwrap_or(false);
            let dbfs = da.get("dbfs").and_then(|v| v.as_f64()).unwrap_or(-60.0) as f32;
            let pl = da.get("peak_left").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
            let pr = da.get("peak_right").and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
            (active, dbfs, pl.max(pr))
        } else {
            (
                false,
                local_telem.audio.dbfs,
                local_telem
                    .audio
                    .peak_left
                    .max(local_telem.audio.peak_right),
            )
        };

        // Peers (merge live + remembered)
        let paired_ids: HashSet<String> = st.paired_peers.keys().cloned().collect();
        let mut peer_views = Vec::new();
        let mut seen_ids = HashSet::new();
        for p in &live_peers {
            if p.id == cfg.device.id || seen_ids.contains(&p.id) {
                continue;
            }
            if !is_private_address(&p.host) || is_own_non_loopback_ip(&p.host) {
                continue;
            }
            seen_ids.insert(p.id.clone());
            peer_views.push(PeerView {
                id: p.id.clone(),
                name: p.name.clone(),
                host: p.host.clone(),
                port: p.port,
                platform: p.platform.clone(),
                security: p.security.clone(),
                rtt_ms: p.rtt_ms,
                online: true,
                paired: paired_ids.contains(&p.id) || p.security == "psk",
            });
        }
        for kp in st.known_peers.values() {
            if kp.id.is_empty()
                || kp.id == cfg.device.id
                || seen_ids.contains(&kp.id)
                || !is_private_address(&kp.host)
                || is_own_non_loopback_ip(&kp.host)
            {
                continue;
            }
            seen_ids.insert(kp.id.clone());
            let is_conn =
                connected_peer_id == kp.id || multiroom_peers.iter().any(|mp| mp.peer_id == kp.id);
            peer_views.push(PeerView {
                id: kp.id.clone(),
                name: kp.name.clone(),
                host: kp.host.clone(),
                port: kp.port,
                platform: kp.platform.clone(),
                security: kp.security.clone(),
                rtt_ms: if is_conn { rtt_ms } else { None },
                online: is_conn,
                paired: paired_ids.contains(&kp.id) || kp.security == "psk",
            });
        }

        // Apps
        let apps: Vec<AppStreamView> = apps_json
            .into_iter()
            .map(|a| {
                let id = a.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
                let app_name = a
                    .get("app_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("App")
                    .to_string();
                let media_name = a
                    .get("media_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let is_remote = a
                    .get("is_remote")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let routed_rule = a
                    .get("routed_rule")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let persistent = a
                    .get("persistent")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                AppStreamView {
                    app_name,
                    media_name,
                    is_remote,
                    routed_rule,
                    persistent,
                    active: id > 0,
                }
            })
            .collect();

        if !allow_during_command
            && (self.active_commands.load(Ordering::SeqCst) > 0
                || self.mutation_seq.load(Ordering::SeqCst) != seq_before)
        {
            return;
        }

        let mut guard = write_or_recover(&self.snapshot);
        if !guard.hydrated && !self.onboarding_reopened.load(Ordering::Relaxed) {
            guard.show_onboarding = !cfg.ui.onboarding_completed && st.known_peers.is_empty();
        }
        guard.hydrated = true;
        guard.daemon_connected = daemon_up;
        guard.config = cfg;
        guard.connected = connected;
        guard.multiroom = multiroom;
        guard.connected_peer_id = connected_peer_id;
        guard.connected_name = connected_name;
        guard.connected_host = connected_host;
        guard.connected_port = connected_port;
        guard.connected_duration = connected_duration;
        guard.multiroom_peers = multiroom_peers;
        guard.keep_local = keep_local;
        guard.sharing_enabled = sharing_enabled;
        guard.auto_connect = auto_connect;
        guard.auto_paused = auto_paused;
        guard.volume_level = volume_level;
        guard.volume_muted = volume_muted;
        guard.mic_enabled = mic_enabled;
        guard.mic_direction = mic_direction;
        guard.peers = peer_views;
        guard.apps = apps;
        guard.audio_active = audio_active;
        guard.audio_dbfs = audio_dbfs;
        guard.audio_peak = audio_peak;
        guard.rtt_ms = rtt_ms;
        guard.jitter_ms = jitter_ms;
        guard.android_session_active = android_session_active;
        guard.android_session_device = android_session_device;
        guard.paired_count = paired_ids.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soundhop_core::backend::linux::LinuxAudioBackend;
    use soundhop_core::state::State;
    use soundhop_core::util::{CommandResult, MockRunner};

    #[test]
    fn test_snapshot_plain_language_helpers() {
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
        let mut snap = GuiSnapshot::from_controller(&ctrl);
        assert_eq!(snap.output_headline(), "Playing on This Device");
        assert!(snap.show_onboarding);

        snap.connected = true;
        snap.connected_name = "Living Room PC".to_string();
        assert_eq!(snap.output_headline(), "Playing on Living Room PC");

        let peer = PeerView {
            id: "1".to_string(),
            name: "Living Room".to_string(),
            host: "192.168.1.50".to_string(),
            port: 4713,
            platform: "linux".to_string(),
            security: "trusted-lan".to_string(),
            rtt_ms: Some(4.2),
            online: true,
            paired: false,
        };
        assert_eq!(peer.signal_label(), "Excellent signal");
        assert_eq!(security_friendly_label("psk"), "PIN-Paired Devices Only");
    }

    fn test_controller() -> Arc<Controller> {
        let runner = Arc::new(MockRunner::new(|args, _| {
            Ok(CommandResult {
                args: args.to_vec(),
                returncode: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }));
        let backend = Arc::new(LinuxAudioBackend::with_runner(runner));
        let ctrl = Arc::new(Controller::new(
            Some(Config::default()),
            Some(State::default()),
            Some(backend),
            None,
            None,
            false,
        ));
        ctrl.set_save_enabled(false);
        ctrl
    }

    #[test]
    fn test_optimistic_updates_on_send() {
        let ctrl = test_controller();
        let initial = GuiSnapshot::from_controller(&ctrl);
        let snapshot = Arc::new(RwLock::new(initial));
        let (tx, _rx) = mpsc::channel();
        let gui_backend = GuiBackend {
            snapshot: Arc::clone(&snapshot),
            tx: Some(tx),
            toasts: Arc::new(Mutex::new(Vec::new())),
            ctx: None,
            mutation_seq: Arc::new(AtomicU64::new(0)),
            active_commands: Arc::new(AtomicUsize::new(0)),
            stop_flag: Arc::new(AtomicBool::new(false)),
            poller_wake: Arc::new((Mutex::new(false), Condvar::new())),
            cmd_handle: None,
            poller_handle: None,
        };

        gui_backend.send(GuiCommand::SetShare { enabled: false });
        assert!(!gui_backend.read().sharing_enabled);

        gui_backend.send(GuiCommand::SetShare { enabled: true });
        assert!(gui_backend.read().sharing_enabled);

        gui_backend.send(GuiCommand::SetMute { muted: true });
        assert!(gui_backend.read().volume_muted);

        gui_backend.send(GuiCommand::SetDefaultRoutingMode {
            mode: "selective".to_string(),
        });
        assert_eq!(gui_backend.read().config.routing.default_mode, "selective");

        gui_backend.send(GuiCommand::SaveConfigPatch {
            patch: serde_json::json!({ "ui": { "theme": "light" } }),
            toast: None,
        });
        assert_eq!(gui_backend.read().config.ui.theme, "light");

        gui_backend.send(GuiCommand::RefreshPeers);
        assert!(gui_backend.read().refreshing_peers);

        gui_backend.send(GuiCommand::ConnectPeer {
            peer_id: "peer-1".to_string(),
        });
        let snap = gui_backend.read();
        assert_eq!(snap.pending_peer_id.as_deref(), Some("peer-1"));
        assert!(snap.busy_message.is_some());
    }

    #[test]
    fn test_stale_refresh_does_not_clobber_active_mutation() {
        let ctrl = test_controller();
        let initial = GuiSnapshot::from_controller(&ctrl);
        let snapshot = Arc::new(RwLock::new(initial));
        let mutation_seq = Arc::new(AtomicU64::new(0));
        let active_commands = Arc::new(AtomicUsize::new(1));

        let mut worker = WorkerState {
            controller: Arc::clone(&ctrl),
            snapshot: Arc::clone(&snapshot),
            toasts: Arc::new(Mutex::new(Vec::new())),
            ctx: None,
            cached_live_peers: Arc::new(Mutex::new(Vec::new())),
            mutation_seq: Arc::clone(&mutation_seq),
            active_commands: Arc::clone(&active_commands),
            onboarding_reopened: Arc::new(AtomicBool::new(false)),
            controller_sync_lock: Arc::new(Mutex::new(())),
            use_ipc: false,
        };

        // Simulate optimistic UI state (false) opposing controller truth (true) while a command is in flight
        snapshot.write().unwrap().sharing_enabled = false;
        mutation_seq.fetch_add(1, Ordering::SeqCst);

        // Background poller refresh (allow_during_command = false) must not revert it
        worker.refresh_fast(false);
        assert!(!snapshot.read().unwrap().sharing_enabled);

        // Once command completes (active_commands == 0), forced refresh updates to truth (true)
        active_commands.store(0, Ordering::SeqCst);
        worker.refresh_fast(true);
        assert!(snapshot.read().unwrap().sharing_enabled);
        assert!(snapshot.read().unwrap().hydrated);
    }

    #[test]
    fn test_lock_recovery_after_poison() {
        let m = Arc::new(Mutex::new(42));
        let m2 = Arc::clone(&m);
        let _ = std::thread::spawn(move || {
            let _g = m2.lock().unwrap();
            panic!("intentional poison");
        })
        .join();
        assert_eq!(*lock_or_recover(&m), 42);

        let rw = Arc::new(RwLock::new(99));
        let rw2 = Arc::clone(&rw);
        let _ = std::thread::spawn(move || {
            let _g = rw2.write().unwrap();
            panic!("intentional rw poison");
        })
        .join();
        assert_eq!(*read_or_recover(&rw), 99);
        *write_or_recover(&rw) = 100;
        assert_eq!(*read_or_recover(&rw), 100);
    }

    #[test]
    fn test_connect_address_invalid_input_clears_busy() {
        let ctrl = test_controller();
        let initial = GuiSnapshot::from_controller(&ctrl);
        let snapshot = Arc::new(RwLock::new(initial));
        let toasts = Arc::new(Mutex::new(Vec::new()));
        let (tx, _rx) = mpsc::channel();
        let gui_backend = GuiBackend {
            snapshot: Arc::clone(&snapshot),
            tx: Some(tx),
            toasts: Arc::clone(&toasts),
            ctx: None,
            mutation_seq: Arc::new(AtomicU64::new(0)),
            active_commands: Arc::new(AtomicUsize::new(0)),
            stop_flag: Arc::new(AtomicBool::new(false)),
            poller_wake: Arc::new((Mutex::new(false), Condvar::new())),
            cmd_handle: None,
            poller_handle: None,
        };

        // Invalid host-less port input must not set optimistic busy_message
        gui_backend.send(GuiCommand::ConnectAddress {
            host_port: ":4713".to_string(),
        });
        assert!(gui_backend.read().busy_message.is_none());

        // Even if busy_message was already set, handle_command early return clears it
        snapshot.write().unwrap().busy_message = Some("Connecting...".to_string());
        let mut worker = WorkerState {
            controller: Arc::clone(&ctrl),
            snapshot: Arc::clone(&snapshot),
            toasts: Arc::clone(&toasts),
            ctx: None,
            cached_live_peers: Arc::new(Mutex::new(Vec::new())),
            mutation_seq: Arc::new(AtomicU64::new(0)),
            active_commands: Arc::new(AtomicUsize::new(0)),
            onboarding_reopened: Arc::new(AtomicBool::new(false)),
            controller_sync_lock: Arc::new(Mutex::new(())),
            use_ipc: false,
        };
        worker.handle_command(GuiCommand::ConnectAddress {
            host_port: ":4713".to_string(),
        });
        assert!(snapshot.read().unwrap().busy_message.is_none());
        assert!(toasts.lock().unwrap().last().is_some_and(|t| t.is_error));
    }

    #[test]
    fn test_optimistic_route_app_updates_all_matching_streams() {
        let ctrl = test_controller();
        let mut initial = GuiSnapshot::from_controller(&ctrl);
        initial.apps = vec![
            AppStreamView {
                app_name: "Firefox".to_string(),
                media_name: "Tab 1".to_string(),
                is_remote: false,
                routed_rule: String::new(),
                persistent: false,
                active: true,
            },
            AppStreamView {
                app_name: "firefox".to_string(),
                media_name: "Tab 2".to_string(),
                is_remote: false,
                routed_rule: String::new(),
                persistent: false,
                active: true,
            },
        ];
        let snapshot = Arc::new(RwLock::new(initial));
        let (tx, _rx) = mpsc::channel();
        let gui_backend = GuiBackend {
            snapshot: Arc::clone(&snapshot),
            tx: Some(tx),
            toasts: Arc::new(Mutex::new(Vec::new())),
            ctx: None,
            mutation_seq: Arc::new(AtomicU64::new(0)),
            active_commands: Arc::new(AtomicUsize::new(0)),
            stop_flag: Arc::new(AtomicBool::new(false)),
            poller_wake: Arc::new((Mutex::new(false), Condvar::new())),
            cmd_handle: None,
            poller_handle: None,
        };

        gui_backend.send(GuiCommand::RouteApp {
            app: "Firefox".to_string(),
            target: "remote".to_string(),
            save: true,
        });
        let snap = gui_backend.read();
        assert_eq!(snap.apps.len(), 2);
        assert!(snap
            .apps
            .iter()
            .all(|a| a.is_remote && a.routed_rule == "remote" && a.persistent));

        gui_backend.send(GuiCommand::UnrouteApp {
            app: "Firefox".to_string(),
            remove_rule: true,
        });
        let snap = gui_backend.read();
        assert!(snap
            .apps
            .iter()
            .all(|a| !a.is_remote && a.routed_rule.is_empty() && !a.persistent));
    }
}
