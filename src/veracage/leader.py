"""Vault-side session leader: runs AS the vault uid (the helper dropped to it).

The Rust helper has already opened the volume, idmap-mounted it at MOUNTPOINT as
the vault uid (in a private mount NS), created the control listening socket, and
provisioned a vault-writable runtime dir. It passes those through the environment
and execs us:

  VERACAGE_CONTROL_FD     inherited control *listening* socket (we accept on it)
  VERACAGE_VAULT_RUNTIME  our XDG_RUNTIME_DIR (bwrap /run/user)

We launch apps in bwrap wired to the ONE persistent compositor's shared socket
(/run/veracage/rt/wl-vc, brought up separately by the helper. We do not spawn
it). The leader is a plain executor: it runs the command the human hands it,
sandboxed, and outlives no compositor of its own. The
security property (*external processes can't read the vault*) comes from the
idmap (the vault is owned by a uid no one else has) + the mount NS (hidden) +
bwrap (apps have no net/host-FS, so they can't exfiltrate). The app allowlist is
UX on the human side (which apps to offer); it is NOT a vault-side restriction,
so the leader does not load config or police what it's told to run.
"""
from __future__ import annotations

import contextlib
import html
import json
import os
import selectors
import signal
import socket
import subprocess
import sys
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path
from xml.sax.saxutils import quoteattr

from .apps import App, is_file_manager
from .sandbox import bwrap_command
from .wayland import COMPOSITOR_RUNTIME, COMPOSITOR_SOCKET, compositor_is_up

_MAX_REQUEST_BYTES = 64 * 1024  # control requests are tiny; cap to bound memory

# Wall clock at import, so debug lines carry an elapsed time that lines up with
# the compositor's own log rather than an absolute clock.
_STARTED = time.monotonic()


def _debug(state, msg: str) -> None:
    """One timing line to stderr (the session unit's journal) when debug logging
    is on: `veracage[+12.34s] <msg>`. Read it with
    `journalctl --user -u 'veracage-*' -f`. See docs/debugging.md."""
    if state.debug:
        print(f"veracage[+{time.monotonic() - _STARTED:6.2f}s] {msg}",
              file=sys.stderr, flush=True)

# The shared-workspace root (must match WORKSPACE in helper-rs/src/main.rs): the
# leader's private-NS tmpfs holding every open volume at <WORKSPACE>/<label>. The
# sandbox binds this whole tree at /vaults, so one app sees all volumes.
WORKSPACE = Path("/run/veracage/vaults")

# Human-published default-app associations (config.publish_apps writes it, the
# pub dir is root-created and human-owned): seeded into each sandbox as
# $XDG_CONFIG_HOME/mimeapps.list so its file managers open files with the
# human's chosen apps. Human-trust UX data, same as the enabled-app list.
MIMEAPPS_SEED = Path("/run/veracage/pub/mimeapps.list")


def scan_volumes(root: Path | None = None) -> list[str]:
    """The open volumes' labels: the directory names under the workspace root
    (the helper already sanitised them to a single safe path component). Dot
    entries are skipped (defense in depth; nothing dot-named is expected under
    the workspace. The exchange mount lives OUTSIDE it). Sorted."""
    if root is None:
        root = WORKSPACE   # resolved at call time (tests monkeypatch WORKSPACE)
    try:
        names = [e.name for e in os.scandir(root)
                 if e.is_dir(follow_symlinks=False) and not e.name.startswith(".")]
    except OSError:
        return []
    return sorted(names)


# --------------------------------------------------------- leader state ----

@dataclass
class _LeaderState:
    mountpoint: str
    wl_socket: Path | None = None       # shared compositor socket, once verified
    children: dict[int, tuple[str, float]] = field(default_factory=dict)  # pid -> (label, launch monotonic)
    closing: bool = False
    app_specs: list = field(default_factory=list)  # enabled apps, for the toolbar
    places_file: Path | None = None  # seeded KDE Places (vault under its label)
    volume_label: str = "Volume"      # joined labels for the window title
    volumes: list = field(default_factory=list)  # per-volume labels (per-vol close)
    exchange: str | None = None      # idmapped host<->vault shared dir -> /exchange
    debug: bool = False              # verbose timing logs (config debug / --debug)


