//! Veracage privileged helper (Rust port of helpers/veracage-helper).
//!
//! Invoked as root via pkexec. Sequence:
//!
//!   1. fork()
//!   2. child:
//!        a. unshare(CLONE_NEWNS), make / rslave
//!        b. cryptsetup --type tcrypt --veracrypt open VAULT veracage-XXXX
//!        c. mount /dev/mapper/veracage-XXXX MOUNTPOINT (nodev,nosuid)
//!        d. drop privileges to the PKEXEC_UID caller (setresgid/setresuid)
//!        e. exec(continuation)
//!   3. parent:
//!        a. waitpid(child)
//!        b. cryptsetup close veracage-XXXX, tidy mountpoint + lock
//!
//! The mount is created inside the child's mount NS only — invisible to the
//! host. The dm-crypt device IS globally visible to root (kernel limitation,
//! documented in §3 of the design).
//!
//! SECURITY: reachable by any active local user via pkexec (polkit
//! `auth_self_keep`). It therefore does NOT trust caller-supplied identity or
//! code paths: the uid/gid come from PKEXEC_UID (set by pkexec to the real
//! caller), the continuation is pinned at build time, and forwarded env vars
//! are allowlisted. There is no --user / --continuation argument to abuse.

use std::env;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::ptr;
use std::sync::atomic::{AtomicI32, Ordering};

/// Env vars the launcher may forward into the (unprivileged) continuation.
/// pkexec strips the environment; anything outside this set is dropped, so a
/// direct pkexec caller can't smuggle e.g. LD_PRELOAD/LD_LIBRARY_PATH through.
const ALLOWED_SETENV: &[&str] = &[
    "WAYLAND_DISPLAY",
    "XDG_RUNTIME_DIR",
    "XDG_SESSION_TYPE",
    "DISPLAY",
    "LANG",
];

/// Trusted continuation, pinned at build time (the Makefile passes
/// VERACAGE_CONTINUATION=<prefix>/bin/veracage). Never caller-controlled.
fn continuation() -> &'static str {
    option_env!("VERACAGE_CONTINUATION").unwrap_or("/usr/local/bin/veracage")
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

/// Resolve the validated mountpoint under the *canonical* /run/veracage, so
/// neither `..` nor a symlinked component can redirect the root chown/mount.
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
    vault: String,
    mountpoint: String,
    setenv: Vec<String>,
    rest: Vec<String>,
}

/// Parse argv. Only --vault/--mountpoint/--setenv are accepted before `--`;
/// anything else is an error (notably there is no --user/--continuation).
fn parse_args() -> Args {
    let mut vault: Option<String> = None;
    let mut mountpoint: Option<String> = None;
    let mut setenv: Vec<String> = Vec::new();
    let mut rest: Vec<String> = Vec::new();
    let mut it = env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--vault" => vault = it.next(),
            "--mountpoint" => mountpoint = it.next(),
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
    Args {
        vault: vault.unwrap_or_else(|| fail("--vault required", 2)),
        mountpoint: mountpoint.unwrap_or_else(|| fail("--mountpoint required", 2)),
        setenv,
        rest,
    }
}

/// (uid, gid) of the invoking user, taken from PKEXEC_UID — never from argv.
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

/// 16-hex-char hash matching cleanup.vault_hash (sha256(path)[:16]).
fn vault_hash(p: &Path) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(p.as_os_str().as_bytes());  // raw bytes — matches Python on UTF-8 paths
    h.finalize()
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
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
        fail_errno("chown mountpoint");
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

