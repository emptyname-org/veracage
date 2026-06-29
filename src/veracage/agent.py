"""Veracage agent — Qt tray UI running on the host, as the human.

In the deny-by-UID model the agent has **no vault access**: it can't read the
vault, the outbox, or the sandbox clipboard. Everything goes through the leader
over the control socket (the same one `veracage exec` uses), so the agent holds
no privilege and sees no plaintext:

  - Open app in vault   -> leader exec (resolved app spec)
  - Push/pull clipboard -> leader clip-push / clip-pull (text)
  - Drop zone (import)  -> leader import_file (host fd passed to the vault uid)
  - Export from sandbox -> leader list_outbox + export_file (vault fd passed back)
  - Close vault         -> leader close

Spawned by the launcher (`veracage open`). Soft-imports PySide6: if it's missing
the agent exits 0 and the session continues headless (use `veracage exec/list/
close` from the CLI).
"""
from __future__ import annotations

import os
import sys
import time
from pathlib import Path

from . import config, leader


def run(vault: str) -> int:
    try:
        from PySide6.QtGui import QIcon
        from PySide6.QtWidgets import QApplication, QMenu, QSystemTrayIcon
    except ImportError:
        sys.stderr.write(
            "veracage-agent: PySide6 not installed; UI disabled.\n"
            "(install: sudo apt install python3-pyside6.qtwidgets)\n"
        )
        return 0

    cfg = config.load()

    qt = QApplication(sys.argv)
    qt.setQuitOnLastWindowClosed(False)

    _start_suspend_watcher(vault, cfg.suspend_action)

    tray = QSystemTrayIcon(QIcon.fromTheme("security-high"))
    tray.setToolTip(f"Veracage — {Path(vault).name}")
    menu = QMenu()

    # ---- Launch app submenu (apps resolved here; leader runs them verbatim) ----
    launch_menu = menu.addMenu("Open app in vault…")
    for app_def in cfg.apps.values():
        action = launch_menu.addAction(app_def.name)
        action.triggered.connect(_launch_app(vault, app_def, tray))
    if not cfg.apps:
        empty = launch_menu.addAction("(no apps configured)")
        empty.setEnabled(False)

    menu.addSeparator()

    # ---- Clipboard transfer (text) ----
    push = menu.addAction("Push host clipboard → sandbox")
    pull = menu.addAction("Pull sandbox clipboard → host")
    push.triggered.connect(_clip_push(vault, qt, tray))
    pull.triggered.connect(_clip_pull(vault, qt, tray))

    menu.addSeparator()

    # ---- Drop zone (import) ----
    drop = DropZone(vault, tray)
    dz_action = menu.addAction("Show drop zone…")
    dz_action.triggered.connect(drop.toggle)

    menu.addSeparator()

    # ---- Outbox (export) ----
    outbox = OutboxHandler(vault, tray)
    out_action = menu.addAction("Export from sandbox…")
    out_action.triggered.connect(outbox.export_dialog)

    menu.addSeparator()

    # ---- Close session ----
    close_action = menu.addAction("Close vault")
    close_action.triggered.connect(_close_session(vault, qt))

    tray.setContextMenu(menu)

    def _on_activated(reason):
        if reason == QSystemTrayIcon.ActivationReason.Trigger:
            outbox.export_dialog()
    tray.activated.connect(_on_activated)

    tray.show()
    return qt.exec()


# --------------------------------------------------------------- helpers ----

def _notify(tray, msg: str, level: str = "info", ms: int = 2500) -> None:
    from PySide6.QtWidgets import QSystemTrayIcon
    icon = {
        "info": QSystemTrayIcon.MessageIcon.Information,
        "warn": QSystemTrayIcon.MessageIcon.Warning,
        "error": QSystemTrayIcon.MessageIcon.Critical,
    }.get(level, QSystemTrayIcon.MessageIcon.Information)
    tray.showMessage("Veracage", msg, icon, ms)


