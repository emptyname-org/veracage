//! Idmapped-mount primitive: the heart of the UID-isolation core.
//!
//! Presents an already-mounted filesystem at `target` as owned by the vault
//! uid/gid, WITHOUT touching the data. Validated recipe (docs/uid-isolation.md,
//! proven by prototype/spike5): a transient user namespace mapping
//! `inside = on-disk owner`, `outside = vault id`, then
//! `open_tree(OPEN_TREE_CLONE)` -> `mount_setattr(MOUNT_ATTR_IDMAP)` ->
//! `move_mount`. Requires kernel >= 5.12.

use std::ffi::CString;
use std::fs::File;
use std::io::{self, ErrorKind};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::ptr;

// x86_64 syscall numbers (these mount APIs predate glibc wrappers we can rely on).
const SYS_OPEN_TREE: libc::c_long = 428;
const SYS_MOVE_MOUNT: libc::c_long = 429;
const SYS_MOUNT_SETATTR: libc::c_long = 442;

const OPEN_TREE_CLONE: libc::c_long = 1;
const AT_RECURSIVE: libc::c_long = 0x8000;
const MOVE_MOUNT_F_EMPTY_PATH: libc::c_long = 4;
const MOUNT_ATTR_IDMAP: u64 = 0x0010_0000;

/// `nosuid,nodev,noexec` as a `mount_setattr` attr_set, pass as `extra_attr` for
/// the exchange folder (a host-shared dir the sandbox can write, so it must never
/// carry setuid/device/executable semantics). The vault passes `0`.
pub const ATTR_NOSUID_NODEV_NOEXEC: u64 = 0x2 | 0x4 | 0x8;

#[repr(C)]
struct MountAttr {
    attr_set: u64,
    attr_clr: u64,
    propagation: u64,
    userns_fd: u64,
}

fn oserr(ctx: &str) -> io::Error {
    io::Error::new(ErrorKind::Other, format!("{ctx}: {}", io::Error::last_os_error()))
}

/// Create a transient user namespace mapping `inside = on-disk owner`,
/// `outside = vault id` (one uid + one gid). Returns the held `/proc/.../ns/user`
/// file. Keep it alive until after `mount_setattr` consumes it.
fn make_userns(on_disk_uid: u32, vault_uid: u32, on_disk_gid: u32, vault_gid: u32) -> io::Result<File> {
    let mut sig = [0 as libc::c_int; 2];
    let mut hold = [0 as libc::c_int; 2];
    if unsafe { libc::pipe(sig.as_mut_ptr()) } != 0 {
        return Err(oserr("pipe"));
    }
    if unsafe { libc::pipe(hold.as_mut_ptr()) } != 0 {
        return Err(oserr("pipe"));
    }
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(oserr("fork"));
    }
    if pid == 0 {
        // child: unshare a userns, signal the parent, then block alive until
        // the parent has written the maps and opened the ns fd.
        unsafe {
            libc::close(sig[0]);
            libc::close(hold[1]);
            if libc::unshare(libc::CLONE_NEWUSER) != 0 {
                libc::_exit(1);
            }
            libc::write(sig[1], b"1".as_ptr() as *const libc::c_void, 1);
            libc::close(sig[1]);
            let mut b = [0u8; 1];
            libc::read(hold[0], b.as_mut_ptr() as *mut libc::c_void, 1);
            libc::_exit(0);
        }
    }
    unsafe {
        libc::close(sig[1]);
        libc::close(hold[0]);
    }
    let mut b = [0u8; 1];
    let n = unsafe { libc::read(sig[0], b.as_mut_ptr() as *mut libc::c_void, 1) };
    unsafe { libc::close(sig[0]) };
    if n != 1 || b[0] != b'1' {
        unsafe {
            libc::close(hold[1]);
            libc::waitpid(pid, ptr::null_mut(), 0);
        }
        return Err(io::Error::new(ErrorKind::Other, "userns child failed to unshare"));
    }
    let res = (|| -> io::Result<File> {
        std::fs::write(format!("/proc/{pid}/setgroups"), "deny")?;
        std::fs::write(format!("/proc/{pid}/uid_map"), format!("{on_disk_uid} {vault_uid} 1\n"))?;
        std::fs::write(format!("/proc/{pid}/gid_map"), format!("{on_disk_gid} {vault_gid} 1\n"))?;
        File::open(format!("/proc/{pid}/ns/user"))
    })();
    unsafe {
        libc::close(hold[1]); // release the child
        libc::waitpid(pid, ptr::null_mut(), 0);
    }
    res
}

