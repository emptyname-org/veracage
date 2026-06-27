"""Session leader + control socket.

The continuation process (running as the user, inside the private mount NS,
with the vault mounted at MOUNTPOINT) becomes the *session leader*:

  - spawns weston (nested compositor, holds sandbox Wayland scope)
  - listens on a UNIX control socket for `veracage exec` requests
  - spawns one bwrap per requested app
  - waits until either:
      (a) the user issues 'close',
      (b) all spawned apps have exited and the user enabled close-on-empty
  - tears weston down on exit

The socket lives at $XDG_RUNTIME_DIR/veracage/sessions/<vault-hash>.sock
so peer `veracage exec` invocations can find it without privileged lookup.
"""
from __future__ import annotations

import contextlib
import hashlib
import json
import os
import selectors
import signal
import socket
import subprocess
import sys
import threading
from dataclasses import dataclass, field
from pathlib import Path

from . import config
from .sandbox import bwrap_command
from .wayland import nested_weston, WestonStartFailed


# --------------------------------------------------------------- paths -----

def _sessions_dir() -> Path:
    base = Path(os.environ["XDG_RUNTIME_DIR"]) / "veracage" / "sessions"
    base.mkdir(parents=True, exist_ok=True)
    return base


def session_socket_path(vault: str) -> Path:
    h = hashlib.sha256(vault.encode()).hexdigest()[:16]
    return _sessions_dir() / f"{h}.sock"


# --------------------------------------------------------- session state ---

@dataclass
class _SessionState:
    mountpoint: str
    vault: str
    weston_socket: Path
    children: dict[int, str] = field(default_factory=dict)  # pid → app key
    closing: bool = False


# ------------------------------------------------------- protocol -----------
#
# Single line of JSON per request, single line of JSON per reply.
#
#   request:  {"cmd": "exec",   "app": "<key>"}
#             {"cmd": "list"}
#             {"cmd": "close"}
#   reply:    {"ok": true,  "pid": 12345}
#             {"ok": false, "error": "..."}
#             {"ok": true,  "apps": [...]}


def _handle_request(state: _SessionState, req: dict) -> dict:
    cmd = req.get("cmd")
    if cmd == "exec":
        key = req.get("app")
        cfg = config.load()
        app = cfg.apps.get(key)
        if app is None:
            return {"ok": False, "error": f"app '{key}' is not enabled"}
        try:
            argv = bwrap_command(state.mountpoint, app, state.weston_socket)
            proc = subprocess.Popen(argv)
        except FileNotFoundError as e:
            return {"ok": False, "error": f"missing dependency: {e.filename}"}
        state.children[proc.pid] = key
        return {"ok": True, "pid": proc.pid}

    if cmd == "list":
        return {"ok": True,
                "apps": [{"pid": p, "app": k} for p, k in state.children.items()]}

    if cmd == "close":
        state.closing = True
        return {"ok": True}

    return {"ok": False, "error": f"unknown cmd: {cmd}"}


# ----------------------------------------------------------- reaping -------

def _reap_children(state: _SessionState) -> None:
    """Non-blocking reap of exited bwrap app children.

    Only the app PIDs we track are reaped here. weston and the host agent
    are owned by their own Popen objects (nested_weston / _spawn_agent);
    reaping them with waitpid(-1) would race those owners and clobber the
    exit status they later read. We iterate a snapshot so popping is safe.
    """
    for pid in list(state.children):
        try:
            reaped, _status = os.waitpid(pid, os.WNOHANG)
        except ChildProcessError:
            # No such child — already gone. Don't leak the tracking entry.
            state.children.pop(pid, None)
            continue
        if reaped == pid:
            state.children.pop(pid, None)


# --------------------------------------------------------- session run -----

