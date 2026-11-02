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
from .android import AndroidManager, SUPPORTED_AUDIO_CODECS
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
        app.setDesktopFileName("io.github.soundhop.desktop")
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

            # Android audio settings
            self.android_box = QtWidgets.QGroupBox("Android Audio (scrcpy)")
            ab_layout = QtWidgets.QFormLayout(self.android_box)
            self.android_path = QtWidgets.QLineEdit(getattr(config.android, "scrcpy_path", ""))
            self.android_path.setPlaceholderText("Auto-detect / system PATH")
            self.android_codec = QtWidgets.QComboBox()
            for c in ("aac", "opus", "flac", "raw"):
                self.android_codec.addItem(c.upper(), c)
            cur_codec = getattr(config.android, "default_codec", "aac")
            codec_idx = self.android_codec.findData(cur_codec)
            self.android_codec.setCurrentIndex(max(0, codec_idx))
            self.android_buffer = QtWidgets.QSpinBox()
            self.android_buffer.setRange(10, 500)
            self.android_buffer.setValue(getattr(config.android, "buffer_ms", 50))
            self.android_buffer.setSuffix(" ms")
            ab_layout.addRow("scrcpy executable", self.android_path)
            ab_layout.addRow("Default codec", self.android_codec)
            ab_layout.addRow("Audio buffer", self.android_buffer)
            layout.addRow(self.android_box)

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
                "android": {
                    "scrcpy_path": self.android_path.text().strip(),
                    "default_codec": self.android_codec.currentData(),
                    "buffer_ms": int(self.android_buffer.value()),
                },
            }
            self.dialog.accept()

        def patch(self) -> dict[str, Any]:
            return self._patch

    class AndroidWizardDialog:
        def __init__(
            self,
            parent: Any,
            executor: ThreadPoolExecutor,
            notify_fn: Callable[..., Any],
        ) -> None:
            self._executor = executor
            self._notify = notify_fn
            self.dialog = QtWidgets.QDialog(parent)
            self.dialog.setWindowTitle("Android Audio Capture — SoundHop")
            self.dialog.setMinimumWidth(500)
            layout = QtWidgets.QVBoxLayout(self.dialog)

            # Tool Status Box
            self.tool_box = QtWidgets.QGroupBox("scrcpy & adb Tools")
            tb_layout = QtWidgets.QVBoxLayout(self.tool_box)
            self.tool_label = QtWidgets.QLabel("Detecting tools...")
            tb_layout.addWidget(self.tool_label)

            self.install_btn = QtWidgets.QPushButton("Download & Install Standalone scrcpy")
            self.install_btn.setStyleSheet(
                "font-weight: bold; background-color: #2b78e4; color: white; padding: 6px;"
            )
            self.install_btn.clicked.connect(self._install_tools)
            self.install_btn.hide()
            tb_layout.addWidget(self.install_btn)

            self.progress = QtWidgets.QProgressBar()
            self.progress.setRange(0, 100)
            self.progress.hide()
            tb_layout.addWidget(self.progress)
            layout.addWidget(self.tool_box)

            # Devices Box
            self.dev_box = QtWidgets.QGroupBox("Connected Android Devices")
            db_layout = QtWidgets.QVBoxLayout(self.dev_box)
            self.dev_list = QtWidgets.QListWidget()
            self.dev_list.currentRowChanged.connect(self._device_selected)
            db_layout.addWidget(self.dev_list)

            btn_row = QtWidgets.QHBoxLayout()
            self.refresh_btn = QtWidgets.QPushButton("Refresh Devices")
            self.refresh_btn.clicked.connect(self._refresh_all)
            btn_row.addWidget(self.refresh_btn)

            self.switch_wifi_btn = QtWidgets.QPushButton("Switch to Wi-Fi")
            self.switch_wifi_btn.setEnabled(False)
            self.switch_wifi_btn.clicked.connect(self._switch_wifi)
            btn_row.addWidget(self.switch_wifi_btn)
            db_layout.addLayout(btn_row)

            # Manual Wi-Fi Connect row
            wifi_row = QtWidgets.QHBoxLayout()
            self.ip_input = QtWidgets.QLineEdit()
            self.ip_input.setPlaceholderText("Connect via Wi-Fi IP (e.g. 192.168.1.99)")
            self.ip_btn = QtWidgets.QPushButton("Connect IP")
            self.ip_btn.clicked.connect(self._connect_ip)
            wifi_row.addWidget(self.ip_input)
            wifi_row.addWidget(self.ip_btn)
            db_layout.addLayout(wifi_row)

            tip = QtWidgets.QLabel(
                "Tip: Enable 'USB Debugging' in Android Developer Options. "
                "Connect via cable and accept 'Allow debugging' on phone screen."
            )
            tip.setWordWrap(True)
            tip.setStyleSheet("color: palette(mid); font-size: 11px;")
            db_layout.addWidget(tip)
            layout.addWidget(self.dev_box)

            # Capture Settings Box
            self.cfg_box = QtWidgets.QGroupBox("Audio Capture Settings")
            cb_layout = QtWidgets.QFormLayout(self.cfg_box)

            self.codec_combo = QtWidgets.QComboBox()
            self.codec_combo.addItem("AAC (Recommended - Low Latency)", "aac")
            self.codec_combo.addItem("Opus (High Quality)", "opus")
            self.codec_combo.addItem("FLAC (Lossless)", "flac")
            self.codec_combo.addItem("Raw PCM", "raw")
            cb_layout.addRow("Codec", self.codec_combo)

            self.source_combo = QtWidgets.QComboBox()
            self.source_combo.addItem("Output (Android 12+ System Output)", "output")
            self.source_combo.addItem("Playback (Android 11+ App Audio)", "playback")
            self.source_combo.addItem("Microphone (Android 10 / Mic)", "mic")
            cb_layout.addRow("Source", self.source_combo)

            self.buffer_spin = QtWidgets.QSpinBox()
            self.buffer_spin.setRange(10, 500)
            self.buffer_spin.setValue(50)
            self.buffer_spin.setSuffix(" ms")
            cb_layout.addRow("Buffer latency", self.buffer_spin)

            self.dup_check = QtWidgets.QCheckBox(
                "Duplicate audio (keep playing on device too)"
            )
            cb_layout.addRow(self.dup_check)
            layout.addWidget(self.cfg_box)

            # Control Row
            ctrl_box = QtWidgets.QHBoxLayout()
            self.status_lbl = QtWidgets.QLabel("Status: Idle")
            ctrl_box.addWidget(self.status_lbl)

            self.toggle_btn = QtWidgets.QPushButton("Start Audio Capture")
            self.toggle_btn.setStyleSheet("font-weight: bold; padding: 6px 14px;")
            self.toggle_btn.clicked.connect(self._toggle_capture)
            ctrl_box.addWidget(self.toggle_btn)
            layout.addLayout(ctrl_box)

            close_btn = QtWidgets.QPushButton("Close")
            close_btn.clicked.connect(self.dialog.accept)
            layout.addWidget(close_btn)

            self._devices: list[dict[str, Any]] = []
            self._is_capturing = False
            self._mgr = AndroidManager()
            self._session: Any = None

            self._timer = QtCore.QTimer(self.dialog)
            self._timer.setInterval(1000)
            self._timer.timeout.connect(self._poll_status)
            self._timer.start()

            self._refresh_all()

        def exec(self) -> int:
            return int(self.dialog.exec())

        def _refresh_all(self) -> None:
            try:
                tools = self._mgr.get_tool_status()
                if tools.scrcpy_path:
                    self.tool_label.setText(
                        f"✓ scrcpy {tools.scrcpy_version} ({tools.scrcpy_path})\n"
                        f"✓ adb {tools.adb_version} ({tools.adb_path})"
                    )
                    self.tool_label.setStyleSheet("color: #2e7d32;")
                    self.install_btn.hide()
                else:
                    self.tool_label.setText("scrcpy is not installed on this system.")
                    self.tool_label.setStyleSheet("color: #d32f2f;")
                    self.install_btn.show()
            except Exception as exc:
                self.tool_label.setText(f"Error checking tools: {exc}")

            self.dev_list.clear()
            try:
                self._devices = [
                    {
                        "serial": d.serial,
                        "display_name": d.display_name,
                        "is_wifi": d.is_wifi,
                        "state": d.state,
                        "android_version": d.android_version,
                        "sdk_version": d.sdk_version,
                    }
                    for d in self._mgr.list_devices()
                ]
                if not self._devices:
                    self.dev_list.addItem("No connected devices found")
                else:
                    for d in self._devices:
                        kind = "Wi-Fi" if d["is_wifi"] else "USB"
                        ver = f"Android {d['android_version']}" if d.get("android_version") else ""
                        item = QtWidgets.QListWidgetItem(
                            f"{d['display_name']} [{kind}] {ver} — {d['state']}"
                        )
                        self.dev_list.addItem(item)
                    self.dev_list.setCurrentRow(0)
            except Exception as exc:
                self.dev_list.addItem(f"Error listing devices: {exc}")

            self._poll_status()

        def _device_selected(self, row: int) -> None:
            if 0 <= row < len(self._devices):
                dev = self._devices[row]
                self.switch_wifi_btn.setEnabled(not dev["is_wifi"] and dev["state"] == "device")
                sdk = dev.get("sdk_version", 0)
                if sdk >= 31:
                    idx = self.source_combo.findData("output")
                    if idx >= 0:
                        self.source_combo.setCurrentIndex(idx)
                elif sdk == 30:
                    idx = self.source_combo.findData("playback")
                    if idx >= 0:
                        self.source_combo.setCurrentIndex(idx)
                elif sdk > 0 and sdk < 30:
                    idx = self.source_combo.findData("mic")
                    if idx >= 0:
                        self.source_combo.setCurrentIndex(idx)
            else:
                self.switch_wifi_btn.setEnabled(False)

        def _switch_wifi(self) -> None:
            row = self.dev_list.currentRow()
            if not (0 <= row < len(self._devices)):
                return
            dev = self._devices[row]
            serial = dev["serial"]
            self.switch_wifi_btn.setEnabled(False)

            def work() -> tuple[bool, str]:
                ip = self._mgr.get_device_wifi_ip(serial)
                ok, msg = self._mgr.enable_tcpip(serial)
                if ok and ip:
                    import time
                    time.sleep(1.0)
                    conn_ok, conn_msg = self._mgr.connect_wifi(ip)
                    if conn_ok:
                        return True, f"Connected to {ip}:5555"
                return ok, msg

            def done(res: Any) -> None:
                if isinstance(res, tuple):
                    ok_f, msg = res
                    if ok_f:
                        self._notify(APP_NAME, f"Switched to Wi-Fi: {msg}")
                    else:
                        self._notify(APP_NAME, f"Wi-Fi switch error: {msg}", warning=True)
                self._refresh_all()

            future = self._executor.submit(work)

            def poll() -> None:
                if not future.done():
                    QtCore.QTimer.singleShot(100, poll)
                else:
                    done(future.result())

            QtCore.QTimer.singleShot(50, poll)

        def _connect_ip(self) -> None:
            ip = self.ip_input.text().strip()
            if not ip:
                return

            def work() -> tuple[bool, str]:
                return self._mgr.connect_wifi(ip)

            def done(res: Any) -> None:
                if isinstance(res, tuple):
                    ok_f, msg = res
                    if ok_f:
                        self._notify(APP_NAME, msg)
                    else:
                        self._notify(APP_NAME, f"Connection failed: {msg}", warning=True)
                self._refresh_all()

            future = self._executor.submit(work)

            def poll() -> None:
                if not future.done():
                    QtCore.QTimer.singleShot(100, poll)
                else:
                    done(future.result())

            QtCore.QTimer.singleShot(50, poll)

        def _install_tools(self) -> None:
            self.install_btn.setEnabled(False)
            self.progress.show()
            self.progress.setValue(0)

            def work() -> Path:
                def prog(cur: int, tot: int) -> None:
                    if tot > 0:
                        QtCore.QMetaObject.invokeMethod(
                            self.progress,
                            "setValue",
                            QtCore.Qt.ConnectionType.QueuedConnection,
                            QtCore.Q_ARG(int, int((cur / tot) * 100)),
                        )

                return self._mgr.install_scrcpy(progress_cb=prog)

            def done(res: Any) -> None:
                self.progress.hide()
                self.install_btn.setEnabled(True)
                self._notify(APP_NAME, "Standalone scrcpy installed successfully!")
                self._refresh_all()

            future = self._executor.submit(work)

            def poll() -> None:
                if not future.done():
                    QtCore.QTimer.singleShot(100, poll)
                else:
                    try:
                        done(future.result())
                    except Exception as exc:
                        self.progress.hide()
                        self.install_btn.setEnabled(True)
                        self._notify(APP_NAME, f"Install failed: {exc}", warning=True)

            QtCore.QTimer.singleShot(50, poll)

        def _toggle_capture(self) -> None:
            if self._is_capturing:
                def work() -> dict[str, Any]:
                    if self._session is not None:
                        try:
                            self._session.stop()
                        except Exception:
                            pass
                        self._session = None
                    try:
                        return request("android_stop", timeout=4.0)
                    except Exception:
                        from .util import run as r_run

                        r_run(["killall", "-q", "scrcpy"])
                        return {}

                def done(_: Any) -> None:
                    self._is_capturing = False
                    self._poll_status()

                future = self._executor.submit(work)

                def poll() -> None:
                    if not future.done():
                        QtCore.QTimer.singleShot(100, poll)
                    else:
                        done(future.result())

                QtCore.QTimer.singleShot(50, poll)
            else:
                row = self.dev_list.currentRow()
                if not (0 <= row < len(self._devices)):
                    self._notify(
                        APP_NAME, "Please select an Android device first.", warning=True
                    )
                    return
                dev = self._devices[row]
                serial = dev["serial"]
                codec = self.codec_combo.currentData()
                source = self.source_combo.currentData()
                buffer_ms = int(self.buffer_spin.value())
                dup = self.dup_check.isChecked()

                def work() -> dict[str, Any]:
                    try:
                        return request(
                            "android_start",
                            timeout=10.0,
                            serial=serial,
                            name=dev["display_name"],
                            codec=codec,
                            source=source,
                            buffer_ms=buffer_ms,
                            duplicate=dup,
                        )
                    except (DaemonNotRunning, IpcError, OSError):
                        from .android import AndroidCaptureSession

                        self._session = AndroidCaptureSession(self._mgr)
                        return self._session.start(
                            serial,
                            device_name=dev["display_name"],
                            codec=codec,
                            source=source,
                            buffer_ms=buffer_ms,
                            duplicate=dup,
                        )

                def done(res: Any) -> None:
                    self._poll_status()

                future = self._executor.submit(work)

                def poll() -> None:
                    if not future.done():
                        QtCore.QTimer.singleShot(100, poll)
                    else:
                        try:
                            done(future.result())
                        except Exception as exc:
                            self._notify(
                                APP_NAME, f"Capture failed: {exc}", warning=True
                            )

                QtCore.QTimer.singleShot(50, poll)

        def _poll_status(self) -> None:
            def work() -> dict[str, Any]:
                if self._session is not None and self._session.is_active:
                    return {"session": self._session.status()}
                try:
                    return request("android_status", timeout=2.0)
                except Exception:
                    from .util import process_running

                    return {"session": {"active": process_running("scrcpy")}}

            def done(res: Any) -> None:
                if not isinstance(res, dict):
                    return
                session = res.get("session", {})
                active = bool(session.get("active"))
                self._is_capturing = active
                if active:
                    name = session.get("device_name") or session.get("serial", "Android")
                    codec = str(session.get("codec", "")).upper()
                    uptime = format_duration(session.get("uptime_s"))
                    self.status_lbl.setText(
                        f"Status: Streaming from {name} ({codec}, {uptime})"
                    )
                    self.status_lbl.setStyleSheet("color: #2e7d32; font-weight: bold;")
                    self.toggle_btn.setText("Stop Audio Capture")
                    self.toggle_btn.setStyleSheet(
                        "font-weight: bold; background-color: #d32f2f; color: white; padding: 6px 14px;"
                    )
                else:
                    self.status_lbl.setText("Status: Idle")
                    self.status_lbl.setStyleSheet(
                        "color: palette(windowText); font-weight: normal;"
                    )
                    self.toggle_btn.setText("Start Audio Capture")
                    self.toggle_btn.setStyleSheet(
                        "font-weight: bold; background-color: #2e7d32; color: white; padding: 6px 14px;"
                    )

            future = self._executor.submit(work)

            def poll() -> None:
                if not future.done():
                    QtCore.QTimer.singleShot(100, poll)
                else:
                    done(future.result())

            QtCore.QTimer.singleShot(30, poll)

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

            android = status.get("android") or {}
            if android.get("active"):
                dev_name = (
                    android.get("device_name") or android.get("serial", "Android")
                )
                self.android_stop_action.setText(f"Stop Android audio ({dev_name})")
                self.android_stop_action.setVisible(True)
                self.android_capture_action.setText("Android audio settings…")
            else:
                self.android_stop_action.setVisible(False)
                self.android_capture_action.setText("Capture Android audio…")

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

        def _open_android_wizard(self) -> None:
            dialog = AndroidWizardDialog(None, self._executor, self._notify)
            dialog.exec()
            self.refresh()

        def _stop_android(self) -> None:
            def work() -> dict[str, Any]:
                try:
                    return request("android_stop", timeout=3.0)
                except Exception:
                    from .util import run as r_run

                    r_run(["killall", "-q", "scrcpy"])
                    return {}

            def done(_: Any) -> None:
                self._notify(APP_NAME, "Stopped Android audio capture.")
                self.refresh()

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
