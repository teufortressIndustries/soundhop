//! Preferences view: 4 clean sub-tabs (General, Audio & Sync, Privacy & Devices,
//! Network & Health) with GStreamer-tuned jitter/profile controls, full port mapping,
//! live-apply updates, and diagnostics.

use eframe::egui::{self, RichText};
use soundhop_core::config::{is_paired_security_mode, DEFAULT_RTP_PORT};
use soundhop_core::constants::{DEFAULT_AUDIO_PORT, DEFAULT_DISCOVERY_PORT};
use soundhop_core::pairing::DEFAULT_PAIR_PORT;
use soundhop_core::transport::DEFAULT_V3_PORT;

use crate::backend::{security_friendly_label, GuiBackend, GuiCommand, GuiSnapshot};
use crate::theme::{Palette, ThemeMode};
use crate::widgets::{
    callout_card, card, danger_button, pill_badge, primary_button, secondary_button,
    section_header, segmented_control, selectable_card, toggle_switch,
};

const AUDIO_PROFILE_CARDS: &[(&str, &str, &str)] = &[
    (
        "opus-balanced",
        "Opus Balanced (Default)",
        "48 kHz stereo Opus (20 ms frames, 128 kbps VBR, in-band FEC) • Best all-around quality and Wi-Fi resilience.",
    ),
    (
        "opus-low-latency",
        "Opus Low-Latency (10 ms)",
        "48 kHz stereo Opus (10 ms frames, 128 kbps VBR, in-band FEC) • Ideal for videos, calls, and gaming.",
    ),
    (
        "opus-low-bandwidth",
        "Opus Low-Bandwidth (64 kbps)",
        "48 kHz stereo Opus (20 ms frames, 64 kbps VBR, in-band FEC) • Smooth playback on congested Wi-Fi.",
    ),
    (
        "pcm-lossless",
        "Encrypted Lossless PCM",
        "48 kHz stereo uncompressed L16 PCM over SRTP • Zero-loss studio audio.",
    ),
];

const DEFAULT_JITTER_MS: u32 = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SettingsSubTab {
    #[default]
    General,
    Audio,
    Privacy,
    Network,
}

pub struct SettingsViewState {
    pub active_subtab: SettingsSubTab,
    pub device_name: String,
    pub latency_ms: u32,
    pub control_port: u16,
    pub pair_port: u16,
    pub rtp_port: u16,
    pub discovery_port: u16,
    pub legacy_tcp_port: u16,
    pub pending_reconnect_hint: bool,
    pub pending_copy_bug_report: bool,
    synced_name: String,
    synced_latency: u32,
    synced_ports: (u16, u16, u16, u16, u16),
}

impl Default for SettingsViewState {
    fn default() -> Self {
        let default_ports = (
            DEFAULT_V3_PORT,
            DEFAULT_PAIR_PORT,
            DEFAULT_RTP_PORT,
            DEFAULT_DISCOVERY_PORT,
            DEFAULT_AUDIO_PORT,
        );
        Self {
            active_subtab: SettingsSubTab::General,
            device_name: String::new(),
            latency_ms: DEFAULT_JITTER_MS,
            control_port: default_ports.0,
            pair_port: default_ports.1,
            rtp_port: default_ports.2,
            discovery_port: default_ports.3,
            legacy_tcp_port: default_ports.4,
            pending_reconnect_hint: false,
            pending_copy_bug_report: false,
            synced_name: String::new(),
            synced_latency: DEFAULT_JITTER_MS,
            synced_ports: default_ports,
        }
    }
}

impl SettingsViewState {
    fn current_ports(&self) -> (u16, u16, u16, u16, u16) {
        (
            self.control_port,
            self.pair_port,
            self.rtp_port,
            self.discovery_port,
            self.legacy_tcp_port,
        )
    }

    fn set_ports(&mut self, ports: (u16, u16, u16, u16, u16)) {
        self.control_port = ports.0;
        self.pair_port = ports.1;
        self.rtp_port = ports.2;
        self.discovery_port = ports.3;
        self.legacy_tcp_port = ports.4;
    }

    fn sync_from_snapshot(&mut self, snap: &GuiSnapshot) {
        if (self.synced_name != snap.config.device.name && self.device_name == self.synced_name)
            || (self.device_name.is_empty() && self.synced_name.is_empty())
        {
            self.device_name = snap.config.device.name.clone();
            self.synced_name = snap.config.device.name.clone();
        }

        let snap_lat = snap
            .config
            .connection
            .latency_ms
            .unwrap_or(DEFAULT_JITTER_MS)
            .clamp(10, 500);
        if self.synced_latency != snap_lat && self.latency_ms == self.synced_latency {
            self.latency_ms = snap_lat;
            self.synced_latency = snap_lat;
        }

        let ports = (
            snap.config.sharing.v3_port,
            snap.config.sharing.pair_port,
            snap.config.sharing.rtp_port,
            snap.config.discovery.port,
            snap.config.sharing.port,
        );
        if self.synced_ports != ports && self.current_ports() == self.synced_ports {
            self.set_ports(ports);
            self.synced_ports = ports;
        }

        if !snap.connected {
            self.pending_reconnect_hint = false;
        }
    }
}

