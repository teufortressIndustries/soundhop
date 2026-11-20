//! `systemd` user service, XDG autostart `.desktop`, and Windows `HKCU Run` helpers.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::constants::{APP_NAME, APP_SLUG, VERSION};
#[cfg(not(windows))]
use crate::util::{ensure_dir, home_dir, ProcessRunner, SystemRunner};
#[cfg(windows)]
use crate::util::home_dir;

pub const UNIT_NAME: &str = "soundhop.service";
pub const AUTOSTART_NAME: &str = "soundhop-tray.desktop";

pub const WINDOWS_RUN_SUBKEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
pub const WINDOWS_RUN_VALUE_NAME: &str = "SoundHop";
pub const WINDOWS_RUN_DISPLAY_PATH: &str =
    "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run\\SoundHop";

/// Resolve the Windows executable to launch for background/autostart.
/// Prefers a sibling `soundhop-gui.exe` (GUI subsystem, no console window) if present
/// next to `current_exe`, falling back to `current_exe`.
pub fn resolve_windows_launch_binary(current_exe: &Path) -> PathBuf {
    if let Some(parent) = current_exe.parent() {
        let gui_sibling = parent.join("soundhop-gui.exe");
        if gui_sibling.is_file() {
            return gui_sibling;
        }
    }
    current_exe.to_path_buf()
}

/// Build the Windows `HKCU\...\Run` command line for a given executable path.
pub fn windows_autostart_command_for(current_exe: &Path, tray: bool) -> String {
    let target = resolve_windows_launch_binary(current_exe);
    let subcmd = if tray { "tray" } else { "daemon" };
    format!("\"{}\" {subcmd}", target.display())
}

#[cfg(windows)]
mod win_reg {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW,
        RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE,
        REG_OPTION_NON_VOLATILE, REG_SZ,
    };

    pub fn wide_null(s: &str) -> Vec<u16> {
        OsStr::new(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    pub fn set_hkcu_sz(subkey: &str, value_name: &str, value: &str) -> Result<(), u32> {
        let subkey_w = wide_null(subkey);
        let name_w = wide_null(value_name);
        let val_w = wide_null(value);
        let mut hkey: HKEY = ptr::null_mut();
        let rc = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                subkey_w.as_ptr(),
                0,
                ptr::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                ptr::null(),
                &mut hkey,
                ptr::null_mut(),
            )
        };
        if rc != ERROR_SUCCESS {
            return Err(rc);
        }
        let byte_len = (val_w.len() * std::mem::size_of::<u16>()) as u32;
        let set_rc = unsafe {
            RegSetValueExW(
                hkey,
                name_w.as_ptr(),
                0,
                REG_SZ,
                val_w.as_ptr() as *const u8,
                byte_len,
            )
        };
        unsafe {
            RegCloseKey(hkey);
        }
        if set_rc == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(set_rc)
        }
    }

    pub fn get_hkcu_sz(subkey: &str, value_name: &str) -> Option<String> {
        let subkey_w = wide_null(subkey);
        let name_w = wide_null(value_name);
        let mut hkey: HKEY = ptr::null_mut();
        let rc = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                subkey_w.as_ptr(),
                0,
                KEY_QUERY_VALUE,
                &mut hkey,
            )
        };
        if rc != ERROR_SUCCESS {
            return None;
        }
        let mut reg_type: u32 = 0;
        let mut byte_len: u32 = 0;
        let q1 = unsafe {
            RegQueryValueExW(
                hkey,
                name_w.as_ptr(),
                ptr::null(),
                &mut reg_type,
                ptr::null_mut(),
                &mut byte_len,
            )
        };
        if q1 != ERROR_SUCCESS || byte_len == 0 {
            unsafe {
                RegCloseKey(hkey);
            }
            return None;
        }
        let mut buf = vec![0u16; (byte_len as usize).div_ceil(2)];
        let mut actual_bytes = (buf.len() * 2) as u32;
        let q2 = unsafe {
            RegQueryValueExW(
                hkey,
                name_w.as_ptr(),
                ptr::null(),
                &mut reg_type,
                buf.as_mut_ptr() as *mut u8,
                &mut actual_bytes,
            )
        };
        unsafe {
            RegCloseKey(hkey);
        }
        if q2 != ERROR_SUCCESS {
            return None;
        }
        let u16_len = (actual_bytes as usize) / 2;
        let slice = &buf[..u16_len.min(buf.len())];
        let trimmed = match slice.iter().position(|&c| c == 0) {
            Some(pos) => &slice[..pos],
            None => slice,
        };
        Some(String::from_utf16_lossy(trimmed))
    }

    pub fn delete_hkcu_value(subkey: &str, value_name: &str) -> Result<bool, u32> {
        let subkey_w = wide_null(subkey);
        let name_w = wide_null(value_name);
        let mut hkey: HKEY = ptr::null_mut();
        let rc = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                subkey_w.as_ptr(),
                0,
                KEY_SET_VALUE,
                &mut hkey,
            )
        };
        if rc == ERROR_FILE_NOT_FOUND {
            return Ok(false);
        }
        if rc != ERROR_SUCCESS {
            return Err(rc);
        }
        let del_rc = unsafe { RegDeleteValueW(hkey, name_w.as_ptr()) };
        unsafe {
            RegCloseKey(hkey);
        }
        if del_rc == ERROR_SUCCESS {
            Ok(true)
        } else if del_rc == ERROR_FILE_NOT_FOUND {
            Ok(false)
        } else {
            Err(del_rc)
        }
    }
}

