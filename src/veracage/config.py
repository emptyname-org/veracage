"""Config file at ~/.config/veracage/config.toml.

Schema:

    [default]
    last_used_app = "kate"

    [apps.kate]
    name     = "Kate"
    exec     = "kate"

The `apps.*` table is the user's enabled list - any installed binary they added.
Anything not in the config is not shown as a toolbar launcher by `veracage open`.
A legacy `args` key (the old "open at /vaults" launch argument) is ignored on
load and dropped on save: apps always launch bare.
"""
from __future__ import annotations

import configparser
import contextlib
import math
import os
import struct
import sys
import tomllib
from dataclasses import dataclass, field
from pathlib import Path

from .apps import App


def config_path() -> Path:
    base = Path(os.environ.get("XDG_CONFIG_HOME") or (Path.home() / ".config"))
    return base / "veracage" / "config.toml"


def _capitalize_first(s: str) -> str:
    """Capitalize the first character so a bare basename name like 'kate' shows
    as 'Kate'. Leaves the rest of the string as-is."""
    return s[:1].upper() + s[1:] if s else s


_VALID_BACKENDS = ("auto", "luks", "veracrypt")


@dataclass
class VolumeConfig:
    """Per-volume overrides; unset (None) fields inherit from [default]."""
    default_app: str | None = None
    display_name: str | None = None
    backend: str | None = None   # "auto" | "luks" | "veracrypt"


def _norm_vault(p: str) -> str:
    """Canonical key for a vault path (expanduser + resolve), so a config
    entry written as ~/x.vc matches the resolved path the launcher passes."""
    return str(Path(p).expanduser().resolve())


_VALID_FONTS = ("system", "noto", "liberation", "dejavu", "dejavu-mono")

# fontconfig family for each ui_font key ("system" -> the host desktop font).
_FONT_FAMILIES = {
    "noto": "Noto Sans",
    "liberation": "Liberation Sans",
    "dejavu": "DejaVu Sans",
    "dejavu-mono": "DejaVu Sans Mono",
}

# Base POINT size when the host size can't be read (matches agent fonts.rs).
FALLBACK_FONT_PT = 10.0


def _font_dpi() -> float:
    """The font DPI Qt/KDE renders points at. KDE's forceFontDPI override if set,
    else the CSS/logical 96 (physical HiDPI scaling is handled separately by the
    compositor's pixels_per_point, so this stays the logical DPI)."""
    for tool in ("kreadconfig6", "kreadconfig5"):
        v = _run(tool, ["--group", "General", "--key", "forceFontDPI"])
        if v:
            try:
                dpi = float(v)
            except ValueError:
                dpi = 0.0
            if dpi > 0:
                return dpi
    return 96.0


def _pt_to_px(pt: float) -> float:
    """Convert a typographic point size to logical pixels (egui's unit)."""
    return round(pt * _font_dpi() / 72.0)


def _valid_font_size(s: str) -> bool:
    """'system' or an integer point size in a sane range."""
    if s == "system":
        return True
    try:
        return 6 <= int(s) <= 48
    except (TypeError, ValueError):
        return False


# Modifier-mapping choices (mirrors agent-rs keyboard.rs CHOICES): "system"
# follows the host desktop, "none" strips any modifier remapping it has, the
# rest are XKB options verbatim.
_MODIFIER_KEYS = ("system", "none", "altwin:ctrl_win", "altwin:alt_win",
                  "altwin:ctrl_alt_win", "ctrl:swap_lwin_lctl",
                  "ctrl:swap_lalt_lctl", "ctrl:nocaps")

# The XKB option groups the modifier setting owns. Choosing a mapping replaces
# the host's entries in these groups and leaves its other options alone.
_MODIFIER_GROUPS = ("altwin", "ctrl")


# Host-detection tools (kreadconfig, gsettings, fc-match) run on the `veracage
# open` path. One that hangs would block a mount for as long as it hangs, so
# every probe is bounded: no answer in this long is read as "not installed".
_PROBE_TIMEOUT = 2.0


def _run(bin_: str, args: list[str]) -> str | None:
    """Run a command, return trimmed stdout, or None on any failure."""
    import subprocess
    try:
        r = subprocess.run([bin_, *args], capture_output=True, text=True,
                           timeout=_PROBE_TIMEOUT)
    except (OSError, subprocess.SubprocessError, UnicodeDecodeError):
        return None
    if r.returncode != 0:
        return None
    out = r.stdout.strip()
    return out or None


