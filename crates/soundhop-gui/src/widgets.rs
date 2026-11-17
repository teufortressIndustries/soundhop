//! Reusable NUI widgets for SoundHop (`card`, `toggle_switch`, `pill_badge`,
//! `segmented_control`, `selectable_card`, `audio_wave_indicator`, and collapsible `Advanced` drawers).

use std::sync::Arc;

use eframe::egui::{
    self, Color32, CornerRadius, Margin, RichText, Sense, Stroke, StrokeKind, WidgetText,
};

use crate::theme::{lerp_color, Palette};

#[derive(Clone, Copy)]
enum ButtonTone {
    Primary,
    Secondary,
    Danger,
    NavTab { selected: bool },
}

fn click_pulse(ctx: &egui::Context, id: egui::Id, clicked: bool) -> f32 {
    let now = ctx.input(|i| i.time);
    let key = id.with("click_pulse_t");
    if clicked {
        ctx.data_mut(|d| d.insert_temp(key, now));
    }
    let Some(t0) = ctx.data(|d| d.get_temp::<f64>(key)) else {
        return 0.0;
    };
    let elapsed = (now - t0) as f32;
    let duration = 0.18_f32;
    if elapsed >= 0.0 && elapsed < duration {
        ctx.request_repaint();
        1.0 - egui::emath::easing::cubic_out(elapsed / duration)
    } else {
        0.0
    }
}

