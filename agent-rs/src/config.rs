//! Read/write `~/.config/veracage/config.toml`. Round-trips *every* section so
//! the picker can never drop a user's `[default]`/`[volumes]` settings, and the
//! format stays interchangeable with `config.py` (Python's `tomllib` reads what
//! we write, and we read what it writes). Tolerant of a malformed file (a
//! same-uid process can write it), degrades to an empty config, like Python.

use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::apps::App;

/// The basename of an exec (bare name or path), for duplicate detection:
/// `/usr/bin/dolphin` and `dolphin` are the same app.
pub fn exec_basename(exec: &str) -> String {
    std::path::Path::new(exec)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(exec)
        .to_string()
}

pub fn config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        });
    base.join("veracage").join("config.toml")
}

// --- on-disk shape (serde) --------------------------------------------------

#[derive(Default, Serialize, Deserialize)]
struct Raw {
    #[serde(default)]
    default: DefaultSection,
    // IndexMap (not BTreeMap): preserve the on-disk / insertion order of apps so a
    // GUI save doesn't reorder the toolbar launchers alphabetically, and doesn't
    // change which file manager auto-opens (the first one in order). Matches
    // config.py, which uses an insertion-ordered dict.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    apps: IndexMap<String, AppEntry>,
    // Veracage keyboard shortcuts (compositor-level clipboard transfers).
    #[serde(default)]
    shortcuts: BTreeMap<String, String>,
    // Preserved verbatim across a save: the picker never touches per-volume
    // overrides.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    volumes: BTreeMap<String, toml::Value>,
}

/// The configurable shortcut actions and their defaults.
pub const SHORTCUT_DEFAULTS: &[(&str, &str)] =
    &[("copy_out", "Ctrl+Alt+C"), ("paste_in", "Ctrl+Alt+V")];

fn default_shortcuts() -> BTreeMap<String, String> {
    SHORTCUT_DEFAULTS
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// The default bind for a shortcut action ("" if unknown).
pub fn default_shortcut(action: &str) -> String {
    SHORTCUT_DEFAULTS
        .iter()
        .find(|(k, _)| *k == action)
        .map(|(_, v)| v.to_string())
        .unwrap_or_default()
}

#[derive(Serialize, Deserialize)]
struct DefaultSection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_used_app: Option<String>,
    #[serde(default = "default_suspend")]
    suspend_action: String,
    #[serde(default = "default_theme")]
    theme: String,
    #[serde(default = "default_font")]
    ui_font: String,
    #[serde(default = "default_font_size")]
    ui_font_size: String,
    #[serde(default = "default_window_size")]
    window_size: String,
    #[serde(default = "default_modifier_keys")]
    modifier_keys: String,
    #[serde(default = "default_true")]
    exchange: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    exchange_dir: Option<String>,
    #[serde(default = "default_true")]
    clip_clear: bool,
    #[serde(default = "default_clip_clear_timeout")]
    clip_clear_timeout: u32,
    /// Idle minutes before the session dismounts itself (0 = off).
    #[serde(default)]
    auto_dismount: u32,
    /// Verbose timing logs across the components (see docs/debugging.md).
    #[serde(default)]
    debug: bool,
}

impl Default for DefaultSection {
    fn default() -> Self {
        DefaultSection {
            last_used_app: None,
            suspend_action: default_suspend(),
            theme: default_theme(),
            ui_font: default_font(),
            ui_font_size: default_font_size(),
            window_size: default_window_size(),
            modifier_keys: default_modifier_keys(),
            exchange: true,
            exchange_dir: None,
            clip_clear: true,
            clip_clear_timeout: default_clip_clear_timeout(),
            auto_dismount: 0,
            debug: false,
        }
    }
}

fn default_suspend() -> String {
    "dismount".into()
}

/// Default modifier mapping: whatever the host desktop is configured with.
fn default_modifier_keys() -> String {
    "system".into()
}

/// Longest idle timeout before an automatic dismount, in minutes (a day).
pub const AUTO_DISMOUNT_MAX: u32 = 1440;

