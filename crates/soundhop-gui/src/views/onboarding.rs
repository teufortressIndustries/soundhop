//! Guided 3-step First-Run Onboarding overlay: plain-language setup for device name,
//! speaker sharing role, firewall allowance, and login autostart.

use std::time::Instant;

use eframe::egui::{self, RichText};

use crate::backend::{GuiBackend, GuiCommand, GuiSnapshot};
use crate::theme::{lerp_color, Palette};
use crate::widgets::{card, primary_button, secondary_button, selectable_card, toggle_switch};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnboardingRole {
    Both,
    SenderOnly,
    ReceiverOnly,
}

pub struct OnboardingState {
    pub step: u8,
    pub device_name: String,
    pub role: OnboardingRole,
    pub enable_autostart: bool,
    pub open_firewall: bool,
    initialized: bool,
    step_transition_start: Option<Instant>,
}

impl Default for OnboardingState {
    fn default() -> Self {
        Self {
            step: 0,
            device_name: String::new(),
            role: OnboardingRole::Both,
            enable_autostart: true,
            open_firewall: true,
            initialized: false,
            step_transition_start: None,
        }
    }
}

impl OnboardingState {
    fn sync_initial(&mut self, snap: &GuiSnapshot) {
        if !self.initialized {
            self.device_name = snap.config.device.name.clone();
            self.enable_autostart = snap.autostart_installed || snap.config.ui.start_tray;
            self.open_firewall = snap.firewall_active && snap.firewall_kind != "none";
            self.initialized = true;
        }
    }
}

