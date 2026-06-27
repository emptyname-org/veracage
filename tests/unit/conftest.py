"""Shared fixtures for unit tests.

Unit tests must not touch the user's real config, real Wayland, real bwrap,
or real veracrypt — fixtures here redirect everything to tmp paths.
"""
from __future__ import annotations

import tempfile
from pathlib import Path

import pytest


@pytest.fixture
def tmp_xdg_config(monkeypatch, tmp_path: Path) -> Path:
    """Redirect $XDG_CONFIG_HOME to a tmp path so config tests can't pollute ~/.config."""
    cfg_dir = tmp_path / "config"
    cfg_dir.mkdir()
    monkeypatch.setenv("XDG_CONFIG_HOME", str(cfg_dir))
    return cfg_dir


@pytest.fixture
def tmp_xdg_runtime(monkeypatch, tmp_path: Path) -> Path:
    """Redirect $XDG_RUNTIME_DIR. Required by sandbox/wayland modules."""
    rt = tmp_path / "run"
    rt.mkdir()
    monkeypatch.setenv("XDG_RUNTIME_DIR", str(rt))
    return rt


@pytest.fixture
def fake_path_with(monkeypatch):
    """Return a function that constructs a $PATH containing only the named bins.

    Usage:
        path = fake_path_with(["kate", "okular"])
        # now shutil.which("kate") returns a path; "dolphin" returns None
    """
    def _make(names: list[str]) -> Path:
        bindir = Path(tempfile.mkdtemp(prefix="veracage-fakepath-"))
        for n in names:
            f = bindir / n
            f.write_text("#!/bin/sh\nexit 0\n")
            f.chmod(0o755)
        monkeypatch.setenv("PATH", str(bindir))
        return bindir
    return _make
