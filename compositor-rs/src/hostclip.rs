//! Host clipboard via the wlr-data-control protocol, on the compositor's own
//! winit->host wl_display connection.
//!
//! Replaces smithay-clipboard: setting the host selection through
//! `wl_data_device` needs the serial of a recent input event while the surface
//! has focus, and in this nested setup that serial isn't reliably available, so
//! Copy out silently failed. `zwlr_data_control` is the clipboard-manager
//! protocol - it sets/reads the selection with NO serial and NO focus, which is
//! exactly what a compositor pushing its sandbox's selection out needs. KWin and
//! wlroots implement it.
//!
//! A dedicated worker thread runs a minimal wayland-client loop on the shared
//! connection (the same pattern smithay-clipboard used), woken for store/load
//! requests via a self-pipe.

use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::io::FromRawFd;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle,
    backend::Backend,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_registry::WlRegistry, wl_seat::WlSeat},
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};

/// MIME types we offer/accept, best first.
const MIMES: &[&str] = &[
    "text/plain;charset=utf-8",
    "text/plain;charset=UTF-8",
    "UTF8_STRING",
    "text/plain",
    "STRING",
    "TEXT",
];
use crate::clipio;
/// Time budget for reading the host selection on the worker, kept just under
/// `load()`'s wait so the worker replies before the caller gives up.
const READ_BUDGET_MS: u128 = 900;
/// Time budget for serving our bytes to a host paster (a detached thread, off
/// the main loop), so a host peer that never drains can't hold a thread forever.
const CLIP_BUDGET_MS: u128 = 2_000;
/// Cap on concurrent serve threads (a hostile host paster could open many
/// receive fds and never drain them); over the cap we drop the request.
const MAX_SERVE_THREADS: usize = 16;
static SERVE_THREADS: AtomicUsize = AtomicUsize::new(0);
/// Cap on MIME types accumulated for one offer, so a hostile host compositor
/// can't OOM the worker by streaming unbounded Offer events.
const MAX_OFFER_MIMES: usize = 64;

/// Secure default host-clipboard auto-clear policy (enabled, timeout secs),
/// applied before the first published policy arrives. Mirrors the config
/// defaults. Also the initial value of `State::clip_clear_applied`.
pub const DEFAULT_CLEAR_POLICY: (bool, u32) = (true, 30);

/// Bounded wait for the clear-on-exit handshake, so a wedged host can't hang
/// process shutdown. The worker acks after it has flushed the clear to the host.
const EXIT_CLEAR_WAIT_MS: u64 = 1_500;

enum Cmd {
    Store(Vec<u8>),
    Load(Sender<Option<String>>),
    /// Update the auto-clear policy (enabled, timeout secs).
    Policy { enabled: bool, secs: u32 },
    /// Clear now (process exit) and ack when the clear has been flushed.
    ClearOnExit(Sender<()>),
}

/// Handle to the host-clipboard worker. Cheap to clone (Arc'd sender + wake fd).
#[derive(Clone)]
pub struct HostClipboard {
    tx: Sender<Cmd>,
    wake: Arc<OwnedFd>,
}

impl HostClipboard {
    /// SAFETY: `display` must be the winit backend's valid wl_display (compositor
    /// lifetime). Returns None if the worker or the data-control global is absent.
    pub unsafe fn from_display_ptr(display: *mut std::ffi::c_void) -> Option<Self> {
        let backend = unsafe { Backend::from_foreign_display(display.cast()) };
        let conn = Connection::from_backend(backend);
        let (wake_r, wake_w) = pipe()?;
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("veracage-hostclip".into())
            .spawn(move || worker(conn, rx, wake_r))
            .ok()?;
        Some(HostClipboard { tx, wake: Arc::new(wake_w) })
    }

    /// Set the host clipboard to `text` (sandbox -> host). This is the ONLY path
    /// that pushes sensitive sandbox text to the host, so it is also where the
    /// auto-clear countdown is armed (in the worker). Copying inside the sandbox
    /// never reaches here.
    pub fn store(&self, text: String) {
        if self.tx.send(Cmd::Store(text.into_bytes())).is_ok() {
            self.poke();
        }
    }

