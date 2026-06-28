"""Vault-side session leader — runs AS the vault uid (the helper dropped to it).

The Rust helper has already opened the volume, idmap-mounted it at MOUNTPOINT as
the vault uid (in a private mount NS), connected the host Wayland socket, created
the control listening socket, and provisioned a vault-writable runtime dir. It
passes those to us through the environment and execs us:

  VERACAGE_CONTROL_FD     inherited control *listening* socket (we accept on it)
  VERACAGE_WAYLAND_FD     inherited connected fd to the host compositor (weston)
  VERACAGE_VAULT_RUNTIME  our XDG_RUNTIME_DIR (weston socket, bwrap /run/user)

We serve the control protocol over the inherited socket and (from increment 2)
run nested weston against the host fd + launch apps in bwrap. The human side
connects to the socket *by path* to drive the session; it has no vault access.

This is the deny-by-UID counterpart of session.py (the option-a leader, which
ran as the human inside a hide-only mount NS). session.py stays until cli.py is
rewired onto this module.
"""
from __future__ import annotations

import contextlib
import json
import os
import selectors
import signal
import socket
import subprocess
import threading
import time
from dataclasses import dataclass, field
from pathlib import Path

from . import config
from .sandbox import bwrap_command

_MAX_REQUEST_BYTES = 64 * 1024  # control requests are tiny; cap to bound memory


# --------------------------------------------------------- leader state ----

@dataclass
class _LeaderState:
    mountpoint: str
    vault: str
    weston_socket: Path | None = None   # set once weston is up (increment 2)
    gpu: bool = False
    children: dict[int, str] = field(default_factory=dict)  # pid -> app key
    closing: bool = False


# ------------------------------------------------------------- protocol ----
#
# One line of JSON per request, one per reply.
#   {"cmd": "ping"}                  -> {"ok": true, "uid": <vault uid>, ...}
#   {"cmd": "list"}                  -> {"ok": true, "apps": [{"pid","app"}, ...]}
#   {"cmd": "exec", "app": "<key>"}  -> {"ok": true, "pid": N} | {"ok": false,...}
#   {"cmd": "close"}                 -> {"ok": true}

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
        key = req.get("app")
        if not isinstance(key, str):
            return {"ok": False, "error": "missing or invalid 'app'"}
        if state.weston_socket is None:
            return {"ok": False, "error": "compositor not ready"}
        cfg = config.load()
        app = cfg.apps.get(key)
        if app is None:
            return {"ok": False, "error": f"app '{key}' is not enabled"}
        try:
            argv = bwrap_command(state.mountpoint, app, state.weston_socket, state.gpu)
            proc = subprocess.Popen(argv)
        except FileNotFoundError as e:
            return {"ok": False, "error": f"missing dependency: {e.filename}"}
        state.children[proc.pid] = key
        return {"ok": True, "pid": proc.pid}

    return {"ok": False, "error": f"unknown cmd: {cmd}"}


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
        try:
            data = b""
            while not data.endswith(b"\n"):
                chunk = conn.recv(4096)
                if not chunk:
                    break
                data += chunk
                if len(data) > _MAX_REQUEST_BYTES:
                    raise ValueError("request too large")
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


# --------------------------------------------------------- leader run ------

def run_leader(mountpoint: str, vault: str, first_app_key: str | None) -> int:
    """Become the vault-side session leader. Returns the exit code.

    Increment 1: serve the control socket (ping/list/close; exec replies
    'compositor not ready' until weston is wired in increment 2) and exit on
    'close' or a signal.
    """
    vr = os.environ.get("VERACAGE_VAULT_RUNTIME")
    if vr:
        os.environ["XDG_RUNTIME_DIR"] = vr

    cfg = config.load()
    state = _LeaderState(mountpoint=mountpoint, vault=vault, gpu=cfg.gpu_for(vault))

    stop = threading.Event()

    def _on_signal(_signum, _frame):
        stop.set()
    signal.signal(signal.SIGINT, _on_signal)
    signal.signal(signal.SIGTERM, _on_signal)
    signal.signal(signal.SIGCHLD, lambda *_: None)

    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM, fileno=_control_fd())
    srv.setblocking(False)
    sel = selectors.DefaultSelector()
    sel.register(srv, selectors.EVENT_READ)
    try:
        while not stop.is_set():
            _reap_children(state)
            if state.closing:
                break
            for key, _ in sel.select(timeout=1.0):
                if key.fileobj is srv:
                    _accept_one(srv, state)
    finally:
        sel.close()
        srv.close()
        _terminate_children(state)
    return 0


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
