//! Android audio capture integration using `scrcpy` and `adb`.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tar::Archive;
use thiserror::Error;

use crate::backend::AudioBackend;
use crate::constants::{
    DEFAULT_AUDIO_BIT_RATE, DEFAULT_AUDIO_BUFFER_MS, DEFAULT_AUDIO_CODEC, DEFAULT_SCRCPY_VERSION,
    SUPPORTED_AUDIO_CODECS,
};
use crate::util::{
    data_dir, home_dir, is_executable, now_secs, which_binary, CommandResult, ProcessRunner,
    SystemRunner,
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
    pub fn ready(&self) -> bool {
        self.scrcpy_path.is_some() && self.adb_path.is_some()
    }
}

pub struct AndroidManager {
    pub custom_scrcpy: String,
    pub custom_adb: String,
    runner: Arc<dyn ProcessRunner>,
}

impl AndroidManager {
    pub fn new(scrcpy_path: &str, adb_path: &str) -> Self {
        Self {
            custom_scrcpy: scrcpy_path.trim().to_string(),
            custom_adb: adb_path.trim().to_string(),
            runner: Arc::new(SystemRunner),
        }
    }

    pub fn with_runner(scrcpy_path: &str, adb_path: &str, runner: Arc<dyn ProcessRunner>) -> Self {
        Self {
            custom_scrcpy: scrcpy_path.trim().to_string(),
            custom_adb: adb_path.trim().to_string(),
            runner,
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

        let resp = ureq::get(&url)
            .set("User-Agent", "SoundHop-AndroidManager")
            .call()
            .map_err(|e| AndroidError::Message(format!("Failed to download {url}: {e}")))?;

        let mut reader = resp.into_reader();
        let mut out_file = File::create(&archive_path)?;
        let mut hasher = Sha256::new();
        let mut buf = [0u8; 65536];
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            out_file.write_all(&buf[..n])?;
            hasher.update(&buf[..n]);
        }
        drop(out_file);

        if let Some(expected) = expected_sha256.map(str::trim).filter(|s| !s.is_empty()) {
            let actual = hex::encode(hasher.finalize());
            if !actual.eq_ignore_ascii_case(expected) {
                let _ = fs::remove_file(&archive_path);
                return Err(AndroidError::Message(format!(
                    "SHA-256 checksum mismatch: expected {expected}, got {actual}"
                )));
            }
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
        if let Some(s) = serial.filter(|s| !s.is_empty()) {
            full.push("-s".to_string());
            full.push(s.to_string());
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
        let target = if host.contains(':') {
            host.to_string()
        } else {
            format!("{host}:{port}")
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
        let args: Vec<&str> = match target {
            Some(t) if !t.is_empty() => vec!["disconnect", t],
            _ => vec!["disconnect"],
        };
        match self.run_adb(&args, None) {
            Ok(res) => (true, res.stdout.trim().to_string()),
            Err(e) => (false, e.to_string()),
        }
    }

    pub fn list_encoders(&self, serial: Option<&str>) -> Result<AudioEncoderInfo, AndroidError> {
        let scrcpy_bin = self
            .find_scrcpy()
            .ok_or_else(|| AndroidError::Message("scrcpy is not installed.".to_string()))?;
        let mut args = vec![
            scrcpy_bin.to_string_lossy().to_string(),
            "--list-encoders".to_string(),
        ];
        if let Some(s) = serial.filter(|s| !s.is_empty()) {
            args.push("-s".to_string());
            args.push(s.to_string());
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
        let scrcpy_bin = self
            .find_scrcpy()
            .ok_or_else(|| AndroidError::Message("scrcpy binary not found.".to_string()))?;
        let mut cmd = vec![
            scrcpy_bin.to_string_lossy().to_string(),
            "-s".to_string(),
            serial.to_string(),
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
            cmd.push(format!("--audio-source={}", src.to_lowercase()));
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
    log_lines: Vec<String>,
    last_error: String,
}

pub struct AndroidCaptureSession {
    pub manager: Arc<AndroidManager>,
    backend: Arc<dyn AudioBackend>,
    inner: Arc<Mutex<CaptureInner>>,
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
                log_lines: Vec::new(),
                last_error: String::new(),
            })),
        }
    }

    pub fn is_active(&self) -> bool {
        let mut guard = self.inner.lock().unwrap();
        if let Some(ref mut child) = guard.child {
            if let Ok(None) = child.try_wait() {
                return true;
            }
        }
        false
    }

    pub fn status(&self) -> serde_json::Value {
        let mut guard = self.inner.lock().unwrap();
        let mut active = false;
        let mut pid = 0u32;
        if let Some(ref mut child) = guard.child {
            if let Ok(None) = child.try_wait() {
                active = true;
                pid = child.id();
            }
        }
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
        serde_json::json!({
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
        })
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
            if let Ok(nid) = self.backend.load_module(
                "module-null-sink",
                &[
                    format!("sink_name={sink_name}"),
                    "sink_properties=device.description=SoundHop-Android-Mic-Sink".to_string(),
                ],
            ) {
                virt_sink_mod = nid;
            }
            if let Ok(rid) = self.backend.load_module(
                "module-remap-source",
                &[
                    format!("source_name={source_name}"),
                    format!("master={sink_name}.monitor"),
                    "source_properties=device.description=Android-Microphone".to_string(),
                ],
            ) {
                virt_src_mod = rid;
            }
            prev_source = self.backend.default_source().unwrap_or_default();
            let _ = self.backend.set_default_source(source_name);
            cmd.env("SDL_AUDIODRIVER", "pulseaudio")
                .env("PULSE_SINK", sink_name)
                .env("PIPEWIRE_NODE", sink_name);
        }

        crate::util::close_fds_pre_exec(&mut cmd);

        let mut child = cmd
            .spawn()
            .map_err(|e| AndroidError::Message(format!("Failed to start scrcpy: {e}")))?;

        if let Some(stdout) = child.stdout.take() {
            let inner_clone = Arc::clone(&self.inner);
            thread::spawn(move || {
                let reader = BufReader::new(stdout);
                for line in reader.lines().map_while(Result::ok) {
                    let mut g = inner_clone.lock().unwrap();
                    g.log_lines.push(line);
                    if g.log_lines.len() > 200 {
                        g.log_lines.remove(0);
                    }
                }
            });
        }

        {
            let mut g = self.inner.lock().unwrap();
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
        }

        Ok(self.status())
    }

    pub fn stop(&self) -> serde_json::Value {
        {
            let mut guard = self.inner.lock().unwrap();
            if let Some(mut child) = guard.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            if guard.virtual_source_module > 0 {
                let _ = self
                    .backend
                    .unload_module(guard.virtual_source_module, true);
                guard.virtual_source_module = 0;
            }
            if guard.virtual_sink_module > 0 {
                let _ = self.backend.unload_module(guard.virtual_sink_module, true);
                guard.virtual_sink_module = 0;
            }
            if !guard.previous_default_source.is_empty() {
                let _ = self
                    .backend
                    .set_default_source(&guard.previous_default_source);
                guard.previous_default_source.clear();
            }
            guard.as_mic = false;
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
}