    /// Update the auto-clear policy live (from the compositor's ~1s scan of the
    /// published `clipclear` file). The worker keeps its secure default until
    /// the first policy arrives.
    pub fn set_policy(&self, enabled: bool, secs: u32) {
        if self.tx.send(Cmd::Policy { enabled, secs }).is_ok() {
            self.poke();
        }
    }

    /// Clear the host clipboard on process exit, if sensitive text was pushed and
    /// not yet cleared. Blocks (bounded) until the worker has flushed the clear,
    /// so the selection is gone before we exit. No-op if the worker is dead.
    pub fn clear_on_exit(&self) {
        let (tx, rx) = mpsc::channel();
        if self.tx.send(Cmd::ClearOnExit(tx)).is_err() {
            return;
        }
        self.poke();
        let _ = rx.recv_timeout(Duration::from_millis(EXIT_CLEAR_WAIT_MS));
    }

    /// Read the host clipboard (host -> sandbox). Called on the compositor's
    /// single-threaded loop (Paste in), so the wait is bounded tightly: a
    /// stalled/hostile host selection owner must not freeze the whole UI. Text
    /// pastes complete in milliseconds; a slower one is simply dropped rather
    /// than stalling every co-hosted app.
    pub fn load(&self) -> Option<String> {
        let (rtx, rrx) = mpsc::channel();
        self.tx.send(Cmd::Load(rtx)).ok()?;
        self.poke();
        rrx.recv_timeout(Duration::from_millis(1000)).ok().flatten()
    }

    fn poke(&self) {
        let _ = unsafe { libc::write(self.wake.as_raw_fd(), [1u8].as_ptr().cast(), 1) };
    }
}

// ------------------------------------------------------------- worker --------

struct State {
    qh: QueueHandle<State>,
    manager: ZwlrDataControlManagerV1,
    device: ZwlrDataControlDeviceV1,
    /// The selection source we currently own + the bytes it serves.
    source: Option<ZwlrDataControlSourceV1>,
    serve: Arc<Vec<u8>>,
    /// The incoming (host-owned) CLIPBOARD selection offer + its MIME types.
    offer: Option<ZwlrDataControlOfferV1>,
    offer_mimes: Vec<String>,
    /// The incoming (host-owned) PRIMARY selection offer + its MIME types
    /// (wlr-data-control v2 only). Read back at clear time to decide whether the
    /// primary selection still holds our pushed value.
    primary_offer: Option<ZwlrDataControlOfferV1>,
    primary_offer_mimes: Vec<String>,
    /// MIME types accumulating on the most recently announced offer, before it
    /// is promoted to the current (clipboard or primary) selection.
    building: Vec<String>,
    /// The negotiated device supports the primary selection (v2+).
    has_primary: bool,
    // ---- auto-clear (KeePassXC-style) ------------------------------------
    /// Whether to arm the auto-clear countdown on a Copy out.
    clear_enabled: bool,
    /// Countdown length in seconds.
    clear_secs: u32,
    /// The exact plaintext last pushed sandbox -> host. `Some` means sensitive
    /// text was pushed and not yet cleared/discarded (drives clear-on-exit).
    /// Shares the buffer with `serve` while we own the clipboard.
    pushed: Option<Arc<Vec<u8>>>,
    /// Our source still owns the host CLIPBOARD selection: nothing has replaced
    /// it (a replacement Cancels our source). While true, the clipboard still
    /// holds exactly `pushed`, so a clear may safely unset it.
    owns_clip: bool,
    /// When the auto-clear fires; `None` = no countdown armed.
    deadline: Option<Instant>,
}

