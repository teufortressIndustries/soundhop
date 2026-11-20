//! Cross-platform utilities: XDG/Win32 paths, atomic file writes, process execution,
//! string formatting, network address inspection, and desktop notifications.

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ipnet::Ipv4Net;
use rand::Rng;
use thiserror::Error;

use crate::constants::{APP_SLUG, VERSION};

// ---------------------------------------------------------------------------
// Synchronization helpers
// ---------------------------------------------------------------------------

/// Lock a [`Mutex`], recovering the inner guard if a previous holder panicked.
pub fn lock_or_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------
// Process execution & Mockable Runner
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum UtilError {
    #[error("Required command not found: {0}")]
    ToolMissing(String),
    #[error("Command failed ({returncode}): {command}\n{detail}")]
    CommandFailed {
        command: String,
        returncode: i32,
        detail: String,
    },
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandResult {
    pub args: Vec<String>,
    pub returncode: i32,
    pub stdout: String,
    pub stderr: String,
}

impl CommandResult {
    pub fn ok(&self) -> bool {
        self.returncode == 0
    }

    pub fn combined_output(&self) -> String {
        let err = self.stderr.trim();
        if !err.is_empty() {
            err.to_string()
        } else {
            self.stdout.trim().to_string()
        }
    }
}

pub trait ProcessRunner: Send + Sync {
    fn run(
        &self,
        args: &[String],
        env_vars: Option<&HashMap<String, String>>,
        timeout: Duration,
    ) -> Result<CommandResult, UtilError>;

    fn command_exists(&self, name: &str) -> bool;
}

#[derive(Debug, Default, Clone)]
pub struct SystemRunner;

impl SystemRunner {
    /// Execute a command allowing interactive terminal input (no `process_group(0)`,
    /// inherited `stdin`) so tools like `sudo` can prompt on `/dev/tty`, while still
    /// capturing `stdout` and `stderr`.
    pub fn run_interactive(
        &self,
        args: &[String],
        env_vars: Option<&HashMap<String, String>>,
        timeout: Duration,
    ) -> Result<CommandResult, UtilError> {
        run_with_system_options(args, env_vars, timeout, true)
    }
}

/// Terminates `child` and, on Unix, its entire process group so forked
/// grandchildren do not keep inherited stdout/stderr pipes open after timeout.
fn terminate_child(child: &mut std::process::Child) {
    #[cfg(unix)]
    unsafe {
        let pgid = child.id() as libc::pid_t;
        if pgid > 0 {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn run_with_system_options(
    args: &[String],
    env_vars: Option<&HashMap<String, String>>,
    timeout: Duration,
    interactive: bool,
) -> Result<CommandResult, UtilError> {
    let Some(program) = args.first() else {
        return Err(UtilError::ToolMissing(String::new()));
    };
    let mut cmd = Command::new(program);
    if args.len() > 1 {
        cmd.args(&args[1..]);
    }
    cmd.stdin(if interactive {
        Stdio::inherit()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    if let Some(vars) = env_vars {
        for (k, v) in vars {
            cmd.env(k, v);
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    if !interactive {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    close_fds_pre_exec(&mut cmd);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(UtilError::ToolMissing(program.clone()));
        }
        Err(err) => return Err(UtilError::Io(err)),
    };

    let stdout_handle = child.stdout.take().map(|mut out| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = std::io::Read::read_to_end(&mut out, &mut buf);
            String::from_utf8_lossy(&buf).into_owned()
        })
    });
    let stderr_handle = child.stderr.take().map(|mut err| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = std::io::Read::read_to_end(&mut err, &mut buf);
            String::from_utf8_lossy(&buf).into_owned()
        })
    });

    let effective_timeout = if timeout.is_zero() {
        Duration::from_secs(10)
    } else {
        timeout
    };
    let start = Instant::now();
    let mut timed_out = false;
    let status_code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code().unwrap_or(1),
            Ok(None) => {
                if start.elapsed() >= effective_timeout {
                    timed_out = true;
                    if interactive {
                        let _ = child.kill();
                        let _ = child.wait();
                    } else {
                        terminate_child(&mut child);
                    }
                    break 124;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(err) => {
                if interactive {
                    let _ = child.kill();
                    let _ = child.wait();
                } else {
                    terminate_child(&mut child);
                }
                return Err(UtilError::Io(err));
            }
        }
    };

    let stdout = stdout_handle
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    let mut stderr = stderr_handle
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    if timed_out {
        if !stderr.is_empty() && !stderr.ends_with('\n') {
            stderr.push('\n');
        }
        stderr.push_str(&format!(
            "Command timed out after {:.1}s",
            effective_timeout.as_secs_f64()
        ));
    }

    Ok(CommandResult {
        args: args.to_vec(),
        returncode: status_code,
        stdout,
        stderr,
    })
}

impl ProcessRunner for SystemRunner {
    fn run(
        &self,
        args: &[String],
        env_vars: Option<&HashMap<String, String>>,
        timeout: Duration,
    ) -> Result<CommandResult, UtilError> {
        run_with_system_options(args, env_vars, timeout, false)
    }

    fn command_exists(&self, name: &str) -> bool {
        which_binary(name).is_some()
    }
}

/// Deterministic mock process runner for unit tests.
pub struct MockRunner {
    #[allow(clippy::type_complexity)]
    handler: Arc<
        dyn Fn(&[String], Option<&HashMap<String, String>>) -> Result<CommandResult, UtilError>
            + Send
            + Sync,
    >,
    pub calls: Mutex<Vec<Vec<String>>>,
    pub available_commands: Mutex<HashSet<String>>,
}

impl MockRunner {
    pub fn new<F>(handler: F) -> Self
    where
        F: Fn(&[String], Option<&HashMap<String, String>>) -> Result<CommandResult, UtilError>
            + Send
            + Sync
            + 'static,
    {
        let mut cmds = HashSet::new();
        cmds.insert("pactl".to_string());
        cmds.insert("adb".to_string());
        cmds.insert("scrcpy".to_string());
        Self {
            handler: Arc::new(handler),
            calls: Mutex::new(Vec::new()),
            available_commands: Mutex::new(cmds),
        }
    }

    pub fn recorded_calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl ProcessRunner for MockRunner {
    fn run(
        &self,
        args: &[String],
        env_vars: Option<&HashMap<String, String>>,
        _timeout: Duration,
    ) -> Result<CommandResult, UtilError> {
        self.calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(args.to_vec());
        (self.handler)(args, env_vars)
    }

    fn command_exists(&self, name: &str) -> bool {
        self.available_commands
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(name)
    }
}

pub fn run_command(args: &[&str], check: bool) -> Result<CommandResult, UtilError> {
    let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
    let res = SystemRunner.run(&owned, None, Duration::from_secs(15))?;
    if check && !res.ok() {
        return Err(UtilError::CommandFailed {
            command: owned.join(" "),
            returncode: res.returncode,
            detail: res.combined_output(),
        });
    }
    Ok(res)
}

pub fn which_binary(name: &str) -> Option<PathBuf> {
    let candidate_path = Path::new(name);
    if candidate_path.components().count() > 1 && candidate_path.is_file() {
        return Some(candidate_path.to_path_buf());
    }
    let path_var = env::var_os("PATH")?;
    let exts: &[&str] = if cfg!(windows) {
        &[".exe", ".cmd", ".bat", ""]
    } else {
        &[""]
    };
    for dir in env::split_paths(&path_var) {
        for ext in exts {
            let file_name = if ext.is_empty() || name.to_lowercase().ends_with(ext) {
                name.to_string()
            } else {
                format!("{name}{ext}")
            };
            let full = dir.join(file_name);
            if full.is_file() && is_executable(&full) {
                return Some(full);
            }
        }
    }
    None
}

pub fn command_exists(name: &str) -> bool {
    which_binary(name).is_some()
}

pub fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        if !path.is_file() {
            return false;
        }
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = path.metadata() {
            return (meta.permissions().mode() & 0o111) != 0;
        }
        false
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

// ---------------------------------------------------------------------------
// XDG & Cross-Platform Paths
// ---------------------------------------------------------------------------

pub fn home_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(up) = env::var_os("USERPROFILE").filter(|v| !v.is_empty()) {
            return PathBuf::from(up);
        }
        if let Some(h) = env::var_os("HOME").filter(|v| !v.is_empty()) {
            return PathBuf::from(h);
        }
        env::temp_dir()
    }
    #[cfg(not(windows))]
    {
        if let Some(h) = env::var_os("HOME").filter(|v| !v.is_empty()) {
            return PathBuf::from(h);
        }
        if let Some(up) = env::var_os("USERPROFILE").filter(|v| !v.is_empty()) {
            return PathBuf::from(up);
        }
        env::temp_dir()
    }
}

