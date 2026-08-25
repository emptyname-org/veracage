"""Vault-side leader control protocol (increment 1)."""
from __future__ import annotations

import json
import os
import socket
import subprocess
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
    assert text.strip() == "VeraCrypt failed to launch."


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


def test_launch_app_that_cannot_spawn_takes_the_note_down(tmp_path, monkeypatch):
    # The note is written before the spawn (so it can never be younger than the
    # window that ends it), which means a spawn that fails has to take it down or
    # the spinner turns for nothing.
    monkeypatch.setattr(leader, "COMPOSITOR_RUNTIME", tmp_path)
    monkeypatch.setattr(leader, "bwrap_command", lambda *a, **k: ["bwrap"])

    def boom(argv, **kw):
        raise FileNotFoundError(2, "no such file", "bwrap")

    monkeypatch.setattr(leader.subprocess, "Popen", boom)
    st = _state(wl_socket=Path("/run/x/wayland-1"))
    reply = leader._launch_app(st, {"name": "Kate", "exec": "kate"})

    assert reply["ok"] is False
    assert not (tmp_path / "status").exists(), "a failed launch left the spinner on"


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


@pytest.mark.parametrize("line, closing, launched_exec", [
    (b"close\n", True, None),      # the compositor closing the Veracage window
    (b"0\n", False, "kate"),       # an ordinary toolbar launch still works
    (b"nonsense\n", False, None),  # anything else is ignored, session unharmed
])
def test_accept_app_launch_close_verb(tmp_path, monkeypatch, line, closing, launched_exec):
    """`close` on the toolbar socket ends the session: that is how the window
    close dismounts without the password the per-volume helper path needs."""
    monkeypatch.setattr(leader, "COMPOSITOR_RUNTIME", tmp_path)
    launched: dict = {}
    monkeypatch.setattr(leader, "_launch_app",
                        lambda st, spec: launched.update(spec=spec) or {"ok": True})
    st = leader._LeaderState(mountpoint="/run/veracage/abc123",
                             app_specs=[{"name": "Kate", "exec": "kate", "args": []}])
    srv = leader._publish_apps(st)
    assert srv is not None
    try:
        c = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        c.connect(str(leader._app_socket_path(st)))
        c.sendall(line)
        c.close()
        leader._accept_app_launch(srv, st)
    finally:
        srv.close()
        leader._unpublish_apps(st)
    assert st.closing is closing
    assert launched.get("spec", {}).get("exec") == launched_exec


def test_close_apps_stops_the_apps_keeps_the_session_and_says_so(tmp_path, monkeypatch):
    """`close-apps` is the dismount path, not the quit path: a running app holds
    the volume's dm device, so the apps have to go before it can be closed - but
    the session stays up. The signal it writes is what lets the human side retry
    the dismount at the right moment instead of polling pkexec."""
    monkeypatch.setattr(leader, "COMPOSITOR_RUNTIME", tmp_path)
    terminated: list = []
    monkeypatch.setattr(leader, "_terminate_children", lambda st: terminated.append(st))
    st = leader._LeaderState(mountpoint="/run/veracage/abc123",
                             app_specs=[{"name": "Kate", "exec": "kate", "args": []}])
    st.children[4242] = {"name": "Kate"}
    srv = leader._publish_apps(st)
    assert srv is not None
    try:
        c = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        c.connect(str(leader._app_socket_path(st)))
        c.sendall(b"close-apps\n")
        c.close()
        leader._accept_app_launch(srv, st)
    finally:
        srv.close()
        leader._unpublish_apps(st)
    assert terminated == [st]
    assert st.children == {}
    assert st.closing is False          # the session keeps running
    done = tmp_path / "closeapps.done"
    assert done.exists() and done.read_text().strip().isdigit()


def test_write_places_file_one_entry_per_volume(tmp_path, monkeypatch):
    monkeypatch.setenv("XDG_RUNTIME_DIR", str(tmp_path))
    p = leader._write_places_file(["Work", "Photos"])
    assert p == tmp_path / "user-places.xbel"
    body = p.read_text()
    # one Places entry per open volume, each at /vaults/<label>
    assert 'href="file:///vaults/Work"' in body
    assert 'href="file:///vaults/Photos"' in body
    assert "<title>Work</title>" in body and "<title>Photos</title>" in body


def test_write_places_file_has_no_volume_entry_when_none_is_mounted(tmp_path, monkeypatch):
    """The front-door scratchpad has no volume, so Places must not offer one: a
    placeholder entry pointed the file manager at a /vaults path that never exists."""
    monkeypatch.setenv("XDG_RUNTIME_DIR", str(tmp_path))
    body = leader._write_places_file([], with_exchange=True).read_text()
    assert "/vaults/" not in body
    assert 'href="file:///exchange"' in body   # the shared directory still shows


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


