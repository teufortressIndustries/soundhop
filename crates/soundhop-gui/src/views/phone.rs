//! Phone Audio ("Android Bridge") view: guided setup, USB/Wi-Fi phone cards,
//! 1-click "Go Wireless", plain-language options, and collapsible Advanced tuning.

use std::collections::HashSet;

use eframe::egui::{self, RichText};
use soundhop_core::state::KnownAndroidDevice;

use crate::backend::{GuiBackend, GuiCommand, GuiSnapshot};
use crate::theme::Palette;
use crate::widgets::{
    callout_card, card, collapsible_advanced, danger_button, pill_badge, primary_button,
    secondary_button, section_header, toggle_switch,
};

pub struct PhoneViewState {
    pub selected_serial: String,
    pub manual_ip: String,
    pub show_manual_wifi: bool,
    pub codec: String,
    pub source: String,
    pub buffer_ms: u32,
    pub duplicate: bool,
    pub as_mic: bool,
    initialized: bool,
}

impl Default for PhoneViewState {
    fn default() -> Self {
        Self {
            selected_serial: String::new(),
            manual_ip: String::new(),
            show_manual_wifi: false,
            codec: "opus".to_string(),
            source: "output".to_string(),
            buffer_ms: 50,
            duplicate: false,
            as_mic: false,
            initialized: false,
        }
    }
}

impl PhoneViewState {
    fn sync_initial(&mut self, snap: &GuiSnapshot) {
        if !self.initialized {
            self.codec = snap.config.android.default_codec.clone();
            self.source = snap.config.android.default_source.clone();
            self.buffer_ms = snap.config.android.buffer_ms;
            self.duplicate = snap.config.android.duplicate_audio;
            if let Some(first) = snap.android_devices.first() {
                self.selected_serial = first.serial.clone();
            }
            self.initialized = true;
        } else if self.selected_serial.is_empty() {
            if let Some(first) = snap.android_devices.first() {
                self.selected_serial = first.serial.clone();
            }
        }
    }
}

pub(crate) fn reconnectable_wifi_history(snap: &GuiSnapshot) -> Vec<(&KnownAndroidDevice, String)> {
    let mut out = Vec::new();
    let mut seen_targets = HashSet::new();
    for rec in &snap.android_history {
        let target = if !rec.last_ip.is_empty() {
            let port = if rec.last_port == 0 {
                5555
            } else {
                rec.last_port
            };
            format!("{}:{port}", rec.last_ip)
        } else if rec.is_wifi && rec.serial.contains(':') {
            rec.serial.clone()
        } else {
            continue;
        };

        let already_connected = snap.android_devices.iter().any(|dev| {
            dev.serial == rec.serial
                || dev.serial == target
                || (dev.is_wifi && !rec.last_ip.is_empty() && dev.ip == rec.last_ip)
        });
        if already_connected || !seen_targets.insert(target.clone()) {
            continue;
        }
        out.push((rec, target));
    }
    out
}

