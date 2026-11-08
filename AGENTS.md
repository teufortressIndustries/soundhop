# SoundHop Agent Rules & Guardrails (Rust v2.0 Workspace)

## 1. Pre-Commit Verification Gate
Before finishing any code or test modification, always run the full Rust CI quality and test suite locally:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

### Static Analysis & Security Conventions
- **Clippy (`-D warnings`)**:
  - Keep all workspace crates (`soundhop-core`, `soundhop-gui`, `soundhop-cli`) free of `clippy` and compiler warnings across both normal and `test` targets.
- **Network & Archive Safety**:
  - Always validate `url.starts_with("https://")` before downloading external archives (e.g., `scrcpy`), verify SHA-256 digests when provided, and reject any `..` or root path traversal components when extracting `.tar.gz` or `.zip` archives.
  - Validate that peer IP addresses are private LAN / link-local addresses (`is_private_address`) before establishing audio tunnels.

## 2. Cross-Platform (`linux` & `windows`) Code Invariants
CI runs on both `ubuntu-latest` and `windows-latest`. All code and tests must be cross-platform safe:

- **Console Output & Encoding**:
  - Route CLI output through `crates/soundhop-cli/src/ui.rs` (`safe_print`, `info`, `ok`, `warning`, `error`) which automatically falls back to ASCII symbols (`[OK]`, `[X]`, `*`, `->`) on legacy Windows consoles or when `--no-color` / `NO_COLOR` is set.
- **Filesystem & OS Primitives**:
  - Guard Unix-only APIs (`std::os::unix::fs::PermissionsExt`, `UnixListener`, `UnixStream`, `libc::geteuid`) with `#[cfg(unix)]`.
  - Preserve the cross-process `.lock` file and exponential retry backoff in `atomic_write` (`crates/soundhop-core/src/util.rs`) so concurrent writes on Windows do not fail with `PermissionDenied`.
  - When searching for managed or system binaries (`scrcpy`, `adb`), check `.exe` variants on `windows`.

## 3. Unit Test Isolation
- **Always inject a `MockRunner`-backed `AudioBackend` into `Controller`**:
  - Calling `Controller::new` with `backend: None` invokes `create_backend()`, which interacts with the host's live `pactl` or Windows audio subsystem.
  - In unit tests for `Controller` or `TuiState`, always construct an isolated `LinuxAudioBackend::with_runner(Arc::new(MockRunner::new(...)))` and call `ctrl.set_save_enabled(false)` to prevent writing to the host's `~/.config/soundhop` or `~/.local/share/soundhop`.