pub fn render(
    ui: &mut egui::Ui,
    pal: &Palette,
    snap: &GuiSnapshot,
    state: &mut SettingsViewState,
    backend: &GuiBackend,
) {
    state.sync_from_snapshot(snap);

    section_header(
        ui,
        pal,
        "Preferences",
        Some("Personalize how this device looks, streams audio, and stays secure."),
    );
    ui.add_space(8.0);

    segmented_control(
        ui,
        pal,
        &mut state.active_subtab,
        &[
            ("General", SettingsSubTab::General),
            ("Audio & Sync", SettingsSubTab::Audio),
            ("Privacy & Devices", SettingsSubTab::Privacy),
            ("Network & Health", SettingsSubTab::Network),
        ],
    );
    ui.add_space(12.0);

    match state.active_subtab {
        SettingsSubTab::General => render_general_tab(ui, pal, snap, state, backend),
        SettingsSubTab::Audio => render_audio_tab(ui, pal, snap, state, backend),
        SettingsSubTab::Privacy => render_privacy_tab(ui, pal, snap, backend),
        SettingsSubTab::Network => render_network_tab(ui, pal, snap, state, backend),
    }
}

fn render_reconnect_hint_banner(ui: &mut egui::Ui, pal: &Palette) {
    callout_card(ui, pal.accent_soft, pal.accent, |ui| {
        ui.horizontal_wrapped(|ui| {
            pill_badge(ui, "Reconnect to Apply", pal.accent, pal.card_elevated);
            ui.label(
                RichText::new(
                    "Settings saved. Reconnect your active speaker to apply the new audio or port configuration.",
                )
                .color(pal.text_secondary)
                .size(12.0),
            );
        });
    });
    ui.add_space(10.0);
}

fn render_toggle_row(
    ui: &mut egui::Ui,
    pal: &Palette,
    title: &str,
    subtitle: &str,
    value: bool,
) -> Option<bool> {
    let mut toggled = value;
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(RichText::new(title).color(pal.text).size(13.5));
            ui.label(RichText::new(subtitle).color(pal.text_muted).size(12.0));
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if toggle_switch(ui, pal, &mut toggled).changed() {
                changed = true;
            }
        });
    });
    changed.then_some(toggled)
}

