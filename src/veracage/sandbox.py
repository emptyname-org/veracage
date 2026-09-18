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
        # /etc/alternatives/x-cursor-theme -> /etc/X11/cursors/<theme>.theme.
        # Both hops must be present: with a dangling symlink no theme loads at all
        # and an app falls back to its own built-in bitmaps, which have an arrow and
        # an I-beam but no resize shapes.
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
        # Env - start from EMPTY (`--clearenv`) so the app does NOT inherit the
        # leader's environment (host DISPLAY, session tokens, auth sockets, etc.);
        # set only what it needs below. PATH is required for bwrap to resolve a bare
        # `app.exec`; locale and the cursor theme come from an allowlist below.
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
        # Load the KDE platform theme, which is what actually applies the
        # seeded kdeglobals (font, palette, widget style, icons). Without it Qt
        # uses its generic theme and ignores the file: measured with the same
        # kdeglobals, `fusion` / DejaVu 12pt / #efefef window without it, and
        # `breeze` / Noto Sans 16pt / #2a2e32 with it. Only Frameworks
        # components that read the colours themselves followed, which is why a
        # dark session used to come out with light widgets on a dark backdrop.
        # The plugin is a host Qt plugin like every other one the app loads
        # from the read-only /usr, and it needs no session bus.
        "--setenv", "QT_QPA_PLATFORMTHEME", "kde",
        "--chdir", "/vaults",
    ]
    # Preserve locale and the cursor theme (an explicit allowlist, not blanket
    # inheritance) so dates, numbers, fonts and the pointer match the host session;
    # everything else stays cleared. XCURSOR_SIZE matters: without it an app picks
    # the theme's next nominal size up, drawing larger cursors than the host.
    for var in ("LANG", "LANGUAGE", "LC_ALL", "LC_CTYPE", "LC_TIME", "LC_NUMERIC",
                "XCURSOR_THEME", "XCURSOR_SIZE"):
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
    # host session: it is a bus per app, not the host's bus. REAPER sits inside it,
    # so the bus lasts as long as the app's processes do.
    argv += ["--", *dbus_wrapper(), *REAPER, app.exec]
    return argv


# The reaper's process name (`/proc/<pid>/comm`, 15 bytes at most). The helper
# lists it as sandbox plumbing, so "Close <app> to continue." names the app and
# not a bare python3 that the human cannot recognise.
REAPER_NAME = "veracage-reaper"

# Runs the app and waits for every process it leaves behind, not just the first.
# bwrap ends the sandbox, and SIGKILLs whatever is still in it, when its first
# process exits: dbus-run-session, which exits with the app's first process. Kate
# 25.04 forks itself into the background at start (daemon(3) unless given
# --block), so that first process exits 0 at once and took Kate down with it. The
# same exit kills anything an app started that outlives it (measured with a
# background child of the first process), a Kate opened from Dolphin once Dolphin
# closes among them. As PR_SET_CHILD_SUBREAPER, this process becomes the parent
# of every orphan below it and exits, with the first process's status, once the
# last one has. Bus-activated services are children of the bus, not of the app,
# so they do not hold the sandbox open.
_REAPER_CODE = f"""\
import ctypes, os, sys
libc = ctypes.CDLL(None)
libc.prctl(36, 1)  # PR_SET_CHILD_SUBREAPER
libc.prctl(15, {REAPER_NAME.encode()!r})  # PR_SET_NAME
app = os.fork()
if app == 0:
    try:
        os.execvp(sys.argv[1], sys.argv[1:])
    except OSError as e:
        print(sys.argv[1] + ":", e, file=sys.stderr)
    os._exit(127)
status = 0
while True:
    try:
        pid, wstatus = os.wait()
    except ChildProcessError:
        break
    if pid == app:
        status = os.waitstatus_to_exitcode(wstatus)
sys.exit(status if status >= 0 else 128 - status)
"""

# `-I`: the working directory and HOME are the workspace, so neither may add code
# from a volume to the interpreter: without it `import ctypes` would find a
# `ctypes.py` in /vaults first, and a user site-packages or a PYTHON* variable
# could do the same. python3 is the host's, read-only under /usr, and Veracage
# itself depends on it.
REAPER = ["python3", "-I", "-c", _REAPER_CODE]

def dbus_wrapper() -> list[str]:
    """`dbus-run-session --` when it is installed, else nothing. The sandbox sees
    the host's read-only /usr, so testing the host path is testing the sandbox's.
    Without it apps still run, just with the 25s D-Bus stall."""
    return ["dbus-run-session", "--"] if shutil.which("dbus-run-session") else []