pub fn config_dir() -> PathBuf {
    if let Some(base) = env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(base).join(APP_SLUG);
    }
    #[cfg(windows)]
    {
        if let Some(appdata) = env::var_os("APPDATA").or_else(|| env::var_os("LOCALAPPDATA")) {
            return PathBuf::from(appdata).join(APP_SLUG);
        }
    }
    home_dir().join(".config").join(APP_SLUG)
}

pub fn state_dir() -> PathBuf {
    if let Some(base) = env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(base).join(APP_SLUG);
    }
    #[cfg(windows)]
    {
        if let Some(local) = env::var_os("LOCALAPPDATA") {
            return PathBuf::from(local).join(APP_SLUG);
        }
    }
    home_dir().join(".local").join("state").join(APP_SLUG)
}

pub fn data_dir() -> PathBuf {
    if let Some(base) = env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(base).join(APP_SLUG);
    }
    #[cfg(windows)]
    {
        if let Some(local) = env::var_os("LOCALAPPDATA") {
            return PathBuf::from(local).join(APP_SLUG);
        }
    }
    home_dir().join(".local").join("share").join(APP_SLUG)
}

pub fn runtime_dir() -> PathBuf {
    if let Some(base) = env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(base).join(APP_SLUG);
    }
    #[cfg(windows)]
    {
        if let Some(local) = env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()) {
            return PathBuf::from(local).join(APP_SLUG).join("run");
        }
        if let Some(up) = env::var_os("USERPROFILE").filter(|v| !v.is_empty()) {
            return PathBuf::from(up)
                .join("AppData")
                .join("Local")
                .join(APP_SLUG)
                .join("run");
        }
        let user = env::var("USERNAME").unwrap_or_else(|_| "user".to_string());
        env::temp_dir().join(format!("{APP_SLUG}-{user}"))
    }
    #[cfg(not(windows))]
    {
        #[cfg(unix)]
        let uid = unsafe { libc::getuid() }.to_string();
        #[cfg(not(unix))]
        let uid = env::var("USERNAME").unwrap_or_else(|_| "user".to_string());
        env::temp_dir().join(format!("{APP_SLUG}-{uid}"))
    }
}

