//! Veracage privileged helper: UID-isolation model.
//!
//! Invoked as root via pkexec. Opens an encrypted volume (LUKS or VeraCrypt,
//! file or block device) and presents it, via an *idmapped mount*, as owned by
//! a dedicated *vault* system user, then runs the vault-side leader as that
//! user. The decrypted data is owned by a uid the human cannot assume, and the
//! mount lives in a private namespace, denied AND hidden from host processes.
//!
//! Child:
//!   1. unshare(CLONE_NEWNS), make / rslave
//!   2. cryptsetup open SOURCE (LUKS|VeraCrypt) -> /dev/mapper/veracage-XXXX
//!   3. plain-mount it at <mountpoint>.raw, read its on-disk owner
//!   4. idmap-mount it at MOUNTPOINT, presenting that owner as the vault uid
//!   5. dismount the staging mount
//!   6. drop privileges to the vault uid/gid; exec the (pinned) continuation
//!
//! Parent: waitpid, cryptsetup close, tidy mountpoint + lock.
//!
//! SECURITY: reachable by any active local user via pkexec. It does NOT trust
//! caller-supplied identity or code paths: the human uid comes from PKEXEC_UID,
//! the *vault* uid is resolved from a fixed system user (not argv), the
//! continuation is pinned at build time, forwarded env is allowlisted, and the
//! mountpoint must be a direct child of the canonical /run/veracage.

use std::env;
use std::ffi::CString;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
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

/// Shared runtime dir for the ONE persistent compositor. Root-created,
/// veracage-owned, mode 0711 (the human can traverse to stat the socket/pidfile
/// for a liveness check, but only veracage-uid apps can connect). Holds the fixed
/// `wl-vc` Wayland socket and `compositor.pid`.
const COMPOSITOR_RUNTIME: &str = "/run/veracage/rt";

/// Human-published dir for the compositor's Apps menu: the configured app list
/// (`config.apps`) and menu icons, written by the human side (broker/CLI) and
/// read by the veracage-uid compositor. Root-created inside root-owned
/// /run/veracage (so it can't be pre-planted), then handed to the human uid,
/// mode 0755, the same trust level as ~/.config/veracage/config.toml.
const PUB_DIR: &str = "/run/veracage/pub";

/// Exit code for "the decrypt itself failed" (cryptsetup open declined, almost
/// always a wrong passphrase). Distinct from generic failures (1) and usage/env
/// errors (2) so the GUI can re-prompt for the passphrase on exactly this case.
const EXIT_CRYPT_FAILED: i32 = 4;

/// Exit code for "the decrypted filesystem needs a repair we will not do": the
/// volume was opened, checked, and left dismounted. Distinct so the GUI can say
/// what happened instead of showing a bare exit code.
const EXIT_FSCK_FAILED: i32 = 5;

/// Exit code for "the volume was detached but an app still holds it open, so its
/// dm device (and its key) are still there". Distinct because it is the one
/// dismount outcome the human MUST be told about: the menu entry is gone, yet
/// the volume stays decrypted until that app lets go.
const EXIT_VOLUME_BUSY: i32 = 6;

/// Shared-workspace model (docs/shared-workspace.md): the ONE
/// session's private mount NS holds every open volume under this tmpfs, each at
/// `<WORKSPACE>/<label>`. A tmpfs so the whole tree (and every idmap mount on it)
/// vanishes when the session leader (which holds the NS) dies; only the global
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
    "XCURSOR_THEME",
    "XCURSOR_SIZE",
    "VERACAGE_THEME",
    "VERACAGE_FONT_FILE",
    "VERACAGE_FONT_SIZE",
    "VERACAGE_WINDOW_SIZE",
    "VERACAGE_DEBUG",
    "VERACAGE_LOG_DIR",
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

fn fail(msg: &str, code: i32) -> ! {
    eprintln!("veracage-helper: {msg}");
    std::process::exit(code);
}

fn fail_errno(what: &str) -> ! {
    fail(&format!("{what}: {}", std::io::Error::last_os_error()), 1);
}

struct Args {
    /// `--spawn-compositor`: bring up the ONE persistent compositor instead of
    /// mounting a vault. No --source needed in this mode.
    spawn_compositor: bool,
    /// `--empty-session`: bootstrap a session leader with NO volume (the empty
    /// front door / scratchpad). Same NS + workspace tmpfs + idmapped exchange +
    /// leader as a volume bootstrap, minus cryptsetup. Volumes join later via the
    /// add-volume path. Needs --session; no --source.
    empty_session: bool,
    source: Option<String>,
    backend: Option<Backend>, // None = auto-detect
    /// `--session <id>`: the shared-workspace session (required for a mount).
    /// The helper mounts the volume at `<WORKSPACE>/<label>` in the session's
    /// tmpfs workspace and holds it via the session leader, writing
    /// `session-<id>.lock`/`.pid`.
    session: Option<String>,
    /// `--close-volume <label>`: setns into the session and close JUST this volume
    /// (dismount + `cryptsetup close --deferred` + drop it from the lock), leaving
    /// the rest of the session running. Needs --session.
    close_volume: Option<String>,
    /// `--exchange <dir>`: a human-owned host directory to idmap-mount into the
    /// sandbox as the shared directory. Caller-supplied, so validated (owner
    /// == human, real dir, no final-component symlink) before root touches it.
    exchange: Option<String>,
    setenv: Vec<String>,
    rest: Vec<String>,
    /// Volume passphrase read from stdin (`--passphrase-stdin`), newline-stripped.
    /// Set by the GUI launcher (no tty for cryptsetup to prompt on); None means
    /// cryptsetup prompts interactively (the terminal CLI path).
    passphrase: Option<Vec<u8>>,
}