#[allow(clippy::too_many_arguments)]
fn styled_button(
    ui: &mut egui::Ui,
    pal: &Palette,
    text: &str,
    tone: ButtonTone,
    min_size: egui::Vec2,
    padding: egui::Vec2,
    corner_radius: u8,
    text_size: Option<f32>,
    strong: bool,
    left_align: bool,
) -> egui::Response {
    let mut rich = RichText::new(text);
    if let Some(sz) = text_size {
        rich = rich.size(sz);
    }
    if strong {
        rich = rich.strong();
    }

    let wrap_width = (ui.available_width() - 2.0 * padding.x).max(40.0);
    let galley = WidgetText::from(rich).into_galley(ui, None, wrap_width, egui::TextStyle::Button);

    let mut desired_size = galley.size() + 2.0 * padding;
    desired_size = desired_size.max(min_size);

    let (rect, response) = ui.allocate_at_least(desired_size, Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, ui.is_enabled(), galley.text())
    });

    let enabled = ui.is_enabled();
    if enabled && response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    if ui.is_rect_visible(rect) {
        let hover_t = ui.ctx().animate_bool_with_time_and_easing(
            response.id.with("hover"),
            enabled && (response.hovered() || response.has_focus()),
            0.09,
            egui::emath::easing::cubic_out,
        );
        let press_t = ui.ctx().animate_bool_with_time_and_easing(
            response.id.with("press"),
            enabled && response.is_pointer_button_down_on(),
            0.06,
            egui::emath::easing::cubic_out,
        );
        let pulse_t = if enabled {
            click_pulse(ui.ctx(), response.id, response.clicked())
        } else {
            0.0
        };

        let (mut fill, mut stroke_color, mut stroke_w, mut text_color, pulse_color) = match tone {
            ButtonTone::Primary => {
                let hover_fill = lerp_color(pal.accent, pal.accent_hover, hover_t);
                let press_target = lerp_color(pal.accent, Color32::from_rgb(8, 36, 56), 0.25);
                let fill = lerp_color(hover_fill, press_target, press_t);
                let stroke_c = lerp_color(
                    Color32::TRANSPARENT,
                    Color32::from_rgba_unmultiplied(255, 255, 255, if pal.dark { 55 } else { 70 }),
                    hover_t,
                );
                (fill, stroke_c, 1.0_f32, pal.on_accent, pal.accent_hover)
            }
            ButtonTone::Secondary => {
                let hover_fill = lerp_color(pal.card_elevated, pal.card_hover, hover_t);
                let fill = lerp_color(hover_fill, pal.accent_soft, press_t * 0.65);
                let stroke_c = lerp_color(pal.border, pal.accent, (hover_t * 0.85).max(press_t));
                let stroke_w = 1.0 + 0.2 * hover_t.max(press_t);
                (fill, stroke_c, stroke_w, pal.text, pal.accent)
            }
            ButtonTone::Danger => {
                let alpha_base = pal.danger_soft.a() as f32;
                let hover_bg = Color32::from_rgba_unmultiplied(
                    pal.danger.r(),
                    pal.danger.g(),
                    pal.danger.b(),
                    (alpha_base + 24.0).min(90.0) as u8,
                );
                let press_bg = Color32::from_rgba_unmultiplied(
                    pal.danger.r(),
                    pal.danger.g(),
                    pal.danger.b(),
                    (alpha_base + 52.0).min(125.0) as u8,
                );
                let fill = lerp_color(
                    lerp_color(pal.danger_soft, hover_bg, hover_t),
                    press_bg,
                    press_t,
                );
                let idle_stroke = Color32::from_rgba_unmultiplied(
                    pal.danger.r(),
                    pal.danger.g(),
                    pal.danger.b(),
                    185,
                );
                let stroke_c = lerp_color(idle_stroke, pal.danger, hover_t.max(press_t));
                (
                    fill,
                    stroke_c,
                    1.0 + 0.3 * hover_t.max(press_t),
                    pal.danger,
                    pal.danger,
                )
            }
            ButtonTone::NavTab { selected } => {
                let sel_t = ui.ctx().animate_bool_with_time_and_easing(
                    response.id.with("selected"),
                    selected,
                    0.13,
                    egui::emath::easing::cubic_out,
                );
                let idle_fill = lerp_color(Color32::TRANSPARENT, pal.accent_soft, sel_t);
                let hover_fill = lerp_color(pal.card_elevated, pal.accent_soft, sel_t);
                let fill = lerp_color(
                    lerp_color(idle_fill, hover_fill, hover_t),
                    pal.accent_soft,
                    press_t * 0.75,
                );
                let idle_stroke = lerp_color(Color32::TRANSPARENT, pal.accent, sel_t);
                let hover_stroke = lerp_color(pal.border_subtle, pal.accent, sel_t.max(0.35));
                let stroke_c = lerp_color(idle_stroke, hover_stroke, hover_t);
                let fg = lerp_color(
                    lerp_color(pal.text_secondary, pal.text, hover_t),
                    pal.accent,
                    sel_t,
                );
                (fill, stroke_c, 1.0, fg, pal.accent)
            }
        };

        if !enabled {
            fill = fill.gamma_multiply(0.5);
            stroke_color = stroke_color.gamma_multiply(0.45);
            text_color = text_color.gamma_multiply(0.45);
            stroke_w = 1.0;
        }

        let draw_rect = rect.shrink(press_t * 1.0);
        let cr = CornerRadius::same(corner_radius);
        ui.painter().rect(
            draw_rect,
            cr,
            fill,
            Stroke::new(stroke_w, stroke_color),
            StrokeKind::Inside,
        );

        if pulse_t > 0.01 {
            let pulse_alpha = (pulse_t * 130.0) as u8;
            let pulse_stroke = Color32::from_rgba_unmultiplied(
                pulse_color.r(),
                pulse_color.g(),
                pulse_color.b(),
                pulse_alpha,
            );
            ui.painter().rect_stroke(
                draw_rect.expand((1.0 - pulse_t) * 2.5),
                CornerRadius::same(corner_radius.saturating_add(1)),
                Stroke::new(1.5_f32, pulse_stroke),
                StrokeKind::Outside,
            );
        }

        if enabled && response.has_focus() {
            ui.painter().rect_stroke(
                draw_rect,
                cr,
                Stroke::new(1.5_f32, pal.accent),
                StrokeKind::Outside,
            );
        }

        let text_offset_y = press_t * 1.0;
        let text_pos = if left_align {
            egui::pos2(
                draw_rect.left() + padding.x,
                draw_rect.center().y - galley.size().y * 0.5 + text_offset_y,
            )
        } else {
            egui::pos2(
                draw_rect.center().x - galley.size().x * 0.5,
                draw_rect.center().y - galley.size().y * 0.5 + text_offset_y,
            )
        };
        ui.painter().galley(text_pos, galley, text_color);
    }

    response
}

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

