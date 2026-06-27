"""Build the bwrap command line for a given app + mountpoint + Wayland socket."""
from __future__ import annotations

import os
from pathlib import Path

from .apps import App


def bwrap_command(mountpoint: str, app: App, wayland_socket: Path) -> list[str]:
    """Construct argv for `bwrap`.

    `wayland_socket` is the host-visible path to the (nested) Wayland socket
    the sandboxed app should connect to. We mount only that single socket
    into the sandbox at /run/user/$UID/wayland-0; the rest of the user's
    runtime dir is hidden behind a tmpfs.
    """
    uid = os.getuid()
    return [
        "bwrap",
        # Namespaces — full isolation
        "--unshare-pid", "--unshare-uts", "--unshare-ipc",
        "--unshare-cgroup-try",
        "--unshare-net",
        # Safety
        "--die-with-parent", "--new-session",
        # Filesystem
        "--proc", "/proc",
        "--dev", "/dev",
        "--tmpfs", "/tmp",
        "--ro-bind", "/usr", "/usr",
        "--ro-bind", "/etc", "/etc",
        "--symlink", "usr/lib",   "/lib",
        "--symlink", "usr/lib64", "/lib64",
        "--symlink", "usr/bin",   "/bin",
        "--symlink", "usr/sbin",  "/sbin",
        "--bind", mountpoint, "/vault",
        # Hide the host runtime dir behind a tmpfs, then bind only the
        # (nested) Wayland socket. The sandbox can't see anything else
        # the user session left in there (dbus, pulseaudio, host wayland).
        "--tmpfs", f"/run/user/{uid}",
        "--bind", str(wayland_socket), f"/run/user/{uid}/wayland-0",
        # Env
        "--setenv", "HOME", "/vault",
        "--setenv", "XDG_RUNTIME_DIR", f"/run/user/{uid}",
        "--setenv", "XDG_CONFIG_HOME", "/vault/.config",
        "--setenv", "XDG_DATA_HOME",   "/vault/.local/share",
        "--setenv", "XDG_CACHE_HOME",  "/vault/.cache",
        "--setenv", "WAYLAND_DISPLAY", "wayland-0",
        "--chdir", "/vault",
        "--",
        app.exec, *app.args,
    ]
