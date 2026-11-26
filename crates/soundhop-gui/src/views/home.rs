//! Home ("Play & Share") view: Hero output & volume card, nearby speaker cards
//! (single or Multi-Room), speaker sharing toggle, and microphone quick switcher.

use std::collections::HashSet;

use eframe::egui::{self, RichText};

use crate::backend::{GuiBackend, GuiCommand, GuiSnapshot, PeerView};
use crate::theme::Palette;
#[cfg(not(target_os = "windows"))]
use crate::widgets::segmented_control;
use crate::widgets::{
    audio_wave_indicator, callout_card, card, compact_button, danger_button, pill_badge,
    primary_button, secondary_button, section_header, toggle_switch,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerTrustState {
    Trusted,
    PairingRequired,
    OpenLan,
}

pub(crate) fn peer_trust_state(peer: &PeerView) -> PeerTrustState {
    if peer.paired {
        PeerTrustState::Trusted
    } else if matches!(peer.security.as_str(), "psk" | "paired" | "pin" | "cookie") {
        PeerTrustState::PairingRequired
    } else {
        PeerTrustState::OpenLan
    }
}

#[derive(Default)]
pub struct HomeViewState {
    pub multiroom_mode: bool,
    pub selected_peers: HashSet<String>,
    pub keep_local: bool,
    pub show_manual_connect: bool,
    pub manual_address: String,
    pub local_volume_drag: Option<u32>,
    qr_texture: Option<egui::TextureHandle>,
    qr_cache_key: Option<(String, Option<String>)>,
    initialized_from_snap: bool,
}

impl HomeViewState {
    fn sync_initial(&mut self, snap: &GuiSnapshot) {
        if !self.initialized_from_snap && snap.hydrated {
            self.multiroom_mode = snap.multiroom;
            self.keep_local = snap.keep_local;
            for p in &snap.multiroom_peers {
                self.selected_peers.insert(p.peer_id.clone());
            }
            self.initialized_from_snap = true;
        }
    }

    fn reconcile_with_snapshot(&mut self, snap: &GuiSnapshot) {
        self.sync_initial(snap);
        if snap.peers.len() <= 1 {
            self.multiroom_mode = false;
        }
        self.selected_peers
            .retain(|id| snap.peers.iter().any(|p| p.id == *id));
    }
}

pub(crate) fn should_show_firewall_banner(snap: &GuiSnapshot) -> bool {
    (snap.sharing_enabled || snap.web_broadcast_active)
        && snap.firewall_active
        && snap.firewall_kind != "none"
        && snap.firewall_ports_allowed != Some(true)
}

pub(crate) fn summarize_user_agent(ua: &str) -> String {
    let trimmed = ua.trim();
    if trimmed.is_empty() {
        return "Browser".to_string();
    }
    let lower = trimmed.to_ascii_lowercase();
    let browser = if lower.contains("edg") {
        "Edge"
    } else if lower.contains("firefox") || lower.contains("fxios") {
        "Firefox"
    } else if lower.contains("crios") || lower.contains("chrome") {
        "Chrome"
    } else if lower.contains("safari") {
        "Safari"
    } else {
        "Browser"
    };
    let os = if lower.contains("android") {
        "Android"
    } else if lower.contains("iphone") || lower.contains("ipad") {
        "iOS"
    } else if lower.contains("windows") {
        "Windows"
    } else if lower.contains("mac os") || lower.contains("macintosh") {
        "macOS"
    } else if lower.contains("linux") {
        "Linux"
    } else {
        ""
    };
    if os.is_empty() {
        browser.to_string()
    } else {
        format!("{browser} · {os}")
    }
}

pub(crate) fn primary_web_broadcast_url(snap: &GuiSnapshot) -> Option<String> {
    snap.web_urls
        .first()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            let localhost = snap.web_localhost_url.trim();
            (!localhost.is_empty()).then(|| localhost.to_string())
        })
}

