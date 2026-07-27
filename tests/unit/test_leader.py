"""Vault-side leader control protocol (increment 1)."""
from __future__ import annotations

import json
import os
import socket
import threading
import time
from pathlib import Path

import pytest

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
    r = leader._handle_request(_state(), {"cmd": "list"})
    assert r["ok"] is True and r["apps"] == []
    # the reply also reports the open-volume set for the CLI's already-open probe
    assert r["volumes"] == []            # /run/veracage/x doesn't exist here
    assert r["bootstrap_open"] is False


def test_list_reports_bootstrap_open(tmp_path, monkeypatch):
    """After a per-volume close of the bootstrap volume, `bootstrap_open` must go
    False so `veracage open <that vault>` is allowed again (the socket itself
    keeps serving for the rest of the session)."""
    monkeypatch.setattr(leader, "WORKSPACE", tmp_path)
    (tmp_path / "Work").mkdir()
    st = leader._LeaderState(mountpoint=str(tmp_path / "Work"))
    r = leader._handle_request(st, {"cmd": "list"})
    assert r["volumes"] == ["Work"]
    assert r["bootstrap_open"] is True
    (tmp_path / "Work").rmdir()          # the volume was closed
    r = leader._handle_request(st, {"cmd": "list"})
    assert r["bootstrap_open"] is False


def test_close_sets_closing():
    st = _state()
    assert leader._handle_request(st, {"cmd": "close"}) == {"ok": True}
    assert st.closing is True


def test_set_apps_replaces_list_and_rewrites_apps_file(tmp_path, monkeypatch):
    monkeypatch.setattr(leader, "COMPOSITOR_RUNTIME", tmp_path)
    st = leader._LeaderState(mountpoint="/run/veracage/deadbeef",
                             app_specs=[{"name": "Kate", "exec": "kate"}])
    r = leader._handle_request(
        st, {"cmd": "set-apps",
             "apps": [{"name": "Kate", "exec": "kate"},
                      {"name": "Gimp", "exec": "gimp"}]})
    assert r == {"ok": True}
    assert [a["exec"] for a in st.app_specs] == ["kate", "gimp"]
    lines = (tmp_path / "app-deadbeef.apps").read_text().splitlines()
    assert lines[4:] == ["Kate", "Gimp"]


def test_set_apps_rejects_malformed():
    st = _state()
    for bad in (None, "kate", [{"name": "x"}], [{"exec": ""}],
                [{"exec": "x" * 600}], [{"exec": "kate", "name": "n" * 200}],
                [{"exec": "kate"}] * 65,
                # exec must be a BARE command: no args, whitespace, shell
                # metacharacters, or control chars (confused-deputy hardening).
                [{"exec": "kate --evil"}], [{"exec": "sh -c id"}],
                [{"exec": "a;b"}], [{"exec": "a|b"}], [{"exec": "a$(id)"}],
                [{"exec": "a\tb"}], [{"exec": "a\nb"}]):
        r = leader._handle_request(st, {"cmd": "set-apps", "apps": bad})
        assert r["ok"] is False
    assert st.app_specs == []  # untouched on every rejection


def test_set_apps_accepts_bare_paths(tmp_path, monkeypatch):
    monkeypatch.setattr(leader, "COMPOSITOR_RUNTIME", tmp_path)
    st = leader._LeaderState(mountpoint="/run/veracage/deadbeef")
    r = leader._handle_request(st, {"cmd": "set-apps", "apps": [
        {"name": "Kate", "exec": "kate"},
        {"name": "App", "exec": "/opt/app/bin/app-1.2"},
    ]})
    assert r["ok"] is True
    assert [a["exec"] for a in st.app_specs] == ["kate", "/opt/app/bin/app-1.2"]


def test_unknown_cmd():
    r = leader._handle_request(_state(), {"cmd": "nope"})
    assert r["ok"] is False and "unknown" in r["error"]


def test_non_dict_request():
    assert leader._handle_request(_state(), ["not", "a", "dict"])["ok"] is False


# --------------------------------------------------------------- launch ----
# `_launch_app` is the ONLY launch path. It is fed specs from the leader's own
# enabled list (first_app at open, or app_specs[idx] on a toolbar click), never
# from a control-socket peer.

def test_control_socket_has_no_launch_or_bridge():
    """Regression for the pen-test finding: the human-owned control socket must
    NOT expose exec / import / export / outbox. Any process running as the human
    uid can reach it, so those would be a same-uid vault-exfiltration primitive."""
    st = _state(wl_socket=Path("/run/x/wayland-1"))
    for cmd in ("exec", "import", "export", "outbox"):
        r = leader._handle_request(st, {"cmd": cmd, "app": {"exec": "kate"}})
        assert r["ok"] is False and "unknown cmd" in r["error"]