def test_kdeglobals_carries_the_font_and_the_desktop_scheme(tmp_path, monkeypatch):
    """The seeded kdeglobals must set the Veracage font in Qt's own unit and
    carry the colours from the desktop's scheme file, with the scheme's own
    [General] (its translated names) dropped so it can't shadow the fonts."""
    scheme = tmp_path / "BreezeDark.colors"
    scheme.write_text(
        "[Colors:Window]\nBackgroundNormal=42,46,50\n\n"
        "[General]\nColorScheme=BreezeDark\nName=Breeze Dark\n\n"
        "[KDE]\ncontrast=4\n"
    )
    monkeypatch.setitem(leader.COLOR_SCHEMES, "dark", scheme)
    body = leader._kdeglobals_body("dark", "Noto Sans", 12.0, single_click=False)

    assert "font=Noto Sans,12,-1,5,50,0,0,0,0,0" in body
    assert "smallestReadableFont=Noto Sans,10," in body   # 0.85 of the base
    assert "BackgroundNormal=42,46,50" in body
    assert "Theme=breeze-dark" in body
    assert "widgetStyle=Breeze" in body
    # The scheme's [General] must not come through: one [General], ours.
    assert body.count("[General]") == 1
    assert "Name=Breeze Dark" not in body


def test_kdeglobals_without_a_scheme_still_sets_the_font(tmp_path, monkeypatch):
    # A host with no KDE colour schemes installed: fonts apply, colours do not.
    monkeypatch.setitem(leader.COLOR_SCHEMES, "light", tmp_path / "absent.colors")
    body = leader._kdeglobals_body("light", "DejaVu Sans", 11.0, single_click=False)
    assert "font=DejaVu Sans,11," in body
    assert "[Colors:" not in body


def test_kdeglobals_seeds_the_hosts_click_behaviour_exactly_once(tmp_path, monkeypatch):
    """Dolphin inside the sandbox must follow the host: without a seeded
    SingleClick it falls back to KDE's own default (single click) on a
    double-click host. The [KDE] group is written once either way."""
    scheme = tmp_path / "BreezeLight.colors"
    scheme.write_text("[Colors:Window]\nBackgroundNormal=252,252,252\n\n[KDE]\ncontrast=4\n")
    monkeypatch.setitem(leader.COLOR_SCHEMES, "light", scheme)
    for single_click, expected in ((False, "SingleClick=false"), (True, "SingleClick=true")):
        body = leader._kdeglobals_body("light", "Noto Sans", 12.0, single_click)
        assert expected in body
        assert body.count("[KDE]") == 1
        assert "contrast=4" in body        # the scheme's own [KDE] key survives
    # No scheme file at all: the group is still written, so the setting applies.
    monkeypatch.setitem(leader.COLOR_SCHEMES, "light", tmp_path / "absent.colors")
    body = leader._kdeglobals_body("light", "Noto Sans", 12.0, True)
    assert body.count("[KDE]") == 1 and "SingleClick=true" in body


def test_kdeglobals_seed_needs_both_published_inputs(tmp_path, monkeypatch):
    monkeypatch.setenv("XDG_RUNTIME_DIR", str(tmp_path))
    monkeypatch.setattr(leader, "THEME_PUB", tmp_path / "theme")
    monkeypatch.setattr(leader, "APPFONT_PUB", tmp_path / "appfont")
    assert leader._write_kdeglobals_file() is None      # nothing published yet
    (tmp_path / "theme").write_text("dark\n")
    (tmp_path / "appfont").write_text("Noto Sans\n12\n")
    p = leader._write_kdeglobals_file()
    assert p == tmp_path / "kdeglobals"
    assert "font=Noto Sans,12," in p.read_text()


# ----------------------------------------------- hostile labels and names ---

def test_a_crafted_volume_label_cannot_desync_the_apps_file(tmp_path, monkeypatch):
    """The `.apps` file the compositor reads is newline- and tab-delimited, and
    the volume label comes off an attacker-supplied filesystem. A label carrying
    a newline (or a tab, or control characters) must not be able to add lines or
    fields: that would forge a socket name, a title or an app entry in the menu."""
    monkeypatch.setattr(leader, "COMPOSITOR_RUNTIME", tmp_path)
    st = _state()
    st.volumes = ["ok", "evil\nveracage-fake.sock\nPWNED"]
    st.volume_label = "title\nSPOOFED"
    st.app_specs = [{"name": "App\nInjected", "exec": "kate"}]
    leader._write_apps_file(st)

    body = leader._apps_file_path(st).read_text()
    lines = body.splitlines()
    # <sock>\n<title>\n<volumes>\n<opener>\n<name>...  : exactly one app name.
    assert len(lines) == 5, lines
    assert lines[0].endswith(".sock")
    assert "SPOOFED" in lines[1] and "\n" not in lines[1]
    assert len(lines[2].split("\t")) == 2, "a label must not add a volume field"
    assert "PWNED" in lines[2]        # neutralised, not dropped
    assert lines[4].startswith("App") and "Injected" in lines[4]


