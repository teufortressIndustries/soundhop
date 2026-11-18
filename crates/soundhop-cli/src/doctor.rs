//! `soundhop doctor` and `soundhop bug-report` diagnostic commands.

use std::sync::Arc;

use soundhop_core::constants::{PROTOCOL_VERSION, VERSION};
use soundhop_core::controller::Controller;
use soundhop_core::firewall::{detect as detect_firewall, open_ports};
use soundhop_core::ipc::is_running as ipc_is_running;
use soundhop_core::service::{install as install_service, status as service_status};
use soundhop_core::util::{config_path, local_addresses, state_path};

use crate::ui::{error, header, info, lock_or_recover, ok, print_json, safe_print, warning};

pub fn run_doctor(controller: &Arc<Controller>, fix: bool, json_out: bool) -> i32 {
    let cfg = lock_or_recover(&controller.config).clone();
    let backend_ok = controller.backend.available();
    let server_name = controller.backend.server_label().unwrap_or_default();
    let is_pw = controller.backend.is_pipewire().unwrap_or(false);
    let def_sink = controller.backend.default_sink().unwrap_or_default();

    let mut fixed_actions: Vec<String> = Vec::new();
    let mut fix_errors: Vec<String> = Vec::new();
    let mut fix_failed = false;

    if fix {
        let rec = controller.reconcile();
        if let Some(arr) = rec.get("actions").and_then(|v| v.as_array()) {
            for item in arr {
                if let Some(s) = item.as_str() {
                    fixed_actions.push(s.to_string());
                }
            }
        }
    }

    let fw = detect_firewall();
    if fix && fw.active {
        let fw_res = open_ports(
            cfg.sharing.port,
            cfg.discovery.port,
            false,
            cfg.discovery.mdns,
        );
        if fw_res.ran {
            if fw_res.ok {
                fixed_actions.push(format!("Opened firewall ports on {}", fw.kind));
            } else {
                fix_failed = true;
                if fw_res.errors.is_empty() {
                    fix_errors.push(format!("Failed to open firewall ports on {}", fw.kind));
                } else {
                    for err in fw_res.errors {
                        fix_errors.push(format!("Firewall ({}): {err}", fw.kind));
                    }
                }
            }
        }
    }

    let svc = service_status();
    let svc_active = svc.get("active").and_then(|v| v.as_bool()).unwrap_or(false);
    let daemon_ipc = ipc_is_running(None);
    if fix && !svc_active && !daemon_ipc {
        let svc_res = install_service(true, true, true);
        let enabled = svc_res
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let started = svc_res
            .get("started")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if started {
            fixed_actions.push("Installed and started user service".to_string());
        } else {
            fix_failed = true;
            fix_errors.push(format!(
                "Failed to install/start user service (enabled={enabled}, started={started})"
            ));
        }
    }

    #[cfg(target_os = "linux")]
    let appimage_mounts: Vec<String> = {
        let mut mounts = Vec::new();
        if let Ok(content) = std::fs::read_to_string("/proc/self/mountinfo") {
            for line in content.lines() {
                if let Some(pos) = line.find(" /tmp/.mount_SoundH") {
                    let part = &line[pos + 1..];
                    if let Some(end) = part.find(' ') {
                        let path = part[..end].to_string();
                        if !mounts.contains(&path) {
                            mounts.push(path);
                        }
                    }
                }
            }
        }
        mounts
    };
    #[cfg(not(target_os = "linux"))]
    let appimage_mounts: Vec<String> = Vec::new();

    #[cfg(target_os = "linux")]
    if fix && appimage_mounts.len() > 1 {
        let own_mount = std::env::var_os("APPDIR")
            .map(|v| v.to_string_lossy().into_owned())
            .filter(|s| s.starts_with("/tmp/.mount_SoundH"));
        for m in appimage_mounts
            .iter()
            .filter(|m| own_mount.as_deref() != Some(m.as_str()))
        {
            let mut cmd = std::process::Command::new("fusermount");
            cmd.args(["-u", m]);
            soundhop_core::util::close_fds_pre_exec(&mut cmd);
            match cmd.output() {
                Ok(out) if out.status.success() => {
                    fixed_actions.push(format!("Unmounted stale AppImage mount {m}"));
                }
                Ok(out) => {
                    fix_failed = true;
                    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                    fix_errors.push(format!(
                        "Failed to unmount stale AppImage mount {m}{}",
                        if stderr.is_empty() {
                            String::new()
                        } else {
                            format!(": {stderr}")
                        }
                    ));
                }
                Err(e) => {
                    fix_failed = true;
                    fix_errors.push(format!(
                        "Failed to run fusermount -u {m}: {e}"
                    ));
                }
            }
        }
    }

    let addrs = local_addresses(false);
    let tools = controller.android_tool_status();
    let exit_ok = backend_ok && !fix_failed;

    let report = serde_json::json!({
        "ok": exit_ok,
        "version": VERSION,
        "protocol": PROTOCOL_VERSION,
        "backend": {
            "available": backend_ok,
            "server": server_name,
            "pipewire": is_pw,
            "default_sink": def_sink,
        },
        "network": {
            "addresses": addrs,
            "sharing_port": cfg.sharing.port,
            "discovery_port": cfg.discovery.port,
        },
        "firewall": fw,
        "service": svc,
        "appimage_mounts": appimage_mounts,
        "daemon_ipc_running": daemon_ipc,
        "android_tools": tools,
        "fixed_actions": fixed_actions,
        "fix_errors": fix_errors,
        "healthy": backend_ok && !addrs.is_empty() && !fix_failed,
    });

    if json_out {
        print_json(&report);
        return if exit_ok { 0 } else { 1 };
    }

    header(&format!("SoundHop v{VERSION} System Diagnostics"));
    if backend_ok {
        ok(&format!(
            "Audio Backend: {server_name} (PipeWire: {is_pw}, Default Sink: {def_sink})"
        ));
    } else {
        error("Audio Backend: unreachable (install pipewire-pulse or pulseaudio-utils)");
    }

    if addrs.is_empty() {
        warning("Network: No private LAN IPv4 addresses detected.");
    } else {
        ok(&format!("LAN Addresses: {}", addrs.join(", ")));
    }

    if fw.active {
        info(&format!("Firewall: {} ({})", fw.kind, fw.detail));
    } else {
        ok(&format!("Firewall: {} ({})", fw.kind, fw.detail));
    }

    if daemon_ipc || svc_active {
        ok("Background Daemon: Running");
    } else {
        warning("Background Daemon: Not running (run `soundhop daemon` or `soundhop setup`)");
    }

    let scrcpy_ready = tools
        .get("ready")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if scrcpy_ready {
        ok(&format!(
            "Android Bridge: scrcpy {} + adb {}",
            tools
                .get("scrcpy_version")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            tools
                .get("adb_version")
                .and_then(|v| v.as_str())
                .unwrap_or("")
        ));
    } else {
        info("Android Bridge: Optional scrcpy/adb not installed (`soundhop android install`)");
    }

    if appimage_mounts.len() > 1 {
        warning(&format!(
            "AppImage Mounts: {} active mounts detected (run `soundhop doctor --fix` to clean up stale mounts)",
            appimage_mounts.len()
        ));
    }

    for act in &fixed_actions {
        ok(&format!("Fixed: {act}"));
    }
    for ferr in &fix_errors {
        error(ferr);
    }

    if exit_ok {
        0
    } else {
        1
    }
}

pub fn run_bug_report(controller: &Arc<Controller>, json_out: bool) -> i32 {
    let st = controller.status(None);
    let fw = detect_firewall();
    let svc = service_status();
    let bundle = serde_json::json!({
        "version": VERSION,
        "protocol": PROTOCOL_VERSION,
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "config_path": config_path().to_string_lossy(),
        "state_path": state_path().to_string_lossy(),
        "status": st,
        "firewall": fw,
        "service": svc,
    });

    if json_out {
        print_json(&bundle);
    } else {
        header("SoundHop Diagnostic Bug Report");
        safe_print(&serde_json::to_string_pretty(&bundle).unwrap_or_default());
    }
    0
}