#[cfg(windows)]
fn spawn_windows_background(tray: bool) -> bool {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::Instant;

    if crate::ipc::is_running(None) {
        return true;
    }

    const CREATE_NO_WINDOW: u32 = 0x08000000;
    const DETACHED_PROCESS: u32 = 0x00000008;

    let exe = resolve_windows_launch_binary(&PathBuf::from(current_executable()));
    let subcmd = if tray { "tray" } else { "daemon" };
    let mut cmd = Command::new(&exe);
    cmd.arg(subcmd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS);
    if cmd.spawn().is_err() {
        return false;
    }
    let deadline = Instant::now() + Duration::from_millis(3500);
    while Instant::now() < deadline {
        if crate::ipc::is_running(None) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[cfg(windows)]
pub fn windows_autostart_value() -> Option<String> {
    win_reg::get_hkcu_sz(WINDOWS_RUN_SUBKEY, WINDOWS_RUN_VALUE_NAME)
}

pub fn systemd_dir() -> PathBuf {
    let base = env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".config"));
    base.join("systemd").join("user")
}

pub fn unit_path() -> PathBuf {
    systemd_dir().join(UNIT_NAME)
}

pub fn autostart_dir() -> PathBuf {
    let base = env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir().join(".config"));
    base.join("autostart")
}

pub fn autostart_path() -> PathBuf {
    autostart_dir().join(AUTOSTART_NAME)
}

pub fn unit_installed() -> bool {
    #[cfg(windows)]
    {
        windows_autostart_value().is_some()
    }
    #[cfg(not(windows))]
    {
        unit_path().exists()
    }
}

pub fn current_executable() -> String {
    if let Ok(appimage) = env::var("APPIMAGE") {
        if !appimage.trim().is_empty() {
            return appimage;
        }
    }
    env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_else(|| "soundhop".to_string())
}

fn quote_exec(value: &str) -> String {
    if value.chars().any(char::is_whitespace) {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        value.to_string()
    }
}

pub fn unit_text_for(exe_path: &str) -> String {
    let exe = quote_exec(exe_path);
    format!(
        "# Generated by SoundHop {VERSION}
[Unit]
Description={APP_NAME} local audio sharing
After=pipewire-pulse.service pipewire.service wireplumber.service
Wants=pipewire-pulse.service
PartOf=graphical-session.target
StartLimitIntervalSec=60
StartLimitBurst=5

[Service]
Type=simple
ExecStart={exe} daemon
Restart=on-failure
RestartSec=5
RestartPreventExitStatus=2
TimeoutStopSec=10
KillMode=mixed

[Install]
WantedBy=default.target
"
    )
}

pub fn unit_text() -> String {
    unit_text_for(&current_executable())
}

pub fn autostart_text_for(exe_path: &str) -> String {
    let exe = quote_exec(exe_path);
    format!(
        "[Desktop Entry]
Type=Application
Version=1.0
Name={APP_NAME}
Comment=Share audio with devices on your local network
Exec={exe} tray
Terminal=false
Icon={APP_SLUG}
Categories=AudioVideo;Audio;Network;
StartupNotify=false
X-GNOME-Autostart-enabled=true
"
    )
}

pub fn autostart_text() -> String {
    autostart_text_for(&current_executable())
}

