//! The Veracage broker, the human-uid session helper, **windowless**.
//!
//! It does the host-side things a `veracage`-uid process cannot: pick a volume,
//! collect its passphrase, `pkexec` the mount, run the config picker, edit
//! settings. It shows no persistent window, only transient dialogs (an `rfd`
//! file picker, and one-shot
//! `veracage-agent _passphrase`/`_settings`/`_help`/`configure` subprocesses;
//! each is a fresh process because winit can't reopen an EventLoop in one
//! process).
//!
//! It is driven by two signals:
//!   * the compositor's `cmd.req` (verbs open/open-app:<key>/configure/settings/
//!     exchange/help/about/close-volume:<label>): the in-session menu; the
//!     authority lives in the compositor, whose menu clicks a same-uid attacker
//!     can't forge, and whose `cmd.req` he can't write (`/run/veracage/rt` is
//!     0711 veracage);
//!   * our own `open.req` in the human runtime dir, a second `veracage-agent`
//!     launch (double-clicking the icon again) signals the running broker to open
//!     another volume instead of starting a duplicate.
//!
//! It also PUBLISHES the configured app list (plus each app's host icon) to
//! `/run/veracage/pub`, so the compositor's Apps menu is populated before any
//! volume is mounted. Republished whenever config.toml changes. (The menu-item
//! glyphs are the compositor's own: see compositor mono_icons.)
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
/// Human-published dir (created by the helper, owned by us): the config-app
/// list + menu icons for the compositor's Apps menu. Must match toolbar.rs.
const PUB_DIR: &str = "/run/veracage/pub";
const POLL: Duration = Duration::from_millis(300);
/// How long to wait for the compositor to FIRST appear before giving up (it comes
/// up behind a polkit prompt whose duration is the user's, not ours). Generous so
/// a slow authentication never orphans it; bounded so a cancelled auth still exits.
const COMPOSITOR_FIRST_WAIT: Duration = Duration::from_secs(180);
/// The helper's "decrypt failed" exit code (EXIT_CRYPT_FAILED in helper-rs):
/// almost always a wrong passphrase: re-prompt instead of failing silently.
const EXIT_CRYPT_FAILED: i32 = 4;

pub fn run_broker() -> ! {
    let dir = human_runtime_dir();
    let _ = std::fs::create_dir_all(&dir);

    // Single instance, but ONLY exit if a *live* broker already holds the pidfile
    // (then ask it to open another volume). Any other outcome (no pidfile, or we
    // can't write one) means we ARE the broker: proceed. Never silently exit on a
    // dir/permission problem: that's how "run it, nothing happens" happened.
    if another_broker_live(&dir) {
        signal_open(&dir);
        std::process::exit(0);
    }
    claim_pidfile(&dir); // best-effort; failure must not stop us

    let mut b = Broker {
        jobs: Vec::new(),
        dialogs: Vec::new(),
        opens: Vec::new(),
        cmd_seen: mtime(Path::new(CMD_REQ)),
        open_seen: mtime(&dir.join("open.req")),
        cfg_seen: mtime(&crate::config::config_path()),
        open_req: dir.join("open.req"),
        comp_seen: false,
        started: std::time::Instant::now(),
    };

    // Compositor-first: bring up the EMPTY compositor (the front door). No
    // startup picker. File > Mount volume mounts one; Apps > Configure sets up
    // apps. If the bring-up FAILED (e.g. the user cancelled or mistyped the
    // polkit password 3x), exit now rather than lingering: a lingering broker
    // would answer a second launch's open.req with a stray volume picker, and
    // there is no compositor window to serve anyway.
    if !b.ensure_compositor() && !compositor_is_up() {
        eprintln!("veracage: the compositor did not start (authentication cancelled or failed); exiting.");
        std::process::exit(1);
    }
    publish_apps();

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

        // A second `veracage-agent` launch asking us to open another volume.
        let now = mtime(&b.open_req);
        if now.is_some() && now != b.open_seen {
            b.open_seen = now;
            b.open_flow(None);
        }

        // Config changed (Configure apps saved, settings edited, hand edit):
        // republish the app list + icons for the compositor's Apps menu, and
        // push the new list into the RUNNING session so it applies now.
        let now = mtime(&crate::config::config_path());
        if now != b.cfg_seen {
            b.cfg_seen = now;
            publish_apps();
            b.sync_apps();
        }

        // Exit decision. The compositor is brought up asynchronously (its own
        // systemd unit + a polkit prompt): `_up` returns after only a few seconds,
        // but the user may still be authenticating, so the compositor can appear
        // LATER. Exiting the moment it isn't up yet would orphan the compositor
        // with no broker to serve its menu: the "run it, nothing happens" bug.
        // So: never treat "compositor down" as session-over until we've SEEN it up
        // at least once; before that, wait out a generous first-appearance grace
        // (covers a slow polkit auth) rather than exiting.
        if compositor_is_up() {
            b.comp_seen = true;
        } else if b.comp_seen {
            // The user quit Veracage: transient dialogs (Settings, Configure
            // apps, Help...) must not outlive the Veracage window. CLI jobs
            // (unmounts, opens) still finish on their own below.
            for d in &mut b.dialogs {
                let _ = d.kill();
            }
        }
        if b.jobs.is_empty() && b.dialogs.is_empty() && b.opens.is_empty() {
            if b.comp_seen && !compositor_is_up() {
                std::process::exit(0); // session fully over
            }
            if !b.comp_seen && b.started.elapsed() > COMPOSITOR_FIRST_WAIT {
                // It never came up (auth cancelled / bring-up failed) and nothing
                // is in flight, give up rather than poll forever.
                std::process::exit(0);
            }
        }
        std::thread::sleep(POLL);
    }
}

