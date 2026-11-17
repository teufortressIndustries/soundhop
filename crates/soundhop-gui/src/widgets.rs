//! Reusable NUI widgets for SoundHop (`card`, `toggle_switch`, `pill_badge`,
//! `segmented_control`, `audio_wave_indicator`, and collapsible `Advanced` drawers).

use eframe::egui::{self, Color32, CornerRadius, Margin, RichText, Sense, Stroke, StrokeKind};

use crate::theme::Palette;

/// Renders a softly rounded, elevated container card.
pub fn card<R>(
    ui: &mut egui::Ui,
    pal: &Palette,
    accent_border: Option<Color32>,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<R> {
    let stroke = match accent_border {
        Some(c) => Stroke::new(1.5_f32, c),
        None => Stroke::new(1.0_f32, pal.border_subtle),
    };
    egui::Frame::new()
        .fill(pal.card)
        .stroke(stroke)
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::same(16))
        .show(ui, add_contents)
}

/// Renders a softly tinted inner banner/callout card (e.g. tips, warnings, status).
pub fn callout_card<R>(
    ui: &mut egui::Ui,
    bg: Color32,
    border: Color32,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<R> {
    egui::Frame::new()
        .fill(bg)
        .stroke(Stroke::new(1.0_f32, border))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::symmetric(14, 10))
        .show(ui, add_contents)
}

/// Animated iOS/macOS-style pill toggle switch.
pub fn toggle_switch(ui: &mut egui::Ui, pal: &Palette, on: &mut bool) -> egui::Response {
    let desired_size = egui::vec2(44.0, 24.0);
    let (rect, mut response) = ui.allocate_exact_size(desired_size, Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Checkbox, ui.is_enabled(), *on, "")
    });

    if ui.is_rect_visible(rect) {
        let how_on = ui.ctx().animate_bool_responsive(response.id, *on);
        let bg = egui::lerp(
            egui::Rgba::from(pal.switch_off_bg)..=egui::Rgba::from(pal.accent),
            how_on,
        );
        let bg_color = Color32::from(bg);
        let radius = rect.height() * 0.5;
        ui.painter().rect_filled(rect, radius, bg_color);

        if response.hovered() || response.has_focus() {
            ui.painter().rect_stroke(
                rect,
                radius,
                Stroke::new(1.5_f32, pal.accent_hover),
                StrokeKind::Outside,
            );
        }

        let knob_radius = (rect.height() - 6.0) * 0.5;
        let left_x = rect.left() + 3.0 + knob_radius;
        let right_x = rect.right() - 3.0 - knob_radius;
        let center_x = egui::lerp(left_x..=right_x, how_on);
        let center = egui::pos2(center_x, rect.center().y);
        ui.painter()
            .circle_filled(center, knob_radius, pal.switch_knob);
    }

    response
}

/// Soft rounded pill badge for compact status tags (`Connected`, `Wi-Fi`, `USB`, etc.).
pub fn pill_badge(ui: &mut egui::Ui, text: &str, fg: Color32, bg: Color32) {
    egui::Frame::new()
        .fill(bg)
        .corner_radius(CornerRadius::same(255))
        .inner_margin(Margin::symmetric(10, 3))
        .show(ui, |ui| {
            ui.label(RichText::new(text).color(fg).size(11.5).strong());
        });
}

/// High-contrast primary CTA button.
pub fn primary_button(ui: &mut egui::Ui, pal: &Palette, text: &str) -> egui::Response {
    let btn = egui::Button::new(RichText::new(text).color(pal.on_accent).strong())
        .fill(pal.accent)
        .stroke(Stroke::NONE)
        .corner_radius(CornerRadius::same(8))
        .min_size(egui::vec2(0.0, 32.0));
    ui.add(btn)
}

/// Subtle secondary ghost/outlined button.
pub fn secondary_button(ui: &mut egui::Ui, pal: &Palette, text: &str) -> egui::Response {
    let btn = egui::Button::new(RichText::new(text).color(pal.text))
        .fill(pal.card_elevated)
        .stroke(Stroke::new(1.0_f32, pal.border))
        .corner_radius(CornerRadius::same(8))
        .min_size(egui::vec2(0.0, 32.0));
    ui.add(btn)
}

