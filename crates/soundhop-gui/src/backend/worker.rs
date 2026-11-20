//! Background worker state, polling refreshes, and Android/system/onboarding handlers.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use eframe::egui;
use soundhop_core::controller::Controller;
use soundhop_core::discovery::{discover_once_with_psk, Peer};
use soundhop_core::firewall::{detect as detect_firewall, open_ports};
use soundhop_core::ipc::{is_running as ipc_is_running, request as ipc_request};
use soundhop_core::service::{
    install as install_service, status as service_status, uninstall as uninstall_service,
};

use crate::{lock_or_recover, write_or_recover};

use super::commands::ipc_checked;
use super::snapshot::{GuiSnapshot, GuiToast, RefreshUpdate};

pub(super) struct WorkerState {
    pub(super) controller: Arc<Controller>,
    pub(super) snapshot: Arc<RwLock<GuiSnapshot>>,
    pub(super) toasts: Arc<Mutex<Vec<GuiToast>>>,
    pub(super) ctx: Option<egui::Context>,
    pub(super) cached_live_peers: Arc<Mutex<Vec<Peer>>>,
    pub(super) mutation_seq: Arc<AtomicU64>,
    pub(super) active_commands: Arc<AtomicUsize>,
    pub(super) onboarding_reopened: Arc<AtomicBool>,
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

    pub(super) fn refresh_android(&mut self) {
        let tools = self.controller.android_manager.get_tool_status();
        let devices = self.controller.android_manager.list_devices();
        let mut guard = write_or_recover(&self.snapshot);
        guard.android_tools = tools;
        guard.android_devices = devices;
    }

    pub(super) fn refresh_system(&mut self) {
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

    pub(super) fn refresh_fast(&mut self, allow_during_command: bool) {
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

        let update = RefreshUpdate::from_status(
            daemon_up,
            cfg,
            st,
            local_telem,
            &status_json,
            apps_json,
            &live_peers,
        );

        if !allow_during_command
            && (self.active_commands.load(Ordering::SeqCst) > 0
                || self.mutation_seq.load(Ordering::SeqCst) != seq_before)
        {
            return;
        }

        let mut guard = write_or_recover(&self.snapshot);
        update.apply_to(&mut guard, self.onboarding_reopened.load(Ordering::Relaxed));
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

    pub(super) fn handle_reopen_onboarding(&mut self) {
        self.onboarding_reopened.store(true, Ordering::SeqCst);
        write_or_recover(&self.snapshot).show_onboarding = true;
    }

    pub(super) fn apply_config_patch(&mut self, patch: serde_json::Value, toast: Option<String>) {
        let res = if ipc_is_running(None) {
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
            let (prev_security, prev_port) = {
                let cfg = lock_or_recover(&self.controller.config);
                (cfg.sharing.security.clone(), cfg.sharing.port)
            };
            let r = lock_or_recover(&self.controller.config).apply_patch(&patch);
            if r.is_ok() {
                self.controller.save();
                let (sharing_enabled, new_security, new_port) = {
                    let cfg = lock_or_recover(&self.controller.config);
                    (
                        cfg.sharing.enabled,
                        cfg.sharing.security.clone(),
                        cfg.sharing.port,
                    )
                };
                if (new_security != prev_security || new_port != prev_port)
                    && (sharing_enabled || self.controller.listener_running())
                {
                    let _ = self.controller.start_listener(Some(false));
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