/// A running `veracage open`, tracked with enough context to re-prompt on a
/// wrong passphrase (helper exit code 4) instead of failing silently.
struct OpenJob {
    child: Child,
    volume: String,      // the volume path handed to `veracage open`
    app: Option<String>, // the config-app key to auto-launch, if any
}

struct Broker {
    /// Spawned CLI children (close-volume, _sync-apps), left to finish even
    /// when the session ends.
    jobs: Vec<Child>,
    /// One-shot GUI dialogs (Settings, Configure apps, Help...), killed when
    /// the compositor goes away, so no window outlives the session.
    dialogs: Vec<Child>,
    opens: Vec<OpenJob>,
    cmd_seen: Option<u128>,
    open_seen: Option<u128>,
    cfg_seen: Option<u128>,
    open_req: PathBuf,
    /// Latched once the compositor has been observed up: only then does its
    /// later disappearance mean the session is over (see the exit decision).
    comp_seen: bool,
    started: std::time::Instant,
}

impl Broker {
    /// Pick a volume file, then collect the passphrase and mount it. `app` is
    /// the config-app key to auto-launch after the mount (an Apps-menu click
    /// with nothing mounted yet).
    fn open_flow(&mut self, app: Option<String>) {
        let Some(path) = rfd::FileDialog::new()
            .set_title("Select an encrypted volume to mount")
            .pick_file()
        else {
            return; // cancelled
        };
        self.open_with(path.to_string_lossy().into_owned(), app, false);
    }

