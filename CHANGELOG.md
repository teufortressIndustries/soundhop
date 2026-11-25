# Changelog

All notable changes to SoundHop will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [2.7.0] - 2026-11-25

### Changed
- **GStreamer SRTP/Opus Audio Transport (Replaces SHT3)**: Replaced custom TCP-based SHT3 audio streaming (`opus2`/`mousiki`, `DriftController`, `AdaptiveResampler`, `parec`/`pacat` bridges) with a GStreamer 1.20+ pipeline (`srtpenc`/`srtpdec` `aes-128-icm` + `hmac-sha1-80`, `rtpjitterbuffer mode=slave`, `opusenc`/`opusdec` with in-band FEC and PLC, and `rtpL16pay`/`rtpL16depay` for `pcm-lossless`) paired with the authenticated v4 `X25519` + `K_pair` TCP control channel.
- **Preferences Overhaul**: Reorganized `soundhop-gui` Preferences into 4 sub-tabs (`General`, `Audio & Sync`, `Privacy & Devices`, `Network & Health`) with live GStreamer runtime status, GStreamer-tuned jitter buffer controls (`20 / 40 / 80 / 150 ms`), full 5-port configuration (`4714/tcp`, `4715/tcp`, `4716/udp`, `47777/udp`, `4713/tcp`), and live transport telemetry.
- **Packaging & CI**: Added GStreamer dependencies and runtime plugin bundling across GitHub Actions CI, AnyLinux AppImage, `PKGBUILD`, `soundhop.spec`, Flatpak, and Windows Inno Setup / portable ZIP builds.

### Added
- **UDP SRTP Media Port & Multi-Port Firewall**: Added `sharing.rtp_port` (`4716/udp`), `gst-srtp` capability advertisement in discovery metadata (`discover_once_with_meta`), and multi-port TCP/UDP firewall rule management across UFW, firewalld, nftables, and Windows Defender.
- **Headless `netsim` Verification**: Added deterministic `audiotestsrc` -> `netsim` -> `appsink` regression tests covering packet drops, jitter spikes, reordering, live volume/mute, telemetry, and SRTP key zeroization.

## [2.6.0] - 2026-11-25

### Added
- **Closed-Loop SHT3 Clock-Drift Compensation**: Added `DriftController` (EMA-smoothed PI controller with slew-rate limiting) and `AdaptiveResampler` (4-point Catmull-Rom cubic stereo resampler), plus RFC 3550 jitter diagnostics.
- **Unified Zero-Orphan Quit SoundHop**: Added `quit_all_soundhop` across System Tray, Control Center GUI (`GuiCommand::QuitAll`), and `soundhop stop` CLI to cleanly tear down `soundhop.service`, daemon IPC, and peer SoundHop processes without leaving orphan children.

### Changed
- **Non-Blocking PCM Playback & PLC Fallback**: Upgraded `LinuxPcmPlayback` with a non-blocking 8 KB-pipe writer and `WindowsPcmPlayback` with a 40 ms prebuffered ring buffer, click-free overrun/underrun fades, and Opus decode-error PLC fallback.
- **Linux Wayland/KDE/GNOME Clean Shutdown & Process Hygiene**: Added `SIGTERM`/`SIGHUP` handling, occluded Wayland viewport wake (`< 50 ms`), synchronous `on_exit` teardown, `systemd --user` `graphical-session.target` + `KillMode=mixed` + `Slice=background.slice`, `PR_SET_PDEATHSIG` + process-group child hygiene (`parec`, `pacat`, `pactl subscribe`, `scrcpy`), and duplicate `parec` elimination.
- **DSP & S16LE Encoder Deduplication**: Deduplicated DSP resamplers and gain-ramped S16LE encoders across `soundhop-core`.

### Fixed
- **Linux Remote Audio & mDNS Advertising**: Fixed `LinuxAudioBackend` `cookie_path_override` propagation in remote `pactl_call` and `send_mic_to_remote`, and fixed live mDNS unregistration/re-registration in `DiscoveryManager::set_advertising`.

## [2.5.0] - 2026-11-24

### Added
- **Control Center GUI**: Added an SHT3 audio quality/latency profile selector in Advanced Settings (`opus-balanced`, `opus-low-latency`, `opus-low-bandwidth`, `pcm-lossless`).
- **Congestion Adaptation & Session Supervision**: Added SHT3 `BitrateHint` feedback for adaptive Opus payload rate control, proactive session rekeying, and multi-peer SHT3 controller/zombie supervision with live telemetry synchronization.

