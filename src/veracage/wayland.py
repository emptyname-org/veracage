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
import os
import secrets
import signal
import subprocess
import time
from pathlib import Path


WESTON_STARTUP_TIMEOUT_S = 5.0


class WestonStartFailed(RuntimeError):
    pass


@contextlib.contextmanager
def nested_weston():
    """Spawn a nested Weston, yield the host-visible socket path, kill on exit."""
    runtime = Path(os.environ["XDG_RUNTIME_DIR"])
    socket_name = f"veracage-{secrets.token_hex(4)}"
    socket_path = runtime / socket_name

    # We rely on Weston's auto-detection: with WAYLAND_DISPLAY set in env,
    # it picks the wayland-backend (nests inside the host compositor).
    proc = subprocess.Popen(
        [
            "weston",
            f"--socket={socket_name}",
            # Run a no-op shell so weston doesn't auto-launch a terminal;
            # our bwrap'd app connects to the socket directly.
            "--shell=desktop-shell.so",
        ],
        # Detach signals so Ctrl+C in the launcher kills the whole tree
        # rather than just the launcher.
        preexec_fn=os.setsid,
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