def host_ui_font() -> tuple[str | None, float | None]:
    """The host desktop's UI font family + point size, if detectable: KDE
    (kreadconfig), then GNOME (gsettings), then fontconfig's sans-serif."""
    for tool in ("kreadconfig6", "kreadconfig5"):
        v = _run(tool, ["--group", "General", "--key", "font"])
        if v:
            fam, _, rest = v.partition(",")
            fam = fam.strip()
            if fam:
                size = None
                sz = rest.split(",")[0].strip() if rest else ""
                try:
                    size = float(sz) if sz else None
                except ValueError:
                    size = None
                return fam, size
    v = _run("gsettings", ["get", "org.gnome.desktop.interface", "font-name"])
    if v:
        v = v.strip().strip("'\"")
        if "," in v:
            fam, _, sz = v.partition(",")
            try:
                return fam.strip(), float(sz.strip())
            except ValueError:
                return fam.strip() or None, None
        fam, _, sz = v.rpartition(" ")
        try:
            return (fam.strip() or v), float(sz)
        except ValueError:
            return (v or None), None
    v = _run("fc-match", ["--format=%{family}", "sans-serif"])
    if v:
        return v.split(",")[0].strip() or None, None
    return None, None


def host_keyboard() -> tuple[str, str, str, str]:
    """The host desktop's XKB configuration (model, layout, variant, options),
    if detectable. KDE keeps it in kxkbrc, and `Use=false` means it leaves the
    layout to the system, so we do too. Empty fields mean libxkbcommon's own
    default, which is what an undetectable desktop gets."""
    def read(key: str) -> str | None:
        for tool in ("kreadconfig6", "kreadconfig5"):
            v = _run(tool, ["--file", "kxkbrc", "--group", "Layout", "--key", key])
            if v is not None:
                return v
        return None

    if read("Use") != "true":
        return ("", "", "", "")
    return (read("Model") or "", read("LayoutList") or "",
            read("VariantList") or "", read("Options") or "")


def host_single_click() -> bool:
    """Whether the host desktop opens files on a SINGLE click (KDE's
    `[KDE] SingleClick` in kdeglobals). Undetectable hosts get False: KDE's own
    built-in default is single click, so an unseeded sandbox opens files on one
    click even when the host is set to two, which is the surprise this exists to
    remove."""
    for tool in ("kreadconfig6", "kreadconfig5"):
        v = _run(tool, ["--group", "KDE", "--key", "SingleClick"])
        if v is not None:
            return v.strip().lower() == "true"
    return False


def modifier_options(host_options: str, choice: str) -> str:
    """The XKB option list to publish: the host's options with the configured
    modifier mapping applied (mirrors agent-rs keyboard.rs `options_for`)."""
    if choice == "system":
        return host_options
    kept = [o.strip() for o in host_options.split(",")
            if o.strip() and o.split(":")[0] not in _MODIFIER_GROUPS]
    if choice != "none":
        kept.append(choice)
    return ",".join(kept)


def app_font(cfg: "Config") -> tuple[str, float]:
    """The font for apps INSIDE the sandbox: family and POINT size. Same setting
    that sizes the Veracage UI, but in Qt's own unit (kdeglobals stores points),
    so no pixel conversion. "system" on either follows the host desktop."""
    host_family, host_pt = host_ui_font()
    family = host_family if cfg.ui_font == "system" else _FONT_FAMILIES.get(cfg.ui_font)
    if not family:
        family = host_family or _FONT_FAMILIES["noto"]
    if cfg.ui_font_size == "system":
        points = host_pt or FALLBACK_FONT_PT
    else:
        points = float(cfg.ui_font_size)   # validated on load
    return family, points


def font_file(ui_font: str) -> str:
    """Resolve the configured font key to a system font FILE path via fontconfig.
    Empty string for the host font that can't be pinned or a family that isn't
    installed - the UI then keeps its built-in face."""
    family = host_ui_font()[0] if ui_font == "system" else _FONT_FAMILIES.get(ui_font)
    if not family:
        return ""
    v = _run("fc-match", ["--format=%{family}|%{file}", f"{family}:style=Regular"])
    if not v or "|" not in v:
        return ""
    matched, _, path = v.partition("|")
    if family.lower() not in matched.lower():   # substituted -> not installed
        return ""
    return path.strip()


