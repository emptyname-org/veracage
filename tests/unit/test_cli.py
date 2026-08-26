"""CLI argv plumbing: pkexec command shape, env-forwarding, validation."""
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
    # publish_apps resolves fonts via fc-match/kreadconfig and writes the live
    # /run/veracage/pub dir; neutralise it so the argv tests neither shell out
    # (subprocess.run is mocked here) nor mutate a running session's state.
    monkeypatch.setattr("veracage.cli.config.publish_apps", lambda _c: None)


@pytest.fixture
def fake_vault(tmp_path):
    p = tmp_path / "fake.vc"
    p.write_bytes(b"\x00" * 1024)
    return p


@pytest.fixture
def configured(tmp_xdg_config):
    cfg = config.Config(
        apps={"kate": apps.App("kate", "Kate", "kate"),
              "okular": apps.App("okular", "Okular", "okular")},
        last_used_app="kate",
    )
    config.save(cfg)
    return cfg


def _run_open(monkeypatch, vault, app=None):
    monkeypatch.setenv("WAYLAND_DISPLAY", "wayland-0")
    monkeypatch.setenv("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}")
    ns = argparse.Namespace(volume=str(vault), app=app, passphrase_stdin=False)
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
    the piped passphrase reaches the helper, and forwards --passphrase-stdin."""
    monkeypatch.setenv("WAYLAND_DISPLAY", "wayland-0")
    monkeypatch.setenv("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}")
    ns = argparse.Namespace(volume=str(fake_vault), app=None, passphrase_stdin=True)
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


def test_open_unit_description_omits_the_volume_filename(monkeypatch, configured, fake_vault):
    """The unit description reaches the journal and outlives the session, so it
    names the volume by the same hash as the unit, never by its filename."""
    from veracage import cleanup
    _, argv = _run_open(monkeypatch, fake_vault, "kate")
    desc = next((a for a in argv if a.startswith("--description=")), None)
    assert desc is not None
    assert fake_vault.name not in desc
    assert cleanup.vault_hash(str(fake_vault))[:8] in desc


def test_open_passes_vault_but_not_identity(monkeypatch, configured, fake_vault):
    """The launcher passes the vault path but NOT uid/gid/continuation: the
    privileged helper derives identity from PKEXEC_UID and pins the
    continuation itself, so a direct `pkexec` call can't choose --user 0."""
    _, argv = _run_open(monkeypatch, fake_vault, "kate")
    pairs = list(zip(argv, argv[1:]))
    assert ("--source", str(fake_vault)) in pairs
    assert ("--session", str(os.getuid())) in pairs   # the workspace session id
    # Check the HELPER's own argv (everything from `pkexec` on), and check the
    # FLAG rather than one value of it: asserting only that it is not
    # `--user <our own uid>` left `--user 0`, the escalation this test is named
    # for, passing. (`systemd-run --user` earlier in the line is unrelated.)
    helper_argv = argv[argv.index("pkexec"):]
    assert "--user" not in helper_argv
    assert "--group" not in helper_argv
    assert "--continuation" not in helper_argv


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
    """The leader does not spawn a compositor, so cmd_open threads no
    --compositor to it. It attaches to the shared /run/veracage socket."""
    _, argv = _run_open(monkeypatch, fake_vault, "kate")
    sep = argv.index("--")
    assert "--compositor" not in argv[sep + 1:]


def test_open_without_app_launches_nothing(monkeypatch, configured, fake_vault):
    """`veracage open <vault>` (no app) auto-launches nothing: it publishes all
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
    config no longer blocks a mount. The vault opens with nothing launched
    (--apps [], no --first), and the user enables/launches apps afterwards."""
    monkeypatch.setenv("WAYLAND_DISPLAY", "wayland-0")
    monkeypatch.setenv("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}")
    with mock.patch("subprocess.run") as run, mock.patch("subprocess.Popen"):
        run.return_value.returncode = 0
        cli.cmd_open(argparse.Namespace(
            volume=str(fake_vault), app=None, passphrase_stdin=False))
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
    whole session, so a bare ok:true reply must not make the vault unopenable
    after its volume was closed via Close volume, until the session ended."""
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


def test_open_into_live_session_reports_the_join(
        monkeypatch, configured, fake_vault, capsys):
    """`veracage open B kate` on a live session mounts B into the running
    workspace (the helper hands the --first app to the leader via launch.req).
    The CLI reports the join so an exit 0 isn't silent."""
    monkeypatch.setattr("veracage.cli._any_live_session", lambda: True)
    rc, _ = _run_open(monkeypatch, fake_vault, "kate")
    assert rc == 0
    err = capsys.readouterr().err
    assert "added to the running workspace" in err


# ------------------------------------------ empty session (front-door B) ---

