//! egui theme for the agent's transient windows. Default LIGHT (the compositor
//! matches via `VERACAGE_THEME`); `dark` opts into dark. `system` (follow the
//! desktop color-scheme) is not wired yet — treated as light for now.

use eframe::egui;

pub fn apply(ctx: &egui::Context, theme: &str) {
    let dark = theme == "dark";
    ctx.set_visuals(if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    });
    bump_fonts(ctx);
}

/// The house style for a dialog's panels: wide left/right margins (matches the
/// settings design). Use for BOTH the central panel and the bottom button bar so
/// content and buttons share the same side margins.
pub fn content_frame(ctx: &egui::Context) -> egui::Frame {
    egui::Frame::central_panel(&ctx.style())
        .inner_margin(egui::Margin::symmetric(36.0, 18.0))
}

/// Roomier internal padding for the Save/Cancel buttons (left/right breathing
/// room). Call inside the button row: `theme::pad_buttons(ui)`.
pub fn pad_buttons(ui: &mut egui::Ui) {
    ui.spacing_mut().button_padding = egui::vec2(16.0, 6.0);
}

/// Enlarge every text style ~50% (egui's defaults are ~9/12.5/18; a 12 becomes an
/// 18). Absolute sizes, so it is safe to call every frame (multiplying would
/// compound). Applied everywhere `apply` runs, so no window is left small.
pub fn bump_fonts(ctx: &egui::Context) {
    use egui::{FontFamily::Monospace, FontFamily::Proportional, FontId, TextStyle};
    ctx.style_mut(|s| {
        s.text_styles.insert(TextStyle::Small, FontId::new(14.0, Proportional));
        s.text_styles.insert(TextStyle::Body, FontId::new(18.0, Proportional));
        s.text_styles.insert(TextStyle::Button, FontId::new(18.0, Proportional));
        s.text_styles.insert(TextStyle::Heading, FontId::new(28.0, Proportional));
        s.text_styles.insert(TextStyle::Monospace, FontId::new(18.0, Monospace));
    });
}
