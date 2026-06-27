"""Cleanup module — lock file handling, dm device validation, idempotency."""
from __future__ import annotations

import hashlib
from pathlib import Path
from unittest import mock

import pytest

from veracage import cleanup


# --------------------------------------------------------- hashing & paths --

def test_vault_hash_is_deterministic():
    h1 = cleanup.vault_hash("/tmp/foo.vc")
    h2 = cleanup.vault_hash("/tmp/foo.vc")
    assert h1 == h2
    assert len(h1) == 16
    assert all(c in "0123456789abcdef" for c in h1)


def test_vault_hash_differs_for_different_vaults():
    a = cleanup.vault_hash("/tmp/a.vc")
    b = cleanup.vault_hash("/tmp/b.vc")
    assert a != b


def test_vault_hash_matches_sha256_prefix():
    expected = hashlib.sha256(b"/tmp/x.vc").hexdigest()[:16]
    assert cleanup.vault_hash("/tmp/x.vc") == expected


# --------------------------------------------------------- lock file I/O ---

def test_write_then_parse_roundtrip(tmp_path):
    p = tmp_path / "x.lock"
    cleanup.write_lock(p, {
        "dm_name": "veracage-abc123def456",
        "mountpoint": "/run/veracage/xyz",
        "user_uid": "1000",
    })
    fields = cleanup.parse_lock(p)
    assert fields["dm_name"] == "veracage-abc123def456"
    assert fields["mountpoint"] == "/run/veracage/xyz"
    assert fields["user_uid"] == "1000"


def test_write_lock_chmods_to_600(tmp_path):
    p = tmp_path / "x.lock"
    cleanup.write_lock(p, {"dm_name": "veracage-aaaaaaaaaaaa"})
    assert (p.stat().st_mode & 0o777) == 0o600


def test_parse_lock_ignores_blank_lines(tmp_path):
    p = tmp_path / "x.lock"
    p.write_text("\ndm_name=veracage-abcabcabcabc\n\nmountpoint=/run/veracage/x\n")
    fields = cleanup.parse_lock(p)
    assert set(fields) == {"dm_name", "mountpoint"}


# ---------------------------------------------------------------- cleanup --

def _lock(tmp_path: Path, **fields) -> Path:
    p = tmp_path / "session.lock"
    cleanup.write_lock(p, fields)
    return p


def test_cleanup_one_returns_0_when_lock_missing(tmp_path):
    p = tmp_path / "nope.lock"
    assert cleanup.cleanup_one(p) == 0


def test_cleanup_one_runs_cryptsetup_close(tmp_path):
    p = _lock(tmp_path,
              dm_name="veracage-abc123def456",
              mountpoint="/run/veracage/xyz")
    fake = mock.MagicMock()
    fake.returncode = 0
    with mock.patch("veracage.cleanup.subprocess.run", return_value=fake) as r, \
         mock.patch("veracage.cleanup.Path.exists", return_value=True):
        rc = cleanup.cleanup_one(p)
    assert rc == 0
    r.assert_called_once_with(
        ["cryptsetup", "close", "veracage-abc123def456"],
        capture_output=True, text=True,
    )
    # Lock file removed after cleanup.
    assert not p.exists()


def test_cleanup_one_skips_when_dm_device_already_gone(tmp_path):
    """If /dev/mapper/<name> doesn't exist, skip cryptsetup close gracefully."""
    p = _lock(tmp_path, dm_name="veracage-abc123def456")
    with mock.patch("veracage.cleanup.subprocess.run") as r, \
         mock.patch("veracage.cleanup.Path.exists", return_value=False):
        rc = cleanup.cleanup_one(p)
    assert rc == 0
    r.assert_not_called()
    assert not p.exists()


def test_cleanup_one_refuses_garbage_dm_name(tmp_path):
    """A corrupted lock must not let us close arbitrary dm devices."""
    p = _lock(tmp_path, dm_name="luks-rootvg-home")
    with mock.patch("veracage.cleanup.subprocess.run") as r:
        rc = cleanup.cleanup_one(p)
    assert rc == 2
    r.assert_not_called()


def test_cleanup_one_refuses_dm_name_path_traversal(tmp_path):
    p = _lock(tmp_path, dm_name="veracage-aaaaaaaaaaaa/../../etc")
    with mock.patch("veracage.cleanup.subprocess.run") as r:
        rc = cleanup.cleanup_one(p)
    assert rc == 2
    r.assert_not_called()


def test_cleanup_one_returns_cryptsetup_failure(tmp_path):
    p = _lock(tmp_path, dm_name="veracage-abc123def456")
    fake = mock.MagicMock()
    fake.returncode = 5
    fake.stderr = "device busy"
    with mock.patch("veracage.cleanup.subprocess.run", return_value=fake), \
         mock.patch("veracage.cleanup.Path.exists", return_value=True):
        rc = cleanup.cleanup_one(p)
    assert rc == 5


def test_cleanup_one_removes_orphan_mountpoint(tmp_path):
    """The (host-side empty) mountpoint dir under /run/veracage/ is rmdir'd."""
    mp = tmp_path / "orphan"
    mp.mkdir()
    p = _lock(tmp_path,
              dm_name="veracage-abc123def456",
              mountpoint=f"/run/veracage/{mp.name}")
    with mock.patch("veracage.cleanup.subprocess.run", return_value=mock.MagicMock(returncode=0)), \
         mock.patch("veracage.cleanup.Path.exists", return_value=True), \
         mock.patch("veracage.cleanup.Path.rmdir") as rmdir:
        cleanup.cleanup_one(p)
    rmdir.assert_called_once()


def test_cleanup_one_ignores_non_run_veracage_mountpoint(tmp_path):
    """A maliciously-edited lock file must not let us rmdir arbitrary paths."""
    p = _lock(tmp_path,
              dm_name="veracage-abc123def456",
              mountpoint="/etc/passwd")  # nope
    with mock.patch("veracage.cleanup.subprocess.run", return_value=mock.MagicMock(returncode=0)), \
         mock.patch("veracage.cleanup.Path.exists", return_value=True), \
         mock.patch("veracage.cleanup.Path.rmdir") as rmdir:
        cleanup.cleanup_one(p)
    rmdir.assert_not_called()


# --------------------------------------------------------------- main() ---

def test_main_requires_root(monkeypatch, capsys):
    monkeypatch.setattr("veracage.cleanup.os.geteuid", lambda: 1000)
    rc = cleanup.main(["--vault-hash", "abcd"])
    assert rc == 2
    assert "must run as root" in capsys.readouterr().err


def test_main_dispatches_to_cleanup_one(monkeypatch, tmp_path):
    monkeypatch.setattr("veracage.cleanup.os.geteuid", lambda: 0)
    monkeypatch.setattr("veracage.cleanup.LOCKS_DIR", tmp_path)
    p = tmp_path / "abcd.lock"
    p.write_text("dm_name=veracage-abc123def456\n")
    with mock.patch("veracage.cleanup.cleanup_one", return_value=0) as c:
        cleanup.main(["--vault-hash", "abcd"])
    c.assert_called_once_with(tmp_path / "abcd.lock")
