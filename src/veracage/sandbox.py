"""Build the bwrap command line for a given app + mountpoint + Wayland socket."""
from __future__ import annotations

import os
import shutil
from collections.abc import Sequence
from pathlib import Path

from .apps import App


def bwrap_command(workspace: str, app: App, wayland_socket: Path,
                  seeds: Sequence[tuple[int, str]] | None = None,
                  exchange: str | None = None) -> list[str]:
    """Construct argv for `bwrap`.

    `workspace` is the shared-workspace root (the leader's tmpfs holding every
    open volume at `<workspace>/<label>`); it is recursively bound at `/vaults`,
    so a single app sees ALL open volumes side by side (`/vaults/<label>`) - the
    basis for cross-volume drag-and-drop. Apps see the volumes mounted **at launch
    time** (their mount namespace is fixed then); open the volumes first, then
    launch.

    `wayland_socket` is the host-visible path to the (nested) Wayland socket
    the sandboxed app should connect to. We mount only that single socket
    into the sandbox at /run/user/$UID/wayland-0; the rest of the user's
    runtime dir is hidden behind a tmpfs.

    `seeds` are (fd, destination) pairs copied into the sandbox as ordinary
    WRITABLE files via bwrap `--file` (apps such as Dolphin rewrite them on
    startup, so a read-only bind would error): the KDE Places seed and the
    default-app associations. The caller must also pass the fds to Popen's
    pass_fds.

    The host GPU is always passed through (`/dev/dri` plus the /sys device
    metadata Mesa needs to pick its hardware driver): without it every app
    falls back to llvmpipe software rendering and burns CPU. The isolation is
    one-directional (keep the host out of the volume), so the GPU is not a
    boundary the sandbox needs to withhold.
    """
    uid = os.getuid()
    argv = [
        "bwrap",
        # Namespaces - full isolation
        "--unshare-pid", "--unshare-uts", "--unshare-ipc",
        "--unshare-cgroup-try",
        "--unshare-net",
        # Safety
        "--die-with-parent", "--new-session",
        # Filesystem
        "--proc", "/proc",
        "--dev", "/dev",
        # GPU: the render node (into the fresh devtmpfs, so this must come after
        # `--dev /dev`) plus the /sys device metadata Mesa's loader reads to
        # identify the hardware and pick its driver (verified: without these it
        # logs "failed to retrieve device information" and falls back to
        # swrast/llvmpipe). `-try` so a GPU-less host still works (software).
        "--dev-bind-try", "/dev/dri", "/dev/dri",
        "--ro-bind-try", "/sys/dev/char", "/sys/dev/char",
        "--ro-bind-try", "/sys/devices", "/sys/devices",
        "--tmpfs", "/tmp",
        "--ro-bind", "/usr", "/usr",
        # Curated /etc instead of a wholesale `--ro-bind /etc /etc`: expose
        # only what apps need to start and render (dynamic linker, fontconfig,
        # timezone, NSS for getpwuid, machine-id for Qt/D-Bus, system XDG
        # config, TLS trust store). Keeps host network/VPN/mail/kerberos
        # configs and the rest of /etc out of a possibly-hostile viewer.
        # `-try` so a path absent on some distro doesn't abort the sandbox.
        # The host's prebuilt font cache. `/etc/fonts/fonts.conf` names
        # `/var/cache/fontconfig` first, and without it fontconfig rescans every
        # font on each app start into the throwaway /xdg tmpfs: measured at ~1s
        # per launch. Read-only, and it holds nothing but font metadata.
        "--ro-bind-try", "/var/cache/fontconfig", "/var/cache/fontconfig",
        "--ro-bind-try", "/etc/ld.so.cache", "/etc/ld.so.cache",
        "--ro-bind-try", "/etc/ld.so.conf.d", "/etc/ld.so.conf.d",
        "--ro-bind-try", "/etc/alternatives", "/etc/alternatives",
        # The cursor theme. Toolkits load cursors themselves from the theme named
        # "default", which resolves /usr/share/icons/default/index.theme ->
        # /etc/alternatives/x-cursor-theme -> /etc/X11/cursors/<theme>.theme. With
        # that last hop missing the symlink dangles, no theme loads, and an app
        # falls back to its own built-in bitmaps: those cover the arrow and the
        # I-beam but NOT the resize shapes, so a window edge gave no resize cursor
        # (measured: 10x16 fallback bitmaps instead of the theme's 48x48).
        "--ro-bind-try", "/etc/X11/cursors", "/etc/X11/cursors",
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
        "--bind", workspace, "/vaults",
        # Hide the host runtime dir behind a tmpfs, then bind only the
        # (nested) Wayland socket. The sandbox can't see anything else
        # the user session left in there (dbus, pulseaudio, host wayland).
        # 0700: XDG_RUNTIME_DIR must not be group/world-readable (Qt refuses
        # it otherwise) and the wayland socket lives inside it.
        "--perms", "0700", "--tmpfs", f"/run/user/{uid}",
        "--bind", str(wayland_socket), f"/run/user/{uid}/wayland-0",
        # HOME is the workspace ROOT so open/save dialogs default to the volumes
        # (each a directory under /vaults); the file manager also opens here, showing
        # every volume side by side. App config/cache/data go to an ephemeral tmpfs
        # (via XDG_*), so nothing app-generated is written into any volume. Files
        # the user saves under /vaults/<label> persist; a stray save to ~ itself
        # (the tmpfs workspace root) would not - but that is not a volume.
        "--perms", "0700", "--tmpfs", "/xdg",
        # Env - start from EMPTY (`--clearenv`) so the possibly-hostile app does
        # NOT inherit the leader's environment (host DISPLAY, session tokens, auth
        # sockets, etc.); set only what it needs below. Inheriting was inert today
        # (--unshare-net kills the X11/abstract-socket paths) but left isolation
        # implicit; this makes it explicit. PATH is required for bwrap to resolve
        # a bare `app.exec`; locale is added from an allowlist further down.
        "--clearenv",
        "--setenv", "HOME", "/vaults",
        "--setenv", "PATH", "/usr/bin:/bin:/usr/local/bin",
        "--setenv", "XDG_RUNTIME_DIR", f"/run/user/{uid}",
        "--setenv", "XDG_CONFIG_HOME", "/xdg/config",
        "--setenv", "XDG_DATA_HOME",   "/xdg/data",
        "--setenv", "XDG_CACHE_HOME",  "/xdg/cache",
        "--setenv", "XDG_STATE_HOME",  "/xdg/state",
        "--setenv", "WAYLAND_DISPLAY", "wayland-0",
        "--setenv", "XDG_SESSION_TYPE", "wayland",
        # The sandbox runs as the veracage uid, whose passwd shell is nologin.
        # Terminals (Konsole) launch the login shell and exit immediately with
        # it, so hand them a real shell explicitly.
        "--setenv", "SHELL", "/bin/bash",
        "--chdir", "/vaults",
    ]
    # Preserve locale (an explicit allowlist, not blanket inheritance) so dates,
    # numbers and fonts render correctly; everything else stays cleared.
    for var in ("LANG", "LANGUAGE", "LC_ALL", "LC_CTYPE", "LC_TIME", "LC_NUMERIC"):
        val = os.environ.get(var)
        if val:
            argv += ["--setenv", var, val]
    for fd, dest in seeds or ():
        argv += ["--file", str(fd), dest]
    if exchange is not None:
        # The idmapped host<->vault shared directory (helper mounted it in this NS,
        # presented as veracage-owned). Bind it at /exchange - a top-level path,
        # NOT under /vaults, so the "everything in HOME is encrypted" invariant
        # holds. The underlying mount already carries nosuid,nodev,noexec.
        argv += ["--bind", exchange, "/exchange"]
    # A private D-Bus session bus inside the sandbox. Qt/KDE apps expect one:
    # without it they block on the D-Bus connect timeout (measured: exactly 25.0s
    # of an app doing nothing, then "Not connected to D-Bus server" from KDE's
    # Solid backend) before carrying on degraded. `dbus-run-session` starts a bus
    # for this app alone and tears it down when it exits. The bus socket lives in
    # the sandbox's own /tmp and its IPC namespace, so this exposes nothing of the
    # host session: it is a bus per app, not the host's bus.
    argv += ["--", *dbus_wrapper(), app.exec]
    return argv


def dbus_wrapper() -> list[str]:
    """`dbus-run-session --` when it is installed, else nothing. The sandbox sees
    the host's read-only /usr, so testing the host path is testing the sandbox's.
    Without it apps still run, just with the 25s D-Bus stall."""
    return ["dbus-run-session", "--"] if shutil.which("dbus-run-session") else []
