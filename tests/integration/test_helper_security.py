"""Automated integration checks against the BUILT Rust privilege helper.

Run after `make build` (or `make install-dev`):

    pytest tests/integration/test_helper_security.py -v

Unlike the manual tests in MANUAL.md these need no vault/Wayland/root: every
case is crafted to fail *before* the helper would fork/mount, so it is safe
to run anywhere. They assert the privilege-boundary argument contract on the
real binary: in particular that the LPE vector (caller-chosen --user /
--continuation) no longer exists.
"""
from __future__ import annotations

import os
import subprocess
from pathlib import Path

import pytest

REPO = Path(__file__).resolve().parents[2]
HELPER = REPO / "helper-rs" / "target" / "release" / "veracage-helper"

pytestmark = pytest.mark.skipif(
    not HELPER.is_file(),
    reason="Rust helper not built: run `make build` first",
)


def _run(args, env=None):
    return subprocess.run(
        [str(HELPER), *args],
        capture_output=True,
        text=True,
        env=env,
        timeout=10,
    )


# Every case uses a VALID flag (`--source`) first so the offending flag is the
# one the parser rejects. Otherwise a stale/unknown leading flag (e.g. an old
# `--vault`) would be rejected first and the regression wouldn't be exercised.
# `parse_args` runs before the euid/PKEXEC_UID checks, so these assert the
# argument contract even when run unprivileged.

def test_rejects_user_argument():
    """The LPE vector: a caller-chosen target uid must not be accepted."""
    r = _run(["--source", "/etc/hostname", "--user", "0", "--", "_leader"])
    assert r.returncode != 0
    assert "unexpected argument: --user" in r.stderr.lower()


def test_rejects_continuation_argument():
    """The continuation is pinned at build time; --continuation is rejected."""
    r = _run(["--source", "/etc/hostname", "--continuation", "/bin/sh", "--", "x"])
    assert r.returncode != 0
    assert "unexpected argument: --continuation" in r.stderr.lower()


def test_rejects_group_argument():
    r = _run(["--source", "/etc/hostname", "--group", "0", "--", "x"])
    assert r.returncode != 0
    assert "unexpected argument: --group" in r.stderr.lower()


def test_does_not_proceed_without_pkexec_uid():
    """Valid args, but run as a normal user with no PKEXEC_UID: the helper must
    refuse before doing anything privileged (it fails the euid check, or, if
    somehow run as root, the PKEXEC_UID check). Either way: non-zero, no mount."""
    env = {k: v for k, v in os.environ.items() if k != "PKEXEC_UID"}
    r = _run(["--source", "/etc/hostname", "--session", "1000",
              "--", "_leader"], env=env)
    assert r.returncode != 0


def test_session_must_match_caller():
    """A `--session` that isn't the caller's own uid is refused (cross-user
    setns/mount/close guard). Run as root would hit check_session_caller; run
    unprivileged it fails the euid check first: either way non-zero, no mount."""
    env = dict(os.environ, PKEXEC_UID="1000")
    r = _run(["--source", "/etc/hostname", "--session", "999999",
              "--", "_leader"], env=env)
    assert r.returncode != 0
