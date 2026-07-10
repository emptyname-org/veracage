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
}
