//! Sense the host's DEFAULT apps (file manager, text editor, PDF/image viewer)
//! rather than hardcoding a catalog. Uses `xdg-mime query default <mime>` to get
//! the user's chosen `.desktop` per type, then parses its `Name`/`Exec`. The agent
//! runs as the human on the host, so these are the real per-user defaults.

use std::path::PathBuf;

pub struct Suggestion {
    pub name: String,
    pub exec: String,
    pub category: &'static str,
}

/// (mime type, category label) probed for a default — order = display order.
const PROBES: &[(&str, &str)] = &[
    ("inode/directory", "File manager"),
    ("text/plain", "Text editor"),
    ("application/pdf", "PDF viewer"),
    ("image/png", "Image viewer"),
    ("image/jpeg", "Image viewer"),
    ("text/html", "Browser"),
];

/// The host's default apps for the probed types, de-duplicated by exec (image/png
/// and image/jpeg usually resolve to the same viewer). Empty if `xdg-mime` is
/// absent or nothing is configured — the caller falls back to a static list.
pub fn detected_defaults() -> Vec<Suggestion> {
    let mut out: Vec<Suggestion> = Vec::new();
    for (mime, cat) in PROBES {
        let Some(desktop_id) = default_desktop_id(mime) else { continue };
        let Some((name, exec)) = parse_desktop(&desktop_id) else { continue };
        if !crate::apps::is_installed(&exec) {
            continue;
        }
        if out.iter().any(|s| s.exec == exec) {
            continue; // same app for two mimes (e.g. png+jpeg)
        }
        out.push(Suggestion { name, exec, category: cat });
    }
    // A GUI text editor and a terminal are useful against a vault but aren't a
    // mime default (there is no mimetype for "terminal", and text/plain may
    // resolve to something odd), so fill them from a small installed-app probe.
    if !out.iter().any(|s| s.category == "Text editor") {
        if let Some(s) = first_installed(EDITORS, "Text editor") {
            out.push(s);
        }
    }
    if let Some(term) = first_installed(TERMINALS, "Terminal") {
        if !out.iter().any(|e| e.exec == term.exec) {
            out.push(term);
        }
    }
    out
}

/// Common GUI text editors, best-known first (fallback when `text/plain` didn't
/// resolve to one).
const EDITORS: &[(&str, &str)] = &[
    ("Kate", "kate"),
    ("Text Editor", "gnome-text-editor"),
    ("gedit", "gedit"),
    ("Mousepad", "mousepad"),
    ("KWrite", "kwrite"),
];

/// Terminal emulators. `x-terminal-emulator` is Debian's update-alternatives
/// default (a symlink to the user's chosen terminal), so it comes first.
const TERMINALS: &[(&str, &str)] = &[
    ("Terminal", "x-terminal-emulator"),
    ("Konsole", "konsole"),
    ("GNOME Terminal", "gnome-terminal"),
    ("Alacritty", "alacritty"),
    ("kitty", "kitty"),
    ("xterm", "xterm"),
];

/// First installed (name, exec) from `list`, as a Suggestion in `category`.
fn first_installed(list: &[(&str, &str)], category: &'static str) -> Option<Suggestion> {
    list.iter().find(|(_, exec)| crate::apps::is_installed(exec)).map(|(name, exec)| {
        Suggestion { name: name.to_string(), exec: exec.to_string(), category }
    })
}

/// `xdg-mime query default <mime>` -> a `.desktop` id (e.g. `org.kde.dolphin.desktop`).
fn default_desktop_id(mime: &str) -> Option<String> {
    let out = std::process::Command::new("xdg-mime")
        .args(["query", "default", mime])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    // A desktop id, nothing path-y: it names a file we look up in known dirs.
    if id.is_empty() || id.contains('/') || !id.ends_with(".desktop") {
        return None;
    }
    Some(id)
}

/// The XDG application directories, most-specific first (user overrides system).
fn application_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let home = std::env::var("HOME").unwrap_or_default();
    let data_home = std::env::var("XDG_DATA_HOME")
        .unwrap_or_else(|_| format!("{home}/.local/share"));
    dirs.push(PathBuf::from(data_home).join("applications"));
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    for d in data_dirs.split(':').filter(|s| !s.is_empty()) {
        dirs.push(PathBuf::from(d).join("applications"));
    }
    dirs
}

/// Find `<id>` in the application dirs and parse its `Name`/`Exec` (first exec
/// token, `%`-field-codes stripped). Handles the `foo-subdir.desktop` layout where
/// the id maps to `foo/subdir.desktop`.
fn parse_desktop(id: &str) -> Option<(String, String)> {
    for dir in application_dirs() {
        for candidate in [dir.join(id), dir.join(id.replacen('-', "/", 1))] {
            if let Ok(body) = std::fs::read_to_string(&candidate) {
                return parse_desktop_body(&body);
            }
        }
    }
    None
}

/// Parse the `[Desktop Entry]` group: `Name=` and the binary from `Exec=` (drop
/// `%U`/`%f`/… field codes, take argv[0]'s basename). Pure, so it is unit-tested.
fn parse_desktop_body(body: &str) -> Option<(String, String)> {
    let mut in_entry = false;
    let mut name = String::new();
    let mut exec = String::new();
    for line in body.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        if let Some(v) = line.strip_prefix("Name=") {
            if name.is_empty() {
                name = v.trim().to_string();
            }
        } else if let Some(v) = line.strip_prefix("Exec=") {
            if exec.is_empty() {
                exec = exec_binary(v.trim());
            }
        }
    }
    if exec.is_empty() {
        return None;
    }
    if name.is_empty() {
        name = exec.clone();
    }
    Some((name, exec))
}

/// The launchable binary from an `Exec=` line: first whitespace token, `%`-codes
/// dropped, path reduced to its basename (`/usr/bin/dolphin` -> `dolphin`).
fn exec_binary(exec_line: &str) -> String {
    let first = exec_line.split_whitespace().next().unwrap_or("");
    // A leading env/wrapper token isn't handled — the common case is a bare bin.
    let base = first.rsplit('/').next().unwrap_or(first);
    if base.starts_with('%') { String::new() } else { base.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_name_and_exec_stripping_field_codes() {
        let body = "[Desktop Entry]\nName=Dolphin\nExec=/usr/bin/dolphin %u\nType=Application\n";
        assert_eq!(parse_desktop_body(body), Some(("Dolphin".into(), "dolphin".into())));
    }

    #[test]
    fn ignores_other_groups_and_actions() {
        let body = "[Desktop Entry]\nExec=okular %U\nName=Okular\n\
                    [Desktop Action new]\nExec=okular --new\nName=New\n";
        assert_eq!(parse_desktop_body(body), Some(("Okular".into(), "okular".into())));
    }

    #[test]
    fn exec_binary_strips_path_and_args() {
        assert_eq!(exec_binary("/usr/bin/kate -b %F"), "kate");
        assert_eq!(exec_binary("nautilus --new-window %U"), "nautilus");
        assert_eq!(exec_binary("%U"), "");
    }

    #[test]
    fn no_exec_is_none() {
        assert_eq!(parse_desktop_body("[Desktop Entry]\nName=X\n"), None);
    }
}
