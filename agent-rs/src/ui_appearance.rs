//! Appearance: `veracage-agent _appearance` (Settings > Appearance).
//!
//! How Veracage LOOKS: theme, the compositor window's default size, and the UI
//! font. A separate one-shot window from Settings, which keeps the session
//! settings (clipboard clearing, idle dismount, suspend, the shared directory).
//! The theme and font apply to this window live, so a choice is visible before
//! it is saved.

use eframe::egui;

use crate::config;

pub fn run() -> Result<(), eframe::Error> {
    crate::theme::dialog("veracage-appearance", "Veracage Appearance", [660.0, 420.0], [560.0, 360.0], |cc| {
        let _ = cc;
        Box::new(Appearance::new())
    })
}

/// The labels here, so the control column starts past the widest of them.
const LABELS: &[&str] = &["Theme:", "Window size:", "Font:", "Font size:"];

/// Shown beside the Theme dropdown: the window follows a theme change at once,
/// an app only reads it when it starts.
const THEME_NOTE: &str = "Restart apps to apply";

/// Shown beside the Window size dropdown. The size is applied when the Veracage
/// window is CREATED, not to the running one: resizing the nested output mid
/// session leaves the menu bar laid out for the old width, so the strip is drawn
/// and hit-tested in different places (docs/known-problems.md).
const SIZE_NOTE: &str = "Applies at next start";

/// The widest text a dropdown is expected to show; longer selections truncate
/// rather than stretch their row.
const CONTROL_SAMPLES: &[&str] = &["Host system font (Noto Sans)", "Host system size (12 pt)"];

/// Default-window-size choices: (stored value, menu label).
const WINDOW_SIZES: &[(&str, &str)] = &[
    ("default", "Default"),
    ("1024x680", "1024 x 680"),
    ("1280x800", "1280 x 800"),
    ("1600x1000", "1600 x 1000"),
    ("max", "Maximized"),
];

fn window_size_label(val: &str) -> &'static str {
    WINDOW_SIZES
        .iter()
        .find(|(v, _)| *v == val)
        .map(|(_, l)| *l)
        .unwrap_or("Default")
}

struct Appearance {
    cfg: config::Config,
    status: String,
    /// Font + theme applied live, so a choice is visible before it is saved.
    style: crate::theme::LiveStyle,
    /// The host desktop's (family, point size), resolved once so the "Host
    /// system ..." entries can show the concrete values.
    host_font: (Option<String>, Option<f32>),
    /// One "window up" debug line per dialog, not one per frame.
    first_frame_logged: bool,
}

impl Appearance {
    fn new() -> Self {
        Self {
            cfg: config::load(),
            status: String::new(),
            style: crate::theme::LiveStyle::default(),
            host_font: crate::fonts::host_ui_font(),
            first_frame_logged: false,
        }
    }
}

