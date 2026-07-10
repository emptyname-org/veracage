//! Veracage privileged helper — UID-isolation model.
//!
//! Invoked as root via pkexec. Opens an encrypted volume (LUKS or VeraCrypt,
//! file or block device) and presents it, via an *idmapped mount*, as owned by
//! a dedicated *vault* system user, then runs the vault-side leader as that
//! user. The decrypted data is owned by a uid the human cannot assume, and the
//! mount lives in a private namespace — denied AND hidden from host processes.
//!
//! Child:
//!   1. unshare(CLONE_NEWNS), make / rslave
//!   2. cryptsetup open SOURCE (LUKS|VeraCrypt) -> /dev/mapper/veracage-XXXX
//!   3. plain-mount it at <mountpoint>.raw, read its on-disk owner
//!   4. idmap-mount it at MOUNTPOINT, presenting that owner as the vault uid
//!   5. unmount the staging mount
//!   6. drop privileges to the vault uid/gid; exec the (pinned) continuation
//! Parent: waitpid, cryptsetup close, tidy mountpoint + lock.
//!
//! SECURITY: reachable by any active local user via pkexec. It does NOT trust
//! caller-supplied identity or code paths: the human uid comes from PKEXEC_UID,
//! the *vault* uid is resolved from a fixed system user (not argv), the
//! continuation is pinned at build time, forwarded env is allowlisted, and the
//! mountpoint must be a direct child of the canonical /run/veracage.

use std::env;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::ptr;
use std::sync::atomic::{AtomicI32, Ordering};

mod crypt;
mod idmap;
mod ipc;

use crypt::Backend;

/// The dedicated system user the vault is presented as, and the vault-side
/// leader/apps run as. Resolved at runtime (never from argv); created by
/// `make install`.
const VAULT_USER: &str = "veracage";

/// Shared runtime dir for the ONE persistent compositor (Phase 2). Root-created,
/// veracage-owned, mode 0711 (the human can traverse to stat the socket/pidfile
/// for a liveness check, but only veracage-uid apps can connect). Holds the fixed
/// `wl-vc` Wayland socket and `compositor.pid`.
const COMPOSITOR_RUNTIME: &str = "/run/veracage/rt";

/// Shared-workspace model (docs/shared-workspace-redesign.md, Phase 2): the ONE
/// session's private mount NS holds every open volume under this tmpfs, each at
/// `<WORKSPACE>/<label>`. A tmpfs so the whole tree (and every idmap mount on it)
/// vanishes when the session leader — which holds the NS — dies; only the global
/// dm devices survive and are closed from the session lock.
const WORKSPACE: &str = "/run/veracage/vaults";

/// Env vars the launcher may forward into the (unprivileged) continuation.
/// pkexec strips the environment; anything outside this set is dropped, so a
/// direct pkexec caller can't smuggle e.g. LD_PRELOAD/LD_LIBRARY_PATH through.
const ALLOWED_SETENV: &[&str] = &[
    "WAYLAND_DISPLAY",
    "XDG_RUNTIME_DIR",
    "XDG_SESSION_TYPE",
    "DISPLAY",
    "LANG",
    "VERACAGE_THEME",
];

/// Trusted continuation, pinned at build time (the Makefile passes
/// VERACAGE_CONTINUATION=<prefix>/bin/veracage). Never caller-controlled.
fn continuation() -> &'static str {
    option_env!("VERACAGE_CONTINUATION").unwrap_or("/usr/local/bin/veracage")
}

/// The continuation binary is pinned/trusted, but its FIRST argument selects what
/// it does. Restrict it to the two internal subcommands the helper is meant to
/// launch, so a direct `pkexec veracage-helper … -- <other subcommand>` can't
/// reach arbitrary `veracage` subcommands as the vault uid.
fn check_continuation_argv(rest: &[String]) {
    match rest.first().map(String::as_str) {
        Some("_leader") | Some("_compositor") => {}
        other => fail(
            &format!("refusing continuation subcommand {other:?}; only _leader/_compositor allowed"),
            2,
        ),
    }
}

/// Resolve an external tool to an absolute path from a fixed set of trusted
/// system directories rather than trusting `$PATH`. pkexec already sanitizes
/// PATH, so this is defense-in-depth (and distro-robust vs. a hard-coded path).
pub(crate) fn tool(name: &str) -> String {
    for dir in ["/usr/sbin", "/sbin", "/usr/bin", "/bin", "/usr/local/sbin", "/usr/local/bin"] {
        let p = format!("{dir}/{name}");
        if Path::new(&p).is_file() {
            return p;
        }
    }
    name.to_string() // fall back to the (pkexec-sanitized) PATH
}

/// True iff `arg` is exactly /run/veracage/<one-non-empty-component>. Pure
/// (no filesystem) so it can be unit-tested. Defeats `..` traversal and the
/// bare-directory case that a component *prefix* check would wrongly allow.
fn mountpoint_ok(arg: &str) -> bool {
    let mut c = Path::new(arg).components();
    matches!(c.next(), Some(Component::RootDir))
        && c.next().map(|x| x.as_os_str().as_bytes()) == Some(&b"run"[..])
        && c.next().map(|x| x.as_os_str().as_bytes()) == Some(&b"veracage"[..])
        && matches!(c.next(), Some(Component::Normal(n)) if !n.is_empty())
        && c.next().is_none()
}

/// Resolve the validated mountpoint under the *canonical* /run/veracage.
fn validated_mountpoint(arg: &str) -> Result<PathBuf, String> {
    if !mountpoint_ok(arg) {
        return Err(format!("mountpoint must be /run/veracage/<name>: {arg}"));
    }
    let name = Path::new(arg)
        .file_name()
        .ok_or_else(|| "mountpoint has no final component".to_string())?;
    let parent = std::fs::canonicalize("/run/veracage")
        .map_err(|e| format!("/run/veracage: {e}"))?;
    Ok(parent.join(name))
}

fn fail(msg: &str, code: i32) -> ! {
    eprintln!("veracage-helper: {msg}");
    std::process::exit(code);
}

fn fail_errno(what: &str) -> ! {
    fail(&format!("{what}: {}", std::io::Error::last_os_error()), 1);
}

