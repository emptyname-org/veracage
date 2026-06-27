"""Clipboard bridge — verify the subprocess invocations target the right
sockets via WAYLAND_DISPLAY+XDG_RUNTIME_DIR overrides."""
from __future__ import annotations

from pathlib import Path
from unittest import mock

import pytest

from veracage import clipboard


@pytest.fixture
def patch_tools(monkeypatch):
    monkeypatch.setattr("shutil.which",
                        lambda name: f"/usr/bin/{name}" if name in {"wl-copy", "wl-paste"} else None)


def test_missing_tools_raises(monkeypatch):
    monkeypatch.setattr("shutil.which", lambda _: None)
    with pytest.raises(clipboard.WlClipboardMissing):
        clipboard.push_host_to_sandbox(Path("/run/user/1000/veracage-x"))


def test_push_uses_correct_sockets(patch_tools, monkeypatch):
    """push: src=host (wayland-0), dst=weston (veracage-x)."""
    fake = mock.MagicMock()
    fake.stdout = mock.MagicMock()
    fake.returncode = 0

    with mock.patch("veracage.clipboard.subprocess.Popen", return_value=fake) as p:
        rc = clipboard.push_host_to_sandbox(Path("/run/user/1000/veracage-x"))
    assert rc == 0
    # Two Popen calls: paste, then copy.
    assert p.call_count == 2

    paste_call, copy_call = p.call_args_list
    assert paste_call.args[0][0] == "wl-paste"
    assert paste_call.kwargs["env"]["WAYLAND_DISPLAY"] == "wayland-0"
    assert paste_call.kwargs["env"]["XDG_RUNTIME_DIR"] == "/run/user/1000"

    assert copy_call.args[0][0] == "wl-copy"
    assert copy_call.kwargs["env"]["WAYLAND_DISPLAY"] == "veracage-x"
    assert copy_call.kwargs["env"]["XDG_RUNTIME_DIR"] == "/run/user/1000"


def test_pull_swaps_direction(patch_tools, monkeypatch):
    fake = mock.MagicMock()
    fake.returncode = 0
    fake.stdout = mock.MagicMock()
    with mock.patch("veracage.clipboard.subprocess.Popen", return_value=fake) as p:
        clipboard.pull_sandbox_to_host(Path("/run/user/1000/veracage-x"))
    paste_call, copy_call = p.call_args_list
    assert paste_call.kwargs["env"]["WAYLAND_DISPLAY"] == "veracage-x"
    assert copy_call.kwargs["env"]["WAYLAND_DISPLAY"] == "wayland-0"


def test_custom_host_display(patch_tools):
    fake = mock.MagicMock()
    fake.returncode = 0
    fake.stdout = mock.MagicMock()
    with mock.patch("veracage.clipboard.subprocess.Popen", return_value=fake) as p:
        clipboard.push_host_to_sandbox(
            Path("/run/user/1000/veracage-x"),
            host_display="wayland-99",
        )
    assert p.call_args_list[0].kwargs["env"]["WAYLAND_DISPLAY"] == "wayland-99"