fn worker(conn: Connection, rx: Receiver<Cmd>, wake_r: OwnedFd) {
    let Ok((globals, mut queue)) = registry_queue_init::<State>(&conn) else {
        return;
    };
    let qh = queue.handle();
    let Ok(seat) = globals.bind::<WlSeat, _, _>(&qh, 1..=1, ()) else {
        return;
    };
    let Ok(manager) = globals.bind::<ZwlrDataControlManagerV1, _, _>(&qh, 1..=2, ()) else {
        return; // host has no wlr-data-control
    };
    let device = manager.get_data_device(&seat, &qh, ());
    let has_primary = device.version() >= 2; // primary selection is v2+
    let (clear_enabled, clear_secs) = DEFAULT_CLEAR_POLICY;
    let mut state = State {
        qh: qh.clone(),
        manager,
        device,
        source: None,
        serve: Arc::new(Vec::new()),
        offer: None,
        offer_mimes: Vec::new(),
        primary_offer: None,
        primary_offer_mimes: Vec::new(),
        building: Vec::new(),
        has_primary,
        clear_enabled,
        clear_secs,
        pushed: None,
        owns_clip: false,
        deadline: None,
    };
    // Prime the current selection offer.
    let _ = queue.roundtrip(&mut state);

    loop {
        if conn.flush().is_err() {
            return;
        }
        if queue.dispatch_pending(&mut state).is_err() {
            return;
        }
        // Handle any queued commands.
        while let Ok(cmd) = rx.try_recv() {
            handle_cmd(&mut state, &conn, &mut queue, cmd);
        }
        // Fire the auto-clear when its countdown elapses.
        if state.deadline.is_some_and(|d| Instant::now() >= d) {
            clear_pushed(&mut state, &conn, &mut queue);
        }
        if conn.flush().is_err() {
            return;
        }
        // Block until the wayland fd or the wake pipe is readable.
        let Some(guard) = conn.prepare_read() else {
            continue; // events already queued; loop to dispatch them
        };
        let cfd = guard.connection_fd().as_raw_fd();
        let wfd = wake_r.as_raw_fd();
        let mut pfds = [
            libc::pollfd { fd: cfd, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: wfd, events: libc::POLLIN, revents: 0 },
        ];
        // Block indefinitely unless a countdown is armed; then wake at most once
        // per second (the spec's per-second tick) and never sleep past the
        // deadline, so the clear fires on time.
        let timeout = match state.deadline {
            None => -1,
            Some(d) => {
                let now = Instant::now();
                if now >= d {
                    0
                } else {
                    d.saturating_duration_since(now).as_millis().min(1000) as libc::c_int
                }
            }
        };
        let n = unsafe { libc::poll(pfds.as_mut_ptr(), 2, timeout) };
        if n < 0 {
            drop(guard);
            continue;
        }
        if pfds[0].revents & libc::POLLIN != 0 {
            let _ = guard.read();
        } else {
            drop(guard);
        }
        if pfds[1].revents & libc::POLLIN != 0 {
            let mut buf = [0u8; 64];
            let _ = unsafe { libc::read(wfd, buf.as_mut_ptr().cast(), buf.len()) };
        }
    }
}

fn handle_cmd(state: &mut State, conn: &Connection, queue: &mut wayland_client::EventQueue<State>, cmd: Cmd) {
    match cmd {
        Cmd::Store(bytes) => {
            let arc = Arc::new(bytes);
            state.serve = arc.clone();
            let source = state.manager.create_data_source(&state.qh, ());
            for m in MIMES {
                source.offer((*m).to_string());
            }
            state.device.set_selection(Some(&source));
            if let Some(old) = state.source.replace(source) {
                old.destroy();
            }
            // Record the pushed value and arm the auto-clear countdown (only if
            // enabled). `pushed` also gates clear-on-exit, which fires regardless
            // of the enable flag once anything has been pushed.
            state.pushed = Some(arc);
            state.owns_clip = true;
            state.deadline = state
                .clear_enabled
                .then(|| Instant::now() + Duration::from_secs(state.clear_secs as u64));
        }
        Cmd::Load(reply) => {
            let text = read_selection(state, conn, queue, false);
            let _ = reply.send(text);
        }
        Cmd::Policy { enabled, secs } => {
            state.clear_enabled = enabled;
            state.clear_secs = secs;
            if !enabled {
                // Turning clearing off cancels a pending countdown, but keeps
                // `pushed` so clear-on-exit still fires.
                state.deadline = None;
            } else if state.deadline.is_none() && state.owns_clip && state.pushed.is_some() {
                // Turned on while our pushed value is still on the clipboard: arm
                // a fresh countdown from now.
                state.deadline = Some(Instant::now() + Duration::from_secs(secs as u64));
            }
        }
        Cmd::ClearOnExit(ack) => {
            clear_pushed(state, conn, queue);
            let _ = ack.send(());
        }
    }
}

