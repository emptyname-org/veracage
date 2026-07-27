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
            "Apps also work before any volume is mounted, which makes an empty \
             Veracage a private scratchpad.",
        ],
    ),
    (
        "Nothing is stored in Veracage",
        &[
            "Veracage has no storage of its own. A file is kept only if you save \
             it into a mounted volume or into the shared directory.",
            "Everything else lives in memory: the home directory apps start in, \
             their settings, caches and downloads. It is discarded when Veracage \
             quits, without asking.",
            "That memory is private to Veracage. It is held in Veracage's own \
             mount namespace, owned by the Veracage system user, so no other \
             program running as you can read it or even reach its path.",
            "So save your work into one of the volume directories, or into \
             /exchange to pass it to the host.",
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
            "Apps render on the host GPU automatically, so video and 3D are \
             smooth. On a Host without a usable GPU they fall back to software \
             rendering.",
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
        crate::theme::content_panel(ctx, |ui| {
            for (title, lines) in SECTIONS {
                ui.heading(*title);
                ui.add_space(2.0);
                for line in *lines {
                    ui.label(*line);
                }
                ui.add_space(14.0);
            }
        });
    }
}
