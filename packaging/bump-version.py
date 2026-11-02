#!/usr/bin/env python3
"""Automated single-command version bumper for SoundHop.

Updates:
- src/soundhop/constants.py (Single source of truth for Python, CLI, GUI, and pyproject.toml)
- packaging/PKGBUILD
- packaging/soundhop.spec
- README.md (AppImage artifact mentions)

Usage:
  ./packaging/bump-version.py 1.2.0
  ./packaging/bump-version.py --patch
  ./packaging/bump-version.py --minor
  ./packaging/bump-version.py --major
"""

import argparse
import re
import subprocess
import sys
from pathlib import Path

ROOT_DIR = Path(__file__).resolve().parent.parent


def get_current_version() -> str:
    const_file = ROOT_DIR / "src" / "soundhop" / "constants.py"
    text = const_file.read_text(encoding="utf-8")
    m = re.search(r'VERSION\s*=\s*["\']([^"\']+)["\']', text)
    if not m:
        raise RuntimeError(f"Could not find VERSION in {const_file}")
    return m.group(1)


def parse_semver(ver: str) -> tuple[int, int, int]:
    parts = ver.split(".")
    if len(parts) != 3 or not all(p.isdigit() for p in parts):
        raise ValueError(f"Version must be in semver format X.Y.Z, got: {ver}")
    return int(parts[0]), int(parts[1]), int(parts[2])


def bump(current: str, mode: str) -> str:
    major, minor, patch = parse_semver(current)
    if mode == "major":
        return f"{major + 1}.0.0"
    if mode == "minor":
        return f"{major}.{minor + 1}.0"
    if mode == "patch":
        return f"{major}.{minor}.{patch + 1}"
    return mode  # explicit version


def update_file(path: Path, pattern: str, replacement: str) -> bool:
    if not path.exists():
        return False
    content = path.read_text(encoding="utf-8")
    new_content, count = re.subn(pattern, replacement, content)
    if count > 0 and new_content != content:
        path.write_text(new_content, encoding="utf-8")
        return True
    return False


def main() -> int:
    parser = argparse.ArgumentParser(description="Bump SoundHop project version.")
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument("version", nargs="?", help="New version (e.g. 1.2.0)")
    group.add_argument("--patch", action="store_true", help="Bump patch version (X.Y.Z+1)")
    group.add_argument("--minor", action="store_true", help="Bump minor version (X.Y+1.0)")
    group.add_argument("--major", action="store_true", help="Bump major version (X+1.0.0)")
    parser.add_argument("--no-test", action="store_true", help="Skip running unit tests")

    args = parser.parse_args()

    curr_ver = get_current_version()

    if args.patch:
        new_ver = bump(curr_ver, "patch")
    elif args.minor:
        new_ver = bump(curr_ver, "minor")
    elif args.major:
        new_ver = bump(curr_ver, "major")
    else:
        new_ver = args.version.lstrip("v")
        parse_semver(new_ver)

    if curr_ver == new_ver:
        print(f"Current version is already {curr_ver}")
        return 0

    print(f"Bumping SoundHop version: {curr_ver} → {new_ver}\n")

    changes: list[str] = []

    # 1. src/soundhop/constants.py
    const_path = ROOT_DIR / "src" / "soundhop" / "constants.py"
    if update_file(const_path, r'VERSION\s*=\s*["\'][^"\']+["\']', f'VERSION = "{new_ver}"'):
        changes.append(f"✓ {const_path.relative_to(ROOT_DIR)} (Python source of truth)")

    # 2. packaging/PKGBUILD
    pkg_path = ROOT_DIR / "packaging" / "PKGBUILD"
    if update_file(pkg_path, r'pkgver=[0-9.]+', f'pkgver={new_ver}'):
        changes.append(f"✓ {pkg_path.relative_to(ROOT_DIR)} (Arch Linux package)")

    # 3. packaging/soundhop.spec
    spec_path = ROOT_DIR / "packaging" / "soundhop.spec"
    if update_file(spec_path, r'Version:\s+[0-9.]+', f'Version:        {new_ver}'):
        changes.append(f"✓ {spec_path.relative_to(ROOT_DIR)} (RPM spec file)")

    # 4. README.md
    readme_path = ROOT_DIR / "README.md"
    if update_file(
        readme_path,
        r'SoundHop-[0-9.]+-universal-x86_64\.AppImage',
        f'SoundHop-{new_ver}-universal-x86_64.AppImage',
    ):
        changes.append(f"✓ {readme_path.relative_to(ROOT_DIR)} (documentation)")

    for c in changes:
        print(c)

    print("\nNote: pyproject.toml, build-appimage.sh, and container-build.sh read VERSION dynamically!")

    # Verify with unit tests
    if not args.no_test:
        print("\nRunning unit tests to verify version bump...")
        res = subprocess.run(
            [sys.executable, "-m", "unittest", "discover", "tests"],
            cwd=ROOT_DIR,
            env={"PYTHONPATH": "src"},
        )
        if res.returncode != 0:
            print("\n❌ Tests failed after version bump!")
            return res.returncode
        print("✓ All tests passed!")

    print(f"\nSuccessfully updated version to {new_ver}.")
    print(f"Git commit: git commit -am 'chore: bump version to v{new_ver}'")
    return 0


if __name__ == "__main__":
    sys.exit(main())