/// Parse argv. Only the flags below are accepted before `--`; anything else is
/// an error (notably no --user/--continuation). Presence of required args is
/// validated per-mode in main(), not here.
fn parse_args() -> Args {
    let mut spawn_compositor = false;
    let mut empty_session = false;
    let mut source: Option<String> = None;
    let mut backend: Option<Backend> = None;
    let mut session: Option<String> = None;
    let mut close_volume: Option<String> = None;
    let mut exchange: Option<String> = None;
    let mut setenv: Vec<String> = Vec::new();
    let mut rest: Vec<String> = Vec::new();
    let mut passphrase: Option<Vec<u8>> = None;
    let mut it = env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--spawn-compositor" => spawn_compositor = true,
            "--empty-session" => empty_session = true,
            "--passphrase-stdin" => {
                use std::io::Read;
                let mut buf = Vec::new();
                // Fail on a real read error rather than swallowing it: an empty
                // passphrase would surface as EXIT_CRYPT_FAILED (wrong
                // passphrase), masking the actual I/O failure. Cap the read so a
                // caller can't make the root helper buffer unbounded input.
                let mut limited = std::io::stdin().take(64 * 1024);
                if let Err(e) = limited.read_to_end(&mut buf) {
                    fail(&format!("reading passphrase from stdin: {e}"), 2);
                }
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
            "--session" => session = it.next(),
            "--close-volume" => close_volume = it.next(),
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
    Args { spawn_compositor, empty_session, source, backend, session, close_volume, exchange, setenv, rest, passphrase }
}

/// (uid, gid) of the invoking human, taken from PKEXEC_UID, never from argv.
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

/// Keep decrypted content out of core dumps, for this process and everything it
/// execs: the compositor, the session leader, bwrap and every app. A core is
/// written to a host-readable file under /var/lib/systemd/coredump and would
/// carry volume plaintext (and the passphrase this helper reads for cryptsetup),
/// which is exactly the accidental leak Veracage exists to prevent.
///
/// `RLIMIT_CORE` alone does NOT stop it. When `kernel.core_pattern` is a pipe
/// (systemd-coredump) the kernel skips the limit check, and systemd's pattern
/// passes a hardcoded infinity in the slot where `%c` would carry the real
/// limit, so the dump is written whatever the limit says. `coredump_filter`
/// selects which memory a dump may contain: cleared, the dump holds no memory at
/// all. It survives `execve` and is inherited by children, so setting it once
/// here covers the whole session. The limit is still set for hosts whose
/// `core_pattern` is a plain file, where it does take effect.
fn suppress_core_dumps() {
    unsafe {
        let rl = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        libc::setrlimit(libc::RLIMIT_CORE, &rl);
    }
    let _ = std::fs::write("/proc/self/coredump_filter", "0\n");
}

/// Drop privileges to the vault user: supplementary groups from the vault
/// user's /etc/group memberships (initgroups - notably `render`, granted at
/// install so Mesa can open /dev/dri/renderD* for hardware GL; a bare
/// setgroups(0) would silently strip it and force llvmpipe software
/// rendering), then gid, then uid. The group list is root-administered via
/// /etc/group, never caller input.
fn drop_to_vault_user(vault_uid: u32, vault_gid: u32) {
    let name = CString::new(VAULT_USER).unwrap();
    unsafe {
        if libc::initgroups(name.as_ptr(), vault_gid) != 0 {
            fail_errno("initgroups");
        }
        if libc::setresgid(vault_gid, vault_gid, vault_gid) != 0 {
            fail_errno("setresgid");
        }
        if libc::setresuid(vault_uid, vault_uid, vault_uid) != 0 {
            fail_errno("setresuid");
        }
    }
}

/// (uid, gid) of the dedicated vault system user, resolved by NAME, never
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
    h.update(p.as_os_str().as_bytes()); // raw bytes, matches Python on UTF-8 paths
    h.finalize().iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// `<dev>:<ino>` of the source: what the container IS, rather than one of the
/// names it answers to. The duplicate-open guard keys on this because a hash of
/// the canonical path is defeated by a hard link, or by the same filesystem
/// reachable at two canonical paths - and a second open of a container that is
/// already open means two dm devices over one filesystem, both mounted
/// read-write, which is corruption rather than a leak.
fn source_key(source: &Path) -> String {
    use std::os::unix::fs::MetadataExt;
    match std::fs::metadata(source) {
        Ok(md) => format!("{}:{}", md.dev(), md.ino()),
        Err(_) => String::new(),
    }
}

fn random_hex(bytes: usize) -> String {
    use std::io::Read;
    let mut buf = vec![0u8; bytes];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .unwrap_or_else(|e| fail(&format!("/dev/urandom: {e}"), 1));
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// "veracage-" + 12 lowercase hex, matching DM_NAME_RE in cleanup.py.
fn random_dm_name() -> String {
    format!("veracage-{}", random_hex(6))
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

/// True for a filesystem with no per-file ownership (FAT/exFAT/NTFS): ownership
/// comes from mount options and `chown(2)` returns EPERM. VeraCrypt/TrueCrypt
/// volumes are commonly FAT. Pure, so it is unit-tested.
fn is_ownerless_fs(fstype: Option<&str>) -> bool {
    matches!(
        fstype.unwrap_or("").to_ascii_lowercase().as_str(),
        "vfat" | "exfat" | "ntfs" | "ntfs3" | "msdos",
    )
}

/// Stage-mount options for the decrypted `dm_path`, and whether a chown is still
/// needed afterward. An ownerless fs (FAT/exFAT/NTFS) is mounted with `uid=/gid=`
/// so every file already appears owned by the human uid (the idmap then remaps it
/// to the vault uid, a WRITABLE vault). Without this such a volume mounts
/// root-owned, `chown` fails with EPERM, and the sandbox sees `nobody`,
/// read-only. A POSIX fs is mounted plain and its root inode chowned instead.
fn stage_opts(fstype: Option<&str>, human_uid: u32, human_gid: u32) -> (String, bool) {
    if is_ownerless_fs(fstype) {
        (format!("nodev,nosuid,uid={human_uid},gid={human_gid},umask=0077"), false)
    } else {
        ("nodev,nosuid".to_string(), true)
    }
}

fn is_executable(p: &Path) -> bool {
    p.is_file()
        && std::fs::metadata(p)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
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

/// Validate `--session <sid>` AND bind it to the authenticated caller. The
/// session id IS the human's own uid; a session's leader runs as the shared
/// `veracage` uid, so `verify_session_leader` alone cannot tell owners apart.
/// Without this a second local user could `setns`/mount/close against another
/// user's session by passing a foreign `--session`. `cleanup.py` already does
/// the equivalent `PKEXEC_UID`-vs-owner check; the mount helper must match it.
/// Exits (via `fail`) on a malformed or foreign session id; returns otherwise.
fn check_session_caller(sid: &str, human_uid: u32) {
    if !session_id_ok(sid) {
        fail(&format!("invalid --session {sid:?}"), 2);
    }
    if sid != human_uid.to_string() {
        fail(
            &format!("--session {sid:?} does not belong to the caller (uid {human_uid})"),
            2,
        );
    }
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
    let trimmed = s.trim_matches(|c| matches!(c, '.' | '_' | '-'));
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        return format!("vault-{}", vault_hash(source));
    }
    trimmed.to_string()
}

/// The per-volume mountpoint under the workspace tmpfs: `<WORKSPACE>/<label>`,
/// with a `-2`, `-3`… suffix if that directory already exists (a second volume
/// with the same label). Called inside the session NS, after the tmpfs is up.
fn workspace_path(label: &str, source: &Path) -> PathBuf {
    workspace_path_in(Path::new(WORKSPACE), label, source)
}

/// The same, under an explicit root, so the suffix logic can be tested without
/// the session's tmpfs.
fn workspace_path_in(root: &Path, label: &str, source: &Path) -> PathBuf {
    let base = sanitize_label(label, source);
    let first = root.join(&base);
    if !first.exists() {
        return first;
    }
    (2..1000)
        .map(|n| root.join(format!("{base}-{n}")))
        .find(|p| !p.exists())
        .unwrap_or(first)
}

/// Serialize session-state transitions for `sid`: the bootstrap-vs-add decision,
/// session-lock writes, and teardown. A dedicated sidecar file (never deleted:
/// deleting a held flock path lets a second opener lock a NEW inode and both
/// "hold" it) because the session lock itself is atomically REPLACED by
/// `session_lock_remove`, which would break flock identity. cleanup.py takes the
/// same flock (its sibling `.flock` of the lock path). Blocks; released on drop.
fn session_flock(sid: &str) -> std::fs::File {
    let path = format!("/run/veracage/session-{sid}.flock");
    let f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .unwrap_or_else(|e| fail(&format!("session flock {path}: {e}"), 1));
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
        fail_errno("flock(session)");
    }
    f
}

/// The `generation=` header of a session-lock body: a random nonce each
/// bootstrap writes so its own teardown can tell "this lock is still mine" from
/// "a successor session reused my sid while I was tearing down".
fn lock_generation(body: &str) -> Option<&str> {
    body.lines().find_map(|l| l.strip_prefix("generation=")).map(str::trim)
}

/// Create the session lock fresh (`session-<sid>.lock`, root-owned 0600) with
/// its `user_uid=` and `generation=` headers. The caller holds the session
/// flock and has already recovered/removed any stale predecessor lock.
fn create_session_lock(sid: &str, uid: u32, gen: &str) {
    let path = format!("/run/veracage/session-{sid}.lock");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .unwrap_or_else(|e| fail(&format!("session lock {path}: {e}"), 1));
    f.write_all(format!("user_uid={uid}\ngeneration={gen}\n").as_bytes())
        .unwrap_or_else(|e| fail(&format!("write session lock {path}: {e}"), 1));
}

/// Append a volume to the session lock: a
/// `volume=<dm_name>\t<label>\t<vault_hash>\t<dev>:<ino>` line per open volume. cleanup.py `cleanup_session` walks these to close every
/// dm on teardown; the vault hash is what lets a later open detect "this source
/// is already open" (the duplicate-open guard in `run_session_add`). Written
/// BEFORE the mount (with the dm_name we pre-generated) so an early crash still
/// leaves the ExecStopPost a device to close.
fn append_session_volume(sid: &str, human_uid: u32, dm_name: &str, label: &str,
                        vhash: &str, skey: &str) {
    let path = format!("/run/veracage/session-{sid}.lock");
    // create(true): if the lock somehow vanished under a live leader, a recreated
    // lock still tracks the dm for the ExecStopPost cleanup.
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .unwrap_or_else(|e| fail(&format!("session lock {path}: {e}"), 1));
    // If we just recreated it (empty), write the `user_uid` header first so the
    // lock is NEVER header-less: cleanup.py refuses to act on a lock whose owner
    // it can't verify, and a legitimate recreated lock must carry that owner.
    let fresh = f.metadata().map(|m| m.len() == 0).unwrap_or(false);
    if fresh {
        f.write_all(format!("user_uid={human_uid}\n").as_bytes())
            .unwrap_or_else(|e| fail(&format!("write session lock {path}: {e}"), 1));
    }
    f.write_all(format!("volume={dm_name}\t{label}\t{vhash}\t{skey}\n").as_bytes())
        .unwrap_or_else(|e| fail(&format!("write session lock {path}: {e}"), 1));
}

/// dm names of the volumes a session-lock body records for `vhash` (the third
/// tab field of a `volume=` line). Pure, for unit tests; the caller decides
/// which of them are still live by checking /dev/mapper.
fn lock_dms_for_vhash(body: &str, vhash: &str) -> Vec<String> {
    body.lines()
        .filter_map(|l| l.strip_prefix("volume="))
        .filter_map(|v| {
            let mut f = v.split('\t');
            let dm = f.next().unwrap_or("").trim().to_string();
            let _label = f.next();
            (f.next() == Some(vhash) && !dm.is_empty()).then_some(dm)
        })
        .collect()
}

/// dm names a session-lock body records for `skey` (the FOURTH tab field, the
/// source's dev:ino). Pure, for unit tests. A line written before this field
/// existed simply does not match, which is the same answer as "not open".
fn lock_dms_for_source(body: &str, skey: &str) -> Vec<String> {
    if skey.is_empty() {
        return Vec::new();
    }
    body.lines()
        .filter_map(|l| l.strip_prefix("volume="))
        .filter_map(|v| {
            let mut f = v.split('\t');
            let dm = f.next().unwrap_or("").trim().to_string();
            let _label = f.next();
            let _vhash = f.next();
            (f.next().map(str::trim) == Some(skey) && !dm.is_empty()).then_some(dm)
        })
        .collect()
}

/// The `volume=` lines of a stale lock whose dm device is STILL there, verbatim.
/// A recovery that could not close a device must keep its line: that line is the
/// only record any teardown has (cleanup.py and the sleep hook both walk it), so
/// dropping it orphans a decrypted device, with its key in RAM, for good.
fn lock_survivors(body: &str, still_open: impl Fn(&str) -> bool) -> Vec<String> {
    body.lines()
        .filter(|l| match l.strip_prefix("volume=") {
            // No `let...else`: this crate builds on Debian 12's rustc 1.63.
            Some(v) => {
                let dm = v.split('\t').next().unwrap_or("").trim();
                is_veracage_dm(dm) && still_open(dm)
            }
            None => false,
        })
        .map(str::to_string)
        .collect()
}

/// Append raw `volume=` lines to the session lock, for a bootstrap carrying a
/// stale session's still-open devices into its own lock.
fn append_session_lines(sid: &str, lines: &[String]) {
    if lines.is_empty() {
        return;
    }
    let path = format!("/run/veracage/session-{sid}.lock");
    match std::fs::OpenOptions::new().append(true).open(&path) {
        Ok(mut f) => {
            for l in lines {
                let _ = f.write_all(format!("{l}\n").as_bytes());
            }
        }
        Err(e) => eprintln!("veracage-helper: carrying stale volumes into {path}: {e}"),
    }
}

/// Record the session leader's pid + start-time (`session-<sid>.pid`, root 0600):
/// the add-volume path reads it to find the workspace NS holder, and the
/// start-time pins it against pid reuse. Root-only (consumed by another pkexec
/// helper / the root cleanup, never the human side).
fn write_session_pidfile(sid: &str, pid: i32) {
    let path = format!("/run/veracage/session-{sid}.pid");
    let st = proc_starttime(pid).unwrap_or(0);
    let _ = std::fs::remove_file(&path);
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
    {
        Ok(mut f) => {
            let _ = f.write_all(format!("{pid}\n{st}\n").as_bytes());
        }
        Err(e) => eprintln!("veracage-helper: session pidfile {path}: {e}"),
    }
}

/// `/proc/<pid>/stat` field 22 (start-time, clock ticks since boot). `comm`
/// (field 2) is parenthesised and may contain spaces/parens, so split after the
/// LAST ')'; the remaining fields start at field 3, so start-time is index 19.
fn proc_starttime(pid: i32) -> Option<u64> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &s[s.rfind(')')? + 1..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// True if `/proc/<pid>` exists and is owned by `uid`, i.e. the process is alive
/// and its real uid is `uid` (the leader is the only thing running as the vault uid).
fn proc_owned_by(pid: i32, uid: u32) -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(format!("/proc/{pid}")).map(|m| m.uid() == uid).unwrap_or(false)
}

/// `veracage-` + 12 lowercase hex, matching DM_NAME_RE in cleanup.py.
fn is_veracage_dm(name: &str) -> bool {
    matches!(name.strip_prefix("veracage-"),
        Some(h) if h.len() == 12 && h.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()))
}

/// If a live, verified session leader is recorded for `sid`, return its pid.
/// Verifies: pid alive + owned by the vault uid (only the leader runs as it) +
/// start-time matches the pidfile (defeats pid reuse). `None` => no live session,
/// so the caller BOOTSTRAPS a new one instead of joining.
fn verify_session_leader(sid: &str, vault_uid: u32) -> Option<i32> {
    let body = std::fs::read_to_string(format!("/run/veracage/session-{sid}.pid")).ok()?;
    let mut lines = body.lines();
    let pid: i32 = lines.next()?.trim().parse().ok()?;
    let want_st: u64 = lines.next().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    if pid <= 1 || !proc_owned_by(pid, vault_uid) || proc_starttime(pid) != Some(want_st) {
        return None;
    }
    Some(pid)
}

/// Close EVERY volume's dm named in the session lock. The leader's namespace (and
/// thus every `/vaults/*` mount) is already gone, so only the global dm devices
/// survive and must be closed. Mirrors cleanup.py's `cleanup_session`, for the
/// graceful in-helper teardown. Returns true iff every dm is now gone.
fn close_all_session_dms(sid: &str) -> bool {
    let path = format!("/run/veracage/session-{sid}.lock");
    let body = match std::fs::read_to_string(&path) {
        Ok(body) => body,
        Err(_) => return true, // no lock: nothing recorded to close
    };
    let mut all_ok = true;
    for line in body.lines() {
        if let Some(v) = line.strip_prefix("volume=") {
            let dm = v.split('\t').next().unwrap_or("").trim();
            if is_veracage_dm(dm) && Path::new(&format!("/dev/mapper/{dm}")).exists() {
                all_ok &= crypt::close(dm).is_ok();
            }
        }
    }
    all_ok
}

/// How long to keep retrying a `cryptsetup close` the kernel reports busy, and
/// how long to wait between tries. Mirrors cleanup.py's CLOSE_RETRY_FOR: a
/// namespace whose last process just died is torn down asynchronously, so a
/// single attempt can lose a race it only has to wait out, and losing it leaves
/// the volume unmounted with its key still in RAM.
const CLOSE_RETRY_FOR: std::time::Duration = std::time::Duration::from_secs(5);
const CLOSE_RETRY_EVERY: std::time::Duration = std::time::Duration::from_millis(200);

/// `cryptsetup close` a dm device, retrying while the kernel reports it busy.
/// True once the device is gone (including "it was never there").
fn close_dm_retrying(dm: &str) -> bool {
    let dm_path = format!("/dev/mapper/{dm}");
    let deadline = std::time::Instant::now() + CLOSE_RETRY_FOR;
    loop {
        if !Path::new(&dm_path).exists() {
            return true;
        }
        let _ = Command::new(tool("cryptsetup")).args(["close", dm]).status();
        if !Path::new(&dm_path).exists() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(CLOSE_RETRY_EVERY);
    }
}

/// A safe volume label = a single path component (non-empty, no '/', not '.'/'..').
fn label_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && !s.contains('/') && s != "." && s != ".."
}