struct Args {
    /// `--spawn-compositor`: bring up the ONE persistent compositor instead of
    /// mounting a vault. No --source/--mountpoint needed in this mode.
    spawn_compositor: bool,
    source: Option<String>,
    backend: Option<Backend>, // None = auto-detect
    mountpoint: Option<String>,
    /// `--session <id>`: shared-workspace bootstrap. When set, the helper mounts
    /// the volume at `<WORKSPACE>/<label>` in a tmpfs workspace and holds it via
    /// the session leader (Phase 2), writing `session-<id>.lock`/`.pid`. When
    /// unset, the legacy per-vault path runs (own NS, own `/run/veracage/<hash>`).
    session: Option<String>,
    /// `--exchange <dir>`: a human-owned host directory to idmap-mount into the
    /// sandbox as a shared exchange folder. Caller-supplied, so validated (owner
    /// == human, real dir, no final-component symlink) before root touches it.
    exchange: Option<String>,
    setenv: Vec<String>,
    rest: Vec<String>,
    /// Volume passphrase read from stdin (`--passphrase-stdin`), newline-stripped.
    /// Set by the GUI launcher (no tty for cryptsetup to prompt on); None means
    /// cryptsetup prompts interactively (the terminal CLI path).
    passphrase: Option<Vec<u8>>,
}

/// Parse argv. Only --spawn-compositor/--source/--backend/--mountpoint/--setenv
/// are accepted before `--`; anything else is an error (notably no
/// --user/--continuation). Presence of required args is validated per-mode in
/// main(), not here.
fn parse_args() -> Args {
    let mut spawn_compositor = false;
    let mut source: Option<String> = None;
    let mut backend: Option<Backend> = None;
    let mut mountpoint: Option<String> = None;
    let mut session: Option<String> = None;
    let mut exchange: Option<String> = None;
    let mut setenv: Vec<String> = Vec::new();
    let mut rest: Vec<String> = Vec::new();
    let mut passphrase: Option<Vec<u8>> = None;
    let mut it = env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--spawn-compositor" => spawn_compositor = true,
            "--passphrase-stdin" => {
                use std::io::Read;
                let mut buf = Vec::new();
                std::io::stdin().read_to_end(&mut buf).ok();
                // --key-file=- takes ALL of stdin as the key, so strip trailing
                // newlines the pipe/GUI may append (see crypt::open).
                while matches!(buf.last(), Some(b'\n' | b'\r')) {
                    buf.pop();
                }
                passphrase = Some(buf);
            }
            "--source" => source = it.next(),
            "--backend" => {
                match it.next().as_deref() {
                    Some("auto") | None => backend = None,
                    Some(v) => {
                        backend = Some(Backend::parse(v).unwrap_or_else(|| {
                            fail(&format!("unknown backend: {v}"), 2)
                        }))
                    }
                }
            }
            "--mountpoint" => mountpoint = it.next(),
            "--session" => session = it.next(),
            "--exchange" => exchange = it.next(),
            "--setenv" => {
                if let Some(kv) = it.next() {
                    setenv.push(kv);
                }
            }
            "--" => {
                rest.extend(it.by_ref());
                break;
            }
            other => fail(&format!("unexpected argument: {other}"), 2),
        }
    }
    Args { spawn_compositor, source, backend, mountpoint, session, exchange, setenv, rest, passphrase }
}

/// (uid, gid) of the invoking human, taken from PKEXEC_UID — never from argv.
fn resolve_caller() -> Result<(u32, u32), String> {
    let raw = env::var("PKEXEC_UID")
        .map_err(|_| "PKEXEC_UID not set; refusing to run outside pkexec".to_string())?;
    let uid: u32 = raw
        .parse()
        .map_err(|_| format!("PKEXEC_UID not numeric: {raw}"))?;
    if uid == 0 {
        return Err("refusing to run for a root caller".into());
    }
    let gid = unsafe {
        let pw = libc::getpwuid(uid);
        if pw.is_null() {
            return Err(format!("unknown caller uid {uid}"));
        }
        (*pw).pw_gid
    };
    Ok((uid, gid))
}

/// (uid, gid) of the dedicated vault system user — resolved by NAME, never
/// from argv, so the caller can't choose to run as their own (or root's) uid.
fn resolve_vault_user(human_uid: u32) -> Result<(u32, u32), String> {
    let name = CString::new(VAULT_USER).unwrap();
    let pw = unsafe { libc::getpwnam(name.as_ptr()) };
    if pw.is_null() {
        return Err(format!(
            "system user '{VAULT_USER}' not found; run `make install` to create it"
        ));
    }
    let (uid, gid) = unsafe { ((*pw).pw_uid, (*pw).pw_gid) };
    if uid == 0 {
        return Err(format!("vault user '{VAULT_USER}' must not be uid 0"));
    }
    if uid == human_uid {
        return Err(format!("vault user '{VAULT_USER}' must differ from the caller"));
    }
    Ok((uid, gid))
}

