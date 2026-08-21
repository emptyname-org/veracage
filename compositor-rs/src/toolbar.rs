//! The egui toolbar, rendered INTO the compositor's own GL context.
//!
//! `egui_glow` shares smithay's EGL context via
//! `smithay::backend::egl::get_proc_address`. egui is driven manually (no
//! `egui-winit`): the compositor already decodes pointer events, so we feed those
//! in and paint into the currently-bound framebuffer each Redraw, on top of the
//! sandbox windows.
//!
//! What this module guarantees: it constructs against smithay's context, paints
//! every frame, and NEVER crashes or hangs the compositor: if the GL painter
//! can't be created it returns `None` and the compositor runs on with no toolbar.

use std::collections::HashMap;
use std::sync::Arc;

use smithay::utils::{Physical, Rectangle};

/// Height of the toolbar strip, in logical points (== logical px for window
/// placement, since a point and a logical pixel are the same size). The sandbox
/// space is offset down by this much (see xdg_shell) so app titlebars aren't
/// hidden under the overlay, and the strip is gated from sandbox input.
pub const TOOLBAR_HEIGHT: i32 = 40;

/// What a menu selection maps to. Clipboard actions reuse the in-process bridge
/// (identical to the Ctrl+Alt+V/C keybinds); LaunchApp asks a volume's leader
/// (over its veracage-owned app socket) to launch enabled app `index`; Command
/// signals the human-uid broker (the compositor can't spawn a human GUI /
/// pkexec / open host files, see `request_command`); Quit stops the compositor.
pub enum ToolbarAction {
    None,
    ClipPush, // host selection -> sandbox
    ClipPull, // sandbox selection -> host
    LaunchApp { sock: std::path::PathBuf, index: usize },
    /// Broker verbs: open / configure / settings / appearance / shortcuts /
    /// exchange / help / about.
    Command(String),
    /// Dismount these volumes (by label), in order.
    CloseVolumes(Vec<String>),
    /// Close Veracage, dismounting the mounted volumes first (see `begin_quit`).
    Quit,
}

/// One session's launchers, discovered from `/run/veracage/rt/<id>.apps`:
/// the app socket to poke, the window-title label, the open-volume labels, the
/// enabled app names (button labels, in order), and the "opener" app index
/// (carried in the wire format, not used here).
pub struct LeaderApps {
    pub sock: std::path::PathBuf,
    pub label: String,
    pub volumes: Vec<String>, // open-volume labels (for the Dismount menu)
    pub opener: Option<usize>,
    pub names: Vec<String>,
}

/// One configured app, published by the human side to `/run/veracage/pub/
/// config.apps` so the Apps menu is populated before any volume is mounted.
/// Clicking one asks the broker to run the open flow with that app.
#[derive(Clone, PartialEq)]
pub struct ConfigApp {
    pub key: String,
    pub name: String,
}

/// A cached menu icon: the texture (None if the icon file is absent/invalid)
/// plus the source file's mtime, so an updated icon reloads on the next scan.
struct IconSlot {
    mtime: u128,
    tex: Option<egui::TextureHandle>,
}

pub struct Toolbar {
    ctx: egui::Context,
    painter: egui_glow::Painter,
    events: Vec<egui::Event>,
    pointer: egui::Pos2,
    /// Reserved strip height, in logical points, at the top of the window.
    pub height: f32,
    /// Dark vs light egui visuals, from `pub/theme` and refreshed live on the
    /// discovery scan. Light until the human side publishes anything.
    dark: bool,
    /// Menu icons by config-app key, refreshed on the ~1s scan (never per frame).
    icons: HashMap<String, IconSlot>,
    /// Display name -> config key, to find icons for leader-published app names.
    name_to_key: HashMap<String, String>,
    /// Base UI point size (the host desktop's, or the configured override), and
    /// the font file currently installed. Refreshed live from `pub/font`.
    base_size: f32,
    font_file: String,
    /// Current clipboard-shortcut labels, shown in the Clipboard menu.
    copy_out_label: String,
    paste_in_label: String,
    /// Menu-item icons: Veracage's own two-color glyphs (mono_icons), drawn
    /// once at construction in the theme's ink. App icons stay host-colored.
    menu_icons: HashMap<String, egui::TextureHandle>,
    /// The frame `run()` produced, painted by `paint()` after the windows are
    /// composited: tessellated primitives, texture updates, pixels per point.
    pending: Option<(Vec<egui::ClippedPrimitive>, egui::TexturesDelta, f32)>,
    /// Set while something slow is happening (unlocking a volume, an app
    /// starting): the spinner turns over the desktop icon. The text itself is not
    /// drawn, it only names the note in the debug log.
    status: Option<String>,
    /// The user-facing banner currently shown, if any.
    notice: Option<Notice>,
}

/// A user-facing banner over the desktop: something happened that the human did
/// not ask for and would otherwise not see (a failed launch, an idle dismount).
/// Expires after NOTICE_TTL. Operations the human just triggered do NOT get one.
struct Notice {
    text: String,
    at: std::time::Instant,
}

impl Notice {
    fn expired(&self) -> bool {
        self.at.elapsed() >= NOTICE_TTL
    }
}

/// Veracage's own two-color menu glyphs, baked into textures in the theme's ink.
/// Done once per theme (at construction and on a theme change), never per frame.
fn bake_menu_icons(
    ctx: &egui::Context,
    dark: bool,
) -> HashMap<String, egui::TextureHandle> {
    let ink = if dark {
        egui::Color32::from_gray(222)
    } else {
        egui::Color32::from_gray(35)
    };
    let mut icons = HashMap::new();
    for key in MENU_ICON_KEYS {
        if let Some(img) = crate::mono_icons::icon(key, ink) {
            let tex = ctx.load_texture(
                format!("veracage-menu-{key}"),
                img,
                egui::TextureOptions::LINEAR,
            );
            icons.insert((*key).to_string(), tex);
        }
    }
    icons
}

/// The progress spinner (see the status spec above `pick_status`): a full ring of
/// short radial strokes over the desktop icon, with a brightness wave running
/// clockwise. Diameter and stroke width are logical points.
const SPINNER_DIAMETER: f32 = 46.0;
const SPINNER_STROKE: f32 = 4.0;
const SPINNER_STROKES: usize = 12;
/// Inner end of each stroke, as a fraction of the outer radius.
const SPINNER_INNER: f32 = 0.6;
/// How long the wave takes to go round once.
const SPINNER_CYCLE: f32 = 1.2;
/// Alpha of the stroke the wave has just left, and how sharply the tail decays
/// (>1 keeps the bright part short without ever making a stroke vanish).
const SPINNER_DIM: f32 = 0.14;
const SPINNER_DECAY: f32 = 1.7;
const SPINNER_ACCENT: egui::Color32 = egui::Color32::from_rgb(237, 100, 75);

/// Alpha of the stroke sitting `at` (a fraction of the way round the ring) while
/// the wave's head is at `head`: full at the head itself, decaying with the
/// distance the head has already travelled past it, down to SPINNER_DIM for the
/// stroke the head is about to reach. Continuous in both arguments and across the
/// wrap, which is what makes the ring read as one brightness moving rather than
/// strokes switching on and off.
fn stroke_alpha(head: f32, at: f32) -> f32 {
    let behind = (head - at).rem_euclid(1.0);
    SPINNER_DIM + (1.0 - SPINNER_DIM) * (1.0 - behind).powf(SPINNER_DECAY)
}

/// Where the spinner is drawn: centred on the desktop icon, the one fixed landmark
/// on the backdrop, so it appears in the same place whether or not app windows are
/// open.
fn spinner_center(w_logical: i32, h_logical: i32) -> egui::Pos2 {
    let (x, y) = hint_icon_pos(w_logical, h_logical);
    egui::pos2(
        x as f32 + HINT_ICON_PX as f32 * 0.5,
        y as f32 + HINT_ICON_PX as f32 * 0.5,
    )
}

