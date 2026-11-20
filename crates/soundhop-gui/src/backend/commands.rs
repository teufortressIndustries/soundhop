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
}