def test_launch_app_without_compositor_is_refused():
    r = leader._launch_app(_state(), {"exec": "kate"})
    assert r["ok"] is False and "compositor" in r["error"]


def test_launch_app_missing_spec():
    st = _state(wl_socket=Path("/run/x/wayland-1"))
    assert leader._launch_app(st, None)["ok"] is False


def test_launch_app_needs_exec_field():
    st = _state(wl_socket=Path("/run/x/wayland-1"))
    r = leader._launch_app(st, {"args": ["x"]})
    assert r["ok"] is False and "exec" in r["error"]


def test_launch_app_launches_and_tracks(monkeypatch):
    monkeypatch.setattr(leader, "bwrap_command",
                        lambda mp, a, ws, places=None, exchange=None: ["true"])

    captured: dict = {}

    class FakeProc:
        pid = 4321

    def fake_popen(argv, **kw):
        captured.update(kw)
        return FakeProc()
    monkeypatch.setattr(leader.subprocess, "Popen", fake_popen)

    st = _state(wl_socket=Path("/run/x/wayland-1"))
    r = leader._launch_app(st, {"name": "Kate", "exec": "kate", "args": ["/vault"]})
    assert r == {"ok": True, "pid": 4321}
    # Tracked as (label, launch_monotonic) so the reaper can spot an early exit.
    label, launched_at = st.children[4321]
    assert label == "Kate"
    assert isinstance(launched_at, float)
    # M1: the app's stdio must be detached so a viewer can't leak /vault paths to
    # the leader's terminal/journal.
    assert captured["stdin"] == leader.subprocess.DEVNULL
    assert captured["stdout"] == leader.subprocess.DEVNULL
    assert captured["stderr"] == leader.subprocess.DEVNULL


def test_consume_launch_request_launches_and_removes(tmp_path, monkeypatch):
    """The helper's add-volume path drops launch.req (root-written) into the
    vault runtime dir; the leader launches the spec once and removes the file."""
    monkeypatch.setenv("XDG_RUNTIME_DIR", str(tmp_path))
    (tmp_path / "launch.req").write_text('{"name": "Dolphin", "exec": "dolphin"}')
    launched = []
    monkeypatch.setattr(
        leader, "_launch_app",
        lambda st, spec: launched.append(spec) or {"ok": True})
    leader._consume_launch_request(_state(wl_socket=Path("/run/x/wayland-1")))
    assert launched == [{"name": "Dolphin", "exec": "dolphin"}]
    assert not (tmp_path / "launch.req").exists()


def test_consume_launch_request_noop_without_file(tmp_path, monkeypatch):
    monkeypatch.setenv("XDG_RUNTIME_DIR", str(tmp_path))
    monkeypatch.setattr(
        leader, "_launch_app",
        lambda st, spec: pytest.fail("must not launch"))
    leader._consume_launch_request(_state(wl_socket=Path("/run/x/wayland-1")))


def test_consume_launch_request_rejects_bad_specs(tmp_path, monkeypatch, capsys):
    """Malformed JSON and a metacharacter exec are dropped (file removed, no
    launch): the same bare-command rule as set-apps."""
    monkeypatch.setenv("XDG_RUNTIME_DIR", str(tmp_path))
    monkeypatch.setattr(
        leader, "_launch_app",
        lambda st, spec: pytest.fail("must not launch"))
    st = _state(wl_socket=Path("/run/x/wayland-1"))
    for body in ('{"exec": "rm -rf /"}', "not json", '{"name": "x"}'):
        (tmp_path / "launch.req").write_text(body)
        leader._consume_launch_request(st)
        assert not (tmp_path / "launch.req").exists()
    assert capsys.readouterr().err.count("malformed launch request") == 3


def test_reap_reports_immediate_exit(tmp_path, monkeypatch):
    # An app that exits within _EARLY_EXIT_SECONDS of launch is a failed launch:
    # the reaper drops it and publishes a notice for the compositor to show. An
    # X11-only GUI in this Wayland-only sandbox is the motivating case.
    monkeypatch.setattr(leader, "COMPOSITOR_RUNTIME", tmp_path)
    monkeypatch.setattr(leader.os, "waitpid", lambda pid, flags: (pid, 0))

    st = _state(wl_socket=Path("/run/x/wayland-1"))
    st.children = {99: ("VeraCrypt", time.monotonic())}   # just launched, now dead
    leader._reap_children(st)

    assert st.children == {}
    nonce, _, text = (tmp_path / "notice").read_text().partition("\t")
    assert nonce.isdigit()
    assert "VeraCrypt failed to launch (exited immediately)" in text


