//! Interactive Terminal UI (`soundhop tui`) built with `ratatui` + `crossterm`.
//! Features:
//! - Tab 1: Peers & Multi-Room Broadcast (`Space` to toggle peer in multi-room group, `Enter` to connect, `c`/`i` manual IP connect, `d` to disconnect, `s` to toggle speaker sharing)
//! - Tab 2: Per-App Mixer & Persistent Routing (`Enter` to toggle remote/local routing, `p` to save persistent rule in `config.toml`)
//! - Tab 3: Microphone Pass-Through & Android Audio Bridge (`s` send mic, `r` receive mic, `o` stop mic, `a` start/stop Android stream, `w` switch USB Android device to Wi-Fi)
//! - Tab 4: Web Broadcast (`b` toggle broadcast, `t` rotate token, `r` toggle USB `adb reverse` + open browser, live QR code + listener list)
//! - Bottom Telemetry Bar: Live Stereo Peak/RMS VU gauge + dBFS + Network RTT/Jitter (`ms`)

use std::collections::HashSet;
use std::io::{self, IsTerminal};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::cursor::Show;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Gauge, List, ListItem, Paragraph, Tabs};
use ratatui::{Frame, Terminal};
use soundhop_core::controller::{ConnectOptions, Controller};
use soundhop_core::discovery::{discover_once_with_meta, DiscoveryManager, Peer};
use soundhop_core::pairing::PendingPairRequest;
use soundhop_core::util::{
    install_signal_handlers, is_own_non_loopback_ip, is_private_address, now_secs,
    signal_stop_requested, split_host_port,
};
use soundhop_core::web::qr::render_terminal_qr;
use soundhop_core::web::WebClientInfo;

use crate::core_cmds::format_bytes;
use crate::ui::lock_or_recover;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuiTab {
    PeersAndMultiRoom,
    AppMixerAndRouting,
    MicAndAndroid,
    WebBroadcast,
}

impl TuiTab {
    pub fn next(self) -> Self {
        match self {
            Self::PeersAndMultiRoom => Self::AppMixerAndRouting,
            Self::AppMixerAndRouting => Self::MicAndAndroid,
            Self::MicAndAndroid => Self::WebBroadcast,
            Self::WebBroadcast => Self::PeersAndMultiRoom,
        }
    }

    pub fn index(self) -> usize {
        match self {
            Self::PeersAndMultiRoom => 0,
            Self::AppMixerAndRouting => 1,
            Self::MicAndAndroid => 2,
            Self::WebBroadcast => 3,
        }
    }
}

pub struct TuiState {
    pub controller: Arc<Controller>,
    pub tab: TuiTab,
    pub peers: Vec<Peer>,
    pub selected_peer_idx: usize,
    pub multiroom_selected: HashSet<String>,
    pub keep_local: bool,
    pub apps: Vec<serde_json::Value>,
    pub selected_app_idx: usize,
    pub selected_web_client_idx: usize,
    pub manual_ip_prompt: Option<String>,
    pub pending_pair_requests: Vec<PendingPairRequest>,
    pub status_message: String,
    pub should_quit: bool,
}

impl TuiState {
    pub fn new(controller: Arc<Controller>) -> Self {
        let st = lock_or_recover(&controller.state).clone();
        let mut multiroom_selected = HashSet::new();
        for s in &st.multiroom_peers {
            multiroom_selected.insert(s.peer_id.clone());
        }
        let peers: Vec<Peer> = st
            .known_peers
            .values()
            .filter(|kp| is_private_address(&kp.host) && !is_own_non_loopback_ip(&kp.host))
            .map(|kp| Peer {
                id: kp.id.clone(),
                name: kp.name.clone(),
                host: kp.host.clone(),
                port: kp.port,
                version: kp.version.clone(),
                protocol: soundhop_core::constants::PROTOCOL_VERSION,
                security: kp.security.clone(),
                platform: kp.platform.clone(),
                last_seen: kp.last_seen,
                source: "remembered".to_string(),
                rtt_ms: controller
                    .telemetry
                    .peer_stats(&kp.id)
                    .map(|n| n.last_rtt_ms),
            })
            .collect();
        let apps = controller.list_apps();
        let pending_pair_requests = controller.pending_pair_requests();
        Self {
            controller,
            tab: TuiTab::PeersAndMultiRoom,
            peers,
            selected_peer_idx: 0,
            multiroom_selected,
            keep_local: st.keep_local,
            apps,
            selected_app_idx: 0,
            selected_web_client_idx: 0,
            manual_ip_prompt: None,
            pending_pair_requests,
            status_message: "Press Tab to switch views, Space to select multi-room peers, Enter to apply, c/i for manual IP, q to quit.".to_string(),
            should_quit: false,
        }
    }

    pub fn merge_live_peers(&mut self, live: Vec<Peer>) {
        if live.is_empty() {
            return;
        }
        for p in &live {
            if let Some(rtt) = p.rtt_ms {
                self.controller.telemetry.record_rtt(&p.id, rtt);
            }
        }
        let mut merged = live;
        let seen: HashSet<String> = merged.iter().map(|p| p.id.clone()).collect();
        for old in &self.peers {
            if !seen.contains(&old.id)
                && is_private_address(&old.host)
                && !is_own_non_loopback_ip(&old.host)
            {
                merged.push(old.clone());
            }
        }
        self.peers = merged;
    }

    pub fn refresh_data(&mut self) {
        let _ = self.controller.status(Some(&self.peers));
        self.apps = self.controller.list_apps();
        self.pending_pair_requests = self.controller.pending_pair_requests();
        if self.selected_peer_idx >= self.peers.len() && !self.peers.is_empty() {
            self.selected_peer_idx = self.peers.len() - 1;
        }
        if self.selected_app_idx >= self.apps.len() && !self.apps.is_empty() {
            self.selected_app_idx = self.apps.len() - 1;
        }
        let web_client_count = self.controller.list_web_clients().len();
        if web_client_count == 0 {
            self.selected_web_client_idx = 0;
        } else if self.selected_web_client_idx >= web_client_count {
            self.selected_web_client_idx = web_client_count - 1;
        }
    }

    pub fn handle_key(&mut self, code: KeyCode) {
        if self.manual_ip_prompt.is_some() {
            self.handle_manual_ip_key(code);
            return;
        }
        if !self.pending_pair_requests.is_empty() {
            match code {
                KeyCode::Char('a') | KeyCode::Char('A') => {
                    let req = self.pending_pair_requests.remove(0);
                    match self.controller.approve_pair_request(&req.request_id) {
                        Ok(_) => {
                            self.status_message =
                                format!("Approved pairing request from {}.", req.display_name);
                        }
                        Err(e) => {
                            self.status_message = format!(
                                "Approved pairing request from {} ({e}).",
                                req.display_name
                            );
                        }
                    }
                    return;
                }
                KeyCode::Char('d') | KeyCode::Char('D') => {
                    let req = self.pending_pair_requests.remove(0);
                    match self.controller.deny_pair_request(&req.request_id) {
                        Ok(_) => {
                            self.status_message =
                                format!("Denied pairing request from {}.", req.display_name);
                        }
                        Err(e) => {
                            self.status_message =
                                format!("Denied pairing request from {} ({e}).", req.display_name);
                        }
                    }
                    return;
                }
                _ => {}
            }
        }
        match code {
            KeyCode::Char('q') | KeyCode::Esc => {
                self.should_quit = true;
            }
            KeyCode::Tab => {
                self.tab = self.tab.next();
            }
            KeyCode::Char('1') => self.tab = TuiTab::PeersAndMultiRoom,
            KeyCode::Char('2') => self.tab = TuiTab::AppMixerAndRouting,
            KeyCode::Char('3') => self.tab = TuiTab::MicAndAndroid,
            KeyCode::Char('4') => self.tab = TuiTab::WebBroadcast,
            KeyCode::Char('+') | KeyCode::Char('=') => {
                let v = self.controller.get_volume();
                let cur = v.get("volume").and_then(|x| x.as_u64()).unwrap_or(100) as u32;
                let next = (cur + 5).min(150);
                let _ = self.controller.set_volume(Some(next), None, true);
                self.status_message = format!("Volume set to {next}%");
            }
            KeyCode::Char('-') => {
                let v = self.controller.get_volume();
                let cur = v.get("volume").and_then(|x| x.as_u64()).unwrap_or(100) as u32;
                let next = cur.saturating_sub(5);
                let _ = self.controller.set_volume(Some(next), None, true);
                self.status_message = format!("Volume set to {next}%");
            }
            KeyCode::Char('m') => {
                let v = self.controller.get_volume();
                let cur_mute = v.get("mute").and_then(|x| x.as_bool()).unwrap_or(false);
                let _ = self.controller.set_volume(None, Some(!cur_mute), true);
                self.status_message = format!("Mute toggled to {}", !cur_mute);
            }
            _ => match self.tab {
                TuiTab::PeersAndMultiRoom => self.handle_peers_key(code),
                TuiTab::AppMixerAndRouting => self.handle_apps_key(code),
                TuiTab::MicAndAndroid => self.handle_mic_android_key(code),
                TuiTab::WebBroadcast => self.handle_web_broadcast_key(code),
            },
        }
    }

