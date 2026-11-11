//! Cross-platform Native System Tray (`tray-icon` + `muda`) with dynamic RGBA status icons,
//! Multi-Room peer toggles, per-app routing submenus, mic pass-through, volume presets, and
//! child dialog launchers.

use std::collections::HashSet;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use muda::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use soundhop_core::constants::PROTOCOL_VERSION;
use soundhop_core::controller::{ConnectOptions, Controller};
use soundhop_core::daemon::Daemon;
use soundhop_core::discovery::Peer;
use soundhop_core::ipc::{is_running as ipc_is_running, request as ipc_request};
use soundhop_core::util::{is_own_non_loopback_ip, is_private_address, now_secs};
use tray_icon::{Icon, TrayIconBuilder};

const EMBEDDED_PNG: &[u8] = include_bytes!("../../../assets/soundhop.png");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayBadgeState {
    Idle,
    Sharing,
    Connected,
    MultiRoom,
    Error,
}

/// Generates a 64x64 RGBA tray icon with a colored status badge in the bottom-right corner.
pub fn build_tray_icon(state: TrayBadgeState) -> Icon {
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

fn compute_badge_state(controller: &Controller) -> TrayBadgeState {
    let st = controller.state.lock().unwrap().clone();
    let cfg = controller.config.lock().unwrap().clone();
    if st.is_multiroom() {
        TrayBadgeState::MultiRoom
    } else if st.is_connected() {
        TrayBadgeState::Connected
    } else if cfg.sharing.enabled || st.listener_module > 0 {
        TrayBadgeState::Sharing
    } else {
        TrayBadgeState::Idle
    }
}

fn daemon_peers(daemon: &Daemon) -> Vec<Peer> {
    let mut peers: Vec<Peer> = daemon
        .discovery
        .lock()
        .unwrap()
        .as_ref()
        .map(|d| d.peers())
        .unwrap_or_default();
    let mut seen_ids: HashSet<String> = peers.iter().map(|p| p.id.clone()).collect();
    let own_id = daemon.controller.config.lock().unwrap().device.id.clone();

    if daemon.ipc.lock().unwrap().is_none() && ipc_is_running(None) {
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

    let st = daemon.controller.state.lock().unwrap().clone();
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
    let menu = Menu::new();
    let st = controller.state.lock().unwrap().clone();
    let cfg = controller.config.lock().unwrap().clone();

    let status_text = if st.is_multiroom() {
        format!("SoundHop — Multi-Room ({})", st.connected_name)
    } else if st.is_connected() {
        format!("SoundHop — Connected to {}", st.connected_name)
    } else if cfg.sharing.enabled {
        "SoundHop — Sharing Speakers".to_string()
    } else {
        "SoundHop — Ready".to_string()
    };
    let header_item = MenuItem::with_id(MenuId::new("noop_header"), status_text, false, None);
    let _ = menu.append(&header_item);
    let _ = menu.append(&PredefinedMenuItem::separator());

    let connected_ids: HashSet<String> = if !st.multiroom_peers.is_empty() {
        st.multiroom_peers
            .iter()
            .map(|s| s.peer_id.clone())
            .collect()
    } else if !st.connected_peer.is_empty() {
        let mut s = HashSet::new();
        s.insert(st.connected_peer.clone());
        s
    } else {
        HashSet::new()
    };

    if peers.is_empty() {
        let no_peers = MenuItem::with_id(
            MenuId::new("noop_peers"),
            "Searching for LAN speakers...",
            false,
            None,
        );
        let _ = menu.append(&no_peers);
    } else {
        for peer in peers {
            let is_conn = connected_ids.contains(&peer.id)
                || (st.connected_host == peer.host && !st.connected_peer.is_empty());
            let rtt_suffix = peer
                .rtt_ms
                .map(|r| format!(" ({r:.0} ms)"))
                .unwrap_or_default();
            let label = format!("{} [{}]{}", peer.name, peer.host, rtt_suffix);
            let item = CheckMenuItem::with_id(
                MenuId::new(format!("peer_toggle:{}", peer.id)),
                label,
                true,
                is_conn,
                None,
            );
            let _ = menu.append(&item);
        }

        if peers.len() > 1 {
            let multi_sub = Submenu::new("Multi-Room Broadcast", true);
            for peer in peers {
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
                "Keep Local Speakers Active (--keep-local)",
                true,
                st.keep_local,
                None,
            );
            let _ = multi_sub.append(&keep_local_chk);
            let broadcast_all = MenuItem::with_id(
                MenuId::new("multiroom_broadcast_all"),
                "Broadcast to All Discovered Peers",
                true,
                None,
            );
            let _ = multi_sub.append(&broadcast_all);
            let _ = menu.append(&multi_sub);
        }
    }

    if st.is_connected() {
        let disc_item = MenuItem::with_id(
            MenuId::new("action_disconnect"),
            "Disconnect Speaker(s)",
            true,
            None,
        );
        let _ = menu.append(&disc_item);
    }

    let _ = menu.append(&PredefinedMenuItem::separator());

    let share_chk = CheckMenuItem::with_id(
        MenuId::new("toggle_share"),
        format!("Share My Speakers (TCP :{})", cfg.sharing.port),
        true,
        cfg.sharing.enabled || st.listener_module > 0,
        None,
    );
    let _ = menu.append(&share_chk);

    let auto_chk = CheckMenuItem::with_id(
        MenuId::new("toggle_autoconnect"),
        "Auto-Connect to Preferred Speaker",
        true,
        cfg.connection.auto_connect && !st.auto_paused,
        None,
    );
    let _ = menu.append(&auto_chk);

    // Per-App Audio Routing submenu
    let app_sub = Submenu::new("Per-App Audio Routing", true);
    let streams = controller.list_apps();
    if streams.is_empty() {
        let empty = MenuItem::with_id(
            MenuId::new("noop_apps"),
            "No active audio streams",
            false,
            None,
        );
        let _ = app_sub.append(&empty);
    } else {
        for stream in streams {
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
            let chk = CheckMenuItem::with_id(
                MenuId::new(format!("app_route:{app_name}")),
                format!("{app_name} ({media_name})"),
                st.is_connected(),
                is_routed,
                None,
            );
            let _ = app_sub.append(&chk);
        }
    }
    let _ = menu.append(&app_sub);

    // Microphone Pass-Through submenu
    let mic_active = !st.connected_source.is_empty();
    let mic_sub = Submenu::new("Microphone Pass-Through", true);
    let mic_send = CheckMenuItem::with_id(
        MenuId::new("mic_send_toggle"),
        "Send My Microphone to Connected Peer",
        st.is_connected() || mic_active,
        mic_active && st.mic_direction == "send",
        None,
    );
    let mic_recv = CheckMenuItem::with_id(
        MenuId::new("mic_recv_toggle"),
        "Receive Peer Microphone",
        st.is_connected() || mic_active,
        mic_active && st.mic_direction == "receive",
        None,
    );
    let _ = mic_sub.append(&mic_send);
    let _ = mic_sub.append(&mic_recv);
    if mic_active {
        let mic_off = MenuItem::with_id(
            MenuId::new("mic_stop"),
            "Stop Microphone Pass-Through",
            true,
            None,
        );
        let _ = mic_sub.append(&mic_off);
    }
    let _ = menu.append(&mic_sub);

    // Volume submenu
    let vol_sub = Submenu::new("Volume & Mute", st.is_connected());
    for pct in [25u32, 50, 75, 100, 125] {
        let item = MenuItem::with_id(
            MenuId::new(format!("vol_set:{pct}")),
            format!("Set Volume {pct}%"),
            st.is_connected(),
            None,
        );
        let _ = vol_sub.append(&item);
    }
    let _ = vol_sub.append(&PredefinedMenuItem::separator());
    let mute_item = MenuItem::with_id(
        MenuId::new("vol_mute_toggle"),
        "Toggle Mute",
        st.is_connected(),
        None,
    );
    let _ = vol_sub.append(&mute_item);
    let _ = menu.append(&vol_sub);

    // Android Audio submenu
    let android_sub = Submenu::new("Android Audio Bridge", true);
    let and_running = controller.android_session.is_active();
    let and_wiz = MenuItem::with_id(
        MenuId::new("open_android_wizard"),
        "Open Android Audio & Wi-Fi ADB Wizard...",
        true,
        None,
    );
    let _ = android_sub.append(&and_wiz);
    if and_running {
        let and_stop = MenuItem::with_id(
            MenuId::new("android_stop"),
            "Stop Active Android Stream",
            true,
            None,
        );
        let _ = android_sub.append(&and_stop);
    }
    let _ = menu.append(&android_sub);

    let _ = menu.append(&PredefinedMenuItem::separator());

    let settings_item = MenuItem::with_id(
        MenuId::new("open_settings"),
        "Settings & Routing Rules...",
        true,
        None,
    );
    let _ = menu.append(&settings_item);

    let quit_item = MenuItem::with_id(MenuId::new("quit_tray"), "Quit SoundHop", true, None);
    let _ = menu.append(&quit_item);

    menu
}

fn spawn_self_dialog(flag: &str) {
    if let Ok(exe) = std::env::current_exe() {
        let mut cmd = Command::new(exe);
        cmd.arg(flag);
        soundhop_core::util::close_fds_pre_exec(&mut cmd);
        let _ = cmd.spawn();
    }
}

fn handle_menu_action(id_str: &str, daemon: &Daemon, running: &AtomicBool) {
    let ctrl = &daemon.controller;
    let peers = daemon_peers(daemon);

    if id_str == "quit_tray" {
        running.store(false, Ordering::SeqCst);
        return;
    }
    if id_str == "open_settings" {
        spawn_self_dialog("--settings-dialog");
        return;
    }
    if id_str == "open_android_wizard" {
        spawn_self_dialog("--android-wizard");
        return;
    }
    if id_str == "android_stop" {
        let _ = ctrl.android_session.stop();
        return;
    }
    if id_str == "action_disconnect" {
        let _ = ctrl.disconnect(false, None, true, Some(true), false, false);
        return;
    }
    if id_str == "toggle_share" {
        let currently_enabled = ctrl.config.lock().unwrap().sharing.enabled;
        let next_enabled = !currently_enabled;
        let _ = ctrl.share(next_enabled, Some(true));
        if let Some(disc) = daemon.discovery.lock().unwrap().as_ref() {
            disc.set_advertising(next_enabled);
        }
        return;
    }
    if id_str == "toggle_autoconnect" {
        let cur = ctrl.config.lock().unwrap().connection.auto_connect;
        let _ = ctrl.set_auto(!cur);
        return;
    }
    if let Some(peer_id) = id_str.strip_prefix("peer_toggle:") {
        let st = ctrl.state.lock().unwrap().clone();
        if st.connected_peer == peer_id {
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
        return;
    }
    if let Some(peer_id) = id_str.strip_prefix("multiroom_toggle:") {
        let st = ctrl.state.lock().unwrap().clone();
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
        return;
    }
    if id_str == "multiroom_keep_local" {
        let st = ctrl.state.lock().unwrap().clone();
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
        return;
    }
    if id_str == "multiroom_broadcast_all" {
        if !peers.is_empty() {
            let st = ctrl.state.lock().unwrap().clone();
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
        return;
    }
    if let Some(app_name) = id_str.strip_prefix("app_route:") {
        let st = ctrl.state.lock().unwrap().clone();
        let already = st.routed_apps.contains_key(&app_name.to_lowercase());
        if already {
            let _ = ctrl.route_app(app_name, Some("local"), false);
        } else {
            let _ = ctrl.route_app(app_name, Some("remote"), false);
        }
        return;
    }
    if id_str == "mic_send_toggle" {
        let st = ctrl.state.lock().unwrap().clone();
        if !st.connected_source.is_empty() && st.mic_direction == "send" {
            let _ = ctrl.disable_mic();
        } else {
            let _ = ctrl.enable_mic("send");
        }
        return;
    }
    if id_str == "mic_recv_toggle" {
        let st = ctrl.state.lock().unwrap().clone();
        if !st.connected_source.is_empty() && st.mic_direction == "receive" {
            let _ = ctrl.disable_mic();
        } else {
            let _ = ctrl.enable_mic("receive");
        }
        return;
    }
    if id_str == "mic_stop" {
        let _ = ctrl.disable_mic();
        return;
    }
    if let Some(pct_str) = id_str.strip_prefix("vol_set:") {
        if let Ok(pct) = pct_str.parse::<u32>() {
            let _ = ctrl.set_volume(Some(pct), None, true);
        }
        return;
    }
    if id_str == "vol_mute_toggle" {
        let v = ctrl.get_volume();
        let cur_mute = v.get("mute").and_then(|x| x.as_bool()).unwrap_or(false);
        let _ = ctrl.set_volume(None, Some(!cur_mute), true);
    }
}

fn menu_state_signature(controller: &Controller, peers: &[Peer]) -> String {
    let st = controller.state.lock().unwrap().clone();
    let cfg = controller.config.lock().unwrap().clone();
    let peer_sig: Vec<String> = peers
        .iter()
        .map(|p| format!("{}:{}:{}:{}", p.id, p.name, p.host, p.port))
        .collect();
    format!(
        "peers={}|conn={}|multi={}|share={}|auto={}|keep={}|mic={}",
        peer_sig.join(","),
        st.connected_peer,
        st.multiroom_peers.len(),
        cfg.sharing.enabled,
        cfg.connection.auto_connect && !st.auto_paused,
        st.keep_local,
        st.mic_direction
    )
}

/// Runs the native system tray and embedded background daemon loop.
pub fn run_tray(verbose: bool) -> Result<i32, String> {
    let _lock = match soundhop_core::util::try_acquire_instance_lock("tray") {
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

    let daemon = Daemon::new(None, None, !verbose, None);
    if daemon.start().is_err() {
        daemon.start_discovery_only();
    }

    let ctrl = Arc::clone(&daemon.controller);
    let initial_peers = daemon_peers(&daemon);
    let initial_badge = compute_badge_state(&ctrl);
    let initial_icon = build_tray_icon(initial_badge);
    let initial_menu = build_tray_menu(&ctrl, &initial_peers);

    let dev_name = ctrl.config.lock().unwrap().device.name.clone();
    let tray_icon = TrayIconBuilder::new()
        .with_tooltip(format!("SoundHop — {dev_name}"))
        .with_icon(initial_icon)
        .with_menu(Box::new(initial_menu))
        .build()
        .map_err(|e| format!("Failed to create system tray icon: {e}"))?;

    let running = Arc::new(AtomicBool::new(true));
    let mut last_badge = initial_badge;
    let mut last_refresh = Instant::now();
    let mut last_sig = menu_state_signature(&ctrl, &initial_peers);

    while running.load(Ordering::Relaxed) {
        #[cfg(target_os = "linux")]
        {
            while gtk::events_pending() {
                gtk::main_iteration_do(false);
            }
        }

        while let Ok(event) = MenuEvent::receiver().try_recv() {
            handle_menu_action(event.id.0.as_str(), &daemon, &running);
            let peers = daemon_peers(&daemon);
            let new_badge = compute_badge_state(&ctrl);
            let _ = tray_icon.set_icon(Some(build_tray_icon(new_badge)));
            tray_icon.set_menu(Some(Box::new(build_tray_menu(&ctrl, &peers))));
            last_badge = new_badge;
            last_sig = menu_state_signature(&ctrl, &peers);
            last_refresh = Instant::now();
        }

        if last_refresh.elapsed() >= Duration::from_secs(2) {
            let peers = daemon_peers(&daemon);
            if peers.is_empty() {
                if let Some(disc) = daemon.discovery.lock().unwrap().as_ref() {
                    disc.probe();
                }
            }
            let badge = compute_badge_state(&ctrl);
            let sig = menu_state_signature(&ctrl, &peers);
            if badge != last_badge {
                let _ = tray_icon.set_icon(Some(build_tray_icon(badge)));
                last_badge = badge;
            }
            if sig != last_sig {
                tray_icon.set_menu(Some(Box::new(build_tray_menu(&ctrl, &peers))));
                last_sig = sig;
            }
            let _ = now_secs();
            last_refresh = Instant::now();
        }

        std::thread::sleep(Duration::from_millis(30));
    }

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
}
