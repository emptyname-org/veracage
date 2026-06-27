"""File transfer between host and sandbox via staging dirs in the vault.

Layout inside the decrypted vault:

    /vault/.veracage/
        in/      ← host writes here, sandbox sees imported files
        out/     ← sandbox writes here, host pulls files out

Neither side has filesystem access to the other's namespace. The host
agent (running in the same mount NS as Weston) and the sandboxed apps
both see the vault, so the staging dirs are the single channel.
"""
from __future__ import annotations

import shutil
from pathlib import Path

DIR_NAME = ".veracage"
IN = "in"
OUT = "out"


def staging_dirs(mountpoint: str | Path) -> tuple[Path, Path]:
    base = Path(mountpoint) / DIR_NAME
    return base / IN, base / OUT


def ensure_staging(mountpoint: str | Path) -> tuple[Path, Path]:
    in_dir, out_dir = staging_dirs(mountpoint)
    for d in (in_dir, out_dir):
        d.mkdir(parents=True, exist_ok=True)
        d.chmod(0o700)
    return in_dir, out_dir


def import_file(host_src: str | Path, mountpoint: str | Path) -> Path:
    """Copy `host_src` into the inbox; return the destination path."""
    src = Path(host_src)
    if not src.is_file():
        raise FileNotFoundError(str(host_src))
    in_dir, _ = ensure_staging(mountpoint)
    dst = unique_path(in_dir / src.name)
    shutil.copy2(src, dst)
    dst.chmod(0o600)
    return dst


def export_file(out_path: str | Path, host_dst: str | Path) -> Path:
    """Move a file from the outbox to a host-side path."""
    src = Path(out_path)
    dst = Path(host_dst)
    shutil.move(src, dst)
    return dst


def list_outbox(mountpoint: str | Path) -> list[Path]:
    _, out_dir = staging_dirs(mountpoint)
    if not out_dir.is_dir():
        return []
    return sorted(p for p in out_dir.iterdir() if p.is_file())


def unique_path(path: Path) -> Path:
    """Return `path` if free, else `path (1)`, `path (2)`, …"""
    if not path.exists():
        return path
    stem, suffix = path.stem, path.suffix
    n = 1
    while True:
        cand = path.with_name(f"{stem} ({n}){suffix}")
        if not cand.exists():
            return cand
        n += 1