/// Look up a forwarded `--setenv KEY=VALUE` value.
fn setenv_value(args: &Args, key: &str) -> Option<String> {
    for kv in &args.setenv {
        if let Some((k, v)) = kv.split_once('=') {
            if k == key {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// 16-hex-char hash matching cleanup.vault_hash (sha256(path)[:16]).
fn vault_hash(p: &Path) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(p.as_os_str().as_bytes()); // raw bytes — matches Python on UTF-8 paths
    h.finalize().iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// "veracage-" + 12 lowercase hex, matching DM_NAME_RE in cleanup.py.
fn random_dm_name() -> String {
    use std::io::Read;
    let mut buf = [0u8; 6];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .unwrap_or_else(|e| fail(&format!("/dev/urandom: {e}"), 1));
    let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    format!("veracage-{hex}")
}

fn set_mode<P: AsRef<Path>>(p: P, mode: u32) {
    let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode));
}

fn chown(p: &Path, uid: u32, gid: u32) {
    let c = CString::new(p.as_os_str().as_bytes()).unwrap();
    if unsafe { libc::chown(c.as_ptr(), uid, gid) } != 0 {
        fail_errno("chown");
    }
}

/// Best-effort chown: warn, never abort. Used to make the vault root writable
/// without failing the open on a read-only or already-correct volume.
fn try_chown(p: &Path, uid: u32, gid: u32) {
    let c = CString::new(p.as_os_str().as_bytes()).unwrap();
    if unsafe { libc::chown(c.as_ptr(), uid, gid) } != 0 {
        eprintln!(
            "veracage-helper: chown {} (vault stays read-only): {}",
            p.display(),
            std::io::Error::last_os_error()
        );
    }
}

fn is_executable(p: &Path) -> bool {
    p.is_file()
        && std::fs::metadata(p)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

fn write_lock(path: &str, dm_name: &str, mountpoint: &Path, uid: u32) {
    let body = format!(
        "dm_name={dm_name}\nmountpoint={}\nuser_uid={uid}\n",
        mountpoint.display()
    );
    // Clear any stale lock (the dir is root-only-writable), then create fresh
    // with O_EXCL|O_NOFOLLOW so a pre-planted symlink/file can't redirect this
    // root write. Mode 0600 is set at creation (no separate chmod to fail).
    let _ = std::fs::remove_file(path);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .unwrap_or_else(|e| fail(&format!("create lock {path}: {e}"), 1));
    use std::io::Write;
    f.write_all(body.as_bytes())
        .unwrap_or_else(|e| fail(&format!("write lock {path}: {e}"), 1));
}

/// Record the compositor pid (which, because we `exec` rather than fork, is this
/// process's own pid) so the human side can check liveness without connecting to
/// the veracage-only socket. 0644 so the human can read it; veracage-owned.
fn write_pidfile(path: &Path, uid: u32, gid: u32) {
    let _ = std::fs::remove_file(path);
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(mut f) => {
            use std::io::Write;
            let _ = f.write_all(format!("{}\n", std::process::id()).as_bytes());
            try_chown(path, uid, gid);
        }
        Err(e) => eprintln!("veracage-helper: pidfile {}: {e}", path.display()),
    }
}

// --------------------------------------------------- shared-workspace ------

/// A 16-char session id: `[0-9]` only (the CLI passes the human uid; the field
/// names `session-<id>.lock`/`.pid`, so reject anything that could traverse).
fn session_id_ok(sid: &str) -> bool {
    !sid.is_empty() && sid.len() <= 16 && sid.bytes().all(|b| b.is_ascii_digit())
}

/// Sanitize a volume label into a single safe path component for `<WORKSPACE>/…`
/// (mirrors leader.py `_sanitize_label`): keep `[A-Za-z0-9._-]`, map the rest to
/// `_`, cap length, and fall back to a hash for empty / `.` / `..`.
fn sanitize_label(label: &str, source: &Path) -> String {
    let mut s: String = label
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' })
        .collect();
    s.truncate(64);
    let trimmed = s.trim_matches(['.', '_', '-']);
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        return format!("vault-{}", vault_hash(source));
    }
    trimmed.to_string()
}

/// The per-volume mountpoint under the workspace tmpfs: `<WORKSPACE>/<label>`,
/// with a `-2`, `-3`… suffix if that directory already exists (a second volume
/// with the same label). Called inside the session NS, after the tmpfs is up.
fn workspace_path(label: &str, source: &Path) -> PathBuf {
    let base = sanitize_label(label, source);
    let root = Path::new(WORKSPACE);
    let first = root.join(&base);
    if !first.exists() {
        return first;
    }
    (2..1000)
        .map(|n| root.join(format!("{base}-{n}")))
        .find(|p| !p.exists())
        .unwrap_or(first)
}

/// Append a volume to the session lock (`session-<sid>.lock`, root-owned 0600):
/// `user_uid=<uid>` once, then a `volume=<dm_name>\t<label>` line per open
/// volume. cleanup.py `cleanup_session` walks these to close every dm on teardown.
/// Written BEFORE the mount (with the dm_name we pre-generated) so an early crash
/// still leaves the ExecStopPost a device to close.
fn write_session_lock(sid: &str, dm_name: &str, label: &str, uid: u32) {
    let path = format!("/run/veracage/session-{sid}.lock");
    use std::io::Write;
    // Create-or-append: the first volume creates it (with the uid header); later
    // volumes (Phase 3, via setns) append their own `volume=` line.
    let fresh = !Path::new(&path).exists();
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .unwrap_or_else(|e| fail(&format!("session lock {path}: {e}"), 1));
    if fresh {
        let _ = f.write_all(format!("user_uid={uid}\n").as_bytes());
    }
    f.write_all(format!("volume={dm_name}\t{label}\n").as_bytes())
        .unwrap_or_else(|e| fail(&format!("write session lock {path}: {e}"), 1));
}

/// Record the session leader's pid (`session-<sid>.pid`, root-owned 0600) so the
/// add-volume path (Phase 3) can find + verify the NS holder. Root-only: the pid
/// is only consumed by another root (pkexec) helper, never the human side.
fn write_session_pidfile(sid: &str, pid: i32) {
    let path = format!("/run/veracage/session-{sid}.pid");
    let _ = std::fs::remove_file(&path);
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
    {
        Ok(mut f) => {
            use std::io::Write;
            let _ = f.write_all(format!("{pid}\n").as_bytes());
        }
        Err(e) => eprintln!("veracage-helper: session pidfile {path}: {e}"),
    }
}

/// Bring up the ONE persistent `veracage`-uid compositor. Unlike the vault path
/// there is no crypt/mount/idmap and no fork: connect the host Wayland fd, prep
/// the shared runtime dir, drop to the vault uid, and exec the pinned
/// continuation (which execs the compositor). The surviving process IS the
/// compositor, so the human's systemd/pkexec chain tracks it directly.
/// Liveness of the shared compositor (mirrors wayland.py::compositor_is_up): the
/// recorded pid is alive AND its socket exists. Both, so a stale pidfile or a
/// half-started compositor reads as down.
fn compositor_is_up() -> bool {
    let rt = Path::new(COMPOSITOR_RUNTIME);
    let pid = std::fs::read_to_string(rt.join("compositor.pid"))
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok());
    match pid {
        Some(p) => {
            // Identity-check the pid so a reused pid (the compositor crashed and
            // its number was recycled) can't fool us into skipping the spawn and
            // leaving the leader with a dead socket. comm is truncated to 15
            // chars: "veracage-compositor" -> "veracage-compos".
            let comm = std::fs::read_to_string(format!("/proc/{p}/comm")).unwrap_or_default();
            comm.trim_end().starts_with("veracage") && rt.join("wl-vc").exists()
        }
        None => false,
    }
}