pub fn render(
    ui: &mut egui::Ui,
    pal: &Palette,
    snap: &GuiSnapshot,
    state: &mut PhoneViewState,
    backend: &GuiBackend,
) {
    state.sync_initial(snap);

    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            section_header(
                ui,
                pal,
                "Listen to your Android phone",
                Some(
                    "Stream phone music, videos, or mic audio to this computer over USB or Wi-Fi.",
                ),
            );
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_enabled_ui(!snap.refreshing_android, |ui| {
                let label = if snap.refreshing_android {
                    "Refreshing..."
                } else {
                    "Refresh"
                };
                if secondary_button(ui, pal, label).clicked() {
                    backend.send(GuiCommand::RefreshAndroid);
                }
            });
        });
    });

    ui.add_space(10.0);

    // 1. One-time helper setup (only shown when tools aren't ready)
    if !snap.android_tools.ready() {
        callout_card(ui, pal.accent_soft, pal.accent, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new("One-time helper setup needed")
                            .color(pal.text)
                            .size(14.5)
                            .strong(),
                    );
                    ui.label(
                        RichText::new(
                            "SoundHop uses the official scrcpy audio bridge. Click install to set it up automatically.",
                        )
                        .color(pal.text_secondary)
                        .size(12.0),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_enabled_ui(!snap.android_installing, |ui| {
                        let btn_text = if snap.android_installing {
                            "Installing..."
                        } else {
                            "Install Audio Helper"
                        };
                        if primary_button(ui, pal, btn_text).clicked() {
                            backend.send(GuiCommand::AndroidInstallTools);
                        }
                    });
                });
            });
        });
        ui.add_space(12.0);
    }

    // 2. Active session banner if currently streaming
    if snap.android_session_active {
        card(ui, pal, Some(pal.success), |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!(
                                "Listening to {}",
                                if snap.android_session_device.is_empty() {
                                    "Android Device"
                                } else {
                                    &snap.android_session_device
                                }
                            ))
                            .color(pal.text)
                            .size(15.5)
                            .strong(),
                        );
                        pill_badge(ui, "Live", pal.success, pal.success_soft);
                    });
                    ui.label(
                        RichText::new("Phone audio is playing through your active output.")
                            .color(pal.text_muted)
                            .size(12.0),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if danger_button(ui, pal, "Stop Listening").clicked() {
                        backend.send(GuiCommand::AndroidStop);
                    }
                });
            });
        });
        ui.add_space(12.0);
    }

    // 3. Connected Phone Cards or Friendly Guide
    if snap.android_devices.is_empty() {
        card(ui, pal, None, |ui| {
            ui.label(
                RichText::new("No phone connected yet")
                    .color(pal.text)
                    .size(15.0)
                    .strong(),
            );
            ui.add_space(6.0);
            for step in [
                "1. Plug your phone in with a USB cable (or connect over Wi-Fi below).",
                "2. Turn on USB Debugging in your phone's Developer Options and tap Allow.",
                "3. Click Refresh above.",
            ] {
                ui.label(RichText::new(step).color(pal.text_secondary).size(12.5));
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.add_enabled_ui(!snap.refreshing_android, |ui| {
                    let label = if snap.refreshing_android {
                        "Refreshing..."
                    } else {
                        "Refresh Devices"
                    };
                    if secondary_button(ui, pal, label).clicked() {
                        backend.send(GuiCommand::RefreshAndroid);
                    }
                });
                let wifi_btn = if state.show_manual_wifi {
                    "Hide Wi-Fi IP"
                } else {
                    "Connect via Wi-Fi IP"
                };
                if secondary_button(ui, pal, wifi_btn).clicked() {
                    state.show_manual_wifi = !state.show_manual_wifi;
                }
            });

            let wifi_empty_t = ui.ctx().animate_bool_with_time_and_easing(
                egui::Id::new("phone_manual_wifi_empty_reveal"),
                state.show_manual_wifi,
                0.14,
                egui::emath::easing::cubic_out,
            );
            if state.show_manual_wifi || wifi_empty_t > 0.02 {
                ui.scope(|ui| {
                    ui.set_opacity(wifi_empty_t);
                    ui.add_space(10.0 * wifi_empty_t);
                    ui.separator();
                    ui.add_space(8.0 * wifi_empty_t);
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("Phone Wi-Fi IP:")
                                .color(pal.text_secondary)
                                .size(12.5),
                        );
                        ui.add(
                            egui::TextEdit::singleline(&mut state.manual_ip)
                                .hint_text("e.g. 192.168.1.99")
                                .desired_width(190.0),
                        );
                        if primary_button(ui, pal, "Connect").clicked()
                            && !state.manual_ip.trim().is_empty()
                        {
                            backend.send(GuiCommand::AndroidConnectWifi {
                                ip_port: state.manual_ip.clone(),
                            });
                        }
                    });
                });
            }
        });
    } else {
        for dev in &snap.android_devices {
            let selected = state.selected_serial == dev.serial;
            let border = if selected { Some(pal.accent) } else { None };
            card(ui, pal, border, |ui| {
                ui.horizontal(|ui| {
                    let mut is_sel = selected;
                    if ui.radio(is_sel, "").clicked() {
                        is_sel = true;
                    }
                    if is_sel {
                        state.selected_serial = dev.serial.clone();
                    }

                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(dev.display_name())
                                    .color(pal.text)
                                    .size(15.0)
                                    .strong(),
                            );
                            if dev.is_wifi {
                                pill_badge(ui, "Wi-Fi", pal.accent, pal.accent_soft);
                            } else {
                                pill_badge(ui, "USB", pal.success, pal.success_soft);
                            }
                            if !dev.is_ready() {
                                pill_badge(ui, "Tap Allow on Phone", pal.warning, pal.warning_soft);
                            }
                        });
                        let subtitle = if dev.is_ready() {
                            if dev.android_version.is_empty() {
                                "Ready for audio streaming".to_string()
                            } else {
                                format!("Android {} • Ready", dev.android_version)
                            }
                        } else {
                            "Check your phone screen to authorize USB debugging".to_string()
                        };
                        ui.label(RichText::new(subtitle).color(pal.text_muted).size(12.0));
                    });

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if !snap.android_session_active && dev.is_ready() {
                            let can_start = snap.android_tools.ready() && !snap.android_starting;
                            ui.add_enabled_ui(can_start, |ui| {
                                let label = if snap.android_starting {
                                    "Starting..."
                                } else {
                                    "Start Listening"
                                };
                                if primary_button(ui, pal, label).clicked() {
                                    state.selected_serial = dev.serial.clone();
                                    backend.send(GuiCommand::AndroidStart {
                                        serial: dev.serial.clone(),
                                        device_name: dev.display_name(),
                                        codec: state.codec.clone(),
                                        source: state.source.clone(),
                                        buffer_ms: state.buffer_ms,
                                        duplicate: state.duplicate,
                                        as_mic: state.as_mic,
                                    });
                                }
                            });
                        }

                        if !dev.is_wifi && dev.is_ready() {
                            if secondary_button(ui, pal, "Go Wireless").clicked() {
                                backend.send(GuiCommand::AndroidSwitchWifi {
                                    serial: dev.serial.clone(),
                                });
                            }
                        } else if dev.is_wifi
                            && secondary_button(ui, pal, "Disconnect Wi-Fi").clicked()
                        {
                            backend.send(GuiCommand::AndroidDisconnectWifi {
                                serial: dev.serial.clone(),
                            });
                        }
                    });
                });
            });
            ui.add_space(6.0);
        }

        // Subtle Wi-Fi IP connect option even when a device is already shown
        ui.horizontal(|ui| {
            let link_text = if state.show_manual_wifi {
                "Hide manual Wi-Fi IP"
            } else {
                "+ Connect another phone by Wi-Fi IP"
            };
            if ui
                .link(RichText::new(link_text).color(pal.accent))
                .clicked()
            {
                state.show_manual_wifi = !state.show_manual_wifi;
            }
        });
        let wifi_dev_t = ui.ctx().animate_bool_with_time_and_easing(
            egui::Id::new("phone_manual_wifi_list_reveal"),
            state.show_manual_wifi,
            0.14,
            egui::emath::easing::cubic_out,
        );
        if state.show_manual_wifi || wifi_dev_t > 0.02 {
            ui.scope(|ui| {
                ui.set_opacity(wifi_dev_t);
                ui.add_space(4.0 * wifi_dev_t);
                card(ui, pal, None, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Phone Wi-Fi IP:").color(pal.text_secondary));
                        ui.add(
                            egui::TextEdit::singleline(&mut state.manual_ip)
                                .hint_text("192.168.1.99")
                                .desired_width(190.0),
                        );
                        if secondary_button(ui, pal, "Connect").clicked()
                            && !state.manual_ip.trim().is_empty()
                        {
                            backend.send(GuiCommand::AndroidConnectWifi {
                                ip_port: state.manual_ip.clone(),
                            });
                        }
                    });
                });
            });
        }
    }

    let wifi_history = reconnectable_wifi_history(snap);
    if !wifi_history.is_empty() {
        ui.add_space(12.0);
        ui.label(
            RichText::new("Previously Connected (Wi-Fi)")
                .color(pal.text_secondary)
                .size(13.0)
                .strong(),
        );
        ui.add_space(6.0);
        for (rec, ip_port) in wifi_history {
            card(ui, pal, None, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(rec.display_name())
                                    .color(pal.text)
                                    .size(14.5)
                                    .strong(),
                            );
                            pill_badge(ui, "Saved Wi-Fi", pal.accent, pal.accent_soft);
                        });
                        ui.label(
                            RichText::new(format!("Last address: {ip_port}"))
                                .color(pal.text_muted)
                                .size(12.0),
                        );
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if primary_button(ui, pal, "Reconnect").clicked() {
                            backend.send(GuiCommand::AndroidConnectWifi { ip_port });
                        }
                    });
                });
            });
            ui.add_space(6.0);
        }
    }

    ui.add_space(12.0);

    // 4. Simple Listening Options
    card(ui, pal, None, |ui| {
        ui.label(
            RichText::new("Listening Options")
                .color(pal.text)
                .size(14.5)
                .strong(),
        );
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    RichText::new("Keep playing on phone speakers too")
                        .color(pal.text)
                        .size(13.5),
                );
                ui.label(
                    RichText::new("Hear audio on both your phone and computer at the same time.")
                        .color(pal.text_muted)
                        .size(12.0),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if toggle_switch(ui, pal, &mut state.duplicate).changed() {
                    backend.send(GuiCommand::SaveConfigPatch {
                        patch: serde_json::json!({
                            "android": { "duplicate_audio": state.duplicate }
                        }),
                        toast: None,
                    });
                }
            });
        });

        ui.add_space(10.0);
        #[cfg(target_os = "windows")]
        {
            state.as_mic = false;
            let row_resp = ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("Use phone as a PC microphone")
                                .color(pal.text_muted)
                                .size(13.5),
                        );
                        pill_badge(ui, "Linux only", pal.text_muted, pal.card_elevated);
                    });
                    ui.label(
                        RichText::new(
                            "Virtual microphone routing requires PulseAudio/PipeWire on Linux.",
                        )
                        .color(pal.text_muted)
                        .size(12.0),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_enabled_ui(false, |ui| {
                        let mut disabled_val = false;
                        toggle_switch(ui, pal, &mut disabled_val);
                    });
                });
            });
            row_resp.response.on_hover_text(
                "Creating a virtual PC microphone is supported on Linux (PulseAudio/PipeWire). On Windows, phone audio plays through your speakers.",
            );
        }
        #[cfg(not(target_os = "windows"))]
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    RichText::new("Use phone as a PC microphone")
                        .color(pal.text)
                        .size(13.5),
                );
                ui.label(
                    RichText::new("Creates a virtual mic you can select in Discord, Zoom, or OBS.")
                        .color(pal.text_muted)
                        .size(12.0),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                toggle_switch(ui, pal, &mut state.as_mic);
            });
        });
    });

    ui.add_space(10.0);

    // 5. Progressive Disclosure: Audio Quality & Latency (Advanced)
    collapsible_advanced(
        ui,
        pal,
        "phone_advanced_drawer",
        "Audio Quality & Latency (Advanced)",
        |ui| {
            egui::Grid::new("phone_adv_grid")
                .num_columns(2)
                .spacing([18.0, 10.0])
                .show(ui, |ui| {
                    ui.label(RichText::new("Capture From").color(pal.text_secondary));
                    let prev_source = state.source.clone();
                    let source_label = match state.source.as_str() {
                        "mic" => "Phone Microphone",
                        "playback" => "App Playback",
                        _ => "All Phone Audio",
                    };
                    egui::ComboBox::from_id_salt("phone_source_combo")
                        .selected_text(source_label)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut state.source,
                                "output".to_string(),
                                "All Phone Audio (Android 11+)",
                            );
                            ui.selectable_value(
                                &mut state.source,
                                "playback".to_string(),
                                "App Playback",
                            );
                            ui.selectable_value(
                                &mut state.source,
                                "mic".to_string(),
                                "Phone Microphone",
                            );
                        });
                    if state.source != prev_source {
                        backend.send(GuiCommand::SaveConfigPatch {
                            patch: serde_json::json!({
                                "android": { "default_source": state.source }
                            }),
                            toast: None,
                        });
                    }
                    ui.end_row();

                    ui.label(RichText::new("Audio Format").color(pal.text_secondary));
                    let prev_codec = state.codec.clone();
                    let codec_label = match state.codec.as_str() {
                        "aac" => "AAC",
                        "flac" => "FLAC (Lossless)",
                        "raw" => "Uncompressed PCM",
                        _ => "Opus (Recommended)",
                    };
                    egui::ComboBox::from_id_salt("phone_codec_combo")
                        .selected_text(codec_label)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut state.codec,
                                "opus".to_string(),
                                "Opus (Recommended)",
                            );
                            ui.selectable_value(&mut state.codec, "aac".to_string(), "AAC");
                            ui.selectable_value(
                                &mut state.codec,
                                "flac".to_string(),
                                "FLAC (Lossless)",
                            );
                            ui.selectable_value(
                                &mut state.codec,
                                "raw".to_string(),
                                "Uncompressed PCM",
                            );
                        });
                    if state.codec != prev_codec {
                        backend.send(GuiCommand::SaveConfigPatch {
                            patch: serde_json::json!({
                                "android": { "default_codec": state.codec }
                            }),
                            toast: None,
                        });
                    }
                    ui.end_row();

                    ui.label(RichText::new("Audio Buffer").color(pal.text_secondary));
                    let buf_slider =
                        ui.add(egui::Slider::new(&mut state.buffer_ms, 15..=300).suffix(" ms"));
                    if buf_slider.drag_stopped() || (buf_slider.changed() && !buf_slider.dragged())
                    {
                        backend.send(GuiCommand::SaveConfigPatch {
                            patch: serde_json::json!({
                                "android": { "buffer_ms": state.buffer_ms }
                            }),
                            toast: None,
                        });
                    }
                    ui.end_row();
                });

            if let Some(ref p) = snap.android_tools.scrcpy_path {
                ui.add_space(6.0);
                ui.label(
                    RichText::new(format!(
                        "Helper engine: {} ({})",
                        p.display(),
                        if snap.android_tools.scrcpy_version.is_empty() {
                            "ready"
                        } else {
                            &snap.android_tools.scrcpy_version
                        }
                    ))
                    .color(pal.text_muted)
                    .size(11.5),
                );
            }
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use soundhop_core::android::AdbDevice;
    use soundhop_core::backend::linux::LinuxAudioBackend;
    use soundhop_core::config::Config;
    use soundhop_core::controller::Controller;
    use soundhop_core::state::State;
    use soundhop_core::util::{CommandResult, MockRunner};
    use std::sync::Arc;

    fn test_snapshot(cfg: Config, st: State) -> GuiSnapshot {
        let runner = Arc::new(MockRunner::new(|args, _| {
            Ok(CommandResult {
                args: args.to_vec(),
                returncode: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }));
        let backend = Arc::new(LinuxAudioBackend::with_runner(runner));
        let ctrl = Controller::new(Some(cfg), Some(st), Some(backend), None, None, false);
        ctrl.set_save_enabled(false);
        GuiSnapshot::from_controller(&ctrl)
    }

    #[test]
    fn test_reconnectable_wifi_history_filters_connected_and_usb_only() {
        let mut st = State::default();
        st.known_android_devices.insert(
            "192.168.1.99:5555".to_string(),
            KnownAndroidDevice {
                serial: "192.168.1.99:5555".to_string(),
                model: "Pixel 8".to_string(),
                manufacturer: "Google".to_string(),
                last_ip: "192.168.1.99".to_string(),
                last_port: 5555,
                last_seen: 200.0,
                last_connected: 200.0,
                is_wifi: true,
                auto_connect: false,
            },
        );
        st.known_android_devices.insert(
            "192.168.1.88:5555".to_string(),
            KnownAndroidDevice {
                serial: "192.168.1.88:5555".to_string(),
                model: "Galaxy S24".to_string(),
                manufacturer: "Samsung".to_string(),
                last_ip: "192.168.1.88".to_string(),
                last_port: 5555,
                last_seen: 100.0,
                last_connected: 100.0,
                is_wifi: true,
                auto_connect: false,
            },
        );
        st.known_android_devices.insert(
            "USB123".to_string(),
            KnownAndroidDevice {
                serial: "USB123".to_string(),
                model: "USB Only".to_string(),
                manufacturer: "Acme".to_string(),
                last_ip: String::new(),
                last_port: 5555,
                last_seen: 300.0,
                last_connected: 300.0,
                is_wifi: false,
                auto_connect: false,
            },
        );

        let mut snap = test_snapshot(Config::default(), st);
        snap.android_devices = vec![AdbDevice {
            serial: "192.168.1.88:5555".to_string(),
            state: "device".to_string(),
            model: "Galaxy S24".to_string(),
            product: "e1q".to_string(),
            device: "e1q".to_string(),
            transport_id: "1".to_string(),
            manufacturer: "Samsung".to_string(),
            android_version: "14".to_string(),
            sdk_version: 34,
            is_wifi: true,
            ip: "192.168.1.88".to_string(),
            port: 5555,
        }];

        let history = reconnectable_wifi_history(&snap);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].0.display_name(), "Google Pixel 8");
        assert_eq!(history[0].1, "192.168.1.99:5555");
    }
}
