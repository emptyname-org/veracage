"""system-sleep hook logic: phase gating, ignore-honoring, teardown flow."""
from __future__ import annotations

import os
import signal
from unittest import mock

import pytest

from veracage import cleanup, leader, sleep_hook

# ------------------------------------------------------------ phase gating --

@pytest.mark.parametrize("argv", [
    ["post", "suspend"],     # resume: nothing to do
    ["pre", "shutdown"],     # not a sleep state
    ["pre"],                 # missing state
    [],                      # nothing
    ["pre", "boot"],
])
def test_main_noop_off_sleep_pre(argv):
    with mock.patch.object(sleep_hook, "teardown_shared_session") as td:
        assert sleep_hook.main(argv) == 0
        td.assert_not_called()


@pytest.mark.parametrize("state", sorted(sleep_hook.SLEEP_STATES))
def test_main_acts_on_pre_sleep(tmp_path, state):
    locks = tmp_path / "veracage"
    locks.mkdir()
    (locks / "session-1000.lock").write_text(
        "user_uid=1000\nvolume=veracage-aaaaaaaaaaaa\tA\n")
    (locks / "session-1001.lock").write_text(
        "user_uid=1001\nvolume=veracage-bbbbbbbbbbbb\tB\n")
    seen = []
    with mock.patch.object(sleep_hook.cleanup, "LOCKS_DIR", locks), \
         mock.patch("os.geteuid", return_value=0), \
         mock.patch.object(sleep_hook, "teardown_shared_session",
                           side_effect=seen.append):
        assert sleep_hook.main(["pre", state]) == 0
    assert [p.name for p in seen] == ["session-1000.lock", "session-1001.lock"]


def test_main_non_root_is_noop_not_error():
    with mock.patch("os.geteuid", return_value=1000), \
         mock.patch.object(sleep_hook, "teardown_shared_session") as td:
        assert sleep_hook.main(["pre", "suspend"]) == 0
        td.assert_not_called()


def test_main_skips_non_session_locks(tmp_path):
    # Only session-<sid>.lock files are ours to tear down; anything else in the
    # dir (a foreign or stale file) is left alone rather than parsed blind.
    locks = tmp_path / "veracage"
    locks.mkdir()
    (locks / "aa.lock").write_text("dm_name=veracage-aaaaaaaaaaaa\n")
    (locks / "session-1000.lock").write_text(
        "user_uid=1000\nvolume=veracage-bbbbbbbbbbbb\tWork\n")
    shared = []
    with mock.patch.object(sleep_hook.cleanup, "LOCKS_DIR", locks), \
         mock.patch("os.geteuid", return_value=0), \
         mock.patch.object(sleep_hook, "teardown_shared_session",
                           side_effect=shared.append):
        assert sleep_hook.main(["pre", "suspend"]) == 0
    assert [p.name for p in shared] == ["session-1000.lock"]


def test_main_survives_one_session_raising(tmp_path):
    locks = tmp_path / "veracage"
    locks.mkdir()
    (locks / "session-1000.lock").write_text("user_uid=1000\n")
    (locks / "session-1001.lock").write_text("user_uid=1001\n")
    calls = []

    def boom(p):
        calls.append(p.name)
        if p.name == "session-1000.lock":
            raise RuntimeError("kaboom")

    with mock.patch.object(sleep_hook.cleanup, "LOCKS_DIR", locks), \
         mock.patch("os.geteuid", return_value=0), \
         mock.patch.object(sleep_hook, "teardown_shared_session", side_effect=boom):
        assert sleep_hook.main(["pre", "suspend"]) == 0
    # the second session is still processed after the first threw
    assert calls == ["session-1000.lock", "session-1001.lock"]


# ------------------------------------------------------- suspend_action ----

def test_owner_wants_dismount_default_when_no_config():
    with mock.patch("pwd.getpwuid") as gp:
        gp.return_value = mock.Mock(pw_dir="/nonexistent-home-xyz")
        assert sleep_hook.owner_wants_dismount("1000") is True


