//! Cross-platform utilities: XDG/Win32 paths, atomic file writes, process execution,
//! string formatting, network address inspection, and desktop notifications.

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ipnet::Ipv4Net;
use rand::Rng;
use thiserror::Error;

use crate::constants::APP_SLUG;

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

impl ProcessRunner for SystemRunner {
    fn run(
        &self,
        args: &[String],
        env_vars: Option<&HashMap<String, String>>,
        timeout: Duration,
    ) -> Result<CommandResult, UtilError> {
        let Some(program) = args.first() else {
            return Err(UtilError::ToolMissing(String::new()));
        };
        let mut cmd = Command::new(program);
        if args.len() > 1 {
            cmd.args(&args[1..]);
        }
        cmd.stdin(Stdio::null())
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
                        let _ = child.kill();
                        let _ = child.wait();
                        break 124;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(err) => {
                    let _ = child.kill();
                    let _ = child.wait();
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
        self.calls.lock().unwrap().clone()
    }
}

impl ProcessRunner for MockRunner {
    fn run(
        &self,
        args: &[String],
        env_vars: Option<&HashMap<String, String>>,
        _timeout: Duration,
    ) -> Result<CommandResult, UtilError> {
        self.calls.lock().unwrap().push(args.to_vec());
        (self.handler)(args, env_vars)
    }

    fn command_exists(&self, name: &str) -> bool {
        self.available_commands.lock().unwrap().contains(name)
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
    if let Some(h) = env::var_os("HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(h);
    }
    if let Some(up) = env::var_os("USERPROFILE").filter(|v| !v.is_empty()) {
        return PathBuf::from(up);
    }
    env::temp_dir()
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
    #[cfg(unix)]
    let uid = unsafe { libc::getuid() }.to_string();
    #[cfg(not(unix))]
    let uid = env::var("USERNAME").unwrap_or_else(|_| "user".to_string());
    env::temp_dir().join(format!("{APP_SLUG}-{uid}"))
}

pub fn ensure_dir(path: &Path) -> std::io::Result<PathBuf> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
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

/// Closes non-standard file descriptors (>= 3) on Unix before executing child processes.
/// This prevents leaking parent file descriptors (such as AppImage FUSE mount directory
/// file descriptors like FD 1023) into child processes (pactl, parec, scrcpy, notify-send).
#[cfg(unix)]
pub fn close_fds_pre_exec(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(|| {
            if libc::close_range(3, !0, 0) != 0 {
                for fd in 3..1024 {
                    libc::close(fd);
                }
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
pub fn close_fds_pre_exec(_cmd: &mut Command) {}

/// Single-instance process lock backed by kernel flock on Unix and exclusive sharing on Windows.
pub struct InstanceLock {
    _file: fs::File,
    _path: PathBuf,
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            unsafe {
                libc::flock(self._file.as_raw_fd(), libc::LOCK_UN);
            }
        }
    }
}

pub fn try_acquire_instance_lock(name: &str) -> Option<InstanceLock> {
    let dir = runtime_dir();
    let _ = ensure_dir(&dir);
    let lock_path = dir.join(format!("{name}.lock"));
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

    Some(InstanceLock {
        _file: file,
        _path: lock_path,
    })
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
        "docker", "br-", "veth", "virbr", "podman", "cni", "flannel", "cali", "lxcbr", "vboxnet",
        "vmnet", "tunl", "sit",
    ] {
        if lower.starts_with(prefix) {
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
    let clean = clean.split('%').next().unwrap_or(clean);
    let addr: SocketAddr = if let Ok(ip) = clean.parse::<IpAddr>() {
        SocketAddr::new(ip, port)
    } else {
        (clean, port).to_socket_addrs().ok()?.next()?
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
        let _ = (title, body, urgency);
    }
}

static SIGNAL_STOP_FLAG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn handle_posix_signal(_sig: libc::c_int) {
    SIGNAL_STOP_FLAG.store(true, std::sync::atomic::Ordering::SeqCst);
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
    fn test_try_acquire_instance_lock() {
        let lock1 = try_acquire_instance_lock("test_single_instance");
        assert!(lock1.is_some());
        // Second acquisition while lock1 is held should fail
        let lock2 = try_acquire_instance_lock("test_single_instance");
        assert!(lock2.is_none());
        // Dropping lock1 frees the lock
        drop(lock1);
        let lock3 = try_acquire_instance_lock("test_single_instance");
        assert!(lock3.is_some());
    }

    #[cfg(unix)]
    #[test]
    fn test_system_runner_timeout_kills_hung_child() {
        let runner = SystemRunner;
        let args = vec!["sh".to_string(), "-c".to_string(), "sleep 5".to_string()];
        let start = Instant::now();
        let res = runner
            .run(&args, None, Duration::from_millis(80))
            .expect("spawn sh");
        assert_eq!(res.returncode, 124);
        assert!(res.stderr.contains("timed out"));
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}
