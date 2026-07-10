//! egui config picker — manage which apps are enabled in the sandbox (replaces
//! `configure.py`'s Qt window). Shows a curated list of common apps as tick-boxes
//! for one-click enabling, plus an "Add another app…" form for ANY installed
//! binary (a name on `$PATH`, or an absolute path). Save writes `config.toml`,
//! preserving `[default]`/`[volumes]` (only the app list changes). Closing the
//! window SAVES (the state is the config; there is nothing to "cancel").

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
            .with_title("Veracage — sandbox apps")
            .with_app_id("veracage")
            .with_inner_size([560.0, 600.0])
            .with_min_inner_size([420.0, 360.0]),
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
    /// The host's DEFAULT apps (file manager / editor / viewer…), sensed via
    /// xdg-mime — the one-click tick-boxes. Not a catalog we hardcode.
    suggested: Vec<detect::Suggestion>,
    show_custom: bool,
    new_exec: String,
    new_name: String,
    new_args: String,
    error: String,
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
            show_custom: false,
            new_exec: String::new(),
            new_name: String::new(),
            new_args: "/vault".into(),
            error: String::new(),
            outcome,
        }
    }

    fn enabled(&self, exec: &str) -> bool {
        self.apps.iter().any(|a| a.exec == exec)
    }

    /// Enable/disable a suggested app by its exec (tick-box).
    fn set_enabled(&mut self, name: &str, exec: &str, on: bool) {
        if on {
            if self.enabled(exec) {
                return;
            }
            let taken: HashSet<String> = self.apps.iter().map(|a| a.key.clone()).collect();
            self.apps.push(apps::App {
                key: key_for(exec, &taken),
                name: name.to_string(),
                exec: exec.to_string(),
                args: vec!["/vault".to_string()],
            });
        } else {
            self.apps.retain(|a| a.exec != exec);
        }
    }

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
        let name = if self.new_name.trim().is_empty() {
            basename(&exec)
        } else {
            self.new_name.trim().to_string()
        };
        let args = self.new_args.split_whitespace().map(str::to_string).collect();
        self.apps.retain(|a| a.key != key);
        self.apps.push(apps::App { key, name, exec, args });
        self.new_exec.clear();
        self.new_name.clear();
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
/// to nudge the user to enable one — it's what opens the vault on load.
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
        // Closing the window IS the save (the config is the state — there is no
        // separate "cancel"): intercept the close request, persist, then let it go.
        if ctx.input(|i| i.viewport().close_requested()) {
            self.save();
        }

        let mut do_close = false;

        egui::TopBottomPanel::bottom("buttons").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.button("Save & Close").clicked() {
                    do_close = true;
                }
                ui.label(
                    egui::RichText::new(format!("{} app(s) enabled", self.apps.len())).weak(),
                );
            });
            ui.add_space(4.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Sandbox apps");
            ui.label(
                "Tick the apps you want available against the vault. They run\n\
                 confined by the sandbox (no host files, no network).",
            );

            // File-manager nudge — a file manager opens the vault on load.
            let have_fm = self.apps.iter().any(|a| is_file_manager(&a.exec));
            if have_fm {
                ui.label(egui::RichText::new(
                    "\u{1F4C1} A file manager is enabled — it opens the vault on load.",
                ).weak());
            } else {
                ui.colored_label(
                    egui::Color32::from_rgb(180, 130, 40),
                    "\u{1F4C1} Tick a file manager (e.g. Dolphin) to browse the vault \
                     — it opens automatically when a vault loads.",
                );
            }
            ui.separator();

            egui::ScrollArea::vertical().show(ui, |ui| {
                // The host's default apps (sensed via xdg-mime) as tick-boxes.
                // Cloned into locals so the loop can call &mut self (set_enabled).
                let suggested: Vec<(String, String, &str)> = self
                    .suggested
                    .iter()
                    .map(|s| (s.name.clone(), s.exec.clone(), s.category))
                    .collect();
                if suggested.is_empty() {
                    ui.weak("(No host default apps detected — add one below.)");
                } else {
                    ui.label(egui::RichText::new("Your default apps").weak());
                }
                for (name, exec, cat) in &suggested {
                    let mut on = self.enabled(exec);
                    let tag = if is_file_manager(exec) { "\u{1F4C1} " } else { "" };
                    if ui.checkbox(&mut on, format!("{tag}{name}  \u{2014}  {cat}")).changed() {
                        self.set_enabled(name, exec, on);
                    }
                }

                ui.add_space(6.0);
                ui.separator();

                // Add-another-app form (any installed binary). Collapsed by default.
                let hdr = if self.show_custom { "\u{25BE} Add another app\u{2026}" }
                          else { "\u{25B8} Add another app\u{2026}" };
                if ui.selectable_label(false, hdr).clicked() {
                    self.show_custom = !self.show_custom;
                }
                if self.show_custom {
                    ui.horizontal(|ui| {
                        ui.label("Binary:");
                        ui.add(egui::TextEdit::singleline(&mut self.new_exec)
                            .desired_width(150.0)
                            .hint_text("e.g. gimp or /opt/app/bin/app"));
                        ui.label("Name:");
                        ui.add(egui::TextEdit::singleline(&mut self.new_name)
                            .desired_width(90.0)
                            .hint_text("optional"));
                    });
                    ui.horizontal(|ui| {
                        ui.label("Args:").on_hover_text(
                            "Passed to the app on launch — /vault is the mounted vault, \
                             so the app opens there.");
                        ui.add(egui::TextEdit::singleline(&mut self.new_args).desired_width(120.0));
                        if ui.button("Add").clicked() {
                            self.add_custom();
                        }
                    });
                    if !self.error.is_empty() {
                        ui.colored_label(egui::Color32::from_rgb(200, 80, 80), &self.error);
                    }
                }

                // Custom (non-suggested) apps, with remove — the tick-boxes above
                // already manage the suggested ones.
                let custom: Vec<usize> = self.apps.iter().enumerate()
                    .filter(|(_, a)| !self.suggested.iter().any(|s| s.exec == a.exec))
                    .map(|(i, _)| i)
                    .collect();
                if !custom.is_empty() {
                    ui.add_space(6.0);
                    ui.separator();
                    ui.label(egui::RichText::new("Custom apps").weak());
                    let mut remove: Option<usize> = None;
                    for i in custom {
                        let a = &self.apps[i];
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
                }
            });
        });

        if do_close {
            self.save();
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}
