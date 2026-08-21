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
//!   * the compositor's `cmd.log` (verbs open/configure/settings/
//!     exchange/help/about/close-volume:<label>): the in-session menu; the
//!     authority lives in the compositor, whose menu clicks a same-uid attacker
//!     can't forge, and whose `cmd.log` he can't write (`/run/veracage/rt` is
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
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use zeroize::{Zeroize, Zeroizing};

/// compositor -> broker command log, `<nonce>\t<verb>` per line, append-only
/// (must match CMD_LOG in compositor-rs/src/toolbar.rs). We only ever read it:
/// `rt` is 0711 veracage, so this uid can traverse but not write, and the
/// compositor is the one that starts it empty each session.
const CMD_LOG: &str = "/run/veracage/rt/cmd.log";
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
/// The helper's "the filesystem needs repair" exit code (EXIT_FSCK_FAILED): the
/// volume decrypted fine but was left dismounted, so say what to do about it.
const EXIT_FSCK_FAILED: i32 = 5;
/// The helper's "the volume stayed open" exit code (EXIT_VOLUME_BUSY): it
/// refused, so the volume is untouched and still decrypted.
const EXIT_VOLUME_BUSY: i32 = 6;
/// The leader's "the apps are gone" signal (leader.py `_post_apps_closed`): the
/// waiting helper is told to go on when this changes.
const APPS_CLOSED: &str = "/run/veracage/rt/closeapps.done";
/// How long that signal is waited for before the dismount is cancelled. Only
/// reached when the human leaves an app's own save prompt unanswered, so the
/// volume stays mounted and they already know why.
const APPS_CLOSED_WAIT: Duration = Duration::from_secs(180);