# ------------------------------------------------------------- protocol ----
#
# One line of JSON per request, one per reply. Status/lifecycle only: no launch
# or file transfer (this socket is human-owned; any same-uid process can reach it).
#   {"cmd": "ping"}                       -> {"ok": true, "uid": <vault uid>, ...}
#   {"cmd": "list"}                       -> {"ok": true, "apps": [{"pid","app"}]}
#   {"cmd": "close"}                      -> {"ok": true}
#   {"cmd": "set-apps", "apps": [...]}    -> {"ok": true}   (replace enabled list)

def _handle_request(state: _LeaderState, req: dict) -> dict:
    if not isinstance(req, dict):
        return {"ok": False, "error": "request must be a JSON object"}
    cmd = req.get("cmd")

    if cmd == "ping":
        return {"ok": True, "uid": os.getuid(), "mountpoint": state.mountpoint}

    if cmd == "list":
        # `volumes` + `bootstrap_open` let the CLI's already-open probe tell "the
        # session is alive" apart from "THIS vault is still mounted": after a
        # per-volume close of the bootstrap volume, this socket keeps serving for
        # the rest of the session, but the vault itself may be reopened. Scanned
        # fresh (not state.volumes) so the reply can't lag the 1s rescan loop.
        cur = scan_volumes()
        return {"ok": True,
                "apps": [{"pid": p, "app": lbl} for p, (lbl, _t) in state.children.items()],
                "volumes": cur,
                "bootstrap_open": Path(state.mountpoint).name in cur}

    if cmd == "close":
        state.closing = True
        return {"ok": True}

    if cmd == "set-apps":
        # Replace the enabled-app list live (Configure apps saved mid-session).
        # This is UX data with the same trust as config.toml, its source: both
        # are writable by the human uid, and the list only defines what a REAL
        # compositor menu click will launch (by index, over the veracage-owned
        # app socket). A peer here still cannot CAUSE a launch or read vault
        # data, so the pen-test property (no exec/import/export) holds.
        #
        # Defence in depth against a same-uid confused-deputy that swaps the
        # list before a click: every `exec` must be a bare command (no
        # whitespace, no shell metacharacters, no control chars). bwrap already
        # runs it as a SINGLE argv element under `--` (no shell, see
        # test_bwrap_runs_exec_as_single_argv), so a swapped entry can at most
        # launch an installed `/usr` binary bare, never inject args or a shell.
        apps = req.get("apps")
        if not isinstance(apps, list) or len(apps) > 64:
            return {"ok": False, "error": "apps must be a list of at most 64 entries"}
        specs = []
        for a in apps:
            exe = a.get("exec") if isinstance(a, dict) else None
            if not isinstance(exe, str) or not _exec_ok(exe):
                return {"ok": False, "error": "each app needs a bare exec command"}
            name = a.get("name")
            if name is not None and (not isinstance(name, str) or len(name) > 128):
                return {"ok": False, "error": "bad app name"}
            specs.append({"name": name or exe, "exec": exe})
        state.app_specs = specs
        _write_apps_file(state)
        return {"ok": True}

    # There is deliberately NO exec / import / export / outbox here. The control
    # socket is human-owned, so ANY process running as the human uid can connect
    # to it. If it could make the leader run a command in the vault (exec) or
    # hand vault files back out (export), a same-uid attacker would have a full
    # vault-exfiltration primitive (it did, see the pen test). Launching happens
    # only over the veracage-owned app socket, by index into the human's own
    # enabled list (`_accept_app_launch`), which other uids cannot reach.
    return {"ok": False, "error": f"unknown cmd: {cmd}"}