    /// Collect the passphrase for `volume` (one-shot subprocess) and hand both
    /// to `veracage open --passphrase-stdin` (which `pkexec`s the mount +
    /// compositor). With `wrong_pass` the prompt says the last try failed.
    fn open_with(&mut self, volume: String, app: Option<String>, wrong_pass: bool) {
        let name = base(&volume);

        // Collect the volume passphrase. Wrapped in Zeroizing so it's wiped on drop.
        let Some(mut pass) = prompt_passphrase(&name, wrong_pass) else {
            return; // cancelled or no prompt available
        };

        let Some(bin) = veracage_bin() else {
            eprintln!("veracage: cannot find the veracage CLI next to this app; refusing to send the passphrase.");
            pass.zeroize();
            return;
        };
        let mut cmd = Command::new(bin);
        cmd.arg("open").arg(&volume);
        if let Some(a) = &app {
            cmd.arg(a);
        }
        match cmd.arg("--passphrase-stdin").stdin(Stdio::piped()).spawn() {
            Ok(mut child) => {
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(&pass); // drop -> EOF
                }
                // Tell the compositor to show a progress indicator: unlocking a
                // volume is several seconds of key derivation with nothing else
                // to see. Cleared in `reap` when this child finishes.
                publish_status(&format!("Unlocking {name}"));
                debug_log(&format!("open {name}: pid {} spawned", child.id()));
                self.opens.push(OpenJob { child, volume, app });
            }
            Err(e) => eprintln!("veracage: could not run `veracage open`: {e}"),
        }
        pass.zeroize();
    }

    fn dispatch(&mut self, verb: &str) {
        // Unmount one volume carries a label: `close-volume:<label>`.
        if let Some(label) = verb.strip_prefix("close-volume:") {
            self.close_volume(label);
            return;
        }
        match verb {
            "open" => self.open_flow(None),
            "configure" => self.spawn_dialog("configure"),
            "settings" => self.spawn_dialog("_settings"),
            "shortcuts" => self.spawn_dialog("_shortcuts"),
            "about" => self.spawn_dialog("_about"),
            "help" => self.spawn_dialog("_help"),
            "exchange" => {
                // File transfer is the shared directory. Drop files in on
                // either side. This just opens it in the host file manager.
                // Use the SAME dir the sandbox mounts at /exchange (the configured
                // exchange_dir), not a hardcoded ~/Veracage/Exchange. Otherwise
                // files dropped here never appear inside the sandbox.
                let dir = crate::config::load().exchange_path();
                let _ = std::fs::create_dir_all(&dir);
                match Command::new("xdg-open").arg(&dir).spawn() {
                    // Track it so `reap()` waits on it. Otherwise the exited
                    // xdg-open lingers as a zombie for the broker's lifetime.
                    Ok(child) => self.jobs.push(child),
                    Err(e) => eprintln!("veracage: open the shared directory: {e}"),
                }
            }
            other => eprintln!("veracage: unknown command {other:?}"),
        }
    }

    /// Bring up the empty compositor (front door) if it isn't running: via the
    /// CLI, which pkexecs the helper's `--spawn-compositor` and waits for it. This
    /// blocks on the polkit prompt + the compositor coming up. `veracage _up`
    /// itself forwards the config theme/font/window size. Returns true iff it
    /// succeeded (exit 0); false on auth cancel/failure or a spawn error.
    fn ensure_compositor(&mut self) -> bool {
        let Some(bin) = veracage_bin() else {
            eprintln!("veracage: cannot find the veracage CLI; no compositor.");
            return false;
        };
        match Command::new(bin).arg("_up").status() {
            Ok(st) => st.success(),
            Err(e) => {
                eprintln!("veracage: could not start the compositor: {e}");
                false
            }
        }
    }

    /// Spawn a one-shot GUI subcommand of ourselves (configure / _settings / ...).
    fn spawn_dialog(&mut self, sub: &str) {
        let exe = std::env::current_exe()
            .unwrap_or_else(|_| PathBuf::from("veracage-agent"));
        match Command::new(exe).arg(sub).spawn() {
            Ok(child) => self.dialogs.push(child),
            Err(e) => eprintln!("veracage: could not open {sub}: {e}"),
        }
    }

    /// Push the changed enabled-app list into the running session's leader
    /// (`veracage _sync-apps`), so a Configure-apps save updates the Apps menu
    /// now, not only on the next mount.
    fn sync_apps(&mut self) {
        let Some(bin) = veracage_bin() else { return };
        match Command::new(bin).arg("_sync-apps").spawn() {
            Ok(child) => self.jobs.push(child),
            Err(e) => eprintln!("veracage: could not sync the app list: {e}"),
        }
    }

    /// Unmount ONE volume of the running session (compositor's Unmount menu).
    fn close_volume(&mut self, label: &str) {
        let Some(bin) = veracage_bin() else { return };
        match Command::new(bin).arg("close-volume").arg(label).spawn() {
            Ok(child) => self.jobs.push(child),
            Err(e) => eprintln!("veracage: could not unmount volume {label}: {e}"),
        }
    }

    /// Reap finished children. A finished `veracage open` is inspected: exit 4
    /// (decrypt failed, wrong passphrase) re-prompts for the same volume; a
    /// polkit cancel (126/127) is silent; any other failure shows an error
    /// dialog instead of failing into the journal only.
    fn reap(&mut self) {
        self.jobs.retain_mut(|c| !matches!(c.try_wait(), Ok(Some(_))));
        self.dialogs.retain_mut(|c| !matches!(c.try_wait(), Ok(Some(_))));
        let mut retries: Vec<(String, Option<String>)> = Vec::new();
        let mut errors: Vec<String> = Vec::new();
        self.opens.retain_mut(|j| match j.child.try_wait() {
            Ok(Some(st)) => {
                debug_log(&format!("open {} finished: {st}", base(&j.volume)));
                match st.code() {
                    Some(0) | None => {}
                    Some(EXIT_CRYPT_FAILED) => {
                        retries.push((j.volume.clone(), j.app.take()));
                    }
                    Some(126) | Some(127) => {} // polkit auth cancelled / denied
                    Some(c) => errors.push(format!(
                        "Could not mount {} (exit {c}).\nSee `journalctl --user` for details.",
                        base(&j.volume)
                    )),
                }
                false
            }
            _ => true,
        });
        // No open in flight: drop the progress indicator. A retry below
        // republishes it, so this can't leave the spinner up on a wrong
        // passphrase.
        if self.opens.is_empty() {
            publish_status("");
        }
        for msg in errors {
            // Track the dialog child so it's reaped, not left a zombie.
            if let Some(child) = show_error(&msg) {
                self.jobs.push(child);
            }
        }
        for (volume, app) in retries {
            self.open_with(volume, app, true);
        }
    }
}

