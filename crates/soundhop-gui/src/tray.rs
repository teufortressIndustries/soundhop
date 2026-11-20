//! Cross-platform Native System Tray (`tray-icon` + `muda`) with dynamic RGBA status icons,
//! Multi-Room peer toggles, per-app routing submenus, mic pass-through, volume presets, and
//! child dialog launchers.

use std::collections::HashSet;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, OnceLock};
use std::time::{Duration, Instant};

use crate::lock_or_recover;

use muda::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use soundhop_core::constants::{PROTOCOL_VERSION, VERSION};
use soundhop_core::controller::{ConnectOptions, Controller};
use soundhop_core::daemon::Daemon;
use soundhop_core::discovery::Peer;
use soundhop_core::ipc::{
    find_soundhop_process_with_arg, is_running as ipc_is_running, ping as ipc_ping,
    request as ipc_request,
};
use soundhop_core::pairing::PendingPairRequest;
use soundhop_core::service::sync_appimage_paths_if_installed;
use soundhop_core::util::{
    is_own_non_loopback_ip, is_private_address, now_secs, replace_stale_instance, InstanceLockInfo,
};
use tray_icon::{Icon, TrayIconBuilder, TrayIconEvent};

const EMBEDDED_PNG: &[u8] = include_bytes!("../../../assets/soundhop.png");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayBadgeState {
    Idle,
    Sharing,
    Connected,
    MultiRoom,
    Error,
}

fn base_tray_rgba() -> &'static [u8] {
    static BASE_ICON_RGBA: OnceLock<Vec<u8>> = OnceLock::new();
    BASE_ICON_RGBA.get_or_init(|| {
        let mut rgba = vec![0u8; 64 * 64 * 4];
        if let Ok(img) = image::load_from_memory(EMBEDDED_PNG) {
            let resized = img.resize_exact(64, 64, image::imageops::FilterType::Lanczos3);
            let raw = resized.to_rgba8();
            rgba.copy_from_slice(raw.as_raw());
        } else {
            for y in 0..64 {
                for x in 0..64 {
                    let dx = x as i32 - 32;
                    let dy = y as i32 - 32;
                    if dx * dx + dy * dy <= 28 * 28 {
                        let idx = (y * 64 + x) * 4;
                        rgba[idx] = 45;
                        rgba[idx + 1] = 135;
                        rgba[idx + 2] = 245;
                        rgba[idx + 3] = 255;
                    }
                }
            }
        }
        rgba
    })
}

/// Generates a 64x64 RGBA tray icon with a colored status badge in the bottom-right corner.
pub fn build_tray_icon(state: TrayBadgeState) -> Icon {
    let mut rgba = base_tray_rgba().to_vec();

    let badge_color: Option<[u8; 4]> = match state {
        TrayBadgeState::Idle => None,
        TrayBadgeState::Sharing => Some([50, 160, 250, 255]), // Blue
        TrayBadgeState::Connected => Some([46, 204, 113, 255]), // Green
        TrayBadgeState::MultiRoom => Some([165, 94, 234, 255]), // Purple
        TrayBadgeState::Error => Some([231, 76, 60, 255]),    // Red
    };

    if let Some(color) = badge_color {
        let cx = 50i32;
        let cy = 50i32;
        let r_inner = 10i32;
        let r_outer = 12i32;
        for y in 36..64 {
            for x in 36..64 {
                let dx = x as i32 - cx;
                let dy = y as i32 - cy;
                let d2 = dx * dx + dy * dy;
                let idx = (y * 64 + x) * 4;
                if d2 <= r_inner * r_inner {
                    rgba[idx] = color[0];
                    rgba[idx + 1] = color[1];
                    rgba[idx + 2] = color[2];
                    rgba[idx + 3] = color[3];
                } else if d2 <= r_outer * r_outer {
                    rgba[idx] = 20;
                    rgba[idx + 1] = 24;
                    rgba[idx + 2] = 30;
                    rgba[idx + 3] = 255;
                }
            }
        }
    }

    Icon::from_rgba(rgba, 64, 64).expect("Valid 64x64 RGBA icon")
}

fn is_external_daemon(daemon: &Daemon) -> bool {
    lock_or_recover(&daemon.ipc).is_none() && ipc_is_running(None)
}

fn daemon_peers(daemon: &Daemon) -> Vec<Peer> {
    let mut peers: Vec<Peer> = lock_or_recover(&daemon.discovery)
        .as_ref()
        .map(|d| d.peers())
        .unwrap_or_default();
    let mut seen_ids: HashSet<String> = peers.iter().map(|p| p.id.clone()).collect();
    let own_id = lock_or_recover(&daemon.controller.config).device.id.clone();

    if is_external_daemon(daemon) {
        if let Ok(resp) = ipc_request(
            "peers",
            serde_json::json!({}),
            None,
            Duration::from_millis(400),
        ) {
            if let Some(arr) = resp.get("peers").and_then(|v| v.as_array()) {
                for item in arr {
                    let id = item
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let host = item
                        .get("host")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    if id.is_empty()
                        || id == own_id
                        || seen_ids.contains(&id)
                        || !is_private_address(&host)
                        || is_own_non_loopback_ip(&host)
                    {
                        continue;
                    }
                    let name = item
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&host)
                        .to_string();
                    let port = item.get("port").and_then(|v| v.as_u64()).unwrap_or(4713) as u16;
                    let version = item
                        .get("version")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let protocol =
                        item.get("protocol")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(u64::from(PROTOCOL_VERSION)) as u32;
                    let security = item
                        .get("security")
                        .and_then(|v| v.as_str())
                        .unwrap_or("trusted-lan")
                        .to_string();
                    let platform = item
                        .get("platform")
                        .and_then(|v| v.as_str())
                        .unwrap_or("linux")
                        .to_string();
                    let rtt_ms = item.get("rtt_ms").and_then(|v| v.as_f64());
                    seen_ids.insert(id.clone());
                    peers.push(Peer {
                        id,
                        name,
                        host,
                        port,
                        version,
                        protocol,
                        security,
                        platform,
                        last_seen: now_secs(),
                        source: "ipc".to_string(),
                        rtt_ms,
                    });
                }
            }
        }
    }

    let st = lock_or_recover(&daemon.controller.state).clone();
    for kp in st.known_peers.values() {
        if kp.id.is_empty()
            || kp.id == own_id
            || seen_ids.contains(&kp.id)
            || !is_private_address(&kp.host)
            || is_own_non_loopback_ip(&kp.host)
        {
            continue;
        }
        seen_ids.insert(kp.id.clone());
        peers.push(Peer {
            id: kp.id.clone(),
            name: kp.name.clone(),
            host: kp.host.clone(),
            port: kp.port,
            version: kp.version.clone(),
            protocol: PROTOCOL_VERSION,
            security: kp.security.clone(),
            platform: kp.platform.clone(),
            last_seen: kp.last_seen,
            source: "remembered".to_string(),
            rtt_ms: daemon
                .controller
                .telemetry
                .peer_stats(&kp.id)
                .map(|n| n.last_rtt_ms),
        });
    }

    peers
}

pub fn build_tray_menu(controller: &Controller, peers: &[Peer]) -> Menu {
    let st = lock_or_recover(&controller.state).clone();
    let cfg = lock_or_recover(&controller.config).clone();
    let streams = controller.list_apps();
    let and_running = controller.android_session.is_active();
    let pending_pair_requests = controller.pending_pair_requests();
    let snap = TrayMenuSnapshot::from_parts(
        peers.to_vec(),
        streams,
        st,
        cfg,
        and_running,
        pending_pair_requests,
    );
    build_tray_menu_from_snapshot(&snap)
}

