//! Interactive Terminal UI (`soundhop tui`) built with `ratatui` + `crossterm`.
//! Features:
//! - Tab 1: Peers & Multi-Room Broadcast (`Space` to toggle peer in multi-room group, `Enter` to connect, `d` to disconnect, `s` to toggle speaker sharing)
//! - Tab 2: Per-App Mixer & Persistent Routing (`Enter` to toggle remote/local routing, `p` to save persistent rule in `config.toml`)
//! - Tab 3: Microphone Pass-Through & Android Audio Bridge (`s` send mic, `r` receive mic, `o` stop mic, `a` start/stop Android stream, `w` switch USB Android device to Wi-Fi)
//! - Bottom Telemetry Bar: Live Stereo Peak/RMS VU gauge + dBFS + Network RTT/Jitter (`ms`)

use std::collections::HashSet;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, List, ListItem, Paragraph, Tabs};
use ratatui::{Frame, Terminal};
use soundhop_core::controller::{ConnectOptions, Controller};
use soundhop_core::discovery::{discover_once, DiscoveryManager, Peer};
use soundhop_core::util::{is_own_non_loopback_ip, is_private_address};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuiTab {
    PeersAndMultiRoom,
    AppMixerAndRouting,
    MicAndAndroid,
}

impl TuiTab {
    pub fn next(self) -> Self {
        match self {
            Self::PeersAndMultiRoom => Self::AppMixerAndRouting,
            Self::AppMixerAndRouting => Self::MicAndAndroid,
            Self::MicAndAndroid => Self::PeersAndMultiRoom,
        }
    }

