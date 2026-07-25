//! One-shot settings dialog: `veracage-agent _settings`.
//!
//! A fresh process (winit can't reopen an EventLoop) that edits the general
//! settings in `~/.config/veracage/config.toml`: theme, window size, font,
//! host-clipboard auto-clear, the shared directory, and the suspend action.
//! Spawned by the broker when the compositor's Settings menu is used. Layout:
//! the appearance rows share one grid (so they align in columns), a separator,
//! then the clipboard-clear + suspend grid, then the shared-directory checkbox,
//! Save/Cancel bottom-right.

use eframe::egui;

use crate::config;

pub fn run() -> Result<(), eframe::Error> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Veracage Settings")
            .with_app_id("veracage")
            .with_inner_size([620.0, 620.0])
            .with_min_inner_size([520.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "veracage-settings",
        options,
        Box::new(|_cc| Ok(Box::new(Settings::new()) as Box<dyn eframe::App>)),
    )
}

struct Settings {
    cfg: config::Config,
    exchange_dir: String,
    status: String,
    /// The font/size currently applied to the egui context. Re-resolve only when
    /// the selection changes: `set_fonts` rebuilds the atlas, and resolving the
    /// base size may shell out to read the host size - neither should run per
    /// frame. `base` caches the resolved point size.
    applied_font: Option<String>,
    applied_size: Option<String>,
    base: f32,
    /// The host desktop's (family, point size), resolved once so the "Host
    /// system ..." dropdown entries can show the concrete values.
    host_font: (Option<String>, Option<f32>),
}

impl Settings {
    fn new() -> Self {
        let cfg = config::load();
        let exchange_dir = cfg
            .exchange_dir
            .clone()
            .unwrap_or_else(default_exchange_dir);
        Self {
            cfg,
            exchange_dir,
            status: String::new(),
            applied_font: None,
            applied_size: None,
            base: crate::fonts::FALLBACK_SIZE,
            host_font: crate::fonts::host_ui_font(),
        }
    }
}

fn default_exchange_dir() -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    format!("{home}/Veracage/Exchange")
}

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

/// Host-clipboard auto-clear timeout presets, in seconds. A hand-edited config
/// value outside this list still shows verbatim ("<n> seconds") in the dropdown.
const CLIP_TIMEOUTS: &[u32] = &[10, 15, 20, 30, 45, 60, 90, 120];

