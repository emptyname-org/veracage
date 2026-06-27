"""Veracage agent — Qt UI running on the host alongside the session.

Provides:

  - Tray icon with menu (open app, push/pull clipboard, drop zone, close)
  - Drop-zone window for host→sandbox file transfer
  - Outbox watcher (sandbox→host file transfer)

Spawned by the session leader. Communicates with it through the same
control socket that `veracage exec` uses, so the agent has no special
privilege.

Soft-imports PySide6: if it's missing the agent exits 0 and the session
continues without UI (the user can still use `veracage exec/list/close`
from the CLI).
"""
from __future__ import annotations

import os
import sys
import time
from pathlib import Path

from . import clipboard, config, session, transfer


def run(vault: str, mountpoint: str, weston_socket: str) -> int:
    try:
        from PySide6.QtGui import QIcon
        from PySide6.QtWidgets import QApplication, QMenu, QSystemTrayIcon
    except ImportError:
        sys.stderr.write(
            "veracage-agent: PySide6 not installed; UI disabled.\n"
            "(install: sudo apt install python3-pyside6.qtwidgets)\n"
        )
        return 0

    sock = Path(weston_socket)
    transfer.ensure_staging(mountpoint)
    cfg = config.load()

    qt = QApplication(sys.argv)
    qt.setQuitOnLastWindowClosed(False)

    _start_suspend_watcher(vault, cfg.suspend_action)

    tray = QSystemTrayIcon(QIcon.fromTheme("security-high"))
    tray.setToolTip(f"Veracage — {Path(vault).name}")
    menu = QMenu()

    # ---- Launch app submenu ----
    launch_menu = menu.addMenu("Open app in vault…")
    for key, app_def in cfg.apps.items():
        action = launch_menu.addAction(app_def.name)
        action.triggered.connect(_launch_app(vault, key, tray))
    if not cfg.apps:
        empty = launch_menu.addAction("(no apps configured)")
        empty.setEnabled(False)

    menu.addSeparator()

    # ---- Clipboard transfer ----
    push = menu.addAction("Push host clipboard → sandbox")
    pull = menu.addAction("Pull sandbox clipboard → host")
    push.triggered.connect(_clipboard_op(clipboard.push_host_to_sandbox, sock,
                                         tray, "Pushed to sandbox"))
    pull.triggered.connect(_clipboard_op(clipboard.pull_sandbox_to_host, sock,
                                         tray, "Pulled to host"))

    menu.addSeparator()

    # ---- Drop zone ----
    drop = DropZone(mountpoint, tray)
    dz_action = menu.addAction("Show drop zone…")
    dz_action.triggered.connect(drop.toggle)

    menu.addSeparator()

    # ---- Outbox ----
    outbox = OutboxHandler(mountpoint, tray)
    out_action = menu.addAction("Export from sandbox…")
    out_action.triggered.connect(outbox.export_dialog)

    menu.addSeparator()

    # ---- Close session ----
    close_action = menu.addAction("Close vault")
    close_action.triggered.connect(_close_session(vault, qt))

    tray.setContextMenu(menu)

    # Left-click also opens the export dialog.
    def _on_activated(reason):
        if reason == QSystemTrayIcon.ActivationReason.Trigger:
            outbox.export_dialog()
    tray.activated.connect(_on_activated)

    tray.show()
    return qt.exec()


# --------------------------------------------------------------- helpers ----

def _launch_app(vault: str, app_key: str, tray):
    def _do():
        from PySide6.QtWidgets import QSystemTrayIcon
        try:
            reply = session.send_request(vault, {"cmd": "exec", "app": app_key})
            if not reply.get("ok"):
                tray.showMessage(
                    "Veracage", f"Failed to start {app_key}: {reply.get('error')}",
                    QSystemTrayIcon.MessageIcon.Warning, 4000,
                )
        except Exception as e:
            tray.showMessage("Veracage", str(e),
                             QSystemTrayIcon.MessageIcon.Critical, 4000)
    return _do


def _clipboard_op(fn, sock: Path, tray, ok_msg: str):
    def _do():
        from PySide6.QtWidgets import QSystemTrayIcon
        try:
            fn(sock)
            tray.showMessage("Veracage", ok_msg,
                             QSystemTrayIcon.MessageIcon.Information, 1800)
        except Exception as e:
            tray.showMessage("Veracage", f"Clipboard transfer failed: {e}",
                             QSystemTrayIcon.MessageIcon.Warning, 4000)
    return _do