fn render_general_tab(
    ui: &mut egui::Ui,
    pal: &Palette,
    snap: &GuiSnapshot,
    state: &mut SettingsViewState,
    backend: &GuiBackend,
) {
    // 1. Device & Appearance
    card(ui, pal, None, |ui| {
        ui.label(
            RichText::new("Device & Appearance")
                .color(pal.text)
                .size(15.0)
                .strong(),
        );
        ui.add_space(8.0);

        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new("Device Name").color(pal.text).size(13.5));
                ui.label(
                    RichText::new("How this computer appears to other devices.")
                        .color(pal.text_muted)
                        .size(12.0),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let trimmed = state.device_name.trim();
                let name_changed = trimmed != snap.config.device.name;
                if name_changed {
                    ui.add_enabled_ui(!trimmed.is_empty(), |ui| {
                        if primary_button(ui, pal, "Save").clicked() {
                            let next = trimmed.to_string();
                            state.synced_name = next.clone();
                            backend.send(GuiCommand::SaveConfigPatch {
                                patch: serde_json::json!({ "device": { "name": next } }),
                                toast: Some("Updated device name.".to_string()),
                            });
                        }
                    });
                }
                ui.add(egui::TextEdit::singleline(&mut state.device_name).desired_width(180.0));
            });
        });

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(10.0);

        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new("Appearance").color(pal.text).size(13.5));
                ui.label(
                    RichText::new("Switch between Dark, Light, or your system theme.")
                        .color(pal.text_muted)
                        .size(12.0),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let mut mode = ThemeMode::from_str_loose(&snap.config.ui.theme);
                if segmented_control(
                    ui,
                    pal,
                    &mut mode,
                    &[
                        ("Dark", ThemeMode::Dark),
                        ("Light", ThemeMode::Light),
                        ("System", ThemeMode::System),
                    ],
                ) {
                    backend.send(GuiCommand::SaveConfigPatch {
                        patch: serde_json::json!({ "ui": { "theme": mode.as_config_str() } }),
                        toast: None,
                    });
                }
            });
        });

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(10.0);

        if let Some(notif) = render_toggle_row(
            ui,
            pal,
            "Desktop notifications",
            "Show helpful alerts when speakers connect or disconnect.",
            snap.config.ui.notifications,
        ) {
            backend.send(GuiCommand::SaveConfigPatch {
                patch: serde_json::json!({ "ui": { "notifications": notif } }),
                toast: None,
            });
        }

        ui.add_space(10.0);

        #[cfg(target_os = "windows")]
        let (autostart_title, autostart_sub) = (
            "Start with Windows",
            "Starts quietly in the Windows notification area so your speakers are always ready.",
        );
        #[cfg(not(target_os = "windows"))]
        let (autostart_title, autostart_sub) = (
            "Start SoundHop when I log in",
            "Keeps your system tray and speaker discovery ready in the background.",
        );
        if let Some(start_login) = render_toggle_row(
            ui,
            pal,
            autostart_title,
            autostart_sub,
            snap.autostart_installed || snap.service_enabled,
        ) {
            backend.send(GuiCommand::SetAutostart {
                enabled: start_login,
            });
        }
    });

    ui.add_space(12.0);

    // 2. Connection Behavior
    card(ui, pal, None, |ui| {
        ui.label(
            RichText::new("Connection Behavior")
                .color(pal.text)
                .size(15.0)
                .strong(),
        );
        ui.add_space(8.0);

        if let Some(auto) = render_toggle_row(
            ui,
            pal,
            "Auto-connect to preferred speaker",
            "Automatically reconnect when your last speaker comes online.",
            snap.auto_connect,
        ) {
            backend.send(GuiCommand::SetAutoConnect { enabled: auto });
        }

        ui.add_space(8.0);

        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    RichText::new("Preferred auto-connect speaker")
                        .color(pal.text)
                        .size(13.5),
                );
                ui.label(
                    RichText::new("Choose which speaker SoundHop reconnects to automatically.")
                        .color(pal.text_muted)
                        .size(12.0),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let current_peer_id = snap.config.connection.last_peer.clone();
                if !current_peer_id.is_empty() && secondary_button(ui, pal, "Clear").clicked() {
                    backend.send(GuiCommand::SaveConfigPatch {
                        patch: serde_json::json!({ "connection": { "last_peer": "" } }),
                        toast: Some("Cleared preferred auto-connect speaker.".to_string()),
                    });
                }

                let selected_label = if current_peer_id.is_empty() {
                    "None (Most Recent)".to_string()
                } else {
                    snap.peers
                        .iter()
                        .find(|p| p.id == current_peer_id)
                        .map(|p| p.name.clone())
                        .unwrap_or_else(|| current_peer_id.clone())
                };
                let mut chosen = current_peer_id.clone();
                egui::ComboBox::from_id_salt("settings_preferred_peer_combo")
                    .selected_text(selected_label)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut chosen, String::new(), "None (Most Recent)");
                        for peer in &snap.peers {
                            let label = if peer.online {
                                peer.name.clone()
                            } else {
                                format!("{} (saved)", peer.name)
                            };
                            ui.selectable_value(&mut chosen, peer.id.clone(), label);
                        }
                    });
                if chosen != current_peer_id {
                    let toast = if chosen.is_empty() {
                        "Cleared preferred auto-connect speaker.".to_string()
                    } else {
                        let name = snap
                            .peers
                            .iter()
                            .find(|p| p.id == chosen)
                            .map(|p| p.name.as_str())
                            .unwrap_or(chosen.as_str());
                        format!("Preferred auto-connect speaker set to {name}.")
                    };
                    backend.send(GuiCommand::SaveConfigPatch {
                        patch: serde_json::json!({ "connection": { "last_peer": chosen } }),
                        toast: Some(toast),
                    });
                }
            });
        });

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(10.0);

        if let Some(disconnect_on_quit) = render_toggle_row(
            ui,
            pal,
            "Disconnect speakers when quitting",
            "Automatically return audio to this device when SoundHop exits.",
            snap.config.connection.disconnect_on_quit,
        ) {
            backend.send(GuiCommand::SaveConfigPatch {
                patch: serde_json::json!({
                    "connection": { "disconnect_on_quit": disconnect_on_quit }
                }),
                toast: None,
            });
        }

        ui.add_space(10.0);

        if let Some(keep_local) = render_toggle_row(
            ui,
            pal,
            "Keep local speakers active by default in Multi-Room",
            "Include this computer's speakers alongside remote speakers when starting multi-room.",
            snap.config.connection.keep_local_output,
        ) {
            backend.send(GuiCommand::SaveConfigPatch {
                patch: serde_json::json!({
                    "connection": { "keep_local_output": keep_local }
                }),
                toast: None,
            });
        }

        #[cfg(not(target_os = "windows"))]
        {
            ui.add_space(10.0);
            if let Some(switch_default) = render_toggle_row(
                ui,
                pal,
                "Switch default system output on connect",
                "Route new application audio automatically to the remote speaker.",
                snap.config.connection.switch_default,
            ) {
                backend.send(GuiCommand::SaveConfigPatch {
                    patch: serde_json::json!({ "connection": { "switch_default": switch_default } }),
                    toast: None,
                });
            }

            ui.add_space(10.0);
            if let Some(move_streams) = render_toggle_row(
                ui,
                pal,
                "Move currently playing apps on connect",
                "Transfer already-running audio streams when a speaker connects.",
                snap.config.connection.move_streams,
            ) {
                backend.send(GuiCommand::SaveConfigPatch {
                    patch: serde_json::json!({ "connection": { "move_streams": move_streams } }),
                    toast: None,
                });
            }
        }

        #[cfg(target_os = "windows")]
        {
            ui.add_space(8.0);
            ui.label(
                RichText::new(
                    "Audio capture uses Windows WASAPI loopback without changing your default playback device.",
                )
                .color(pal.text_muted)
                .size(11.5),
            );
        }
    });

    ui.add_space(12.0);

    // 3. Application
    card(ui, pal, None, |ui| {
        ui.label(
            RichText::new("Application")
                .color(pal.text)
                .size(15.0)
                .strong(),
        );
        ui.add_space(8.0);

        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new("Welcome Tour").color(pal.text).size(13.5));
                ui.label(
                    RichText::new("Revisit the quick-start guide and pairing setup.")
                        .color(pal.text_muted)
                        .size(12.0),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if secondary_button(ui, pal, "Replay Welcome Tour").clicked() {
                    backend.send(GuiCommand::ReopenOnboarding);
                }
            });
        });

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(10.0);

        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new("Quit SoundHop").color(pal.text).size(13.5));
                ui.label(
                    RichText::new(
                        "Stop the background service, close the system tray, and exit completely.",
                    )
                    .color(pal.text_muted)
                    .size(12.0),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if danger_button(ui, pal, "Quit SoundHop").clicked() {
                    backend.send(GuiCommand::QuitAll);
                }
            });
        });
    });
}

