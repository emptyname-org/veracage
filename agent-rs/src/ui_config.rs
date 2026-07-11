//! egui config picker — manage which apps are enabled against the vault (replaces
//! `configure.py`'s Qt window). Host-sensed default apps appear as tick-boxes to
//! enable; every enabled app is listed with an ✖ to remove; "Add another app…"
//! takes ANY installed binary (a name on `$PATH`, or an absolute path). Closing
//! the window (titlebar or Save) SAVES; Cancel discards. Layout follows the user's
//! design: wide margins, Save/Cancel bottom-right (same house style as settings).

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use eframe::egui;

use crate::{apps, config, detect};

#[derive(Default)]
pub struct Outcome {
    pub saved: bool,
    pub count: usize,
}

pub fn run_configure() -> Result<Outcome, eframe::Error> {
    let outcome = Arc::new(Mutex::new(Outcome::default()));
    let app = ConfigApp::new(outcome.clone());
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Veracage — apps")
            .with_app_id("veracage")
            .with_inner_size([620.0, 640.0])
            .with_min_inner_size([500.0, 420.0]),
        ..Default::default()
    };
    eframe::run_native(
        "veracage-configure",
        options,
        Box::new(move |cc| {
            crate::theme::apply(&cc.egui_ctx, &config::load().theme);
            Ok(Box::new(app) as Box<dyn eframe::App>)
        }),
    )?;
    let out = std::mem::take(&mut *outcome.lock().unwrap());
    Ok(out)
}

struct ConfigApp {
    cfg: config::Config, // existing config — non-apps fields preserved on save
    apps: Vec<apps::App>,
    suggested: Vec<detect::Suggestion>, // host defaults (xdg-mime) — quick tick-boxes
    new_exec: String,
    error: String,
    cancelled: bool,
    outcome: Arc<Mutex<Outcome>>,
}

impl ConfigApp {
    fn new(outcome: Arc<Mutex<Outcome>>) -> Self {
        let cfg = config::load();
        let apps = cfg.apps.clone();
        ConfigApp {
            cfg,
            apps,
            suggested: detect::detected_defaults(),
            new_exec: String::new(),
            error: String::new(),
            cancelled: false,
            outcome,
        }
    }

    fn enabled(&self, exec: &str) -> bool {
        self.apps.iter().any(|a| a.exec == exec)
    }

    /// Enable a suggested app (tick-box) — it then leaves the suggestions and
    /// appears in the enabled list below.
    fn enable(&mut self, name: &str, exec: &str) {
        if self.enabled(exec) {
            return;
        }
        let taken: HashSet<String> = self.apps.iter().map(|a| a.key.clone()).collect();
        self.apps.push(apps::App {
            key: key_for(exec, &taken),
            name: name.to_string(),
            exec: exec.to_string(),
            args: vec!["/vaults".to_string()],
        });
    }

    /// Add ANY installed binary (name defaults to its basename, opened at /vaults).
    fn add_custom(&mut self) {
        let exec = self.new_exec.trim().to_string();
        if exec.is_empty() {
            self.error = "Type a binary name or an absolute path.".into();
            return;
        }
        if !apps::is_installed(&exec) {
            self.error = format!("'{exec}' is not installed / not on $PATH.");
            return;
        }
        let taken: HashSet<String> = self.apps.iter().map(|a| a.key.clone()).collect();
        let key = self
            .apps
            .iter()
            .find(|a| a.exec == exec)
            .map(|a| a.key.clone())
            .unwrap_or_else(|| key_for(&exec, &taken));
        self.apps.retain(|a| a.exec != exec);
        self.apps.push(apps::App { key, name: basename(&exec), exec, args: vec!["/vaults".into()] });
        self.new_exec.clear();
        self.error.clear();
    }

    fn save(&mut self) {
        self.cfg.apps = self.apps.clone();
        match config::save(&self.cfg) {
            Ok(p) => {
                let mut o = self.outcome.lock().unwrap();
                o.saved = true;
                o.count = self.cfg.apps.len();
                eprintln!("veracage: wrote {}", p.display());
            }
            Err(e) => eprintln!("veracage: save failed: {e}"),
        }
    }
}

fn basename(s: &str) -> String {
    std::path::Path::new(s)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(s)
        .to_string()
}