impl eframe::App for Appearance {
    fn update(&mut self, ctx: &egui::Context, _f: &mut eframe::Frame) {
        self.style.apply(ctx, &self.cfg);
        if !self.first_frame_logged {
            self.first_frame_logged = true;
            crate::broker::debug_log(&format!(
                "appearance: window up, focused={}",
                ctx.input(|i| i.focused)
            ));
        }

        let status = &self.status;
        let (do_save, do_cancel) = crate::theme::action_bar(ctx, |ui| {
            if !status.is_empty() {
                ui.colored_label(crate::theme::ERROR, status);
            }
        });

        crate::theme::content_panel(ctx, |ui| {
            let col = crate::theme::text_width(ui, LABELS) + 24.0;
            // The Theme row carries a note to the right of its dropdown, so the
            // controls are only as wide as what is left after the label column
            // and that note. Long selections truncate rather than push it out of
            // the window.
            let note = crate::theme::text_width(ui, &[THEME_NOTE, SIZE_NOTE]) + 16.0;
            let natural = crate::theme::text_width(ui, CONTROL_SAMPLES) + 44.0;
            let ctrl = natural.min((ui.available_width() - col - note).max(140.0));

                // The Veracage window follows a theme change at once, an app only
                // reads it when it starts, so say so where the choice is made rather
                // than leaving it to look broken.
                crate::theme::row(ui, col, "Theme:", |ui| {
                    egui::ComboBox::from_id_salt("theme")
                        .width(ctrl)
                        .truncate()
                        .selected_text(match self.cfg.theme.as_str() {
                            "dark" => "Dark",
                            "light" => "Light",
                            _ => "Follow the Host",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.cfg.theme, "system".into(),
                                                "Follow the Host");
                            ui.selectable_value(&mut self.cfg.theme, "light".into(), "Light");
                            ui.selectable_value(&mut self.cfg.theme, "dark".into(), "Dark");
                        });
                    // Beside the dropdown: the controls all end at the same x,
                    // so the note starts there and lines up down the dialog.
                    ui.label(egui::RichText::new(THEME_NOTE).weak());
                });

                crate::theme::row(ui, col, "Window size:", |ui| {
                    egui::ComboBox::from_id_salt("window_size")
                        .width(ctrl)
                        .truncate()
                        .selected_text(window_size_label(&self.cfg.window_size))
                        .show_ui(ui, |ui| {
                            for (val, label) in WINDOW_SIZES {
                                ui.selectable_value(
                                    &mut self.cfg.window_size,
                                    (*val).to_string(),
                                    *label,
                                );
                            }
                        });
                    ui.label(egui::RichText::new(SIZE_NOTE).weak());
                });

                // The "Host system ..." entries show what the host actually uses, so
                // the default is a concrete choice, not a mystery.
                let host_font_label = match &self.host_font.0 {
                    Some(f) => format!("Host system font ({f})"),
                    None => "Host system font".to_string(),
                };
                let host_size_label = match self.host_font.1 {
                    Some(pt) => format!("Host system size ({} pt)", pt.round() as u32),
                    None => "Host system size".to_string(),
                };

                crate::theme::row(ui, col, "Font:", |ui| {
                    let cur = crate::fonts::CHOICES
                        .iter()
                        .find(|(k, _)| *k == self.cfg.ui_font)
                        .map(|(_, l)| *l)
                        .unwrap_or("Host system font");
                    egui::ComboBox::from_id_salt("ui_font")
                        .width(ctrl)
                        .truncate()
                        .selected_text(cur)
                        .show_ui(ui, |ui| {
                            for (key, label) in crate::fonts::CHOICES {
                                let label = if *key == "system" {
                                    host_font_label.clone()
                                } else {
                                    (*label).to_string()
                                };
                                ui.selectable_value(&mut self.cfg.ui_font, (*key).to_string(), label);
                            }
                        });
                });

                crate::theme::row(ui, col, "Font size:", |ui| {
                    let cur = crate::fonts::SIZE_CHOICES
                        .iter()
                        .find(|(v, _)| *v == self.cfg.ui_font_size)
                        .map(|(_, l)| *l)
                        .unwrap_or("Host system size");
                    egui::ComboBox::from_id_salt("ui_font_size")
                        .width(ctrl)
                        .truncate()
                        .selected_text(cur)
                        .show_ui(ui, |ui| {
                            for (val, label) in crate::fonts::SIZE_CHOICES {
                                let label = if *val == "system" {
                                    host_size_label.clone()
                                } else {
                                    (*label).to_string()
                                };
                                ui.selectable_value(
                                    &mut self.cfg.ui_font_size,
                                    (*val).to_string(),
                                    label,
                                );
                            }
                        });
                });
        });

        if do_cancel {
            crate::broker::debug_log("appearance: cancel, exiting without saving");
            std::process::exit(0);
        }
        if do_save {
            crate::broker::debug_log("appearance: save requested");
            match config::save(&self.cfg) {
                Ok(_) => {
                    // Push to the compositor so the change lands now, not only on
                    // the next launch (the broker republishes on the config change
                    // too; this is immediate).
                    crate::broker::publish_font(&self.cfg);
                    crate::broker::publish_theme(&self.cfg);
                    crate::broker::publish_appfont(&self.cfg);
                    crate::broker::debug_log("appearance: saved and published, exiting");
                    std::process::exit(0);
                }
                Err(e) => {
                    crate::broker::debug_log(&format!("appearance: save FAILED: {e}"));
                    self.status = format!("Save failed: {e}");
                }
            }
        }
    }
}
