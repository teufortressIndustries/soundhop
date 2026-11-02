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
        with patch.dict(os.environ, {"APPIMAGE": "/home/user/Applications/SoundHop-x86_64.AppImage"}):
            unit = unit_text()
            self.assertIn('ExecStart=/home/user/Applications/SoundHop-x86_64.AppImage daemon', unit)
            autostart = autostart_text()
            self.assertIn('Exec=/home/user/Applications/SoundHop-x86_64.AppImage tray', autostart)


if __name__ == "__main__":
    unittest.main()
