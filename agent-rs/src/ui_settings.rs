//! System integration: `veracage-agent _settings`
//! (Settings > System Integration).
//!
//! A fresh process (winit can't reopen an EventLoop) that edits what Veracage
//! DOES on its own, in `~/.config/veracage/config.toml`: clearing the host
//! clipboard, dismounting an idle session, what happens on suspend, and the
//! shared directory. How it LOOKS lives in `ui_appearance`, how you drive it in
//! `ui_shortcuts`.
//! Spawned by the broker when the compositor's Settings menu is used;
//! Save/Cancel are pinned bottom-right by `theme::action_bar`.

use eframe::egui;

use crate::config;

pub fn run() -> Result<(), eframe::Error> {
    crate::theme::dialog("veracage-settings", "Veracage System Integration", [680.0, 380.0], [580.0, 320.0], |cc| {
        let _ = cc;
        Box::new(Settings::new())
    })
}

struct Settings {
    cfg: config::Config,
    exchange_dir: String,
    status: String,
    /// Font + theme applied live, so a choice is visible before it is saved.
    style: crate::theme::LiveStyle,
    /// The browse button's icon, resolved from the host theme on first use
    /// (inner None = the theme has neither name, so the button shows text).
    browse: Option<Option<egui::TextureHandle>>,
    /// One "window up" debug line per dialog, not one per frame.
    first_frame_logged: bool,
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
            style: crate::theme::LiveStyle::default(),
            browse: None,
            first_frame_logged: false,
        }
    }
}

impl Settings {
    /// The browse icon, loaded from the host icon theme once per dialog and
    /// desaturated to the theme's ink, so it reads like the menu bar's own
    /// two-colour glyphs instead of a lone coloured badge.
    fn browse_icon(&mut self, ctx: &egui::Context) -> Option<egui::TextureHandle> {
        if self.browse.is_none() {
            let dark = self.cfg.theme == "dark";
            let tex = ["document-open-folder", "document-open", "folder-open"]
                .iter()
                .find_map(|name| crate::detect::icon_rgba_for_name(name))
                .map(|(w, h, rgba)| {
                    let img = egui::ColorImage::from_rgba_unmultiplied(
                        [w as usize, h as usize],
                        &mono(&rgba, dark),
                    );
                    ctx.load_texture("veracage-browse", img, egui::TextureOptions::LINEAR)
                });
            self.browse = Some(tex);
        }
        self.browse.clone().flatten()
    }
}

fn default_exchange_dir() -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    format!("{home}/Veracage/Exchange")
}

/// Host-clipboard auto-clear delays offered in Settings, in seconds. A
/// hand-edited config value outside this list still shows verbatim.
const CLIP_TIMEOUTS: &[u32] = &[10, 30, 60, 120];

/// Every label in the dialog, so the control column starts past the widest of
/// them whatever font size the user picked.
const LABELS: &[&str] = &[
    "Clear host clipboard:",
    "Auto-dismount after:",
    "On system suspend:",
    "Shared directory",
];

/// The widest text a dropdown is expected to show. Every control is given the
/// same width so their right edges line up, and anything longer than this is
/// truncated rather than allowed to stretch its row.
const CONTROL_SAMPLES: &[&str] = &["Dismount (recommended)", "After 120 seconds"];

/// Width of the control column at the CURRENT font: the widest sample plus room
/// for the dropdown's arrow.
fn control_width(ui: &egui::Ui) -> f32 {
    crate::theme::text_width(ui, CONTROL_SAMPLES) + 64.0
}

/// Width of the label column at the CURRENT font: the widest label plus a gap.
/// Measured rather than hardcoded, because the user can set the UI font size.
fn label_column_width(ui: &egui::Ui) -> f32 {
    crate::theme::text_width(ui, LABELS) + 24.0
}

/// The shared-directory field's frame margin. Named because the browse button's
/// height is derived from it (a TextEdit reports its INNER rect).
const FIELD_MARGIN: egui::Margin = egui::Margin { left: 4.0, right: 4.0, top: 2.0, bottom: 2.0 };

