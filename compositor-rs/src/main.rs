//! Veracage nested compositor, fixed-function. Seeded from smithay's `smallvil`
//! reference; grown with a leader-only clipboard channel. Renders the sandbox
//! apps into one window on the host (nested via the inherited WAYLAND_SOCKET fd),
//! exposes NO `data-control` global (the host clipboard bridge is a private
//! socket only the leader holds).
#![allow(irrefutable_let_patterns)]

mod clipboard;
mod clipio;
mod fonts;
mod grabs;
mod hostclip;
mod handlers;
mod input;
mod mono_icons;
mod shortcuts;
mod state;
mod toolbar;
mod winit;

use smithay::reexports::{calloop::EventLoop, wayland_server::Display};
pub use state::State;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_logging();

    // Never dump core: the compositor holds decrypted on-screen content and
    // clipboard secrets, so a core would write that plaintext to a host-readable
    // file (/var/lib/systemd/coredump). Suppress it before anything sensitive.
    unsafe {
        let rl = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        libc::setrlimit(libc::RLIMIT_CORE, &rl);
    }

    // --socket <name>  the wayland socket name apps connect to (the leader picks
    //                  it, like `weston --socket=`, so it knows it). The clipboard
    //                  is owned in-process now (clipboard.rs), no clip socket.
    let mut socket: Option<String> = None;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--socket" => socket = it.next(),
            other => eprintln!("veracage-compositor: ignoring unknown arg {other}"),
        }
    }

    let mut event_loop: EventLoop<State> = EventLoop::try_new()?;
    let display: Display<State> = Display::new()?;
    let mut state = State::new(&mut event_loop, display, socket);

    // Open our nested output window on the host (via the inherited WAYLAND_SOCKET).
    // The host clipboard bridge is set up inside init_winit (needs the backend).
    crate::winit::init_winit(&mut event_loop, &mut state)?;

    // Route SIGTERM/SIGINT to a clean loop stop so the clear-on-exit below runs
    // when the leader tears the session down, not only on menu Quit / window close.
    crate::state::install_exit_signals(&event_loop);

    let run_result = event_loop.run(None, &mut state, move |_| {});

    // Clear any sensitive text still on the host clipboard (KeePassXC-style
    // clear-on-quit; no-op if nothing was pushed), then STOP its worker BEFORE the
    // winit backend - which owns the wl_display the worker borrows - is dropped.
    // clear_on_exit makes the worker return; joining it here, while the backend is
    // still alive, prevents the teardown use-after-free that otherwise SIGSEGVs
    // the compositor on shutdown.
    if let Some(hc) = &state.host_clipboard {
        hc.clear_on_exit();
    }
    if let Some(worker) = state.host_clipboard_worker.take() {
        let _ = worker.join();
    }
    run_result?;
    Ok(())
}

/// Append a debug line to `/run/veracage/rt/compositor.log` (0644, so the human
/// uid can read it through the 0711 rt dir). The compositor's stdio is swallowed
/// by pkexec/privilege-drop, so the journal never sees its tracing - this is the
/// reliable channel for live debugging. Kept for ad-hoc instrumentation.
#[allow(dead_code)]
pub fn vcdebug(msg: &str) {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let path = "/run/veracage/rt/compositor.log";
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644));
        let _ = writeln!(f, "{msg}");
    }
}

fn init_logging() {
    if let Ok(env_filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    } else {
        tracing_subscriber::fmt().init();
    }
}
