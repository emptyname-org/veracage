//! Keyboard and shortcuts: `veracage-agent _shortcuts`
//! (Settings > Keyboard and Shortcuts). Two things, both about keys: which
//! modifier mapping the sandbox uses, and the Veracage clipboard bindings -
//! click a binding, press the new combo, Save. (The apps' own Cut/Copy/Paste
//! are not Veracage bindings.)

use eframe::egui;

use crate::config;

/// (config action key, display label, one-line description).
const ROWS: &[(&str, &str, &str)] = &[
    ("copy_out", "Copy out", "Copy the Veracage clipboard to the host"),
    ("paste_in", "Paste in", "Copy the host clipboard into Veracage"),
];

pub fn run() -> Result<(), eframe::Error> {
    crate::theme::dialog("veracage-shortcuts", "Veracage Keyboard and Shortcuts", [660.0, 520.0], [560.0, 420.0], |cc| {
        crate::theme::apply_config(&cc.egui_ctx, &config::load());
        Box::new(Shortcuts::new())
    })
}

struct Shortcuts {
    cfg: config::Config,
    /// The host desktop's own XKB options, resolved once, so "Host setting"
    /// shows what it actually follows.
    host_modifiers: String,
    /// The action currently capturing a new combo, if any.
    capturing: Option<String>,
    /// True while OUR sentinel is on the host clipboard, so it is removed again
    /// on every exit path. The user's own clipboard is never seeded over.
    seeded_clipboard: bool,
    status: String,
}

impl Shortcuts {
    fn new() -> Self {
        Self {
            cfg: config::load(),
            host_modifiers: crate::keyboard::host_keyboard().options,
            capturing: None,
            seeded_clipboard: false,
            status: String::new(),
        }
    }

    /// Drop our sentinel from the host clipboard, if we put one there.
    fn unseed_clipboard(&mut self) {
        if self.seeded_clipboard {
            clear_clipboard();
            self.seeded_clipboard = false;
        }
    }

    fn bind(&self, action: &str) -> String {
        self.cfg
            .shortcuts
            .get(action)
            .cloned()
            .unwrap_or_else(|| config::default_shortcut(action))
    }
}

/// egui key -> the character used in a bind string, for letters and digits.
fn key_char(key: egui::Key) -> Option<char> {
    use egui::Key::*;
    let c = match key {
        A => 'a', B => 'b', C => 'c', D => 'd', E => 'e', F => 'f', G => 'g',
        H => 'h', I => 'i', J => 'j', K => 'k', L => 'l', M => 'm', N => 'n',
        O => 'o', P => 'p', Q => 'q', R => 'r', S => 's', T => 't', U => 'u',
        V => 'v', W => 'w', X => 'x', Y => 'y', Z => 'z',
        Num0 => '0', Num1 => '1', Num2 => '2', Num3 => '3', Num4 => '4',
        Num5 => '5', Num6 => '6', Num7 => '7', Num8 => '8', Num9 => '9',
        _ => return None,
    };
    Some(c)
}

/// Build a bind string ("Ctrl+Alt+C") from captured modifiers + key. None unless
/// at least one modifier is held (a bare key must not become a global shortcut).
fn combo_from(mods: egui::Modifiers, key: egui::Key) -> Option<String> {
    let c = key_char(key)?;
    let mut s = String::new();
    if mods.ctrl || mods.command {
        s.push_str("Ctrl+");
    }
    if mods.alt {
        s.push_str("Alt+");
    }
    if mods.shift {
        s.push_str("Shift+");
    }
    if s.is_empty() {
        return None;
    }
    s.push(c.to_ascii_uppercase());
    Some(s)
}

/// Placeholder text put on an EMPTY host clipboard while capturing, so egui
/// delivers the Paste event that carries Ctrl+V. Removed again on every exit.
const SENTINEL: &str = "Veracage";

/// True if the host clipboard already has text. egui only delivers a Paste event
/// when it does, so this decides whether capture needs a sentinel at all. Errors
/// read as "no text", the conservative answer (we seed and clean up after).
fn host_clipboard_has_text() -> bool {
    match arboard::Clipboard::new() {
        Ok(mut cb) => cb.get_text().is_ok_and(|t| !t.is_empty()),
        Err(_) => false,
    }
}

/// Remove the sentinel this dialog put on the host clipboard. Best-effort, so a
/// headless/odd clipboard can't break the configurator. Only ever called when we
/// seeded it: the user's own clipboard contents are left untouched.
fn clear_clipboard() {
    if let Ok(mut cb) = arboard::Clipboard::new() {
        let _ = cb.clear();
    }
}