/// Ask the host to send the current CLIPBOARD (`primary` = false) or PRIMARY
/// (`primary` = true) selection and read it (bounded, blocking).
fn read_selection(
    state: &mut State,
    conn: &Connection,
    queue: &mut wayland_client::EventQueue<State>,
    primary: bool,
) -> Option<String> {
    // Clone the offer + its MIME list out first so `state` is free for the
    // roundtrip below (which needs &mut State).
    let (offer, mimes) = if primary {
        (state.primary_offer.clone()?, state.primary_offer_mimes.clone())
    } else {
        (state.offer.clone()?, state.offer_mimes.clone())
    };
    let mime = MIMES.iter().find(|m| mimes.iter().any(|o| o == *m))?;
    let (r, w) = pipe()?;
    offer.receive((*mime).to_string(), w.as_fd());
    // Flush the receive request and let the host act on it; then drop our write
    // end so the read sees EOF when the host is done.
    let _ = conn.flush();
    drop(w);
    // Pump the queue briefly so the request is sent, then read the pipe.
    let _ = queue.roundtrip(state);
    // `r` is an OwnedFd; read it directly (non-blocking + time-budgeted, so a
    // host owner that opens the pipe but never writes/closes can't block the
    // worker for the session). The budget is kept under load()'s wait.
    let raw = r.as_raw_fd();
    let mut file = std::fs::File::from(r);
    let buf = clipio::read_bounded(raw, &mut file, READ_BUDGET_MS, 100);
    String::from_utf8(buf).ok()
}

/// Which selections a clear should touch, given the tracked state. Pure, so the
/// clear policy is unit-testable without a live Wayland host.
///
/// - CLIPBOARD: cleared iff our source still owns it (`owns_clip`). Any host
///   replacement Cancels our source, so ownership == "still holds our value";
///   this preserves newer host content without a fragile read-back-and-compare.
/// - PRIMARY: we never own it, so cleared only when it currently reads back as
///   exactly the pushed value (and the device supports primary at all).
fn plan_clear(
    owns_clip: bool,
    has_primary: bool,
    pushed: &[u8],
    current_primary: Option<&[u8]>,
) -> (bool, bool) {
    let clipboard = owns_clip;
    let primary = has_primary && current_primary == Some(pushed);
    (clipboard, primary)
}

/// Clear the host clipboard (and primary selection) of the pushed value, then
/// discard it. No-op if nothing was pushed. Runs on the countdown and at exit.
fn clear_pushed(state: &mut State, conn: &Connection, queue: &mut wayland_client::EventQueue<State>) {
    state.deadline = None;
    let Some(pushed) = state.pushed.take() else {
        return;
    };
    // Read the current primary selection (if the device supports it) so the plan
    // can decide whether it still holds our value.
    let current_primary = if state.has_primary {
        read_selection(state, conn, queue, true)
    } else {
        None
    };
    let (clear_clip, clear_primary) = plan_clear(
        state.owns_clip,
        state.has_primary,
        pushed.as_slice(),
        current_primary.as_deref().map(str::as_bytes),
    );

    if clear_clip {
        state.device.set_selection(None);
        if let Some(src) = state.source.take() {
            src.destroy();
        }
        state.owns_clip = false;
    }
    if clear_primary {
        state.device.set_primary_selection(None);
    }
    // Flush so the host applies the clear before we (possibly) exit.
    let _ = conn.flush();
    let _ = queue.roundtrip(state);
}

