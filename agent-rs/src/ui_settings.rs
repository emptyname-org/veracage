//! One-shot settings dialog: `veracage-agent _settings`.
//!
//! A fresh process (winit can't reopen an EventLoop) that edits the general
//! settings in `~/.config/veracage/config.toml` — GPU passthrough and the suspend
//! action. Spawned by the broker when the compositor's Settings menu is used.

use eframe::egui;

use crate::config;

pub fn run() -> Result<(), eframe::Error> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Veracage — settings")
            .with_app_id("veracage")
            .with_inner_size([400.0, 200.0])
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
    status: String,
}

impl Settings {
    fn new() -> Self {
        Self { cfg: config::load(), status: String::new() }
    }
}

impl eframe::App for Settings {
    fn update(&mut self, ctx: &egui::Context, _f: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(6.0);
            ui.strong("Settings");
            ui.add_space(8.0);
            ui.checkbox(
                &mut self.cfg.gpu,
                "GPU passthrough for apps (faster, but a shared-GPU side channel)",
            );
            ui.add_space(6.0);
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
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    self.status = match config::save(&self.cfg) {
                        Ok(_) => "Settings saved.".into(),
                        Err(e) => format!("Save failed: {e}"),
                    };
                }
                if ui.button("Close").clicked() {
                    std::process::exit(0);
                }
            });
            if !self.status.is_empty() {
                ui.add_space(6.0);
                ui.label(egui::RichText::new(&self.status).weak());
            }
        });
    }
}