#[derive(Clone)]
struct TrayMenuSnapshot {
    peers: Vec<Peer>,
    streams: Vec<serde_json::Value>,
    connected: bool,
    multiroom: bool,
    connected_peer: String,
    connected_name: String,
    connected_host: String,
    multiroom_peer_ids: Vec<String>,
    keep_local: bool,
    sharing_enabled: bool,
    auto_enabled: bool,
    mic_active: bool,
    mic_direction: String,
    routed_apps_lower: HashSet<String>,
    android_active: bool,
    pending_pair_requests: Vec<PendingPairRequest>,
    badge: TrayBadgeState,
    signature: String,
}

impl TrayMenuSnapshot {
    fn from_parts(
        peers: Vec<Peer>,
        streams: Vec<serde_json::Value>,
        st: soundhop_core::state::State,
        cfg: soundhop_core::config::Config,
        android_active: bool,
        pending_pair_requests: Vec<PendingPairRequest>,
    ) -> Self {
        let connected = st.is_connected();
        let multiroom = st.is_multiroom();
        let multiroom_peer_ids: Vec<String> = st
            .multiroom_peers
            .iter()
            .map(|s| s.peer_id.clone())
            .collect();
        let sharing_enabled = cfg.sharing.enabled || st.listener_module > 0;
        let auto_enabled = cfg.connection.auto_connect && !st.auto_paused;
        let mic_active = !st.connected_source.is_empty();
        let routed_apps_lower: HashSet<String> = st.routed_apps.keys().cloned().collect();
        let badge = if multiroom {
            TrayBadgeState::MultiRoom
        } else if connected {
            TrayBadgeState::Connected
        } else if sharing_enabled {
            TrayBadgeState::Sharing
        } else {
            TrayBadgeState::Idle
        };
        let signature = compute_menu_signature(
            &peers,
            &streams,
            &st.connected_peer,
            &st.connected_name,
            &multiroom_peer_ids,
            sharing_enabled,
            auto_enabled,
            st.keep_local,
            mic_active,
            &st.mic_direction,
            android_active,
            &pending_pair_requests,
        );
        Self {
            peers,
            streams,
            connected,
            multiroom,
            connected_peer: st.connected_peer,
            connected_name: st.connected_name,
            connected_host: st.connected_host,
            multiroom_peer_ids,
            keep_local: st.keep_local,
            sharing_enabled,
            auto_enabled,
            mic_active,
            mic_direction: st.mic_direction,
            routed_apps_lower,
            android_active,
            pending_pair_requests,
            badge,
            signature,
        }
    }
}