/// The dm device backing the mount at `mp` in the CURRENT namespace: read
/// /proc/self/mountinfo (field 5 = mount point, field 3 = maj:min), resolve
/// `/sys/dev/block/<maj:min>/dm/name`, and accept only a `veracage-<12hex>` name.
fn dm_at_mountpoint(mp: &str) -> Option<String> {
    let mi = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    for line in mi.lines() {
        let f: Vec<&str> = line.split(' ').collect();
        if f.len() > 4 && f[4] == mp {
            let name = std::fs::read_to_string(format!("/sys/dev/block/{}/dm/name", f[2])).ok()?;
            let name = name.trim().to_string();
            return is_veracage_dm(&name).then_some(name);
        }
    }
    None
}

/// Rewrite the session lock without `dm_name`'s volume line (a closed volume). The
/// lock is on the shared root fs, so this works from inside the leader's NS too.
fn session_lock_remove(sid: &str, dm_name: &str) {
    let path = format!("/run/veracage/session-{sid}.lock");
    let body = match std::fs::read_to_string(&path) {
        Ok(body) => body,
        Err(_) => return,
    };
    let kept: String = body
        .lines()
        .filter(|l| {
            l.strip_prefix("volume=")
                .map(|v| v.split('\t').next().unwrap_or("").trim() != dm_name)
                .unwrap_or(true) // keep non-volume lines (user_uid header)
        })
        .map(|l| format!("{l}\n"))
        .collect();
    let tmp = format!("{path}.tmp");
    if std::fs::write(&tmp, kept).is_ok() {
        set_mode(&tmp, 0o600);
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Which processes hold `dm` mounted from OUTSIDE this mount namespace, as their
/// command names, deduplicated and sorted. EVERY holder is listed, sandbox
/// plumbing included: this is the close decision, not the message. Formatting
/// (which drops the plumbing) is `holder_names`.
///
/// This is what decides whether a volume can really be closed. `cryptsetup close`
/// fails with "Device is still in use" while any other mount namespace has the
/// filesystem, and every sandboxed app has one: bubblewrap binds the workspace
/// recursively, so the volume's mount is in the app's namespace whether or not it
/// has a file open. Measured: this count predicted the close outcome exactly
/// (0 -> closes, non-zero -> "still in use"), while `umount`
/// succeeded in every case - so VeraCrypt's own busy test (run `umount`, report
/// its failure) would detect nothing here.
///
/// Must be called AFTER setns into the leader's namespace, so `/proc/self/ns/mnt`
/// is the namespace to exclude. Best-effort: an unreadable `/proc` entry is a
/// process that is exiting, not a holder.
fn foreign_holders(dm: &str) -> Vec<String> {
    let needle = format!(" /dev/mapper/{dm} ");
    let own_ns = match std::fs::read_link("/proc/self/ns/mnt") {
        Ok(ns) => ns,
        Err(_) => return Vec::new(),
    };
    let entries = match std::fs::read_dir("/proc") {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut names: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let pid = entry.file_name();
        let pid = pid.to_string_lossy();
        if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        match std::fs::read_to_string(format!("/proc/{pid}/mountinfo")) {
            Ok(mountinfo) if mountinfo.contains(&needle) => {}
            _ => continue, // not a holder, or a process that just exited
        }
        match std::fs::read_link(format!("/proc/{pid}/ns/mnt")) {
            Ok(ns) if ns != own_ns => {}
            _ => continue, // our own namespace, or gone
        }
        // `/proc/<pid>/comm` is set by the process itself and may contain any
        // byte, tabs and newlines included, and it goes out on a line-oriented
        // protocol the broker parses. Sanitize it the way volume labels are.
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .ok()
            .and_then(|c| sanitized(&c));
        names.push(comm.unwrap_or_else(|| "a process".to_string()));
    }
    names.sort();
    names.dedup();
    names
}

/// The holders worth naming to the human: Veracage's own sandbox plumbing is
/// dropped so a message says Dolphin, not bwrap. Presentation ONLY. The decision
/// to close is made on the full `foreign_holders` list, because a volume held by
/// nothing but a still-exiting `bwrap` is just as unclosable, and umounting it
/// first would leave the volume detached and still decrypted.
fn holder_names(holders: &[String]) -> String {
    let named: Vec<&str> = holders
        .iter()
        .map(String::as_str)
        .filter(|c| !is_sandbox_wrapper(c))
        .collect();
    if named.is_empty() {
        "Apps inside Veracage".to_string()
    } else {
        named.join(", ")
    }
}

/// Process names that are Veracage's own sandbox plumbing rather than an app the
/// human would recognise, so a "still in use" message names Dolphin, not bwrap.
/// `veracage-reaper` is the subreaper the leader puts between the bus and the
/// app (`sandbox.REAPER_NAME`).
const SANDBOX_WRAPPERS: &[&str] =
    &["bwrap", "dbus-run-session", "dbus-daemon", "veracage-reaper", "sh", "bash"];

/// True if `comm` is sandbox plumbing. `/proc/<pid>/comm` is capped at 15
/// characters, so "dbus-run-session" arrives as "dbus-run-sessio" and an equality
/// test silently misses it - which is how the human got told a dismount was
/// blocked by "dbus-run-sessio". Match the truncation instead.
fn is_sandbox_wrapper(comm: &str) -> bool {
    SANDBOX_WRAPPERS.iter().any(|w| *w == comm || w.starts_with(comm) && comm.len() >= 15)
}

/// How long the helper waits, after reporting holders, for the caller to say
/// whether to go on. This is a ROOT process parked in the session's mount
/// namespace, so the wait is bounded: the human side drives it (see the broker's
/// APPS_CLOSED_WAIT) and this is only the backstop for a caller that dies.
const HOLDERS_WAIT: std::time::Duration = std::time::Duration::from_secs(300);

/// Longest single poll slice while parked, so the leader-liveness check below
/// runs about once a second rather than only when the caller says something.
const HOLDERS_POLL_SLICE_MS: i32 = 1000;

/// Wait for the caller's answer to a `holders` report: `go` (the apps are gone,
/// try again) or EOF/anything else (leave the volume exactly as it is). Times out
/// at HOLDERS_WAIT, and gives up at once if `leader_pid` dies.
///
/// The answer grants nothing: on `go` the holders are checked again in here, so
/// the caller can only ask for a re-check it could have got by running us again.
/// Waiting instead of exiting is what keeps ONE dismount to ONE authentication:
/// polkit's auth_self_keep is bound to the calling process, so a second attempt
/// from a second process would ask for the password again.
///
/// The leader check is what keeps this wait from outliving the thing it is
/// waiting on. We are a ROOT process inside the session's mount namespace and we
/// hold the session flock, so parking on after the leader has gone would keep
/// that namespace (and every volume mount in it) alive AND block the teardown
/// paths that take the same flock: the unit's ExecStopPost cleanup, which
/// systemd kills at its stop timeout, and the root suspend hook, which the
/// sleep transition waits on.
fn wait_for_go(leader_pid: i32) -> bool {
    wait_for_go_on(libc::STDIN_FILENO, leader_pid, HOLDERS_WAIT)
}

/// The wait itself, against an explicit descriptor and timeout so the protocol
/// can be exercised over a pipe in a test. `leader_pid` <= 0 skips the liveness
/// check (there is no session to watch in a test).
fn wait_for_go_on(fd: libc::c_int, leader_pid: i32, budget: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + budget;
    let mut line = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return false;
        }
        if leader_pid > 0 && !Path::new(&format!("/proc/{leader_pid}")).exists() {
            return false; // the session went away under us
        }
        let slice = (left.as_millis() as i32).min(HOLDERS_POLL_SLICE_MS);
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        match unsafe { libc::poll(&mut pfd, 1, slice) } {
            0 => continue,      // slice elapsed: re-check the leader, then wait on
            n if n < 0 => return false, // poll failed: treat as gone
            _ => {}
        }
        let mut buf = [0u8; 32];
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            return false; // EOF: cancelled
        }
        line.extend_from_slice(&buf[..n as usize]);
        if let Some(end) = line.iter().position(|b| *b == b'\n') {
            return &line[..end] == b"go";
        }
        if line.len() >= buf.len() {
            return false; // not our protocol
        }
    }
}