def test_owner_wants_dismount_honors_ignore(tmp_path):
    home = tmp_path / "home"
    (home / ".config" / "veracage").mkdir(parents=True)
    (home / ".config" / "veracage" / "config.toml").write_text(
        '[default]\nsuspend_action = "ignore"\n'
    )
    with mock.patch("pwd.getpwuid") as gp:
        gp.return_value = mock.Mock(pw_dir=str(home))
        assert sleep_hook.owner_wants_dismount("1000") is False


def test_owner_wants_dismount_true_for_dismount(tmp_path):
    home = tmp_path / "home"
    (home / ".config" / "veracage").mkdir(parents=True)
    (home / ".config" / "veracage" / "config.toml").write_text(
        '[default]\nsuspend_action = "dismount"\n'
    )
    with mock.patch("pwd.getpwuid") as gp:
        gp.return_value = mock.Mock(pw_dir=str(home))
        assert sleep_hook.owner_wants_dismount("1000") is True


def test_owner_wants_dismount_bad_uid():
    assert sleep_hook.owner_wants_dismount("") is True
    assert sleep_hook.owner_wants_dismount("notanum") is True


def test_owner_wants_dismount_malformed_toml(tmp_path):
    home = tmp_path / "home"
    (home / ".config" / "veracage").mkdir(parents=True)
    (home / ".config" / "veracage" / "config.toml").write_text("this = = = broken")
    with mock.patch("pwd.getpwuid") as gp:
        gp.return_value = mock.Mock(pw_dir=str(home))
        assert sleep_hook.owner_wants_dismount("1000") is True


def test_pid_alive():
    assert sleep_hook.pid_alive(os.getpid()) is True
    # a pid that is essentially never live
    assert sleep_hook.pid_alive(2**31 - 1) is False


# --------------------------------------------------- session teardown flow ---
#
# Every `veracage open` writes a session-<sid>.lock (user_uid= +
# volume=<dm>\t<label> lines); the hook must close EVERY volume's dm before
# sleep, or the machine sleeps with dm-crypt keys still in RAM.

def _session_lock(tmp_path, volumes=None):
    p = tmp_path / "session-1000.lock"
    lines = ["user_uid=1000"]
    lines += [f"volume={dm}\t{label}"
              for dm, label in (volumes or [("veracage-aaaaaaaaaaaa", "A"),
                                            ("veracage-bbbbbbbbbbbb", "B")])]
    p.write_text("\n".join(lines) + "\n")
    return p


def test_shared_teardown_skips_when_ignore(tmp_path):
    p = _session_lock(tmp_path)
    with mock.patch.object(sleep_hook, "owner_wants_dismount", return_value=False), \
         mock.patch.object(sleep_hook, "dm_present") as dm, \
         mock.patch("os.kill") as k:
        sleep_hook.teardown_shared_session(p)
    dm.assert_not_called()
    k.assert_not_called()


def test_shared_teardown_noop_when_dms_already_gone(tmp_path):
    p = _session_lock(tmp_path)
    with mock.patch.object(sleep_hook, "owner_wants_dismount", return_value=True), \
         mock.patch.object(sleep_hook, "dm_present", return_value=False), \
         mock.patch.object(sleep_hook, "_session_leader_pid") as lp, \
         mock.patch("os.kill") as k:
        sleep_hook.teardown_shared_session(p)
    lp.assert_not_called()
    k.assert_not_called()


def test_shared_teardown_graceful_sigterm_then_dms_gone(tmp_path):
    p = _session_lock(tmp_path)
    with mock.patch.object(sleep_hook, "owner_wants_dismount", return_value=True), \
         mock.patch.object(sleep_hook, "dm_present", return_value=True), \
         mock.patch.object(sleep_hook, "_session_leader_pid", return_value=4321), \
         mock.patch.object(sleep_hook, "_wait_dms_gone", return_value=True) as w, \
         mock.patch.object(sleep_hook.cleanup, "cleanup_session") as cs, \
         mock.patch("os.kill") as k:
        sleep_hook.teardown_shared_session(p)
    k.assert_called_once_with(4321, signal.SIGTERM)
    # the grace wait covered EVERY volume's dm, not just the first
    assert w.call_args.args[0] == ["veracage-aaaaaaaaaaaa", "veracage-bbbbbbbbbbbb"]
    cs.assert_not_called()