fn spawn_compositor(vault_uid: u32, vault_gid: u32, args: &Args, cont_rest: &[String]) -> ! {
    let rt = Path::new(COMPOSITOR_RUNTIME);
    std::fs::create_dir_all(rt).unwrap_or_else(|e| fail(&format!("mkdir {}: {e}", rt.display()), 1));
    set_mode(rt, 0o711);
    chown(rt, vault_uid, vault_gid);

    // Connect the host compositor. The fd is inherited across exec (cloexec
    // cleared) and its number is handed to the compositor as WAYLAND_SOCKET —
    // the same mechanism the vault leader uses for its nested output.
    let runtime = setenv_value(args, "XDG_RUNTIME_DIR")
        .unwrap_or_else(|| fail("XDG_RUNTIME_DIR not forwarded", 2));
    let disp = setenv_value(args, "WAYLAND_DISPLAY")
        .unwrap_or_else(|| fail("WAYLAND_DISPLAY not forwarded; need it to reach the host", 2));
    // Must be a bare socket NAME (the normal form, e.g. "wayland-0"), resolved
    // under the runtime dir. Reject any '/' so a caller can't make root connect()
    // to an arbitrary unix socket outside the runtime dir.
    if disp.contains('/') {
        fail(&format!("WAYLAND_DISPLAY must be a bare name, got {disp:?}"), 2);
    }
    let wl_path = Path::new(&runtime).join(disp);
    let wl_fd = ipc::connect_host_wayland(&wl_path)
        .unwrap_or_else(|e| fail(&format!("host wayland connect ({}): {e}", wl_path.display()), 1));

    // Clear any stale socket left by a crashed compositor, then record our pid
    // (survives exec) for the liveness check.
    let _ = std::fs::remove_file(rt.join("wl-vc"));
    write_pidfile(&rt.join("compositor.pid"), vault_uid, vault_gid);

    // Drop privileges to the vault uid (gid first, then uid).
    unsafe {
        if libc::setgroups(0, ptr::null()) != 0 {
            fail_errno("setgroups");
        }
        if libc::setresgid(vault_gid, vault_gid, vault_gid) != 0 {
            fail_errno("setresgid");
        }
        if libc::setresuid(vault_uid, vault_uid, vault_uid) != 0 {
            fail_errno("setresuid");
        }
    }

    // Allowlisted env pkexec stripped, then force the compositor's runtime dir
    // (veracage-writable; the human's isn't reachable by the vault uid) and the
    // inherited host fd.
    for kv in &args.setenv {
        if let Some((k, v)) = kv.split_once('=') {
            if ALLOWED_SETENV.contains(&k) {
                env::set_var(k, v);
            }
        }
    }
    env::set_var("XDG_RUNTIME_DIR", COMPOSITOR_RUNTIME);
    env::remove_var("WAYLAND_DISPLAY"); // force the fd, not a path lookup
    env::set_var("WAYLAND_SOCKET", wl_fd.to_string());

    // Force a private umask so the compositor's listening socket is created 0700
    // (owner-only connect) regardless of the inherited umask. A permissive umask
    // (e.g. 000 -> a 0777 socket) would otherwise let a non-veracage process
    // connect and snoop/inject the sandbox selection or input. Set here, after
    // the pidfile write (which must stay 0644-readable by the human), so it only
    // affects what the compositor creates.
    unsafe { libc::umask(0o077) };

    let cont = PathBuf::from(continuation());
    if !is_executable(&cont) {
        fail(&format!("continuation not executable: {}", cont.display()), 2);
    }
    check_continuation_argv(cont_rest);
    let err = Command::new(&cont).args(cont_rest).exec();
    fail(&format!("exec continuation {}: {err}", cont.display()), 127);
}

static CHILD_PID: AtomicI32 = AtomicI32::new(0);

extern "C" fn forward_signal(sig: i32) {
    let pid = CHILD_PID.load(Ordering::SeqCst);
    if pid > 0 {
        unsafe {
            libc::kill(pid, sig);
        }
    }
}

fn install_signal_forwarding(pid: i32) {
    CHILD_PID.store(pid, Ordering::SeqCst);
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        unsafe {
            libc::signal(sig, forward_signal as usize as libc::sighandler_t);
        }
    }
}

fn wait_for(pid: i32) -> i32 {
    let mut status: i32 = 0;
    loop {
        let r = unsafe { libc::waitpid(pid, &mut status, 0) };
        if r == -1 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return 1;
        }
        break;
    }
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        1
    }
}

/// The filesystem label of the decrypted volume (`blkid` on the dm device), or
/// None if it has none. Used to name the vault in the sandbox file manager.
fn read_volume_label(dm_path: &str) -> Option<String> {
    let out = Command::new(tool("blkid"))
        .args(["-o", "value", "-s", "LABEL", dm_path])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Child path: new mount NS, open + idmap-mount the vault as the vault uid,
/// drop to the vault uid, exec the continuation.
#[allow(clippy::too_many_arguments)]
/// Validate a caller-supplied exchange directory before root idmap-mounts it.
/// Opens it `O_DIRECTORY|O_NOFOLLOW` (a final-component symlink fails) and requires
/// a directory owned by the human. Intermediate symlinks aren't fully chased, but
/// the owner check rejects anything resolving to a root/other-owned dir (e.g.
/// `--exchange /etc`); a symlink to another of the human's OWN dirs is harmless
/// (their own data, and a same-uid attacker already has it).
fn validated_exchange(path: &str, human_uid: u32) -> Option<PathBuf> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
        .ok()?;
    let md = f.metadata().ok()?;
    (md.is_dir() && md.uid() == human_uid).then(|| PathBuf::from(path))
}

