//! egui config picker: manage which apps are enabled inside Veracage (replaces
//! `configure.py`'s Qt window). One unified list: enabled apps are checked,
//! host-sensed suggestions are unchecked; ticking enables, unticking removes.
//! "Add another app" takes ANY installed binary (a name on `$PATH`, or an
//! absolute path). Closing the window (titlebar or Save) SAVES; Cancel discards.

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
    crate::theme::dialog(
        "veracage-configure",
        "Veracage Apps",
        [620.0, 640.0],
        [500.0, 420.0],
        move |cc| {
            crate::theme::apply_config(&cc.egui_ctx, &config::load());
            Box::new(app)
        },
    )?;
    let out = std::mem::take(&mut *outcome.lock().unwrap());
    Ok(out)
}

struct ConfigApp {
    cfg: config::Config, // existing config, non-apps fields preserved on save
    apps: Vec<apps::App>,
    suggested: Vec<detect::Suggestion>, // host defaults (xdg-mime)
    new_exec: String,
    error: String,
    cancelled: bool,
    outcome: Arc<Mutex<Outcome>>,
    /// Per-exec installed-state, memoized for the dialog's lifetime so the
    /// `$PATH` stat-walk in `is_installed` runs once per exec, not per repaint.
    installed: std::collections::HashMap<String, bool>,
    /// Per-exec host icon (None = no icon in the theme), resolved a few rows per
    /// frame: an icon that is NOT in the theme costs a bounded walk of every
    /// icon directory, and this dialog lists every suggestion, so resolving them
    /// all in one frame would stall the window on first paint.
    icons: std::collections::HashMap<String, Option<egui::TextureHandle>>,
}

/// Icon edge in logical points, and how many rows may resolve their icon per
/// frame (the rest fill in over the next frames).
const ROW_ICON_PT: f32 = 20.0;
const ICONS_PER_FRAME: usize = 2;

impl ConfigApp {
    fn new(outcome: Arc<Mutex<Outcome>>) -> Self {
        let cfg = config::load(); // load() dedupes by exec basename
        let apps = cfg.apps.clone();
        ConfigApp {
            cfg,
            apps,
            suggested: detect::detected_defaults(),
            new_exec: String::new(),
            error: String::new(),
            cancelled: false,
            outcome,
            installed: std::collections::HashMap::new(),
            icons: std::collections::HashMap::new(),
        }
    }

    /// The host icon for `exec`, resolved at most `ICONS_PER_FRAME` times per
    /// frame and cached. Returns None while still unresolved or when the theme
    /// has no icon for it; `budget` carries the remaining allowance.
    fn icon(
        &mut self,
        ctx: &egui::Context,
        exec: &str,
        budget: &mut usize,
    ) -> Option<egui::TextureHandle> {
        if let Some(slot) = self.icons.get(exec) {
            return slot.clone();
        }
        if *budget == 0 {
            return None;
        }
        *budget -= 1;
        let tex = detect::icon_rgba_for_exec(exec).map(|(w, h, rgba)| {
            let img = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
            ctx.load_texture(format!("veracage-cfg-{exec}"), img, egui::TextureOptions::LINEAR)
        });
        self.icons.insert(exec.to_string(), tex.clone());
        tex
    }

    /// Installed-state for `exec`, computed once and cached (stable for the
    /// short dialog session).
    fn is_installed(&mut self, exec: &str) -> bool {
        if let Some(&v) = self.installed.get(exec) {
            return v;
        }
        let v = apps::is_installed(exec);
        self.installed.insert(exec.to_string(), v);
        v
    }

    fn enabled_basename(&self, exec: &str) -> bool {
        let base = config::exec_basename(exec);
        self.apps.iter().any(|a| config::exec_basename(&a.exec) == base)
    }

    /// Enable an app (ticked suggestion): it moves into the enabled set.
    fn enable(&mut self, name: &str, exec: &str) {
        if self.enabled_basename(exec) {
            return;
        }
        let taken: HashSet<String> = self.apps.iter().map(|a| a.key.clone()).collect();
        self.apps.push(apps::App {
            key: key_for(exec, &taken),
            name: config::capitalize_first(name),
            exec: exec.to_string(),
        });
    }