def _start_suspend_watcher(vault: str, suspend_action: str = "dismount") -> None:
    """Subscribe to login1 PrepareForSleep; close the session on suspend.

    Takes a logind *delay* inhibitor lock so the system waits for us to start
    tearing the session down before it actually sleeps — otherwise it can
    suspend with the dm-crypt key still in RAM. With `suspend_action =
    "ignore"` no watcher is installed. Soft-imports `gi`; without it, suspend
    handling is disabled with a warning.
    """
    if suspend_action == "ignore":
        sys.stderr.write(
            "veracage-agent: suspend_action=ignore; vault stays mounted "
            "across suspend.\n"
        )
        return
    try:
        import gi
        gi.require_version("Gio", "2.0")
        gi.require_version("GLib", "2.0")
        from gi.repository import Gio, GLib
    except (ImportError, ValueError):
        sys.stderr.write(
            "veracage-agent: python3-gi not available; suspend handling off.\n"
        )
        return

    def _take_delay_lock(bus):
        """logind 'delay' sleep inhibitor → held fd, or None on failure."""
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
        # PrepareForSleep(b active) — True just before suspending.
        try:
            suspending = bool(params[0]) if params else False
        except Exception:
            return
        if not suspending:
            return
        # Hold the delay lock across the close request, give teardown a brief
        # bounded window (logind InhibitDelayMaxSec ~5s), then release so the
        # system may sleep.
        try:
            session.send_request(vault, {"cmd": "close"})
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
                "org.freedesktop.login1",
                "org.freedesktop.login1.Manager",
                "PrepareForSleep",
                "/org/freedesktop/login1",
                None,
                Gio.DBusSignalFlags.NONE,
                _on_signal,
            )
            loop.run()
        except Exception as e:
            sys.stderr.write(f"veracage-agent: suspend watcher: {e}\n")

    import threading
    t = threading.Thread(target=_run, daemon=True, name="veracage-suspend")
    t.start()


def _wait_session_gone(vault: str, timeout: float) -> None:
    """Poll until the session control socket disappears, up to `timeout`s —
    a proxy for 'teardown has started' before we release the suspend lock."""
    try:
        sock = session.session_socket_path(vault)
    except Exception:
        return
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if not sock.exists():
            return
        time.sleep(0.1)


def _close_session(vault: str, qt_app):
    def _do():
        try:
            session.send_request(vault, {"cmd": "close"})
        except FileNotFoundError:
            pass
        qt_app.quit()
    return _do


# ------------------------------------------------------------ drop zone -----

class DropZone:
    def __init__(self, mountpoint: str, tray):
        from PySide6.QtCore import Qt
        from PySide6.QtWidgets import QLabel, QVBoxLayout
        self.mountpoint = mountpoint
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
        from PySide6.QtWidgets import QSystemTrayIcon
        for url in urls:
            if not url.isLocalFile():
                continue
            try:
                dst = transfer.import_file(url.toLocalFile(), self.mountpoint)
                self.tray.showMessage(
                    "Veracage", f"Imported: {dst.name}",
                    QSystemTrayIcon.MessageIcon.Information, 2500,
                )
            except Exception as e:
                self.tray.showMessage(
                    "Veracage", f"Import failed: {e}",
                    QSystemTrayIcon.MessageIcon.Warning, 4000,
                )


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
    def __init__(self, mountpoint: str, tray):
        from PySide6.QtCore import QFileSystemWatcher
        self.mountpoint = mountpoint
        self.tray = tray
        _, out_dir = transfer.staging_dirs(mountpoint)
        self.out_dir = out_dir
        self.watcher = QFileSystemWatcher([str(out_dir)])
        self.watcher.directoryChanged.connect(self._on_changed)

    def _on_changed(self, _path: str):
        from PySide6.QtWidgets import QSystemTrayIcon
        new = transfer.list_outbox(self.mountpoint)
        if new:
            self.tray.showMessage(
                "Veracage outbox",
                f"{len(new)} file(s) in outbox — click tray to export.",
                QSystemTrayIcon.MessageIcon.Information, 4000,
            )

    def export_dialog(self):
        from PySide6.QtWidgets import QFileDialog, QSystemTrayIcon
        files = transfer.list_outbox(self.mountpoint)
        if not files:
            self.tray.showMessage(
                "Veracage", "Outbox is empty.",
                QSystemTrayIcon.MessageIcon.Information, 2000,
            )
            return
        for f in files:
            target, _ = QFileDialog.getSaveFileName(
                None, f"Export {f.name}",
                str(Path.home() / f.name),
            )
            if target:
                try:
                    transfer.export_file(str(f), target)
                    self.tray.showMessage(
                        "Veracage", f"Exported to {target}",
                        QSystemTrayIcon.MessageIcon.Information, 2500,
                    )
                except Exception as e:
                    self.tray.showMessage(
                        "Veracage", f"Export failed: {e}",
                        QSystemTrayIcon.MessageIcon.Warning, 4000,
                    )
