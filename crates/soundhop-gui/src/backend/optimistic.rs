//! Immediate optimistic snapshot updates applied on the UI thread when commands are queued.

use soundhop_core::util::split_host_port;

use super::commands::GuiCommand;
use super::snapshot::{AppStreamView, GuiSnapshot};

pub(super) fn apply_optimistic_update(snap: &mut GuiSnapshot, cmd: &GuiCommand) {
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
            snap.audio_profile = snap.config.audio.profile.clone();
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
        GuiCommand::SendPairRequest { host, .. } => {
            if !host.trim().is_empty() {
                snap.busy_message = Some(format!("Sending pair request to {}...", host.trim()));
            }
        }
        GuiCommand::ApprovePairRequest { request_id }
        | GuiCommand::DenyPairRequest { request_id } => {
            snap.pending_pair_requests
                .retain(|r| r.request_id != *request_id && r.sas_code != *request_id);
        }
        GuiCommand::RevokeTrustedPeer { device_id } => {
            snap.trusted_peers.retain(|p| p.device_id != *device_id);
            for peer in &mut snap.peers {
                if peer.id == *device_id {
                    peer.paired = false;
                }
            }
            snap.paired_peer_count = snap.trusted_peers.len();
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
        GuiCommand::RunDiagnostics { .. } => {
            snap.diagnostics_running = true;
            snap.busy_message = Some("Running diagnostics & fixes...".to_string());
        }
        GuiCommand::ExportBugReport => {
            snap.busy_message = Some("Collecting bug report...".to_string());
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
        GuiCommand::QuitAll => {
            snap.busy_message = Some("Quitting SoundHop...".to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize};
    use std::sync::{mpsc, Arc, Condvar, Mutex, RwLock};

    use super::super::tests::test_controller;
    use super::super::GuiBackend;
    use super::*;

    fn test_gui_backend(initial: GuiSnapshot) -> GuiBackend {
        let snapshot = Arc::new(RwLock::new(initial));
        let (tx, _rx) = mpsc::channel();
        GuiBackend {
            snapshot,
            tx: Some(tx),
            toasts: Arc::new(Mutex::new(Vec::new())),
            ctx: None,
            mutation_seq: Arc::new(AtomicU64::new(0)),
            active_commands: Arc::new(AtomicUsize::new(0)),
            stop_flag: Arc::new(AtomicBool::new(false)),
            quit_flag: Arc::new(AtomicBool::new(false)),
            poller_wake: Arc::new((Mutex::new(false), Condvar::new())),
            cmd_handle: None,
            poller_handle: None,
        }
    }

    #[test]
    fn test_optimistic_updates_on_send() {
        let ctrl = test_controller();
        let gui_backend = test_gui_backend(GuiSnapshot::from_controller(&ctrl));

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
            patch: serde_json::json!({ "ui": { "theme": "light" }, "audio": { "profile": "opus-low-latency" } }),
            toast: None,
        });
        assert_eq!(gui_backend.read().config.ui.theme, "light");
        assert_eq!(gui_backend.read().audio_profile, "opus-low-latency");

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
        let gui_backend = test_gui_backend(initial);

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
