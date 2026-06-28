//! Helper-side IPC setup. pkexec closes inherited fds, so the launcher cannot
//! hand the vault-uid leader a host-Wayland fd or a control channel directly.
//! The helper (root, past pkexec) is the only place that can: it connects the
//! host Wayland socket and creates the control listening socket, then execs the
//! leader with those fds inherited (their numbers passed via env). The vault
//! uid can't reach the human's 0700 runtime dir by path, but it holds the fds.

use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
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

fn chown(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path has NUL"))?;
    if unsafe { libc::chown(c.as_ptr(), uid, gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Connect to the host compositor's Wayland socket; return an inheritable
/// connected fd (the leader passes it to weston as WAYLAND_SOCKET). Done as
/// root because the vault uid can't enter the human's 0700 runtime dir.
pub fn connect_host_wayland(path: &Path) -> io::Result<RawFd> {
    let s = UnixStream::connect(path)?;
    let fd = s.into_raw_fd();
    clear_cloexec(fd)?;
    Ok(fd)
}

/// Create the control listening socket at `path` (under the human's runtime
/// dir), owned by the human so they can connect by path. Returns an
/// inheritable listening fd for the leader to accept() on.
pub fn create_control_socket(path: &Path, human_uid: u32, human_gid: u32) -> io::Result<RawFd> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        let _ = chown(dir, human_uid, human_gid);
    }
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    chown(path, human_uid, human_gid)?;
    let fd = listener.into_raw_fd();
    clear_cloexec(fd)?;
    Ok(fd)
}
