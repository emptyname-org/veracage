//! egui config picker — manage which apps are enabled in the sandbox (replaces
//! `configure.py`'s Qt window). There is no catalog: type ANY installed binary
//! (a name on `$PATH`, or an absolute path) and add it. Save writes
//! `config.toml`, preserving `[default]`/`[volumes]` (only the app list changes).

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use eframe::egui;

use crate::{apps, config};

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
            .with_inner_size([560.0, 560.0])
            .with_min_inner_size([400.0, 320.0]),
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
            new_exec: String::new(),
            new_name: String::new(),
            new_args: "/vault".into(),
            error: String::new(),
            outcome,
        }
    }

    fn add(&mut self) {
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
        // Re-adding the same binary updates its entry rather than duplicating it.
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
        let mut do_save = false;
        let mut do_cancel = false;

        egui::TopBottomPanel::bottom("buttons").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    do_save = true;
                }
                if ui.button("Cancel").clicked() {
                    do_cancel = true;
                }
                ui.label(egui::RichText::new(format!("{} app(s)", self.apps.len())).weak());
            });
            ui.add_space(4.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Sandbox apps");
            ui.label(
                "Any installed program can be added. It runs against the vault,\n\
                 confined by the sandbox (no host files, no network).",
            );
            // Nudge: a file manager is what opens the vault on load — make sure one
            // is enabled.
            let have_fm = self.apps.iter().any(|a| is_file_manager(&a.exec));
            if have_fm {
                ui.label(egui::RichText::new(
                    "\u{1F4C1} A file manager is enabled — it opens the vault on load.",
                ).weak());
            } else {
                ui.colored_label(
                    egui::Color32::from_rgb(180, 130, 40),
                    "\u{1F4C1} Add a file manager (e.g. Dolphin) to browse the vault \
                     — it opens automatically when a vault loads.",
                );
            }
            ui.separator();

            // Add row
            ui.horizontal(|ui| {
                ui.label("Binary:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.new_exec)
                        .desired_width(150.0)
                        .hint_text("kate or /path/to/app"),
                );
                ui.label("Name:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.new_name)
                        .desired_width(90.0)
                        .hint_text("optional"),
                );
                ui.label("Args:");
                ui.add(egui::TextEdit::singleline(&mut self.new_args).desired_width(80.0));
                if ui.button("Add").clicked() {
                    self.add();
                }
            });
            if !self.error.is_empty() {
                ui.colored_label(egui::Color32::from_rgb(200, 80, 80), &self.error);
            }
            ui.separator();

            // Enabled list
            egui::ScrollArea::vertical().show(ui, |ui| {
                if self.apps.is_empty() {
                    ui.weak("No apps yet — add one above.");
                }
                let mut remove: Option<usize> = None;
                for (i, a) in self.apps.iter().enumerate() {
                    ui.horizontal(|ui| {
                        if ui.button("✖").on_hover_text("Remove").clicked() {
                            remove = Some(i);
                        }
                        let args = if a.args.is_empty() {
                            String::new()
                        } else {
                            format!(" {}", a.args.join(" "))
                        };
                        let missing = if apps::is_installed(&a.exec) {
                            ""
                        } else {
                            "  (not installed)"
                        };
                        let tag = if is_file_manager(&a.exec) { "\u{1F4C1} " } else { "" };
                        ui.label(format!("{}{}  —  {}{}{}", tag, a.name, a.exec, args, missing));
                    });
                }
                if let Some(i) = remove {
                    self.apps.remove(i);
                }
            });
        });

        if do_save {
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
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if do_cancel {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}