def test_the_app_socket_is_owner_only(tmp_path, monkeypatch):
    """A connect needs write on the socket inode, so the mode is what denies
    every other uid even though the runtime dir is traversable."""
    monkeypatch.setattr(leader, "COMPOSITOR_RUNTIME", tmp_path)
    st = _state()
    srv = leader._publish_apps(st)
    assert srv is not None
    try:
        mode = leader._app_socket_path(st).stat().st_mode & 0o777
        assert mode == 0o700, oct(mode)
    finally:
        srv.close()
        leader._unpublish_apps(st)


# ------------------------------------------------------- terminate children --

def _sleeper(ignore_term: bool = False):
    """A real child process that sleeps, optionally ignoring SIGTERM.

    A subprocess rather than a fork + `signal.signal`: in a forked child of the
    pytest process that call is not dependable (the child can end up not counting
    as the main thread), and a "stubborn" child that quietly dies on SIGTERM
    would make this test pass against a leader that never escalates."""
    argv = ["sh", "-c", 'trap "" TERM; sleep 30'] if ignore_term else ["sleep", "30"]
    return subprocess.Popen(argv)


def test_terminate_children_really_ends_them_and_reaps(monkeypatch):
    """close-apps and session teardown both depend on this: while an app is
    alive its sandbox keeps the volume's mount, so `cryptsetup close` fails and
    the key stays in RAM. A polite child must get SIGTERM, one that ignores it
    must still be gone afterwards, and neither may be left as a zombie."""
    polite, stubborn = _sleeper(), _sleeper(ignore_term=True)
    st = _state()
    st.children = {polite.pid: ("polite", 0.0), stubborn.pid: ("stubborn", 0.0)}
    time.sleep(0.2)   # let `sh` install its trap before we signal it

    t0 = time.monotonic()
    leader._terminate_children(st, timeout=1.0)
    elapsed = time.monotonic() - t0

    # Bounded: the grace is shared across children and a child that ignores
    # SIGTERM is escalated to SIGKILL rather than waited out. Without the
    # escalation this call blocks until the app exits on its own, and the session
    # (and its decrypted volume) waits for it.
    assert elapsed < 2.5, f"took {elapsed:.1f}s: no SIGKILL escalation"
    for proc in (polite, stubborn):
        with pytest.raises(ChildProcessError):
            os.waitpid(proc.pid, os.WNOHANG)   # already reaped: no zombie left
        with pytest.raises(ProcessLookupError):
            os.kill(proc.pid, 0)               # and really gone
        proc.returncode = 0                    # already reaped by the leader


def test_debug_lines_go_to_the_log_directory_when_there_is_one(tmp_path, monkeypatch, capsys):
    """With a log directory configured the leader writes beside the compositor's
    log. Without one it falls back to stderr, which journald files under the
    SYSTEM journal because this process runs as the veracage uid."""
    monkeypatch.setenv("VERACAGE_LOG_DIR", str(tmp_path))
    leader._debug(_state(debug=True), "launch 'Kate': pid=1 spawned in 1ms")
    written = (tmp_path / "leader.log").read_text()
    assert "launch 'Kate': pid=1 spawned in 1ms" in written
    assert capsys.readouterr().err == ""

    monkeypatch.delenv("VERACAGE_LOG_DIR")
    leader._debug(_state(debug=True), "exit 'Kate'")
    assert "exit 'Kate'" in capsys.readouterr().err

    # debug off writes nothing, either way
    monkeypatch.setenv("VERACAGE_LOG_DIR", str(tmp_path))
    leader._debug(_state(debug=False), "silent")
    assert "silent" not in (tmp_path / "leader.log").read_text()
    assert capsys.readouterr().err == ""


def test_a_symlink_in_the_log_directory_is_refused(tmp_path, monkeypatch, capsys):
    """The directory is named by the human side and the leader runs as the
    veracage uid: a symlink planted there must not redirect the append."""
    monkeypatch.setenv("VERACAGE_LOG_DIR", str(tmp_path))
    target = tmp_path / "elsewhere"
    (tmp_path / "leader.log").symlink_to(target)
    leader._debug(_state(debug=True), "launch 'Kate'")
    assert not target.exists()
    assert "launch 'Kate'" in capsys.readouterr().err   # fell back to stderr