def _hinted_ascent_factor(path: str, px: float) -> float:
    """Qt/FreeType hint the font's ascender UP to the pixel grid; egui does not
    hint at all, so at the same nominal size its text renders one pixel shorter
    than every Qt app. Derive Qt's rounding from the font's own head/hhea
    tables: scale so the unhinted ascender lands on the hinted integer. 1.0
    when the tables can't be read."""
    try:
        data = Path(path).read_bytes()
        base = struct.unpack(">I", data[12:16])[0] if data[:4] == b"ttcf" else 0
        num = struct.unpack(">H", data[base + 4:base + 6])[0]
        head = hhea = None
        for i in range(min(num, 64)):
            e = base + 12 + 16 * i
            tag = data[e:e + 4]
            off = struct.unpack(">I", data[e + 8:e + 12])[0]
            if tag == b"head":
                head = off
            elif tag == b"hhea":
                hhea = off
        if head is None or hhea is None:
            return 1.0
        upem = struct.unpack(">H", data[head + 18:head + 20])[0]
        ascender = struct.unpack(">h", data[hhea + 4:hhea + 6])[0]
        if upem <= 0 or ascender <= 0:
            return 1.0
        ascent_px = px * ascender / upem
        return min(max(math.ceil(ascent_px) / ascent_px, 1.0), 1.25)
    except (OSError, struct.error, IndexError, ZeroDivisionError):
        return 1.0


def base_font_size(ui_font_size: str, ui_font: str = "system") -> float:
    """The base body size for egui, in LOGICAL PIXELS. The configured/host size is
    in typographic points ('system' -> the host point size, else the number);
    this converts points -> pixels via the font DPI so Veracage matches the
    desktop (a 12pt/96dpi UI renders at 16px, not 12), then applies the hinting
    correction of the resolved font so the optical height matches Qt's."""
    if ui_font_size == "system":
        pt = host_ui_font()[1]
        pt = pt if pt is not None else FALLBACK_FONT_PT
    else:
        try:
            pt = float(ui_font_size)
        except (TypeError, ValueError):
            pt = FALLBACK_FONT_PT
    px = min(max(_pt_to_px(pt), 6.0), 72.0)
    path = font_file(ui_font)
    if path:
        px = min(px * _hinted_ascent_factor(path, px), 72.0)
    return px


def _window_size_valid(s: str) -> bool:
    """Accept 'default', 'max', or '<w>x<h>' within sane bounds."""
    if s in ("default", "max"):
        return True
    parts = s.split("x")
    if len(parts) != 2:
        return False
    try:
        w, h = int(parts[0]), int(parts[1])
    except ValueError:
        return False
    return 320 <= w <= 16384 and 240 <= h <= 16384


# User-configurable Veracage keyboard shortcuts (the compositor-level clipboard
# transfers; the apps' own Cut/Copy/Paste are not Veracage bindings).
_DEFAULT_SHORTCUTS = {"copy_out": "Ctrl+Alt+C", "paste_in": "Ctrl+Alt+V"}
_SHORTCUT_ACTIONS = ("copy_out", "paste_in")


def _default_shortcuts() -> dict[str, str]:
    return dict(_DEFAULT_SHORTCUTS)


def _valid_keybind(s: str) -> bool:
    """A '+'-joined combo whose final part is a non-empty key token."""
    if not isinstance(s, str) or not s:
        return False
    parts = [p for p in s.split("+") if p]
    return len(parts) >= 1 and bool(parts[-1])


# Host-clipboard auto-clear (KeePassXC-style): default on, 30s, clamped in range.
DEFAULT_CLIP_CLEAR_TIMEOUT = 30
_CLIP_CLEAR_TIMEOUT_MIN = 1
_CLIP_CLEAR_TIMEOUT_MAX = 3600


def _coerce_clip_timeout(val: object) -> int:
    """Clamp the auto-clear timeout (seconds) into range. A non-int (bool counts
    as non-int here) or out-of-range value falls back to / is clamped to a sane
    value, so a crafted config can't wedge the timer (0 = clear instantly)."""
    if isinstance(val, bool) or not isinstance(val, int):
        print(f"veracage: invalid clip_clear_timeout {val!r}, using "
              f"{DEFAULT_CLIP_CLEAR_TIMEOUT}", file=sys.stderr)
        return DEFAULT_CLIP_CLEAR_TIMEOUT
    return max(_CLIP_CLEAR_TIMEOUT_MIN, min(val, _CLIP_CLEAR_TIMEOUT_MAX))


