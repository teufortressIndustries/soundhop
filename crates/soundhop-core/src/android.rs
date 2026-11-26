//! Android audio capture integration using `scrcpy` and `adb`.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use flate2::read::GzDecoder;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tar::Archive;
use thiserror::Error;

use crate::backend::AudioBackend;
use crate::constants::{
    ADB_MDNS_TCP_SERVICE_TYPE, ADB_MDNS_TLS_CONNECT_SERVICE_TYPE, DEFAULT_AUDIO_BIT_RATE,
    DEFAULT_AUDIO_BUFFER_MS, DEFAULT_AUDIO_CODEC, DEFAULT_SCRCPY_VERSION,
    SCRCPY_V3_1_SHA256_LINUX_AARCH64, SCRCPY_V3_1_SHA256_LINUX_X86_64, SCRCPY_V3_1_SHA256_WIN64,
    SUPPORTED_AUDIO_CODECS, SUPPORTED_AUDIO_SOURCES,
};
use crate::util::{
    data_dir, home_dir, is_executable, is_own_non_loopback_ip, is_private_address, lock_or_recover,
    now_secs, select_best_peer_ip, which_binary, CommandResult, ProcessRunner, SystemRunner,
};

#[derive(Debug, Error)]
pub enum AndroidError {
    #[error("{0}")]
    Message(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdbDevice {
    pub serial: String,
    pub state: String,
    pub model: String,
    pub product: String,
    pub device: String,
    pub transport_id: String,
    pub is_wifi: bool,
    pub ip: String,
    pub port: u16,
    pub android_version: String,
    pub sdk_version: u32,
    pub manufacturer: String,
}

impl AdbDevice {
    pub fn is_ready(&self) -> bool {
        self.state == "device"
    }

    pub fn display_name(&self) -> String {
        let mut parts = Vec::new();
        if !self.manufacturer.is_empty() {
            parts.push(self.manufacturer.as_str());
        }
        if !self.model.is_empty() && self.model != self.manufacturer {
            parts.push(self.model.as_str());
        }
        if parts.is_empty() {
            self.serial.clone()
        } else {
            parts.join(" ")
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AudioEncoderInfo {
    pub codecs: BTreeMap<String, Vec<String>>,
    pub raw_output: String,
}

impl AudioEncoderInfo {
    pub fn available_codecs(&self) -> Vec<String> {
        SUPPORTED_AUDIO_CODECS
            .iter()
            .filter(|&&c| self.codecs.contains_key(c) || c == "raw")
            .map(|&c| c.to_string())
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolStatus {
    pub scrcpy_path: Option<PathBuf>,
    pub scrcpy_version: String,
    pub adb_path: Option<PathBuf>,
    pub adb_version: String,
    pub is_managed: bool,
}

impl ToolStatus {
    pub fn adb_ready(&self) -> bool {
        self.adb_path.is_some()
    }

    pub fn scrcpy_ready(&self) -> bool {
        self.scrcpy_path.is_some() && self.adb_path.is_some()
    }

    pub fn ready(&self) -> bool {
        self.scrcpy_ready()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiscoveredAdbDevice {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub service_type: String,
    pub tls: bool,
}

impl DiscoveredAdbDevice {
    pub fn endpoint(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WifiSwitchOutcome {
    pub usb_serial: String,
    pub wifi_serial: String,
    pub ip: String,
    pub port: u16,
    pub model: String,
    pub manufacturer: String,
}

/// Resolve the required SHA-256 checksum for a `scrcpy` download.
///
/// Fails closed if no pinned checksum exists for the requested OS/architecture/version
/// and no explicit `expected_sha256` was supplied by the caller.
pub fn resolve_scrcpy_sha256<'a>(
    target_os: &str,
    target_arch: &str,
    version: &str,
    expected_sha256: Option<&'a str>,
) -> Result<&'a str, AndroidError> {
    if let Some(explicit) = expected_sha256.map(str::trim).filter(|s| !s.is_empty()) {
        return Ok(explicit);
    }
    if version == DEFAULT_SCRCPY_VERSION {
        if target_os == "windows" {
            return Ok(SCRCPY_V3_1_SHA256_WIN64);
        }
        if target_os == "linux" && target_arch == "x86_64" {
            return Ok(SCRCPY_V3_1_SHA256_LINUX_X86_64);
        }
        if target_os == "linux" && target_arch == "aarch64" {
            if let Some(pinned) = SCRCPY_V3_1_SHA256_LINUX_AARCH64 {
                return Ok(pinned);
            }
            return Err(AndroidError::Message(format!(
                "Refusing unverified scrcpy v{version} download on linux/aarch64: no pinned SHA-256 checksum exists (upstream scrcpy v{version} does not publish a verified Linux aarch64 tarball). Install system `scrcpy` via your package manager, configure `android.scrcpy_path`, or provide an explicit verified SHA-256 checksum."
            )));
        }
    }
    Err(AndroidError::Message(format!(
        "Refusing unverified scrcpy v{version} download on {target_os}/{target_arch}: no pinned SHA-256 checksum exists. Install system `scrcpy`, configure `android.scrcpy_path`, or supply an explicit verified SHA-256 checksum."
    )))
}

pub fn verify_scrcpy_sha256(actual_hex: &str, expected_hex: &str) -> Result<(), AndroidError> {
    let actual_lower = actual_hex.trim().to_ascii_lowercase();
    let expected_lower = expected_hex.trim().to_ascii_lowercase();
    if expected_lower.is_empty()
        || !crate::security::constant_time_eq(actual_lower.as_bytes(), expected_lower.as_bytes())
    {
        return Err(AndroidError::Message(format!(
            "SHA-256 checksum mismatch: expected {expected_hex}, got {actual_hex}"
        )));
    }
    Ok(())
}

pub fn parse_adb_mdns_service_info(info: &ServiceInfo) -> Option<DiscoveredAdbDevice> {
    let port = info.get_port();
    if port == 0 {
        return None;
    }
    let host = select_best_peer_ip(info.get_addresses())?;
    if !is_private_address(&host) || is_own_non_loopback_ip(&host) {
        return None;
    }
    let service_type = info.get_type().to_string();
    let tls = service_type == ADB_MDNS_TLS_CONNECT_SERVICE_TYPE;
    let raw_full = info.get_fullname();
    let name = raw_full
        .strip_suffix(&format!(".{service_type}"))
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| info.get_hostname().trim_end_matches('.'))
        .to_string();
    Some(DiscoveredAdbDevice {
        name,
        host,
        port,
        service_type,
        tls,
    })
}

pub fn parse_adb_mdns_services_output(stdout: &str) -> Vec<DiscoveredAdbDevice> {
    let mut devices = Vec::new();
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("List of discovered") {
            continue;
        }
        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        if parts.len() < 3 {
            continue;
        }
        let name = parts[0].to_string();
        let raw_service = parts[1];
        let endpoint = parts[2];
        let service_type = if raw_service.contains("_adb-tls-connect._tcp") {
            ADB_MDNS_TLS_CONNECT_SERVICE_TYPE.to_string()
        } else if raw_service.contains("_adb._tcp") {
            ADB_MDNS_TCP_SERVICE_TYPE.to_string()
        } else {
            continue;
        };
        let Some((host, port_str)) = endpoint.rsplit_once(':') else {
            continue;
        };
        let host = host.trim_matches(|c| c == '[' || c == ']');
        let Ok(port) = port_str.parse::<u16>() else {
            continue;
        };
        if port == 0 || !is_private_address(host) || is_own_non_loopback_ip(host) {
            continue;
        }
        let tls = service_type == ADB_MDNS_TLS_CONNECT_SERVICE_TYPE;
        devices.push(DiscoveredAdbDevice {
            name,
            host: host.to_string(),
            port,
            service_type,
            tls,
        });
    }
    devices
}

/// Background mDNS browser for wireless Android ADB endpoints
/// (`_adb-tls-connect._tcp.local.` and `_adb._tcp.local.`).
pub struct AdbMdnsBrowser {
    devices: Arc<Mutex<BTreeMap<String, DiscoveredAdbDevice>>>,
    stop_flag: Arc<AtomicBool>,
    daemon: Mutex<Option<ServiceDaemon>>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

impl Default for AdbMdnsBrowser {
    fn default() -> Self {
        Self::new()
    }
}

impl AdbMdnsBrowser {
    pub fn new() -> Self {
        Self {
            devices: Arc::new(Mutex::new(BTreeMap::new())),
            stop_flag: Arc::new(AtomicBool::new(false)),
            daemon: Mutex::new(None),
            threads: Mutex::new(Vec::new()),
        }
    }

    pub fn start(&self) -> Result<(), AndroidError> {
        self.stop();
        self.stop_flag.store(false, Ordering::SeqCst);
        let mdns = ServiceDaemon::new().map_err(|e| {
            AndroidError::Message(format!("Failed to start Android ADB mDNS browser: {e}"))
        })?;
        let mut handles = Vec::new();
        for svc_type in [ADB_MDNS_TLS_CONNECT_SERVICE_TYPE, ADB_MDNS_TCP_SERVICE_TYPE] {
            if let Ok(receiver) = mdns.browse(svc_type) {
                let stop = Arc::clone(&self.stop_flag);
                let map = Arc::clone(&self.devices);
                let handle = thread::spawn(move || {
                    while !stop.load(Ordering::SeqCst) {
                        match receiver.recv_timeout(Duration::from_millis(250)) {
                            Ok(ServiceEvent::ServiceResolved(info)) => {
                                if let Some(dev) = parse_adb_mdns_service_info(&info) {
                                    lock_or_recover(&map).insert(dev.endpoint(), dev);
                                }
                            }
                            Ok(ServiceEvent::ServiceRemoved(_, fullname)) => {
                                lock_or_recover(&map).retain(|_, d| !fullname.starts_with(&d.name));
                            }
                            _ => {}
                        }
                    }
                });
                handles.push(handle);
            }
        }
        *lock_or_recover(&self.daemon) = Some(mdns);
        *lock_or_recover(&self.threads) = handles;
        Ok(())
    }

    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(mdns) = lock_or_recover(&self.daemon).take() {
            let _ = mdns.shutdown();
        }
        let handles: Vec<JoinHandle<()>> = std::mem::take(&mut *lock_or_recover(&self.threads));
        for h in handles {
            let _ = h.join();
        }
    }

    pub fn record_device(&self, dev: DiscoveredAdbDevice) {
        lock_or_recover(&self.devices).insert(dev.endpoint(), dev);
    }

    pub fn devices(&self) -> Vec<DiscoveredAdbDevice> {
        lock_or_recover(&self.devices).values().cloned().collect()
    }
}

impl Drop for AdbMdnsBrowser {
    fn drop(&mut self) {
        self.stop();
    }
}

pub struct AndroidManager {
    pub custom_scrcpy: String,
    pub custom_adb: String,
    runner: Arc<dyn ProcessRunner>,
    reverse_tunnels: Mutex<BTreeSet<(String, u16)>>,
}

impl AndroidManager {
    pub fn new(scrcpy_path: &str, adb_path: &str) -> Self {
        Self {
            custom_scrcpy: scrcpy_path.trim().to_string(),
            custom_adb: adb_path.trim().to_string(),
            runner: Arc::new(SystemRunner),
            reverse_tunnels: Mutex::new(BTreeSet::new()),
        }
    }

    pub fn with_runner(scrcpy_path: &str, adb_path: &str, runner: Arc<dyn ProcessRunner>) -> Self {
        Self {
            custom_scrcpy: scrcpy_path.trim().to_string(),
            custom_adb: adb_path.trim().to_string(),
            runner,
            reverse_tunnels: Mutex::new(BTreeSet::new()),
        }
    }

    pub fn managed_dir() -> PathBuf {
        data_dir().join("scrcpy")
    }

    pub fn find_scrcpy(&self) -> Option<PathBuf> {
        let mut candidates = Vec::new();
        if !self.custom_scrcpy.is_empty() {
            candidates.push(PathBuf::from(&self.custom_scrcpy));
        }
        let names: &[&str] = if cfg!(windows) {
            &["scrcpy.exe", "scrcpy"]
        } else {
            &["scrcpy"]
        };
        let managed = Self::managed_dir();
        for &n in names {
            candidates.push(managed.join(n));
        }
        let home = home_dir();
        for &n in names {
            candidates.push(home.join(".local").join("bin").join(n));
            candidates.push(home.join("Applications").join("scrcpy").join(n));
        }
        for c in candidates {
            if c.is_file() && is_executable(&c) {
                return Some(c);
            }
        }
        for &n in names {
            if let Some(found) = which_binary(n) {
                return Some(found);
            }
        }
        None
    }

    pub fn find_adb(&self) -> Option<PathBuf> {
        let mut candidates = Vec::new();
        if !self.custom_adb.is_empty() {
            candidates.push(PathBuf::from(&self.custom_adb));
        }
        let names: &[&str] = if cfg!(windows) {
            &["adb.exe", "adb"]
        } else {
            &["adb"]
        };
        if let Some(scrcpy_bin) = self.find_scrcpy() {
            if let Some(parent) = scrcpy_bin.parent() {
                for &n in names {
                    candidates.push(parent.join(n));
                }
            }
        }
        let managed = Self::managed_dir();
        for &n in names {
            candidates.push(managed.join(n));
        }
        let home = home_dir();
        for &n in names {
            candidates.push(home.join(".local").join("bin").join(n));
        }
        for c in candidates {
            if c.is_file() && is_executable(&c) {
                return Some(c);
            }
        }
        for &n in names {
            if let Some(found) = which_binary(n) {
                return Some(found);
            }
        }
        None
    }

    pub fn get_tool_status(&self) -> ToolStatus {
        let scrcpy_bin = self.find_scrcpy();
        let adb_bin = self.find_adb();
        let mut scrcpy_version = String::new();
        let mut adb_version = String::new();
        let mut is_managed = false;

        if let Some(ref sbin) = scrcpy_bin {
            let managed_str = Self::managed_dir().to_string_lossy().to_string();
            is_managed = sbin.to_string_lossy().contains(&managed_str);
            let args = vec![sbin.to_string_lossy().to_string(), "--version".to_string()];
            if let Ok(res) = self.runner.run(&args, None, Duration::from_secs(3)) {
                let first = res.stdout.lines().next().unwrap_or("").trim();
                scrcpy_version = first
                    .strip_prefix("scrcpy ")
                    .and_then(|r| r.split_whitespace().next())
                    .unwrap_or(first)
                    .to_string();
            }
        }

        if let Some(ref abin) = adb_bin {
            let args = vec![abin.to_string_lossy().to_string(), "version".to_string()];
            if let Ok(res) = self.runner.run(&args, None, Duration::from_secs(3)) {
                for line in res.stdout.lines() {
                    if let Some(rest) = line.trim().strip_prefix("Android Debug Bridge version ") {
                        adb_version = rest.trim().to_string();
                        break;
                    }
                }
                if adb_version.is_empty() {
                    adb_version = "installed".to_string();
                }
            }
        }

        ToolStatus {
            scrcpy_path: scrcpy_bin,
            scrcpy_version,
            adb_path: adb_bin,
            adb_version,
            is_managed,
        }
    }

    pub fn install_scrcpy(
        &self,
        version: Option<&str>,
        expected_sha256: Option<&str>,
    ) -> Result<PathBuf, AndroidError> {
        let ver = version.unwrap_or(DEFAULT_SCRCPY_VERSION);
        // Fail closed before performing any network download if no pinned checksum exists.
        let pinned_sha256 = resolve_scrcpy_sha256(
            std::env::consts::OS,
            std::env::consts::ARCH,
            ver,
            expected_sha256,
        )?;
        let dest_dir = Self::managed_dir();
        fs::create_dir_all(&dest_dir)?;

        let (url, archive_path) = if cfg!(windows) {
            (
                format!(
                    "https://github.com/Genymobile/scrcpy/releases/download/v{ver}/scrcpy-win64-v{ver}.zip"
                ),
                dest_dir.join(format!("scrcpy-win64-{ver}.zip")),
            )
        } else {
            let arch = match std::env::consts::ARCH {
                "x86_64" => "x86_64",
                "aarch64" => "aarch64",
                other => {
                    return Err(AndroidError::Message(format!(
                        "Unsupported architecture for prebuilt scrcpy: {other}"
                    )))
                }
            };
            (
                format!(
                    "https://github.com/Genymobile/scrcpy/releases/download/v{ver}/scrcpy-linux-{arch}-v{ver}.tar.gz"
                ),
                dest_dir.join(format!("scrcpy-{arch}-{ver}.tar.gz")),
            )
        };

        if !url.starts_with("https://") {
            return Err(AndroidError::Message(format!(
                "Refusing non-HTTPS download URL: {url}"
            )));
        }

        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(60))
            .build();
        let resp = agent
            .get(&url)
            .set("User-Agent", "SoundHop-AndroidManager")
            .call()
            .map_err(|e| AndroidError::Message(format!("Failed to download {url}: {e}")))?;

        const MAX_ARCHIVE_BYTES: u64 = 100 * 1024 * 1024;
        let mut reader = resp.into_reader().take(MAX_ARCHIVE_BYTES + 1);
        let mut out_file = File::create(&archive_path)?;
        let mut hasher = Sha256::new();
        let mut total_read: u64 = 0;
        let mut buf = [0u8; 65536];
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            total_read += n as u64;
            if total_read > MAX_ARCHIVE_BYTES {
                drop(out_file);
                let _ = fs::remove_file(&archive_path);
                return Err(AndroidError::Message(
                    "Downloaded archive exceeded 100 MiB safety limit".to_string(),
                ));
            }
            out_file.write_all(&buf[..n])?;
            hasher.update(&buf[..n]);
        }
        drop(out_file);

        let actual = hex::encode(hasher.finalize());
        if let Err(err) = verify_scrcpy_sha256(&actual, pinned_sha256) {
            let _ = fs::remove_file(&archive_path);
            return Err(err);
        }

        let extract_res = if cfg!(windows) {
            extract_zip_safely(&archive_path, &dest_dir)
        } else {
            extract_tar_gz_safely(&archive_path, &dest_dir)
        };
        let _ = fs::remove_file(&archive_path);
        extract_res?;

        self.find_scrcpy().ok_or_else(|| {
            AndroidError::Message("scrcpy binary not found after extraction".to_string())
        })
    }

    fn run_adb(&self, args: &[&str], serial: Option<&str>) -> Result<CommandResult, AndroidError> {
        let adb_bin = self.find_adb().ok_or_else(|| {
            AndroidError::Message(
                "adb command not found. Please install adb or run 'soundhop android install'."
                    .to_string(),
            )
        })?;
        let mut full = vec![adb_bin.to_string_lossy().to_string()];
        if let Some(s) = serial.filter(|s| !s.trim().is_empty()) {
            let clean = validate_adb_target(s)?;
            full.push("-s".to_string());
            full.push(clean.to_string());
        }
        for a in args {
            full.push((*a).to_string());
        }
        self.runner
            .run(&full, None, Duration::from_secs(10))
            .map_err(|e| AndroidError::Message(e.to_string()))
    }

    pub fn list_devices(&self) -> Vec<AdbDevice> {
        let Ok(res) = self.run_adb(&["devices", "-l"], None) else {
            return Vec::new();
        };
        parse_adb_devices_output(&res.stdout)
    }

    pub fn get_device_wifi_ip(&self, serial: &str) -> Option<String> {
        if let Ok(res) = self.run_adb(&["shell", "ip -f inet addr show wlan0"], Some(serial)) {
            if let Some(ip) = extract_ipv4_after_inet(&res.stdout) {
                return Some(ip);
            }
        }
        if let Ok(res) = self.run_adb(&["shell", "ip route"], Some(serial)) {
            for line in res.stdout.lines() {
                if line.contains("wlan") && line.contains("src ") {
                    if let Some(after_src) = line.split("src ").nth(1) {
                        if let Some(tok) = after_src.split_whitespace().next() {
                            if tok.split('.').count() == 4 {
                                return Some(tok.to_string());
                            }
                        }
                    }
                }
            }
        }
        None
    }

    pub fn enable_tcpip(&self, serial: &str, port: u16) -> (bool, String) {
        let port_str = port.to_string();
        match self.run_adb(&["tcpip", &port_str], Some(serial)) {
            Ok(res) if res.ok() || res.stdout.contains("restarting in TCP mode") => {
                (true, format!("Device switched to TCP mode on port {port}"))
            }
            Ok(res) => (false, res.combined_output()),
            Err(e) => (false, e.to_string()),
        }
    }

    pub fn connect_wifi(&self, host: &str, port: u16) -> (bool, String) {
        let clean_host = match validate_adb_target(host) {
            Ok(h) => h,
            Err(e) => return (false, e.to_string()),
        };
        let target = if clean_host.contains(':') {
            clean_host.to_string()
        } else {
            format!("{clean_host}:{port}")
        };
        match self.run_adb(&["connect", &target], None) {
            Ok(res) => {
                let out = format!("{} {}", res.stdout, res.stderr);
                if out.contains("connected to") || out.contains("already connected") {
                    (true, format!("Connected to {target}"))
                } else {
                    (false, out.trim().to_string())
                }
            }
            Err(e) => (false, e.to_string()),
        }
    }

    pub fn disconnect_wifi(&self, target: Option<&str>) -> (bool, String) {
        let clean_target = match target.filter(|t| !t.trim().is_empty()) {
            Some(t) => match validate_adb_target(t) {
                Ok(c) => Some(c),
                Err(e) => return (false, e.to_string()),
            },
            None => None,
        };
        let tracked_to_clean: Vec<(String, u16)> = {
            let guard = lock_or_recover(&self.reverse_tunnels);
            guard
                .iter()
                .filter(|(serial, _)| match clean_target {
                    Some(t) => serial == t || (!t.contains(':') && serial == &format!("{t}:5555")),
                    None => serial.contains(':') || serial.contains("._adb"),
                })
                .cloned()
                .collect()
        };
        for (serial, port) in &tracked_to_clean {
            let _ = self.remove_reverse_tunnel(serial, Some(*port));
        }

        let args: Vec<&str> = match clean_target {
            Some(t) => vec!["disconnect", t],
            None => vec!["disconnect"],
        };
        match self.run_adb(&args, None) {
            Ok(res) => {
                if !tracked_to_clean.is_empty() {
                    let mut guard = lock_or_recover(&self.reverse_tunnels);
                    for entry in &tracked_to_clean {
                        guard.remove(entry);
                    }
                }
                (true, res.stdout.trim().to_string())
            }
            Err(e) => (false, e.to_string()),
        }
    }

    pub fn setup_reverse_tunnel(
        &self,
        serial: &str,
        remote_port: u16,
        local_port: u16,
    ) -> Result<(), AndroidError> {
        let clean_serial = validate_adb_target(serial)?;
        if remote_port == 0 || local_port == 0 {
            return Err(AndroidError::Message(format!(
                "Invalid reverse tunnel ports: remote={remote_port}, local={local_port} (must be non-zero)"
            )));
        }
        let remote_spec = format!("tcp:{remote_port}");
        let local_spec = format!("tcp:{local_port}");
        let res = self.run_adb(&["reverse", &remote_spec, &local_spec], Some(clean_serial))?;
        if !res.ok() {
            return Err(adb_command_error(
                &format!("adb reverse {remote_spec} {local_spec}"),
                &res,
            ));
        }
        lock_or_recover(&self.reverse_tunnels).insert((clean_serial.to_string(), remote_port));
        Ok(())
    }

    pub fn remove_reverse_tunnel(
        &self,
        serial: &str,
        remote_port: Option<u16>,
    ) -> Result<(), AndroidError> {
        let clean_serial = validate_adb_target(serial)?;
        let port_spec = match remote_port {
            Some(0) => {
                return Err(AndroidError::Message(
                    "Invalid reverse tunnel remote port: 0 (must be non-zero)".to_string(),
                ));
            }
            Some(port) => Some(format!("tcp:{port}")),
            None => None,
        };
        let args: Vec<&str> = match port_spec.as_deref() {
            Some(spec) => vec!["reverse", "--remove", spec],
            None => vec!["reverse", "--remove-all"],
        };
        let clear_tracked = || {
            let mut guard = lock_or_recover(&self.reverse_tunnels);
            match remote_port {
                Some(port) => {
                    guard.remove(&(clean_serial.to_string(), port));
                }
                None => {
                    guard.retain(|(s, _)| s != clean_serial);
                }
            }
        };

        match self.run_adb(&args, Some(clean_serial)) {
            Ok(res) if res.ok() => {
                clear_tracked();
                Ok(())
            }
            Ok(res) => {
                let detail = res.combined_output();
                if is_benign_reverse_teardown_error(&detail) {
                    clear_tracked();
                    Ok(())
                } else {
                    Err(adb_command_error("adb reverse remove", &res))
                }
            }
            Err(e) => {
                if is_benign_reverse_teardown_error(&e.to_string()) {
                    clear_tracked();
                    Ok(())
                } else {
                    Err(e)
                }
            }
        }
    }

    pub fn list_reverse_tunnels(&self, serial: &str) -> Result<Vec<String>, AndroidError> {
        let clean_serial = validate_adb_target(serial)?;
        let res = self.run_adb(&["reverse", "--list"], Some(clean_serial))?;
        if !res.ok() {
            return Err(adb_command_error("adb reverse --list", &res));
        }
        Ok(res
            .stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToString::to_string)
            .collect())
    }

    pub fn tracked_reverse_tunnels(&self) -> Vec<(String, u16)> {
        lock_or_recover(&self.reverse_tunnels)
            .iter()
            .cloned()
            .collect()
    }

    pub fn open_browser_url(&self, serial: &str, url: &str) -> Result<(), AndroidError> {
        let clean_serial = validate_adb_target(serial)?;
        validate_local_browser_url(url)?;
        // Single-quote the validated URL (which cannot contain `'`) so characters
        // like `&` remain literal when `adb shell` invokes Android `/system/bin/sh`.
        let quoted_url = format!("'{url}'");
        let res = self.run_adb(
            &[
                "shell",
                "am",
                "start",
                "-a",
                "android.intent.action.VIEW",
                "-d",
                &quoted_url,
            ],
            Some(clean_serial),
        )?;
        if !res.ok() {
            return Err(adb_command_error("adb shell am start", &res));
        }
        Ok(())
    }

    /// Discover wireless Android ADB targets via `adb mdns services` and `mdns-sd`
    /// (`_adb-tls-connect._tcp.local.` and `_adb._tcp.local.`).
    ///
    /// Strictly gated by `enabled` (`config.android.mdns_discovery`).
    pub fn discover_mdns_devices(
        &self,
        enabled: bool,
        timeout: Duration,
    ) -> Vec<DiscoveredAdbDevice> {
        if !enabled {
            return Vec::new();
        }
        let mut found: BTreeMap<String, DiscoveredAdbDevice> = BTreeMap::new();
        if let Ok(res) = self.run_adb(&["mdns", "services"], None) {
            if res.ok() {
                for dev in parse_adb_mdns_services_output(&res.stdout) {
                    found.insert(dev.endpoint(), dev);
                }
            }
        }
        if !timeout.is_zero() {
            if let Ok(mdns) = ServiceDaemon::new() {
                let mut receivers = Vec::new();
                for svc_type in [ADB_MDNS_TLS_CONNECT_SERVICE_TYPE, ADB_MDNS_TCP_SERVICE_TYPE] {
                    if let Ok(rx) = mdns.browse(svc_type) {
                        receivers.push(rx);
                    }
                }
                let deadline = Instant::now() + timeout;
                while Instant::now() < deadline && !receivers.is_empty() {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    let step = remaining.min(Duration::from_millis(50));
                    for rx in &receivers {
                        if let Ok(ServiceEvent::ServiceResolved(info)) = rx.recv_timeout(step) {
                            if let Some(dev) = parse_adb_mdns_service_info(&info) {
                                found.insert(dev.endpoint(), dev);
                            }
                        }
                    }
                }
                let _ = mdns.shutdown();
            }
        }
        found.into_values().collect()
    }

    /// Automatically switch a ready USB Android device into ADB TCP/IP Wi-Fi mode.
    ///
    /// Strictly gated by `enabled` (`config.android.auto_switch_wifi`).
    pub fn auto_switch_usb_to_wifi(
        &self,
        dev: &AdbDevice,
        enabled: bool,
        port: u16,
    ) -> Result<Option<WifiSwitchOutcome>, AndroidError> {
        if !enabled || dev.is_wifi || !dev.is_ready() {
            return Ok(None);
        }
        let ip = self.get_device_wifi_ip(&dev.serial).ok_or_else(|| {
            AndroidError::Message(format!(
                "Could not determine Wi-Fi IP for USB Android device {}",
                dev.serial
            ))
        })?;
        let (tcp_ok, tcp_msg) = self.enable_tcpip(&dev.serial, port);
        if !tcp_ok {
            return Err(AndroidError::Message(format!(
                "Failed to enable ADB TCP/IP on {}: {tcp_msg}",
                dev.serial
            )));
        }
        let (conn_ok, conn_msg) = self.connect_wifi(&ip, port);
        if !conn_ok {
            return Err(AndroidError::Message(format!(
                "Failed to connect to {ip}:{port} over Wi-Fi: {conn_msg}"
            )));
        }
        Ok(Some(WifiSwitchOutcome {
            usb_serial: dev.serial.clone(),
            wifi_serial: format!("{ip}:{port}"),
            ip,
            port,
            model: dev.model.clone(),
            manufacturer: dev.manufacturer.clone(),
        }))
    }

    pub fn list_encoders(&self, serial: Option<&str>) -> Result<AudioEncoderInfo, AndroidError> {
        let scrcpy_bin = self
            .find_scrcpy()
            .ok_or_else(|| AndroidError::Message("scrcpy is not installed.".to_string()))?;
        let mut args = vec![
            scrcpy_bin.to_string_lossy().to_string(),
            "--list-encoders".to_string(),
        ];
        if let Some(s) = serial.filter(|s| !s.trim().is_empty()) {
            let clean = validate_adb_target(s)?;
            args.push("-s".to_string());
            args.push(clean.to_string());
        }
        let Ok(res) = self.runner.run(&args, None, Duration::from_secs(10)) else {
            return Ok(default_encoders());
        };
        let combined = format!("{}\n{}", res.stdout, res.stderr);
        Ok(parse_scrcpy_encoders(&combined))
    }

    pub fn build_capture_args(
        &self,
        serial: &str,
        codec: &str,
        source: Option<&str>,
        buffer_ms: Option<u32>,
        bit_rate: Option<u32>,
        duplicate: bool,
    ) -> Result<Vec<String>, AndroidError> {
        let clean_serial = validate_adb_target(serial)?;
        let scrcpy_bin = self
            .find_scrcpy()
            .ok_or_else(|| AndroidError::Message("scrcpy binary not found.".to_string()))?;
        let mut cmd = vec![
            scrcpy_bin.to_string_lossy().to_string(),
            "-s".to_string(),
            clean_serial.to_string(),
            "--no-video".to_string(),
            "--no-window".to_string(),
            "--no-control".to_string(),
            "--require-audio".to_string(),
        ];
        let codec_clean = codec.trim().to_lowercase();
        if SUPPORTED_AUDIO_CODECS.contains(&codec_clean.as_str()) {
            cmd.push(format!("--audio-codec={codec_clean}"));
        }
        if let Some(src) = source.map(str::trim).filter(|s| !s.is_empty()) {
            let src_clean = src.to_lowercase();
            if !SUPPORTED_AUDIO_SOURCES.contains(&src_clean.as_str()) {
                return Err(AndroidError::Message(format!(
                    "Unsupported Android audio source: {src}"
                )));
            }
            cmd.push(format!("--audio-source={src_clean}"));
        }
        if let Some(buf) = buffer_ms.filter(|&b| b > 0) {
            cmd.push(format!("--audio-buffer={buf}"));
        }
        if let Some(br) = bit_rate.filter(|&b| b > 0) {
            cmd.push(format!("--audio-bit-rate={br}"));
        }
        if duplicate {
            cmd.push("--audio-dup".to_string());
        }
        Ok(cmd)
    }
}

/// Validate an ADB device serial or `host[:port]` target to prevent CLI option injection.
pub fn validate_adb_target(value: &str) -> Result<&str, AndroidError> {
    let trimmed = value.trim();
    if trimmed.is_empty()
        || trimmed.starts_with('-')
        || trimmed.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(AndroidError::Message(format!(
            "Invalid ADB target or device serial: {value:?}"
        )));
    }
    Ok(trimmed)
}

fn validate_local_browser_url(url: &str) -> Result<(), AndroidError> {
    if url.is_empty()
        || !url.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '_' | '.' | '-' | '~' | ':' | '/' | '?' | '=' | '#' | '&')
        })
    {
        return Err(AndroidError::Message(format!(
            "Refusing browser URL with disallowed characters: {url:?}"
        )));
    }

