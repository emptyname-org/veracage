use smithay::{
    desktop::{
        PopupKeyboardGrab, PopupKind, PopupManager, PopupPointerGrab, PopupUngrabStrategy, Space, Window,
        find_popup_root_surface, get_popup_toplevel_coords,
    },
    input::{
        Seat,
        pointer::{Focus, GrabStartData as PointerGrabStartData},
    },
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            Resource,
            protocol::{wl_seat, wl_surface::WlSurface},
        },
    },
    utils::{Rectangle, SERIAL_COUNTER, Serial},
    wayland::{
        compositor::with_states,
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
            XdgToplevelSurfaceData,
        },
    },
};

use crate::{
    State,
    grabs::{MoveSurfaceGrab, ResizeSurfaceGrab},
};

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let window = Window::new_wayland_window(surface.clone());

        // Advertise output bounds so KDE dialogs size themselves sanely, and
        // cascade placement (deterministic, no `rand`) so a new window/dialog
        // doesn't land exactly on top of the previous one — smallvil mapped
        // everything at (0,0).
        let output_geo = self
            .space
            .outputs()
            .next()
            .and_then(|o| self.space.output_geometry(o));

        let loc = if let Some(geo) = output_geo {
            // Reserve the toolbar strip: the work area is the output minus the top
            // strip, and windows are placed inside it so their titlebars aren't
            // hidden under the overlay.
            let top = crate::toolbar::TOOLBAR_HEIGHT;
            let work_h = (geo.size.h - top).max(1);
            surface.with_pending_state(|state| {
                state.bounds = Some((geo.size.w, work_h).into());
            });
            let n = self.space.elements().count() as i32;
            let step = 32 * (n % 8);
            let x = step.min((geo.size.w - 200).max(0));
            let y = top + step.min((work_h - 200).max(0));
            (x, y)
        } else {
            (0, crate::toolbar::TOOLBAR_HEIGHT)
        };

        self.space.map_element(window, loc, true);
        tracing::debug!(
            "placement: mapped toplevel at {:?} (toolbar strip reserves y<{})",
            loc,
            crate::toolbar::TOOLBAR_HEIGHT
        );

        // Focus a newly-mapped window ONLY when nothing currently holds keyboard
        // focus (the genuine first-window / just-closed-window case). One
        // compositor hosts every vault's apps, so unconditional focus-on-map lets
        // a co-hosted (possibly hostile) app map a toplevel mid-keystroke and
        // capture input meant for the app the user is actually typing into. Don't
        // steal focus from an existing surface; also never fight a grab (open
        // popup/menu — A1). A launched app the user wasn't typing in still gets
        // focus (nothing was focused); one that pops up while you type does not.
        if let Some(keyboard) = self.seat.get_keyboard() {
            if !keyboard.is_grabbed() && keyboard.current_focus().is_none() {
                let serial = SERIAL_COUNTER.next_serial();
                keyboard.set_focus(self, Some(surface.wl_surface().clone()), serial);
            }
        }
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        self.unconstrain_popup(&surface);
        let _ = self.popups.track_popup(PopupKind::Xdg(surface));
    }

    fn reposition_request(&mut self, surface: PopupSurface, positioner: PositionerState, token: u32) {
        surface.with_pending_state(|state| {
            let geometry = positioner.get_geometry();
            state.geometry = geometry;
            state.positioner = positioner;
        });
        self.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }

    fn move_request(&mut self, surface: ToplevelSurface, seat: wl_seat::WlSeat, serial: Serial) {
        let Some(seat) = Seat::<State>::from_resource(&seat) else {
            return;
        };
        let wl_surface = surface.wl_surface();
        let Some(start_data) = check_grab(&seat, wl_surface, serial) else {
            return;
        };
        let Some(pointer) = seat.get_pointer() else {
            return;
        };
        // A client can request a move for a surface we no longer track (just
        // unmapped, or never mapped). Ignore it — never panic the compositor.
        let Some(window) = self.window_for_surface(wl_surface) else {
            return;
        };
        let Some(initial_window_location) = self.space.element_location(&window) else {
            return;
        };

        let grab = MoveSurfaceGrab {
            start_data,
            window,
            initial_window_location,
        };
        pointer.set_grab(self, grab, serial, Focus::Clear);
    }

    fn resize_request(
        &mut self,
        surface: ToplevelSurface,
        seat: wl_seat::WlSeat,
        serial: Serial,
        edges: xdg_toplevel::ResizeEdge,
    ) {
        let Some(seat) = Seat::<State>::from_resource(&seat) else {
            return;
        };
        let wl_surface = surface.wl_surface();
        let Some(start_data) = check_grab(&seat, wl_surface, serial) else {
            return;
        };
        let Some(pointer) = seat.get_pointer() else {
            return;
        };
        let Some(window) = self.window_for_surface(wl_surface) else {
            return;
        };
        let Some(initial_window_location) = self.space.element_location(&window) else {
            return;
        };
        let initial_window_size = window.geometry().size;

        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Resizing);
        });
        surface.send_pending_configure();

        let grab = ResizeSurfaceGrab::start(
            start_data,
            window,
            edges.into(),
            Rectangle::new(initial_window_location, initial_window_size),
        );
        pointer.set_grab(self, grab, serial, Focus::Clear);
    }

    fn grab(&mut self, surface: PopupSurface, seat: wl_seat::WlSeat, serial: Serial) {
        // Without this, KDE/Qt menus, comboboxes and context menus never receive
        // an explicit grab and won't dismiss on click-outside. Cribbed from
        // anvil/src/shell/xdg.rs:380, simplified: our seat focus IS `WlSurface`
        // (and `From<PopupKind> for WlSurface` exists), so the popup root is just
        // the surface — no `KeyboardFocusTarget` enum or layer-shell fallback.
        let Some(seat) = Seat::<State>::from_resource(&seat) else {
            return;
        };
        let kind = PopupKind::Xdg(surface);
        let Ok(root) = find_popup_root_surface(&kind) else {
            return;
        };
        let Ok(mut grab) = self.popups.grab_popup(root, kind, &seat, serial) else {
            return;
        };

        if let Some(keyboard) = seat.get_keyboard() {
            if keyboard.is_grabbed()
                && !(keyboard.has_grab(serial)
                    || keyboard.has_grab(grab.previous_serial().unwrap_or(serial)))
            {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            keyboard.set_focus(self, grab.current_grab(), serial);
            keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
        }
        if let Some(pointer) = seat.get_pointer() {
            if pointer.is_grabbed()
                && !(pointer.has_grab(serial)
                    || pointer.has_grab(grab.previous_serial().unwrap_or_else(|| grab.serial())))
            {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
        }
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        // Single nested output — "maximize" = fill it. Without this handler the
        // Breeze titlebar's maximize button hits the no-op default and does nothing.
        let Some(window) = self.window_for_surface(surface.wl_surface()) else {
            return;
        };
        let Some(output) = self.space.outputs().next().cloned() else {
            return;
        };
        let Some(geometry) = self.space.output_geometry(&output) else {
            return;
        };
        // Maximize fills the work area (output minus the toolbar strip), placed
        // just below the strip so the titlebar stays visible.
        let top = crate::toolbar::TOOLBAR_HEIGHT;
        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Maximized);
            state.size = Some((geometry.size.w, (geometry.size.h - top).max(1)).into());
        });
        self.space
            .map_element(window, (geometry.loc.x, geometry.loc.y + top), true);
        if surface.is_initial_configure_sent() {
            surface.send_configure();
        }
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        surface.with_pending_state(|state| {
            state.states.unset(xdg_toplevel::State::Maximized);
            state.size = None;
        });
        if surface.is_initial_configure_sent() {
            surface.send_configure();
        }
    }
}