def _launch_app(state: _LeaderState, spec) -> dict:
    """Launch the command in `spec` (an {exec, name} dict; a legacy 'args' key is
    ignored) in bwrap against the compositor; track its pid. `spec` only ever
    comes from the leader's OWN enabled list, `first_app` (set at open) or
    `state.app_specs[idx]` on a toolbar click, never from a control-socket peer,
    so a same-uid caller can't make it run an arbitrary command. bwrap is what
    confines whatever does run."""
    if state.wl_socket is None:
        return {"ok": False, "error": "compositor not ready"}
    if not isinstance(spec, dict):
        return {"ok": False, "error": "missing app spec"}
    command = spec.get("exec")
    if not isinstance(command, str) or not command:
        return {"ok": False, "error": "app spec needs a non-empty 'exec'"}
    name_val = spec.get("name")
    label = name_val if isinstance(name_val, str) else command

    app = App(key=label, name=label, exec=command)
    # Open the seed files and hand bwrap their fds: `--file` writes a WRITABLE
    # copy into the sandbox tmpfs (apps rewrite these on startup, so a
    # read-only bind would error). A missing seed is skipped.
    seeds: list[tuple[int, str]] = []
    for src, dest in ((state.places_file, "/xdg/data/user-places.xbel"),
                      (MIMEAPPS_SEED, "/xdg/config/mimeapps.list")):
        if src is None:
            continue
        try:
            seeds.append((os.open(src, os.O_RDONLY), dest))
        except OSError:
            continue
    t0 = time.monotonic()
    try:
        # Bind the whole workspace (/vaults tree), not one volume: the app sees
        # every volume open at launch time (docs/shared-workspace.md).
        argv = bwrap_command(str(WORKSPACE), app, state.wl_socket,
                             seeds, state.exchange)
        _debug(state, f"launch {label!r}: exec={command} seeds={len(seeds)} "
                      f"argv={len(argv)} words")
        # Detach the app's stdio. Inheriting the leader's stdin/out/err hands a
        # chatty viewer the session's terminal/journal: Qt/KF apps print the paths
        # of files they open on stderr, which would persist unencrypted in the
        # user journal, readable by any same-uid process after the vault closes
        # (an accidental-leak channel in the threat model), and hands the app an
        # fd to the human's pty. Nothing vault-side needs the app's stdio.
        # The app's stdio is normally discarded: a chatty viewer prints the paths
        # of files it opens, which would persist unencrypted in the journal after
        # the volume closes (an accidental-leak channel in the threat model).
        # Debug logging deliberately lifts that, because an app's own warnings are
        # exactly what a launch problem looks like - it is opt-in, and the trade
        # is documented in docs/debugging.md.
        app_out = None if state.debug else subprocess.DEVNULL
        proc = subprocess.Popen(
            argv,
            stdin=subprocess.DEVNULL,
            stdout=app_out,
            stderr=app_out,
            pass_fds=[fd for fd, _ in seeds],
        )
    except FileNotFoundError as e:
        return {"ok": False, "error": f"missing dependency: {e.filename}"}
    except OSError as e:
        # Anything else the spawn can fail with (EMFILE/ENOMEM/EAGAIN under
        # load) is reported like any other launch failure. It must NOT escape:
        # the callers run inside the serve loop, whose unwind path terminates
        # every app in the session.
        return {"ok": False, "error": f"cannot launch {label}: {e}"}
    finally:
        for fd, _ in seeds:
            os.close(fd)
    # Track the launch time (monotonic) alongside the label. The reaper uses it to
    # tell an immediate failure (a GUI that needs X11 in this Wayland-only sandbox,
    # a crash, a missing in-sandbox dependency) from a normal quit, and report it.
    # The app's stdio is DEVNULL'd, so without this a launch that dies at once
    # leaves no trace. Done in the reaper (not here) so the serve loop never
    # blocks: the launch returns at once.
    state.children[proc.pid] = (label, time.monotonic())
    _debug(state, f"launch {label!r}: pid={proc.pid} spawned in "
                  f"{(time.monotonic() - t0) * 1000:.0f}ms")
    # An app takes a second or two to put its first window up, with nothing on
    # screen meanwhile: ask the compositor to show a progress note until the
    # window appears (it clears the note itself, see scan_status).
    _post_status(f"Starting {label}")
    return {"ok": True, "pid": proc.pid}


