use super::tokens::UiTokens;

use egui::{Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Stroke, Vec2};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeKind {
    RiverDark,
    RiverLight,
    Unknown,
}

pub fn theme_kind(id: &riverdeck_core::store::ThemeId) -> ThemeKind {
    match id.0.as_str() {
        riverdeck_core::store::ThemeId::RIVER_DARK_ID => ThemeKind::RiverDark,
        riverdeck_core::store::ThemeId::RIVER_LIGHT_ID => ThemeKind::RiverLight,
        _ => ThemeKind::Unknown,
    }
}

fn install_fonts(ctx: &egui::Context) {
    // Safe to call multiple times; egui will just replace the definitions.
    let mut fonts = FontDefinitions::default();

    // Bundled modern UI font for a polished look across platforms.
    let bytes = include_bytes!("../../assets/fonts/Inter.ttf").to_vec();
    fonts
        .font_data
        .insert("Inter".to_owned(), FontData::from_owned(bytes).into());
    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, "Inter".to_owned());

    ctx.set_fonts(fonts);
}

pub fn apply_theme(ctx: &egui::Context, theme_id: &riverdeck_core::store::ThemeId) {
    install_fonts(ctx);

    let tokens = UiTokens::modern_neutral();
    let kind = theme_kind(theme_id);

    ctx.style_mut(|s| {
        s.spacing.item_spacing = Vec2::new(tokens.item_spacing_x, tokens.item_spacing_y);
        s.spacing.button_padding = Vec2::new(tokens.button_pad_x, tokens.button_pad_y);
        s.spacing.window_margin = egui::Margin::same(tokens.window_margin);

        // Typography scale: egui defaults skew small; bump slightly for a “designed” feel.
        s.text_styles.insert(egui::TextStyle::Heading, FontId::proportional(20.0));
        s.text_styles
            .insert(egui::TextStyle::Body, FontId::proportional(14.0));
        s.text_styles
            .insert(egui::TextStyle::Monospace, FontId::monospace(12.0));
        s.text_styles
            .insert(egui::TextStyle::Button, FontId::proportional(14.0));
        s.text_styles
            .insert(egui::TextStyle::Small, FontId::proportional(11.5));

        let mut v = match kind {
            ThemeKind::RiverLight => egui::Visuals::light(),
            _ => egui::Visuals::dark(),
        };

        match kind {
            ThemeKind::RiverLight => {
                // Modern neutral light: avoid pure white; keep subtle depth.
                v.window_fill = Color32::from_rgb(246, 247, 249);
                v.panel_fill = Color32::from_rgb(241, 242, 245);
                v.extreme_bg_color = Color32::from_rgb(255, 255, 255);
                v.faint_bg_color = Color32::from_rgb(235, 237, 240);

                v.widgets.inactive.bg_fill = Color32::from_rgb(255, 255, 255);
                v.widgets.hovered.bg_fill = Color32::from_rgb(250, 251, 252);
                v.widgets.active.bg_fill = Color32::from_rgb(245, 246, 248);

                v.widgets.inactive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(206, 210, 216));
                v.widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(176, 182, 190));
                v.widgets.active.bg_stroke = Stroke::new(1.0, Color32::from_rgb(140, 147, 157));

                v.selection.bg_fill = Color32::TRANSPARENT;
                v.selection.stroke = Stroke::new(2.0, Color32::from_rgb(0, 120, 255));
                v.error_fg_color = Color32::from_rgb(200, 40, 40);
            }
            _ => {
                // River Dark: tuned toward a modern neutral dark with a restrained blue accent.
                v.window_fill = Color32::from_rgb(24, 25, 27);
                v.panel_fill = Color32::from_rgb(28, 29, 31);
                v.extreme_bg_color = Color32::from_rgb(20, 21, 23);
                v.faint_bg_color = Color32::from_rgb(34, 35, 38);

                v.widgets.inactive.bg_fill = Color32::from_rgb(33, 34, 37);
                v.widgets.hovered.bg_fill = Color32::from_rgb(41, 42, 46);
                v.widgets.active.bg_fill = Color32::from_rgb(48, 50, 55);

                v.widgets.inactive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(56, 58, 62));
                v.widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(82, 85, 90));
                v.widgets.active.bg_stroke = Stroke::new(1.0, Color32::from_rgb(110, 114, 122));

                v.selection.bg_fill = Color32::TRANSPARENT;
                v.selection.stroke = Stroke::new(2.0, Color32::from_rgb(0, 145, 255));
                v.error_fg_color = Color32::from_rgb(255, 95, 95);
            }
        }

        v.window_corner_radius = CornerRadius::same(tokens.radius_window);
        v.menu_corner_radius = CornerRadius::same(10);
        s.visuals = v;
    });
}