#[cfg(windows)]
fn is_windows_reparse_point_or_symlink(meta: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    meta.file_type().is_symlink() || (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0
}

pub fn ensure_dir(path: &Path) -> std::io::Result<PathBuf> {
    #[cfg(unix)]
    {
        if let Ok(pre_meta) = fs::symlink_metadata(path) {
            if pre_meta.file_type().is_symlink() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "Refusing symlinked directory",
                ));
            }
        }
    }
    #[cfg(windows)]
    {
        if let Ok(pre_meta) = fs::symlink_metadata(path) {
            if is_windows_reparse_point_or_symlink(&pre_meta) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "Refusing symlinked or reparse-point directory",
                ));
            }
        }
    }
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let meta = fs::symlink_metadata(path)?;
        if meta.file_type().is_symlink() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "Refusing symlinked directory",
            ));
        }
        let uid = unsafe { libc::getuid() };
        if meta.uid() != uid {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "Directory is not owned by the current user",
            ));
        }
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
    }
    #[cfg(windows)]
    {
        let meta = fs::symlink_metadata(path)?;
        if is_windows_reparse_point_or_symlink(&meta) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "Refusing symlinked or reparse-point directory",
            ));
        }
    }
    Ok(path.to_path_buf())
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn state_path() -> PathBuf {
    state_dir().join("state.json")
}

pub fn log_path() -> PathBuf {
    state_dir().join("soundhop.log")
}

pub fn socket_path() -> PathBuf {
    runtime_dir().join("control.sock")
}

pub fn lock_path() -> PathBuf {
    runtime_dir().join("daemon.lock")
}

pub fn pid_path() -> PathBuf {
    runtime_dir().join("daemon.pid")
}

/// Maximum log file size before rotating `soundhop.log` -> `soundhop.log.old` (2 MiB).
pub const MAX_LOG_FILE_BYTES: u64 = 2 * 1024 * 1024;

static LOG_FILE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// Rotate `path` to `<path>.old` (or truncate on rename failure) if its size
/// meets or exceeds `max_bytes`.
pub fn rotate_log_if_needed(path: &Path, max_bytes: u64) {
    let Ok(meta) = fs::metadata(path) else {
        return;
    };
    if meta.len() < max_bytes {
        return;
    }
    let backup = path.with_extension("log.old");
    let _ = fs::remove_file(&backup);
    if fs::rename(path, &backup).is_err() {
        let _ = fs::write(path, "");
    }
}

/// Append a single timestamped log line to `path` with size-bounded rotation.
pub fn append_log_line_to(path: &Path, level: &str, message: &str, max_bytes: u64) {
    let lock = LOG_FILE_LOCK.get_or_init(|| Mutex::new(()));
    let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(parent) = path.parent() {
        if ensure_dir(parent).is_err() {
            return;
        }
    }
    rotate_log_if_needed(path, max_bytes);
    let ts = now_secs() as u64;
    let clean_msg = message.replace(['\r', '\n'], " ");
    let line = format!("[{ts}] [{level}] {clean_msg}\n");
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(line.as_bytes());
    }
}

/// Append a timestamped diagnostic line to `log_path()`.
pub fn append_log_line(level: &str, message: &str) {
    append_log_line_to(&log_path(), level, message, MAX_LOG_FILE_BYTES);
}

/// Initialize the daemon log file at `log_path()`, rotating if oversized and
/// writing a startup marker.
pub fn init_daemon_log() {
    let path = log_path();
    append_log_line_to(
        &path,
        "INFO",
        &format!(
            "SoundHop daemon v{VERSION} (pid={}) starting (runtime_dir={})",
            std::process::id(),
            runtime_dir().display()
        ),
        MAX_LOG_FILE_BYTES,
    );
}

/// Marks non-standard file descriptors (>= 3) as `FD_CLOEXEC` on Unix before executing
/// child processes. Using `CLOEXEC` (instead of closing immediately) preserves Rust's
/// internal `O_CLOEXEC` exec-error pipe so `Command::spawn` still reports `ENOENT`/`EACCES`,
/// while preventing parent file descriptors (such as AppImage FUSE mount directory fds)
/// from leaking into child processes (pactl, parec, scrcpy, notify-send).
#[cfg(unix)]
pub fn close_fds_pre_exec(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    let max_fd = unsafe {
        let lim = libc::sysconf(libc::_SC_OPEN_MAX);
        if lim > 3 {
            lim.min(4096) as libc::c_int
        } else {
            1024
        }
    };
    unsafe {
        cmd.pre_exec(move || {
            #[cfg(target_os = "linux")]
            let cloexec_done = libc::close_range(3, u32::MAX, libc::CLOSE_RANGE_CLOEXEC as _) == 0;
            #[cfg(not(target_os = "linux"))]
            let cloexec_done = false;

            if !cloexec_done {
                for fd in 3..max_fd {
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    if flags != -1 && (flags & libc::FD_CLOEXEC) == 0 {
                        libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
                    }
                }
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
pub fn close_fds_pre_exec(_cmd: &mut Command) {}

/// Metadata written by the active lock holder after acquiring `InstanceLock`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceLockInfo {
    pub pid: Option<u64>,
    pub version: String,
}

/// Single-instance process lock backed by kernel flock on Unix and exclusive sharing on Windows.
pub struct InstanceLock {
    _file: fs::File,
    _path: PathBuf,
    _info_path: PathBuf,
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        // Remove companion metadata while the lock is still held so another acquirer
        // cannot race to recreate it before we unlock.
        let _ = fs::remove_file(&self._info_path);
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            unsafe {
                libc::flock(self._file.as_raw_fd(), libc::LOCK_UN);
            }
        }
    }
}

pub fn instance_lock_path(name: &str) -> PathBuf {
    runtime_dir().join(format!("{name}.lock"))
}

pub fn instance_lock_info_path(name: &str) -> PathBuf {
    runtime_dir().join(format!("{name}.lock.info"))
}

pub fn read_instance_lock_info(name: &str) -> Option<InstanceLockInfo> {
    let content = fs::read_to_string(instance_lock_info_path(name))
        .or_else(|_| fs::read_to_string(instance_lock_path(name)))
        .ok()?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }
    let val: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    let pid = val.get("pid").and_then(|v| v.as_u64());
    let version = val
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Some(InstanceLockInfo { pid, version })
}

pub fn try_acquire_instance_lock(name: &str) -> Option<InstanceLock> {
    let dir = runtime_dir();
    let _ = ensure_dir(&dir);
    let lock_path = dir.join(format!("{name}.lock"));
    let info_path = dir.join(format!("{name}.lock.info"));
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        opts.share_mode(0);
    }
    let file = opts.open(&lock_path).ok()?;

    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let res = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if res != 0 {
            return None;
        }
    }

    let meta = serde_json::json!({
        "pid": std::process::id(),
        "version": VERSION,
    })
    .to_string();
    let _ = atomic_write(&info_path, &meta);

    Some(InstanceLock {
        _file: file,
        _path: lock_path,
        _info_path: info_path,
    })
}

