"""Cleanup module: lock file handling, dm device validation, idempotency."""
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


# --------------------------------------------------------------- main() ---

def test_main_requires_root(monkeypatch, capsys):
    monkeypatch.setattr("veracage.cleanup.os.geteuid", lambda: 1000)
    rc = cleanup.main(["--session", "1000"])
    assert rc == 2
    assert "must run as root" in capsys.readouterr().err


def test_main_has_no_lock_argument():
    """--lock was a passwordless arbitrary-path root delete; it must be gone."""
    with pytest.raises(SystemExit):
        cleanup.main(["--lock", "/etc/shadow"])


def test_main_has_no_vault_hash_argument():
    """--vault-hash was the removed per-vault path; it must be gone."""
    with pytest.raises(SystemExit):
        cleanup.main(["--vault-hash", "a" * 16])


# ------------------------------------------------------------ session lock --

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


def test_cleanup_session_closes_every_dm(tmp_path, monkeypatch):
    monkeypatch.setattr("veracage.cleanup.SESSIONS_BASE", tmp_path / "run-user")
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


def test_cleanup_session_refuses_wrong_or_missing_owner(tmp_path, monkeypatch):
    """cleanup_session is fail-CLOSED: a pkexec caller that isn't the recorded
    owner (including a header-less lock) is refused before any device is
    touched."""
    monkeypatch.setenv("PKEXEC_UID", "1001")
    for uid in ("1000", ""):   # wrong owner, then header-less
        p = _session_lock(tmp_path, "d" * 16,
                          [("veracage-abc123abc123", "A")], user_uid=uid)
        with mock.patch("veracage.cleanup.subprocess.run") as r:
            rc = cleanup.cleanup_session(p)
        assert rc == 2
        r.assert_not_called()
        assert p.exists()


def test_remove_stale_session_sockets_sweeps_the_dir(tmp_path, monkeypatch):
    """Regression: the control socket is keyed by the bootstrap VAULT's hash, not
    the session id. Cleanup once unlinked a 'session-<sid>.sock' that never
    existed and left the real stale socket behind."""
    import socket as socket_mod
    base = tmp_path.resolve()
    monkeypatch.setattr("veracage.cleanup.SESSIONS_BASE", base)
    d = base / "1000" / "veracage" / "sessions"
    d.mkdir(parents=True)
    s = socket_mod.socket(socket_mod.AF_UNIX, socket_mod.SOCK_STREAM)
    s.bind(str(d / "fff2a519aabbccdd.sock"))     # vault-hash-named, stale
    (d / "not-a-socket.sock").write_text("plain file, must survive")
    cleanup._remove_stale_session_sockets("1000")
    s.close()
    assert not (d / "fff2a519aabbccdd.sock").exists()
    assert (d / "not-a-socket.sock").exists()


def test_remove_stale_session_sockets_refuses_symlinked_dir(tmp_path, monkeypatch):
    """Root must not follow a human-planted symlink out of the sessions dir."""
    import socket as socket_mod
    base = tmp_path.resolve()
    monkeypatch.setattr("veracage.cleanup.SESSIONS_BASE", base)
    elsewhere = base / "elsewhere" / "sessions"
    elsewhere.mkdir(parents=True)
    s = socket_mod.socket(socket_mod.AF_UNIX, socket_mod.SOCK_STREAM)
    s.bind(str(elsewhere / "victim.sock"))
    (base / "1000").mkdir()
    (base / "1000" / "veracage").symlink_to(base / "elsewhere")
    cleanup._remove_stale_session_sockets("1000")
    s.close()
    assert (elsewhere / "victim.sock").exists()   # untouched


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
    sid = "1000"
    with mock.patch("veracage.cleanup.cleanup_session", return_value=0) as c:
        cleanup.main(["--session", sid])
    c.assert_called_once_with(tmp_path / f"session-{sid}.lock")


def test_main_accepts_the_sid_the_cli_actually_passes(monkeypatch, tmp_path):
    """Regression: cli.py registers `ExecStopPost=… --session <uid>` (a short
    decimal). SID_RE once demanded 16 hex chars, so the crash-safety teardown
    was ALWAYS rejected and the dm-crypt key stayed in RAM after a SIGKILL."""
    import os
    monkeypatch.setattr("veracage.cleanup.os.geteuid", lambda: 0)
    monkeypatch.setattr("veracage.cleanup.LOCKS_DIR", tmp_path)
    sid = str(os.getuid())
    with mock.patch("veracage.cleanup.cleanup_session", return_value=0) as c:
        assert cleanup.main(["--session", sid]) == 0
    c.assert_called_once_with(tmp_path / f"session-{sid}.lock")


def test_main_rejects_bad_session_id(monkeypatch, capsys):
    monkeypatch.setattr("veracage.cleanup.os.geteuid", lambda: 0)
    # Must mirror helper-rs session_id_ok: 1-16 ASCII digits, nothing else.
    for bad in ["../x", "abc", "g" * 16, "ABCDABCDABCDABCD", "", "1" * 17,
                "abcdef0123456789"]:
        assert cleanup.main(["--session", bad]) == 2
    assert "invalid session id" in capsys.readouterr().err


def test_cleanup_session_refuses_symlink_lock(tmp_path):
    target = _session_lock(tmp_path, "1000", [("veracage-abc123abc123", "A")])
    link = tmp_path / "link.lock"
    link.symlink_to(target)
    assert cleanup.cleanup_session(link) == 2


# ------------------------------------------- session liveness guard (Phase 3) --

def test_session_leader_alive_true_for_self(tmp_path):
    import os
    pidf = tmp_path / "session-x.pid"
    st = cleanup._proc_starttime(os.getpid())
    pidf.write_text(f"{os.getpid()}\n{st}\n")
    assert cleanup.session_leader_alive(pidf) is True


def test_session_leader_alive_false_on_starttime_mismatch(tmp_path):
    import os
    pidf = tmp_path / "session-x.pid"
    pidf.write_text(f"{os.getpid()}\n0\n")   # our pid, wrong start-time (reuse)
    assert cleanup.session_leader_alive(pidf) is False


def test_session_leader_alive_false_when_missing(tmp_path):
    assert cleanup.session_leader_alive(tmp_path / "nope.pid") is False


def test_cleanup_session_noop_while_leader_alive(tmp_path):
    """A 2nd open's ExecStopPost fires while the leader lives → must NOT tear
    down the live session."""
    p = _session_lock(tmp_path, "a" * 16, [("veracage-abc123abc123", "A")])
    with mock.patch("veracage.cleanup.session_leader_alive", return_value=True), \
         mock.patch("veracage.cleanup.subprocess.run") as r:
        rc = cleanup.cleanup_session(p)
    assert rc == 0
    r.assert_not_called()      # leader alive → nothing closed
    assert p.exists()          # lock kept