# Idle minutes before the session dismounts itself (0 = off). Bounded so a
# crafted config can neither wedge the timer nor keep a volume open forever.
_AUTO_DISMOUNT_MAX = 1440


def _coerce_auto_dismount(val: object) -> int:
    """Clamp the auto-dismount idle timeout (minutes) into range. A non-int
    (bool counts as non-int) falls back to off."""
    if isinstance(val, bool) or not isinstance(val, int):
        print(f"veracage: invalid auto_dismount {val!r}, using 0 (off)", file=sys.stderr)
        return 0
    return max(0, min(val, _AUTO_DISMOUNT_MAX))


@dataclass
class Config:
    apps: dict[str, App]
    last_used_app: str | None = None
    suspend_action: str = "dismount"      # "dismount" | "ignore"
    theme: str = "light"                  # compositor/agent egui theme: light|dark|system
    ui_font: str = "system"               # UI font key (see _VALID_FONTS); system = host
    ui_font_size: str = "system"          # "system" (host size) | a point size
    window_size: str = "default"          # window size at compositor start
    modifier_keys: str = "system"         # modifier mapping (see _MODIFIER_KEYS)
    exchange: bool = True                 # host<->volume shared directory
    exchange_dir: str | None = None       # default ~/Veracage/Exchange when unset
    clip_clear: bool = True               # auto-clear host clipboard after Copy out
    clip_clear_timeout: int = DEFAULT_CLIP_CLEAR_TIMEOUT   # seconds before it fires
    auto_dismount: int = 0                # idle minutes before a dismount (0 = off)
    debug: bool = False                   # verbose timing logs (see docs/debugging.md)
    shortcuts: dict[str, str] = field(default_factory=_default_shortcuts)
    volumes: dict[str, VolumeConfig] = field(default_factory=dict)

    def exchange_path(self) -> Path:
        """The host exchange directory (default ~/Veracage/Exchange)."""
        return (Path(self.exchange_dir).expanduser() if self.exchange_dir
                else Path.home() / "Veracage" / "Exchange")

    def is_empty(self) -> bool:
        return not self.apps

    def default_app_for(self, vault: str) -> str | None:
        vc = self.volumes.get(_norm_vault(vault))
        return vc.default_app if vc is not None else None

    def backend_for(self, vault: str) -> str:
        """Crypto backend for `vault`: per-volume override, else 'auto'."""
        vc = self.volumes.get(_norm_vault(vault))
        if vc is not None and vc.backend is not None:
            return vc.backend
        return "auto"


