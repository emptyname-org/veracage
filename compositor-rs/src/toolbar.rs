//! Phase 3 spike: an egui toolbar rendered INTO the compositor's own GL context.
//!
//! `egui_glow` shares smithay's EGL context via
//! `smithay::backend::egl::get_proc_address` — this is the "one real UI unknown"
//! the plan flagged. We drive egui manually (no `egui-winit`): the compositor
//! already decodes pointer events, so we feed those in and paint into the
//! currently-bound framebuffer each Redraw, on top of the sandbox windows.
//!
//! What this module guarantees: it constructs against smithay's context, paints
//! every frame, and NEVER crashes or hangs the compositor — if the GL painter
//! can't be created it returns `None` and the compositor runs on with no toolbar.
//!
//! What still needs a real box (visual iteration, not architecture): the output
//! is rendered `Transform::Flipped180`, so egui's top-origin panel and the
//! pointer hit-test may need a y-flip to line up; layout, spacing, fonts, and the
//! feel of input routing are all things to tune once it's on screen.

use std::sync::Arc;

/// Height of the toolbar strip, in logical points (== logical px for window
/// placement, since a point and a logical pixel are the same size). The sandbox
/// space is offset down by this much (see xdg_shell) so app titlebars aren't
/// hidden under the overlay, and the strip is gated from sandbox input.
pub const TOOLBAR_HEIGHT: i32 = 44;

