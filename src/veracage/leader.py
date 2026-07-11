"""Vault-side session leader — runs AS the vault uid (the helper dropped to it).

The Rust helper has already opened the volume, idmap-mounted it at MOUNTPOINT as
the vault uid (in a private mount NS), created the control listening socket, and
provisioned a vault-writable runtime dir. It passes those through the environment
and execs us:

  VERACAGE_CONTROL_FD     inherited control *listening* socket (we accept on it)
  VERACAGE_VAULT_RUNTIME  our XDG_RUNTIME_DIR (bwrap /run/user)

We launch apps in bwrap wired to the ONE persistent compositor's shared socket
(/run/veracage/rt/wl-vc, brought up separately by the helper — we do not spawn
it). The leader is a plain executor: it runs the command the human hands it,
sandboxed, and outlives no compositor of its own. The
security property — *external processes can't read the vault* — comes from the
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

from .apps import App, is_file_manager
from .sandbox import bwrap_command
from .wayland import COMPOSITOR_RUNTIME, COMPOSITOR_SOCKET, compositor_is_up

_MAX_REQUEST_BYTES = 64 * 1024  # control requests are tiny; cap to bound memory

# The shared-workspace root (must match WORKSPACE in helper-rs/src/main.rs): the
# leader's private-NS tmpfs holding every open volume at <WORKSPACE>/<label>. The
# sandbox binds this whole tree at /vaults, so one app sees all volumes.
WORKSPACE = Path("/run/veracage/vaults")


def scan_volumes(root: Path = WORKSPACE) -> list[str]:
    """The open volumes' labels — the directory names under the workspace root
    (the helper already sanitised them to a single safe path component). Dot
    entries (the `.exchange` mount, staging leftovers) are skipped. Sorted."""
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
    gpu: bool = False
    children: dict[int, str] = field(default_factory=dict)  # pid -> label
    closing: bool = False
    app_specs: list = field(default_factory=list)  # enabled apps, for the toolbar
    places_file: Path | None = None  # seeded KDE Places (vault under its label)
    volume_label: str = "Vault"      # volume label (window title + Places name)
    exchange: str | None = None      # idmapped host<->vault shared dir -> /exchange


# ------------------------------------------------------------- protocol ----
#
# One line of JSON per request, one per reply. Status/lifecycle only — no launch
# or file transfer (this socket is human-owned; any same-uid process can reach it).
#   {"cmd": "ping"}                       -> {"ok": true, "uid": <vault uid>, ...}
#   {"cmd": "list"}                       -> {"ok": true, "apps": [{"pid","app"}]}
#   {"cmd": "close"}                      -> {"ok": true}

def _handle_request(state: _LeaderState, req: dict) -> dict:
    if not isinstance(req, dict):
        return {"ok": False, "error": "request must be a JSON object"}
    cmd = req.get("cmd")

    if cmd == "ping":
        return {"ok": True, "uid": os.getuid(), "mountpoint": state.mountpoint}

    if cmd == "list":
        return {"ok": True,
                "apps": [{"pid": p, "app": k} for p, k in state.children.items()]}

    if cmd == "close":
        state.closing = True
        return {"ok": True}

    # There is deliberately NO exec / import / export / outbox here. The control
    # socket is human-owned, so ANY process running as the human uid can connect
    # to it. If it could make the leader run a command in the vault (exec) or
    # hand vault files back out (export), a same-uid attacker would have a full
    # vault-exfiltration primitive (it did — see the pen test). Launching happens
    # only over the veracage-owned app socket, by index into the human's own
    # enabled list (`_accept_app_launch`), which other uids cannot reach.
    return {"ok": False, "error": f"unknown cmd: {cmd}"}


def _launch_app(state: _LeaderState, spec) -> dict:
    """Launch the command in `spec` (an {exec, args, name} dict) in bwrap against
    the compositor; track its pid. `spec` only ever comes from the leader's OWN
    enabled list — `first_app` (set at open) or `state.app_specs[idx]` on a
    toolbar click — never from a control-socket peer, so a same-uid caller can't
    make it run an arbitrary command. bwrap is what confines whatever does run."""
    if state.wl_socket is None:
        return {"ok": False, "error": "compositor not ready"}
    if not isinstance(spec, dict):
        return {"ok": False, "error": "missing app spec"}
    command = spec.get("exec")
    if not isinstance(command, str) or not command:
        return {"ok": False, "error": "app spec needs a non-empty 'exec'"}
    args = spec.get("args", [])
    if not isinstance(args, list):
        return {"ok": False, "error": "app 'args' must be a list"}
    name_val = spec.get("name")
    label = name_val if isinstance(name_val, str) else command

    app = App(key=label, name=label, exec=command, args=[str(a) for a in args])
    # Open the seeded Places file and hand bwrap its fd: `--file` writes a
    # WRITABLE copy into the sandbox tmpfs (Dolphin rewrites it on startup, so a
    # read-only bind would error). None if there's no seed.
    places_fd = None
    if state.places_file is not None:
        try:
            places_fd = os.open(state.places_file, os.O_RDONLY)
        except OSError:
            places_fd = None
    try:
        # Bind the whole workspace (/vaults tree), not one volume — the app sees
        # every volume open at launch time (docs/shared-workspace-redesign.md).
        argv = bwrap_command(str(WORKSPACE), app, state.wl_socket, state.gpu,
                             places_fd, state.exchange)
        # Detach the app's stdio. Inheriting the leader's stdin/out/err hands a
        # chatty viewer the session's terminal/journal: Qt/KF apps print the paths
        # of files they open on stderr, which would persist unencrypted in the
        # user journal, readable by any same-uid process after the vault closes
        # (an accidental-leak channel in the threat model) — and hands the app an
        # fd to the human's pty. Nothing vault-side needs the app's stdio.
        proc = subprocess.Popen(
            argv,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            pass_fds=(places_fd,) if places_fd is not None else (),
        )
    except FileNotFoundError as e:
        return {"ok": False, "error": f"missing dependency: {e.filename}"}
    finally:
        if places_fd is not None:
            os.close(places_fd)
    state.children[proc.pid] = label
    return {"ok": True, "pid": proc.pid}


# The vault file bridge (import/export/outbox over the control socket) was
# removed. The control socket is human-owned, so any same-uid process could
# drive it to read arbitrary vault files. Cross-boundary file transfer, when we
# add it, must go through the human-driven compositor path (which other uid
# processes cannot reach), never this socket.


# ------------------------------------------------------------- reaping -----

def _reap_children(state: _LeaderState) -> None:
    """Non-blocking reap of exited bwrap app children (only the pids we track,
    so we don't race the compositor's own Popen)."""
    for pid in list(state.children):
        try:
            reaped, _status = os.waitpid(pid, os.WNOHANG)
        except ChildProcessError:
            state.children.pop(pid, None)
            continue
        if reaped == pid:
            state.children.pop(pid, None)


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
# reach the socket, and a launch only runs an app the human already enabled —
# nothing here widens what the human (or the vault) can already do.

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
    body = [_app_socket_path(state).name, state.volume_label or "Vault", str(opener), *names]
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
    title); length-capped. Falls back to 'Vault' if nothing printable remains.
    Defends against a crafted filesystem label on an attacker-supplied volume."""
    cleaned = "".join(c if c.isprintable() else " " for c in raw).strip()
    return cleaned[:64] or "Vault"


