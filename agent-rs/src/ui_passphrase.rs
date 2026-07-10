//! One-shot passphrase dialog: `veracage-agent _passphrase <vault-name>`.
//!
//! Pops a small window; on submit it writes the passphrase to **stdout as raw
//! bytes (no newline)** and exits 0; on cancel / window-close it exits 1. It runs
//! as a fresh process per prompt because winit/eframe can't create a second
//! `EventLoop` in one process — the headless broker (`broker.rs`) spawns this and
//! captures stdout, then pipes the passphrase to `veracage open --passphrase-stdin`.
//!
//! The field is hardened exactly like the old launcher: `Zeroizing<String>`,
//! pre-reserved so growth doesn't scatter copies, wiped before exit, and egui's
//! per-widget undo history (which snapshots plaintext) is reset on leave.

use std::io::Write;

use eframe::egui;
use zeroize::{Zeroize, Zeroizing};

pub fn run(vault_name: String) -> Result<(), eframe::Error> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Veracage — passphrase")
            .with_app_id("veracage")
            .with_inner_size([360.0, 150.0])
            .with_resizable(false),
        ..Default::default()
    };
    eframe::run_native(
        "veracage-passphrase",
        options,
        Box::new(move |cc| {
            crate::theme::apply(&cc.egui_ctx, &crate::config::load().theme);
            Ok(Box::new(PassphraseDialog::new(vault_name)) as Box<dyn eframe::App>)
        }),
    )
}

struct PassphraseDialog {
    name: String,
    passphrase: Zeroizing<String>,
}

impl PassphraseDialog {
    fn new(name: String) -> Self {
        Self { name, passphrase: Zeroizing::new(String::with_capacity(512)) }
    }

    /// Write the passphrase to stdout (raw, no newline), wipe, and exit 0. Never
    /// returns. `process::exit` skips destructors, so we zeroize explicitly first.
    fn submit(&mut self) -> ! {
        let out = std::io::stdout();
        let mut h = out.lock();
        let _ = h.write_all(self.passphrase.as_bytes());
        let _ = h.flush();
        self.passphrase.zeroize();
        std::process::exit(0);
    }
}

impl eframe::App for PassphraseDialog {
    fn update(&mut self, ctx: &egui::Context, _f: &mut eframe::Frame) {
        let pass_id = egui::Id::new("veracage-passphrase-field");
        let mut do_submit = false;
        let mut do_cancel = false;
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(6.0);
            ui.strong(format!("Passphrase for {}", self.name));
            ui.add_space(8.0);
            let resp = ui.add(
                egui::TextEdit::singleline(&mut *self.passphrase)
                    .id(pass_id)
                    .password(true)
                    .hint_text("volume passphrase")
                    .desired_width(f32::INFINITY),
            );
            resp.request_focus();
            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                do_submit = true;
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("Unlock").clicked() {
                    do_submit = true;
                }
                if ui.button("Cancel").clicked() {
                    do_cancel = true;
                }
            });
        });
        if do_submit || do_cancel {
            // Drop egui's undo history for the field (holds plaintext snapshots).
            egui::text_edit::TextEditState::default().store(ctx, pass_id);
        }
        if do_submit {
            self.submit(); // writes stdout + exits 0
        }
        if do_cancel {
            self.passphrase.zeroize();
            std::process::exit(1);
        }
    }
}
