//! Unified SoundHop Control Center (`eframe`/`egui`) with sidebar navigation,
//! live non-blocking backend sync, embedded brand icon, and onboarding overlay.

use std::sync::Arc;
use std::time::Duration;

use eframe::egui::{self, Color32, CornerRadius, Margin, RichText, Stroke};
use soundhop_core::controller::Controller;

use crate::backend::{GuiBackend, GuiCommand};
use crate::theme::{apply_theme, Palette, ThemeMode};
use crate::views::{apps, home, onboarding, phone, settings};
use crate::widgets::{callout_card, pill_badge};

const EMBEDDED_LOGO_PNG: &[u8] = include_bytes!("../../../assets/soundhop.png");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuiTab {
    Home,
    Apps,
    Phone,
    Settings,
}

impl GuiTab {
    pub fn from_arg(raw: &str) -> Self {
        match raw.trim().to_lowercase().as_str() {
            "apps" | "routing" => Self::Apps,
            "phone" | "android" => Self::Phone,
            "settings" | "preferences" => Self::Settings,
            _ => Self::Home,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Home => "Play & Share",
            Self::Apps => "Apps",
            Self::Phone => "Phone Audio",
            Self::Settings => "Preferences",
        }
    }
}

pub fn open_control_center(controller: Arc<Controller>, initial_tab: GuiTab) -> Result<(), String> {
    open_control_center_with_toast(controller, initial_tab, None)
}

pub fn open_control_center_with_toast(
    controller: Arc<Controller>,
    initial_tab: GuiTab,
    initial_toast: Option<String>,
) -> Result<(), String> {
    let _gui_lock = match soundhop_core::util::replace_stale_instance("gui", None) {
        Some(l) => l,
        None => {
            return Ok(());
        }
    };

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([840.0, 620.0])
        .with_min_inner_size([680.0, 500.0])
        .with_title("SoundHop");

    if let Some(icon) = load_window_icon() {
        viewport = viewport.with_icon(Arc::new(icon));
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "SoundHop",
        options,
        Box::new(move |cc| {
            let backend =
                GuiBackend::start_with_toast(controller, Some(cc.egui_ctx.clone()), initial_toast);
            Ok(Box::new(ControlCenterApp::new(backend, initial_tab)))
        }),
    )
    .map_err(|e| format!("Failed to open SoundHop Control Center: {e}"))
}

fn load_window_icon() -> Option<egui::IconData> {
    let img = image::load_from_memory(EMBEDDED_LOGO_PNG).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Some(egui::IconData {
        rgba: rgba.into_raw(),
        width: w,
        height: h,
    })
}

pub struct ControlCenterApp {
    backend: GuiBackend,
    active_tab: GuiTab,
    home_state: home::HomeViewState,
    apps_state: apps::AppsViewState,
    phone_state: phone::PhoneViewState,
    settings_state: settings::SettingsViewState,
    onboarding_state: onboarding::OnboardingState,
    logo_texture: Option<egui::TextureHandle>,
    system_dark_hint: Option<bool>,
}

impl ControlCenterApp {
    fn new(backend: GuiBackend, initial_tab: GuiTab) -> Self {
        Self {
            backend,
            active_tab: initial_tab,
            home_state: home::HomeViewState::default(),
            apps_state: apps::AppsViewState::default(),
            phone_state: phone::PhoneViewState::default(),
            settings_state: settings::SettingsViewState::default(),
            onboarding_state: onboarding::OnboardingState::default(),
            logo_texture: None,
            system_dark_hint: None,
        }
    }

    fn ensure_logo_texture(&mut self, ctx: &egui::Context) {
        if self.logo_texture.is_some() {
            return;
        }
        if let Ok(img) = image::load_from_memory(EMBEDDED_LOGO_PNG) {
            let rgba = img.to_rgba8();
            let size = [rgba.width() as usize, rgba.height() as usize];
            let color_image = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
            self.logo_texture = Some(ctx.load_texture(
                "soundhop_brand_logo",
                color_image,
                egui::TextureOptions::LINEAR,
            ));
        }
    }
}