def _consume_launch_request(state: _LeaderState) -> None:
    """Launch the app the helper's add-volume path handed over (`launch.req` in
    the vault-owned runtime dir): a volume mounted into the RUNNING session
    never reaches the leader argv, so its `--first` app (the user's explicit
    pick, or their default file manager) arrives here instead. The file is
    root-written into a 0700 veracage dir, unreachable from sandboxes and the
    human uid; the spec gets the same validation as `set-apps` anyway."""
    path = Path(os.environ.get("XDG_RUNTIME_DIR", "/nonexistent")) / "launch.req"
    try:
        # errors="replace": undecodable bytes must not raise (a ValueError here
        # would escape the serve loop and tear the session down); the spec
        # validation below rejects the result anyway.
        raw = path.read_text(errors="replace")
    except OSError:
        return
    # Remove before launching, so a failing spec can never launch-loop.
    with contextlib.suppress(OSError):
        path.unlink()
    try:
        spec = json.loads(raw)
    except ValueError:
        spec = None
    exe = spec.get("exec") if isinstance(spec, dict) else None
    name = spec.get("name") if isinstance(spec, dict) else None
    if (not isinstance(exe, str) or not _exec_ok(exe)
            or not (name is None or (isinstance(name, str) and len(name) <= 128))):
        print("veracage: ignoring malformed launch request", file=sys.stderr)
        return
    r = _launch_app(state, {"name": name or exe, "exec": exe})
    if not r["ok"]:
        print(f"veracage: launch request: {r['error']}", file=sys.stderr)


def _post_status(message: str) -> None:
    """Publish a short progress note (`<nonce>\\t<text>`) to
    `/run/veracage/rt/status` for the compositor's toolbar. The compositor stops
    showing it once the app's window is mapped, or after its own timeout, so
    there is nothing to clear here. Best-effort."""
    text = "".join(c for c in message if c.isprintable())[:80]
    tmp = COMPOSITOR_RUNTIME / f"status.{os.getpid()}.tmp"
    try:
        tmp.write_text(f"{time.time_ns()}\t{text}\n")
        tmp.replace(COMPOSITOR_RUNTIME / "status")
    except OSError:
        with contextlib.suppress(OSError):
            tmp.unlink()


def _post_notice(message: str) -> None:
    """Publish a short user-facing notice for the compositor to show as a
    transient banner: `/run/veracage/rt/notice`, one `<nonce>\\t<text>` line. The
    leader is the veracage uid and rt is veracage-owned. Best-effort. The nonce
    (a wall-clock ns stamp) lets the compositor show each distinct notice once."""
    text = "".join(c for c in message if c.isprintable())[:200]
    notice = COMPOSITOR_RUNTIME / "notice"
    tmp = COMPOSITOR_RUNTIME / f"notice.{os.getpid()}.tmp"
    try:
        tmp.write_text(f"{time.time_ns()}\t{text}\n")
        tmp.replace(notice)
    except OSError:
        with contextlib.suppress(OSError):
            tmp.unlink()


# The vault file bridge (import/export/outbox over the control socket) was
# removed. The control socket is human-owned, so any same-uid process could
# drive it to read arbitrary vault files. Cross-boundary file transfer, when we
# add it, must go through the human-driven compositor path (which other uid
# processes cannot reach), never this socket.


# ------------------------------------------------------------- reaping -----

# A tracked app that exits within this many seconds of launch is reported as a
# failed launch, not a normal quit. The window covers the reaper's own latency (it
# runs once per serve-loop pass, at most ~1s apart) plus the app's brief startup.
_EARLY_EXIT_SECONDS = 2.0


def _reap_children(state: _LeaderState) -> None:
    """Non-blocking reap of exited bwrap app children (only the pids we track, so
    we don't race the compositor's own Popen). An app that dies within
    `_EARLY_EXIT_SECONDS` of launch is reported as a failed launch, since a launch
    that fails at once is otherwise silent (the app's stdio is discarded)."""
    now = time.monotonic()
    for pid in list(state.children):
        try:
            reaped, status = os.waitpid(pid, os.WNOHANG)
        except ChildProcessError:
            state.children.pop(pid, None)
            continue
        if reaped != pid:
            continue
        label, launched_at = state.children.pop(pid)
        _debug(state, f"exit {label!r}: pid={pid} status={status} "
                      f"after {now - launched_at:.1f}s")
        if now - launched_at < _EARLY_EXIT_SECONDS:
            _post_notice(f"{label} failed to launch (exited immediately)")