    fn handle_peers_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                if self.selected_peer_idx > 0 {
                    self.selected_peer_idx -= 1;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected_peer_idx + 1 < self.peers.len() {
                    self.selected_peer_idx += 1;
                }
            }
            KeyCode::Char(' ') => {
                if let Some(peer) = self.peers.get(self.selected_peer_idx) {
                    if self.multiroom_selected.contains(&peer.id) {
                        self.multiroom_selected.remove(&peer.id);
                    } else {
                        self.multiroom_selected.insert(peer.id.clone());
                    }
                }
            }
            KeyCode::Char('l') => {
                self.keep_local = !self.keep_local;
                self.status_message = format!("Keep local speakers: {}", self.keep_local);
            }
            KeyCode::Enter => {
                let targets: Vec<Peer> = if !self.multiroom_selected.is_empty() {
                    self.peers
                        .iter()
                        .filter(|p| self.multiroom_selected.contains(&p.id))
                        .cloned()
                        .collect()
                } else if let Some(p) = self.peers.get(self.selected_peer_idx) {
                    vec![p.clone()]
                } else {
                    Vec::new()
                };

                if targets.is_empty() {
                    self.status_message = "No peer selected to connect.".to_string();
                } else {
                    let res = self.controller.connect_multi(
                        &targets,
                        ConnectOptions {
                            keep_local: self.keep_local,
                            by: "tui".to_string(),
                            notify_user: Some(true),
                            ..Default::default()
                        },
                    );
                    match res {
                        Ok(_) => {
                            let names: Vec<_> = targets.iter().map(|p| p.name.as_str()).collect();
                            self.status_message = format!("Connected to {}", names.join(" + "));
                        }
                        Err(e) => {
                            self.status_message = format!("Connect error: {e}");
                        }
                    }
                }
            }
            KeyCode::Char('d') => {
                match self
                    .controller
                    .disconnect(false, None, true, Some(true), false, false)
                {
                    Ok(_) => self.status_message = "Disconnected remote speaker(s).".to_string(),
                    Err(e) => self.status_message = format!("Disconnect error: {e}"),
                }
            }
            KeyCode::Char('s') => {
                let cur = lock_or_recover(&self.controller.config).sharing.enabled;
                match self.controller.share(!cur, Some(true)) {
                    Ok(_) => {
                        self.status_message = format!(
                            "Speaker sharing {}",
                            if !cur { "enabled" } else { "disabled" }
                        );
                    }
                    Err(e) => self.status_message = format!("Share error: {e}"),
                }
            }
            KeyCode::Char('r') => {
                let cfg = lock_or_recover(&self.controller.config).clone();
                let psk =
                    (!cfg.sharing.psk_hash.is_empty()).then_some(cfg.sharing.psk_hash.as_str());
                let found_with_meta =
                    discover_once_with_meta(0.8, cfg.discovery.port, &cfg.device.id, psk);
                let mut found = Vec::with_capacity(found_with_meta.len());
                for (p, meta) in found_with_meta {
                    if let Some(m) = meta {
                        self.controller.remember_peer_v3_meta(&p.id, m);
                    }
                    self.controller.remember_peer(&p);
                    found.push(p);
                }
                let found_count = found.len();
                self.merge_live_peers(found);
                self.refresh_data();
                self.status_message = format!(
                    "Discovered {found_count} LAN speaker(s) ({} total).",
                    self.peers.len()
                );
            }
            KeyCode::Char('c') | KeyCode::Char('i') => {
                self.manual_ip_prompt = Some(String::new());
                self.status_message =
                    "Type IP[:port] and press Enter to connect (Esc to cancel).".to_string();
            }
            _ => {}
        }
    }

    fn handle_manual_ip_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => {
                self.manual_ip_prompt = None;
                self.status_message = "Cancelled manual IP connect.".to_string();
            }
            KeyCode::Backspace => {
                if let Some(ref mut buf) = self.manual_ip_prompt {
                    buf.pop();
                }
            }
            KeyCode::Enter => {
                let input = self.manual_ip_prompt.take().unwrap_or_default();
                self.connect_manual_address(&input);
            }
            KeyCode::Char(ch) if !ch.is_control() => {
                if let Some(ref mut buf) = self.manual_ip_prompt {
                    buf.push(ch);
                }
            }
            _ => {}
        }
    }

    fn connect_manual_address(&mut self, raw: &str) {
        let (host, port) = split_host_port(raw.trim(), 4713);
        if host.is_empty() {
            self.status_message =
                "Enter an IP address or hostname (e.g. 192.168.1.50).".to_string();
            return;
        }
        let peer = Peer {
            id: format!("manual-{host}-{port}"),
            name: host.clone(),
            host: host.clone(),
            port,
            version: String::new(),
            protocol: soundhop_core::constants::PROTOCOL_VERSION,
            security: "trusted-lan".to_string(),
            platform: "linux".to_string(),
            last_seen: now_secs(),
            source: "manual".to_string(),
            rtt_ms: None,
        };
        match self.controller.connect(
            &peer,
            ConnectOptions {
                keep_local: self.keep_local,
                by: "tui".to_string(),
                notify_user: Some(true),
                ..Default::default()
            },
        ) {
            Ok(_) => {
                self.controller.remember_peer(&peer);
                if !self.peers.iter().any(|p| p.id == peer.id) {
                    self.peers.push(peer);
                }
                self.status_message = format!("Connected to {host}:{port}.");
            }
            Err(e) => {
                self.status_message = format!("Connect error: {e}");
            }
        }
    }

    fn handle_apps_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                if self.selected_app_idx > 0 {
                    self.selected_app_idx -= 1;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected_app_idx + 1 < self.apps.len() {
                    self.selected_app_idx += 1;
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                if let Some(app_val) = self.apps.get(self.selected_app_idx) {
                    let app_name = app_val
                        .get("app_name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let cur_rule = app_val
                        .get("routed_rule")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let next_target = if cur_rule == "remote" {
                        "local"
                    } else {
                        "remote"
                    };
                    match self
                        .controller
                        .route_app(&app_name, Some(next_target), false)
                    {
                        Ok(_) => {
                            self.status_message =
                                format!("Routed '{app_name}' to {next_target} output.");
                            self.apps = self.controller.list_apps();
                        }
                        Err(e) => self.status_message = format!("Route error: {e}"),
                    }
                }
            }
            KeyCode::Char('p') => {
                if let Some(app_val) = self.apps.get(self.selected_app_idx) {
                    let app_name = app_val
                        .get("app_name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let cur_rule = app_val
                        .get("routed_rule")
                        .and_then(|v| v.as_str())
                        .unwrap_or("remote");
                    let target = if cur_rule == "local" {
                        "local"
                    } else {
                        "remote"
                    };
                    match self.controller.route_app(&app_name, Some(target), true) {
                        Ok(_) => {
                            self.status_message = format!(
                                "Saved persistent '{target}' routing rule for '{app_name}'."
                            );
                            self.apps = self.controller.list_apps();
                        }
                        Err(e) => self.status_message = format!("Save rule error: {e}"),
                    }
                }
            }
            _ => {}
        }
    }

    fn handle_mic_android_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('s') => match self.controller.enable_mic("send") {
                Ok(_) => self.status_message = "Sending local microphone to peer.".to_string(),
                Err(e) => self.status_message = format!("Mic send error: {e}"),
            },
            KeyCode::Char('r') => match self.controller.enable_mic("receive") {
                Ok(_) => self.status_message = "Receiving microphone from peer.".to_string(),
                Err(e) => self.status_message = format!("Mic receive error: {e}"),
            },
            KeyCode::Char('o') => match self.controller.disable_mic() {
                Ok(_) => self.status_message = "Microphone pass-through disabled.".to_string(),
                Err(e) => self.status_message = format!("Mic disable error: {e}"),
            },
            KeyCode::Char('a') => {
                if self.controller.android_session.is_active() {
                    let _ = self.controller.android_session.stop();
                    self.status_message = "Stopped Android audio capture.".to_string();
                } else {
                    let devs = self.controller.android_manager.list_devices();
                    if let Some(first) = devs.first() {
                        match self.controller.android_session.start(
                            &first.serial,
                            &first.display_name(),
                            None,
                            None,
                            false,
                            None,
                            None,
                            false,
                        ) {
                            Ok(_) => {
                                self.status_message = format!(
                                    "Started Android audio capture from {}.",
                                    first.display_name()
                                );
                            }
                            Err(e) => self.status_message = format!("Android start error: {e}"),
                        }
                    } else {
                        self.status_message = "No ADB Android devices connected.".to_string();
                    }
                }
            }
            KeyCode::Char('w') => {
                let devs = self.controller.android_manager.list_devices();
                if let Some(usb_dev) = devs.iter().find(|d| !d.is_wifi && d.is_ready()) {
                    if let Some(ip) = self
                        .controller
                        .android_manager
                        .get_device_wifi_ip(&usb_dev.serial)
                    {
                        let (_, msg1) = self
                            .controller
                            .android_manager
                            .enable_tcpip(&usb_dev.serial, 5555);
                        let (ok2, msg2) = self.controller.android_manager.connect_wifi(&ip, 5555);
                        if ok2 {
                            self.status_message = format!(
                                "Switched {} to Wi-Fi ({ip}:5555). You can unplug USB.",
                                usb_dev.display_name()
                            );
                        } else {
                            self.status_message = format!("Wi-Fi switch failed: {msg1} {msg2}");
                        }
                    } else {
                        self.status_message = format!(
                            "Could not detect Wi-Fi IP for {}. Make sure it is on the same Wi-Fi.",
                            usb_dev.display_name()
                        );
                    }
                } else if devs.iter().any(|d| !d.is_wifi) {
                    self.status_message =
                        "USB Android device is not authorized yet — tap Allow on your phone."
                            .to_string();
                } else if devs.iter().any(|d| d.is_wifi) {
                    self.status_message =
                        "Android device is already connected over Wi-Fi.".to_string();
                } else {
                    self.status_message = "No USB Android devices connected.".to_string();
                }
            }
            _ => {}
        }
    }

    fn handle_web_broadcast_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('b') => {
                if self.controller.web_broadcast.is_active() {
                    match self.controller.stop_web_broadcast() {
                        Ok(_) => self.status_message = "Web Broadcast stopped.".to_string(),
                        Err(e) => {
                            self.status_message = format!("Web Broadcast stop error: {e}");
                        }
                    }
                } else {
                    match self
                        .controller
                        .start_web_broadcast(None, Some(self.keep_local), None)
                    {
                        Ok(st) => {
                            self.status_message = format!(
                                "Web Broadcast started on HTTP :{} / WebRTC UDP :{}",
                                st.http_port, st.webrtc_udp_port
                            );
                        }
                        Err(e) => {
                            self.status_message = format!("Web Broadcast start error: {e}");
                        }
                    }
                }
            }
            KeyCode::Char('t') => match self.controller.rotate_web_token() {
                Ok(_) => {
                    self.status_message = "Rotated Web Broadcast session token.".to_string();
                }
                Err(e) => {
                    self.status_message = format!("Rotate token error: {e}");
                }
            },
            KeyCode::Char('r') => {
                let st = self.controller.web_broadcast_status();
                if !st.adb_reversed_serials.is_empty() {
                    match self.controller.web_adb_reverse(None, None, false, false) {
                        Ok(_) => {
                            self.status_message = "Disabled USB adb reverse tunnel.".to_string();
                        }
                        Err(e) => {
                            self.status_message = format!("ADB reverse error: {e}");
                        }
                    }
                } else if !st.active {
                    self.status_message =
                        "Start Web Broadcast ([b]) first before enabling USB adb reverse."
                            .to_string();
                } else {
                    match self
                        .controller
                        .web_adb_reverse(None, Some(st.http_port), true, true)
                    {
                        Ok(next) => {
                            let target = if next.adb_reversed_serials.is_empty() {
                                "phone".to_string()
                            } else {
                                next.adb_reversed_serials.join(", ")
                            };
                            self.status_message = format!(
                                "Enabled USB adb reverse ({target}) & opened phone browser."
                            );
                        }
                        Err(e) => {
                            self.status_message = format!("ADB reverse error: {e}");
                        }
                    }
                }
            }
            KeyCode::Char('l') => {
                self.keep_local = !self.keep_local;
                self.status_message = if self.controller.web_broadcast.is_active() {
                    format!(
                        "Keep local speakers: {} (restart broadcast to apply)",
                        self.keep_local
                    )
                } else {
                    format!("Keep local speakers: {}", self.keep_local)
                };
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.selected_web_client_idx > 0 {
                    self.selected_web_client_idx -= 1;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let count = self.controller.list_web_clients().len();
                if self.selected_web_client_idx + 1 < count {
                    self.selected_web_client_idx += 1;
                }
            }
            KeyCode::Char('x') => {
                let clients = self.controller.list_web_clients();
                if let Some(client) = clients.get(self.selected_web_client_idx) {
                    match self.controller.kick_web_client(&client.id) {
                        Ok(true) => {
                            self.status_message =
                                format!("Disconnected listener {}.", client.remote_addr);
                        }
                        Ok(false) => {
                            self.status_message =
                                format!("Listener {} already disconnected.", client.id);
                        }
                        Err(e) => {
                            self.status_message = format!("Kick listener error: {e}");
                        }
                    }
                } else {
                    self.status_message = "No Web Broadcast listener selected.".to_string();
                }
            }
            _ => {}
        }
    }
}