/// Close JUST one volume of a running session: join the leader's NS, check that
/// nothing outside it still holds the volume, then really dismount it and really
/// `cryptsetup close` its dm before returning - the key is out of RAM by the time
/// this exits, not "later, when an app lets go". The rest of the session runs on.
/// The volume is either closed or untouched, never half-dismounted.
///
/// A volume an app still holds cannot be closed, and this is the ONE process the
/// human authenticated for, so it does not hand the problem back and exit: it
/// reports the holders on stdout (`holders\t<names>`) and waits for the caller to
/// close them and answer `go` on stdin. Cancelling (EOF) leaves everything as it
/// is. Refusals - a cancel, a re-check that still finds holders, a lost race -
/// exit EXIT_VOLUME_BUSY with a `busy\t<reason>` line.
///
/// No --source, no leader, no compositor.
fn run_close_volume(args: &Args, human_uid: u32, vault_uid: u32) -> ! {
    let sid = args.session.as_deref().unwrap_or_else(|| fail("--close-volume needs --session", 2));
    check_session_caller(sid, human_uid);
    let label = args.close_volume.as_deref().unwrap();
    if !label_ok(label) {
        fail(&format!("invalid --close-volume label {label:?}"), 2);
    }
    // Serialize the lock rewrite (session_lock_remove) with opens/teardowns.
    let _flock = session_flock(sid);
    let leader_pid = verify_session_leader(sid, vault_uid)
        .unwrap_or_else(|| fail("no live session to close a volume from", 1));

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        fail_errno("fork");
    }
    if pid == 0 {
        let ns = format!("/proc/{leader_pid}/ns/mnt");
        let c = CString::new(ns.clone()).unwrap();
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            fail_errno(&format!("open {ns}"));
        }
        if unsafe { libc::setns(fd, libc::CLONE_NEWNS) } != 0 {
            fail_errno("setns(mnt)");
        }
        unsafe { libc::close(fd) };
        let mp = format!("{WORKSPACE}/{label}");
        let dm = dm_at_mountpoint(&mp).unwrap_or_else(|| fail(&format!("volume {label} not mounted"), 1));

        // Check BEFORE touching anything, so a volume that cannot be closed is
        // left exactly as it was rather than detached-but-decrypted. The names go
        // to stdout for the caller to put in front of the human, who is the only
        // one who can close those apps - so wait here for the answer rather than
        // making them authenticate a second attempt.
        let holders = foreign_holders(&dm);
        if !holders.is_empty() {
            println!("holders\t{}", holder_names(&holders));
            let _ = std::io::stdout().flush();
            if !wait_for_go(leader_pid) {
                println!("busy\tcancelled");
                std::process::exit(EXIT_VOLUME_BUSY);
            }
            if !foreign_holders(&dm).is_empty() {
                println!("busy\tan app is still running");
                std::process::exit(EXIT_VOLUME_BUSY);
            }
        }

        // A real dismount and a real close: no MNT_DETACH, no --deferred. If
        // either is refused the volume stays as it was and so does the lock line,
        // which is what the duplicate-open guard reads.
        let cmp = CString::new(mp.clone()).unwrap();
        if unsafe { libc::umount2(cmp.as_ptr(), 0) } != 0 {
            eprintln!("veracage-helper: umount {mp}: {}", std::io::Error::last_os_error());
            println!("busy\tthe volume is still in use");
            std::process::exit(EXIT_VOLUME_BUSY);
        }
        if !close_dm_retrying(&dm) {
            // Unmounted but not closed. Nothing held it a moment ago and the
            // retries above waited out the usual cause (a namespace the kernel
            // had not finished tearing down), so this is a holder that appeared
            // between the check and the umount. Say so: the volume is gone from
            // the workspace but its key is not gone from RAM, and only the
            // session teardown will close it now. The lock line stays, so a
            // reopen keeps being refused and the teardown still knows the device.
            println!("busy\tits device is still in use, so it is still decrypted");
            std::process::exit(EXIT_VOLUME_BUSY);
        }
        session_lock_remove(sid, &dm);
        let _ = std::fs::remove_dir(&mp);
        std::process::exit(0);
    }
    // Forward a SIGTERM/SIGINT to the child: it is a root process parked inside
    // the session's namespace holding the session flock, so it must not outlive
    // the helper it belongs to.
    install_signal_forwarding(pid);
    std::process::exit(wait_for(pid));
}

/// Bring up the ONE persistent `veracage`-uid compositor. Unlike the vault path
/// there is no crypt/mount/idmap and no fork: connect the host Wayland fd, prep
/// the shared runtime dir, drop to the vault uid, and exec the pinned
/// continuation (which execs the compositor). The surviving process IS the
/// compositor, so its OWN systemd --user transient unit (created by the CLI's
/// `veracage _up`) tracks it directly, decoupled from any vault session's unit,
/// so a session ending never takes the shared compositor down with it. Liveness
/// is checked CLI-side by wayland.py::compositor_is_up (which correctly rejects a
/// zombie pid); there is no in-helper liveness check because the helper no longer
/// decides whether to spawn (the CLI does, before the mount pkexec).
fn spawn_compositor(
    human_uid: u32,
    human_gid: u32,
    vault_uid: u32,
    vault_gid: u32,
    args: &Args,
    cont_rest: &[String],
) -> ! {
    let rt = Path::new(COMPOSITOR_RUNTIME);
    std::fs::create_dir_all(rt).unwrap_or_else(|e| fail(&format!("mkdir {}: {e}", rt.display()), 1));
    set_mode(rt, 0o711);
    chown(rt, vault_uid, vault_gid);

    // The human-published dir (config-app list + menu icons for the Apps menu).
    // Best-effort: the compositor degrades to a text-only menu without it.
    let pub_dir = Path::new(PUB_DIR);
    if std::fs::create_dir_all(pub_dir).is_ok() {
        set_mode(pub_dir, 0o755);
        chown(pub_dir, human_uid, human_gid);
    }

    // Connect the host compositor. The fd is inherited across exec (cloexec
    // cleared) and its number is handed to the compositor as WAYLAND_SOCKET,
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
    let wl_fd = ipc::connect_host_wayland(&wl_path, human_uid)
        .unwrap_or_else(|e| fail(&format!("host wayland connect ({}): {e}", wl_path.display()), 1));

    // Clear any stale socket left by a crashed compositor, then record our pid
    // (survives exec) for the liveness check.
    let _ = std::fs::remove_file(rt.join("wl-vc"));
    write_pidfile(&rt.join("compositor.pid"), vault_uid, vault_gid);

    drop_to_vault_user(vault_uid, vault_gid);

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
            // `sighandler_t` is an integer, and casting a function straight to
            // one is what `function_casts_as_integer` warns about (the function
            // item has no address until it becomes a pointer). Take the address
            // first, then convert that: keep the `*const ()` hop.
            libc::signal(sig, forward_signal as *const () as libc::sighandler_t);
        }
    }
}

/// Refuse a source the CALLER could not open themselves.
///
/// Everything past this point runs as root, so without the check the helper is a
/// way to have root open - and `fsck -p` WRITE to - a container or block device
/// the caller has no permission for: file modes and the `disk` group stop
/// mattering for anyone who knows the passphrase. The check is read-write because
/// that is how Veracage mounts a volume and what the filesystem check needs.
///
/// Done in a child, so the uid drop is thrown away with it. There is a residual
/// TOCTOU (we check a path and root opens it again later); closing that means
/// passing the descriptor through to cryptsetup, which is a bigger change.
fn check_caller_can_open(source: &Path, human_uid: u32, human_gid: u32) {
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        fail_errno("fork(access check)");
    }
    if pid == 0 {
        let path = match CString::new(source.as_os_str().as_bytes()) {
            Ok(p) => p,
            Err(_) => unsafe { libc::_exit(1) },
        };
        unsafe {
            // Supplementary groups first, while still root: otherwise the caller
            // would keep root's group memberships and the check would be laxer
            // than the caller really is.
            if libc::setgroups(0, std::ptr::null()) != 0
                || libc::setresgid(human_gid, human_gid, human_gid) != 0
                || libc::setresuid(human_uid, human_uid, human_uid) != 0
            {
                libc::_exit(1);
            }
            let fd = libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC);
            libc::_exit(if fd < 0 { 1 } else { 0 });
        }
    }
    if wait_for(pid) != 0 {
        fail(
            &format!(
                "{} is not yours to open: the account that authenticated cannot \
                 open it read-write",
                source.display()
            ),
            2,
        );
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

/// Probe the decrypted volume ONCE for both properties the mount needs: its
/// filesystem label and type. `-p` probes the device directly instead of
/// answering from the /run/blkid cache, which can hold a stale entry for a
/// reused dm device (a wrong TYPE would silently mount an ownerless volume
/// read-only). `-o export` prints `KEY=value` lines, so a missing key is simply
/// absent rather than an unlabelled empty line.
fn probe_fs(dm_path: &str) -> (Option<String>, Option<String>) {
    let probe = Command::new(tool("blkid"))
        .args(["-p", "-o", "export", dm_path])
        .output();
    let out = match probe {
        Ok(out) => out,
        Err(_) => return (None, None),
    };
    let mut label = None;
    let mut fstype = None;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        match line.split_once('=') {
            // LABEL, not LABEL_FATBOOT / LABEL_ENC: match the key exactly.
            Some(("LABEL", v)) => label = sanitized(v),
            Some(("TYPE", v)) => fstype = sanitized(v),
            _ => {}
        }
    }
    (label, fstype)
}

/// A blkid value made safe to carry: the label names the vault in the sandbox
/// file manager and is forwarded via `env::set_var`, so strip control chars (a
/// NUL would make `set_var` panic/abort; others would corrupt the title). The
/// value comes from an attacker-supplied volume, so it is not trusted.
fn sanitized(raw: &str) -> Option<String> {
    let s: String = raw.trim().chars().filter(|c| !c.is_control()).collect();
    (!s.is_empty()).then_some(s)
}

/// Validate a caller-supplied exchange directory before root idmap-mounts it.
/// Opens it `O_DIRECTORY|O_NOFOLLOW` (a final-component symlink fails) and requires
/// a directory owned by the human. Intermediate symlinks aren't fully chased, but
/// the owner check rejects anything resolving to a root/other-owned dir (e.g.
/// `--exchange /etc`); a symlink to another of the human's OWN dirs is harmless
/// (their own data, and a same-uid attacker already has it).
fn validated_exchange(path: &str, human_uid: u32) -> Option<std::fs::File> {
    use std::os::unix::fs::MetadataExt;   // OpenOptionsExt is imported at the top
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
        .ok()?;
    let md = f.metadata().ok()?;
    // The FILE is returned, not the path: what was checked is what gets mounted
    // (see idmap::idmap_mount_at). Handing the path back let the caller, who owns
    // the parent directory, swap the final component between this check and the
    // mount and have root clone a different tree into the sandbox.
    (md.is_dir() && md.uid() == human_uid).then_some(f)
}

