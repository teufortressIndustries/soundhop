//! Preferences view: clean intent-first settings cards, visual PIN pairing,
//! Dark/Light/System theme selector, and collapsible Advanced diagnostics.

use eframe::egui::{self, RichText};

use crate::backend::{security_friendly_label, GuiBackend, GuiCommand, GuiSnapshot};
use crate::theme::{Palette, ThemeMode};
use crate::widgets::{
    callout_card, card, collapsible_advanced, danger_button, pill_badge, primary_button,
    secondary_button, section_header, segmented_control, selectable_card, toggle_switch,
};

const AUDIO_PROFILE_OPTIONS: &[(&str, &str)] = &[
    ("opus-balanced", "Balanced (Opus 128k)"),
    ("opus-low-latency", "Low Latency (Opus 10ms)"),
    ("opus-low-bandwidth", "Wi-Fi Saver (Opus 64k)"),
    ("pcm-lossless", "Lossless (Uncompressed PCM)"),
];

pub struct SettingsViewState {
    pub device_name: String,
    pub pairing_code_input: String,
    pub generated_code: String,
    pub latency_ms: u32,
    pub tcp_port: u16,
    pub discovery_port: u16,
    pub pending_copy_bug_report: bool,
    synced_name: String,
    synced_latency: u32,
    synced_ports: (u16, u16),
}

impl Default for SettingsViewState {
    fn default() -> Self {
        Self {
            device_name: String::new(),
            pairing_code_input: String::new(),
            generated_code: String::new(),
            latency_ms: 150,
            tcp_port: 4713,
            discovery_port: 47777,
            pending_copy_bug_report: false,
            synced_name: String::new(),
            synced_latency: 150,
            synced_ports: (4713, 47777),
        }
    }
}

