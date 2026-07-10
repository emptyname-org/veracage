//! One-shot settings dialog: `veracage-agent _settings`.
//!
//! A fresh process (winit can't reopen an EventLoop) that edits the general
//! settings in `~/.config/veracage/config.toml` — theme, GPU passthrough, the
//! suspend action, and the shared Exchange folder. Spawned by the broker when the
//! compositor's Settings menu is used.

use eframe::egui;

use crate::config;

pub fn run() -> Result<(), eframe::Error> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Veracage — settings")
            .with_app_id("veracage")
            .with_inner_size([440.0, 300.0])
            .with_resizable(false),
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
}

impl Settings {
    fn new() -> Self {
        let cfg = config::load();
        let exchange_dir = cfg
            .exchange_dir
            .clone()
            .unwrap_or_else(default_exchange_dir);
        Self { cfg, exchange_dir, status: String::new() }
    }
}

fn default_exchange_dir() -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    format!("{home}/Veracage/Exchange")
}

impl eframe::App for Settings {
    fn update(&mut self, ctx: &egui::Context, _f: &mut eframe::Frame) {
        // Apply the (possibly just-changed) theme every frame so the picker is live.
        crate::theme::apply(ctx, &self.cfg.theme);

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(6.0);
            ui.strong("Settings");
            ui.add_space(10.0);

            ui.horizontal(|ui| {
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
            });
            ui.add_space(8.0);

            ui.checkbox(
                &mut self.cfg.gpu,
                "GPU passthrough for apps (faster, but a shared-GPU side channel)",
            );
            ui.add_space(8.0);

            ui.horizontal(|ui| {
                ui.label("On system suspend:");
                egui::ComboBox::from_id_salt("suspend")
                    .selected_text(match self.cfg.suspend_action.as_str() {
                        "ignore" => "Leave mounted",
                        _ => "Dismount (recommended)",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.cfg.suspend_action,
                            "dismount".into(),
                            "Dismount (recommended)",
                        );
                        ui.selectable_value(
                            &mut self.cfg.suspend_action,
                            "ignore".into(),
                            "Leave mounted",
                        );
                    });
            });
            ui.add_space(8.0);

            ui.checkbox(&mut self.cfg.exchange, "Shared Exchange folder (host \u{2194} vault)");
            ui.add_enabled_ui(self.cfg.exchange, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Exchange folder:");
                    ui.add(egui::TextEdit::singleline(&mut self.exchange_dir)
                        .desired_width(f32::INFINITY));
                });
            });

            ui.add_space(14.0);
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    let d = self.exchange_dir.trim();
                    self.cfg.exchange_dir = (!d.is_empty()).then(|| d.to_string());
                    match config::save(&self.cfg) {
                        Ok(_) => std::process::exit(0),
                        Err(e) => self.status = format!("Save failed: {e}"),
                    }
                }
                if ui.button("Cancel").clicked() {
                    std::process::exit(0);
                }
            });
            if !self.status.is_empty() {
                ui.add_space(6.0);
                ui.colored_label(egui::Color32::from_rgb(200, 80, 80), &self.status);
            }
        });
    }
}