/// cryptsetup-open `source` and idmap-mount it at `<WORKSPACE>/<label>` in the
/// CURRENT mount namespace (the workspace tmpfs must already be mounted: the
/// bootstrap child mounts it after unshare; the add child inherits it via setns).
/// The label is read from the decrypted device, so the mountpoint is only known
/// here. Returns (mountpoint, label). Fails (closing the dm) on any mount error.
/// Filesystems checked before mounting, and their checker is `fsck.<type>`. All of
/// them take `-p`: fix what is unambiguous, never ask a question, since there is no
/// terminal here. NTFS is deliberately absent, `fsck.ntfs` is not a repair tool.
const FSCK_TYPES: &[&str] = &["ext2", "ext3", "ext4", "vfat", "exfat"];

/// The fsck exit bits that still allow the mount: 0 (clean), 1 (errors corrected),
/// 2 (corrected, a reboot would be advised for a system disk). Anything else means
/// uncorrected errors (4), an operational error (8), bad usage (16) or a cancelled
/// run (32), and the filesystem needs attention this cannot give it.
const FSCK_CORRECTED: i32 = 1 | 2;

fn fsck_ok(code: Option<i32>) -> bool {
    match code {
        Some(code) => code & !FSCK_CORRECTED == 0,
        None => false, // killed by a signal: nothing was concluded
    }
}

/// The checker for this filesystem, if it is one we check and its binary is
/// installed. A checker the host does not have is no reason to refuse a volume.
fn fsck_checker(fstype: Option<&str>) -> Option<String> {
    let fstype = match fstype {
        Some(t) if FSCK_TYPES.contains(&t) => t,
        _ => return None,
    };
    let bin = tool(&format!("fsck.{fstype}"));
    Path::new(&bin).is_absolute().then_some(bin)
}

/// Check and repair the decrypted filesystem BEFORE mounting it. A volume that was
/// not dismounted cleanly (a crash, a lost device, a pulled disk) carries a dirty
/// filesystem, and mounting one dirty compounds the damage. Preen mode fixes what
/// it safely can; anything left is reported so the caller refuses the mount, which
/// leaves the user free to run a full check themselves instead of Veracage guessing
/// at their data.
fn fsck_volume(dm_path: &str, fstype: Option<&str>) -> Result<(), String> {
    let checker = match fsck_checker(fstype) {
        Some(c) => c,
        None => return Ok(()),
    };
    let st = match Command::new(&checker).args(["-p", dm_path]).status() {
        Ok(st) => st,
        Err(e) => {
            // The binary is there but would not run: still not a reason to
            // withhold the volume.
            eprintln!("veracage: {checker} did not run: {e}");
            return Ok(());
        }
    };
    if fsck_ok(st.code()) {
        if st.code() != Some(0) {
            eprintln!("veracage: {checker} repaired the filesystem before mounting");
        }
        return Ok(());
    }
    Err(format!(
        "the filesystem on this volume needs a repair Veracage will not make for you \
         ({checker} exit {}). Open the volume with your usual tool and run a full \
         check on it, then try again.",
        match st.code() {
            Some(c) => c.to_string(),
            None => "signal".to_string(),
        }
    ))
}

/// Overwrite the passphrase this process still holds, and drop the buffer.
///
/// It reached cryptsetup on its stdin and is never read again, but the process
/// holding it is not short-lived: the parent waits for the leader and closes the
/// volume on the way out, so an unwiped copy would sit in root-owned heap for as
/// long as the volume is open. `write_volatile` because a plain store to memory
/// nothing reads again is dead code the compiler is free to drop, and a fence so
/// the wipe is not sunk past what follows.
fn wipe_passphrase(passphrase: &mut Option<Vec<u8>>) {
    if let Some(p) = passphrase.as_mut() {
        wipe(p);
    }
    *passphrase = None;
}