/// Acquires the single-instance lock `name`, terminating an older-version holder if one
/// is detected via lock metadata (or `legacy_finder` for older builds without lock metadata).
pub fn replace_stale_instance(
    name: &str,
    legacy_finder: Option<fn() -> Option<InstanceLockInfo>>,
) -> Option<InstanceLock> {
    if let Some(lock) = try_acquire_instance_lock(name) {
        return Some(lock);
    }

    let holder = read_instance_lock_info(name).or_else(|| legacy_finder.and_then(|f| f()))?;
    if holder.version == VERSION {
        return None;
    }

    let pid = holder.pid?;
    if !crate::ipc::is_soundhop_process(pid) {
        return None;
    }

    let _ = crate::ipc::terminate_process(pid);

    for _ in 0..60 {
        if let Some(lock) = try_acquire_instance_lock(name) {
            return Some(lock);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

static ATOMIC_WRITE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub fn atomic_write(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        ensure_dir(parent)?;
    }
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let rand_hex: u64 = rand::thread_rng().gen();
    let tmp_name = format!(".{file_name}.{}.{rand_hex:016x}.tmp", std::process::id());
    let tmp_path = path.with_file_name(tmp_name);

    let write_res = (|| -> std::io::Result<()> {
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&tmp_path)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);

        let lock = ATOMIC_WRITE_LOCK.get_or_init(|| Mutex::new(()));
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        for attempt in 0..5 {
            match fs::rename(&tmp_path, path) {
                Ok(()) => return Ok(()),
                Err(err) => {
                    if !cfg!(windows) || attempt == 4 {
                        return Err(err);
                    }
                    std::thread::sleep(Duration::from_millis(10 * (attempt + 1)));
                }
            }
        }
        Ok(())
    })();

    if write_res.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    write_res
}

// ---------------------------------------------------------------------------
// Time & Formatting Helpers
// ---------------------------------------------------------------------------

pub fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub fn format_age(seconds: Option<f64>) -> String {
    let Some(secs) = seconds else {
        return "never".to_string();
    };
    if secs < 5.0 {
        "just now".to_string()
    } else if secs < 60.0 {
        format!("{}s ago", secs as u64)
    } else if secs < 3600.0 {
        format!("{}m ago", (secs / 60.0) as u64)
    } else if secs < 86400.0 {
        format!("{}h ago", (secs / 3600.0) as u64)
    } else {
        format!("{}d ago", (secs / 86400.0) as u64)
    }
}

pub fn format_duration(seconds: Option<f64>) -> String {
    let Some(secs) = seconds else {
        return "-".to_string();
    };
    let total = secs.max(0.0) as u64;
    let hours = total / 3600;
    let rem = total % 3600;
    let minutes = rem / 60;
    let s = rem % 60;
    if hours > 0 {
        format!("{hours}h {minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

pub fn human_bytes(num: u64) -> String {
    let mut val = num as f64;
    for unit in ["B", "KiB", "MiB", "GiB"] {
        if val < 1024.0 || unit == "GiB" {
            return if unit == "B" {
                format!("{val:.0} {unit}")
            } else {
                format!("{val:.1} {unit}")
            };
        }
        val /= 1024.0;
    }
    format!("{val:.1} GiB")
}

pub fn slugify(value: &str, max_length: usize) -> String {
    let mut out = String::with_capacity(value.len());
    let mut last_underscore = true;
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_underscore = false;
        } else if !last_underscore {
            out.push('_');
            last_underscore = true;
        }
    }
    let trimmed = out.trim_matches('_');
    if trimmed.is_empty() {
        return "device".to_string();
    }
    let truncated: String = trimmed.chars().take(max_length).collect();
    let final_slug = truncated.trim_matches('_');
    if final_slug.is_empty() {
        "device".to_string()
    } else {
        final_slug.to_string()
    }
}

pub fn sanitize_sink_name(value: &str, max_length: usize) -> String {
    let mut out = String::with_capacity(value.len());
    let mut last_underscore = true;
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '.' || ch == '-' {
            out.push(ch);
            last_underscore = ch == '_';
        } else if !last_underscore {
            out.push('_');
            last_underscore = true;
        }
    }
    let trimmed = out.trim_matches('_');
    if trimmed.is_empty() {
        "device".to_string()
    } else {
        trimmed.chars().take(max_length).collect()
    }
}

pub fn hostname_name() -> String {
    if let Ok(h) = env::var("HOSTNAME").or_else(|_| env::var("COMPUTERNAME")) {
        let short = h.split('.').next().unwrap_or("").trim();
        if !short.is_empty() {
            return short.to_string();
        }
    }
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
        if rc == 0 {
            let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            if let Ok(s) = std::str::from_utf8(&buf[..len]) {
                let short = s.split('.').next().unwrap_or("").trim();
                if !short.is_empty() {
                    return short.to_string();
                }
            }
        }
    }
    if cfg!(windows) {
        "Windows device".to_string()
    } else {
        "Linux device".to_string()
    }
}

