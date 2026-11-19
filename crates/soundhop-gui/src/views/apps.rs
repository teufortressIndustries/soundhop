//! Apps ("Per-App Audio") view: choose which apps play on remote speakers vs this device,
//! with 1-click "Always remember" persistent rules.

use eframe::egui::{self, RichText};

use crate::backend::{GuiBackend, GuiCommand, GuiSnapshot};
use crate::theme::Palette;
use crate::widgets::{card, pill_badge, secondary_button, section_header, segmented_control};

#[derive(Default)]
pub struct AppsViewState {
    pub custom_app_name: String,
    pub custom_target_remote: bool,
}

pub fn render(
    ui: &mut egui::Ui,
    pal: &Palette,
    snap: &GuiSnapshot,
    state: &mut AppsViewState,
    backend: &GuiBackend,
) {
    section_header(
        ui,
        pal,
        "Choose where each app plays",
        Some("Send music to your speakers while keeping browser calls or alerts on this device."),
    );
    ui.add_space(10.0);

    // Default routing behavior
    card(ui, pal, None, |ui| {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(
                    RichText::new("When connected to a speaker")
                        .color(pal.text)
                        .size(14.0)
                        .strong(),
                );
                ui.label(
                    RichText::new("Pick what happens to apps that don't have a custom rule.")
                        .color(pal.text_muted)
                        .size(12.0),
                );
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                #[derive(Clone, Copy, PartialEq)]
                enum DefaultMode {
                    All,
                    Selective,
                }
                let mut mode = if snap.config.routing.default_mode == "selective" {
                    DefaultMode::Selective
                } else {
                    DefaultMode::All
                };
                if segmented_control(
                    ui,
                    pal,
                    &mut mode,
                    &[
                        ("Send all apps", DefaultMode::All),
                        ("Only picked apps", DefaultMode::Selective),
                    ],
                ) {
                    let next = match mode {
                        DefaultMode::All => "all",
                        DefaultMode::Selective => "selective",
                    };
                    backend.send(GuiCommand::SetDefaultRoutingMode {
                        mode: next.to_string(),
                    });
                }
            });
        });
    });

    ui.add_space(14.0);

    let active_apps: Vec<_> = snap.apps.iter().filter(|a| a.active).collect();
    let saved_only: Vec<_> = snap
        .apps
        .iter()
        .filter(|a| !a.active && a.persistent)
        .collect();

    ui.label(
        RichText::new("Playing now")
            .color(pal.text)
            .size(14.5)
            .strong(),
    );
    ui.add_space(6.0);

    if active_apps.is_empty() {
        card(ui, pal, None, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(6.0);
                ui.label(
                    RichText::new("No apps are playing audio right now")
                        .color(pal.text)
                        .size(14.0)
                        .strong(),
                );
                ui.label(
                    RichText::new(
                        "Start playing music or a video in any app and it will appear here.",
                    )
                    .color(pal.text_muted)
                    .size(12.0),
                );
                ui.add_space(6.0);
            });
        });
    } else {
        for app in active_apps {
            card(ui, pal, None, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(&app.app_name)
                                    .color(pal.text)
                                    .size(14.5)
                                    .strong(),
                            );
                            if app.persistent {
                                pill_badge(ui, "Remembered", pal.accent, pal.accent_soft);
                            }
                        });
                        if !app.media_name.is_empty() {
                            ui.label(
                                RichText::new(&app.media_name)
                                    .color(pal.text_muted)
                                    .size(12.0),
                            );
                        }
                    });

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let rem_label = if app.persistent {
                            "Forget Rule"
                        } else {
                            "Always Remember"
                        };
                        if secondary_button(ui, pal, rem_label).clicked() {
                            if app.persistent {
                                backend.send(GuiCommand::UnrouteApp {
                                    app: app.app_name.clone(),
                                    remove_rule: true,
                                });
                            } else {
                                let target = if app.is_remote || app.routed_rule == "remote" {
                                    "remote"
                                } else {
                                    "local"
                                };
                                backend.send(GuiCommand::RouteApp {
                                    app: app.app_name.clone(),
                                    target: target.to_string(),
                                    save: true,
                                });
                            }
                        }

                        ui.add_space(6.0);

                        #[derive(Clone, Copy, PartialEq)]
                        enum AppDest {
                            ThisDevice,
                            Remote,
                        }
                        let is_remote = app.is_remote || app.routed_rule == "remote";
                        let mut dest = if is_remote {
                            AppDest::Remote
                        } else {
                            AppDest::ThisDevice
                        };
                        ui.add_enabled_ui(snap.connected || app.persistent, |ui| {
                            if segmented_control(
                                ui,
                                pal,
                                &mut dest,
                                &[
                                    ("This Device", AppDest::ThisDevice),
                                    ("Remote", AppDest::Remote),
                                ],
                            ) {
                                let target = match dest {
                                    AppDest::ThisDevice => "local",
                                    AppDest::Remote => "remote",
                                };
                                backend.send(GuiCommand::RouteApp {
                                    app: app.app_name.clone(),
                                    target: target.to_string(),
                                    save: app.persistent,
                                });
                            }
                        });
                    });
                });
            });
            ui.add_space(6.0);
        }
    }

    ui.add_space(14.0);
    ui.label(
        RichText::new("Remembered app rules")
            .color(pal.text)
            .size(14.5)
            .strong(),
    );
    ui.add_space(6.0);

    card(ui, pal, None, |ui| {
        let mut showed_above_divider = false;
        if saved_only.is_empty() && snap.apps.iter().all(|a| !a.persistent) {
            ui.label(
                RichText::new("No saved app rules yet. Click \"Always Remember\" on any playing app or add one below.")
                    .color(pal.text_muted)
                    .size(12.0),
            );
            showed_above_divider = true;
        } else if !saved_only.is_empty() {
            for app in saved_only {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&app.app_name).color(pal.text).strong());
                    if app.routed_rule == "remote" {
                        pill_badge(ui, "Always Remote", pal.success, pal.success_soft);
                    } else {
                        pill_badge(ui, "Always This Device", pal.accent, pal.accent_soft);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if secondary_button(ui, pal, "Remove").clicked() {
                            backend.send(GuiCommand::UnrouteApp {
                                app: app.app_name.clone(),
                                remove_rule: true,
                            });
                        }
                    });
                });
                ui.add_space(4.0);
            }
            showed_above_divider = true;
        }

        if showed_above_divider {
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(8.0);
        }
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut state.custom_app_name)
                    .hint_text("App name (e.g. spotify, firefox)")
                    .desired_width(210.0),
            );
            #[derive(Clone, Copy, PartialEq)]
            enum RuleDest {
                Remote,
                Local,
            }
            let mut rd = if state.custom_target_remote {
                RuleDest::Remote
            } else {
                RuleDest::Local
            };
            if segmented_control(
                ui,
                pal,
                &mut rd,
                &[
                    ("Remote", RuleDest::Remote),
                    ("This Device", RuleDest::Local),
                ],
            ) {
                state.custom_target_remote = matches!(rd, RuleDest::Remote);
            }
            if secondary_button(ui, pal, "Add Rule").clicked()
                && !state.custom_app_name.trim().is_empty()
            {
                let name = state.custom_app_name.trim().to_string();
                let target = if state.custom_target_remote {
                    "remote"
                } else {
                    "local"
                };
                backend.send(GuiCommand::RouteApp {
                    app: name,
                    target: target.to_string(),
                    save: true,
                });
                state.custom_app_name.clear();
            }
        });
    });
}