/// Overwrite a buffer with zeroes, for real.
fn wipe(buf: &mut [u8]) {
    for b in buf.iter_mut() {
        unsafe { ptr::write_volatile(b, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

#[allow(clippy::too_many_arguments)]
fn mount_volume_at_workspace(
    source: &Path,
    backend: Option<Backend>,
    passphrase: Option<&[u8]>,
    human_uid: u32,
    human_gid: u32,
    vault_uid: u32,
    vault_gid: u32,
    dm_name: &str,
) -> (PathBuf, String) {
    let backend = backend.unwrap_or_else(|| crypt::detect(source));
    // Exit code 4 = decrypt failed (wrong passphrase, usually): see EXIT_CRYPT_FAILED.
    crypt::open(source, backend, dm_name, passphrase)
        .unwrap_or_else(|e| fail(&format!("{e}"), EXIT_CRYPT_FAILED));
    let dm_path = format!("/dev/mapper/{dm_name}");

    // One probe for both the label (names the volume) and the filesystem type
    // (decides the stage-mount options below).
    let (probed_label, fstype) = probe_fs(&dm_path);
    let label = probed_label.unwrap_or_else(|| {
        source.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Volume".into())
    });
    // Check the filesystem while nothing has it mounted: this is the only moment
    // when a repair is both possible and safe. A volume that fails leaves nothing
    // behind (the dm is closed) so the user can check it themselves.
    if let Err(e) = fsck_volume(&dm_path, fstype.as_deref()) {
        let _ = crypt::close(dm_name);
        fail(&e, EXIT_FSCK_FAILED);
    }
    let mountpoint = workspace_path(&label, source);
    // Staging mount target: a DOT-prefixed sibling of the volume dir under the
    // workspace (`<WORKSPACE>/.<label>.raw`). The leading dot means the leader's
    // scan_volumes skips it, so a transient staging dir (or one leaked by an
    // error below) never surfaces as a phantom "volume" in the title / Places /
    // Close menu. Cleaned up on EVERY exit path (all three below).
    let raw = {
        let parent = mountpoint.parent().unwrap_or(Path::new(WORKSPACE));
        let base = mountpoint.file_name().map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| label.clone());
        parent.join(format!(".{base}.raw"))
    };

    // Stage-mount the decrypted fs with ownership appropriate to the filesystem
    // (an ownerless FAT/exFAT/NTFS gets uid=/gid= options, a POSIX fs is mounted
    // plain and its root inode chowned), then idmap-clone it to the workspace path.
    std::fs::create_dir_all(&raw).unwrap_or_else(|e| fail(&format!("mkdir staging: {e}"), 1));
    set_mode(&raw, 0o700);
    let (opts, needs_chown) = stage_opts(fstype.as_deref(), human_uid, human_gid);
    let st = Command::new(tool("mount"))
        .args(["-o", &opts])
        .arg(&dm_path)
        .arg(&raw)
        .status()
        .unwrap_or_else(|e| fail(&format!("spawn mount: {e}"), 1));
    if !st.success() {
        let _ = std::fs::remove_dir(&raw);
        let _ = crypt::close(dm_name);
        fail(&format!("mount failed: {}", st.code().unwrap_or(-1)), st.code().unwrap_or(1));
    }
    if needs_chown {
        try_chown(&raw, human_uid, human_gid);
    }

    std::fs::create_dir_all(&mountpoint).unwrap_or_else(|e| fail(&format!("mkdir mountpoint: {e}"), 1));
    set_mode(&mountpoint, 0o755);
    if let Err(e) = idmap::idmap_mount(&raw, &mountpoint, human_uid, human_gid, vault_uid, vault_gid, 0) {
        let _ = Command::new(tool("umount")).arg(&raw).status();
        let _ = std::fs::remove_dir(&raw);
        // Remove the mountpoint dir too: this runs inside the LIVE leader's
        // namespace, and a leftover non-dot dir under the workspace tmpfs would
        // be reported by scan_volumes as a phantom, unclosable volume.
        let _ = std::fs::remove_dir(&mountpoint);
        let _ = crypt::close(dm_name);
        fail(&format!("idmap_mount: {e}"), 1);
    }
    let _ = Command::new(tool("umount")).arg(&raw).status();
    let _ = std::fs::remove_dir(&raw);
    (mountpoint, label)
}

#[allow(clippy::too_many_arguments)]
fn session_child(
    vault_uid: u32,
    vault_gid: u32,
    human_uid: u32,
    human_gid: u32,
    source: Option<&Path>,
    backend: Option<Backend>,
    sid: &str,
    dm_name: Option<&str>,
    ctl_path: &Path,
    cont: &Path,
    args: &mut Args,
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

    // cryptsetup + idmap-mount the volume into the workspace (shared with the
    // add-volume path). The label is only known post-cryptsetup, so the leader is
    // told the resulting mountpoint by injecting it into the continuation argv.
    // An EMPTY session mounts no volume: the leader runs against the empty
    // workspace root (+ /exchange), and volumes join later via the add path.
    let (mountpoint, volume_label) = match (source, dm_name) {
        (Some(src), Some(dm)) => mount_volume_at_workspace(
            src, backend, args.passphrase.as_deref(),
            human_uid, human_gid, vault_uid, vault_gid, dm,
        ),
        _ => (PathBuf::from(WORKSPACE), String::new()),
    };
    // cryptsetup has it; this copy has no further use. The exec below would drop
    // it anyway, but there is mount work in between and this is root.
    wipe_passphrase(&mut args.passphrase);
    let vault_run = PathBuf::from(format!("/run/veracage/session-{sid}.run"));

    // Shared directory (idmap a human-owned host dir), bound at /exchange,
    // top-level, presented as veracage-owned. The mount lives OUTSIDE the
    // workspace (a `session-<sid>.x` sibling): the sandbox binds the whole workspace at /vaults
    // recursively, and /vaults is HOME, so a mount under it would surface the
    // host-plaintext directory inside HOME and break the "everything in HOME is
    // encrypted at rest" invariant (an app writing under ~/.exchange, or any
    // recursive copy of ~, would land plaintext on the host disk).
    if let Some(xpath) = args.exchange.as_deref() {
        match validated_exchange(xpath, human_uid) {
            Some(x) => {
                let xmp = PathBuf::from(format!("/run/veracage/session-{sid}.x"));
                let _ = std::fs::create_dir_all(&xmp);
                set_mode(&xmp, 0o700);
                match idmap::idmap_mount_at(
                    x.as_raw_fd(), &xmp, human_uid, human_gid, vault_uid, vault_gid,
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

    drop_to_vault_user(vault_uid, vault_gid);

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

    // Inject the helper-computed mountpoint into the leader's argv: the CLI can't
    // know `<WORKSPACE>/<label>` (the label is read post-cryptsetup, above).
    let mut rest = args.rest.clone();
    rest.push("--mountpoint".to_string());
    rest.push(mountpoint.display().to_string());
    check_continuation_argv(&rest);
    // The exec below also closes our inherited copy of the session flock (Rust
    // opens files O_CLOEXEC), which is what releases it. See the fork site.
    let err = Command::new(cont).args(&rest).exec();
    fail(&format!("exec continuation {}: {err}", cont.display()), 127);
}

/// Shared-workspace bootstrap parent. Brings up the compositor if
/// needed, records the session lock (pre-fork, for crash safety), forks the
/// session leader (`session_child`), records its pid, and on its exit closes the
/// volume's dm + drops the session lock/pid/socket. The vault mounts died with
/// the leader's NS, so there is no mountpoint tidy here.
fn run_session_bootstrap(
    args: &mut Args,
    human_uid: u32,
    human_gid: u32,
    vault_uid: u32,
    vault_gid: u32,
    source: Option<&Path>,
    flock: std::fs::File,
) -> ! {
    // Owned: the passphrase is wiped through `args` further down, and a `sid`
    // still borrowed from it would keep the whole struct borrowed until then.
    let sid_owned = args.session.clone().unwrap();
    let sid = sid_owned.as_str();
    check_session_caller(sid, human_uid);
    let runtime_dir = setenv_value(args, "XDG_RUNTIME_DIR")
        .unwrap_or_else(|| fail("XDG_RUNTIME_DIR not forwarded; need it for the control socket", 2));
    // Key the control socket by the source vault hash, so `veracage list/close
    // <vault>` keeps finding it (one bootstrap vault per
    // session). An EMPTY session has no vault, so it keys by the session id; the
    // add-volume path finds the leader by `session-<sid>.pid`, not this socket.
    let ctl_path = PathBuf::from(&runtime_dir)
        .join("veracage/sessions")
        .join(match source {
            Some(s) => format!("{}.sock", vault_hash(s)),
            None => format!("session-{sid}.sock"),
        });

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

    let lock_path = format!("/run/veracage/session-{sid}.lock");

    // Serialized with any concurrent open/teardown for this sid (main() took the
    // flock before the bootstrap-vs-add decision and passed it down). A stale
    // lock here (crashed predecessor whose ExecStopPost failed) is recovered
    // first: close its orphan dms and start from a clean slate.
    // Devices a recovery could not close, carried into the new lock below.
    let mut carried: Vec<String> = Vec::new();
    if Path::new(&lock_path).exists() {
        eprintln!("veracage-helper: recovering a stale session lock for sid {sid}");
        if !close_all_session_dms(sid) {
            // A device that would not close is still decrypted. Its line is the
            // only thing that will ever close it (the ExecStopPost cleanup and
            // the sleep hook both read the lock), so carry it rather than start
            // "from a clean slate" and lose the key in RAM.
            carried = std::fs::read_to_string(&lock_path)
                .map(|b| lock_survivors(&b, |dm| Path::new(&format!("/dev/mapper/{dm}")).exists()))
                .unwrap_or_default();
            eprintln!(
                "veracage-helper: {} volume(s) of the stale session are still open; \
                 carrying them into the new session lock",
                carried.len()
            );
        }
        let _ = std::fs::remove_file(&lock_path);
    }

    // Each bootstrap gets a random GENERATION nonce in the lock header. Our
    // teardown only touches the session files if the lock still carries OUR
    // generation: a successor session that reused the sid while we were still
    // tearing down must not have its lock/pidfile/dms clobbered.
    let gen = random_hex(8);
    create_session_lock(sid, human_uid, &gen);
    append_session_lines(sid, &carried);
    // Only a VOLUME bootstrap records a dm in the lock pre-fork; an empty session
    // starts with no volume (they join later via the add-volume path, which
    // appends their own lines). Pre-fork volume line uses an empty label; the
    // dm_name is what teardown needs to close the device.
    let dm_name = source.map(|_| random_dm_name());
    if let (Some(s), Some(dm)) = (source, dm_name.as_deref()) {
        append_session_volume(sid, human_uid, dm, "", &vault_hash(s), &source_key(s));
    }

    // The persistent compositor is brought up by the CLI (`veracage _up`, its own
    // systemd --user unit) BEFORE this mount pkexec, NOT forked here, which would
    // put it in this session's cgroup and let its stop kill the shared compositor.
    // The leader just verifies the socket is present.

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        fail_errno("fork");
    }
    if pid == 0 {
        session_child(vault_uid, vault_gid, human_uid, human_gid, source, args.backend,
                      sid, dm_name.as_deref(), &ctl_path, &cont, args);
    }

    // Parent: the child has the passphrase and does the cryptsetup open. This
    // copy is never read again, and this process outlives the whole session.
    wipe_passphrase(&mut args.passphrase);

    // Parent (root, original NS): the session leader's pid (+ start-time) lets the
    // add-volume path find + verify the workspace NS holder.
    write_session_pidfile(sid, pid);
    // Release OUR reference to the session flock. The lock itself stays held until
    // the CHILD's inherited copy closes, which happens when it execs the leader -
    // i.e. exactly when `verify_session_leader` starts recognising the session.
    //
    // That is load-bearing and easy to miss: an flock lives on the open file
    // description, which fork shares, so dropping it here does NOT release it.
    // Between the fork and the exec the child is still root, so a concurrent
    // `veracage open` would see no live session and take the bootstrap path over
    // this one - and it is this inherited reference that stops it, by making that
    // second helper block in `session_flock` until we are established. Measured
    // by taking `flock -n` on the sidecar one second into a deliberately slow
    // open, with no leader up yet: the lock is HELD, and the holder is this
    // helper. Anything that changes the fd handling around this fork has to keep
    // that property.
    drop(flock);
    install_signal_forwarding(pid);
    let rc = wait_for(pid);

    // TEST-ONLY fault injection (debug builds only; compiled out of `make install`
    // release binaries): widen the teardown window so a test can deterministically
    // interleave a successor bootstrap for the SAME sid between the leader's death
    // and this teardown, proving the generation guard below protects the successor.
    #[cfg(debug_assertions)]
    if let Some(ms) = std::env::var("VERACAGE_DEBUG_TEARDOWN_DELAY_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
    {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }

    // The leader (holding the workspace NS) has exited: every /vaults/* mount is
    // gone with the NS, so close EVERY volume's dm from the session lock (this
    // volume plus any added later via setns). ExecStopPost `cleanup --session` is
    // the crash-safety backup (idempotent; it no-ops while the leader is alive).
    // Re-serialized: a concurrent `veracage open` may be bootstrapping a NEW
    // session for this sid right now, if the lock's generation is no longer
    // ours, the successor already recovered our dms (its bootstrap closes every
    // dm in the stale lock) and owns every session-<sid> file; touch nothing.
    let _flock = session_flock(sid);
    let ours = std::fs::read_to_string(&lock_path)
        .map(|b| lock_generation(&b) == Some(gen.as_str()))
        .unwrap_or(false);
    if !ours {
        std::process::exit(rc);
    }
    let all_closed = close_all_session_dms(sid);
    let _ = std::fs::remove_dir_all(format!("/run/veracage/session-{sid}.run"));
    // The exchange idmap mount died with the leader's NS; only its (host-visible)
    // empty target dir remains.
    let _ = std::fs::remove_dir(format!("/run/veracage/session-{sid}.x"));
    let _ = std::fs::remove_file(&ctl_path);
    // Belt on top of the generation check: only drop the pidfile if it still
    // names OUR leader.
    let pid_path = format!("/run/veracage/session-{sid}.pid");
    let pid_is_ours = std::fs::read_to_string(&pid_path)
        .ok()
        .and_then(|b| b.split_whitespace().next().map(|p| p == pid.to_string()))
        .unwrap_or(false);
    if pid_is_ours {
        let _ = std::fs::remove_file(&pid_path);
    }
    if all_closed {
        let _ = std::fs::remove_file(&lock_path);
    }
    std::process::exit(rc);
}

/// Subsequent open: a live session already holds the workspace NS, so
/// JOIN it (setns) and mount this volume there, then EXIT. The session leader
/// keeps holding the mount, and teardown closes this dm from the session lock. No
/// leader, no compositor bring-up, no ExecStopPost teardown (the CLI's
/// `cleanup --session` no-ops while the leader is alive).
#[allow(clippy::too_many_arguments)]
fn run_session_add(
    args: &mut Args,
    human_uid: u32,
    human_gid: u32,
    vault_uid: u32,
    vault_gid: u32,
    source: &Path,
    leader_pid: i32,
    // Held (not used) for our whole lifetime: serializes the lock append + mount
    // against a concurrent teardown/bootstrap for the same sid. Released on exit.
    _flock: std::fs::File,
) -> ! {
    let sid = args.session.as_deref().unwrap();

    // Duplicate-open guard: refuse a source that is ALREADY open in this session.
    // Without it a second `veracage open <same vault>` would cryptsetup-open the
    // same backing file again (a fresh loop device) and rw-mount the same
    // filesystem twice at `<label>-2`: filesystem corruption. The lock records
    // each volume's source hash; a line whose dm still exists means the volume is
    // genuinely open (a per-volume close drops the line once its dm is gone, so a
    // closed volume can be reopened).
    let vhash = vault_hash(source);
    let skey = source_key(source);
    let lock_body = std::fs::read_to_string(format!("/run/veracage/session-{sid}.lock"))
        .unwrap_or_default();
    // Both keys: dev:ino catches the same container under another name (a hard
    // link, a second mount of the same filesystem), and the path hash still
    // catches lines written before dev:ino was recorded.
    let mut open_dms = lock_dms_for_source(&lock_body, &skey);
    open_dms.extend(lock_dms_for_vhash(&lock_body, &vhash));
    for dm in open_dms {
        if Path::new(&format!("/dev/mapper/{dm}")).exists() {
            fail(&format!(
                "{} is already open in the running workspace; close that volume \
                 first (Close volume in the toolbar, or `veracage close-volume \
                 <label>`)", source.display()), 2);
        }
    }

    let dm_name = random_dm_name();
    // Append to the session lock BEFORE the mount (crash safety): the dm_name is
    // pre-generated, so even an early kill leaves teardown a device to close.
    append_session_volume(sid, human_uid, &dm_name, "", &vhash, &skey);

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        fail_errno("fork");
    }
    if pid == 0 {
        // Child: join the leader's mount namespace (the workspace), mount the
        // volume there, exit. setns must run in a child so the parent stays in the
        // host NS. The pid was verified (owner + start-time) before we got here.
        let ns = format!("/proc/{leader_pid}/ns/mnt");
        let c = CString::new(ns.clone()).unwrap();
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            fail_errno(&format!("open {ns}"));
        }
        if unsafe { libc::setns(fd, libc::CLONE_NEWNS) } != 0 {
            fail_errno("setns(mnt)");
        }
        unsafe { libc::close(fd) };
        // In the workspace NS now (tmpfs at WORKSPACE inherited from the leader).
        let _ = mount_volume_at_workspace(
            source, args.backend, args.passphrase.as_deref(),
            human_uid, human_gid, vault_uid, vault_gid, &dm_name,
        );
        wipe_passphrase(&mut args.passphrase);
        std::process::exit(0);
    }
    wipe_passphrase(&mut args.passphrase);

    // Parent: wait for the add-child. On failure, close this volume's dm (the child
    // may have opened it before erroring) so it doesn't leak; the stale lock line
    // is harmless (teardown's close is guarded by the device existing).
    let rc = wait_for(pid);
    if rc != 0 {
        let dm_path = format!("/dev/mapper/{dm_name}");
        if Path::new(&dm_path).exists() {
            let _ = crypt::close(&dm_name);
        }
        std::process::exit(rc);
    }

    // The volume joined a RUNNING session, so the leader argv we were handed
    // (`--first`, the app to auto-launch: the user's explicit pick or their
    // default file manager) was never consumed. Hand the spec to the live
    // leader instead: drop it, root-written, into the vault-owned runtime dir
    // (0700 veracage; unreachable from sandboxes and the human uid), where the
    // leader's serve loop picks it up. Same human-trust UX data as `--apps`.
    if let Some(spec) = rest_value(&args.rest, "--first") {
        write_launch_request(sid, spec, vault_uid, vault_gid);
    }
    std::process::exit(0);
}

/// The value following `flag` in the leader argv after `--` (None if absent).
fn rest_value<'a>(rest: &'a [String], flag: &str) -> Option<&'a str> {
    let i = rest.iter().position(|a| a == flag)?;
    rest.get(i + 1).map(|s| s.as_str())
}

/// Write the auto-launch app spec for the leader to consume (add-volume path).
/// Best-effort: a mount without the popup beats failing a completed mount.
///
/// The target dir is vault-owned, so the veracage uid can plant a symlink (or
/// swap the file) there. Everything root does here therefore goes through ONE
/// fd: `O_CREAT|O_EXCL|O_NOFOLLOW` refuses a pre-planted name, the mode is set
/// at creation, and the ownership handover is `fchown` on that same fd - never a
/// path lookup a replacement could redirect.
fn write_launch_request(sid: &str, spec: &str, vault_uid: u32, vault_gid: u32) {
    if spec.len() > 4096 {
        return; // a real {name, exec} spec is tiny; don't relay junk
    }
    let path = PathBuf::from(format!("/run/veracage/session-{sid}.run/launch.req"));
    // Unlink any leftover from a crashed open first: remove_file acts on the
    // NAME, so it drops a planted symlink rather than following it.
    let _ = std::fs::remove_file(&path);
    let opened = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path);
    let mut file = match opened {
        Ok(file) => file,
        Err(_) => {
            eprintln!("veracage-helper: launch request not written (no app auto-launched)");
            return;
        }
    };
    if file.write_all(spec.as_bytes()).is_err() {
        let _ = std::fs::remove_file(&path);
        return;
    }
    // fchown, not chown: the leader must be able to read it, and the fd cannot
    // be redirected between the create above and this call.
    if unsafe { libc::fchown(file.as_raw_fd(), vault_uid, vault_gid) } != 0 {
        eprintln!(
            "veracage-helper: launch request fchown: {}",
            std::io::Error::last_os_error()
        );
        let _ = std::fs::remove_file(&path);
    }
}

fn main() {
    suppress_core_dumps();
    let mut args = parse_args();

    if unsafe { libc::geteuid() } != 0 {
        fail("must be invoked as root (via pkexec)", 2);
    }

    // Identities: human from pkexec; vault from a fixed system user. Neither
    // from argv.
    let (human_uid, human_gid) = resolve_caller().unwrap_or_else(|e| fail(&e, 2));
    let (vault_uid, vault_gid) = resolve_vault_user(human_uid).unwrap_or_else(|e| fail(&e, 2));

    // XDG_RUNTIME_DIR is forwarded by the caller and used AS ROOT to create/chown
    // the control-socket dir and to reach the host Wayland socket. It MUST be the
    // caller's own runtime dir, a crafted value (with a planted symlink) would
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

    // `--spawn-compositor`: bring up the persistent shared compositor and exec:
    // no vault, no mount NS, no fork, no cleanup.
    if args.spawn_compositor {
        spawn_compositor(human_uid, human_gid, vault_uid, vault_gid, &args, &args.rest);
    }

    // `--close-volume <label>`: close ONE volume of a running session.
    // No --source; joins the leader's NS and dismounts/closes just that volume.
    if args.close_volume.is_some() {
        run_close_volume(&args, human_uid, vault_uid);
    }

    // `--empty-session`: bootstrap a session leader with NO volume (front-door
    // scratchpad). Idempotent: a no-op if a live leader already holds the NS.
    if args.empty_session {
        let sid = args
            .session
            .as_deref()
            .unwrap_or_else(|| fail("--empty-session needs --session", 2));
        check_session_caller(sid, human_uid);
        let flock = session_flock(sid);
        if verify_session_leader(sid, vault_uid).is_some() {
            std::process::exit(0); // a session already exists
        }
        run_session_bootstrap(&mut args, human_uid, human_gid, vault_uid, vault_gid, None, flock);
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
    check_caller_can_open(&source, human_uid, human_gid);

    // Shared-workspace open (the only mount mode). The helper picks bootstrap
    // vs. add-volume by whether a LIVE, verified session leader already holds
    // the workspace NS (so the CLI needs no session-liveness check: it always
    // passes --session <uid>).
    let sid = args
        .session
        .as_deref()
        .unwrap_or_else(|| fail("--session required", 2));
    check_session_caller(sid, human_uid);
    // Serialize the bootstrap-vs-add decision (and everything it leads to)
    // against concurrent opens/teardowns for the same sid: without this, an
    // open racing a slow teardown appends to a lock the teardown is about to
    // act on, and the teardown then deletes the NEW session's pidfile / closes
    // its dm. The callee decides when to release (bootstrap: before waiting
    // out the leader; add: on exit).
    let flock = session_flock(sid);
    match verify_session_leader(sid, vault_uid) {
        Some(leader_pid) => {
            run_session_add(&mut args, human_uid, human_gid, vault_uid, vault_gid, &source,
                            leader_pid, flock)
        }
        None => {
            run_session_bootstrap(&mut args, human_uid, human_gid, vault_uid, vault_gid,
                                  Some(&source), flock)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wiped_buffer_keeps_none_of_the_passphrase() {
        let mut secret = b"correct horse battery staple".to_vec();
        wipe(&mut secret);
        assert!(secret.iter().all(|b| *b == 0), "{secret:?}");
    }

    #[test]
    fn wiping_the_passphrase_also_drops_it() {
        // Both halves matter: the bytes are overwritten before the buffer is
        // freed, and nothing is left holding it afterwards.
        let mut passphrase = Some(b"hunter2".to_vec());
        wipe_passphrase(&mut passphrase);
        assert!(passphrase.is_none());
        wipe_passphrase(&mut passphrase);   // idempotent: teardown may run twice
    }

    #[test]
    fn fsck_lets_a_clean_or_repaired_filesystem_through_and_nothing_else() {
        // 0 clean, 1 corrected, 2 corrected + reboot advised, 3 both bits.
        for code in [0, 1, 2, 3] {
            assert!(fsck_ok(Some(code)), "exit {code} should allow the mount");
        }
        // 4 uncorrected, 8 operational, 12 both, 16 usage, 32 cancelled, 128 lib.
        for code in [4, 8, 12, 16, 32, 128] {
            assert!(!fsck_ok(Some(code)), "exit {code} must refuse the mount");
        }
        // Killed by a signal: nothing was concluded, so do not mount.
        assert!(!fsck_ok(None));
    }

    #[test]
    fn only_filesystems_with_a_preen_checker_are_checked() {
        // No type probed, and types we deliberately leave alone.
        assert_eq!(fsck_checker(None), None);
        assert_eq!(fsck_checker(Some("ntfs")), None);
        assert_eq!(fsck_checker(Some("btrfs")), None);
        assert_eq!(fsck_checker(Some("")), None);
        // The ones we do check, when the host has the checker installed (a host
        // without it must simply skip the check, which is what None means here).
        for fstype in ["ext4", "vfat", "exfat"] {
            let installed = Path::new(&tool(&format!("fsck.{fstype}"))).is_absolute();
            assert_eq!(
                fsck_checker(Some(fstype)).is_some(),
                installed,
                "{fstype}: checker presence and decision disagree"
            );
        }
    }

    #[test]
    fn ownerless_filesystems_get_uid_mount_options() {
        // FAT/exFAT/NTFS carry no per-file ownership: they must be mounted with
        // uid=/gid= (and NOT chowned, which returns EPERM) or the vault ends up
        // read-only. Case-insensitive: blkid reports lowercase, but don't rely on it.
        for fs in ["vfat", "exfat", "ntfs", "ntfs3", "msdos", "VFAT"] {
            assert!(is_ownerless_fs(Some(fs)), "{fs} must be treated as ownerless");
            let (opts, needs_chown) = stage_opts(Some(fs), 1000, 1000);
            assert!(opts.contains("uid=1000") && opts.contains("umask=0077"));
            assert!(!needs_chown);
        }
        // A POSIX fs (and an unprobeable device) keeps the plain mount + chown.
        for fs in [Some("ext4"), Some("btrfs"), Some("xfs"), None] {
            assert!(!is_ownerless_fs(fs));
            let (opts, needs_chown) = stage_opts(fs, 1000, 1000);
            assert_eq!(opts, "nodev,nosuid");
            assert!(needs_chown);
        }
    }

    #[test]
    fn sanitized_strips_control_chars_and_empties() {
        // The label reaches env::set_var and the window title, from an
        // attacker-supplied volume.
        assert_eq!(sanitized("  Work  ").as_deref(), Some("Work"));
        assert_eq!(sanitized("Wo\u{0}rk\n").as_deref(), Some("Work"));
        assert_eq!(sanitized("   "), None);
        assert_eq!(sanitized("\u{0}\n"), None);
    }

    #[test]
    fn rest_value_reads_the_flag_that_follows() {
        let rest: Vec<String> = ["_leader", "--apps", "[]", "--first", "{\"exec\":\"kate\"}"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(rest_value(&rest, "--first"), Some("{\"exec\":\"kate\"}"));
        assert_eq!(rest_value(&rest, "--apps"), Some("[]"));
        assert_eq!(rest_value(&rest, "--nope"), None);
        // A trailing flag with no value must not panic or wrap around.
        assert_eq!(rest_value(&["--first".to_string()], "--first"), None);
    }

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

    #[test]
    fn is_veracage_dm_matches_cleanup_regex() {
        assert!(is_veracage_dm("veracage-0123456789ab"));
        assert!(!is_veracage_dm("veracage-0123456789")); // 10 hex
        assert!(!is_veracage_dm("veracage-0123456789ABCD")); // upper + len
        assert!(!is_veracage_dm("veracage-../../x"));
        assert!(!is_veracage_dm("evil"));
        assert!(!is_veracage_dm(""));
    }

    #[test]
    fn proc_starttime_reads_self() {
        let pid = std::process::id() as i32;
        assert!(proc_starttime(pid).map(|s| s > 0).unwrap_or(false));
    }

    #[test]
    fn verify_session_leader_none_without_pidfile() {
        // A sid with no session pidfile → no live session (caller bootstraps).
        assert!(verify_session_leader("987654321098765", 0).is_none());
    }

    #[test]
    fn label_ok_rejects_traversal() {
        assert!(label_ok("volA"));
        assert!(label_ok("Work_Docs"));
        assert!(!label_ok(""));
        assert!(!label_ok("."));
        assert!(!label_ok(".."));
        assert!(!label_ok("a/b"));
        assert!(!label_ok("../../etc"));
    }

    #[test]
    fn lock_dms_for_vhash_matches_third_field() {
        let body = "user_uid=1000\n\
                    volume=veracage-aaaaaaaaaaaa\tWork\t0631c55cceb0614f\n\
                    volume=veracage-bbbbbbbbbbbb\t\tdeadbeefdeadbeef\n\
                    volume=veracage-cccccccccccc\tX\t0631c55cceb0614f\n";
        assert_eq!(
            lock_dms_for_vhash(body, "0631c55cceb0614f"),
            vec!["veracage-aaaaaaaaaaaa".to_string(), "veracage-cccccccccccc".to_string()]
        );
        assert_eq!(lock_dms_for_vhash(body, "deadbeefdeadbeef"),
                   vec!["veracage-bbbbbbbbbbbb".to_string()]);
        assert!(lock_dms_for_vhash(body, "0000000000000000").is_empty());
    }

    /// A pipe with `bytes` already in it, plus its write end (dropped by the
    /// caller to signal EOF).
    fn pipe_with(bytes: &[u8]) -> (libc::c_int, libc::c_int) {
        let mut fds = [0 as libc::c_int; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        if !bytes.is_empty() {
            let n = unsafe {
                libc::write(fds[1], bytes.as_ptr() as *const libc::c_void, bytes.len())
            };
            assert_eq!(n as usize, bytes.len());
        }
        (fds[0], fds[1])
    }

    #[test]
    fn the_go_protocol_accepts_only_go_and_treats_everything_else_as_a_refusal() {
        let short = std::time::Duration::from_millis(200);

        // The one answer that continues a close.
        let (r, w) = pipe_with(b"go\n");
        assert!(wait_for_go_on(r, 0, short));
        unsafe { libc::close(r); libc::close(w) };

        // A cancel is EOF: the caller drops our stdin.
        let (r, w) = pipe_with(b"");
        unsafe { libc::close(w) };
        assert!(!wait_for_go_on(r, 0, short), "EOF must not continue the close");
        unsafe { libc::close(r) };

        // Anything else on the line is not our protocol.
        for junk in [&b"no\n"[..], b"GO\n", b" go\n", b"gogo\n"] {
            let (r, w) = pipe_with(junk);
            assert!(!wait_for_go_on(r, 0, short), "{:?} must not continue", junk);
            unsafe { libc::close(r); libc::close(w) };
        }

        // A flood with no newline is refused rather than buffered without bound:
        // this parks a ROOT process inside the session namespace, so it must not
        // be steerable by a caller that just keeps writing.
        let (r, w) = pipe_with(&[b'x'; 64]);
        assert!(!wait_for_go_on(r, 0, short));
        unsafe { libc::close(r); libc::close(w) };

        // Nothing at all: the budget expires and the volume is left alone.
        let (r, w) = pipe_with(b"");
        let t0 = std::time::Instant::now();
        assert!(!wait_for_go_on(r, 0, short));
        assert!(t0.elapsed() >= short, "it must wait out the budget, not spin");
        unsafe { libc::close(r); libc::close(w) };
    }

    #[test]
    fn a_second_volume_with_the_same_label_gets_its_own_directory() {
        // Two volumes can carry the same filesystem label. They must not land on
        // the same mountpoint: the second would mount over the first, and the
        // close menu would show one entry for two open volumes.
        let root = std::env::temp_dir().join(format!("vc-ws-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let src = Path::new("/tmp/work.vc");

        let first = workspace_path_in(&root, "work", src);
        assert_eq!(first, root.join("work"));
        std::fs::create_dir(&first).unwrap();

        let second = workspace_path_in(&root, "work", src);
        assert_eq!(second, root.join("work-2"));
        std::fs::create_dir(&second).unwrap();

        assert_eq!(workspace_path_in(&root, "work", src), root.join("work-3"));

        // The label is sanitized first, so a hostile one cannot escape the root.
        let evil = workspace_path_in(&root, "../../etc", src);
        assert_eq!(evil.parent(), Some(root.as_path()));
        assert!(!evil.to_string_lossy().contains(".."));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sandbox_plumbing_is_filtered_from_the_message_only() {
        // The close decision is made on the full holder list. Filtering the
        // plumbing out of DETECTION was the bug: with only a still-exiting bwrap
        // holding the volume, the helper saw "no holders", unmounted, and then
        // could not close the device - leaving it detached and still decrypted.
        let holders = vec!["bwrap".to_string(), "dbus-run-sessio".to_string()];
        assert!(!holders.is_empty(), "detection must still see them");
        assert_eq!(holder_names(&holders), "Apps inside Veracage");

        let mixed = vec!["bwrap".to_string(), "dolphin".to_string()];
        assert_eq!(holder_names(&mixed), "dolphin");
    }

    #[test]
    fn is_sandbox_wrapper_matches_the_15_char_comm_truncation() {
        // /proc/<pid>/comm is capped at 15 characters, so "dbus-run-session"
        // arrives truncated and an equality test misses it.
        assert!(is_sandbox_wrapper("bwrap"));
        assert!(is_sandbox_wrapper("dbus-run-sessio"));
        assert!(is_sandbox_wrapper("dbus-daemon"));
        assert!(is_sandbox_wrapper("veracage-reaper"));
        assert!(!is_sandbox_wrapper("dolphin"));
        assert!(!is_sandbox_wrapper("dbus"));   // a prefix, but not truncated at 15
    }

    #[test]
    fn lock_survivors_keeps_only_the_lines_whose_device_is_still_open() {
        // A recovery that could not close a device must carry its line forward:
        // the line is the only record any teardown has of that device.
        let body = "user_uid=1000\n\
                    generation=00aabbccddeeff11\n\
                    volume=veracage-aaaaaaaaaaaa\tWork\t0631c55cceb0614f\n\
                    volume=veracage-bbbbbbbbbbbb\tGone\tdeadbeefdeadbeef\n";
        let live = |dm: &str| dm == "veracage-aaaaaaaaaaaa";
        assert_eq!(
            lock_survivors(body, live),
            vec!["volume=veracage-aaaaaaaaaaaa\tWork\t0631c55cceb0614f".to_string()]
        );
        // Nothing still open -> nothing carried, and a header is never carried.
        assert!(lock_survivors(body, |_| false).is_empty());
    }

    #[test]
    fn lock_generation_parses_header() {
        assert_eq!(
            lock_generation("user_uid=1000\ngeneration=00aabbccddeeff11\nvolume=x\ty\tz\n"),
            Some("00aabbccddeeff11")
        );
        // A lock with no header carries no generation, so teardown must
        // treat the lock as not-ours and leave it to the ExecStopPost cleanup
        assert_eq!(lock_generation("user_uid=1000\nvolume=x\ty\tz\n"), None);
        assert_eq!(lock_generation(""), None);
    }

    #[test]
    fn suppress_core_dumps_clears_the_coredump_filter() {
        // The dump itself may still be created (the kernel ignores RLIMIT_CORE
        // when core_pattern is a pipe): what this guarantees is that it carries
        // no memory, here and in every process the helper execs.
        suppress_core_dumps();
        let filter = std::fs::read_to_string("/proc/self/coredump_filter").unwrap();
        assert_eq!(u32::from_str_radix(filter.trim(), 16).unwrap(), 0);
    }

    #[test]
    fn a_hard_link_does_not_get_past_the_duplicate_open_guard() {
        // The guard's whole job is to stop one container being opened twice: two
        // dm devices over one filesystem, both mounted read-write. Keyed on the
        // canonical PATH it was defeated by a second name for the same file.
        let dir = std::env::temp_dir().join(format!("vc-dup-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let a = dir.join("volume.vc");
        let b = dir.join("same-volume.vc");
        std::fs::write(&a, b"x").unwrap();
        let _ = std::fs::remove_file(&b);
        std::fs::hard_link(&a, &b).unwrap();

        let key = source_key(&a);
        assert!(!key.is_empty());
        assert_eq!(key, source_key(&b), "a hard link is the same container");
        assert_ne!(vault_hash(&a), vault_hash(&b), "but not the same path");

        let body = format!("user_uid=1000\nvolume=veracage-aaaaaaaaaaaa\tWork\t{}\t{}\n",
                           vault_hash(&a), key);
        assert_eq!(lock_dms_for_source(&body, &key),
                   vec!["veracage-aaaaaaaaaaaa".to_string()]);
        // And the old path-keyed lookup still answers for the line it wrote.
        assert_eq!(lock_dms_for_vhash(&body, &vault_hash(&a)),
                   vec!["veracage-aaaaaaaaaaaa".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lock_dms_for_source_ignores_lines_without_the_field() {
        // Written before dev:ino was recorded: no match, i.e. the same answer as
        // "not open", so an upgrade mid-session cannot produce a false refusal.
        let body = "user_uid=1000\nvolume=veracage-aaaaaaaaaaaa\tWork\t0631c55cceb0614f\n";
        assert!(lock_dms_for_source(body, "66306:1234").is_empty());
        assert!(lock_dms_for_source(body, "").is_empty());
    }

    #[test]
    fn lock_dms_for_vhash_ignores_legacy_two_field_lines() {
        // Pre-guard locks had no vault-hash field: those lines must never match
        // (no false "already open" refusals after an upgrade mid-session).
        let body = "user_uid=1000\nvolume=veracage-aaaaaaaaaaaa\tWork\n";
        assert!(lock_dms_for_vhash(body, "0631c55cceb0614f").is_empty());
        assert!(lock_dms_for_vhash("", "0631c55cceb0614f").is_empty());
    }
}