/// Destructive/disconnect tinted button.
pub fn danger_button(ui: &mut egui::Ui, pal: &Palette, text: &str) -> egui::Response {
    let btn = egui::Button::new(RichText::new(text).color(pal.danger).strong())
        .fill(pal.danger_soft)
        .stroke(Stroke::new(1.0_f32, pal.danger))
        .corner_radius(CornerRadius::same(8))
        .min_size(egui::vec2(0.0, 32.0));
    ui.add(btn)
}

/// Clean segmented pill selector for 2-3 mutually exclusive choices.
pub fn segmented_control<T: Copy + PartialEq>(
    ui: &mut egui::Ui,
    pal: &Palette,
    current: &mut T,
    options: &[(&str, T)],
) -> bool {
    let mut changed = false;
    egui::Frame::new()
        .fill(pal.card_elevated)
        .stroke(Stroke::new(1.0_f32, pal.border_subtle))
        .corner_radius(CornerRadius::same(9))
        .inner_margin(Margin::same(3))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 3.0;
                for (label, value) in options {
                    let selected = *current == *value;
                    let fill = if selected {
                        pal.accent
                    } else {
                        Color32::TRANSPARENT
                    };
                    let fg = if selected {
                        pal.on_accent
                    } else {
                        pal.text_secondary
                    };
                    let btn =
                        egui::Button::new(RichText::new(*label).color(fg).size(12.5).strong())
                            .fill(fill)
                            .stroke(Stroke::NONE)
                            .corner_radius(CornerRadius::same(6))
                            .min_size(egui::vec2(0.0, 26.0));
                    if ui.add(btn).clicked() && !selected {
                        *current = *value;
                        changed = true;
                    }
                }
            });
        });
    changed
}

/// Animated 5-bar SoundHop audio wave visual echoing the app logo.
pub fn audio_wave_indicator(
    ui: &mut egui::Ui,
    pal: &Palette,
    active: bool,
    level_norm: f32,
    color: Color32,
) {
    let size = egui::vec2(38.0, 28.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }

    let time = if active {
        ui.ctx().input(|i| i.time) as f32
    } else {
        0.0
    };
    let bar_count = 5usize;
    let bar_w = 4.0;
    let gap = 3.5;
    let total_w = bar_count as f32 * bar_w + (bar_count - 1) as f32 * gap;
    let start_x = rect.center().x - total_w * 0.5;
    let base_profiles = [0.42_f32, 0.72, 1.0, 0.68, 0.38];

    for (i, base_profile) in base_profiles.iter().enumerate() {
        let phase = time * 6.5 + i as f32 * 0.95;
        let pulse = if active {
            let amp = (0.35 + level_norm * 0.65).clamp(0.25, 1.0);
            (0.32 + 0.68 * ((phase.sin() * 0.5 + 0.5) * amp)) * base_profile
        } else {
            0.22 * base_profile
        };
        let h = (rect.height() * pulse.clamp(0.16, 1.0)).max(4.0);
        let x = start_x + i as f32 * (bar_w + gap);
        let y_top = rect.center().y - h * 0.5;
        let bar_rect = egui::Rect::from_min_size(egui::pos2(x, y_top), egui::vec2(bar_w, h));
        let fill = if active { color } else { pal.text_muted };
        ui.painter().rect_filled(bar_rect, bar_w * 0.5, fill);
    }
}

/// Section header with crisp title and muted explanatory subtitle.
pub fn section_header(ui: &mut egui::Ui, pal: &Palette, title: &str, subtitle: Option<&str>) {
    ui.label(RichText::new(title).color(pal.text).size(16.0).strong());
    if let Some(sub) = subtitle {
        ui.label(RichText::new(sub).color(pal.text_muted).size(12.5));
    }
}

/// Progressive-disclosure collapsible Advanced drawer (closed by default).
pub fn collapsible_advanced(
    ui: &mut egui::Ui,
    pal: &Palette,
    id_salt: &str,
    title: &str,
    add_contents: impl FnOnce(&mut egui::Ui),
) {
    card(ui, pal, None, |ui| {
        egui::CollapsingHeader::new(RichText::new(title).color(pal.text_secondary).strong())
            .id_salt(id_salt)
            .default_open(false)
            .show(ui, |ui| {
                ui.add_space(6.0);
                add_contents(ui);
            });
    });
}
