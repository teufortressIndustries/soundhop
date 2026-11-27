//! Background worker state, polling refreshes, and Android/system/onboarding handlers.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use eframe::egui;
use soundhop_core::constants::{PROTOCOL_VERSION, VERSION};
use soundhop_core::controller::Controller;
use soundhop_core::discovery::{discover_once_with_meta, Peer};
use soundhop_core::firewall::{detect_for_config, open_ports_for_config};
use soundhop_core::ipc::{is_running as ipc_is_running, request as ipc_request};
use soundhop_core::service::{
    install as install_service, status as service_status, uninstall as uninstall_service,
};
use soundhop_core::util::{atomic_write, config_path, local_addresses, log_path, state_path};

use crate::{lock_or_recover, write_or_recover};

use super::commands::ipc_checked;
use super::snapshot::{
    parse_android_history, parse_pending_pair_requests_json, parse_trusted_peers_json, GuiSnapshot,
    GuiToast, RefreshUpdate,
};

pub(super) struct WorkerState {
    pub(super) controller: Arc<Controller>,
    pub(super) snapshot: Arc<RwLock<GuiSnapshot>>,
    pub(super) toasts: Arc<Mutex<Vec<GuiToast>>>,
    pub(super) ctx: Option<egui::Context>,
    pub(super) cached_live_peers: Arc<Mutex<Vec<Peer>>>,
    pub(super) mutation_seq: Arc<AtomicU64>,
    pub(super) active_commands: Arc<AtomicUsize>,
    pub(super) onboarding_reopened: Arc<AtomicBool>,
    pub(super) quit_flag: Arc<AtomicBool>,
    pub(super) controller_sync_lock: Arc<Mutex<()>>,
    pub(super) use_ipc: bool,
}

impl WorkerState {
    pub(super) fn request_repaint(&self) {
        if let Some(ref ctx) = self.ctx {
            ctx.request_repaint();
        }
    }

    pub(super) fn push_toast(&self, message: impl Into<String>, is_error: bool) {
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

    pub(super) fn set_busy(&self, msg: Option<String>) {
        write_or_recover(&self.snapshot).busy_message = msg;
        self.request_repaint();
    }

    pub(super) fn find_peer_for_id(&self, peer_id: &str) -> Option<Peer> {
        if let Some(p) = lock_or_recover(&self.cached_live_peers)
            .iter()
            .find(|p| p.id == peer_id)
        {
            return Some(p.clone());
        }
        self.controller.known_peer(peer_id)
    }

    pub(super) fn refresh_peers(&mut self) {
        if self.use_ipc && ipc_is_running(None) {
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
        let found_with_meta = discover_once_with_meta(0.7, cfg.discovery.port, &cfg.device.id, psk);
        let mut found = Vec::with_capacity(found_with_meta.len());
        for (p, meta) in found_with_meta {
            if let Some(m) = meta {
                self.controller.remember_peer_v3_meta(&p.id, m);
            }
            self.controller.remember_peer(&p);
            found.push(p);
        }
        *lock_or_recover(&self.cached_live_peers) = found;
    }

    pub(super) fn refresh_android(&mut self) {
        let mut tools = self.controller.android_manager.get_tool_status();
        let devices = self.controller.android_manager.list_devices();
        let mut adb_ready_opt: Option<bool> = None;
        let mut scrcpy_ready_opt: Option<bool> = None;
        let history = if self.use_ipc && ipc_is_running(None) {
            if let Ok(resp) = ipc_request(
                "android_status",
                serde_json::json!({}),
                None,
                Duration::from_millis(800),
            ) {
                if let Some(t_val) = resp.get("tools") {
                    adb_ready_opt = t_val.get("adb_ready").and_then(|v| v.as_bool());
                    scrcpy_ready_opt = t_val.get("scrcpy_ready").and_then(|v| v.as_bool());
                    if let Ok(parsed) =
                        serde_json::from_value::<soundhop_core::android::ToolStatus>(t_val.clone())
                    {
                        tools = parsed;
                    }
                }
                resp.get("history")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .map(parse_android_history)
                    .unwrap_or_else(|| parse_android_history(self.controller.android_history()))
            } else {
                parse_android_history(self.controller.android_history())
            }
        } else {
            parse_android_history(self.controller.android_history())
        };
        let mut guard = write_or_recover(&self.snapshot);
        guard.adb_ready = adb_ready_opt.unwrap_or_else(|| tools.adb_ready());
        guard.scrcpy_ready = scrcpy_ready_opt.unwrap_or_else(|| tools.scrcpy_ready());
        guard.android_tools = tools;
        guard.android_devices = devices;
        guard.android_history = history;
    }

    pub(super) fn refresh_system(&mut self) {
        let cfg = lock_or_recover(&self.controller.config).clone();
        let (fw_kind, fw_active, fw_detail, fw_ports_allowed) =
            if self.use_ipc && ipc_is_running(None) {
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
                        resp.get("ports_allowed").and_then(|v| v.as_bool()),
                    )
                } else {
                    let fw = detect_for_config(&cfg);
                    (fw.kind, fw.active, fw.detail, fw.ports_allowed)
                }
            } else {
                let fw = detect_for_config(&cfg);
                (fw.kind, fw.active, fw.detail, fw.ports_allowed)
            };

        let svc = service_status();
        let (gst_status, gst_error) = match soundhop_core::transport::check_gstreamer_runtime() {
            Ok(st) => (Some(st), None),
            Err(e) => (None, Some(e.to_string())),
        };
        let mut guard = write_or_recover(&self.snapshot);
        guard.firewall_kind = fw_kind;
        guard.firewall_active = fw_active;
        guard.firewall_detail = fw_detail;
        guard.firewall_ports_allowed = fw_ports_allowed;
        guard.service_running = svc.get("active").and_then(|v| v.as_bool()).unwrap_or(false);
        guard.service_enabled = svc
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        guard.autostart_installed = svc
            .get("tray_autostart")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        guard.gstreamer_status = gst_status;
        guard.gstreamer_error = gst_error;
    }