fn render_gstreamer_status_banner(
    ui: &mut egui::Ui,
    pal: &Palette,
    snap: &GuiSnapshot,
    state: &mut SettingsViewState,
) {
    match (&snap.gstreamer_status, &snap.gstreamer_error) {
        (Some(gst), _) if gst.available => {
            callout_card(ui, pal.success_soft, pal.success, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new("GStreamer Audio Engine")
                                    .color(pal.text)
                                    .size(13.0)
                                    .strong(),
                            );
                            pill_badge(ui, "Ready", pal.success, pal.card_elevated);
                        });
                        let plugin_mode = if gst.bundled_plugin_dir.is_some() {
                            "Bundled plugins"
                        } else {
                            "System plugins"
                        };
                        ui.label(
                            RichText::new(format!(
                                "GStreamer {} • SRTP/Opus & L16 PCM plugins active • {plugin_mode}",
                                gst.version
                            ))
                            .color(pal.text_secondary)
                            .size(11.5),
                        );
                    });
                });
            });
        }
        (Some(gst), _) => {
            callout_card(ui, pal.warning_soft, pal.warning, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new("GStreamer Plugins Incomplete")
                                    .color(pal.warning)
                                    .size(13.0)
                                    .strong(),
                            );
                            pill_badge(ui, "Missing Plugins", pal.warning, pal.card_elevated);
                        });
                        let detail = if !gst.missing_elements.is_empty() {
                            format!(
                                "GStreamer {} missing: {}",
                                gst.version,
                                gst.missing_elements.join(", ")
                            )
                        } else if !gst.audio_src_available || !gst.audio_sink_available {
                            format!(
                                "GStreamer {} audio I/O unavailable (src={}, sink={}).",
                                gst.version, gst.audio_src_available, gst.audio_sink_available
                            )
                        } else {
                            format!("GStreamer {} not ready.", gst.version)
                        };
                        ui.label(RichText::new(detail).color(pal.text_secondary).size(11.5));
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if secondary_button(ui, pal, "Open Diagnostics").clicked() {
                            state.active_subtab = SettingsSubTab::Network;
                        }
                    });
                });
            });
        }
        (None, Some(err)) => {
            callout_card(ui, pal.warning_soft, pal.warning, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("GStreamer Runtime Unavailable")
                                .color(pal.warning)
                                .size(13.0)
                                .strong(),
                        );
                        ui.label(RichText::new(err).color(pal.text_secondary).size(11.5));
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if secondary_button(ui, pal, "Open Diagnostics").clicked() {
                            state.active_subtab = SettingsSubTab::Network;
                        }
                    });
                });
            });
        }
        (None, None) => {
            callout_card(ui, pal.card_elevated, pal.border_subtle, |ui| {
                ui.label(
                    RichText::new("Checking GStreamer runtime & plugins...")
                        .color(pal.text_muted)
                        .size(12.0),
                );
            });
        }
    }
    ui.add_space(10.0);
}

fn render_audio_tab(
    ui: &mut egui::Ui,
    pal: &Palette,
    snap: &GuiSnapshot,
    state: &mut SettingsViewState,
    backend: &GuiBackend,
) {
    render_gstreamer_status_banner(ui, pal, snap, state);

    if snap.connected && state.pending_reconnect_hint {
        render_reconnect_hint_banner(ui, pal);
    }

    // 1. Audio Quality Profile
    card(ui, pal, None, |ui| {
        ui.label(
            RichText::new("Audio Quality Profile")
                .color(pal.text)
                .size(15.0)
                .strong(),
        );
        ui.label(
            RichText::new(
                "Select the GStreamer SRTP/Opus or lossless PCM encoding profile for encrypted streams.",
            )
            .color(pal.text_muted)
            .size(12.0),
        );
        ui.add_space(10.0);

        let active_profile = if snap.audio_profile.is_empty() {
            snap.config.audio.profile.as_str()
        } else {
            snap.audio_profile.as_str()
        };

        for &(profile_key, title, desc) in AUDIO_PROFILE_CARDS {
            let selected = active_profile == profile_key;
            if selectable_card(
                ui,
                pal,
                ("audio_profile", profile_key),
                selected,
                title,
                desc,
            )
            .clicked()
                && !selected
            {
                if snap.connected {
                    state.pending_reconnect_hint = true;
                }
                backend.send(GuiCommand::SaveConfigPatch {
                    patch: serde_json::json!({ "audio": { "profile": profile_key } }),
                    toast: Some(format!("Audio profile set to {title}.")),
                });
            }
            ui.add_space(6.0);
        }
    });

    ui.add_space(12.0);

    // 2. Jitter Buffer & Latency
    card(ui, pal, None, |ui| {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    RichText::new("Jitter Buffer & Latency")
                        .color(pal.text)
                        .size(15.0)
                        .strong(),
                );
                ui.label(
                    RichText::new(
                        "Controls GStreamer rtpjitterbuffer depth (10–500 ms). Lower values reduce delay; higher values absorb Wi-Fi spikes.",
                    )
                    .color(pal.text_muted)
                    .size(12.0),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if snap.config.connection.latency_ms.is_some()
                    && secondary_button(ui, pal, "Reset to Auto (40 ms)").clicked()
                {
                    state.latency_ms = DEFAULT_JITTER_MS;
                    state.synced_latency = DEFAULT_JITTER_MS;
                    if snap.connected {
                        state.pending_reconnect_hint = true;
                    }
                    backend.send(GuiCommand::SaveConfigPatch {
                        patch: serde_json::json!({ "connection": { "latency_ms": null } }),
                        toast: Some("Jitter buffer reset to Auto (40 ms).".to_string()),
                    });
                }
            });
        });
        ui.add_space(8.0);

        ui.horizontal_wrapped(|ui| {
            for (label, ms) in [
                ("Ultra-Low (20 ms)", 20u32),
                ("Balanced (40 ms)", 40),
                ("Wi-Fi (80 ms)", 80),
                ("Stable (150 ms)", 150),
            ] {
                let is_active = state.latency_ms == ms;
                let clicked = if is_active {
                    primary_button(ui, pal, label).clicked()
                } else {
                    secondary_button(ui, pal, label).clicked()
                };
                if clicked && !is_active {
                    state.latency_ms = ms;
                    state.synced_latency = ms;
                    if snap.connected {
                        state.pending_reconnect_hint = true;
                    }
                    backend.send(GuiCommand::SaveConfigPatch {
                        patch: serde_json::json!({ "connection": { "latency_ms": ms } }),
                        toast: Some(format!("Jitter buffer set to {ms} ms.")),
                    });
                }
            }
        });
        ui.add_space(8.0);

        ui.horizontal(|ui| {
            let slider = ui.add(egui::Slider::new(&mut state.latency_ms, 10..=500).suffix(" ms"));
            if slider.drag_stopped() || (slider.changed() && !slider.dragged()) {
                state.synced_latency = state.latency_ms;
                if snap.connected {
                    state.pending_reconnect_hint = true;
                }
                backend.send(GuiCommand::SaveConfigPatch {
                    patch: serde_json::json!({ "connection": { "latency_ms": state.latency_ms } }),
                    toast: None,
                });
            }
        });
    });
}