fn build_tray_menu_from_snapshot(snap: &TrayMenuSnapshot) -> Menu {
    let menu = Menu::new();

    if !snap.pending_pair_requests.is_empty() {
        let count = snap.pending_pair_requests.len();
        let title = if count == 1 {
            format!(
                "Pair Request: {} (Code: {})",
                snap.pending_pair_requests[0].display_name, snap.pending_pair_requests[0].sas_code
            )
        } else {
            format!("Pending Pair Requests ({count})")
        };
        let pair_sub = Submenu::new(title, true);
        for req in &snap.pending_pair_requests {
            let info_item = MenuItem::with_id(
                MenuId::new(format!("noop_pair:{}", req.request_id)),
                format!("{} — Code: {}", req.display_name, req.sas_code),
                false,
                None,
            );
            let _ = pair_sub.append(&info_item);
            let approve_item = MenuItem::with_id(
                MenuId::new(format!("pair_approve:{}", req.request_id)),
                format!("Approve {}", req.display_name),
                true,
                None,
            );
            let _ = pair_sub.append(&approve_item);
            let deny_item = MenuItem::with_id(
                MenuId::new(format!("pair_deny:{}", req.request_id)),
                format!("Deny {}", req.display_name),
                true,
                None,
            );
            let _ = pair_sub.append(&deny_item);
            let _ = pair_sub.append(&PredefinedMenuItem::separator());
        }
        let review_item =
            MenuItem::with_id(MenuId::new("open_gui"), "Review in SoundHop...", true, None);
        let _ = pair_sub.append(&review_item);
        let _ = menu.append(&pair_sub);
        let _ = menu.append(&PredefinedMenuItem::separator());
    }

    let open_item = MenuItem::with_id(MenuId::new("open_gui"), "Open SoundHop", true, None);
    let _ = menu.append(&open_item);

    let status_text = if snap.multiroom {
        format!("Multi-room: {}", snap.connected_name)
    } else if snap.connected {
        format!("Playing on: {}", snap.connected_name)
    } else if snap.sharing_enabled {
        "Ready • Sharing my speakers".to_string()
    } else {
        "Ready • Playing on this device".to_string()
    };
    let header_item = MenuItem::with_id(MenuId::new("noop_header"), status_text, false, None);
    let _ = menu.append(&header_item);
    let _ = menu.append(&PredefinedMenuItem::separator());

    let connected_ids: HashSet<String> = if !snap.multiroom_peer_ids.is_empty() {
        snap.multiroom_peer_ids.iter().cloned().collect()
    } else if !snap.connected_peer.is_empty() {
        let mut s = HashSet::new();
        s.insert(snap.connected_peer.clone());
        s
    } else {
        HashSet::new()
    };

    let play_header = MenuItem::with_id(MenuId::new("noop_play_on"), "Play Audio On", false, None);
    let _ = menu.append(&play_header);

    let local_chk = CheckMenuItem::with_id(
        MenuId::new("action_disconnect"),
        "This Device (Local Speakers)",
        snap.connected,
        !snap.connected,
        None,
    );
    let _ = menu.append(&local_chk);

    if snap.peers.is_empty() {
        let no_peers = MenuItem::with_id(
            MenuId::new("noop_peers"),
            "Looking for nearby speakers...",
            false,
            None,
        );
        let _ = menu.append(&no_peers);
    } else {
        for peer in &snap.peers {
            let is_conn = connected_ids.contains(&peer.id)
                || (snap.connected_host == peer.host && !snap.connected_peer.is_empty());
            let rtt_suffix = peer
                .rtt_ms
                .map(|r| format!(" ({r:.0} ms)"))
                .unwrap_or_default();
            let label = format!("{}{}", peer.name, rtt_suffix);
            let item = CheckMenuItem::with_id(
                MenuId::new(format!("peer_toggle:{}", peer.id)),
                label,
                true,
                is_conn,
                None,
            );
            let _ = menu.append(&item);
        }

        if snap.peers.len() > 1 {
            let multi_sub = Submenu::new("Multiple Speakers", true);
            for peer in &snap.peers {
                let is_in_group = connected_ids.contains(&peer.id);
                let chk = CheckMenuItem::with_id(
                    MenuId::new(format!("multiroom_toggle:{}", peer.id)),
                    format!("Include {}", peer.name),
                    true,
                    is_in_group,
                    None,
                );
                let _ = multi_sub.append(&chk);
            }
            let _ = multi_sub.append(&PredefinedMenuItem::separator());
            let keep_local_chk = CheckMenuItem::with_id(
                MenuId::new("multiroom_keep_local"),
                "Keep Playing on This Device Too",
                snap.multiroom && !snap.multiroom_peer_ids.is_empty(),
                snap.keep_local,
                None,
            );
            let _ = multi_sub.append(&keep_local_chk);
            let broadcast_all = MenuItem::with_id(
                MenuId::new("multiroom_broadcast_all"),
                "Play on All Speakers",
                true,
                None,
            );
            let _ = multi_sub.append(&broadcast_all);
            let _ = menu.append(&multi_sub);
        }
    }

    let _ = menu.append(&PredefinedMenuItem::separator());

    let share_chk = CheckMenuItem::with_id(
        MenuId::new("toggle_share"),
        "Share My Speakers",
        true,
        snap.sharing_enabled,
        None,
    );
    let _ = menu.append(&share_chk);

    let auto_chk = CheckMenuItem::with_id(
        MenuId::new("toggle_autoconnect"),
        "Auto-Connect to Last Speaker",
        true,
        snap.auto_enabled,
        None,
    );
    let _ = menu.append(&auto_chk);

    // Apps submenu (plain labels, no & mnemonics)
    let app_sub = Submenu::new("Apps", true);
    if snap.streams.is_empty() {
        #[cfg(target_os = "windows")]
        let empty_label = "System-wide audio (WASAPI)";
        #[cfg(not(target_os = "windows"))]
        let empty_label = "No apps playing right now";
        let empty = MenuItem::with_id(MenuId::new("noop_apps"), empty_label, false, None);
        let _ = app_sub.append(&empty);
    } else {
        for stream in &snap.streams {
            let app_name = stream
                .get("app_name")
                .and_then(|v| v.as_str())
                .unwrap_or("App");
            let media_name = stream
                .get("media_name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let is_routed = stream
                .get("is_remote")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
                || stream
                    .get("routed_rule")
                    .and_then(|v| v.as_str())
                    .map(|r| r == "remote")
                    .unwrap_or(false);
            let label = if media_name.is_empty() {
                app_name.to_string()
            } else {
                format!("{app_name} - {media_name}")
            };
            let chk = CheckMenuItem::with_id(
                MenuId::new(format!("app_route:{app_name}")),
                label,
                snap.connected,
                is_routed,
                None,
            );
            let _ = app_sub.append(&chk);
        }
    }
    let _ = menu.append(&app_sub);

    // Volume submenu (avoid '&' which GTK treats as mnemonic underline)
    let vol_sub = Submenu::new("Volume and Mute", snap.connected);
    for pct in [25u32, 50, 75, 100, 125] {
        let item = MenuItem::with_id(
            MenuId::new(format!("vol_set:{pct}")),
            format!("Volume {pct}%"),
            snap.connected,
            None,
        );
        let _ = vol_sub.append(&item);
    }
    let _ = vol_sub.append(&PredefinedMenuItem::separator());
    let mute_item = MenuItem::with_id(
        MenuId::new("vol_mute_toggle"),
        "Toggle Mute",
        snap.connected,
        None,
    );
    let _ = vol_sub.append(&mute_item);
    let _ = menu.append(&vol_sub);

    // Microphone submenu (Linux PulseAudio/PipeWire pass-through only)
    #[cfg(not(target_os = "windows"))]
    {
        let mic_sub = Submenu::new("Microphone", true);
        let mic_send = CheckMenuItem::with_id(
            MenuId::new("mic_send_toggle"),
            "Send My Microphone",
            snap.connected || snap.mic_active,
            snap.mic_active && snap.mic_direction == "send",
            None,
        );
        let mic_recv = CheckMenuItem::with_id(
            MenuId::new("mic_recv_toggle"),
            "Hear Peer Microphone",
            snap.connected || snap.mic_active,
            snap.mic_active && snap.mic_direction == "receive",
            None,
        );
        let _ = mic_sub.append(&mic_send);
        let _ = mic_sub.append(&mic_recv);
        if snap.mic_active {
            let mic_off =
                MenuItem::with_id(MenuId::new("mic_stop"), "Turn Off Microphone", true, None);
            let _ = mic_sub.append(&mic_off);
        }
        let _ = menu.append(&mic_sub);
    }

    // Phone Audio action
    let and_wiz = MenuItem::with_id(
        MenuId::new("open_android_wizard"),
        "Phone Audio...",
        true,
        None,
    );
    let _ = menu.append(&and_wiz);
    if snap.android_active {
        let and_stop =
            MenuItem::with_id(MenuId::new("android_stop"), "Stop Phone Audio", true, None);
        let _ = menu.append(&and_stop);
    }

    let _ = menu.append(&PredefinedMenuItem::separator());

    let settings_item =
        MenuItem::with_id(MenuId::new("open_settings"), "Preferences...", true, None);
    let _ = menu.append(&settings_item);

    let quit_item = MenuItem::with_id(MenuId::new("quit_tray"), "Quit SoundHop", true, None);
    let _ = menu.append(&quit_item);

    menu
}

fn format_tray_tooltip(snap: &TrayMenuSnapshot) -> String {
    let raw = if !snap.pending_pair_requests.is_empty() {
        let count = snap.pending_pair_requests.len();
        if count == 1 {
            format!(
                "SoundHop — Pair request from {} (Code: {})",
                snap.pending_pair_requests[0].display_name, snap.pending_pair_requests[0].sas_code
            )
        } else {
            format!("SoundHop — {count} pending pair requests")
        }
    } else if snap.multiroom && !snap.multiroom_peer_ids.is_empty() {
        if !snap.connected_name.is_empty() {
            format!("SoundHop — Streaming to {}", snap.connected_name)
        } else {
            format!(
                "SoundHop — Streaming to {} speakers",
                snap.multiroom_peer_ids.len()
            )
        }
    } else if snap.connected {
        if !snap.connected_name.is_empty() {
            format!("SoundHop — Streaming to {}", snap.connected_name)
        } else {
            "SoundHop — Streaming".to_string()
        }
    } else if snap.android_active {
        "SoundHop — Receiving phone audio".to_string()
    } else if snap.sharing_enabled {
        "SoundHop — Sharing speakers".to_string()
    } else {
        "SoundHop — Idle".to_string()
    };

    // Windows NOTIFYICONDATAW.szTip supports up to 127 UTF-16 code units.
    const MAX_TOOLTIP_CHARS: usize = 120;
    if raw.chars().count() <= MAX_TOOLTIP_CHARS {
        raw
    } else {
        let truncated: String = raw.chars().take(MAX_TOOLTIP_CHARS - 3).collect();
        format!("{truncated}...")
    }
}

fn resolve_gui_executable_from(current_exe: &std::path::Path) -> std::path::PathBuf {
    #[cfg(target_os = "windows")]
    {
        if let Some(dir) = current_exe.parent() {
            let gui_exe = dir.join("soundhop-gui.exe");
            if gui_exe.is_file() {
                return gui_exe;
            }
        }
    }
    current_exe.to_path_buf()
}

fn spawn_self_args(args: &[&str]) {
    if let Ok(current_exe) = std::env::current_exe() {
        let exe = resolve_gui_executable_from(&current_exe);
        let mut cmd = Command::new(exe);
        cmd.args(args);
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        soundhop_core::util::close_fds_pre_exec(&mut cmd);
        if let Ok(mut child) = cmd.spawn() {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

fn collect_tray_snapshot(daemon: &Daemon) -> TrayMenuSnapshot {
    let peers = daemon_peers(daemon);
    if is_external_daemon(daemon) {
        let status_json = ipc_request(
            "status",
            serde_json::json!({}),
            None,
            Duration::from_millis(600),
        )
        .ok();
        let apps = ipc_request(
            "apps",
            serde_json::json!({}),
            None,
            Duration::from_millis(500),
        )
        .ok()
        .and_then(|v| v.get("apps").and_then(|a| a.as_array()).cloned())
        .unwrap_or_default();
        let pending_pair_requests: Vec<PendingPairRequest> = ipc_request(
            "pair_pending",
            serde_json::json!({}),
            None,
            Duration::from_millis(400),
        )
        .ok()
        .and_then(|v| v.get("requests").and_then(|a| a.as_array()).cloned())
        .map(|arr| {
            arr.into_iter()
                .filter_map(|item| serde_json::from_value::<PendingPairRequest>(item).ok())
                .collect()
        })
        .unwrap_or_default();

        if let Some(st_json) = status_json {
            let conn = st_json.get("connection").filter(|c| !c.is_null());
            let connected = conn.is_some();
            let multiroom = conn
                .and_then(|c| c.get("multiroom"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let connected_peer = conn
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
            let keep_local = conn
                .and_then(|c| c.get("keep_local"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let multiroom_peer_ids: Vec<String> = conn
                .and_then(|c| c.get("multiroom_peers"))
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|p| {
                            p.get("peer_id")
                                .and_then(|x| x.as_str())
                                .map(|s| s.to_string())
                        })
                        .collect()
                })
                .unwrap_or_default();
            let sharing_enabled = st_json
                .get("sharing")
                .and_then(|s| s.get("enabled"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let auto_enabled = st_json
                .get("auto")
                .and_then(|a| a.get("enabled"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
                && !st_json
                    .get("auto")
                    .and_then(|a| a.get("paused"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
            let mic_active = conn
                .and_then(|c| c.get("with_mic"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let mic_direction = conn
                .and_then(|c| c.get("mic_direction"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let android_active = st_json
                .get("android")
                .and_then(|a| a.get("active"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let routed_apps_lower: HashSet<String> = apps
                .iter()
                .filter(|a| {
                    a.get("is_remote")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                        || a.get("routed_rule")
                            .and_then(|v| v.as_str())
                            .map(|r| r == "remote")
                            .unwrap_or(false)
                })
                .filter_map(|a| {
                    a.get("app_name")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_lowercase())
                })
                .collect();
            let badge = if multiroom {
                TrayBadgeState::MultiRoom
            } else if connected {
                TrayBadgeState::Connected
            } else if sharing_enabled {
                TrayBadgeState::Sharing
            } else {
                TrayBadgeState::Idle
            };
            let signature = compute_menu_signature(
                &peers,
                &apps,
                &connected_peer,
                &connected_name,
                &multiroom_peer_ids,
                sharing_enabled,
                auto_enabled,
                keep_local,
                mic_active,
                &mic_direction,
                android_active,
                &pending_pair_requests,
            );
            return TrayMenuSnapshot {
                peers,
                streams: apps,
                connected,
                multiroom,
                connected_peer,
                connected_name,
                connected_host,
                multiroom_peer_ids,
                keep_local,
                sharing_enabled,
                auto_enabled,
                mic_active,
                mic_direction,
                routed_apps_lower,
                android_active,
                pending_pair_requests,
                badge,
                signature,
            };
        }
    }

    let ctrl = &daemon.controller;
    let st = lock_or_recover(&ctrl.state).clone();
    let cfg = lock_or_recover(&ctrl.config).clone();
    let streams = ctrl.list_apps();
    let and_running = ctrl.android_session.is_active();
    let pending_pair_requests = ctrl.pending_pair_requests();
    TrayMenuSnapshot::from_parts(peers, streams, st, cfg, and_running, pending_pair_requests)
}

fn handle_menu_action(id_str: &str, daemon: &Daemon, running: &AtomicBool) {
    let ctrl = &daemon.controller;
    let external = is_external_daemon(daemon);

    if id_str == "quit_tray" {
        running.store(false, Ordering::SeqCst);
        return;
    }
    if id_str == "open_gui" {
        spawn_self_args(&["--gui-tab", "home"]);
        return;
    }
    if id_str == "open_settings" {
        spawn_self_args(&["--gui-tab", "settings"]);
        return;
    }
    if id_str == "open_android_wizard" {
        spawn_self_args(&["--gui-tab", "phone"]);
        return;
    }
    if let Some(req_id) = id_str.strip_prefix("pair_approve:") {
        if external {
            let _ = ipc_request(
                "pair_approve",
                serde_json::json!({ "request_id": req_id }),
                None,
                Duration::from_secs(5),
            );
        } else {
            let _ = ctrl.approve_pair_request(req_id);
        }
        return;
    }
    if let Some(req_id) = id_str.strip_prefix("pair_deny:") {
        if external {
            let _ = ipc_request(
                "pair_deny",
                serde_json::json!({ "request_id": req_id }),
                None,
                Duration::from_secs(5),
            );
        } else {
            let _ = ctrl.deny_pair_request(req_id);
        }
        return;
    }
    if id_str == "android_stop" {
        if external {
            let _ = ipc_request(
                "android_stop",
                serde_json::json!({}),
                None,
                Duration::from_secs(5),
            );
        } else {
            let _ = ctrl.android_session.stop();
        }
        return;
    }
    if id_str == "action_disconnect" {
        if external {
            let _ = ipc_request(
                "disconnect",
                serde_json::json!({}),
                None,
                Duration::from_secs(6),
            );
        } else {
            let _ = ctrl.disconnect(false, None, true, Some(true), false, false);
        }
        return;
    }
    if id_str == "toggle_share" {
        if external {
            let currently_enabled = ipc_request(
                "status",
                serde_json::json!({}),
                None,
                Duration::from_millis(600),
            )
            .ok()
            .and_then(|v| {
                v.get("sharing")
                    .and_then(|s| s.get("enabled"))
                    .and_then(|b| b.as_bool())
            })
            .unwrap_or_else(|| lock_or_recover(&ctrl.config).sharing.enabled);
            let _ = ipc_request(
                "share",
                serde_json::json!({ "enabled": !currently_enabled }),
                None,
                Duration::from_secs(5),
            );
        } else {
            let currently_enabled = lock_or_recover(&ctrl.config).sharing.enabled;
            let next_enabled = !currently_enabled;
            let _ = ctrl.share(next_enabled, Some(true));
            if let Some(disc) = lock_or_recover(&daemon.discovery).as_ref() {
                disc.set_advertising(next_enabled);
            }
        }
        return;
    }
    if id_str == "toggle_autoconnect" {
        if external {
            let cur = ipc_request(
                "status",
                serde_json::json!({}),
                None,
                Duration::from_millis(600),
            )
            .ok()
            .and_then(|v| {
                v.get("auto")
                    .and_then(|a| a.get("enabled"))
                    .and_then(|b| b.as_bool())
            })
            .unwrap_or_else(|| lock_or_recover(&ctrl.config).connection.auto_connect);
            let _ = ipc_request(
                "auto",
                serde_json::json!({ "action": if cur { "off" } else { "on" } }),
                None,
                Duration::from_secs(3),
            );
        } else {
            let cur = lock_or_recover(&ctrl.config).connection.auto_connect;
            let _ = ctrl.set_auto(!cur);
        }
        return;
    }

    let peers = daemon_peers(daemon);

    if let Some(peer_id) = id_str.strip_prefix("peer_toggle:") {
        if external {
            let snap = collect_tray_snapshot(daemon);
            if snap.multiroom && snap.multiroom_peer_ids.iter().any(|id| id == peer_id) {
                let remaining: Vec<String> = snap
                    .multiroom_peer_ids
                    .into_iter()
                    .filter(|id| id != peer_id)
                    .collect();
                if remaining.is_empty() {
                    let _ = ipc_request(
                        "disconnect",
                        serde_json::json!({}),
                        None,
                        Duration::from_secs(6),
                    );
                } else {
                    let _ = ipc_request(
                        "connect",
                        serde_json::json!({ "peers": remaining, "keep_local": snap.keep_local }),
                        None,
                        Duration::from_secs(12),
                    );
                }
            } else if snap.connected_peer == peer_id {
                let _ = ipc_request(
                    "disconnect",
                    serde_json::json!({}),
                    None,
                    Duration::from_secs(6),
                );
            } else {
                let _ = ipc_request(
                    "connect",
                    serde_json::json!({ "peer": peer_id }),
                    None,
                    Duration::from_secs(10),
                );
            }
        } else {
            let st = lock_or_recover(&ctrl.state).clone();
            let active_multi: Vec<String> = st
                .multiroom_peers
                .iter()
                .map(|s| s.peer_id.clone())
                .collect();
            if st.is_multiroom() && active_multi.iter().any(|id| id == peer_id) {
                let remaining: Vec<String> = active_multi
                    .into_iter()
                    .filter(|id| id != peer_id)
                    .collect();
                let target_peers: Vec<Peer> = peers
                    .into_iter()
                    .filter(|p| remaining.contains(&p.id))
                    .collect();
                if target_peers.is_empty() {
                    let _ = ctrl.disconnect(false, None, true, Some(true), false, false);
                } else {
                    let _ = ctrl.connect_multi(
                        &target_peers,
                        ConnectOptions {
                            keep_local: st.keep_local,
                            notify_user: Some(true),
                            by: "tray".to_string(),
                            ..Default::default()
                        },
                    );
                }
            } else if st.connected_peer == peer_id {
                let _ = ctrl.disconnect(false, None, true, Some(true), false, false);
            } else if let Some(peer) = peers.iter().find(|p| p.id == peer_id) {
                let _ = ctrl.connect(
                    peer,
                    ConnectOptions {
                        notify_user: Some(true),
                        by: "tray".to_string(),
                        ..Default::default()
                    },
                );
            }
        }
        return;
    }
    if let Some(peer_id) = id_str.strip_prefix("multiroom_toggle:") {
        if external {
            let snap = collect_tray_snapshot(daemon);
            let mut active_ids = if !snap.multiroom_peer_ids.is_empty() {
                snap.multiroom_peer_ids
            } else if !snap.connected_peer.is_empty() {
                vec![snap.connected_peer]
            } else {
                Vec::new()
            };
            if active_ids.iter().any(|x| x == peer_id) {
                active_ids.retain(|x| x != peer_id);
            } else {
                active_ids.push(peer_id.to_string());
            }
            if active_ids.is_empty() {
                let _ = ipc_request(
                    "disconnect",
                    serde_json::json!({}),
                    None,
                    Duration::from_secs(6),
                );
            } else {
                let _ = ipc_request(
                    "connect",
                    serde_json::json!({ "peers": active_ids, "keep_local": snap.keep_local }),
                    None,
                    Duration::from_secs(12),
                );
            }
        } else {
            let st = lock_or_recover(&ctrl.state).clone();
            let mut active_ids: Vec<String> = if !st.multiroom_peers.is_empty() {
                st.multiroom_peers
                    .iter()
                    .map(|s| s.peer_id.clone())
                    .collect()
            } else if !st.connected_peer.is_empty() {
                vec![st.connected_peer.clone()]
            } else {
                Vec::new()
            };
            if active_ids.contains(&peer_id.to_string()) {
                active_ids.retain(|x| x != peer_id);
            } else {
                active_ids.push(peer_id.to_string());
            }
            let target_peers: Vec<Peer> = peers
                .into_iter()
                .filter(|p| active_ids.contains(&p.id))
                .collect();
            if target_peers.is_empty() {
                let _ = ctrl.disconnect(false, None, true, Some(true), false, false);
            } else {
                let _ = ctrl.connect_multi(
                    &target_peers,
                    ConnectOptions {
                        keep_local: st.keep_local,
                        notify_user: Some(true),
                        by: "tray".to_string(),
                        ..Default::default()
                    },
                );
            }
        }
        return;
    }
    if id_str == "multiroom_keep_local" {
        if external {
            let snap = collect_tray_snapshot(daemon);
            if !snap.multiroom_peer_ids.is_empty() {
                let _ = ipc_request(
                    "connect",
                    serde_json::json!({
                        "peers": snap.multiroom_peer_ids,
                        "keep_local": !snap.keep_local,
                    }),
                    None,
                    Duration::from_secs(12),
                );
            }
        } else {
            let st = lock_or_recover(&ctrl.state).clone();
            let new_keep = !st.keep_local;
            let active_ids: Vec<String> = st
                .multiroom_peers
                .iter()
                .map(|s| s.peer_id.clone())
                .collect();
            let target_peers: Vec<Peer> = peers
                .into_iter()
                .filter(|p| active_ids.contains(&p.id))
                .collect();
            if !target_peers.is_empty() {
                let _ = ctrl.connect_multi(
                    &target_peers,
                    ConnectOptions {
                        keep_local: new_keep,
                        notify_user: Some(true),
                        by: "tray".to_string(),
                        ..Default::default()
                    },
                );
            }
        }
        return;
    }
    if id_str == "multiroom_broadcast_all" {
        if !peers.is_empty() {
            if external {
                let snap = collect_tray_snapshot(daemon);
                let all_ids: Vec<String> = peers.into_iter().map(|p| p.id).collect();
                let _ = ipc_request(
                    "connect",
                    serde_json::json!({ "peers": all_ids, "keep_local": snap.keep_local }),
                    None,
                    Duration::from_secs(12),
                );
            } else {
                let st = lock_or_recover(&ctrl.state).clone();
                let _ = ctrl.connect_multi(
                    &peers,
                    ConnectOptions {
                        keep_local: st.keep_local,
                        notify_user: Some(true),
                        by: "tray".to_string(),
                        ..Default::default()
                    },
                );
            }
        }
        return;
    }
    if let Some(app_name) = id_str.strip_prefix("app_route:") {
        if external {
            let snap = collect_tray_snapshot(daemon);
            let already = snap.routed_apps_lower.contains(&app_name.to_lowercase());
            let target = if already { "local" } else { "remote" };
            let _ = ipc_request(
                "route",
                serde_json::json!({ "app": app_name, "target": target, "save": false }),
                None,
                Duration::from_secs(4),
            );
        } else {
            let st = lock_or_recover(&ctrl.state).clone();
            let already = st.routed_apps.contains_key(&app_name.to_lowercase());
            if already {
                let _ = ctrl.route_app(app_name, Some("local"), false);
            } else {
                let _ = ctrl.route_app(app_name, Some("remote"), false);
            }
        }
        return;
    }
    if id_str == "mic_send_toggle" {
        if external {
            let snap = collect_tray_snapshot(daemon);
            let payload = if snap.mic_active && snap.mic_direction == "send" {
                serde_json::json!({ "action": "off" })
            } else {
                serde_json::json!({ "action": "enable", "direction": "send" })
            };
            let _ = ipc_request("mic", payload, None, Duration::from_secs(5));
        } else {
            let st = lock_or_recover(&ctrl.state).clone();
            if !st.connected_source.is_empty() && st.mic_direction == "send" {
                let _ = ctrl.disable_mic();
            } else {
                let _ = ctrl.enable_mic("send");
            }
        }
        return;
    }
    if id_str == "mic_recv_toggle" {
        if external {
            let snap = collect_tray_snapshot(daemon);
            let payload = if snap.mic_active && snap.mic_direction == "receive" {
                serde_json::json!({ "action": "off" })
            } else {
                serde_json::json!({ "action": "enable", "direction": "receive" })
            };
            let _ = ipc_request("mic", payload, None, Duration::from_secs(5));
        } else {
            let st = lock_or_recover(&ctrl.state).clone();
            if !st.connected_source.is_empty() && st.mic_direction == "receive" {
                let _ = ctrl.disable_mic();
            } else {
                let _ = ctrl.enable_mic("receive");
            }
        }
        return;
    }
    if id_str == "mic_stop" {
        if external {
            let _ = ipc_request(
                "mic",
                serde_json::json!({ "action": "off" }),
                None,
                Duration::from_secs(5),
            );
        } else {
            let _ = ctrl.disable_mic();
        }
        return;
    }
    if let Some(pct_str) = id_str.strip_prefix("vol_set:") {
        if let Ok(pct) = pct_str.parse::<u32>() {
            if external {
                let _ = ipc_request(
                    "volume",
                    serde_json::json!({ "volume": pct }),
                    None,
                    Duration::from_secs(2),
                );
            } else {
                let _ = ctrl.set_volume(Some(pct), None, true);
            }
        }
        return;
    }
    if id_str == "vol_mute_toggle" {
        if external {
            let cur_mute = ipc_request(
                "status",
                serde_json::json!({}),
                None,
                Duration::from_millis(600),
            )
            .ok()
            .and_then(|v| {
                v.get("volume")
                    .and_then(|vol| vol.get("mute"))
                    .and_then(|b| b.as_bool())
            })
            .unwrap_or(false);
            let _ = ipc_request(
                "volume",
                serde_json::json!({ "mute": !cur_mute }),
                None,
                Duration::from_secs(2),
            );
        } else {
            let v = ctrl.get_volume();
            let cur_mute = v.get("mute").and_then(|x| x.as_bool()).unwrap_or(false);
            let _ = ctrl.set_volume(None, Some(!cur_mute), true);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn compute_menu_signature(
    peers: &[Peer],
    streams: &[serde_json::Value],
    connected_peer: &str,
    connected_name: &str,
    multiroom_peer_ids: &[String],
    sharing_enabled: bool,
    auto_enabled: bool,
    keep_local: bool,
    mic_active: bool,
    mic_direction: &str,
    android_active: bool,
    pending_pair_requests: &[PendingPairRequest],
) -> String {
    let peer_sig: Vec<String> = peers
        .iter()
        .map(|p| format!("{}:{}:{}:{}", p.id, p.name, p.host, p.port))
        .collect();
    let app_sig: Vec<String> = streams
        .iter()
        .map(|a| {
            let name = a.get("app_name").and_then(|v| v.as_str()).unwrap_or("");
            let media = a.get("media_name").and_then(|v| v.as_str()).unwrap_or("");
            let remote = a
                .get("is_remote")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let rule = a.get("routed_rule").and_then(|v| v.as_str()).unwrap_or("");
            format!("{name}:{media}:{remote}:{rule}")
        })
        .collect();
    let mut sorted_multi: Vec<&str> = multiroom_peer_ids.iter().map(String::as_str).collect();
    sorted_multi.sort_unstable();
    let pair_sig: Vec<String> = pending_pair_requests
        .iter()
        .map(|r| format!("{}:{}", r.request_id, r.sas_code))
        .collect();
    format!(
        "peers={}|apps={}|conn={}|cname={}|multi={}|share={}|auto={}|keep={}|mic_on={}|mic={}|and={}|pair={}",
        peer_sig.join(","),
        app_sig.join(","),
        connected_peer,
        connected_name,
        sorted_multi.join(","),
        sharing_enabled,
        auto_enabled,
        keep_local,
        mic_active,
        mic_direction,
        android_active,
        pair_sig.join(","),
    )
}

fn find_legacy_tray_holder() -> Option<InstanceLockInfo> {
    if let Some(pid) = find_soundhop_process_with_arg("tray") {
        let ver = ipc_ping(None)
            .filter(|p| p.pid == Some(pid))
            .map(|p| p.version)
            .unwrap_or_default();
        return Some(InstanceLockInfo {
            pid: Some(pid),
            version: ver,
        });
    }
    if let Some(info) = ipc_ping(None) {
        if info.version != VERSION && info.pid.is_some() {
            return Some(InstanceLockInfo {
                pid: info.pid,
                version: info.version,
            });
        }
    }
    None
}

/// Pump pending native OS UI events without blocking.
///
/// Returns `false` if the platform event loop signaled a quit request
/// (e.g., `WM_QUIT` on Windows), or `true` to keep running.
#[cfg(target_os = "linux")]
fn pump_platform_events() -> bool {
    while gtk::events_pending() {
        gtk::main_iteration_do(false);
    }
    true
}

#[cfg(target_os = "windows")]
mod win32_session {
    use std::sync::atomic::{AtomicBool, Ordering};

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, PostQuitMessage, RegisterClassW,
        UnregisterClassW, WM_CLOSE, WM_ENDSESSION, WM_QUERYENDSESSION, WNDCLASSW, WS_OVERLAPPED,
    };

    static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

    pub fn shutdown_requested() -> bool {
        SHUTDOWN_REQUESTED.load(Ordering::Acquire)
    }

    pub fn reset_shutdown_flag() {
        SHUTDOWN_REQUESTED.store(false, Ordering::Release);
    }

    unsafe extern "system" fn wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_QUERYENDSESSION => {
                // Return TRUE (1) so Windows logoff/shutdown proceeds without blocking.
                1
            }
            WM_ENDSESSION => {
                if wparam != 0 {
                    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
                    PostQuitMessage(0);
                }
                0
            }
            WM_CLOSE => {
                SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }

    /// Hidden top-level Win32 window so this tray thread receives broadcast
    /// `WM_QUERYENDSESSION`, `WM_ENDSESSION`, and `WM_CLOSE` notifications.
    pub struct SessionShutdownWindow {
        hwnd: HWND,
        class_name: Vec<u16>,
    }

    impl SessionShutdownWindow {
        pub fn create() -> Option<Self> {
            reset_shutdown_flag();
            let class_name: Vec<u16> = "SoundHopTraySessionWnd\0".encode_utf16().collect();
            let title: Vec<u16> = "SoundHop\0".encode_utf16().collect();
            // SAFETY: Valid null-terminated UTF-16 class/title buffers and valid `wnd_proc`.
            unsafe {
                let hinstance = GetModuleHandleW(std::ptr::null());
                let wc = WNDCLASSW {
                    lpfnWndProc: Some(wnd_proc),
                    hInstance: hinstance,
                    lpszClassName: class_name.as_ptr(),
                    ..std::mem::zeroed()
                };
                let _ = RegisterClassW(&wc);
                let hwnd = CreateWindowExW(
                    0,
                    class_name.as_ptr(),
                    title.as_ptr(),
                    WS_OVERLAPPED,
                    0,
                    0,
                    0,
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    hinstance,
                    std::ptr::null(),
                );
                if hwnd.is_null() {
                    return None;
                }
                Some(Self { hwnd, class_name })
            }
        }
    }

    impl Drop for SessionShutdownWindow {
        fn drop(&mut self) {
            // SAFETY: `self.hwnd` was created on this thread via `CreateWindowExW`.
            unsafe {
                if !self.hwnd.is_null() {
                    DestroyWindow(self.hwnd);
                }
                let hinstance = GetModuleHandleW(std::ptr::null());
                UnregisterClassW(self.class_name.as_ptr(), hinstance);
            }
        }
    }
}

/// Pump pending native OS UI events without blocking.
///
/// Returns `false` if the platform event loop signaled a quit request
/// (`WM_QUIT`, `WM_CLOSE`, `WM_ENDSESSION`), or `true` to keep running.
#[cfg(target_os = "windows")]
fn pump_platform_events() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE, WM_CLOSE, WM_ENDSESSION,
        WM_QUERYENDSESSION, WM_QUIT,
    };

    if win32_session::shutdown_requested() {
        return false;
    }

    // SAFETY: `MSG` is a plain-old-data C struct populated by `PeekMessageW`.
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    // SAFETY: Passing a null HWND pumps all queued messages for the current thread
    // (required on Windows for `tray-icon` and `muda` menu callbacks).
    while unsafe { PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
        match msg.message {
            WM_QUIT | WM_CLOSE => {
                return false;
            }
            WM_QUERYENDSESSION => {
                // SAFETY: `msg` was just populated by `PeekMessageW`.
                unsafe {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            WM_ENDSESSION => {
                // SAFETY: `msg` was just populated by `PeekMessageW`.
                unsafe {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                if msg.wParam != 0 {
                    return false;
                }
            }
            _ => {
                // SAFETY: `msg` was just populated by `PeekMessageW`.
                unsafe {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        }
        if win32_session::shutdown_requested() {
            return false;
        }
    }
    !win32_session::shutdown_requested()
}

/// Pump pending native OS UI events without blocking.
///
/// Returns `false` if the platform event loop signaled a quit request,
/// or `true` to keep running.
#[cfg(target_os = "macos")]
fn pump_platform_events() -> bool {
    use core_foundation::runloop::{
        kCFRunLoopDefaultMode, kCFRunLoopRunHandledSource, CFRunLoopRunInMode,
    };

    const MAX_PUMP_ITERATIONS: usize = 64;
    for _ in 0..MAX_PUMP_ITERATIONS {
        // SAFETY: `kCFRunLoopDefaultMode` is a valid CoreFoundation run-loop mode
        // constant, and `CFRunLoopRunInMode` is called on the main thread with a
        // zero timeout and `returnAfterSourceHandled = 1` (non-blocking).
        let result = unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.0, 1) };
        if result != kCFRunLoopRunHandledSource {
            break;
        }
    }
    true
}

/// Pump pending native OS UI events without blocking.
///
/// Returns `false` if the platform event loop signaled a quit request,
/// or `true` to keep running.
#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
fn pump_platform_events() -> bool {
    true
}

/// Runs the native system tray and embedded background daemon loop.
pub fn run_tray(verbose: bool) -> Result<i32, String> {
    let _ = sync_appimage_paths_if_installed(true);

    let _lock = match replace_stale_instance("tray", Some(find_legacy_tray_holder)) {
        Some(l) => l,
        None => {
            if verbose {
                eprintln!("SoundHop system tray is already running in this session.");
            }
            return Ok(0);
        }
    };

    #[cfg(target_os = "linux")]
    {
        gtk::init().map_err(|e| format!("Failed to initialize GTK for system tray: {e}"))?;
    }

    #[cfg(target_os = "windows")]
    let _session_window = win32_session::SessionShutdownWindow::create();

    let daemon = Daemon::new(None, None, !verbose, None);
    if daemon.start().is_err() {
        daemon.start_discovery_only();
    }

    let ctrl = Arc::clone(&daemon.controller);
    let initial_st = lock_or_recover(&ctrl.state).clone();
    let initial_cfg = lock_or_recover(&ctrl.config).clone();
    let initial_snap = TrayMenuSnapshot::from_parts(
        Vec::new(),
        Vec::new(),
        initial_st,
        initial_cfg,
        ctrl.android_session.is_active(),
        ctrl.pending_pair_requests(),
    );
    let initial_icon = build_tray_icon(initial_snap.badge);
    let initial_menu = build_tray_menu_from_snapshot(&initial_snap);
    let mut last_tooltip = format_tray_tooltip(&initial_snap);
    let tray_icon = TrayIconBuilder::new()
        .with_tooltip(&last_tooltip)
        .with_icon(initial_icon)
        .with_menu(Box::new(initial_menu))
        .build()
        .map_err(|e| format!("Failed to create system tray icon: {e}"))?;

    soundhop_core::util::install_signal_handlers();
    let running = Arc::new(AtomicBool::new(true));
    let mut last_badge = initial_snap.badge;
    let mut last_sig = initial_snap.signature;

    let (cmd_tx, cmd_rx) = mpsc::channel::<String>();
    let (ui_tx, ui_rx) = mpsc::channel::<TrayMenuSnapshot>();
    let worker_daemon = Arc::clone(&daemon);
    let worker_running = Arc::clone(&running);

    let worker_handle = std::thread::spawn(move || {
        let _ = ui_tx.send(collect_tray_snapshot(&worker_daemon));
        let mut last_refresh = Instant::now();
        while worker_running.load(Ordering::Relaxed)
            && !soundhop_core::util::signal_stop_requested()
        {
            match cmd_rx.recv_timeout(Duration::from_millis(200)) {
                Ok(id_str) => {
                    handle_menu_action(&id_str, &worker_daemon, &worker_running);
                    let snap = collect_tray_snapshot(&worker_daemon);
                    let _ = ui_tx.send(snap);
                    last_refresh = Instant::now();
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if last_refresh.elapsed() >= Duration::from_secs(2) {
                        let snap = collect_tray_snapshot(&worker_daemon);
                        if snap.peers.is_empty() {
                            if let Some(disc) = lock_or_recover(&worker_daemon.discovery).as_ref() {
                                disc.probe();
                            }
                        }
                        let _ = ui_tx.send(snap);
                        let _ = now_secs();
                        last_refresh = Instant::now();
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });

    while running.load(Ordering::Relaxed) && !soundhop_core::util::signal_stop_requested() {
        if !pump_platform_events() {
            running.store(false, Ordering::SeqCst);
            break;
        }

        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            if matches!(event, TrayIconEvent::DoubleClick { .. }) {
                spawn_self_args(&["--gui-tab", "home"]);
            }
        }

        while let Ok(event) = MenuEvent::receiver().try_recv() {
            let id_str = event.id.0;
            if id_str == "quit_tray" {
                running.store(false, Ordering::SeqCst);
                break;
            } else if id_str == "open_gui" {
                spawn_self_args(&["--gui-tab", "home"]);
            } else if id_str == "open_settings" {
                spawn_self_args(&["--gui-tab", "settings"]);
            } else if id_str == "open_android_wizard" {
                spawn_self_args(&["--gui-tab", "phone"]);
            } else {
                let _ = cmd_tx.send(id_str);
            }
        }

        let mut latest_snap: Option<TrayMenuSnapshot> = None;
        while let Ok(snap) = ui_rx.try_recv() {
            latest_snap = Some(snap);
        }
        if let Some(snap) = latest_snap {
            if snap.badge != last_badge {
                let _ = tray_icon.set_icon(Some(build_tray_icon(snap.badge)));
                last_badge = snap.badge;
            }
            let next_tooltip = format_tray_tooltip(&snap);
            if next_tooltip != last_tooltip {
                let _ = tray_icon.set_tooltip(Some(&next_tooltip));
                last_tooltip = next_tooltip;
            }
            if snap.signature != last_sig {
                tray_icon.set_menu(Some(Box::new(build_tray_menu_from_snapshot(&snap))));
                last_sig = snap.signature;
            }
        }

        std::thread::sleep(Duration::from_millis(30));
    }

    running.store(false, Ordering::SeqCst);
    drop(cmd_tx);
    let _ = worker_handle.join();
    daemon.shutdown();
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_tray_icon_badges() {
        for state in [
            TrayBadgeState::Idle,
            TrayBadgeState::Sharing,
            TrayBadgeState::Connected,
            TrayBadgeState::MultiRoom,
            TrayBadgeState::Error,
        ] {
            let _icon = build_tray_icon(state);
        }
    }

    #[test]
    fn test_tray_menu_labels_avoid_gtk_mnemonics() {
        let src = include_str!("tray.rs");
        let prod_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        for forbidden in [
            "Volume & Mute",
            "Settings & Routing",
            "Android Audio & Wi-Fi",
        ] {
            assert!(
                !prod_src.contains(forbidden),
                "Tray menu must not contain single '&' mnemonic string: {forbidden}"
            );
        }
    }

    #[test]
    fn test_compute_menu_signature_tracks_apps_and_android() {
        let s1 = compute_menu_signature(
            &[],
            &[],
            "",
            "",
            &[],
            false,
            false,
            false,
            false,
            "",
            false,
            &[],
        );
        let s2 = compute_menu_signature(
            &[],
            &[serde_json::json!({
                "app_name": "spotify",
                "media_name": "Track 1",
                "is_remote": true,
                "routed_rule": "remote"
            })],
            "",
            "",
            &[],
            false,
            false,
            false,
            false,
            "",
            false,
            &[],
        );
        let s3 = compute_menu_signature(
            &[],
            &[],
            "",
            "",
            &[],
            false,
            false,
            false,
            false,
            "",
            true,
            &[],
        );
        assert_ne!(s1, s2);
        assert_ne!(s1, s3);
    }

    #[test]
    fn test_compute_menu_signature_tracks_multiroom_mic_active_and_connected_name() {
        let base = compute_menu_signature(
            &[],
            &[],
            "peer-1",
            "Living Room",
            &["peer-1".to_string(), "peer-2".to_string()],
            false,
            false,
            false,
            false,
            "send",
            false,
            &[],
        );
        // Same count of multiroom peers, different peer IDs
        let diff_multi = compute_menu_signature(
            &[],
            &[],
            "peer-1",
            "Living Room",
            &["peer-1".to_string(), "peer-3".to_string()],
            false,
            false,
            false,
            false,
            "send",
            false,
            &[],
        );
        assert_ne!(base, diff_multi);

        // Same mic_direction, different mic_active
        let diff_mic = compute_menu_signature(
            &[],
            &[],
            "peer-1",
            "Living Room",
            &["peer-1".to_string(), "peer-2".to_string()],
            false,
            false,
            false,
            true,
            "send",
            false,
            &[],
        );
        assert_ne!(base, diff_mic);

        // Different connected_name
        let diff_name = compute_menu_signature(
            &[],
            &[],
            "peer-1",
            "Kitchen",
            &["peer-1".to_string(), "peer-2".to_string()],
            false,
            false,
            false,
            false,
            "send",
            false,
            &[],
        );
        assert_ne!(base, diff_name);
    }

    #[test]
    fn test_format_tray_tooltip_reflects_live_state() {
        let mut st = soundhop_core::state::State::default();
        let mut cfg = soundhop_core::config::Config::default();

        // Idle
        cfg.sharing.enabled = false;
        let idle = TrayMenuSnapshot::from_parts(
            Vec::new(),
            Vec::new(),
            st.clone(),
            cfg.clone(),
            false,
            Vec::new(),
        );
        assert_eq!(format_tray_tooltip(&idle), "SoundHop — Idle");

        // Sharing speakers
        cfg.sharing.enabled = true;
        let sharing = TrayMenuSnapshot::from_parts(
            Vec::new(),
            Vec::new(),
            st.clone(),
            cfg.clone(),
            false,
            Vec::new(),
        );
        assert_eq!(format_tray_tooltip(&sharing), "SoundHop — Sharing speakers");

        // Receiving phone audio
        let phone = TrayMenuSnapshot::from_parts(
            Vec::new(),
            Vec::new(),
            st.clone(),
            cfg.clone(),
            true,
            Vec::new(),
        );
        assert_eq!(
            format_tray_tooltip(&phone),
            "SoundHop — Receiving phone audio"
        );

        // Pending pair request takes priority in tooltip
        let pending = vec![PendingPairRequest {
            request_id: "req-1".to_string(),
            device_id: "dev-1".to_string(),
            display_name: "Studio Laptop".to_string(),
            platform: "linux".to_string(),
            remote_addr: "192.168.1.42:51234".to_string(),
            fingerprint: "aa:bb:cc:dd".to_string(),
            sas_code: "482-913".to_string(),
            created_at: 100.0,
            expires_at: 160.0,
            outgoing: false,
        }];
        let pair_snap = TrayMenuSnapshot::from_parts(
            Vec::new(),
            Vec::new(),
            st.clone(),
            cfg.clone(),
            false,
            pending,
        );
        assert_eq!(
            format_tray_tooltip(&pair_snap),
            "SoundHop — Pair request from Studio Laptop (Code: 482-913)"
        );

        // Streaming to a single peer
        st.connected_peer = "peer-1".to_string();
        st.connected_name = "Living Room".to_string();
        st.connected_sink = "sink-1".to_string();
        let streaming = TrayMenuSnapshot::from_parts(
            Vec::new(),
            Vec::new(),
            st.clone(),
            cfg.clone(),
            false,
            Vec::new(),
        );
        assert_eq!(
            format_tray_tooltip(&streaming),
            "SoundHop — Streaming to Living Room"
        );

        // Long peer name is truncated safely within Windows szTip limits
        st.connected_name = "A".repeat(160);
        let long_tip = format_tray_tooltip(&TrayMenuSnapshot::from_parts(
            Vec::new(),
            Vec::new(),
            st,
            cfg,
            false,
            Vec::new(),
        ));
        assert!(long_tip.chars().count() <= 120);
        assert!(long_tip.ends_with("..."));
    }

    #[test]
    fn test_resolve_gui_executable_prefers_sibling_gui_on_windows() {
        let dir = std::env::temp_dir().join(format!("soundhop-tray-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let cli_exe = dir.join("soundhop.exe");
        let gui_exe = dir.join("soundhop-gui.exe");
        let _ = std::fs::write(&cli_exe, b"");

        // Without sibling soundhop-gui.exe -> falls back to current_exe
        let _ = std::fs::remove_file(&gui_exe);
        assert_eq!(resolve_gui_executable_from(&cli_exe), cli_exe);

        // With sibling soundhop-gui.exe -> prefers soundhop-gui.exe on Windows
        let _ = std::fs::write(&gui_exe, b"");
        #[cfg(target_os = "windows")]
        assert_eq!(resolve_gui_executable_from(&cli_exe), gui_exe);
        #[cfg(not(target_os = "windows"))]
        assert_eq!(resolve_gui_executable_from(&cli_exe), cli_exe);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