pub fn draw_tui(frame: &mut Frame<'_>, state: &TuiState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(10),
            Constraint::Length(4),
            Constraint::Length(3),
        ])
        .split(frame.area());

    draw_top_bar(frame, chunks[0], state);
    match state.tab {
        TuiTab::PeersAndMultiRoom => draw_peers_tab(frame, chunks[1], state),
        TuiTab::AppMixerAndRouting => draw_apps_tab(frame, chunks[1], state),
        TuiTab::MicAndAndroid => draw_mic_android_tab(frame, chunks[1], state),
        TuiTab::WebBroadcast => draw_web_broadcast_tab(frame, chunks[1], state),
    }
    draw_telemetry_bar(frame, chunks[2], state);
    draw_footer_bar(frame, chunks[3], state);

    if let Some(req) = state.pending_pair_requests.first() {
        draw_pair_request_modal(frame, frame.area(), req, state.pending_pair_requests.len());
    }
}

fn centered_rect(percent_x: u16, height: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Fill(1),
            Constraint::Length(height.min(area.height)),
            Constraint::Fill(1),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

fn draw_pair_request_modal(
    frame: &mut Frame<'_>,
    area: Rect,
    req: &PendingPairRequest,
    total_pending: usize,
) {
    let popup_area = centered_rect(72, 10, area);
    frame.render_widget(Clear, popup_area);

    let title = if req.outgoing {
        " Verify Pairing Code "
    } else {
        " Incoming Pair Request "
    };
    let headline = if req.outgoing {
        format!(
            "Pairing with {} — confirm code matches on both devices",
            req.display_name
        )
    } else {
        format!("{} wants to pair", req.display_name)
    };
    let approve_lbl = if req.outgoing {
        "[a] Codes Match — Confirm"
    } else {
        "[a] Approve"
    };
    let deny_lbl = if req.outgoing {
        "[d] Cancel (Mismatch)"
    } else {
        "[d] Deny"
    };

    let mut lines = vec![
        Line::from(Span::styled(
            headline,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("Verification Code: ", Style::default().fg(Color::Gray)),
            Span::styled(
                &req.sas_code,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(Span::styled(
            format!(
                "From: {}  •  Fingerprint: {}",
                req.remote_addr, req.fingerprint
            ),
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                approve_lbl,
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("     "),
            Span::styled(
                deny_lbl,
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
        ]),
    ];
    if total_pending > 1 {
        lines.push(Line::from(Span::styled(
            format!("(+{} more pending)", total_pending - 1),
            Style::default().fg(Color::Cyan),
        )));
    }

    let modal = Paragraph::new(lines).alignment(Alignment::Center).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow))
            .title(title),
    );
    frame.render_widget(modal, popup_area);
}

fn draw_top_bar(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let titles = vec![
        Line::from(" [1] Speakers "),
        Line::from(" [2] Apps "),
        Line::from(" [3] Phone & Mic "),
        Line::from(" [4] Web Broadcast "),
    ];
    let tabs = Tabs::new(titles)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" SoundHop Control Dashboard "),
        )
        .select(state.tab.index())
        .style(Style::default().fg(Color::Cyan))
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_widget(tabs, area);
}

