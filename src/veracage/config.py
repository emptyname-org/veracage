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


@dataclass
class VolumeConfig:
    """Per-volume overrides; unset (None) fields inherit from [default]."""
    gpu: bool | None = None
    default_app: str | None = None
    display_name: str | None = None


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


def load() -> Config:
    p = config_path()
    if not p.is_file():
        return Config(apps={}, last_used_app=None)
    with open(p, "rb") as f:
        raw = tomllib.load(f)
    apps_raw = raw.get("apps", {}) or {}
    apps: dict[str, App] = {}
    for key, entry in apps_raw.items():
        try:
            apps[key] = App(
                key=key,
                name=entry["name"],
                category=entry["category"],
                exec=entry["exec"],
                args=list(entry.get("args", [])),
                note=entry.get("note", ""),
            )
        except KeyError as e:
            print(f"veracage: config entry [apps.{key}] missing {e}; skipping",
                  file=sys.stderr)
    default = raw.get("default") or {}
    last = default.get("last_used_app")
    gpu = bool(default.get("gpu", False))
    suspend_action = str(default.get("suspend_action", "dismount"))
    if suspend_action not in ("dismount", "ignore"):
        print(f"veracage: invalid suspend_action {suspend_action!r} "
              f"(want 'dismount' or 'ignore'); using 'dismount'",
              file=sys.stderr)
        suspend_action = "dismount"
    volumes: dict[str, VolumeConfig] = {}
    for key, entry in (raw.get("volumes") or {}).items():
        if not isinstance(entry, dict):
            continue
        volumes[_norm_vault(key)] = VolumeConfig(
            gpu=entry.get("gpu"),
            default_app=entry.get("default_app"),
            display_name=entry.get("display_name"),
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
        lines += [""]
    p.write_text("\n".join(lines))
    return p


def _esc(s: str) -> str:
    return s.replace("\\", "\\\\").replace('"', '\\"')


def _toml_list(xs: list[str]) -> str:
    return "[" + ", ".join(f'"{_esc(x)}"' for x in xs) + "]"


def _toml_bool(b: bool) -> str:
    return "true" if b else "false"