pub fn new_device_id() -> String {
    let bytes: [u8; 16] = rand::thread_rng().gen();
    hex::encode(bytes)
}

pub fn short_id(value: &str, length: usize) -> String {
    let filtered: String = value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(length)
        .collect();
    if filtered.is_empty() {
        "0".repeat(length)
    } else {
        filtered
    }
}

// ---------------------------------------------------------------------------
// Networking & Address Helpers
// ---------------------------------------------------------------------------

pub fn tcp_server_arg(host: &str, port: u16) -> String {
    let mut cleaned = host.trim();
    if cleaned.starts_with('[') && cleaned.ends_with(']') && cleaned.len() >= 2 {
        cleaned = &cleaned[1..cleaned.len() - 1];
    }
    if cleaned.contains(':') {
        let no_zone = cleaned.split('%').next().unwrap_or(cleaned);
        format!("tcp:[{no_zone}]:{port}")
    } else {
        format!("tcp:{cleaned}:{port}")
    }
}

pub fn split_host_port(value: &str, default_port: u16) -> (String, u16) {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return (String::new(), default_port);
    }
    if let Some(rest) = trimmed.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            let host = &rest[..end];
            let after = &rest[end + 1..];
            if let Some(port_str) = after.strip_prefix(':') {
                if let Ok(p) = port_str.parse::<u16>() {
                    return (host.to_string(), p);
                }
            }
            return (host.to_string(), default_port);
        }
    }
    if trimmed.matches(':').count() == 1 {
        if let Some((host, port_str)) = trimmed.rsplit_once(':') {
            if let Ok(p) = port_str.parse::<u16>() {
                return (host.to_string(), p);
            }
        }
    }
    (trimmed.to_string(), default_port)
}

pub fn is_ip_link_local(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_link_local(),
        IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80,
    }
}

pub fn is_ip_private_or_local(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // Unique Local (fc00::/7)
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // Link-Local (fe80::/10)
        }
    }
}

pub fn is_private_address(value: &str) -> bool {
    let mut trimmed = value.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.starts_with('[') && trimmed.ends_with(']') && trimmed.len() >= 2 {
        trimmed = &trimmed[1..trimmed.len() - 1];
    }
    if trimmed.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let host_clean = trimmed.split('%').next().unwrap_or(trimmed);
    if let Ok(ip) = host_clean.parse::<IpAddr>() {
        return is_ip_private_or_local(&ip);
    }
    if let Ok(addrs) = (host_clean, 0u16).to_socket_addrs() {
        let mut any = false;
        for sock_addr in addrs {
            any = true;
            if !is_ip_private_or_local(&sock_addr.ip()) {
                return false;
            }
        }
        return any;
    }
    false
}

pub fn is_virtual_bridge_interface(name: &str) -> bool {
    let lower = name.trim().to_ascii_lowercase();
    for prefix in [
        "docker",
        "br-",
        "veth",
        "virbr",
        "podman",
        "cni",
        "flannel",
        "cali",
        "lxcbr",
        "vboxnet",
        "vmnet",
        "tunl",
        "sit",
        "vethernet",
    ] {
        if lower.starts_with(prefix) {
            return true;
        }
    }
    for token in [
        "vethernet",
        "wsl",
        "vmware",
        "virtualbox",
        "npcap",
        "hyper-v",
        "loopback",
    ] {
        if lower.contains(token) {
            return true;
        }
    }
    false
}

pub fn all_local_ips() -> HashSet<String> {
    let mut set = HashSet::new();
    if let Ok(interfaces) = if_addrs::get_if_addrs() {
        for iface in interfaces {
            let ip = iface.ip();
            if ip.is_loopback() {
                continue;
            }
            set.insert(ip.to_string());
        }
    }
    set
}

pub fn is_own_non_loopback_ip(host: &str) -> bool {
    let mut cleaned = host.trim();
    if cleaned.starts_with('[') && cleaned.ends_with(']') && cleaned.len() >= 2 {
        cleaned = &cleaned[1..cleaned.len() - 1];
    }
    let cleaned = cleaned.split('%').next().unwrap_or(cleaned);
    if cleaned.is_empty()
        || cleaned == "127.0.0.1"
        || cleaned == "::1"
        || cleaned.eq_ignore_ascii_case("localhost")
    {
        return false;
    }
    all_local_ips().contains(cleaned)
}

pub fn is_same_lan_subnet(host: &str) -> bool {
    let Ok(IpAddr::V4(target_v4)) = host.trim().parse::<IpAddr>() else {
        return false;
    };
    if let Ok(interfaces) = if_addrs::get_if_addrs() {
        for iface in interfaces {
            if is_virtual_bridge_interface(&iface.name) {
                continue;
            }
            if let if_addrs::IfAddr::V4(v4) = iface.addr {
                if v4.ip.is_loopback() || v4.ip.is_link_local() {
                    continue;
                }
                if let Ok(net) = Ipv4Net::with_netmask(v4.ip, v4.netmask) {
                    if net.contains(&target_v4) {
                        return true;
                    }
                }
            }
        }
    }
    false
}

pub fn select_best_peer_ip<'a>(candidates: impl IntoIterator<Item = &'a IpAddr>) -> Option<String> {
    let own_ips = all_local_ips();
    let mut scored: Vec<(u8, String)> = Vec::new();
    for ip in candidates {
        if !is_ip_private_or_local(ip) || is_ip_link_local(ip) || ip.is_loopback() {
            continue;
        }
        let s = ip.to_string();
        if own_ips.contains(&s) {
            continue;
        }
        let score = match ip {
            IpAddr::V4(v4) => {
                if is_same_lan_subnet(&s) {
                    0
                } else if v4.octets()[0] == 192 && v4.octets()[1] == 168 {
                    1
                } else if v4.octets()[0] == 10 {
                    2
                } else {
                    3
                }
            }
            IpAddr::V6(_) => 4,
        };
        scored.push((score, s));
    }
    scored.sort_by_key(|(score, _)| *score);
    scored.into_iter().next().map(|(_, s)| s)
}