/// Draw the ring centred on `center`. Every stroke is drawn every frame, only its
/// alpha moves.
fn draw_spinner(ui: &mut egui::Ui, center: egui::Pos2, diameter: f32) {
    // This animation IS the progress indication, so keep asking for frames.
    ui.ctx().request_repaint();
    let outer = diameter * 0.5;
    let head = (ui.input(|i| i.time) / SPINNER_CYCLE as f64).rem_euclid(1.0) as f32;
    let painter = ui.painter();
    for i in 0..SPINNER_STROKES {
        let at = i as f32 / SPINNER_STROKES as f32;
        // Screen y grows downward, so increasing the angle turns clockwise.
        let angle = std::f32::consts::TAU * at - std::f32::consts::FRAC_PI_2;
        let dir = egui::vec2(angle.cos(), angle.sin());
        painter.line_segment(
            [center + dir * outer * SPINNER_INNER, center + dir * outer],
            egui::Stroke::new(
                SPINNER_STROKE,
                SPINNER_ACCENT.gamma_multiply(stroke_alpha(head, at)),
            ),
        );
    }
}

/// How long a transient toolbar notice (e.g. a failed-launch banner) stays up.
const NOTICE_TTL: std::time::Duration = std::time::Duration::from_secs(6);

/// The shared app icon PNG (192px = 2x the 96pt display box, for HiDPI), the
/// same file the About window uses. Regenerate with `convert
/// Icons/veracage_icon_turquoise_transparent_corners.png -resize 192x192
/// PNG32:Icons/veracage_icon.png` after new art.
pub const DESKTOP_ICON_PNG: &[u8] = include_bytes!("../../Icons/veracage_icon.png");
/// The icon PNG's pixel side, and thus its logical display side scale factor:
/// the 192px art represents a 96pt icon, so its buffer scale is 2.
pub const ICON_BUFFER_SCALE: i32 = 2;

/// The hint icon's displayed square side, in LOGICAL points.
pub const HINT_ICON_PX: i32 = 96;

/// The hint icon's top-left origin in LOGICAL points: horizontally centered,
/// ~30% down. Given the LOGICAL viewport size. Shared by the smithay render
/// element (winit.rs, which draws the icon) and the egui text (centered under
/// it); winit scales the result to physical for the render element.
pub fn hint_icon_pos(w_logical: i32, h_logical: i32) -> (i32, i32) {
    ((w_logical - HINT_ICON_PX) / 2, h_logical * 3 / 10)
}

/// Decode the embedded icon PNG to (width, height, straight-alpha RGBA bytes),
/// for the smithay memory buffer that draws the desktop hint icon.
pub fn decode_icon_rgba() -> Option<(u32, u32, Vec<u8>)> {
    let mut reader = png::Decoder::new(std::io::Cursor::new(DESKTOP_ICON_PNG)).read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    if info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    let rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => buf[..info.buffer_size()].to_vec(),
        png::ColorType::Rgb => buf[..info.buffer_size()]
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        _ => return None,
    };
    (rgba.len() as u32 == info.width * info.height * 4).then_some((info.width, info.height, rgba))
}

/// Menu-item icon keys (each names a glyph in mono_icons).
pub(crate) const MENU_ICON_KEYS: &[&str] = &[
    "mount", "exchange", "dismount", "quit", "copy_out", "paste_in",
    "configure_apps", "settings", "shortcuts", "help", "about",
];