pub fn render(
    ui: &mut egui::Ui,
    pal: &Palette,
    snap: &GuiSnapshot,
    state: &mut HomeViewState,
    backend: &GuiBackend,
) {
    state.reconcile_with_snapshot(snap);

    // 1. Hero Output & Master Volume Card
    let hero_border = if snap.multiroom {
        Some(pal.multiroom)
    } else if snap.connected {
        Some(pal.success)
    } else {
        None
    };

    card(ui, pal, hero_border, |ui| {
        ui.horizontal(|ui| {
            let wave_color = if snap.multiroom {
                pal.multiroom
            } else if snap.connected {
                pal.success
            } else {
                pal.accent
            };
            let level_norm = ((snap.audio_dbfs + 60.0) / 60.0).clamp(0.0, 1.0);
            audio_wave_indicator(
                ui,
                pal,
                snap.audio_active || snap.connected,
                level_norm.max(snap.audio_peak),
                wave_color,
            );
            ui.add_space(6.0);
            ui.vertical(|ui| {
                ui.label(
                    RichText::new(snap.output_headline())
                        .color(pal.text)
                        .size(18.0)
                        .strong(),
                );
                ui.label(
                    RichText::new(snap.output_subtitle())
                        .color(pal.text_muted)
                        .size(12.5),
                );
            });

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if snap.connected {
                    if danger_button(ui, pal, "Disconnect").clicked() {
                        backend.send(GuiCommand::Disconnect);
                    }
                } else if snap.sharing_enabled {
                    pill_badge(ui, "Sharing On", pal.accent, pal.accent_soft);
                } else {
                    pill_badge(ui, "Local Only", pal.text_muted, pal.card_elevated);
                }
            });
        });

        ui.add_space(12.0);
        ui.separator();
        ui.add_space(8.0);

        // Volume & Mute Bar directly accessible
        ui.horizontal(|ui| {
            let mute_label = if snap.volume_muted { "Unmute" } else { "Mute" };
            if secondary_button(ui, pal, mute_label).clicked() {
                backend.send(GuiCommand::SetMute {
                    muted: !snap.volume_muted,
                });
            }
            ui.add_space(6.0);
            let mut vol = state.local_volume_drag.unwrap_or(snap.volume_level);
            let slider = ui.add(
                egui::Slider::new(&mut vol, 0..=150)
                    .suffix("%")
                    .show_value(true),
            );
            if slider.changed() {
                state.local_volume_drag = Some(vol);
            }
            if slider.drag_stopped() || (slider.changed() && !slider.dragged()) {
                backend.send(GuiCommand::SetVolume { level: vol });
                state.local_volume_drag = None;
            } else if !slider.dragged() {
                state.local_volume_drag = None;
            }
        });
    });

    ui.add_space(14.0);

    // 2. Nearby Speakers Section
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            section_header(
                ui,
                pal,
                "Play my audio on",
                Some("Pick a nearby computer or speaker on your Wi-Fi."),
            );
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_enabled_ui(!snap.refreshing_peers, |ui| {
                let refresh_label = if snap.refreshing_peers {
                    "Refreshing..."
                } else {
                    "Refresh"
                };
                if secondary_button(ui, pal, refresh_label).clicked() {
                    backend.send(GuiCommand::RefreshPeers);
                }
            });
            if snap.peers.len() > 1 {
                ui.add_space(8.0);
                let mut mr = state.multiroom_mode;
                if toggle_switch(ui, pal, &mut mr).changed() {
                    state.multiroom_mode = mr;
                }
                ui.label(
                    RichText::new("Multiple speakers")
                        .color(pal.text_secondary)
                        .size(12.5),
                );
            }
        });
    });

    ui.add_space(8.0);

    if snap.peers.is_empty() {
        card(ui, pal, None, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(8.0);
                ui.label(
                    RichText::new("Looking for nearby speakers...")
                        .color(pal.text)
                        .size(15.0)
                        .strong(),
                );
                ui.add_space(4.0);
                ui.label(
                    RichText::new(
                        "Open SoundHop on another computer on the same Wi-Fi and turn on \"Share my speakers\".",
                    )
                    .color(pal.text_muted)
                    .size(12.5),
                );
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(!snap.refreshing_peers, |ui| {
                        let scan_label = if snap.refreshing_peers {
                            "Scanning..."
                        } else {
                            "Scan Again"
                        };
                        if secondary_button(ui, pal, scan_label).clicked() {
                            backend.send(GuiCommand::RefreshPeers);
                        }
                    });
                    let addr_label = if state.show_manual_connect {
                        "Hide Manual Connect"
                    } else {
                        "Connect by IP Address"
                    };
                    if secondary_button(ui, pal, addr_label).clicked() {
                        state.show_manual_connect = !state.show_manual_connect;
                    }
                });
            });

            let manual_t = ui.ctx().animate_bool_with_time_and_easing(
                egui::Id::new("home_manual_connect_reveal"),
                state.show_manual_connect,
                0.14,
                egui::emath::easing::cubic_out,
            );
            if state.show_manual_connect || manual_t > 0.02 {
                ui.scope(|ui| {
                    ui.set_opacity(manual_t);
                    ui.add_space(10.0 * manual_t);
                    ui.separator();
                    ui.add_space(8.0 * manual_t);
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("Device IP or Hostname:")
                                .color(pal.text_secondary)
                                .size(12.5),
                        );
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut state.manual_address)
                                .hint_text("e.g. 192.168.1.50")
                                .desired_width(210.0),
                        );
                        let enter =
                            resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        if (primary_button(ui, pal, "Connect").clicked() || enter)
                            && !state.manual_address.trim().is_empty()
                        {
                            backend.send(GuiCommand::ConnectAddress {
                                host_port: state.manual_address.clone(),
                            });
                        }
                    });
                });
            }
        });
    } else {
        for peer in &snap.peers {
            let is_conn = snap.is_peer_connected(&peer.id);
            let border = if is_conn { Some(pal.success) } else { None };
            card(ui, pal, border, |ui| {
                ui.horizontal(|ui| {
                    if state.multiroom_mode {
                        let mut checked = state.selected_peers.contains(&peer.id);
                        if ui.checkbox(&mut checked, "").changed() {
                            if checked {
                                state.selected_peers.insert(peer.id.clone());
                            } else {
                                state.selected_peers.remove(&peer.id);
                            }
                        }
                    }

                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(&peer.name)
                                    .color(pal.text)
                                    .size(15.0)
                                    .strong(),
                            );
                            pill_badge(
                                ui,
                                peer.platform_label(),
                                pal.text_secondary,
                                pal.card_elevated,
                            );
                            let trust_state = peer_trust_state(peer);
                            match trust_state {
                                PeerTrustState::Trusted => {
                                    pill_badge(ui, "Trusted", pal.success, pal.success_soft);
                                }
                                PeerTrustState::PairingRequired => {
                                    pill_badge(
                                        ui,
                                        "Pairing Required",
                                        pal.warning,
                                        pal.warning_soft,
                                    );
                                }
                                PeerTrustState::OpenLan => {
                                    pill_badge(
                                        ui,
                                        "Open LAN",
                                        pal.text_secondary,
                                        pal.card_elevated,
                                    );
                                }
                            }
                            if is_conn {
                                pill_badge(ui, "Connected", pal.success, pal.success_soft);
                            }
                        });
                        let sig_text = peer.signal_label();
                        let label_resp =
                            ui.label(RichText::new(sig_text).color(pal.text_muted).size(12.0));
                        label_resp.on_hover_text(format!(
                            "Address: {}:{} • Security: {}{}",
                            peer.host,
                            peer.port,
                            peer.security,
                            peer.rtt_ms
                                .map(|r| format!(" • Latency: {r:.1} ms"))
                                .unwrap_or_default()
                        ));
                    });

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if !state.multiroom_mode {
                            let is_connecting_this =
                                snap.pending_peer_id.as_deref() == Some(peer.id.as_str());
                            let trust_state = peer_trust_state(peer);
                            if is_conn {
                                if danger_button(ui, pal, "Disconnect").clicked() {
                                    backend.send(GuiCommand::Disconnect);
                                }
                            } else {
                                match trust_state {
                                    PeerTrustState::Trusted => {
                                        ui.add_enabled_ui(!is_connecting_this, |ui| {
                                            let btn_label = if is_connecting_this {
                                                "Connecting..."
                                            } else {
                                                "Play Here"
                                            };
                                            if primary_button(ui, pal, btn_label).clicked() {
                                                backend.send(GuiCommand::ConnectPeer {
                                                    peer_id: peer.id.clone(),
                                                });
                                            }
                                        });
                                    }
                                    PeerTrustState::PairingRequired => {
                                        if primary_button(ui, pal, "Pair").clicked() {
                                            backend.send(GuiCommand::SendPairRequest {
                                                host: peer.host.clone(),
                                                port: None,
                                                code: None,
                                            });
                                        }
                                    }
                                    PeerTrustState::OpenLan => {
                                        ui.add_enabled_ui(!is_connecting_this, |ui| {
                                            let btn_label = if is_connecting_this {
                                                "Connecting..."
                                            } else {
                                                "Play Here"
                                            };
                                            if primary_button(ui, pal, btn_label).clicked() {
                                                backend.send(GuiCommand::ConnectPeer {
                                                    peer_id: peer.id.clone(),
                                                });
                                            }
                                        });
                                        ui.add_space(6.0);
                                        if secondary_button(ui, pal, "Pair").clicked() {
                                            backend.send(GuiCommand::SendPairRequest {
                                                host: peer.host.clone(),
                                                port: None,
                                                code: None,
                                            });
                                        }
                                    }
                                }
                            }
                        }
                    });
                });
            });
            ui.add_space(6.0);
        }

        if state.multiroom_mode {
            card(ui, pal, Some(pal.multiroom), |ui| {
                ui.horizontal(|ui| {
                    toggle_switch(ui, pal, &mut state.keep_local);
                    ui.label(
                        RichText::new("Keep playing on this device too")
                            .color(pal.text)
                            .size(13.0),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let count = state.selected_peers.len();
                        let label = if count == 0 {
                            "Select Speakers".to_string()
                        } else {
                            format!(
                                "Play on {count} Speaker{}",
                                if count == 1 { "" } else { "s" }
                            )
                        };
                        ui.add_enabled_ui(count > 0, |ui| {
                            if primary_button(ui, pal, &label).clicked() {
                                let ids: Vec<String> =
                                    state.selected_peers.iter().cloned().collect();
                                backend.send(GuiCommand::ConnectMulti {
                                    peer_ids: ids,
                                    keep_local: state.keep_local,
                                });
                            }
                        });
                    });
                });
            });
            ui.add_space(6.0);
        }

        // Keep manual IP connect reachable even when nearby peers exist
        ui.horizontal(|ui| {
            let link_text = if state.show_manual_connect {
                "Hide Manual Connect"
            } else {
                "+ Connect by IP Address"
            };
            if ui
                .link(RichText::new(link_text).color(pal.accent))
                .clicked()
            {
                state.show_manual_connect = !state.show_manual_connect;
            }
        });
        let manual_list_t = ui.ctx().animate_bool_with_time_and_easing(
            egui::Id::new("home_manual_connect_list_reveal"),
            state.show_manual_connect,
            0.14,
            egui::emath::easing::cubic_out,
        );
        if state.show_manual_connect || manual_list_t > 0.02 {
            ui.scope(|ui| {
                ui.set_opacity(manual_list_t);
                ui.add_space(4.0 * manual_list_t);
                card(ui, pal, None, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("Device IP or Hostname:")
                                .color(pal.text_secondary)
                                .size(12.5),
                        );
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut state.manual_address)
                                .hint_text("e.g. 192.168.1.50")
                                .desired_width(210.0),
                        );
                        let enter =
                            resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        if (primary_button(ui, pal, "Connect").clicked() || enter)
                            && !state.manual_address.trim().is_empty()
                        {
                            backend.send(GuiCommand::ConnectAddress {
                                host_port: state.manual_address.clone(),
                            });
                        }
                    });
                });
            });
        }
    }

    ui.add_space(14.0);

    // 3. Share My Speakers Card
    section_header(
        ui,
        pal,
        "Let others play on my speakers",
        Some("Make this computer appear as a wireless speaker for your other devices."),
    );
    ui.add_space(6.0);

    card(ui, pal, None, |ui| {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    RichText::new("Share this device's speakers")
                        .color(pal.text)
                        .size(14.5)
                        .strong(),
                );
                let sub = if snap.sharing_enabled {
                    format!("Visible on your Wi-Fi as \"{}\"", snap.config.device.name)
                } else {
                    "Currently hidden from other devices".to_string()
                };
                ui.label(RichText::new(sub).color(pal.text_muted).size(12.0));
            });

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let mut share = snap.sharing_enabled;
                if toggle_switch(ui, pal, &mut share).changed() {
                    backend.send(GuiCommand::SetShare { enabled: share });
                }
            });
        });

        if should_show_firewall_banner(snap) {
            ui.add_space(10.0);
            let fw_display = if snap.firewall_kind == "windows-defender" {
                "Windows Defender Firewall"
            } else {
                snap.firewall_kind.as_str()
            };
            callout_card(ui, pal.warning_soft, pal.warning, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new(format!(
                                "{fw_display} may block incoming audio"
                            ))
                            .color(pal.warning)
                            .size(12.5)
                            .strong(),
                        );
                        ui.label(
                            RichText::new(
                                "If other devices cannot connect, allow SoundHop through the firewall.",
                            )
                            .color(pal.text_secondary)
                            .size(11.5),
                        );
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if secondary_button(ui, pal, "Allow in Firewall").clicked() {
                            backend.send(GuiCommand::OpenFirewall);
                        }
                    });
                });
            });
        }
    });

    ui.add_space(14.0);

    // 4. Web Broadcast (Listen on Phone) Card
    let web_border = if snap.web_broadcast_active {
        Some(pal.accent)
    } else {
        None
    };
    card(ui, pal, web_border, |ui| {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("Web Broadcast (Listen on Phone)")
                            .color(pal.text)
                            .size(14.5)
                            .strong(),
                    );
                    if snap.web_broadcast_active {
                        pill_badge(
                            ui,
                            &format!("Live ({})", snap.web_clients.len()),
                            pal.success,
                            pal.success_soft,
                        );
                    }
                });
                let sub = if snap.web_broadcast_active {
                    format!(
                        "Listening on port {} (WebRTC UDP {}) • Scan QR or open link in your mobile browser",
                        snap.web_http_port, snap.web_webrtc_port
                    )
                } else {
                    "Stream PC audio to any phone or tablet browser over Wi-Fi or USB—no app install required."
                        .to_string()
                };
                ui.label(RichText::new(sub).color(pal.text_muted).size(12.0));
            });

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let mut web_on = snap.web_broadcast_active;
                if toggle_switch(ui, pal, &mut web_on).changed() {
                    if web_on {
                        backend.send(GuiCommand::StartWebBroadcast {
                            keep_local: snap.web_keep_local,
                        });
                    } else {
                        backend.send(GuiCommand::StopWebBroadcast);
                    }
                }
            });
        });

        ui.add_space(8.0);
        let mut keep_local = snap.web_keep_local;
        if ui.checkbox(&mut keep_local, "Keep playing on PC").changed() {
            backend.send(GuiCommand::SaveConfigPatch {
                patch: serde_json::json!({
                    "sharing": { "web_keep_local": keep_local }
                }),
                toast: None,
            });
        }

        if snap.web_broadcast_active {
            if let Some(primary_url) = primary_web_broadcast_url(snap) {
                let cache_key = (primary_url.clone(), snap.web_token.clone());
                if state.qr_cache_key.as_ref() != Some(&cache_key) {
                    state.qr_texture =
                        match soundhop_core::web::qr::encode_qr_rgba(&primary_url, 6, 2) {
                            Ok((px_w, rgba)) => {
                                let img =
                                    egui::ColorImage::from_rgba_unmultiplied([px_w, px_w], &rgba);
                                Some(ui.ctx().load_texture(
                                    "web_broadcast_qr",
                                    img,
                                    egui::TextureOptions::NEAREST,
                                ))
                            }
                            Err(_) => None,
                        };
                    state.qr_cache_key = Some(cache_key);
                }

                ui.add_space(10.0);
                ui.separator();
                ui.add_space(10.0);

                ui.horizontal(|ui| {
                    if let Some(ref tex) = state.qr_texture {
                        ui.add(
                            egui::Image::new(tex)
                                .fit_to_exact_size(egui::vec2(148.0, 148.0))
                                .corner_radius(egui::CornerRadius::same(6)),
                        )
                        .on_hover_text(
                            "Scan with your phone camera to open the SoundHop Web Player",
                        );
                        ui.add_space(12.0);
                    }

                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("Listen URL")
                                .color(pal.text_secondary)
                                .size(12.5)
                                .strong(),
                        );
                        ui.add_space(2.0);
                        let url_resp = ui.add(
                            egui::Label::new(
                                RichText::new(&primary_url)
                                    .color(pal.accent)
                                    .size(12.0)
                                    .monospace(),
                            )
                            .sense(egui::Sense::click()),
                        );
                        if url_resp.on_hover_text("Click to copy Listen URL").clicked() {
                            ui.ctx().copy_text(primary_url.clone());
                        }

                        if !snap.web_adb_reversed.is_empty() && !snap.web_localhost_url.is_empty() {
                            ui.add_space(4.0);
                            ui.horizontal(|ui| {
                                pill_badge(ui, "USB Tunnel", pal.success, pal.success_soft);
                                if ui
                                    .add(
                                        egui::Label::new(
                                            RichText::new(&snap.web_localhost_url)
                                                .color(pal.text_secondary)
                                                .size(11.5)
                                                .monospace(),
                                        )
                                        .sense(egui::Sense::click()),
                                    )
                                    .on_hover_text("Click to copy USB localhost URL")
                                    .clicked()
                                {
                                    ui.ctx().copy_text(snap.web_localhost_url.clone());
                                }
                            });
                        }

                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if secondary_button(ui, pal, "Copy URL").clicked() {
                                ui.ctx().copy_text(primary_url.clone());
                            }
                            if snap.web_token_required
                                && secondary_button(ui, pal, "Rotate Token").clicked()
                            {
                                backend.send(GuiCommand::RotateWebToken);
                            }
                        });
                    });
                });
            } else {
                state.qr_texture = None;
                state.qr_cache_key = None;
            }

            ui.add_space(10.0);
            ui.separator();
            ui.add_space(8.0);

            ui.label(
                RichText::new(format!("Listeners ({})", snap.web_clients.len()))
                    .color(pal.text_secondary)
                    .size(12.5)
                    .strong(),
            );
            ui.add_space(4.0);

            if snap.web_clients.is_empty() {
                ui.label(
                    RichText::new(
                        "No browsers connected yet. Scan the QR code or open the URL on your phone.",
                    )
                    .color(pal.text_muted)
                    .size(12.0),
                );
            } else {
                for client in &snap.web_clients {
                    ui.horizontal(|ui| {
                        if client.transport.eq_ignore_ascii_case("webrtc") {
                            pill_badge(ui, "WebRTC", pal.success, pal.success_soft);
                        } else {
                            pill_badge(ui, "WebSocket", pal.warning, pal.warning_soft);
                        }
                        ui.label(
                            RichText::new(&client.remote_addr)
                                .color(pal.text)
                                .size(12.5)
                                .strong(),
                        );
                        let ua_summary = summarize_user_agent(&client.user_agent);
                        let ua_lbl =
                            ui.label(RichText::new(ua_summary).color(pal.text_muted).size(12.0));
                        if !client.user_agent.is_empty() {
                            ua_lbl.on_hover_text(&client.user_agent);
                        }
                        let rtt_text = client
                            .rtt_ms
                            .map(|r| format!("{r:.0} ms RTT"))
                            .unwrap_or_else(|| "— RTT".to_string());
                        ui.label(
                            RichText::new(format!(
                                "• {rtt_text} • {} dropped",
                                client.dropped_frames
                            ))
                            .color(pal.text_muted)
                            .size(11.5),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if compact_button(ui, pal, "Kick").clicked() {
                                backend.send(GuiCommand::KickWebClient {
                                    client_id: client.id.clone(),
                                });
                            }
                        });
                    });
                    ui.add_space(4.0);
                }
            }
        } else {
            state.qr_texture = None;
            state.qr_cache_key = None;
        }
    });

    // 5. Quick Microphone Card (Linux PulseAudio/PipeWire pass-through only)
    #[cfg(not(target_os = "windows"))]
    if snap.connected || snap.mic_enabled {
        ui.add_space(14.0);
        card(ui, pal, None, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new("Microphone Sharing")
                            .color(pal.text)
                            .size(14.5)
                            .strong(),
                    );
                    ui.label(
                        RichText::new("Use your mic on the remote PC, or hear their mic here.")
                            .color(pal.text_muted)
                            .size(12.0),
                    );
                });

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    #[derive(Clone, Copy, PartialEq)]
                    enum MicChoice {
                        Off,
                        Send,
                        Receive,
                    }
                    let mut choice = if !snap.mic_enabled {
                        MicChoice::Off
                    } else if snap.mic_direction == "receive" {
                        MicChoice::Receive
                    } else {
                        MicChoice::Send
                    };
                    if segmented_control(
                        ui,
                        pal,
                        &mut choice,
                        &[
                            ("Off", MicChoice::Off),
                            ("Send My Mic", MicChoice::Send),
                            ("Hear Theirs", MicChoice::Receive),
                        ],
                    ) {
                        let dir = match choice {
                            MicChoice::Off => None,
                            MicChoice::Send => Some("send".to_string()),
                            MicChoice::Receive => Some("receive".to_string()),
                        };
                        backend.send(GuiCommand::SetMic { direction: dir });
                    }
                });
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::PeerView;
    use soundhop_core::backend::linux::LinuxAudioBackend;
    use soundhop_core::config::Config;
    use soundhop_core::controller::Controller;
    use soundhop_core::state::State;
    use soundhop_core::util::{CommandResult, MockRunner};
    use std::sync::Arc;

    fn test_snapshot() -> GuiSnapshot {
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
        GuiSnapshot::from_controller(&ctrl)
    }

    fn peer(id: &str) -> PeerView {
        PeerView {
            id: id.to_string(),
            name: id.to_string(),
            host: "192.168.1.10".to_string(),
            port: 4713,
            platform: "linux".to_string(),
            security: "trusted-lan".to_string(),
            rtt_ms: None,
            online: true,
            paired: false,
        }
    }

    #[test]
    fn test_home_reconcile_resets_multiroom_and_prunes_selected_peers() {
        let mut snap = test_snapshot();
        snap.hydrated = true;
        snap.peers = vec![peer("p1"), peer("p2")];

        let mut state = HomeViewState::default();
        state.reconcile_with_snapshot(&snap);
        state.multiroom_mode = true;
        state.selected_peers.insert("p1".to_string());
        state.selected_peers.insert("p2".to_string());
        state.selected_peers.insert("stale".to_string());

        state.reconcile_with_snapshot(&snap);
        assert!(state.multiroom_mode);
        assert_eq!(state.selected_peers.len(), 2);
        assert!(!state.selected_peers.contains("stale"));

        // Drop down to 1 peer: multiroom_mode must reset and vanished peer pruned
        snap.peers = vec![peer("p1")];
        state.reconcile_with_snapshot(&snap);
        assert!(!state.multiroom_mode);
        assert_eq!(state.selected_peers.len(), 1);
        assert!(state.selected_peers.contains("p1"));
    }

    #[test]
    fn test_firewall_banner_hides_when_ports_allowed() {
        let mut snap = test_snapshot();
        snap.sharing_enabled = true;
        snap.firewall_active = true;
        snap.firewall_kind = "ufw".to_string();
        snap.firewall_ports_allowed = None;
        assert!(should_show_firewall_banner(&snap));

        snap.firewall_ports_allowed = Some(false);
        assert!(should_show_firewall_banner(&snap));

        snap.firewall_ports_allowed = Some(true);
        assert!(!should_show_firewall_banner(&snap));

        snap.firewall_ports_allowed = Some(false);
        snap.sharing_enabled = false;
        assert!(!should_show_firewall_banner(&snap));
    }

    #[test]
    fn test_peer_trust_state_classification() {
        let mut p = peer("p1");
        p.security = "trusted-lan".to_string();
        p.paired = false;
        assert_eq!(peer_trust_state(&p), PeerTrustState::OpenLan);

        p.security = "psk".to_string();
        p.paired = false;
        assert_eq!(peer_trust_state(&p), PeerTrustState::PairingRequired);

        p.paired = true;
        assert_eq!(peer_trust_state(&p), PeerTrustState::Trusted);
    }

    #[test]
    fn test_web_broadcast_home_helpers() {
        let mut snap = test_snapshot();
        assert!(primary_web_broadcast_url(&snap).is_none());

        snap.web_localhost_url = "http://localhost:4717/#t=tok".to_string();
        assert_eq!(
            primary_web_broadcast_url(&snap).as_deref(),
            Some("http://localhost:4717/#t=tok")
        );

        snap.web_urls = vec!["http://192.168.1.42:4717/#t=tok".to_string()];
        assert_eq!(
            primary_web_broadcast_url(&snap).as_deref(),
            Some("http://192.168.1.42:4717/#t=tok")
        );

        assert_eq!(
            summarize_user_agent("Mozilla/5.0 (Linux; Android 14) Chrome/124.0"),
            "Chrome · Android"
        );
        assert_eq!(
            summarize_user_agent("Mozilla/5.0 (iPhone; CPU iPhone OS 17_0) Safari/604.1"),
            "Safari · iOS"
        );
    }
}