pub fn local_addresses(include_loopback: bool) -> Vec<String> {
    let mut v4_primary = Vec::new();
    let mut v4_secondary = Vec::new();
    let mut v6_addrs = Vec::new();
    if let Ok(interfaces) = if_addrs::get_if_addrs() {
        for iface in interfaces {
            if is_virtual_bridge_interface(&iface.name) {
                continue;
            }
            let ip = iface.ip();
            if ip.is_loopback() && !include_loopback {
                continue;
            }
            if is_ip_link_local(&ip) || !is_ip_private_or_local(&ip) {
                continue;
            }
            let s = ip.to_string();
            match ip {
                IpAddr::V4(v4) if !v4.is_loopback() => {
                    let oct = v4.octets();
                    if (oct[0] == 192 && oct[1] == 168) || oct[0] == 10 {
                        if !v4_primary.contains(&s) {
                            v4_primary.push(s);
                        }
                    } else if !v4_secondary.contains(&s) {
                        v4_secondary.push(s);
                    }
                }
                _ => {
                    if !v6_addrs.contains(&s) {
                        v6_addrs.push(s);
                    }
                }
            }
        }
    }
    let mut addrs = Vec::new();
    addrs.extend(v4_primary);
    addrs.extend(v4_secondary);
    addrs.extend(v6_addrs);
    if include_loopback && !addrs.iter().any(|a| a == "127.0.0.1") {
        addrs.push("127.0.0.1".to_string());
    }
    addrs
}

/// Return deduplicated private IPv4 CIDR subnets (e.g. `"192.168.1.0/24"`) for active
/// non-virtual network interfaces on this host.
pub fn active_lan_subnets() -> Vec<String> {
    let mut subnets = Vec::new();
    if let Ok(interfaces) = if_addrs::get_if_addrs() {
        for iface in interfaces {
            if is_virtual_bridge_interface(&iface.name) {
                continue;
            }
            if let if_addrs::IfAddr::V4(v4) = iface.addr {
                if v4.ip.is_loopback() || v4.ip.is_link_local() || !v4.ip.is_private() {
                    continue;
                }
                if let Ok(net) = Ipv4Net::with_netmask(v4.ip, v4.netmask) {
                    let cidr = net.trunc().to_string();
                    if !subnets.contains(&cidr) {
                        subnets.push(cidr);
                    }
                }
            }
        }
    }
    subnets
}

pub fn broadcast_addresses(include_loopback: bool) -> Vec<String> {
    let mut targets = vec!["255.255.255.255".to_string()];
    if let Ok(interfaces) = if_addrs::get_if_addrs() {
        for iface in interfaces {
            if is_virtual_bridge_interface(&iface.name) {
                continue;
            }
            if let if_addrs::IfAddr::V4(v4) = iface.addr {
                if v4.ip.is_loopback() {
                    continue;
                }
                if let Some(bcast) = v4.broadcast {
                    let s = bcast.to_string();
                    if !targets.contains(&s) {
                        targets.push(s);
                    }
                } else if let Ok(net) = Ipv4Net::with_netmask(v4.ip, v4.netmask) {
                    let s = net.broadcast().to_string();
                    if !targets.contains(&s) {
                        targets.push(s);
                    }
                }
            }
        }
    }
    if include_loopback {
        let lo = Ipv4Addr::LOCALHOST.to_string();
        if !targets.contains(&lo) {
            targets.push(lo);
        }
    } else {
        targets.retain(|t| t != "127.0.0.1");
    }
    targets
}

pub fn tcp_reachable(host: &str, port: u16, timeout: Duration) -> bool {
    tcp_probe_rtt(host, port, timeout).is_some()
}

/// Connect to `(host, port)` and return the TCP handshake RTT in milliseconds.
pub fn tcp_probe_rtt(host: &str, port: u16, timeout: Duration) -> Option<f64> {
    let clean = host.trim().trim_start_matches('[').trim_end_matches(']');
    let addr: SocketAddr = if let Ok(ip) = clean.parse::<IpAddr>() {
        SocketAddr::new(ip, port)
    } else if let Ok(sa) = format!("[{clean}]:{port}").parse::<SocketAddr>() {
        sa
    } else if let Some(sa) = (clean, port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut it| it.next())
    {
        sa
    } else {
        let no_zone = clean.split('%').next().unwrap_or(clean);
        let ip = no_zone.parse::<IpAddr>().ok()?;
        SocketAddr::new(ip, port)
    };
    let start = Instant::now();
    TcpStream::connect_timeout(&addr, timeout).ok()?;
    Some(start.elapsed().as_secs_f64() * 1000.0)
}