def test_shared_teardown_force_path_when_grace_expires(tmp_path):
    p = _session_lock(tmp_path)
    with mock.patch.object(sleep_hook, "owner_wants_dismount", return_value=True), \
         mock.patch.object(sleep_hook, "dm_present", return_value=True), \
         mock.patch.object(sleep_hook, "_session_leader_pid", return_value=999), \
         mock.patch.object(sleep_hook, "pid_alive", return_value=True), \
         mock.patch.object(sleep_hook, "_wait_dms_gone", return_value=False), \
         mock.patch.object(sleep_hook.cleanup, "cleanup_session") as cs, \
         mock.patch("os.kill") as k:
        sleep_hook.teardown_shared_session(p)
    kinds = {call.args[1] for call in k.call_args_list}
    assert signal.SIGTERM in kinds and signal.SIGKILL in kinds
    cs.assert_called_once_with(p)


def test_shared_teardown_works_without_a_pidfile(tmp_path):
    """No session pidfile (crashed helper): still force-close via cleanup."""
    p = _session_lock(tmp_path)
    with mock.patch.object(sleep_hook, "owner_wants_dismount", return_value=True), \
         mock.patch.object(sleep_hook, "dm_present", return_value=True), \
         mock.patch.object(sleep_hook, "_wait_dms_gone", return_value=False), \
         mock.patch.object(sleep_hook.cleanup, "cleanup_session") as cs, \
         mock.patch("os.kill") as k:
        sleep_hook.teardown_shared_session(p)
    k.assert_not_called()          # no verified leader → nothing to signal
    cs.assert_called_once_with(p)  # but the devices still get closed


def test_session_leader_pid_reads_verified_pidfile(tmp_path):
    p = tmp_path / "session-1000.lock"
    p.write_text("user_uid=1000\n")
    pidf = tmp_path / "session-1000.pid"
    st = cleanup._proc_starttime(os.getpid())
    pidf.write_text(f"{os.getpid()}\n{st}\n")
    assert sleep_hook._session_leader_pid(p) == os.getpid()
    pidf.write_text(f"{os.getpid()}\n0\n")   # wrong start-time (pid reuse)
    assert sleep_hook._session_leader_pid(p) is None


def test_the_suspend_grace_outlasts_the_leaders_app_shutdown_budget():
    """The hook SIGTERMs the leader and SIGKILLs it after GRACE_SECONDS, but the
    leader itself gives its apps leader.TERMINATE_GRACE to flush before it exits
    and lets the devices close. A shorter hook grace kills the apps mid-write and
    leaves the filesystem dirty."""
    assert sleep_hook.GRACE_SECONDS > leader.TERMINATE_GRACE


# The three helpers below were mocked out of every existing test, so nothing
# exercised them: `dm_present` could return a constant False (the hook becomes a
# no-op and every suspend leaves keys in RAM) and the suite stayed green.

def test_dm_present_reads_the_real_device_node(tmp_path, monkeypatch):
    seen = []

    class FakePath(type(tmp_path)):
        pass

    def fake_exists(self):
        seen.append(str(self))
        return str(self).endswith("veracage-aaaaaaaaaaaa")

    monkeypatch.setattr(sleep_hook.Path, "exists", fake_exists)
    assert sleep_hook.dm_present("veracage-aaaaaaaaaaaa") is True
    assert sleep_hook.dm_present("veracage-bbbbbbbbbbbb") is False
    assert sleep_hook.dm_present("") is False
    assert any(s.startswith("/dev/mapper/") for s in seen), seen


