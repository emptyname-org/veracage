//! Host<->sandbox clipboard, owned entirely by the compositor.
//!
//! The compositor is BOTH the sandbox compositor (it owns the sandbox selection)
//! AND a client of the host (kwin) via its winit window — so it bridges the two
//! in-process, with no leader relay and no `wl-data-control` global exposed to
//! sandbox apps. A shortcut / toolbar button fired while the compositor window is
//! focused supplies the serial `smithay-clipboard` needs (see input.rs).

use std::io::{Read, Write};
use std::os::unix::io::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use smithay::wayland::selection::data_device::{
    SelectionRequestError, request_data_device_client_selection, set_data_device_selection,
};

use crate::State;

const MIMES: &[&str] = &["text/plain;charset=utf-8", "text/plain", "UTF8_STRING", "STRING", "TEXT"];

/// Cap the host<->sandbox clipboard so a hostile selection owner can't OOM the
/// single-process compositor, and give every transfer a total time budget so a
/// peer that opens the pipe but never drains/fills it can't leak a thread + fd
/// forever (both DoS vectors the sandbox app could otherwise trigger per paste).
const MAX_CLIP_BYTES: usize = 16 * 1024 * 1024;
const CLIP_BUDGET_MS: u128 = 8_000;

/// Cap on concurrent clipboard transfer threads. A hostile sandbox app can loop
/// `wl_data_offer.receive` after a single user paste; without a cap each request
/// would spawn a thread + hold an fd for the whole time budget, exhausting the
/// process — or make `thread::spawn` panic on ENOMEM and crash the compositor
/// (killing every co-hosted vault's apps). Over the cap we drop the request and
/// the app just sees an empty paste.
const MAX_CLIP_THREADS: usize = 16;
static CLIP_THREADS: AtomicUsize = AtomicUsize::new(0);

/// Run `f` on a bounded, non-panicking clipboard worker thread. Returns `false`
/// (dropping `f`, which closes any fd it captured) if we are at the in-flight cap
/// or the OS refuses the thread — so a client can never drive unbounded threads
/// nor trigger a `spawn` panic. The caller must not depend on `f` having run.
pub(crate) fn spawn_clip_worker<F: FnOnce() + Send + 'static>(f: F) -> bool {
    if CLIP_THREADS.fetch_add(1, Ordering::AcqRel) >= MAX_CLIP_THREADS {
        CLIP_THREADS.fetch_sub(1, Ordering::AcqRel);
        return false;
    }
    match std::thread::Builder::new()
        .name("veracage-clip".into())
        .spawn(move || {
            f();
            CLIP_THREADS.fetch_sub(1, Ordering::AcqRel);
        }) {
        Ok(_) => true,
        Err(_) => {
            CLIP_THREADS.fetch_sub(1, Ordering::AcqRel);
            false
        }
    }
}

fn set_nonblocking(fd: i32) {
    unsafe {
        let f = libc::fcntl(fd, libc::F_GETFL);
        if f >= 0 {
            libc::fcntl(fd, libc::F_SETFL, f | libc::O_NONBLOCK);
        }
    }
}

fn poll_ready(fd: i32, events: i16, timeout_ms: i32) -> bool {
    let mut pfd = libc::pollfd { fd, events, revents: 0 };
    unsafe { libc::poll(&mut pfd, 1, timeout_ms) > 0 && (pfd.revents & events) != 0 }
}