// --------------------------------------------------------------- publish -----

/// The publish dir. Debug builds honour VERACAGE_PUB_DIR so the publish path
/// can be exercised without the root-created /run/veracage/pub.
fn pub_dir() -> PathBuf {
    #[cfg(debug_assertions)]
    if let Some(d) = std::env::var_os("VERACAGE_PUB_DIR") {
        return PathBuf::from(d);
    }
    PathBuf::from(PUB_DIR)
}

/// Write the desired compositor window size to `PUB_DIR/window.size` so the
/// running compositor can pick it up on its next scan and resize live (Settings
/// Save calls this; the compositor validates the value). Best-effort.
pub fn publish_window_size(size: &str) {
    let dir = pub_dir();
    if !dir.is_dir() {
        return;
    }
    let tmp = dir.join(format!("window.size.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, format!("{size}\n")).is_ok() {
        let _ = std::fs::rename(&tmp, dir.join("window.size"));
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Write the Veracage keyboard shortcuts to `PUB_DIR/shortcuts` (`<action>\t<bind>`
/// per line) so the compositor matches them live. Best-effort.
pub fn publish_shortcuts(cfg: &crate::config::Config) {
    let dir = pub_dir();
    if !dir.is_dir() {
        return;
    }
    let mut body = String::new();
    for (action, default) in crate::config::SHORTCUT_DEFAULTS {
        let bind = cfg.shortcuts.get(*action).map(String::as_str).unwrap_or(default);
        body.push_str(&format!("{action}\t{bind}\n"));
    }
    let tmp = dir.join(format!("shortcuts.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, body).is_ok() {
        let _ = std::fs::rename(&tmp, dir.join("shortcuts"));
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Write the host-clipboard auto-clear policy to `PUB_DIR/clipclear`
/// (`<0|1 enabled>\n<timeout secs>`) so the compositor's clipboard worker picks
/// it up on its next scan. Best-effort.
/// One timing line to stderr (the broker's journal entry) when config `debug` is
/// on. Read it with `journalctl --user -f`; see docs/debugging.md.
pub fn debug_log(msg: &str) {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    if *ON.get_or_init(|| crate::config::load().debug) {
        eprintln!("veracage-agent[debug] {msg}");
    }
}

/// Publish a one-line progress note for the compositor's toolbar (a spinner plus
/// this text), or clear it with an empty `text`. Used around the seconds-long
/// unlock, so the window is not silently busy. Best-effort: no indicator is a
/// cosmetic loss, never a reason to fail a mount.
pub fn publish_status(text: &str) {
    let dir = pub_dir();
    if !dir.is_dir() {
        return;
    }
    let path = dir.join("status");
    if text.is_empty() {
        let _ = std::fs::remove_file(&path);
        return;
    }
    let body: String = text.chars().filter(|c| !c.is_control()).take(80).collect();
    let tmp = dir.join(format!("status.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, format!("{body}\n")).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

pub fn publish_clipclear(cfg: &crate::config::Config) {
    let dir = pub_dir();
    if !dir.is_dir() {
        return;
    }
    let body = format!("{}\n{}\n", cfg.clip_clear as u8, cfg.clip_clear_timeout);
    let tmp = dir.join(format!("clipclear.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, body).is_ok() {
        let _ = std::fs::rename(&tmp, dir.join("clipclear"));
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Write the resolved UI font file + base size to `PUB_DIR/font` (`<path>\n<size>`)
/// so the running compositor re-loads its menu-bar font live on its next scan.
/// Empty path line means "keep egui's default face". Best-effort.
pub fn publish_font(cfg: &crate::config::Config) {
    let dir = pub_dir();
    if !dir.is_dir() {
        return;
    }
    let path = crate::fonts::resolve_font_file(&cfg.ui_font).unwrap_or_default();
    let size = crate::fonts::base_size(&cfg.ui_font, &cfg.ui_font_size);
    let body = format!("{path}\n{size}\n");
    let tmp = dir.join(format!("font.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, body).is_ok() {
        let _ = std::fs::rename(&tmp, dir.join("font"));
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Write the configured app list (and menu icons) where the compositor's Apps
/// menu reads them: `PUB_DIR/config.apps` (`<key>\t<name>` per line) and
/// `PUB_DIR/icons/<key>.rgba` (`<w u32 LE><h u32 LE><rgba>`). Best-effort: the
/// dir exists only once the helper has spawned the compositor, and the menu
/// degrades gracefully without any of it.
pub fn publish_apps() {
    let dir = pub_dir();
    let dir = dir.as_path();
    if !dir.is_dir() {
        return;
    }
    let cfg = crate::config::load();
    publish_window_size(&cfg.window_size);
    publish_font(&cfg);
    publish_shortcuts(&cfg);
    publish_clipclear(&cfg);
    // The key becomes a file name and a cmd.req verb suffix, keep it plain.
    let sane = |k: &str| !k.is_empty() && k.len() <= 64 && !k.contains('/') && k != "..";
    // The name is written into the `<key>\t<name>` TSV: strip tab/newline/control
    // chars so a hostile config.toml name can't split or inject lines in the
    // published file (the compositor re-validates keys, but keep the format
    // un-corruptible at the source).
    let clean_name = |n: &str| -> String {
        n.chars().filter(|c| !c.is_control()).take(128).collect()
    };

    let mut body = String::new();
    for a in cfg.apps.iter().filter(|a| sane(&a.key)) {
        body.push_str(&format!("{}\t{}\n", a.key, clean_name(&a.name)));
    }
    let tmp = dir.join(format!("config.apps.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, body).is_ok() {
        let _ = std::fs::rename(&tmp, dir.join("config.apps"));
    } else {
        let _ = std::fs::remove_file(&tmp);
    }

    let icons = dir.join("icons");
    let _ = std::fs::create_dir_all(&icons);
    // Drop icons of apps no longer configured.
    if let Ok(entries) = std::fs::read_dir(&icons) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(stem) = name.strip_suffix(".rgba") else { continue };
            if !cfg.apps.iter().any(|a| a.key == stem) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    for a in cfg.apps.iter().filter(|a| sane(&a.key)) {
        let path = icons.join(format!("{}.rgba", a.key));
        // Skip only a NON-EMPTY icon already on disk: a real icon (or the generic
        // fallback) is cached, so an app never re-runs the expensive recursive
        // theme scan on every publish_apps. A 0-byte file is a stale "miss"
        // sentinel from an older binary (before SVG + generic fallback existed);
        // re-resolve it, so an upgrade heals it to a real or generic icon instead
        // of pinning the app to text forever. Genuine misses now cache the
        // (non-empty) generic icon, so they don't rescan either.
        if path.metadata().map(|m| m.len() > 0).unwrap_or(false) {
            continue;
        }
        // The app's own icon, else a generic app icon so the menu shows a glyph
        // rather than bare text. The 0-byte sentinel is reached only if even the
        // generic can't be resolved (no theme, missing bundled asset).
        let blob = crate::detect::icon_rgba_for_exec(&a.exec)
            .or_else(crate::detect::generic_icon_rgba)
            .map(|(w, h, rgba)| {
                let mut b = Vec::with_capacity(8 + rgba.len());
                b.extend_from_slice(&w.to_le_bytes());
                b.extend_from_slice(&h.to_le_bytes());
                b.extend_from_slice(&rgba);
                b
            })
            .unwrap_or_default();
        let tmp = icons.join(format!("{}.tmp", a.key));
        if std::fs::write(&tmp, blob).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        } else {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

// --------------------------------------------------------------- helpers -----

/// The installed `veracage` CLI (does the privileged mount + close). Resolved as a
/// SIBLING of our OWN binary: deliberately NOT via `$PATH`, and NOT via a
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
/// With `wrong_pass` the prompt says the previous attempt failed. Returns None on
/// cancel. The bytes are Zeroizing-wrapped for wipe-on-drop.
fn prompt_passphrase(name: &str, wrong_pass: bool) -> Option<Zeroizing<Vec<u8>>> {
    let msg = if wrong_pass {
        format!("Wrong passphrase. Enter the passphrase for {name}")
    } else {
        format!("Enter the passphrase for {name}")
    };
    match Command::new("kdialog").arg("--password").arg(&msg).output() {
        Ok(o) if o.status.success() => {
            let mut p = o.stdout;
            while matches!(p.last(), Some(b'\n' | b'\r')) {
                p.pop();
            }
            return Some(Zeroizing::new(p));
        }
        // kdialog's documented cancel is exit code 1. ONLY that means "the user
        // cancelled" → abort. Any OTHER failure (a missing Qt platform plugin,
        // an unusable display, a killed-by-signal process, code() == None) is an
        // ENVIRONMENTAL failure, not a cancel: fall through to our own egui dialog
        // rather than silently aborting the open.
        Ok(o) if o.status.code() == Some(1) => return None,
        Ok(_) => {} // kdialog present but failed for another reason, try egui
        Err(_) => {} // kdialog not installed, fall through to egui
    }
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("veracage-agent"));
    let mut cmd = Command::new(exe);
    cmd.arg("_passphrase").arg(name);
    if wrong_pass {
        cmd.arg("Wrong passphrase, try again");
    }
    let out = cmd.output().ok()?;
    out.status.success().then(|| Zeroizing::new(out.stdout))
}

/// Show a (non-blocking) error dialog: kdialog, else zenity, else the journal.
/// Returns the spawned dialog Child (for the caller to reap) or None if it fell
/// back to stderr.
fn show_error(msg: &str) -> Option<Child> {
    for (bin, args) in [
        ("kdialog", vec!["--error", msg]),
        ("zenity", vec!["--error", "--text", msg]),
    ] {
        if let Ok(child) = Command::new(bin).args(&args).spawn() {
            return Some(child);
        }
    }
    eprintln!("veracage: {msg}");
    None
}

fn human_runtime_dir() -> PathBuf {
    // A dir WE own: deliberately NOT `.../veracage`, which the root helper creates
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

/// True only if the pidfile names a **live** process: the one case where a second
/// launch should defer to a running broker. A missing/unreadable pidfile returns
/// false (we proceed), so a permission problem never causes a silent no-op.
fn another_broker_live(dir: &Path) -> bool {
    match std::fs::read_to_string(dir.join("broker.pid")) {
        Ok(s) => s.trim().parse::<i32>().map(pid_alive).unwrap_or(false),
        Err(_) => false,
    }
}

/// Best-effort: record our pid so a later launch can defer to us. Any failure
/// (e.g. an unwritable dir) is logged, not fatal. We still run.
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

/// Ask the already-running broker to open another volume (bumps a file it polls).
fn signal_open(dir: &Path) {
    let _ = std::fs::write(dir.join("open.req"), b"1");
}