def run_session(mountpoint: str, vault: str, first_app_key: str) -> int:
    """Become the session leader. Returns the exit code."""
    cfg = config.load()
    first_app = cfg.apps.get(first_app_key)
    if first_app is None:
        print(f"veracage: app '{first_app_key}' not enabled", file=sys.stderr)
        return 2

    sock_path = session_socket_path(vault)
    if sock_path.exists():
        sock_path.unlink()

    # Quiet shutdown on Ctrl+C / SIGTERM.
    stop = threading.Event()

    def _on_signal(_signum, _frame):
        stop.set()
    signal.signal(signal.SIGINT, _on_signal)
    signal.signal(signal.SIGTERM, _on_signal)
    # SIGCHLD wakes the selector loop so we can reap.
    signal.signal(signal.SIGCHLD, lambda *_: None)

    try:
        with nested_weston() as wl_socket:
            state = _SessionState(
                mountpoint=mountpoint,
                vault=vault,
                weston_socket=wl_socket,
            )

            # Spawn the host-side agent (tray UI, drop zone, clipboard
            # bridge). Soft-fails if PySide6 is missing.
            agent_proc = _spawn_agent(vault, mountpoint, wl_socket)

            # Spawn the first app.
            first_argv = bwrap_command(mountpoint, first_app, wl_socket)
            first_proc = subprocess.Popen(first_argv)
            state.children[first_proc.pid] = first_app_key

            # Listen for exec/list/close requests.
            srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            srv.bind(str(sock_path))
            os.chmod(sock_path, 0o600)
            srv.listen(8)

            sel = selectors.DefaultSelector()
            sel.register(srv, selectors.EVENT_READ)

            try:
                while not stop.is_set():
                    _reap_children(state)
                    if state.closing or not state.children:
                        break
                    events = sel.select(timeout=1.0)
                    for key, _ in events:
                        if key.fileobj is srv:
                            _accept_one(srv, state)
            finally:
                sel.close()
                srv.close()
                with contextlib.suppress(FileNotFoundError):
                    sock_path.unlink()

            # Tell remaining children to exit; --die-with-parent already
            # covers SIGKILL of us, but on graceful close we send SIGTERM
            # so apps can flush state.
            for pid in list(state.children):
                with contextlib.suppress(ProcessLookupError):
                    os.kill(pid, signal.SIGTERM)
            for pid in list(state.children):
                with contextlib.suppress(ChildProcessError):
                    os.waitpid(pid, 0)

            # Tear down the agent.
            if agent_proc and agent_proc.poll() is None:
                with contextlib.suppress(ProcessLookupError):
                    agent_proc.terminate()
                with contextlib.suppress(subprocess.TimeoutExpired):
                    agent_proc.wait(timeout=2)
                if agent_proc.poll() is None:
                    with contextlib.suppress(ProcessLookupError):
                        agent_proc.kill()

        return 0

    except WestonStartFailed as e:
        print(f"veracage: weston failed to start: {e}", file=sys.stderr)
        print("(install with: sudo apt install weston)", file=sys.stderr)
        return 1


def _spawn_agent(vault: str, mountpoint: str, weston_socket: Path):
    """Start the host-side Qt agent as a sibling subprocess.

    Returns the Popen, or None if the entry script can't be found.
    """
    entry = os.environ.get("VERACAGE_ENTRY") or _find_entry_script()
    if entry is None:
        print("veracage: agent entry script not found; UI disabled.",
              file=sys.stderr)
        return None
    return subprocess.Popen([
        entry, "_agent",
        "--vault", vault,
        "--mountpoint", mountpoint,
        "--weston-socket", str(weston_socket),
    ])


def _find_entry_script() -> str | None:
    # session.py lives at src/veracage/session.py → src/bin/veracage
    here = Path(__file__).resolve()
    cand = here.parent.parent / "bin" / "veracage"
    return str(cand) if cand.is_file() else None


def _accept_one(srv: socket.socket, state: _SessionState) -> None:
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
            req = json.loads(data.decode().strip() or "{}")
        except (ValueError, OSError) as e:
            conn.sendall((json.dumps({"ok": False, "error": str(e)}) + "\n").encode())
            return

        reply = _handle_request(state, req)
        conn.sendall((json.dumps(reply) + "\n").encode())


# ---------------------------------------------------- client (`veracage exec`)

def send_request(vault: str, request: dict) -> dict:
    """Connect to the running session for `vault` and send a single request.

    Raises FileNotFoundError if no session is running.
    """
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
