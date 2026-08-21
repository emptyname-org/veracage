"""Cleanup module: lock file handling, dm device validation, idempotency."""
from __future__ import annotations

import fcntl
import hashlib
import os
import time
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


def test_cleanup_session_keeps_lock_if_a_close_fails(tmp_path, monkeypatch):
    """One EBUSY device ⇒ the session may be live ⇒ keep the lock (recovery)."""
    monkeypatch.setattr(cleanup, "CLOSE_RETRY_FOR", 0.0)
    p = _session_lock(tmp_path, "d" * 16,
                      [("veracage-abc123abc123", "A"),
                       ("veracage-def456def456", "B")])
    busy = mock.MagicMock(returncode=5, stderr="device busy")
    with mock.patch("veracage.cleanup.subprocess.run", return_value=busy), \
         mock.patch("veracage.cleanup.Path.exists", return_value=True):
        rc = cleanup.cleanup_session(p)
    assert rc == 1
    assert p.exists()


def test_close_dm_waits_out_a_device_the_kernel_has_not_released_yet(monkeypatch):
    """This teardown runs moments after the session leader exited, and the kernel
    frees its mount namespace - the last holder of the filesystem - asynchronously.
    A single attempt loses a race it only has to wait out, and losing it leaves the
    volume open with its key still in RAM."""
    monkeypatch.setattr(cleanup, "CLOSE_RETRY_FOR", 5.0)
    monkeypatch.setattr(cleanup, "CLOSE_RETRY_EVERY", 0.0)
    busy = mock.MagicMock(returncode=5, stderr="Device veracage-a is still in use.")
    ok = mock.MagicMock(returncode=0, stderr="")
    with mock.patch("veracage.cleanup.subprocess.run",
                    side_effect=[busy, busy, ok]) as r, \
         mock.patch("veracage.cleanup.Path.exists", return_value=True):
        assert cleanup.close_dm("veracage-abc123abc123") is None
    assert r.call_count == 3


def test_close_dm_gives_up_and_reports_a_device_that_stays_busy(monkeypatch):
    monkeypatch.setattr(cleanup, "CLOSE_RETRY_FOR", 0.0)
    busy = mock.MagicMock(returncode=5, stderr="Device veracage-a is still in use.")
    with mock.patch("veracage.cleanup.subprocess.run", return_value=busy), \
         mock.patch("veracage.cleanup.Path.exists", return_value=True):
        assert cleanup.close_dm("veracage-abc123abc123") == "Device veracage-a is still in use."


def test_close_dm_is_a_no_op_for_a_device_that_is_already_gone():
    with mock.patch("veracage.cleanup.subprocess.run") as r, \
         mock.patch("veracage.cleanup.Path.exists", return_value=False):
        assert cleanup.close_dm("veracage-abc123abc123") is None
    r.assert_not_called()


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


def test_close_dm_reports_an_os_error_instead_of_raising(monkeypatch, tmp_path):
    """cryptsetup missing from pkexec's PATH, or a fork failure under the memory
    pressure that just OOM-killed the leader. Raising here abandoned every volume
    after the failing one, each still decrypted."""
    monkeypatch.setattr(cleanup.Path, "exists", lambda self: True)

    def boom(*a, **k):
        raise OSError("cannot fork")
    monkeypatch.setattr(cleanup.subprocess, "run", boom)
    monkeypatch.setattr(cleanup, "CLOSE_RETRY_FOR", 0.0)

    err = cleanup.close_dm("veracage-0123456789ab")
    assert err is not None and "fork" in err


def test_a_zombie_leader_does_not_count_as_a_live_session(tmp_path):
    """A leader that was SIGKILLed but not yet reaped keeps its /proc entry with
    an unchanged start-time. Reading that as "still running" makes the teardown a
    no-op - including on the suspend hook's force path, which runs moments after
    sending that very SIGKILL."""
    pid = os.fork()
    if pid == 0:
        os._exit(0)          # becomes a zombie: nobody reaps it until below
    try:
        time.sleep(0.05)
        p = tmp_path / "session-1000.pid"
        st = Path(f"/proc/{pid}/stat").read_text()
        start = st[st.rfind(")") + 1:].split()[19]
        p.write_text(f"{pid}\n{start}\n")
        assert cleanup.session_leader_alive(p) is False
    finally:
        os.waitpid(pid, 0)


