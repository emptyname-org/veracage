//! egui house style for the agent's transient windows: theme colors, rounded
//! widgets, roomy spacing, and a text scale derived from the configured base
//! point size (which defaults to the host desktop's - see `fonts`). No absolute
//! size is hardcoded; only the typographic ratios (heading vs body) are.

use eframe::egui;

/// Veracage accent (the turquoise of the app icon).
pub const ACCENT: egui::Color32 = egui::Color32::from_rgb(0x21, 0x9e, 0x96);
/// Error text (form validation, wrong passphrase).
pub const ERROR: egui::Color32 = egui::Color32::from_rgb(0xc0, 0x45, 0x45);

/// Full style pass for a one-shot window: font family + theme + sizes. Resolves
/// the base size once (may shell out to read the host size), so call per window,
/// not per frame - live dialogs (Settings) cache the base and call the pieces.
pub fn apply_config(ctx: &egui::Context, cfg: &crate::config::Config) {
    crate::fonts::install(ctx, &cfg.ui_font);
    apply_theme(ctx, &cfg.theme);
    set_text_sizes(ctx, crate::fonts::base_size(&cfg.ui_font, &cfg.ui_font_size));
}

/// Theme colors, widget rounding, and spacing (no fonts). Cheap to call per frame.
pub fn apply_theme(ctx: &egui::Context, theme: &str) {
    let dark = theme == "dark";
    let mut v = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };

    if dark {
        v.panel_fill = egui::Color32::from_rgb(0x20, 0x23, 0x25);
        v.window_fill = v.panel_fill;
        v.extreme_bg_color = egui::Color32::from_rgb(0x17, 0x1a, 0x1c);
        v.faint_bg_color = egui::Color32::from_rgb(0x2a, 0x2e, 0x31);
    } else {
        v.panel_fill = egui::Color32::from_rgb(0xfa, 0xfa, 0xf8);
        v.window_fill = v.panel_fill;
        v.extreme_bg_color = egui::Color32::WHITE;
        v.faint_bg_color = egui::Color32::from_rgb(0xf0, 0xf1, 0xef);
    }

    v.selection.bg_fill = ACCENT.gamma_multiply(if dark { 0.55 } else { 0.35 });
    v.selection.stroke = egui::Stroke::new(1.0, ACCENT);
    v.hyperlink_color = ACCENT;

    let r = egui::Rounding::same(6.0);
    v.widgets.noninteractive.rounding = r;
    v.widgets.inactive.rounding = r;
    v.widgets.hovered.rounding = r;
    v.widgets.active.rounding = r;
    v.widgets.open.rounding = r;
    v.window_rounding = egui::Rounding::same(10.0);

    v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, ACCENT.gamma_multiply(0.7));
    v.widgets.active.bg_stroke = egui::Stroke::new(1.0, ACCENT);

    ctx.set_visuals(v);
    ctx.style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(10.0, 10.0);
        s.spacing.button_padding = egui::vec2(14.0, 6.0);
        s.spacing.interact_size = egui::vec2(44.0, 26.0);
        s.spacing.icon_width = 22.0;
        s.spacing.icon_width_inner = 14.0;
        s.spacing.combo_width = 160.0;
    });
}

/// Set every text style from a base point size (the body size). Headings and the
/// small style scale from it, so nothing is an absolute hardcoded size. Cheap.
pub fn set_text_sizes(ctx: &egui::Context, base: f32) {
    use egui::{FontFamily::Monospace, FontFamily::Proportional, FontId, TextStyle};
    let base = base.clamp(6.0, 48.0);
    ctx.style_mut(|s| {
        s.text_styles.insert(TextStyle::Small, FontId::new((base * 0.85).round(), Proportional));
        s.text_styles.insert(TextStyle::Body, FontId::new(base, Proportional));
        s.text_styles.insert(TextStyle::Button, FontId::new(base, Proportional));
        s.text_styles.insert(TextStyle::Heading, FontId::new((base * 1.5).round(), Proportional));
        s.text_styles.insert(TextStyle::Monospace, FontId::new(base, Monospace));
    });
}

/// The house style for a dialog's panels: wide left/right margins.
pub fn content_frame(ctx: &egui::Context) -> egui::Frame {
    egui::Frame::central_panel(&ctx.style())
        .inner_margin(egui::Margin::symmetric(36.0, 18.0))
}

/// Roomier internal padding for the Save/Cancel buttons.
pub fn pad_buttons(ui: &mut egui::Ui) {
    ui.spacing_mut().button_padding = egui::vec2(16.0, 6.0);
}

/// The shared dialog action bar (bottom panel): Save / Cancel right-aligned,
/// with a caller-drawn `trailing` widget to their left (an error status, an
/// "N enabled" count, ...). Returns `(save_clicked, cancel_clicked)`.
pub fn action_bar(ctx: &egui::Context, trailing: impl FnOnce(&mut egui::Ui)) -> (bool, bool) {
    let (mut save, mut cancel) = (false, false);
    egui::TopBottomPanel::bottom("actions")
        .frame(content_frame(ctx))
        .show(ctx, |ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                pad_buttons(ui);
                cancel = ui.button("Cancel").clicked();
                save = ui.button("Save").clicked();
                trailing(ui);
            });
        });
    (save, cancel)
}