fn render_privacy_tab(ui: &mut egui::Ui, pal: &Palette, snap: &GuiSnapshot, backend: &GuiBackend) {
    let trusted_count = snap.trusted_peers.len();
    let effective_paired_count = trusted_count.max(snap.paired_peer_count);

    // 1. Speaker Access Control
    card(ui, pal, None, |ui| {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    RichText::new("Speaker Access Control")
                        .color(pal.text)
                        .size(15.0)
                        .strong(),
                );
                ui.label(
                    RichText::new(
                        "Control who on your Wi-Fi can discover and play audio on your speakers.",
                    )
                    .color(pal.text_muted)
                    .size(12.0),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if !snap.sharing_enabled && snap.listener_security.is_empty() {
                    pill_badge(ui, "Sharing Off", pal.text_muted, pal.card_elevated);
                } else {
                    match snap.listener_security.as_str() {
                        "cookie" | "psk" | "paired" | "pin" => {
                            if effective_paired_count > 0
                                || !snap.config.sharing.psk_hash.is_empty()
                            {
                                pill_badge(
                                    ui,
                                    "Enforcing: Paired Only",
                                    pal.success,
                                    pal.success_soft,
                                );
                            } else {
                                pill_badge(ui, "Enforcing: Cookie", pal.accent, pal.accent_soft);
                            }
                        }
                        "trusted-lan" => {
                            pill_badge(ui, "Enforcing: Home Wi-Fi", pal.accent, pal.accent_soft);
                        }
                        "open" => {
                            pill_badge(ui, "Enforcing: Open", pal.warning, pal.warning_soft);
                        }
                        _ => {
                            if effective_paired_count > 0
                                || !snap.config.sharing.psk_hash.is_empty()
                            {
                                pill_badge(ui, "Paired Devices", pal.success, pal.success_soft);
                            } else {
                                pill_badge(ui, "Home Wi-Fi", pal.accent, pal.accent_soft);
                            }
                        }
                    }
                }
            });
        });

        ui.add_space(8.0);
        let psk_mode_selected = matches!(
            snap.config.sharing.security.as_str(),
            "psk" | "paired" | "pin"
        );
        let legacy_pa_status = if psk_mode_selected {
            "Legacy PA-TCP off"
        } else if snap.sharing_enabled {
            "Legacy PA-TCP active"
        } else {
            "Legacy PA-TCP off"
        };
        let trusted_label = format!(
            "{} trusted device{}",
            trusted_count,
            if trusted_count == 1 { "" } else { "s" }
        );
        let live_enforcement_line = if !snap.sharing_enabled && snap.listener_security.is_empty() {
            format!("Listener inactive (sharing off) • {trusted_label} • {legacy_pa_status}")
        } else if psk_mode_selected {
            format!("Enforcing: Paired Devices Only • {trusted_label} • {legacy_pa_status}")
        } else if snap.listener_security.is_empty() {
            format!(
                "Configured: {} • {trusted_label} • {legacy_pa_status}",
                security_friendly_label(&snap.config.sharing.security)
            )
        } else {
            let enforced_label = match snap.listener_security.as_str() {
                "cookie" => "System Audio Cookie",
                "psk" | "paired" | "pin" => "Paired Devices Only",
                "open" => "Open (No Check)",
                "trusted-lan" => "Home Wi-Fi (Automatic)",
                other => other,
            };
            format!("Enforcing: {enforced_label} • {trusted_label} • {legacy_pa_status}")
        };
        callout_card(
            ui,
            if psk_mode_selected {
                pal.success_soft
            } else {
                pal.card_elevated
            },
            if psk_mode_selected {
                pal.success
            } else {
                pal.border
            },
            |ui| {
                ui.label(
                    RichText::new(live_enforcement_line)
                        .color(if psk_mode_selected {
                            pal.success
                        } else {
                            pal.text_secondary
                        })
                        .size(12.0)
                        .strong(),
                );
            },
        );

        if psk_mode_selected
            && effective_paired_count == 0
            && snap.config.sharing.psk_hash.is_empty()
        {
            ui.add_space(8.0);
            callout_card(ui, pal.warning_soft, pal.warning, |ui| {
                ui.label(
                    RichText::new("No trusted devices paired yet")
                        .color(pal.warning)
                        .size(12.5)
                        .strong(),
                );
                ui.label(
                    RichText::new(
                        "\"Paired Devices Only\" is selected, but no devices are trusted yet. Click Pair on a nearby speaker in Play & Share to verify its 6-digit code.",
                    )
                    .color(pal.text_secondary)
                    .size(11.5),
                );
            });
        }

        ui.add_space(10.0);

        for (mode_key, title, desc) in [
            (
                "trusted-lan",
                "Home Wi-Fi (Automatic)",
                "Great for home networks — any SoundHop device on your Wi-Fi can connect.",
            ),
            (
                "psk",
                "Paired Devices Only (Recommended on shared Wi-Fi)",
                "Enforces encrypted SRTP streaming and requires Pairing v2 verification between devices.",
            ),
            (
                "cookie",
                "System Audio Cookie",
                "Uses your computer's built-in PulseAudio/PipeWire network cookie.",
            ),
        ] {
            // PulseAudio/PipeWire cookie mode is Linux-only; only show on Windows if already active.
            #[cfg(target_os = "windows")]
            if mode_key == "cookie" && snap.config.sharing.security != "cookie" {
                continue;
            }
            let selected = snap.config.sharing.security == mode_key;
            if selectable_card(ui, pal, ("privacy_mode", mode_key), selected, title, desc).clicked()
                && !selected
            {
                backend.send(GuiCommand::SaveConfigPatch {
                    patch: serde_json::json!({ "sharing": { "security": mode_key } }),
                    toast: Some(format!("Switched privacy mode to {title}.")),
                });
            }
            ui.add_space(6.0);
        }
    });

    ui.add_space(12.0);

    // 2. Trusted Devices
    card(ui, pal, None, |ui| {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("Trusted Devices")
                    .color(pal.text)
                    .size(15.0)
                    .strong(),
            );
            pill_badge(ui, &format!("{trusted_count}"), pal.accent, pal.accent_soft);
        });
        ui.add_space(6.0);
        if snap.trusted_peers.is_empty() {
            ui.label(
                RichText::new(
                    "No trusted devices yet. Click Pair on a device in Play & Share or run 'soundhop pair request <host>'.",
                )
                .color(pal.text_muted)
                .size(12.0),
            );
        } else {
            for tp in &snap.trusted_peers {
                callout_card(ui, pal.card_elevated, pal.border_subtle, |ui| {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(
                                RichText::new(&tp.display_name)
                                    .color(pal.text)
                                    .size(13.0)
                                    .strong(),
                            );
                            ui.label(
                                RichText::new(format!(
                                    "Fingerprint: {} • ID: {}",
                                    tp.fingerprint, tp.device_id
                                ))
                                .color(pal.text_muted)
                                .size(11.0)
                                .monospace(),
                            );
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if danger_button(ui, pal, "Revoke").clicked() {
                                backend.send(GuiCommand::RevokeTrustedPeer {
                                    device_id: tp.device_id.clone(),
                                });
                            }
                        });
                    });
                });
                ui.add_space(4.0);
            }
        }
    });
}

