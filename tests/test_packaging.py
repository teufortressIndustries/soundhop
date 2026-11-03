"""Tests for AppImage packaging, desktop entries, and version bumping."""

import importlib.util
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT_DIR = Path(__file__).resolve().parent.parent


class PackagingTest(unittest.TestCase):
    def test_desktop_entry_appimage_metadata(self) -> None:
        desktop_file = ROOT_DIR / "packaging" / "io.github.soundhop.desktop"
        self.assertTrue(desktop_file.exists(), "io.github.soundhop.desktop must exist")

        content = desktop_file.read_text(encoding="utf-8")
        self.assertIn("[Desktop Entry]", content)
        from soundhop.constants import VERSION
        self.assertEqual(VERSION, "1.1.2")
        self.assertIn("X-AppImage-Version=1.1.2", content)
        self.assertIn(f"X-AppImage-Version={VERSION}", content)
        self.assertIn(
            "X-AppImage-UpdateURL=https://github.com/teufortressIndustries/soundhop",
            content,
        )
        self.assertIn(
            "X-AppImage-Homepage=https://github.com/teufortressIndustries/soundhop",
            content,
        )

    def test_version_consistency_across_packaging(self) -> None:
        from soundhop.constants import VERSION
        self.assertEqual(VERSION, "1.1.2")

        pkgbuild = (ROOT_DIR / "packaging" / "PKGBUILD").read_text(encoding="utf-8")
        self.assertIn(f"pkgver={VERSION}", pkgbuild)

        spec_file = (ROOT_DIR / "packaging" / "soundhop.spec").read_text(encoding="utf-8")
        self.assertIn(f"Version:        {VERSION}", spec_file)

        readme = (ROOT_DIR / "README.md").read_text(encoding="utf-8")
        self.assertIn(f"SoundHop-{VERSION}-universal-x86_64.AppImage", readme)

        pyproject = (ROOT_DIR / "pyproject.toml").read_text(encoding="utf-8")
        self.assertIn(f'version = "{VERSION}"', pyproject)

        desktop = (ROOT_DIR / "packaging" / "io.github.soundhop.desktop").read_text(encoding="utf-8")
        self.assertIn(f"X-AppImage-Version={VERSION}", desktop)
        self.assertNotIn("0.2.1", desktop)

    def test_bump_version_updates_pyproject_toml(self) -> None:
        import re
        bump_script = ROOT_DIR / "packaging" / "bump-version.py"
        spec = importlib.util.spec_from_file_location("bump_version", bump_script)
        self.assertIsNotNone(spec)
        self.assertIsNotNone(spec.loader)
        bump_module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(bump_module)

        with tempfile.TemporaryDirectory() as tmpdir:
            tmp_pyproject = Path(tmpdir) / "pyproject.toml"
            tmp_pyproject.write_text(
                '[project]\nname = "soundhop"\nversion = "1.0.0"\n',
                encoding="utf-8",
            )
            updated = bump_module.update_file(
                tmp_pyproject,
                r'^version\s*=\s*["\'][0-9.]+["\']',
                'version = "1.1.2"',
                flags=re.MULTILINE,
            )
            self.assertTrue(updated)
            content = tmp_pyproject.read_text(encoding="utf-8")
            self.assertIn('version = "1.1.2"', content)
            self.assertNotIn("1.0.0", content)

    def test_bump_version_updates_desktop_entry(self) -> None:
        bump_script = ROOT_DIR / "packaging" / "bump-version.py"
        spec = importlib.util.spec_from_file_location("bump_version", bump_script)
        self.assertIsNotNone(spec)
        self.assertIsNotNone(spec.loader)
        bump_module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(bump_module)

        with tempfile.TemporaryDirectory() as tmpdir:
            tmp_desktop = Path(tmpdir) / "io.github.soundhop.desktop"

            # Case 1: Existing X-AppImage-Version
            tmp_desktop.write_text(
                "[Desktop Entry]\nType=Application\nX-AppImage-Version=0.2.1\n",
                encoding="utf-8",
            )
            updated = bump_module.update_file(
                tmp_desktop,
                r"X-AppImage-Version=[^\r\n]*",
                "X-AppImage-Version=1.2.3",
            )
            self.assertTrue(updated)
            content = tmp_desktop.read_text(encoding="utf-8")
            self.assertIn("X-AppImage-Version=1.2.3", content)
            self.assertNotIn("0.2.1", content)

            # Case 2: Empty X-AppImage-Version=
            tmp_desktop.write_text(
                "[Desktop Entry]\nType=Application\nX-AppImage-Version=\n",
                encoding="utf-8",
            )
            updated = bump_module.update_file(
                tmp_desktop,
                r"X-AppImage-Version=[^\r\n]*",
                "X-AppImage-Version=1.2.3",
            )
            self.assertTrue(updated)
            content = tmp_desktop.read_text(encoding="utf-8")
            self.assertIn("X-AppImage-Version=1.2.3", content)

            # Case 3: Missing X-AppImage-Version (append logic)
            tmp_desktop.write_text(
                "[Desktop Entry]\nType=Application\n",
                encoding="utf-8",
            )
            content = tmp_desktop.read_text(encoding="utf-8")
            import re
            if not re.search(r"X-AppImage-Version=[^\r\n]*", content):
                tmp_desktop.write_text(
                    content.rstrip() + "\nX-AppImage-Version=1.2.3\n",
                    encoding="utf-8",
                )
            content = tmp_desktop.read_text(encoding="utf-8")
            self.assertIn("X-AppImage-Version=1.2.3", content)

    def test_update_info_format_bash(self) -> None:
        cmd = [
            "bash",
            "-c",
            'UPDATE_REPO="${UPDATE_REPO:-teufortressIndustries/soundhop}"; '
            'echo "gh-releases-zsync|${UPDATE_REPO%/*}|${UPDATE_REPO#*/}|latest|SoundHop-*x86_64.AppImage.zsync"',
        ]
        res = subprocess.run(cmd, capture_output=True, text=True, check=True)
        self.assertEqual(
            res.stdout.strip(),
            "gh-releases-zsync|teufortressIndustries|soundhop|latest|SoundHop-*x86_64.AppImage.zsync",
        )

        # Custom repo override test
        res_custom = subprocess.run(
            cmd,
            env={"UPDATE_REPO": "custom-owner/custom-repo"},
            capture_output=True,
            text=True,
            check=True,
        )
        self.assertEqual(
            res_custom.stdout.strip(),
            "gh-releases-zsync|custom-owner|custom-repo|latest|SoundHop-*x86_64.AppImage.zsync",
        )

    def test_appdir_desktop_version_replacement_bash(self) -> None:
        with tempfile.TemporaryDirectory() as tmpdir:
            app_dir = Path(tmpdir)
            apps_dir = app_dir / "usr" / "share" / "applications"
            apps_dir.mkdir(parents=True)

            dfile1 = app_dir / "io.github.soundhop.desktop"
            dfile2 = apps_dir / "io.github.soundhop.desktop"
            dfile1.write_text("[Desktop Entry]\nX-AppImage-Version=0.2.1\n", encoding="utf-8")
            dfile2.write_text("[Desktop Entry]\nName=SoundHop\n", encoding="utf-8")

            script = f"""
VERSION="2.0.0"
APP_DIR="{app_dir}"
for dfile in "$APP_DIR/io.github.soundhop.desktop" "$APP_DIR/usr/share/applications/io.github.soundhop.desktop"; do
    if [ -f "$dfile" ]; then
        if grep -q '^X-AppImage-Version=' "$dfile"; then
            sed -i "s|^X-AppImage-Version=.*|X-AppImage-Version=$VERSION|" "$dfile"
        else
            echo "X-AppImage-Version=$VERSION" >> "$dfile"
        fi
    fi
done
"""
            subprocess.run(["bash", "-c", script], check=True)

            self.assertIn("X-AppImage-Version=2.0.0", dfile1.read_text(encoding="utf-8"))
            self.assertNotIn("0.2.1", dfile1.read_text(encoding="utf-8"))
            self.assertIn("X-AppImage-Version=2.0.0", dfile2.read_text(encoding="utf-8"))

    def test_ci_workflow_artifacts(self) -> None:
        ci_path = ROOT_DIR / ".github" / "workflows" / "ci.yml"
        self.assertTrue(ci_path.exists())
        ci_content = ci_path.read_text(encoding="utf-8")

        # Verify upload-artifact includes .zsync
        self.assertIn("dist/*.AppImage", ci_content)
        self.assertIn("dist/*.zsync", ci_content)
        # Verify release step includes .zsync
        release_block = ci_content.split("name: Publish GitHub Release", 1)[1]
        self.assertIn("dist/*.zsync", release_block)


if __name__ == "__main__":
    unittest.main()