def test_the_scratch_is_not_removed_when_rmtree_cannot_resist_symlinks(tmp_path, monkeypatch):
    """`<lock>.run` is veracage-owned, so a compromised sandbox app can plant
    symlinks in it. Root only walks it with the fd-based rmtree; on a platform
    without that guarantee the tree is left alone rather than followed."""
    p = _session_lock(tmp_path, "a" * 16, [])
    run_dir = p.with_suffix(".run")
    run_dir.mkdir()
    (run_dir / "keep").write_text("x")
    monkeypatch.setattr(cleanup.shutil.rmtree, "avoids_symlink_attacks", False,
                        raising=False)
    assert cleanup.cleanup_session(p) == 0
    assert run_dir.exists(), "root must not walk a tree it cannot walk safely"


def test_a_symlinked_scratch_is_never_followed(tmp_path):
    """The same guard for the case where `.run` IS the symlink."""
    p = _session_lock(tmp_path, "b" * 16, [])
    target = tmp_path / "elsewhere"
    target.mkdir()
    (target / "precious").write_text("x")
    p.with_suffix(".run").symlink_to(target)
    assert cleanup.cleanup_session(p) == 0
    assert (target / "precious").exists(), "followed a symlink as root"


def test_the_teardown_takes_the_session_flock(tmp_path, monkeypatch):
    """It serializes against a successor session bootstrapping the same sid. The
    wait is BOUNDED and then proceeds: this runs from ExecStopPost, which systemd
    kills at the unit's stop timeout, and a close-volume helper parked on a human
    answer holds the same lock. Blocking here would mean the devices are never
    closed at all."""
    p = _session_lock(tmp_path, "c" * 16, [("veracage-abc123abc123", "A")])
    monkeypatch.setattr(cleanup, "FLOCK_WAIT", 0.2)
    monkeypatch.setattr("veracage.cleanup.SESSIONS_BASE", tmp_path / "run-user")

    # It must actually TAKE the sidecar lock (not merely be able to run without
    # it): that is what serializes this teardown against a successor session
    # bootstrapping the same sid.
    taken: list = []
    real_take = cleanup._take_flock
    monkeypatch.setattr(cleanup, "_take_flock",
                        lambda path: (taken.append(path), real_take(path))[1])

    # Somebody else holds it for the whole call.
    held = open(p.with_suffix(".flock"), "w")
    fcntl.flock(held, fcntl.LOCK_EX)
    try:
        ok = mock.MagicMock(returncode=0)
        t0 = time.monotonic()
        with mock.patch("veracage.cleanup.subprocess.run", return_value=ok) as r, \
             mock.patch("veracage.cleanup.Path.exists", return_value=True):
            rc = cleanup.cleanup_session(p)
        elapsed = time.monotonic() - t0
    finally:
        held.close()

    assert rc == 0
    assert taken == [p.with_suffix(".flock")], "the session flock was not taken"
    assert r.called, "the devices must be closed even when the lock is held"
    assert elapsed < 5.0, f"waited {elapsed:.1f}s; the wait must be bounded"


def test_the_cli_refuses_a_caller_supplied_path(tmp_path):
    """`--lock`/`--vault-hash` were removed because the action is passwordless:
    an arbitrary path would be a root file-delete primitive. Assert the parser
    REJECTS them, rather than relying on --session being required."""
    for flag in ("--lock", "--vault-hash"):
        with pytest.raises(SystemExit):
            cleanup.main([flag, str(tmp_path / "x"), "--session", "1000"])


def test_the_lock_format_the_helper_actually_writes_parses(tmp_path):
    """The fixtures elsewhere in this file write two-field volume lines, but the
    helper writes four (`append_session_volume`: dm, label, source hash,
    dev:ino) plus a `generation=` header. Parse the real shape, or a field the
    helper adds could break the teardown and nothing here would notice."""
    p = tmp_path / "session-1000.lock"
    p.write_text(
        "user_uid=1000\n"
        "generation=00aabbccddeeff11\n"
        "volume=veracage-aaaaaaaaaaaa\tWork\t0631c55cceb0614f\t66306:12345\n"
        "volume=veracage-bbbbbbbbbbbb\t\tdeadbeefdeadbeef\t66306:67890\n")
    owner, volumes = cleanup.parse_session_lock(p)
    assert owner == "1000"
    assert volumes == [("veracage-aaaaaaaaaaaa", "Work"),
                       ("veracage-bbbbbbbbbbbb", "")]
