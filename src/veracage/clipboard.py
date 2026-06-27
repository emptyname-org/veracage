"""Clipboard bridge between host and sandbox compositors.

`wl-paste | wl-copy` across two Wayland sockets. Both compositors live
in the same `$XDG_RUNTIME_DIR`, so the only thing that selects between
them is `WAYLAND_DISPLAY`.

We deliberately don't keep a long-running bridge process. Each transfer
is one-shot, triggered by the user (tray click, future hotkey).
"""
from __future__ import annotations

import os
import shutil
import subprocess
from pathlib import Path


class WlClipboardMissing(RuntimeError):
    pass


class ClipboardError(RuntimeError):
    pass


CLIPBOARD_TIMEOUT_S = 10.0


def _check_tools() -> None:
    if shutil.which("wl-paste") is None or shutil.which("wl-copy") is None:
        raise WlClipboardMissing(
            "wl-clipboard not installed (apt install wl-clipboard)"
        )


def _build_env(socket_name: str, runtime_dir: str) -> dict[str, str]:
    return {
        **os.environ,
        "WAYLAND_DISPLAY": socket_name,
        "XDG_RUNTIME_DIR": runtime_dir,
    }


def _host_display() -> str:
    """The host compositor's WAYLAND_DISPLAY (falls back to wayland-0)."""
    return os.environ.get("WAYLAND_DISPLAY") or "wayland-0"


def _pipe(src_socket: str, dst_socket: str, runtime_dir: str) -> int:
    """Read clipboard from `src_socket`, write it to `dst_socket`.

    Raises ClipboardError on tool failure or timeout, so the caller surfaces
    it instead of reporting a silent success.
    """
    _check_tools()
    paste = subprocess.Popen(
        ["wl-paste", "--no-newline"],
        env=_build_env(src_socket, runtime_dir),
        stdout=subprocess.PIPE,
    )
    copy = subprocess.Popen(
        ["wl-copy"],
        env=_build_env(dst_socket, runtime_dir),
        stdin=paste.stdout,
    )
    assert paste.stdout is not None
    paste.stdout.close()  # so wl-copy gets EOF when paste exits
    try:
        copy_rc = copy.wait(timeout=CLIPBOARD_TIMEOUT_S)
        paste_rc = paste.wait(timeout=CLIPBOARD_TIMEOUT_S)
    except subprocess.TimeoutExpired as e:
        copy.kill()
        paste.kill()
        raise ClipboardError("clipboard transfer timed out") from e
    if paste_rc != 0:
        raise ClipboardError(f"reading source clipboard failed (rc={paste_rc})")
    if copy_rc != 0:
        raise ClipboardError(f"writing destination clipboard failed (rc={copy_rc})")
    return 0


def push_host_to_sandbox(weston_socket: Path,
                         host_display: str | None = None) -> int:
    """Read host clipboard, write into the nested compositor's clipboard."""
    return _pipe(
        src_socket=host_display or _host_display(),
        dst_socket=weston_socket.name,
        runtime_dir=str(weston_socket.parent),
    )


def pull_sandbox_to_host(weston_socket: Path,
                         host_display: str | None = None) -> int:
    """Read nested compositor clipboard, write into host clipboard."""
    return _pipe(
        src_socket=weston_socket.name,
        dst_socket=host_display or _host_display(),
        runtime_dir=str(weston_socket.parent),
    )
