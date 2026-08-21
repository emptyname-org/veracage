"""Veracage suspend teardown - the logic behind the systemd system-sleep hook.

Installed as `/usr/lib/systemd/system-sleep/veracage` (a thin shim generated
from `install/veracage-sleep.in`). systemd runs it as root around every sleep
transition with `argv = [pre|post, suspend|hibernate|hybrid-sleep|
suspend-then-hibernate]` and **blocks the transition until the `pre` invocation
returns** - so, unlike a D-Bus `PrepareForSleep` inhibitor, teardown is
guaranteed to complete before the machine sleeps with no inhibitor to acquire.
This replaces the old `agent-rs/src/suspend.rs` zbus watcher, whose dismount
silently stopped happening whenever `Inhibit` was unavailable.

On `pre`, every active session's dm-crypt key must leave RAM.

Shared-workspace sessions (`/run/veracage/session-<sid>.lock` - what every
`veracage open` creates today):

  1. Skip if the session owner set `suspend_action = "ignore"`.
  2. SIGTERM the session leader (pid + start-time from `session-<sid>.pid`,
     the helper's own verified record): it terminates its apps, exits, its
     private mount NS (all `/vaults/*` mounts) dies with it, and the helper
     parent / ExecStopPost `cryptsetup close`s every volume's dm device.
  3. Wait for every dm named in the lock to disappear. Past the budget, force:
     SIGKILL the leader and run `cleanup.cleanup_session` directly, so we never
     return to systemd with a key still resident.

"""
from __future__ import annotations

import contextlib
import os
import signal
import stat
import sys
import time
import tomllib
from pathlib import Path

from . import cleanup, leader

# systemd passes the transition name as argv[1]; these are the sleep states.
SLEEP_STATES = frozenset(
    {"suspend", "hibernate", "hybrid-sleep", "suspend-then-hibernate"}
)

# Per-session teardown budget (seconds). Graceful window first, then force.
# The graceful window must OUTLAST the leader's own app shutdown budget
# (leader.TERMINATE_GRACE): the leader SIGTERMs its apps, waits that long for
# them to flush, and only then exits and lets the dm devices close. SIGKILLing
# it sooner (6s against the leader's 8s) killed the apps mid-write and left the
# filesystem dirty, which is what the leader's grace exists to prevent.
GRACE_SECONDS = leader.TERMINATE_GRACE + 1.0
FORCE_SECONDS = 4.0
POLL_INTERVAL = 0.1

# Most of a config.toml is comments and a handful of keys; anything past this
# is not something this hook needs to read as root.
_CONFIG_READ_CAP = 64 * 1024