def _launch_app(vault: str, app_def, tray):
    spec = {"name": app_def.name, "exec": app_def.exec, "args": app_def.args}

    def _do():
        try:
            reply = leader.send_request(vault, {"cmd": "exec", "app": spec})
            if not reply.get("ok"):
                _notify(tray, f"Failed to start {app_def.name}: {reply.get('error')}", "warn", 4000)
        except Exception as e:
            _notify(tray, str(e), "error", 4000)
    return _do


def _clip_push(vault: str, qt, tray):
    def _do():
        try:
            text = qt.clipboard().text()
            reply = leader.clip_push(vault, text)
            _notify(tray, "Pushed to sandbox" if reply.get("ok")
                    else f"Clipboard push failed: {reply.get('error')}",
                    "info" if reply.get("ok") else "warn", 1800)
        except Exception as e:
            _notify(tray, f"Clipboard push failed: {e}", "warn", 4000)
    return _do


def _clip_pull(vault: str, qt, tray):
    def _do():
        try:
            reply = leader.clip_pull(vault)
            if reply.get("ok"):
                qt.clipboard().setText(reply.get("text", ""))
                _notify(tray, "Pulled to host", "info", 1800)
            else:
                _notify(tray, f"Clipboard pull failed: {reply.get('error')}", "warn", 4000)
        except Exception as e:
            _notify(tray, f"Clipboard pull failed: {e}", "warn", 4000)
    return _do


def _close_session(vault: str, qt_app):
    def _do():
        try:
            leader.send_request(vault, {"cmd": "close"})
        except FileNotFoundError:
            pass
        qt_app.quit()
    return _do


# ------------------------------------------------------- suspend watcher ----

def _start_suspend_watcher(vault: str, suspend_action: str = "dismount") -> None:
    """Subscribe to login1 PrepareForSleep; close the session on suspend.

    Takes a logind *delay* inhibitor so the system waits for teardown to start
    before sleeping — otherwise it can suspend with the dm-crypt key still in
    RAM. `suspend_action = "ignore"` skips this. Soft-imports `gi`.
    """
    if suspend_action == "ignore":
        sys.stderr.write(
            "veracage-agent: suspend_action=ignore; vault stays mounted across suspend.\n"
        )
        return
    try:
        import gi
        gi.require_version("Gio", "2.0")
        gi.require_version("GLib", "2.0")
        from gi.repository import Gio, GLib
    except (ImportError, ValueError):
        sys.stderr.write("veracage-agent: python3-gi not available; suspend handling off.\n")
        return

    def _take_delay_lock(bus):
        try:
            ret, fds = bus.call_with_unix_fd_list_sync(
                "org.freedesktop.login1", "/org/freedesktop/login1",
                "org.freedesktop.login1.Manager", "Inhibit",
                GLib.Variant("(ssss)", ("sleep", "veracage",
                                        "Dismount vault before sleep", "delay")),
                GLib.VariantType.new("(h)"),
                Gio.DBusCallFlags.NONE, -1, None, None,
            )
            return fds.get(ret.get_child_value(0).get_handle())
        except Exception as e:  # pragma: no cover - needs a live system bus
            sys.stderr.write(f"veracage-agent: no sleep inhibitor ({e}); "
                             "dismount may race suspend.\n")
            return None

    lock = {"fd": None}

    def _on_signal(_conn, _sender, _path, _iface, _signal, params):
        try:
            suspending = bool(params[0]) if params else False
        except Exception:
            return
        if not suspending:
            return
        try:
            leader.send_request(vault, {"cmd": "close"})
        except Exception:
            pass
        _wait_session_gone(vault, timeout=4.0)
        fd, lock["fd"] = lock["fd"], None
        if fd is not None:
            try:
                os.close(fd)
            except OSError:
                pass

    def _run() -> None:
        loop = GLib.MainLoop()
        try:
            bus = Gio.bus_get_sync(Gio.BusType.SYSTEM, None)
            lock["fd"] = _take_delay_lock(bus)
            bus.signal_subscribe(
                "org.freedesktop.login1", "org.freedesktop.login1.Manager",
                "PrepareForSleep", "/org/freedesktop/login1", None,
                Gio.DBusSignalFlags.NONE, _on_signal,
            )
            loop.run()
        except Exception as e:
            sys.stderr.write(f"veracage-agent: suspend watcher: {e}\n")

    import threading
    threading.Thread(target=_run, daemon=True, name="veracage-suspend").start()