    pub fn index(self) -> usize {
        match self {
            Self::PeersAndMultiRoom => 0,
            Self::AppMixerAndRouting => 1,
            Self::MicAndAndroid => 2,
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
    pub status_message: String,
    pub should_quit: bool,
}

impl TuiState {
    pub fn new(controller: Arc<Controller>) -> Self {
        let st = controller.state.lock().unwrap().clone();
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
        Self {
            controller,
            tab: TuiTab::PeersAndMultiRoom,
            peers,
            selected_peer_idx: 0,
            multiroom_selected,
            keep_local: st.keep_local,
            apps,
            selected_app_idx: 0,
            status_message: "Press Tab to switch views, Space to select multi-room peers, Enter to apply, q to quit.".to_string(),
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
        if self.selected_peer_idx >= self.peers.len() && !self.peers.is_empty() {
            self.selected_peer_idx = self.peers.len() - 1;
        }
        if self.selected_app_idx >= self.apps.len() && !self.apps.is_empty() {
            self.selected_app_idx = self.apps.len() - 1;
        }
    }

    pub fn handle_key(&mut self, code: KeyCode) {
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
                let cur = self.controller.config.lock().unwrap().sharing.enabled;
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
                let cfg = self.controller.config.lock().unwrap().clone();
                self.peers = discover_once(0.8, cfg.discovery.port, &cfg.device.id);
                for p in &self.peers {
                    self.controller.remember_peer(p);
                }
                self.status_message = format!("Discovered {} LAN speaker(s).", self.peers.len());
            }
            _ => {}
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
    }
    draw_telemetry_bar(frame, chunks[2], state);
    draw_footer_bar(frame, chunks[3], state);
}

fn draw_top_bar(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let titles = vec![
        Line::from(" [1] Peers & Multi-Room "),
        Line::from(" [2] App Mixer & Rules "),
        Line::from(" [3] Mic & Android Bridge "),
    ];
    let tabs = Tabs::new(titles)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" SoundHop v2.0 Interactive Dashboard "),
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

    let st = state.controller.state.lock().unwrap().clone();
    let cfg = state.controller.config.lock().unwrap().clone();

    let items: Vec<ListItem<'_>> = if state.peers.is_empty() {
        vec![ListItem::new(
            "  No speakers discovered yet. Press 'r' to scan LAN.",
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
                let rtt_tag = p.rtt_ms.map(|r| format!("  {r:.1} ms")).unwrap_or_default();
                let text = format!(
                    "{selected_cursor} {checked} {} ({}:{}){}{}",
                    p.name, p.host, p.port, rtt_tag, conn_tag
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

    let list =
        List::new(items).block(Block::default().borders(Borders::ALL).title(
            " Discovered Speakers (Space: Multi-Room Select | Enter: Connect | r: Rescan) ",
        ));
    frame.render_widget(list, cols[0]);

    let summary_lines = vec![
        Line::from(vec![
            Span::styled("Device Name: ", Style::default().fg(Color::Gray)),
            Span::styled(
                &cfg.device.name,
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("Sharing Speakers: ", Style::default().fg(Color::Gray)),
            Span::styled(
                if cfg.sharing.enabled {
                    format!("ON (TCP :{})", cfg.sharing.port)
                } else {
                    "OFF".to_string()
                },
                Style::default().fg(if cfg.sharing.enabled {
                    Color::Green
                } else {
                    Color::Red
                }),
            ),
        ]),
        Line::from(vec![
            Span::styled("Security Mode: ", Style::default().fg(Color::Gray)),
            Span::raw(&cfg.sharing.security),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("Active Output: ", Style::default().fg(Color::Gray)),
            Span::styled(
                if st.is_connected() {
                    &st.connected_name
                } else {
                    "Local Speakers"
                },
                Style::default().fg(Color::Cyan),
            ),
        ]),
        Line::from(vec![
            Span::styled("Multi-Room Mode: ", Style::default().fg(Color::Gray)),
            Span::raw(if st.is_multiroom() {
                "Active"
            } else {
                "Single"
            }),
        ]),
        Line::from(vec![
            Span::styled("Keep Local Output (l): ", Style::default().fg(Color::Gray)),
            Span::raw(if state.keep_local { "Yes" } else { "No" }),
        ]),
    ];

    let right_panel = Paragraph::new(summary_lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Session Status (s: Share | d: Disconnect) "),
    );
    frame.render_widget(right_panel, cols[1]);
}

fn draw_apps_tab(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let items: Vec<ListItem<'_>> = if state.apps.is_empty() {
        vec![ListItem::new("  No active audio application streams.")]
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
                    .unwrap_or("Unknown");
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
                    "[LOCAL]"
                };
                let saved_tag = if persistent { " (Saved Rule)" } else { "" };
                let line_str = format!("{cursor} {badge:<9} {name} — {media}{saved_tag}");
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
            " Per-App Audio Routing (Enter: Toggle Remote/Local | p: Save Persistent Rule) ",
        ));
    frame.render_widget(list, area);
}

fn draw_mic_android_tab(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let st = state.controller.state.lock().unwrap().clone();
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
            "Network Microphone Pass-Through",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(format!(
            "  Active: {} | Direction: {} | Source: {}",
            !st.connected_source.is_empty(),
            if st.mic_direction.is_empty() {
                "off"
            } else {
                &st.mic_direction
            },
            if st.connected_source.is_empty() {
                "none"
            } else {
                &st.connected_source
            }
        )),
        Line::from("  Keys: [s] Send My Mic to Peer | [r] Receive Peer Mic | [o] Stop Mic"),
        Line::from(""),
        Line::from(Span::styled(
            "Android Audio & Virtual Microphone Bridge (scrcpy)",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(format!(
            "  Streaming Active: {} | Device: {}",
            if and_active { "Yes" } else { "No" },
            and_dev
        )),
        Line::from("  Keys: [a] Start / Stop Android Audio Capture"),
    ];

    let block = Paragraph::new(text).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Microphone Pass-Through & Android Audio "),
    );
    frame.render_widget(block, area);
}

fn draw_telemetry_bar(frame: &mut Frame<'_>, area: Rect, state: &TuiState) {
    let snap = state.controller.telemetry_snapshot();
    let ratio = ((snap.audio.dbfs + 60.0) / 60.0).clamp(0.0, 1.0) as f64;
    let first_net = snap.peers.values().next();
    let peak_max = snap.audio.peak_left.max(snap.audio.peak_right);
    let net_label = if let Some(n) = first_net {
        format!(
            "RTT: {:.1} ms | Jitter: {:.1} ms | Level: {:.1} dBFS | L: {:.0}% R: {:.0}%",
            n.last_rtt_ms,
            n.jitter_ms,
            snap.audio.dbfs,
            snap.audio.peak_left * 100.0,
            snap.audio.peak_right * 100.0
        )
    } else {
        format!(
            "RTT: -- ms | Jitter: -- ms | Level: {:.1} dBFS | L: {:.0}% R: {:.0}%",
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
                .title(" Live Audio VU Meter & Network Latency / Jitter Telemetry "),
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

pub fn run_tui(controller: Arc<Controller>) -> Result<i32, String> {
    let cfg = controller.config.lock().unwrap().clone();
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
    disc.store.set_on_peer(move |peer| {
        ctrl_cb.remember_peer(peer);
    });
    let _ = disc.start(cfg.sharing.enabled);
    let telem_monitor = controller.start_telemetry_monitor();

    enable_raw_mode().map_err(|e| format!("Failed to enable terminal raw mode: {e}"))?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)
        .map_err(|e| format!("Failed to enter alternate screen: {e}"))?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal =
        Terminal::new(backend).map_err(|e| format!("Failed to initialize TUI terminal: {e}"))?;

    let mut tui_state = TuiState::new(Arc::clone(&controller));
    tui_state.merge_live_peers(disc.peers());
    tui_state.refresh_data();
    let mut last_tick = Instant::now();

    let loop_res: Result<(), String> = (|| {
        while !tui_state.should_quit {
            terminal
                .draw(|f| draw_tui(f, &tui_state))
                .map_err(|e| e.to_string())?;

            if event::poll(Duration::from_millis(50)).map_err(|e| e.to_string())? {
                if let Event::Key(key) = event::read().map_err(|e| e.to_string())? {
                    if key.kind == KeyEventKind::Press {
                        tui_state.handle_key(key.code);
                        let share_now = controller.config.lock().unwrap().sharing.enabled;
                        disc.set_advertising(share_now);
                        tui_state.merge_live_peers(disc.peers());
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

    telem_monitor.stop();
    disc.stop();
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();

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
}