/// What a menu selection maps to. Clipboard actions reuse the in-process bridge
/// (identical to the Ctrl+Alt+V/C keybinds); LaunchApp asks a vault's leader (over
/// its veracage-owned app socket) to launch enabled app `index`; Command signals
/// the human-uid broker (the compositor can't spawn a human GUI / pkexec / open
/// host files — see `request_command`); Quit stops the compositor loop.
pub enum ToolbarAction {
    None,
    ClipPush, // host selection -> sandbox
    ClipPull, // sandbox selection -> host
    LaunchApp { sock: std::path::PathBuf, index: usize },
    Command(&'static str), // broker verbs: open/configure/settings/close/import/export
    Quit,                  // stop the compositor loop (in-process)
}

/// One open vault's launchers, discovered from `/run/veracage/rt/<id>.apps`:
/// the app socket to poke, the volume label (window title), the enabled app
/// names (button labels, in order), and the "opener" — the app index the desktop
/// tile launches to open the vault (a file manager if enabled, else the first app;
/// `None` if the vault has no enabled apps).
pub struct LeaderApps {
    pub sock: std::path::PathBuf,
    pub label: String,
    pub opener: Option<usize>,
    pub names: Vec<String>,
}

pub struct Toolbar {
    ctx: egui::Context,
    painter: egui_glow::Painter,
    events: Vec<egui::Event>,
    pointer: egui::Pos2,
    /// Reserved strip height, in logical points, at the top of the window.
    pub height: f32,
    /// Dark vs light egui visuals. Default LIGHT; the human side passes
    /// `VERACAGE_THEME=dark` (from config) when it spawns the compositor.
    dark: bool,
    /// When true, draw the desktop (mounted-vault tiles) as an OVERLAY on top of
    /// the app windows — the ⌂ Home button toggles it, so the desktop is reachable
    /// even with a maximized app up (it's normally only shown when no window maps).
    show_desktop: bool,
}

impl Toolbar {
    /// Build the toolbar. MUST be called with smithay's EGL context CURRENT (e.g.
    /// right after `backend.bind()` in the Redraw path). Returns `None` if the GL
    /// painter can't be created, so the compositor degrades to no-toolbar.
    pub fn new() -> Option<Self> {
        // SAFETY: called with the winit/EGL context current; get_proc_address is
        // only valid then (documented). glow just records the function pointers.
        let gl = unsafe {
            glow::Context::from_loader_function(|s| smithay::backend::egl::get_proc_address(s))
        };
        let painter = match egui_glow::Painter::new(Arc::new(gl), "", None, false) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!("toolbar: egui_glow painter init failed: {e}");
                return None;
            }
        };
        Some(Self {
            ctx: egui::Context::default(),
            painter,
            events: Vec::new(),
            pointer: egui::Pos2::ZERO,
            height: TOOLBAR_HEIGHT as f32,
            dark: std::env::var("VERACAGE_THEME").as_deref() == Ok("dark"),
            show_desktop: false,
        })
    }

    /// Feed a pointer move (logical points, output coordinates).
    pub fn pointer_moved(&mut self, x: f64, y: f64) {
        self.pointer = egui::pos2(x as f32, y as f32);
        self.events.push(egui::Event::PointerMoved(self.pointer));
    }

    /// Feed a primary-button press/release at the current pointer position.
    pub fn pointer_button(&mut self, pressed: bool) {
        self.events.push(egui::Event::PointerButton {
            pos: self.pointer,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        });
    }

    /// True if a logical-y coordinate falls within the toolbar strip — so the
    /// compositor can withhold that event from the sandbox windows underneath.
    pub fn contains_y(&self, y_logical: f64) -> bool {
        (y_logical as f32) < self.height
    }

    /// True if the toolbar's OWN last-tracked pointer is over the strip. Used to
    /// gate clicks: the compositor returns early on toolbar motion (before it
    /// updates smithay's pointer), so smithay's location is stale for a click on
    /// the strip — but egui's tracked pointer is always current.
    pub fn over_strip(&self) -> bool {
        self.contains_y(self.pointer.y as f64)
    }

    /// True if egui is currently using the pointer — i.e. a menu dropdown is open
    /// or a widget is active. Menus extend BELOW the strip, so the compositor also
    /// withholds pointer events from the sandbox while this holds, so a click on a
    /// dropdown item reaches egui rather than the app underneath.
    pub fn wants_pointer(&self) -> bool {
        // While the desktop overlay is up it covers the app: gate ALL pointer
        // events to egui so clicks hit the tiles, not the app underneath.
        self.ctx.wants_pointer_input() || self.show_desktop
    }

    /// Run the UI and paint it into the currently-bound framebuffer. `size_px` is
    /// the winit framebuffer size (physical pixels), `scale` the output fractional
    /// scale. MUST be called with the EGL context current. Returns the button
    /// action for this frame, if any.
    pub fn render(
        &mut self,
        size_px: (i32, i32),
        scale: f64,
        leaders: &[LeaderApps],
        has_windows: bool,
    ) -> ToolbarAction {
        let ppp = (scale as f32).max(1.0);
        self.ctx.set_pixels_per_point(ppp);
        self.ctx.set_visuals(if self.dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        });
        // Fonts ~50% bigger, matching the agent windows (theme::bump_fonts).
        // Absolute sizes, so calling every frame is idempotent.
        self.ctx.style_mut(|s| {
            use egui::FontFamily::{Monospace, Proportional};
            use egui::{FontId, TextStyle};
            s.text_styles.insert(TextStyle::Small, FontId::new(14.0, Proportional));
            s.text_styles.insert(TextStyle::Body, FontId::new(18.0, Proportional));
            s.text_styles.insert(TextStyle::Button, FontId::new(18.0, Proportional));
            s.text_styles.insert(TextStyle::Heading, FontId::new(28.0, Proportional));
            s.text_styles.insert(TextStyle::Monospace, FontId::new(18.0, Monospace));
        });
        let (pw, ph) = (size_px.0.max(1) as f32, size_px.1.max(1) as f32);
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(pw / ppp, ph / ppp),
            )),
            events: std::mem::take(&mut self.events),
            ..Default::default()
        };

        let mut action = ToolbarAction::None;
        let height = self.height;
        // Local copy the closure can mutate (can't touch &mut self inside ctx.run).
        let mut show_desktop = self.show_desktop;
        let full = self.ctx.run(raw, |ctx| {
            egui::TopBottomPanel::top("veracage_menu")
                .exact_height(height)
                .show(ctx, |ui| {
                    egui::menu::bar(ui, |ui| {
                        // ⌂ Home: toggle the desktop (mounted-vault tiles) as an
                        // overlay, so it's reachable even with a maximized app up.
                        if ui
                            .selectable_label(show_desktop, "\u{2302}")
                            .on_hover_text("Show the Veracage desktop")
                            .clicked()
                        {
                            show_desktop = !show_desktop;
                        }
                        ui.menu_button("File", |ui| {
                            if ui.button("Open vault\u{2026}").clicked() {
                                action = ToolbarAction::Command("open");
                                ui.close_menu();
                            }
                            if ui.button("Import file\u{2026}").clicked() {
                                action = ToolbarAction::Command("import");
                                ui.close_menu();
                            }
                            if ui.button("Export file\u{2026}").clicked() {
                                action = ToolbarAction::Command("export");
                                ui.close_menu();
                            }
                            ui.separator();
                            if ui.button("Close vault").clicked() {
                                action = ToolbarAction::Command("close");
                                ui.close_menu();
                            }
                            if ui.button("Quit").clicked() {
                                action = ToolbarAction::Quit;
                                ui.close_menu();
                            }
                        });
                        ui.menu_button("Edit", |ui| {
                            if ui.button("Paste \u{2192} Veracage").clicked() {
                                action = ToolbarAction::ClipPush;
                                ui.close_menu();
                            }
                            if ui.button("Copy \u{2192} host").clicked() {
                                action = ToolbarAction::ClipPull;
                                ui.close_menu();
                            }
                        });
                        ui.menu_button("Apps", |ui| {
                            // One item per enabled app of each open vault; clicking
                            // asks that vault's leader to launch it.
                            if leaders.is_empty() {
                                ui.add_enabled(false, egui::Button::new("(open a vault first)"));
                            }
                            for l in leaders {
                                for (i, name) in l.names.iter().enumerate() {
                                    if ui.button(name).clicked() {
                                        action = ToolbarAction::LaunchApp {
                                            sock: l.sock.clone(),
                                            index: i,
                                        };
                                        ui.close_menu();
                                    }
                                }
                            }
                            // Configure the enabled-app set — last line of Apps.
                            ui.separator();
                            if ui.button("Configure apps\u{2026}").clicked() {
                                action = ToolbarAction::Command("configure");
                                ui.close_menu();
                            }
                        });
                        ui.menu_button("Settings", |ui| {
                            if ui.button("Settings\u{2026}").clicked() {
                                action = ToolbarAction::Command("settings");
                                ui.close_menu();
                            }
                        });
                        ui.menu_button("Help", |ui| {
                            if ui.button("About Veracage\u{2026}").clicked() {
                                action = ToolbarAction::Command("about");
                                ui.close_menu();
                            }
                        });
                    });
                });

            // Desktop — the mounted-volume "home": shown when no window is mapped,
            // OR on demand as an overlay via the ⌂ Home button (so it's reachable
            // with a maximized app up). The filled CentralPanel is composited AFTER
            // the sandbox surfaces, so as an overlay it naturally covers the app.
            // With no vault it prompts to open one; with vaults mounted it shows a
            // clickable tile per vault that launches the vault's opener.
            if !has_windows || show_desktop {
                egui::CentralPanel::default().show(ctx, |ui| {
                    if leaders.is_empty() {
                        ui.vertical_centered(|ui| {
                            ui.add_space(ui.available_height() * 0.38);
                            ui.label(
                                egui::RichText::new("\u{1F512}  No vault open")
                                    .size(28.0)
                                    .weak(),
                            );
                            ui.add_space(4.0);
                            ui.label(
                                egui::RichText::new("File \u{25B8} Open vault\u{2026}").weak(),
                            );
                        });
                    } else {
                        ui.add_space(24.0);
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing = egui::vec2(16.0, 16.0);
                            for l in leaders {
                                let text = egui::RichText::new(format!(
                                    "\u{1F4C1}\n{}",
                                    l.label
                                ))
                                .size(20.0);
                                let tile =
                                    ui.add_sized([160.0, 116.0], egui::Button::new(text));
                                let tile = tile.on_hover_text(match l.opener {
                                    Some(_) => "Open this vault",
                                    None => "No app enabled \u{2014} Apps \u{25B8} Configure",
                                });
                                if let Some(idx) = l.opener {
                                    if tile.clicked() {
                                        action = ToolbarAction::LaunchApp {
                                            sock: l.sock.clone(),
                                            index: idx,
                                        };
                                        show_desktop = false; // launched → back to the app
                                    }
                                }
                            }
                        });
                    }
                });
            }
        });
        self.show_desktop = show_desktop;

        let clipped = self.ctx.tessellate(full.shapes, full.pixels_per_point);
        self.painter.paint_and_update_textures(
            [size_px.0.max(1) as u32, size_px.1.max(1) as u32],
            full.pixels_per_point,
            &clipped,
            &full.textures_delta,
        );
        action
    }
}