/// Fallback base size in LOGICAL PIXELS when none is provided (the human side
/// passes the already-resolved pixel size via VERACAGE_FONT_SIZE / pub/font).
const FALLBACK_SIZE: f32 = 13.0;

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
        let ctx = egui::Context::default();
        // System UI font + base size, resolved by the human side and passed in
        // VERACAGE_FONT_FILE / VERACAGE_FONT_SIZE; empty/unset keeps egui's face.
        let font_file = std::env::var("VERACAGE_FONT_FILE").unwrap_or_default();
        crate::fonts::install_from_file(&ctx, &font_file);
        let base_size = std::env::var("VERACAGE_FONT_SIZE")
            .ok()
            .and_then(|s| s.trim().parse::<f32>().ok())
            .map(|s| s.clamp(6.0, 48.0))
            .unwrap_or(FALLBACK_SIZE);
        let dark = initial_dark();
        let menu_icons = bake_menu_icons(&ctx, dark);
        Some(Self {
            ctx,
            painter,
            events: Vec::new(),
            pointer: egui::Pos2::ZERO,
            height: TOOLBAR_HEIGHT as f32,
            dark,
            icons: HashMap::new(),
            name_to_key: HashMap::new(),
            base_size,
            font_file,
            copy_out_label: crate::shortcuts::DEFAULT_COPY_OUT.to_string(),
            paste_in_label: crate::shortcuts::DEFAULT_PASTE_IN.to_string(),
            menu_icons,
            pending: None,
            status: None,
            notice: None,
        })
    }

    /// Show a transient banner (e.g. a leader-reported failed launch). Cleared
    /// automatically after NOTICE_TTL.
    pub fn set_notice(&mut self, msg: String) {
        self.notice = Some(Notice { text: msg, at: std::time::Instant::now() });
    }

    /// True while egui still wants to animate (an open menu, a hover transition,
    /// the notice banner countdown), so the render loop keeps drawing until it
    /// settles instead of freezing mid-animation.
    pub fn wants_repaint(&self) -> bool {
        self.ctx.has_requested_repaint()
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

    /// True if a logical-y coordinate falls within the toolbar strip, so the
    /// compositor can withhold that event from the sandbox windows underneath.
    pub fn contains_y(&self, y_logical: f64) -> bool {
        (y_logical as f32) < self.height
    }

    /// True if the toolbar's OWN last-tracked pointer is over the strip. Used to
    /// gate clicks: the compositor returns early on toolbar motion (before it
    /// updates smithay's pointer), so smithay's location is stale for a click on
    /// the strip, but egui's tracked pointer is always current.
    pub fn over_strip(&self) -> bool {
        self.contains_y(self.pointer.y as f64)
    }

    /// True if egui is currently using the pointer, i.e. a menu dropdown is open
    /// or a widget is active. Menus extend BELOW the strip, so the compositor also
    /// withholds pointer events from the sandbox while this holds, so a click on a
    /// dropdown item reaches egui rather than the app underneath.
    pub fn wants_pointer(&self) -> bool {
        self.ctx.wants_pointer_input()
    }

    /// Update the clipboard-shortcut labels shown in the Clipboard menu.
    pub fn refresh_shortcuts(&mut self, copy_out: String, paste_in: String) {
        self.copy_out_label = copy_out;
        self.paste_in_label = paste_in;
    }

    /// Apply a live theme change (from `pub/theme`). The visuals themselves are
    /// set per frame from `self.dark`, so only the menu glyphs, which are baked
    /// once in the theme's ink, have to be redrawn. True if anything changed.
    pub fn refresh_theme(&mut self, dark: bool) -> bool {
        if dark == self.dark {
            return false;
        }
        self.dark = dark;
        self.menu_icons = bake_menu_icons(&self.ctx, dark);
        true
    }

    /// Apply a live font/size change (from `pub/font`). Re-installs the font only
    /// when the file path actually changes (set_fonts rebuilds the atlas). Called
    /// from the discovery scan, never per frame. True if anything changed.
    pub fn refresh_font(&mut self, path: &str, base: f32) -> bool {
        let changed = path != self.font_file || base != self.base_size;
        if path != self.font_file {
            crate::fonts::install_from_file(&self.ctx, path);
            self.font_file = path.to_string();
        }
        self.base_size = base;
        changed
    }

    /// The progress note currently shown, for the debug log.
    pub fn status_text(&self) -> Option<String> {
        self.status.clone()
    }

    /// Set (or clear, with None) the progress note shown beside a spinner:
    /// "Unlocking <volume>" while the helper derives the key, "Starting <app>"
    /// until its window appears. True if the note changed.
    pub fn set_status(&mut self, status: Option<String>) -> bool {
        if self.status == status {
            return false;
        }
        self.status = status;
        true
    }

    /// Refresh the menu-icon cache for the configured apps. Called from the ~1s
    /// discovery scan, NOT per frame, so the per-frame render never stats files.
    pub fn refresh_icons(&mut self, cfg_apps: &[ConfigApp]) {
        self.name_to_key = cfg_apps
            .iter()
            .map(|a| (a.name.clone(), a.key.clone()))
            .collect();
        // Drop cache entries for apps no longer configured.
        let live: std::collections::HashSet<&str> =
            cfg_apps.iter().map(|a| a.key.as_str()).collect();
        self.icons.retain(|k, _| live.contains(k.as_str()));
        for a in cfg_apps {
            let path = std::path::Path::new(PUB_DIR).join("icons").join(format!("{}.rgba", a.key));
            let mtime = file_mtime(&path);
            if let Some(slot) = self.icons.get(&a.key) {
                if slot.mtime == mtime {
                    continue; // unchanged (or still absent)
                }
            }
            let tex = load_icon_rgba(&path).map(|img| {
                self.ctx.load_texture(
                    format!("veracage-app-{}", a.key),
                    img,
                    egui::TextureOptions::LINEAR,
                )
            });
            self.icons.insert(a.key.clone(), IconSlot { mtime, tex });
        }
    }

    /// Run the UI (CPU only, no GL): produce this frame's primitives for a later
    /// `paint()`. `size_px` is the winit framebuffer size (physical pixels),
    /// `scale` the output fractional scale. Returns the button action for this
    /// frame plus the region egui painted (physical pixels), which the caller
    /// must report to the damage tracker: the overlay is drawn outside it, so
    /// whatever lies beneath must be re-rendered every painted frame.
    pub fn run(
        &mut self,
        size_px: (i32, i32),
        scale: f64,
        leaders: &[LeaderApps],
        cfg_apps: &[ConfigApp],
    ) -> (ToolbarAction, Option<Rectangle<i32, Physical>>) {
        let ppp = (scale as f32).max(1.0);
        self.ctx.set_pixels_per_point(ppp);
        self.ctx.set_visuals(if self.dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        });
        // Text sizes derived from the base point size (the host desktop's, or the
        // configured override), matching the agent windows. Idempotent per frame.
        let base = self.base_size.clamp(6.0, 48.0);
        self.ctx.style_mut(|s| {
            use egui::FontFamily::{Monospace, Proportional};
            use egui::{FontId, TextStyle};
            s.text_styles.insert(TextStyle::Small, FontId::new((base * 0.85).round(), Proportional));
            s.text_styles.insert(TextStyle::Body, FontId::new(base, Proportional));
            s.text_styles.insert(TextStyle::Button, FontId::new(base, Proportional));
            s.text_styles.insert(TextStyle::Heading, FontId::new((base * 1.5).round(), Proportional));
            s.text_styles.insert(TextStyle::Monospace, FontId::new(base, Monospace));
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
        // Split borrows for the run() closure: the closure reads the icon caches
        // while `self.ctx.run` holds the context.
        let icons = &self.icons;
        let name_to_key = &self.name_to_key;
        let menu_icons = &self.menu_icons;
        let notice = &self.notice;
        let status = self.status.as_deref();
        let dark = self.dark;
        let copy_out_label = self.copy_out_label.as_str();
        let paste_in_label = self.paste_in_label.as_str();
        let icon_by_name = |name: &str| -> Option<&egui::TextureHandle> {
            icons.get(name_to_key.get(name)?)?.tex.as_ref()
        };
        let icon_by_key = |key: &str| -> Option<&egui::TextureHandle> {
            icons.get(key)?.tex.as_ref()
        };
        let mi = |key: &str| -> Option<&egui::TextureHandle> { menu_icons.get(key) };
        let full = self.ctx.run(raw, |ctx| {
            egui::TopBottomPanel::top("veracage_menu")
                .exact_height(height)
                .show(ctx, |ui| {
                    // Center the one-line menu row vertically in the strip. The
                    // row is as tall as its buttons (menu style: no vertical
                    // button padding), never less than interact_size.
                    let row_h = ui
                        .text_style_height(&egui::TextStyle::Button)
                        .max(ui.spacing().interact_size.y);
                    ui.add_space(((ui.available_height() - row_h) * 0.5).max(0.0));
                    egui::menu::bar(ui, |ui| {
                        ui.menu_button("File", |ui| {
                            if ui.add(menu_item(mi("mount"), "Open volume...")).clicked() {
                                action = ToolbarAction::Command("open".into());
                                ui.close_menu();
                            }
                            if ui.add(menu_item(mi("exchange"), "Shared directory")).clicked() {
                                action = ToolbarAction::Command("exchange".into());
                                ui.close_menu();
                            }
                            // Close volume ▸ one item per open volume across
                            // the session, plus All once there is more than one.
                            // Absent when no volume is open.
                            let mut vols: Vec<String> =
                                leaders.iter().flat_map(|l| l.volumes.clone()).collect();
                            vols.sort();
                            if !vols.is_empty() {
                                ui.separator();
                                ui.menu_button("Close volume", |ui| {
                                    for v in &vols {
                                        if ui.add(menu_item(mi("dismount"), v)).clicked() {
                                            action = ToolbarAction::CloseVolumes(vec![v.clone()]);
                                            ui.close_menu();
                                        }
                                    }
                                    if vols.len() > 1 {
                                        ui.separator();
                                        if ui.add(menu_item(mi("dismount"), "All")).clicked() {
                                            action = ToolbarAction::CloseVolumes(vols.clone());
                                            ui.close_menu();
                                        }
                                    }
                                });
                            }
                            ui.separator();
                            if ui.add(menu_item(mi("quit"), "Quit")).clicked() {
                                action = ToolbarAction::Quit;
                                ui.close_menu();
                            }
                        });
                        ui.menu_button("Clipboard", |ui| {
                            if ui
                                .add(menu_item(mi("copy_out"), "Copy out").shortcut_text(copy_out_label))
                                .clicked()
                            {
                                action = ToolbarAction::ClipPull;
                                ui.close_menu();
                            }
                            if ui
                                .add(menu_item(mi("paste_in"), "Paste in").shortcut_text(paste_in_label))
                                .clicked()
                            {
                                action = ToolbarAction::ClipPush;
                                ui.close_menu();
                            }
                        });
                        ui.menu_button("Apps", |ui| {
                            if leaders.is_empty() {
                                // No volume open: apps operate on volume
                                // contents, so show the configured apps DISABLED
                                // until one is open (File > Open volume).
                                if cfg_apps.is_empty() {
                                    ui.add_enabled(
                                        false,
                                        egui::Button::new("(no apps configured)"),
                                    );
                                }
                                for a in cfg_apps {
                                    ui.add_enabled(false, app_button(icon_by_key(&a.key), &a.name));
                                }
                            }
                            // One item per enabled app of each mounted session;
                            // clicking asks that session's leader to launch it.
                            for l in leaders {
                                for (i, name) in l.names.iter().enumerate() {
                                    if ui.add(app_button(icon_by_name(name), name)).clicked() {
                                        action = ToolbarAction::LaunchApp {
                                            sock: l.sock.clone(),
                                            index: i,
                                        };
                                        ui.close_menu();
                                    }
                                }
                            }
                            // Configure the enabled-app set: last line of Apps.
                            ui.separator();
                            if ui.add(menu_item(mi("configure_apps"), "Configure apps...")).clicked() {
                                action = ToolbarAction::Command("configure".into());
                                ui.close_menu();
                            }
                        });
                        ui.menu_button("Settings", |ui| {
                            if ui.add(menu_item(mi("settings"), "System Integration...")).clicked() {
                                action = ToolbarAction::Command("settings".into());
                                ui.close_menu();
                            }
                            if ui.add(menu_item(mi("settings"), "Appearance...")).clicked() {
                                action = ToolbarAction::Command("appearance".into());
                                ui.close_menu();
                            }
                            if ui.add(menu_item(mi("shortcuts"), "Keyboard and Shortcuts...")).clicked() {
                                action = ToolbarAction::Command("shortcuts".into());
                                ui.close_menu();
                            }
                        });
                        ui.menu_button("Help", |ui| {
                            if ui.add(menu_item(mi("help"), "Help...")).clicked() {
                                action = ToolbarAction::Command("help".into());
                                ui.close_menu();
                            }
                            if ui.add(menu_item(mi("about"), "About Veracage...")).clicked() {
                                action = ToolbarAction::Command("about".into());
                                ui.close_menu();
                            }
                        });
                    });
                });

            // There is no egui desktop: the desktop IS the compositor's backdrop
            // colour plus the Veracage icon, both drawn by the renderer below the
            // app windows (see winit.rs). egui only adds the strip, the spinner
            // and the notice banner, all of which belong above the windows.

            // Something slow is happening (unlocking a volume, an app starting):
            // just a spinner, centred over everything, no frame and no text - it
            // says "wait" without covering what is underneath. Non-interactive, so
            // it never eats a click. Its animation is what keeps asking for frames
            // until the note clears.
            if status.is_some() {
                // Over the desktop icon, the one fixed landmark on the backdrop, so
                // the spinner always appears in the same place whether or not app
                // windows are open.
                let screen = ctx.screen_rect();
                let center = spinner_center(screen.width() as i32, screen.height() as i32);
                egui::Area::new(egui::Id::new("veracage_status"))
                    .order(egui::Order::Foreground)
                    .fixed_pos(center - egui::vec2(SPINNER_DIAMETER, SPINNER_DIAMETER) * 0.5)
                    .interactable(false)
                    .show(ctx, |ui| {
                        let (rect, _) = ui.allocate_exact_size(
                            egui::vec2(SPINNER_DIAMETER, SPINNER_DIAMETER),
                            egui::Sense::hover(),
                        );
                        draw_spinner(ui, rect.center(), SPINNER_DIAMETER);
                    });
            }

            // Transient banner (e.g. a leader-reported failed launch), floating
            // bottom-center over the app/desktop for NOTICE_TTL. Non-interactive.
            if let Some(notice) = notice {
                if !notice.expired() {
                    egui::Area::new(egui::Id::new("veracage_notice"))
                        .order(egui::Order::Foreground)
                        .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -28.0))
                        .interactable(false)
                        .show(ctx, |ui| {
                            let (bg, fg) = if dark {
                                (egui::Color32::from_rgb(96, 30, 30), egui::Color32::from_gray(240))
                            } else {
                                (egui::Color32::from_rgb(250, 224, 224), egui::Color32::from_rgb(120, 20, 20))
                            };
                            egui::Frame::popup(ui.style()).fill(bg).show(ui, |ui| {
                                ui.colored_label(fg, notice.text.as_str());
                            });
                        });
                }
            }
        });

        // Drop an expired notice so it stops repainting and doesn't linger.
        if self.notice.as_ref().is_some_and(Notice::expired) {
            self.notice = None;
        }

        // Union of everything egui actually painted (per-shape visual bounds,
        // clipped). NOT ctx.used_rect(): that unions only panels and windows,
        // so menu popups (Foreground-order areas) were missing from the damage
        // region and an open menu was never presented over the app windows.
        let mut painted = egui::Rect::NOTHING;
        for cs in &full.shapes {
            let b = cs.shape.visual_bounding_rect().intersect(cs.clip_rect);
            if b.is_positive() {
                painted = painted.union(b);
            }
        }
        let clipped = self.ctx.tessellate(full.shapes, full.pixels_per_point);
        let mut textures_delta = full.textures_delta;
        rebake_font_textures(&mut textures_delta);
        // A frame that was never painted (a skipped render) must not lose its
        // texture updates: egui's deltas are INCREMENTAL and never resent, so a
        // dropped font-atlas patch would leave later text rendering from a stale
        // atlas and leak the textures it freed. Shapes describe one whole frame
        // and are simply replaced; deltas accumulate, oldest first.
        if let Some((_, stale, _)) = self.pending.take() {
            let mut merged = stale;
            merged.set.extend(textures_delta.set);
            merged.free.extend(textures_delta.free);
            textures_delta = merged;
        }
        self.pending = Some((clipped, textures_delta, full.pixels_per_point));
        (action, self.painted_rect(painted, size_px))
    }

    /// `used` (egui logical points) as physical pixels, clamped to the
    /// framebuffer. None when egui painted nothing.
    fn painted_rect(&self, used: egui::Rect, size_px: (i32, i32)) -> Option<Rectangle<i32, Physical>> {
        if !used.is_finite() {
            return None;
        }
        let ppp = self.ctx.pixels_per_point();
        let x0 = ((used.min.x * ppp).floor() as i32).max(0);
        let y0 = ((used.min.y * ppp).floor() as i32).max(0);
        let x1 = ((used.max.x * ppp).ceil() as i32).min(size_px.0);
        let y1 = ((used.max.y * ppp).ceil() as i32).min(size_px.1);
        (x1 > x0 && y1 > y0)
            .then(|| Rectangle::new((x0, y0).into(), (x1 - x0, y1 - y0).into()))
    }

    /// Paint the frame `run()` produced into the currently-bound framebuffer,
    /// on top of the composited windows. MUST be called with the EGL context
    /// current. A no-op if there is nothing pending.
    pub fn paint(&mut self, size_px: (i32, i32)) {
        if let Some((clipped, textures_delta, ppp)) = self.pending.take() {
            self.painter.paint_and_update_textures(
                [size_px.0.max(1) as u32, size_px.1.max(1) as u32],
                ppp,
                &clipped,
                &textures_delta,
            );
        }
    }
}