/// Animated iOS/macOS-style pill toggle switch with hover/press knob feedback.
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

    let enabled = ui.is_enabled();
    if enabled && response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    if ui.is_rect_visible(rect) {
        let how_on = ui.ctx().animate_bool_responsive(response.id, *on);
        let hover_t = ui.ctx().animate_bool_with_time_and_easing(
            response.id.with("hover"),
            enabled && (response.hovered() || response.has_focus()),
            0.09,
            egui::emath::easing::cubic_out,
        );
        let press_t = ui.ctx().animate_bool_with_time_and_easing(
            response.id.with("press"),
            enabled && response.is_pointer_button_down_on(),
            0.06,
            egui::emath::easing::cubic_out,
        );

        let off_bg = lerp_color(pal.switch_off_bg, pal.card_hover, hover_t * 0.4);
        let on_bg = lerp_color(pal.accent, pal.accent_hover, hover_t * 0.7);
        let bg_color = lerp_color(off_bg, on_bg, how_on);
        let radius = rect.height() * 0.5;
        ui.painter().rect_filled(rect, radius, bg_color);

        if hover_t > 0.01 {
            let stroke_c = Color32::from_rgba_unmultiplied(
                pal.accent_hover.r(),
                pal.accent_hover.g(),
                pal.accent_hover.b(),
                (hover_t * 200.0) as u8,
            );
            ui.painter().rect_stroke(
                rect,
                radius,
                Stroke::new(1.5_f32, stroke_c),
                StrokeKind::Outside,
            );
        }

        let base_knob_radius = (rect.height() - 6.0) * 0.5;
        let knob_radius = base_knob_radius + hover_t * 0.5;
        let knob_stretch = press_t * 3.0;
        let left_x = rect.left() + 3.0 + base_knob_radius;
        let right_x = rect.right() - 3.0 - base_knob_radius;
        let center_x = egui::lerp(left_x..=right_x, how_on);
        let knob_w = knob_radius * 2.0 + knob_stretch;
        let knob_h = knob_radius * 2.0;
        let min_x = (center_x - knob_w * 0.5)
            .max(rect.left() + 2.5)
            .min(rect.right() - 2.5 - knob_w);
        let knob_rect = egui::Rect::from_min_size(
            egui::pos2(min_x, rect.center().y - knob_radius),
            egui::vec2(knob_w, knob_h),
        );
        ui.painter()
            .rect_filled(knob_rect, knob_radius, pal.switch_knob);
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

/// High-contrast primary CTA button with smooth hover/press/click animations.
pub fn primary_button(ui: &mut egui::Ui, pal: &Palette, text: &str) -> egui::Response {
    styled_button(
        ui,
        pal,
        text,
        ButtonTone::Primary,
        egui::vec2(0.0, 32.0),
        egui::vec2(14.0, 7.0),
        8,
        None,
        true,
        false,
    )
}

/// Subtle secondary ghost/outlined button with smooth hover/press/click animations.
pub fn secondary_button(ui: &mut egui::Ui, pal: &Palette, text: &str) -> egui::Response {
    styled_button(
        ui,
        pal,
        text,
        ButtonTone::Secondary,
        egui::vec2(0.0, 32.0),
        egui::vec2(14.0, 7.0),
        8,
        None,
        false,
        false,
    )
}

/// Destructive/disconnect tinted button with smooth hover/press/click animations.
pub fn danger_button(ui: &mut egui::Ui, pal: &Palette, text: &str) -> egui::Response {
    styled_button(
        ui,
        pal,
        text,
        ButtonTone::Danger,
        egui::vec2(0.0, 32.0),
        egui::vec2(14.0, 7.0),
        8,
        None,
        true,
        false,
    )
}

/// Compact secondary action button (e.g., banner Dismiss, sidebar theme switch).
pub fn compact_button(ui: &mut egui::Ui, pal: &Palette, text: &str) -> egui::Response {
    styled_button(
        ui,
        pal,
        text,
        ButtonTone::Secondary,
        egui::vec2(0.0, 24.0),
        egui::vec2(10.0, 4.0),
        7,
        Some(11.5),
        false,
        false,
    )
}

/// Animated left navigation rail tab button.
pub fn nav_tab_button(
    ui: &mut egui::Ui,
    pal: &Palette,
    text: &str,
    selected: bool,
) -> egui::Response {
    styled_button(
        ui,
        pal,
        text,
        ButtonTone::NavTab { selected },
        egui::vec2(ui.available_width(), 38.0),
        egui::vec2(14.0, 8.0),
        9,
        Some(14.0),
        true,
        true,
    )
}

/// Animated selectable option card for mutually exclusive choices (privacy modes, onboarding roles).
pub fn selectable_card(
    ui: &mut egui::Ui,
    pal: &Palette,
    id_salt: impl std::hash::Hash,
    selected: bool,
    title: &str,
    description: &str,
) -> egui::Response {
    let id = ui.make_persistent_id(id_salt);
    let pad = egui::vec2(14.0, 10.0);
    let radio_w = 24.0;
    let width = ui.available_width().max(120.0);
    let text_wrap = (width - pad.x * 2.0 - radio_w).max(60.0);

    let title_galley = WidgetText::from(RichText::new(title).size(13.5).strong()).into_galley(
        ui,
        None,
        text_wrap,
        egui::TextStyle::Body,
    );
    let desc_galley = WidgetText::from(RichText::new(description).size(12.0)).into_galley(
        ui,
        None,
        text_wrap,
        egui::TextStyle::Body,
    );

    let text_h = title_galley.size().y + 2.0 + desc_galley.size().y;
    let height = (text_h + pad.y * 2.0).max(48.0);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), Sense::click());
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::WidgetType::RadioButton,
            ui.is_enabled(),
            selected,
            title,
        )
    });

    let enabled = ui.is_enabled();
    if enabled && response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    if ui.is_rect_visible(rect) {
        let sel_t = ui.ctx().animate_bool_with_time_and_easing(
            id.with("sel"),
            selected,
            0.13,
            egui::emath::easing::cubic_out,
        );
        let hover_t = ui.ctx().animate_bool_with_time_and_easing(
            id.with("hover"),
            enabled && (response.hovered() || response.has_focus()),
            0.09,
            egui::emath::easing::cubic_out,
        );
        let press_t = ui.ctx().animate_bool_with_time_and_easing(
            id.with("press"),
            enabled && response.is_pointer_button_down_on(),
            0.06,
            egui::emath::easing::cubic_out,
        );
        let pulse_t = if enabled {
            click_pulse(ui.ctx(), id, response.clicked())
        } else {
            0.0
        };

        let base_fill = lerp_color(pal.card_elevated, pal.card_hover, hover_t * 0.7);
        let fill = lerp_color(base_fill, pal.accent_soft, sel_t.max(press_t * 0.45));
        let base_stroke = lerp_color(pal.border_subtle, pal.border, hover_t);
        let stroke_c = lerp_color(base_stroke, pal.accent, sel_t.max(hover_t * 0.55));
        let stroke_w = 1.0 + 0.35 * sel_t;

        let draw_rect = rect.shrink(press_t * 1.0);
        let cr = CornerRadius::same(10);
        ui.painter().rect(
            draw_rect,
            cr,
            fill,
            Stroke::new(stroke_w, stroke_c),
            StrokeKind::Inside,
        );

        if pulse_t > 0.01 {
            let p_alpha = (pulse_t * 110.0) as u8;
            ui.painter().rect_stroke(
                draw_rect.expand((1.0 - pulse_t) * 2.0),
                CornerRadius::same(11),
                Stroke::new(
                    1.5_f32,
                    Color32::from_rgba_unmultiplied(
                        pal.accent.r(),
                        pal.accent.g(),
                        pal.accent.b(),
                        p_alpha,
                    ),
                ),
                StrokeKind::Outside,
            );
        }

        // Animated radio indicator
        let radio_center = egui::pos2(draw_rect.left() + pad.x + 8.0, draw_rect.center().y);
        let outer_r = 7.5;
        let ring_color = lerp_color(pal.border, pal.accent, sel_t.max(hover_t * 0.6));
        ui.painter()
            .circle_stroke(radio_center, outer_r, Stroke::new(1.5_f32, ring_color));
        if sel_t > 0.02 {
            ui.painter()
                .circle_filled(radio_center, 4.2 * sel_t, pal.accent);
        }

        let text_x = draw_rect.left() + pad.x + radio_w;
        let text_y = draw_rect.center().y - text_h * 0.5 + press_t * 0.5;
        ui.painter()
            .galley(egui::pos2(text_x, text_y), title_galley, pal.text);
        ui.painter().galley(
            egui::pos2(text_x, text_y + text_h - desc_galley.size().y),
            desc_galley,
            pal.text_muted,
        );
    }

    response
}