pub fn notify(title: &str, body: &str, urgency: &str) {
    if env::var_os("SOUNDHOP_NO_NOTIFY").is_some() {
        return;
    }
    #[cfg(target_os = "linux")]
    {
        if command_exists("notify-send") {
            let mut cmd = Command::new("notify-send");
            cmd.args([
                "--app-name=SoundHop",
                "--icon=soundhop",
                &format!("--urgency={urgency}"),
                "--expire-time=4000",
                title,
                body,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
            close_fds_pre_exec(&mut cmd);
            if let Ok(mut child) = cmd.spawn() {
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
            }
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        let _ = urgency;

        // Pass title & body via environment variables so arbitrary characters
        // (quotes, `$`, backticks, newlines) cannot break PowerShell quoting.
        let ps_script = r#"
$t = [System.Security.SecurityElement]::Escape($env:SOUNDHOP_NOTIFY_TITLE)
$b = [System.Security.SecurityElement]::Escape($env:SOUNDHOP_NOTIFY_BODY)
try {
    [Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null
    [Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom.XmlDocument, ContentType = WindowsRuntime] | Out-Null
    $xml = New-Object Windows.Data.Xml.Dom.XmlDocument
    $xml.LoadXml("<toast><visual><binding template='ToastText02'><text id='1'>$t</text><text id='2'>$b</text></binding></visual></toast>")
    $toast = [Windows.UI.Notifications.ToastNotification]::new($xml)
    [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('SoundHop').Show($toast)
} catch {
    try {
        Add-Type -AssemblyName System.Windows.Forms
        $n = New-Object System.Windows.Forms.NotifyIcon
        $n.Icon = [System.Drawing.SystemIcons]::Information
        $n.Visible = $true
        $n.ShowBalloonTip(4000, $env:SOUNDHOP_NOTIFY_TITLE, $env:SOUNDHOP_NOTIFY_BODY, [System.Windows.Forms.ToolTipIcon]::Info)
        Start-Sleep -Milliseconds 4200
        $n.Dispose()
    } catch {}
}
"#;
        let mut cmd = Command::new("powershell.exe");
        cmd.args([
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            ps_script,
        ])
        .env("SOUNDHOP_NOTIFY_TITLE", title)
        .env("SOUNDHOP_NOTIFY_BODY", body)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW);
        if let Ok(mut child) = cmd.spawn() {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

static SIGNAL_STOP_FLAG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn handle_posix_signal(_sig: libc::c_int) {
    SIGNAL_STOP_FLAG.store(true, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(windows)]
static WINDOWS_CTRL_HANDLER_INSTALLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(windows)]
unsafe extern "system" fn handle_windows_console_ctrl(ctrl_type: u32) -> i32 {
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
    };
    match ctrl_type {
        CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT
        | CTRL_SHUTDOWN_EVENT => {
            SIGNAL_STOP_FLAG.store(true, std::sync::atomic::Ordering::SeqCst);
            1
        }
        _ => 0,
    }
}

pub fn install_signal_handlers() {
    SIGNAL_STOP_FLAG.store(false, std::sync::atomic::Ordering::SeqCst);
    #[cfg(unix)]
    unsafe {
        libc::signal(
            libc::SIGINT,
            handle_posix_signal as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGTERM,
            handle_posix_signal as *const () as libc::sighandler_t,
        );
    }
    #[cfg(windows)]
    {
        if !WINDOWS_CTRL_HANDLER_INSTALLED.swap(true, std::sync::atomic::Ordering::SeqCst) {
            unsafe {
                windows_sys::Win32::System::Console::SetConsoleCtrlHandler(
                    Some(handle_windows_console_ctrl),
                    1,
                );
            }
        }
    }
}

pub fn signal_stop_requested() -> bool {
    SIGNAL_STOP_FLAG.load(std::sync::atomic::Ordering::SeqCst)
}

pub fn dedupe(values: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for val in values {
        if !val.is_empty() && seen.insert(val.clone()) {
            out.push(val);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_slugify_and_sanitize() {
        assert_eq!(slugify("Living Room PC!", 32), "living_room_pc");
        assert_eq!(slugify("   ---   ", 32), "device");
        assert_eq!(
            sanitize_sink_name("soundhop_my_pc_123", 64),
            "soundhop_my_pc_123"
        );
        assert_eq!(sanitize_sink_name("bad/name:here!", 64), "bad_name_here");
    }

    #[test]
    fn test_tcp_server_arg_and_split_host_port() {
        assert_eq!(
            tcp_server_arg("192.168.1.50", 4713),
            "tcp:192.168.1.50:4713"
        );
        assert_eq!(tcp_server_arg("fe80::1%eth0", 4713), "tcp:[fe80::1]:4713");
        assert_eq!(tcp_server_arg("[::1]", 4713), "tcp:[::1]:4713");

        assert_eq!(
            split_host_port("10.0.0.5:4800", 4713),
            ("10.0.0.5".to_string(), 4800)
        );
        assert_eq!(
            split_host_port("10.0.0.5", 4713),
            ("10.0.0.5".to_string(), 4713)
        );
        assert_eq!(
            split_host_port("[fd00::1]:4900", 4713),
            ("fd00::1".to_string(), 4900)
        );
    }

    #[test]
    fn test_private_addresses() {
        assert!(is_private_address("127.0.0.1"));
        assert!(is_private_address("192.168.1.10"));
        assert!(is_private_address("10.1.2.3"));
        assert!(is_private_address("172.16.5.4"));
        assert!(is_private_address("::1"));
        assert!(is_private_address("fd00::1234"));
        assert!(!is_private_address("8.8.8.8"));
        assert!(!is_private_address("1.1.1.1"));
        assert!(!is_private_address(""));
    }

    #[test]
    fn test_atomic_write_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("nested").join("test.toml");
        atomic_write(&target, "hello = true\n").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "hello = true\n");
    }

    #[test]
    fn test_format_helpers() {
        assert_eq!(format_age(None), "never");
        assert_eq!(format_age(Some(2.0)), "just now");
        assert_eq!(format_age(Some(45.0)), "45s ago");
        assert_eq!(format_duration(Some(65.0)), "1m 05s");
        assert_eq!(format_duration(Some(3661.0)), "1h 01m");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KiB");
    }

    #[test]
    fn test_virtual_bridge_filtering_and_peer_ip_selection() {
        assert!(is_virtual_bridge_interface("docker0"));
        assert!(is_virtual_bridge_interface("br-0c060ea37032"));
        assert!(is_virtual_bridge_interface("veth1234"));
        assert!(is_virtual_bridge_interface("virbr0"));
        assert!(is_virtual_bridge_interface("podman0"));
        assert!(!is_virtual_bridge_interface("wlp0s20f3"));
        assert!(!is_virtual_bridge_interface("eth0"));
        assert!(!is_virtual_bridge_interface("enp3s0"));

        let candidates: Vec<IpAddr> = vec![
            "172.18.0.250".parse().unwrap(),
            "192.168.1.250".parse().unwrap(),
        ];
        assert_eq!(
            select_best_peer_ip(&candidates).as_deref(),
            Some("192.168.1.250")
        );
    }

    #[test]
    fn test_lock_or_recover_recovers_from_poison() {
        let mutex = Arc::new(Mutex::new(41_u32));
        let poisoned = Arc::clone(&mutex);
        let handle = std::thread::spawn(move || {
            let mut guard = poisoned.lock().unwrap();
            *guard = 42;
            panic!("intentional panic to poison mutex");
        });
        assert!(handle.join().is_err());
        assert!(mutex.is_poisoned());

        let mut recovered = lock_or_recover(&mutex);
        assert_eq!(*recovered, 42);
        *recovered = 99;
        drop(recovered);

        assert_eq!(*lock_or_recover(&mutex), 99);
    }

    #[test]
    fn test_try_acquire_instance_lock() {
        let lock1 = try_acquire_instance_lock("test_single_instance");
        assert!(lock1.is_some());
        let info = read_instance_lock_info("test_single_instance")
            .expect("lock metadata should be written on acquisition");
        assert_eq!(info.pid, Some(u64::from(std::process::id())));
        assert_eq!(info.version, VERSION);

        // Second acquisition while lock1 is held should fail without wiping metadata
        let lock2 = try_acquire_instance_lock("test_single_instance");
        assert!(lock2.is_none());
        let info_after = read_instance_lock_info("test_single_instance")
            .expect("failed acquisition must preserve lock holder metadata");
        assert_eq!(info_after, info);

        // Dropping lock1 frees the lock
        drop(lock1);
        let lock3 = try_acquire_instance_lock("test_single_instance");
        assert!(lock3.is_some());
    }

    #[cfg(unix)]
    #[test]
    fn test_system_runner_timeout_kills_hung_child() {
        let runner = SystemRunner;
        let args = vec![
            "sh".to_string(),
            "-c".to_string(),
            "sleep 5; true".to_string(),
        ];
        let start = Instant::now();
        let res = runner
            .run(&args, None, Duration::from_millis(80))
            .expect("spawn sh");
        assert_eq!(res.returncode, 124);
        assert!(res.stderr.contains("timed out"));
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn test_ensure_dir_rejects_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real");
        fs::create_dir_all(&target).unwrap();
        let link = dir.path().join("symlink_dir");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let res = ensure_dir(&link);
        assert!(res.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn test_close_fds_pre_exec_preserves_exec_error_reporting() {
        let mut cmd = Command::new("/nonexistent/soundhop-missing-binary-for-test");
        close_fds_pre_exec(&mut cmd);
        let err = cmd
            .spawn()
            .expect_err("spawning a nonexistent binary must return Err(NotFound)");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn test_windows_virtual_adapter_filtering() {
        assert!(is_virtual_bridge_interface("vEthernet (WSL)"));
        assert!(is_virtual_bridge_interface("vEthernet (Default Switch)"));
        assert!(is_virtual_bridge_interface("VMware Network Adapter VMnet8"));
        assert!(is_virtual_bridge_interface("VirtualBox Host-Only Network"));
        assert!(is_virtual_bridge_interface("Npcap Loopback Adapter"));
        assert!(is_virtual_bridge_interface(
            "Hyper-V Virtual Ethernet Adapter"
        ));
        assert!(is_virtual_bridge_interface("Loopback Pseudo-Interface 1"));
        assert!(!is_virtual_bridge_interface("Ethernet"));
        assert!(!is_virtual_bridge_interface("Wi-Fi"));
        assert!(!is_virtual_bridge_interface("Ethernet 2"));
    }

    #[test]
    fn test_daemon_log_append_and_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let log_file = dir.path().join("soundhop.log");
        append_log_line_to(&log_file, "INFO", "first line", 64);
        let content1 = fs::read_to_string(&log_file).unwrap();
        assert!(content1.contains("[INFO] first line"));

        // Exceed small rotation threshold (64 bytes) and ensure rotation creates `.log.old`.
        append_log_line_to(
            &log_file,
            "WARN",
            "a much longer second line that pushes the log over the 64 byte limit",
            64,
        );
        append_log_line_to(&log_file, "INFO", "after rotation", 64);
        let content2 = fs::read_to_string(&log_file).unwrap();
        assert!(content2.contains("after rotation"));
        assert!(log_file.with_extension("log.old").exists());
    }

    #[cfg(windows)]
    #[test]
    fn test_windows_runtime_dir_prefers_localappdata() {
        if env::var_os("XDG_RUNTIME_DIR").is_none() && env::var_os("LOCALAPPDATA").is_some() {
            let rt = runtime_dir();
            assert!(
                rt.ends_with(Path::new(APP_SLUG).join("run")),
                "Expected runtime_dir() to end with {APP_SLUG}\\run, got {}",
                rt.display()
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn test_windows_install_signal_handlers() {
        install_signal_handlers();
        assert!(!signal_stop_requested());
        // Calling a second time must be idempotent.
        install_signal_handlers();
        assert!(!signal_stop_requested());
    }
}