impl eframe::App for Shortcuts {
    fn update(&mut self, ctx: &egui::Context, _f: &mut eframe::Frame) {
        // While capturing, grab the next modifier+key combo (Esc cancels).
        if let Some(action) = self.capturing.clone() {
            let result = ctx.input(|i| {
                if i.key_pressed(egui::Key::Escape) {
                    return Some(None);
                }
                for e in &i.events {
                    // egui swallows Ctrl+C/X/V into Copy/Cut/Paste events (the
                    // Key event is never delivered), which made every Ctrl
                    // combo un-capturable. Recover the key from the event and
                    // the modifiers from the live state.
                    let combo = match e {
                        egui::Event::Key { key, pressed: true, modifiers, .. } => {
                            if matches!(key, egui::Key::Escape) {
                                return Some(None);
                            }
                            combo_from(*modifiers, *key)
                        }
                        egui::Event::Copy => combo_from(i.modifiers, egui::Key::C),
                        egui::Event::Cut => combo_from(i.modifiers, egui::Key::X),
                        egui::Event::Paste(_) => combo_from(i.modifiers, egui::Key::V),
                        _ => None,
                    };
                    if let Some(combo) = combo {
                        return Some(Some(combo));
                    }
                }
                None
            });
            match result {
                Some(Some(combo)) => {
                    self.cfg.shortcuts.insert(action, combo);
                    self.capturing = None;
                    self.unseed_clipboard();
                }
                Some(None) => {
                    self.capturing = None; // cancelled
                    self.unseed_clipboard();
                }
                None => {}
            }
        }

        let status = &self.status;
        let (do_save, do_cancel) = crate::theme::action_bar(ctx, |ui| {
            if !status.is_empty() {
                ui.colored_label(crate::theme::ERROR, status);
            }
        });

        let host_modifiers = match self.host_modifiers.as_str() {
            "" => "Host setting".to_string(),
            opts => format!("Host setting ({opts})"),
        };
        crate::theme::content_panel(ctx, |ui| {
                // Which modifier mapping the sandbox uses. The default follows
                // the host desktop, so Ctrl/Alt/Win behave the same in and out.
                let col = crate::theme::text_width(ui, &["Modifier keys:"]) + 24.0;
                let ctrl = crate::theme::text_width(ui, &["Host setting"]) + 160.0;
                crate::theme::row(ui, col, "Modifier keys:", |ui| {
                    egui::ComboBox::from_id_salt("modifier_keys")
                        .width(ctrl)
                        .truncate()
                        .selected_text(crate::keyboard::label(&self.cfg.modifier_keys))
                        .show_ui(ui, |ui| {
                            for (key, label) in crate::keyboard::CHOICES {
                                let label = if *key == "system" {
                                    host_modifiers.clone()
                                } else {
                                    (*label).to_string()
                                };
                                ui.selectable_value(
                                    &mut self.cfg.modifier_keys,
                                    (*key).to_string(),
                                    label,
                                );
                            }
                        });
                });
                ui.add_space(10.0);
                ui.separator();
                ui.add_space(16.0);

                // Each shortcut button sits UNDER its explanation, with the
                // "how" beside the button it applies to rather than in a header.
                for (action, label, desc) in ROWS {
                    ui.label(*label);
                    ui.label(egui::RichText::new(*desc).weak());
                    ui.add_space(4.0);
                    let capturing = self.capturing.as_deref() == Some(*action);
                    let text = if capturing {
                        "Press new keys".to_string()
                    } else {
                        self.bind(action)
                    };
                    let btn = egui::Button::new(text).min_size(egui::vec2(150.0, 0.0));
                    let mut hit = false;
                    ui.horizontal(|ui| {
                        hit = ui.add(btn).clicked();
                        if !capturing {
                            ui.label(egui::RichText::new("Click to modify").weak());
                        }
                    });
                    if hit {
                        self.capturing = Some((*action).to_string());
                        // egui only emits a Paste event when the clipboard has
                        // content, so with an EMPTY clipboard Ctrl+V yields no
                        // event and the key is uncapturable (Copy always fires,
                        // which is why Ctrl+Shift+C worked but Ctrl+Shift+V did
                        // not). Seed a sentinel only in that case: overwriting a
                        // clipboard the user filled (a passphrase, say) would
                        // destroy it, and text already there makes Paste fire.
                        if !host_clipboard_has_text() {
                            ui.ctx().output_mut(|o| o.copied_text = SENTINEL.to_string());
                            self.seeded_clipboard = true;
                        }
                    }
                    ui.add_space(16.0);
                }
                if self.capturing.is_some() {
                    ctx.request_repaint(); // keep polling input while capturing
                }
            });

        // Exiting mid-capture must not leave the sentinel behind for the user to
        // paste later, so clean up before every exit (and on the window close,
        // via on_exit).
        if do_cancel {
            self.unseed_clipboard();
            std::process::exit(0);
        }
        if do_save {
            match config::save(&self.cfg) {
                Ok(_) => {
                    crate::broker::publish_shortcuts(&self.cfg);
                    crate::broker::publish_keyboard(&self.cfg);
                    self.unseed_clipboard();
                    std::process::exit(0);
                }
                Err(e) => self.status = format!("Save failed: {e}"),
            }
        }
    }

    /// Window closed (titlebar X / compositor): same cleanup as Save and Cancel.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.unseed_clipboard();
    }
}