    let rest = url
        .strip_prefix("http://localhost:")
        .or_else(|| url.strip_prefix("http://127.0.0.1:"))
        .ok_or_else(|| {
            AndroidError::Message(format!(
                "Browser URL must start with http://localhost:<port> or http://127.0.0.1:<port>: {url:?}"
            ))
        })?;

    let port_end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let port_str = &rest[..port_end];
    let after_port = &rest[port_end..];
    let valid_port = !port_str.is_empty()
        && !port_str.starts_with('0')
        && port_str.parse::<u16>().is_ok_and(|p| p > 0);
    let valid_boundary = after_port.is_empty()
        || after_port.starts_with('/')
        || after_port.starts_with('?')
        || after_port.starts_with('#');

    if !valid_port || !valid_boundary {
        return Err(AndroidError::Message(format!(
            "Browser URL has invalid localhost port or authority boundary: {url:?}"
        )));
    }

    Ok(())
}

fn is_benign_reverse_teardown_error(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    if lower.contains("adb command not found") {
        return false;
    }
    lower.contains("device offline")
        || lower.contains("device not found")
        || lower.contains("no devices")
        || (lower.contains("device '") && lower.contains("not found"))
        || (lower.contains("listener") && lower.contains("not found"))
        || (lower.contains("cannot remove") && lower.contains("not found"))
}

