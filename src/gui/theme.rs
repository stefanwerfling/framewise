//! Dark theme with layered surfaces, plus the small set of building blocks
//! every view uses (page header, cards, badges, stat tiles, buttons), so
//! the screens share one visual language.

use eframe::egui::{
    self, Color32, CornerRadius, FontFamily, FontId, Margin, RichText, Shadow, Stroke, TextStyle,
    Visuals,
};

// ---------------------------------------------------------------- palette

/// Central area behind the cards.
pub const BG: Color32 = Color32::from_rgb(15, 17, 21);
/// Sidebar, top and status bar.
pub const CHROME: Color32 = Color32::from_rgb(19, 21, 26);
/// Cards and dialogs.
pub const SURFACE: Color32 = Color32::from_rgb(24, 27, 33);
/// Inputs and other sunken areas.
pub const SUNKEN: Color32 = Color32::from_rgb(12, 13, 17);
pub const BORDER: Color32 = Color32::from_rgb(38, 42, 51);

pub const TEXT: Color32 = Color32::from_rgb(222, 226, 233);
pub const TEXT_STRONG: Color32 = Color32::from_rgb(246, 247, 250);
pub const MUTED: Color32 = Color32::from_rgb(138, 145, 158);

pub const ACCENT: Color32 = Color32::from_rgb(56, 182, 212);
/// Text on an accent fill.
pub const ON_ACCENT: Color32 = Color32::from_rgb(8, 22, 28);
pub const OK: Color32 = Color32::from_rgb(74, 196, 128);
pub const WARN: Color32 = Color32::from_rgb(240, 180, 64);
pub const ERROR: Color32 = Color32::from_rgb(240, 98, 98);
pub const HDR: Color32 = Color32::from_rgb(190, 140, 250);

const RADIUS: u8 = 6;
const CARD_RADIUS: u8 = 10;

pub fn apply(ctx: &egui::Context) {
    let mut v = Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = SURFACE;
    v.window_stroke = Stroke::new(1.0, BORDER);
    v.window_corner_radius = CornerRadius::same(CARD_RADIUS);
    v.window_shadow = Shadow {
        offset: [0, 8],
        blur: 28,
        spread: 0,
        color: Color32::from_black_alpha(110),
    };
    v.popup_shadow = Shadow {
        offset: [0, 4],
        blur: 14,
        spread: 0,
        color: Color32::from_black_alpha(90),
    };
    v.menu_corner_radius = CornerRadius::same(8);
    v.extreme_bg_color = SUNKEN;
    v.text_edit_bg_color = Some(SUNKEN);
    v.faint_bg_color = Color32::from_rgb(21, 24, 29);
    v.code_bg_color = SUNKEN;
    v.selection.bg_fill = ACCENT.gamma_multiply(0.32);
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.hyperlink_color = ACCENT;
    v.warn_fg_color = WARN;
    v.error_fg_color = ERROR;
    // tables opt in to stripes themselves; grids in cards stay calm
    v.striped = false;

    let w = &mut v.widgets;
    w.noninteractive.bg_fill = SURFACE;
    w.noninteractive.weak_bg_fill = SURFACE;
    w.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);

    w.inactive.bg_fill = Color32::from_rgb(34, 38, 46);
    w.inactive.weak_bg_fill = Color32::from_rgb(34, 38, 46);
    w.inactive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(46, 51, 61));
    w.inactive.fg_stroke = Stroke::new(1.0, TEXT);

    w.hovered.bg_fill = Color32::from_rgb(44, 49, 60);
    w.hovered.weak_bg_fill = Color32::from_rgb(44, 49, 60);
    w.hovered.bg_stroke = Stroke::new(1.0, ACCENT.gamma_multiply(0.6));
    w.hovered.fg_stroke = Stroke::new(1.5, TEXT_STRONG);

    w.active.bg_fill = Color32::from_rgb(52, 58, 71);
    w.active.weak_bg_fill = Color32::from_rgb(52, 58, 71);
    w.active.bg_stroke = Stroke::new(1.0, ACCENT);
    w.active.fg_stroke = Stroke::new(1.5, TEXT_STRONG);

    w.open.bg_fill = Color32::from_rgb(40, 45, 55);
    w.open.weak_bg_fill = Color32::from_rgb(40, 45, 55);
    w.open.bg_stroke = Stroke::new(1.0, ACCENT.gamma_multiply(0.6));
    w.open.fg_stroke = Stroke::new(1.0, TEXT_STRONG);

    for s in [
        &mut w.noninteractive,
        &mut w.inactive,
        &mut w.hovered,
        &mut w.active,
        &mut w.open,
    ] {
        s.corner_radius = CornerRadius::same(RADIUS);
    }
    ctx.set_visuals(v);

    ctx.global_style_mut(|style| {
        style.text_styles = [
            (
                TextStyle::Heading,
                FontId::new(21.0, FontFamily::Proportional),
            ),
            (TextStyle::Body, FontId::new(14.0, FontFamily::Proportional)),
            (
                TextStyle::Button,
                FontId::new(14.0, FontFamily::Proportional),
            ),
            (
                TextStyle::Small,
                FontId::new(11.5, FontFamily::Proportional),
            ),
            (
                TextStyle::Monospace,
                FontId::new(13.0, FontFamily::Monospace),
            ),
        ]
        .into();
        let sp = &mut style.spacing;
        sp.item_spacing = egui::vec2(8.0, 6.0);
        sp.button_padding = egui::vec2(10.0, 4.0);
        sp.interact_size.y = 24.0;
        sp.menu_margin = Margin::same(6);
        sp.window_margin = Margin::same(16);
        sp.combo_height = 320.0;
        sp.scroll.bar_width = 8.0;
        sp.scroll.floating = true;
        style.animation_time = 0.1;
    });
}

