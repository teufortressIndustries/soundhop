"""Optional Qt system tray.

PySide6 is intentionally an extra: the daemon and CLI work without it, and
distributions can package Qt separately.  The tray is a thin client of the
same local control socket, so it cannot get out of sync with the daemon.
"""

from __future__ import annotations

import logging
import sys
from concurrent.futures import Future, ThreadPoolExecutor
from typing import Any, Callable

from . import __version__
from .config import Config
from .constants import APP_NAME
from .ipc import DaemonNotRunning, IpcError, request
from .service import tray_dependency_hint
from .util import config_path, format_duration

log = logging.getLogger(__name__)


def _import_qt() -> tuple[Any, Any, Any]:
    try:
        from PySide6 import QtCore, QtGui, QtWidgets
    except ModuleNotFoundError as exc:  # pragma: no cover - depends on extra
        raise ImportError(
            "PySide6 is not installed. " + tray_dependency_hint()
        ) from exc
    return QtCore, QtGui, QtWidgets


def run_tray(*, verbose: bool = False) -> int:
    QtCore, QtGui, QtWidgets = _import_qt()
    if verbose:
        logging.getLogger().setLevel(logging.DEBUG)

    app = QtWidgets.QApplication.instance()
    if app is None:
        app = QtWidgets.QApplication(sys.argv)
    app.setApplicationName(APP_NAME)
    app.setApplicationDisplayName(APP_NAME)
    app.setQuitOnLastWindowClosed(False)
    try:
        app.setDesktopFileName("io.github.soundhop")
    except AttributeError:
        pass

    if not QtWidgets.QSystemTrayIcon.isSystemTrayAvailable():
        print(
            "SoundHop: no system tray is available in this desktop session.\n"
            "Use the CLI instead: soundhop status / soundhop connect",
            file=sys.stderr,
        )
        return 1

    Tray, _SettingsDialog = _build_classes(QtCore, QtGui, QtWidgets)
    tray = Tray(verbose=verbose)
    tray.show()
    try:
        return int(app.exec())
    finally:
        tray.shutdown()