def owner_wants_dismount(uid: str) -> bool:
    """True unless the session owner's config sets suspend_action = "ignore".

    Reads the owner's `~/.config/veracage/config.toml` (default location; a
    root hook can't know a per-user XDG_CONFIG_HOME). Any error → dismount,
    the secure default. suspend_action is a `[default]`-level setting.
    """
    if not uid.isdigit():
        return True
    try:
        import pwd
        home = Path(pwd.getpwuid(int(uid)).pw_dir)
    except (KeyError, ValueError):
        return True
    cfg = home / ".config" / "veracage" / "config.toml"
    # Root, reading a path the session owner controls, inside a hook that
    # BLOCKS the sleep transition (systemd-suspend.service has no start
    # timeout). So: no symlink following, a regular file only, non-blocking
    # (a FIFO there would otherwise park the hook, and the machine, forever),
    # and a size cap.
    try:
        fd = os.open(cfg, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    except OSError:
        return True
    try:
        if not stat.S_ISREG(os.fstat(fd).st_mode):
            print(f"veracage-sleep: {cfg} is not a regular file; closing anyway",
                  file=sys.stderr)
            return True
        raw = os.read(fd, _CONFIG_READ_CAP)
    except OSError:
        return True
    finally:
        os.close(fd)
    try:
        data = tomllib.loads(raw.decode())
    except (UnicodeDecodeError, tomllib.TOMLDecodeError):
        return True
    action = data.get("default", {}).get("suspend_action", "dismount")
    return action != "ignore"


def dm_present(dm_name: str) -> bool:
    return bool(dm_name) and Path(f"/dev/mapper/{dm_name}").exists()


def pid_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True  # exists but not signalable (won't happen as root)
    return True


def _wait_dms_gone(dm_names: list[str], timeout: float) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if not any(dm_present(dm) for dm in dm_names):
            return True
        time.sleep(POLL_INTERVAL)
    return not any(dm_present(dm) for dm in dm_names)


def _session_leader_pid(lock_path: Path) -> int | None:
    """The session leader's pid from `session-<sid>.pid`, verified against its
    recorded start-time (the helper's own pid-reuse defence). None if absent,
    malformed, or no longer naming the same live process."""
    pid_path = lock_path.with_suffix(".pid")
    if not cleanup.session_leader_alive(pid_path):
        return None
    try:
        first = pid_path.read_text().split()[0]
        return int(first)
    except (OSError, ValueError, IndexError):
        return None


def teardown_shared_session(lock_path: Path) -> None:
    """Tear down a shared-workspace session (`session-<sid>.lock`, possibly
    several volumes) so every dm-crypt key leaves RAM. Never raises."""
    try:
        owner, volumes = cleanup.parse_session_lock(lock_path)
    except OSError as e:
        print(f"veracage-sleep: cannot read {lock_path}: {e}", file=sys.stderr)
        return

    if not owner_wants_dismount(owner):
        print(f"veracage-sleep: {lock_path.stem}: suspend_action=ignore, "
              "leaving mounted.", file=sys.stderr)
        return

    dm_names = [dm for dm, _label in volumes if dm_present(dm)]
    if not dm_names:
        return  # nothing open; nothing to do

    leader = _session_leader_pid(lock_path)

    # Graceful: SIGTERM the leader → apps terminated, workspace NS destroyed,
    # the helper parent / ExecStopPost closes every volume's dm. With no
    # leader to signal there is nothing to be graceful about: skip straight
    # to closing the devices ourselves rather than burn the whole budget
    # waiting for an exit that already happened.
    if leader is not None:
        with contextlib.suppress(ProcessLookupError):
            os.kill(leader, signal.SIGTERM)
        if _wait_dms_gone(dm_names, GRACE_SECONDS):
            return

    # Force: SIGKILL the leader (bwrap --die-with-parent takes the apps, freeing
    # the mounts), then close the devices ourselves via the cleanup path.
    if leader is not None and pid_alive(leader):
        with contextlib.suppress(ProcessLookupError):
            os.kill(leader, signal.SIGKILL)
    _wait_dms_gone(dm_names, FORCE_SECONDS)
    cleanup.cleanup_session(lock_path)  # close every dm + tidy (idempotent)

    still = [dm for dm in dm_names if dm_present(dm)]
    if still:
        print(f"veracage-sleep: WARNING {lock_path.stem}: dm {', '.join(still)} "
              "still present after force teardown. Key may remain in RAM.",
              file=sys.stderr)


def main(argv: list[str] | None = None) -> int:
    argv = list(sys.argv[1:] if argv is None else argv)
    phase = argv[0] if argv else ""
    state = argv[1] if len(argv) > 1 else ""

    # We only tear down going INTO a sleep state. `post` (resume) is a no-op:
    # the user re-opens vaults after waking.
    if phase != "pre" or state not in SLEEP_STATES:
        return 0

    if os.geteuid() != 0:
        print("veracage-sleep: must run as root", file=sys.stderr)
        return 0  # non-fatal: never block the sleep transition

    locks_dir = cleanup.LOCKS_DIR
    try:
        locks = sorted(locks_dir.glob("*.lock"))
    except OSError:
        return 0
    for lock in locks:
        # Only session locks exist (the helper writes nothing else here); skip
        # any other *.lock rather than guess at a foreign format.
        if not lock.name.startswith("session-"):
            continue
        try:
            teardown_shared_session(lock)
        except Exception as e:  # noqa: BLE001 - a hook must never abort a sleep
            print(f"veracage-sleep: {lock.stem}: {e}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
