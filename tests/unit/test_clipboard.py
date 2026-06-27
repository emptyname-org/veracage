"""Clipboard bridge — verify the subprocess invocations target the right
sockets, and that tool failures/timeouts surface instead of a silent OK."""
from __future__ import annotations

import subprocess
from pathlib import Path
from unittest import mock

import pytest

from veracage import clipboard


@pytest.fixture
def patch_tools(monkeypatch):
    monkeypatch.setattr(
        "shutil.which",
        lambda name: f"/usr/bin/{name}" if name in {"wl-copy", "wl-paste"} else None,
    )


def _ok_proc():
    fake = mock.MagicMock()
    fake.stdout = mock.MagicMock()
    fake.wait.return_value = 0
    return fake


def test_missing_tools_raises(monkeypatch):
    monkeypatch.setattr("shutil.which", lambda _: None)
    with pytest.raises(clipboard.WlClipboardMissing):
        clipboard.push_host_to_sandbox(Path("/run/user/1000/veracage-x"))


def test_push_uses_correct_sockets(patch_tools, monkeypatch):
    """push: src=host (wayland-0), dst=weston (veracage-x)."""
    monkeypatch.delenv("WAYLAND_DISPLAY", raising=False)
    with mock.patch("veracage.clipboard.subprocess.Popen", return_value=_ok_proc()) as p:
        rc = clipboard.push_host_to_sandbox(Path("/run/user/1000/veracage-x"))
    assert rc == 0
    assert p.call_count == 2
    paste_call, copy_call = p.call_args_list
    assert paste_call.args[0][0] == "wl-paste"
    assert paste_call.kwargs["env"]["WAYLAND_DISPLAY"] == "wayland-0"
    assert paste_call.kwargs["env"]["XDG_RUNTIME_DIR"] == "/run/user/1000"
    assert copy_call.args[0][0] == "wl-copy"
    assert copy_call.kwargs["env"]["WAYLAND_DISPLAY"] == "veracage-x"
    assert copy_call.kwargs["env"]["XDG_RUNTIME_DIR"] == "/run/user/1000"


def test_pull_swaps_direction(patch_tools, monkeypatch):
    monkeypatch.delenv("WAYLAND_DISPLAY", raising=False)
    with mock.patch("veracage.clipboard.subprocess.Popen", return_value=_ok_proc()) as p:
        clipboard.pull_sandbox_to_host(Path("/run/user/1000/veracage-x"))
    paste_call, copy_call = p.call_args_list
    assert paste_call.kwargs["env"]["WAYLAND_DISPLAY"] == "veracage-x"
    assert copy_call.kwargs["env"]["WAYLAND_DISPLAY"] == "wayland-0"


def test_custom_host_display(patch_tools):
    with mock.patch("veracage.clipboard.subprocess.Popen", return_value=_ok_proc()) as p:
        clipboard.push_host_to_sandbox(
            Path("/run/user/1000/veracage-x"), host_display="wayland-99")
    assert p.call_args_list[0].kwargs["env"]["WAYLAND_DISPLAY"] == "wayland-99"


def test_host_display_from_env(patch_tools, monkeypatch):
    """Default host display reads WAYLAND_DISPLAY, not a hardcoded wayland-0."""
    monkeypatch.setenv("WAYLAND_DISPLAY", "wayland-7")
    with mock.patch("veracage.clipboard.subprocess.Popen", return_value=_ok_proc()) as p:
        clipboard.push_host_to_sandbox(Path("/run/user/1000/veracage-x"))
    assert p.call_args_list[0].kwargs["env"]["WAYLAND_DISPLAY"] == "wayland-7"


def test_nonzero_return_raises(patch_tools):
    fake = _ok_proc()
    fake.wait.return_value = 3
    with mock.patch("veracage.clipboard.subprocess.Popen", return_value=fake):
        with pytest.raises(clipboard.ClipboardError):
            clipboard.push_host_to_sandbox(Path("/run/user/1000/veracage-x"))


def test_timeout_kills_and_raises(patch_tools):
    fake = _ok_proc()
    fake.wait.side_effect = subprocess.TimeoutExpired(cmd="wl-copy", timeout=10)
    with mock.patch("veracage.clipboard.subprocess.Popen", return_value=fake):
        with pytest.raises(clipboard.ClipboardError):
            clipboard.push_host_to_sandbox(Path("/run/user/1000/veracage-x"))
    fake.kill.assert_called()