    pub(super) fn refresh_fast(&mut self, allow_during_command: bool) -> bool {
        let seq_before = self.mutation_seq.load(Ordering::SeqCst);
        if !allow_during_command && self.active_commands.load(Ordering::SeqCst) > 0 {
            return false;
        }

        let daemon_up = self.use_ipc && ipc_is_running(None);
        let status_val = if daemon_up {
            match ipc_request(
                "status",
                serde_json::json!({}),
                None,
                Duration::from_millis(2500),
            ) {
                Ok(v) if v.is_object() && v.get("ok").and_then(|o| o.as_bool()) != Some(false) => {
                    Some(v)
                }
                Ok(v) => {
                    let err = v
                        .get("error")
                        .and_then(|e| e.as_str())
                        .unwrap_or("status IPC returned ok=false or non-object");
                    tracing::warn!(error = %err, "daemon is running but status IPC failed; keeping previous snapshot");
                    return false;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "daemon is running but status IPC request failed; keeping previous snapshot");
                    return false;
                }
            }
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

        let trusted_peers_val = if daemon_up {
            ipc_request(
                "pair_list",
                serde_json::json!({}),
                None,
                Duration::from_millis(500),
            )
            .ok()
            .and_then(|v| v.get("trusted_peers").and_then(|a| a.as_array()).cloned())
            .map(|arr| parse_trusted_peers_json(&arr))
        } else {
            None
        };

        let pending_pair_val = if daemon_up {
            ipc_request(
                "pair_pending",
                serde_json::json!({}),
                None,
                Duration::from_millis(500),
            )
            .ok()
            .and_then(|v| v.get("requests").and_then(|a| a.as_array()).cloned())
            .map(|arr| parse_pending_pair_requests_json(&arr))
        } else {
            None
        };

        let live_peers = lock_or_recover(&self.cached_live_peers).clone();

        let (cfg, st, local_telem, status_json, apps_json, trusted_peers, pending_pair_requests) = {
            let _sync_guard = if allow_during_command {
                Some(lock_or_recover(&self.controller_sync_lock))
            } else {
                if self.active_commands.load(Ordering::SeqCst) > 0
                    || self.mutation_seq.load(Ordering::SeqCst) != seq_before
                {
                    return false;
                }
                match self.controller_sync_lock.try_lock() {
                    Ok(g) => Some(g),
                    Err(_) => return false,
                }
            };

            self.controller.reload_from_disk();
            let cfg = lock_or_recover(&self.controller.config).clone();
            let st = lock_or_recover(&self.controller.state).clone();
            let local_telem = self.controller.telemetry_snapshot();

            let status_json =
                status_val.unwrap_or_else(|| self.controller.status(Some(&live_peers)));
            let apps_json = apps_val.unwrap_or_else(|| self.controller.list_apps());
            let trusted_peers =
                trusted_peers_val.unwrap_or_else(|| self.controller.trusted_peers());
            let pending_pair_requests =
                pending_pair_val.unwrap_or_else(|| self.controller.pending_pair_requests());
            (
                cfg,
                st,
                local_telem,
                status_json,
                apps_json,
                trusted_peers,
                pending_pair_requests,
            )
        };

        let update = RefreshUpdate::from_status(
            daemon_up,
            cfg,
            st,
            local_telem,
            &status_json,
            apps_json,
            &live_peers,
            trusted_peers,
            pending_pair_requests,
        );

        if !allow_during_command
            && (self.active_commands.load(Ordering::SeqCst) > 0
                || self.mutation_seq.load(Ordering::SeqCst) != seq_before)
        {
            return false;
        }

        let mut guard = write_or_recover(&self.snapshot);
        update.apply_to(&mut guard, self.onboarding_reopened.load(Ordering::Relaxed))
    }

