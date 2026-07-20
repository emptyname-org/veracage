"""Lifecycle of the ONE persistent nested compositor (Phase 2).

A single `veracage`-uid compositor renders every vault's apps into one host
window, owns the clipboard, and (Phase 3) hosts the toolbar. It is brought up by
the privileged helper (`veracage-helper --spawn-compositor`) on a fixed socket
under `/run/veracage`; the CLI and each vault leader here only *observe* its
liveness and wait for it to appear. There is no longer a per-vault compositor to
spawn - the old `nested_compositor` context manager is gone.

  * sandbox clipboard ≠ host clipboard  (the compositor bridges them in-process)
  * host clipboard managers (Klipper, GPaste) cannot scrape the sandbox
  * screencopy / virtual-input from the sandbox cannot affect the host

We spawn our own compositor rather than a third-party one (weston/sway/…) so the
component facing the possibly-hostile app is ours, pinned and known, and carries
zero extra runtime dependency for the end user.
"""
from __future__ import annotations

import time
from pathlib import Path

COMPOSITOR_STARTUP_TIMEOUT_S = 5.0

# The ONE persistent compositor's runtime dir + fixed paths. MUST match
# COMPOSITOR_RUNTIME in helper-rs/src/main.rs. `rt` is veracage-owned mode 0711:
# the human can traverse it to stat the socket/pidfile for a liveness check, but
# only veracage-uid apps can connect to the socket.
COMPOSITOR_RUNTIME = Path("/run/veracage/rt")
COMPOSITOR_SOCKET = COMPOSITOR_RUNTIME / "wl-vc"
COMPOSITOR_PIDFILE = COMPOSITOR_RUNTIME / "compositor.pid"


class CompositorStartFailed(RuntimeError):
    pass


def compositor_pid() -> int | None:
    """The pid the helper recorded when it brought the compositor up (the
    compositor's own pid - the helper execs it, so the pid survives), or None if
    there is no readable pidfile."""
    try:
        return int(COMPOSITOR_PIDFILE.read_text().strip())
    except (OSError, ValueError):
        return None


def _pid_alive(pid: int) -> bool:
    """True iff pid exists and is not a zombie. A compositor that exited (e.g. its
    window was closed) but has not yet been reaped by its parent keeps a /proc
    entry in state Z - functionally dead, so treat it as down."""
    try:
        stat = Path(f"/proc/{pid}/stat").read_text()
    except OSError:
        return False
    # "pid (comm) STATE ..." - comm may contain ')', so scan past the last one.
    try:
        state = stat[stat.rindex(")") + 1:].split()[0]
    except (ValueError, IndexError):
        return False
    return state != "Z"


def compositor_is_up() -> bool:
    """True iff the persistent compositor is running AND its socket is present.
    Both are required so a stale pidfile (a recycled pid) or a half-started
    compositor reads as *down* and triggers a clean respawn."""
    pid = compositor_pid()
    return pid is not None and _pid_alive(pid) and COMPOSITOR_SOCKET.exists()


def wait_for_compositor(timeout: float = COMPOSITOR_STARTUP_TIMEOUT_S) -> None:
    """Block until the compositor is up, else raise CompositorStartFailed."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if compositor_is_up():
            return
        time.sleep(0.05)
    raise CompositorStartFailed(
        f"compositor socket {COMPOSITOR_SOCKET} did not appear within {timeout}s"
    )