pub fn run_broker() -> ! {
    let Some(dir) = human_runtime_dir() else {
        eprintln!("veracage: XDG_RUNTIME_DIR is not set; refusing to run.");
        std::process::exit(2);
    };
    let _ = std::fs::create_dir_all(&dir);

    // Single instance. Only a live broker holding the lock makes us hand our
    // request over and exit; anything else means we ARE the broker and proceed.
    // `_instance` is bound for the whole function: dropping it would release the
    // lock and let a second launch in.
    let _instance = match claim_single_instance(&dir) {
        Instance::Taken => {
            signal_open(&dir);
            std::process::exit(0);
        }
        other => other,
    };
    // Any progress note here is ours from a previous run that did not get to clear
    // it (a crash, a kill). Left in place it turns the new session's spinner on with
    // nothing happening behind it.
    publish_status("");

    let mut b = Broker {
        jobs: Vec::new(),
        dismounts: Vec::new(),
        dialogs: Vec::new(),
        opens: Vec::new(),
        cmd_seen: last_command_nonce(Path::new(CMD_LOG)),
        open_seen: mtime(&dir.join("open.req")),
        cfg_seen: mtime(&crate::config::config_path()),
        open_req: dir.join("open.req"),
        comp_seen: false,
        started: std::time::Instant::now(),
    };

    // Compositor-first: bring up the EMPTY compositor (the front door). No
    // startup picker. File > Open volume opens one; Apps > Configure sets up
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
        b.drive_dismounts();

        // Compositor menu commands: everything appended since the last drain, in
        // order. Reading the whole log each time (it is a few dozen bytes per
        // click, cleared at compositor startup) is what makes a verb impossible to
        // miss, however long a dialog kept us out of this loop.
        for (nonce, verb) in read_commands(Path::new(CMD_LOG), b.cmd_seen) {
            b.cmd_seen = Some(nonce);
            b.dispatch(&verb);
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
            // (dismounts, opens) still finish on their own below.
            for (_, d) in &mut b.dialogs {
                let _ = d.kill();
            }
            // Including a "Close <apps> to continue." nobody can act on any more.
            for d in &mut b.dismounts {
                d.abort_ask();
            }
        }
        if b.jobs.is_empty() && b.dismounts.is_empty() && b.dialogs.is_empty() && b.opens.is_empty()
        {
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

/// A question on screen that the broker is waiting on, without waiting for it.
struct Asking {
    child: Child,
    /// Tools not tried yet, for when the running one fails for an environmental
    /// reason instead of being answered.
    rest: Vec<(PathBuf, Vec<String>)>,
}

/// A running `veracage open`, tracked with enough context to re-prompt on a
/// wrong passphrase (helper exit code 4) instead of failing silently.
struct OpenJob {
    child: Child,
    volume: String,      // the volume path handed to `veracage open`
    app: Option<String>, // the config-app key to auto-launch, if any
}

/// A running `veracage close-volume`, and how far its ONE privileged attempt has
/// got. There is exactly one `pkexec` per dismount: when apps hold the volume the
/// helper stays alive waiting for an answer on its stdin, so pushing the dismount
/// through costs no second password (polkit's auth_self_keep is bound to the
/// calling process, and a second attempt would be a second process).
struct Dismount {
    child: Child,
    label: String,
    /// What the helper has said, delivered by a reader thread so the broker never
    /// blocks on the pipe.
    lines: std::sync::mpsc::Receiver<String>,
    /// The helper's stdin: `go` continues the attempt, EOF cancels it.
    stdin: Option<std::process::ChildStdin>,
    /// Set while the apps are being closed on the helper's behalf.
    waiting: Option<AppsWait>,
    /// The "Close <apps> to continue." question, while the human has not answered
    /// it. A CHILD we poll, never a blocking wait: this loop is the only one the
    /// broker has, and blocking it froze every other menu command, every other
    /// dismount and the compositor-went-away check for as long as the dialog
    /// stood there.
    asking: Option<Asking>,
    /// True only when the HUMAN cancelled the close (they answered Cancel to
    /// "Close <apps> to continue"). Every other refusal is reported: they asked
    /// for a close, it did not happen, and the volume is still decrypted.
    cancelled: bool,
    /// The last `busy` reason the helper gave, shown if it ends up refusing.
    refusal: Option<String>,
}

/// Waiting for the apps that hold a volume to be gone.
struct AppsWait {
    /// The `closeapps.done` mtime when we asked, so only a NEW signal counts.
    asked_at: Option<u128>,
    deadline: std::time::Instant,
}

struct Broker {
    /// Spawned CLI children (_sync-apps, xdg-open), left to finish even when the
    /// session ends. Their exit codes carry nothing we act on.
    jobs: Vec<Child>,
    /// Running `veracage close-volume` children: unlike `jobs`, each is a
    /// conversation (see Dismount), not a fire-and-forget.
    dismounts: Vec<Dismount>,
    /// One-shot GUI dialogs (Settings, Configure apps, Help...) with the
    /// subcommand that spawned each, so a second click cannot open a second copy.
    /// Killed when
    /// the compositor goes away, so no window outlives the session.
    dialogs: Vec<(String, Child)>,
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
    /// Pick a volume file, then collect the passphrase and open it. `app` is
    /// the config-app key to auto-launch after the open (an Apps-menu click
    /// with no volume open yet).
    fn open_flow(&mut self, app: Option<String>) {
        let Some(path) = rfd::FileDialog::new()
            .set_title("Select an encrypted volume to open")
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
        // Dismount one volume carries a label: `close-volume:<label>`.
        if let Some(label) = verb.strip_prefix("close-volume:") {
            self.close_volume(label);
            return;
        }
        match verb {
            "open" => self.open_flow(None),
            "configure" => self.spawn_dialog("configure"),
            "settings" => self.spawn_dialog("_settings"),
            "appearance" => self.spawn_dialog("_appearance"),
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

    /// Spawn a one-shot GUI subcommand of ourselves (configure / _settings / ...),
    /// at most ONE of each kind at a time. Each dialog loads the whole config when
    /// it opens and writes the whole config on Save, so two of them (or the same
    /// one twice) silently revert each other: change the theme in Appearance, save,
    /// then save in System Integration, and the theme goes back.
    fn spawn_dialog(&mut self, sub: &str) {
        if self.dialogs.iter().any(|(kind, _)| kind == sub) {
            return; // already open; its window is the one to use
        }
        let exe = std::env::current_exe()
            .unwrap_or_else(|_| PathBuf::from("veracage-agent"));
        match Command::new(exe).arg(sub).spawn() {
            Ok(child) => self.dialogs.push((sub.to_string(), child)),
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

    /// Close ONE volume of the running session (the compositor's Close volume
    /// menu). Both pipes are kept: the helper reports on stdout and waits on
    /// stdin when apps hold the volume (see Dismount).
    fn close_volume(&mut self, label: &str) {
        let Some(bin) = veracage_bin() else { return };
        let child = Command::new(bin)
            .arg("close-volume")
            .arg(label)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn();
        match child {
            Ok(mut child) => {
                let lines = read_lines_in_background(child.stdout.take());
                self.dismounts.push(Dismount {
                    stdin: child.stdin.take(),
                    child,
                    label: label.to_string(),
                    lines,
                    waiting: None,
                    asking: None,
                    cancelled: false,
                    refusal: None,
                });
            }
            Err(e) => eprintln!("veracage: could not close volume {label}: {e}"),
        }
    }

    /// Carry every running dismount forward: read what its helper has said, and
    /// answer it. Cheap: reads that never block, and one mtime per waiting one.
    fn drive_dismounts(&mut self) {
        for d in &mut self.dismounts {
            while let Ok(line) = d.lines.try_recv() {
                d.handle(&line);
            }
            d.drive_ask();
            d.drive_apps_wait();
        }
    }

    /// Reap finished children. A finished `veracage open` is inspected: exit 4
    /// (decrypt failed, wrong passphrase) re-prompts for the same volume; a
    /// polkit cancel (126/127) is silent; any other failure shows an error
    /// dialog instead of failing into the journal only.
    fn reap(&mut self) {
        self.jobs.retain_mut(|c| !matches!(c.try_wait(), Ok(Some(_))));
        self.dialogs.retain_mut(|(_, c)| !matches!(c.try_wait(), Ok(Some(_))));
        let mut retries: Vec<(String, Option<String>)> = Vec::new();
        let mut errors: Vec<String> = Vec::new();
        // A finished dismount. A refusal the human caused is not news: they
        // cancelled, or they were told an app is still running and it still is.
        // Every OTHER refusal is: the volume they asked to close is still
        // decrypted and nothing on screen says so. That includes the case where
        // no dialog ever appeared, which is exactly when they have no way to
        // know. A success speaks for itself: the menu entry goes.
        self.dismounts.retain_mut(|d| match d.child.try_wait() {
            Ok(Some(st)) => {
                // The helper is gone, so an unanswered question about it is stale.
                d.abort_ask();
                d.drain_lines();
                if st.code() == Some(EXIT_VOLUME_BUSY) && !d.cancelled {
                    let why = d.refusal.as_deref().unwrap_or("it is still in use");
                    errors.push(format!("{} was not closed: {why}.", d.label));
                }
                false
            }
            _ => true,
        });
        self.opens.retain_mut(|j| match j.child.try_wait() {
            Ok(Some(st)) => {
                debug_log(&format!("open {} finished: {st}", base(&j.volume)));
                match st.code() {
                    Some(0) | None => {}
                    Some(EXIT_CRYPT_FAILED) => {
                        retries.push((j.volume.clone(), j.app.take()));
                    }
                    Some(EXIT_FSCK_FAILED) => errors.push(format!(
                        "{} was not opened: its filesystem needs a repair.",
                        base(&j.volume)
                    )),
                    Some(126) | Some(127) => {} // polkit auth cancelled / denied
                    Some(c) => errors.push(format!(
                        "Could not open {} (exit {c}).",
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

impl Dismount {
    /// One line from the helper. Two shapes: `holders\t<names>`, meaning it is
    /// waiting for us to clear them, and `busy\t<reason>`, meaning it gave up.
    fn handle(&mut self, line: &str) {
        if let Some(holders) = line.strip_prefix("holders\t") {
            self.ask_to_close_apps(holders);
        } else if let Some(reason) = line.strip_prefix("busy\t") {
            self.refusal = Some(reason.to_string());
        }
    }

    /// Apps hold the volume. One line, one action: closing them is the only way
    /// through, their own unsaved-work prompts still come up in the Veracage
    /// window, and cancelling leaves everything as it is. The question goes up as
    /// a child process; `drive_ask` collects the answer on a later poll.
    fn ask_to_close_apps(&mut self, holders: &str) {
        let msg = format!("Close {} to continue.", holder_app_names(holders));
        self.start_ask(ask_candidates(&msg, "Close", "Cancel"));
    }

    /// Put the question up with the first tool that runs. Nothing runnable means
    /// nobody can answer, so leave the volume as it is.
    fn start_ask(&mut self, mut candidates: Vec<(PathBuf, Vec<String>)>) {
        while !candidates.is_empty() {
            let (bin, args) = candidates.remove(0);
            match Command::new(&bin).args(&args).spawn() {
                Ok(child) => {
                    self.asking = Some(Asking { child, rest: candidates });
                    return;
                }
                Err(e) => eprintln!("veracage: {} did not run: {e}", bin.display()),
            }
        }
        eprintln!("veracage: no dialog tool to ask with; leaving {} as it is.", self.label);
        self.cancelled = true;
        self.cancel();
    }

    /// Collect the answer if it has arrived. Never blocks.
    fn drive_ask(&mut self) {
        enum Answer {
            Yes,
            No,
            ToolFailed(Vec<(PathBuf, Vec<String>)>),
        }
        let answer = match self.asking.as_mut() {
            None => return,
            Some(a) => match a.child.try_wait() {
                Ok(None) => return, // still on screen
                Ok(Some(st)) if st.success() => Answer::Yes,
                // Both tools answer "no" with exit 1. Anything else is the tool
                // failing (a missing Qt platform plugin, an unusable display, a
                // signal), not the human answering, so try the next one.
                Ok(Some(st)) if st.code() == Some(1) => Answer::No,
                Ok(Some(_)) | Err(_) => Answer::ToolFailed(std::mem::take(&mut a.rest)),
            },
        };
        self.asking = None;
        match answer {
            Answer::Yes => {
                self.waiting = Some(AppsWait {
                    asked_at: mtime(Path::new(APPS_CLOSED)),
                    deadline: std::time::Instant::now() + APPS_CLOSED_WAIT,
                });
                request_close_apps();
            }
            Answer::No => {
                self.cancelled = true;
                self.cancel();
            }
            Answer::ToolFailed(rest) => self.start_ask(rest),
        }
    }

    /// Take the question down: the helper it belonged to is gone.
    fn abort_ask(&mut self) {
        if let Some(mut a) = self.asking.take() {
            let _ = a.child.kill();
            let _ = a.child.wait();
        }
    }

    /// Let the waiting helper go on as soon as the leader reports the apps gone,
    /// and cancel it at the deadline (an app's own save prompt left unanswered).
    fn drive_apps_wait(&mut self) {
        let Some(wait) = &self.waiting else { return };
        let now = mtime(Path::new(APPS_CLOSED));
        if now.is_some() && now != wait.asked_at {
            self.waiting = None;
            self.say("go\n");
        } else if std::time::Instant::now() >= wait.deadline {
            eprintln!("veracage: {} stayed open (its apps were never closed).", self.label);
            self.waiting = None;
            self.cancel();
        }
    }

    /// Drop the helper's stdin: the EOF is what tells it to leave the volume
    /// exactly as it is and exit.
    fn cancel(&mut self) {
        self.stdin = None;
    }

    fn say(&mut self, answer: &str) {
        if let Some(mut stdin) = self.stdin.take() {
            let _ = stdin.write_all(answer.as_bytes()); // dropped after: EOF
        }
    }

    /// Collect the helper's last word once it has exited: only a refusal reason
    /// matters then, a `holders` question cannot be answered by a dead process.
    /// The short wait covers a line still in flight from the reader thread.
    fn drain_lines(&mut self) {
        while let Ok(line) = self.lines.recv_timeout(Duration::from_millis(50)) {
            if let Some(reason) = line.strip_prefix("busy\t") {
                self.refusal = Some(reason.to_string());
            }
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

/// Write the app-side font to `PUB_DIR/appfont` (`<family>\n<points>`): the
/// session leader builds the sandbox's kdeglobals from this and `PUB_DIR/theme`,
/// so apps use the configured font and theme instead of their own defaults.
pub fn publish_appfont(cfg: &crate::config::Config) {
    let dir = pub_dir();
    if !dir.is_dir() {
        return;
    }
    let (family, points) = crate::fonts::app_font(&cfg.ui_font, &cfg.ui_font_size);
    let tmp = dir.join(format!("appfont.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, format!("{family}\n{points}\n")).is_ok() {
        let _ = std::fs::rename(&tmp, dir.join("appfont"));
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Write the idle-dismount timeout to `PUB_DIR/autodismount` (minutes, 0 = off).
/// The compositor is the only component that sees whether the human is using
/// Veracage, so it runs the timer and asks for the dismount. Best-effort.
pub fn publish_autodismount(cfg: &crate::config::Config) {
    let dir = pub_dir();
    if !dir.is_dir() {
        return;
    }
    let tmp = dir.join(format!("autodismount.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, format!("{}\n", cfg.auto_dismount)).is_ok() {
        let _ = std::fs::rename(&tmp, dir.join("autodismount"));
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Write the configured theme to `PUB_DIR/theme` ("light" | "dark") so the
/// running compositor repaints its backdrop, menu bar and backdrop hint on its
/// next scan instead of waiting for a restart. Best-effort.
pub fn publish_theme(cfg: &crate::config::Config) {
    let dir = pub_dir();
    if !dir.is_dir() {
        return;
    }
    let tmp = dir.join(format!("theme.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, format!("{}\n", cfg.theme)).is_ok() {
        let _ = std::fs::rename(&tmp, dir.join("theme"));
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Write the keyboard configuration the compositor hands to libxkbcommon to
/// `PUB_DIR/keyboard` (`<model>\n<layout>\n<variant>\n<options>`): the host
/// desktop's own XKB settings, with the configured modifier mapping applied.
/// Best-effort, and an absent file leaves the compositor on libxkbcommon's
/// default layout.
pub fn publish_keyboard(cfg: &crate::config::Config) {
    let dir = pub_dir();
    if !dir.is_dir() {
        return;
    }
    let host = crate::keyboard::host_keyboard();
    let options = crate::keyboard::options_for(&host.options, &cfg.modifier_keys);
    let body = format!("{}\n{}\n{}\n{options}\n", host.model, host.layout, host.variant);
    let tmp = dir.join(format!("keyboard.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, body).is_ok() {
        let _ = std::fs::rename(&tmp, dir.join("keyboard"));
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
    publish_font(&cfg);
    publish_shortcuts(&cfg);
    publish_clipclear(&cfg);
    publish_keyboard(&cfg);
    publish_theme(&cfg);
    publish_appfont(&cfg);
    publish_autodismount(&cfg);
    // The key becomes a file name and a command verb suffix, keep it plain.
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
    // Only a distro-owned kdialog, never one $PATH picked: this program is handed
    // the volume passphrase on its stdout, and a desktop session normally has
    // ~/.local/bin ahead of /usr/bin, so a bare name would let anything that can
    // write there collect the passphrase directly. `veracage_bin` refuses $PATH
    // for exactly this reason. No trusted kdialog means our own dialog, below.
    if let Some(kdialog) = trusted_tool("kdialog") {
        match Command::new(kdialog).arg("--password").arg(&msg).output() {
            Ok(o) if o.status.success() => {
                let mut p = Zeroizing::new(o.stdout);
                while matches!(p.last(), Some(b'\n' | b'\r')) {
                    p.pop();
                }
                return Some(p);
            }
            // kdialog's documented cancel is exit code 1. ONLY that means "the user
            // cancelled" → abort. Any OTHER failure (a missing Qt platform plugin,
            // an unusable display, a killed-by-signal process, code() == None) is an
            // ENVIRONMENTAL failure, not a cancel: fall through to our own egui dialog
            // rather than silently aborting the open.
            Ok(o) if o.status.code() == Some(1) => return None,
            // kdialog printed something before failing: wipe it rather than drop it.
            Ok(o) => drop(Zeroizing::new(o.stdout)),
            Err(_) => {} // not runnable, fall through to egui
        }
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
/// Ask the human a yes/no question and BLOCK for the answer, with captioned
/// buttons so neither reads as "OK". True only on an explicit yes: a dialog that
/// cannot be shown answers no, because the yes branch closes their apps.
/// The yes/no dialogs to try, in order, as (program, argv). Both answer "no"
/// with exit 1, which is what lets a failure be told from an answer.
fn ask_candidates(msg: &str, yes: &str, no: &str) -> Vec<(PathBuf, Vec<String>)> {
    let mut out = Vec::new();
    for (bin, args) in [
        ("kdialog", vec!["--yesno", msg, "--yes-label", yes, "--no-label", no]),
        ("zenity", vec!["--question", "--text", msg, "--ok-label", yes, "--cancel-label", no]),
    ] {
        if let Some(path) = trusted_tool(bin) {
            out.push((path, args.into_iter().map(str::to_string).collect()));
        }
    }
    out
}

/// A GUI helper resolved to an absolute path in a distro-owned directory, never
/// through `$PATH`. `/usr/local/bin` is deliberately absent: it is root-owned on
/// a normal system but is exactly where a hand-installed shim would sit.
fn trusted_tool(name: &str) -> Option<PathBuf> {
    ["/usr/bin", "/bin"]
        .iter()
        .map(|d| PathBuf::from(d).join(name))
        .find(|p| p.is_file())
}

/// Ask the compositor to clear the apps out of the way of a dismount: it asks
/// every app window to close (so unsaved work still gets its prompt) and then
/// has the sessions stop whatever is left. Best-effort file touch, read by the
/// compositor's discovery scan.
fn request_close_apps() {
    let dir = pub_dir();
    if !dir.is_dir() {
        return;
    }
    let tmp = dir.join(format!("closeapps.{}.tmp", std::process::id()));
    let write = || -> std::io::Result<()> {
        std::fs::write(&tmp, b"1\n")?;
        std::fs::rename(&tmp, dir.join("closeapps"))
    };
    if let Err(e) = write() {
        eprintln!("veracage: could not ask the compositor to close the apps: {e}");
        let _ = std::fs::remove_file(&tmp);
    }
}

/// The apps the human would recognise, from the helper's raw list of process
/// names. Everything a KDE app starts for itself - kioslave5, kglobalaccel5,
/// kactivitymanagerd - holds the volume just as hard, but the human never opened
/// those and cannot close them: the thing to close is Dolphin. So the list is
/// matched against the CONFIGURED apps and reported under their own names.
///
/// Falls back to a generic phrase rather than the raw names, which are noise.
fn holder_app_names(raw: &str) -> String {
    let cfg = crate::config::load();
    let mut names: Vec<String> = Vec::new();
    for comm in raw.split(',').map(str::trim).filter(|c| !c.is_empty()) {
        let app = cfg
            .apps
            .iter()
            .find(|a| comm_matches(comm, &crate::config::exec_basename(&a.exec)));
        if let Some(app) = app {
            if !names.contains(&app.name) {
                names.push(app.name.clone());
            }
        }
    }
    if names.is_empty() {
        "Apps inside Veracage".to_string()
    } else {
        names.join(", ")
    }
}

/// Whether a `/proc/<pid>/comm` names the binary `full`. comm is capped at 15
/// characters, so a longer name arrives truncated and only a prefix test finds
/// it ("kactivitymanagerd" appears as "kactivitymanage").
fn comm_matches(comm: &str, full: &str) -> bool {
    full == comm || (comm.len() == 15 && full.starts_with(comm))
}

/// Read a child's stdout line by line on a thread, so the broker's loop picks
/// lines up with `try_recv` instead of blocking on a pipe that stays open for as
/// long as the child lives. Ends at EOF.
fn read_lines_in_background(
    stdout: Option<std::process::ChildStdout>,
) -> std::sync::mpsc::Receiver<String> {
    use std::io::BufRead;
    let (tx, rx) = std::sync::mpsc::channel();
    if let Some(stdout) = stdout {
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
    }
    rx
}

fn show_error(msg: &str) -> Option<Child> {
    for (bin, args) in [
        ("kdialog", vec!["--error", msg]),
        ("zenity", vec!["--error", "--text", msg]),
    ] {
        let Some(path) = trusted_tool(bin) else { continue };
        if let Ok(child) = Command::new(path).args(&args).spawn() {
            return Some(child);
        }
    }
    eprintln!("veracage: {msg}");
    None
}

/// Our own dir under the caller's runtime dir: deliberately NOT `.../veracage`,
/// which the root helper creates (root-owned 0711) for the control socket, where
/// we could not write our pidfile.
///
/// No `/tmp` fallback. `/tmp` is shared and sticky, so another user can
/// pre-create `veracage-agent` and own everything in it: the pidfile we write,
/// and `open.req`, which is how a second launch asks us to raise a passphrase
/// prompt. A desktop session always sets XDG_RUNTIME_DIR, and the root helper
/// already refuses to run without it, so an unset one means something is wrong
/// enough to stop for.
fn human_runtime_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")?;
    Some(PathBuf::from(base).join("veracage-agent"))
}

fn mtime(p: &Path) -> Option<u128> {
    let mt = std::fs::metadata(p).ok()?.modified().ok()?;
    Some(mt.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos())
}

/// Parse one `<nonce>\t<verb>` line of the command log.
fn parse_command(line: &str) -> Option<(u128, String)> {
    let (nonce, verb) = line.split_once('\t')?;
    let nonce: u128 = nonce.trim().parse().ok()?;
    let verb = verb.trim();
    (!verb.is_empty()).then(|| (nonce, verb.to_string()))
}

/// Every command appended after `after`, oldest first. A trailing line without a
/// newline is a write still in flight, so it is left for the next poll rather
/// than dispatched half-read.
fn read_commands(p: &Path, after: Option<u128>) -> Vec<(u128, String)> {
    let Ok(s) = std::fs::read_to_string(p) else {
        return Vec::new();
    };
    let complete = match s.rfind('\n') {
        Some(i) => &s[..=i],
        None => return Vec::new(),
    };
    let mut out: Vec<(u128, String)> = complete
        .lines()
        .filter_map(parse_command)
        .filter(|(nonce, _)| after.is_none_or(|seen| *nonce > seen))
        .collect();
    out.sort_by_key(|(nonce, _)| *nonce);
    out
}

/// The newest nonce already in the log, so a broker starting up steps over the
/// backlog instead of replaying a previous session's menu clicks.
fn last_command_nonce(p: &Path) -> Option<u128> {
    let s = std::fs::read_to_string(p).ok()?;
    s.lines().filter_map(parse_command).map(|(n, _)| n).max()
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

/// The outcome of trying to become THE broker for this session.
enum Instance {
    /// We hold the lock. The file is never read, only HELD: the flock is
    /// released when it drops, so keeping it alive is the whole point.
    Claimed(#[allow(dead_code)] std::fs::File),
    /// Another live broker holds it: the caller should hand the request over.
    Taken,
    /// We could not even try (unwritable dir). Run anyway: a permission problem
    /// must never turn into a silent no-op, which is how "run it, nothing
    /// happens" happened.
    Unknown,
}

/// Become the single broker, atomically. An `flock` on `broker.pid` held for our
/// whole lifetime, then our pid written into it for anything that wants to look.
///
/// The lock IS the claim, rather than "read the pidfile, then unlink and rewrite
/// it": that sequence is a plain race, and two launches milliseconds apart (a
/// double-clicked launcher) both became brokers, after which every menu verb was
/// dispatched twice - two volume pickers, two `pkexec close-volume` for one
/// label. A crashed broker releases its lock in the kernel, so there is no stale
/// state to clean up and no pid-liveness guess to get wrong.
fn claim_single_instance(dir: &Path) -> Instance {
    let pidfile = dir.join("broker.pid");
    let opened = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&pidfile);
    let mut f = match opened {
        Ok(f) => f,
        Err(e) => {
            eprintln!("veracage-agent: can't open {}: {e} (continuing)", pidfile.display());
            return Instance::Unknown;
        }
    };
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Instance::Taken;
    }
    let _ = f.set_len(0);
    let _ = write!(f, "{}", std::process::id());
    let _ = f.flush();
    Instance::Claimed(f)
}

/// Ask the already-running broker to open another volume (bumps a file it polls).
fn signal_open(dir: &Path) {
    let _ = std::fs::write(dir.join("open.req"), b"1");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log(dir: &Path, body: &str) -> PathBuf {
        let p = dir.join("cmd.log");
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn every_verb_survives_a_drain_however_late_it_happens() {
        // The point of the log: two batches written between two polls (or while a
        // dialog kept the loop busy) must BOTH arrive. The old single-slot file
        // kept only the last one, and the earlier verbs vanished with no error.
        let dir = std::env::temp_dir().join(format!("vc-cmd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = log(&dir, "10\tclose-volume:work\n11\tclose-volume:private\n12\thelp\n");

        let cmds = read_commands(&p, None);
        assert_eq!(
            cmds.iter().map(|(_, v)| v.as_str()).collect::<Vec<_>>(),
            ["close-volume:work", "close-volume:private", "help"]
        );

        // Resume: only what came after the last one we handled.
        let cmds = read_commands(&p, Some(11));
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].1, "help");
        assert!(read_commands(&p, Some(12)).is_empty());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_line_still_being_written_waits_for_the_next_poll() {
        let dir = std::env::temp_dir().join(format!("vc-cmd-partial-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // No trailing newline on the last line: the append is in flight.
        let p = log(&dir, "10\thelp\n11\tclose-vol");
        let cmds = read_commands(&p, None);
        assert_eq!(cmds.len(), 1, "the partial line must not be dispatched");
        assert_eq!(cmds[0].1, "help");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_new_broker_steps_over_the_backlog() {
        let dir = std::env::temp_dir().join(format!("vc-cmd-backlog-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = log(&dir, "10\thelp\n99\tabout\ngarbage\n\n");
        assert_eq!(last_command_nonce(&p), Some(99));
        assert!(read_commands(&p, last_command_nonce(&p)).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
