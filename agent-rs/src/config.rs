//! Read/write `~/.config/veracage/config.toml`. Round-trips *every* section so
//! the picker can never drop a user's `[default]`/`[volumes]` settings, and the
//! format stays interchangeable with `config.py` (Python's `tomllib` reads what
//! we write, and we read what it writes). Tolerant of a malformed file (a
//! same-uid process can write it) — degrades to an empty config, like Python.

use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::apps::App;

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
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    apps: BTreeMap<String, AppEntry>,
    // Preserved verbatim across a save — the picker never touches per-volume
    // overrides.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    volumes: BTreeMap<String, toml::Value>,
}

#[derive(Serialize, Deserialize)]
struct DefaultSection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_used_app: Option<String>,
    #[serde(default)]
    gpu: bool,
    #[serde(default = "default_suspend")]
    suspend_action: String,
    #[serde(default = "default_theme")]
    theme: String,
    #[serde(default = "default_true")]
    exchange: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    exchange_dir: Option<String>,
}

impl Default for DefaultSection {
    fn default() -> Self {
        DefaultSection {
            last_used_app: None,
            gpu: false,
            suspend_action: default_suspend(),
            theme: default_theme(),
            exchange: true,
            exchange_dir: None,
        }
    }
}

fn default_suspend() -> String {
    "dismount".into()
}

fn default_theme() -> String {
    "light".into()
}

fn default_true() -> bool {
    true
}

#[derive(Serialize, Deserialize)]
struct AppEntry {
    name: String,
    exec: String,
    #[serde(default)]
    args: Vec<String>,
    // A legacy `category`/`note` key from an older config is simply ignored
    // (serde drops unknown fields) — we neither require nor write them.
}

// --- public config ----------------------------------------------------------

pub struct Config {
    pub apps: Vec<App>,
    pub last_used_app: Option<String>,
    pub gpu: bool,
    pub suspend_action: String,
    pub theme: String,             // "light" | "dark" (| "system", future)
    pub exchange: bool,            // host<->vault shared folder on/off
    pub exchange_dir: Option<String>,
    volumes: BTreeMap<String, toml::Value>, // opaque pass-through
}

impl Config {
    pub fn empty() -> Config {
        Config {
            apps: Vec::new(),
            last_used_app: None,
            gpu: false,
            suspend_action: "dismount".into(),
            theme: "light".into(),
            exchange: true,
            exchange_dir: None,
            volumes: BTreeMap::new(),
        }
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
    let apps = raw
        .apps
        .into_iter()
        .map(|(key, e)| App {
            key,
            name: e.name,
            exec: e.exec,
            args: e.args,
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
    Config {
        apps,
        last_used_app: raw.default.last_used_app,
        gpu: raw.default.gpu,
        suspend_action,
        theme,
        exchange: raw.default.exchange,
        exchange_dir: raw.default.exchange_dir,
        volumes: raw.volumes,
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
                    args: a.args.clone(),
                },
            )
        })
        .collect();
    let raw = Raw {
        default: DefaultSection {
            last_used_app: cfg.last_used_app.clone(),
            gpu: cfg.gpu,
            suspend_action: cfg.suspend_action.clone(),
            theme: cfg.theme.clone(),
            exchange: cfg.exchange,
            exchange_dir: cfg.exchange_dir.clone(),
        },
        apps,
        volumes: cfg.volumes.clone(),
    };
    let body = toml::to_string(&raw).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let text = format!(
        "# Veracage config — edit carefully, see `veracage configure`\n\n{body}"
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