def test_empty_session_bootstraps_when_none_live(monkeypatch, configured):
    """The front door stands up a session leader with --empty-session (no
    --source, no passphrase) so apps are launchable before any mount."""
    monkeypatch.setenv("WAYLAND_DISPLAY", "wayland-0")
    monkeypatch.setenv("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}")
    monkeypatch.setattr("veracage.cli._any_live_session", lambda: False)
    with mock.patch("subprocess.run") as run:
        run.return_value.returncode = 0
        cli.ensure_empty_session()
    argv = run.call_args.args[0]
    assert "pkexec" in argv
    assert "--empty-session" in argv
    assert "--source" not in argv                     # no volume
    assert "--passphrase-stdin" not in argv
    after = argv[argv.index("--") + 1:]
    assert "--first" not in after                      # nothing auto-launched
    assert json.loads(dict(zip(after, after[1:]))["--apps"])  # app list forwarded
    assert any(p.startswith("ExecStopPost=") for p in argv)   # crash-safe teardown


def test_empty_session_skipped_when_one_is_live(monkeypatch, configured):
    monkeypatch.setattr("veracage.cli._any_live_session", lambda: True)
    with mock.patch("subprocess.run") as run:
        cli.ensure_empty_session()
    assert not run.called                              # idempotent no-op


def test_up_brings_compositor_then_empty_session(monkeypatch):
    order = []
    monkeypatch.setattr("veracage.cli.ensure_compositor_up",
                        lambda: order.append("compositor") or 0)
    monkeypatch.setattr("veracage.cli.config.publish_apps", lambda _c: None)
    monkeypatch.setattr("veracage.cli.ensure_empty_session",
                        lambda: order.append("session"))
    assert cli.cmd_up(argparse.Namespace()) == 0
    assert order == ["compositor", "session"]          # compositor first


def test_list_and_close_survive_a_stale_socket(monkeypatch, tmp_path, capsys):
    """Regression: a socket file with a dead leader raises ConnectionRefusedError,
    which cmd_list/cmd_close must handle rather than traceback."""
    stale = tmp_path / "stale.sock"
    stale.write_text("")
    monkeypatch.setattr("veracage.cli.leader.send_request",
                        mock.Mock(side_effect=ConnectionRefusedError))
    monkeypatch.setattr("veracage.cli.leader.session_socket_path",
                        lambda _v: stale)
    ns = argparse.Namespace(volume=str(tmp_path / "x.vc"))
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


def test_the_stop_post_helper_path_is_quoted(monkeypatch, configured, fake_vault):
    """systemd re-tokenizes an ExecStopPost value on whitespace. An unquoted
    helper path under a directory with a space would silently register a
    different command, and the crash teardown that closes the dm devices would
    never run."""
    _, argv = _run_open(monkeypatch, fake_vault, "kate")
    stop_post = next(a for a in argv if a.startswith("ExecStopPost="))
    assert '"' in stop_post, stop_post
    quoted = stop_post.split("pkexec ", 1)[1]
    assert quoted.startswith('"') and '" --session ' in quoted


def test_the_volume_path_is_canonicalized_before_the_helper_sees_it(
        monkeypatch, configured, tmp_path):
    """The helper is handed a path it opens as root. Resolving it here keeps a
    relative path or a symlinked directory from being what reaches it."""
    real = tmp_path / "real.vc"
    real.write_bytes(b"x")
    link = tmp_path / "link.vc"
    link.symlink_to(real)
    _, argv = _run_open(monkeypatch, link, None)
    pairs = list(zip(argv, argv[1:]))
    assert ("--source", str(real)) in pairs
    assert ("--source", str(link)) not in pairs


def test_a_probe_that_cannot_answer_refuses_rather_than_risking_a_double_mount(
        monkeypatch, configured, fake_vault, capsys):
    """A session MAY be alive but too busy to answer. Unlinking its socket and
    opening anyway would cryptsetup-open the same container twice and rw-mount
    one filesystem twice: corruption."""
    def busy(*a, **k):
        raise TimeoutError("no reply")
    monkeypatch.setattr(cli.leader, "send_request", busy)
    with mock.patch("veracage.cli.subprocess.run") as run:
        run.return_value.returncode = 0
        rc = cli.cmd_open(argparse.Namespace(volume=str(fake_vault), app=None,
                                             passphrase_stdin=False))
    assert rc == 2
    run.assert_not_called()   # the helper must not be invoked
    assert "double mount" in capsys.readouterr().err


def test_no_unlisted_environment_variable_reaches_the_helper(
        monkeypatch, configured, fake_vault):
    """pkexec strips the environment and the helper re-applies an allowlist; the
    CLI must not widen it. LD_PRELOAD is the one that turns a root helper into
    arbitrary root code."""
    monkeypatch.setenv("LD_PRELOAD", "/tmp/evil.so")
    monkeypatch.setenv("LD_LIBRARY_PATH", "/tmp/evil")
    _, argv = _run_open(monkeypatch, fake_vault, "kate")
    joined = " ".join(argv)
    assert "LD_PRELOAD" not in joined
    assert "LD_LIBRARY_PATH" not in joined