def test_reap_does_not_report_normal_quit(tmp_path, monkeypatch):
    # An app the user ran and closed later (exited well after launch) is reaped
    # silently, with no failed-launch notice.
    monkeypatch.setattr(leader, "COMPOSITOR_RUNTIME", tmp_path)
    monkeypatch.setattr(leader.os, "waitpid", lambda pid, flags: (pid, 0))

    st = _state(wl_socket=Path("/run/x/wayland-1"))
    old = time.monotonic() - (leader._EARLY_EXIT_SECONDS + 5.0)
    st.children = {7: ("Kate", old)}
    leader._reap_children(st)

    assert st.children == {}
    assert not (tmp_path / "notice").exists()


def test_reap_takes_the_progress_note_down(tmp_path, monkeypatch):
    # The note turns the compositor's spinner on, so an exited app must take it
    # down: whether the launch failed or the app simply quit, it is resolved. A
    # note left behind spins on, and outlives the session in the shared runtime
    # directory.
    monkeypatch.setattr(leader, "COMPOSITOR_RUNTIME", tmp_path)
    monkeypatch.setattr(leader.os, "waitpid", lambda pid, flags: (pid, 0))
    status = tmp_path / "status"

    for launched_at, label in [(time.monotonic(), "VeraCrypt"),                    # failed
                               (time.monotonic() - 60.0, "Kate")]:                 # quit later
        status.write_text("1\tStarting something\n")
        st = _state(wl_socket=Path("/run/x/wayland-1"))
        st.children = {11: (label, launched_at)}
        leader._reap_children(st)
        assert not status.exists(), f"note survived the {label} exit"


def test_launch_app_missing_dependency(monkeypatch):
    monkeypatch.setattr(leader, "bwrap_command", lambda *a, **k: ["bwrap"])

    def boom(argv, **kw):
        raise FileNotFoundError(2, "no such file", "bwrap")
    monkeypatch.setattr(leader.subprocess, "Popen", boom)

    st = _state(wl_socket=Path("/run/x/wayland-1"))
    r = leader._launch_app(st, {"exec": "kate"})
    assert r["ok"] is False and "missing dependency" in r["error"]


# ---------------------------------------------------------------- reaping --

def test_reap_children_drops_exited():
    st = _state()
    pid = os.fork()
    if pid == 0:
        os._exit(0)
    st.children[pid] = ("x", 0.0)   # launch time in the past: reap, no notice
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


# The vault file bridge (import/export/outbox) and the clipboard channel are no
# longer part of the leader's control socket. Their former tests are gone. The
# bridge was removed as a same-uid exfiltration surface (see
# test_control_socket_has_no_launch_or_bridge); clipboard is owned in-process by
# the compositor.


# ------------------------------------------------- toolbar app channel -----

def test_publish_apps_writes_socket_and_file(tmp_path, monkeypatch):
    monkeypatch.setattr(leader, "COMPOSITOR_RUNTIME", tmp_path)
    st = leader._LeaderState(
        mountpoint="/run/veracage/deadbeef",
        volume_label="MyVol",
        app_specs=[{"name": "Kate", "exec": "kate", "args": []},
                   {"name": "Okular", "exec": "okular", "args": []}])
    srv = leader._publish_apps(st)
    try:
        assert srv is not None
        lines = (tmp_path / "app-deadbeef.apps").read_text().splitlines()
        assert lines[0] == "app-deadbeef.sock"     # compositor connects to this
        assert lines[1] == "MyVol"                 # volume label (window title)
        assert lines[2] == ""                      # open-volume list (none scanned here)
        assert lines[3] == "0"                     # opener index (no fm -> first app)
        assert lines[4:] == ["Kate", "Okular"]     # button labels, in order
        assert (tmp_path / "app-deadbeef.sock").is_socket()
    finally:
        srv.close()
        leader._unpublish_apps(st)
    assert not (tmp_path / "app-deadbeef.sock").exists()
    assert not (tmp_path / "app-deadbeef.apps").exists()