    pub(super) fn handle_android_install_tools(&mut self) {
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

    pub(super) fn handle_android_switch_wifi(&mut self, serial: String) {
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

    pub(super) fn handle_android_connect_wifi(&mut self, ip_port: String) {
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

    pub(super) fn handle_android_disconnect_wifi(&mut self, serial: String) {
        let (ok, msg) = self
            .controller
            .android_manager
            .disconnect_wifi(Some(&serial));
        self.push_toast(msg, !ok);
        self.refresh_android();
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn handle_android_start(
        &mut self,
        serial: String,
        device_name: String,
        codec: String,
        source: String,
        buffer_ms: u32,
        duplicate: bool,
        as_mic: bool,
    ) {
        write_or_recover(&self.snapshot).android_starting = true;
        self.set_busy(Some(format!("Starting audio from {device_name}...")));
        let res = if ipc_is_running(None) {
            ipc_checked(
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
                Duration::from_secs(10),
                "Failed to start phone audio",
            )
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

    pub(super) fn handle_android_stop(&mut self) {
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

    pub(super) fn handle_open_firewall(&mut self) {
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
            open_ports_for_config(&cfg, false).ok
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

    pub(super) fn handle_run_diagnostics(&mut self, fix: bool) {
        write_or_recover(&self.snapshot).diagnostics_running = true;
        self.set_busy(Some("Running diagnostics & fixes...".to_string()));

        let cfg = lock_or_recover(&self.controller.config).clone();
        let backend_ok = self.controller.backend.available();
        let server_name = self.controller.backend.server_label().unwrap_or_default();
        let def_sink = self.controller.backend.default_sink().unwrap_or_default();
        let addrs = local_addresses(false);

        let mut fixed_actions: Vec<String> = Vec::new();
        let mut fix_errors: Vec<String> = Vec::new();

        if fix {
            let _sync = lock_or_recover(&self.controller_sync_lock);
            let rec = self.controller.reconcile();
            if let Some(arr) = rec.get("actions").and_then(|v| v.as_array()) {
                for item in arr {
                    if let Some(s) = item.as_str() {
                        fixed_actions.push(s.to_string());
                    }
                }
            }
        }

        let mut fw = detect_for_config(&cfg);
        if fix && fw.active && fw.ports_allowed != Some(true) {
            let fw_ok = if self.use_ipc && ipc_is_running(None) {
                ipc_request(
                    "firewall_open",
                    serde_json::json!({}),
                    None,
                    Duration::from_secs(15),
                )
                .map(|v| v.get("ok").and_then(|x| x.as_bool()).unwrap_or(false))
                .unwrap_or(false)
            } else {
                open_ports_for_config(&cfg, false).ok
            };
            if fw_ok {
                fixed_actions.push(format!("Opened firewall ports on {}", fw.kind));
            } else {
                fix_errors.push(format!("Could not open firewall ports on {}", fw.kind));
            }
            fw = detect_for_config(&cfg);
        }

        let mut svc = service_status();
        let mut svc_active = svc.get("active").and_then(|v| v.as_bool()).unwrap_or(false);
        let mut daemon_ipc = self.use_ipc && ipc_is_running(None);
        if fix && !svc_active && !daemon_ipc {
            let svc_res = install_service(true, true, true);
            let started = svc_res
                .get("started")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if started {
                fixed_actions.push("Installed and started background service".to_string());
            } else {
                fix_errors.push("Could not start background service automatically".to_string());
            }
            svc = service_status();
            svc_active = svc.get("active").and_then(|v| v.as_bool()).unwrap_or(false);
            daemon_ipc = self.use_ipc && ipc_is_running(None);
        }

        let tools = self.controller.android_manager.get_tool_status();

        let mut lines = Vec::new();
        if backend_ok {
            lines.push(format!(
                "Audio Backend: OK ({}, output: {})",
                if server_name.is_empty() {
                    "available"
                } else {
                    server_name.as_str()
                },
                if def_sink.is_empty() {
                    "default"
                } else {
                    def_sink.as_str()
                }
            ));
        } else {
            lines.push(
                "Audio Backend: Unavailable (check PulseAudio/PipeWire or Windows Audio)"
                    .to_string(),
            );
        }

        if addrs.is_empty() {
            lines.push("LAN Network: No private LAN IPv4 addresses detected".to_string());
        } else {
            lines.push(format!("LAN Network: OK ({})", addrs.join(", ")));
        }

        let fw_rule_note = match fw.ports_allowed {
            Some(true) => "ports allowed",
            Some(false) => "ports may be blocked",
            None => "rule check unavailable",
        };
        lines.push(format!(
            "Firewall: {} ({}, {})",
            fw.kind,
            if fw.active { "active" } else { "inactive" },
            fw_rule_note
        ));

        lines.push(format!(
            "Background Service: {}",
            if daemon_ipc || svc_active {
                "Running"
            } else {
                "Not running"
            }
        ));

        lines.push(format!(
            "Android Bridge: {}",
            if tools.ready() {
                format!("Ready (scrcpy {})", tools.scrcpy_version)
            } else {
                "Not installed (optional)".to_string()
            }
        ));

        for act in &fixed_actions {
            lines.push(format!("Fixed: {act}"));
        }
        for err in &fix_errors {
            lines.push(format!("Issue: {err}"));
        }

        let healthy = backend_ok && !addrs.is_empty() && fix_errors.is_empty();
        let summary = lines.join("\n");

        {
            let mut guard = write_or_recover(&self.snapshot);
            guard.diagnostics_running = false;
            guard.diagnostics_summary = Some(summary);
            guard.diagnostics_healthy = Some(healthy);
            guard.firewall_kind = fw.kind;
            guard.firewall_active = fw.active;
            guard.firewall_detail = fw.detail;
            guard.firewall_ports_allowed = fw.ports_allowed;
        }
        self.set_busy(None);
        if healthy {
            self.push_toast("Diagnostics complete — system looks healthy.", false);
        } else {
            self.push_toast(
                "Diagnostics complete — review findings in Preferences.",
                true,
            );
        }
        self.refresh_system();
        self.refresh_fast(true);
    }

    pub(super) fn handle_export_bug_report(&mut self) {
        self.set_busy(Some("Collecting bug report...".to_string()));
        let daemon_up = self.use_ipc && ipc_is_running(None);
        let (bundle, daemon_log) = self.collect_bug_report_bundle(&|cmd, payload| {
            if !daemon_up {
                return None;
            }
            match ipc_request(cmd, payload, None, Duration::from_millis(2500)) {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!(cmd, error = %e, "bug report IPC request failed");
                    Some(serde_json::json!({ "ok": false, "error": e.to_string() }))
                }
            }
        });

        let pretty = serde_json::to_string_pretty(&bundle).unwrap_or_else(|_| bundle.to_string());
        let report_path = daemon_log
            .parent()
            .map(|p| p.join("soundhop-bug-report.json"))
            .unwrap_or_else(|| std::env::temp_dir().join("soundhop-bug-report.json"));
        let saved_path = match atomic_write(&report_path, &pretty) {
            Ok(()) => Some(report_path.to_string_lossy().into_owned()),
            Err(_) => None,
        };

        {
            let mut guard = write_or_recover(&self.snapshot);
            guard.bug_report_json = Some(pretty);
            guard.bug_report_path = saved_path.clone();
        }
        self.set_busy(None);
        if let Some(path) = saved_path {
            self.push_toast(format!("Bug report exported to {path}"), false);
        } else {
            self.push_toast("Bug report collected and ready to copy.", false);
        }
    }

    fn collect_bug_report_bundle(
        &self,
        fetch_ipc: &dyn Fn(&str, serde_json::Value) -> Option<serde_json::Value>,
    ) -> (serde_json::Value, std::path::PathBuf) {
        let cfg = lock_or_recover(&self.controller.config).clone();
        let mut status_ipc_error: Option<String> = None;
        let st = match fetch_ipc("status", serde_json::json!({})) {
            Some(v) if v.is_object() && v.get("ok").and_then(|o| o.as_bool()) != Some(false) => v,
            Some(v) => {
                let err_msg = v
                    .get("error")
                    .and_then(|e| e.as_str())
                    .unwrap_or("status IPC returned ok=false or non-object")
                    .to_string();
                tracing::warn!(
                    error = %err_msg,
                    "status IPC failed during bug report collection; falling back to local controller"
                );
                status_ipc_error = Some(err_msg);
                let _sync = lock_or_recover(&self.controller_sync_lock);
                self.controller.status(None)
            }
            None => {
                let _sync = lock_or_recover(&self.controller_sync_lock);
                self.controller.status(None)
            }
        };
        let fw = detect_for_config(&cfg);
        let svc = service_status();
        let daemon_log = log_path();
        let recent_logs: Vec<String> = fetch_ipc("logs", serde_json::json!({ "lines": 50 }))
            .filter(|v| v.get("ok").and_then(|o| o.as_bool()) != Some(false))
            .and_then(|v| v.get("lines").and_then(|l| l.as_array()).cloned())
            .map(|arr| {
                arr.into_iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_else(|| {
                std::fs::read_to_string(&daemon_log)
                    .ok()
                    .map(|content| {
                        let all: Vec<&str> = content.lines().collect();
                        let start = all.len().saturating_sub(50);
                        all[start..].iter().map(|s| (*s).to_string()).collect()
                    })
                    .unwrap_or_default()
            });

        let sanitized_config = serde_json::json!({
            "device_name": cfg.device.name,
            "sharing_enabled": cfg.sharing.enabled,
            "sharing_port": cfg.sharing.port,
            "sharing_security": cfg.sharing.security,
            "has_psk_hash": !cfg.sharing.psk_hash.is_empty(),
            "auto_connect": cfg.connection.auto_connect,
            "disconnect_on_quit": cfg.connection.disconnect_on_quit,
            "keep_local_output": cfg.connection.keep_local_output,
            "latency_ms": cfg.connection.latency_ms,
            "discovery_mdns": cfg.discovery.mdns,
            "discovery_udp": cfg.discovery.udp,
            "discovery_port": cfg.discovery.port,
            "routing_default_mode": cfg.routing.default_mode,
        });

        let mut bundle = serde_json::json!({
            "version": VERSION,
            "protocol": PROTOCOL_VERSION,
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "config_path": config_path().to_string_lossy(),
            "state_path": state_path().to_string_lossy(),
            "log_path": daemon_log.to_string_lossy(),
            "config": sanitized_config,
            "status": st,
            "firewall": fw,
            "service": svc,
            "recent_logs": recent_logs,
        });
        if let Some(err) = status_ipc_error {
            bundle["status_ipc_error"] = serde_json::Value::String(err);
        }
        (bundle, daemon_log)
    }

    pub(super) fn handle_set_autostart(&mut self, enabled: bool) {
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

    pub(super) fn handle_complete_onboarding(
        &mut self,
        device_name: String,
        enable_sharing: bool,
        enable_autostart: bool,
        open_firewall: bool,
    ) {
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
                open_ports_for_config(&cfg, false).ok
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

    pub(super) fn handle_reopen_onboarding(&mut self) {
        self.onboarding_reopened.store(true, Ordering::SeqCst);
        write_or_recover(&self.snapshot).show_onboarding = true;
    }

    pub(super) fn apply_config_patch(&mut self, patch: serde_json::Value, toast: Option<String>) {
        let res = if self.use_ipc && ipc_is_running(None) {
            ipc_checked(
                "config_patch",
                serde_json::json!({ "patch": patch }),
                Duration::from_secs(4),
                "Could not save setting",
            )
            .map(|_| {
                let _sync = lock_or_recover(&self.controller_sync_lock);
                self.controller.reload_from_disk();
            })
        } else {
            let _sync = lock_or_recover(&self.controller_sync_lock);
            let (
                prev_security,
                prev_port,
                prev_web_port,
                prev_webrtc_port,
                prev_web_token,
                prev_web_keep_local,
            ) = {
                let cfg = lock_or_recover(&self.controller.config);
                (
                    cfg.sharing.security.clone(),
                    cfg.sharing.port,
                    cfg.sharing.web_port,
                    cfg.sharing.webrtc_port,
                    cfg.sharing.web_require_token,
                    cfg.sharing.web_keep_local,
                )
            };
            let r = lock_or_recover(&self.controller.config).apply_patch(&patch);
            if r.is_ok() {
                self.controller.save();
                let (
                    sharing_enabled,
                    new_security,
                    new_port,
                    new_web_port,
                    new_webrtc_port,
                    new_web_token,
                    new_web_keep_local,
                ) = {
                    let cfg = lock_or_recover(&self.controller.config);
                    (
                        cfg.sharing.enabled,
                        cfg.sharing.security.clone(),
                        cfg.sharing.port,
                        cfg.sharing.web_port,
                        cfg.sharing.webrtc_port,
                        cfg.sharing.web_require_token,
                        cfg.sharing.web_keep_local,
                    )
                };
                if (new_security != prev_security || new_port != prev_port)
                    && (sharing_enabled || self.controller.listener_running())
                {
                    let _ = self.controller.start_listener(Some(false));
                }
                if (new_web_port != prev_web_port
                    || new_webrtc_port != prev_webrtc_port
                    || new_web_token != prev_web_token
                    || new_web_keep_local != prev_web_keep_local)
                    && self.controller.web_broadcast.is_active()
                {
                    let prev_serials = self.controller.web_broadcast_status().adb_reversed_serials;
                    let _ = self.controller.stop_web_broadcast();
                    if let Ok(st) = self.controller.start_web_broadcast(None, None, None) {
                        for serial in prev_serials {
                            let _ = self.controller.web_adb_reverse(
                                Some(&serial),
                                Some(st.http_port),
                                true,
                                false,
                            );
                        }
                    }
                }
                let _ = self.controller.enforce_routed_apps();
            }
            r.map_err(|e| e.to_string())
        };
        match res {
            Ok(_) => {
                if let Some(msg) = toast {
                    self.push_toast(msg, false);
                }
            }
            Err(e) => self.push_toast(e, true),
        }
        self.refresh_fast(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::GuiCommand;
    use soundhop_core::backend::linux::LinuxAudioBackend;
    use soundhop_core::config::Config;
    use soundhop_core::state::State;
    use soundhop_core::util::{CommandResult, MockRunner};

    struct TempDirGuard(std::path::PathBuf);

    impl TempDirGuard {
        fn new(tag: &str) -> Self {
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let dir = std::env::temp_dir().join(format!(
                "soundhop-gui-{tag}-{}-{unique}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn make_worker_with_temp_config(tag: &str) -> (TempDirGuard, WorkerState) {
        let dir = TempDirGuard::new(tag);
        let cfg_path = dir.path().join("config.toml");
        let state_path = dir.path().join("state.json");
        let mut cfg = Config::default();
        cfg.sharing.psk_hash = "super-secret-hash-should-not-leak".to_string();
        cfg.save(Some(&cfg_path)).unwrap();

        let runner = Arc::new(MockRunner::new(|args, _| {
            let joined = args.join(" ");
            let stdout = if joined.ends_with(" info") {
                "Server Name: PulseAudio (on PipeWire 1.2.0)\nDefault Sink: alsa_output.mock\n"
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
        }));
        let backend = Arc::new(LinuxAudioBackend::with_runner(runner));
        let ctrl = Arc::new(Controller::new(
            Some(cfg),
            Some(State::default()),
            Some(backend),
            Some(cfg_path),
            Some(state_path),
            false,
        ));
        let snap = Arc::new(RwLock::new(GuiSnapshot::from_controller(&ctrl)));
        let worker = WorkerState {
            controller: ctrl,
            snapshot: snap,
            toasts: Arc::new(Mutex::new(Vec::new())),
            ctx: None,
            cached_live_peers: Arc::new(Mutex::new(Vec::new())),
            mutation_seq: Arc::new(AtomicU64::new(0)),
            active_commands: Arc::new(AtomicUsize::new(0)),
            onboarding_reopened: Arc::new(AtomicBool::new(false)),
            quit_flag: Arc::new(AtomicBool::new(false)),
            controller_sync_lock: Arc::new(Mutex::new(())),
            use_ipc: false,
        };
        (dir, worker)
    }

    #[test]
    fn test_phone_and_settings_config_patches_persist_to_disk() {
        let (dir, mut worker) = make_worker_with_temp_config("patches");

        worker.handle_command(GuiCommand::SaveConfigPatch {
            patch: serde_json::json!({
                "android": {
                    "default_codec": "flac",
                    "default_source": "mic",
                    "buffer_ms": 120,
                    "duplicate_audio": true,
                },
                "connection": {
                    "disconnect_on_quit": false,
                    "last_peer": "peer-living-room",
                }
            }),
            toast: None,
        });

        let reloaded = Config::load(Some(&dir.path().join("config.toml"))).unwrap();
        assert_eq!(reloaded.android.default_codec, "flac");
        assert_eq!(reloaded.android.default_source, "mic");
        assert_eq!(reloaded.android.buffer_ms, 120);
        assert!(reloaded.android.duplicate_audio);
        assert!(!reloaded.connection.disconnect_on_quit);
        assert_eq!(reloaded.connection.last_peer, "peer-living-room");

        // Clearing preferred peer persists empty string
        worker.handle_command(GuiCommand::SaveConfigPatch {
            patch: serde_json::json!({ "connection": { "last_peer": "" }, "audio": { "profile": "pcm-lossless" } }),
            toast: None,
        });
        let reloaded2 = Config::load(Some(&dir.path().join("config.toml"))).unwrap();
        assert_eq!(reloaded2.connection.last_peer, "");
        assert_eq!(reloaded2.audio.profile, "pcm-lossless");
        assert_eq!(
            worker.snapshot.read().unwrap().audio_profile,
            "pcm-lossless"
        );
    }

    #[test]
    fn test_diagnostics_and_bug_report_commands() {
        let (_dir, mut worker) = make_worker_with_temp_config("diag");

        worker.handle_command(GuiCommand::RunDiagnostics { fix: false });
        {
            let snap = worker.snapshot.read().unwrap();
            assert!(!snap.diagnostics_running);
            assert!(snap.diagnostics_summary.is_some());
            assert!(snap
                .diagnostics_summary
                .as_deref()
                .unwrap()
                .contains("Audio Backend:"));
            assert!(snap.diagnostics_healthy.is_some());
        }

        worker.handle_command(GuiCommand::ExportBugReport);
        {
            let snap = worker.snapshot.read().unwrap();
            let json_str = snap
                .bug_report_json
                .as_deref()
                .expect("bug report JSON populated");
            assert!(
                !json_str.contains("super-secret-hash-should-not-leak"),
                "psk_hash must be redacted from exported bug report"
            );
            let parsed: serde_json::Value = serde_json::from_str(json_str).unwrap();
            assert_eq!(parsed["version"], VERSION);
            assert_eq!(parsed["config"]["has_psk_hash"], true);
        }
    }

    #[test]
    fn test_export_bug_report_preserves_web_broadcast_token() {
        use soundhop_core::web::{WebBroadcastConfig, WebError};
        use std::net::{IpAddr, Ipv4Addr};

        let (_dir, mut worker) = make_worker_with_temp_config("wb-unredacted");
        let started = worker
            .controller
            .web_broadcast
            .start(
                WebBroadcastConfig {
                    http_port: 0,
                    webrtc_udp_port: 0,
                    bind_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
                    require_token: true,
                    capture_source_label: "mock.monitor".to_string(),
                    ..Default::default()
                },
                Arc::new(|| Err(WebError::NotRunning)),
            )
            .expect("web broadcast start succeeds");
        let secret_token = started.token.expect("active web broadcast must have token");

        worker.handle_command(GuiCommand::ExportBugReport);
        worker.controller.web_broadcast.stop();

        let snap = worker.snapshot.read().unwrap();
        let json_str = snap
            .bug_report_json
            .as_deref()
            .expect("bug report JSON populated");
        let parsed: serde_json::Value = serde_json::from_str(json_str).unwrap();
        assert_eq!(parsed["version"], VERSION);
        let wb = &parsed["status"]["web_broadcast"];
        assert_eq!(wb["active"], true);
        assert_eq!(wb["http_port"], started.http_port);
        assert_eq!(wb["webrtc_udp_port"], started.webrtc_udp_port);
        assert_eq!(wb["token_required"], true);
        assert_eq!(wb["capture_source"], "mock.monitor");
        assert_eq!(wb["token"], secret_token);
        let expected_suffix = format!("/#t={secret_token}");
        let localhost_url = wb["localhost_url"].as_str().unwrap();
        assert!(localhost_url.ends_with(&expected_suffix));
        for url in wb["urls"].as_array().unwrap() {
            let s = url.as_str().unwrap();
            assert!(s.ends_with(&expected_suffix));
        }
    }

    #[test]
    fn test_export_bug_report_uses_live_daemon_ipc_and_preserves_web_tokens() {
        let (_dir, worker) = make_worker_with_temp_config("wb-ipc");
        let (bundle, _) = worker.collect_bug_report_bundle(&|cmd, payload| match cmd {
            "status" => Some(serde_json::json!({
                "ok": true,
                "v3_active": true,
                "peers": [{ "id": "peer-gui-1", "name": "Studio" }],
                "telemetry": { "rx_bytes": 4242 },
                "web_broadcast": {
                    "active": true,
                    "http_port": 4717,
                    "webrtc_udp_port": 4718,
                    "token_required": true,
                    "token": "gui-ipc-secret",
                    "localhost_url": "http://localhost:4717/#t=gui-ipc-secret",
                    "urls": ["http://127.0.0.1:4717/#t=gui-ipc-secret"],
                },
            })),
            "logs" => {
                assert_eq!(payload["lines"], 50);
                Some(serde_json::json!({
                    "ok": true,
                    "lines": [
                        "[daemon] memory warning",
                        "android: streaming",
                        "opened http://127.0.0.1:4717/#token=gui-ipc-secret"
                    ]
                }))
            }
            _ => None,
        });

        let serialized = serde_json::to_string(&bundle).unwrap();
        assert!(!serialized.contains("super-secret-hash-should-not-leak"));
        assert_eq!(bundle["config"]["has_psk_hash"], true);
        assert!(bundle.get("status_ipc_error").is_none());
        assert_eq!(bundle["status"]["v3_active"], true);
        assert_eq!(bundle["status"]["peers"][0]["id"], "peer-gui-1");
        assert_eq!(bundle["status"]["telemetry"]["rx_bytes"], 4242);
        assert_eq!(bundle["status"]["web_broadcast"]["token"], "gui-ipc-secret");
        assert_eq!(
            bundle["status"]["web_broadcast"]["localhost_url"],
            "http://localhost:4717/#t=gui-ipc-secret"
        );
        assert_eq!(
            bundle["status"]["web_broadcast"]["urls"][0],
            "http://127.0.0.1:4717/#t=gui-ipc-secret"
        );
        assert_eq!(
            bundle["recent_logs"],
            serde_json::json!([
                "[daemon] memory warning",
                "android: streaming",
                "opened http://127.0.0.1:4717/#token=gui-ipc-secret"
            ])
        );
    }

    #[test]
    fn test_export_bug_report_falls_back_on_ipc_failure() {
        let (_dir, worker) = make_worker_with_temp_config("wb-ipc-fallback");
        let (bundle, _) = worker.collect_bug_report_bundle(&|_cmd, _payload| {
            Some(serde_json::json!({ "ok": false, "error": "daemon busy" }))
        });
        let serialized = serde_json::to_string(&bundle).unwrap();
        assert!(!serialized.contains("super-secret-hash-should-not-leak"));
        assert_eq!(bundle["config"]["has_psk_hash"], true);
        assert_eq!(bundle["status_ipc_error"], "daemon busy");
        assert!(bundle["status"].is_object());
        assert!(bundle["status"].get("ok").is_none());
        assert!(bundle["recent_logs"].is_array());
    }
}
