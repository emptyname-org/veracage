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
        cursor_shape::CursorShapeManagerState,
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
    /// wp_cursor_shape_v1. With it, a toolkit asks for a cursor by NAME
    /// ("se-resize") instead of loading an XCursor theme itself and handing us a
    /// cursor surface. The host window then draws the themed cursor, so resize
    /// cursors work even though the sandbox has no cursor theme configured.
    /// Held only to keep the global alive.
    pub cursor_shape_state: CursorShapeManagerState,
    pub popups: PopupManager,

    pub seat: Seat<Self>,

    /// Text the compositor currently serves as the selection source (set via the
    /// leader's clipboard channel). `send_selection` writes this to pasting apps.
    pub clip_source: Option<Arc<Vec<u8>>>,

    /// Host clipboard bridge (compositor's own winit->host connection). Set in
    /// `init_winit` once the backend exists; None if the host isn't Wayland.
    pub host_clipboard: Option<crate::hostclip::HostClipboard>,

    /// The host-clipboard worker thread. Joined at teardown (after clear-on-exit)
    /// so it stops touching the winit backend's wl_display before that backend is
    /// dropped - otherwise the worker faults on the freed display (SIGSEGV).
    pub host_clipboard_worker: Option<std::thread::JoinHandle<()>>,

    /// In-window egui toolbar (Phase 3). Built lazily on the first Redraw (needs
    /// the GL context current); `toolbar_failed` latches a construction failure so
    /// we don't retry every frame. The compositor then just runs with no toolbar.
    pub toolbar: Option<crate::toolbar::Toolbar>,
    pub toolbar_failed: bool,

    /// Something changed and a frame must be (re)rendered: a client committed, an
    /// input arrived, or egui is animating. The compositor software-renders
    /// (llvmpipe), so redrawing an unchanged frame is pure CPU waste - the timer
    /// only wakes the renderer when this is set (or the ~1s scan is due). Starts
    /// true so the first frame paints.
    pub dirty: bool,

    /// Ask winit for a frame (set by `init_winit`). Marking the output dirty is
    /// not enough on its own: the pacing timer idles until the next scan, so
    /// whatever sets `dirty` outside the render path must also call `wake`.
    pub request_redraw: Option<std::rc::Rc<dyn Fn()>>,

    /// Toolbar launcher list, refreshed by the discovery scan from the leaders'
    /// `.apps` files so app buttons appear/disappear as volumes open and close.
    pub leaders: Vec<crate::toolbar::LeaderApps>,

    /// The toolbar's own output may have changed (input on the strip, an egui
    /// animation, a discovery scan), so the overlay's damage region must be
    /// re-reported. See EguiDamage in winit.rs.
    pub toolbar_changed: bool,

    /// Last transient-notice nonce shown (a leader-reported failed launch, etc.),
    /// so each distinct notice shows once. Seeded from any file present at
    /// startup, so a stale notice from before the compositor started is not shown.
    pub notice_nonce: u64,

    /// The human-published configured apps (`/run/veracage/pub/config.apps`),
    /// listed in the Apps menu when no volume is mounted.
    pub cfg_apps: Vec<crate::toolbar::ConfigApp>,

    /// The window size last applied from `/run/veracage/pub/window.size`, so a
    /// Settings change resizes the live window (initialized to the startup env
    /// value, so the first matching scan is a no-op).
    pub window_size_applied: String,

    /// Active Veracage keyboard shortcuts (clipboard transfers), refreshed from
    /// `/run/veracage/pub/shortcuts` on the ~1s scan.
    pub binds: crate::shortcuts::Binds,

    /// The host-clipboard auto-clear policy (enabled, timeout secs) last pushed
    /// to the clipboard worker, so the ~1s scan only sends it on a change.
    /// Initialized to the worker's own secure default (enabled, 30s).
    pub clip_clear_applied: (bool, u32),

    /// The current drag-and-drop icon surface (the "ghost" that follows the
    /// cursor during a DnD), set when a client starts a drag and cleared on drop.
    /// Composited at the pointer each frame: without it a drag has no visual
    /// feedback even though the drop itself works.
    pub dnd_icon: Option<DndIcon>,

    /// The "No volume mounted" hint icon as a smithay memory buffer, drawn
    /// directly by the renderer, bypasses the egui_glow texture path (whose
    /// sRGB handling fringes a transparent-edged icon), so it renders cleanly
    /// like the app windows and backdrop.
    pub hint_icon: Option<smithay::backend::renderer::element::memory::MemoryRenderBuffer>,

    /// The launch progress note being tracked (see toolbar::LaunchNote): it
    /// finishes when the launching app maps its own window, and stays finished.
    pub status_note: Option<crate::toolbar::LaunchNote>,

    /// What the focused client wants the cursor to look like. Applied every frame in
    /// winit.rs: a named shape goes to the host window, a surface is composited
    /// by us at the pointer. Without this the cursor never changed shape, which
    /// is why window borders were so hard to grab.
    pub cursor_status: smithay::input::pointer::CursorImageStatus,

    /// The desktop's text lines, rasterised for the backdrop (hint_text.rs), and
    /// the font they are drawn with (path + base size, from the discovery scan).
    pub hint_text: crate::hint_text::HintText,
    pub font: Option<(String, f32)>,

    /// Per-window drop shadows (scenefx's shader; see shadow.rs).
    pub shadows: crate::shadow::Shadows,

    /// Debug counters, reported once a second when `VERACAGE_DEBUG=1`: frames
    /// rendered and buffers submitted since the last report (a frame with no
    /// damage renders but submits nothing), plus when that report last ran.
    pub frames: u32,
    pub submits: u32,
    pub debug_logged_at: std::time::Duration,
}

