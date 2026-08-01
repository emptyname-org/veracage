"""Liveness of the ONE persistent compositor: pidfile read, up/down logic, wait.

The per-vault `nested_compositor` spawn is gone (Phase 2): the compositor is
brought up by the privileged helper on a fixed socket, and this module only
observes it.
"""
from __future__ import annotations

import os
import time

import pytest

from veracage import wayland


@pytest.fixture
def fake_rt(monkeypatch, tmp_path):
    """Redirect the fixed compositor paths into a tmp dir."""
    sock = tmp_path / "wl-vc"
    pidf = tmp_path / "compositor.pid"
    monkeypatch.setattr(wayland, "COMPOSITOR_RUNTIME", tmp_path)
    monkeypatch.setattr(wayland, "COMPOSITOR_SOCKET", sock)
    monkeypatch.setattr(wayland, "COMPOSITOR_PIDFILE", pidf)
    return tmp_path, sock, pidf


# ------------------------------------------------------------- pidfile -----

def test_compositor_pid_reads_pidfile(fake_rt):
    _, _, pidf = fake_rt
    pidf.write_text("4321\n")
    assert wayland.compositor_pid() == 4321


def test_compositor_pid_none_when_absent(fake_rt):
    assert wayland.compositor_pid() is None


def test_compositor_pid_none_when_garbage(fake_rt):
    _, _, pidf = fake_rt
    pidf.write_text("not-a-pid")
    assert wayland.compositor_pid() is None


# --------------------------------------------------------------- up/down ---

def test_is_up_true_when_pid_alive_and_socket(fake_rt, monkeypatch):
    _, sock, pidf = fake_rt
    pidf.write_text("4321\n")
    sock.touch()
    monkeypatch.setattr(wayland, "_pid_alive", lambda pid: True)
    assert wayland.compositor_is_up() is True


def test_is_down_when_pid_dead(fake_rt, monkeypatch):
    _, sock, pidf = fake_rt
    pidf.write_text("4321\n")
    sock.touch()
    monkeypatch.setattr(wayland, "_pid_alive", lambda pid: False)
    assert wayland.compositor_is_up() is False


def test_is_down_when_socket_missing(fake_rt, monkeypatch):
    _, _, pidf = fake_rt
    pidf.write_text("4321\n")  # pid alive but no socket yet (half-started)
    monkeypatch.setattr(wayland, "_pid_alive", lambda pid: True)
    assert wayland.compositor_is_up() is False


def test_is_down_when_no_pidfile(fake_rt, monkeypatch):
    _, sock, _ = fake_rt
    sock.touch()
    monkeypatch.setattr(wayland, "_pid_alive", lambda pid: True)
    assert wayland.compositor_is_up() is False


# --------------------------------------------------------------- wait ------

def test_wait_returns_when_up(fake_rt, monkeypatch):
    _, sock, pidf = fake_rt
    pidf.write_text("4321\n")
    sock.touch()
    monkeypatch.setattr(wayland, "_pid_alive", lambda pid: True)
    wayland.wait_for_compositor(timeout=0.5)  # returns without raising


def test_wait_raises_on_timeout(fake_rt):
    with pytest.raises(wayland.CompositorStartFailed, match="did not appear"):
        wayland.wait_for_compositor(timeout=0.05)


# ----------------------------------------------------- _pid_alive (real) ----

def test_pid_alive_true_for_running():
    assert wayland._pid_alive(os.getpid()) is True


def test_pid_alive_false_for_absent():
    # PID 2**31-1 is above pid_max on any real system → no such process.
    assert wayland._pid_alive(2**31 - 1) is False


def test_pid_alive_false_for_zombie():
    """A defunct compositor keeps a /proc entry in state Z until reaped. It must
    read as *down* so the leader tears the session down (the window-close bug)."""
    pid = os.fork()
    if pid == 0:  # child: exit immediately, become a zombie (parent won't reap yet)
        os._exit(0)
    try:
        # The child has to be scheduled before it can reach state Z, so wait for
        # it against a deadline. A fixed number of busy polls (what this used to
        # do) burns through them before the child has run at all on a fast or
        # loaded machine, which made this test flaky.
        deadline = time.monotonic() + 5.0
        while wayland._pid_alive(pid) and time.monotonic() < deadline:
            time.sleep(0.005)
        assert wayland._pid_alive(pid) is False
    finally:
        os.waitpid(pid, 0)