# The autouse fixture above stubs `ensure_compositor_up` for the argv tests;
# the test below is about that function itself, so it keeps the real one.
_REAL_ENSURE_COMPOSITOR_UP = cli.ensure_compositor_up


def test_log_dir_reaches_the_compositor_only_when_it_is_a_directory(
        monkeypatch, tmp_xdg_config, tmp_path, capsys):
    """`log_dir` is forwarded as VERACAGE_LOG_DIR, but a path that is not a
    directory is refused here: the compositor has no stdio, so it could not
    report a log file it failed to open."""
    monkeypatch.setenv("VERACAGE_LOG_DIR", "")   # so monkeypatch owns the key
    monkeypatch.setattr("veracage.wayland.compositor_is_up", lambda: False)

    def spawn_argv(**cfg_kwargs) -> str:
        os.environ.pop("VERACAGE_LOG_DIR", None)
        config.save(config.Config(apps={}, **cfg_kwargs))
        with mock.patch("subprocess.run") as run:
            run.return_value.returncode = 1      # stop before the socket wait
            _REAL_ENSURE_COMPOSITOR_UP()
        return " ".join(run.call_args.args[0])

    logs = tmp_path / "logs"
    logs.mkdir()
    assert f"VERACAGE_LOG_DIR={logs}" in spawn_argv(debug=True, log_dir=str(logs))
    assert "VERACAGE_LOG_DIR" not in spawn_argv(debug=True,
                                                log_dir=str(tmp_path / "gone"))
    assert "not a directory" in capsys.readouterr().err
    # No debug logging, nothing to place: the variable stays out of the env.
    assert "VERACAGE_LOG_DIR" not in spawn_argv(debug=False, log_dir=str(logs))


def _pkaction_result(stdout: str, rc: int = 0):
    r = mock.Mock()
    r.returncode = rc
    r.stdout = stdout
    return r


def test_a_polkit_policy_naming_another_helper_is_reported():
    """Two installs (a .deb over a `make install`) share one policy file, so the
    one that lost names the other's helper and every pkexec asks for a password.
    The complaint has to name both paths: that is what tells them apart."""
    other = "/usr/libexec/veracage/veracage-helper"
    annotation = "  annotation:  org.freedesktop.policykit.exec.path -> {}\n"

    with mock.patch("subprocess.run",
                    return_value=_pkaction_result(annotation.format(cli.HELPER_PATH))):
        assert cli._polkit_helper_complaint() is None

    with mock.patch("subprocess.run",
                    return_value=_pkaction_result(annotation.format(other))):
        complaint = cli._polkit_helper_complaint()
    assert complaint is not None
    assert other in complaint and cli.HELPER_PATH in complaint

    # polkitd drops a malformed policy file whole, so the action goes missing.
    with mock.patch("subprocess.run", return_value=_pkaction_result("", rc=1)):
        assert "policy file" in (cli._polkit_helper_complaint() or "")

    # No pkaction on the host: nothing to check against, so say nothing.
    with mock.patch("subprocess.run", side_effect=FileNotFoundError):
        assert cli._polkit_helper_complaint() is None


def test_a_bare_veracage_starts_veracage(monkeypatch):
    """`veracage` with no subcommand is what a person types to start it. It has
    to do what the desktop entry does, which is exec the broker, rather than
    answer with an argparse usage error."""
    execs: list[tuple] = []
    monkeypatch.setattr(os, "execvp", lambda path, argv: execs.append((path, argv)))
    monkeypatch.setattr("sys.argv", ["veracage"])
    cli.main()
    assert execs == [(cli.AGENT_PATH, [cli.AGENT_PATH])]


def test_a_bare_veracage_says_so_when_the_broker_is_missing(monkeypatch, capsys):
    monkeypatch.setattr(os, "execvp",
                        lambda *_: (_ for _ in ()).throw(FileNotFoundError("no agent")))
    monkeypatch.setattr("sys.argv", ["veracage"])
    assert cli.main() == 1
    assert "cannot start" in capsys.readouterr().err


def test_help_lists_only_the_commands_a_person_can_type(monkeypatch, capsys):
    """The internal subcommands are registered with help=SUPPRESS, which argparse
    ignores for subcommands: they used to be listed as "==SUPPRESS=="."""
    monkeypatch.setattr("sys.argv", ["veracage", "--help"])
    with pytest.raises(SystemExit):
        cli.main()
    out = capsys.readouterr().out
    assert "SUPPRESS" not in out
    assert "_leader" not in out and "_compositor" not in out
    for public in ("open", "list", "close", "close-volume", "configure"):
        assert public in out
