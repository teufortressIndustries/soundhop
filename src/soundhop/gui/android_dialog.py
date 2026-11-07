"""Android audio capture wizard dialog for the SoundHop Qt system tray."""

from __future__ import annotations

import logging
import os
import re
import signal
from collections.abc import Callable
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from typing import Any

from ..android import AndroidManager
from ..constants import APP_NAME
from ..ipc import DaemonNotRunning, IpcError
from ..state import State
from ..util import format_duration

log = logging.getLogger(__name__)


def _ipc_request(command: str, **kwargs: Any) -> dict[str, Any]:
    from .. import gui as _gui_pkg

    return _gui_pkg.request(command, **kwargs)


def _load_config() -> Any:
    from .. import gui as _gui_pkg

    return _gui_pkg.Config.load()


def build_android_wizard_class(QtCore: Any, QtGui: Any, QtWidgets: Any) -> type:
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
            self.refresh_btn.clicked.connect(self._refresh_all_async)
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

            # Recent Wi-Fi History Reconnect row
            history_row = QtWidgets.QHBoxLayout()
            self.history_combo = QtWidgets.QComboBox()
            self.history_combo.setSizePolicy(
                QtWidgets.QSizePolicy.Policy.Expanding, QtWidgets.QSizePolicy.Policy.Fixed
            )
            self.history_btn = QtWidgets.QPushButton("Reconnect Recent")
            self.history_btn.clicked.connect(self._reconnect_history)
            history_row.addWidget(self.history_combo)
            history_row.addWidget(self.history_btn)
            db_layout.addLayout(history_row)

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

            self.dup_check = QtWidgets.QCheckBox("Duplicate audio (keep playing on device too)")
            cb_layout.addRow(self.dup_check)

            self.as_mic_check = QtWidgets.QCheckBox(
                "Use as PC microphone (virtual input for Discord & calls)"
            )
            self.as_mic_check.toggled.connect(self._on_as_mic_toggled)
            cb_layout.addRow(self.as_mic_check)
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
            scrcpy_path = ""
            adb_path = ""
            try:
                cfg = _load_config()
                scrcpy_path = getattr(cfg.android, "scrcpy_path", "")
                adb_path = getattr(cfg.android, "adb_path", "")
            except Exception:
                pass
            self._mgr = AndroidManager(scrcpy_path=scrcpy_path, adb_path=adb_path)
            self._session: Any = None

            self._timer = QtCore.QTimer(self.dialog)
            self._timer.setInterval(1000)
            self._timer.timeout.connect(self._poll_status)
            self._timer.start()

            self._refresh_all_async()

        def exec(self) -> int:
            return int(self.dialog.exec())

        def _fetch_local_payload(self) -> dict[str, Any]:
            tools = self._mgr.get_tool_status()
            raw_devs = self._mgr.list_devices()
            history_list: list[dict[str, Any]] = []
            try:
                state = State.load()
                devs = list(state.known_android_devices.values())
                devs.sort(key=lambda d: max(d.last_connected, d.last_seen), reverse=True)
                for d in devs:
                    history_list.append(
                        {
                            "serial": d.serial,
                            "display_name": d.display_name,
                            "last_ip": d.last_ip,
                            "last_port": d.last_port,
                            "is_wifi": d.is_wifi,
                        }
                    )
            except Exception as exc:
                log.debug("Error loading local Android history: %s", exc)

            return {
                "tools": {
                    "scrcpy_path": str(tools.scrcpy_path) if tools.scrcpy_path else None,
                    "scrcpy_version": tools.scrcpy_version,
                    "adb_path": str(tools.adb_path) if tools.adb_path else None,
                    "adb_version": tools.adb_version,
                },
                "devices": [
                    {
                        "serial": d.serial,
                        "display_name": d.display_name,
                        "is_wifi": d.is_wifi,
                        "state": d.state,
                        "android_version": d.android_version,
                        "sdk_version": d.sdk_version,
                    }
                    for d in raw_devs
                ],
                "history": history_list,
            }

        def _fetch_refresh_payload(self, *, refresh_wifi: bool = False) -> dict[str, Any]:
            """Trigger daemon mDNS/Wi-Fi refresh via IPC if running, then return current state."""
            try:
                _ipc_request("android_devices", refresh=refresh_wifi, timeout=6.0)
            except (DaemonNotRunning, IpcError, OSError):
                pass
            return self._fetch_local_payload()

        def _apply_refresh_payload(self, payload: dict[str, Any]) -> None:
            tools = payload.get("tools", {})
            scrcpy_path = tools.get("scrcpy_path")
            if scrcpy_path:
                self.tool_label.setText(
                    f"✓ scrcpy {tools.get('scrcpy_version', '')} ({scrcpy_path})\n"
                    f"✓ adb {tools.get('adb_version', '')} ({tools.get('adb_path', '')})"
                )
                self.tool_label.setStyleSheet("color: #2e7d32;")
                self.install_btn.hide()
            else:
                self.tool_label.setText("scrcpy is not installed on this system.")
                self.tool_label.setStyleSheet("color: #d32f2f;")
                self.install_btn.show()

            self.dev_list.clear()
            self._devices = list(payload.get("devices", []))
            if not self._devices:
                self.dev_list.addItem("No connected devices found")
                self.switch_wifi_btn.setEnabled(False)
            else:
                for d in self._devices:
                    kind = "Wi-Fi" if d.get("is_wifi") else "USB"
                    ver = f"Android {d['android_version']}" if d.get("android_version") else ""
                    item = QtWidgets.QListWidgetItem(
                        f"{d['display_name']} [{kind}] {ver} — {d['state']}"
                    )
                    self.dev_list.addItem(item)
                self.dev_list.setCurrentRow(0)
                self._device_selected(self.dev_list.currentRow())

            self.history_combo.clear()
            for d in payload.get("history", []):
                last_ip = d.get("last_ip", "")
                last_port = d.get("last_port", 5555)
                serial = d.get("serial", "")
                target = f"{last_ip}:{last_port}" if last_ip else serial
                disp = d.get("display_name") or (
                    f"{d.get('manufacturer', '')} {d.get('model', '')}".strip() or serial
                )
                mode = "Wi-Fi" if d.get("is_wifi") else "USB"
                self.history_combo.addItem(f"{disp} [{mode}] ({target})", target)
            self.history_btn.setEnabled(self.history_combo.count() > 0)

        def _refresh_all(self) -> None:
            try:
                payload = self._fetch_local_payload()
                self._apply_refresh_payload(payload)
            except Exception as exc:
                self.tool_label.setText(f"Error checking tools: {exc}")
                self.dev_list.clear()
                self.dev_list.addItem(f"Error listing devices: {exc}")
                self.switch_wifi_btn.setEnabled(False)
                self.history_btn.setEnabled(False)
            self._poll_status()

        def _refresh_all_async(self) -> None:
            def work() -> dict[str, Any]:
                return self._fetch_refresh_payload(refresh_wifi=True)

            def done(res: Any) -> None:
                if isinstance(res, dict):
                    self._apply_refresh_payload(res)
                self._poll_status()

            try:
                future = self._executor.submit(work)
            except RuntimeError:
                self._refresh_all()
                return

            def poll() -> None:
                if not future.done():
                    QtCore.QTimer.singleShot(80, poll)
                else:
                    try:
                        done(future.result())
                    except Exception as exc:
                        self.tool_label.setText(f"Error checking tools: {exc}")

            QtCore.QTimer.singleShot(30, poll)

        def _device_selected(self, row: int) -> None:
            if 0 <= row < len(self._devices):
                dev = self._devices[row]
                self.switch_wifi_btn.setEnabled(
                    not dev.get("is_wifi", False) and dev.get("state") == "device"
                )
                try:
                    sdk = int(dev.get("sdk_version", 0) or 0)
                except (ValueError, TypeError):
                    sdk = 0
                major_v = 0
                ver_str = dev.get("android_version", "")
                if ver_str:
                    m = re.match(r"^(\d+)", str(ver_str).strip())
                    if m:
                        major_v = int(m.group(1))

                if self.as_mic_check.isChecked():
                    idx = self.source_combo.findData("mic")
                    if idx >= 0:
                        self.source_combo.setCurrentIndex(idx)
                elif sdk >= 31 or major_v >= 12:
                    idx = self.source_combo.findData("output")
                    if idx >= 0:
                        self.source_combo.setCurrentIndex(idx)
                elif sdk == 30 or major_v >= 11:
                    idx = self.source_combo.findData("playback")
                    if idx >= 0:
                        self.source_combo.setCurrentIndex(idx)
                elif (0 < sdk < 30) or (0 < major_v < 11):
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
                try:
                    resp = _ipc_request("android_wifi", action="switch", serial=serial, timeout=15.0)
                    return bool(resp.get("ok", False)), str(resp.get("message", ""))
                except (DaemonNotRunning, IpcError, OSError):
                    pass
                ip = self._mgr.get_device_wifi_ip(serial)
                ok, msg = self._mgr.enable_tcpip(serial)
                if not ok:
                    return False, msg
                if ip:
                    import time

                    time.sleep(1.0)
                    conn_ok, conn_msg = self._mgr.connect_wifi(ip)
                    if conn_ok:
                        return True, f"Connected to {ip}:5555"
                    return (
                        True,
                        f"Switched to TCP mode, but Wi-Fi connect to {ip}:5555 failed: {conn_msg}",
                    )
                return True, "Switched to TCP mode on port 5555. Please connect via Wi-Fi IP."

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

        def _reconnect_history(self) -> None:
            target = self.history_combo.currentData()
            if not target:
                return
            ip = target.split(":")[0] if ":" in target else target
            port = (
                int(target.split(":")[1])
                if ":" in target and target.split(":")[1].isdigit()
                else 5555
            )

            def work() -> tuple[bool, str]:
                try:
                    resp = _ipc_request(
                        "android_wifi", action="connect", host=ip, port=port, timeout=10.0
                    )
                    return bool(resp.get("ok", False)), str(resp.get("message", ""))
                except (DaemonNotRunning, IpcError, OSError):
                    return self._mgr.connect_wifi(ip, port)

            def done(res: Any) -> None:
                if isinstance(res, tuple):
                    ok_f, msg = res
                    if ok_f:
                        self._notify(APP_NAME, msg)
                    else:
                        self._notify(APP_NAME, f"Reconnection failed: {msg}", warning=True)
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
                try:
                    resp = _ipc_request("android_wifi", action="connect", host=ip, timeout=10.0)
                    return bool(resp.get("ok", False)), str(resp.get("message", ""))
                except (DaemonNotRunning, IpcError, OSError):
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

        def stop_capture(self) -> dict[str, Any]:
            active_pid = getattr(self, "_active_pid", 0)
            if self._session is not None:
                try:
                    if getattr(self._session, "_proc", None):
                        active_pid = self._session._proc.pid
                    self._session.stop()
                except Exception:
                    pass
                self._session = None
            res: dict[str, Any] = {}
            try:
                res = _ipc_request("android_stop", timeout=4.0)
            except Exception:
                if active_pid > 0:
                    try:
                        os.kill(active_pid, signal.SIGTERM)
                    except OSError:
                        pass
            self._is_capturing = False
            self._active_pid = 0
            return res

        def _on_as_mic_toggled(self, checked: bool) -> None:
            if checked:
                idx = self.source_combo.findData("mic")
                if idx >= 0:
                    self.source_combo.setCurrentIndex(idx)

        def _toggle_capture(self) -> None:
            if self._is_capturing:

                def work() -> dict[str, Any]:
                    return self.stop_capture()

                def done(_: Any) -> None:
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
                    self._notify(APP_NAME, "Please select an Android device first.", warning=True)
                    return
                dev = self._devices[row]
                serial = dev["serial"]
                codec = self.codec_combo.currentData()
                source = self.source_combo.currentData()
                buffer_ms = int(self.buffer_spin.value())
                dup = self.dup_check.isChecked()
                as_mic = self.as_mic_check.isChecked()

                def work() -> dict[str, Any]:
                    try:
                        return _ipc_request(
                            "android_start",
                            timeout=10.0,
                            serial=serial,
                            name=dev["display_name"],
                            codec=codec,
                            source=source,
                            as_mic=as_mic,
                            buffer_ms=buffer_ms,
                            duplicate=dup,
                        )
                    except (DaemonNotRunning, IpcError, OSError):
                        from ..android import AndroidCaptureSession
                        from ..backend import create_backend

                        self._session = AndroidCaptureSession(self._mgr, backend=create_backend())
                        return self._session.start(
                            serial,
                            device_name=dev["display_name"],
                            codec=codec,
                            source=source,
                            as_mic=as_mic,
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
                            self._notify(APP_NAME, f"Capture failed: {exc}", warning=True)

                QtCore.QTimer.singleShot(50, poll)

        def _poll_status(self) -> None:
            def work() -> dict[str, Any]:
                if self._session is not None and self._session.is_active:
                    return {"session": self._session.status()}
                try:
                    return _ipc_request("android_status", timeout=2.0)
                except Exception:
                    from ..util import process_running

                    return {"session": {"active": process_running("scrcpy")}}

            def done(res: Any) -> None:
                if not isinstance(res, dict):
                    return
                session = res.get("session", {})
                active = bool(session.get("active"))
                self._is_capturing = active
                self._active_pid = int(session.get("pid", 0) or 0)
                if active:
                    name = session.get("device_name") or session.get("serial", "Android")
                    codec = str(session.get("codec", "")).upper()
                    uptime = format_duration(session.get("uptime_s"))
                    self.status_lbl.setText(f"Status: Streaming from {name} ({codec}, {uptime})")
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

            try:
                future = self._executor.submit(work)
            except RuntimeError:
                return

            def poll() -> None:
                if not future.done():
                    QtCore.QTimer.singleShot(100, poll)
                else:
                    done(future.result())

            QtCore.QTimer.singleShot(30, poll)

    return AndroidWizardDialog
