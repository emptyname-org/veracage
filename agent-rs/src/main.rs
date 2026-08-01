//! Veracage human-side agent, one binary, two faces, no vault access:
//!
//!   * **CLI** (this file): `veracage-agent <vault> <cmd>`, a thin, scriptable
//!     client over the leader's control socket. First-class and kept: handy for
//!     CLI users and automation/scripts.
//!   * **GUI** (next phase): a small egui control window for the same actions.
//!
//! Everything goes through `proto` (the control socket); the agent holds no
//! privilege and never touches the vault.

mod apps;
mod broker;
mod config;
mod detect;
mod fonts;
mod gl_icon;
mod keyboard;
mod proto;
mod theme;
mod ui_about;
mod ui_config;
mod ui_help;
mod ui_passphrase;
mod ui_appearance;
mod ui_settings;
mod ui_shortcuts;

use std::io;
use std::process::ExitCode;

use serde_json::Value;

fn usage() {
    eprint!(
        r#"usage:
  veracage-agent <volume> <cmd> [args]

session cmds (need a running `veracage open`):
  ping | list | close | socket-path

(launching and file transfer are NOT here: the control socket is human-owned, so
 any same-uid process can reach it. Launch apps from the compositor toolbar.)
"#
    );
}

fn main() -> ExitCode {
    // Never dump core: the agent collects the volume passphrase and pipes it to
    // the helper, and a core would write it to a host-readable file under
    // /var/lib/systemd/coredump. Clearing `coredump_filter` leaves a dump with no
    // memory in it; `RLIMIT_CORE` would not help, the kernel ignores it when
    // `core_pattern` is a pipe (the systemd default). See the helper's
    // `suppress_core_dumps`, which does the same for the vault side.
    let _ = std::fs::write("/proc/self/coredump_filter", "0\n");

    let args: Vec<String> = std::env::args().collect();

    // No args (the `.desktop` app / double-click): run the headless broker, the
    // windowless human-side session helper (front door + in-session commands). It
    // never returns.
    if args.len() == 1 {
        broker::run_broker();
    }

    // One-shot transient passphrase dialog, spawned by the broker:
    // `veracage-agent _passphrase <volume-name> [error]`. Writes the passphrase
    // to stdout; the optional error line is shown in red (retry after a wrong
    // passphrase).
    if args.get(1).map(String::as_str) == Some("_passphrase") {
        let name = args.get(2).cloned().unwrap_or_default();
        let error = args.get(3).cloned();
        return match ui_passphrase::run(name, error) {
            // Reaching here means the window closed without submitting (submit
            // exits 0 itself, cancel exits 1), treat as cancelled.
            Ok(()) => ExitCode::from(1),
            Err(e) => {
                eprintln!("veracage-agent: passphrase: {e}");
                ExitCode::FAILURE
            }
        };
    }

    // One-shot transient settings dialog: `veracage-agent _settings`.
    if args.get(1).map(String::as_str) == Some("_settings") {
        return match ui_settings::run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("veracage-agent: settings: {e}");
                ExitCode::FAILURE
            }
        };
    }

    // About window: `veracage-agent _about` (Help > About Veracage).
    if args.get(1).map(String::as_str) == Some("_about") {
        return match ui_about::run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("veracage-agent: about: {e}");
                ExitCode::FAILURE
            }
        };
    }

    // Hidden: republish the configured app list + menu icons for the
    // compositor's Apps menu (normally the broker does this automatically).
    if args.get(1).map(String::as_str) == Some("_publish") {
        broker::publish_apps();
        return ExitCode::SUCCESS;
    }

    // Help window: `veracage-agent _help` (Help > Help).
    if args.get(1).map(String::as_str) == Some("_help") {
        return match ui_help::run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("veracage-agent: help: {e}");
                ExitCode::FAILURE
            }
        };
    }

    // Appearance window: `veracage-agent _appearance` (Settings > Appearance).
    if args.get(1).map(String::as_str) == Some("_appearance") {
        return match ui_appearance::run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("veracage-agent: appearance: {e}");
                ExitCode::FAILURE
            }
        };
    }

    // Shortcuts window: `veracage-agent _shortcuts` (Settings > Shortcuts).
    if args.get(1).map(String::as_str) == Some("_shortcuts") {
        return match ui_shortcuts::run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("veracage-agent: shortcuts: {e}");
                ExitCode::FAILURE
            }
        };
    }

    // GUI config picker: `veracage-agent configure` (replaces configure.py's Qt).
    if args.get(1).map(String::as_str) == Some("configure") {
        return match ui_config::run_configure() {
            Ok(o) if o.saved => {
                println!("veracage: enabled {} app(s).", o.count);
                ExitCode::SUCCESS
            }
            Ok(_) => {
                println!("veracage: configuration cancelled.");
                ExitCode::from(1)
            }
            Err(e) => {
                eprintln!("veracage-agent: configure: {e}");
                ExitCode::FAILURE
            }
        };
    }

    if args.len() < 3 {
        usage();
        return ExitCode::from(2);
    }
    // Resolve the vault path (canonicalize, like the helper/cli.py) so the
    // control-socket hash matches regardless of a relative path or symlink.
    let vault_resolved = std::fs::canonicalize(&args[1])
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| args[1].clone());
    let vault = vault_resolved.as_str();
    let cmd = args[2].as_str();

    // Non-socket helper: print the control-socket path.
    if cmd == "socket-path" {
        return match proto::socket_path(vault) {
            Ok(p) => {
                println!("{}", p.display());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        };
    }

    let result: io::Result<Value> = match cmd {
        "ping" => proto::ping(vault),
        "list" => proto::list(vault),
        "close" => proto::close(vault),
        other => {
            eprintln!("unknown cmd: {other}");
            usage();
            return ExitCode::from(2);
        }
    };

    match result {
        Ok(v) => {
            println!("{v}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

