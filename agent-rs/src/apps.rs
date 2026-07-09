//! The `App` type + a `$PATH` lookup. There is no catalog / whitelist any more:
//! the user enables ANY installed binary (see `ui_config`), mirroring `apps.py`.

use std::path::Path;

#[derive(Clone, Debug)]
pub struct App {
    pub key: String,
    pub name: String,
    pub exec: String,
    pub args: Vec<String>,
}

/// True if `binary` is runnable: a bare name found on `$PATH`, or a path that
/// points at an existing executable.
pub fn is_installed(binary: &str) -> bool {
    if binary.contains('/') {
        return is_executable(Path::new(binary));
    }
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|dir| is_executable(&dir.join(binary))))
        .unwrap_or(false)
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}