fn draw_peers_tab(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
        .split(area);

    let st = lock_or_recover(&state.controller.state).clone();
    let cfg = lock_or_recover(&state.controller.config).clone();

    let items: Vec<ListItem<'_>> = if state.peers.is_empty() {
        vec![ListItem::new(
            "  Looking for nearby speakers... Press 'r' to scan again.",
        )]
    } else {
        state
            .peers
            .iter()
            .enumerate()
            .map(|(idx, p)| {
                let selected_cursor = if idx == state.selected_peer_idx {
                    ">"
                } else {
                    " "
                };
                let checked = if state.multiroom_selected.contains(&p.id) {
                    "[x]"
                } else {
                    "[ ]"
                };
                let is_active = st.connected_peer == p.id
                    || st.multiroom_peers.iter().any(|s| s.peer_id == p.id);
                let conn_tag = if is_active { " (CONNECTED)" } else { "" };
                let rtt_tag = p.rtt_ms.map(|r| format!("  {r:.0} ms")).unwrap_or_default();
                let text = format!(
                    "{selected_cursor} {checked} {} ({}){}{}",
                    p.name, p.host, rtt_tag, conn_tag
                );
                let style = if is_active {
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD)
                } else if idx == state.selected_peer_idx {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default()
                };
                ListItem::new(Line::from(Span::styled(text, style)))
            })
            .collect()
    };

    let list = List::new(items).block(Block::default().borders(Borders::ALL).title(
        " Play Audio On (Space: Multi-Room | Enter: Connect | c/i: Connect IP | r: Refresh) ",
    ));
    if let Some(ref ip_buf) = state.manual_ip_prompt {
        let left_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(3)])
            .split(cols[0]);
        frame.render_widget(list, left_chunks[0]);
        let prompt = Paragraph::new(Line::from(vec![
            Span::styled(" > ", Style::default().fg(Color::Yellow)),
            Span::styled(
                format!("{ip_buf}_"),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        ]))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow))
                .title(" Connect by IP[:port] (Enter: Connect | Esc: Cancel) "),
        );
        frame.render_widget(prompt, left_chunks[1]);
    } else {
        frame.render_widget(list, cols[0]);
    }

    let privacy_label = match cfg.sharing.security.as_str() {
        "psk" => "PIN-Paired Only",
        "cookie" => "System Cookie",
        "open" => "Open",
        _ => "Home Wi-Fi",
    };

    let summary_lines = vec![
        Line::from(vec![
            Span::styled("Device Name: ", Style::default().fg(Color::Gray)),
            Span::styled(
                &cfg.device.name,
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("Share My Speakers: ", Style::default().fg(Color::Gray)),
            Span::styled(
                if cfg.sharing.enabled {
                    "On (Visible on Wi-Fi)"
                } else {
                    "Off"
                },
                Style::default().fg(if cfg.sharing.enabled {
                    Color::Green
                } else {
                    Color::Red
                }),
            ),
        ]),
        Line::from(vec![
            Span::styled("Privacy: ", Style::default().fg(Color::Gray)),
            Span::raw(privacy_label),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("Playing On: ", Style::default().fg(Color::Gray)),
            Span::styled(
                if st.is_connected() {
                    &st.connected_name
                } else {
                    "This Device"
                },
                Style::default().fg(Color::Cyan),
            ),
        ]),
        Line::from(vec![
            Span::styled("Mode: ", Style::default().fg(Color::Gray)),
            Span::raw(if st.is_multiroom() {
                "Multiple Speakers"
            } else {
                "Single Speaker"
            }),
        ]),
        Line::from(vec![
            Span::styled("Keep Playing Here (l): ", Style::default().fg(Color::Gray)),
            Span::raw(if state.keep_local { "Yes" } else { "No" }),
        ]),
    ];

    let right_panel = Paragraph::new(summary_lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" This Device (s: Share | d: Disconnect) "),
    );
    frame.render_widget(right_panel, cols[1]);
}

