//! Android Audio & Wi-Fi ADB Setup Wizard built with pure-Rust `eframe` / `egui`.

use std::sync::{Arc, Mutex};
use std::thread;

use eframe::egui;
use soundhop_core::android::{AdbDevice, ToolStatus};
use soundhop_core::controller::Controller;

pub fn open_android_wizard(controller: Arc<Controller>) -> Result<(), String> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([660.0, 620.0])
            .with_min_inner_size([540.0, 480.0])
            .with_title("SoundHop v2.0 — Android Audio & Wi-Fi ADB Wizard"),
        ..Default::default()
    };

    let app = AndroidWizardApp::new(controller);
    eframe::run_native(
        "SoundHop Android Audio Wizard",
        options,
        Box::new(|_cc| Ok(Box::new(app))),
    )
    .map_err(|e| format!("Failed to open Android Wizard window: {e}"))
}

struct AndroidWizardApp {
    controller: Arc<Controller>,
    tool_status: ToolStatus,
    devices: Vec<AdbDevice>,
    selected_serial: String,
    manual_ip: String,
    codec: String,
    source: String,
    bit_rate: u32,
    buffer_ms: u32,
    as_mic: bool,
    duplicate: bool,
    status_msg: Arc<Mutex<Option<(String, bool)>>>,
    installing: Arc<Mutex<bool>>,
}

impl AndroidWizardApp {
    fn new(controller: Arc<Controller>) -> Self {
        let tool_status = controller.android_manager.get_tool_status();
        let devices = controller.android_manager.list_devices();
        let selected_serial = devices
            .first()
            .map(|d| d.serial.clone())
            .unwrap_or_default();
        Self {
            controller,
            tool_status,
            devices,
            selected_serial,
            manual_ip: String::new(),
            codec: "opus".to_string(),
            source: "output".to_string(),
            bit_rate: 128_000,
            buffer_ms: 50,
            as_mic: false,
            duplicate: false,
            status_msg: Arc::new(Mutex::new(None)),
            installing: Arc::new(Mutex::new(false)),
        }
    }

    fn refresh_devices(&mut self) {
        self.tool_status = self.controller.android_manager.get_tool_status();
        self.devices = self.controller.android_manager.list_devices();
        if self.selected_serial.is_empty() {
            if let Some(first) = self.devices.first() {
                self.selected_serial = first.serial.clone();
            }
        }
    }

    fn set_status(&self, msg: impl Into<String>, ok: bool) {
        *self.status_msg.lock().unwrap() = Some((msg.into(), ok));
    }
}

