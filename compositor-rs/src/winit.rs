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

use crate::State;

use smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement;
use smithay::backend::renderer::{ImportAll, ImportMem};

// Custom render elements composited on top of the app windows: the DnD ghost
// (a wayland surface, needs ImportAll) and the "No volume mounted" hint icon (a
// memory buffer drawn directly by the renderer, needs ImportMem, bypasses
// egui, whose sRGB texture path fringes the icon's transparent edges).
smithay::backend::renderer::element::render_elements! {
    HintElement<R> where R: ImportMem + ImportAll;
    Surface = WaylandSurfaceRenderElement<R>,
    Memory = MemoryRenderBufferRenderElement<R>,
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
    let (mut backend, winit) = winit::init_from_attributes(attributes)?;

    // Host clipboard bridge on the compositor's own winit->host connection.
    {
        use raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
        if let Ok(RawDisplayHandle::Wayland(w)) =
            backend.window().display_handle().map(|h| h.as_raw())
        {
            // SAFETY: the display ptr belongs to the winit backend (compositor lifetime).
            state.host_clipboard =
                unsafe { crate::hostclip::HostClipboard::from_display_ptr(w.display.as_ptr()) };
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

    // The backdrop IS the desktop: drawn first, behind every window. Themed:
    // light-gray under the light theme, near-black under dark.
    let clear_color: [f32; 4] = if std::env::var("VERACAGE_THEME").as_deref() == Ok("dark") {
        [0.10, 0.10, 0.10, 1.0]
    } else {
        [0.85, 0.85, 0.87, 1.0]
    };

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
            }
            WinitEvent::Input(event) => state.process_input_event(event),
            WinitEvent::Redraw => {
                let size = backend.window_size();
                let damage = Rectangle::from_size(size);
                let has_windows = state.space.elements().next().is_some();

                // A transient EGL/GL error (context loss, host-resize race, GL OOM)
                // must skip the frame, not abort the compositor and every app.
                let scale_f = output.current_scale().fractional_scale();
                let render_err: Option<String> = match backend.bind() {
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
                        if !has_windows {
                            if let Some(buf) = &state.hint_icon {
                                // Position in LOGICAL points, then scale to
                                // physical for the element (it renders at the
                                // buffer's logical size = 96pt).
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
                                    custom.push(HintElement::Memory(el));
                                }
                            }
                        }
                        smithay::desktop::space::render_output::<
                            _,
                            HintElement<GlesRenderer>,
                            _,
                            _,
                        >(
                            &output,
                            renderer,
                            &mut framebuffer,
                            scale_f as f32,
                            0,
                            [&state.space],
                            &custom,
                            &mut damage_tracker,
                            clear_color,
                        )
                        .err()
                        .map(|e| e.to_string())
                    }
                    Err(e) => Some(e.to_string()),
                };
                if let Some(e) = render_err {
                    tracing::warn!("render skipped this frame: {e}");
                    backend.window().request_redraw();
                    return;
                }

                // Toolbar (Phase 3): paint egui on top, into the same (still-bound,
                // still-current) framebuffer, before we swap. Built lazily now
                // because it needs the GL context current, which bind() just made.
                if state.toolbar.is_none() && !state.toolbar_failed {
                    match crate::toolbar::Toolbar::new() {
                        Some(tb) => state.toolbar = Some(tb),
                        None => state.toolbar_failed = true,
                    }
                }
                // Refresh the launcher list from the leaders' .apps files and the
                // human-published config-app list (~1s).
                let now = state.start_time.elapsed();
                if now.saturating_sub(state.leaders_scan_at)
                    >= std::time::Duration::from_secs(1)
                {
                    let fresh = crate::toolbar::scan_leaders();
                    let changed = fresh.len() != state.leaders.len()
                        || fresh.iter().zip(&state.leaders).any(|(a, b)| {
                            a.names != b.names || a.label != b.label || a.volumes != b.volumes
                        });
                    if changed {
                        tracing::debug!(
                            "toolbar: launchers = {:?}",
                            fresh.iter().map(|l| l.names.clone()).collect::<Vec<_>>()
                        );
                        // Title = the mounted volumes; the app_id keeps the icon.
                        backend.window().set_title(&crate::toolbar::volumes_title(&fresh));
                    }
                    state.leaders = fresh;
                    state.cfg_apps = crate::toolbar::scan_config_apps();
                    let font = crate::toolbar::scan_font();
                    if let Some(tb) = state.toolbar.as_mut() {
                        tb.refresh_icons(&state.cfg_apps);
                        if let Some((path, base)) = font {
                            tb.refresh_font(&path, base);
                        }
                    }
                    // Live window resize: pick up a Settings change to the default
                    // window size (published to /run/veracage/pub/window.size).
                    if let Some(sz) = crate::toolbar::scan_window_size() {
                        if sz != state.window_size_applied {
                            apply_window_size(backend.window(), &sz);
                            state.window_size_applied = sz;
                        }
                    }
                    // Live keyboard shortcuts (Copy out / Paste in).
                    if let Some(binds) = crate::shortcuts::scan() {
                        let label = |b: &Option<crate::shortcuts::Keybind>| {
                            b.as_ref().map(|k| k.label()).unwrap_or_else(|| "unset".into())
                        };
                        if let Some(tb) = state.toolbar.as_mut() {
                            tb.refresh_shortcuts(label(&binds.copy_out), label(&binds.paste_in));
                        }
                        state.binds = binds;
                    }
                    // Live host-clipboard auto-clear policy; push to the worker
                    // only on a change so we don't poke it every scan.
                    if let Some(policy) = crate::toolbar::scan_clipclear() {
                        if policy != state.clip_clear_applied {
                            if let Some(hc) = &state.host_clipboard {
                                hc.set_policy(policy.0, policy.1);
                            }
                            state.clip_clear_applied = policy;
                        }
                    }
                    state.leaders_scan_at = now;
                }
                let scale = output.current_scale().fractional_scale();
                // The desktop indicator shows only with an empty space; a mapped
                // sandbox window means the app IS the indication (and a filled
                // CentralPanel would paint over it).
                let action = if let Some(tb) = state.toolbar.as_mut() {
                    tb.render(
                        (size.w, size.h),
                        scale,
                        &state.leaders,
                        &state.cfg_apps,
                        has_windows,
                    )
                } else {
                    crate::toolbar::ToolbarAction::None
                };
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

                if let Err(e) = backend.submit(Some(&[damage])) {
                    tracing::warn!("submit skipped this frame: {e}");
                    backend.window().request_redraw();
                    return;
                }

                state.space.elements().for_each(|window| {
                    window.send_frame(
                        &output,
                        state.start_time.elapsed(),
                        Some(Duration::ZERO),
                        |_, _| Some(output.clone()),
                    )
                });

                state.space.refresh();
                state.popups.cleanup();
                let _ = state.display_handle.flush_clients();

                // Ask for redraw to schedule new frame.
                backend.window().request_redraw();
            }
            WinitEvent::CloseRequested => {
                state.loop_signal.stop();
            }
            _ => (),
        };
    })?;

    Ok(())
}
