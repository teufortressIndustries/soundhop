"""Unit tests for soundhop.gui AndroidWizardDialog."""

import os
import unittest
from concurrent.futures import ThreadPoolExecutor
from unittest.mock import MagicMock, patch

try:
    from PySide6 import QtCore, QtGui, QtWidgets

    HAVE_QT = True
except ImportError:
    HAVE_QT = False


@unittest.skipUnless(HAVE_QT, "PySide6 not available")
class GuiAndroidWizardTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        os.environ["QT_QPA_PLATFORM"] = "offscreen"
        cls.app = QtWidgets.QApplication.instance()
        if cls.app is None:
            cls.app = QtWidgets.QApplication(["-platform", "offscreen"])

    def setUp(self) -> None:
        from soundhop.gui import _build_classes

        Tray, _ = _build_classes(QtCore, QtGui, QtWidgets)
        self.AndroidWizardDialog = Tray.AndroidWizardDialog
        self.executor = ThreadPoolExecutor(max_workers=1)
        self.notifications: list[str] = []

    def tearDown(self) -> None:
        self.executor.shutdown(wait=False)

    def _notify(self, title: str, msg: str, warning: bool = False) -> None:
        self.notifications.append(f"{title}: {msg}")

    def test_switch_wifi_button_enabled_for_usb_device(self) -> None:
        wizard = self.AndroidWizardDialog(None, self.executor, self._notify)

        # Populate devices: 1 USB device (ready), 1 Wi-Fi device, 1 unauthorized
        wizard._devices = [
            {
                "serial": "USB_READY_1",
                "display_name": "Pixel 7",
                "is_wifi": False,
                "state": "device",
                "android_version": "14",
                "sdk_version": 34,
            },
            {
                "serial": "192.168.1.50:5555",
                "display_name": "Galaxy S23",
                "is_wifi": True,
                "state": "device",
                "android_version": "14",
                "sdk_version": 34,
            },
            {
                "serial": "USB_UNAUTH_2",
                "display_name": "Old Phone",
                "is_wifi": False,
                "state": "unauthorized",
                "android_version": "",
                "sdk_version": 0,
            },
        ]

        # 1. Selecting ready USB device enables Switch to Wi-Fi button
        wizard._device_selected(0)
        self.assertTrue(wizard.switch_wifi_btn.isEnabled())

        # 2. Selecting Wi-Fi device disables Switch to Wi-Fi button (already wifi)
        wizard._device_selected(1)
        self.assertFalse(wizard.switch_wifi_btn.isEnabled())

        # 3. Selecting unauthorized USB device disables Switch to Wi-Fi button
        wizard._device_selected(2)
        self.assertFalse(wizard.switch_wifi_btn.isEnabled())

        # 4. Out of bounds index disables button
        wizard._device_selected(-1)
        self.assertFalse(wizard.switch_wifi_btn.isEnabled())
        wizard._device_selected(99)
        self.assertFalse(wizard.switch_wifi_btn.isEnabled())

    def test_switch_wifi_button_when_devices_empty(self) -> None:
        wizard = self.AndroidWizardDialog(None, self.executor, self._notify)
        wizard._devices = []
        wizard._device_selected(-1)
        self.assertFalse(wizard.switch_wifi_btn.isEnabled())

    def test_refresh_all_updates_button_and_selection(self) -> None:
        from soundhop.android import AdbDevice

        wizard = self.AndroidWizardDialog(None, self.executor, self._notify)
        mock_devices = [
            AdbDevice(
                serial="USB_DEV_1",
                state="device",
                model="Pixel",
                manufacturer="Google",
                is_wifi=False,
            )
        ]
        with patch.object(wizard._mgr, "list_devices", return_value=mock_devices):
            wizard._refresh_all()
            self.assertEqual(len(wizard._devices), 1)
            self.assertEqual(wizard.dev_list.count(), 1)
            self.assertTrue(wizard.switch_wifi_btn.isEnabled())

    def test_switch_wifi_action_daemon_ipc(self) -> None:

        wizard = self.AndroidWizardDialog(None, self.executor, self._notify)
        wizard._devices = [
            {
                "serial": "USB_DEV_1",
                "display_name": "Google Pixel",
                "is_wifi": False,
                "state": "device",
            }
        ]
        wizard.dev_list.addItem("Google Pixel [USB] — device")
        wizard.dev_list.setCurrentRow(0)

        with patch("soundhop.gui.request") as mock_request:
            mock_request.return_value = {"ok": True, "message": "Connected to 192.168.1.55:5555"}
            with patch.object(wizard, "_refresh_all"):
                wizard._switch_wifi()
                # Run queued tasks in thread pool
                self.executor.shutdown(wait=True)
                mock_request.assert_called_with(
                    "android_wifi", action="switch", serial="USB_DEV_1", timeout=15.0
                )
                self.assertFalse(wizard.switch_wifi_btn.isEnabled())

    def test_disconnect_on_quit_enabled(self) -> None:
        from pathlib import Path

        from soundhop.config import Config
        from soundhop.gui import _build_classes

        Tray, _ = _build_classes(QtCore, QtGui, QtWidgets)
        with patch.object(Tray, "_kickstart"):
            tray = Tray()

        cfg = Config(Path("/tmp/test.toml"))
        cfg.connection.disconnect_on_quit = True

        mock_dialog = MagicMock()
        tray._android_dialog = mock_dialog
        tray._status = {"android": {"active": True, "pid": 0}}

        with (
            patch("soundhop.gui.Config.load", return_value=cfg),
            patch("soundhop.gui.request") as mock_request,
        ):
            tray._disconnect_on_quit_if_enabled()

            mock_request.assert_any_call("disconnect", timeout=4.0, restore=True, stop_android=True)
            mock_request.assert_any_call("android_stop", timeout=3.0)
            mock_dialog.stop_capture.assert_called_once()

        tray.shutdown()

    def test_disconnect_on_quit_disabled(self) -> None:
        from pathlib import Path

        from soundhop.config import Config
        from soundhop.gui import _build_classes

        Tray, _ = _build_classes(QtCore, QtGui, QtWidgets)
        with patch.object(Tray, "_kickstart"):
            tray = Tray()

        cfg = Config(Path("/tmp/test.toml"))
        cfg.connection.disconnect_on_quit = False

        mock_dialog = MagicMock()
        tray._android_dialog = mock_dialog

        with (
            patch("soundhop.gui.Config.load", return_value=cfg),
            patch("soundhop.gui.request") as mock_request,
        ):
            tray._disconnect_on_quit_if_enabled()

            mock_request.assert_not_called()
            mock_dialog.stop_capture.assert_not_called()

        tray.shutdown()


if __name__ == "__main__":
    unittest.main()