impl eframe::App for ControlCenterApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.ensure_logo_texture(ctx);
        if self.system_dark_hint.is_none() {
            self.system_dark_hint = Some(ctx.style().visuals.dark_mode);
        }

        let snap = self.backend.read();
        let mode = ThemeMode::from_str_loose(&snap.config.ui.theme);
        let pal = apply_theme(ctx, mode, self.system_dark_hint.unwrap_or(true));

        let repaint_ms = if snap.audio_active || snap.connected || snap.busy_message.is_some() {
            66
        } else {
            400
        };
        ctx.request_repaint_after(Duration::from_millis(repaint_ms));

        if snap.show_onboarding {
            onboarding::render(ctx, &pal, &snap, &mut self.onboarding_state, &self.backend);
            return;
        }

        // Left Navigation Rail
        egui::SidePanel::left("soundhop_nav_rail")
            .exact_width(198.0)
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(pal.sidebar)
                    .stroke(Stroke::new(1.0_f32, pal.border_subtle))
                    .inner_margin(Margin::same(14)),
            )
            .show(ctx, |ui| {
                // Brand Header
                ui.horizontal(|ui| {
                    if let Some(ref tex) = self.logo_texture {
                        ui.add(
                            egui::Image::new(tex)
                                .fit_to_exact_size(egui::vec2(34.0, 34.0))
                                .corner_radius(CornerRadius::same(8)),
                        );
                    }
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("SoundHop")
                                .color(pal.text)
                                .size(16.5)
                                .strong(),
                        );
                        ui.label(
                            RichText::new(&snap.config.device.name)
                                .color(pal.text_muted)
                                .size(11.5),
                        );
                    });
                });

                ui.add_space(18.0);

                for tab in [GuiTab::Home, GuiTab::Apps, GuiTab::Phone, GuiTab::Settings] {
                    let selected = self.active_tab == tab;
                    let fill = if selected {
                        pal.accent_soft
                    } else {
                        Color32::TRANSPARENT
                    };
                    let stroke = if selected {
                        Stroke::new(1.0_f32, pal.accent)
                    } else {
                        Stroke::NONE
                    };
                    let text_color = if selected {
                        pal.accent
                    } else {
                        pal.text_secondary
                    };

                    let btn = egui::Button::new(
                        RichText::new(tab.label())
                            .color(text_color)
                            .size(14.0)
                            .strong(),
                    )
                    .fill(fill)
                    .stroke(stroke)
                    .corner_radius(CornerRadius::same(9))
                    .min_size(egui::vec2(ui.available_width(), 38.0));

                    if ui.add(btn).clicked() {
                        self.active_tab = tab;
                        match tab {
                            GuiTab::Phone => self.backend.send(GuiCommand::RefreshAndroid),
                            GuiTab::Settings => self.backend.send(GuiCommand::RefreshSystem),
                            GuiTab::Home => self.backend.send(GuiCommand::RefreshPeers),
                            GuiTab::Apps => self.backend.send(GuiCommand::RefreshFast),
                        }
                    }
                    ui.add_space(4.0);
                }

                // Bottom Rail Status + Quick Theme Switch
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                    ui.horizontal(|ui| {
                        let next_theme = if pal.dark { "light" } else { "dark" };
                        let theme_label = if pal.dark { "Light Mode" } else { "Dark Mode" };
                        let tbtn = egui::Button::new(
                            RichText::new(theme_label).color(pal.text_muted).size(11.5),
                        )
                        .fill(pal.card)
                        .stroke(Stroke::new(1.0_f32, pal.border_subtle))
                        .corner_radius(CornerRadius::same(7));
                        if ui.add(tbtn).clicked() {
                            self.backend.send(GuiCommand::SaveConfigPatch {
                                patch: serde_json::json!({ "ui": { "theme": next_theme } }),
                                toast: None,
                            });
                        }
                    });

                    ui.add_space(6.0);

                    if snap.android_session_active {
                        pill_badge(ui, "Phone Audio Live", pal.success, pal.success_soft);
                        ui.add_space(4.0);
                    }
                    if snap.connected {
                        pill_badge(
                            ui,
                            if snap.multiroom {
                                "Multi-Room Active"
                            } else {
                                "Speaker Connected"
                            },
                            pal.success,
                            pal.success_soft,
                        );
                    } else if snap.sharing_enabled {
                        pill_badge(ui, "Sharing Speakers", pal.accent, pal.accent_soft);
                    } else {
                        pill_badge(ui, "Ready", pal.text_muted, pal.card);
                    }
                });
            });

        // Main Workspace
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(pal.canvas)
                    .inner_margin(Margin::same(22)),
            )
            .show(ctx, |ui| {
                // Toast or busy indicator at top of workspace if present
                if let Some(ref busy) = snap.busy_message {
                    render_banner(ui, &pal, busy, false, true, &self.backend);
                    ui.add_space(10.0);
                } else if let Some(toast) = self.backend.latest_toast() {
                    render_banner(
                        ui,
                        &pal,
                        &toast.message,
                        toast.is_error,
                        false,
                        &self.backend,
                    );
                    ui.add_space(10.0);
                }

                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| match self.active_tab {
                        GuiTab::Home => {
                            home::render(ui, &pal, &snap, &mut self.home_state, &self.backend)
                        }
                        GuiTab::Apps => {
                            apps::render(ui, &pal, &snap, &mut self.apps_state, &self.backend)
                        }
                        GuiTab::Phone => {
                            phone::render(ui, &pal, &snap, &mut self.phone_state, &self.backend)
                        }
                        GuiTab::Settings => settings::render(
                            ui,
                            &pal,
                            &snap,
                            &mut self.settings_state,
                            &self.backend,
                        ),
                    });
            });
    }
}

fn render_banner(
    ui: &mut egui::Ui,
    pal: &Palette,
    message: &str,
    is_error: bool,
    is_busy: bool,
    backend: &GuiBackend,
) {
    let (bg, border, fg) = if is_error {
        (pal.danger_soft, pal.danger, pal.danger)
    } else if is_busy {
        (pal.accent_soft, pal.accent, pal.accent)
    } else {
        (pal.success_soft, pal.success, pal.success)
    };
    callout_card(ui, bg, border, |ui| {
        ui.horizontal(|ui| {
            if is_busy {
                ui.spinner();
            }
            ui.label(RichText::new(message).color(fg).size(12.5).strong());
            if !is_busy {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .small_button(RichText::new("Dismiss").color(pal.text_muted))
                        .clicked()
                    {
                        backend.dismiss_toast();
                    }
                });
            }
        });
    });
}