### Changed
- **Full-Duplex SHT3 Engine & Handshake Hardening**: Split transport AEAD and TCP stream halves (`into_split()`) with wire-buffer reuse, socket timeouts, bounded concurrent server handshakes, and outbound reconnect backoff honoring `disconnect_grace_s`.
- **Real-Time Opus & Audio DSP**: Upgraded Opus FEC/reset handling, real-time Opus PLC, adaptive jitter cushion, and stereo gain-ramping / de-clicking DSP across discontinuity boundaries.

### Fixed
- **Linux & Windows PCM Bridges**: Hardened Linux `parec` / `O_NONBLOCK` capture and Windows WASAPI idle-silence handling and gain ramping against stalls and audio clicks.

## [2.4.1] - 2026-11-20

### Added
- Two-way mutual SAS verification during Pairing v2 and removal of the legacy PIN pairing card.

### Fixed
- Enabled `mousiki` `fixed_point` decoding to eliminate Opus decoder distortion.
- Aligned `parec` capture latency to the active profile `frame_ms`, enforced atomic frame writes with partial-read timeout safety, and added SHT3 volume synchronization.

## [2.4.0] - 2026-11-20

### Added
- **SoundHop Transport v3 (`SHT3`)**: Authenticated ChaCha20-Poly1305 encrypted TCP audio transport (`sharing.v3_port`, default `4714`) with pure-Rust Opus (`opus-balanced`, `opus-low-latency`, `opus-low-bandwidth`) and `pcm-lossless` profiles (`audio.profile`), PCM bridges, and live session revocation.
- **Pairing v2 Interactive Device Trust**: Per-device Ed25519 identity keys, X25519 key agreement, and interactive TCP pairing listener (`sharing.pair_port`, default `4715`) with 6-digit SAS verification, 120-second headless one-time codes, and daemon IPC (`pair_request`, `pair_pending`, `pair_approve`, `pair_deny`, `pair_revoke`, `pair_list`, `pair_code`).
- **Pairing v2 & Audio Profile UX**:
  - Control Center GUI: incoming pair-request approval modal with SAS verification code, `PeerTrustState` badges (`Trusted` / `Pairing Required` / `Open LAN`) with one-click **Pair** action, **Trusted Devices** management with one-click **Revoke**, live security enforcement banner, and **Audio Quality & Latency** profile selector.
  - System Tray: pending pair request indicators in tray tooltip and quick Approve/Deny menu actions.
  - CLI & TUI: `soundhop pair request/pending/approve/deny/revoke/list/code` subcommands and TUI popup approval modal (`[a]` Approve / `[d]` Deny).
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
- Refreshed `README.md` for v2.4.0 (Pairing v2, SHT3 Opus + ChaCha20-Poly1305 profiles, and headless ARM64 builds).

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

[Unreleased]: https://github.com/teufortressIndustries/soundhop/compare/v2.6.0...HEAD
[2.6.0]: https://github.com/teufortressIndustries/soundhop/compare/v2.5.0...v2.6.0
[2.5.0]: https://github.com/teufortressIndustries/soundhop/compare/v2.4.1...v2.5.0
[2.4.1]: https://github.com/teufortressIndustries/soundhop/compare/v2.4.0...v2.4.1
[2.4.0]: https://github.com/teufortressIndustries/soundhop/compare/v2.3.1...v2.4.0
[2.3.1]: https://github.com/teufortressIndustries/soundhop/compare/v2.3.0...v2.3.1
[2.3.0]: https://github.com/teufortressIndustries/soundhop/compare/v2.2.0...v2.3.0
[2.2.0]: https://github.com/teufortressIndustries/soundhop/compare/v2.1.2...v2.2.0
[2.1.2]: https://github.com/teufortressIndustries/soundhop/compare/v2.1.1...v2.1.2
[2.1.1]: https://github.com/teufortressIndustries/soundhop/compare/v2.1.0...v2.1.1
[2.1.0]: https://github.com/teufortressIndustries/soundhop/compare/v2.0.0...v2.1.0
[2.0.0]: https://github.com/teufortressIndustries/soundhop/releases/tag/v2.0.0