impl eframe::App for AndroidWizardApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint_after(std::time::Duration::from_millis(250));

        egui::TopBottomPanel::top("android_top").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading("Android Audio & Wireless ADB Bridge");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Refresh Devices").clicked() {
                        self.refresh_devices();
                    }
                });
            });
            ui.add_space(4.0);
        });

        egui::TopBottomPanel::bottom("android_bottom").show(ctx, |ui| {
            ui.add_space(6.0);
            let status_clone = self.status_msg.lock().unwrap().clone();
            if let Some((msg, ok)) = status_clone {
                let color = if ok {
                    egui::Color32::from_rgb(60, 190, 90)
                } else {
                    egui::Color32::from_rgb(220, 80, 80)
                };
                ui.colored_label(color, msg);
                ui.add_space(4.0);
            }
            ui.horizontal(|ui| {
                let is_running = self.controller.android_session.is_active();
                if !is_running {
                    if ui
                        .add_enabled(
                            self.tool_status.ready() && !self.selected_serial.is_empty(),
                            egui::Button::new("Start Android Audio Capture"),
                        )
                        .clicked()
                    {
                        let dev_name = self
                            .devices
                            .iter()
                            .find(|d| d.serial == self.selected_serial)
                            .map(|d| d.display_name())
                            .unwrap_or_else(|| self.selected_serial.clone());
                        match self.controller.android_session.start(
                            &self.selected_serial,
                            &dev_name,
                            Some(&self.codec),
                            Some(&self.source),
                            self.as_mic,
                            Some(self.buffer_ms),
                            Some(self.bit_rate),
                            self.duplicate,
                        ) {
                            Ok(_) => {
                                self.set_status(
                                    format!("Android audio capture started for {dev_name}."),
                                    true,
                                );
                            }
                            Err(e) => {
                                self.set_status(format!("Failed to start session: {e}"), false);
                            }
                        }
                    }
                } else if ui.button("Stop Android Audio Capture").clicked() {
                    let _ = self.controller.android_session.stop();
                    self.set_status("Android audio capture stopped.", true);
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Close").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
            });
            ui.add_space(4.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.group(|ui| {
                    ui.heading("1. scrcpy & ADB Engine");
                    ui.label(format!(
                        "scrcpy: {} (version: {}) | adb: {} (version: {})",
                        if self.tool_status.scrcpy_path.is_some() {
                            "Found"
                        } else {
                            "Missing"
                        },
                        if self.tool_status.scrcpy_version.is_empty() {
                            "N/A"
                        } else {
                            &self.tool_status.scrcpy_version
                        },
                        if self.tool_status.adb_path.is_some() {
                            "Found"
                        } else {
                            "Missing"
                        },
                        if self.tool_status.adb_version.is_empty() {
                            "N/A"
                        } else {
                            &self.tool_status.adb_version
                        }
                    ));

                    let is_installing = *self.installing.lock().unwrap();
                    if !self.tool_status.ready() {
                        ui.add_space(4.0);
                        if ui
                            .add_enabled(
                                !is_installing,
                                egui::Button::new(if is_installing {
                                    "Downloading scrcpy + adb..."
                                } else {
                                    "Download & Install Managed scrcpy + adb"
                                }),
                            )
                            .clicked()
                        {
                            *self.installing.lock().unwrap() = true;
                            let status_arc = Arc::clone(&self.status_msg);
                            let installing_arc = Arc::clone(&self.installing);
                            let mgr = Arc::clone(&self.controller.android_manager);
                            thread::spawn(move || {
                                match mgr.install_scrcpy(None, None) {
                                    Ok(path) => {
                                        *status_arc.lock().unwrap() = Some((
                                            format!("Installed scrcpy to {}", path.display()),
                                            true,
                                        ));
                                    }
                                    Err(e) => {
                                        *status_arc.lock().unwrap() =
                                            Some((format!("Install failed: {e}"), false));
                                    }
                                }
                                *installing_arc.lock().unwrap() = false;
                            });
                        }
                    }
                });

                ui.add_space(10.0);

                ui.group(|ui| {
                    ui.heading("2. Connected Android Devices (USB & Wi-Fi)");
                    if self.devices.is_empty() {
                        ui.label(
                            "No ADB devices detected. Connect via USB with USB Debugging enabled, or connect over Wi-Fi below.",
                        );
                    } else {
                        for dev in &self.devices.clone() {
                            let transport = if dev.is_wifi { "Wi-Fi" } else { "USB" };
                            ui.horizontal(|ui| {
                                ui.radio_value(
                                    &mut self.selected_serial,
                                    dev.serial.clone(),
                                    format!(
                                        "{} ({}) — [{}]",
                                        dev.display_name(),
                                        transport,
                                        dev.state
                                    ),
                                );
                                if !dev.is_wifi && dev.is_ready() {
                                    if ui.small_button("Switch to Wi-Fi").clicked() {
                                        if let Some(ip) = self
                                            .controller
                                            .android_manager
                                            .get_device_wifi_ip(&dev.serial)
                                        {
                                            let (_, msg1) = self
                                                .controller
                                                .android_manager
                                                .enable_tcpip(&dev.serial, 5555);
                                            let (ok2, msg2) = self
                                                .controller
                                                .android_manager
                                                .connect_wifi(&ip, 5555);
                                            self.set_status(format!("{msg1} | {msg2}"), ok2);
                                            self.refresh_devices();
                                        } else {
                                            self.set_status(
                                                "Could not detect Wi-Fi IP on phone.",
                                                false,
                                            );
                                        }
                                    }
                                } else if dev.is_wifi
                                    && ui.small_button("Disconnect Wi-Fi").clicked()
                                {
                                    let (ok, msg) = self
                                        .controller
                                        .android_manager
                                        .disconnect_wifi(Some(&dev.serial));
                                    self.set_status(msg, ok);
                                    self.refresh_devices();
                                }
                            });
                        }
                    }

                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label("Manual Wi-Fi IP[:Port]:");
                        ui.text_edit_singleline(&mut self.manual_ip);
                        if ui.button("Connect over Wi-Fi").clicked()
                            && !self.manual_ip.trim().is_empty()
                        {
                            let (ok, msg) = self
                                .controller
                                .android_manager
                                .connect_wifi(self.manual_ip.trim(), 5555);
                            self.set_status(msg, ok);
                            self.refresh_devices();
                        }
                    });
                });

                ui.add_space(10.0);

                ui.group(|ui| {
                    ui.heading("3. Audio Stream & Virtual Microphone Options");
                    egui::Grid::new("android_opts_grid")
                        .num_columns(2)
                        .spacing([16.0, 8.0])
                        .show(ui, |ui| {
                            ui.label("Audio Source:");
                            egui::ComboBox::from_id_salt("android_source")
                                .selected_text(&self.source)
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut self.source,
                                        "output".to_string(),
                                        "output (Device Media Playback - Android 11+)",
                                    );
                                    ui.selectable_value(
                                        &mut self.source,
                                        "mic".to_string(),
                                        "mic (Phone Microphone)",
                                    );
                                    ui.selectable_value(
                                        &mut self.source,
                                        "playback".to_string(),
                                        "playback (App Playback Capture)",
                                    );
                                });
                            ui.end_row();

                            ui.label("Audio Codec:");
                            egui::ComboBox::from_id_salt("android_codec")
                                .selected_text(&self.codec)
                                .show_ui(ui, |ui| {
                                    for c in ["opus", "aac", "flac", "raw"] {
                                        ui.selectable_value(&mut self.codec, c.to_string(), c);
                                    }
                                });
                            ui.end_row();

                            ui.label("Buffer (ms):");
                            ui.add(
                                egui::Slider::new(&mut self.buffer_ms, 10..=300).suffix(" ms"),
                            );
                            ui.end_row();

                            ui.label("Duplicate on Phone:");
                            ui.checkbox(
                                &mut self.duplicate,
                                "Keep playing audio on phone speakers (--audio-dup)",
                            );
                            ui.end_row();

                            ui.label("Virtual Microphone:");
                            ui.checkbox(
                                &mut self.as_mic,
                                "Route Android audio into a virtual PC microphone (Discord / Zoom / OBS)",
                            );
                            ui.end_row();
                        });
                });
            });
        });
    }
}