def _bookmark(href: str, title: str, icon: str, ident: str) -> str:
    return (
        f' <bookmark href="{href}">\n'
        f'  <title>{html.escape(title)}</title>\n'
        '  <info>\n'
        '   <metadata owner="http://freedesktop.org">\n'
        f'    <bookmark:icon name="{icon}"/>\n'
        '   </metadata>\n'
        '   <metadata owner="http://www.kde.org">\n'
        f'    <ID>{ident}</ID>\n'
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
        for lbl in (labels or ["Vault"])
    )
    if with_exchange:
        # The shared host<->vault folder, mounted at /exchange in the sandbox.
        body += _bookmark("file:///exchange", "Exchange (host-shared)",
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

def run_leader(mountpoint: str, gpu: bool, app_specs: list, first_app: dict | None) -> int:
    """Become the vault-side session leader. Returns the exit code.

    Attaches to the ONE persistent compositor's shared socket (brought up
    separately by the helper), publishes `app_specs` to the compositor toolbar,
    optionally launches `first_app`, then serves the control socket
    (ping/list/close) and the toolbar app socket until 'close', a signal, or
    the compositor going away. The leader does NOT own the compositor — it
    survives every app opening and closing — but when the compositor itself exits
    (the user closed the vault window) the leader exits too, so the session tears
    down cleanly (unit stop → ExecStopPost → dm close + unmount) instead of
    leaving the vault mounted and blocking the next open.
    """
    vr = os.environ.get("VERACAGE_VAULT_RUNTIME")
    if vr:
        os.environ["XDG_RUNTIME_DIR"] = vr

    state = _LeaderState(mountpoint=mountpoint, gpu=gpu, app_specs=app_specs or [])

    # The shared compositor must already be up (cli.py brings it up before the
    # mount). We only observe its socket — we never spawn it.
    if not COMPOSITOR_SOCKET.exists():
        print(f"veracage: compositor socket {COMPOSITOR_SOCKET} not found; "
              "the persistent compositor is not running.", file=sys.stderr)
        return 1
    state.wl_socket = COMPOSITOR_SOCKET

    # The open volumes (there may be several — subsequent opens setns more into the
    # workspace). Their labels drive the window title + the Places entries; the
    # sandbox binds the whole /vaults tree so one app sees them all.
    state.exchange = os.environ.get("VERACAGE_EXCHANGE") or None
    labels = scan_volumes()
    state.volume_label = ", ".join(labels) if labels else "Vault"
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
        # its ExecStopPost cleanup close the dm device + unmount — otherwise the
        # leader would keep the vault mounted forever and block the next open.
        comp_seen = False
        seen_labels = labels
        try:
            while not stop.is_set():
                _reap_children(state)
                if state.closing:
                    break
                # Pick up volumes added to the workspace (subsequent opens setns
                # more in): refresh Places (for the NEXT app launch — a running
                # app's mount NS is fixed) and the compositor title.
                cur = scan_volumes()
                if cur != seen_labels:
                    seen_labels = cur
                    state.volume_label = ", ".join(cur) if cur else "Vault"
                    state.places_file = _write_places_file(cur, state.exchange is not None)
                    _write_apps_file(state)
                if compositor_is_up():
                    comp_seen = True
                elif comp_seen:
                    print("veracage: compositor gone (window closed) — "
                          "unmounting and exiting.", file=sys.stderr)
                    break
                for key, _ in sel.select(timeout=1.0):
                    # A transient accept() error (ECONNABORTED/EAGAIN from a peer
                    # that aborts a queued connection) or an unexpected launch
                    # failure must NOT unwind into the finally and SIGKILL every
                    # running app — log and keep serving.
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
    """Path of the control socket for `vault` — the same location the helper
    creates it (sha256(canonical path)[:16]). The caller must pass the resolved
    vault path so the hash matches the helper's."""
    import hashlib
    h = hashlib.sha256(vault.encode()).hexdigest()[:16]
    return Path(os.environ["XDG_RUNTIME_DIR"]) / "veracage" / "sessions" / f"{h}.sock"


def send_request(vault: str, request: dict) -> dict:
    """Connect to the running session for `vault` (by path) and send one
    request. Raises FileNotFoundError if no session is running."""
    sock_path = session_socket_path(vault)
    if not sock_path.exists():
        raise FileNotFoundError(f"no active session for {vault}")
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