fn port_drag_value(ui: &mut egui::Ui, port: &mut u16) {
    let mut val = u32::from(*port);
    if ui
        .add(egui::DragValue::new(&mut val).range(1024..=65535))
        .changed()
    {
        *port = val as u16;
    }
}

fn render_network_tab(
    ui: &mut egui::Ui,
    pal: &Palette,
    snap: &GuiSnapshot,
    state: &mut SettingsViewState,
    backend: &GuiBackend,
) {
    if snap.connected && state.pending_reconnect_hint {
        render_reconnect_hint_banner(ui, pal);
    }

    // 1. LAN Discovery
    card(ui, pal, None, |ui| {
        ui.label(
            RichText::new("LAN Discovery")
                .color(pal.text)
                .size(15.0)
                .strong(),
        );
        ui.add_space(8.0);

        if let Some(mdns) = render_toggle_row(
            ui,
            pal,
            "mDNS / Zeroconf",
            "Advertise and browse nearby SoundHop devices using standard multicast DNS.",
            snap.config.discovery.mdns,
        ) {
            backend.send(GuiCommand::SaveConfigPatch {
                patch: serde_json::json!({ "discovery": { "mdns": mdns } }),
                toast: None,
            });
        }

        ui.add_space(10.0);

        if let Some(udp) = render_toggle_row(
            ui,
            pal,
            "UDP LAN Beacon",
            "Broadcast authenticated discovery packets on your local subnet.",
            snap.config.discovery.udp,
        ) {
            backend.send(GuiCommand::SaveConfigPatch {
                patch: serde_json::json!({ "discovery": { "udp": udp } }),
                toast: None,
            });
        }
    });

    ui.add_space(12.0);

    // 2. Network Ports
    card(ui, pal, None, |ui| {
        let default_ports = (
            DEFAULT_V3_PORT,
            DEFAULT_PAIR_PORT,
            DEFAULT_RTP_PORT,
            DEFAULT_DISCOVERY_PORT,
            DEFAULT_AUDIO_PORT,
        );
        let paired_mode = is_paired_security_mode(&snap.config.sharing.security);

        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    RichText::new("Network Ports")
                        .color(pal.text)
                        .size(15.0)
                        .strong(),
                );
                ui.label(
                    RichText::new(
                        "Encrypted control, pairing, SRTP media, discovery, and legacy PA-TCP ports.",
                    )
                    .color(pal.text_muted)
                    .size(12.0),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if state.current_ports() != state.synced_ports
                    && primary_button(ui, pal, "Apply Ports").clicked()
                {
                    let next = state.current_ports();
                    state.synced_ports = next;
                    if snap.connected {
                        state.pending_reconnect_hint = true;
                    }
                    backend.send(GuiCommand::SaveConfigPatch {
                        patch: serde_json::json!({
                            "sharing": {
                                "v3_port": next.0,
                                "pair_port": next.1,
                                "rtp_port": next.2,
                                "port": next.4,
                            },
                            "discovery": { "port": next.3 }
                        }),
                        toast: Some("Updated network ports.".to_string()),
                    });
                }
                if state.current_ports() != default_ports
                    && secondary_button(ui, pal, "Defaults").clicked()
                {
                    state.set_ports(default_ports);
                    state.synced_ports = default_ports;
                    if snap.connected {
                        state.pending_reconnect_hint = true;
                    }
                    backend.send(GuiCommand::SaveConfigPatch {
                        patch: serde_json::json!({
                            "sharing": {
                                "v3_port": default_ports.0,
                                "pair_port": default_ports.1,
                                "rtp_port": default_ports.2,
                                "port": default_ports.4,
                            },
                            "discovery": { "port": default_ports.3 }
                        }),
                        toast: Some("Restored default network ports.".to_string()),
                    });
                }
            });
        });

        ui.add_space(10.0);

        egui::Grid::new("settings_network_ports_grid")
            .num_columns(3)
            .spacing([14.0, 8.0])
            .show(ui, |ui| {
                ui.label(
                    RichText::new("Control Channel (TCP)")
                        .color(pal.text)
                        .size(13.0),
                );
                port_drag_value(ui, &mut state.control_port);
                ui.label(
                    RichText::new("Encrypted handshake, volume & telemetry")
                        .color(pal.text_muted)
                        .size(11.5),
                );
                ui.end_row();

                ui.label(
                    RichText::new("Pairing Handshake (TCP)")
                        .color(pal.text)
                        .size(13.0),
                );
                port_drag_value(ui, &mut state.pair_port);
                ui.label(
                    RichText::new("Pairing v2 interactive code exchange")
                        .color(pal.text_muted)
                        .size(11.5),
                );
                ui.end_row();

                ui.label(
                    RichText::new("SRTP Audio Media (UDP)")
                        .color(pal.text)
                        .size(13.0),
                );
                port_drag_value(ui, &mut state.rtp_port);
                ui.label(
                    RichText::new("Encrypted GStreamer RTP/Opus & PCM stream")
                        .color(pal.text_muted)
                        .size(11.5),
                );
                ui.end_row();

                ui.label(
                    RichText::new("LAN Discovery (UDP)")
                        .color(pal.text)
                        .size(13.0),
                );
                port_drag_value(ui, &mut state.discovery_port);
                ui.label(
                    RichText::new("Local subnet peer discovery beacons")
                        .color(pal.text_muted)
                        .size(11.5),
                );
                ui.end_row();

                ui.label(
                    RichText::new("Legacy PA-TCP (TCP)")
                        .color(pal.text_secondary)
                        .size(13.0),
                );
                port_drag_value(ui, &mut state.legacy_tcp_port);
                let legacy_note = if paired_mode {
                    "Disabled in Paired Devices Only mode"
                } else {
                    "PulseAudio / PipeWire compatibility tunnel"
                };
                ui.label(RichText::new(legacy_note).color(pal.text_muted).size(11.5));
                ui.end_row();
            });
    });

    ui.add_space(12.0);

    // 3. Connection Health & Diagnostics
    card(ui, pal, None, |ui| {
        ui.label(
            RichText::new("Connection Health & Diagnostics")
                .color(pal.text)
                .size(15.0)
                .strong(),
        );
        ui.add_space(6.0);

        let transport_str = if !snap.connected {
            "Idle"
        } else if snap.connected_transport.is_empty() {
            "Active"
        } else {
            snap.connected_transport.as_str()
        };
        let gst_str = match (&snap.gstreamer_status, &snap.gstreamer_error) {
            (Some(gst), _) if gst.available => gst.version.clone(),
            (Some(gst), _) => format!("{} (incomplete)", gst.version),
            (None, Some(_)) => "Unavailable".to_string(),
            (None, None) => "Checking".to_string(),
        };
        let rtt_str = snap
            .rtt_ms
            .map(|r| format!("{r:.1} ms"))
            .unwrap_or_else(|| "--".to_string());
        let jit_str = snap
            .jitter_ms
            .map(|j| format!("{j:.1} ms"))
            .unwrap_or_else(|| "--".to_string());
        let jb_str = snap
            .jitter_buf_ms
            .map(|b| format!("{b} ms"))
            .unwrap_or_else(|| "--".to_string());
        let drop_str = snap
            .dropped_frames
            .map(|d| d.to_string())
            .unwrap_or_else(|| "--".to_string());
        let fw_display = if snap.firewall_kind == "windows-defender" {
            "Windows Defender Firewall"
        } else {
            snap.firewall_kind.as_str()
        };

        ui.label(
            RichText::new(format!(
                "Daemon: {} • Transport: {} • GStreamer: {} • Firewall: {}\nRTT: {} • Jitter: {} • Buffer: {} • Drops: {} • Audio: {:.1} dBFS",
                if snap.daemon_connected {
                    "Active"
                } else {
                    "Standalone"
                },
                transport_str,
                gst_str,
                fw_display,
                rtt_str,
                jit_str,
                jb_str,
                drop_str,
                snap.audio_dbfs,
            ))
            .color(pal.text_muted)
            .size(11.5)
            .monospace(),
        );
        ui.add_space(8.0);

        ui.horizontal_wrapped(|ui| {
            ui.add_enabled_ui(!snap.diagnostics_running, |ui| {
                let diag_label = if snap.diagnostics_running {
                    "Running Diagnostics..."
                } else {
                    "Run Diagnostics & Fix"
                };
                if primary_button(ui, pal, diag_label).clicked() {
                    backend.send(GuiCommand::RunDiagnostics { fix: true });
                }
            });
            if secondary_button(ui, pal, "Copy / Export Bug Report").clicked() {
                state.pending_copy_bug_report = true;
                backend.send(GuiCommand::ExportBugReport);
            }
            #[cfg(target_os = "windows")]
            let fw_btn_label = "Allow in Windows Firewall";
            #[cfg(not(target_os = "windows"))]
            let fw_btn_label = "Open Firewall Ports";
            if secondary_button(ui, pal, fw_btn_label).clicked() {
                backend.send(GuiCommand::OpenFirewall);
            }
        });

        if state.pending_copy_bug_report {
            if let Some(ref report_json) = snap.bug_report_json {
                ui.ctx().copy_text(report_json.clone());
                state.pending_copy_bug_report = false;
            }
        }

        if let Some(ref summary) = snap.diagnostics_summary {
            ui.add_space(8.0);
            let (bg, border) = if snap.diagnostics_healthy == Some(true) {
                (pal.success_soft, pal.success)
            } else {
                (pal.warning_soft, pal.warning)
            };
            callout_card(ui, bg, border, |ui| {
                ui.label(
                    RichText::new("Diagnostics Summary")
                        .color(border)
                        .size(12.5)
                        .strong(),
                );
                ui.add_space(4.0);
                ui.label(
                    RichText::new(summary)
                        .color(pal.text_secondary)
                        .size(11.5)
                        .monospace(),
                );
            });
        }

        if let Some(ref report_json) = snap.bug_report_json {
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                if secondary_button(ui, pal, "Copy Report JSON").clicked() {
                    ui.ctx().copy_text(report_json.clone());
                }
                if let Some(ref path) = snap.bug_report_path {
                    ui.label(
                        RichText::new(format!("Saved: {path}"))
                            .color(pal.text_muted)
                            .size(11.5)
                            .monospace(),
                    );
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{render, SettingsSubTab, SettingsViewState, AUDIO_PROFILE_CARDS};
    use crate::backend::{GuiBackend, GuiSnapshot};
    use crate::theme::Palette;
    use eframe::egui;
    use soundhop_core::config::{Config, VALID_AUDIO_PROFILES};
    use soundhop_core::controller::Controller;
    use soundhop_core::state::State;
    use soundhop_core::transport::GstreamerRuntimeStatus;
    use std::sync::Arc;

    #[test]
    fn audio_profile_options_match_valid_audio_profiles() {
        let keys: Vec<&str> = AUDIO_PROFILE_CARDS.iter().map(|(k, _, _)| *k).collect();
        assert_eq!(keys, VALID_AUDIO_PROFILES);
        for (_, title, desc) in AUDIO_PROFILE_CARDS {
            assert!(!title.is_empty());
            assert!(!desc.is_empty());
        }
    }

    #[test]
    fn settings_subtabs_render_cleanly_in_both_themes_and_states() {
        let controller = Arc::new(Controller::new(
            Some(Config::default()),
            Some(State::default()),
            None,
            None,
            None,
            false,
        ));
        let backend = GuiBackend::start(controller.clone(), None);
        let mut snap = GuiSnapshot::from_controller(&controller);
        snap.connected = true;
        snap.connected_transport = "gst-srtp".to_string();
        snap.jitter_buf_ms = Some(40);
        snap.dropped_frames = Some(2);
        snap.gstreamer_status = Some(GstreamerRuntimeStatus {
            available: true,
            version: "1.28.9".to_string(),
            missing_elements: Vec::new(),
            audio_src_available: true,
            audio_sink_available: true,
            bundled_plugin_dir: None,
        });

        let mut state = SettingsViewState::default();
        let ctx = egui::Context::default();
        for pal in [Palette::dark(), Palette::light()] {
            for tab in [
                SettingsSubTab::General,
                SettingsSubTab::Audio,
                SettingsSubTab::Privacy,
                SettingsSubTab::Network,
            ] {
                state.active_subtab = tab;
                state.pending_reconnect_hint = true;
                let _ = ctx.run(egui::RawInput::default(), |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        render(ui, &pal, &snap, &mut state, &backend);
                    });
                });
            }
        }

        // Disconnected + missing plugins clears pending_reconnect_hint on sync
        snap.connected = false;
        snap.gstreamer_status = Some(GstreamerRuntimeStatus {
            available: false,
            version: "1.20.3".to_string(),
            missing_elements: vec!["srtpenc".to_string()],
            audio_src_available: true,
            audio_sink_available: true,
            bundled_plugin_dir: None,
        });
        state.active_subtab = SettingsSubTab::Audio;
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                render(ui, &Palette::dark(), &snap, &mut state, &backend);
            });
        });
        assert!(!state.pending_reconnect_hint);
    }
}
