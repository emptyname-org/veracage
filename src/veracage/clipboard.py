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


def _pipe(src_socket: str, dst_socket: str, runtime_dir: str) -> int:
    """Read clipboard from `src_socket`, write it to `dst_socket`."""
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
    copy.wait()
    paste.wait()
    return copy.returncode


def push_host_to_sandbox(weston_socket: Path,
                         host_display: str = "wayland-0") -> int:
    """Read host clipboard, write into the nested compositor's clipboard."""
    return _pipe(
        src_socket=host_display,
        dst_socket=weston_socket.name,
        runtime_dir=str(weston_socket.parent),
    )


def pull_sandbox_to_host(weston_socket: Path,
                         host_display: str = "wayland-0") -> int:
    """Read nested compositor clipboard, write into host clipboard."""
    return _pipe(
        src_socket=weston_socket.name,
        dst_socket=host_display,
        runtime_dir=str(weston_socket.parent),
    )
