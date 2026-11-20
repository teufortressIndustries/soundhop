# Changelog

All notable changes to SoundHop will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Optional `gui` feature flag on `soundhop-cli` (`--no-default-features`) for headless Linux / Raspberry Pi (`aarch64`) builds without GTK/X11/GL dependencies.
- Linux `aarch64` headless CLI tarball (`soundhop-*-linux-aarch64-headless.tar.gz`) CI build and release attachment.
- Winget (`packaging/windows/winget/`) and Scoop (`packaging/windows/scoop/soundhop.json`) Windows package manifest templates synced via `cargo xtask bump-version`.
- End-to-end daemon & IPC integration test suite (`crates/soundhop-core/tests/daemon_ipc_integration.rs`).
- Headless unit tests for `soundhop doctor` diagnostics, `--fix` flows, and bug-report JSON output.
- Edge-case unit tests for Windows audio ring-buffer overflow/underrun and mono/stereo/multi-channel conversion.
- Poison-safe mutex locking (`soundhop_core::util::lock_or_recover`) across workspace crates.

### Changed
- Decomposed `controller`, `soundhop-gui/src/backend`, and `soundhop-cli/src/core_cmds` into focused submodules.
- Updated `PKGBUILD`, `soundhop.spec`, `io.github.soundhop.metainfo.xml`, and `io.github.soundhop.yaml` to ship `soundhop-gui`, use modern `.metainfo.xml` paths, add AppStream categories/screenshots, and enable `--device=dri` in Flatpak.

## [2.3.1] - 2026-11-20

### Added
- AppManager update metadata in desktop/AppImage packaging.

### Changed
- Enforced `cargo xtask bump-version --check` in CI pipelines.

## [2.3.0] - 2026-11-20

### Added
- Windows Inno Setup installer (`SoundHop-*-windows-x86_64-setup.exe`) and portable ZIP (`SoundHop-*-windows-x86_64-portable.zip`) build scripts and CI release pipeline.
- Win32 dual-binary subsystem (`soundhop.exe` CLI + `soundhop-gui.exe` GUI) with embedded icons and manifests.
- Native Windows lifecycle: HKCU Run autostart, signal handling, paths, logging, notifications, and Win32 tray polish.
- Windows Firewall UAC elevation (`ShellExecuteExW("runas")`) and idempotent rule management.
- Persistent-phase linear resampling, WASAPI hotplug recovery, async tunnel writer, volume/mute sync, and live VU telemetry on Windows.

### Fixed
- Switched Windows GUI renderer to `wgpu` DX12 instead of OpenGL `glow` for broader GPU compatibility.
- Exact rational phase arithmetic in `LinearResampler`.

## [2.2.0] - 2026-11-19

### Added
- Zero-dependency `cargo xtask bump-version` version bumper and packaging drift checker.

### Changed
- Derived crate `VERSION` directly from workspace `Cargo.toml` and removed legacy Python leftovers.
- Used mock audio workers in Windows unit tests for headless CI execution.

## [2.1.2] - 2026-11-18

### Fixed
- Packaging and desktop integration polish.

## [2.1.1] - 2026-11-17

### Changed
- Packaging and CI alignment across Linux AppImage, AUR, and RPM specs.

## [2.1.0] - 2026-11-17

### Added
- Native desktop GUI (`soundhop-gui`) and Android audio capture bridge (`scrcpy`/`adb`).

## [2.0.0] - 2026-11-08

### Added
- Complete Rust workspace rewrite (`soundhop-core`, `soundhop-cli`, `soundhop-gui`).
- Multi-room broadcast via `module-combine-sink`.
- Persistent per-app audio routing rules.
- PIN/PSK HMAC pairing and trusted-LAN security modes.
- Interactive terminal UI (TUI), native system tray, and live telemetry.

[Unreleased]: https://github.com/teufortressIndustries/soundhop/compare/v2.3.1...HEAD
[2.3.1]: https://github.com/teufortressIndustries/soundhop/compare/v2.3.0...v2.3.1
[2.3.0]: https://github.com/teufortressIndustries/soundhop/compare/v2.2.0...v2.3.0
[2.2.0]: https://github.com/teufortressIndustries/soundhop/compare/v2.1.2...v2.2.0
[2.1.2]: https://github.com/teufortressIndustries/soundhop/compare/v2.1.1...v2.1.2
[2.1.1]: https://github.com/teufortressIndustries/soundhop/compare/v2.1.0...v2.1.1
[2.1.0]: https://github.com/teufortressIndustries/soundhop/compare/v2.0.0...v2.1.0
[2.0.0]: https://github.com/teufortressIndustries/soundhop/releases/tag/v2.0.0
