//! The Veracage broker — the human-uid session helper, **windowless**.
//!
//! Replaces the old standalone launcher window. It does the host-side things a
//! `veracage`-uid process cannot — pick a vault, collect its passphrase, `pkexec`
//! the mount, run the config picker, edit settings — but shows no persistent
//! window: only transient dialogs (an `rfd` file picker, and one-shot
//! `veracage-agent _passphrase`/`_settings`/`configure` subprocesses; each is a
//! fresh process because winit can't reopen an EventLoop in one process).
//!
//! It is driven by two signals:
//!   * the compositor's `cmd.req` (verbs open/configure/settings/close/import/
//!     export) — the in-session menu; the authority lives in the compositor, whose
//!     menu clicks a same-uid attacker can't forge, and whose `cmd.req` he can't
//!     write (`/run/veracage/rt` is 0711 veracage);
//!   * our own `open.req` in the human runtime dir — a second `veracage-agent`
//!     launch (double-clicking the icon again) signals the running broker to open
//!     another vault instead of starting a duplicate.
//!
//! Lifetime: single-instance (a pid file); it does the first open, then lives as
//! long as any `veracage open` child is running OR the compositor is up, and exits
//! when the session is fully gone.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use zeroize::{Zeroize, Zeroizing};

const CMD_REQ: &str = "/run/veracage/rt/cmd.req"; // compositor -> broker (must match toolbar.rs)
const COMPOSITOR_PID: &str = "/run/veracage/rt/compositor.pid";
const POLL: Duration = Duration::from_millis(300);
/// How long to wait for the compositor to FIRST appear before giving up (it comes
/// up behind a polkit prompt whose duration is the user's, not ours). Generous so
/// a slow authentication never orphans it; bounded so a cancelled auth still exits.
const COMPOSITOR_FIRST_WAIT: Duration = Duration::from_secs(180);

pub fn run_broker() -> ! {
    let dir = human_runtime_dir();
    let _ = std::fs::create_dir_all(&dir);

    // Single instance — but ONLY exit if a *live* broker already holds the pidfile
    // (then ask it to open another vault). Any other outcome (no pidfile, or we
    // can't write one) means we ARE the broker: proceed. Never silently exit on a
    // dir/permission problem — that's how "run it, nothing happens" happened.
    if another_broker_live(&dir) {
        signal_open(&dir);
        std::process::exit(0);
    }
    claim_pidfile(&dir); // best-effort; failure must not stop us

    let mut b = Broker {
        jobs: Vec::new(),
        cmd_seen: mtime(Path::new(CMD_REQ)),
        open_seen: mtime(&dir.join("open.req")),
        open_req: dir.join("open.req"),
        comp_seen: false,
        started: std::time::Instant::now(),
    };

    // Compositor-first: bring up the EMPTY compositor (the front door) — no
    // startup picker. File → Open mounts a vault; Apps → Configure sets up apps.
    // (No auto-Configure on empty config: it popped up behind the compositor and
    // was more jarring than helpful.)
    b.ensure_compositor();

    loop {
        b.reap();

        // Compositor menu commands.
        let now = mtime(Path::new(CMD_REQ));
        if now.is_some() && now != b.cmd_seen {
            b.cmd_seen = now;
            if let Some(verb) = read_verb(Path::new(CMD_REQ)) {
                b.dispatch(&verb);
            }
        }

        // A second `veracage-agent` launch asking us to open another vault.
        let now = mtime(&b.open_req);
        if now.is_some() && now != b.open_seen {
            b.open_seen = now;
            b.open_flow();
        }

        // Exit decision. The compositor is brought up asynchronously (its own
        // systemd unit + a polkit prompt): `_up` returns after only a few seconds,
        // but the user may still be authenticating, so the compositor can appear
        // LATER. Exiting the moment it isn't up yet would orphan the compositor
        // with no broker to serve its menu — the "run it, nothing happens" bug.
        // So: never treat "compositor down" as session-over until we've SEEN it up
        // at least once; before that, wait out a generous first-appearance grace
        // (covers a slow polkit auth) rather than exiting.
        if compositor_is_up() {
            b.comp_seen = true;
        }
        if b.jobs.is_empty() {
            if b.comp_seen && !compositor_is_up() {
                std::process::exit(0); // session fully over
            }
            if !b.comp_seen && b.started.elapsed() > COMPOSITOR_FIRST_WAIT {
                // It never came up (auth cancelled / bring-up failed) and nothing
                // is in flight — give up rather than poll forever.
                std::process::exit(0);
            }
        }
        std::thread::sleep(POLL);
    }
}