fn child(
    vault_uid: u32,
    vault_gid: u32,
    human_uid: u32,
    human_gid: u32,
    source: &Path,
    backend: Option<Backend>,
    mountpoint: &Path,
    raw: &Path,
    vault_run: &Path,
    ctl_path: &Path,
    dm_name: &str,
    cont: &Path,
    args: &Args,
) -> ! {
    if unsafe { libc::unshare(libc::CLONE_NEWNS) } != 0 {
        fail_errno("unshare(CLONE_NEWNS)");
    }
    let root = CString::new("/").unwrap();
    if unsafe {
        libc::mount(ptr::null(), root.as_ptr(), ptr::null(), libc::MS_REC | libc::MS_SLAVE, ptr::null())
    } != 0
    {
        fail_errno("mount --make-rslave /");
    }

    // Open the encrypted volume. With --passphrase-stdin (the GUI launcher) the
    // passphrase comes from stdin; otherwise cryptsetup prompts on the tty.
    let backend = backend.unwrap_or_else(|| crypt::detect(source));
    crypt::open(source, backend, dm_name, args.passphrase.as_deref())
        .unwrap_or_else(|e| fail(&format!("{e}"), 1));
    let dm_path = format!("/dev/mapper/{dm_name}");

    // The volume's own filesystem label (or the vault's file name if unlabeled),
    // passed to the leader so the sandbox file manager shows it as a named place.
    let volume_label = read_volume_label(&dm_path).unwrap_or_else(|| {
        source
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Vault".into())
    });

    // Stage-mount the decrypted fs so we can read its on-disk owner and clone
    // it into an idmapped mount.
    std::fs::create_dir_all(raw).unwrap_or_else(|e| fail(&format!("mkdir staging: {e}"), 1));
    set_mode(raw, 0o700);
    let st = Command::new(tool("mount"))
        .args(["-o", "nodev,nosuid", &dm_path])
        .arg(raw)
        .status()
        .unwrap_or_else(|e| fail(&format!("spawn mount: {e}"), 1));
    if !st.success() {
        let _ = crypt::close(dm_name);
        fail(&format!("mount failed: {}", st.code().unwrap_or(-1)), st.code().unwrap_or(1));
    }

    // Make the vault writable to the vault uid. The idmap below maps only the
    // human uid -> vault uid, but a freshly-mkfs'd volume's root dir is
    // root-owned, so /vault would appear as "nobody" and be read-only. chown
    // ONLY the root inode (non-recursive) to the human uid so the idmap
    // presents it as vault-uid-owned and writable. This is the *single*
    // automatic on-disk change we make; the app's config/cache/rc live on a
    // tmpfs, never the volume. Best-effort so a genuinely read-only volume
    // still opens.
    try_chown(raw, human_uid, human_gid);

    // Present the vault as the vault uid via an idmapped mount. Map the HUMAN's
    // uid/gid -> the vault uid/gid: a single-user vault's data is owned by the
    // human who created it, so that is the owner that must become readable as
    // the vault uid. (Files owned by *other* uids inside the vault remain
    // inaccessible — single-user limitation.)
    std::fs::create_dir_all(mountpoint).unwrap_or_else(|e| fail(&format!("mkdir mountpoint: {e}"), 1));
    set_mode(mountpoint, 0o700);
    if let Err(e) = idmap::idmap_mount(raw, mountpoint, human_uid, human_gid, vault_uid, vault_gid, 0) {
        let _ = Command::new(tool("umount")).arg(raw).status();
        let _ = crypt::close(dm_name);
        fail(&format!("idmap_mount: {e}"), 1);
    }

    // The idmapped clone is independent; drop the staging mount.
    let _ = Command::new(tool("umount")).arg(raw).status();
    let _ = std::fs::remove_dir(raw);

    // Optional shared EXCHANGE folder: idmap a human-owned host directory into
    // this private NS (nosuid/nodev/noexec), presenting it to the sandbox as
    // veracage-owned and reverse-mapping the sandbox's writes back to the human on
    // disk — a dialog-free host<->vault shared folder (docs/single-window-ux.md).
    // The path is caller-supplied, so validate it (owner == human, real dir, no
    // final-component symlink) before root open_trees it; a same-uid host attacker
    // owns their own home, so this stops `--exchange /etc` / a symlink to a
    // root-owned dir. Soft-fail: any problem just means no /exchange this session.
    if let Some(xpath) = args.exchange.as_deref() {
        match validated_exchange(xpath, human_uid) {
            Some(x) => {
                let xmp = PathBuf::from(format!("{}.x", mountpoint.display()));
                let _ = std::fs::create_dir_all(&xmp);
                set_mode(&xmp, 0o700);
                match idmap::idmap_mount(
                    &x, &xmp, human_uid, human_gid, vault_uid, vault_gid,
                    idmap::ATTR_NOSUID_NODEV_NOEXEC,
                ) {
                    Ok(()) => env::set_var("VERACAGE_EXCHANGE", &xmp),
                    Err(e) => eprintln!("veracage: exchange idmap failed ({e}); no /exchange"),
                }
            }
            None => eprintln!("veracage: exchange {xpath:?} is not a human-owned directory; skipping"),
        }
    }

    // Past pkexec, the helper is the only place that can hand the leader its
    // control socket (pkexec closes inherited fds). The host-Wayland connection
    // now belongs to the persistent compositor (spawned separately), not the
    // per-vault leader — so there is no wl_fd to pass here anymore.
    let ctl_fd = ipc::create_control_socket(ctl_path, human_uid, human_gid)
        .unwrap_or_else(|e| fail(&format!("control socket: {e}"), 1));

    // Provision a vault-writable runtime dir (bwrap /run/user) — the vault uid
    // can't write under root-owned /run/veracage.
    std::fs::create_dir_all(vault_run).unwrap_or_else(|e| fail(&format!("mkdir vault-run: {e}"), 1));
    set_mode(vault_run, 0o700);
    chown(vault_run, vault_uid, vault_gid);

    // Drop privileges to the vault uid (gid first, then uid).
    unsafe {
        if libc::setgroups(0, ptr::null()) != 0 {
            fail_errno("setgroups");
        }
        if libc::setresgid(vault_gid, vault_gid, vault_gid) != 0 {
            fail_errno("setresgid");
        }
        if libc::setresuid(vault_uid, vault_uid, vault_uid) != 0 {
            fail_errno("setresuid");
        }
    }

    // Re-establish the handful of allowlisted env vars pkexec stripped.
    for kv in &args.setenv {
        if let Some((k, v)) = kv.split_once('=') {
            if ALLOWED_SETENV.contains(&k) {
                env::set_var(k, v);
            }
        }
    }

    // Tell the leader which inherited control fd to accept on (the number isn't
    // secret; the fd is inherited and the vault uid can't re-open the socket).
    env::set_var("VERACAGE_CONTROL_FD", ctl_fd.to_string());
    env::set_var("VERACAGE_VAULT_RUNTIME", vault_run);
    env::set_var("VERACAGE_VOLUME_LABEL", &volume_label);

    // Hand off to the pinned continuation (replaces this process).
    check_continuation_argv(&args.rest);
    let err = Command::new(cont).args(&args.rest).exec();
    fail(&format!("exec continuation {}: {err}", cont.display()), 127);
}

