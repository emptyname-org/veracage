//! Helper-side IPC setup. pkexec closes inherited fds, so the launcher cannot
//! hand the vault-uid leader a host-Wayland fd or a control channel directly.
//! The helper (root, past pkexec) is the only place that can: it connects the
//! host Wayland socket and creates the control listening socket, then execs the
//! leader with those fds inherited (their numbers passed via env). The vault
//! uid can't reach the human's 0700 runtime dir by path, but it holds the fds.

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::{IntoRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

fn clear_cloexec(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn fchmod(fd: RawFd, mode: u32) -> io::Result<()> {
    if unsafe { libc::fchmod(fd, mode as libc::mode_t) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn fchown(fd: RawFd, uid: u32, gid: u32) -> io::Result<()> {
    if unsafe { libc::fchown(fd, uid, gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Open `dir` (an absolute path), creating any missing component as root, and
/// return an `O_NOFOLLOW` directory fd for the FINAL component. Each component is
/// created with `mkdirat` and re-opened with `openat(O_DIRECTORY|O_NOFOLLOW)`
/// **relative to the previous fd**, never by full path. So a caller who owns an
/// ancestor (their own `/run/user/<uid>`) cannot swap a component for a symlink
/// between check and use to redirect the `fchmod`/`fchown`/`bind` that follow: a
/// component that already exists as a symlink makes `openat` fail (ELOOP) and we
/// refuse. Intermediate components are left root-owned `0711` (traverse-only).
fn open_dir_created(dir: &Path) -> io::Result<RawFd> {
    let root = CString::new("/").unwrap();
    let mut cur = unsafe {
        libc::open(root.as_ptr(), libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
    };
    if cur < 0 {
        return Err(io::Error::last_os_error());
    }
    for comp in dir.components() {
        let name = match comp {
            std::path::Component::RootDir => continue,
            std::path::Component::Normal(n) => n,
            _ => {
                unsafe { libc::close(cur) };
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "control-dir path must be absolute with normal components",
                ));
            }
        };
        let cname = match CString::new(name.as_bytes()) {
            Ok(c) => c,
            Err(_) => {
                unsafe { libc::close(cur) };
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "path component has NUL"));
            }
        };
        let mr = unsafe { libc::mkdirat(cur, cname.as_ptr(), 0o711) };
        if mr != 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EEXIST) {
                unsafe { libc::close(cur) };
                return Err(e);
            }
        }
        // O_NOFOLLOW: if this component was swapped for a symlink, fail rather
        // than resolve through it.
        let next = unsafe {
            libc::openat(cur, cname.as_ptr(), libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        };
        unsafe { libc::close(cur) };
        if next < 0 {
            return Err(io::Error::last_os_error());
        }
        cur = next;
    }
    Ok(cur)
}

/// Connect to the host compositor's Wayland socket; return an inheritable
/// connected fd (the leader passes it to the compositor as WAYLAND_SOCKET). Done
/// as root because the vault uid can't enter the human's 0700 runtime dir.
pub fn connect_host_wayland(path: &Path) -> io::Result<RawFd> {
    let s = UnixStream::connect(path)?;
    let fd = s.into_raw_fd();
    clear_cloexec(fd)?;
    Ok(fd)
}

/// Create the control listening socket at `path` (under the human's runtime
/// dir), owned by the human so they can connect by path. Returns an inheritable
/// listening fd for the leader to accept() on.
///
/// Every privileged step operates on an `O_NOFOLLOW` directory fd, and the
/// directory stays root-owned while the socket is created and locked down (the
/// human can't tamper mid-setup); ownership is handed to the human LAST. So a
/// caller can't win a TOCTOU race to redirect a root chmod/chown/bind onto an
/// arbitrary path.
pub fn create_control_socket(path: &Path, human_uid: u32, human_gid: u32) -> io::Result<RawFd> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "control path has no parent"))?;
    let sockname = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "control path has no filename"))?;
    let sockname = CString::new(sockname.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "socket name has NUL"))?;

    // Race-free fd to the real control dir. Keep it root-owned 0700 while we bind
    // + lock the socket, so the human can't swap the socket for a symlink and
    // redirect the chmod below. Chown to the human LAST.
    let dfd = open_dir_created(dir)?;
    let finish = |dfd: RawFd, r: io::Result<RawFd>| -> io::Result<RawFd> {
        unsafe { libc::close(dfd) };
        r
    };
    if let Err(e) = fchmod(dfd, 0o700) {
        return finish(dfd, Err(e));
    }

    // Bind the socket INSIDE that exact directory. There is no bindat(2), so save
    // CWD, fchdir into the dir fd, bind a RELATIVE name (resolved against the
    // O_NOFOLLOW'd real inode → not redirectable), then restore CWD.
    let saved_cwd =
        unsafe { libc::open(b".\0".as_ptr() as *const libc::c_char, libc::O_DIRECTORY | libc::O_CLOEXEC) };
    if saved_cwd < 0 {
        return finish(dfd, Err(io::Error::last_os_error()));
    }
    unsafe { libc::unlinkat(dfd, sockname.as_ptr(), 0) }; // drop any stale socket/symlink
    let bound: io::Result<UnixListener> = (|| {
        if unsafe { libc::fchdir(dfd) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let rel = Path::new(std::ffi::OsStr::from_bytes(sockname.to_bytes()));
        let l = UnixListener::bind(rel);
        let _ = unsafe { libc::fchdir(saved_cwd) }; // restore CWD regardless
        l
    })();
    unsafe { libc::close(saved_cwd) };
    let listener = match bound {
        Ok(l) => l,
        Err(e) => return finish(dfd, Err(e)),
    };

    // Lock the socket down, relative to the dir fd (no path re-resolution). The
    // dir is still root-owned, so the socket name can't have been swapped for a
    // symlink → fchmodat's symlink-follow is not exploitable here.
    if unsafe { libc::fchmodat(dfd, sockname.as_ptr(), 0o600, 0) } != 0 {
        return finish(dfd, Err(io::Error::last_os_error()));
    }
    if unsafe {
        libc::fchownat(dfd, sockname.as_ptr(), human_uid, human_gid, libc::AT_SYMLINK_NOFOLLOW)
    } != 0
    {
        return finish(dfd, Err(io::Error::last_os_error()));
    }

    // Finally hand the directory to the human (0700) so they can traverse + list
    // it to connect. Any tampering after this is same-uid-on-own-files, not a
    // root-privilege TOCTOU.
    if let Err(e) = fchown(dfd, human_uid, human_gid) {
        return finish(dfd, Err(e));
    }
    unsafe { libc::close(dfd) };

    let fd = listener.into_raw_fd();
    clear_cloexec(fd)?;
    Ok(fd)
}