fn adb_command_error(context: &str, res: &CommandResult) -> AndroidError {
    let detail = res.combined_output();
    if detail.is_empty() {
        AndroidError::Message(format!(
            "{context} failed with exit code {}",
            res.returncode
        ))
    } else {
        AndroidError::Message(format!("{context} failed: {detail}"))
    }
}

pub fn parse_adb_devices_output(stdout: &str) -> Vec<AdbDevice> {
    let mut devices = Vec::new();
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("List of devices") || trimmed.starts_with('*')
        {
            continue;
        }
        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        if parts.len() < 2 {
            continue;
        }
        let serial = parts[0].to_string();
        let state = parts[1].to_string();
        let mut model = String::new();
        let mut product = String::new();
        let mut device_name = String::new();
        let mut transport_id = String::new();

        for &item in &parts[2..] {
            if let Some(v) = item.strip_prefix("model:") {
                model = v.to_string();
            } else if let Some(v) = item.strip_prefix("product:") {
                product = v.to_string();
            } else if let Some(v) = item.strip_prefix("device:") {
                device_name = v.to_string();
            } else if let Some(v) = item.strip_prefix("transport_id:") {
                transport_id = v.to_string();
            }
        }
        let is_wifi = serial.contains(':');
        let (ip, port) = if is_wifi {
            let mut sp = serial.splitn(2, ':');
            let h = sp.next().unwrap_or("").to_string();
            let p = sp
                .next()
                .and_then(|s| s.parse::<u16>().ok())
                .unwrap_or(5555);
            (h, p)
        } else {
            (String::new(), 5555)
        };
        devices.push(AdbDevice {
            serial,
            state,
            model,
            product,
            device: device_name,
            transport_id,
            is_wifi,
            ip,
            port,
            android_version: String::new(),
            sdk_version: 0,
            manufacturer: String::new(),
        });
    }
    devices
}

