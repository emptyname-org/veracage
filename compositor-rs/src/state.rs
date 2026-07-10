use std::os::unix::io::AsRawFd;
use std::{ffi::OsString, sync::Arc};

/// Peer uid of a connected unix socket via SO_PEERCRED (std's `peer_cred()` is
/// still unstable). None if the credentials can't be read.
fn peer_uid(fd: i32) -> Option<u32> {
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let r = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    };
    if r == 0 { Some(cred.uid) } else { None }
}

use smithay::{
    desktop::{PopupManager, Space, Window, WindowSurfaceType},
    input::{Seat, SeatState},
    reexports::{
        calloop::{EventLoop, Interest, LoopSignal, Mode, PostAction, generic::Generic},
        wayland_server::{
            Display, DisplayHandle,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
        },
    },
    utils::{Logical, Point},
    wayland::{
        compositor::{CompositorClientState, CompositorState},
        fractional_scale::FractionalScaleManagerState,
        output::OutputManagerState,
        selection::data_device::DataDeviceState,
        selection::primary_selection::PrimarySelectionState,
        shell::kde::decoration::KdeDecorationState,
        shell::xdg::XdgShellState,
        shell::xdg::decoration::XdgDecorationState,
        shm::ShmState,
        socket::ListeningSocketSource,
        viewporter::ViewporterState,
    },
};
use smithay::reexports::wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration_manager::Mode as KdeManagerMode;

pub struct State {
    pub start_time: std::time::Instant,
    pub socket_name: OsString,
    pub display_handle: DisplayHandle,

    pub space: Space<Window>,
    pub loop_signal: LoopSignal,

    // Smithay State
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<State>,
    pub data_device_state: DataDeviceState,
    pub primary_selection_state: PrimarySelectionState,
    // We draw no server-side decorations; both are advertised so Qt/KDE's
    // decoration negotiation succeeds, and both force client-side (handlers/decoration.rs).
    pub xdg_decoration_state: XdgDecorationState,
    pub kde_decoration_state: KdeDecorationState,
    pub viewporter_state: ViewporterState,
    pub fractional_scale_manager_state: FractionalScaleManagerState,
    pub popups: PopupManager,

    pub seat: Seat<Self>,

    /// Text the compositor currently serves as the selection source (set via the
    /// leader's clipboard channel). `send_selection` writes this to pasting apps.
    pub clip_source: Option<Arc<Vec<u8>>>,

    /// Host clipboard bridge (compositor's own winit->host connection). Set in
    /// `init_winit` once the backend exists; None if the host isn't Wayland.
    pub host_clipboard: Option<crate::clipboard::HostClipboard>,

    /// In-window egui toolbar (Phase 3). Built lazily on the first Redraw (needs
    /// the GL context current); `toolbar_failed` latches a construction failure so
    /// we don't retry every frame — the compositor then just runs with no toolbar.
    pub toolbar: Option<crate::toolbar::Toolbar>,
    pub toolbar_failed: bool,

    /// Toolbar launcher list, refreshed (throttled) from the leaders' `.apps`
    /// files so app buttons appear/disappear as vaults open and close.
    pub leaders: Vec<crate::toolbar::LeaderApps>,
    pub leaders_scan_at: std::time::Duration,

    /// The current drag-and-drop icon surface (the "ghost" that follows the
    /// cursor during a DnD), set when a client starts a drag and cleared on drop.
    /// Composited at the pointer each frame — without it a drag has no visual
    /// feedback even though the drop itself works.
    pub dnd_icon: Option<DndIcon>,
}

/// A drag-and-drop icon: the client's icon surface plus the offset from the
/// cursor hotspot at which to draw it.
#[derive(Debug)]
pub struct DndIcon {
    pub surface: WlSurface,
    pub offset: Point<i32, Logical>,
}

impl State {
    pub fn new(event_loop: &mut EventLoop<Self>, display: Display<Self>, socket: Option<String>) -> Self {
        let start_time = std::time::Instant::now();

        let dh = display.handle();

        // Here we initialize implementations of some wayland protocols
        // Some of them require us to implement traits on the State state,
        // you can find those implementations in the `crate::handlers` module

        // Initialize protocols needed for displaying windows
        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let popups = PopupManager::default();

        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);

        // Data device is responsible for clipboard and drag-and-drop
        let data_device_state = DataDeviceState::new::<Self>(&dh);
        // Primary selection (middle-click paste) between sandboxed apps.
        let primary_selection_state = PrimarySelectionState::new::<Self>(&dh);