/// egui bakes its font atlas with alpha = coverage^0.55, a boost that reads as
/// semibold in this GL pipeline next to the host's own text rendering. Rebake
/// font-texture updates with linear coverage so the menu text weight matches
/// the desktop's.
fn rebake_font_textures(delta: &mut egui::TexturesDelta) {
    for (_, d) in &mut delta.set {
        if let egui::ImageData::Font(f) = &d.image {
            let img = egui::ColorImage {
                size: f.size,
                pixels: f.srgba_pixels(Some(1.0)).collect(),
            };
            d.image = egui::ImageData::Color(Arc::new(img));
        }
    }
}

/// The "N volumes open (a, b)" summary of a session's volumes, used for
/// both the host window title and the desktop hint. "Veracage" when none.
pub fn volumes_title(leaders: &[LeaderApps]) -> String {
    let vols: Vec<&str> = leaders
        .iter()
        .flat_map(|l| l.volumes.iter().map(|s| s.as_str()))
        .collect();
    match vols.len() {
        0 => "Veracage".to_string(),
        1 => format!("1 volume open ({})", vols[0]),
        n => format!("{n} volumes open ({})", vols.join(", ")),
    }
}

/// A menu item widget: a small host-theme icon (when cached) + text. Icon-less
/// items fall back to text only.
fn menu_item(icon: Option<&egui::TextureHandle>, text: &str) -> egui::Button<'static> {
    const ICON_PT: f32 = 16.0;
    // Never wrap: a menu entry is one line, and the menu widens to fit it. An
    // item carrying its shortcut ("Copy out    Ctrl+Alt+C") is wide enough to
    // wrap onto two lines otherwise.
    let label = egui::RichText::new(text.to_owned());
    match icon {
        Some(tex) => {
            let img = egui::Image::from_texture(egui::load::SizedTexture::new(
                tex.id(),
                egui::vec2(ICON_PT, ICON_PT),
            ));
            egui::Button::image_and_text(img, label).wrap_mode(egui::TextWrapMode::Extend)
        }
        None => egui::Button::new(label).wrap_mode(egui::TextWrapMode::Extend),
    }
}

/// A menu entry widget for an app: small icon (when one is cached) + name.
/// Returned as a `Button` so the caller can add it enabled (a mounted session's
/// launcher) or disabled (a configured app with nothing mounted yet). The
/// texture id is Copy, so the button borrows nothing.
fn app_button(icon: Option<&egui::TextureHandle>, name: &str) -> egui::Button<'static> {
    const ICON_PT: f32 = 20.0;
    let label = egui::RichText::new(name.to_owned());
    match icon {
        Some(tex) => {
            let img = egui::Image::from_texture(egui::load::SizedTexture::new(
                tex.id(),
                egui::vec2(ICON_PT, ICON_PT),
            ));
            egui::Button::image_and_text(img, label).wrap_mode(egui::TextWrapMode::Extend)
        }
        None => egui::Button::new(label).wrap_mode(egui::TextWrapMode::Extend),
    }
}

// --------------------------------------------------------- discovery -------

/// The shared compositor runtime dir (must match COMPOSITOR_RUNTIME in wayland.py).
const RUNTIME_DIR: &str = "/run/veracage/rt";

/// The human-published dir (created by the helper, owned by the human uid):
/// `config.apps` (the configured app list) + `icons/<key>.rgba` (menu icons).
/// Writable only by the human uid, the same trust level as config.toml itself.
const PUB_DIR: &str = "/run/veracage/pub";

/// The append-only log the human-side broker drains for commands, one
/// `<nonce>\t<verb>` line per verb. Must match CMD_LOG in agent-rs/src/broker.rs.
///
/// A log, not a file we replace: the old `cmd.req` was one slot the compositor
/// atomically renamed over, and the broker noticed it by mtime. Two batches
/// written inside one broker poll (300ms), or any batch written while the broker
/// sat in a modal dialog, collapsed into the last one and the earlier verbs were
/// lost with no error anywhere - a menu item that did nothing, or a close-volume
/// that silently never happened. Appending keeps every verb, and the nonce lets
/// the broker resume exactly where it stopped.
///
/// The broker cannot unlink here (`rt` is 0711 veracage, so the human uid can
/// traverse but not write), which is why the compositor truncates the log itself
/// at startup rather than the reader deleting what it consumed.
const CMD_LOG: &str = "cmd.log";

