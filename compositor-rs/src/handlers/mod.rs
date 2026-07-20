mod compositor;
mod decoration;
mod xdg_shell;

use crate::State;
use crate::state::DndIcon;

//
// Wl Seat
//

use smithay::input::dnd::{DnDGrab, DndGrabHandler, DndTarget, GrabType, Source};
use smithay::input::pointer::Focus;
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Serial};
use smithay::wayland::compositor::with_states;
use smithay::wayland::fractional_scale::{FractionalScaleHandler, with_fractional_scale};
use smithay::wayland::output::OutputHandler;
use smithay::wayland::selection::{SelectionHandler, SelectionTarget};
use smithay::wayland::selection::data_device::{
    DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler, set_data_device_focus,
};
use smithay::wayland::selection::primary_selection::{
    PrimarySelectionHandler, PrimarySelectionState, set_primary_focus,
};

impl SeatHandler for State {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<State> {
        &mut self.seat_state
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, _image: smithay::input::pointer::CursorImageStatus) {}

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let dh = &self.display_handle;
        let client = focused.and_then(|s| dh.get_client(s.id()).ok());
        set_data_device_focus(dh, seat, client.clone());
        // Primary selection needs its OWN focus call: without it, middle-click
        // paste never crosses between windows (the primary offer isn't delivered
        // to the newly focused client).
        set_primary_focus(dh, seat, client);
    }
}

//
// Wl Data Device
//

impl SelectionHandler for State {
    type SelectionUserData = ();

    /// An app is pasting the selection the leader set: hand over the bytes.
    /// Write on a thread so a slow reader can't stall the compositor.
    fn send_selection(
        &mut self,
        _ty: SelectionTarget,
        _mime_type: String,
        fd: std::os::unix::io::OwnedFd,
        _seat: Seat<Self>,
        _user_data: &(),
    ) {
        // Fires when an app requests the selection bytes (i.e. pastes). No log
        // here: the old `eprintln!` leaked paste metadata (mime + byte length) to
        // the journal on every paste.
        // `clip_source` is an Arc, so this clone is a refcount bump, not a copy of
        // the (up to 16 MiB) buffer. The write runs on a bounded, non-panicking
        // worker: a hostile app looping `receive` can't exhaust threads/fds or
        // panic the compositor. Over the cap `fd` drops here and it sees an empty
        // paste.
        if let Some(text) = self.clip_source.clone() {
            crate::clipboard::spawn_clip_worker(move || {
                crate::clipboard::write_all_bounded(fd, &text[..]);
            });
        }
    }
}

impl DataDeviceHandler for State {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device_state
    }
}

impl PrimarySelectionHandler for State {
    fn primary_selection_state(&mut self) -> &mut PrimarySelectionState {
        &mut self.primary_selection_state
    }
}

impl DndGrabHandler for State {
    // Remove the drag icon once the drag ends (drop or cancel), otherwise the
    // ghost would linger under the cursor after the DnD completes.
    fn dropped(
        &mut self,
        _target: Option<DndTarget<'_, Self>>,
        _validated: bool,
        _seat: Seat<Self>,
        _location: Point<f64, Logical>,
    ) {
        self.dnd_icon = None;
    }
    fn cancelled(&mut self, _seat: Seat<Self>, _location: Point<f64, Logical>) {
        self.dnd_icon = None;
    }
}
impl WaylandDndGrabHandler for State {
    fn dnd_requested<S: Source>(
        &mut self,
        source: S,
        icon: Option<WlSurface>,
        seat: Seat<Self>,
        serial: Serial,
        type_: GrabType,
    ) {
        // Track the icon surface so the render loop can composite it at the
        // cursor (the drag's visual feedback; the drop itself works regardless).
        self.dnd_icon = icon.map(|surface| DndIcon { surface, offset: (0, 0).into() });
        match type_ {
            GrabType::Pointer => {
                let ptr = seat.get_pointer().unwrap();
                let start_data = ptr.grab_start_data().unwrap();

                // create a dnd grab to start the operation
                let grab = DnDGrab::new_pointer(&self.display_handle, start_data, source, seat);
                ptr.set_grab(self, grab, serial, Focus::Keep);
            }
            GrabType::Touch => {
                // smallvil lacks touch handling
                source.cancel();
            }
        }
    }
}

//
// Wl Output & Xdg Output
//

impl OutputHandler for State {}

impl FractionalScaleHandler for State {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        // Single nested output, hand every surface that output's scale.
        let Some(scale) = self
            .space
            .outputs()
            .next()
            .map(|o| o.current_scale().fractional_scale())
        else {
            return;
        };
        with_states(&surface, |states| {
            with_fractional_scale(states, |fs| {
                fs.set_preferred_scale(scale);
            });
        });
    }
}

smithay::delegate_dispatch2!(State);
