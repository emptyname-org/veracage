"""Session control-socket protocol — request/reply shapes, error paths.

We patch out subprocess.Popen and weston so we never spawn real processes;
the tests only exercise the message-handling logic.
"""
from __future__ import annotations

from unittest import mock

import pytest

from veracage import apps, config, session


@pytest.fixture
def state(tmp_path) -> session._SessionState:
    return session._SessionState(
        mountpoint="/run/veracage/abcd",
        vault="/tmp/test.vc",
        weston_socket=tmp_path / "weston.sock",
    )


@pytest.fixture
def configured(tmp_xdg_config):
    config.save(config.Config(
        apps={"kate": apps.KNOWN_APPS["kate"], "okular": apps.KNOWN_APPS["okular"]},
        last_used_app="kate",
    ))


# ----------------------------------------------------------------- exec ----

def test_exec_accepts_enabled_app(state, configured):
    fake_proc = mock.Mock(pid=4242)
    with mock.patch("veracage.session.subprocess.Popen", return_value=fake_proc) as p:
        reply = session._handle_request(state, {"cmd": "exec", "app": "kate"})
    assert reply == {"ok": True, "pid": 4242}
    assert state.children == {4242: "kate"}
    p.assert_called_once()
    argv = p.call_args.args[0]
    assert argv[0] == "bwrap"
    assert "kate" in argv


def test_exec_rejects_unknown_app(state, configured):
    reply = session._handle_request(state, {"cmd": "exec", "app": "vim"})
    assert reply["ok"] is False
    assert "not enabled" in reply["error"]
    assert state.children == {}


def test_exec_handles_missing_binary(state, configured):
    with mock.patch(
        "veracage.session.subprocess.Popen",
        side_effect=FileNotFoundError(2, "x", "bwrap"),
    ):
        reply = session._handle_request(state, {"cmd": "exec", "app": "kate"})
    assert reply["ok"] is False
    assert "missing dependency" in reply["error"]


def test_exec_passes_gpu_flag_through(state, configured):
    """state.gpu must reach bwrap_command (regression: first app + exec)."""
    state.gpu = True
    captured = {}

    def fake_bwrap(_mp, app, _sock, gpu=False):
        captured["gpu"] = gpu
        return ["bwrap", app.exec]

    with mock.patch("veracage.session.bwrap_command", side_effect=fake_bwrap), \
         mock.patch("veracage.session.subprocess.Popen",
                    return_value=mock.Mock(pid=7)):
        session._handle_request(state, {"cmd": "exec", "app": "kate"})
    assert captured["gpu"] is True


def test_exec_rejects_non_string_app(state, configured):
    reply = session._handle_request(state, {"cmd": "exec", "app": 123})
    assert reply["ok"] is False
    assert state.children == {}


# ----------------------------------------------------------------- list ----

def test_list_returns_children(state):
    state.children[101] = "kate"
    state.children[102] = "okular"
    reply = session._handle_request(state, {"cmd": "list"})
    assert reply["ok"] is True
    by_pid = {e["pid"]: e["app"] for e in reply["apps"]}
    assert by_pid == {101: "kate", 102: "okular"}


def test_list_empty_when_no_children(state):
    reply = session._handle_request(state, {"cmd": "list"})
    assert reply == {"ok": True, "apps": []}


# ---------------------------------------------------------------- close ----

def test_close_sets_flag(state):
    assert state.closing is False
    reply = session._handle_request(state, {"cmd": "close"})
    assert reply == {"ok": True}
    assert state.closing is True


# --------------------------------------------------------------- unknown ---

def test_unknown_cmd(state):
    reply = session._handle_request(state, {"cmd": "what"})
    assert reply["ok"] is False
    assert "unknown" in reply["error"]


def test_missing_cmd_field(state):
    reply = session._handle_request(state, {})
    assert reply["ok"] is False


# --------------------------------------------------------------- reaping ---

def test_reap_removes_only_exited_tracked_children(state):
    state.children[101] = "kate"
    state.children[102] = "okular"

    def fake_waitpid(pid, _flags):
        return (101, 0) if pid == 101 else (0, 0)  # 101 exited, 102 running

    with mock.patch("veracage.session.os.waitpid", side_effect=fake_waitpid):
        session._reap_children(state)
    assert state.children == {102: "okular"}


def test_reap_drops_already_reaped_child(state):
    state.children[201] = "kate"
    with mock.patch("veracage.session.os.waitpid", side_effect=ChildProcessError):
        session._reap_children(state)
    assert state.children == {}


def test_reap_never_waits_on_minus_one(state):
    """Regression: reaping must target tracked PIDs only, never waitpid(-1),
    which would race weston/agent reaping and clobber their exit status."""
    state.children[301] = "kate"
    seen = []

    def fake_waitpid(pid, _flags):
        seen.append(pid)
        return (0, 0)

    with mock.patch("veracage.session.os.waitpid", side_effect=fake_waitpid):
        session._reap_children(state)
    assert seen == [301]
    assert -1 not in seen


# ------------------------------------------------ control-socket robustness -

def test_handle_request_rejects_non_dict(state):
    """A valid-JSON non-object (e.g. []) must not crash the handler (H2)."""
    reply = session._handle_request(state, [])
    assert reply == {"ok": False, "error": "request must be a JSON object"}


def _fake_server(payload: bytes):
    """A mock srv whose accept() yields one pre-fed connection; returns
    (mock_srv, client_side) — read the reply off client_side."""
    import socket as _socket
    srv_side, cli_side = _socket.socketpair()
    cli_side.sendall(payload)
    fake = mock.Mock()
    fake.accept.return_value = (srv_side, None)
    return fake, cli_side


def test_accept_one_survives_non_dict_request(state):
    """_accept_one must reply with an error, not raise, for a non-dict body."""
    fake_srv, cli = _fake_server(b"[]\n")
    session._accept_one(fake_srv, state)   # must not raise
    assert b'"ok": false' in cli.recv(4096)
    cli.close()


def test_accept_one_caps_oversized_request(state):
    """An unbounded trickle is rejected, not accumulated forever (M2)."""
    fake_srv, cli = _fake_server(b"x" * (70 * 1024))  # no newline, over the cap
    session._accept_one(fake_srv, state)
    assert b"too large" in cli.recv(4096)
    cli.close()


# ---------------------------------------------------------- socket path ----

def test_session_socket_path_is_per_vault(tmp_xdg_runtime):
    a = session.session_socket_path("/tmp/a.vc")
    b = session.session_socket_path("/tmp/b.vc")
    assert a != b
    assert a.parent == b.parent
    assert a.suffix == ".sock"


def test_session_socket_path_is_stable(tmp_xdg_runtime):
    """Same vault path → same socket path. Critical for `veracage exec`."""
    a = session.session_socket_path("/tmp/x.vc")
    b = session.session_socket_path("/tmp/x.vc")
    assert a == b