struct Broker {
    jobs: Vec<Child>,
    cmd_seen: Option<u128>,
    open_seen: Option<u128>,
    open_req: PathBuf,
    /// Latched once the compositor has been observed up — only then does its
    /// later disappearance mean the session is over (see the exit decision).
    comp_seen: bool,
    started: std::time::Instant,
}

impl Broker {
    /// Pick a vault, collect its passphrase (one-shot subprocess), and hand both to
    /// `veracage open --passphrase-stdin` (which `pkexec`s the mount + compositor).
    fn open_flow(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .set_title("Select an encrypted vault to open")
            .pick_file()
        else {
            return; // cancelled
        };
        let vault = path.to_string_lossy().into_owned();
        let name = base(&vault);

        // Collect the volume passphrase. Wrapped in Zeroizing so it's wiped on drop.
        let Some(mut pass) = prompt_passphrase(&name) else {
            return; // cancelled or no prompt available
        };

        let Some(bin) = veracage_bin() else {
            eprintln!("veracage: cannot find the veracage CLI next to this app; refusing to send the passphrase.");
            pass.zeroize();
            return;
        };
        match Command::new(bin)
            .arg("open")
            .arg(&vault)
            .arg("--passphrase-stdin")
            .stdin(Stdio::piped())
            .spawn()
        {
            Ok(mut child) => {
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(&pass); // drop -> EOF
                }
                self.jobs.push(child);
            }
            Err(e) => eprintln!("veracage: could not run `veracage open`: {e}"),
        }
        pass.zeroize();
    }

    fn dispatch(&mut self, verb: &str) {
        // Per-volume close carries a label: `close-volume:<label>` (Phase 5).
        if let Some(label) = verb.strip_prefix("close-volume:") {
            self.close_volume(label);
            return;
        }
        // "Close vault" closes EVERY open volume: `close-all:<l1>\t<l2>…`. Driven
        // by the labels the compositor already has, so it works regardless of
        // which process opened the vaults (unlike the old in-memory last_vault).
        if let Some(labels) = verb.strip_prefix("close-all:") {
            for label in labels.split('\t').filter(|l| !l.is_empty()) {
                self.close_volume(label);
            }
            return;
        }
        match verb {
            "open" => self.open_flow(),
            "configure" => self.spawn_dialog("configure"),
            "settings" => self.spawn_dialog("_settings"),
            "about" => self.spawn_dialog("_about"),
            "import" | "export" => {
                // File transfer is the shared Exchange folder — drop files in on
                // either side. Import/Export just open it in the host file manager.
                // Use the SAME dir the sandbox mounts at /exchange (the configured
                // exchange_dir), not a hardcoded ~/Veracage/Exchange — otherwise
                // files dropped here never appear inside the sandbox.
                let dir = crate::config::load().exchange_path();
                let _ = std::fs::create_dir_all(&dir);
                if let Err(e) = Command::new("xdg-open").arg(&dir).spawn() {
                    eprintln!("veracage: open exchange folder: {e}");
                }
            }
            other => eprintln!("veracage: unknown command {other:?}"),
        }
    }

    /// Bring up the empty compositor (front door) if it isn't running — via the
    /// CLI, which pkexecs the helper's `--spawn-compositor` and waits for it. This
    /// blocks on the polkit prompt + the compositor coming up. Passes the config
    /// theme through so the compositor matches the agent windows.
    fn ensure_compositor(&mut self) {
        let Some(bin) = veracage_bin() else {
            eprintln!("veracage: cannot find the veracage CLI; no compositor.");
            return;
        };
        let mut c = Command::new(bin);
        c.arg("_up");
        if crate::config::load().theme == "dark" {
            c.env("VERACAGE_THEME", "dark");
        }
        if let Err(e) = c.status() {
            eprintln!("veracage: could not start the compositor: {e}");
        }
    }

    /// Spawn a one-shot GUI subcommand of ourselves (configure / _settings).
    fn spawn_dialog(&mut self, sub: &str) {
        let exe = std::env::current_exe()
            .unwrap_or_else(|_| PathBuf::from("veracage-agent"));
        match Command::new(exe).arg(sub).spawn() {
            Ok(child) => self.jobs.push(child),
            Err(e) => eprintln!("veracage: could not open {sub}: {e}"),
        }
    }

    /// Close ONE volume of the running session (compositor's Close volume ▸ …).
    fn close_volume(&mut self, label: &str) {
        let Some(bin) = veracage_bin() else { return };
        match Command::new(bin).arg("close-volume").arg(label).spawn() {
            Ok(child) => self.jobs.push(child),
            Err(e) => eprintln!("veracage: could not close volume {label}: {e}"),
        }
    }

    /// Reap finished children so they don't linger as zombies.
    fn reap(&mut self) {
        self.jobs.retain_mut(|c| !matches!(c.try_wait(), Ok(Some(_))));
    }
}