/// Drain a selection pipe into a size-capped, time-budgeted buffer.
fn drain_bounded(reader: std::io::PipeReader) -> Vec<u8> {
    let fd = reader.as_raw_fd();
    set_nonblocking(fd);
    let mut reader = reader;
    let mut out = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    let start = Instant::now();
    while out.len() < MAX_CLIP_BYTES && start.elapsed().as_millis() < CLIP_BUDGET_MS {
        if !poll_ready(fd, libc::POLLIN, 500) {
            continue; // no data yet — loop re-checks the total budget
        }
        match reader.read(&mut buf) {
            Ok(0) => break, // EOF
            Ok(n) => {
                let take = n.min(MAX_CLIP_BYTES - out.len());
                out.extend_from_slice(&buf[..take]);
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(_) => break,
        }
    }
    out
}

/// Serve `data` to an app that requested the selection, size-capped + budgeted so
/// an app that never reads the pipe can't leak this thread forever.
pub(crate) fn write_all_bounded(fd: OwnedFd, data: &[u8]) {
    let raw = fd.as_raw_fd();
    set_nonblocking(raw);
    let mut file = std::fs::File::from(fd);
    let data = &data[..data.len().min(MAX_CLIP_BYTES)];
    let mut off = 0;
    let start = Instant::now();
    while off < data.len() && start.elapsed().as_millis() < CLIP_BUDGET_MS {
        if !poll_ready(raw, libc::POLLOUT, 500) {
            continue;
        }
        match file.write(&data[off..]) {
            Ok(0) => break,
            Ok(n) => off += n,
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(_) => break,
        }
    }
}

fn mimes() -> Vec<String> {
    MIMES.iter().map(|s| s.to_string()).collect()
}

// `smithay_clipboard::Clipboard` is Send but not Sync (internal mpsc::Receiver),
// so a `Mutex` is needed to share the handle with the pull drain thread.
type SharedClip = std::sync::Arc<std::sync::Mutex<smithay_clipboard::Clipboard>>;

/// The host clipboard, reached through the compositor's OWN winit->host
/// wl_display connection.
pub struct HostClipboard {
    inner: SharedClip,
}

impl HostClipboard {
    /// SAFETY: `display` must be the winit backend's valid wl_display, which
    /// outlives the compositor.
    pub unsafe fn from_display_ptr(display: *mut std::ffi::c_void) -> Self {
        HostClipboard {
            inner: std::sync::Arc::new(std::sync::Mutex::new(unsafe {
                smithay_clipboard::Clipboard::new(display)
            })),
        }
    }
    fn load(&self) -> Option<String> {
        self.inner.lock().ok()?.load().ok()
    }
    fn handle(&self) -> SharedClip {
        self.inner.clone()
    }
}

fn store_host(clip: &SharedClip, text: String) {
    if let Ok(c) = clip.lock() {
        c.store(text);
    }
}

/// Push host clipboard -> sandbox selection (Ctrl+Alt+V, and the toolbar).
pub fn push_from_host(state: &mut State) {
    let Some(host) = &state.host_clipboard else {
        return;
    };
    let Some(text) = host.load() else {
        return;
    };
    // Arc so `send_selection` shares the buffer with a cheap refcount bump per
    // paste request instead of cloning up to MAX_CLIP_BYTES on the event loop.
    state.clip_source = Some(Arc::new(text.into_bytes()));
    set_data_device_selection(&state.display_handle, &state.seat, mimes(), ());
}

/// Pull sandbox selection -> host clipboard (Ctrl+Alt+C, and the toolbar).
/// Three cases: we hold the selection, an app holds it, or nothing — the last two
/// used to return empty, which silently WIPED the host clipboard.
pub fn pull_to_host(state: &mut State) {
    let Some(host) = state.host_clipboard.as_ref().map(HostClipboard::handle) else {
        return;
    };
    for &mime in MIMES {
        let (reader, writer) = match std::io::pipe() {
            Ok(p) => p,
            Err(_) => return,
        };
        let write_fd: OwnedFd = writer.into();
        match request_data_device_client_selection(&state.seat, mime.to_string(), write_fd) {
            Ok(()) => {
                // An app owns the selection; it writes to the pipe on the main
                // loop, so drain on a short (bounded, non-panicking) thread and
                // store to the host. If we're at the worker cap the reader drops
                // here (pipe closes) and the pull is simply a no-op.
                let h = host.clone();
                spawn_clip_worker(move || {
                    if let Ok(s) = String::from_utf8(drain_bounded(reader)) {
                        store_host(&h, s);
                    }
                });
                return;
            }
            // We are the selection source — serve the pushed bytes directly.
            Err(SelectionRequestError::ServerSideSelection) => {
                if let Some(bytes) = state.clip_source.as_ref() {
                    if let Ok(s) = String::from_utf8(bytes.as_ref().clone()) {
                        store_host(&host, s);
                    }
                }
                return;
            }
            // This MIME isn't offered — try the next.
            Err(SelectionRequestError::InvalidMimetype) => continue,
            // Genuinely no active selection.
            Err(SelectionRequestError::NoSelection) => return,
        }
    }
}