impl eframe::App for Settings {
    fn update(&mut self, ctx: &egui::Context, _f: &mut eframe::Frame) {
        // Live theme every frame (cheap). Font only when it changes, so switching
        // to the host default actually reverts and we don't rebuild the atlas per
        // frame. The base size depends on BOTH the size key and the font (the
        // hinting correction is per font file), so recompute on either change.
        let font_changed = self.applied_font.as_deref() != Some(self.cfg.ui_font.as_str());
        let size_changed = self.applied_size.as_deref() != Some(self.cfg.ui_font_size.as_str());
        if font_changed {
            crate::fonts::install(ctx, &self.cfg.ui_font);
            self.applied_font = Some(self.cfg.ui_font.clone());
        }
        if font_changed || size_changed {
            self.base = crate::fonts::base_size(&self.cfg.ui_font, &self.cfg.ui_font_size);
            self.applied_size = Some(self.cfg.ui_font_size.clone());
        }
        crate::theme::apply_theme(ctx, &self.cfg.theme);
        crate::theme::set_text_sizes(ctx, self.base);

        // Save / Cancel pinned bottom-right (design), with the error status.
        let status = &self.status;
        let (do_save, do_cancel) = crate::theme::action_bar(ctx, |ui| {
            if !status.is_empty() {
                ui.colored_label(crate::theme::ERROR, status);
            }
        });

        crate::theme::content_panel(ctx, |ui| {
            ui.add_space(6.0);

            // One label+dropdown pair per grid row, so every label and every
            // dropdown lines up in its column (and centers vertically).
            egui::Grid::new("general")
                .num_columns(2)
                .spacing([24.0, 18.0])
                .show(ui, |ui| {
                    ui.label("Theme:");
                    egui::ComboBox::from_id_salt("theme")
                        .selected_text(match self.cfg.theme.as_str() {
                            "dark" => "Dark",
                            _ => "Light",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.cfg.theme, "light".into(), "Light");
                            ui.selectable_value(&mut self.cfg.theme, "dark".into(), "Dark");
                        });
                    ui.end_row();

                    ui.label("Window size:");
                    egui::ComboBox::from_id_salt("window_size")
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
                    ui.end_row();

                    // The "Host system ..." entries show what the host actually
                    // uses, so the default is a concrete choice, not a mystery.
                    let host_font_label = match &self.host_font.0 {
                        Some(f) => format!("Host system font ({f})"),
                        None => "Host system font".to_string(),
                    };
                    let host_size_label = match self.host_font.1 {
                        Some(pt) => format!("Host system size ({} pt)", pt.round() as u32),
                        None => "Host system size".to_string(),
                    };

                    ui.label("Font:");
                    let cur = crate::fonts::CHOICES
                        .iter()
                        .find(|(k, _)| *k == self.cfg.ui_font)
                        .map(|(_, l)| *l)
                        .unwrap_or("Host system font");
                    egui::ComboBox::from_id_salt("ui_font")
                        .selected_text(cur)
                        .show_ui(ui, |ui| {
                            for (key, label) in crate::fonts::CHOICES {
                                let label = if *key == "system" {
                                    host_font_label.clone()
                                } else {
                                    (*label).to_string()
                                };
                                ui.selectable_value(
                                    &mut self.cfg.ui_font,
                                    (*key).to_string(),
                                    label,
                                );
                            }
                        });
                    ui.end_row();

                    ui.label("Font size:");
                    let cur = crate::fonts::SIZE_CHOICES
                        .iter()
                        .find(|(v, _)| *v == self.cfg.ui_font_size)
                        .map(|(_, l)| *l)
                        .unwrap_or("Host system size");
                    egui::ComboBox::from_id_salt("ui_font_size")
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
                    ui.end_row();
                });

            // Horizontal rule, then the clipboard-clear + suspend settings in
            // their own aligned grid.
            ui.add_space(16.0);
            ui.separator();
            ui.add_space(16.0);

            egui::Grid::new("clipboard")
                .num_columns(2)
                .spacing([24.0, 18.0])
                .show(ui, |ui| {
                    // Auto-clear the host clipboard some seconds after a Copy out
                    // (sensitive text pushed sandbox -> host). Copying inside the
                    // sandbox never arms it; the compositor enforces both.
                    ui.label("Clear host clipboard:");
                    egui::ComboBox::from_id_salt("clip_clear")
                        .selected_text(if self.cfg.clip_clear { "On" } else { "Off" })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.cfg.clip_clear, true, "On");
                            ui.selectable_value(&mut self.cfg.clip_clear, false, "Off");
                        });
                    ui.end_row();

                    // The timeout is meaningless with clearing off, so grey it out.
                    ui.label("Clear after:");
                    ui.add_enabled_ui(self.cfg.clip_clear, |ui| {
                        egui::ComboBox::from_id_salt("clip_clear_timeout")
                            .selected_text(format!("{} seconds", self.cfg.clip_clear_timeout))
                            .show_ui(ui, |ui| {
                                for &secs in CLIP_TIMEOUTS {
                                    ui.selectable_value(
                                        &mut self.cfg.clip_clear_timeout,
                                        secs,
                                        format!("{secs} seconds"),
                                    );
                                }
                            });
                    });
                    ui.end_row();

                    ui.label("On system suspend:");
                    egui::ComboBox::from_id_salt("suspend")
                        .selected_text(match self.cfg.suspend_action.as_str() {
                            "ignore" => "Leave mounted",
                            _ => "Unmount (recommended)",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.cfg.suspend_action,
                                "dismount".into(),
                                "Unmount (recommended)",
                            );
                            ui.selectable_value(
                                &mut self.cfg.suspend_action,
                                "ignore".into(),
                                "Leave mounted",
                            );
                        });
                    ui.end_row();
                });
            ui.add_space(20.0);

            // Shared directory: checkbox, then an indented full-width path field.
            ui.checkbox(&mut self.cfg.exchange, "Shared directory");
            ui.add_enabled_ui(self.cfg.exchange, |ui| {
                ui.horizontal(|ui| {
                    ui.add_space(24.0);
                    ui.add(
                        egui::TextEdit::singleline(&mut self.exchange_dir)
                            .desired_width(f32::INFINITY),
                    );
                });
            });
        });

        if do_cancel {
            std::process::exit(0);
        }
        if do_save {
            let d = self.exchange_dir.trim();
            self.cfg.exchange_dir = (!d.is_empty()).then(|| d.to_string());
            match config::save(&self.cfg) {
                Ok(_) => {
                    // Push the window size + font to the compositor so they take
                    // effect now, not only on next launch (the broker also
                    // republishes on the config change; this is immediate).
                    crate::broker::publish_window_size(&self.cfg.window_size);
                    crate::broker::publish_font(&self.cfg);
                    crate::broker::publish_clipclear(&self.cfg);
                    std::process::exit(0);
                }
                Err(e) => self.status = format!("Save failed: {e}"),
            }
        }
    }
}
