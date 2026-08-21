//! Host<->sandbox clipboard, owned entirely by the compositor.
//!
//! The compositor is BOTH the sandbox compositor (it owns the sandbox selection)
//! AND a client of the host (kwin) via its winit window, so it bridges the two
//! in-process, with no leader relay and no `wl-data-control` global exposed to
//! sandbox apps. The HOST side (reading/writing the user's real clipboard) uses
//! wlr-data-control (serial-free) via `hostclip`, a trusted client of the outer
//! desktop; the sandbox side is a separate connection that never sees it.

use std::os::unix::io::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use smithay::wayland::selection::data_device::{
    SelectionRequestError, request_data_device_client_selection, set_data_device_selection,
};

use crate::clipio;
use crate::State;

const MIMES: &[&str] = &["text/plain;charset=utf-8", "text/plain", "UTF8_STRING", "STRING", "TEXT"];

/// Total time budget for one host<->sandbox transfer (see `clipio` for why a
/// budget is needed). Generous, since this side faces the local sandbox apps.
const CLIP_BUDGET_MS: u128 = 8_000;

/// Cap on concurrent clipboard transfer threads. A hostile sandbox app can loop
/// `wl_data_offer.receive` after a single user paste; without a cap each request
/// would spawn a thread + hold an fd for the whole time budget, exhausting the
/// process, or make `thread::spawn` panic on ENOMEM and crash the compositor
/// (killing every co-hosted vault's apps). Over the cap we drop the request and
/// the app just sees an empty paste.
const MAX_CLIP_THREADS: usize = 16;
static CLIP_THREADS: AtomicUsize = AtomicUsize::new(0);

/// Run `f` on a bounded, non-panicking clipboard worker thread. Returns `false`
/// (dropping `f`, which closes any fd it captured) if we are at the in-flight cap
/// or the OS refuses the thread, so a client can never drive unbounded threads
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

/// Drain a selection pipe into a size-capped, time-budgeted buffer.
fn drain_bounded(mut reader: std::io::PipeReader) -> Vec<u8> {
    clipio::read_bounded(reader.as_raw_fd(), &mut reader, CLIP_BUDGET_MS, 500)
}

/// Serve `data` to an app that requested the selection, size-capped + budgeted so
/// an app that never reads the pipe can't leak this thread forever.
pub(crate) fn write_all_bounded(fd: OwnedFd, data: &[u8]) {
    clipio::write_bounded(fd, data, CLIP_BUDGET_MS, 500)
}

fn mimes() -> Vec<String> {
    MIMES.iter().map(|s| s.to_string()).collect()
}

/// Push host clipboard -> sandbox selection (Paste in). Reads the host clipboard
/// via data-control (no serial/focus needed) and makes the compositor the sandbox
/// selection source.
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

/// Drop the sandbox selection we are serving (Paste in's text) and stop offering
/// it. Called when the last volume closes: the compositor outlives every volume,
/// so without this a password pasted into one volume is still handed, byte for
/// byte, to the apps of a volume opened hours later. The host-side copy has its
/// own auto-clear; this is the sandbox side of the same rule.
pub fn clear_sandbox_selection(state: &mut State) {
    if state.clip_source.take().is_some() {
        set_data_device_selection(&state.display_handle, &state.seat, Vec::new(), ());
    }
}

/// Pull sandbox selection -> host clipboard (Copy out). Reads the current sandbox
/// selection (an app's, or our own pushed one) and stores it to the host via
/// data-control, which needs no focus serial.
pub fn pull_to_host(state: &mut State) {
    let Some(host) = state.host_clipboard.clone() else {
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
                        h.store(s);
                    }
                });
                return;
            }
            // We are the selection source: serve the pushed bytes directly.
            Err(SelectionRequestError::ServerSideSelection) => {
                if let Some(bytes) = state.clip_source.as_ref() {
                    if let Ok(s) = String::from_utf8(bytes.as_ref().clone()) {
                        host.store(s);
                    }
                }
                return;
            }
            // This MIME isn't offered: try the next.
            Err(SelectionRequestError::InvalidMimetype) => continue,
            // Genuinely no active selection.
            Err(SelectionRequestError::NoSelection) => return,
        }
    }
}
