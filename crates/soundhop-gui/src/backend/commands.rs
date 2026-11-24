//! GUI command definitions and core audio/connection/routing/config command dispatch.

use std::time::Duration;

use soundhop_core::constants::PROTOCOL_VERSION;
use soundhop_core::controller::ConnectOptions;
use soundhop_core::discovery::Peer;
use soundhop_core::ipc::{is_running as ipc_is_running, request as ipc_request};
use soundhop_core::util::{now_secs, split_host_port};

use crate::{lock_or_recover, write_or_recover};

use super::worker::WorkerState;

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
    SendPairRequest {
        host: String,
        port: Option<u16>,
        code: Option<String>,
    },
    ApprovePairRequest {
        request_id: String,
    },
    DenyPairRequest {
        request_id: String,
    },
    RevokeTrustedPeer {
        device_id: String,
    },
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
    RunDiagnostics {
        fix: bool,
    },
    ExportBugReport,
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
    QuitAll,
}

pub(super) fn is_mutating_command(cmd: &GuiCommand) -> bool {
    !matches!(
        cmd,
        GuiCommand::RefreshFast
            | GuiCommand::RefreshPeers
            | GuiCommand::RefreshAndroid
            | GuiCommand::RefreshSystem
    )
}

pub(super) fn check_ok_response(
    v: serde_json::Value,
    fallback: &str,
) -> Result<serde_json::Value, String> {
    if v.get("ok").and_then(|x| x.as_bool()).unwrap_or(true) {
        Ok(v)
    } else {
        Err(v
            .get("error")
            .and_then(|x| x.as_str())
            .unwrap_or(fallback)
            .to_string())
    }
}

pub(super) fn ipc_checked(
    cmd: &str,
    payload: serde_json::Value,
    timeout: Duration,
    fallback: &str,
) -> Result<serde_json::Value, String> {
    ipc_request(cmd, payload, None, timeout)
        .map_err(|e| e.to_string())
        .and_then(|v| check_ok_response(v, fallback))
}

impl WorkerState {
    fn finish_with_toast<T>(&mut self, res: Result<T, String>, ok_msg: impl Into<String>) {
        match res {
            Ok(_) => self.push_toast(ok_msg, false),
            Err(e) => self.push_toast(e, true),
        }
        self.refresh_fast(true);
    }

