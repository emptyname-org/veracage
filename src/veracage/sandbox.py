"""Build the bwrap command line for a given app + mountpoint + Wayland socket."""
from __future__ import annotations

import os
from pathlib import Path

from .apps import App


def bwrap_command(workspace: str, app: App, wayland_socket: Path,
                  gpu: bool = False, places_fd: int | None = None,
                  exchange: str | None = None) -> list[str]:
    """Construct argv for `bwrap`.

    `workspace` is the shared-workspace root (the leader's tmpfs holding every
    open volume at `<workspace>/<label>`); it is recursively bound at `/vaults`,
    so a single app sees ALL open volumes side by side (`/vaults/<label>`) — the
    basis for cross-volume drag-and-drop. Apps see the volumes mounted **at launch
    time** (their mount namespace is fixed then); open the volumes first, then
    launch.

    `wayland_socket` is the host-visible path to the (nested) Wayland socket
    the sandboxed app should connect to. We mount only that single socket
    into the sandbox at /run/user/$UID/wayland-0; the rest of the user's
    runtime dir is hidden behind a tmpfs.

    `gpu` opts into /dev/dri passthrough (per-volume `gpu = true`). Off by
    default — Okular/Kate render fine on CPU and a shared GPU is a documented
    side channel.
    """
    uid = os.getuid()
    argv = [
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
        "--bind", workspace, "/vaults",
        # Hide the host runtime dir behind a tmpfs, then bind only the
        # (nested) Wayland socket. The sandbox can't see anything else
        # the user session left in there (dbus, pulseaudio, host wayland).
        # 0700: XDG_RUNTIME_DIR must not be group/world-readable (Qt refuses
        # it otherwise) and the wayland socket lives inside it.
        "--perms", "0700", "--tmpfs", f"/run/user/{uid}",
        "--bind", str(wayland_socket), f"/run/user/{uid}/wayland-0",
        # HOME is the workspace ROOT so open/save dialogs default to the volumes
        # (each a folder under /vaults); the file manager also opens here, showing
        # every volume side by side. App config/cache/data go to an ephemeral tmpfs
        # (via XDG_*), so nothing app-generated is written into any volume. Files
        # the user saves under /vaults/<label> persist; a stray save to ~ itself
        # (the tmpfs workspace root) would not — but that is not a volume.
        "--perms", "0700", "--tmpfs", "/xdg",
        # Env — start from EMPTY (`--clearenv`) so the possibly-hostile app does
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
        "--chdir", "/vaults",
    ]
    # Preserve locale (an explicit allowlist, not blanket inheritance) so dates,
    # numbers and fonts render correctly; everything else stays cleared.
    for var in ("LANG", "LANGUAGE", "LC_ALL", "LC_CTYPE", "LC_TIME", "LC_NUMERIC"):
        val = os.environ.get(var)
        if val:
            argv += ["--setenv", var, val]
    if places_fd is not None:
        # Show the vault as a named place in the sandbox file manager: KDE reads
        # $XDG_DATA_HOME/user-places.xbel (XDG_DATA_HOME=/xdg/data). Use --file
        # (not --ro-bind): it writes the seed into a normal WRITABLE file in the
        # /xdg tmpfs, so Dolphin can rewrite it (it merges its default places on
        # startup) instead of erroring "not writable". `places_fd` carries the
        # seed content; it's passed to bwrap via pass_fds.
        argv += ["--file", str(places_fd), "/xdg/data/user-places.xbel"]
    if gpu:
        # Comes after `--dev /dev`, so it binds into the fresh devtmpfs.
        argv += ["--dev-bind-try", "/dev/dri", "/dev/dri"]
    if exchange is not None:
        # The idmapped host<->vault shared folder (helper mounted it in this NS,
        # presented as veracage-owned). Bind it at /exchange — a top-level path,
        # NOT under /vaults, so the "everything in HOME is encrypted" invariant
        # holds. The underlying mount already carries nosuid,nodev,noexec.
        argv += ["--bind", exchange, "/exchange"]
    argv += ["--", app.exec, *app.args]
    return argv