/// A drag-and-drop icon: the client's icon surface plus the offset from the
/// cursor hotspot at which to draw it.
#[derive(Debug)]
pub struct DndIcon {
    pub surface: WlSurface,
    pub offset: Point<i32, Logical>,
}

impl State {
    /// Ask winit for a frame. Pair it with every `dirty = true` outside the
    /// render path, so the change is drawn now rather than at the next scan.
    pub fn wake(&self) {
        if let Some(request) = &self.request_redraw {
            request();
        }
    }

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
        let cursor_shape_state = CursorShapeManagerState::new::<Self>(&dh);

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
            cursor_shape_state,
            popups,
            seat,
            clip_source: None,
            host_clipboard: None,
            host_clipboard_worker: None,
            toolbar: None,
            toolbar_failed: false,
            dnd_icon: None,
            hint_icon: build_hint_icon(),
            cursor_status: smithay::input::pointer::CursorImageStatus::default_named(),
            hint_text: Default::default(),
            font: None,
            shadows: Default::default(),
            status_note: None,
            frames: 0,
            submits: 0,
            debug_logged_at: std::time::Duration::ZERO,
            dirty: true,
            request_redraw: None,
            leaders: Vec::new(),
            toolbar_changed: true,
            // Baseline: adopt any notice already on disk without showing it, so a
            // stale one from before this compositor started stays hidden.
            notice_nonce: crate::toolbar::scan_notice().map(|(n, _)| n).unwrap_or(0),
            cfg_apps: Vec::new(),
            window_size_applied: std::env::var("VERACAGE_WINDOW_SIZE")
                .unwrap_or_else(|_| "default".into()),
            binds: crate::shortcuts::Binds::default(),
            clip_clear_applied: crate::hostclip::DEFAULT_CLEAR_POLICY,
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
                // right. A future permissive-umask regression can't then silently
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
                    // Safety: we don't drop the display. A dispatch error must
                    // NOT unwind: this one compositor hosts every open volume's
                    // apps, so a panic here would tear them all down. Log and
                    // continue (per-client protocol errors are handled inside
                    // wayland-server, so this fires only on a fatal condition).
                    unsafe {
                        if let Err(e) = display.get_mut().dispatch_clients(state) {
                            tracing::error!("wayland dispatch_clients error: {e}");
                        }
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
    /// the compositor, which would tear down every sandboxed app. Folds the
    /// `toplevel()` check in safely (no `.unwrap()`).
    pub fn window_for_surface(&self, surface: &WlSurface) -> Option<Window> {
        self.space
            .elements()
            .find(|w| w.toplevel().map(|t| t.wl_surface() == surface).unwrap_or(false))
            .cloned()
    }
}

/// Build the smithay memory buffer for the hint icon from the embedded PNG.
/// RGBA is premultiplied (the renderer blends premultiplied, like wayland
/// surfaces). None if the PNG can't be decoded.
fn build_hint_icon() -> Option<smithay::backend::renderer::element::memory::MemoryRenderBuffer> {
    use smithay::backend::allocator::Fourcc;
    use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
    use smithay::utils::Transform;
    let (w, h, mut rgba) = crate::toolbar::decode_icon_rgba()?;
    for px in rgba.chunks_exact_mut(4) {
        let a = px[3] as u16;
        px[0] = (px[0] as u16 * a / 255) as u8;
        px[1] = (px[1] as u16 * a / 255) as u8;
        px[2] = (px[2] as u16 * a / 255) as u8;
    }
    // RGBA byte order == DRM Abgr8888. Buffer scale = ICON_BUFFER_SCALE (the
    // 192px art is a 96pt icon), so the element renders at 96 logical points and
    // stays crisp on a HiDPI (scale 2) output.
    Some(MemoryRenderBuffer::from_slice(
        &rgba,
        Fourcc::Abgr8888,
        (w as i32, h as i32),
        crate::toolbar::ICON_BUFFER_SCALE,
        Transform::Normal,
        None,
    ))
}

// -------------------------------------------------------- exit signals ------

/// Write end of a self-pipe the SIGTERM/SIGINT handler pokes. Stored as a raw fd
/// so the async-signal-safe handler can reach it without allocation or locks.
static SIG_WAKE_FD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

/// async-signal-safe: just a single `write()` to the self-pipe, which wakes the
/// event loop so its registered source can stop the loop cleanly.
extern "C" fn on_exit_signal(_sig: libc::c_int) {
    let fd = SIG_WAKE_FD.load(std::sync::atomic::Ordering::Relaxed);
    if fd >= 0 {
        let byte = [1u8];
        unsafe { libc::write(fd, byte.as_ptr().cast(), 1) };
    }
}

/// Route SIGTERM/SIGINT to a clean event-loop stop, so `event_loop.run()`
/// returns and `main` can run the host-clipboard clear-on-exit before quitting
/// (the leader SIGTERMs the compositor on session teardown / suspend-dismount).
/// A self-pipe (written by the handler, read by a calloop source) keeps the
/// signal path async-signal-safe. Best-effort: on failure we simply keep the
/// default disposition and rely on the auto-clear timer as the backstop.
pub fn install_exit_signals(event_loop: &EventLoop<State>) {
    use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return;
    }
    let read_fd = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let write_fd = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    // Nonblocking read end so the loop callback's drain never blocks.
    let rfd = read_fd.as_raw_fd();
    unsafe {
        let flags = libc::fcntl(rfd, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(rfd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
    // The write end must outlive every handler invocation -> leak it (process
    // lifetime); its fd is what the handler writes to.
    SIG_WAKE_FD.store(write_fd.into_raw_fd(), std::sync::atomic::Ordering::Relaxed);
    let handler = on_exit_signal as extern "C" fn(libc::c_int);
    unsafe {
        libc::signal(libc::SIGTERM, handler as libc::sighandler_t);
        libc::signal(libc::SIGINT, handler as libc::sighandler_t);
    }
    let _ = event_loop.handle().insert_source(
        Generic::new(read_fd, Interest::READ, Mode::Level),
        |_readiness, fd, state: &mut State| {
            let mut buf = [0u8; 8];
            let _ = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            state.loop_signal.stop();
            Ok(PostAction::Continue)
        },
    );
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