/// Known file-manager binaries (keep in sync with FILE_MANAGERS in cli.py). Used
/// to tag them with a folder glyph — a file manager is what opens the vault.
const FILE_MANAGERS: &[&str] = &[
    "dolphin", "nautilus", "nemo", "thunar", "pcmanfm", "pcmanfm-qt",
    "caja", "konqueror", "krusader", "nnn", "ranger",
];

fn is_file_manager(exec: &str) -> bool {
    FILE_MANAGERS.contains(&basename(exec).as_str())
}

fn key_for(exec: &str, taken: &HashSet<String>) -> String {
    let base: String = basename(exec)
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    let base = base.trim_matches('-');
    let base = if base.is_empty() { "app" } else { base };
    if !taken.contains(base) {
        return base.to_string();
    }
    (2..).map(|n| format!("{base}-{n}")).find(|k| !taken.contains(k)).unwrap()
}

impl eframe::App for ConfigApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Closing the window (titlebar) SAVES; Cancel sets `cancelled` first so it
        // discards. The config IS the state — there is no separate confirm step.
        if ctx.input(|i| i.viewport().close_requested()) && !self.cancelled {
            self.save();
        }

        let mut do_save = false;
        let mut do_cancel = false;

        egui::TopBottomPanel::bottom("actions")
            .frame(crate::theme::content_frame(ctx))
            .show(ctx, |ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    crate::theme::pad_buttons(ui);
                    if ui.button("Cancel").clicked() {
                        do_cancel = true;
                    }
                    if ui.button("Save").clicked() {
                        do_save = true;
                    }
                    ui.label(
                        egui::RichText::new(format!("{} enabled", self.apps.len())).weak(),
                    );
                });
            });

        egui::CentralPanel::default()
            .frame(crate::theme::content_frame(ctx))
            .show(ctx, |ui| {
                ui.heading("Pick the apps you want to use in Veracage");
                ui.add_space(12.0);

                egui::ScrollArea::vertical().show(ui, |ui| {
                    // Host-sensed suggestions not yet enabled — tick to enable.
                    // Collect the click and apply after the loop (can't call
                    // &mut self.enable while iterating self.suggested).
                    let mut enable_now: Option<(String, String)> = None;
                    for s in self.suggested.iter().filter(|s| !self.enabled(&s.exec)) {
                        let tag = if is_file_manager(&s.exec) { "\u{1F4C1} " } else { "" };
                        let mut on = false;
                        if ui
                            .checkbox(&mut on, format!("{tag}{}  \u{2014}  {}", s.name, s.category))
                            .changed()
                        {
                            enable_now = Some((s.name.clone(), s.exec.clone()));
                        }
                    }
                    if let Some((name, exec)) = enable_now {
                        self.enable(&name, &exec);
                    }

                    // Every enabled app, with an ✖ to remove.
                    let mut remove: Option<usize> = None;
                    for (i, a) in self.apps.iter().enumerate() {
                        ui.horizontal(|ui| {
                            if ui.button("\u{2716}").on_hover_text("Remove").clicked() {
                                remove = Some(i);
                            }
                            let args = if a.args.is_empty() { String::new() }
                                       else { format!(" {}", a.args.join(" ")) };
                            let missing = if apps::is_installed(&a.exec) { "" }
                                          else { "  (not installed)" };
                            let tag = if is_file_manager(&a.exec) { "\u{1F4C1} " } else { "" };
                            ui.label(format!("{}{}  \u{2014}  {}{}{}", tag, a.name, a.exec, args, missing));
                        });
                    }
                    if let Some(i) = remove {
                        self.apps.remove(i);
                    }

                    ui.add_space(14.0);
                    ui.strong("Add another app\u{2026}");
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        let add_w = 64.0;
                        let field_w = (ui.available_width() - add_w - 8.0).max(120.0);
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut self.new_exec)
                                .desired_width(field_w)
                                .hint_text("e.g. gimp or /opt/app/bin/app"),
                        );
                        let enter =
                            resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        if ui.button("Add").clicked() || enter {
                            self.add_custom();
                        }
                    });
                    if !self.error.is_empty() {
                        ui.colored_label(egui::Color32::from_rgb(200, 80, 80), &self.error);
                    }
                });
            });

        if do_cancel {
            self.cancelled = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if do_save {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close); // close_requested saves
        }
    }
}