/// The leaders' transient user-notice file (must match `_post_notice` in
/// leader.py): one `<nonce>\t<text>` line, veracage-written.
const NOTICE_FILE: &str = "notice";

/// The broker's request for the apps to be cleared out of the way of a dismount
/// (`PUB_DIR/closeapps`, written by the human side, whose dir this is). The
/// compositor owns the app windows, so only it can ask them politely; the broker
/// owns the pkexec that does the dismount. This is the one thing the human side
/// needs to ask the compositor for, so it is a file with a timestamp rather than
/// a channel: the scan shows each distinct request once.
const CLOSEAPPS_REQ: &str = "closeapps";

/// The mtime of the broker's clear-the-apps request, 0 when there is none. Same
/// nonce discipline as the notice file: the caller acts when it changes.
pub fn scan_closeapps_request() -> u128 {
    file_mtime(&std::path::Path::new(PUB_DIR).join(CLOSEAPPS_REQ))
}

/// Where the session locks live (must match LOCKS_DIR in cleanup.py). 0755, so
/// the compositor can list it even though the locks themselves are root-only.
const LOCKS_DIR: &str = "/run/veracage";

/// True while a `session-<sid>.lock` is still on disk.
///
/// The cleanup that runs when a session's transient unit stops unlinks that lock
/// only AFTER every volume's dm device has actually been closed (see
/// `cleanup.py`), so its disappearance is the honest end of a dismount - later
/// than the leader exiting, which only takes the mounts down. An unreadable
/// directory reads as "nothing left": the wait must not become a hang.
pub fn session_lock_present() -> bool {
    let Ok(entries) = std::fs::read_dir(LOCKS_DIR) else {
        return false;
    };
    entries.flatten().any(|e| {
        let name = e.file_name();
        let name = name.to_string_lossy();
        name.starts_with("session-") && name.ends_with(".lock")
    })
}

/// A progress note the compositor is tracking: which note (its file's timestamp)
/// and whether it has finished.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LaunchNote {
    stamp: u128,
    done: bool,
}

/// What the compositor has SEEN happen, as wall-clock nanosecond counts directly
/// comparable with a note's own file timestamp. This is how a progress note ends
/// early: the operation it names has visibly landed (see `pick_status`).
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Seen {
    /// When a client last announced a new window (ends a launch note).
    pub window_ns: u128,
    /// When a volume was last mounted into the workspace (ends an unlock note).
    pub volume_ns: u128,
}

/// How long a note may keep the spinner turning. A launch note is bounded tightly:
/// an app that never maps a window (it crashed, or it has no window) must not leave
/// the spinner going, and the failure banner explains it anyway. A broker note is
/// deleted when the open resolves, so its bound is only a backstop for a broker
/// that died mid-open.
const STATUS_TTL_LAUNCH: std::time::Duration = std::time::Duration::from_secs(10);
const STATUS_TTL_BROKER: std::time::Duration = std::time::Duration::from_secs(90);

/// Read both status files and decide what the spinner shows.
///
/// Also deletes a leader note left behind by an earlier session: `rt/status` lives
/// in a runtime directory shared by every session, and the compositor owns it.
pub fn scan_status(
    seen: Seen,
    note: Option<LaunchNote>,
) -> (Option<String>, Option<LaunchNote>) {
    let leader_path = std::path::Path::new(RUNTIME_DIR).join("status");
    let from_leader = read_status_line(&leader_path);
    if from_leader.as_ref().is_some_and(|(stamp, _)| *stamp < session_start_ns()) {
        let _ = std::fs::remove_file(&leader_path);
        return pick_status(None, None, seen, note, now_ns(), session_start_ns());
    }
    pick_status(
        read_status_line(&std::path::Path::new(PUB_DIR).join("status")),
        from_leader,
        seen,
        note,
        now_ns(),
        session_start_ns(),
    )
}

pub fn now_ns() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// When this compositor started, as a wall-clock nanosecond count. Notes older
/// than this belong to a previous session (see `pick_status`).
fn session_start_ns() -> u128 {
    use std::sync::OnceLock;
    static START: OnceLock<u128> = OnceLock::new();
    *START.get_or_init(now_ns)
}

/// Which note to show, and the note state to remember. Pure, so it is unit-tested.
///
/// THE SPEC. Two publishers ask for a spinner, each with one file whose modification
/// time is the note's identity and its age:
///   * `pub/status` - the human-side broker, while a volume is being unlocked.
///   * `rt/status` - a session leader, while a just-launched app has no window yet.
///
/// A note stops being shown as soon as ANY of these holds, which is what keeps the
/// spinner honest:
///   1. its file is gone, because the publisher resolved the operation;
///   2. it predates this compositor, so it is a leftover from an earlier session
///      (the runtime directory outlives one session);
///   3. what it was waiting for has visibly landed AFTER it was written: a window
///      appeared for a launch note, a volume was mounted for an unlock note. Both
///      are timestamp comparisons, not counts: the thing can happen inside the gap
///      between two scans, and a count taken when the note is first READ would then
///      already include it and never rise. A launch note additionally STAYS
///      finished, so a later window cannot revive it;
///   4. it is older than its TTL (see the constants above).
///
/// Rule 3 is what the broker's note needs and cannot get from its own file: the
/// broker deletes that file when `veracage open` exits, but an open that BOOTSTRAPS
/// the session does not exit until the session ends (the CLI runs the transient
/// unit with --pipe), so the file would sit there and spin the whole TTL out over a
/// volume that is long since mounted.
///
/// The broker's note wins while both exist: unlocking is the operation the user is
/// waiting on, and an app launch that follows publishes a fresher note anyway.
fn pick_status(
    from_broker: Option<(u128, String)>,
    from_leader: Option<(u128, String)>,
    seen: Seen,
    note: Option<LaunchNote>,
    now_ns: u128,
    session_start_ns: u128,
) -> (Option<String>, Option<LaunchNote>) {
    let fresh = |src: Option<(u128, String)>, ttl: std::time::Duration| {
        src.filter(|(stamp, _)| {
            *stamp >= session_start_ns && now_ns.saturating_sub(*stamp) < ttl.as_nanos()
        })
    };
    if let Some((stamp, text)) = fresh(from_broker, STATUS_TTL_BROKER) {
        if seen.volume_ns <= stamp {
            return (Some(text), note);
        }
    }
    let Some((stamp, text)) = fresh(from_leader, STATUS_TTL_LAUNCH) else {
        return (None, None);
    };
    let mut note = match note {
        Some(n) if n.stamp == stamp => n,
        _ => LaunchNote { stamp, done: false },
    };
    if seen.window_ns > stamp {
        note.done = true;
    }
    ((!note.done).then_some(text), Some(note))
}

/// A status file's one line, as `(modification time in ns, text)`. The leader
/// writes `<nonce>\t<text>` and the broker writes bare text: the nonce is dropped,
/// since the file's own timestamp dates every note uniformly.
fn read_status_line(path: &std::path::Path) -> Option<(u128, String)> {
    let md = std::fs::metadata(path).ok()?;
    if !md.is_file() || md.len() > 4096 {
        return None;
    }
    let stamp = md
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    let body = std::fs::read_to_string(path).ok()?;
    let line = body.lines().next()?;
    let raw = line.split_once('\t').map_or(line, |(_, rest)| rest);
    let text: String = raw.chars().filter(|c| !c.is_control()).take(80).collect();
    (!text.is_empty()).then_some((stamp, text))
}

/// Read the transient notice a leader published (e.g. a failed launch), as
/// `(nonce, text)`. The nonce (a wall-clock ns stamp) lets the caller show each
/// distinct notice once. Bounded read, control chars stripped, malformed ignored.
pub fn scan_notice() -> Option<(u64, String)> {
    let path = std::path::Path::new(RUNTIME_DIR).join(NOTICE_FILE);
    let md = std::fs::metadata(&path).ok()?;
    if !md.is_file() || md.len() > 4096 {
        return None;
    }
    let body = std::fs::read_to_string(&path).ok()?;
    let (nonce, text) = body.lines().next()?.split_once('\t')?;
    let nonce: u64 = nonce.parse().ok()?;
    let text: String = text.chars().filter(|c| !c.is_control()).take(200).collect();
    (!text.is_empty()).then_some((nonce, text))
}

