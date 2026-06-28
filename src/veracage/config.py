"""Config file at ~/.config/veracage/config.toml.

Schema (slice 1.5):

    [default]
    last_used_app = "kate"

    [apps.kate]
    name     = "Kate"
    category = "text"
    exec     = "kate"
    args     = ["/vault"]

The `apps.*` table is the user's enabled allowlist. Anything not in the
config is not launchable by `veracage open`.
"""
from __future__ import annotations

import os
import sys
import tomllib
from dataclasses import dataclass, field
from pathlib import Path

from .apps import App


def config_path() -> Path:
    base = Path(os.environ.get("XDG_CONFIG_HOME") or (Path.home() / ".config"))
    return base / "veracage" / "config.toml"


_VALID_BACKENDS = ("auto", "luks", "veracrypt")


@dataclass
class VolumeConfig:
    """Per-volume overrides; unset (None) fields inherit from [default]."""
    gpu: bool | None = None
    default_app: str | None = None
    display_name: str | None = None
    backend: str | None = None   # "auto" | "luks" | "veracrypt"


def _norm_vault(p: str) -> str:
    """Canonical key for a vault path (expanduser + resolve), so a config
    entry written as ~/x.vc matches the resolved path the launcher passes."""
    return str(Path(p).expanduser().resolve())


@dataclass
class Config:
    apps: dict[str, App]
    last_used_app: str | None = None
    gpu: bool = False                     # /dev/dri passthrough default (off)
    suspend_action: str = "dismount"      # "dismount" | "ignore"
    volumes: dict[str, VolumeConfig] = field(default_factory=dict)

    def is_empty(self) -> bool:
        return not self.apps

    def gpu_for(self, vault: str) -> bool:
        """GPU policy for `vault`: per-volume override, else the default."""
        vc = self.volumes.get(_norm_vault(vault))
        if vc is not None and vc.gpu is not None:
            return vc.gpu
        return self.gpu

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
        print(f"veracage: cannot read {p}: {e}; using empty config",
              file=sys.stderr)
        return Config(apps={})
    if not isinstance(raw, dict):
        return Config(apps={})

    apps: dict[str, App] = {}
    apps_raw = raw.get("apps")
    if isinstance(apps_raw, dict):
        for key, entry in apps_raw.items():
            if not isinstance(entry, dict):
                print(f"veracage: [apps.{key}] is not a table; skipping",
                      file=sys.stderr)
                continue
            try:
                apps[key] = App(
                    key=key,
                    name=entry["name"],
                    category=entry["category"],
                    exec=entry["exec"],
                    args=list(entry.get("args", [])),
                    note=entry.get("note", ""),
                )
            except (KeyError, TypeError) as e:
                print(f"veracage: config entry [apps.{key}] invalid ({e}); skipping",
                      file=sys.stderr)

    default = raw.get("default")
    if not isinstance(default, dict):
        default = {}
    last = default.get("last_used_app")
    if not isinstance(last, str):
        last = None
    gpu = _coerce_bool(default.get("gpu", False), "default.gpu")
    suspend_action = default.get("suspend_action", "dismount")
    if suspend_action not in ("dismount", "ignore"):
        print(f"veracage: invalid suspend_action {suspend_action!r} "
              f"(want 'dismount' or 'ignore'); using 'dismount'", file=sys.stderr)
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
            gval = entry.get("gpu")
            bval = entry.get("backend")
            if bval is not None and bval not in _VALID_BACKENDS:
                print(f'veracage: volumes."{key}".backend {bval!r} invalid; ignoring',
                      file=sys.stderr)
                bval = None
            volumes[nk] = VolumeConfig(
                gpu=_coerce_bool(gval, f'volumes."{key}".gpu') if gval is not None else None,
                default_app=entry.get("default_app"),
                display_name=entry.get("display_name"),
                backend=bval,
            )
    return Config(apps=apps, last_used_app=last, gpu=gpu,
                  suspend_action=suspend_action, volumes=volumes)


def save(cfg: Config) -> Path:
    p = config_path()
    p.parent.mkdir(parents=True, exist_ok=True)
    lines: list[str] = ["# Veracage config — edit carefully, see `veracage configure`",
                        "", "[default]"]
    if cfg.last_used_app:
        lines += [f'last_used_app  = "{_esc(cfg.last_used_app)}"']
    lines += [f"gpu            = {_toml_bool(cfg.gpu)}",
              f'suspend_action = "{_esc(cfg.suspend_action)}"',
              ""]
    for key, a in cfg.apps.items():
        lines += [f"[apps.{key}]",
                  f'name     = "{_esc(a.name)}"',
                  f'category = "{_esc(a.category)}"',
                  f'exec     = "{_esc(a.exec)}"',
                  f"args     = {_toml_list(a.args)}"]
        if a.note:
            lines += [f'note     = "{_esc(a.note)}"']
        lines += [""]
    for key, vc in cfg.volumes.items():
        lines += [f'[volumes."{_esc(key)}"]']
        if vc.display_name is not None:
            lines += [f'display_name = "{_esc(vc.display_name)}"']
        if vc.default_app is not None:
            lines += [f'default_app  = "{_esc(vc.default_app)}"']
        if vc.gpu is not None:
            lines += [f"gpu          = {_toml_bool(vc.gpu)}"]
        if vc.backend is not None:
            lines += [f'backend      = "{_esc(vc.backend)}"']
        lines += [""]
    p.write_text("\n".join(lines))
    return p


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


def _toml_list(xs: list[str]) -> str:
    return "[" + ", ".join(f'"{_esc(x)}"' for x in xs) + "]"


def _toml_bool(b: bool) -> str:
    return "true" if b else "false"


def _coerce_bool(val: object, where: str) -> bool:
    """Strict bool: only a real TOML bool counts. Anything else (e.g. the
    string "false", which is truthy) warns and is treated as False — so a
    bad gpu value fails *closed*, not open."""
    if isinstance(val, bool):
        return val
    print(f"veracage: {where} should be true/false, got {val!r}; using false",
          file=sys.stderr)
    return False