def test_wait_dms_gone_waits_and_then_reports_what_is_left(monkeypatch):
    """It is the gate between the graceful phase and the force phase, so it must
    answer about EVERY device and must not report success early."""
    calls = {"n": 0}

    def flaky(dm):
        calls["n"] += 1
        return calls["n"] < 3      # gone from the third look on

    monkeypatch.setattr(sleep_hook, "dm_present", flaky)
    monkeypatch.setattr(sleep_hook, "POLL_INTERVAL", 0.01)
    assert sleep_hook._wait_dms_gone(["veracage-aaaaaaaaaaaa"], 1.0) is True

    monkeypatch.setattr(sleep_hook, "dm_present", lambda dm: True)
    assert sleep_hook._wait_dms_gone(["veracage-aaaaaaaaaaaa"], 0.05) is False


def test_the_force_path_kills_the_leader_BEFORE_closing_the_devices(tmp_path):
    """Ordering matters: while the leader is alive its namespace still holds the
    volume's filesystem, so a close attempted first fails with the device busy
    and the key stays in RAM. The SIGKILL has to come first."""
    p = _session_lock(tmp_path)
    order = []
    with mock.patch.object(sleep_hook, "owner_wants_dismount", return_value=True), \
         mock.patch.object(sleep_hook, "dm_present", return_value=True), \
         mock.patch.object(sleep_hook, "_session_leader_pid", return_value=999), \
         mock.patch.object(sleep_hook, "pid_alive", return_value=True), \
         mock.patch.object(sleep_hook, "_wait_dms_gone", return_value=False), \
         mock.patch.object(sleep_hook.cleanup, "cleanup_session",
                           side_effect=lambda *_: order.append("close")), \
         mock.patch("os.kill", side_effect=lambda pid, sig: order.append(f"sig{sig}")):
        sleep_hook.teardown_shared_session(p)
    assert order == [f"sig{int(signal.SIGTERM)}", f"sig{int(signal.SIGKILL)}", "close"], order


def test_a_device_that_survives_the_force_path_is_reported(tmp_path, capsys):
    """The one case the human must be told about: the machine is about to sleep
    with a key still in RAM."""
    p = _session_lock(tmp_path)
    with mock.patch.object(sleep_hook, "owner_wants_dismount", return_value=True), \
         mock.patch.object(sleep_hook, "dm_present", return_value=True), \
         mock.patch.object(sleep_hook, "_session_leader_pid", return_value=None), \
         mock.patch.object(sleep_hook, "_wait_dms_gone", return_value=False), \
         mock.patch.object(sleep_hook.cleanup, "cleanup_session"):
        sleep_hook.teardown_shared_session(p)
    err = capsys.readouterr().err
    assert "WARNING" in err and "still present" in err


def test_only_the_pre_phase_of_a_sleep_state_acts(tmp_path, monkeypatch):
    """systemd runs this around every transition. Acting on `post` would tear a
    session down on RESUME, and acting on a non-sleep state is just wrong."""
    monkeypatch.setattr(sleep_hook.cleanup, "LOCKS_DIR", tmp_path)
    _session_lock(tmp_path)
    monkeypatch.setattr(sleep_hook.os, "geteuid", lambda: 0)
    calls = []
    monkeypatch.setattr(sleep_hook, "teardown_shared_session", lambda p: calls.append(p))

    assert sleep_hook.main(["post", "suspend"]) == 0
    assert sleep_hook.main(["pre", "not-a-sleep-state"]) == 0
    assert calls == [], "only `pre <sleep state>` may tear a session down"

    assert sleep_hook.main(["pre", "suspend"]) == 0
    assert len(calls) == 1


def test_a_non_root_run_never_touches_a_session(tmp_path, monkeypatch):
    """Everything here (SIGKILL the leader, close the devices) needs root. Run as
    anyone else it must be a no-op rather than a partial teardown - and the test
    must prove that against a lock that really exists, not an empty directory."""
    monkeypatch.setattr(sleep_hook.cleanup, "LOCKS_DIR", tmp_path)
    _session_lock(tmp_path)
    monkeypatch.setattr(sleep_hook.os, "geteuid", lambda: 1000)
    calls = []
    monkeypatch.setattr(sleep_hook, "teardown_shared_session", lambda p: calls.append(p))
    assert sleep_hook.main(["pre", "suspend"]) == 0
    assert calls == []