/// The browse button beside the shared-directory field: the host theme's own
/// "open" icon, or "..." when the theme has no such icon. Square and exactly as
/// tall as the field it sits next to, like the file dialogs the desktop draws.
fn browse_button(
    ui: &mut egui::Ui,
    icon: Option<&egui::TextureHandle>,
    side: f32,
) -> egui::Response {
    ui.scope(|ui| {
        // A button is its content plus `button_padding`, floored at
        // `interact_size` - whose default 40x18 is what made this wider and
        // taller than the field it sits beside. Pin both to the field's height
        // and the button comes out exactly square, with the glyph at half size.
        ui.spacing_mut().interact_size = egui::Vec2::splat(side);
        ui.spacing_mut().button_padding = egui::Vec2::splat(side * 0.25);
        match icon {
            Some(tex) => ui.add(egui::ImageButton::new(egui::load::SizedTexture::new(
                tex.id(),
                egui::Vec2::splat(side * 0.5),
            ))),
            None => ui.add(egui::Button::new("...")),
        }
        .on_hover_text("Choose a directory")
    })
    .inner
}

/// An RGBA icon as one ink colour: luminance, inverted under the dark theme so a
/// dark-drawn glyph shows light. Transparency is untouched, so the shape stays.
fn mono(rgba: &[u8], dark: bool) -> Vec<u8> {
    rgba.chunks_exact(4)
        .flat_map(|px| {
            let lum = 0.299 * px[0] as f32 + 0.587 * px[1] as f32 + 0.114 * px[2] as f32;
            let ink = if dark { 255.0 - lum } else { lum } as u8;
            [ink, ink, ink, px[3]]
        })
        .collect()
}

/// Where the directory picker opens: the path in the field (an empty field means
/// the default shared directory), walking up to the nearest parent that exists
/// so it never opens somewhere unrelated, and the home directory as the floor.
fn pick_start_dir(configured: &str) -> std::path::PathBuf {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/"));
    let configured = configured.trim();
    let start = if configured.is_empty() {
        default_exchange_dir()
    } else {
        configured.to_string()
    };
    // The field may hold the same `~/...` form the config file allows.
    let start = match start.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if start == "~" => home.clone(),
        None => std::path::PathBuf::from(start),
    };
    let mut path = start.as_path();
    loop {
        if path.is_dir() {
            return path.to_path_buf();
        }
        match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => path = parent,
            _ => return home,
        }
    }
}

/// Idle timeouts offered for the automatic dismount, in minutes (0 = off).
const AUTO_DISMOUNTS: &[u32] = &[0, 30, 60, 120, 720];

/// One label for an idle-dismount timeout, so the dropdown's closed state and
/// its rows cannot drift apart. A hand-edited value shows verbatim.
fn auto_dismount_label(minutes: u32) -> String {
    match minutes {
        0 => "Off".to_string(),
        1 => "1 minute".to_string(),
        m if m < 60 => format!("{m} minutes"),
        60 => "1 hour".to_string(),
        m if m % 60 == 0 => format!("{} hours", m / 60),
        m => format!("{} hours {} minutes", m / 60, m % 60),
    }
}

/// One label for the clipboard-clear choice, so the dropdown's closed state and
/// its rows cannot drift apart.
fn clip_clear_label(on: bool, secs: u32) -> String {
    if on {
        format!("After {secs} seconds")
    } else {
        "Off".to_string()
    }
}