fn draw_apps_tab(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let items: Vec<ListItem<'_>> = if state.apps.is_empty() {
        vec![ListItem::new("  No apps are playing audio right now.")]
    } else {
        state
            .apps
            .iter()
            .enumerate()
            .map(|(idx, app)| {
                let cursor = if idx == state.selected_app_idx {
                    ">"
                } else {
                    " "
                };
                let name = app
                    .get("app_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("App");
                let media = app.get("media_name").and_then(|v| v.as_str()).unwrap_or("");
                let rule = app
                    .get("routed_rule")
                    .and_then(|v| v.as_str())
                    .unwrap_or("default");
                let persistent = app
                    .get("persistent")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let badge = if rule == "remote" {
                    "[REMOTE]"
                } else {
                    "[THIS PC]"
                };
                let saved_tag = if persistent { " (Remembered)" } else { "" };
                let line_str = format!("{cursor} {badge:<10} {name} — {media}{saved_tag}");
                let style = if idx == state.selected_app_idx {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else if rule == "remote" {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default()
                };
                ListItem::new(Line::from(Span::styled(line_str, style)))
            })
            .collect()
    };

    let list =
        List::new(items).block(Block::default().borders(Borders::ALL).title(
            " Choose Where Apps Play (Enter: Switch This PC / Remote | p: Always Remember) ",
        ));
    frame.render_widget(list, area);
}

fn draw_mic_android_tab(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let st = lock_or_recover(&state.controller.state).clone();
    let and_st = state.controller.android_session.status();
    let and_active = and_st
        .get("active")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let and_dev = and_st
        .get("device_name")
        .and_then(|v| v.as_str())
        .unwrap_or("None");

    let text = vec![
        Line::from(Span::styled(
            "Microphone Sharing",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(format!(
            "  Status: {} | Mode: {}",
            if !st.connected_source.is_empty() {
                "Active"
            } else {
                "Off"
            },
            if st.mic_direction.is_empty() {
                "none"
            } else if st.mic_direction == "send" {
                "Sending my mic"
            } else {
                "Hearing peer mic"
            }
        )),
        Line::from("  Keys: [s] Send My Mic | [r] Hear Peer Mic | [o] Turn Off"),
        Line::from(""),
        Line::from(Span::styled(
            "Phone Audio (Android)",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(format!(
            "  Listening: {} | Phone: {}",
            if and_active { "Yes" } else { "No" },
            and_dev
        )),
        Line::from("  Keys: [a] Start / Stop Listening | [w] Switch USB to Wi-Fi"),
    ];

    let block = Paragraph::new(text).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Phone Audio & Microphone Sharing "),
    );
    frame.render_widget(block, area);
}

fn format_web_client_row(client: &WebClientInfo, selected: bool) -> String {
    let cursor = if selected { ">" } else { " " };
    let badge = if client.transport.eq_ignore_ascii_case("webrtc") {
        "[WebRTC]"
    } else if client.transport.eq_ignore_ascii_case("ws") {
        "[WS]"
    } else {
        "[OTHER]"
    };
    let rtt = client
        .rtt_ms
        .map(|r| format!("{r:.1} ms"))
        .unwrap_or_else(|| "-".to_string());
    format!(
        "{cursor} {badge:<8} {:<20} RTT: {:<7}  {}",
        client.remote_addr,
        rtt,
        format_bytes(client.bytes_sent)
    )
}

fn draw_web_broadcast_tab(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let wb = state.controller.web_broadcast_status();
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(46), Constraint::Percentage(54)])
        .split(area);

    let primary_url = if !wb.adb_reversed_serials.is_empty() && !wb.localhost_url.is_empty() {
        wb.localhost_url.clone()
    } else {
        wb.urls
            .first()
            .cloned()
            .unwrap_or_else(|| wb.localhost_url.clone())
    };

    let qr_lines: Vec<Line<'_>> = if wb.active && !primary_url.is_empty() {
        match render_terminal_qr(&primary_url) {
            Ok(qr) => qr
                .lines()
                .map(|l| Line::from(Span::raw(l.to_string())))
                .collect(),
            Err(e) => vec![Line::from(Span::styled(
                format!("QR error: {e}"),
                Style::default().fg(Color::Red),
            ))],
        }
    } else {
        vec![
            Line::from(""),
            Line::from(Span::styled(
                "Web Broadcast is Off.",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(""),
            Line::from("Press [b] to start broadcasting"),
            Line::from("PC audio to phone/browser listeners."),
        ]
    };

    let qr_block = Paragraph::new(qr_lines).alignment(Alignment::Center).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Listen QR Code "),
    );
    frame.render_widget(qr_block, cols[0]);

    let right_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(10), Constraint::Min(4)])
        .split(cols[1]);

    let state_span = if wb.active {
        Span::styled(
            format!(
                "On (HTTP :{} | WebRTC UDP :{})",
                wb.http_port, wb.webrtc_udp_port
            ),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(
            "Off",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )
    };
    let pipeline_label = if wb.active {
        if wb.pipeline_running {
            "Running"
        } else {
            "Idle (0 listeners)"
        }
    } else {
        "Stopped"
    };
    let token_label = if !wb.active {
        "-".to_string()
    } else if wb.token_required {
        wb.token.clone().unwrap_or_else(|| "<hidden>".to_string())
    } else {
        "Disabled".to_string()
    };
    let url_display = if wb.active && !primary_url.is_empty() {
        primary_url
    } else {
        "(inactive)".to_string()
    };
    let adb_label = if wb.adb_reversed_serials.is_empty() {
        "Off".to_string()
    } else {
        format!("On ({})", wb.adb_reversed_serials.join(", "))
    };
    let keep_local_display = if wb.active {
        wb.keep_local
    } else {
        state.keep_local
    };

    let info_lines = vec![
        Line::from(vec![
            Span::styled("Web Broadcast: ", Style::default().fg(Color::Gray)),
            state_span,
        ]),
        Line::from(format!(
            "Pipeline: {pipeline_label} | Keep Local (l): {}",
            if keep_local_display { "Yes" } else { "No" }
        )),
        Line::from(format!("Token: {token_label}")),
        Line::from(vec![
            Span::styled("URL: ", Style::default().fg(Color::Gray)),
            Span::styled(url_display, Style::default().fg(Color::Cyan)),
        ]),
        Line::from(format!(
            "Localhost: {}",
            if wb.localhost_url.is_empty() {
                "-"
            } else {
                &wb.localhost_url
            }
        )),
        Line::from(format!("ADB Reverse: {adb_label}")),
        Line::from(Span::styled(
            "Keys: [b] Toggle  [t] Rotate Token  [r] ADB Reverse+Open  [x] Kick",
            Style::default().fg(Color::Yellow),
        )),
    ];
    let info_block = Paragraph::new(info_lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Web Broadcast (Listen on Phone) "),
    );
    frame.render_widget(info_block, right_rows[0]);

    let listener_items: Vec<ListItem<'_>> = if wb.clients.is_empty() {
        vec![ListItem::new("  No browser listeners connected.")]
    } else {
        wb.clients
            .iter()
            .enumerate()
            .map(|(idx, client)| {
                let selected = idx == state.selected_web_client_idx;
                let row = format_web_client_row(client, selected);
                let style = if selected {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else if client.transport.eq_ignore_ascii_case("webrtc") {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default().fg(Color::Cyan)
                };
                ListItem::new(Line::from(Span::styled(row, style)))
            })
            .collect()
    };
    let listeners_block = List::new(listener_items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" Listeners ({}) ", wb.clients.len())),
    );
    frame.render_widget(listeners_block, right_rows[1]);
}

fn draw_telemetry_bar(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let snap = state.controller.telemetry_snapshot();
    let ratio = ((snap.audio.dbfs + 60.0) / 60.0).clamp(0.0, 1.0) as f64;
    let first_net = snap.peers.values().next();
    let peak_max = snap.audio.peak_left.max(snap.audio.peak_right);
    let net_label = if let Some(n) = first_net {
        format!(
            "Signal: {:.1} ms RTT | Jitter: {:.1} ms | Level: {:.1} dBFS",
            n.last_rtt_ms, n.jitter_ms, snap.audio.dbfs
        )
    } else {
        format!(
            "Audio Level: {:.1} dBFS (L: {:.0}% R: {:.0}%)",
            snap.audio.dbfs,
            snap.audio.peak_left * 100.0,
            snap.audio.peak_right * 100.0
        )
    };

    let gauge_color = if peak_max >= 0.95 {
        Color::Red
    } else if peak_max >= 0.75 {
        Color::Yellow
    } else {
        Color::Green
    };

    let gauge = Gauge::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Audio Level & Connection Health "),
        )
        .gauge_style(Style::default().fg(gauge_color).bg(Color::Black))
        .ratio(ratio)
        .label(net_label);
    frame.render_widget(gauge, area);
}

