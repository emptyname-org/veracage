"""Veracage session cleanup.

Reads `/run/veracage/<vault-hash>.lock`, closes the dm-crypt device named
in it, removes the orphan mountpoint dir, deletes the lock. Idempotent.

Invoked as root via pkexec (see `helpers/veracage-cleanup`). Triggered
two ways:
  1. Normal exit — the privileged helper invokes this directly.
  2. Crash / SIGKILL — the systemd transient scope's `ExecStopPost` runs
     `pkexec veracage-cleanup --vault-hash <hash>` after the launcher dies.

The dm-crypt device name follows `veracage-<12-hex>`. Anything else in
the lock file is ignored. The function refuses to act on `dm_name`
values that don't match this pattern, so a corrupted lock file can't
make us close arbitrary dm devices.
"""
from __future__ import annotations

import argparse
import contextlib
import hashlib
import os
import re
import shutil
import stat
import subprocess
import sys
from pathlib import Path

VAULT_HASH_RE = re.compile(r"^[0-9a-f]{16}$")

LOCKS_DIR = Path("/run/veracage")
DM_NAME_RE = re.compile(r"^veracage-[0-9a-f]{12}$")


def vault_hash(vault: str) -> str:
    """16-hex-char hash used to name a vault's lock file and control socket."""
    return hashlib.sha256(vault.encode()).hexdigest()[:16]


def lock_path(vault: str) -> Path:
    return LOCKS_DIR / f"{vault_hash(vault)}.lock"


def parse_lock(p: Path) -> dict[str, str]:
    out: dict[str, str] = {}
    for line in p.read_text().splitlines():
        if "=" in line:
            k, _, v = line.partition("=")
            out[k.strip()] = v.strip()
    return out


def write_lock(p: Path, fields: dict[str, str]) -> None:
    p.parent.mkdir(parents=True, exist_ok=True)
    body = "\n".join(f"{k}={v}" for k, v in fields.items()) + "\n"
    p.write_text(body)
    p.chmod(0o600)


def cleanup_one(p: Path) -> int:
    """Process a single lock file: close its dm device, tidy mountpoint, unlink.

    This helper is reachable passwordless via polkit, so it must not follow a
    symlink to an arbitrary file. The lock path itself is always constructed
    from a validated vault-hash by main(); here we additionally refuse a
    non-regular or symlinked lock.
    """
    try:
        st = p.lstat()
    except OSError:
        return 0  # nothing there; already cleaned up by graceful exit
    if stat.S_ISLNK(st.st_mode) or not stat.S_ISREG(st.st_mode):
        print(f"veracage-cleanup: refusing non-regular lock {p}", file=sys.stderr)
        return 2

    try:
        fields = parse_lock(p)
    except OSError as e:
        print(f"veracage-cleanup: cannot read {p}: {e}", file=sys.stderr)
        return 1

    # Ownership check. The cleanup polkit action is passwordless, so without this
    # any local user could `pkexec veracage-cleanup --vault-hash <someone-else's>`
    # and disrupt another session. PKEXEC_UID is the real caller pkexec vouches
    # for (absent only when the already-root helper invokes us directly — same
    # trust domain). Refuse when it's set and doesn't match the recorded owner.
    caller = os.environ.get("PKEXEC_UID", "")
    owner = fields.get("user_uid", "")
    if caller and owner and caller != owner:
        print(f"veracage-cleanup: uid {caller} does not own session {p.stem} "
              f"(owner uid {owner}); refusing", file=sys.stderr)
        return 2

    rc = 0
    dm_name = fields.get("dm_name", "")
    if DM_NAME_RE.match(dm_name):
        dm_path = Path(f"/dev/mapper/{dm_name}")
        if dm_path.exists():
            r = subprocess.run(
                ["cryptsetup", "close", dm_name],
                capture_output=True, text=True,
            )
            if r.returncode != 0:
                print(f"veracage-cleanup: cryptsetup close {dm_name}: "
                      f"{r.stderr.strip()}", file=sys.stderr)
                rc = r.returncode
    else:
        print(f"veracage-cleanup: refusing to act on dm_name={dm_name!r} "
              f"(does not match veracage-<12-hex>)", file=sys.stderr)
        rc = 2

    # Tear down the session's scratch (orphan mountpoint dir, the idmap staging
    # `.raw` and vault runtime `.run`) + control socket, and drop the lock, ONLY
    # once the device is actually closed. A failed close (e.g. EBUSY) means the
    # session is still LIVE — deleting its runtime dir / socket then would
    # sabotage a running session (and, via the passwordless action, be a
    # cross-user DoS lever); leave the lock as a recovery trail instead. None of
    # this holds plaintext — the key left with the closed dm device.
    if rc == 0:
        mp = fields.get("mountpoint", "")
        if mp.startswith("/run/veracage/") and not Path(mp).is_symlink():
            with contextlib.suppress(OSError):
                Path(mp).rmdir()
            with contextlib.suppress(OSError):
                Path(mp + ".raw").rmdir()
            run_dir = Path(mp + ".run")
            if not run_dir.is_symlink():
                shutil.rmtree(run_dir, ignore_errors=True)
        uid = fields.get("user_uid", "")
        if uid.isdigit():
            with contextlib.suppress(OSError):
                (Path(f"/run/user/{uid}/veracage/sessions") / f"{p.stem}.sock").unlink()
        with contextlib.suppress(FileNotFoundError):
            p.unlink()
    return rc


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(prog="veracage-cleanup")
    # Only a validated vault-hash — never a caller-supplied path. The action
    # is passwordless (polkit allow_active=yes), so an arbitrary --lock path
    # would be a root file-delete primitive.
    p.add_argument("--vault-hash", required=True,
                   help="16-hex vault hash; cleans /run/veracage/<hash>.lock")
    args = p.parse_args(argv)

    if os.geteuid() != 0:
        print("veracage-cleanup: must run as root (via pkexec)", file=sys.stderr)
        return 2

    if not VAULT_HASH_RE.match(args.vault_hash):
        print(f"veracage-cleanup: invalid vault-hash {args.vault_hash!r}",
              file=sys.stderr)
        return 2

    return cleanup_one(LOCKS_DIR / f"{args.vault_hash}.lock")


if __name__ == "__main__":
    sys.exit(main())