impl eframe::App for Settings {
    fn update(&mut self, ctx: &egui::Context, _f: &mut eframe::Frame) {
        self.style.apply(ctx, &self.cfg);
        if !self.first_frame_logged {
            self.first_frame_logged = true;
            crate::broker::debug_log(&format!(
                "settings: window up, focused={}",
                ctx.input(|i| i.focused)
            ));
        }

        // Save / Cancel pinned bottom-right (design), with the error status.
        let status = &self.status;
        let (do_save, do_cancel) = crate::theme::action_bar(ctx, |ui| {
            if !status.is_empty() {
                ui.colored_label(crate::theme::ERROR, status);
            }
        });

        crate::theme::content_panel(ctx, |ui| {
            ui.add_space(6.0);
            let col = label_column_width(ui);
            let ctrl = control_width(ui);

            // Auto-clear the host clipboard some seconds after a Copy out
            // (sensitive text pushed sandbox -> host). Copying inside the sandbox
            // never arms it; the compositor enforces both. On/off and the delay
            // are one choice: "Off", or how long a copied secret may linger.
            crate::theme::row(ui, col, "Clear host clipboard:", |ui| {
                egui::ComboBox::from_id_salt("clip_clear")
                    .width(ctrl)
                    .truncate()
                    .selected_text(clip_clear_label(
                        self.cfg.clip_clear,
                        self.cfg.clip_clear_timeout,
                    ))
                    .show_ui(ui, |ui| {
                        // Off leaves the stored timeout alone, so switching back
                        // on does not silently change the delay.
                        if ui.selectable_label(!self.cfg.clip_clear, "Off").clicked() {
                            self.cfg.clip_clear = false;
                        }
                        for &secs in CLIP_TIMEOUTS {
                            let picked =
                                self.cfg.clip_clear && self.cfg.clip_clear_timeout == secs;
                            if ui
                                .selectable_label(picked, clip_clear_label(true, secs))
                                .clicked()
                            {
                                self.cfg.clip_clear = true;
                                self.cfg.clip_clear_timeout = secs;
                            }
                        }
                    });
            });

            // Idle, not elapsed: the volume closes when Veracage has been left
            // alone, not while it is being used.
            crate::theme::row(ui, col, "Auto-dismount after:", |ui| {
                egui::ComboBox::from_id_salt("auto_dismount")
                    .width(ctrl)
                    .truncate()
                    .selected_text(auto_dismount_label(self.cfg.auto_dismount))
                    .show_ui(ui, |ui| {
                        for &minutes in AUTO_DISMOUNTS {
                            ui.selectable_value(
                                &mut self.cfg.auto_dismount,
                                minutes,
                                auto_dismount_label(minutes),
                            );
                        }
                    });
            });

            crate::theme::row(ui, col, "On system suspend:", |ui| {
                egui::ComboBox::from_id_salt("suspend")
                    .width(ctrl)
                    .truncate()
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

            // The shared directory is a session setting, not a look: it decides
            // what can cross the boundary. Checkbox in the label column, path in
            // the control column like every other row.
            let browse = self.browse_icon(ctx);
            // Both sides of the row touch `self`, so work on locals and write
            // back: the two closures would otherwise borrow it twice. The
            // checkbox state crosses between them, hence the Cell.
            let enabled = std::cell::Cell::new(self.cfg.exchange);
            let mut dir = std::mem::take(&mut self.exchange_dir);
            let mut pick = false;
            crate::theme::labelled_row(
                ui,
                col,
                |ui| {
                    let mut on = enabled.get();
                    ui.checkbox(&mut on, "Shared directory");
                    enabled.set(on);
                },
                |ui| {
                    ui.add_enabled_ui(enabled.get(), |ui| {
                        // Field and button get the SAME height.
                        let h = crate::theme::row_height(ui);
                        // Field plus button add up to one control width, so this
                        // row ends where every dropdown does.
                        let field_w = (ctrl - h - ui.spacing().item_spacing.x).max(80.0);
                        let field = ui.add_sized(
                            [field_w, h],
                            egui::TextEdit::singleline(&mut dir).margin(FIELD_MARGIN),
                        );
                        // The field decides the height. Its response rect is the
                        // INNER one, so add back the margin its frame is drawn
                        // with, or the button is a few pixels short of it.
                        let side = field.rect.height() + FIELD_MARGIN.sum().y;
                        pick = browse_button(ui, browse.as_ref(), side).clicked();
                    });
                },
            );
            self.cfg.exchange = enabled.get();
            self.exchange_dir = dir;
            if pick {
                if let Some(chosen) = rfd::FileDialog::new()
                    .set_title("Shared directory")
                    .set_directory(pick_start_dir(&self.exchange_dir))
                    .pick_folder()
                {
                    self.exchange_dir = chosen.display().to_string();
                }
            }
        });

        if do_cancel {
            crate::broker::debug_log("settings: cancel, exiting without saving");
            std::process::exit(0);
        }
        if do_save {
            crate::broker::debug_log("settings: save requested");
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
                    crate::broker::publish_theme(&self.cfg);
                    crate::broker::publish_appfont(&self.cfg);
                    crate::broker::publish_autodismount(&self.cfg);
                    crate::broker::debug_log("settings: saved and published, exiting");
                    std::process::exit(0);
                }
                Err(e) => {
                    crate::broker::debug_log(&format!("settings: save FAILED: {e}"));
                    self.status = format!("Save failed: {e}");
                }
            }
        }
    }
}