def load() -> Config:
    """Parse the config. Tolerant of a malformed/crafted file: any parse or
    type error degrades to defaults with a warning rather than crashing the
    agent/session (a same-uid process can write this file)."""
    p = config_path()
    if not p.is_file():
        return Config(apps={})
    try:
        with open(p, "rb") as f:
            raw = tomllib.load(f)
    except (tomllib.TOMLDecodeError, OSError) as e:
        print(f"veracage: cannot read {p}: {e}, using empty config",
              file=sys.stderr)
        return Config(apps={})
    if not isinstance(raw, dict):
        return Config(apps={})

    apps: dict[str, App] = {}
    seen_basenames: set[str] = set()
    apps_raw = raw.get("apps")
    if isinstance(apps_raw, dict):
        for key, entry in apps_raw.items():
            if not isinstance(entry, dict):
                print(f"veracage: [apps.{key}] is not a table, skipping",
                      file=sys.stderr)
                continue
            try:
                # An `args` key is ignored (apps launch bare). Dedupe by exec
                # basename, first entry wins: a config can name the same app
                # twice (e.g. `dolphin` and `/usr/bin/dolphin`).
                exec_ = entry["exec"]
                base = os.path.basename(str(exec_))
                if base in seen_basenames:
                    continue
                seen_basenames.add(base)
                apps[key] = App(
                    key=key,
                    name=_capitalize_first(str(entry["name"])),
                    exec=exec_,
                )
            except (KeyError, TypeError) as e:
                print(f"veracage: config entry [apps.{key}] invalid ({e}), skipping",
                      file=sys.stderr)

    default = raw.get("default")
    if not isinstance(default, dict):
        default = {}
    last = default.get("last_used_app")
    if not isinstance(last, str):
        last = None
    # A `gpu` key is ignored on load and dropped on save: the GPU is always
    # passed through to the sandbox.
    exchange = _coerce_bool(default.get("exchange", True), "default.exchange")
    exchange_dir = default.get("exchange_dir") or None
    if exchange_dir is not None and not isinstance(exchange_dir, str):
        # It becomes a path (and a bwrap argument). A non-string used to
        # raise out of cfg.exchange_path() and traceback `veracage open`.
        print(f"veracage: invalid exchange_dir {exchange_dir!r}, using the default",
              file=sys.stderr)
        exchange_dir = None
    clip_clear = _coerce_bool(default.get("clip_clear", True), "default.clip_clear")
    debug = _coerce_bool(default.get("debug", False), "default.debug")
    clip_clear_timeout = _coerce_clip_timeout(
        default.get("clip_clear_timeout", DEFAULT_CLIP_CLEAR_TIMEOUT))
    auto_dismount = _coerce_auto_dismount(default.get("auto_dismount", 0))
    theme = default.get("theme", "light")
    if theme not in ("light", "dark", "system"):
        print(f"veracage: invalid theme {theme!r}, using 'light'", file=sys.stderr)
        theme = "light"
    ui_font = default.get("ui_font", "system")
    if ui_font not in _VALID_FONTS:
        print(f"veracage: invalid ui_font {ui_font!r}, using 'system'", file=sys.stderr)
        ui_font = "system"
    ui_font_size = str(default.get("ui_font_size", "system"))
    if not _valid_font_size(ui_font_size):
        print(f"veracage: invalid ui_font_size {ui_font_size!r}, using 'system'",
              file=sys.stderr)
        ui_font_size = "system"
    window_size = default.get("window_size", "default")
    if not isinstance(window_size, str) or not _window_size_valid(window_size):
        print(f"veracage: invalid window_size {window_size!r}, using 'default'",
              file=sys.stderr)
        window_size = "default"
    modifier_keys = default.get("modifier_keys", "system")
    if modifier_keys not in _MODIFIER_KEYS:
        print(f"veracage: invalid modifier_keys {modifier_keys!r}, using 'system'",
              file=sys.stderr)
        modifier_keys = "system"
    suspend_action = default.get("suspend_action", "dismount")
    if suspend_action not in ("dismount", "ignore"):
        print(f"veracage: invalid suspend_action {suspend_action!r} "
              f"(want 'dismount' or 'ignore'), using 'dismount'", file=sys.stderr)
        suspend_action = "dismount"

    volumes: dict[str, VolumeConfig] = {}
    vol_raw = raw.get("volumes")
    if isinstance(vol_raw, dict):
        for key, entry in vol_raw.items():
            if not isinstance(entry, dict):
                continue
            try:
                nk = _norm_vault(key)
            except (OSError, RuntimeError):
                nk = str(Path(key).expanduser())
            bval = entry.get("backend")
            if bval is not None and bval not in _VALID_BACKENDS:
                print(f'veracage: volumes."{key}".backend {bval!r} invalid, ignoring',
                      file=sys.stderr)
                bval = None
            volumes[nk] = VolumeConfig(
                default_app=entry.get("default_app"),
                display_name=entry.get("display_name"),
                backend=bval,
            )
    shortcuts = _default_shortcuts()
    sc_raw = raw.get("shortcuts")
    if isinstance(sc_raw, dict):
        for action in _SHORTCUT_ACTIONS:
            v = sc_raw.get(action)
            if isinstance(v, str) and _valid_keybind(v):
                shortcuts[action] = v
            elif v is not None:
                print(f"veracage: invalid shortcut [shortcuts].{action} {v!r}; "
                      f"using {shortcuts[action]!r}", file=sys.stderr)

    return Config(apps=apps, last_used_app=last,
                  suspend_action=suspend_action, theme=theme, ui_font=ui_font,
                  ui_font_size=ui_font_size, window_size=window_size,
                  modifier_keys=modifier_keys,
                  exchange=exchange, exchange_dir=exchange_dir,
                  clip_clear=clip_clear, clip_clear_timeout=clip_clear_timeout,
                  auto_dismount=auto_dismount,
                  debug=debug, shortcuts=shortcuts, volumes=volumes)


