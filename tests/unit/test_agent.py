"""Agent — soft-fail when PySide6 missing, callback wiring."""
from __future__ import annotations

import sys
from unittest import mock

import pytest

from veracage import agent


def test_run_returns_0_when_pyside6_missing(monkeypatch, capsys):
    """Without PySide6, agent.run() exits 0 without spinning a Qt loop."""
    # Force the `from PySide6.QtCore import Qt` line in agent.run() to fail.
    monkeypatch.setitem(sys.modules, "PySide6", None)
    monkeypatch.setitem(sys.modules, "PySide6.QtCore", None)
    rc = agent.run("/tmp/x.vc", "/tmp/x", "/run/user/1000/x.sock")
    assert rc == 0
    assert "PySide6 not installed" in capsys.readouterr().err


@pytest.fixture
def fake_pyside(monkeypatch):
    """Stub PySide6 so the inner `from PySide6.QtWidgets import …` works."""
    monkeypatch.setitem(sys.modules, "PySide6", mock.MagicMock())
    monkeypatch.setitem(sys.modules, "PySide6.QtWidgets", mock.MagicMock())


def test_launch_app_sends_exec_request(fake_pyside):
    tray = mock.MagicMock()
    callback = agent._launch_app("/tmp/x.vc", "kate", tray)
    with mock.patch.object(
        agent.session, "send_request",
        return_value={"ok": True, "pid": 999},
    ) as send:
        callback()
    send.assert_called_once_with("/tmp/x.vc", {"cmd": "exec", "app": "kate"})
    tray.showMessage.assert_not_called()


def test_launch_app_warns_on_failure(fake_pyside):
    tray = mock.MagicMock()
    callback = agent._launch_app("/tmp/x.vc", "kate", tray)
    with mock.patch.object(
        agent.session, "send_request",
        return_value={"ok": False, "error": "bad"},
    ):
        callback()
    tray.showMessage.assert_called_once()
    msg = tray.showMessage.call_args.args[1]
    assert "Failed" in msg and "kate" in msg


def test_launch_app_warns_on_exception(fake_pyside):
    tray = mock.MagicMock()
    callback = agent._launch_app("/tmp/x.vc", "kate", tray)
    with mock.patch.object(
        agent.session, "send_request",
        side_effect=ConnectionRefusedError("nope"),
    ):
        callback()
    tray.showMessage.assert_called_once()


def test_close_sends_close_and_quits(fake_pyside):
    qt_app = mock.MagicMock()
    callback = agent._close_session("/tmp/x.vc", qt_app)
    with mock.patch.object(agent.session, "send_request",
                           return_value={"ok": True}) as send:
        callback()
    send.assert_called_once_with("/tmp/x.vc", {"cmd": "close"})
    qt_app.quit.assert_called_once()


def test_close_quits_even_if_no_session(fake_pyside):
    """If the session is already gone, the agent should still tear down."""
    qt_app = mock.MagicMock()
    callback = agent._close_session("/tmp/x.vc", qt_app)
    with mock.patch.object(
        agent.session, "send_request",
        side_effect=FileNotFoundError,
    ):
        callback()
    qt_app.quit.assert_called_once()


def test_clipboard_op_calls_fn_then_notifies(fake_pyside):
    tray = mock.MagicMock()
    fn = mock.Mock(return_value=0)
    callback = agent._clipboard_op(fn, "/run/user/1000/x.sock", tray, "Done")
    callback()
    fn.assert_called_once_with("/run/user/1000/x.sock")
    tray.showMessage.assert_called_once()


def test_clipboard_op_warns_on_exception(fake_pyside):
    tray = mock.MagicMock()
    fn = mock.Mock(side_effect=RuntimeError("boom"))
    callback = agent._clipboard_op(fn, "/x", tray, "Done")
    callback()
    tray.showMessage.assert_called_once()
    assert "failed" in tray.showMessage.call_args.args[1].lower()


def test_suspend_watcher_skipped_when_ignore(capsys):
    """suspend_action=ignore installs no watcher (no gi import) and says so."""
    agent._start_suspend_watcher("/tmp/x.vc", "ignore")
    assert "ignore" in capsys.readouterr().err
