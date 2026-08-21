"""Veracage session cleanup.

Reads `/run/veracage/session-<sid>.lock`, closes every dm-crypt device named
in it, removes the session's scratch, deletes the lock. Idempotent.

Invoked as root via pkexec (see `helpers/veracage-cleanup`) by the systemd
transient unit's `ExecStopPost` (`veracage-cleanup --session <sid>`), which
fires on any exit: graceful, crash, SIGKILL, OOM, logout.

The dm-crypt device names follow `veracage-<12-hex>`. Anything else in the
lock file is ignored. The function refuses to act on `dm_name` values that
don't match this pattern, so a corrupted lock file can't make us close
arbitrary dm devices.
"""
from __future__ import annotations

import argparse
import contextlib
import fcntl
import hashlib
import os
import re
import shutil
import stat
import subprocess
import sys
import time
from pathlib import Path

# The session id is the human uid (cli.py passes str(os.getuid())). Must accept
# exactly what the Rust helper's session_id_ok accepts (1-16 ASCII digits). The
# two sides validate the SAME id, and a mismatch would dead-letter the
# ExecStopPost crash teardown.
SID_RE = re.compile(r"^[0-9]{1,16}$")

LOCKS_DIR = Path("/run/veracage")
SESSIONS_BASE = Path("/run/user")   # <base>/<uid>/veracage/sessions/<hash>.sock
DM_NAME_RE = re.compile(r"^veracage-[0-9a-f]{12}$")


def vault_hash(vault: str) -> str:
    """16-hex-char hash used to name a vault's control socket."""
    return hashlib.sha256(vault.encode()).hexdigest()[:16]


# --------------------------------------------------------------- session ----
# The shared-workspace model (docs/shared-workspace.md): ONE session
# holds every open volume in a single private mount namespace (the leader). The
# vault MOUNTS live in that NS's tmpfs and vanish when the leader dies, so
# session teardown only has to `cryptsetup close` each volume's dm device (dm
# devices are global, not NS-scoped). The session lock records every open volume
# so a crash still closes them all:
#
#     user_uid=1000
#     generation=<16hex>
#     volume=veracage-<12hex>\t<label>\t<source hash>\t<dev>:<ino>
#     volume=veracage-<12hex>\t<label>\t<source hash>\t<dev>:<ino>
#
# Only the first two fields matter here; the rest are the helper's own keys for
# the duplicate-open guard and are ignored (a line written by an older helper
# carries fewer fields and must still parse).
#
# `session-<sid>.lock`, root-owned 0600. `<sid>` is the human uid (digits).

def session_lock_path(sid: str) -> Path:
    return LOCKS_DIR / f"session-{sid}.lock"


def parse_session_lock(p: Path) -> tuple[str, list[tuple[str, str]]]:
    """Return (user_uid, [(dm_name, label), ...]) from a session lock. Malformed
    `volume=` lines are skipped; the caller validates each dm_name before acting.
    Volume lines may carry extra tab-separated fields after the label (the helper
    records the source vault's hash there); they are ignored here."""
    user_uid = ""
    volumes: list[tuple[str, str]] = []
    for line in p.read_text().splitlines():
        k, _, v = line.partition("=")
        k = k.strip()
        if k == "user_uid":
            user_uid = v.strip()
        elif k == "volume":
            fields = v.split("\t")
            dm = fields[0].strip()
            label = fields[1] if len(fields) > 1 else ""
            if dm:
                volumes.append((dm, label))
    return user_uid, volumes


def _proc_starttime(pid: int) -> str | None:
    """`/proc/<pid>/stat` field 22 (start-time), or None if the process is gone
    or a ZOMBIE. comm (field 2) is parenthesised and may contain spaces, so
    split after the last ')': the remaining fields start at field 3, so state
    is index 0 and start-time is index 19.

    The zombie test is what makes this usable as a liveness check. A leader
    that was SIGKILLed but not yet reaped keeps its /proc entry with an
    unchanged start-time, and reading that as "still running" makes the
    teardown a no-op - including on the suspend hook's force path, which
    calls us moments after sending that very SIGKILL. Mirrors
    wayland._pid_alive."""
    try:
        data = Path(f"/proc/{pid}/stat").read_text()
    except OSError:
        return None
    rest = data[data.rfind(")") + 1:].split()
    if len(rest) <= 19 or rest[0] == "Z":
        return None
    return rest[19]


def session_leader_alive(pid_path: Path) -> bool:
    """True if `pid_path` (`session-<sid>.pid`, "pid\\nstarttime") names a process
    still alive with the SAME start-time (defeats pid reuse), i.e. the session
    leader is still running."""
    try:
        parts = pid_path.read_text().split()
    except OSError:
        return False
    if not parts or not parts[0].isdigit():
        return False
    st = _proc_starttime(int(parts[0]))
    if st is None:
        return False
    return len(parts) < 2 or st == parts[1]


