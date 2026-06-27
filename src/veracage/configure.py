"""`veracage configure` — choose which detected apps are enabled.

Three modes:

  * Qt window  — default if PySide6 is importable and stdin is a tty/no
                 special flags
  * Auto       — enable everything detected, no UI
  * Text       — interactive y/N prompts on the terminal
"""
from __future__ import annotations

import argparse
import sys

from . import config
from .apps import KNOWN_APPS, CATEGORY_LABELS, CATEGORY_ORDER, App, detected


# ---------------------------------------------------------------- helpers ---

def _by_category(apps: dict[str, App]) -> dict[str, list[App]]:
    out: dict[str, list[App]] = {c: [] for c in CATEGORY_ORDER}
    for a in apps.values():
        out.setdefault(a.category, []).append(a)
    return out


def _save_and_report(cfg: config.Config) -> int:
    p = config.save(cfg)
    n = len(cfg.apps)
    print(f"veracage: wrote {p} ({n} app{'s' if n != 1 else ''} enabled)")
    return 0


# ------------------------------------------------------------- modes -------

def _auto(_args: argparse.Namespace) -> int:
    found = detected()
    cfg = config.load()
    cfg.apps = found
    return _save_and_report(cfg)


def _list(_args: argparse.Namespace) -> int:
    cfg = config.load()
    found = detected()
    enabled = set(cfg.apps)
    print(f"Catalog: {len(KNOWN_APPS)} known, {len(found)} installed,"
          f" {len(enabled)} enabled.\n")
    for cat in CATEGORY_ORDER:
        rows = [a for a in KNOWN_APPS.values() if a.category == cat]
        if not rows:
            continue
        print(f"== {CATEGORY_LABELS[cat]} ==")
        for a in rows:
            mark = "[x]" if a.key in enabled else (
                "[ ]" if a.key in found else "[-]")
            tag = "" if a.key in found else "  (not installed)"
            print(f"  {mark} {a.key:<14} {a.name}{tag}")
        print()
    print("Legend: [x] enabled  [ ] installed but disabled  [-] not installed")
    return 0


def _text(_args: argparse.Namespace) -> int:
    found = detected()
    cfg = config.load()
    enabled = set(cfg.apps)
    print(f"Found {len(found)} installed app(s) from the catalog.")
    print("Enable each? (y/N, Enter = keep current)\n")
    new_apps: dict[str, App] = {}
    for cat in CATEGORY_ORDER:
        rows = [a for a in found.values() if a.category == cat]
        if not rows:
            continue
        print(f"-- {CATEGORY_LABELS[cat]} --")
        for a in rows:
            cur = "y" if a.key in enabled else "n"
            ans = input(f"  {a.name:<24} [{cur}] > ").strip().lower()
            if not ans:
                ans = cur
            if ans.startswith("y"):
                new_apps[a.key] = a
    cfg.apps = new_apps
    return _save_and_report(cfg)


def _qt(_args: argparse.Namespace) -> int:
    try:
        from PySide6.QtWidgets import (
            QApplication, QCheckBox, QDialog, QDialogButtonBox,
            QGroupBox, QLabel, QScrollArea, QVBoxLayout, QWidget,
        )
    except ImportError:
        print("veracage: PySide6 not installed — falling back to text mode.\n"
              "(install with: sudo apt install python3-pyside6.qtwidgets)\n",
              file=sys.stderr)
        return _text(_args)

    found = detected()
    cfg = config.load()
    enabled = set(cfg.apps)

    app = QApplication.instance() or QApplication([])
    dlg = QDialog()
    dlg.setWindowTitle("Veracage — choose sandbox apps")
    dlg.resize(520, 600)

    root = QVBoxLayout(dlg)
    intro = QLabel(
        f"Detected {len(found)} installed app(s). "
        "Tick the ones to make available inside the sandbox."
    )
    intro.setWordWrap(True)
    root.addWidget(intro)

    inner = QWidget()
    inner_l = QVBoxLayout(inner)
    boxes: dict[str, QCheckBox] = {}
    for cat in CATEGORY_ORDER:
        rows = [a for a in found.values() if a.category == cat]
        if not rows:
            continue
        gb = QGroupBox(CATEGORY_LABELS[cat])
        gl = QVBoxLayout(gb)
        for a in rows:
            cb = QCheckBox(f"{a.name}  —  {a.exec}")
            cb.setChecked(a.key in enabled or not enabled)  # default-on if no prior config
            if a.note:
                cb.setToolTip(a.note)
            boxes[a.key] = cb
            gl.addWidget(cb)
        inner_l.addWidget(gb)
    inner_l.addStretch()

    scroll = QScrollArea()
    scroll.setWidgetResizable(True)
    scroll.setWidget(inner)
    root.addWidget(scroll)

    buttons = QDialogButtonBox(
        QDialogButtonBox.StandardButton.Save
        | QDialogButtonBox.StandardButton.Cancel
    )
    buttons.accepted.connect(dlg.accept)
    buttons.rejected.connect(dlg.reject)
    root.addWidget(buttons)

    if dlg.exec() != QDialog.DialogCode.Accepted:
        print("veracage: configuration cancelled.")
        return 1

    cfg.apps = {k: found[k] for k, cb in boxes.items() if cb.isChecked()}
    return _save_and_report(cfg)


# -------------------------------------------------------------- entry ------

def main(args: argparse.Namespace) -> int:
    if args.auto:
        return _auto(args)
    if args.list:
        return _list(args)
    if args.text:
        return _text(args)
    return _qt(args)


def add_subparser(sub: argparse._SubParsersAction) -> None:
    p = sub.add_parser(
        "configure",
        help="choose which sandbox apps are enabled",
    )
    g = p.add_mutually_exclusive_group()
    g.add_argument("--auto", action="store_true",
                   help="enable all detected apps; no UI")
    g.add_argument("--list", action="store_true",
                   help="print catalog state, no changes")
    g.add_argument("--text", action="store_true",
                   help="text-mode prompt instead of Qt window")
    p.set_defaults(func=main)
