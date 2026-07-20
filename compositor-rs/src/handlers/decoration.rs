//! Decoration policy. We render no server-side decorations, so we advertise both
//! decoration managers (the xdg one and the older `org_kde_kwin_server_decoration`
//! that Qt5/KDE still probe) and force **client-side** in every case. The app
//! then draws its own titlebar (Breeze CSD), which our interactive move/resize
//! grabs rely on (there is no server titlebar for the user to drag otherwise).

use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as XdgMode;
use smithay::reexports::wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration::{
    Mode as KdeMode, OrgKdeKwinServerDecoration,
};
use smithay::reexports::wayland_server::{WEnum, protocol::wl_surface::WlSurface};
use smithay::wayland::shell::kde::decoration::{KdeDecorationHandler, KdeDecorationState};
use smithay::wayland::shell::xdg::ToplevelSurface;
use smithay::wayland::shell::xdg::decoration::XdgDecorationHandler;

use crate::State;

impl XdgDecorationHandler for State {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(XdgMode::ClientSide);
        });
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: XdgMode) {
        // Ignore what the client asked for, we can't draw SSD, so always CSD.
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(XdgMode::ClientSide);
        });
        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(XdgMode::ClientSide);
        });
        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
    }
}

impl KdeDecorationHandler for State {
    fn kde_decoration_state(&self) -> &KdeDecorationState {
        &self.kde_decoration_state
    }

    fn request_mode(
        &mut self,
        _surface: &WlSurface,
        decoration: &OrgKdeKwinServerDecoration,
        _mode: WEnum<KdeMode>,
    ) {
        // Force client-side regardless of the client's request (feedback-loop safe
        // per the protocol: the client is free to honor or ignore this).
        decoration.mode(KdeMode::Client);
    }
}