def _wait_session_gone(vault: str, timeout: float) -> None:
    """Poll until the control socket disappears, up to `timeout`s — a proxy for
    'teardown has started' before we release the suspend lock."""
    try:
        sock = leader.session_socket_path(vault)
    except Exception:
        return
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if not sock.exists():
            return
        time.sleep(0.1)


# ------------------------------------------------------------ drop zone -----

class DropZone:
    def __init__(self, vault: str, tray):
        from PySide6.QtCore import Qt
        from PySide6.QtWidgets import QLabel, QVBoxLayout
        self.vault = vault
        self.tray = tray
        self.widget = _DropWidget(self._handle_drop)
        self.widget.setWindowTitle("Veracage drop zone")
        self.widget.resize(280, 180)
        layout = QVBoxLayout(self.widget)
        label = QLabel("Drop files here\nto import into the vault")
        label.setAlignment(Qt.AlignmentFlag.AlignCenter)
        label.setStyleSheet("font-size: 16px; color: #888;")
        layout.addWidget(label)

    def toggle(self):
        if self.widget.isVisible():
            self.widget.hide()
        else:
            self.widget.show()
            self.widget.raise_()
            self.widget.activateWindow()

    def _handle_drop(self, urls):
        for url in urls:
            if not url.isLocalFile():
                continue
            try:
                reply = leader.import_file(self.vault, url.toLocalFile())
                if reply.get("ok"):
                    _notify(self.tray, f"Imported: {Path(reply['path']).name}", "info", 2500)
                else:
                    _notify(self.tray, f"Import failed: {reply.get('error')}", "warn", 4000)
            except Exception as e:
                _notify(self.tray, f"Import failed: {e}", "warn", 4000)


def _DropWidget(on_drop):
    """Return a QWidget subclass that delegates drop events to `on_drop`."""
    from PySide6.QtWidgets import QWidget

    class _W(QWidget):
        def __init__(self):
            super().__init__()
            self.setAcceptDrops(True)

        def dragEnterEvent(self, event):
            if event.mimeData().hasUrls():
                event.acceptProposedAction()

        def dropEvent(self, event):
            on_drop(event.mimeData().urls())
            event.acceptProposedAction()

    return _W()


# ------------------------------------------------------------- outbox -------

class OutboxHandler:
    def __init__(self, vault: str, tray):
        self.vault = vault
        self.tray = tray

    def export_dialog(self):
        from PySide6.QtWidgets import QFileDialog
        try:
            reply = leader.list_outbox(self.vault)
        except Exception as e:
            _notify(self.tray, f"Outbox unavailable: {e}", "warn", 4000)
            return
        files = reply.get("files", []) if reply.get("ok") else []
        if not files:
            _notify(self.tray, "Outbox is empty.", "info", 2000)
            return
        for name in files:
            target, _ = QFileDialog.getSaveFileName(
                None, f"Export {name}", str(Path.home() / name))
            if not target:
                continue
            try:
                r = leader.export_file(self.vault, name, target)
                if r.get("ok"):
                    _notify(self.tray, f"Exported to {target}", "info", 2500)
                else:
                    _notify(self.tray, f"Export failed: {r.get('error')}", "warn", 4000)
            except Exception as e:
                _notify(self.tray, f"Export failed: {e}", "warn", 4000)
