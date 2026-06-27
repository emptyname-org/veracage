"""Automated integration checks against the BUILT Rust privilege helper.

Run after `make build` (or `make install-dev`):

    pytest tests/integration/test_helper_security.py -v

Unlike the manual tests in MANUAL.md these need no vault/Wayland/root: every
case is crafted to fail *before* the helper would fork/mount, so it is safe
to run anywhere. They assert the privilege-boundary argument contract on the
real binary — in particular that the LPE vector (caller-chosen --user /
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
    reason="Rust helper not built — run `make build` first",
)


def _run(args, env=None):
    return subprocess.run(
        [str(HELPER), *args],
        capture_output=True,
        text=True,
        env=env,
        timeout=10,
    )


def test_rejects_user_argument():
    """The LPE vector: --user must not be accepted (regression)."""
    r = _run(["--vault", "/etc/hostname", "--mountpoint", "/run/veracage/x",
              "--user", "0", "--", "_continue"])
    assert r.returncode != 0
    assert "unexpected argument" in r.stderr.lower()


def test_rejects_continuation_argument():
    """The continuation is pinned at build time; --continuation is rejected."""
    r = _run(["--vault", "/etc/hostname", "--mountpoint", "/run/veracage/x",
              "--continuation", "/bin/sh", "--", "x"])
    assert r.returncode != 0
    assert "unexpected argument" in r.stderr.lower()


def test_rejects_group_argument():
    r = _run(["--vault", "/etc/hostname", "--mountpoint", "/run/veracage/x",
              "--group", "0", "--", "x"])
    assert r.returncode != 0
    assert "unexpected argument" in r.stderr.lower()


def test_does_not_proceed_without_pkexec_uid():
    """Run as a normal user with no PKEXEC_UID: the helper must refuse before
    doing anything privileged (it fails the euid check, or — if somehow run as
    root — the PKEXEC_UID check). Either way: non-zero, no mount."""
    env = {k: v for k, v in os.environ.items() if k != "PKEXEC_UID"}
    r = _run(["--vault", "/etc/hostname", "--mountpoint", "/run/veracage/x",
              "--", "_continue"], env=env)
    assert r.returncode != 0


def test_mountpoint_must_be_under_run_veracage():
    """As root via the helper, a mountpoint outside /run/veracage/ is rejected.
    Simulated here by providing PKEXEC_UID and a bad mountpoint; the euid check
    (non-root) or the path check fires first — never a mount at the bad path."""
    env = dict(os.environ, PKEXEC_UID="1000")
    r = _run(["--vault", "/etc/hostname", "--mountpoint", "/tmp/evil",
              "--", "_continue"], env=env)
    assert r.returncode != 0