# --------------------------------------------------------- accept / serve --

def _accept_one(srv: socket.socket, state: _LeaderState) -> None:
    conn, _ = srv.accept()
    with conn:
        conn.settimeout(2.0)
        try:
            data = conn.recv(_MAX_REQUEST_BYTES)
            req = json.loads(data.decode().strip() or "{}")
            reply = _handle_request(state, req)
        except (ValueError, OSError) as e:
            reply = {"ok": False, "error": str(e)}
        except Exception as e:  # a handler bug must not kill the serve loop
            reply = {"ok": False, "error": f"internal error: {e}"}
        with contextlib.suppress(OSError):
            conn.sendall((json.dumps(reply) + "\n").encode())


def _control_fd() -> int:
    raw = os.environ.get("VERACAGE_CONTROL_FD")
    if not raw:
        raise RuntimeError("VERACAGE_CONTROL_FD not set (leader must be run via the helper)")
    try:
        return int(raw)
    except ValueError as e:
        raise RuntimeError(f"VERACAGE_CONTROL_FD not an integer: {raw!r}") from e


# ----------------------------------------------- toolbar app channel -------
#
# The compositor toolbar (same veracage uid, different process) launches this
# vault's apps. It can't reach the leader's human-owned control socket, so we
# expose a SECOND, veracage-owned socket under /run/veracage/rt and advertise the
# enabled app names in a sibling `.apps` file the compositor reads. A toolbar
# click sends a bare app index and we launch that app. Only the veracage uid can
# reach the socket, and a launch only runs an app the human already enabled.
# Nothing here widens what the human (or the vault) can already do.

def _session_id(state: _LeaderState) -> str:
    return Path(state.mountpoint).name


def _app_socket_path(state: _LeaderState) -> Path:
    return COMPOSITOR_RUNTIME / f"app-{_session_id(state)}.sock"


def _apps_file_path(state: _LeaderState) -> Path:
    return COMPOSITOR_RUNTIME / f"app-{_session_id(state)}.apps"


def _publish_apps(state: _LeaderState) -> socket.socket | None:
    """Create the veracage-owned app socket and advertise the app names so the
    compositor toolbar can show launcher buttons. Returns the listening socket,
    or None if the runtime dir isn't writable (the toolbar then just shows no
    launchers for this vault)."""
    sock_path = _app_socket_path(state)
    try:
        with contextlib.suppress(FileNotFoundError):
            sock_path.unlink()
        srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        srv.bind(str(sock_path))
        # 0700 explicitly, not whatever the inherited umask happened to be: only
        # the veracage uid may connect (matching the compositor socket's umask
        # hardening). A connect needs write on the socket inode, so this denies
        # every other uid even if the 0711 rt dir is traversable.
        os.chmod(sock_path, 0o700)
        srv.listen(4)
        srv.setblocking(False)
        _write_apps_file(state)
        return srv
    except OSError as e:
        print(f"veracage: could not publish toolbar apps: {e}", file=sys.stderr)
        return None


def _write_apps_file(state: _LeaderState) -> None:
    """(Re)write the `.apps` file the compositor toolbar reads. Plain, dependency-
    free format:  <sock filename>\\n<volume label(s)>\\n<opener index>\\n<app name>…
    Re-called when the open-volume set changes so the title tracks the volumes.
    Names are sanitised like the label (a newline would desync the protocol)."""
    names = [_sanitize_label(str(a.get("name") or a.get("exec") or "app"))
             for a in state.app_specs]
    # The "opener": first file manager, else the first app, else -1.
    opener = next(
        (i for i, a in enumerate(state.app_specs)
         if is_file_manager(str(a.get("exec") or ""))),
        0 if state.app_specs else -1,
    )
    # Format the compositor parses (see scan_leaders in toolbar.rs):
    #   <sock>\n<title>\n<vol1>\t<vol2>…\n<opener>\n<name>\n<name>…
    # The volumes line (tab-separated) drives the per-volume Close menu. Sanitize
    # the title AND every volume label (not just app names): a label with an
    # embedded newline/tab would desync the newline- and tab-delimited protocol.
    title = _sanitize_label(state.volume_label or "Volume")
    volumes = "\t".join(_sanitize_label(v) for v in state.volumes)
    body = [_app_socket_path(state).name, title, volumes, str(opener), *names]
    _apps_file_path(state).write_text("\n".join(body) + "\n")