/// Shared-workspace bootstrap child (Phase 2). Like `child`, but instead of a
/// per-vault `/run/veracage/<hash>` mountpoint it stands up the session's tmpfs
/// **workspace** and lands the first volume at `<WORKSPACE>/<label>`. The process
/// becomes the session leader and holds this namespace for the session's life
/// (Phase 3 `setns`es into it to add more volumes). The staging `.raw` mount and
/// every `<WORKSPACE>/*` idmap mount live on the tmpfs and vanish with the NS —
/// only the global dm device must be closed (from the session lock) on teardown.
#[allow(clippy::too_many_arguments)]
fn session_child(
    vault_uid: u32,
    vault_gid: u32,
    human_uid: u32,
    human_gid: u32,
    source: &Path,
    backend: Option<Backend>,
    sid: &str,
    dm_name: &str,
    ctl_path: &Path,
    cont: &Path,
    args: &Args,
) -> ! {
    if unsafe { libc::unshare(libc::CLONE_NEWNS) } != 0 {
        fail_errno("unshare(CLONE_NEWNS)");
    }
    let root = CString::new("/").unwrap();
    if unsafe {
        libc::mount(ptr::null(), root.as_ptr(), ptr::null(), libc::MS_REC | libc::MS_SLAVE, ptr::null())
    } != 0
    {
        fail_errno("mount --make-rslave /");
    }

    // The workspace tmpfs: all volumes live here, private to this NS, gone when
    // the leader dies. mode=0755 so the vault uid can traverse into its volumes.
    let ws = CString::new(WORKSPACE).unwrap();
    let tmpfs = CString::new("tmpfs").unwrap();
    let data = CString::new("mode=0755").unwrap();
    if unsafe { libc::mount(tmpfs.as_ptr(), ws.as_ptr(), tmpfs.as_ptr(), 0, data.as_ptr() as *const _) } != 0 {
        fail_errno(&format!("mount tmpfs {WORKSPACE}"));
    }

    let backend = backend.unwrap_or_else(|| crypt::detect(source));
    crypt::open(source, backend, dm_name, args.passphrase.as_deref())
        .unwrap_or_else(|e| fail(&format!("{e}"), 1));
    let dm_path = format!("/dev/mapper/{dm_name}");

    let volume_label = read_volume_label(&dm_path).unwrap_or_else(|| {
        source.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Vault".into())
    });
    // Where this volume lands in the workspace (deduped). Computed here, INSIDE
    // the NS, because the label is only known after cryptsetup — so the leader is
    // told the path by injecting it into the continuation argv below.
    let mountpoint = workspace_path(&volume_label, source);
    let raw = PathBuf::from(format!("{}.raw", mountpoint.display()));
    let vault_run = PathBuf::from(format!("/run/veracage/session-{sid}.run"));

    // Stage-mount the decrypted fs to read its on-disk owner, then idmap-clone it
    // to the workspace path (identical dance to the per-vault `child`).
    std::fs::create_dir_all(&raw).unwrap_or_else(|e| fail(&format!("mkdir staging: {e}"), 1));
    set_mode(&raw, 0o700);
    let st = Command::new(tool("mount"))
        .args(["-o", "nodev,nosuid", &dm_path])
        .arg(&raw)
        .status()
        .unwrap_or_else(|e| fail(&format!("spawn mount: {e}"), 1));
    if !st.success() {
        let _ = crypt::close(dm_name);
        fail(&format!("mount failed: {}", st.code().unwrap_or(-1)), st.code().unwrap_or(1));
    }
    try_chown(&raw, human_uid, human_gid);

    std::fs::create_dir_all(&mountpoint).unwrap_or_else(|e| fail(&format!("mkdir mountpoint: {e}"), 1));
    set_mode(&mountpoint, 0o755);
    if let Err(e) = idmap::idmap_mount(&raw, &mountpoint, human_uid, human_gid, vault_uid, vault_gid, 0) {
        let _ = Command::new(tool("umount")).arg(&raw).status();
        let _ = crypt::close(dm_name);
        fail(&format!("idmap_mount: {e}"), 1);
    }
    let _ = Command::new(tool("umount")).arg(&raw).status();
    let _ = std::fs::remove_dir(&raw);

    // Shared exchange folder (idmap a human-owned host dir), same as the per-vault
    // path — bound at /exchange, top-level, presented as veracage-owned.
    if let Some(xpath) = args.exchange.as_deref() {
        match validated_exchange(xpath, human_uid) {
            Some(x) => {
                let xmp = PathBuf::from(format!("{WORKSPACE}/.exchange"));
                let _ = std::fs::create_dir_all(&xmp);
                set_mode(&xmp, 0o700);
                match idmap::idmap_mount(
                    &x, &xmp, human_uid, human_gid, vault_uid, vault_gid,
                    idmap::ATTR_NOSUID_NODEV_NOEXEC,
                ) {
                    Ok(()) => env::set_var("VERACAGE_EXCHANGE", &xmp),
                    Err(e) => eprintln!("veracage: exchange idmap failed ({e}); no /exchange"),
                }
            }
            None => eprintln!("veracage: exchange {xpath:?} is not a human-owned directory; skipping"),
        }
    }

    let ctl_fd = ipc::create_control_socket(ctl_path, human_uid, human_gid)
        .unwrap_or_else(|e| fail(&format!("control socket: {e}"), 1));

    std::fs::create_dir_all(&vault_run).unwrap_or_else(|e| fail(&format!("mkdir vault-run: {e}"), 1));
    set_mode(&vault_run, 0o700);
    chown(&vault_run, vault_uid, vault_gid);

    unsafe {
        if libc::setgroups(0, ptr::null()) != 0 {
            fail_errno("setgroups");
        }
        if libc::setresgid(vault_gid, vault_gid, vault_gid) != 0 {
            fail_errno("setresgid");
        }
        if libc::setresuid(vault_uid, vault_uid, vault_uid) != 0 {
            fail_errno("setresuid");
        }
    }

    for kv in &args.setenv {
        if let Some((k, v)) = kv.split_once('=') {
            if ALLOWED_SETENV.contains(&k) {
                env::set_var(k, v);
            }
        }
    }
    env::set_var("VERACAGE_CONTROL_FD", ctl_fd.to_string());
    env::set_var("VERACAGE_VAULT_RUNTIME", &vault_run);
    env::set_var("VERACAGE_VOLUME_LABEL", &volume_label);

    // Inject the helper-computed mountpoint into the leader's argv — the CLI can't
    // know `<WORKSPACE>/<label>` (the label is read post-cryptsetup, above).
    let mut rest = args.rest.clone();
    rest.push("--mountpoint".to_string());
    rest.push(mountpoint.display().to_string());
    check_continuation_argv(&rest);
    let err = Command::new(cont).args(&rest).exec();
    fail(&format!("exec continuation {}: {err}", cont.display()), 127);
}

