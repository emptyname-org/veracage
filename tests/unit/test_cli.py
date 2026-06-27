"""CLI argv plumbing — pkexec command shape, env-forwarding, validation."""
from __future__ import annotations

import argparse
import os
from unittest import mock

import pytest

from veracage import apps, cli, config


@pytest.fixture(autouse=True)
def _no_active_session(monkeypatch):
    """Pretend there's no running session so cmd_open doesn't refuse."""
    monkeypatch.setattr(
        "veracage.cli.session.send_request",
        mock.Mock(side_effect=FileNotFoundError),
    )


@pytest.fixture
def fake_vault(tmp_path):
    p = tmp_path / "fake.vc"
    p.write_bytes(b"\x00" * 1024)
    return p


@pytest.fixture
def configured(tmp_xdg_config):
    cfg = config.Config(
        apps={"kate": apps.KNOWN_APPS["kate"], "okular": apps.KNOWN_APPS["okular"]},
        last_used_app="kate",
    )
    config.save(cfg)
    return cfg


def _run_open(monkeypatch, vault, app=None):
    monkeypatch.setenv("WAYLAND_DISPLAY", "wayland-0")
    monkeypatch.setenv("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}")
    ns = argparse.Namespace(vault=str(vault), app=app)
    with mock.patch("subprocess.run") as run:
        run.return_value.returncode = 0
        rc = cli.cmd_open(ns)
    argv = run.call_args.args[0] if run.called else None
    return rc, argv


def test_open_invokes_pkexec_helper(monkeypatch, configured, fake_vault):
    _, argv = _run_open(monkeypatch, fake_vault, "kate")
    assert "pkexec" in argv
    assert argv[argv.index("pkexec") + 1].endswith("/veracage-helper")


def test_open_wraps_in_systemd_transient_scope(monkeypatch, configured, fake_vault):
    _, argv = _run_open(monkeypatch, fake_vault, "kate")
    assert argv[0] == "systemd-run"
    assert "--user" in argv
    assert "--scope" in argv
    assert "--collect" in argv


def test_open_registers_execstoppost_cleanup(monkeypatch, configured, fake_vault):
    _, argv = _run_open(monkeypatch, fake_vault, "kate")
    pairs = list(zip(argv, argv[1:]))
    prop = next((b for a, b in pairs if a == "--property"
                 and b.startswith("ExecStopPost=")), None)
    assert prop is not None, "ExecStopPost property missing"
    assert "pkexec" in prop
    assert "veracage-cleanup" in prop
    assert "--vault-hash" in prop


def test_open_unit_name_includes_vault_hash(monkeypatch, configured, fake_vault):
    from veracage import cleanup
    _, argv = _run_open(monkeypatch, fake_vault, "kate")
    unit = next((a for a in argv if a.startswith("--unit=")), None)
    assert unit is not None
    expected = cleanup.vault_hash(str(fake_vault))[:8]
    assert expected in unit


def test_open_passes_vault_user_group(monkeypatch, configured, fake_vault):
    _, argv = _run_open(monkeypatch, fake_vault, "kate")
    assert ("--vault", str(fake_vault)) in zip(argv, argv[1:])
    assert ("--user", str(os.getuid())) in zip(argv, argv[1:])
    assert ("--group", str(os.getgid())) in zip(argv, argv[1:])


def test_open_forwards_wayland_display(monkeypatch, configured, fake_vault):
    _, argv = _run_open(monkeypatch, fake_vault, "kate")
    pairs = list(zip(argv, argv[1:]))
    setenv_pairs = [b for a, b in pairs if a == "--setenv"]
    assert any(s.startswith("WAYLAND_DISPLAY=") for s in setenv_pairs)


def test_open_forwards_xdg_runtime_dir(monkeypatch, configured, fake_vault):
    _, argv = _run_open(monkeypatch, fake_vault, "kate")
    pairs = list(zip(argv, argv[1:]))
    setenv_pairs = [b for a, b in pairs if a == "--setenv"]
    assert any(s.startswith("XDG_RUNTIME_DIR=") for s in setenv_pairs)


def test_open_continuation_path_exists(monkeypatch, configured, fake_vault):
    _, argv = _run_open(monkeypatch, fake_vault, "kate")
    cont_idx = argv.index("--continuation")
    cont_path = argv[cont_idx + 1]
    assert cont_path.endswith("/src/bin/veracage")
    assert os.path.isfile(cont_path), f"continuation path {cont_path} does not exist on disk"


def test_open_uses_last_used_app_when_unspecified(monkeypatch, configured, fake_vault):
    _, argv = _run_open(monkeypatch, fake_vault, app=None)
    sep = argv.index("--")
    after = argv[sep + 1:]
    assert after[0] == "_continue"
    pairs = list(zip(after, after[1:]))
    assert ("--app", "kate") in pairs           # configured.last_used_app
    assert ("--vault", str(fake_vault)) in pairs


def test_open_rejects_unenabled_app(monkeypatch, configured, fake_vault, capsys):
    rc, _ = _run_open(monkeypatch, fake_vault, "dolphin")
    assert rc == 2
    assert "not enabled" in capsys.readouterr().err


def test_open_rejects_missing_vault(monkeypatch, configured, tmp_path, capsys):
    rc, _ = _run_open(monkeypatch, tmp_path / "does-not-exist.vc", "kate")
    assert rc == 2
    assert "not found" in capsys.readouterr().err


def test_open_auto_configures_when_no_config(monkeypatch, tmp_xdg_config, fake_vault, fake_path_with):
    """First-run path: empty config triggers _auto and proceeds."""
    fake_path_with(["kate"])
    monkeypatch.setenv("WAYLAND_DISPLAY", "wayland-0")
    monkeypatch.setenv("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}")
    ns = argparse.Namespace(vault=str(fake_vault), app=None)
    with mock.patch("subprocess.run") as run:
        run.return_value.returncode = 0
        rc = cli.cmd_open(ns)
    assert rc == 0
    # config now exists
    cfg = config.load()
    assert "kate" in cfg.apps


def test_open_fails_when_no_apps_installed(monkeypatch, tmp_xdg_config, fake_vault, fake_path_with, capsys):
    fake_path_with([])
    monkeypatch.setenv("WAYLAND_DISPLAY", "wayland-0")
    rc, _ = _run_open(monkeypatch, fake_vault, app=None)
    assert rc == 2
    assert "Install at least one" in capsys.readouterr().err