impl SettingsViewState {
    fn sync_from_snapshot(&mut self, snap: &GuiSnapshot) {
        if (self.synced_name != snap.config.device.name && self.device_name == self.synced_name)
            || (self.device_name.is_empty() && self.synced_name.is_empty())
        {
            self.device_name = snap.config.device.name.clone();
            self.synced_name = snap.config.device.name.clone();
        }

        let snap_lat = snap.config.connection.latency_ms.unwrap_or(150);
        if self.synced_latency != snap_lat && self.latency_ms == self.synced_latency {
            self.latency_ms = snap_lat;
            self.synced_latency = snap_lat;
        }

        let ports = (snap.config.sharing.port, snap.config.discovery.port);
        if self.synced_ports != ports && (self.tcp_port, self.discovery_port) == self.synced_ports {
            self.tcp_port = ports.0;
            self.discovery_port = ports.1;
            self.synced_ports = ports;
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
        Some("Personalize how this device looks, connects, and stays secure."),
    );
    ui.add_space(10.0);

    // 1. General & Appearance
    card(ui, pal, None, |ui| {
        ui.label(
            RichText::new("General & Appearance")
                .color(pal.text)
                .size(15.0)
                .strong(),
        );
        ui.add_space(8.0);

        // Device Name
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

        // Theme selector
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

        // Auto-connect
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    RichText::new("Auto-connect to preferred speaker")
                        .color(pal.text)
                        .size(13.5),
                );
                ui.label(
                    RichText::new("Automatically reconnect when your last speaker comes online.")
                        .color(pal.text_muted)
                        .size(12.0),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let mut auto = snap.auto_connect;
                if toggle_switch(ui, pal, &mut auto).changed() {
                    backend.send(GuiCommand::SetAutoConnect { enabled: auto });
                }
            });
        });

        ui.add_space(8.0);

        // Preferred Auto-Connect Peer selector & clear
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

        // Disconnect on Quit
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    RichText::new("Disconnect speakers when quitting")
                        .color(pal.text)
                        .size(13.5),
                );
                ui.label(
                    RichText::new("Automatically return audio to this device when SoundHop exits.")
                        .color(pal.text_muted)
                        .size(12.0),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let mut disconnect_on_quit = snap.config.connection.disconnect_on_quit;
                if toggle_switch(ui, pal, &mut disconnect_on_quit).changed() {
                    backend.send(GuiCommand::SaveConfigPatch {
                        patch: serde_json::json!({
                            "connection": { "disconnect_on_quit": disconnect_on_quit }
                        }),
                        toast: None,
                    });
                }
            });
        });

        ui.add_space(10.0);

        // Notifications
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    RichText::new("Desktop notifications")
                        .color(pal.text)
                        .size(13.5),
                );
                ui.label(
                    RichText::new("Show helpful alerts when speakers connect or disconnect.")
                        .color(pal.text_muted)
                        .size(12.0),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let mut notif = snap.config.ui.notifications;
                if toggle_switch(ui, pal, &mut notif).changed() {
                    backend.send(GuiCommand::SaveConfigPatch {
                        patch: serde_json::json!({ "ui": { "notifications": notif } }),
                        toast: None,
                    });
                }
            });
        });

        ui.add_space(10.0);

        // Start at Login
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
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
                ui.label(RichText::new(autostart_title).color(pal.text).size(13.5));
                ui.label(
                    RichText::new(autostart_sub)
                        .color(pal.text_muted)
                        .size(12.0),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let mut start_login = snap.autostart_installed || snap.service_enabled;
                if toggle_switch(ui, pal, &mut start_login).changed() {
                    backend.send(GuiCommand::SetAutostart {
                        enabled: start_login,
                    });
                }
            });
        });

        ui.add_space(10.0);

        // System / Background Service
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    RichText::new("Background Service & Tray")
                        .color(pal.text)
                        .size(13.5),
                );
                ui.label(
                    RichText::new(
                        "Quit SoundHop completely, stopping the background service and system tray.",
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

    ui.add_space(12.0);

    // 2. Audio Quality & Latency (SHT3 Transport Profile)
    card(ui, pal, None, |ui| {
        ui.label(
            RichText::new("Audio Quality & Latency")
                .color(pal.text)
                .size(15.0)
                .strong(),
        );
        ui.label(
            RichText::new(
                "Choose the SoundHop Transport v3 (SHT3) codec and latency profile for encrypted streaming.",
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

        for (profile_key, title, desc) in [
            (
                "opus-balanced",
                "Opus Balanced (Default)",
                "48 kHz stereo Opus (20 ms frames, 128 kbps VBR) • Best all-around quality and Wi-Fi resilience.",
            ),
            (
                "opus-low-latency",
                "Opus Low-Latency (10 ms)",
                "48 kHz stereo Opus (10 ms frames, 128 kbps VBR) • Ideal for videos, calls, and gaming.",
            ),
            (
                "opus-low-bandwidth",
                "Opus Low-Bandwidth (64 kbps)",
                "48 kHz stereo Opus (20 ms frames, 64 kbps VBR) • Smooth playback on congested Wi-Fi.",
            ),
            (
                "pcm-lossless",
                "Encrypted Lossless PCM",
                "48 kHz stereo uncompressed S16LE over ChaCha20-Poly1305 • Zero-loss studio audio.",
            ),
        ] {
            let selected = active_profile == profile_key;
            if selectable_card(ui, pal, ("audio_profile", profile_key), selected, title, desc)
                .clicked()
                && !selected
            {
                backend.send(GuiCommand::SaveConfigPatch {
                    patch: serde_json::json!({ "audio": { "profile": profile_key } }),
                    toast: Some(format!("Audio profile set to {title}.")),
                });
            }
            ui.add_space(6.0);
        }
    });

    ui.add_space(12.0);

    // 3. Privacy & Pairing
    card(ui, pal, None, |ui| {
        let trusted_count = snap.trusted_peers.len();
        let effective_paired_count = trusted_count.max(snap.paired_peer_count);
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    RichText::new("Privacy & Pairing")
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
                        "\"Paired Devices Only\" is selected, but no devices are trusted yet. Click Pair on a nearby speaker in Play & Share, or enter a shared code below.",
                    )
                    .color(pal.text_secondary)
                    .size(11.5),
                );
            });
        }

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(8.0);

        // Trusted Devices (Pairing v2)
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("Trusted Devices")
                    .color(pal.text)
                    .size(13.5)
                    .strong(),
            );
            pill_badge(ui, &format!("{trusted_count}"), pal.accent, pal.accent_soft);
        });
        ui.add_space(4.0);
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

        ui.add_space(10.0);

        for (mode_key, title, desc) in [
            (
                "trusted-lan",
                "Home Wi-Fi (Automatic)",
                "Great for home networks — any SoundHop device on your Wi-Fi can connect.",
            ),
            (
                "psk",
                "PIN-Paired Devices Only (Recommended on shared Wi-Fi)",
                "Requires matching a secret pairing code between your devices.",
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

    ui.add_space(10.0);

    // 3. Progressive Disclosure: Advanced Settings & Diagnostics (collapsed by default)
    collapsible_advanced(
        ui,
        pal,
        "settings_advanced_drawer",
        "Advanced Audio, Network & Diagnostics",
        |ui| {
            // Audio Codec & Quality (SHT3)
            ui.label(
                RichText::new("Audio Codec & Quality (SHT3)")
                    .color(pal.text)
                    .size(13.5)
                    .strong(),
            );
            ui.label(
                RichText::new(
                    "Opus compresses and protects encrypted streams over Wi-Fi; Lossless sends raw 48 kHz stereo PCM.",
                )
                .color(pal.text_muted)
                .size(12.0),
            );
            ui.add_space(6.0);
            let current_profile = snap.config.audio.profile.as_str();
            let selected_label = AUDIO_PROFILE_OPTIONS
                .iter()
                .find(|(key, _)| *key == current_profile)
                .map(|(_, label)| *label)
                .unwrap_or(current_profile);
            let mut chosen = snap.config.audio.profile.clone();
            egui::ComboBox::from_id_salt("settings_audio_profile_combo")
                .selected_text(selected_label)
                .show_ui(ui, |ui| {
                    for (key, label) in AUDIO_PROFILE_OPTIONS {
                        ui.selectable_value(&mut chosen, (*key).to_string(), *label);
                    }
                });
            if chosen != snap.config.audio.profile {
                let chosen_label = AUDIO_PROFILE_OPTIONS
                    .iter()
                    .find(|(key, _)| *key == chosen)
                    .map(|(_, label)| *label)
                    .unwrap_or(chosen.as_str());
                backend.send(GuiCommand::SaveConfigPatch {
                    patch: serde_json::json!({ "audio": { "profile": chosen } }),
                    toast: Some(format!("Audio profile set to {chosen_label}.")),
                });
            }

            ui.add_space(10.0);
            ui.separator();
            ui.add_space(8.0);

            // Latency Presets
            ui.label(
                RichText::new("Audio Sync & Buffer Latency")
                    .color(pal.text)
                    .size(13.5)
                    .strong(),
            );
            ui.label(
                RichText::new(
                    "Lower latency is better for videos; higher stability helps on busy Wi-Fi.",
                )
                .color(pal.text_muted)
                .size(12.0),
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                for (label, ms) in [
                    ("Low (50 ms)", 50u32),
                    ("Balanced (150 ms)", 150),
                    ("Stable (300 ms)", 300),
                ] {
                    if secondary_button(ui, pal, label).clicked() {
                        state.latency_ms = ms;
                        state.synced_latency = ms;
                        backend.send(GuiCommand::SaveConfigPatch {
                            patch: serde_json::json!({ "connection": { "latency_ms": ms } }),
                            toast: Some(format!("Audio buffer set to {ms} ms.")),
                        });
                    }
                }
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let slider = ui.add(egui::Slider::new(&mut state.latency_ms, 10..=2000).suffix(" ms"));
                if slider.drag_stopped() || (slider.changed() && !slider.dragged()) {
                    state.synced_latency = state.latency_ms;
                    backend.send(GuiCommand::SaveConfigPatch {
                        patch: serde_json::json!({ "connection": { "latency_ms": state.latency_ms } }),
                        toast: None,
                    });
                }
            });

            ui.add_space(10.0);
            ui.separator();
            ui.add_space(8.0);

            // Behavior toggles
            #[cfg(not(target_os = "windows"))]
            for (title, val, key) in [
                (
                    "Switch default system output on connect",
                    snap.config.connection.switch_default,
                    "switch_default",
                ),
                (
                    "Move currently playing apps on connect",
                    snap.config.connection.move_streams,
                    "move_streams",
                ),
            ] {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(title).color(pal.text_secondary).size(12.5));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let mut b = val;
                        if toggle_switch(ui, pal, &mut b).changed() {
                            backend.send(GuiCommand::SaveConfigPatch {
                                patch: serde_json::json!({ "connection": { key: b } }),
                                toast: None,
                            });
                        }
                    });
                });
            }
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Keep local speakers active by default in Multi-Room")
                        .color(pal.text_secondary)
                        .size(12.5),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let mut b = snap.config.connection.keep_local_output;
                    if toggle_switch(ui, pal, &mut b).changed() {
                        backend.send(GuiCommand::SaveConfigPatch {
                            patch: serde_json::json!({ "connection": { "keep_local_output": b } }),
                            toast: None,
                        });
                    }
                });
            });
            #[cfg(target_os = "windows")]
            {
                ui.add_space(4.0);
                ui.label(
                    RichText::new(
                        "Audio capture uses Windows WASAPI loopback without changing your default playback device.",
                    )
                    .color(pal.text_muted)
                    .size(11.5),
                );
            }

            ui.add_space(10.0);
            ui.separator();
            ui.add_space(8.0);

            // Network Ports & Discovery
            ui.label(
                RichText::new("Network Ports & Discovery")
                    .color(pal.text)
                    .size(13.5)
                    .strong(),
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let mut mdns = snap.config.discovery.mdns;
                if ui.checkbox(&mut mdns, "mDNS / Zeroconf").changed() {
                    backend.send(GuiCommand::SaveConfigPatch {
                        patch: serde_json::json!({ "discovery": { "mdns": mdns } }),
                        toast: None,
                    });
                }
                let mut udp = snap.config.discovery.udp;
                if ui.checkbox(&mut udp, "UDP LAN Beacon").changed() {
                    backend.send(GuiCommand::SaveConfigPatch {
                        patch: serde_json::json!({ "discovery": { "udp": udp } }),
                        toast: None,
                    });
                }
            });
            ui.horizontal(|ui| {
                ui.label(RichText::new("Audio TCP Port:").color(pal.text_secondary));
                let mut tp = state.tcp_port as u32;
                if ui
                    .add(egui::DragValue::new(&mut tp).range(1024..=65535))
                    .changed()
                {
                    state.tcp_port = tp as u16;
                }
                ui.label(RichText::new("Discovery UDP Port:").color(pal.text_secondary));
                let mut dp = state.discovery_port as u32;
                if ui
                    .add(egui::DragValue::new(&mut dp).range(1024..=65535))
                    .changed()
                {
                    state.discovery_port = dp as u16;
                }
                if (state.tcp_port, state.discovery_port) != state.synced_ports
                    && secondary_button(ui, pal, "Apply Ports").clicked()
                {
                    state.synced_ports = (state.tcp_port, state.discovery_port);
                    backend.send(GuiCommand::SaveConfigPatch {
                        patch: serde_json::json!({
                            "sharing": { "port": state.tcp_port },
                            "discovery": { "port": state.discovery_port }
                        }),
                        toast: Some("Updated network ports.".to_string()),
                    });
                }
            });

            ui.add_space(10.0);
            ui.separator();
            ui.add_space(8.0);

            // Live Diagnostics & Welcome Tour
            ui.label(
                RichText::new("Connection Health & Diagnostics")
                    .color(pal.text)
                    .size(13.5)
                    .strong(),
            );
            ui.add_space(4.0);
            let rtt_str = snap
                .rtt_ms
                .map(|r| format!("{r:.1} ms"))
                .unwrap_or_else(|| "--".to_string());
            let jit_str = snap
                .jitter_ms
                .map(|j| format!("{j:.1} ms"))
                .unwrap_or_else(|| "--".to_string());
            let fw_display = if snap.firewall_kind == "windows-defender" {
                "Windows Defender Firewall"
            } else {
                snap.firewall_kind.as_str()
            };
            ui.label(
                RichText::new(format!(
                    "Daemon: {} • Firewall: {} • RTT: {} • Jitter: {} • Audio: {:.1} dBFS",
                    if snap.daemon_connected {
                        "Active"
                    } else {
                        "Standalone"
                    },
                    fw_display,
                    rtt_str,
                    jit_str,
                    snap.audio_dbfs,
                ))
                .color(pal.text_muted)
                .size(11.5)
                .monospace(),
            );
            ui.add_space(6.0);
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
                if secondary_button(ui, pal, "Replay Welcome Tour").clicked() {
                    backend.send(GuiCommand::ReopenOnboarding);
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
        },
    );
}

#[cfg(test)]
mod tests {
    use super::AUDIO_PROFILE_OPTIONS;
    use soundhop_core::config::VALID_AUDIO_PROFILES;

    #[test]
    fn audio_profile_options_match_valid_audio_profiles() {
        let keys: Vec<&str> = AUDIO_PROFILE_OPTIONS.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, VALID_AUDIO_PROFILES);
        for (_, label) in AUDIO_PROFILE_OPTIONS {
            assert!(!label.is_empty());
        }
    }
}