def save(cfg: Config) -> Path:
    p = config_path()
    p.parent.mkdir(parents=True, exist_ok=True)
    lines: list[str] = ["# Veracage config - edit carefully, see `veracage configure`",
                        "", "[default]"]
    if cfg.last_used_app:
        lines += [f'last_used_app  = "{_esc(cfg.last_used_app)}"']
    lines += [f'theme          = "{_esc(cfg.theme)}"',
              f'ui_font        = "{_esc(cfg.ui_font)}"',
              f'ui_font_size   = "{_esc(cfg.ui_font_size)}"',
              f'window_size    = "{_esc(cfg.window_size)}"',
              f'modifier_keys  = "{_esc(cfg.modifier_keys)}"',
              f"exchange       = {_toml_bool(cfg.exchange)}",
              f'suspend_action = "{_esc(cfg.suspend_action)}"',
              f"clip_clear     = {_toml_bool(cfg.clip_clear)}",
              f"clip_clear_timeout = {int(cfg.clip_clear_timeout)}",
              f"auto_dismount  = {int(cfg.auto_dismount)}",
              f"debug          = {_toml_bool(cfg.debug)}"]
    if cfg.exchange_dir:
        lines += [f'exchange_dir   = "{_esc(cfg.exchange_dir)}"']
    lines += [
              ""]
    lines += ["[shortcuts]"]
    for action in _SHORTCUT_ACTIONS:
        lines += [f'{action:<9} = "{_esc(cfg.shortcuts.get(action, _DEFAULT_SHORTCUTS[action]))}"']
    lines += [""]
    for key, a in cfg.apps.items():
        # Quoted like the [volumes."..."] tables below. A bare key is only
        # valid TOML for [A-Za-z0-9_-], and an app key that is not (a
        # hand-edited [apps."VS Code"], or `configure --key "my key"`) used to
        # be written back unquoted: the file then failed to parse and the NEXT
        # load silently fell back to an empty config, losing every app, the
        # theme, the shortcuts and every per-volume override.
        lines += [f'[apps."{_esc(key)}"]',
                  f'name     = "{_esc(a.name)}"',
                  f'exec     = "{_esc(a.exec)}"',
                  ""]
    for key, vc in cfg.volumes.items():
        lines += [f'[volumes."{_esc(key)}"]']
        if vc.display_name is not None:
            lines += [f'display_name = "{_esc(vc.display_name)}"']
        if vc.default_app is not None:
            lines += [f'default_app  = "{_esc(vc.default_app)}"']
        if vc.backend is not None:
            lines += [f'backend      = "{_esc(vc.backend)}"']
        lines += [""]
    # Atomic write: a concurrent `veracage open` reading config mid-save must
    # never see a truncated file (which load() treats as "no apps", then
    # auto-detect overwrites the user's curated selection).
    # Unique (pid-tagged) temp name so a concurrent save from another process
    # (e.g. the Settings window racing the configure picker) can't clobber our
    # tmp mid-write; the rename stays atomic either way.
    tmp = p.with_name(f"{p.name}.{os.getpid()}.tmp")
    tmp.write_text("\n".join(lines))
    tmp.replace(p)
    return p


# --------------------------------------------------- compositor publish ----

# The human-published dir the compositor's Apps menu reads (created by the
# helper at compositor spawn, owned by the human uid). Must match PUB_DIR in
# toolbar.rs / broker.rs. Overridable via VERACAGE_PUB_DIR (read at call time, so
# tests redirect it away from a running session's real dir).
_PUB_DIR_DEFAULT = "/run/veracage/pub"


def _pub_dir() -> Path:
    return Path(os.environ.get("VERACAGE_PUB_DIR") or _PUB_DIR_DEFAULT)


# The sandbox starts with an empty XDG config tmpfs, so its file managers have
# no file-type associations and would ask "open with?" for every file. Publish
# a mimeapps.list seed (the leader copies it into each sandbox): the enabled
# apps become the default handlers for the types their .desktop files declare
# (config order, first app wins), and the host's own defaults fill in the rest.


def _application_dirs() -> list[Path]:
    """The XDG application directories, most-specific first (user overrides
    system). Mirrors application_dirs in agent-rs detect.rs."""
    data_home = os.environ.get("XDG_DATA_HOME") or str(Path.home() / ".local/share")
    data_dirs = os.environ.get("XDG_DATA_DIRS") or "/usr/local/share:/usr/share"
    return [Path(d) / "applications"
            for d in [data_home, *data_dirs.split(":")] if d]