/// Emit a command to the human-uid broker. The compositor runs as the `veracage`
/// uid and cannot spawn a human GUI / `pkexec` / open host files, so it appends a
/// verb to a log in its own runtime dir; the broker (human uid) drains everything
/// newer than the last verb it handled. Verbs: open/configure/settings/appearance/
/// shortcuts/exchange/help/about/close-volume:<label>.
///
/// Security: `/run/veracage/rt` is `0711 veracage`, so a same-uid attacker CANNOT
/// create/forge this file, only the compositor writes it, and a compositor menu
/// click is a genuine user action he can't inject. The verb isn't secret, so the
/// file is world-readable (the broker reads it by exact path through the 0711 dir);
/// at most an attacker learns a command was issued.
pub fn request_command(verb: &str) {
    request_commands(std::slice::from_ref(&verb));
}

/// Ask the broker for SEVERAL verbs at once. Each is appended as its own
/// `<nonce>\t<verb>` line, so a batch (Close volume > All) arrives whole and
/// nothing a later batch writes can overwrite it.
pub fn request_commands(verbs: &[&str]) {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let path = std::path::Path::new(RUNTIME_DIR).join(CMD_LOG);
    let mut body = String::new();
    for verb in verbs {
        // Nanoseconds since the epoch: monotonic enough to order the log and to
        // survive a compositor restart, which a per-run counter would not.
        body.push_str(&format!("{}\t{}\n", now_ns(), verb));
    }
    let append = || -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o644)
            .open(&path)?;
        // create() honours the umask (077 here), so force 0644 or the broker,
        // which runs as the human uid, cannot read what we just wrote.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))?;
        // One write per batch: the kernel appends it atomically, so the broker
        // never sees a half-written line.
        f.write_all(body.as_bytes())
    };
    if let Err(e) = append() {
        tracing::warn!("toolbar: command {verbs:?}: {e}");
    }
}

/// Start the command log empty. Called once at startup: the log is append-only,
/// so without this it would carry every verb of every previous session, and the
/// broker would have to skip a backlog that grows forever.
pub fn reset_command_log() {
    let path = std::path::Path::new(RUNTIME_DIR).join(CMD_LOG);
    if let Err(e) = std::fs::remove_file(&path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!("toolbar: could not clear {}: {e}", path.display());
        }
    }
}

/// mtime of `p` in ns since epoch, 0 if it can't be read (absent file).
fn file_mtime(p: &std::path::Path) -> u128 {
    std::fs::symlink_metadata(p)
        .ok()
        .filter(|md| md.file_type().is_file())
        .and_then(|md| md.modified().ok())
        .and_then(|mt| mt.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Load a published menu icon: `<w:u32 LE><h:u32 LE><rgba bytes>`, both sides
/// 1..=128. The blob comes from a human-owned dir, so validate strictly and
/// reject anything malformed rather than trusting it.
fn load_icon_rgba(p: &std::path::Path) -> Option<egui::ColorImage> {
    const MAX_SIDE: usize = 128;
    let md = std::fs::symlink_metadata(p).ok()?;
    if !md.file_type().is_file() || md.len() > (8 + MAX_SIDE * MAX_SIDE * 4) as u64 {
        return None;
    }
    let body = std::fs::read(p).ok()?;
    if body.len() < 8 {
        return None;
    }
    let w = u32::from_le_bytes(body[0..4].try_into().ok()?) as usize;
    let h = u32::from_le_bytes(body[4..8].try_into().ok()?) as usize;
    if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE || body.len() != 8 + w * h * 4 {
        return None;
    }
    Some(egui::ColorImage::from_rgba_unmultiplied([w, h], &body[8..]))
}

/// Read the human-published UI font from `PUB_DIR/font` (`<path>\n<size>`).
/// Returns (file path or empty, base point size). None if the file is absent.
pub fn scan_font() -> Option<(String, f32)> {
    let path = std::path::Path::new(PUB_DIR).join("font");
    let md = std::fs::symlink_metadata(&path).ok()?;
    if !md.file_type().is_file() || md.len() > 4096 {
        return None;
    }
    let body = std::fs::read_to_string(&path).ok()?;
    let mut lines = body.lines();
    let file = lines.next().unwrap_or("").trim().to_string();
    let size = lines
        .next()
        .and_then(|s| s.trim().parse::<f32>().ok())
        .unwrap_or(11.0)
        .clamp(6.0, 48.0);
    Some((file, size))
}

/// Read the idle-dismount timeout from `PUB_DIR/autodismount` (minutes,
/// 0 = off). None if absent or unreadable, in which case the current setting
/// stays. Bounded to a day, like the human side's own validation.
pub fn scan_autodismount() -> Option<u32> {
    let path = std::path::Path::new(PUB_DIR).join("autodismount");
    let md = std::fs::symlink_metadata(&path).ok()?;
    if !md.file_type().is_file() || md.len() > 16 {
        return None;
    }
    let minutes: u32 = std::fs::read_to_string(&path).ok()?.trim().parse().ok()?;
    Some(minutes.min(1440))
}

/// The theme to start with: the published one when the human side has already
/// written it, else the value the helper forwarded at spawn. Without the env
/// fallback a dark session would flash light for one scan interval.
pub fn initial_dark() -> bool {
    scan_theme().unwrap_or_else(|| std::env::var("VERACAGE_THEME").as_deref() == Ok("dark"))
}

/// Read the human-published theme from `PUB_DIR/theme` ("light" | "dark").
/// None if the file is absent or holds anything else, in which case the current
/// theme stays.
pub fn scan_theme() -> Option<bool> {
    let path = std::path::Path::new(PUB_DIR).join("theme");
    let md = std::fs::symlink_metadata(&path).ok()?;
    if !md.file_type().is_file() || md.len() > 32 {
        return None;
    }
    match std::fs::read_to_string(&path).ok()?.trim() {
        "dark" => Some(true),
        "light" => Some(false),
        _ => None,
    }
}

/// Read the host-clipboard auto-clear policy from `PUB_DIR/clipclear`
/// (`<0|1 enabled>\n<timeout secs>`). None if absent/unreadable, in which case
/// the worker keeps its secure default (enabled, 30s). Timeout clamped 1..=3600.
pub fn scan_clipclear() -> Option<(bool, u32)> {
    let path = std::path::Path::new(PUB_DIR).join("clipclear");
    let md = std::fs::symlink_metadata(&path).ok()?;
    if !md.file_type().is_file() || md.len() > 64 {
        return None;
    }
    let body = std::fs::read_to_string(&path).ok()?;
    let mut lines = body.lines();
    let enabled = lines.next()?.trim() == "1";
    let secs = lines.next()?.trim().parse::<u32>().ok()?.clamp(1, 3600);
    Some((enabled, secs))
}

/// The keyboard configuration the human published: the host desktop's own XKB
/// settings with the configured modifier mapping applied. Empty fields mean
/// "libxkbcommon's default".
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct KeyboardConfig {
    pub model: String,
    pub layout: String,
    pub variant: String,
    pub options: String,
}

/// XKB names are lowercase identifiers joined by a few separators. Anything
/// else means a corrupt (or hostile) pub dir, and a keymap that fails to
/// compile would leave the session with no keyboard at all, so the whole file
/// is rejected and the default layout kept.
fn xkb_names_ok(s: &str) -> bool {
    s.len() <= 256
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-:,+()".contains(c))
}

/// Read the published keyboard configuration from `PUB_DIR/keyboard`
/// (`<model>\n<layout>\n<variant>\n<options>`). None if absent, unreadable or
/// not plausible XKB names.
pub fn scan_keyboard() -> Option<KeyboardConfig> {
    let path = std::path::Path::new(PUB_DIR).join("keyboard");
    let md = std::fs::symlink_metadata(&path).ok()?;
    if !md.file_type().is_file() || md.len() > 1024 {
        return None;
    }
    let body = std::fs::read_to_string(&path).ok()?;
    let mut lines = body.lines().map(str::trim);
    let mut next = || lines.next().unwrap_or("").to_string();
    let cfg = KeyboardConfig {
        model: next(),
        layout: next(),
        variant: next(),
        options: next(),
    };
    let ok = [&cfg.model, &cfg.layout, &cfg.variant, &cfg.options]
        .iter()
        .all(|s| xkb_names_ok(s));
    ok.then_some(cfg)
}

/// Scan the human-published configured-app list at `PUB_DIR/config.apps`
/// (`<key>\t<name>` per line). Shown in the Apps menu when no volume is mounted;
/// clicking runs the broker's open flow with that app. Size- and count-capped,
/// and keys are validated (they become icon file names).
pub fn scan_config_apps() -> Vec<ConfigApp> {
    let path = std::path::Path::new(PUB_DIR).join("config.apps");
    let Ok(md) = std::fs::symlink_metadata(&path) else {
        return Vec::new();
    };
    if !md.file_type().is_file() || md.len() > 64 * 1024 {
        return Vec::new();
    }
    let Ok(body) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in body.lines().take(64) {
        let Some((key, name)) = line.split_once('\t') else {
            continue;
        };
        if key.is_empty() || key.len() > 64 || key.contains('/') || key == ".." {
            continue;
        }
        let name = if name.is_empty() { key } else { name };
        if name.len() > 128 {
            continue;
        }
        out.push(ConfigApp { key: key.to_string(), name: name.to_string() });
    }
    out
}

/// Scan `RUNTIME_DIR` for the `*.apps` files each session leader writes, building
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
        // Only a real regular file, size-capped, never block on a FIFO or OOM on
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
        let label = lines.next().unwrap_or("Volume").to_string();
        // Open-volume labels (tab-separated) for the Dismount menu.
        let volumes: Vec<String> = lines
            .next()
            .unwrap_or("")
            .split('\t')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        // Opener index: the session's opening app. `-1` (or an
        // out-of-range value, checked once names are known) means "no opener".
        let opener_raw: i64 = lines.next().and_then(|s| s.trim().parse().ok()).unwrap_or(-1);
        let names: Vec<String> = lines.filter(|l| !l.is_empty()).map(str::to_string).collect();
        let opener = usize::try_from(opener_raw).ok().filter(|&i| i < names.len());
        out.push(LeaderApps {
            sock: runtime.join(sockname),
            label,
            volumes,
            opener,
            names,
        });
    }
    out
}

