use std::time::Duration;

use smithay::{
    backend::{
        renderer::{
            damage::OutputDamageTracker, element::surface::WaylandSurfaceRenderElement, gles::GlesRenderer,
        },
        winit::{self, WinitEvent},
    },
    output::{Mode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::{
        calloop::EventLoop,
        winit::{platform::wayland::WindowAttributesWayland, window::WindowAttributes},
    },
    utils::{IsAlive, Rectangle, Transform},
};

use smithay::input::pointer::{CursorImageAttributes, CursorImageStatus};
use smithay::reexports::wayland_server::Resource;

use crate::State;

use smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement;
use smithay::backend::renderer::{ImportAll, ImportMem};

// The renderer's own elements: wayland surfaces (the DnD ghost and a client's
// cursor, needs ImportAll), memory buffers (the backdrop's Veracage icon, needs
// ImportMem - drawn by the renderer rather than egui, whose sRGB texture path
// fringes a transparent-edged icon), and the draw-nothing damage marker for the
// egui overlay.
smithay::backend::renderer::element::render_elements! {
    HintElement<R> where R: ImportMem + ImportAll;
    Surface = WaylandSurfaceRenderElement<R>,
    Memory = MemoryRenderBufferRenderElement<R>,
    Egui = EguiDamage,
}

// The full element list for a frame. smithay's own `space::render_output` cannot
// express the order we need - it always puts custom elements ON TOP - so the
// list is assembled here: overlay above the windows, each window's shadow
// directly beneath THAT window, and the backdrop at the bottom.
smithay::backend::renderer::element::render_elements! {
    OutputElement<='a, GlesRenderer>;
    Surface = &'a WaylandSurfaceRenderElement<GlesRenderer>,
    Custom = &'a HintElement<GlesRenderer>,
    Shadow = &'a smithay::backend::renderer::gles::element::PixelShaderElement,
}

/// A draw-nothing element that reports the egui overlay's region as damaged.
/// The toolbar is painted straight into the framebuffer AFTER render_output,
/// outside the damage tracker, so with a real buffer age the tracker must be
/// told to re-render what lies beneath the overlay: where it paints this frame
/// (egui blends, so stale pixels would shine through) and where it painted
/// before (a closed menu must not linger).
///
/// The id is STABLE across frames and the commit counter is bumped only when
/// the toolbar's output can actually have changed (input on the strip, an egui
/// animation, or the discovery scan). The tracker then damages the old and
/// new geometry on a change (so a closed menu is repainted) and nothing at all
/// on an unchanged frame, instead of re-rendering the whole strip on every
/// frame an app's commit triggers.
struct EguiDamage {
    id: smithay::backend::renderer::element::Id,
    commit: smithay::backend::renderer::utils::CommitCounter,
    geometry: Rectangle<i32, smithay::utils::Physical>,
}

impl EguiDamage {
    fn new(
        id: smithay::backend::renderer::element::Id,
        commit: smithay::backend::renderer::utils::CommitCounter,
        geometry: Rectangle<i32, smithay::utils::Physical>,
    ) -> Self {
        Self { id, commit, geometry }
    }
}

impl smithay::backend::renderer::element::Element for EguiDamage {
    fn id(&self) -> &smithay::backend::renderer::element::Id {
        &self.id
    }
    fn current_commit(&self) -> smithay::backend::renderer::utils::CommitCounter {
        self.commit
    }
    fn src(&self) -> Rectangle<f64, smithay::utils::Buffer> {
        Rectangle::from_size(
            (self.geometry.size.w as f64, self.geometry.size.h as f64).into(),
        )
    }
    fn geometry(
        &self,
        _scale: smithay::utils::Scale<f64>,
    ) -> Rectangle<i32, smithay::utils::Physical> {
        self.geometry
    }
}

impl<R: smithay::backend::renderer::Renderer> smithay::backend::renderer::element::RenderElement<R>
    for EguiDamage
{
    fn draw(
        &self,
        _frame: &mut R::Frame<'_, '_>,
        _src: Rectangle<f64, smithay::utils::Buffer>,
        _dst: Rectangle<i32, smithay::utils::Physical>,
        _damage: &[Rectangle<i32, smithay::utils::Physical>],
        _opaque_regions: &[Rectangle<i32, smithay::utils::Physical>],
        _cache: Option<&smithay::utils::user_data::UserDataMap>,
    ) -> Result<(), R::Error> {
        Ok(())
    }
}

/// Parse a "<w>x<h>" window-size string into bounded logical dimensions. Bounds
/// match agent-rs config::window_size_valid; None on anything malformed.
fn parse_size(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.split_once('x')?;
    let w: u32 = w.parse().ok()?;
    let h: u32 = h.parse().ok()?;
    ((320..=16384).contains(&w) && (240..=16384).contains(&h)).then_some((w, h))
}

/// Apply a window-size value ("max" | "<w>x<h>" | "default") to the live window.
fn apply_window_size(window: &dyn smithay::reexports::winit::window::Window, size: &str) {
    use smithay::reexports::winit::dpi::LogicalSize;
    match size {
        "max" => window.set_maximized(true),
        "default" => window.set_maximized(false),
        s => {
            if let Some((w, h)) = parse_size(s) {
                window.set_maximized(false);
                let _ = window.request_surface_size(LogicalSize::new(w, h).into());
            }
        }
    }
}

/// True when the human side asked for the dark theme (same env the toolbar
/// reads). Read once; it decides the backdrop text's ink.
fn dark_theme() -> bool {
    use std::sync::OnceLock;
    static DARK: OnceLock<bool> = OnceLock::new();
    *DARK.get_or_init(|| std::env::var("VERACAGE_THEME").as_deref() == Ok("dark"))
}

/// How often the discovery scan runs: it is what makes a mounted volume show up
/// in the title and the Apps menu, and what picks up a progress note, so it also
/// bounds how stale those look. Cheap (a few stats on tmpfs) and it only wakes
/// the renderer when something actually changed.
const SCAN_INTERVAL: Duration = Duration::from_millis(250);

/// Poll the published state: the leaders' `.apps` files, the human-side config
/// apps, font, window size, shortcuts, clipboard policy, notices and progress
/// notes. Returns true when anything the toolbar draws changed, so the caller
/// can request a frame instead of rendering on a timer.
fn run_discovery_scan(
    state: &mut State,
    window: &dyn smithay::reexports::winit::window::Window,
) -> bool {
    let fresh = crate::toolbar::scan_leaders();
    let leaders_changed = fresh.len() != state.leaders.len()
        || fresh.iter().zip(&state.leaders).any(|(a, b)| {
            a.names != b.names || a.label != b.label || a.volumes != b.volumes
        });
    if leaders_changed {
        tracing::debug!(
            "toolbar: launchers = {:?}",
            fresh.iter().map(|l| l.names.clone()).collect::<Vec<_>>()
        );
        // Title = the mounted volumes; the app_id keeps the icon.
        window.set_title(&crate::toolbar::volumes_title(&fresh));
    }
    state.leaders = fresh;
    let mut changed = leaders_changed;

    let cfg_apps = crate::toolbar::scan_config_apps();
    if cfg_apps != state.cfg_apps {
        state.cfg_apps = cfg_apps;
        changed = true;
    }
    let font = crate::toolbar::scan_font();
    if let Some(tb) = state.toolbar.as_mut() {
        tb.refresh_icons(&state.cfg_apps);
        if let Some((path, base)) = font.clone() {
            changed |= tb.refresh_font(&path, base);
        }
    }
    if font.is_some() && font != state.font {
        state.font = font;   // the backdrop text re-rasterises with it
        changed = true;
    }
    // Live window resize: pick up a Settings change to the default window size
    // (published to /run/veracage/pub/window.size).
    if let Some(sz) = crate::toolbar::scan_window_size() {
        if sz != state.window_size_applied {
            apply_window_size(window, &sz);
            state.window_size_applied = sz;
        }
    }
    // Live keyboard shortcuts (Copy out / Paste in).
    if let Some(binds) = crate::shortcuts::scan() {
        if binds != state.binds {
            let label = |b: &Option<crate::shortcuts::Keybind>| {
                b.as_ref().map(|k| k.label()).unwrap_or_else(|| "unset".into())
            };
            if let Some(tb) = state.toolbar.as_mut() {
                tb.refresh_shortcuts(label(&binds.copy_out), label(&binds.paste_in));
            }
            state.binds = binds;
            changed = true;
        }
    }
    // Live host-clipboard auto-clear policy; push to the worker only on a
    // change so we don't poke it every scan.
    if let Some(policy) = crate::toolbar::scan_clipclear() {
        if policy != state.clip_clear_applied {
            if let Some(hc) = &state.host_clipboard {
                hc.set_policy(policy.0, policy.1);
            }
            state.clip_clear_applied = policy;
        }
    }
    // A transient notice a leader published (a failed launch, etc.): shown once.
    if let Some((nonce, msg)) = crate::toolbar::scan_notice() {
        if nonce != state.notice_nonce {
            state.notice_nonce = nonce;
            if let Some(tb) = state.toolbar.as_mut() {
                tb.set_notice(msg);
            }
            changed = true;
        }
    }
    // Progress note (unlocking a volume, starting an app), shown with a spinner.
    // A launch note clears when a window appears after it, so the moment the last
    // one did is carried along.
    let (status, note) = crate::toolbar::scan_status(state.last_window_ns, state.status_note);
    state.status_note = note;
    if let Some(tb) = state.toolbar.as_mut() {
        changed |= tb.set_status(status);
    }
    changed
}

pub fn init_winit(
    event_loop: &mut EventLoop<State>,
    state: &mut State,
) -> Result<(), Box<dyn std::error::Error>> {
    // Host window: app_id "veracage" so KWin pairs it with veracage.desktop and
    // shows the Veracage icon; title starts "Veracage" and becomes the mounted
    // volume label once a volume is open (updated below from leader discovery).
    // Default size comes from config via VERACAGE_WINDOW_SIZE ("<w>x<h>" | "max" |
    // unset). "max" asks the host to maximize; a size sets the initial inner size.
    let mut attributes = WindowAttributes::default()
        .with_title("Veracage")
        .with_platform_attributes(Box::new(
            WindowAttributesWayland::default().with_name("veracage", "veracage"),
        ));
    match std::env::var("VERACAGE_WINDOW_SIZE").ok().as_deref() {
        Some("max") => attributes = attributes.with_maximized(true),
        Some(s) => {
            if let Some((w, h)) = parse_size(s) {
                attributes = attributes.with_surface_size(
                    smithay::reexports::winit::dpi::LogicalSize::new(w, h),
                );
            }
        }
        None => {}
    }
    let (backend, winit) = winit::init_from_attributes(attributes)?;

    // Host clipboard bridge on the compositor's own winit->host connection.
    {
        use raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
        if let Ok(RawDisplayHandle::Wayland(w)) =
            backend.window().display_handle().map(|h| h.as_raw())
        {
            // SAFETY: the display ptr belongs to the winit backend (compositor lifetime).
            // Keep the worker's JoinHandle so teardown can stop it before the
            // backend (and the wl_display it borrows) is dropped.
            if let Some((hc, worker)) =
                unsafe { crate::hostclip::HostClipboard::from_display_ptr(w.display.as_ptr()) }
            {
                state.host_clipboard = Some(hc);
                state.host_clipboard_worker = Some(worker);
            }
        }
    }

    // Host scale factor (winit reports the host window's DPI): 1.0 on a 1x host
    // (so a no-op there) and the real factor on HiDPI, so apps render crisply
    // instead of being upscaled by the host.
    let host_scale = backend.scale_factor();

    let mode = Mode {
        size: backend.window_size(),
        refresh: 60_000,
    };

    let output = Output::new(
        "winit".to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "Smithay".into(),
            model: "Winit".into(),
            serial_number: "Unknown".into(),
        },
    );
    let _global = output.create_global::<State>(&state.display_handle);
    output.change_current_state(
        Some(mode),
        Some(Transform::Flipped180),
        Some(Scale::Fractional(host_scale)),
        Some((0, 0).into()),
    );
    output.set_preferred(mode);

    state.space.map_output(&output, (0, 0));

    let mut damage_tracker = OutputDamageTracker::from_output(&output);
    // Frames left to render with age 0 (full redraw) after a resize, while the
    // swapchain reallocates and reported buffer ages are unreliable (as anvil).
    let mut full_redraw: u8 = 0;
    // Last cursor decision written to the debug log, so it is logged on change
    // rather than every frame.
    let mut cursor_logged: Option<String> = None;
    // Damage bookkeeping for the egui overlay (see EguiDamage): a stable id
    // plus a counter bumped only when the toolbar's output can have changed.
    let egui_id = smithay::backend::renderer::element::Id::new();
    let mut egui_commit = smithay::backend::renderer::utils::CommitCounter::default();
    // True while the pointer is on the toolbar strip or in an open menu, so a
    // pointer move that leaves it still gets one frame to drop the highlight.
    let mut strip_hot = false;

    // The backdrop IS the desktop: drawn first, behind every window. Themed:
    // light-gray under the light theme, near-black under dark.
    let clear_color: [f32; 4] = if std::env::var("VERACAGE_THEME").as_deref() == Ok("dark") {
        [0.10, 0.10, 0.10, 1.0]
    } else {
        [0.85, 0.85, 0.87, 1.0]
    };

    // Rendering stays in the winit source (on Redraw). Share the backend so a
    // paced timer can drive redraws, instead of the render re-requesting one
    // every frame (which spun the CPU at ~130% - the loop never idled). calloop
    // is single-threaded, so the timer's borrow and the render's borrow_mut can
    // never overlap.
    let backend = std::rc::Rc::new(std::cell::RefCell::new(backend));
    let backend_render = backend.clone();

    // Let the parts of the compositor that mark the output dirty (client
    // commits, input, window destruction) also ask winit for a frame, so the
    // pacing timer can idle until the next scan instead of polling at 60 Hz.
    // try_borrow: a failure means we are inside the render path, which is
    // already producing the frame.
    let backend_wake = backend.clone();
    let backend_scan = backend.clone();
    state.request_redraw = Some(std::rc::Rc::new(move || {
        if let Ok(b) = backend_wake.try_borrow() {
            b.window().request_redraw();
        }
    }));

    event_loop.handle().insert_source(winit, move |event, _, state| {
        match event {
            WinitEvent::Resized { size, scale_factor } => {
                output.change_current_state(
                    Some(Mode {
                        size,
                        refresh: 60_000,
                    }),
                    None,
                    Some(Scale::Fractional(scale_factor)),
                    None,
                );
                full_redraw = 4;
                state.dirty = true;
                state.toolbar_changed = true; // the strip is re-laid out at the new width
                state.wake();
            }
            WinitEvent::Input(event) => {
                state.process_input_event(event);
                // ALWAYS repaint on input. It is tempting to repaint only when the
                // toolbar or a drag is involved, since an app's response to a key
                // or click comes back as a commit - but plenty of compositor state
                // moves with the pointer and commits nothing: a window drag
                // (move_grab maps the element at the new position), an interactive
                // resize, a popup grab. Gating input cost us those: dragging a
                // window only repainted when the discovery scan happened to fire.
                state.dirty = true;
                // The toolbar's own pixels change only when the strip is involved,
                // so its damage region is re-reported only then (see EguiDamage).
                let hot = state.dnd_icon.is_some()
                    || state
                        .toolbar
                        .as_ref()
                        .is_some_and(|tb| tb.over_strip() || tb.wants_pointer());
                if hot || strip_hot {
                    state.toolbar_changed = true;
                }
                strip_hot = hot;
                state.wake();
            }
            WinitEvent::Redraw => {
                let mut backend = backend_render.borrow_mut();
                // Consume the dirty flag: this frame satisfies it. egui animations
                // re-arm it below (via wants_repaint); commits/input set it again
                // as they arrive.
                state.dirty = false;
                state.frames = state.frames.saturating_add(1);
                let size = backend.window_size();
                let scale_f = output.current_scale().fractional_scale();

                // Run the toolbar UI (CPU only) BEFORE compositing: the region
                // egui will paint goes to the damage tracker (as EguiDamage),
                // since egui is painted into the framebuffer after render_output.
                // The desktop indicator shows only with an empty space; a mapped
                // sandbox window means the app IS the indication (and a filled
                // CentralPanel would paint over it).
                let (action, egui_rect) = if let Some(tb) = state.toolbar.as_mut() {
                    tb.run(
                        (size.w, size.h),
                        scale_f,
                        &state.leaders,
                        &state.cfg_apps,
                    )
                } else {
                    (crate::toolbar::ToolbarAction::None, None)
                };

                // Act on a menu selection BEFORE rendering: it needs no GL, so a
                // frame that later fails to render must not swallow the click.
                match action {
                    crate::toolbar::ToolbarAction::ClipPush => {
                        crate::clipboard::push_from_host(state)
                    }
                    crate::toolbar::ToolbarAction::ClipPull => {
                        crate::clipboard::pull_to_host(state)
                    }
                    crate::toolbar::ToolbarAction::LaunchApp { sock, index } => {
                        crate::toolbar::launch_app(&sock, index)
                    }
                    crate::toolbar::ToolbarAction::Command(verb) => {
                        crate::toolbar::request_command(&verb)
                    }
                    crate::toolbar::ToolbarAction::CloseVolume(label) => {
                        crate::toolbar::request_command(&format!("close-volume:{label}"))
                    }
                    crate::toolbar::ToolbarAction::Quit => state.loop_signal.stop(),
                    crate::toolbar::ToolbarAction::None => {}
                }
                if state.toolbar_changed {
                    egui_commit.increment();
                    state.toolbar_changed = false;
                }

                // Re-render only what changed since this buffer was last drawn
                // (its EGL buffer age). Right after a resize the age is forced
                // to 0 (full frame) while the swapchain reallocates.
                let age = if full_redraw > 0 {
                    full_redraw -= 1;
                    0
                } else {
                    backend.buffer_age().unwrap_or(0)
                };

                // Cursor, following anvil (smithay's reference compositor): drop a
                // dead cursor surface back to the default, hand a NAMED shape to
                // the host window so it draws the themed cursor (this is what
                // makes a window's resize edges show a resize cursor), and hide
                // the host cursor only while we are compositing a client's own
                // cursor surface ourselves.
                if let CursorImageStatus::Surface(surface) = &state.cursor_status {
                    if !surface.alive() {
                        state.cursor_status = CursorImageStatus::default_named();
                    }
                }
                // Anvil's rule: the host cursor is hidden exactly while a client
                // provides its own cursor surface, which we composite below. The
                // condition must be the status alone, not whether that surface is
                // already mapped - a toolkit's cursor surface is unmapped at the
                // moment it hands it over, and deferring would show the host's
                // plain arrow in place of the shape the client asked for.
                let cursor_visible = !matches!(state.cursor_status, CursorImageStatus::Surface(_));
                if let CursorImageStatus::Named(icon) = state.cursor_status {
                    backend.window().set_cursor(icon.into());
                }
                backend.window().set_cursor_visible(cursor_visible);

                // A transient EGL/GL error (context loss, host-resize race, GL OOM)
                // must skip the frame, not abort the compositor and every app.
                let render_res = match backend.bind() {
                    Ok((renderer, mut framebuffer)) => {
                        // Drag-and-drop icon: composite the "ghost" at the cursor,
                        // on top of the app windows, so a drag has visual feedback
                        // (the drop itself works even without it). Cleared on drop.
                        let dnd: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                            match &state.dnd_icon {
                                Some(icon) if icon.surface.alive() => {
                                    let cursor = state
                                        .seat
                                        .get_pointer()
                                        .map(|p| p.current_location())
                                        .unwrap_or_default();
                                    let pos = (cursor + icon.offset.to_f64())
                                        .to_physical(scale_f)
                                        .to_i32_round();
                                    smithay::backend::renderer::element::AsRenderElements::<
                                        GlesRenderer,
                                    >::render_elements(
                                        &smithay::desktop::space::SurfaceTree::from_surface(
                                            &icon.surface,
                                        ),
                                        renderer,
                                        pos,
                                        smithay::utils::Scale::from(scale_f),
                                        1.0,
                                    )
                                }
                                _ => Vec::new(),
                            };
                        // Combine the DnD ghost with the "No volume mounted"
                        // hint icon, drawn by the renderer (not egui) so it has
                        // no egui_glow sRGB-texture fringe.
                        let mut custom: Vec<HintElement<GlesRenderer>> =
                            dnd.into_iter().map(HintElement::Surface).collect();
                        // A client-drawn cursor surface, composited at the pointer
                        // minus its hotspot (anvil's cursor path). The host cursor is
                        // hidden for exactly this case, so there is one cursor on
                        // screen: whichever of the two is ours to draw.
                        let mut cursor_drawn = 0usize;
                        if let CursorImageStatus::Surface(surface) = state.cursor_status.clone() {
                            let hotspot = smithay::wayland::compositor::with_states(
                                &surface,
                                |states| {
                                    states
                                        .data_map
                                        .get::<std::sync::Mutex<CursorImageAttributes>>()
                                        .map(|attrs| attrs.lock().unwrap().hotspot)
                                        .unwrap_or_default()
                                },
                            );
                            let cursor = state
                                .seat
                                .get_pointer()
                                .map(|p| p.current_location())
                                .unwrap_or_default();
                            let pos = (cursor - hotspot.to_f64())
                                .to_physical(scale_f)
                                .to_i32_round();
                            let cursor_elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                                smithay::backend::renderer::element::surface::render_elements_from_surface_tree(
                                    renderer,
                                    &surface,
                                    pos,
                                    smithay::utils::Scale::from(scale_f),
                                    1.0,
                                    smithay::backend::renderer::element::Kind::Cursor,
                                );
                            cursor_drawn = cursor_elements.len();
                            custom.extend(cursor_elements.into_iter().map(HintElement::Surface));
                        }
                        if crate::debug_enabled() {
                            let now = format!(
                                "{} ({cursor_drawn} element(s) composited)",
                                crate::describe_cursor(&state.cursor_status)
                            );
                            if cursor_logged.as_deref() != Some(now.as_str()) {
                                crate::vcdebug(&format!("cursor: {now}"));
                                cursor_logged = Some(now);
                            }
                        }
                        // The Veracage icon belongs to the BACKDROP: drawn below the
                        // app windows, so it stays visible behind them like a
                        // desktop wallpaper. The hint TEXT under it is egui, which
                        // paints above the windows, so that stays conditional.
                        let mut backdrop: Vec<HintElement<GlesRenderer>> = Vec::new();
                        if let Some(buf) = &state.hint_icon {
                            // Position in LOGICAL points, then scale to physical
                            // for the element (it renders at the buffer's logical
                            // size = 96pt).
                            let (lx, ly) = crate::toolbar::hint_icon_pos(
                                (size.w as f64 / scale_f) as i32,
                                (size.h as f64 / scale_f) as i32,
                            );
                            let loc = smithay::utils::Point::<f64, smithay::utils::Physical>::from(
                                (lx as f64 * scale_f, ly as f64 * scale_f),
                            );
                            if let Ok(el) = smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement::from_buffer(
                                renderer,
                                loc,
                                buf,
                                None,
                                None,
                                None,
                                smithay::backend::renderer::element::Kind::Unspecified,
                            ) {
                                backdrop.push(HintElement::Memory(el));
                            }
                        }
                        // ... and the two text lines under it, also rasterised
                        // into a buffer (hint_text.rs) so they belong to the
                        // backdrop rather than to the on-top egui layer.
                        let mounted: Vec<&str> = state
                            .leaders
                            .iter()
                            .flat_map(|l| l.volumes.iter().map(|s| s.as_str()))
                            .collect();
                        let (line1, line2) = if mounted.is_empty() {
                            ("No volume mounted".to_string(), "File > Mount volume")
                        } else {
                            (crate::toolbar::volumes_title(&state.leaders), "")
                        };
                        let (font_path, base) = state
                            .font
                            .clone()
                            .unwrap_or_else(|| (String::new(), 16.0));
                        let (w_l, h_l) = (
                            (size.w as f64 / scale_f) as i32,
                            (size.h as f64 / scale_f) as i32,
                        );
                        let (_, icon_y) = crate::toolbar::hint_icon_pos(w_l, h_l);
                        if let Some((buf, (tw, _th))) = state.hint_text.buffer(
                            &line1,
                            line2,
                            &font_path,
                            base,
                            dark_theme(),
                        ) {
                            let tx = (w_l - tw) / 2;
                            let ty = icon_y + crate::toolbar::HINT_ICON_PX + 16;
                            let loc = smithay::utils::Point::<f64, smithay::utils::Physical>::from(
                                (tx as f64 * scale_f, ty as f64 * scale_f),
                            );
                            if let Ok(el) = smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement::from_buffer(
                                renderer,
                                loc,
                                buf,
                                None,
                                None,
                                None,
                                smithay::backend::renderer::element::Kind::Unspecified,
                            ) {
                                backdrop.push(HintElement::Memory(el));
                            }
                        }
                        // The egui overlay's region, so the tracker re-renders
                        // beneath it (see EguiDamage).
                        if let Some(rect) = egui_rect {
                            custom.push(HintElement::Egui(EguiDamage::new(
                                egui_id.clone(),
                                egui_commit,
                                rect,
                            )));
                        }
                        // One layer per window, TOP-MOST FIRST, each carrying its
                        // own shadow so the shadow lands directly beneath THAT
                        // window - on the windows behind it - the way a scene
                        // graph (scenefx, sway) stacks them. Drawing every shadow
                        // below every window instead only ever showed a halo
                        // around the whole group.
                        //
                        // Dead windows are skipped, so a window and its shadow
                        // disappear in the same frame: the space keeps an element
                        // until its next refresh, and a shadow drawn for one would
                        // outlive its window.
                        //
                        // The window list is collected first so its borrow of
                        // `state.space` ends before `state.shadows` is touched.
                        let windows: Vec<_> = state
                            .space
                            .elements()
                            .rev()
                            .filter(|w| w.alive())
                            .filter_map(|w| {
                                let id = w.toplevel()?.wl_surface().id();
                                let geo = state.space.element_geometry(w)?;
                                // Exactly what Space::render_elements_for_region
                                // uses: the mapped location minus the window's
                                // own geometry offset.
                                let loc = state.space.element_location(w)? - w.geometry().loc;
                                Some((w.clone(), id, geo, loc))
                            })
                            .collect();
                        let live: Vec<_> = windows.iter().map(|(_, id, _, _)| id.clone()).collect();
                        state.shadows.retain(&live);
                        let mut layers: Vec<(
                            Vec<WaylandSurfaceRenderElement<GlesRenderer>>,
                            Option<smithay::backend::renderer::gles::element::PixelShaderElement>,
                        )> = Vec::with_capacity(windows.len());
                        for (window, id, geo, loc) in &windows {
                            let elements = smithay::backend::renderer::element::AsRenderElements::<
                                GlesRenderer,
                            >::render_elements(
                                window,
                                renderer,
                                loc.to_physical_precise_round(scale_f),
                                smithay::utils::Scale::from(scale_f),
                                1.0,
                            );
                            let shadow = state.shadows.element(renderer, id, *geo, scale_f);
                            layers.push((elements, shadow));
                        }

                        // Earlier in the list is higher up. Overlay first (cursor,
                        // DnD ghost, egui damage), then window-with-its-shadow
                        // from front to back, then the backdrop at the very
                        // bottom so the shadows fall on it too.
                        let mut elements: Vec<OutputElement<'_>> = Vec::new();
                        elements.extend(custom.iter().map(OutputElement::Custom));
                        for (window_elements, shadow) in &layers {
                            elements.extend(window_elements.iter().map(OutputElement::Surface));
                            elements.extend(shadow.iter().map(OutputElement::Shadow));
                        }
                        elements.extend(backdrop.iter().map(OutputElement::Custom));
                        damage_tracker
                            .render_output(renderer, &mut framebuffer, age, &elements, clear_color)
                            .map_err(|e| e.to_string())
                    }
                    Err(e) => Err(e.to_string()),
                };
                let res = match render_res {
                    Ok(res) => res,
                    Err(e) => {
                        // Retry on the next frame: this one produced nothing, and
                        // the toolbar output it ran is still pending (kept, not
                        // dropped, by Toolbar::run).
                        tracing::warn!("render skipped this frame: {e}");
                        state.dirty = true;
                        state.toolbar_changed = true;
                        drop(backend);
                        state.wake();
                        return;
                    }
                };

                // Toolbar built lazily AFTER the first render pass: the egui
                // painter needs the GL context current, which only rendering
                // guarantees (bind alone defers it). Re-arm the dirty flag so
                // the strip paints on the very next frame.
                if state.toolbar.is_none() && !state.toolbar_failed {
                    match crate::toolbar::Toolbar::new() {
                        Some(tb) => {
                            state.toolbar = Some(tb);
                            state.dirty = true;
                        }
                        None => state.toolbar_failed = true,
                    }
                }

                // Paint egui on top, into the same (still-bound, still-current)
                // framebuffer, before the swap. Skipped when nothing will be
                // submitted: painting a buffer the damage tracker considers
                // unchanged would leave pixels it doesn't know about.
                if res.damage.is_some() {
                    if let Some(tb) = state.toolbar.as_mut() {
                        tb.paint((size.w, size.h));
                    }
                }

                // Keep rendering while egui is animating (menu fade, hover, the
                // notice banner countdown): it asks for another frame.
                if state.toolbar.as_ref().is_some_and(|tb| tb.wants_repaint()) {
                    state.dirty = true;
                    state.toolbar_changed = true;
                }

                // Swap with the real damage: the host compositor recomposites
                // only what actually changed. No damage means nothing was
                // rendered, so there is nothing to swap.
                if let Some(damage) = res.damage {
                    state.submits = state.submits.saturating_add(1);
                    if let Err(e) = backend.submit(Some(damage.as_slice())) {
                        tracing::warn!("submit skipped this frame: {e}");
                        state.dirty = true;
                        drop(backend);
                        state.wake();
                        return;
                    }
                }

                state.space.elements().for_each(|window| {
                    window.send_frame(
                        &output,
                        state.start_time.elapsed(),
                        Some(Duration::ZERO),
                        |_, _| Some(output.clone()),
                    )
                });

                // The frame callbacks just queued (and everything else) go out in
                // the event loop's own callback, which runs right after this.
                // Another frame is already due (an egui animation, a lazily built
                // toolbar): ask for it directly, since the pacing timer may be
                // sleeping until the next scan. The backend borrow must go first,
                // because request_redraw takes it again.
                drop(backend);
                if state.dirty {
                    state.wake();
                }
            }
            WinitEvent::CloseRequested => {
                state.loop_signal.stop();
            }
            _ => (),
        };
    })?;

    // Pace redraws at ~60fps with a timer instead of an unbounded self-requesting
    // redraw. A nested compositor must still redraw periodically to pick up client
    // commits (they don't wake winit), so we can't render purely on demand; the
    // timer bounds it to the refresh rate. Single-threaded calloop means this
    // borrow never overlaps the render's borrow_mut.
    use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
    event_loop
        .handle()
        .insert_source(Timer::immediate(), move |_, _, state| {
            // Wake the renderer only when something changed (a client commit,
            // input, an egui animation, the scan below). Re-drawing an unchanged
            // frame is the ~130% CPU we are avoiding.
            if state.dirty {
                backend.borrow().window().request_redraw();
                return TimeoutAction::ToDuration(Duration::from_millis(16));
            }
            // Idle: nothing to draw. Sleep a whole frame budget instead of
            // spinning; a client commit, input, a closing window or the scan
            // timer each wake the loop on their own source and ask for the
            // frame directly (State::wake), so nothing waits on this tick.
            TimeoutAction::ToDuration(Duration::from_millis(250))
        })?;

    // Discovery scan on its own timer: it is pure file polling, so it is not tied
    // to rendering - a frame happens only when the scan finds something changed.
    event_loop
        .handle()
        .insert_source(Timer::immediate(), move |_, _, state| {
            // try_borrow: skip this tick if the renderer holds the backend (the
            // scan only needs the window handle, and the next tick is 250ms away).
            if let Ok(b) = backend_scan.try_borrow() {
                // Debug logging: one line a second with what the render loop is
                // actually doing, so a "it feels slow" report has numbers behind
                // it (see docs/debugging.md).
                if crate::debug_enabled() {
                    let now = state.start_time.elapsed();
                    if now.saturating_sub(state.debug_logged_at) >= Duration::from_secs(1) {
                        state.debug_logged_at = now;
                        crate::vcdebug(&format!(
                            "[+{:6.1}s] frames={} submits={} windows={} dirty={} status={:?}",
                            now.as_secs_f32(),
                            state.frames,
                            state.submits,
                            state.space.elements().count(),
                            state.dirty,
                            state.toolbar.as_ref().and_then(|t| t.status_text()),
                        ));
                        state.frames = 0;
                        state.submits = 0;
                    }
                }
                if run_discovery_scan(state, b.window()) {
                    state.dirty = true;
                    state.toolbar_changed = true;
                    b.window().request_redraw();
                }
            }
            TimeoutAction::ToDuration(SCAN_INTERVAL)
        })?;

    Ok(())
}