pub fn parse_scrcpy_encoders(output: &str) -> AudioEncoderInfo {
    let mut codecs: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut in_audio = false;
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.contains("List of audio encoders:") {
            in_audio = true;
            continue;
        }
        if !in_audio || trimmed.is_empty() || trimmed.starts_with("[server]") {
            continue;
        }
        let mut codec_name = None;
        let mut enc_name = None;
        for tok in trimmed.split_whitespace() {
            if let Some(c) = tok.strip_prefix("--audio-codec=") {
                codec_name = Some(c.to_lowercase());
            } else if let Some(e) = tok.strip_prefix("--audio-encoder=") {
                enc_name = Some(e.to_string());
            }
        }
        if let Some(c) = codec_name {
            let list = codecs.entry(c).or_default();
            if let Some(e) = enc_name {
                if !list.contains(&e) {
                    list.push(e);
                }
            }
        }
    }
    if codecs.is_empty() {
        return default_encoders();
    }
    AudioEncoderInfo {
        codecs,
        raw_output: output.to_string(),
    }
}

fn default_encoders() -> AudioEncoderInfo {
    let mut codecs = BTreeMap::new();
    for &c in SUPPORTED_AUDIO_CODECS {
        codecs.insert(c.to_string(), Vec::new());
    }
    AudioEncoderInfo {
        codecs,
        raw_output: String::new(),
    }
}

fn extract_ipv4_after_inet(text: &str) -> Option<String> {
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(idx) = trimmed.find("inet ") {
            let rest = &trimmed[idx + 5..];
            if let Some(addr_cidr) = rest.split_whitespace().next() {
                let ip = addr_cidr.split('/').next().unwrap_or("");
                if ip.split('.').count() == 4 && !ip.starts_with("127.") {
                    return Some(ip.to_string());
                }
            }
        }
    }
    None
}