def _remove_stale_session_sockets(owner: str) -> None:
    """Remove leftover control sockets in the owner's sessions dir. The helper
    keys the session's control socket by the BOOTSTRAP VAULT's hash (not the
    session id), which the session lock does not record, but there is exactly
    one session per uid, so once that session is dead every socket in the dir is
    stale.

    Runs as ROOT under a tree the caller owns, so the path is never resolved
    twice: each component is opened with O_NOFOLLOW relative to the previous
    directory fd, and the unlink is done with `dir_fd`. Resolving the path,
    checking it, and then globbing it again let the caller swap a component for a
    symlink in between and have root unlink sockets anywhere on the system, and
    the cleanup action is passwordless so it could be retried until it won."""
    parts = [c for c in SESSIONS_BASE.parts if c != os.sep]
    parts += [owner, "veracage", "sessions"]
    try:
        fd = os.open(os.sep if SESSIONS_BASE.is_absolute() else ".", os.O_RDONLY | os.O_DIRECTORY)
    except OSError:
        return
    try:
        for comp in parts:
            try:
                nxt = os.open(comp, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=fd)
            except OSError:
                return  # gone, or a symlinked component: refuse either way
            os.close(fd)
            fd = nxt
        for name in os.listdir(fd):
            if not name.endswith(".sock"):
                continue
            try:
                if stat.S_ISSOCK(os.lstat(name, dir_fd=fd).st_mode):
                    os.unlink(name, dir_fd=fd)
            except OSError:
                continue
    finally:
        with contextlib.suppress(OSError):
            os.close(fd)


# How long to keep retrying a `cryptsetup close` that reports the device busy,
# and how long to wait between tries. This teardown runs from the unit's
# ExecStopPost, i.e. moments after the session leader exited, and the kernel
# releases the leader's mount namespace (and with it the last holder of the
# filesystem) asynchronously. A single attempt therefore loses a race it only has
# to wait out - and losing it leaves the volume open with its key still in RAM.
CLOSE_RETRY_FOR = 5.0
CLOSE_RETRY_EVERY = 0.2


def close_dm(dm_name: str) -> str | None:
    """`cryptsetup close` a dm device, retrying while it reports busy. Returns
    None once the device is gone (including "it was never there"), else the last
    error text. The caller must have validated `dm_name`."""
    deadline = time.monotonic() + CLOSE_RETRY_FOR
    while True:
        if not Path(f"/dev/mapper/{dm_name}").exists():
            return None
        try:
            r = subprocess.run(["cryptsetup", "close", dm_name],
                               capture_output=True, text=True)
        except OSError as e:
            # Fork failure under the memory pressure that just OOM-killed the
            # leader, or cryptsetup missing from pkexec's PATH. Report it as
            # this device's error: raising here would abandon every volume
            # after it, each one still decrypted.
            return str(e)
        if r.returncode == 0:
            return None
        err = r.stderr.strip() or f"exit {r.returncode}"
        if time.monotonic() >= deadline:
            return err
        time.sleep(CLOSE_RETRY_EVERY)


# How long to wait for the session flock before cleaning up WITHOUT it. The wait
# is bounded because this runs from the unit's ExecStopPost, which systemd kills
# at the unit's stop timeout (90s by default), and because a close-volume helper
# parked on a human answer holds that same flock for up to five minutes. Blocking
# here would mean the dm devices are never closed at all, which is strictly worse
# than racing a successor session: this is the last thing standing between a
# decrypted device and a machine that believes it closed it.
FLOCK_WAIT = 2.0
FLOCK_RETRY_EVERY = 0.05


def _take_flock(path: Path):
    """Take the session flock, waiting at most FLOCK_WAIT. Returns the open file
    (locked or not) so the caller can hold it for the teardown, or None if it
    could not be opened at all."""
    try:
        f = open(path, "w")
    except OSError:
        return None
    deadline = time.monotonic() + FLOCK_WAIT
    while True:
        try:
            fcntl.flock(f, fcntl.LOCK_EX | fcntl.LOCK_NB)
            return f
        except OSError:
            if time.monotonic() >= deadline:
                print(f"veracage-cleanup: {path.name} is held by another Veracage "
                      "process; closing the volumes anyway", file=sys.stderr)
                return f
            time.sleep(FLOCK_RETRY_EVERY)