/// True when `sock` is a socket file with NOTHING listening on it, i.e. the
/// leader that published it is gone. Used to drop a `.apps` file left behind by a
/// leader that was SIGKILLed (the suspend hook's force path does exactly that,
/// and `_unpublish_apps` only runs on a graceful exit): the toolbar would
/// otherwise keep listing a volume that no longer exists, and the quit gate would
/// wait for a session that cannot answer.
///
/// The connect is NON-BLOCKING, and only ECONNREFUSED/ENOENT count as dead. A
/// live leader whose accept loop is busy (it blocks for up to 8s inside
/// `close-apps`) fills its listen backlog and answers EAGAIN, which must read as
/// alive: pruning it would drop a real session from the menu.
pub fn leader_socket_dead(sock: &std::path::Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c_path) = std::ffi::CString::new(sock.as_os_str().as_bytes()) else {
        return false;
    };
    let fd = unsafe {
        libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC, 0)
    };
    if fd < 0 {
        return false;
    }
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = c_path.as_bytes_with_nul();
    if bytes.len() > addr.sun_path.len() {
        unsafe { libc::close(fd) };
        return false;
    }
    for (dst, src) in addr.sun_path.iter_mut().zip(bytes) {
        *dst = *src as libc::c_char;
    }
    let rc = unsafe {
        libc::connect(
            fd,
            &addr as *const libc::sockaddr_un as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        )
    };
    let err = std::io::Error::last_os_error().raw_os_error();
    unsafe { libc::close(fd) };
    rc != 0 && matches!(err, Some(libc::ECONNREFUSED) | Some(libc::ENOENT))
}

/// Ask a session's leader to launch enabled app `index` by poking its app socket
/// with a bare index line.
pub fn launch_app(sock: &std::path::Path, index: usize) {
    poke_leader(sock, &index.to_string(), "launch app");
}

/// Ask a session's leader to end the session: it stops its apps and exits, which
/// stops its transient unit and runs the ExecStopPost cleanup that dismounts the
/// volumes. The verb the leader's app socket understands (see
/// `_accept_app_launch` in leader.py); nothing else can reach that socket.
pub fn close_session(sock: &std::path::Path) {
    poke_leader(sock, CLOSE_VERB, "close session");
}

/// Ask a session's leader to stop its apps and keep running. What makes a
/// dismount possible: a running app holds the volume's dm device (see
/// `foreign_holders` in the helper), so the apps have to go before the volume
/// can actually be closed.
pub fn close_session_apps(sock: &std::path::Path) {
    poke_leader(sock, CLOSE_APPS_VERB, "close session apps");
}

/// The lines the leader's app socket reads as "end this session" and "stop the
/// apps but keep the session" (must match leader.py).
const CLOSE_VERB: &str = "close";
const CLOSE_APPS_VERB: &str = "close-apps";

