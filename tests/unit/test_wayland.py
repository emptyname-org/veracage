"""Nested-Weston context manager — spawn args, startup wait, cleanup."""
from __future__ import annotations

import signal
from unittest import mock

import pytest

from veracage import wayland


@pytest.fixture
def fake_runtime(monkeypatch, tmp_path):
    monkeypatch.setenv("XDG_RUNTIME_DIR", str(tmp_path))
    return tmp_path


def _popen_that_creates_socket(tmp_path):
    """Side-effect: when Popen is called, create the requested socket file."""
    proc = mock.MagicMock()
    proc.pid = 12345
    proc.poll = mock.Mock(return_value=None)  # stays alive

    def side_effect(argv, **_):
        for arg in argv:
            if isinstance(arg, str) and arg.startswith("--socket="):
                (tmp_path / arg.split("=", 1)[1]).touch()
        return proc

    return proc, side_effect


def test_yields_socket_path_under_runtime_dir(fake_runtime):
    _, popen = _popen_that_creates_socket(fake_runtime)
    with mock.patch("veracage.wayland.subprocess.Popen", side_effect=popen), \
         mock.patch("veracage.wayland.os.killpg"):
        with wayland.nested_weston() as sock:
            assert sock.exists()
            assert sock.parent == fake_runtime
            assert sock.name.startswith("veracage-")


def test_invokes_weston_with_socket_arg(fake_runtime):
    _, popen = _popen_that_creates_socket(fake_runtime)
    with mock.patch("veracage.wayland.subprocess.Popen",
                    side_effect=popen) as p, \
         mock.patch("veracage.wayland.os.killpg"):
        with wayland.nested_weston():
            pass
    argv = p.call_args.args[0]
    assert argv[0] == "weston"
    assert any(a.startswith("--socket=veracage-") for a in argv)


def test_raises_when_weston_exits_early(fake_runtime):
    proc = mock.MagicMock()
    proc.pid = 12345
    proc.poll = mock.Mock(return_value=1)  # already exited
    proc.returncode = 1
    with mock.patch("veracage.wayland.subprocess.Popen", return_value=proc), \
         mock.patch("veracage.wayland.os.killpg"):
        with pytest.raises(wayland.WestonStartFailed, match="exited early"):
            with wayland.nested_weston():
                pass


def test_raises_when_socket_does_not_appear(fake_runtime, monkeypatch):
    proc = mock.MagicMock()
    proc.pid = 12345
    proc.poll = mock.Mock(return_value=None)  # alive but no socket file
    monkeypatch.setattr(wayland, "WESTON_STARTUP_TIMEOUT_S", 0.05)
    with mock.patch("veracage.wayland.subprocess.Popen", return_value=proc), \
         mock.patch("veracage.wayland.os.killpg"):
        with pytest.raises(wayland.WestonStartFailed, match="did not appear"):
            with wayland.nested_weston():
                pass


def test_kills_process_group_on_exit(fake_runtime):
    proc, popen = _popen_that_creates_socket(fake_runtime)
    with mock.patch("veracage.wayland.subprocess.Popen", side_effect=popen), \
         mock.patch("veracage.wayland.os.killpg") as killpg:
        with wayland.nested_weston():
            pass
    killpg.assert_called_with(12345, signal.SIGTERM)


def test_does_not_kill_if_already_exited(fake_runtime):
    proc, popen = _popen_that_creates_socket(fake_runtime)
    with mock.patch("veracage.wayland.subprocess.Popen", side_effect=popen), \
         mock.patch("veracage.wayland.os.killpg") as killpg:
        with wayland.nested_weston():
            # Simulate weston exiting cleanly during the with-block
            proc.poll = mock.Mock(return_value=0)
    killpg.assert_not_called()


# ----------------------------------------------- invocation builder --------

def test_invocation_default_no_upstream(tmp_path):
    argv, env, pass_fds = wayland._weston_invocation("veracage-aa", None, tmp_path)
    assert argv[0] == "weston"
    assert "--socket=veracage-aa" in argv
    assert not any(a.startswith("--backend=") for a in argv)
    assert "WAYLAND_SOCKET" not in env
    assert pass_fds == ()
    assert env["XDG_RUNTIME_DIR"] == str(tmp_path)


def test_invocation_with_upstream_fd(tmp_path, monkeypatch):
    monkeypatch.setenv("WAYLAND_DISPLAY", "wayland-0")
    argv, env, pass_fds = wayland._weston_invocation("veracage-bb", 7, tmp_path)
    assert "--backend=wayland-backend.so" in argv
    assert env["WAYLAND_SOCKET"] == "7"
    assert "WAYLAND_DISPLAY" not in env       # forced to use the inherited fd
    assert pass_fds == (7,)
