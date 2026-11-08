//! Settings & Routing Rules Dialog built with pure-Rust `eframe` / `egui`.

use std::sync::Arc;

use eframe::egui;
use soundhop_core::config::Config;
use soundhop_core::controller::Controller;
use soundhop_core::security::{
    derive_key_hex, derive_pa_cookie, generate_pairing_pin, write_pa_cookie,
};
use soundhop_core::service::{install, status, uninstall};
use soundhop_core::telemetry::{AudioTelemetryMonitor, TelemetryTracker};

pub fn open_settings_dialog(
    controller: Arc<Controller>,
    telemetry: Option<Arc<TelemetryTracker>>,
) -> Result<(), String> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([620.0, 680.0])
            .with_min_inner_size([520.0, 500.0])
            .with_title("SoundHop v2.0 Settings"),
        ..Default::default()
    };

    let app = SettingsApp::new(controller, telemetry);
    eframe::run_native(
        "SoundHop Settings",
        options,
        Box::new(|_cc| Ok(Box::new(app))),
    )
    .map_err(|e| format!("Failed to open Settings window: {e}"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsTab {
    GeneralAndSharing,
    ConnectionAndMultiRoom,
    AppRoutingRules,
    SecurityAndPairing,
    DiscoveryAndService,
}

struct SettingsApp {
    controller: Arc<Controller>,
    telemetry: Option<Arc<TelemetryTracker>>,
    _telemetry_monitor: Option<AudioTelemetryMonitor>,
    draft: Config,
    active_tab: SettingsTab,
    new_remote_app: String,
    new_local_app: String,
    pairing_pin_input: String,
    generated_pin: String,
    status_banner: Option<(String, bool)>,
    service_running: bool,
    service_enabled: bool,
    autostart_installed: bool,
}

impl SettingsApp {
    fn new(controller: Arc<Controller>, telemetry: Option<Arc<TelemetryTracker>>) -> Self {
        let draft = controller.config.lock().unwrap().clone();
        let svc = status();
        let service_running = svc.get("active").and_then(|v| v.as_bool()).unwrap_or(false);
        let service_enabled = svc
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let autostart_installed = svc
            .get("tray_autostart")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let _telemetry_monitor = if controller.is_save_enabled() {
            Some(controller.start_telemetry_monitor())
        } else {
            None
        };

        Self {
            controller,
            telemetry,
            _telemetry_monitor,
            draft,
            active_tab: SettingsTab::GeneralAndSharing,
            new_remote_app: String::new(),
            new_local_app: String::new(),
            pairing_pin_input: String::new(),
            generated_pin: String::new(),
            status_banner: None,
            service_running,
            service_enabled,
            autostart_installed,
        }
    }

    fn refresh_service_state(&mut self) {
        let svc = status();
        self.service_running = svc.get("active").and_then(|v| v.as_bool()).unwrap_or(false);
        self.service_enabled = svc
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        self.autostart_installed = svc
            .get("tray_autostart")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
    }

    fn save_changes(&mut self) {
        if let Err(e) = self.draft.normalize() {
            self.status_banner = Some((format!("Validation error: {e}"), false));
            return;
        }
        *self.controller.config.lock().unwrap() = self.draft.clone();
        match self.draft.save(None) {
            Ok(_) => {
                let _ = self.controller.enforce_routed_apps();
                self.status_banner = Some(("Settings saved and applied.".to_string(), true));
            }
            Err(e) => {
                self.status_banner = Some((format!("Failed to save config: {e}"), false));
            }
        }
    }
}

impl eframe::App for SettingsApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint_after(std::time::Duration::from_millis(66));

        egui::TopBottomPanel::top("settings_top_bar").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading("SoundHop v2.0 — Settings & Audio Telemetry");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(ref telem) = self.telemetry {
                        let snap = telem.snapshot();
                        let first_peer = snap.peers.values().next();
                        let rtt_txt = first_peer
                            .map(|p| format!("RTT: {:.1} ms", p.last_rtt_ms))
                            .unwrap_or_else(|| "RTT: --".to_string());
                        let jit_txt = first_peer
                            .map(|p| format!("Jitter: {:.1} ms", p.jitter_ms))
                            .unwrap_or_else(|| "Jitter: --".to_string());
                        ui.label(
                            egui::RichText::new(format!(
                                "{} | {} | {:.1} dBFS",
                                rtt_txt, jit_txt, snap.audio.dbfs
                            ))
                            .monospace(),
                        );
                    }
                });
            });

            if let Some(ref telem) = self.telemetry {
                let snap = telem.snapshot();
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label("Live Level:");
                    let norm = ((snap.audio.dbfs + 60.0) / 60.0).clamp(0.0, 1.0);
                    let peak_max = snap.audio.peak_left.max(snap.audio.peak_right);
                    ui.add(
                        egui::ProgressBar::new(norm)
                            .desired_width(220.0)
                            .text(format!("Peak {:.0}%", peak_max * 100.0)),
                    );
                });
            }

            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                ui.selectable_value(
                    &mut self.active_tab,
                    SettingsTab::GeneralAndSharing,
                    "General & Sharing",
                );
                ui.selectable_value(
                    &mut self.active_tab,
                    SettingsTab::ConnectionAndMultiRoom,
                    "Connection & Multi-Room",
                );
                ui.selectable_value(
                    &mut self.active_tab,
                    SettingsTab::AppRoutingRules,
                    "App Routing Rules",
                );
                ui.selectable_value(
                    &mut self.active_tab,
                    SettingsTab::SecurityAndPairing,
                    "PIN / PSK Security",
                );
                ui.selectable_value(
                    &mut self.active_tab,
                    SettingsTab::DiscoveryAndService,
                    "Discovery & Service",
                );
            });
            ui.add_space(4.0);
        });

        egui::TopBottomPanel::bottom("settings_bottom_bar").show(ctx, |ui| {
            ui.add_space(6.0);
            if let Some((ref msg, ok)) = self.status_banner {
                let color = if ok {
                    egui::Color32::from_rgb(60, 190, 90)
                } else {
                    egui::Color32::from_rgb(220, 80, 80)
                };
                ui.colored_label(color, msg);
                ui.add_space(4.0);
            }
            ui.horizontal(|ui| {
                if ui.button("Save & Apply").clicked() {
                    self.save_changes();
                }
                if ui.button("Reload from Disk").clicked() {
                    if let Ok(reloaded) = self.controller.reload_config() {
                        self.draft = reloaded;
                    }
                    self.status_banner =
                        Some(("Reloaded current configuration.".to_string(), true));
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
            egui::ScrollArea::vertical().show(ui, |ui| match self.active_tab {
                SettingsTab::GeneralAndSharing => self.render_general_tab(ui),
                SettingsTab::ConnectionAndMultiRoom => self.render_connection_tab(ui),
                SettingsTab::AppRoutingRules => self.render_routing_tab(ui),
                SettingsTab::SecurityAndPairing => self.render_security_tab(ui),
                SettingsTab::DiscoveryAndService => self.render_discovery_tab(ui),
            });
        });
    }
}

impl SettingsApp {
    fn render_general_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("Device Identity & Notifications");
        ui.add_space(6.0);
        egui::Grid::new("general_grid")
            .num_columns(2)
            .spacing([16.0, 10.0])
            .show(ui, |ui| {
                ui.label("Device Name:");
                ui.text_edit_singleline(&mut self.draft.device.name);
                ui.end_row();

                ui.label("Desktop Notifications:");
                ui.checkbox(&mut self.draft.ui.notifications, "Show toast alerts");
                ui.end_row();

                ui.label("Start System Tray:");
                ui.checkbox(
                    &mut self.draft.ui.start_tray,
                    "Launch system tray icon automatically",
                );
                ui.end_row();
            });

        ui.add_space(16.0);
        ui.separator();
        ui.heading("Speaker Sharing (TCP Server)");
        ui.add_space(6.0);
        egui::Grid::new("sharing_grid")
            .num_columns(2)
            .spacing([16.0, 10.0])
            .show(ui, |ui| {
                ui.label("Share Speakers on Startup:");
                ui.checkbox(
                    &mut self.draft.sharing.enabled,
                    "Allow LAN peers to stream audio to this device",
                );
                ui.end_row();

                ui.label("TCP Listen Port:");
                let mut port_val = self.draft.sharing.port as u32;
                if ui
                    .add(egui::DragValue::new(&mut port_val).range(1024..=65535))
                    .changed()
                {
                    self.draft.sharing.port = port_val as u16;
                }
                ui.end_row();

                ui.label("Security Mode:");
                egui::ComboBox::from_id_salt("sharing_sec_mode")
                    .selected_text(&self.draft.sharing.security)
                    .show_ui(ui, |ui| {
                        for mode in ["trusted-lan", "cookie", "psk", "open"] {
                            ui.selectable_value(
                                &mut self.draft.sharing.security,
                                mode.to_string(),
                                mode,
                            );
                        }
                    });
                ui.end_row();
            });
    }

    fn render_connection_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("Connection & Multi-Room Broadcast");
        ui.add_space(6.0);
        egui::Grid::new("connection_grid")
            .num_columns(2)
            .spacing([16.0, 10.0])
            .show(ui, |ui| {
                ui.label("Auto-Connect:");
                ui.checkbox(
                    &mut self.draft.connection.auto_connect,
                    "Automatically reconnect when preferred peer is discovered",
                );
                ui.end_row();

                ui.label("Preferred Peer ID / Name:");
                ui.text_edit_singleline(&mut self.draft.connection.last_peer);
                ui.end_row();

                ui.label("Switch Default Sink:");
                ui.checkbox(
                    &mut self.draft.connection.switch_default,
                    "Set remote tunnel or multi-room sink as default output",
                );
                ui.end_row();

                ui.label("Move Active Streams:");
                ui.checkbox(
                    &mut self.draft.connection.move_streams,
                    "Immediately move playing audio streams upon connect",
                );
                ui.end_row();

                ui.label("Keep Local Output:");
                ui.checkbox(
                    &mut self.draft.connection.keep_local_output,
                    "Play on local speakers simultaneously with remote speakers",
                );
                ui.end_row();

                ui.label("Buffer Latency (ms):");
                let mut lat = self.draft.connection.latency_ms.unwrap_or(150);
                if ui
                    .add(egui::Slider::new(&mut lat, 10..=2000).suffix(" ms"))
                    .changed()
                {
                    self.draft.connection.latency_ms = Some(lat);
                }
                ui.end_row();
            });
    }

    fn render_routing_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("Persistent Per-App Routing Rules");
        ui.label(
            "Configure how individual applications are routed whenever SoundHop connects to a remote speaker or multi-room group.",
        );
        ui.add_space(8.0);

        ui.horizontal(|ui| {
            ui.label("Default Routing Mode:");
            egui::ComboBox::from_id_salt("default_routing_mode")
                .selected_text(&self.draft.routing.default_mode)
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut self.draft.routing.default_mode,
                        "all".to_string(),
                        "all (Route all apps except Always-Local)",
                    );
                    ui.selectable_value(
                        &mut self.draft.routing.default_mode,
                        "selective".to_string(),
                        "selective (Only route Always-Remote apps)",
                    );
                });
        });

        ui.add_space(12.0);
        ui.separator();
        ui.subheading("Always Route to Remote Speaker (`always_remote`)");
        ui.horizontal(|ui| {
            ui.text_edit_singleline(&mut self.new_remote_app);
            if ui.button("Add App").clicked() && !self.new_remote_app.trim().is_empty() {
                let app = self.new_remote_app.trim().to_lowercase();
                if !self.draft.routing.always_remote.contains(&app) {
                    self.draft.routing.always_remote.push(app);
                }
                self.new_remote_app.clear();
            }
        });
        let mut remove_remote = None;
        for (idx, app) in self.draft.routing.always_remote.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.label(format!("• {app}"));
                if ui.small_button("Remove").clicked() {
                    remove_remote = Some(idx);
                }
            });
        }
        if let Some(idx) = remove_remote {
            self.draft.routing.always_remote.remove(idx);
        }

        ui.add_space(12.0);
        ui.separator();
        ui.subheading("Always Keep on Local Speaker (`always_local`)");
        ui.horizontal(|ui| {
            ui.text_edit_singleline(&mut self.new_local_app);
            if ui.button("Add App").clicked() && !self.new_local_app.trim().is_empty() {
                let app = self.new_local_app.trim().to_lowercase();
                if !self.draft.routing.always_local.contains(&app) {
                    self.draft.routing.always_local.push(app);
                }
                self.new_local_app.clear();
            }
        });
        let mut remove_local = None;
        for (idx, app) in self.draft.routing.always_local.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.label(format!("• {app}"));
                if ui.small_button("Remove").clicked() {
                    remove_local = Some(idx);
                }
            });
        }
        if let Some(idx) = remove_local {
            self.draft.routing.always_local.remove(idx);
        }
    }

    fn render_security_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("PIN / Pre-Shared Key (PSK) Pairing & HMAC Beacons");
        ui.label(
            "Pair devices with a shared 6-digit PIN to sign UDP discovery beacons with HMAC-SHA256 and synchronize a 256-byte PulseAudio cookie.",
        );
        ui.add_space(8.0);

        ui.horizontal(|ui| {
            if ui.button("Generate 6-Digit PIN").clicked() {
                self.generated_pin = generate_pairing_pin();
                self.pairing_pin_input = self.generated_pin.clone();
            }
            if !self.generated_pin.is_empty() {
                ui.label(
                    egui::RichText::new(format!("Generated PIN: {}", self.generated_pin))
                        .strong()
                        .size(16.0),
                );
            }
        });

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label("Enter Shared PIN:");
            ui.text_edit_singleline(&mut self.pairing_pin_input);
            if ui.button("Apply PIN & Sync Cookie").clicked() {
                let pin = self.pairing_pin_input.trim();
                if pin.len() < 4 {
                    self.status_banner =
                        Some(("PIN must be at least 4 characters.".to_string(), false));
                } else {
                    let shared_key = derive_key_hex(pin, "soundhop-pair");
                    let cookie = derive_pa_cookie(&shared_key);
                    match write_pa_cookie(None, &cookie) {
                        Ok(path) => {
                            self.draft.sharing.psk_hash = shared_key;
                            self.draft.sharing.security = "psk".to_string();
                            self.status_banner = Some((
                                format!("Paired with PIN and wrote cookie to {}", path.display()),
                                true,
                            ));
                        }
                        Err(e) => {
                            self.status_banner =
                                Some((format!("Failed to write cookie: {e}"), false));
                        }
                    }
                }
            }
        });

        ui.add_space(8.0);
        ui.label(format!(
            "Current PSK Status: {}",
            if self.draft.sharing.psk_hash.is_empty() {
                "Unpaired (Trusted LAN)"
            } else {
                "Configured (256-bit derived key active)"
            }
        ));
    }

    fn render_discovery_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("Discovery Protocols");
        ui.add_space(6.0);
        ui.checkbox(
            &mut self.draft.discovery.mdns,
            "Enable mDNS / Zeroconf (_soundhop._tcp.local.)",
        );
        ui.checkbox(
            &mut self.draft.discovery.udp,
            "Enable UDP Broadcast Beacons (with RTT echo)",
        );
        ui.horizontal(|ui| {
            ui.label("UDP Broadcast Port:");
            let mut bport = self.draft.discovery.port as u32;
            if ui
                .add(egui::DragValue::new(&mut bport).range(1024..=65535))
                .changed()
            {
                self.draft.discovery.port = bport as u16;
            }
        });

        ui.add_space(16.0);
        ui.separator();
        ui.heading("Background Daemon & Desktop Autostart");
        ui.add_space(6.0);
        ui.label(format!(
            "Service Running: {} | Service Enabled: {} | Tray Autostart: {}",
            if self.service_running { "Yes" } else { "No" },
            if self.service_enabled { "Yes" } else { "No" },
            if self.autostart_installed {
                "Installed"
            } else {
                "Not Installed"
            }
        ));
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui
                .button("Install & Start Service + Tray Autostart")
                .clicked()
            {
                let _ = install(true, true, true);
                self.refresh_service_state();
                self.status_banner =
                    Some(("Installed and enabled SoundHop service.".to_string(), true));
            }
            if ui.button("Uninstall Service").clicked() {
                let _ = uninstall();
                self.refresh_service_state();
                self.status_banner = Some(("Uninstalled SoundHop service.".to_string(), true));
            }
        });
    }
}

trait UiSubheadingExt {
    fn subheading(&mut self, text: &str);
}

impl UiSubheadingExt for egui::Ui {
    fn subheading(&mut self, text: &str) {
        self.label(egui::RichText::new(text).strong().size(15.0));
    }
}