        // Advertise both decoration managers and force client-side decorations
        // (we render no titlebars). Qt5/KDE probe the older org_kde_kwin one.
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&dh);
        let kde_decoration_state = KdeDecorationState::new::<Self>(&dh, KdeManagerMode::Client);

        // wp_viewporter + fractional-scale so HiDPI apps aren't pinned to integer
        // 1x. No-ops on a 1x host; the output scale (winit.rs) reflects the host.
        let viewporter_state = ViewporterState::new::<Self>(&dh);
        let fractional_scale_manager_state = FractionalScaleManagerState::new::<Self>(&dh);

        // A seat is a group of keyboards, pointer and touch devices.
        // A seat typically has a pointer and maintains a keyboard focus and a pointer focus.
        let mut seat_state = SeatState::new();
        let mut seat: Seat<Self> = seat_state.new_wl_seat(&dh, "winit");

        // Notify clients that we have a keyboard, for the sake of the example we assume that keyboard is always present.
        // You may want to track keyboard hot-plug in real compositor.
        // A real compositor must not die on a bad/custom keymap: try the
        // environment's layout, and if it won't compile, fall back to plain US.
        if seat.add_keyboard(Default::default(), 200, 25).is_err() {
            eprintln!("veracage-compositor: environment keymap failed to compile; falling back to 'us'");
            seat.add_keyboard(
                smithay::input::keyboard::XkbConfig { layout: "us", ..Default::default() },
                200,
                25,
            )
            .expect("us keymap must compile");
        }

        // Notify clients that we have a pointer (mouse)
        // Here we assume that there is always pointer plugged in
        seat.add_pointer();

        // A space represents a two-dimensional plane. Windows and Outputs can be mapped onto it.
        //
        // Windows get a position and stacking order through mapping.
        // Outputs become views of a part of the Space and can be rendered via Space::render_output.
        let space = Space::default();

        // Setup a wayland socket that will be used to accept clients
        let socket_name = Self::init_wayland_listener(display, event_loop, socket);

        // Get the loop signal, used to stop the event loop
        let loop_signal = event_loop.get_signal();

        Self {
            start_time,
            display_handle: dh,

            space,
            loop_signal,
            socket_name,

            compositor_state,
            xdg_shell_state,
            shm_state,
            output_manager_state,
            seat_state,
            data_device_state,
            primary_selection_state,
            xdg_decoration_state,
            kde_decoration_state,
            viewporter_state,
            fractional_scale_manager_state,
            popups,
            seat,
            clip_source: None,
            host_clipboard: None,
            toolbar: None,
            toolbar_failed: false,
            dnd_icon: None,
            leaders: Vec::new(),
            leaders_scan_at: std::time::Duration::ZERO,
        }
    }

    fn init_wayland_listener(
        display: Display<State>,
        event_loop: &mut EventLoop<Self>,
        socket: Option<String>,
    ) -> OsString {
        // Use the leader-supplied socket name if given, else auto-pick.
        let listening_socket = match socket {
            Some(name) => ListeningSocketSource::with_name(&name).unwrap(),
            None => ListeningSocketSource::new_auto().unwrap(),
        };

        // Get the name of the listening socket.
        // Clients will connect to this socket.
        let socket_name = listening_socket.socket_name().to_os_string();

        let loop_handle = event_loop.handle();

        loop_handle
            .insert_source(listening_socket, move |client_stream, _, state| {
                // Only accept connections from our OWN (veracage) uid. The socket
                // is 0700 via the helper's umask, but check the peer explicitly so
                // sandbox isolation doesn't rest solely on the ambient umask being
                // right — a future permissive-umask regression can't then silently
                // expose the sandbox selection/seat to another local uid.
                let euid = unsafe { libc::geteuid() };
                match peer_uid(client_stream.as_raw_fd()) {
                    Some(uid) if uid == euid => {
                        if let Err(e) = state
                            .display_handle
                            .insert_client(client_stream, Arc::new(ClientState::default()))
                        {
                            tracing::warn!("insert_client failed: {e}");
                        }
                    }
                    Some(uid) => tracing::warn!(
                        "rejecting wayland client from uid {uid} (compositor uid {euid})"
                    ),
                    None => tracing::warn!("rejecting client: cannot read peer cred"),
                }
            })
            .expect("Failed to init the wayland event source.");

        // You also need to add the display itself to the event loop, so that client events will be processed by wayland-server.
        loop_handle
            .insert_source(
                Generic::new(display, Interest::READ, Mode::Level),
                |_, display, state| {
                    // Safety: we don't drop the display
                    unsafe {
                        display.get_mut().dispatch_clients(state).unwrap();
                    }
                    Ok(PostAction::Continue)
                },
            )
            .unwrap();

        socket_name
    }

    pub fn surface_under(&self, pos: Point<f64, Logical>) -> Option<(WlSurface, Point<f64, Logical>)> {
        self.space.element_under(pos).and_then(|(window, location)| {
            window
                .surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                .map(|(s, p)| (s, (p + location).to_f64()))
        })
    }

    /// The tracked toplevel window backing `surface`, if any. Returns `None` for
    /// a surface we don't track (unmapped, never mapped, or not a toplevel) so a
    /// client request naming a stale surface can be ignored rather than panicking
    /// the compositor — which would tear down every sandboxed app. Folds the
    /// `toplevel()` check in safely (no `.unwrap()`).
    pub fn window_for_surface(&self, surface: &WlSurface) -> Option<Window> {
        self.space
            .elements()
            .find(|w| w.toplevel().map(|t| t.wl_surface() == surface).unwrap_or(false))
            .cloned()
    }
}

/// Data associated with a wayland client that connects to State.
/// One instance of this type per client.
#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}