def _unpublish_apps(state: _LeaderState) -> None:
    for p in (_app_socket_path(state), _apps_file_path(state)):
        with contextlib.suppress(OSError):
            p.unlink()


def _accept_app_launch(app_srv: socket.socket, state: _LeaderState) -> None:
    """A toolbar click: read a bare app index and launch that enabled app."""
    conn, _ = app_srv.accept()
    with conn:
        conn.settimeout(2.0)
        try:
            idx = int(conn.recv(64).decode().strip())
        except (ValueError, OSError):
            return
        if 0 <= idx < len(state.app_specs):
            r = _launch_app(state, state.app_specs[idx])
            if not r["ok"]:
                print(f"veracage: toolbar launch: {r['error']}", file=sys.stderr)


# ------------------------------------------------------------- places ------
#
# Seed the sandbox file manager's Places so the vault appears as a named volume
# (KDE/Dolphin reads $XDG_DATA_HOME/user-places.xbel). The label is the volume's
# own filesystem label (the helper reads it via blkid); the entry points at /vaults.

def _sanitize_label(raw: str) -> str:
    """A safe volume label: no newlines (they'd desync the newline-delimited
    `.apps` protocol) and no control chars (they'd break the KDE XBEL / window
    title), length-capped. Falls back to 'Volume' if nothing printable remains.
    Defends against a crafted filesystem label on an attacker-supplied volume."""
    cleaned = "".join(c if c.isprintable() else " " for c in raw).strip()
    return cleaned[:64] or "Volume"


# Characters that must not appear in an app `exec`: whitespace + shell
# metacharacters. bwrap runs exec as a single argv element (no shell), so these
# can't be interpreted, but rejecting them keeps a same-uid `set-apps` from
# swapping a menu entry to anything but a bare command.
_EXEC_META = set(" \t\n\r\f\v;|&$<>`'\"\\(){}[]*?!#~")


def _exec_ok(exe: str) -> bool:
    """True if `exe` is a plausible bare command / path: non-empty, length-
    capped, no whitespace, shell metacharacters, or control characters."""
    return (
        bool(exe)
        and len(exe) <= 512
        and not any(c in _EXEC_META or ord(c) < 0x20 for c in exe)
    )


def _bookmark(href: str, title: str, icon: str, ident: str) -> str:
    # Escape EVERY interpolated value (attributes with quoteattr, text with
    # escape), so the seed stays well-formed XBEL regardless of what a volume
    # label contains. Labels are already charset-restricted by the helper, but
    # don't rely on that external rule to keep the XML safe.
    return (
        f' <bookmark href={quoteattr(href)}>\n'
        f'  <title>{html.escape(title)}</title>\n'
        '  <info>\n'
        '   <metadata owner="http://freedesktop.org">\n'
        f'    <bookmark:icon name={quoteattr(icon)}/>\n'
        '   </metadata>\n'
        '   <metadata owner="http://www.kde.org">\n'
        f'    <ID>{html.escape(ident)}</ID>\n'
        '    <isSystemItem>false</isSystemItem>\n'
        '   </metadata>\n'
        '  </info>\n'
        ' </bookmark>\n'
    )


def _places_xbel(labels: list[str], with_exchange: bool) -> str:
    # One Places entry per open volume, each pointing at /vaults/<label>. The
    # names are the workspace directory names (helper-sanitised: no spaces/slashes),
    # so they need no URL-encoding.
    body = "".join(
        _bookmark(f"file:///vaults/{lbl}", lbl, "drive-harddisk-encrypted",
                  f"veracage-vault-{lbl}")
        for lbl in (labels or ["Volume"])
    )
    if with_exchange:
        # The shared host<->vault directory, mounted at /exchange in the sandbox.
        body += _bookmark("file:///exchange", "Shared directory",
                          "folder-publicshare", "veracage-exchange")
    return (
        '<?xml version="1.0" encoding="UTF-8"?>\n'
        '<!DOCTYPE xbel>\n'
        '<xbel xmlns:bookmark="http://www.freedesktop.org/standards/desktop-bookmarks"'
        ' xmlns:kdepriv="http://www.kde.org/kdepriv"'
        ' xmlns:mime="http://www.freedesktop.org/standards/shared-mime-info">\n'
        f'{body}'
        '</xbel>\n'
    )


