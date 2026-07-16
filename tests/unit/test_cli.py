"""CLI argv plumbing — pkexec command shape, env-forwarding, validation."""
from __future__ import annotations

import argparse
import json
import os
from unittest import mock

import pytest

from veracage import apps, cli, config


@pytest.fixture(autouse=True)
def _no_active_session(monkeypatch):
    """Pretend there's no running session so cmd_open doesn't refuse. (The
    compositor is brought up by the mount helper now, not cmd_open.) Also
    neutralise the join-detection ping so tests never touch the developer's
    real /run/user sockets."""
    monkeypatch.setattr(
        "veracage.cli.leader.send_request",
        mock.Mock(side_effect=FileNotFoundError),
    )
    monkeypatch.setattr("veracage.cli._any_live_session", lambda: False)
    # The compositor is brought up as its own unit before the mount; don't run a
    # real systemd-run/pkexec from the argv tests. (A dedicated test asserts the
    # bring-up happens.)
    monkeypatch.setattr("veracage.cli.ensure_compositor_up", lambda: 0)


@pytest.fixture
def fake_vault(tmp_path):
    p = tmp_path / "fake.vc"
    p.write_bytes(b"\x00" * 1024)
    return p


@pytest.fixture
def configured(tmp_xdg_config):
    cfg = config.Config(
        apps={"kate": apps.App("kate", "Kate", "kate", ["/vault"]),
              "okular": apps.App("okular", "Okular", "okular")},
        last_used_app="kate",
    )
    config.save(cfg)
    return cfg


def _run_open(monkeypatch, vault, app=None):
    monkeypatch.setenv("WAYLAND_DISPLAY", "wayland-0")
    monkeypatch.setenv("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}")
    ns = argparse.Namespace(vault=str(vault), app=app, passphrase_stdin=False)
    with mock.patch("subprocess.run") as run, mock.patch("subprocess.Popen") as popen:
        run.return_value.returncode = 0
        popen.return_value.poll.return_value = 0  # agent "exited" -> skip teardown
        rc = cli.cmd_open(ns)
    argv = run.call_args.args[0] if run.called else None
    return rc, argv


def test_open_invokes_pkexec_helper(monkeypatch, configured, fake_vault):
    _, argv = _run_open(monkeypatch, fake_vault, "kate")
    assert "pkexec" in argv
    assert argv[argv.index("pkexec") + 1].endswith("/veracage-helper")


def test_open_wraps_in_systemd_transient_service(monkeypatch, configured, fake_vault):
    # A transient *service* (via --pty), not a --scope: scope units reject the
    # Exec* properties, so ExecStopPost cleanup would never register on a scope.
    _, argv = _run_open(monkeypatch, fake_vault, "kate")
    assert argv[0] == "systemd-run"
    assert "--user" in argv
    assert "--pty" in argv
    assert "--scope" not in argv
    assert "--collect" in argv


def test_open_passphrase_stdin_uses_pipe(monkeypatch, configured, fake_vault):
    """The GUI path (--passphrase-stdin) uses systemd-run --pipe, not --pty, so
    the piped passphrase reaches the helper — and forwards --passphrase-stdin."""
    monkeypatch.setenv("WAYLAND_DISPLAY", "wayland-0")
    monkeypatch.setenv("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}")
    ns = argparse.Namespace(vault=str(fake_vault), app=None, passphrase_stdin=True)
    with mock.patch("subprocess.run") as run, mock.patch("subprocess.Popen"):
        run.return_value.returncode = 0
        cli.cmd_open(ns)
    argv = run.call_args.args[0]
    assert "--pipe" in argv and "--pty" not in argv
    assert "--passphrase-stdin" in argv


def test_open_registers_execstoppost_cleanup(monkeypatch, configured, fake_vault):
    _, argv = _run_open(monkeypatch, fake_vault, "kate")
    pairs = list(zip(argv, argv[1:]))
    prop = next((b for a, b in pairs if a == "--property"
                 and b.startswith("ExecStopPost=")), None)
    assert prop is not None, "ExecStopPost property missing"
    assert "pkexec" in prop
    assert "veracage-cleanup" in prop
    # Shared-workspace: teardown is session-scoped (closes every volume's dm).
    assert f"--session {os.getuid()}" in prop