def _build_classes(QtCore: Any, QtGui: Any, QtWidgets: Any) -> tuple[type, type]:
    class SettingsDialog:
        def __init__(self, config: Config) -> None:
            self._patch: dict[str, Any] = {}
            self.dialog = QtWidgets.QDialog()
            self.dialog.setWindowTitle(f"{APP_NAME} settings")
            self.dialog.setMinimumWidth(440)
            layout = QtWidgets.QFormLayout(self.dialog)

            self.name = QtWidgets.QLineEdit(config.device.name)
            self.port = QtWidgets.QSpinBox()
            self.port.setRange(1, 65535)
            self.port.setValue(config.sharing.port)
            self.security = QtWidgets.QComboBox()
            for label, value in (
                ("Trusted LAN (recommended)", "trusted-lan"),
                ("Cookie (shared PulseAudio cookie)", "cookie"),
                ("Open (not recommended)", "open"),
            ):
                self.security.addItem(label, value)
            index = self.security.findData(config.sharing.security)
            self.security.setCurrentIndex(max(0, index))
            self.share_enabled = QtWidgets.QCheckBox(
                "Share this device's speakers"
            )
            self.share_enabled.setChecked(config.sharing.enabled)
            self.auto = QtWidgets.QCheckBox("Auto-connect to the last device")
            self.auto.setChecked(config.connection.auto_connect)
            self.switch = QtWidgets.QCheckBox(
                "Switch the default output automatically"
            )
            self.switch.setChecked(config.connection.switch_default)
            self.restore = QtWidgets.QCheckBox(
                "Restore local audio on disconnect"
            )
            self.restore.setChecked(config.connection.restore_default)
            self.move = QtWidgets.QCheckBox("Move currently playing audio")
            self.move.setChecked(config.connection.move_streams)
            self.disconnect_quit = QtWidgets.QCheckBox(
                "Disconnect remote audio when quitting"
            )
            self.disconnect_quit.setChecked(
                getattr(config.connection, "disconnect_on_quit", True)
            )

            self.latency = QtWidgets.QComboBox()
            self.latency.addItem("Auto (default PipeWire buffer)", 0)
            self.latency.addItem("Low Latency (50 ms - fast network)", 50)
            self.latency.addItem("Balanced (100 ms - recommended)", 100)
            self.latency.addItem("Stable Buffer (200 ms - Wi-Fi / EasyEffects)", 200)
            cur_lat = config.connection.latency_ms or 0
            lat_idx = self.latency.findData(cur_lat)
            self.latency.setCurrentIndex(max(0, lat_idx))

            layout.addRow("Device name", self.name)
            layout.addRow("Audio port", self.port)
            layout.addRow("Security", self.security)
            layout.addRow("Buffer latency", self.latency)
            layout.addRow(self.share_enabled)
            layout.addRow(self.auto)
            layout.addRow(self.switch)
            layout.addRow(self.restore)
            layout.addRow(self.move)
            layout.addRow(self.disconnect_quit)
            note = QtWidgets.QLabel(
                "Changing the port or security mode briefly restarts the listener."
            )
            note.setWordWrap(True)
            note.setStyleSheet("color: palette(mid);")
            layout.addRow(note)

            buttons = QtWidgets.QDialogButtonBox(
                QtWidgets.QDialogButtonBox.StandardButton.Save
                | QtWidgets.QDialogButtonBox.StandardButton.Cancel
            )
            buttons.accepted.connect(self._save)
            buttons.rejected.connect(self.dialog.reject)
            layout.addRow(buttons)

        def exec(self) -> int:
            return int(self.dialog.exec())

        def _save(self) -> None:
            lat_val = int(self.latency.currentData() or 0)
            self._patch = {
                "device": {"name": self.name.text().strip()},
                "sharing": {
                    "enabled": self.share_enabled.isChecked(),
                    "port": int(self.port.value()),
                    "security": self.security.currentData(),
                },
                "connection": {
                    "auto_connect": self.auto.isChecked(),
                    "switch_default": self.switch.isChecked(),
                    "restore_default": self.restore.isChecked(),
                    "move_streams": self.move.isChecked(),
                    "disconnect_on_quit": self.disconnect_quit.isChecked(),
                    "latency_ms": lat_val if lat_val > 0 else None,
                },
            }
            self.dialog.accept()

        def patch(self) -> dict[str, Any]:
            return self._patch

    class Tray(QtCore.QObject):
        def __init__(self, *, verbose: bool = False) -> None:
            super().__init__()
            self.verbose = verbose
            self._executor = ThreadPoolExecutor(
                max_workers=2, thread_name_prefix="soundhop-tray"
            )
            self._status: dict[str, Any] | None = None
            self._refreshing = False
            self._started = False
            self._busy = False
            self._closing = False

            self.icon = self._build_icon()
            self.tray = QtWidgets.QSystemTrayIcon(self.icon, self)
            self.tray.setToolTip(f"{APP_NAME} — starting…")
            self.menu = QtWidgets.QMenu()
            self._build_menu()
            self.tray.setContextMenu(self.menu)
            self.tray.activated.connect(self._on_activated)

            self._timer = QtCore.QTimer(self)
            self._timer.setInterval(2500)
            self._timer.timeout.connect(self.refresh)
            self._timer.start()
            self._kickstart()

        # -- construction ------------------------------------------------

        def _build_icon(self) -> Any:
            from importlib.resources import files

            try:
                icon_path = files("soundhop").joinpath("assets/soundhop.svg")
                icon = QtGui.QIcon(str(icon_path))
                if not icon.isNull():
                    return icon
            except Exception:
                log.debug("Could not load packaged icon", exc_info=True)
            pixmap = QtGui.QPixmap(64, 64)
            pixmap.fill(QtCore.Qt.GlobalColor.transparent)
            painter = QtGui.QPainter(pixmap)
            painter.setRenderHint(QtGui.QPainter.RenderHint.Antialiasing)
            painter.setPen(QtCore.Qt.PenStyle.NoPen)
            painter.setBrush(QtGui.QColor("#3daee9"))
            painter.drawRoundedRect(4, 4, 56, 56, 14, 14)
            painter.setPen(QtGui.QPen(QtGui.QColor("#ffffff"), 4))
            painter.drawArc(14, 18, 24, 28, -55 * 16, 110 * 16)
            painter.drawArc(22, 12, 30, 40, -55 * 16, 110 * 16)
            painter.end()
            return QtGui.QIcon(pixmap)

        def _action(
            self,
            menu: Any,
            text: str,
            *,
            checkable: bool = False,
            checked: bool = False,
            callback: Callable[..., Any] | None = None,
            enabled: bool = True,
        ) -> Any:
            action = QtGui.QAction(text, menu)
            action.setCheckable(checkable)
            action.setChecked(checked)
            action.setEnabled(enabled)
            if callback is not None:
                if checkable:
                    action.toggled.connect(callback)
                else:
                    action.triggered.connect(callback)
            menu.addAction(action)
            return action

        def _build_menu(self) -> None:
            self.menu.clear()
            self.header_action = self._action(
                self.menu, f"{APP_NAME} {__version__}", enabled=False
            )
            self.output_action = self._action(
                self.menu, "Output: this device", enabled=False
            )
            self.menu.addSeparator()

            self.share_action = self._action(
                self.menu,
                "Share this device's speakers",
                checkable=True,
                callback=self._toggle_share,
            )
            self.devices_menu = self.menu.addMenu("Play audio on")
            self.disconnect_action = self._action(
                self.menu, "Disconnect remote audio", callback=self._disconnect
            )
            self.menu.addSeparator()
            self.auto_action = self._action(
                self.menu,
                "Auto-connect to last device",
                checkable=True,
                callback=self._toggle_auto,
            )
            self._action(self.menu, "Settings…", callback=self._open_settings)
            self._action(self.menu, "Diagnostics…", callback=self._show_diagnostics)
            self._action(self.menu, "Report an issue…", callback=self._report_issue)
            self._action(self.menu, "Open configuration", callback=self._open_config)
            self.menu.addSeparator()
            self._action(self.menu, "Quit tray", callback=self._quit)
            self._update_actions()

        # -- lifecycle ---------------------------------------------------

        def show(self) -> None:
            self.tray.show()
            self._started = True

        def shutdown(self) -> None:
            self._closing = True
            self._timer.stop()
            self._executor.shutdown(wait=False, cancel_futures=True)

        def _kickstart(self) -> None:
            def work() -> bool:
                from .daemon import ensure_daemon

                return ensure_daemon()

            def done(_result: Any) -> None:
                self.refresh()

            self._run_async(work, done, quiet=True)

        # -- async helpers ----------------------------------------------

        def _run_async(
            self,
            work: Callable[[], Any],
            done: Callable[[Any], None],
            *,
            quiet: bool = False,
        ) -> None:
            future: Future[Any] = self._executor.submit(work)

            def poll() -> None:
                if self._closing:
                    return
                if not future.done():
                    QtCore.QTimer.singleShot(100, poll)
                    return
                try:
                    result = future.result()
                except Exception as exc:  # noqa: BLE001 - show the error to the user
                    if not quiet:
                        self._notify("SoundHop", str(exc), warning=True)
                    done(None)
                    return
                done(result)

            QtCore.QTimer.singleShot(40, poll)

        def _notify(self, title: str, body: str, *, warning: bool = False) -> None:
            try:
                icon = (
                    QtWidgets.QSystemTrayIcon.MessageIcon.Warning
                    if warning
                    else QtWidgets.QSystemTrayIcon.MessageIcon.Information
                )
                self.tray.showMessage(title, body, icon, 4000)
            except Exception:
                from .util import notify

                notify(title, body, urgency="critical" if warning else "normal")

        # -- refresh -----------------------------------------------------

        def refresh(self) -> None:
            if self._refreshing or not self._started or self._closing:
                return
            self._refreshing = True

            def work() -> dict[str, Any] | None:
                try:
                    return request("status", timeout=2.0)
                except (DaemonNotRunning, IpcError, OSError):
                    return None

            def done(result: Any) -> None:
                self._refreshing = False
                if isinstance(result, dict):
                    self._apply_status(result)
                else:
                    self._apply_status(None)

            self._run_async(work, done, quiet=True)

        def _apply_status(self, status: dict[str, Any] | None) -> None:
            self._status = status
            self._update_actions()
            self._rebuild_devices()

        def _update_actions(self) -> None:
            status = self._status
            if status is None:
                self.header_action.setText(f"{APP_NAME} — daemon not running")
                self.output_action.setText("Start the daemon to connect")
                self.share_action.setEnabled(False)
                self.auto_action.setEnabled(False)
                self.disconnect_action.setEnabled(False)
                self.tray.setToolTip(f"{APP_NAME} — daemon not running")
                return
            device = status.get("device", {})
            daemon = status.get("daemon", {})
            sharing = status.get("sharing", {})
            connection = status.get("connection")
            auto = status.get("auto", {})
            name = device.get("name", "this device")
            uptime = format_duration(daemon.get("uptime"))
            self.header_action.setText(f"{APP_NAME} — {name}  ({uptime})")
            self.share_action.setEnabled(True)
            self.share_action.blockSignals(True)
            self.share_action.setChecked(bool(sharing.get("enabled")))
            self.share_action.blockSignals(False)
            self.auto_action.setEnabled(True)
            self.auto_action.blockSignals(True)
            self.auto_action.setChecked(
                bool(auto.get("enabled", True)) and not bool(auto.get("paused"))
            )
            self.auto_action.blockSignals(False)
            if connection:
                self.output_action.setText(
                    f"Output: {connection.get('name', 'remote')} "
                    f"({connection.get('duration', '')})"
                )
                self.disconnect_action.setEnabled(True)
                self.tray.setToolTip(
                    f"{APP_NAME} — playing on {connection.get('name', 'remote')}"
                )
            else:
                self.output_action.setText("Output: this device")
                self.disconnect_action.setEnabled(False)
                self.tray.setToolTip(
                    f"{APP_NAME} — {'sharing' if sharing.get('enabled') else 'not sharing'}"
                )

        def _rebuild_devices(self) -> None:
            self.devices_menu.clear()
            status = self._status
            if not status:
                action = self.devices_menu.addAction("Daemon not running")
                action.setEnabled(False)
                action = self.devices_menu.addAction("Start daemon")
                action.triggered.connect(self._start_daemon)
                return
            connection = status.get("connection") or {}
            current_id = connection.get("peer_id", "")
            peers = status.get("peers", []) or []
            known = status.get("known_peers", []) or []

            this_device = QtGui.QAction("This device (disconnect)", self.devices_menu)
            this_device.setCheckable(True)
            this_device.setChecked(not current_id)
            this_device.triggered.connect(self._disconnect)
            self.devices_menu.addAction(this_device)

            if peers:
                self.devices_menu.addSeparator()
                for peer in peers:
                    label = f"{peer.get('name') or peer.get('id', 'device')}"
                    address = f"{peer.get('host', '')}:{peer.get('port', '')}"
                    action = QtGui.QAction(
                        f"{label}  ({address})", self.devices_menu
                    )
                    action.setCheckable(True)
                    action.setChecked(peer.get("id") == current_id)
                    if not peer.get("compatible", True):
                        action.setEnabled(False)
                        action.setText(action.text() + " — update needed")
                    action.triggered.connect(
                        lambda checked=False, p=peer: self._connect_peer(p)
                    )
                    self.devices_menu.addAction(action)
            elif known:
                self.devices_menu.addSeparator()
                for peer in known[:5]:
                    label = peer.get("name") or peer.get("id", "device")
                    action = QtGui.QAction(
                        f"{label}  (last seen)", self.devices_menu
                    )
                    action.setEnabled(False)
                    self.devices_menu.addAction(action)
            else:
                action = self.devices_menu.addAction("No devices found")
                action.setEnabled(False)
            self.devices_menu.addSeparator()
            self.devices_menu.addAction("Scan again", self.refresh)

        # -- actions -----------------------------------------------------

        def _on_activated(self, reason: Any) -> None:
            if reason in (
                QtWidgets.QSystemTrayIcon.ActivationReason.Trigger,
                QtWidgets.QSystemTrayIcon.ActivationReason.DoubleClick,
            ):
                platform = (QtWidgets.QApplication.platformName() or "").lower()
                if "wayland" in platform:
                    # In native Wayland sessions (e.g. KDE Plasma, GNOME Shell),
                    # unparented QMenu popups cannot grab input surfaces because the tray
                    # icon belongs to the desktop panel compositor, not an application window.
                    # Right-click displays the full menu natively via StatusNotifierItem/DBusMenu.
                    # On left-click, open the Settings window which is a valid toplevel Wayland surface.
                    self._open_settings()
                else:
                    self.menu.popup(QtGui.QCursor.pos())

        def _start_daemon(self) -> None:
            def work() -> bool:
                from .daemon import ensure_daemon

                return ensure_daemon()

            def done(result: Any) -> None:
                if not result:
                    self._notify(
                        APP_NAME,
                        "Could not start the daemon. Run 'soundhop daemon' to see why.",
                        warning=True,
                    )
                self.refresh()

            self._run_async(work, done)

        def _connect_peer(self, peer: dict[str, Any]) -> None:
            if self._busy:
                return
            self._busy = True

            def work() -> dict[str, Any]:
                return request(
                    "connect",
                    timeout=30.0,
                    peer=peer.get("id", ""),
                    host=peer.get("host", ""),
                    port=peer.get("port", 0),
                    name=peer.get("name", ""),
                    by="tray",
                )

            def done(result: Any) -> None:
                self._busy = False
                if isinstance(result, dict):
                    self._notify(
                        APP_NAME,
                        f"Audio is now playing on {result.get('name', 'the remote device')}",
                    )
                self.refresh()

            self._run_async(work, done)

        def _disconnect(self, *_args: Any) -> None:
            if self._busy:
                return
            self._busy = True

            def work() -> dict[str, Any]:
                return request("disconnect", timeout=20.0, all=False)

            def done(result: Any) -> None:
                self._busy = False
                if isinstance(result, dict) and result.get("restored_to"):
                    self._notify(
                        APP_NAME, f"Audio restored to {result['restored_to']}"
                    )
                self.refresh()

            self._run_async(work, done)

        def _toggle_share(self, checked: bool) -> None:
            def work() -> dict[str, Any]:
                return request("share", timeout=20.0, enabled=checked)

            def done(result: Any) -> None:
                if isinstance(result, dict):
                    if checked:
                        self._notify(
                            APP_NAME,
                            f"Sharing speakers on port {result.get('port', '')}",
                        )
                    else:
                        note = result.get("note", "")
                        self._notify(
                            APP_NAME,
                            "Stopped sharing this device's speakers"
                            + (f"\n{note}" if note else ""),
                        )
                self.refresh()

            self._run_async(work, done)

        def _toggle_auto(self, checked: bool) -> None:
            def work() -> dict[str, Any]:
                return request("auto", timeout=10.0, enabled=checked)

            def done(result: Any) -> None:
                self.refresh()

            self._run_async(work, done)

        def _open_settings(self) -> None:
            try:
                config = Config.load()
            except Exception as exc:
                self._notify(APP_NAME, f"Could not load configuration: {exc}", warning=True)
                return
            dialog = SettingsDialog(config)
            if dialog.exec() != QtWidgets.QDialog.DialogCode.Accepted:
                return
            patch = dialog.patch()
            if not patch:
                return

            def work() -> dict[str, Any]:
                try:
                    return request("config", timeout=20.0, patch=patch)
                except (DaemonNotRunning, IpcError, OSError):
                    cfg = Config.load()
                    cfg.apply_patch(patch)
                    cfg.save()
                    return {"config": cfg.to_dict()}

            def done(result: Any) -> None:
                self._notify(APP_NAME, "Settings saved.")
                self.refresh()

            self._run_async(work, done)

        def _show_diagnostics(self) -> None:
            def work() -> dict[str, Any]:
                try:
                    return request("system", timeout=4.0)
                except (DaemonNotRunning, IpcError, OSError):
                    return {}

            def done(result: Any) -> None:
                if not isinstance(result, dict) or not result:
                    self._notify(
                        APP_NAME,
                        "The daemon is not running. Start it with 'soundhop daemon'.",
                        warning=True,
                    )
                    return
                backend = result.get("backend", {})
                sharing = result.get("sharing", {})
                connection = result.get("connection") or {}
                daemon = result.get("daemon", {})
                peers = result.get("peers", []) or []
                lines = [
                    f"SoundHop {__version__}",
                    f"Device: {result.get('device', {}).get('name', '')}",
                    f"Daemon: pid {daemon.get('pid', '?')} ({format_duration(daemon.get('uptime'))})",
                    f"Backend: {backend.get('server', 'unavailable')}",
                    f"Default sink: {backend.get('default_sink', '')}",
                    f"Sharing: {'on' if sharing.get('enabled') else 'off'} "
                    f"port {sharing.get('port', '')} ({sharing.get('security', '')})",
                    f"Output: {connection.get('name', 'this device')}",
                    f"Peers: {len(peers)}",
                    f"Config: {result.get('config_path', config_path())}",
                ]
                for item in result.get("warnings", []) or []:
                    lines.append(f"Warning: {item}")
                QtWidgets.QMessageBox.information(
                    None, f"{APP_NAME} diagnostics", "\n".join(lines)
                )

            self._run_async(work, done)

        def _open_config(self) -> None:
            try:
                config = Config.load()
                path = config.path
            except Exception:
                path = config_path()
            QtGui.QDesktopServices.openUrl(QtCore.QUrl.fromLocalFile(str(path)))

        def _report_issue(self) -> None:
            QtGui.QDesktopServices.openUrl(
                QtCore.QUrl("https://github.com/soundhop/soundhop/issues/new")
            )

        def _quit(self) -> None:
            try:
                config = Config.load()
                if getattr(config.connection, "disconnect_on_quit", True):
                    request("disconnect", timeout=4.0, restore=True)
            except Exception:
                pass
            self.shutdown()
            app = QtWidgets.QApplication.instance()
            if app is not None:
                app.quit()

    return Tray, SettingsDialog