#[cfg(not(windows))]
fn run_systemctl(args: &[&str]) -> bool {
    let mut full = vec!["systemctl".to_string(), "--user".to_string()];
    for a in args {
        full.push((*a).to_string());
    }
    SystemRunner
        .run(&full, None, Duration::from_secs(15))
        .map(|r| r.ok())
        .unwrap_or(false)
}

pub fn systemctl_available() -> bool {
    #[cfg(windows)]
    {
        false
    }
    #[cfg(not(windows))]
    {
        run_systemctl(&["show-environment"])
    }
}

pub fn daemon_reload() -> bool {
    #[cfg(windows)]
    {
        true
    }
    #[cfg(not(windows))]
    {
        run_systemctl(&["daemon-reload"])
    }
}

pub fn enable_service() -> bool {
    #[cfg(windows)]
    {
        let cmd = windows_autostart_command_for(&PathBuf::from(current_executable()), true);
        win_reg::set_hkcu_sz(WINDOWS_RUN_SUBKEY, WINDOWS_RUN_VALUE_NAME, &cmd).is_ok()
    }
    #[cfg(not(windows))]
    {
        run_systemctl(&["enable", UNIT_NAME])
    }
}

pub fn disable_service() -> bool {
    #[cfg(windows)]
    {
        win_reg::delete_hkcu_value(WINDOWS_RUN_SUBKEY, WINDOWS_RUN_VALUE_NAME).is_ok()
    }
    #[cfg(not(windows))]
    {
        run_systemctl(&["disable", UNIT_NAME])
    }
}

pub fn start_service() -> bool {
    #[cfg(windows)]
    {
        let use_tray = windows_autostart_value()
            .map(|v| !v.to_ascii_lowercase().ends_with(" daemon"))
            .unwrap_or(true);
        spawn_windows_background(use_tray)
    }
    #[cfg(not(windows))]
    {
        run_systemctl(&["start", UNIT_NAME])
    }
}