/// Write one line to a leader's app socket. The blocking connect+write runs on a
/// short-lived thread so a stalled or missing leader socket can never freeze the
/// compositor's single-threaded event loop (matching the clipboard bridge's
/// discipline).
fn poke_leader(sock: &std::path::Path, line: &str, what: &'static str) {
    let sock = sock.to_path_buf();
    let line = line.to_string();
    // Builder::spawn (not thread::spawn) so an OS thread-creation failure returns
    // Err and is dropped, never panicking and unwinding the compositor.
    let _ = std::thread::Builder::new()
        .name("veracage-leader-poke".into())
        .spawn(move || {
            use std::io::Write;
            use std::os::unix::net::UnixStream;
            match UnixStream::connect(&sock) {
                Ok(mut s) => {
                    let _ = s.write_all(format!("{line}\n").as_bytes());
                }
                Err(e) => tracing::warn!("toolbar: {what}: {e}"),
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn published_xkb_names_are_checked_before_they_reach_libxkbcommon() {
        assert!(xkb_names_ok(""));
        assert!(xkb_names_ok("us,rp"));
        assert!(xkb_names_ok("compose:caps,eurosign:e,altwin:ctrl_win"));
        assert!(xkb_names_ok("nodeadkeys(legacy)"));
        // A corrupt or hostile pub dir must not reach the keymap compiler.
        assert!(!xkb_names_ok("us; rm -rf"));
        assert!(!xkb_names_ok("us\u{0}rp"));
        assert!(!xkb_names_ok(&"a".repeat(257)));
    }

    const HOUR_NS: u128 = 3_600_000_000_000;

    /// A session that started an hour into the epoch, so notes can be dated
    /// before it (an earlier session) or after it.
    const START: u128 = HOUR_NS;

    fn note_at(stamp: u128, text: &str) -> Option<(u128, String)> {
        Some((stamp, text.to_string()))
    }

    /// A compositor that has seen a window at `ns` and no volume mounted.
    fn window(ns: u128) -> Seen {
        Seen { window_ns: ns, volume_ns: 0 }
    }

    #[test]
    fn broker_note_wins_while_unlocking() {
        // The broker deletes its file when the open ends, so whatever is there is
        // what the user is waiting on - even with a window mapped since, or with a
        // leader note beside it.
        let broker = note_at(START, "Unlocking work.vc");
        let leader = note_at(START, "Starting Dolphin");
        for last_window in [0, START + 1] {
            let (shown, _) =
                pick_status(broker.clone(), leader.clone(), window(last_window), None, START, START);
            assert_eq!(shown, Some("Unlocking work.vc".to_string()));
        }
    }

    #[test]
    fn unlock_note_ends_when_the_volume_is_mounted() {
        // An open that BOOTSTRAPS the session keeps `veracage open` running for the
        // whole session, so the broker never deletes its file and the spinner used
        // to turn until the 90s TTL over a volume mounted seconds earlier. The
        // volume appearing is what ends it. A volume mounted BEFORE the note (the
        // second volume of a workspace) proves nothing about this open.
        let broker = note_at(START, "Unlocking work.vc");
        let seen = |volume_ns| Seen { window_ns: 0, volume_ns };
        let (shown, _) = pick_status(broker.clone(), None, seen(START - 5), None, START, START);
        assert_eq!(shown, Some("Unlocking work.vc".to_string()));
        let (shown, _) = pick_status(broker.clone(), None, seen(START + 1), None, START, START);
        assert_eq!(shown, None);
        // And a leader note published after the mount takes over from there.
        let leader = note_at(START + 2, "Starting Dolphin");
        let (shown, _) = pick_status(broker, leader, seen(START + 1), None, START + 2, START);
        assert_eq!(shown, Some("Starting Dolphin".to_string()));
    }

    #[test]
    fn launch_note_ends_when_a_window_appears_after_it_and_stays_ended() {
        let leader = note_at(START, "Starting Kate");
        // A window that was already there (mapped BEFORE the note) proves nothing.
        let (shown, note) = pick_status(None, leader.clone(), window(START - 5), None, START, START);
        assert_eq!(shown, Some("Starting Kate".to_string()));
        let (shown, note) = pick_status(None, leader.clone(), window(START - 5), note, START, START);
        assert_eq!(shown, Some("Starting Kate".to_string()));
        // Kate puts its window up: done.
        let (shown, note) = pick_status(None, leader.clone(), window(START + 1), note, START, START);
        assert_eq!(shown, None);
        // And it stays done, whatever happens to windows afterwards.
        let (shown, note) = pick_status(None, leader.clone(), window(START + 1), note, START, START);
        assert_eq!(shown, None);
        let (shown, _) = pick_status(None, leader, window(0), note, START, START);
        assert_eq!(shown, None);
    }

    #[test]
    fn a_window_that_maps_before_the_note_is_first_read_still_ends_it() {
        // The compositor reads the status file on a timer, so an app can put its
        // window up between the note being written and the note being seen. The
        // note must end at once: a window COUNT taken at first sight would already
        // include that window and could never rise, which left the spinner turning
        // for the whole timeout (the reported Konsole case).
        let leader = note_at(START, "Starting Konsole");
        let window_mapped = START + 700_000_000; // 0.7s after the note, before the scan
        let (shown, note) = pick_status(None, leader, window(window_mapped), None, START + 1_000_000_000, START);
        assert_eq!(shown, None, "the note should be finished the first time it is seen");
        assert!(note.is_some_and(|n| n.done));
    }

    #[test]
    fn a_launch_that_never_maps_a_window_stops_at_the_ttl() {
        // A crashed or window-less app must not leave the spinner turning.
        let leader = note_at(START, "Starting Dolphin");
        let inside = START + STATUS_TTL_LAUNCH.as_nanos() - 1;
        let outside = START + STATUS_TTL_LAUNCH.as_nanos();
        let (shown, note) = pick_status(None, leader.clone(), window(0), None, inside, START);
        assert!(shown.is_some());
        let (shown, _) = pick_status(None, leader, window(0), note, outside, START);
        assert_eq!(shown, None);
    }

    #[test]
    fn a_broker_note_that_is_never_deleted_stops_at_its_own_ttl() {
        // The broker deletes its file; this only covers one that died mid-open.
        let broker = note_at(START, "Unlocking work.vc");
        let inside = START + STATUS_TTL_BROKER.as_nanos() - 1;
        let outside = START + STATUS_TTL_BROKER.as_nanos();
        assert!(pick_status(broker.clone(), None, window(0), None, inside, START).0.is_some());
        assert_eq!(pick_status(broker, None, window(0), None, outside, START).0, None);
    }

    #[test]
    fn a_note_from_an_earlier_session_is_never_shown() {
        // Both status files live in directories that outlive one session, so a note
        // written before this compositor started belongs to a session that is gone:
        // showing it spins on a fresh desktop with nothing happening.
        let before = START - 1;
        let leader = note_at(before, "Starting Konsole");
        let broker = note_at(before, "Unlocking work.vc");
        assert_eq!(pick_status(None, leader, window(0), None, START, START).0, None);
        assert_eq!(pick_status(broker, None, window(0), None, START, START).0, None);
        // A note written after startup is fine.
        let live = note_at(START + 1, "Starting Konsole");
        assert!(pick_status(None, live, window(0), None, START + 2, START).0.is_some());
    }

    #[test]
    fn a_new_note_starts_fresh_after_the_previous_one_finished() {
        // Launching another app writes the file again, which must show again even
        // though the previous note was finished.
        let first = note_at(START, "Starting Kate");
        let (_, note) = pick_status(None, first.clone(), window(0), None, START, START);
        let (_, note) = pick_status(None, first, window(START + 1), note, START + 1, START); // finished
        let second = note_at(START + 5, "Starting Dolphin");
        let (shown, _) = pick_status(None, second, window(START + 1), note, START + 5, START);
        assert_eq!(shown, Some("Starting Dolphin".to_string()));
    }

    #[test]
    fn no_note_published_forgets_the_previous_one() {
        assert_eq!(pick_status(None, None, window(START), None, START, START), (None, None));
    }

    #[test]
    fn a_socket_with_no_listener_reads_as_dead_and_a_live_one_does_not() {
        use std::os::unix::net::UnixListener;
        let dir = std::env::temp_dir().join(format!("vc-sock-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);

        // Alive: a listener is bound.
        let live = dir.join("live.sock");
        let _ = std::fs::remove_file(&live);
        let listener = UnixListener::bind(&live).unwrap();
        assert!(!leader_socket_dead(&live));

        // Dead: the leader was SIGKILLed, so the socket FILE is still there with
        // nothing behind it. This is the case that leaves a stale `.apps`.
        drop(listener);
        assert!(leader_socket_dead(&live));

        // A path that is not there at all reads as dead too.
        assert!(leader_socket_dead(&dir.join("nope.sock")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn status_line_takes_the_text_from_both_writers_and_the_time_from_the_file() {
        let dir = std::env::temp_dir().join(format!("veracage-status-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("status");
        let now = || {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        };
        // Leader form: "<nonce>\t<text>" - the nonce is dropped, the mtime dates it.
        let before = now();
        std::fs::write(&p, "12345\tStarting Kate\n").unwrap();
        let (stamp, text) = read_status_line(&p).unwrap();
        assert_eq!(text, "Starting Kate");
        // The file's own clock, so allow a second of slack against ours.
        assert!(
            stamp.abs_diff(before) < 2_000_000_000,
            "stamp {stamp} is not around the write time {before}"
        );
        // Broker form: bare text, no nonce.
        std::fs::write(&p, "Unlocking work.vc\n").unwrap();
        assert_eq!(read_status_line(&p).unwrap().1, "Unlocking work.vc");
        // Control characters are stripped; an empty result is no status.
        std::fs::write(&p, "1\t\u{1b}[31mred\u{7}\n").unwrap();
        assert_eq!(read_status_line(&p).unwrap().1, "[31mred");
        std::fs::write(&p, "\n").unwrap();
        assert_eq!(read_status_line(&p), None);
        std::fs::write(&p, "").unwrap();
        assert_eq!(read_status_line(&p), None);
        assert_eq!(read_status_line(&dir.join("absent")), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_spinner_wave_is_brightest_at_the_head_and_fades_backwards() {
        // The head is full brightness, and strokes the head has already passed fade
        // with distance, all the way round: no stroke is ever invisible, and none
        // brighter than the head.
        let head = 0.5;
        assert!((stroke_alpha(head, head) - 1.0).abs() < 1e-6);
        let mut previous = 1.0;
        for step in 1..=11 {
            let at = (head - step as f32 / 12.0).rem_euclid(1.0);
            let alpha = stroke_alpha(head, at);
            assert!(alpha < previous, "stroke {step} behind the head did not fade");
            assert!(alpha >= SPINNER_DIM, "stroke {step} fell below the floor");
            previous = alpha;
        }
    }

    #[test]
    fn the_spinner_wave_is_continuous_across_the_wrap() {
        // A stroke just behind the head is bright even when the head has wrapped
        // past zero, otherwise the ring flickers once per turn.
        let just_behind = stroke_alpha(0.0, 0.99);
        assert!(just_behind > 0.9, "wrap makes the wave jump: {just_behind}");
    }

    #[test]
    fn the_spinner_sits_on_the_desktop_icon() {
        let (ix, iy) = hint_icon_pos(1024, 680);
        let c = spinner_center(1024, 680);
        assert_eq!(c.x, ix as f32 + HINT_ICON_PX as f32 / 2.0);
        assert_eq!(c.y, iy as f32 + HINT_ICON_PX as f32 / 2.0);
    }

    #[test]
    fn hint_icon_pos_is_centered() {
        let (x, y) = hint_icon_pos(800, 600);
        assert_eq!(x, (800 - HINT_ICON_PX) / 2); // horizontally centered
        assert_eq!(y, 600 * 3 / 10); // ~30% down
    }

    #[test]
    fn volumes_title_empty_is_veracage() {
        assert_eq!(volumes_title(&[]), "Veracage");
    }
}