    /// Add ANY installed binary (name defaults to its basename).
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
        if self.enabled_basename(&exec) {
            self.error = format!("'{}' is already enabled.", basename(&exec));
            return;
        }
        let taken: HashSet<String> = self.apps.iter().map(|a| a.key.clone()).collect();
        let name = config::capitalize_first(&basename(&exec));
        self.apps.push(apps::App { key: key_for(&exec, &taken), name, exec });
        self.new_exec.clear();
        self.error.clear();
    }

    /// The gray annotation for an app: its detected category, else its path
    /// (for a custom absolute-path binary), else nothing.
    fn note_for(&self, a: &apps::App) -> Option<String> {
        let base = config::exec_basename(&a.exec);
        if let Some(s) = self
            .suggested
            .iter()
            .find(|s| config::exec_basename(&s.exec) == base)
        {
            return Some(s.category.to_string());
        }
        if is_file_manager(&a.exec) {
            return Some("file manager".into());
        }
        a.exec.contains('/').then(|| a.exec.clone())
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

/// Known file-manager binaries (keep in sync with FILE_MANAGERS in cli.py),
/// annotated as such, and a file manager is what auto-opens a mounted volume.
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

/// One row's label: the app name, plus an optional gray "(note)" in the same
/// widget so the whole line is one click target.
/// One app row: tick box, the app's host icon, then its name and note. The whole
/// row toggles, not just the box. Returns true if this row was just toggled;
/// `on` holds the new state.
fn app_row(
    ui: &mut egui::Ui,
    on: &mut bool,
    icon: Option<&egui::TextureHandle>,
    label: egui::text::LayoutJob,
) -> bool {
    let mut toggled = false;
    let row = ui
        .horizontal(|ui| {
            toggled = ui.checkbox(on, "").changed();
            match icon {
                Some(tex) => {
                    ui.add(egui::Image::from_texture(egui::load::SizedTexture::new(
                        tex.id(),
                        egui::vec2(ROW_ICON_PT, ROW_ICON_PT),
                    )));
                }
                // Keep the names aligned while an icon is missing or pending.
                None => ui.add_space(ROW_ICON_PT),
            }
            ui.label(label);
        })
        .response;
    if !toggled && row.interact(egui::Sense::click()).clicked() {
        *on = !*on;
        toggled = true;
    }
    toggled
}

fn row_label(ui: &egui::Ui, name: &str, note: Option<&str>) -> egui::text::LayoutJob {
    let font = egui::TextStyle::Body.resolve(ui.style());
    let mut job = egui::text::LayoutJob::default();
    job.append(
        name,
        0.0,
        egui::TextFormat {
            font_id: font.clone(),
            color: ui.visuals().text_color(),
            ..Default::default()
        },
    );
    if let Some(n) = note {
        job.append(
            &format!("  ({n})"),
            0.0,
            egui::TextFormat {
                font_id: font,
                color: ui.visuals().weak_text_color(),
                ..Default::default()
            },
        );
    }
    job
}

impl eframe::App for ConfigApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Closing the window (titlebar) SAVES; Cancel sets `cancelled` first so it
        // discards. The config IS the state: there is no separate confirm step.
        if ctx.input(|i| i.viewport().close_requested()) && !self.cancelled {
            self.save();
        }

        let count = self.apps.len();
        let (do_save, do_cancel) = crate::theme::action_bar(ctx, |ui| {
            ui.label(egui::RichText::new(format!("{count} enabled")).weak());
        });

        crate::theme::content_panel(ctx, |ui| {
                ui.label(egui::RichText::new("Select apps to use in Veracage").weak());
                ui.add_space(10.0);

                // Warm the installed-state cache for every exec once, so the
                // per-row reads below don't stat-walk $PATH each repaint.
                let execs: Vec<String> = self
                    .apps
                    .iter()
                    .map(|a| a.exec.clone())
                    .chain(self.suggested.iter().map(|s| s.exec.clone()))
                    .collect();
                for e in execs {
                    self.is_installed(&e);
                }

                    // Enabled apps first (config order): checked; unticking removes.
                    let mut icon_budget = ICONS_PER_FRAME;
                    let mut remove: Option<usize> = None;
                    for i in 0..self.apps.len() {
                        let a = self.apps[i].clone();
                        let installed = self.installed.get(&a.exec).copied().unwrap_or(true);
                        let note = match (self.note_for(&a), installed) {
                            (Some(n), true) => Some(n),
                            (Some(n), false) => Some(format!("{n}, not installed")),
                            (None, true) => None,
                            (None, false) => Some("not installed".to_string()),
                        };
                        let label = row_label(ui, &a.name, note.as_deref());
                        let icon = self.icon(ctx, &a.exec, &mut icon_budget);
                        let mut on = true;
                        if app_row(ui, &mut on, icon.as_ref(), label) && !on {
                            remove = Some(i);
                        }
                    }
                    if let Some(i) = remove {
                        self.apps.remove(i);
                    }

                    // Host-sensed suggestions not yet enabled: unchecked; ticking
                    // enables. Collect the click and apply after the loop (can't
                    // call &mut self.enable while iterating self.suggested).
                    let mut enable_now: Option<(String, String)> = None;
                    for i in 0..self.suggested.len() {
                        // Copy the few fields the row needs, so the immutable
                        // borrow ends before the &mut self calls below.
                        let (exec, raw_name, category) = {
                            let s = &self.suggested[i];
                            (s.exec.clone(), s.name.clone(), s.category)
                        };
                        if self.enabled_basename(&exec) {
                            continue;
                        }
                        let mut on = false;
                        let name = config::capitalize_first(&raw_name);
                        let label = row_label(ui, &name, Some(category));
                        let icon = self.icon(ctx, &exec, &mut icon_budget);
                        if app_row(ui, &mut on, icon.as_ref(), label) && on {
                            enable_now = Some((name, exec));
                        }
                    }
                    // Icons resolve a couple of rows per frame: keep painting
                    // until they are all in.
                    if self.icons.len() < self.apps.len() + self.suggested.len() {
                        ctx.request_repaint();
                    }
                    if let Some((name, exec)) = enable_now {
                        self.enable(&name, &exec);
                    }

                    ui.add_space(14.0);
                    ui.label("Add another app");
                    ui.add_space(2.0);
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
                        ui.colored_label(crate::theme::ERROR, &self.error);
                    }
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
