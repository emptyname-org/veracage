"""Agent — soft-fail when PySide6 missing, callback wiring (B2: via the leader)."""
from __future__ import annotations

import sys
from unittest import mock

import pytest

from veracage import agent
from veracage.apps import App

APP = App(key="kate", name="Kate", category="text", exec="kate", args=["/vault"])


def test_run_returns_0_when_pyside6_missing(monkeypatch, capsys):
    """Without PySide6, agent.run() exits 0 without spinning a Qt loop."""
    monkeypatch.setitem(sys.modules, "PySide6", None)
    monkeypatch.setitem(sys.modules, "PySide6.QtGui", None)
    rc = agent.run("/tmp/x.vc")
    assert rc == 0
    assert "PySide6 not installed" in capsys.readouterr().err


@pytest.fixture
def fake_pyside(monkeypatch):
    """Stub PySide6 so the inner `from PySide6.QtWidgets import …` works."""
    monkeypatch.setitem(sys.modules, "PySide6", mock.MagicMock())
    monkeypatch.setitem(sys.modules, "PySide6.QtWidgets", mock.MagicMock())


# ---- launch app: resolved spec sent to the leader (no key/allowlist) --------

def test_launch_app_sends_resolved_spec(fake_pyside):
    tray = mock.MagicMock()
    cb = agent._launch_app("/tmp/x.vc", APP, tray)
    with mock.patch.object(agent.leader, "send_request",
                           return_value={"ok": True, "pid": 9}) as send:
        cb()
    send.assert_called_once_with(
        "/tmp/x.vc",
        {"cmd": "exec", "app": {"name": "Kate", "exec": "kate", "args": ["/vault"]}})
    tray.showMessage.assert_not_called()


def test_launch_app_warns_on_failure(fake_pyside):
    tray = mock.MagicMock()
    cb = agent._launch_app("/tmp/x.vc", APP, tray)
    with mock.patch.object(agent.leader, "send_request",
                           return_value={"ok": False, "error": "bad"}):
        cb()
    tray.showMessage.assert_called_once()
    assert "Kate" in tray.showMessage.call_args.args[1]


def test_launch_app_warns_on_exception(fake_pyside):
    tray = mock.MagicMock()
    cb = agent._launch_app("/tmp/x.vc", APP, tray)
    with mock.patch.object(agent.leader, "send_request",
                           side_effect=ConnectionRefusedError("nope")):
        cb()
    tray.showMessage.assert_called_once()


# ---- clipboard: host <-> leader --------------------------------------------

def test_clip_push_reads_host_and_pushes(fake_pyside):
    tray, qt = mock.MagicMock(), mock.MagicMock()
    qt.clipboard.return_value.text.return_value = "HELLO"
    cb = agent._clip_push("/tmp/x.vc", qt, tray)
    with mock.patch.object(agent.leader, "clip_push", return_value={"ok": True}) as push:
        cb()
    push.assert_called_once_with("/tmp/x.vc", "HELLO")
    tray.showMessage.assert_called_once()


def test_clip_pull_sets_host_clipboard(fake_pyside):
    tray, qt = mock.MagicMock(), mock.MagicMock()
    cb = agent._clip_pull("/tmp/x.vc", qt, tray)
    with mock.patch.object(agent.leader, "clip_pull",
                           return_value={"ok": True, "text": "WORLD"}):
        cb()
    qt.clipboard.return_value.setText.assert_called_once_with("WORLD")
    tray.showMessage.assert_called_once()


# ---- close -----------------------------------------------------------------

def test_close_sends_close_and_quits(fake_pyside):
    qt_app = mock.MagicMock()
    cb = agent._close_session("/tmp/x.vc", qt_app)
    with mock.patch.object(agent.leader, "send_request", return_value={"ok": True}) as send:
        cb()
    send.assert_called_once_with("/tmp/x.vc", {"cmd": "close"})
    qt_app.quit.assert_called_once()


def test_close_quits_even_if_no_session(fake_pyside):
    qt_app = mock.MagicMock()
    cb = agent._close_session("/tmp/x.vc", qt_app)
    with mock.patch.object(agent.leader, "send_request", side_effect=FileNotFoundError):
        cb()
    qt_app.quit.assert_called_once()


# ---- suspend watcher -------------------------------------------------------

def test_suspend_watcher_skipped_when_ignore(capsys):
    agent._start_suspend_watcher("/tmp/x.vc", "ignore")
    assert "ignore" in capsys.readouterr().err


def test_suspend_watcher_soft_fails_without_gi(monkeypatch, capsys):
    import builtins
    real_import = builtins.__import__

    def fake_import(name, *a, **k):
        if name == "gi":
            raise ImportError("no gi")
        return real_import(name, *a, **k)

    monkeypatch.setattr(builtins, "__import__", fake_import)
    agent._start_suspend_watcher("/tmp/x.vc", "dismount")   # must not raise
    assert "python3-gi not available" in capsys.readouterr().err


def test_wait_session_gone_returns_when_absent(tmp_xdg_runtime):
    agent._wait_session_gone("/tmp/nope.vc", timeout=2.0)   # must not hang/raise
