//! The `App` type + a `$PATH` lookup. There is no catalog / whitelist any more:
//! the user enables ANY installed binary (see `ui_config`), mirroring `apps.py`.

use std::path::Path;

#[derive(Clone, Debug)]
pub struct App {
    pub key: String,
    pub name: String,
    pub exec: String,
}

/// True if `binary` is runnable: a bare name found on `$PATH`, or a path that
/// points at an existing executable.
pub fn is_installed(binary: &str) -> bool {
    path_of(binary).is_some()
}

/// The full path of an installed binary: a bare name resolved on `$PATH`, or a
/// given path checked for an executable. None when not installed.
pub fn path_of(binary: &str) -> Option<std::path::PathBuf> {
    if binary.contains('/') {
        let p = std::path::PathBuf::from(binary);
        return is_executable(&p).then_some(p);
    }
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(binary))
            .find(|p| is_executable(p))
    })
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}