/// Default host-clipboard auto-clear timeout, in seconds (KeePassXC-style).
pub fn default_clip_clear_timeout() -> u32 {
    30
}

/// Bounds for the auto-clear timeout. A hand-edited config outside this range is
/// clamped in: the timer stays sane and the UI presets all fall inside it.
pub const CLIP_CLEAR_TIMEOUT_MIN: u32 = 1;
pub const CLIP_CLEAR_TIMEOUT_MAX: u32 = 3600;

/// Clamp a hand-edited auto-clear timeout (seconds) into range.
pub fn clamp_clip_timeout(secs: u32) -> u32 {
    secs.clamp(CLIP_CLEAR_TIMEOUT_MIN, CLIP_CLEAR_TIMEOUT_MAX)
}

fn default_theme() -> String {
    "light".into()
}

fn default_font() -> String {
    "system".into()
}

fn default_font_size() -> String {
    "system".into()
}

fn default_window_size() -> String {
    "default".into()
}

fn default_true() -> bool {
    true
}

#[derive(Serialize, Deserialize)]
struct AppEntry {
    name: String,
    exec: String,
    // Unknown keys in an existing config (`args`, `category`, `note`) are
    // ignored: serde drops them, and they are never written back. Apps launch
    // bare and start in the workspace directory.
}

// --- public config ----------------------------------------------------------

pub struct Config {
    pub apps: Vec<App>,
    pub last_used_app: Option<String>,
    pub suspend_action: String,
    pub theme: String,             // "light" | "dark" (| "system", future)
    pub ui_font: String,           // fonts::CHOICES key ("system" default = host)
    pub ui_font_size: String,      // "system" (host size) | a point size
    pub window_size: String,       // "default" | "max" | "<w>x<h>"
    pub modifier_keys: String,     // keyboard::CHOICES key ("system" = host)
    pub exchange: bool,            // host<->volume shared directory on/off
    pub exchange_dir: Option<String>,
    pub clip_clear: bool,          // auto-clear host clipboard after Copy out
    pub clip_clear_timeout: u32,   // seconds before the auto-clear fires
    pub auto_dismount: u32,        // idle minutes before a dismount (0 = off)
    pub debug: bool,               // verbose timing logs (docs/debugging.md)
    pub shortcuts: BTreeMap<String, String>, // action -> keybind (copy_out/paste_in)
    volumes: BTreeMap<String, toml::Value>, // opaque pass-through
}

impl Config {
    /// The host exchange directory (mirrors config.py `Config.exchange_path`):
    /// the configured `exchange_dir` (with a leading `~` expanded), else
    /// `~/Veracage/Exchange`. This is the SAME dir cli.py mounts into the sandbox
    /// at /exchange, so the broker's Import/Export must open exactly this.
    pub fn exchange_path(&self) -> PathBuf {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        match self.exchange_dir.as_deref() {
            Some(d) if d == "~" => home.unwrap_or_else(|| PathBuf::from("~")),
            Some(d) => {
                if let Some(rest) = d.strip_prefix("~/") {
                    if let Some(h) = home {
                        return h.join(rest);
                    }
                }
                PathBuf::from(d)
            }
            None => home.unwrap_or_default().join("Veracage").join("Exchange"),
        }
    }

    pub fn empty() -> Config {
        Config {
            apps: Vec::new(),
            last_used_app: None,
            suspend_action: "dismount".into(),
            theme: "light".into(),
            ui_font: default_font(),
            ui_font_size: default_font_size(),
            window_size: default_window_size(),
            modifier_keys: default_modifier_keys(),
            exchange: true,
            exchange_dir: None,
            clip_clear: true,
            clip_clear_timeout: default_clip_clear_timeout(),
            auto_dismount: 0,
            debug: false,
            shortcuts: default_shortcuts(),
            volumes: BTreeMap::new(),
        }
    }
}

/// Capitalize the first character (Unicode-aware) so a bare basename name like
/// "kate" is shown as "Kate". Leaves an already-capitalized or non-letter start
/// untouched.
pub fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