/// Child path: new mount NS, mount the vault inside it, drop privs, exec.
#[allow(clippy::too_many_arguments)]
fn child(uid: u32, gid: u32, vault: &Path, mountpoint: &Path, dm_name: &str, cont: &Path, args: &Args) -> ! {
    if unsafe { libc::unshare(libc::CLONE_NEWNS) } != 0 {
        fail_errno("unshare(CLONE_NEWNS)");
    }
    let root = CString::new("/").unwrap();
    if unsafe {
        libc::mount(
            ptr::null(),
            root.as_ptr(),
            ptr::null(),
            libc::MS_REC | libc::MS_SLAVE,
            ptr::null(),
        )
    } != 0
    {
        fail_errno("mount --make-rslave /");
    }

    std::fs::create_dir_all(mountpoint)
        .unwrap_or_else(|e| fail(&format!("mkdir mountpoint: {e}"), 1));
    set_mode(mountpoint, 0o700);
    chown(mountpoint, uid, gid);

    let dm_path = format!("/dev/mapper/{dm_name}");

    // Open the VeraCrypt volume. Prompts for the password on the tty.
    let st = Command::new("cryptsetup")
        .args(["--type", "tcrypt", "--veracrypt", "open"])
        .arg(vault)
        .arg(dm_name)
        .status()
        .unwrap_or_else(|e| fail(&format!("spawn cryptsetup: {e}"), 1));
    if !st.success() {
        fail(
            &format!("cryptsetup open failed: {}", st.code().unwrap_or(-1)),
            st.code().unwrap_or(1),
        );
    }

    let st = Command::new("mount")
        .args(["-o", "nodev,nosuid", &dm_path])
        .arg(mountpoint)
        .status()
        .unwrap_or_else(|e| fail(&format!("spawn mount: {e}"), 1));
    if !st.success() {
        let _ = Command::new("cryptsetup").args(["close", dm_name]).status();
        fail(
            &format!("mount failed: {}", st.code().unwrap_or(-1)),
            st.code().unwrap_or(1),
        );
    }

    // Drop privileges to the pkexec caller (gid first, then uid).
    unsafe {
        if libc::setgroups(0, ptr::null()) != 0 {
            fail_errno("setgroups");
        }
        if libc::setresgid(gid, gid, gid) != 0 {
            fail_errno("setresgid");
        }
        if libc::setresuid(uid, uid, uid) != 0 {
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

    // Hand off to the pinned continuation (replaces this process).
    let err = Command::new(cont).args(&args.rest).exec();
    fail(&format!("exec continuation {}: {err}", cont.display()), 127);
}

fn main() {
    let args = parse_args();

    if unsafe { libc::geteuid() } != 0 {
        fail("must be invoked as root (via pkexec)", 2);
    }

    // Identity comes from pkexec, never from argv.
    let (uid, gid) = resolve_caller().unwrap_or_else(|e| fail(&e, 2));

    let vault = std::fs::canonicalize(&args.vault)
        .unwrap_or_else(|_| fail(&format!("vault not found: {}", args.vault), 2));
    if !vault.is_file() {
        fail(&format!("vault is not a file: {}", vault.display()), 2);
    }

    // Create the root-owned mountpoint parent BEFORE validating against it.
    if let Err(e) = std::fs::create_dir_all("/run/veracage") {
        fail(&format!("/run/veracage: {e}"), 1);
    }
    set_mode("/run/veracage", 0o755);

    // Mountpoint must be a direct child of the canonical /run/veracage —
    // resolved here so `..` traversal, the bare dir, or symlinked components
    // can't redirect the root chown/mount that the child performs.
    let mountpoint = validated_mountpoint(&args.mountpoint)
        .unwrap_or_else(|e| fail(&e, 2));

    let cont = PathBuf::from(continuation());
    if !is_executable(&cont) {
        fail(&format!("continuation not executable: {}", cont.display()), 2);
    }

    let dm_name = random_dm_name();

    let lock = format!("/run/veracage/{}.lock", vault_hash(&vault));
    write_lock(&lock, &dm_name, &mountpoint, uid);

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        fail_errno("fork");
    }
    if pid == 0 {
        child(uid, gid, &vault, &mountpoint, &dm_name, &cont, &args);
    }

    // Parent (still root, original mount NS).
    install_signal_forwarding(pid);
    let rc = wait_for(pid);

    let dm_path = format!("/dev/mapper/{dm_name}");
    let mut close_ok = true;
    if Path::new(&dm_path).exists() {
        let st = Command::new("cryptsetup").args(["close", &dm_name]).status();
        close_ok = matches!(st, Ok(s) if s.success());
    }
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
        // C1 regression guard: these must NOT pass.
        assert!(!mountpoint_ok("/run/veracage/../../etc/cron.d"));
        assert!(!mountpoint_ok("/run/veracage/../veracage/x"));
        assert!(!mountpoint_ok("/run/veracage"));
        assert!(!mountpoint_ok("/run/veracage/"));
        assert!(!mountpoint_ok("/run/veracage/a/b"));
        assert!(!mountpoint_ok("/tmp/evil"));
        assert!(!mountpoint_ok("/run/veracageX/x"));
        assert!(!mountpoint_ok("run/veracage/x"));
    }

    // Filled in from `python3 -c 'import hashlib;
    // print(hashlib.sha256(b"/tmp/veracage-x.vc").hexdigest()[:16])'`
    const PY_KNOWN_HASH: &str = "0631c55cceb0614f";
}