pub fn render(
    ctx: &egui::Context,
    pal: &Palette,
    snap: &GuiSnapshot,
    state: &mut OnboardingState,
    backend: &GuiBackend,
) {
    state.sync_initial(snap);

    egui::CentralPanel::default()
        .frame(egui::Frame::new().fill(pal.canvas).inner_margin(egui::Margin::same(28)))
        .show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.set_max_width(540.0);
                ui.add_space(16.0);

                // Step dots
                ui.horizontal(|ui| {
                    for s in 0..3u8 {
                        let is_current = s == state.step;
                        let is_done = s < state.step;
                        let active_t = ctx.animate_bool_with_time_and_easing(
                            egui::Id::new(("onboard_step_dot_active", s)),
                            is_current,
                            0.14,
                            egui::emath::easing::cubic_out,
                        );
                        let done_t = ctx.animate_bool_with_time_and_easing(
                            egui::Id::new(("onboard_step_dot_done", s)),
                            is_done,
                            0.14,
                            egui::emath::easing::cubic_out,
                        );
                        let color = lerp_color(
                            lerp_color(pal.border, pal.success, done_t),
                            pal.accent,
                            active_t,
                        );
                        let width = 22.0 + 10.0 * active_t;
                        let (rect, _) =
                            ui.allocate_exact_size(egui::vec2(width, 6.0), egui::Sense::hover());
                        ui.painter().rect_filled(rect, 3.0, color);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .link(RichText::new("Skip setup").color(pal.text_muted).size(12.5))
                            .clicked()
                        {
                            backend.send(GuiCommand::CompleteOnboarding {
                                device_name: state.device_name.clone(),
                                enable_sharing: snap.sharing_enabled,
                                enable_autostart: snap.autostart_installed,
                                open_firewall: false,
                            });
                        }
                    });
                });

                ui.add_space(18.0);

                let (step_alpha, step_offset_y) =
                    if let Some(started) = state.step_transition_start {
                        let elapsed = started.elapsed().as_secs_f32();
                        let dur = 0.14_f32;
                        if elapsed < dur {
                            ctx.request_repaint();
                            let eased =
                                egui::emath::easing::cubic_out((elapsed / dur).clamp(0.0, 1.0));
                            (0.25 + 0.75 * eased, (1.0 - eased) * 6.0)
                        } else {
                            state.step_transition_start = None;
                            (1.0, 0.0)
                        }
                    } else {
                        (1.0, 0.0)
                    };

                card(ui, pal, Some(pal.border), |ui| {
                    ui.set_min_height(310.0);
                    ui.scope(|ui| {
                        if step_offset_y > 0.05 {
                            ui.add_space(step_offset_y);
                        }
                        ui.set_opacity(step_alpha);
                        match state.step {
                            0 => {
                                ui.label(
                                    RichText::new("Welcome to SoundHop")
                                        .color(pal.text)
                                        .size(22.0)
                                        .strong(),
                                );
                                ui.add_space(6.0);
                                ui.label(
                                    RichText::new(
                                        "Send music, videos, and calls to any speakers on your Wi-Fi — or turn this computer into wireless speakers.",
                                    )
                                    .color(pal.text_secondary)
                                    .size(13.5),
                                );
                                ui.add_space(20.0);
                                ui.label(
                                    RichText::new("What should we call this device?")
                                        .color(pal.text)
                                        .size(14.0)
                                        .strong(),
                                );
                                ui.label(
                                    RichText::new("Example: \"Living Room PC\" or \"Work Laptop\"")
                                        .color(pal.text_muted)
                                        .size(12.0),
                                );
                                ui.add_space(8.0);
                                ui.add(
                                    egui::TextEdit::singleline(&mut state.device_name)
                                        .desired_width(f32::INFINITY),
                                );
                            }
                            1 => {
                                ui.label(
                                    RichText::new("How will you use this device?")
                                        .color(pal.text)
                                        .size(20.0)
                                        .strong(),
                                );
                                ui.add_space(6.0);
                                ui.label(
                                    RichText::new("You can always change this later from Home.")
                                        .color(pal.text_muted)
                                        .size(12.5),
                                );
                                ui.add_space(14.0);

                                for (idx, (role, title, desc)) in [
                                    (
                                        OnboardingRole::Both,
                                        "Send and receive audio (Recommended)",
                                        "Play audio on nearby speakers, and let other devices use these speakers too.",
                                    ),
                                    (
                                        OnboardingRole::SenderOnly,
                                        "Play my audio on other speakers",
                                        "Keep this device private while streaming to speakers in other rooms.",
                                    ),
                                    (
                                        OnboardingRole::ReceiverOnly,
                                        "Use this PC as wireless speakers",
                                        "Share these speakers so your laptop or phone can play audio here.",
                                    ),
                                ]
                                .into_iter()
                                .enumerate()
                                {
                                    let selected = state.role == role;
                                    if selectable_card(
                                        ui,
                                        pal,
                                        ("onboard_role", idx),
                                        selected,
                                        title,
                                        desc,
                                    )
                                    .clicked()
                                    {
                                        state.role = role;
                                    }
                                    ui.add_space(8.0);
                                }
                            }
                            _ => {
                                ui.label(
                                    RichText::new("Ready for effortless audio")
                                        .color(pal.text)
                                        .size(20.0)
                                        .strong(),
                                );
                                ui.add_space(12.0);

                                ui.horizontal(|ui| {
                                    ui.vertical(|ui| {
                                        ui.label(
                                            RichText::new("Keep SoundHop running in the background")
                                                .color(pal.text)
                                                .size(14.0)
                                                .strong(),
                                        );
                                        ui.label(
                                            RichText::new(
                                                "Start quietly in the system tray at login so your speakers are always ready.",
                                            )
                                            .color(pal.text_muted)
                                            .size(12.0),
                                        );
                                    });
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            toggle_switch(ui, pal, &mut state.enable_autostart);
                                        },
                                    );
                                });

                                if snap.firewall_active
                                    && snap.firewall_kind != "none"
                                    && state.role != OnboardingRole::SenderOnly
                                {
                                    ui.add_space(14.0);
                                    ui.separator();
                                    ui.add_space(10.0);
                                    ui.horizontal(|ui| {
                                        ui.vertical(|ui| {
                                            ui.label(
                                                RichText::new(format!(
                                                    "Allow SoundHop through {} firewall",
                                                    snap.firewall_kind
                                                ))
                                                .color(pal.text)
                                                .size(14.0)
                                                .strong(),
                                            );
                                            ui.label(
                                                RichText::new(
                                                    "Needed so other devices can reach your speakers.",
                                                )
                                                .color(pal.text_muted)
                                                .size(12.0),
                                            );
                                        });
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                toggle_switch(ui, pal, &mut state.open_firewall);
                                            },
                                        );
                                    });
                                }
                            }
                        }
                    });

                    ui.add_space(20.0);
                    ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                        ui.horizontal(|ui| {
                            if state.step > 0 && secondary_button(ui, pal, "Back").clicked() {
                                state.step -= 1;
                                state.step_transition_start = Some(Instant::now());
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if state.step < 2 {
                                        if primary_button(ui, pal, "Continue").clicked() {
                                            state.step += 1;
                                            state.step_transition_start = Some(Instant::now());
                                        }
                                    } else if primary_button(ui, pal, "Start Using SoundHop")
                                        .clicked()
                                    {
                                        let enable_sharing =
                                            state.role != OnboardingRole::SenderOnly;
                                        backend.send(GuiCommand::CompleteOnboarding {
                                            device_name: state.device_name.clone(),
                                            enable_sharing,
                                            enable_autostart: state.enable_autostart,
                                            open_firewall: state.open_firewall && enable_sharing,
                                        });
                                    }
                                },
                            );
                        });
                    });
                });
            });
        });
}