pub fn load() -> Config {
    let p = config_path();
    let text = match std::fs::read_to_string(&p) {
        Ok(t) => t,
        Err(_) => return Config::empty(),
    };
    let raw: Raw = match toml::from_str(&text) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("veracage: cannot parse {}: {e}; using empty config", p.display());
            return Config::empty();
        }
    };
    // Dedupe by exec basename (first entry wins): older configs could hold the
    // same app twice (e.g. `dolphin` and `/usr/bin/dolphin`). Names are shown
    // capitalized (kate -> Kate).
    let mut seen = std::collections::HashSet::new();
    let apps = raw
        .apps
        .into_iter()
        .filter(|(_, e)| seen.insert(exec_basename(&e.exec)))
        .map(|(key, e)| App {
            key,
            name: capitalize_first(&e.name),
            exec: e.exec,
        })
        .collect();
    let suspend_action = match raw.default.suspend_action.as_str() {
        "dismount" | "ignore" => raw.default.suspend_action,
        other => {
            eprintln!("veracage: invalid suspend_action {other:?}; using 'dismount'");
            "dismount".into()
        }
    };
    let theme = match raw.default.theme.as_str() {
        "light" | "dark" | "system" => raw.default.theme,
        other => {
            eprintln!("veracage: invalid theme {other:?}; using 'light'");
            "light".into()
        }
    };
    // The value is published to the compositor and handed to libxkbcommon, so
    // only the offered choices are accepted.
    let modifier_keys = if crate::keyboard::is_valid(&raw.default.modifier_keys) {
        raw.default.modifier_keys
    } else {
        eprintln!(
            "veracage: invalid modifier_keys {:?}; using 'system'",
            raw.default.modifier_keys
        );
        default_modifier_keys()
    };
    let ui_font = if crate::fonts::is_valid_font(&raw.default.ui_font) {
        raw.default.ui_font
    } else {
        eprintln!("veracage: invalid ui_font {:?}; using 'system'", raw.default.ui_font);
        default_font()
    };
    let ui_font_size = if crate::fonts::is_valid_size(&raw.default.ui_font_size) {
        raw.default.ui_font_size
    } else {
        eprintln!(
            "veracage: invalid ui_font_size {:?}; using 'system'",
            raw.default.ui_font_size
        );
        default_font_size()
    };
    let window_size = if window_size_valid(&raw.default.window_size) {
        raw.default.window_size
    } else {
        eprintln!(
            "veracage: invalid window_size {:?}; using 'default'",
            raw.default.window_size
        );
        default_window_size()
    };
    // Clamp a hand-edited timeout into range so a bad value can't wedge the
    // clear timer (0 = clear instantly, absurdly large = never).
    let clip_clear_timeout = clamp_clip_timeout(raw.default.clip_clear_timeout);
    // Fill any missing shortcut actions with their defaults so the configurator
    // always has every row.
    let mut shortcuts = default_shortcuts();
    for (k, v) in raw.shortcuts {
        if !v.trim().is_empty() {
            shortcuts.insert(k, v);
        }
    }
    Config {
        apps,
        last_used_app: raw.default.last_used_app,
        suspend_action,
        theme,
        ui_font,
        ui_font_size,
        window_size,
        modifier_keys,
        exchange: raw.default.exchange,
        exchange_dir: raw.default.exchange_dir,
        clip_clear: raw.default.clip_clear,
        clip_clear_timeout,
        // Bounded so a hand-edited config can't keep a volume open forever.
        auto_dismount: raw.default.auto_dismount.min(AUTO_DISMOUNT_MAX),
        debug: raw.default.debug,
        shortcuts,
        volumes: raw.volumes,
    }
}

/// Accept "default", "max", or "<w>x<h>" with sane bounds. Keeps a hand-edited
/// config from wedging the compositor with an absurd size.
pub fn window_size_valid(s: &str) -> bool {
    if s == "default" || s == "max" {
        return true;
    }
    match s.split_once('x') {
        Some((w, h)) => matches!(
            (w.parse::<u32>(), h.parse::<u32>()),
            (Ok(w), Ok(h)) if (320..=16384).contains(&w) && (240..=16384).contains(&h)
        ),
        None => false,
    }
}

