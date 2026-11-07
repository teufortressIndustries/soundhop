"""Settings dialog for the SoundHop Qt system tray."""

from __future__ import annotations

from typing import Any

from ..config import Config
from ..constants import APP_NAME


def build_settings_dialog_class(QtCore: Any, QtGui: Any, QtWidgets: Any) -> type:
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
            self.share_enabled = QtWidgets.QCheckBox("Share this device's speakers")
            self.share_enabled.setChecked(config.sharing.enabled)
            self.auto = QtWidgets.QCheckBox("Auto-connect to the last device")
            self.auto.setChecked(config.connection.auto_connect)
            self.switch = QtWidgets.QCheckBox("Switch the default output automatically")
            self.switch.setChecked(config.connection.switch_default)
            self.restore = QtWidgets.QCheckBox("Restore local audio on disconnect")
            self.restore.setChecked(config.connection.restore_default)
            self.move = QtWidgets.QCheckBox("Move currently playing audio")
            self.move.setChecked(config.connection.move_streams)
            self.disconnect_quit = QtWidgets.QCheckBox("Disconnect remote audio when quitting")
            self.disconnect_quit.setChecked(getattr(config.connection, "disconnect_on_quit", True))

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

    return SettingsDialog
