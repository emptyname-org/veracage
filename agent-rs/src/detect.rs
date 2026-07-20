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

/// (mime type, category label) probed for a default: order = display order.
/// Labels are lowercase because the UI shows them as a gray "(file manager)"
/// annotation after the app name.
const PROBES: &[(&str, &str)] = &[
    ("inode/directory", "file manager"),
    ("text/plain", "text editor"),
    ("application/pdf", "PDF viewer"),
    ("image/png", "image viewer"),
    ("image/jpeg", "image viewer"),
    ("text/html", "browser"),
];

/// The host's default apps for the probed types, de-duplicated by exec (image/png
/// and image/jpeg usually resolve to the same viewer). Empty if `xdg-mime` is
/// absent or nothing is configured. The caller falls back to a static list.
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
    // A GUI text editor and a terminal are useful against a volume but aren't a
    // mime default (there is no mimetype for "terminal", and text/plain may
    // resolve to something odd), so fill them from a small installed-app probe.
    if !out.iter().any(|s| s.category == "text editor") {
        if let Some(s) = first_installed(EDITORS, "text editor") {
            out.push(s);
        }
    }
    if let Some(term) = first_installed(TERMINALS, "terminal").map(resolve_terminal) {
        let base = crate::config::exec_basename(&term.exec);
        if !out.iter().any(|e| crate::config::exec_basename(&e.exec) == base) {
            out.push(term);
        }
    }
    out
}

/// Debian's `x-terminal-emulator` is an update-alternatives symlink. Resolve it
/// to the real terminal so the suggestion names the actual app and de-dupes
/// against an already-enabled copy of it (an enabled "konsole" and a suggested
/// "x-terminal-emulator" are the same program twice).
fn resolve_terminal(s: Suggestion) -> Suggestion {
    if s.exec != "x-terminal-emulator" {
        return s;
    }
    let Some(target) = crate::apps::path_of(&s.exec)
        .and_then(|p| std::fs::canonicalize(p).ok())
    else {
        return s;
    };
    let Some(base) = target.file_name().and_then(|n| n.to_str()) else {
        return s;
    };
    let name = TERMINALS
        .iter()
        .find(|(_, e)| *e == base)
        .map(|(n, _)| (*n).to_string())
        .unwrap_or_else(|| crate::config::capitalize_first(base));
    Suggestion { name, exec: base.to_string(), category: s.category }
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
    // A leading env/wrapper token isn't handled. The common case is a bare bin.
    let base = first.rsplit('/').next().unwrap_or(first);
    if base.starts_with('%') { String::new() } else { base.to_string() }
}

// ------------------------------------------------------------ icons --------

/// The host icon for `exec`, as raw RGBA no larger than 64x64: find a `.desktop`
/// whose `Exec=` launches the same binary, take its `Icon=`, resolve that to a
/// PNG (hicolor sizes, then pixmaps), decode, and downscale. `None` when any
/// step has no answer. The menu then simply shows text without an icon.
pub fn icon_rgba_for_exec(exec: &str) -> Option<(u32, u32, Vec<u8>)> {
    let icon_name = desktop_icon_for_exec(exec)?;
    icon_rgba_for_name(&icon_name)
}

/// A host icon by freedesktop NAME, as raw RGBA no larger than 64x64. `None`
/// if the icon isn't in the theme.
fn icon_rgba_for_name(name: &str) -> Option<(u32, u32, Vec<u8>)> {
    let png = read_icon_png(name)?;
    let data = eframe::icon_data::from_png_bytes(&png).ok()?;
    Some(downscale_max(data.width, data.height, data.rgba, 64))
}


/// Scan the XDG application dirs (user first) for a `.desktop` whose `Exec=`
/// binary matches `exec`'s basename, and return its `Icon=` value.
fn desktop_icon_for_exec(exec: &str) -> Option<String> {
    let want = std::path::Path::new(exec)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(exec)
        .to_string();
    for dir in application_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            let Ok(body) = std::fs::read_to_string(&path) else { continue };
            let mut in_entry = false;
            let mut exec_bin = String::new();
            let mut icon = String::new();
            for line in body.lines() {
                let line = line.trim();
                if line.starts_with('[') {
                    in_entry = line == "[Desktop Entry]";
                    continue;
                }
                if !in_entry {
                    continue;
                }
                if let Some(v) = line.strip_prefix("Exec=") {
                    if exec_bin.is_empty() {
                        exec_bin = exec_binary(v.trim());
                    }
                } else if let Some(v) = line.strip_prefix("Icon=") {
                    if icon.is_empty() {
                        icon = v.trim().to_string();
                    }
                }
            }
            if !icon.is_empty() && exec_bin == want {
                return Some(icon);
            }
        }
    }
    None
}

