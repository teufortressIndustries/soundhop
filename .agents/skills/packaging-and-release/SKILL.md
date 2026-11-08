---
name: packaging-and-release
description: >-
  Workflow and invariants for bumping SoundHop versions, building the Linux
  AppImage (build-appimage.sh and container-build.sh), and building the Windows
  executable (cargo build --release). Use when modifying packaging scripts,
  CI release workflows, desktop/AppStream metadata, or cutting a new release.
---

# SoundHop Packaging & Release Skill (Rust v2.0)

## 1. Bumping Project Version
Never hand-edit version strings in individual files. Run the version bumper script to synchronize all 7 versioned files (`Cargo.toml`, `crates/soundhop-core/src/constants.rs`, `packaging/PKGBUILD`, `packaging/soundhop.spec`, `README.md`, `packaging/io.github.soundhop.desktop`, and `packaging/io.github.soundhop.metainfo.xml`):

```bash
./packaging/bump-version.py <X.Y.Z | --patch | --minor | --major>
```

## 2. Linux AppImage Invariants
Changes to AppImage layout or metadata must be kept synchronized between [`packaging/build-appimage.sh`](../../../packaging/build-appimage.sh) and [`packaging/container-build.sh`](../../../packaging/container-build.sh).

1. **No Duplicate `.desktop` Symlinks**:
   - Copy `packaging/io.github.soundhop.desktop` only as `io.github.soundhop.desktop` in `$APP_DIR/` and `$APP_DIR/usr/share/applications/`.
   - **Never** create `soundhop.desktop` symlinks alongside it (`appimagetool` fails when multiple `.desktop` files exist).
2. **AppStream Metainfo (`io.github.soundhop.metainfo.xml`)**:
   - Install to `$APP_DIR/usr/share/metainfo/io.github.soundhop.appdata.xml` (no duplicate symlinks).
   - Must contain `<developer_name>` and `<content_rating type="oars-1.1"/>` to pass `appstreamcli` validation during `appimagetool` packaging.
3. **Zsync & Working Directory**:
   - `appimagetool -u "$UPDATE_INFO"` requires the `zsync` system package (`zsyncmake`).
   - Always `cd "$OUTPUT_DIR"` (`dist/`) before running `appimagetool` so the `.AppImage.zsync` file is emitted into `dist/` rather than the repository root.
4. **Bundled Non-Default Shared Libraries (`$APP_DIR/usr/lib`)**:
   - Always bundle `libxdo.so*`, `libayatana-appindicator3.so*`, `libappindicator3.so*`, `libayatana-indicator3.so*`, `libindicator3.so*`, `libayatana-ido3-0.4.so*`, `libdbusmenu-gtk3.so*`, and `libdbusmenu-glib.so*` into `$APP_DIR/usr/lib/` (`AppRun` exports `LD_LIBRARY_PATH="${HERE}/usr/lib:..."`) so target distros work out of the box without needing `xdotool` or `libappindicator` packages installed.
5. **Verification**:
   ```bash
   ./dist/*.AppImage --appimage-extract-and-run version
   ./dist/*.AppImage --appimage-extract-and-run doctor
   ```

## 3. Windows Executable
- Built natively via `cargo build --release --workspace`, producing `target/release/soundhop.exe` (`dist/SoundHop.exe`) and `target/release/shop.exe`.
