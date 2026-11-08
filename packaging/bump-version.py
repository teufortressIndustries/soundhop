#!/usr/bin/env python3
"""Automated single-command version bumper for SoundHop v2.0 (Rust workspace).

Updates:
- Cargo.toml (version = "X.Y.Z" in [workspace.package])
- crates/soundhop-core/src/constants.rs (pub const VERSION: &str = "X.Y.Z";)
- packaging/PKGBUILD (pkgver=X.Y.Z)
- packaging/soundhop.spec (Version: X.Y.Z)
- README.md (AppImage artifact mentions)
- packaging/io.github.soundhop.desktop (X-AppImage-Version)
- packaging/io.github.soundhop.metainfo.xml (<release version="X.Y.Z">)
"""

import argparse
import re
import subprocess
import sys
from pathlib import Path


def _find_repo_root() -> Path:
    current = Path(__file__).resolve().parent
    for candidate in [current, current.parent]:
        if (candidate / "Cargo.toml").is_file():
            return candidate
    return current.parent


ROOT_DIR = _find_repo_root()


def get_current_version() -> str:
    cargo_file = ROOT_DIR / "Cargo.toml"
    if cargo_file.is_file():
        text = cargo_file.read_text(encoding="utf-8")
        m = re.search(r'^version\s*=\s*"([^"]+)"', text, flags=re.MULTILINE)
        if m:
            return m.group(1)
    const_file = ROOT_DIR / "crates" / "soundhop-core" / "src" / "constants.rs"
    text = const_file.read_text(encoding="utf-8")
    m = re.search(r'pub const VERSION:\s*&str\s*=\s*"([^"]+)"', text)
    if not m:
        raise RuntimeError(f"Could not find VERSION in {cargo_file} or {const_file}")
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
    return mode


def update_file(path: Path, pattern: str, replacement: str, flags: int = 0) -> bool:
    if not path.exists():
        return False
    content = path.read_text(encoding="utf-8")
    new_content, count = re.subn(pattern, replacement, content, flags=flags)
    if count > 0 and new_content != content:
        path.write_text(new_content, encoding="utf-8")
        return True
    return False


def main() -> int:
    parser = argparse.ArgumentParser(description="Bump SoundHop project version.")
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument("version", nargs="?", help="New version (e.g. 2.1.0)")
    group.add_argument("--patch", action="store_true", help="Bump patch version")
    group.add_argument("--minor", action="store_true", help="Bump minor version")
    group.add_argument("--major", action="store_true", help="Bump major version")
    parser.add_argument("--no-test", action="store_true", help="Skip running cargo test")

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

    print(f"Bumping SoundHop version: {curr_ver} -> {new_ver}\n")
    changes: list[str] = []

    cargo_path = ROOT_DIR / "Cargo.toml"
    if update_file(
        cargo_path,
        r'^version\s*=\s*"[0-9.]+"',
        f'version = "{new_ver}"',
        flags=re.MULTILINE,
    ):
        changes.append("✓ Cargo.toml")

    const_path = ROOT_DIR / "crates" / "soundhop-core" / "src" / "constants.rs"
    if update_file(
        const_path,
        r'pub const VERSION:\s*&str\s*=\s*"[^"]+"',
        f'pub const VERSION: &str = "{new_ver}"',
    ):
        changes.append("✓ crates/soundhop-core/src/constants.rs")

    pkg_path = ROOT_DIR / "packaging" / "PKGBUILD"
    if update_file(pkg_path, r'pkgver=[0-9.]+', f'pkgver={new_ver}'):
        changes.append("✓ packaging/PKGBUILD")

    spec_path = ROOT_DIR / "packaging" / "soundhop.spec"
    if update_file(spec_path, r'Version:\s+[0-9.]+', f'Version:        {new_ver}'):
        changes.append("✓ packaging/soundhop.spec")

    readme_path = ROOT_DIR / "README.md"
    if update_file(
        readme_path,
        r'SoundHop-[0-9.]+-universal-x86_64\.AppImage',
        f'SoundHop-{new_ver}-universal-x86_64.AppImage',
    ):
        changes.append("✓ README.md")

    desktop_path = ROOT_DIR / "packaging" / "io.github.soundhop.desktop"
    if update_file(
        desktop_path,
        r'X-AppImage-Version=[^\r\n]*',
        f'X-AppImage-Version={new_ver}',
    ):
        changes.append("✓ packaging/io.github.soundhop.desktop")

    metainfo_path = ROOT_DIR / "packaging" / "io.github.soundhop.metainfo.xml"
    if update_file(
        metainfo_path,
        r'<release version="[^"]+"',
        f'<release version="{new_ver}"',
    ):
        changes.append("✓ packaging/io.github.soundhop.metainfo.xml")

    for c in changes:
        print(c)

    if not args.no_test:
        print("\nRunning cargo test --workspace to verify version bump...")
        res = subprocess.run(["cargo", "test", "--workspace"], cwd=ROOT_DIR)
        if res.returncode != 0:
            return res.returncode

    return 0


if __name__ == "__main__":
    sys.exit(main())
