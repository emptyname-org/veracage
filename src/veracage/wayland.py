"""Wayland isolation for the sandbox.

Slice 2: Mode B only — a nested `weston` running as a Wayland client of the
host compositor. Weston creates its own Wayland socket; the sandboxed app
connects to that socket instead of the host's, so:

  * sandbox clipboard ≠ host clipboard
  * host clipboard managers (Klipper, GPaste) cannot scrape the sandbox
  * screencopy / virtual-input from the sandbox cannot affect the host

Mode A (`wp-security-context-v1`) is deferred until the dev box runs a
compositor that supports it (KWin ≥ 6, Mutter ≥ 47, sway ≥ 1.10).
"""
from __future__ import annotations

import contextlib
import ctypes
import os
import secrets
import signal
import subprocess
import time
from pathlib import Path

WESTON_STARTUP_TIMEOUT_S = 5.0


class WestonStartFailed(RuntimeError):
    pass


def _weston_preexec() -> None:  # pragma: no cover - runs post-fork in the child
    """New session (so the launcher's Ctrl+C doesn't hit weston directly), plus
    PR_SET_PDEATHSIG=SIGKILL so weston dies if the session leader is killed —
    otherwise an orphaned weston keeps the scope cgroup alive and blocks the
    crash-safe ExecStopPost cleanup."""
    os.setsid()
    ctypes.CDLL("libc.so.6", use_errno=True).prctl(1, signal.SIGKILL, 0, 0, 0)
    if os.getppid() == 1:  # leader already died before prctl took effect
        os._exit(0)


def _weston_invocation(socket_name: str, upstream_fd: int | None,
                       runtime: Path) -> tuple[list[str], dict[str, str], tuple[int, ...]]:
    """Build (argv, env, pass_fds) for a nested weston. Pure, so it's testable.

    With `upstream_fd` set, weston connects to the host compositor via the
    inherited fd (WAYLAND_SOCKET) and the wayland backend — the deny-by-UID
    leader uses this because the vault uid can't reach the host's runtime dir by
    path. Without it, weston auto-detects via WAYLAND_DISPLAY (option-a).
    """
    env = {**os.environ, "XDG_RUNTIME_DIR": str(runtime)}
    # Run a no-op shell so weston doesn't auto-launch a terminal; our bwrap'd
    # app connects to the socket directly.
    argv = ["weston", f"--socket={socket_name}", "--shell=desktop-shell.so"]
    pass_fds: tuple[int, ...] = ()
    if upstream_fd is not None:
        argv.insert(1, "--backend=wayland-backend.so")
        env["WAYLAND_SOCKET"] = str(upstream_fd)
        env.pop("WAYLAND_DISPLAY", None)  # force the fd, not a path lookup
        pass_fds = (upstream_fd,)
    return argv, env, pass_fds


@contextlib.contextmanager
def nested_weston(upstream_fd: int | None = None):
    """Spawn a nested Weston, yield the host-visible socket path, kill on exit.

    `upstream_fd` (the leader's inherited connection to the host compositor) is
    passed to weston as WAYLAND_SOCKET; without it weston auto-detects via
    WAYLAND_DISPLAY.
    """
    runtime = Path(os.environ["XDG_RUNTIME_DIR"])
    socket_name = f"veracage-{secrets.token_hex(4)}"
    socket_path = runtime / socket_name

    argv, env, pass_fds = _weston_invocation(socket_name, upstream_fd, runtime)
    proc = subprocess.Popen(
        argv,
        env=env,
        pass_fds=pass_fds,
        # New session (Ctrl+C in the launcher doesn't hit weston directly) +
        # die-with-leader via PR_SET_PDEATHSIG (see _weston_preexec).
        preexec_fn=_weston_preexec,
        # Close stdin so weston doesn't read from the user's terminal.
        stdin=subprocess.DEVNULL,
    )

    try:
        deadline = time.monotonic() + WESTON_STARTUP_TIMEOUT_S
        while time.monotonic() < deadline:
            if socket_path.exists():
                break
            if proc.poll() is not None:
                raise WestonStartFailed(
                    f"weston exited early with rc={proc.returncode}"
                )
            time.sleep(0.05)
        else:
            raise WestonStartFailed(
                f"weston socket {socket_path} did not appear within "
                f"{WESTON_STARTUP_TIMEOUT_S}s"
            )

        yield socket_path

    finally:
        if proc.poll() is None:
            try:
                os.killpg(proc.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                proc.wait(timeout=3)
            except subprocess.TimeoutExpired:
                with contextlib.suppress(ProcessLookupError):
                    os.killpg(proc.pid, signal.SIGKILL)
                proc.wait()
        # Socket file is removed by weston on clean exit; if not, tidy up.
        with contextlib.suppress(FileNotFoundError):
            socket_path.unlink()
