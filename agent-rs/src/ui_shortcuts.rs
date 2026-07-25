//! Shortcut configurator: `veracage-agent _shortcuts` (Settings > Shortcuts).
//! Rebind the Veracage clipboard transfers - click a binding, press the new
//! combo, Save. (The apps' own Cut/Copy/Paste are not Veracage bindings.)

use eframe::egui;

use crate::config;

/// (config action key, display label, one-line description).
const ROWS: &[(&str, &str, &str)] = &[
    ("copy_out", "Copy out", "Copy the Veracage clipboard to the host"),
    ("paste_in", "Paste in", "Copy the host clipboard into Veracage"),
];

pub fn run() -> Result<(), eframe::Error> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Veracage Shortcuts")
            .with_app_id("veracage")
            .with_inner_size([520.0, 480.0])
            .with_min_inner_size([440.0, 400.0]),
        ..Default::default()
    };
    eframe::run_native(
        "veracage-shortcuts",
        options,
        Box::new(|cc| {
            crate::theme::apply_config(&cc.egui_ctx, &config::load());
            Ok(Box::new(Shortcuts::new()) as Box<dyn eframe::App>)
        }),
    )
}

struct Shortcuts {
    cfg: config::Config,
    /// The action currently capturing a new combo, if any.
    capturing: Option<String>,
    status: String,
}

impl Shortcuts {
    fn new() -> Self {
        Self { cfg: config::load(), capturing: None, status: String::new() }
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

/// Clear the host clipboard, removing the sentinel that capture seeds to make
/// Ctrl+V's Paste event fire. Best-effort, so a headless/odd clipboard can't
/// break the configurator.
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
                    clear_clipboard();
                }
                Some(None) => {
                    self.capturing = None; // cancelled
                    clear_clipboard();
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

        crate::theme::content_panel(ctx, |ui| {
                ui.label(egui::RichText::new("Click a shortcut to modify").weak());
                ui.add_space(14.0);

                // Each shortcut button sits UNDER its explanation, not beside it.
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
                    if ui.add(btn).clicked() {
                        self.capturing = Some((*action).to_string());
                        // egui only emits a Paste event when the clipboard has
                        // content, so with an empty clipboard Ctrl+V yields no
                        // event and the key is uncapturable (Copy always fires,
                        // which is why Ctrl+Shift+C worked but Ctrl+Shift+V did
                        // not). Seed the clipboard so a paste is always delivered.
                        ui.ctx().output_mut(|o| o.copied_text = "Veracage".to_string());
                    }
                    ui.add_space(16.0);
                }
                if self.capturing.is_some() {
                    ctx.request_repaint(); // keep polling input while capturing
                }
            });

        if do_cancel {
            std::process::exit(0);
        }
        if do_save {
            match config::save(&self.cfg) {
                Ok(_) => {
                    crate::broker::publish_shortcuts(&self.cfg);
                    std::process::exit(0);
                }
                Err(e) => self.status = format!("Save failed: {e}"),
            }
        }
    }
}