// --------------------------------------------------------- discovery -------

/// The shared compositor runtime dir (must match COMPOSITOR_RUNTIME in wayland.py).
const RUNTIME_DIR: &str = "/run/veracage/rt";

/// The file the human-side broker polls for commands (verb line). Must match
/// CMD_REQ in agent-rs/src/broker.rs.
const CMD_REQ: &str = "cmd.req";

/// Emit a command to the human-uid broker. The compositor runs as the `veracage`
/// uid and cannot spawn a human GUI / `pkexec` / open host files, so it drops a
/// one-line verb into its own runtime dir; the broker (human uid) polls the file's
/// mtime and dispatches the verb. Verbs: open/configure/settings/close/import/export.
///
/// Security: `/run/veracage/rt` is `0711 veracage`, so a same-uid attacker CANNOT
/// create/forge this file — only the compositor writes it, and a compositor menu
/// click is a genuine user action he can't inject. The verb isn't secret, so the
/// file is world-readable (the broker reads it by exact path through the 0711 dir);
/// at most an attacker learns a command was issued.
pub fn request_command(verb: &str) {
    let path = std::path::Path::new(RUNTIME_DIR).join(CMD_REQ);
    // Truncating write bumps the mtime (the broker's change signal) and carries the
    // verb. fs::write always updates mtime, so repeated same-verb clicks re-fire.
    if let Err(e) = std::fs::write(&path, format!("{verb}\n")) {
        tracing::warn!("toolbar: command {verb}: {e}");
        return;
    }
    // The compositor runs with umask 077 (socket hygiene), so the file lands 0600;
    // make it broker-readable explicitly (the verb is not a secret).
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644));
}

