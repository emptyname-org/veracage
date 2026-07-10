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
    """The (host-side empty) mountpoint dir + its `.raw` staging under
    /run/veracage/ are rmdir'd."""
    mp = tmp_path / "orphan"
    mp.mkdir()
    p = _lock(tmp_path,
              dm_name="veracage-abc123def456",
              mountpoint=f"/run/veracage/{mp.name}")
    with mock.patch("veracage.cleanup.subprocess.run", return_value=mock.MagicMock(returncode=0)), \
         mock.patch("veracage.cleanup.Path.exists", return_value=True), \
         mock.patch("veracage.cleanup.Path.rmdir") as rmdir:
        cleanup.cleanup_one(p)
    assert rmdir.call_count == 2          # mountpoint + .raw staging


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


def test_cleanup_one_refuses_wrong_owner(tmp_path, monkeypatch):
    """The action is passwordless, so a caller whose uid != the session owner
    (PKEXEC_UID mismatch) must be refused before touching anything."""
    monkeypatch.setenv("PKEXEC_UID", "1001")            # attacker
    p = _lock(tmp_path, dm_name="veracage-abc123def456",
              mountpoint="/run/veracage/xyz", user_uid="1000")  # owner
    with mock.patch("veracage.cleanup.subprocess.run") as r:
        rc = cleanup.cleanup_one(p)
    assert rc == 2
    r.assert_not_called()          # device never touched
    assert p.exists()              # lock left intact


def test_cleanup_one_allows_matching_owner(tmp_path, monkeypatch):
    monkeypatch.setenv("PKEXEC_UID", "1000")
    p = _lock(tmp_path, dm_name="veracage-abc123def456", user_uid="1000")
    with mock.patch("veracage.cleanup.subprocess.run",
                    return_value=mock.MagicMock(returncode=0)), \
         mock.patch("veracage.cleanup.Path.exists", return_value=True):
        rc = cleanup.cleanup_one(p)
    assert rc == 0


def test_cleanup_one_keeps_scratch_and_lock_on_failed_close(tmp_path, monkeypatch):
    """A failed (EBUSY) close means the session is still LIVE — its scratch dir /
    socket must NOT be torn down, and the lock stays as a recovery trail."""
    monkeypatch.delenv("PKEXEC_UID", raising=False)
    mp = tmp_path / "orphan"
    mp.mkdir()
    p = _lock(tmp_path, dm_name="veracage-abc123def456",
              mountpoint=f"/run/veracage/{mp.name}", user_uid="1000")
    with mock.patch("veracage.cleanup.subprocess.run",
                    return_value=mock.MagicMock(returncode=5, stderr="busy")), \
         mock.patch("veracage.cleanup.Path.exists", return_value=True), \
         mock.patch("veracage.cleanup.Path.rmdir") as rmdir, \
         mock.patch("veracage.cleanup.shutil.rmtree") as rmtree:
        rc = cleanup.cleanup_one(p)
    assert rc == 5
    rmdir.assert_not_called()
    rmtree.assert_not_called()
    assert p.exists()


# --------------------------------------------------------------- main() ---

def test_main_requires_root(monkeypatch, capsys):
    monkeypatch.setattr("veracage.cleanup.os.geteuid", lambda: 1000)
    rc = cleanup.main(["--vault-hash", "abcd"])
    assert rc == 2
    assert "must run as root" in capsys.readouterr().err


def test_main_dispatches_to_cleanup_one(monkeypatch, tmp_path):
    monkeypatch.setattr("veracage.cleanup.os.geteuid", lambda: 0)
    monkeypatch.setattr("veracage.cleanup.LOCKS_DIR", tmp_path)
    h = "abcdabcdabcdabcd"
    with mock.patch("veracage.cleanup.cleanup_one", return_value=0) as c:
        cleanup.main(["--vault-hash", h])
    c.assert_called_once_with(tmp_path / f"{h}.lock")


def test_main_rejects_bad_vault_hash(monkeypatch, capsys):
    """Traversal / wrong-length / uppercase hashes are refused (C1)."""
    monkeypatch.setattr("veracage.cleanup.os.geteuid", lambda: 0)
    for bad in ["../../etc/x", "abc", "g" * 16, "ABCDABCDABCDABCD"]:
        assert cleanup.main(["--vault-hash", bad]) == 2
    assert "invalid vault-hash" in capsys.readouterr().err


def test_main_has_no_lock_argument():
    """--lock was a passwordless arbitrary-path root delete; it must be gone."""
    with pytest.raises(SystemExit):
        cleanup.main(["--lock", "/etc/shadow"])


def test_cleanup_one_refuses_symlink_lock(tmp_path):
    target = tmp_path / "real.lock"
    cleanup.write_lock(target, {"dm_name": "veracage-aaaaaaaaaaaa"})
    link = tmp_path / "link.lock"
    link.symlink_to(target)
    assert cleanup.cleanup_one(link) == 2