def test_open_unit_name_includes_vault_hash(monkeypatch, configured, fake_vault):
    from veracage import cleanup
    _, argv = _run_open(monkeypatch, fake_vault, "kate")
    unit = next((a for a in argv if a.startswith("--unit=")), None)
    assert unit is not None
    assert unit.endswith(".service")
    expected = cleanup.vault_hash(str(fake_vault))[:8]
    assert expected in unit


def test_open_passes_vault_but_not_identity(monkeypatch, configured, fake_vault):
    """The launcher passes the vault path but NOT uid/gid/continuation: the
    privileged helper derives identity from PKEXEC_UID and pins the
    continuation itself, so a direct `pkexec` call can't choose --user 0."""
    _, argv = _run_open(monkeypatch, fake_vault, "kate")
    pairs = list(zip(argv, argv[1:]))
    assert ("--source", str(fake_vault)) in pairs
    assert ("--session", str(os.getuid())) in pairs   # the workspace session id
    assert ("--user", str(os.getuid())) not in pairs
    assert ("--group", str(os.getgid())) not in pairs
    assert "--continuation" not in argv


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


def test_open_omits_compositor_flag(monkeypatch, configured, fake_vault):
    """Phase 2: the leader no longer spawns a compositor, so cmd_open threads no
    --compositor to it — it attaches to the shared /run/veracage socket."""
    _, argv = _run_open(monkeypatch, fake_vault, "kate")
    sep = argv.index("--")
    assert "--compositor" not in argv[sep + 1:]


def test_open_without_app_launches_nothing(monkeypatch, configured, fake_vault):
    """`veracage open <vault>` (no app) auto-launches nothing — it publishes all
    enabled apps to the toolbar via --apps and passes no --first."""
    _, argv = _run_open(monkeypatch, fake_vault, app=None)
    sep = argv.index("--")
    after = argv[sep + 1:]
    assert after[0] == "_leader"
    pairs = dict(zip(after, after[1:]))
    # No --mountpoint: the helper computes /vaults/<label> post-cryptsetup and
    # injects it into the leader argv (the CLI can't know the label upfront).
    assert "--mountpoint" not in pairs
    assert "--first" not in after                # nothing auto-launched
    specs = json.loads(pairs["--apps"])          # all enabled apps for the toolbar
    execs = {s["exec"] for s in specs}
    assert "kate" in execs
    assert "okular" in execs


def test_open_with_app_sets_first(monkeypatch, configured, fake_vault):
    """Naming an app still auto-launches it (via --first) on top of publishing
    the full toolbar list."""
    _, argv = _run_open(monkeypatch, fake_vault, "okular")
    sep = argv.index("--")
    after = argv[sep + 1:]
    pairs = dict(zip(after, after[1:]))
    assert json.loads(pairs["--first"])["exec"] == "okular"
    assert "--apps" in pairs


def test_open_rejects_unenabled_app(monkeypatch, configured, fake_vault, capsys):
    rc, _ = _run_open(monkeypatch, fake_vault, "dolphin")
    assert rc == 2
    assert "not enabled" in capsys.readouterr().err


def test_open_rejects_missing_vault(monkeypatch, configured, tmp_path, capsys):
    rc, _ = _run_open(monkeypatch, tmp_path / "does-not-exist.vc", "kate")
    assert rc == 2
    assert "not found" in capsys.readouterr().err


def test_open_mounts_even_with_no_apps_configured(monkeypatch, tmp_xdg_config, fake_vault):
    """Shared-workspace model: apps and volumes are independent, so an empty app
    config no longer blocks a mount — the vault opens with nothing launched
    (--apps [], no --first), and the user enables/launches apps afterwards."""
    monkeypatch.setenv("WAYLAND_DISPLAY", "wayland-0")
    monkeypatch.setenv("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}")
    with mock.patch("subprocess.run") as run, mock.patch("subprocess.Popen"):
        run.return_value.returncode = 0
        cli.cmd_open(argparse.Namespace(
            vault=str(fake_vault), app=None, passphrase_stdin=False))
    argv = run.call_args.args[0]
    assert "pkexec" in argv                      # a mount WAS attempted
    after = argv[argv.index("--") + 1:]
    assert "--first" not in after                # nothing auto-launched
    assert json.loads(dict(zip(after, after[1:]))["--apps"]) == []  # no apps


# ---------------------------------------------- persistent compositor ----

# --------------------------------------- compositor brought up as own unit --

