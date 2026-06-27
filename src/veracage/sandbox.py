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
        # Curated /etc instead of a wholesale `--ro-bind /etc /etc`: expose
        # only what apps need to start and render (dynamic linker, fontconfig,
        # timezone, NSS for getpwuid, machine-id for Qt/D-Bus, system XDG
        # config, TLS trust store). Keeps host network/VPN/mail/kerberos
        # configs and the rest of /etc out of a possibly-hostile viewer.
        # `-try` so a path absent on some distro doesn't abort the sandbox.
        "--ro-bind-try", "/etc/ld.so.cache", "/etc/ld.so.cache",
        "--ro-bind-try", "/etc/ld.so.conf.d", "/etc/ld.so.conf.d",
        "--ro-bind-try", "/etc/alternatives", "/etc/alternatives",
        "--ro-bind-try", "/etc/fonts", "/etc/fonts",
        "--ro-bind-try", "/etc/localtime", "/etc/localtime",
        "--ro-bind-try", "/etc/machine-id", "/etc/machine-id",
        "--ro-bind-try", "/etc/passwd", "/etc/passwd",
        "--ro-bind-try", "/etc/group", "/etc/group",
        "--ro-bind-try", "/etc/nsswitch.conf", "/etc/nsswitch.conf",
        "--ro-bind-try", "/etc/xdg", "/etc/xdg",
        "--ro-bind-try", "/etc/ca-certificates", "/etc/ca-certificates",
        "--ro-bind-try", "/etc/ca-certificates.conf", "/etc/ca-certificates.conf",
        "--ro-bind-try", "/etc/ssl", "/etc/ssl",
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