/// Resolve an `Icon=` value to PNG bytes: an absolute `.png` path is read as
/// is; a bare name is searched in the hicolor theme's app sizes and pixmaps,
/// and on a miss in a bounded recursive scan of every installed icon theme
/// (KDE's breeze is SVG-only, but gnome/oxygen ship PNGs for the same names).
/// Size-capped so a huge file can't balloon the broker.
fn read_icon_png(icon: &str) -> Option<Vec<u8>> {
    const MAX_PNG: u64 = 1024 * 1024;
    let read_capped = |p: &std::path::Path| -> Option<Vec<u8>> {
        let md = std::fs::metadata(p).ok()?;
        (md.is_file() && md.len() <= MAX_PNG).then(|| std::fs::read(p).ok())?
    };
    if icon.starts_with('/') {
        return icon.ends_with(".png").then(|| read_capped(std::path::Path::new(icon)))?;
    }
    let mut roots = Vec::new();
    let home = std::env::var("HOME").unwrap_or_default();
    let data_home =
        std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| format!("{home}/.local/share"));
    roots.push(format!("{data_home}/icons"));
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    for d in data_dirs.split(':').filter(|s| !s.is_empty()) {
        roots.push(format!("{d}/icons"));
    }
    // Fast path: hicolor (where apps install their own icons), then pixmaps.
    for size in [48, 64, 32, 96, 128, 256] {
        for root in &roots {
            let p = std::path::PathBuf::from(root)
                .join("hicolor")
                .join(format!("{size}x{size}"))
                .join("apps")
                .join(format!("{icon}.png"));
            if let Some(bytes) = read_capped(&p) {
                return Some(bytes);
            }
        }
    }
    if let Some(bytes) = read_capped(std::path::Path::new(&format!("/usr/share/pixmaps/{icon}.png"))) {
        return Some(bytes);
    }
    // Slow path (runs once per app, at publish time): scan the icon roots for
    // `<icon>.png` in ANY theme/category and pick the size closest to 48.
    let want = format!("{icon}.png");
    let mut best: Option<(u32, std::path::PathBuf)> = None;
    let mut budget: u32 = 80_000; // dir-entry cap: bounded even on a huge theme set
    for root in &roots {
        scan_for_icon(std::path::Path::new(root), &want, 0, &mut budget, &mut best);
    }
    read_capped(&best?.1)
}

/// Recursive `<name>.png` search under `dir` (depth- and entry-capped). Ranks
/// candidates by the size hint in the path ("48x48" or "/48/"), preferring the
/// one closest to 48px; `best` holds (distance, path).
fn scan_for_icon(
    dir: &std::path::Path,
    want: &str,
    depth: u32,
    budget: &mut u32,
    best: &mut Option<(u32, std::path::PathBuf)>,
) {
    if depth > 6 || *budget == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        if *budget == 0 {
            return;
        }
        *budget -= 1;
        let path = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            scan_for_icon(&path, want, depth + 1, budget, best);
        } else if ft.is_file() && entry.file_name().to_str() == Some(want) {
            let size = path
                .to_str()
                .and_then(|s| {
                    s.split('/').find_map(|c| {
                        let n = c.split('x').next()?;
                        n.parse::<u32>().ok().filter(|&v| (8..=512).contains(&v))
                    })
                })
                .unwrap_or(0);
            let dist = size.abs_diff(48);
            if best.as_ref().is_none_or(|(d, _)| dist < *d) {
                *best = Some((dist, path));
            }
        }
    }
}

/// Nearest-neighbor downscale of an RGBA image so neither side exceeds `max`.
/// Integer stride sampling, plenty for a 20pt menu icon.
fn downscale_max(w: u32, h: u32, rgba: Vec<u8>, max: u32) -> (u32, u32, Vec<u8>) {
    if w <= max && h <= max || w == 0 || h == 0 {
        return (w, h, rgba);
    }
    let step = (w.max(h)).div_ceil(max) as usize;
    let (w, h) = (w as usize, h as usize);
    let (nw, nh) = ((w / step).max(1), (h / step).max(1));
    let mut out = Vec::with_capacity(nw * nh * 4);
    for y in 0..nh {
        for x in 0..nw {
            let src = ((y * step) * w + (x * step)) * 4;
            out.extend_from_slice(&rgba[src..src + 4]);
        }
    }
    (nw as u32, nh as u32, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downscale_caps_the_long_side() {
        let (w, h, px) = downscale_max(128, 128, vec![7u8; 128 * 128 * 4], 64);
        assert_eq!((w, h), (64, 64));
        assert_eq!(px.len(), 64 * 64 * 4);
        // Small images pass through untouched.
        let (w, h, px) = downscale_max(32, 32, vec![9u8; 32 * 32 * 4], 64);
        assert_eq!((w, h, px.len()), (32, 32, 32 * 32 * 4));
    }

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
