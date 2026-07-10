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

pub fn init_winit(
    event_loop: &mut EventLoop<State>,
    state: &mut State,
) -> Result<(), Box<dyn std::error::Error>> {
    // Host window: app_id "veracage" so KWin pairs it with veracage.desktop and
    // shows the Veracage icon; title starts "Veracage" and becomes the vault's
    // volume label once a vault is open (updated below from the leader discovery).
    let attributes = WindowAttributes::default()
        .with_title("Veracage")
        .with_platform_attributes(Box::new(
            WindowAttributesWayland::default().with_name("veracage", "veracage"),
        ));
    let (mut backend, winit) = winit::init_from_attributes(attributes)?;

    // Host clipboard bridge on the compositor's own winit->host connection.
    {
        use raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
        if let Ok(RawDisplayHandle::Wayland(w)) =
            backend.window().display_handle().map(|h| h.as_raw())
        {
            // SAFETY: the display ptr belongs to the winit backend (compositor lifetime).
            state.host_clipboard = Some(unsafe {
                crate::clipboard::HostClipboard::from_display_ptr(w.display.as_ptr())
            });
        }
    }

    // Host scale factor (winit reports the host window's DPI): 1.0 on a 1x host —
    // so a no-op there — and the real factor on HiDPI, so apps render crisply
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
                        smithay::desktop::space::render_output::<
                            _,
                            WaylandSurfaceRenderElement<GlesRenderer>,
                            _,
                            _,
                        >(
                            &output,
                            renderer,
                            &mut framebuffer,
                            scale_f as f32,
                            0,
                            [&state.space],
                            &dnd,
                            &mut damage_tracker,
                            [0.1, 0.1, 0.1, 1.0],
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
                // Refresh the launcher list from the leaders' .apps files (~1s).
                let now = state.start_time.elapsed();
                if now.saturating_sub(state.leaders_scan_at)
                    >= std::time::Duration::from_secs(1)
                {
                    let fresh = crate::toolbar::scan_leaders();
                    let changed = fresh.len() != state.leaders.len()
                        || fresh
                            .iter()
                            .zip(&state.leaders)
                            .any(|(a, b)| a.names != b.names || a.label != b.label);
                    if changed {
                        tracing::debug!(
                            "toolbar: launchers = {:?}",
                            fresh.iter().map(|l| l.names.clone()).collect::<Vec<_>>()
                        );
                        // Title = the open vault(s) by label; the app_id keeps the icon.
                        let title = if fresh.is_empty() {
                            "Veracage".to_string()
                        } else {
                            fresh
                                .iter()
                                .map(|l| l.label.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        };
                        backend.window().set_title(&title);
                    }
                    state.leaders = fresh;
                    state.leaders_scan_at = now;
                }
                let scale = output.current_scale().fractional_scale();
                // The desktop indicator shows only with an empty space; a mapped
                // sandbox window means the app IS the indication (and a filled
                // CentralPanel would paint over it).
                let has_windows = state.space.elements().next().is_some();
                let action = if let Some(tb) = state.toolbar.as_mut() {
                    tb.render((size.w, size.h), scale, &state.leaders, has_windows)
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
                        crate::toolbar::request_command(verb)
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
