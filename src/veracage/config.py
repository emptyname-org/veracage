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
from dataclasses import dataclass
from pathlib import Path

from .apps import App


def config_path() -> Path:
    base = Path(os.environ.get("XDG_CONFIG_HOME") or (Path.home() / ".config"))
    return base / "veracage" / "config.toml"


@dataclass
class Config:
    apps: dict[str, App]
    last_used_app: str | None = None

    def is_empty(self) -> bool:
        return not self.apps


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
    last = (raw.get("default") or {}).get("last_used_app")
    return Config(apps=apps, last_used_app=last)


def save(cfg: Config) -> Path:
    p = config_path()
    p.parent.mkdir(parents=True, exist_ok=True)
    lines: list[str] = ["# Veracage config — edit carefully, see `veracage configure`",
                        ""]
    if cfg.last_used_app:
        lines += ["[default]",
                  f'last_used_app = "{_esc(cfg.last_used_app)}"',
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
    p.write_text("\n".join(lines))
    return p


def _esc(s: str) -> str:
    return s.replace("\\", "\\\\").replace('"', '\\"')


def _toml_list(xs: list[str]) -> str:
    return "[" + ", ".join(f'"{_esc(x)}"' for x in xs) + "]"