/// Idmap-mount `source_mount` (must already be a mountpoint) at `target`,
/// presenting the on-disk owner as the vault uid/gid. The data is untouched.
pub fn idmap_mount(
    source_mount: &Path,
    target: &Path,
    on_disk_uid: u32,
    on_disk_gid: u32,
    vault_uid: u32,
    vault_gid: u32,
    extra_attr: u64,
) -> io::Result<()> {
    let ns = make_userns(on_disk_uid, vault_uid, on_disk_gid, vault_gid)?;
    let src = CString::new(source_mount.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "source path has NUL"))?;
    let tgt = CString::new(target.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "target path has NUL"))?;
    let empty = CString::new("").unwrap();

    let mnt_fd = unsafe {
        libc::syscall(
            SYS_OPEN_TREE,
            libc::AT_FDCWD as libc::c_long,
            src.as_ptr(),
            OPEN_TREE_CLONE | (libc::O_CLOEXEC as libc::c_long) | AT_RECURSIVE,
        )
    };
    if mnt_fd < 0 {
        return Err(oserr("open_tree"));
    }
    let mnt_fd = mnt_fd as libc::c_int;

    let attr = MountAttr {
        attr_set: MOUNT_ATTR_IDMAP | extra_attr,
        attr_clr: 0,
        propagation: 0,
        userns_fd: ns.as_raw_fd() as u64,
    };
    let rc = unsafe {
        libc::syscall(
            SYS_MOUNT_SETATTR,
            mnt_fd as libc::c_long,
            empty.as_ptr(),
            libc::AT_EMPTY_PATH as libc::c_long,
            &attr as *const MountAttr,
            std::mem::size_of::<MountAttr>() as libc::c_long,
        )
    };
    if rc < 0 {
        let e = oserr("mount_setattr");
        unsafe { libc::close(mnt_fd) };
        return Err(e);
    }

    let rc = unsafe {
        libc::syscall(
            SYS_MOVE_MOUNT,
            mnt_fd as libc::c_long,
            empty.as_ptr(),
            libc::AT_FDCWD as libc::c_long,
            tgt.as_ptr(),
            MOVE_MOUNT_F_EMPTY_PATH,
        )
    };
    unsafe { libc::close(mnt_fd) };
    if rc < 0 {
        return Err(oserr("move_mount"));
    }
    // `ns` drops here (fd closed); the mount keeps its idmapping.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idmap_mount_has_expected_signature() {
        // Compile-time check that the primitive type-checks.
        let _f: fn(&Path, &Path, u32, u32, u32, u32, u64) -> io::Result<()> = idmap_mount;
    }

    /// Real idmapped-mount test (needs root + ext4 idmap support).
    ///   sudo cargo test --manifest-path helper-rs/Cargo.toml -- --ignored
    #[test]
    #[ignore = "needs root; run with sudo and --ignored"]
    fn idmap_mount_isolates() {
        use std::process::Command;
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("skipping idmap_mount_isolates: not root");
            return;
        }
        let on_disk = 1000u32; // typical vault owner
        let vault = 65500u32; // a uid no human has
        let work = std::env::temp_dir().join(format!("vc-idmap-{}", std::process::id()));
        let m0 = work.join("m0");
        let target = work.join("idmapped");
        std::fs::create_dir_all(&m0).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        let chmod = |p: &Path, m: u32| {
            let c = CString::new(p.as_os_str().as_bytes()).unwrap();
            unsafe { libc::chmod(c.as_ptr(), m as libc::mode_t) };
        };
        let chown = |p: &Path, u: u32| {
            let c = CString::new(p.as_os_str().as_bytes()).unwrap();
            unsafe { libc::chown(c.as_ptr(), u, u) };
        };
        chmod(&work, 0o755);
        let img = work.join("ext4.img");
        let sh = |c: &str, a: &[&str]| Command::new(c).args(a).status().unwrap().success();
        assert!(sh("truncate", &["-s", "16M", img.to_str().unwrap()]));
        assert!(sh("mkfs.ext4", &["-q", img.to_str().unwrap()]));
        assert!(sh("mount", &["-o", "loop", img.to_str().unwrap(), m0.to_str().unwrap()]));
        let cleanup = || {
            let _ = Command::new("umount").arg(&target).status();
            let _ = Command::new("umount").arg(&m0).status();
            let _ = std::fs::remove_dir_all(&work);
        };
        let d = m0.join("data");
        std::fs::create_dir(&d).unwrap();
        let sec = d.join("secret.txt");
        std::fs::write(&sec, "SECRET").unwrap();
        chown(&d, on_disk);
        chmod(&d, 0o700);
        chown(&sec, on_disk);
        chmod(&sec, 0o600);

        if let Err(e) = idmap_mount(&m0, &target, on_disk, on_disk, vault, vault, 0) {
            cleanup();
            panic!("idmap_mount failed: {e}");
        }
        let tsec = target.join("data/secret.txt");
        let reads = |uid: u32| {
            Command::new("setpriv")
                .args(["--reuid", &uid.to_string(), "--regid", &uid.to_string(), "--clear-groups", "cat"])
                .arg(&tsec)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        let vault_reads = reads(vault);
        let human_denied = !reads(on_disk);
        cleanup();
        assert!(vault_reads, "vault uid {vault} should read the idmapped vault");
        assert!(human_denied, "on-disk uid {on_disk} must be denied");
    }
}