/// Shared-workspace bootstrap parent (Phase 2). Brings up the compositor if
/// needed, records the session lock (pre-fork, for crash safety), forks the
/// session leader (`session_child`), records its pid, and on its exit closes the
/// volume's dm + drops the session lock/pid/socket. The vault mounts died with
/// the leader's NS, so there is no mountpoint tidy here.
fn run_session_bootstrap(
    args: &Args,
    human_uid: u32,
    human_gid: u32,
    vault_uid: u32,
    vault_gid: u32,
    source: &Path,
) -> ! {
    let sid = args.session.as_deref().unwrap();
    if !session_id_ok(sid) {
        fail(&format!("invalid --session {sid:?}"), 2);
    }
    let runtime_dir = setenv_value(args, "XDG_RUNTIME_DIR")
        .unwrap_or_else(|| fail("XDG_RUNTIME_DIR not forwarded; need it for the control socket", 2));
    let ctl_path = PathBuf::from(&runtime_dir)
        .join("veracage/sessions")
        .join(format!("session-{sid}.sock"));

    let cont = PathBuf::from(continuation());
    if !is_executable(&cont) {
        fail(&format!("continuation not executable: {}", cont.display()), 2);
    }

    // Workspace tmpfs target must exist for the child's mount (created in the
    // shared fs; the child's tmpfs hides it, the host sees an empty dir).
    if let Err(e) = std::fs::create_dir_all(WORKSPACE) {
        fail(&format!("{WORKSPACE}: {e}"), 1);
    }
    set_mode(WORKSPACE, 0o755);

    let dm_name = random_dm_name();
    // Pre-fork lock (empty label; the dm_name is what teardown needs). The child
    // knows the real label but the lock only has to name the device to close.
    write_session_lock(sid, &dm_name, "", human_uid);

    ensure_compositor_running(args, vault_uid, vault_gid);

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        fail_errno("fork");
    }
    if pid == 0 {
        session_child(vault_uid, vault_gid, human_uid, human_gid, source, args.backend,
                      sid, &dm_name, &ctl_path, &cont, args);
    }

    // Parent (root, original NS): the session leader's pid lets Phase 3 find the
    // workspace NS; on the leader's exit we close the dm + drop the session state.
    write_session_pidfile(sid, pid);
    install_signal_forwarding(pid);
    let rc = wait_for(pid);

    let dm_path = format!("/dev/mapper/{dm_name}");
    let mut close_ok = true;
    if Path::new(&dm_path).exists() {
        close_ok = crypt::close(&dm_name).is_ok();
    }
    let _ = std::fs::remove_dir_all(format!("/run/veracage/session-{sid}.run"));
    let _ = std::fs::remove_file(&ctl_path);
    let _ = std::fs::remove_file(format!("/run/veracage/session-{sid}.pid"));
    if close_ok {
        let _ = std::fs::remove_file(format!("/run/veracage/session-{sid}.lock"));
    }
    std::process::exit(rc);
}

