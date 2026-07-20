//! Low-level clipboard I/O primitives shared by the sandbox-side bridge
//! (`clipboard.rs`) and the host-side data-control client (`hostclip.rs`): a
//! non-blocking fd helper, a poll that treats HUP/ERR as ready, and bounded
//! (size-capped + time-budgeted, non-blocking) read/write loops. Both sides cap
//! a single transfer at `MAX_CLIP_BYTES` so a hostile peer that opens a pipe but
//! never drains/fills it can't leak a thread + fd or OOM the single-process
//! compositor.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::time::Instant;

pub const MAX_CLIP_BYTES: usize = 16 * 1024 * 1024;

pub fn set_nonblocking(fd: i32) {
    unsafe {
        let f = libc::fcntl(fd, libc::F_GETFL);
        if f >= 0 {
            libc::fcntl(fd, libc::F_SETFL, f | libc::O_NONBLOCK);
        }
    }
}

/// True when the fd is ready for `events`. POLLHUP/POLLERR count as ready too:
/// a drained pipe whose writer closed reports only POLLHUP (no POLLIN), so the
/// read/write call must run to observe the EOF/EPIPE. Gating on `events` alone
/// would spin for the whole budget after the peer already finished.
pub fn poll_ready(fd: i32, events: i16, timeout_ms: i32) -> bool {
    let ready = events | libc::POLLHUP | libc::POLLERR;
    let mut pfd = libc::pollfd { fd, events, revents: 0 };
    unsafe { libc::poll(&mut pfd, 1, timeout_ms) > 0 && (pfd.revents & ready) != 0 }
}

/// Drain `reader` (its fd is `raw`) into a size-capped, time-budgeted buffer,
/// non-blocking. `poll_ms` is the per-iteration poll timeout.
pub fn read_bounded(raw: i32, reader: &mut impl Read, budget_ms: u128, poll_ms: i32) -> Vec<u8> {
    set_nonblocking(raw);
    let mut out = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    let start = Instant::now();
    while out.len() < MAX_CLIP_BYTES && start.elapsed().as_millis() < budget_ms {
        if !poll_ready(raw, libc::POLLIN, poll_ms) {
            continue; // no data yet, loop re-checks the total budget
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

/// Serve `data` to a peer that opened `fd`, size-capped + time-budgeted +
/// non-blocking, so a peer that never reads can't hold this thread forever.
pub fn write_bounded(fd: OwnedFd, data: &[u8], budget_ms: u128, poll_ms: i32) {
    let raw = fd.as_raw_fd();
    set_nonblocking(raw);
    let mut file = std::fs::File::from(fd);
    let data = &data[..data.len().min(MAX_CLIP_BYTES)];
    let mut off = 0;
    let start = Instant::now();
    while off < data.len() && start.elapsed().as_millis() < budget_ms {
        if !poll_ready(raw, libc::POLLOUT, poll_ms) {
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
