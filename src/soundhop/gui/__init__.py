"""Optional Qt system tray package.

PySide6 is intentionally an extra: the daemon and CLI work without it, and
distributions can package Qt separately.  The tray is a thin client of the
same local control socket, so it cannot get out of sync with the daemon.
"""

from __future__ import annotations

import logging
import sys
from typing import Any

from ..config import Config
from ..constants import APP_NAME
from ..ipc import DaemonNotRunning, IpcError, request
from ..service import tray_dependency_hint
from .android_dialog import build_android_wizard_class
from .settings import build_settings_dialog_class
from .tray import build_tray_class

__all__ = [
    "Config",
    "DaemonNotRunning",
    "IpcError",
    "_build_classes",
    "_import_qt",
    "request",
    "run_tray",
]

log = logging.getLogger(__name__)


def _import_qt() -> tuple[Any, Any, Any]:
    try:
        from PySide6 import QtCore, QtGui, QtWidgets
    except ModuleNotFoundError as exc:  # pragma: no cover - depends on extra
        raise ImportError("PySide6 is not installed. " + tray_dependency_hint()) from exc
    return QtCore, QtGui, QtWidgets


def _build_classes(QtCore: Any, QtGui: Any, QtWidgets: Any) -> tuple[type, type]:
    SettingsDialog = build_settings_dialog_class(QtCore, QtGui, QtWidgets)
    AndroidWizardDialog = build_android_wizard_class(QtCore, QtGui, QtWidgets)
    Tray = build_tray_class(
        QtCore,
        QtGui,
        QtWidgets,
        SettingsDialog=SettingsDialog,
        AndroidWizardDialog=AndroidWizardDialog,
    )
    Tray.AndroidWizardDialog = AndroidWizardDialog  # type: ignore[attr-defined]
    return Tray, SettingsDialog


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
