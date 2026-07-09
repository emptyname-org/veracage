"""Veracage suspend teardown — the logic behind the systemd system-sleep hook.

Installed as `/usr/lib/systemd/system-sleep/veracage` (a thin shim generated
from `install/veracage-sleep.in`). systemd runs it as root around every sleep
transition with `argv = [pre|post, suspend|hibernate|hybrid-sleep|
suspend-then-hibernate]` and **blocks the transition until the `pre` invocation
returns** — so, unlike a D-Bus `PrepareForSleep` inhibitor, teardown is
guaranteed to complete before the machine sleeps with no inhibitor to acquire.
This replaces the old `agent-rs/src/suspend.rs` zbus watcher, whose dismount
silently stopped happening whenever `Inhibit` was unavailable.

On `pre`, every active session's dm-crypt key must leave RAM. For each
`/run/veracage/<hash>.lock`:

  1. Skip if the session owner set `suspend_action = "ignore"` (they opted to
     keep the vault mounted across sleep).
  2. SIGTERM the session leader: it terminates its apps, exits, and its private
     mount namespace is destroyed (freeing the vault mount); the leader's
     systemd unit then stops and its `ExecStopPost` runs `veracage-cleanup`,
     which `cryptsetup close`s the device.
  3. Wait for the dm device to disappear. If it lingers past the budget, force
     it: SIGKILL the leader (bwrap `--die-with-parent` takes the apps with it,
     tearing down the namespace) and run the cleanup close directly, so we never
     return to systemd with a key still resident.
"""
from __future__ import annotations

import os
import signal
import sys
import time
import tomllib
from pathlib import Path

from . import cleanup

# systemd passes the transition name as argv[1]; these are the sleep states.
SLEEP_STATES = frozenset(
    {"suspend", "hibernate", "hybrid-sleep", "suspend-then-hibernate"}
)

# Per-session teardown budget (seconds). Graceful window first, then force.
GRACE_SECONDS = 6.0
FORCE_SECONDS = 4.0
POLL_INTERVAL = 0.1

PROC = Path("/proc")


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
    try:
        with cfg.open("rb") as f:
            data = tomllib.load(f)
    except (OSError, tomllib.TOMLDecodeError):
        return True
    action = data.get("default", {}).get("suspend_action", "dismount")
    return action != "ignore"


def _leader_uid() -> int | None:
    """The uid the real session leader runs as — the `veracage` system user.

    The leader is spawned by the root helper, which drops to this uid; a same-uid
    (human) attacker cannot run a process as it. Requiring the /proc match to be
    owned by it defeats a decoy that forges a `_leader` argv to misdirect the
    kill. Returns None only if the user can't be resolved (an unconfigured box) —
    then we fall back to the cmdline-only match rather than never tearing down.
    """
    try:
        import pwd
        return pwd.getpwnam("veracage").pw_uid
    except KeyError:
        return None


def find_leader_pid(mountpoint: str) -> int | None:
    """The `veracage _leader --mountpoint <mountpoint>` process, or None.

    Matches on `/proc/<pid>/cmdline` (the mountpoint is a per-session random
    path) AND requires the process to be owned by the veracage uid — so a
    same-uid attacker's forged-argv decoy is rejected before we signal it.
    """
    leader_uid = _leader_uid()
    for entry in PROC.iterdir():
        if not entry.name.isdigit():
            continue
        # Reject decoys: only a veracage-uid process can be the real leader.
        if leader_uid is not None:
            try:
                if entry.stat().st_uid != leader_uid:
                    continue
            except OSError:
                continue  # pid vanished
        try:
            argv = (entry / "cmdline").read_bytes().split(b"\0")
        except OSError:
            continue  # pid vanished or not ours to read
        if b"_leader" in argv and mountpoint.encode() in argv:
            return int(entry.name)
    return None


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


def _wait_dm_gone(dm_name: str, timeout: float) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if not dm_present(dm_name):
            return True
        time.sleep(POLL_INTERVAL)
    return not dm_present(dm_name)


def teardown_session(lock_path: Path) -> None:
    """Tear one session down so its dm-crypt key leaves RAM. Never raises."""
    try:
        fields = cleanup.parse_lock(lock_path)
    except OSError as e:
        print(f"veracage-sleep: cannot read {lock_path}: {e}", file=sys.stderr)
        return

    dm_name = fields.get("dm_name", "")
    mountpoint = fields.get("mountpoint", "")
    uid = fields.get("user_uid", "")

    if not owner_wants_dismount(uid):
        print(f"veracage-sleep: {lock_path.stem}: suspend_action=ignore; "
              "leaving mounted.", file=sys.stderr)
        return

    if not dm_present(dm_name):
        return  # already gone; nothing to do

    leader = find_leader_pid(mountpoint) if mountpoint else None

    # Graceful: SIGTERM the leader → apps terminated, NS destroyed, unit stops,
    # ExecStopPost closes the device.
    if leader is not None:
        try:
            os.kill(leader, signal.SIGTERM)
        except ProcessLookupError:
            pass
    if _wait_dm_gone(dm_name, GRACE_SECONDS):
        return

    # Force: SIGKILL the leader (bwrap --die-with-parent takes the apps, freeing
    # the mount), then close the device ourselves via the cleanup path.
    if leader is not None and pid_alive(leader):
        try:
            os.kill(leader, signal.SIGKILL)
        except ProcessLookupError:
            pass
    _wait_dm_gone(dm_name, FORCE_SECONDS)
    cleanup.cleanup_one(lock_path)  # cryptsetup close + tidy (idempotent)

    if dm_present(dm_name):
        print(f"veracage-sleep: WARNING {lock_path.stem}: dm {dm_name} still "
              "present after force teardown; key may remain in RAM.",
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
        try:
            teardown_session(lock)
        except Exception as e:  # noqa: BLE001 — a hook must never abort a sleep
            print(f"veracage-sleep: {lock.stem}: {e}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