def cleanup_session(p: Path) -> int:
    """Close EVERY volume's dm device named in a session lock, then unlink the
    lock + control socket. The vault mounts died with the leader's namespace, so
    there is no per-mountpoint tidy: only the global dm devices survive a crash
    and must be closed. Idempotent. Reachable passwordless via polkit, so it
    refuses a symlinked or non-regular lock and a non-owner caller."""
    try:
        st = p.lstat()
    except OSError:
        return 0  # already gone
    if stat.S_ISLNK(st.st_mode) or not stat.S_ISREG(st.st_mode):
        print(f"veracage-cleanup: refusing non-regular lock {p}", file=sys.stderr)
        return 2
    # Serialize with the helper's open/teardown paths (they flock the same
    # sidecar, `session-<sid>.flock`): without this, a cleanup firing while a
    # successor session bootstraps the same sid could close a dm the successor
    # just opened, or delete its freshly written lock.
    _lockf = _take_flock(p.with_suffix(".flock"))
    try:
        return _cleanup_session_locked(p)
    finally:
        if _lockf is not None:
            _lockf.close()


def _cleanup_session_locked(p: Path) -> int:
    try:
        owner, volumes = parse_session_lock(p)
    except OSError as e:
        print(f"veracage-cleanup: cannot read {p}: {e}", file=sys.stderr)
        return 1

    # Ownership gate, fail-CLOSED: the cleanup polkit action is passwordless,
    # so refuse a pkexec caller who isn't the recorded owner, INCLUDING a
    # header-less lock (empty owner): the helper always writes user_uid, so an
    # owner-less lock is corrupt or forged. PKEXEC_UID absent = the root helper
    # invoking us directly (same trust domain), which is allowed.
    caller = os.environ.get("PKEXEC_UID", "")
    if caller and caller != owner:
        print(f"veracage-cleanup: uid {caller} does not own session {p.stem} "
              f"(owner uid {owner or 'unset'}), refusing", file=sys.stderr)
        return 2

    # Liveness guard: if the session leader is still running, this teardown was
    # triggered by a SUBSIDIARY (add-volume) unit stopping, not the session
    # ending, so it must be a no-op. Only when the leader is gone do we close the
    # volumes. This is what makes it safe for every `veracage open` to carry the
    # same `ExecStopPost=cleanup --session <sid>` (the shared-workspace model).
    if session_leader_alive(p.with_suffix(".pid")):
        return 0

    rc = 0
    for dm_name, _label in volumes:
        if not DM_NAME_RE.match(dm_name):
            print(f"veracage-cleanup: refusing dm_name={dm_name!r} "
                  f"(does not match veracage-<12-hex>)", file=sys.stderr)
            rc = 2
            continue
        err = close_dm(dm_name)
        if err is not None:
            print(f"veracage-cleanup: cryptsetup close {dm_name}: {err}",
                  file=sys.stderr)
            rc = 1

    # Only drop the lock + socket once every device is actually closed: a failed
    # close (EBUSY) means the session may still be live; leave the lock as a
    # recovery trail rather than orphan a running dm device.
    if rc == 0:
        if owner.isdigit():
            # The control socket is keyed by the bootstrap vault's hash (see
            # helper-rs run_session_bootstrap), NOT by the session id: sweep the
            # sessions dir rather than guess a name that never existed.
            _remove_stale_session_sockets(owner)
        # Drop the session pidfile and the host-side vault-runtime scratch too
        # (the workspace tmpfs itself died with the leader's namespace). The
        # `.x` exchange-mount target is an empty host-visible dir by now: its
        # idmap mount was NS-private; rmdir (never rmtree) in case it isn't.
        with contextlib.suppress(OSError):
            p.with_suffix(".pid").unlink()
        # The `.run` tree is veracage-owned, so a compromised sandbox app could
        # plant symlinks inside it: only remove it via the stdlib's fd-based
        # symlink-attack-resistant walk. Leave it otherwise: it holds no
        # plaintext (the key left with the closed dm).
        run_dir = p.with_suffix(".run")
        if not run_dir.is_symlink() and shutil.rmtree.avoids_symlink_attacks:
            with contextlib.suppress(OSError):
                shutil.rmtree(run_dir, ignore_errors=True)
        with contextlib.suppress(OSError):
            p.with_suffix(".x").rmdir()
        with contextlib.suppress(FileNotFoundError):
            p.unlink()
    return rc


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(prog="veracage-cleanup")
    # Only a validated id, never a caller-supplied path. The action is
    # passwordless (polkit allow_active=yes), so an arbitrary --lock path would
    # be a root file-delete primitive.
    p.add_argument("--session", required=True,
                   help="numeric session id, the human uid "
                        "(cleans /run/veracage/session-<sid>.lock)")
    args = p.parse_args(argv)

    if os.geteuid() != 0:
        print("veracage-cleanup: must run as root (via pkexec)", file=sys.stderr)
        return 2

    if not SID_RE.match(args.session):
        print(f"veracage-cleanup: invalid session id {args.session!r}",
              file=sys.stderr)
        return 2
    return cleanup_session(session_lock_path(args.session))


if __name__ == "__main__":
    sys.exit(main())