// ------------------------------------------------------------ dispatch -------

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &WlRegistry, _: <WlRegistry as wayland_client::Proxy>::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<WlSeat, ()> for State {
    fn event(_: &mut Self, _: &WlSeat, _: <WlSeat as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<ZwlrDataControlManagerV1, ()> for State {
    fn event(_: &mut Self, _: &ZwlrDataControlManagerV1, _: <ZwlrDataControlManagerV1 as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_data_control_device_v1::Event;
        match event {
            // A new offer is announced; its Offer(mime) events follow, then a
            // Selection / PrimarySelection promotes it. Reset the MIME accumulator.
            Event::DataOffer { .. } => state.building.clear(),
            Event::Selection { id } => match id {
                Some(offer) => {
                    if let Some(old) = state.offer.replace(offer) {
                        old.destroy();
                    }
                    state.offer_mimes = std::mem::take(&mut state.building);
                }
                None => {
                    if let Some(old) = state.offer.take() {
                        old.destroy();
                    }
                    state.offer_mimes.clear();
                }
            },
            // Primary selection (wlr-data-control v2). Tracked so a clear can read
            // it back and unset it only when it still holds our pushed value.
            Event::PrimarySelection { id } => match id {
                Some(offer) => {
                    if let Some(old) = state.primary_offer.replace(offer) {
                        old.destroy();
                    }
                    state.primary_offer_mimes = std::mem::take(&mut state.building);
                }
                None => {
                    if let Some(old) = state.primary_offer.take() {
                        old.destroy();
                    }
                    state.primary_offer_mimes.clear();
                }
            },
            _ => {}
        }
    }

    // Data-control offers are created by the server; declare their user-data type.
    wayland_client::event_created_child!(State, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
            // Cap the accumulation: a hostile host compositor could otherwise
            // stream unbounded Offer events to OOM the worker.
            if state.building.len() < MAX_OFFER_MIMES {
                state.building.push(mime_type);
            }
        }
    }
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for State {
    fn event(
        state: &mut Self,
        source: &ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_data_control_source_v1::Event;
        match event {
            // A host app is pasting: write our bytes to the fd, on a short
            // detached thread. Bounded (non-blocking + time budget) and
            // concurrency-capped so a host paster that never drains the pipe
            // can't leak threads/fds or stall the worker forever.
            Event::Send { fd, .. } => {
                if SERVE_THREADS.fetch_add(1, Ordering::AcqRel) >= MAX_SERVE_THREADS {
                    SERVE_THREADS.fetch_sub(1, Ordering::AcqRel);
                    return; // over the cap: drop fd (closes the pipe), no serve
                }
                let data = state.serve.clone();
                if std::thread::Builder::new()
                    .spawn(move || {
                        crate::clipio::write_bounded(fd, &data, CLIP_BUDGET_MS, 200);
                        SERVE_THREADS.fetch_sub(1, Ordering::AcqRel);
                    })
                    .is_err()
                {
                    SERVE_THREADS.fetch_sub(1, Ordering::AcqRel);
                }
            }
            Event::Cancelled => {
                source.destroy();
                // Only our CURRENT source being cancelled means the host replaced
                // our clipboard selection: we no longer own it, so a later clear
                // must not touch it (it now holds newer host content). A cancel
                // for a source we already replaced ourselves is ignored.
                if state.source.as_ref() == Some(source) {
                    state.source = None;
                    state.owns_clip = false;
                }
            }
            _ => {}
        }
    }
}

// --------------------------------------------------------------- helpers -----

/// A close-on-exec pipe as (read, write) owned fds.
fn pipe() -> Option<(OwnedFd, OwnedFd)> {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return None;
    }
    Some(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

#[cfg(test)]
mod tests {
    use super::plan_clear;

    const SECRET: &[u8] = b"s3cr3t";

    #[test]
    fn clipboard_cleared_only_while_owned() {
        // We still own it -> current clipboard is still our value -> clear it.
        assert_eq!(plan_clear(true, false, SECRET, None).0, true);
        // Replaced by the host (our source was cancelled) -> preserve it.
        assert_eq!(plan_clear(false, false, SECRET, None).0, false);
    }

    #[test]
    fn primary_cleared_only_on_exact_match() {
        // Matches the pushed value -> clear it.
        assert_eq!(plan_clear(false, true, SECRET, Some(SECRET)).1, true);
        // Host replaced it with newer text -> preserve it.
        assert_eq!(plan_clear(false, true, SECRET, Some(b"other")).1, false);
        // No primary offer to read -> nothing to clear.
        assert_eq!(plan_clear(false, true, SECRET, None).1, false);
    }

    #[test]
    fn primary_untouched_without_v2_support() {
        // Even a byte-identical primary is left alone when the device is v1.
        assert_eq!(plan_clear(true, false, SECRET, Some(SECRET)).1, false);
    }
}

