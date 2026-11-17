//! SoundHop NUI Design System: brand-aligned dark & light palettes, typography scale,
//! and soft spacing/corner-radius styling for `egui`.

use eframe::egui::{self, Color32, CornerRadius, Margin, Shadow, Stroke, Visuals};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeMode {
    Dark,
    Light,
    System,
}

impl ThemeMode {
    pub fn from_str_loose(raw: &str) -> Self {
        match raw.trim().to_lowercase().as_str() {
            "light" => Self::Light,
            "system" | "auto" => Self::System,
            _ => Self::Dark,
        }
    }

    pub fn as_config_str(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
            Self::System => "system",
        }
    }

    pub fn is_dark(self, ctx: &egui::Context) -> bool {
        match self {
            Self::Dark => true,
            Self::Light => false,
            Self::System => ctx.style().visuals.dark_mode,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub dark: bool,
    pub canvas: Color32,
    pub sidebar: Color32,
    pub card: Color32,
    pub card_elevated: Color32,
    pub card_hover: Color32,
    pub border: Color32,
    pub border_subtle: Color32,
    pub text: Color32,
    pub text_secondary: Color32,
    pub text_muted: Color32,
    pub accent: Color32,
    pub accent_hover: Color32,
    pub accent_soft: Color32,
    pub on_accent: Color32,
    pub success: Color32,
    pub success_soft: Color32,
    pub multiroom: Color32,
    pub multiroom_soft: Color32,
    pub warning: Color32,
    pub warning_soft: Color32,
    pub danger: Color32,
    pub danger_soft: Color32,
    pub switch_off_bg: Color32,
    pub switch_knob: Color32,
}

impl Palette {
    pub fn dark() -> Self {
        Self {
            dark: true,
            canvas: Color32::from_rgb(15, 20, 27),
            sidebar: Color32::from_rgb(19, 25, 34),
            card: Color32::from_rgb(24, 32, 44),
            card_elevated: Color32::from_rgb(30, 40, 54),
            card_hover: Color32::from_rgb(34, 46, 62),
            border: Color32::from_rgb(44, 58, 78),
            border_subtle: Color32::from_rgb(33, 44, 60),
            text: Color32::from_rgb(241, 245, 249),
            text_secondary: Color32::from_rgb(186, 198, 214),
            text_muted: Color32::from_rgb(132, 149, 172),
            accent: Color32::from_rgb(61, 174, 233),
            accent_hover: Color32::from_rgb(90, 192, 242),
            accent_soft: Color32::from_rgba_unmultiplied(61, 174, 233, 32),
            on_accent: Color32::from_rgb(9, 17, 26),
            success: Color32::from_rgb(126, 231, 135),
            success_soft: Color32::from_rgba_unmultiplied(126, 231, 135, 28),
            multiroom: Color32::from_rgb(167, 139, 250),
            multiroom_soft: Color32::from_rgba_unmultiplied(167, 139, 250, 30),
            warning: Color32::from_rgb(245, 158, 11),
            warning_soft: Color32::from_rgba_unmultiplied(245, 158, 11, 28),
            danger: Color32::from_rgb(248, 113, 113),
            danger_soft: Color32::from_rgba_unmultiplied(248, 113, 113, 28),
            switch_off_bg: Color32::from_rgb(52, 66, 86),
            switch_knob: Color32::from_rgb(248, 250, 252),
        }
    }

    pub fn light() -> Self {
        Self {
            dark: false,
            canvas: Color32::from_rgb(244, 247, 251),
            sidebar: Color32::from_rgb(233, 239, 247),
            card: Color32::from_rgb(255, 255, 255),
            card_elevated: Color32::from_rgb(248, 250, 253),
            card_hover: Color32::from_rgb(238, 244, 252),
            border: Color32::from_rgb(212, 222, 235),
            border_subtle: Color32::from_rgb(228, 235, 244),
            text: Color32::from_rgb(15, 23, 42),
            text_secondary: Color32::from_rgb(51, 65, 85),
            text_muted: Color32::from_rgb(100, 116, 139),
            accent: Color32::from_rgb(29, 127, 176),
            accent_hover: Color32::from_rgb(42, 148, 201),
            accent_soft: Color32::from_rgba_unmultiplied(29, 127, 176, 26),
            on_accent: Color32::from_rgb(255, 255, 255),
            success: Color32::from_rgb(22, 163, 74),
            success_soft: Color32::from_rgba_unmultiplied(22, 163, 74, 24),
            multiroom: Color32::from_rgb(124, 58, 237),
            multiroom_soft: Color32::from_rgba_unmultiplied(124, 58, 237, 22),
            warning: Color32::from_rgb(217, 119, 6),
            warning_soft: Color32::from_rgba_unmultiplied(217, 119, 6, 24),
            danger: Color32::from_rgb(220, 38, 38),
            danger_soft: Color32::from_rgba_unmultiplied(220, 38, 38, 22),
            switch_off_bg: Color32::from_rgb(203, 213, 225),
            switch_knob: Color32::from_rgb(255, 255, 255),
        }
    }

    pub fn for_mode(mode: ThemeMode, system_dark: bool) -> Self {
        let use_dark = match mode {
            ThemeMode::Dark => true,
            ThemeMode::Light => false,
            ThemeMode::System => system_dark,
        };
        if use_dark {
            Self::dark()
        } else {
            Self::light()
        }
    }
}

/// Apply the SoundHop NUI theme to `egui::Context` and return the active `Palette`.
pub fn apply_theme(ctx: &egui::Context, mode: ThemeMode, system_dark: bool) -> Palette {
    let pal = Palette::for_mode(mode, system_dark);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(10.0, 10.0);
    style.spacing.button_padding = egui::vec2(14.0, 8.0);
    style.spacing.interact_size = egui::vec2(40.0, 30.0);
    style.spacing.window_margin = Margin::same(16);
    style.spacing.slider_width = 180.0;

    let mut visuals = if pal.dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };

    visuals.window_fill = pal.canvas;
    visuals.panel_fill = pal.canvas;
    visuals.extreme_bg_color = if pal.dark {
        Color32::from_rgb(12, 16, 22)
    } else {
        Color32::from_rgb(239, 243, 249)
    };
    visuals.faint_bg_color = pal.card_elevated;
    visuals.code_bg_color = pal.card_elevated;
    visuals.window_corner_radius = CornerRadius::same(12);
    visuals.menu_corner_radius = CornerRadius::same(10);
    visuals.window_shadow = Shadow {
        offset: [0, 8],
        blur: 24,
        spread: 0,
        color: Color32::from_rgba_unmultiplied(0, 0, 0, if pal.dark { 90 } else { 30 }),
    };

    visuals.selection.bg_fill = pal.accent;
    visuals.selection.stroke = Stroke::new(1.0_f32, pal.on_accent);
    visuals.override_text_color = Some(pal.text);

    // Noninteractive widgets
    visuals.widgets.noninteractive.bg_fill = pal.card;
    visuals.widgets.noninteractive.weak_bg_fill = pal.card;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, pal.border_subtle);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, pal.text);
    visuals.widgets.noninteractive.corner_radius = CornerRadius::same(8);

    // Inactive interactive widgets
    visuals.widgets.inactive.bg_fill = pal.card_elevated;
    visuals.widgets.inactive.weak_bg_fill = pal.card_elevated;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, pal.border);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, pal.text);
    visuals.widgets.inactive.corner_radius = CornerRadius::same(8);

    // Hovered widgets
    visuals.widgets.hovered.bg_fill = pal.card_hover;
    visuals.widgets.hovered.weak_bg_fill = pal.card_hover;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.2_f32, pal.accent);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, pal.text);
    visuals.widgets.hovered.corner_radius = CornerRadius::same(8);

    // Active / pressed widgets
    visuals.widgets.active.bg_fill = pal.accent_soft;
    visuals.widgets.active.weak_bg_fill = pal.accent_soft;
    visuals.widgets.active.bg_stroke = Stroke::new(1.5_f32, pal.accent);
    visuals.widgets.active.fg_stroke = Stroke::new(1.0_f32, pal.text);
    visuals.widgets.active.corner_radius = CornerRadius::same(8);

    // Open combos/menus
    visuals.widgets.open.bg_fill = pal.card_hover;
    visuals.widgets.open.weak_bg_fill = pal.card_hover;
    visuals.widgets.open.bg_stroke = Stroke::new(1.2_f32, pal.accent);
    visuals.widgets.open.fg_stroke = Stroke::new(1.0_f32, pal.text);
    visuals.widgets.open.corner_radius = CornerRadius::same(8);

    style.visuals = visuals;
    ctx.set_style(style);
    pal
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_theme_mode_parsing_and_palettes() {
        assert_eq!(ThemeMode::from_str_loose("light"), ThemeMode::Light);
        assert_eq!(ThemeMode::from_str_loose("SYSTEM"), ThemeMode::System);
        assert_eq!(ThemeMode::from_str_loose("unknown"), ThemeMode::Dark);

        let dark = Palette::for_mode(ThemeMode::Dark, false);
        let light = Palette::for_mode(ThemeMode::Light, true);
        let sys_light = Palette::for_mode(ThemeMode::System, false);
        assert!(dark.dark);
        assert!(!light.dark);
        assert!(!sys_light.dark);
        assert_ne!(dark.canvas, light.canvas);
    }
}
