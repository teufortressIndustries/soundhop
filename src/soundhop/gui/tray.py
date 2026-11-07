"""System tray controller and menu for the SoundHop Qt GUI."""

from __future__ import annotations

import logging
import os
import signal
from collections.abc import Callable
from concurrent.futures import Future, ThreadPoolExecutor
from typing import Any

from .. import __version__
from ..constants import APP_NAME, ISSUES_URL
from ..ipc import DaemonNotRunning, IpcError
from ..util import config_path, format_duration

log = logging.getLogger(__name__)


def _ipc_request(command: str, **kwargs: Any) -> dict[str, Any]:
    from .. import gui as _gui_pkg

    return _gui_pkg.request(command, **kwargs)


def _load_config() -> Any:
    from .. import gui as _gui_pkg

    return _gui_pkg.Config.load()


def build_tray_class(
    QtCore: Any,
    QtGui: Any,
    QtWidgets: Any,
    *,
    SettingsDialog: type,
    AndroidWizardDialog: type,
) -> type:
    class Tray(QtCore.QObject):
        def __init__(self, *, verbose: bool = False) -> None:
            super().__init__()
            self.verbose = verbose
            self._executor = ThreadPoolExecutor(max_workers=2, thread_name_prefix="soundhop-tray")
            self._status: dict[str, Any] | None = None
            self._refreshing = False
            self._started = False
            self._busy = False
            self._closing = False
            self._android_dialog: Any = None
            self._cleaned_up_on_quit = False

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

            app = QtWidgets.QApplication.instance()
            if app is not None:
                try:
                    app.aboutToQuit.connect(self._disconnect_on_quit_if_enabled)
                except Exception:
                    pass

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
            self.header_action = self._action(self.menu, f"{APP_NAME} {__version__}", enabled=False)
            self.output_action = self._action(self.menu, "Output: this device", enabled=False)
            self.menu.addSeparator()

            self.share_action = self._action(
                self.menu,
                "Share this device's speakers",
                checkable=True,
                callback=self._toggle_share,
            )
            self.devices_menu = self.menu.addMenu("Play audio on")
            self.apps_menu = self.menu.addMenu("Route applications")
            self.volume_menu = self.menu.addMenu("Volume")
            for pct in (100, 75, 50, 25):
                act = self.volume_menu.addAction(f"{pct}%")
                act.triggered.connect(lambda _, v=pct: self._set_volume_preset(v))
            self.volume_menu.addSeparator()
            self.mute_action = self._action(
                self.volume_menu, "Mute", checkable=True, callback=self._toggle_mute
            )
            self.mic_menu = self.menu.addMenu("Microphone")
            self.mic_send_action = self._action(
                self.mic_menu,
                "Send this device's mic to remote PC",
                checkable=True,
                callback=lambda: self._set_mic_mode("send"),
            )
            self.mic_recv_action = self._action(
                self.mic_menu,
                "Receive remote mic on this device",
                checkable=True,
                callback=lambda: self._set_mic_mode("receive"),
            )
            self.mic_menu.addSeparator()
            self.mic_off_action = self._action(
                self.mic_menu,
                "Off",
                callback=lambda: self._set_mic_mode("off"),
            )
            self.disconnect_action = self._action(
                self.menu, "Disconnect remote audio", callback=self._disconnect
            )
            self.menu.addSeparator()
            self.android_capture_action = self._action(
                self.menu,
                "Capture Android audio…",
                callback=self._open_android_wizard,
            )
            self.android_stop_action = self._action(
                self.menu,
                "Stop Android audio",
                callback=self._stop_android,
            )
            self.android_stop_action.setVisible(False)
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
            self._disconnect_on_quit_if_enabled()
            self._timer.stop()
            self._executor.shutdown(wait=False, cancel_futures=True)

        def _kickstart(self) -> None:
            def work() -> bool:
                from ..daemon import ensure_daemon

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
                from ..util import notify

                notify(title, body, urgency="critical" if warning else "normal")

        # -- refresh -----------------------------------------------------

        def refresh(self) -> None:
            if self._refreshing or not self._started or self._closing:
                return
            self._refreshing = True

            def work() -> dict[str, Any] | None:
                try:
                    return _ipc_request("status", timeout=2.0)
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
            self._rebuild_apps()

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
                    f"Output: {connection.get('name', 'remote')} ({connection.get('duration', '')})"
                )
                self.disconnect_action.setEnabled(True)
                self.mic_menu.setEnabled(True)
                has_mic = bool(connection.get("with_mic"))
                mic_dir = connection.get("mic_direction") or "receive"
                is_send = has_mic and mic_dir == "send"
                is_recv = has_mic and mic_dir == "receive"
                self.mic_send_action.blockSignals(True)
                self.mic_send_action.setChecked(is_send)
                self.mic_send_action.blockSignals(False)
                self.mic_recv_action.blockSignals(True)
                self.mic_recv_action.setChecked(is_recv)
                self.mic_recv_action.blockSignals(False)
                if is_send:
                    self.mic_menu.setTitle("Microphone (Sending to remote)")
                    self.tray.setToolTip(
                        f"{APP_NAME} — playing on {connection.get('name', 'remote')} (Mic: Out)"
                    )
                elif is_recv:
                    self.mic_menu.setTitle("Microphone (Receiving from remote)")
                    self.tray.setToolTip(
                        f"{APP_NAME} — playing on {connection.get('name', 'remote')} (Mic: In)"
                    )
                else:
                    self.mic_menu.setTitle("Microphone (Off)")
                    self.tray.setToolTip(
                        f"{APP_NAME} — playing on {connection.get('name', 'remote')}"
                    )
            else:
                self.output_action.setText("Output: this device")
                self.disconnect_action.setEnabled(False)
                self.mic_menu.setEnabled(False)
                self.mic_menu.setTitle("Microphone")
                self.mic_send_action.blockSignals(True)
                self.mic_send_action.setChecked(False)
                self.mic_send_action.blockSignals(False)
                self.mic_recv_action.blockSignals(True)
                self.mic_recv_action.setChecked(False)
                self.mic_recv_action.blockSignals(False)
                self.tray.setToolTip(
                    f"{APP_NAME} — {'sharing' if sharing.get('enabled') else 'not sharing'}"
                )

            android = status.get("android") or {}
            if android.get("active"):
                dev_name = android.get("device_name") or android.get("serial", "Android")
                self.android_stop_action.setText(f"Stop Android audio ({dev_name})")
                self.android_stop_action.setVisible(True)
                self.android_capture_action.setText("Android audio settings…")
            else:
                self.android_stop_action.setVisible(False)
                self.android_capture_action.setText("Capture Android audio…")

            vol_info = status.get("volume") or {}
            vol_lvl = vol_info.get("level", 100)
            vol_mute = bool(vol_info.get("mute", False))
            self.volume_menu.setTitle(f"Volume ({vol_lvl}%{' - Muted' if vol_mute else ''})")
            self.mute_action.blockSignals(True)
            self.mute_action.setChecked(vol_mute)
            self.mute_action.blockSignals(False)

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
                    action = QtGui.QAction(f"{label}  ({address})", self.devices_menu)
                    action.setCheckable(True)
                    action.setChecked(peer.get("id") == current_id)
                    if not peer.get("compatible", True):
                        action.setEnabled(False)
                        action.setText(action.text() + " — update needed")
                    action.triggered.connect(lambda checked=False, p=peer: self._connect_peer(p))
                    self.devices_menu.addAction(action)
            elif known:
                self.devices_menu.addSeparator()
                for peer in known[:5]:
                    label = peer.get("name") or peer.get("id", "device")
                    action = QtGui.QAction(f"{label}  (last seen)", self.devices_menu)
                    action.setEnabled(False)
                    self.devices_menu.addAction(action)
            else:
                action = self.devices_menu.addAction("No devices found")
                action.setEnabled(False)
            self.devices_menu.addSeparator()
            self.devices_menu.addAction("Scan again", self.refresh)

        def _rebuild_apps(self) -> None:
            self.apps_menu.clear()
            status = self._status
            if not status:
                self.apps_menu.setEnabled(False)
                return

            def work() -> list[dict[str, Any]]:
                try:
                    res = _ipc_request("apps_list", timeout=2.0)
                    return list(res.get("apps", []))
                except Exception:
                    return []

            def done(apps: list[dict[str, Any]]) -> None:
                self.apps_menu.clear()
                if not apps:
                    action = self.apps_menu.addAction("No active playback streams")
                    action.setEnabled(False)
                    self.apps_menu.setEnabled(True)
                    return

                self.apps_menu.setEnabled(True)
                for item in apps:
                    app_name = item.get("app_name") or item.get("binary") or f"Stream #{item['id']}"
                    target_identifier = (
                        item.get("app_name") or item.get("binary") or str(item["id"])
                    )
                    is_rem = bool(item.get("is_remote", False))
                    action = QtGui.QAction(
                        f"{app_name}  ({'Remote' if is_rem else 'Local'})", self.apps_menu
                    )
                    action.setCheckable(True)
                    action.setChecked(is_rem)
                    action.triggered.connect(
                        lambda checked=False, ident=target_identifier: self._toggle_app_route(
                            ident, checked
                        )
                    )
                    self.apps_menu.addAction(action)

            self._run_async(work, done, quiet=True)

        def _toggle_app_route(self, app_identifier: str | int, checked: bool) -> None:
            target = "remote" if checked else "local"

            def work() -> dict[str, Any]:
                return _ipc_request("route_app", app=app_identifier, target=target, timeout=3.0)

            def done(res: Any) -> None:
                self.refresh()

            self._run_async(work, done, quiet=True)

        def _set_mic_mode(self, mode: str) -> None:
            if mode == "off":
                cmd = "disable_mic"
                kwargs: dict[str, Any] = {}
            elif mode == "send":
                cmd = "enable_mic"
                kwargs = {"direction": "send"}
            else:
                cmd = "enable_mic"
                kwargs = {"direction": "receive"}

            def work() -> dict[str, Any]:
                return _ipc_request(cmd, timeout=5.0, **kwargs)

            def done(res: Any) -> None:
                self.refresh()

            self._run_async(work, done, quiet=True)

        def _set_volume_preset(self, volume: int) -> None:
            def work() -> dict[str, Any]:
                return _ipc_request("set_volume", volume=volume, timeout=3.0)

            def done(res: Any) -> None:
                self.refresh()

            self._run_async(work, done, quiet=True)

        def _toggle_mute(self) -> None:
            muted = self.mute_action.isChecked()

            def work() -> dict[str, Any]:
                return _ipc_request("set_volume", mute=muted, timeout=3.0)

            def done(res: Any) -> None:
                self.refresh()

            self._run_async(work, done, quiet=True)

        # -- actions -----------------------------------------------------

        def _on_activated(self, reason: Any) -> None:
            if reason in (
                QtWidgets.QSystemTrayIcon.ActivationReason.Trigger,
                QtWidgets.QSystemTrayIcon.ActivationReason.DoubleClick,
            ):
                platform = (QtWidgets.QApplication.platformName() or "").lower()
                if "wayland" in platform:
                    self._open_settings()
                else:
                    self.menu.popup(QtGui.QCursor.pos())

        def _start_daemon(self) -> None:
            def work() -> bool:
                from ..daemon import ensure_daemon

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

            with_mic = self.mic_send_action.isChecked() or self.mic_recv_action.isChecked()
            mic_direction = "send" if self.mic_send_action.isChecked() else "receive"

            def work() -> dict[str, Any]:
                return _ipc_request(
                    "connect",
                    timeout=30.0,
                    peer=peer.get("id", ""),
                    host=peer.get("host", ""),
                    port=peer.get("port", 0),
                    name=peer.get("name", ""),
                    by="tray",
                    with_mic=with_mic,
                    mic_direction=mic_direction,
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
                return _ipc_request("disconnect", timeout=20.0, all=False)

            def done(result: Any) -> None:
                self._busy = False
                if isinstance(result, dict) and result.get("restored_to"):
                    self._notify(APP_NAME, f"Audio restored to {result['restored_to']}")
                self.refresh()

            self._run_async(work, done)

        def _toggle_share(self, checked: bool) -> None:
            def work() -> dict[str, Any]:
                return _ipc_request("share", timeout=20.0, enabled=checked)

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
                return _ipc_request("auto", timeout=10.0, enabled=checked)

            def done(result: Any) -> None:
                self.refresh()

            self._run_async(work, done)

        def _open_settings(self) -> None:
            try:
                config = _load_config()
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
                    return _ipc_request("config", timeout=20.0, patch=patch)
                except (DaemonNotRunning, IpcError, OSError):
                    cfg = _load_config()
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
                    return _ipc_request("system", timeout=4.0)
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
                QtWidgets.QMessageBox.information(None, f"{APP_NAME} diagnostics", "\n".join(lines))

            self._run_async(work, done)

        def _open_android_wizard(self) -> None:
            dialog = AndroidWizardDialog(None, self._executor, self._notify)
            self._android_dialog = dialog
            dialog.exec()
            self.refresh()

        def _stop_android(self) -> None:
            android = (self._status or {}).get("android") or {}
            active_pid = int(android.get("pid", 0) or 0)

            def work() -> dict[str, Any]:
                if getattr(self, "_android_dialog", None) is not None:
                    try:
                        self._android_dialog.stop_capture()
                    except Exception:
                        pass
                try:
                    return _ipc_request("android_stop", timeout=3.0)
                except Exception:
                    if active_pid > 0:
                        try:
                            os.kill(active_pid, signal.SIGTERM)
                        except OSError:
                            pass
                    return {}

            def done(_: Any) -> None:
                self._notify(APP_NAME, "Stopped Android audio capture.")
                self.refresh()

            self._run_async(work, done)

        def _open_config(self) -> None:
            try:
                config = _load_config()
                path = config.path
            except Exception:
                path = config_path()
            QtGui.QDesktopServices.openUrl(QtCore.QUrl.fromLocalFile(str(path)))

        def _report_issue(self) -> None:
            QtGui.QDesktopServices.openUrl(QtCore.QUrl(f"{ISSUES_URL}/new"))

        def _disconnect_on_quit_if_enabled(self) -> None:
            if getattr(self, "_cleaned_up_on_quit", False):
                return
            self._cleaned_up_on_quit = True
            try:
                config = _load_config()
                if not getattr(config.connection, "disconnect_on_quit", True):
                    return
            except Exception:
                return

            try:
                _ipc_request("disconnect", timeout=4.0, restore=True, stop_android=True)
            except Exception:
                pass
            try:
                _ipc_request("android_stop", timeout=3.0)
            except Exception:
                pass
            if getattr(self, "_android_dialog", None) is not None:
                try:
                    self._android_dialog.stop_capture()
                except Exception:
                    pass
            android = (self._status or {}).get("android") or {}
            active_pid = int(android.get("pid", 0) or 0)
            if active_pid > 0:
                try:
                    os.kill(active_pid, signal.SIGTERM)
                except OSError:
                    pass

        def _quit(self) -> None:
            self._disconnect_on_quit_if_enabled()
            self.shutdown()
            app = QtWidgets.QApplication.instance()
            if app is not None:
                app.quit()

    return Tray
