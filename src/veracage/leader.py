"""Vault-side session leader — runs AS the vault uid (the helper dropped to it).

The Rust helper has already opened the volume, idmap-mounted it at MOUNTPOINT as
the vault uid (in a private mount NS), connected the host Wayland socket, created
the control listening socket, and provisioned a vault-writable runtime dir. It
passes those through the environment and execs us:

  VERACAGE_CONTROL_FD     inherited control *listening* socket (we accept on it)
  VERACAGE_WAYLAND_FD     inherited connected fd to the host compositor (weston)
  VERACAGE_VAULT_RUNTIME  our XDG_RUNTIME_DIR (weston socket, bwrap /run/user)

We run nested weston against the host fd and launch apps in bwrap. The leader is
a plain executor: it runs the command the human hands it, sandboxed. The
security property — *external processes can't read the vault* — comes from the
idmap (the vault is owned by a uid no one else has) + the mount NS (hidden) +
bwrap (apps have no net/host-FS, so they can't exfiltrate). The app allowlist is
UX on the human side (which apps to offer); it is NOT a vault-side restriction,
so the leader does not load config or police what it's told to run.

This is the deny-by-UID counterpart of session.py (the option-a leader). session
.py stays until cli.py is rewired onto this module.
"""
from __future__ import annotations

import contextlib
import json
import os
import selectors
import shutil
import signal
import socket
import subprocess
import sys
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path

from .apps import App
from .sandbox import bwrap_command
from .wayland import WestonStartFailed, nested_weston

_MAX_REQUEST_BYTES = 64 * 1024  # control requests are tiny; cap to bound memory


# --------------------------------------------------------- leader state ----

@dataclass
class _LeaderState:
    mountpoint: str
    weston_socket: Path | None = None   # set once weston is up
    gpu: bool = False
    children: dict[int, str] = field(default_factory=dict)  # pid -> label
    closing: bool = False
    launched_any: bool = False          # gate close-on-empty until first launch


# ------------------------------------------------------------- protocol ----
#
# One line of JSON per request, one per reply.
#   {"cmd": "ping"}                       -> {"ok": true, "uid": <vault uid>, ...}
#   {"cmd": "list"}                       -> {"ok": true, "apps": [{"pid","app"}]}
#   {"cmd": "exec", "app": {"exec","args","name"}}
#                                         -> {"ok": true, "pid": N} | {"ok": false}
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

    if cmd == "exec":
        return _launch_app(state, req.get("app"))

    return {"ok": False, "error": f"unknown cmd: {cmd}"}


def _launch_app(state: _LeaderState, spec) -> dict:
    """Launch the command in `spec` (an {exec, args, name} dict the human side
    resolved) in bwrap against the nested weston; track its pid. Shared by the
    first-app launch and the `exec` command. The leader runs what it's given —
    bwrap, not an allowlist, is what stops a launched app exfiltrating."""
    if state.weston_socket is None:
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

    app = App(key=label, name=label, category="app",
              exec=command, args=[str(a) for a in args])
    try:
        argv = bwrap_command(state.mountpoint, app, state.weston_socket, state.gpu)
        proc = subprocess.Popen(argv)
    except FileNotFoundError as e:
        return {"ok": False, "error": f"missing dependency: {e.filename}"}
    state.children[proc.pid] = label
    state.launched_any = True
    return {"ok": True, "pid": proc.pid}


# --------------------------------------------------------------- bridge ----
#
# The vault is owned by the vault uid, so the human can't read/write it
# directly. Files cross via fd-passing over the control socket:
#   import: the human sends a host-file fd; the leader writes it to the inbox.
#   export: the human asks for an outbox file; the leader sends back its fd.
# This is the user's own deliberate channel; the security property (external
# processes can't read the *vault*) is unchanged — only the inbox/outbox cross.

def _vault_subdir(state: _LeaderState, sub: str) -> Path:
    d = Path(state.mountpoint) / ".veracage" / sub
    d.mkdir(parents=True, exist_ok=True)
    return d


def _safe_name(raw) -> str | None:
    """A basename within the inbox/outbox — never a path that escapes them."""
    name = os.path.basename(str(raw or "")).strip()
    return name if name and name not in (".", "..") else None


def _do_import(state: _LeaderState, req: dict, fds: list[int]) -> dict:
    """Write a received host-file fd into the vault inbox (.veracage/in/)."""
    if not fds:
        return {"ok": False, "error": "import needs a file descriptor"}
    name = _safe_name(req.get("name"))
    if not name:
        return {"ok": False, "error": "invalid import name"}
    dest = _vault_subdir(state, "in") / name
    try:
        with os.fdopen(fds[0], "rb", closefd=False) as src, open(dest, "wb") as out:
            shutil.copyfileobj(src, out)
    except OSError as e:
        return {"ok": False, "error": f"import failed: {e}"}
    return {"ok": True, "path": str(dest)}


def _do_export(state: _LeaderState, req: dict) -> tuple[dict, list[int]]:
    """Open an outbox file (.veracage/out/) and hand its fd back to the human."""
    name = _safe_name(req.get("name"))
    if not name:
        return {"ok": False, "error": "invalid export name"}, []
    src = _vault_subdir(state, "out") / name
    try:
        fd = os.open(src, os.O_RDONLY)
    except OSError as e:
        return {"ok": False, "error": f"export failed: {e}"}, []
    return {"ok": True, "name": name}, [fd]