    pub(super) fn handle_command(&mut self, cmd: GuiCommand) {
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
            GuiCommand::ConnectPeer { peer_id } => self.handle_connect_peer(peer_id),
            GuiCommand::ConnectAddress { host_port } => self.handle_connect_address(host_port),
            GuiCommand::ConnectMulti {
                peer_ids,
                keep_local,
            } => self.handle_connect_multi(peer_ids, keep_local),
            GuiCommand::Disconnect => self.handle_disconnect(),
            GuiCommand::SetShare { enabled } => self.handle_set_share(enabled),
            GuiCommand::SetAutoConnect { enabled } => self.handle_set_auto_connect(enabled),
            GuiCommand::SetVolume { level } => self.handle_set_volume(level),
            GuiCommand::SetMute { muted } => self.handle_set_mute(muted),
            GuiCommand::RouteApp { app, target, save } => self.handle_route_app(app, target, save),
            GuiCommand::UnrouteApp { app, remove_rule } => {
                self.handle_unroute_app(app, remove_rule)
            }
            GuiCommand::SetDefaultRoutingMode { mode } => {
                self.apply_config_patch(
                    serde_json::json!({ "routing": { "default_mode": mode } }),
                    Some("Updated default app routing.".to_string()),
                );
            }
            GuiCommand::SetMic { direction } => self.handle_set_mic(direction),
            GuiCommand::SaveConfigPatch { patch, toast } => {
                self.apply_config_patch(patch, toast);
            }
            GuiCommand::ApplyPairingPin { pin } => self.handle_apply_pairing_pin(pin),
            GuiCommand::ClearPairing => self.handle_clear_pairing(),
            GuiCommand::SendPairRequest { host, port, code } => {
                self.handle_send_pair_request(host, port, code)
            }
            GuiCommand::ApprovePairRequest { request_id } => {
                self.handle_approve_pair_request(request_id)
            }
            GuiCommand::DenyPairRequest { request_id } => self.handle_deny_pair_request(request_id),
            GuiCommand::RevokeTrustedPeer { device_id } => {
                self.handle_revoke_trusted_peer(device_id)
            }
            GuiCommand::AndroidInstallTools => self.handle_android_install_tools(),
            GuiCommand::AndroidSwitchWifi { serial } => self.handle_android_switch_wifi(serial),
            GuiCommand::AndroidConnectWifi { ip_port } => self.handle_android_connect_wifi(ip_port),
            GuiCommand::AndroidDisconnectWifi { serial } => {
                self.handle_android_disconnect_wifi(serial)
            }
            GuiCommand::AndroidStart {
                serial,
                device_name,
                codec,
                source,
                buffer_ms,
                duplicate,
                as_mic,
            } => self.handle_android_start(
                serial,
                device_name,
                codec,
                source,
                buffer_ms,
                duplicate,
                as_mic,
            ),
            GuiCommand::AndroidStop => self.handle_android_stop(),
            GuiCommand::OpenFirewall => self.handle_open_firewall(),
            GuiCommand::RunDiagnostics { fix } => self.handle_run_diagnostics(fix),
            GuiCommand::ExportBugReport => self.handle_export_bug_report(),
            GuiCommand::SetAutostart { enabled } => self.handle_set_autostart(enabled),
            GuiCommand::CompleteOnboarding {
                device_name,
                enable_sharing,
                enable_autostart,
                open_firewall,
            } => self.handle_complete_onboarding(
                device_name,
                enable_sharing,
                enable_autostart,
                open_firewall,
            ),
            GuiCommand::ReopenOnboarding => self.handle_reopen_onboarding(),
            GuiCommand::QuitAll => self.handle_quit_all(),
        }
    }

    fn handle_connect_peer(&mut self, peer_id: String) {
        self.set_busy(Some("Connecting to speaker...".to_string()));
        let res = if ipc_is_running(None) {
            ipc_checked(
                "connect",
                serde_json::json!({ "peer": peer_id }),
                Duration::from_secs(10),
                "Could not connect",
            )
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
        self.finish_with_toast(res, "Connected to remote speaker.");
    }

    fn handle_connect_address(&mut self, host_port: String) {
        let (host, port) = split_host_port(host_port.trim(), 4713);
        if host.is_empty() {
            self.set_busy(None);
            self.push_toast("Enter an IP address or hostname first.", true);
            return;
        }
        self.set_busy(Some(format!("Connecting to {host}...")));
        let res = if ipc_is_running(None) {
            ipc_checked(
                "connect",
                serde_json::json!({ "host": host, "port": port }),
                Duration::from_secs(10),
                "Could not connect",
            )
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

    fn handle_connect_multi(&mut self, peer_ids: Vec<String>, keep_local: bool) {
        if peer_ids.is_empty() {
            self.set_busy(None);
            self.push_toast("Select at least one speaker.", true);
            return;
        }
        self.set_busy(Some("Starting multi-room audio...".to_string()));
        let res = if ipc_is_running(None) {
            ipc_checked(
                "connect",
                serde_json::json!({
                    "peers": peer_ids,
                    "keep_local": keep_local,
                }),
                Duration::from_secs(12),
                "Multi-room connection failed",
            )
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
        self.finish_with_toast(res, "Multi-room speakers connected.");
    }

    fn handle_disconnect(&mut self) {
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
        self.finish_with_toast(res, "Audio returned to this device.");
    }

    fn handle_set_share(&mut self, enabled: bool) {
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
        let msg = if enabled {
            "Others can now play audio on your speakers."
        } else {
            "Speaker sharing turned off."
        };
        self.finish_with_toast(res, msg);
    }

    fn handle_set_auto_connect(&mut self, enabled: bool) {
        let res = if ipc_is_running(None) {
            ipc_checked(
                "auto",
                serde_json::json!({ "action": if enabled { "on" } else { "off" } }),
                Duration::from_secs(3),
                "Failed to update auto-connect",
            )
            .map(|_| ())
        } else {
            let _sync = lock_or_recover(&self.controller_sync_lock);
            let v = self.controller.set_auto(enabled);
            check_ok_response(v, "Failed to update auto-connect").map(|_| ())
        };
        if let Err(e) = res {
            self.push_toast(format!("Failed to update auto-connect: {e}"), true);
        }
        self.refresh_fast(true);
    }

    fn handle_set_volume(&mut self, level: u32) {
        let res = if ipc_is_running(None) {
            ipc_checked(
                "volume",
                serde_json::json!({ "volume": level }),
                Duration::from_secs(2),
                "Failed to set volume",
            )
            .map(|_| ())
        } else {
            let _sync = lock_or_recover(&self.controller_sync_lock);
            let v = self.controller.set_volume(Some(level), None, true);
            check_ok_response(v, "Failed to set volume").map(|_| ())
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

    fn handle_set_mute(&mut self, muted: bool) {
        let res = if ipc_is_running(None) {
            ipc_checked(
                "volume",
                serde_json::json!({ "mute": muted }),
                Duration::from_secs(2),
                "Failed to update mute",
            )
            .map(|_| ())
        } else {
            let _sync = lock_or_recover(&self.controller_sync_lock);
            let v = self.controller.set_volume(None, Some(muted), true);
            check_ok_response(v, "Failed to update mute").map(|_| ())
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

    fn handle_route_app(&mut self, app: String, target: String, save: bool) {
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

    fn handle_unroute_app(&mut self, app: String, remove_rule: bool) {
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

    fn handle_set_mic(&mut self, direction: Option<String>) {
        let res = if ipc_is_running(None) {
            let payload = match direction.as_deref() {
                Some(dir) => serde_json::json!({ "action": "enable", "direction": dir }),
                None => serde_json::json!({ "action": "off" }),
            };
            ipc_checked(
                "mic",
                payload,
                Duration::from_secs(5),
                "Microphone update failed",
            )
        } else {
            let _sync = lock_or_recover(&self.controller_sync_lock);
            match direction.as_deref() {
                Some(dir) => self.controller.enable_mic(dir).map_err(|e| e.to_string()),
                None => self.controller.disable_mic().map_err(|e| e.to_string()),
            }
        };
        let msg = match direction.as_deref() {
            Some("send") => "Sending your microphone to the connected speaker.",
            Some("receive") => "Listening to the remote device's microphone.",
            _ => "Microphone sharing turned off.",
        };
        self.finish_with_toast(res, msg);
    }

    fn handle_apply_pairing_pin(&mut self, pin: String) {
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
        self.finish_with_toast(
            res,
            "Devices paired! Enter the same code on your other device.",
        );
    }

    fn handle_clear_pairing(&mut self) {
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
        self.finish_with_toast(res, "Pairing cleared. Switched back to Home Wi-Fi mode.");
    }

    fn handle_send_pair_request(&mut self, host: String, port: Option<u16>, code: Option<String>) {
        let trimmed = host.trim().to_string();
        if trimmed.is_empty() {
            self.set_busy(None);
            self.push_toast("Target device address is required to pair.", true);
            return;
        }
        let has_headless_code = code.as_deref().is_some_and(|c| !c.trim().is_empty());
        if has_headless_code {
            self.set_busy(Some(format!("Sending pair request to {trimmed}...")));
            let res = if self.use_ipc && ipc_is_running(None) {
                let mut payload = serde_json::json!({ "host": trimmed });
                if let Some(p) = port {
                    payload["port"] = serde_json::json!(p);
                }
                if let Some(ref c) = code {
                    if !c.trim().is_empty() {
                        payload["code"] = serde_json::json!(c.trim());
                    }
                }
                ipc_checked(
                    "pair_request",
                    payload,
                    Duration::from_secs(65),
                    "Pair request failed",
                )
                .map(|_| ())
            } else {
                let _sync = lock_or_recover(&self.controller_sync_lock);
                self.controller
                    .pair_request(&trimmed, port, code.as_deref())
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            };
            self.set_busy(None);
            self.finish_with_toast(res, format!("Paired with {trimmed}!"));
            return;
        }

        // Interactive Pairing v2: run on a background thread so `cmd_handle` remains free
        // to process the user's mutual SAS confirmation (`ApprovePairRequest` / `DenyPairRequest`)
        // while the Verification Code modal is shown on both devices.
        self.set_busy(Some(format!(
            "Connecting to {trimmed} for verification code..."
        )));
        let use_ipc = self.use_ipc;
        let controller = std::sync::Arc::clone(&self.controller);
        let snapshot = std::sync::Arc::clone(&self.snapshot);
        let toasts = std::sync::Arc::clone(&self.toasts);
        let ctx = self.ctx.clone();

        // Quick helper thread to surface the outgoing 6-digit SAS code modal within ~80ms
        let snap_poll = std::sync::Arc::clone(&self.snapshot);
        let ctrl_poll = std::sync::Arc::clone(&self.controller);
        let ctx_poll = self.ctx.clone();
        std::thread::spawn(move || {
            for delay_ms in [60_u64, 150, 350] {
                std::thread::sleep(Duration::from_millis(delay_ms));
                let pending = if use_ipc && ipc_is_running(None) {
                    ipc_request(
                        "pair_pending",
                        serde_json::json!({}),
                        None,
                        Duration::from_millis(800),
                    )
                    .ok()
                    .and_then(|v| v.get("requests").and_then(|a| a.as_array()).cloned())
                    .map(|arr| super::snapshot::parse_pending_pair_requests_json(&arr))
                    .unwrap_or_else(|| ctrl_poll.pending_pair_requests())
                } else {
                    ctrl_poll.pending_pair_requests()
                };
                if !pending.is_empty() {
                    let mut guard = write_or_recover(&snap_poll);
                    guard.pending_pair_requests = pending;
                    guard.busy_message = None;
                    drop(guard);
                    if let Some(ref c) = ctx_poll {
                        c.request_repaint();
                    }
                    break;
                }
            }
        });

        std::thread::spawn(move || {
            let res = if use_ipc && ipc_is_running(None) {
                let mut payload = serde_json::json!({ "host": trimmed });
                if let Some(p) = port {
                    payload["port"] = serde_json::json!(p);
                }
                ipc_checked(
                    "pair_request",
                    payload,
                    Duration::from_secs(65),
                    "Pair request failed",
                )
            } else {
                controller
                    .pair_request(&trimmed, port, None)
                    .map_err(|e| e.to_string())
            };

            let (toast_msg, is_err) = match res {
                Ok(val) => {
                    let peer_name = val
                        .get("peer")
                        .and_then(|p| p.get("display_name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or(&trimmed);
                    let sas = val.get("sas_code").and_then(|v| v.as_str()).unwrap_or("");
                    if sas.is_empty() {
                        (format!("Paired with {peer_name}!"), false)
                    } else {
                        (
                            format!("Paired with {peer_name} (code {sas} verified)!"),
                            false,
                        )
                    }
                }
                Err(e) => (e, true),
            };

            let trusted = if use_ipc && ipc_is_running(None) {
                ipc_request(
                    "pair_list",
                    serde_json::json!({}),
                    None,
                    Duration::from_millis(800),
                )
                .ok()
                .and_then(|v| v.get("trusted_peers").and_then(|a| a.as_array()).cloned())
                .map(|arr| super::snapshot::parse_trusted_peers_json(&arr))
                .unwrap_or_else(|| controller.trusted_peers())
            } else {
                controller.trusted_peers()
            };
            let pending = if use_ipc && ipc_is_running(None) {
                ipc_request(
                    "pair_pending",
                    serde_json::json!({}),
                    None,
                    Duration::from_millis(800),
                )
                .ok()
                .and_then(|v| v.get("requests").and_then(|a| a.as_array()).cloned())
                .map(|arr| super::snapshot::parse_pending_pair_requests_json(&arr))
                .unwrap_or_else(|| controller.pending_pair_requests())
            } else {
                controller.pending_pair_requests()
            };

            {
                let mut guard = write_or_recover(&snapshot);
                guard.busy_message = None;
                guard.paired_peer_count = trusted.len();
                let trusted_ids: std::collections::HashSet<String> =
                    trusted.iter().map(|t| t.device_id.clone()).collect();
                for peer in &mut guard.peers {
                    if trusted_ids.contains(&peer.id) {
                        peer.paired = true;
                    }
                }
                guard.trusted_peers = trusted;
                guard.pending_pair_requests = pending;
            }
            {
                let mut t_guard = lock_or_recover(&toasts);
                t_guard.push(super::snapshot::GuiToast {
                    message: toast_msg,
                    is_error: is_err,
                    created_at: std::time::Instant::now(),
                });
                if t_guard.len() > 3 {
                    let drain = t_guard.len() - 3;
                    t_guard.drain(0..drain);
                }
            }
            if let Some(ref c) = ctx {
                c.request_repaint();
            }
        });
    }

    fn handle_approve_pair_request(&mut self, request_id: String) {
        let is_outgoing = self
            .controller
            .pending_pair_requests()
            .iter()
            .any(|r| r.request_id == request_id && r.outgoing);
        let res = if self.use_ipc && ipc_is_running(None) {
            ipc_checked(
                "pair_approve",
                serde_json::json!({ "request_id": request_id }),
                Duration::from_secs(5),
                "Failed to approve pair request",
            )
            .map(|v| {
                let outgoing_ipc = v
                    .get("request")
                    .and_then(|r| r.get("outgoing"))
                    .and_then(|b| b.as_bool())
                    .unwrap_or(is_outgoing);
                outgoing_ipc
            })
        } else {
            let _sync = lock_or_recover(&self.controller_sync_lock);
            self.controller
                .approve_pair_request(&request_id)
                .map(|_| is_outgoing)
                .map_err(|e| e.to_string())
        };
        match res {
            Ok(true) => {
                self.push_toast(
                    "Code confirmed on this device — make sure it is also approved on the other device.",
                    false,
                );
                self.refresh_fast(true);
            }
            Ok(false) => {
                self.finish_with_toast(Ok(()), "Device pairing approved.");
            }
            Err(e) => {
                self.finish_with_toast(Err::<(), _>(e), "Device pairing approved.");
            }
        }
    }

    fn handle_deny_pair_request(&mut self, request_id: String) {
        let res = if self.use_ipc && ipc_is_running(None) {
            ipc_checked(
                "pair_deny",
                serde_json::json!({ "request_id": request_id }),
                Duration::from_secs(5),
                "Failed to deny pair request",
            )
            .map(|_| ())
        } else {
            let _sync = lock_or_recover(&self.controller_sync_lock);
            self.controller
                .deny_pair_request(&request_id)
                .map(|_| ())
                .map_err(|e| e.to_string())
        };
        self.finish_with_toast(res, "Device pairing request denied.");
    }

    fn handle_revoke_trusted_peer(&mut self, device_id: String) {
        let res = if self.use_ipc && ipc_is_running(None) {
            ipc_checked(
                "pair_revoke",
                serde_json::json!({ "device_id": device_id }),
                Duration::from_secs(5),
                "Failed to revoke trusted device",
            )
            .map(|_| ())
        } else {
            let _sync = lock_or_recover(&self.controller_sync_lock);
            self.controller
                .revoke_trusted_peer(&device_id)
                .map(|_| ())
                .map_err(|e| e.to_string())
        };
        self.finish_with_toast(res, "Trusted device revoked.");
    }

    fn handle_quit_all(&mut self) {
        let _ = self.controller.android_session.stop();
        let _ = soundhop_core::service::quit_all_soundhop(None);

        self.quit_flag
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(ref ctx) = self.ctx {
            ctx.send_viewport_cmd(eframe::egui::ViewportCommand::Close);
            ctx.request_repaint();
        }
    }
}