fn draw_footer_bar(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let footer = Paragraph::new(Line::from(vec![
        Span::styled(" Status: ", Style::default().fg(Color::Cyan)),
        Span::raw(&state.status_message),
        Span::styled(
            "  [+/-: Vol | m: Mute | Tab: View | q: Quit]",
            Style::default().fg(Color::DarkGray),
        ),
    ]))
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(footer, area);
}

type PanicHook = Arc<dyn Fn(&std::panic::PanicHookInfo<'_>) + Send + Sync + 'static>;

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
}

struct TerminalGuard {
    active: bool,
    prev_hook: Option<PanicHook>,
}

impl TerminalGuard {
    fn enter() -> Result<Self, String> {
        enable_raw_mode().map_err(|e| format!("Failed to enable terminal raw mode: {e}"))?;
        if let Err(e) = execute!(io::stdout(), EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(format!("Failed to enter alternate screen: {e}"));
        }
        let prev_hook: PanicHook = Arc::from(std::panic::take_hook());
        let prev_for_panic = Arc::clone(&prev_hook);
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            prev_for_panic(info);
        }));
        Ok(Self {
            active: true,
            prev_hook: Some(prev_hook),
        })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.active {
            restore_terminal();
            if !std::thread::panicking() {
                if let Some(prev) = self.prev_hook.take() {
                    std::panic::set_hook(Box::new(move |info| prev(info)));
                }
            }
            self.active = false;
        }
    }
}

fn ensure_interactive_tty(stdin_is_tty: bool, stdout_is_tty: bool) -> Result<(), String> {
    if !stdin_is_tty || !stdout_is_tty {
        return Err(
            "TUI requires an interactive terminal (stdin and stdout must be TTYs)".to_string(),
        );
    }
    Ok(())
}

#[cfg(unix)]
fn fd_hung_up(fd: std::os::unix::io::RawFd) -> bool {
    if fd < 0 {
        return true;
    }
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let rc = unsafe { libc::poll(&mut pfd, 1, 0) };
    if rc < 0 {
        let err = io::Error::last_os_error();
        return err.kind() != io::ErrorKind::Interrupted;
    }
    rc > 0 && (pfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL)) != 0
}

#[cfg(unix)]
fn stdin_hung_up() -> bool {
    fd_hung_up(libc::STDIN_FILENO)
}

#[cfg(not(unix))]
fn stdin_hung_up() -> bool {
    false
}

fn is_terminal_disconnect_error(err: &io::Error) -> bool {
    let kind_disconnected = matches!(
        err.kind(),
        io::ErrorKind::UnexpectedEof | io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
    );
    #[cfg(unix)]
    {
        kind_disconnected
            || matches!(
                err.raw_os_error(),
                Some(libc::EIO | libc::EBADF | libc::ENXIO)
            )
    }
    #[cfg(not(unix))]
    {
        kind_disconnected
    }
}

fn should_exit_tui_loop(should_quit: bool, signal_stop: bool, hung_up: bool) -> bool {
    should_quit || signal_stop || hung_up
}

/// Returns `Ok(true)` if the TUI loop should exit cleanly, `Ok(false)` to continue
/// after a transient `EINTR`, or `Err` for fatal I/O errors.
fn should_break_on_event_error(err: &io::Error) -> Result<bool, String> {
    if err.kind() == io::ErrorKind::Interrupted {
        return Ok(signal_stop_requested());
    }
    if is_terminal_disconnect_error(err) || signal_stop_requested() || stdin_hung_up() {
        return Ok(true);
    }
    Err(err.to_string())
}

