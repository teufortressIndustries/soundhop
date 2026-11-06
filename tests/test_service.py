"""Tests for systemd and autostart text generation."""

import unittest

from soundhop.service import autostart_text, unit_text


class ServiceTest(unittest.TestCase):
    def test_unit_text_structure(self) -> None:
        text = unit_text()
        self.assertIn("[Unit]", text)
        self.assertIn("[Service]", text)
        self.assertIn("[Install]", text)
        self.assertIn("ExecStart=", text)
        self.assertIn("-m soundhop daemon", text)
        self.assertIn("pipewire-pulse.service", text)

    def test_autostart_text_structure(self) -> None:
        text = autostart_text()
        self.assertIn("[Desktop Entry]", text)
        self.assertIn("Type=Application", text)
        self.assertIn("-m soundhop tray", text)
        self.assertIn("Icon=soundhop", text)

    def test_appimage_text_structure(self) -> None:
        import os
        from unittest.mock import patch

        with patch.dict(
            os.environ, {"APPIMAGE": "/home/user/Applications/SoundHop-x86_64.AppImage"}
        ):
            unit = unit_text()
            self.assertIn("ExecStart=/home/user/Applications/SoundHop-x86_64.AppImage daemon", unit)
            autostart = autostart_text()
            self.assertIn("Exec=/home/user/Applications/SoundHop-x86_64.AppImage tray", autostart)

    def test_windows_autostart_registry_and_fallback(self) -> None:
        import sys
        from unittest.mock import MagicMock, patch

        from soundhop.service import (
            install,
            install_tray_autostart,
            remove_tray_autostart,
            service_status,
            uninstall,
            windows_autostart_command,
        )

        with patch.object(sys, "platform", "win32"):
            # 1. Registry path succeeds
            fake_reg_store = {}

            def fake_write(name="SoundHop", command=None):
                fake_reg_store[name] = command or windows_autostart_command()
                return True

            def fake_query(name="SoundHop"):
                return fake_reg_store.get(name)

            def fake_delete(name="SoundHop"):
                return fake_reg_store.pop(name, None) is not None

            with (
                patch("soundhop.service.winreg_write_run", side_effect=fake_write),
                patch("soundhop.service.winreg_query_run", side_effect=fake_query),
                patch("soundhop.service.winreg_delete_run", side_effect=fake_delete),
                patch("soundhop.service.windows_startup_path") as mock_vbs_path,
            ):
                mock_vbs = MagicMock()
                mock_vbs.exists.return_value = False
                mock_vbs_path.return_value = mock_vbs

                # Install tray
                inst_res = install(tray=True)
                self.assertTrue(inst_res["enabled"])
                self.assertIn("Registry", inst_res["unit"])
                self.assertIn("SoundHop", fake_reg_store)

                # Status
                st = service_status()
                self.assertTrue(st["installed"])
                self.assertTrue(st["running"])

                # Uninstall
                uninst_res = uninstall(remove_tray=True)
                self.assertTrue(uninst_res["removed"])
                self.assertNotIn("SoundHop", fake_reg_store)

            # 2. Registry fails, falls back to startup folder
            with (
                patch("soundhop.service.winreg_write_run", return_value=False),
                patch("soundhop.service.winreg_query_run", return_value=None),
                patch("soundhop.service.winreg_delete_run", return_value=False),
                patch("soundhop.service.windows_startup_path") as mock_vbs_path,
                patch("soundhop.service.windows_startup_dir"),
            ):
                mock_vbs = MagicMock()
                mock_vbs.exists.return_value = True
                mock_vbs_path.return_value = mock_vbs

                self.assertTrue(install_tray_autostart())
                mock_vbs.write_text.assert_called_once()

                # Status detects startup folder script
                st = service_status()
                self.assertTrue(st["installed"])

                # Remove cleans up startup folder script
                self.assertTrue(remove_tray_autostart())
                mock_vbs.unlink.assert_called_once()

    def test_windows_service_install_defaults(self) -> None:
        import sys
        from unittest.mock import patch

        from soundhop.service import install

        with patch.object(sys, "platform", "win32"):
            with (
                patch("soundhop.service.install_tray_autostart", return_value=True) as mock_inst,
                patch("soundhop.service.winreg_query_run", return_value="soundhop.exe tray"),
            ):
                res = install()  # tray defaults to False, enable defaults to True
                self.assertTrue(res["enabled"])
                mock_inst.assert_called_once()

    def test_windows_notification_escaping(self) -> None:
        import sys
        from unittest.mock import patch

        from soundhop.util import notify

        with patch.object(sys, "platform", "win32"):
            with patch("subprocess.run") as mock_run:
                notify("SoundHop & Co <Test>", "Audio 'playing' & streaming > 100%")
                self.assertTrue(mock_run.called)
                cmd = mock_run.call_args[0][0][-1]
                # XML special chars must be escaped in XML string
                self.assertIn("SoundHop &amp; Co &lt;Test&gt;", cmd)
                self.assertIn("Audio ''playing'' &amp; streaming &gt; 100%", cmd)


if __name__ == "__main__":
    unittest.main()
