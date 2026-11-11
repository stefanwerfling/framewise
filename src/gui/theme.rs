//! Dark, information-dense theme.

use eframe::egui::{self, Color32, CornerRadius, Stroke, Visuals};

pub const ACCENT: Color32 = Color32::from_rgb(64, 170, 190);
pub const OK: Color32 = Color32::from_rgb(90, 190, 120);
pub const WARN: Color32 = Color32::from_rgb(230, 180, 70);
pub const ERROR: Color32 = Color32::from_rgb(225, 90, 90);
pub const MUTED: Color32 = Color32::from_rgb(140, 145, 155);
pub const HDR: Color32 = Color32::from_rgb(200, 140, 240);

pub fn apply(ctx: &egui::Context) {
    let mut visuals = Visuals::dark();
    visuals.panel_fill = Color32::from_rgb(24, 26, 30);
    visuals.window_fill = Color32::from_rgb(30, 33, 38);
    visuals.extreme_bg_color = Color32::from_rgb(18, 19, 22);
    visuals.faint_bg_color = Color32::from_rgb(32, 35, 41);
    visuals.selection.bg_fill = ACCENT.linear_multiply(0.45);
    visuals.selection.stroke = Stroke::new(1.0, ACCENT);
    visuals.hyperlink_color = ACCENT;
    visuals.widgets.noninteractive.corner_radius = CornerRadius::same(4);
    visuals.widgets.inactive.corner_radius = CornerRadius::same(4);
    visuals.widgets.hovered.corner_radius = CornerRadius::same(4);
    visuals.widgets.active.corner_radius = CornerRadius::same(4);
    visuals.window_corner_radius = CornerRadius::same(6);
    visuals.striped = true;
    ctx.set_visuals(visuals);

    ctx.global_style_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 5.0);
        style.spacing.button_padding = egui::vec2(8.0, 3.0);
        style.animation_time = 0.08;
    });
}

/// Card-like frame for dashboard tiles and sections.
pub fn card() -> egui::Frame {
    egui::Frame::new()
        .fill(Color32::from_rgb(32, 35, 41))
        .stroke(Stroke::new(1.0, Color32::from_rgb(48, 52, 60)))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(egui::Margin::same(10))
}
