//! Veracage nested compositor — fixed-function. Seeded from smithay's `smallvil`
//! reference; grown with a leader-only clipboard channel. Renders the sandbox
//! apps into one window on the host (nested via the inherited WAYLAND_SOCKET fd),
//! exposes NO `data-control` global (the host clipboard bridge is a private
//! socket only the leader holds).
#![allow(irrefutable_let_patterns)]

mod clipboard;
mod grabs;
mod handlers;
mod input;
mod state;
mod toolbar;
mod winit;

use smithay::reexports::{calloop::EventLoop, wayland_server::Display};
pub use state::State;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_logging();

    // --socket <name>  the wayland socket name apps connect to (the leader picks
    //                  it, like `weston --socket=`, so it knows it). The clipboard
    //                  is owned in-process now (clipboard.rs) — no clip socket.
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

    event_loop.run(None, &mut state, move |_| {})?;
    Ok(())
}

fn init_logging() {
    if let Ok(env_filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    } else {
        tracing_subscriber::fmt().init();
    }
}