/// Scan `RUNTIME_DIR` for the `*.apps` files each vault's leader writes, building
/// the toolbar's launcher list. Cheap; called on a ~1s throttle from the redraw.
/// Format per file: line 1 = the app socket's filename, lines 2.. = app names.
pub fn scan_leaders() -> Vec<LeaderApps> {
    let runtime = std::path::Path::new(RUNTIME_DIR);
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(runtime) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("apps") {
            continue;
        }
        // Only a real regular file, size-capped — never block on a FIFO or OOM on
        // a huge/looping file a same-uid process could plant in the runtime dir.
        let Ok(md) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !md.file_type().is_file() || md.len() > 64 * 1024 {
            continue;
        }
        let Ok(body) = std::fs::read_to_string(&path) else {
            continue;
        };
        let mut lines = body.lines();
        let Some(sockname) = lines.next().filter(|s| !s.is_empty()) else {
            continue;
        };
        // Line 0 must be a plain filename in the runtime dir: reject "/" and ".."
        // so a crafted .apps can't point launch_app at an arbitrary socket path.
        if sockname.contains('/') || sockname == ".." {
            continue;
        }
        let label = lines.next().unwrap_or("Vault").to_string();
        // Opener index: which app the desktop tile launches. `-1` (or an
        // out-of-range value, checked once names are known) means "no opener".
        let opener_raw: i64 = lines.next().and_then(|s| s.trim().parse().ok()).unwrap_or(-1);
        let names: Vec<String> = lines.filter(|l| !l.is_empty()).map(str::to_string).collect();
        let opener = usize::try_from(opener_raw).ok().filter(|&i| i < names.len());
        out.push(LeaderApps {
            sock: runtime.join(sockname),
            label,
            opener,
            names,
        });
    }
    out
}

/// Ask a vault's leader to launch enabled app `index` by poking its app socket
/// with a bare index line. The blocking connect+write runs on a short-lived
/// thread so a stalled or missing leader socket can never freeze the compositor's
/// single-threaded event loop (matching the clipboard bridge's discipline).
pub fn launch_app(sock: &std::path::Path, index: usize) {
    let sock = sock.to_path_buf();
    // Builder::spawn (not thread::spawn) so an OS thread-creation failure returns
    // Err and is dropped, never panicking and unwinding the compositor.
    let _ = std::thread::Builder::new()
        .name("veracage-launch".into())
        .spawn(move || {
            use std::io::Write;
            use std::os::unix::net::UnixStream;
            match UnixStream::connect(&sock) {
                Ok(mut s) => {
                    let _ = s.write_all(format!("{index}\n").as_bytes());
                }
                Err(e) => tracing::warn!("toolbar: launch app {index}: {e}"),
            }
        });
}