def test_open_brings_compositor_up_before_mount(monkeypatch, configured, fake_vault):
    """The compositor must be brought up as its own systemd unit BEFORE the mount
    (a compositor forked inside the session's unit dies when that unit stops)."""
    calls = []
    monkeypatch.setattr("veracage.cli.ensure_compositor_up",
                        lambda: calls.append("up") or 0)
    rc, argv = _run_open(monkeypatch, fake_vault, "kate")
    assert rc == 0
    assert calls == ["up"]                 # bring-up ran
    assert argv is not None                # and the mount followed


def test_open_aborts_if_compositor_wont_start(monkeypatch, configured, fake_vault):
    monkeypatch.setattr("veracage.cli.ensure_compositor_up", lambda: 1)
    rc, argv = _run_open(monkeypatch, fake_vault, "kate")
    assert rc == 1
    assert argv is None                    # never reached the mount pkexec


# ------------------------------------------- already-open probe (Phase 5) --

def test_open_refuses_when_bootstrap_volume_still_mounted(
        monkeypatch, configured, fake_vault, capsys):
    monkeypatch.setattr(
        "veracage.cli.leader.send_request",
        mock.Mock(return_value={"ok": True, "bootstrap_open": True,
                                "volumes": ["fake"]}))
    rc, argv = _run_open(monkeypatch, fake_vault, "kate")
    assert rc == 2
    assert argv is None                      # helper never invoked
    assert "already open" in capsys.readouterr().err


def test_open_proceeds_after_per_volume_close_of_bootstrap(
        monkeypatch, configured, fake_vault):
    """Regression: the leader keeps serving the bootstrap vault's socket for the
    whole session, so a bare ok:true reply used to make the vault unopenable
    after its volume was closed via Close volume — until the session ended."""
    monkeypatch.setattr(
        "veracage.cli.leader.send_request",
        mock.Mock(return_value={"ok": True, "bootstrap_open": False,
                                "volumes": ["other"]}))
    rc, argv = _run_open(monkeypatch, fake_vault, "kate")
    assert rc == 0
    assert argv is not None and "pkexec" in argv   # proceeded to the helper


def test_open_still_refuses_on_legacy_reply_without_bootstrap_open(
        monkeypatch, configured, fake_vault):
    monkeypatch.setattr(
        "veracage.cli.leader.send_request",
        mock.Mock(return_value={"ok": True}))
    rc, _ = _run_open(monkeypatch, fake_vault, "kate")
    assert rc == 2


def test_open_into_live_session_tells_user_about_the_app(
        monkeypatch, configured, fake_vault, capsys):
    """Regression: the helper's add-volume path drops the leader argv, so
    `veracage open B kate` on a live session mounts B but never launches kate —
    the CLI must say so instead of exiting 0 silently."""
    monkeypatch.setattr("veracage.cli._any_live_session", lambda: True)
    rc, _ = _run_open(monkeypatch, fake_vault, "kate")
    assert rc == 0
    err = capsys.readouterr().err
    assert "added to the running workspace" in err
    assert "no app was auto-launched" in err


def test_list_and_close_survive_a_stale_socket(monkeypatch, tmp_path, capsys):
    """Regression: a socket file with a dead leader raises ConnectionRefusedError,
    which cmd_list/cmd_close used to let traceback."""
    stale = tmp_path / "stale.sock"
    stale.write_text("")
    monkeypatch.setattr("veracage.cli.leader.send_request",
                        mock.Mock(side_effect=ConnectionRefusedError))
    monkeypatch.setattr("veracage.cli.leader.session_socket_path",
                        lambda _v: stale)
    ns = argparse.Namespace(vault=str(tmp_path / "x.vc"))
    assert cli.cmd_list(ns) == 2
    assert not stale.exists()               # stale socket removed
    stale.write_text("")
    assert cli.cmd_close(ns) == 2
    assert not stale.exists()
    assert "no active session" in capsys.readouterr().err


def test_cmd_compositor_execs_binary(monkeypatch, tmp_path):
    exe = tmp_path / "veracage-compositor"
    exe.write_text("#!/bin/sh\n")
    exe.chmod(0o755)
    monkeypatch.setattr("veracage.cli.COMPOSITOR_PATH", str(exe))
    captured = {}
    monkeypatch.setattr("veracage.cli.os.execv",
                        lambda path, argv: captured.update(path=path, argv=argv))
    cli.cmd_compositor(argparse.Namespace(socket="wl-vc"))
    assert captured["path"] == str(exe)
    assert captured["argv"] == [str(exe), "--socket", "wl-vc"]
