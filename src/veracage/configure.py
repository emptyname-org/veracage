"""`veracage configure` — manage the enabled app list.

There is no catalog / whitelist: you enable ANY installed binary. It is launched
in the sandbox against the vault (bwrap-confined — no host filesystem, no
network), so which binary it is doesn't widen what the vault can do.

  veracage configure --add kate --arg /vault   # enable `kate /vault`
  veracage configure --add /opt/foo/bin/foo    # enable an arbitrary binary
  veracage configure --remove kate             # disable it
  veracage configure --list                    # show enabled apps
  veracage configure                           # open the GUI picker
"""
from __future__ import annotations

import argparse
import re
import shutil
import subprocess
import sys
from pathlib import Path

from . import config
from .apps import App


def _resolve(binary: str) -> str | None:
    """The runnable path for `binary`: a $PATH lookup, or an absolute/relative
    path that exists. None if it isn't installed."""
    return shutil.which(binary) or (binary if Path(binary).is_file() else None)


def _key_for(binary: str, taken: set[str]) -> str:
    base = re.sub(r"[^a-z0-9_-]", "-", Path(binary).name.lower()).strip("-") or "app"
    key, n = base, 2
    while key in taken:
        key, n = f"{base}-{n}", n + 1
    return key


def _add(args: argparse.Namespace) -> int:
    if _resolve(args.add) is None:
        print(f"veracage: '{args.add}' is not installed / not on $PATH.\n"
              "Pass an installed binary name (e.g. `--add kate`) or an absolute path.",
              file=sys.stderr)
        return 2
    cfg = config.load()
    # Re-adding the same binary updates its entry rather than duplicating it.
    existing = next((k for k, a in cfg.apps.items() if a.exec == args.add), None)
    key = args.key or existing or _key_for(args.add, set(cfg.apps))
    name = args.name or Path(args.add).name
    cfg.apps = {**cfg.apps, key: App(key=key, name=name, exec=args.add,
                                     args=list(args.arg or []))}
    p = config.save(cfg)
    print(f"veracage: enabled '{name}' ({args.add}) as [{key}] → {p}")
    return 0


def _remove(args: argparse.Namespace) -> int:
    cfg = config.load()
    if args.remove not in cfg.apps:
        print(f"veracage: '{args.remove}' is not enabled.\n"
              f"Enabled: {', '.join(cfg.apps) or '(none)'}", file=sys.stderr)
        return 2
    cfg.apps = {k: a for k, a in cfg.apps.items() if k != args.remove}
    config.save(cfg)
    print(f"veracage: removed '{args.remove}'.")
    return 0


def _list(_args: argparse.Namespace) -> int:
    cfg = config.load()
    if not cfg.apps:
        print("No apps enabled. Add one with `veracage configure --add <binary>`.")
        return 0
    print(f"{len(cfg.apps)} app(s) enabled:")
    for key, a in cfg.apps.items():
        missing = "" if _resolve(a.exec) else "  (not installed)"
        argstr = (" " + " ".join(a.args)) if a.args else ""
        print(f"  {key:<16} {a.exec}{argstr}{missing}")
    return 0


def _gui(_args: argparse.Namespace) -> int:
    """Open the Rust agent's config-picker window (it writes config.toml itself)."""
    from .cli import AGENT_PATH  # lazy import avoids a cli <-> configure cycle
    exe = AGENT_PATH
    if not (Path(exe).is_file() or shutil.which(exe)):
        print("veracage: GUI agent not found. Use `--add` / `--remove` / `--list`,\n"
              "or build/install it with `make install`.", file=sys.stderr)
        return 2
    return subprocess.run([exe, "configure"]).returncode


def main(args: argparse.Namespace) -> int:
    if args.add is not None:
        return _add(args)
    if args.remove is not None:
        return _remove(args)
    if args.list:
        return _list(args)
    return _gui(args)


def add_subparser(sub: argparse._SubParsersAction) -> None:
    p = sub.add_parser("configure", help="manage the enabled sandbox apps")
    g = p.add_mutually_exclusive_group()
    g.add_argument("--add", metavar="BINARY",
                   help="enable an installed binary (name on $PATH, or a path)")
    g.add_argument("--remove", metavar="KEY", help="disable an app by its key")
    g.add_argument("--list", action="store_true", help="list enabled apps")
    p.add_argument("--name", help="display name for --add (default: the binary name)")
    p.add_argument("--arg", action="append", metavar="ARG",
                   help="argument to pass the app (repeatable), e.g. --arg /vault")
    p.add_argument("--key", help="explicit config key for --add")
    p.set_defaults(func=main)