# A .desktop file is read only for its Exec and MimeType keys, so stop at the
# end of the [Desktop Entry] group and cap the read: KDE entries carry hundreds
# of Name[xx]= translations and trailing [Desktop Action] groups after them.
_DESKTOP_READ_LIMIT = 256 * 1024


def _desktop_entry_fields(body: str) -> tuple[str, str]:
    """(Exec binary basename, raw MimeType value) from a .desktop file's
    `[Desktop Entry]` group. Empty strings when absent."""
    in_entry = False
    exec_bin = ""
    mimes = ""
    for line in body.splitlines():
        line = line.strip()
        if line.startswith("["):
            if in_entry:
                break            # past [Desktop Entry]: nothing left to find
            in_entry = line == "[Desktop Entry]"
            continue
        if not in_entry:
            continue
        if line.startswith("Exec=") and not exec_bin:
            tokens = line[len("Exec="):].split()
            first = tokens[0] if tokens else ""
            base = first.rsplit("/", 1)[-1]
            exec_bin = "" if base.startswith("%") else base
        elif line.startswith("MimeType=") and not mimes:
            mimes = line[len("MimeType="):]
        if exec_bin and mimes:
            break
    return exec_bin, mimes


def _enabled_desktop_entries(apps: list[App]) -> dict[str, tuple[str, str]]:
    """Exec basename -> (desktop id, raw MimeType value) for the enabled apps,
    found by scanning the XDG application dirs (user dirs first, so a user
    override wins). Stops as soon as every app is resolved, since the system dir
    holds hundreds of entries this never needs to look at."""
    targets = {os.path.basename(a.exec) for a in apps}
    found: dict[str, tuple[str, str]] = {}
    for d in _application_dirs():
        try:
            entries = sorted(os.scandir(d), key=lambda e: e.name)
        except OSError:
            continue
        for e in entries:
            if targets <= found.keys():
                return found
            if not e.name.endswith(".desktop"):
                continue
            try:
                with open(e.path, errors="replace") as f:
                    body = f.read(_DESKTOP_READ_LIMIT)
            except OSError:
                continue
            exec_bin, mimes = _desktop_entry_fields(body)
            if exec_bin in targets and exec_bin not in found and mimes:
                found[exec_bin] = (e.name, mimes)
    return found


def _mime_defaults(entries: dict[str, tuple[str, str]],
                   apps: list[App]) -> dict[str, str]:
    """Mime type -> desktop id: each enabled app claims the types its .desktop
    declares. On overlap the first app in config order wins."""
    defaults: dict[str, str] = {}
    for a in apps:
        hit = entries.get(os.path.basename(a.exec))
        if hit is None:
            continue
        desktop_id, mimes = hit
        for mime in (m.strip() for m in mimes.split(";")):
            if mime:
                defaults.setdefault(mime, desktop_id)
    return defaults


def _host_mimeapps() -> configparser.ConfigParser:
    """The host's own mimeapps.list (missing or unparsable reads as empty)."""
    cp = configparser.ConfigParser(interpolation=None, strict=False,
                                   delimiters=("=",))
    cp.optionxform = str  # type: ignore[assignment]  # mime types are case-sensitive
    base = Path(os.environ.get("XDG_CONFIG_HOME") or (Path.home() / ".config"))
    with contextlib.suppress(OSError, configparser.Error):
        cp.read_string((base / "mimeapps.list").read_text(errors="replace"))
    return cp


def _mimeapps_body(cfg: Config) -> str:
    """The mimeapps.list seed: every type an enabled app declares, plus the
    host's own associations for the types they don't declare, restricted to
    handlers that are themselves enabled apps.

    Only enabled apps may be named. The sandbox has the whole read-only /usr, so
    an unrestricted host entry (`x-scheme-handler/http=firefox.desktop`) would
    let a click start an app the user never enabled, inside the volume sandbox
    and with no network to serve it."""
    apps = list(cfg.apps.values())
    entries = _enabled_desktop_entries(apps)
    defaults = _mime_defaults(entries, apps)
    allowed = {desktop_id for desktop_id, _ in entries.values()}
    host = _host_mimeapps()
    out = ["[Default Applications]\n"]
    for section in ("Default Applications", "Added Associations"):
        if not host.has_section(section):
            continue
        for mime, ids in host.items(section):
            keep = [i for i in (v.strip() for v in ids.split(";")) if i in allowed]
            if keep:
                defaults.setdefault(mime, ";".join(keep))
    out += [f"{mime}={ids}{'' if ids.endswith(';') else ';'}\n"
            for mime, ids in defaults.items()]
    return "".join(out)


