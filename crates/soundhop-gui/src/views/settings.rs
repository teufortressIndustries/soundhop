//! Preferences view: clean intent-first settings cards, visual PIN pairing,
//! Dark/Light/System theme selector, and collapsible Advanced diagnostics.

use eframe::egui::{self, RichText};
use soundhop_core::security::generate_pairing_pin;

use crate::backend::{GuiBackend, GuiCommand, GuiSnapshot};
use crate::theme::{Palette, ThemeMode};
use crate::widgets::{
    card, collapsible_advanced, danger_button, pill_badge, primary_button, secondary_button,
    section_header, segmented_control, selectable_card, toggle_switch,
};

pub struct SettingsViewState {
    pub device_name: String,
    pub pairing_code_input: String,
    pub generated_code: String,
    pub latency_ms: u32,
    pub tcp_port: u16,
    pub discovery_port: u16,
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
    });

    ui.add_space(12.0);

    // 2. Privacy & Pairing
    card(ui, pal, None, |ui| {
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
                if !snap.config.sharing.psk_hash.is_empty() {
                    pill_badge(ui, "PIN Paired", pal.success, pal.success_soft);
                } else {
                    pill_badge(ui, "Home Wi-Fi", pal.accent, pal.accent_soft);
                }
            });
        });

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

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(8.0);

        ui.label(
            RichText::new("Pair Devices with a Code")
                .color(pal.text)
                .size(13.5)
                .strong(),
        );
        ui.label(
            RichText::new(
                "Generate a code here and type the exact same code on your other device, or enter a code from another device.",
            )
            .color(pal.text_muted)
            .size(12.0),
        );
        ui.add_space(6.0);

        ui.horizontal(|ui| {
            if secondary_button(ui, pal, "Create Pairing Code").clicked() {
                state.generated_code = generate_pairing_pin();
                state.pairing_code_input = state.generated_code.clone();
            }
            if !state.generated_code.is_empty() {
                ui.add_space(6.0);
                ui.label(
                    RichText::new(&state.generated_code)
                        .color(pal.accent)
                        .size(15.5)
                        .strong()
                        .monospace(),
                );
                if secondary_button(ui, pal, "Copy").clicked() {
                    ui.ctx().copy_text(state.generated_code.clone());
                }
            }
        });

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut state.pairing_code_input)
                    .hint_text("Enter shared code (6+ chars)")
                    .desired_width(210.0),
            );
            if primary_button(ui, pal, "Pair Devices").clicked() {
                backend.send(GuiCommand::ApplyPairingPin {
                    pin: state.pairing_code_input.clone(),
                });
            }
            if !snap.config.sharing.psk_hash.is_empty()
                && danger_button(ui, pal, "Reset Pairing").clicked()
            {
                state.generated_code.clear();
                state.pairing_code_input.clear();
                backend.send(GuiCommand::ClearPairing);
            }
        });
    });

    ui.add_space(10.0);

    // 3. Progressive Disclosure: Advanced Settings & Diagnostics (collapsed by default)
    collapsible_advanced(
        ui,
        pal,
        "settings_advanced_drawer",
        "Advanced Audio, Network & Diagnostics",
        |ui| {
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
            ui.horizontal(|ui| {
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
        },
    );
}
