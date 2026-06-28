//! Idmapped-mount primitive — the heart of the UID-isolation core.
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
/// file — keep it alive until after `mount_setattr` consumes it.
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
#[allow(dead_code)] // wired into the helper flow in the next increment
pub fn idmap_mount(
    source_mount: &Path,
    target: &Path,
    on_disk_uid: u32,
    on_disk_gid: u32,
    vault_uid: u32,
    vault_gid: u32,
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
        attr_set: MOUNT_ATTR_IDMAP,
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
        // Compile-time check that the primitive type-checks; the real behaviour
        // is exercised under root by the helper integration test.
        let _f: fn(&Path, &Path, u32, u32, u32, u32) -> io::Result<()> = idmap_mount;
    }
}