pub fn extract_tar_gz_safely(archive_path: &Path, dest_dir: &Path) -> Result<(), AndroidError> {
    let file = File::open(archive_path)?;
    let gz = GzDecoder::new(file);
    let mut archive = Archive::new(gz);
    for entry_res in archive.entries()? {
        let mut entry = entry_res?;
        let rel_path = entry.path()?.into_owned();
        if rel_path.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::RootDir
            )
        }) {
            continue;
        }
        // Strip single top-level directory if present
        let components: Vec<_> = rel_path.components().collect();
        let target_rel: PathBuf = if components.len() > 1 {
            components[1..].iter().collect()
        } else {
            rel_path
        };
        if target_rel.as_os_str().is_empty() {
            continue;
        }
        let out_path = dest_dir.join(&target_rel);
        if entry.header().entry_type().is_dir() {
            fs::create_dir_all(&out_path)?;
        } else if entry.header().entry_type().is_file() {
            if let Some(p) = out_path.parent() {
                fs::create_dir_all(p)?;
            }
            let mut out_f = File::create(&out_path)?;
            std::io::copy(&mut entry, &mut out_f)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = entry.header().mode().unwrap_or(0o644);
                let name_str = out_path.to_string_lossy();
                if name_str.ends_with("scrcpy") || name_str.ends_with("adb") || (mode & 0o111 != 0)
                {
                    let _ = fs::set_permissions(&out_path, fs::Permissions::from_mode(0o755));
                }
            }
        }
    }
    Ok(())
}

pub fn extract_zip_safely(archive_path: &Path, dest_dir: &Path) -> Result<(), AndroidError> {
    let file = File::open(archive_path)?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| AndroidError::Message(format!("Invalid zip archive: {e}")))?;
    for i in 0..archive.len() {
        let mut member = archive
            .by_index(i)
            .map_err(|e| AndroidError::Message(e.to_string()))?;
        let Some(enclosed) = member.enclosed_name().map(|p| p.to_path_buf()) else {
            continue;
        };
        let components: Vec<_> = enclosed.components().collect();
        let target_rel: PathBuf = if components.len() > 1 {
            components[1..].iter().collect()
        } else {
            enclosed
        };
        if target_rel.as_os_str().is_empty() {
            continue;
        }
        let out_path = dest_dir.join(&target_rel);
        if member.is_dir() {
            fs::create_dir_all(&out_path)?;
        } else {
            if let Some(p) = out_path.parent() {
                fs::create_dir_all(p)?;
            }
            let mut out_f = File::create(&out_path)?;
            std::io::copy(&mut member, &mut out_f)?;
        }
    }
    Ok(())
}

struct CaptureInner {
    child: Option<Child>,
    serial: String,
    device_name: String,
    codec: String,
    source: String,
    as_mic: bool,
    virtual_sink_module: u32,
    virtual_source_module: u32,
    previous_default_source: String,
    started_at: f64,
    log_lines: VecDeque<String>,
    last_error: String,
}

pub struct AndroidCaptureSession {
    pub manager: Arc<AndroidManager>,
    backend: Arc<dyn AudioBackend>,
    inner: Arc<Mutex<CaptureInner>>,
    op_lock: Mutex<()>,
}

impl Drop for AndroidCaptureSession {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

impl AndroidCaptureSession {
    pub fn new(manager: Arc<AndroidManager>, backend: Arc<dyn AudioBackend>) -> Self {
        Self {
            manager,
            backend,
            inner: Arc::new(Mutex::new(CaptureInner {
                child: None,
                serial: String::new(),
                device_name: String::new(),
                codec: String::new(),
                source: String::new(),
                as_mic: false,
                virtual_sink_module: 0,
                virtual_source_module: 0,
                previous_default_source: String::new(),
                started_at: 0.0,
                log_lines: VecDeque::new(),
                last_error: String::new(),
            })),
            op_lock: Mutex::new(()),
        }
    }

    fn take_exited_cleanup(guard: &mut CaptureInner) -> Option<(u32, u32, String)> {
        guard.child = None;
        let src_mod = std::mem::take(&mut guard.virtual_source_module);
        let sink_mod = std::mem::take(&mut guard.virtual_sink_module);
        let prev_src = std::mem::take(&mut guard.previous_default_source);
        guard.as_mic = false;
        if src_mod > 0 || sink_mod > 0 || !prev_src.is_empty() {
            Some((src_mod, sink_mod, prev_src))
        } else {
            None
        }
    }

    fn cleanup_virtual_mic(&self, virt_src_mod: u32, virt_sink_mod: u32, prev_source: &str) {
        if virt_src_mod > 0 {
            let _ = self.backend.unload_module(virt_src_mod, true);
        }
        if virt_sink_mod > 0 {
            let _ = self.backend.unload_module(virt_sink_mod, true);
        }
        if !prev_source.is_empty() {
            let _ = self.backend.set_default_source(prev_source);
        }
    }

    pub fn is_active(&self) -> bool {
        let cleanup = {
            let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(ref mut child) = guard.child {
                if let Ok(None) = child.try_wait() {
                    return true;
                }
            }
            Self::take_exited_cleanup(&mut guard)
        };
        if let Some((src_mod, sink_mod, prev_src)) = cleanup {
            self.cleanup_virtual_mic(src_mod, sink_mod, &prev_src);
        }
        false
    }

    pub fn status(&self) -> serde_json::Value {
        let (val, cleanup) = {
            let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let mut active = false;
            let mut pid = 0u32;
            if let Some(ref mut child) = guard.child {
                if let Ok(None) = child.try_wait() {
                    active = true;
                    pid = child.id();
                }
            }
            let cleanup = if !active {
                Self::take_exited_cleanup(&mut guard)
            } else {
                None
            };
            let elapsed = if active {
                (now_secs() - guard.started_at).max(0.0)
            } else {
                0.0
            };
            let recent_logs: Vec<String> = guard
                .log_lines
                .iter()
                .rev()
                .take(20)
                .cloned()
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            let val = serde_json::json!({
                "active": active,
                "serial": if active { &guard.serial } else { "" },
                "device_name": if active { &guard.device_name } else { "" },
                "codec": if active { &guard.codec } else { "" },
                "source": if active { &guard.source } else { "" },
                "as_mic": if active { guard.as_mic } else { false },
                "virtual_source": if active && guard.as_mic { "soundhop_android_mic" } else { "" },
                "pid": pid,
                "started_at": if active { guard.started_at } else { 0.0 },
                "uptime_s": elapsed,
                "last_error": guard.last_error,
                "recent_logs": recent_logs,
            });
            (val, cleanup)
        };
        if let Some((src_mod, sink_mod, prev_src)) = cleanup {
            self.cleanup_virtual_mic(src_mod, sink_mod, &prev_src);
        }
        val
    }

    #[allow(clippy::too_many_arguments)]
    pub fn start(
        &self,
        serial: &str,
        device_name: &str,
        codec: Option<&str>,
        source: Option<&str>,
        as_mic: bool,
        buffer_ms: Option<u32>,
        bit_rate: Option<u32>,
        duplicate: bool,
    ) -> Result<serde_json::Value, AndroidError> {
        let _op_guard = self.op_lock.lock().unwrap_or_else(|e| e.into_inner());
        if self.is_active() {
            return Err(AndroidError::Message(
                "Audio capture is already active. Stop the current capture before starting another."
                    .to_string(),
            ));
        }

        let effective_codec = codec.unwrap_or(DEFAULT_AUDIO_CODEC);
        let effective_source = if as_mic {
            "mic"
        } else {
            source.unwrap_or("output")
        };

        let args = self.manager.build_capture_args(
            serial,
            effective_codec,
            Some(effective_source),
            buffer_ms.or(Some(DEFAULT_AUDIO_BUFFER_MS)),
            bit_rate.or(Some(DEFAULT_AUDIO_BIT_RATE)),
            duplicate,
        )?;

        let mut cmd = Command::new(&args[0]);
        cmd.args(&args[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut virt_sink_mod = 0;
        let mut virt_src_mod = 0;
        let mut prev_source = String::new();

        if as_mic {
            let sink_name = "soundhop_android_mic_sink";
            let source_name = "soundhop_android_mic";
            virt_sink_mod = self
                .backend
                .load_module(
                    "module-null-sink",
                    &[
                        format!("sink_name={sink_name}"),
                        "sink_properties=device.description=SoundHop-Android-Mic-Sink".to_string(),
                    ],
                )
                .map_err(|e| {
                    AndroidError::Message(format!("Failed to create virtual mic sink: {e}"))
                })?;
            virt_src_mod = match self.backend.load_module(
                "module-remap-source",
                &[
                    format!("source_name={source_name}"),
                    format!("master={sink_name}.monitor"),
                    "source_properties=device.description=Android-Microphone".to_string(),
                ],
            ) {
                Ok(rid) => rid,
                Err(e) => {
                    self.cleanup_virtual_mic(0, virt_sink_mod, "");
                    return Err(AndroidError::Message(format!(
                        "Failed to create virtual mic source: {e}"
                    )));
                }
            };
            prev_source = self.backend.default_source().unwrap_or_default();
            let _ = self.backend.set_default_source(source_name);
            cmd.env("SDL_AUDIODRIVER", "pulseaudio")
                .env("PULSE_SINK", sink_name)
                .env("PIPEWIRE_NODE", sink_name);
        }

        crate::util::close_fds_pre_exec(&mut cmd);

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                self.cleanup_virtual_mic(virt_src_mod, virt_sink_mod, &prev_source);
                return Err(AndroidError::Message(format!(
                    "Failed to start scrcpy: {e}"
                )));
            }
        };

        let stdout_opt = child.stdout.take();
        let stderr_opt = child.stderr.take();

        let prior_cleanup = {
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let prior = Self::take_exited_cleanup(&mut g);
            g.child = Some(child);
            g.serial = serial.to_string();
            g.device_name = if device_name.is_empty() {
                serial.to_string()
            } else {
                device_name.to_string()
            };
            g.codec = effective_codec.to_string();
            g.source = effective_source.to_string();
            g.as_mic = as_mic;
            g.virtual_sink_module = virt_sink_mod;
            g.virtual_source_module = virt_src_mod;
            g.previous_default_source = prev_source;
            g.started_at = now_secs();
            g.log_lines.clear();
            g.last_error.clear();
            prior
        };
        if let Some((old_src, old_sink, old_prev)) = prior_cleanup {
            let restore = if as_mic { "" } else { old_prev.as_str() };
            self.cleanup_virtual_mic(old_src, old_sink, restore);
        }

        if let Some(stdout) = stdout_opt {
            let inner_clone = Arc::clone(&self.inner);
            thread::spawn(move || {
                let reader = BufReader::new(stdout);
                for line in reader.lines().map_while(Result::ok) {
                    let mut g = inner_clone.lock().unwrap_or_else(|e| e.into_inner());
                    if g.log_lines.len() >= 200 {
                        g.log_lines.pop_front();
                    }
                    g.log_lines.push_back(line);
                }
            });
        }
        if let Some(stderr) = stderr_opt {
            let inner_clone = Arc::clone(&self.inner);
            thread::spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines().map_while(Result::ok) {
                    let mut g = inner_clone.lock().unwrap_or_else(|e| e.into_inner());
                    if !line.trim().is_empty() {
                        g.last_error = line.clone();
                    }
                    if g.log_lines.len() >= 200 {
                        g.log_lines.pop_front();
                    }
                    g.log_lines.push_back(line);
                }
            });
        }

