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
