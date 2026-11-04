#!/usr/bin/env python3
"""Build a standalone single-file Windows executable (SoundHop.exe) using PyInstaller."""

import os
import subprocess
import sys
from pathlib import Path

ROOT_DIR = Path(__file__).resolve().parent.parent
SRC_DIR = ROOT_DIR / "src"
ASSETS_DIR = SRC_DIR / "soundhop" / "assets"
DIST_DIR = ROOT_DIR / "dist"


def main() -> int:
    print("=== Building SoundHop Windows Standalone Executable ===")

    # Check for pyinstaller
    try:
        import PyInstaller
    except ImportError:
        print("PyInstaller is not installed. Installing pyinstaller...")
        subprocess.check_call([sys.executable, "-m", "pip", "install", "pyinstaller"])

    # Ensure required dependencies for full Windows standalone bundle are installed
    try:
        import PySide6
        import sounddevice
    except ImportError:
        print("Required dependencies not found. Installing '.[all]'...")
        try:
            subprocess.check_call(
                [sys.executable, "-m", "pip", "install", "-e", ".[all]"], cwd=ROOT_DIR
            )
        except subprocess.CalledProcessError as exc:
            print(f"Warning: pip install '.[all]' failed: {exc}")

    # Gather available hidden imports
    hidden_imports = [
        "soundhop",
        "soundhop.platform.windows",
        "soundhop.platform.windows.audio",
        "soundhop.platform.windows.backend",
        "soundhop.platform.windows.protocol",
        "soundhop.cli",
        "soundhop.gui",
    ]
    for opt_mod in (
        "sounddevice",
        "soundcard",
        "numpy",
        "psutil",
        "zeroconf",
        "PySide6",
        "PySide6.QtCore",
        "PySide6.QtGui",
        "PySide6.QtWidgets",
    ):
        try:
            __import__(opt_mod)
            hidden_imports.append(opt_mod)
        except ImportError:
            pass

    cmd = [
        sys.executable,
        "-m",
        "PyInstaller",
        "--name=SoundHop",
        "--onefile",
        "--noconsole",
        "--clean",
        f"--paths={SRC_DIR}",
        f"--add-data={ASSETS_DIR}{os.pathsep}soundhop/assets",
        *[f"--hidden-import={m}" for m in hidden_imports],
        str(SRC_DIR / "soundhop" / "__main__.py"),
    ]

    print("Running:", " ".join(cmd))
    res = subprocess.run(cmd, cwd=ROOT_DIR)
    if res.returncode == 0:
        exe_path = DIST_DIR / "SoundHop.exe"
        print(f"\n[OK] Successfully built standalone executable: {exe_path}")
    else:
        print(f"\n[FAIL] Build failed with exit code {res.returncode}")
    return res.returncode


if __name__ == "__main__":
    sys.exit(main())