pub fn stop_service() -> bool {
    #[cfg(windows)]
    {
        if !crate::ipc::is_reachable(None) {
            return true;
        }
        let pid = crate::ipc::ping(None)
            .and_then(|info| info.pid)
            .or_else(|| {
                fs::read_to_string(crate::util::pid_path())
                    .ok()
                    .and_then(|s| s.trim().parse::<u64>().ok())
            });
        let _ = crate::ipc::request(
            "stop",
            serde_json::json!({}),
            None,
            Duration::from_secs(2),
        );
        for attempt in 0..30 {
            if !crate::ipc::is_reachable(None) {
                return true;
            }
            if attempt == 15 {
                if let Some(p) = pid {
                    let _ = crate::ipc::terminate_process(p);
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        !crate::ipc::is_reachable(None)
    }
    #[cfg(not(windows))]
    {
        run_systemctl(&["stop", UNIT_NAME])
    }
}

pub fn restart_service() -> bool {
    #[cfg(windows)]
    {
        let _ = stop_service();
        start_service()
    }
    #[cfg(not(windows))]
    {
        run_systemctl(&["restart", UNIT_NAME])
    }
}

pub fn install_tray_autostart() -> bool {
    #[cfg(windows)]
    {
        let cmd = windows_autostart_command_for(&PathBuf::from(current_executable()), true);
        win_reg::set_hkcu_sz(WINDOWS_RUN_SUBKEY, WINDOWS_RUN_VALUE_NAME, &cmd).is_ok()
    }
    #[cfg(not(windows))]
    {
        let dir = autostart_dir();
        if ensure_dir(&dir).is_err() {
            return false;
        }
        fs::write(autostart_path(), autostart_text()).is_ok()
    }
}

pub fn remove_tray_autostart() -> bool {
    #[cfg(windows)]
    {
        win_reg::delete_hkcu_value(WINDOWS_RUN_SUBKEY, WINDOWS_RUN_VALUE_NAME).unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        let p = autostart_path();
        if p.exists() {
            fs::remove_file(p).is_ok()
        } else {
            false
        }
    }
}

pub fn install(enable: bool, start: bool, tray: bool) -> serde_json::Value {
    #[cfg(windows)]
    {
        let enabled = if enable || tray {
            let cmd = windows_autostart_command_for(&PathBuf::from(current_executable()), tray);
            match win_reg::set_hkcu_sz(WINDOWS_RUN_SUBKEY, WINDOWS_RUN_VALUE_NAME, &cmd) {
                Ok(()) => true,
                Err(code) => {
                    return serde_json::json!({
                        "ok": false,
                        "error": format!("Failed to write Windows autostart registry key (Win32 error {code})"),
                        "unit": WINDOWS_RUN_DISPLAY_PATH,
                        "daemon_reloaded": false,
                        "enabled": false,
                        "started": false,
                        "tray_autostart": String::new(),
                    });
                }
            }
        } else {
            false
        };
        let started = if start {
            spawn_windows_background(tray)
        } else {
            false
        };
        let tray_written = tray && enabled;
        let ok = (!enable || enabled) && (!start || started) && (!tray || tray_written);
        serde_json::json!({
            "ok": ok,
            "unit": WINDOWS_RUN_DISPLAY_PATH,
            "daemon_reloaded": true,
            "enabled": enabled,
            "started": started,
            "tray_autostart": if tray_written { WINDOWS_RUN_DISPLAY_PATH } else { "" },
        })
    }
    #[cfg(not(windows))]
    {
        let dir = systemd_dir();
        if let Err(err) = ensure_dir(&dir) {
            return serde_json::json!({
                "ok": false,
                "error": format!("Failed to prepare systemd user directory: {err}"),
                "unit": unit_path().to_string_lossy(),
                "daemon_reloaded": false,
                "enabled": false,
                "started": false,
                "tray_autostart": String::new(),
            });
        }
        let path = unit_path();
        if let Err(err) = fs::write(&path, unit_text()) {
            return serde_json::json!({
                "ok": false,
                "error": format!("Failed to write systemd unit {}: {err}", path.display()),
                "unit": path.to_string_lossy(),
                "daemon_reloaded": false,
                "enabled": false,
                "started": false,
                "tray_autostart": String::new(),
            });
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o644));
        }
        let reloaded = daemon_reload();
        let enabled = if enable { enable_service() } else { false };
        let started = if start { start_service() } else { false };
        let tray_written = if tray {
            install_tray_autostart()
        } else {
            false
        };
        let ok = (!enable || enabled) && (!start || started) && (!tray || tray_written);
        serde_json::json!({
            "ok": ok,
            "unit": path.to_string_lossy(),
            "daemon_reloaded": reloaded,
            "enabled": enabled,
            "started": started,
            "tray_autostart": if tray_written { autostart_path().to_string_lossy().to_string() } else { String::new() },
        })
    }
}

pub fn uninstall() -> serde_json::Value {
    #[cfg(windows)]
    {
        let _ = stop_service();
        let removed =
            win_reg::delete_hkcu_value(WINDOWS_RUN_SUBKEY, WINDOWS_RUN_VALUE_NAME).unwrap_or(false);
        serde_json::json!({
            "removed": removed,
            "tray_removed": removed,
        })
    }
    #[cfg(not(windows))]
    {
        let _ = stop_service();
        let _ = disable_service();
        let p = unit_path();
        let removed = if p.exists() {
            fs::remove_file(&p).is_ok()
        } else {
            false
        };
        let _ = daemon_reload();
        let tray_removed = remove_tray_autostart();
        serde_json::json!({
            "removed": removed,
            "tray_removed": tray_removed,
        })
    }
}

pub fn status() -> serde_json::Value {
    #[cfg(windows)]
    {
        let installed = windows_autostart_value().is_some();
        let active = crate::ipc::is_running(None);
        serde_json::json!({
            "unit": WINDOWS_RUN_DISPLAY_PATH,
            "installed": installed,
            "systemctl": false,
            "enabled": installed,
            "active": active,
            "tray_autostart": installed,
        })
    }
    #[cfg(not(windows))]
    {
        let installed = unit_installed();
        let active = run_systemctl(&["is-active", "--quiet", UNIT_NAME]);
        let enabled = run_systemctl(&["is-enabled", "--quiet", UNIT_NAME]);
        serde_json::json!({
            "unit": unit_path().to_string_lossy(),
            "installed": installed,
            "systemctl": systemctl_available(),
            "enabled": enabled,
            "active": active,
            "tray_autostart": autostart_path().exists(),
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServiceSyncOutcome {
    pub unit_updated: bool,
    pub autostart_updated: bool,
    pub old_service_stopped: bool,
}

impl ServiceSyncOutcome {
    pub fn any_updated(&self) -> bool {
        self.unit_updated || self.autostart_updated || self.old_service_stopped
    }
}

pub fn extract_exec_binary(content: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    for line in content.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix(&prefix) else {
            continue;
        };
        let rest = rest.trim_start();
        if let Some(after_quote) = rest.strip_prefix('"') {
            let mut out = String::new();
            let mut escaped = false;
            for ch in after_quote.chars() {
                if escaped {
                    out.push(ch);
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    return (!out.is_empty()).then_some(out);
                } else {
                    out.push(ch);
                }
            }
            return (!out.is_empty()).then_some(out);
        }
        let token = rest.split_whitespace().next().unwrap_or("");
        return (!token.is_empty()).then(|| token.to_string());
    }
    None
}

fn should_update_appimage_exec(existing_exe: &str, new_appimage: &str) -> bool {
    if existing_exe.trim().is_empty() || existing_exe == new_appimage {
        return false;
    }
    existing_exe.to_ascii_lowercase().ends_with(".appimage") || !Path::new(existing_exe).exists()
}

pub fn sync_appimage_paths_at(
    unit_file: &Path,
    autostart_file: &Path,
    current_appimage: &str,
) -> ServiceSyncOutcome {
    let mut outcome = ServiceSyncOutcome::default();

    if unit_file.exists() {
        if let Ok(content) = fs::read_to_string(unit_file) {
            if let Some(existing) = extract_exec_binary(&content, "ExecStart") {
                if should_update_appimage_exec(&existing, current_appimage)
                    && fs::write(unit_file, unit_text_for(current_appimage)).is_ok()
                {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let _ = fs::set_permissions(unit_file, fs::Permissions::from_mode(0o644));
                    }
                    outcome.unit_updated = true;
                }
            }
        }
    }

    if autostart_file.exists() {
        if let Ok(content) = fs::read_to_string(autostart_file) {
            if let Some(existing) = extract_exec_binary(&content, "Exec") {
                if should_update_appimage_exec(&existing, current_appimage)
                    && fs::write(autostart_file, autostart_text_for(current_appimage)).is_ok()
                {
                    outcome.autostart_updated = true;
                }
            }
        }
    }

    outcome
}

/// If running from an AppImage and existing systemd/autostart entries point to a
/// different `.AppImage` (or missing executable), updates them to the current `$APPIMAGE`.
/// Optionally stops an active old-version `soundhop.service` so the new version can take over.
pub fn sync_appimage_paths_if_installed(stop_old_service: bool) -> ServiceSyncOutcome {
    let Ok(appimage) = env::var("APPIMAGE") else {
        return ServiceSyncOutcome::default();
    };
    let current = appimage.trim();
    if current.is_empty() {
        return ServiceSyncOutcome::default();
    }

    #[cfg_attr(windows, allow(unused_mut))]
    let mut outcome = sync_appimage_paths_at(&unit_path(), &autostart_path(), current);
    if outcome.unit_updated {
        let _ = daemon_reload();
    }

    #[cfg(not(windows))]
    if stop_old_service && run_systemctl(&["is-active", "--quiet", UNIT_NAME]) {
        let version_mismatch = crate::ipc::ping(None).is_some_and(|p| p.version != VERSION);
        if (outcome.unit_updated || version_mismatch) && stop_service() {
            outcome.old_service_stopped = true;
        }
    }
    #[cfg(windows)]
    let _ = stop_old_service;

    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unit_and_autostart_generation() {
        let unit = unit_text();
        assert!(unit.contains("Description=SoundHop local audio sharing"));
        assert!(unit.contains("daemon"));
        assert!(unit.contains("RestartPreventExitStatus=2"));
        assert!(unit.contains("KillMode=mixed"));

        let desktop = autostart_text();
        assert!(desktop.contains("[Desktop Entry]"));
        assert!(desktop.contains("tray"));
    }

    #[test]
    fn test_extract_exec_binary_quoted_and_unquoted() {
        let quoted = "ExecStart=\"/home/user/My Apps/SoundHop-2.0.4.AppImage\" daemon\n";
        assert_eq!(
            extract_exec_binary(quoted, "ExecStart").as_deref(),
            Some("/home/user/My Apps/SoundHop-2.0.4.AppImage")
        );

        let unquoted = "Exec=/opt/SoundHop-2.0.4.AppImage tray\n";
        assert_eq!(
            extract_exec_binary(unquoted, "Exec").as_deref(),
            Some("/opt/SoundHop-2.0.4.AppImage")
        );
    }

    #[test]
    fn test_sync_appimage_paths_updates_old_appimage_but_keeps_existing_native_bin() {
        let dir = tempfile::tempdir().unwrap();
        let unit_file = dir.path().join("soundhop.service");
        let desktop_file = dir.path().join("soundhop-tray.desktop");

        fs::write(
            &unit_file,
            unit_text_for("/tmp/SoundHop-2.0.4-universal-x86_64.AppImage"),
        )
        .unwrap();
        fs::write(
            &desktop_file,
            autostart_text_for("/tmp/SoundHop-2.0.4-universal-x86_64.AppImage"),
        )
        .unwrap();

        let new_appimage = "/tmp/SoundHop-2.1.0-universal-x86_64.AppImage";
        let out = sync_appimage_paths_at(&unit_file, &desktop_file, new_appimage);
        assert!(out.unit_updated);
        assert!(out.autostart_updated);
        assert_eq!(
            extract_exec_binary(&fs::read_to_string(&unit_file).unwrap(), "ExecStart").as_deref(),
            Some(new_appimage)
        );
        assert_eq!(
            extract_exec_binary(&fs::read_to_string(&desktop_file).unwrap(), "Exec").as_deref(),
            Some(new_appimage)
        );

        // Existing native binary path on disk should NOT be overwritten by an AppImage run
        let native_bin = dir.path().join("soundhop");
        fs::write(&native_bin, "#!/bin/sh\n").unwrap();
        let native_str = native_bin.to_str().unwrap();
        fs::write(&unit_file, unit_text_for(native_str)).unwrap();
        let out2 = sync_appimage_paths_at(&unit_file, &desktop_file, new_appimage);
        assert!(!out2.unit_updated);
        assert!(!out2.autostart_updated);
    }

    #[test]
    fn test_resolve_windows_launch_binary_prefers_sibling_gui_exe() {
        let dir = tempfile::tempdir().unwrap();
        let cli_exe = dir.path().join("soundhop.exe");
        fs::write(&cli_exe, b"cli").unwrap();

        // Without sibling soundhop-gui.exe, falls back to current exe.
        assert_eq!(resolve_windows_launch_binary(&cli_exe), cli_exe);
        let cmd_cli = windows_autostart_command_for(&cli_exe, true);
        assert!(cmd_cli.ends_with("\" tray"));
        assert!(cmd_cli.contains("soundhop.exe"));

        // With sibling soundhop-gui.exe present, prefers soundhop-gui.exe.
        let gui_exe = dir.path().join("soundhop-gui.exe");
        fs::write(&gui_exe, b"gui").unwrap();
        assert_eq!(resolve_windows_launch_binary(&cli_exe), gui_exe);
        let cmd_gui = windows_autostart_command_for(&cli_exe, true);
        assert!(cmd_gui.contains("soundhop-gui.exe"));
        assert!(cmd_gui.ends_with("\" tray"));

        let cmd_daemon = windows_autostart_command_for(&cli_exe, false);
        assert!(cmd_daemon.ends_with("\" daemon"));
    }

    #[cfg(windows)]
    #[test]
    fn test_windows_registry_roundtrip_in_test_subkey() {
        let subkey = format!("Software\\SoundHop\\UnitTest_{}", std::process::id());
        let value_name = "TestAutostart";
        let expected = "\"C:\\Program Files\\SoundHop\\soundhop-gui.exe\" tray";

        win_reg::set_hkcu_sz(&subkey, value_name, expected).expect("set_hkcu_sz should succeed");
        let got = win_reg::get_hkcu_sz(&subkey, value_name);
        assert_eq!(got.as_deref(), Some(expected));

        let deleted = win_reg::delete_hkcu_value(&subkey, value_name)
            .expect("delete_hkcu_value should succeed");
        assert!(deleted);
        assert_eq!(win_reg::get_hkcu_sz(&subkey, value_name), None);

        // Deleting again should cleanly return Ok(false).
        let deleted_again = win_reg::delete_hkcu_value(&subkey, value_name)
            .expect("second delete should be idempotent");
        assert!(!deleted_again);

        // Clean up temporary test subkey.
        unsafe {
            let subkey_w = win_reg::wide_null(&subkey);
            windows_sys::Win32::System::Registry::RegDeleteKeyW(
                windows_sys::Win32::System::Registry::HKEY_CURRENT_USER,
                subkey_w.as_ptr(),
            );
        }
    }
}
