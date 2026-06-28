"""Vault-side leader control protocol (increment 1)."""
from __future__ import annotations

import json
import os
import socket
import threading
import time
from pathlib import Path

from veracage import leader


def _state(**kw):
    return leader._LeaderState(mountpoint="/run/veracage/x", **kw)


# --------------------------------------------------------------- protocol --

def test_ping_returns_uid():
    r = leader._handle_request(_state(), {"cmd": "ping"})
    assert r["ok"] is True
    assert r["uid"] == os.getuid()
    assert r["mountpoint"] == "/run/veracage/x"


def test_list_empty():
    assert leader._handle_request(_state(), {"cmd": "list"}) == {"ok": True, "apps": []}


def test_close_sets_closing():
    st = _state()
    assert leader._handle_request(st, {"cmd": "close"}) == {"ok": True}
    assert st.closing is True


def test_unknown_cmd():
    r = leader._handle_request(_state(), {"cmd": "nope"})
    assert r["ok"] is False and "unknown" in r["error"]


def test_non_dict_request():
    assert leader._handle_request(_state(), ["not", "a", "dict"])["ok"] is False


# ------------------------------------------------------------------ exec ---

def test_exec_without_weston_is_refused():
    r = leader._handle_request(_state(), {"cmd": "exec", "app": {"exec": "kate"}})
    assert r["ok"] is False and "compositor" in r["error"]


def test_exec_missing_spec():
    st = _state(weston_socket=Path("/run/x/wayland-1"))
    assert leader._handle_request(st, {"cmd": "exec"})["ok"] is False


def test_exec_needs_exec_field():
    st = _state(weston_socket=Path("/run/x/wayland-1"))
    r = leader._handle_request(st, {"cmd": "exec", "app": {"args": ["x"]}})
    assert r["ok"] is False and "exec" in r["error"]


def test_exec_launches_and_tracks(monkeypatch):
    monkeypatch.setattr(leader, "bwrap_command", lambda mp, a, ws, gpu: ["true"])

    class FakeProc:
        pid = 4321
    monkeypatch.setattr(leader.subprocess, "Popen", lambda argv: FakeProc())

    st = _state(weston_socket=Path("/run/x/wayland-1"))
    r = leader._handle_request(
        st, {"cmd": "exec", "app": {"name": "Kate", "exec": "kate", "args": ["/vault"]}})
    assert r == {"ok": True, "pid": 4321}
    assert st.children == {4321: "Kate"}


def test_exec_runs_exactly_what_it_is_handed(monkeypatch):
    """No config lookup / allowlist on the vault side — the leader runs the
    resolved command verbatim (bwrap is what prevents exfiltration)."""
    captured = {}

    def fake_bwrap(mp, app, ws, gpu):
        captured["exec"], captured["args"] = app.exec, app.args
        return ["true"]
    monkeypatch.setattr(leader, "bwrap_command", fake_bwrap)
    monkeypatch.setattr(leader.subprocess, "Popen",
                        lambda argv: type("P", (), {"pid": 1})())

    st = _state(weston_socket=Path("/run/x/wayland-1"))
    leader._handle_request(
        st, {"cmd": "exec", "app": {"exec": "okular", "args": ["/vault/a.pdf"]}})
    assert captured == {"exec": "okular", "args": ["/vault/a.pdf"]}


def test_exec_missing_dependency(monkeypatch):
    monkeypatch.setattr(leader, "bwrap_command", lambda *a: ["bwrap"])

    def boom(argv):
        raise FileNotFoundError(2, "no such file", "bwrap")
    monkeypatch.setattr(leader.subprocess, "Popen", boom)

    st = _state(weston_socket=Path("/run/x/wayland-1"))
    r = leader._handle_request(st, {"cmd": "exec", "app": {"exec": "kate"}})
    assert r["ok"] is False and "missing dependency" in r["error"]


# ---------------------------------------------------------------- reaping --

def test_reap_children_drops_exited():
    st = _state()
    pid = os.fork()
    if pid == 0:
        os._exit(0)
    st.children[pid] = "x"
    for _ in range(50):
        leader._reap_children(st)
        if pid not in st.children:
            break
        time.sleep(0.02)
    assert pid not in st.children


# -------------------------------------------------------- wire round-trip --

def test_wire_roundtrip_via_accept_one(tmp_path):
    """A real listening socket + client connection through _accept_one,
    exercising the JSON line framing (the path the inherited fd takes)."""
    sock_path = tmp_path / "ctl.sock"
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind(str(sock_path))
    srv.listen(1)
    replies: list = []

    def client():
        c = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        c.connect(str(sock_path))
        c.sendall(b'{"cmd": "ping"}\n')
        data = b""
        while not data.endswith(b"\n"):
            data += c.recv(4096)
        replies.append(json.loads(data))
        c.close()

    t = threading.Thread(target=client)
    t.start()
    leader._accept_one(srv, _state())
    t.join(timeout=2)
    srv.close()
    assert replies and replies[0]["ok"] and replies[0]["uid"] == os.getuid()