pub fn save(cfg: &Config) -> io::Result<PathBuf> {
    let p = config_path();
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let apps = cfg
        .apps
        .iter()
        .map(|a| {
            (
                a.key.clone(),
                AppEntry {
                    name: a.name.clone(),
                    exec: a.exec.clone(),
                },
            )
        })
        .collect();
    let raw = Raw {
        default: DefaultSection {
            last_used_app: cfg.last_used_app.clone(),
            suspend_action: cfg.suspend_action.clone(),
            theme: cfg.theme.clone(),
            ui_font: cfg.ui_font.clone(),
            ui_font_size: cfg.ui_font_size.clone(),
            window_size: cfg.window_size.clone(),
            modifier_keys: cfg.modifier_keys.clone(),
            exchange: cfg.exchange,
            exchange_dir: cfg.exchange_dir.clone(),
            clip_clear: cfg.clip_clear,
            clip_clear_timeout: cfg.clip_clear_timeout,
            auto_dismount: cfg.auto_dismount,
            debug: cfg.debug,
        },
        apps,
        shortcuts: cfg.shortcuts.clone(),
        volumes: cfg.volumes.clone(),
    };
    let body = toml::to_string(&raw).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let text = format!(
        "# Veracage config - edit carefully, see `veracage configure`\n\n{body}"
    );
    // Atomic write (temp + rename) so a concurrent reader never sees a truncated
    // config (which load() would treat as empty).
    // Pid-tagged temp name so a concurrent save from another process (Settings
    // window vs. the configure picker) can't clobber our tmp mid-write.
    let tmp = p.with_extension(format!("toml.{}.tmp", std::process::id()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &p)?;
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str) -> AppEntry {
        AppEntry { name: name.into(), exec: name.to_lowercase() }
    }

    #[test]
    fn apps_keep_insertion_order_not_alphabetical() {
        // Regression: a BTreeMap re-sorted the app list alphabetically on save,
        // reordering the toolbar and possibly changing which file manager auto-
        // opens. IndexMap must preserve the order they were inserted.
        let mut apps = IndexMap::new();
        apps.insert("zed".to_string(), entry("Zed"));
        apps.insert("kate".to_string(), entry("Kate"));
        apps.insert("dolphin".to_string(), entry("Dolphin"));
        let raw = Raw {
            default: DefaultSection::default(),
            apps,
            shortcuts: BTreeMap::new(),
            volumes: BTreeMap::new(),
        };
        let body = toml::to_string(&raw).unwrap();
        let zed = body.find("[apps.zed]").unwrap();
        let kate = body.find("[apps.kate]").unwrap();
        let dolphin = body.find("[apps.dolphin]").unwrap();
        assert!(zed < kate && kate < dolphin, "apps were reordered:\n{body}");
    }

    #[test]
    fn clip_clear_defaults_when_absent() {
        // A config with no clip_clear keys must default to the secure policy.
        let raw: Raw = toml::from_str("[default]\n").unwrap();
        assert!(raw.default.clip_clear);
        assert_eq!(raw.default.clip_clear_timeout, 30);
    }

    #[test]
    fn clip_clear_roundtrips() {
        let raw = Raw {
            default: DefaultSection {
                clip_clear: false,
                clip_clear_timeout: 45,
                ..DefaultSection::default()
            },
            apps: IndexMap::new(),
            shortcuts: BTreeMap::new(),
            volumes: BTreeMap::new(),
        };
        let text = toml::to_string(&raw).unwrap();
        let back: Raw = toml::from_str(&text).unwrap();
        assert!(!back.default.clip_clear);
        assert_eq!(back.default.clip_clear_timeout, 45);
    }

    #[test]
    fn clip_timeout_clamped_into_range() {
        assert_eq!(clamp_clip_timeout(0), CLIP_CLEAR_TIMEOUT_MIN);
        assert_eq!(clamp_clip_timeout(30), 30);
        assert_eq!(clamp_clip_timeout(999_999), CLIP_CLEAR_TIMEOUT_MAX);
    }
}