// ------------------------------------------------------------- components

/// Card for sections and tiles.
pub fn card() -> egui::Frame {
    egui::Frame::new()
        .fill(SURFACE)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(CARD_RADIUS))
        .inner_margin(Margin::same(14))
        .outer_margin(Margin {
            left: 0,
            right: 0,
            top: 0,
            bottom: 4,
        })
}

/// Title line of a card.
pub fn section_title(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).size(15.0).strong().color(TEXT_STRONG));
    ui.add_space(2.0);
}

/// Page title with an optional one-line description; `actions` go to the
/// right of the title.
pub fn page_header(
    ui: &mut egui::Ui,
    title: &str,
    subtitle: Option<&str>,
    actions: impl FnOnce(&mut egui::Ui),
) {
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(RichText::new(title).size(22.0).strong().color(TEXT_STRONG));
            if let Some(s) = subtitle {
                ui.label(RichText::new(s).color(MUTED));
            }
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), actions);
    });
    ui.add_space(8.0);
}

/// Small rounded label with a tinted background.
pub fn badge(ui: &mut egui::Ui, text: impl Into<String>, color: Color32) -> egui::Response {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.16))
        .stroke(Stroke::new(1.0, color.gamma_multiply(0.35)))
        .corner_radius(CornerRadius::same(9))
        .inner_margin(Margin::symmetric(7, 1))
        .show(ui, |ui| {
            ui.label(RichText::new(text.into()).small().strong().color(color));
        })
        .response
}

/// Button for the main action of a screen.
pub fn primary_button(text: impl Into<String>) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text.into()).strong().color(ON_ACCENT))
        .fill(ACCENT)
        .stroke(Stroke::NONE)
}

/// Button for a destructive action.
pub fn danger_button(text: impl Into<String>) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text.into()).strong().color(TEXT_STRONG))
        .fill(ERROR.gamma_multiply(0.75))
        .stroke(Stroke::NONE)
}

/// Key figure: label, large value, optional hint below.
pub fn stat_tile(
    ui: &mut egui::Ui,
    width: f32,
    title: &str,
    value: String,
    hint: Option<&str>,
    color: Color32,
) {
    card().show(ui, |ui| {
        ui.set_width(width - 30.0);
        ui.set_min_height(70.0);
        ui.vertical(|ui| {
            ui.label(RichText::new(title).color(MUTED));
            ui.label(RichText::new(value).size(26.0).strong().color(color));
            if let Some(h) = hint {
                ui.label(RichText::new(h).small().color(MUTED));
            }
        });
    });
}

/// Thin horizontal meter (status bar, hardware view).
pub fn meter(ui: &mut egui::Ui, width: f32, fraction: f32, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 6.0), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 3.0, BORDER);
    let mut fill = rect;
    fill.set_width(rect.width() * fraction.clamp(0.0, 1.0));
    painter.rect_filled(fill, 3.0, color);
}

/// Colour for a load fraction: calm, then warning, then critical.
pub fn load_color(fraction: f32) -> Color32 {
    if fraction > 0.9 {
        ERROR
    } else if fraction > 0.7 {
        WARN
    } else {
        ACCENT
    }
}
