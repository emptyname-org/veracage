"""Shared fixtures for unit tests.

Unit tests must not touch the user's real config, real Wayland, real bwrap,
or real veracrypt. Fixtures here redirect everything to tmp paths.
"""
from __future__ import annotations

import tempfile
from pathlib import Path

import pytest


@pytest.fixture(autouse=True)
def _isolate_from_the_real_session(monkeypatch, tmp_path_factory) -> None:
    """Redirect everything a unit test could write into the developer's live
    session: the compositor publish dir (`config.publish_apps`), the runtime dir
    (`leader` seeds kdeglobals and user-places into `$XDG_RUNTIME_DIR`, and the
    leader/agent socket paths are derived from it) and HOME (`cfg.exchange_path()`
    creates `~/Veracage/Exchange`, and config reads `~/.config`).

    Autouse, because the leak was silent: running the suite rewrote the real
    `$XDG_RUNTIME_DIR/kdeglobals` and created the real shared directory, and a
    few tests asserted against ambient state instead of their own fixtures."""
    # A directory of its OWN, not the test's `tmp_path`: several tests scan
    # `tmp_path` as a workspace root and would see these as volumes.
    sandbox = tmp_path_factory.mktemp("isolated")
    monkeypatch.setenv("VERACAGE_PUB_DIR", str(sandbox / "pub"))
    home = sandbox / "home"
    (home / ".config").mkdir(parents=True, exist_ok=True)
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("XDG_CONFIG_HOME", str(home / ".config"))
    runtime = sandbox / "runtime"
    runtime.mkdir(exist_ok=True)
    monkeypatch.setenv("XDG_RUNTIME_DIR", str(runtime))


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
