//! Help window: `veracage-agent _help` (Help > Help in the compositor menu).
//! Short, matter-of-fact usage notes. Static text, one scrollable page.

use eframe::egui;

use crate::config;

/// (section title, lines) - rendered as heading + body paragraphs.
const SECTIONS: &[(&str, &[&str])] = &[
    (
        "Mount a volume",
        &[
            "File > Mount volume. Pick the encrypted volume file and enter its \
             passphrase.",
            "The system then asks for your account password. That prompt \
             authorizes Veracage's privileged helper to decrypt the volume and \
             mount it privately for Veracage, the only step that needs \
             root. The authorization is kept for a few minutes, so mounting \
             another volume right after usually does not ask again.",
            "The volume mounts and the file manager opens on it. Mount more \
             volumes the same way and they appear side by side.",
        ],
    ),
    (
        "Use apps",
        &[
            "The Apps menu lists the enabled apps. Click one to run it inside \
             Veracage.",
            "Apps run isolated inside Veracage. They see the mounted volumes, a \
             temporary workspace and the shared directory, nothing else of your \
             system, and they have no network.",
            "Apps also work before any volume is mounted. Everything they \
             write outside the shared directory lands in the temporary \
             workspace, which is discarded when Veracage quits, so an empty \
             Veracage is a private scratchpad.",
        ],
    ),
    (
        "Clipboard",
        &[
            "The Veracage clipboard is separate from the host clipboard. \
             Nothing crosses by itself.",
            "Clipboard > Copy out puts what you last copied inside Veracage \
             onto the host clipboard. Clipboard > Paste in puts the host \
             clipboard onto the Veracage clipboard. Text only. The keyboard \
             shortcuts are configurable under Settings > Configure shortcuts.",
            "After a Copy out, the host clipboard is cleared automatically after \
             a timeout (default 30 seconds), and again when Veracage quits, so \
             a copied secret does not linger on the host. Both are configurable \
             under Settings.",
        ],
    ),
    (
        "Move files in and out",
        &[
            "The shared directory is visible to both the host and Veracage. A \
             file dropped on one side appears on the other.",
            "File > Shared directory opens it on the host. Inside Veracage it \
             is /exchange.",
            "Settings lets you turn the shared directory off or point it at a \
             different host path.",
        ],
    ),
    (
        "Configure apps",
        &[
            "Apps > Configure apps. Tick the apps you want, or add any \
             installed program by name.",
            "Saving updates the Apps menu right away, including the running \
             session.",
        ],
    ),
    (
        "GPU acceleration",
        &[
            "Settings > GPU acceleration for apps chooses how apps draw.",
            "Off means software rendering. Apps draw on the CPU, so video and \
             3D are slower, and the sandbox shares no graphics hardware with \
             the host.",
            "On lets apps render on the host GPU, so video and 3D are smooth. \
             The GPU is hardware shared with the host, and shared hardware is \
             a potential side channel: a compromised app could try to observe \
             traces of other GPU work, or leave traces of its own. Keep it \
             off for maximum isolation, turn it on when playback or rendering \
             is too slow.",
        ],
    ),
    (
        "Unmount",
        &[
            "File > Unmount lists the mounted volumes. Unmounting one locks \
             its data again and leaves the rest of the session running.",
            "Closing the Veracage window (or File > Quit) unmounts everything.",
        ],
    ),
];

pub fn run() -> Result<(), eframe::Error> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Veracage Help")
            .with_app_id("veracage")
            .with_inner_size([560.0, 660.0])
            .with_min_inner_size([440.0, 400.0]),
        ..Default::default()
    };
    eframe::run_native(
        "veracage-help",
        options,
        Box::new(|cc| {
            crate::theme::apply_config(&cc.egui_ctx, &config::load());
            Ok(Box::new(Help) as Box<dyn eframe::App>)
        }),
    )
}

struct Help;

impl eframe::App for Help {
    fn update(&mut self, ctx: &egui::Context, _f: &mut eframe::Frame) {
        egui::CentralPanel::default()
            .frame(crate::theme::content_frame(ctx))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for (title, lines) in SECTIONS {
                        ui.heading(*title);
                        ui.add_space(2.0);
                        for line in *lines {
                            ui.label(*line);
                        }
                        ui.add_space(14.0);
                    }
                });
            });
    }
}