def test_publish_apps_opener_prefers_file_manager(tmp_path, monkeypatch):
    monkeypatch.setattr(leader, "COMPOSITOR_RUNTIME", tmp_path)
    st = leader._LeaderState(
        mountpoint="/run/veracage/deadbeef",
        volume_label="MyVol",
        app_specs=[{"name": "Kate", "exec": "kate", "args": []},
                   {"name": "Files", "exec": "/usr/bin/dolphin", "args": []}])
    srv = leader._publish_apps(st)
    try:
        lines = (tmp_path / "app-deadbeef.apps").read_text().splitlines()
        assert lines[3] == "1"                     # dolphin is the opener, not kate
    finally:
        srv.close()
        leader._unpublish_apps(st)


def test_publish_apps_opener_minus_one_when_no_apps(tmp_path, monkeypatch):
    monkeypatch.setattr(leader, "COMPOSITOR_RUNTIME", tmp_path)
    st = leader._LeaderState(
        mountpoint="/run/veracage/deadbeef", volume_label="Empty", app_specs=[])
    srv = leader._publish_apps(st)
    try:
        lines = (tmp_path / "app-deadbeef.apps").read_text().splitlines()
        assert lines[3] == "-1"                    # no app -> no opener
    finally:
        srv.close()
        leader._unpublish_apps(st)


def test_accept_app_launch_execs_by_index(tmp_path, monkeypatch):
    monkeypatch.setattr(leader, "COMPOSITOR_RUNTIME", tmp_path)
    launched: dict = {}
    monkeypatch.setattr(leader, "_launch_app",
                        lambda st, spec: launched.update(spec=spec) or {"ok": True})
    st = leader._LeaderState(
        mountpoint="/run/veracage/abc123",
        app_specs=[{"name": "Kate", "exec": "kate", "args": []},
                   {"name": "Okular", "exec": "okular", "args": []}])
    srv = leader._publish_apps(st)
    assert srv is not None
    try:
        c = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        c.connect(str(leader._app_socket_path(st)))
        c.sendall(b"1\n")               # launch index 1 -> Okular
        c.close()
        leader._accept_app_launch(srv, st)
    finally:
        srv.close()
        leader._unpublish_apps(st)
    assert launched["spec"]["exec"] == "okular"


def test_write_places_file_one_entry_per_volume(tmp_path, monkeypatch):
    monkeypatch.setenv("XDG_RUNTIME_DIR", str(tmp_path))
    p = leader._write_places_file(["Work", "Photos"])
    assert p == tmp_path / "user-places.xbel"
    body = p.read_text()
    # one Places entry per open volume, each at /vaults/<label>
    assert 'href="file:///vaults/Work"' in body
    assert 'href="file:///vaults/Photos"' in body
    assert "<title>Work</title>" in body and "<title>Photos</title>" in body


def test_write_places_file_escapes_hostile_label(tmp_path, monkeypatch):
    """A label with XML-special chars must produce well-formed, escaped XBEL
    (the href/ID/title are all interpolated) - not broken or injected markup."""
    import xml.etree.ElementTree as ET
    monkeypatch.setenv("XDG_RUNTIME_DIR", str(tmp_path))
    p = leader._write_places_file(['a"<&\'>b'])
    body = p.read_text()
    # Raw specials must not appear unescaped in the title, and the doc must parse.
    assert "<title>a\"<&'>b</title>" not in body
    assert "&lt;" in body and "&amp;" in body
    ET.fromstring(body)   # raises if the XBEL is malformed


def test_scan_volumes_lists_dirs_skips_dotfiles(tmp_path):
    (tmp_path / "volA").mkdir()
    (tmp_path / "volB").mkdir()
    (tmp_path / ".exchange").mkdir()          # dot entry: skipped
    (tmp_path / "note.txt").write_text("x")   # not a dir: skipped
    assert leader.scan_volumes(tmp_path) == ["volA", "volB"]


def test_accept_app_launch_ignores_out_of_range(tmp_path, monkeypatch):
    monkeypatch.setattr(leader, "COMPOSITOR_RUNTIME", tmp_path)

    def boom(*_a):
        raise AssertionError("must not launch on a bad index")
    monkeypatch.setattr(leader, "_launch_app", boom)
    st = leader._LeaderState(
        mountpoint="/run/veracage/abc123",
        app_specs=[{"name": "Kate", "exec": "kate", "args": []}])
    srv = leader._publish_apps(st)
    try:
        c = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        c.connect(str(leader._app_socket_path(st)))
        c.sendall(b"9\n")               # out of range -> no launch, no raise
        c.close()
        leader._accept_app_launch(srv, st)
    finally:
        srv.close()
        leader._unpublish_apps(st)