fn check_grab(
    seat: &Seat<State>,
    surface: &WlSurface,
    serial: Serial,
) -> Option<PointerGrabStartData<State>> {
    let pointer = seat.get_pointer()?;

    // Check that this surface has a click grab.
    if !pointer.has_grab(serial) {
        return None;
    }

    let start_data = pointer.grab_start_data()?;

    let (focus, _) = start_data.focus.as_ref()?;
    // If the focus was for a different surface, ignore the request.
    if !focus.id().same_client_as(&surface.id()) {
        return None;
    }

    Some(start_data)
}

/// Should be called on `WlSurface::commit`
pub fn handle_commit(popups: &mut PopupManager, space: &Space<Window>, surface: &WlSurface) {
    // Handle toplevel commits.
    if let Some(window) = space
        .elements()
        .find(|w| w.toplevel().map(|t| t.wl_surface() == surface).unwrap_or(false))
        .cloned()
    {
        let initial_configure_sent = with_states(surface, |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .unwrap()
                .lock()
                .unwrap()
                .initial_configure_sent
        });

        if !initial_configure_sent {
            window.toplevel().unwrap().send_configure();
        }
    }

    // Handle popup commits.
    popups.commit(surface);
    if let Some(popup) = popups.find_popup(surface) {
        match popup {
            PopupKind::Xdg(ref xdg) => {
                if !xdg.is_initial_configure_sent() {
                    // NOTE: This should never fail as the initial configure is always
                    // allowed.
                    xdg.send_configure().expect("initial configure failed");
                }
            }
            PopupKind::InputMethod(ref _input_method) => {}
        }
    }
}

impl State {
    fn unconstrain_popup(&self, popup: &PopupSurface) {
        let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(popup.clone())) else {
            return;
        };
        let Some(window) = self.window_for_surface(&root) else {
            return;
        };

        let Some(output) = self.space.outputs().next() else {
            return;
        };
        let Some(output_geo) = self.space.output_geometry(output) else {
            return;
        };
        let Some(window_geo) = self.space.element_geometry(&window) else {
            return;
        };

        // The target geometry for the positioner should be relative to its parent's geometry, so
        // we will compute that here.
        let mut target = output_geo;
        target.loc -= get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));
        target.loc -= window_geo.loc;

        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }
}