// --------------------------------------------------------------- helpers -----

/// The installed `veracage` CLI (does the privileged mount + close). Resolved as a
/// SIBLING of our OWN binary — deliberately NOT via `$PATH`, and NOT via a
/// caller-settable env var in a shipped build (we pipe the plaintext passphrase to
/// it, so any redirection hands the passphrase to attacker code). Returns `None`
/// rather than falling back to a `$PATH`-resolved bare name.
fn veracage_bin() -> Option<std::ffi::OsString> {
    #[cfg(debug_assertions)]
    if let Some(v) = std::env::var_os("VERACAGE_CLI") {
        return Some(v);
    }
    let exe = std::env::current_exe().ok()?;
    let sib = exe.parent()?.join("veracage");
    sib.is_file().then(|| sib.into_os_string())
}

fn base(p: &str) -> String {
    Path::new(p)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(p)
        .to_string()
}

/// Prompt for the volume passphrase. Prefer the native KDE dialog
/// (`kdialog --password`) so it matches the polkit prompt's look; fall back to our
/// own hardened egui dialog (`veracage-agent _passphrase`) if kdialog is absent.
/// Returns None on cancel. The bytes are Zeroizing-wrapped for wipe-on-drop.
fn prompt_passphrase(name: &str) -> Option<Zeroizing<Vec<u8>>> {
    match Command::new("kdialog")
        .arg("--password")
        .arg(format!("Enter the passphrase for {name}"))
        .output()
    {
        Ok(o) if o.status.success() => {
            let mut p = o.stdout;
            while matches!(p.last(), Some(b'\n' | b'\r')) {
                p.pop();
            }
            return Some(Zeroizing::new(p));
        }
        // kdialog's documented cancel is exit code 1. ONLY that means "the user
        // cancelled" → abort. Any OTHER failure (a missing Qt platform plugin,
        // an unusable display, a killed-by-signal process — code() == None) is an
        // ENVIRONMENTAL failure, not a cancel: fall through to our own egui dialog
        // rather than silently aborting the open.
        Ok(o) if o.status.code() == Some(1) => return None,
        Ok(_) => {} // kdialog present but failed for another reason — try egui
        Err(_) => {} // kdialog not installed — fall through to egui
    }
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("veracage-agent"));
    let out = Command::new(exe).arg("_passphrase").arg(name).output().ok()?;
    out.status.success().then(|| Zeroizing::new(out.stdout))
}

fn human_runtime_dir() -> PathBuf {
    // A dir WE own — deliberately NOT `.../veracage`, which the root helper creates
    // (root-owned 0711) for the control socket; we couldn't write our pidfile there.
    let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(base).join("veracage-agent")
}

fn mtime(p: &Path) -> Option<u128> {
    let mt = std::fs::metadata(p).ok()?.modified().ok()?;
    Some(mt.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos())
}

fn read_verb(p: &Path) -> Option<String> {
    let s = std::fs::read_to_string(p).ok()?;
    let v = s.trim();
    (!v.is_empty()).then(|| v.to_string())
}

fn pid_alive(pid: i32) -> bool {
    pid > 0 && Path::new(&format!("/proc/{pid}")).exists()
}

/// True while the persistent compositor is running (its pidfile names a live pid).
fn compositor_is_up() -> bool {
    match std::fs::read_to_string(COMPOSITOR_PID) {
        Ok(s) => s.trim().parse::<i32>().map(pid_alive).unwrap_or(false),
        Err(_) => false,
    }
}

/// True only if the pidfile names a **live** process — the one case where a second
/// launch should defer to a running broker. A missing/unreadable pidfile returns
/// false (we proceed), so a permission problem never causes a silent no-op.
fn another_broker_live(dir: &Path) -> bool {
    match std::fs::read_to_string(dir.join("broker.pid")) {
        Ok(s) => s.trim().parse::<i32>().map(pid_alive).unwrap_or(false),
        Err(_) => false,
    }
}

/// Best-effort: record our pid so a later launch can defer to us. Any failure
/// (e.g. an unwritable dir) is logged, not fatal — we still run.
fn claim_pidfile(dir: &Path) {
    let pidfile = dir.join("broker.pid");
    let _ = std::fs::remove_file(&pidfile); // clear any stale one
    match OpenOptions::new().write(true).create(true).truncate(true).open(&pidfile) {
        Ok(mut f) => {
            let _ = write!(f, "{}", std::process::id());
        }
        Err(e) => eprintln!("veracage-agent: can't write {}: {e} (continuing)", pidfile.display()),
    }
}

/// Ask the already-running broker to open another vault (bumps a file it polls).
fn signal_open(dir: &Path) {
    let _ = std::fs::write(dir.join("open.req"), b"1");
}