# ------------------------------------------------------------- reaping -----

def _reap_children(state: _LeaderState) -> None:
    """Non-blocking reap of exited bwrap app children (only the pids we track,
    so we don't race weston's own Popen)."""
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
        in_fds: list[int] = []
        out_fds: list[int] = []
        try:
            data, in_fds, _flags, _addr = socket.recv_fds(conn, _MAX_REQUEST_BYTES, 1)
            req = json.loads(data.decode().strip() or "{}")
            cmd = req.get("cmd") if isinstance(req, dict) else None
            if cmd == "import":
                reply = _do_import(state, req, in_fds)
            elif cmd == "export":
                reply, out_fds = _do_export(state, req)
            else:
                reply = _handle_request(state, req)
        except (ValueError, OSError) as e:
            reply = {"ok": False, "error": str(e)}
        except Exception as e:  # a handler bug must not kill the serve loop
            reply = {"ok": False, "error": f"internal error: {e}"}
        msg = (json.dumps(reply) + "\n").encode()
        with contextlib.suppress(OSError):
            if out_fds:
                socket.send_fds(conn, [msg], out_fds)
            else:
                conn.sendall(msg)
        for fd in in_fds + out_fds:
            with contextlib.suppress(OSError):
                os.close(fd)


def _control_fd() -> int:
    raw = os.environ.get("VERACAGE_CONTROL_FD")
    if not raw:
        raise RuntimeError("VERACAGE_CONTROL_FD not set (leader must be run via the helper)")
    try:
        return int(raw)
    except ValueError as e:
        raise RuntimeError(f"VERACAGE_CONTROL_FD not an integer: {raw!r}") from e


# --------------------------------------------------------- leader run ------

def run_leader(mountpoint: str, gpu: bool, first_app: dict | None) -> int:
    """Become the vault-side session leader. Returns the exit code.

    Starts nested weston (against the host fd in VERACAGE_WAYLAND_FD), launches
    `first_app` if given, then serves the control socket (ping/list/exec/close)
    until 'close', a signal, or — once an app has launched — all apps exit.
    """
    vr = os.environ.get("VERACAGE_VAULT_RUNTIME")
    if vr:
        os.environ["XDG_RUNTIME_DIR"] = vr

    state = _LeaderState(mountpoint=mountpoint, gpu=gpu)

    stop = threading.Event()

    def _on_signal(_signum, _frame):
        stop.set()
    signal.signal(signal.SIGINT, _on_signal)
    signal.signal(signal.SIGTERM, _on_signal)
    signal.signal(signal.SIGCHLD, lambda *_: None)

    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM, fileno=_control_fd())
    srv.setblocking(False)

    wl_raw = os.environ.get("VERACAGE_WAYLAND_FD")
    upstream = int(wl_raw) if wl_raw else None

    try:
        with nested_weston(upstream_fd=upstream) as wl_socket:
            state.weston_socket = wl_socket
            if first_app:
                r = _launch_app(state, first_app)
                if not r["ok"]:
                    print(f"veracage: first app: {r['error']}", file=sys.stderr)

            sel = selectors.DefaultSelector()
            sel.register(srv, selectors.EVENT_READ)
            try:
                while not stop.is_set():
                    _reap_children(state)
                    if state.closing or (state.launched_any and not state.children):
                        break
                    for key, _ in sel.select(timeout=1.0):
                        if key.fileobj is srv:
                            _accept_one(srv, state)
            finally:
                sel.close()
                _terminate_children(state)
        return 0
    except WestonStartFailed as e:
        print(f"veracage: weston failed to start: {e}", file=sys.stderr)
        print("(install with: apt install weston)", file=sys.stderr)
        return 1
    finally:
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


def _connect(vault: str) -> socket.socket:
    sock_path = session_socket_path(vault)
    if not sock_path.exists():
        raise FileNotFoundError(f"no active session for {vault}")
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(5.0)
    s.connect(str(sock_path))
    return s


def import_file(vault: str, host_path: str) -> dict:
    """Pass a host file's fd to the leader, which writes it to the vault inbox.
    The leader reads the data as the vault uid; the human never touches the vault."""
    name = os.path.basename(host_path)
    fd = os.open(host_path, os.O_RDONLY)
    s = _connect(vault)
    try:
        req = json.dumps({"cmd": "import", "name": name}).encode() + b"\n"
        socket.send_fds(s, [req], [fd])
        data, _fds, _f, _a = socket.recv_fds(s, 65536, 0)
        return json.loads(data.decode().strip() or "{}")
    finally:
        os.close(fd)
        s.close()


def export_file(vault: str, name: str, dest_path: str) -> dict:
    """Ask the leader for an outbox file; it sends the fd, we write it host-side."""
    s = _connect(vault)
    try:
        req = json.dumps({"cmd": "export", "name": name}).encode() + b"\n"
        s.sendall(req)
        data, fds, _f, _a = socket.recv_fds(s, 65536, 1)
        reply = json.loads(data.decode().strip() or "{}")
        try:
            if reply.get("ok") and fds:
                with os.fdopen(fds[0], "rb", closefd=False) as src, open(dest_path, "wb") as out:
                    shutil.copyfileobj(src, out)
        finally:
            for fd in fds:
                with contextlib.suppress(OSError):
                    os.close(fd)
        return reply
    finally:
        s.close()