def _write_places_file(labels: list[str], with_exchange: bool = False) -> Path | None:
    """Write the seeded Places file (one entry per open volume) to the vault
    runtime dir and return its path, or None if it can't be written (the sandbox
    then just has no Places entries)."""
    try:
        path = Path(os.environ["XDG_RUNTIME_DIR"]) / "user-places.xbel"
        path.write_text(_places_xbel(labels, with_exchange))
        return path
    except (OSError, KeyError) as e:
        print(f"veracage: could not seed Places: {e}", file=sys.stderr)
        return None


# --------------------------------------------------------- leader run ------

def run_leader(mountpoint: str, app_specs: list, first_app: dict | None,
               debug: bool = False) -> int:
    """Become the vault-side session leader. Returns the exit code.

    Attaches to the ONE persistent compositor's shared socket (brought up
    separately by the helper), publishes `app_specs` to the compositor toolbar,
    optionally launches `first_app`, then serves the control socket
    (ping/list/close) and the toolbar app socket until 'close', a signal, or
    the compositor going away. The leader does NOT own the compositor (it
    survives every app opening and closing) but when the compositor itself exits
    (the user closed the vault window) the leader exits too, so the session tears
    down cleanly (unit stop → ExecStopPost → dm close + unmount) instead of
    leaving the vault mounted and blocking the next open.
    """
    vr = os.environ.get("VERACAGE_VAULT_RUNTIME")
    if vr:
        os.environ["XDG_RUNTIME_DIR"] = vr

    state = _LeaderState(mountpoint=mountpoint, app_specs=app_specs or [],
                         debug=debug)

    # The shared compositor must already be up (cli.py brings it up before the
    # mount). We only observe its socket. We never spawn it.
    if not COMPOSITOR_SOCKET.exists():
        print(f"veracage: compositor socket {COMPOSITOR_SOCKET} not found. "
              "The persistent compositor is not running.", file=sys.stderr)
        return 1
    state.wl_socket = COMPOSITOR_SOCKET

    # The open volumes (there may be several: subsequent opens setns more into the
    # workspace). Their labels drive the window title + the Places entries; the
    # sandbox binds the whole /vaults tree so one app sees them all.
    state.exchange = os.environ.get("VERACAGE_EXCHANGE") or None
    labels = scan_volumes()
    state.volumes = labels
    state.volume_label = ", ".join(labels) if labels else "Volume"
    state.places_file = _write_places_file(labels, state.exchange is not None)

    stop = threading.Event()

    def _on_signal(_signum, _frame):
        stop.set()
    signal.signal(signal.SIGINT, _on_signal)
    signal.signal(signal.SIGTERM, _on_signal)
    signal.signal(signal.SIGCHLD, lambda *_: None)

    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM, fileno=_control_fd())
    srv.setblocking(False)
    app_srv = _publish_apps(state)

    try:
        if first_app:
            r = _launch_app(state, first_app)
            if not r["ok"]:
                print(f"veracage: first app: {r['error']}", file=sys.stderr)

        sel = selectors.DefaultSelector()
        sel.register(srv, selectors.EVENT_READ)
        if app_srv is not None:
            sel.register(app_srv, selectors.EVENT_READ)
        # Tie our lifetime to the compositor's: once it has been seen up, its
        # disappearance (the user closed the vault window → the compositor exits)
        # means the session is over. Exiting here lets the systemd unit stop and
        # its ExecStopPost cleanup close the dm device + unmount. Otherwise the
        # leader would keep the vault mounted forever and block the next open.
        comp_seen = False
        seen_labels = labels
        try:
            while not stop.is_set():
                _reap_children(state)
                if state.closing:
                    break
                # Pick up volumes added to the workspace (subsequent opens setns
                # more in): refresh Places (for the NEXT app launch: a running
                # app's mount NS is fixed) and the compositor title.
                cur = scan_volumes()
                if cur != seen_labels:
                    seen_labels = cur
                    state.volumes = cur
                    state.volume_label = ", ".join(cur) if cur else "Volume"
                    state.places_file = _write_places_file(cur, state.exchange is not None)
                    _write_apps_file(state)
                # After the Places refresh, so the app launched for a just-added
                # volume gets the seed that already lists it. Guarded like the
                # accept paths below: an unexpected failure here must not unwind
                # into the finally and SIGKILL every running app.
                try:
                    _consume_launch_request(state)
                except Exception as e:  # noqa: BLE001 - serve loop must survive
                    print(f"veracage: launch request error (continuing): {e}",
                          file=sys.stderr)
                if compositor_is_up():
                    comp_seen = True
                elif comp_seen:
                    print("veracage: compositor gone (window closed) - "
                          "unmounting and exiting.", file=sys.stderr)
                    break
                # A quarter second, not a second: this timeout also bounds how
                # long a just-mounted volume waits for its app to be launched
                # (the helper hands it over through launch.req, polled above).
                # The loop body is a scandir on a tmpfs plus a non-blocking
                # reap, so polling four times a second costs nothing measurable.
                for key, _ in sel.select(timeout=0.25):
                    # A transient accept() error (ECONNABORTED/EAGAIN from a peer
                    # that aborts a queued connection) or an unexpected launch
                    # failure must NOT unwind into the finally and SIGKILL every
                    # running app: log and keep serving.
                    try:
                        if key.fileobj is srv:
                            _accept_one(srv, state)
                        elif key.fileobj is app_srv:
                            _accept_app_launch(app_srv, state)
                    except Exception as e:  # noqa: BLE001 - serve loop must survive
                        print(f"veracage: serve error (continuing): {e}", file=sys.stderr)
        finally:
            sel.close()
            _terminate_children(state)
        return 0
    finally:
        _unpublish_apps(state)
        if app_srv is not None:
            app_srv.close()
        srv.close()