        Ok(self.status())
    }

    pub fn stop(&self) -> serde_json::Value {
        let _op_guard = self.op_lock.lock().unwrap_or_else(|e| e.into_inner());
        let cleanup = {
            let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(mut child) = guard.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            Self::take_exited_cleanup(&mut guard)
        };
        if let Some((src_mod, sink_mod, prev_src)) = cleanup {
            self.cleanup_virtual_mic(src_mod, sink_mod, &prev_src);
        }
        self.status()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_adb_devices_and_encoders() {
        let adb_out = "List of devices attached\nR58M123456\tdevice usb:1-2 product:beyond1 model:SM_G973F device:beyond1 transport_id:1\n192.168.1.80:5555\tdevice product:pixel model:Pixel_8 device:shiba transport_id:2\n";
        let devs = parse_adb_devices_output(adb_out);
        assert_eq!(devs.len(), 2);
        assert_eq!(devs[0].serial, "R58M123456");
        assert_eq!(devs[0].model, "SM_G973F");
        assert!(!devs[0].is_wifi);
        assert!(devs[1].is_wifi);
        assert_eq!(devs[1].ip, "192.168.1.80");
        assert_eq!(devs[1].port, 5555);

        let enc_out = "List of audio encoders:\n    --audio-codec=opus --audio-encoder=c2.android.opus.encoder\n    --audio-codec=aac --audio-encoder=c2.android.aac.encoder\n";
        let info = parse_scrcpy_encoders(enc_out);
        assert!(info.codecs.contains_key("opus"));
        assert!(info.codecs.contains_key("aac"));
    }

    #[test]
    fn test_validate_adb_target_and_capture_args() {
        assert_eq!(
            validate_adb_target("192.168.1.10:5555").unwrap(),
            "192.168.1.10:5555"
        );
        assert!(validate_adb_target("--help").is_err());
        assert!(validate_adb_target("-s").is_err());
        assert!(validate_adb_target("abc def").is_err());
        assert!(validate_adb_target("   ").is_err());

        let dir = tempfile::tempdir().unwrap();
        let fake_scrcpy = dir.path().join("scrcpy");
        fs::write(&fake_scrcpy, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&fake_scrcpy, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mgr = AndroidManager::new(&fake_scrcpy.to_string_lossy(), "");

        assert!(mgr
            .build_capture_args("-evil", "opus", Some("output"), None, None, false)
            .is_err());
        assert!(mgr
            .build_capture_args("R58M123", "opus", Some("invalid_source"), None, None, false)
            .is_err());
        let args = mgr
            .build_capture_args("R58M123", "opus", Some("output"), Some(50), None, false)
            .unwrap();
        assert!(args.contains(&"--audio-source=output".to_string()));
        assert!(args.contains(&"R58M123".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn test_virtual_mic_cleanup_on_exited_child_and_load_failure() {
        use crate::backend::linux::LinuxAudioBackend;
        use crate::util::{CommandResult, MockRunner};
        use std::sync::atomic::{AtomicBool, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let fake_scrcpy = dir.path().join("scrcpy");
        fs::write(&fake_scrcpy, b"#!/bin/sh\nexit 0\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&fake_scrcpy, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mgr = Arc::new(AndroidManager::new(&fake_scrcpy.to_string_lossy(), ""));

        // Case 1: child exits on its own -> status()/is_active() cleans up modules and restores source.
        let fail_remap = Arc::new(AtomicBool::new(false));
        let fail_remap_c = Arc::clone(&fail_remap);
        let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let calls_c = Arc::clone(&calls);

        let runner = Arc::new(MockRunner::new(move |args, _| {
            let joined = args.join(" ");
            calls_c.lock().unwrap().push(joined.clone());
            if joined.contains("load-module module-null-sink") {
                return Ok(CommandResult {
                    args: args.to_vec(),
                    returncode: 0,
                    stdout: "101\n".to_string(),
                    stderr: String::new(),
                });
            }
            if joined.contains("load-module module-remap-source") {
                if fail_remap_c.load(Ordering::SeqCst) {
                    return Ok(CommandResult {
                        args: args.to_vec(),
                        returncode: 1,
                        stdout: String::new(),
                        stderr: "module load failed".to_string(),
                    });
                }
                return Ok(CommandResult {
                    args: args.to_vec(),
                    returncode: 0,
                    stdout: "102\n".to_string(),
                    stderr: String::new(),
                });
            }
            if joined.contains("info") {
                return Ok(CommandResult {
                    args: args.to_vec(),
                    returncode: 0,
                    stdout: "Default Source: alsa_input.orig_mic\n".to_string(),
                    stderr: String::new(),
                });
            }
            Ok(CommandResult {
                args: args.to_vec(),
                returncode: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }));
        let backend: Arc<dyn AudioBackend> = Arc::new(LinuxAudioBackend::with_runner(runner));
        let session = AndroidCaptureSession::new(Arc::clone(&mgr), Arc::clone(&backend));

        let _ = session
            .start("R58M123", "Pixel", None, None, true, None, None, false)
            .expect("start should succeed");

        // Wait briefly for the immediate-exit fake scrcpy child to terminate.
        for _ in 0..50 {
            if !session.is_active() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(!session.is_active());
        let recorded = calls.lock().unwrap().clone();
        assert!(
            recorded.iter().any(|c| c.contains("unload-module 102")),
            "expected remap-source module 102 to be unloaded; calls: {recorded:?}"
        );
        assert!(
            recorded.iter().any(|c| c.contains("unload-module 101")),
            "expected null-sink module 101 to be unloaded; calls: {recorded:?}"
        );
        assert!(
            recorded
                .iter()
                .any(|c| c.contains("set-default-source alsa_input.orig_mic")),
            "expected default source to be restored; calls: {recorded:?}"
        );

        // Case 2: module-remap-source fails -> null-sink 101 is unloaded and default source is NOT changed.
        calls.lock().unwrap().clear();
        fail_remap.store(true, Ordering::SeqCst);
        let err = session
            .start("R58M123", "Pixel", None, None, true, None, None, false)
            .expect_err("start should fail when remap-source load fails");
        assert!(err.to_string().contains("virtual mic source"));
        let recorded2 = calls.lock().unwrap().clone();
        assert!(
            recorded2.iter().any(|c| c.contains("unload-module 101")),
            "expected null-sink 101 to be rolled back; calls: {recorded2:?}"
        );
        assert!(
            !recorded2
                .iter()
                .any(|c| c.contains("set-default-source soundhop_android_mic")),
            "must not switch default source when remap-source failed; calls: {recorded2:?}"
        );
    }

    #[test]
    fn test_aarch64_scrcpy_checksum_fails_closed_and_verifies() {
        // Linux aarch64 v3.1 has no pinned upstream checksum -> must fail closed.
        let err = resolve_scrcpy_sha256("linux", "aarch64", DEFAULT_SCRCPY_VERSION, None)
            .expect_err("linux/aarch64 must fail closed without an explicit verified SHA-256");
        let msg = err.to_string();
        assert!(msg.contains("aarch64"), "unexpected error: {msg}");
        assert!(msg.contains("scrcpy"), "unexpected error: {msg}");

        // Custom unpinned version also fails closed without an explicit SHA-256.
        assert!(resolve_scrcpy_sha256("linux", "x86_64", "9.9.9", None).is_err());

        // Pinned platforms resolve cleanly.
        assert_eq!(
            resolve_scrcpy_sha256("linux", "x86_64", DEFAULT_SCRCPY_VERSION, None).unwrap(),
            SCRCPY_V3_1_SHA256_LINUX_X86_64
        );
        assert_eq!(
            resolve_scrcpy_sha256("windows", "x86_64", DEFAULT_SCRCPY_VERSION, None).unwrap(),
            SCRCPY_V3_1_SHA256_WIN64
        );

        // Explicit verified hash is accepted on aarch64.
        let explicit = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        assert_eq!(
            resolve_scrcpy_sha256("linux", "aarch64", DEFAULT_SCRCPY_VERSION, Some(explicit))
                .unwrap(),
            explicit
        );

        // Checksum verification enforces constant-time match and rejects mismatches.
        assert!(verify_scrcpy_sha256(explicit, &explicit.to_uppercase()).is_ok());
        assert!(verify_scrcpy_sha256("deadbeef", explicit).is_err());
        assert!(verify_scrcpy_sha256(explicit, "").is_err());
    }

    #[test]
    fn test_mdns_discovery_flag_gates_adb_wireless_discovery() {
        use crate::util::MockRunner;
        use std::sync::atomic::AtomicUsize;

        let dir = tempfile::tempdir().unwrap();
        let fake_adb = dir.path().join("adb");
        fs::write(&fake_adb, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&fake_adb, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_c = Arc::clone(&call_count);
        let runner = Arc::new(MockRunner::new(move |args, _| {
            call_count_c.fetch_add(1, Ordering::SeqCst);
            if args.iter().any(|a| a == "mdns") {
                return Ok(CommandResult {
                    args: args.to_vec(),
                    returncode: 0,
                    stdout: "List of discovered mdns services\nadb-R58M123-abc123\t_adb-tls-connect._tcp.\t192.168.1.77:37123\npixel8-tcp\t_adb._tcp.\t192.168.1.88:5555\npublic-host\t_adb._tcp.\t8.8.8.8:5555\n".to_string(),
                    stderr: String::new(),
                });
            }
            Ok(CommandResult {
                args: args.to_vec(),
                returncode: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }));

        let mgr = AndroidManager::with_runner("", &fake_adb.to_string_lossy(), runner);

        // Disabled flag: returns empty without running adb.
        let disabled = mgr.discover_mdns_devices(false, Duration::ZERO);
        assert!(disabled.is_empty());
        assert_eq!(call_count.load(Ordering::SeqCst), 0);

        // Enabled flag: returns both private TLS and TCP endpoints, filtering out public IPs.
        let enabled = mgr.discover_mdns_devices(true, Duration::ZERO);
        assert_eq!(call_count.load(Ordering::SeqCst), 1);
        assert_eq!(enabled.len(), 2);
        assert!(enabled
            .iter()
            .any(|d| d.endpoint() == "192.168.1.77:37123" && d.tls));
        assert!(enabled
            .iter()
            .any(|d| d.endpoint() == "192.168.1.88:5555" && !d.tls));

        // ServiceInfo parser handles `_adb-tls-connect._tcp.local.` cleanly.
        let empty_props: &[(&str, &str)] = &[];
        let info = ServiceInfo::new(
            ADB_MDNS_TLS_CONNECT_SERVICE_TYPE,
            "adb-pixel-8",
            "pixel8.local.",
            "192.168.1.99",
            41234,
            empty_props,
        )
        .unwrap();
        let parsed = parse_adb_mdns_service_info(&info).expect("should parse private ADB mDNS");
        assert_eq!(parsed.name, "adb-pixel-8");
        assert_eq!(parsed.endpoint(), "192.168.1.99:41234");
        assert!(parsed.tls);
    }

    #[test]
    fn test_auto_switch_wifi_flag_gates_usb_to_tcpip_switch() {
        use crate::util::MockRunner;

        let dir = tempfile::tempdir().unwrap();
        let fake_adb = dir.path().join("adb");
        fs::write(&fake_adb, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&fake_adb, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let calls_c = Arc::clone(&calls);
        let runner = Arc::new(MockRunner::new(move |args, _| {
            let joined = args.join(" ");
            calls_c.lock().unwrap().push(joined.clone());
            if joined.contains("ip -f inet addr show wlan0") {
                return Ok(CommandResult {
                    args: args.to_vec(),
                    returncode: 0,
                    stdout: "    inet 192.168.1.64/24 brd 192.168.1.255 scope global wlan0\n"
                        .to_string(),
                    stderr: String::new(),
                });
            }
            if joined.contains("tcpip 5555") {
                return Ok(CommandResult {
                    args: args.to_vec(),
                    returncode: 0,
                    stdout: "restarting in TCP mode port: 5555\n".to_string(),
                    stderr: String::new(),
                });
            }
            if joined.contains("connect 192.168.1.64:5555") {
                return Ok(CommandResult {
                    args: args.to_vec(),
                    returncode: 0,
                    stdout: "connected to 192.168.1.64:5555\n".to_string(),
                    stderr: String::new(),
                });
            }
            Ok(CommandResult {
                args: args.to_vec(),
                returncode: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }));

        let mgr = AndroidManager::with_runner("", &fake_adb.to_string_lossy(), runner);
        let usb_dev = AdbDevice {
            serial: "R58M123456".to_string(),
            state: "device".to_string(),
            model: "Pixel_8".to_string(),
            product: "shiba".to_string(),
            device: "shiba".to_string(),
            transport_id: "1".to_string(),
            is_wifi: false,
            ip: String::new(),
            port: 5555,
            android_version: "14".to_string(),
            sdk_version: 34,
            manufacturer: "Google".to_string(),
        };

        // Disabled flag: returns Ok(None) and makes zero adb calls.
        let skipped = mgr
            .auto_switch_usb_to_wifi(&usb_dev, false, 5555)
            .expect("disabled auto_switch_wifi should succeed with None");
        assert!(skipped.is_none());
        assert!(calls.lock().unwrap().is_empty());

        // Enabled flag: switches USB device to Wi-Fi and returns outcome.
        let outcome = mgr
            .auto_switch_usb_to_wifi(&usb_dev, true, 5555)
            .expect("enabled auto_switch_wifi should succeed")
            .expect("expected WifiSwitchOutcome");
        assert_eq!(outcome.usb_serial, "R58M123456");
        assert_eq!(outcome.wifi_serial, "192.168.1.64:5555");
        assert_eq!(outcome.ip, "192.168.1.64");
        assert_eq!(outcome.port, 5555);
        assert_eq!(outcome.model, "Pixel_8");

        let recorded = calls.lock().unwrap().clone();
        assert!(recorded.iter().any(|c| c.contains("tcpip 5555")));
        assert!(recorded
            .iter()
            .any(|c| c.contains("connect 192.168.1.64:5555")));
    }

    fn make_fake_adb(dir: &tempfile::TempDir) -> PathBuf {
        let fake_adb = dir.path().join("adb");
        fs::write(&fake_adb, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&fake_adb, fs::Permissions::from_mode(0o755)).unwrap();
        }
        fake_adb
    }

    #[test]
    fn test_tool_status_adb_ready_vs_scrcpy_ready() {
        use crate::util::MockRunner;

        let adb_only = ToolStatus {
            scrcpy_path: None,
            scrcpy_version: String::new(),
            adb_path: Some(PathBuf::from("/usr/bin/adb")),
            adb_version: "1.0.41".to_string(),
            is_managed: false,
        };
        assert!(adb_only.adb_ready());
        assert!(!adb_only.scrcpy_ready());
        assert!(!adb_only.ready());

        let scrcpy_only = ToolStatus {
            scrcpy_path: Some(PathBuf::from("/usr/bin/scrcpy")),
            scrcpy_version: "3.1".to_string(),
            adb_path: None,
            adb_version: String::new(),
            is_managed: false,
        };
        assert!(!scrcpy_only.adb_ready());
        assert!(!scrcpy_only.scrcpy_ready());
        assert!(!scrcpy_only.ready());

        let both = ToolStatus {
            scrcpy_path: Some(PathBuf::from("/usr/bin/scrcpy")),
            scrcpy_version: "3.1".to_string(),
            adb_path: Some(PathBuf::from("/usr/bin/adb")),
            adb_version: "1.0.41".to_string(),
            is_managed: false,
        };
        assert!(both.adb_ready());
        assert!(both.scrcpy_ready());
        assert!(both.ready());

        let neither = ToolStatus {
            scrcpy_path: None,
            scrcpy_version: String::new(),
            adb_path: None,
            adb_version: String::new(),
            is_managed: false,
        };
        assert!(!neither.adb_ready());
        assert!(!neither.scrcpy_ready());
        assert!(!neither.ready());

        let dir = tempfile::tempdir().unwrap();
        let fake_adb = make_fake_adb(&dir);
        let runner = Arc::new(MockRunner::new(move |args, _| {
            let joined = args.join(" ");
            if joined.ends_with(" version") {
                return Ok(CommandResult {
                    args: args.to_vec(),
                    returncode: 0,
                    stdout: "Android Debug Bridge version 1.0.41\nVersion 35.0.0\n".to_string(),
                    stderr: String::new(),
                });
            }
            Ok(CommandResult {
                args: args.to_vec(),
                returncode: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }));
        let mgr = AndroidManager::with_runner("", &fake_adb.to_string_lossy(), runner);
        let status = mgr.get_tool_status();
        assert!(status.adb_ready());
        assert_eq!(status.adb_version, "1.0.41");
        assert_eq!(status.ready(), status.scrcpy_ready());
    }

    #[test]
    fn test_reverse_tunnel_setup_list_and_remove() {
        use crate::util::MockRunner;

        let dir = tempfile::tempdir().unwrap();
        let fake_adb = make_fake_adb(&dir);

        let calls: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let calls_c = Arc::clone(&calls);
        let mode = Arc::new(Mutex::new("ok".to_string()));
        let mode_c = Arc::clone(&mode);

        let runner = Arc::new(MockRunner::new(move |args, _| {
            calls_c.lock().unwrap().push(args.to_vec());
            let cur_mode = mode_c.lock().unwrap().clone();
            if args.windows(2).any(|w| w == ["reverse", "--list"]) {
                return Ok(CommandResult {
                    args: args.to_vec(),
                    returncode: 0,
                    stdout: "host-1 tcp:4717 tcp:4717\nhost-1 tcp:4718 tcp:4718\n\n".to_string(),
                    stderr: String::new(),
                });
            }
            if args.iter().any(|a| a == "--remove" || a == "--remove-all") {
                match cur_mode.as_str() {
                    "offline" => {
                        return Ok(CommandResult {
                            args: args.to_vec(),
                            returncode: 1,
                            stdout: String::new(),
                            stderr: "adb: error: device offline".to_string(),
                        });
                    }
                    "not_found" => {
                        return Ok(CommandResult {
                            args: args.to_vec(),
                            returncode: 1,
                            stdout: String::new(),
                            stderr: "adb: error: device 'R58M123' not found".to_string(),
                        });
                    }
                    "listener_missing" => {
                        return Ok(CommandResult {
                            args: args.to_vec(),
                            returncode: 1,
                            stdout: String::new(),
                            stderr: "adb: error: listener 'tcp:4717' not found".to_string(),
                        });
                    }
                    "perm_denied" => {
                        return Ok(CommandResult {
                            args: args.to_vec(),
                            returncode: 1,
                            stdout: String::new(),
                            stderr: "adb: error: insufficient permissions".to_string(),
                        });
                    }
                    _ => {}
                }
            }
            Ok(CommandResult {
                args: args.to_vec(),
                returncode: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }));

        let mgr = AndroidManager::with_runner("", &fake_adb.to_string_lossy(), runner);

        // Input validation
        assert!(mgr.setup_reverse_tunnel("-evil", 4717, 4717).is_err());
        assert!(mgr.setup_reverse_tunnel("R58M123", 0, 4717).is_err());
        assert!(mgr.setup_reverse_tunnel("R58M123", 4717, 0).is_err());
        assert!(mgr.remove_reverse_tunnel("-evil", Some(4717)).is_err());
        assert!(mgr.remove_reverse_tunnel("R58M123", Some(0)).is_err());
        assert!(mgr.list_reverse_tunnels("bad serial").is_err());
        assert!(calls.lock().unwrap().is_empty());

        // Setup and list
        mgr.setup_reverse_tunnel("R58M123", 4717, 4717).unwrap();
        mgr.setup_reverse_tunnel("R58M123", 4718, 4718).unwrap();
        assert_eq!(
            mgr.tracked_reverse_tunnels(),
            vec![("R58M123".to_string(), 4717), ("R58M123".to_string(), 4718)]
        );

        let listed = mgr.list_reverse_tunnels("R58M123").unwrap();
        assert_eq!(
            listed,
            vec![
                "host-1 tcp:4717 tcp:4717".to_string(),
                "host-1 tcp:4718 tcp:4718".to_string()
            ]
        );

        // Remove single port
        mgr.remove_reverse_tunnel("R58M123", Some(4717)).unwrap();
        assert_eq!(
            mgr.tracked_reverse_tunnels(),
            vec![("R58M123".to_string(), 4718)]
        );

        // Remove all ports for serial
        mgr.remove_reverse_tunnel("R58M123", None).unwrap();
        assert!(mgr.tracked_reverse_tunnels().is_empty());

        // Benign offline/not-found errors during teardown are ignored and clear tracking
        for err_mode in ["offline", "not_found", "listener_missing"] {
            *mode.lock().unwrap() = "ok".to_string();
            mgr.setup_reverse_tunnel("R58M123", 4717, 4717).unwrap();
            *mode.lock().unwrap() = err_mode.to_string();
            mgr.remove_reverse_tunnel("R58M123", Some(4717))
                .expect("benign teardown error must be ignored");
            assert!(mgr.tracked_reverse_tunnels().is_empty());
        }

        // Non-benign teardown errors still fail
        *mode.lock().unwrap() = "ok".to_string();
        mgr.setup_reverse_tunnel("R58M123", 4717, 4717).unwrap();
        *mode.lock().unwrap() = "perm_denied".to_string();
        assert!(mgr.remove_reverse_tunnel("R58M123", Some(4717)).is_err());

        let recorded = calls.lock().unwrap().clone();
        assert!(recorded.iter().any(|a| {
            a.ends_with(&[
                "-s".into(),
                "R58M123".into(),
                "reverse".into(),
                "tcp:4717".into(),
                "tcp:4717".into(),
            ])
        }));
        assert!(recorded.iter().any(|a| {
            a.ends_with(&[
                "-s".into(),
                "R58M123".into(),
                "reverse".into(),
                "--remove".into(),
                "tcp:4717".into(),
            ])
        }));
        assert!(recorded.iter().any(|a| {
            a.ends_with(&[
                "-s".into(),
                "R58M123".into(),
                "reverse".into(),
                "--remove-all".into(),
            ])
        }));
        assert!(recorded.iter().any(|a| {
            a.ends_with(&[
                "-s".into(),
                "R58M123".into(),
                "reverse".into(),
                "--list".into(),
            ])
        }));
    }

    #[test]
    fn test_disconnect_wifi_cleans_up_tracked_reverse_tunnels() {
        use crate::util::MockRunner;

        let dir = tempfile::tempdir().unwrap();
        let fake_adb = make_fake_adb(&dir);

        let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let calls_c = Arc::clone(&calls);
        let runner = Arc::new(MockRunner::new(move |args, _| {
            calls_c.lock().unwrap().push(args.join(" "));
            Ok(CommandResult {
                args: args.to_vec(),
                returncode: 0,
                stdout: "disconnected everything".to_string(),
                stderr: String::new(),
            })
        }));

        let mgr = AndroidManager::with_runner("", &fake_adb.to_string_lossy(), runner);
        mgr.setup_reverse_tunnel("192.168.1.50:5555", 4717, 4717)
            .unwrap();
        mgr.setup_reverse_tunnel("192.168.1.60:5555", 4717, 4717)
            .unwrap();
        mgr.setup_reverse_tunnel("R58M123456", 4717, 4717).unwrap();

        // Disconnecting a specific Wi-Fi target cleans up only that target's tunnel.
        let (ok, _) = mgr.disconnect_wifi(Some("192.168.1.50:5555"));
        assert!(ok);
        assert_eq!(
            mgr.tracked_reverse_tunnels(),
            vec![
                ("192.168.1.60:5555".to_string(), 4717),
                ("R58M123456".to_string(), 4717),
            ]
        );

        // Disconnecting all Wi-Fi targets cleans remaining Wi-Fi tunnels while preserving USB.
        let (ok, _) = mgr.disconnect_wifi(None);
        assert!(ok);
        assert_eq!(
            mgr.tracked_reverse_tunnels(),
            vec![("R58M123456".to_string(), 4717)]
        );

        let recorded = calls.lock().unwrap().clone();
        assert!(recorded
            .iter()
            .any(|c| c.contains("-s 192.168.1.50:5555 reverse --remove tcp:4717")));
        assert!(recorded
            .iter()
            .any(|c| c.contains("-s 192.168.1.60:5555 reverse --remove tcp:4717")));
        assert!(!recorded
            .iter()
            .any(|c| c.contains("-s R58M123456 reverse --remove")));
    }

    #[test]
    fn test_open_browser_url_allowlist_and_injection_rejection() {
        use crate::util::MockRunner;

        let dir = tempfile::tempdir().unwrap();
        let fake_adb = make_fake_adb(&dir);

        let calls: Arc<Mutex<Vec<Vec<String>>>> = Arc::new(Mutex::new(Vec::new()));
        let calls_c = Arc::clone(&calls);
        let runner = Arc::new(MockRunner::new(move |args, _| {
            calls_c.lock().unwrap().push(args.to_vec());
            Ok(CommandResult {
                args: args.to_vec(),
                returncode: 0,
                stdout: "Starting: Intent { act=android.intent.action.VIEW }".to_string(),
                stderr: String::new(),
            })
        }));

        let mgr = AndroidManager::with_runner("", &fake_adb.to_string_lossy(), runner);

        let valid_urls = [
            "http://localhost:4717",
            "http://localhost:4717/",
            "http://localhost:4717/#t=0123456789abcdef0123456789abcdef",
            "http://127.0.0.1:4717",
            "http://127.0.0.1:4717/",
            "http://127.0.0.1:4717/?foo=bar&v=1#t=abc_.-~",
        ];
        for url in valid_urls {
            mgr.open_browser_url("R58M123456", url)
                .unwrap_or_else(|e| panic!("expected valid URL {url:?} to succeed: {e}"));
        }
        assert_eq!(calls.lock().unwrap().len(), valid_urls.len());
        assert_eq!(
            calls.lock().unwrap()[2],
            vec![
                fake_adb.to_string_lossy().to_string(),
                "-s".to_string(),
                "R58M123456".to_string(),
                "shell".to_string(),
                "am".to_string(),
                "start".to_string(),
                "-a".to_string(),
                "android.intent.action.VIEW".to_string(),
                "-d".to_string(),
                "'http://localhost:4717/#t=0123456789abcdef0123456789abcdef'".to_string(),
            ]
        );

        calls.lock().unwrap().clear();
        let rejected_urls = [
            "",
            "http://192.168.1.10:4717/#t=abc",
            "http://evil.com/",
            "https://localhost:4717/",
            "javascript:alert(1)",
            "http://localhost/",
            "http://127.0.0.1/",
            "http://localhost:0/",
            "http://localhost:04717/",
            "http://localhost:99999/",
            "http://localhost:4717.evil.com/",
            "http://127.0.0.1:4717.evil.com/",
            "http://localhost:4717/#t=abc;id",
            "http://localhost:4717/#t=$(id)",
            "http://localhost:4717/#t=`id`",
            "http://localhost:4717/#t=abc def",
            "http://localhost:4717/#t=\"abc\"",
            "http://localhost:4717/#t='abc'",
            "http://localhost:4717/#t=abc\nid",
            "http://localhost:4717/#t=abc|cat",
        ];
        for url in rejected_urls {
            assert!(
                mgr.open_browser_url("R58M123456", url).is_err(),
                "expected malicious/disallowed URL {url:?} to be rejected"
            );
        }
        assert!(mgr
            .open_browser_url("-evil", "http://localhost:4717/")
            .is_err());
        assert!(
            calls.lock().unwrap().is_empty(),
            "rejected URLs/serials must never invoke adb"
        );
    }
}