def test_cleanup_one_keeps_lock_on_failed_close(tmp_path):
    """A failed cryptsetup close must NOT delete the lock (recovery trail)."""
    p = _lock(tmp_path, dm_name="veracage-abc123def456")
    fake = mock.MagicMock()
    fake.returncode = 5
    fake.stderr = "device busy"
    with mock.patch("veracage.cleanup.subprocess.run", return_value=fake), \
         mock.patch("veracage.cleanup.Path.exists", return_value=True):
        rc = cleanup.cleanup_one(p)
    assert rc == 5
    assert p.exists()


# ------------------------------------------------- session lock (shared WS) --

def _session_lock(tmp_path: Path, sid: str, volumes, user_uid="1000") -> Path:
    """Write a session lock: one `volume=<dm>\t<label>` line per volume."""
    lines = [f"user_uid={user_uid}"]
    lines += [f"volume={dm}\t{label}" for dm, label in volumes]
    p = tmp_path / f"session-{sid}.lock"
    p.write_text("\n".join(lines) + "\n")
    return p


def test_parse_session_lock_roundtrip(tmp_path):
    p = _session_lock(tmp_path, "a" * 16,
                      [("veracage-aaaaaaaaaaaa", "Work"),
                       ("veracage-bbbbbbbbbbbb", "Photos")])
    owner, vols = cleanup.parse_session_lock(p)
    assert owner == "1000"
    assert vols == [("veracage-aaaaaaaaaaaa", "Work"),
                    ("veracage-bbbbbbbbbbbb", "Photos")]


def test_parse_session_lock_skips_malformed_volume_lines(tmp_path):
    p = tmp_path / "s.lock"
    p.write_text("user_uid=1000\nvolume=\nvolume=veracage-aaaaaaaaaaaa\ngarbage\n")
    owner, vols = cleanup.parse_session_lock(p)
    assert owner == "1000"
    assert vols == [("veracage-aaaaaaaaaaaa", "")]   # empty dm skipped; label optional


def test_cleanup_session_closes_every_dm(tmp_path):
    p = _session_lock(tmp_path, "c" * 16,
                      [("veracage-abc123abc123", "A"),
                       ("veracage-def456def456", "B")])
    ok = mock.MagicMock(returncode=0)
    with mock.patch("veracage.cleanup.subprocess.run", return_value=ok) as r, \
         mock.patch("veracage.cleanup.Path.exists", return_value=True):
        rc = cleanup.cleanup_session(p)
    assert rc == 0
    closed = [c.args[0] for c in r.call_args_list]
    assert ["cryptsetup", "close", "veracage-abc123abc123"] in closed
    assert ["cryptsetup", "close", "veracage-def456def456"] in closed
    assert not p.exists()   # lock dropped once all closed


def test_cleanup_session_keeps_lock_if_a_close_fails(tmp_path):
    """One EBUSY device ⇒ the session may be live ⇒ keep the lock (recovery)."""
    p = _session_lock(tmp_path, "d" * 16,
                      [("veracage-abc123abc123", "A"),
                       ("veracage-def456def456", "B")])
    busy = mock.MagicMock(returncode=5, stderr="device busy")
    with mock.patch("veracage.cleanup.subprocess.run", return_value=busy), \
         mock.patch("veracage.cleanup.Path.exists", return_value=True):
        rc = cleanup.cleanup_session(p)
    assert rc == 5
    assert p.exists()


def test_cleanup_session_refuses_garbage_dm(tmp_path):
    p = _session_lock(tmp_path, "e" * 16, [("../../evil", "A")])
    with mock.patch("veracage.cleanup.subprocess.run") as r:
        rc = cleanup.cleanup_session(p)
    assert rc == 2
    r.assert_not_called()
    assert p.exists()


def test_cleanup_session_refuses_wrong_owner(tmp_path, monkeypatch):
    monkeypatch.setenv("PKEXEC_UID", "1001")   # attacker, not the owner
    p = _session_lock(tmp_path, "f" * 16, [("veracage-abc123abc123", "A")])
    with mock.patch("veracage.cleanup.subprocess.run") as r:
        rc = cleanup.cleanup_session(p)
    assert rc == 2
    r.assert_not_called()


def test_cleanup_session_returns_0_when_missing(tmp_path):
    assert cleanup.cleanup_session(tmp_path / "session-nope.lock") == 0


def test_main_dispatches_to_cleanup_session(monkeypatch, tmp_path):
    monkeypatch.setattr("veracage.cleanup.os.geteuid", lambda: 0)
    monkeypatch.setattr("veracage.cleanup.LOCKS_DIR", tmp_path)
    sid = "abcdef0123456789"
    with mock.patch("veracage.cleanup.cleanup_session", return_value=0) as c:
        cleanup.main(["--session", sid])
    c.assert_called_once_with(tmp_path / f"session-{sid}.lock")


def test_main_rejects_bad_session_id(monkeypatch, capsys):
    monkeypatch.setattr("veracage.cleanup.os.geteuid", lambda: 0)
    for bad in ["../x", "abc", "g" * 16, "ABCDABCDABCDABCD"]:
        assert cleanup.main(["--session", bad]) == 2
    assert "invalid session id" in capsys.readouterr().err


def test_main_session_and_vault_hash_mutually_exclusive():
    with pytest.raises(SystemExit):
        cleanup.main(["--session", "a" * 16, "--vault-hash", "b" * 16])