/// One prompt: if the shared compositor isn't running, bring it up (detached) as
/// part of THIS pkexec so `veracage open` needs no second authorization. The
/// forked child setsid()s and execs the compositor (outlives the session); the
/// caller doesn't wait on it. Shared by the per-vault and session-bootstrap paths.
fn ensure_compositor_running(args: &Args, vault_uid: u32, vault_gid: u32) {
    if compositor_is_up() {
        return;
    }
    let cpid = unsafe { libc::fork() };
    if cpid < 0 {
        fail_errno("fork(compositor)");
    }
    if cpid == 0 {
        unsafe { libc::setsid() };
        let rest = [
            "_compositor".to_string(),
            "--socket".to_string(),
            "wl-vc".to_string(),
        ];
        spawn_compositor(vault_uid, vault_gid, args, &rest); // execs; never returns
    }
    // Give it a moment to create its socket so the leader finds it.
    let sock = Path::new(COMPOSITOR_RUNTIME).join("wl-vc");
    for _ in 0..120 {
        if sock.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn main() {
    let args = parse_args();

    if unsafe { libc::geteuid() } != 0 {
        fail("must be invoked as root (via pkexec)", 2);
    }

    // Identities: human from pkexec; vault from a fixed system user. Neither
    // from argv.
    let (human_uid, human_gid) = resolve_caller().unwrap_or_else(|e| fail(&e, 2));
    let (vault_uid, vault_gid) = resolve_vault_user(human_uid).unwrap_or_else(|e| fail(&e, 2));

    // XDG_RUNTIME_DIR is forwarded by the caller and used AS ROOT to create/chown
    // the control-socket dir and to reach the host Wayland socket. It MUST be the
    // caller's own runtime dir — a crafted value (with a planted symlink) would
    // otherwise let root chown/chmod an arbitrary path (privilege escalation).
    if let Some(rt) = setenv_value(&args, "XDG_RUNTIME_DIR") {
        let expected = format!("/run/user/{human_uid}");
        if rt != expected {
            fail(&format!("XDG_RUNTIME_DIR must be {expected} (got {rt:?})"), 2);
        }
    }

    // Create the root-owned mountpoint/runtime parent BEFORE anything under it.
    if let Err(e) = std::fs::create_dir_all("/run/veracage") {
        fail(&format!("/run/veracage: {e}"), 1);
    }
    set_mode("/run/veracage", 0o755);

    // `--spawn-compositor`: bring up the persistent shared compositor and exec —
    // no vault, no mount NS, no fork, no cleanup.
    if args.spawn_compositor {
        spawn_compositor(vault_uid, vault_gid, &args, &args.rest);
    }

    let source_arg = args
        .source
        .as_deref()
        .unwrap_or_else(|| fail("--source required", 2));
    let source = std::fs::canonicalize(source_arg)
        .unwrap_or_else(|_| fail(&format!("source not found: {source_arg}"), 2));
    let ft = std::fs::metadata(&source)
        .unwrap_or_else(|e| fail(&format!("stat source: {e}"), 2))
        .file_type();
    if !(ft.is_file() || ft.is_block_device()) {
        fail(&format!("source must be a file or block device: {}", source.display()), 2);
    }

    // Shared-workspace bootstrap (Phase 2): mount into the session's tmpfs
    // workspace + hold it via the session leader. Diverges entirely from the
    // legacy per-vault path below (own NS, own /run/veracage/<hash>).
    if args.session.is_some() {
        run_session_bootstrap(&args, human_uid, human_gid, vault_uid, vault_gid, &source);
    }

    let mountpoint_arg = args
        .mountpoint
        .as_deref()
        .unwrap_or_else(|| fail("--mountpoint required", 2));
    let mountpoint = validated_mountpoint(mountpoint_arg).unwrap_or_else(|e| fail(&e, 2));
    let raw = PathBuf::from(format!("{}.raw", mountpoint.display()));
    let vault_run = PathBuf::from(format!("{}.run", mountpoint.display()));
    let runtime_dir = setenv_value(&args, "XDG_RUNTIME_DIR")
        .unwrap_or_else(|| fail("XDG_RUNTIME_DIR not forwarded; need it for the control socket", 2));
    let ctl_path = PathBuf::from(&runtime_dir)
        .join("veracage/sessions")
        .join(format!("{}.sock", vault_hash(&source)));

    let cont = PathBuf::from(continuation());
    if !is_executable(&cont) {
        fail(&format!("continuation not executable: {}", cont.display()), 2);
    }

    let dm_name = random_dm_name();
    let lock = format!("/run/veracage/{}.lock", vault_hash(&source));
    write_lock(&lock, &dm_name, &mountpoint, human_uid);

    ensure_compositor_running(&args, vault_uid, vault_gid);

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        fail_errno("fork");
    }
    if pid == 0 {
        child(vault_uid, vault_gid, human_uid, human_gid, &source, args.backend,
              &mountpoint, &raw, &vault_run, &ctl_path, &dm_name, &cont, &args);
    }

    // Parent (still root, original mount NS).
    install_signal_forwarding(pid);
    let rc = wait_for(pid);

    let dm_path = format!("/dev/mapper/{dm_name}");
    let mut close_ok = true;
    if Path::new(&dm_path).exists() {
        close_ok = crypt::close(&dm_name).is_ok();
    }
    let _ = std::fs::remove_dir(&raw);
    let _ = std::fs::remove_dir_all(&vault_run);
    let _ = std::fs::remove_file(&ctl_path);
    let _ = std::fs::remove_dir(&mountpoint);
    // Remove the lock only if the device is actually gone, so a failed close
    // leaves a recovery trail for the ExecStopPost cleanup.
    if close_ok {
        let _ = std::fs::remove_file(&lock);
    }

    std::process::exit(rc);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dm_name_matches_cleanup_regex() {
        let n = random_dm_name();
        assert!(n.starts_with("veracage-"));
        let hex = &n["veracage-".len()..];
        assert_eq!(hex.len(), 12);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn vault_hash_is_16_lowercase_hex() {
        let h = vault_hash(Path::new("/tmp/x.vc"));
        assert_eq!(h.len(), 16);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn vault_hash_matches_python_sha256_prefix() {
        // Python: hashlib.sha256(b"/tmp/veracage-x.vc").hexdigest()[:16]
        assert_eq!(vault_hash(Path::new("/tmp/veracage-x.vc")), PY_KNOWN_HASH);
    }

    #[test]
    fn allowed_setenv_excludes_dangerous_keys() {
        assert!(!ALLOWED_SETENV.contains(&"LD_PRELOAD"));
        assert!(!ALLOWED_SETENV.contains(&"LD_LIBRARY_PATH"));
        assert!(ALLOWED_SETENV.contains(&"WAYLAND_DISPLAY"));
    }

    #[test]
    fn mountpoint_accepts_direct_child() {
        assert!(mountpoint_ok("/run/veracage/abc123"));
        assert!(mountpoint_ok("/run/veracage/0123456789abcdef"));
    }

    #[test]
    fn mountpoint_rejects_traversal_and_bare_dir() {
        assert!(!mountpoint_ok("/run/veracage/../../etc/cron.d"));
        assert!(!mountpoint_ok("/run/veracage/../veracage/x"));
        assert!(!mountpoint_ok("/run/veracage"));
        assert!(!mountpoint_ok("/run/veracage/"));
        assert!(!mountpoint_ok("/run/veracage/a/b"));
        assert!(!mountpoint_ok("/tmp/evil"));
        assert!(!mountpoint_ok("/run/veracageX/x"));
        assert!(!mountpoint_ok("run/veracage/x"));
    }

    const PY_KNOWN_HASH: &str = "0631c55cceb0614f";

    #[test]
    fn session_id_accepts_uid_rejects_traversal() {
        assert!(session_id_ok("1000"));
        assert!(session_id_ok("0"));
        assert!(!session_id_ok(""));
        assert!(!session_id_ok("../etc"));
        assert!(!session_id_ok("10a0"));          // non-digit
        assert!(!session_id_ok("12345678901234567")); // 17 chars, too long
        assert!(!session_id_ok("../../root"));
    }

    #[test]
    fn sanitize_label_keeps_safe_maps_unsafe() {
        let src = Path::new("/tmp/x.vc");
        assert_eq!(sanitize_label("Work Docs", src), "Work_Docs");
        assert_eq!(sanitize_label("photos-2024", src), "photos-2024");
        assert_eq!(sanitize_label("a/b/../c", src), "a_b_.._c");
    }

    #[test]
    fn sanitize_label_falls_back_to_hash_for_empty_or_dots() {
        let src = Path::new("/tmp/veracage-x.vc");
        let want = format!("vault-{}", vault_hash(src));
        assert_eq!(sanitize_label("", src), want);
        assert_eq!(sanitize_label("...", src), want);
        assert_eq!(sanitize_label("/", src), want);   // '/'->'_', trims to empty
    }

    #[test]
    fn sanitize_label_is_a_single_component() {
        let src = Path::new("/tmp/x.vc");
        for label in ["../../etc", "a/b", "..", ".", "  ", "normal"] {
            let s = sanitize_label(label, src);
            assert!(!s.contains('/'), "{label:?} -> {s:?} contains a slash");
            assert!(s != "." && s != "..", "{label:?} -> {s:?}");
            assert!(!s.is_empty());
        }
    }
}
