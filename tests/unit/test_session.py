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