def publish_apps(cfg: Config) -> None:
    """Publish the enabled app list to `pub/config.apps` (`<key>\\t<name>` per
    line) so the compositor's Apps menu has content before any volume is
    mounted. Best-effort: the dir exists only once the compositor has been
    spawned, and the menu degrades gracefully without the file. The broker
    (veracage-agent) writes the same file and adds menu icons."""
    pub = _pub_dir()
    if not pub.is_dir():
        return
    sc = cfg.shortcuts
    apps = "".join(
        f"{a.key}\t{a.name}\n" for a in cfg.apps.values()
        if a.key and len(a.key) <= 64 and "/" not in a.key and a.key != ".."
    )
    # Each file the compositor reads on its scan: the app list, the UI font
    # (path + base size), and the keyboard shortcuts. All published atomically
    # (temp + replace). The window size is NOT here: it applies when the window
    # is created, forwarded as VERACAGE_WINDOW_SIZE by cli.py.
    _publish_atomic(pub, "config.apps", apps)
    _publish_atomic(pub, "theme", cfg.theme + "\n")
    # The app-side font (family + points): the leader builds the sandbox's
    # kdeglobals from this, pub/theme and pub/singleclick.
    family, points = app_font(cfg)
    _publish_atomic(pub, "appfont", f"{family}\n{points:g}\n")
    _publish_atomic(pub, "singleclick", f"{1 if host_single_click() else 0}\n")
    _publish_atomic(pub, "font",
                    f"{font_file(cfg.ui_font)}\n{base_font_size(cfg.ui_font_size, cfg.ui_font)}\n")
    _publish_atomic(pub, "shortcuts",
                    "".join(f"{a}\t{sc.get(a, _DEFAULT_SHORTCUTS[a])}\n" for a in _SHORTCUT_ACTIONS))
    model, layout, variant, options = host_keyboard()
    _publish_atomic(pub, "keyboard",
                    f"{model}\n{layout}\n{variant}\n"
                    f"{modifier_options(options, cfg.modifier_keys)}\n")
    _publish_atomic(pub, "autodismount", f"{int(cfg.auto_dismount)}\n")
    _publish_atomic(pub, "clipclear",
                    f"{1 if cfg.clip_clear else 0}\n{cfg.clip_clear_timeout}\n")
    # The default-app associations the leader seeds into each sandbox.
    _publish_atomic(pub, "mimeapps.list", _mimeapps_body(cfg))


def _publish_atomic(pub: Path, name: str, body: str) -> None:
    """Write `body` to `pub/<name>` atomically (pid-tagged temp + replace).
    Best-effort but NOT silent: a failure is logged, and the temp is removed."""
    tmp = pub / f"{name}.{os.getpid()}.tmp"
    try:
        tmp.write_text(body)
        tmp.replace(pub / name)
    except OSError as e:
        print(f"veracage: could not publish {name}: {e}", file=sys.stderr)
        with contextlib.suppress(OSError):
            tmp.unlink()


_ESC_MAP = {"\\": "\\\\", '"': '\\"', "\n": "\\n", "\t": "\\t",
            "\r": "\\r", "\f": "\\f", "\b": "\\b"}


def _esc(s: str) -> str:
    """Escape a string for a TOML basic string, including control chars, so a
    name/path with a newline/tab can't produce an invalid file that the next
    load() chokes on."""
    out = []
    for ch in s:
        if ch in _ESC_MAP:
            out.append(_ESC_MAP[ch])
        elif ord(ch) < 0x20:
            out.append(f"\\u{ord(ch):04x}")
        else:
            out.append(ch)
    return "".join(out)


def _toml_bool(b: bool) -> str:
    return "true" if b else "false"


def _coerce_bool(val: object, where: str) -> bool:
    """Strict bool: only a real TOML bool counts. Anything else (e.g. the
    string "false", which is truthy) warns and is treated as False, so a bad
    value fails predictably instead of silently counting as true."""
    if isinstance(val, bool):
        return val
    print(f"veracage: {where} should be true/false, got {val!r}, using false",
          file=sys.stderr)
    return False
