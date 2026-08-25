//! Veracage nested compositor, fixed-function. Seeded from smithay's `smallvil`
//! reference; grown with a leader-only clipboard channel. Renders the sandbox
//! apps into one window on the host (nested via the inherited WAYLAND_SOCKET fd),
//! exposes NO `data-control` global (the host clipboard bridge is a private
//! socket only the leader holds).
// `collapsible_if` is allowed crate-wide: this code nests `if let Some(x) = scan()`
// around `if x != current` on purpose, so the comment above each outer test still
// reads as a comment about that test. Collapsing them into let-chains buys nothing
// and costs the explanation. Every other clippy lint is enforced (CI runs
// `--all-targets -D warnings`), including the ones that found real defects here:
// a doc comment left behind by a deleted function, and an `#[allow]` that had
// drifted off the function it was written for.
#![allow(clippy::collapsible_if)]
#![allow(irrefutable_let_patterns)]

mod clipboard;
mod clipio;
mod fonts;
mod grabs;
mod hostclip;
mod handlers;
mod hint_text;
mod input;
mod mono_icons;
mod shadow;
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
    // The helper already clears `coredump_filter` for everything it execs (see
    // its `suppress_core_dumps`); repeated here because the compositor also runs
    // directly, from the tests and the headless smoke. The limit alone is not
    // enough: the kernel ignores it when `core_pattern` is a pipe, which is the
    // systemd default, so the filter is what actually empties the dump.
    unsafe {
        let rl = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        libc::setrlimit(libc::RLIMIT_CORE, &rl);
    }
    let _ = std::fs::write("/proc/self/coredump_filter", "0\n");

    // --socket <name>  the wayland socket name apps connect to (the leader picks
    //                  it, like `weston --socket=`, so it knows it). The clipboard
    //                  is owned in-process, see clipboard.rs.
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

    // The command log is append-only and the broker cannot unlink in our runtime
    // dir, so we start it empty: otherwise it carries every verb of every earlier
    // session forever.
    crate::toolbar::reset_command_log();

    // Open our nested output window on the host (via the inherited WAYLAND_SOCKET).
    // The host clipboard bridge is set up inside init_winit (needs the backend).
    crate::winit::init_winit(&mut event_loop, &mut state)?;

    // Route SIGTERM/SIGINT to a clean loop stop so the clear-on-exit below runs
    // when the leader tears the session down, not only on menu Quit / window close.
    crate::state::install_exit_signals(&event_loop);

    // Flush queued events to the clients after EVERY batch of loop events, as
    // smallvil and anvil do. It must not be tied to rendering: an app that connects
    // while nothing is dirty would then wait for its globals until something else
    // happened to trigger a frame, which is a launch that hangs for no reason.
    let run_result = event_loop.run(None, &mut state, move |state| {
        state.space.refresh();
        state.popups.cleanup();
        let _ = state.display_handle.flush_clients();
    });

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
        // Join ONLY once the worker has actually finished. Its host roundtrips
        // are unbounded, so a wedged host compositor could otherwise leave it
        // blocked and this join would hang the process forever, keeping the
        // volume mounted (the leader waits on our exit) with the dm-crypt key
        // still in RAM. If it has not stopped in time, exit WITHOUT running
        // destructors: process::exit never drops the winit backend, so the
        // display the worker borrows outlives it by construction.
        let stopped = state
            .host_clipboard
            .as_ref()
            .is_some_and(|hc| hc.wait_stopped(WORKER_STOP_WAIT));
        if stopped {
            let _ = worker.join();
        } else {
            tracing::warn!("host-clipboard worker still busy; exiting without teardown");
            std::process::exit(if run_result.is_err() { 1 } else { 0 });
        }
    }
    run_result?;
    Ok(())
}

/// How long teardown waits for the host-clipboard worker to finish before
/// abandoning it (see the exit path above). Its own clear is already bounded by
/// `hostclip::EXIT_CLEAR_WAIT_MS`; this only covers the thread winding down.
const WORKER_STOP_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

/// One-line description of what a client asked the cursor to be, for the debug
/// log: which named shape, or - for a client-drawn cursor - which surface it is,
/// which version of that surface's content, and whether it is mapped. Both parts
/// are needed: a toolkit may hand over a new surface per shape, or keep one
/// surface and commit a new buffer into it.
pub fn describe_cursor(status: &smithay::input::pointer::CursorImageStatus) -> String {
    use smithay::backend::renderer::utils::with_renderer_surface_state;
    use smithay::input::pointer::CursorImageStatus;
    use smithay::reexports::wayland_server::Resource;
    use smithay::utils::IsAlive;
    match status {
        CursorImageStatus::Hidden => "hidden".to_string(),
        CursorImageStatus::Named(icon) => format!("named {}", icon.name()),
        CursorImageStatus::Surface(surface) => {
            let mapped = with_renderer_surface_state(surface, |st| st.buffer().is_some());
            // The version matters as much as the identity: a toolkit swaps a resize
            // shape in by committing a NEW BUFFER to the SAME cursor surface, which
            // shows up here only as a bumped commit count.
            let version = with_renderer_surface_state(surface, |st| {
                format!("{:?} {:?}", st.current_commit(), st.buffer_size())
            });
            format!(
                "client surface {} v{} (alive {}, mapped {mapped:?})",
                surface.id().protocol_id(),
                version.unwrap_or_else(|| "-".into()),
                surface.alive()
            )
        }
    }
}

/// True when `VERACAGE_DEBUG=1` was forwarded (config `debug`): the compositor then
/// writes a per-second render summary and the cursor decisions through `vcdebug`.
/// Read once.
pub fn debug_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("VERACAGE_DEBUG").as_deref() == Ok("1"))
}

/// Where `vcdebug` appends: `$VERACAGE_LOG_DIR/compositor.log` when the human
/// side configured a log directory (config `log_dir`, forwarded through the
/// helper's env allowlist), else `compositor.log` in the runtime dir. Read once.
fn log_path() -> &'static std::path::Path {
    use std::sync::OnceLock;
    static PATH: OnceLock<std::path::PathBuf> = OnceLock::new();
    PATH.get_or_init(|| {
        let dir = std::env::var("VERACAGE_LOG_DIR")
            .unwrap_or_else(|_| crate::toolbar::RUNTIME_DIR.to_string());
        std::path::PathBuf::from(dir).join("compositor.log")
    })
}

/// Append a debug line to the log file (0644, so the human uid can read it
/// through the 0711 rt dir). The compositor's stdio is swallowed by
/// pkexec/privilege-drop, so the journal never sees its tracing - this is the
/// reliable channel for live debugging. See docs/debugging.md.
pub fn vcdebug(msg: &str) {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    // O_NOFOLLOW: the log directory can be configured by the human uid, so a
    // symlink planted in it would redirect a veracage-uid append into a file of
    // the planter's choosing. Permissions are set through the fd for the same
    // reason - the name may be a different file by the time we chmod it.
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(log_path())
    {
        let _ = f.set_permissions(std::fs::Permissions::from_mode(0o644));
        let _ = writeln!(f, "{msg}");
        return;
    }
    // No session directory (the compositor run straight from a shell, as in the
    // headless smoke): stderr is then the only place these can go.
    eprintln!("vcdebug: {msg}");
}

fn init_logging() {
    if let Ok(env_filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    } else {
        tracing_subscriber::fmt().init();
    }
}