pub fn run_tui(controller: Arc<Controller>) -> Result<i32, String> {
    ensure_interactive_tty(io::stdin().is_terminal(), io::stdout().is_terminal())?;
    install_signal_handlers();
    let cfg = lock_or_recover(&controller.config).clone();
    if cfg.sharing.enabled {
        let _ = controller.start_listener(Some(false));
    }
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
    let ctrl_cb = Arc::clone(&controller);
    let disc_store = Arc::clone(&disc.store);
    disc.store.set_on_peer(move |peer| {
        if let Some(meta) = disc_store.v3_meta(&peer.id) {
            ctrl_cb.remember_peer_v3_meta(&peer.id, meta);
        }
        ctrl_cb.remember_peer(peer);
    });
    let _ = disc.start(cfg.sharing.enabled);
    let telem_monitor = controller.start_telemetry_monitor();

    let _term_guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal =
        Terminal::new(backend).map_err(|e| format!("Failed to initialize TUI terminal: {e}"))?;

    let mut tui_state = TuiState::new(Arc::clone(&controller));
    tui_state.merge_live_peers(disc.peers());
    tui_state.refresh_data();
    let mut last_tick = Instant::now();

    let loop_res: Result<(), String> = (|| {
        while !should_exit_tui_loop(
            tui_state.should_quit,
            signal_stop_requested(),
            stdin_hung_up(),
        ) {
            terminal
                .draw(|f| draw_tui(f, &tui_state))
                .map_err(|e| e.to_string())?;

            if should_exit_tui_loop(
                tui_state.should_quit,
                signal_stop_requested(),
                stdin_hung_up(),
            ) {
                break;
            }

            match event::poll(Duration::from_millis(50)) {
                Ok(true) => {
                    if should_exit_tui_loop(
                        tui_state.should_quit,
                        signal_stop_requested(),
                        stdin_hung_up(),
                    ) {
                        break;
                    }
                    match event::read() {
                        Ok(Event::Key(key)) => {
                            if key.kind == KeyEventKind::Press {
                                if key.code == KeyCode::Char('c')
                                    && key.modifiers.contains(KeyModifiers::CONTROL)
                                {
                                    tui_state.should_quit = true;
                                } else {
                                    tui_state.handle_key(key.code);
                                    let share_now =
                                        lock_or_recover(&controller.config).sharing.enabled;
                                    disc.set_advertising(share_now);
                                    tui_state.merge_live_peers(disc.peers());
                                }
                            }
                        }
                        Ok(_) => {}
                        Err(e) => {
                            if should_break_on_event_error(&e)? {
                                break;
                            }
                        }
                    }
                }
                Ok(false) => {}
                Err(e) => {
                    if should_break_on_event_error(&e)? {
                        break;
                    }
                }
            }

            if last_tick.elapsed() >= Duration::from_millis(1200) {
                if disc.peers().is_empty() {
                    disc.probe();
                }
                tui_state.merge_live_peers(disc.peers());
                tui_state.refresh_data();
                last_tick = Instant::now();
            }
        }
        Ok(())
    })();

    if controller.web_broadcast.is_active() {
        let _ = controller.stop_web_broadcast();
    }
    telem_monitor.stop();
    disc.stop();
    drop(_term_guard);

    loop_res?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use soundhop_core::backend::linux::LinuxAudioBackend;
    use soundhop_core::config::Config;
    use soundhop_core::state::State;
    use soundhop_core::util::{CommandResult, MockRunner};

    #[test]
    fn test_headless_tui_render_and_navigation() {
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

        let mut tui_state = TuiState::new(ctrl);
        let test_backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(test_backend).unwrap();

        terminal.draw(|f| draw_tui(f, &tui_state)).unwrap();
        tui_state.handle_key(KeyCode::Tab);
        assert_eq!(tui_state.tab, TuiTab::AppMixerAndRouting);
        terminal.draw(|f| draw_tui(f, &tui_state)).unwrap();

        tui_state.handle_key(KeyCode::Tab);
        assert_eq!(tui_state.tab, TuiTab::MicAndAndroid);
        terminal.draw(|f| draw_tui(f, &tui_state)).unwrap();

        tui_state.handle_key(KeyCode::Char('q'));
        assert!(tui_state.should_quit);
    }

    #[test]
    fn test_merge_live_peers_preserves_existing_private_peers() {
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

        let mut tui_state = TuiState::new(ctrl);
        let existing = Peer {
            id: "peer-old".to_string(),
            name: "Bedroom".to_string(),
            host: "192.168.10.20".to_string(),
            port: 4713,
            version: soundhop_core::constants::VERSION.to_string(),
            protocol: soundhop_core::constants::PROTOCOL_VERSION,
            security: "trusted-lan".to_string(),
            platform: "linux".to_string(),
            last_seen: 100.0,
            source: "udp".to_string(),
            rtt_ms: Some(1.5),
        };
        tui_state.peers = vec![existing.clone()];
        tui_state.merge_live_peers(Vec::new());
        assert_eq!(tui_state.peers.len(), 1);
        assert_eq!(tui_state.peers[0].id, existing.id);
    }

    #[test]
    fn test_tui_manual_ip_connect_prompt_render_cancel_and_submit() {
        let runner = Arc::new(MockRunner::new(|args, _| {
            let joined = args.join(" ");
            let stdout = if joined.contains("load-module") {
                "42\n".to_string()
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
            Some(Config::default()),
            Some(State::default()),
            Some(backend),
            None,
            None,
            false,
        ));
        ctrl.set_save_enabled(false);

        let mut tui_state = TuiState::new(ctrl);
        let test_backend = TestBackend::new(110, 30);
        let mut terminal = Terminal::new(test_backend).unwrap();

        // Open prompt with 'c' on Tab 1
        tui_state.handle_key(KeyCode::Char('c'));
        assert_eq!(tui_state.manual_ip_prompt.as_deref(), Some(""));

        // Typing 'q' inside the prompt must append to input, NOT quit
        tui_state.handle_key(KeyCode::Char('q'));
        assert!(!tui_state.should_quit);
        assert_eq!(tui_state.manual_ip_prompt.as_deref(), Some("q"));

        // Backspace removes 'q', then Esc cancels
        tui_state.handle_key(KeyCode::Backspace);
        assert_eq!(tui_state.manual_ip_prompt.as_deref(), Some(""));
        tui_state.handle_key(KeyCode::Esc);
        assert!(tui_state.manual_ip_prompt.is_none());
        assert!(tui_state.status_message.contains("Cancelled"));

        // Reopen with 'i', type an IP:port, render with TestBackend, and submit with Enter
        tui_state.handle_key(KeyCode::Char('i'));
        for ch in "192.168.1.50:4713".chars() {
            tui_state.handle_key(KeyCode::Char(ch));
        }
        assert_eq!(
            tui_state.manual_ip_prompt.as_deref(),
            Some("192.168.1.50:4713")
        );

        terminal.draw(|f| draw_tui(f, &tui_state)).unwrap();
        let buf_text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(buf_text.contains("Connect by IP[:port]"));
        assert!(buf_text.contains("192.168.1.50:4713"));

        tui_state.handle_key(KeyCode::Enter);
        assert!(tui_state.manual_ip_prompt.is_none());
        assert!(
            tui_state.status_message.contains("192.168.1.50")
                || tui_state.status_message.contains("Connect error"),
            "unexpected status after manual submit: {}",
            tui_state.status_message
        );
    }

    #[test]
    fn test_tui_android_wifi_switch_key() {
        use soundhop_core::android::AndroidManager;

        let dir = tempfile::tempdir().unwrap();
        let fake_adb = dir.path().join("adb");
        std::fs::write(&fake_adb, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake_adb, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let adb_calls = Arc::new(std::sync::Mutex::new(Vec::<Vec<String>>::new()));
        let adb_calls_cb = Arc::clone(&adb_calls);

        let adb_runner = Arc::new(MockRunner::new(move |args, _| {
            adb_calls_cb.lock().unwrap().push(args.to_vec());
            let joined = args.join(" ");
            let stdout = if joined.contains("devices -l") {
                "List of devices attached\nUSB12345               device usb:1-1 product:lynx model:Pixel_7a device:lynx\n"
                    .to_string()
            } else if joined.contains("ro.product.manufacturer") {
                "Google\n".to_string()
            } else if joined.contains("ro.build.version.release") {
                "14\n".to_string()
            } else if joined.contains("ip route") {
                "192.168.1.0/24 dev wlan0 proto kernel scope link src 192.168.1.99\n".to_string()
            } else if joined.contains("tcpip 5555") {
                "restarting in TCP mode port: 5555\n".to_string()
            } else if joined.contains("connect 192.168.1.99:5555") {
                "connected to 192.168.1.99:5555\n".to_string()
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

        let audio_runner = Arc::new(MockRunner::new(|args, _| {
            Ok(CommandResult {
                args: args.to_vec(),
                returncode: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }));
        let backend = Arc::new(LinuxAudioBackend::with_runner(audio_runner));
        let mut ctrl = Controller::new(
            Some(Config::default()),
            Some(State::default()),
            Some(backend),
            None,
            None,
            false,
        );
        ctrl.android_manager = Arc::new(AndroidManager::with_runner(
            "",
            &fake_adb.to_string_lossy(),
            adb_runner,
        ));
        let ctrl = Arc::new(ctrl);
        ctrl.set_save_enabled(false);

        let mut tui_state = TuiState::new(ctrl);
        let test_backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(test_backend).unwrap();

        tui_state.handle_key(KeyCode::Char('3'));
        assert_eq!(tui_state.tab, TuiTab::MicAndAndroid);
        terminal.draw(|f| draw_tui(f, &tui_state)).unwrap();
        let buf_text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(buf_text.contains("[w] Switch USB to Wi-Fi"));

        tui_state.handle_key(KeyCode::Char('w'));
        assert!(
            tui_state.status_message.contains("192.168.1.99:5555"),
            "expected Wi-Fi switch confirmation, got: {}",
            tui_state.status_message
        );
        let calls = adb_calls.lock().unwrap();
        assert!(calls.iter().any(|c| c.join(" ").contains("tcpip 5555")));
        assert!(calls
            .iter()
            .any(|c| c.join(" ").contains("connect 192.168.1.99:5555")));
    }

    #[test]
    fn test_tui_pending_pair_request_modal_render_approve_and_deny() {
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

        let mut tui_state = TuiState::new(ctrl);
        tui_state.pending_pair_requests = vec![
            PendingPairRequest {
                request_id: "req-approve".to_string(),
                device_id: "dev-1".to_string(),
                display_name: "Living Room Laptop".to_string(),
                platform: "linux".to_string(),
                remote_addr: "192.168.1.42:54321".to_string(),
                fingerprint: "11:22:33:44".to_string(),
                sas_code: "482-913".to_string(),
                created_at: 100.0,
                expires_at: 160.0,
                outgoing: false,
            },
            PendingPairRequest {
                request_id: "req-deny".to_string(),
                device_id: "dev-2".to_string(),
                display_name: "Guest Phone".to_string(),
                platform: "android".to_string(),
                remote_addr: "192.168.1.43:54322".to_string(),
                fingerprint: "55:66:77:88".to_string(),
                sas_code: "901-234".to_string(),
                created_at: 101.0,
                expires_at: 161.0,
                outgoing: true,
            },
        ];

        let test_backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(test_backend).unwrap();
        terminal.draw(|f| draw_tui(f, &tui_state)).unwrap();

        let buf_text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(buf_text.contains("Incoming Pair Request"));
        assert!(buf_text.contains("Living Room Laptop wants to pair"));
        assert!(buf_text.contains("482-913"));
        assert!(buf_text.contains("[a] Approve"));
        assert!(buf_text.contains("[d] Deny"));

        // Press 'a' to approve the first request
        tui_state.handle_key(KeyCode::Char('a'));
        assert_eq!(tui_state.pending_pair_requests.len(), 1);
        assert!(tui_state
            .status_message
            .contains("Approved pairing request from Living Room Laptop"));

        // Press 'd' to deny the second request
        tui_state.handle_key(KeyCode::Char('d'));
        assert!(tui_state.pending_pair_requests.is_empty());
        assert!(tui_state
            .status_message
            .contains("Denied pairing request from Guest Phone"));
    }

    #[test]
    fn test_ensure_interactive_tty_requires_both_ttys() {
        assert!(ensure_interactive_tty(true, true).is_ok());
        assert!(ensure_interactive_tty(false, true).is_err());
        assert!(ensure_interactive_tty(true, false).is_err());
        assert!(ensure_interactive_tty(false, false).is_err());
    }

    #[test]
    fn test_tui_shutdown_and_hangup_exit_conditions() {
        assert!(!should_exit_tui_loop(false, false, false));
        assert!(should_exit_tui_loop(true, false, false));
        assert!(should_exit_tui_loop(false, true, false));
        assert!(should_exit_tui_loop(false, false, true));

        assert!(is_terminal_disconnect_error(&io::Error::from(
            io::ErrorKind::UnexpectedEof
        )));
        assert!(is_terminal_disconnect_error(&io::Error::from(
            io::ErrorKind::BrokenPipe
        )));
        assert!(!is_terminal_disconnect_error(&io::Error::from(
            io::ErrorKind::TimedOut
        )));
    }

    #[cfg(unix)]
    #[test]
    fn test_fd_hung_up_detects_pipe_close_and_invalid_fd() {
        let mut fds = [-1; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let read_fd = fds[0];
        let write_fd = fds[1];

        assert!(!fd_hung_up(read_fd));

        unsafe {
            libc::close(write_fd);
        }
        assert!(fd_hung_up(read_fd));

        unsafe {
            libc::close(read_fd);
        }
        assert!(fd_hung_up(-1));
        assert!(is_terminal_disconnect_error(&io::Error::from_raw_os_error(
            libc::EIO
        )));
    }

    #[test]
    fn test_tui_web_broadcast_tab_render_and_keybindings() {
        use soundhop_core::android::AndroidManager;

        let dir = tempfile::tempdir().unwrap();
        let fake_adb = dir.path().join("adb");
        std::fs::write(&fake_adb, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake_adb, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let adb_calls = Arc::new(std::sync::Mutex::new(Vec::<Vec<String>>::new()));
        let adb_calls_cb = Arc::clone(&adb_calls);
        let adb_runner = Arc::new(MockRunner::new(move |args, _| {
            adb_calls_cb.lock().unwrap().push(args.to_vec());
            let joined = args.join(" ");
            let stdout = if joined.contains("devices -l") {
                "List of devices attached\nUSB777                 device usb:1-1 product:lynx model:Pixel_7a device:lynx\n"
                    .to_string()
            } else if joined.contains("ro.product.manufacturer") {
                "Google\n".to_string()
            } else if joined.contains("ro.build.version.release") {
                "14\n".to_string()
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

        let audio_runner = Arc::new(MockRunner::new(|args, _| {
            let joined = args.join(" ");
            let stdout = if joined.ends_with(" info") {
                "Server Name: PulseAudio (on PipeWire 1.2.0)\nDefault Sink: alsa_output.pci\n"
                    .to_string()
            } else if joined.contains("load-module") {
                "201\n".to_string()
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
        let backend = Arc::new(LinuxAudioBackend::with_runner(audio_runner));
        let mut cfg = Config::default();
        cfg.sharing.web_port = 0;
        cfg.sharing.webrtc_port = 0;

        let mut ctrl = Controller::new(
            Some(cfg),
            Some(State::default()),
            Some(backend),
            None,
            None,
            false,
        );
        ctrl.android_manager = Arc::new(AndroidManager::with_runner(
            "",
            &fake_adb.to_string_lossy(),
            adb_runner,
        ));
        let ctrl = Arc::new(ctrl);
        ctrl.set_save_enabled(false);

        let mut tui_state = TuiState::new(Arc::clone(&ctrl));
        let test_backend = TestBackend::new(110, 32);
        let mut terminal = Terminal::new(test_backend).unwrap();

        // Switch via '4' to Web Broadcast tab
        tui_state.handle_key(KeyCode::Char('4'));
        assert_eq!(tui_state.tab, TuiTab::WebBroadcast);

        // Press 'b' to start Web Broadcast
        tui_state.handle_key(KeyCode::Char('b'));
        let started = ctrl.web_broadcast_status();
        assert!(started.active);
        let first_tok = started.token.clone().expect("token should be generated");

        terminal.draw(|f| draw_tui(f, &tui_state)).unwrap();
        let buf_text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(buf_text.contains("[4] Web Broadcast"));
        assert!(buf_text.contains("On (HTTP :"));
        assert!(buf_text.contains("[b] Toggle"));
        assert!(buf_text.contains("[t] Rotate Token"));
        assert!(buf_text.contains("[r] ADB Reverse+Open"));
        assert!(
            buf_text.chars().any(|c| matches!(c, '▀' | '▄' | '█')),
            "expected UTF-8 half-block QR code in Web Broadcast tab"
        );

        // Press 't' to rotate token
        tui_state.handle_key(KeyCode::Char('t'));
        let rotated = ctrl.web_broadcast_status();
        assert_ne!(rotated.token.as_deref(), Some(first_tok.as_str()));

        // Press 'r' to enable USB adb reverse + open phone browser, then 'r' again to disable
        tui_state.handle_key(KeyCode::Char('r'));
        assert!(ctrl
            .web_broadcast_status()
            .adb_reversed_serials
            .contains(&"USB777".to_string()));
        {
            let calls = adb_calls.lock().unwrap();
            assert!(calls.iter().any(|c| c.join(" ").contains("reverse tcp:")));
            assert!(calls
                .iter()
                .any(|c| c.join(" ").contains("android.intent.action.VIEW")));
        }

        tui_state.handle_key(KeyCode::Char('r'));
        assert!(ctrl.web_broadcast_status().adb_reversed_serials.is_empty());

        // Press 'b' to stop Web Broadcast
        tui_state.handle_key(KeyCode::Char('b'));
        assert!(!ctrl.web_broadcast_status().active);

        // Verify listener row formatting
        let webrtc_row = format_web_client_row(
            &WebClientInfo {
                id: "rtc-1".to_string(),
                remote_addr: "192.168.1.50:55555".to_string(),
                transport: "webrtc".to_string(),
                user_agent: "Chrome".to_string(),
                connected_since: 1.0,
                bytes_sent: 2048,
                rtt_ms: Some(12.4),
                dropped_frames: 0,
            },
            true,
        );
        assert!(webrtc_row.starts_with('>'));
        assert!(webrtc_row.contains("[WebRTC]"));
        assert!(webrtc_row.contains("192.168.1.50:55555"));
        assert!(webrtc_row.contains("12.4 ms"));
        assert!(webrtc_row.contains("2.0 KiB"));

        let ws_row = format_web_client_row(
            &WebClientInfo {
                id: "ws-2".to_string(),
                remote_addr: "127.0.0.1:41000".to_string(),
                transport: "ws".to_string(),
                user_agent: "Firefox".to_string(),
                connected_since: 2.0,
                bytes_sent: 512,
                rtt_ms: None,
                dropped_frames: 1,
            },
            false,
        );
        assert!(ws_row.starts_with(' '));
        assert!(ws_row.contains("[WS]"));
        assert!(ws_row.contains("512 B"));
    }
}