/// Clean sliding segmented pill selector for 2-3 mutually exclusive choices.
pub fn segmented_control<T: Copy + PartialEq>(
    ui: &mut egui::Ui,
    pal: &Palette,
    current: &mut T,
    options: &[(&str, T)],
) -> bool {
    let mut changed = false;
    let enabled = ui.is_enabled();

    egui::Frame::new()
        .fill(pal.card_elevated)
        .stroke(Stroke::new(1.0_f32, pal.border_subtle))
        .corner_radius(CornerRadius::same(9))
        .inner_margin(Margin::same(3))
        .show(ui, |ui| {
            struct SegmentLayout {
                rect: egui::Rect,
                response: egui::Response,
                galley: Arc<egui::Galley>,
                selected: bool,
                hovered: bool,
                pressed: bool,
            }

            let mut layouts: Vec<SegmentLayout> = Vec::with_capacity(options.len());
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 3.0;
                for (label, value) in options {
                    let selected = *current == *value;
                    let galley = WidgetText::from(RichText::new(*label).size(12.5).strong())
                        .into_galley(
                            ui,
                            Some(egui::TextWrapMode::Extend),
                            f32::INFINITY,
                            egui::TextStyle::Button,
                        );
                    let desired =
                        egui::vec2(galley.size().x + 22.0, (galley.size().y + 10.0).max(26.0));
                    let (rect, response) = ui.allocate_exact_size(desired, Sense::click());
                    response.widget_info(|| {
                        egui::WidgetInfo::selected(
                            egui::WidgetType::RadioButton,
                            enabled,
                            selected,
                            *label,
                        )
                    });
                    if enabled && response.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    if response.clicked() && !selected {
                        *current = *value;
                        changed = true;
                    }
                    let hovered = enabled && (response.hovered() || response.has_focus());
                    let pressed = enabled && response.is_pointer_button_down_on();
                    layouts.push(SegmentLayout {
                        rect,
                        response,
                        galley,
                        selected: *current == *value,
                        hovered,
                        pressed,
                    });
                }
            });

            if let Some(first) = layouts.first() {
                let seg_id = first.response.id;
                let origin_x = first.rect.left();
                let selected_layout = layouts.iter().find(|l| l.selected).unwrap_or(first);
                let target_rel_x = selected_layout.rect.left() - origin_x;
                let target_w = selected_layout.rect.width();

                let anim_rel_x =
                    ui.ctx()
                        .animate_value_with_time(seg_id.with("pill_x"), target_rel_x, 0.12);
                let anim_w =
                    ui.ctx()
                        .animate_value_with_time(seg_id.with("pill_w"), target_w, 0.12);

                // Hover tints on unselected options (drawn behind sliding pill)
                for (idx, item) in layouts.iter().enumerate() {
                    let hover_t = ui.ctx().animate_bool_with_time_and_easing(
                        seg_id.with(("hover", idx)),
                        item.hovered && !item.selected,
                        0.08,
                        egui::emath::easing::cubic_out,
                    );
                    if hover_t > 0.01 {
                        let hover_fill = Color32::from_rgba_unmultiplied(
                            pal.card_hover.r(),
                            pal.card_hover.g(),
                            pal.card_hover.b(),
                            (hover_t * 210.0) as u8,
                        );
                        ui.painter()
                            .rect_filled(item.rect, CornerRadius::same(6), hover_fill);
                    }
                }

                // Sliding active pill
                let pill_rect = egui::Rect::from_min_size(
                    egui::pos2(origin_x + anim_rel_x, selected_layout.rect.top()),
                    egui::vec2(anim_w.max(8.0), selected_layout.rect.height()),
                );
                let pill_fill = if enabled {
                    pal.accent
                } else {
                    pal.accent.gamma_multiply(0.5)
                };
                ui.painter()
                    .rect_filled(pill_rect, CornerRadius::same(6), pill_fill);

                // Labels on top
                for (idx, item) in layouts.iter().enumerate() {
                    let sel_t = ui.ctx().animate_bool_with_time_and_easing(
                        seg_id.with(("sel_fg", idx)),
                        item.selected,
                        0.11,
                        egui::emath::easing::cubic_out,
                    );
                    let hover_t = ui.ctx().animate_bool_with_time_and_easing(
                        seg_id.with(("hover_fg", idx)),
                        item.hovered,
                        0.08,
                        egui::emath::easing::cubic_out,
                    );
                    let press_t = ui.ctx().animate_bool_with_time_and_easing(
                        seg_id.with(("press_fg", idx)),
                        item.pressed,
                        0.06,
                        egui::emath::easing::cubic_out,
                    );
                    let _ = &item.response;
                    let unselected_fg = lerp_color(pal.text_secondary, pal.text, hover_t);
                    let mut fg = lerp_color(unselected_fg, pal.on_accent, sel_t);
                    if !enabled {
                        fg = fg.gamma_multiply(0.5);
                    }
                    let pos = egui::pos2(
                        item.rect.center().x - item.galley.size().x * 0.5,
                        item.rect.center().y - item.galley.size().y * 0.5 + press_t * 0.7,
                    );
                    ui.painter().galley(pos, Arc::clone(&item.galley), fg);
                }
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{apply_theme, ThemeMode};

    #[test]
    fn test_widgets_render_and_click_headless() {
        let ctx = egui::Context::default();
        let pal = apply_theme(&ctx, ThemeMode::Dark, true);
        let mut toggle_on = false;
        let mut seg_val = 0usize;
        let mut card_val = 0usize;

        let mut click_pos = egui::Pos2::ZERO;

        // Frame 1: initial layout + capture button position
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let r1 = primary_button(ui, &pal, "Primary");
                click_pos = r1.rect.center();
                let _ = secondary_button(ui, &pal, "Secondary");
                let _ = danger_button(ui, &pal, "Danger");
                let _ = compact_button(ui, &pal, "Compact");
                let _ = nav_tab_button(ui, &pal, "Home", true);
                let _ = toggle_switch(ui, &pal, &mut toggle_on);
                let _ = segmented_control(ui, &pal, &mut seg_val, &[("One", 0), ("Two", 1)]);
                if selectable_card(ui, &pal, "card_0", card_val == 0, "Title", "Desc").clicked() {
                    card_val = 0;
                }
            });
        });

        // Frame 2: pointer down on primary button
        let input_down = egui::RawInput {
            events: vec![
                egui::Event::PointerMoved(click_pos),
                egui::Event::PointerButton {
                    pos: click_pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            ..Default::default()
        };
        let _ = ctx.run(input_down, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let r = primary_button(ui, &pal, "Primary");
                assert!(r.hovered());
                assert!(r.is_pointer_button_down_on());
            });
        });

        // Frame 3: pointer release -> click registered & pulse starts
        let input_up = egui::RawInput {
            events: vec![
                egui::Event::PointerMoved(click_pos),
                egui::Event::PointerButton {
                    pos: click_pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            ..Default::default()
        };
        let mut clicked = false;
        let _ = ctx.run(input_up, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let r = primary_button(ui, &pal, "Primary");
                clicked = r.clicked();
            });
        });
        assert!(clicked);
    }
}