def _terminate_children(state: _LeaderState, timeout: float = 3.0) -> None:
    """SIGTERM tracked apps, wait up to `timeout`s, then SIGKILL stragglers."""
    for pid in list(state.children):
        with contextlib.suppress(ProcessLookupError):
            os.kill(pid, signal.SIGTERM)
    deadline = time.monotonic() + timeout
    for pid in list(state.children):
        while time.monotonic() < deadline:
            try:
                if os.waitpid(pid, os.WNOHANG)[0] == pid:
                    break
            except ChildProcessError:
                break
            time.sleep(0.05)
        else:
            with contextlib.suppress(ProcessLookupError):
                os.kill(pid, signal.SIGKILL)
            with contextlib.suppress(ChildProcessError):
                os.waitpid(pid, 0)


# ------------------------------------- human-side control client (cli.py) --

def session_socket_path(vault: str) -> Path:
    """Path of the control socket for `vault`: the same location the helper
    creates it (sha256(canonical path)[:16]). The caller must pass the resolved
    vault path so the hash matches the helper's."""
    import hashlib
    h = hashlib.sha256(vault.encode()).hexdigest()[:16]
    return Path(os.environ["XDG_RUNTIME_DIR"]) / "veracage" / "sessions" / f"{h}.sock"


def send_request(vault: str, request: dict) -> dict:
    """Connect to the running session for `vault` (by path) and send one
    request. Raises FileNotFoundError if no session is running."""
    return send_request_to(session_socket_path(vault), request)


def send_request_to(sock_path: Path, request: dict) -> dict:
    """Send one request to a session control socket by PATH (the per-vault hash
    naming is the caller's concern). Raises FileNotFoundError if absent."""
    if not sock_path.exists():
        raise FileNotFoundError(f"no session socket at {sock_path}")
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(2.0)
    try:
        s.connect(str(sock_path))
        s.sendall((json.dumps(request) + "\n").encode())
        data = b""
        while not data.endswith(b"\n"):
            chunk = s.recv(4096)
            if not chunk:
                break
            data += chunk
        return json.loads(data.decode().strip() or "{}")
    finally:
        s.close()


